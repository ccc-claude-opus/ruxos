#!/bin/sh
# Runs ruxos's tests: the in-kernel self tests and the user-space tests (tests/initramfs: busybox
# and tests/abi) on aarch64 (-accel hvf when available, and TCG) and
# riscv64 (TCG), each with 4 CPUs, in ruvm; and in QEMU when it is installed (its riscv64 harts run
# in parallel threads, which ruvm's emulation does not).
#   ./test.sh            everything
#   ./test.sh sched      one test
set -e
cd "$(dirname "$0")"
tests=${*:-sched m2}
tools/build.sh >/dev/null
e2p=$(brew --prefix e2fsprogs 2>/dev/null || echo /usr)/sbin
mkfs=$e2p/mkfs.ext4 e2fsck=$e2p/e2fsck
for a in aarch64 riscv64; do tools/mkinitramfs.sh $a; done
fail=0
run() { # run <arch> <accel> <test> [qemu]
    arch=$1 accel=$2 t=$3 vm=${4:-}
    q=; label=$arch/$accel
    [ "$vm" = qemu ] && q=--qemu label=$arch/qemu-$accel
    console=ttyAMA0; [ $arch = riscv64 ] && console=ttyS0
    case $t in
        m2)
            # a fresh scratch disk for the ext4 write tests, checked with e2fsck afterwards
            "$mkfs" -q -F -b 4096 out/$arch/scratch.ext4 256m
            out=$(INITRD=out/$arch/test-initramfs.cpio.gz APPEND="console=$console rdinit=/init ext4rw=1 keep=1" timeout 900 tools/run.sh $arch $q --accel $accel --smp 4 </dev/null 2>&1 || true)
            if ! fsck=$("$e2fsck" -fn out/$arch/scratch.ext4 2>&1); then
                out="$out
e2fsck: $fsck"
                out=$(echo "$out" | sed 's/ruxos-test m2: ok/ruxos-test m2: e2fsck found errors/')
            fi ;;
        *) out=$(APPEND="console=$console ruxos.test=$t" timeout 120 tools/run.sh $arch $q --accel $accel --smp 4 </dev/null 2>&1 || true) ;;
    esac
    line=$(echo "$out" | grep -a "ruxos-test $t:" | tail -1)
    case $line in
        *": ok"*) echo "ok    $label $line" ;;
        *) echo "FAIL  $label $t"; echo "$out" | tail -15; fail=1 ;;
    esac
}
host_accel=tcg
[ "$(uname -sm)" = "Darwin arm64" ] && host_accel=hvf
for t in $tests; do
    [ $host_accel = hvf ] && run aarch64 hvf $t
    run aarch64 tcg $t
    run riscv64 tcg $t
    if command -v qemu-system-aarch64 >/dev/null; then run aarch64 $host_accel $t qemu; fi
    if command -v qemu-system-riscv64 >/dev/null; then run riscv64 tcg $t qemu; fi
done
exit $fail
