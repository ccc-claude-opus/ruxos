//! Process, thread, signal, credential and miscellaneous system calls.
use crate::arch::{self, TrapFrame};
use crate::errno::*;
use crate::mm::vm::{copy_from_user, copy_to_user, read_ptr_array, read_user, write_user};
use crate::sched;
use crate::sync::SpinLock;
use crate::task::signal::{self, SigAction, SigInfo, UNBLOCKABLE};
use crate::task::*;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

pub fn clone(flags: u64, stack: usize, ptid: usize, tls: usize, ctid: usize, f: &mut TrapFrame) -> Result<usize> {
    Ok(crate::task::clone(flags, stack, ptid, tls, ctid, f)? as usize)
}

pub fn execve(dirfd: i32, path: usize, argv: usize, envp: usize, flags: u32, f: &mut TrapFrame) -> Result<usize> {
    let p = super::fs::user_path(path)?;
    let read_strs = |va: usize| -> Result<Vec<Vec<u8>>> {
        read_ptr_array(va, 65536)?.into_iter().map(|s| crate::mm::vm::read_cstr(s, 131072)).collect()
    };
    let av = read_strs(argv)?;
    let ev = read_strs(envp)?;
    let at = super::fs::start(dirfd)?;
    let p = if p.is_empty() && flags & super::fs::AT_EMPTY_PATH != 0 {
        // execveat(fd, "", AT_EMPTY_PATH): the file itself
        let file = super::fs::file(dirfd)?;
        let fp = file.path.clone().ok_or(EBADF)?;
        let root = current_thread().fs.lock().root.clone();
        fp.path_from(&root)
    } else {
        p
    };
    exec::execve(&p, av, ev, &at, f)?;
    Ok(0)
}

pub fn exit(code: i32) -> Result<usize> {
    crate::task::exit((code & 0xff) << 8, false)
}

pub fn exit_group(code: i32) -> Result<usize> {
    crate::task::exit((code & 0xff) << 8, true)
}

pub fn wait4(pid: i32, status: usize, options: u32, rusage: usize) -> Result<usize> {
    let who = match pid {
        -1 => WaitWho::Any,
        0 => WaitWho::Pgid(current_proc().pgid.load(Ordering::Relaxed)),
        p if p < 0 => WaitWho::Pgid((-p) as u32),
        p => WaitWho::Pid(p as u32),
    };
    match crate::task::wait(who, (options & (WNOHANG | WUNTRACED | WCONTINUED)) | WEXITED)? {
        None => Ok(0),
        Some((pid, st, _)) => {
            if status != 0 {
                write_user(status, &st)?;
            }
            if rusage != 0 {
                crate::mm::vm::zero_user(rusage, 144)?;
            }
            Ok(pid as usize)
        }
    }
}

pub fn waitid(idtype: u32, id: i32, info: usize, options: u32, _ru: usize) -> Result<usize> {
    let who = match idtype {
        0 => WaitWho::Any,
        1 => WaitWho::Pid(id as u32),
        2 => WaitWho::Pgid(if id == 0 { current_proc().pgid.load(Ordering::Relaxed) } else { id as u32 }),
        3 => {
            // P_PIDFD
            let f = super::fs::file(id)?;
            let pf = f.ops.as_any().downcast_ref::<crate::vfs::events::PidFd>().ok_or(EINVAL)?;
            WaitWho::Pid(pf.proc.pid)
        }
        _ => return Err(EINVAL),
    };
    if options & (WEXITED | WSTOPPED | WCONTINUED) == 0 {
        return Err(EINVAL);
    }
    let r = crate::task::wait(who, options)?;
    if info != 0 {
        let mut b = [0u8; 128];
        if let Some((pid, st, kind)) = r {
            let (code, status) = match kind {
                1 => (5, (st >> 8) & 0xff),  // CLD_STOPPED
                2 => (6, signal::SIGCONT as i32), // CLD_CONTINUED
                _ if st & 0x7f == 0 => (1, (st >> 8) & 0xff), // CLD_EXITED
                _ if st & 0x80 != 0 => (3, st & 0x7f), // CLD_DUMPED
                _ => (2, st & 0x7f),          // CLD_KILLED
            };
            b[0..4].copy_from_slice(&(signal::SIGCHLD as i32).to_le_bytes());
            b[8..12].copy_from_slice(&(code as i32).to_le_bytes());
            b[16..20].copy_from_slice(&(pid as i32).to_le_bytes());
            b[24..28].copy_from_slice(&status.to_le_bytes());
        }
        copy_to_user(info, &b)?;
    }
    Ok(0)
}
pub const WSTOPPED: u32 = 2;

