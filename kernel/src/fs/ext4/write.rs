//! Changing an ext4 file system: inode updates, file data (page cache, block allocation),
//! directory entries, creating and removing files, and committing everything through the journal.
use super::dir;
use super::inode::{Ext4Inode, EXT4_EXTENTS_FL, EXT4_INDEX_FL};
use super::{le16, le32, Ext4};
use crate::errno::*;
use crate::mm::frame::{self, PAGE_SIZE};
use crate::vfs::*;
use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn ftype_of(mode: u32) -> u8 {
    match mode & S_IFMT {
        S_IFREG => 1,
        S_IFDIR => 2,
        S_IFCHR => 3,
        S_IFBLK => 4,
        S_IFIFO => 5,
        S_IFSOCK => 6,
        S_IFLNK => 7,
        _ => 0,
    }
}

impl Ext4Inode {
    pub fn seed(&self) -> u32 {
        let generation = le32(&self.raw.lock(), 100);
        self.fs.inode_seed(self.ino, generation)
    }

    pub fn set_iblock(&self, b: &[u8]) {
        self.raw.lock()[40..40 + b.len()].copy_from_slice(b);
        self.mark_dirty();
    }

    pub fn set_flags(&self, f: u32) {
        put32(&mut self.raw.lock(), 32, f);
        self.mark_dirty();
    }

    pub fn set_size(&self, size: u64) {
        let mut r = self.raw.lock();
        put32(&mut r, 4, size as u32);
        put32(&mut r, 108, (size >> 32) as u32);
        drop(r);
        self.mark_dirty();
    }

    pub fn nlink(&self) -> u16 {
        le16(&self.raw.lock(), 26)
    }

    pub fn set_nlink(&self, n: u16) {
        put16(&mut self.raw.lock(), 26, n);
        self.mark_dirty();
    }

    /// Adds `n` file system blocks to i_blocks (512-byte units).
    pub fn add_blocks(&self, n: i64) {
        let per = (self.fs.bs / 512) as i64;
        let mut r = self.raw.lock();
        let cur = le32(&r, 28) as i64 | ((le16(&r, 116) as i64) << 32);
        let v = (cur + n * per).max(0) as u64;
        put32(&mut r, 28, v as u32);
        put16(&mut r, 116, (v >> 32) as u16);
    }

    fn set_time(&self, off: usize, extra_off: usize, t: Timespec) {
        let mut r = self.raw.lock();
        put32(&mut r, off, t.sec as u32);
        let extra = le16(&r, 128) as usize;
        if r.len() > 128 && 128 + extra >= extra_off + 4 {
            let epoch = ((t.sec >> 32) & 3) as u32;
            put32(&mut r, extra_off, ((t.nsec as u32) << 2) | epoch);
        }
    }

    pub fn touch(&self, m: bool, c: bool) {
        let now = Timespec::now();
        if m {
            self.set_time(16, 136, now);
        }
        if c {
            self.set_time(12, 132, now);
        }
        self.mark_dirty();
    }

