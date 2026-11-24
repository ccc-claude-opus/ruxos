//! Time system calls (the vDSO answers clock_gettime/gettimeofday without entering the kernel).
use crate::arch;
use crate::errno::*;
use crate::mm::vm::{read_user, write_user};
use crate::sched::{WaitQueue, WaitResult};
use crate::time;

pub const CLOCK_REALTIME: u32 = 0;
pub const CLOCK_MONOTONIC: u32 = 1;
pub const CLOCK_PROCESS_CPUTIME_ID: u32 = 2;
pub const CLOCK_THREAD_CPUTIME_ID: u32 = 3;
pub const CLOCK_MONOTONIC_RAW: u32 = 4;
pub const CLOCK_REALTIME_COARSE: u32 = 5;
pub const CLOCK_MONOTONIC_COARSE: u32 = 6;
pub const CLOCK_BOOTTIME: u32 = 7;
pub const CLOCK_REALTIME_ALARM: u32 = 8;
pub const CLOCK_BOOTTIME_ALARM: u32 = 9;

pub fn clock_ns(clk: u32) -> Result<i64> {
    match clk {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE | CLOCK_REALTIME_ALARM | 11 => Ok(time::realtime_ns()),
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME | CLOCK_BOOTTIME_ALARM => Ok(time::monotonic_ns() as i64),
        // CPU time: approximated by the time since the process (thread) started
        CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => {
            let p = crate::task::current_proc();
            Ok(time::ticks_to_ns(arch::ticks() - p.start_ticks) as i64)
        }
        _ => Err(EINVAL),
    }
}

fn put_ts(p: usize, ns: i64) -> Result<()> {
    write_user(p, &[ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000)])
}

pub fn get_ts(p: usize) -> Result<i64> {
    let t: [i64; 2] = read_user(p)?;
    if t[0] < 0 || !(0..1_000_000_000).contains(&t[1]) {
        return Err(EINVAL);
    }
    Ok(t[0].saturating_mul(1_000_000_000).saturating_add(t[1]))
}

pub fn clock_gettime(clk: u32, ts: usize) -> Result<usize> {
    put_ts(ts, clock_ns(clk)?)?;
    Ok(0)
}

pub fn clock_getres(clk: u32, ts: usize) -> Result<usize> {
    clock_ns(clk)?;
    if ts != 0 {
        put_ts(ts, 1)?;
    }
    Ok(0)
}

pub fn clock_settime(clk: u32, ts: usize) -> Result<usize> {
    if clk != CLOCK_REALTIME {
        return Err(EINVAL);
    }
    if !crate::task::current_proc().cred().capable(25) {
        return Err(EPERM);
    }
    time::set_realtime_ns(get_ts(ts)?);
    Ok(0)
}

pub fn gettimeofday(tv: usize, tz: usize) -> Result<usize> {
    if tv != 0 {
        let ns = time::realtime_ns();
        write_user(tv, &[ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000) / 1000])?;
    }
    if tz != 0 {
        write_user(tz, &[0i32, 0i32])?;
    }
    Ok(0)
}

pub fn settimeofday(tv: usize, _tz: usize) -> Result<usize> {
    if tv == 0 {
        return Ok(0);
    }
    if !crate::task::current_proc().cred().capable(25) {
        return Err(EPERM);
    }
    let t: [i64; 2] = read_user(tv)?;
    time::set_realtime_ns(t[0] * 1_000_000_000 + t[1] * 1000);
    Ok(0)
}

/// Sleeps until the deadline (ticks); Err(ERESTART_RESTARTBLOCK) with the time left when a
/// signal interrupts.
fn sleep_until(deadline: u64, rem: usize) -> Result<usize> {
    let wq = WaitQueue::new();
    match wq.wait_until(|| false, Some(deadline), true) {
        WaitResult::Interrupted => {
            if rem != 0 {
                let left = deadline.saturating_sub(arch::ticks());
                put_ts(rem, time::ticks_to_ns(left) as i64)?;
            }
            Err(ERESTART_RESTARTBLOCK)
        }
        _ => Ok(0),
    }
}

pub fn nanosleep(req: usize, rem: usize) -> Result<usize> {
    let ns = get_ts(req)?;
    sleep_until(arch::ticks() + time::ns_to_ticks(ns as u64), rem)
}

pub fn clock_nanosleep(clk: u32, flags: u32, req: usize, rem: usize) -> Result<usize> {
    let ns = get_ts(req)?;
    let now = clock_ns(clk)?;
    let rel = if flags & 1 != 0 { (ns - now).max(0) } else { ns };
    sleep_until(arch::ticks() + time::ns_to_ticks(rel as u64), if flags & 1 != 0 { 0 } else { rem })
}

pub fn times(buf: usize) -> Result<usize> {
    let p = crate::task::current_proc();
    if buf != 0 {
        let t = |v: usize| (v / 10_000) as u64; // microseconds -> clock ticks (100 Hz)
        let v = [
            t(p.utime.load(core::sync::atomic::Ordering::Relaxed)),
            t(p.stime.load(core::sync::atomic::Ordering::Relaxed)),
            t(p.cutime.load(core::sync::atomic::Ordering::Relaxed)),
            t(p.cstime.load(core::sync::atomic::Ordering::Relaxed)),
        ];
        write_user(buf, &v)?;
    }
    Ok((time::monotonic_ns() / 10_000_000) as usize)
}

pub fn getitimer(_which: u32, val: usize) -> Result<usize> {
    write_user(val, &[0i64; 4])?;
    Ok(0)
}

pub fn setitimer(which: u32, new: usize, old: usize) -> Result<usize> {
    if which != 0 {
        return Err(EINVAL);
    }
    if old != 0 {
        write_user(old, &[0i64; 4])?;
    }
    let v: [i64; 4] = read_user(new)?;
    // struct itimerval { it_interval, it_value }
    crate::task::itimer::set_real(v[2] * 1_000_000 + v[3], v[0] * 1_000_000 + v[1])?;
    Ok(0)
}
