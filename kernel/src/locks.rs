//! File locks: POSIX record locks (fcntl F_SETLK/F_SETLKW/F_GETLK, owned by the process), open
//! file description locks (F_OFD_*, owned by the open file), and flock(2).
use crate::errno::*;
use crate::mm::vm::{read_user, write_user};
use crate::sched::{WaitQueue, WaitResult};
use crate::sync::SpinLock;
use crate::vfs::File;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Owner {
    Process(u32),
    File(usize),
}

#[derive(Clone, Copy, Debug)]
struct Lock {
    owner: Owner,
    pid: u32,
    start: u64,
    end: u64, // inclusive
    write: bool,
}

#[derive(Default)]
struct InodeLocks {
    posix: Vec<Lock>,
    /// flock: (open file, exclusive)
    flock: Vec<(usize, bool)>,
}

static LOCKS: SpinLock<BTreeMap<usize, InodeLocks>> = SpinLock::new(BTreeMap::new());
static WQ: WaitQueue = WaitQueue::new();

fn key(f: &File) -> Result<usize> {
    let i = f.inode().ok_or(EBADF)?;
    Ok(Arc::as_ptr(&i) as *const u8 as usize)
}

fn conflicts(a: &Lock, owner: Owner, start: u64, end: u64, write: bool) -> bool {
    a.owner != owner && a.start <= end && start <= a.end && (a.write || write)
}

/// Sets [start, end] to `kind` for `owner` (F_UNLCK removes), splitting and merging ranges.
fn apply(list: &mut Vec<Lock>, owner: Owner, pid: u32, start: u64, end: u64, kind: Option<bool>) {
    let mut out = Vec::with_capacity(list.len() + 2);
    for l in list.drain(..) {
        if l.owner != owner || l.end < start || l.start > end {
            out.push(l);
            continue;
        }
        if l.start < start {
            out.push(Lock { end: start - 1, ..l });
        }
        if l.end > end {
            out.push(Lock { start: end + 1, ..l });
        }
    }
    if let Some(w) = kind {
        out.push(Lock { owner, pid, start, end, write: w });
    }
    *list = out;
}

pub fn fcntl_lock(f: &Arc<File>, cmd: u32, arg: usize) -> Result<usize> {
    let raw: [u8; 32] = read_user(arg)?;
    let l_type = i16::from_le_bytes([raw[0], raw[1]]);
    let whence = i16::from_le_bytes([raw[2], raw[3]]);
    let l_start = i64::from_le_bytes(raw[8..16].try_into().unwrap());
    let l_len = i64::from_le_bytes(raw[16..24].try_into().unwrap());
    let base: i64 = match whence {
        0 => 0,
        1 => *f.pos.lock() as i64,
        2 => f.inode().ok_or(EBADF)?.stat()?.size as i64,
        _ => return Err(EINVAL),
    };
    let mut s = base.checked_add(l_start).ok_or(EOVERFLOW)?;
    let mut e = if l_len == 0 { i64::MAX } else { s.checked_add(l_len).ok_or(EOVERFLOW)? - 1 };
    if l_len < 0 {
        e = s - 1;
        s += l_len;
    }
    if s < 0 {
        return Err(EINVAL);
    }
    let (start, end) = (s as u64, e as u64);
    let pid = crate::task::current_proc().pid;
    let ofd = matches!(cmd, 36..=38);
    let owner = if ofd { Owner::File(Arc::as_ptr(f) as usize) } else { Owner::Process(pid) };
    let k = key(f)?;
    match cmd {
        5 | 36 => {
            // F_GETLK: the first conflicting lock, or F_UNLCK
            let locks = LOCKS.lock();
            let found = locks.get(&k).and_then(|il| il.posix.iter().find(|l| conflicts(l, owner, start, end, l_type == F_WRLCK)).copied());
            drop(locks);
            let mut out = raw;
            match found {
                None => out[0..2].copy_from_slice(&F_UNLCK.to_le_bytes()),
                Some(l) => {
                    out[0..2].copy_from_slice(&(if l.write { F_WRLCK } else { F_RDLCK }).to_le_bytes());
                    out[2..4].copy_from_slice(&0i16.to_le_bytes());
                    out[8..16].copy_from_slice(&(l.start as i64).to_le_bytes());
                    let len = if l.end == i64::MAX as u64 { 0 } else { (l.end - l.start + 1) as i64 };
                    out[16..24].copy_from_slice(&len.to_le_bytes());
                    out[24..28].copy_from_slice(&(if ofd { -1 } else { l.pid as i32 }).to_le_bytes());
                }
            }
            write_user(arg, &out)?;
            Ok(0)
        }
        _ => {
            let wait = cmd == 7 || cmd == 38;
            let kind = match l_type {
                F_RDLCK => {
                    if !f.readable() {
                        return Err(EBADF);
                    }
                    Some(false)
                }
                F_WRLCK => {
                    if !f.writable() {
                        return Err(EBADF);
                    }
                    Some(true)
                }
                F_UNLCK => None,
                _ => return Err(EINVAL),
            };
            loop {
                {
                    let mut locks = LOCKS.lock();
                    let il = locks.entry(k).or_default();
                    let blocked = kind.is_some_and(|w| il.posix.iter().any(|l| conflicts(l, owner, start, end, w)));
                    if !blocked {
                        apply(&mut il.posix, owner, pid, start, end, kind);
                        let empty = il.posix.is_empty() && il.flock.is_empty();
                        if empty {
                            locks.remove(&k);
                        }
                        drop(locks);
                        if kind.is_none() || kind == Some(false) {
                            WQ.wake_all();
                        }
                        return Ok(0);
                    }
                }
                if !wait {
                    return Err(EAGAIN);
                }
                let free = || {
                    let locks = LOCKS.lock();
                    !locks.get(&k).is_some_and(|il| il.posix.iter().any(|l| conflicts(l, owner, start, end, kind.unwrap_or(false))))
                };
                if let WaitResult::Interrupted = WQ.wait_until(free, None, true) {
                    return Err(ERESTARTSYS);
                }
            }
        }
    }
}

