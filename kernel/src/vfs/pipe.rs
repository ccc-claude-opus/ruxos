//! Pipes and FIFOs: a 64 KB ring buffer with reader and writer counts.
use super::file::{File, FileOps, O_ACCMODE, O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY};
use super::poll::*;
use super::*;
use alloc::collections::VecDeque;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering::*;

pub const PIPE_BUF: usize = 4096;
const CAPACITY: usize = 65536;

pub struct Pipe {
    buf: SpinLock<VecDeque<u8>>,
    readers: AtomicUsize,
    writers: AtomicUsize,
    /// Writers ever attached (a FIFO reader opened before any writer does not see EOF).
    had_writer: core::sync::atomic::AtomicBool,
    pub pq: PollQueue,
    pub capacity: AtomicUsize,
    pub inode: Arc<dyn Inode>,
}

impl Pipe {
    pub fn new() -> Arc<Pipe> {
        Arc::new(Pipe {
            buf: SpinLock::new(VecDeque::new()),
            readers: AtomicUsize::new(0),
            writers: AtomicUsize::new(0),
            had_writer: core::sync::atomic::AtomicBool::new(false),
            pq: PollQueue::new(),
            capacity: AtomicUsize::new(CAPACITY),
            inode: super::anon::AnonInode::new(S_IFIFO | 0o600),
        })
    }

    pub fn len(&self) -> usize {
        self.buf.lock().len()
    }
}

pub struct PipeEnd {
    pub pipe: Arc<Pipe>,
    read: bool,
    write: bool,
}

impl PipeEnd {
    pub fn new(pipe: &Arc<Pipe>, read: bool, write: bool) -> Arc<PipeEnd> {
        if read {
            pipe.readers.fetch_add(1, SeqCst);
        }
        if write {
            pipe.writers.fetch_add(1, SeqCst);
            pipe.had_writer.store(true, SeqCst);
        }
        pipe.pq.notify(POLLIN | POLLOUT);
        Arc::new(PipeEnd { pipe: pipe.clone(), read, write })
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        if self.read {
            self.pipe.readers.fetch_sub(1, SeqCst);
        }
        if self.write {
            self.pipe.writers.fetch_sub(1, SeqCst);
        }
        self.pipe.pq.notify(POLLIN | POLLOUT | POLLHUP | POLLERR);
    }
}

