/* ============================================================================
 * Vredrs Cstar — Default linker script for ARM Cortex-M3 bare-metal targets.
 *
 * Memory layout for QEMU -M lm3s6965evb:
 *   Flash: 0x00000000, 256 KiB
 *   SRAM:  0x20000000, 64 KiB
 *
 * The script defines the following symbols used by startup.s:
 *   _stack_top     Top of stack (initial SP value)
 *   _bss_start     Start of .bss (zeroed at boot)
 *   _bss_end       End of .bss
 *   _data_load     Load address of .data (in flash)
 *   _data_start    Run address of .data (in SRAM)
 *   _data_end      End of .data
 * ==========================================================================*/

ENTRY(_start)

MEMORY
{
    FLASH (rx)  : ORIGIN = 0x00000000, LENGTH = 256K
    SRAM  (rwx) : ORIGIN = 0x20000000, LENGTH = 64K
}

/* Stack lives at the top of SRAM. */
_stack_top = ORIGIN(SRAM) + LENGTH(SRAM);

SECTIONS
{
    /* --- Interrupt vector table (must be at 0x0) --- */
    .vectors ORIGIN(FLASH) :
    {
        KEEP(*(.vectors))
    } > FLASH

    /* --- Code --- */
    .text :
    {
        *(.text.startup)    /* _start must come first */
        *(.text)
        *(.text.*)
        *(.rodata)
        *(.rodata.*)
        . = ALIGN(4);
    } > FLASH

    /* --- Initialized data (copied from flash to SRAM at boot) --- */
    _data_load = LOADADDR(.data);
    .data : ALIGN(4)
    {
        _data_start = .;
        *(.data)
        *(.data.*)
        . = ALIGN(4);
        _data_end = .;
    } > SRAM AT > FLASH

    /* --- Zero-initialized data --- */
    .bss : ALIGN(4)
    {
        _bss_start = .;
        *(.bss)
        *(.bss.*)
        *(COMMON)
        . = ALIGN(4);
        _bss_end = .;
    } > SRAM

    /* --- Heap grows from end of .bss toward stack --- */
    _heap_start = .;
    _heap_end = _stack_top - 0x1000;  /* Leave 4 KiB for stack */
}
