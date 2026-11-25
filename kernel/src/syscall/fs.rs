//! File system calls.
use crate::errno::*;
use crate::mm::vm::{copy_from_user, copy_to_user, read_cstr, read_user, write_user};
use crate::task::{current_thread, Cred};
use crate::vfs::file::*;
use crate::vfs::path::{may, Lookup, MNT_RDONLY};
use crate::vfs::*;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub const AT_FDCWD: i32 = -100;
pub const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
pub const AT_REMOVEDIR: u32 = 0x200;
pub const AT_SYMLINK_FOLLOW: u32 = 0x400;
pub const AT_EACCESS: u32 = 0x200;
pub const AT_EMPTY_PATH: u32 = 0x1000;

pub fn file(fd: i32) -> Result<Arc<File>> {
    current_thread().files.lock().get(fd)
}

pub fn cred() -> Cred {
    current_thread().proc.cred()
}

fn root() -> PathRef {
    current_thread().fs.lock().root.clone()
}

/// Where a relative path starts: the cwd or the directory of `dirfd`.
pub fn start(dirfd: i32) -> Result<PathRef> {
    if dirfd == AT_FDCWD {
        return Ok(current_thread().fs.lock().cwd.clone());
    }
    let f = file(dirfd)?;
    f.path.clone().ok_or(ENOTDIR)
}

pub fn user_path(p: usize) -> Result<Vec<u8>> {
    if p == 0 {
        return Err(EFAULT);
    }
    read_cstr(p, 4096)
}

/// Resolves (dirfd, path) to an existing object.
pub fn lookup_at(dirfd: i32, path: &[u8], follow: bool, empty_ok: bool) -> Result<PathRef> {
    if path.is_empty() {
        if !empty_ok {
            return Err(ENOENT);
        }
        if dirfd == AT_FDCWD {
            return start(dirfd);
        }
        let f = file(dirfd)?;
        return f.path.clone().ok_or(EBADF);
    }
    let c = cred();
    let r = root();
    let st = start(dirfd)?;
    Lookup { root: &r, cred: &c, follow_last: follow, beneath: false, no_symlinks: false }.resolve(&st, path)
}

fn parent_at(dirfd: i32, path: &[u8]) -> Result<(PathRef, Vec<u8>, bool)> {
    let c = cred();
    let r = root();
    let st = start(dirfd)?;
    Lookup { root: &r, cred: &c, follow_last: true, beneath: false, no_symlinks: false }.resolve_parent(&st, path)
}

fn check_writable_mount(p: &PathRef) -> Result<()> {
    if p.mnt.flags() & MNT_RDONLY != 0 { Err(EROFS) } else { Ok(()) }
}

/// Opens (dirfd, path) with open(2) flags.
pub fn open_path(dirfd: i32, path: &[u8], flags: u32, mode: u32) -> Result<Arc<File>> {
    if path.is_empty() {
        return Err(ENOENT);
    }
    let c = cred();
    let th = current_thread();
    let umask = th.fs.lock().umask;
    let r = root();
    let st = start(dirfd)?;
    let follow = flags & O_NOFOLLOW == 0;
    let lk = Lookup { root: &r, cred: &c, follow_last: follow, beneath: false, no_symlinks: false };
    let mut created = false;
    let p = if flags & O_CREAT != 0 {
        let (dir, name, trailing) = lk.resolve_parent(&st, path)?;
        if name.is_empty() || name == b"." || name == b".." || trailing {
            return Err(EISDIR);
        }
        let d = dir.d.child(&name)?;
        let existing = d.inode.lock().clone();
        match existing {
            Some(i) => {
                if flags & O_EXCL != 0 {
                    return Err(EEXIST);
                }
                if path::is_symlink(&i) {
                    if !follow {
                        return Err(ELOOP);
                    }
                    lk.resolve(&st, path)?
                } else {
                    PathRef { mnt: dir.mnt.clone(), d }.clone()
                }
            }
            None => {
                let dinode = dir.inode()?;
                may(&dinode, 3, &c)?;
                check_writable_mount(&dir)?;
                let gid = if dinode.stat()?.mode & S_ISGID != 0 { dinode.stat()?.gid } else { c.fsgid };
                let i = dinode.create(&name, S_IFREG | (mode & 0o7777 & !umask), 0, c.fsuid, gid)?;
                dir.d.set_child(&name, Some(i));
                created = true;
                let d = dir.d.child(&name)?;
                PathRef { mnt: dir.mnt.clone(), d }
            }
        }
    } else {
        lk.resolve(&st, path)?
    };
    let inode = p.inode()?;
    let stat = inode.stat()?;
    let ftype = stat.mode & S_IFMT;
    if flags & O_PATH != 0 {
        return Ok(File::new(ops_for(&inode, flags)?, flags, Some(p)));
    }
    if ftype == S_IFLNK {
        return Err(ELOOP);
    }
    if flags & O_DIRECTORY != 0 && ftype != S_IFDIR {
        return Err(ENOTDIR);
    }
    let acc = flags & O_ACCMODE;
    if ftype == S_IFDIR && (acc != O_RDONLY || flags & O_TRUNC != 0) {
        return Err(EISDIR);
    }
    if !created {
        let want = match acc {
            O_RDONLY => 4,
            O_WRONLY => 2,
            _ => 6,
        } | if flags & O_TRUNC != 0 { 2 } else { 0 };
        may(&inode, want, &c)?;
    }
    if (acc != O_RDONLY || flags & O_TRUNC != 0) && ftype == S_IFREG {
        check_writable_mount(&p)?;
    }
    if flags & O_TRUNC != 0 && ftype == S_IFREG && !created {
        inode.setattr(&SetAttr { size: Some(0), mtime: Some(Timespec::now()), ..Default::default() })?;
    }
    let ops = ops_for(&inode, flags)?;
    Ok(File::new(ops, flags, Some(p)))
}

