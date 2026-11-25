//! The kernel's own output: the serial port the device tree names in /chosen stdout-path
//! (PL011 or NS16550A), polled. The tty driver (later) takes it over for /dev/console.
use crate::fdt::Fdt;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicUsize, Ordering};

const NONE: usize = 0;
const PL011: usize = 1;
const NS16550: usize = 2;

static KIND: AtomicUsize = AtomicUsize::new(NONE);
static BASE: AtomicUsize = AtomicUsize::new(0);
static SHIFT: AtomicUsize = AtomicUsize::new(0);

pub fn init(fdt: &Fdt) {
    // stdout-path first, else the first UART we know
    let mut node = None;
    if let Some(path) = fdt.chosen().and_then(|c| c.prop_str("stdout-path")) {
        let path = path.split(':').next().unwrap_or(path);
        let leaf = path.rsplit('/').next().unwrap_or(path);
        node = fdt.find(|n| n.name == leaf);
    }
    let node = node
        .filter(|n| n.compatible("arm,pl011") || n.compatible("ns16550a") || n.compatible("ns16550"))
        .or_else(|| fdt.find_compatible("arm,pl011"))
        .or_else(|| fdt.find_compatible("ns16550a"));
    let Some(n) = node else { return };
    let Some((base, _)) = n.reg(0) else { return };
    BASE.store(crate::arch::phys_to_virt(base as usize), Ordering::Relaxed);
    SHIFT.store(n.prop_u32("reg-shift").unwrap_or(0) as usize, Ordering::Relaxed);
    KIND.store(if n.compatible("arm,pl011") { PL011 } else { NS16550 }, Ordering::Relaxed);
}

/// The physical base address and kind of the console UART (for the tty driver).
pub fn uart() -> (usize, bool) {
    (BASE.load(Ordering::Relaxed), KIND.load(Ordering::Relaxed) == PL011)
}

pub fn putc(c: u8) {
    let base = BASE.load(Ordering::Relaxed);
    unsafe {
        match KIND.load(Ordering::Relaxed) {
            PL011 => {
                while core::ptr::read_volatile((base + 0x18) as *const u32) & (1 << 5) != 0 {}
                core::ptr::write_volatile(base as *mut u32, c as u32);
            }
            NS16550 => {
                let sh = SHIFT.load(Ordering::Relaxed);
                while core::ptr::read_volatile((base + (5 << sh)) as *const u8) & 0x20 == 0 {}
                core::ptr::write_volatile(base as *mut u8, c);
            }
            _ => {}
        }
    }
}

pub fn write_bytes(b: &[u8]) {
    for &c in b {
        if c == b'\n' {
            putc(b'\r');
        }
        putc(c);
    }
}

pub struct Console;

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_bytes(s.as_bytes());
        Ok(())
    }
}

static LOCK: crate::sync::SpinLock<()> = crate::sync::SpinLock::new(());

pub fn print(args: fmt::Arguments) {
    // one message at a time (unless a panic interrupted someone holding the lock)
    let g = LOCK.try_lock().or_else(|| {
        for _ in 0..1_000_000 {
            if let Some(g) = LOCK.try_lock() {
                return Some(g);
            }
            core::hint::spin_loop();
        }
        None
    });
    let _ = Console.write_fmt(args);
    drop(g);
}

/// For the lock watchdog: prints a number without taking the print lock.
pub fn write_hex(v: usize) {
    write_bytes(b"  at ");
    for i in (0..16).rev() {
        let d = (v >> (4 * i)) & 0xf;
        putc(if d < 10 { b'0' + d as u8 } else { b'a' + d as u8 - 10 });
    }
    write_bytes(b"\n");
}

#[allow(unused_macros)]
macro_rules! print {
    ($($arg:tt)*) => ($crate::console::print(format_args!($($arg)*)));
}
macro_rules! println {
    () => (print!("\n"));
    ($($arg:tt)*) => ($crate::console::print(format_args!("{}\n", format_args!($($arg)*))));
}

pub fn uart_shift() -> usize {
    SHIFT.load(Ordering::Relaxed)
}
