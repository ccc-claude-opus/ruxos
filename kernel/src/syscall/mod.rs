//! System calls: the generic Linux table (asm-generic/unistd.h), shared by arm64 and riscv64.
pub mod events;
pub mod fs;
pub mod mm;
pub mod net;
pub mod poll;
pub mod proc;
pub mod time;

use crate::arch::TrapFrame;
use crate::errno::*;

pub mod nr {
    pub const RT_SIGRETURN: usize = 139;
}

/// The name /proc/PID/fd shows for an anon_inode file.
pub fn anon_name(f: &crate::vfs::File) -> &'static str {
    crate::vfs::events::anon_name(f)
}

/// Unimplemented calls that ruwebos's programs probe (logged once per number with ruxos.strace).
fn unknown(n: usize) -> Result<usize> {
    if crate::kernel_param("ruxos.strace").is_some() {
        println!("ruxos: unimplemented system call {}", n);
    }
    Err(ENOSYS)
}

pub fn dispatch(n: usize, a: [usize; 6], f: &mut TrapFrame) -> isize {
    if STRACE.load(core::sync::atomic::Ordering::Relaxed) && crate::kernel_param("ruxos.strace") == Some("entry") {
        println!("[{}] -> {}({:#x}, {:#x}, {:#x})", crate::sched::current().tid, n, a[0], a[1], a[2]);
    }
    let r: Result<usize> = match n {
        17 => fs::getcwd(a[0], a[1]),
        19 => events::eventfd2(a[0] as u32, a[1] as u32),
        20 => events::epoll_create1(a[0] as u32),
        21 => events::epoll_ctl(a[0] as i32, a[1] as u32, a[2] as i32, a[3]),
        22 => events::epoll_pwait(a[0] as i32, a[1], a[2] as i32, a[3] as i32, a[4], a[5]),
        74 => events::signalfd4(a[0] as i32, a[1], a[2], a[3] as u32),
        424 => events::pidfd_send_signal(a[0] as i32, a[1] as u32, a[2], a[3] as u32),
        441 => events::epoll_pwait2(a[0] as i32, a[1], a[2] as i32, a[3], a[4], a[5]),
        23 => fs::dup(a[0] as i32),
        24 => fs::dup3(a[0] as i32, a[1] as i32, a[2] as u32),
        25 => fs::fcntl(a[0] as i32, a[1] as u32, a[2]),
        29 => fs::ioctl(a[0] as i32, a[1] as u32, a[2]),
        32 => fs::flock(a[0] as i32, a[1] as u32),
        33 => fs::mknodat(a[0] as i32, a[1], a[2] as u32, a[3] as u64),
        34 => fs::mkdirat(a[0] as i32, a[1], a[2] as u32),
        35 => fs::unlinkat(a[0] as i32, a[1], a[2] as u32),
        36 => fs::symlinkat(a[0], a[1] as i32, a[2]),
        37 => fs::linkat(a[0] as i32, a[1], a[2] as i32, a[3], a[4] as u32),
        38 => fs::renameat2(a[0] as i32, a[1], a[2] as i32, a[3], 0),
        39 => fs::umount2(a[0], a[1] as u32),
        40 => fs::mount(a[0], a[1], a[2], a[3] as u64, a[4]),
        43 => fs::statfs(a[0], a[1]),
        44 => fs::fstatfs(a[0] as i32, a[1]),
        45 => fs::truncate(a[0], a[1] as i64),
        46 => fs::ftruncate(a[0] as i32, a[1] as i64),
        47 => fs::fallocate(a[0] as i32, a[1] as u32, a[2] as i64, a[3] as i64),
        48 => fs::faccessat(a[0] as i32, a[1], a[2] as u32, 0),
        49 => fs::chdir(a[0]),
        50 => fs::fchdir(a[0] as i32),
        51 => fs::chroot(a[0]),
        52 => fs::fchmod(a[0] as i32, a[1] as u32),
        53 => fs::fchmodat(a[0] as i32, a[1], a[2] as u32, 0),
        54 => fs::fchownat(a[0] as i32, a[1], a[2] as u32, a[3] as u32, a[4] as u32),
        55 => fs::fchown(a[0] as i32, a[1] as u32, a[2] as u32),
        56 => fs::openat(a[0] as i32, a[1], a[2] as u32, a[3] as u32),
        57 => fs::close(a[0] as i32),
        59 => fs::pipe2(a[0], a[1] as u32),
        61 => fs::getdents64(a[0] as i32, a[1], a[2]),
        62 => fs::lseek(a[0] as i32, a[1] as i64, a[2] as u32),
        63 => fs::read(a[0] as i32, a[1], a[2]),
        64 => fs::write(a[0] as i32, a[1], a[2]),
        65 => fs::readv(a[0] as i32, a[1], a[2], None),
        66 => fs::writev(a[0] as i32, a[1], a[2], None),
        67 => fs::pread64(a[0] as i32, a[1], a[2], a[3] as i64),
        68 => fs::pwrite64(a[0] as i32, a[1], a[2], a[3] as i64),
        69 => fs::readv(a[0] as i32, a[1], a[2], Some(a[3] as i64)),
        70 => fs::writev(a[0] as i32, a[1], a[2], Some(a[3] as i64)),
        71 => fs::sendfile(a[0] as i32, a[1] as i32, a[2], a[3]),
        72 => poll::pselect6(a[0], a[1], a[2], a[3], a[4], a[5]),
        73 => poll::ppoll(a[0], a[1], a[2], a[3], a[4]),
        78 => fs::readlinkat(a[0] as i32, a[1], a[2], a[3]),
        79 => fs::newfstatat(a[0] as i32, a[1], a[2], a[3] as u32),
        80 => fs::fstat(a[0] as i32, a[1]),
        81 => fs::sync(),
        82 => fs::fsync(a[0] as i32, false),
        83 => fs::fsync(a[0] as i32, true),
        88 => fs::utimensat(a[0] as i32, a[1], a[2], a[3] as u32),
        90 => proc::capget(a[0], a[1]),
        91 => proc::capset(a[0], a[1]),
        93 => proc::exit(a[0] as i32),
        94 => proc::exit_group(a[0] as i32),
        95 => proc::waitid(a[0] as u32, a[1] as i32, a[2], a[3] as u32, a[4]),
        96 => proc::set_tid_address(a[0]),
        98 => proc::futex(a[0], a[1] as u32, a[2] as u32, a[3], a[4], a[5] as u32),
        99 => proc::set_robust_list(a[0], a[1]),
        100 => proc::get_robust_list(a[0] as i32, a[1], a[2]),
        101 => time::nanosleep(a[0], a[1]),
        102 => time::getitimer(a[0] as u32, a[1]),
        103 => time::setitimer(a[0] as u32, a[1], a[2]),
        112 => time::clock_settime(a[0] as u32, a[1]),
        113 => time::clock_gettime(a[0] as u32, a[1]),
        114 => time::clock_getres(a[0] as u32, a[1]),
        115 => time::clock_nanosleep(a[0] as u32, a[1] as u32, a[2], a[3]),
        116 => Ok(0), // syslog: the kernel log is the console
        118 | 119 => proc::sched_set(a[0] as i32),
        120 => Ok(0), // sched_getscheduler: SCHED_OTHER
        121 => proc::sched_getparam(a[1]),
        122 => Ok(0), // sched_setaffinity: accepted (all CPUs are equal here)
        123 => proc::sched_getaffinity(a[0] as i32, a[1], a[2]),
        124 => {
            crate::sched::yield_now();
            Ok(0)
        }
        125 | 126 => Ok(0), // sched_get_priority_max/min for SCHED_OTHER
        128 => Err(EINTR), // restart_syscall
        129 => proc::kill(a[0] as i32, a[1] as u32),
        130 => proc::tkill(a[0] as i32, a[1] as u32),
        131 => proc::tgkill(a[0] as i32, a[1] as i32, a[2] as u32),
        132 => proc::sigaltstack(a[0], a[1]),
        133 => proc::rt_sigsuspend(a[0], a[1]),
        134 => proc::rt_sigaction(a[0] as u32, a[1], a[2], a[3]),
        135 => proc::rt_sigprocmask(a[0] as u32, a[1], a[2], a[3]),
        136 => proc::rt_sigpending(a[0], a[1]),
        137 => proc::rt_sigtimedwait(a[0], a[1], a[2], a[3]),
        138 => proc::rt_sigqueueinfo(a[0] as i32, a[1] as u32, a[2]),
        139 => crate::task::signal::sigreturn(f),
        140 => Ok(0), // setpriority
        141 => Ok(20), // getpriority: nice 0
        142 => proc::reboot(a[0] as u32, a[1] as u32, a[2] as u32),
        143 => proc::setregid(a[0] as u32, a[1] as u32),
        144 => proc::setgid(a[0] as u32),
        145 => proc::setreuid(a[0] as u32, a[1] as u32),
        146 => proc::setuid(a[0] as u32),
        147 => proc::setresuid(a[0] as u32, a[1] as u32, a[2] as u32),
        148 => proc::getresuid(a[0], a[1], a[2]),
        149 => proc::setresgid(a[0] as u32, a[1] as u32, a[2] as u32),
        150 => proc::getresgid(a[0], a[1], a[2]),
        151 => proc::setfsuid(a[0] as u32),
        152 => proc::setfsgid(a[0] as u32),
        153 => time::times(a[0]),
        154 => proc::setpgid(a[0] as i32, a[1] as i32),
        155 => proc::getpgid(a[0] as i32),
        156 => proc::getsid(a[0] as i32),
        157 => proc::setsid(),
        158 => proc::getgroups(a[0], a[1]),
        159 => proc::setgroups(a[0], a[1]),
        160 => proc::uname(a[0]),
        161 => proc::sethostname(a[0], a[1]),
        162 => proc::setdomainname(a[0], a[1]),
        163 => proc::prlimit64(0, a[0] as u32, 0, a[1]),
        164 => proc::prlimit64(0, a[0] as u32, a[1], 0),
        165 => proc::getrusage(a[0] as i32, a[1]),
        166 => fs::umask(a[0] as u32),
        167 => proc::prctl(a[0] as u32, a[1], a[2], a[3], a[4]),
        168 => proc::getcpu(a[0], a[1]),
        169 => time::gettimeofday(a[0], a[1]),
        170 => time::settimeofday(a[0], a[1]),
        172 => Ok(crate::task::vpid(crate::task::current_proc().pid) as usize),
        173 => proc::getppid(),
        174 => proc::getuid(),
        175 => proc::geteuid(),
        176 => proc::getgid(),
        177 => proc::getegid(),
        178 => Ok(crate::task::vpid(crate::sched::current().tid) as usize),
        97 => proc::unshare(a[0] as u64),
        41 => fs::pivot_root(a[0], a[1]),
        // extended attributes: not supported by these file systems (EOPNOTSUPP), none to list
        5..=10 | 14..=16 => Err(EOPNOTSUPP),
        11..=13 => Ok(0),
        437 => fs::openat2(a[0] as i32, a[1], a[2], a[3]),
        277 => crate::seccomp::seccomp(a[0] as u32, a[1] as u32, a[2]),
        179 => proc::sysinfo(a[0]),
        198 => net::socket(a[0] as u32, a[1] as u32, a[2] as i32),
        199 => net::socketpair(a[0] as u32, a[1] as u32, a[2] as i32, a[3]),
        200 => net::bind(a[0] as i32, a[1], a[2] as u32),
        201 => net::listen(a[0] as i32, a[1] as i32),
        202 => net::accept4(a[0] as i32, a[1], a[2], 0),
        203 => net::connect(a[0] as i32, a[1], a[2] as u32),
        204 => net::getsockname(a[0] as i32, a[1], a[2]),
        205 => net::getpeername(a[0] as i32, a[1], a[2]),
        206 => net::sendto(a[0] as i32, a[1], a[2], a[3] as u32, a[4], a[5] as u32),
        207 => net::recvfrom(a[0] as i32, a[1], a[2], a[3] as u32, a[4], a[5]),
        208 => net::setsockopt(a[0] as i32, a[1] as i32, a[2] as i32, a[3], a[4] as u32),
        209 => net::getsockopt(a[0] as i32, a[1] as i32, a[2] as i32, a[3], a[4]),
        210 => net::shutdown(a[0] as i32, a[1] as u32),
        211 => net::sendmsg(a[0] as i32, a[1], a[2] as u32),
        212 => net::recvmsg(a[0] as i32, a[1], a[2] as u32),
        213 => Ok(0), // readahead
        214 => mm::brk(a[0]),
        215 => mm::munmap(a[0], a[1]),
        216 => mm::mremap(a[0], a[1], a[2], a[3] as u32, a[4]),
        220 => proc::clone(a[0] as u64, a[1], a[2], a[3], a[4], f),
        221 => proc::execve(-100, a[0], a[1], a[2], 0, f),
        222 => mm::mmap(a[0], a[1], a[2] as u32, a[3] as u32, a[4] as i32, a[5] as u64),
        223 => Ok(0), // fadvise64
        226 => mm::mprotect(a[0], a[1], a[2] as u32),
        227 => mm::msync(a[0], a[1], a[2] as u32),
        228..=231 | 284 => Ok(0), // mlock family: no swapping here, memory is always resident
        232 => mm::mincore(a[0], a[1], a[2]),
        233 => mm::madvise(a[0], a[1], a[2] as u32),
        242 => net::accept4(a[0] as i32, a[1], a[2], a[3] as u32),
        243 => net::recvmmsg(a[0] as i32, a[1], a[2] as u32, a[3] as u32, a[4]),
        240 => proc::rt_tgsigqueueinfo(a[0] as i32, a[1] as i32, a[2] as u32, a[3]),
        258 => Err(ENOSYS), // riscv_hwprobe: glibc falls back to AT_HWCAP
        #[cfg(target_arch = "riscv64")]
        259 => {
            crate::arch::sync_icache(0, 0);
            Ok(0)
        }
        260 => proc::wait4(a[0] as i32, a[1], a[2] as u32, a[3]),
        261 => proc::prlimit64(a[0] as i32, a[1] as u32, a[2], a[3]),
        267 => fs::sync(),
        276 => fs::renameat2(a[0] as i32, a[1], a[2] as i32, a[3], a[4] as u32),
        278 => proc::getrandom(a[0], a[1], a[2] as u32),
        281 => proc::execve(a[0] as i32, a[1], a[2], a[3], a[4] as u32, f),
        269 => net::sendmmsg(a[0] as i32, a[1], a[2] as u32, a[3] as u32),
        283 => proc::membarrier(a[0] as u32),
        285 => Err(EXDEV), // copy_file_range: callers fall back to read/write
        291 => fs::statx(a[0] as i32, a[1], a[2] as u32, a[3] as u32, a[4]),
        293 => Err(ENOSYS), // rseq: optional for glibc
        425..=427 => Err(ENOSYS), // io_uring: libuv falls back to epoll
        434 => events::pidfd_open(a[0] as i32, a[1] as u32),
        435 => Err(ENOSYS), // clone3: glibc falls back to clone
        436 => fs::close_range(a[0] as u32, a[1] as u32, a[2] as u32),
        439 => fs::faccessat(a[0] as i32, a[1], a[2] as u32, a[3] as u32),
        452 => fs::fchmodat(a[0] as i32, a[1], a[2] as u32, a[3] as u32),
        _ => unknown(n),
    };
    let ret = match r {
        Ok(v) => v as isize,
        Err(e) => e.neg(),
    };
    if STRACE.load(core::sync::atomic::Ordering::Relaxed) {
        println!("[{}] {}({:#x}, {:#x}, {:#x}, {:#x}) = {}", crate::sched::current().tid, n, a[0], a[1], a[2], a[3], ret);
    }
    ret
}

/// ruxos.strace on the kernel command line: print every system call.
pub static STRACE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
