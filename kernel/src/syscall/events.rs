//! epoll, eventfd, signalfd, pidfd system calls.
use super::fs::file;
use crate::errno::*;
use crate::mm::vm::read_user;
use crate::task::current_thread;
use crate::vfs::events::*;
use crate::vfs::file::{File, O_CLOEXEC, O_NONBLOCK, O_RDWR};
use alloc::sync::Arc;
use core::sync::atomic::Ordering;

fn install(ops: Arc<dyn crate::vfs::FileOps>, flags: u32) -> Result<usize> {
    let f = File::new(ops, O_RDWR | (flags & O_NONBLOCK), None);
    Ok(current_thread().files.lock().alloc(f, 0, flags & O_CLOEXEC != 0)? as usize)
}

pub fn epoll_create1(flags: u32) -> Result<usize> {
    if flags & !O_CLOEXEC != 0 {
        return Err(EINVAL);
    }
    install(Arc::new(EpollFile { ep: Epoll::new() }), flags)
}

fn epoll_of(epfd: i32) -> Result<Arc<Epoll>> {
    let f = file(epfd)?;
    Ok(f.ops.as_any().downcast_ref::<EpollFile>().ok_or(EINVAL)?.ep.clone())
}

pub fn epoll_ctl(epfd: i32, op: u32, fd: i32, ev: usize) -> Result<usize> {
    let ep = epoll_of(epfd)?;
    let target = file(fd)?;
    if target.inode().and_then(|i| i.stat().ok()).is_some_and(|s| s.mode & crate::vfs::S_IFMT == crate::vfs::S_IFREG || s.mode & crate::vfs::S_IFMT == crate::vfs::S_IFDIR) && target.ops.poll_queue().is_none() {
        return Err(EPERM); // regular files and directories cannot be watched
    }
    let (events, data) = if op != 2 {
        let e: [u64; 2] = read_user(ev)?;
        (e[0] as u32, e[1])
    } else {
        (0, 0)
    };
    ep.ctl(op, fd, &target, events, data)?;
    Ok(0)
}

pub fn epoll_pwait(epfd: i32, events: usize, max: i32, timeout_ms: i32, sigmask: usize, size: usize) -> Result<usize> {
    if max <= 0 {
        return Err(EINVAL);
    }
    let deadline = if timeout_ms < 0 { None } else { Some(crate::arch::ticks() + crate::time::ns_to_ticks(timeout_ms as u64 * 1_000_000)) };
    wait(epfd, events, max, deadline, sigmask, size)
}

pub fn epoll_pwait2(epfd: i32, events: usize, max: i32, ts: usize, sigmask: usize, size: usize) -> Result<usize> {
    if max <= 0 {
        return Err(EINVAL);
    }
    let deadline = if ts == 0 { None } else { Some(crate::arch::ticks() + crate::time::ns_to_ticks(super::time::get_ts(ts)? as u64)) };
    wait(epfd, events, max, deadline, sigmask, size)
}

fn wait(epfd: i32, events: usize, max: i32, deadline: Option<u64>, sigmask: usize, size: usize) -> Result<usize> {
    let ep = epoll_of(epfd)?;
    if sigmask != 0 {
        if size != 8 {
            return Err(EINVAL);
        }
        let m: u64 = read_user(sigmask)?;
        let th = current_thread();
        let old = th.sigmask.swap((m & !crate::task::signal::UNBLOCKABLE) as usize, Ordering::Relaxed);
        crate::task::own().saved_mask = Some(old as u64);
    }
    let r = ep.wait(max as usize, deadline);
    match r {
        Ok(ev) => {
            write_events(events, &ev)?;
            if !crate::task::signal_pending() {
                if let Some(m) = crate::task::own().saved_mask.take() {
                    current_thread().sigmask.store(m as usize, Ordering::Relaxed);
                }
            }
            Ok(ev.len())
        }
        Err(_) => Err(EINTR),
    }
}

pub fn eventfd2(init: u32, flags: u32) -> Result<usize> {
    if flags & !(O_CLOEXEC | O_NONBLOCK | 1) != 0 {
        return Err(EINVAL);
    }
    install(EventFd::new(init as u64, flags & 1 != 0), flags)
}

pub fn signalfd4(fd: i32, mask: usize, size: usize, flags: u32) -> Result<usize> {
    if size != 8 || flags & !(O_CLOEXEC | O_NONBLOCK) != 0 {
        return Err(EINVAL);
    }
    let m: u64 = read_user::<u64>(mask)? & !crate::task::signal::UNBLOCKABLE;
    if fd != -1 {
        let f = file(fd)?;
        let s = f.ops.as_any().downcast_ref::<SignalFd>().ok_or(EINVAL)?;
        s.mask.store(m, Ordering::Relaxed);
        return Ok(fd as usize);
    }
    install(SignalFd::new(m), flags)
}

pub fn pidfd_open(pid: i32, flags: u32) -> Result<usize> {
    if pid <= 0 || flags & !O_NONBLOCK != 0 {
        return Err(EINVAL);
    }
    let p = crate::task::proc_by_vpid(pid as u32).ok_or(ESRCH)?;
    install(PidFd::new(p), flags | O_CLOEXEC)
}

pub fn pidfd_send_signal(fd: i32, sig: u32, _info: usize, flags: u32) -> Result<usize> {
    if flags != 0 {
        return Err(EINVAL);
    }
    let f = file(fd)?;
    let p = f.ops.as_any().downcast_ref::<PidFd>().ok_or(EBADF)?.proc.clone();
    if p.is_zombie() {
        return Err(ESRCH);
    }
    if sig != 0 {
        crate::task::signal::send_process(&p, crate::task::signal::SigInfo::user(sig, crate::task::signal::SI_USER));
    }
    Ok(0)
}
