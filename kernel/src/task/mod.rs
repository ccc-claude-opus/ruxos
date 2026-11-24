//! Processes and user threads: credentials, clone (fork, vfork, threads), exit, wait, and the
//! per-thread state that is switched with the thread (FP registers, TLS, address space).
pub mod exec;
pub mod itimer;
pub mod signal;

use crate::arch::{self, FpState, TrapFrame};
use crate::errno::*;
use crate::mm::vm::Mm;
use crate::sched::{self, Task, WaitQueue};
use crate::sync::SpinLock;
use crate::vfs::file::SharedFds;
use crate::vfs::{FdTable, PathRef};
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};
use signal::{SigHandlers, SigInfo, SigPending};

#[derive(Clone, Debug)]
pub struct Cred {
    pub uid: u32,
    pub euid: u32,
    pub suid: u32,
    pub fsuid: u32,
    pub gid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub fsgid: u32,
    pub groups: Vec<u32>,
    /// Capability sets (effective, permitted, inheritable, bounding).
    pub cap_eff: u64,
    pub cap_perm: u64,
    pub cap_inh: u64,
    pub cap_bnd: u64,
    pub no_new_privs: bool,
}

pub const CAP_ALL: u64 = (1 << 41) - 1;

impl Cred {
    pub fn root() -> Cred {
        Cred {
            uid: 0,
            euid: 0,
            suid: 0,
            fsuid: 0,
            gid: 0,
            egid: 0,
            sgid: 0,
            fsgid: 0,
            groups: Vec::new(),
            cap_eff: CAP_ALL,
            cap_perm: CAP_ALL,
            cap_inh: 0,
            cap_bnd: CAP_ALL,
            no_new_privs: false,
        }
    }
    pub fn capable(&self, cap: u32) -> bool {
        self.cap_eff & (1 << cap) != 0
    }
}

pub struct FsContext {
    pub root: PathRef,
    pub cwd: PathRef,
    pub umask: u32,
}

pub const RLIMIT_CPU: usize = 0;
pub const RLIMIT_FSIZE: usize = 1;
pub const RLIMIT_DATA: usize = 2;
pub const RLIMIT_STACK: usize = 3;
pub const RLIMIT_CORE: usize = 4;
pub const RLIMIT_NOFILE: usize = 7;
pub const RLIMIT_AS: usize = 9;
pub const RLIM_INFINITY: u64 = u64::MAX;

pub struct ProcState {
    /// wait status once a zombie
    pub exit_status: Option<i32>,
    pub reaped: bool,
    /// stopped by a signal (status for WUNTRACED, reported once)
    pub stop_report: Option<i32>,
    pub cont_report: bool,
}

pub struct Process {
    pub pid: u32,
    pub parent: SpinLock<Weak<Process>>,
    pub children: SpinLock<Vec<Arc<Process>>>,
    pub threads: SpinLock<Vec<Weak<Task>>>,
    pub pgid: AtomicU32,
    pub sid: AtomicU32,
    pub state: SpinLock<ProcState>,
    /// The parent waits here for children to change state.
    pub child_wq: WaitQueue,
    pub cred: SpinLock<Cred>,
    pub rlimits: SpinLock<[(u64, u64); 16]>,
    pub pending: SpinLock<SigPending>,
    pub exit_signal: u32,
    pub group_exit: AtomicBool,
    pub group_exit_code: AtomicI32,
    pub live_threads: AtomicUsize,
    pub stopped: AtomicBool,
    pub stop_wq: WaitQueue,
    pub start_ticks: u64,
    pub utime: AtomicUsize,
    pub stime: AtomicUsize,
    pub cutime: AtomicUsize,
    pub cstime: AtomicUsize,
    pub pdeath_signal: AtomicU32,
    pub dumpable: AtomicU32,
    pub ctty: SpinLock<Option<Arc<crate::tty::Tty>>>,
    pub pidfd_wq: WaitQueue,
    pub itimer_gen: core::sync::atomic::AtomicU64,
}

impl Process {
    pub fn is_zombie(&self) -> bool {
        self.state.lock().exit_status.is_some()
    }
    pub fn cred(&self) -> Cred {
        self.cred.lock().clone()
    }
    pub fn parent(&self) -> Option<Arc<Process>> {
        self.parent.lock().upgrade()
    }
    pub fn leader(&self) -> Option<Arc<Task>> {
        self.threads.lock().iter().filter_map(|w| w.upgrade()).find(|t| t.tid == self.pid)
    }
    pub fn rlimit(&self, r: usize) -> (u64, u64) {
        self.rlimits.lock()[r]
    }
}

/// Per-thread state of a user thread. Fields in `UnsafeCell` are touched only by the thread itself.
pub struct UserThread {
    pub proc: Arc<Process>,
    pub mm: Arc<Mm>,
    pub files: SharedFds,
    pub fs: Arc<SpinLock<FsContext>>,
    pub sighand: Arc<SpinLock<SigHandlers>>,
    pub pending: SpinLock<SigPending>,
    pub sigmask: AtomicUsize,
    pub own: UnsafeCell<Own>,
    pub comm: SpinLock<[u8; 16]>,
    /// Set by exit/exec so the parent of a vfork child can continue.
    pub vfork_done: SpinLock<Option<Arc<Completion>>>,
    /// This thread alone must exit (another thread is executing a new program).
    pub exit_now: AtomicBool,
}

unsafe impl Sync for UserThread {}
unsafe impl Send for UserThread {}

pub struct Own {
    pub fp: FpState,
    pub tls: usize,
    pub clear_child_tid: usize,
    pub robust_list: usize,
    pub altstack: (usize, usize, u32),
    /// The trap frame of the current system call or exception (on the kernel stack).
    pub frame: *mut TrapFrame,
    pub orig_arg0: usize,
    pub in_syscall: bool,
    /// The mask to restore after a ppoll/pselect/sigsuspend wait, once a signal is delivered.
    pub saved_mask: Option<u64>,
}

pub struct Completion {
    pub done: AtomicBool,
    pub wq: WaitQueue,
}

impl Completion {
    pub fn new() -> Arc<Completion> {
        Arc::new(Completion { done: AtomicBool::new(false), wq: WaitQueue::new() })
    }
    pub fn complete(&self) {
        self.done.store(true, Ordering::Release);
        self.wq.wake_all();
    }
    pub fn wait(&self) {
        self.wq.wait(|| self.done.load(Ordering::Acquire));
    }
}

/// All user threads by tid, and processes by pid.
pub static TASKS: SpinLock<BTreeMap<u32, Weak<Task>>> = SpinLock::new(BTreeMap::new());
pub static PROCS: SpinLock<BTreeMap<u32, Weak<Process>>> = SpinLock::new(BTreeMap::new());
pub static INIT: crate::sync::Once<Arc<Process>> = crate::sync::Once::new();

pub fn current_thread() -> &'static UserThread {
    unsafe { sched::current().user_mut().expect("not a user thread") }
}

pub fn try_current_thread() -> Option<&'static UserThread> {
    unsafe { sched::current().user_mut().map(|u| &*u) }
}

pub fn own() -> &'static mut Own {
    unsafe { &mut *current_thread().own.get() }
}

pub fn current_proc() -> &'static Arc<Process> {
    &current_thread().proc
}

pub fn task_by_tid(tid: u32) -> Option<Arc<Task>> {
    TASKS.lock().get(&tid).and_then(|w| w.upgrade())
}

pub fn proc_by_pid(pid: u32) -> Option<Arc<Process>> {
    PROCS.lock().get(&pid).and_then(|w| w.upgrade())
}

pub fn thread_of(t: &Task) -> Option<&UserThread> {
    unsafe { (*t.user.get()).as_deref() }
}

