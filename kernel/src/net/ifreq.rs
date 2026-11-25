//! Interface ioctls on sockets (netdevice(7)): what busybox ifconfig/route and libc use.
use super::inet;
use crate::errno::*;
use crate::mm::vm::{copy_from_user, copy_to_user, read_user, write_user};
use alloc::vec::Vec;

/// (name, index) of the caller's network namespace: a new one has only lo.
fn interfaces() -> Vec<(&'static [u8], i32)> {
    let mut v: Vec<(&'static [u8], i32)> = alloc::vec![(b"lo", 1)];
    if crate::drivers::virtio_net::NIC.get().is_some() && crate::ns::cur_net().is_init {
        v.push((b"eth0", 2));
    }
    v
}

const IFF_UP: u16 = 1;
const IFF_LOOPBACK: u16 = 8;
const IFF_RUNNING: u16 = 0x40;
const IFF_BROADCAST: u16 = 2;
const IFF_MULTICAST: u16 = 0x1000;

fn sin(ip: [u8; 4]) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..2].copy_from_slice(&2u16.to_le_bytes());
    b[4..8].copy_from_slice(&ip);
    b
}

fn addr_of(name: &[u8]) -> Option<([u8; 4], u8)> {
    let ns = crate::ns::cur_net();
    match name {
        b"lo" if ns.lo_addr.load(core::sync::atomic::Ordering::Relaxed) => Some(([127, 0, 0, 1], 8)),
        b"lo" => None,
        _ if !ns.is_init => None,
        b"eth0" => inet::STACK.lock().as_ref().and_then(|s| s.addr).map(|(a, p)| (a.octets(), p)),
        _ => None,
    }
}

fn mask(p: u8) -> [u8; 4] {
    (if p == 0 { 0u32 } else { u32::MAX << (32 - p) }).to_be_bytes()
}

pub fn ioctl(cmd: u32, arg: usize) -> Result<isize> {
    // struct ifreq: char ifr_name[16]; union { sockaddr, short flags, int ivalue, ... } (24 bytes)
    let mut req = [0u8; 40];
    let rd = |req: &mut [u8; 40]| copy_from_user(req, arg);
    let name_of = |req: &[u8; 40]| -> Vec<u8> { req[..16].iter().copied().take_while(|&c| c != 0).collect() };
    match cmd {
        0x8912 => {
            // SIOCGIFCONF: struct ifconf { int len; char *buf }
            let conf: [usize; 2] = read_user(arg)?;
            let (len, buf) = (conf[0] as u32 as usize, conf[1]);
            let mut out = Vec::new();
            for (n, _) in interfaces() {
                if let Some((ip, _)) = addr_of(n) {
                    let mut r = [0u8; 40];
                    r[..n.len()].copy_from_slice(n);
                    r[16..32].copy_from_slice(&sin(ip));
                    out.extend_from_slice(&r);
                }
            }
            if buf != 0 {
                let k = out.len().min(len) / 40 * 40;
                copy_to_user(buf, &out[..k])?;
                write_user(arg, &(k as i32))?;
            } else {
                write_user(arg, &(out.len() as i32))?;
            }
            Ok(0)
        }
        0x8913 | 0x8914 | 0x8915 | 0x8916 | 0x891b | 0x891c | 0x8921 | 0x8927 | 0x8933 | 0x8910 => {
            rd(&mut req)?;
            let name = name_of(&req);
            let idx = interfaces().iter().find(|(n, _)| *n == &name[..]).map(|x| x.1);
            if cmd == 0x8910 {
                // SIOCGIFNAME: by index
                let i = i32::from_le_bytes(req[16..20].try_into().unwrap());
                let (n, _) = interfaces().into_iter().find(|(_, x)| *x == i).ok_or(ENODEV)?;
                req[..16].fill(0);
                req[..n.len()].copy_from_slice(n);
                copy_to_user(arg, &req)?;
                return Ok(0);
            }
            let idx = idx.ok_or(ENODEV)?;
            match cmd {
                0x8913 => {
                    // SIOCGIFFLAGS
                    let up = crate::ns::cur_net().lo_up.load(core::sync::atomic::Ordering::Relaxed);
                    let f = if name == b"lo" { if up { IFF_UP | IFF_LOOPBACK | IFF_RUNNING } else { IFF_LOOPBACK } } else { IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST };
                    req[16..18].copy_from_slice(&f.to_le_bytes());
                }
                0x8914 => {
                    // SIOCSIFFLAGS: eth0 is always up; lo of a new namespace can be brought up
                    need_admin()?;
                    let ns = crate::ns::cur_net();
                    if name == b"lo" && !ns.is_init {
                        let up = u16::from_le_bytes([req[16], req[17]]) & IFF_UP != 0;
                        ns.lo_up.store(up, core::sync::atomic::Ordering::Relaxed);
                        if up {
                            ns.lo_addr.store(true, core::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    return Ok(0);
                }
                0x8915 => req[16..32].copy_from_slice(&sin(addr_of(&name).ok_or(EADDRNOTAVAIL)?.0)),
                0x8916 => {
                    // SIOCSIFADDR
                    need_admin()?;
                    if name == b"eth0" {
                        inet::set_addr(Some([req[20], req[21], req[22], req[23]]), None);
                    }
                    return Ok(0);
                }
                0x891b => req[16..32].copy_from_slice(&sin(mask(addr_of(&name).ok_or(EADDRNOTAVAIL)?.1))),
                0x891c => {
                    // SIOCSIFNETMASK
                    need_admin()?;
                    let m = u32::from_be_bytes([req[20], req[21], req[22], req[23]]);
                    if name == b"eth0" {
                        inet::set_addr(None, Some(m.count_ones() as u8));
                    }
                    return Ok(0);
                }
                0x8921 => req[16..20].copy_from_slice(&(if name == b"lo" { 65536i32 } else { 1500 }).to_le_bytes()),
                0x8927 => {
                    // SIOCGIFHWADDR
                    let mac = if name == b"eth0" { crate::drivers::virtio_net::NIC.get().map_or([0; 6], |n| n.mac) } else { [0; 6] };
                    req[16..18].copy_from_slice(&(if name == b"lo" { 772u16 } else { 1u16 }).to_le_bytes());
                    req[18..24].copy_from_slice(&mac);
                }
                0x8933 => req[16..20].copy_from_slice(&idx.to_le_bytes()), // SIOCGIFINDEX
                _ => {}
            }
            copy_to_user(arg, &req)?;
            Ok(0)
        }
        0x890b => {
            // SIOCADDRT: struct rtentry { ulong pad; sockaddr dst; sockaddr gateway; sockaddr genmask; short flags; ... }
            need_admin()?;
            if !crate::ns::cur_net().is_init {
                return Err(ENETUNREACH); // a new namespace has no route out
            }
            let mut r = [0u8; 120];
            copy_from_user(&mut r, arg)?;
            let dst = [r[12], r[13], r[14], r[15]];
            let gw = [r[28], r[29], r[30], r[31]];
            let flags = u16::from_le_bytes([r[56], r[57]]);
            if dst == [0; 4] && flags & 2 != 0 {
                inet::set_gateway(gw);
            }
            Ok(0)
        }
        0x890c => Ok(0), // SIOCDELRT
        _ => Err(ENOTTY),
    }
}

/// CAP_NET_ADMIN over the caller's network namespace.
fn need_admin() -> Result<()> {
    let ns = crate::ns::cur_net();
    if crate::task::current_proc().cred().ns_capable(&ns.user_ns, 12) { Ok(()) } else { Err(EPERM) }
}
