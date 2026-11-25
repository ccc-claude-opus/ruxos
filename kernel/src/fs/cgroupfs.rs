//! The cgroup v2 file system (Documentation/admin-guide/cgroup-v2.rst): a directory per cgroup
//! (mkdir creates one, rmdir removes an empty one) with the interface files of the core and of the
//! controllers enabled for it.
use crate::cgroup::{self, Cgroup, CTRL_CPU, CTRL_MEMORY, CTRL_NAMES, CTRL_PIDS};
use crate::errno::*;
use crate::vfs::pseudo::{self, file, ino_of, text, Gen, PseudoFs, Writer};
use crate::vfs::*;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt::Write;
use core::sync::atomic::Ordering;

pub const CGROUP2_SUPER_MAGIC: u64 = 0x6367_7270;

/// A cgroup2 instance rooted at `root` (the root of the mounter's cgroup namespace).
pub fn new(root: Arc<Cgroup>) -> Arc<dyn FileSystem> {
    let fs = Arc::new(PseudoFs { name: "cgroup2", dev: tmpfs::anon_dev(), magic: CGROUP2_SUPER_MAGIC, root: crate::sync::SpinLock::new(None) });
    let dev = fs.dev;
    *fs.root.lock() = Some(Arc::new(CgDir { cg: root, dev }));
    fs
}

struct CgDir {
    cg: Arc<Cgroup>,
    dev: u64,
}

fn max_or(v: u64) -> String {
    if v == u64::MAX { String::from("max") } else { format!("{}", v) }
}

fn parse_max(d: &[u8]) -> Result<u64> {
    let s = core::str::from_utf8(d).map_err(|_| EINVAL)?.trim();
    if s == "max" {
        return Ok(u64::MAX);
    }
    s.parse().map_err(|_| EINVAL)
}

/// A memory size: bytes, optionally with K, M, G (memory.max accepts them).
fn parse_bytes(d: &[u8]) -> Result<u64> {
    let s = core::str::from_utf8(d).map_err(|_| EINVAL)?.trim();
    if s == "max" {
        return Ok(u64::MAX);
    }
    let (num, mul) = match s.as_bytes().last() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1u64 << 10),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1 << 20),
        Some(b'g' | b'G') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    let n: u64 = num.parse().map_err(|_| EINVAL)?;
    // rounded down to whole pages, as the kernel does
    Ok(n.checked_mul(mul).ok_or(EINVAL)? & !4095)
}

