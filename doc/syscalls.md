# ruwebos 實際用到的系統呼叫（量測紀錄）

2026-11-25 量的。給 [plan.md](plan.md) 決定 ruxos 要實作哪些系統呼叫。

## 怎麼量的

在 Docker Desktop 的 linux/arm64 容器裡（Linux 6.x 核心）跑 ruwebos 的系統碟映像 `ruwebos-system`（Debian 13、Node 24、rucloud、bubblewrap），容器用 `--privileged`，讓 `[isolation] mode = "on"` 的籠子（bwrap 的 namespace、seccomp、cgroup v2）真的建得起來。用 `strace -f` 從 `rucloud serve` 開始追（包含它啟動的 bwrap 與所有 Worker 的 node），等首頁回應後在主機上跑 `node apps/test.mjs http://localhost:8700`：**14/14 通過**，然後停掉。

```sh
strace -f -qq -o trace.txt -tt -T -s 64 rucloud serve /run/ruwebos/rucloud.toml
```

沒有量到的部分：ruwebos 的 `/sbin/init`（busybox 的 `mount`、`ifconfig`、`route`、`mkfs.ext4`、`e2fsck`、`blkid`、`df`）與 ruvm 的 initramfs，這些在 [plan.md](plan.md) 用讀程式的方式列出。

## 結果：103 種系統呼叫，55.8 萬次

| 系統呼叫 | 次數 |
|---|---:|
| `openat` | 65308 |
| `futex` | 56391 |
| `close` | 29119 |
| `epoll_pwait` | 18525 |
| `readlinkat` | 18290 |
| `fcntl` | 16365 |
| `mprotect` | 15031 |
| `read` | 10098 |
| `write` | 7526 |
| `mmap` | 5765 |
| `fstat` | 5108 |
| `statx` | 4837 |
| `writev` | 4779 |
| `prctl` | 4009 |
| `munmap` | 3858 |
| `pwrite64` | 3592 |
| `recvfrom` | 3291 |
| `newfstatat` | 2825 |
| `madvise` | 2678 |
| `sched_yield` | 2110 |
| `brk` | 2021 |
| `getpid` | 1960 |
| `getdents64` | 1960 |
| `mkdirat` | 1769 |
| `rt_sigprocmask` | 1627 |
| `openat2` | 1533 |
| `epoll_ctl` | 1468 |
| `rt_sigaction` | 1374 |
| `unlinkat` | 1110 |
| `mount` | 1070 |
| `geteuid` | 1060 |
| `utimensat` | 926 |
| `fchmod` | 926 |
| `pread64` | 823 |
| `getcwd` | 815 |
| `fsync` | 780 |
| `getuid` | 657 |
| `getgid` | 618 |
| `capget` | 618 |
| `setsockopt` | 613 |
| `getegid` | 579 |
| `ioctl` | 555 |
| `prlimit64` | 507 |
| `sendto` | 503 |
| `fchown` | 441 |
| `set_robust_list` | 425 |
| `socket` | 368 |
| `rseq` | 347 |
| `symlinkat` | 342 |
| `pipe2` | 270 |
| `clone3` | 268 |
| `accept4` | 266 |
| `gettid` | 253 |
| `chdir` | 233 |
| `connect` | 219 |
| `shutdown` | 214 |
| `getrandom` | 210 |
| `eventfd2` | 193 |
| `getsockopt` | 159 |
| `dup3` | 158 |
| `bind` | 156 |
| `faccessat2` | 152 |
| `ftruncate` | 130 |
| `sched_getaffinity` | 128 |
| `renameat` | 125 |
| `faccessat` | 119 |
| `clone` | 117 |
| `capset` | 117 |
| `uname` | 115 |
| `epoll_create1` | 115 |
| `io_uring_setup` | 114 |
| `listen` | 113 |
| `wait4` | 99 |
| `fchownat` | 94 |
| `execve` | 84 |
| `set_tid_address` | 80 |
| `unshare` | 78 |
| `umount2` | 78 |
| `umask` | 78 |
| `statfs` | 78 |
| `pivot_root` | 78 |
| `getsockname` | 69 |
| `sigaltstack` | 64 |
| `ppoll` | 61 |
| `lseek` | 47 |
| `socketpair` | 40 |
| `signalfd4` | 39 |
| `setuid` | 39 |
| `setgroups` | 39 |
| `setgid` | 39 |
| `fchdir` | 39 |
| `setsid` | 38 |
| `sethostname` | 38 |
| `pidfd_open` | 38 |
| `exit` | 38 |
| `sched_getscheduler` | 27 |
| `sched_getparam` | 27 |
| `kill` | 18 |
| `restart_syscall` | 15 |
| `recvmsg` | 8 |
| `fchmodat` | 7 |
| `sendmmsg` | 6 |
| `exit_group` | 4 |

## 從參數看到的細節

