//! The console tty: the UART the kernel prints to, with interrupt-driven input and a line
//! discipline (canonical mode, echo, erase/kill, signals from ^C ^\ ^Z), termios, window size, and
//! the controlling terminal of a session.
use crate::errno::*;
use crate::mm::vm::{read_user, write_user};
use crate::sync::{Once, SpinLock};
use crate::task::signal::{self, SigInfo};
use crate::vfs::file::{File, FileOps, O_NOCTTY};
use crate::vfs::poll::*;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU32, Ordering};

// termios flags (asm-generic/termbits.h)
const ICRNL: u32 = 0o400;
const IXON: u32 = 0o2000;
const OPOST: u32 = 1;
const ONLCR: u32 = 4;
const ISIG: u32 = 1;
const ICANON: u32 = 2;
const ECHO: u32 = 0o10;
const ECHOE: u32 = 0o20;
const ECHOK: u32 = 0o40;
const ECHOCTL: u32 = 0o1000;
const ECHOKE: u32 = 0o4000;
const IEXTEN: u32 = 0o100000;
const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const VTIME: usize = 5;
const VMIN: usize = 6;
const VSUSP: usize = 10;

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; 19],
}
unsafe impl crate::mm::vm::Pod for Termios {}

impl Default for Termios {
    fn default() -> Self {
        let mut cc = [0u8; 19];
        cc[VINTR] = 3;
        cc[VQUIT] = 0x1c;
        cc[VERASE] = 0x7f;
        cc[VKILL] = 0x15;
        cc[VEOF] = 4;
        cc[VMIN] = 1;
        cc[VSUSP] = 0x1a;
        cc[11] = 0x11; // VSTART
        cc[12] = 0x13; // VSTOP
        cc[14] = 0x17; // VWERASE
        cc[15] = 0x16; // VLNEXT
        Termios {
            iflag: ICRNL | IXON,
            oflag: OPOST | ONLCR,
            cflag: 0o277 | 0o4000, // B38400 CS8 CREAD HUPCL...
            lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
            line: 0,
            cc,
        }
    }
}

struct Input {
    /// Bytes ready for read() (complete lines in canonical mode).
    ready: VecDeque<u8>,
    /// The line being edited (canonical mode).
    line: Vec<u8>,
    /// ^D at the start of a line: read() returns 0 once.
    eof: usize,
    termios: Termios,
}

pub struct Tty {
    input: SpinLock<Input>,
    pub pq: PollQueue,
    pub pgrp: AtomicU32,
    pub sid: AtomicU32,
    winsize: SpinLock<[u16; 4]>,
}

pub static CONSOLE: Once<Arc<Tty>> = Once::new();

impl Tty {
    fn output(&self, data: &[u8]) {
        let opost = { self.input.lock().termios.oflag };
        if opost & OPOST != 0 && opost & ONLCR != 0 {
            crate::console::write_bytes(data);
        } else {
            for &c in data {
                crate::console::putc(c);
            }
        }
    }

    fn echo(&self, t: &Termios, c: u8) {
        if t.lflag & ECHO == 0 {
            return;
        }
        if c < 0x20 && c != b'\n' && c != b'\t' && t.lflag & ECHOCTL != 0 {
            crate::console::putc(b'^');
            crate::console::putc(c + 0x40);
        } else if c == b'\n' {
            crate::console::write_bytes(b"\n");
        } else {
            crate::console::putc(c);
        }
    }

    /// One byte from the UART.
    pub fn receive(&self, mut c: u8) {
        if c == 0x14 && crate::kernel_param("ruxos.debug").is_some() {
            crate::sched::dump_tasks();
            return;
        }
        let mut sig = None;
        {
            let mut i = self.input.lock();
            let t = i.termios;
            if t.iflag & ICRNL != 0 && c == b'\r' {
                c = b'\n';
            }
            if t.lflag & ISIG != 0 {
                if c == t.cc[VINTR] {
                    sig = Some(signal::SIGINT);
                } else if c == t.cc[VQUIT] {
                    sig = Some(signal::SIGQUIT);
                } else if c == t.cc[VSUSP] {
                    sig = Some(signal::SIGTSTP);
                }
            }
            if let Some(_s) = sig {
                i.line.clear();
                self.echo(&t, c);
            } else if t.lflag & ICANON != 0 {
                if c == t.cc[VERASE] || c == 8 {
                    if i.line.pop().is_some() && t.lflag & ECHO != 0 {
                        crate::console::write_bytes(b"\x08 \x08");
                    }
                } else if c == t.cc[VKILL] {
                    while i.line.pop().is_some() {
                        if t.lflag & ECHO != 0 {
                            crate::console::write_bytes(b"\x08 \x08");
                        }
                    }
                } else if c == t.cc[VEOF] {
                    if i.line.is_empty() {
                        i.eof += 1;
                    } else {
                        let l = core::mem::take(&mut i.line);
                        i.ready.extend(l);
                    }
                } else {
                    i.line.push(c);
                    self.echo(&t, c);
                    if c == b'\n' {
                        let l = core::mem::take(&mut i.line);
                        i.ready.extend(l);
                    }
                }
            } else {
                i.ready.push_back(c);
                self.echo(&t, c);
            }
        }
        if let Some(s) = sig {
            let pg = self.pgrp.load(Ordering::Relaxed);
            if pg != 0 {
                let _ = crate::syscall::proc::kill_pgrp(pg, SigInfo::kernel(s));
            }
        }
        self.pq.notify(POLLIN);
    }

