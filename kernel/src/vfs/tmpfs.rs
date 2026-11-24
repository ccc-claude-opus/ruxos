//! tmpfs: a file system in memory (also the root of the initramfs and devtmpfs). File data lives in
//! the page cache.
use super::*;
use crate::mm::frame::PAGE_SIZE;
use alloc::sync::Weak;

pub struct TmpFs {
    dev: u64,
    root: SpinLock<Option<Arc<TmpInode>>>,
    name: &'static str,
}

pub struct TmpInode {
    pub meta: SpinLock<MemMeta>,
    fs: Weak<TmpFs>,
    kind: Kind,
}

struct DirData {
    /// name -> (inode, cookie)
    entries: BTreeMap<Vec<u8>, (Arc<dyn Inode>, u64)>,
    by_cookie: BTreeMap<u64, Vec<u8>>,
    next_cookie: u64,
    parent_ino: u64,
}

enum Kind {
    File { cache: PageCache, size: SpinLock<u64> },
    Dir(SpinLock<DirData>),
    Symlink(Vec<u8>),
    /// Device nodes, FIFOs, sockets: no data here.
    Special,
}

static NEXT_DEV: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

pub fn anon_dev() -> u64 {
    // "unnamed" devices: major 0
    makedev(0, NEXT_DEV.fetch_add(1, core::sync::atomic::Ordering::Relaxed))
}

impl TmpFs {
    pub fn new(name: &'static str, mode: u32) -> Arc<TmpFs> {
        let fs = Arc::new(TmpFs { dev: anon_dev(), root: SpinLock::new(None), name });
        let root = TmpInode::new(&fs, S_IFDIR | (mode & 0o7777), 0, 0, 0);
        if let Kind::Dir(d) = &root.kind {
            d.lock().parent_ino = root.meta.lock().ino;
        }
        root.meta.lock().nlink = 2;
        *fs.root.lock() = Some(root);
        fs
    }
}

impl FileSystem for TmpFs {
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
        let total = crate::mm::frame::TOTAL_PAGES.load(core::sync::atomic::Ordering::Relaxed) as u64;
        let free = crate::mm::frame::free_count() as u64;
        StatFs { fs_type: 0x0102_1994, bsize: 4096, blocks: total / 2, bfree: free / 2, bavail: free / 2, files: free, ffree: free, namelen: 255, flags: 0 }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl TmpInode {
    fn new(fs: &Arc<TmpFs>, mode: u32, rdev: u64, uid: u32, gid: u32) -> Arc<TmpInode> {
        let kind = match mode & S_IFMT {
            S_IFREG => Kind::File { cache: PageCache::new(), size: SpinLock::new(0) },
            S_IFDIR => Kind::Dir(SpinLock::new(DirData {
                entries: BTreeMap::new(),
                by_cookie: BTreeMap::new(),
                next_cookie: 2,
                parent_ino: 0,
            })),
            _ => Kind::Special,
        };
        let mut meta = MemMeta::new(mode, uid, gid);
        meta.rdev = rdev;
        if mode & S_IFMT == S_IFDIR {
            meta.nlink = 2;
        }
        Arc::new(TmpInode { meta: SpinLock::new(meta), fs: Arc::downgrade(fs), kind })
    }

    fn dir(&self) -> Result<&SpinLock<DirData>> {
        match &self.kind {
            Kind::Dir(d) => Ok(d),
            _ => Err(ENOTDIR),
        }
    }

    fn tfs(&self) -> Arc<TmpFs> {
        self.fs.upgrade().unwrap()
    }

    fn touch(&self) {
        let now = Timespec::now();
        let mut m = self.meta.lock();
        m.mtime = now;
        m.ctime = now;
    }

    fn add_entry(&self, name: &[u8], inode: Arc<dyn Inode>) -> Result<()> {
        let mut d = self.dir()?.lock();
        if d.entries.contains_key(name) {
            return Err(EEXIST);
        }
        let c = d.next_cookie;
        d.next_cookie += 1;
        d.entries.insert(name.to_vec(), (inode, c));
        d.by_cookie.insert(c, name.to_vec());
        drop(d);
        self.touch();
        Ok(())
    }

    fn remove_entry(&self, name: &[u8]) -> Option<Arc<dyn Inode>> {
        let mut d = self.dir().ok()?.lock();
        let (i, c) = d.entries.remove(name)?;
        d.by_cookie.remove(&c);
        drop(d);
        self.touch();
        Some(i)
    }

    fn size(&self) -> u64 {
        match &self.kind {
            Kind::File { size, .. } => *size.lock(),
            Kind::Symlink(t) => t.len() as u64,
            Kind::Dir(d) => (d.lock().entries.len() as u64 + 2) * 20,
            Kind::Special => 0,
        }
    }

    fn set_size(&self, new: u64) -> Result<()> {
        let Kind::File { cache, size } = &self.kind else { return Err(EINVAL) };
        let mut s = size.lock();
        if new < *s {
            let first_gone = new.div_ceil(PAGE_SIZE as u64);
            cache.truncate_from(first_gone);
            // zero the tail of the last page so a later extension reads zeros
            if new % PAGE_SIZE as u64 != 0 {
                if let Some(pa) = cache.lookup(new / PAGE_SIZE as u64) {
                    page_slice(pa)[(new % PAGE_SIZE as u64) as usize..].fill(0);
                }
            }
        }
        *s = new;
        Ok(())
    }
}

fn as_tmp(i: &Arc<dyn Inode>) -> Option<&TmpInode> {
    i.as_any().downcast_ref::<TmpInode>()
}

impl Inode for TmpInode {
    fn stat(&self) -> Result<Stat> {
        Ok(self.meta.lock().stat(self.tfs().dev, self.size()))
    }

