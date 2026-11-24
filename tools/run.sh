#!/bin/sh
# Boots ruxos in ruvm.
#   tools/run.sh [aarch64|riscv64] [--accel hvf|kvm|tcg] [--smp N] [-m MB] [--qemu] [-- extra ruvm args]
# aarch64 defaults to the ruwebos disks (../ruwebos/out/system.ext4, data.ext4) and -accel hvf on an
# Apple Silicon Mac; riscv64 uses ruvm's Debian riscv64 root (../ruvm/tests/guest/out/linux/root/riscv64).
# APPEND overrides the kernel command line; INITRD adds an initramfs.
set -e
cd "$(dirname "$0")/.."
arch=aarch64
case $1 in aarch64 | riscv64) arch=$1; shift ;; esac
ruvm_dir=${RUVM:-../ruvm}
accel= smp=4 mem=2048 qemu=
[ $arch = riscv64 ] && mem=1024 smp=2
[ $arch = aarch64 ] && [ "$(uname -sm)" = "Darwin arm64" ] && accel=hvf
while [ $# -gt 0 ]; do
    case $1 in
        --accel) accel=$2; shift ;;
        --smp) smp=$2; shift ;;
        -m) mem=$2; shift ;;
        --qemu) qemu=1 ;;
        --) shift; break ;;
        *) echo "run.sh: unknown option $1" >&2; exit 2 ;;
    esac
    shift
done
[ "$accel" = tcg ] && accel=
tools/build.sh $arch
if [ $arch = aarch64 ]; then
    console=ttyAMA0
    disks="-drive file=${SYSTEM:-../ruwebos/out/system.ext4},snapshot=on -drive file=${DATA:-../ruwebos/out/data.ext4}"
else
    console=ttyS0
    disks="-drive file=${SYSTEM:-$ruvm_dir/tests/guest/out/linux/root/riscv64/root.ext4},snapshot=on"
fi
append=${APPEND:-"console=$console root=/dev/vda rootfstype=ext4 rw init=/sbin/init"}
initrd=
[ -n "$INITRD" ] && initrd="-initrd $INITRD"
if [ -n "$qemu" ]; then
    m=virt cpu="-cpu max"
    [ -n "$accel" ] && cpu="-cpu host -accel $accel"
    # shellcheck disable=SC2086
    exec qemu-system-$arch -M $m $cpu -smp $smp -m $mem -nographic -kernel out/$arch/ruxos $initrd -append "$append" "$@"
fi
# shellcheck disable=SC2086
exec "$ruvm_dir/target/release/ruvm" -M $arch -m $mem -smp $smp ${accel:+-accel $accel} -kernel out/$arch/ruxos $initrd \
    $disks -rng -nic user,hostfwd=tcp::${PORT:-8701}-:8700 -serial stdio -append "$append" "$@"
