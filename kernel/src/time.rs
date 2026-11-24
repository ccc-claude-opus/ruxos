//! Time: the architecture's counter (ticks) as the monotonic clock, per-CPU sleep timers, the time
//! slice, and the realtime clock (set from the RTC at boot, adjustable with clock_settime).
use crate::arch;
use crate::percpu;
use crate::sched::Task;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};

static FREQ: AtomicU64 = AtomicU64::new(1);
/// Realtime = monotonic + this (nanoseconds).
pub static REALTIME_OFFSET: AtomicI64 = AtomicI64::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
pub const SLICE_NS: u64 = 4_000_000;

pub fn init() {
    let f = arch::tick_freq();
    FREQ.store(f, Ordering::Relaxed);
    MULT.store(((1_000_000_000u128 << 32) / f as u128) as u64, Ordering::Relaxed);
}

/// Publishes the clock parameters to the vDSO.
pub fn publish() {
    crate::vdso::update(MULT.load(Ordering::Relaxed), REALTIME_OFFSET.load(Ordering::Relaxed));
}

pub fn freq() -> u64 {
    FREQ.load(Ordering::Relaxed)
}

/// ns = (ticks * MULT) >> 32, the same arithmetic as the vDSO (so the two never disagree).
static MULT: AtomicU64 = AtomicU64::new(1 << 32);

pub fn ticks_to_ns(t: u64) -> u64 {
    ((t as u128 * MULT.load(Ordering::Relaxed) as u128) >> 32) as u64
}

pub fn ns_to_ticks(ns: u64) -> u64 {
    ((ns as u128 * freq() as u128) / 1_000_000_000) as u64
}

/// Nanoseconds since boot (CLOCK_MONOTONIC).
pub fn monotonic_ns() -> u64 {
    ticks_to_ns(arch::ticks())
}

pub fn realtime_ns() -> i64 {
    monotonic_ns() as i64 + REALTIME_OFFSET.load(Ordering::Relaxed)
}

pub fn set_realtime_ns(ns: i64) {
    REALTIME_OFFSET.store(ns - monotonic_ns() as i64, Ordering::Relaxed);
    publish();
}

pub type TimerId = (usize, (u64, u64));

/// Wakes `t` at `deadline` (ticks).
pub fn add_timer(deadline: u64, t: Arc<Task>) -> TimerId {
    let pc = percpu::this();
    let key = (deadline, NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let first = {
        let mut tm = pc.timers.lock();
        tm.insert(key, t);
        tm.keys().next() == Some(&key)
    };
    if first {
        reprogram();
    }
    (pc.id, key)
}

pub fn cancel_timer(id: TimerId) {
    percpu::cpu(id.0).timers.lock().remove(&id.1);
}

/// Programs this CPU's timer for the earliest of its sleep timers and the end of the time slice.
fn reprogram() {
    let irq = arch::irq_save();
    let pc = percpu::this();
    let mut next = unsafe { *pc.slice_end.get() };
    if let Some(&(d, _)) = pc.timers.lock().keys().next() {
        if next == 0 || d < next {
            next = d;
        }
    }
    if next == 0 { arch::stop_timer() } else { arch::set_timer(next) }
    arch::irq_restore(irq);
}

/// A new task got the CPU: its time slice starts (the idle task has none).
pub fn start_slice() {
    let pc = percpu::this();
    let idle = crate::sched::current().is_idle;
    unsafe { *pc.slice_end.get() = if idle { 0 } else { arch::ticks() + ns_to_ticks(SLICE_NS) } };
    reprogram();
}

pub fn idle_timer() {
    unsafe { *percpu::this().slice_end.get() = 0 };
    reprogram();
}

pub fn timer_interrupt() {
    let pc = percpu::this();
    let now = arch::ticks();
    loop {
        let t = {
            let mut tm = pc.timers.lock();
            match tm.first_key_value() {
                Some((&(d, _), _)) if d <= now => tm.pop_first().map(|(_, t)| t),
                _ => None,
            }
        };
        match t {
            Some(t) => {
                crate::sched::wake(&t);
            }
            None => break,
        }
    }
    let slice_end = unsafe { &mut *pc.slice_end.get() };
    if *slice_end != 0 && now >= *slice_end {
        if !pc.rq.lock().is_empty() {
            pc.need_resched.store(true, Ordering::Relaxed);
        }
        *slice_end = now + ns_to_ticks(SLICE_NS);
    }
    reprogram();
}

/// Reads the RTC the device tree describes (PL031 or goldfish) and sets the realtime clock.
pub fn rtc_init(fdt: &crate::fdt::Fdt) {
    let read = |n: &crate::fdt::Node| n.reg(0).map(|(b, _)| arch::phys_to_virt(b as usize));
    let ns = if let Some(base) = fdt.find_compatible("arm,pl031").as_ref().and_then(read) {
        unsafe { core::ptr::read_volatile(base as *const u32) as i64 * 1_000_000_000 }
    } else if let Some(base) = fdt.find_compatible("google,goldfish-rtc").as_ref().and_then(read) {
        unsafe {
            let lo = core::ptr::read_volatile(base as *const u32) as u64;
            let hi = core::ptr::read_volatile((base + 4) as *const u32) as u64;
            ((hi << 32) | lo) as i64
        }
    } else {
        0
    };
    set_realtime_ns(ns);
}
