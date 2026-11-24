//! AArch64 (ARMv8-A, EL1).
use core::arch::asm;

core::arch::global_asm!(include_str!("boot.S"), options(raw));
core::arch::global_asm!(include_str!("entry.S"), options(raw));

pub mod gic;

pub const NAME: &str = "aarch64";
/// Start of the linear map: virtual = physical + LM.
pub const LM: usize = 0xffff_8000_0000_0000;

unsafe extern "C" {
    pub static _end: u8;
    pub static _start_image: u8;
    static exception_vectors: u8;
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

/// Registers saved on the kernel stack on every exception.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct TrapFrame {
    pub x: [usize; 31],
    pub sp: usize,
    pub pc: usize,
    pub pstate: usize,
}

/// Callee-saved registers of a sleeping kernel thread: x19..x30, sp.
#[repr(C)]
#[derive(Default)]
pub struct Context {
    r: [usize; 13],
}

#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct FpState {
    pub q: [u128; 32],
    pub fpcr: u64,
    pub fpsr: u64,
}
impl Default for FpState {
    fn default() -> Self {
        FpState { q: [0; 32], fpcr: 0, fpsr: 0 }
    }
}

pub fn init_context(ctx: &mut Context, stack_top: usize, f: usize, arg: usize) {
    ctx.r = [0; 13];
    ctx.r[0] = f; // x19
    ctx.r[1] = arg; // x20
    ctx.r[11] = task_trampoline as *const () as usize; // x30
    ctx.r[12] = stack_top;
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

/// Disables IRQs; returns whether they were enabled.
#[inline]
pub fn irq_save() -> bool {
    let daif: usize;
    unsafe { asm!("mrs {}, daif", "msr daifset, #2", out(reg) daif, options(nomem, nostack)) };
    daif & (1 << 7) == 0
}
#[inline]
pub fn irq_restore(enabled: bool) {
    if enabled {
        unsafe { asm!("msr daifclr, #2", options(nomem, nostack)) };
    }
}
#[inline]
pub fn irq_enable() {
    unsafe { asm!("msr daifclr, #2", options(nomem, nostack)) };
}
#[inline]
pub fn irq_disable() {
    unsafe { asm!("msr daifset, #2", options(nomem, nostack)) };
}
pub fn irqs_enabled() -> bool {
    let daif: usize;
    unsafe { asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack)) };
    daif & (1 << 7) == 0
}

/// Sleeps until an interrupt is pending (works with IRQs masked).
pub fn wait_for_interrupt() {
    unsafe { asm!("wfi", options(nomem, nostack)) };
}

pub fn wait_forever() -> ! {
    loop {
        unsafe { asm!("msr daifset, #0xf", "wfi") };
    }
}

/// Per-CPU setup: the per-CPU pointer, exception vectors, FP/SIMD for EL0, EL0 access to the
/// virtual counter (vDSO), the empty user address space.
pub fn cpu_init(percpu: usize) {
    unsafe {
        asm!("msr tpidr_el1, {}", in(reg) percpu);
        asm!("msr vbar_el1, {}", in(reg) &exception_vectors as *const u8 as usize);
        asm!("msr cpacr_el1, {}", in(reg) 3usize << 20);
        asm!("msr cntkctl_el1, {}", in(reg) 2usize); // EL0VCTEN
        asm!("isb");
    }
}

#[inline]
pub fn percpu_ptr() -> usize {
    let p: usize;
    unsafe { asm!("mrs {}, tpidr_el1", out(reg) p, options(nomem, nostack, preserves_flags)) };
    p
}

/// The counter the timer and the vDSO use (CNTVCT_EL0).
#[inline]
pub fn ticks() -> u64 {
    let t: u64;
    unsafe { asm!("isb", "mrs {}, cntvct_el0", out(reg) t, options(nomem, nostack)) };
    t
}

pub fn tick_freq() -> u64 {
    let f: u64;
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) f, options(nomem, nostack)) };
    f
}

/// Arms this CPU's timer for an absolute counter value.
pub fn set_timer(deadline: u64) {
    unsafe {
        asm!("msr cntv_cval_el0, {}", "msr cntv_ctl_el0, {}", in(reg) deadline, in(reg) 1usize, options(nomem, nostack));
    }
}
pub fn stop_timer() {
    unsafe { asm!("msr cntv_ctl_el0, {}", in(reg) 0usize, options(nomem, nostack)) };
}

