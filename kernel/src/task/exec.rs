//! execve: ELF64 loading (static and dynamically linked, ET_EXEC and ET_DYN), `#!` scripts, the
//! initial stack (argv, envp, auxv) and the vDSO.
use super::{current_thread, own, signal};
use crate::arch::{self, FpState, TrapFrame};
use crate::errno::*;
use crate::mm::frame::PAGE_SIZE;
use crate::mm::vm::{self, Mm, Vma, VmaKind, PROT_EXEC, PROT_READ, PROT_WRITE};
use crate::vfs::path::{may, Lookup, MNT_NOEXEC};
use crate::vfs::{Inode, PathRef, S_IFMT, S_IFREG};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;
const PT_GNU_STACK: u32 = 0x6474_e551;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;

#[derive(Clone, Copy, Default)]
struct Phdr {
    p_type: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
}

struct Elf {
    etype: u16,
    entry: u64,
    phoff: u64,
    phnum: usize,
    phdrs: Vec<Phdr>,
}

fn u16at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn parse_elf(inode: &Arc<dyn Inode>, head: &[u8]) -> Result<Elf> {
    if head.len() < 64 || &head[..4] != b"\x7fELF" || head[4] != 2 || head[5] != 1 {
        return Err(ENOEXEC);
    }
    let etype = u16at(head, 16);
    if (etype != ET_EXEC && etype != ET_DYN) || u16at(head, 18) != arch::ELF_MACHINE {
        return Err(ENOEXEC);
    }
    let phoff = u64at(head, 32);
    let phentsize = u16at(head, 54) as usize;
    let phnum = u16at(head, 56) as usize;
    if phentsize != 56 || phnum == 0 || phnum > 64 {
        return Err(ENOEXEC);
    }
    let mut ph = alloc::vec![0u8; phnum * 56];
    if inode.read_at(phoff, &mut ph)? != ph.len() {
        return Err(ENOEXEC);
    }
    let phdrs = (0..phnum)
        .map(|i| {
            let p = &ph[i * 56..];
            Phdr { p_type: u32at(p, 0), flags: u32at(p, 4), offset: u64at(p, 8), vaddr: u64at(p, 16), filesz: u64at(p, 32), memsz: u64at(p, 40) }
        })
        .collect();
    Ok(Elf { etype, entry: u64at(head, 24), phoff, phnum, phdrs })
}

fn open_exec(path: &[u8], at: &PathRef) -> Result<(PathRef, Arc<dyn Inode>)> {
    let th = current_thread();
    let cred = th.proc.cred();
    let root = th.fs.lock().root.clone();
    let lk = Lookup { root: &root, cred: &cred, follow_last: true, beneath: false, no_symlinks: false };
    let p = lk.resolve(at, path)?;
    let inode = p.inode()?;
    if inode.stat()?.mode & S_IFMT != S_IFREG {
        return Err(EACCES);
    }
    if p.mnt.flags() & MNT_NOEXEC != 0 {
        return Err(EACCES);
    }
    may(&inode, 1, &cred)?;
    Ok((p, inode))
}

