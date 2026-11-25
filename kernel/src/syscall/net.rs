//! Socket system calls.
use super::fs::file;
use crate::errno::*;
use crate::mm::vm::{copy_from_user, copy_to_user, read_user, write_user};
use crate::net::*;
use crate::task::current_thread;
use crate::vfs::file::{File, O_CLOEXEC, O_NONBLOCK, O_RDWR};
use alloc::sync::Arc;
use alloc::vec::Vec;

fn install(sock: Arc<dyn Socket>, flags: u32) -> Result<i32> {
    let f = File::new(SocketFile::new(sock), O_RDWR | (flags & O_NONBLOCK), None);
    current_thread().files.lock().alloc(f, 0, flags & O_CLOEXEC != 0)
}

fn sock(fd: i32) -> Result<(Arc<File>, Arc<dyn Socket>)> {
    let f = file(fd)?;
    let s = sock_of(&f)?;
    Ok((f, s))
}

fn read_addr(p: usize, len: u32) -> Result<Vec<u8>> {
    if len as usize > 128 {
        return Err(EINVAL);
    }
    let mut v = alloc::vec![0u8; len as usize];
    copy_from_user(&mut v, p)?;
    Ok(v)
}

/// Writes a socket address to (addr, addrlen*) as accept/getsockname/recvfrom do.
fn write_addr(a: &[u8], p: usize, lenp: usize) -> Result<()> {
    if p == 0 || lenp == 0 {
        return Ok(());
    }
    let cap: u32 = read_user(lenp)?;
    let n = a.len().min(cap as usize);
    copy_to_user(p, &a[..n])?;
    write_user(lenp, &(a.len() as u32))?;
    Ok(())
}

pub fn socket(family: u32, ty: u32, proto: i32) -> Result<usize> {
    let flags = ty & (SOCK_NONBLOCK | SOCK_CLOEXEC);
    let s = create(family as u16, ty & 0xf, proto)?;
    Ok(install(s, flags)? as usize)
}

pub fn socketpair(family: u32, ty: u32, _proto: i32, sv: usize) -> Result<usize> {
    if family as u16 != AF_UNIX {
        return Err(EOPNOTSUPP);
    }
    let flags = ty & (SOCK_NONBLOCK | SOCK_CLOEXEC);
    let (a, b) = unix::UnixSocket::pair(ty & 0xf)?;
    let fa = install(a, flags)?;
    let fb = match install(b, flags) {
        Ok(x) => x,
        Err(e) => {
            let _ = current_thread().files.lock().close(fa);
            return Err(e);
        }
    };
    write_user(sv, &[fa, fb])?;
    Ok(0)
}

pub fn bind(fd: i32, addr: usize, len: u32) -> Result<usize> {
    let (_, s) = sock(fd)?;
    s.bind(&read_addr(addr, len)?)?;
    Ok(0)
}

pub fn listen(fd: i32, backlog: i32) -> Result<usize> {
    let (_, s) = sock(fd)?;
    s.listen(backlog)?;
    Ok(0)
}

pub fn accept4(fd: i32, addr: usize, lenp: usize, flags: u32) -> Result<usize> {
    if flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    let (f, s) = sock(fd)?;
    let (ns, a) = s.accept(f.nonblock())?;
    let nfd = install(ns, flags)?;
    write_addr(&a, addr, lenp)?;
    Ok(nfd as usize)
}

pub fn connect(fd: i32, addr: usize, len: u32) -> Result<usize> {
    let (f, s) = sock(fd)?;
    s.connect(&read_addr(addr, len)?, f.nonblock())?;
    Ok(0)
}

pub fn getsockname(fd: i32, addr: usize, lenp: usize) -> Result<usize> {
    let (_, s) = sock(fd)?;
    write_addr(&s.local_addr()?, addr, lenp)?;
    Ok(0)
}

pub fn getpeername(fd: i32, addr: usize, lenp: usize) -> Result<usize> {
    let (_, s) = sock(fd)?;
    write_addr(&s.peer_addr()?, addr, lenp)?;
    Ok(0)
}

pub fn sendto(fd: i32, buf: usize, len: usize, flags: u32, addr: usize, alen: u32) -> Result<usize> {
    let (f, s) = sock(fd)?;
    let mut data = alloc::vec![0u8; len];
    copy_from_user(&mut data, buf)?;
    let a = if addr != 0 { Some(read_addr(addr, alen)?) } else { None };
    s.send(Msg { data, addr: a, ..Default::default() }, flags, f.nonblock())
}

pub fn recvfrom(fd: i32, buf: usize, len: usize, flags: u32, addr: usize, lenp: usize) -> Result<usize> {
    let (f, s) = sock(fd)?;
    let m = s.recv(len.min(1 << 24), flags, f.nonblock())?;
    copy_to_user(buf, &m.data)?;
    if let Some(a) = &m.addr {
        write_addr(a, addr, lenp)?;
    } else if addr != 0 && lenp != 0 {
        write_user(lenp, &0u32)?;
    }
    install_fds(m.fds, false)?;
    Ok(if flags & MSG_TRUNC != 0 && m.flags & MSG_TRUNC != 0 { m.data.len().max(len) } else { m.data.len() })
}

fn install_fds(fds: Vec<Arc<File>>, cloexec: bool) -> Result<Vec<i32>> {
    let th = current_thread();
    let mut t = th.files.lock();
    let mut out = Vec::new();
    for f in fds {
        out.push(t.alloc(f, 0, cloexec)?);
    }
    Ok(out)
}

pub fn setsockopt(fd: i32, level: i32, name: i32, val: usize, len: u32) -> Result<usize> {
    let (_, s) = sock(fd)?;
    let mut v = alloc::vec![0u8; (len as usize).min(256)];
    copy_from_user(&mut v, val)?;
    s.setsockopt(level, name, &v)?;
    Ok(0)
}

pub fn getsockopt(fd: i32, level: i32, name: i32, val: usize, lenp: usize) -> Result<usize> {
    let (_, s) = sock(fd)?;
    let v = s.getsockopt(level, name)?;
    let cap: u32 = read_user(lenp)?;
    let n = v.len().min(cap as usize);
    copy_to_user(val, &v[..n])?;
    write_user(lenp, &(n as u32))?;
    Ok(0)
}

pub fn shutdown(fd: i32, how: u32) -> Result<usize> {
    if how > 2 {
        return Err(EINVAL);
    }
    let (_, s) = sock(fd)?;
    s.shutdown(how)?;
    Ok(0)
}

fn gather(iov: usize, cnt: usize) -> Result<Vec<u8>> {
    if cnt > 1024 {
        return Err(EMSGSIZE);
    }
    let mut data = Vec::new();
    for i in 0..cnt {
        let e: [usize; 2] = read_user(iov + 16 * i)?;
        let o = data.len();
        data.resize(o + e[1], 0);
        copy_from_user(&mut data[o..], e[0])?;
    }
    Ok(data)
}

pub fn sendmsg(fd: i32, msg: usize, flags: u32) -> Result<usize> {
    let (f, s) = sock(fd)?;
    let h: [usize; 7] = read_user(msg)?;
    let (name, namelen, iov, iovlen, ctl, ctllen) = (h[0], h[1] as u32, h[2], h[3], h[4], h[5]);
    let data = gather(iov, iovlen)?;
    let addr = if name != 0 && namelen > 0 { Some(read_addr(name, namelen)?) } else { None };
    let mut m = Msg { data, addr, ..Default::default() };
    // control messages
    let mut off = 0;
    while off + 16 <= ctllen {
        let hdr: [u8; 16] = read_user(ctl + off)?;
        let clen = usize::from_le_bytes(hdr[0..8].try_into().unwrap());
        let level = i32::from_le_bytes(hdr[8..12].try_into().unwrap());
        let ty = i32::from_le_bytes(hdr[12..16].try_into().unwrap());
        if clen < 16 || off + clen > ctllen {
            return Err(EINVAL);
        }
        if level == SOL_SOCKET && ty == 1 {
            let n = (clen - 16) / 4;
            let fds = current_thread().files.lock().clone();
            for i in 0..n {
                let fdn: i32 = read_user(ctl + off + 16 + 4 * i)?;
                m.fds.push(fds.get(fdn)?);
            }
        } else if level == SOL_SOCKET && ty == 2 {
            let c: [u32; 3] = read_user(ctl + off + 16)?;
            m.creds = Some((c[0], c[1], c[2]));
        }
        off += (clen + 7) & !7;
    }
    s.send(m, flags, f.nonblock())
}

pub fn recvmsg(fd: i32, msg: usize, flags: u32) -> Result<usize> {
    let (f, s) = sock(fd)?;
    let h: [usize; 7] = read_user(msg)?;
    let (name, iov, iovlen, ctl, ctllen) = (h[0], h[2], h[3], h[4], h[5]);
    if iovlen > 1024 {
        return Err(EMSGSIZE);
    }
    let mut iovs = Vec::new();
    let mut total = 0;
    for i in 0..iovlen {
        let e: [usize; 2] = read_user(iov + 16 * i)?;
        total += e[1];
        iovs.push(e);
    }
    let m = s.recv(total, flags, f.nonblock())?;
    let mut o = 0;
    for e in &iovs {
        if o >= m.data.len() {
            break;
        }
        let n = e[1].min(m.data.len() - o);
        copy_to_user(e[0], &m.data[o..o + n])?;
        o += n;
    }
    // name
    let namelen = match (&m.addr, name) {
        (Some(a), n) if n != 0 => {
            let cap = h[1] as u32 as usize;
            copy_to_user(n, &a[..a.len().min(cap)])?;
            a.len()
        }
        _ => 0,
    };
    write_user(msg + 8, &(namelen as u32))?;
    // control: SCM_RIGHTS, SCM_CREDENTIALS (when asked for with SO_PASSCRED)
    let mut cbuf: Vec<u8> = Vec::new();
    let mut mflags = m.flags;
    if !m.fds.is_empty() {
        let fds = install_fds(m.fds, flags & MSG_CMSG_CLOEXEC != 0)?;
        let clen = 16 + 4 * fds.len();
        if clen <= ctllen {
            cbuf.extend_from_slice(&(clen as u64).to_le_bytes());
            cbuf.extend_from_slice(&SOL_SOCKET.to_le_bytes());
            cbuf.extend_from_slice(&1i32.to_le_bytes());
            for x in &fds {
                cbuf.extend_from_slice(&x.to_le_bytes());
            }
            while cbuf.len() % 8 != 0 {
                cbuf.push(0);
            }
        } else {
            mflags |= MSG_CTRUNC;
            let th = current_thread();
            for x in fds {
                let _ = th.files.lock().close(x);
            }
        }
    }
    let passcred = s.getsockopt(SOL_SOCKET, SO_PASSCRED).map(|v| v.first().copied().unwrap_or(0) != 0).unwrap_or(false);
    if passcred && cbuf.len() + 32 <= ctllen {
        let (p, u, g) = m.creds.unwrap_or((0, u32::MAX, u32::MAX));
        cbuf.extend_from_slice(&28u64.to_le_bytes());
        cbuf.extend_from_slice(&SOL_SOCKET.to_le_bytes());
        cbuf.extend_from_slice(&2i32.to_le_bytes());
        for x in [p, u, g] {
            cbuf.extend_from_slice(&x.to_le_bytes());
        }
        cbuf.extend_from_slice(&[0; 4]);
    }
    if !cbuf.is_empty() {
        copy_to_user(ctl, &cbuf)?;
    }
    write_user(msg + 40, &cbuf.len())?;
    write_user(msg + 48, &(mflags as i32))?;
    Ok(if flags & MSG_TRUNC != 0 && m.flags & MSG_TRUNC != 0 { total.max(m.data.len()) } else { m.data.len() })
}

pub fn sendmmsg(fd: i32, vec: usize, n: u32, flags: u32) -> Result<usize> {
    let mut done = 0;
    for i in 0..n.min(1024) as usize {
        match sendmsg(fd, vec + 64 * i, flags) {
            Ok(len) => {
                write_user(vec + 64 * i + 56, &(len as u32))?;
                done += 1;
            }
            Err(e) => return if done > 0 { Ok(done) } else { Err(e) },
        }
    }
    Ok(done)
}

pub fn recvmmsg(fd: i32, vec: usize, n: u32, flags: u32, _timeout: usize) -> Result<usize> {
    let mut done = 0;
    for i in 0..n.min(1024) as usize {
        let fl = if done > 0 { flags | MSG_DONTWAIT } else { flags };
        match recvmsg(fd, vec + 64 * i, fl) {
            Ok(len) => {
                write_user(vec + 64 * i + 56, &(len as u32))?;
                done += 1;
            }
            Err(e) => return if done > 0 { Ok(done) } else { Err(e) },
        }
    }
    Ok(done)
}
