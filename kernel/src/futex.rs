//! Fast user-space mutexes (futex(2)): wait/wake keyed by (address space, address) for private
//! futexes and by physical address for shared ones; bitsets, requeue, wake-op; robust lists.
use crate::arch;
use crate::errno::*;
use crate::mm::frame;
use crate::mm::vm::{self, Access};
use crate::sched::{self, Task};
use crate::sync::SpinLock;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub const FUTEX_WAIT: u32 = 0;
pub const FUTEX_WAKE: u32 = 1;
pub const FUTEX_REQUEUE: u32 = 3;
pub const FUTEX_CMP_REQUEUE: u32 = 4;
pub const FUTEX_WAKE_OP: u32 = 5;
pub const FUTEX_WAIT_BITSET: u32 = 9;
pub const FUTEX_WAKE_BITSET: u32 = 10;
pub const FUTEX_PRIVATE_FLAG: u32 = 128;
pub const FUTEX_CLOCK_REALTIME: u32 = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Key(u64, usize);

struct Waiter {
    key: SpinLock<Key>,
    task: Arc<Task>,
    bitset: u32,
    woken: AtomicBool,
}

const NB: usize = 256;
static BUCKETS: [SpinLock<Vec<Arc<Waiter>>>; NB] = [const { SpinLock::new(Vec::new()) }; NB];

fn bucket(k: Key) -> &'static SpinLock<Vec<Arc<Waiter>>> {
    let h = (k.1 >> 2).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (k.0 as usize).wrapping_mul(31);
    &BUCKETS[(h >> 7) % NB]
}

/// The key of a futex word, and its physical page (with a reference) for reading it.
fn key_of(uaddr: usize, private: bool) -> Result<(Key, usize)> {
    if uaddr & 3 != 0 {
        return Err(EINVAL);
    }
    let mm = vm::current_mm();
    let pa = mm.get_user_page(uaddr, Access::Read)?;
    let key = if private { Key(mm.id, uaddr) } else { Key(0, pa + (uaddr & 0xfff)) };
    Ok((key, pa))
}

fn word(pa: usize, uaddr: usize) -> &'static AtomicU32 {
    unsafe { &*((arch::phys_to_virt(pa) + (uaddr & 0xfff)) as *const AtomicU32) }
}

/// FUTEX_WAIT(_BITSET): sleeps while *uaddr == val. `deadline` in ticks.
pub fn wait(uaddr: usize, val: u32, deadline: Option<u64>, bitset: u32, private: bool) -> Result<usize> {
    if bitset == 0 {
        return Err(EINVAL);
    }
    let (key, pa) = key_of(uaddr, private)?;
    let me = sched::current_arc();
    me.wchan.store(0xf00, Ordering::Relaxed);
    let w = Arc::new(Waiter { key: SpinLock::new(key), task: me.clone(), bitset, woken: AtomicBool::new(false) });
    {
        let mut b = bucket(key).lock();
        let cur = word(pa, uaddr).load(Ordering::SeqCst);
        frame::put_page(pa);
        if cur != val {
            return Err(EAGAIN);
        }
        b.push(w.clone());
        sched::prepare_sleep();
    }
    let mut timer = None;
    let result = loop {
        if w.woken.load(Ordering::SeqCst) {
            sched::finish_sleep();
            break Ok(0);
        }
        if crate::task::signal_pending() {
            sched::finish_sleep();
            break Err(ERESTARTSYS);
        }
        if let Some(d) = deadline {
            if arch::ticks() >= d {
                sched::finish_sleep();
                break Err(ETIMEDOUT);
            }
            if timer.is_none() {
                timer = Some(crate::time::add_timer(d, me.clone()));
            }
        }
        sched::schedule();
        sched::prepare_sleep();
    };
    if let Some(t) = timer {
        crate::time::cancel_timer(t);
    }
    if result.is_err() {
        // leave the queue (the key may have changed by a requeue)
        loop {
            let k = *w.key.lock();
            let mut b = bucket(k).lock();
            if *w.key.lock() != k {
                continue;
            }
            b.retain(|x| !Arc::ptr_eq(x, &w));
            break;
        }
        if w.woken.load(Ordering::SeqCst) {
            return Ok(0);
        }
    }
    result
}

fn wake_key(key: Key, n: usize, bitset: u32) -> usize {
    let mut woken = Vec::new();
    {
        let mut b = bucket(key).lock();
        let mut i = 0;
        while i < b.len() && woken.len() < n {
            if *b[i].key.lock() == key && b[i].bitset & bitset != 0 {
                let w = b.remove(i);
                w.woken.store(true, Ordering::SeqCst);
                woken.push(w);
            } else {
                i += 1;
            }
        }
    }
    for w in &woken {
        sched::wake(&w.task);
    }
    woken.len()
}

pub fn wake(uaddr: usize, private: bool, n: usize, bitset: u32) -> Result<usize> {
    if bitset == 0 {
        return Err(EINVAL);
    }
    let (key, pa) = key_of(uaddr, private)?;
    frame::put_page(pa);
    Ok(wake_key(key, n, bitset))
}

