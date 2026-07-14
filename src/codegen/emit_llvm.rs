//! # emit_llvm — Static IR → LLVM IR 发射器（渐进式）
//!
//! 手册 Phase 3 的核心。消费 [`crate::codegen::ir::IrModule`]，生成 LLVM IR
//! 文本（`.ll`）。
//!
//! ## 渐进式语义（手册 Phase 3 修正版）
//!
//! - **静态路径**（有类型注解）：用 `i64`/`double`/`i1` 原生类型，不出现
//!   `vredrs_value`。手册验收："有类型注解的代码在 LLVM 下不生成
//!   `vredrs_value` 相关调用"。
//! - **动态路径**（无注解）：用 `i64`（`vredrs_value` 的 payload 位宽）+
//!   `i8` tag 传递，调用 `vredrs_value_*` runtime 函数。手册验收："无类型
//!   注解的代码在 LLVM 下生成的二进制与 VM 行为完全一致"。
//! - **混合**：同一函数内静态与动态可共存，边界处自动 `Box`/`Unbox`。
//!
//! ## 输出
//!
//! 生成可被 `llc`/`clang` 进一步编译的 LLVM IR 文本。不直接生成可执行文件
//! （那是 `compile_to_llvm_ir` 的职责，链接 vredrs_runtime.c）。

use crate::codegen::ir::{BinOp, Cond, IrFunction, IrModule, Operand, StaticInsn, TypeHint};
use std::collections::HashMap;

/// IR → LLVM IR 发射器。
pub struct LlvmEmitter {
    out: String,
    /// 当前函数内：虚拟寄存器名 → LLVM SSA 值名（如 %v0 → %3）。
    ssa_map: HashMap<String, String>,
    /// SSA 计数器。
    ssa_counter: usize,
    /// 字符串全局变量名。
    str_globals: Vec<String>,
    /// 已声明的外部函数。
    declared_externs: Vec<String>,
}

impl LlvmEmitter {
    pub fn new() -> Self {
        LlvmEmitter {
            out: String::new(),
            ssa_map: HashMap::new(),
            ssa_counter: 0,
            str_globals: Vec::new(),
            declared_externs: Vec::new(),
        }
    }

    /// 发射整个 IR 模块为 LLVM IR 文本。
    pub fn emit_module(&mut self, m: &IrModule) -> String {
        self.out.clear();
        self.out.push_str("; Vredrs LLVM IR (gradual: static + dynamic)\n");
        // 字符串池：全局常量。
        for (i, s) in m.string_pool.iter().enumerate() {
            let name = format!("@.str{}", i);
            self.str_globals.push(name.clone());
            self.out.push_str(&format!(
                "{} = private unnamed_addr constant [{} x i8] c\"{}\\00\"\n",
                name,
                s.len() + 1,
                escape_llvm_str(s)
            ));
        }
        // 收集所有 extern 声明。
        let mut exts: Vec<String> = Vec::new();
        for f in &m.functions {
            for i in &f.body {
                collect_externs(i, &mut exts);
            }
        }
        exts.sort();
        exts.dedup();
        self.declared_externs = exts.clone();
        for e in &exts {
            self.out.push_str(&format!("declare {} @{}({})\n", extern_return_ty(e), e, extern_param_sig(e)));
        }
        // vredrs_value 类型声明（动态路径用）。
        if exts.iter().any(|e| e.starts_with("vredrs_value_")) {
            self.out.push_str("%vredrs_value = type { i8, i64 }\n");
        }
        // 每个函数。
        for f in &m.functions {
            self.emit_function(f);
        }
        self.out.clone()
    }

