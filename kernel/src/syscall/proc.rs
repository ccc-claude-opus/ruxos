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
        p if p < 0 => WaitWho::Pgid(crate::task::gpid((-p) as u32).unwrap_or(u32::MAX)),
        p => WaitWho::Pid(crate::task::gpid(p as u32).unwrap_or(u32::MAX)),
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
        1 => WaitWho::Pid(crate::task::gpid(id as u32).unwrap_or(u32::MAX)),
        2 => WaitWho::Pgid(if id == 0 { current_proc().pgid.load(Ordering::Relaxed) } else { crate::task::gpid(id as u32).unwrap_or(u32::MAX) }),
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
    Ok(crate::task::vpid(sched::current().tid) as usize)
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
        let t = crate::task::task_by_vtid(tid as u32).ok_or(ESRCH)?;
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
            let t = crate::task::proc_by_vpid(p as u32).ok_or(ESRCH)?;
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
            // every process visible in our PID namespace but its init and ourselves
            let all: Vec<Arc<Process>> = PROCS.lock().values().filter_map(|w| w.upgrade()).filter(|p| me.pid_ns.contains(&p.pid_ns)).collect();
            let reaper = me.pid_ns.reaper.lock().upgrade();
            for p in all {
                let is_init = reaper.as_ref().is_some_and(|r| Arc::ptr_eq(r, &p)) || p.pid == INIT.get().map_or(0, |i| i.pid);
                if !is_init && !Arc::ptr_eq(&p, &me) && may_signal(&p, sig) && sig != 0 {
                    signal::send_process(&p, info);
                }
            }
            Ok(0)
        }
        p => {
            if sig == 0 {
                return Ok(0);
            }
            kill_pgrp(crate::task::gpid((-p) as u32).ok_or(ESRCH)?, info)
        }
    }
}

