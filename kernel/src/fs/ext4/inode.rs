//! ext4 inodes: metadata, file data through the page cache (with readahead), directories,
//! symbolic links.
use super::dir;
use super::extent::{map_extent, map_indirect, Run};
use super::{le16, le32, Ext4, RO_COMPAT_HUGE_FILE};
use crate::errno::*;
use crate::mm::frame::{self, PAGE_SIZE};
use crate::sched::Mutex;
use crate::sync::SpinLock;
use crate::vfs::*;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

pub const EXT4_EXTENTS_FL: u32 = 0x80000;
pub const EXT4_HUGE_FILE_FL: u32 = 0x40000;
pub const EXT4_INDEX_FL: u32 = 0x1000;
pub const EXT4_INLINE_DATA_FL: u32 = 0x1000_0000;
const READAHEAD_PAGES: u64 = 32;

pub struct Ext4Inode {
    pub fs: Arc<Ext4>,
    pub ino: u32,
    pub raw: SpinLock<Vec<u8>>,
    pub cache: PageCache,
    /// Serializes changes to this inode's data and directory entries.
    pub lock: Mutex<()>,
    pub me: alloc::sync::Weak<Ext4Inode>,
    /// The raw inode changed since it was written to the inode table.
    pub dirty: core::sync::atomic::AtomicBool,
    /// Page-cache pages written since the last writeback.
    pub dirty_pages: SpinLock<alloc::collections::BTreeSet<u64>>,
}

fn ts(sec: u32, extra: u32) -> Timespec {
    // the two low bits of `extra` extend the seconds beyond 2038
    let s = sec as i32 as i64 + (((extra & 3) as i64) << 32);
    Timespec { sec: s, nsec: (extra >> 2) as i64 }
}

impl Ext4Inode {
    pub fn load(fs: Arc<Ext4>, ino: u32) -> Result<Arc<Ext4Inode>> {
        let (blk, off) = fs.inode_location(ino)?;
        let isz = fs.sb.lock().inode_size;
        let b = fs.bufs.get(blk)?;
        let raw = b.read(|d| d[off..off + isz].to_vec());
        Ok(Self::from_raw(fs, ino, raw))
    }

    pub fn from_raw(fs: Arc<Ext4>, ino: u32, raw: Vec<u8>) -> Arc<Ext4Inode> {
        Arc::new_cyclic(|me| Ext4Inode {
            fs,
            ino,
            raw: SpinLock::new(raw),
            cache: PageCache::new(),
            lock: Mutex::new(()),
            me: me.clone(),
            dirty: core::sync::atomic::AtomicBool::new(false),
            dirty_pages: SpinLock::new(alloc::collections::BTreeSet::new()),
        })
    }

    pub fn mode(&self) -> u32 {
        le16(&self.raw.lock(), 0) as u32
    }
    pub fn flags(&self) -> u32 {
        le32(&self.raw.lock(), 32)
    }
    pub fn size(&self) -> u64 {
        let r = self.raw.lock();
        le32(&r, 4) as u64 | ((le32(&r, 108) as u64) << 32)
    }
    pub fn iblock(&self) -> Vec<u8> {
        self.raw.lock()[40..100].to_vec()
    }
    fn is_dir(&self) -> bool {
        self.mode() & S_IFMT == S_IFDIR
    }

    /// Maps a logical block.
    pub fn map(&self, lblk: u32) -> Result<Option<Run>> {
        let root = self.iblock();
        if self.flags() & EXT4_EXTENTS_FL != 0 {
            map_extent(&self.fs, &root, lblk)
        } else {
            map_indirect(&self.fs, &root, lblk)
        }
    }

