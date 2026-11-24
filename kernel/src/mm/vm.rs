//! User address spaces: page tables, VMAs (virtual memory areas), demand paging, copy-on-write,
//! and access to user memory from the kernel (through the linear map, after faulting pages in).
use super::frame::{self, PAGE_SIZE};
use crate::arch::{self, phys_to_virt};
use crate::errno::*;
use crate::sched::Mutex;
use crate::vfs::Inode;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

pub const PROT_READ: u32 = 1;
pub const PROT_WRITE: u32 = 2;
pub const PROT_EXEC: u32 = 4;
pub const MAP_SHARED: u32 = 1;
pub const MAP_PRIVATE: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    Read,
    Write,
    Exec,
}

#[derive(Clone)]
pub struct Vma {
    pub start: usize,
    pub end: usize,
    pub prot: u32,
    pub shared: bool,
    /// File (or shared anonymous memory) behind the mapping; `offset` is the file offset of `start`.
    pub file: Option<Arc<dyn Inode>>,
    pub offset: u64,
    pub dontfork: bool,
    pub kind: VmaKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VmaKind {
    Normal,
    Heap,
    Stack,
    Vdso,
    Vvar,
}

pub struct MmInner {
    pub vmas: BTreeMap<usize, Vma>,
    pub brk_start: usize,
    pub brk: usize,
    pub mmap_top: usize,
    pub stack_top: usize,
    pub arg_start: usize,
    pub arg_end: usize,
    pub env_start: usize,
    pub env_end: usize,
    pub auxv: Vec<usize>,
    pub exe: Option<crate::vfs::PathRef>,
}

pub struct Mm {
    pub id: u64,
    pub root: usize,
    pub asid: usize,
    pub inner: Mutex<MmInner>,
    /// CPUs that have this address space loaded (or had: their TLB may hold its entries).
    pub cpus: AtomicUsize,
}

static NEXT_ASID: AtomicUsize = AtomicUsize::new(1);
static NEXT_MM: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

fn alloc_asid() -> usize {
    let bits = arch::asid_bits();
    if bits == 0 {
        return 0;
    }
    let max = (1usize << bits.min(16)) - 1;
    let a = NEXT_ASID.fetch_add(1, Ordering::Relaxed);
    let asid = (a - 1) % max + 1;
    if a > max {
        // reusing an ASID: drop whatever an old address space left in the TLBs
        arch::tlb_flush_asid(asid);
    }
    asid
}

fn table(pa: usize) -> &'static mut [usize; 512] {
    unsafe { &mut *(phys_to_virt(pa) as *mut [usize; 512]) }
}

fn index(va: usize, level: usize) -> usize {
    // level 0 = root
    (va >> (12 + 9 * (arch::PT_LEVELS - 1 - level))) & 511
}

impl Mm {
    pub fn new() -> Arc<Mm> {
        let root = frame::zeroed_page();
        arch::init_user_root(phys_to_virt(root));
        Arc::new(Mm {
            id: NEXT_MM.fetch_add(1, Ordering::Relaxed),
            root,
            asid: alloc_asid(),
            inner: Mutex::new(MmInner {
                vmas: BTreeMap::new(),
                brk_start: 0,
                brk: 0,
                mmap_top: 0,
                stack_top: 0,
                arg_start: 0,
                arg_end: 0,
                env_start: 0,
                env_end: 0,
                auxv: Vec::new(),
                exe: None,
            }),
            cpus: AtomicUsize::new(0),
        })
    }

