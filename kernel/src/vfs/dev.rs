//! Device files: the memory devices (null, zero, full, random, urandom), the console tty, block
//! devices (registered by drivers), and devtmpfs.
use super::file::{File, FileOps};
use super::tmpfs::TmpFs;
use super::*;
use crate::sync::Once;

pub static DEVTMPFS: Once<Arc<TmpFs>> = Once::new();

/// Character devices by (major, minor), registered by drivers.
pub type ChrOpen = fn(minor: u32, flags: u32) -> Result<Arc<dyn FileOps>>;
static CHR: SpinLock<BTreeMap<u32, ChrOpen>> = SpinLock::new(BTreeMap::new());
/// Block devices by device number.
static BLK: SpinLock<BTreeMap<u64, Arc<dyn crate::block::BlockDevice>>> = SpinLock::new(BTreeMap::new());

pub fn register_chr(major: u32, open: ChrOpen) {
    CHR.lock().insert(major, open);
}

pub fn register_blk(dev: u64, name: &str, b: Arc<dyn crate::block::BlockDevice>) {
    BLK.lock().insert(dev, b);
    add_node(name.as_bytes(), S_IFBLK | 0o660, dev);
}

pub fn block_device(dev: u64) -> Option<Arc<dyn crate::block::BlockDevice>> {
    BLK.lock().get(&dev).cloned()
}

pub fn init() {
    let fs = TmpFs::new("devtmpfs", 0o755);
    DEVTMPFS.set(fs);
    register_chr(1, open_mem);
    add_node(b"null", S_IFCHR | 0o666, makedev(1, 3));
    add_node(b"zero", S_IFCHR | 0o666, makedev(1, 5));
    add_node(b"full", S_IFCHR | 0o666, makedev(1, 7));
    add_node(b"random", S_IFCHR | 0o666, makedev(1, 8));
    add_node(b"urandom", S_IFCHR | 0o666, makedev(1, 9));
    let root = DEVTMPFS.root();
    let _ = root.create(b"pts", S_IFDIR | 0o755, 0, 0, 0);
    let _ = root.create(b"shm", S_IFDIR | 0o1777, 0, 0, 0);
    let _ = root.symlink(b"fd", b"/proc/self/fd", 0, 0);
    let _ = root.symlink(b"stdin", b"/proc/self/fd/0", 0, 0);
    let _ = root.symlink(b"stdout", b"/proc/self/fd/1", 0, 0);
    let _ = root.symlink(b"stderr", b"/proc/self/fd/2", 0, 0);
}

pub fn add_node(name: &[u8], mode: u32, rdev: u64) {
    if let Some(fs) = DEVTMPFS.get() {
        let _ = fs.root().create(name, mode, rdev, 0, 0);
    }
}

pub fn open_device(inode: &Arc<dyn Inode>, flags: u32) -> Result<Arc<dyn FileOps>> {
    let st = inode.stat()?;
    if st.mode & S_IFMT == S_IFBLK {
        let b = block_device(st.rdev).ok_or(ENXIO)?;
        return Ok(Arc::new(crate::block::BlockFile { dev: b, inode: inode.clone() }));
    }
    let open = CHR.lock().get(&major(st.rdev)).copied().ok_or(ENXIO)?;
    open(minor(st.rdev), flags)
}

struct Mem {
    minor: u32,
}

fn open_mem(minor: u32, _flags: u32) -> Result<Arc<dyn FileOps>> {
    match minor {
        3 | 5 | 7 | 8 | 9 => Ok(Arc::new(Mem { minor })),
        _ => Err(ENXIO),
    }
}

impl FileOps for Mem {
    fn read(&self, _f: &File, buf: &mut [u8]) -> Result<usize> {
        match self.minor {
            3 => Ok(0),
            5 | 7 => {
                buf.fill(0);
                Ok(buf.len())
            }
            _ => {
                crate::random::fill(buf);
                Ok(buf.len())
            }
        }
    }
    fn write(&self, _f: &File, buf: &[u8]) -> Result<usize> {
        match self.minor {
            7 => Err(ENOSPC),
            8 | 9 => {
                crate::random::mix(buf);
                Ok(buf.len())
            }
            _ => Ok(buf.len()),
        }
    }
    fn pread(&self, f: &File, _off: u64, buf: &mut [u8]) -> Result<usize> {
        self.read(f, buf)
    }
    fn pwrite(&self, f: &File, _off: u64, buf: &[u8]) -> Result<usize> {
        self.write(f, buf)
    }
    fn seek(&self, _f: &File, _off: i64, _w: u32) -> Result<u64> {
        Ok(0)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
