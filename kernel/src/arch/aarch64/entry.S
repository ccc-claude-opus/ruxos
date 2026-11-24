// Exception vectors, trap frames and context switching.
// Trap frame (272 bytes, on the kernel stack): x0..x30 at 0..240, sp_el0 at 248, elr at 256, spsr at 264.

.equ FRAME, 272

.macro SAVE
    sub     sp, sp, #FRAME
    stp     x0, x1, [sp, #0]
    stp     x2, x3, [sp, #16]
    stp     x4, x5, [sp, #32]
    stp     x6, x7, [sp, #48]
    stp     x8, x9, [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    mrs     x21, sp_el0
    mrs     x22, elr_el1
    mrs     x23, spsr_el1
    stp     x30, x21, [sp, #240]
    stp     x22, x23, [sp, #256]
.endm

.macro RESTORE
    ldp     x22, x23, [sp, #256]
    ldp     x30, x21, [sp, #240]
    msr     elr_el1, x22
    msr     spsr_el1, x23
    msr     sp_el0, x21
    ldp     x0, x1, [sp, #0]
    ldp     x2, x3, [sp, #16]
    ldp     x4, x5, [sp, #32]
    ldp     x6, x7, [sp, #48]
    ldp     x8, x9, [sp, #64]
    ldp     x10, x11, [sp, #80]
    ldp     x12, x13, [sp, #96]
    ldp     x14, x15, [sp, #112]
    ldp     x16, x17, [sp, #128]
    ldp     x18, x19, [sp, #144]
    ldp     x20, x21, [sp, #160]
    ldp     x22, x23, [sp, #176]
    ldp     x24, x25, [sp, #192]
    ldp     x26, x27, [sp, #208]
    ldp     x28, x29, [sp, #224]
    add     sp, sp, #FRAME
.endm

.macro VEC target
    .balign 128
    b       \target
.endm

.section .text
.balign 2048
.global exception_vectors
exception_vectors:
    VEC     bad_exception       // current EL, SP_EL0
    VEC     bad_exception
    VEC     bad_exception
    VEC     bad_exception
    VEC     el1_sync            // current EL, SP_ELx
    VEC     el1_irq
    VEC     bad_exception
    VEC     el1_serror
    VEC     el0_sync            // lower EL, AArch64
    VEC     el0_irq
    VEC     bad_exception
    VEC     el0_serror
    VEC     bad_exception       // lower EL, AArch32
    VEC     bad_exception
    VEC     bad_exception
    VEC     bad_exception

el1_sync:
    SAVE
    mov     x0, sp
    bl      trap_el1_sync
    RESTORE
    eret

el1_irq:
    SAVE
    mov     x0, sp
    bl      trap_irq
    RESTORE
    eret

el1_serror:
    SAVE
    mov     x0, sp
    mov     x1, #1
    bl      trap_bad
    b       .

el0_sync:
    SAVE
    mov     x0, sp
    bl      trap_el0_sync
    b       ret_to_user

el0_irq:
    SAVE
    mov     x0, sp
    bl      trap_el0_irq
    b       ret_to_user

el0_serror:
    SAVE
    mov     x0, sp
    mov     x1, #2
    bl      trap_bad
    b       ret_to_user

bad_exception:
    SAVE
    mov     x0, sp
    mov     x1, #0
    bl      trap_bad
    b       .

// Returns to EL0 from the trap frame at sp (also how a new user thread starts).
.global ret_to_user
ret_to_user:
    msr     daifset, #0xf
    RESTORE
    eret

// switch_context(prev: *mut Context, next: *const Context)
// Context: x19..x28, x29, x30, sp (13 words)
.global switch_context
switch_context:
    stp     x19, x20, [x0, #0]
    stp     x21, x22, [x0, #16]
    stp     x23, x24, [x0, #32]
    stp     x25, x26, [x0, #48]
    stp     x27, x28, [x0, #64]
    stp     x29, x30, [x0, #80]
    mov     x9, sp
    str     x9, [x0, #96]
    ldp     x19, x20, [x1, #0]
    ldp     x21, x22, [x1, #16]
    ldp     x23, x24, [x1, #32]
    ldp     x25, x26, [x1, #48]
    ldp     x27, x28, [x1, #64]
    ldp     x29, x30, [x1, #80]
    ldr     x9, [x1, #96]
    mov     sp, x9
    ret

// A new kernel thread starts here: x19 = function, x20 = argument.
.global task_trampoline
task_trampoline:
    mov     x0, x19
    mov     x1, x20
    bl      task_start
    b       .

// fp_save(state: *mut FpState) / fp_restore(state: *const FpState): q0..q31, fpcr, fpsr
.global fp_save
fp_save:
    .arch_extension fp
    .arch_extension simd
    stp     q0, q1, [x0, #0]
    stp     q2, q3, [x0, #32]
    stp     q4, q5, [x0, #64]
    stp     q6, q7, [x0, #96]
    stp     q8, q9, [x0, #128]
    stp     q10, q11, [x0, #160]
    stp     q12, q13, [x0, #192]
    stp     q14, q15, [x0, #224]
    stp     q16, q17, [x0, #256]
    stp     q18, q19, [x0, #288]
    stp     q20, q21, [x0, #320]
    stp     q22, q23, [x0, #352]
    stp     q24, q25, [x0, #384]
    stp     q26, q27, [x0, #416]
    stp     q28, q29, [x0, #448]
    stp     q30, q31, [x0, #480]
    mrs     x9, fpcr
    mrs     x10, fpsr
    add     x0, x0, #512
    stp     x9, x10, [x0]
    ret

.global fp_restore
fp_restore:
    ldp     q0, q1, [x0, #0]
    ldp     q2, q3, [x0, #32]
    ldp     q4, q5, [x0, #64]
    ldp     q6, q7, [x0, #96]
    ldp     q8, q9, [x0, #128]
    ldp     q10, q11, [x0, #160]
    ldp     q12, q13, [x0, #192]
    ldp     q14, q15, [x0, #224]
    ldp     q16, q17, [x0, #256]
    ldp     q18, q19, [x0, #288]
    ldp     q20, q21, [x0, #320]
    ldp     q22, q23, [x0, #352]
    ldp     q24, q25, [x0, #384]
    ldp     q26, q27, [x0, #416]
    ldp     q28, q29, [x0, #448]
    ldp     q30, q31, [x0, #480]
    add     x0, x0, #512
    ldp     x9, x10, [x0]
    msr     fpcr, x9
    msr     fpsr, x10
    ret