/// Maps the PT_LOAD segments of `elf` at `bias` into the current (new) address space. Returns the
/// end of the highest segment.
fn map_segments(mm: &Arc<Mm>, inode: &Arc<dyn Inode>, path: &PathRef, elf: &Elf, bias: usize) -> Result<usize> {
    let mut end = 0;
    for ph in elf.phdrs.iter().filter(|p| p.p_type == PT_LOAD && p.memsz > 0) {
        let prot = (if ph.flags & 4 != 0 { PROT_READ } else { 0 })
            | (if ph.flags & 2 != 0 { PROT_WRITE } else { 0 })
            | (if ph.flags & 1 != 0 { PROT_EXEC } else { 0 });
        let va = bias + ph.vaddr as usize;
        let start = va & !(PAGE_SIZE - 1);
        let file_end = va + ph.filesz as usize;
        let mem_end = va + ph.memsz as usize;
        let file_page_end = (file_end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let mem_page_end = (mem_end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let off = ph.offset - (va - start) as u64;
        {
            let mut inner = mm.inner.lock();
            if ph.filesz > 0 {
                inner.unmap(mm, start, file_page_end);
                inner.insert(Vma {
                    start,
                    end: file_page_end,
                    prot,
                    shared: false,
                    file: Some(inode.clone()),
                    offset: off,
                    dontfork: false,
                    kind: VmaKind::Normal,
                    path: Some(path.clone()),
                });
            }
            let anon_start = if ph.filesz > 0 { file_page_end } else { start };
            if mem_page_end > anon_start {
                inner.unmap(mm, anon_start, mem_page_end);
                inner.insert(Vma { start: anon_start, end: mem_page_end, prot, shared: false, file: None, offset: 0, dontfork: false, kind: VmaKind::Normal, path: None });
            }
        }
        // the part of the last file page past p_filesz is .bss: zero it
        if ph.memsz > ph.filesz && file_end < file_page_end && ph.filesz > 0 {
            let n = (file_page_end.min(mem_end)) - file_end;
            if prot & PROT_WRITE != 0 {
                vm::zero_user(file_end, n)?;
            } else {
                // a read-only segment with a partial page: write through a temporary mapping change
                let mut inner = mm.inner.lock();
                inner.protect(mm, start, file_page_end, prot | PROT_WRITE)?;
                drop(inner);
                vm::zero_user(file_end, n)?;
                mm.inner.lock().protect(mm, start, file_page_end, prot)?;
            }
        }
        end = end.max(mem_end);
    }
    Ok(end)
}

fn load_bias(elf: &Elf, mm: &Arc<Mm>, main: bool) -> Result<usize> {
    if elf.etype == ET_EXEC {
        return Ok(0);
    }
    let lo = elf.phdrs.iter().filter(|p| p.p_type == PT_LOAD).map(|p| p.vaddr as usize).min().unwrap_or(0) & !(PAGE_SIZE - 1);
    let hi = elf.phdrs.iter().filter(|p| p.p_type == PT_LOAD).map(|p| (p.vaddr + p.memsz) as usize).max().unwrap_or(0);
    let span = ((hi - lo) + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    if main {
        // ELF_ET_DYN_BASE: two thirds of the user address space
        let base = (arch::USER_TOP / 3 * 2) & !0xffff;
        return Ok(base - lo);
    }
    let inner = mm.inner.lock();
    let at = inner.find_free(span).ok_or(ENOMEM)?;
    Ok(at - lo)
}

pub const AT_NULL: usize = 0;
pub const AT_PHDR: usize = 3;
pub const AT_PHENT: usize = 4;
pub const AT_PHNUM: usize = 5;
pub const AT_PAGESZ: usize = 6;
pub const AT_BASE: usize = 7;
pub const AT_FLAGS: usize = 8;
pub const AT_ENTRY: usize = 9;
pub const AT_UID: usize = 11;
pub const AT_EUID: usize = 12;
pub const AT_GID: usize = 13;
pub const AT_EGID: usize = 14;
pub const AT_PLATFORM: usize = 15;
pub const AT_HWCAP: usize = 16;
pub const AT_CLKTCK: usize = 17;
pub const AT_SECURE: usize = 23;
pub const AT_RANDOM: usize = 25;
pub const AT_HWCAP2: usize = 26;
pub const AT_EXECFN: usize = 31;
pub const AT_SYSINFO_EHDR: usize = 33;
pub const AT_MINSIGSTKSZ: usize = 51;

/// Replaces the current program. `frame` is the trap frame to return to user mode with.
pub fn execve(path: &[u8], argv: Vec<Vec<u8>>, envp: Vec<Vec<u8>>, at: &PathRef, frame: &mut TrapFrame) -> Result<()> {
    let mut path = path.to_vec();
    let mut argv = argv;
    let mut depth = 0;
    let (file, inode, elf, head) = loop {
        let (file, inode) = open_exec(&path, at)?;
        let mut head = alloc::vec![0u8; 256];
        let n = inode.read_at(0, &mut head)?;
        head.truncate(n);
        if head.starts_with(b"#!") {
            depth += 1;
            if depth > 4 {
                return Err(ELOOP);
            }
            let line_end = head.iter().position(|&c| c == b'\n').unwrap_or(head.len());
            let line = &head[2..line_end];
            let line: Vec<u8> = line.iter().copied().skip_while(|c| *c == b' ' || *c == b'\t').collect();
            let interp_end = line.iter().position(|&c| c == b' ' || c == b'\t').unwrap_or(line.len());
            let interp = line[..interp_end].to_vec();
            let arg: Vec<u8> = {
                let rest = &line[interp_end..];
                let s = rest.iter().position(|&c| c != b' ' && c != b'\t').unwrap_or(rest.len());
                let mut a = rest[s..].to_vec();
                while a.last().is_some_and(|c| *c == b' ' || *c == b'\t' || *c == b'\r') {
                    a.pop();
                }
                a
            };
            if interp.is_empty() {
                return Err(ENOEXEC);
            }
            let mut nv = alloc::vec![interp.clone()];
            if !arg.is_empty() {
                nv.push(arg);
            }
            nv.push(path.clone());
            nv.extend(argv.into_iter().skip(1));
            argv = nv;
            path = interp;
            continue;
        }
        let elf = parse_elf(&inode, &head)?;
        break (file, inode, elf, head);
    };
    let _ = head;
    // the interpreter (dynamic linker)
    let mut interp = None;
    if let Some(ph) = elf.phdrs.iter().find(|p| p.p_type == PT_INTERP) {
        let mut name = alloc::vec![0u8; ph.filesz as usize];
        inode.read_at(ph.offset, &mut name)?;
        while name.last() == Some(&0) {
            name.pop();
        }
        let (ipath, ii) = open_exec(&name, at)?;
        let mut h = alloc::vec![0u8; 64];
        ii.read_at(0, &mut h)?;
        let ie = parse_elf(&ii, &h)?;
        interp = Some((ii, ie, ipath));
    }

    // ---- point of no return: a new address space
    let th = current_thread();
    de_thread();
    let mm = Mm::new();
    let stack_lim = th.proc.rlimit(super::RLIMIT_STACK).0.min(1 << 30).max(128 << 10) as usize;
    let stack_top = arch::USER_TOP - PAGE_SIZE;
    {
        let mut inner = mm.inner.lock();
        inner.stack_top = stack_top;
        inner.mmap_top = stack_top - stack_lim.max(128 << 20) - (16 << 20);
        inner.exe = Some(file.clone());
        inner.insert(Vma { start: stack_top - stack_lim, end: stack_top, prot: PROT_READ | PROT_WRITE, shared: false, file: None, offset: 0, dontfork: false, kind: VmaKind::Stack, path: None });
    }
    switch_mm(mm.clone());
    let fail = |_e: Errno| -> ! {
        let status = signal::killed_status(signal::SIGSEGV, false);
        super::exit(status, true)
    };
    let bias = load_bias(&elf, &mm, true).unwrap_or_else(|e| fail(e));
    let end = map_segments(&mm, &inode, &file, &elf, bias).unwrap_or_else(|e| fail(e));
    let (entry, base) = match &interp {
        Some((ii, ie, ip)) => {
            let ib = load_bias(ie, &mm, false).unwrap_or_else(|e| fail(e));
            map_segments(&mm, ii, ip, ie, ib).unwrap_or_else(|e| fail(e));
            (ib + ie.entry as usize, ib)
        }
        None => (bias + elf.entry as usize, 0),
    };
    {
        let mut inner = mm.inner.lock();
        let brk = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        inner.brk_start = brk;
        inner.brk = brk;
    }
    let vdso_base = crate::vdso::map(&mm).unwrap_or_else(|e| fail(e));
    let phdr = match elf.phdrs.iter().find(|p| p.p_type == PT_PHDR) {
        Some(p) => bias + p.vaddr as usize,
        None => {
            let first = elf.phdrs.iter().find(|p| p.p_type == PT_LOAD && p.offset == 0);
            bias + first.map_or(0, |p| p.vaddr as usize) + elf.phoff as usize
        }
    };
    let _ = PT_GNU_STACK;

    // ---- the initial stack
    let cred = th.proc.cred();
    let mut sp = stack_top;
    let push_bytes = |sp: &mut usize, b: &[u8]| -> usize {
        *sp -= b.len() + 1;
        let _ = vm::copy_to_user(*sp, b);
        let _ = vm::write_user(*sp + b.len(), &0u8);
        *sp
    };
    let execfn = push_bytes(&mut sp, &path);
    let mut env_ptrs = Vec::with_capacity(envp.len());
    for e in envp.iter().rev() {
        env_ptrs.push(push_bytes(&mut sp, e));
    }
    env_ptrs.reverse();
    let mut arg_ptrs = Vec::with_capacity(argv.len());
    for a in argv.iter().rev() {
        arg_ptrs.push(push_bytes(&mut sp, a));
    }
    arg_ptrs.reverse();
    let platform = push_bytes(&mut sp, arch::PLATFORM);
    sp &= !15;
    sp -= 16;
    let random = sp;
    let mut rb = [0u8; 16];
    crate::random::fill(&mut rb);
    vm::copy_to_user(random, &rb).unwrap_or_else(|e| fail(e));
    let (hw, hw2) = arch::hwcap();
    let minsig = arch::SIGFRAME_SIZE + 128;
    let auxv: Vec<usize> = alloc::vec![
        AT_SYSINFO_EHDR, vdso_base,
        AT_MINSIGSTKSZ, minsig,
        AT_HWCAP, hw,
        AT_PAGESZ, PAGE_SIZE,
        AT_CLKTCK, 100,
        AT_PHDR, phdr,
        AT_PHENT, 56,
        AT_PHNUM, elf.phnum,
        AT_BASE, base,
        AT_FLAGS, 0,
        AT_ENTRY, bias + elf.entry as usize,
        AT_UID, cred.uid as usize,
        AT_EUID, cred.euid as usize,
        AT_GID, cred.gid as usize,
        AT_EGID, cred.egid as usize,
        AT_SECURE, 0,
        AT_RANDOM, random,
        AT_HWCAP2, hw2,
        AT_EXECFN, execfn,
        AT_PLATFORM, platform,
        AT_NULL, 0,
    ];
    let words = 1 + arg_ptrs.len() + 1 + env_ptrs.len() + 1 + auxv.len();
    sp -= words * 8;
    sp &= !15;
    let mut v: Vec<usize> = Vec::with_capacity(words);
    v.push(arg_ptrs.len());
    v.extend(&arg_ptrs);
    v.push(0);
    v.extend(&env_ptrs);
    v.push(0);
    v.extend(&auxv);
    let bytes = unsafe { core::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 8) };
    vm::copy_to_user(sp, bytes).unwrap_or_else(|e| fail(e));
    {
        let mut inner = mm.inner.lock();
        inner.arg_start = arg_ptrs.first().copied().unwrap_or(sp);
        inner.arg_end = arg_ptrs.last().map_or(sp, |&p| p + argv.last().map_or(0, |a| a.len() + 1));
        inner.env_start = env_ptrs.first().copied().unwrap_or(inner.arg_end);
        inner.env_end = env_ptrs.last().map_or(inner.env_start, |&p| p + envp.last().map_or(0, |e| e.len() + 1));
        inner.auxv = auxv;
    }

    // ---- the rest of the process state
    th.files.lock().close_on_exec();
    th.sighand.lock().reset_handled();
    let o = own();
    o.altstack = (0, 0, signal::SS_DISABLE);
    o.fp = FpState::default();
    arch::restore_fp(&o.fp);
    o.tls = 0;
    arch::write_tls(0);
    o.clear_child_tid = 0;
    o.robust_list = 0;
    o.in_syscall = false;
    let name = path.rsplit(|&c| c == b'/').next().unwrap_or(&path);
    let mut comm = [0u8; 16];
    let n = name.len().min(15);
    comm[..n].copy_from_slice(&name[..n]);
    *th.comm.lock() = comm;
    if let Some(c) = th.vfork_done.lock().take() {
        c.complete();
    }
    *frame = arch::new_user_frame(entry, sp);
    Ok(())
}

/// Replaces the thread's address space (exec).
fn switch_mm(mm: Arc<Mm>) {
    let t = crate::sched::current();
    let th = unsafe { t.user_mut().unwrap() };
    let irq = arch::irq_save();
    arch::activate_mm(mm.root, mm.asid);
    crate::percpu::this().loaded_mm.store(mm.id, Ordering::Relaxed);
    mm.cpus.fetch_or(1 << crate::percpu::this().id, Ordering::Relaxed);
    let old = core::mem::replace(&mut th.mm, mm);
    arch::irq_restore(irq);
    drop(old);
}

/// Other threads of the process end before exec continues.
fn de_thread() {
    let th = current_thread();
    let proc = &th.proc;
    let me = crate::sched::current_arc();
    let others: Vec<_> = proc.threads.lock().iter().filter_map(|w| w.upgrade()).filter(|t| !Arc::ptr_eq(t, &me)).collect();
    if others.is_empty() {
        return;
    }
    for t in &others {
        if let Some(o) = super::thread_of(t) {
            o.exit_now.store(true, Ordering::SeqCst);
        }
        crate::sched::wake(t);
    }
    proc.child_wq.wait_until(|| proc.live_threads.load(Ordering::SeqCst) == 1, None, false);
}
