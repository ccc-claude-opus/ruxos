//! seccomp(2): SECCOMP_MODE_STRICT and SECCOMP_MODE_FILTER with classic BPF programs over
//! `struct seccomp_data` (Documentation/userspace-api/seccomp_filter.rst). Filters stack: every
//! filter of the thread runs and the action with the highest precedence wins.
use crate::errno::*;
use crate::mm::vm::read_user;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub const SECCOMP_MODE_DISABLED: u32 = 0;
pub const SECCOMP_MODE_STRICT: u32 = 1;
pub const SECCOMP_MODE_FILTER: u32 = 2;

pub const RET_KILL_PROCESS: u32 = 0x8000_0000;
pub const RET_KILL_THREAD: u32 = 0x0000_0000;
pub const RET_TRAP: u32 = 0x0003_0000;
pub const RET_ERRNO: u32 = 0x0005_0000;
pub const RET_USER_NOTIF: u32 = 0x7fc0_0000;
pub const RET_TRACE: u32 = 0x7ff0_0000;
pub const RET_LOG: u32 = 0x7ffc_0000;
pub const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ACTION_FULL: u32 = 0xffff_0000;
const RET_DATA: u32 = 0x0000_ffff;

#[cfg(target_arch = "aarch64")]
pub const AUDIT_ARCH: u32 = 0xC000_00B7;
#[cfg(target_arch = "riscv64")]
pub const AUDIT_ARCH: u32 = 0xC000_00F3;

#[derive(Clone, Copy, Debug)]
pub struct Insn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// One installed program; `prev` is the filter that was installed before it.
pub struct Filter {
    prog: Vec<Insn>,
    prev: Option<Arc<Filter>>,
}

// classic BPF (include/uapi/linux/filter.h)
const LD: u16 = 0x00;
const LDX: u16 = 0x01;
const ST: u16 = 0x02;
const STX: u16 = 0x03;
const ALU: u16 = 0x04;
const JMP: u16 = 0x05;
const RET: u16 = 0x06;
const MISC: u16 = 0x07;
const W: u16 = 0x00;
const ABS: u16 = 0x20;
const IMM: u16 = 0x00;
const MEM: u16 = 0x60;
const LEN: u16 = 0x80;
const K: u16 = 0x00;
const X: u16 = 0x08;
const A: u16 = 0x10;

const MAX_INSNS: usize = 4096;
/// sizeof(struct seccomp_data)
const DATA_LEN: u32 = 64;

/// Linux's seccomp_check_filter: only the instructions that make sense on seccomp_data (32-bit
/// aligned absolute loads within it), jumps forward within the program, ending in a return.
fn check(prog: &mut [Insn]) -> Result<()> {
    let n = prog.len();
    if n == 0 || n > MAX_INSNS {
        return Err(EINVAL);
    }
    for i in 0..n {
        let ins = &mut prog[i];
        let class = ins.code & 7;
        match ins.code {
            // loads
            c if c == LD | W | ABS => {
                if ins.k >= DATA_LEN || ins.k & 3 != 0 {
                    return Err(EINVAL);
                }
            }
            c if c == LD | W | LEN => {
                ins.code = LD | IMM;
                ins.k = DATA_LEN;
            }
            c if c == LDX | W | LEN => {
                ins.code = LDX | IMM;
                ins.k = DATA_LEN;
            }
            c if c == LD | IMM || c == LDX | IMM => {}
            c if c == LD | MEM || c == LDX | MEM || c == ST || c == STX => {
                if ins.k >= 16 {
                    return Err(EINVAL);
                }
            }
            c if class == ALU => {
                let op = c & 0xf0;
                // ADD SUB MUL DIV OR AND LSH RSH NEG MOD XOR
                if !matches!(op, 0x00 | 0x10 | 0x20 | 0x30 | 0x40 | 0x50 | 0x60 | 0x70 | 0x80 | 0x90 | 0xa0) {
                    return Err(EINVAL);
                }
                if c & X == 0 && (op == 0x30 || op == 0x90) && ins.k == 0 {
                    return Err(EINVAL); // division by a zero constant
                }
            }
            c if class == JMP => {
                let op = c & 0xf0;
                match op {
                    0x00 => {
                        // JA
                        if (i + 1).checked_add(ins.k as usize).is_none_or(|t| t >= n) {
                            return Err(EINVAL);
                        }
                    }
                    0x10 | 0x20 | 0x30 | 0x40 => {
                        if i + 1 + ins.jt as usize >= n || i + 1 + ins.jf as usize >= n {
                            return Err(EINVAL);
                        }
                    }
                    _ => return Err(EINVAL),
                }
            }
            c if c == RET | K || c == RET | A => {}
            c if c == MISC || c == MISC | 0x80 => {} // TAX, TXA
            _ => return Err(EINVAL),
        }
    }
    let last = prog[n - 1].code;
    if last != RET | K && last != RET | A {
        return Err(EINVAL);
    }
    Ok(())
}