pub fn openat(dirfd: i32, path: usize, flags: u32, mode: u32) -> Result<usize> {
    let path = user_path(path)?;
    let f = open_path(dirfd, &path, flags, mode)?;
    let th = current_thread();
    let mut fds = th.files.lock();
    fds.limit = th.proc.rlimit(crate::task::RLIMIT_NOFILE).0.min(1 << 20) as usize;
    Ok(fds.alloc(f, 0, flags & O_CLOEXEC != 0)? as usize)
}

pub fn close(fd: i32) -> Result<usize> {
    let f = current_thread().files.lock().close(fd)?;
    crate::locks::on_close(&f, current_thread().proc.pid);
    drop(f);
    Ok(0)
}

pub fn close_range(first: u32, last: u32, flags: u32) -> Result<usize> {
    let th = current_thread();
    let mut fds = th.files.lock();
    let end = (last as usize).min(fds.fds.len().saturating_sub(1));
    let mut dropped = Vec::new();
    for fd in first as usize..=end {
        if flags & 4 != 0 {
            // CLOSE_RANGE_CLOEXEC
            let _ = fds.set_cloexec(fd as i32, true);
        } else if let Ok(f) = fds.close(fd as i32) {
            dropped.push(f);
        }
    }
    drop(fds);
    drop(dropped);
    Ok(0)
}

const CHUNK: usize = 1 << 16;

pub fn read(fd: i32, buf: usize, len: usize) -> Result<usize> {
    let f = file(fd)?;
    if !f.readable() {
        return Err(EBADF);
    }
    let mut kb = alloc::vec![0u8; len.min(CHUNK)];
    let mut done = 0;
    while done < len {
        let want = (len - done).min(CHUNK);
        let n = match f.read(&mut kb[..want]) {
            Ok(n) => n,
            Err(e) if done > 0 && e != EFAULT => break,
            Err(e) => return Err(e),
        };
        copy_to_user(buf + done, &kb[..n])?;
        done += n;
        if n < want {
            break;
        }
    }
    Ok(done)
}

pub fn write(fd: i32, buf: usize, len: usize) -> Result<usize> {
    let f = file(fd)?;
    if !f.writable() {
        return Err(EBADF);
    }
    let mut kb = alloc::vec![0u8; len.min(CHUNK)];
    let mut done = 0;
    while done < len {
        let want = (len - done).min(CHUNK);
        copy_from_user(&mut kb[..want], buf + done)?;
        let n = match f.write(&kb[..want]) {
            Ok(n) => n,
            Err(e) if done > 0 => {
                let _ = e;
                break;
            }
            Err(e) => return Err(e),
        };
        done += n;
        if n < want {
            break;
        }
    }
    Ok(done)
}

pub fn pread64(fd: i32, buf: usize, len: usize, off: i64) -> Result<usize> {
    if off < 0 {
        return Err(EINVAL);
    }
    let f = file(fd)?;
    if !f.readable() {
        return Err(EBADF);
    }
    let mut kb = alloc::vec![0u8; len.min(1 << 20)];
    let n = f.ops.pread(&f, off as u64, &mut kb)?;
    copy_to_user(buf, &kb[..n])?;
    Ok(n)
}

pub fn pwrite64(fd: i32, buf: usize, len: usize, off: i64) -> Result<usize> {
    if off < 0 {
        return Err(EINVAL);
    }
    let f = file(fd)?;
    if !f.writable() {
        return Err(EBADF);
    }
    let mut kb = alloc::vec![0u8; len.min(1 << 20)];
    copy_from_user(&mut kb, buf)?;
    f.ops.pwrite(&f, off as u64, &kb)
}

fn iovecs(iov: usize, cnt: usize) -> Result<Vec<(usize, usize)>> {
    if cnt > 1024 {
        return Err(EINVAL);
    }
    let mut v = Vec::with_capacity(cnt);
    for i in 0..cnt {
        let e: [usize; 2] = read_user(iov + 16 * i)?;
        v.push((e[0], e[1]));
    }
    Ok(v)
}

pub fn readv(fd: i32, iov: usize, cnt: usize, off: Option<i64>) -> Result<usize> {
    let mut total = 0;
    let mut pos = off;
    for (base, len) in iovecs(iov, cnt)? {
        if len == 0 {
            continue;
        }
        let n = match pos {
            Some(p) => pread64(fd, base, len, p)?,
            None => match read(fd, base, len) {
                Ok(n) => n,
                Err(e) if total > 0 && e == EAGAIN => break,
                Err(e) => return Err(e),
            },
        };
        total += n;
        if let Some(p) = pos.as_mut() {
            *p += n as i64;
        }
        if n < len {
            break;
        }
    }
    Ok(total)
}

pub fn writev(fd: i32, iov: usize, cnt: usize, off: Option<i64>) -> Result<usize> {
    let vecs = iovecs(iov, cnt)?;
    // gather into one buffer so that the write is one operation (pipes, sockets)
    let total: usize = vecs.iter().map(|v| v.1).sum();
    if total <= CHUNK {
        let mut kb = alloc::vec![0u8; total];
        let mut o = 0;
        for (b, l) in &vecs {
            copy_from_user(&mut kb[o..o + l], *b)?;
            o += l;
        }
        let f = file(fd)?;
        if !f.writable() {
            return Err(EBADF);
        }
        return match off {
            Some(p) => f.ops.pwrite(&f, p as u64, &kb),
            None => f.write(&kb),
        };
    }
    let mut done = 0;
    let mut pos = off;
    for (base, len) in vecs {
        let n = match pos {
            Some(p) => pwrite64(fd, base, len, p)?,
            None => write(fd, base, len)?,
        };
        done += n;
        if let Some(p) = pos.as_mut() {
            *p += n as i64;
        }
        if n < len {
            break;
        }
    }
    Ok(done)
}

