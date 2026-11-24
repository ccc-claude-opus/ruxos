//! File system types that can be mounted, and syncing them all.
use crate::errno::*;
use crate::vfs::FileSystem;
use alloc::sync::Arc;

/// mount(2): a new instance of the file system `fstype` on `source`.
pub fn make_fs(fstype: &[u8], _source: &[u8], _data: &[u8], _ro: bool) -> Result<Arc<dyn FileSystem>> {
    match fstype {
        b"tmpfs" | b"ramfs" => Ok(crate::vfs::tmpfs::TmpFs::new("tmpfs", 0o1777)),
        b"devtmpfs" => Ok(crate::vfs::dev::DEVTMPFS.get().ok_or(ENODEV)?.clone()),
        _ => Err(ENODEV),
    }
}

pub fn sync_all() {}

/// Mounts root= (later milestones: ext4 on a virtio disk).
pub fn mount_root() -> Result<Arc<crate::vfs::Mount>> {
    Err(ENODEV)
}