/// Runs one program over `data` (the 64 bytes of seccomp_data).
fn run(prog: &[Insn], data: &[u8; 64]) -> u32 {
    let (mut a, mut x) = (0u32, 0u32);
    let mut m = [0u32; 16];
    let mut pc = 0usize;
    while pc < prog.len() {
        let ins = prog[pc];
        pc += 1;
        let class = ins.code & 7;
        match class {
            LD => match ins.code & 0xe0 {
                ABS => a = u32::from_le_bytes(data[ins.k as usize..ins.k as usize + 4].try_into().unwrap()),
                IMM => a = ins.k,
                MEM => a = m[ins.k as usize],
                _ => return RET_KILL_PROCESS,
            },
            LDX => match ins.code & 0xe0 {
                IMM => x = ins.k,
                MEM => x = m[ins.k as usize],
                _ => return RET_KILL_PROCESS,
            },
            ST => m[ins.k as usize] = a,
            STX => m[ins.k as usize] = x,
            ALU => {
                let v = if ins.code & X != 0 { x } else { ins.k };
                a = match ins.code & 0xf0 {
                    0x00 => a.wrapping_add(v),
                    0x10 => a.wrapping_sub(v),
                    0x20 => a.wrapping_mul(v),
                    0x30 => {
                        if v == 0 {
                            return 0; // as Linux: division by zero ends the program returning 0
                        }
                        a / v
                    }
                    0x40 => a | v,
                    0x50 => a & v,
                    0x60 => a.checked_shl(v).unwrap_or(0),
                    0x70 => a.checked_shr(v).unwrap_or(0),
                    0x80 => a.wrapping_neg(),
                    0x90 => {
                        if v == 0 {
                            return 0;
                        }
                        a % v
                    }
                    _ => a ^ v,
                };
            }
            JMP => {
                let v = if ins.code & X != 0 { x } else { ins.k };
                let take = match ins.code & 0xf0 {
                    0x00 => {
                        pc += ins.k as usize;
                        continue;
                    }
                    0x10 => a == v,
                    0x20 => a > v,
                    0x30 => a >= v,
                    _ => a & v != 0,
                };
                pc += if take { ins.jt as usize } else { ins.jf as usize };
            }
            RET => return if ins.code & 0x18 == A { a } else { ins.k },
            _ => {
                if ins.code & 0x80 == 0 {
                    x = a; // TAX
                } else {
                    a = x; // TXA
                }
            }
        }
    }
    RET_KILL_PROCESS
}

/// Precedence of an action (higher wins): KILL_PROCESS, KILL_THREAD, TRAP, ERRNO, USER_NOTIF,
/// TRACE, LOG, ALLOW.
fn rank(r: u32) -> u32 {
    match r & RET_ACTION_FULL {
        RET_KILL_PROCESS => 7,
        RET_KILL_THREAD => 6,
        RET_TRAP => 5,
        RET_ERRNO => 4,
        RET_USER_NOTIF => 3,
        RET_TRACE => 2,
        RET_LOG => 1,
        RET_ALLOW => 0,
        _ => 7, // unknown actions kill the process
    }
}

/// What the current thread's filters say about system call `nr`: None to run it, Some(errno) to fail
/// it. Kills the thread or process (SIGSYS) when a filter says so.
pub fn filter_syscall(nr: usize, args: &[usize; 6], pc: usize) -> Option<isize> {
    let th = crate::task::current_thread();
    let mode = th.seccomp_mode.load(core::sync::atomic::Ordering::Relaxed);
    if mode == SECCOMP_MODE_DISABLED {
        return None;
    }
    if mode == SECCOMP_MODE_STRICT {
        // read, write, exit, rt_sigreturn (generic numbers)
        return if matches!(nr, 63 | 64 | 93 | 139) { None } else { kill(true, nr) };
    }
    let f = th.seccomp.lock().clone()?;
    let mut data = [0u8; 64];
    data[0..4].copy_from_slice(&(nr as u32).to_le_bytes());
    data[4..8].copy_from_slice(&AUDIT_ARCH.to_le_bytes());
    data[8..16].copy_from_slice(&(pc as u64).to_le_bytes());
    for (i, a) in args.iter().enumerate() {
        data[16 + 8 * i..24 + 8 * i].copy_from_slice(&(*a as u64).to_le_bytes());
    }
    let mut best = RET_ALLOW;
    let mut cur = Some(f);
    while let Some(x) = cur {
        let r = run(&x.prog, &data);
        if rank(r) > rank(best) {
            best = r;
        }
        cur = x.prev.clone();
    }
    match best & RET_ACTION_FULL {
        RET_ALLOW | RET_LOG => None,
        RET_ERRNO => Some(-((best & RET_DATA).min(4095) as isize)),
        // no tracer and no listener: the system call fails with ENOSYS, as Linux
        RET_TRACE | RET_USER_NOTIF => Some(ENOSYS.neg()),
        RET_TRAP => {
            let mut info = crate::task::signal::SigInfo::kernel(crate::task::signal::SIGSYS);
            info.code = 1; // SYS_SECCOMP
            info.errno = (best & RET_DATA) as i32;
            info.addr = pc;
            info.value = nr; // si_syscall
            crate::task::signal::send_thread(&crate::sched::current_arc(), info);
            Some(ENOSYS.neg())
        }
        RET_KILL_THREAD => kill(false, nr),
        _ => kill(true, nr),
    }
}

