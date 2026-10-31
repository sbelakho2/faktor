/* HAR-18 emulated-HIL firmware link script: TI LM3S6965 (Cortex-M3) as
 * modelled by QEMU's `lm3s6965evb` machine. Flash is mapped at 0x0 and the
 * ELF -kernel loader maps every loadable segment at its vaddr, so the
 * vector table lands on the reset address. RAM: 64 KiB at 0x20000000; the
 * initial stack pointer is hard-coded to 0x20008000 in the vector table
 * (well inside RAM, grows down over unused space).
 */
MEMORY {
  FLASH (rx)  : ORIGIN = 0x00000000, LENGTH = 256K
  RAM   (rwx) : ORIGIN = 0x20000000, LENGTH = 64K
}

ENTRY(reset_handler)

SECTIONS {
  .text : {
    KEEP(*(.vector_table))
    *(.text*)
    *(.rodata*)
  } > FLASH

  /DISCARD/ : {
    *(.ARM.exidx*)
    *(.ARM.extab*)
    *(.eh_frame*)
  }
}
