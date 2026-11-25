//! /proc: processes (stat, status, maps, cmdline, environ, fd, exe, cwd, task...), system
//! information (meminfo, stat, cpuinfo, uptime, mounts...) and /proc/sys.
use crate::errno::*;
use crate::mm::frame;
use crate::task::{self, Process, PROCS};
use crate::vfs::pseudo::{self, dir, file, ino_of, link, text, Gen, Lister};
use crate::vfs::*;
use crate::sync::SpinLock;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::Ordering;

/// A proc instance shows the processes of the PID namespace of whoever mounted it.
pub fn new(ns: Arc<crate::ns::PidNs>) -> Arc<dyn FileSystem> {
    let fs = Arc::new(pseudo::PseudoFs { name: "proc", dev: tmpfs::anon_dev(), magic: 0x9fa0, root: SpinLock::new(None) });
    let dev = fs.dev;
    *fs.root.lock() = Some(dir(dev, 1, 0o555, root_lister(dev, ns)));
    fs
}

fn f(dev: u64, path: &[&[u8]], g: Gen) -> Arc<dyn Inode> {
    file(dev, ino_of(path), 0o444, g, None)
}

fn entry(name: &str, i: Arc<dyn Inode>) -> (Vec<u8>, Arc<dyn Inode>) {
    (name.as_bytes().to_vec(), i)
}

fn kb(pages: usize) -> usize {
    pages * 4
}

fn root_lister(dev: u64, ns: Arc<crate::ns::PidNs>) -> Lister {
    alloc::boxed::Box::new(move || {
        let mut v: Vec<(Vec<u8>, Arc<dyn Inode>)> = Vec::new();
        let (n1, n2) = (ns.clone(), ns.clone());
        v.push(entry("self", link(dev, 2, alloc::boxed::Box::new(move || {
            let p = n1.local_of(task::current_proc().pid);
            if p == 0 { Err(ENOENT) } else { text(format!("{}", p)) }
        }))));
        v.push(entry(
            "thread-self",
            link(dev, 3, alloc::boxed::Box::new(move || {
                let (p, t) = (n2.local_of(task::current_proc().pid), n2.local_of(crate::sched::current().tid));
                if p == 0 { Err(ENOENT) } else { text(format!("{}/task/{}", p, t)) }
            })),
        ));
        v.push(entry("cmdline", f(dev, &[b"cmdline"], alloc::boxed::Box::new(|| text(format!("{}\n", crate::CMDLINE.get().copied().unwrap_or("")))))));
        v.push(entry("version", f(dev, &[b"version"], alloc::boxed::Box::new(|| text(format!("Linux version {} (ruxos) {}\n", crate::RELEASE, crate::VERSION))))));
        v.push(entry("uptime", f(dev, &[b"uptime"], alloc::boxed::Box::new(|| {
            let ns = crate::time::monotonic_ns();
            text(format!("{}.{:02} {}.{:02}\n", ns / 1_000_000_000, (ns / 10_000_000) % 100, ns / 1_000_000_000, (ns / 10_000_000) % 100))
        }))));
        v.push(entry("loadavg", f(dev, &[b"loadavg"], alloc::boxed::Box::new(|| {
            let n = PROCS.lock().len();
            text(format!("0.00 0.00 0.00 1/{} {}\n", n, crate::sched::current().tid))
        }))));
        v.push(entry("meminfo", f(dev, &[b"meminfo"], alloc::boxed::Box::new(meminfo))));
        v.push(entry("stat", f(dev, &[b"stat"], alloc::boxed::Box::new(stat))));
        v.push(entry("cpuinfo", f(dev, &[b"cpuinfo"], alloc::boxed::Box::new(cpuinfo))));
        v.push(entry("filesystems", f(dev, &[b"filesystems"], alloc::boxed::Box::new(|| {
            text(String::from("nodev\tsysfs\nnodev\ttmpfs\nnodev\tproc\nnodev\tdevtmpfs\nnodev\tdevpts\nnodev\tcgroup2\nnodev\tramfs\n\text4\n\text3\n\text2\n"))
        }))));
        v.push(entry("partitions", f(dev, &[b"partitions"], alloc::boxed::Box::new(partitions))));
        v.push(entry("mounts", link(dev, 4, alloc::boxed::Box::new(|| text(String::from("self/mounts"))))));
        v.push(entry("sys", dir(dev, ino_of(&[b"sys"]), 0o555, sys_lister(dev))));
        v.push(entry("net", link(dev, 5, alloc::boxed::Box::new(|| text(String::from("self/net"))))));
        // the processes of this instance's PID namespace (and the ones below it), by their pid here
        let procs: Vec<Arc<Process>> = PROCS.lock().values().filter_map(|w| w.upgrade()).filter(|p| ns.contains(&p.pid_ns)).collect();
        for p in procs {
            let local = ns.local_of(p.pid);
            if local != 0 {
                v.push((format!("{}", local).into_bytes(), pid_dir(dev, p.pid, None)));
            }
        }
        v
    })
}

fn meminfo() -> Result<Vec<u8>> {
    let total = frame::TOTAL_PAGES.load(Ordering::Relaxed);
    let free = frame::free_count();
    let mut s = String::new();
    let _ = write!(s, "MemTotal:       {:8} kB\nMemFree:        {:8} kB\nMemAvailable:   {:8} kB\n", kb(total), kb(free), kb(free));
    for k in ["Buffers", "Cached", "SwapCached", "Active", "Inactive", "SwapTotal", "SwapFree", "Dirty", "Writeback", "Shmem", "Slab"] {
        let _ = writeln!(s, "{:<16}{:8} kB", format!("{}:", k), 0);
    }
    let _ = writeln!(s, "CommitLimit:    {:8} kB\nCommitted_AS:   {:8} kB", kb(total), kb(total - free));
    text(s)
}

