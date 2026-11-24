//! ruxos: a small kernel with the Linux system call ABI, for AArch64 and riscv64.
#![no_std]
#![no_main]
#![allow(dead_code)]

extern crate alloc;

#[macro_use]
mod console;
mod arch;
mod fdt;
mod irq;
mod mm;
mod percpu;
mod power;
mod sched;
mod selftest;
mod sync;
mod task;
mod time;
mod trap;

use core::panic::PanicInfo;
use core::sync::atomic::Ordering;

/// The device tree (virtual address in the linear map).
pub static FDT: sync::Once<fdt::Fdt<'static>> = sync::Once::new();
pub static CMDLINE: sync::Once<&'static str> = sync::Once::new();

/// A `name=value` (or bare `name`) parameter of the kernel command line.
pub fn kernel_param(name: &str) -> Option<&'static str> {
    CMDLINE.get()?.split_whitespace().find_map(|a| match a.split_once('=') {
        Some((k, v)) if k == name => Some(v),
        None if a == name => Some(""),
        _ => None,
    })
}

/// The boot CPU arrives here with the MMU on, running at its linked (linear map) address.
#[unsafe(no_mangle)]
pub extern "C" fn kmain(dtb_pa: usize, hart: usize) -> ! {
    let fdt = unsafe { fdt::Fdt::from_ptr(arch::phys_to_virt(dtb_pa) as *const u8) }.unwrap_or_else(|| power::halt());
    FDT.set(fdt);
    console::init(&fdt);
    CMDLINE.set(fdt.chosen_bootargs().unwrap_or(""));
    let quiet = kernel_param("quiet").is_some();

    let mut ram = (0, 0);
    fdt.memory(|b, s| {
        if s > ram.1 {
            ram = (b, s);
        }
    });
    let mut reserved = [(0usize, 0usize); 4];
    reserved[0] = (dtb_pa, dtb_pa + fdt.total_size());
    if let Some(r) = fdt.initrd() {
        reserved[1] = r;
    }
    mm::frame::init(ram, &reserved);

    let hwid = if cfg!(target_arch = "aarch64") { arch::boot_hwid() } else { hart };
    percpu::init(0, hwid);
    let pc0 = percpu::cpu(0);
    sched::init_cpu_idle(pc0, false);
    pc0.online.store(true, Ordering::Relaxed);
    time::init();
    time::rtc_init(&fdt);
    arch::timer_init(&fdt);
    arch::irq_init(&fdt);
    if !quiet {
        println!(
            "ruxos {} ({}): {} MB free, timer {} Hz",
            env!("CARGO_PKG_VERSION"),
            arch::NAME,
            mm::frame::free_count() * 4 / 1024,
            time::freq()
        );
    }
    start_secondaries(&fdt, hwid);
    if !quiet {
        println!("ruxos: {} cpus online", percpu::ncpus());
    }
    sched::spawn_kernel(kernel_init, 0);
    sched::idle_loop();
}

fn start_secondaries(fdt: &fdt::Fdt, boot_hwid: usize) {
    let mut ids = [0usize; percpu::MAX_CPUS];
    let n = arch::cpu_hwids(fdt, &mut ids);
    let mut next = 1;
    for &id in &ids[..n] {
        if id == boot_hwid || next >= percpu::MAX_CPUS {
            continue;
        }
        let pc = percpu::cpu(next);
        let idle = sched::init_cpu_idle(pc, true);
        // { stack_top, cpu index }, read by the secondary entry code with the MMU on
        let record = alloc::boxed::Box::leak(alloc::boxed::Box::new([idle.kstack_top(), next]));
        pc.hwid.store(id, Ordering::Relaxed);
        if !arch::start_cpu(fdt, id, record.as_ptr() as usize) {
            println!("ruxos: cpu {:#x} did not start", id);
            continue;
        }
        let t0 = arch::ticks();
        while !pc.online.load(Ordering::Acquire) {
            if arch::ticks() - t0 > time::freq() {
                println!("ruxos: cpu {:#x} did not come online", id);
                break;
            }
            core::hint::spin_loop();
        }
        if pc.online.load(Ordering::Acquire) {
            next += 1;
            percpu::NCPUS.store(next, Ordering::Release);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn secondary_main(cpu: usize, _hart: usize) -> ! {
    let pc = percpu::cpu(cpu);
    percpu::init(cpu, pc.hwid.load(Ordering::Relaxed));
    arch::irq_init_cpu();
    pc.online.store(true, Ordering::Release);
    sched::idle_loop();
}

/// The first kernel thread: brings up the rest of the system.
extern "C" fn kernel_init(_: usize) {
    if let Some(t) = kernel_param("ruxos.test") {
        selftest::run(t);
        power::poweroff(&FDT);
    }
    println!("ruxos: nothing to run yet");
    power::poweroff(&FDT);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    arch::irq_disable();
    println!("ruxos: panic on cpu {}: {info}", percpu::this().id);
    power::halt();
}
