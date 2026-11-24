#!/bin/sh
# Runs ruxos's tests: the in-kernel self tests on aarch64 (-accel hvf when available, and TCG) and
# riscv64 (TCG), each with 4 CPUs.
#   ./test.sh            everything
#   ./test.sh sched      one test
set -e
cd "$(dirname "$0")"
tests=${*:-sched}
tools/build.sh >/dev/null
fail=0
run() { # run <arch> <accel> <test>
    arch=$1 accel=$2 t=$3
    console=ttyAMA0; [ $arch = riscv64 ] && console=ttyS0
    out=$(APPEND="console=$console ruxos.test=$t" timeout 120 tools/run.sh $arch --accel $accel --smp 4 2>&1 || true)
    line=$(echo "$out" | grep "ruxos-test $t:" | tail -1)
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
