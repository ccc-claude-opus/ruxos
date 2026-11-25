//! Per-CPU data. The architecture keeps a pointer to this CPU's `PerCpu` in a register
//! (TPIDR_EL1 / tp); the first two fields are used by the riscv64 trap entry.
use crate::sched::Task;
use crate::sync::SpinLock;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub const MAX_CPUS: usize = 8;

#[repr(C)]
pub struct PerCpu {
    /// Top of the current task's kernel stack (riscv64 trap entry from user mode).
    pub kstack_top: UnsafeCell<usize>,
    /// Scratch slot for the trap entry (riscv64: the user stack pointer).
    pub scratch: UnsafeCell<usize>,
    pub id: usize,
    /// Architecture hardware id (MPIDR / hart id).
    pub hwid: AtomicUsize,
    pub online: AtomicBool,
    /// Running the idle task with nothing to do.
    pub idle: AtomicBool,
    pub need_resched: AtomicBool,
    pub current: UnsafeCell<Option<Arc<Task>>>,
    pub idle_task: UnsafeCell<Option<Arc<Task>>>,
    /// The task this CPU just switched away from (released by the next task).
    pub prev: UnsafeCell<Option<Arc<Task>>>,
    pub rq: SpinLock<VecDeque<Arc<Task>>>,
    /// Sleeping tasks by (deadline in ticks, timer id).
    pub timers: SpinLock<BTreeMap<(u64, u64), Arc<Task>>>,
    /// Ticks at which the time slice of the running task ends (0 = none).
    pub slice_end: UnsafeCell<u64>,
    /// Id of the user address space loaded in this CPU (0 = none).
    pub loaded_mm: core::sync::atomic::AtomicU64,
    /// When the running task got the CPU (ticks), for CPU time accounting.
    pub run_start: core::sync::atomic::AtomicU64,
    /// Ticks at which throttled tasks (cpu.max) may run again (0 = none).
    pub throttle_wake: core::sync::atomic::AtomicU64,
}

unsafe impl Sync for PerCpu {}

#[allow(clippy::declare_interior_mutable_const)]
const INIT: PerCpu = PerCpu {
    kstack_top: UnsafeCell::new(0),
    scratch: UnsafeCell::new(0),
    id: 0,
    hwid: AtomicUsize::new(0),
    online: AtomicBool::new(false),
    idle: AtomicBool::new(false),
    need_resched: AtomicBool::new(false),
    current: UnsafeCell::new(None),
    idle_task: UnsafeCell::new(None),
    prev: UnsafeCell::new(None),
    rq: SpinLock::new(VecDeque::new()),
    timers: SpinLock::new(BTreeMap::new()),
    slice_end: UnsafeCell::new(0),
    loaded_mm: core::sync::atomic::AtomicU64::new(0),
    run_start: core::sync::atomic::AtomicU64::new(0),
    throttle_wake: core::sync::atomic::AtomicU64::new(0),
};

static mut CPUS: [PerCpu; MAX_CPUS] = [INIT; MAX_CPUS];
pub static NCPUS: AtomicUsize = AtomicUsize::new(1);

pub fn cpu(i: usize) -> &'static PerCpu {
    unsafe { &*(&raw const CPUS[i]) }
}

/// Called once per CPU before anything else uses per-CPU data.
pub fn init(i: usize, hwid: usize) {
    unsafe {
        let p = &raw mut CPUS[i];
        (*p).id = i;
        (*p).hwid.store(hwid, Ordering::Relaxed);
        crate::arch::cpu_init(p as usize);
    }
}

#[inline]
pub fn this() -> &'static PerCpu {
    unsafe { &*(crate::arch::percpu_ptr() as *const PerCpu) }
}

pub fn ncpus() -> usize {
    NCPUS.load(Ordering::Relaxed)
}
