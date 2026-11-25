//! Building blocks of /proc and /sys: directories listed on demand, files generated when opened
//! (a snapshot, like seq_file), optional write handlers, and symbolic links computed on demand.
use super::file::{File, FileOps};
use super::*;
use alloc::boxed::Box;

pub type Lister = Box<dyn Fn() -> Vec<(Vec<u8>, Arc<dyn Inode>)> + Send + Sync>;
pub type Gen = Box<dyn Fn() -> Result<Vec<u8>> + Send + Sync>;
pub type Writer = Box<dyn Fn(&[u8]) -> Result<()> + Send + Sync>;

pub struct PDir {
    ino: u64,
    mode: u32,
    dev: u64,
    list: Lister,
}
pub struct PFile {
    ino: u64,
    mode: u32,
    dev: u64,
    generate: Gen,
    write: Option<Writer>,
}
pub struct PLink {
    ino: u64,
    dev: u64,
    target: Gen,
}

/// Stable inode numbers from a name path.
pub fn ino_of(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for &b in p.iter() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h ^= b'/' as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    (h >> 1) | 1
}

pub fn dir(dev: u64, ino: u64, mode: u32, list: Lister) -> Arc<dyn Inode> {
    Arc::new(PDir { ino, mode: S_IFDIR | mode, dev, list })
}
pub fn file(dev: u64, ino: u64, mode: u32, generate: Gen, write: Option<Writer>) -> Arc<dyn Inode> {
    Arc::new(PFile { ino, mode: S_IFREG | mode, dev, generate, write })
}
pub fn link(dev: u64, ino: u64, target: Gen) -> Arc<dyn Inode> {
    Arc::new(PLink { ino, dev, target })
}
pub fn text(s: alloc::string::String) -> Result<Vec<u8>> {
    Ok(s.into_bytes())
}

fn meta_stat(dev: u64, ino: u64, mode: u32, size: u64) -> Stat {
    let now = Timespec::now();
    Stat { dev, ino, mode, nlink: 1, uid: 0, gid: 0, rdev: 0, size, blksize: 4096, blocks: 0, atime: now, mtime: now, ctime: now, btime: now }
}

impl Inode for PDir {
    fn stat(&self) -> Result<Stat> {
        let mut s = meta_stat(self.dev, self.ino, self.mode, 0);
        s.nlink = 2;
        Ok(s)
    }
    fn setattr(&self, _a: &SetAttr) -> Result<()> {
        Ok(())
    }
    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>> {
        (self.list)().into_iter().find(|(n, _)| n == name).map(|(_, i)| i).ok_or(ENOENT)
    }
    fn readdir(&self, pos: u64, f: &mut dyn FnMut(&[u8], u64, u8, u64) -> bool) -> Result<()> {
        let mut all: Vec<(Vec<u8>, u64, u8)> = alloc::vec![(b".".to_vec(), self.ino, DT_DIR), (b"..".to_vec(), 1, DT_DIR)];
        for (n, i) in (self.list)() {
            let st = i.stat()?;
            all.push((n, st.ino, dtype(st.mode)));
        }
        for (k, (n, ino, dt)) in all.iter().enumerate().skip(pos as usize) {
            if !f(n, *ino, *dt, k as u64 + 1) {
                break;
            }
        }
        Ok(())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Inode for PFile {
    fn stat(&self) -> Result<Stat> {
        Ok(meta_stat(self.dev, self.ino, self.mode, 0))
    }
    fn setattr(&self, a: &SetAttr) -> Result<()> {
        if a.size.is_some() && self.write.is_none() {
            return Err(EACCES);
        }
        Ok(())
    }
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize> {
        let d = (self.generate)()?;
        if off as usize >= d.len() {
            return Ok(0);
        }
        let n = buf.len().min(d.len() - off as usize);
        buf[..n].copy_from_slice(&d[off as usize..off as usize + n]);
        Ok(n)
    }
    fn write_at(&self, _off: u64, data: &[u8]) -> Result<usize> {
        match &self.write {
            Some(w) => {
                w(data)?;
                Ok(data.len())
            }
            None => Err(EACCES),
        }
    }
    fn open(&self, _me: &Arc<dyn Inode>, _flags: u32) -> Option<Result<Arc<dyn FileOps>>> {
        // the snapshot is taken at open time
        Some((self.generate)().map(|d| Arc::new(Snapshot { data: d, me: _me.clone() }) as Arc<dyn FileOps>))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct Snapshot {
    data: Vec<u8>,
    me: Arc<dyn Inode>,
}

impl FileOps for Snapshot {
    fn read(&self, f: &File, buf: &mut [u8]) -> Result<usize> {
        let mut pos = f.pos.lock();
        let p = (*pos as usize).min(self.data.len());
        let n = buf.len().min(self.data.len() - p);
        buf[..n].copy_from_slice(&self.data[p..p + n]);
        *pos += n as u64;
        Ok(n)
    }
    fn pread(&self, _f: &File, off: u64, buf: &mut [u8]) -> Result<usize> {
        let p = (off as usize).min(self.data.len());
        let n = buf.len().min(self.data.len() - p);
        buf[..n].copy_from_slice(&self.data[p..p + n]);
        Ok(n)
    }
    fn write(&self, _f: &File, buf: &[u8]) -> Result<usize> {
        self.me.write_at(0, buf)
    }
    fn pwrite(&self, _f: &File, _off: u64, buf: &[u8]) -> Result<usize> {
        self.me.write_at(0, buf)
    }
    fn seek(&self, f: &File, off: i64, whence: u32) -> Result<u64> {
        let mut pos = f.pos.lock();
        let new = match whence {
            0 => off,
            1 => *pos as i64 + off,
            2 => self.data.len() as i64 + off,
            _ => return Err(EINVAL),
        };
        if new < 0 {
            return Err(EINVAL);
        }
        *pos = new as u64;
        Ok(*pos)
    }
    fn inode(&self) -> Option<Arc<dyn Inode>> {
        Some(self.me.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Inode for PLink {
    fn stat(&self) -> Result<Stat> {
        Ok(meta_stat(self.dev, self.ino, S_IFLNK | 0o777, 0))
    }
    fn readlink(&self) -> Result<Vec<u8>> {
        (self.target)()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A pseudo file system whose root is a lister.
pub struct PseudoFs {
    pub name: &'static str,
    pub dev: u64,
    pub magic: u64,
    pub root: crate::sync::SpinLock<Option<Arc<dyn Inode>>>,
}

impl FileSystem for PseudoFs {
    fn name(&self) -> &'static str {
        self.name
    }
    fn root(&self) -> Arc<dyn Inode> {
        self.root.lock().clone().unwrap()
    }
    fn dev(&self) -> u64 {
        self.dev
    }
    fn statfs(&self) -> StatFs {
        StatFs { fs_type: self.magic, bsize: 4096, namelen: 255, ..Default::default() }
    }
    fn cache_dentries(&self) -> bool {
        false
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
