//! Namespaces (namespaces(7)): user, UTS, IPC, mount, PID, network and cgroup.
//!
//! Inside the kernel every id stays global: uids and gids are kernel ids ("kuid"), pids and tids are
//! the ids of the initial PID namespace. Namespaces translate at the system call boundary:
//! `from_kuid` / `make_kuid` for user ids, `PidNs::local_of` / `PidNs::global_of` for process ids.
use crate::errno::*;
use crate::sync::SpinLock;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Inode numbers of the initial namespaces, as Linux (PROC_*_INIT_INO), shown in /proc/PID/ns.
pub const INO_IPC: u64 = 0xEFFF_FFFF;
pub const INO_UTS: u64 = 0xEFFF_FFFE;
pub const INO_USER: u64 = 0xEFFF_FFFD;
pub const INO_PID: u64 = 0xEFFF_FFFC;
pub const INO_CGROUP: u64 = 0xEFFF_FFFB;
pub const INO_TIME: u64 = 0xEFFF_FFFA;
pub const INO_NET: u64 = 0xEFFF_FFF9;
pub const INO_MNT: u64 = 0xEFFF_FFF8;

static NEXT_INO: AtomicU64 = AtomicU64::new(0xF000_0000);

fn new_ino() -> u64 {
    NEXT_INO.fetch_add(1, Ordering::Relaxed)
}

pub fn new_ino_pub() -> u64 {
    new_ino()
}

// ---------------------------------------------------------------- user namespaces

/// Linux's limit on nesting (user_namespaces(7)).
const MAX_DEPTH: u32 = 32;

/// One line of uid_map / gid_map: `inside` .. `inside + count` maps to kernel ids `kernel` .. .
#[derive(Clone, Copy, Debug)]
pub struct Extent {
    pub inside: u32,
    pub kernel: u32,
    pub count: u32,
}

pub struct UserNs {
    pub ino: u64,
    pub parent: Option<Arc<UserNs>>,
    pub level: u32,
    /// The creator's effective uid and gid (kernel ids): it has every capability in this namespace
    /// when it is in the parent namespace.
    pub owner: u32,
    pub group: u32,
    pub uid_map: SpinLock<Option<Vec<Extent>>>,
    pub gid_map: SpinLock<Option<Vec<Extent>>>,
    /// /proc/PID/setgroups: "deny" makes setgroups(2) fail in this namespace (and its children).
    pub setgroups_allowed: AtomicBool,
    /// /proc/sys/user/max_user_namespaces as seen in this namespace, and how many are alive below it.
    pub max_user_namespaces: AtomicU32,
    pub user_namespaces: AtomicU32,
}

impl core::fmt::Debug for UserNs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "user:[{}]", self.ino)
    }
}

pub static INIT_USER_NS: crate::sync::Once<Arc<UserNs>> = crate::sync::Once::new();

pub fn init_user_ns() -> Arc<UserNs> {
    INIT_USER_NS
        .call_once(|| {
            Arc::new(UserNs {
                ino: INO_USER,
                parent: None,
                level: 0,
                owner: 0,
                group: 0,
                uid_map: SpinLock::new(Some(alloc::vec![Extent { inside: 0, kernel: 0, count: u32::MAX }])),
                gid_map: SpinLock::new(Some(alloc::vec![Extent { inside: 0, kernel: 0, count: u32::MAX }])),
                setgroups_allowed: AtomicBool::new(true),
                max_user_namespaces: AtomicU32::new(15_000),
                user_namespaces: AtomicU32::new(0),
            })
        })
        .clone()
}

/// The id that `id` (in `map`) has in the kernel, or None if it is not mapped.
fn down(map: &Option<Vec<Extent>>, id: u32) -> Option<u32> {
    map.as_ref()?.iter().find(|e| id >= e.inside && id - e.inside < e.count).map(|e| e.kernel + (id - e.inside))
}

fn up(map: &Option<Vec<Extent>>, kid: u32) -> Option<u32> {
    map.as_ref()?.iter().find(|e| kid >= e.kernel && kid - e.kernel < e.count).map(|e| e.inside + (kid - e.kernel))
}

/// The overflow ids shown for kernel ids that have no mapping (/proc/sys/kernel/overflowuid).
pub const OVERFLOW_ID: u32 = 65534;

