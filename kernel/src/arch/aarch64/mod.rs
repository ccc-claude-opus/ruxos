//! AArch64 (ARMv8-A, EL1).
use core::arch::asm;

core::arch::global_asm!(include_str!("boot.S"), options(raw));
core::arch::global_asm!(include_str!("entry.S"), options(raw));
core::arch::global_asm!(include_str!("vdso.S"), options(raw));

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

// ---- user page tables (4 KB granule, 48-bit VA, 4 levels; TTBR0 with 16-bit ASIDs)

pub const PT_LEVELS: usize = 4;
pub const USER_TOP: usize = 1 << 48;
const PTE_ADDR: usize = 0x0000_ffff_ffff_f000;

pub fn pte_table(pa: usize) -> usize {
    pa | 3
}
pub fn pte_valid(pte: usize) -> bool {
    pte & 1 != 0
}
pub fn pte_pa(pte: usize) -> usize {
    pte & PTE_ADDR
}
/// A user page: `r` false = no user access at all (PROT_NONE).
pub fn pte_user(pa: usize, r: bool, w: bool, x: bool) -> usize {
    let mut p = pa | 3 | (1 << 10) | (3 << 8) | (1 << 2) | (1 << 11) | (1 << 53);
    p |= if !r { 0 } else if w { 1 << 6 } else { 3 << 6 };
    if !(r && x) {
        p |= 1 << 54;
    }
    p
}
pub fn pte_user_writable(pte: usize) -> bool {
    (pte >> 6) & 3 == 1
}
pub fn pte_user_readable(pte: usize) -> bool {
    (pte >> 6) & 1 == 1
}

static ASID_BITS: AtomicUsize = AtomicUsize::new(16);
use core::sync::atomic::{AtomicUsize, Ordering};

pub fn asid_bits() -> usize {
    ASID_BITS.load(Ordering::Relaxed)
}

/// Makes the stores to page tables visible to the table walker.
#[inline]
pub fn pt_sync() {
    unsafe { asm!("dsb ishst", "isb", options(nostack)) };
}

pub fn tlb_flush_page(asid: usize, va: usize) {
    unsafe {
        asm!("dsb ishst", "tlbi vale1is, {}", "dsb ish", "isb", in(reg) (asid << 48) | ((va >> 12) & 0xfff_ffff_ffff), options(nostack));
    }
}
pub fn tlb_flush_asid(asid: usize) {
    unsafe { asm!("dsb ishst", "tlbi aside1is, {}", "dsb ish", "isb", in(reg) asid << 48, options(nostack)) };
}
/// A spurious fault on this CPU (the entry is valid now): nothing cached to drop on AArch64.
pub fn tlb_local_page(_asid: usize, _va: usize) {}

pub fn activate_mm(root: usize, asid: usize) {
    unsafe { asm!("msr ttbr0_el1, {}", "isb", in(reg) (asid << 48) | root, options(nostack)) };
}

/// New user root tables need nothing from the kernel on AArch64 (the kernel is in TTBR1).
pub fn init_user_root(_root_va: usize) {}

pub fn read_tls() -> usize {
    let v: usize;
    unsafe { asm!("mrs {}, tpidr_el0", out(reg) v) };
    v
}
pub fn write_tls(v: usize) {
    unsafe { asm!("msr tpidr_el0, {}", "msr tpidrro_el0, xzr", in(reg) v) };
}

/// Instruction cache coherence after the kernel wrote code into a page (ELF loading, COW of an
/// executable page): clean the data cache to the point of unification and invalidate the I-cache.
pub fn sync_icache(va: usize, len: usize) {
    let ctr: usize;
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr) };
    let dline = 4 << ((ctr >> 16) & 0xf);
    let iline = 4 << (ctr & 0xf);
    let mut a = va & !(dline - 1);
    while a < va + len {
        unsafe { asm!("dc cvau, {}", in(reg) a) };
        a += dline;
    }
    unsafe { asm!("dsb ish") };
    let mut a = va & !(iline - 1);
    while a < va + len {
        unsafe { asm!("ic ivau, {}", in(reg) a) };
        a += iline;
    }
    unsafe { asm!("dsb ish", "isb") };
}

// ---- user mode: system calls, new threads, signal frames

pub fn syscall_nr(f: &TrapFrame) -> usize {
    f.x[8]
}
pub fn syscall_args(f: &TrapFrame) -> [usize; 6] {
    [f.x[0], f.x[1], f.x[2], f.x[3], f.x[4], f.x[5]]
}
pub fn syscall_ret(f: &TrapFrame) -> usize {
    f.x[0]
}
pub fn set_syscall_ret(f: &mut TrapFrame, v: usize) {
    f.x[0] = v;
}
/// Back to the `svc` with the original first argument.
pub fn restart_syscall(f: &mut TrapFrame, orig0: usize) {
    f.x[0] = orig0;
    f.pc -= 4;
}
pub fn frame_sp(f: &TrapFrame) -> usize {
    f.sp
}
pub fn set_sp(f: &mut TrapFrame, sp: usize) {
    f.sp = sp;
}
pub fn frame_pc(f: &TrapFrame) -> usize {
    f.pc
}
/// AArch64 keeps the TLS pointer in TPIDR_EL0 (switched with the thread), not in the frame.
pub fn set_frame_tls(_f: &mut TrapFrame, _tls: usize) {}