    /// Records the inode for writing back at the next commit.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
        if let Some(me) = self.me.upgrade() {
            self.fs.dirty_inodes.lock().insert(self.ino, me);
        }
    }

    /// Writes the in-memory inode into its inode table block (with its checksum).
    pub fn flush_raw(&self) -> Result<()> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let (blk, off) = self.fs.inode_location(self.ino)?;
        let mut raw = self.raw.lock().clone();
        self.fs.set_inode_csum(self.ino, &mut raw);
        let b = self.fs.bufs.get(blk)?;
        b.write(|d| d[off..off + raw.len()].copy_from_slice(&raw));
        Ok(())
    }

    /// The physical block for file block `lblk`, allocating it when it has none.
    fn block_for_write(&self, lblk: u32) -> Result<u64> {
        if let Some(r) = self.map(lblk)? {
            if !r.unwritten {
                return Ok(r.pblk);
            }
        }
        // goal: right after the previous block of the file
        let goal = if lblk > 0 { self.map(lblk - 1)?.map(|r| r.pblk + 1) } else { None };
        let goal = goal.unwrap_or_else(|| {
            let ipg = self.fs.sb.lock().inodes_per_group;
            self.fs.group_first_block(((self.ino - 1) / ipg) as usize)
        });
        let (p, _) = self.fs.alloc_blocks(goal, 1)?;
        self.insert_extent(lblk, p, 1)?;
        self.add_blocks(1);
        Ok(p)
    }

    pub fn do_write(&self, off: u64, data: &[u8]) -> Result<usize> {
        if self.fs.ro {
            return Err(EROFS);
        }
        let _g = self.lock.lock();
        let size = self.size();
        let end = off + data.len() as u64;
        let mut done = 0;
        while done < data.len() {
            let p = off + done as u64;
            let idx = p / PAGE_SIZE as u64;
            let po = (p % PAGE_SIZE as u64) as usize;
            let k = (PAGE_SIZE - po).min(data.len() - done);
            let pa = match self.cache.lookup(idx) {
                Some(pa) => pa,
                None => {
                    let whole = po == 0 && k == PAGE_SIZE;
                    if !whole && idx * (PAGE_SIZE as u64) < size {
                        self.page(idx)?
                    } else {
                        let pa = frame::alloc_pages(0).ok_or(ENOMEM)?;
                        page_slice(pa).fill(0);
                        let mut m = self.cache.pages.lock();
                        match m.get(&idx) {
                            Some(&x) => {
                                drop(m);
                                frame::put_page(pa);
                                x
                            }
                            None => {
                                m.insert(idx, pa);
                                pa
                            }
                        }
                    }
                }
            };
            page_slice(pa)[po..po + k].copy_from_slice(&data[done..done + k]);
            self.dirty_pages.lock().insert(idx);
            done += k;
        }
        if end > size {
            self.set_size(end);
        }
        self.touch(true, true);
        self.fs.note_dirty_data(self);
        Ok(data.len())
    }

    /// Writes the dirty pages to disk, allocating blocks for them (ordered mode: before the
    /// metadata that points at them is committed).
    pub fn writeback(&self) -> Result<()> {
        let pages: BTreeSet<u64> = core::mem::take(&mut *self.dirty_pages.lock());
        if pages.is_empty() {
            return Ok(());
        }
        let _g = self.lock.lock();
        let bs = self.fs.bs as u64;
        let bpp = PAGE_SIZE as u64 / bs;
        let size = self.size();
        let mut still_mapped = BTreeSet::new();
        for idx in pages {
            let Some(pa) = self.cache.lookup(idx) else { continue };
            // a page also mapped (MAP_SHARED) may be written again through the mapping without a
            // fault: keep writing it back while it is mapped
            if frame::page_refs(pa) > 1 {
                still_mapped.insert(idx);
            }
            for k in 0..bpp {
                let lblk = idx * bpp + k;
                if lblk * bs >= size {
                    break;
                }
                let pblk = self.block_for_write(lblk as u32)?;
                self.fs.dev.write_phys(self.fs.block_sectors(pblk), &[(pa + (k * bs) as usize, bs as usize)])?;
            }
        }
        if !still_mapped.is_empty() {
            self.dirty_pages.lock().extend(still_mapped);
            self.fs.note_dirty_data(self);
        }
        Ok(())
    }

    pub fn do_truncate(&self, new: u64) -> Result<()> {
        if self.fs.ro {
            return Err(EROFS);
        }
        let _g = self.lock.lock();
        let old = self.size();
        if new < old {
            let bs = self.fs.bs as u64;
            let first_free = new.div_ceil(bs) as u32;
            // drop cached pages and pending writes past the end, zero the tail of the last page
            let first_gone_page = new.div_ceil(PAGE_SIZE as u64);
            self.cache.truncate_from(first_gone_page);
            self.dirty_pages.lock().retain(|&p| p < first_gone_page);
            if new % PAGE_SIZE as u64 != 0 {
                let idx = new / PAGE_SIZE as u64;
                if let Ok(pa) = self.page(idx) {
                    page_slice(pa)[(new % PAGE_SIZE as u64) as usize..].fill(0);
                    self.dirty_pages.lock().insert(idx);
                    self.fs.note_dirty_data(self);
                }
            }
            self.remove_extents_from(first_free)?;
        }
        self.set_size(new);
        self.touch(true, true);
        Ok(())
    }

    // ---- directories

    fn dir_block_buf(&self, lblk: u32) -> Result<Option<Arc<crate::fs::bufcache::Buf>>> {
        match self.map(lblk)? {
            Some(r) if !r.unwritten => Ok(Some(self.fs.bufs.get(r.pblk)?)),
            _ => Ok(None),
        }
    }

    /// Adds a directory entry.
    pub fn add_entry(&self, name: &[u8], ino: u32, ftype: u8) -> Result<()> {
        let need = (8 + name.len() + 3) & !3;
        let bs = self.fs.bs;
        let tail = self.fs.dir_tail_len();
        let seed = self.seed();
        let nblocks = (self.size() / bs as u64) as u32;
        let indexed = self.flags() & EXT4_INDEX_FL != 0;
        let candidates: Vec<u32> = if indexed {
            alloc::vec![super::htree::leaf_for(self, name)?]
        } else {
            (0..nblocks).collect()
        };
        for b in candidates {
            let Some(buf) = self.dir_block_buf(b)? else { continue };
            let placed = buf.write(|d| {
                let limit = bs - tail;
                let mut off = 0;
                while off + 8 <= limit {
                    let e_ino = le32(d, off);
                    let rec = le16(d, off + 4) as usize;
                    if rec < 8 || off + rec > limit {
                        break;
                    }
                    let used = if e_ino == 0 { 0 } else { (8 + d[off + 6] as usize + 3) & !3 };
                    if rec - used >= need {
                        let (at, len) = if e_ino == 0 { (off, rec) } else {
                            put16(d, off + 4, used as u16);
                            (off + used, rec - used)
                        };
                        put32(d, at, ino);
                        put16(d, at + 4, len as u16);
                        d[at + 6] = name.len() as u8;
                        d[at + 7] = ftype;
                        d[at + 8..at + 8 + name.len()].copy_from_slice(name);
                        if tail > 0 {
                            self.fs.set_dirent_tail(seed, d);
                        }
                        return true;
                    }
                    off += rec;
                }
                false
            });
            if placed {
                self.touch(true, true);
                return Ok(());
            }
        }
        if indexed {
            return super::htree::split_and_add(self, name, ino, ftype);
        }
        // a new block at the end
        let lblk = nblocks;
        let pblk = self.block_for_write(lblk)?;
        let buf = self.fs.bufs.get_new(pblk);
        buf.write(|d| {
            put32(d, 0, ino);
            put16(d, 4, (bs - tail) as u16);
            d[6] = name.len() as u8;
            d[7] = ftype;
            d[8..8 + name.len()].copy_from_slice(name);
            if tail > 0 {
                self.fs.set_dirent_tail(seed, d);
            }
        });
        self.set_size((lblk as u64 + 1) * bs as u64);
        self.touch(true, true);
        Ok(())
    }

    /// Removes the entry `name`; returns its inode number.
    pub fn remove_entry(&self, name: &[u8]) -> Result<u32> {
        let bs = self.fs.bs;
        let tail = self.fs.dir_tail_len();
        let seed = self.seed();
        for b in 0..(self.size() / bs as u64) as u32 {
            let Some(buf) = self.dir_block_buf(b)? else { continue };
            let found = buf.write(|d| {
                let limit = bs - tail;
                let mut off = 0;
                let mut prev: Option<usize> = None;
                while off + 8 <= limit {
                    let e_ino = le32(d, off);
                    let rec = le16(d, off + 4) as usize;
                    if rec < 8 || off + rec > limit {
                        break;
                    }
                    let nl = d[off + 6] as usize;
                    if e_ino != 0 && &d[off + 8..off + 8 + nl] == name {
                        match prev {
                            Some(p) => {
                                let prec = le16(d, p + 4) as usize;
                                put16(d, p + 4, (prec + rec) as u16);
                            }
                            None => put32(d, off, 0),
                        }
                        if tail > 0 {
                            self.fs.set_dirent_tail(seed, d);
                        }
                        return Some(e_ino);
                    }
                    prev = Some(off);
                    off += rec;
                }
                None
            });
            if let Some(i) = found {
                self.touch(true, true);
                return Ok(i);
            }
        }
        Err(ENOENT)
    }

    /// Changes the inode an entry points to (rename over ".." when moving a directory).
    pub fn retarget_entry(&self, name: &[u8], ino: u32) -> Result<()> {
        let bs = self.fs.bs;
        let tail = self.fs.dir_tail_len();
        let seed = self.seed();
        for b in 0..(self.size() / bs as u64) as u32 {
            let Some(buf) = self.dir_block_buf(b)? else { continue };
            let done = buf.write(|d| {
                let mut hit = None;
                dir::walk_block(&d[..bs - tail], |e| {
                    if e.name == name {
                        hit = Some(e.off);
                        false
                    } else {
                        true
                    }
                });
                if let Some(off) = hit {
                    put32(d, off, ino);
                    if tail > 0 {
                        self.fs.set_dirent_tail(seed, d);
                    }
                    true
                } else {
                    false
                }
            });
            if done {
                return Ok(());
            }
        }
        Err(ENOENT)
    }

    pub fn is_empty_dir(&self) -> Result<bool> {
        let mut empty = true;
        self.readdir_raw(|n, _| {
            if n != b"." && n != b".." {
                empty = false;
            }
            empty
        })?;
        Ok(empty)
    }

    fn readdir_raw(&self, mut f: impl FnMut(&[u8], u32) -> bool) -> Result<()> {
        for b in 0..(self.size() / self.fs.bs as u64) as u32 {
            let Some(buf) = self.dir_block_buf(b)? else { continue };
            let go = buf.read(|d| dir::walk_block(d, |e| f(e.name, e.ino)));
            if !go {
                break;
            }
        }
        Ok(())
    }
}

