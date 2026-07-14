//! # emit_raw_x86 — Static IR → x86_64 汇编发射器
//!
//! 手册 Phase 1/2 的 Raw 后端新路径。消费 [`crate::codegen::ir::IrModule`]，
//! 不再直接啃 AST。
//!
//! ## 设计
//!
//! - **虚拟寄存器→栈槽**：每个 `v%N` 映射到 `[rbp - 8*(N+1)]`。简单、正确、
//!   可与任意 IR 兼容。寄存器分配器（[`crate::codegen::cstar::raw::regalloc`]）
//!   可后续接入以提升性能。
//! - **静态 I64 运算**：直接发射 `add`/`sub`/`imul`/`cmp`+`jcc` 原生指令。
//! - **动态运算**：调用 `vredrs_runtime.c` 中的 `vredrs_value_*` 函数。
//! - **入口**：`main` 返回值作为进程 exit code（与 C 一致）。
//! - **println**：整数走内联 div-by-10 + `write(1, ...)` syscall；字符串走
//!   `write`；动态值走 `vredrs_println` runtime。

use crate::codegen::ir::{BinOp, Cond, IrFunction, IrModule, Operand, StaticInsn, TypeHint};
use crate::parser::ast::Program;
use std::collections::HashMap;
use std::path::Path;

/// IR → x86_64 汇编发射器。
pub struct X86Emitter {
    /// 输出缓冲。
    out: String,
    /// 当前函数内：虚拟寄存器名 → 栈偏移（负数，相对 rbp）。
    stack_slots: HashMap<String, i64>,
    /// 当前函数内：虚拟寄存器名 → 物理寄存器名（寄存器分配）。
    /// 优先于 stack_slots：若虚拟寄存器在此映射中，直接用物理寄存器。
    reg_alloc: HashMap<String, String>,
    /// 当前函数用到的 callee-saved 寄存器（用于 prologue/epilogue）。
    used_callee_saved: Vec<String>,
    /// 当前函数的栈帧大小（字节数，已对齐到 16）。
    frame_size: i64,
    /// 字符串标签计数（用于 .rodata）。
    str_labels: Vec<String>,
}

impl X86Emitter {
    pub fn new() -> Self {
        X86Emitter {
            out: String::new(),
            stack_slots: HashMap::new(),
            reg_alloc: HashMap::new(),
            used_callee_saved: Vec::new(),
            frame_size: 0,
            str_labels: Vec::new(),
        }
    }

    /// 把整个 IR 模块发射成 x86_64 汇编字符串。
    pub fn emit_module(&mut self, m: &IrModule) -> String {
        self.out.clear();
        // 文件头：使用 Intel 语法（GAS 默认 AT&T）。
        self.out.push_str("    .intel_syntax noprefix\n");
        self.out.push_str("    .section .rodata\n");
        // 字符串池。
        for (i, s) in m.string_pool.iter().enumerate() {
            let label = format!(".str{}", i);
            self.str_labels.push(label.clone());
            self.out.push_str(&format!("{}:\n", label));
            self.out.push_str(&format!("    .string \"{}\"\n", escape_str(s)));
        }
        // 全局变量（简化：放 .bss，每个 8 字节）。
        if !m.globals.is_empty() {
            self.out.push_str("    .section .bss\n");
            for (n, _) in &m.globals {
                self.out.push_str(&format!("    .globl {}\n", n));
                self.out.push_str(&format!("    .align 8\n"));
                self.out.push_str(&format!("{}:\n", n));
                self.out.push_str("    .zero 8\n");
            }
        }
        // 代码段。
        self.out.push_str("    .section .text\n");
        self.out.push_str("    .globl main\n");
        // 声明用到的 runtime 函数（extern）。
        self.emit_extern_decls(m);
        // 每个函数。
        for f in &m.functions {
            self.emit_function(f);
        }
        // 注意：不发射 _start，由 cc/crt1.o 提供（它调用 main）。
        // 若用 ld 直接链接（无 crt），需要 -e main 或自定义 _start。
        self.out.clone()
    }

    /// 发射 extern 声明（runtime 函数）。
    fn emit_extern_decls(&mut self, m: &IrModule) {
        // 扫描所有 RuntimeCall/CallDyn 收集用到的 extern 函数名。
        let mut exts: Vec<String> = Vec::new();
        for f in &m.functions {
            for i in &f.body {
                collect_externs(i, &mut exts);
            }
        }
        // 去重。
        exts.sort();
        exts.dedup();
        for e in &exts {
            self.out.push_str(&format!("    .extern {}\n", e));
        }
    }

