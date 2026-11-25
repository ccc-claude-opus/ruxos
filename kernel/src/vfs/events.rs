//! Event descriptors: epoll, eventfd, signalfd, pidfd.
use super::file::{File, FileOps};
use super::poll::*;
use super::*;
use crate::mm::vm::write_user;
use crate::sched::WaitResult;
use alloc::collections::VecDeque;
use alloc::sync::Weak;
use core::sync::atomic::{AtomicU64, Ordering};

pub const EPOLLIN: u32 = 0x1;
pub const EPOLLOUT: u32 = 0x4;
pub const EPOLLERR: u32 = 0x8;
pub const EPOLLHUP: u32 = 0x10;
pub const EPOLLRDHUP: u32 = 0x2000;
pub const EPOLLEXCLUSIVE: u32 = 1 << 28;
pub const EPOLLWAKEUP: u32 = 1 << 29;
pub const EPOLLONESHOT: u32 = 1 << 30;
pub const EPOLLET: u32 = 1 << 31;

type Key = (i32, usize);

struct Item {
    file: Weak<File>,
    events: u32,
    data: u64,
    watcher: Arc<ItemWatcher>,
    queued: bool,
    /// what the last harvest saw (edge-triggered lost-wakeup check)
    last: u32,
}

struct ItemWatcher {
    ep: Weak<Epoll>,
    key: Key,
}

impl Watcher for ItemWatcher {
    fn notify(&self, _events: u32) {
        if let Some(ep) = self.ep.upgrade() {
            ep.make_ready(self.key);
        }
    }
}

pub struct Epoll {
    items: SpinLock<BTreeMap<Key, Item>>,
    ready: SpinLock<VecDeque<Key>>,
    pub pq: PollQueue,
    me: Weak<Epoll>,
    inode: Arc<dyn Inode>,
}

/// Every epoll instance, for the Ctrl-T check (ruxos.debug only).
static ALL: SpinLock<Vec<Weak<Epoll>>> = SpinLock::new(Vec::new());

fn kind(f: &File) -> &'static str {
    let a = f.ops.as_any();
    if let Some(s) = a.downcast_ref::<crate::net::SocketFile>() {
        let s = s.sock.as_any();
        if s.is::<crate::net::inet::InetSocket>() {
            return "inet";
        }
        if s.is::<crate::net::unix::UnixSocket>() {
            return "unix";
        }
        return "socket";
    }
    if a.is::<super::pipe::PipeEnd>() {
        return "pipe";
    }
    if a.is::<EventFd>() {
        return "eventfd";
    }
    if a.is::<EpollFile>() {
        return "epoll";
    }
    "other"
}

/// Ctrl-T: epoll instances with sleepers and an empty ready list although a watched file is ready
/// (a lost wakeup).
pub fn check_lost_wakeups() {
    let eps: Vec<Arc<Epoll>> = {
        let mut a = ALL.lock();
        a.retain(|w| w.strong_count() > 0);
        a.iter().filter_map(|w| w.upgrade()).collect()
    };
    let mut n = 0;
    for ep in eps {
        let Some(items) = ep.items.try_lock() else { continue };
        let ready = ep.ready.try_lock().map_or(usize::MAX, |r| r.len());
        let sleepers = !ep.pq.wq.is_empty();
        for (key, it) in items.iter() {
            let Some(f) = it.file.upgrade() else { continue };
            let mask = (it.events & !(EPOLLET | EPOLLONESHOT | EPOLLEXCLUSIVE | EPOLLWAKEUP)) | EPOLLERR | EPOLLHUP;
            if it.events & !(EPOLLET | EPOLLONESHOT | EPOLLEXCLUSIVE | EPOLLWAKEUP) == 0 {
                continue;
            }
            let r = f.ops.poll(&f) & mask;
            let lost = if it.events & EPOLLET != 0 { r & !it.last != 0 } else { r != 0 };
            if lost && !it.queued {
                println!("epoll {:p}: fd {} ({}) ready {:#x} (last seen {:#x}) events {:#x} not queued; ready list {} sleepers {}", Arc::as_ptr(&ep), key.0, kind(&f), r, it.last, it.events, ready, sleepers);
                n += 1;
            }
        }
    }
    println!("epoll: {} ready-but-unqueued items", n);
}

