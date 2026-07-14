//! # emit_raw_arm — Static IR → ARM/AArch64 汇编发射器
//!
//! 手册 Phase 1/2 的 Raw 后端新路径（ARM 侧）。消费 [`crate::codegen::ir::IrModule`]。
//!
//! 支持两种目标：
//! - **AArch64**（arm64 主机）：生成 AArch64 汇编，可原生组装链接。
//! - **ARM Thumb-2**（Cortex-M3 固件）：生成 Thumb-2 汇编，用于 QEMU。
//!
//! 与 [`crate::codegen::emit_raw_x86`] 共享同一个 IR 输入，行为等价（手册 §3.3）。

use crate::codegen::ir::{BinOp, Cond, IrFunction, IrModule, Operand, StaticInsn, TypeHint};
use crate::parser::ast::Program;
use std::collections::HashMap;
use std::path::Path;

/// IR → ARM 汇编发射器。
pub struct ArmEmitter {
    out: String,
    /// 虚拟寄存器名 → 栈偏移（相对 sp/fp）。
    stack_slots: HashMap<String, i64>,
    /// 虚拟寄存器名 → 物理寄存器名（寄存器分配）。
    reg_alloc: HashMap<String, String>,
    /// 当前函数用到的 callee-saved 寄存器。
    used_callee_saved: Vec<String>,
    frame_size: i64,
    str_labels: Vec<String>,
    /// 是否为 AArch64（true）或 ARM32（false）。
    is_aarch64: bool,
}

impl ArmEmitter {
    pub fn new(is_aarch64: bool) -> Self {
        ArmEmitter {
            out: String::new(),
            stack_slots: HashMap::new(),
            reg_alloc: HashMap::new(),
            used_callee_saved: Vec::new(),
            frame_size: 0,
            str_labels: Vec::new(),
            is_aarch64,
        }
    }

    /// 发射整个 IR 模块为 ARM 汇编字符串。
    pub fn emit_module(&mut self, m: &IrModule) -> String {
        self.out.clear();
        if self.is_aarch64 {
            self.emit_module_aarch64(m);
        } else {
            self.emit_module_arm32(m);
        }
        self.out.clone()
    }

    fn emit_module_aarch64(&mut self, m: &IrModule) {
        self.out.push_str("    .arch armv8-a\n");
        self.out.push_str("    .section .rodata\n");
        for (i, s) in m.string_pool.iter().enumerate() {
            let label = format!(".str{}", i);
            self.str_labels.push(label.clone());
            self.out.push_str(&format!("{}:\n", label));
            self.out.push_str(&format!("    .asciz \"{}\"\n", escape_str(s)));
        }
        self.out.push_str("    .section .text\n");
        self.out.push_str("    .globl main\n");
        // extern 声明。
        let mut exts: Vec<String> = Vec::new();
        for f in &m.functions {
            for i in &f.body {
                collect_externs(i, &mut exts);
            }
        }
        exts.sort();
        exts.dedup();
        for e in &exts {
            self.out.push_str(&format!("    .extern {}\n", e));
        }
        for f in &m.functions {
            self.emit_function_aarch64(f);
        }
        // _start stub。
        self.out.push_str("\n    .globl _start\n_start:\n");
        self.out.push_str("    bl main\n");
        self.out.push_str("    mov x8, x0\n");
        self.out.push_str("    mov x0, #1\n"); // _exit syscall
        self.out.push_str("    svc #0\n");
    }

    fn emit_function_aarch64(&mut self, f: &IrFunction) {
        self.stack_slots.clear();
        self.reg_alloc.clear();
        self.used_callee_saved.clear();
        self.frame_size = 0;
        self.out.push_str(&format!("\n{}:\n", f.name));
        // Prologue: stp x29, x30, [sp, #-16]!; mov x29, sp
        self.out.push_str("    stp x29, x30, [sp, #-16]!\n");
        self.out.push_str("    mov x29, sp\n");
        // 计算栈帧大小。
        let mut max_slot = 0i64;
        for insn in &f.body {
            collect_regs(insn, &mut |r| {
                if r.starts_with('v') {
                    let idx: i64 = r[1..].parse().unwrap_or(0);
                    if idx > max_slot {
                        max_slot = idx;
                    }
                }
            });
        }
        let n_params = f.params.len() as i64;
        let total_slots = max_slot.max(n_params) + 1;
        self.frame_size = ((total_slots * 8 + 15) / 16) * 16;

        // 寄存器分配：把前几个虚拟寄存器映射到 callee-saved x19-x23。
        let callee_regs = ["x19", "x20", "x21", "x22", "x23"];
        let mut alloc_order: Vec<String> = f.param_regs.clone();
        for i in 0..=max_slot {
            let name = format!("v{}", i);
            if !alloc_order.contains(&name) {
                alloc_order.push(name);
            }
        }
        for (i, vreg) in alloc_order.iter().enumerate() {
            if i < callee_regs.len() {
                self.reg_alloc.insert(vreg.clone(), callee_regs[i].to_string());
                self.used_callee_saved.push(callee_regs[i].to_string());
            }
        }
        for i in 0..=max_slot {
            let name = format!("v{}", i);
            if !self.reg_alloc.contains_key(&name) {
                self.stack_slots.insert(name, 8 * (i + 1));
            }
        }

        // Prologue：保存 callee-saved 寄存器（stp 成对，对齐 16）。
        let n_callee = self.used_callee_saved.len();
        let pairs = n_callee / 2;
        let odd = n_callee % 2 == 1;
        let extra = if odd { 16 } else { 0 }; // 单个寄存器用 str，需要补 8 字节对齐
        for p in 0..pairs {
            let r1 = &self.used_callee_saved[p * 2];
            let r2 = &self.used_callee_saved[p * 2 + 1];
            self.out.push_str(&format!("    stp {}, {}, [sp, #-16]!\n", r1, r2));
        }
        if odd {
            let r = &self.used_callee_saved[n_callee - 1];
            self.out.push_str(&format!("    str {}, [sp, #-16]!\n", r));
        }
        if self.frame_size > 0 {
            self.out.push_str(&format!("    sub sp, sp, #{}\n", self.frame_size + extra));
        } else if extra > 0 {
            self.out.push_str(&format!("    sub sp, sp, #{}\n", extra));
        }

        // 参数：x0..x7 → 物理寄存器或栈槽。
        let arg_regs = ["x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7"];
        for (i, reg_name) in f.param_regs.iter().enumerate() {
            if i < 8 {
                self.store_to_vreg_aarch64(reg_name, arg_regs[i]);
            }
        }
        // 发射函数体，应用 fallthrough 优化。
        let body = &f.body;
        for (idx, insn) in body.iter().enumerate() {
            if let StaticInsn::Jump { target } = insn {
                if idx + 1 < body.len() {
                    if let StaticInsn::Label(next) = &body[idx + 1] {
                        if next == target {
                            continue;
                        }
                    }
                }
            }
            self.emit_insn_aarch64(insn);
        }
        if !matches!(f.body.last(), Some(StaticInsn::Ret { .. })) {
            self.emit_epilogue_aarch64();
        }
    }

