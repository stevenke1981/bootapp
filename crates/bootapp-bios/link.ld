ENTRY(image_start)
SECTIONS {
  . = 0x10000;
  .start : { KEEP(*(.start)) __start_end = .; }
  .text : { *(.text .text.*) }
  .rodata : { *(.rodata .rodata.*) }
  .data : { *(.data .data.*) }
  __image_end = .;
  .bss : { __bss_start = .; *(.bss .bss.* COMMON) __bss_end = .; }
  /DISCARD/ : { *(.eh_frame*) *(.comment*) *(.note*) *(.got*) }
}
/* Real-mode code/data use 1000:(label - 0x10000); keep it inside that window. */
ASSERT(__start_end <= 0x20000, "stage2 .start must stay inside the real-mode 0x1000 segment")
/* Stage1 loads exactly 120 sectors (0x10000..0x1efff). */
ASSERT(__image_end <= 0x1f000, "stage2 loaded image exceeds the 120-sector stage1 window")
/* The protected-mode stack top is never below 0x20000 (see stage2.asm). */
ASSERT(__bss_end <= 0x20000, "stage2 BSS would overlap the minimum stack region")
