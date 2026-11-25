//! AF_INET: TCP and UDP. Traffic to other hosts goes through smoltcp on eth0 (virtio-net);
//! connections to this host (127.0.0.0/8 or eth0's address) are paired inside the kernel as two
//! byte streams, which is what loopback TCP amounts to (ordered, lossless) without the protocol.
use super::unix::UnixSocket;
use super::*;
use crate::drivers::virtio_net::NIC;
use crate::sched::{WaitQueue, WaitResult};
use crate::sync::SpinLock;
use crate::vfs::poll::*;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Weak;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, Ipv4Address};

const TCP_BUF: usize = 256 * 1024;
const UDP_BUF: usize = 64 * 1024;

// ---- the smoltcp side

struct Dev;

struct Rx(Vec<u8>);
struct Tx;

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for Tx {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut b = alloc::vec![0u8; len];
        let r = f(&mut b);
        if DEBUG.load(Ordering::Relaxed) && b.len() >= 38 {
            println!("net: tx type {:02x}{:02x} proto {} -> {}.{}.{}.{}:{} from :{} flags {:02x} len {}", b[12], b[13], b[23], b[30], b[31], b[32], b[33], u16::from_be_bytes([b[36], b[37]]), u16::from_be_bytes([b[34], b[35]]), b.get(47).copied().unwrap_or(0), b.len());
        }
        if DEBUG.load(Ordering::Relaxed) && b.len() >= 48 && b[47] == 0x12 {
            for c in b.chunks(16) {
                for x in c {
                    print!("{:02x} ", x);
                }
                println!();
            }
        }
        if let Some(n) = NIC.get() {
            let _ = n.send(&b);
        }
        r
    }
}

impl Device for Dev {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx;
    fn receive(&mut self, _t: Instant) -> Option<(Rx, Tx)> {
        let f = NIC.get()?.recv()?;
        if DEBUG.load(Ordering::Relaxed) && f.len() >= 38 {
            println!("net: rx type {:02x}{:02x} proto {} {}.{}.{}.{}:{} -> :{} flags {:02x} len {}", f[12], f[13], f[23], f[26], f[27], f[28], f[29], u16::from_be_bytes([f[34], f[35]]), u16::from_be_bytes([f[36], f[37]]), f.get(47).copied().unwrap_or(0), f.len());
        }
        Some((Rx(f), Tx))
    }
    fn transmit(&mut self, _t: Instant) -> Option<Tx> {
        NIC.get().map(|_| Tx)
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = 1514;
        c
    }
}

pub struct Stack {
    iface: Interface,
    sockets: SocketSet<'static>,
    /// who owns each smoltcp socket (to wake it)
    owners: BTreeMap<SocketHandle, Weak<InetSocket>>,
    pub addr: Option<(Ipv4Address, u8)>,
    pub gateway: Option<Ipv4Address>,
    pub up: bool,
}

pub static STACK: SpinLock<Option<Stack>> = SpinLock::new(None);
static NET_WQ: WaitQueue = WaitQueue::new();
static KICKED: AtomicBool = AtomicBool::new(false);
static NEXT_PORT: AtomicU16 = AtomicU16::new(32768);

fn now() -> Instant {
    Instant::from_micros((crate::time::monotonic_ns() / 1000) as i64)
}

static DEBUG: AtomicBool = AtomicBool::new(false);

pub fn init() {
    DEBUG.store(crate::kernel_param("ruxos.debug") == Some("net"), Ordering::Relaxed);
    crate::fs::sysfs::NET_IFS.lock().insert(0, (alloc::string::String::from("lo"), [0; 6], 65536, "", 1));
    let Some(nic) = NIC.get() else { return };
    let cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(nic.mac)));
    let iface = Interface::new(cfg, &mut Dev, now());
    *STACK.lock() = Some(Stack { iface, sockets: SocketSet::new(Vec::new()), owners: BTreeMap::new(), addr: None, gateway: None, up: false });
    crate::sched::spawn_kernel(net_thread, 0);
}

/// Something happened (a frame arrived, a socket queued data): poll soon.
pub fn kick() {
    KICKED.store(true, Ordering::Release);
    NET_WQ.wake_all();
}

/// Runs the protocol: receive, transmit, timers. Wakes sockets that may have changed.
pub fn poll() {
    let owners: Vec<Weak<InetSocket>> = {
        let mut g = STACK.lock();
        let Some(s) = g.as_mut() else { return };
        let r = s.iface.poll(now(), &mut Dev, &mut s.sockets);
        if r == smoltcp::iface::PollResult::None {
            return;
        }
        s.owners.values().cloned().collect()
    };
    for o in owners {
        if let Some(sock) = o.upgrade() {
            sock.on_poll();
        }
    }
}