pub fn set_tid_address(p: usize) -> Result<usize> {
    own().clear_child_tid = p;
    Ok(sched::current().tid as usize)
}

pub fn set_robust_list(head: usize, len: usize) -> Result<usize> {
    if len != 24 {
        return Err(EINVAL);
    }
    own().robust_list = head;
    Ok(0)
}

pub fn get_robust_list(tid: i32, head_ptr: usize, len_ptr: usize) -> Result<usize> {
    let head = if tid == 0 {
        own().robust_list
    } else {
        let t = task_by_tid(tid as u32).ok_or(ESRCH)?;
        unsafe { (*thread_of(&t).ok_or(ESRCH)?.own.get()).robust_list }
    };
    write_user(head_ptr, &head)?;
    write_user(len_ptr, &24usize)?;
    Ok(0)
}

pub fn futex(uaddr: usize, op: u32, val: u32, timeout: usize, uaddr2: usize, val3: u32) -> Result<usize> {
    use crate::futex::*;
    let private = op & FUTEX_PRIVATE_FLAG != 0;
    let cmd = op & 0x7f;
    let deadline = |abs: bool| -> Result<Option<u64>> {
        if timeout == 0 {
            return Ok(None);
        }
        let ts: [i64; 2] = read_user(timeout)?;
        if ts[0] < 0 || !(0..1_000_000_000).contains(&ts[1]) {
            return Err(EINVAL);
        }
        let ns = ts[0] as i128 * 1_000_000_000 + ts[1] as i128;
        let now_ticks = arch::ticks();
        let rel = if abs {
            let now = if op & FUTEX_CLOCK_REALTIME != 0 { crate::time::realtime_ns() as i128 } else { crate::time::monotonic_ns() as i128 };
            (ns - now).max(0)
        } else {
            ns
        };
        Ok(Some(now_ticks + crate::time::ns_to_ticks(rel.min(i64::MAX as i128) as u64)))
    };
    match cmd {
        FUTEX_WAIT => wait(uaddr, val, deadline(false)?, u32::MAX, private),
        FUTEX_WAIT_BITSET => wait(uaddr, val, deadline(true)?, val3, private),
        FUTEX_WAKE => wake(uaddr, private, val as usize, u32::MAX),
        FUTEX_WAKE_BITSET => wake(uaddr, private, val as usize, val3),
        FUTEX_REQUEUE => requeue(uaddr, private, val as usize, uaddr2, timeout, None),
        FUTEX_CMP_REQUEUE => requeue(uaddr, private, val as usize, uaddr2, timeout, Some(val3)),
        FUTEX_WAKE_OP => wake_op(uaddr, private, val as usize, uaddr2, timeout, val3),
        _ => Err(ENOSYS),
    }
}

// ---- signals

#[cfg(target_arch = "aarch64")]
const SIGACTION_SIZE: usize = 32; // handler, flags, restorer, mask
#[cfg(not(target_arch = "aarch64"))]
const SIGACTION_SIZE: usize = 24; // handler, flags, mask

fn read_sigaction(p: usize) -> Result<SigAction> {
    let mut b = [0u8; 32];
    copy_from_user(&mut b[..SIGACTION_SIZE], p)?;
    let w = |i: usize| u64::from_le_bytes(b[8 * i..8 * i + 8].try_into().unwrap());
    Ok(if SIGACTION_SIZE == 32 {
        SigAction { handler: w(0) as usize, flags: w(1), restorer: w(2) as usize, mask: w(3) }
    } else {
        SigAction { handler: w(0) as usize, flags: w(1), restorer: 0, mask: w(2) }
    })
}

fn write_sigaction(p: usize, a: &SigAction) -> Result<()> {
    let v: Vec<u64> = if SIGACTION_SIZE == 32 {
        alloc::vec![a.handler as u64, a.flags, a.restorer as u64, a.mask]
    } else {
        alloc::vec![a.handler as u64, a.flags, a.mask]
    };
    let b = unsafe { core::slice::from_raw_parts(v.as_ptr() as *const u8, SIGACTION_SIZE) };
    copy_to_user(p, b)
}