    /// The leaf entry for `va`, creating intermediate tables when `create`.
    pub fn pte(&self, va: usize, create: bool) -> Option<&'static mut usize> {
        let mut t = self.root;
        for level in 0..arch::PT_LEVELS - 1 {
            let e = &mut table(t)[index(va, level)];
            if !arch::pte_valid(*e) {
                if !create {
                    return None;
                }
                *e = arch::pte_table(frame::zeroed_page());
            }
            t = arch::pte_pa(*e);
        }
        Some(&mut table(t)[index(va, arch::PT_LEVELS - 1)])
    }

    fn flush(&self, va: usize) {
        arch::tlb_flush_page(self.asid, va);
    }

    /// Removes the pages of [start, end) from the page table (the VMAs stay).
    pub fn unmap_pages(&self, start: usize, end: usize) {
        let mut va = start;
        let mut any = false;
        while va < end {
            // skip missing tables quickly
            let mut t = self.root;
            let mut skip = 0;
            for level in 0..arch::PT_LEVELS - 1 {
                let e = table(t)[index(va, level)];
                if !arch::pte_valid(e) {
                    skip = 1usize << (12 + 9 * (arch::PT_LEVELS - 1 - level));
                    break;
                }
                t = arch::pte_pa(e);
            }
            if skip != 0 {
                va = (va & !(skip - 1)) + skip;
                continue;
            }
            let e = &mut table(t)[index(va, arch::PT_LEVELS - 1)];
            if arch::pte_valid(*e) {
                let pa = arch::pte_pa(*e);
                *e = 0;
                if (end - start) / PAGE_SIZE <= 32 {
                    self.flush(va);
                } else {
                    any = true;
                }
                frame::put_page(pa);
            }
            va += PAGE_SIZE;
        }
        if any {
            arch::tlb_flush_asid(self.asid);
        }
    }

    /// Recomputes the protection of the present pages of [start, end).
    fn reprotect(&self, start: usize, end: usize, prot: u32, shared: bool) {
        let mut va = start;
        let mut changed = 0;
        while va < end {
            if let Some(e) = self.pte(va, false) {
                if arch::pte_valid(*e) {
                    let pa = arch::pte_pa(*e);
                    let was_w = arch::pte_user_writable(*e);
                    // write access stays only where it is already granted (COW and dirty tracking)
                    let w = prot & PROT_WRITE != 0 && (was_w || (!shared && frame::page_refs(pa) == 1));
                    let n = arch::pte_user(pa, prot != 0, w, prot & PROT_EXEC != 0);
                    if n != *e {
                        *e = n;
                        changed += 1;
                        if changed <= 32 {
                            self.flush(va);
                        }
                    }
                    if prot & PROT_EXEC != 0 {
                        arch::sync_icache(phys_to_virt(pa), PAGE_SIZE);
                    }
                }
            }
            va += PAGE_SIZE;
        }
        if changed > 32 {
            arch::tlb_flush_asid(self.asid);
        }
    }

    /// Handles a fault at `va`. Err(EFAULT) = no mapping (SIGSEGV), Err(EACCES) = not permitted
    /// (SIGSEGV), Err(EIO)/Err(ENXIO) = beyond the end of a file (SIGBUS).
    pub fn fault(&self, va: usize, access: Access) -> Result<()> {
        let inner = self.inner.lock();
        let page_va = va & !(PAGE_SIZE - 1);
        let vma = find_vma(&inner.vmas, va).ok_or(EFAULT)?;
        let ok = match access {
            Access::Read => vma.prot & (PROT_READ | PROT_WRITE | PROT_EXEC) != 0,
            Access::Write => vma.prot & PROT_WRITE != 0,
            Access::Exec => vma.prot & PROT_EXEC != 0,
        };
        if !ok {
            return Err(EACCES);
        }
        let exec = vma.prot & PROT_EXEC != 0;
        let writable = vma.prot & PROT_WRITE != 0;
        let e = self.pte(page_va, true).ok_or(ENOMEM)?;
        if arch::pte_valid(*e) && arch::pte_user_readable(*e) {
            if access != Access::Write || arch::pte_user_writable(*e) {
                // spurious (another CPU just fixed it, or a stale TLB entry)
                arch::tlb_local_page(self.asid, page_va);
                return Ok(());
            }
            // write to a read-only page of a writable mapping
            let pa = arch::pte_pa(*e);
            if vma.shared {
                if let Some(f) = &vma.file {
                    f.page_mkwrite(file_index(vma, page_va));
                }
                *e = arch::pte_user(pa, true, true, exec);
            } else if frame::page_refs(pa) == 1 {
                *e = arch::pte_user(pa, true, true, exec);
            } else {
                let np = frame::alloc_pages(0).ok_or(ENOMEM)?;
                unsafe { core::ptr::copy_nonoverlapping(phys_to_virt(pa) as *const u8, phys_to_virt(np) as *mut u8, PAGE_SIZE) };
                *e = arch::pte_user(np, true, true, exec);
                frame::put_page(pa);
                if exec {
                    arch::sync_icache(phys_to_virt(np), PAGE_SIZE);
                }
            }
            self.flush(page_va);
            return Ok(());
        }
        if arch::pte_valid(*e) {
            // PROT_NONE page whose mapping allows access now
            let pa = arch::pte_pa(*e);
            let w = writable && (vma.shared || frame::page_refs(pa) == 1) && access == Access::Write;
            *e = arch::pte_user(pa, true, w, exec);
            self.flush(page_va);
            return Ok(());
        }
        let vma = vma.clone();
        let pa = match &vma.file {
            None => {
                let pa = frame::try_zeroed_page().ok_or(ENOMEM)?;
                *e = arch::pte_user(pa, true, writable, exec);
                pa
            }
            Some(f) => {
                let idx = file_index(&vma, page_va);
                let cache = f.get_page(idx)?; // a reference for us
                if vma.shared {
                    if access == Access::Write {
                        f.page_mkwrite(idx);
                    }
                    *e = arch::pte_user(cache, true, writable && access == Access::Write, exec);
                    cache
                } else if access == Access::Write {
                    let np = frame::alloc_pages(0).ok_or(ENOMEM)?;
                    unsafe { core::ptr::copy_nonoverlapping(phys_to_virt(cache) as *const u8, phys_to_virt(np) as *mut u8, PAGE_SIZE) };
                    frame::put_page(cache);
                    *e = arch::pte_user(np, true, true, exec);
                    np
                } else {
                    *e = arch::pte_user(cache, true, false, exec);
                    cache
                }
            }
        };
        if exec {
            arch::sync_icache(phys_to_virt(pa), PAGE_SIZE);
        }
        arch::pt_sync();
        arch::tlb_local_page(self.asid, page_va);
        drop(inner);
        Ok(())
    }

    /// The physical page behind user address `va`, faulted in for `access`, with a reference the
    /// caller must drop (`frame::put_page`).
    pub fn get_user_page(&self, va: usize, access: Access) -> Result<usize> {
        if va >= arch::USER_TOP {
            return Err(EFAULT);
        }
        for _ in 0..3 {
            {
                let _g = self.inner.lock();
                if let Some(e) = self.pte(va, false) {
                    let ok = arch::pte_valid(*e)
                        && arch::pte_user_readable(*e)
                        && (access != Access::Write || arch::pte_user_writable(*e));
                    if ok {
                        let pa = arch::pte_pa(*e);
                        frame::get_page(pa);
                        return Ok(pa);
                    }
                }
            }
            self.fault(va, access).map_err(|e| if e == ENOMEM { ENOMEM } else { EFAULT })?;
        }
        Err(EFAULT)
    }

    /// Copies a new address space for fork: shared mappings share pages, private ones become
    /// copy-on-write in both.
    pub fn fork(&self) -> Result<Arc<Mm>> {
        let child = Mm::new();
        let inner = self.inner.lock();
        let mut ci = child.inner.lock();
        let mut flush = false;
        for (k, v) in inner.vmas.iter() {
            if v.dontfork {
                continue;
            }
            ci.vmas.insert(*k, v.clone());
            let mut va = v.start;
            while va < v.end {
                if let Some(e) = self.pte(va, false) {
                    if arch::pte_valid(*e) {
                        let pa = arch::pte_pa(*e);
                        frame::get_page(pa);
                        let ne = if !v.shared && arch::pte_user_writable(*e) {
                            *e = arch::pte_user(pa, true, false, v.prot & PROT_EXEC != 0);
                            flush = true;
                            *e
                        } else {
                            *e
                        };
                        *child.pte(va, true).ok_or(ENOMEM)? = ne;
                    }
                } else {
                    // no table for this range: skip to the next leaf table
                    va = (va | ((PAGE_SIZE << 9) - 1)) + 1 - PAGE_SIZE;
                }
                va += PAGE_SIZE;
            }
        }
        ci.brk_start = inner.brk_start;
        ci.brk = inner.brk;
        ci.mmap_top = inner.mmap_top;
        ci.stack_top = inner.stack_top;
        ci.arg_start = inner.arg_start;
        ci.arg_end = inner.arg_end;
        ci.env_start = inner.env_start;
        ci.env_end = inner.env_end;
        ci.auxv = inner.auxv.clone();
        ci.exe = inner.exe.clone();
        if flush {
            arch::tlb_flush_asid(self.asid);
        }
        drop(ci);
        Ok(child)
    }
}