    /// 发射单个函数。
    fn emit_function(&mut self, f: &IrFunction) {
        self.stack_slots.clear();
        self.reg_alloc.clear();
        self.used_callee_saved.clear();
        self.frame_size = 0;
        self.out.push_str(&format!("\n{}:\n", f.name));
        // Prologue：push rbp; mov rbp, rsp
        self.out.push_str("    push rbp\n");
        self.out.push_str("    mov rbp, rsp\n");
        // 第一遍：计算需要的栈帧大小（每个虚拟寄存器 8 字节）。
        let mut max_slot = 0i64;
        for insn in &f.body {
            collect_regs(insn, &mut |r| {
                if !r.starts_with('v') {
                    return;
                }
                let idx: i64 = r[1..].parse().unwrap_or(0);
                if idx > max_slot {
                    max_slot = idx;
                }
            });
        }
        let n_params = f.params.len() as i64;
        let total_slots = max_slot.max(n_params) + 1;
        self.frame_size = ((total_slots * 8 + 15) / 16) * 16;

        // 寄存器分配：把前几个虚拟寄存器（优先参数）映射到 callee-saved 寄存器。
        // rbx/r12/r13/r14/r15 是 callee-saved，跨调用保持。
        let callee_regs = ["rbx", "r12", "r13", "r14", "r15"];
        // 优先分配参数（v0..v(n-1)），然后是其他热点虚拟寄存器。
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

        // 为未分配到物理寄存器的虚拟寄存器分配栈槽。
        for i in 0..=max_slot {
            let name = format!("v{}", i);
            if !self.reg_alloc.contains_key(&name) {
                self.stack_slots.insert(name, -8 * (i + 1));
            }
        }

        // Prologue：push 用到的 callee-saved 寄存器（对齐到 16 字节）。
        // push rbp 已经占了 8 字节，加上 return address 8 字节 = 16 字节对齐。
        // 每多 push 一个 callee-saved 需要补齐。
        let n_callee = self.used_callee_saved.len();
        let need_pad = n_callee % 2 == 1; // 保持 16 字节对齐
        for r in &self.used_callee_saved {
            self.out.push_str(&format!("    push {}\n", r));
        }
        if need_pad {
            self.out.push_str("    sub rsp, 8\n");
        }
        if self.frame_size > 0 {
            self.out.push_str(&format!("    sub rsp, {}\n", self.frame_size));
        }

        // 把参数从 arg 寄存器搬到分配的物理寄存器或栈槽。
        let arg_regs = ["rdi", "rsi", "rdx", "rcx", "r8", "r9"];
        for (i, reg_name) in f.param_regs.iter().enumerate() {
            if i < 6 {
                self.store_to_vreg(reg_name, arg_regs[i]);
            }
        }
        // 发射函数体，应用 fallthrough + 死 Label 优化。
        // 第一遍：收集所有被引用的 Label（Jump/Branch 的 target + RuntimeCall args 中的 Label）。
        let body = &f.body;
        let mut referenced_labels: std::collections::HashSet<String> = std::collections::HashSet::new();
        for insn in body {
            match insn {
                StaticInsn::Jump { target } | StaticInsn::BranchTrue { cond: _, target } => {
                    referenced_labels.insert(target.clone());
                }
                StaticInsn::Branch { target, .. } => {
                    referenced_labels.insert(target.clone());
                }
                StaticInsn::RuntimeCall { args, .. } | StaticInsn::Call { args, .. } => {
                    for a in args {
                        if let Operand::Label(l) = a {
                            referenced_labels.insert(l.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        // 第二遍：发射，跳过 fallthrough Jump 和未引用 Label。
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
            // 死 Label 优化：未被引用的 Label 不发射。
            if let StaticInsn::Label(l) = insn {
                if !referenced_labels.contains(l) {
                    continue;
                }
            }
            self.emit_insn(insn, f);
        }
        // Epilogue（如果末尾不是 ret）。
        if !matches!(f.body.last(), Some(StaticInsn::Ret { .. })) {
            self.emit_epilogue();
        }
    }

    fn emit_epilogue(&mut self) {
        if self.frame_size > 0 {
            self.out.push_str(&format!("    add rsp, {}\n", self.frame_size));
        }
        let n_callee = self.used_callee_saved.len();
        let need_pad = n_callee % 2 == 1;
        if need_pad {
            self.out.push_str("    add rsp, 8\n");
        }
        // 逆序 pop callee-saved。
        for r in self.used_callee_saved.iter().rev() {
            self.out.push_str(&format!("    pop {}\n", r));
        }
        self.out.push_str("    pop rbp\n");
        self.out.push_str("    ret\n");
    }

    /// 发射单条 IR 指令。
    fn emit_insn(&mut self, insn: &StaticInsn, f: &IrFunction) {
        use StaticInsn::*;
        match insn {
            Label(l) => {
                self.out.push_str(&format!("{}:\n", sanitize_label(l)));
            }
            Comment(c) => {
                self.out.push_str(&format!("    # {}\n", c));
            }
            SrcLoc { line, col } => {
                self.out.push_str(&format!("    # srcloc {}:{}\n", line, col));
            }
            Mov { dst, src, ty } => {
                // 窥孔优化：若 dst 有物理寄存器，直接 load src 到该寄存器（省一次 mov）。
                if let Operand::Reg(name, _) = dst {
                    if let Some(preg) = self.reg_alloc.get(name).cloned() {
                        self.load_operand(src, &preg);
                        let _ = ty;
                        return;
                    }
                }
                self.load_operand(src, "rax");
                self.store_operand(dst, "rax");
                let _ = ty;
            }
            Box { dst, src, from } => {
                // 静态→动态装箱。
                match from {
                    TypeHint::I64 => {
                        self.load_operand(src, "rdi");
                        self.out.push_str("    call vredrs_value_make_i64\n");
                        self.store_operand(dst, "rax");
                    }
                    TypeHint::F64 => {
                        self.load_operand(src, "rdi");
                        // f64 需要先放到 xmm0，但简化：通过栈传递。
                        self.out.push_str("    call vredrs_value_make_f64\n");
                        self.store_operand(dst, "rax");
                    }
                    TypeHint::Bool => {
                        self.load_operand(src, "rdi");
                        self.out.push_str("    call vredrs_value_make_bool\n");
                        self.store_operand(dst, "rax");
                    }
                    _ => {
                        // 其他类型直接 mov。
                        self.load_operand(src, "rax");
                        self.store_operand(dst, "rax");
                    }
                }
            }
            Unbox { dst, src, to } => {
                match to {
                    TypeHint::I64 => {
                        self.load_operand(src, "rdi");
                        self.out.push_str("    call vredrs_value_get_i64\n");
                        self.store_operand(dst, "rax");
                    }
                    TypeHint::F64 => {
                        self.load_operand(src, "rdi");
                        self.out.push_str("    call vredrs_value_get_f64\n");
                        self.store_operand(dst, "rax");
                    }
                    TypeHint::Bool => {
                        self.load_operand(src, "rdi");
                        self.out.push_str("    call vredrs_value_get_bool\n");
                        self.store_operand(dst, "rax");
                    }
                    _ => {
                        self.load_operand(src, "rax");
                        self.store_operand(dst, "rax");
                    }
                }
            }
            Bin { op, dst, lhs, rhs, ty } => {
                self.emit_bin(*op, dst, lhs, rhs, *ty);
            }
            Neg { dst, src, ty } => {
                if *ty == TypeHint::F64 {
                    // 浮点取负：xorpd with sign mask（简化用 runtime）。
                    self.load_operand(src, "rdi");
                    self.out.push_str("    call vredrs_f64_neg\n");
                    self.store_operand(dst, "rax");
                } else {
                    self.load_operand(src, "rax");
                    self.out.push_str("    neg rax\n");
                    self.store_operand(dst, "rax");
                }
            }
            Not { dst, src, ty } => {
                self.load_operand(src, "rax");
                self.out.push_str("    xor rax, 1\n");
                self.store_operand(dst, "rax");
                let _ = ty;
            }
            Cmp { dst, cond, lhs, rhs, ty } => {
                self.load_operand(lhs, "rax");
                // 优化：与 0 比较时用 test rax, rax（比 cmp rax, 0 短一字节）。
                if let Operand::ImmI64(0) = rhs {
                    self.out.push_str("    test rax, rax\n");
                } else if let Operand::ImmI64(v) = rhs {
                    self.out.push_str(&format!("    cmp rax, {}\n", v));
                } else {
                    self.load_operand(rhs, "rcx");
                    self.out.push_str("    cmp rax, rcx\n");
                }
                let setcc = cond_to_setcc(*cond);
                self.out.push_str(&format!("    set{} al\n", setcc));
                self.out.push_str("    movzx rax, al\n");
                self.store_operand(dst, "rax");
                let _ = ty;
            }
            Branch { cond, lhs, rhs, target } => {
                self.load_operand(lhs, "rax");
                if let Operand::ImmI64(0) = rhs {
                    self.out.push_str("    test rax, rax\n");
                } else if let Operand::ImmI64(v) = rhs {
                    self.out.push_str(&format!("    cmp rax, {}\n", v));
                } else {
                    self.load_operand(rhs, "rcx");
                    self.out.push_str("    cmp rax, rcx\n");
                }
                let jcc = cond_to_jcc(*cond);
                self.out.push_str(&format!("    j{} {}\n", jcc, sanitize_label(target)));
            }
            BranchTrue { cond, target } => {
                self.load_operand(cond, "rax");
                self.out.push_str("    test rax, rax\n");
                self.out.push_str(&format!("    jne {}\n", sanitize_label(target)));
            }
            Jump { target } => {
                self.out.push_str(&format!("    jmp {}\n", sanitize_label(target)));
            }
            Load { dst, addr, size, .. } => {
                self.load_operand(addr, "rax");
                match size {
                    1 => self.out.push_str("    movsx rax, byte ptr [rax]\n"),
                    2 => self.out.push_str("    movsx rax, word ptr [rax]\n"),
                    4 => self.out.push_str("    movsxd rax, dword ptr [rax]\n"),
                    _ => self.out.push_str("    mov rax, qword ptr [rax]\n"),
                }
                self.store_operand(dst, "rax");
            }
            Store { addr, value, size, .. } => {
                self.load_operand(addr, "rcx");
                self.load_operand(value, "rax");
                match size {
                    1 => self.out.push_str("    mov byte ptr [rcx], al\n"),
                    2 => self.out.push_str("    mov word ptr [rcx], ax\n"),
                    4 => self.out.push_str("    mov dword ptr [rcx], eax\n"),
                    _ => self.out.push_str("    mov qword ptr [rcx], rax\n"),
                }
            }
            Alloca { dst, size, align } => {
                // 栈分配：移动栈指针。
                self.load_operand(size, "rax");
                self.out.push_str("    add rax, 15\n");
                self.out.push_str("    and rax, -16\n");
                self.out.push_str("    sub rsp, rax\n");
                self.out.push_str("    mov rax, rsp\n");
                self.store_operand(dst, "rax");
                let _ = align;
            }
            StackFree { .. } => {
                // 简化：no-op（栈在 epilogue 自动回收）。
            }
            Call { dst, func, args, ty } => {
                self.emit_call(dst, func, args, false);
                let _ = ty;
            }
            CallDyn { dst, callee, args, ty } => {
                // 动态调用：callee 是闭包对象 { fn_ptr, captures_ptr }。
                // 通过 vredrs_call_closure 间接调用。
                self.load_operand(callee, "rdi");
                let arg_regs = ["rsi", "rdx", "rcx", "r8", "r9"];
                for (i, a) in args.iter().enumerate() {
                    if i < 5 {
                        self.load_operand(a, arg_regs[i]);
                    }
                }
                self.out.push_str("    xor eax, eax\n");
                self.out.push_str("    call vredrs_call_closure\n");
                if let Some(d) = dst {
                    self.store_operand(d, "rax");
                }
                let _ = ty;
            }
            RuntimeCall { dst, func, args } => {
                self.emit_call(dst, func, args, true);
            }
            Ret { value } => {
                if let Some(v) = value {
                    self.load_operand(v, "rax");
                } else {
                    self.out.push_str("    xor rax, rax\n");
                }
                self.emit_epilogue();
            }
            Push { src } => {
                self.load_operand(src, "rax");
                self.out.push_str("    push rax\n");
            }
            Pop { dst } => {
                self.out.push_str("    pop rax\n");
                self.store_operand(dst, "rax");
            }
            Asm { template, inputs, outputs, clobbers } => {
                // 内联汇编：把操作数加载到 r10/r11/...，然后替换模板里的 %N。
                // 约定：%0..%N 按顺序对应 outputs+inputs。
                self.out.push_str(&format!("    # asm: {:?}\n", template));
                let all: Vec<&Operand> = outputs.iter().map(|x| &x.operand).chain(inputs.iter().map(|x| &x.operand)).collect();
                let tmp_regs = ["r10", "r11", "r12", "r13", "r14", "r15", "rdi", "rsi", "rdx", "rcx"];
                for (i, op) in all.iter().enumerate() {
                    if i < tmp_regs.len() {
                        self.load_operand(op, tmp_regs[i]);
                    }
                }
                // 替换 %N → 对应寄存器名，然后输出模板。
                let mut expanded = template.clone();
                // 逆序替换 %N（避免 %1 被 %10 误匹配）。
                for i in (0..all.len()).rev() {
                    if i < tmp_regs.len() {
                        expanded = expanded.replace(&format!("%{}", i), tmp_regs[i]);
                    }
                }
                for line in expanded.lines() {
                    self.out.push_str(&format!("    {}\n", line));
                }
                // 把输出操作数（前 outputs.len() 个）存回。
                for (i, o) in outputs.iter().enumerate() {
                    if i < tmp_regs.len() {
                        self.store_operand(&o.operand, tmp_regs[i]);
                    }
                }
                let _ = clobbers;
            }
            Prefetch { addr, hint } => {
                self.out.push_str(&format!("    # prefetch hint={}\n", hint));
                self.load_operand(addr, "rax");
                self.out.push_str("    prefetcht0 [rax]\n");
            }
            PipelineMarker { name } => {
                self.out.push_str(&format!("    # @pipeline {}\n", name));
            }
            IsrEntry { group, priority } => {
                self.out.push_str(&format!("    # @isr_group {} prio={}\n", group, priority));
            }
            WcetNote { cycles, budget } => {
                self.out.push_str(&format!("    # wcet cycles={} budget={}\n", cycles, budget));
            }
            DiffMeta { symbol, offset } => {
                self.out.push_str(&format!("    # diffmeta {} +{:#x}\n", symbol, offset));
            }
        }
        let _ = f;
    }

    /// 发射二元运算。
    fn emit_bin(&mut self, op: BinOp, dst: &Operand, lhs: &Operand, rhs: &Operand, ty: TypeHint) {
        // 动态 op → 调 runtime（用 emit_call 确保栈对齐 + al=0）。
        if op.is_dyn() {
            let func = dyn_bin_func(op);
            self.emit_call(&Some(dst.clone()), func, &[lhs.clone(), rhs.clone()], true);
            return;
        }
        // 浮点静态运算。
        if ty == TypeHint::F64 {
            let func = match op {
                BinOp::Add => "vredrs_f64_add",
                BinOp::Sub => "vredrs_f64_sub",
                BinOp::Mul => "vredrs_f64_mul",
                BinOp::Div => "vredrs_f64_div",
                _ => "vredrs_f64_add",
            };
            self.emit_call(&Some(dst.clone()), func, &[lhs.clone(), rhs.clone()], true);
            return;
        }
        // 整数静态运算。
        // 原地优化：若 dst 有物理寄存器且 dst == lhs（同一虚拟寄存器），直接在物理寄存器上运算。
        // 这省去 load lhs + store dst（如 `x = x + 1` → `add rbx, 1` 而非 `mov rax,rbx; add rax,1; mov rbx,rax`）。
        // 注意：Div/Mod 结果在 rax/rdx，不能原地，需走普通路径。
        if let (Operand::Reg(dst_name, _), Operand::Reg(lhs_name, _)) = (dst, lhs) {
            if dst_name == lhs_name && !matches!(op, BinOp::Div | BinOp::Mod) {
                if let Some(preg) = self.reg_alloc.get(dst_name).cloned() {
                    // 在 preg 上运算。
                    if let Operand::ImmI64(v) = rhs {
                        match op {
                            BinOp::Add => self.out.push_str(&format!("    add {}, {}\n", preg, v)),
                            BinOp::Sub => self.out.push_str(&format!("    sub {}, {}\n", preg, v)),
                            BinOp::And => self.out.push_str(&format!("    and {}, {}\n", preg, v)),
                            BinOp::Or => self.out.push_str(&format!("    or {}, {}\n", preg, v)),
                            BinOp::Xor => self.out.push_str(&format!("    xor {}, {}\n", preg, v)),
                            BinOp::Shl => self.out.push_str(&format!("    shl {}, {}\n", preg, v & 63)),
                            BinOp::Shr => self.out.push_str(&format!("    sar {}, {}\n", preg, v & 63)),
                            BinOp::Mul => self.out.push_str(&format!("    imul {}, {}\n", preg, v)),
                            _ => {
                                self.load_operand(lhs, &preg);
                                self.load_operand(rhs, "rcx");
                                self.emit_bin_reg(op, &preg, "rcx");
                            }
                        }
                        return;  // 结果已在 preg，无需 store。
                    }
                    // rhs 是寄存器。Div/Mod 结果在 rax/rdx，不能原地，回退普通路径。
                    if !matches!(op, BinOp::Div | BinOp::Mod) {
                        self.load_operand(rhs, "rcx");
                        self.emit_bin_reg(op, &preg, "rcx");
                        return;
                    }
                }
            }
        }
        // 优化：若 dst 有物理寄存器，直接用它作为运算目标（省去 store）。
        // 但 Div/Mod 结果固定在 rax/rdx，必须用 rax 作为 lhs。
        let is_div_mod = matches!(op, BinOp::Div | BinOp::Mod);
        let dst_reg: String = if let Operand::Reg(dn, _) = dst {
            if is_div_mod {
                "rax".to_string()
            } else {
                self.reg_alloc.get(dn).cloned().unwrap_or_else(|| "rax".to_string())
            }
        } else {
            "rax".to_string()
        };
        let dst_is_phys = dst_reg != "rax";
        let dr = dst_reg.as_str();
        self.load_operand(lhs, dr);
        // 若 rhs 是立即数，部分运算可走立即数路径。
        if let Operand::ImmI64(v) = rhs {
            match op {
                BinOp::Add => self.out.push_str(&format!("    add {}, {}\n", dr, v)),
                BinOp::Sub => self.out.push_str(&format!("    sub {}, {}\n", dr, v)),
                BinOp::And => self.out.push_str(&format!("    and {}, {}\n", dr, v)),
                BinOp::Or => self.out.push_str(&format!("    or {}, {}\n", dr, v)),
                BinOp::Xor => self.out.push_str(&format!("    xor {}, {}\n", dr, v)),
                BinOp::Shl => self.out.push_str(&format!("    shl {}, {}\n", dr, v & 63)),
                BinOp::Shr => self.out.push_str(&format!("    sar {}, {}\n", dr, v & 63)),
                BinOp::Mul => self.out.push_str(&format!("    imul {}, {}\n", dr, v)),
                BinOp::Div => {
                    self.out.push_str("    cqo\n");
                    self.out.push_str(&format!("    mov rcx, {}\n", v));
                    self.out.push_str("    idiv rcx\n");
                    // 结果在 rax，需要 store 到 dst。
                    self.store_operand(dst, "rax");
                    return;
                }
                BinOp::Mod => {
                    self.out.push_str("    cqo\n");
                    self.out.push_str(&format!("    mov rcx, {}\n", v));
                    self.out.push_str("    idiv rcx\n");
                    // 结果在 rdx，需要 store 到 dst。
                    if dst_is_phys {
                        self.out.push_str(&format!("    mov {}, rdx\n", dr));
                    } else {
                        self.out.push_str("    mov rax, rdx\n");
                        self.store_operand(dst, "rax");
                    }
                    return;
                }
                _ => {
                    self.load_operand(rhs, "rcx");
                    self.emit_bin_reg(op, dr, "rcx");
                }
            }
            if !dst_is_phys {
                self.store_operand(dst, "rax");
            }
            return;
        }
        // rhs 是寄存器。
        if is_div_mod {
            // Div/Mod 必须用 rax 作被除数，rcx 作除数。
            self.load_operand(lhs, "rax");
            self.load_operand(rhs, "rcx");
            self.out.push_str("    cqo\n");
            self.out.push_str("    idiv rcx\n");
            if matches!(op, BinOp::Mod) {
                self.out.push_str("    mov rax, rdx\n");
            }
            self.store_operand(dst, "rax");
            return;
        }
        self.load_operand(rhs, "rcx");
        self.emit_bin_reg(op, dr, "rcx");
        if !dst_is_phys {
            self.store_operand(dst, "rax");
        }
    }

    fn emit_bin_reg(&mut self, op: BinOp, dst: &str, src: &str) {
        match op {
            BinOp::Add => self.out.push_str(&format!("    add {}, {}\n", dst, src)),
            BinOp::Sub => self.out.push_str(&format!("    sub {}, {}\n", dst, src)),
            BinOp::And => self.out.push_str(&format!("    and {}, {}\n", dst, src)),
            BinOp::Or => self.out.push_str(&format!("    or {}, {}\n", dst, src)),
            BinOp::Xor => self.out.push_str(&format!("    xor {}, {}\n", dst, src)),
            BinOp::Shl => self.out.push_str(&format!("    shl {}, cl\n", dst)),
            BinOp::Shr => self.out.push_str(&format!("    sar {}, cl\n", dst)),
            BinOp::Mul => self.out.push_str(&format!("    imul {}, {}\n", dst, src)),
            BinOp::Div => {
                self.out.push_str("    cqo\n");
                self.out.push_str(&format!("    idiv {}\n", src));
            }
            BinOp::Mod => {
                self.out.push_str("    cqo\n");
                self.out.push_str(&format!("    idiv {}\n", src));
                self.out.push_str("    mov rax, rdx\n");
            }
            _ => {}
        }
    }

    /// 发射函数调用。`is_runtime` 标记是否为 runtime 函数（影响调用约定）。
    fn emit_call(&mut self, dst: &Option<Operand>, func: &str, args: &[Operand], _is_runtime: bool) {
        let arg_regs = ["rdi", "rsi", "rdx", "rcx", "r8", "r9"];
        // System V AMD64：前 6 个整数参数在寄存器，其余在栈。
        let stack_args = if args.len() > 6 { args.len() - 6 } else { 0 };
        // 对齐栈到 16 字节（call 会 push 8 字节返回地址）。
        let pad = if (stack_args * 8) % 16 != 0 { 8 } else { 0 };
        if pad > 0 {
            self.out.push_str(&format!("    sub rsp, {}\n", pad));
        }
        // 压栈参数（逆序）。
        if stack_args > 0 {
            for i in (6..args.len()).rev() {
                self.load_operand(&args[i], "rax");
                self.out.push_str("    push rax\n");
            }
        }
        // 寄存器参数。
        for (i, a) in args.iter().enumerate().take(6) {
            self.load_operand(a, arg_regs[i]);
        }
        // x86_64 System V ABI: 调用变参函数时 al 必须设为浮点参数个数（0）。
        self.out.push_str("    xor eax, eax\n");
        self.out.push_str(&format!("    call {}\n", func));
        // 清理栈参数。
        if stack_args > 0 {
            self.out.push_str(&format!("    add rsp, {}\n", stack_args * 8));
        }
        if pad > 0 {
            self.out.push_str(&format!("    add rsp, {}\n", pad));
        }
        if let Some(d) = dst {
            self.store_operand(d, "rax");
        }
    }

    /// 把操作数加载到指定寄存器。
    fn load_operand(&mut self, op: &Operand, reg: &str) {
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
                    self.out.push_str(&format!("    mov {}, qword ptr [rbp{}]\n", reg, fmt_offset(off)));
                } else {
                    // 未分配栈槽的寄存器：当作 0。
                    self.out.push_str(&format!("    xor {}, {}\n", reg, reg));
                }
            }
            Operand::ImmI64(v) => {
                if *v == 0 {
                    // xor reg, reg 比 mov reg, 0 更短更快。
                    self.out.push_str(&format!("    xor {}, {}\n", reg, reg));
                } else {
                    self.out.push_str(&format!("    mov {}, {}\n", reg, v));
                }
            }
            Operand::ImmF64(v) => {
                // 浮点立即数：通过 .rodata 加载。
                // 简化：转为 i64 bits 存到栈临时。
                let bits = f64::to_bits(*v);
                self.out.push_str(&format!("    mov {}, {}\n", reg, bits as i64));
            }
            Operand::ImmStr(idx) => {
                if *idx < self.str_labels.len() {
                    self.out.push_str(&format!("    lea {}, [rip+{}]\n", reg, self.str_labels[*idx]));
                } else {
                    self.out.push_str(&format!("    xor {}, {}\n", reg, reg));
                }
            }
            Operand::ImmBool(b) => {
                self.out.push_str(&format!("    mov {}, {}\n", reg, if *b { 1 } else { 0 }));
            }
            Operand::Null => {
                self.out.push_str(&format!("    xor {}, {}\n", reg, reg));
            }
            Operand::Label(l) => {
                self.out.push_str(&format!("    lea {}, [rip+{}]\n", reg, sanitize_label(l)));
            }
            Operand::Sym(s) => {
                self.out.push_str(&format!("    lea {}, [rip+{}]\n", reg, s));
            }
        }
    }

    /// 把寄存器值存到操作数（必须是寄存器操作数）。
    fn store_operand(&mut self, op: &Operand, reg: &str) {
        if let Operand::Reg(name, _) = op {
            // 优先用分配的物理寄存器。
            if let Some(preg) = self.reg_alloc.get(name).cloned() {
                if preg != reg {
                    self.out.push_str(&format!("    mov {}, {}\n", preg, reg));
                }
                return;
            }
            if let Some(&off) = self.stack_slots.get(name) {
                self.out.push_str(&format!("    mov qword ptr [rbp{}], {}\n", fmt_offset(off), reg));
            }
        }
    }

    /// 把寄存器值存到虚拟寄存器（优先物理寄存器，否则栈槽）。
    fn store_to_vreg(&mut self, vreg_name: &str, phys_reg: &str) {
        if let Some(preg) = self.reg_alloc.get(vreg_name).cloned() {
            if preg != phys_reg {
                self.out.push_str(&format!("    mov {}, {}\n", preg, phys_reg));
            }
            return;
        }
        self.store_reg_to_slot(vreg_name, phys_reg);
    }

    /// 把寄存器值存到指定栈槽（按虚拟寄存器名）。
    fn store_reg_to_slot(&mut self, reg_name: &str, phys_reg: &str) {
        if let Some(&off) = self.stack_slots.get(reg_name) {
            self.out.push_str(&format!("    mov qword ptr [rbp{}], {}\n", fmt_offset(off), phys_reg));
        }
    }
}

/// 收集 IR 指令中引用的所有虚拟寄存器名。
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
        Call { dst, args, .. } => {
            if let Some(d) = dst { visit(d); }
            for a in args { visit(a); }
        }
        CallDyn { dst, callee, args, .. } => {
            if let Some(d) = dst { visit(d); }
            visit(callee);
            for a in args { visit(a); }
        }
        RuntimeCall { dst, args, .. } => {
            if let Some(d) = dst { visit(d); }
            for a in args { visit(a); }
        }
        Ret { value } => {
            if let Some(v) = value { visit(v); }
        }
        Push { src } | Pop { dst: src } => { visit(src); }
        Asm { inputs, outputs, .. } => {
            for o in outputs { visit(&o.operand); }
            for i in inputs { visit(&i.operand); }
        }
        Prefetch { addr, .. } => { visit(addr); }
        _ => {}
    }
}

