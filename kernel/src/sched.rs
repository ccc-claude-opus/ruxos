//! Tasks (threads) and the scheduler: a run queue per CPU, round robin with a time slice, idle
//! CPUs steal work. The kernel is not preemptive: tasks switch when they block, yield, or return to
//! user mode with `need_resched` set.
use crate::arch;
use crate::percpu::{self, PerCpu};
use crate::sync::SpinLock;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering};

pub const RUNNING: u8 = 0;
pub const RUNNABLE: u8 = 1;
pub const SLEEPING: u8 = 2;
pub const DEAD: u8 = 3;

pub const KSTACK_ORDER: usize = 3; // 32 KB
pub const KSTACK_SIZE: usize = 4096 << KSTACK_ORDER;
const STACK_CANARY: usize = 0x5275_786f_7353_746b;

static NEXT_TID: AtomicU32 = AtomicU32::new(1);

pub struct Task {
    pub tid: u32,
    ctx: UnsafeCell<arch::Context>,
    /// Kernel stack (virtual base address); 0 for the boot CPU's idle task.
    kstack: usize,
    pub state: AtomicU8,
    pub cpu: AtomicUsize,
    /// Running on a CPU, or its registers are still live there (until the switch away finishes).
    on_cpu: AtomicBool,
    pub is_idle: bool,
    /// What the task waits for when sleeping (debugging).
    pub wchan: core::sync::atomic::AtomicUsize,
    /// The last system call and how many the task has made (debugging).
    pub last_nr: core::sync::atomic::AtomicUsize,
    pub nr_count: core::sync::atomic::AtomicUsize,
    pub faults: core::sync::atomic::AtomicUsize,
    pub last_fault: core::sync::atomic::AtomicUsize,
    /// Thread state that only matters for user threads (process, signals, FP registers...).
    pub user: UnsafeCell<Option<alloc::boxed::Box<crate::task::UserThread>>>,
}

unsafe impl Sync for Task {}
unsafe impl Send for Task {}

impl Task {
    fn new(kstack: usize, is_idle: bool) -> Task {
        Task {
            tid: if is_idle { 0 } else { NEXT_TID.fetch_add(1, Ordering::Relaxed) },
            ctx: UnsafeCell::new(arch::Context::default()),
            kstack,
            state: AtomicU8::new(RUNNABLE),
            cpu: AtomicUsize::new(0),
            on_cpu: AtomicBool::new(false),
            is_idle,
            wchan: AtomicUsize::new(0),
            last_nr: AtomicUsize::new(0),
            nr_count: AtomicUsize::new(0),
            faults: AtomicUsize::new(0),
            last_fault: AtomicUsize::new(0),
            user: UnsafeCell::new(None),
        }
    }

    pub fn kstack_top(&self) -> usize {
        self.kstack + KSTACK_SIZE
    }

    /// # Safety: only the task itself (or the code creating it) may use this.
    pub unsafe fn user_mut(&self) -> Option<&mut crate::task::UserThread> {
        unsafe { (*self.user.get()).as_deref_mut() }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        if self.kstack != 0 {
            crate::mm::frame::free_pages(arch::virt_to_phys(self.kstack), KSTACK_ORDER);
        }
    }
}

fn alloc_kstack() -> usize {
    let pa = crate::mm::frame::alloc_pages(KSTACK_ORDER).expect("out of memory for a kernel stack");
    let va = arch::phys_to_virt(pa);
    unsafe { *(va as *mut usize) = STACK_CANARY };
    va
}

/// A task that will run `f(arg)` in kernel mode when first scheduled (not yet queued).
pub fn new_kernel_task(f: extern "C" fn(usize), arg: usize) -> Arc<Task> {
    let t = Task::new(alloc_kstack(), false);
    arch::init_context(unsafe { &mut *t.ctx.get() }, t.kstack_top(), f as usize, arg);
    Arc::new(t)
}

/// Same, but the context starts at `entry` with stack pointer `sp` (used to start user threads
/// whose trap frame was built at the top of the kernel stack).
pub fn new_task_with_stack(f: extern "C" fn(usize), arg: usize, sp: impl FnOnce(usize) -> usize) -> Arc<Task> {
    let t = Task::new(alloc_kstack(), false);
    let top = sp(t.kstack_top());
    arch::init_context(unsafe { &mut *t.ctx.get() }, top, f as usize, arg);
    Arc::new(t)
}

pub fn spawn_kernel(f: extern "C" fn(usize), arg: usize) -> Arc<Task> {
    let t = new_kernel_task(f, arg);
    enqueue(t.clone());
    t
}

