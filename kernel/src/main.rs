//! ruxos: a small kernel with the Linux system call ABI, for AArch64 and riscv64.
#![no_std]
#![no_main]
#![allow(dead_code)]

#[macro_use]
mod console;
mod arch;
mod fdt;
mod power;

use core::panic::PanicInfo;

/// The device tree, as the boot loader passed it (virtual address in the linear map).
pub static mut DTB: usize = 0;

/// The boot CPU arrives here with the MMU on, running at its linked (linear map) address.
#[unsafe(no_mangle)]
pub extern "C" fn kmain(dtb_pa: usize, hart: usize) -> ! {
    let dtb = arch::phys_to_virt(dtb_pa);
    let fdt = unsafe { fdt::Fdt::from_ptr(dtb as *const u8) }.unwrap_or_else(|| power::halt());
    unsafe { DTB = dtb };
    console::init(&fdt);
    println!("ruxos {} ({}), boot cpu {}", env!("CARGO_PKG_VERSION"), arch::NAME, hart);
    if let Some(args) = fdt.chosen_bootargs() {
        println!("command line: {args}");
    }
    let mut mem = 0;
    fdt.memory(|base, size| {
        println!("memory: {:#x}..{:#x} ({} MB)", base, base + size, size >> 20);
        mem += size;
    });
    let mut cpus = 0;
    fdt.for_each_node(|n| {
        if n.prop_str("device_type") == Some("cpu") {
            cpus += 1;
        }
    });
    println!("{} cpus", cpus);
    fdt.for_each_node(|n| {
        if n.compatible("virtio,mmio") {
            if let Some((base, _)) = n.reg(0) {
                let pa = arch::phys_to_virt(base as usize) as *const u32;
                let id = unsafe { core::ptr::read_volatile(pa.add(2)) };
                if id != 0 {
                    println!("virtio-mmio {:#x}: device {}", base, id);
                }
            }
        }
    });
    power::poweroff(&fdt);
}

#[unsafe(no_mangle)]
pub extern "C" fn secondary_main(_cpu: usize, _hart: usize) -> ! {
    power::halt();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("ruxos: panic: {info}");
    power::halt();
}
