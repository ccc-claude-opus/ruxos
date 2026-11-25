//! Signals: actions, pending sets, sending, delivery on the way back to user mode (frames built
//! by the architecture code in the Linux layout), and sigreturn.
use super::{current_thread, own, thread_of, Process, UserThread};
use crate::arch::{self, TrapFrame};
use crate::errno::*;
use crate::sched::{self, Task};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::Ordering;

pub const SIGHUP: u32 = 1;
pub const SIGINT: u32 = 2;
pub const SIGQUIT: u32 = 3;
pub const SIGILL: u32 = 4;
pub const SIGTRAP: u32 = 5;
pub const SIGABRT: u32 = 6;
pub const SIGBUS: u32 = 7;
pub const SIGFPE: u32 = 8;
pub const SIGKILL: u32 = 9;
pub const SIGUSR1: u32 = 10;
pub const SIGSEGV: u32 = 11;
pub const SIGUSR2: u32 = 12;
pub const SIGPIPE: u32 = 13;
pub const SIGALRM: u32 = 14;
pub const SIGTERM: u32 = 15;
pub const SIGCHLD: u32 = 17;
pub const SIGCONT: u32 = 18;
pub const SIGSTOP: u32 = 19;
pub const SIGTSTP: u32 = 20;
pub const SIGTTIN: u32 = 21;
pub const SIGTTOU: u32 = 22;
pub const SIGURG: u32 = 23;
pub const SIGXCPU: u32 = 24;
pub const SIGXFSZ: u32 = 25;
pub const SIGVTALRM: u32 = 26;
pub const SIGPROF: u32 = 27;
pub const SIGWINCH: u32 = 28;
pub const SIGIO: u32 = 29;
pub const SIGPWR: u32 = 30;
pub const SIGSYS: u32 = 31;
pub const NSIG: u32 = 64;

pub const SA_NOCLDSTOP: u64 = 1;
pub const SA_NOCLDWAIT: u64 = 2;
pub const SA_SIGINFO: u64 = 4;
pub const SA_RESTORER: u64 = 0x0400_0000;
pub const SA_ONSTACK: u64 = 0x0800_0000;
pub const SA_RESTART: u64 = 0x1000_0000;
pub const SA_NODEFER: u64 = 0x4000_0000;
pub const SA_RESETHAND: u64 = 0x8000_0000;

pub const SS_ONSTACK: u32 = 1;
pub const SS_DISABLE: u32 = 2;
pub const SS_AUTODISARM: u32 = 1 << 31;

pub const SIG_DFL: usize = 0;
pub const SIG_IGN: usize = 1;

pub fn bit(sig: u32) -> u64 {
    1u64 << (sig - 1)
}

/// Signals that can never be blocked or caught.
pub const UNBLOCKABLE: u64 = (1 << (SIGKILL - 1)) | (1 << (SIGSTOP - 1));

#[derive(Clone, Copy, Default)]
pub struct SigAction {
    pub handler: usize,
    pub flags: u64,
    pub restorer: usize,
    pub mask: u64,
}

#[derive(Clone)]
pub struct SigHandlers {
    pub act: [SigAction; 65],
}

impl Default for SigHandlers {
    fn default() -> Self {
        SigHandlers { act: [SigAction::default(); 65] }
    }
}

