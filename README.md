# ruxos

一個用 Rust 寫的作業系統核心，跑在 AArch64 與 RISC-V（riscv64）上：只做到足以取代 [ruwebos](../ruwebos) 裡的 Linux 核心——沒有改過的 Debian 13 使用者空間、Node.js 24 與 rucloud 照常執行——效能接近 Linux，在 Apple Silicon 上用 [ruvm](../ruvm) `-accel hvf` 硬體加速。

**狀態**：M6 完成：ruwebos 以 `./ruwebos --kernel ruxos --test` 在 ruxos 上開機，14 個應用測試全部通過（hvf，連續三次；從啟動到可用約 1.4～1.7 秒，Linux 約 2.0～2.8 秒），之後同一份資料碟 e2fsck 乾淨、Linux 照常開機；應用隔離暫用 `auto`（M8 改回 `on`）。M5：網路（virtio-net、smoltcp 上的 TCP/UDP、本機連線在核心內直接配對、NETLINK_ROUTE、ifconfig/route 的 ioctl）。M4：ext4 讀寫（配置、extent 樹、目錄含 htree 葉節點分割、jbd2 日誌與重播、metadata_csum），寫入後 e2fsck 乾淨；Debian 的 glibc 程式與 Node.js 24 在 ruxos 上執行（hvf、aarch64 TCG），riscv64 TCG 跑 busybox、glibc 與 ext4 測試。

* [doc/plan.md](doc/plan.md)：架構、最小的系統呼叫集合、驅動、檔案系統、網路、效能目標、ruwebos 怎麼換核心、里程碑與風險
* [doc/syscalls.md](doc/syscalls.md)：ruwebos（rucloud 加 14 個應用的測試）在 Linux 上實際用到的系統呼叫

授權：MIT（[LICENSE](LICENSE)）。
