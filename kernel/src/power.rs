//! Powering off and halting: PSCI on AArch64, SBI SRST on riscv64 (falling back to the syscon
//! "sifive,test" device the device tree describes).
use crate::fdt::Fdt;

pub fn halt() -> ! {
    crate::arch::wait_forever()
}

#[cfg(target_arch = "aarch64")]
pub fn poweroff(fdt: &Fdt) -> ! {
    crate::arch::psci_call(psci_smc(fdt), 0x8400_0008, 0, 0, 0); // SYSTEM_OFF
    halt()
}

#[cfg(target_arch = "aarch64")]
pub fn reboot(fdt: &Fdt) -> ! {
    crate::arch::psci_call(psci_smc(fdt), 0x8400_0009, 0, 0, 0); // SYSTEM_RESET
    halt()
}

/// Whether PSCI is called with `smc` (else `hvc`).
#[cfg(target_arch = "aarch64")]
pub fn psci_smc(fdt: &Fdt) -> bool {
    let mut smc = false;
    fdt.for_each_node(|n| {
        if n.compatible("arm,psci") || n.compatible("arm,psci-0.2") || n.compatible("arm,psci-1.0") {
            smc = n.prop_str("method") == Some("smc");
        }
    });
    smc
}

#[cfg(target_arch = "riscv64")]
pub fn poweroff(_fdt: &Fdt) -> ! {
    crate::arch::sbi_call(0x5352_5354, 0, 0, 0, 0); // SRST: shutdown
    halt()
}

#[cfg(target_arch = "riscv64")]
pub fn reboot(_fdt: &Fdt) -> ! {
    crate::arch::sbi_call(0x5352_5354, 0, 1, 0, 0); // SRST: cold reboot
    halt()
}