extern "C" fn net_thread(_: usize) {
    loop {
        KICKED.store(false, Ordering::Release);
        poll();
        if DEBUG.load(Ordering::Relaxed) {
            if let Some(n) = NIC.get() {
                n.debug();
            }
        }
        let delay = {
            let mut g = STACK.lock();
            match g.as_mut() {
                Some(s) => s.iface.poll_delay(now(), &s.sockets),
                None => None,
            }
        };
        // at least 1 ms: smoltcp may ask for "now" again while nothing can progress
        let d = delay.unwrap_or(Duration::from_millis(1000)).total_micros().clamp(1000, 1_000_000);
        let deadline = crate::arch::ticks() + crate::time::ns_to_ticks(d * 1000);
        NET_WQ.wait_until(|| KICKED.load(Ordering::Acquire), Some(deadline), false);
    }
}

pub fn local_ip(a: [u8; 4]) -> bool {
    if a[0] == 127 {
        return true;
    }
    STACK.lock().as_ref().and_then(|s| s.addr).is_some_and(|(ip, _)| ip.octets() == a)
}

// ---- addresses

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Sin {
    pub ip: [u8; 4],
    pub port: u16,
}

impl Sin {
    pub fn parse(b: &[u8]) -> Result<Sin> {
        if b.len() < 8 {
            return Err(EINVAL);
        }
        let fam = u16::from_le_bytes([b[0], b[1]]);
        if fam != AF_INET && !(fam == 0) {
            return Err(EAFNOSUPPORT);
        }
        Ok(Sin { port: u16::from_be_bytes([b[2], b[3]]), ip: [b[4], b[5], b[6], b[7]] })
    }
    pub fn bytes(&self) -> Vec<u8> {
        let mut v = alloc::vec![0u8; 16];
        v[0..2].copy_from_slice(&AF_INET.to_le_bytes());
        v[2..4].copy_from_slice(&self.port.to_be_bytes());
        v[4..8].copy_from_slice(&self.ip);
        v
    }
    fn any(&self) -> bool {
        self.ip == [0; 4]
    }
}

fn ephemeral() -> u16 {
    loop {
        let p = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        if p >= 60999 {
            NEXT_PORT.store(32768, Ordering::Relaxed);
        }
        if !LISTENERS.lock().contains_key(&p) && !UDP_PORTS.lock().contains_key(&p) {
            return p;
        }
    }
}

/// Listening TCP sockets by port (for local connections).
static LISTENERS: SpinLock<BTreeMap<u16, Vec<Weak<InetSocket>>>> = SpinLock::new(BTreeMap::new());
/// Bound UDP sockets by port.
static UDP_PORTS: SpinLock<BTreeMap<u16, Weak<InetSocket>>> = SpinLock::new(BTreeMap::new());

// ---- sockets

enum State {
    Fresh,
    Listening { backlog: VecDeque<Arc<InetSocket>>, max: usize, pool: Vec<SocketHandle> },
    /// connected to a local socket: the byte stream is a pair of AF_UNIX stream ends
    Local { end: Arc<UnixSocket> },
    Tcp { h: SocketHandle },
    Udp { h: Option<SocketHandle> },
    Closed,
}

pub struct InetSocket {
    ty: u32,
    me: Weak<InetSocket>,
    st: SpinLock<State>,
    local: SpinLock<Option<Sin>>,
    peer: SpinLock<Option<Sin>>,
    pq: PollQueue,
    error: AtomicU32,
    nodelay: AtomicBool,
    reuse: AtomicBool,
    keepalive: AtomicBool,
    shut_wr: AtomicBool,
    /// local UDP datagrams received (from, data)
    udp_rx: SpinLock<VecDeque<(Sin, Vec<u8>)>>,
}

pub fn socket(_family: u16, ty: u32, proto: i32) -> Result<Arc<dyn Socket>> {
    match ty {
        SOCK_STREAM if proto == 0 || proto == 6 => {}
        SOCK_DGRAM if proto == 0 || proto == 17 => {}
        SOCK_STREAM | SOCK_DGRAM => return Err(EPROTONOSUPPORT),
        _ => return Err(ESOCKTNOSUPPORT),
    }
    Ok(InetSocket::new(ty, State::Fresh))
}

impl InetSocket {
    fn new(ty: u32, st: State) -> Arc<InetSocket> {
        Arc::new_cyclic(|me| InetSocket {
            ty,
            me: me.clone(),
            st: SpinLock::new(st),
            local: SpinLock::new(None),
            peer: SpinLock::new(None),
            pq: PollQueue::new(),
            error: AtomicU32::new(0),
            nodelay: AtomicBool::new(false),
            reuse: AtomicBool::new(false),
            keepalive: AtomicBool::new(false),
            shut_wr: AtomicBool::new(false),
            udp_rx: SpinLock::new(VecDeque::new()),
        })
    }

    fn own(&self, s: &mut Stack, h: SocketHandle) {
        s.owners.insert(h, self.me.clone());
    }

    fn new_tcp(s: &mut Stack) -> SocketHandle {
        let t = tcp::Socket::new(tcp::SocketBuffer::new(alloc::vec![0; TCP_BUF]), tcp::SocketBuffer::new(alloc::vec![0; TCP_BUF]));
        s.sockets.add(t)
    }

    /// The protocol moved: accept finished handshakes, wake waiters.
    fn on_poll(&self) {
        let mut moved = Vec::new();
        {
            let mut g = STACK.lock();
            let Some(s) = g.as_mut() else { return };
            if let State::Listening { pool, .. } = &mut *self.st.lock() {
                let mut i = 0;
                while i < pool.len() {
                    let t = s.sockets.get_mut::<tcp::Socket>(pool[i]);
                    if t.is_active() && t.state() != tcp::State::Listen {
                        moved.push(pool.remove(i));
                    } else {
                        i += 1;
                    }
                }
                // keep the listening pool full
                let port = self.local.lock().map_or(0, |l| l.port);
                while pool.len() < 4 && port != 0 {
                    let h = Self::new_tcp(s);
                    let _ = s.sockets.get_mut::<tcp::Socket>(h).listen(IpListenEndpoint { addr: None, port });
                    self.own(s, h);
                    pool.push(h);
                }
            }
            for &h in &moved {
                let t = s.sockets.get_mut::<tcp::Socket>(h);
                let peer = t.remote_endpoint().map(ep_sin).unwrap_or_default();
                let local = t.local_endpoint().map(ep_sin).unwrap_or_default();
                t.set_nagle_enabled(false);
                let c = InetSocket::new(SOCK_STREAM, State::Tcp { h });
                *c.local.lock() = Some(local);
                *c.peer.lock() = Some(peer);
                s.owners.insert(h, c.me.clone());
                if let State::Listening { backlog, .. } = &mut *self.st.lock() {
                    backlog.push_back(c);
                }
            }
        }
        self.pq.notify(POLLIN | POLLOUT);
    }

    fn tcp<R>(&self, h: SocketHandle, f: impl FnOnce(&mut tcp::Socket) -> R) -> Option<R> {
        let mut g = STACK.lock();
        let s = g.as_mut()?;
        Some(f(s.sockets.get_mut::<tcp::Socket>(h)))
    }

    fn wait(&self, nonblock: bool, mut ready: impl FnMut() -> bool) -> Result<()> {
        if ready() {
            return Ok(());
        }
        if nonblock {
            return Err(EAGAIN);
        }
        match self.pq.wq.wait_until(&mut ready, None, true) {
            WaitResult::Interrupted => Err(ERESTARTSYS),
            _ => Ok(()),
        }
    }
}

fn ep_sin(e: IpEndpoint) -> Sin {
    let IpAddress::Ipv4(a) = e.addr;
    Sin { ip: a.octets(), port: e.port }
}

impl Drop for InetSocket {
    fn drop(&mut self) {
        let st = core::mem::replace(self.st.get_mut_inner(), State::Closed);
        if let Some(l) = *self.local.lock() {
            LISTENERS.lock().get_mut(&l.port).map(|v| v.retain(|w| w.strong_count() > 0));
            let mut u = UDP_PORTS.lock();
            if u.get(&l.port).is_some_and(|w| w.strong_count() == 0) {
                u.remove(&l.port);
            }
        }
        let mut g = STACK.lock();
        let Some(s) = g.as_mut() else { return };
        match st {
            State::Tcp { h } => {
                // close gracefully; the socket is removed once closed (by the reaper below)
                s.sockets.get_mut::<tcp::Socket>(h).close();
                s.owners.remove(&h);
                CLOSING.lock().push(h);
            }
            State::Listening { pool, .. } => {
                for h in pool {
                    s.owners.remove(&h);
                    s.sockets.remove(h);
                }
            }
            State::Udp { h: Some(h) } => {
                s.owners.remove(&h);
                s.sockets.remove(h);
            }
            _ => {}
        }
        // reap sockets that finished closing
        let mut c = CLOSING.lock();
        c.retain(|&h| {
            let done = s.sockets.get::<tcp::Socket>(h).state() == tcp::State::Closed;
            if done {
                s.sockets.remove(h);
            }
            !done
        });
        drop(c);
        drop(g);
        kick();
    }
}

static CLOSING: SpinLock<Vec<SocketHandle>> = SpinLock::new(Vec::new());

impl Socket for InetSocket {
    fn bind(&self, addr: &[u8]) -> Result<()> {
        let mut a = Sin::parse(addr)?;
        if self.local.lock().is_some() {
            return Err(EINVAL);
        }
        if !a.any() && !local_ip(a.ip) {
            return Err(EADDRNOTAVAIL);
        }
        if a.port == 0 {
            a.port = ephemeral();
        } else if a.port < 1024 && !crate::task::current_proc().cred().capable(10) {
            return Err(EACCES);
        }
        if self.ty == SOCK_DGRAM {
            let mut u = UDP_PORTS.lock();
            if u.get(&a.port).is_some_and(|w| w.strong_count() > 0) {
                return Err(EADDRINUSE);
            }
            u.insert(a.port, self.me.clone());
        } else if LISTENERS.lock().get(&a.port).is_some_and(|v| v.iter().any(|w| w.upgrade().is_some_and(|l| l.local.lock().is_some_and(|x| x.any() || a.any() || x.ip == a.ip)))) {
            return Err(EADDRINUSE);
        }
        *self.local.lock() = Some(a);
        Ok(())
    }

    fn listen(&self, backlog: i32) -> Result<()> {
        if self.ty != SOCK_STREAM {
            return Err(EOPNOTSUPP);
        }
        if self.local.lock().is_none() {
            *self.local.lock() = Some(Sin { ip: [0; 4], port: ephemeral() });
        }
        let l = self.local.lock().unwrap();
        match self.view() {
            View::Listening => {
                if let State::Listening { max, .. } = &mut *self.st.lock() {
                    *max = backlog.clamp(1, 4096) as usize;
                }
                return Ok(());
            }
            View::Fresh => {}
            _ => return Err(EINVAL),
        }
        // from other hosts: smoltcp listeners on eth0 (unless bound to loopback only)
        let mut pool = Vec::new();
        if l.ip[0] != 127 {
            let mut g = STACK.lock();
            if let Some(s) = g.as_mut() {
                for _ in 0..4 {
                    let h = Self::new_tcp(s);
                    let _ = s.sockets.get_mut::<tcp::Socket>(h).listen(IpListenEndpoint { addr: None, port: l.port });
                    self.own(s, h);
                    pool.push(h);
                }
                *self.st.lock() = State::Listening { backlog: VecDeque::new(), max: backlog.clamp(1, 4096) as usize, pool };
            } else {
                *self.st.lock() = State::Listening { backlog: VecDeque::new(), max: backlog.clamp(1, 4096) as usize, pool };
            }
        } else {
            *self.st.lock() = State::Listening { backlog: VecDeque::new(), max: backlog.clamp(1, 4096) as usize, pool };
        }
        LISTENERS.lock().entry(l.port).or_default().push(self.me.clone());
        kick();
        Ok(())
    }

    fn accept(&self, nonblock: bool) -> Result<(Arc<dyn Socket>, Vec<u8>)> {
        loop {
            {
                let mut st = self.st.lock();
                let State::Listening { backlog, .. } = &mut *st else { return Err(EINVAL) };
                if let Some(c) = backlog.pop_front() {
                    drop(st);
                    let a = c.peer.lock().unwrap_or_default().bytes();
                    return Ok((c, a));
                }
            }
            self.wait(nonblock, || matches!(&*self.st.lock(), State::Listening { backlog, .. } if !backlog.is_empty()))?;
        }
    }

    fn connect(&self, addr: &[u8], nonblock: bool) -> Result<()> {
        let to = Sin::parse(addr)?;
        if self.ty == SOCK_DGRAM {
            *self.peer.lock() = Some(to);
            if self.local.lock().is_none() {
                let ip = if local_ip(to.ip) { [127, 0, 0, 1] } else { STACK.lock().as_ref().and_then(|s| s.addr).map_or([0; 4], |a| a.0.octets()) };
                let p = ephemeral();
                *self.local.lock() = Some(Sin { ip, port: p });
                UDP_PORTS.lock().insert(p, self.me.clone());
            }
            return Ok(());
        }
        match &*self.st.lock() {
            State::Fresh => {}
            State::Tcp { .. } | State::Local { .. } => return Err(EISCONN),
            _ => return Err(EINVAL),
        }
        if local_ip(to.ip) || to.ip == [0; 4] {
            // a listener on this host
            let l = LISTENERS.lock().get(&to.port).and_then(|v| {
                v.iter().filter_map(|w| w.upgrade()).find(|l| l.local.lock().is_some_and(|a| a.any() || a.ip == to.ip || (a.ip[0] == 127 && to.ip[0] == 127)))
            });
            let Some(l) = l else { return Err(ECONNREFUSED) };
            let (a, b) = UnixSocket::pair(SOCK_STREAM)?;
            let me_addr = Sin { ip: if to.ip[0] == 127 || to.ip == [0; 4] { [127, 0, 0, 1] } else { to.ip }, port: ephemeral() };
            let server = InetSocket::new(SOCK_STREAM, State::Local { end: b });
            *server.local.lock() = Some(Sin { ip: me_addr.ip, port: to.port });
            *server.peer.lock() = Some(me_addr);
            {
                let mut st = l.st.lock();
                let State::Listening { backlog, max, .. } = &mut *st else { return Err(ECONNREFUSED) };
                if backlog.len() >= *max {
                    return Err(if nonblock { EAGAIN } else { ECONNREFUSED });
                }
                backlog.push_back(server.clone());
            }
            *self.local.lock() = Some(me_addr);
            *self.peer.lock() = Some(Sin { ip: me_addr.ip, port: to.port });
            *self.st.lock() = State::Local { end: a.clone() };
            // both ends' readiness flows to the inet sockets' poll queues
            relay(&a, &self.me);
            relay(server.local_end().as_ref().unwrap(), &server.me);
            l.pq.notify(POLLIN);
            return Ok(());
        }
        // another host: TCP over eth0
        let lport = ephemeral();
        let h = {
            let mut g = STACK.lock();
            let s = g.as_mut().ok_or(ENETUNREACH)?;
            if s.addr.is_none() {
                return Err(ENETUNREACH);
            }
            let h = Self::new_tcp(s);
            let ctx = s.iface.context();
            let t = s.sockets.get_mut::<tcp::Socket>(h);
            t.set_nagle_enabled(!self.nodelay.load(Ordering::Relaxed));
            if self.keepalive.load(Ordering::Relaxed) {
                t.set_keep_alive(Some(Duration::from_secs(60)));
            }
            if t.connect(ctx, (IpAddress::v4(to.ip[0], to.ip[1], to.ip[2], to.ip[3]), to.port), lport).is_err() {
                s.sockets.remove(h);
                return Err(EADDRNOTAVAIL);
            }
            self.own(s, h);
            let local = s.addr.map(|a| a.0.octets()).unwrap_or([0; 4]);
            *self.local.lock() = Some(Sin { ip: local, port: lport });
            h
        };
        *self.peer.lock() = Some(to);
        *self.st.lock() = State::Tcp { h };
        kick();
        poll();
        let established = |s: &Self| s.tcp(h, |t| t.state()).map_or(true, |st| st != tcp::State::SynSent && st != tcp::State::SynReceived);
        if nonblock {
            if !established(self) {
                return Err(EINPROGRESS);
            }
        } else {
            self.wait(false, || established(self))?;
        }
        if self.tcp(h, |t| t.is_active() && t.may_send()).unwrap_or(false) {
            Ok(())
        } else {
            self.error.store(ECONNREFUSED.0 as u32, Ordering::Relaxed);
            Err(ECONNREFUSED)
        }
    }

    fn send(&self, msg: Msg, flags: u32, nonblock: bool) -> Result<usize> {
        let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
        if self.ty == SOCK_DGRAM {
            return self.send_udp(msg, nonblock);
        }
        if self.shut_wr.load(Ordering::Relaxed) {
            return Err(EPIPE);
        }
        let h = match self.view() {
            View::Local(e) => return e.send(Msg { data: msg.data, ..Default::default() }, flags | MSG_NOSIGNAL, nonblock).map_err(|e| self.epipe(e, flags)),
            View::Tcp(h) => h,
            _ => return Err(ENOTCONN),
        };
        let data = msg.data;
        let mut sent = 0;
        while sent < data.len() || data.is_empty() {
            let r = self.tcp(h, |t| {
                if !t.may_send() {
                    return Err(EPIPE);
                }
                Ok(t.send_slice(&data[sent..]).unwrap_or(0))
            });
            match r {
                Some(Ok(n)) => sent += n,
                Some(Err(e)) => return if sent > 0 { Ok(sent) } else { Err(self.epipe(e, flags)) },
                None => return Err(ENETDOWN),
            }
            kick();
            if sent == data.len() {
                break;
            }
            if nonblock {
                return if sent > 0 { Ok(sent) } else { Err(EAGAIN) };
            }
            if let Err(e) = self.wait(false, || self.tcp(h, |t| t.can_send() || !t.may_send()).unwrap_or(true)) {
                return if sent > 0 { Ok(sent) } else { Err(e) };
            }
        }
        poll();
        Ok(sent)
    }

    fn recv(&self, len: usize, flags: u32, nonblock: bool) -> Result<Msg> {
        let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
        if self.ty == SOCK_DGRAM {
            return self.recv_udp(len, flags, nonblock);
        }
        let h = match self.view() {
            View::Local(e) => return e.recv(len, flags, nonblock),
            View::Tcp(h) => h,
            _ => return Err(ENOTCONN),
        };
        loop {
            let r = self.tcp(h, |t| {
                if t.can_recv() {
                    let mut buf = alloc::vec![0u8; len.min(TCP_BUF)];
                    let n = if flags & MSG_PEEK != 0 { t.peek_slice(&mut buf).unwrap_or(0) } else { t.recv_slice(&mut buf).unwrap_or(0) };
                    buf.truncate(n);
                    Some(Ok(buf))
                } else if !t.may_recv() {
                    // FIN received, or reset
                    if t.state() == tcp::State::Closed && t.remote_endpoint().is_none() && self.error.load(Ordering::Relaxed) == 0 {
                        Some(Ok(Vec::new()))
                    } else {
                        Some(Ok(Vec::new()))
                    }
                } else {
                    None
                }
            });
            match r {
                Some(Some(Ok(d))) => {
                    if !d.is_empty() {
                        kick(); // a window update
                    }
                    return Ok(Msg { data: d, ..Default::default() });
                }
                Some(Some(Err(e))) => return Err(e),
                Some(None) => {}
                None => return Err(ENETDOWN),
            }
            self.wait(nonblock, || self.tcp(h, |t| t.can_recv() || !t.may_recv()).unwrap_or(true))?;
        }
    }

    fn shutdown(&self, how: u32) -> Result<()> {
        match self.view() {
            View::Local(end) => return end.shutdown(how),
            View::Tcp(h) => {
                if how != 0 {
                    self.shut_wr.store(true, Ordering::Relaxed);
                    self.tcp(h, |t| t.close());
                    kick();
                }
            }
            View::Closed => return Err(ENOTCONN),
            _ => return if self.ty == SOCK_DGRAM { Ok(()) } else { Err(ENOTCONN) },
        }
        self.pq.notify(POLLIN | POLLOUT);
        Ok(())
    }

    fn local_addr(&self) -> Result<Vec<u8>> {
        Ok(self.local.lock().unwrap_or_default().bytes())
    }

    fn peer_addr(&self) -> Result<Vec<u8>> {
        match *self.peer.lock() {
            Some(p) if !matches!(&*self.st.lock(), State::Fresh | State::Listening { .. }) || self.ty == SOCK_DGRAM => Ok(p.bytes()),
            _ => Err(ENOTCONN),
        }
    }

    fn setsockopt(&self, level: i32, name: i32, val: &[u8]) -> Result<()> {
        let v = if val.len() >= 4 { i32::from_le_bytes(val[..4].try_into().unwrap()) } else { 0 };
        match (level, name) {
            (SOL_SOCKET, SO_REUSEADDR) | (SOL_SOCKET, 15) => self.reuse.store(v != 0, Ordering::Relaxed),
            (SOL_SOCKET, SO_KEEPALIVE) => {
                self.keepalive.store(v != 0, Ordering::Relaxed);
                if let View::Tcp(h) = self.view() {
                    self.tcp(h, |t| t.set_keep_alive(if v != 0 { Some(Duration::from_secs(60)) } else { None }));
                }
            }
            (6, 1) => {
                // TCP_NODELAY
                self.nodelay.store(v != 0, Ordering::Relaxed);
                if let View::Tcp(h) = self.view() {
                    self.tcp(h, |t| t.set_nagle_enabled(v == 0));
                }
            }
            _ => {} // buffer sizes, keepalive timing, IP options: accepted
        }
        Ok(())
    }

    fn getsockopt(&self, level: i32, name: i32) -> Result<Vec<u8>> {
        let i = |v: i32| v.to_le_bytes().to_vec();
        Ok(match (level, name) {
            (SOL_SOCKET, SO_TYPE) => i(self.ty as i32),
            (SOL_SOCKET, SO_ERROR) => i(self.error.swap(0, Ordering::Relaxed) as i32),
            (SOL_SOCKET, SO_DOMAIN) => i(AF_INET as i32),
            (SOL_SOCKET, SO_PROTOCOL) => i(if self.ty == SOCK_STREAM { 6 } else { 17 }),
            (SOL_SOCKET, SO_SNDBUF) | (SOL_SOCKET, SO_RCVBUF) => i(TCP_BUF as i32),
            (SOL_SOCKET, SO_REUSEADDR) => i(self.reuse.load(Ordering::Relaxed) as i32),
            (SOL_SOCKET, SO_KEEPALIVE) => i(self.keepalive.load(Ordering::Relaxed) as i32),
            (SOL_SOCKET, SO_ACCEPTCONN) => i(matches!(&*self.st.lock(), State::Listening { .. }) as i32),
            (6, 1) => i(self.nodelay.load(Ordering::Relaxed) as i32),
            (6, 2) => i(1460), // TCP_MAXSEG
            _ => i(0),
        })
    }

    fn poll(&self) -> u32 {
        match self.view() {
            View::Listening => match &*self.st.lock() {
                State::Listening { backlog, .. } if !backlog.is_empty() => POLLIN | POLLRDNORM,
                _ => 0,
            },
            View::Local(end) => end.poll(),
            View::Tcp(h) => self
                .tcp(h, |t| {
                    let mut ev = 0;
                    if t.can_recv() {
                        ev |= POLLIN | POLLRDNORM;
                    }
                    if !t.may_recv() && !matches!(t.state(), tcp::State::SynSent | tcp::State::SynReceived) {
                        ev |= POLLIN | POLLRDHUP;
                    }
                    if t.can_send() {
                        ev |= POLLOUT | POLLWRNORM;
                    }
                    if t.state() == tcp::State::Closed {
                        ev |= POLLHUP | POLLOUT | POLLERR;
                        if self.error.load(Ordering::Relaxed) == 0 {
                            self.error.store(ECONNRESET.0 as u32, Ordering::Relaxed);
                        }
                    }
                    ev
                })
                .unwrap_or(POLLERR),
            View::Udp(h) => {
                let mut ev = POLLOUT | POLLWRNORM;
                if !self.udp_rx.lock().is_empty() || h.is_some_and(|h| STACK.lock().as_mut().is_some_and(|s| s.sockets.get_mut::<udp::Socket>(h).can_recv())) {
                    ev |= POLLIN | POLLRDNORM;
                }
                ev
            }
            View::Fresh => {
                if self.ty == SOCK_DGRAM {
                    POLLOUT | if self.udp_rx.lock().is_empty() { 0 } else { POLLIN }
                } else {
                    POLLOUT | POLLHUP
                }
            }
            View::Closed => POLLHUP,
        }
    }

    fn poll_queue(&self) -> &PollQueue {
        &self.pq
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> Result<isize> {
        if cmd == 0x541b {
            let n = match self.view() {
                View::Local(e) => return e.ioctl(cmd, arg),
                View::Tcp(h) => self.tcp(h, |t| t.recv_queue()).unwrap_or(0),
                View::Udp(h) => {
                    let local = self.udp_rx.lock().front().map(|d| d.1.len());
                    // the size of the next datagram (what FIONREAD means for UDP)
                    local.or_else(|| h.and_then(|h| STACK.lock().as_mut().and_then(|s| s.sockets.get_mut::<udp::Socket>(h).peek().ok().map(|(d, _)| d.len())))).unwrap_or(0)
                }
                _ => self.udp_rx.lock().front().map_or(0, |d| d.1.len()),
            };
            crate::mm::vm::write_user(arg, &(n as i32))?;
            return Ok(0);
        }
        super::ifreq::ioctl(cmd, arg)
    }

    fn family(&self) -> u16 {
        AF_INET
    }
    fn sock_type(&self) -> u32 {
        self.ty
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Readiness of a local stream end shows up on the inet socket wrapping it.
fn relay(end: &Arc<UnixSocket>, to: &Weak<InetSocket>) {
    struct R(Weak<InetSocket>);
    impl Watcher for R {
        fn notify(&self, ev: u32) {
            if let Some(s) = self.0.upgrade() {
                s.pq.notify(ev);
            }
        }
    }
    let w: Arc<dyn Watcher> = Arc::new(R(to.clone()));
    end.poll_queue().watch(Arc::downgrade(&w));
    RELAYS.lock().push(w);
}

static RELAYS: SpinLock<Vec<Arc<dyn Watcher>>> = SpinLock::new(Vec::new());

/// A copy of the socket's state, taken without holding its lock (the lock order is STACK, then
/// st; nothing may sleep or take STACK while holding st).
enum View {
    Fresh,
    Listening,
    Local(Arc<UnixSocket>),
    Tcp(SocketHandle),
    Udp(Option<SocketHandle>),
    Closed,
}

impl InetSocket {
    fn view(&self) -> View {
        match &*self.st.lock() {
            State::Fresh => View::Fresh,
            State::Listening { .. } => View::Listening,
            State::Local { end } => View::Local(end.clone()),
            State::Tcp { h } => View::Tcp(*h),
            State::Udp { h } => View::Udp(*h),
            State::Closed => View::Closed,
        }
    }

    fn local_end(&self) -> Option<Arc<UnixSocket>> {
        match &*self.st.lock() {
            State::Local { end } => Some(end.clone()),
            _ => None,
        }
    }

    fn epipe(&self, e: Errno, flags: u32) -> Errno {
        if e == EPIPE && flags & MSG_NOSIGNAL == 0 {
            crate::task::send_signal_current(crate::task::signal::SIGPIPE);
        }
        e
    }

    fn udp_handle(&self, s: &mut Stack) -> SocketHandle {
        let mut st = self.st.lock();
        if let State::Udp { h: Some(h) } = &*st {
            return *h;
        }
        let u = udp::Socket::new(
            udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 64], alloc::vec![0; UDP_BUF]),
            udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 64], alloc::vec![0; UDP_BUF]),
        );
        let h = s.sockets.add(u);
        let port = {
            let mut l = self.local.lock();
            if l.is_none() {
                let p = ephemeral();
                *l = Some(Sin { ip: [0; 4], port: p });
                UDP_PORTS.lock().insert(p, self.me.clone());
            }
            l.unwrap().port
        };
        let _ = s.sockets.get_mut::<udp::Socket>(h).bind(port);
        s.owners.insert(h, self.me.clone());
        *st = State::Udp { h: Some(h) };
        h
    }

    fn send_udp(&self, msg: Msg, _nonblock: bool) -> Result<usize> {
        let to = match &msg.addr {
            Some(a) => Sin::parse(a)?,
            None => self.peer.lock().ok_or(EDESTADDRREQ)?,
        };
        if local_ip(to.ip) {
            if self.local.lock().is_none() {
                let p = ephemeral();
                *self.local.lock() = Some(Sin { ip: [127, 0, 0, 1], port: p });
                UDP_PORTS.lock().insert(p, self.me.clone());
            }
            let from = Sin { ip: [127, 0, 0, 1], port: self.local.lock().unwrap().port };
            if let Some(t) = UDP_PORTS.lock().get(&to.port).and_then(|w| w.upgrade()) {
                t.udp_rx.lock().push_back((from, msg.data.clone()));
                t.pq.notify(POLLIN);
            }
            return Ok(msg.data.len());
        }
        let n = msg.data.len();
        {
            let mut g = STACK.lock();
            let s = g.as_mut().ok_or(ENETUNREACH)?;
            if s.addr.is_none() {
                return Err(ENETUNREACH);
            }
            let h = self.udp_handle(s);
            let ep = IpEndpoint::new(IpAddress::v4(to.ip[0], to.ip[1], to.ip[2], to.ip[3]), to.port);
            s.sockets.get_mut::<udp::Socket>(h).send_slice(&msg.data, ep).map_err(|_| EAGAIN)?;
        }
        kick();
        poll();
        Ok(n)
    }

    fn recv_udp(&self, len: usize, flags: u32, nonblock: bool) -> Result<Msg> {
        loop {
            if let Some((from, d)) = if flags & MSG_PEEK != 0 { self.udp_rx.lock().front().cloned() } else { self.udp_rx.lock().pop_front() } {
                let mut m = Msg { addr: Some(from.bytes()), ..Default::default() };
                if d.len() > len {
                    m.flags |= MSG_TRUNC;
                }
                m.data = d[..d.len().min(len)].to_vec();
                return Ok(m);
            }
            let h = match &*self.st.lock() {
                State::Udp { h: Some(h) } => Some(*h),
                _ => None,
            };
            if let Some(h) = h {
                let got = {
                    let mut g = STACK.lock();
                    let s = g.as_mut().ok_or(ENETDOWN)?;
                    let u = s.sockets.get_mut::<udp::Socket>(h);
                    if u.can_recv() {
                        let mut buf = alloc::vec![0u8; 65536];
                        match u.recv_slice(&mut buf) {
                            Ok((n, meta)) => Some((n, meta.endpoint, buf)),
                            Err(_) => None,
                        }
                    } else {
                        None
                    }
                };
                if let Some((n, ep, buf)) = got {
                    let mut m = Msg { addr: Some(ep_sin(ep).bytes()), ..Default::default() };
                    if n > len {
                        m.flags |= MSG_TRUNC;
                    }
                    m.data = buf[..n.min(len)].to_vec();
                    return Ok(m);
                }
            }
            self.wait(nonblock, || {
                !self.udp_rx.lock().is_empty() || h.is_some_and(|h| STACK.lock().as_mut().is_some_and(|s| s.sockets.get_mut::<udp::Socket>(h).can_recv()))
            })?;
        }
    }
}

/// SIOCSIFADDR / SIOCSIFNETMASK / SIOCADDRT: eth0's configuration.
pub fn set_addr(ip: Option<[u8; 4]>, prefix: Option<u8>) {
    let mut g = STACK.lock();
    let Some(s) = g.as_mut() else { return };
    let (cur_ip, cur_pfx) = s.addr.map(|(a, p)| (a.octets(), p)).unwrap_or(([0; 4], 24));
    let ip = ip.unwrap_or(cur_ip);
    let pfx = prefix.unwrap_or(cur_pfx);
    s.addr = Some((Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]), pfx));
    s.iface.update_ip_addrs(|a| {
        a.clear();
        let _ = a.push(IpCidr::new(IpAddress::v4(ip[0], ip[1], ip[2], ip[3]), pfx));
    });
}

pub fn set_gateway(gw: [u8; 4]) {
    let mut g = STACK.lock();
    let Some(s) = g.as_mut() else { return };
    let a = Ipv4Address::new(gw[0], gw[1], gw[2], gw[3]);
    s.gateway = Some(a);
    let _ = s.iface.routes_mut().add_default_ipv4_route(a);
}
