#!/bin/sh
# Boots ruxos in ruvm.
#   tools/run.sh [aarch64|riscv64] [--shell] [--accel hvf|kvm|tcg] [--smp N] [-m MB] [--qemu] [-- extra ruvm args]
# --shell: a shell as init instead of /sbin/init: /bin/sh of the ruwebos system disk, busybox's sh on
# ruvm's riscv64 root (only /bin/busybox there); SHELL_INIT="/path -- args" to choose.
# aarch64 defaults to the ruwebos system disk (../ruwebos/out/system.ext4, read-only snapshot) and a
# scratch data disk out/<arch>/scratch.ext4 (never the real ruwebos data disk; DATA=... to choose), and
# -accel hvf on an Apple Silicon Mac; riscv64 uses ruvm's Debian riscv64 root (../ruvm/tests/guest/out/linux/root/riscv64).
# APPEND overrides the kernel command line; INITRD adds an initramfs; KERNEL boots another kernel
# (e.g. Linux, to compare it with ruxos on the same tests).
set -e
cd "$(dirname "$0")/.."
arch=aarch64
case $1 in aarch64 | riscv64) arch=$1; shift ;; esac
ruvm_dir=${RUVM:-../ruvm}
accel= smp=4 mem=2048 qemu= init=/sbin/init
[ $arch = riscv64 ] && mem=1024 smp=2
[ $arch = aarch64 ] && [ "$(uname -sm)" = "Darwin arm64" ] && accel=hvf
while [ $# -gt 0 ]; do
    case $1 in
        --accel) accel=$2; shift ;;
        --smp) smp=$2; shift ;;
        -m) mem=$2; shift ;;
        --qemu) qemu=1 ;;
        --shell) init=shell ;;
        --) shift; break ;;
        *) echo "run.sh: unknown option $1" >&2; exit 2 ;;
    esac
    shift
done
[ "$accel" = tcg ] && accel=
tools/build.sh $arch
if [ $arch = aarch64 ]; then
    console=ttyAMA0
    disks="-drive file=${SYSTEM:-../ruwebos/out/system.ext4},snapshot=on"
else
    console=ttyS0
    disks="-drive file=${SYSTEM:-$ruvm_dir/tests/guest/out/linux/root/riscv64/root.ext4},snapshot=on"
fi
data=${DATA:-out/$arch/scratch.ext4}
[ -f "$data" ] || dd if=/dev/zero of="$data" bs=1 count=0 seek=256m 2>/dev/null
disks="$disks -drive file=$data"
shell=
if [ "$init" = shell ]; then
    shell=1
    if [ -n "$SHELL_INIT" ]; then init=$SHELL_INIT
    elif [ $arch = riscv64 ] && [ -z "$SYSTEM" ]; then init="/bin/busybox -- sh"
    else init=/bin/sh
    fi
fi
extra=
# leaving the shell ends the VM (init exits, the kernel panics and, with panic=-1, reboots;
# QEMU runs with -no-reboot)
# (panic= before init=: what follows "--" in init= goes to init)
[ -n "$shell" ] && extra="panic=-1 "
append=${APPEND:-"console=$console root=/dev/vda rootfstype=ext4 rw PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin ${extra}init=$init"}
initrd=
[ -n "$INITRD" ] && initrd="-initrd $INITRD"
if [ -n "$qemu" ]; then
    m=virt cpu="-cpu max"
    [ -n "$accel" ] && cpu="-cpu host -accel $accel"
    # shellcheck disable=SC2086
    # virtio-mmio devices in the same roles as with ruvm: QEMU's arm virt lists them in the device
    # tree in reverse order, its riscv virt in order
    qdisks=""
    for d in $(echo "$disks" | sed 's/-drive file=//g'); do
        f=${d%%,*}
        snap=""
        case $d in *snapshot=on*) snap=",snapshot=on" ;; esac
        n=d$(echo "$f" | cksum | cut -d' ' -f1)
        dev="-drive if=none,id=$n,file=$f,format=raw$snap -device virtio-blk-device,drive=$n"
        if [ $arch = aarch64 ]; then qdisks="$dev $qdisks"; else qdisks="$qdisks $dev"; fi
    done
    exec qemu-system-$arch -M $m $cpu -smp $smp -m $mem -nographic -no-reboot -kernel ${KERNEL:-out/$arch/ruxos} $initrd -append "$append" \
        -global virtio-mmio.force-legacy=false -netdev user,id=net,hostfwd=tcp::${PORT:-8701}-:8700 -device virtio-net-device,netdev=net \
        -object rng-random,id=rng,filename=/dev/urandom -device virtio-rng-device,rng=rng $qdisks "$@"
fi
# shellcheck disable=SC2086
exec "$ruvm_dir/target/release/ruvm" -M $arch -m $mem -smp $smp ${accel:+-accel $accel} -no-reboot -kernel ${KERNEL:-out/$arch/ruxos} $initrd \
    $disks -rng -nic user,hostfwd=tcp::${PORT:-8701}-:8700 -serial stdio -append "$append" "$@"
