// riscv64 entry: the Linux RISC-V Image header, then the boot hart's path to Rust.
// Boot protocol (Documentation/arch/riscv/boot.rst): S-mode, a0 = hart id, a1 = device tree, MMU off.
// The Sv39 boot table maps the first 256 GB twice with 1 GB pages: at its physical address (identity)
// and at physical + LM (the linear map the kernel is linked in).

.equ LM, 0xffffffc000000000
.equ PTE_LEAF, 0xef                  // V R W X G A D (no U)

.section .text.head, "ax"
.option norvc
.global _start
_start:
    j       primary_entry           // code0
    .word   0                       // code1
    .dword  0x200000                // text_offset
    .dword  _image_size             // image_size
    .dword  0                       // flags: little endian
    .word   2                       // version 0.2
    .word   0                       // res1
    .dword  0                       // res2
    .ascii  "RISCV\0\0\0"           // magic
    .ascii  "RSC\x05"               // magic2
    .word   0                       // res3
.option rvc

primary_entry:
    csrw    sie, zero
    csrw    sip, zero
    mv      s0, a0                  // hart id
    mv      s1, a1                  // device tree
    lla     t0, _bss_start
    lla     t1, _bss_end
1:  bgeu    t0, t1, 2f
    sd      zero, 0(t0)
    addi    t0, t0, 8
    j       1b
2:  call    build_boot_table
    call    enable_mmu
    lla     t0, primary_high_addr
    ld      t0, 0(t0)
    jr      t0
primary_high:
    lla     t0, boot_stack_top_addr
    ld      sp, 0(t0)
    mv      a0, s1
    mv      a1, s0
    call    kmain
3:  wfi
    j       3b

// Secondary harts (SBI HSM hart_start): a0 = hart id, a1 = opaque = virtual address of the boot
// record { stack_top, cpu_index }.
.global secondary_entry
secondary_entry:
    csrw    sie, zero
    mv      s0, a0
    mv      s1, a1
    call    enable_mmu
    lla     t0, secondary_high_addr
    ld      t0, 0(t0)
    jr      t0
secondary_high:
    ld      sp, 0(s1)
    ld      a0, 8(s1)
    mv      a1, s0
    call    secondary_main
4:  wfi
    j       4b

build_boot_table:
    lla     t0, boot_root
    li      t1, 0                   // i
    li      t2, 256
1:  slli    t3, t1, 28              // (i << 30) >> 12 << 10
    ori     t3, t3, PTE_LEAF
    slli    t4, t1, 3
    add     t4, t4, t0
    sd      t3, 0(t4)               // identity
    li      t5, 2048
    add     t4, t4, t5
    sd      t3, 0(t4)               // linear map (entry 256 + i)
    addi    t1, t1, 1
    blt     t1, t2, 1b
    ret

enable_mmu:
    lla     t0, boot_root
    srli    t0, t0, 12
    li      t1, 8
    slli    t1, t1, 60
    or      t0, t0, t1
    sfence.vma
    csrw    satp, t0
    sfence.vma
    ret

.section .rodata
.balign 8
primary_high_addr: .dword primary_high
secondary_high_addr: .dword secondary_high
boot_stack_top_addr: .dword boot_stack_top

.section .bss
.balign 4096
.global boot_root
boot_root: .space 4096
.balign 16
boot_stack: .space 65536
.global boot_stack_top
boot_stack_top:
