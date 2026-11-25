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
zig cc -target $arch-linux-musl -static -O2 -o "$work/bin/futex" tests/abi/futex.c
# a glibc sysroot (Debian libc6 + dash, from ruvm's tools/linux/fetch-sysroot.sh) with the same tests
# linked dynamically against glibc
sysroot=$ruvm/tests/guest/out/user/$deb/sysroot
if [ -d "$sysroot" ]; then
    cp -a "$sysroot" "$work/sysroot"
    mkdir -p "$work/sysroot/proc" "$work/sysroot/tmp" "$work/sysroot/dev"
    zig cc -target $arch-linux-gnu.2.36 -O2 -o "$work/sysroot/abi" tests/abi/abi.c -lpthread
    zig cc -target $arch-linux-gnu.2.36 -O2 -o "$work/sysroot/futex" tests/abi/futex.c -lpthread
fi
mkdir -p "$work/tests"
cp tests/node/*.js "$work/tests/"
(cd "$work" && find . | LC_ALL=C sort | cpio -o --quiet -H newc) | gzip -9 > out/$arch/test-initramfs.cpio.gz