    fn emit_epilogue_aarch64(&mut self) {
        let n_callee = self.used_callee_saved.len();
        let pairs = n_callee / 2;
        let odd = n_callee % 2 == 1;
        let extra = if odd { 16 } else { 0 };
        if self.frame_size > 0 || extra > 0 {
            self.out.push_str(&format!("    add sp, sp, #{}\n", self.frame_size + extra));
        }
        // 逆序恢复 callee-saved。
        if odd {
            let r = &self.used_callee_saved[n_callee - 1];
            self.out.push_str(&format!("    ldr {}, [sp], #16\n", r));
        }
        for p in (0..pairs).rev() {
            let r1 = &self.used_callee_saved[p * 2];
            let r2 = &self.used_callee_saved[p * 2 + 1];
            self.out.push_str(&format!("    ldp {}, {}, [sp], #16\n", r1, r2));
        }
        self.out.push_str("    ldp x29, x30, [sp], #16\n");
        self.out.push_str("    ret\n");
    }

    /// 把物理寄存器值存到虚拟寄存器（优先物理寄存器，否则栈槽）。
    fn store_to_vreg_aarch64(&mut self, vreg_name: &str, phys_reg: &str) {
        if let Some(preg) = self.reg_alloc.get(vreg_name).cloned() {
            if preg != phys_reg {
                self.out.push_str(&format!("    mov {}, {}\n", preg, phys_reg));
            }
            return;
        }
        self.store_reg_to_slot(vreg_name, phys_reg);
    }

    fn emit_insn_aarch64(&mut self, insn: &StaticInsn) {
        use StaticInsn::*;
        match insn {
            Label(l) => self.out.push_str(&format!("{}:\n", sanitize_label(l))),
            Comment(c) => self.out.push_str(&format!("    // {}\n", c)),
            SrcLoc { line, col } => self.out.push_str(&format!("    // srcloc {}:{}\n", line, col)),
            Mov { dst, src, .. } => {
                self.load_operand_aarch64(src, "x9");
                self.store_operand_aarch64(dst, "x9");
            }
            Box { dst, src, from } => {
                let func = match from {
                    TypeHint::I64 => "vredrs_value_make_i64",
                    TypeHint::F64 => "vredrs_value_make_f64",
                    TypeHint::Bool => "vredrs_value_make_bool",
                    _ => "",
                };
                if !func.is_empty() {
                    self.load_operand_aarch64(src, "x0");
                    self.out.push_str(&format!("    bl {}\n", func));
                    self.store_operand_aarch64(dst, "x0");
                } else {
                    self.load_operand_aarch64(src, "x9");
                    self.store_operand_aarch64(dst, "x9");
                }
            }
            Unbox { dst, src, to } => {
                let func = match to {
                    TypeHint::I64 => "vredrs_value_get_i64",
                    TypeHint::F64 => "vredrs_value_get_f64",
                    TypeHint::Bool => "vredrs_value_get_bool",
                    _ => "",
                };
                if !func.is_empty() {
                    self.load_operand_aarch64(src, "x0");
                    self.out.push_str(&format!("    bl {}\n", func));
                    self.store_operand_aarch64(dst, "x0");
                } else {
                    self.load_operand_aarch64(src, "x9");
                    self.store_operand_aarch64(dst, "x9");
                }
            }
            Bin { op, dst, lhs, rhs, ty } => self.emit_bin_aarch64(*op, dst, lhs, rhs, *ty),
            Neg { dst, src, ty } => {
                if *ty == TypeHint::F64 {
                    self.load_operand_aarch64(src, "x0");
                    self.out.push_str("    bl vredrs_f64_neg\n");
                    self.store_operand_aarch64(dst, "x0");
                } else {
                    self.load_operand_aarch64(src, "x9");
                    self.out.push_str("    neg x9, x9\n");
                    self.store_operand_aarch64(dst, "x9");
                }
            }
            Not { dst, src, .. } => {
                self.load_operand_aarch64(src, "x9");
                self.out.push_str("    eor x9, x9, #1\n");
                self.store_operand_aarch64(dst, "x9");
            }
            Cmp { dst, cond, lhs, rhs, .. } => {
                self.load_operand_aarch64(lhs, "x9");
                // 优化：与 0 比较用 cbz/cbnz 风格（此处 cmp 仍需，但立即数路径更短）。
                if let Operand::ImmI64(v) = rhs {
                    self.out.push_str(&format!("    cmp x9, {}\n", v));
                } else {
                    self.load_operand_aarch64(rhs, "x10");
                    self.out.push_str("    cmp x9, x10\n");
                }
                let cset = cond_to_cset_aarch64(*cond);
                self.out.push_str(&format!("    cset x9, {}\n", cset));
                self.store_operand_aarch64(dst, "x9");
            }
            Branch { cond, lhs, rhs, target } => {
                self.load_operand_aarch64(lhs, "x9");
                if let Operand::ImmI64(v) = rhs {
                    self.out.push_str(&format!("    cmp x9, {}\n", v));
                } else {
                    self.load_operand_aarch64(rhs, "x10");
                    self.out.push_str("    cmp x9, x10\n");
                }
                let b = cond_to_branch_aarch64(*cond);
                self.out.push_str(&format!("    b.{} {}\n", b, sanitize_label(target)));
            }
            BranchTrue { cond, target } => {
                self.load_operand_aarch64(cond, "x9");
                self.out.push_str("    cbnz x9, ");
                self.out.push_str(&sanitize_label(target));
                self.out.push('\n');
            }
            Jump { target } => {
                self.out.push_str("    b ");
                self.out.push_str(&sanitize_label(target));
                self.out.push('\n');
            }
            Load { dst, addr, .. } => {
                self.load_operand_aarch64(addr, "x9");
                self.out.push_str("    ldr x9, [x9]\n");
                self.store_operand_aarch64(dst, "x9");
            }
            Store { addr, value, .. } => {
                self.load_operand_aarch64(addr, "x10");
                self.load_operand_aarch64(value, "x9");
                self.out.push_str("    str x9, [x10]\n");
            }
            Alloca { dst, size, .. } => {
                self.load_operand_aarch64(size, "x9");
                self.out.push_str("    sub sp, sp, x9\n");
                self.out.push_str("    mov x9, sp\n");
                self.store_operand_aarch64(dst, "x9");
            }
            StackFree { .. } => {}
            Call { dst, func, args, .. } => self.emit_call_aarch64(dst, func, args),
            CallDyn { dst, callee, args, .. } => {
                self.load_operand_aarch64(callee, "x0");
                let regs = ["x1", "x2", "x3", "x4", "x5", "x6", "x7"];
                for (i, a) in args.iter().enumerate().take(7) {
                    self.load_operand_aarch64(a, regs[i]);
                }
                self.out.push_str("    bl vredrs_call_dynamic\n");
                if let Some(d) = dst {
                    self.store_operand_aarch64(d, "x0");
                }
            }
            RuntimeCall { dst, func, args } => self.emit_call_aarch64(dst, func, args),
            Ret { value } => {
                if let Some(v) = value {
                    self.load_operand_aarch64(v, "x0");
                } else {
                    self.out.push_str("    mov x0, #0\n");
                }
                self.emit_epilogue_aarch64();
            }
            Push { src } => {
                self.load_operand_aarch64(src, "x9");
                self.out.push_str("    str x9, [sp, #-16]!\n");
            }
            Pop { dst } => {
                self.out.push_str("    ldr x9, [sp], #16\n");
                self.store_operand_aarch64(dst, "x9");
            }
            Asm { template, .. } => {
                self.out.push_str(&format!("    // asm: {:?}\n", template));
                for line in template.lines() {
                    self.out.push_str(&format!("    {}\n", line));
                }
            }
            Prefetch { addr, hint } => {
                self.load_operand_aarch64(addr, "x9");
                self.out.push_str(&format!("    prfm {}, [x9]\n", hint));
            }
            PipelineMarker { name } => self.out.push_str(&format!("    // @pipeline {}\n", name)),
            IsrEntry { group, priority } => self.out.push_str(&format!("    // @isr_group {} prio={}\n", group, priority)),
            WcetNote { cycles, budget } => self.out.push_str(&format!("    // wcet cycles={} budget={}\n", cycles, budget)),
            DiffMeta { symbol, offset } => self.out.push_str(&format!("    // diffmeta {} +{:#x}\n", symbol, offset)),
        }
    }

    fn emit_bin_aarch64(&mut self, op: BinOp, dst: &Operand, lhs: &Operand, rhs: &Operand, ty: TypeHint) {
        if op.is_dyn() {
            let func = dyn_bin_func(op);
            self.load_operand_aarch64(lhs, "x0");
            self.load_operand_aarch64(rhs, "x1");
            self.out.push_str(&format!("    bl {}\n", func));
            self.store_operand_aarch64(dst, "x0");
            return;
        }
        if ty == TypeHint::F64 {
            let func = match op {
                BinOp::Add => "vredrs_f64_add",
                BinOp::Sub => "vredrs_f64_sub",
                BinOp::Mul => "vredrs_f64_mul",
                BinOp::Div => "vredrs_f64_div",
                _ => "vredrs_f64_add",
            };
            self.load_operand_aarch64(lhs, "x0");
            self.load_operand_aarch64(rhs, "x1");
            self.out.push_str(&format!("    bl {}\n", func));
            self.store_operand_aarch64(dst, "x0");
            return;
        }
        // 原地优化：若 dst==lhs 且有物理寄存器，直接在物理寄存器上运算。
        if let (Operand::Reg(dst_name, _), Operand::Reg(lhs_name, _)) = (dst, lhs) {
            if dst_name == lhs_name {
                if let Some(preg) = self.reg_alloc.get(dst_name).cloned() {
                    if !matches!(op, BinOp::Div | BinOp::Mod) {
                        self.load_operand_aarch64(lhs, &preg);
                        self.load_operand_aarch64(rhs, "x10");
                        let p = preg.as_str();
                        match op {
                            BinOp::Add => self.out.push_str(&format!("    add {}, {}, x10\n", p, p)),
                            BinOp::Sub => self.out.push_str(&format!("    sub {}, {}, x10\n", p, p)),
                            BinOp::And => self.out.push_str(&format!("    and {}, {}, x10\n", p, p)),
                            BinOp::Or => self.out.push_str(&format!("    orr {}, {}, x10\n", p, p)),
                            BinOp::Xor => self.out.push_str(&format!("    eor {}, {}, x10\n", p, p)),
                            BinOp::Shl => self.out.push_str(&format!("    lsl {}, {}, x10\n", p, p)),
                            BinOp::Shr => self.out.push_str(&format!("    asr {}, {}, x10\n", p, p)),
                            BinOp::Mul => self.out.push_str(&format!("    mul {}, {}, x10\n", p, p)),
                            _ => {}
                        }
                        return;
                    }
                }
            }
        }
        self.load_operand_aarch64(lhs, "x9");
        self.load_operand_aarch64(rhs, "x10");
        match op {
            BinOp::Add => self.out.push_str("    add x9, x9, x10\n"),
            BinOp::Sub => self.out.push_str("    sub x9, x9, x10\n"),
            BinOp::And => self.out.push_str("    and x9, x9, x10\n"),
            BinOp::Or => self.out.push_str("    orr x9, x9, x10\n"),
            BinOp::Xor => self.out.push_str("    eor x9, x9, x10\n"),
            BinOp::Shl => self.out.push_str("    lsl x9, x9, x10\n"),
            BinOp::Shr => self.out.push_str("    asr x9, x9, x10\n"),
            BinOp::Mul => self.out.push_str("    mul x9, x9, x10\n"),
            BinOp::Div => self.out.push_str("    sdiv x9, x9, x10\n"),
            BinOp::Mod => {
                self.out.push_str("    sdiv x11, x9, x10\n");
                self.out.push_str("    msub x9, x11, x10, x9\n");
            }
            BinOp::Shl => self.out.push_str("    lsl x9, x9, x10\n"),
            BinOp::Shr => self.out.push_str("    asr x9, x9, x10\n"),
            BinOp::And => self.out.push_str("    and x9, x9, x10\n"),
            BinOp::Or => self.out.push_str("    orr x9, x9, x10\n"),
            BinOp::Xor => self.out.push_str("    eor x9, x9, x10\n"),
            _ => {}
        }
        self.store_operand_aarch64(dst, "x9");
    }