pub fn new_user_frame(pc: usize, sp: usize) -> TrapFrame {
    TrapFrame { x: [0; 31], sp, pc, pstate: 0 }
}

/// # Safety
/// `f` must be the trap frame at the top of the current kernel stack.
pub unsafe fn jump_to_user(f: *mut TrapFrame) -> ! {
    unsafe { asm!("mov sp, {}", "b ret_to_user", in(reg) f, options(noreturn)) }
}

const UC: usize = 128; // ucontext after the siginfo
const MC: usize = UC + 176; // uc_mcontext (struct sigcontext)
const RESERVED: usize = MC + 288; // sigcontext.__reserved
pub const SIGFRAME_SIZE: usize = RESERVED + 4096;
const FPSIMD_MAGIC: u32 = 0x4650_8001;

/// Builds the Linux `rt_sigframe` (arch/arm64/kernel/signal.c) and points the frame at the handler.
pub fn setup_signal_frame(
    f: &mut TrapFrame,
    info: &crate::task::signal::SigInfo,
    act: &crate::task::signal::SigAction,
    oldmask: u64,
    altstack: (usize, u32, usize),
    stack_top: usize,
    fp: &FpState,
    tramp: usize,
) -> crate::errno::Result<()> {
    use crate::mm::vm::{copy_to_user, write_user};
    let record = (stack_top - 16) & !15;
    let base = (record - SIGFRAME_SIZE) & !15;
    let mut b = alloc::vec![0u8; SIGFRAME_SIZE];
    b[..128].copy_from_slice(&info.to_bytes());
    let put = |b: &mut [u8], off: usize, v: u64| b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    put(&mut b, UC + 16, altstack.0 as u64);
    b[UC + 24..UC + 28].copy_from_slice(&altstack.1.to_le_bytes());
    put(&mut b, UC + 32, altstack.2 as u64);
    put(&mut b, UC + 40, oldmask);
    put(&mut b, MC, info.addr as u64);
    for i in 0..31 {
        put(&mut b, MC + 8 + 8 * i, f.x[i] as u64);
    }
    put(&mut b, MC + 256, f.sp as u64);
    put(&mut b, MC + 264, f.pc as u64);
    put(&mut b, MC + 272, f.pstate as u64);
    b[RESERVED..RESERVED + 4].copy_from_slice(&FPSIMD_MAGIC.to_le_bytes());
    b[RESERVED + 4..RESERVED + 8].copy_from_slice(&528u32.to_le_bytes());
    b[RESERVED + 8..RESERVED + 12].copy_from_slice(&(fp.fpsr as u32).to_le_bytes());
    b[RESERVED + 12..RESERVED + 16].copy_from_slice(&(fp.fpcr as u32).to_le_bytes());
    for i in 0..32 {
        b[RESERVED + 16 + 16 * i..RESERVED + 32 + 16 * i].copy_from_slice(&fp.q[i].to_le_bytes());
    }
    // the terminator record (zeros) follows at RESERVED + 528
    copy_to_user(base, &b)?;
    write_user(record, &[f.x[29], f.x[30]])?;
    f.x[0] = info.signo as usize;
    f.x[1] = base;
    f.x[2] = base + UC;
    f.x[29] = record;
    f.x[30] = tramp;
    f.sp = base;
    f.pc = act.handler;
    f.pstate &= !(1 << 21); // PSTATE.SS
    Ok(())
}

/// Reads the frame back (rt_sigreturn): registers into `f`, FP/SIMD into `fp`. Returns the saved
/// mask and uc_stack (sp, flags, size).
pub fn restore_signal_frame(f: &mut TrapFrame, fp: &mut FpState) -> crate::errno::Result<(u64, (usize, u32, usize))> {
    use crate::mm::vm::copy_from_user;
    let base = f.sp;
    if base & 15 != 0 {
        return Err(crate::errno::EFAULT);
    }
    let mut b = alloc::vec![0u8; RESERVED + 528 + 8];
    copy_from_user(&mut b, base)?;
    let get = |off: usize| u64::from_le_bytes(b[off..off + 8].try_into().unwrap());
    for i in 0..31 {
        f.x[i] = get(MC + 8 + 8 * i) as usize;
    }
    f.sp = get(MC + 256) as usize;
    f.pc = get(MC + 264) as usize;
    // only the condition flags and SSBS may come from user space; always EL0t
    f.pstate = get(MC + 272) as usize & (0xf000_0000 | (1 << 12));
    if u32::from_le_bytes(b[RESERVED..RESERVED + 4].try_into().unwrap()) == FPSIMD_MAGIC {
        fp.fpsr = u32::from_le_bytes(b[RESERVED + 8..RESERVED + 12].try_into().unwrap()) as u64;
        fp.fpcr = u32::from_le_bytes(b[RESERVED + 12..RESERVED + 16].try_into().unwrap()) as u64;
        for i in 0..32 {
            fp.q[i] = u128::from_le_bytes(b[RESERVED + 16 + 16 * i..RESERVED + 32 + 16 * i].try_into().unwrap());
        }
    }
    let ss = (get(UC + 16) as usize, u32::from_le_bytes(b[UC + 24..UC + 28].try_into().unwrap()), get(UC + 32) as usize);
    Ok((get(UC + 40), ss))
}

