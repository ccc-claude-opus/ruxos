//! The RISC-V Platform-Level Interrupt Controller (riscv-plic-1.0.0). Each hart's S-mode context is
//! found through `interrupts-extended` (cpu-intc phandle, 9 = supervisor external interrupt).
use crate::arch::phys_to_virt;
use crate::percpu::{self, MAX_CPUS};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};

static BASE: AtomicUsize = AtomicUsize::new(0);
static CTX: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(usize::MAX) }; MAX_CPUS];
static NDEV: AtomicUsize = AtomicUsize::new(0);
/// hart id -> S-mode context, for harts 0..63
static HART_CTX: [AtomicUsize; 64] = [const { AtomicUsize::new(usize::MAX) }; 64];

fn reg(off: usize) -> *mut u32 {
    (BASE.load(Ordering::Relaxed) + off) as *mut u32
}

pub fn init(fdt: &crate::fdt::Fdt) {
    let node = fdt
        .find(|n| n.compatible("riscv,plic0") || n.compatible("sifive,plic-1.0.0"))
        .expect("no PLIC in the device tree");
    BASE.store(phys_to_virt(node.reg(0).unwrap().0 as usize), Ordering::Relaxed);
    NDEV.store(node.prop_u32("riscv,ndev").unwrap_or(127) as usize, Ordering::Relaxed);
    let mut ctx = 0;
    while let (Some(ph), Some(irq)) = (node.cell("interrupts-extended", 2 * ctx), node.cell("interrupts-extended", 2 * ctx + 1)) {
        if irq == 9 {
            // the cpu-intc node's parent is the cpu node, whose reg is the hart id: find it by
            // walking the tree and remembering the last cpu node
            let mut hart = None;
            let mut last_cpu = 0usize;
            fdt.for_each_node(|n| {
                if n.prop_str("device_type") == Some("cpu") {
                    last_cpu = n.reg(0).map_or(0, |r| r.0 as usize);
                }
                if n.prop_u32("phandle") == Some(ph) {
                    hart = Some(last_cpu);
                }
            });
            if let Some(h) = hart {
                if h < 64 {
                    HART_CTX[h].store(ctx, Ordering::Relaxed);
                }
            }
        }
        ctx += 1;
    }
    init_cpu();
}

pub fn init_cpu() {
    let pc = percpu::this();
    let hart = pc.hwid.load(Ordering::Relaxed);
    let ctx = HART_CTX.get(hart).map_or(usize::MAX, |c| c.load(Ordering::Relaxed));
    CTX[pc.id].store(ctx, Ordering::Relaxed);
    if ctx != usize::MAX {
        unsafe { write_volatile(reg(0x20_0000 + 0x1000 * ctx), 0) }; // threshold
    }
}

/// Enables a source on the boot CPU's context.
pub fn enable(irq: u32) {
    let ctx = CTX[0].load(Ordering::Relaxed);
    unsafe {
        write_volatile(reg(4 * irq as usize), 1); // priority
        let en = reg(0x2000 + 0x80 * ctx + 4 * (irq as usize / 32));
        write_volatile(en, read_volatile(en) | (1 << (irq % 32)));
    }
}

pub fn handle_irqs() {
    let ctx = CTX[percpu::this().id].load(Ordering::Relaxed);
    if ctx == usize::MAX {
        return;
    }
    let claim = reg(0x20_0004 + 0x1000 * ctx);
    loop {
        let irq = unsafe { read_volatile(claim) };
        if irq == 0 {
            break;
        }
        crate::irq::dispatch(irq);
        unsafe { write_volatile(claim, irq) };
    }
}
