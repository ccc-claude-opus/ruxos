//! Sockets: the socket file, addresses, and the address families (AF_UNIX here; AF_INET and
//! AF_NETLINK in the network milestone).
pub mod unix;

use crate::errno::*;
use crate::vfs::file::{File, FileOps};
use crate::vfs::poll::PollQueue;
use crate::vfs::Inode;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

pub const AF_UNIX: u16 = 1;
pub const AF_INET: u16 = 2;
pub const AF_INET6: u16 = 10;
pub const AF_NETLINK: u16 = 16;
pub const SOCK_STREAM: u32 = 1;
pub const SOCK_DGRAM: u32 = 2;
pub const SOCK_RAW: u32 = 3;
pub const SOCK_SEQPACKET: u32 = 5;
pub const SOCK_NONBLOCK: u32 = 0o4000;
pub const SOCK_CLOEXEC: u32 = 0o2000000;

pub const MSG_OOB: u32 = 1;
pub const MSG_PEEK: u32 = 2;
pub const MSG_DONTWAIT: u32 = 0x40;
pub const MSG_TRUNC: u32 = 0x20;
pub const MSG_CTRUNC: u32 = 0x8;
pub const MSG_WAITALL: u32 = 0x100;
pub const MSG_NOSIGNAL: u32 = 0x4000;
pub const MSG_CMSG_CLOEXEC: u32 = 0x4000_0000;

pub const SOL_SOCKET: i32 = 1;
pub const SO_REUSEADDR: i32 = 2;
pub const SO_TYPE: i32 = 3;
pub const SO_ERROR: i32 = 4;
pub const SO_SNDBUF: i32 = 7;
pub const SO_RCVBUF: i32 = 8;
pub const SO_KEEPALIVE: i32 = 9;
pub const SO_PASSCRED: i32 = 16;
pub const SO_PEERCRED: i32 = 17;
pub const SO_ACCEPTCONN: i32 = 30;
pub const SO_DOMAIN: i32 = 39;
pub const SO_PROTOCOL: i32 = 38;

/// A message to send or the result of a receive.
#[derive(Default)]
pub struct Msg {
    pub data: Vec<u8>,
    /// SCM_RIGHTS
    pub fds: Vec<Arc<File>>,
    /// sockaddr bytes (sendto destination, or recvfrom source)
    pub addr: Option<Vec<u8>>,
    /// set by recv: MSG_TRUNC etc.
    pub flags: u32,
    /// SCM_CREDENTIALS (pid, uid, gid)
    pub creds: Option<(u32, u32, u32)>,
}

#[allow(unused_variables)]
pub trait Socket: Send + Sync + Any {
    fn bind(&self, addr: &[u8]) -> Result<()> {
        Err(EOPNOTSUPP)
    }
    fn listen(&self, backlog: i32) -> Result<()> {
        Err(EOPNOTSUPP)
    }
    fn accept(&self, nonblock: bool) -> Result<(Arc<dyn Socket>, Vec<u8>)> {
        Err(EOPNOTSUPP)
    }
    fn connect(&self, addr: &[u8], nonblock: bool) -> Result<()> {
        Err(EOPNOTSUPP)
    }
    fn send(&self, msg: Msg, flags: u32, nonblock: bool) -> Result<usize>;
    /// Receives up to `len` bytes.
    fn recv(&self, len: usize, flags: u32, nonblock: bool) -> Result<Msg>;
    fn shutdown(&self, how: u32) -> Result<()> {
        Err(ENOTCONN)
    }
    fn local_addr(&self) -> Result<Vec<u8>>;
    fn peer_addr(&self) -> Result<Vec<u8>> {
        Err(ENOTCONN)
    }
    fn setsockopt(&self, level: i32, name: i32, val: &[u8]) -> Result<()> {
        Ok(())
    }
    fn getsockopt(&self, level: i32, name: i32) -> Result<Vec<u8>> {
        Err(ENOPROTOOPT)
    }
    fn poll(&self) -> u32;
    fn poll_queue(&self) -> &PollQueue;
    fn ioctl(&self, cmd: u32, arg: usize) -> Result<isize> {
        Err(ENOTTY)
    }
    fn family(&self) -> u16;
    fn sock_type(&self) -> u32;
    fn as_any(&self) -> &dyn Any;
}

pub struct SocketFile {
    pub sock: Arc<dyn Socket>,
    pub inode: Arc<dyn Inode>,
}

impl SocketFile {
    pub fn new(sock: Arc<dyn Socket>) -> Arc<SocketFile> {
        Arc::new(SocketFile { sock, inode: crate::vfs::anon::AnonInode::new(crate::vfs::S_IFSOCK | 0o777) })
    }
}

impl FileOps for SocketFile {
    fn read(&self, f: &File, buf: &mut [u8]) -> Result<usize> {
        let m = self.sock.recv(buf.len(), 0, f.nonblock())?;
        let n = m.data.len().min(buf.len());
        buf[..n].copy_from_slice(&m.data[..n]);
        Ok(n)
    }
    fn write(&self, f: &File, buf: &[u8]) -> Result<usize> {
        self.sock.send(Msg { data: buf.to_vec(), ..Default::default() }, 0, f.nonblock())
    }
    fn poll(&self, _f: &File) -> u32 {
        self.sock.poll()
    }
    fn poll_queue(&self) -> Option<&PollQueue> {
        Some(self.sock.poll_queue())
    }
    fn ioctl(&self, _f: &File, cmd: u32, arg: usize) -> Result<isize> {
        self.sock.ioctl(cmd, arg)
    }
    fn inode(&self) -> Option<Arc<dyn Inode>> {
        Some(self.inode.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub fn sock_of(f: &File) -> Result<Arc<dyn Socket>> {
    f.ops.as_any().downcast_ref::<SocketFile>().map(|s| s.sock.clone()).ok_or(ENOTSOCK)
}

pub fn create(family: u16, ty: u32, proto: i32) -> Result<Arc<dyn Socket>> {
    match family {
        AF_UNIX => unix::UnixSocket::new(ty),
        AF_INET | AF_INET6 => crate::net::inet_socket(family, ty, proto),
        AF_NETLINK => crate::net::netlink_socket(ty, proto),
        _ => Err(EAFNOSUPPORT),
    }
}

/// Filled in by the network milestone.
pub fn inet_socket(_family: u16, _ty: u32, _proto: i32) -> Result<Arc<dyn Socket>> {
    Err(EAFNOSUPPORT)
}

pub fn netlink_socket(_ty: u32, _proto: i32) -> Result<Arc<dyn Socket>> {
    Err(EAFNOSUPPORT)
}
