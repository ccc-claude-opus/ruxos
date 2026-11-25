//! Memory system calls: mmap, munmap, mprotect, brk, mremap, madvise, msync, mincore.
use crate::errno::*;
use crate::mm::frame::PAGE_SIZE;
use crate::mm::vm::{self, current_mm, find_vma, Vma, VmaKind, PROT_EXEC, PROT_READ, PROT_WRITE};
use crate::vfs::file::{O_ACCMODE, O_RDONLY, O_WRONLY};
use crate::vfs::{S_IFMT, S_IFREG, S_IFBLK, S_IFCHR};
use alloc::sync::Arc;

pub const MAP_SHARED: u32 = 1;
pub const MAP_PRIVATE: u32 = 2;
pub const MAP_SHARED_VALIDATE: u32 = 3;
pub const MAP_FIXED: u32 = 0x10;
pub const MAP_ANONYMOUS: u32 = 0x20;
pub const MAP_NORESERVE: u32 = 0x4000;
pub const MAP_POPULATE: u32 = 0x8000;
pub const MAP_FIXED_NOREPLACE: u32 = 0x100000;
pub const MAP_GROWSDOWN: u32 = 0x100;
pub const MAP_STACK: u32 = 0x20000;

fn page_up(x: usize) -> Option<usize> {
    x.checked_add(PAGE_SIZE - 1).map(|v| v & !(PAGE_SIZE - 1))
}

pub fn mmap(addr: usize, len: usize, prot: u32, flags: u32, fd: i32, off: u64) -> Result<usize> {
    if len == 0 || off as usize % PAGE_SIZE != 0 || prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 {
        return Err(EINVAL);
    }
    let len = page_up(len).ok_or(ENOMEM)?;
    if len > crate::arch::USER_TOP {
        return Err(ENOMEM);
    }
    let kind = flags & 0xf;
    let shared = match kind {
        MAP_SHARED | MAP_SHARED_VALIDATE => true,
        MAP_PRIVATE => false,
        _ => return Err(EINVAL),
    };
    let mut file = None;
    let mut fpath = None;
    if flags & MAP_ANONYMOUS == 0 {
        let f = super::fs::file(fd)?;
        fpath = f.path.clone();
        let inode = f.inode().ok_or(ENODEV)?;
        let t = inode.stat()?.mode & S_IFMT;
        if t != S_IFREG && t != S_IFBLK && t != S_IFCHR {
            return Err(ENODEV);
        }
        let acc = f.flags() & O_ACCMODE;
        if acc == O_WRONLY {
            return Err(EACCES);
        }
        if shared && prot & PROT_WRITE != 0 && acc == O_RDONLY {
            return Err(EACCES);
        }
        if t == S_IFCHR {
            // /dev/zero: anonymous memory
            if shared {
                file = Some(crate::vfs::shmem::new(len as u64)?);
            }
        } else {
            file = Some(inode);
        }
    } else if shared {
        file = Some(crate::vfs::shmem::new(len as u64)?);
    }
    let mm = current_mm();
    let mut inner = mm.inner.lock();
    let start = if flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0 {
        if addr % PAGE_SIZE != 0 || addr.checked_add(len).is_none_or(|e| e > crate::arch::USER_TOP) {
            return Err(EINVAL);
        }
        if flags & MAP_FIXED_NOREPLACE != 0 && !inner.is_free(addr, addr + len) {
            return Err(EEXIST);
        }
        inner.unmap(&mm, addr, addr + len);
        addr
    } else {
        let hint = addr & !(PAGE_SIZE - 1);
        if hint != 0 && hint.checked_add(len).is_some_and(|e| e <= inner.mmap_top) && inner.is_free(hint, hint + len) {
            hint
        } else {
            inner.find_free(len).ok_or(ENOMEM)?
        }
    };
    inner.insert(Vma { start, end: start + len, prot, shared, file, offset: off, dontfork: false, kind: VmaKind::Normal, path: fpath });
    drop(inner);
    if flags & MAP_POPULATE != 0 {
        let mut va = start;
        while va < start + len {
            let _ = mm.fault(va, if prot & PROT_WRITE != 0 { vm::Access::Write } else { vm::Access::Read });
            va += PAGE_SIZE;
        }
    }
    Ok(start)
}

pub fn munmap(addr: usize, len: usize) -> Result<usize> {
    if addr % PAGE_SIZE != 0 || len == 0 {
        return Err(EINVAL);
    }
    let len = page_up(len).ok_or(EINVAL)?;
    let mm = current_mm();
    mm.inner.lock().unmap(&mm, addr, addr + len);
    Ok(0)
}

pub fn mprotect(addr: usize, len: usize, prot: u32) -> Result<usize> {
    if addr % PAGE_SIZE != 0 || prot & !(PROT_READ | PROT_WRITE | PROT_EXEC | 0x0100_0000 | 0x0200_0000) != 0 {
        return Err(EINVAL);
    }
    let prot = prot & 7;
    let len = page_up(len).ok_or(ENOMEM)?;
    if len == 0 {
        return Ok(0);
    }
    let mm = current_mm();
    mm.inner.lock().protect(&mm, addr, addr + len, prot)?;
    Ok(0)
}