pub fn rt_sigaction(sig: u32, act: usize, old: usize, size: usize) -> Result<usize> {
    if size != 8 || sig == 0 || sig > signal::NSIG {
        return Err(EINVAL);
    }
    let th = current_thread();
    let new = if act != 0 { Some(read_sigaction(act)?) } else { None };
    if new.is_some() && (sig == signal::SIGKILL || sig == signal::SIGSTOP) {
        return Err(EINVAL);
    }
    let prev = {
        let mut h = th.sighand.lock();
        let prev = h.act[sig as usize];
        if let Some(mut n) = new {
            n.mask &= !UNBLOCKABLE;
            h.act[sig as usize] = n;
        }
        prev
    };
    // setting SIG_IGN discards pending instances
    if let Some(n) = new {
        if n.handler == signal::SIG_IGN || (n.handler == signal::SIG_DFL && matches!(sig, signal::SIGCHLD | signal::SIGURG | signal::SIGWINCH)) {
            th.proc.pending.lock().remove(signal::bit(sig));
            for t in th.proc.threads.lock().iter().filter_map(|w| w.upgrade()) {
                if let Some(x) = thread_of(&t) {
                    x.pending.lock().remove(signal::bit(sig));
                }
            }
        }
    }
    if old != 0 {
        write_sigaction(old, &prev)?;
    }
    Ok(0)
}

pub fn rt_sigprocmask(how: u32, set: usize, old: usize, size: usize) -> Result<usize> {
    if size != 8 {
        return Err(EINVAL);
    }
    let th = current_thread();
    let cur = th.sigmask.load(Ordering::Relaxed) as u64;
    if old != 0 {
        write_user(old, &cur)?;
    }
    if set != 0 {
        let s: u64 = read_user(set)?;
        let n = match how {
            0 => cur | s,
            1 => cur & !s,
            2 => s,
            _ => return Err(EINVAL),
        };
        th.sigmask.store((n & !UNBLOCKABLE) as usize, Ordering::Relaxed);
    }
    Ok(0)
}

pub fn rt_sigpending(set: usize, size: usize) -> Result<usize> {
    if size != 8 {
        return Err(EINVAL);
    }
    let th = current_thread();
    let p = (th.pending.lock().set | th.proc.pending.lock().set) & th.sigmask.load(Ordering::Relaxed) as u64;
    write_user(set, &p)?;
    Ok(0)
}

pub fn rt_sigsuspend(mask: usize, size: usize) -> Result<usize> {
    if size != 8 {
        return Err(EINVAL);
    }
    let m: u64 = read_user(mask)?;
    let th = current_thread();
    let old = th.sigmask.swap((m & !UNBLOCKABLE) as usize, Ordering::Relaxed);
    own().saved_mask = Some(old as u64);
    let wq = sched::WaitQueue::new();
    wq.wait_until(|| false, None, true);
    Err(ERESTARTNOHAND)
}

pub fn rt_sigtimedwait(set: usize, info: usize, timeout: usize, size: usize) -> Result<usize> {
    if size != 8 {
        return Err(EINVAL);
    }
    let want: u64 = read_user::<u64>(set)? & !UNBLOCKABLE;
    let deadline = if timeout != 0 {
        let ts: [i64; 2] = read_user(timeout)?;
        Some(arch::ticks() + crate::time::ns_to_ticks((ts[0] as u64) * 1_000_000_000 + ts[1] as u64))
    } else {
        None
    };
    let th = current_thread();
    let take = || -> Option<SigInfo> {
        // only signals in `want`: take from the thread, then the process
        let blocked = !want;
        th.pending.lock().take(blocked).or_else(|| th.proc.pending.lock().take(blocked))
    };
    // temporarily unblock the wanted signals so that sending them wakes us
    let old = th.sigmask.load(Ordering::Relaxed);
    if let Some(i) = take() {
        return finish_timedwait(i, info);
    }
    th.sigmask.store(old & !(want as usize), Ordering::Relaxed);
    let wq = sched::WaitQueue::new();
    let mut got = None;
    let r = wq.wait_until(
        || {
            got = take();
            got.is_some()
        },
        deadline,
        true,
    );
    th.sigmask.store(old, Ordering::Relaxed);
    if let Some(i) = got {
        return finish_timedwait(i, info);
    }
    match r {
        sched::WaitResult::TimedOut => Err(EAGAIN),
        _ => Err(EINTR),
    }
}

fn finish_timedwait(i: SigInfo, info: usize) -> Result<usize> {
    if info != 0 {
        copy_to_user(info, &i.to_bytes())?;
    }
    Ok(i.signo as usize)
}