    fn setattr(&self, a: &SetAttr) -> Result<()> {
        if let Some(s) = a.size {
            self.set_size(s)?;
            self.touch();
        }
        self.meta.lock().apply(a);
        Ok(())
    }

    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize> {
        let Kind::File { cache, size } = &self.kind else {
            return Err(if matches!(self.kind, Kind::Dir(_)) { EISDIR } else { EINVAL });
        };
        let size = *size.lock();
        if off >= size {
            return Ok(0);
        }
        let n = buf.len().min((size - off) as usize);
        let mut done = 0;
        while done < n {
            let p = off + done as u64;
            let idx = p / PAGE_SIZE as u64;
            let po = (p % PAGE_SIZE as u64) as usize;
            let k = (PAGE_SIZE - po).min(n - done);
            match cache.lookup(idx) {
                Some(pa) => buf[done..done + k].copy_from_slice(&page_slice(pa)[po..po + k]),
                None => buf[done..done + k].fill(0),
            }
            done += k;
        }
        Ok(n)
    }

    fn write_at(&self, off: u64, buf: &[u8]) -> Result<usize> {
        let Kind::File { cache, size } = &self.kind else { return Err(EINVAL) };
        let mut done = 0;
        while done < buf.len() {
            let p = off + done as u64;
            let idx = p / PAGE_SIZE as u64;
            let po = (p % PAGE_SIZE as u64) as usize;
            let k = (PAGE_SIZE - po).min(buf.len() - done);
            let pa = cache.get_or_fill(idx, |pg| {
                pg.fill(0);
                Ok(())
            })?;
            page_slice(pa)[po..po + k].copy_from_slice(&buf[done..done + k]);
            done += k;
        }
        let mut s = size.lock();
        if off + done as u64 > *s {
            *s = off + done as u64;
        }
        drop(s);
        self.touch();
        Ok(done)
    }

    fn get_page(&self, index: u64) -> Result<usize> {
        let Kind::File { cache, size } = &self.kind else { return Err(ENODEV) };
        if index * PAGE_SIZE as u64 >= *size.lock() {
            return Err(ENXIO);
        }
        let pa = cache.get_or_fill(index, |pg| {
            pg.fill(0);
            Ok(())
        })?;
        crate::mm::frame::get_page(pa);
        Ok(pa)
    }