    fn emit_call_aarch64(&mut self, dst: &Option<Operand>, func: &str, args: &[Operand]) {
        let arg_regs = ["x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7"];
        for (i, a) in args.iter().enumerate().take(8) {
            self.load_operand_aarch64(a, arg_regs[i]);
        }
        // 栈参数（>8 个）简化忽略。
        self.out.push_str(&format!("    bl {}\n", func));
        if let Some(d) = dst {
            self.store_operand_aarch64(d, "x0");
        }
    }

    fn load_operand_aarch64(&mut self, op: &Operand, reg: &str) {
        match op {
            Operand::Reg(name, _) => {
                // 优先用分配的物理寄存器。
                if let Some(preg) = self.reg_alloc.get(name).cloned() {
                    if preg != reg {
                        self.out.push_str(&format!("    mov {}, {}\n", reg, preg));
                    }
                    return;
                }
                if let Some(&off) = self.stack_slots.get(name) {
                    self.out.push_str(&format!("    ldr {}, [x29, #-{}]\n", reg, off));
                } else {
                    self.out.push_str(&format!("    mov {}, #0\n", reg));
                }
            }
            Operand::ImmI64(v) => {
                // AArch64 立即数有限制，用 mov + movk。
                emit_imm64_aarch64(&mut self.out, reg, *v);
            }
            Operand::ImmF64(v) => {
                let bits = f64::to_bits(*v) as i64;
                emit_imm64_aarch64(&mut self.out, reg, bits);
            }
            Operand::ImmStr(idx) => {
                if *idx < self.str_labels.len() {
                    self.out.push_str(&format!("    adr {}, {}\n", reg, self.str_labels[*idx]));
                } else {
                    self.out.push_str(&format!("    mov {}, #0\n", reg));
                }
            }
            Operand::ImmBool(b) => {
                self.out.push_str(&format!("    mov {}, #{}\n", reg, if *b { 1 } else { 0 }));
            }
            Operand::Null | Operand::Label(_) | Operand::Sym(_) => {
                self.out.push_str(&format!("    mov {}, #0\n", reg));
            }
        }
    }

    fn store_operand_aarch64(&mut self, op: &Operand, reg: &str) {
        if let Operand::Reg(name, _) = op {
            // 优先用分配的物理寄存器。
            if let Some(preg) = self.reg_alloc.get(name).cloned() {
                if preg != reg {
                    self.out.push_str(&format!("    mov {}, {}\n", preg, reg));
                }
                return;
            }
            if let Some(&off) = self.stack_slots.get(name) {
                self.out.push_str(&format!("    str {}, [x29, #-{}]\n", reg, off));
            }
        }
    }

    fn store_reg_to_slot(&mut self, reg_name: &str, phys_reg: &str) {
        if let Some(&off) = self.stack_slots.get(reg_name) {
            self.out.push_str(&format!("    str {}, [x29, #-{}]\n", phys_reg, off));
        }
    }

    // ── ARM32 / Thumb-2（固件）────────────────────────────
    fn emit_module_arm32(&mut self, m: &IrModule) {
        self.out.push_str("    .syntax unified\n");
        self.out.push_str("    .thumb\n");
        self.out.push_str("    .section .rodata\n");
        for (i, s) in m.string_pool.iter().enumerate() {
            let label = format!(".str{}", i);
            self.str_labels.push(label.clone());
            self.out.push_str(&format!("{}:\n", label));
            self.out.push_str(&format!("    .asciz \"{}\"\n", escape_str(s)));
        }
        self.out.push_str("    .section .text\n");
        self.out.push_str("    .globl main\n");
        for f in &m.functions {
            self.emit_function_arm32(f);
        }
        // reset stub。
        self.out.push_str("\n    .globl _start\n_start:\n");
        self.out.push_str("    bl main\n");
        self.out.push_str("    b .\n"); // 死循环（固件）
    }

    fn emit_function_arm32(&mut self, f: &IrFunction) {
        self.stack_slots.clear();
        self.out.push_str(&format!("\n{}:\n", f.name));
        self.out.push_str("    push {r4, r5, r6, r7, lr}\n");
        self.out.push_str("    mov r7, sp\n");
        let mut max_slot = 0i64;
        for insn in &f.body {
            collect_regs(insn, &mut |r| {
                if r.starts_with('v') {
                    let idx: i64 = r[1..].parse().unwrap_or(0);
                    if idx > max_slot {
                        max_slot = idx;
                    }
                }
            });
        }
        let total_slots = max_slot.max(f.params.len() as i64) + 1;
        self.frame_size = ((total_slots * 4 + 7) / 8) * 8;
        if self.frame_size > 0 {
            self.out.push_str(&format!("    sub sp, #{}\n", self.frame_size));
        }
        for i in 0..=max_slot {
            self.stack_slots.insert(format!("v{}", i), 4 * (i + 1));
        }
        let arg_regs = ["r0", "r1", "r2", "r3"];
        for (i, reg_name) in f.param_regs.iter().enumerate().take(4) {
            if let Some(&off) = self.stack_slots.get(reg_name) {
                self.out.push_str(&format!("    str {}, [r7, #-{}]\n", arg_regs[i], off));
            }
        }
        // ARM32 指令发射（简化版，覆盖核心运算），应用 fallthrough 优化。
        let body = &f.body;
        for (idx, insn) in body.iter().enumerate() {
            if let StaticInsn::Jump { target } = insn {
                if idx + 1 < body.len() {
                    if let StaticInsn::Label(next) = &body[idx + 1] {
                        if next == target {
                            continue;
                        }
                    }
                }
            }
            self.emit_insn_arm32(insn);
        }
        if !matches!(f.body.last(), Some(StaticInsn::Ret { .. })) {
            self.emit_epilogue_arm32();
        }
    }

    fn emit_epilogue_arm32(&mut self) {
        if self.frame_size > 0 {
            self.out.push_str(&format!("    add sp, #{}\n", self.frame_size));
        }
        self.out.push_str("    pop {r4, r5, r6, r7, pc}\n");
    }