impl Ext4 {
    pub fn note_dirty_data(&self, i: &Ext4Inode) {
        if let Some(me) = i.me.upgrade() {
            self.dirty_data.lock().insert(i.ino, me);
        }
    }

    /// A new inode: allocated, zeroed, with mode and owner, written at the next commit.
    pub fn new_inode(&self, dir_ino: u32, mode: u32, uid: u32, gid: u32, rdev: u64) -> Result<Arc<Ext4Inode>> {
        let is_dir = mode & S_IFMT == S_IFDIR;
        let ino = self.alloc_inode(is_dir, dir_ino)?;
        let isz = self.sb.lock().inode_size;
        let mut raw = alloc::vec![0u8; isz];
        put16(&mut raw, 0, mode as u16);
        put16(&mut raw, 2, uid as u16);
        put16(&mut raw, 120, (uid >> 16) as u16);
        put16(&mut raw, 24, gid as u16);
        put16(&mut raw, 122, (gid >> 16) as u16);
        let generation = crate::random::u64() as u32;
        put32(&mut raw, 100, generation);
        if isz > 128 {
            put16(&mut raw, 128, 32); // i_extra_isize
        }
        let t = mode & S_IFMT;
        if t == S_IFREG || t == S_IFDIR || t == S_IFLNK {
            // an empty extent tree
            let mut root = [0u8; 12];
            root[0..2].copy_from_slice(&super::extent::EXTENT_MAGIC.to_le_bytes());
            root[4..6].copy_from_slice(&4u16.to_le_bytes());
            raw[40..52].copy_from_slice(&root);
            put32(&mut raw, 32, EXT4_EXTENTS_FL);
        }
        if t == S_IFCHR || t == S_IFBLK {
            // new_encode_dev
            let (ma, mi) = (major(rdev), minor(rdev));
            put32(&mut raw, 44, (mi & 0xff) | (ma << 8) | ((mi & !0xff) << 12));
        }
        self.forget_inode(ino);
        let i = Ext4Inode::from_raw(self.me.upgrade().unwrap(), ino, raw);
        let now = Timespec::now();
        for (o, e) in [(8usize, 140usize), (12, 132), (16, 136), (144, 148)] {
            i.set_time(o, e, now);
        }
        i.mark_dirty();
        self.insert_inode(&i);
        Ok(i)
    }