fn stat() -> Result<Vec<u8>> {
    let mut s = String::new();
    let n = crate::percpu::ncpus();
    let up = crate::time::monotonic_ns() / 10_000_000; // clock ticks (100 Hz)
    let _ = writeln!(s, "cpu  0 0 0 {} 0 0 0 0 0 0", up * n as u64);
    for c in 0..n {
        let _ = writeln!(s, "cpu{} 0 0 0 {} 0 0 0 0 0 0", c, up);
    }
    let boot = (crate::time::realtime_ns() - crate::time::monotonic_ns() as i64) / 1_000_000_000;
    let _ = write!(s, "intr 0\nctxt 0\nbtime {}\nprocesses {}\nprocs_running 1\nprocs_blocked 0\nsoftirq 0\n", boot, PROCS.lock().len());
    text(s)
}

fn cpuinfo() -> Result<Vec<u8>> {
    let mut s = String::new();
    for c in 0..crate::percpu::ncpus() {
        if cfg!(target_arch = "aarch64") {
            let _ = write!(s, "processor\t: {}\nBogoMIPS\t: {}\nFeatures\t: fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm jscvt fcma lrcpc dcpop sha3 asimddp sha512 asimdfhm\nCPU implementer\t: 0x61\nCPU architecture: 8\nCPU variant\t: 0x0\nCPU part\t: 0x000\nCPU revision\t: 0\n\n", c, crate::time::freq() / 500_000);
        } else {
            let _ = write!(s, "processor\t: {}\nhart\t\t: {}\nisa\t\t: rv64imafdc\nmmu\t\t: sv39\n\n", c, c);
        }
    }
    text(s)
}

fn partitions() -> Result<Vec<u8>> {
    let mut s = String::from("major minor  #blocks  name\n\n");
    for i in 0..8u32 {
        let d = makedev(254, 16 * i);
        if let Some(b) = dev::block_device(d) {
            let _ = writeln!(s, " {:4} {:7} {:10} vd{}", 254, 16 * i, b.sectors() / 2, (b'a' + i as u8) as char);
        }
    }
    text(s)
}

fn sys_lister(dev: u64) -> Lister {
    alloc::boxed::Box::new(move || {
        let kernel = dir(dev, ino_of(&[b"sys", b"kernel"]), 0o555, alloc::boxed::Box::new(move || {
            let mut v = Vec::new();
            v.push(entry("hostname", file(dev, ino_of(&[b"sys", b"kernel", b"hostname"]), 0o644,
                alloc::boxed::Box::new(|| {
                    let mut h = crate::syscall::proc::hostname();
                    h.push(b'\n');
                    Ok(h)
                }),
                Some(alloc::boxed::Box::new(|d: &[u8]| {
                    let mut h = d.to_vec();
                    while h.last().is_some_and(|c| *c == b'\n') {
                        h.pop();
                    }
                    crate::syscall::proc::set_hostname(h)
                })))));
            let consts: [(&str, String); 11] = [
                ("osrelease", String::from(crate::RELEASE)),
                ("ostype", String::from("Linux")),
                ("version", String::from(crate::VERSION)),
                ("pid_max", String::from("4194304")),
                ("threads-max", String::from("63000")),
                ("overflowuid", String::from("65534")),
                ("overflowgid", String::from("65534")),
                ("ngroups_max", String::from("65536")),
                ("cap_last_cap", String::from("40")),
                ("domainname", String::from("(none)")),
                ("panic", String::from("0")),
            ];
            for (n, val) in consts {
                v.push(entry(n, f(dev, &[b"sys", b"kernel", n.as_bytes()], alloc::boxed::Box::new(move || text(format!("{}\n", val))))));
            }
            v.push(entry("random", dir(dev, ino_of(&[b"sys", b"kernel", b"random"]), 0o555, alloc::boxed::Box::new(move || {
                alloc::vec![
                    entry("boot_id", f(dev, &[b"boot_id"], alloc::boxed::Box::new(|| text(boot_id())))),
                    entry("uuid", f(dev, &[b"uuid"], alloc::boxed::Box::new(|| text(uuid())))),
                    entry("entropy_avail", f(dev, &[b"entropy_avail"], alloc::boxed::Box::new(|| text(String::from("256\n"))))),
                    entry("poolsize", f(dev, &[b"poolsize"], alloc::boxed::Box::new(|| text(String::from("256\n"))))),
                ]
            }))));
            v
        }));
        let simple = |path: &'static [&'static str], items: &'static [(&'static str, &'static str)]| -> (Vec<u8>, Arc<dyn Inode>) {
            let p: Vec<&[u8]> = path.iter().map(|s| s.as_bytes()).collect();
            let d = dir(dev, ino_of(&p), 0o555, alloc::boxed::Box::new(move || {
                items.iter().map(|(n, val)| entry(n, f(dev, &[path[0].as_bytes(), n.as_bytes()], alloc::boxed::Box::new(move || text(format!("{}\n", val)))))).collect()
            }));
            (path.last().unwrap().as_bytes().to_vec(), d)
        };
        alloc::vec![
            (b"kernel".to_vec(), kernel),
            (b"user".to_vec(), dir(dev, ino_of(&[b"sys", b"user"]), 0o555, alloc::boxed::Box::new(move || {
                // per user namespace: the caller's (user_namespaces(7)); writable by its CAP_SYS_RESOURCE
                let mut v = alloc::vec![entry("max_user_namespaces", pseudo::file_mode_fn(dev, ino_of(&[b"sys", b"user", b"max_user_namespaces"]), userns_sysctl_mode,
                    alloc::boxed::Box::new(|| text(format!("{}\n", task::current_proc().cred().user_ns.max_user_namespaces.load(Ordering::Relaxed)))),
                    Some(alloc::boxed::Box::new(|d: &[u8]| {
                        let c = task::current_proc().cred();
                        if !c.own_capable(24) {
                            return Err(EPERM);
                        }
                        let n: u32 = core::str::from_utf8(d).map_err(|_| EINVAL)?.trim().parse().map_err(|_| EINVAL)?;
                        if n > i32::MAX as u32 {
                            return Err(EINVAL);
                        }
                        c.user_ns.max_user_namespaces.store(n, Ordering::Relaxed);
                        Ok(())
                    }))))];
                for n in ["max_pid_namespaces", "max_net_namespaces", "max_mnt_namespaces", "max_uts_namespaces", "max_ipc_namespaces", "max_cgroup_namespaces"] {
                    v.push(entry(n, f(dev, &[b"sys", b"user", n.as_bytes()], alloc::boxed::Box::new(|| text(String::from("15000\n"))))));
                }
                v
            }))),
            simple(&["sys", "vm"], &[("overcommit_memory", "0"), ("max_map_count", "65530"), ("mmap_min_addr", "4096"), ("swappiness", "60")]),
            simple(&["sys", "fs"], &[("file-max", "9223372036854775807"), ("nr_open", "1048576"), ("pipe-max-size", "1048576")]),
            (b"net".to_vec(), dir(dev, ino_of(&[b"sys", b"net"]), 0o555, alloc::boxed::Box::new(move || {
                let core = dir(dev, ino_of(&[b"sys", b"net", b"core"]), 0o555, alloc::boxed::Box::new(move || {
                    alloc::vec![entry("somaxconn", f(dev, &[b"somaxconn"], alloc::boxed::Box::new(|| text(String::from("4096\n")))))]
                }));
                let ipv4 = dir(dev, ino_of(&[b"sys", b"net", b"ipv4"]), 0o555, alloc::boxed::Box::new(move || {
                    alloc::vec![entry("ip_local_port_range", f(dev, &[b"ip_local_port_range"], alloc::boxed::Box::new(|| text(String::from("32768\t60999\n")))))]
                }));
                alloc::vec![entry("core", core), entry("ipv4", ipv4)]
            }))),
        ]
    })
}