    fn emit_function(&mut self, f: &IrFunction) {
        self.ssa_map.clear();
        self.ssa_counter = 0;
        // 返回类型。
        let ret_ty = llvm_type(f.return_ty);
        // 参数类型（用虚拟寄存器名作为 LLVM 参数名，保证与 IR 指令一致）。
        let params: Vec<String> = f
            .param_regs
            .iter()
            .zip(f.params.iter())
            .map(|(reg, (_, t))| format!("{} %{}", llvm_type(*t), sanitize_name(reg)))
            .collect();
        self.out.push_str(&format!("\ndefine {} @{}({}) {{\n", ret_ty, f.name, params.join(", ")));
        self.out.push_str("entry:\n");
        // 收集所有用到的虚拟寄存器名。
        let mut all_regs: Vec<String> = Vec::new();
        for insn in &f.body {
            collect_regs(insn, &mut |r| {
                if !all_regs.iter().any(|x| x == r) {
                    all_regs.push(r.to_string());
                }
            });
        }
        // 为每个虚拟寄存器分配 alloca 栈槽。
        for reg in &all_regs {
            let slot = self.fresh_ssa();
            // 类型：若是参数，用参数类型；否则默认 i64。
            let ty = f.param_regs.iter().position(|r| r == reg)
                .map(|i| f.params[i].1)
                .unwrap_or(TypeHint::I64);
            self.out.push_str(&format!("  {} = alloca {}, align 8\n", slot, llvm_type(ty)));
            self.ssa_map.insert(reg.clone(), slot);
        }
        // 把参数从 LLVM 参数寄存器存到栈槽。
        for (i, reg) in f.param_regs.iter().enumerate() {
            if let Some(slot) = self.ssa_map.get(reg).cloned() {
                let ty = f.params[i].1;
                self.out.push_str(&format!("  store {} %{}, {}* {}\n", llvm_type(ty), sanitize_name(reg), llvm_type(ty), slot));
            }
        }
        // 发射函数体。
        for insn in &f.body {
            self.emit_insn(insn);
        }
        // 确保有终结指令。
        if !self.out.trim_end().ends_with("ret") && !self.ends_with_terminator() {
            self.out.push_str(&format!("  ret {}\n", default_value(f.return_ty)));
        }
        self.out.push_str("}\n");
    }

    fn ends_with_terminator(&self) -> bool {
        let trimmed = self.out.trim_end();
        trimmed.ends_with("ret") || trimmed.ends_with("br ")
            || trimmed.ends_with("unreachable")
            || trimmed.ends_with("switch")
    }

    fn fresh_ssa(&mut self) -> String {
        let n = format!("%{}", self.ssa_counter);
        self.ssa_counter += 1;
        n
    }

