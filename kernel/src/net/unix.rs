//! AF_UNIX sockets (unix(7)): stream, datagram and seqpacket; path, abstract and autobound
//! addresses; SCM_RIGHTS; SO_PEERCRED.
use super::*;
use crate::sched::WaitResult;
use crate::sync::SpinLock;
use crate::vfs::poll::*;
use crate::vfs::{S_IFMT, S_IFSOCK};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Weak;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const CAPACITY: usize = 212_992;

struct Seg {
    data: Vec<u8>,
    off: usize,
    fds: Vec<Arc<File>>,
    from: Option<Vec<u8>>,
    creds: (u32, u32, u32),
}

#[derive(Default)]
struct Rx {
    segs: VecDeque<Seg>,
    bytes: usize,
}

enum Conn {
    None,
    Listening { backlog: VecDeque<Arc<UnixSocket>>, max: usize },
    Connected { peer: Weak<UnixSocket> },
    /// A datagram socket with a default destination.
    DgramPeer { peer: Weak<UnixSocket> },
}

pub struct UnixSocket {
    ty: u32,
    me: Weak<UnixSocket>,
    conn: SpinLock<Conn>,
    rx: SpinLock<Rx>,
    local: SpinLock<Option<Vec<u8>>>,
    peer_addr: SpinLock<Option<Vec<u8>>>,
    pq: PollQueue,
    /// the peer is closed: reads see EOF once the queue is empty, writes fail with EPIPE
    peer_gone: AtomicBool,
    /// the peer shut down writing (shutdown(SHUT_WR)): reads see EOF, but we may still write
    peer_shut_wr: AtomicBool,
    shut_rd: AtomicBool,
    shut_wr: AtomicBool,
    /// pid, uid, gid of whoever created/connected the socket (SO_PEERCRED of the other side)
    cred: (AtomicU32, AtomicU32, AtomicU32),
    peer_cred: SpinLock<(u32, u32, u32)>,
    passcred: AtomicBool,
    /// path sockets: the inode they are bound to
    bound_inode: SpinLock<Option<usize>>,
    sndbuf: AtomicU32,
    rcvbuf: AtomicU32,
}

/// Every unix socket, for the Ctrl-T dump (ruxos.debug only).
static ALL: SpinLock<Vec<Weak<UnixSocket>>> = SpinLock::new(Vec::new());

/// Ctrl-T: unix sockets with unread data, and who could be waiting for it.
pub fn dump() {
    let all: Vec<Arc<UnixSocket>> = {
        let mut a = ALL.lock();
        a.retain(|w| w.strong_count() > 0);
        a.iter().filter_map(|w| w.upgrade()).collect()
    };
    let mut n = 0;
    for s in &all {
        let Some(rx) = s.rx.try_lock() else { continue };
        if rx.bytes == 0 {
            continue;
        }
        n += 1;
        let (sleepers, watchers) = s.pq.debug_counts();
        println!(
            "unix {:p}: type {} {} bytes in {} segments unread, pid {} (peer pid {}), sleepers {} epoll watchers {}",
            Arc::as_ptr(s),
            s.ty,
            rx.bytes,
            rx.segs.len(),
            s.cred.0.load(Ordering::Relaxed),
            s.peer_cred.try_lock().map_or(0, |c| c.0),
            sleepers,
            watchers
        );
    }
    println!("unix: {} sockets, {} with unread data", all.len(), n);
}

static BY_INODE: SpinLock<BTreeMap<usize, Weak<UnixSocket>>> = SpinLock::new(BTreeMap::new());
static ABSTRACT: SpinLock<BTreeMap<Vec<u8>, Weak<UnixSocket>>> = SpinLock::new(BTreeMap::new());

fn my_cred() -> (u32, u32, u32) {
    match crate::task::try_current_thread() {
        Some(t) => {
            let c = t.proc.cred();
            (t.proc.pid, c.euid, c.egid)
        }
        None => (0, 0, 0),
    }
}

