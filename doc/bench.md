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
