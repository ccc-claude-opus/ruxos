# 授權

ruxos 本身：MIT（[LICENSE](../LICENSE)）。編進核心的 crate（`tools/licenses.sh` 由 `cargo tree` 產生）：

| crate | 版本 | 授權 |
|---|---|---|
| adler2 | v2.0.1 | 0BSD OR MIT OR Apache-2.0 |
| bitflags | v1.3.2 | MIT/Apache-2.0 |
| byteorder | v1.5.0 | Unlicense OR MIT |
| cfg-if | v1.0.5 | MIT OR Apache-2.0 |
| hash32 | v0.3.1 | MIT OR Apache-2.0 |
| heapless | v0.8.0 | MIT OR Apache-2.0 |
| managed | v0.8.0 | 0BSD |
| miniz_oxide | v0.8.9 | MIT OR Zlib OR Apache-2.0 |
| smoltcp | v0.12.0 | 0BSD |
| stable_deref_trait | v1.2.1 | MIT OR Apache-2.0 |

測試與建置時用到、但不編進核心的工具：zig（MIT，交叉編譯 tests/abi）、busybox（GPL-2.0，取自 ruvm 的 initramfs，只放在測試用的 initramfs 裡）。
