//! riscv64 (RV64GC, S-mode, Sv39).
core::arch::global_asm!(include_str!("boot.S"), options(raw));

pub const NAME: &str = "riscv64";
/// Start of the linear map: virtual = physical + LM.
pub const LM: usize = 0xffff_ffc0_0000_0000;

pub fn phys_to_virt(pa: usize) -> usize {
    pa + LM
}
pub fn virt_to_phys(va: usize) -> usize {
    va - LM
}

/// SBI call: returns (error, value).
pub fn sbi_call(eid: usize, fid: usize, a0: usize, a1: usize, a2: usize) -> (isize, usize) {
    let (mut e, mut v) = (a0, a1);
    unsafe {
        core::arch::asm!("ecall", inout("a0") e, inout("a1") v, in("a2") a2, in("a6") fid, in("a7") eid, options(nostack));
    }
    (e as isize, v)
}

pub fn wait_forever() -> ! {
    loop {
        unsafe { core::arch::asm!("csrci sstatus, 2", "wfi") };
    }
}