pub fn file_index(vma: &Vma, page_va: usize) -> u64 {
    (vma.offset + (page_va - vma.start) as u64) / PAGE_SIZE as u64
}

pub fn find_vma(vmas: &BTreeMap<usize, Vma>, va: usize) -> Option<&Vma> {
    vmas.range(..=va).next_back().map(|(_, v)| v).filter(|v| va < v.end)
}

impl Drop for Mm {
    fn drop(&mut self) {
        // free every page and table
        fn free(t: usize, level: usize) {
            let tb = table(t);
            let last = level == arch::PT_LEVELS - 1;
            let n = if level == 0 && cfg!(target_arch = "riscv64") { 256 } else { 512 };
            for e in tb.iter().take(n) {
                if arch::pte_valid(*e) {
                    if last {
                        frame::put_page(arch::pte_pa(*e));
                    } else {
                        free(arch::pte_pa(*e), level + 1);
                    }
                }
            }
            frame::free_pages(t, 0);
        }
        free(self.root, 0);
        if self.asid != 0 {
            arch::tlb_flush_asid(self.asid);
        }
    }
}

impl MmInner {
    /// Splits VMAs so that `at` is a boundary.
    fn split(&mut self, at: usize) {
        let Some((&s, v)) = self.vmas.range(..at).next_back() else { return };
        if v.end <= at || s == at {
            return;
        }
        let mut hi = v.clone();
        hi.offset += (at - v.start) as u64;
        hi.start = at;
        self.vmas.get_mut(&s).unwrap().end = at;
        self.vmas.insert(at, hi);
    }

