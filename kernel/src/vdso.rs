//! The vDSO: an ELF shared object built at boot (code from arch/*/vdso.S), mapped into every
//! process with the vvar page before it. glibc finds `__kernel_clock_gettime` (AArch64,
//! LINUX_2.6.39) or `__vdso_clock_gettime` (riscv64, LINUX_4.15) in it, and the signal return
//! trampoline.
use crate::arch;
use crate::errno::*;
use crate::mm::frame::{self, PAGE_SIZE};
use crate::mm::vm::{Mm, Vma, VmaKind, PROT_EXEC, PROT_READ};
use crate::sync::Once;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

unsafe extern "C" {
    static vdso_start: u8;
    static vdso_end: u8;
    static vdso_clock_gettime: u8;
    static vdso_gettimeofday: u8;
    static vdso_clock_getres: u8;
    static vdso_rt_sigreturn: u8;
}

const CODE_OFF: usize = 0x800;

struct Vdso {
    vvar: usize,
    image: usize,
    sigreturn_off: usize,
}

static VDSO: Once<Vdso> = Once::new();

#[repr(C)]
pub struct VvarData {
    pub seq: AtomicU32,
    pub _pad: u32,
    pub mult: AtomicU64,
    pub realtime_offset: AtomicU64,
}

fn vvar() -> &'static VvarData {
    unsafe { &*(arch::phys_to_virt(VDSO.vvar) as *const VvarData) }
}

/// The kernel's realtime offset changed (clock_settime): publish it to the vDSO.
pub fn update(mult: u64, realtime_offset: i64) {
    let Some(_) = VDSO.get() else { return };
    let v = vvar();
    let s = v.seq.load(Ordering::Relaxed);
    v.seq.store(s.wrapping_add(1), Ordering::Release);
    core::sync::atomic::fence(Ordering::SeqCst);
    v.mult.store(mult, Ordering::Relaxed);
    v.realtime_offset.store(realtime_offset as u64, Ordering::Relaxed);
    core::sync::atomic::fence(Ordering::SeqCst);
    v.seq.store(s.wrapping_add(2), Ordering::Release);
}

fn elf_hash(name: &[u8]) -> u32 {
    let mut h: u32 = 0;
    for &c in name {
        h = (h << 4).wrapping_add(c as u32);
        let g = h & 0xf000_0000;
        if g != 0 {
            h ^= g >> 24;
        }
        h &= !g;
    }
    h
}