    fn emit_insn_arm32(&mut self, insn: &StaticInsn) {
        use StaticInsn::*;
        match insn {
            Label(l) => self.out.push_str(&format!("{}:\n", sanitize_label(l))),
            Comment(c) => self.out.push_str(&format!("    @ {}\n", c)),
            Mov { dst, src, .. } => {
                self.load_operand_arm32(src, "r4");
                self.store_operand_arm32(dst, "r4");
            }
            Bin { op, dst, lhs, rhs, ty } => {
                if op.is_dyn() {
                    let func = dyn_bin_func(*op);
                    self.load_operand_arm32(lhs, "r0");
                    self.load_operand_arm32(rhs, "r1");
                    self.out.push_str(&format!("    bl {}\n", func));
                    self.store_operand_arm32(dst, "r0");
                    return;
                }
                if *ty == TypeHint::F64 {
                    let func = match op {
                        BinOp::Add => "vredrs_f64_add",
                        BinOp::Sub => "vredrs_f64_sub",
                        BinOp::Mul => "vredrs_f64_mul",
                        BinOp::Div => "vredrs_f64_div",
                        _ => "vredrs_f64_add",
                    };
                    self.load_operand_arm32(lhs, "r0");
                    self.load_operand_arm32(rhs, "r1");
                    self.out.push_str(&format!("    bl {}\n", func));
                    self.store_operand_arm32(dst, "r0");
                    return;
                }
                self.load_operand_arm32(lhs, "r4");
                self.load_operand_arm32(rhs, "r5");
                match op {
                    BinOp::Add => self.out.push_str("    add r4, r4, r5\n"),
                    BinOp::Sub => self.out.push_str("    sub r4, r4, r5\n"),
                    BinOp::And => self.out.push_str("    and r4, r4, r5\n"),
                    BinOp::Or => self.out.push_str("    orr r4, r4, r5\n"),
                    BinOp::Xor => self.out.push_str("    eor r4, r4, r5\n"),
                    BinOp::Mul => self.out.push_str("    mul r4, r4, r5\n"),
                    BinOp::Div => self.out.push_str("    sdiv r4, r4, r5\n"),
                    BinOp::Mod => {
                        self.out.push_str("    sdiv r6, r4, r5\n");
                        self.out.push_str("    mls r4, r6, r5, r4\n");
                    }
                    BinOp::Shl => self.out.push_str("    lsl r4, r4, r5\n"),
                    BinOp::Shr => self.out.push_str("    asr r4, r4, r5\n"),
                    // 动态 op → 调用 runtime 函数（r0=lhs, r1=rhs, bl func）。
                    BinOp::AddDyn => { self.out.push_str("    mov r0, r4\n    mov r1, r5\n    bl vredrs_value_add\n    mov r4, r0\n"); }
                    BinOp::SubDyn => { self.out.push_str("    mov r0, r4\n    mov r1, r5\n    bl vredrs_value_sub\n    mov r4, r0\n"); }
                    BinOp::MulDyn => { self.out.push_str("    mov r0, r4\n    mov r1, r5\n    bl vredrs_value_mul\n    mov r4, r0\n"); }
                    BinOp::DivDyn => { self.out.push_str("    mov r0, r4\n    mov r1, r5\n    bl vredrs_value_div\n    mov r4, r0\n"); }
                    BinOp::ModDyn => { self.out.push_str("    mov r0, r4\n    mov r1, r5\n    bl vredrs_value_mod\n    mov r4, r0\n"); }
                    BinOp::EqDyn => { self.out.push_str("    mov r0, r4\n    mov r1, r5\n    bl vredrs_value_eq\n    mov r4, r0\n"); }
                    BinOp::NeDyn => { self.out.push_str("    mov r0, r4\n    mov r1, r5\n    bl vredrs_value_ne\n    mov r4, r0\n"); }
                    BinOp::LtDyn => { self.out.push_str("    mov r0, r4\n    mov r1, r5\n    bl vredrs_value_lt\n    mov r4, r0\n"); }
                    BinOp::LeDyn => { self.out.push_str("    mov r0, r4\n    mov r1, r5\n    bl vredrs_value_le\n    mov r4, r0\n"); }
                    BinOp::GtDyn => { self.out.push_str("    mov r0, r4\n    mov r1, r5\n    bl vredrs_value_gt\n    mov r4, r0\n"); }
                    BinOp::GeDyn => { self.out.push_str("    mov r0, r4\n    mov r1, r5\n    bl vredrs_value_ge\n    mov r4, r0\n"); }
                    _ => self.out.push_str("    @ fallback: add r4, r4, r5\n"),
                }
                self.store_operand_arm32(dst, "r4");
            }
            Branch { cond, lhs, rhs, target } => {
                self.load_operand_arm32(lhs, "r4");
                self.load_operand_arm32(rhs, "r5");
                self.out.push_str("    cmp r4, r5\n");
                let b = cond_to_branch_arm32(*cond);
                self.out.push_str(&format!("    b{} {}\n", b, sanitize_label(target)));
            }
            BranchTrue { cond, target } => {
                self.load_operand_arm32(cond, "r4");
                self.out.push_str("    cmp r4, #0\n");
                self.out.push_str(&format!("    bne {}\n", sanitize_label(target)));
            }
            Jump { target } => {
                self.out.push_str(&format!("    b {}\n", sanitize_label(target)));
            }
            Call { dst, func, args, .. } | RuntimeCall { dst, func, args } => {
                let arg_regs = ["r0", "r1", "r2", "r3"];
                for (i, a) in args.iter().enumerate().take(4) {
                    self.load_operand_arm32(a, arg_regs[i]);
                }
                self.out.push_str(&format!("    bl {}\n", func));
                if let Some(d) = dst {
                    self.store_operand_arm32(d, "r0");
                }
            }
            CallDyn { dst, callee, args, .. } => {
                self.load_operand_arm32(callee, "r0");
                let regs = ["r1", "r2", "r3"];
                for (i, a) in args.iter().enumerate().take(3) {
                    self.load_operand_arm32(a, regs[i]);
                }
                self.out.push_str("    bl vredrs_call_dynamic\n");
                if let Some(d) = dst {
                    self.store_operand_arm32(d, "r0");
                }
            }
            Ret { value } => {
                if let Some(v) = value {
                    self.load_operand_arm32(v, "r0");
                } else {
                    self.out.push_str("    mov r0, #0\n");
                }
                self.emit_epilogue_arm32();
            }
            Box { dst, src, from } => {
                let func = match from {
                    TypeHint::I64 => "vredrs_value_make_i64",
                    TypeHint::Bool => "vredrs_value_make_bool",
                    _ => "",
                };
                if !func.is_empty() {
                    self.load_operand_arm32(src, "r0");
                    self.out.push_str(&format!("    bl {}\n", func));
                    self.store_operand_arm32(dst, "r0");
                } else {
                    self.load_operand_arm32(src, "r4");
                    self.store_operand_arm32(dst, "r4");
                }
            }
            Unbox { dst, src, to } => {
                let func = match to {
                    TypeHint::I64 => "vredrs_value_get_i64",
                    TypeHint::Bool => "vredrs_value_get_bool",
                    _ => "",
                };
                if !func.is_empty() {
                    self.load_operand_arm32(src, "r0");
                    self.out.push_str(&format!("    bl {}\n", func));
                    self.store_operand_arm32(dst, "r0");
                } else {
                    self.load_operand_arm32(src, "r4");
                    self.store_operand_arm32(dst, "r4");
                }
            }
            _ => {
                // 完整覆盖剩余指令（Label/Cmp/Neg/Not/Load/Store/Alloca/
                // StackFree/Asm/Prefetch/PipelineMarker/IsrEntry/WcetNote/
                // DiffMeta 等）。
                use StaticInsn::*;
                match insn {
                    Label(l) => self.out.push_str(&format!("{}:\n", sanitize_label(l))),
                    SrcLoc { line, col } => self.out.push_str(&format!("    @ srcloc {}:{}\n", line, col)),
                    Cmp { dst, cond, lhs, rhs, .. } => {
                        self.load_operand_arm32(lhs, "r4");
                        self.load_operand_arm32(rhs, "r5");
                        self.out.push_str("    cmp r4, r5\n");
                        let c = cond_to_branch_arm32(*cond);
                        self.out.push_str(&format!("    mov r4, #0\n    b{} 1f\n    mov r4, #1\n1:\n", c));
                        self.store_operand_arm32(dst, "r4");
                    }
                    Neg { dst, src, .. } => {
                        self.load_operand_arm32(src, "r4");
                        self.out.push_str("    rsb r4, r4, #0\n");
                        self.store_operand_arm32(dst, "r4");
                    }
                    Not { dst, src, .. } => {
                        self.load_operand_arm32(src, "r4");
                        self.out.push_str("    eor r4, r4, #1\n");
                        self.store_operand_arm32(dst, "r4");
                    }
                    Load { dst, addr, .. } => {
                        self.load_operand_arm32(addr, "r4");
                        self.out.push_str("    ldr r4, [r4]\n");
                        self.store_operand_arm32(dst, "r4");
                    }
                    Store { addr, value, .. } => {
                        self.load_operand_arm32(addr, "r5");
                        self.load_operand_arm32(value, "r4");
                        self.out.push_str("    str r4, [r5]\n");
                    }
                    Alloca { dst, size, .. } => {
                        self.load_operand_arm32(size, "r4");
                        self.out.push_str("    sub sp, sp, r4\n    mov r4, sp\n");
                        self.store_operand_arm32(dst, "r4");
                    }
                    StackFree { .. } => {}
                    Push { src } => {
                        self.load_operand_arm32(src, "r4");
                        self.out.push_str("    push {r4}\n");
                    }
                    Pop { dst } => {
                        self.out.push_str("    pop {r4}\n");
                        self.store_operand_arm32(dst, "r4");
                    }
                    Asm { template, .. } => {
                        for line in template.lines() {
                            self.out.push_str(&format!("    {}\n", line));
                        }
                    }
                    Prefetch { addr, .. } => {
                        self.load_operand_arm32(addr, "r4");
                        self.out.push_str("    pld [r4]\n");
                    }
                    PipelineMarker { name } => self.out.push_str(&format!("    @ @pipeline {}\n", name)),
                    IsrEntry { group, priority } => self.out.push_str(&format!("    @ @isr_group {} prio={}\n", group, priority)),
                    WcetNote { cycles, budget } => self.out.push_str(&format!("    @ wcet cycles={} budget={}\n", cycles, budget)),
                    DiffMeta { symbol, offset } => self.out.push_str(&format!("    @ diffmeta {} +{:#x}\n", symbol, offset)),
                    _ => {
                        // 所有 StaticInsn 变体已在上面处理。这分支不应触发。
                        self.out.push_str("    @ unreachable insn\n");
                    }
                }
            }
        }
    }