pub fn sigaltstack(ss: usize, old: usize) -> Result<usize> {
    let o = own();
    if old != 0 {
        let (sp, size, flags) = o.altstack;
        let sp_now = arch::frame_sp(unsafe { &*o.frame });
        let on = flags & signal::SS_DISABLE == 0 && sp_now > sp && sp_now <= sp + size;
        let mut b = [0u8; 24];
        b[0..8].copy_from_slice(&(sp as u64).to_le_bytes());
        b[8..12].copy_from_slice(&(if on { signal::SS_ONSTACK } else { flags }).to_le_bytes());
        b[16..24].copy_from_slice(&(size as u64).to_le_bytes());
        copy_to_user(old, &b)?;
    }
    if ss != 0 {
        let mut b = [0u8; 24];
        copy_from_user(&mut b, ss)?;
        let sp = u64::from_le_bytes(b[0..8].try_into().unwrap()) as usize;
        let flags = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let size = u64::from_le_bytes(b[16..24].try_into().unwrap()) as usize;
        if flags & !(signal::SS_DISABLE | signal::SS_AUTODISARM | signal::SS_ONSTACK) != 0 {
            return Err(EINVAL);
        }
        if flags & signal::SS_DISABLE != 0 {
            o.altstack = (0, 0, signal::SS_DISABLE);
        } else {
            if size < 2048 {
                return Err(ENOMEM);
            }
            o.altstack = (sp, size, flags & signal::SS_AUTODISARM);
        }
    }
    Ok(0)
}

fn may_signal(target: &Process, sig: u32) -> bool {
    let me = current_proc().cred();
    let t = target.cred();
    me.capable(5) || me.euid == t.uid || me.euid == t.suid || me.uid == t.uid || me.uid == t.suid || (sig == signal::SIGCONT && current_proc().sid.load(Ordering::Relaxed) == target.sid.load(Ordering::Relaxed))
}

pub fn kill_pgrp(pg: u32, info: SigInfo) -> Result<usize> {
    let procs: Vec<Arc<Process>> = PROCS.lock().values().filter_map(|w| w.upgrade()).filter(|p| p.pgid.load(Ordering::Relaxed) == pg && !p.is_zombie()).collect();
    if procs.is_empty() {
        return Err(ESRCH);
    }
    for p in procs {
        signal::send_process(&p, info);
    }
    Ok(0)
}

pub fn kill(pid: i32, sig: u32) -> Result<usize> {
    if sig > signal::NSIG {
        return Err(EINVAL);
    }
    let info = SigInfo::user(sig, signal::SI_USER);
    let me = current_proc().clone();
    match pid {
        p if p > 0 => {
            let t = proc_by_pid(p as u32).ok_or(ESRCH)?;
            if !may_signal(&t, sig) {
                return Err(EPERM);
            }
            if sig != 0 && !t.is_zombie() {
                signal::send_process(&t, info);
            }
            Ok(0)
        }
        0 => {
            if sig == 0 {
                return Ok(0);
            }
            kill_pgrp(me.pgid.load(Ordering::Relaxed), info)
        }
        -1 => {
            let all: Vec<Arc<Process>> = PROCS.lock().values().filter_map(|w| w.upgrade()).collect();
            for p in all {
                if p.pid != 1 && !Arc::ptr_eq(&p, &me) && p.pid != INIT.get().map_or(0, |i| i.pid) && may_signal(&p, sig) && sig != 0 {
                    signal::send_process(&p, info);
                }
            }
            Ok(0)
        }
        p => {
            if sig == 0 {
                return Ok(0);
            }
            kill_pgrp((-p) as u32, info)
        }
    }
}

pub fn tgkill(tgid: i32, tid: i32, sig: u32) -> Result<usize> {
    if sig > signal::NSIG || tid <= 0 {
        return Err(EINVAL);
    }
    let t = task_by_tid(tid as u32).ok_or(ESRCH)?;
    let th = thread_of(&t).ok_or(ESRCH)?;
    if tgid > 0 && th.proc.pid != tgid as u32 {
        return Err(ESRCH);
    }
    if sig != 0 {
        signal::send_thread(&t, SigInfo::user(sig, signal::SI_TKILL));
    }
    Ok(0)
}

pub fn tkill(tid: i32, sig: u32) -> Result<usize> {
    tgkill(-1, tid, sig)
}

