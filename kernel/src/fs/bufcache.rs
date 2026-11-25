//! The buffer cache: file system metadata blocks of a block device, read once and kept (written
//! back by the file system).
use crate::block::BlockDevice;
use crate::errno::*;
use crate::sync::SpinLock;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

pub struct Buf {
    pub block: u64,
    pub data: SpinLock<Vec<u8>>,
    pub dirty: AtomicBool,
}

impl Buf {
    pub fn read<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        f(&self.data.lock())
    }
    pub fn write<R>(&self, f: impl FnOnce(&mut [u8]) -> R) -> R {
        let r = f(&mut self.data.lock());
        self.dirty.store(true, Ordering::Release);
        r
    }
}

pub struct BufCache {
    pub dev: Arc<dyn BlockDevice>,
    pub bs: usize,
    map: SpinLock<BTreeMap<u64, Arc<Buf>>>,
}

impl BufCache {
    pub fn new(dev: Arc<dyn BlockDevice>, bs: usize) -> BufCache {
        BufCache { dev, bs, map: SpinLock::new(BTreeMap::new()) }
    }

    pub fn get(&self, block: u64) -> Result<Arc<Buf>> {
        if let Some(b) = self.map.lock().get(&block) {
            return Ok(b.clone());
        }
        let mut v = alloc::vec![0u8; self.bs];
        self.dev.read(block * (self.bs as u64 / 512), &mut v)?;
        let b = Arc::new(Buf { block, data: SpinLock::new(v), dirty: AtomicBool::new(false) });
        let mut m = self.map.lock();
        Ok(m.entry(block).or_insert(b).clone())
    }

    /// A block that will be overwritten entirely: no need to read it.
    pub fn get_new(&self, block: u64) -> Arc<Buf> {
        let mut m = self.map.lock();
        if let Some(b) = m.get(&block) {
            b.data.lock().fill(0);
            return b.clone();
        }
        let b = Arc::new(Buf { block, data: SpinLock::new(alloc::vec![0u8; self.bs]), dirty: AtomicBool::new(false) });
        m.insert(block, b.clone());
        b
    }

    pub fn forget_all(&self) {
        self.map.lock().clear();
    }

    pub fn forget(&self, block: u64) {
        self.map.lock().remove(&block);
    }

    pub fn dirty_bufs(&self) -> Vec<Arc<Buf>> {
        self.map.lock().values().filter(|b| b.dirty.load(Ordering::Acquire)).cloned().collect()
    }

    pub fn write_back(&self, b: &Buf) -> Result<()> {
        if b.dirty.swap(false, Ordering::AcqRel) {
            let data = b.data.lock().clone();
            if let Err(e) = self.dev.write(b.block * (self.bs as u64 / 512), &data) {
                b.dirty.store(true, Ordering::Release);
                return Err(e);
            }
        }
        Ok(())
    }
}