impl UserNs {
    pub fn is_init(&self) -> bool {
        self.parent.is_none()
    }
    /// A uid of this namespace as a kernel uid.
    pub fn make_kuid(&self, uid: u32) -> Option<u32> {
        down(&self.uid_map.lock(), uid)
    }
    pub fn make_kgid(&self, gid: u32) -> Option<u32> {
        down(&self.gid_map.lock(), gid)
    }
    /// A kernel uid as seen in this namespace, or None.
    pub fn uid_in(&self, kuid: u32) -> Option<u32> {
        up(&self.uid_map.lock(), kuid)
    }
    pub fn gid_in(&self, kgid: u32) -> Option<u32> {
        up(&self.gid_map.lock(), kgid)
    }
    /// For system calls that return ids: unmapped ids read as the overflow id.
    pub fn from_kuid(&self, kuid: u32) -> u32 {
        if kuid == u32::MAX {
            return u32::MAX;
        }
        self.uid_in(kuid).unwrap_or(OVERFLOW_ID)
    }
    pub fn from_kgid(&self, kgid: u32) -> u32 {
        if kgid == u32::MAX {
            return u32::MAX;
        }
        self.gid_in(kgid).unwrap_or(OVERFLOW_ID)
    }
    /// Whether `self` is `other` or one of its ancestors.
    pub fn is_ancestor_of(self: &Arc<Self>, other: &Arc<UserNs>) -> bool {
        let mut n = Some(other.clone());
        while let Some(x) = n {
            if Arc::ptr_eq(&x, self) {
                return true;
            }
            n = x.parent.clone();
        }
        false
    }
}

/// A new user namespace owned by `cred` (clone/unshare with CLONE_NEWUSER).
pub fn new_user_ns(cred: &crate::task::Cred) -> Result<Arc<UserNs>> {
    let parent = cred.user_ns.clone();
    if parent.level + 1 > MAX_DEPTH {
        return Err(EUSERS);
    }
    // the limit of every ancestor applies (ucounts)
    let mut n = Some(parent.clone());
    while let Some(x) = n {
        if x.user_namespaces.load(Ordering::Relaxed) + 1 > x.max_user_namespaces.load(Ordering::Relaxed) {
            return Err(ENOSPC);
        }
        n = x.parent.clone();
    }
    let mut n = Some(parent.clone());
    while let Some(x) = n {
        x.user_namespaces.fetch_add(1, Ordering::Relaxed);
        n = x.parent.clone();
    }
    Ok(Arc::new(UserNs {
        ino: new_ino(),
        level: parent.level + 1,
        owner: cred.euid,
        group: cred.egid,
        uid_map: SpinLock::new(None),
        gid_map: SpinLock::new(None),
        setgroups_allowed: AtomicBool::new(parent.setgroups_allowed.load(Ordering::Relaxed)),
        max_user_namespaces: AtomicU32::new(parent.max_user_namespaces.load(Ordering::Relaxed)),
        user_namespaces: AtomicU32::new(0),
        parent: Some(parent),
    }))
}

impl Drop for UserNs {
    fn drop(&mut self) {
        let mut n = self.parent.clone();
        while let Some(x) = n {
            x.user_namespaces.fetch_sub(1, Ordering::Relaxed);
            n = x.parent.clone();
        }
    }
}

/// Linux's cap_capable: does `cred` have `cap` in `ns`? Capabilities count in the credential's own
/// namespace and every namespace below it; the owner of a child namespace has all of them there.
pub fn ns_capable(cred: &crate::task::Cred, ns: &Arc<UserNs>, cap: u32) -> bool {
    let mut ns = ns.clone();
    loop {
        if Arc::ptr_eq(&ns, &cred.user_ns) {
            return cred.cap_eff & (1 << cap) != 0;
        }
        if ns.level <= cred.user_ns.level {
            return false;
        }
        let parent = ns.parent.clone().unwrap();
        if Arc::ptr_eq(&parent, &cred.user_ns) && ns.owner == cred.euid {
            return true;
        }
        ns = parent;
    }
}

/// Writes /proc/PID/uid_map or gid_map of the namespace `ns` (user_namespaces(7)): once, by a
/// process in that namespace or its parent; an unprivileged writer may only map its own id.
pub fn write_map(ns: &Arc<UserNs>, gid: bool, text: &[u8], writer: &crate::task::Cred) -> Result<()> {
    let parent = ns.parent.clone().ok_or(EPERM)?;
    if !(Arc::ptr_eq(&writer.user_ns, ns) || Arc::ptr_eq(&writer.user_ns, &parent)) {
        return Err(EPERM);
    }
    let slot = if gid { &ns.gid_map } else { &ns.uid_map };
    if slot.lock().is_some() {
        return Err(EPERM);
    }
    let text = core::str::from_utf8(text).map_err(|_| EINVAL)?;
    let mut ext: Vec<Extent> = Vec::new();
    let mut lower: Vec<(u32, u32)> = Vec::new(); // (id in the parent namespace, count)
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.is_empty() {
            continue;
        }
        if f.len() != 3 || ext.len() >= 340 {
            return Err(EINVAL);
        }
        let n = |s: &str| s.parse::<u32>().map_err(|_| EINVAL);
        let (inside, outside, count) = (n(f[0])?, n(f[1])?, n(f[2])?);
        if count == 0 || inside.checked_add(count).is_none() || outside.checked_add(count).is_none() {
            return Err(EINVAL);
        }
        // no overlaps on either side
        for (e, l) in ext.iter().zip(&lower) {
            if inside < e.inside + e.count && e.inside < inside + count || outside < l.0 + l.1 && l.0 < outside + count {
                return Err(EINVAL);
            }
        }
        // the parent's ids as kernel ids (each extent must map entirely)
        let k = if gid { parent.make_kgid(outside) } else { parent.make_kuid(outside) }.ok_or(EPERM)?;
        let last = if gid { parent.make_kgid(outside + count - 1) } else { parent.make_kuid(outside + count - 1) };
        if last != Some(k + count - 1) {
            return Err(EPERM);
        }
        ext.push(Extent { inside, kernel: k, count });
        lower.push((outside, count));
    }
    if ext.is_empty() {
        return Err(EINVAL);
    }
    // permission: privileged in the parent namespace, or a single line mapping the writer's own id
    let cap = if gid { 6 } else { 7 }; // CAP_SETGID, CAP_SETUID
    let own = ext.len() == 1 && ext[0].count == 1 && {
        if gid {
            ext[0].kernel == writer.egid && !ns.setgroups_allowed.load(Ordering::Relaxed)
        } else {
            ext[0].kernel == writer.euid
        }
    };
    if !own && !ns_capable(writer, &parent, cap) {
        return Err(EPERM);
    }
    let mut s = slot.lock();
    if s.is_some() {
        return Err(EPERM);
    }
    *s = Some(ext);
    Ok(())
}

/// /proc/PID/uid_map as seen by a reader in `reader`: the parent's ids, relative to the reader.
pub fn read_map(ns: &Arc<UserNs>, gid: bool, reader: &Arc<UserNs>) -> Vec<u8> {
    let slot = if gid { &ns.gid_map } else { &ns.uid_map };
    let mut out = alloc::string::String::new();
    // the reader sees lower ids in its own namespace when it is the namespace itself: its parent's
    let base = if Arc::ptr_eq(reader, ns) { ns.parent.clone().unwrap_or_else(|| ns.clone()) } else { reader.clone() };
    if let Some(m) = slot.lock().as_ref() {
        for e in m {
            let lower = if base.is_init() { Some(e.kernel) } else if gid { base.gid_in(e.kernel) } else { base.uid_in(e.kernel) };
            if let Some(l) = lower {
                use core::fmt::Write;
                let _ = writeln!(out, "{:>10} {:>10} {:>10}", e.inside, l, e.count);
            }
        }
    }
    out.into_bytes()
}

// ---------------------------------------------------------------- UTS, IPC, cgroup

pub struct UtsNs {
    pub ino: u64,
    pub user_ns: Arc<UserNs>,
    pub hostname: SpinLock<Vec<u8>>,
    pub domainname: SpinLock<Vec<u8>>,
}

pub struct IpcNs {
    pub ino: u64,
    pub user_ns: Arc<UserNs>,
}

pub struct CgroupNs {
    pub ino: u64,
    pub user_ns: Arc<UserNs>,
    /// The cgroup that is "/" inside this namespace (path in the hierarchy).
    pub root: Vec<u8>,
}

// ---------------------------------------------------------------- network namespaces

pub struct NetNs {
    pub ino: u64,
    pub user_ns: Arc<UserNs>,
    /// The initial namespace owns the devices (eth0 and the TCP/IP stack); a new one has only lo.
    pub is_init: bool,
    /// lo is up, and has 127.0.0.1/8 (a new namespace starts with it down and without addresses)
    pub lo_up: AtomicBool,
    pub lo_addr: AtomicBool,
}