/// struct sockaddr_un as bytes for an address (path without NUL, or abstract starting with 0).
fn sockaddr(name: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(2 + name.len() + 1);
    v.extend_from_slice(&AF_UNIX.to_le_bytes());
    v.extend_from_slice(name);
    if !name.is_empty() && name[0] != 0 {
        v.push(0);
    }
    v
}

/// The name part of a sockaddr_un.
fn parse(addr: &[u8]) -> Result<Vec<u8>> {
    if addr.len() < 2 || u16::from_le_bytes([addr[0], addr[1]]) != AF_UNIX {
        return Err(EINVAL);
    }
    let p = &addr[2..addr.len().min(110)];
    if p.is_empty() {
        return Ok(Vec::new());
    }
    if p[0] == 0 {
        return Ok(p.to_vec()); // abstract: every byte counts
    }
    let end = p.iter().position(|&c| c == 0).unwrap_or(p.len());
    Ok(p[..end].to_vec())
}

impl UnixSocket {
    pub fn new(ty: u32) -> Result<Arc<dyn Socket>> {
        Ok(Self::new_raw(ty)?)
    }

    fn new_raw(ty: u32) -> Result<Arc<UnixSocket>> {
        if !matches!(ty, SOCK_STREAM | SOCK_DGRAM | SOCK_SEQPACKET) {
            return Err(ESOCKTNOSUPPORT);
        }
        let (p, u, g) = my_cred();
        let s = Arc::new_cyclic(|me| UnixSocket {
            ty,
            me: me.clone(),
            conn: SpinLock::new(Conn::None),
            rx: SpinLock::new(Rx::default()),
            local: SpinLock::new(None),
            peer_addr: SpinLock::new(None),
            pq: PollQueue::new(),
            peer_gone: AtomicBool::new(false),
            peer_shut_wr: AtomicBool::new(false),
            shut_rd: AtomicBool::new(false),
            shut_wr: AtomicBool::new(false),
            cred: (AtomicU32::new(p), AtomicU32::new(u), AtomicU32::new(g)),
            peer_cred: SpinLock::new((0, u32::MAX, u32::MAX)),
            passcred: AtomicBool::new(false),
            bound_inode: SpinLock::new(None),
            sndbuf: AtomicU32::new(CAPACITY as u32),
            rcvbuf: AtomicU32::new(CAPACITY as u32),
        });
        if crate::kernel_param("ruxos.debug").is_some() {
            let mut a = ALL.lock();
            if a.len() >= 4096 {
                a.retain(|w| w.strong_count() > 0);
            }
            a.push(Arc::downgrade(&s));
        }
        Ok(s)
    }

    /// Two connected sockets (socketpair).
    pub fn pair(ty: u32) -> Result<(Arc<UnixSocket>, Arc<UnixSocket>)> {
        let a = Self::new_raw(ty)?;
        let b = Self::new_raw(ty)?;
        let ca = a.my_cred();
        *a.peer_cred.lock() = ca;
        *b.peer_cred.lock() = ca;
        if ty == SOCK_DGRAM {
            *a.conn.lock() = Conn::DgramPeer { peer: Arc::downgrade(&b) };
            *b.conn.lock() = Conn::DgramPeer { peer: Arc::downgrade(&a) };
        } else {
            *a.conn.lock() = Conn::Connected { peer: Arc::downgrade(&b) };
            *b.conn.lock() = Conn::Connected { peer: Arc::downgrade(&a) };
        }
        Ok((a, b))
    }

    fn my_cred(&self) -> (u32, u32, u32) {
        (self.cred.0.load(Ordering::Relaxed), self.cred.1.load(Ordering::Relaxed), self.cred.2.load(Ordering::Relaxed))
    }

    fn connection_oriented(&self) -> bool {
        self.ty != SOCK_DGRAM
    }

    fn peer(&self) -> Option<Arc<UnixSocket>> {
        match &*self.conn.lock() {
            Conn::Connected { peer } | Conn::DgramPeer { peer } => peer.upgrade(),
            _ => None,
        }
    }

