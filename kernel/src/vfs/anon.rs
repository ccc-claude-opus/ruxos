//! Inodes for objects that are not in any directory: pipes (S_IFIFO), sockets (S_IFSOCK) and
//! anon_inode files (eventfd, epoll, signalfd, timerfd, pidfd: no type bits), so fstat works.
use super::*;

pub struct AnonInode {
    meta: SpinLock<MemMeta>,
}

static ANON_DEV: crate::sync::Once<u64> = crate::sync::Once::new();

fn dev() -> u64 {
    if ANON_DEV.get().is_none() {
        ANON_DEV.set(super::tmpfs::anon_dev());
    }
    *ANON_DEV
}

impl AnonInode {
    pub fn new(mode: u32) -> Arc<dyn Inode> {
        let cred = crate::task::try_current_thread().map(|t| t.proc.cred());
        let (u, g) = cred.map_or((0, 0), |c| (c.fsuid, c.fsgid));
        Arc::new(AnonInode { meta: SpinLock::new(MemMeta::new(mode, u, g)) })
    }
}

impl Inode for AnonInode {
    fn stat(&self) -> Result<Stat> {
        let mut s = self.meta.lock().stat(dev(), 0);
        s.blksize = 4096;
        Ok(s)
    }
    fn setattr(&self, a: &SetAttr) -> Result<()> {
        self.meta.lock().apply(a);
        Ok(())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
