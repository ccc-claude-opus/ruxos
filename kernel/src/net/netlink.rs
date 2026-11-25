//! NETLINK_ROUTE (rtnetlink(7)), enough for getifaddrs(3) and for programs that bring up "lo":
//! link and address dumps, and accepting address and link changes.
use super::*;
use crate::sync::SpinLock;
use crate::vfs::poll::*;
use alloc::collections::VecDeque;

pub struct Netlink {
    proto: i32,
    rx: SpinLock<VecDeque<Vec<u8>>>,
    pq: PollQueue,
    pid: u32,
}

pub fn socket(ty: u32, proto: i32) -> Result<Arc<dyn Socket>> {
    if ty != SOCK_RAW && ty != SOCK_DGRAM {
        return Err(ESOCKTNOSUPPORT);
    }
    if proto != 0 {
        return Err(EPROTONOSUPPORT); // only NETLINK_ROUTE
    }
    Ok(Arc::new(Netlink { proto, rx: SpinLock::new(VecDeque::new()), pq: PollQueue::new(), pid: crate::task::current_proc().pid }))
}

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_GETADDR: u16 = 22;
const RTM_NEWROUTE: u16 = 24;
const RTM_GETROUTE: u16 = 26;
const NLM_F_MULTI: u16 = 2;

fn align(n: usize) -> usize {
    (n + 3) & !3
}

