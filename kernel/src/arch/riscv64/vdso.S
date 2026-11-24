// vDSO code (position independent, copied into the vDSO page at offset 0x800). The vvar page is
// the page before: { u32 seq; u32 pad; u64 mult; i64 realtime_offset_ns }. ns = (ticks * mult) >> 32.
.section .text.vdso, "ax"
.balign 16
.global vdso_start
vdso_start:

.macro VVAR reg, tmp
    auipc   \reg, 0
    li      \tmp, -4096
    and     \reg, \reg, \tmp
    add     \reg, \reg, \tmp
.endm

// a0 = clock, a1 = timespec, a2 = realtime flag (internal)
.global vdso_clock_gettime
vdso_clock_gettime:
    li      t0, 1
    beqz    a0, 1f
    li      t1, 5
    beq     a0, t1, 1f
    li      t0, 0
    li      t1, 1
    beq     a0, t1, 1f
    li      t1, 4
    beq     a0, t1, 1f
    li      t1, 6
    beq     a0, t1, 1f
    li      t1, 7
    beq     a0, t1, 1f
    li      a7, 113
    ecall
    ret
1:  VVAR    t2, t3
2:  lw      t3, 0(t2)
    andi    t4, t3, 1
    bnez    t4, 2b
    fence   r, r
    rdtime  t4
    ld      t5, 8(t2)
    ld      t6, 16(t2)
    fence   r, r
    lw      a3, 0(t2)
    bne     a3, t3, 2b
    mul     a4, t4, t5
    mulhu   a5, t4, t5
    srli    a4, a4, 32
    slli    a5, a5, 32
    or      a4, a4, a5
    beqz    t0, 3f
    add     a4, a4, t6
3:  li      a5, 1000000000
    divu    a6, a4, a5
    remu    a7, a4, a5
    sd      a6, 0(a1)
    sd      a7, 8(a1)
    li      a0, 0
    ret

// a0 = timeval, a1 = timezone
.global vdso_gettimeofday
vdso_gettimeofday:
    beqz    a1, 1f
    sd      zero, 0(a1)
1:  beqz    a0, 3f
    VVAR    t2, t3
2:  lw      t3, 0(t2)
    andi    t4, t3, 1
    bnez    t4, 2b
    fence   r, r
    rdtime  t4
    ld      t5, 8(t2)
    ld      t6, 16(t2)
    fence   r, r
    lw      a3, 0(t2)
    bne     a3, t3, 2b
    mul     a4, t4, t5
    mulhu   a5, t4, t5
    srli    a4, a4, 32
    slli    a5, a5, 32
    or      a4, a4, a5
    add     a4, a4, t6
    li      a5, 1000000000
    divu    a6, a4, a5
    remu    a7, a4, a5
    li      a5, 1000
    divu    a7, a7, a5
    sd      a6, 0(a0)
    sd      a7, 8(a0)
3:  li      a0, 0
    ret

.global vdso_clock_getres
vdso_clock_getres:
    li      t0, 7
    bgtu    a0, t0, 2f
    li      t0, 2
    beq     a0, t0, 2f
    li      t0, 3
    beq     a0, t0, 2f
    beqz    a1, 1f
    sd      zero, 0(a1)
    li      t0, 1
    sd      t0, 8(a1)
1:  li      a0, 0
    ret
2:  li      a7, 114
    ecall
    ret

.global vdso_rt_sigreturn
vdso_rt_sigreturn:
    li      a7, 139
    ecall

.global vdso_end
vdso_end:
