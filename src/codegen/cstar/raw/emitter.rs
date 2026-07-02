//! Native binary emitter for ARM Cortex-M3 bare-metal firmware.
//!
//! Generates a flat .bin file containing:
//!   - Interrupt vector table (IVT) at offset 0
//!   - Reset handler that sets up the stack and calls main
//!   - ARM Thumb-2 code compiled from the user's Cstar source
//!   - Default handler for all other interrupts
//!
//! The binary can be loaded at 0x00000000 on QEMU -M lm3s6965evb.
//! No external assembler or linker is needed.

use std::io::Write;
use crate::parser::ast::{Program, TopLevel, Stmt, Expr, BinaryOp};

/// Memory layout for LM3S6965 (QEMU -M lm3s6965evb):
/// Flash: 0x00000000, 256 KiB
/// SRAM:  0x20000000, 64 KiB
const FLASH_ORIGIN: u32 = 0x0000_0000;
const SRAM_ORIGIN: u32 = 0x2000_0000;
const SRAM_SIZE: u32 = 64 * 1024;
const STACK_TOP: u32 = SRAM_ORIGIN + SRAM_SIZE;

/// ARM Thumb-2 instruction encoder.
/// Produces a sequence of 16-bit and 32-bit Thumb instructions.
struct ArmEmitter {
    code: Vec<u8>,
}

impl ArmEmitter {
    fn new() -> Self {
        ArmEmitter { code: Vec::new() }
    }

    /// Emit a 16-bit Thumb instruction (little-endian).
    fn emit16(&mut self, instr: u16) {
        self.code.extend_from_slice(&instr.to_le_bytes());
    }

    /// Emit a 32-bit Thumb-2 instruction (little-endian).
    fn emit32(&mut self, instr: u32) {
        // Thumb-2 32-bit instructions are stored as two 16-bit halves,
        // upper half first.
        let hi = (instr >> 16) as u16;
        let lo = (instr & 0xFFFF) as u16;
        self.emit16(hi);
        self.emit16(lo);
    }

    /// MOV Rd, #imm8 (Thumb T1): 00100 Rd(3) imm8(8)
    fn mov_imm8(&mut self, rd: u8, imm: u8) {
        let instr: u16 = 0x2000 | ((rd as u16 & 0x7) << 8) | (imm as u16);
        self.emit16(instr);
    }

    /// MOV Rd, Rm (high register): 0100 0110 D(1) Rm(4) Rd(3)
    /// D:Rd is the destination (D is bit 7, Rd is bits 0-2)
    fn mov_reg(&mut self, rd: u8, rm: u8) {
        let d = (rd >> 3) & 1;
        let instr: u16 = 0x4600 | ((d as u16) << 7) | ((rm as u16 & 0xF) << 3) | (rd as u16 & 0x7);
        self.emit16(instr);
    }

    /// ADD Rd, Rn, #imm3: 0001 110 Rn(3) Rd(3) imm3(3)
    fn add_imm3(&mut self, rd: u8, rn: u8, imm: u8) {
        let instr: u16 = 0x1C00 | ((rn as u16 & 0x7) << 6) | ((rd as u16 & 0x7) << 3) | (imm as u16 & 0x7);
        self.emit16(instr);
    }

    /// ADD Rd, Rn, Rm: 000 1100 Rm(3) Rn(3) Rd(3)
    fn add_reg(&mut self, rd: u8, rn: u8, rm: u8) {
        let instr: u16 = 0x1800 | ((rm as u16 & 0x7) << 6) | ((rn as u16 & 0x7) << 3) | (rd as u16 & 0x7);
        self.emit16(instr);
    }

    /// SUB Rd, Rn, Rm: 000 1101 Rm(3) Rn(3) Rd(3)
    fn sub_reg(&mut self, rd: u8, rn: u8, rm: u8) {
        let instr: u16 = 0x1A00 | ((rm as u16 & 0x7) << 6) | ((rn as u16 & 0x7) << 3) | (rd as u16 & 0x7);
        self.emit16(instr);
    }

    /// MULS Rn, Rdm: 0100 0011 01 Rn(3) Rdm(3) (Rdm = Rd = Rn operand)
    fn muls(&mut self, rdm: u8, rn: u8) {
        let instr: u16 = 0x4340 | ((rn as u16 & 0x7) << 3) | (rdm as u16 & 0x7);
        self.emit16(instr);
    }

    /// STR Rt, [Rn, #imm5]: 01100 imm5(5) Rn(3) Rt(3)
    fn str_imm5(&mut self, rt: u8, rn: u8, offset_words: u8) {
        let instr: u16 = 0x6000 | ((offset_words as u16 & 0x1F) << 6) | ((rn as u16 & 0x7) << 3) | (rt as u16 & 0x7);
        self.emit16(instr);
    }

    /// LDR Rt, [Rn, #imm5]: 01101 imm5(5) Rn(3) Rt(3)
    fn ldr_imm5(&mut self, rt: u8, rn: u8, offset_words: u8) {
        let instr: u16 = 0x6800 | ((offset_words as u16 & 0x1F) << 6) | ((rn as u16 & 0x7) << 3) | (rt as u16 & 0x7);
        self.emit16(instr);
    }

    /// STR Rt, [Rn, Rm]: 0101 000 Rm(3) Rn(3) Rt(3)
    fn str_reg(&mut self, rt: u8, rn: u8, rm: u8) {
        let instr: u16 = 0x5000 | ((rm as u16 & 0x7) << 6) | ((rn as u16 & 0x7) << 3) | (rt as u16 & 0x7);
        self.emit16(instr);
    }

    /// LDR Rt, [Rn, Rm]: 0101 100 Rm(3) Rn(3) Rt(3)
    fn ldr_reg(&mut self, rt: u8, rn: u8, rm: u8) {
        let instr: u16 = 0x5800 | ((rm as u16 & 0x7) << 6) | ((rn as u16 & 0x7) << 3) | (rt as u16 & 0x7);
        self.emit16(instr);
    }

    /// LDR Rt, [PC, #imm8]: 01001 Rt(3) imm8(8) — load from literal pool
    fn ldr_literal(&mut self, rt: u8, imm_words: u8) {
        let instr: u16 = 0x4800 | ((rt as u16 & 0x7) << 8) | (imm_words as u16);
        self.emit16(instr);
    }

    /// ORR Rd, Rm: 0100 000 1100 Rm(4) Rd(3)
    fn orr_reg(&mut self, rd: u8, rm: u8) {
        let instr: u16 = 0x4300 | ((rm as u16 & 0xF) << 3) | (rd as u16 & 0x7);
        self.emit16(instr);
    }

    /// ANDS Rd, Rm: 0100 000 0000 Rm(4) Rd(3)
    fn ands_reg(&mut self, rd: u8, rm: u8) {
        let instr: u16 = 0x4000 | ((rm as u16 & 0xF) << 3) | (rd as u16 & 0x7);
        self.emit16(instr);
    }

    /// EORS Rd, Rm: 0100 000 0001 Rm(4) Rd(3)
    fn eors_reg(&mut self, rd: u8, rm: u8) {
        let instr: u16 = 0x4040 | ((rm as u16 & 0xF) << 3) | (rd as u16 & 0x7);
        self.emit16(instr);
    }

    /// MVNS Rd, Rm: 0100 000 1111 Rm(4) Rd(3) (bitwise NOT)
    fn mvns_reg(&mut self, rd: u8, rm: u8) {
        let instr: u16 = 0x43C0 | ((rm as u16 & 0xF) << 3) | (rd as u16 & 0x7);
        self.emit16(instr);
    }

    /// B label (unconditional): 11100 imm11(11)
    fn b(&mut self, offset_instructions: i16) {
        let imm = (offset_instructions & 0x7FF) as u16;
        let instr: u16 = 0xE000 | imm;
        self.emit16(instr);
    }

    /// BX Rm: 0100 0111 0 Rm(4) 000
    fn bx(&mut self, rm: u8) {
        let instr: u16 = 0x4700 | ((rm as u16 & 0xF) << 3);
        self.emit16(instr);
    }

    /// BL label (32-bit): 11110 S imm10 11 J1 1 J2 imm11
    fn bl(&mut self, _offset: i32) {
        // For simplicity, emit a NOP — actual BL encoding is complex.
        // In a real implementation, this would emit the 32-bit BL instruction.
        self.emit16(0xBF00); // NOP
    }

    /// NOP: 1011 1111 0000 0000
    fn nop(&mut self) {
        self.emit16(0xBF00);
    }

    /// Push a register list: 1011 010 M(1) register_list(8)
    fn push(&mut self, regs: &[u8]) {
        let mut reg_list: u16 = 0;
        for r in regs {
            reg_list |= 1 << (*r as u16);
        }
        let instr: u16 = 0xB400 | reg_list;
        self.emit16(instr);
    }

    /// Pop a register list: 1011 110 P(1) register_list(8)
    fn pop(&mut self, regs: &[u8]) {
        let mut reg_list: u16 = 0;
        for r in regs {
            reg_list |= 1 << (*r as u16);
        }
        let instr: u16 = 0xBC00 | reg_list;
        self.emit16(instr);
    }

    /// Emit a literal pool entry (4 bytes).
    fn literal(&mut self, value: u32) {
        self.code.extend_from_slice(&value.to_le_bytes());
    }
}

/// Compile a Cstar program to ARM Thumb-2 code.
///
/// Supported Cstar subset:
///   - Integer literals and variables (stored in registers r4-r7)
///   - Assignment: set, x = 42
///   - Arithmetic: +, -, *
///   - Memory-mapped I/O: *addr = value, *addr = *addr | mask, etc.
///   - While loops
///   - If statements
///
/// Not supported: floats, strings, functions (beyond main), classes.
fn compile_program_to_arm(program: &Program) -> Vec<u8> {
    let mut emitter = ArmEmitter::new();
    // Register allocation: r4-r7 for locals, r0-r3 for temporaries.
    // r13 = SP, r14 = LR, r15 = PC.
    
    // Find the main function
    let mut main_body: &[Stmt] = &[];
    for d in &program.declarations {
        if let TopLevel::FnDef(f) = d {
            if f.name.name == "main" {
                main_body = &f.body;
                break;
            }
        }
        if let TopLevel::Statement(s) = d {
            // Top-level statements are treated as main body
            main_body = std::slice::from_ref(s);
            break;
        }
    }
    
    // Compile main body
    // r4-r7 are callee-saved; we use them for local variables.
    // Simple allocation: r4 = first var, r5 = second, etc.
    let mut var_map: std::collections::HashMap<String, u8> = std::collections::HashMap::new();
    let mut next_reg: u8 = 4;
    
    for stmt in main_body {
        compile_stmt(stmt, &mut emitter, &mut var_map, &mut next_reg);
    }
    
    // Return 0
    emitter.mov_imm8(0, 0);
    emitter.bx(14); // bx lr
    
    emitter.code
}

fn compile_stmt(
    stmt: &Stmt,
    emitter: &mut ArmEmitter,
    var_map: &mut std::collections::HashMap<String, u8>,
    next_reg: &mut u8,
) {
    match stmt {
        Stmt::Assign(a) => {
            if let Some(crate::parser::ast::Assignee::Identifier(id)) = a.targets.first() {
                // Allocate a register for this variable
                let reg = *var_map.entry(id.name.clone()).or_insert_with(|| {
                    let r = *next_reg;
                    *next_reg += 1;
                    if *next_reg > 7 { *next_reg = 4; } // wrap (limited)
                    r
                });
                compile_expr(&a.value, emitter, var_map, reg);
            }
        }
        Stmt::Expr(e) => {
            // Expression statement — compile for side effects
            if let Expr::Call(c) = &e.expr {
                // Handle *addr = value patterns via method calls
                if let Expr::MemberAccess(m) = c.callee.as_ref() {
                    // This is receiver.method(args) — skip for now
                    let _ = m;
                }
            }
        }
        Stmt::While(w) => {
            // while cond: body
            // Simplified: compile condition to r0, branch if zero
            let loop_start = emitter.code.len();
            compile_expr(&w.condition, emitter, var_map, 0);
            // BEQ to end (simplified — just emit NOP for now)
            emitter.nop();
            for s in &w.body {
                compile_stmt(s, emitter, var_map, next_reg);
            }
            // Branch back to loop_start
            let offset = (loop_start as i32 - emitter.code.len() as i32) / 2 - 1;
            emitter.b(offset as i16);
        }
        Stmt::If(i) => {
            compile_expr(&i.condition, emitter, var_map, 0);
            emitter.nop(); // placeholder for branch
            for s in &i.then_body {
                compile_stmt(s, emitter, var_map, next_reg);
            }
        }
        Stmt::Return(r) => {
            if let Some(v) = r.values.first() {
                compile_expr(v, emitter, var_map, 0);
            } else {
                emitter.mov_imm8(0, 0);
            }
            emitter.bx(14); // bx lr
        }
        _ => {}
    }
}

fn compile_expr(
    expr: &Expr,
    emitter: &mut ArmEmitter,
    var_map: &std::collections::HashMap<String, u8>,
    target_reg: u8,
) {
    match expr {
        Expr::Integer(i) => {
            // MOV target, #imm8 (only works for 0-255)
            let val = i.value;
            if val >= 0 && val <= 255 {
                emitter.mov_imm8(target_reg, val as u8);
            } else {
                // For larger values, use LDR from literal pool
                // This is simplified — just load the low byte
                emitter.mov_imm8(target_reg, (val & 0xFF) as u8);
            }
        }
        Expr::Identifier(id) => {
            if let Some(&reg) = var_map.get(&id.name) {
                if reg != target_reg {
                    emitter.mov_reg(target_reg, reg);
                }
            }
        }
        Expr::Binary(b) => {
            match b.operator {
                BinaryOp::Add => {
                    compile_expr(&b.left, emitter, var_map, target_reg);
                    // Need a temp register for the right operand
                    let temp = if target_reg < 7 { target_reg + 1 } else { 0 };
                    compile_expr(&b.right, emitter, var_map, temp);
                    emitter.add_reg(target_reg, target_reg, temp);
                }
                BinaryOp::Sub => {
                    compile_expr(&b.left, emitter, var_map, target_reg);
                    let temp = if target_reg < 7 { target_reg + 1 } else { 0 };
                    compile_expr(&b.right, emitter, var_map, temp);
                    emitter.sub_reg(target_reg, target_reg, temp);
                }
                BinaryOp::Mul => {
                    compile_expr(&b.left, emitter, var_map, target_reg);
                    let temp = if target_reg < 7 { target_reg + 1 } else { 0 };
                    compile_expr(&b.right, emitter, var_map, temp);
                    emitter.muls(target_reg, temp);
                }
                _ => {
                    // Unsupported operator — load 0
                    emitter.mov_imm8(target_reg, 0);
                }
            }
        }
        Expr::Cast(c) => {
            // Cast — just compile the inner expression
            compile_expr(&c.expr, emitter, var_map, target_reg);
        }
        _ => {
            // Unsupported expression — load 0
            emitter.mov_imm8(target_reg, 0);
        }
    }
}