    /// Frees an inode whose last link and last user are gone.
    pub fn release_inode(&self, ino: u32) -> Result<()> {
        let i = self.iget(ino)?;
        if i.nlink() != 0 {
            return Ok(());
        }
        let is_dir = i.mode() & S_IFMT == S_IFDIR;
        i.dirty_pages.lock().clear();
        i.cache.truncate_from(0);
        if i.mode() & S_IFMT != S_IFLNK || i.flags() & EXT4_EXTENTS_FL != 0 {
            i.remove_extents_from(0)?;
        }
        {
            let mut r = i.raw.lock();
            let now = Timespec::now().sec as u32;
            put32(&mut r, 20, now); // i_dtime
            put16(&mut r, 0, 0);
        }
        i.mark_dirty();
        i.flush_raw()?;
        self.free_inode(ino, is_dir)?;
        Ok(())
    }

    /// Writes back file data and commits the metadata changes through the journal.
    pub fn commit(&self) -> Result<()> {
        if self.ro {
            return Ok(());
        }
        let _g = self.commit_lock.lock();
        // data first (ordered mode)
        let data: Vec<Arc<Ext4Inode>> = core::mem::take(&mut *self.dirty_data.lock()).into_values().collect();
        let _w = self.wlock.lock();
        for i in &data {
            i.writeback()?;
        }
        // inodes whose last link went while they were open
        let orphans: Vec<u32> = core::mem::take(&mut *self.orphans.lock());
        for ino in orphans {
            self.release_inode(ino)?;
        }
        let inodes: Vec<Arc<Ext4Inode>> = core::mem::take(&mut *self.dirty_inodes.lock()).into_values().collect();
        for i in &inodes {
            i.flush_raw()?;
        }
        self.apply_bitmap_csums()?;
        self.write_super_buf()?;
        let dirty = self.bufs.dirty_bufs();
        if dirty.is_empty() {
            return Ok(());
        }
        let blocks: Vec<(u64, Vec<u8>)> = dirty.iter().map(|b| (b.block, b.data.lock().clone())).collect();
        for b in &dirty {
            b.dirty.store(false, Ordering::Release);
        }
        let mut j = self.journal.lock();
        match j.as_mut() {
            Some(j) => j.commit(self, &blocks)?,
            None => {
                for (blk, d) in &blocks {
                    self.dev.write(self.block_sectors(*blk), d)?;
                }
                self.dev.flush()?;
            }
        }
        Ok(())
    }

