//! virtio over MMIO (virtio 1.2, sections 2 and 4.2): device discovery from the device tree, the
//! initialization handshake, and split virtqueues.
use crate::arch::{phys_to_virt, virt_to_phys};
use crate::errno::*;
use crate::fdt::Fdt;
use crate::mm::frame;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, AtomicBool, AtomicU32, Ordering};

const MAGIC: u32 = 0x7472_6976;
pub const F_VERSION_1: u64 = 1 << 32;
pub const F_EVENT_IDX: u64 = 1 << 29;

const STATUS_ACK: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;

pub struct Mmio {
    pub base: usize,
    pub irq: u32,
}

impl Mmio {
    pub fn r(&self, off: usize) -> u32 {
        unsafe { read_volatile((self.base + off) as *const u32) }
    }
    pub fn w(&self, off: usize, v: u32) {
        unsafe { write_volatile((self.base + off) as *mut u32, v) }
    }
    pub fn config_u8(&self, off: usize) -> u8 {
        unsafe { read_volatile((self.base + 0x100 + off) as *const u8) }
    }
    pub fn config_u32(&self, off: usize) -> u32 {
        unsafe { read_volatile((self.base + 0x100 + off) as *const u32) }
    }
    pub fn config_u64(&self, off: usize) -> u64 {
        // read twice around the generation counter for consistency
        loop {
            let g = self.r(0xfc);
            let lo = self.config_u32(off) as u64;
            let hi = self.config_u32(off + 4) as u64;
            if self.r(0xfc) == g {
                return lo | (hi << 32);
            }
        }
    }

    /// The handshake up to FEATURES_OK: returns the negotiated features.
    pub fn negotiate(&self, wanted: u64) -> Result<u64> {
        self.w(0x70, 0); // reset
        self.w(0x70, STATUS_ACK);
        self.w(0x70, STATUS_ACK | STATUS_DRIVER);
        self.w(0x14, 0);
        let lo = self.r(0x10) as u64;
        self.w(0x14, 1);
        let hi = self.r(0x10) as u64;
        let offered = lo | (hi << 32);
        let f = offered & (wanted | F_VERSION_1);
        if f & F_VERSION_1 == 0 {
            return Err(ENODEV);
        }
        self.w(0x24, 0);
        self.w(0x20, f as u32);
        self.w(0x24, 1);
        self.w(0x20, (f >> 32) as u32);
        self.w(0x70, STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK);
        if self.r(0x70) & STATUS_FEATURES_OK == 0 {
            return Err(ENODEV);
        }
        Ok(f)
    }