pub fn rt_sigqueueinfo(pid: i32, sig: u32, info: usize) -> Result<usize> {
    let b: [u8; 128] = read_user(info)?;
    let mut i = SigInfo::from_user_bytes(&b);
    i.signo = sig;
    let p = proc_by_pid(pid as u32).ok_or(ESRCH)?;
    signal::send_process(&p, i);
    Ok(0)
}

pub fn rt_tgsigqueueinfo(tgid: i32, tid: i32, sig: u32, info: usize) -> Result<usize> {
    let b: [u8; 128] = read_user(info)?;
    let mut i = SigInfo::from_user_bytes(&b);
    i.signo = sig;
    let t = task_by_tid(tid as u32).ok_or(ESRCH)?;
    if thread_of(&t).map(|x| x.proc.pid) != Some(tgid as u32) {
        return Err(ESRCH);
    }
    signal::send_thread(&t, i);
    Ok(0)
}

// ---- process groups and sessions

pub fn getppid() -> Result<usize> {
    Ok(current_proc().parent().map_or(0, |p| p.pid as usize))
}

pub fn setpgid(pid: i32, pgid: i32) -> Result<usize> {
    if pgid < 0 {
        return Err(EINVAL);
    }
    let me = current_proc().clone();
    let target = if pid == 0 { me.clone() } else { proc_by_pid(pid as u32).ok_or(ESRCH)? };
    if !Arc::ptr_eq(&target, &me) && !target.parent().is_some_and(|p| Arc::ptr_eq(&p, &me)) {
        return Err(ESRCH);
    }
    if target.sid.load(Ordering::Relaxed) == target.pid {
        return Err(EPERM);
    }
    let pg = if pgid == 0 { target.pid } else { pgid as u32 };
    target.pgid.store(pg, Ordering::Relaxed);
    Ok(0)
}

pub fn getpgid(pid: i32) -> Result<usize> {
    let p = if pid == 0 { current_proc().clone() } else { proc_by_pid(pid as u32).ok_or(ESRCH)? };
    Ok(p.pgid.load(Ordering::Relaxed) as usize)
}

pub fn getsid(pid: i32) -> Result<usize> {
    let p = if pid == 0 { current_proc().clone() } else { proc_by_pid(pid as u32).ok_or(ESRCH)? };
    Ok(p.sid.load(Ordering::Relaxed) as usize)
}

pub fn setsid() -> Result<usize> {
    let me = current_proc();
    let pid = me.pid;
    let leader_of_group = PROCS.lock().values().filter_map(|w| w.upgrade()).any(|p| p.pgid.load(Ordering::Relaxed) == pid && p.pid != pid) || me.pgid.load(Ordering::Relaxed) == pid;
    if leader_of_group {
        return Err(EPERM);
    }
    me.sid.store(pid, Ordering::Relaxed);
    me.pgid.store(pid, Ordering::Relaxed);
    *me.ctty.lock() = None;
    Ok(pid as usize)
}

// ---- credentials

fn set_cred(f: impl FnOnce(&mut Cred) -> Result<()>) -> Result<usize> {
    let p = current_proc();
    let mut c = p.cred.lock();
    let mut n = c.clone();
    f(&mut n)?;
    // dropping all root ids clears the capabilities (no keepcaps)
    if c.euid == 0 && n.euid != 0 && n.uid != 0 && n.suid != 0 {
        n.cap_eff = 0;
        n.cap_perm = 0;
    } else if c.euid == 0 && n.euid != 0 {
        n.cap_eff = 0;
    } else if c.euid != 0 && n.euid == 0 {
        n.cap_eff = n.cap_perm;
    }
    *c = n;
    Ok(0)
}

pub fn setuid(uid: u32) -> Result<usize> {
    set_cred(|c| {
        if c.capable(7) {
            c.uid = uid;
            c.euid = uid;
            c.suid = uid;
        } else if uid == c.uid || uid == c.suid {
            c.euid = uid;
        } else {
            return Err(EPERM);
        }
        c.fsuid = c.euid;
        Ok(())
    })
}

pub fn setgid(gid: u32) -> Result<usize> {
    set_cred(|c| {
        if c.capable(6) {
            c.gid = gid;
            c.egid = gid;
            c.sgid = gid;
        } else if gid == c.gid || gid == c.sgid {
            c.egid = gid;
        } else {
            return Err(EPERM);
        }
        c.fsgid = c.egid;
        Ok(())
    })
}