    fn can_read(&self) -> bool {
        let i = self.input.lock();
        !i.ready.is_empty() || i.eof > 0
    }

    /// The session leader of the tty's session exited: the session loses its terminal.
    pub fn session_leader_exit(&self, sid: u32) {
        if self.sid.load(Ordering::Relaxed) == sid {
            self.sid.store(0, Ordering::Relaxed);
            let pg = self.pgrp.swap(0, Ordering::Relaxed);
            if pg != 0 {
                let _ = crate::syscall::proc::kill_pgrp(pg, SigInfo::kernel(signal::SIGHUP));
                let _ = crate::syscall::proc::kill_pgrp(pg, SigInfo::kernel(signal::SIGCONT));
            }
        }
    }
}

pub struct TtyFile {
    pub tty: Arc<Tty>,
}

impl FileOps for TtyFile {
    fn read(&self, f: &File, buf: &mut [u8]) -> Result<usize> {
        let tty = &self.tty;
        loop {
            {
                let mut i = tty.input.lock();
                if !i.ready.is_empty() {
                    let canon = i.termios.lflag & ICANON != 0;
                    let mut n = 0;
                    while n < buf.len() {
                        let Some(c) = i.ready.pop_front() else { break };
                        buf[n] = c;
                        n += 1;
                        if canon && c == b'\n' {
                            break;
                        }
                    }
                    return Ok(n);
                }
                if i.eof > 0 {
                    i.eof -= 1;
                    return Ok(0);
                }
                if i.termios.lflag & ICANON == 0 && i.termios.cc[VMIN] == 0 && i.termios.cc[VTIME] == 0 {
                    return Ok(0);
                }
            }
            if f.nonblock() {
                return Err(EAGAIN);
            }
            if let crate::sched::WaitResult::Interrupted = tty.pq.wq.wait_until(|| tty.can_read(), None, true) {
                return Err(ERESTARTSYS);
            }
        }
    }

    fn write(&self, _f: &File, buf: &[u8]) -> Result<usize> {
        self.tty.output(buf);
        Ok(buf.len())
    }

    fn poll(&self, _f: &File) -> u32 {
        POLLOUT | POLLWRNORM | if self.tty.can_read() { POLLIN | POLLRDNORM } else { 0 }
    }

    fn poll_queue(&self) -> Option<&PollQueue> {
        Some(&self.tty.pq)
    }

    fn ioctl(&self, _f: &File, cmd: u32, arg: usize) -> Result<isize> {
        let tty = &self.tty;
        // job control ioctls only on the caller's controlling terminal
        let is_ctty = || crate::task::current_proc().ctty.lock().as_ref().is_some_and(|t| Arc::ptr_eq(t, tty));
        if matches!(cmd, 0x540f | 0x5410 | 0x5429) && !is_ctty() {
            return Err(ENOTTY);
        }
        match cmd {
            0x5401 => write_user(arg, &tty.input.lock().termios)?, // TCGETS
            0x5402..=0x5404 => {
                // TCSETS, TCSETSW, TCSETSF
                let t: Termios = read_user(arg)?;
                let mut i = tty.input.lock();
                if cmd == 0x5404 {
                    i.ready.clear();
                    i.line.clear();
                }
                // leaving canonical mode makes the partial line readable
                if i.termios.lflag & ICANON != 0 && t.lflag & ICANON == 0 {
                    let l = core::mem::take(&mut i.line);
                    i.ready.extend(l);
                }
                i.termios = t;
            }
            0x5413 => write_user(arg, &*tty.winsize.lock())?, // TIOCGWINSZ
            0x5414 => *tty.winsize.lock() = read_user(arg)?,  // TIOCSWINSZ
            0x540f => write_user(arg, &(tty.pgrp.load(Ordering::Relaxed) as i32))?, // TIOCGPGRP
            0x5410 => {
                // TIOCSPGRP
                let pg: i32 = read_user(arg)?;
                tty.pgrp.store(pg as u32, Ordering::Relaxed);
            }
            0x5429 => {
                // TIOCGSID
                let sid = tty.sid.load(Ordering::Relaxed);
                if sid == 0 {
                    return Err(ENOTTY);
                }
                write_user(arg, &(sid as i32))?
            }
            0x540e => {
                // TIOCSCTTY
                let p = crate::task::current_proc();
                if p.sid.load(Ordering::Relaxed) != p.pid {
                    return Err(EPERM);
                }
                tty.sid.store(p.pid, Ordering::Relaxed);
                tty.pgrp.store(p.pgid.load(Ordering::Relaxed), Ordering::Relaxed);
                *p.ctty.lock() = Some(tty.clone());
            }
            0x5422 => {
                // TIOCNOTTY
                *crate::task::current_proc().ctty.lock() = None;
            }
            0x541b => write_user(arg, &(tty.input.lock().ready.len() as i32))?, // FIONREAD
            0x5411 => write_user(arg, &0i32)?,                                  // TIOCOUTQ
            0x540b | 0x5409 | 0x540a => {
                // TCFLSH, TCSBRK, TCXONC
                if cmd == 0x540b && (arg == 0 || arg == 2) {
                    let mut i = tty.input.lock();
                    i.ready.clear();
                    i.line.clear();
                }
            }
            _ => return Err(ENOTTY),
        }
        Ok(0)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn open_tty(minor: u32, flags: u32, console: bool) -> Result<Arc<dyn FileOps>> {
    let _ = minor;
    let tty = CONSOLE.get().ok_or(ENXIO)?.clone();
    // a session leader without a terminal acquires this one (never through /dev/console)
    if !console && flags & O_NOCTTY == 0 {
        if let Some(th) = crate::task::try_current_thread() {
            let p = &th.proc;
            if p.sid.load(Ordering::Relaxed) == p.pid && p.ctty.lock().is_none() && tty.sid.load(Ordering::Relaxed) == 0 {
                tty.sid.store(p.pid, Ordering::Relaxed);
                tty.pgrp.store(p.pgid.load(Ordering::Relaxed), Ordering::Relaxed);
                *p.ctty.lock() = Some(tty.clone());
            }
        }
    }
    Ok(Arc::new(TtyFile { tty }))
}

fn open_major5(minor: u32, flags: u32) -> Result<Arc<dyn FileOps>> {
    match minor {
        0 => {
            // /dev/tty: the controlling terminal
            let t = crate::task::current_proc().ctty.lock().clone().ok_or(ENXIO)?;
            Ok(Arc::new(TtyFile { tty: t }))
        }
        1 => open_tty(1, flags, true),
        _ => Err(ENXIO),
    }
}

fn open_serial(minor: u32, flags: u32) -> Result<Arc<dyn FileOps>> {
    open_tty(minor, flags, false)
}

static UART_PL011: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn uart_irq(_irq: u32) {
    let (base, pl011) = crate::console::uart();
    let Some(tty) = CONSOLE.get() else { return };
    unsafe {
        if pl011 {
            while core::ptr::read_volatile((base + 0x18) as *const u32) & (1 << 4) == 0 {
                let c = core::ptr::read_volatile(base as *const u32) as u8;
                tty.receive(c);
            }
            core::ptr::write_volatile((base + 0x44) as *mut u32, 0x7ff); // ICR
        } else {
            let sh = crate::console::uart_shift();
            while core::ptr::read_volatile((base + (5 << sh)) as *const u8) & 1 != 0 {
                let c = core::ptr::read_volatile(base as *const u8);
                tty.receive(c);
            }
        }
    }
}

pub fn init(fdt: &crate::fdt::Fdt) {
    let tty = Arc::new(Tty {
        input: SpinLock::new(Input { ready: VecDeque::new(), line: Vec::new(), eof: 0, termios: Termios::default() }),
        pq: PollQueue::new(),
        pgrp: AtomicU32::new(0),
        sid: AtomicU32::new(0),
        winsize: SpinLock::new([24, 80, 0, 0]),
    });
    CONSOLE.set(tty);
    use crate::vfs::{dev, makedev, S_IFCHR};
    dev::register_chr(5, open_major5);
    dev::register_chr(204, open_serial);
    dev::register_chr(4, open_serial);
    dev::add_node(b"console", S_IFCHR | 0o600, makedev(5, 1));
    dev::add_node(b"tty", S_IFCHR | 0o666, makedev(5, 0));
    let (base, pl011) = crate::console::uart();
    UART_PL011.store(pl011, Ordering::Relaxed);
    if pl011 {
        dev::add_node(b"ttyAMA0", S_IFCHR | 0o620, makedev(204, 64));
    } else {
        dev::add_node(b"ttyS0", S_IFCHR | 0o620, makedev(4, 64));
    }
    // the UART's interrupt
    let node = if pl011 { fdt.find_compatible("arm,pl011") } else { fdt.find_compatible("ns16550a") };
    if let Some(irq) = node.as_ref().and_then(|n| crate::arch::dt_irq(n, 0)) {
        crate::irq::register(irq, uart_irq);
        unsafe {
            if pl011 {
                core::ptr::write_volatile((base + 0x44) as *mut u32, 0x7ff);
                core::ptr::write_volatile((base + 0x38) as *mut u32, (1 << 4) | (1 << 6)); // RXIM | RTIM
            } else {
                let sh = crate::console::uart_shift();
                core::ptr::write_volatile((base + (1 << sh)) as *mut u8, 1); // IER: received data
            }
        }
    }
}