    fn autobind(&self) {
        let mut l = self.local.lock();
        if l.is_some() {
            return;
        }
        loop {
            let n = crate::random::u64() & 0xfffff;
            let name = alloc::format!("\0{:05x}", n).into_bytes();
            let mut a = ABSTRACT.lock();
            if a.get(&name).and_then(|w| w.upgrade()).is_none() {
                a.insert(name.clone(), self.me.clone());
                *l = Some(name);
                return;
            }
        }
    }

    /// Finds the socket bound to `name`.
    fn lookup(name: &[u8]) -> Result<Arc<UnixSocket>> {
        if name.first() == Some(&0) {
            return ABSTRACT.lock().get(name).and_then(|w| w.upgrade()).ok_or(ECONNREFUSED);
        }
        let p = crate::syscall::fs::lookup_at(-100, name, true, false)?;
        let i = p.inode()?;
        if i.stat()?.mode & S_IFMT != S_IFSOCK {
            return Err(ECONNREFUSED);
        }
        crate::vfs::path::may(&i, 2, &crate::syscall::fs::cred())?;
        let key = Arc::as_ptr(&i) as *const u8 as usize;
        BY_INODE.lock().get(&key).and_then(|w| w.upgrade()).ok_or(ECONNREFUSED)
    }

    /// Nothing more will arrive (Linux: RCV_SHUTDOWN).
    fn rcv_shut(&self) -> bool {
        self.shut_rd.load(Ordering::SeqCst)
            || (self.connection_oriented() && (self.peer_gone.load(Ordering::SeqCst) || self.peer_shut_wr.load(Ordering::SeqCst)))
    }

    fn room(&self) -> usize {
        let cap = self.rcvbuf.load(Ordering::Relaxed) as usize;
        cap.saturating_sub(self.rx.lock().bytes)
    }

    fn deliver(&self, seg: Seg) {
        {
            let mut rx = self.rx.lock();
            rx.bytes += seg.data.len().max(1);
            rx.segs.push_back(seg);
        }
        self.pq.notify(POLLIN);
    }
}

impl Drop for UnixSocket {
    fn drop(&mut self) {
        if let Some(k) = self.bound_inode.lock().take() {
            BY_INODE.lock().remove(&k);
        }
        if let Some(n) = self.local.lock().take() {
            if n.first() == Some(&0) {
                let mut a = ABSTRACT.lock();
                if a.get(&n).is_some_and(|w| w.strong_count() == 0) {
                    a.remove(&n);
                }
            }
        }
        let peer = match &*self.conn.lock() {
            Conn::Connected { peer } => peer.upgrade(),
            _ => None,
        };
        if let Some(p) = peer {
            p.peer_gone.store(true, Ordering::SeqCst);
            p.pq.notify(POLLIN | POLLOUT | POLLHUP | POLLRDHUP);
        }
    }
}

impl Socket for UnixSocket {
    fn bind(&self, addr: &[u8]) -> Result<()> {
        let name = parse(addr)?;
        if self.local.lock().is_some() {
            return Err(EINVAL);
        }
        if name.is_empty() {
            self.autobind();
            return Ok(());
        }
        if name[0] == 0 {
            let mut a = ABSTRACT.lock();
            if a.get(&name).and_then(|w| w.upgrade()).is_some() {
                return Err(EADDRINUSE);
            }
            a.insert(name.clone(), self.me.clone());
        } else {
            // a socket node in the file system
            let th = crate::task::current_thread();
            let umask = th.fs.lock().umask;
            let c = crate::syscall::fs::cred();
            let r = th.fs.lock().root.clone();
            let st = crate::syscall::fs::start(-100)?;
            let (dir, last, _) = crate::vfs::path::Lookup { root: &r, cred: &c, follow_last: true, beneath: false, no_symlinks: false }.resolve_parent(&st, &name)?;
            let di = dir.inode()?;
            if dir.d.child(&last)?.inode.lock().is_some() {
                return Err(EADDRINUSE);
            }
            crate::vfs::path::may(&di, 3, &c)?;
            let i = di.create(&last, S_IFSOCK | (0o777 & !umask), 0, c.fsuid, c.fsgid)?;
            dir.d.set_child(&last, Some(i.clone()));
            let key = Arc::as_ptr(&i) as *const u8 as usize;
            BY_INODE.lock().insert(key, self.me.clone());
            *self.bound_inode.lock() = Some(key);
        }
        *self.local.lock() = Some(name);
        Ok(())
    }