fn attr(out: &mut Vec<u8>, ty: u16, data: &[u8]) {
    out.extend_from_slice(&((4 + data.len()) as u16).to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn msg(ty: u16, flags: u16, seq: u32, pid: u32, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(16 + body.len());
    m.extend_from_slice(&((16 + body.len()) as u32).to_le_bytes());
    m.extend_from_slice(&ty.to_le_bytes());
    m.extend_from_slice(&flags.to_le_bytes());
    m.extend_from_slice(&seq.to_le_bytes());
    m.extend_from_slice(&pid.to_le_bytes());
    m.extend_from_slice(body);
    m
}

/// (index, name, mac, mtu, flags, address/prefix)
fn links() -> Vec<(i32, &'static [u8], [u8; 6], u32, u32, Option<([u8; 4], u8)>)> {
    let mut v = alloc::vec![(1, &b"lo"[..], [0u8; 6], 65536u32, 0x49u32, Some(([127, 0, 0, 1], 8u8)))];
    if let Some(n) = crate::drivers::virtio_net::NIC.get() {
        let a = super::inet::STACK.lock().as_ref().and_then(|s| s.addr).map(|(a, p)| (a.octets(), p));
        v.push((2, &b"eth0"[..], n.mac, 1500, 0x11043, a));
    }
    v
}

impl Netlink {
    fn reply(&self, req: &[u8]) {
        let mut off = 0;
        let mut out: Vec<Vec<u8>> = Vec::new();
        while off + 16 <= req.len() {
            let len = u32::from_le_bytes(req[off..off + 4].try_into().unwrap()) as usize;
            if len < 16 || off + len > req.len() {
                break;
            }
            let ty = u16::from_le_bytes([req[off + 4], req[off + 5]]);
            let flags = u16::from_le_bytes([req[off + 6], req[off + 7]]);
            let seq = u32::from_le_bytes(req[off + 8..off + 12].try_into().unwrap());
            let pid = self.pid;
            match ty {
                RTM_GETLINK => {
                    for (idx, name, mac, mtu, fl, _) in links() {
                        // struct ifinfomsg { family u8, pad, type u16, index i32, flags u32, change u32 }
                        let mut b = Vec::new();
                        b.push(0);
                        b.push(0);
                        b.extend_from_slice(&(if idx == 1 { 772u16 } else { 1u16 }).to_le_bytes());
                        b.extend_from_slice(&idx.to_le_bytes());
                        b.extend_from_slice(&fl.to_le_bytes());
                        b.extend_from_slice(&0u32.to_le_bytes());
                        let mut n = name.to_vec();
                        n.push(0);
                        attr(&mut b, 3, &n); // IFLA_IFNAME
                        attr(&mut b, 4, &mtu.to_le_bytes()); // IFLA_MTU
                        attr(&mut b, 1, &mac); // IFLA_ADDRESS
                        attr(&mut b, 2, &if idx == 1 { [0u8; 6] } else { [0xff; 6] }); // IFLA_BROADCAST
                        attr(&mut b, 16, &[6u8]); // IFLA_OPERSTATE: up
                        out.push(msg(RTM_NEWLINK, NLM_F_MULTI, seq, pid, &b));
                    }
                    out.push(msg(NLMSG_DONE, NLM_F_MULTI, seq, pid, &0i32.to_le_bytes()));
                }
                RTM_GETADDR => {
                    for (idx, name, _, _, _, a) in links() {
                        let Some((ip, pfx)) = a else { continue };
                        // struct ifaddrmsg { family u8, prefixlen u8, flags u8, scope u8, index u32 }
                        let mut b = alloc::vec![2u8, pfx, 0x80, if idx == 1 { 254 } else { 0 }];
                        b.extend_from_slice(&(idx as u32).to_le_bytes());
                        attr(&mut b, 1, &ip); // IFA_ADDRESS
                        attr(&mut b, 2, &ip); // IFA_LOCAL
                        let mut n = name.to_vec();
                        n.push(0);
                        attr(&mut b, 3, &n); // IFA_LABEL
                        if idx != 1 {
                            let m = if pfx == 0 { 0 } else { u32::MAX << (32 - pfx) };
                            let bc = (u32::from_be_bytes(ip) | !m).to_be_bytes();
                            attr(&mut b, 4, &bc); // IFA_BROADCAST
                        }
                        out.push(msg(RTM_NEWADDR, NLM_F_MULTI, seq, pid, &b));
                    }
                    out.push(msg(NLMSG_DONE, NLM_F_MULTI, seq, pid, &0i32.to_le_bytes()));
                }
                RTM_GETROUTE => {
                    let gw = super::inet::STACK.lock().as_ref().and_then(|s| s.gateway).map(|g| g.octets());
                    if let Some(gw) = gw {
                        // struct rtmsg { family, dst_len, src_len, tos, table, protocol, scope, type, flags u32 }
                        let mut b = alloc::vec![2u8, 0, 0, 0, 254, 4, 0, 1];
                        b.extend_from_slice(&0u32.to_le_bytes());
                        attr(&mut b, 15, &254u32.to_le_bytes()); // RTA_TABLE
                        attr(&mut b, 5, &gw); // RTA_GATEWAY
                        attr(&mut b, 4, &2u32.to_le_bytes()); // RTA_OIF
                        out.push(msg(RTM_NEWROUTE, NLM_F_MULTI, seq, pid, &b));
                    }
                    out.push(msg(NLMSG_DONE, NLM_F_MULTI, seq, pid, &0i32.to_le_bytes()));
                }
                _ => {
                    // changes (new address, link up...) are accepted; acknowledge when asked
                    if flags & 4 != 0 || ty >= 16 {
                        let mut b = 0i32.to_le_bytes().to_vec();
                        b.extend_from_slice(&req[off..off + 16]);
                        out.push(msg(NLMSG_ERROR, 0, seq, pid, &b));
                    }
                }
            }
            off += align(len);
        }
        let mut rx = self.rx.lock();
        rx.extend(out);
        drop(rx);
        self.pq.notify(POLLIN);
    }
}

impl Socket for Netlink {
    fn bind(&self, _addr: &[u8]) -> Result<()> {
        Ok(())
    }
    fn connect(&self, _addr: &[u8], _nb: bool) -> Result<()> {
        Ok(())
    }
    fn send(&self, msg: Msg, _flags: u32, _nb: bool) -> Result<usize> {
        self.reply(&msg.data);
        Ok(msg.data.len())
    }
    fn recv(&self, len: usize, flags: u32, nonblock: bool) -> Result<Msg> {
        loop {
            {
                let mut rx = self.rx.lock();
                if !rx.is_empty() {
                    // one datagram holds as many messages as fit
                    let mut data = Vec::new();
                    while let Some(m) = rx.front() {
                        if !data.is_empty() && data.len() + m.len() > len {
                            break;
                        }
                        let m = if flags & MSG_PEEK != 0 { m.clone() } else { rx.pop_front().unwrap() };
                        data.extend_from_slice(&m);
                        if flags & MSG_PEEK != 0 {
                            break;
                        }
                    }
                    let mut out = Msg { addr: Some(alloc::vec![16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), ..Default::default() };
                    if data.len() > len {
                        out.flags |= MSG_TRUNC;
                        data.truncate(len);
                    }
                    out.data = data;
                    return Ok(out);
                }
            }
            if nonblock || flags & MSG_DONTWAIT != 0 {
                return Err(EAGAIN);
            }
            if let crate::sched::WaitResult::Interrupted = self.pq.wq.wait_until(|| !self.rx.lock().is_empty(), None, true) {
                return Err(ERESTARTSYS);
            }
        }
    }
    fn local_addr(&self) -> Result<Vec<u8>> {
        let mut a = alloc::vec![16u8, 0, 0, 0];
        a.extend_from_slice(&self.pid.to_le_bytes());
        a.extend_from_slice(&0u32.to_le_bytes());
        Ok(a)
    }
    fn poll(&self) -> u32 {
        POLLOUT | if self.rx.lock().is_empty() { 0 } else { POLLIN | POLLRDNORM }
    }
    fn poll_queue(&self) -> &PollQueue {
        &self.pq
    }
    fn ioctl(&self, cmd: u32, arg: usize) -> Result<isize> {
        super::ifreq::ioctl(cmd, arg)
    }
    fn family(&self) -> u16 {
        AF_NETLINK
    }
    fn sock_type(&self) -> u32 {
        let _ = self.proto;
        SOCK_RAW
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