    fn load_operand_arm32(&mut self, op: &Operand, reg: &str) {
        match op {
            Operand::Reg(name, _) => {
                if let Some(&off) = self.stack_slots.get(name) {
                    self.out.push_str(&format!("    ldr {}, [r7, #-{}]\n", reg, off));
                } else {
                    self.out.push_str(&format!("    mov {}, #0\n", reg));
                }
            }
            Operand::ImmI64(v) => {
                self.out.push_str(&format!("    mov {}, #{}\n", reg, *v as i32));
            }
            Operand::ImmBool(b) => {
                self.out.push_str(&format!("    mov {}, #{}\n", reg, if *b { 1 } else { 0 }));
            }
            Operand::ImmStr(idx) => {
                if *idx < self.str_labels.len() {
                    self.out.push_str(&format!("    ldr {}, ={}\n", reg, self.str_labels[*idx]));
                } else {
                    self.out.push_str(&format!("    mov {}, #0\n", reg));
                }
            }
            _ => {
                self.out.push_str(&format!("    mov {}, #0\n", reg));
            }
        }
    }

    fn store_operand_arm32(&mut self, op: &Operand, reg: &str) {
        if let Operand::Reg(name, _) = op {
            if let Some(&off) = self.stack_slots.get(name) {
                self.out.push_str(&format!("    str {}, [r7, #-{}]\n", reg, off));
            }
        }
    }
}

// ── 共享辅助（与 emit_raw_x86 一致）──────────────────────

fn collect_regs(insn: &StaticInsn, f: &mut impl FnMut(&str)) {
    let mut visit = |op: &Operand| {
        if let Operand::Reg(n, _) = op {
            f(n);
        }
    };
    use StaticInsn::*;
    match insn {
        Mov { dst, src, .. } => { visit(dst); visit(src); }
        Box { dst, src, .. } => { visit(dst); visit(src); }
        Unbox { dst, src, .. } => { visit(dst); visit(src); }
        Bin { dst, lhs, rhs, .. } => { visit(dst); visit(lhs); visit(rhs); }
        Neg { dst, src, .. } | Not { dst, src, .. } => { visit(dst); visit(src); }
        Cmp { dst, lhs, rhs, .. } => { visit(dst); visit(lhs); visit(rhs); }
        Branch { lhs, rhs, .. } => { visit(lhs); visit(rhs); }
        BranchTrue { cond, .. } => { visit(cond); }
        Load { dst, addr, .. } => { visit(dst); visit(addr); }
        Store { addr, value, .. } => { visit(addr); visit(value); }
        Alloca { dst, size, .. } => { visit(dst); visit(size); }
        StackFree { ptr, size } => { visit(ptr); visit(size); }
        Call { dst, args, .. } | RuntimeCall { dst, args, .. } => {
            if let Some(d) = dst { visit(d); }
            for a in args { visit(a); }
        }
        CallDyn { dst, callee, args, .. } => {
            if let Some(d) = dst { visit(d); }
            visit(callee);
            for a in args { visit(a); }
        }
        Ret { value } => { if let Some(v) = value { visit(v); } }
        Push { src } | Pop { dst: src } => { visit(src); }
        Asm { inputs, outputs, .. } => {
            for o in outputs { visit(&o.operand); }
            for i in inputs { visit(&i.operand); }
        }
        Prefetch { addr, .. } => { visit(addr); }
        _ => {}
    }
}

fn collect_externs(insn: &StaticInsn, out: &mut Vec<String>) {
    use StaticInsn::*;
    match insn {
        RuntimeCall { func, .. } => out.push(func.clone()),
        CallDyn { .. } => out.push("vredrs_call_dynamic".into()),
        Bin { op, .. } if op.is_dyn() => out.push(dyn_bin_func(*op).into()),
        Unbox { to, .. } => match to {
            TypeHint::I64 => out.push("vredrs_value_get_i64".into()),
            TypeHint::F64 => out.push("vredrs_value_get_f64".into()),
            TypeHint::Bool => out.push("vredrs_value_get_bool".into()),
            _ => {}
        },
        Box { from, .. } => match from {
            TypeHint::I64 => out.push("vredrs_value_make_i64".into()),
            TypeHint::F64 => out.push("vredrs_value_make_f64".into()),
            TypeHint::Bool => out.push("vredrs_value_make_bool".into()),
            _ => {}
        },
        Neg { ty, .. } if *ty == TypeHint::F64 => out.push("vredrs_f64_neg".into()),
        Bin { ty, op, .. } if *ty == TypeHint::F64 && !op.is_dyn() => {
            out.push(match op {
                BinOp::Add => "vredrs_f64_add".into(),
                BinOp::Sub => "vredrs_f64_sub".into(),
                BinOp::Mul => "vredrs_f64_mul".into(),
                BinOp::Div => "vredrs_f64_div".into(),
                _ => "vredrs_f64_add".into(),
            });
        }
        _ => {}
    }
}

fn dyn_bin_func(op: BinOp) -> &'static str {
    match op {
        BinOp::AddDyn => "vredrs_value_add",
        BinOp::SubDyn => "vredrs_value_sub",
        BinOp::MulDyn => "vredrs_value_mul",
        BinOp::DivDyn => "vredrs_value_div",
        BinOp::ModDyn => "vredrs_value_mod",
        BinOp::EqDyn => "vredrs_value_eq",
        BinOp::NeDyn => "vredrs_value_ne",
        BinOp::LtDyn => "vredrs_value_lt",
        BinOp::LeDyn => "vredrs_value_le",
        BinOp::GtDyn => "vredrs_value_gt",
        BinOp::GeDyn => "vredrs_value_ge",
        _ => "vredrs_value_add",
    }
}

