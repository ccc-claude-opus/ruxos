# ruxos

一個用 Rust 寫的作業系統核心，跑在 AArch64 與 RISC-V（riscv64）上：只做到足以取代 [ruwebos](../ruwebos) 裡的 Linux 核心——沒有改過的 Debian 13 使用者空間、Node.js 24 與 rucloud 照常執行——效能接近 Linux，在 Apple Silicon 上用 [ruvm](../ruvm) `-accel hvf` 硬體加速。

**狀態**：M2（使用者程式）完成：busybox 與 ABI 測試在 hvf、aarch64 TCG、riscv64 TCG 上通過。`./test.sh` 跑全部測試；`tools/run.sh` 在 ruvm 裡開機。

* [doc/plan.md](doc/plan.md)：架構、最小的系統呼叫集合、驅動、檔案系統、網路、效能目標、ruwebos 怎麼換核心、里程碑與風險
* [doc/syscalls.md](doc/syscalls.md)：ruwebos（rucloud 加 14 個應用的測試）在 Linux 上實際用到的系統呼叫

授權：MIT（[LICENSE](LICENSE)）。