/// Called on every context switch.
pub fn switch_user_state(prev: &Task, next: &Task) {
    if let Some(p) = thread_of(prev) {
        let o = unsafe { &mut *p.own.get() };
        arch::save_fp(&mut o.fp);
        o.tls = arch::read_tls();
    }
    if let Some(n) = thread_of(next) {
        let o = unsafe { &*n.own.get() };
        arch::restore_fp(&o.fp);
        arch::write_tls(o.tls);
        let cpu = crate::percpu::this().id;
        let loaded = crate::percpu::this().loaded_mm.load(Ordering::Relaxed);
        if loaded != n.mm.id {
            arch::activate_mm(n.mm.root, n.mm.asid);
            crate::percpu::this().loaded_mm.store(n.mm.id, Ordering::Relaxed);
        }
        n.mm.cpus.fetch_or(1 << cpu, Ordering::Relaxed);
    }
}

pub fn signal_pending() -> bool {
    match try_current_thread() {
        Some(t) => signal::has_deliverable(t),
        None => false,
    }
}

pub fn send_signal_current(sig: u32) {
    let t = sched::current_arc();
    signal::send_thread(&t, SigInfo::kernel(sig));
}

fn default_rlimits() -> [(u64, u64); 16] {
    let mut r = [(RLIM_INFINITY, RLIM_INFINITY); 16];
    r[RLIMIT_STACK] = (8 << 20, RLIM_INFINITY);
    r[RLIMIT_CORE] = (0, RLIM_INFINITY);
    r[RLIMIT_NOFILE] = (1024, 524288);
    r[8] = (8 << 20, 8 << 20); // MEMLOCK
    r[6] = (63_000, 63_000); // NPROC
    r[11] = (63_000, 63_000); // SIGPENDING
    r[12] = (819_200, 819_200); // MSGQUEUE
    r[14] = (0, 0); // RTPRIO
    r
}

/// The first user process (pid = the init kernel thread's tid), run by the current kernel thread.
pub fn make_init(root: PathRef) -> Arc<Process> {
    let me = sched::current_arc();
    let proc = Arc::new(Process {
        pid: me.tid,
        parent: SpinLock::new(Weak::new()),
        children: SpinLock::new(Vec::new()),
        threads: SpinLock::new(alloc::vec![Arc::downgrade(&me)]),
        pgid: AtomicU32::new(me.tid),
        sid: AtomicU32::new(me.tid),
        state: SpinLock::new(ProcState { exit_status: None, reaped: false, stop_report: None, cont_report: false }),
        child_wq: WaitQueue::new(),
        cred: SpinLock::new(Cred::root()),
        rlimits: SpinLock::new(default_rlimits()),
        pending: SpinLock::new(SigPending::default()),
        exit_signal: signal::SIGCHLD,
        group_exit: AtomicBool::new(false),
        group_exit_code: AtomicI32::new(0),
        live_threads: AtomicUsize::new(1),
        stopped: AtomicBool::new(false),
        stop_wq: WaitQueue::new(),
        start_ticks: arch::ticks(),
        utime: AtomicUsize::new(0),
        stime: AtomicUsize::new(0),
        cutime: AtomicUsize::new(0),
        cstime: AtomicUsize::new(0),
        pdeath_signal: AtomicU32::new(0),
        dumpable: AtomicU32::new(1),
        ctty: SpinLock::new(None),
        pidfd_wq: WaitQueue::new(),
        itimer_gen: core::sync::atomic::AtomicU64::new(0),
    });
    let th = UserThread {
        proc: proc.clone(),
        mm: Mm::new(),
        files: Arc::new(SpinLock::new(FdTable::new())),
        fs: Arc::new(SpinLock::new(FsContext { root: root.clone(), cwd: root, umask: 0o022 })),
        sighand: Arc::new(SpinLock::new(SigHandlers::default())),
        pending: SpinLock::new(SigPending::default()),
        sigmask: AtomicUsize::new(0),
        own: UnsafeCell::new(Own {
            fp: FpState::default(),
            tls: 0,
            clear_child_tid: 0,
            robust_list: 0,
            altstack: (0, 0, signal::SS_DISABLE),
            frame: core::ptr::null_mut(),
            orig_arg0: 0,
            in_syscall: false,
            saved_mask: None,
        }),
        comm: SpinLock::new(*b"init\0\0\0\0\0\0\0\0\0\0\0\0"),
        vfork_done: SpinLock::new(None),
        exit_now: AtomicBool::new(false),
    };
    unsafe { *me.user.get() = Some(alloc::boxed::Box::new(th)) };
    TASKS.lock().insert(me.tid, Arc::downgrade(&me));
    PROCS.lock().insert(proc.pid, Arc::downgrade(&proc));
    INIT.set(proc.clone());
    proc
}

pub const CLONE_VM: u64 = 0x100;
pub const CLONE_FS: u64 = 0x200;
pub const CLONE_FILES: u64 = 0x400;
pub const CLONE_SIGHAND: u64 = 0x800;
pub const CLONE_PIDFD: u64 = 0x1000;
pub const CLONE_VFORK: u64 = 0x4000;
pub const CLONE_PARENT: u64 = 0x8000;
pub const CLONE_THREAD: u64 = 0x10000;
pub const CLONE_NEWNS: u64 = 0x20000;
pub const CLONE_SYSVSEM: u64 = 0x40000;
pub const CLONE_SETTLS: u64 = 0x80000;
pub const CLONE_PARENT_SETTID: u64 = 0x100000;
pub const CLONE_CHILD_CLEARTID: u64 = 0x200000;
pub const CLONE_DETACHED: u64 = 0x400000;
pub const CLONE_CHILD_SETTID: u64 = 0x1000000;
pub const CLONE_NEWCGROUP: u64 = 0x2000000;
pub const CLONE_NEWUTS: u64 = 0x4000000;
pub const CLONE_NEWIPC: u64 = 0x8000000;
pub const CLONE_NEWUSER: u64 = 0x10000000;
pub const CLONE_NEWPID: u64 = 0x20000000;
pub const CLONE_NEWNET: u64 = 0x40000000;
pub const CLONE_IO: u64 = 0x80000000;
pub const CLONE_CLEAR_SIGHAND: u64 = 0x1_0000_0000;

/// A new user thread or process from the current one; returns the new tid.
pub fn clone(flags: u64, stack: usize, ptid: usize, tls: usize, ctid: usize, frame: &TrapFrame) -> Result<u32> {
    let me = current_thread();
    let proc = &me.proc;
    if flags & CLONE_THREAD != 0 && flags & CLONE_SIGHAND == 0 {
        return Err(EINVAL);
    }
    if flags & CLONE_SIGHAND != 0 && flags & CLONE_VM == 0 {
        return Err(EINVAL);
    }
    if flags & (CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET) != 0 {
        return Err(EINVAL); // namespaces: milestone M8
    }
    let mm = if flags & CLONE_VM != 0 { me.mm.clone() } else { me.mm.fork()? };
    let files = if flags & CLONE_FILES != 0 { me.files.clone() } else { Arc::new(SpinLock::new(me.files.lock().clone())) };
    let fs = if flags & CLONE_FS != 0 {
        me.fs.clone()
    } else {
        let f = me.fs.lock();
        Arc::new(SpinLock::new(FsContext { root: f.root.clone(), cwd: f.cwd.clone(), umask: f.umask }))
    };
    let sighand = if flags & CLONE_SIGHAND != 0 {
        me.sighand.clone()
    } else {
        let mut h = me.sighand.lock().clone();
        if flags & CLONE_CLEAR_SIGHAND != 0 {
            h.reset_handled();
        }
        Arc::new(SpinLock::new(h))
    };

    // the child's registers: the parent's, returning 0, maybe on a new stack and TLS
    let mut cf = *frame;
    arch::set_syscall_ret(&mut cf, 0);
    if stack != 0 {
        arch::set_sp(&mut cf, stack);
    }
    let own_parent = own();
    let mut child_tls = arch::read_tls();
    if flags & CLONE_SETTLS != 0 {
        child_tls = tls;
        arch::set_frame_tls(&mut cf, tls);
    }
    let mut fp = FpState::default();
    arch::save_fp(&mut fp);

    let task = sched::new_task_with_stack(user_thread_start, 0, |top| top - core::mem::size_of::<TrapFrame>());
    let tid = task.tid;
    let fpos = task.kstack_top() - core::mem::size_of::<TrapFrame>();
    unsafe { *(fpos as *mut TrapFrame) = cf };

    let new_proc = if flags & CLONE_THREAD != 0 {
        proc.clone()
    } else {
        let parent = if flags & CLONE_PARENT != 0 { proc.parent().unwrap_or_else(|| proc.clone()) } else { proc.clone() };
        let p = Arc::new(Process {
            pid: tid,
            parent: SpinLock::new(Arc::downgrade(&parent)),
            children: SpinLock::new(Vec::new()),
            threads: SpinLock::new(Vec::new()),
            pgid: AtomicU32::new(proc.pgid.load(Ordering::Relaxed)),
            sid: AtomicU32::new(proc.sid.load(Ordering::Relaxed)),
            state: SpinLock::new(ProcState { exit_status: None, reaped: false, stop_report: None, cont_report: false }),
            child_wq: WaitQueue::new(),
            cred: SpinLock::new(proc.cred()),
            rlimits: SpinLock::new(*proc.rlimits.lock()),
            pending: SpinLock::new(SigPending::default()),
            exit_signal: (flags & 0xff) as u32,
            group_exit: AtomicBool::new(false),
            group_exit_code: AtomicI32::new(0),
            live_threads: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
            stop_wq: WaitQueue::new(),
            start_ticks: arch::ticks(),
            utime: AtomicUsize::new(0),
            stime: AtomicUsize::new(0),
            cutime: AtomicUsize::new(0),
            cstime: AtomicUsize::new(0),
            pdeath_signal: AtomicU32::new(0),
            dumpable: AtomicU32::new(proc.dumpable.load(Ordering::Relaxed)),
            ctty: SpinLock::new(proc.ctty.lock().clone()),
            pidfd_wq: WaitQueue::new(),
            itimer_gen: core::sync::atomic::AtomicU64::new(0),
        });
        parent.children.lock().push(p.clone());
        PROCS.lock().insert(tid, Arc::downgrade(&p));
        p
    };
    new_proc.live_threads.fetch_add(1, Ordering::SeqCst);
    new_proc.threads.lock().push(Arc::downgrade(&task));

    let vfork = if flags & CLONE_VFORK != 0 { Some(Completion::new()) } else { None };
    let th = UserThread {
        proc: new_proc,
        mm,
        files,
        fs,
        sighand,
        pending: SpinLock::new(SigPending::default()),
        sigmask: AtomicUsize::new(me.sigmask.load(Ordering::Relaxed)),
        own: UnsafeCell::new(Own {
            fp,
            tls: child_tls,
            clear_child_tid: if flags & CLONE_CHILD_CLEARTID != 0 { ctid } else { 0 },
            robust_list: 0,
            altstack: if flags & CLONE_VM != 0 && flags & CLONE_VFORK == 0 { (0, 0, signal::SS_DISABLE) } else { own_parent.altstack },
            frame: fpos as *mut TrapFrame,
            orig_arg0: 0,
            in_syscall: false,
            saved_mask: None,
        }),
        comm: SpinLock::new(*me.comm.lock()),
        vfork_done: SpinLock::new(vfork.clone()),
        exit_now: AtomicBool::new(false),
    };
    unsafe { *task.user.get() = Some(alloc::boxed::Box::new(th)) };
    TASKS.lock().insert(tid, Arc::downgrade(&task));

    if flags & CLONE_PARENT_SETTID != 0 {
        crate::mm::vm::write_user(ptid, &(tid as i32))?;
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        // in the child's address space (the same one unless fork)
        let child_mm = thread_of(&task).unwrap().mm.clone();
        let pa = child_mm.get_user_page(ctid, crate::mm::vm::Access::Write)?;
        unsafe { *((arch::phys_to_virt(pa) + (ctid & 0xfff)) as *mut i32) = tid as i32 };
        crate::mm::frame::put_page(pa);
    }
    task.cpu.store(crate::percpu::this().id, Ordering::Relaxed);
    sched::enqueue(task);
    if let Some(c) = vfork {
        c.wait();
    }
    Ok(tid)
}

extern "C" fn user_thread_start(_: usize) {
    let f = own().frame;
    enter_user(f)
}

/// Leaves the kernel to user mode with the trap frame at `f` (on this task's kernel stack).
pub fn enter_user(f: *mut TrapFrame) -> ! {
    crate::trap::return_to_user(unsafe { &mut *f });
    arch::irq_disable();
    unsafe { arch::jump_to_user(f) }
}

/// Ends the current thread. With `group`, the whole process (exit_group, fatal signal).
pub fn exit(code: i32, group: bool) -> ! {
    let me = current_thread();
    let proc = me.proc.clone();
    if group && !proc.group_exit.swap(true, Ordering::SeqCst) {
        proc.group_exit_code.store(code, Ordering::SeqCst);
        // kill the other threads
        for t in proc.threads.lock().iter().filter_map(|w| w.upgrade()) {
            if !core::ptr::eq(Arc::as_ptr(&t), sched::current()) {
                signal::send_thread(&t, SigInfo::kernel(signal::SIGKILL));
            }
        }
    }
    let o = own();
    // CLONE_CHILD_CLEARTID: clear the tid word and wake a futex waiter (pthread_join)
    if o.clear_child_tid != 0 {
        let _ = crate::mm::vm::write_user(o.clear_child_tid, &0i32);
        let _ = crate::futex::wake(o.clear_child_tid, false, 1, u32::MAX);
    }
    crate::futex::exit_robust_list(o.robust_list);
    if let Some(c) = me.vfork_done.lock().take() {
        c.complete();
    }
    let status = if proc.group_exit.load(Ordering::SeqCst) { proc.group_exit_code.load(Ordering::SeqCst) } else { code };
    let me_task = sched::current_arc();
    TASKS.lock().remove(&me_task.tid);
    proc.threads.lock().retain(|w| w.upgrade().is_some_and(|t| !Arc::ptr_eq(&t, &me_task)));
    let last = proc.live_threads.fetch_sub(1, Ordering::SeqCst) == 1;
    // drop our references to shared state while we can still sleep
    {
        let th = unsafe { me_task.user_mut().unwrap() };
        let fds = core::mem::replace(&mut th.files, Arc::new(SpinLock::new(FdTable::new())));
        drop(fds);
    }
    if last {
        process_exit(&proc, status);
    } else {
        proc.child_wq.wake_all();
    }
    sched::exit_current();
}