pub fn brk(addr: usize) -> Result<usize> {
    let mm = current_mm();
    let mut inner = mm.inner.lock();
    let cur = inner.brk;
    if addr < inner.brk_start {
        return Ok(cur);
    }
    let new_end = page_up(addr).unwrap_or(cur);
    let old_end = page_up(cur).unwrap_or(cur);
    if new_end > old_end {
        if !inner.is_free(old_end, new_end) {
            return Ok(cur);
        }
        inner.insert(Vma { start: old_end, end: new_end, prot: PROT_READ | PROT_WRITE, shared: false, file: None, offset: 0, dontfork: false, kind: VmaKind::Heap, path: None });
    } else if new_end < old_end {
        inner.unmap(&mm, new_end, old_end);
    }
    inner.brk = addr;
    Ok(addr)
}

pub const MREMAP_MAYMOVE: u32 = 1;
pub const MREMAP_FIXED: u32 = 2;

pub fn mremap(old: usize, old_len: usize, new_len: usize, flags: u32, new_addr: usize) -> Result<usize> {
    if old % PAGE_SIZE != 0 || new_len == 0 {
        return Err(EINVAL);
    }
    let old_len = page_up(old_len).ok_or(EINVAL)?;
    let new_len = page_up(new_len).ok_or(ENOMEM)?;
    let mm = current_mm();
    let mut inner = mm.inner.lock();
    let v = find_vma(&inner.vmas, old).cloned().ok_or(EFAULT)?;
    if old + old_len > v.end {
        return Err(EFAULT);
    }
    if flags & MREMAP_FIXED == 0 {
        if new_len <= old_len {
            inner.unmap(&mm, old + new_len, old + old_len);
            return Ok(old);
        }
        // grow in place
        if old + old_len == v.end && inner.is_free(v.end, old + new_len) {
            let mut nv = v.clone();
            nv.offset += (v.end - v.start) as u64;
            nv.start = v.end;
            nv.end = old + new_len;
            inner.vmas.insert(nv.start, nv);
            return Ok(old);
        }
        if flags & MREMAP_MAYMOVE == 0 {
            return Err(ENOMEM);
        }
    }
    let dest = if flags & MREMAP_FIXED != 0 {
        if new_addr % PAGE_SIZE != 0 {
            return Err(EINVAL);
        }
        inner.unmap(&mm, new_addr, new_addr + new_len);
        new_addr
    } else {
        inner.find_free(new_len).ok_or(ENOMEM)?
    };
    // move the page table entries, then the VMA
    let keep = old_len.min(new_len);
    let mut va = 0;
    while va < keep {
        if let Some(e) = mm.pte(old + va, false) {
            if crate::arch::pte_valid(*e) {
                let pte = *e;
                *e = 0;
                *mm.pte(dest + va, true).ok_or(ENOMEM)? = pte;
            }
        }
        va += PAGE_SIZE;
    }
    crate::arch::tlb_flush_asid(mm.asid);
    let mut nv = v.clone();
    nv.offset += (old - v.start) as u64;
    nv.start = dest;
    nv.end = dest + new_len;
    inner.unmap(&mm, old, old + old_len);
    inner.vmas.insert(dest, nv);
    Ok(dest)
}

pub const MADV_DONTNEED: u32 = 4;
pub const MADV_FREE: u32 = 8;
pub const MADV_REMOVE: u32 = 9;
pub const MADV_DONTFORK: u32 = 10;
pub const MADV_DOFORK: u32 = 11;

pub fn madvise(addr: usize, len: usize, advice: u32) -> Result<usize> {
    if addr % PAGE_SIZE != 0 {
        return Err(EINVAL);
    }
    let len = page_up(len).ok_or(EINVAL)?;
    let mm = current_mm();
    match advice {
        MADV_DONTNEED | MADV_FREE | MADV_REMOVE => {
            let inner = mm.inner.lock();
            // only private pages are dropped (shared ones keep their contents in the cache)
            let ranges: alloc::vec::Vec<(usize, usize)> = inner
                .vmas
                .range(..addr + len)
                .filter(|(_, v)| v.end > addr && !v.shared)
                .map(|(_, v)| (v.start.max(addr), v.end.min(addr + len)))
                .collect();
            for (s, e) in ranges {
                mm.unmap_pages(s, e);
            }
            Ok(0)
        }
        MADV_DONTFORK | MADV_DOFORK => {
            mm.inner.lock().set_dontfork(addr, addr + len, advice == MADV_DONTFORK)?;
            Ok(0)
        }
        _ => Ok(0), // hints
    }
}

pub fn msync(addr: usize, len: usize, _flags: u32) -> Result<usize> {
    if addr % PAGE_SIZE != 0 {
        return Err(EINVAL);
    }
    let mm = current_mm();
    let files: alloc::vec::Vec<Arc<dyn crate::vfs::Inode>> = {
        let inner = mm.inner.lock();
        inner.vmas.range(..addr + len.max(1)).filter(|(_, v)| v.end > addr && v.shared).filter_map(|(_, v)| v.file.clone()).collect()
    };
    for f in files {
        f.fsync(true)?;
    }
    Ok(0)
}

pub fn mincore(addr: usize, len: usize, vec: usize) -> Result<usize> {
    if addr % PAGE_SIZE != 0 {
        return Err(EINVAL);
    }
    let mm = current_mm();
    let n = page_up(len).ok_or(ENOMEM)? / PAGE_SIZE;
    let mut out = alloc::vec![0u8; n];
    {
        let inner = mm.inner.lock();
        for (i, o) in out.iter_mut().enumerate() {
            let va = addr + i * PAGE_SIZE;
            if find_vma(&inner.vmas, va).is_none() {
                return Err(ENOMEM);
            }
            if mm.pte(va, false).is_some_and(|e| crate::arch::pte_valid(*e)) {
                *o = 1;
            }
        }
    }
    vm::copy_to_user(vec, &out)?;
    Ok(0)
}