    pub fn driver_ok(&self) {
        self.w(0x70, STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
    }

    /// Acknowledges the interrupt; returns the status bits (1 = used buffers, 2 = config change).
    pub fn ack_interrupt(&self) -> u32 {
        let s = self.r(0x60);
        if s != 0 {
            self.w(0x64, s);
        }
        s
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;

/// One buffer of a request: (physical address, length, device writes it).
#[derive(Clone, Copy)]
pub struct Buf {
    pub pa: usize,
    pub len: usize,
    pub write: bool,
}

impl Buf {
    pub fn out(b: &[u8]) -> Buf {
        Buf { pa: virt_to_phys(b.as_ptr() as usize), len: b.len(), write: false }
    }
    pub fn inp(b: &mut [u8]) -> Buf {
        Buf { pa: virt_to_phys(b.as_ptr() as usize), len: b.len(), write: true }
    }
}

struct QInner {
    free: Vec<u16>,
    last_used: u16,
    /// per head descriptor: (done, used length, chain length)
    done: Vec<(bool, u32, u16)>,
}

pub struct VirtQueue {
    pub index: u32,
    size: u16,
    desc: usize,
    avail: usize,
    used: usize,
    inner: SpinLock<QInner>,
    wq: WaitQueue,
    event_idx: AtomicBool,
    pub mmio: Arc<Mmio>,
    pub notifies: AtomicU32,
}

impl VirtQueue {
    pub fn new(mmio: &Arc<Mmio>, index: u32, event_idx: bool) -> Result<Arc<VirtQueue>> {
        mmio.w(0x30, index);
        let max = mmio.r(0x34);
        if max == 0 {
            return Err(ENODEV);
        }
        let size = max.min(256) as u16;
        mmio.w(0x38, size as u32);
        // one page each is enough for 256 entries
        let d = frame::zeroed_page();
        let a = frame::zeroed_page();
        let u = frame::zeroed_page();
        mmio.w(0x80, d as u32);
        mmio.w(0x84, (d >> 32) as u32);
        mmio.w(0x90, a as u32);
        mmio.w(0x94, (a >> 32) as u32);
        mmio.w(0xa0, u as u32);
        mmio.w(0xa4, (u >> 32) as u32);
        mmio.w(0x44, 1);
        Ok(Arc::new(VirtQueue {
            index,
            size,
            desc: phys_to_virt(d),
            avail: phys_to_virt(a),
            used: phys_to_virt(u),
            inner: SpinLock::new(QInner { free: (0..size).rev().collect(), last_used: 0, done: alloc::vec![(false, 0, 0); size as usize] }),
            wq: WaitQueue::new(),
            event_idx: AtomicBool::new(event_idx),
            mmio: mmio.clone(),
            notifies: AtomicU32::new(0),
        }))
    }

    fn desc_at(&self, i: u16) -> *mut Desc {
        (self.desc + 16 * i as usize) as *mut Desc
    }
    fn avail_idx(&self) -> *mut u16 {
        (self.avail + 2) as *mut u16
    }
    fn avail_ring(&self, i: u16) -> *mut u16 {
        (self.avail + 4 + 2 * (i % self.size) as usize) as *mut u16
    }
    fn used_idx(&self) -> *const u16 {
        (self.used + 2) as *const u16
    }
    fn used_elem(&self, i: u16) -> (u32, u32) {
        let p = self.used + 4 + 8 * (i % self.size) as usize;
        unsafe { (read_volatile(p as *const u32), read_volatile((p + 4) as *const u32)) }
    }
    /// avail_event (the device asks to be notified when avail idx passes it)
    fn avail_event(&self) -> u16 {
        unsafe { read_volatile((self.used + 4 + 8 * self.size as usize) as *const u16) }
    }
    fn set_used_event(&self, v: u16) {
        unsafe { write_volatile((self.avail + 4 + 2 * self.size as usize) as *mut u16, v) };
    }

    /// Queues a chain of buffers; returns the head descriptor (the request's token).
    pub fn submit(&self, bufs: &[Buf]) -> Result<u16> {
        let n = bufs.len() as u16;
        loop {
            let mut q = self.inner.lock();
            if q.free.len() >= n as usize {
                let ids: Vec<u16> = (0..n).map(|_| q.free.pop().unwrap()).collect();
                for (k, b) in bufs.iter().enumerate() {
                    let mut flags = if b.write { DESC_WRITE } else { 0 };
                    let next = if k + 1 < ids.len() {
                        flags |= DESC_NEXT;
                        ids[k + 1]
                    } else {
                        0
                    };
                    unsafe { write_volatile(self.desc_at(ids[k]), Desc { addr: b.pa as u64, len: b.len as u32, flags, next }) };
                }
                let head = ids[0];
                q.done[head as usize] = (false, 0, n);
                let idx = unsafe { read_volatile(self.avail_idx()) };
                unsafe { write_volatile(self.avail_ring(idx), head) };
                fence(Ordering::SeqCst);
                unsafe { write_volatile(self.avail_idx(), idx.wrapping_add(1)) };
                fence(Ordering::SeqCst);
                let notify = if self.event_idx.load(Ordering::Relaxed) {
                    // vring_need_event(avail_event, new, old)
                    let ev = self.avail_event();
                    idx.wrapping_add(1).wrapping_sub(ev).wrapping_sub(1) < 1
                } else {
                    true
                };
                drop(q);
                if notify {
                    self.notifies.fetch_add(1, Ordering::Relaxed);
                    self.mmio.w(0x50, self.index);
                }
                return Ok(head);
            }
            drop(q);
            self.wq.wait(|| self.inner.lock().free.len() >= n as usize);
        }
    }

    /// Collects completed requests (from the interrupt handler or a poller); wakes waiters.
    pub fn collect(&self) -> usize {
        let mut q = self.inner.lock();
        let mut n = 0;
        loop {
            fence(Ordering::SeqCst);
            let used = unsafe { read_volatile(self.used_idx()) };
            if q.last_used == used {
                break;
            }
            let (id, len) = self.used_elem(q.last_used);
            q.last_used = q.last_used.wrapping_add(1);
            let id = id as u16;
            // free the chain
            let mut d = id;
            loop {
                let desc = unsafe { read_volatile(self.desc_at(d)) };
                q.free.push(d);
                if desc.flags & DESC_NEXT == 0 {
                    break;
                }
                d = desc.next;
            }
            let e = &mut q.done[id as usize];
            e.0 = true;
            e.1 = len;
            n += 1;
        }
        if self.event_idx.load(Ordering::Relaxed) {
            let lu = q.last_used;
            self.set_used_event(lu);
        }
        drop(q);
        if n > 0 {
            self.wq.wake_all();
        }
        n
    }

    pub fn is_done(&self, head: u16) -> bool {
        self.inner.lock().done[head as usize].0
    }

    /// Waits for request `head`; returns the length the device wrote.
    pub fn wait(&self, head: u16) -> u32 {
        self.collect();
        self.wq.wait(|| {
            let q = self.inner.lock();
            q.done[head as usize].0
        });
        let mut q = self.inner.lock();
        let len = q.done[head as usize].1;
        q.done[head as usize] = (false, 0, 0);
        len
    }

    /// Takes the next completed buffer without a waiter (receive queues): (head, length).
    pub fn pop_done(&self, head_order: &mut alloc::collections::VecDeque<u16>) -> Option<(u16, u32)> {
        let mut q = self.inner.lock();
        let &h = head_order.front()?;
        if !q.done[h as usize].0 {
            return None;
        }
        head_order.pop_front();
        let len = q.done[h as usize].1;
        q.done[h as usize] = (false, 0, 0);
        Some((h, len))
    }
}

pub struct Found {
    pub mmio: Arc<Mmio>,
    pub device: u32,
}

/// Every virtio-mmio device, in device tree order (the order Linux probes them, which decides
/// the /dev/vdX names).
pub fn probe(fdt: &Fdt) -> Vec<Found> {
    let mut out = Vec::new();
    fdt.for_each_node(|n| {
        if !n.compatible("virtio,mmio") {
            return;
        }
        let Some((base, _)) = n.reg(0) else { return };
        let Some(irq) = crate::arch::dt_irq(n, 0) else { return };
        let mmio = Mmio { base: phys_to_virt(base as usize), irq };
        if mmio.r(0) != MAGIC || mmio.r(4) != 2 {
            return;
        }
        let device = mmio.r(8);
        if device != 0 {
            out.push(Found { mmio: Arc::new(mmio), device });
        }
    });
    out
}

pub fn init(fdt: &Fdt) {
    let devs = probe(fdt);
    let mut blk = 0;
    for f in &devs {
        match f.device {
            2 => {
                if super::virtio_blk::init(f.mmio.clone(), blk).is_ok() {
                    blk += 1;
                }
            }
            4 => {
                let _ = super::virtio_rng::init(f.mmio.clone());
            }
            _ => {}
        }
    }
}