    /// Removes the VMAs (and pages) in [start, end).
    pub fn unmap(&mut self, mm: &Mm, start: usize, end: usize) {
        self.split(start);
        self.split(end);
        let keys: Vec<usize> = self.vmas.range(start..end).map(|(k, _)| *k).collect();
        for k in keys {
            self.vmas.remove(&k);
        }
        mm.unmap_pages(start, end);
    }

    pub fn is_free(&self, start: usize, end: usize) -> bool {
        if let Some((_, v)) = self.vmas.range(..end).next_back() {
            if v.end > start {
                return false;
            }
        }
        true
    }

    /// A free range of `len` bytes, top-down from mmap_top.
    pub fn find_free(&self, len: usize) -> Option<usize> {
        let mut top = self.mmap_top;
        for (_, v) in self.vmas.range(..self.mmap_top).rev() {
            if v.end <= top && top - v.end >= len {
                return Some(top - len);
            }
            top = top.min(v.start);
        }
        if top >= len + 0x10000 { Some(top - len) } else { None }
    }

    pub fn insert(&mut self, v: Vma) {
        // merge with an identical neighbour below (common for anonymous memory)
        if let Some((&s, prev)) = self.vmas.range_mut(..v.start).next_back() {
            if prev.end == v.start
                && prev.file.is_none()
                && v.file.is_none()
                && prev.prot == v.prot
                && prev.shared == v.shared
                && prev.kind == v.kind
                && prev.dontfork == v.dontfork
            {
                prev.end = v.end;
                let _ = s;
                return;
            }
        }
        self.vmas.insert(v.start, v);
    }

    pub fn protect(&mut self, mm: &Mm, start: usize, end: usize, prot: u32) -> Result<()> {
        // the whole range must be mapped
        let mut va = start;
        while va < end {
            let v = find_vma(&self.vmas, va).ok_or(ENOMEM)?;
            if prot & PROT_WRITE != 0 && v.shared && v.file.as_ref().is_some_and(|f| !f.mmap_writable()) {
                return Err(EACCES);
            }
            va = v.end;
        }
        self.split(start);
        self.split(end);
        let keys: Vec<usize> = self.vmas.range(start..end).map(|(k, _)| *k).collect();
        for k in keys {
            let v = self.vmas.get_mut(&k).unwrap();
            v.prot = prot;
            let (s, e, sh) = (v.start, v.end, v.shared);
            mm.reprotect(s, e, prot, sh);
        }
        Ok(())
    }