    fn emit_insn(&mut self, insn: &StaticInsn) {
        use StaticInsn::*;
        match insn {
            Label(l) => {
                // LLVM 标签：以 ; 分隔的基本块。
                self.out.push_str(&format!("; label {}\n", sanitize_name(l)));
                self.out.push_str(&format!("{}:\n", sanitize_label(l)));
            }
            Comment(c) => {
                self.out.push_str(&format!("  ; {}\n", c));
            }
            SrcLoc { line, col } => {
                self.out.push_str(&format!("  ; srcloc {}:{}\n", line, col));
            }
            Mov { dst, src, ty } => {
                let val = self.load_operand(src);
                self.store_operand(dst, &val, *ty);
            }
            Box { dst, src, from } => {
                let func = match from {
                    TypeHint::I64 => "vredrs_value_make_i64",
                    TypeHint::F64 => "vredrs_value_make_f64",
                    TypeHint::Bool => "vredrs_value_make_bool",
                    _ => "",
                };
                if !func.is_empty() {
                    let v = self.load_operand(src);
                    let r = self.fresh_ssa();
                    self.out.push_str(&format!("  {} = call %vredrs_value @{}({} {})\n", r, func, llvm_type(*from), v));
                    self.store_operand(dst, &r, TypeHint::Dynamic);
                } else {
                    let v = self.load_operand(src);
                    self.store_operand(dst, &v, *from);
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
                    let v = self.load_operand(src);
                    let r = self.fresh_ssa();
                    self.out.push_str(&format!("  {} = call {} @{}(%vredrs_value {})\n", r, llvm_type(*to), func, v));
                    self.store_operand(dst, &r, *to);
                } else {
                    let v = self.load_operand(src);
                    self.store_operand(dst, &v, *to);
                }
            }
            Bin { op, dst, lhs, rhs, ty } => self.emit_bin(*op, dst, lhs, rhs, *ty),
            Neg { dst, src, ty } => {
                let v = self.load_operand(src);
                let r = self.fresh_ssa();
                if *ty == TypeHint::F64 {
                    self.out.push_str(&format!("  {} = fneg double {}\n", r, v));
                } else {
                    self.out.push_str(&format!("  {} = sub i64 0, {}\n", r, v));
                }
                self.store_operand(dst, &r, *ty);
            }
            Not { dst, src, ty } => {
                let v = self.load_operand(src);
                let r = self.fresh_ssa();
                self.out.push_str(&format!("  {} = xor i1 {}, true\n", r, v));
                self.store_operand(dst, &r, TypeHint::Bool);
                let _ = ty;
            }
            Cmp { dst, cond, lhs, rhs, ty } => {
                let l = self.load_operand(lhs);
                let r = self.load_operand(rhs);
                let cmp = self.fresh_ssa();
                let pred = cond_to_llvm_pred(*cond, *ty);
                self.out.push_str(&format!("  {} = icmp {} i64 {}, {}\n", cmp, pred, l, r));
                let ext = self.fresh_ssa();
                self.out.push_str(&format!("  {} = zext i1 {} to i64\n", ext, cmp));
                self.store_operand(dst, &ext, TypeHint::Bool);
            }
            Branch { cond, lhs, rhs, target } => {
                let l = self.load_operand(lhs);
                let r = self.load_operand(rhs);
                let cmp = self.fresh_ssa();
                self.out.push_str(&format!("  {} = icmp {} i64 {}, {}\n", cmp, cond_to_llvm_pred(*cond, TypeHint::I64), l, r));
                self.out.push_str(&format!("  br i1 {}, label %{}\n", cmp, sanitize_label(target)));
            }
            BranchTrue { cond, target } => {
                let v = self.load_operand(cond);
                let cmp = self.fresh_ssa();
                self.out.push_str(&format!("  {} = icmp ne i64 {}, 0\n", cmp, v));
                self.out.push_str(&format!("  br i1 {}, label %{}\n", cmp, sanitize_label(target)));
            }
            Jump { target } => {
                self.out.push_str(&format!("  br label %{}\n", sanitize_label(target)));
            }
            Load { dst, addr, size, ty } => {
                let a = self.load_operand(addr);
                let r = self.fresh_ssa();
                let llvm_ty = match size {
                    1 => "i8",
                    2 => "i16",
                    4 => "i32",
                    _ => "i64",
                };
                self.out.push_str(&format!("  {} = load {}, {}* {}\n", r, llvm_type(*ty), llvm_ty, a));
                self.store_operand(dst, &r, *ty);
            }
            Store { addr, value, size, ty } => {
                let a = self.load_operand(addr);
                let v = self.load_operand(value);
                let llvm_ty = match size {
                    1 => "i8",
                    2 => "i16",
                    4 => "i32",
                    _ => "i64",
                };
                self.out.push_str(&format!("  store {} {}, {}* {}\n", llvm_type(*ty), v, llvm_ty, a));
            }
            Alloca { dst, size, align } => {
                let s = self.load_operand(size);
                let r = self.fresh_ssa();
                self.out.push_str(&format!("  {} = alloca i8, i64 {}\n", r, s));
                if *align > 0 {
                    // LLVM alloca 对齐通过属性。
                }
                self.store_operand(dst, &r, TypeHint::Ptr);
            }
            StackFree { ptr, size } => {
                // LLVM 栈释放：插入 lifetime.end 标记（帮助优化器回收栈槽）。
                let p = self.load_operand(ptr);
                self.out.push_str(&format!("  call void @llvm.lifetime.end.p0i8(i64 -1, i8* {})\n", p));
                let _ = size;
            }
            Call { dst, func, args, ty } => {
                let loaded: Vec<String> = args.iter().map(|a| self.load_operand(a)).collect();
                let arg_tys: Vec<&str> = args.iter().map(|a| llvm_type(a.ty())).collect();
                let call_args: Vec<String> = loaded.iter().zip(arg_tys.iter()).map(|(v, t)| format!("{} {}", t, v)).collect();
                if let Some(d) = dst {
                    let r = self.fresh_ssa();
                    self.out.push_str(&format!("  {} = call {} @{}({})\n", r, llvm_type(*ty), func, call_args.join(", ")));
                    self.store_operand(d, &r, *ty);
                } else {
                    self.out.push_str(&format!("  call void @{}({})\n", func, call_args.join(", ")));
                }
            }
            CallDyn { dst, callee, args, ty } => {
                let c = self.load_operand(callee);
                let mut loaded = vec![c];
                for a in args {
                    loaded.push(self.load_operand(a));
                }
                if let Some(d) = dst {
                    let r = self.fresh_ssa();
                    self.out.push_str(&format!("  {} = call {} @vredrs_call_dynamic(i64 {}", r, llvm_type(*ty), loaded[0]));
                    for v in &loaded[1..] {
                        self.out.push_str(&format!(", i64 {}", v));
                    }
                    self.out.push_str(")\n");
                    self.store_operand(d, &r, *ty);
                } else {
                    self.out.push_str(&format!("  call void @vredrs_call_dynamic(i64 {}", loaded[0]));
                    for v in &loaded[1..] {
                        self.out.push_str(&format!(", i64 {}", v));
                    }
                    self.out.push_str(")\n");
                }
            }
            RuntimeCall { dst, func, args } => {
                let loaded: Vec<String> = args.iter().map(|a| self.load_operand(a)).collect();
                let arg_tys: Vec<&str> = args.iter().map(|a| llvm_type(a.ty())).collect();
                let call_args: Vec<String> = loaded.iter().zip(arg_tys.iter()).map(|(v, t)| format!("{} {}", t, v)).collect();
                if let Some(d) = dst {
                    let r = self.fresh_ssa();
                    self.out.push_str(&format!("  {} = call {} @{}({})\n", r, "i64", func, call_args.join(", ")));
                    self.store_operand(d, &r, TypeHint::Dynamic);
                } else {
                    self.out.push_str(&format!("  call void @{}({})\n", func, call_args.join(", ")));
                }
            }
            Ret { value } => {
                match value {
                    Some(v) => {
                        let val = self.load_operand(v);
                        self.out.push_str(&format!("  ret {} {}\n", llvm_type(v.ty()), val));
                    }
                    None => {
                        self.out.push_str("  ret void\n");
                    }
                }
            }
            Push { src } => {
                let v = self.load_operand(src);
                self.out.push_str(&format!("  ; push {}\n", v));
            }
            Pop { dst } => {
                let r = self.fresh_ssa();
                self.out.push_str(&format!("  ; pop -> {}\n", r));
                self.store_operand(dst, &r, TypeHint::Dynamic);
            }
            Asm { template, inputs, outputs, .. } => {
                // LLVM 内联汇编：call void asm sideeffect "..." : ... : ...
                let mut constraint_str = String::new();
                for (i, o) in outputs.iter().enumerate() {
                    if i > 0 {
                        constraint_str.push(',');
                    }
                    constraint_str.push_str(&format!("={}", o.constraint));
                }
                if !outputs.is_empty() && !inputs.is_empty() {
                    constraint_str.push(',');
                }
                for (i, inp) in inputs.iter().enumerate() {
                    if i > 0 {
                        constraint_str.push(',');
                    }
                    constraint_str.push_str(&format!("{}", inp.constraint));
                }
                let loaded: Vec<String> = inputs.iter().map(|a| self.load_operand(&a.operand)).collect();
                self.out.push_str(&format!(
                    "  call void asm sideeffect \"{}\", \"{}\"({})\n",
                    template.replace("\\", "\\\\").replace("\"", "\\22"),
                    constraint_str,
                    loaded.join(", ")
                ));
            }
            Prefetch { addr, hint } => {
                let a = self.load_operand(addr);
                self.out.push_str(&format!("  call void @llvm.prefetch.p0i8(i8* {}, i32 0, i32 3, i32 1)\n", a));
                let _ = hint;
            }
            PipelineMarker { name } => {
                self.out.push_str(&format!("  ; @pipeline {}\n", name));
            }
            IsrEntry { group, priority } => {
                self.out.push_str(&format!("  ; @isr_group {} prio={}\n", group, priority));
            }
            WcetNote { cycles, budget } => {
                self.out.push_str(&format!("  ; wcet cycles={} budget={}\n", cycles, budget));
            }
            DiffMeta { symbol, offset } => {
                self.out.push_str(&format!("  ; diffmeta {} +{:#x}\n", symbol, offset));
            }
        }
    }

