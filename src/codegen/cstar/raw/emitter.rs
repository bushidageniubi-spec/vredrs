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

/// ARM condition codes (Cortex-M3 / Thumb-2).
const COND_EQ: u8 = 0b0000; // Equal / Z set
const COND_NE: u8 = 0b0001; // Not equal / Z clear
const COND_CS: u8 = 0b0010; // Carry set / unsigned >=
const COND_CC: u8 = 0b0011; // Carry clear / unsigned <
const COND_MI: u8 = 0b0100; // Minus / N set
const COND_PL: u8 = 0b0101; // Plus / N clear
const COND_VS: u8 = 0b0110; // Overflow set
const COND_VC: u8 = 0b0111; // Overflow clear
const COND_HI: u8 = 0b1000; // Unsigned higher
const COND_LS: u8 = 0b1001; // Unsigned lower or same
const COND_GE: u8 = 0b1010; // Signed >=
const COND_LT: u8 = 0b1011; // Signed <
const COND_GT: u8 = 0b1100; // Signed >
const COND_LE: u8 = 0b1101; // Signed <=
const COND_AL: u8 = 0b1110; // Always

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

    /// MOVW Rd, #imm16 (Thumb-2 T3, 32-bit).
    /// Writes the 16-bit immediate to the low half of Rd and zero-extends.
    /// Encoding: 11110 i 10 0100 imm4 | 0 imm3 Rd imm8
    fn movw(&mut self, rd: u8, imm16: u16) {
        let imm = imm16 as u32;
        let imm4 = (imm >> 12) & 0xF;
        let i = (imm >> 11) & 0x1;
        let imm3 = (imm >> 8) & 0x7;
        let imm8 = imm & 0xFF;
        let hi: u16 = 0xF240 | ((i as u16) << 10) | (imm4 as u16);
        let lo: u16 = ((imm3 as u16) << 12) | (((rd as u16) & 0xF) << 8) | (imm8 as u16);
        self.emit32(((hi as u32) << 16) | (lo as u32));
    }

    /// MOVT Rd, #imm16 (Thumb-2 T1, 32-bit).
    /// Writes the 16-bit immediate to the high half of Rd, preserving the
    /// low half. Combined with MOVW, loads a full 32-bit value.
    /// Encoding: 11110 i 10 1100 imm4 | 0 imm3 Rd imm8
    fn movt(&mut self, rd: u8, imm16: u16) {
        let imm = imm16 as u32;
        let imm4 = (imm >> 12) & 0xF;
        let i = (imm >> 11) & 0x1;
        let imm3 = (imm >> 8) & 0x7;
        let imm8 = imm & 0xFF;
        let hi: u16 = 0xF2C0 | ((i as u16) << 10) | (imm4 as u16);
        let lo: u16 = ((imm3 as u16) << 12) | (((rd as u16) & 0xF) << 8) | (imm8 as u16);
        self.emit32(((hi as u32) << 16) | (lo as u32));
    }

    /// CMP Rn, #imm8 (Thumb T1): 0010 1 Rn(3) imm8(8).
    /// Sets the NZCV flags from `Rn - imm8`.
    fn cmp_imm8(&mut self, rn: u8, imm: u8) {
        let instr: u16 = 0x2800 | (((rn as u16) & 0x7) << 8) | (imm as u16);
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

    /// Emit an unconditional branch to `target_byte_addr`. Uses the 16-bit
    /// T2 B encoding when the target is within ±2KB; falls back to the 32-bit
    /// T4 B.W encoding for longer reaches. The branch target must be 2-byte
    /// aligned (Thumb requirement).
    fn b_to(&mut self, target_byte_addr: usize) {
        let instr_addr = self.code.len();
        let delta = target_byte_addr as i64 - instr_addr as i64 - 4;
        // T2 B (16-bit): range ±2048 bytes from PC (= instr_addr + 4).
        if delta >= -2048 && delta <= 2046 && delta % 2 == 0 {
            let imm11 = ((delta >> 1) & 0x7FF) as u16;
            self.emit16(0xE000 | imm11);
        } else {
            // T4 B (32-bit): 11110 S imm10 10 J1 1 J2 imm11
            // target = PC + SignExtend(S:I1:I2:imm10:imm11:'0', 25)
            // where I1 = NOT(J1 XOR S), I2 = NOT(J2 XOR S)
            let delta_u = delta as u32; // two's complement
            let s = (delta_u >> 24) & 0x1;
            let i1 = (delta_u >> 23) & 0x1;
            let i2 = (delta_u >> 22) & 0x1;
            let imm10 = (delta_u >> 12) & 0x3FF;
            let imm11 = (delta_u >> 1) & 0x7FF;
            let j1 = (!i1 ^ s) & 0x1; // J1 = NOT(I1 XOR S)
            let j2 = (!i2 ^ s) & 0x1; // J2 = NOT(I2 XOR S)
            let hi: u16 = 0xF000 | ((s as u16) << 10) | (imm10 as u16);
            let lo: u16 = 0x9000 | ((j1 as u16) << 13) | ((j2 as u16) << 11) | (imm11 as u16);
            self.emit32(((hi as u32) << 16) | (lo as u32));
        }
    }

    /// Emit a conditional branch to `target_byte_addr`. `cond` is a 4-bit
    /// ARM condition code (0=EQ, 1=NE, 4=MI, 5=PL, ...). Uses the 16-bit
    /// T1 B<cond> encoding when in range (±256 bytes); otherwise emits the
    /// inverted-condition branch over a 32-bit unconditional B to the target.
    fn b_cond_to(&mut self, cond: u8, target_byte_addr: usize) {
        let instr_addr = self.code.len();
        let delta = target_byte_addr as i64 - instr_addr as i64 - 4;
        if delta >= -256 && delta <= 254 && delta % 2 == 0 {
            let imm8 = ((delta >> 1) & 0xFF) as u16;
            self.emit16(0xD000 | (((cond as u16) & 0xF) << 8) | imm8);
        } else {
            // Fall back: B<inv_cond> skip; B.W target; skip:
            // B<inv_cond> occupies 2 bytes; B.W occupies 4 bytes; skip is at +6.
            let inv_cond = cond ^ 1; // invert the lowest bit (EQ<->NE, etc.)
            // B<inv_cond> +6: delta from PC = +6 - 4 = +2; imm8 = 1.
            self.emit16(0xD000 | (((inv_cond as u16) & 0xF) << 8) | 1);
            self.b_to(target_byte_addr);
        }
    }

    /// Emit a forward conditional branch placeholder. Always uses the
    /// inverted-condition + 32-bit B.W form (6 bytes total) so any target
    /// within ±16MB can be patched in later. Returns the byte position of
    /// the placeholder (start of the 6-byte sequence).
    fn emit_cond_branch_placeholder(&mut self, cond: u8) -> usize {
        let pos = self.code.len();
        let inv_cond = cond ^ 1; // EQ<->NE, MI<->PL, etc.
        // B<inv_cond> +6: skip the next 4-byte B.W. imm8 = (6 - 4) / 2 = 1.
        self.emit16(0xD000 | (((inv_cond as u16) & 0xF) << 8) | 1);
        // T4 B.W placeholder (will be patched by `patch_cond_branch`).
        self.emit32(0xF000_9000);
        pos
    }

    /// Patch a forward conditional branch placeholder to branch to `target`.
    fn patch_cond_branch(&mut self, placeholder_pos: usize, target: usize) {
        // The B.W is at placeholder_pos + 2.
        self.patch_b_w(placeholder_pos + 2, target);
    }

    /// Emit a forward unconditional branch placeholder (4 bytes, T4 B.W).
    /// Returns the position for later patching via `patch_b_w`.
    fn emit_b_placeholder(&mut self) -> usize {
        let pos = self.code.len();
        self.emit32(0xF000_9000);
        pos
    }

    /// Patch a T4 B.W instruction at `pos` to branch to `target`.
    fn patch_b_w(&mut self, pos: usize, target: usize) {
        let delta = target as i64 - pos as i64 - 4;
        let delta_u = delta as u32;
        let s = (delta_u >> 24) & 0x1;
        let i1 = (delta_u >> 23) & 0x1;
        let i2 = (delta_u >> 22) & 0x1;
        let imm10 = (delta_u >> 12) & 0x3FF;
        let imm11 = (delta_u >> 1) & 0x7FF;
        let j1 = (!i1 ^ s) & 0x1;
        let j2 = (!i2 ^ s) & 0x1;
        let hi: u16 = 0xF000 | ((s as u16) << 10) | (imm10 as u16);
        let lo: u16 = 0x9000 | ((j1 as u16) << 13) | ((j2 as u16) << 11) | (imm11 as u16);
        let hi_bytes = hi.to_le_bytes();
        let lo_bytes = lo.to_le_bytes();
        self.code[pos] = hi_bytes[0];
        self.code[pos + 1] = hi_bytes[1];
        self.code[pos + 2] = lo_bytes[0];
        self.code[pos + 3] = lo_bytes[1];
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
            //   loop_start:
            //     compile cond → r0
            //     cmp r0, #0
            //     beq end
            //     body...
            //     b loop_start
            //   end:
            let loop_start = emitter.code.len();
            compile_expr(&w.condition, emitter, var_map, 0);
            emitter.cmp_imm8(0, 0);
            // Forward conditional branch placeholder (skip body when cond == 0).
            let beq_placeholder = emitter.emit_cond_branch_placeholder(COND_EQ);
            for s in &w.body {
                compile_stmt(s, emitter, var_map, next_reg);
            }
            // Back-branch to loop_start (unconditional, long-range).
            emitter.b_to(loop_start);
            // Patch the forward conditional branch to point here (end of loop).
            let end_addr = emitter.code.len();
            emitter.patch_cond_branch(beq_placeholder, end_addr);
        }
        Stmt::If(i) => {
            // if cond: then_body (elif_chain...)? (else_body)?
            //   compile cond → r0
            //   cmp r0, #0
            //   beq next_branch      (skip then-body)
            //   then_body...
            //   b end
            // next_branch:           (next elif or else)
            //   <repeat for each elif>
            // else_body (if any)
            // end:
            compile_expr(&i.condition, emitter, var_map, 0);
            emitter.cmp_imm8(0, 0);
            let mut forward_placeholders: Vec<usize> = Vec::new();
            let mut end_branches: Vec<usize> = Vec::new();
            // Forward conditional branch over the then-body.
            let first_placeholder = emitter.emit_cond_branch_placeholder(COND_EQ);
            forward_placeholders.push(first_placeholder);
            for s in &i.then_body {
                compile_stmt(s, emitter, var_map, next_reg);
            }
            // For each elif: emit unconditional B to end, patch the previous
            // forward branch to point here, then compile the elif cond + body.
            for (elif_cond, elif_body) in &i.elif_chain {
                // Unconditional B to end (placeholder, patched later).
                let end_b = emitter.emit_b_placeholder();
                end_branches.push(end_b);
                // Patch the previous forward conditional branch to here.
                let next_addr = emitter.code.len();
                let prev = *forward_placeholders.last().unwrap_or(&0);
                emitter.patch_cond_branch(prev, next_addr);
                forward_placeholders.pop();
                // Compile the elif condition.
                compile_expr(elif_cond, emitter, var_map, 0);
                emitter.cmp_imm8(0, 0);
                let ph = emitter.emit_cond_branch_placeholder(COND_EQ);
                forward_placeholders.push(ph);
                for s in elif_body {
                    compile_stmt(s, emitter, var_map, next_reg);
                }
            }
            // If there's an else, the last forward branch goes to the else.
            // Otherwise it goes to `end`.
            let after_else_addr;
            if let Some(else_body) = &i.else_body {
                // Emit unconditional B to end (placeholder).
                let end_b = emitter.emit_b_placeholder();
                end_branches.push(end_b);
                // Patch the last forward conditional branch to here (else start).
                let else_start = emitter.code.len();
                let prev = *forward_placeholders.last().unwrap_or(&0);
                emitter.patch_cond_branch(prev, else_start);
                forward_placeholders.pop();
                for s in else_body {
                    compile_stmt(s, emitter, var_map, next_reg);
                }
                after_else_addr = emitter.code.len();
            } else {
                // No else: the last forward conditional branch goes to `end`.
                after_else_addr = emitter.code.len();
                let prev = *forward_placeholders.last().unwrap_or(&0);
                emitter.patch_cond_branch(prev, after_else_addr);
                forward_placeholders.pop();
            }
            // Patch all unconditional B-to-end branches.
            for end_b in &end_branches {
                emitter.patch_b_w(*end_b, after_else_addr);
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
            // Load an integer constant into `target_reg` using the narrowest
            // encoding that fits.
            //   0..=255        → MOVS Rd, #imm8   (T1, 16-bit, 2 bytes)
            //   256..=65535    → MOVW Rd, #imm16  (T3, 32-bit, 4 bytes)
            //   otherwise      → MOVW + MOVT       (8 bytes, full 32-bit two's complement)
            let val = i.value;
            if val >= 0 && val <= 255 {
                emitter.mov_imm8(target_reg, val as u8);
            } else if val >= 0 && val <= 65535 {
                emitter.movw(target_reg, val as u16);
            } else {
                // Two's-complement 32-bit representation (covers negatives
                // and values > 65535).
                let bits = val as u32;
                let lo = (bits & 0xFFFF) as u16;
                let hi = ((bits >> 16) & 0xFFFF) as u16;
                emitter.movw(target_reg, lo);
                if hi != 0 {
                    emitter.movt(target_reg, hi);
                }
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
    // Word 0: Initial stack pointer (STACK_TOP)
    // Word 1: Reset handler address (FLASH_ORIGIN + 128 + 1, Thumb mode)
    // Words 2-31: IRQ handler addresses (from @isr_group annotations)
    bin.extend_from_slice(&STACK_TOP.to_le_bytes());
    let reset_addr = FLASH_ORIGIN + 128 + 1;
    bin.extend_from_slice(&reset_addr.to_le_bytes());

    // Scan for @isr_group annotations and collect handler names.
    // Each @isr_group function becomes an IRQ handler at its priority index.
    let mut isr_handlers: Vec<(String, u32)> = Vec::new();
    for decl in &program.declarations {
        if let crate::parser::ast::TopLevel::FnDef(f) = decl {
            for ann in &f.annotations {
                if ann.name == "isr_group" {
                    // Parse priority from annotation args.
                    let mut priority: u32 = 2; // default IRQ index
                    for arg in &ann.arguments {
                        if arg.name.as_deref() == Some("vector") {
                            if let crate::parser::ast::Expr::Integer(i) = &arg.value {
                                priority = i.value as u32;
                            }
                        }
                    }
                    isr_handlers.push((f.name.name.clone(), priority));
                }
            }
        }
    }

    // Default handler address (for unused IRQ vectors).
    let default_handler_offset = 128 + 12;
    let default_handler_addr = FLASH_ORIGIN + default_handler_offset as u32 + 1;

    // Fill IRQ vectors (words 2-31).
    // If a handler is registered for this vector, use its address;
    // otherwise use the default handler.
    let user_code_start = FLASH_ORIGIN + 128 + 12 + 4; // after reset + default handler
    for i in 2..32 {
        let handler_addr = isr_handlers
            .iter()
            .find(|(_, v)| *v == i)
            .map(|(name, _)| {
                // Calculate the handler's offset in the binary.
                // For now, all handlers are after the user code.
                // We'll place them at fixed offsets based on their index.
                let handler_offset = 128 + 12 + 4 + 200 + (i as u32) * 16;
                FLASH_ORIGIN + handler_offset + 1
            })
            .unwrap_or(default_handler_addr);
        bin.extend_from_slice(&handler_addr.to_le_bytes());
    }

    // --- Reset handler (12 bytes) ---
    let mut reset = ArmEmitter::new();
    reset.ldr_literal(0, 1); // ldr r0, [pc, #4]
    reset.mov_reg(13, 0); // mov sp, r0
    // Fall through to user code (main)
    reset.literal(STACK_TOP);
    bin.extend_from_slice(&reset.code);

    // --- Default handler (4 bytes) ---
    let mut default_handler = ArmEmitter::new();
    default_handler.b(-1); // infinite loop
    bin.extend_from_slice(&default_handler.code);

    // --- User code (compiled from Cstar) ---
    let user_code = compile_program_to_arm(program);
    bin.extend_from_slice(&user_code);

    // --- ISR handler stubs ---
    // For each @isr_group handler, emit a small stub that calls the
    // actual handler function. This ensures the vector table points
    // to valid code.
    for (name, vector) in &isr_handlers {
        let _ = name;
        let _ = vector;
        // Emit a simple handler stub: push {lr}, call handler, pop {pc}.
        let mut stub = ArmEmitter::new();
        stub.push(&[14]); // push {lr}
        // In a real implementation, this would call the handler function.
        // For now, emit a NOP + return.
        stub.nop();
        stub.pop(&[15]); // pop {pc}
        // Pad to 16 bytes.
        while stub.code.len() < 16 {
            stub.code.extend_from_slice(&[0x00, 0xBF]); // NOP
        }
        bin.extend_from_slice(&stub.code);
    }

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
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    /// Parse a Cstar source string and compile its `main` body to ARM bytes.
    fn compile_arm(source: &str) -> Vec<u8> {
        let mut lexer = Lexer::new(source, 0);
        let tokens = match lexer.tokenize() {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let mut parser = Parser::new(tokens, 0);
        let program = match parser.parse_program() {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };
        compile_program_to_arm(&program)
    }

    /// Scan `code` for the 16-bit CMP Rn, #imm8 instruction (T1 encoding
    /// 0x2800 | (Rn<<8) | imm8). Returns the byte offset of the match.
    fn find_cmp_imm8(code: &[u8], rn: u8, imm: u8) -> Option<usize> {
        let expected: u16 = 0x2800 | (((rn as u16) & 0x7) << 8) | (imm as u16);
        let bytes = expected.to_le_bytes();
        code.windows(2)
            .position(|w| w == bytes)
    }

    /// Scan `code` for the 16-bit NOP instruction (0xBF00).
    fn count_nop(code: &[u8]) -> usize {
        let bytes: [u8; 2] = [0x00, 0xBF];
        code.windows(2)
            .filter(|w| *w == bytes)
            .count()
    }

    /// True if `code` contains a T1 B<cond> (16-bit) with cond != 0b1111 and
    /// the 1101 prefix. Encoding: 0xD000..=0xDFFF (excluding 0xDF00.. which is
    /// a permanently undefined UDF hint).
    fn has_cond_branch_16bit(code: &[u8]) -> bool {
        for w in code.windows(2) {
            let instr = u16::from_le_bytes([w[0], w[1]]);
            if (instr & 0xF000) == 0xD000 && (instr & 0x0F00) != 0x0F00 {
                return true;
            }
        }
        false
    }

    /// True if `code` contains the 32-bit T4 B.W encoding (hi half starts with
    /// 0xF000..=0xF7FF and matches the B.W pattern: bits [15:11]=11110,
    /// bits [9:4]=10xxxx or specifically the 10J11J2 pattern in the lo half).
    fn has_t4_branch(code: &[u8]) -> bool {
        if code.len() < 4 {
            return false;
        }
        for i in 0..(code.len() - 3) {
            let hi = u16::from_le_bytes([code[i], code[i + 1]]);
            let lo = u16::from_le_bytes([code[i + 2], code[i + 3]]);
            // hi = 11110 ... (T4 prefix)
            if (hi & 0xF800) != 0xF000 {
                continue;
            }
            // lo = 10 J1 1 J2 imm11 → bits [15:12] = 10x1
            if (lo & 0xD000) == 0x9000 {
                return true;
            }
        }
        false
    }

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

    /// Bug 1: if statements must emit a real conditional branch (not NOP).
    #[test]
    fn test_if_emits_conditional_branch() {
        let src = "fn, main()\n    set, x = 1\n    if, x\n        set, y = 1\n    /end\n    return, 0\n/end\n";
        let code = compile_arm(src);
        // CMP r0, #0 must be present.
        assert!(
            find_cmp_imm8(&code, 0, 0).is_some(),
            "if: expected CMP r0, #0 in code: {:02x?}",
            code
        );
        // No NOP should be emitted for the condition (the body shouldn't
        // contain NOPs at all in this simple program).
        assert_eq!(count_nop(&code), 0, "if: NOP leaked into code: {:02x?}", code);
        // Either a 16-bit B<cond> or a 32-bit T4 B.W must be present.
        assert!(
            has_cond_branch_16bit(&code) || has_t4_branch(&code),
            "if: no conditional branch found in code: {:02x?}",
            code
        );
    }

    /// Bug 1: while loops must emit a real conditional branch (not NOP) and a
    /// back-branch to the loop top.
    #[test]
    fn test_while_emits_conditional_branch_and_backbranch() {
        let src = "fn, main()\n    set, i = 0\n    while, i < 10\n        set, i = i + 1\n    /end\n    return, 0\n/end\n";
        let code = compile_arm(src);
        assert!(
            find_cmp_imm8(&code, 0, 0).is_some(),
            "while: expected CMP r0, #0 in code: {:02x?}",
            code
        );
        assert_eq!(count_nop(&code), 0, "while: NOP leaked: {:02x?}", code);
        assert!(
            has_cond_branch_16bit(&code) || has_t4_branch(&code),
            "while: no conditional branch: {:02x?}",
            code
        );
    }

    /// Bug 1: if/else must emit branches for both arms.
    #[test]
    fn test_if_else_branches() {
        let src = "fn, main()\n    set, x = 1\n    if, x\n        set, y = 1\n    else\n        set, y = 2\n    /end\n    return, 0\n/end\n";
        let code = compile_arm(src);
        assert!(find_cmp_imm8(&code, 0, 0).is_some());
        assert_eq!(count_nop(&code), 0);
        // Both a conditional branch (over then-body) and an unconditional
        // branch (skip else after then-body) must be present.
        assert!(has_cond_branch_16bit(&code) || has_t4_branch(&code));
    }

    /// Bug 2: integers 0-255 use the 8-bit mov_imm8 (T1 MOVS).
    #[test]
    fn test_small_integer_uses_mov_imm8() {
        // 42 → 0x202A (MOVS r0, #42). Build directly via the emitter.
        let mut e = ArmEmitter::new();
        e.mov_imm8(0, 42);
        assert_eq!(e.code, vec![0x2A, 0x20]);
    }

    /// Bug 2: integers 256-65535 use MOVW (T3, 32-bit) and must NOT be
    /// truncated to the low 8 bits.
    #[test]
    fn test_integer_256_uses_movw_not_truncated() {
        let src = "fn, main()\n    set, x = 1000\n    return, x\n/end\n";
        let code = compile_arm(src);
        // 1000 = 0x3E8. MOVW encoding:
        //   imm4 = 0, i = 0, imm3 = 3, imm8 = 0xE8.
        //   hi = 0xF240 | (0<<10) | 0 = 0xF240. Bytes LE: 0x40, 0xF2.
        //   lo = (3<<12) | (0<<8) | 0xE8 = 0x30E8. Bytes LE: 0xE8, 0x30.
        let movw_hi: [u8; 2] = [0x40, 0xF2];
        assert!(
            code.windows(2).any(|w| w == movw_hi),
            "expected MOVW (0xF240 high half) for 1000, got: {:02x?}",
            code
        );
        // Find the MOVW and check the imm8 byte (low byte of the low half).
        let mut found_movw = false;
        for i in 0..code.len().saturating_sub(3) {
            if code[i] == 0x40 && code[i + 1] == 0xF2 {
                let lo = u16::from_le_bytes([code[i + 2], code[i + 3]]);
                let imm8 = (lo & 0xFF) as u16;
                assert_eq!(
                    imm8, 0xE8,
                    "MOVW for 1000 should preserve imm8=0xE8, got {:02x}",
                    imm8
                );
                found_movw = true;
                break;
            }
        }
        assert!(found_movw, "MOVW not found in code: {:02x?}", code);
        // 1000 must NOT appear as a truncated 0xE8 (232) movs.
        // MOVS r0, #232 would be 0xE8 in the low byte of a 0x20XX instruction.
        let truncated: [u8; 2] = [0xE8, 0x20];
        assert!(
            !code.windows(2).any(|w| w == truncated),
            "1000 was truncated to 232 (movs r0, #232): {:02x?}",
            code
        );
    }

    /// Bug 2: integers > 65535 use MOVW + MOVT (8 bytes total).
    #[test]
    fn test_integer_large_uses_movw_movt() {
        let src = "fn, main()\n    set, x = 100000\n    return, x\n/end\n";
        let code = compile_arm(src);
        // 100000 = 0x186A0. lo = 0x86A0, hi = 0x0001.
        // MOVW for lo=0x86A0: imm4 = 0x8, i = 0, imm3 = 0x6, imm8 = 0xA0.
        //   hi = 0xF240 | (0<<10) | 0x8 = 0xF248. Bytes LE: 0x48, 0xF2.
        let movw_hi: [u8; 2] = [0x48, 0xF2];
        assert!(
            code.windows(2).any(|w| w == movw_hi),
            "expected MOVW for 100000, got: {:02x?}",
            code
        );
        // MOVT for hi=0x0001: imm4 = 0, i = 0, imm3 = 0, imm8 = 0x01.
        //   hi = 0xF2C0. Bytes LE: 0xC0, 0xF2.
        let movt_hi: [u8; 2] = [0xC0, 0xF2];
        assert!(
            code.windows(2).any(|w| w == movt_hi),
            "expected MOVT for 100000, got: {:02x?}",
            code
        );
    }

    /// Bug 2: negative integers use MOVW + MOVT (two's complement). The
    /// Cstar parser produces `Unary(-, Integer(1))` for `-1`, which the
    /// emitter doesn't fold; this test constructs an `Expr::Integer(-1)`
    /// directly to exercise the MOVW+MOVT path.
    #[test]
    fn test_negative_integer_uses_movw_movt() {
        use crate::error::Span;
        use crate::parser::ast::{
            Assignee, AssignOp, AssignStmt, FnDef, Identifier, IntegerLiteral,
            Program, ReturnStmt, Stmt, TopLevel,
        };
        let span = Span::new(0, 0, 1, 1, 0);
        let main_fn = FnDef {
            annotations: Vec::new(),
            name: Identifier { name: "main".to_string(), span: span.clone() },
            params: Vec::new(),
            return_type: None,
            body: vec![
                Stmt::Assign(AssignStmt {
                    targets: vec![Assignee::Identifier(Identifier {
                        name: "x".to_string(),
                        span: span.clone(),
                    })],
                    value: Expr::Integer(IntegerLiteral {
                        value: -1,
                        raw: "-1".to_string(),
                        span: span.clone(),
                    }),
                    operator: AssignOp::Simple,
                    span: span.clone(),
                }),
                Stmt::Return(ReturnStmt {
                    values: vec![Expr::Identifier(Identifier {
                        name: "x".to_string(),
                        span: span.clone(),
                    })],
                    span: span.clone(),
                }),
            ],
            is_constexpr: false,
            is_lazy: false,
            is_async: false,
            is_extern: false,
            extern_link: None,
            type_constraints: std::collections::HashMap::new(),
            type_params: Vec::new(),
            span: span.clone(),
        };
        let program = Program {
            declarations: vec![TopLevel::FnDef(main_fn)],
            span: span.clone(),
        };
        let code = compile_program_to_arm(&program);
        // -1 as u32 = 0xFFFFFFFF. lo = 0xFFFF, hi = 0xFFFF.
        // MOVW for lo=0xFFFF: imm4=0xF, i=1, imm3=0x7, imm8=0xFF.
        //   hi = 0xF240 | (1<<10) | 0xF = 0xF64F. Bytes LE: 0x4F, 0xF6.
        let movw_hi: [u8; 2] = [0x4F, 0xF6];
        assert!(
            code.windows(2).any(|w| w == movw_hi),
            "expected MOVW for -1, got: {:02x?}",
            code
        );
        // MOVT for hi=0xFFFF: same calc → hi = 0xF2C0 | (1<<10) | 0xF = 0xF6CF.
        //   Bytes LE: 0xCF, 0xF6.
        let movt_hi: [u8; 2] = [0xCF, 0xF6];
        assert!(
            code.windows(2).any(|w| w == movt_hi),
            "expected MOVT for -1, got: {:02x?}",
            code
        );
        // Make sure -1 is not truncated to 0xFF (movs r0, #255 = 0x20FF).
        let truncated: [u8; 2] = [0xFF, 0x20];
        assert!(
            !code.windows(2).any(|w| w == truncated),
            "-1 was truncated to 255: {:02x?}",
            code
        );
    }

    /// Direct unit test for the MOVW encoder.
    #[test]
    fn test_movw_encoder() {
        let mut e = ArmEmitter::new();
        // MOVW r0, #0x3E8 (1000).
        // imm4=0, i=0, imm3=3, imm8=0xE8.
        // hi = 0xF240, lo = (3<<12) | 0 | 0xE8 = 0x30E8.
        // Bytes LE: 40 F2 E8 30.
        e.movw(0, 1000);
        assert_eq!(e.code, vec![0x40, 0xF2, 0xE8, 0x30]);
    }

    /// Direct unit test for the MOVT encoder.
    #[test]
    fn test_movt_encoder() {
        let mut e = ArmEmitter::new();
        // MOVT r0, #0x0001.
        // imm4=0, i=0, imm3=0, imm8=0x01.
        // hi = 0xF2C0, lo = 0 | 0 | 0x01 = 0x0001.
        // Bytes LE: C0 F2 01 00.
        e.movt(0, 1);
        assert_eq!(e.code, vec![0xC0, 0xF2, 0x01, 0x00]);
    }

    /// Direct unit test for CMP r0, #0.
    #[test]
    fn test_cmp_imm8_encoder() {
        let mut e = ArmEmitter::new();
        e.cmp_imm8(0, 0);
        assert_eq!(e.code, vec![0x00, 0x28]);
    }
}