pub fn sendfile(out_fd: i32, in_fd: i32, offp: usize, count: usize) -> Result<usize> {
    let inf = file(in_fd)?;
    let outf = file(out_fd)?;
    let mut off: Option<u64> = if offp != 0 { Some(read_user::<u64>(offp)?) } else { None };
    let mut kb = alloc::vec![0u8; count.min(CHUNK)];
    let mut done = 0;
    while done < count {
        let want = (count - done).min(CHUNK);
        let n = match off {
            Some(o) => inf.ops.pread(&inf, o, &mut kb[..want])?,
            None => inf.read(&mut kb[..want])?,
        };
        if n == 0 {
            break;
        }
        let w = outf.write(&kb[..n])?;
        done += w;
        if let Some(o) = off.as_mut() {
            *o += w as u64;
        }
        if w < n {
            break;
        }
    }
    if let Some(o) = off {
        write_user(offp, &o)?;
    }
    Ok(done)
}

pub fn lseek(fd: i32, off: i64, whence: u32) -> Result<usize> {
    let f = file(fd)?;
    Ok(f.ops.seek(&f, off, whence)? as usize)
}

pub fn dup(fd: i32) -> Result<usize> {
    let th = current_thread();
    let mut fds = th.files.lock();
    let f = fds.get(fd)?;
    Ok(fds.alloc(f, 0, false)? as usize)
}

pub fn dup3(old: i32, new: i32, flags: u32) -> Result<usize> {
    if old == new {
        return Err(EINVAL);
    }
    if new < 0 || new as usize >= 1 << 20 {
        return Err(EBADF);
    }
    let th = current_thread();
    let mut fds = th.files.lock();
    let f = fds.get(old)?;
    let prev = fds.set(new as usize, f, flags & O_CLOEXEC != 0);
    drop(fds);
    drop(prev);
    Ok(new as usize)
}

pub fn fcntl(fd: i32, cmd: u32, arg: usize) -> Result<usize> {
    let th = current_thread();
    match cmd {
        0 | 1030 => {
            // F_DUPFD, F_DUPFD_CLOEXEC
            let mut fds = th.files.lock();
            let f = fds.get(fd)?;
            Ok(fds.alloc(f, arg, cmd == 1030)? as usize)
        }
        1 => Ok(th.files.lock().cloexec(fd)? as usize),
        2 => {
            th.files.lock().set_cloexec(fd, arg as u32 & FD_CLOEXEC != 0)?;
            Ok(0)
        }
        3 => Ok(file(fd)?.flags() as usize | O_LARGEFILE as usize),
        4 => {
            let f = file(fd)?;
            let old = f.flags();
            f.flags.store((old & !SETFL_MASK) | (arg as u32 & SETFL_MASK), core::sync::atomic::Ordering::Relaxed);
            Ok(0)
        }
        5..=7 | 36..=38 => crate::locks::fcntl_lock(&file(fd)?, cmd, arg),
        8 | 9 | 10 | 11 | 15 | 16 => Ok(0), // F_SETOWN/GETOWN/SETSIG/GETSIG/SETOWN_EX/GETOWN_EX
        1031 => Ok(65536), // F_SETPIPE_SZ
        1032 => Ok(65536), // F_GETPIPE_SZ
        1033 => Err(EINVAL), // F_ADD_SEALS: no sealing (memfd later)
        1034 => Ok(0),
        _ => Err(EINVAL),
    }
}

pub fn flock(fd: i32, op: u32) -> Result<usize> {
    crate::locks::flock(&file(fd)?, op)
}

pub fn ioctl(fd: i32, cmd: u32, arg: usize) -> Result<usize> {
    let f = file(fd)?;
    match cmd {
        0x5421 => {
            // FIONBIO
            let on: i32 = read_user(arg)?;
            if on != 0 {
                f.flags.fetch_or(O_NONBLOCK, core::sync::atomic::Ordering::Relaxed);
            } else {
                f.flags.fetch_and(!O_NONBLOCK, core::sync::atomic::Ordering::Relaxed);
            }
            Ok(0)
        }
        0x5451 | 0x5450 => {
            // FIOCLEX, FIONCLEX
            current_thread().files.lock().set_cloexec(fd, cmd == 0x5451)?;
            Ok(0)
        }
        _ => Ok(f.ops.ioctl(&f, cmd, arg)? as usize),
    }
}

pub fn pipe2(fds_ptr: usize, flags: u32) -> Result<usize> {
    if flags & !(O_CLOEXEC | O_NONBLOCK | O_DIRECT) != 0 {
        return Err(EINVAL);
    }
    let p = pipe::Pipe::new();
    let r = File::new(pipe::PipeEnd::new(&p, true, false), O_RDONLY | (flags & O_NONBLOCK), None);
    let w = File::new(pipe::PipeEnd::new(&p, false, true), O_WRONLY | (flags & O_NONBLOCK), None);
    let th = current_thread();
    let mut fds = th.files.lock();
    let a = fds.alloc(r, 0, flags & O_CLOEXEC != 0)?;
    let b = match fds.alloc(w, 0, flags & O_CLOEXEC != 0) {
        Ok(b) => b,
        Err(e) => {
            let _ = fds.close(a);
            return Err(e);
        }
    };
    drop(fds);
    write_user(fds_ptr, &[a, b])?;
    Ok(0)
}

