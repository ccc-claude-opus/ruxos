//! Dentries (the cache of names), mounts and path resolution. A position in the tree is a
//! `PathRef` = (mount, dentry), as in Linux, so the same file system can be mounted in several
//! places (bind mounts) and `..` leaves a mount through its mount point.
use super::{FileSystem, Inode, S_IFDIR, S_IFLNK, S_IFMT};
use crate::errno::*;
use crate::sync::SpinLock;
use crate::task::Cred;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

pub struct DEntry {
    pub name: SpinLock<Vec<u8>>,
    pub parent: SpinLock<Option<Arc<DEntry>>>,
    /// None: a negative entry (the name does not exist).
    pub inode: SpinLock<Option<Arc<dyn Inode>>>,
    children: SpinLock<BTreeMap<Vec<u8>, Arc<DEntry>>>,
    pub fs: Arc<dyn FileSystem>,
}

impl DEntry {
    pub fn new_root(fs: Arc<dyn FileSystem>) -> Arc<DEntry> {
        Arc::new(DEntry {
            name: SpinLock::new(b"/".to_vec()),
            parent: SpinLock::new(None),
            inode: SpinLock::new(Some(fs.root())),
            children: SpinLock::new(BTreeMap::new()),
            fs,
        })
    }

    pub fn inode(&self) -> Result<Arc<dyn Inode>> {
        self.inode.lock().clone().ok_or(ENOENT)
    }

    pub fn parent(&self) -> Option<Arc<DEntry>> {
        self.parent.lock().clone()
    }

    /// The child `name`, from the cache or the file system (negative entries are cached too).
    pub fn child(self: &Arc<DEntry>, name: &[u8]) -> Result<Arc<DEntry>> {
        let cache = self.fs.cache_dentries();
        // file systems that do not cache dentries still keep their mount points here (pinned by
        // Mount::attach), so what is mounted on them is found again
        if let Some(d) = self.children.lock().get(name) {
            return Ok(d.clone());
        }
        let dir = self.inode()?;
        let inode = match dir.lookup(name) {
            Ok(i) => Some(i),
            Err(ENOENT) => None,
            Err(e) => return Err(e),
        };
        let d = Arc::new(DEntry {
            name: SpinLock::new(name.to_vec()),
            parent: SpinLock::new(Some(self.clone())),
            inode: SpinLock::new(inode),
            children: SpinLock::new(BTreeMap::new()),
            fs: self.fs.clone(),
        });
        if cache {
            let mut c = self.children.lock();
            if let Some(old) = c.get(name) {
                return Ok(old.clone());
            }
            c.insert(name.to_vec(), d.clone());
        }
        Ok(d)
    }

    /// Records that `name` now exists (after create/link/rename in).
    pub fn set_child(self: &Arc<DEntry>, name: &[u8], inode: Option<Arc<dyn Inode>>) {
        if !self.fs.cache_dentries() {
            return;
        }
        let mut c = self.children.lock();
        match c.get(name) {
            Some(d) => *d.inode.lock() = inode,
            None => {
                let d = Arc::new(DEntry {
                    name: SpinLock::new(name.to_vec()),
                    parent: SpinLock::new(Some(self.clone())),
                    inode: SpinLock::new(inode),
                    children: SpinLock::new(BTreeMap::new()),
                    fs: self.fs.clone(),
                });
                c.insert(name.to_vec(), d);
            }
        }
    }

    /// Forgets `name` (it was removed or replaced by something the cache cannot describe).
    pub fn drop_child(&self, name: &[u8]) {
        if let Some(d) = self.children.lock().remove(name) {
            *d.inode.lock() = None;
        }
    }

    /// Moves the cached entry `old` of self to `new` of `newdir` (rename).
    pub fn move_child(self: &Arc<DEntry>, old: &[u8], newdir: &Arc<DEntry>, new: &[u8]) {
        let d = self.children.lock().remove(old);
        // the replaced target (if any) is gone
        if let Some(t) = newdir.children.lock().remove(new) {
            *t.inode.lock() = None;
        }
        if let Some(d) = d {
            *d.name.lock() = new.to_vec();
            *d.parent.lock() = Some(newdir.clone());
            if newdir.fs.cache_dentries() {
                newdir.children.lock().insert(new.to_vec(), d);
            }
        }
    }
}

pub const MNT_RDONLY: u32 = 1;
pub const MNT_NOSUID: u32 = 2;
pub const MNT_NODEV: u32 = 4;
pub const MNT_NOEXEC: u32 = 8;
pub const MNT_NOATIME: u32 = 0x400;
pub const MNT_RELATIME: u32 = 0x200000;

/// The flags a less privileged mount namespace may not clear on mounts it copied, and the mark of a
/// mount it may not unmount on its own (it would uncover what is beneath), as Linux's MNT_LOCK_*.
pub const MNT_LOCK_FLAGS: u32 = MNT_RDONLY | MNT_NOSUID | MNT_NODEV | MNT_NOEXEC | MNT_NOATIME | MNT_RELATIME;
pub const MNT_LOCKED: u32 = 1 << 31;

pub struct Mount {
    pub id: u32,
    pub root: Arc<DEntry>,
    /// MNT_LOCK_FLAGS bits that are locked, and MNT_LOCKED
    pub locked: AtomicU32,
    /// the mount namespace this mount belongs to
    pub ns: SpinLock<alloc::sync::Weak<MntNs>>,
    /// (parent mount, mount point) — None for the root of a namespace.
    pub parent: SpinLock<Option<(Arc<Mount>, Arc<DEntry>)>>,
    pub children: SpinLock<Vec<Arc<Mount>>>,
    pub flags: AtomicU32,
    pub source: String,
    pub fstype: String,
    pub options: String,
}

static NEXT_MNT: AtomicU32 = AtomicU32::new(1);

impl Mount {
    pub fn new(root: Arc<DEntry>, flags: u32, source: &str, fstype: &str, options: &str) -> Arc<Mount> {
        Arc::new(Mount {
            id: NEXT_MNT.fetch_add(1, Ordering::Relaxed),
            root,
            locked: AtomicU32::new(0),
            ns: SpinLock::new(alloc::sync::Weak::new()),
            parent: SpinLock::new(None),
            children: SpinLock::new(Vec::new()),
            flags: AtomicU32::new(flags),
            source: String::from(source),
            fstype: String::from(fstype),
            options: String::from(options),
        })
    }

    pub fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }

    /// The topmost mount on dentry `d` of this mount, if any.
    fn mounted_on(&self, d: &Arc<DEntry>) -> Option<Arc<Mount>> {
        self.children
            .lock()
            .iter()
            .rev()
            .find(|m| m.parent.lock().as_ref().is_some_and(|(_, mp)| Arc::ptr_eq(mp, d)))
            .cloned()
    }

    /// Attaches `child` at `at` (a dentry of self).
    pub fn attach(self: &Arc<Mount>, at: &Arc<DEntry>, child: &Arc<Mount>) {
        // a mount point of a file system without a dentry cache stays in its parent's children,
        // and so do its ancestors (up to the file system's root), so lookups find it again
        if !at.fs.cache_dentries() {
            let mut d = at.clone();
            while let Some(p) = d.parent() {
                let name = d.name.lock().clone();
                let kept = p.children.lock().entry(name).or_insert_with(|| d.clone()).clone();
                if !Arc::ptr_eq(&kept, &d) {
                    break; // already pinned from here up
                }
                d = p;
            }
        }
        *child.parent.lock() = Some((self.clone(), at.clone()));
        set_ns(child, &self.ns.lock().clone());
        self.children.lock().push(child.clone());
    }

    pub fn detach(self: &Arc<Mount>) {
        let p = self.parent.lock().take();
        if let Some((pm, _)) = p {
            pm.children.lock().retain(|m| !Arc::ptr_eq(m, self));
        }
    }
}