    /// The superblock (free counts, state) into the buffer holding it.
    pub fn write_super_buf(&self) -> Result<()> {
        let (blk, off) = if self.bs == 1024 { (1u64, 0usize) } else { (0, 1024) };
        let raw = {
            let mut sb = self.sb.lock();
            let fb = sb.free_blocks;
            let fi = sb.free_inodes;
            put32(&mut sb.raw, 12, fb as u32);
            if sb.incompat & 0x80 != 0 {
                put32(&mut sb.raw, 344, (fb >> 32) as u32);
            }
            put32(&mut sb.raw, 16, fi);
            let mut r = sb.raw.clone();
            drop(sb);
            self.set_sb_csum(&mut r);
            self.sb.lock().raw = r.clone();
            r
        };
        let b = self.bufs.get(blk)?;
        let changed = b.read(|d| d[off..off + 1024] != raw[..]);
        if changed {
            b.write(|d| d[off..off + 1024].copy_from_slice(&raw));
        }
        Ok(())
    }
}

pub fn ext4_ftype(mode: u32) -> u8 {
    ftype_of(mode)
}

impl Ext4Inode {
    pub fn apply_attr(&self, a: &SetAttr) {
        {
            let mut r = self.raw.lock();
            if let Some(m) = a.mode {
                let t = le16(&r, 0) as u32 & S_IFMT;
                put16(&mut r, 0, (t | (m & 0o7777)) as u16);
            }
            if let Some(u) = a.uid {
                put16(&mut r, 2, u as u16);
                put16(&mut r, 120, (u >> 16) as u16);
            }
            if let Some(g) = a.gid {
                put16(&mut r, 24, g as u16);
                put16(&mut r, 122, (g >> 16) as u16);
            }
        }
        if let Some(t) = a.atime {
            self.set_time(8, 140, t);
        }
        if let Some(t) = a.mtime {
            self.set_time(16, 136, t);
        }
        self.touch(false, true);
    }