    fn fallocate(&self, mode: u32, off: u64, len: u64) -> Result<()> {
        if mode != 0 {
            return Err(EOPNOTSUPP);
        }
        let Kind::File { size, .. } = &self.kind else { return Err(ENODEV) };
        let end = off + len;
        let mut s = size.lock();
        if end > *s {
            *s = end;
        }
        Ok(())
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>> {
        self.dir()?.lock().entries.get(name).map(|(i, _)| i.clone()).ok_or(ENOENT)
    }

    fn create(&self, name: &[u8], mode: u32, rdev: u64, uid: u32, gid: u32) -> Result<Arc<dyn Inode>> {
        let fs = self.tfs();
        let i = TmpInode::new(&fs, mode, rdev, uid, gid);
        if let Kind::Dir(d) = &i.kind {
            d.lock().parent_ino = self.meta.lock().ino;
        }
        self.add_entry(name, i.clone())?;
        if mode & S_IFMT == S_IFDIR {
            self.meta.lock().nlink += 1;
        }
        Ok(i)
    }

    fn symlink(&self, name: &[u8], target: &[u8], uid: u32, gid: u32) -> Result<Arc<dyn Inode>> {
        let fs = self.tfs();
        let i = Arc::new(TmpInode {
            meta: SpinLock::new(MemMeta::new(S_IFLNK | 0o777, uid, gid)),
            fs: Arc::downgrade(&fs),
            kind: Kind::Symlink(target.to_vec()),
        });
        self.add_entry(name, i.clone())?;
        Ok(i)
    }

    fn link(&self, name: &[u8], target: &Arc<dyn Inode>) -> Result<()> {
        let t = as_tmp(target).ok_or(EXDEV)?;
        if !Arc::ptr_eq(&t.tfs(), &self.tfs()) {
            return Err(EXDEV);
        }
        self.add_entry(name, target.clone())?;
        let mut m = t.meta.lock();
        m.nlink += 1;
        m.ctime = Timespec::now();
        Ok(())
    }

    fn unlink(&self, name: &[u8]) -> Result<()> {
        let i = self.lookup(name)?;
        if is_dir(&i) {
            return Err(EISDIR);
        }
        self.remove_entry(name);
        if let Some(t) = as_tmp(&i) {
            let mut m = t.meta.lock();
            m.nlink = m.nlink.saturating_sub(1);
            m.ctime = Timespec::now();
        }
        Ok(())
    }

    fn rmdir(&self, name: &[u8]) -> Result<()> {
        let i = self.lookup(name)?;
        let t = as_tmp(&i).ok_or(ENOTDIR)?;
        if !t.dir()?.lock().entries.is_empty() {
            return Err(ENOTEMPTY);
        }
        self.remove_entry(name);
        t.meta.lock().nlink = 0;
        self.meta.lock().nlink -= 1;
        Ok(())
    }

    fn rename(&self, old: &[u8], newdir: &Arc<dyn Inode>, new: &[u8], flags: u32) -> Result<()> {
        let nd = as_tmp(newdir).ok_or(EXDEV)?;
        if !Arc::ptr_eq(&nd.tfs(), &self.tfs()) {
            return Err(EXDEV);
        }
        let src = self.lookup(old)?;
        let src_dir = is_dir(&src);
        if flags & 2 != 0 {
            // RENAME_EXCHANGE
            let dst = nd.lookup(new)?;
            let same = core::ptr::eq(self as *const _ as *const u8, Arc::as_ptr(newdir) as *const u8);
            if same {
                let mut d = self.dir()?.lock();
                let a = d.entries.get(old).unwrap().0.clone();
                let b = d.entries.get(new).unwrap().0.clone();
                d.entries.get_mut(old).unwrap().0 = b;
                d.entries.get_mut(new).unwrap().0 = a;
            } else {
                self.dir()?.lock().entries.get_mut(old).unwrap().0 = dst;
                nd.dir()?.lock().entries.get_mut(new).unwrap().0 = src;
            }
            return Ok(());
        }
        if let Ok(dst) = nd.lookup(new) {
            if Arc::ptr_eq(&dst, &src) {
                return Ok(());
            }
            if flags & 1 != 0 {
                return Err(EEXIST);
            }
            let dst_dir = is_dir(&dst);
            if src_dir && !dst_dir {
                return Err(ENOTDIR);
            }
            if !src_dir && dst_dir {
                return Err(EISDIR);
            }
            if dst_dir {
                nd.rmdir(new)?;
            } else {
                nd.unlink(new)?;
            }
        }
        self.remove_entry(old);
        nd.add_entry(new, src.clone())?;
        if src_dir {
            if let Some(t) = as_tmp(&src) {
                if let Ok(d) = t.dir() {
                    d.lock().parent_ino = nd.meta.lock().ino;
                }
            }
            self.meta.lock().nlink -= 1;
            nd.meta.lock().nlink += 1;
        }
        Ok(())
    }

    fn readdir(&self, pos: u64, f: &mut dyn FnMut(&[u8], u64, u8, u64) -> bool) -> Result<()> {
        let d = self.dir()?;
        let me = self.meta.lock().ino;
        let mut pos = pos;
        if pos == 0 {
            if !f(b".", me, DT_DIR, 1) {
                return Ok(());
            }
            pos = 1;
        }
        if pos == 1 {
            let p = d.lock().parent_ino;
            if !f(b"..", p, DT_DIR, 2) {
                return Ok(());
            }
            pos = 2;
        }
        loop {
            let next = {
                let g = d.lock();
                g.by_cookie.range(pos..).next().map(|(c, n)| (*c, n.clone(), g.entries.get(n).map(|e| e.0.clone())))
            };
            let Some((c, name, Some(inode))) = next else { return Ok(()) };
            let st = inode.stat()?;
            if !f(&name, st.ino, dtype(st.mode), c + 1) {
                return Ok(());
            }
            pos = c + 1;
        }
    }

    fn readlink(&self) -> Result<Vec<u8>> {
        match &self.kind {
            Kind::Symlink(t) => Ok(t.clone()),
            _ => Err(EINVAL),
        }
    }

    fn fs(&self) -> Option<Arc<dyn FileSystem>> {
        Some(self.tfs())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
