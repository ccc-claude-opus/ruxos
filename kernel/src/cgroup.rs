//! cgroup v2 (Documentation/admin-guide/cgroup-v2.rst): one hierarchy, every process in exactly one
//! cgroup, inherited across fork. Controllers: memory (memory.max), pids (pids.max), cpu (cpu.max).
use crate::errno::*;
use crate::sync::SpinLock;
use crate::task::Process;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub const CTRL_CPU: u32 = 1;
pub const CTRL_MEMORY: u32 = 2;
pub const CTRL_PIDS: u32 = 4;
pub const CTRL_ALL: u32 = CTRL_CPU | CTRL_MEMORY | CTRL_PIDS;
pub const CTRL_NAMES: [(u32, &str); 3] = [(CTRL_CPU, "cpu"), (CTRL_MEMORY, "memory"), (CTRL_PIDS, "pids")];

pub struct Cgroup {
    pub ino: u64,
    pub name: Vec<u8>,
    pub parent: Option<Arc<Cgroup>>,
    pub children: SpinLock<BTreeMap<Vec<u8>, Arc<Cgroup>>>,
    /// the processes in this cgroup (global pids)
    pub procs: SpinLock<Vec<u32>>,
    /// controllers enabled for the children (cgroup.subtree_control)
    pub subtree_control: AtomicU32,
    /// memory: limit and usage in bytes (memory.max, memory.current), and memory.events max/oom_kill
    pub memory_max: AtomicU64,
    pub memory_current: AtomicU64,
    pub memory_events_max: AtomicU64,
    pub memory_events_oom_kill: AtomicU64,
    /// pids: limit and tasks (pids.max, pids.current), pids.events max
    pub pids_max: AtomicU64,
    pub pids_current: AtomicU64,
    pub pids_events_max: AtomicU64,
    /// cpu: quota per period in microseconds (cpu.max; quota u64::MAX = max), and the run time
    /// used in the current period and in total (ns)
    pub cpu_quota_us: AtomicU64,
    pub cpu_period_us: AtomicU64,
    pub cpu_used_ns: AtomicU64,
    pub cpu_period_start: AtomicU64,
    pub cpu_usage_ns: AtomicU64,
    pub cpu_nr_throttled: AtomicU64,
    pub cpu_throttled_ns: AtomicU64,
    pub cpu_nr_periods: AtomicU64,
    /// when the quota of the current period ran out (0: it has not)
    pub cpu_throttle_start: AtomicU64,
}

static NEXT_INO: AtomicU64 = AtomicU64::new(0x100);

impl Cgroup {
    fn new(name: &[u8], parent: Option<Arc<Cgroup>>) -> Arc<Cgroup> {
        Arc::new(Cgroup {
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
            name: name.to_vec(),
            parent,
            children: SpinLock::new(BTreeMap::new()),
            procs: SpinLock::new(Vec::new()),
            subtree_control: AtomicU32::new(0),
            memory_max: AtomicU64::new(u64::MAX),
            memory_current: AtomicU64::new(0),
            memory_events_max: AtomicU64::new(0),
            memory_events_oom_kill: AtomicU64::new(0),
            pids_max: AtomicU64::new(u64::MAX),
            pids_current: AtomicU64::new(0),
            pids_events_max: AtomicU64::new(0),
            cpu_quota_us: AtomicU64::new(u64::MAX),
            cpu_period_us: AtomicU64::new(100_000),
            cpu_used_ns: AtomicU64::new(0),
            cpu_period_start: AtomicU64::new(0),
            cpu_usage_ns: AtomicU64::new(0),
            cpu_nr_throttled: AtomicU64::new(0),
            cpu_throttled_ns: AtomicU64::new(0),
            cpu_nr_periods: AtomicU64::new(0),
            cpu_throttle_start: AtomicU64::new(0),
        })
    }

    pub fn is_root(&self) -> bool {
        self.parent.is_none()
    }