fn uuid_from(b: [u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-4{:01x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\n",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6] & 0xf, b[7], (b[8] & 0x3f) | 0x80, b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

fn uuid() -> String {
    let mut b = [0u8; 16];
    crate::random::fill(&mut b);
    uuid_from(b)
}

static BOOT_ID: crate::sync::Once<[u8; 16]> = crate::sync::Once::new();

fn boot_id() -> String {
    if BOOT_ID.get().is_none() {
        let mut b = [0u8; 16];
        crate::random::fill(&mut b);
        BOOT_ID.set(b);
    }
    uuid_from(*BOOT_ID)
}

fn proc_of(pid: u32) -> Result<Arc<Process>> {
    task::proc_by_pid(pid).ok_or(ENOENT)
}

/// A thread of the process (the leader, or `tid`) with its user state.
fn thread_of(pid: u32, tid: Option<u32>) -> Result<Arc<crate::sched::Task>> {
    let p = proc_of(pid)?;
    let want = tid.unwrap_or(pid);
    let ts: Vec<_> = p.threads.lock().iter().filter_map(|w| w.upgrade()).collect();
    ts.iter().find(|t| t.tid == want).or(ts.first()).cloned().ok_or(ENOENT)
}

fn pid_dir(dev: u64, pid: u32, tid: Option<u32>) -> Arc<dyn Inode> {
    let key = format!("{}/{}", pid, tid.unwrap_or(0));
    pseudo::dir_owned(dev, ino_of(&[key.as_bytes()]), 0o555, alloc::boxed::Box::new(move || pid_entries(dev, pid, tid)), pid)
}

fn pid_entries(dev: u64, pid: u32, tid: Option<u32>) -> Vec<(Vec<u8>, Arc<dyn Inode>)> {
    let k = format!("{}/{}", pid, tid.unwrap_or(0));
    let kb_ = |n: &str| ino_of(&[k.as_bytes(), n.as_bytes()]);
    let generate = |g: fn(u32, Option<u32>) -> Result<Vec<u8>>| -> Gen { alloc::boxed::Box::new(move || g(pid, tid)) };
    // the files belong to the process (its effective ids), as Linux's /proc/PID
    let fo = |ino: u64, mode: u32, g: Gen, w: Option<pseudo::Writer>| pseudo::file_owned(dev, ino, mode, g, w, pid);
    let mut v = alloc::vec![
        entry("stat", fo(kb_("stat"), 0o444, generate(pid_stat), None)),
        entry("statm", fo(kb_("statm"), 0o444, generate(pid_statm), None)),
        entry("status", fo(kb_("status"), 0o444, generate(pid_status), None)),
        entry("maps", fo(kb_("maps"), 0o444, generate(pid_maps), None)),
        entry("cmdline", fo(kb_("cmdline"), 0o444, generate(pid_cmdline), None)),
        entry("environ", fo(kb_("environ"), 0o400, generate(pid_environ), None)),
        entry("auxv", fo(kb_("auxv"), 0o400, generate(pid_auxv), None)),
        entry("cgroup", fo(kb_("cgroup"), 0o444, alloc::boxed::Box::new(move || Ok(crate::fs::cgroupfs::proc_cgroup(&*proc_of(pid)?))), None)),
        entry("mountinfo", fo(kb_("mountinfo"), 0o444, generate(|p, _| mounts(p, true)), None)),
        entry("mounts", fo(kb_("mounts"), 0o444, generate(|p, _| mounts(p, false)), None)),
        entry("limits", fo(kb_("limits"), 0o444, generate(pid_limits), None)),
        entry("oom_score_adj", fo(kb_("oom_score_adj"), 0o644, alloc::boxed::Box::new(|| text(String::from("0\n"))), Some(alloc::boxed::Box::new(|_| Ok(()))))),
        entry("syscall", fo(kb_("syscall"), 0o400, generate(pid_syscall), None)),
        // user namespaces (user_namespaces(7))
        entry("uid_map", fo(kb_("uid_map"), 0o644, generate(|p, t| id_map(p, t, false)), Some(alloc::boxed::Box::new(move |d: &[u8]| write_id_map(pid, tid, false, d))))),
        entry("gid_map", fo(kb_("gid_map"), 0o644, generate(|p, t| id_map(p, t, true)), Some(alloc::boxed::Box::new(move |d: &[u8]| write_id_map(pid, tid, true, d))))),
        entry("setgroups", fo(kb_("setgroups"), 0o644, generate(setgroups_read), Some(alloc::boxed::Box::new(move |d: &[u8]| setgroups_write(pid, tid, d))))),
        entry("ns", pseudo::dir_owned(dev, kb_("ns"), 0o511, alloc::boxed::Box::new(move || ns_entries(dev, pid, tid)), pid)),
        entry("wchan", fo(kb_("wchan"), 0o444, generate(pid_wchan), None)),
        entry("comm", fo(kb_("comm"), 0o644, generate(pid_comm), Some(alloc::boxed::Box::new(move |d: &[u8]| {
            let t = thread_of(pid, tid)?;
            let th = task::thread_of(&t).ok_or(ESRCH)?;
            let mut n = [0u8; 16];
            let s: Vec<u8> = d.iter().copied().take_while(|c| *c != b'\n').take(15).collect();
            n[..s.len()].copy_from_slice(&s);
            *th.comm.lock() = n;
            Ok(())
        })))),
        // exe, cwd, root: magic links (path walking goes to the object itself)
        entry("exe", pseudo::magic_link(dev, kb_("exe"), alloc::boxed::Box::new(move || {
            let t = thread_of(pid, tid)?;
            let th = task::thread_of(&t).ok_or(ENOENT)?;
            let exe = th.mm.inner.lock().exe.clone().ok_or(ENOENT)?;
            Ok(exe.path_from(&reader_root()))
        }), alloc::boxed::Box::new(move || {
            let t = thread_of(pid, tid)?;
            let th = task::thread_of(&t).ok_or(ENOENT)?;
            th.mm.inner.lock().exe.clone().ok_or(ENOENT)
        }), pid)),
        entry("cwd", pseudo::magic_link(dev, kb_("cwd"), alloc::boxed::Box::new(move || {
            let t = thread_of(pid, tid)?;
            let th = task::thread_of(&t).ok_or(ENOENT)?;
            let cwd = th.fs.lock().cwd.clone();
            Ok(cwd.path_from(&reader_root()))
        }), alloc::boxed::Box::new(move || {
            let t = thread_of(pid, tid)?;
            let th = task::thread_of(&t).ok_or(ENOENT)?;
            let cwd = th.fs.lock().cwd.clone();
            Ok(cwd)
        }), pid)),
        entry("root", pseudo::magic_link(dev, kb_("root"), alloc::boxed::Box::new(move || {
            let t = thread_of(pid, tid)?;
            let th = task::thread_of(&t).ok_or(ENOENT)?;
            let r = th.fs.lock().root.clone();
            Ok(r.path_from(&reader_root()))
        }), alloc::boxed::Box::new(move || {
            let t = thread_of(pid, tid)?;
            let th = task::thread_of(&t).ok_or(ENOENT)?;
            let r = th.fs.lock().root.clone();
            Ok(r)
        }), pid)),
        entry("fd", pseudo::dir_owned(dev, kb_("fd"), 0o500, alloc::boxed::Box::new(move || fd_entries(dev, pid, tid)), pid)),
        entry("net", link(dev, kb_("net"), alloc::boxed::Box::new(|| Ok(b"/sys/class/net".to_vec())))),
    ];
    if tid.is_none() {
        v.push(entry("task", dir(dev, kb_("task"), 0o555, alloc::boxed::Box::new(move || {
            let Ok(p) = proc_of(pid) else { return Vec::new() };
            let ts: Vec<u32> = p.threads.lock().iter().filter_map(|w| w.upgrade()).map(|t| t.tid).collect();
            ts.into_iter().map(|t| (format!("{}", task::vpid(t)).into_bytes(), pid_dir(dev, pid, Some(t)))).collect()
        }))));
    }
    v
}

fn fd_entries(dev: u64, pid: u32, tid: Option<u32>) -> Vec<(Vec<u8>, Arc<dyn Inode>)> {
    let Ok(t) = thread_of(pid, tid) else { return Vec::new() };
    let Some(th) = task::thread_of(&t) else { return Vec::new() };
    let fds = th.files.lock().clone();
    let mut v = Vec::new();
    for (i, e) in fds.fds.iter().enumerate() {
        let Some(e) = e else { continue };
        let (f1, f2) = (e.file.clone(), e.file.clone());
        // a magic link: opening it reaches the open file's object, wherever it is
        v.push((
            format!("{}", i).into_bytes(),
            pseudo::magic_link(
                dev,
                ino_of(&[b"fd", format!("{}/{}", pid, i).as_bytes()]),
                alloc::boxed::Box::new(move || Ok(fd_target(&f1, &reader_root()))),
                alloc::boxed::Box::new(move || f2.path.clone().ok_or(ENOENT)),
                pid,
            ),
        ));
    }
    v
}

/// The root of whoever reads a link (paths are shown relative to it, as d_path).
fn reader_root() -> PathRef {
    task::current_thread().fs.lock().root.clone()
}

/// What /proc/PID/fd/N points to.
pub fn fd_target(f: &File, root: &PathRef) -> Vec<u8> {
    if let Some(p) = &f.path {
        return p.path_from(root);
    }
    match f.inode().and_then(|i| i.stat().ok()) {
        Some(st) if st.mode & S_IFMT == S_IFIFO => format!("pipe:[{}]", st.ino).into_bytes(),
        Some(st) if st.mode & S_IFMT == S_IFSOCK => format!("socket:[{}]", st.ino).into_bytes(),
        _ => format!("anon_inode:[{}]", crate::syscall::anon_name(f)).into_bytes(),
    }
}

fn comm(t: &crate::sched::Task) -> String {
    let c = task::thread_of(t).map(|th| *th.comm.lock()).unwrap_or_default();
    let n = c.iter().position(|&b| b == 0).unwrap_or(16);
    String::from_utf8_lossy(&c[..n]).into_owned()
}

fn pid_comm(pid: u32, tid: Option<u32>) -> Result<Vec<u8>> {
    let t = thread_of(pid, tid)?;
    text(format!("{}\n", comm(&t)))
}

/// The trap frame of a thread that is not running (it sits on its kernel stack), and whether the
/// thread is inside a system call.
fn parked_frame(t: &Arc<crate::sched::Task>) -> Option<(crate::arch::TrapFrame, usize, bool)> {
    if t.state.load(core::sync::atomic::Ordering::Acquire) != crate::sched::SLEEPING {
        return None;
    }
    let th = task::thread_of(t)?;
    // SAFETY: a sleeping thread does not change its own state; the frame lives on its kernel stack
    // as long as the task exists (we hold a reference).
    let own = unsafe { &*th.own.get() };
    if own.frame.is_null() {
        return None;
    }
    Some((unsafe { (*own.frame).clone() }, own.orig_arg0, own.cur_syscall >= 0))
}

/// /proc/PID/task/TID/syscall: "nr arg1..arg6 sp pc" while blocked in a system call, "-1 sp pc"
/// otherwise, "running" for a thread on a CPU or ready to run.
fn pid_syscall(pid: u32, tid: Option<u32>) -> Result<Vec<u8>> {
    let t = thread_of(pid, tid)?;
    let Some((f, a0, in_sys)) = parked_frame(&t) else { return text(String::from("running\n")) };
    let (sp, pc) = (crate::arch::frame_sp(&f), crate::arch::frame_pc(&f));
    if !in_sys {
        return text(format!("-1 {:#x} {:#x}\n", sp, pc));
    }
    let mut a = crate::arch::syscall_args(&f);
    a[0] = a0;
    text(format!("{} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x}\n", crate::arch::syscall_nr(&f), a[0], a[1], a[2], a[3], a[4], a[5], sp, pc))
}

/// /proc/PID/task/TID/wchan: where a sleeping thread waits, named after the Linux function that
/// waits for the same thing; "0" when it is not sleeping.
fn pid_wchan(pid: u32, tid: Option<u32>) -> Result<Vec<u8>> {
    let t = thread_of(pid, tid)?;
    let Some((f, _, in_sys)) = parked_frame(&t) else { return text(String::from("0")) };
    let name = if !in_sys {
        "do_signal_stop"
    } else {
        match crate::arch::syscall_nr(&f) {
            98 => "futex_wait_queue",
            22 => "do_epoll_wait",
            72 => "do_select",
            73 => "do_sys_poll",
            63 | 65 => "wait_woken",
            64 | 66 => "wait_woken",
            101 | 115 => "hrtimer_nanosleep",
            202 | 242 => "inet_csk_accept",
            203 => "inet_wait_for_connect",
            206 | 207 | 211 | 212 => "wait_woken",
            260 => "do_wait",
            133 => "sigsuspend",
            _ => "schedule",
        }
    };
    text(String::from(name))
}

/// /proc/sys/user: read-write for whoever has CAP_SYS_RESOURCE over its user namespace (the
/// caller's), read-only for the others (Linux's set_permissions in kernel/ucount.c).
fn userns_sysctl_mode() -> u32 {
    if task::current_proc().cred().own_capable(24) { 0o666 } else { 0o444 }
}

fn user_ns_of(pid: u32) -> Result<Arc<crate::ns::UserNs>> {
    Ok(proc_of(pid)?.cred().user_ns.clone())
}

fn id_map(pid: u32, _tid: Option<u32>, gid: bool) -> Result<Vec<u8>> {
    let ns = user_ns_of(pid)?;
    let reader = task::current_proc().cred().user_ns.clone();
    Ok(crate::ns::read_map(&ns, gid, &reader))
}

fn write_id_map(pid: u32, _tid: Option<u32>, gid: bool, d: &[u8]) -> Result<()> {
    let ns = user_ns_of(pid)?;
    crate::ns::write_map(&ns, gid, d, &task::current_proc().cred())
}

fn setgroups_read(pid: u32, _tid: Option<u32>) -> Result<Vec<u8>> {
    let ns = user_ns_of(pid)?;
    text(String::from(if ns.setgroups_allowed.load(Ordering::Relaxed) { "allow\n" } else { "deny\n" }))
}

/// /proc/PID/setgroups: "deny" before gid_map is written (and only by someone who could write it);
/// "allow" is only accepted while it still is.
fn setgroups_write(pid: u32, _tid: Option<u32>, d: &[u8]) -> Result<()> {
    let ns = user_ns_of(pid)?;
    let w = core::str::from_utf8(d).map_err(|_| EINVAL)?.trim();
    let allow = match w {
        "allow" => true,
        "deny" => false,
        _ => return Err(EINVAL),
    };
    let cred = task::current_proc().cred();
    let parent = ns.parent.clone().ok_or(EPERM)?;
    if !(Arc::ptr_eq(&cred.user_ns, &ns) || Arc::ptr_eq(&cred.user_ns, &parent)) {
        return Err(EPERM);
    }
    if allow {
        if !ns.setgroups_allowed.load(Ordering::Relaxed) {
            return Err(EPERM);
        }
    } else {
        if ns.gid_map.lock().is_some() {
            return Err(EPERM);
        }
        ns.setgroups_allowed.store(false, Ordering::Relaxed);
    }
    Ok(())
}

/// /proc/PID/ns: "type:[inode]" links (namespaces(7)).
fn ns_entries(dev: u64, pid: u32, tid: Option<u32>) -> Vec<(Vec<u8>, Arc<dyn Inode>)> {
    let Ok(p) = proc_of(pid) else { return Vec::new() };
    let n = p.ns.lock().clone();
    let k = format!("{}/{}/ns", pid, tid.unwrap_or(0));
    let items: [(&str, u64); 9] = [
        ("cgroup", n.cgroup.ino),
        ("ipc", n.ipc.ino),
        ("mnt", n.mnt.ino),
        ("net", n.net.ino),
        ("pid", p.pid_ns.ino),
        ("pid_for_children", n.pid_for_children.ino),
        ("time", crate::ns::INO_TIME),
        ("time_for_children", crate::ns::INO_TIME),
        ("user", p.cred().user_ns.ino),
    ];
    items
        .iter()
        .map(|(name, ino)| {
            let kind = if name.starts_with("pid") { "pid" } else if name.starts_with("time") { "time" } else { name };
            let s = format!("{}:[{}]", kind, ino);
            (name.as_bytes().to_vec(), link(dev, ino_of(&[k.as_bytes(), name.as_bytes()]), alloc::boxed::Box::new(move || Ok(s.clone().into_bytes()))))
        })
        .collect()
}

fn pid_stat(pid: u32, tid: Option<u32>) -> Result<Vec<u8>> {
    let p = proc_of(pid)?;
    let t = thread_of(pid, tid)?;
    let th = task::thread_of(&t).ok_or(ENOENT)?;
    let state = if p.is_zombie() { 'Z' } else if t.state.load(Ordering::Relaxed) == crate::sched::SLEEPING { 'S' } else { 'R' };
    let ppid = p.parent().map_or(0, |x| task::vpid(x.pid));
    let nthreads = p.threads.lock().len();
    let start = crate::time::ticks_to_ns(p.start_ticks) / 10_000_000;
    let (vsize, start_stack, brk_start, brk, arg_s, arg_e, env_s, env_e) = {
        let m = th.mm.inner.lock();
        let vs: usize = m.vmas.values().map(|v| v.end - v.start).sum();
        (vs, m.stack_top, m.brk_start, m.brk, m.arg_start, m.arg_end, m.env_start, m.env_end)
    };
    let rss = p.mm_pages();
    // the 52 fields of proc(5)
    let fields: [u64; 49] = [
        ppid as u64, task::vpid(p.pgid.load(Ordering::Relaxed)) as u64, task::vpid(p.sid.load(Ordering::Relaxed)) as u64, 0, u64::MAX, 4194560,
        0, 0, 0, 0, 0, 0, 0, 0, 20, 0, nthreads as u64, 0, start, vsize as u64, rss as u64, u64::MAX,
        0, 0, start_stack as u64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 17, t.cpu.load(Ordering::Relaxed) as u64, 0, 0, 0, 0, 0,
        0, 0, brk_start as u64, brk as u64, arg_s as u64, arg_e as u64, env_s as u64, env_e as u64,
    ];
    let mut out = format!("{} ({}) {}", task::vpid(t.tid), comm(&t), state);
    for (i, v) in fields.iter().enumerate() {
        if i == 4 {
            out.push_str(" -1"); // tpgid
        } else {
            let _ = write!(out, " {}", v);
        }
    }
    out.push_str(" 0\n");
    text(out)
}

fn pid_statm(pid: u32, tid: Option<u32>) -> Result<Vec<u8>> {
    let t = thread_of(pid, tid)?;
    let th = task::thread_of(&t).ok_or(ENOENT)?;
    let size: usize = th.mm.inner.lock().vmas.values().map(|v| v.end - v.start).sum::<usize>() / 4096;
    text(format!("{} {} 0 0 0 0 0\n", size, 0))
}

fn pid_status(pid: u32, tid: Option<u32>) -> Result<Vec<u8>> {
    let p = proc_of(pid)?;
    let t = thread_of(pid, tid)?;
    let th = task::thread_of(&t).ok_or(ENOENT)?;
    let c = p.cred();
    let pending = th.pending.lock().set;
    let shared = p.pending.lock().set;
    let blocked = th.sigmask.load(Ordering::Relaxed) as u64;
    let (ign, cgt) = {
        let h = th.sighand.lock();
        let mut ign = 0u64;
        let mut cgt = 0u64;
        for s in 1..=64usize {
            match h.act[s].handler {
                0 => {}
                1 => ign |= 1 << (s - 1),
                _ => cgt |= 1 << (s - 1),
            }
        }
        (ign, cgt)
    };
    let vsize: usize = th.mm.inner.lock().vmas.values().map(|v| v.end - v.start).sum();
    let state = if p.is_zombie() { "Z (zombie)" } else if t.state.load(Ordering::Relaxed) == crate::sched::SLEEPING { "S (sleeping)" } else { "R (running)" };
    // ids as the reader sees them
    let un = task::current_proc().cred().user_ns.clone();
    let groups: Vec<String> = c.groups.iter().map(|g| format!("{}", un.from_kgid(*g))).collect();
    let v = task::vpid;
    // NSpid and friends: the ids from the reader's PID namespace down to the process's own
    let nsids = |g: u32| -> String {
        let me = task::current_proc().pid_ns.clone();
        let mut chain = Vec::new();
        let mut n = Some(p.pid_ns.clone());
        while let Some(x) = n {
            chain.push(x.clone());
            if Arc::ptr_eq(&x, &me) {
                break;
            }
            n = x.parent.clone();
        }
        chain.iter().rev().map(|x| format!("{}", x.local_of(g))).collect::<Vec<_>>().join("\t")
    };
    let seccomp = th.seccomp_mode.load(Ordering::Relaxed);
    text(format!(
        "Name:\t{}\nUmask:\t{:04o}\nState:\t{}\nTgid:\t{}\nNgid:\t0\nPid:\t{}\nPPid:\t{}\nTracerPid:\t0\nUid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nFDSize:\t{}\nGroups:\t{}\nNStgid:\t{}\nNSpid:\t{}\nNSpgid:\t{}\nNSsid:\t{}\nVmPeak:\t{:8} kB\nVmSize:\t{:8} kB\nVmRSS:\t{:8} kB\nThreads:\t{}\nSigQ:\t0/63000\nSigPnd:\t{:016x}\nShdPnd:\t{:016x}\nSigBlk:\t{:016x}\nSigIgn:\t{:016x}\nSigCgt:\t{:016x}\nCapInh:\t{:016x}\nCapPrm:\t{:016x}\nCapEff:\t{:016x}\nCapBnd:\t{:016x}\nCapAmb:\t{:016x}\nNoNewPrivs:\t{}\nSeccomp:\t{}\nCpus_allowed:\t{:x}\nCpus_allowed_list:\t0-{}\n",
        comm(&t), th.fs.lock().umask, state, v(p.pid), v(t.tid), p.parent().map_or(0, |x| v(x.pid)),
        un.from_kuid(c.uid), un.from_kuid(c.euid), un.from_kuid(c.suid), un.from_kuid(c.fsuid),
        un.from_kgid(c.gid), un.from_kgid(c.egid), un.from_kgid(c.sgid), un.from_kgid(c.fsgid),
        th.files.lock().fds.len().max(64), groups.join(" "), nsids(p.pid), nsids(t.tid), nsids(p.pgid.load(Ordering::Relaxed)), nsids(p.sid.load(Ordering::Relaxed)),
        vsize / 1024, vsize / 1024, p.mm_pages() * 4, p.threads.lock().len(),
        pending, shared, blocked, ign, cgt, c.cap_inh, c.cap_perm, c.cap_eff, c.cap_bnd, c.cap_amb, c.no_new_privs as u32, seccomp,
        (1u64 << crate::percpu::ncpus()) - 1, crate::percpu::ncpus() - 1
    ))
}

fn pid_maps(pid: u32, tid: Option<u32>) -> Result<Vec<u8>> {
    use crate::mm::vm::{VmaKind, PROT_EXEC, PROT_READ, PROT_WRITE};
    let t = thread_of(pid, tid)?;
    let th = task::thread_of(&t).ok_or(ENOENT)?;
    let root = th.fs.lock().root.clone();
    let m = th.mm.inner.lock();
    let mut s = String::new();
    for v in m.vmas.values() {
        let perms = format!(
            "{}{}{}{}",
            if v.prot & PROT_READ != 0 { 'r' } else { '-' },
            if v.prot & PROT_WRITE != 0 { 'w' } else { '-' },
            if v.prot & PROT_EXEC != 0 { 'x' } else { '-' },
            if v.shared { 's' } else { 'p' }
        );
        let (devno, ino) = match &v.file {
            Some(f) if v.path.is_some() => f.stat().map(|st| (st.dev, st.ino)).unwrap_or((0, 0)),
            _ => (0, 0),
        };
        let line = format!("{:08x}-{:08x} {} {:08x} {:02x}:{:02x} {}", v.start, v.end, perms, if v.file.is_some() { v.offset } else { 0 }, major(devno), minor(devno), ino);
        let name: String = match (&v.path, v.kind) {
            (Some(p), _) => String::from_utf8_lossy(&p.path_from(&root)).into_owned(),
            (None, VmaKind::Stack) => String::from("[stack]"),
            (None, VmaKind::Heap) => String::from("[heap]"),
            (None, VmaKind::Vdso) => String::from("[vdso]"),
            (None, VmaKind::Vvar) => String::from("[vvar]"),
            (None, _) if v.start <= m.brk && v.end > m.brk_start && v.file.is_none() => String::from("[heap]"),
            _ => String::new(),
        };
        if name.is_empty() {
            let _ = writeln!(s, "{}", line);
        } else {
            let _ = writeln!(s, "{:<73}{}", line, name);
        }
    }
    text(s)
}

fn read_range(pid: u32, tid: Option<u32>, which: u8) -> Result<Vec<u8>> {
    let t = thread_of(pid, tid)?;
    let th = task::thread_of(&t).ok_or(ENOENT)?;
    let (a, b) = {
        let m = th.mm.inner.lock();
        if which == 0 { (m.arg_start, m.arg_end) } else { (m.env_start, m.env_end) }
    };
    if b <= a || b - a > 1 << 20 {
        return Ok(Vec::new());
    }
    let mut v = alloc::vec![0u8; b - a];
    th.mm.read_mem(a, &mut v)?;
    Ok(v)
}

fn pid_cmdline(pid: u32, tid: Option<u32>) -> Result<Vec<u8>> {
    read_range(pid, tid, 0)
}

fn pid_environ(pid: u32, tid: Option<u32>) -> Result<Vec<u8>> {
    read_range(pid, tid, 1)
}

fn pid_auxv(pid: u32, tid: Option<u32>) -> Result<Vec<u8>> {
    let t = thread_of(pid, tid)?;
    let th = task::thread_of(&t).ok_or(ENOENT)?;
    let a = th.mm.inner.lock().auxv.clone();
    Ok(a.iter().flat_map(|w| w.to_le_bytes()).collect())
}

fn pid_limits(pid: u32, _tid: Option<u32>) -> Result<Vec<u8>> {
    let p = proc_of(pid)?;
    let names = [
        ("Max cpu time", "seconds"), ("Max file size", "bytes"), ("Max data size", "bytes"), ("Max stack size", "bytes"),
        ("Max core file size", "bytes"), ("Max resident set", "bytes"), ("Max processes", "processes"), ("Max open files", "files"),
        ("Max locked memory", "bytes"), ("Max address space", "bytes"), ("Max file locks", "locks"), ("Max pending signals", "signals"),
        ("Max msgqueue size", "bytes"), ("Max nice priority", ""), ("Max realtime priority", ""), ("Max realtime timeout", "us"),
    ];
    let r = *p.rlimits.lock();
    let mut s = String::from("Limit                     Soft Limit           Hard Limit           Units     \n");
    let fmt = |v: u64| if v == u64::MAX { String::from("unlimited") } else { format!("{}", v) };
    for (i, (n, u)) in names.iter().enumerate() {
        let _ = writeln!(s, "{:<26}{:<21}{:<21}{:<10}", n, fmt(r[i].0), fmt(r[i].1), u);
    }
    text(s)
}

/// /proc/PID/mountinfo (or mounts): the mount tree below the process's root.
fn mounts(pid: u32, info: bool) -> Result<Vec<u8>> {
    let t = thread_of(pid, None)?;
    let th = task::thread_of(&t).ok_or(ENOENT)?;
    let root = th.fs.lock().root.clone();
    let mut s = String::new();
    fn walk(m: &Arc<Mount>, parent_id: u32, root: &PathRef, info: bool, s: &mut String) {
        let mp = PathRef { mnt: m.clone(), d: m.root.clone() };
        let at = String::from_utf8_lossy(&mp.path_from(root)).into_owned();
        let fl = m.flags();
        let mut opts = String::from(if fl & path::MNT_RDONLY != 0 { "ro" } else { "rw" });
        if fl & path::MNT_NOSUID != 0 {
            opts.push_str(",nosuid");
        }
        if fl & path::MNT_NODEV != 0 {
            opts.push_str(",nodev");
        }
        if fl & path::MNT_NOEXEC != 0 {
            opts.push_str(",noexec");
        }
        if fl & path::MNT_NOATIME != 0 {
            opts.push_str(",noatime");
        } else {
            opts.push_str(",relatime");
        }
        let src = if m.source.is_empty() { "none" } else { &m.source };
        let d = m.root.fs.dev();
        if info {
            let _ = writeln!(s, "{} {} {}:{} / {} {} - {} {} {}", m.id, parent_id, major(d), minor(d), at, opts, m.fstype, src, if fl & path::MNT_RDONLY != 0 { "ro" } else { "rw" });
        } else {
            let _ = writeln!(s, "{} {} {} {} 0 0", src, at, m.fstype, opts);
        }
        let kids = m.children.lock().clone();
        for k in kids {
            walk(&k, m.id, root, info, s);
        }
    }
    walk(&root.mnt, root.mnt.id, &root, info, &mut s);
    text(s)
}
