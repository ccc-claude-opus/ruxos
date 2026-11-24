//! In-kernel tests, run with `ruxos.test=<name>` on the kernel command line; each prints
//! `ruxos-test <name>: ok` or panics.
use crate::sched::{self, WaitQueue};
use crate::{arch, percpu, time};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

pub fn run(name: &str) {
    match name {
        "sched" => sched_test(),
        _ => println!("ruxos-test {name}: unknown test"),
    }
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);
static DONE: AtomicUsize = AtomicUsize::new(0);
static TURN: AtomicUsize = AtomicUsize::new(0);
static WQ: WaitQueue = WaitQueue::new();
static CPUS_SEEN: AtomicUsize = AtomicUsize::new(0);
const THREADS: usize = 16;
const ROUNDS: usize = 2000;

extern "C" fn worker(i: usize) {
    for _ in 0..ROUNDS {
        // heap traffic from every CPU
        let v: Vec<usize> = (0..(i * 7 + 13)).collect();
        COUNTER.fetch_add(v.len() - (i * 7 + 13) + 1, Ordering::Relaxed);
        CPUS_SEEN.fetch_or(1 << percpu::this().id, Ordering::Relaxed);
        if i % 4 == 0 {
            sched::yield_now();
        }
    }
    // ping-pong: thread i waits for its turn
    for round in 0..50 {
        WQ.wait(|| TURN.load(Ordering::Acquire) % THREADS == i);
        let _ = round;
        TURN.fetch_add(1, Ordering::Release);
        WQ.wake_all();
    }
    DONE.fetch_add(1, Ordering::Release);
    WQ.wake_all();
}

fn sched_test() {
    // sleeping with a timer
    let t0 = time::monotonic_ns();
    let me = sched::current_arc();
    let wq = WaitQueue::new();
    let d = arch::ticks() + time::ns_to_ticks(20_000_000);
    wq.wait_until(|| false, Some(d), false);
    let slept = time::monotonic_ns() - t0;
    assert!(slept >= 20_000_000 && slept < 200_000_000, "slept {slept} ns");
    drop(me);
    let mut over = 0;
    for _ in 0..20 {
        let t = time::monotonic_ns();
        wq.wait_until(|| false, Some(arch::ticks() + time::ns_to_ticks(1_000_000)), false);
        over += time::monotonic_ns() - t - 1_000_000;
    }
    println!("ruxos-test sched: 1 ms sleeps overshoot by {} us on average", over / 20 / 1000);
    for i in 0..THREADS {
        sched::spawn_kernel(worker, i);
    }
    WQ.wait(|| DONE.load(Ordering::Acquire) == THREADS);
    assert_eq!(COUNTER.load(Ordering::Relaxed), THREADS * ROUNDS);
    assert_eq!(TURN.load(Ordering::Relaxed), THREADS * 50);
    let seen = CPUS_SEEN.load(Ordering::Relaxed).count_ones() as usize;
    println!(
        "ruxos-test sched: ok ({} threads on {} of {} cpus, slept {} us, {} ms total)",
        THREADS,
        seen,
        percpu::ncpus(),
        slept / 1000,
        (time::monotonic_ns() - t0) / 1_000_000
    );
}
