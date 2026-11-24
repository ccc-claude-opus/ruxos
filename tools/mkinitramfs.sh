#!/bin/sh
# Builds out/<arch>/test-initramfs.cpio.gz: busybox (static, from ruvm's initramfs) + tests/initramfs/init.
#   tools/mkinitramfs.sh aarch64|riscv64
set -e
cd "$(dirname "$0")/.."
arch=${1:-aarch64}
deb=arm64; [ "$arch" = riscv64 ] && deb=riscv64
ruvm=${RUVM:-../ruvm}
work=out/$arch/initramfs
rm -rf "$work"
mkdir -p "$work/bin" "$work/dev" "$work/proc" "$work/sys" "$work/tmp" "$work/mnt" "$work/etc"
gunzip -c "$ruvm/tests/guest/out/linux/root/$deb/initramfs.cpio.gz" | (cd "$work" && cpio -id --quiet bin/busybox)
cp tests/initramfs/init "$work/init"
# the ABI tests, static (musl), built with zig cc
zig cc -target $arch-linux-musl -static -O2 -o "$work/bin/abi" tests/abi/abi.c
(cd "$work" && find . | LC_ALL=C sort | cpio -o --quiet -H newc) | gzip -9 > out/$arch/test-initramfs.cpio.gz
