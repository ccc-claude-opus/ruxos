//! Block devices: the driver interface, a buffer cache of 4 KB blocks (shared by mounted file
//! systems and /dev/vdX), and the file operations of block device files.
use crate::errno::*;
use crate::vfs::file::{File, FileOps, SEEK_CUR, SEEK_END, SEEK_SET};
use crate::vfs::Inode;
use alloc::sync::Arc;
use core::any::Any;

pub trait BlockDevice: Send + Sync {
    /// Size in 512-byte sectors.
    fn sectors(&self) -> u64;
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<()>;
    fn write(&self, sector: u64, buf: &[u8]) -> Result<()>;
    fn flush(&self) -> Result<()>;
    fn readonly(&self) -> bool {
        false
    }
    /// Reads into (physical address, length) buffers (page cache pages).
    fn read_phys(&self, sector: u64, pages: &[(usize, usize)]) -> Result<()> {
        let mut s = sector;
        for &(pa, len) in pages {
            let b = unsafe { core::slice::from_raw_parts_mut(crate::arch::phys_to_virt(pa) as *mut u8, len) };
            self.read(s, b)?;
            s += len as u64 / 512;
        }
        Ok(())
    }
    fn write_phys(&self, sector: u64, pages: &[(usize, usize)]) -> Result<()> {
        let mut s = sector;
        for &(pa, len) in pages {
            let b = unsafe { core::slice::from_raw_parts(crate::arch::phys_to_virt(pa) as *const u8, len) };
            self.write(s, b)?;
            s += len as u64 / 512;
        }
        Ok(())
    }
}

/// /dev/vdX opened directly (mkfs, fsck, blkid): byte-addressed reads and writes.
pub struct BlockFile {
    pub dev: Arc<dyn BlockDevice>,
    pub inode: Arc<dyn Inode>,
}

impl BlockFile {
    fn rw(&self, off: u64, buf: &mut [u8], write: Option<&[u8]>) -> Result<usize> {
        let size = self.dev.sectors() * 512;
        if off >= size {
            return Ok(0);
        }
        let n = (buf.len() as u64).min(size - off) as usize;
        let mut done = 0;
        let mut tmp = alloc::vec![0u8; 4096];
        while done < n {
            let p = off + done as u64;
            let sec = p / 512;
            let in_sec = (p % 512) as usize;
            let chunk = if in_sec == 0 && (n - done) >= 512 { ((n - done) / 512 * 512).min(65536) } else { (512 - in_sec).min(n - done) };
            if in_sec == 0 && chunk % 512 == 0 {
                match write {
                    None => self.dev.read(sec, &mut buf[done..done + chunk])?,
                    Some(w) => self.dev.write(sec, &w[done..done + chunk])?,
                }
            } else {
                let s = &mut tmp[..512];
                self.dev.read(sec, s)?;
                match write {
                    None => buf[done..done + chunk].copy_from_slice(&s[in_sec..in_sec + chunk]),
                    Some(w) => {
                        s[in_sec..in_sec + chunk].copy_from_slice(&w[done..done + chunk]);
                        self.dev.write(sec, s)?;
                    }
                }
            }
            done += chunk;
        }
        Ok(n)
    }
}

impl FileOps for BlockFile {
    fn read(&self, f: &File, buf: &mut [u8]) -> Result<usize> {
        let mut pos = f.pos.lock();
        let n = self.rw(*pos, buf, None)?;
        *pos += n as u64;
        Ok(n)
    }
    fn write(&self, f: &File, buf: &[u8]) -> Result<usize> {
        let mut pos = f.pos.lock();
        let mut dummy = alloc::vec![0u8; buf.len()];
        let n = self.rw(*pos, &mut dummy, Some(buf))?;
        *pos += n as u64;
        Ok(n)
    }
    fn pread(&self, _f: &File, off: u64, buf: &mut [u8]) -> Result<usize> {
        self.rw(off, buf, None)
    }
    fn pwrite(&self, _f: &File, off: u64, buf: &[u8]) -> Result<usize> {
        let mut dummy = alloc::vec![0u8; buf.len()];
        self.rw(off, &mut dummy, Some(buf))
    }
    fn seek(&self, f: &File, off: i64, whence: u32) -> Result<u64> {
        let mut pos = f.pos.lock();
        let size = (self.dev.sectors() * 512) as i64;
        let new = match whence {
            SEEK_SET => off,
            SEEK_CUR => *pos as i64 + off,
            SEEK_END => size + off,
            _ => return Err(EINVAL),
        };
        if new < 0 {
            return Err(EINVAL);
        }
        *pos = new as u64;
        Ok(*pos)
    }
    fn ioctl(&self, _f: &File, cmd: u32, arg: usize) -> Result<isize> {
        use crate::mm::vm::write_user;
        match cmd {
            0x8008_1272 => write_user(arg, &(self.dev.sectors() * 512))?, // BLKGETSIZE64
            0x1260 => write_user(arg, &(self.dev.sectors() as usize))?,   // BLKGETSIZE
            0x1268 | 0x127b => write_user(arg, &512i32)?,                  // BLKSSZGET, BLKPBSZGET
            0x1270 => write_user(arg, &4096i32)?,                          // BLKBSZGET
            0x125e => write_user(arg, &(self.dev.readonly() as i32))?,     // BLKROGET
            0x1261 => self.dev.flush()?,                                   // BLKFLSBUF
            0x1279 | 0x127a | 0x1278 => write_user(arg, &0i32)?,           // BLKIOMIN/OPT, ALIGNOFF
            0x127c => write_user(arg, &0u64)?,                             // BLKDISCARDZEROES
            _ => return Err(ENOTTY),
        }
        Ok(0)
    }
    fn inode(&self) -> Option<Arc<dyn Inode>> {
        Some(self.inode.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