fn pick(cur: u32, new: u32) -> u32 {
    if new == u32::MAX { cur } else { new }
}

pub fn setresuid(r: u32, e: u32, s: u32) -> Result<usize> {
    set_cred(|c| {
        let ok = |v: u32| v == u32::MAX || v == c.uid || v == c.euid || v == c.suid;
        if !c.capable(7) && !(ok(r) && ok(e) && ok(s)) {
            return Err(EPERM);
        }
        c.uid = pick(c.uid, r);
        c.euid = pick(c.euid, e);
        c.suid = pick(c.suid, s);
        c.fsuid = c.euid;
        Ok(())
    })
}

pub fn setresgid(r: u32, e: u32, s: u32) -> Result<usize> {
    set_cred(|c| {
        let ok = |v: u32| v == u32::MAX || v == c.gid || v == c.egid || v == c.sgid;
        if !c.capable(6) && !(ok(r) && ok(e) && ok(s)) {
            return Err(EPERM);
        }
        c.gid = pick(c.gid, r);
        c.egid = pick(c.egid, e);
        c.sgid = pick(c.sgid, s);
        c.fsgid = c.egid;
        Ok(())
    })
}

pub fn setreuid(r: u32, e: u32) -> Result<usize> {
    setresuid(r, e, if r != u32::MAX || e != u32::MAX { pick(current_proc().cred.lock().euid, e) } else { u32::MAX })
}

pub fn setregid(r: u32, e: u32) -> Result<usize> {
    setresgid(r, e, u32::MAX)
}

pub fn getresuid(r: usize, e: usize, s: usize) -> Result<usize> {
    let c = current_proc().cred();
    write_user(r, &c.uid)?;
    write_user(e, &c.euid)?;
    write_user(s, &c.suid)?;
    Ok(0)
}

pub fn getresgid(r: usize, e: usize, s: usize) -> Result<usize> {
    let c = current_proc().cred();
    write_user(r, &c.gid)?;
    write_user(e, &c.egid)?;
    write_user(s, &c.sgid)?;
    Ok(0)
}

pub fn setfsuid(u: u32) -> Result<usize> {
    let p = current_proc();
    let mut c = p.cred.lock();
    let old = c.fsuid;
    if u != u32::MAX && (c.capable(7) || u == c.uid || u == c.euid || u == c.suid) {
        c.fsuid = u;
    }
    Ok(old as usize)
}

pub fn setfsgid(g: u32) -> Result<usize> {
    let p = current_proc();
    let mut c = p.cred.lock();
    let old = c.fsgid;
    if g != u32::MAX && (c.capable(6) || g == c.gid || g == c.egid || g == c.sgid) {
        c.fsgid = g;
    }
    Ok(old as usize)
}

pub fn getgroups(size: usize, list: usize) -> Result<usize> {
    let g = current_proc().cred().groups;
    if size == 0 {
        return Ok(g.len());
    }
    if size < g.len() {
        return Err(EINVAL);
    }
    for (i, x) in g.iter().enumerate() {
        write_user(list + 4 * i, x)?;
    }
    Ok(g.len())
}

pub fn setgroups(size: usize, list: usize) -> Result<usize> {
    if size > 65536 {
        return Err(EINVAL);
    }
    let mut g = Vec::with_capacity(size);
    for i in 0..size {
        g.push(read_user::<u32>(list + 4 * i)?);
    }
    let p = current_proc();
    let mut c = p.cred.lock();
    if !c.capable(6) {
        return Err(EPERM);
    }
    c.groups = g;
    Ok(0)
}

pub fn capget(hdr: usize, data: usize) -> Result<usize> {
    let h: [u32; 2] = read_user(hdr)?;
    let c = current_proc().cred();
    if h[0] != 0x2008_0522 && h[0] != 0x2007_1026 {
        write_user(hdr, &[0x2008_0522u32, h[1]])?;
        return if data == 0 { Ok(0) } else { Err(EINVAL) };
    }
    if data != 0 {
        let v: [u32; 6] = [c.cap_eff as u32, c.cap_perm as u32, c.cap_inh as u32, (c.cap_eff >> 32) as u32, (c.cap_perm >> 32) as u32, (c.cap_inh >> 32) as u32];
        write_user(data, &v)?;
    }
    Ok(0)
}