| 項目 | 看到的 | 對 ruxos 的意思 |
|---|---|---|
| 時間 | **`clock_gettime` 0 次** | 全部走 vDSO：ruxos 必須提供 vDSO（`__kernel_clock_gettime` 等），不然每次讀時間都是一次系統呼叫 |
| `socket` | `AF_UNIX` stream 156、dgram 39；`AF_INET` stream 120、dgram 10；`AF_NETLINK` raw 43 | Unix socket 是主要的通道（rucloud↔Worker）；TCP 給 edge 與出口；netlink 給 bwrap 開 `lo`、glibc 的 `getifaddrs` |
| `setsockopt` | `SO_KEEPALIVE`、`SO_REUSEADDR`、`TCP_NODELAY`、`TCP_KEEPIDLE/INTVL/CNT`、`IP_RECVERR` | 接受並照做（keepalive 可以先只記住數值） |
| `ioctl` | `TCGETS` 384（isatty）、`FIONBIO` 120、`SIOCGIFINDEX` 39、`FIONREAD` 12 | |
| `clone` | 執行緒 `CLONE_VM\|FS\|FILES\|SIGHAND\|THREAD\|SYSVSEM\|SETTLS\|PARENT_SETTID\|CHILD_CLEARTID` 267；`fork` 78；`vfork`（`CLONE_VM\|CLONE_VFORK\|CLONE_CLEAR_SIGHAND`）1；bwrap 的 `CLONE_NEWNS\|NEWCGROUP\|NEWUTS\|NEWIPC\|NEWUSER\|NEWPID\|NEWNET` 39 | `clone3` 也有 268 次：回 `ENOSYS` 時 glibc 會改用 `clone`（標準行為） |
| `futex` | `WAKE_PRIVATE` 31620、`WAIT_PRIVATE` 18136、`WAIT_BITSET_PRIVATE\|CLOCK_REALTIME` 6632 | 第二多的系統呼叫：要快（每個 CPU 不搶同一把鎖） |
| `mmap` | 私有匿名 3272、檔案私有（`MAP_FIXED\|MAP_DENYWRITE`，載入函式庫）1185、`MAP_NORESERVE` 226、`MAP_STACK` 300、**`MAP_SHARED` 130**（SQLite 的 `-shm`）、`MAP_DROPPABLE` 40 | `MAP_DROPPABLE` 只有在 vDSO 有 `vgetrandom` 時 glibc 才用，ruxos 不提供，就不會出現 |
| `mprotect` / `mmap` 帶 `PROT_EXEC` | 132 / 790 次，包括 V8 一次 256 MB 的 `PROT_READ\|PROT_WRITE\|PROT_EXEC` | JIT 要能有 RWX 頁；EL0 要能自己做快取維護（`SCTLR_EL1.UCI`、`UCT`） |
| `madvise` | `MADV_DONTFORK` 2490、`MADV_DONTNEED` 188 | `DONTNEED` 之後要讀到 0 |
| `fcntl` | **`F_SETLK` 14484**、`F_GETLK` 130、`F_SETFD`、`F_GETFL`/`F_SETFL` | SQLite 的 POSIX 記錄鎖：要快 |
| `pwrite64` 3592、`fsync` 780、`pread64` 823 | | SQLite（D1、Durable Objects）：`fsync` 要真的寫到磁碟（virtio-blk FLUSH） |
| `epoll_ctl` | `EPOLLIN`、`EPOLLOUT`、`EPOLLRDHUP`、`EPOLLET` | 要支援邊緣觸發 |
| 其他 fd | `eventfd2` 193、`signalfd4` 39（bwrap 等 `SIGCHLD`）、`pidfd_open` 38、`pipe2` 270、`socketpair` 40 | |
| 信號 | `SIGSEGV`、`SIGBUS` 有處理函式（V8 的 WebAssembly 越界陷阱）、`SIGCHLD`、`SIGPIPE`、`SIGTERM`、`SIGINT` | 信號框架要和 Linux 一模一樣（`ucontext`、`fpsimd_context`），V8 會讀改 `pc` |
| `prlimit64` | `RLIMIT_STACK` 348、`RLIMIT_AS` 76、`RLIMIT_NOFILE` 45、`RLIMIT_DATA` 38 | |
| `mount` | bind（`MS_BIND\|MS_REC`）509、remount 唯讀 139、`proc`、`tmpfs` 115、`devpts` 38、`MS_PRIVATE` 39；`pivot_root` 78、`umount2` 78 | 全部是 bwrap |
| `prctl` | `PR_CAPBSET_DROP` 3198、`PR_SET_NAME` 267、`PR_SET_PDEATHSIG` 117、`PR_SET_DUMPABLE` 78、`PR_SET_SECCOMP` 76（rucloud 的 42 條指令的過濾程式）、`PR_SET_NO_NEW_PRIVS` 39 | |
| `openat2` | 1533 次，都是 `RESOLVE_NO_MAGICLINKS\|RESOLVE_IN_ROOT` | bwrap 建籠子時用 |
| 讀的 `/proc`、`/sys` | `/proc/self/maps`、`/proc/self/cgroup`、`/proc/stat`、`/proc/meminfo`、`/proc/filesystems`、`/proc/version_signature`、`/proc/sys/user/max_user_namespaces`、`/proc/sys/kernel/overflowuid`、`overflowgid`、`/sys/devices/system/cpu/online`、`/sys/fs/cgroup/**/{memory.max,memory.high,cpu.max,pids.max,cgroup.procs,cgroup.subtree_control}` | |
| 回錯誤的 | `io_uring_setup` → `EPERM`（libuv 退回 epoll）、`unshare` → `ENOSPC`、`readlinkat` → `EINVAL`（realpath 逐段檢查） | ruxos 對 `io_uring_*` 回 `ENOSYS`，結果一樣 |