    pub fn set_dontfork(&mut self, start: usize, end: usize, on: bool) -> Result<()> {
        self.split(start);
        self.split(end);
        for (_, v) in self.vmas.range_mut(start..end) {
            v.dontfork = on;
        }
        Ok(())
    }
}

// ---- access to the current task's user memory

pub fn current_mm() -> Arc<Mm> {
    crate::task::current_thread().mm.clone()
}

/// Calls `f` with kernel views of the user range [va, va+len), page by page.
pub fn with_user(va: usize, len: usize, access: Access, mut f: impl FnMut(&mut [u8], usize)) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let end = va.checked_add(len).ok_or(EFAULT)?;
    if end > arch::USER_TOP {
        return Err(EFAULT);
    }
    let mm = current_mm();
    let mut p = va;
    let mut done = 0;
    while p < end {
        let off = p & (PAGE_SIZE - 1);
        let n = (PAGE_SIZE - off).min(end - p);
        let pa = mm.get_user_page(p, access)?;
        let k = unsafe { core::slice::from_raw_parts_mut((phys_to_virt(pa) + off) as *mut u8, n) };
        f(k, done);
        frame::put_page(pa);
        done += n;
        p += n;
    }
    Ok(())
}

pub fn copy_from_user(dst: &mut [u8], va: usize) -> Result<()> {
    with_user(va, dst.len(), Access::Read, |k, off| dst[off..off + k.len()].copy_from_slice(k))
}

pub fn copy_to_user(va: usize, src: &[u8]) -> Result<()> {
    with_user(va, src.len(), Access::Write, |k, off| k.copy_from_slice(&src[off..off + k.len()]))
}

pub fn zero_user(va: usize, len: usize) -> Result<()> {
    with_user(va, len, Access::Write, |k, _| k.fill(0))
}

/// Plain-old-data values that can be copied to and from user memory.
/// # Safety
/// Every bit pattern must be a valid value.
pub unsafe trait Pod: Copy {}
unsafe impl Pod for u8 {}
unsafe impl Pod for u16 {}
unsafe impl Pod for u32 {}
unsafe impl Pod for u64 {}
unsafe impl Pod for i32 {}
unsafe impl Pod for i64 {}
unsafe impl Pod for usize {}
unsafe impl Pod for isize {}
unsafe impl<T: Pod, const N: usize> Pod for [T; N] {}

pub fn read_user<T: Pod>(va: usize) -> Result<T> {
    let mut v = core::mem::MaybeUninit::<T>::zeroed();
    let b = unsafe { core::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, core::mem::size_of::<T>()) };
    copy_from_user(b, va)?;
    Ok(unsafe { v.assume_init() })
}

pub fn write_user<T: Pod>(va: usize, v: &T) -> Result<()> {
    let b = unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) };
    copy_to_user(va, b)
}

/// A NUL-terminated string from user memory (at most `max` bytes).
pub fn read_cstr(va: usize, max: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut p = va;
    let mm = current_mm();
    loop {
        let off = p & (PAGE_SIZE - 1);
        let pa = mm.get_user_page(p, Access::Read)?;
        let k = unsafe { core::slice::from_raw_parts((phys_to_virt(pa) + off) as *const u8, PAGE_SIZE - off) };
        let nul = k.iter().position(|&b| b == 0);
        let take = nul.unwrap_or(k.len());
        out.extend_from_slice(&k[..take]);
        frame::put_page(pa);
        if nul.is_some() {
            return Ok(out);
        }
        if out.len() > max {
            return Err(ENAMETOOLONG);
        }
        p += take;
    }
}

/// An array of user pointers ending with NULL (argv, envp).
pub fn read_ptr_array(va: usize, max: usize) -> Result<Vec<usize>> {
    let mut v = Vec::new();
    if va == 0 {
        return Ok(v);
    }
    loop {
        let p: usize = read_user(va + v.len() * 8)?;
        if p == 0 {
            return Ok(v);
        }
        v.push(p);
        if v.len() > max {
            return Err(E2BIG);
        }
    }
}