/// The idle task of a CPU: for the boot CPU it is the boot thread itself (stack not owned).
pub fn init_cpu_idle(pc: &PerCpu, own_stack: bool) -> Arc<Task> {
    let t = Arc::new(Task::new(if own_stack { alloc_kstack() } else { 0 }, true));
    t.state.store(RUNNING, Ordering::Relaxed);
    t.cpu.store(pc.id, Ordering::Relaxed);
    t.on_cpu.store(true, Ordering::Relaxed);
    unsafe {
        *pc.idle_task.get() = Some(t.clone());
        *pc.current.get() = Some(t.clone());
    }
    t
}

#[inline]
pub fn current() -> &'static Task {
    unsafe { (*percpu::this().current.get()).as_deref().unwrap() }
}

pub fn current_arc() -> Arc<Task> {
    unsafe { (*percpu::this().current.get()).clone().unwrap() }
}

/// Puts a runnable task on a run queue: its last CPU if that one is idle, else an idle CPU, else
/// its last CPU. Kicks the chosen CPU if it is idle.
pub fn enqueue(t: Arc<Task>) {
    let n = percpu::ncpus();
    let last = t.cpu.load(Ordering::Relaxed);
    let mut target = last;
    if !percpu::cpu(last).idle.load(Ordering::Relaxed) {
        let me = percpu::this().id;
        for k in 0..n {
            let c = (me + k) % n;
            if percpu::cpu(c).online.load(Ordering::Relaxed) && percpu::cpu(c).idle.load(Ordering::Relaxed) {
                target = c;
                break;
            }
        }
    }
    let pc = percpu::cpu(target);
    pc.rq.lock().push_back(t);
    // pairs with the fence in idle_loop: either the idle CPU sees the task, or we see it idle
    core::sync::atomic::fence(Ordering::SeqCst);
    if target == percpu::this().id {
        pc.need_resched.store(true, Ordering::Relaxed);
    } else if pc.idle.load(Ordering::Relaxed) {
        arch::send_resched_ipi(target);
    }
}

/// Makes a sleeping task runnable. Returns whether it was sleeping.
pub fn wake(t: &Arc<Task>) -> bool {
    if t.state.compare_exchange(SLEEPING, RUNNABLE, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
        enqueue(t.clone());
        true
    } else {
        false
    }
}

fn steal(me: usize) -> Option<Arc<Task>> {
    let n = percpu::ncpus();
    for k in 1..n {
        let c = percpu::cpu((me + k) % n);
        if let Some(mut q) = c.rq.try_lock() {
            if q.len() > 0 {
                // take from the back: the task that has waited least (its cache is coldest there)
                return q.pop_back();
            }
        }
    }
    None
}

/// Gives up the CPU. The current task stays runnable if its state is RUNNING; with SLEEPING it
/// waits for `wake`; with DEAD it never returns.
pub fn schedule() {
    let irq = arch::irq_save();
    let pc = percpu::this();
    let cur = unsafe { (*pc.current.get()).clone().unwrap() };
    pc.need_resched.store(false, Ordering::Relaxed);
    let next = {
        let mut rq = pc.rq.lock();
        if !cur.is_idle && cur.state.load(Ordering::Relaxed) == RUNNING {
            cur.state.store(RUNNABLE, Ordering::Relaxed);
            rq.push_back(cur.clone());
        }
        rq.pop_front()
    };
    let next = next
        .or_else(|| steal(pc.id))
        .unwrap_or_else(|| unsafe { (*pc.idle_task.get()).clone().unwrap() });
    if Arc::ptr_eq(&next, &cur) {
        cur.state.store(RUNNING, Ordering::Relaxed);
        arch::irq_restore(irq);
        return;
    }
    // `next` may still be finishing its switch away from another CPU (it was woken while going
    // to sleep there). Waiting for it here can deadlock (that CPU may be waiting for our task), so
    // put it back and run the idle task this time; it is picked again once it has left that CPU.
    let next = if !next.is_idle && next.on_cpu.load(Ordering::Acquire) {
        pc.rq.lock().push_back(next);
        pc.need_resched.store(true, Ordering::Relaxed);
        if cur.is_idle {
            arch::irq_restore(irq);
            return;
        }
        unsafe { (*pc.idle_task.get()).clone().unwrap() }
    } else {
        next
    };
    next.on_cpu.store(true, Ordering::Relaxed);
    next.state.store(RUNNING, Ordering::Relaxed);
    next.cpu.store(pc.id, Ordering::Relaxed);
    check_stack(&cur);
    unsafe {
        *pc.kstack_top.get() = next.kstack_top();
        crate::task::switch_user_state(&cur, &next);
        let prev_ctx = cur.ctx.get();
        let next_ctx = next.ctx.get() as *const arch::Context;
        *pc.current.get() = Some(next);
        *pc.prev.get() = Some(cur);
        arch::switch(prev_ctx, next_ctx);
    }
    finish_switch();
    arch::irq_restore(irq);
}

/// Runs on the new task right after a switch: releases the previous task.
fn finish_switch() {
    let pc = percpu::this();
    let prev = unsafe { (*pc.prev.get()).take() };
    if let Some(p) = prev {
        p.on_cpu.store(false, Ordering::Release);
        drop(p);
    }
    crate::time::start_slice();
}

fn check_stack(t: &Task) {
    if t.kstack != 0 && unsafe { *(t.kstack as *const usize) } != STACK_CANARY {
        panic!("kernel stack overflow in task {}", t.tid);
    }
}

/// First code of every new task (from the architecture's trampoline).
#[unsafe(no_mangle)]
extern "C" fn task_start(f: extern "C" fn(usize), arg: usize) {
    finish_switch();
    arch::irq_enable();
    f(arg);
    exit_current();
}

/// Ends the current kernel task.
pub fn exit_current() -> ! {
    current().state.store(DEAD, Ordering::Relaxed);
    schedule();
    unreachable!()
}

pub fn yield_now() {
    schedule();
}

/// Called on return to user mode and in the idle loop.
pub fn resched_if_needed() {
    if percpu::this().need_resched.load(Ordering::Relaxed) {
        schedule();
    }
}

pub fn resched_ipi() {
    percpu::this().need_resched.store(true, Ordering::Relaxed);
}

/// The idle loop of every CPU.
pub fn idle_loop() -> ! {
    let pc = percpu::this();
    loop {
        arch::irq_disable();
        pc.idle.store(true, Ordering::SeqCst);
        core::sync::atomic::fence(Ordering::SeqCst);
        let has_work = !pc.rq.lock().is_empty() || pc.need_resched.load(Ordering::Relaxed);
        if has_work {
            pc.idle.store(false, Ordering::Relaxed);
            arch::irq_enable();
            schedule();
            continue;
        }
        // try to steal before sleeping
        if let Some(t) = steal(pc.id) {
            pc.idle.store(false, Ordering::Relaxed);
            pc.rq.lock().push_back(t);
            arch::irq_enable();
            continue;
        }
        crate::time::idle_timer();
        arch::wait_for_interrupt();
        arch::irq_enable();
    }
}

/// Marks the current task as going to sleep (before registering where a waker will find it).
pub fn prepare_sleep() {
    current().state.store(SLEEPING, Ordering::Relaxed);
}

/// Ends a sleep that may not have happened (the condition became true after `prepare_sleep`): if a
/// waker already queued us, let the scheduler consume that queue entry.
pub fn finish_sleep() {
    let t = current();
    if t.state.compare_exchange(SLEEPING, RUNNING, Ordering::AcqRel, Ordering::Relaxed).is_err()
        && t.state.load(Ordering::Acquire) == RUNNABLE
    {
        schedule();
    }
}

pub enum WaitResult {
    Ok,
    TimedOut,
    Interrupted,
}