    fn emit_bin(&mut self, op: BinOp, dst: &Operand, lhs: &Operand, rhs: &Operand, ty: TypeHint) {
        // 动态 op → runtime 调用。
        if op.is_dyn() {
            let func = dyn_bin_func(op);
            let l = self.load_operand(lhs);
            let r = self.load_operand(rhs);
            let res = self.fresh_ssa();
            self.out.push_str(&format!("  {} = call %vredrs_value @{}(%vredrs_value {}, %vredrs_value {})\n", res, func, l, r));
            self.store_operand(dst, &res, TypeHint::Dynamic);
            return;
        }
        // 浮点静态。
        if ty == TypeHint::F64 {
            let l = self.load_operand(lhs);
            let r = self.load_operand(rhs);
            let res = self.fresh_ssa();
            let opstr = match op {
                BinOp::Add => "fadd",
                BinOp::Sub => "fsub",
                BinOp::Mul => "fmul",
                BinOp::Div => "fdiv",
                _ => "fadd",
            };
            self.out.push_str(&format!("  {} = {} double {}, {}\n", res, opstr, l, r));
            self.store_operand(dst, &res, TypeHint::F64);
            return;
        }
        // 整数静态。
        let l = self.load_operand(lhs);
        let r = self.load_operand(rhs);
        let res = self.fresh_ssa();
        let opstr = match op {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::Mul => "mul",
            BinOp::Div => "sdiv",
            BinOp::Mod => "srem",
            BinOp::And => "and",
            BinOp::Or => "or",
            BinOp::Xor => "xor",
            BinOp::Shl => "shl",
            BinOp::Shr => "ashr",
            _ => "add",
        };
        self.out.push_str(&format!("  {} = {} i64 {}, {}\n", res, opstr, l, r));
        self.store_operand(dst, &res, TypeHint::I64);
    }

