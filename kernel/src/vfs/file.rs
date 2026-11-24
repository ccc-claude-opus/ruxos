//! Open files, their operations, and descriptor tables.
use super::poll::{PollQueue, POLLIN, POLLOUT};
use super::{Inode, PathRef, S_IFDIR, S_IFMT, S_IFREG};
use crate::errno::*;
use crate::sched::Mutex;
use crate::sync::SpinLock;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU32, Ordering};

pub const O_ACCMODE: u32 = 3;
pub const O_RDONLY: u32 = 0;
pub const O_WRONLY: u32 = 1;
pub const O_RDWR: u32 = 2;
pub const O_CREAT: u32 = 0o100;
pub const O_EXCL: u32 = 0o200;
pub const O_NOCTTY: u32 = 0o400;
pub const O_TRUNC: u32 = 0o1000;
pub const O_APPEND: u32 = 0o2000;
pub const O_NONBLOCK: u32 = 0o4000;
pub const O_DSYNC: u32 = 0o10000;
pub const O_ASYNC: u32 = 0o20000;
#[cfg(target_arch = "aarch64")]
pub const O_DIRECTORY: u32 = 0o40000;
#[cfg(target_arch = "aarch64")]
pub const O_NOFOLLOW: u32 = 0o100000;
#[cfg(target_arch = "aarch64")]
pub const O_DIRECT: u32 = 0o200000;
#[cfg(target_arch = "aarch64")]
pub const O_LARGEFILE: u32 = 0o400000;
#[cfg(not(target_arch = "aarch64"))]
pub const O_DIRECTORY: u32 = 0o200000;
#[cfg(not(target_arch = "aarch64"))]
pub const O_NOFOLLOW: u32 = 0o400000;
#[cfg(not(target_arch = "aarch64"))]
pub const O_DIRECT: u32 = 0o40000;
#[cfg(not(target_arch = "aarch64"))]
pub const O_LARGEFILE: u32 = 0o100000;
pub const O_NOATIME: u32 = 0o1000000;
pub const O_CLOEXEC: u32 = 0o2000000;
pub const O_SYNC: u32 = 0o4010000;
pub const O_PATH: u32 = 0o10000000;
pub const O_TMPFILE: u32 = 0o20000000;

/// Flags F_SETFL may change.
pub const SETFL_MASK: u32 = O_APPEND | O_NONBLOCK | O_ASYNC | O_DIRECT | O_NOATIME;

pub const SEEK_SET: u32 = 0;
pub const SEEK_CUR: u32 = 1;
pub const SEEK_END: u32 = 2;
pub const SEEK_DATA: u32 = 3;
pub const SEEK_HOLE: u32 = 4;

#[allow(unused_variables)]
pub trait FileOps: Send + Sync + Any {
    fn read(&self, f: &File, buf: &mut [u8]) -> Result<usize> {
        Err(EINVAL)
    }
    fn write(&self, f: &File, buf: &[u8]) -> Result<usize> {
        Err(EINVAL)
    }
    fn pread(&self, f: &File, off: u64, buf: &mut [u8]) -> Result<usize> {
        Err(ESPIPE)
    }
    fn pwrite(&self, f: &File, off: u64, buf: &[u8]) -> Result<usize> {
        Err(ESPIPE)
    }
    fn seek(&self, f: &File, off: i64, whence: u32) -> Result<u64> {
        Err(ESPIPE)
    }
    fn poll(&self, f: &File) -> u32 {
        POLLIN | POLLOUT
    }
    fn poll_queue(&self) -> Option<&PollQueue> {
        None
    }
    fn ioctl(&self, f: &File, cmd: u32, arg: usize) -> Result<isize> {
        Err(ENOTTY)
    }
    /// The inode behind the file (fstat, mmap, fsync...).
    fn inode(&self) -> Option<Arc<dyn Inode>> {
        None
    }
    fn as_any(&self) -> &dyn Any;
}

pub struct File {
    pub ops: Arc<dyn FileOps>,
    /// O_ACCMODE bits and the status flags (O_APPEND, O_NONBLOCK, O_PATH...).
    pub flags: AtomicU32,
    pub pos: Mutex<u64>,
    pub path: Option<PathRef>,
    /// The inode it was opened from (device files: the device node).
    pub node: Option<Arc<dyn Inode>>,
}