pub fn stat_bytes(s: &Stat) -> [u8; 128] {
    let mut b = [0u8; 128];
    let p64 = |b: &mut [u8; 128], o: usize, v: u64| b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    let p32 = |b: &mut [u8; 128], o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    p64(&mut b, 0, s.dev);
    p64(&mut b, 8, s.ino);
    p32(&mut b, 16, s.mode);
    p32(&mut b, 20, s.nlink);
    p32(&mut b, 24, s.uid);
    p32(&mut b, 28, s.gid);
    p64(&mut b, 32, s.rdev);
    p64(&mut b, 48, s.size);
    p32(&mut b, 56, s.blksize);
    p64(&mut b, 64, s.blocks);
    p64(&mut b, 72, s.atime.sec as u64);
    p64(&mut b, 80, s.atime.nsec as u64);
    p64(&mut b, 88, s.mtime.sec as u64);
    p64(&mut b, 96, s.mtime.nsec as u64);
    p64(&mut b, 104, s.ctime.sec as u64);
    p64(&mut b, 112, s.ctime.nsec as u64);
    b
}

pub fn fstat(fd: i32, buf: usize) -> Result<usize> {
    let f = file(fd)?;
    let i = f.inode().ok_or(EBADF)?;
    copy_to_user(buf, &stat_bytes(&i.stat()?))?;
    Ok(0)
}

pub fn newfstatat(dirfd: i32, path: usize, buf: usize, flags: u32) -> Result<usize> {
    let path = user_path(path)?;
    let i = if path.is_empty() && flags & AT_EMPTY_PATH != 0 && dirfd != AT_FDCWD {
        file(dirfd)?.inode().ok_or(EBADF)?
    } else {
        lookup_at(dirfd, &path, flags & AT_SYMLINK_NOFOLLOW == 0, flags & AT_EMPTY_PATH != 0)?.inode()?
    };
    copy_to_user(buf, &stat_bytes(&i.stat()?))?;
    Ok(0)
}

pub fn statx(dirfd: i32, path: usize, flags: u32, _mask: u32, buf: usize) -> Result<usize> {
    let path = user_path(path)?;
    let (i, mnt_id) = if path.is_empty() && flags & AT_EMPTY_PATH != 0 && dirfd != AT_FDCWD {
        let f = file(dirfd)?;
        (f.inode().ok_or(EBADF)?, f.path.as_ref().map_or(0, |p| p.mnt.id))
    } else {
        let p = lookup_at(dirfd, &path, flags & AT_SYMLINK_NOFOLLOW == 0, flags & AT_EMPTY_PATH != 0)?;
        (p.inode()?, p.mnt.id)
    };
    let s = i.stat()?;
    let mut b = [0u8; 256];
    let p64 = |b: &mut [u8; 256], o: usize, v: u64| b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    let p32 = |b: &mut [u8; 256], o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    p32(&mut b, 0, 0x7ff | 0x800 | 0x1000);
    p32(&mut b, 4, s.blksize);
    p32(&mut b, 16, s.nlink);
    p32(&mut b, 20, s.uid);
    p32(&mut b, 24, s.gid);
    b[28..30].copy_from_slice(&(s.mode as u16).to_le_bytes());
    p64(&mut b, 32, s.ino);
    p64(&mut b, 40, s.size);
    p64(&mut b, 48, s.blocks);
    for (o, t) in [(64, s.atime), (80, s.btime), (96, s.ctime), (112, s.mtime)] {
        p64(&mut b, o, t.sec as u64);
        p32(&mut b, o + 8, t.nsec as u32);
    }
    p32(&mut b, 128, major(s.rdev));
    p32(&mut b, 132, minor(s.rdev));
    p32(&mut b, 136, major(s.dev));
    p32(&mut b, 140, minor(s.dev));
    p64(&mut b, 144, mnt_id as u64);
    copy_to_user(buf, &b)?;
    Ok(0)
}

pub fn getdents64(fd: i32, buf: usize, len: usize) -> Result<usize> {
    let f = file(fd)?;
    let i = f.inode().ok_or(EBADF)?;
    if i.stat()?.mode & S_IFMT != S_IFDIR {
        return Err(ENOTDIR);
    }
    let mut pos = f.pos.lock();
    let mut out: Vec<u8> = Vec::new();
    let mut next = *pos;
    let mut too_small = false;
    i.readdir(*pos, &mut |name, ino, dt, np| {
        let reclen = (19 + name.len() + 1 + 7) & !7;
        if out.len() + reclen > len {
            if out.is_empty() {
                too_small = true;
            }
            return false;
        }
        let o = out.len();
        out.resize(o + reclen, 0);
        out[o..o + 8].copy_from_slice(&ino.to_le_bytes());
        out[o + 8..o + 16].copy_from_slice(&(np as i64).to_le_bytes());
        out[o + 16..o + 18].copy_from_slice(&(reclen as u16).to_le_bytes());
        out[o + 18] = dt;
        out[o + 19..o + 19 + name.len()].copy_from_slice(name);
        next = np;
        true
    })?;
    if too_small {
        return Err(EINVAL);
    }
    copy_to_user(buf, &out)?;
    *pos = next;
    Ok(out.len())
}

pub fn getcwd(buf: usize, size: usize) -> Result<usize> {
    let th = current_thread();
    let (cwd, root) = {
        let f = th.fs.lock();
        (f.cwd.clone(), f.root.clone())
    };
    let mut p = cwd.path_from(&root);
    p.push(0);
    if p.len() > size {
        return Err(ERANGE);
    }
    copy_to_user(buf, &p)?;
    Ok(p.len())
}

