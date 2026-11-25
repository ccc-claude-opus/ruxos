//! virtio-net (virtio 1.2 section 5.1): receive buffers posted ahead, frames sent one per chain,
//! each with the 12-byte virtio_net_hdr.
use super::virtio::{Buf, Mmio, VirtQueue};
use crate::errno::*;
use crate::sync::SpinLock;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;

const F_MAC: u64 = 1 << 5;
pub const HDR: usize = 12;
const RX_BUF: usize = 2048;
const RX_COUNT: usize = 128;

pub struct VirtioNet {
    pub mac: [u8; 6],
    rx: Arc<VirtQueue>,
    tx: Arc<VirtQueue>,
    /// posted receive buffers by head descriptor, in posting order
    rx_bufs: SpinLock<(BTreeMap<u16, Vec<u8>>, VecDeque<u16>)>,
    /// frames being sent, freed when the device is done with them
    tx_bufs: SpinLock<BTreeMap<u16, Vec<u8>>>,
    pub irq: u32,
}

pub static NIC: crate::sync::Once<Arc<VirtioNet>> = crate::sync::Once::new();

pub static IRQS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

fn irq(_n: u32) {
    IRQS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if let Some(n) = NIC.get() {
        n.rx.mmio.ack_interrupt();
        n.rx.collect();
        n.tx.collect();
        crate::net::inet::kick();
    }
}

pub fn init(mmio: Arc<Mmio>) -> Result<()> {
    if NIC.get().is_some() {
        return Ok(()); // one interface
    }
    let f = mmio.negotiate(F_MAC)?;
    let rx = VirtQueue::new(&mmio, 0, false)?;
    let tx = VirtQueue::new(&mmio, 1, false)?;
    mmio.driver_ok();
    let mut mac = [0u8; 6];
    if f & F_MAC != 0 {
        for (i, m) in mac.iter_mut().enumerate() {
            *m = mmio.config_u8(i);
        }
    }
    let nic = Arc::new(VirtioNet { mac, rx, tx, rx_bufs: SpinLock::new((BTreeMap::new(), VecDeque::new())), tx_bufs: SpinLock::new(BTreeMap::new()), irq: mmio.irq });
    for _ in 0..RX_COUNT {
        nic.post_rx()?;
    }
    NIC.set(nic);
    crate::irq::register(mmio.irq, irq);
    crate::fs::sysfs::NET_IFS.lock().push((alloc::string::String::from("eth0"), mac, 1500, "virtio_net", 2));
    Ok(())
}

impl VirtioNet {
    fn post_rx(&self) -> Result<()> {
        let mut b = alloc::vec![0u8; RX_BUF];
        let head = self.rx.submit(&[Buf::inp(&mut b)])?;
        let mut r = self.rx_bufs.lock();
        r.0.insert(head, b);
        r.1.push_back(head);
        Ok(())
    }

    /// The next received frame (without the header).
    pub fn recv(&self) -> Option<Vec<u8>> {
        self.rx.collect();
        let (head, len) = {
            let mut r = self.rx_bufs.lock();
            self.rx.pop_done(&mut r.1)?
        };
        let buf = self.rx_bufs.lock().0.remove(&head)?;
        let _ = self.post_rx();
        let len = len as usize;
        if len <= HDR {
            return Some(Vec::new());
        }
        Some(buf[HDR..len.min(buf.len())].to_vec())
    }

    pub fn debug(&self) {
        let (u, l, f, a) = self.rx.debug_state();
        let pending = self.rx_bufs.lock().1.len();
        let (tu, tl, tf, ta) = self.tx.debug_state();
        println!("net: rx used {} seen {} free {} avail {} posted {} irqs {} | tx used {} seen {} free {} avail {}", u, l, f, a, pending, IRQS.load(core::sync::atomic::Ordering::Relaxed), tu, tl, tf, ta);
    }

    pub fn send(&self, frame: &[u8]) -> Result<()> {
        // drop buffers of finished sends
        self.tx.collect();
        {
            let mut t = self.tx_bufs.lock();
            let done: Vec<u16> = t.keys().copied().filter(|h| self.tx.is_done(*h)).collect();
            for h in done {
                t.remove(&h);
                self.tx.reap(h);
            }
        }
        let mut b = alloc::vec![0u8; HDR + frame.len()];
        b[HDR..].copy_from_slice(frame);
        let head = self.tx.submit(&[Buf::out(&b)])?;
        self.tx_bufs.lock().insert(head, b);
        Ok(())
    }
}