    /// 加载操作数为 LLVM SSA 值字符串。
    fn load_operand(&mut self, op: &Operand) -> String {
        match op {
            Operand::Reg(name, ty) => {
                if let Some(slot) = self.ssa_map.get(name).cloned() {
                    let r = self.fresh_ssa();
                    self.out.push_str(&format!("  {} = load {}, {}* {}\n", r, llvm_type(*ty), llvm_type(*ty), slot));
                    r
                } else {
                    // 未分配栈槽的寄存器：用 0 作为默认值（poison 在某些 LLVM
                    // 版本不可用，改用 0 更安全）。
                    match ty {
                        TypeHint::F64 => "double 0.0".to_string(),
                        _ => "i64 0".to_string(),
                    }
                }
            }
            Operand::ImmI64(v) => format!("{}", v),
            Operand::ImmF64(v) => format!("double 0x{:016x}", f64::to_bits(*v)),
            Operand::ImmStr(idx) => {
                if *idx < self.str_globals.len() {
                    let r = self.fresh_ssa();
                    let g = &self.str_globals[*idx];
                    self.out.push_str(&format!("  {} = getelementptr [{} x i8], [{} x i8]* {}, i32 0, i32 0\n", r, "x", "x", g));
                    r
                } else {
                    "i8* null".to_string()
                }
            }
            Operand::ImmBool(b) => format!("i1 {}", if *b { 1 } else { 0 }),
            Operand::Null => "i64 0".to_string(),
            Operand::Label(l) => format!("i8* blockaddress @main, %{}", sanitize_label(l)),
            Operand::Sym(s) => format!("i8* bitcast ({}* @{} to i8*)", "i64", s),
        }
    }

    /// 把 SSA 值存到操作数（寄存器）的栈槽。
    fn store_operand(&mut self, op: &Operand, val: &str, ty: TypeHint) {
        if let Operand::Reg(name, _) = op {
            if let Some(slot) = self.ssa_map.get(name).cloned() {
                self.out.push_str(&format!("  store {} {}, {}* {}\n", llvm_type(ty), val, llvm_type(ty), slot));
            }
        }
    }
}

// ── 辅助函数 ──────────────────────────────────────────────

fn llvm_type(t: TypeHint) -> &'static str {
    match t {
        TypeHint::I64 | TypeHint::Bool | TypeHint::Ptr | TypeHint::Str => "i64",
        TypeHint::F64 => "double",
        TypeHint::Unit => "void",
        TypeHint::Dynamic | TypeHint::Mixed => "%vredrs_value",
    }
}

fn default_value(t: TypeHint) -> &'static str {
    match t {
        TypeHint::Unit | TypeHint::Dynamic | TypeHint::Mixed => "void",
        _ => "i64 0",
    }
}

fn cond_to_llvm_pred(c: Cond, ty: TypeHint) -> &'static str {
    if ty == TypeHint::F64 {
        match c {
            Cond::Eq => "oeq",
            Cond::Ne => "one",
            Cond::Lt => "olt",
            Cond::Le => "ole",
            Cond::Gt => "ogt",
            Cond::Ge => "oge",
        }
    } else {
        match c {
            Cond::Eq => "eq",
            Cond::Ne => "ne",
            Cond::Lt => "slt",
            Cond::Le => "sle",
            Cond::Gt => "sgt",
            Cond::Ge => "sge",
        }
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
        Neg { ty, .. } if *ty == TypeHint::F64 => {}
        Bin { ty, op, .. } if *ty == TypeHint::F64 && !op.is_dyn() => {}
        _ => {}
    }
}

fn extern_return_ty(name: &str) -> &'static str {
    if name.starts_with("vredrs_value_make") || name.starts_with("vredrs_value_add")
        || name.starts_with("vredrs_value_sub") || name.starts_with("vredrs_value_mul")
        || name.starts_with("vredrs_value_div") || name.starts_with("vredrs_value_mod")
        || name.starts_with("vredrs_value_nil")
    {
        "%vredrs_value"
    } else if name.starts_with("vredrs_value_get") || name.starts_with("vredrs_value_tag")
        || name.starts_with("vredrs_value_eq") || name.starts_with("vredrs_value_ne")
        || name.starts_with("vredrs_value_lt") || name.starts_with("vredrs_value_le")
        || name.starts_with("vredrs_value_gt") || name.starts_with("vredrs_value_ge")
        || name.starts_with("vredrs_len")
    {
        "i64"
    } else if name.contains("print") || name.contains("throw") || name.contains("panic")
        || name.contains("assert") || name.contains("spawn") || name.contains("yield")
        || name.contains("flush") || name.contains("with")
    {
        "void"
    } else {
        "i64"
    }
}

fn extern_param_sig(name: &str) -> String {
    if name.starts_with("vredrs_value_add") || name.starts_with("vredrs_value_sub")
        || name.starts_with("vredrs_value_mul") || name.starts_with("vredrs_value_div")
        || name.starts_with("vredrs_value_mod")
        || name.starts_with("vredrs_value_eq") || name.starts_with("vredrs_value_ne")
        || name.starts_with("vredrs_value_lt") || name.starts_with("vredrs_value_le")
        || name.starts_with("vredrs_value_gt") || name.starts_with("vredrs_value_ge")
    {
        return "%vredrs_value, %vredrs_value".into();
    }
    if name.starts_with("vredrs_value_make_i64") || name.starts_with("vredrs_value_make_f64")
        || name.starts_with("vredrs_value_make_bool")
    {
        return "i64".into();
    }
    if name.starts_with("vredrs_value_get") {
        return "%vredrs_value".into();
    }
    if name == "vredrs_print" || name == "vredrs_println" {
        return "%vredrs_value".into();
    }
    if name == "vredrs_len" || name == "vredrs_index" {
        return "i64, i64".into();
    }
    "i64".into()
}

fn sanitize_name(n: &str) -> String {
    // LLVM 标识符：字母、数字、-、_、.、$。其他替换。
    n.chars().map(|c| {
        if c.is_alphanumeric() || c == '_' || c == '-' || c == '.' {
            c
        } else {
            '_'
        }
    }).collect()
}

fn sanitize_label(l: &str) -> String {
    sanitize_name(l)
}

fn escape_llvm_str(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\22"),
            b'\n' => out.push_str("\\0A"),
            b'\r' => out.push_str("\\0D"),
            b'\t' => out.push_str("\\09"),
            b if b < 0x20 || b >= 0x7f => out.push_str(&format!("\\{:02X}", b)),
            b => out.push(b as char),
        }
    }
    out
}

/// 对外入口：通过新 IR 路径生成 LLVM IR 文本。
pub fn emit_module_string(m: &IrModule) -> String {
    let mut e = LlvmEmitter::new();
    e.emit_module(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::ir::{IrFunction, IrModule, Operand, StaticInsn, TypeHint, BinOp};

    #[test]
    fn emit_static_no_vredrs_value() {
        // 有类型注解的 add(i64, i64) -> i64 不应出现 vredrs_value。
        let m = IrModule {
            functions: vec![IrFunction {
                name: "add".into(),
                params: vec![("a".into(), TypeHint::I64), ("b".into(), TypeHint::I64)],
                param_regs: vec![],
                return_ty: TypeHint::I64,
                body: vec![
                    StaticInsn::Bin { op: BinOp::Add, dst: Operand::Reg("v2".into(), TypeHint::I64), lhs: Operand::Reg("v0".into(), TypeHint::I64), rhs: Operand::Reg("v1".into(), TypeHint::I64), ty: TypeHint::I64 },
                    StaticInsn::Ret { value: Some(Operand::Reg("v2".into(), TypeHint::I64)) },
                ],
                is_main: false,
                annotations: vec![],
                locals: vec![],
            }],
            string_pool: vec![],
            globals: vec![],
            entry: Some("add".into()),
        };
        let ll = emit_module_string(&m);
        assert!(ll.contains("define i64 @add"));
        assert!(ll.contains("add i64"));
        assert!(!ll.contains("vredrs_value"), "static path must not emit vredrs_value: {}", ll);
    }

    #[test]
    fn emit_dynamic_uses_runtime() {
        // 无注解的 fib 应该用 vredrs_value_add。
        let m = IrModule {
            functions: vec![IrFunction {
                name: "fib".into(),
                params: vec![("n".into(), TypeHint::Dynamic)],
                param_regs: vec![],
                return_ty: TypeHint::Dynamic,
                body: vec![
                    StaticInsn::Bin { op: BinOp::AddDyn, dst: Operand::Reg("v1".into(), TypeHint::Dynamic), lhs: Operand::Reg("v0".into(), TypeHint::Dynamic), rhs: Operand::ImmI64(1), ty: TypeHint::Dynamic },
                    StaticInsn::Ret { value: Some(Operand::Reg("v1".into(), TypeHint::Dynamic)) },
                ],
                is_main: false,
                annotations: vec![],
                locals: vec![],
            }],
            string_pool: vec![],
            globals: vec![],
            entry: Some("fib".into()),
        };
        let ll = emit_module_string(&m);
        assert!(ll.contains("declare %vredrs_value @vredrs_value_add"));
        assert!(ll.contains("call %vredrs_value @vredrs_value_add"));
    }

    #[test]
    fn emit_mixed_function() {
        // 同一函数内静态与动态混合。
        let m = IrModule {
            functions: vec![IrFunction {
                name: "f".into(),
                params: vec![],
                param_regs: vec![],
                return_ty: TypeHint::I64,
                body: vec![
                    StaticInsn::Bin { op: BinOp::Add, dst: Operand::Reg("v0".into(), TypeHint::I64), lhs: Operand::ImmI64(1), rhs: Operand::ImmI64(2), ty: TypeHint::I64 },
                    StaticInsn::Box { dst: Operand::Reg("v1".into(), TypeHint::Dynamic), src: Operand::Reg("v0".into(), TypeHint::I64), from: TypeHint::I64 },
                    StaticInsn::Bin { op: BinOp::AddDyn, dst: Operand::Reg("v2".into(), TypeHint::Dynamic), lhs: Operand::Reg("v1".into(), TypeHint::Dynamic), rhs: Operand::ImmI64(1), ty: TypeHint::Dynamic },
                    StaticInsn::Unbox { dst: Operand::Reg("v3".into(), TypeHint::I64), src: Operand::Reg("v2".into(), TypeHint::Dynamic), to: TypeHint::I64 },
                    StaticInsn::Ret { value: Some(Operand::Reg("v3".into(), TypeHint::I64)) },
                ],
                is_main: false,
                annotations: vec![],
                locals: vec![],
            }],
            string_pool: vec![],
            globals: vec![],
            entry: Some("f".into()),
        };
        let ll = emit_module_string(&m);
        assert!(ll.contains("add i64"));
        assert!(ll.contains("call %vredrs_value @vredrs_value_make_i64"));
        assert!(ll.contains("call %vredrs_value @vredrs_value_add"));
        assert!(ll.contains("call i64 @vredrs_value_get_i64"));
    }
}