/// The last thread of `proc` is gone: zombie it, reparent its children, tell the parent.
fn process_exit(proc: &Arc<Process>, status: i32) {
    crate::locks::release_process(proc.pid);
    let init = INIT.get().cloned();
    let orphans: Vec<Arc<Process>> = core::mem::take(&mut *proc.children.lock());
    if let Some(init) = &init {
        for c in orphans {
            *c.parent.lock() = Arc::downgrade(init);
            let zombie = c.is_zombie();
            let sig = c.pdeath_signal.load(Ordering::Relaxed);
            if sig != 0 {
                signal::send_process(&c, SigInfo::kernel(sig));
            }
            init.children.lock().push(c);
            if zombie {
                init.child_wq.wake_all();
            }
        }
    }
    if proc.ctty.lock().is_some() && proc.sid.load(Ordering::Relaxed) == proc.pid {
        if let Some(t) = proc.ctty.lock().take() {
            t.session_leader_exit(proc.sid.load(Ordering::Relaxed));
        }
    }
    proc.state.lock().exit_status = Some(status);
    proc.pidfd_wq.wake_all();
    match proc.parent() {
        Some(parent) => {
            if proc.exit_signal != 0 {
                let mut info = SigInfo::kernel(proc.exit_signal);
                info.code = if status & 0x7f == 0 { 1 } else { 2 }; // CLD_EXITED / CLD_KILLED
                info.pid = proc.pid as i32;
                info.status = if status & 0x7f == 0 { (status >> 8) & 0xff } else { status & 0x7f };
                signal::send_process(&parent, info);
            }
            parent.child_wq.wake_all();
            // a parent ignoring SIGCHLD (or with SA_NOCLDWAIT) does not keep zombies
            let auto = parent.leader().and_then(|l| thread_of(&l).map(|t| t.sighand.lock().auto_reap())).unwrap_or(false);
            if auto {
                reap(&parent, proc);
            }
        }
        None => {
            PROCS.lock().remove(&proc.pid);
        }
    }
}

fn reap(parent: &Arc<Process>, child: &Arc<Process>) {
    child.state.lock().reaped = true;
    parent.children.lock().retain(|c| !Arc::ptr_eq(c, child));
    PROCS.lock().remove(&child.pid);
}

pub const WNOHANG: u32 = 1;
pub const WUNTRACED: u32 = 2;
pub const WEXITED: u32 = 4;
pub const WCONTINUED: u32 = 8;
pub const WNOWAIT: u32 = 0x0100_0000;

pub enum WaitWho {
    Any,
    Pid(u32),
    Pgid(u32),
}

/// wait4/waitid: Ok(Some((pid, status))) or Ok(None) with WNOHANG and nothing to report.
pub fn wait(who: WaitWho, options: u32) -> Result<Option<(u32, i32, u32)>> {
    let me = current_proc().clone();
    let mut found: Option<(u32, i32, u32)> = None;
    let r = me.child_wq.wait_until(
        || {
            let kids = me.children.lock().clone();
            let matching: Vec<_> = kids
                .iter()
                .filter(|c| match who {
                    WaitWho::Any => true,
                    WaitWho::Pid(p) => c.pid == p,
                    WaitWho::Pgid(g) => c.pgid.load(Ordering::Relaxed) == g,
                })
                .cloned()
                .collect();
            if matching.is_empty() {
                found = Some((0, 0, u32::MAX)); // ECHILD
                return true;
            }
            for c in matching {
                let mut st = c.state.lock();
                if let (Some(s), false) = (st.exit_status, st.reaped) {
                    if options & WEXITED != 0 || options & (WUNTRACED | WCONTINUED) == 0 || true {
                        if options & WNOWAIT == 0 {
                            st.reaped = true;
                            drop(st);
                            me.cutime.fetch_add(c.utime.load(Ordering::Relaxed), Ordering::Relaxed);
                            me.cstime.fetch_add(c.stime.load(Ordering::Relaxed), Ordering::Relaxed);
                            me.children.lock().retain(|x| !Arc::ptr_eq(x, &c));
                            PROCS.lock().remove(&c.pid);
                        }
                        found = Some((c.pid, s, 0));
                        return true;
                    }
                }
                if options & WUNTRACED != 0 {
                    if let Some(s) = st.stop_report.take() {
                        found = Some((c.pid, s, 1));
                        return true;
                    }
                }
                if options & WCONTINUED != 0 && st.cont_report {
                    st.cont_report = false;
                    found = Some((c.pid, 0xffff, 2));
                    return true;
                }
            }
            options & WNOHANG != 0
        },
        None,
        true,
    );
    if let sched::WaitResult::Interrupted = r {
        return Err(ERESTARTSYS);
    }
    match found {
        Some((_, _, u32::MAX)) => Err(ECHILD),
        Some(x) => Ok(Some(x)),
        None => Ok(None),
    }
}
