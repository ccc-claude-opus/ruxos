//! Linux error numbers (include/uapi/asm-generic/errno-base.h, errno.h).
#![allow(clippy::upper_case_acronyms)]

pub type Result<T> = core::result::Result<T, Errno>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Errno(pub i32);

macro_rules! errnos {
    ($($name:ident = $v:expr),* $(,)?) => { $(pub const $name: Errno = Errno($v);)* };
}
errnos! {
    EPERM = 1, ENOENT = 2, ESRCH = 3, EINTR = 4, EIO = 5, ENXIO = 6, E2BIG = 7, ENOEXEC = 8,
    EBADF = 9, ECHILD = 10, EAGAIN = 11, ENOMEM = 12, EACCES = 13, EFAULT = 14, ENOTBLK = 15,
    EBUSY = 16, EEXIST = 17, EXDEV = 18, ENODEV = 19, ENOTDIR = 20, EISDIR = 21, EINVAL = 22,
    ENFILE = 23, EMFILE = 24, ENOTTY = 25, ETXTBSY = 26, EFBIG = 27, ENOSPC = 28, ESPIPE = 29,
    EROFS = 30, EMLINK = 31, EPIPE = 32, EDOM = 33, ERANGE = 34, EDEADLK = 35, ENAMETOOLONG = 36,
    ENOLCK = 37, ENOSYS = 38, ENOTEMPTY = 39, ELOOP = 40, ENOMSG = 42, ENODATA = 61, ETIME = 62,
    EOVERFLOW = 75, EBADFD = 77, EILSEQ = 84, ERESTART = 85, EUSERS = 87, ENOTSOCK = 88, EDESTADDRREQ = 89,
    EMSGSIZE = 90, EPROTOTYPE = 91, ENOPROTOOPT = 92, EPROTONOSUPPORT = 93, ESOCKTNOSUPPORT = 94,
    EOPNOTSUPP = 95, EPFNOSUPPORT = 96, EAFNOSUPPORT = 97, EADDRINUSE = 98, EADDRNOTAVAIL = 99,
    ENETDOWN = 100, ENETUNREACH = 101, ENETRESET = 102, ECONNABORTED = 103, ECONNRESET = 104,
    ENOBUFS = 105, EISCONN = 106, ENOTCONN = 107, ESHUTDOWN = 108, ETIMEDOUT = 110,
    ECONNREFUSED = 111, EHOSTDOWN = 112, EHOSTUNREACH = 113, EALREADY = 114, EINPROGRESS = 115,
    ESTALE = 116, EDQUOT = 122, ECANCELED = 125, EOWNERDEAD = 130, ENOTRECOVERABLE = 131,
    // kernel-internal: restart the system call after the signal handler (never seen by user space)
    ERESTARTSYS = 512, ERESTARTNOINTR = 513, ERESTARTNOHAND = 514, ERESTART_RESTARTBLOCK = 516,
}

impl Errno {
    pub fn neg(self) -> isize {
        -(self.0 as isize)
    }
}
