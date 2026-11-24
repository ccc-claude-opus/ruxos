/* ruxos on AArch64: linked in the linear map (virtual = physical + 0xffff800000000000), loaded by
   ruvm/QEMU at RAM (0x40000000) + text_offset (0x200000). */
OUTPUT_ARCH(aarch64)
ENTRY(_start)
KERNEL_VA = 0xffff800040200000;
SECTIONS
{
    . = KERNEL_VA;
    _start_image = .;
    .text : { KEEP(*(.text.head)) *(.text .text.*) }
    . = ALIGN(4096);
    _etext = .;
    .rodata : { *(.rodata .rodata.*) }
    . = ALIGN(4096);
    .data : { *(.data .data.*) *(.sdata .sdata.*) *(.got .got.*) }
    . = ALIGN(4096);
    _bss_start = .;
    .bss (NOLOAD) : { *(.bss .bss.*) *(.sbss .sbss.*) *(COMMON) }
    . = ALIGN(4096);
    _bss_end = .;
    _end = .;
    _image_size = _end - _start_image;
    /DISCARD/ : { *(.eh_frame*) *(.comment) }
}