pub fn capset(hdr: usize, data: usize) -> Result<usize> {
    let h: [u32; 2] = read_user(hdr)?;
    if h[1] != 0 && h[1] != current_proc().pid {
        return Err(EPERM);
    }
    let v: [u32; 6] = read_user(data)?;
    let eff = v[0] as u64 | ((v[3] as u64) << 32);
    let perm = v[1] as u64 | ((v[4] as u64) << 32);
    let inh = v[2] as u64 | ((v[5] as u64) << 32);
    let p = current_proc();
    let mut c = p.cred.lock();
    if perm & !c.cap_perm != 0 || eff & !perm != 0 {
        return Err(EPERM);
    }
    c.cap_eff = eff;
    c.cap_perm = perm;
    c.cap_inh = inh;
    Ok(0)
}

// ---- system information

pub static HOSTNAME: SpinLock<Vec<u8>> = SpinLock::new(Vec::new());

pub fn hostname() -> Vec<u8> {
    let h = HOSTNAME.lock();
    if h.is_empty() { b"(none)".to_vec() } else { h.clone() }
}

pub fn uname(buf: usize) -> Result<usize> {
    let mut b = [0u8; 65 * 6];
    let put = |b: &mut [u8], i: usize, s: &[u8]| {
        let n = s.len().min(64);
        b[65 * i..65 * i + n].copy_from_slice(&s[..n]);
    };
    put(&mut b, 0, b"Linux");
    put(&mut b, 1, &hostname());
    put(&mut b, 2, crate::RELEASE.as_bytes());
    put(&mut b, 3, crate::VERSION.as_bytes());
    put(&mut b, 4, arch::PLATFORM);
    put(&mut b, 5, b"(none)");
    copy_to_user(buf, &b)?;
    Ok(0)
}

pub fn sethostname(name: usize, len: usize) -> Result<usize> {
    if len > 64 {
        return Err(EINVAL);
    }
    if !current_proc().cred().capable(21) {
        return Err(EPERM);
    }
    let mut b = alloc::vec![0u8; len];
    copy_from_user(&mut b, name)?;
    *HOSTNAME.lock() = b;
    Ok(0)
}

pub fn sysinfo(buf: usize) -> Result<usize> {
    use crate::mm::frame;
    let mut b = [0u8; 112];
    let up = crate::time::monotonic_ns() / 1_000_000_000;
    let total = frame::TOTAL_PAGES.load(Ordering::Relaxed) as u64 * 4096;
    let free = frame::free_count() as u64 * 4096;
    b[0..8].copy_from_slice(&up.to_le_bytes());
    b[32..40].copy_from_slice(&total.to_le_bytes());
    b[40..48].copy_from_slice(&free.to_le_bytes());
    let procs = PROCS.lock().len() as u16;
    b[80..82].copy_from_slice(&procs.to_le_bytes());
    b[104..108].copy_from_slice(&1u32.to_le_bytes());
    copy_to_user(buf, &b)?;
    Ok(0)
}

pub fn getrusage(_who: i32, buf: usize) -> Result<usize> {
    let p = current_proc();
    let mut b = [0u8; 144];
    let ut = p.utime.load(Ordering::Relaxed) as u64;
    b[0..8].copy_from_slice(&(ut / 1_000_000).to_le_bytes());
    b[8..16].copy_from_slice(&(ut % 1_000_000).to_le_bytes());
    copy_to_user(buf, &b)?;
    Ok(0)
}

pub fn prlimit64(pid: i32, res: u32, new: usize, old: usize) -> Result<usize> {
    if res as usize >= 16 {
        return Err(EINVAL);
    }
    let p = if pid == 0 { current_proc().clone() } else { proc_by_pid(pid as u32).ok_or(ESRCH)? };
    let mut r = p.rlimits.lock();
    let cur = r[res as usize];
    if new != 0 {
        let n: [u64; 2] = read_user(new)?;
        if n[0] > n[1] {
            return Err(EINVAL);
        }
        if n[1] > cur.1 && !current_proc().cred().capable(24) {
            return Err(EPERM);
        }
        r[res as usize] = (n[0], n[1]);
    }
    drop(r);
    if old != 0 {
        write_user(old, &[cur.0, cur.1])?;
    }
    Ok(0)
}

pub fn sched_set(_pid: i32) -> Result<usize> {
    Ok(0)
}

pub fn sched_getparam(p: usize) -> Result<usize> {
    write_user(p, &0i32)?;
    Ok(0)
}

