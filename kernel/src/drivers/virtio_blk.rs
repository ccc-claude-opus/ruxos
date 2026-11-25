//! virtio-blk (virtio 1.2 section 5.2): /dev/vda, /dev/vdb... with flush.
use super::virtio::{Buf, Mmio, VirtQueue, F_EVENT_IDX};
use crate::block::BlockDevice;
use crate::errno::*;
use crate::sync::SpinLock;
use crate::vfs::makedev;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;

const F_RO: u64 = 1 << 5;
const F_BLK_SIZE: u64 = 1 << 6;
const F_FLUSH: u64 = 1 << 9;
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;

pub struct VirtioBlk {
    q: Arc<VirtQueue>,
    sectors: u64,
    ro: bool,
    flush: bool,
}

static DEVS: SpinLock<BTreeMap<u32, Arc<VirtioBlk>>> = SpinLock::new(BTreeMap::new());

fn irq(n: u32) {
    let d = DEVS.lock().get(&n).cloned();
    if let Some(d) = d {
        d.q.mmio.ack_interrupt();
        d.q.collect();
    }
}

pub fn init(mmio: Arc<Mmio>, index: usize) -> Result<()> {
    let f = mmio.negotiate(F_RO | F_BLK_SIZE | F_FLUSH | F_EVENT_IDX)?;
    let q = VirtQueue::new(&mmio, 0, f & F_EVENT_IDX != 0)?;
    mmio.driver_ok();
    let sectors = mmio.config_u64(0);
    let dev = Arc::new(VirtioBlk { q, sectors, ro: f & F_RO != 0, flush: f & F_FLUSH != 0 });
    DEVS.lock().insert(mmio.irq, dev.clone());
    crate::irq::register(mmio.irq, irq);
    let name = alloc::format!("vd{}", (b'a' + index as u8) as char);
    crate::vfs::dev::register_blk(makedev(254, 16 * index as u32), &name, dev);
    if crate::kernel_param("quiet").is_none() {
        println!("ruxos: {}: {} MB", name, sectors / 2048);
    }
    Ok(())
}

impl VirtioBlk {
    fn request(&self, kind: u32, sector: u64, data: &[Buf]) -> Result<()> {
        let mut hdr = [0u8; 16];
        hdr[0..4].copy_from_slice(&kind.to_le_bytes());
        hdr[8..16].copy_from_slice(&sector.to_le_bytes());
        let mut status = [0xffu8; 1];
        let mut bufs = alloc::vec::Vec::with_capacity(data.len() + 2);
        bufs.push(Buf::out(&hdr));
        bufs.extend_from_slice(data);
        bufs.push(Buf::inp(&mut status));
        let head = self.q.submit(&bufs)?;
        self.q.wait(head);
        if unsafe { core::ptr::read_volatile(&status[0]) } == 0 { Ok(()) } else { Err(EIO) }
    }

    /// Reads into physical pages (page cache): one request, several buffers.
    pub fn read_pages(&self, sector: u64, pages: &[(usize, usize)]) -> Result<()> {
        let bufs: alloc::vec::Vec<Buf> = pages.iter().map(|&(pa, len)| Buf { pa, len, write: true }).collect();
        self.request(T_IN, sector, &bufs)
    }
}

impl BlockDevice for VirtioBlk {
    fn sectors(&self) -> u64 {
        self.sectors
    }
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<()> {
        if sector + (buf.len() as u64).div_ceil(512) > self.sectors {
            return Err(EIO);
        }
        self.request(T_IN, sector, &[Buf::inp(buf)])
    }
    fn write(&self, sector: u64, buf: &[u8]) -> Result<()> {
        if self.ro {
            return Err(EROFS);
        }
        if sector + (buf.len() as u64).div_ceil(512) > self.sectors {
            return Err(EIO);
        }
        self.request(T_OUT, sector, &[Buf::out(buf)])
    }
    fn flush(&self) -> Result<()> {
        if self.flush { self.request(T_FLUSH, 0, &[]) } else { Ok(()) }
    }
    fn readonly(&self) -> bool {
        self.ro
    }
    fn read_phys(&self, sector: u64, pages: &[(usize, usize)]) -> Result<()> {
        self.read_pages(sector, pages)
    }
    fn write_phys(&self, sector: u64, pages: &[(usize, usize)]) -> Result<()> {
        if self.ro {
            return Err(EROFS);
        }
        let bufs: alloc::vec::Vec<Buf> = pages.iter().map(|&(pa, len)| Buf { pa, len, write: false }).collect();
        self.request(T_OUT, sector, &bufs)
    }
}