impl File {
    pub fn new(ops: Arc<dyn FileOps>, flags: u32, path: Option<PathRef>) -> Arc<File> {
        let node = path.as_ref().and_then(|p| p.inode().ok());
        Self::with_node(ops, flags, path, node)
    }
    pub fn with_node(ops: Arc<dyn FileOps>, flags: u32, path: Option<PathRef>, node: Option<Arc<dyn Inode>>) -> Arc<File> {
        Arc::new(File { ops, flags: AtomicU32::new(flags & !(O_CREAT | O_EXCL | O_NOCTTY | O_TRUNC | O_CLOEXEC)), pos: Mutex::new(0), path, node })
    }
    pub fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }
    pub fn nonblock(&self) -> bool {
        self.flags() & O_NONBLOCK != 0
    }
    pub fn readable(&self) -> bool {
        let f = self.flags();
        f & O_PATH == 0 && f & O_ACCMODE != O_WRONLY
    }
    pub fn writable(&self) -> bool {
        let f = self.flags();
        f & O_PATH == 0 && f & O_ACCMODE != O_RDONLY
    }
    pub fn inode(&self) -> Option<Arc<dyn Inode>> {
        self.ops.inode().or_else(|| self.node.clone())
    }
    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        self.ops.read(self, buf)
    }
    pub fn write(&self, buf: &[u8]) -> Result<usize> {
        self.ops.write(self, buf)
    }
}

/// Regular files: reads and writes at the file position through the inode.
pub struct RegFile {
    pub inode: Arc<dyn Inode>,
}