    pub fn op_create(&self, name: &[u8], mode: u32, rdev: u64, uid: u32, gid: u32, target: Option<&[u8]>) -> Result<Arc<Ext4Inode>> {
        if self.fs.ro {
            return Err(EROFS);
        }
        let _g = self.fs.wlock.lock();
        if self.find(name)?.is_some() {
            return Err(EEXIST);
        }
        let i = self.fs.new_inode(self.ino, mode, uid, gid, rdev)?;
        let t = mode & S_IFMT;
        if t == S_IFDIR {
            let bs = self.fs.bs;
            let tail = self.fs.dir_tail_len();
            let pblk = i.block_for_write_pub(0)?;
            let seed = i.seed();
            let parent = self.ino;
            self.fs.bufs.get_new(pblk).write(|d| {
                put32(d, 0, i.ino);
                put16(d, 4, 12);
                d[6] = 1;
                d[7] = 2;
                d[8] = b'.';
                put32(d, 12, parent);
                put16(d, 16, (bs - 12 - tail) as u16);
                d[18] = 2;
                d[19] = 2;
                d[20] = b'.';
                d[21] = b'.';
                if tail > 0 {
                    self.fs.set_dirent_tail(seed, d);
                }
            });
            i.set_size(bs as u64);
            i.set_nlink(2);
            self.set_nlink(self.nlink() + 1);
        } else {
            i.set_nlink(1);
        }
        if let Some(target) = target {
            if target.len() < 60 {
                // a fast symlink: the target lives in i_block
                let mut b = [0u8; 60];
                b[..target.len()].copy_from_slice(target);
                i.set_iblock(&b);
                i.set_flags(i.flags() & !EXT4_EXTENTS_FL);
                i.set_size(target.len() as u64);
            } else {
                i.do_write(0, target)?;
            }
        }
        if let Err(e) = self.add_entry(name, i.ino, ftype_of(mode)) {
            i.set_nlink(0);
            return Err(e);
        }
        Ok(i)
    }

    pub fn block_for_write_pub(&self, lblk: u32) -> Result<u64> {
        self.block_for_write(lblk)
    }

    pub fn op_link(&self, name: &[u8], target: &Arc<dyn Inode>) -> Result<()> {
        let t = target.as_any().downcast_ref::<Ext4Inode>().ok_or(EXDEV)?;
        if !Arc::ptr_eq(&t.fs, &self.fs) {
            return Err(EXDEV);
        }
        if self.fs.ro {
            return Err(EROFS);
        }
        let _g = self.fs.wlock.lock();
        if self.find(name)?.is_some() {
            return Err(EEXIST);
        }
        if t.nlink() >= 65000 {
            return Err(EMLINK);
        }
        self.add_entry(name, t.ino, ftype_of(t.mode()))?;
        t.set_nlink(t.nlink() + 1);
        t.touch(false, true);
        Ok(())
    }