    fn listen(&self, backlog: i32) -> Result<()> {
        if !self.connection_oriented() {
            return Err(EOPNOTSUPP);
        }
        if self.local.lock().is_none() {
            self.autobind();
        }
        let max = backlog.clamp(1, 4096) as usize;
        let mut c = self.conn.lock();
        match &mut *c {
            Conn::None => *c = Conn::Listening { backlog: VecDeque::new(), max },
            Conn::Listening { max: m, .. } => *m = max,
            _ => return Err(EINVAL),
        }
        Ok(())
    }

    fn accept(&self, nonblock: bool) -> Result<(Arc<dyn Socket>, Vec<u8>)> {
        loop {
            {
                let mut c = self.conn.lock();
                let Conn::Listening { backlog, .. } = &mut *c else { return Err(EINVAL) };
                if let Some(s) = backlog.pop_front() {
                    drop(c);
                    let addr = s.peer_addr.lock().clone().map(|n| sockaddr(&n)).unwrap_or_else(|| AF_UNIX.to_le_bytes().to_vec());
                    return Ok((s, addr));
                }
            }
            if nonblock {
                return Err(EAGAIN);
            }
            let r = self.pq.wq.wait_until(|| matches!(&*self.conn.lock(), Conn::Listening { backlog, .. } if !backlog.is_empty()), None, true);
            if let WaitResult::Interrupted = r {
                return Err(ERESTARTSYS);
            }
        }
    }

    fn connect(&self, addr: &[u8], nonblock: bool) -> Result<()> {
        let name = parse(addr)?;
        let target = Self::lookup(&name)?;
        if target.ty != self.ty {
            return Err(EPROTOTYPE);
        }
        if !self.connection_oriented() {
            *self.conn.lock() = Conn::DgramPeer { peer: Arc::downgrade(&target) };
            *self.peer_addr.lock() = Some(name);
            return Ok(());
        }
        if matches!(&*self.conn.lock(), Conn::Connected { .. }) {
            return Err(EISCONN);
        }
        loop {
            {
                let mut tc = target.conn.lock();
                let Conn::Listening { backlog, max } = &mut *tc else { return Err(ECONNREFUSED) };
                if backlog.len() < *max {
                    // the server side of the new connection
                    let server = Self::new_raw(self.ty)?;
                    let (tp, tu, tg) = target.my_cred();
                    server.cred.0.store(tp, Ordering::Relaxed);
                    server.cred.1.store(tu, Ordering::Relaxed);
                    server.cred.2.store(tg, Ordering::Relaxed);
                    *server.local.lock() = target.local.lock().clone();
                    *server.peer_cred.lock() = my_cred();
                    *server.peer_addr.lock() = self.local.lock().clone();
                    *server.conn.lock() = Conn::Connected { peer: self.me.clone() };
                    *self.peer_cred.lock() = (tp, tu, tg);
                    *self.peer_addr.lock() = Some(name.clone());
                    *self.conn.lock() = Conn::Connected { peer: Arc::downgrade(&server) };
                    backlog.push_back(server);
                    drop(tc);
                    target.pq.notify(POLLIN);
                    self.pq.notify(POLLOUT);
                    return Ok(());
                }
            }
            if nonblock {
                return Err(EAGAIN);
            }
            let r = target.pq.wq.wait_until(|| matches!(&*target.conn.lock(), Conn::Listening { backlog, max } if backlog.len() < *max), None, true);
            if let WaitResult::Interrupted = r {
                return Err(ERESTARTSYS);
            }
        }
    }