pub fn chdir(path: usize) -> Result<usize> {
    let path = user_path(path)?;
    let p = lookup_at(AT_FDCWD, &path, true, false)?;
    let i = p.inode()?;
    if !is_dir(&i) {
        return Err(ENOTDIR);
    }
    may(&i, 1, &cred())?;
    current_thread().fs.lock().cwd = p;
    Ok(0)
}

pub fn fchdir(fd: i32) -> Result<usize> {
    let f = file(fd)?;
    let p = f.path.clone().ok_or(ENOTDIR)?;
    if !is_dir(&p.inode()?) {
        return Err(ENOTDIR);
    }
    current_thread().fs.lock().cwd = p;
    Ok(0)
}

pub fn chroot(path: usize) -> Result<usize> {
    if !cred().capable(18) {
        return Err(EPERM);
    }
    let path = user_path(path)?;
    let p = lookup_at(AT_FDCWD, &path, true, false)?;
    if !is_dir(&p.inode()?) {
        return Err(ENOTDIR);
    }
    current_thread().fs.lock().root = p;
    Ok(0)
}

pub fn umask(m: u32) -> Result<usize> {
    let th = current_thread();
    let mut f = th.fs.lock();
    let old = f.umask;
    f.umask = m & 0o777;
    Ok(old as usize)
}

pub fn mkdirat(dirfd: i32, path: usize, mode: u32) -> Result<usize> {
    let path = user_path(path)?;
    let (dir, name, _) = parent_at(dirfd, &path)?;
    if name.is_empty() || name == b"." || name == b".." {
        return Err(EEXIST);
    }
    let c = cred();
    let di = dir.inode()?;
    if dir.d.child(&name)?.inode.lock().is_some() {
        return Err(EEXIST);
    }
    may(&di, 3, &c)?;
    check_writable_mount(&dir)?;
    let umask = current_thread().fs.lock().umask;
    let dst = di.stat()?;
    let gid = if dst.mode & S_ISGID != 0 { dst.gid } else { c.fsgid };
    let mode = (mode & 0o7777 & !umask) | if dst.mode & S_ISGID != 0 { S_ISGID } else { 0 };
    let i = di.create(&name, S_IFDIR | mode, 0, c.fsuid, gid)?;
    dir.d.set_child(&name, Some(i));
    Ok(0)
}

pub fn mknodat(dirfd: i32, path: usize, mode: u32, dev: u64) -> Result<usize> {
    let path = user_path(path)?;
    let (dir, name, _) = parent_at(dirfd, &path)?;
    let c = cred();
    let di = dir.inode()?;
    if dir.d.child(&name)?.inode.lock().is_some() {
        return Err(EEXIST);
    }
    let t = mode & S_IFMT;
    let t = if t == 0 { S_IFREG } else { t };
    if (t == S_IFCHR || t == S_IFBLK) && !c.capable(27) {
        return Err(EPERM);
    }
    if t == S_IFDIR {
        return Err(EINVAL);
    }
    may(&di, 3, &c)?;
    check_writable_mount(&dir)?;
    let umask = current_thread().fs.lock().umask;
    let i = di.create(&name, t | (mode & 0o7777 & !umask), dev, c.fsuid, c.fsgid)?;
    dir.d.set_child(&name, Some(i));
    Ok(0)
}

fn sticky_ok(dir: &Arc<dyn Inode>, victim: &Arc<dyn Inode>, c: &Cred) -> Result<()> {
    let ds = dir.stat()?;
    if ds.mode & S_ISVTX != 0 && c.fsuid != 0 && c.fsuid != ds.uid && c.fsuid != victim.stat()?.uid {
        return Err(EPERM);
    }
    Ok(())
}

pub fn unlinkat(dirfd: i32, path: usize, flags: u32) -> Result<usize> {
    let path = user_path(path)?;
    let (dir, name, _) = parent_at(dirfd, &path)?;
    if name == b"." {
        return Err(EINVAL);
    }
    if name == b".." || name.is_empty() {
        return Err(ENOTEMPTY);
    }
    let c = cred();
    let di = dir.inode()?;
    let victim = dir.d.child(&name)?.inode()?;
    may(&di, 3, &c)?;
    check_writable_mount(&dir)?;
    sticky_ok(&di, &victim, &c)?;
    if flags & AT_REMOVEDIR != 0 {
        if !is_dir(&victim) {
            return Err(ENOTDIR);
        }
        di.rmdir(&name)?;
    } else {
        if is_dir(&victim) {
            return Err(EISDIR);
        }
        di.unlink(&name)?;
    }
    dir.d.drop_child(&name);
    Ok(0)
}

pub fn symlinkat(target: usize, dirfd: i32, path: usize) -> Result<usize> {
    let target = user_path(target)?;
    let path = user_path(path)?;
    let (dir, name, _) = parent_at(dirfd, &path)?;
    let c = cred();
    let di = dir.inode()?;
    if dir.d.child(&name)?.inode.lock().is_some() {
        return Err(EEXIST);
    }
    may(&di, 3, &c)?;
    check_writable_mount(&dir)?;
    let i = di.symlink(&name, &target, c.fsuid, c.fsgid)?;
    dir.d.set_child(&name, Some(i));
    Ok(0)
}

