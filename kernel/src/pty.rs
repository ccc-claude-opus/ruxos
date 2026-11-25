//! Pseudo-terminals (pty(7), devpts): opening ptmx gives a master and creates the slave
//! /dev/pts/N (136:N) in the same devpts instance. The slave is an ordinary terminal (tty.rs)
//! whose output goes to the master; what is written to the master is the slave's input.
use crate::errno::*;
use crate::mm::vm::{read_user, write_user};
use crate::sync::SpinLock;
use crate::tty::{Sink, Tty, TtyFile};
use crate::vfs::file::{File, FileOps};
use crate::vfs::poll::*;
use crate::vfs::{FileSystem, Inode};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

pub const PTY_SLAVE_MAJOR: u32 = 136;

/// A devpts instance: its file system and its terminals by index.
pub struct Devpts {
    fs: Weak<dyn FileSystem>,
    next: AtomicU32,
    ptys: SpinLock<BTreeMap<u32, Weak<Master>>>,
    /// mode= of new slaves, gid= (kernel id; u32::MAX: the opener's)
    mode: u32,
    gid: u32,
}

static INSTANCES: SpinLock<Vec<Arc<Devpts>>> = SpinLock::new(Vec::new());

/// A new devpts instance on `fs` (whose root holds ptmx).
pub fn register(fs: &Arc<dyn FileSystem>, mode: u32, gid: u32) {
    let mut v = INSTANCES.lock();
    v.retain(|d| d.fs.strong_count() > 0);
    v.push(Arc::new(Devpts { fs: Arc::downgrade(fs), next: AtomicU32::new(0), ptys: SpinLock::new(BTreeMap::new()), mode, gid }));
}

fn instance_of(fs: &Arc<dyn FileSystem>) -> Option<Arc<Devpts>> {
    INSTANCES.lock().iter().find(|d| d.fs.upgrade().is_some_and(|f| core::ptr::addr_eq(Arc::as_ptr(&f), Arc::as_ptr(fs)))).cloned()
}

/// The devpts instance an opened node belongs to: its own file system (ptmx and slaves in a devpts
/// mount), or for /dev/ptmx elsewhere the one mounted on /dev/pts in the caller's namespace.
fn instance_for(inode: &Arc<dyn Inode>) -> Result<Arc<Devpts>> {
    if let Some(d) = inode.fs().and_then(|f| instance_of(&f)) {
        return Ok(d);
    }
    let p = crate::syscall::fs::lookup_at(-100, b"/dev/pts", true, false)?;
    instance_of(&p.d.fs).ok_or(ENXIO)
}

pub struct Master {
    index: u32,
    tty: Arc<Tty>,
    /// output of the slave, waiting to be read from the master
    out: SpinLock<VecDeque<u8>>,
    pq: PollQueue,
    /// TIOCSPTLCK: the slave cannot be opened while locked (new terminals start locked)
    locked: AtomicBool,
    /// open slave files (none left: the master reads EIO)
    slaves: AtomicUsize,
    /// a slave has been opened at least once
    opened: AtomicBool,
    devpts: Weak<Devpts>,
}

impl Master {
    /// Output of the slave (after its output processing).
    pub fn from_slave(&self, data: &[u8]) {
        self.out.lock().extend(data.iter().copied());
        self.pq.notify(POLLIN);
    }

    fn slave_gone(&self) -> bool {
        self.opened.load(Ordering::Relaxed) && self.slaves.load(Ordering::Relaxed) == 0
    }
}

struct MasterFile {
    m: Arc<Master>,
}

impl Drop for MasterFile {
    fn drop(&mut self) {
        // the terminal hangs up and its node goes away
        self.m.tty.hangup();
        if let Some(d) = self.m.devpts.upgrade() {
            d.ptys.lock().remove(&self.m.index);
            if let Some(fs) = d.fs.upgrade() {
                let _ = fs.root().unlink(alloc::format!("{}", self.m.index).as_bytes());
            }
        }
    }
}

impl FileOps for MasterFile {
    fn read(&self, f: &File, buf: &mut [u8]) -> Result<usize> {
        loop {
            {
                let mut o = self.m.out.lock();
                if !o.is_empty() {
                    let n = buf.len().min(o.len());
                    for b in buf.iter_mut().take(n) {
                        *b = o.pop_front().unwrap();
                    }
                    drop(o);
                    self.m.tty.pq.notify(POLLOUT);
                    return Ok(n);
                }
            }
            if self.m.slave_gone() {
                return Err(EIO);
            }
            if f.nonblock() {
                return Err(EAGAIN);
            }
            let m = &self.m;
            if let crate::sched::WaitResult::Interrupted = m.pq.wq.wait_until(|| !m.out.lock().is_empty() || m.slave_gone(), None, true) {
                return Err(ERESTARTSYS);
            }
        }
    }

    fn write(&self, _f: &File, buf: &[u8]) -> Result<usize> {
        for &c in buf {
            self.m.tty.receive(c);
        }
        Ok(buf.len())
    }

    fn poll(&self, _f: &File) -> u32 {
        let mut ev = POLLOUT | POLLWRNORM;
        if !self.m.out.lock().is_empty() {
            ev |= POLLIN | POLLRDNORM;
        }
        if self.m.slave_gone() {
            ev |= POLLHUP;
        }
        ev
    }

    fn poll_queue(&self) -> Option<&PollQueue> {
        Some(&self.m.pq)
    }

    fn ioctl(&self, f: &File, cmd: u32, arg: usize) -> Result<isize> {
        match cmd {
            0x8004_5430 => write_user(arg, &self.m.index)?, // TIOCGPTN
            0x4004_5431 => {
                // TIOCSPTLCK
                let v: i32 = read_user(arg)?;
                self.m.locked.store(v != 0, Ordering::Relaxed);
            }
            0x8004_5439 => write_user(arg, &(self.m.locked.load(Ordering::Relaxed) as i32))?, // TIOCGPTLCK
            0x5441 => {
                // TIOCGPTPEER: the slave, opened through the master
                let flags = arg as u32;
                let file = open_slave_of(&self.m, flags)?;
                let inode = slave_inode(&self.m);
                let fl = crate::vfs::file::File::with_node(file, flags & !crate::vfs::file::O_CLOEXEC, None, inode);
                let th = crate::task::current_thread();
                let fd = th.files.lock().alloc(fl, 0, flags & crate::vfs::file::O_CLOEXEC != 0)?;
                return Ok(fd as isize);
            }
            0x541b => write_user(arg, &(self.m.out.lock().len() as i32))?, // FIONREAD
            // termios and window size are the terminal's
            _ => return TtyFile { tty: self.m.tty.clone() }.ioctl(f, cmd, arg),
        }
        Ok(0)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The slave's file: a terminal file that counts its opens for the master.
struct SlaveFile {
    t: TtyFile,
    m: Arc<Master>,
}

impl Drop for SlaveFile {
    fn drop(&mut self) {
        self.m.slaves.fetch_sub(1, Ordering::SeqCst);
        self.m.pq.notify(POLLHUP | POLLIN);
    }
}

impl FileOps for SlaveFile {
    fn read(&self, f: &File, buf: &mut [u8]) -> Result<usize> {
        self.t.read(f, buf)
    }
    fn write(&self, f: &File, buf: &[u8]) -> Result<usize> {
        self.t.write(f, buf)
    }
    fn poll(&self, f: &File) -> u32 {
        self.t.poll(f)
    }
    fn poll_queue(&self) -> Option<&PollQueue> {
        self.t.poll_queue()
    }
    fn ioctl(&self, f: &File, cmd: u32, arg: usize) -> Result<isize> {
        if cmd == 0x8004_5430 {
            return Err(ENOTTY); // TIOCGPTN is for the master
        }
        self.t.ioctl(f, cmd, arg)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn open_slave_of(m: &Arc<Master>, flags: u32) -> Result<Arc<dyn FileOps>> {
    if m.tty.hung.load(Ordering::Relaxed) {
        return Err(EIO);
    }
    m.slaves.fetch_add(1, Ordering::SeqCst);
    m.opened.store(true, Ordering::SeqCst);
    m.tty.maybe_acquire(flags);
    Ok(Arc::new(SlaveFile { t: TtyFile { tty: m.tty.clone() }, m: m.clone() }))
}

fn slave_inode(m: &Master) -> Option<Arc<dyn Inode>> {
    let d = m.devpts.upgrade()?;
    d.fs.upgrade()?.root().lookup(alloc::format!("{}", m.index).as_bytes()).ok()
}

/// open("/dev/ptmx" or "/dev/pts/ptmx"): a new terminal pair.
pub fn open_ptmx(inode: &Arc<dyn Inode>, _flags: u32) -> Result<Arc<dyn FileOps>> {
    let d = instance_for(inode)?;
    let fs = d.fs.upgrade().ok_or(ENXIO)?;
    let index = d.next.fetch_add(1, Ordering::Relaxed);
    let m = Arc::new_cyclic(|me: &Weak<Master>| Master {
        index,
        tty: Tty::new(Sink::Pty(me.clone())),
        out: SpinLock::new(VecDeque::new()),
        pq: PollQueue::new(),
        locked: AtomicBool::new(true),
        slaves: AtomicUsize::new(0),
        opened: AtomicBool::new(false),
        devpts: Arc::downgrade(&d),
    });
    // the slave node, owned by the opener (gid= of the instance, or the opener's)
    let c = crate::task::current_proc().cred();
    let gid = if d.gid == u32::MAX { c.fsgid } else { d.gid };
    fs.root().create(alloc::format!("{}", index).as_bytes(), crate::vfs::S_IFCHR | d.mode, crate::vfs::makedev(PTY_SLAVE_MAJOR, index), c.fsuid, gid)?;
    d.ptys.lock().insert(index, Arc::downgrade(&m));
    Ok(Arc::new(MasterFile { m }))
}

/// open("/dev/pts/N").
pub fn open_slave(inode: &Arc<dyn Inode>, index: u32, flags: u32) -> Result<Arc<dyn FileOps>> {
    let d = instance_for(inode)?;
    let m = d.ptys.lock().get(&index).and_then(|w| w.upgrade()).ok_or(EIO)?;
    if m.locked.load(Ordering::Relaxed) {
        return Err(EIO);
    }
    open_slave_of(&m, flags)
}
