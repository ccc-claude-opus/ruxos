#!/bin/sh
# Builds ruxos for one or both architectures into out/<arch>/ruxos (a flat binary with a Linux Image
# header, bootable with -kernel in ruvm and QEMU) and out/<arch>/ruxos.elf (symbols).
#   tools/build.sh [aarch64|riscv64|all] [--debug]
set -e
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
arches=${1:-all}
[ "$arches" = all ] && arches="aarch64 riscv64"
profile=release flag=--release
[ "$2" = --debug ] && profile=debug flag=
objcopy=$(command -v llvm-objcopy || echo /opt/homebrew/opt/llvm/bin/llvm-objcopy)
for arch in $arches; do
    case $arch in
        aarch64) target=aarch64-unknown-none-softfloat ;;
        riscv64) target=riscv64imac-unknown-none-elf ;;
        *) echo "unknown arch $arch" >&2; exit 2 ;;
    esac
    cargo build -q $flag --target $target -p ruxos
    mkdir -p out/$arch
    cp target/$target/$profile/ruxos out/$arch/ruxos.elf
    "$objcopy" -O binary out/$arch/ruxos.elf out/$arch/ruxos
done
