#!/bin/sh
# Lists the crates ruxos is built from, with their licenses, as doc/licenses.md.
set -e
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
{
    echo "# 授權"
    echo
    echo "ruxos 本身：MIT（[LICENSE](../LICENSE)）。編進核心的 crate（\`tools/licenses.sh\` 由 \`cargo tree\` 產生）："
    echo
    echo "| crate | 版本 | 授權 |"
    echo "|---|---|---|"
    for t in aarch64-unknown-none-softfloat riscv64imac-unknown-none-elf; do
        cargo tree -q -e normal --target $t -p ruxos --prefix none -f '{p} {l}'
    done | sed 's/ (.*)//' | sort -u | awk '$1 != "ruxos" { v=$2; $1=$1; n=$1; $1=""; $2=""; sub(/^ +/, ""); printf "| %s | %s | %s |\n", n, v, $0 }'
    echo
    echo "測試與建置時用到、但不編進核心的工具：zig（MIT，交叉編譯 tests/abi）、busybox（GPL-2.0，取自 ruvm 的 initramfs，只放在測試用的 initramfs 裡）。"
} > doc/licenses.md
cat doc/licenses.md
