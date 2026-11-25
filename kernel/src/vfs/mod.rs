//! The virtual file system: inodes and file systems (traits), the page cache, dentries and mounts
//! (path.rs), open files and descriptor tables (file.rs).
pub mod anon;
pub mod cpio;
pub mod dev;
pub mod events;
pub mod file;
pub mod path;
pub mod pipe;
pub mod shmem;
pub mod poll;
pub mod pseudo;
pub mod tmpfs;

use crate::errno::*;
use crate::mm::frame::{self, PAGE_SIZE};
use crate::sync::SpinLock;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

pub use file::{File, FileOps, FdTable};
pub use path::{Mount, PathRef};

pub const S_IFMT: u32 = 0o170000;
pub const S_IFSOCK: u32 = 0o140000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFIFO: u32 = 0o010000;
pub const S_ISUID: u32 = 0o4000;
pub const S_ISGID: u32 = 0o2000;
pub const S_ISVTX: u32 = 0o1000;

pub const DT_FIFO: u8 = 1;
pub const DT_CHR: u8 = 2;
pub const DT_DIR: u8 = 4;
pub const DT_BLK: u8 = 6;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;
pub const DT_SOCK: u8 = 12;

pub fn dtype(mode: u32) -> u8 {
    ((mode & S_IFMT) >> 12) as u8
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i64,
}

impl Timespec {
    pub fn now() -> Timespec {
        let ns = crate::time::realtime_ns();
        Timespec { sec: ns.div_euclid(1_000_000_000), nsec: ns.rem_euclid(1_000_000_000) }
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: u64,
    pub blksize: u32,
    pub blocks: u64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub btime: Timespec,
}

#[derive(Default, Clone, Copy)]
pub struct SetAttr {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<Timespec>,
    pub mtime: Option<Timespec>,
}

#[derive(Default, Clone, Copy)]
pub struct StatFs {
    pub fs_type: u64,
    pub bsize: u64,
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub namelen: u64,
    pub flags: u64,
}

pub trait FileSystem: Send + Sync {
    fn name(&self) -> &'static str;
    fn root(&self) -> Arc<dyn Inode>;
    fn dev(&self) -> u64;
    fn statfs(&self) -> StatFs {
        StatFs { bsize: 4096, namelen: 255, ..Default::default() }
    }
    fn sync(&self) -> Result<()> {
        Ok(())
    }
    /// Whether lookups may be cached in dentries (false for /proc and /sys, whose contents change).
    fn cache_dentries(&self) -> bool {
        true
    }
    fn as_any(&self) -> &dyn Any;
}

/// A file system object. Directory methods take names as bytes (no '/', not "." or "..").
#[allow(unused_variables)]
pub trait Inode: Send + Sync + Any {
    fn stat(&self) -> Result<Stat>;
    fn setattr(&self, a: &SetAttr) -> Result<()> {
        Err(EPERM)
    }

    // regular files
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize> {
        Err(EINVAL)
    }
    fn write_at(&self, off: u64, buf: &[u8]) -> Result<usize> {
        Err(EINVAL)
    }
    fn fsync(&self, datasync: bool) -> Result<()> {
        Ok(())
    }
    /// The physical page caching file page `index`, with a new reference for the caller (mmap).
    /// Beyond the end of the file: Err(ENXIO) (SIGBUS).
    fn get_page(&self, index: u64) -> Result<usize> {
        Err(ENODEV)
    }
    /// A shared mapping is about to write to page `index` (mark it dirty).
    fn page_mkwrite(&self, index: u64) {}
    fn mmap_writable(&self) -> bool {
        true
    }
    fn fallocate(&self, mode: u32, off: u64, len: u64) -> Result<()> {
        Err(EOPNOTSUPP)
    }

    // directories
    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>> {
        Err(ENOTDIR)
    }
    /// Creates a regular file, directory, device node, FIFO or socket (by the type bits of `mode`).
    fn create(&self, name: &[u8], mode: u32, rdev: u64, uid: u32, gid: u32) -> Result<Arc<dyn Inode>> {
        Err(ENOTDIR)
    }
    fn symlink(&self, name: &[u8], target: &[u8], uid: u32, gid: u32) -> Result<Arc<dyn Inode>> {
        Err(ENOTDIR)
    }
    fn link(&self, name: &[u8], target: &Arc<dyn Inode>) -> Result<()> {
        Err(ENOTDIR)
    }
    fn unlink(&self, name: &[u8]) -> Result<()> {
        Err(ENOTDIR)
    }
    fn rmdir(&self, name: &[u8]) -> Result<()> {
        Err(ENOTDIR)
    }
    /// Moves `old` in this directory to `new` in `newdir` (same file system), replacing a
    /// compatible existing entry.
    fn rename(&self, old: &[u8], newdir: &Arc<dyn Inode>, new: &[u8], flags: u32) -> Result<()> {
        Err(ENOTDIR)
    }
    /// Calls `f(name, ino, d_type, next_pos)` for the entries from position `pos` on, until it
    /// returns false. "." and ".." are included.
    fn readdir(&self, pos: u64, f: &mut dyn FnMut(&[u8], u64, u8, u64) -> bool) -> Result<()> {
        Err(ENOTDIR)
    }
    fn readlink(&self) -> Result<Vec<u8>> {
        Err(EINVAL)
    }

    /// Files with their own behaviour when opened (devices, FIFOs, /proc files): the operations.
    fn open(&self, me: &Arc<dyn Inode>, flags: u32) -> Option<Result<Arc<dyn FileOps>>> {
        None
    }
    fn fs(&self) -> Option<Arc<dyn FileSystem>> {
        None
    }
    fn as_any(&self) -> &dyn Any;
}

pub fn is_dir(i: &Arc<dyn Inode>) -> bool {
    i.stat().map(|s| s.mode & S_IFMT == S_IFDIR).unwrap_or(false)
}

/// Pages of a file in memory: index -> physical page (the cache holds one reference).
pub struct PageCache {
    pub pages: SpinLock<BTreeMap<u64, usize>>,
}

impl Default for PageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PageCache {
    pub const fn new() -> Self {
        PageCache { pages: SpinLock::new(BTreeMap::new()) }
    }

    pub fn lookup(&self, idx: u64) -> Option<usize> {
        self.pages.lock().get(&idx).copied()
    }

    /// The page at `idx`, filling a new one with `fill` when missing. Returns the page without
    /// taking an extra reference.
    pub fn get_or_fill(&self, idx: u64, fill: impl FnOnce(&mut [u8]) -> Result<()>) -> Result<usize> {
        if let Some(p) = self.lookup(idx) {
            return Ok(p);
        }
        let pa = frame::alloc_pages(0).ok_or(ENOMEM)?;
        let k = unsafe { core::slice::from_raw_parts_mut(crate::arch::phys_to_virt(pa) as *mut u8, PAGE_SIZE) };
        if let Err(e) = fill(k) {
            frame::put_page(pa);
            return Err(e);
        }
        let mut m = self.pages.lock();
        if let Some(&p) = m.get(&idx) {
            drop(m);
            frame::put_page(pa);
            return Ok(p);
        }
        m.insert(idx, pa);
        Ok(pa)
    }

    /// Drops every page from `first` on.
    pub fn truncate_from(&self, first: u64) {
        let gone: Vec<usize> = {
            let mut m = self.pages.lock();
            let tail = m.split_off(&first);
            tail.into_values().collect()
        };
        for pa in gone {
            frame::put_page(pa);
        }
    }
}

impl Drop for PageCache {
    fn drop(&mut self) {
        self.truncate_from(0);
    }
}

pub fn page_slice(pa: usize) -> &'static mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(crate::arch::phys_to_virt(pa) as *mut u8, PAGE_SIZE) }
}

/// Metadata of an inode that lives in memory (tmpfs, devtmpfs, proc, pipes, sockets).
#[derive(Clone, Copy)]
pub struct MemMeta {
    pub ino: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub rdev: u64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
}

static NEXT_INO: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(100);

pub fn new_ino() -> u64 {
    NEXT_INO.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

impl MemMeta {
    pub fn new(mode: u32, uid: u32, gid: u32) -> MemMeta {
        let now = Timespec::now();
        MemMeta { ino: new_ino(), mode, uid, gid, nlink: 1, rdev: 0, atime: now, mtime: now, ctime: now }
    }

    pub fn stat(&self, dev: u64, size: u64) -> Stat {
        Stat {
            dev,
            ino: self.ino,
            mode: self.mode,
            nlink: self.nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: self.rdev,
            size,
            blksize: 4096,
            blocks: size.div_ceil(512),
            atime: self.atime,
            mtime: self.mtime,
            ctime: self.ctime,
            btime: self.ctime,
        }
    }

    pub fn apply(&mut self, a: &SetAttr) {
        if let Some(m) = a.mode {
            self.mode = (self.mode & S_IFMT) | (m & 0o7777);
        }
        if let Some(u) = a.uid {
            self.uid = u;
        }
        if let Some(g) = a.gid {
            self.gid = g;
        }
        if let Some(t) = a.atime {
            self.atime = t;
        }
        if let Some(t) = a.mtime {
            self.mtime = t;
        }
        self.ctime = Timespec::now();
    }
}

pub fn makedev(major: u32, minor: u32) -> u64 {
    // glibc's encoding (include/bits/sysmacros.h)
    let (ma, mi) = (major as u64, minor as u64);
    ((ma & 0xfffff000) << 32) | ((ma & 0xfff) << 8) | ((mi & 0xffffff00) << 12) | (mi & 0xff)
}

pub fn major(dev: u64) -> u32 {
    (((dev >> 32) & 0xfffff000) | ((dev >> 8) & 0xfff)) as u32
}

pub fn minor(dev: u64) -> u32 {
    (((dev >> 12) & 0xffffff00) | (dev & 0xff)) as u32
}
