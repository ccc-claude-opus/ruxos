//! ruxos: a small kernel with the Linux system call ABI, for AArch64 and riscv64.
#![no_std]
#![no_main]
#![allow(dead_code)]

extern crate alloc;

#[macro_use]
mod console;
mod arch;
mod block;
mod drivers;
mod errno;
mod fdt;
mod fs;
mod futex;
mod irq;
mod locks;
mod mm;
mod net;
mod percpu;
mod power;
mod random;
mod sched;
mod selftest;
mod sync;
mod syscall;
mod task;
mod time;
mod trap;
mod tty;
mod vdso;
mod vfs;

use core::panic::PanicInfo;
use core::sync::atomic::Ordering;

/// uname: the Linux ABI level ruxos implements, and its own version.
pub const RELEASE: &str = concat!("6.12.0-ruxos-", env!("CARGO_PKG_VERSION"));
pub const VERSION: &str = concat!("#1 SMP ruxos ", env!("CARGO_PKG_VERSION"));

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
    arch::timer_init(&fdt);
    time::init();
    time::rtc_init(&fdt);
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

/// The first kernel thread: brings up the rest of the system and becomes init.
extern "C" fn kernel_init(_: usize) {
    if let Some(t) = kernel_param("ruxos.test") {
        selftest::run(t);
        power::poweroff(&FDT);
    }
    let fdt = *FDT;
    random::init(&fdt);
    vfs::dev::init();
    tty::init(&fdt);
    drivers::virtio::init(&fdt);
    vdso::init();

    // the root file system: the initramfs in a tmpfs, else root=
    let (root_mnt, init_path) = if let Some((s, e)) = fdt.initrd().filter(|r| r.1 > r.0) {
        let fs = vfs::tmpfs::TmpFs::new("rootfs", 0o755);
        let m = vfs::Mount::new(vfs::path::DEntry::new_root(fs), 0, "rootfs", "rootfs", "");
        let root = vfs::PathRef { mnt: m.clone(), d: m.root.clone() };
        let data = unsafe { core::slice::from_raw_parts(arch::phys_to_virt(s) as *const u8, e - s) };
        match vfs::cpio::unpack(data, &root) {
            Ok(n) => {
                if kernel_param("quiet").is_none() {
                    println!("ruxos: initramfs: {} entries", n);
                }
            }
            Err(e) => println!("ruxos: initramfs: cannot unpack ({:?})", e),
        }
        mm::frame::free_range(s, e);
        (m, kernel_param("rdinit").unwrap_or("/init"))
    } else {
        match fs::mount_root() {
            Ok(m) => (m, kernel_param("init").unwrap_or("/sbin/init")),
            Err(e) => panic!("cannot mount the root file system ({:?})", e),
        }
    };
    let root = vfs::PathRef { mnt: root_mnt.clone(), d: root_mnt.root.clone() };
    task::make_init(root);
    if kernel_param("ruxos.strace").is_some() {
        syscall::STRACE.store(true, Ordering::Relaxed);
    }

    // fds 0, 1, 2: /dev/console
    let console = syscall::fs::open_path(-100, b"/dev/console", vfs::file::O_RDWR, 0).or_else(|_| {
        let i = vfs::FileSystem::root(&**vfs::dev::DEVTMPFS).lookup(b"console")?;
        Ok::<_, errno::Errno>(vfs::File::with_node(vfs::file::ops_for(&i, vfs::file::O_RDWR)?, vfs::file::O_RDWR, None, Some(i)))
    });
    match console {
        Ok(c) => {
            let th = task::current_thread();
            let mut fds = th.files.lock();
            for _ in 0..3 {
                let _ = fds.alloc(c.clone(), 0, false);
            }
        }
        Err(_) => println!("ruxos: warning: unable to open an initial console"),
    }

    // run init with a trap frame at the top of this thread's kernel stack
    let me = sched::current();
    let fptr = (me.kstack_top() - core::mem::size_of::<arch::TrapFrame>()) as *mut arch::TrapFrame;
    let mut frame = arch::new_user_frame(0, 0);
    let (argv, envp) = init_args(init_path);
    let cwd = task::current_thread().fs.lock().cwd.clone();
    if let Err(e) = task::exec::execve(init_path.as_bytes(), argv, envp, &cwd, &mut frame) {
        panic!("cannot run {} ({:?})", init_path, e);
    }
    unsafe { *fptr = frame };
    task::own().frame = fptr;
    task::enter_user(fptr);
}

/// Like Linux (init/main.c): kernel parameters the kernel does not know go to init, `name=value`
/// ones as environment variables and the others as arguments; names with a '.' are module
/// parameters and are not passed.
fn init_args(init: &str) -> (alloc::vec::Vec<alloc::vec::Vec<u8>>, alloc::vec::Vec<alloc::vec::Vec<u8>>) {
    const KNOWN: &[&str] = &["console", "root", "rootfstype", "rootflags", "rootwait", "rw", "ro", "init", "rdinit", "quiet", "loglevel", "debug", "earlycon", "panic"];
    let mut argv = alloc::vec![init.as_bytes().to_vec()];
    let mut envp = alloc::vec![b"HOME=/".to_vec(), b"TERM=linux".to_vec()];
    let mut after_dashes = false;
    for a in CMDLINE.get().copied().unwrap_or("").split_whitespace() {
        if a == "--" {
            after_dashes = true;
            continue;
        }
        let name = a.split('=').next().unwrap_or(a);
        if !after_dashes && (KNOWN.contains(&name) || name.contains('.')) {
            continue;
        }
        if !after_dashes && a.contains('=') {
            envp.push(a.as_bytes().to_vec());
        } else {
            argv.push(a.as_bytes().to_vec());
        }
    }
    (argv, envp)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    arch::irq_disable();
    println!("ruxos: panic on cpu {}: {info}", percpu::this().id);
    power::halt();
}