/// What a user-mode exception means: a system call, a fault (address, access) or a signal.
pub enum UserTrap {
    Syscall,
    PageFault(usize, crate::mm::vm::Access),
    Signal(u32, i32, usize),
}

pub fn decode_user_trap(esr: usize, far: usize, f: &TrapFrame) -> UserTrap {
    use crate::mm::vm::Access;
    use crate::task::signal::*;
    let ec = esr >> 26;
    match ec {
        0x15 => UserTrap::Syscall,
        0x20 => UserTrap::PageFault(far, Access::Exec),
        0x24 => UserTrap::PageFault(far, if esr & (1 << 6) != 0 { Access::Write } else { Access::Read }),
        0x22 => UserTrap::Signal(SIGBUS, 1, f.pc),  // PC alignment: BUS_ADRALN
        0x26 => UserTrap::Signal(SIGBUS, 1, f.sp),  // SP alignment
        0x2c => UserTrap::Signal(SIGFPE, 0, f.pc),
        0x3c => UserTrap::Signal(SIGTRAP, 1, f.pc), // BRK: TRAP_BRKPT
        0x0e => UserTrap::Signal(SIGILL, 1, f.pc),  // illegal execution state
        _ => UserTrap::Signal(SIGILL, 1, f.pc),     // ILL_ILLOPC
    }
}

/// AT_HWCAP from the CPU's ID registers (arch/arm64/include/uapi/asm/hwcap.h).
pub fn hwcap() -> (usize, usize) {
    let (isar0, isar1, pfr0): (usize, usize, usize);
    unsafe {
        asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0);
        asm!("mrs {}, id_aa64isar1_el1", out(reg) isar1);
        asm!("mrs {}, id_aa64pfr0_el1", out(reg) pfr0);
    }
    let field = |r: usize, s: usize| (r >> s) & 0xf;
    let mut h = 0usize;
    if field(pfr0, 16) != 0xf {
        h |= 1 << 0; // FP
        if field(pfr0, 16) == 1 {
            h |= 1 << 9; // FPHP
        }
    }
    if field(pfr0, 20) != 0xf {
        h |= 1 << 1; // ASIMD
        if field(pfr0, 20) == 1 {
            h |= 1 << 10; // ASIMDHP
        }
    }
    h |= 1 << 2; // EVTSTRM is about the timer event stream; harmless to omit? keep off
    h &= !(1 << 2);
    match field(isar0, 4) {
        1 => h |= 1 << 3,              // AES
        2 => h |= (1 << 3) | (1 << 4), // AES + PMULL
        _ => {}
    }
    if field(isar0, 8) != 0 {
        h |= 1 << 5; // SHA1
    }
    if field(isar0, 12) != 0 {
        h |= 1 << 6; // SHA2
        if field(isar0, 12) == 2 {
            h |= 1 << 21; // SHA512
        }
    }
    if field(isar0, 16) != 0 {
        h |= 1 << 7; // CRC32
    }
    if field(isar0, 20) == 2 {
        h |= 1 << 8; // ATOMICS
    }
    if field(isar0, 28) != 0 {
        h |= 1 << 12; // ASIMDRDM
    }
    if field(isar0, 32) != 0 {
        h |= 1 << 17; // SHA3
    }
    if field(isar0, 44) != 0 {
        h |= 1 << 20; // ASIMDDP
    }
    if field(isar0, 48) != 0 {
        h |= 1 << 23; // ASIMDFHM
    }
    if field(isar0, 52) != 0 {
        h |= 1 << 27; // FLAGM
    }
    if field(isar1, 0) != 0 {
        h |= 1 << 16; // DCPOP
    }
    if field(isar1, 12) != 0 {
        h |= 1 << 13; // JSCVT
    }
    if field(isar1, 16) != 0 {
        h |= 1 << 14; // FCMA
    }
    if field(isar1, 20) != 0 {
        h |= 1 << 15; // LRCPC
        if field(isar1, 20) >= 2 {
            h |= 1 << 26; // ILRCPC
        }
    }
    (h, 0)
}

pub const ELF_MACHINE: u16 = 183;
pub const PLATFORM: &[u8] = b"aarch64";