/// FUTEX_(CMP_)REQUEUE: wakes `n` waiters of uaddr and moves up to `n2` others to uaddr2.
pub fn requeue(uaddr: usize, private: bool, n: usize, uaddr2: usize, n2: usize, cmp: Option<u32>) -> Result<usize> {
    let (k1, pa1) = key_of(uaddr, private)?;
    let (k2, pa2) = key_of(uaddr2, private)?;
    frame::put_page(pa2);
    if let Some(v) = cmp {
        let b = bucket(k1).lock();
        let cur = word(pa1, uaddr).load(Ordering::SeqCst);
        drop(b);
        if cur != v {
            frame::put_page(pa1);
            return Err(EAGAIN);
        }
    }
    frame::put_page(pa1);
    let woken = wake_key(k1, n, u32::MAX);
    // move the rest
    let mut moved = Vec::new();
    {
        let mut b = bucket(k1).lock();
        let mut i = 0;
        while i < b.len() && moved.len() < n2 {
            if *b[i].key.lock() == k1 {
                let w = b.remove(i);
                *w.key.lock() = k2;
                moved.push(w);
            } else {
                i += 1;
            }
        }
    }
    let count = moved.len();
    bucket(k2).lock().extend(moved);
    Ok(woken + count)
}

/// FUTEX_WAKE_OP: *uaddr2 = op(*uaddr2, oparg); wake n on uaddr; if cmp(old, cmparg) wake n2 on uaddr2.
pub fn wake_op(uaddr: usize, private: bool, n: usize, uaddr2: usize, n2: usize, op: u32) -> Result<usize> {
    let (k1, pa1) = key_of(uaddr, private)?;
    frame::put_page(pa1);
    let mm = vm::current_mm();
    let pa2 = mm.get_user_page(uaddr2, Access::Write)?;
    let k2 = if private { Key(mm.id, uaddr2) } else { Key(0, pa2 + (uaddr2 & 0xfff)) };
    let w = word(pa2, uaddr2);
    let shift = (op >> 28) & 8 != 0;
    let mut oparg = (op >> 12) & 0xfff;
    if oparg & 0x800 != 0 {
        oparg |= 0xffff_f000;
    }
    if shift {
        oparg = 1u32.wrapping_shl(oparg & 31);
    }
    let old = match (op >> 28) & 7 {
        0 => w.swap(oparg, Ordering::SeqCst),
        1 => w.fetch_add(oparg, Ordering::SeqCst),
        2 => w.fetch_or(oparg, Ordering::SeqCst),
        3 => w.fetch_and(!oparg, Ordering::SeqCst),
        4 => w.fetch_xor(oparg, Ordering::SeqCst),
        _ => {
            frame::put_page(pa2);
            return Err(ENOSYS);
        }
    };
    frame::put_page(pa2);
    let mut cmparg = op & 0xfff;
    if cmparg & 0x800 != 0 {
        cmparg |= 0xffff_f000;
    }
    let (o, c) = (old as i32, cmparg as i32);
    let cond = match (op >> 24) & 0xf {
        0 => o == c,
        1 => o != c,
        2 => o < c,
        3 => o <= c,
        4 => o > c,
        5 => o >= c,
        _ => return Err(ENOSYS),
    };
    let mut total = wake_key(k1, n, u32::MAX);
    if cond {
        total += wake_key(k2, n2, u32::MAX);
    }
    Ok(total)
}

const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;

/// At thread exit: marks robust futexes held by the thread as owner-died and wakes a waiter.
pub fn exit_robust_list(head: usize) {
    if head == 0 {
        return;
    }
    let tid = sched::current().tid;
    let Ok([mut entry, offset, pending]) = vm::read_user::<[usize; 3]>(head) else { return };
    let handle = |e: usize| {
        let uaddr = e.wrapping_add(offset);
        let mm = vm::current_mm();
        let Ok(pa) = mm.get_user_page(uaddr, Access::Write) else { return };
        let w = word(pa, uaddr);
        let mut v = w.load(Ordering::SeqCst);
        loop {
            if v & FUTEX_TID_MASK != tid {
                break;
            }
            let nv = (v & FUTEX_WAITERS) | FUTEX_OWNER_DIED;
            match w.compare_exchange(v, nv, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => {
                    if v & FUTEX_WAITERS != 0 {
                        frame::put_page(pa);
                        let _ = wake(uaddr, false, 1, u32::MAX);
                        let _ = wake(uaddr, true, 1, u32::MAX);
                        return;
                    }
                    break;
                }
                Err(x) => v = x,
            }
        }
        frame::put_page(pa);
    };
    let mut n = 0;
    while entry != head && entry != 0 && n < 2048 {
        let next: usize = vm::read_user(entry).unwrap_or(0);
        if entry != pending {
            handle(entry);
        }
        entry = next;
        n += 1;
    }
    if pending != 0 {
        handle(pending);
    }
}