fn kill(process: bool, nr: usize) -> ! {
    if crate::syscall::STRACE.load(core::sync::atomic::Ordering::Relaxed) {
        println!("[{}] seccomp: killed at system call {}", crate::sched::current().tid, nr);
    }
    let sig = crate::task::signal::SIGSYS as i32;
    // core dump bit, as a fatal SIGSYS
    crate::task::exit(sig | 0x80, process)
}

/// The current thread's mode (PR_GET_SECCOMP).
pub fn mode() -> u32 {
    crate::task::current_thread().seccomp_mode.load(core::sync::atomic::Ordering::Relaxed)
}

const FLAG_TSYNC: u32 = 1;
const FLAG_LOG: u32 = 2;
const FLAG_SPEC_ALLOW: u32 = 4;
const FLAG_NEW_LISTENER: u32 = 8;
const FLAG_TSYNC_ESRCH: u32 = 16;

/// prctl(PR_SET_SECCOMP, mode, prog) and seccomp(SECCOMP_SET_MODE_*, flags, prog).
pub fn set_mode(mode: u32, flags: u32, prog: usize) -> Result<usize> {
    use core::sync::atomic::Ordering::Relaxed;
    let th = crate::task::current_thread();
    match mode {
        SECCOMP_MODE_STRICT => {
            if flags != 0 || prog != 0 {
                return Err(EINVAL);
            }
            if th.seccomp_mode.load(Relaxed) == SECCOMP_MODE_FILTER {
                return Err(EINVAL);
            }
            th.seccomp_mode.store(SECCOMP_MODE_STRICT, Relaxed);
            Ok(0)
        }
        SECCOMP_MODE_FILTER => {
            if flags & !(FLAG_TSYNC | FLAG_LOG | FLAG_SPEC_ALLOW | FLAG_TSYNC_ESRCH) != 0 {
                return Err(EINVAL); // SECCOMP_FILTER_FLAG_NEW_LISTENER: no user notification support
            }
            if th.seccomp_mode.load(Relaxed) == SECCOMP_MODE_STRICT {
                return Err(EINVAL);
            }
            // without no_new_privs, installing a filter takes CAP_SYS_ADMIN (in the own namespace)
            let cred = th.proc.cred();
            if !cred.no_new_privs && !cred.own_capable(21) {
                return Err(EACCES);
            }
            // struct sock_fprog { unsigned short len; struct sock_filter *filter; }
            let len: u16 = read_user(prog)?;
            let ptr: usize = read_user(prog + 8)?;
            if len == 0 || len as usize > MAX_INSNS {
                return Err(EINVAL);
            }
            let mut v = Vec::with_capacity(len as usize);
            for i in 0..len as usize {
                let raw: [u8; 8] = read_user(ptr + 8 * i)?;
                v.push(Insn { code: u16::from_le_bytes([raw[0], raw[1]]), jt: raw[2], jf: raw[3], k: u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) });
            }
            check(&mut v)?;
            let prev = th.seccomp.lock().clone();
            let f = Arc::new(Filter { prog: v, prev });
            if flags & FLAG_TSYNC != 0 {
                // every thread of the process gets this filter (they must have ours as an ancestor)
                let threads: Vec<_> = th.proc.threads.lock().iter().filter_map(|w| w.upgrade()).collect();
                for t in threads {
                    if let Some(o) = crate::task::thread_of(&t) {
                        *o.seccomp.lock() = Some(f.clone());
                        o.seccomp_mode.store(SECCOMP_MODE_FILTER, Relaxed);
                    }
                }
            } else {
                *th.seccomp.lock() = Some(f);
                th.seccomp_mode.store(SECCOMP_MODE_FILTER, Relaxed);
            }
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

/// seccomp(2)
pub fn seccomp(op: u32, flags: u32, args: usize) -> Result<usize> {
    match op {
        0 => set_mode(SECCOMP_MODE_STRICT, flags, args),
        1 => set_mode(SECCOMP_MODE_FILTER, flags, args),
        2 => {
            // SECCOMP_GET_ACTION_AVAIL
            if flags != 0 {
                return Err(EINVAL);
            }
            let a: u32 = read_user(args)?;
            match a {
                RET_KILL_PROCESS | RET_KILL_THREAD | RET_TRAP | RET_ERRNO | RET_USER_NOTIF | RET_TRACE | RET_LOG | RET_ALLOW => Ok(0),
                _ => Err(EOPNOTSUPP),
            }
        }
        3 => Err(EOPNOTSUPP), // SECCOMP_GET_NOTIF_SIZES: no user notification support
        _ => Err(EINVAL),
    }
}