impl Epoll {
    pub fn new() -> Arc<Epoll> {
        let ep = Arc::new_cyclic(|me| Epoll {
            items: SpinLock::new(BTreeMap::new()),
            ready: SpinLock::new(VecDeque::new()),
            pq: PollQueue::new(),
            me: me.clone(),
            inode: super::anon::AnonInode::new(0o600),
        });
        if crate::kernel_param("ruxos.debug").is_some() {
            let mut a = ALL.lock();
            if a.len() >= 4096 {
                a.retain(|w| w.strong_count() > 0);
            }
            a.push(Arc::downgrade(&ep));
        }
        ep
    }

    fn make_ready(&self, key: Key) {
        {
            let mut items = self.items.lock();
            let Some(it) = items.get_mut(&key) else { return };
            if it.queued {
                return;
            }
            it.queued = true;
        }
        self.ready.lock().push_back(key);
        self.pq.notify(POLLIN);
    }

    pub fn ctl(&self, op: u32, fd: i32, file: &Arc<File>, events: u32, data: u64) -> Result<()> {
        let key = (fd, Arc::as_ptr(file) as usize);
        match op {
            1 => {
                // EPOLL_CTL_ADD
                if self.items.lock().contains_key(&key) {
                    return Err(EEXIST);
                }
                if file.ops.as_any().downcast_ref::<EpollFile>().is_some_and(|e| Arc::ptr_eq(&e.ep, &self.me.upgrade().unwrap())) {
                    return Err(EINVAL);
                }
                let w = Arc::new(ItemWatcher { ep: self.me.clone(), key });
                if let Some(q) = file.ops.poll_queue() {
                    let ww: Arc<dyn Watcher> = w.clone();
                    q.watch(Arc::downgrade(&ww));
                }
                self.items.lock().insert(key, Item { file: Arc::downgrade(file), events, data, watcher: w, queued: false, last: 0 });
                self.make_ready(key);
            }
            2 => {
                // EPOLL_CTL_DEL
                let it = self.items.lock().remove(&key).ok_or(ENOENT)?;
                if let Some(q) = file.ops.poll_queue() {
                    let ww: Arc<dyn Watcher> = it.watcher.clone();
                    q.unwatch(&Arc::downgrade(&ww));
                }
            }
            3 => {
                // EPOLL_CTL_MOD
                {
                    let mut items = self.items.lock();
                    let it = items.get_mut(&key).ok_or(ENOENT)?;
                    it.events = events;
                    it.data = data;
                }
                self.make_ready(key);
            }
            _ => return Err(EINVAL),
        }
        Ok(())
    }

    /// Collects up to `max` ready events.
    fn harvest(&self, max: usize) -> Vec<(u32, u64)> {
        let mut out = Vec::new();
        let mut requeue = Vec::new();
        let n = self.ready.lock().len();
        for _ in 0..n {
            if out.len() >= max {
                break;
            }
            let Some(key) = self.ready.lock().pop_front() else { break };
            let (file, events, data) = {
                let mut items = self.items.lock();
                let Some(it) = items.get_mut(&key) else { continue };
                it.queued = false;
                (it.file.upgrade(), it.events, it.data)
            };
            let Some(file) = file else {
                self.items.lock().remove(&key);
                continue;
            };
            let mask = (events & !(EPOLLET | EPOLLONESHOT | EPOLLEXCLUSIVE | EPOLLWAKEUP)) | EPOLLERR | EPOLLHUP;
            let r = file.ops.poll(&file) & mask;
            if let Some(it) = self.items.lock().get_mut(&key) {
                it.last = r;
            }
            if r == 0 || events & !(EPOLLET | EPOLLONESHOT | EPOLLEXCLUSIVE | EPOLLWAKEUP) == 0 && events & EPOLLONESHOT != 0 {
                continue;
            }
            out.push((r, data));
            let mut items = self.items.lock();
            if let Some(it) = items.get_mut(&key) {
                if events & EPOLLONESHOT != 0 {
                    it.events &= EPOLLET | EPOLLONESHOT;
                } else if events & EPOLLET == 0 {
                    // level triggered: stays ready until it reports nothing
                    it.queued = true;
                    requeue.push(key);
                }
            }
        }
        if !requeue.is_empty() {
            self.ready.lock().extend(requeue);
        }
        out
    }

