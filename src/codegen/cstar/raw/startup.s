;; ============================================================================
;; Vredrs Cstar — Default startup code for bare-metal targets.
;;
;; This file is linked into every `vredrs build --raw` firmware. It provides:
;;   - The reset handler (_start) that sets up the stack pointer and calls
;;     the user's `main()` function.
;;   - A default interrupt vector table (IVT) for ARM Cortex-M3.
;;   - Default handlers for all IRQ exceptions (infinite loop).
;;
;; Target: ARM Cortex-M3 (QEMU -M lm3s6965evb)
;; ============================================================================

    .syntax unified
    .cpu cortex-m3
    .thumb

;; ----------------------------------------------------------------------------
;; Section: .vectors — Interrupt Vector Table
;; ----------------------------------------------------------------------------
.section .vectors, "a", %progbits
.align 2
.global __vectors
__vectors:
    .word _stack_top              ; 0  Initial stack pointer
    .word _start                  ; 1  Reset
    .word _default_handler        ; 2  NMI
    .word _default_handler        ; 3  HardFault
    .word _default_handler        ; 4  MemManage
    .word _default_handler        ; 5  BusFault
    .word _default_handler        ; 6  UsageFault
    .word 0                       ; 7  Reserved
    .word 0                       ; 8  Reserved
    .word 0                       ; 9  Reserved
    .word 0                       ; 10 Reserved
    .word _default_handler        ; 11 SVCall
    .word _default_handler        ; 12 Debug Monitor
    .word 0                       ; 13 Reserved
    .word _default_handler        ; 14 PendSV
    .word _default_handler        ; 15 SysTick
    ;; External interrupts (16+): fill with default handlers.
    .rept 16
    .word _default_handler
    .endr

;; ----------------------------------------------------------------------------
;; Section: .text — Startup code
;; ----------------------------------------------------------------------------
.section .text.startup
.align 2
.global _start
.type _start, %function
_start:
    ;; Set up the stack pointer from the linker symbol.
    ldr r0, =_stack_top
    mov sp, r0

    ;; Zero out the .bss section (required by C runtime).
    ldr r0, =_bss_start
    ldr r1, =_bss_end
    mov r2, #0
.bss_zero_loop:
    cmp r0, r1
    bge .bss_zero_done
    str r2, [r0], #4
    b .bss_zero_loop
.bss_zero_done:

    ;; Optionally copy .data from flash to SRAM (if .data is non-empty).
    ldr r0, =_data_load
    ldr r1, =_data_start
    ldr r2, =_data_end
.data_copy_loop:
    cmp r1, r2
    bge .data_copy_done
    ldr r3, [r0], #4
    str r3, [r1], #4
    b .data_copy_loop
.data_copy_done:

    ;; Call the user's main() function.
    bl main

    ;; If main returns, loop forever (bare-metal has no OS to return to).
.hang:
    b .hang

.size _start, .-_start

;; ----------------------------------------------------------------------------
;; Default handler for all unhandled interrupts.
;; ----------------------------------------------------------------------------
.section .text
.align 2
.global _default_handler
.type _default_handler, %function
_default_handler:
    b _default_handler
.size _default_handler, .-_default_handler

;; ----------------------------------------------------------------------------
;; Weak reference to main (user must define this).
;; ----------------------------------------------------------------------------
.weak main