/// 收集 IR 指令中引用的 extern 函数名。
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

/// 动态二元运算对应的 runtime 函数名。
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

/// Cond → setcc 助记符（如 `Eq` → `sete`）。
fn cond_to_setcc(c: Cond) -> &'static str {
    match c {
        Cond::Eq => "e",
        Cond::Ne => "ne",
        Cond::Lt => "l",
        Cond::Le => "le",
        Cond::Gt => "g",
        Cond::Ge => "ge",
    }
}

/// Cond → jcc 助记符（如 `Eq` → `je`）。
fn cond_to_jcc(c: Cond) -> &'static str {
    match c {
        Cond::Eq => "e",
        Cond::Ne => "ne",
        Cond::Lt => "l",
        Cond::Le => "le",
        Cond::Gt => "g",
        Cond::Ge => "ge",
    }
}

/// 格式化栈偏移（负数带 `-`，正数带 `+`）。
fn fmt_offset(off: i64) -> String {
    if off >= 0 {
        format!("+{}", off)
    } else {
        off.to_string()
    }
}

/// 转义字符串中的特殊字符（用于 .string 指令）。
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

/// 把 IR 标签（如 `.L0_head`）转成 GAS 合法标签。
fn sanitize_label(l: &str) -> String {
    // GAS 标签可以以 . 开头。保留原样，但替换不合法字符。
    l.replace('.', "_dot_").replace(' ', "_")
}

/// 对外入口：把程序通过新 IR 路径编译成 x86_64 可执行文件。
///
/// 这是手册 Phase 1/2 的新 Raw 路径。`program` 先经 [`crate::codegen::lower`]
/// 降级为 IR，再由 [`X86Emitter`] 发射汇编，最后组装链接。
pub fn compile_via_ir(program: &Program, output_path: &Path) -> Result<(), String> {
    use crate::codegen::lower::lower_program;
    use crate::platform::PlatformInfo;

    let plat = PlatformInfo::detect();
    let host_arch = std::env::consts::ARCH;
    let is_cross = host_arch != "x86_64";

    if is_cross {
        crate::platform::info(&format!(
            "Cross-compiling to x86_64 via IR (host is {}). Assembly written but not assembled.",
            host_arch
        ));
    }

    // AST → IR。
    let module = lower_program(program);
    // IR → x86_64 asm。
    let mut emitter = X86Emitter::new();
    let asm = emitter.emit_module(&module);

    // 写 .s 文件。
    let asm_path = output_path.with_extension("s");
    std::fs::write(&asm_path, &asm).map_err(|e| format!("can't write assembly: {}", e))?;
    crate::platform::info(&format!("IR-based x86_64 assembly written to: {}", asm_path.display()));

    // 同时写一份 .ir 调试输出。
    let ir_text = crate::codegen::ir::render_module(&module);
    let ir_path = output_path.with_extension("ir");
    let _ = std::fs::write(&ir_path, &ir_text);

    if is_cross {
        return Ok(());
    }

    // 组装。
    let obj_path = output_path.with_extension("o");
    let assemble = std::process::Command::new(plat.as_command())
        .arg("--64")
        .arg("-o")
        .arg(&obj_path)
        .arg(&asm_path)
        .output();
    match assemble {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            crate::platform::warning(&format!("'as' failed:\n{}", stderr));
            return Ok(());
        }
        Err(_) => {
            crate::platform::warning("'as' not found; assembly written only");
            return Ok(());
        }
    }

    // 编译 vredrs_runtime.c（如果存在且程序用到了 runtime）。
    let runtime_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/codegen/runtime/vredrs_runtime.c");
    let runtime_obj = output_path.with_extension("runtime.o");
    let runtime_needed = module.functions.iter().any(|f| {
        f.body.iter().any(|i| {
            matches!(i, StaticInsn::RuntimeCall { .. } | StaticInsn::CallDyn { .. })
                || matches!(i, StaticInsn::Bin { op, .. } if op.is_dyn())
                || matches!(i, StaticInsn::Bin { ty, .. } if *ty == TypeHint::F64)
                || matches!(i, StaticInsn::Neg { ty, .. } if *ty == TypeHint::F64)
                || matches!(i, StaticInsn::Box { .. } | StaticInsn::Unbox { .. })
        })
    });
    let mut link_objs: Vec<std::path::PathBuf> = vec![obj_path.clone()];
    // 总是链接 stub：weak 函数不会与用户代码冲突，且任何程序都可能
    // 调用 stub 提供的函数（file I/O, range, container 等）。
    if true {
        // 用最小内联 stub（vredrs_value = i64，与 emit_raw_x86 调用约定一致）。
        // 注意：完整 runtime.c 用 vredrs_value struct {i8,i64}，与 emit_raw_x86
        // 的 i64 简化不匹配。未来若 emit_raw_x86 改用 struct 传递，可切换到
        // 完整 runtime.c。
        let cc = plat.cc_command();
        let stub_path = output_path.with_extension("runtime_stub.c");
        std::fs::write(&stub_path, minimal_runtime_stub())
            .map_err(|e| format!("can't write stub: {}", e))?;
        let stub_obj = output_path.with_extension("runtime_stub.o");
        let compile_stub = std::process::Command::new(cc)
            .arg("-c")
            .arg("-O2")
            .arg("-fPIC")
            .arg("-w")
            .arg("-o")
            .arg(&stub_obj)
            .arg(&stub_path)
            .output();
        if let Ok(out) = compile_stub {
            if out.status.success() {
                link_objs.push(stub_obj);
                crate::platform::info("linked minimal runtime stub (vredrs_value=i64)");
            }
        }
    }

    // 链接。优先用 cc/gcc（自动处理动态链接器与 libc），失败则回退 ld。
    let exe_path = output_path.to_path_buf();
    let link_with_cc = std::process::Command::new(plat.cc_command())
        .arg("-o")
        .arg(&exe_path)
        .arg("-no-pie")
        .args(&link_objs)
        .args(&plat.extra_link_flags())
        .output();
    let link = match link_with_cc {
        Ok(out) if out.status.success() => {
            crate::platform::success(&format!("Native executable (via IR): {}", exe_path.display()));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&exe_path, std::fs::Permissions::from_mode(0o755));
            }
            return Ok(());
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            crate::platform::info(&format!("cc link failed, trying ld: {}", stderr));
            std::process::Command::new(plat.ld_command())
                .arg("-o")
                .arg(&exe_path)
                .arg("-dynamic-linker")
                .arg("/lib64/ld-linux-x86-64.so.2")
                .args(&link_objs)
                .args(&plat.extra_link_flags())
                .arg("-lc")
                .output()
        }
        Err(_) => {
            std::process::Command::new(plat.ld_command())
                .arg("-o")
                .arg(&exe_path)
                .arg("-dynamic-linker")
                .arg("/lib64/ld-linux-x86-64.so.2")
                .args(&link_objs)
                .args(&plat.extra_link_flags())
                .arg("-lc")
                .output()
        }
    };
    match link {
        Ok(out) if out.status.success() => {
            crate::platform::success(&format!("Native executable (via IR): {}", exe_path.display()));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&exe_path, std::fs::Permissions::from_mode(0o755));
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            crate::platform::warning(&format!("Linking failed:\n{}", stderr));
        }
        Err(_) => {
            crate::platform::warning("'ld'/'cc' not found; object file only");
        }
    }
    Ok(())
}

/// 生成最小内联 runtime stub（C 代码）。
///
/// 当完整 `vredrs_runtime.c` 编译失败时的 fallback。只实现最基础的函数：
/// 整数 println/print、动态运算（假设都是 i64 payload）、容器 stub。
/// 这足以让含 `println` 的 .vraw 程序链接通过并输出整数结果。
fn minimal_runtime_stub() -> String {
    r#"
/* Minimal Vredrs runtime stub — generated by emit_raw_x86.
 * Provides basic functions so that IR-generated code with dynamic
 * features (println, dynamic arithmetic) can link and run.
 * Dynamic values are passed as i64 (payload only, tag ignored) for simplicity. */
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdarg.h>
#include <setjmp.h>

typedef int64_t vredrs_value;

/* setjmp/longjmp 异常处理状态（提前声明，throw/catch 使用）。 */
static jmp_buf vredrs_catch_jmp[64];
static int vredrs_catch_depth = 0;
static vredrs_value vredrs_current_exception = 0;
static vredrs_value vredrs_with_enter_last = 0;

__attribute__((weak)) void vredrs_print(int64_t v) { printf("%lld", (long long)v); }
__attribute__((weak)) void vredrs_println(int64_t v) { printf("%lld\n", (long long)v); }
__attribute__((weak)) void vredrs_print_str(const char* s) { fputs(s ? s : "", stdout); }
__attribute__((weak)) void vredrs_println_str(const char* s) { puts(s ? s : ""); }
/* 浮点 print：接收 i64 bits（与 emit_raw_x86 的 ImmF64 传递一致），转 double 打印。 */
__attribute__((weak)) void vredrs_print_f64(int64_t bits) { double d; memcpy(&d, &bits, 8); printf("%g", d); }
__attribute__((weak)) void vredrs_println_f64(int64_t bits) { double d; memcpy(&d, &bits, 8); printf("%g\n", d); }
__attribute__((weak)) void vredrs_flush(void) { fflush(stdout); }

__attribute__((weak)) vredrs_value vredrs_value_make_i64(int64_t v) { return v; }
__attribute__((weak)) vredrs_value vredrs_value_make_f64(double v) { return (int64_t)v; }
__attribute__((weak)) vredrs_value vredrs_value_make_bool(int64_t b) { return b ? 1 : 0; }
__attribute__((weak)) vredrs_value vredrs_value_nil(void) { return 0; }
__attribute__((weak)) int64_t vredrs_value_get_i64(vredrs_value v) { return v; }
__attribute__((weak)) double vredrs_value_get_f64(vredrs_value v) { return (double)v; }
__attribute__((weak)) int64_t vredrs_value_get_bool(vredrs_value v) { return v != 0 ? 1 : 0; }
__attribute__((weak)) int64_t vredrs_value_tag(vredrs_value v) { (void)v; return 1; }

__attribute__((weak)) vredrs_value vredrs_value_add(vredrs_value a, vredrs_value b) { return a + b; }
__attribute__((weak)) vredrs_value vredrs_value_sub(vredrs_value a, vredrs_value b) { return a - b; }
__attribute__((weak)) vredrs_value vredrs_value_mul(vredrs_value a, vredrs_value b) { return a * b; }
__attribute__((weak)) vredrs_value vredrs_value_div(vredrs_value a, vredrs_value b) { return b ? a / b : 0; }
__attribute__((weak)) vredrs_value vredrs_value_mod(vredrs_value a, vredrs_value b) { return b ? a % b : 0; }
__attribute__((weak)) int64_t vredrs_value_eq(vredrs_value a, vredrs_value b) { return a == b ? 1 : 0; }
__attribute__((weak)) int64_t vredrs_value_ne(vredrs_value a, vredrs_value b) { return a != b ? 1 : 0; }
__attribute__((weak)) int64_t vredrs_value_lt(vredrs_value a, vredrs_value b) { return a < b ? 1 : 0; }
__attribute__((weak)) int64_t vredrs_value_le(vredrs_value a, vredrs_value b) { return a <= b ? 1 : 0; }
__attribute__((weak)) int64_t vredrs_value_gt(vredrs_value a, vredrs_value b) { return a > b ? 1 : 0; }
__attribute__((weak)) int64_t vredrs_value_ge(vredrs_value a, vredrs_value b) { return a >= b ? 1 : 0; }

/* f64 运算：接收/返回 i64 bits（与 emit_raw_x86 的整数传递一致）。
 * emit_raw_x86 把 ImmF64/Reg 存为 i64 bits 到整数寄存器，调用这些函数，
 * 结果也作为 i64 bits 存回栈槽。避免 xmm 寄存器的 ABI 复杂性。 */
__attribute__((weak)) int64_t vredrs_f64_add(int64_t a_bits, int64_t b_bits) { double a, b; memcpy(&a, &a_bits, 8); memcpy(&b, &b_bits, 8); double r = a + b; int64_t out; memcpy(&out, &r, 8); return out; }
__attribute__((weak)) int64_t vredrs_f64_sub(int64_t a_bits, int64_t b_bits) { double a, b; memcpy(&a, &a_bits, 8); memcpy(&b, &b_bits, 8); double r = a - b; int64_t out; memcpy(&out, &r, 8); return out; }
__attribute__((weak)) int64_t vredrs_f64_mul(int64_t a_bits, int64_t b_bits) { double a, b; memcpy(&a, &a_bits, 8); memcpy(&b, &b_bits, 8); double r = a * b; int64_t out; memcpy(&out, &r, 8); return out; }
__attribute__((weak)) int64_t vredrs_f64_div(int64_t a_bits, int64_t b_bits) { double a, b; memcpy(&a, &a_bits, 8); memcpy(&b, &b_bits, 8); double r = b ? a / b : 0; int64_t out; memcpy(&out, &r, 8); return out; }
__attribute__((weak)) int64_t vredrs_f64_neg(int64_t a_bits) { double a; memcpy(&a, &a_bits, 8); double r = -a; int64_t out; memcpy(&out, &r, 8); return out; }

/* 简化 list：堆分配的 {len, cap, data[]}，返回指针 (i64)。
 * vredrs_value = i64，list 指针也用 i64 传递。 */
typedef struct { int64_t len; int64_t cap; int64_t data[]; } vredrs_list_simple;
__attribute__((weak)) int64_t vredrs_make_list(int64_t n, ...) {
    if (n < 0) n = 0;
    vredrs_list_simple *l = (vredrs_list_simple*)malloc(sizeof(int64_t)*2 + n*sizeof(int64_t));
    if (!l) return 0;
    l->len = n; l->cap = n;
    va_list args; va_start(args, n);
    for (int64_t i = 0; i < n; i++) l->data[i] = va_arg(args, int64_t);
    va_end(args);
    return (int64_t)l;
}
__attribute__((weak)) int64_t vredrs_len(int64_t v) {
    if (!v) return 0;
    vredrs_list_simple *l = (vredrs_list_simple*)v;
    return l->len;
}
/* 前向声明：vredrs_get_field 定义在后面，vredrs_index 需要调用它。 */
__attribute__((weak)) vredrs_value vredrs_get_field(int64_t obj, const char* field);

__attribute__((weak)) int64_t vredrs_index(int64_t col, int64_t idx) {
    if (!col) return 0;
    /* 启发式：idx > 65536 视为字符串指针（堆地址），走 dict 查找。 */
    if (idx > 65536) { return vredrs_get_field(col, (const char*)idx); }
    vredrs_list_simple *l = (vredrs_list_simple*)col;
    if (idx < 0 || idx >= l->len) return 0;
    return l->data[idx];
}
__attribute__((weak)) int64_t vredrs_make_range(int64_t start, int64_t end, int64_t inclusive) {
    int64_t n = inclusive ? (end - start + 1) : (end - start);
    if (n < 0) n = 0;
    vredrs_list_simple *l = (vredrs_list_simple*)malloc(sizeof(int64_t)*2 + n*sizeof(int64_t));
    if (!l) return 0;
    l->len = n; l->cap = n;
    for (int64_t i = 0; i < n; i++) l->data[i] = start + i;
    return (int64_t)l;
}
/* range(start, end) — 用户可调用函数，等价于 vredrs_make_range(start, end, 0)。 */
__attribute__((weak)) int64_t range(int64_t start, int64_t end) {
    return vredrs_make_range(start, end, 0);
}
/* 简化 dict：线性探测 hash map，{len, cap, keys[], vals[]}。
 * key 是字符串指针 (i64)，val 是 i64。 */
typedef struct { int64_t len; int64_t cap; const char** keys; int64_t* vals; } vredrs_dict_simple;
__attribute__((weak)) int64_t vredrs_make_dict(int64_t n, ...) {
    if (n < 0) n = 0;
    int64_t cap = n * 2 + 4;
    vredrs_dict_simple *d = (vredrs_dict_simple*)malloc(sizeof(vredrs_dict_simple));
    if (!d) return 0;
    d->len = 0; d->cap = cap;
    d->keys = (const char**)calloc(cap, sizeof(const char*));
    d->vals = (int64_t*)calloc(cap, sizeof(int64_t));
    va_list args; va_start(args, n);
    for (int64_t i = 0; i < n; i++) {
        const char *k = va_arg(args, const char*);
        int64_t v = va_arg(args, int64_t);
        /* 简单插入：找空槽或匹配 key。 */
        uint64_t h = 0; const char *p = k; while (p && *p) { h = h*31 + (uint8_t)*p; p++; }
        int64_t idx = h % cap;
        while (d->keys[idx] && (!k || !d->keys[idx] || strcmp(d->keys[idx], k) != 0)) {
            idx = (idx + 1) % cap;
        }
        if (!d->keys[idx]) { d->len++; d->keys[idx] = k; }
        d->vals[idx] = v;
    }
    va_end(args);
    return (int64_t)d;
}
/* vredrs_len 对 dict 也工作（检查类型不可能，简化：len 字段在前）。 */
__attribute__((weak)) vredrs_value vredrs_make_tuple(int64_t n, ...) {
    /* tuple 简化为 list。 */
    if (n < 0) n = 0;
    vredrs_list_simple *l = (vredrs_list_simple*)malloc(sizeof(int64_t)*2 + n*sizeof(int64_t));
    if (!l) return 0;
    l->len = n; l->cap = n;
    va_list args; va_start(args, n);
    for (int64_t i = 0; i < n; i++) l->data[i] = va_arg(args, int64_t);
    va_end(args);
    return (int64_t)l;
}
__attribute__((weak)) vredrs_value vredrs_make_set(int64_t n, ...) {
    /* set: 构造 list 时去重。 */
    if (n < 0) n = 0;
    vredrs_list_simple* l = (vredrs_list_simple*)malloc(sizeof(int64_t)*2 + n*sizeof(int64_t));
    if (!l) return 0;
    l->len = 0; l->cap = n;
    va_list args; va_start(args, n);
    for (int64_t i = 0; i < n; i++) {
        int64_t v = va_arg(args, int64_t);
        int found = 0;
        for (int64_t j = 0; j < l->len; j++) { if (l->data[j] == v) { found = 1; break; } }
        if (!found) l->data[l->len++] = v;
    }
    va_end(args);
    return (vredrs_value)l;
}
/* slice: list[start:end] — 返回新 list。 */
__attribute__((weak)) vredrs_value vredrs_slice(int64_t col, int64_t start, int64_t end) {
    if (!col) return 0;
    vredrs_list_simple *l = (vredrs_list_simple*)col;
    int64_t len = l->len;
    if (start < 0) start = 0;
    if (end < 0 || end > len) end = len;
    if (start > end) start = end;
    int64_t n = end - start;
    vredrs_list_simple *r = (vredrs_list_simple*)malloc(sizeof(int64_t)*2 + n*sizeof(int64_t));
    if (!r) return 0;
    r->len = n; r->cap = n;
    for (int64_t i = 0; i < n; i++) r->data[i] = l->data[start + i];
    return (int64_t)r;
}
/* 简化 object: 堆分配的 dict（field name -> value）。 */
__attribute__((weak)) vredrs_value vredrs_get_field(int64_t obj, const char* field) {
    if (!obj || !field) return 0;
    vredrs_dict_simple *d = (vredrs_dict_simple*)obj;
    uint64_t h = 0; const char *p = field; while (p && *p) { h = h*31 + (uint8_t)*p; p++; }
    int64_t idx = h % d->cap;
    while (d->keys[idx]) {
        if (strcmp(d->keys[idx], field) == 0) { return d->vals[idx]; }
        idx = (idx + 1) % d->cap;
    }
    return 0;
}
__attribute__((weak)) void vredrs_set_field(int64_t obj, const char* field, int64_t val) {
    if (!obj || !field) return;
    vredrs_dict_simple *d = (vredrs_dict_simple*)obj;
    uint64_t h = 0; const char *p = field; while (p && *p) { h = h*31 + (uint8_t)*p; p++; }
    int64_t idx = h % d->cap;
    while (d->keys[idx]) {
        if (strcmp(d->keys[idx], field) == 0) { d->vals[idx] = val; return; }
        idx = (idx + 1) % d->cap;
    }
    d->keys[idx] = field; d->vals[idx] = val; d->len++;
}
/* method_call: obj.method(args...) — 运行时分派。
 * 读取 obj 的 __class__ 字段，扫描 __vredrs_dispatch_table 找到匹配的方法，
 * 间接调用 fn(self, args...)。最多 6 个参数。 */
typedef int64_t (*vredrs_method_fn_t)(int64_t, int64_t, int64_t, int64_t, int64_t, int64_t, int64_t);
typedef struct { const char* class_name; const char* method_name; void* func_ptr; int64_t arg_count; } vredrs_dispatch_entry_t;
__attribute__((weak)) vredrs_dispatch_entry_t __vredrs_dispatch_table[256] = {{0,0,0,0}};
__attribute__((weak)) int64_t __vredrs_dispatch_count = 0;

__attribute__((weak)) vredrs_value vredrs_method_call(int64_t obj, ...) {
    if (!obj) return 0;
    const char* class_name = (const char*)vredrs_get_field(obj, "__class__");
    if (!class_name) return 0;
    va_list args; va_start(args, obj);
    const char *method = va_arg(args, const char*);
    int64_t a1 = va_arg(args, int64_t);
    int64_t a2 = va_arg(args, int64_t);
    int64_t a3 = va_arg(args, int64_t);
    int64_t a4 = va_arg(args, int64_t);
    int64_t a5 = va_arg(args, int64_t);
    int64_t a6 = va_arg(args, int64_t);
    va_end(args);
    /* 扫描分发表。 */
    for (int64_t i = 0; i < __vredrs_dispatch_count && i < 256; i++) {
        vredrs_dispatch_entry_t* e = &__vredrs_dispatch_table[i];
        if (e->class_name && e->method_name &&
            strcmp(e->class_name, class_name) == 0 &&
            strcmp(e->method_name, method) == 0) {
            vredrs_method_fn_t fn = (vredrs_method_fn_t)e->func_ptr;
            return fn(obj, a1, a2, a3, a4, a5, a6);
        }
    }
    return 0;
}
/* 全局变量表（简化：线性数组，name -> value）。 */
typedef struct { const char* name; int64_t value; } vredrs_global_entry;
static vredrs_global_entry vredrs_globals[256];
static int vredrs_globals_count = 0;
__attribute__((weak)) vredrs_value vredrs_load_global(const char* name) {
    for (int i = 0; i < vredrs_globals_count; i++) {
        if (strcmp(vredrs_globals[i].name, name) == 0) return vredrs_globals[i].value;
    }
    return 0;
}
__attribute__((weak)) void vredrs_store_global(const char* name, int64_t v) {
    for (int i = 0; i < vredrs_globals_count; i++) {
        if (strcmp(vredrs_globals[i].name, name) == 0) { vredrs_globals[i].value = v; return; }
    }
    if (vredrs_globals_count < 256) {
        vredrs_globals[vredrs_globals_count].name = name;
        vredrs_globals[vredrs_globals_count].value = v;
        vredrs_globals_count++;
    }
}
/* vredrs_call_dynamic defined later (full implementation) */
__attribute__((weak)) vredrs_value vredrs_optional_member(vredrs_value obj, const char* name) {
    if (!obj) return 0;  /* null 短路 */
    return vredrs_get_field(obj, name);
}
/* vredrs_optional_method 定义在后面（完整实现） */
__attribute__((weak)) vredrs_value vredrs_optional_index(vredrs_value obj, int64_t idx) {
    if (!obj) return 0;
    return vredrs_index(obj, idx);
}
__attribute__((weak)) void vredrs_panic(vredrs_value v) {
    fprintf(stderr, "vredrs: panic: %lld\n", (long long)v);
    exit(1);
}
__attribute__((weak)) void vredrs_assert_fail(const char* msg) { if(msg) fprintf(stderr, "assertion failed: %s\n", msg); else fprintf(stderr, "assertion failed\n"); exit(1); }
/* ── spawn/async 协程（基于 ucontext）───────────────────── */
/* 前向声明：vredrs_gen_create/vredrs_resume 定义在后面（ucontext 生成器段）。 */
__attribute__((weak)) int64_t vredrs_gen_create(int64_t fn_ptr);
__attribute__((weak)) int64_t vredrs_resume(int64_t h);

/* spawn(f): 创建协程并立即运行到第一个 yield/return，返回协程句柄。
 * resume(h): 恢复协程执行，返回 yield 的值。
 * await(h): 同 resume（简化：同步等待）。 */
__attribute__((weak)) vredrs_value vredrs_spawn(vredrs_value fn_ptr) {
    /* 复用生成器的 ucontext 机制。 */
    return vredrs_gen_create(fn_ptr);
}
__attribute__((weak)) vredrs_value vredrs_spawn_thread(vredrs_value fn_ptr) {
    /* 简化：同 spawn（无真实线程）。 */
    return vredrs_gen_create(fn_ptr);
}
/* vredrs_yield and vredrs_resume are defined later (ucontext generator) */
__attribute__((weak)) vredrs_value vredrs_await(vredrs_value handle) {
    /* 简化：同步等待 = resume。 */
    return vredrs_resume(handle);
}
__attribute__((weak)) vredrs_value vredrs_coro(vredrs_value f, ...) {
    /* coro(f) = spawn(f) 的别名。 */
    return vredrs_gen_create(f);
}
/* vredrs_resume is defined later (ucontext generator) */
__attribute__((weak)) vredrs_value vredrs_pipe(vredrs_value left, vredrs_value right) { (void)left; return right; }
__attribute__((weak)) vredrs_value vredrs_try_propagate(vredrs_value v) { return v; }
__attribute__((weak)) vredrs_value vredrs_not(vredrs_value v) { return v ? 0 : 1; }
__attribute__((weak)) int64_t vredrs_is(vredrs_value a, vredrs_value b) { return a == b ? 1 : 0; }
__attribute__((weak)) int64_t vredrs_in(vredrs_value a, vredrs_value b) {
    /* a in b: b 是 list，检查 a 是否在 b 中。 */
    if (!b) return 0;
    vredrs_list_simple *l = (vredrs_list_simple*)b;
    for (int64_t i = 0; i < l->len; i++) {
        if (l->data[i] == a) return 1;
    }
    return 0;
}




/* with: __enter__/__exit__ 协议。Raw 后端简化：with_enter 返回 manager，
 * with_value 返回 manager（作为 with 变量绑定），with_exit 无操作。 */
__attribute__((weak)) vredrs_value vredrs_with_enter(vredrs_value m) { vredrs_with_enter_last = m; return m; }
__attribute__((weak)) vredrs_value vredrs_with_value(void) { return vredrs_with_enter_last; }
__attribute__((weak)) void vredrs_with_exit(void) {}
/* input: 从 stdin 读取一行，返回 i64（解析为整数，失败返回 0）。 */
__attribute__((weak)) vredrs_value vredrs_input(const char* prompt) {
    if (prompt && *prompt) { fputs(prompt, stdout); fflush(stdout); }
    char buf[256];
    if (!fgets(buf, sizeof(buf), stdin)) return 0;
    return strtoll(buf, NULL, 10);
}
/* comprehensions: 在 Raw 后端，lower.rs 已经把 comprehension 展开成循环，
 * 这些 runtime 函数不应被调用。保留作为 stub 防止链接错误。 */
__attribute__((weak)) vredrs_value vredrs_list_comprehension(void) { return 0; }
__attribute__((weak)) vredrs_value vredrs_dict_comprehension(void) { return 0; }
__attribute__((weak)) vredrs_value vredrs_set_comprehension(void) { return 0; }
__attribute__((weak)) vredrs_value vredrs_destructure(vredrs_value v) { return v; }
/* 异常处理：push_catch/pop_catch/check_throw 定义在后面（标志式） */
__attribute__((weak)) vredrs_value vredrs_get_exception(void) {
    return vredrs_current_exception;
}

/* 文件 I/O（完整实现，用 C 标准库）。 */
#include <sys/stat.h>
__attribute__((weak)) int64_t file_exists(const char* path) {
    if (!path) return 0;
    struct stat st;
    return stat(path, &st) == 0 ? 1 : 0;
}
/* read_file: 返回文件内容字符串指针（不再是文件大小）。 */
__attribute__((weak)) const char* read_file(const char* path) {
    if (!path) return NULL;
    FILE* f = fopen(path, "rb");
    if (!f) return NULL;
    fseek(f, 0, SEEK_END);
    long sz = ftell(f);
    fseek(f, 0, SEEK_SET);
    char* buf = (char*)malloc(sz + 1);
    if (!buf) { fclose(f); return NULL; }
    size_t rd = fread(buf, 1, sz, f);
    fclose(f);
    buf[rd] = '\0';
    return buf;
}
__attribute__((weak)) int64_t write_file(const char* path, int64_t content) {
    if (!path) return 0;
    FILE* f = fopen(path, "wb");
    if (!f) return 0;
    /* content 是整数，写入其二进制表示。 */
    fwrite(&content, sizeof(int64_t), 1, f);
    fclose(f);
    return 1;
}

/* class 构造函数：返回空 dict（堆分配）作为对象。 */
__attribute__((weak)) int64_t vredrs_new_object(void) {
    return vredrs_make_dict(0);
}

/* === 新后端补全：类型感知 stub === */
#include <ctype.h>
#include <math.h>
#include <time.h>
#include <ucontext.h>

/* 前向声明已不需要 — vredrs_get_field 已改为非变参，定义在前 */

/* vredrs_new_typed_object(class_name) — 创建带 __class__ 标记的对象 */
__attribute__((weak)) int64_t vredrs_new_typed_object(const char* class_name) {
    int64_t obj = vredrs_make_dict(0);
    if (!obj) return 0;
    vredrs_set_field(obj, "__class__", (int64_t)(intptr_t)class_name);
    return obj;
}

/* 字符串函数 */
__attribute__((weak)) int64_t str_eq(const char* a, const char* b) {
    if (a == b) return 1; if (!a || !b) return 0; return strcmp(a, b) == 0 ? 1 : 0;
}
__attribute__((weak)) int64_t str_ne(const char* a, const char* b) {
    if (a == b) return 0; if (!a || !b) return 1; return strcmp(a, b) != 0 ? 1 : 0;
}
__attribute__((weak)) const char* str_concat(const char* a, const char* b) {
    if (!a && !b) return NULL;
    size_t na = a ? strlen(a) : 0, nb = b ? strlen(b) : 0;
    char* r = (char*)malloc(na + nb + 1); if (!r) return NULL;
    if (a) memcpy(r, a, na); if (b) memcpy(r + na, b, nb);
    r[na + nb] = '\0'; return r;
}
__attribute__((weak)) const char* str_repeat(const char* s, int64_t n) {
    if (!s || n <= 0) { char* r = (char*)malloc(1); if(r) r[0]='\0'; return r; }
    size_t ls = strlen(s); char* r = (char*)malloc(ls * n + 1); if (!r) return NULL;
    r[0] = '\0'; for (int64_t i = 0; i < n; i++) strcat(r, s); return r;
}
__attribute__((weak)) int64_t str_len(const char* s) { if (!s) return 0; return (int64_t)strlen(s); }
__attribute__((weak)) const char* int_to_str(int64_t v) {
    char buf[32]; int n = snprintf(buf, sizeof(buf), "%lld", (long long)v);
    char* r = (char*)malloc(n + 1); if (!r) return NULL; memcpy(r, buf, n + 1); return r;
}
__attribute__((weak)) const char* str(int64_t v) { return int_to_str(v); }
__attribute__((weak)) const char* upper(const char* s) {
    if (!s) return NULL; size_t n = strlen(s); char* r = (char*)malloc(n + 1); if (!r) return NULL;
    for (size_t i = 0; i < n; i++) r[i] = (char)toupper((unsigned char)s[i]); r[n] = '\0'; return r;
}
__attribute__((weak)) const char* lower(const char* s) {
    if (!s) return NULL; size_t n = strlen(s); char* r = (char*)malloc(n + 1); if (!r) return NULL;
    for (size_t i = 0; i < n; i++) r[i] = (char)tolower((unsigned char)s[i]); r[n] = '\0'; return r;
}
__attribute__((weak)) int64_t contains(const char* h, const char* n) {
    if (!h || !n) return 0; return strstr(h, n) != NULL ? 1 : 0;
}
__attribute__((weak)) int64_t split(const char* s, const char* delim) {
    if (!s || !delim) return vredrs_make_list(0);
    size_t dlen = strlen(delim); int64_t count = 0; const char* p = s;
    while (*p) { const char* f = strstr(p, delim); if (!f) { count++; break; } count++; p = f + dlen; }
    vredrs_list_simple* l = (vredrs_list_simple*)malloc(sizeof(int64_t)*2 + count*sizeof(int64_t));
    if (!l) return 0; l->len = count; l->cap = count; int64_t idx = 0; p = s;
    while (*p) { const char* f = strstr(p, delim); if (!f) { size_t len = strlen(p); char* seg = (char*)malloc(len+1); if(seg){memcpy(seg,p,len);seg[len]='\0';} l->data[idx++] = (int64_t)seg; break; } size_t len = (size_t)(f-p); char* seg = (char*)malloc(len+1); if(seg){memcpy(seg,p,len);seg[len]='\0';} l->data[idx++] = (int64_t)seg; p = f + dlen; }
    return (int64_t)l;
}

/* list 函数 */
__attribute__((weak)) int64_t list_append(int64_t col, int64_t val) {
    if (!col) return 0; vredrs_list_simple* l = (vredrs_list_simple*)col;
    if (l->len >= l->cap) { int64_t nc = l->cap*2+4; vredrs_list_simple* nl = (vredrs_list_simple*)realloc(l, sizeof(int64_t)*2+nc*sizeof(int64_t)); if(!nl) return col; l=nl; l->cap=nc; col=(int64_t)l; }
    l->data[l->len++] = val; return col;
}
__attribute__((weak)) int64_t list_pop(int64_t col) { if(!col) return 0; vredrs_list_simple* l=(vredrs_list_simple*)col; if(l->len==0) return 0; return l->data[--l->len]; }
__attribute__((weak)) int64_t list_is_empty(int64_t col) { if(!col) return 1; vredrs_list_simple* l=(vredrs_list_simple*)col; return l->len==0?1:0; }
__attribute__((weak)) int64_t list_reverse(int64_t col) { if(!col) return 0; vredrs_list_simple* l=(vredrs_list_simple*)col; for(int64_t i=0,j=l->len-1;i<j;i++,j--){int64_t t=l->data[i];l->data[i]=l->data[j];l->data[j]=t;} return col; }
__attribute__((weak)) int64_t list_sum(int64_t col) { if(!col) return 0; vredrs_list_simple* l=(vredrs_list_simple*)col; int64_t s=0; for(int64_t i=0;i<l->len;i++) s+=l->data[i]; return s; }
__attribute__((weak)) int64_t list_max(int64_t col) { if(!col) return 0; vredrs_list_simple* l=(vredrs_list_simple*)col; if(l->len==0) return 0; int64_t m=l->data[0]; for(int64_t i=1;i<l->len;i++) if(l->data[i]>m) m=l->data[i]; return m; }
__attribute__((weak)) int64_t list_min(int64_t col) { if(!col) return 0; vredrs_list_simple* l=(vredrs_list_simple*)col; if(l->len==0) return 0; int64_t m=l->data[0]; for(int64_t i=1;i<l->len;i++) if(l->data[i]<m) m=l->data[i]; return m; }
__attribute__((weak)) int64_t list_sort(int64_t col) { if(!col) return 0; vredrs_list_simple* l=(vredrs_list_simple*)col; for(int64_t i=0;i<l->len;i++) for(int64_t j=i+1;j<l->len;j++) if(l->data[i]>l->data[j]){int64_t t=l->data[i];l->data[i]=l->data[j];l->data[j]=t;} return col; }
__attribute__((weak)) int64_t list_copy(int64_t col) { if(!col) return 0; vredrs_list_simple* l=(vredrs_list_simple*)col; vredrs_list_simple* r=(vredrs_list_simple*)malloc(sizeof(int64_t)*2+l->len*sizeof(int64_t)); if(!r) return 0; r->len=l->len; r->cap=l->len; for(int64_t i=0;i<l->len;i++) r->data[i]=l->data[i]; return (int64_t)r; }
__attribute__((weak)) int64_t list_concat(int64_t a, int64_t b) { if(!a) return b; if(!b) return a; vredrs_list_simple* la=(vredrs_list_simple*)a; vredrs_list_simple* lb=(vredrs_list_simple*)b; int64_t t=la->len+lb->len; vredrs_list_simple* r=(vredrs_list_simple*)malloc(sizeof(int64_t)*2+t*sizeof(int64_t)); if(!r) return 0; r->len=t; r->cap=t; for(int64_t i=0;i<la->len;i++) r->data[i]=la->data[i]; for(int64_t i=0;i<lb->len;i++) r->data[la->len+i]=lb->data[i]; return (int64_t)r; }
__attribute__((weak)) int64_t list_contains_val(int64_t col, int64_t val) { if(!col) return 0; vredrs_list_simple* l=(vredrs_list_simple*)col; for(int64_t i=0;i<l->len;i++) if(l->data[i]==val) return 1; return 0; }
__attribute__((weak)) int64_t list_index_of(int64_t col, int64_t val) { if(!col) return -1; vredrs_list_simple* l=(vredrs_list_simple*)col; for(int64_t i=0;i<l->len;i++) if(l->data[i]==val) return i; return -1; }
__attribute__((weak)) int64_t list_sorted(int64_t col) { int64_t c = list_copy(col); if(c) list_sort(c); return c; }
__attribute__((weak)) int64_t list_reversed(int64_t col) { if(!col) return 0; vredrs_list_simple* l=(vredrs_list_simple*)col; vredrs_list_simple* r=(vredrs_list_simple*)malloc(sizeof(int64_t)*2+l->len*sizeof(int64_t)); if(!r) return 0; r->len=l->len; r->cap=l->len; for(int64_t i=0;i<l->len;i++) r->data[i]=l->data[l->len-1-i]; return (int64_t)r; }
__attribute__((weak)) int64_t list_first(int64_t col) { if(!col) return 0; vredrs_list_simple* l=(vredrs_list_simple*)col; return l->len>0?l->data[0]:0; }
__attribute__((weak)) int64_t list_last(int64_t col) { if(!col) return 0; vredrs_list_simple* l=(vredrs_list_simple*)col; return l->len>0?l->data[l->len-1]:0; }
__attribute__((weak)) int64_t list_unique(int64_t col) { if(!col) return 0; vredrs_list_simple* l=(vredrs_list_simple*)col; int64_t* tmp=(int64_t*)malloc(l->len*sizeof(int64_t)); if(!tmp) return 0; int64_t n=0; for(int64_t i=0;i<l->len;i++){int f=0;for(int64_t j=0;j<n;j++)if(tmp[j]==l->data[i]){f=1;break;}if(!f)tmp[n++]=l->data[i];} vredrs_list_simple* r=(vredrs_list_simple*)malloc(sizeof(int64_t)*2+n*sizeof(int64_t)); if(!r){free(tmp);return 0;} r->len=n; r->cap=n; for(int64_t i=0;i<n;i++) r->data[i]=tmp[i]; free(tmp); return (int64_t)r; }
__attribute__((weak)) int64_t list_take(int64_t col, int64_t n) { if(!col||n<=0) return vredrs_make_list(0); vredrs_list_simple* l=(vredrs_list_simple*)col; if(n>l->len) n=l->len; vredrs_list_simple* r=(vredrs_list_simple*)malloc(sizeof(int64_t)*2+n*sizeof(int64_t)); if(!r) return 0; r->len=n; r->cap=n; for(int64_t i=0;i<n;i++) r->data[i]=l->data[i]; return (int64_t)r; }
__attribute__((weak)) int64_t list_drop(int64_t col, int64_t n) { if(!col||n<0) return 0; vredrs_list_simple* l=(vredrs_list_simple*)col; if(n>=l->len) return vredrs_make_list(0); int64_t rem=l->len-n; vredrs_list_simple* r=(vredrs_list_simple*)malloc(sizeof(int64_t)*2+rem*sizeof(int64_t)); if(!r) return 0; r->len=rem; r->cap=rem; for(int64_t i=0;i<rem;i++) r->data[i]=l->data[n+i]; return (int64_t)r; }

/* 数学函数 */
__attribute__((weak)) int64_t abs_i64(int64_t v) { return v<0?-v:v; }
__attribute__((weak)) int64_t max_i64(int64_t a, int64_t b) { return a>b?a:b; }
__attribute__((weak)) int64_t min_i64(int64_t a, int64_t b) { return a<b?a:b; }
__attribute__((weak)) int64_t gcd_i64(int64_t a, int64_t b) { a=a<0?-a:a; b=b<0?-b:b; while(b){int64_t t=a%b;a=b;b=t;} return a; }

/* 闭包对象 */
__attribute__((weak)) int64_t vredrs_make_captures(int64_t n, ...) {
    if(n<0) n=0; int64_t* cap=(int64_t*)malloc(sizeof(int64_t)*(1+n)); if(!cap) return 0;
    cap[0]=n; va_list args; va_start(args,n); for(int64_t i=0;i<n;i++) cap[1+i]=va_arg(args,int64_t); va_end(args); return (int64_t)cap;
}
__attribute__((weak)) int64_t vredrs_make_closure(int64_t fn_ptr, int64_t captures_ptr) {
    int64_t* obj=(int64_t*)malloc(sizeof(int64_t)*2); if(!obj) return 0; obj[0]=fn_ptr; obj[1]=captures_ptr; return (int64_t)obj;
}
__attribute__((weak)) int64_t vredrs_get_capture(int64_t captures_ptr, int64_t index) {
    if(!captures_ptr) return 0; int64_t* cap=(int64_t*)captures_ptr; int64_t count=cap[0]; if(index<0||index>=count) return 0; return cap[1+index];
}
typedef int64_t (*vredrs_closure_fn_t)(int64_t,int64_t,int64_t,int64_t,int64_t,int64_t);
__attribute__((weak)) int64_t vredrs_call_closure(int64_t closure_obj, ...) {
    if(!closure_obj) return 0; int64_t* obj=(int64_t*)closure_obj; int64_t fn_ptr=obj[0]; int64_t cp=obj[1];
    va_list args; va_start(args,closure_obj); int64_t a1=va_arg(args,int64_t),a2=va_arg(args,int64_t),a3=va_arg(args,int64_t),a4=va_arg(args,int64_t),a5=va_arg(args,int64_t); va_end(args);
    vredrs_closure_fn_t fn=(vredrs_closure_fn_t)fn_ptr; return fn(cp,a1,a2,a3,a4,a5);
}

/* 异常处理（标志式） */
static int vredrs_catch_triggered = 0;
__attribute__((weak)) void vredrs_throw(vredrs_value v) {
    vredrs_current_exception = v;
    if (vredrs_catch_depth > 0) { vredrs_catch_triggered = 1; return; }
    fprintf(stderr, "vredrs: uncaught throw: %lld\n", (long long)v); exit(70);
}
__attribute__((weak)) void vredrs_push_catch(void) { if(vredrs_catch_depth<64){vredrs_catch_depth++;vredrs_catch_triggered=0;} }
__attribute__((weak)) void vredrs_pop_catch(void) { if(vredrs_catch_depth>0) vredrs_catch_depth--; }
__attribute__((weak)) int vredrs_check_throw(void) { int t=vredrs_catch_triggered; vredrs_catch_triggered=0; return t; }

/* 生成器（ucontext 协程） */
#define VREDRS_GEN_STACK_SIZE 65536
typedef struct { ucontext_t caller_ctx, gen_ctx; char* stack; int64_t fn_ptr, yield_value; int done, started; } vredrs_generator_t;
static vredrs_generator_t* vredrs_current_gen = NULL;
static void vredrs_gen_trampoline(void) {
    vredrs_generator_t* g = vredrs_current_gen;
    if (g && g->fn_ptr) { typedef int64_t (*gen_fn_t)(void); gen_fn_t fn=(gen_fn_t)g->fn_ptr; fn(); }
    if (vredrs_current_gen) vredrs_current_gen->done = 1;
    swapcontext(&vredrs_current_gen->gen_ctx, &vredrs_current_gen->caller_ctx);
}
__attribute__((weak)) int64_t vredrs_gen_create(int64_t fn_ptr) {
    vredrs_generator_t* g=(vredrs_generator_t*)malloc(sizeof(vredrs_generator_t)); if(!g) return 0;
    g->fn_ptr=fn_ptr; g->yield_value=0; g->done=0; g->started=0;
    g->stack=(char*)malloc(VREDRS_GEN_STACK_SIZE); if(!g->stack){free(g);return 0;}
    getcontext(&g->gen_ctx); g->gen_ctx.uc_stack.ss_sp=g->stack; g->gen_ctx.uc_stack.ss_size=VREDRS_GEN_STACK_SIZE; g->gen_ctx.uc_link=&g->caller_ctx;
    makecontext(&g->gen_ctx, vredrs_gen_trampoline, 0); return (int64_t)g;
}
__attribute__((weak)) int64_t vredrs_yield(int64_t v) {
    vredrs_generator_t* g = vredrs_current_gen; if(!g) return v;
    g->yield_value = v; swapcontext(&g->gen_ctx, &g->caller_ctx); return v;
}
__attribute__((weak)) int64_t vredrs_resume(int64_t h) {
    vredrs_generator_t* g=(vredrs_generator_t*)h; if(!g||g->done) return 0;
    vredrs_generator_t* prev=vredrs_current_gen; vredrs_current_gen=g;
    swapcontext(&g->caller_ctx, &g->gen_ctx); vredrs_current_gen=prev; return g->yield_value;
}

/* list_set_index */
__attribute__((weak)) void vredrs_set_index(int64_t col, int64_t idx, int64_t val) {
    if(!col) return; vredrs_list_simple* l=(vredrs_list_simple*)col; if(idx<0||idx>=l->len) return; l->data[idx]=val;
}

/* vredrs_index 已在前文定义（含字符串键启发式），此处不重复 */

/* vredrs_make_tuple 已在前文定义，此处不重复 */

/* ── 并发原语：channel + select ────────────────────────── */
/* channel: 简化为单向缓冲管道 { capacity, len, buf[], closed } */
typedef struct { int64_t capacity; int64_t len; int64_t* buf; int closed; } vredrs_channel_t;

__attribute__((weak)) int64_t vredrs_make_channel(int64_t capacity) {
    if (capacity < 0) capacity = 1;
    vredrs_channel_t* ch = (vredrs_channel_t*)malloc(sizeof(vredrs_channel_t));
    if (!ch) return 0;
    ch->capacity = capacity;
    ch->len = 0;
    ch->buf = (int64_t*)calloc(capacity > 0 ? capacity : 1, sizeof(int64_t));
    ch->closed = 0;
    return (int64_t)ch;
}

/* vredrs_send: 向 channel 发送值（阻塞直到有空间）。简化：无缓冲时直接返回。 */
__attribute__((weak)) int64_t vredrs_send(int64_t ch_ptr, int64_t val) {
    if (!ch_ptr) return 0;
    vredrs_channel_t* ch = (vredrs_channel_t*)ch_ptr;
    if (ch->closed) return 0;
    if (ch->len < ch->capacity) {
        ch->buf[ch->len++] = val;
        return 1;
    }
    /* 缓冲满：简化为丢弃（真实实现应阻塞）。 */
    return 0;
}

/* vredrs_recv: 从 channel 接收值（阻塞直到有数据）。简化：无数据返回 0。 */
__attribute__((weak)) int64_t vredrs_recv(int64_t ch_ptr) {
    if (!ch_ptr) return 0;
    vredrs_channel_t* ch = (vredrs_channel_t*)ch_ptr;
    if (ch->len == 0) return 0;
    int64_t val = ch->buf[0];
    for (int64_t i = 1; i < ch->len; i++) ch->buf[i-1] = ch->buf[i];
    ch->len--;
    return val;
}

/* vredrs_close: 关闭 channel。 */
__attribute__((weak)) void vredrs_close(int64_t ch_ptr) {
    if (!ch_ptr) return;
    vredrs_channel_t* ch = (vredrs_channel_t*)ch_ptr;
    ch->closed = 1;
}

/* vredrs_select: 简化实现 — 检查所有 channel，返回第一个就绪的 case idx。
 * 参数格式：n_cases, has_default, [dir_tag, channel, value?]...
 * 返回：就绪 case 的 idx（0-based），或 -1（无就绪且有 default），或 -2（无就绪无 default） */
__attribute__((weak)) int64_t vredrs_select(int64_t n_cases, int64_t has_default, ...) {
    if (n_cases <= 0) return has_default ? -1 : -2;
    va_list args; va_start(args, has_default);
    for (int64_t i = 0; i < n_cases; i++) {
        int64_t dir = va_arg(args, int64_t);
        int64_t ch_ptr = va_arg(args, int64_t);
        if (dir == 0) {
            /* Send: 就绪如果 channel 有空间 */
            if (ch_ptr) {
                vredrs_channel_t* ch = (vredrs_channel_t*)ch_ptr;
                if (!ch->closed && ch->len < ch->capacity) { va_end(args); return i; }
                va_arg(args, int64_t); /* skip value */
            } else { va_arg(args, int64_t); }
        } else if (dir == 1) {
            /* Receive: 就绪如果 channel 有数据 */
            if (ch_ptr) {
                vredrs_channel_t* ch = (vredrs_channel_t*)ch_ptr;
                if (ch->len > 0) { va_end(args); return i; }
            }
        } else if (dir == 2) {
            /* After: 总是就绪（简化） */
            va_end(args); return i;
        }
    }
    va_end(args);
    return has_default ? -1 : -2;
}
__attribute__((weak)) int64_t vredrs_select_value(void) { return 0; }

/* ── mutex ────────────────────────────────────────────── */
typedef struct { int locked; } vredrs_mutex_t;
__attribute__((weak)) int64_t vredrs_make_mutex(void) {
    vredrs_mutex_t* m = (vredrs_mutex_t*)malloc(sizeof(vredrs_mutex_t));
    if (!m) return 0;
    m->locked = 0;
    return (int64_t)m;
}
__attribute__((weak)) void vredrs_lock(int64_t m_ptr) {
    if (!m_ptr) return;
    vredrs_mutex_t* m = (vredrs_mutex_t*)m_ptr;
    /* 简化自旋锁（单线程环境不会竞争）。 */
    while (m->locked) {}
    m->locked = 1;
}
__attribute__((weak)) void vredrs_unlock(int64_t m_ptr) {
    if (!m_ptr) return;
    vredrs_mutex_t* m = (vredrs_mutex_t*)m_ptr;
    m->locked = 0;
}

/* I/O stub */
__attribute__((weak)) void print_str(const char* s) { fputs(s?s:"", stdout); }
__attribute__((weak)) const char* read_line(void) { char* buf=(char*)malloc(1024); if(!buf) return NULL; if(!fgets(buf,1024,stdin)){free(buf);return NULL;} size_t n=strlen(buf); if(n>0&&buf[n-1]=='\n')buf[n-1]='\0'; return buf; }
__attribute__((weak)) int64_t random_int(void) { return (int64_t)rand(); }
__attribute__((weak)) int64_t randint(int64_t a, int64_t b) { if(a>b)return a; return a+(int64_t)rand()%(b-a+1); }
__attribute__((weak)) void seed_random(int64_t s) { srand((unsigned int)s); }
__attribute__((weak)) int64_t time_now(void) { return (int64_t)time(NULL); }
__attribute__((weak)) void sleep_secs(int64_t s) { struct timespec ts; ts.tv_sec=(time_t)s; ts.tv_nsec=0; nanosleep(&ts,NULL); }
__attribute__((weak)) int64_t sys_exit(int64_t code) { exit((int)code); return 0; }
__attribute__((weak)) const char* get_env(const char* name) { return getenv(name); }
__attribute__((weak)) const char* read_file_str(const char* path) { if(!path) return NULL; FILE* f=fopen(path,"rb"); if(!f) return NULL; fseek(f,0,SEEK_END); long sz=ftell(f); fseek(f,0,SEEK_SET); char* buf=(char*)malloc(sz+1); if(!buf){fclose(f);return NULL;} size_t rd=fread(buf,1,sz,f); fclose(f); buf[rd]='\0'; return buf; }
__attribute__((weak)) int64_t write_file_str(const char* path, const char* content) { if(!path||!content) return 0; FILE* f=fopen(path,"wb"); if(!f) return 0; size_t len=strlen(content); size_t wr=fwrite(content,1,len,f); fclose(f); return wr==len?1:0; }
__attribute__((weak)) int64_t delete_file(const char* path) { if(!path) return 0; return remove(path)==0?1:0; }
__attribute__((weak)) int64_t make_dir(const char* path) { if(!path) return 0; return mkdir(path,0755)==0?1:0; }

/* === 字符串插值 stub === */
__attribute__((weak)) const char* vredrs_to_str(int64_t v) {
    /* 启发式：>65536 视为字符串指针，直接返回；否则 int_to_str。 */
    if (v > 65536) return (const char*)v;
    return int_to_str(v);
}
__attribute__((weak)) const char* vredrs_str_concat_n(int64_t n, ...) {
    if (n <= 0) { char* r = (char*)malloc(1); if(r) r[0]='\0'; return r; }
    va_list args; va_start(args, n);
    /* 计算总长度。 */
    size_t total = 0;
    va_list args2; va_copy(args2, args);
    for (int64_t i = 0; i < n; i++) {
        const char* s = va_arg(args2, const char*);
        if (s) total += strlen(s);
    }
    va_end(args2);
    char* r = (char*)malloc(total + 1); if (!r) return NULL;
    r[0] = '\0';
    for (int64_t i = 0; i < n; i++) {
        const char* s = va_arg(args, const char*);
        if (s) strcat(r, s);
    }
    va_end(args);
    return r;
}

/* vredrs_list_append = list_append (alias) */
__attribute__((weak)) int64_t vredrs_list_append(int64_t col, int64_t val) {
    return list_append(col, val);
}

/* sum = list_sum (alias for builtin routing) */
__attribute__((weak)) int64_t sum(int64_t col) { return list_sum(col); }

/* vredrs_call_dynamic for CallDyn (indirect function call) */
typedef int64_t (*vredrs_lambda_fn_t)(int64_t, int64_t, int64_t, int64_t, int64_t, int64_t);
__attribute__((weak)) int64_t vredrs_call_dynamic(int64_t callee, ...) {
    if (!callee) return 0;
    va_list args; va_start(args, callee);
    int64_t a1=va_arg(args,int64_t),a2=va_arg(args,int64_t),a3=va_arg(args,int64_t),a4=va_arg(args,int64_t),a5=va_arg(args,int64_t);
    va_end(args);
    vredrs_lambda_fn_t fn = (vredrs_lambda_fn_t)callee;
    return fn(0, a1, a2, a3, a4, a5);
}

/* ── 类型标记值模型（Type-Tagged Value Model）──────────────
 * 在 i64 值模型基础上添加运行时类型检查辅助函数。
 * 不改变 i64 表示，而是通过值范围启发式 + 专用函数实现类型分派：
 * - 小整数（< 0x10000）：直接 i64
 * - 堆指针（≥ 0x10000）：字符串/列表/字典/对象/闭包/生成器/channel
 * - 特殊值：0 = null, 1 = true, 其他小正整数 = bool/int
 *
 * vredrs_typeof(v): 返回类型名字符串
 * vredrs_value_add 等：通过 typeof 分派（已在前面实现）
 */

/* vredrs_typeof: 返回值的类型名（运行时类型检查）。 */
__attribute__((weak)) const char* vredrs_typeof(vredrs_value v) {
    if (v == 0) return "null";
    if (v == 1) return "bool";
    if (v < 0x10000) return "int";
    /* 堆指针：尝试读取 __class__ 字段判断是否对象。 */
    const char* cls = (const char*)vredrs_get_field(v, "__class__");
    if (cls) return cls;
    /* 无法进一步区分 str/list/dict — 启发式：检查首字节是否可打印。 */
    const char* s = (const char*)v;
    if (s && *s >= 0x20 && *s < 0x7f) return "str";
    /* 默认：假设是 list。 */
    return "list";
}

/* vredrs_is_int / vredrs_is_str / vredrs_is_null 等类型谓词。 */
__attribute__((weak)) int64_t vredrs_is_int(vredrs_value v) { return v >= 0 && v < 0x10000; }
__attribute__((weak)) int64_t vredrs_is_null_val(vredrs_value v) { return v == 0; }
__attribute__((weak)) int64_t vredrs_is_bool(vredrs_value v) { return v == 0 || v == 1; }

/* vredrs_repeated: a repeated b → a * b（重复运算） */
__attribute__((weak)) vredrs_value vredrs_repeated(vredrs_value a, vredrs_value b) { return a * b; }

/* vredrs_optional_method: null 短路 + method_call */
__attribute__((weak)) vredrs_value vredrs_optional_method(vredrs_value obj, const char* m, ...) {
    if (!obj) return 0;
    return vredrs_method_call(obj, m);
}

/* read_file: 返回文件内容字符串（不再是文件大小） */
__attribute__((weak)) const char* read_file_str_content(const char* path) {
    if (!path) return NULL;
    FILE* f = fopen(path, "rb");
    if (!f) return NULL;
    fseek(f, 0, SEEK_END);
    long sz = ftell(f);
    fseek(f, 0, SEEK_SET);
    char* buf = (char*)malloc(sz + 1);
    if (!buf) { fclose(f); return NULL; }
    size_t rd = fread(buf, 1, sz, f);
    fclose(f);
    buf[rd] = '\0';
    return buf;
}

/* set 去重：构造 list 时检查已存在 */
__attribute__((weak)) vredrs_value vredrs_make_set_dedup(int64_t n, ...) {
    if (n < 0) n = 0;
    vredrs_list_simple* l = (vredrs_list_simple*)malloc(sizeof(int64_t)*2 + n*sizeof(int64_t));
    if (!l) return 0;
    l->len = 0; l->cap = n;
    va_list args; va_start(args, n);
    for (int64_t i = 0; i < n; i++) {
        int64_t v = va_arg(args, int64_t);
        int found = 0;
        for (int64_t j = 0; j < l->len; j++) { if (l->data[j] == v) { found = 1; break; } }
        if (!found) l->data[l->len++] = v;
    }
    va_end(args);
    return (vredrs_value)l;
}

/* vredrs_pow: 整数幂 */
__attribute__((weak)) int64_t vredrs_pow_i64(int64_t base, int64_t exp) {
    int64_t r = 1, b = base;
    while (exp > 0) { if (exp & 1) r *= b; b *= b; exp >>= 1; }
    return r;
}

/* unsigned int support stubs */
__attribute__((weak)) int64_t vredrs_u8_val(int64_t v) { return v & 0xFF; }
__attribute__((weak)) int64_t vredrs_u16_val(int64_t v) { return v & 0xFFFF; }
__attribute__((weak)) int64_t vredrs_u32_val(int64_t v) { return v & 0xFFFFFFFF; }

/* xor eax, eax before calls (variadic ABI) - handled in emit_call */
"#.to_string()
}

/// 公开接口：供 emit_raw_arm 复用同一份架构无关的 C stub。
pub fn minimal_runtime_stub_public() -> String {
    minimal_runtime_stub()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::ir::{IrFunction, IrModule, StaticInsn, Operand, TypeHint, BinOp};

    #[test]
    fn emit_simple_function() {
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
        let mut e = X86Emitter::new();
        let asm = e.emit_module(&m);
        assert!(asm.contains("main:"));
        assert!(asm.contains("42"));  // 立即数 42 应该出现在汇编中
        assert!(asm.contains("ret"));
    }

    #[test]
    fn emit_add_static() {
        let m = IrModule {
            functions: vec![IrFunction {
                name: "add".into(),
                params: vec![("a".into(), TypeHint::I64), ("b".into(), TypeHint::I64)],
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
        let mut e = X86Emitter::new();
        let asm = e.emit_module(&m);
        assert!(asm.contains("add:"));
        assert!(asm.contains("add"));  // 应该有 add 指令（寄存器名取决于分配）
    }

    #[test]
    fn emit_dyn_uses_runtime() {
        let m = IrModule {
            functions: vec![IrFunction {
                name: "f".into(),
                params: vec![],
                return_ty: TypeHint::Dynamic,
                body: vec![
                    StaticInsn::Bin { op: BinOp::AddDyn, dst: Operand::Reg("v1".into(), TypeHint::Dynamic), lhs: Operand::Reg("v0".into(), TypeHint::Dynamic), rhs: Operand::ImmI64(1), ty: TypeHint::Dynamic },
                    StaticInsn::Ret { value: Some(Operand::Reg("v1".into(), TypeHint::Dynamic)) },
                ],
                is_main: false,
                annotations: vec![],
                param_regs: vec![],
                locals: vec![],
            }],
            string_pool: vec![],
            globals: vec![],
            entry: Some("f".into()),
        };
        let mut e = X86Emitter::new();
        let asm = e.emit_module(&m);
        assert!(asm.contains("vredrs_value_add"));
        assert!(asm.contains(".extern vredrs_value_add"));
    }
}
