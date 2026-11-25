//! File system types that can be mounted, and syncing them all.
pub mod bufcache;
pub mod crc32c;
pub mod ext4;
pub mod procfs;
pub mod sysfs;
use crate::errno::*;
use crate::vfs::FileSystem;
use alloc::sync::Arc;

/// mount(2): a new instance of the file system `fstype` on `source`.
pub fn make_fs(fstype: &[u8], _source: &[u8], _data: &[u8], _ro: bool) -> Result<Arc<dyn FileSystem>> {
    match fstype {
        b"tmpfs" | b"ramfs" => Ok(crate::vfs::tmpfs::TmpFs::new("tmpfs", 0o1777)),
        b"proc" => Ok(procfs::new()),
        b"sysfs" => Ok(sysfs::new()),
        b"devtmpfs" => Ok(crate::vfs::dev::DEVTMPFS.get().ok_or(ENODEV)?.clone()),
        b"ext4" | b"ext3" | b"ext2" => {
            let (dev, devno) = block_dev_by_path(_source)?;
            Ok(ext4::Ext4::mount(dev, devno, _ro)?)
        }
        _ => Err(ENODEV),
    }
}

pub fn sync_all() {}

/// The block device a path names (/dev/vda...).
fn block_dev_by_path(p: &[u8]) -> Result<(Arc<dyn crate::block::BlockDevice>, u64)> {
    let path = crate::syscall::fs::lookup_at(-100, p, true, false)?;
    let st = path.inode()?.stat()?;
    if st.mode & crate::vfs::S_IFMT != crate::vfs::S_IFBLK {
        return Err(ENOTBLK);
    }
    Ok((crate::vfs::dev::block_device(st.rdev).ok_or(ENXIO)?, st.rdev))
}

/// Mounts root= (a /dev/vdX name, ext4) as the root of the mount tree.
pub fn mount_root() -> Result<Arc<crate::vfs::Mount>> {
    let name = crate::kernel_param("root").ok_or(ENODEV)?;
    let dev = name.strip_prefix("/dev/").ok_or(ENODEV)?;
    let i = crate::vfs::FileSystem::root(&**crate::vfs::dev::DEVTMPFS).lookup(dev.as_bytes())?;
    let st = i.stat()?;
    let bdev = crate::vfs::dev::block_device(st.rdev).ok_or(ENXIO)?;
    let ro = crate::kernel_param("rw").is_none();
    let fs = ext4::Ext4::mount(bdev, st.rdev, ro)?;
    let flags = if ro { crate::vfs::path::MNT_RDONLY } else { 0 };
    if crate::kernel_param("quiet").is_none() {
        println!("ruxos: root {} (ext4{})", name, if fs.ro { ", read-only" } else { "" });
    }
    Ok(crate::vfs::Mount::new(crate::vfs::path::DEntry::new_root(fs), flags, name, "ext4", if ro { "ro" } else { "rw" }))
}
