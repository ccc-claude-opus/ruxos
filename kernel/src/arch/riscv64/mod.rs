//! riscv64 (RV64GC, S-mode, Sv39).
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

core::arch::global_asm!(include_str!("boot.S"), options(raw));
core::arch::global_asm!(include_str!("entry.S"), options(raw));
core::arch::global_asm!(include_str!("vdso.S"), options(raw));

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
        asm!("csrw scounteren, {}", in(reg) 2usize); // user mode may read the time CSR (rdtime, vDSO)
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

// ---- user page tables (Sv39: 3 levels; the kernel half of every root is the boot table's)

pub const PT_LEVELS: usize = 3;
pub const USER_TOP: usize = 1 << 38;
const V: usize = 1;
const R: usize = 2;
const W: usize = 4;
const X: usize = 8;
const U: usize = 16;
const A: usize = 64;
const D: usize = 128;

unsafe extern "C" {
    static boot_root: [usize; 512];
}

pub fn pte_table(pa: usize) -> usize {
    ((pa >> 12) << 10) | V
}
pub fn pte_valid(pte: usize) -> bool {
    pte & V != 0
}
pub fn pte_pa(pte: usize) -> usize {
    ((pte >> 10) & ((1 << 44) - 1)) << 12
}
/// A user page: `r` false = no user access at all (PROT_NONE: no U bit).
pub fn pte_user(pa: usize, r: bool, w: bool, x: bool) -> usize {
    let mut p = ((pa >> 12) << 10) | V | R | A | D;
    if r {
        p |= U;
        if w {
            p |= W;
        }
        if x {
            p |= X;
        }
    }
    p
}
pub fn pte_user_writable(pte: usize) -> bool {
    pte & (U | W) == U | W
}
pub fn pte_user_readable(pte: usize) -> bool {
    pte & U != 0
}

static ASID_BITS: AtomicUsize = AtomicUsize::new(usize::MAX);

/// ASIDLEN: write all ones to satp.ASID and read back (privileged spec 4.1.11).
pub fn asid_bits() -> usize {
    let b = ASID_BITS.load(Ordering::Relaxed);
    if b != usize::MAX {
        return b;
    }
    let old: usize;
    let new: usize;
    unsafe {
        asm!("csrr {0}, satp", out(reg) old);
        asm!("csrw satp, {0}", in(reg) old | (0xffff << 44));
        asm!("csrr {0}, satp", out(reg) new);
        asm!("csrw satp, {0}", "sfence.vma", in(reg) old);
    }
    let bits = ((new >> 44) & 0xffff).count_ones() as usize;
    ASID_BITS.store(bits, Ordering::Relaxed);
    bits
}

pub fn pt_sync() {
    unsafe { asm!("fence rw, rw", options(nostack)) };
}

fn other_harts_mask() -> (usize, usize) {
    (usize::MAX, 0) // all harts (hart_mask_base = 0 with mask all ones)
}

pub fn tlb_flush_page(asid: usize, va: usize) {
    unsafe { asm!("sfence.vma {}, {}", in(reg) va, in(reg) asid, options(nostack)) };
    if crate::percpu::ncpus() > 1 {
        let (m, b) = other_harts_mask();
        sbi_call6(0x5246_4e43, 2, m, b, va & !0xfff, 4096, asid);
    }
}
pub fn tlb_flush_asid(asid: usize) {
    unsafe { asm!("sfence.vma zero, {}", in(reg) asid, options(nostack)) };
    if crate::percpu::ncpus() > 1 {
        let (m, b) = other_harts_mask();
        sbi_call6(0x5246_4e43, 2, m, b, 0, usize::MAX, asid);
    }
}
/// A spurious fault: drop this hart's cached (invalid) translation.
pub fn tlb_local_page(asid: usize, va: usize) {
    unsafe { asm!("sfence.vma {}, {}", in(reg) va, in(reg) asid, options(nostack)) };
}

fn sbi_call6(eid: usize, fid: usize, a0: usize, a1: usize, a2: usize, a3: usize, a4: usize) {
    unsafe {
        asm!("ecall", inout("a0") a0 => _, inout("a1") a1 => _, in("a2") a2, in("a3") a3, in("a4") a4,
             in("a6") fid, in("a7") eid, options(nostack));
    }
}

pub fn activate_mm(root: usize, asid: usize) {
    let satp = (8usize << 60) | (asid << 44) | (root >> 12);
    unsafe { asm!("csrw satp, {}", in(reg) satp, options(nostack)) };
    if asid_bits() == 0 {
        unsafe { asm!("sfence.vma", options(nostack)) };
    }
}

/// The kernel half (entries 256..511) of a new user root table.
pub fn init_user_root(root_va: usize) {
    let r = unsafe { core::slice::from_raw_parts_mut(root_va as *mut usize, 512) };
    let boot = unsafe { &*(&raw const boot_root) };
    r[256..].copy_from_slice(&boot[256..]);
}

pub fn read_tls() -> usize {
    0 // tp is part of the trap frame
}
pub fn write_tls(_v: usize) {}

pub fn sync_icache(_va: usize, _len: usize) {
    unsafe { asm!("fence.i", options(nostack)) };
    if crate::percpu::ncpus() > 1 {
        sbi_call6(0x5246_4e43, 0, usize::MAX, 0, 0, 0, 0); // remote fence.i
    }
}

// ---- user mode: system calls, new threads, signal frames

