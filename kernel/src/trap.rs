//! Traps from user mode: system calls, page faults, and exceptions that become signals; the checks
//! on the way back to user mode (rescheduling, signal delivery, system call restart).
use crate::arch::{self, TrapFrame, UserTrap};
use crate::errno::*;
use crate::mm::vm::current_mm;
use crate::task::signal::{self, SigInfo};
use crate::task::{own, try_current_thread};

pub fn user_exception(f: &mut TrapFrame, cause: usize, addr: usize) {
    let o = own();
    o.frame = f;
    o.in_syscall = false;
    match arch::decode_user_trap(cause, addr, f) {
        UserTrap::Syscall => {
            #[cfg(target_arch = "riscv64")]
            {
                f.pc += 4;
            }
            let nr = arch::syscall_nr(f);
            let me = crate::sched::current();
            me.last_nr.store(nr, core::sync::atomic::Ordering::Relaxed);
            me.nr_count.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let args = arch::syscall_args(f);
            o.orig_arg0 = args[0];
            o.cur_syscall = nr as isize;
            let r = crate::syscall::dispatch(nr, args, f);
            own().cur_syscall = -1;
            if nr != crate::syscall::nr::RT_SIGRETURN {
                arch::set_syscall_ret(f, r as usize);
                own().in_syscall = true;
            }
        }
        UserTrap::PageFault(va, access) => match {
            let me = crate::sched::current();
            me.faults.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            me.last_fault.store(va, core::sync::atomic::Ordering::Relaxed);
            current_mm().fault(va, access)
        } {
            Ok(()) => {}
            Err(e) => {
                if crate::syscall::STRACE.load(core::sync::atomic::Ordering::Relaxed) {
                    println!("[{}] fault at {:#x} ({:?}) pc {:#x}: {:?}", crate::sched::current().tid, va, access, arch::frame_pc(f), e);
                }
                let info = match e {
                    EFAULT => SigInfo::fault(signal::SIGSEGV, 1, va), // SEGV_MAPERR
                    EACCES => SigInfo::fault(signal::SIGSEGV, 2, va), // SEGV_ACCERR
                    ENOMEM => SigInfo::kernel(signal::SIGKILL),
                    _ => SigInfo::fault(signal::SIGBUS, 2, va), // BUS_ADRERR
                };
                force_signal(info);
            }
        },
        UserTrap::Signal(sig, code, a) => force_signal(SigInfo::fault(sig, code, a)),
    }
    return_to_user(f);
}

/// A synchronous signal (fault): if it is blocked or ignored, the default action (death) applies.
fn force_signal(info: SigInfo) {
    let th = crate::task::current_thread();
    let b = signal::bit(info.signo);
    let mut h = th.sighand.lock();
    let a = &mut h.act[info.signo as usize];
    let blocked = th.sigmask.load(core::sync::atomic::Ordering::Relaxed) as u64 & b != 0;
    if blocked || a.handler == signal::SIG_IGN {
        a.handler = signal::SIG_DFL;
        drop(h);
        th.sigmask.fetch_and(!(b as usize), core::sync::atomic::Ordering::Relaxed);
    } else {
        drop(h);
    }
    let t = crate::sched::current_arc();
    signal::send_thread(&t, info);
}

fn is_restart(r: isize) -> bool {
    r == ERESTARTSYS.neg() || r == ERESTARTNOINTR.neg() || r == ERESTARTNOHAND.neg() || r == ERESTART_RESTARTBLOCK.neg()
}

/// Everything that must happen before user mode resumes with frame `f`.
pub fn return_to_user(f: &mut TrapFrame) {
    loop {
        crate::sched::resched_if_needed();
        let Some(th) = try_current_thread() else { return };
        let o = own();
        let restart = o.in_syscall && is_restart(arch::syscall_ret(f) as isize);
        if signal::has_deliverable(th) || restart || o.saved_mask.is_some() {
            signal::do_signals(f);
        }
        if !crate::percpu::this().need_resched.load(core::sync::atomic::Ordering::Relaxed) {
            return;
        }
    }
}
