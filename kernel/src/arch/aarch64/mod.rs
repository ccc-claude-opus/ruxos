//! AArch64 (ARMv8-A, EL1).
core::arch::global_asm!(include_str!("boot.S"), options(raw));

pub const NAME: &str = "aarch64";
/// Start of the linear map: virtual = physical + LM.
pub const LM: usize = 0xffff_8000_0000_0000;

pub fn phys_to_virt(pa: usize) -> usize {
    pa + LM
}
pub fn virt_to_phys(va: usize) -> usize {
    va - LM
}

/// PSCI call through `hvc` or `smc` (the device tree's /psci method).
pub fn psci_call(smc: bool, func: u32, a1: usize, a2: usize, a3: usize) -> isize {
    let mut x0 = func as usize;
    unsafe {
        if smc {
            core::arch::asm!("smc #0", inout("x0") x0, in("x1") a1, in("x2") a2, in("x3") a3, options(nostack));
        } else {
            core::arch::asm!("hvc #0", inout("x0") x0, in("x1") a1, in("x2") a2, in("x3") a3, options(nostack));
        }
    }
    x0 as isize
}

pub fn wait_forever() -> ! {
    loop {
        unsafe { core::arch::asm!("msr daifset, #0xf", "wfi") };
    }
}