pub fn tgkill(tgid: i32, tid: i32, sig: u32) -> Result<usize> {
    if sig > signal::NSIG || tid <= 0 {
        return Err(EINVAL);
    }
    let t = crate::task::task_by_vtid(tid as u32).ok_or(ESRCH)?;
    let th = thread_of(&t).ok_or(ESRCH)?;
    if tgid > 0 && Some(th.proc.pid) != crate::task::gpid(tgid as u32) {
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
    let p = crate::task::proc_by_vpid(pid as u32).ok_or(ESRCH)?;
    signal::send_process(&p, i);
    Ok(0)
}

pub fn rt_tgsigqueueinfo(tgid: i32, tid: i32, sig: u32, info: usize) -> Result<usize> {
    let b: [u8; 128] = read_user(info)?;
    let mut i = SigInfo::from_user_bytes(&b);
    i.signo = sig;
    let t = crate::task::task_by_vtid(tid as u32).ok_or(ESRCH)?;
    if thread_of(&t).map(|x| x.proc.pid) != crate::task::gpid(tgid as u32) {
        return Err(ESRCH);
    }
    signal::send_thread(&t, i);
    Ok(0)
}

// ---- process groups and sessions

pub fn getppid() -> Result<usize> {
    Ok(current_proc().parent().map_or(0, |p| crate::task::vpid(p.pid) as usize))
}

pub fn setpgid(pid: i32, pgid: i32) -> Result<usize> {
    if pgid < 0 {
        return Err(EINVAL);
    }
    let me = current_proc().clone();
    let target = if pid == 0 { me.clone() } else { crate::task::proc_by_vpid(pid as u32).ok_or(ESRCH)? };
    if !Arc::ptr_eq(&target, &me) && !target.parent().is_some_and(|p| Arc::ptr_eq(&p, &me)) {
        return Err(ESRCH);
    }
    if target.sid.load(Ordering::Relaxed) == target.pid {
        return Err(EPERM);
    }
    let pg = if pgid == 0 { target.pid } else { crate::task::gpid(pgid as u32).ok_or(EPERM)? };
    target.pgid.store(pg, Ordering::Relaxed);
    Ok(0)
}

pub fn getpgid(pid: i32) -> Result<usize> {
    let p = if pid == 0 { current_proc().clone() } else { crate::task::proc_by_vpid(pid as u32).ok_or(ESRCH)? };
    Ok(crate::task::vpid(p.pgid.load(Ordering::Relaxed)) as usize)
}

pub fn getsid(pid: i32) -> Result<usize> {
    let p = if pid == 0 { current_proc().clone() } else { crate::task::proc_by_vpid(pid as u32).ok_or(ESRCH)? };
    Ok(crate::task::vpid(p.sid.load(Ordering::Relaxed)) as usize)
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
    Ok(crate::task::vpid(pid) as usize)
}

// ---- credentials (ids are translated through the caller's user namespace; capabilities count in
// that namespace)

/// The capabilities of the file system ids (CAP_FS_MASK): CHOWN, DAC_OVERRIDE, DAC_READ_SEARCH,
/// FOWNER, FSETID, LINUX_IMMUTABLE, MKNOD, MAC_OVERRIDE.
const CAP_FS_MASK: u64 = 0x1f | (1 << 9) | (1 << 27) | (1 << 32);

fn set_cred(f: impl FnOnce(&mut Cred) -> Result<()>) -> Result<usize> {
    let p = current_proc();
    let mut c = p.cred.lock();
    let mut n = c.clone();
    f(&mut n)?;
    // capabilities follow the uids (capabilities(7), "Effect of user ID changes on capabilities")
    let root = c.user_ns.make_kuid(0);
    let is = |u: u32| Some(u) == root;
    if (is(c.uid) || is(c.euid) || is(c.suid)) && !is(n.uid) && !is(n.euid) && !is(n.suid) {
        if !n.keepcaps {
            n.cap_perm = 0;
            n.cap_eff = 0;
        }
        n.cap_amb = 0;
    }
    if is(c.euid) && !is(n.euid) {
        n.cap_eff = 0;
    }
    if !is(c.euid) && is(n.euid) {
        n.cap_eff = n.cap_perm;
    }
    if is(c.fsuid) && !is(n.fsuid) {
        n.cap_eff &= !CAP_FS_MASK;
    }
    if !is(c.fsuid) && is(n.fsuid) {
        n.cap_eff |= n.cap_perm & CAP_FS_MASK;
    }
    // a process whose ids change is no longer dumpable (its /proc files become root's), as Linux
    if c.euid != n.euid || c.egid != n.egid || c.fsuid != n.fsuid || c.fsgid != n.fsgid {
        p.dumpable.store(0, Ordering::Relaxed);
    }
    *c = n;
    Ok(0)
}

/// A uid argument (-1 = unchanged) as a kernel uid.
fn kuid(c: &Cred, u: u32) -> Result<u32> {
    if u == u32::MAX { Ok(u) } else { c.user_ns.make_kuid(u).ok_or(EINVAL) }
}
fn kgid(c: &Cred, g: u32) -> Result<u32> {
    if g == u32::MAX { Ok(g) } else { c.user_ns.make_kgid(g).ok_or(EINVAL) }
}

pub fn getuid() -> Result<usize> {
    let c = current_proc().cred();
    Ok(c.user_ns.from_kuid(c.uid) as usize)
}
pub fn geteuid() -> Result<usize> {
    let c = current_proc().cred();
    Ok(c.user_ns.from_kuid(c.euid) as usize)
}
pub fn getgid() -> Result<usize> {
    let c = current_proc().cred();
    Ok(c.user_ns.from_kgid(c.gid) as usize)
}
pub fn getegid() -> Result<usize> {
    let c = current_proc().cred();
    Ok(c.user_ns.from_kgid(c.egid) as usize)
}

pub fn setuid(uid: u32) -> Result<usize> {
    set_cred(|c| {
        let uid = kuid(c, uid)?;
        if uid == u32::MAX {
            return Err(EINVAL);
        }
        if c.own_capable(7) {
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
        let gid = kgid(c, gid)?;
        if gid == u32::MAX {
            return Err(EINVAL);
        }
        if c.own_capable(6) {
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
        let (r, e, s) = (kuid(c, r)?, kuid(c, e)?, kuid(c, s)?);
        let ok = |v: u32| v == u32::MAX || v == c.uid || v == c.euid || v == c.suid;
        if !c.own_capable(7) && !(ok(r) && ok(e) && ok(s)) {
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
        let (r, e, s) = (kgid(c, r)?, kgid(c, e)?, kgid(c, s)?);
        let ok = |v: u32| v == u32::MAX || v == c.gid || v == c.egid || v == c.sgid;
        if !c.own_capable(6) && !(ok(r) && ok(e) && ok(s)) {
            return Err(EPERM);
        }
        c.gid = pick(c.gid, r);
        c.egid = pick(c.egid, e);
        c.sgid = pick(c.sgid, s);
        c.fsgid = c.egid;
        Ok(())
    })
}

/// setreuid(2): the saved uid becomes the new effective uid when the real uid is set, or the
/// effective uid is set to something other than the old real uid.
pub fn setreuid(r: u32, e: u32) -> Result<usize> {
    set_cred(|c| {
        let (r, e) = (kuid(c, r)?, kuid(c, e)?);
        let cap = c.own_capable(7);
        if r != u32::MAX && !cap && r != c.uid && r != c.euid {
            return Err(EPERM);
        }
        if e != u32::MAX && !cap && e != c.uid && e != c.euid && e != c.suid {
            return Err(EPERM);
        }
        let old_r = c.uid;
        c.uid = pick(c.uid, r);
        c.euid = pick(c.euid, e);
        if r != u32::MAX || (e != u32::MAX && e != old_r) {
            c.suid = c.euid;
        }
        c.fsuid = c.euid;
        Ok(())
    })
}

pub fn setregid(r: u32, e: u32) -> Result<usize> {
    set_cred(|c| {
        let (r, e) = (kgid(c, r)?, kgid(c, e)?);
        let cap = c.own_capable(6);
        if r != u32::MAX && !cap && r != c.gid && r != c.egid {
            return Err(EPERM);
        }
        if e != u32::MAX && !cap && e != c.gid && e != c.egid && e != c.sgid {
            return Err(EPERM);
        }
        let old_r = c.gid;
        c.gid = pick(c.gid, r);
        c.egid = pick(c.egid, e);
        if r != u32::MAX || (e != u32::MAX && e != old_r) {
            c.sgid = c.egid;
        }
        c.fsgid = c.egid;
        Ok(())
    })
}

pub fn getresuid(r: usize, e: usize, s: usize) -> Result<usize> {
    let c = current_proc().cred();
    let n = &c.user_ns;
    write_user(r, &n.from_kuid(c.uid))?;
    write_user(e, &n.from_kuid(c.euid))?;
    write_user(s, &n.from_kuid(c.suid))?;
    Ok(0)
}

pub fn getresgid(r: usize, e: usize, s: usize) -> Result<usize> {
    let c = current_proc().cred();
    let n = &c.user_ns;
    write_user(r, &n.from_kgid(c.gid))?;
    write_user(e, &n.from_kgid(c.egid))?;
    write_user(s, &n.from_kgid(c.sgid))?;
    Ok(0)
}

/// setfsuid(2) returns the old fsuid whether or not the change was allowed.
pub fn setfsuid(u: u32) -> Result<usize> {
    let old = {
        let c = current_proc().cred();
        c.user_ns.from_kuid(c.fsuid)
    };
    let _ = set_cred(|c| {
        let Ok(u) = kuid(c, u) else { return Ok(()) };
        if u != u32::MAX && (c.own_capable(7) || u == c.uid || u == c.euid || u == c.suid || u == c.fsuid) {
            c.fsuid = u;
        }
        Ok(())
    });
    Ok(old as usize)
}

pub fn setfsgid(g: u32) -> Result<usize> {
    let old = {
        let c = current_proc().cred();
        c.user_ns.from_kgid(c.fsgid)
    };
    let _ = set_cred(|c| {
        let Ok(g) = kgid(c, g) else { return Ok(()) };
        if g != u32::MAX && (c.own_capable(6) || g == c.gid || g == c.egid || g == c.sgid || g == c.fsgid) {
            c.fsgid = g;
        }
        Ok(())
    });
    Ok(old as usize)
}

pub fn getgroups(size: usize, list: usize) -> Result<usize> {
    let c = current_proc().cred();
    let g = &c.groups;
    if size == 0 {
        return Ok(g.len());
    }
    if size < g.len() {
        return Err(EINVAL);
    }
    for (i, x) in g.iter().enumerate() {
        write_user(list + 4 * i, &c.user_ns.from_kgid(*x))?;
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
    // CAP_SETGID in the own namespace, unless the namespace's setgroups is "deny"
    if !c.own_capable(6) || !c.user_ns.setgroups_allowed.load(Ordering::Relaxed) || c.user_ns.gid_map.lock().is_none() {
        return Err(EPERM);
    }
    let mut k = Vec::with_capacity(g.len());
    for x in g {
        k.push(c.user_ns.make_kgid(x).ok_or(EINVAL)?);
    }
    k.sort_unstable();
    k.dedup();
    c.groups = k;
    Ok(0)
}

const CAP_V1: u32 = 0x1998_0330;
const CAP_V2: u32 = 0x2007_1026;
const CAP_V3: u32 = 0x2008_0522;

pub fn capget(hdr: usize, data: usize) -> Result<usize> {
    let h: [u32; 2] = read_user(hdr)?;
    if h[0] != CAP_V3 && h[0] != CAP_V2 && h[0] != CAP_V1 {
        write_user(hdr, &[CAP_V3, h[1]])?;
        return if data == 0 { Ok(0) } else { Err(EINVAL) };
    }
    let c = if h[1] == 0 {
        current_proc().cred()
    } else {
        let g = current_proc().pid_ns.global_of(h[1]).ok_or(ESRCH)?;
        crate::task::proc_by_pid(g).ok_or(ESRCH)?.cred()
    };
    if data != 0 {
        if h[0] == CAP_V1 {
            write_user(data, &[c.cap_eff as u32, c.cap_perm as u32, c.cap_inh as u32])?;
        } else {
            let v: [u32; 6] = [c.cap_eff as u32, c.cap_perm as u32, c.cap_inh as u32, (c.cap_eff >> 32) as u32, (c.cap_perm >> 32) as u32, (c.cap_inh >> 32) as u32];
            write_user(data, &v)?;
        }
    }
    Ok(0)
}

pub fn capset(hdr: usize, data: usize) -> Result<usize> {
    let h: [u32; 2] = read_user(hdr)?;
    if h[0] != CAP_V3 && h[0] != CAP_V2 && h[0] != CAP_V1 {
        write_user(hdr, &[CAP_V3, h[1]])?;
        return Err(EINVAL);
    }
    let p = current_proc();
    if h[1] != 0 && Some(p.pid) != p.pid_ns.global_of(h[1]) {
        return Err(EPERM);
    }
    let (eff, perm, inh) = if h[0] == CAP_V1 {
        let v: [u32; 3] = read_user(data)?;
        let c = p.cred();
        // the upper 32 capabilities stay as they are
        (v[0] as u64 | (c.cap_eff & !0xffff_ffff), v[1] as u64 | (c.cap_perm & !0xffff_ffff), v[2] as u64 | (c.cap_inh & !0xffff_ffff))
    } else {
        let v: [u32; 6] = read_user(data)?;
        (v[0] as u64 | ((v[3] as u64) << 32), v[1] as u64 | ((v[4] as u64) << 32), v[2] as u64 | ((v[5] as u64) << 32))
    };
    let mut c = p.cred.lock();
    // capset(2): inheritable within (inheritable | permitted), or the bounding set with CAP_SETPCAP;
    // permitted can only shrink; effective within the new permitted
    let inh_limit = if c.own_capable(8) { c.cap_inh | c.cap_bnd } else { c.cap_inh | c.cap_perm };
    if inh & !inh_limit != 0 || inh & !(c.cap_inh | c.cap_bnd) != 0 || perm & !c.cap_perm != 0 || eff & !perm != 0 {
        return Err(EPERM);
    }
    c.cap_eff = eff & crate::task::CAP_ALL;
    c.cap_perm = perm & crate::task::CAP_ALL;
    c.cap_inh = inh & crate::task::CAP_ALL;
    // ambient capabilities must stay permitted and inheritable
    c.cap_amb &= c.cap_perm & c.cap_inh;
    Ok(0)
}

// ---- system information

/// The UTS namespace of the current process (the initial one for kernel threads).
fn uts() -> Option<Arc<crate::ns::UtsNs>> {
    crate::task::try_current_proc().map(|p| p.ns.lock().uts.clone())
}

pub fn hostname() -> Vec<u8> {
    let h = uts().map(|u| u.hostname.lock().clone()).unwrap_or_default();
    if h.is_empty() { b"(none)".to_vec() } else { h }
}

pub fn domainname() -> Vec<u8> {
    uts().map(|u| u.domainname.lock().clone()).unwrap_or_else(|| b"(none)".to_vec())
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
    put(&mut b, 5, &domainname());
    copy_to_user(buf, &b)?;
    Ok(0)
}

/// Sets the host name of the caller's UTS namespace (CAP_SYS_ADMIN over that namespace).
pub fn set_hostname(h: Vec<u8>) -> Result<()> {
    if h.len() > 64 {
        return Err(EINVAL);
    }
    let u = uts().ok_or(EPERM)?;
    if !current_proc().cred().ns_capable(&u.user_ns, 21) {
        return Err(EPERM);
    }
    *u.hostname.lock() = h;
    Ok(())
}

pub fn sethostname(name: usize, len: usize) -> Result<usize> {
    if len > 64 {
        return Err(EINVAL);
    }
    let mut b = alloc::vec![0u8; len];
    copy_from_user(&mut b, name)?;
    set_hostname(b)?;
    Ok(0)
}

pub fn setdomainname(name: usize, len: usize) -> Result<usize> {
    if len > 64 {
        return Err(EINVAL);
    }
    let mut b = alloc::vec![0u8; len];
    copy_from_user(&mut b, name)?;
    let u = uts().ok_or(EPERM)?;
    if !current_proc().cred().ns_capable(&u.user_ns, 21) {
        return Err(EPERM);
    }
    *u.domainname.lock() = b;
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
    let p = if pid == 0 { current_proc().clone() } else { crate::task::proc_by_vpid(pid as u32).ok_or(ESRCH)? };
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
            crate::fs::unmount_all();
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
        7 => Ok(p.cred.lock().keepcaps as usize), // PR_GET_KEEPCAPS
        8 => {
            // PR_SET_KEEPCAPS
            if a2 > 1 {
                return Err(EINVAL);
            }
            p.cred.lock().keepcaps = a2 == 1;
            Ok(0)
        }
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
        21 => Ok(crate::seccomp::mode() as usize), // PR_GET_SECCOMP
        22 => crate::seccomp::set_mode(a2 as u32, 0, a3), // PR_SET_SECCOMP
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
            if !c.own_capable(8) {
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
        47 => {
            // PR_CAP_AMBIENT: IS_SET 1, RAISE 2, LOWER 3, CLEAR_ALL 4
            let mut c = p.cred.lock();
            match a2 {
                4 => {
                    c.cap_amb = 0;
                    Ok(0)
                }
                1..=3 if a3 > 40 => Err(EINVAL),
                1 => Ok((c.cap_amb >> a3) as usize & 1),
                2 => {
                    let bit = 1u64 << a3;
                    if c.cap_perm & bit == 0 || c.cap_inh & bit == 0 {
                        return Err(EPERM);
                    }
                    c.cap_amb |= bit;
                    Ok(0)
                }
                3 => {
                    c.cap_amb &= !(1u64 << a3);
                    Ok(0)
                }
                _ => Err(EINVAL),
            }
        }
        _ => Err(EINVAL),
    }
}

// ---- namespaces

/// unshare(2): moves the caller into new namespaces, or gives it its own fs / fd table.
pub fn unshare(mut flags: u64) -> Result<usize> {
    use crate::task::*;
    let known = CLONE_FILES | CLONE_FS | CLONE_SYSVSEM | CLONE_THREAD | CLONE_SIGHAND | CLONE_VM | crate::ns::CLONE_NEW_ANY;
    if flags & !known != 0 {
        return Err(EINVAL);
    }
    if flags & crate::ns::CLONE_NEWUSER != 0 {
        flags |= CLONE_THREAD | CLONE_FS;
    }
    if flags & crate::ns::CLONE_NEWNS != 0 {
        flags |= CLONE_FS;
    }
    let me = current_thread();
    let p = me.proc.clone();
    // sharing threads, handlers or memory can only be "undone" by a process with one thread
    if flags & (CLONE_THREAD | CLONE_SIGHAND | CLONE_VM) != 0 && p.live_threads.load(Ordering::SeqCst) > 1 {
        return Err(EINVAL);
    }
    if flags & CLONE_VM != 0 && Arc::strong_count(&me.mm) > 1 + p.live_threads.load(Ordering::SeqCst) {
        return Err(EINVAL);
    }
    let task = sched::current_arc();
    // SAFETY: the current thread's own state
    let th = unsafe { task.user_mut().ok_or(EINVAL)? };
    let mut fs = {
        let f = th.fs.lock();
        FsContext { root: f.root.clone(), cwd: f.cwd.clone(), umask: f.umask }
    };
    let mut cred = p.cred();
    if flags & crate::ns::CLONE_NEWUSER != 0 {
        cred = new_user_cred(&cred, &fs)?;
    }
    let old = p.ns.lock().clone();
    let nsp = crate::ns::copy_namespaces(flags, &old, &cred, Some(&mut fs), &crate::cgroup::path_of(&p))?;
    // commit
    if flags & CLONE_FILES != 0 {
        let copy = th.files.lock().clone();
        th.files = Arc::new(SpinLock::new(copy));
    }
    if flags & CLONE_FS != 0 {
        th.fs = Arc::new(SpinLock::new(fs));
    }
    *p.ns.lock() = nsp;
    *p.cred.lock() = cred;
    Ok(0)
}