pub fn linkat(olddirfd: i32, old: usize, newdirfd: i32, new: usize, flags: u32) -> Result<usize> {
    let old = user_path(old)?;
    let new = user_path(new)?;
    let src = lookup_at(olddirfd, &old, flags & AT_SYMLINK_FOLLOW != 0, flags & AT_EMPTY_PATH != 0)?;
    let si = src.inode()?;
    if is_dir(&si) {
        return Err(EPERM);
    }
    let (dir, name, _) = parent_at(newdirfd, &new)?;
    if !Arc::ptr_eq(&dir.mnt, &src.mnt) {
        return Err(EXDEV);
    }
    let di = dir.inode()?;
    if dir.d.child(&name)?.inode.lock().is_some() {
        return Err(EEXIST);
    }
    may(&di, 3, &cred())?;
    check_writable_mount(&dir)?;
    di.link(&name, &si)?;
    dir.d.set_child(&name, Some(si));
    Ok(0)
}

pub fn renameat2(od: i32, old: usize, nd: i32, new: usize, flags: u32) -> Result<usize> {
    let old = user_path(old)?;
    let new = user_path(new)?;
    if flags & !7 != 0 || (flags & 1 != 0 && flags & 2 != 0) {
        return Err(EINVAL);
    }
    let (odir, oname, _) = parent_at(od, &old)?;
    let (ndir, nname, _) = parent_at(nd, &new)?;
    if oname.is_empty() || oname == b"." || oname == b".." || nname.is_empty() || nname == b"." || nname == b".." {
        return Err(EBUSY);
    }
    if !Arc::ptr_eq(&odir.mnt, &ndir.mnt) {
        return Err(EXDEV);
    }
    let c = cred();
    let oi = odir.inode()?;
    let ni = ndir.inode()?;
    let victim = odir.d.child(&oname)?.inode()?;
    may(&oi, 3, &c)?;
    may(&ni, 3, &c)?;
    check_writable_mount(&odir)?;
    sticky_ok(&oi, &victim, &c)?;
    // a directory cannot move into itself
    if is_dir(&victim) {
        let mut p = ndir.clone();
        let r = root();
        for _ in 0..4096 {
            if Arc::ptr_eq(&p.d, &odir.d.child(&oname)?) {
                return Err(EINVAL);
            }
            let up = p.up(&r);
            if up.same(&p) {
                break;
            }
            p = up;
        }
    }
    oi.rename(&oname, &ni, &nname, flags)?;
    if flags & 2 != 0 {
        odir.d.drop_child(&oname);
        ndir.d.drop_child(&nname);
    } else {
        odir.d.move_child(&oname, &ndir.d, &nname);
    }
    Ok(0)
}

pub fn readlinkat(dirfd: i32, path: usize, buf: usize, size: usize) -> Result<usize> {
    let path = user_path(path)?;
    let p = lookup_at(dirfd, &path, false, true)?;
    let i = p.inode()?;
    if !path::is_symlink(&i) {
        return Err(EINVAL);
    }
    let t = i.readlink()?;
    let n = t.len().min(size);
    copy_to_user(buf, &t[..n])?;
    Ok(n)
}

pub fn faccessat(dirfd: i32, path: usize, mode: u32, flags: u32) -> Result<usize> {
    let path = user_path(path)?;
    let p = lookup_at(dirfd, &path, flags & AT_SYMLINK_NOFOLLOW == 0, flags & AT_EMPTY_PATH != 0)?;
    let i = p.inode()?;
    if mode == 0 {
        return Ok(0);
    }
    let mut c = cred();
    if flags & AT_EACCESS == 0 {
        c.fsuid = c.uid;
        c.fsgid = c.gid;
    }
    if mode & 2 != 0 && p.mnt.flags() & MNT_RDONLY != 0 && i.stat()?.mode & S_IFMT != S_IFCHR {
        return Err(EROFS);
    }
    may(&i, mode & 7, &c)?;
    Ok(0)
}

fn chmod_inode(i: &Arc<dyn Inode>, mode: u32) -> Result<usize> {
    let c = cred();
    let s = i.stat()?;
    if c.fsuid != 0 && c.fsuid != s.uid {
        return Err(EPERM);
    }
    i.setattr(&SetAttr { mode: Some(mode & 0o7777), ..Default::default() })?;
    Ok(0)
}

pub fn fchmod(fd: i32, mode: u32) -> Result<usize> {
    chmod_inode(&file(fd)?.inode().ok_or(EBADF)?, mode)
}

pub fn fchmodat(dirfd: i32, path: usize, mode: u32, flags: u32) -> Result<usize> {
    let path = user_path(path)?;
    let p = lookup_at(dirfd, &path, flags & AT_SYMLINK_NOFOLLOW == 0, flags & AT_EMPTY_PATH != 0)?;
    check_writable_mount(&p)?;
    chmod_inode(&p.inode()?, mode)
}

fn chown_inode(i: &Arc<dyn Inode>, uid: u32, gid: u32) -> Result<usize> {
    let c = cred();
    let s = i.stat()?;
    let uid = if uid == u32::MAX { None } else { Some(uid) };
    let gid = if gid == u32::MAX { None } else { Some(gid) };
    if !c.capable(0) {
        if uid.is_some_and(|u| u != s.uid) || (c.fsuid != s.uid) {
            return Err(EPERM);
        }
        if gid.is_some_and(|g| g != c.fsgid && !c.groups.contains(&g)) {
            return Err(EPERM);
        }
    }
    let mut a = SetAttr { uid, gid, ..Default::default() };
    if s.mode & S_IFMT == S_IFREG && s.mode & 0o6000 != 0 {
        a.mode = Some(s.mode & 0o1777);
    }
    i.setattr(&a)?;
    Ok(0)
}

pub fn fchown(fd: i32, uid: u32, gid: u32) -> Result<usize> {
    chown_inode(&file(fd)?.inode().ok_or(EBADF)?, uid, gid)
}