fn cond_to_cset_aarch64(c: Cond) -> &'static str {
    match c {
        Cond::Eq => "eq", Cond::Ne => "ne", Cond::Lt => "lt",
        Cond::Le => "le", Cond::Gt => "gt", Cond::Ge => "ge",
    }
}

fn cond_to_branch_aarch64(c: Cond) -> &'static str {
    cond_to_cset_aarch64(c)
}

fn cond_to_branch_arm32(c: Cond) -> &'static str {
    match c {
        Cond::Eq => "eq", Cond::Ne => "ne", Cond::Lt => "lt",
        Cond::Le => "le", Cond::Gt => "gt", Cond::Ge => "ge",
    }
}

fn escape_str(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn sanitize_label(l: &str) -> String {
    l.replace('.', "_dot_").replace(' ', "_")
}

fn emit_imm64_aarch64(out: &mut String, reg: &str, v: i64) {
    let v = v as u64;
    let lo16 = v & 0xffff;
    let mid16 = (v >> 16) & 0xffff;
    let hi16 = (v >> 32) & 0xffff;
    let top16 = (v >> 48) & 0xffff;
    out.push_str(&format!("    mov {}, #{}\n", reg, lo16));
    if mid16 != 0 || hi16 != 0 || top16 != 0 {
        out.push_str(&format!("    movk {}, #{}, lsl #16\n", reg, mid16));
    }
    if hi16 != 0 || top16 != 0 {
        out.push_str(&format!("    movk {}, #{}, lsl #32\n", reg, hi16));
    }
    if top16 != 0 {
        out.push_str(&format!("    movk {}, #{}, lsl #48\n", reg, top16));
    }
}

/// 对外入口：通过新 IR 路径编译成 ARM 可执行文件/固件。
pub fn compile_via_ir(program: &Program, output_path: &Path) -> Result<(), String> {
    use crate::codegen::lower::lower_program;
    use crate::platform::PlatformInfo;

    let plat = PlatformInfo::detect();
    let host_arch = std::env::consts::ARCH;
    let is_aarch64 = host_arch == "aarch64";

    let module = lower_program(program);
    let mut emitter = ArmEmitter::new(is_aarch64);
    let asm = emitter.emit_module(&module);

    let asm_path = output_path.with_extension("s");
    std::fs::write(&asm_path, &asm).map_err(|e| format!("can't write ARM assembly: {}", e))?;
    crate::platform::info(&format!("IR-based {} assembly written to: {}", if is_aarch64 { "AArch64" } else { "ARM32" }, asm_path.display()));

    let ir_text = crate::codegen::ir::render_module(&module);
    let ir_path = output_path.with_extension("ir");
    let _ = std::fs::write(&ir_path, &ir_text);

    if host_arch != "aarch64" && host_arch != "arm" && host_arch != "armv7l" {
        // 非 ARM 主机：只写汇编，不组装。
        return Ok(());
    }

    // 组装链接（AArch64 主机）。
    if is_aarch64 {
        let obj_path = output_path.with_extension("o");
        let assemble = std::process::Command::new(plat.as_command())
            .arg("-o")
            .arg(&obj_path)
            .arg(&asm_path)
            .output();
        if let Ok(out) = assemble {
            if !out.status.success() {
                crate::platform::warning(&format!("'as' failed: {}", String::from_utf8_lossy(&out.stderr)));
                return Ok(());
            }
        }
        let exe_path = output_path.to_path_buf();
        let link = std::process::Command::new(plat.ld_command())
            .arg("-o")
            .arg(&exe_path)
            .arg(&obj_path)
            .output();
        if let Ok(out) = link {
            if out.status.success() {
                crate::platform::success(&format!("Native AArch64 executable (via IR): {}", exe_path.display()));
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&exe_path, std::fs::Permissions::from_mode(0o755));
                }
            } else {
                // Bug N3 fix: 汇编/链接失败时报告错误，不误导用户。
                crate::platform::warning(&format!("Linking failed: {}", String::from_utf8_lossy(&out.stderr)));
            }
        } else {
            crate::platform::warning("Linker not found; assembly written only");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::ir::{IrFunction, IrModule, StaticInsn, Operand, TypeHint, BinOp};

    #[test]
    fn emit_aarch64_simple() {
        let m = IrModule {
            functions: vec![IrFunction {
                name: "main".into(),
                params: vec![],
                return_ty: TypeHint::I64,
                body: vec![
                    StaticInsn::Mov { dst: Operand::Reg("v0".into(), TypeHint::I64), src: Operand::ImmI64(42), ty: TypeHint::I64 },
                    StaticInsn::Ret { value: Some(Operand::Reg("v0".into(), TypeHint::I64)) },
                ],
                is_main: true,
                annotations: vec![],
                param_regs: vec![],
                locals: vec![],
            }],
            string_pool: vec![],
            globals: vec![],
            entry: Some("main".into()),
        };
        let mut e = ArmEmitter::new(true);
        let asm = e.emit_module(&m);
        assert!(asm.contains("main:"));
        assert!(asm.contains("_start:"));
        assert!(asm.contains("#42"));
    }

    #[test]
    fn emit_arm32_add() {
        let m = IrModule {
            functions: vec![IrFunction {
                name: "add".into(),
                params: vec![],
                return_ty: TypeHint::I64,
                body: vec![
                    StaticInsn::Bin { op: BinOp::Add, dst: Operand::Reg("v2".into(), TypeHint::I64), lhs: Operand::Reg("v0".into(), TypeHint::I64), rhs: Operand::Reg("v1".into(), TypeHint::I64), ty: TypeHint::I64 },
                    StaticInsn::Ret { value: Some(Operand::Reg("v2".into(), TypeHint::I64)) },
                ],
                is_main: false,
                annotations: vec![],
                param_regs: vec![],
                locals: vec![],
            }],
            string_pool: vec![],
            globals: vec![],
            entry: Some("add".into()),
        };
        let mut e = ArmEmitter::new(false);
        let asm = e.emit_module(&m);
        assert!(asm.contains("add:"));
        assert!(asm.contains("add r4, r4, r5"));
    }
}
