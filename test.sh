#!/bin/sh
# Runs ruxos's tests: the in-kernel self tests and the user-space tests (tests/initramfs: busybox
# and tests/abi) on aarch64 (-accel hvf when available, and TCG) and
# riscv64 (TCG), each with 4 CPUs.
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
run() { # run <arch> <accel> <test>
    arch=$1 accel=$2 t=$3
    console=ttyAMA0; [ $arch = riscv64 ] && console=ttyS0
    case $t in
        m2)
            # a fresh scratch disk for the ext4 write tests, checked with e2fsck afterwards
            "$mkfs" -q -F -b 4096 out/$arch/scratch.ext4 256m
            out=$(INITRD=out/$arch/test-initramfs.cpio.gz APPEND="console=$console rdinit=/init ext4rw=1 keep=1" timeout 900 tools/run.sh $arch --accel $accel --smp 4 </dev/null 2>&1 || true)
            if ! fsck=$("$e2fsck" -fn out/$arch/scratch.ext4 2>&1); then
                out="$out
e2fsck: $fsck"
                out=$(echo "$out" | sed 's/ruxos-test m2: ok/ruxos-test m2: e2fsck found errors/')
            fi ;;
        *) out=$(APPEND="console=$console ruxos.test=$t" timeout 120 tools/run.sh $arch --accel $accel --smp 4 </dev/null 2>&1 || true) ;;
    esac
    line=$(echo "$out" | grep -a "ruxos-test $t:" | tail -1)
    case $line in
        *": ok"*) echo "ok    $arch/$accel $line" ;;
        *) echo "FAIL  $arch/$accel $t"; echo "$out" | tail -15; fail=1 ;;
    esac
}
accel=tcg
[ "$(uname -sm)" = "Darwin arm64" ] && accel=hvf
for t in $tests; do
    [ $accel = hvf ] && run aarch64 hvf $t
    run aarch64 tcg $t
    run riscv64 tcg $t
done
exit $fail