    pub fn op_unlink(&self, name: &[u8], dir: bool) -> Result<()> {
        if self.fs.ro {
            return Err(EROFS);
        }
        let _g = self.fs.wlock.lock();
        let ino = self.find(name)?.ok_or(ENOENT)?;
        let i = self.fs.iget(ino)?;
        let is_dir = i.mode() & S_IFMT == S_IFDIR;
        if dir && !is_dir {
            return Err(ENOTDIR);
        }
        if !dir && is_dir {
            return Err(EISDIR);
        }
        if is_dir && !i.is_empty_dir()? {
            return Err(ENOTEMPTY);
        }
        self.remove_entry(name)?;
        if is_dir {
            i.set_nlink(0);
            self.set_nlink(self.nlink().saturating_sub(1));
        } else {
            i.set_nlink(i.nlink().saturating_sub(1));
        }
        i.touch(false, true);
        Ok(())
    }

    pub fn op_rename(&self, old: &[u8], newdir: &Arc<dyn Inode>, new: &[u8], flags: u32) -> Result<()> {
        let nd = newdir.as_any().downcast_ref::<Ext4Inode>().ok_or(EXDEV)?;
        if !Arc::ptr_eq(&nd.fs, &self.fs) {
            return Err(EXDEV);
        }
        if self.fs.ro {
            return Err(EROFS);
        }
        let _g = self.fs.wlock.lock();
        let src_ino = self.find(old)?.ok_or(ENOENT)?;
        let src = self.fs.iget(src_ino)?;
        let src_dir = src.mode() & S_IFMT == S_IFDIR;
        let same_dir = nd.ino == self.ino;
        let dst = nd.find(new)?;
        if flags & 2 != 0 {
            // RENAME_EXCHANGE
            let dst_ino = dst.ok_or(ENOENT)?;
            let d = self.fs.iget(dst_ino)?;
            self.retarget_entry(old, dst_ino)?;
            nd.retarget_entry(new, src_ino)?;
            if !same_dir {
                let d_dir = d.mode() & S_IFMT == S_IFDIR;
                if src_dir {
                    src.retarget_entry(b"..", nd.ino)?;
                }
                if d_dir {
                    d.retarget_entry(b"..", self.ino)?;
                }
                if src_dir != d_dir {
                    let (gain, lose) = if src_dir { (nd, self) } else { (self, nd) };
                    gain.set_nlink(gain.nlink() + 1);
                    lose.set_nlink(lose.nlink() - 1);
                }
            }
            return Ok(());
        }
        if let Some(dst_ino) = dst {
            if dst_ino == src_ino {
                return Ok(());
            }
            if flags & 1 != 0 {
                return Err(EEXIST);
            }
            let d = self.fs.iget(dst_ino)?;
            let d_dir = d.mode() & S_IFMT == S_IFDIR;
            if src_dir && !d_dir {
                return Err(ENOTDIR);
            }
            if !src_dir && d_dir {
                return Err(EISDIR);
            }
            if d_dir && !d.is_empty_dir()? {
                return Err(ENOTEMPTY);
            }
            nd.retarget_entry(new, src_ino)?;
            if d_dir {
                d.set_nlink(0);
                nd.set_nlink(nd.nlink() - 1);
            } else {
                d.set_nlink(d.nlink().saturating_sub(1));
            }
            d.touch(false, true);
        } else {
            nd.add_entry(new, src_ino, ftype_of(src.mode()))?;
        }
        self.remove_entry(old)?;
        if src_dir && !same_dir {
            src.retarget_entry(b"..", nd.ino)?;
            self.set_nlink(self.nlink() - 1);
            nd.set_nlink(nd.nlink() + 1);
        }
        src.touch(false, true);
        Ok(())
    }
}

impl Drop for Ext4Inode {
    fn drop(&mut self) {
        // the last user of an inode without links: free it at the next commit
        let r = self.raw.lock();
        if le16(&r, 26) == 0 && le16(&r, 0) != 0 && !self.fs.ro {
            drop(r);
            self.fs.orphans.lock().push(self.ino);
        }
    }
}
