//! riscv64 (RV64GC, S-mode, Sv39).
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

core::arch::global_asm!(include_str!("boot.S"), options(raw));
core::arch::global_asm!(include_str!("entry.S"), options(raw));

pub mod plic;

pub const NAME: &str = "riscv64";
/// Start of the linear map: virtual = physical + LM.
pub const LM: usize = 0xffff_ffc0_0000_0000;

unsafe extern "C" {
    pub static _end: u8;
    pub static _start_image: u8;
    fn trap_entry();
    fn switch_context(prev: *mut Context, next: *const Context);
    fn task_trampoline();
    fn fp_save(s: *mut FpState);
    fn fp_restore(s: *const FpState);
    fn secondary_entry();
    pub fn ret_to_user() -> !;
}

pub fn phys_to_virt(pa: usize) -> usize {
    pa + LM
}
pub fn virt_to_phys(va: usize) -> usize {
    va - LM
}

/// Registers saved on the kernel stack on every trap: x[0] unused, x[2] = sp.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct TrapFrame {
    pub x: [usize; 32],
    pub pc: usize,
    pub sstatus: usize,
}

/// Callee-saved registers of a sleeping kernel thread: ra, sp, s0..s11.
#[repr(C)]
#[derive(Default)]
pub struct Context {
    r: [usize; 14],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FpState {
    pub f: [u64; 32],
    pub fcsr: u64,
}
impl Default for FpState {
    fn default() -> Self {
        FpState { f: [0; 32], fcsr: 0 }
    }
}

pub fn init_context(ctx: &mut Context, stack_top: usize, f: usize, arg: usize) {
    ctx.r = [0; 14];
    ctx.r[0] = task_trampoline as *const () as usize;
    ctx.r[1] = stack_top;
    ctx.r[2] = f; // s0
    ctx.r[3] = arg; // s1
}

/// # Safety
/// Both contexts must be valid; called with interrupts disabled.
pub unsafe fn switch(prev: *mut Context, next: *const Context) {
    unsafe { switch_context(prev, next) }
}

pub fn save_fp(s: &mut FpState) {
    unsafe { fp_save(s) }
}
pub fn restore_fp(s: &FpState) {
    unsafe { fp_restore(s) }
}

const SIE: usize = 1 << 1;

#[inline]
pub fn irq_save() -> bool {
    let s: usize;
    unsafe { asm!("csrrci {}, sstatus, 2", out(reg) s, options(nomem, nostack)) };
    s & SIE != 0
}
#[inline]
pub fn irq_restore(enabled: bool) {
    if enabled {
        unsafe { asm!("csrsi sstatus, 2", options(nomem, nostack)) };
    }
}
#[inline]
pub fn irq_enable() {
    unsafe { asm!("csrsi sstatus, 2", options(nomem, nostack)) };
}
#[inline]
pub fn irq_disable() {
    unsafe { asm!("csrci sstatus, 2", options(nomem, nostack)) };
}
pub fn irqs_enabled() -> bool {
    let s: usize;
    unsafe { asm!("csrr {}, sstatus", out(reg) s, options(nomem, nostack)) };
    s & SIE != 0
}

pub fn wait_for_interrupt() {
    unsafe { asm!("wfi", options(nomem, nostack)) };
}

pub fn wait_forever() -> ! {
    loop {
        unsafe { asm!("csrci sstatus, 2", "wfi") };
    }
}

/// Per-CPU setup: tp = per-CPU pointer, trap vector, interrupt enables (software, timer,
/// external), FP unit on (state Initial).
pub fn cpu_init(percpu: usize) {
    unsafe {
        asm!("mv tp, {}", in(reg) percpu);
        asm!("csrw sscratch, zero");
        asm!("csrw stvec, {}", in(reg) trap_entry as *const () as usize);
        asm!("csrs sie, {}", in(reg) (1usize << 9) | (1 << 5) | (1 << 1));
        asm!("csrs sstatus, {}", in(reg) 1usize << 13);
    }
    stop_timer();
}

#[inline]
pub fn percpu_ptr() -> usize {
    let p: usize;
    unsafe { asm!("mv {}, tp", out(reg) p, options(nomem, nostack, preserves_flags)) };
    p
}

#[inline]
pub fn ticks() -> u64 {
    let t: u64;
    unsafe { asm!("rdtime {}", out(reg) t, options(nomem, nostack)) };
    t
}

static FREQ: AtomicU64 = AtomicU64::new(10_000_000);
static SSTC: AtomicBool = AtomicBool::new(false);

pub fn tick_freq() -> u64 {
    FREQ.load(Ordering::Relaxed)
}

pub fn timer_init(fdt: &crate::fdt::Fdt) {
    if let Some(cpus) = fdt.find(|n| n.depth == 1 && n.name == "cpus") {
        if let Some(f) = cpus.prop_u32("timebase-frequency") {
            FREQ.store(f as u64, Ordering::Relaxed);
        }
    }
    let mut sstc = false;
    fdt.for_each_node(|n| {
        if n.prop_str("device_type") == Some("cpu") {
            if let Some(p) = n.prop("riscv,isa-extensions") {
                sstc |= p.split(|&b| b == 0).any(|s| s == b"sstc");
            } else if let Some(isa) = n.prop_str("riscv,isa") {
                sstc |= isa.split('_').any(|e| e == "sstc");
            }
        }
    });
    SSTC.store(sstc, Ordering::Relaxed);
}

pub fn set_timer(deadline: u64) {
    if SSTC.load(Ordering::Relaxed) {
        unsafe { asm!("csrw stimecmp, {}", in(reg) deadline) };
    } else {
        sbi_call(0x5449_4d45, 0, deadline as usize, 0, 0);
    }
}
pub fn stop_timer() {
    set_timer(u64::MAX);
}

/// SBI call: returns (error, value).
pub fn sbi_call(eid: usize, fid: usize, a0: usize, a1: usize, a2: usize) -> (isize, usize) {
    let (mut e, mut v) = (a0, a1);
    unsafe {
        asm!("ecall", inout("a0") e, inout("a1") v, in("a2") a2, in("a6") fid, in("a7") eid, options(nostack));
    }
    (e as isize, v)
}

pub fn cpu_hwids(fdt: &crate::fdt::Fdt, out: &mut [usize]) -> usize {
    let mut n = 0;
    fdt.for_each_node(|node| {
        if node.prop_str("device_type") == Some("cpu")
            && node.prop_str("status").is_none_or(|s| s == "okay")
            && n < out.len()
        {
            if let Some((r, _)) = node.reg(0) {
                out[n] = r as usize;
                n += 1;
            }
        }
    });
    n
}

pub fn boot_hwid() -> usize {
    crate::percpu::this().hwid.load(Ordering::Relaxed)
}

pub fn start_cpu(_fdt: &crate::fdt::Fdt, hart: usize, record: usize) -> bool {
    let entry = virt_to_phys(secondary_entry as *const () as usize);
    sbi_call(0x48_534d, 0, hart, entry, record).0 == 0
}

pub fn irq_init(fdt: &crate::fdt::Fdt) {
    plic::init(fdt);
}
pub fn irq_init_cpu() {
    plic::init_cpu();
}
pub fn irq_enable_line(irq: u32) {
    plic::enable(irq);
}

static IPI_PENDING: [AtomicUsize; crate::percpu::MAX_CPUS] = [const { AtomicUsize::new(0) }; crate::percpu::MAX_CPUS];

pub fn send_resched_ipi(cpu: usize) {
    IPI_PENDING[cpu].store(1, Ordering::Release);
    let hart = crate::percpu::cpu(cpu).hwid.load(Ordering::Relaxed);
    sbi_call(0x73_5049, 0, 1, hart, 0);
}

pub fn dt_irq(node: &crate::fdt::Node, i: usize) -> Option<u32> {
    node.cell("interrupts", i)
}

fn handle_interrupt(cause: usize) {
    match cause & 0xff {
        1 => {
            unsafe { asm!("csrc sip, {}", in(reg) 2usize) };
            let me = crate::percpu::this().id;
            if IPI_PENDING[me].swap(0, Ordering::Acquire) != 0 {
                crate::sched::resched_ipi();
            }
        }
        5 => crate::time::timer_interrupt(),
        9 => plic::handle_irqs(),
        _ => {}
    }
}

#[unsafe(no_mangle)]
extern "C" fn trap_kernel(f: &mut TrapFrame) {
    let (cause, tval): (usize, usize);
    unsafe { asm!("csrr {}, scause", "csrr {}, stval", out(reg) cause, out(reg) tval) };
    if cause >> 63 != 0 {
        handle_interrupt(cause);
        return;
    }
    panic!("kernel exception: scause {:#x} stval {:#x} sepc {:#x} ra {:#x} sp {:#x} s0 {:#x}", cause, tval, f.pc, f.x[1], f.x[2], f.x[8]);
}

#[unsafe(no_mangle)]
extern "C" fn trap_user(f: &mut TrapFrame) -> ! {
    let (cause, tval): (usize, usize);
    unsafe { asm!("csrr {}, scause", "csrr {}, stval", out(reg) cause, out(reg) tval) };
    if cause >> 63 != 0 {
        handle_interrupt(cause);
        crate::trap::return_to_user(f);
    } else {
        irq_enable();
        crate::trap::user_exception(f, cause, tval);
    }
    unsafe {
        asm!("mv sp, {}", "j ret_to_user", in(reg) f as *mut TrapFrame, options(noreturn));
    }
}
