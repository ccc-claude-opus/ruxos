//! Shared anonymous memory (MAP_SHARED|MAP_ANONYMOUS, /dev/zero shared): an unnamed tmpfs file.
use super::tmpfs::TmpFs;
use super::*;

static SHM: crate::sync::Once<Arc<TmpFs>> = crate::sync::Once::new();

pub fn new(size: u64) -> Result<Arc<dyn Inode>> {
    if SHM.get().is_none() {
        SHM.set(TmpFs::new("shmem", 0o1777));
    }
    let root = SHM.get().unwrap().root();
    let name = alloc::format!("anon-{}", new_ino());
    let i = root.create(name.as_bytes(), S_IFREG | 0o600, 0, 0, 0)?;
    root.unlink(name.as_bytes())?;
    i.setattr(&SetAttr { size: Some(size), ..Default::default() })?;
    Ok(i)
}
