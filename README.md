# ruxos

ruxos 是一個用 Rust 寫的作業系統核心，跑在 **AArch64** 與 **RISC-V（riscv64）** 上。它實作 Linux 的系統呼叫介面（ABI）與開機方式，所以**沒有改過的 Linux 使用者空間**（Debian 的 glibc 程式、busybox、Node.js……）可以直接在它上面執行：給它一顆 ext4 的根檔案系統，它就像 Linux 一樣開機、掛載、執行 `init` 或一個 shell。

它在 [ruvm](https://github.com/ccc-claude-opus/ruvm)（Apple Silicon 上用 `-accel hvf` 硬體加速）與 QEMU 的 `virt` 機型上執行，裝置用 virtio-mmio，硬體從 device tree 讀。

授權：MIT（[LICENSE](LICENSE)）。依賴套件與授權見 [doc/licenses.md](doc/licenses.md)。

## 建置

需要：

* Rust 1.100 以上（stable），加上兩個 target：
  ```bash
  rustup target add aarch64-unknown-none-softfloat riscv64imac-unknown-none-elf
  ```
* `llvm-objcopy`（macOS：`brew install llvm`）

建置：

```bash
tools/build.sh aarch64
```

```bash
tools/build.sh riscv64
```

結果是 `out/<arch>/ruxos`：一個帶 Linux `Image` 標頭的核心映像，可以直接給 ruvm 或 QEMU 的 `-kernel`；`out/<arch>/ruxos.elf` 是帶符號的版本（除錯用）。

## 執行：開機進 shell

`tools/run.sh` 會先建置，再用 ruvm（預設）或 QEMU（`--qemu`）開機，序列埠接在你的終端機。`--shell` 讓根檔案系統上的 shell 當 `init`，開機後直接進 shell：

| | ruvm | QEMU |
|---|---|---|
| aarch64 | `tools/run.sh --shell` | `tools/run.sh --qemu --shell` |
| riscv64 | `tools/run.sh riscv64 --shell` | `tools/run.sh riscv64 --qemu --shell` |

* aarch64 在 Apple Silicon 上自動用 `-accel hvf`（ruvm 與 QEMU 都是）；其他情況是軟體模擬（`--accel tcg` 也可以指定）。ruvm 要用 hvf 時，ruvm 的執行檔要先簽好 entitlement（ruvm 的 `tools/hvf-sign.sh`）。
* riscv64 是軟體模擬；QEMU 由它自己的 OpenSBI 啟動 ruxos，ruvm 內建 SBI。
* 根檔案系統（第一顆磁碟 `/dev/vda`，以唯讀快照開，不會改到原檔）：
  * aarch64：ruwebos 的系統碟 `../ruwebos/out/system.ext4`（Debian 13 arm64；在 ruwebos 跑 `image/build.sh` 產生）。
  * riscv64：ruvm 的測試用根檔案系統 `../ruvm/tests/guest/out/linux/root/riscv64/root.ext4`（只有 `/bin/busybox`，`--shell` 會用 `busybox sh`；進去後跑 `busybox --install -s /bin` 就有其他指令）。
  * 用自己的：`SYSTEM=path/to/root.ext4 tools/run.sh --shell`。任何 arm64／riscv64 的 Linux ext4 根檔案系統都可以。
* 第二顆磁碟 `/dev/vdb` 是 `out/<arch>/scratch.ext4`（自動建立的空白檔；`DATA=...` 換一顆）。
* 網路：user-mode 網路，`eth0` 是 10.0.2.15（要自己設定位址，例如 busybox 的 `ifconfig eth0 10.0.2.15 netmask 255.255.255.0 up` 與 `route add default gw 10.0.2.2`）；主機的 8701 埠轉到虛擬機的 8700（`PORT=...` 換）。
* 進去之後通常先掛 `/proc` 與 `/sys`（`/dev` 是核心自動掛的 devtmpfs）：
  ```sh
  mount -t proc proc /proc; mount -t sysfs sysfs /sys; mount -t tmpfs tmpfs /tmp
  ```
* 離開：在 shell 裡 `exit`。`init` 結束時核心會 panic（「Attempted to kill init!」，和 Linux 一樣），`--shell` 帶了 `panic=-1`，所以接著重新開機，而 run.sh 讓 ruvm／QEMU 在重新開機時結束。也可以在 busybox 裡 `poweroff -f`。

不加 `--shell` 時執行根檔案系統的 `/sbin/init`（ruwebos 的系統碟會開成 ruwebos，見下面）。

其他選項：`--smp N`、`-m MB`；`APPEND="..."` 整個換掉核心命令列；`INITRD=file.cpio.gz` 加 initramfs；`KERNEL=...` 換成別的核心（例如 Linux，拿來和 ruxos 跑同一組測試比較）；`--` 之後的參數原樣交給 ruvm／QEMU。

### 不用 run.sh

ruxos 就是一個 Linux 格式的核心映像，照一般的方式開機即可。ruvm（aarch64）：

```bash
ruvm -M aarch64 -m 2048 -smp 4 -accel hvf -kernel out/aarch64/ruxos -drive file=root.ext4 -rng -nic user -serial stdio -append "console=ttyAMA0 root=/dev/vda rootfstype=ext4 rw init=/bin/sh"
```

QEMU（aarch64）。virtio-mmio 要用現代版（`force-legacy=false`）：

```bash
qemu-system-aarch64 -M virt -cpu host -accel hvf -smp 4 -m 2048 -nographic -kernel out/aarch64/ruxos -append "console=ttyAMA0 root=/dev/vda rootfstype=ext4 rw init=/bin/sh" -global virtio-mmio.force-legacy=false -drive if=none,id=root,file=root.ext4,format=raw -device virtio-blk-device,drive=root
```

QEMU（riscv64）：

```bash
qemu-system-riscv64 -M virt -cpu max -smp 2 -m 1024 -nographic -kernel out/riscv64/ruxos -append "console=ttyS0 root=/dev/vda rootfstype=ext4 rw init=/bin/busybox -- sh" -global virtio-mmio.force-legacy=false -drive if=none,id=root,file=root.ext4,format=raw -device virtio-blk-device,drive=root
```

### 核心參數

| 參數 | 意思 |
|---|---|
| `console=ttyAMA0`／`ttyS0` | 主控台（PL011／16550） |
| `root=/dev/vdX`、`rootfstype=ext4`、`rw`／`ro` | 根檔案系統 |
| `init=PATH` | 第一個程式（預設 `/sbin/init`）；`--` 之後的字是它的參數 |
| `rdinit=PATH` | 有 initramfs 時，在 initramfs 裡執行的程式（預設 `/init`） |
| `panic=N` | panic 後 N 秒重新開機，負數是立刻；0（預設）停住 |
| `quiet` | 少印開機訊息 |
| `ruxos.strace`（`=entry`） | 印出每個系統呼叫（與它的進入點） |
| `ruxos.debug`（`=short`、`=net`） | 主控台按 Ctrl-T 印出工作、各 CPU 的 PC、TCP socket、網卡與中斷控制器的狀態、epoll／futex 的遺失喚醒檢查；`=short` 不印工作清單；`=net` 印出每個封包 |
| `ruxos.test=sched` | 開機自我測試（排程器） |

其他不認得的 `名稱=值` 照 Linux 的習慣成為 `init` 的環境變數，其餘的成為它的參數。

## 測試

```bash
./test.sh
```

在 ruvm 的 aarch64（hvf 與軟體模擬）與 riscv64 上開機，有裝 QEMU 時也在 QEMU 上（它的 riscv64 各個 hart 真的平行執行，ruvm 的模擬不會），跑排程器自我測試與 `tests/initramfs`：busybox 的基本操作、`tests/abi`（行程、訊號、記憶體、時間、檔案、socket 的半關閉、namespace、seccomp、虛擬終端機；musl 與 glibc 各跑一次）、`tests/abi/futex.c`（執行緒池、ping-pong、mutex 競爭）、ext4 的讀寫（之後用 `e2fsck` 檢查）與斷電後的日誌重播、在系統碟上用 Node.js 跑網路測試。這些測試在 Linux 上也跑過（`KERNEL=` 換成 Debian 的核心），結果相同。

## 實作了什麼

* **核心**：Linux `Image` 開機，device tree，每顆 CPU 一個執行佇列（work stealing、無 tick 的閒置），sleeping mutex 與 wait queue，vDSO（`clock_gettime` 等不進核心）。
* **記憶體**：每個行程的位址空間、copy-on-write 的 fork、mmap（檔案、匿名、共享）、page cache。
* **行程與訊號**：Linux 的 `rt_sigframe`、系統呼叫重啟、`sigaltstack`、futex（含 robust list、requeue）、itimer、pidfd、signalfd、eventfd、epoll。
* **檔案系統**：VFS 與 dentry cache，tmpfs、devtmpfs、procfs、sysfs、devpts、cgroup2，cpio initramfs，**ext4 讀寫**（extent、htree、metadata_csum、jbd2 日誌與重播），`openat2`，POSIX／OFD／flock 鎖。
* **網路**：AF_UNIX（stream、dgram、seqpacket、`SCM_RIGHTS`、`SO_PEERCRED`）、AF_INET 的 TCP／UDP（eth0 上用 smoltcp，本機連線在核心內直接配對）、NETLINK_ROUTE、`ifconfig`／`route` 的 ioctl。
* **終端機**：主控台的 line discipline（canonical、echo、^C ^Z）、虛擬終端機（`/dev/ptmx`、`/dev/pts/N`）。
* **隔離**：user、mount、PID、network、UTS、IPC、cgroup 七種 namespace（uid/gid map、`pivot_root`、`unshare`）、seccomp（classic BPF 過濾）、capabilities、cgroup v2 的 `memory.max`、`pids.max`、`cpu.max`。bubblewrap 照常可用。
* **裝置**：virtio-blk、virtio-net、virtio-rng（virtio-mmio），GICv2、PLIC，PL011、16550，PSCI、SBI。

還沒有的：htree 目錄索引節點的分割（目錄裡的檔案非常多時）、System V IPC、ptrace、io_uring、擴充屬性（xattr 回 `EOPNOTSUPP`）。

## 用在 ruwebos

ruxos 最初的目的是取代 [ruwebos](https://github.com/ccc-claude-opus/ruwebos) 裡的 Linux 核心：同一份 Debian 13 系統碟、Node.js 24 與 rucloud，不改一行，效能接近 Linux。在 ruwebos 的資料夾：

```bash
./ruwebos --kernel ruxos
```

```bash
./ruwebos --kernel ruxos --test
```

（ruxos 要放在 `../ruxos`，或用 `RUXOS=` 指定；沒建置會先建置。）應用一樣由 bubblewrap 關在籠子裡（namespace、seccomp、cgroup v2），14 個應用的測試都通過，被關起來的程序數與 Linux 上一樣。與 Linux 的比較（兩邊條件相同，[doc/bench.md](doc/bench.md)）：請求延遲相同或更低，吞吐量是 Linux 的 0.96～1.25 倍，開機後的記憶體少約 80 MB，核心開機到 `init` 0.03 秒（Linux 0.17～0.20 秒）。

用 `tools/run.sh`（不加 `--shell`）開 aarch64 的系統碟，也會開成 ruwebos，但用的是 `out/aarch64/scratch.ext4` 當資料碟，不會動到 ruwebos 的真實資料。

## 文件

* [doc/plan.md](doc/plan.md)：架構、系統呼叫集合、驅動、檔案系統、網路、效能目標、里程碑（M0～M8，都已完成）與當初的決定
* [doc/syscalls.md](doc/syscalls.md)：ruwebos 在 Linux 上實際用到的系統呼叫（量測紀錄）
* [doc/bench.md](doc/bench.md)：與 Linux 的效能比較，以及量測時找到並修正的問題
* [doc/licenses.md](doc/licenses.md)：依賴套件的授權
