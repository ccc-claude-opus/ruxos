// AArch64 entry: the Linux arm64 Image header, then the boot CPU's path to Rust.
// Boot protocol (Documentation/arch/arm64/booting.rst): x0 = physical address of the device tree,
// MMU off, at EL2 or EL1. We build a page table that maps all memory twice, at its physical
// address (identity, only to survive turning the MMU on) and at physical + LM (the linear map the
// kernel is linked in), then jump to the linked address.

.equ LM, 0xffff800000000000

// MAIR_EL1: attr0 Device-nGnRnE, attr1 Normal write-back, attr2 Normal non-cacheable
.equ MAIR, 0x000000000044ff00
// L1 block descriptors (1 GB): valid block | AF | inner shareable | attr index | UXN/PXN
.equ BLK_NORMAL, (1 | (1 << 10) | (3 << 8) | (1 << 2) | (1 << 54))
.equ BLK_DEVICE, (1 | (1 << 10) | (0 << 2) | (1 << 54) | (1 << 53))

.section .text.head, "ax"
.global _start
_start:
    b       primary_entry           // code0
    .long   0                       // code1
    .quad   0x200000                // text_offset
    .quad   _image_size             // image_size (including .bss)
    .quad   0x2                     // flags: little endian, 4 KB pages
    .quad   0, 0, 0                 // res2..res4
    .ascii  "ARM\x64"               // magic
    .long   0                       // res5

primary_entry:
    mov     x19, x0                 // device tree
    bl      el2_to_el1
    // zero .bss (physical addresses: the MMU is off)
    adrp    x0, _bss_start
    adrp    x1, _bss_end
1:  cmp     x0, x1
    b.hs    2f
    stp     xzr, xzr, [x0], #16
    b       1b
2:  bl      build_boot_tables
    bl      enable_mmu
    ldr     x1, =primary_high
    br      x1
primary_high:
    ldr     x1, =boot_stack_top
    mov     sp, x1
    mov     x0, x19
    mov     x1, #0                  // cpu 0
    bl      kmain
3:  wfe
    b       3b

// Secondary CPUs (PSCI CPU_ON): x0 = context = the virtual address of this CPU's boot record
// { stack_top, cpu_index }.
.global secondary_entry
secondary_entry:
    mov     x19, x0
    bl      el2_to_el1
    bl      enable_mmu
    ldr     x1, =secondary_high
    br      x1
secondary_high:
    ldr     x1, [x19]
    mov     sp, x1
    ldr     x0, [x19, #8]
    bl      secondary_main
4:  wfe
    b       4b

// If entered at EL2 (e.g. QEMU with virtualization=on), drop to EL1 with the timer accessible.
el2_to_el1:
    mrs     x0, CurrentEL
    cmp     x0, #(2 << 2)
    b.ne    1f
    mov     x0, #(1 << 31)          // HCR_EL2.RW: EL1 is AArch64
    msr     hcr_el2, x0
    mov     x0, #3                  // CNTHCTL_EL2: EL1 may use the physical counter and timer
    msr     cnthctl_el2, x0
    msr     cntvoff_el2, xzr
    mov     x0, #0x3c5              // EL1h, DAIF masked
    msr     spsr_el2, x0
    msr     elr_el2, x30
    eret
1:  ret

// boot_l0[0] and boot_l0[256] -> boot_l1; boot_l1[i] = 1 GB block at i GB (0: devices, others normal)
build_boot_tables:
    adrp    x0, boot_l0
    adrp    x1, boot_l1
    orr     x2, x1, #3              // table descriptor
    str     x2, [x0]
    str     x2, [x0, #(256 * 8)]
    ldr     x2, =BLK_DEVICE
    str     x2, [x1]
    ldr     x2, =BLK_NORMAL
    mov     x3, #1
1:  lsl     x4, x3, #30
    orr     x4, x4, x2
    str     x4, [x1, x3, lsl #3]
    add     x3, x3, #1
    cmp     x3, #512
    b.lo    1b
    dsb     ish
    ret

enable_mmu:
    ldr     x0, =MAIR
    msr     mair_el1, x0
    // TCR_EL1: T0SZ=T1SZ=16 (48-bit), 4 KB granules, inner/outer WB-WA, inner shareable, 16-bit ASIDs,
    // IPS from ID_AA64MMFR0_EL1.PARange
    ldr     x0, =((16 << 0) | (1 << 8) | (1 << 10) | (3 << 12) | (0 << 14) | (16 << 16) | (1 << 24) | (1 << 26) | (3 << 28) | (2 << 30) | (1 << 36))
    mrs     x1, id_aa64mmfr0_el1
    and     x1, x1, #7
    bfi     x0, x1, #32, #3
    msr     tcr_el1, x0
    adrp    x0, boot_l0
    msr     ttbr0_el1, x0
    msr     ttbr1_el1, x0
    isb
    tlbi    vmalle1
    dsb     nsh
    // SCTLR_EL1: RES1 bits | M | C | SA | SA0 | I | DZE | UCT | nTWI | nTWE | UCI
    ldr     x0, =(0x30d00800 | (1 << 0) | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 12) | (1 << 14) | (1 << 15) | (1 << 16) | (1 << 18) | (1 << 26))
    msr     sctlr_el1, x0
    isb
    ic      iallu
    dsb     nsh
    isb
    ret

.section .bss
.balign 4096
.global boot_l0
boot_l0: .space 4096
.global boot_l1
boot_l1: .space 4096
.balign 16
boot_stack: .space 65536
.global boot_stack_top
boot_stack_top:
