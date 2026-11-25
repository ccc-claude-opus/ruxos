# 效能量測

同一台 iMac（Apple M3），ruvm `-accel hvf -smp 4 -m 2048`，同樣的 ruwebos 系統碟與資料碟。工具：

* `./ruwebos --test`（ruwebos 的 launcher）：啟動時間、14 個應用測試；
* `node tools/bench.mjs --label <核心> --out doc/bench.json`：對開著的 ruwebos 量請求延遲與吞吐量（每個網址 500 個循序請求取 p50／p99，再用 10 個並行連線跑 3 秒）。原始數字在 [bench.json](bench.json)。

## Linux 6.12（Debian 13 的核心），2026-11-25

| 項目 | Linux |
|---|---:|
| 啟動到首頁回應（launcher） | 2.29～2.82 s |
| 核心開機到 init 的第一行（`/proc/uptime`） | 0.12 s |
| 14 個應用測試的時間加總 | 9.3 s |
| 首頁 `/`：p50／p99 | 2.55 ms／8.86 ms |
| 靜態檔（calculator `/`）：p50／p99 | 1.26 ms／3.20 ms |
| D1 查詢（notes `/`）：p50／p99 | 1.29 ms／14.91 ms |
| 吞吐量（首頁／靜態檔／D1） | 639／4870／849 req/s |

### 啟動時間裡不是核心的部分

ruxos M0（印出裝置樹摘要就關機）在 ruvm 裡從啟動到結束的時間：`-m 256` 是 0.04 s，`-m 2048` 是 0.66 s，`-m 2048` 再加上兩個磁碟是 1.39 s。也就是 launcher 量到的 2.3～2.8 s 裡，**約 1.3 s 花在 ruvm 準備記憶體與磁碟**（與核心無關，Linux 與 ruxos 一樣付）。這是 ruvm 那邊可以改善的地方，不在 ruxos 的範圍裡。

## ruxos 與 Linux，兩邊的應用都關在籠子裡，2026-11-25（M8 之後）

**更正**：下一節（M7）的 ruxos 欄量的時候，ruxos 還建不起 bubblewrap 的籠子，rucloud 以 `[isolation] mode = "auto"` 執行，應用**沒有**關在籠子裡，也就沒有 `cpu.max`（每個應用半顆 CPU）的限制；Linux 那邊的應用一直都在籠子裡。所以 M7 那張表的吞吐量（「Linux 的 1.4～3.4 倍」）是不公平的比較。M8 之後兩邊條件相同，重新量：兩個核心各開機一次，各跑 3 次 `tools/bench.mjs`，取中位數（原始數字在 [bench.json](bench.json) 的 `ruxos-caged`、`linux-caged`）。

| 項目 | Linux 6.12 | ruxos | §3.2 的目標 |
|---|---:|---:|---|
| 首頁 `/`：p50／p99 | 3.03／8.80 ms | 1.64／5.90 ms | ≤ 1.1 × Linux ✓ |
| 靜態檔：p50／p99 | 1.52／2.61 ms | 1.53／2.31 ms | ≤ 1.1 × Linux ✓ |
| D1 查詢：p50／p99 | 1.53／10.61 ms | 1.53／7.44 ms | ≤ 1.1 × Linux ✓ |
| 吞吐量（首頁／靜態檔／D1） | 930／6467／867 req/s | 895／6803／1083 req/s | ≥ 0.8 × Linux ✓（0.96～1.25 ×） |
| 開機約 9 秒後的記憶體 | 615 MB | 534 MB | ≤ Linux ✓ |
| `./ruwebos --test`（開機到關機） | 11.4～12.1 s | 11.2～12.5 s | ≤ 1.1 × Linux ✓ |

吞吐量的上限主要是 `cpu.max`（每個應用的 Worker 最多半顆 CPU），兩個核心都照它限制，所以接近。ruxos 的 `cpu.max` 照 Linux CFS 的頻寬控制做：在一個週期（100 ms）內用完配額的 cgroup，它的工作等到下一個週期；時間片在配額用完時結束。

主控台的 `diag` 在兩個核心上看到一樣的籠子：有 seccomp 過濾的程序 42 個、在自己的 PID namespace 的 42 個、以 uid 999 執行的 63 個。

## ruxos 與 Linux，2026-11-25（M7）

（ruxos 的應用沒有關在籠子裡，見上一節的更正。）

同一台機器、同樣的系統碟與資料碟，兩個核心前後各量一次（機器的狀態會影響數字，所以 Linux 也重量；11-24 的 Linux 數字留在 bench.json 的 `linux-2026-11-24`）。

| 項目 | Linux 6.12 | ruxos | §3.2 的目標 |
|---|---:|---:|---|
| 核心開機到 init 的第一行（`/proc/uptime`） | 0.17～0.20 s | 0.03～0.04 s | ≤ 0.05 s ✓ |
| 啟動到首頁回應（launcher） | 1.9～3.0 s | 1.6～2.9 s | ≤ Linux ✓ |
| `./ruwebos --test`：14 個應用測試，開機到關機 | 13.1～13.5 s | 12.4～14.2 s | ≤ 1.1 × Linux ✓ |
| 首頁 `/`：p50／p99 | 3.04／7.75 ms | 1.53／5.90 ms | ≤ 1.1 × Linux ✓ |
| 靜態檔（calculator `/`）：p50／p99 | 1.52／3.02 ms | 1.54／4.31 ms | p50 ✓，p99 1.4 × |
| D1 查詢（notes `/`）：p50／p99 | 1.55／17.4 ms | 1.52／5.12 ms | ≤ 1.1 × Linux ✓ |
| 吞吐量（首頁／靜態檔／D1，10 個並行連線） | 672／5087／766 req/s | 1877／6892／2572 req/s | ≥ 0.8 × Linux ✓（1.4～3.4 ×） |
| 開機 10 秒後的記憶體（MemTotal − MemAvailable） | 587 MB | 518 MB | ≤ Linux ✓ |

記憶體用 ruwebos 主控台的 `status` 指令讀（`echo status > out/run/console.in`）。p99 在幾次量測之間變動很大（同一個核心 2～12 ms），只有 p50 與吞吐量穩定。

### 量測時找到並修正的問題

`tools/bench.mjs` 的並行階段一開始就失敗或卡住；用比 bench 更重的壓力（10 個並行連線、每個網址 3 秒、連續 180 輪，每個請求 2.5 秒逾時）一個一個找出來：

1. **ruxos：同時進來的 SYN 被 RST。** 一個 listen 的 socket 只有 4 個 smoltcp 的 LISTEN socket，同一批封包裡第 5 個以後的 SYN 找不到 socket 就被回 RST。現在一個封包處理完就補滿（Linux 的 SYN backlog）。
2. **ruxos：accept 拿到還在 SYN-RECEIVED 的連線。** 讀它會得到 end-of-file，hyper 就把連線關了；現在只有握手完成的連線進 accept 佇列。
3. **ruxos：程式關掉的 socket 再收到資料時不回 RST。** 伺服器關掉 keep-alive 連線的同時，用戶端剛好送出下一個請求：Linux 回 RST，用戶端立刻重試；smoltcp 收下資料、回 ACK，用戶端就一直等（這就是請求卡 2.5 秒以上的原因）。現在和 Linux 一樣：close 時還有沒讀的資料、或關掉之後又收到資料，就送 RST。
4. **ruxos：Unix socket 的半關閉。** `shutdown(SHUT_WR)` 讓對方連寫都不行（EPIPE）而且 poll 回 POLLHUP；現在只結束一個方向，POLLHUP 與 POLLRDHUP 照 Linux 的 `unix_poll`。讀的時候也不會在對方送完最後的資料並關閉的瞬間誤回 end-of-file。`tests/abi` 加了這些情形（Unix socket 與 loopback TCP），在 Linux 上也跑過。
5. **ruxos：本機連線在 accept 佇列滿的時候失敗。** Linux 只是延後（重送 SYN），不會失敗；現在直接排進佇列。
6. **ruxos：不帶 FUTEX_PRIVATE_FLAG 的 futex 用實體位址當鍵**，即使記憶體是私有映射（寫入時複製後鍵就變了）；Linux 對私有映射用（位址空間, 位址）。`FUTEX_CMP_REQUEUE` 搬移等待者時也有一個空窗。另外加了 `tests/abi/futex.c`（執行緒池、ping-pong、mutex 競爭，偵測卡住），musl 與 glibc 都跑。
7. **rucloud：應用的 log 檔沒有上限。** 大量請求把 512 MB 的資料碟寫滿，D1 寫入失敗（500），Linux 上整個服務停擺。rucloud 現在限制每個應用的 log 檔合計 `log_max_mb`（預設 64 MB）。

還剩一個：D1 壓力測試（10 個並行連線）在 ruxos 上約十六萬個請求出現一次 `D1_ERROR: fetch failed: UND_ERR_SOCKET`（Worker 到綁定 API 的連線被對方關閉），Linux 上五萬九千個請求沒有出現（Linux 的速度只有三分之一）。可能是 HTTP keep-alive 的關閉競爭（undici 不重試 POST），還沒確定。

### 除錯工具

* `ruxos.debug` 下 Ctrl-T 另外印出：TCP socket 的狀態與佇列、virtio-net 與 GIC 的狀態、各 CPU 最後一次 IAR、每個中斷的次數、epoll 裡「已就緒卻沒排隊」的項目、futex 等待者中字已經改變的、還有未讀資料的 Unix socket。`ruxos.debug=short` 不印工作清單（印工作清單本身要 150 ms，會掩蓋問題）。
* `/proc/PID/task/TID/syscall` 與 `wchan`（Linux 的格式），ruwebos 主控台的 `diag` 指令用它們列出 rucloud 每個執行緒在等什麼。
* `tools/run.sh` 的 `KERNEL=` 換成別的核心（例如 Debian 的 Linux）跑同一個測試 initramfs，做差異比對。