pub fn sched_getaffinity(_pid: i32, size: usize, mask: usize) -> Result<usize> {
    let n = crate::percpu::ncpus();
    let bytes = 8usize.max(n.div_ceil(8));
    if size < bytes {
        return Err(EINVAL);
    }
    let m: u64 = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    write_user(mask, &m)?;
    Ok(8)
}

pub fn getcpu(cpu: usize, node: usize) -> Result<usize> {
    if cpu != 0 {
        write_user(cpu, &(crate::percpu::this().id as u32))?;
    }
    if node != 0 {
        write_user(node, &0u32)?;
    }
    Ok(0)
}

pub fn getrandom(buf: usize, len: usize, _flags: u32) -> Result<usize> {
    let mut done = 0;
    let mut kb = [0u8; 256];
    while done < len {
        let n = (len - done).min(256);
        crate::random::fill(&mut kb[..n]);
        copy_to_user(buf + done, &kb[..n])?;
        done += n;
    }
    Ok(len)
}

pub fn membarrier(cmd: u32) -> Result<usize> {
    match cmd {
        0 => Ok(1 | 8 | 16), // QUERY: GLOBAL, PRIVATE_EXPEDITED, REGISTER_PRIVATE_EXPEDITED
        _ => {
            // every CPU passes through a barrier when it switches tasks; an IPI to all makes it now
            core::sync::atomic::fence(Ordering::SeqCst);
            for c in 0..crate::percpu::ncpus() {
                if c != crate::percpu::this().id {
                    arch::send_resched_ipi(c);
                }
            }
            Ok(0)
        }
    }
}

pub fn reboot(m1: u32, m2: u32, cmd: u32) -> Result<usize> {
    if m1 != 0xfee1_dead || !matches!(m2, 672274793 | 85072278 | 369367448 | 537993216) {
        return Err(EINVAL);
    }
    if !current_proc().cred().capable(22) {
        return Err(EPERM);
    }
    match cmd {
        0x4321_fedc | 0xcdef_0123 => {
            crate::fs::sync_all();
            println!("ruxos: power off");
            crate::power::poweroff(&crate::FDT)
        }
        0x0123_4567 => {
            crate::fs::sync_all();
            crate::power::reboot(&crate::FDT)
        }
        0x89ab_cdef | 0 => Ok(0), // CAD on/off
        _ => Err(EINVAL),
    }
}

pub fn prctl(op: u32, a2: usize, a3: usize, _a4: usize, _a5: usize) -> Result<usize> {
    let th = current_thread();
    let p = &th.proc;
    match op {
        1 => {
            if a2 > 64 {
                return Err(EINVAL);
            }
            p.pdeath_signal.store(a2 as u32, Ordering::Relaxed);
            Ok(0)
        }
        2 => {
            write_user(a2, &(p.pdeath_signal.load(Ordering::Relaxed) as i32))?;
            Ok(0)
        }
        3 => Ok(p.dumpable.load(Ordering::Relaxed) as usize),
        4 => {
            if a2 > 1 {
                return Err(EINVAL);
            }
            p.dumpable.store(a2 as u32, Ordering::Relaxed);
            Ok(0)
        }
        7 | 8 => Ok(0), // PR_GET/SET_KEEPCAPS
        15 => {
            let mut n = [0u8; 16];
            let s = crate::mm::vm::read_cstr(a2, 16).unwrap_or_default();
            let k = s.len().min(15);
            n[..k].copy_from_slice(&s[..k]);
            *th.comm.lock() = n;
            Ok(0)
        }
        16 => {
            let n = *th.comm.lock();
            copy_to_user(a2, &n)?;
            Ok(0)
        }
        21 => Ok(0), // PR_GET_SECCOMP
        23 => {
            if a2 > 63 {
                return Err(EINVAL);
            }
            Ok((p.cred.lock().cap_bnd >> a2) as usize & 1)
        }
        24 => {
            if a2 > 63 {
                return Err(EINVAL);
            }
            let mut c = p.cred.lock();
            if !c.capable(8) {
                return Err(EPERM);
            }
            c.cap_bnd &= !(1 << a2);
            Ok(0)
        }
        36 | 37 => Ok(0), // child subreaper
        38 => {
            if a2 != 1 || a3 != 0 {
                return Err(EINVAL);
            }
            p.cred.lock().no_new_privs = true;
            Ok(0)
        }
        39 => Ok(p.cred.lock().no_new_privs as usize),
        41 | 42 => Ok(0), // THP disable
        47 => Ok(0),      // PR_CAP_AMBIENT
        _ => Err(EINVAL),
    }
}
