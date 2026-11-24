// vDSO code (position independent, copied into the vDSO page at offset 0x800). The vvar page is
// the page before the vDSO's first page: { u32 seq; u32 pad; u64 mult; i64 realtime_offset_ns }.
// ns = (ticks * mult) >> 32.
.section .text.vdso, "ax"
.balign 16
.global vdso_start
vdso_start:

.macro VVAR reg
    adr     \reg, .
    and     \reg, \reg, #0xfffffffffffff000
    sub     \reg, \reg, #4096
.endm

// x0 = clock, x1 = timespec; returns 0, or falls back to the system call
.global vdso_clock_gettime
vdso_clock_gettime:
    cmp     w0, #0
    b.eq    1f
    cmp     w0, #5
    b.eq    1f
    cmp     w0, #1
    b.eq    2f
    cmp     w0, #4
    b.eq    2f
    cmp     w0, #6
    b.eq    2f
    cmp     w0, #7
    b.eq    2f
    mov     x8, #113
    svc     #0
    ret
1:  mov     x6, #1                  // realtime
    b       3f
2:  mov     x6, #0
3:  VVAR    x9
4:  ldr     w10, [x9]
    tbnz    w10, #0, 4b
    dmb     ishld
    isb
    mrs     x11, cntvct_el0
    ldr     x12, [x9, #8]
    ldr     x13, [x9, #16]
    dmb     ishld
    ldr     w14, [x9]
    cmp     w14, w10
    b.ne    4b
    mul     x15, x11, x12
    umulh   x16, x11, x12
    extr    x15, x16, x15, #32
    cbz     x6, 5f
    add     x15, x15, x13
5:  movz    x17, #0xca00
    movk    x17, #0x3b9a, lsl #16   // 1e9
    udiv    x16, x15, x17
    msub    x14, x16, x17, x15
    stp     x16, x14, [x1]
    mov     x0, #0
    ret

// x0 = timeval, x1 = timezone
.global vdso_gettimeofday
vdso_gettimeofday:
    mov     x7, x0
    cbz     x1, 1f
    str     xzr, [x1]
1:  cbz     x7, 3f
    VVAR    x9
2:  ldr     w10, [x9]
    tbnz    w10, #0, 2b
    dmb     ishld
    isb
    mrs     x11, cntvct_el0
    ldr     x12, [x9, #8]
    ldr     x13, [x9, #16]
    dmb     ishld
    ldr     w14, [x9]
    cmp     w14, w10
    b.ne    2b
    mul     x15, x11, x12
    umulh   x16, x11, x12
    extr    x15, x16, x15, #32
    add     x15, x15, x13
    movz    x17, #0xca00
    movk    x17, #0x3b9a, lsl #16
    udiv    x16, x15, x17
    msub    x14, x16, x17, x15
    mov     x17, #1000
    udiv    x14, x14, x17
    stp     x16, x14, [x7]
3:  mov     x0, #0
    ret

// x0 = clock, x1 = timespec (resolution)
.global vdso_clock_getres
vdso_clock_getres:
    cmp     w0, #7
    b.hi    2f
    cmp     w0, #2
    b.eq    2f
    cmp     w0, #3
    b.eq    2f
    cbz     x1, 1f
    mov     x9, #1
    stp     xzr, x9, [x1]
1:  mov     x0, #0
    ret
2:  mov     x8, #114
    svc     #0
    ret

.global vdso_rt_sigreturn
vdso_rt_sigreturn:
    mov     x8, #139
    svc     #0

.global vdso_end
vdso_end:
