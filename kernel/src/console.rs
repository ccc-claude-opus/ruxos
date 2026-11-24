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

pub fn print(args: fmt::Arguments) {
    let _ = Console.write_fmt(args);
}

#[allow(unused_macros)]
macro_rules! print {
    ($($arg:tt)*) => ($crate::console::print(format_args!($($arg)*)));
}
macro_rules! println {
    () => (print!("\n"));
    ($($arg:tt)*) => ($crate::console::print(format_args!("{}\n", format_args!($($arg)*))));
}
