//! virtio-rng (virtio 1.2 section 5.4): entropy for the kernel's random generator.
use super::virtio::{Buf, Mmio, VirtQueue};
use crate::errno::*;
use alloc::sync::Arc;

pub fn init(mmio: Arc<Mmio>) -> Result<()> {
    mmio.negotiate(0)?;
    let q = VirtQueue::new(&mmio, 0, false)?;
    mmio.driver_ok();
    // without an interrupt handler: poll for the first 64 bytes at boot
    let mut b = alloc::vec![0u8; 64];
    let head = q.submit(&[Buf::inp(&mut b)])?;
    for _ in 0..1_000_000 {
        if q.collect() > 0 || q.is_done(head) {
            break;
        }
        core::hint::spin_loop();
    }
    if q.is_done(head) {
        crate::random::mix(&b);
        q.reap(head);
    }
    Ok(())
}