    /// The controllers available here (cgroup.controllers): all at the root, else the parent's
    /// subtree_control.
    pub fn controllers(&self) -> u32 {
        match &self.parent {
            None => CTRL_ALL,
            Some(p) => p.subtree_control.load(Ordering::Relaxed),
        }
    }

    /// The path from the root ("/" for the root).
    pub fn path(self: &Arc<Self>) -> Vec<u8> {
        let mut parts = Vec::new();
        let mut c = self.clone();
        while let Some(p) = c.parent.clone() {
            parts.push(c.name.clone());
            c = p;
        }
        let mut out = Vec::new();
        for p in parts.iter().rev() {
            out.push(b'/');
            out.extend_from_slice(p);
        }
        if out.is_empty() {
            out.push(b'/');
        }
        out
    }

    /// This cgroup and its ancestors.
    fn chain(self: &Arc<Self>) -> Vec<Arc<Cgroup>> {
        let mut v = Vec::new();
        let mut c = Some(self.clone());
        while let Some(x) = c {
            c = x.parent.clone();
            v.push(x);
        }
        v
    }

    pub fn mkdir(self: &Arc<Self>, name: &[u8]) -> Result<Arc<Cgroup>> {
        let mut ch = self.children.lock();
        if ch.contains_key(name) {
            return Err(EEXIST);
        }
        let c = Cgroup::new(name, Some(self.clone()));
        ch.insert(name.to_vec(), c.clone());
        Ok(c)
    }

    pub fn rmdir(self: &Arc<Self>, name: &[u8]) -> Result<()> {
        let mut ch = self.children.lock();
        let c = ch.get(name).ok_or(ENOENT)?;
        if !c.procs.lock().is_empty() || !c.children.lock().is_empty() {
            return Err(EBUSY);
        }
        ch.remove(name);
        Ok(())
    }

    /// Writes cgroup.subtree_control ("+memory -cpu ..."). A non-root cgroup with processes cannot
    /// give controllers to its children (the "no internal processes" rule).
    pub fn set_subtree_control(&self, text: &[u8]) -> Result<()> {
        let mut v = self.subtree_control.load(Ordering::Relaxed);
        for w in text.split(|c| c.is_ascii_whitespace()).filter(|w| !w.is_empty()) {
            let (on, name) = match w[0] {
                b'+' => (true, &w[1..]),
                b'-' => (false, &w[1..]),
                _ => return Err(EINVAL),
            };
            let bit = CTRL_NAMES.iter().find(|(_, n)| n.as_bytes() == name).map(|(b, _)| *b).ok_or(EINVAL)?;
            if on {
                if self.controllers() & bit == 0 {
                    return Err(ENOENT);
                }
                v |= bit;
            } else {
                // a child still using it
                if self.children.lock().values().any(|c| c.subtree_control.load(Ordering::Relaxed) & bit != 0) {
                    return Err(EBUSY);
                }
                v &= !bit;
            }
        }
        if v != 0 && !self.is_root() && !self.procs.lock().is_empty() {
            return Err(EBUSY);
        }
        self.subtree_control.store(v, Ordering::Relaxed);
        Ok(())
    }
}

pub static ROOT: crate::sync::Once<Arc<Cgroup>> = crate::sync::Once::new();

pub fn root() -> Arc<Cgroup> {
    ROOT.call_once(|| Cgroup::new(b"", None)).clone()
}

/// Which cgroup each process is in (global pid -> cgroup).
static MEMBER: SpinLock<BTreeMap<u32, Arc<Cgroup>>> = SpinLock::new(BTreeMap::new());

pub fn of_pid(pid: u32) -> Arc<Cgroup> {
    MEMBER.lock().get(&pid).cloned().unwrap_or_else(root)
}

pub fn of(p: &Process) -> Arc<Cgroup> {
    of_pid(p.pid)
}

/// /proc/PID/cgroup's path (relative to the reader's cgroup namespace root is the caller's job).
pub fn path_of(p: &Process) -> Vec<u8> {
    of(p).path()
}

fn threads_of(p: &Process) -> u64 {
    p.live_threads.load(Ordering::Relaxed).max(1) as u64
}

/// The first process: in the root cgroup.
pub fn join_root(p: &Arc<Process>) {
    let r = root();
    r.procs.lock().push(p.pid);
    r.pids_current.fetch_add(1, Ordering::Relaxed);
    MEMBER.lock().insert(p.pid, r);
}

/// A new process (fork): in its parent's cgroup.
pub fn fork(parent: &Process, child: &Arc<Process>) {
    let cg = of(parent);
    cg.procs.lock().push(child.pid);
    MEMBER.lock().insert(child.pid, cg);
}

/// A reaped process leaves its cgroup.
pub fn exit(p: &Process) {
    if let Some(cg) = MEMBER.lock().remove(&p.pid) {
        cg.procs.lock().retain(|&x| x != p.pid);
    }
}

/// pids controller: may `p` get one more task? (charged until `task_gone`)
pub fn charge_task(p: &Process) -> Result<()> {
    let chain = of(p).chain();
    for c in &chain {
        if c.pids_current.load(Ordering::Relaxed) + 1 > c.pids_max.load(Ordering::Relaxed) {
            c.pids_events_max.fetch_add(1, Ordering::Relaxed);
            return Err(EAGAIN);
        }
    }
    for c in &chain {
        c.pids_current.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

pub fn task_gone(p: &Process) {
    for c in of(p).chain() {
        let _ = c.pids_current.try_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(v.saturating_sub(1)));
    }
}

/// Moves process `p` (all its threads) to `to` (a write to cgroup.procs). The target must not give
/// controllers to children unless it is the root.
pub fn attach(p: &Arc<Process>, to: &Arc<Cgroup>) -> Result<()> {
    if !to.is_root() && to.subtree_control.load(Ordering::Relaxed) != 0 {
        return Err(EBUSY);
    }
    let from = of(p);
    if Arc::ptr_eq(&from, to) {
        return Ok(());
    }
    let n = threads_of(p);
    for c in from.chain() {
        let _ = c.pids_current.try_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(v.saturating_sub(n)));
    }
    for c in to.chain() {
        c.pids_current.fetch_add(n, Ordering::Relaxed);
    }
    // the address space's pages follow the process
    if let Some(mm) = p.leader().and_then(|l| crate::task::thread_of(&l).map(|t| t.mm.clone())) {
        mm.set_memcg(Some(to.clone()));
    }
    from.procs.lock().retain(|&x| x != p.pid);
    to.procs.lock().push(p.pid);
    MEMBER.lock().insert(p.pid, to.clone());
    Ok(())
}

/// Weak handle for code that must not keep a cgroup alive.
pub fn weak(c: &Arc<Cgroup>) -> Weak<Cgroup> {
    Arc::downgrade(c)
}

/// memory controller: charges `bytes` to `cg` and its ancestors, or fails with ENOMEM (an OOM kill
/// in this cgroup: the faulting process is killed) when a memory.max would be exceeded.
pub fn charge_mem(cg: &Arc<Cgroup>, bytes: u64) -> Result<()> {
    let chain = cg.chain();
    for (i, c) in chain.iter().enumerate() {
        let now = c.memory_current.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if now > c.memory_max.load(Ordering::Relaxed) {
            for d in &chain[..=i] {
                d.memory_current.fetch_sub(bytes, Ordering::Relaxed);
            }
            c.memory_events_max.fetch_add(1, Ordering::Relaxed);
            c.memory_events_oom_kill.fetch_add(1, Ordering::Relaxed);
            return Err(ENOMEM);
        }
    }
    Ok(())
}

pub fn force_charge_mem(cg: &Arc<Cgroup>, bytes: u64) {
    for c in cg.chain() {
        c.memory_current.fetch_add(bytes, Ordering::Relaxed);
    }
}

