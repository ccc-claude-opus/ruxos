//! ppoll and pselect6: wait for readiness of several descriptors.
use crate::errno::*;
use crate::mm::vm::{copy_from_user, copy_to_user, read_user};
use crate::sched;
use crate::vfs::poll::*;
use crate::vfs::File;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

/// Sleeps until `check()` reports something, the deadline passes, or a signal arrives. The task
/// waits on the poll queues of all the files.
pub fn wait_files(files: &[Arc<File>], deadline: Option<u64>, mut check: impl FnMut() -> bool) -> Result<bool> {
    let me = sched::current_arc();
    let mut timer = None;
    let r = loop {
        for f in files {
            if let Some(q) = f.ops.poll_queue() {
                q.wq.add(&me);
            }
        }
        sched::prepare_sleep();
        if check() {
            sched::finish_sleep();
            break Ok(true);
        }
        if crate::task::signal_pending() {
            sched::finish_sleep();
            break Err(ERESTARTNOHAND);
        }
        if let Some(d) = deadline {
            if crate::arch::ticks() >= d {
                sched::finish_sleep();
                break Ok(false);
            }
            if timer.is_none() {
                timer = Some(crate::time::add_timer(d, me.clone()));
            }
        }
        sched::schedule();
    };
    if let Some(t) = timer {
        crate::time::cancel_timer(t);
    }
    for f in files {
        if let Some(q) = f.ops.poll_queue() {
            q.wq.remove(&me);
        }
    }
    r
}

fn deadline_of(ts: usize) -> Result<Option<u64>> {
    if ts == 0 {
        return Ok(None);
    }
    let ns = super::time::get_ts(ts)?;
    Ok(Some(crate::arch::ticks() + crate::time::ns_to_ticks(ns as u64)))
}

fn set_temp_mask(mask: usize, size: usize) -> Result<()> {
    if mask == 0 {
        return Ok(());
    }
    if size != 8 {
        return Err(EINVAL);
    }
    let m: u64 = read_user(mask)?;
    let th = crate::task::current_thread();
    let old = th.sigmask.swap((m & !crate::task::signal::UNBLOCKABLE) as usize, Ordering::Relaxed);
    crate::task::own().saved_mask = Some(old as u64);
    Ok(())
}

fn restore_mask_if_no_signal() {
    // the mask set for the wait stays until a signal is delivered; without one, restore it now
    if !crate::task::signal_pending() {
        if let Some(m) = crate::task::own().saved_mask.take() {
            crate::task::current_thread().sigmask.store(m as usize, Ordering::Relaxed);
        }
    }
}

pub fn ppoll(fds: usize, n: usize, ts: usize, mask: usize, size: usize) -> Result<usize> {
    if n > 65536 {
        return Err(EINVAL);
    }
    let mut raw = alloc::vec![0u8; n * 8];
    copy_from_user(&mut raw, fds)?;
    let entries: Vec<(i32, u16)> = (0..n)
        .map(|i| (i32::from_le_bytes(raw[8 * i..8 * i + 4].try_into().unwrap()), u16::from_le_bytes(raw[8 * i + 4..8 * i + 6].try_into().unwrap())))
        .collect();
    let deadline = deadline_of(ts)?;
    set_temp_mask(mask, size)?;
    let fdt = crate::task::current_thread().files.lock().clone();
    let files: Vec<Option<Arc<File>>> = entries.iter().map(|(fd, _)| if *fd < 0 { None } else { fdt.get(*fd).ok() }).collect();
    let live: Vec<Arc<File>> = files.iter().flatten().cloned().collect();
    let mut revents = alloc::vec![0u16; n];
    let mut count = 0;
    let scan = |revents: &mut [u16], count: &mut usize| -> bool {
        *count = 0;
        for (i, (fd, ev)) in entries.iter().enumerate() {
            revents[i] = 0;
            if *fd < 0 {
                continue;
            }
            let r = match &files[i] {
                None => POLLNVAL,
                Some(f) => f.ops.poll(f) & (*ev as u32 | POLLERR | POLLHUP | POLLNVAL),
            };
            revents[i] = r as u16;
            if r != 0 {
                *count += 1;
            }
        }
        *count > 0
    };
    let immediate = ts != 0 && deadline == Some(crate::arch::ticks()) || scan(&mut revents, &mut count);
    let r = if immediate { Ok(true) } else { wait_files(&live, deadline, || scan(&mut revents, &mut count)) };
    match r {
        Err(e) => Err(e),
        Ok(_) => {
            for (i, r) in revents.iter().enumerate() {
                raw[8 * i + 6..8 * i + 8].copy_from_slice(&r.to_le_bytes());
            }
            copy_to_user(fds, &raw)?;
            restore_mask_if_no_signal();
            Ok(count)
        }
    }
}

pub fn pselect6(nfds: usize, rd: usize, wr: usize, ex: usize, ts: usize, sigdata: usize) -> Result<usize> {
    if nfds > 65536 {
        return Err(EINVAL);
    }
    let words = nfds.div_ceil(64);
    let load = |p: usize| -> Result<Vec<u64>> {
        if p == 0 {
            return Ok(alloc::vec![0; words]);
        }
        let mut v = alloc::vec![0u64; words];
        let b = unsafe { core::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, words * 8) };
        copy_from_user(b, p)?;
        Ok(v)
    };
    let (r0, w0, e0) = (load(rd)?, load(wr)?, load(ex)?);
    let deadline = deadline_of(ts)?;
    if sigdata != 0 {
        let sd: [usize; 2] = read_user(sigdata)?;
        set_temp_mask(sd[0], sd[1])?;
    }
    let fdt = crate::task::current_thread().files.lock().clone();
    let mut files = Vec::new();
    for fd in 0..nfds {
        let bit = 1u64 << (fd % 64);
        if (r0[fd / 64] | w0[fd / 64] | e0[fd / 64]) & bit != 0 {
            files.push((fd, fdt.get(fd as i32).map_err(|_| EBADF)?));
        }
    }
    let live: Vec<Arc<File>> = files.iter().map(|(_, f)| f.clone()).collect();
    let (mut r1, mut w1, mut e1) = (alloc::vec![0u64; words], alloc::vec![0u64; words], alloc::vec![0u64; words]);
    let mut count = 0;
    let mut scan = || -> bool {
        r1.fill(0);
        w1.fill(0);
        e1.fill(0);
        count = 0;
        for (fd, f) in &files {
            let ev = f.ops.poll(f);
            let bit = 1u64 << (fd % 64);
            let i = fd / 64;
            if r0[i] & bit != 0 && ev & (POLLIN | POLLHUP | POLLERR) != 0 {
                r1[i] |= bit;
                count += 1;
            }
            if w0[i] & bit != 0 && ev & (POLLOUT | POLLERR) != 0 {
                w1[i] |= bit;
                count += 1;
            }
            if e0[i] & bit != 0 && ev & POLLPRI != 0 {
                e1[i] |= bit;
                count += 1;
            }
        }
        count > 0
    };
    let ready = scan() || wait_files(&live, deadline, &mut scan)?;
    let _ = ready;
    let store = |p: usize, v: &Vec<u64>| -> Result<()> {
        if p != 0 {
            let b = unsafe { core::slice::from_raw_parts(v.as_ptr() as *const u8, words * 8) };
            copy_to_user(p, b)?;
        }
        Ok(())
    };
    store(rd, &r1)?;
    store(wr, &w1)?;
    store(ex, &e1)?;
    restore_mask_if_no_signal();
    Ok(count)
}