pub static INIT_NET_NS: crate::sync::Once<Arc<NetNs>> = crate::sync::Once::new();

impl NetNs {
    /// 127.0.0.0/8 is reachable (lo up with its address).
    pub fn loopback_ok(&self) -> bool {
        self.lo_up.load(Ordering::Relaxed) && self.lo_addr.load(Ordering::Relaxed)
    }
}

/// The network namespace of the current process (the initial one for kernel threads).
pub fn cur_net() -> Arc<NetNs> {
    match crate::task::try_current_proc() {
        Some(p) => p.ns.lock().net.clone(),
        None => init_net_ns(),
    }
}

pub fn init_net_ns() -> Arc<NetNs> {
    INIT_NET_NS.call_once(|| Arc::new(NetNs { ino: INO_NET, user_ns: init_user_ns(), is_init: true, lo_up: AtomicBool::new(true), lo_addr: AtomicBool::new(true) })).clone()
}

// ---------------------------------------------------------------- PID namespaces

pub struct PidNs {
    pub ino: u64,
    pub user_ns: Arc<UserNs>,
    pub parent: Option<Arc<PidNs>>,
    pub level: u32,
    next: AtomicU32,
    /// local id -> global id, and back (empty for the initial namespace, where they are equal)
    ids: SpinLock<(BTreeMap<u32, u32>, BTreeMap<u32, u32>)>,
    /// pid 1 of this namespace: orphans go to it; when it exits, the namespace's processes are killed
    pub reaper: SpinLock<Weak<crate::task::Process>>,
    /// pid 1 has exited: no new processes (fork fails with ENOMEM)
    pub dead: AtomicBool,
}

pub static INIT_PID_NS: crate::sync::Once<Arc<PidNs>> = crate::sync::Once::new();

pub fn init_pid_ns() -> Arc<PidNs> {
    INIT_PID_NS
        .call_once(|| {
            Arc::new(PidNs {
                ino: INO_PID,
                user_ns: init_user_ns(),
                parent: None,
                level: 0,
                next: AtomicU32::new(1),
                ids: SpinLock::new((BTreeMap::new(), BTreeMap::new())),
                reaper: SpinLock::new(Weak::new()),
                dead: AtomicBool::new(false),
            })
        })
        .clone()
}

impl PidNs {
    pub fn new(parent: &Arc<PidNs>, user_ns: Arc<UserNs>) -> Result<Arc<PidNs>> {
        if parent.level + 1 > MAX_DEPTH {
            return Err(ENOSPC);
        }
        Ok(Arc::new(PidNs {
            ino: new_ino(),
            user_ns,
            parent: Some(parent.clone()),
            level: parent.level + 1,
            next: AtomicU32::new(1),
            ids: SpinLock::new((BTreeMap::new(), BTreeMap::new())),
            reaper: SpinLock::new(Weak::new()),
            dead: AtomicBool::new(false),
        }))
    }
    pub fn is_init(&self) -> bool {
        self.parent.is_none()
    }
    /// The id of global pid/tid `g` in this namespace (0: not visible here).
    pub fn local_of(&self, g: u32) -> u32 {
        if self.is_init() {
            return g;
        }
        self.ids.lock().1.get(&g).copied().unwrap_or(0)
    }
    /// The global id of `local` in this namespace.
    pub fn global_of(&self, local: u32) -> Option<u32> {
        if self.is_init() {
            return Some(local);
        }
        self.ids.lock().0.get(&local).copied()
    }
    /// Gives global id `g` an id in this namespace and all its ancestors; returns the local id.
    pub fn attach(self: &Arc<Self>, g: u32) -> u32 {
        let mut n = Some(self.clone());
        let mut first = g;
        let mut top = true;
        while let Some(x) = n {
            if !x.is_init() {
                let l = x.next.fetch_add(1, Ordering::Relaxed);
                let mut ids = x.ids.lock();
                ids.0.insert(l, g);
                ids.1.insert(g, l);
                if top {
                    first = l;
                }
            }
            top = false;
            n = x.parent.clone();
        }
        first
    }
    /// Forgets global id `g` (the task was reaped).
    pub fn detach(self: &Arc<Self>, g: u32) {
        let mut n = Some(self.clone());
        while let Some(x) = n {
            if !x.is_init() {
                let mut ids = x.ids.lock();
                if let Some(l) = ids.1.remove(&g) {
                    ids.0.remove(&l);
                }
            }
            n = x.parent.clone();
        }
    }
    /// Whether `self` is `other` or an ancestor of it (processes of `other` are visible in `self`).
    pub fn contains(self: &Arc<Self>, other: &Arc<PidNs>) -> bool {
        let mut n = Some(other.clone());
        while let Some(x) = n {
            if Arc::ptr_eq(&x, self) {
                return true;
            }
            n = x.parent.clone();
        }
        false
    }
    /// The global ids of everything visible here.
    pub fn members(&self) -> Vec<u32> {
        self.ids.lock().1.keys().copied().collect()
    }
}