pub fn uncharge_mem(cg: &Arc<Cgroup>, bytes: u64) {
    for c in cg.chain() {
        let _ = c.memory_current.try_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(v.saturating_sub(bytes)));
    }
}

/// How many cgroups have a cpu.max quota (none: the scheduler skips the checks).
pub static CPU_LIMITED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

impl Cgroup {
    /// Starts a new period when the current one is over (cpu.max bandwidth).
    fn cpu_refresh(&self, now: u64) {
        let period = self.cpu_period_us.load(Ordering::Relaxed) * 1000;
        let start = self.cpu_period_start.load(Ordering::Relaxed);
        if now >= start + period {
            let n = (now - start) / period;
            let new_start = start + n * period;
            self.cpu_period_start.store(new_start, Ordering::Relaxed);
            self.cpu_used_ns.store(0, Ordering::Relaxed);
            self.cpu_nr_periods.fetch_add(1, Ordering::Relaxed);
            // the throttled time of the period that ended
            let ts = self.cpu_throttle_start.swap(0, Ordering::Relaxed);
            if ts != 0 {
                self.cpu_throttled_ns.fetch_add((start + period).saturating_sub(ts), Ordering::Relaxed);
            }
            let _ = new_start;
        }
    }

    pub fn set_cpu_max(&self, quota_us: u64, period_us: u64) {
        let was = self.cpu_quota_us.load(Ordering::Relaxed) != u64::MAX;
        self.cpu_period_us.store(period_us, Ordering::Relaxed);
        self.cpu_quota_us.store(quota_us, Ordering::Relaxed);
        let is = quota_us != u64::MAX;
        if is && !was {
            CPU_LIMITED.fetch_add(1, Ordering::Relaxed);
        } else if was && !is {
            CPU_LIMITED.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// `ns` nanoseconds of CPU time used by a task of `p`.
pub fn charge_cpu(p: &Process, ns: u64) {
    let now = crate::time::monotonic_ns();
    for c in of(p).chain() {
        c.cpu_usage_ns.fetch_add(ns, Ordering::Relaxed);
        if c.cpu_quota_us.load(Ordering::Relaxed) != u64::MAX {
            c.cpu_refresh(now);
            c.cpu_used_ns.fetch_add(ns, Ordering::Relaxed);
        }
    }
}

/// cpu.max: if a cgroup of `p` has used its quota in this period, the nanoseconds until the next
/// period starts.
pub fn throttled(p: &Process) -> Option<u64> {
    if CPU_LIMITED.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let now = crate::time::monotonic_ns();
    let mut wait: Option<u64> = None;
    for c in of(p).chain() {
        let q = c.cpu_quota_us.load(Ordering::Relaxed);
        if q == u64::MAX {
            continue;
        }
        c.cpu_refresh(now);
        if c.cpu_used_ns.load(Ordering::Relaxed) >= q * 1000 {
            // the first time in this period: one more throttled period
            if c.cpu_throttle_start.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                c.cpu_nr_throttled.fetch_add(1, Ordering::Relaxed);
            }
            let end = c.cpu_period_start.load(Ordering::Relaxed) + c.cpu_period_us.load(Ordering::Relaxed) * 1000;
            let w = end.saturating_sub(now).max(1000);
            wait = Some(wait.map_or(w, |x| x.max(w)));
        }
    }
    wait
}

/// cpu.max: the CPU time `p` may still use in this period (None: no limit), so its time slice
/// can end when the quota does.
pub fn runtime_left(p: &Process) -> Option<u64> {
    if CPU_LIMITED.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let now = crate::time::monotonic_ns();
    let mut left: Option<u64> = None;
    for c in of(p).chain() {
        let q = c.cpu_quota_us.load(Ordering::Relaxed);
        if q == u64::MAX {
            continue;
        }
        c.cpu_refresh(now);
        let l = (q * 1000).saturating_sub(c.cpu_used_ns.load(Ordering::Relaxed));
        left = Some(left.map_or(l, |x| x.min(l)));
    }
    left
}