pub fn init() {
    let vvar = frame::zeroed_page();
    let image = frame::zeroed_page();
    let img = unsafe { core::slice::from_raw_parts_mut(arch::phys_to_virt(image) as *mut u8, PAGE_SIZE) };
    let start = unsafe { &vdso_start as *const u8 as usize };
    let end = unsafe { &vdso_end as *const u8 as usize };
    assert!(end - start <= PAGE_SIZE - CODE_OFF);
    img[CODE_OFF..CODE_OFF + (end - start)].copy_from_slice(unsafe { core::slice::from_raw_parts(start as *const u8, end - start) });
    let off = |p: *const u8| CODE_OFF + (p as usize - start);
    let (prefix, version): (&str, &[u8]) =
        if cfg!(target_arch = "aarch64") { ("__kernel_", b"LINUX_2.6.39") } else { ("__vdso_", b"LINUX_4.15") };
    let syms: [(&str, usize); 4] = unsafe {
        [
            ("clock_gettime", off(&vdso_clock_gettime)),
            ("gettimeofday", off(&vdso_gettimeofday)),
            ("clock_getres", off(&vdso_clock_getres)),
            ("rt_sigreturn", off(&vdso_rt_sigreturn)),
        ]
    };
    let sigreturn_off = syms[3].1;

    // string table
    let mut strtab: Vec<u8> = alloc::vec![0];
    let mut add_str = |s: &[u8]| -> u32 {
        let o = strtab.len() as u32;
        strtab.extend_from_slice(s);
        strtab.push(0);
        o
    };
    let soname = add_str(b"linux-vdso.so.1");
    let vername = add_str(version);
    let mut names = Vec::new();
    for (n, _) in &syms {
        let mut full = Vec::from(prefix.as_bytes());
        full.extend_from_slice(n.as_bytes());
        names.push((add_str(&full), full));
    }
    let nsym = syms.len() + 1;

    // layout
    let ehdr = 0usize;
    let phdr = 64;
    let dynamic = phdr + 2 * 56; // 0xb0
    let ndyn = 10;
    let hash = dynamic + ndyn * 16;
    let hash_size = 4 * (2 + 1 + nsym);
    let dynsym = (hash + hash_size + 7) & !7;
    let dynstr = dynsym + nsym * 24;
    let versym = (dynstr + strtab.len() + 1) & !1;
    let verdef = (versym + nsym * 2 + 3) & !3;
    let verdef_size = 2 * 28;
    assert!(verdef + verdef_size <= CODE_OFF);

    let w16 = |b: &mut [u8], o: usize, v: u16| b[o..o + 2].copy_from_slice(&v.to_le_bytes());
    let w32 = |b: &mut [u8], o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    let w64 = |b: &mut [u8], o: usize, v: u64| b[o..o + 8].copy_from_slice(&v.to_le_bytes());

    // ELF header
    img[ehdr..ehdr + 16].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    w16(img, 16, 3); // ET_DYN
    w16(img, 18, arch::ELF_MACHINE);
    w32(img, 20, 1);
    w64(img, 24, 0);
    w64(img, 32, phdr as u64);
    w64(img, 40, 0);
    w32(img, 48, if cfg!(target_arch = "riscv64") { 0x5 } else { 0 }); // RVC | float ABI double
    w16(img, 52, 64);
    w16(img, 54, 56);
    w16(img, 56, 2);
    w16(img, 58, 64);
    // PT_LOAD (r-x) and PT_DYNAMIC
    let ph = |img: &mut [u8], i: usize, t: u32, flags: u32, off: usize, size: usize, align: u64| {
        let o = phdr + 56 * i;
        w32(img, o, t);
        w32(img, o + 4, flags);
        w64(img, o + 8, off as u64);
        w64(img, o + 16, off as u64);
        w64(img, o + 24, off as u64);
        w64(img, o + 32, size as u64);
        w64(img, o + 40, size as u64);
        w64(img, o + 48, align);
    };
    ph(img, 0, 1, 5, 0, PAGE_SIZE, PAGE_SIZE as u64);
    ph(img, 1, 2, 4, dynamic, ndyn * 16, 8);
    // dynamic section
    let dyns: [(u64, u64); 10] = [
        (4, hash as u64),                // DT_HASH
        (5, dynstr as u64),              // DT_STRTAB
        (6, dynsym as u64),              // DT_SYMTAB
        (10, strtab.len() as u64),       // DT_STRSZ
        (11, 24),                        // DT_SYMENT
        (14, soname as u64),             // DT_SONAME
        (0x6fff_fff0, versym as u64),    // DT_VERSYM
        (0x6fff_fffc, verdef as u64),    // DT_VERDEF
        (0x6fff_fffd, 2),                // DT_VERDEFNUM
        (0, 0),                          // DT_NULL
    ];
    for (i, (t, v)) in dyns.iter().enumerate() {
        w64(img, dynamic + 16 * i, *t);
        w64(img, dynamic + 16 * i + 8, *v);
    }
    // SysV hash: one bucket chaining every symbol
    w32(img, hash, 1);
    w32(img, hash + 4, nsym as u32);
    w32(img, hash + 8, 1);
    for i in 0..nsym {
        let next = if i == 0 || i + 1 == nsym { 0 } else { i + 1 };
        w32(img, hash + 12 + 4 * i, next as u32);
    }
    // symbols
    for (i, ((name_off, _), (_, value))) in names.iter().zip(syms.iter()).enumerate() {
        let o = dynsym + 24 * (i + 1);
        w32(img, o, *name_off);
        img[o + 4] = 0x12; // STB_GLOBAL | STT_FUNC
        img[o + 5] = 0;
        w16(img, o + 6, 1); // a defined section
        w64(img, o + 8, *value as u64);
        w64(img, o + 16, 64);
    }
    img[dynstr..dynstr + strtab.len()].copy_from_slice(&strtab);
    // symbol versions: every symbol has version 2
    for i in 1..nsym {
        w16(img, versym + 2 * i, 2);
    }
    // version definitions: 1 = the base (soname), 2 = LINUX_x
    let vd = |img: &mut [u8], o: usize, flags: u16, ndx: u16, name: u32, hash: u32, next: u32| {
        w16(img, o, 1);
        w16(img, o + 2, flags);
        w16(img, o + 4, ndx);
        w16(img, o + 6, 1);
        w32(img, o + 8, hash);
        w32(img, o + 12, 20);
        w32(img, o + 16, next);
        w32(img, o + 20, name);
        w32(img, o + 24, 0);
    };
    vd(img, verdef, 1, 1, soname, elf_hash(b"linux-vdso.so.1"), 28);
    vd(img, verdef + 28, 0, 2, vername, elf_hash(version), 0);

    VDSO.set(Vdso { vvar, image, sigreturn_off });
    crate::time::publish();
}

/// Maps vvar + vDSO into `mm`; returns the vDSO's address (AT_SYSINFO_EHDR).
pub fn map(mm: &Arc<Mm>) -> Result<usize> {
    let v = VDSO.get().ok_or(ENOSYS)?;
    let mut inner = mm.inner.lock();
    let at = inner.find_free(2 * PAGE_SIZE).ok_or(ENOMEM)?;
    inner.insert(Vma { start: at, end: at + PAGE_SIZE, prot: PROT_READ, shared: true, file: None, offset: 0, dontfork: false, kind: VmaKind::Vvar });
    inner.insert(Vma { start: at + PAGE_SIZE, end: at + 2 * PAGE_SIZE, prot: PROT_READ | PROT_EXEC, shared: true, file: None, offset: 0, dontfork: false, kind: VmaKind::Vdso });
    for (va, pa, x) in [(at, v.vvar, false), (at + PAGE_SIZE, v.image, true)] {
        frame::get_page(pa);
        *mm.pte(va, true).ok_or(ENOMEM)? = arch::pte_user(pa, true, false, x);
    }
    arch::sync_icache(arch::phys_to_virt(v.image), PAGE_SIZE);
    arch::pt_sync();
    Ok(at + PAGE_SIZE)
}

pub fn sigreturn_addr(mm: &Mm) -> usize {
    let inner = mm.inner.lock();
    let base = inner.vmas.values().find(|v| v.kind == VmaKind::Vdso).map_or(0, |v| v.start);
    base + VDSO.get().map_or(0, |v| v.sigreturn_off)
}