pub fn timer_init(_fdt: &crate::fdt::Fdt) {}

/// PSCI call through `hvc` or `smc` (the device tree's /psci method).
pub fn psci_call(smc: bool, func: u32, a1: usize, a2: usize, a3: usize) -> isize {
    let mut x0 = func as usize;
    unsafe {
        if smc {
            asm!("smc #0", inout("x0") x0, in("x1") a1, in("x2") a2, in("x3") a3, options(nostack));
        } else {
            asm!("hvc #0", inout("x0") x0, in("x1") a1, in("x2") a2, in("x3") a3, options(nostack));
        }
    }
    x0 as isize
}

/// The hardware ids (MPIDR affinity) of the CPUs in the device tree, in order.
pub fn cpu_hwids(fdt: &crate::fdt::Fdt, out: &mut [usize]) -> usize {
    let mut n = 0;
    fdt.for_each_node(|node| {
        if node.prop_str("device_type") == Some("cpu") && n < out.len() {
            if let Some((r, _)) = node.reg(0) {
                out[n] = r as usize;
                n += 1;
            }
        }
    });
    n
}

pub fn boot_hwid() -> usize {
    let m: usize;
    unsafe { asm!("mrs {}, mpidr_el1", out(reg) m) };
    m & 0xff_00ff_ffff
}

/// Starts a secondary CPU at `secondary_entry` with `record` (a virtual address) in x0.
pub fn start_cpu(fdt: &crate::fdt::Fdt, hwid: usize, record: usize) -> bool {
    let smc = crate::power::psci_smc(fdt);
    let entry = virt_to_phys(secondary_entry as *const () as usize);
    psci_call(smc, 0xc400_0003, hwid, entry, record) == 0
}

/// Interrupt controller: sets up the distributor (boot CPU) and this CPU's interface.
pub fn irq_init(fdt: &crate::fdt::Fdt) {
    gic::init(fdt);
}
pub fn irq_init_cpu() {
    gic::init_cpu();
}
pub fn irq_enable_line(irq: u32) {
    gic::enable(irq);
}
pub fn send_resched_ipi(cpu: usize) {
    gic::send_sgi(cpu, gic::SGI_RESCHED);
}
/// The interrupt number of a device node's i-th interrupt.
pub fn dt_irq(node: &crate::fdt::Node, i: usize) -> Option<u32> {
    // GIC: <type number flags>, type 0 = SPI (32 + n), 1 = PPI (16 + n)
    let kind = node.cell("interrupts", 3 * i)?;
    let n = node.cell("interrupts", 3 * i + 1)?;
    Some(if kind == 0 { 32 + n } else { 16 + n })
}

#[unsafe(no_mangle)]
extern "C" fn trap_irq(_f: &mut TrapFrame) {
    gic::handle_irqs();
}

#[unsafe(no_mangle)]
extern "C" fn trap_el1_sync(f: &mut TrapFrame) {
    let (esr, far): (usize, usize);
    unsafe { asm!("mrs {}, esr_el1", "mrs {}, far_el1", out(reg) esr, out(reg) far) };
    panic!("kernel exception: ESR {:#x} (class {:#x}) FAR {:#x} ELR {:#x}", esr, esr >> 26, far, f.pc);
}

#[unsafe(no_mangle)]
extern "C" fn trap_bad(f: &mut TrapFrame, kind: usize) {
    let esr: usize;
    unsafe { asm!("mrs {}, esr_el1", out(reg) esr) };
    panic!("unexpected exception kind {} ESR {:#x} ELR {:#x}", kind, esr, f.pc);
}

#[unsafe(no_mangle)]
extern "C" fn trap_el0_sync(f: &mut TrapFrame) {
    let (esr, far): (usize, usize);
    unsafe { asm!("mrs {}, esr_el1", "mrs {}, far_el1", out(reg) esr, out(reg) far) };
    irq_enable();
    crate::trap::user_exception(f, esr, far);
}

#[unsafe(no_mangle)]
extern "C" fn trap_el0_irq(f: &mut TrapFrame) {
    gic::handle_irqs();
    crate::trap::return_to_user(f);
}