    fn send(&self, msg: Msg, flags: u32, nonblock: bool) -> Result<usize> {
        let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
        if self.shut_wr.load(Ordering::SeqCst) {
            if flags & MSG_NOSIGNAL == 0 {
                crate::task::send_signal_current(crate::task::signal::SIGPIPE);
            }
            return Err(EPIPE);
        }
        let target = match (&msg.addr, self.connection_oriented()) {
            (Some(a), false) => Self::lookup(&parse(a)?)?,
            (_, true) => match self.peer() {
                Some(p) => p,
                None => {
                    if matches!(&*self.conn.lock(), Conn::Connected { .. }) {
                        if flags & MSG_NOSIGNAL == 0 {
                            crate::task::send_signal_current(crate::task::signal::SIGPIPE);
                        }
                        return Err(EPIPE);
                    }
                    return Err(ENOTCONN);
                }
            },
            (None, false) => self.peer().ok_or(if matches!(&*self.conn.lock(), Conn::DgramPeer { .. }) { ECONNREFUSED } else { EDESTADDRREQ })?,
        };
        if target.shut_rd.load(Ordering::SeqCst)
            || (self.connection_oriented() && (self.peer_gone.load(Ordering::SeqCst) || self.shut_wr.load(Ordering::SeqCst)))
        {
            if flags & MSG_NOSIGNAL == 0 {
                crate::task::send_signal_current(crate::task::signal::SIGPIPE);
            }
            return Err(EPIPE);
        }
        if !self.connection_oriented() && self.local.lock().is_none() && target.passcred.load(Ordering::Relaxed) {
            self.autobind();
        }
        let from = self.local.lock().clone().map(|n| sockaddr(&n));
        let creds = msg.creds.unwrap_or_else(my_cred);
        let total = msg.data.len();
        let mut fds = Some(msg.fds);
        if self.ty == SOCK_STREAM {
            // byte stream: as much as fits, blocking for the rest
            let mut sent = 0;
            loop {
                let room = target.room();
                if room > 0 || total == 0 {
                    let n = (total - sent).min(room);
                    target.deliver(Seg { data: msg.data[sent..sent + n].to_vec(), off: 0, fds: fds.take().unwrap_or_default(), from: None, creds });
                    sent += n;
                    if sent == total {
                        return Ok(total);
                    }
                }
                if nonblock {
                    return if sent > 0 { Ok(sent) } else { Err(EAGAIN) };
                }
                let r = self.pq.wq.wait_until(|| target.room() > 0 || self.peer_gone.load(Ordering::SeqCst) || target.shut_rd.load(Ordering::SeqCst), None, true);
                if self.peer_gone.load(Ordering::SeqCst) {
                    return if sent > 0 { Ok(sent) } else { Err(EPIPE) };
                }
                if let WaitResult::Interrupted = r {
                    return if sent > 0 { Ok(sent) } else { Err(ERESTARTSYS) };
                }
            }
        }
        // datagrams and seqpackets: one record
        if total > target.rcvbuf.load(Ordering::Relaxed) as usize {
            return Err(EMSGSIZE);
        }
        loop {
            if target.room() >= total.max(1) {
                target.deliver(Seg { data: msg.data, off: 0, fds: fds.take().unwrap_or_default(), from, creds });
                return Ok(total);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            let r = self.pq.wq.wait_until(|| target.room() >= total.max(1), None, true);
            if let WaitResult::Interrupted = r {
                return Err(ERESTARTSYS);
            }
        }
    }

    fn recv(&self, len: usize, flags: u32, nonblock: bool) -> Result<Msg> {
        let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
        let peek = flags & MSG_PEEK != 0;
        loop {
            {
                let mut rx = self.rx.lock();
                if !rx.segs.is_empty() {
                    let mut out = Msg::default();
                    if self.ty != SOCK_STREAM {
                        let seg = if peek { None } else { rx.segs.pop_front() };
                        let s = seg.as_ref().unwrap_or_else(|| rx.segs.front().unwrap());
                        let n = s.data.len().min(len);
                        out.data = s.data[..n].to_vec();
                        if s.data.len() > n {
                            out.flags |= MSG_TRUNC;
                        }
                        out.addr = s.from.clone();
                        out.creds = Some(s.creds);
                        if !peek {
                            let s = seg.unwrap();
                            rx.bytes -= s.data.len().max(1);
                            out.fds = s.fds;
                        }
                    } else {
                        // stream: across segments, but not past a segment carrying descriptors
                        let mut i = 0;
                        while out.data.len() < len && i < rx.segs.len() {
                            if i > 0 && !rx.segs[i].fds.is_empty() {
                                break;
                            }
                            let take = (rx.segs[i].data.len() - rx.segs[i].off).min(len - out.data.len());
                            let (start, end) = (rx.segs[i].off, rx.segs[i].off + take);
                            out.data.extend_from_slice(&rx.segs[i].data[start..end]);
                            out.creds = Some(rx.segs[i].creds);
                            if !peek {
                                if out.fds.is_empty() {
                                    out.fds = core::mem::take(&mut rx.segs[i].fds);
                                }
                                rx.segs[i].off += take;
                            }
                            i += 1;
                        }
                        if !peek {
                            while rx.segs.front().is_some_and(|s| s.off >= s.data.len()) {
                                let s = rx.segs.pop_front().unwrap();
                                rx.bytes -= s.data.len().max(1);
                            }
                        }
                    }
                    drop(rx);
                    if !peek {
                        if let Some(p) = self.peer() {
                            p.pq.notify(POLLOUT);
                        }
                        self.pq.notify(POLLOUT);
                    }
                    return Ok(out);
                }
            }
            if self.rcv_shut() {
                // the peer may have queued its last data and closed since the queue was looked at
                if self.rx.lock().segs.is_empty() {
                    return Ok(Msg::default());
                }
                continue;
            }
            if self.connection_oriented() && !matches!(&*self.conn.lock(), Conn::Connected { .. }) {
                return Err(ENOTCONN);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            let r = self.pq.wq.wait_until(
                || !self.rx.lock().segs.is_empty() || self.rcv_shut(),
                None,
                true,
            );
            if let WaitResult::Interrupted = r {
                return Err(ERESTARTSYS);
            }
        }
    }

    fn shutdown(&self, how: u32) -> Result<()> {
        if self.connection_oriented() && !matches!(&*self.conn.lock(), Conn::Connected { .. }) {
            return Err(ENOTCONN);
        }
        if how == 0 || how == 2 {
            self.shut_rd.store(true, Ordering::SeqCst);
        }
        if how == 1 || how == 2 {
            self.shut_wr.store(true, Ordering::SeqCst);
            if let Some(p) = self.peer() {
                // a half-close: the peer reads EOF but may go on writing to us
                p.peer_shut_wr.store(true, Ordering::SeqCst);
                p.pq.notify(POLLIN | POLLRDHUP);
            }
        }
        if how == 0 || how == 2 {
            // the peer's writes now fail (Linux mirrors RCV_SHUTDOWN as the peer's SEND_SHUTDOWN)
            if let Some(p) = self.peer() {
                p.pq.notify(POLLOUT | POLLHUP);
            }
        }
        self.pq.notify(POLLIN | POLLOUT);
        Ok(())
    }

    fn local_addr(&self) -> Result<Vec<u8>> {
        Ok(match &*self.local.lock() {
            Some(n) => sockaddr(n),
            None => AF_UNIX.to_le_bytes().to_vec(),
        })
    }

    fn peer_addr(&self) -> Result<Vec<u8>> {
        if self.peer().is_none() {
            return Err(ENOTCONN);
        }
        Ok(match &*self.peer_addr.lock() {
            Some(n) => sockaddr(n),
            None => AF_UNIX.to_le_bytes().to_vec(),
        })
    }

    fn setsockopt(&self, level: i32, name: i32, val: &[u8]) -> Result<()> {
        let v = if val.len() >= 4 { i32::from_le_bytes(val[..4].try_into().unwrap()) } else { 0 };
        if level == SOL_SOCKET {
            match name {
                SO_PASSCRED => self.passcred.store(v != 0, Ordering::Relaxed),
                SO_SNDBUF => self.sndbuf.store((v.max(2048) as u32).saturating_mul(2), Ordering::Relaxed),
                SO_RCVBUF => self.rcvbuf.store((v.max(2048) as u32).saturating_mul(2), Ordering::Relaxed),
                _ => {}
            }
        }
        Ok(())
    }

    fn getsockopt(&self, level: i32, name: i32) -> Result<Vec<u8>> {
        if level != SOL_SOCKET {
            return Err(ENOPROTOOPT);
        }
        let i = |v: i32| v.to_le_bytes().to_vec();
        Ok(match name {
            SO_TYPE => i(self.ty as i32),
            SO_ERROR => i(0),
            SO_DOMAIN => i(AF_UNIX as i32),
            SO_PROTOCOL => i(0),
            SO_SNDBUF => i(self.sndbuf.load(Ordering::Relaxed) as i32),
            SO_RCVBUF => i(self.rcvbuf.load(Ordering::Relaxed) as i32),
            SO_PASSCRED => i(self.passcred.load(Ordering::Relaxed) as i32),
            SO_ACCEPTCONN => i(matches!(&*self.conn.lock(), Conn::Listening { .. }) as i32),
            SO_PEERCRED => {
                let (p, u, g) = *self.peer_cred.lock();
                let mut v = Vec::new();
                v.extend_from_slice(&p.to_le_bytes());
                v.extend_from_slice(&u.to_le_bytes());
                v.extend_from_slice(&g.to_le_bytes());
                v
            }
            _ => i(0),
        })
    }

    fn poll(&self) -> u32 {
        let mut ev = 0;
        match &*self.conn.lock() {
            Conn::Listening { backlog, .. } => {
                return if backlog.is_empty() { 0 } else { POLLIN | POLLRDNORM };
            }
            Conn::Connected { peer } => {
                if let Some(p) = peer.upgrade() {
                    if p.room() > 0 && !self.shut_wr.load(Ordering::Relaxed) {
                        ev |= POLLOUT | POLLWRNORM;
                    }
                }
            }
            Conn::DgramPeer { peer } => {
                if peer.upgrade().is_some_and(|p| p.room() > 0) {
                    ev |= POLLOUT | POLLWRNORM;
                }
            }
            Conn::None => {
                if !self.connection_oriented() {
                    ev |= POLLOUT | POLLWRNORM;
                }
            }
        }
        if !self.rx.lock().segs.is_empty() {
            ev |= POLLIN | POLLRDNORM;
        }
        // as Linux's unix_poll: RCV_SHUTDOWN reads as POLLIN|POLLRDHUP, both directions shut as POLLHUP
        let rcv = self.rcv_shut();
        if rcv {
            ev |= POLLIN | POLLRDHUP;
        }
        if self.connection_oriented() {
            let snd = self.shut_wr.load(Ordering::Relaxed)
                || self.peer_gone.load(Ordering::Relaxed)
                || self.peer().is_some_and(|p| p.shut_rd.load(Ordering::Relaxed));
            if rcv && snd {
                ev |= POLLHUP;
            }
        }
        ev
    }

    fn poll_queue(&self) -> &PollQueue {
        &self.pq
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> Result<isize> {
        if (0x8900..0x8a00).contains(&cmd) {
            return super::ifreq::ioctl(cmd, arg);
        }
        match cmd {
            0x541b => {
                // FIONREAD
                let rx = self.rx.lock();
                let n: usize = if self.ty == SOCK_STREAM { rx.segs.iter().map(|s| s.data.len() - s.off).sum() } else { rx.segs.front().map_or(0, |s| s.data.len()) };
                drop(rx);
                crate::mm::vm::write_user(arg, &(n as i32))?;
                Ok(0)
            }
            0x5411 => {
                crate::mm::vm::write_user(arg, &0i32)?;
                Ok(0)
            }
            _ => Err(ENOTTY),
        }
    }

    fn family(&self) -> u16 {
        AF_UNIX
    }
    fn sock_type(&self) -> u32 {
        self.ty
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