/// `m` and the mounts below it belong to namespace `ns`.
fn set_ns(m: &Arc<Mount>, ns: &alloc::sync::Weak<MntNs>) {
    *m.ns.lock() = ns.clone();
    for c in m.children.lock().clone() {
        set_ns(&c, ns);
    }
}

/// A mount namespace: a tree of mounts.
pub struct MntNs {
    pub ino: u64,
    pub user_ns: Arc<crate::ns::UserNs>,
    pub root: SpinLock<Arc<Mount>>,
}

static ALL_MNT_NS: SpinLock<Vec<alloc::sync::Weak<MntNs>>> = SpinLock::new(Vec::new());

impl MntNs {
    fn register(ns: Arc<MntNs>) -> Arc<MntNs> {
        set_ns(&ns.root.lock().clone(), &Arc::downgrade(&ns));
        let mut a = ALL_MNT_NS.lock();
        a.retain(|w| w.strong_count() > 0);
        a.push(Arc::downgrade(&ns));
        ns
    }

    /// The initial namespace, whose tree contains `m`.
    pub fn new_init(mut m: Arc<Mount>) -> Arc<MntNs> {
        loop {
            let p = m.parent.lock().clone();
            match p {
                Some((p, _)) => m = p,
                None => break,
            }
        }
        Self::register(Arc::new(MntNs { ino: crate::ns::INO_MNT, user_ns: crate::ns::init_user_ns(), root: SpinLock::new(m) }))
    }

    /// CLONE_NEWNS: a copy of `old`'s tree owned by `user`. When `user` is less privileged than the
    /// owner of `old`, the copied mounts keep their flags and stay together (MNT_LOCK_*). The root
    /// and working directory in `fs` move to the copies.
    pub fn copy(old: &Arc<MntNs>, user: Arc<crate::ns::UserNs>, fs: Option<&mut crate::task::FsContext>) -> Result<Arc<MntNs>> {
        let lock = !Arc::ptr_eq(&user, &old.user_ns);
        let mut map: Vec<(Arc<Mount>, Arc<Mount>)> = Vec::new();
        fn dup(m: &Arc<Mount>, lock: bool, top: bool, map: &mut Vec<(Arc<Mount>, Arc<Mount>)>) -> Arc<Mount> {
            let n = Mount::new(m.root.clone(), m.flags(), &m.source, &m.fstype, &m.options);
            let mut l = m.locked.load(Ordering::Relaxed);
            if lock {
                l |= m.flags() & MNT_LOCK_FLAGS;
                if !top {
                    l |= MNT_LOCKED;
                }
            }
            n.locked.store(l, Ordering::Relaxed);
            map.push((m.clone(), n.clone()));
            for c in m.children.lock().clone() {
                let at = c.parent.lock().as_ref().map(|(_, d)| d.clone());
                if let Some(at) = at {
                    let cn = dup(&c, lock, false, map);
                    *cn.parent.lock() = Some((n.clone(), at));
                    n.children.lock().push(cn);
                }
            }
            n
        }
        let root = dup(&old.root.lock().clone(), lock, true, &mut map);
        let ns = Self::register(Arc::new(MntNs { ino: crate::ns::new_ino_pub(), user_ns: user, root: SpinLock::new(root) }));
        if let Some(f) = fs {
            let tr = |p: &PathRef| -> PathRef {
                match map.iter().find(|(o, _)| Arc::ptr_eq(o, &p.mnt)) {
                    Some((_, n)) => PathRef { mnt: n.clone(), d: p.d.clone() },
                    None => p.clone(),
                }
            };
            f.root = tr(&f.root);
            f.cwd = tr(&f.cwd);
        }
        Ok(ns)
    }

    /// Every mount of every namespace.
    pub fn all_mounts() -> Vec<Arc<Mount>> {
        let nss: Vec<Arc<MntNs>> = ALL_MNT_NS.lock().iter().filter_map(|w| w.upgrade()).collect();
        let mut out = Vec::new();
        fn walk(m: &Arc<Mount>, out: &mut Vec<Arc<Mount>>) {
            out.push(m.clone());
            for c in m.children.lock().clone() {
                walk(&c, out);
            }
        }
        for n in nss {
            let r = n.root.lock().clone();
            walk(&r, &mut out);
        }
        out
    }
}

#[derive(Clone)]
pub struct PathRef {
    pub mnt: Arc<Mount>,
    pub d: Arc<DEntry>,
}

impl PathRef {
    pub fn inode(&self) -> Result<Arc<dyn Inode>> {
        self.d.inode()
    }

    pub fn same(&self, o: &PathRef) -> bool {
        Arc::ptr_eq(&self.mnt, &o.mnt) && Arc::ptr_eq(&self.d, &o.d)
    }

    /// Steps into whatever is mounted here.
    pub fn cross_down(mut self) -> PathRef {
        while let Some(m) = self.mnt.mounted_on(&self.d) {
            self.d = m.root.clone();
            self.mnt = m;
        }
        self
    }

    /// The parent directory (`..`), staying at `root`.
    pub fn up(&self, root: &PathRef) -> PathRef {
        let mut p = self.clone();
        loop {
            if p.same(root) {
                return p;
            }
            if Arc::ptr_eq(&p.d, &p.mnt.root) {
                let parent = p.mnt.parent.lock().clone();
                match parent {
                    Some((pm, mp)) => {
                        p = PathRef { mnt: pm, d: mp };
                        continue;
                    }
                    None => return p,
                }
            }
            return match p.d.parent() {
                Some(d) => PathRef { mnt: p.mnt.clone(), d },
                None => p,
            };
        }
    }

    /// The absolute path relative to `root` (for getcwd, /proc/self/exe...).
    pub fn path_from(&self, root: &PathRef) -> Vec<u8> {
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let mut p = self.clone();
        let mut guard = 0;
        while !p.same(root) && guard < 4096 {
            guard += 1;
            if Arc::ptr_eq(&p.d, &p.mnt.root) {
                let parent = p.mnt.parent.lock().clone();
                match parent {
                    Some((pm, mp)) => {
                        p = PathRef { mnt: pm, d: mp };
                        continue;
                    }
                    None => break,
                }
            }
            parts.push(p.d.name.lock().clone());
            match p.d.parent() {
                Some(d) => p.d = d,
                None => break,
            }
        }
        let mut out = Vec::new();
        for part in parts.iter().rev() {
            out.push(b'/');
            out.extend_from_slice(part);
        }
        if out.is_empty() {
            out.push(b'/');
        }
        out
    }
}

/// Permission check (DAC with the file system uid/gid; root passes except execute without any x bit).
pub fn may(inode: &Arc<dyn Inode>, want: u32, cred: &Cred) -> Result<()> {
    let st = inode.stat()?;
    if cred.fsuid == 0 {
        if want & 1 != 0 && st.mode & S_IFMT != S_IFDIR && st.mode & 0o111 == 0 {
            return Err(EACCES);
        }
        return Ok(());
    }
    let bits = if cred.fsuid == st.uid {
        (st.mode >> 6) & 7
    } else if cred.fsgid == st.gid || cred.groups.contains(&st.gid) {
        (st.mode >> 3) & 7
    } else {
        st.mode & 7
    };
    if bits & want == want { Ok(()) } else { Err(EACCES) }
}

pub struct Lookup<'a> {
    pub root: &'a PathRef,
    pub cred: &'a Cred,
    pub follow_last: bool,
    /// openat2 RESOLVE_BENEATH: leaving `root` (an absolute path, ".." above it) fails with EXDEV
    /// (RESOLVE_IN_ROOT instead just makes the start directory the root)
    pub beneath: bool,
    /// RESOLVE_NO_SYMLINKS, RESOLVE_NO_MAGICLINKS (ELOOP), RESOLVE_NO_XDEV (EXDEV on a mount crossing)
    pub no_symlinks: bool,
    pub no_magiclinks: bool,
    pub no_xdev: bool,
}

const MAX_SYMLINKS: usize = 40;

impl Lookup<'_> {
    /// Resolves `path` from `start` to an existing object.
    pub fn resolve(&self, start: &PathRef, path: &[u8]) -> Result<PathRef> {
        let mut links = 0;
        self.walk(start, path, self.follow_last, &mut links)
    }

    /// Resolves all but the last component: (directory, last name). The name is empty for "/".
    /// A trailing slash is reported through the returned bool.
    pub fn resolve_parent(&self, start: &PathRef, path: &[u8]) -> Result<(PathRef, Vec<u8>, bool)> {
        if path.is_empty() {
            return Err(ENOENT);
        }
        let trailing = path.ends_with(b"/");
        let trimmed = trim_slashes(path);
        let (dir, last) = match trimmed.iter().rposition(|&c| c == b'/') {
            Some(i) => (&trimmed[..i + 1], &trimmed[i + 1..]),
            None => (&b""[..], trimmed),
        };
        let mut links = 0;
        let d = if dir.is_empty() { start.clone() } else { self.walk(start, dir, true, &mut links)? };
        if !super::is_dir(&d.inode()?) {
            return Err(ENOTDIR);
        }
        Ok((d, last.to_vec(), trailing))
    }

    fn walk(&self, start: &PathRef, path: &[u8], follow_last: bool, links: &mut usize) -> Result<PathRef> {
        if path.is_empty() {
            return Err(ENOENT);
        }
        if path.len() > 4096 {
            return Err(ENAMETOOLONG);
        }
        if path[0] == b'/' && self.beneath {
            return Err(EXDEV);
        }
        let mut cur = if path[0] == b'/' { self.root.clone() } else { start.clone() };
        let comps: Vec<&[u8]> = path.split(|&c| c == b'/').filter(|c| !c.is_empty() && *c != b".").collect();
        let must_dir = path.ends_with(b"/");
        for (i, comp) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            let dir = cur.inode()?;
            if dir.stat()?.mode & S_IFMT != S_IFDIR {
                return Err(ENOTDIR);
            }
            may(&dir, 1, self.cred)?;
            if *comp == b".." {
                if self.beneath && cur.same(self.root) {
                    return Err(EXDEV);
                }
                let up = cur.up(self.root);
                if self.no_xdev && !Arc::ptr_eq(&up.mnt, &cur.mnt) {
                    return Err(EXDEV);
                }
                cur = up;
                continue;
            }
            if comp.len() > 255 {
                return Err(ENAMETOOLONG);
            }
            let d = cur.d.child(comp)?;
            let next = PathRef { mnt: cur.mnt.clone(), d }.cross_down();
            if self.no_xdev && !Arc::ptr_eq(&next.mnt, &cur.mnt) {
                return Err(EXDEV);
            }
            let inode = next.inode()?;
            let st = inode.stat()?;
            if st.mode & S_IFMT == S_IFLNK && (!last || follow_last || must_dir) {
                if self.no_symlinks {
                    return Err(ELOOP);
                }
                *links += 1;
                if *links > MAX_SYMLINKS {
                    return Err(ELOOP);
                }
                if let Some(t) = inode.magic_target() {
                    if self.no_magiclinks || self.beneath {
                        return Err(ELOOP);
                    }
                    cur = t?;
                    continue;
                }
                let target = inode.readlink()?;
                if target.is_empty() {
                    return Err(ENOENT);
                }
                let base = cur.clone();
                cur = self.walk(&base, &target, true, links)?;
                continue;
            }
            cur = next;
        }
        if must_dir && !super::is_dir(&cur.inode()?) {
            return Err(ENOTDIR);
        }
        Ok(cur)
    }
}

pub fn trim_slashes(p: &[u8]) -> &[u8] {
    let mut e = p.len();
    while e > 1 && p[e - 1] == b'/' {
        e -= 1;
    }
    &p[..e]
}

pub fn is_symlink(i: &Arc<dyn Inode>) -> bool {
    i.stat().map(|s| s.mode & S_IFMT == S_IFLNK).unwrap_or(false)
}
