// Trap entry, trap frames and context switching.
// sscratch is 0 while in the kernel and holds the per-CPU pointer while in user mode; in the kernel
// tp is always the per-CPU pointer (PerCpu.kstack_top at 0, PerCpu.scratch at 8).
// Trap frame (272 bytes): x0..x31 at 8*i (x0 unused, x2 = sp of the interrupted code), sepc at 256,
// sstatus at 264.

.equ FRAME, 272

.macro SAVE_GPRS
    sd      x1, 8(sp)
    sd      x3, 24(sp)
    sd      x5, 40(sp)
    sd      x6, 48(sp)
    sd      x7, 56(sp)
    sd      x8, 64(sp)
    sd      x9, 72(sp)
    sd      x10, 80(sp)
    sd      x11, 88(sp)
    sd      x12, 96(sp)
    sd      x13, 104(sp)
    sd      x14, 112(sp)
    sd      x15, 120(sp)
    sd      x16, 128(sp)
    sd      x17, 136(sp)
    sd      x18, 144(sp)
    sd      x19, 152(sp)
    sd      x20, 160(sp)
    sd      x21, 168(sp)
    sd      x22, 176(sp)
    sd      x23, 184(sp)
    sd      x24, 192(sp)
    sd      x25, 200(sp)
    sd      x26, 208(sp)
    sd      x27, 216(sp)
    sd      x28, 224(sp)
    sd      x29, 232(sp)
    sd      x30, 240(sp)
    sd      x31, 248(sp)
.endm

.macro RESTORE_GPRS
    ld      x1, 8(sp)
    ld      x3, 24(sp)
    ld      x5, 40(sp)
    ld      x6, 48(sp)
    ld      x7, 56(sp)
    ld      x8, 64(sp)
    ld      x9, 72(sp)
    ld      x10, 80(sp)
    ld      x11, 88(sp)
    ld      x12, 96(sp)
    ld      x13, 104(sp)
    ld      x14, 112(sp)
    ld      x15, 120(sp)
    ld      x16, 128(sp)
    ld      x17, 136(sp)
    ld      x18, 144(sp)
    ld      x19, 152(sp)
    ld      x20, 160(sp)
    ld      x21, 168(sp)
    ld      x22, 176(sp)
    ld      x23, 184(sp)
    ld      x24, 192(sp)
    ld      x25, 200(sp)
    ld      x26, 208(sp)
    ld      x27, 216(sp)
    ld      x28, 224(sp)
    ld      x29, 232(sp)
    ld      x30, 240(sp)
    ld      x31, 248(sp)
.endm

.section .text
.balign 4
.global trap_entry
trap_entry:
    csrrw   tp, sscratch, tp
    bnez    tp, from_user
    csrrw   tp, sscratch, zero      // from the kernel: tp = per-CPU again, sscratch = 0
    addi    sp, sp, -FRAME
    SAVE_GPRS
    sd      tp, 32(sp)
    addi    t0, sp, FRAME
    sd      t0, 16(sp)
    csrr    t0, sepc
    sd      t0, 256(sp)
    csrr    t0, sstatus
    sd      t0, 264(sp)
    mv      a0, sp
    call    trap_kernel
    ld      t0, 256(sp)
    csrw    sepc, t0
    ld      t0, 264(sp)
    csrw    sstatus, t0
    RESTORE_GPRS
    addi    sp, sp, FRAME
    sret

from_user:                          // tp = per-CPU, sscratch = the user's tp
    sd      sp, 8(tp)
    ld      sp, 0(tp)
    addi    sp, sp, -FRAME
    SAVE_GPRS
    ld      t0, 8(tp)
    sd      t0, 16(sp)
    csrrw   t0, sscratch, zero
    sd      t0, 32(sp)
    csrr    t0, sepc
    sd      t0, 256(sp)
    csrr    t0, sstatus
    sd      t0, 264(sp)
    mv      a0, sp
    call    trap_user

// Returns to user mode from the trap frame at sp (also how a new user thread starts).
.global ret_to_user
ret_to_user:
    csrci   sstatus, 2
    csrw    sscratch, tp
    ld      t0, 256(sp)
    csrw    sepc, t0
    ld      t0, 264(sp)
    csrw    sstatus, t0
    RESTORE_GPRS
    ld      tp, 32(sp)
    ld      sp, 16(sp)
    sret

// switch_context(prev: *mut Context, next: *const Context); Context: ra, sp, s0..s11
.global switch_context
switch_context:
    sd      ra, 0(a0)
    sd      sp, 8(a0)
    sd      s0, 16(a0)
    sd      s1, 24(a0)
    sd      s2, 32(a0)
    sd      s3, 40(a0)
    sd      s4, 48(a0)
    sd      s5, 56(a0)
    sd      s6, 64(a0)
    sd      s7, 72(a0)
    sd      s8, 80(a0)
    sd      s9, 88(a0)
    sd      s10, 96(a0)
    sd      s11, 104(a0)
    ld      ra, 0(a1)
    ld      sp, 8(a1)
    ld      s0, 16(a1)
    ld      s1, 24(a1)
    ld      s2, 32(a1)
    ld      s3, 40(a1)
    ld      s4, 48(a1)
    ld      s5, 56(a1)
    ld      s6, 64(a1)
    ld      s7, 72(a1)
    ld      s8, 80(a1)
    ld      s9, 88(a1)
    ld      s10, 96(a1)
    ld      s11, 104(a1)
    ret

// A new kernel thread starts here: s0 = function, s1 = argument.
.global task_trampoline
task_trampoline:
    mv      a0, s0
    mv      a1, s1
    call    task_start
    j       .

.option push
.option arch, +d
// fp_save(state: *mut FpState) / fp_restore(state: *const FpState): f0..f31, fcsr
.global fp_save
fp_save:
    fsd     f0, 0(a0)
    fsd     f1, 8(a0)
    fsd     f2, 16(a0)
    fsd     f3, 24(a0)
    fsd     f4, 32(a0)
    fsd     f5, 40(a0)
    fsd     f6, 48(a0)
    fsd     f7, 56(a0)
    fsd     f8, 64(a0)
    fsd     f9, 72(a0)
    fsd     f10, 80(a0)
    fsd     f11, 88(a0)
    fsd     f12, 96(a0)
    fsd     f13, 104(a0)
    fsd     f14, 112(a0)
    fsd     f15, 120(a0)
    fsd     f16, 128(a0)
    fsd     f17, 136(a0)
    fsd     f18, 144(a0)
    fsd     f19, 152(a0)
    fsd     f20, 160(a0)
    fsd     f21, 168(a0)
    fsd     f22, 176(a0)
    fsd     f23, 184(a0)
    fsd     f24, 192(a0)
    fsd     f25, 200(a0)
    fsd     f26, 208(a0)
    fsd     f27, 216(a0)
    fsd     f28, 224(a0)
    fsd     f29, 232(a0)
    fsd     f30, 240(a0)
    fsd     f31, 248(a0)
    frcsr   t0
    sd      t0, 256(a0)
    ret

.global fp_restore
fp_restore:
    fld     f0, 0(a0)
    fld     f1, 8(a0)
    fld     f2, 16(a0)
    fld     f3, 24(a0)
    fld     f4, 32(a0)
    fld     f5, 40(a0)
    fld     f6, 48(a0)
    fld     f7, 56(a0)
    fld     f8, 64(a0)
    fld     f9, 72(a0)
    fld     f10, 80(a0)
    fld     f11, 88(a0)
    fld     f12, 96(a0)
    fld     f13, 104(a0)
    fld     f14, 112(a0)
    fld     f15, 120(a0)
    fld     f16, 128(a0)
    fld     f17, 136(a0)
    fld     f18, 144(a0)
    fld     f19, 152(a0)
    fld     f20, 160(a0)
    fld     f21, 168(a0)
    fld     f22, 176(a0)
    fld     f23, 184(a0)
    fld     f24, 192(a0)
    fld     f25, 200(a0)
    fld     f26, 208(a0)
    fld     f27, 216(a0)
    fld     f28, 224(a0)
    fld     f29, 232(a0)
    fld     f30, 240(a0)
    fld     f31, 248(a0)
    ld      t0, 256(a0)
    fscsr   t0
    ret
.option pop