pub fn syscall_nr(f: &TrapFrame) -> usize {
    f.x[17]
}
pub fn syscall_args(f: &TrapFrame) -> [usize; 6] {
    [f.x[10], f.x[11], f.x[12], f.x[13], f.x[14], f.x[15]]
}
pub fn syscall_ret(f: &TrapFrame) -> usize {
    f.x[10]
}
pub fn set_syscall_ret(f: &mut TrapFrame, v: usize) {
    f.x[10] = v;
}
/// Back to the `ecall` (pc was advanced past it) with the original first argument.
pub fn restart_syscall(f: &mut TrapFrame, orig0: usize) {
    f.x[10] = orig0;
    f.pc -= 4;
}
pub fn frame_sp(f: &TrapFrame) -> usize {
    f.x[2]
}
pub fn set_sp(f: &mut TrapFrame, sp: usize) {
    f.x[2] = sp;
}
pub fn frame_pc(f: &TrapFrame) -> usize {
    f.pc
}
pub fn set_frame_tls(f: &mut TrapFrame, tls: usize) {
    f.x[4] = tls;
}

pub fn new_user_frame(pc: usize, sp: usize) -> TrapFrame {
    let mut f = TrapFrame { x: [0; 32], pc, sstatus: (1 << 5) | (1 << 13) };
    f.x[2] = sp;
    f
}

/// # Safety
/// `f` must be the trap frame at the top of the current kernel stack.
pub unsafe fn jump_to_user(f: *mut TrapFrame) -> ! {
    unsafe { asm!("mv sp, {}", "j ret_to_user", in(reg) f, options(noreturn)) }
}

const UC: usize = 128;
const MC: usize = UC + 176;
pub const SIGFRAME_SIZE: usize = MC + 256 + 528;

/// Builds the Linux `rt_sigframe` (arch/riscv/kernel/signal.c) and points the frame at the handler.
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
    use crate::mm::vm::copy_to_user;
    let base = (stack_top - SIGFRAME_SIZE) & !15;
    let mut b = alloc::vec![0u8; SIGFRAME_SIZE];
    b[..128].copy_from_slice(&info.to_bytes());
    let put = |b: &mut [u8], off: usize, v: u64| b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    put(&mut b, UC + 16, altstack.0 as u64);
    b[UC + 24..UC + 28].copy_from_slice(&altstack.1.to_le_bytes());
    put(&mut b, UC + 32, altstack.2 as u64);
    put(&mut b, UC + 40, oldmask);
    put(&mut b, MC, f.pc as u64);
    for i in 1..32 {
        put(&mut b, MC + 8 * i, f.x[i] as u64);
    }
    for i in 0..32 {
        put(&mut b, MC + 256 + 8 * i, fp.f[i]);
    }
    b[MC + 512..MC + 516].copy_from_slice(&(fp.fcsr as u32).to_le_bytes());
    copy_to_user(base, &b)?;
    f.x[10] = info.signo as usize;
    f.x[11] = base;
    f.x[12] = base + UC;
    f.x[1] = tramp;
    f.x[2] = base;
    f.pc = act.handler;
    Ok(())
}

pub fn restore_signal_frame(f: &mut TrapFrame, fp: &mut FpState) -> crate::errno::Result<(u64, (usize, u32, usize))> {
    use crate::mm::vm::copy_from_user;
    let base = f.x[2];
    let mut b = alloc::vec![0u8; SIGFRAME_SIZE];
    copy_from_user(&mut b, base)?;
    let get = |off: usize| u64::from_le_bytes(b[off..off + 8].try_into().unwrap());
    f.pc = get(MC) as usize;
    for i in 1..32 {
        f.x[i] = get(MC + 8 * i) as usize;
    }
    for i in 0..32 {
        fp.f[i] = get(MC + 256 + 8 * i);
    }
    fp.fcsr = u32::from_le_bytes(b[MC + 512..MC + 516].try_into().unwrap()) as u64;
    let ss = (get(UC + 16) as usize, u32::from_le_bytes(b[UC + 24..UC + 28].try_into().unwrap()), get(UC + 32) as usize);
    Ok((get(UC + 40), ss))
}

pub enum UserTrap {
    Syscall,
    PageFault(usize, crate::mm::vm::Access),
    Signal(u32, i32, usize),
}

pub fn decode_user_trap(cause: usize, tval: usize, f: &TrapFrame) -> UserTrap {
    use crate::mm::vm::Access;
    use crate::task::signal::*;
    match cause {
        8 => UserTrap::Syscall,
        12 => UserTrap::PageFault(tval, Access::Exec),
        13 => UserTrap::PageFault(tval, Access::Read),
        15 => UserTrap::PageFault(tval, Access::Write),
        1 | 5 | 7 => UserTrap::Signal(SIGSEGV, 2, tval),
        0 | 4 | 6 => UserTrap::Signal(SIGBUS, 1, tval),
        3 => UserTrap::Signal(SIGTRAP, 1, f.pc),
        _ => UserTrap::Signal(SIGILL, 1, f.pc),
    }
}

/// AT_HWCAP: one bit per single-letter extension (arch/riscv/include/uapi/asm/hwcap.h).
pub fn hwcap() -> (usize, usize) {
    let mut h = 0;
    for c in b"imafdc" {
        h |= 1 << (c - b'a');
    }
    (h, 0)
}

pub const ELF_MACHINE: u16 = 243;
pub const PLATFORM: &[u8] = b"riscv64";