pub fn flock(f: &Arc<File>, op: u32) -> Result<usize> {
    const LOCK_SH: u32 = 1;
    const LOCK_EX: u32 = 2;
    const LOCK_NB: u32 = 4;
    const LOCK_UN: u32 = 8;
    let k = key(f)?;
    let me = Arc::as_ptr(f) as usize;
    let op_kind = op & !LOCK_NB;
    loop {
        {
            let mut locks = LOCKS.lock();
            let il = locks.entry(k).or_default();
            match op_kind {
                LOCK_UN => {
                    il.flock.retain(|(o, _)| *o != me);
                    drop(locks);
                    WQ.wake_all();
                    return Ok(0);
                }
                LOCK_SH | LOCK_EX => {
                    let ex = op_kind == LOCK_EX;
                    let blocked = il.flock.iter().any(|(o, x)| *o != me && (*x || ex));
                    if !blocked {
                        il.flock.retain(|(o, _)| *o != me);
                        il.flock.push((me, ex));
                        return Ok(0);
                    }
                }
                _ => return Err(EINVAL),
            }
        }
        if op & LOCK_NB != 0 {
            return Err(EAGAIN);
        }
        let ex = op_kind == LOCK_EX;
        let free = || !LOCKS.lock().get(&k).is_some_and(|il| il.flock.iter().any(|(o, x)| *o != me && (*x || ex)));
        if let WaitResult::Interrupted = WQ.wait_until(free, None, true) {
            return Err(ERESTARTSYS);
        }
    }
}

/// close(): a process loses all its POSIX locks on the file (POSIX semantics).
pub fn on_close(f: &File, pid: u32) {
    let Ok(k) = key(f) else { return };
    let mut locks = LOCKS.lock();
    if let Some(il) = locks.get_mut(&k) {
        let before = il.posix.len();
        il.posix.retain(|l| l.owner != Owner::Process(pid));
        if il.posix.len() != before {
            drop(locks);
            WQ.wake_all();
        }
    }
}

/// The last reference to an open file is gone: its OFD and flock locks go.
pub fn on_file_drop(file_ptr: usize, inode_key: usize) {
    let mut locks = LOCKS.lock();
    if let Some(il) = locks.get_mut(&inode_key) {
        il.posix.retain(|l| l.owner != Owner::File(file_ptr));
        il.flock.retain(|(o, _)| *o != file_ptr);
        if il.posix.is_empty() && il.flock.is_empty() {
            locks.remove(&inode_key);
        }
        drop(locks);
        WQ.wake_all();
    }
}

/// Process exit: all of its POSIX locks go.
pub fn release_process(pid: u32) {
    let mut locks = LOCKS.lock();
    let mut any = false;
    for il in locks.values_mut() {
        let before = il.posix.len();
        il.posix.retain(|l| l.owner != Owner::Process(pid));
        any |= il.posix.len() != before;
    }
    drop(locks);
    if any {
        WQ.wake_all();
    }
}