pub fn fchownat(dirfd: i32, path: usize, uid: u32, gid: u32, flags: u32) -> Result<usize> {
    let path = user_path(path)?;
    let p = lookup_at(dirfd, &path, flags & AT_SYMLINK_NOFOLLOW == 0, flags & AT_EMPTY_PATH != 0)?;
    check_writable_mount(&p)?;
    chown_inode(&p.inode()?, uid, gid)
}

pub fn utimensat(dirfd: i32, path: usize, times: usize, flags: u32) -> Result<usize> {
    const UTIME_NOW: i64 = (1 << 30) - 1;
    const UTIME_OMIT: i64 = (1 << 30) - 2;
    let i = if path == 0 {
        file(dirfd)?.inode().ok_or(EBADF)?
    } else {
        let p = user_path(path)?;
        lookup_at(dirfd, &p, flags & AT_SYMLINK_NOFOLLOW == 0, flags & AT_EMPTY_PATH != 0)?.inode()?
    };
    let now = Timespec::now();
    let (a, m) = if times == 0 {
        (Some(now), Some(now))
    } else {
        let t: [i64; 4] = read_user(times)?;
        let conv = |s: i64, ns: i64| match ns {
            UTIME_NOW => Some(now),
            UTIME_OMIT => None,
            _ => Some(Timespec { sec: s, nsec: ns }),
        };
        (conv(t[0], t[1]), conv(t[2], t[3]))
    };
    let c = cred();
    let s = i.stat()?;
    if c.fsuid != 0 && c.fsuid != s.uid {
        if times != 0 {
            return Err(EPERM);
        }
        may(&i, 2, &c)?;
    }
    i.setattr(&SetAttr { atime: a, mtime: m, ..Default::default() })?;
    Ok(0)
}

fn statfs_bytes(fs: &StatFs, mnt_flags: u32) -> [u8; 120] {
    let mut b = [0u8; 120];
    let v: [u64; 7] = [fs.fs_type, fs.bsize, fs.blocks, fs.bfree, fs.bavail, fs.files, fs.ffree];
    for (k, x) in v.iter().enumerate() {
        b[8 * k..8 * k + 8].copy_from_slice(&x.to_le_bytes());
    }
    // f_fsid at 56 (2 x i32), f_namelen at 64, f_frsize at 72, f_flags at 80
    b[64..72].copy_from_slice(&fs.namelen.to_le_bytes());
    b[72..80].copy_from_slice(&fs.bsize.to_le_bytes());
    let mut fl = 0x20u64; // ST_VALID... (ST_RELATIME on Linux = 0x1000)
    if mnt_flags & MNT_RDONLY != 0 {
        fl |= 1;
    }
    if mnt_flags & path::MNT_NOSUID != 0 {
        fl |= 2;
    }
    if mnt_flags & path::MNT_NODEV != 0 {
        fl |= 4;
    }
    if mnt_flags & path::MNT_NOEXEC != 0 {
        fl |= 8;
    }
    b[80..88].copy_from_slice(&fl.to_le_bytes());
    b
}

pub fn statfs(path: usize, buf: usize) -> Result<usize> {
    let path = user_path(path)?;
    let p = lookup_at(AT_FDCWD, &path, true, false)?;
    copy_to_user(buf, &statfs_bytes(&p.d.fs.statfs(), p.mnt.flags()))?;
    Ok(0)
}

pub fn fstatfs(fd: i32, buf: usize) -> Result<usize> {
    let f = file(fd)?;
    let p = f.path.clone().ok_or(EINVAL)?;
    copy_to_user(buf, &statfs_bytes(&p.d.fs.statfs(), p.mnt.flags()))?;
    Ok(0)
}

pub fn truncate(path: usize, len: i64) -> Result<usize> {
    if len < 0 {
        return Err(EINVAL);
    }
    let path = user_path(path)?;
    let p = lookup_at(AT_FDCWD, &path, true, false)?;
    let i = p.inode()?;
    if is_dir(&i) {
        return Err(EISDIR);
    }
    may(&i, 2, &cred())?;
    check_writable_mount(&p)?;
    i.setattr(&SetAttr { size: Some(len as u64), mtime: Some(Timespec::now()), ..Default::default() })?;
    Ok(0)
}

pub fn ftruncate(fd: i32, len: i64) -> Result<usize> {
    if len < 0 {
        return Err(EINVAL);
    }
    let f = file(fd)?;
    if !f.writable() {
        return Err(EINVAL);
    }
    let i = f.inode().ok_or(EINVAL)?;
    if i.stat()?.mode & S_IFMT != S_IFREG {
        return Err(EINVAL);
    }
    i.setattr(&SetAttr { size: Some(len as u64), mtime: Some(Timespec::now()), ..Default::default() })?;
    Ok(0)
}

pub fn fallocate(fd: i32, mode: u32, off: i64, len: i64) -> Result<usize> {
    if off < 0 || len <= 0 {
        return Err(EINVAL);
    }
    let f = file(fd)?;
    if !f.writable() {
        return Err(EBADF);
    }
    f.inode().ok_or(ENODEV)?.fallocate(mode, off as u64, len as u64)?;
    Ok(0)
}

pub fn fsync(fd: i32, datasync: bool) -> Result<usize> {
    let f = file(fd)?;
    if let Some(i) = f.inode() {
        i.fsync(datasync)?;
    }
    Ok(0)
}

pub fn sync() -> Result<usize> {
    crate::fs::sync_all();
    Ok(0)
}

// ---- mounts