impl FileOps for RegFile {
    fn read(&self, f: &File, buf: &mut [u8]) -> Result<usize> {
        let mut pos = f.pos.lock();
        let n = self.inode.read_at(*pos, buf)?;
        *pos += n as u64;
        Ok(n)
    }
    fn write(&self, f: &File, buf: &[u8]) -> Result<usize> {
        let mut pos = f.pos.lock();
        if f.flags() & O_APPEND != 0 {
            *pos = self.inode.stat()?.size;
        }
        let n = self.inode.write_at(*pos, buf)?;
        *pos += n as u64;
        Ok(n)
    }
    fn pread(&self, _f: &File, off: u64, buf: &mut [u8]) -> Result<usize> {
        self.inode.read_at(off, buf)
    }
    fn pwrite(&self, f: &File, off: u64, buf: &[u8]) -> Result<usize> {
        // POSIX says pwrite ignores O_APPEND; Linux appends anyway
        let off = if f.flags() & O_APPEND != 0 { self.inode.stat()?.size } else { off };
        self.inode.write_at(off, buf)
    }
    fn seek(&self, f: &File, off: i64, whence: u32) -> Result<u64> {
        let mut pos = f.pos.lock();
        let size = self.inode.stat()?.size as i64;
        let new = match whence {
            SEEK_SET => off,
            SEEK_CUR => *pos as i64 + off,
            SEEK_END => size + off,
            SEEK_DATA => {
                if off >= size {
                    return Err(ENXIO);
                }
                off
            }
            SEEK_HOLE => {
                if off >= size {
                    return Err(ENXIO);
                }
                size
            }
            _ => return Err(EINVAL),
        };
        if new < 0 {
            return Err(EINVAL);
        }
        *pos = new as u64;
        Ok(new as u64)
    }
    fn ioctl(&self, _f: &File, cmd: u32, arg: usize) -> Result<isize> {
        match cmd {
            0x541b => {
                // FIONREAD
                let size = self.inode.stat()?.size;
                let pos = *_f.pos.lock();
                crate::mm::vm::write_user(arg, &(size.saturating_sub(pos) as i32))?;
                Ok(0)
            }
            _ => Err(ENOTTY),
        }
    }
    fn inode(&self) -> Option<Arc<dyn Inode>> {
        Some(self.inode.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Directories (and O_PATH descriptors): no read/write; getdents uses the inode.
pub struct DirFile {
    pub inode: Arc<dyn Inode>,
}

impl FileOps for DirFile {
    fn read(&self, _f: &File, _b: &mut [u8]) -> Result<usize> {
        Err(EISDIR)
    }
    fn write(&self, _f: &File, _b: &[u8]) -> Result<usize> {
        Err(EISDIR)
    }
    fn seek(&self, f: &File, off: i64, whence: u32) -> Result<u64> {
        let mut pos = f.pos.lock();
        match whence {
            SEEK_SET if off >= 0 => *pos = off as u64,
            SEEK_CUR if *pos as i64 + off >= 0 => *pos = (*pos as i64 + off) as u64,
            _ => return Err(EINVAL),
        }
        Ok(*pos)
    }
    fn inode(&self) -> Option<Arc<dyn Inode>> {
        Some(self.inode.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The operations for an inode being opened.
pub fn ops_for(inode: &Arc<dyn Inode>, flags: u32) -> Result<Arc<dyn FileOps>> {
    if flags & O_PATH != 0 {
        return Ok(Arc::new(DirFile { inode: inode.clone() }));
    }
    if let Some(r) = inode.open(inode, flags) {
        return r;
    }
    let mode = inode.stat()?.mode & S_IFMT;
    match mode {
        S_IFREG => Ok(Arc::new(RegFile { inode: inode.clone() })),
        S_IFDIR => Ok(Arc::new(DirFile { inode: inode.clone() })),
        super::S_IFCHR | super::S_IFBLK => super::dev::open_device(inode, flags),
        super::S_IFIFO => super::pipe::open_fifo(inode, flags),
        _ => Err(ENXIO),
    }
}

pub const FD_CLOEXEC: u32 = 1;

#[derive(Clone)]
pub struct FdEntry {
    pub file: Arc<File>,
    pub cloexec: bool,
}

#[derive(Clone)]
pub struct FdTable {
    pub fds: Vec<Option<FdEntry>>,
    pub limit: usize,
}

impl Default for FdTable {
    fn default() -> Self {
        Self::new()
    }
}

impl FdTable {
    pub fn new() -> FdTable {
        FdTable { fds: Vec::new(), limit: 1024 }
    }

    pub fn get(&self, fd: i32) -> Result<Arc<File>> {
        if fd < 0 {
            return Err(EBADF);
        }
        self.fds.get(fd as usize).and_then(|e| e.as_ref()).map(|e| e.file.clone()).ok_or(EBADF)
    }

    /// The lowest free descriptor >= min.
    pub fn alloc(&mut self, file: Arc<File>, min: usize, cloexec: bool) -> Result<i32> {
        let mut i = min;
        while i < self.fds.len() && self.fds[i].is_some() {
            i += 1;
        }
        if i >= self.limit {
            return Err(EMFILE);
        }
        if i >= self.fds.len() {
            self.fds.resize(i + 1, None);
        }
        self.fds[i] = Some(FdEntry { file, cloexec });
        Ok(i as i32)
    }

    pub fn set(&mut self, fd: usize, file: Arc<File>, cloexec: bool) -> Option<Arc<File>> {
        if fd >= self.fds.len() {
            self.fds.resize(fd + 1, None);
        }
        self.fds[fd].replace(FdEntry { file, cloexec }).map(|e| e.file)
    }

    pub fn close(&mut self, fd: i32) -> Result<Arc<File>> {
        if fd < 0 {
            return Err(EBADF);
        }
        let e = self.fds.get_mut(fd as usize).and_then(|e| e.take()).ok_or(EBADF)?;
        Ok(e.file)
    }

    pub fn close_on_exec(&mut self) {
        for e in self.fds.iter_mut() {
            if e.as_ref().is_some_and(|x| x.cloexec) {
                *e = None;
            }
        }
    }

    pub fn cloexec(&self, fd: i32) -> Result<bool> {
        self.fds.get(fd as usize).and_then(|e| e.as_ref()).map(|e| e.cloexec).ok_or(EBADF)
    }

    pub fn set_cloexec(&mut self, fd: i32, on: bool) -> Result<()> {
        let e = self.fds.get_mut(fd as usize).and_then(|e| e.as_mut()).ok_or(EBADF)?;
        e.cloexec = on;
        Ok(())
    }
}

/// Shared by threads (CLONE_FILES).
pub type SharedFds = Arc<SpinLock<FdTable>>;

pub fn is_dir_mode(mode: u32) -> bool {
    mode & S_IFMT == S_IFDIR
}

impl Drop for File {
    fn drop(&mut self) {
        if let Some(i) = self.ops.inode() {
            crate::locks::on_file_drop(self as *const File as usize, Arc::as_ptr(&i) as *const u8 as usize);
        }
    }
}
