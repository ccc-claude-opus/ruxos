//! ITIMER_REAL / alarm: SIGALRM after a delay, optionally repeating (a kernel thread per armed
//! timer would be wasteful; one sleeping helper task per process timer is enough here).
use crate::errno::*;
use crate::sched;
use crate::task::signal::{self, SigInfo};
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU64, Ordering};

pub struct RealTimer {
    pub generation: AtomicU64,
}

struct Arg {
    proc: Weak<super::Process>,
    epoch: u64,
    first_us: i64,
    interval_us: i64,
}

extern "C" fn timer_task(arg: usize) {
    let a = unsafe { alloc::boxed::Box::from_raw(arg as *mut Arg) };
    let mut wait = a.first_us;
    loop {
        let d = crate::arch::ticks() + crate::time::ns_to_ticks(wait as u64 * 1000);
        sched::WaitQueue::new().wait_until(|| false, Some(d), false);
        let Some(p) = a.proc.upgrade() else { return };
        if p.itimer_gen.load(Ordering::SeqCst) != a.epoch || p.is_zombie() {
            return;
        }
        signal::send_process(&p, SigInfo::kernel(signal::SIGALRM));
        if a.interval_us <= 0 {
            return;
        }
        wait = a.interval_us;
    }
}

pub fn set_real(value_us: i64, interval_us: i64) -> Result<()> {
    let p: Arc<super::Process> = super::current_proc().clone();
    let epoch = p.itimer_gen.fetch_add(1, Ordering::SeqCst) + 1;
    if value_us <= 0 {
        return Ok(());
    }
    let arg = alloc::boxed::Box::new(Arg { proc: Arc::downgrade(&p), epoch, first_us: value_us, interval_us });
    sched::spawn_kernel(timer_task, alloc::boxed::Box::into_raw(arg) as usize);
    Ok(())
}