    pub fn wait(&self, max: usize, deadline: Option<u64>) -> Result<Vec<(u32, u64)>> {
        loop {
            let ev = self.harvest(max);
            if !ev.is_empty() {
                return Ok(ev);
            }
            if deadline.is_some_and(|d| crate::arch::ticks() >= d) {
                return Ok(ev);
            }
            match self.pq.wq.wait_until(|| !self.ready.lock().is_empty(), deadline, true) {
                WaitResult::Interrupted => return Err(EINTR),
                WaitResult::TimedOut => return Ok(Vec::new()),
                WaitResult::Ok => {}
            }
        }
    }
}

pub struct EpollFile {
    pub ep: Arc<Epoll>,
}

impl FileOps for EpollFile {
    fn poll(&self, _f: &File) -> u32 {
        // ready if a harvest would report something (checked without consuming)
        let keys: Vec<Key> = self.ep.ready.lock().iter().copied().collect();
        for k in keys {
            let (file, events) = {
                let items = self.ep.items.lock();
                match items.get(&k) {
                    Some(it) => (it.file.upgrade(), it.events),
                    None => continue,
                }
            };
            if let Some(f) = file {
                if f.ops.poll(&f) & (events | EPOLLERR | EPOLLHUP) != 0 {
                    return POLLIN;
                }
            }
        }
        0
    }
    fn poll_queue(&self) -> Option<&PollQueue> {
        Some(&self.ep.pq)
    }
    fn inode(&self) -> Option<Arc<dyn Inode>> {
        Some(self.ep.inode.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ---- eventfd

pub struct EventFd {
    count: SpinLock<u64>,
    semaphore: bool,
    pq: PollQueue,
    inode: Arc<dyn Inode>,
}

impl EventFd {
    pub fn new(init: u64, semaphore: bool) -> Arc<EventFd> {
        Arc::new(EventFd { count: SpinLock::new(init), semaphore, pq: PollQueue::new(), inode: super::anon::AnonInode::new(0o600) })
    }
}

impl FileOps for EventFd {
    fn read(&self, f: &File, buf: &mut [u8]) -> Result<usize> {
        if buf.len() < 8 {
            return Err(EINVAL);
        }
        loop {
            {
                let mut c = self.count.lock();
                if *c > 0 {
                    let v = if self.semaphore { 1 } else { *c };
                    *c -= v;
                    drop(c);
                    buf[..8].copy_from_slice(&v.to_le_bytes());
                    self.pq.notify(POLLOUT);
                    return Ok(8);
                }
            }
            if f.nonblock() {
                return Err(EAGAIN);
            }
            if let WaitResult::Interrupted = self.pq.wq.wait_until(|| *self.count.lock() > 0, None, true) {
                return Err(ERESTARTSYS);
            }
        }
    }
    fn write(&self, f: &File, buf: &[u8]) -> Result<usize> {
        if buf.len() < 8 {
            return Err(EINVAL);
        }
        let v = u64::from_le_bytes(buf[..8].try_into().unwrap());
        if v == u64::MAX {
            return Err(EINVAL);
        }
        loop {
            {
                let mut c = self.count.lock();
                if u64::MAX - 1 - *c >= v {
                    *c += v;
                    drop(c);
                    self.pq.notify(POLLIN);
                    return Ok(8);
                }
            }
            if f.nonblock() {
                return Err(EAGAIN);
            }
            if let WaitResult::Interrupted = self.pq.wq.wait_until(|| u64::MAX - 1 - *self.count.lock() >= v, None, true) {
                return Err(ERESTARTSYS);
            }
        }
    }
    fn poll(&self, _f: &File) -> u32 {
        let c = *self.count.lock();
        (if c > 0 { POLLIN | POLLRDNORM } else { 0 }) | (if c < u64::MAX - 1 { POLLOUT | POLLWRNORM } else { 0 })
    }
    fn poll_queue(&self) -> Option<&PollQueue> {
        Some(&self.pq)
    }
    fn inode(&self) -> Option<Arc<dyn Inode>> {
        Some(self.inode.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ---- signalfd

pub struct SignalFd {
    pub mask: AtomicU64,
    pub pq: Arc<PollQueue>,
    inode: Arc<dyn Inode>,
}

impl SignalFd {
    pub fn new(mask: u64) -> Arc<SignalFd> {
        let sfd = Arc::new(SignalFd { mask: AtomicU64::new(mask), pq: Arc::new(PollQueue::new()), inode: super::anon::AnonInode::new(0o600) });
        crate::task::current_proc().signalfds.lock().push(Arc::downgrade(&sfd.pq));
        sfd
    }

    fn take(&self) -> Option<crate::task::signal::SigInfo> {
        let th = crate::task::current_thread();
        let blocked = !self.mask.load(Ordering::Relaxed);
        th.pending.lock().take(blocked).or_else(|| th.proc.pending.lock().take(blocked))
    }

    fn pending(&self) -> bool {
        let Some(th) = crate::task::try_current_thread() else { return false };
        let m = self.mask.load(Ordering::Relaxed);
        (th.pending.lock().set | th.proc.pending.lock().set) & m != 0
    }
}

impl FileOps for SignalFd {
    fn read(&self, f: &File, buf: &mut [u8]) -> Result<usize> {
        if buf.len() < 128 {
            return Err(EINVAL);
        }
        let mut n = 0;
        loop {
            while n + 128 <= buf.len() {
                let Some(i) = self.take() else { break };
                let mut b = [0u8; 128];
                b[0..4].copy_from_slice(&i.signo.to_le_bytes());
                b[4..8].copy_from_slice(&i.errno.to_le_bytes());
                b[8..12].copy_from_slice(&i.code.to_le_bytes());
                b[12..16].copy_from_slice(&(i.pid as u32).to_le_bytes());
                b[16..20].copy_from_slice(&i.uid.to_le_bytes());
                b[20..24].copy_from_slice(&i.fd.to_le_bytes());
                b[44..48].copy_from_slice(&i.status.to_le_bytes());
                b[56..64].copy_from_slice(&(i.value as u64).to_le_bytes());
                b[72..80].copy_from_slice(&(i.addr as u64).to_le_bytes());
                buf[n..n + 128].copy_from_slice(&b);
                n += 128;
            }
            if n > 0 {
                return Ok(n);
            }
            if f.nonblock() {
                return Err(EAGAIN);
            }
            if let WaitResult::Interrupted = self.pq.wq.wait_until(|| self.pending(), None, true) {
                return Err(ERESTARTSYS);
            }
        }
    }
    fn poll(&self, _f: &File) -> u32 {
        if self.pending() { POLLIN | POLLRDNORM } else { 0 }
    }
    fn poll_queue(&self) -> Option<&PollQueue> {
        Some(&self.pq)
    }
    fn inode(&self) -> Option<Arc<dyn Inode>> {
        Some(self.inode.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ---- pidfd

pub struct PidFd {
    pub proc: Arc<crate::task::Process>,
    inode: Arc<dyn Inode>,
}

impl PidFd {
    pub fn new(proc: Arc<crate::task::Process>) -> Arc<PidFd> {
        Arc::new(PidFd { proc, inode: super::anon::AnonInode::new(0o600) })
    }
}

impl FileOps for PidFd {
    fn poll(&self, _f: &File) -> u32 {
        if self.proc.is_zombie() { POLLIN | POLLRDNORM } else { 0 }
    }
    fn poll_queue(&self) -> Option<&PollQueue> {
        Some(&self.proc.pidfd_wq)
    }
    fn inode(&self) -> Option<Arc<dyn Inode>> {
        Some(self.inode.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub fn anon_name(f: &File) -> &'static str {
    let a = f.ops.as_any();
    if a.is::<EpollFile>() {
        "eventpoll"
    } else if a.is::<EventFd>() {
        "eventfd"
    } else if a.is::<SignalFd>() {
        "signalfd"
    } else if a.is::<PidFd>() {
        "pidfd"
    } else {
        "unknown"
    }
}

/// epoll_pwait's output: struct epoll_event is 16 bytes on arm64 and riscv64 (not packed).
pub fn write_events(buf: usize, ev: &[(u32, u64)]) -> Result<()> {
    for (i, (e, d)) in ev.iter().enumerate() {
        write_user(buf + 16 * i, &[*e as u64, *d])?;
    }
    Ok(())
}