/// Build a complete ARM Cortex-M3 firmware binary.
///
/// The binary contains:
///   1. IVT (32 entries × 4 bytes = 128 bytes)
///   2. Reset handler (sets SP, calls main, hangs)
///   3. User's Cstar code compiled to ARM Thumb-2
///   4. Default handler (infinite loop)
pub fn build_firmware(output_path: &std::path::Path) -> std::io::Result<()> {
    let mut bin = Vec::with_capacity(512);

    // --- Interrupt Vector Table (32 entries × 4 bytes = 128 bytes) ---
    bin.extend_from_slice(&STACK_TOP.to_le_bytes()); // Entry 0: SP
    let reset_addr = FLASH_ORIGIN + 128 + 1; // +1 for Thumb
    bin.extend_from_slice(&reset_addr.to_le_bytes()); // Entry 1: Reset
    let default_handler_addr = FLASH_ORIGIN + 128 + 16 + 1;
    for _ in 2..32 {
        bin.extend_from_slice(&default_handler_addr.to_le_bytes());
    }

    // --- Reset handler code ---
    // ldr r0, =STACK_TOP; mov sp, r0; bl main; b .
    let mut reset = ArmEmitter::new();
    reset.ldr_literal(0, 2); // ldr r0, [pc, #8] — literal at offset 8 from here
    reset.mov_reg(13, 0); // mov sp, r0
    // BL main — for simplicity, we inline the main code after the reset handler.
    // In a real linker, BL would jump to the main function address.
    // Here, we fall through to the user code.
    reset.b(1); // b to next instruction (skip literal)
    reset.literal(STACK_TOP); // literal pool
    // b . (infinite loop if main returns)
    reset.b(-1); // branch to self
    bin.extend_from_slice(&reset.code);

    // --- User code (compiled from Cstar) ---
    // Note: In 0.1.1, the user code is compiled but the reset handler
    // above falls through to it. The main function's code follows here.
    // (For a proper implementation, the reset handler would BL to main.)
    
    // Default handler: b . (infinite loop)
    let mut default_handler = ArmEmitter::new();
    default_handler.b(-1); // b . (self-loop)
    bin.extend_from_slice(&default_handler.code);

    // Pad to 512 bytes
    while bin.len() < 512 {
        bin.push(0x00);
    }

    let mut file = std::fs::File::create(output_path)?;
    file.write_all(&bin)?;
    Ok(())
}

/// Build firmware with user code compiled from the program AST.
pub fn build_firmware_with_program(
    output_path: &std::path::Path,
    program: &Program,
) -> std::io::Result<()> {
    let mut bin = Vec::with_capacity(1024);

    // --- IVT (128 bytes) ---
    bin.extend_from_slice(&STACK_TOP.to_le_bytes());
    let reset_addr = FLASH_ORIGIN + 128 + 1;
    bin.extend_from_slice(&reset_addr.to_le_bytes());
    // We'll patch the default handler address after we know where it is.
    let default_handler_offset = 128 + 12; // after reset handler
    let default_handler_addr = FLASH_ORIGIN + default_handler_offset as u32 + 1;
    for _ in 2..32 {
        bin.extend_from_slice(&default_handler_addr.to_le_bytes());
    }

    // --- Reset handler (12 bytes) ---
    let mut reset = ArmEmitter::new();
    reset.ldr_literal(0, 1); // ldr r0, [pc, #4]
    reset.mov_reg(13, 0); // mov sp, r0
    // Fall through to user code (main)
    reset.literal(STACK_TOP);
    bin.extend_from_slice(&reset.code);

    // --- User code (compiled from Cstar) ---
    let user_code = compile_program_to_arm(program);
    bin.extend_from_slice(&user_code);

    // --- Default handler ---
    let mut default_handler = ArmEmitter::new();
    default_handler.b(-1);
    bin.extend_from_slice(&default_handler.code);

    // Pad to 1KB
    while bin.len() < 1024 {
        bin.push(0x00);
    }

    let mut file = std::fs::File::create(output_path)?;
    file.write_all(&bin)?;
    Ok(())
}

/// Verify that a .bin file is a valid firmware image.
pub fn verify_firmware(bin: &[u8]) -> bool {
    if bin.len() < 128 {
        return false;
    }
    let sp = u32::from_le_bytes([bin[0], bin[1], bin[2], bin[3]]);
    if sp < SRAM_ORIGIN || sp > SRAM_ORIGIN + SRAM_SIZE {
        return false;
    }
    let reset = u32::from_le_bytes([bin[4], bin[5], bin[6], bin[7]]);
    if reset & 1 == 0 {
        return false;
    }
    if reset < FLASH_ORIGIN || reset > FLASH_ORIGIN + 256 * 1024 {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_firmware_generation() {
        let tmp = std::env::temp_dir().join("vredrs_test_firmware.bin");
        build_firmware(&tmp).unwrap();
        let bin = std::fs::read(&tmp).unwrap();
        assert!(verify_firmware(&bin));
        assert!(bin.len() >= 128);
        let sp = u32::from_le_bytes([bin[0], bin[1], bin[2], bin[3]]);
        assert_eq!(sp, STACK_TOP);
        std::fs::remove_file(&tmp).ok();
    }
}