// ---------------------------------------------------------------- the set a process uses

#[derive(Clone)]
pub struct NsProxy {
    pub uts: Arc<UtsNs>,
    pub ipc: Arc<IpcNs>,
    pub mnt: Arc<crate::vfs::MntNs>,
    /// where children are created (unshare(CLONE_NEWPID) changes only this)
    pub pid_for_children: Arc<PidNs>,
    pub net: Arc<NetNs>,
    pub cgroup: Arc<CgroupNs>,
}

pub fn init_nsproxy(mnt: Arc<crate::vfs::MntNs>) -> NsProxy {
    let user = init_user_ns();
    NsProxy {
        uts: Arc::new(UtsNs { ino: INO_UTS, user_ns: user.clone(), hostname: SpinLock::new(Vec::new()), domainname: SpinLock::new(Vec::from(&b"(none)"[..])) }),
        ipc: Arc::new(IpcNs { ino: INO_IPC, user_ns: user.clone() }),
        mnt,
        pid_for_children: init_pid_ns(),
        net: init_net_ns(),
        cgroup: Arc::new(CgroupNs { ino: INO_CGROUP, user_ns: user, root: Vec::from(&b"/"[..]) }),
    }
}

pub const CLONE_NEWNS: u64 = 0x20000;
pub const CLONE_NEWCGROUP: u64 = 0x2000000;
pub const CLONE_NEWUTS: u64 = 0x4000000;
pub const CLONE_NEWIPC: u64 = 0x8000000;
pub const CLONE_NEWUSER: u64 = 0x10000000;
pub const CLONE_NEWPID: u64 = 0x20000000;
pub const CLONE_NEWNET: u64 = 0x40000000;
pub const CLONE_NEW_ANY: u64 = CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET;

/// The namespaces after clone/unshare with `flags` (the user namespace, if new, is already in
/// `cred`). `fs` is the caller's root and cwd, moved into the new mount namespace's copy.
pub fn copy_namespaces(flags: u64, old: &NsProxy, cred: &crate::task::Cred, fs: Option<&mut crate::task::FsContext>, cgroup_of_caller: &[u8]) -> Result<NsProxy> {
    let mut n = old.clone();
    if flags & CLONE_NEW_ANY & !CLONE_NEWUSER == 0 {
        return Ok(n);
    }
    // creating namespaces other than a user namespace takes CAP_SYS_ADMIN in the owning namespace
    if !ns_capable(cred, &cred.user_ns, 21) {
        return Err(EPERM);
    }
    let user = cred.user_ns.clone();
    if flags & CLONE_NEWUTS != 0 {
        n.uts = Arc::new(UtsNs {
            ino: new_ino(),
            user_ns: user.clone(),
            hostname: SpinLock::new(old.uts.hostname.lock().clone()),
            domainname: SpinLock::new(old.uts.domainname.lock().clone()),
        });
    }
    if flags & CLONE_NEWIPC != 0 {
        n.ipc = Arc::new(IpcNs { ino: new_ino(), user_ns: user.clone() });
    }
    if flags & CLONE_NEWNET != 0 {
        n.net = Arc::new(NetNs { ino: new_ino(), user_ns: user.clone(), is_init: false, lo_up: AtomicBool::new(false), lo_addr: AtomicBool::new(false) });
    }
    if flags & CLONE_NEWPID != 0 {
        n.pid_for_children = PidNs::new(&old.pid_for_children, user.clone())?;
    }
    if flags & CLONE_NEWCGROUP != 0 {
        n.cgroup = Arc::new(CgroupNs { ino: new_ino(), user_ns: user.clone(), root: Vec::from(cgroup_of_caller) });
    }
    if flags & CLONE_NEWNS != 0 {
        n.mnt = crate::vfs::MntNs::copy(&old.mnt, user, fs)?;
    }
    Ok(n)
}