impl SigHandlers {
    /// execve: caught signals go back to their default action; ignored ones stay ignored.
    pub fn reset_handled(&mut self) {
        for a in self.act.iter_mut() {
            if a.handler != SIG_IGN {
                *a = SigAction::default();
            } else {
                a.flags = 0;
                a.mask = 0;
            }
        }
    }
    pub fn auto_reap(&self) -> bool {
        let a = &self.act[SIGCHLD as usize];
        a.handler == SIG_IGN || a.flags & SA_NOCLDWAIT != 0
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct SigInfo {
    pub signo: u32,
    pub errno: i32,
    pub code: i32,
    pub pid: i32,
    pub uid: u32,
    pub status: i32,
    pub addr: usize,
    pub value: usize,
    pub utime: i64,
    pub stime: i64,
    pub fd: i32,
    pub band: i64,
}

pub const SI_USER: i32 = 0;
pub const SI_KERNEL: i32 = 0x80;
pub const SI_QUEUE: i32 = -1;
pub const SI_TKILL: i32 = -6;

impl SigInfo {
    pub fn kernel(sig: u32) -> SigInfo {
        SigInfo { signo: sig, code: SI_KERNEL, ..Default::default() }
    }

    pub fn user(sig: u32, code: i32) -> SigInfo {
        let p = super::current_proc();
        SigInfo { signo: sig, code, pid: p.pid as i32, uid: p.cred.lock().uid, ..Default::default() }
    }

    pub fn fault(sig: u32, code: i32, addr: usize) -> SigInfo {
        SigInfo { signo: sig, code, addr, ..Default::default() }
    }

    /// The 128-byte `siginfo_t` (union fields by signal and code).
    pub fn to_bytes(&self) -> [u8; 128] {
        let mut b = [0u8; 128];
        b[0..4].copy_from_slice(&(self.signo as i32).to_le_bytes());
        b[4..8].copy_from_slice(&self.errno.to_le_bytes());
        b[8..12].copy_from_slice(&self.code.to_le_bytes());
        let fault = matches!(self.signo, SIGSEGV | SIGBUS | SIGILL | SIGFPE | SIGTRAP) && self.code > 0;
        if fault {
            b[16..24].copy_from_slice(&(self.addr as u64).to_le_bytes());
        } else if self.signo == SIGCHLD && self.code > 0 {
            b[16..20].copy_from_slice(&self.pid.to_le_bytes());
            b[20..24].copy_from_slice(&self.uid.to_le_bytes());
            b[24..28].copy_from_slice(&self.status.to_le_bytes());
            b[32..40].copy_from_slice(&self.utime.to_le_bytes());
            b[40..48].copy_from_slice(&self.stime.to_le_bytes());
        } else if self.signo == SIGIO && self.code > 0 {
            b[16..24].copy_from_slice(&self.band.to_le_bytes());
            b[24..28].copy_from_slice(&self.fd.to_le_bytes());
        } else {
            b[16..20].copy_from_slice(&self.pid.to_le_bytes());
            b[20..24].copy_from_slice(&self.uid.to_le_bytes());
            b[24..32].copy_from_slice(&(self.value as u64).to_le_bytes());
        }
        b
    }

    pub fn from_user_bytes(b: &[u8; 128]) -> SigInfo {
        let i32at = |o: usize| i32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        SigInfo {
            signo: i32at(0) as u32,
            errno: i32at(4),
            code: i32at(8),
            pid: i32at(16),
            uid: i32at(20) as u32,
            value: u64::from_le_bytes(b[24..32].try_into().unwrap()) as usize,
            ..Default::default()
        }
    }
}

#[derive(Default)]
pub struct SigPending {
    pub set: u64,
    pub queue: VecDeque<SigInfo>,
}

impl SigPending {
    fn add(&mut self, info: SigInfo) {
        let b = bit(info.signo);
        if info.signo < 32 && self.set & b != 0 {
            return; // standard signals are not queued twice
        }
        self.set |= b;
        self.queue.push_back(info);
    }

    /// Takes the lowest-numbered pending signal not in `blocked`.
    pub fn take(&mut self, blocked: u64) -> Option<SigInfo> {
        let ready = self.set & !blocked;
        if ready == 0 {
            return None;
        }
        let sig = ready.trailing_zeros() + 1;
        let pos = self.queue.iter().position(|i| i.signo == sig);
        let info = pos.and_then(|p| self.queue.remove(p)).unwrap_or(SigInfo::kernel(sig));
        if !self.queue.iter().any(|i| i.signo == sig) {
            self.set &= !bit(sig);
        }
        Some(info)
    }

    pub fn remove(&mut self, mask: u64) {
        self.set &= !mask;
        self.queue.retain(|i| bit(i.signo) & mask == 0);
    }
}

fn is_ignored(th: &UserThread, sig: u32) -> bool {
    let a = th.sighand.lock().act[sig as usize];
    a.handler == SIG_IGN || (a.handler == SIG_DFL && default_ignored(sig))
}

fn default_ignored(sig: u32) -> bool {
    matches!(sig, SIGCHLD | SIGURG | SIGWINCH | SIGCONT)
}

fn stop_signal(sig: u32) -> bool {
    matches!(sig, SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU)
}

/// Effects at sending time: SIGCONT resumes and discards stops; stop signals discard SIGCONT.
fn on_send(proc: &Arc<Process>, sig: u32) {
    if sig == SIGCONT || sig == SIGKILL {
        let stop_mask = bit(SIGSTOP) | bit(SIGTSTP) | bit(SIGTTIN) | bit(SIGTTOU);
        proc.pending.lock().remove(stop_mask);
        for t in proc.threads.lock().iter().filter_map(|w| w.upgrade()) {
            if let Some(th) = thread_of(&t) {
                th.pending.lock().remove(stop_mask);
            }
        }
        if proc.stopped.swap(false, Ordering::SeqCst) {
            proc.state.lock().cont_report = true;
            proc.stop_wq.wake_all();
            if let Some(p) = proc.parent() {
                p.child_wq.wake_all();
            }
        }
    } else if stop_signal(sig) {
        proc.pending.lock().remove(bit(SIGCONT));
    }
}

/// Wakes a thread so it notices a signal (interruptible sleeps return).
fn kick(t: &Arc<Task>) {
    sched::wake(t);
    let cpu = t.cpu.load(Ordering::Relaxed);
    if t.state.load(Ordering::Relaxed) == sched::RUNNING && cpu != crate::percpu::this().id {
        arch::send_resched_ipi(cpu);
    }
}

/// signalfds of the process see the new pending signal.
fn notify_signalfds(proc: &Process) {
    let qs: alloc::vec::Vec<_> = proc.signalfds.lock().iter().filter_map(|w| w.upgrade()).collect();
    for q in qs {
        q.notify(crate::vfs::poll::POLLIN);
    }
}

pub fn send_thread(t: &Arc<Task>, info: SigInfo) {
    let Some(th) = thread_of(t) else { return };
    on_send(&th.proc, info.signo);
    if info.signo != SIGKILL && info.signo != SIGCONT && is_ignored(th, info.signo) && !stop_signal(info.signo) {
        return;
    }
    th.pending.lock().add(info);
    notify_signalfds(&th.proc);
    kick(t);
}

pub fn send_process(proc: &Arc<Process>, info: SigInfo) {
    on_send(proc, info.signo);
    let threads: alloc::vec::Vec<_> = proc.threads.lock().iter().filter_map(|w| w.upgrade()).collect();
    let Some(first) = threads.first() else { return };
    if let Some(th) = thread_of(first) {
        if info.signo != SIGKILL && info.signo != SIGCONT && is_ignored(th, info.signo) && !stop_signal(info.signo) {
            return;
        }
    }
    if info.signo == SIGKILL {
        proc.group_exit_code.store(SIGKILL as i32, Ordering::SeqCst);
    }
    proc.pending.lock().add(info);
    notify_signalfds(proc);
    // wake a thread that does not block it (all of them for SIGKILL)
    for t in &threads {
        let blocked = thread_of(t).map_or(0, |th| th.sigmask.load(Ordering::Relaxed) as u64);
        if info.signo == SIGKILL || blocked & bit(info.signo) == 0 {
            kick(t);
            if info.signo != SIGKILL {
                break;
            }
        }
    }
}

pub fn has_deliverable(th: &UserThread) -> bool {
    if th.proc.group_exit.load(Ordering::Relaxed) || th.exit_now.load(Ordering::Relaxed) {
        return true;
    }
    let mask = th.sigmask.load(Ordering::Relaxed) as u64 & !UNBLOCKABLE;
    (th.pending.lock().set | th.proc.pending.lock().set) & !mask != 0
}

fn dequeue(th: &UserThread) -> Option<SigInfo> {
    let mask = th.sigmask.load(Ordering::Relaxed) as u64 & !UNBLOCKABLE;
    if let Some(i) = th.pending.lock().take(mask) {
        return Some(i);
    }
    th.proc.pending.lock().take(mask)
}

/// Wait status of a process killed by `sig`.
pub fn killed_status(sig: u32, core: bool) -> i32 {
    (sig as i32 & 0x7f) | if core { 0x80 } else { 0 }
}

fn core_dumps(sig: u32) -> bool {
    matches!(sig, SIGQUIT | SIGILL | SIGTRAP | SIGABRT | SIGBUS | SIGFPE | SIGSEGV | SIGXCPU | SIGXFSZ | SIGSYS)
}

/// Delivers pending signals before returning to user mode. `f` is the trap frame.
pub fn do_signals(f: &mut TrapFrame) {
    let th = current_thread();
    loop {
        if th.proc.group_exit.load(Ordering::Relaxed) {
            super::exit(0, true);
        }
        if th.exit_now.load(Ordering::Relaxed) {
            super::exit(0, false);
        }
        let Some(info) = dequeue(th) else {
            // no handler ran: an interrupted system call restarts
            restart_syscall_if_needed(f, None);
            if let Some(m) = own().saved_mask.take() {
                th.sigmask.store(m as usize, Ordering::Relaxed);
            }
            return;
        };
        let sig = info.signo;
        let act = th.sighand.lock().act[sig as usize];
        if sig == SIGKILL || act.handler == SIG_DFL {
            if default_ignored(sig) {
                continue;
            }
            if stop_signal(sig) {
                do_stop(th, sig);
                continue;
            }
            let status = killed_status(sig, core_dumps(sig));
            th.proc.group_exit_code.store(status, Ordering::SeqCst);
            super::exit(status, true);
        }
        if act.handler == SIG_IGN {
            continue;
        }
        restart_syscall_if_needed(f, Some(act.flags));
        let o = own();
        let oldmask = o.saved_mask.take().unwrap_or(th.sigmask.load(Ordering::Relaxed) as u64);
        // alternate stack: when asked for and not already on it
        let sp = arch::frame_sp(f);
        let (ss_sp, ss_size, ss_flags) = o.altstack;
        let on_alt = ss_flags & SS_DISABLE == 0 && sp > ss_sp && sp <= ss_sp + ss_size;
        let use_alt = act.flags & SA_ONSTACK != 0 && ss_flags & SS_DISABLE == 0 && !on_alt;
        let stack_top = if use_alt { ss_sp + ss_size } else { sp };
        let tramp = if act.flags & SA_RESTORER != 0 && act.restorer != 0 {
            act.restorer
        } else {
            crate::vdso::sigreturn_addr(&th.mm)
        };
        arch::save_fp(&mut o.fp);
        let ok = arch::setup_signal_frame(f, &info, &act, oldmask, (ss_sp, ss_flags | if on_alt { SS_ONSTACK } else { 0 }, ss_size), stack_top, &o.fp, tramp);
        if ok.is_err() {
            // cannot write the frame: kill with SIGSEGV
            let status = killed_status(SIGSEGV, true);
            th.proc.group_exit_code.store(status, Ordering::SeqCst);
            super::exit(status, true);
        }
        if use_alt && ss_flags & SS_AUTODISARM != 0 {
            o.altstack = (0, 0, SS_DISABLE);
        }
        let mut newmask = oldmask | act.mask;
        if act.flags & SA_NODEFER == 0 {
            newmask |= bit(sig);
        }
        th.sigmask.store((newmask & !UNBLOCKABLE) as usize, Ordering::Relaxed);
        if act.flags & SA_RESETHAND != 0 {
            th.sighand.lock().act[sig as usize] = SigAction::default();
        }
        return;
    }
}

fn do_stop(th: &UserThread, sig: u32) {
    let proc = &th.proc;
    if !proc.stopped.swap(true, Ordering::SeqCst) {
        proc.state.lock().stop_report = Some(((sig as i32) << 8) | 0x7f);
        if let Some(p) = proc.parent() {
            let flags = p.leader().and_then(|l| thread_of(&l).map(|t| t.sighand.lock().act[SIGCHLD as usize].flags)).unwrap_or(0);
            if flags & SA_NOCLDSTOP == 0 {
                let mut i = SigInfo::kernel(SIGCHLD);
                i.code = 5; // CLD_STOPPED
                i.pid = proc.pid as i32;
                i.status = sig as i32;
                send_process(&p, i);
            }
            p.child_wq.wake_all();
        }
    }
    proc.stop_wq.wait_until(|| !proc.stopped.load(Ordering::SeqCst) || proc.group_exit.load(Ordering::SeqCst), None, false);
}

/// After a system call returned one of the restart codes: restart it, or turn it into EINTR.
fn restart_syscall_if_needed(f: &mut TrapFrame, handler_flags: Option<u64>) {
    let o = own();
    if !o.in_syscall {
        return;
    }
    o.in_syscall = false;
    let ret = arch::syscall_ret(f) as isize;
    let restart = match (ret, handler_flags) {
        (r, None) if r == ERESTARTSYS.neg() || r == ERESTARTNOINTR.neg() || r == ERESTARTNOHAND.neg() || r == ERESTART_RESTARTBLOCK.neg() => true,
        (r, Some(_)) if r == ERESTARTNOINTR.neg() => true,
        (r, Some(fl)) if r == ERESTARTSYS.neg() => fl & SA_RESTART != 0,
        (r, Some(_)) if r == ERESTARTNOHAND.neg() || r == ERESTART_RESTARTBLOCK.neg() => false,
        _ => return,
    };
    if restart {
        arch::restart_syscall(f, o.orig_arg0);
    } else {
        arch::set_syscall_ret(f, EINTR.neg() as usize);
    }
}

/// rt_sigreturn: restores registers, FP state, mask and alternate stack from the frame at sp.
pub fn sigreturn(f: &mut TrapFrame) -> Result<usize> {
    let th = current_thread();
    let o = own();
    let (mask, stack) = arch::restore_signal_frame(f, &mut o.fp)?;
    arch::restore_fp(&o.fp);
    th.sigmask.store((mask & !UNBLOCKABLE) as usize, Ordering::Relaxed);
    // uc_stack: restore the alternate stack setting
    let (ss_sp, ss_flags, ss_size) = stack;
    if ss_flags & SS_DISABLE != 0 {
        o.altstack = (0, 0, SS_DISABLE);
    } else if ss_size != 0 {
        o.altstack = (ss_sp, ss_size, ss_flags & !SS_ONSTACK);
    }
    o.in_syscall = false;
    Ok(arch::syscall_ret(f))
}