/// A queue of tasks waiting for a condition.
pub struct WaitQueue {
    q: SpinLock<VecDeque<Arc<Task>>>,
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl WaitQueue {
    pub const fn new() -> Self {
        WaitQueue { q: SpinLock::new(VecDeque::new()) }
    }

    /// Sleeps until `cond()` is true (checked with the queue locked, so a `wake` after the
    /// condition changed is never lost), the deadline (ticks) passes, or, if `interruptible`, a
    /// signal is pending.
    pub fn wait_until(&self, mut cond: impl FnMut() -> bool, deadline: Option<u64>, interruptible: bool) -> WaitResult {
        let me = current_arc();
        me.wchan.store(self as *const _ as usize | interruptible as usize, Ordering::Relaxed);
        let mut timer = None;
        let r = loop {
            {
                let mut q = self.q.lock();
                me.state.store(SLEEPING, Ordering::Relaxed);
                if !q.iter().any(|t| Arc::ptr_eq(t, &me)) {
                    q.push_back(me.clone());
                }
            }
            let r = if cond() {
                Some(WaitResult::Ok)
            } else if interruptible && crate::task::signal_pending() {
                Some(WaitResult::Interrupted)
            } else if deadline.is_some_and(|d| arch::ticks() >= d) {
                Some(WaitResult::TimedOut)
            } else {
                None
            };
            if let Some(r) = r {
                finish_sleep();
                break r;
            }
            if let Some(d) = deadline {
                if timer.is_none() {
                    timer = Some(crate::time::add_timer(d, me.clone()));
                }
            }
            schedule();
        };
        if let Some(t) = timer {
            crate::time::cancel_timer(t);
        }
        self.q.lock().retain(|t| !Arc::ptr_eq(t, &me));
        r
    }

    pub fn wait(&self, cond: impl FnMut() -> bool) {
        self.wait_until(cond, None, false);
    }

    pub fn wake_one(&self) {
        let t = self.q.lock().pop_front();
        if let Some(t) = t {
            wake(&t);
        }
    }

    pub fn wake_all(&self) {
        let all: VecDeque<_> = core::mem::take(&mut *self.q.lock());
        for t in all {
            wake(&t);
        }
    }

    pub fn wake_n(&self, n: usize) -> usize {
        let mut woken = 0;
        while woken < n {
            let t = self.q.lock().pop_front();
            match t {
                Some(t) => {
                    wake(&t);
                    woken += 1;
                }
                None => break,
            }
        }
        woken
    }

    pub fn is_empty(&self) -> bool {
        self.q.lock().is_empty()
    }

    /// Registers `t` (multi-queue waits: poll, epoll).
    pub fn add(&self, t: &Arc<Task>) {
        let mut q = self.q.lock();
        if !q.iter().any(|x| Arc::ptr_eq(x, t)) {
            q.push_back(t.clone());
        }
    }

    pub fn remove(&self, t: &Arc<Task>) {
        self.q.lock().retain(|x| !Arc::ptr_eq(x, t));
    }
}

/// A sleeping lock for long critical sections (may block while held).
pub struct Mutex<T: ?Sized> {
    locked: SpinLock<bool>,
    wq: WaitQueue,
    data: UnsafeCell<T>,
}
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}

pub struct MutexGuard<'a, T: ?Sized> {
    m: &'a Mutex<T>,
}

impl<T> Mutex<T> {
    pub const fn new(v: T) -> Self {
        Mutex { locked: SpinLock::new(false), wq: WaitQueue::new(), data: UnsafeCell::new(v) }
    }
}

impl<T: ?Sized> Mutex<T> {
    pub fn lock(&self) -> MutexGuard<'_, T> {
        loop {
            {
                let mut l = self.locked.lock();
                if !*l {
                    *l = true;
                    return MutexGuard { m: self };
                }
            }
            self.wq.wait(|| !*unsafe { self.locked.get_unchecked() });
        }
    }
}

impl<T: ?Sized> core::ops::Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.m.data.get() }
    }
}
impl<T: ?Sized> core::ops::DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.m.data.get() }
    }
}
impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        *self.m.locked.lock() = false;
        self.m.wq.wake_one();
    }
}

/// Prints every task (Ctrl-T on the console with ruxos.debug).
pub fn dump_tasks() {
    {
        let n = crate::percpu::ncpus();
        crate::arch::DUMP_PC.store((1 << n) - 1, Ordering::Relaxed);
        for c in 0..n {
            if c != crate::percpu::this().id {
                arch::send_resched_ipi(c);
            }
        }
    }
    let tasks: alloc::vec::Vec<_> = crate::task::TASKS.lock().values().filter_map(|w| w.upgrade()).collect();
    for t in tasks {
        let st = ["running", "runnable", "sleeping", "dead"][t.state.load(Ordering::Relaxed) as usize];
        let comm = crate::task::thread_of(&t).map(|th| *th.comm.lock()).unwrap_or_default();
        let n = comm.iter().position(|&b| b == 0).unwrap_or(16);
        let pc = crate::task::thread_of(&t).map(|th| unsafe { (*th.own.get()).frame }).filter(|f| !f.is_null()).map(|f| crate::arch::frame_pc(unsafe { &*f })).unwrap_or(0);
        println!("task {} ({}) {} cpu {} wchan {:#x} user pc {:#x} last syscall {} of {}, {} faults, last at {:#x}", t.tid, core::str::from_utf8(&comm[..n]).unwrap_or("?"), st, t.cpu.load(Ordering::Relaxed), t.wchan.load(Ordering::Relaxed), pc, t.last_nr.load(Ordering::Relaxed), t.nr_count.load(Ordering::Relaxed), t.faults.load(Ordering::Relaxed), t.last_fault.load(Ordering::Relaxed));
    }
}
