//! Device interrupt handlers, by interrupt number (GIC interrupt id / PLIC source).
use core::sync::atomic::{AtomicUsize, Ordering};

const MAX: usize = 1024;
static HANDLERS: [AtomicUsize; MAX] = [const { AtomicUsize::new(0) }; MAX];

pub fn register(irq: u32, handler: fn(u32)) {
    HANDLERS[irq as usize].store(handler as usize, Ordering::Release);
    crate::arch::irq_enable_line(irq);
}

pub fn dispatch(irq: u32) {
    let h = HANDLERS.get(irq as usize).map_or(0, |h| h.load(Ordering::Acquire));
    if h != 0 {
        let f: fn(u32) = unsafe { core::mem::transmute(h) };
        f(irq);
    }
}