impl CgDir {
    /// The interface files of this cgroup: (name, mode, read, write).
    fn files(&self) -> Vec<(&'static str, u32, Gen, Option<Writer>)> {
        let cg = &self.cg;
        let mut v: Vec<(&'static str, u32, Gen, Option<Writer>)> = Vec::new();
        let c = cg.clone();
        v.push((
            "cgroup.procs",
            0o644,
            alloc::boxed::Box::new(move || {
                let mut s = String::new();
                for p in c.procs.lock().iter() {
                    let l = crate::task::vpid(*p);
                    if l != 0 {
                        let _ = writeln!(s, "{}", l);
                    }
                }
                text(s)
            }),
            Some({
                let c = cg.clone();
                alloc::boxed::Box::new(move |d: &[u8]| {
                    let pid: u32 = core::str::from_utf8(d).map_err(|_| EINVAL)?.trim().parse().map_err(|_| EINVAL)?;
                    let p = if pid == 0 { crate::task::current_proc().clone() } else { crate::task::proc_by_vpid(pid).ok_or(ESRCH)? };
                    cgroup::attach(&p, &c)
                })
            }),
        ));
        let c = cg.clone();
        v.push((
            "cgroup.threads",
            0o644,
            alloc::boxed::Box::new(move || {
                let mut s = String::new();
                for pid in c.procs.lock().iter() {
                    if let Some(p) = crate::task::proc_by_pid(*pid) {
                        for t in p.threads.lock().iter().filter_map(|w| w.upgrade()) {
                            let l = crate::task::vpid(t.tid);
                            if l != 0 {
                                let _ = writeln!(s, "{}", l);
                            }
                        }
                    }
                }
                text(s)
            }),
            None,
        ));
        let names = |bits: u32| -> String { CTRL_NAMES.iter().filter(|(b, _)| bits & b != 0).map(|(_, n)| *n).collect::<Vec<_>>().join(" ") + "\n" };
        let c = cg.clone();
        v.push(("cgroup.controllers", 0o444, alloc::boxed::Box::new(move || text(names(c.controllers()))), None));
        let (c, c2) = (cg.clone(), cg.clone());
        v.push((
            "cgroup.subtree_control",
            0o644,
            alloc::boxed::Box::new(move || text(names(c.subtree_control.load(Ordering::Relaxed)))),
            Some(alloc::boxed::Box::new(move |d: &[u8]| c2.set_subtree_control(d))),
        ));
        let c = cg.clone();
        v.push((
            "cgroup.events",
            0o444,
            alloc::boxed::Box::new(move || text(format!("populated {}\nfrozen 0\n", populated(&c) as u32))),
            None,
        ));
        v.push(("cgroup.type", 0o644, alloc::boxed::Box::new(|| text(String::from("domain\n"))), None));
        v.push(("cgroup.max.descendants", 0o644, alloc::boxed::Box::new(|| text(String::from("max\n"))), None));
        v.push(("cgroup.max.depth", 0o644, alloc::boxed::Box::new(|| text(String::from("max\n"))), None));
        let c = cg.clone();
        v.push((
            "cgroup.stat",
            0o444,
            alloc::boxed::Box::new(move || text(format!("nr_descendants {}\nnr_dying_descendants 0\n", descendants(&c)))),
            None,
        ));
        let ctl = cg.controllers();
        let root = cg.is_root();
        // cpu
        let c = cg.clone();
        v.push((
            "cpu.stat",
            0o444,
            alloc::boxed::Box::new(move || {
                let u = c.cpu_usage_ns.load(Ordering::Relaxed) / 1000;
                text(format!(
                    "usage_usec {}\nuser_usec {}\nsystem_usec 0\nnr_periods {}\nnr_throttled {}\nthrottled_usec {}\n",
                    u,
                    u,
                    c.cpu_nr_periods.load(Ordering::Relaxed),
                    c.cpu_nr_throttled.load(Ordering::Relaxed),
                    c.cpu_throttled_ns.load(Ordering::Relaxed) / 1000
                ))
            }),
            None,
        ));
        if ctl & CTRL_CPU != 0 && !root {
            let (c, c2) = (cg.clone(), cg.clone());
            v.push((
                "cpu.max",
                0o644,
                alloc::boxed::Box::new(move || text(format!("{} {}\n", max_or(c.cpu_quota_us.load(Ordering::Relaxed)), c.cpu_period_us.load(Ordering::Relaxed)))),
                Some(alloc::boxed::Box::new(move |d: &[u8]| {
                    // "$MAX $PERIOD" or "$MAX"
                    let s = core::str::from_utf8(d).map_err(|_| EINVAL)?;
                    let mut it = s.split_whitespace();
                    let q = parse_max(it.next().ok_or(EINVAL)?.as_bytes())?;
                    let p = match it.next() {
                        Some(p) => p.parse::<u64>().map_err(|_| EINVAL)?,
                        None => c2.cpu_period_us.load(Ordering::Relaxed),
                    };
                    if !(1000..=1_000_000).contains(&p) || (q != u64::MAX && q < 1000) {
                        return Err(EINVAL);
                    }
                    c2.set_cpu_max(q, p);
                    Ok(())
                })),
            ));
            v.push(("cpu.weight", 0o644, alloc::boxed::Box::new(|| text(String::from("100\n"))), Some(alloc::boxed::Box::new(|_| Ok(())))));
        }
        // memory
        if ctl & CTRL_MEMORY != 0 {
            let c = cg.clone();
            v.push(("memory.current", 0o444, alloc::boxed::Box::new(move || text(format!("{}\n", c.memory_current.load(Ordering::Relaxed)))), None));
            let c = cg.clone();
            v.push((
                "memory.stat",
                0o444,
                alloc::boxed::Box::new(move || text(format!("anon {}\nfile 0\n", c.memory_current.load(Ordering::Relaxed)))),
                None,
            ));
            if !root {
                let (c, c2) = (cg.clone(), cg.clone());
                v.push((
                    "memory.max",
                    0o644,
                    alloc::boxed::Box::new(move || text(format!("{}\n", max_or(c.memory_max.load(Ordering::Relaxed))))),
                    Some(alloc::boxed::Box::new(move |d: &[u8]| {
                        c2.memory_max.store(parse_bytes(d)?, Ordering::Relaxed);
                        Ok(())
                    })),
                ));
                for n in ["memory.high", "memory.swap.max"] {
                    v.push((n, 0o644, alloc::boxed::Box::new(|| text(String::from("max\n"))), Some(alloc::boxed::Box::new(|d: &[u8]| parse_bytes(d).map(|_| ())))));
                }
                for n in ["memory.min", "memory.low"] {
                    v.push((n, 0o644, alloc::boxed::Box::new(|| text(String::from("0\n"))), Some(alloc::boxed::Box::new(|d: &[u8]| parse_bytes(d).map(|_| ())))));
                }
                let c = cg.clone();
                v.push((
                    "memory.events",
                    0o444,
                    alloc::boxed::Box::new(move || {
                        text(format!(
                            "low 0\nhigh 0\nmax {}\noom {}\noom_kill {}\noom_group_kill 0\n",
                            c.memory_events_max.load(Ordering::Relaxed),
                            c.memory_events_oom_kill.load(Ordering::Relaxed),
                            c.memory_events_oom_kill.load(Ordering::Relaxed)
                        ))
                    }),
                    None,
                ));
            }
        }
        // pids
        if ctl & CTRL_PIDS != 0 && !root {
            let c = cg.clone();
            v.push(("pids.current", 0o444, alloc::boxed::Box::new(move || text(format!("{}\n", c.pids_current.load(Ordering::Relaxed)))), None));
            let (c, c2) = (cg.clone(), cg.clone());
            v.push((
                "pids.max",
                0o644,
                alloc::boxed::Box::new(move || text(format!("{}\n", max_or(c.pids_max.load(Ordering::Relaxed))))),
                Some(alloc::boxed::Box::new(move |d: &[u8]| {
                    c2.pids_max.store(parse_max(d)?, Ordering::Relaxed);
                    Ok(())
                })),
            ));
            let c = cg.clone();
            v.push(("pids.events", 0o444, alloc::boxed::Box::new(move || text(format!("max {}\n", c.pids_events_max.load(Ordering::Relaxed)))), None));
        }
        v
    }

    fn file_inode(&self, name: &'static str, mode: u32, g: Gen, w: Option<Writer>) -> Arc<dyn Inode> {
        let id = format!("{}", self.cg.ino);
        file(self.dev, ino_of(&[b"cg", id.as_bytes(), name.as_bytes()]), mode, g, w)
    }
}

fn populated(c: &Arc<Cgroup>) -> bool {
    !c.procs.lock().is_empty() || c.children.lock().values().any(populated)
}

fn descendants(c: &Arc<Cgroup>) -> usize {
    c.children.lock().values().map(|k| 1 + descendants(k)).sum()
}

impl Inode for CgDir {
    fn stat(&self) -> Result<Stat> {
        let mut s = pseudo::meta_stat(self.dev, self.cg.ino, S_IFDIR | 0o755, 0);
        s.nlink = 2 + self.cg.children.lock().len() as u32;
        Ok(s)
    }
    fn setattr(&self, _a: &SetAttr) -> Result<()> {
        Ok(())
    }
    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>> {
        if let Some(c) = self.cg.children.lock().get(name).cloned() {
            return Ok(Arc::new(CgDir { cg: c, dev: self.dev }));
        }
        let f = self.files().into_iter().find(|(n, ..)| n.as_bytes() == name).ok_or(ENOENT)?;
        Ok(self.file_inode(f.0, f.1, f.2, f.3))
    }
    /// mkdir: a new child cgroup.
    fn create(&self, name: &[u8], mode: u32, _rdev: u64, _uid: u32, _gid: u32) -> Result<Arc<dyn Inode>> {
        if mode & S_IFMT != S_IFDIR {
            return Err(EPERM);
        }
        if name.contains(&b'.') && self.files().iter().any(|(n, ..)| n.as_bytes() == name) {
            return Err(EEXIST);
        }
        let c = self.cg.mkdir(name)?;
        Ok(Arc::new(CgDir { cg: c, dev: self.dev }))
    }
    fn rmdir(&self, name: &[u8]) -> Result<()> {
        self.cg.rmdir(name)
    }
    fn unlink(&self, _name: &[u8]) -> Result<()> {
        Err(EPERM)
    }
    fn readdir(&self, pos: u64, f: &mut dyn FnMut(&[u8], u64, u8, u64) -> bool) -> Result<()> {
        let mut all: Vec<(Vec<u8>, u64, u8)> = alloc::vec![(b".".to_vec(), self.cg.ino, DT_DIR), (b"..".to_vec(), 1, DT_DIR)];
        for (n, c) in self.cg.children.lock().iter() {
            all.push((n.clone(), c.ino, DT_DIR));
        }
        let id = format!("{}", self.cg.ino);
        for (n, ..) in self.files() {
            all.push((n.as_bytes().to_vec(), ino_of(&[b"cg", id.as_bytes(), n.as_bytes()]), DT_REG));
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

/// The cgroup at `path` (from the hierarchy's root), if it exists.
pub fn lookup_path(path: &[u8]) -> Option<Arc<Cgroup>> {
    let mut c = cgroup::root();
    for part in path.split(|&b| b == b'/').filter(|p| !p.is_empty()) {
        let next = c.children.lock().get(part).cloned()?;
        c = next;
    }
    Some(c)
}

/// /proc/PID/cgroup: "0::PATH", relative to the reader's cgroup namespace root.
pub fn proc_cgroup(p: &crate::task::Process) -> Vec<u8> {
    let path = cgroup::path_of(p);
    let root = crate::task::try_current_proc().map(|r| r.ns.lock().cgroup.root.clone()).unwrap_or_else(|| Vec::from(&b"/"[..]));
    let rel = if root == b"/" {
        path
    } else if path == root {
        Vec::from(&b"/"[..])
    } else if path.starts_with(&root) && path.get(root.len()) == Some(&b'/') {
        path[root.len()..].to_vec()
    } else {
        // outside the namespace: shown relative to it with "/.." steps, as Linux
        let mut v = Vec::from(&b"/.."[..]);
        v.extend_from_slice(&path);
        v
    };
    let mut out = Vec::from(&b"0::"[..]);
    out.extend_from_slice(&rel);
    out.push(b'\n');
    out
}