    /// Reads pages [first, first + n) of the file into new page-cache pages, merging contiguous
    /// blocks into one device request.
    fn fill_pages(&self, first: u64, n: u64) -> Result<()> {
        let bs = self.fs.bs as u64;
        let bpp = PAGE_SIZE as u64 / bs;
        let size = self.size();
        let mut pages: Vec<(u64, usize)> = Vec::new();
        for i in 0..n {
            if self.cache.lookup(first + i).is_some() {
                break;
            }
            let pa = frame::alloc_pages(0).ok_or(ENOMEM)?;
            page_slice(pa).fill(0);
            pages.push((first + i, pa));
        }
        // runs of (start sector, [(pa, len)])
        let mut req_start = 0u64;
        let mut req: Vec<(usize, usize)> = Vec::new();
        let mut next_pblk = u64::MAX;
        let mut result = Ok(());
        let flush = |start: u64, req: &mut Vec<(usize, usize)>| -> Result<()> {
            if !req.is_empty() {
                self.fs.dev.read_phys(self.fs.block_sectors(start), req)?;
                req.clear();
            }
            Ok(())
        };
        'pages: for &(idx, pa) in &pages {
            for k in 0..bpp {
                let lblk = idx * bpp + k;
                if lblk * bs >= size {
                    break 'pages;
                }
                match self.map(lblk as u32) {
                    Ok(Some(r)) if !r.unwritten => {
                        if r.pblk != next_pblk || req.len() >= 120 {
                            if let Err(e) = flush(req_start, &mut req) {
                                result = Err(e);
                                break 'pages;
                            }
                            req_start = r.pblk;
                        }
                        let seg = (pa + (k * bs) as usize, bs as usize);
                        // merge with the previous segment if it is the adjacent part of a page
                        match req.last_mut() {
                            Some(last) if last.0 + last.1 == seg.0 => last.1 += seg.1,
                            _ => req.push(seg),
                        }
                        next_pblk = r.pblk + 1;
                    }
                    Ok(_) => {
                        // hole or unwritten: zeros (already)
                        next_pblk = u64::MAX;
                    }
                    Err(e) => {
                        result = Err(e);
                        break 'pages;
                    }
                }
            }
        }
        if result.is_ok() {
            result = flush(req_start, &mut req);
        }
        for (idx, pa) in pages {
            if result.is_ok() {
                let mut m = self.cache.pages.lock();
                if let alloc::collections::btree_map::Entry::Vacant(e) = m.entry(idx) {
                    e.insert(pa);
                    continue;
                }
            }
            frame::put_page(pa);
        }
        result
    }

    /// The page-cache page for file page `idx` (no extra reference).
    pub fn page(&self, idx: u64) -> Result<usize> {
        if let Some(p) = self.cache.lookup(idx) {
            return Ok(p);
        }
        let last = self.size().div_ceil(PAGE_SIZE as u64);
        let n = READAHEAD_PAGES.min(last.saturating_sub(idx)).max(1);
        self.fill_pages(idx, n)?;
        self.cache.lookup(idx).ok_or(EIO)
    }

    fn read_dir_block(&self, lblk: u32) -> Result<Option<Arc<crate::fs::bufcache::Buf>>> {
        match self.map(lblk)? {
            Some(r) if !r.unwritten => Ok(Some(self.fs.bufs.get(r.pblk)?)),
            _ => Ok(None),
        }
    }

    fn dir_blocks(&self) -> u32 {
        (self.size() / self.fs.bs as u64) as u32
    }

    pub fn find(&self, name: &[u8]) -> Result<Option<u32>> {
        for b in 0..self.dir_blocks() {
            let Some(buf) = self.read_dir_block(b)? else { continue };
            let mut found = None;
            buf.read(|d| {
                dir::walk_block(d, |e| {
                    if e.name == name {
                        found = Some(e.ino);
                        false
                    } else {
                        true
                    }
                })
            });
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }
}

impl Inode for Ext4Inode {
    fn stat(&self) -> Result<Stat> {
        let r = self.raw.lock();
        let mode = le16(&r, 0) as u32;
        let extra = if r.len() > 128 { le16(&r, 128) as usize } else { 0 };
        let has = |o: usize| r.len() > 128 && 128 + extra >= o + 4;
        let x = |o: usize| if has(o) { le32(&r, o) } else { 0 };
        let flags = le32(&r, 32);
        let mut blocks = le32(&r, 28) as u64 | ((le16(&r, 116) as u64) << 32);
        if self.fs.sb.lock().ro_compat & RO_COMPAT_HUGE_FILE != 0 && flags & super::inode::EXT4_HUGE_FILE_FL != 0 {
            blocks *= self.fs.bs as u64 / 512;
        }
        let rdev = if mode & S_IFMT == S_IFCHR || mode & S_IFMT == S_IFBLK {
            let old = le32(&r, 40);
            if old != 0 {
                makedev((old >> 8) & 0xff, old & 0xff)
            } else {
                let new = le32(&r, 44);
                makedev((new & 0xfff00) >> 8, (new & 0xff) | ((new >> 12) & 0xfff00))
            }
        } else {
            0
        };
        Ok(Stat {
            dev: self.fs.devno,
            ino: self.ino as u64,
            mode,
            nlink: le16(&r, 26) as u32,
            uid: le16(&r, 2) as u32 | ((le16(&r, 120) as u32) << 16),
            gid: le16(&r, 24) as u32 | ((le16(&r, 122) as u32) << 16),
            rdev,
            size: le32(&r, 4) as u64 | ((le32(&r, 108) as u64) << 32),
            blksize: 4096,
            blocks,
            atime: ts(le32(&r, 8), x(140)),
            mtime: ts(le32(&r, 16), x(136)),
            ctime: ts(le32(&r, 12), x(132)),
            btime: ts(x(144), x(148)),
        })
    }

    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize> {
        if self.is_dir() {
            return Err(EISDIR);
        }
        let size = self.size();
        if off >= size {
            return Ok(0);
        }
        let n = buf.len().min((size - off) as usize);
        let mut done = 0;
        while done < n {
            let p = off + done as u64;
            let idx = p / PAGE_SIZE as u64;
            let po = (p % PAGE_SIZE as u64) as usize;
            let k = (PAGE_SIZE - po).min(n - done);
            let pa = self.page(idx)?;
            buf[done..done + k].copy_from_slice(&page_slice(pa)[po..po + k]);
            done += k;
        }
        Ok(n)
    }

