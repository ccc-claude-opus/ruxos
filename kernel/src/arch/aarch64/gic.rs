//! GICv2 (ARM Generic Interrupt Controller Architecture Specification v2): distributor and CPU
//! interface. SGI 0 is the reschedule IPI; PPI 27 is the EL1 virtual timer.
use crate::arch::phys_to_virt;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};

static GICD: AtomicUsize = AtomicUsize::new(0);
static GICC: AtomicUsize = AtomicUsize::new(0);
pub const SGI_RESCHED: u32 = 0;
pub const VTIMER: u32 = 27;

fn d(off: usize) -> *mut u32 {
    (GICD.load(Ordering::Relaxed) + off) as *mut u32
}
fn c(off: usize) -> *mut u32 {
    (GICC.load(Ordering::Relaxed) + off) as *mut u32
}

pub fn init(fdt: &crate::fdt::Fdt) {
    let node = fdt
        .find(|n| n.compatible("arm,cortex-a15-gic") || n.compatible("arm,gic-400") || n.compatible("arm,cortex-a9-gic"))
        .expect("no GICv2 in the device tree");
    let (dist, _) = node.reg(0).unwrap();
    let (cpu, _) = node.reg(1).unwrap();
    GICD.store(phys_to_virt(dist as usize), Ordering::Relaxed);
    GICC.store(phys_to_virt(cpu as usize), Ordering::Relaxed);
    unsafe {
        write_volatile(d(0x000), 0); // GICD_CTLR off while configuring
        let lines = ((read_volatile(d(0x004)) & 0x1f) as usize + 1) * 32;
        for i in (32..lines).step_by(32) {
            write_volatile(d(0x180 + i / 8), 0xffff_ffff); // ICENABLER: all SPIs off
        }
        for i in (32..lines).step_by(4) {
            write_volatile(d(0x400 + i), 0xa0a0_a0a0); // IPRIORITYR
            write_volatile(d(0x800 + i), 0x0101_0101); // ITARGETSR: CPU 0
        }
        write_volatile(d(0x000), 1);
    }
    init_cpu();
}

pub fn init_cpu() {
    unsafe {
        write_volatile(d(0x180), 0xffff_ffff); // banked SGI/PPI enables off
        for i in (0..32).step_by(4) {
            write_volatile(d(0x400 + i), 0xa0a0_a0a0);
        }
        write_volatile(d(0x100), (1 << SGI_RESCHED) | (1 << VTIMER)); // ISENABLER0
        write_volatile(c(0x004), 0xff); // GICC_PMR
        write_volatile(c(0x000), 1); // GICC_CTLR
    }
}

pub fn enable(irq: u32) {
    unsafe { write_volatile(d(0x100 + (irq as usize / 32) * 4), 1 << (irq % 32)) };
}

/// Routes an SPI to a CPU (by GIC CPU interface number).
pub fn route(irq: u32, cpu: usize) {
    let reg = d(0x800 + (irq as usize & !3));
    let sh = (irq % 4) * 8;
    unsafe {
        let v = read_volatile(reg) & !(0xff << sh);
        write_volatile(reg, v | ((1 << cpu) << sh));
    }
}

pub fn send_sgi(cpu: usize, sgi: u32) {
    unsafe { write_volatile(d(0xf00), ((1 << cpu) << 16) | sgi) };
}

/// Acknowledges and handles every pending interrupt.
pub static COUNTS: [AtomicUsize; 64] = [const { AtomicUsize::new(0) }; 64];
/// per CPU: (last IAR value, its time in ticks)
pub static LAST_IAR: [(AtomicUsize, AtomicUsize); 8] = [const { (AtomicUsize::new(0), AtomicUsize::new(0)) }; 8];

pub fn handle_irqs() {
    loop {
        let iar = unsafe { read_volatile(c(0x00c)) };
        let id = iar & 0x3ff;
        if let Some(l) = LAST_IAR.get(crate::percpu::this().id) {
            l.0.store(iar as usize, Ordering::Relaxed);
            l.1.store(crate::arch::ticks() as usize, Ordering::Relaxed);
        }
        if let Some(c) = COUNTS.get(id as usize) {
            c.fetch_add(1, Ordering::Relaxed);
        }
        if id >= 1020 {
            break;
        }
        match id {
            SGI_RESCHED => crate::sched::resched_ipi(),
            VTIMER => crate::time::timer_interrupt(),
            _ => crate::irq::dispatch(id),
        }
        unsafe { write_volatile(c(0x010), iar) };
    }
}

/// (pending, active, enabled, ICFGR bits, ITARGETSR byte, priority, this CPU's RPR, HPPIR, PMR, CTLR) for debugging.
pub fn state(irq: u32) -> (bool, bool, bool, u32, u32, u32, u32, u32, u32, u32) {
    let r = |base: usize| unsafe { read_volatile(d(base + (irq as usize / 32) * 4)) } >> (irq % 32) & 1 == 1;
    let byte = |base: usize| unsafe { read_volatile(d(base + (irq as usize & !3))) } >> ((irq % 4) * 8) & 0xff;
    let cfg = unsafe { read_volatile(d(0xc00 + (irq as usize / 16) * 4)) } >> ((irq % 16) * 2) & 3;
    unsafe { (r(0x200), r(0x300), r(0x100), cfg, byte(0x800), byte(0x400), read_volatile(c(0x014)), read_volatile(c(0x018)), read_volatile(c(0x004)), read_volatile(c(0x000))) }
}

pub fn dump() {
    let now = crate::arch::ticks() as usize;
    for (i, l) in LAST_IAR.iter().enumerate().take(crate::percpu::ncpus()) {
        println!("gic: cpu {} last iar {} {} ms ago", i, l.0.load(Ordering::Relaxed), crate::time::ticks_to_ns(now.saturating_sub(l.1.load(Ordering::Relaxed)) as u64) / 1_000_000);
    }
    let mut out = alloc::string::String::new();
    for (i, c) in COUNTS.iter().enumerate() {
        let n = c.load(Ordering::Relaxed);
        if n != 0 {
            out += &alloc::format!(" {}:{}", i, n);
        }
    }
    println!("gic: counts{}", out);
}