impl FileOps for PipeEnd {
    fn read(&self, f: &File, out: &mut [u8]) -> Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let p = &self.pipe;
        loop {
            {
                let mut b = p.buf.lock();
                if !b.is_empty() {
                    let n = out.len().min(b.len());
                    let (a, c) = b.as_slices();
                    let k = n.min(a.len());
                    out[..k].copy_from_slice(&a[..k]);
                    if n > k {
                        out[k..n].copy_from_slice(&c[..n - k]);
                    }
                    b.drain(..n);
                    drop(b);
                    p.pq.notify(POLLOUT);
                    return Ok(n);
                }
            }
            if p.writers.load(SeqCst) == 0 && p.had_writer.load(SeqCst) {
                return Ok(0);
            }
            if f.nonblock() {
                return Err(EAGAIN);
            }
            let r = p.pq.wq.wait_until(
                || !p.buf.lock().is_empty() || (p.writers.load(SeqCst) == 0 && p.had_writer.load(SeqCst)),
                None,
                true,
            );
            if let crate::sched::WaitResult::Interrupted = r {
                return Err(ERESTARTSYS);
            }
        }
    }

    fn write(&self, f: &File, data: &[u8]) -> Result<usize> {
        let p = &self.pipe;
        let mut done = 0;
        while done < data.len() {
            if p.readers.load(SeqCst) == 0 {
                crate::task::send_signal_current(crate::task::signal::SIGPIPE);
                return if done > 0 { Ok(done) } else { Err(EPIPE) };
            }
            let cap = p.capacity.load(Relaxed);
            {
                let mut b = p.buf.lock();
                let room = cap.saturating_sub(b.len());
                let rest = data.len() - done;
                // writes of at most PIPE_BUF bytes are atomic
                let can = if rest <= PIPE_BUF { if room >= rest { rest } else { 0 } } else { room.min(rest) };
                if can > 0 {
                    b.extend(&data[done..done + can]);
                    done += can;
                    drop(b);
                    p.pq.notify(POLLIN);
                    continue;
                }
            }
            if f.nonblock() {
                return if done > 0 { Ok(done) } else { Err(EAGAIN) };
            }
            let rest = data.len() - done;
            let r = p.pq.wq.wait_until(
                || {
                    let room = cap.saturating_sub(p.buf.lock().len());
                    p.readers.load(SeqCst) == 0 || room >= rest.min(PIPE_BUF)
                },
                None,
                true,
            );
            if let crate::sched::WaitResult::Interrupted = r {
                return if done > 0 { Ok(done) } else { Err(ERESTARTSYS) };
            }
        }
        Ok(done)
    }

    fn poll(&self, _f: &File) -> u32 {
        let p = &self.pipe;
        let mut ev = 0;
        let len = p.buf.lock().len();
        if self.read {
            if len > 0 {
                ev |= POLLIN | POLLRDNORM;
            }
            if p.writers.load(SeqCst) == 0 && p.had_writer.load(SeqCst) {
                ev |= POLLHUP;
            }
        }
        if self.write {
            if p.capacity.load(Relaxed) - len.min(p.capacity.load(Relaxed)) >= PIPE_BUF {
                ev |= POLLOUT | POLLWRNORM;
            }
            if p.readers.load(SeqCst) == 0 {
                ev |= POLLERR;
            }
        }
        ev
    }

    fn poll_queue(&self) -> Option<&PollQueue> {
        Some(&self.pipe.pq)
    }

    fn inode(&self) -> Option<Arc<dyn Inode>> {
        Some(self.pipe.inode.clone())
    }

    fn ioctl(&self, _f: &File, cmd: u32, arg: usize) -> Result<isize> {
        match cmd {
            0x541b => {
                crate::mm::vm::write_user(arg, &(self.pipe.len() as i32))?;
                Ok(0)
            }
            _ => Err(ENOTTY),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// FIFOs: the pipe behind a FIFO inode, shared by everyone who opens it.
static FIFOS: SpinLock<BTreeMap<usize, alloc::sync::Weak<Pipe>>> = SpinLock::new(BTreeMap::new());

pub fn open_fifo(inode: &Arc<dyn Inode>, flags: u32) -> Result<Arc<dyn FileOps>> {
    let key = Arc::as_ptr(inode) as *const u8 as usize;
    let pipe = {
        let mut m = FIFOS.lock();
        m.retain(|_, w| w.strong_count() > 0);
        match m.get(&key).and_then(|w| w.upgrade()) {
            Some(p) => p,
            None => {
                let p = Pipe::new();
                m.insert(key, Arc::downgrade(&p));
                p
            }
        }
    };
    let (r, w) = match flags & O_ACCMODE {
        O_RDONLY => (true, false),
        O_WRONLY => (false, true),
        O_RDWR => (true, true),
        _ => return Err(EINVAL),
    };
    let end = PipeEnd::new(&pipe, r, w);
    // opening blocks until the other side is there (unless O_NONBLOCK or O_RDWR)
    if flags & O_NONBLOCK == 0 && !(r && w) {
        let other = || if r { pipe.writers.load(SeqCst) > 0 } else { pipe.readers.load(SeqCst) > 0 };
        if let crate::sched::WaitResult::Interrupted = pipe.pq.wq.wait_until(other, None, true) {
            return Err(ERESTARTSYS);
        }
    } else if w && !r && pipe.readers.load(SeqCst) == 0 {
        return Err(ENXIO);
    }
    Ok(end)
}