    fn write_at(&self, off: u64, buf: &[u8]) -> Result<usize> {
        if self.is_dir() {
            return Err(EISDIR);
        }
        self.do_write(off, buf)
    }

    fn setattr(&self, a: &SetAttr) -> Result<()> {
        if self.fs.ro {
            return Err(EROFS);
        }
        let _g = self.fs.wlock.lock();
        if let Some(size) = a.size {
            self.do_truncate(size)?;
        }
        self.apply_attr(a);
        Ok(())
    }

    fn fsync(&self, _datasync: bool) -> Result<()> {
        self.fs.commit()
    }

    fn page_mkwrite(&self, index: u64) {
        self.dirty_pages.lock().insert(index);
        self.fs.note_dirty_data(self);
    }

    fn fallocate(&self, mode: u32, off: u64, len: u64) -> Result<()> {
        if mode != 0 {
            return Err(EOPNOTSUPP);
        }
        // the size grows; blocks are allocated when data is written
        if off + len > self.size() {
            let _g = self.lock.lock();
            self.set_size(off + len);
        }
        Ok(())
    }

    fn create(&self, name: &[u8], mode: u32, rdev: u64, uid: u32, gid: u32) -> Result<Arc<dyn Inode>> {
        Ok(self.op_create(name, mode, rdev, uid, gid, None)?)
    }

    fn symlink(&self, name: &[u8], target: &[u8], uid: u32, gid: u32) -> Result<Arc<dyn Inode>> {
        Ok(self.op_create(name, S_IFLNK | 0o777, 0, uid, gid, Some(target))?)
    }

    fn link(&self, name: &[u8], target: &Arc<dyn Inode>) -> Result<()> {
        self.op_link(name, target)
    }

    fn unlink(&self, name: &[u8]) -> Result<()> {
        self.op_unlink(name, false)
    }

    fn rmdir(&self, name: &[u8]) -> Result<()> {
        self.op_unlink(name, true)
    }

    fn rename(&self, old: &[u8], newdir: &Arc<dyn Inode>, new: &[u8], flags: u32) -> Result<()> {
        self.op_rename(old, newdir, new, flags)
    }

    fn get_page(&self, index: u64) -> Result<usize> {
        if index * PAGE_SIZE as u64 >= self.size() {
            return Err(ENXIO);
        }
        let pa = self.page(index)?;
        frame::get_page(pa);
        Ok(pa)
    }

    fn mmap_writable(&self) -> bool {
        !self.fs.ro
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>> {
        if !self.is_dir() {
            return Err(ENOTDIR);
        }
        match self.find(name)? {
            Some(ino) => Ok(self.fs.iget(ino)?),
            None => Err(ENOENT),
        }
    }

    fn readdir(&self, pos: u64, f: &mut dyn FnMut(&[u8], u64, u8, u64) -> bool) -> Result<()> {
        if !self.is_dir() {
            return Err(ENOTDIR);
        }
        let bs = self.fs.bs as u64;
        let mut b = (pos / bs) as u32;
        let mut start = (pos % bs) as usize;
        while b < self.dir_blocks() {
            if let Some(buf) = self.read_dir_block(b)? {
                let entries: Vec<(Vec<u8>, u32, u8, usize)> = buf.read(|d| {
                    let mut v = Vec::new();
                    dir::walk_block(d, |e| {
                        if e.off >= start {
                            v.push((e.name.to_vec(), e.ino, e.ftype, e.off + e.rec_len));
                        }
                        true
                    });
                    v
                });
                for (name, ino, ft, next) in entries {
                    if !f(&name, ino as u64, dir::ftype_to_dt(ft), b as u64 * bs + next as u64) {
                        return Ok(());
                    }
                }
            }
            b += 1;
            start = 0;
        }
        Ok(())
    }

    fn readlink(&self) -> Result<Vec<u8>> {
        if self.mode() & S_IFMT != S_IFLNK {
            return Err(EINVAL);
        }
        let size = self.size() as usize;
        if size < 60 && self.flags() & (EXT4_EXTENTS_FL | EXT4_INLINE_DATA_FL) == 0 {
            return Ok(self.iblock()[..size].to_vec()); // a fast symlink: the target is in i_block
        }
        let mut v = alloc::vec![0u8; size];
        let mut done = 0;
        while done < size {
            let idx = (done / PAGE_SIZE) as u64;
            let pa = self.page(idx)?;
            let k = PAGE_SIZE.min(size - done);
            v[done..done + k].copy_from_slice(&page_slice(pa)[..k]);
            done += k;
        }
        Ok(v)
    }

    fn fs(&self) -> Option<Arc<dyn FileSystem>> {
        Some(self.fs.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