pub const MS_RDONLY: u64 = 1;
pub const MS_NOSUID: u64 = 2;
pub const MS_NODEV: u64 = 4;
pub const MS_NOEXEC: u64 = 8;
pub const MS_REMOUNT: u64 = 32;
pub const MS_NOATIME: u64 = 1024;
pub const MS_BIND: u64 = 4096;
pub const MS_MOVE: u64 = 8192;
pub const MS_REC: u64 = 16384;
pub const MS_SILENT: u64 = 32768;
pub const MS_PRIVATE: u64 = 1 << 18;
pub const MS_SLAVE: u64 = 1 << 19;
pub const MS_SHARED: u64 = 1 << 20;
pub const MS_UNBINDABLE: u64 = 1 << 17;
pub const MS_RELATIME: u64 = 1 << 21;
pub const MS_STRICTATIME: u64 = 1 << 24;

fn mnt_flags(f: u64) -> u32 {
    let mut m = 0;
    if f & MS_RDONLY != 0 {
        m |= MNT_RDONLY;
    }
    if f & MS_NOSUID != 0 {
        m |= path::MNT_NOSUID;
    }
    if f & MS_NODEV != 0 {
        m |= path::MNT_NODEV;
    }
    if f & MS_NOEXEC != 0 {
        m |= path::MNT_NOEXEC;
    }
    if f & MS_NOATIME != 0 {
        m |= path::MNT_NOATIME;
    }
    m
}

pub fn mount(src: usize, target: usize, fstype: usize, flags: u64, data: usize) -> Result<usize> {
    if !cred().capable(21) {
        return Err(EPERM);
    }
    let target = user_path(target)?;
    let src = if src != 0 { user_path(src)? } else { Vec::new() };
    let fstype = if fstype != 0 { user_path(fstype)? } else { Vec::new() };
    let data = if data != 0 { read_cstr(data, 4096).unwrap_or_default() } else { Vec::new() };
    let tp = lookup_at(AT_FDCWD, &target, true, false)?;
    if flags & (MS_PRIVATE | MS_SLAVE | MS_SHARED | MS_UNBINDABLE) != 0 && flags & (MS_BIND | MS_MOVE | MS_REMOUNT) == 0 {
        return Ok(0); // propagation types: every mount is private here
    }
    if flags & MS_REMOUNT != 0 {
        let m = &tp.mnt;
        if !Arc::ptr_eq(&tp.d, &m.root) {
            return Err(EINVAL);
        }
        m.flags.store(mnt_flags(flags), core::sync::atomic::Ordering::Relaxed);
        if flags & MS_RDONLY != 0 {
            m.root.fs.unmount()?;
        }
        return Ok(0);
    }
    if flags & MS_MOVE != 0 {
        let sp = lookup_at(AT_FDCWD, &src, true, false)?;
        if !Arc::ptr_eq(&sp.d, &sp.mnt.root) || sp.mnt.parent.lock().is_none() {
            return Err(EINVAL);
        }
        let m = sp.mnt.clone();
        m.detach();
        tp.mnt.attach(&tp.d, &m);
        return Ok(0);
    }
    if flags & MS_BIND != 0 {
        let sp = lookup_at(AT_FDCWD, &src, true, false)?;
        let m = Mount::new(sp.d.clone(), mnt_flags(flags), &sp.mnt.source, &sp.mnt.fstype, &sp.mnt.options);
        tp.mnt.attach(&tp.d, &m);
        if flags & MS_REC != 0 {
            bind_children(&sp, &m);
        }
        return Ok(0);
    }
    let fs = crate::fs::make_fs(&fstype, &src, &data, flags & MS_RDONLY != 0)?;
    let m = Mount::new(path::DEntry::new_root(fs), mnt_flags(flags), core::str::from_utf8(&src).unwrap_or(""), core::str::from_utf8(&fstype).unwrap_or(""), core::str::from_utf8(&data).unwrap_or(""));
    tp.mnt.attach(&tp.d, &m);
    Ok(0)
}

/// MS_BIND | MS_REC: the mounts inside the source tree appear inside the new bind mount too.
fn bind_children(src: &PathRef, newm: &Arc<Mount>) {
    let kids: Vec<Arc<Mount>> = src.mnt.children.lock().clone();
    for k in kids {
        let mp = k.parent.lock().as_ref().map(|(_, d)| d.clone());
        let Some(mp) = mp else { continue };
        // is the mount point under src.d?
        let mut d = Some(mp.clone());
        let mut inside = false;
        while let Some(x) = d {
            if Arc::ptr_eq(&x, &src.d) {
                inside = true;
                break;
            }
            d = x.parent();
        }
        if inside {
            let c = Mount::new(k.root.clone(), k.flags(), &k.source, &k.fstype, &k.options);
            newm.attach(&mp, &c);
            bind_children(&PathRef { mnt: k.clone(), d: k.root.clone() }, &c);
        }
    }
}

pub fn umount2(target: usize, flags: u32) -> Result<usize> {
    if !cred().capable(21) {
        return Err(EPERM);
    }
    let target = user_path(target)?;
    let p = lookup_at(AT_FDCWD, &target, flags & 8 == 0, false)?;
    if !Arc::ptr_eq(&p.d, &p.mnt.root) || p.mnt.parent.lock().is_none() {
        return Err(EINVAL);
    }
    if flags & 2 == 0 && !p.mnt.children.lock().is_empty() {
        return Err(EBUSY);
    }
    p.mnt.detach();
    // the file system is no longer mounted anywhere: flush it and mark it clean
    if !crate::fs::is_mounted(&p.d.fs) {
        p.d.fs.unmount()?;
    }
    Ok(0)
}
