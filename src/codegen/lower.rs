//! # lower.rs — 唯一的 AST 遍历点
//!
//! 这是手册 Phase 1 / Phase 2 的核心：把 [`parser::ast::Program`] 翻译成
//! [`ir::IrModule`]。所有后端（VM / LLVM / Raw / Cstar）都消费这个 IR，
//! 不再各自啃 AST。
//!
//! ## 渐进式类型推断（手册 Phase 3 修正版）
//!
//! - 有类型注解的参数/变量 → 静态 `TypeHint`（`I64`/`F64`/`Bool`/`Ptr`/`Str`）
//! - 无注解 → `Dynamic`，后端降级为 runtime 调用
//! - 字面量：`Integer`→`I64`、`Float`→`F64`、`String_`→`Str`、`Bool`→`Bool`
//! - 二元运算：两边同静态且兼容 → `Bin`（静态）；否则 → `Bin{op: *Dyn}`（动态）
//! - 同一函数体内静态与动态可共存，边界处自动 `Box`/`Unbox`
//!
//! ## 覆盖的 AST 节点（手册 §3.3 完成标准）
//!
//! `Assign`、`If`、`While`、`ForIn`、`ForRange`、`Loop`、`Return`、`Expr`、
//! `Println`、`Asm`、`Try`、`Throw`、`Panic`、`UnsafeBlock`、`Break`、
//! `Continue`、`Match`、`Assert`。

use crate::parser::ast::{
    self, AssignOp, Assignee, BasicType, BinaryOp, ClassDef, Expr, FnDef, Identifier, Pattern,
    Program, Stmt, TopLevel, TypeExpr, UnaryOp,
};
use crate::codegen::ir::{BinOp, Cond, IrFunction, IrModule, Operand, StaticInsn, TypeHint};
use std::collections::{HashMap, HashSet};

// ── Class metadata + VTable support (stage3-vtable) ────────────────────────

/// 编译期收集的 class 元数据：父类、方法集（方法名 → 参数个数）、是否有 new/构造器。
#[derive(Clone, Debug, Default)]
pub struct ClassInfo {
    pub name: String,
    pub extends: Option<String>,
    pub methods: HashMap<String, usize>,
    pub has_new: bool,
}

/// 静态分派表条目：记录 (类名, 方法名) → IR 函数名 + 参数个数，供 VTable 调用使用。
#[derive(Clone, Debug)]
pub struct DispatchEntry {
    pub class_name: String,
    pub method_name: String,
    pub func_name: String,
    pub arg_count: usize,
}

/// Lowering 上下文。每个函数 lowering 时创建一个新的局部上下文。
pub struct Lowerer {
    /// 输出的 IR 模块。
    module: IrModule,
    /// 虚拟寄存器计数器（产出 `v0`, `v1`, ...）。
    reg_counter: usize,
    /// 标签计数器（产出 `.L0`, `.L1`, ...）。
    label_counter: usize,
    /// 当前函数名（用于 label 前缀，避免跨函数 label 名冲突）。
    cur_fn_name: String,
    /// 当前函数的局部变量名 → (虚拟寄存器名, 类型)。
    locals: HashMap<String, (String, TypeHint)>,
    /// 当前函数的参数名列表（用于 `main` 调用约定）。
    params: Vec<(String, TypeHint)>,
    /// 当前函数体指令缓冲。
    body: Vec<StaticInsn>,
    /// 当前函数的局部变量声明（用于 IrFunction.locals）。
    local_decls: Vec<(String, TypeHint)>,
    /// break/continue 跳转栈。每项是 (continue_label, break_label)。
    loop_stack: Vec<(String, String)>,
    /// 已收集的函数定义（用于函数调用解析与内联）。
    fn_sigs: HashMap<String, FnSig>,
    /// 已收集的 class 名（用于识别 class 构造调用 ClassName()）。
    class_names: std::collections::HashSet<String>,
    /// 字符串是否在池中（去重）。
    str_seen: HashMap<String, usize>,
    // ── stage3-vtable 新增字段 ─────────────────────────────────────────
    /// class 元数据：class 名 → ClassInfo（父类、方法集等）。
    pub class_defs: HashMap<String, ClassInfo>,
    /// 局部变量名 → 推断出的 class 名（用于静态方法分派）。
    var_class: HashMap<String, String>,
    /// 局部变量名集合：被推断为字符串类型的变量（用于 str_len/str_concat 等静态分派）。
    var_str: HashSet<String>,
    /// 当前正在 lower 的 class 方法所属的 class 名（None 表示不在 class 方法中）。
    current_class: Option<String>,
    /// 静态分派表：所有 (class, method) → IR 函数名映射。
    pub dispatch_table: Vec<DispatchEntry>,
    /// lambda 计数器，用于生成唯一函数名 `__lambda_N`。
    lambda_counter: usize,
    /// 已识别的 generator 函数名集合（含 yield 的函数）。
    generator_fns: HashSet<String>,
    /// lambda 函数定义缓冲：在 lower 过程中累积的 lambda 函数体，最后追加到模块。
    lambda_fns: Vec<IrFunction>,
}

/// 函数签名摘要（供调用点类型推断）。
#[derive(Clone)]
struct FnSig {
    params: Vec<TypeHint>,
    returns: TypeHint,
}

impl Lowerer {
    pub fn new() -> Self {
        Lowerer {
            module: IrModule::new(),
            reg_counter: 0,
            label_counter: 0,
            cur_fn_name: String::new(),
            locals: HashMap::new(),
            params: Vec::new(),
            body: Vec::new(),
            local_decls: Vec::new(),
            loop_stack: Vec::new(),
            fn_sigs: HashMap::new(),
            class_names: std::collections::HashSet::new(),
            str_seen: HashMap::new(),
            class_defs: HashMap::new(),
            var_class: HashMap::new(),
            var_str: HashSet::new(),
            current_class: None,
            dispatch_table: Vec::new(),
            lambda_counter: 0,
            generator_fns: HashSet::new(),
            lambda_fns: Vec::new(),
        }
    }

    /// 入口：把整个程序 lower 成 IR 模块。
    pub fn lower(mut self, program: &Program) -> IrModule {
        // 第一遍：收集函数签名（供调用点类型推断）和 class 元数据。
        for d in &program.declarations {
            if let TopLevel::FnDef(f) = d {
                let params: Vec<TypeHint> = f
                    .params
                    .iter()
                    .map(|p| p.type_annotation.as_ref().map(type_expr_to_hint).unwrap_or(TypeHint::Dynamic))
                    .collect();
                let ret = f
                    .return_type
                    .as_ref()
                    .map(type_expr_to_hint)
                    .unwrap_or(TypeHint::Dynamic);
                self.fn_sigs.insert(f.name.name.clone(), FnSig { params, returns: ret });
                // generator 检测：函数体含 yield → generator_fns。
                if fn_has_yield(&f.body) {
                    self.generator_fns.insert(f.name.name.clone());
                }
            }
            if let TopLevel::ClassDef(c) = d {
                self.collect_class_metadata(c);
            }
        }
        // 第二遍：lower 每个函数。
        for d in &program.declarations {
            if let TopLevel::FnDef(f) = d {
                self.lower_function(f);
            }
            if let TopLevel::ClassDef(c) = d {
                // lower class 方法为 `Class_method(self, args...)` 形式的顶层函数。
                for m in &c.methods {
                    self.lower_class_method(&c.name.name, m);
                }
            }
        }
        // 追加 lambda 函数（在 lower 过程中累积的）。
        for f in std::mem::take(&mut self.lambda_fns) {
            self.module.functions.push(f);
        }
        // 若没有 main 且存在顶层语句/常量，则把顶层语句包装成一个 main 函数。
        // 纯库文件（只有函数定义）不会生成 main。
        let has_main = self.module.functions.iter().any(|f| f.name == "main");
        if !has_main {
            let has_toplevel_stmts = program.declarations.iter().any(|d| {
                matches!(d, TopLevel::Statement(_) | TopLevel::ConstExpr(_))
            });
            if has_toplevel_stmts {
                self.lower_toplevel_as_main(program);
            }
        }
        self.module.entry = Some("main".to_string());
        self.module
    }

    /// 收集 class 元数据：name、extends、methods、has_new。
    fn collect_class_metadata(&mut self, c: &ClassDef) {
        self.class_names.insert(c.name.name.clone());
        let mut info = ClassInfo {
            name: c.name.name.clone(),
            extends: c.extends.as_ref().and_then(extract_class_name),
            methods: HashMap::new(),
            has_new: false,
        };
        for m in &c.methods {
            let argc = m.params.len();
            info.methods.insert(m.name.name.clone(), argc);
            if m.name.name == "new" || m.name.name == "__init__" {
                info.has_new = true;
            }
            // 注册分派表条目：func_name = `Class_method`。
            let func_name = format!("{}_{}", c.name.name, m.name.name);
            self.dispatch_table.push(DispatchEntry {
                class_name: c.name.name.clone(),
                method_name: m.name.name.clone(),
                func_name,
                arg_count: argc,
            });
            // 把 class 方法的签名也注册到 fn_sigs（参数去掉 self 后的个数）。
            // 注意：lower_call 时，class 方法以 `Class_method(self, args...)` 调用，
            // 所以参数个数是 argc（含 self）。
            let params: Vec<TypeHint> = std::iter::once(TypeHint::Dynamic)
                .chain(m.params.iter().map(|p| {
                    p.type_annotation.as_ref().map(type_expr_to_hint).unwrap_or(TypeHint::Dynamic)
                }))
                .collect();
            let ret = m
                .return_type
                .as_ref()
                .map(type_expr_to_hint)
                .unwrap_or(TypeHint::Dynamic);
            let qualified = format!("{}_{}", c.name.name, m.name.name);
            self.fn_sigs.insert(qualified.clone(), FnSig { params, returns: ret });
            if fn_has_yield(&m.body) {
                self.generator_fns.insert(qualified);
            }
        }
        self.class_defs.insert(c.name.name.clone(), info);
    }

    /// Lower 单个函数定义。
    fn lower_function(&mut self, f: &FnDef) {
        self.reg_counter = 0;
        self.label_counter = 0;
        self.cur_fn_name = f.name.name.clone();
        self.locals.clear();
        self.params.clear();
        self.body.clear();
        self.local_decls.clear();
        self.loop_stack.clear();

        // 注解收集（供 Cstar 过滤器识别）。
        let annotations: Vec<String> = f.annotations.iter().map(|a| a.name.clone()).collect();

        // 参数：有注解→静态寄存器；无注解→Dynamic 寄存器。
        let mut param_regs: Vec<String> = Vec::new();
        for p in &f.params {
            let ty = p
                .type_annotation
                .as_ref()
                .map(type_expr_to_hint)
                .unwrap_or(TypeHint::Dynamic);
            let reg = self.fresh_reg(ty);
            self.locals.insert(p.name.name.clone(), (reg.clone(), ty));
            self.params.push((p.name.name.clone(), ty));
            self.local_decls.push((p.name.name.clone(), ty));
            param_regs.push(reg);
        }

        // 函数体。
        for s in &f.body {
            self.lower_stmt(s);
        }

        // 确保函数末尾有返回（避免 fall-through）。
        if !matches!(self.body.last(), Some(StaticInsn::Ret { .. })) {
            self.body.push(StaticInsn::Ret { value: None });
        }

        let return_ty = f
            .return_type
            .as_ref()
            .map(type_expr_to_hint)
            .unwrap_or(TypeHint::Dynamic);

        let func = IrFunction {
            name: f.name.name.clone(),
            params: self.params.clone(),
            param_regs,
            return_ty,
            body: std::mem::take(&mut self.body),
            is_main: f.name.name == "main",
            annotations,
            locals: self.local_decls.clone(),
        };
        self.module.functions.push(func);
    }

    /// Lower 单个 class 方法为顶层函数 `Class_method(self, args...)`。
    /// 保存/恢复当前 lowering 上下文，避免污染外层函数。
    fn lower_class_method(&mut self, class_name: &str, m: &FnDef) {
        // 保存上下文。
        let saved_reg = self.reg_counter;
        let saved_label = self.label_counter;
        let saved_fn = std::mem::take(&mut self.cur_fn_name);
        let saved_locals = std::mem::take(&mut self.locals);
        let saved_params = std::mem::take(&mut self.params);
        let saved_body = std::mem::take(&mut self.body);
        let saved_decls = std::mem::take(&mut self.local_decls);
        let saved_loops = std::mem::take(&mut self.loop_stack);
        let saved_var_class = std::mem::take(&mut self.var_class);
        let saved_var_str = std::mem::take(&mut self.var_str);
        let saved_current = self.current_class.take();

        // 设置 class 方法上下文。
        self.reg_counter = 0;
        self.label_counter = 0;
        let method_qual = format!("{}_{}", class_name, m.name.name);
        self.cur_fn_name = method_qual.clone();
        self.current_class = Some(class_name.to_string());

        let annotations: Vec<String> = m.annotations.iter().map(|a| a.name.clone()).collect();

        // 第一个参数是 self（Dynamic），后续是方法的参数。
        let mut param_regs: Vec<String> = Vec::new();
        // self
        {
            let ty = TypeHint::Dynamic;
            let reg = self.fresh_reg(ty);
            self.locals.insert("self".to_string(), (reg.clone(), ty));
            self.params.push(("self".to_string(), ty));
            self.local_decls.push(("self".to_string(), ty));
            param_regs.push(reg);
            // self 的类是已知的。
            self.var_class.insert("self".to_string(), class_name.to_string());
        }
        for p in &m.params {
            let ty = p
                .type_annotation
                .as_ref()
                .map(type_expr_to_hint)
                .unwrap_or(TypeHint::Dynamic);
            let reg = self.fresh_reg(ty);
            self.locals.insert(p.name.name.clone(), (reg.clone(), ty));
            self.params.push((p.name.name.clone(), ty));
            self.local_decls.push((p.name.name.clone(), ty));
            param_regs.push(reg);
        }

        for s in &m.body {
            self.lower_stmt(s);
        }
        if !matches!(self.body.last(), Some(StaticInsn::Ret { .. })) {
            self.body.push(StaticInsn::Ret { value: None });
        }

        let return_ty = m
            .return_type
            .as_ref()
            .map(type_expr_to_hint)
            .unwrap_or(TypeHint::Dynamic);

        let func = IrFunction {
            name: method_qual,
            params: self.params.clone(),
            param_regs,
            return_ty,
            body: std::mem::take(&mut self.body),
            is_main: false,
            annotations,
            locals: self.local_decls.clone(),
        };
        self.module.functions.push(func);

        // 恢复上下文。
        self.reg_counter = saved_reg;
        self.label_counter = saved_label;
        self.cur_fn_name = saved_fn;
        self.locals = saved_locals;
        self.params = saved_params;
        self.body = saved_body;
        self.local_decls = saved_decls;
        self.loop_stack = saved_loops;
        self.var_class = saved_var_class;
        self.var_str = saved_var_str;
        self.current_class = saved_current;
    }

    // ── generator 检测 ──────────────────────────────────────

    /// 把顶层语句包装成一个 `main` 函数（与 VM 的"无 main 则跑顶层代码"语义一致）。
    fn lower_toplevel_as_main(&mut self, program: &Program) {
        self.reg_counter = 0;
        self.label_counter = 0;
        self.cur_fn_name = "main".to_string();
        self.locals.clear();
        self.params.clear();
        self.body.clear();
        self.local_decls.clear();
        self.loop_stack.clear();

        self.body.push(StaticInsn::Comment("top-level statements".into()));
        for d in &program.declarations {
            if let TopLevel::Statement(s) = d {
                self.lower_stmt(s);
            } else if let TopLevel::ConstExpr(c) = d {
                // const, NAME = expr  → 视为全局赋值。
                let val = self.lower_expr(&c.value);
                self.declare_local(&c.name.name, val.ty());
                if let Some((reg, _)) = self.locals.get(&c.name.name).cloned() {
                    self.emit(StaticInsn::Mov { dst: Operand::Reg(reg, TypeHint::Dynamic), src: val, ty: TypeHint::Dynamic });
                }
            }
        }
        // main 返回 0。
        self.body.push(StaticInsn::Ret { value: Some(Operand::ImmI64(0)) });

        let func = IrFunction {
            name: "main".into(),
            params: vec![],
            param_regs: vec![],
            return_ty: TypeHint::I64,
            body: std::mem::take(&mut self.body),
            is_main: true,
            annotations: vec![],
            locals: self.local_decls.clone(),
        };
        self.module.functions.push(func);
    }

    // ── 工具方法 ────────────────────────────────────────────

    fn fresh_reg(&mut self, ty: TypeHint) -> String {
        let n = format!("v{}", self.reg_counter);
        self.reg_counter += 1;
        n
    }
    fn fresh_label(&mut self, hint: &str) -> String {
        // 加入函数名前缀，避免跨函数 label 名冲突（汇编器要求 label 全局唯一）。
        let l = format!(".L{}_{}_{}", self.cur_fn_name, self.label_counter, hint);
        self.label_counter += 1;
        l
    }
    fn emit(&mut self, insn: StaticInsn) {
        self.body.push(insn);
    }
    fn intern_str(&mut self, s: &str) -> usize {
        if let Some(&i) = self.str_seen.get(s) {
            return i;
        }
        let i = self.module.intern_string(s);
        self.str_seen.insert(s.to_string(), i);
        i
    }
    /// 声明一个局部变量（分配虚拟寄存器 + 记录类型）。
    fn declare_local(&mut self, name: &str, ty: TypeHint) {
        if self.locals.contains_key(name) {
            // 已存在：更新类型（用于 `set, x = ...` 后续赋值）。
            self.locals.insert(name.to_string(), (self.locals[name].0.clone(), ty));
            return;
        }
        let reg = self.fresh_reg(ty);
        self.locals.insert(name.to_string(), (reg, ty));
        self.local_decls.push((name.to_string(), ty));
    }
    /// 读取局部变量的当前寄存器。
    fn local_reg(&self, name: &str) -> Option<(String, TypeHint)> {
        self.locals.get(name).cloned()
    }

    // ── class / VTable 辅助方法（stage3-vtable）──────────────

    /// 检查某个 class（含继承链）是否定义了某个方法。
    pub fn class_has_method(&self, class_name: &str, method: &str) -> bool {
        let mut current = Some(class_name.to_string());
        while let Some(c) = current {
            if let Some(info) = self.class_defs.get(&c) {
                if info.methods.contains_key(method) {
                    return true;
                }
                current = info.extends.clone();
            } else {
                return false;
            }
        }
        false
    }

    /// 在继承链上解析方法所属的类（用于 super.method 调用与 VTable 分派）。
    /// 返回定义该方法的最近类名。若不存在返回 None。
    pub fn resolve_method_owner(&self, class_name: &str, method: &str) -> Option<String> {
        let mut current = Some(class_name.to_string());
        while let Some(c) = current {
            if let Some(info) = self.class_defs.get(&c) {
                if info.methods.contains_key(method) {
                    return Some(c);
                }
                current = info.extends.clone();
            } else {
                return None;
            }
        }
        None
    }

    /// 返回某个 class 的父类名（无则 None）。
    pub fn class_parent(&self, class_name: &str) -> Option<String> {
        self.class_defs.get(class_name).and_then(|i| i.extends.clone())
    }

    /// 从表达式推断其所属的 class 名（用于静态方法分派）。
    /// 仅识别 Identifier 和 MemberAccess 这两种简单情形，其他返回 None。
    pub fn infer_class_from_expr(&self, e: &Expr) -> Option<String> {
        match e {
            Expr::Identifier(id) => self.var_class.get(&id.name).cloned(),
            Expr::MemberAccess(m) => {
                // self.x → 不推断为 class（字段值可能是任意类型）
                None
            }
            Expr::Call(c) => {
                // ClassName.new(args) or ClassName(args) → class name
                if let Expr::Identifier(id) = c.callee.as_ref() {
                    if self.class_names.contains(&id.name) {
                        return Some(id.name.clone());
                    }
                }
                if let Expr::MemberAccess(ma) = c.callee.as_ref() {
                    if ma.member.name == "new" {
                        if let Expr::Identifier(id) = ma.target.as_ref() {
                            if self.class_names.contains(&id.name) {
                                return Some(id.name.clone());
                            }
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// 推断表达式是否为字符串类型（用于 str_len/str_concat 等静态分派）。
    /// 收集 lambda body 中引用的外层局部变量（captures）。
    fn collect_free_vars_expr(&self, e: &Expr, params: &std::collections::HashSet<String>, out: &mut Vec<String>) {
        match e {
            Expr::Identifier(id) => {
                if !params.contains(&id.name) && !out.contains(&id.name) && self.locals.contains_key(&id.name) {
                    out.push(id.name.clone());
                }
            }
            Expr::Binary(b) => {
                self.collect_free_vars_expr(&b.left, params, out);
                self.collect_free_vars_expr(&b.right, params, out);
            }
            Expr::Unary(u) => self.collect_free_vars_expr(&u.operand, params, out),
            Expr::Call(c) => {
                self.collect_free_vars_expr(&c.callee, params, out);
                for a in &c.args { self.collect_free_vars_expr(a, params, out); }
            }
            Expr::MemberAccess(m) => self.collect_free_vars_expr(&m.target, params, out),
            Expr::Index(i) => {
                self.collect_free_vars_expr(&i.target, params, out);
                self.collect_free_vars_expr(&i.index, params, out);
            }
            Expr::Postfix(p) => self.collect_free_vars_expr(&p.operand, params, out),
            _ => {}
        }
    }

    pub fn infer_is_string(&self, e: &Expr) -> bool {
        match e {
            Expr::String_(_) | Expr::MultiLineString(_) => true,
            Expr::Identifier(id) => self.var_str.contains(&id.name),
            _ => false,
        }
    }

    /// 在继承链上查找分派表条目：返回第一个匹配 (class, method) 的 func_name。
    fn lookup_dispatch(&self, class_name: &str, method: &str) -> Option<DispatchEntry> {
        let mut current = Some(class_name.to_string());
        while let Some(c) = current {
            if let Some(e) = self.dispatch_table.iter().find(|e| e.class_name == c && e.method_name == method) {
                return Some(e.clone());
            }
            current = self.class_defs.get(&c).and_then(|i| i.extends.clone());
        }
        None
    }

    // ── 语句 lowering ──────────────────────────────────────

    fn lower_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Assign(a) => self.lower_assign(a),
            Stmt::Return(r) => {
                if r.values.is_empty() {
                    self.emit(StaticInsn::Ret { value: None });
                } else if r.values.len() == 1 {
                    let v = self.lower_expr(&r.values[0]);
                    self.emit(StaticInsn::Ret { value: Some(v) });
                } else {
                    // 多返回值：装箱成动态元组（VM 语义）。
                    // vredrs_make_tuple(n, v1, v2, ...) — 第一个参数是元素个数。
                    let n = r.values.len() as i64;
                    let mut args = vec![Operand::ImmI64(n)];
                    for v in &r.values {
                        args.push(self.lower_expr(v));
                    }
                    let dst = self.fresh_reg(TypeHint::Dynamic);
                    self.emit(StaticInsn::RuntimeCall {
                        dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                        func: "vredrs_make_tuple".into(),
                        args,
                    });
                    self.emit(StaticInsn::Ret { value: Some(Operand::Reg(dst, TypeHint::Dynamic)) });
                }
            }
            Stmt::Expr(e) => {
                // 表达式语句：求值后丢弃（除非有副作用，后端会处理）。
                let _ = self.lower_expr(&e.expr);
            }
            Stmt::Println(p) => self.lower_println(p),
            Stmt::If(i) => self.lower_if(i),
            Stmt::While(w) => self.lower_while(w),
            Stmt::ForRange(fr) => self.lower_for_range(fr),
            Stmt::ForIn(fi) => self.lower_for_in(fi),
            Stmt::Loop(l) => self.lower_loop(l),
            Stmt::Break(b) => {
                if let Some((_, brk)) = self.loop_stack.last() {
                    let target = b.label.as_ref().map(|l| format!(".Lbreak_{}", l.name)).unwrap_or_else(|| brk.clone());
                    self.emit(StaticInsn::Jump { target });
                }
            }
            Stmt::Continue(c) => {
                if let Some((cont, _)) = self.loop_stack.last() {
                    let target = c.label.as_ref().map(|l| format!(".Lcont_{}", l.name)).unwrap_or_else(|| cont.clone());
                    self.emit(StaticInsn::Jump { target });
                }
            }
            Stmt::Throw(t) => {
                let v = self.lower_expr(&t.value);
                self.emit(StaticInsn::RuntimeCall {
                    dst: None,
                    func: "vredrs_throw".into(),
                    args: vec![v],
                });
            }
            Stmt::Panic(p) => {
                let v = self.lower_expr(&p.message);
                self.emit(StaticInsn::RuntimeCall {
                    dst: None,
                    func: "vredrs_panic".into(),
                    args: vec![v],
                });
            }
            Stmt::Assert(a) => {
                let cond = self.lower_expr(&a.condition);
                let fail = self.fresh_label("assert_fail");
                let end = self.fresh_label("assert_end");
                self.emit(StaticInsn::Branch {
                    cond: Cond::Eq,
                    lhs: cond.clone(),
                    rhs: Operand::ImmBool(false),
                    target: fail.clone(),
                });
                self.emit(StaticInsn::Jump { target: end.clone() });
                self.emit(StaticInsn::Label(fail));
                if let Some(msg) = &a.message {
                    let s = self.intern_str(msg);
                    self.emit(StaticInsn::RuntimeCall {
                        dst: None,
                        func: "vredrs_assert_fail".into(),
                        args: vec![Operand::ImmStr(s)],
                    });
                } else {
                    self.emit(StaticInsn::RuntimeCall {
                        dst: None,
                        func: "vredrs_assert_fail".into(),
                        args: vec![],
                    });
                }
                self.emit(StaticInsn::Label(end));
            }
            Stmt::Asm(a) => self.lower_asm(a),
            Stmt::Try(t) => self.lower_try(t),
            Stmt::UnsafeBlock(u) => {
                self.emit(StaticInsn::Comment("unsafe block".into()));
                for s in &u.body {
                    self.lower_stmt(s);
                }
            }
            Stmt::Match(m) => self.lower_match(m),
            Stmt::Defer(d) => {
                // defer: lower 语句体（Raw 后端无 defer 栈，简化为当前位置执行）。
                self.emit(StaticInsn::Comment("defer block".into()));
                self.lower_stmt(&d.stmt);
            }
            Stmt::Yield(y) => {
                let v = match &y.value {
                    Some(e) => self.lower_expr(e),
                    None => Operand::Null,
                };
                self.emit(StaticInsn::RuntimeCall {
                    dst: None,
                    func: "vredrs_yield".into(),
                    args: vec![v],
                });
            }
            Stmt::Spawn(s) => {
                let v = self.lower_expr(&s.call);
                self.emit(StaticInsn::RuntimeCall {
                    dst: None,
                    func: "vredrs_spawn".into(),
                    args: vec![v],
                });
            }
            Stmt::SpawnThread(s) => {
                let v = self.lower_expr(&s.call);
                self.emit(StaticInsn::RuntimeCall {
                    dst: None,
                    func: "vredrs_spawn_thread".into(),
                    args: vec![v],
                });
            }
            Stmt::Flush(_) => {
                self.emit(StaticInsn::RuntimeCall {
                    dst: None,
                    func: "vredrs_flush".into(),
                    args: vec![],
                });
            }
            Stmt::Input(i) => {
                let dst = self.fresh_reg(TypeHint::Dynamic);
                let prompt_arg = match &i.prompt {
                    Some(p) => {
                        let s = self.intern_str(p);
                        Some(Operand::ImmStr(s))
                    }
                    None => None,
                };
                let mut args = Vec::new();
                if let Some(p) = prompt_arg {
                    args.push(p);
                }
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_input".into(),
                    args,
                });
                // 存入目标。
                self.store_to_assignee(&i.target, Operand::Reg(dst, TypeHint::Dynamic));
            }
            // 其余语句（Pon、TableAssign、Paste、Select、DirectiveBlock、ScopeBlock）
            // 暂时降级为 runtime 调用或注释（手册允许 Raw 忽略 spawn/async 等）。
            Stmt::Pon(p) => self.lower_assign(&p.assign),
            Stmt::TableAssign(t) => {
                // table_assign: 把列名与行数据传给 runtime，由 runtime 构造表对象。
                self.emit(StaticInsn::Comment(format!("table_assign ({} cols)", t.column_names.len())));
                let n_cols = t.column_names.len() as i64;
                let mut args = vec![Operand::ImmI64(n_cols)];
                for col in &t.column_names {
                    let s = self.intern_str(&col.name);
                    args.push(Operand::ImmStr(s));
                }
                let n_rows = t.rows.len() as i64;
                args.push(Operand::ImmI64(n_rows));
                for row in &t.rows {
                    for cell in row {
                        args.push(self.lower_expr(cell));
                    }
                }
                self.emit(StaticInsn::RuntimeCall {
                    dst: None,
                    func: "vredrs_table_assign".into(),
                    args,
                });
            }
            Stmt::Paste(p) => {
                // paste, a, b, c → runtime 拼接。
                let mut args = Vec::new();
                for a in &p.args {
                    args.push(self.lower_expr(a));
                }
                self.emit(StaticInsn::RuntimeCall {
                    dst: None,
                    func: "vredrs_paste".into(),
                    args,
                });
            }
            Stmt::Select(s) => {
                // select (concurrent): 把 case 数和 default 标志传给 runtime。
                self.emit(StaticInsn::Comment("select (concurrent)".into()));
                let n_cases = s.cases.len() as i64;
                let has_default = s.default_case.is_some() as i64;
                let mut args = vec![Operand::ImmI64(n_cases), Operand::ImmI64(has_default)];
                // 把每个 case 的方向和 body 长度也传入（runtime 用于调度决策）。
                for case in &s.cases {
                    let (dir_tag, chan_opt, val_opt): (i64, Option<&Expr>, Option<&Expr>) = match &case.direction {
                        ast::SelectDirection::Send { channel, value } => (0, Some(channel), Some(value)),
                        ast::SelectDirection::Receive { channel, var: _ } => (1, Some(channel), None),
                        ast::SelectDirection::After(dur) => (2, Some(dur), None),
                    };
                    args.push(Operand::ImmI64(dir_tag));
                    if let Some(c) = chan_opt {
                        args.push(self.lower_expr(c));
                    }
                    if let Some(v) = val_opt {
                        args.push(self.lower_expr(v));
                    }
                }
                // 把每个 case 的 body 作为 IR 顺序生成（runtime 决定是否执行）。
                self.emit(StaticInsn::RuntimeCall {
                    dst: None,
                    func: "vredrs_select".into(),
                    args,
                });
                // 顺序 lower 每个 case 的 body（保守：实际 select 调度由 runtime 完成）。
                for case in &s.cases {
                    for st in &case.body {
                        self.lower_stmt(st);
                    }
                }
                if let Some(def) = &s.default_case {
                    for st in def {
                        self.lower_stmt(st);
                    }
                }
            }
            Stmt::DirectiveBlock(db) => {
                self.emit(StaticInsn::Comment(format!("/{} block", db.directive)));
                for s in &db.body {
                    self.lower_stmt(s);
                }
            }
            Stmt::ScopeBlock(sb) => {
                self.emit(StaticInsn::Comment(format!("scope /{}/", sb.prefix)));
                for s in &sb.body {
                    self.lower_stmt(s);
                }
            }
            Stmt::With(w) => {
                // with manager as var: ... — VTable __enter__/__exit__ 分派。
                // 若 manager 推断为已知 class 且实现了 __enter__/__exit__，走静态调用；
                // 否则降级为 vredrs_with_enter/with_value/with_exit runtime。
                let mgr = self.lower_expr(&w.manager);
                let mgr_class = self.infer_class_from_expr(&w.manager);
                let entered = self.fresh_reg(TypeHint::Dynamic);
                let enter_call = if let Some(class) = &mgr_class {
                    if self.class_has_method(class, "__enter__") {
                        // 静态调用 Class___enter__(self)。
                        let func = format!("{}___enter__", class);
                        self.emit(StaticInsn::Call {
                            dst: Some(Operand::Reg(entered.clone(), TypeHint::Dynamic)),
                            func,
                            args: vec![mgr.clone()],
                            ty: TypeHint::Dynamic,
                        });
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !enter_call {
                    // 运行时 VTable 分派：vredrs_with_enter(manager) → 返回 self。
                    self.emit(StaticInsn::RuntimeCall {
                        dst: Some(Operand::Reg(entered.clone(), TypeHint::Dynamic)),
                        func: "vredrs_with_enter".into(),
                        args: vec![mgr.clone()],
                    });
                }
                if let Some(var) = &w.var {
                    self.declare_local(&var.name, TypeHint::Dynamic);
                    if let Some((reg, _)) = self.local_reg(&var.name) {
                        self.emit(StaticInsn::Mov {
                            dst: Operand::Reg(reg, TypeHint::Dynamic),
                            src: Operand::Reg(entered, TypeHint::Dynamic),
                            ty: TypeHint::Dynamic,
                        });
                    }
                }
                for s in &w.body {
                    self.lower_stmt(s);
                }
                // __exit__ 分派（同 __enter__ 的策略）。
                let exit_static = if let Some(class) = &mgr_class {
                    if self.class_has_method(class, "__exit__") {
                        let func = format!("{}___exit__", class);
                        self.emit(StaticInsn::Call {
                            dst: None,
                            func,
                            args: vec![mgr.clone()],
                            ty: TypeHint::Dynamic,
                        });
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !exit_static {
                    self.emit(StaticInsn::RuntimeCall {
                        dst: None,
                        func: "vredrs_with_exit".into(),
                        args: vec![mgr],
                    });
                }
            }
        }
    }

    fn lower_assign(&mut self, a: &ast::AssignStmt) {
        use AssignOp::*;
        match a.operator {
            Delete => {
                // del, target → 调用 runtime 释放。
                for t in &a.targets {
                    if let Assignee::Identifier(id) = t {
                        self.emit(StaticInsn::Comment(format!("del {}", id.name)));
                        // 标记局部变量为已释放（线性类型检查用）。
                        self.locals.remove(&id.name);
                    }
                }
                return;
            }
            Simple => {
                let val = self.lower_expr(&a.value);
                // 类型推断跟踪：根据 RHS 推断 var_class / var_str。
                let inferred_class = self.infer_class_from_expr(&a.value);
                let is_string = self.infer_is_string(&a.value) || val.ty() == TypeHint::Str;
                // 多目标解构：若 targets 长度 > 1 且 RHS 是动态值，按 tuple 解构。
                if a.targets.len() > 1 {
                    for (i, t) in a.targets.iter().enumerate() {
                        // 取 RHS 的第 i 个元素：vredrs_index(rhs, i)。
                        let idx_val = self.fresh_reg(TypeHint::Dynamic);
                        self.emit(StaticInsn::RuntimeCall {
                            dst: Some(Operand::Reg(idx_val.clone(), TypeHint::Dynamic)),
                            func: "vredrs_index".into(),
                            args: vec![val.clone(), Operand::ImmI64(i as i64)],
                        });
                        self.store_to_assignee(t, Operand::Reg(idx_val, TypeHint::Dynamic));
                        // 跟踪 var_class/var_str（保守：解构后无法静态推断类型）。
                        if let Assignee::Identifier(id) = t {
                            self.var_class.remove(&id.name);
                            self.var_str.remove(&id.name);
                        }
                    }
                } else {
                    for t in &a.targets {
                        self.store_to_assignee(t, val.clone());
                    }
                    // 单目标：记录类型推断。
                    if let Some(class) = inferred_class {
                        if let Some(Assignee::Identifier(id)) = a.targets.first() {
                            self.var_class.insert(id.name.clone(), class);
                        }
                    } else if let Some(Assignee::Identifier(id)) = a.targets.first() {
                        // 若 RHS 不是 class 实例，清除之前的 class 标记。
                        self.var_class.remove(&id.name);
                    }
                    if is_string {
                        if let Some(Assignee::Identifier(id)) = a.targets.first() {
                            self.var_str.insert(id.name.clone());
                        }
                    } else if let Some(Assignee::Identifier(id)) = a.targets.first() {
                        self.var_str.remove(&id.name);
                    }
                }
            }
            _ => {
                // 复合赋值 (+=, -=, ...)：读旧值 + 运算 + 存回。
                for t in &a.targets {
                    if let Assignee::Identifier(id) = t {
                        let old = match self.local_reg(&id.name) {
                            Some((r, ty)) => Operand::Reg(r, ty),
                            None => Operand::Null,
                        };
                        let rhs = self.lower_expr(&a.value);
                        let op = match a.operator {
                            Plus => BinOp::Add,
                            Minus => BinOp::Sub,
                            Star => BinOp::Mul,
                            Slash => BinOp::Div,
                            Percent => BinOp::Mod,
                            _ => BinOp::Add,
                        };
                        let ty = if old.ty().is_static() && rhs.ty().is_static() {
                            old.ty()
                        } else {
                            TypeHint::Dynamic
                        };
                        let dyn_op = if ty.is_dynamic() { dyn_op_of(op) } else { op };
                        let dst = self.fresh_reg(ty);
                        self.emit(StaticInsn::Bin {
                            op: dyn_op,
                            dst: Operand::Reg(dst.clone(), ty),
                            lhs: old,
                            rhs,
                            ty,
                        });
                        self.declare_local(&id.name, ty);
                        if let Some((reg, _)) = self.local_reg(&id.name) {
                            self.emit(StaticInsn::Mov {
                                dst: Operand::Reg(reg, ty),
                                src: Operand::Reg(dst, ty),
                                ty,
                            });
                        }
                    }
                }
            }
        }
    }

    /// 把一个值存到赋值目标（标识符 / 索引 / 成员）。
    fn store_to_assignee(&mut self, target: &Assignee, val: Operand) {
        match target {
            Assignee::Identifier(id) => {
                let ty = val.ty();
                if !self.locals.contains_key(&id.name) {
                    self.declare_local(&id.name, ty);
                }
                if let Some((reg, reg_ty)) = self.local_reg(&id.name) {
                    // 若类型不一致（静态→动态或反之），插入 Box/Unbox。
                    if reg_ty != ty {
                        if ty.is_static() && reg_ty.is_dynamic() {
                            self.emit(StaticInsn::Box {
                                dst: Operand::Reg(reg.clone(), reg_ty),
                                src: val,
                                from: ty,
                            });
                        } else if ty.is_dynamic() && reg_ty.is_static() {
                            self.emit(StaticInsn::Unbox {
                                dst: Operand::Reg(reg.clone(), reg_ty),
                                src: val,
                                to: reg_ty,
                            });
                        } else {
                            self.emit(StaticInsn::Mov {
                                dst: Operand::Reg(reg.clone(), reg_ty),
                                src: val,
                                ty: reg_ty,
                            });
                        }
                    } else {
                        self.emit(StaticInsn::Mov {
                            dst: Operand::Reg(reg.clone(), reg_ty),
                            src: val,
                            ty: reg_ty,
                        });
                    }
                }
            }
            Assignee::Qualified(q) => {
                // 模块/对象字段赋值：a.b.c = v → runtime 调用。
                let mut path = String::new();
                for (i, p) in q.parts.iter().enumerate() {
                    if i > 0 {
                        path.push('.');
                    }
                    path.push_str(&p.name);
                }
                self.emit(StaticInsn::RuntimeCall {
                    dst: None,
                    func: "vredrs_set_field".into(),
                    args: vec![Operand::Sym(path), val],
                });
            }
            Assignee::Index(ix) => {
                let target = self.lower_expr(&ix.target);
                let idx = self.lower_expr(&ix.index);
                self.emit(StaticInsn::Store {
                    addr: target,
                    value: val,
                    size: 8,
                    ty: TypeHint::Dynamic,
                });
                // 同时记录一个 runtime 调用（动态容器需要）。
                let _ = idx;
            }
            Assignee::Member(m) => {
                let target = self.lower_expr(&m.target);
                let field_str = self.intern_str(&m.member.name);
                self.emit(StaticInsn::RuntimeCall {
                    dst: None,
                    func: "vredrs_set_field".into(),
                    args: vec![target, Operand::ImmStr(field_str), val],
                });
            }
            Assignee::Tuple(_) => {
                // 元组解构赋值：降级为 runtime。
                self.emit(StaticInsn::RuntimeCall {
                    dst: None,
                    func: "vredrs_destructure".into(),
                    args: vec![val],
                });
            }
        }
    }

    fn lower_println(&mut self, p: &ast::PrintlnStmt) {
        // 每个 arg lower 后调用 vredrs_print / vredrs_println。
        // 区分类型：字符串用 vredrs_println_str，整数用 vredrs_println。
        for (i, arg) in p.args.iter().enumerate() {
            let v = self.lower_expr(arg);
            let is_last = i + 1 == p.args.len();
            // 根据操作数类型选择函数。
            let (func, args) = match &v {
                Operand::ImmStr(_) => {
                    let f = if is_last { "vredrs_println_str" } else { "vredrs_print_str" };
                    (f, vec![v])
                }
                Operand::ImmF64(_) => {
                    // 浮点用专用 print（接收 double，不是 i64）。
                    let f = if is_last { "vredrs_println_f64" } else { "vredrs_print_f64" };
                    (f, vec![v])
                }
                Operand::ImmI64(_) | Operand::ImmBool(_) => {
                    let f = if is_last { "vredrs_println" } else { "vredrs_print" };
                    (f, vec![v])
                }
                _ => {
                    // 动态值：根据 TypeHint 分派。
                    let f = if v.ty() == TypeHint::F64 {
                        if is_last { "vredrs_println_f64" } else { "vredrs_print_f64" }
                    } else if v.ty() == TypeHint::Str {
                        if is_last { "vredrs_println_str" } else { "vredrs_print_str" }
                    } else {
                        if is_last { "vredrs_println" } else { "vredrs_print" }
                    };
                    (f, vec![v])
                }
            };
            self.emit(StaticInsn::RuntimeCall {
                dst: None,
                func: func.into(),
                args,
            });
        }
        if p.args.is_empty() {
            self.emit(StaticInsn::RuntimeCall {
                dst: None,
                func: "vredrs_println".into(),
                args: vec![],
            });
        }
    }

    fn lower_if(&mut self, i: &ast::IfStmt) {
        let else_lbl = self.fresh_label("else");
        let end_lbl = self.fresh_label("end");
        // 条件 → 若为假跳到 else。
        self.lower_cond_branch(&i.condition, else_lbl.clone(), /*jump_if_false=*/ true);
        // then body.
        for s in &i.then_body {
            self.lower_stmt(s);
        }
        self.emit(StaticInsn::Jump { target: end_lbl.clone() });
        self.emit(StaticInsn::Label(else_lbl));
        // elif chain.
        for (cond, body) in &i.elif_chain {
            let next_else = self.fresh_label("elif_else");
            self.lower_cond_branch(cond, next_else.clone(), true);
            for s in body {
                self.lower_stmt(s);
            }
            self.emit(StaticInsn::Jump { target: end_lbl.clone() });
            self.emit(StaticInsn::Label(next_else));
        }
        if let Some(else_body) = &i.else_body {
            for s in else_body {
                self.lower_stmt(s);
            }
        }
        self.emit(StaticInsn::Label(end_lbl));
    }

    /// lower 一个条件表达式，根据 `jump_if_false` 决定跳转方向。
    /// 若条件是 `Binary(op, l, r)`，直接生成 `Branch{cond, l, r, target}`。
    fn lower_cond_branch(&mut self, cond: &Expr, target: String, jump_if_false: bool) {
        if let Expr::Binary(b) = cond {
            if let Some(c) = binop_to_cond(&b.operator) {
                let l = self.lower_expr(&b.left);
                let r = self.lower_expr(&b.right);
                let real_cond = if jump_if_false { negate_cond(c) } else { c };
                self.emit(StaticInsn::Branch {
                    cond: real_cond,
                    lhs: l,
                    rhs: r,
                    target,
                });
                return;
            }
        }
        // 通用：求值成 bool，与 true/false 比较。
        let v = self.lower_expr(cond);
        let rhs = Operand::ImmBool(false);
        let c = if jump_if_false { Cond::Eq } else { Cond::Ne };
        self.emit(StaticInsn::Branch { cond: c, lhs: v, rhs, target });
    }

    fn lower_while(&mut self, w: &ast::WhileStmt) {
        let cont_lbl = self.fresh_label("while_cont");
        let brk_lbl = self.fresh_label("while_break");
        let head_lbl = self.fresh_label("while_head");
        self.emit(StaticInsn::Label(head_lbl.clone()));
        self.lower_cond_branch(&w.condition, brk_lbl.clone(), true);
        self.loop_stack.push((cont_lbl.clone(), brk_lbl.clone()));
        for s in &w.body {
            self.lower_stmt(s);
        }
        self.loop_stack.pop();
        self.emit(StaticInsn::Label(cont_lbl));
        self.emit(StaticInsn::Jump { target: head_lbl });
        // 可选 else 块（循环正常退出时执行）。
        if let Some(else_body) = &w.else_body {
            for s in else_body {
                self.lower_stmt(s);
            }
        }
        self.emit(StaticInsn::Label(brk_lbl));
    }

    fn lower_for_range(&mut self, fr: &ast::ForRangeStmt) {
        let cont_lbl = self.fresh_label("for_cont");
        let brk_lbl = self.fresh_label("for_break");
        let head_lbl = self.fresh_label("for_head");
        let end_lbl = self.fresh_label("for_end");

        // var = from
        let from = self.lower_expr(&fr.from);
        let to = self.lower_expr(&fr.to);
        let step = match &fr.step {
            Some(s) => self.lower_expr(s),
            None => Operand::ImmI64(1),
        };
        // 循环变量类型：若 from/to 都是 I64 则 I64，否则 Dynamic。
        let var_ty = if from.ty() == TypeHint::I64 && to.ty() == TypeHint::I64 {
            TypeHint::I64
        } else {
            TypeHint::Dynamic
        };
        self.declare_local(&fr.var.name, var_ty);
        let var_reg = self.local_reg(&fr.var.name).map(|(r, _)| Operand::Reg(r, var_ty)).unwrap_or(Operand::Null);
        self.emit(StaticInsn::Mov { dst: var_reg.clone(), src: from, ty: var_ty });

        self.emit(StaticInsn::Label(head_lbl.clone()));
        // if var >= to: jump to end
        let cmp_op = if var_ty.is_static() { Cond::Ge } else { Cond::Ge };
        self.emit(StaticInsn::Branch {
            cond: cmp_op,
            lhs: var_reg.clone(),
            rhs: to.clone(),
            target: end_lbl.clone(),
        });
        self.loop_stack.push((cont_lbl.clone(), brk_lbl.clone()));
        for s in &fr.body {
            self.lower_stmt(s);
        }
        self.loop_stack.pop();
        self.emit(StaticInsn::Label(cont_lbl));
        // var += step
        let next = self.fresh_reg(var_ty);
        let op = if var_ty.is_static() { BinOp::Add } else { BinOp::AddDyn };
        self.emit(StaticInsn::Bin {
            op,
            dst: Operand::Reg(next.clone(), var_ty),
            lhs: var_reg.clone(),
            rhs: step.clone(),
            ty: var_ty,
        });
        self.emit(StaticInsn::Mov { dst: var_reg.clone(), src: Operand::Reg(next, var_ty), ty: var_ty });
        self.emit(StaticInsn::Jump { target: head_lbl });
        if let Some(else_body) = &fr.else_body {
            for s in else_body {
                self.lower_stmt(s);
            }
        }
        self.emit(StaticInsn::Label(end_lbl));
        self.emit(StaticInsn::Label(brk_lbl));
    }

    fn lower_for_in(&mut self, fi: &ast::ForInStmt) {
        // 优化：若 iterable 是 RangeExpr（如 0..100），转成静态 for-range 循环，
        // 不依赖 runtime。这让 `for, n, in, 0..100` 在新 IR 路径下原生工作。
        if let Expr::Range(r) = &fi.iterable {
            let from = match &r.start {
                Some(e) => self.lower_expr(e),
                None => Operand::ImmI64(0),
            };
            let to = match &r.end {
                Some(e) => self.lower_expr(e),
                None => Operand::ImmI64(0),
            };
            let step = match &r.step {
                Some(s) => self.lower_expr(s),
                None => Operand::ImmI64(1),
            };
            // 复用 lower_for_range 的逻辑：构造一个 ForRangeStmt 等价物。
            let var_ty = if from.ty() == TypeHint::I64 && to.ty() == TypeHint::I64 {
                TypeHint::I64
            } else {
                TypeHint::Dynamic
            };
            self.declare_local(&fi.var.name, var_ty);
            let var_reg = self.local_reg(&fi.var.name).map(|(r, _)| Operand::Reg(r, var_ty)).unwrap_or(Operand::Null);
            self.emit(StaticInsn::Mov { dst: var_reg.clone(), src: from, ty: var_ty });

            let cont_lbl = self.fresh_label("forr_cont");
            let brk_lbl = self.fresh_label("forr_break");
            let head_lbl = self.fresh_label("forr_head");
            let end_lbl = self.fresh_label("forr_end");
            self.emit(StaticInsn::Label(head_lbl.clone()));
            // 闭区间：var <= to；半开区间：var < to
            let cond = if r.inclusive { Cond::Gt } else { Cond::Ge };
            self.emit(StaticInsn::Branch { cond, lhs: var_reg.clone(), rhs: to.clone(), target: end_lbl.clone() });
            self.loop_stack.push((cont_lbl.clone(), brk_lbl.clone()));
            for s in &fi.body {
                self.lower_stmt(s);
            }
            self.loop_stack.pop();
            self.emit(StaticInsn::Label(cont_lbl));
            let next = self.fresh_reg(var_ty);
            let op = if var_ty.is_static() { BinOp::Add } else { BinOp::AddDyn };
            self.emit(StaticInsn::Bin { op, dst: Operand::Reg(next.clone(), var_ty), lhs: var_reg.clone(), rhs: step, ty: var_ty });
            self.emit(StaticInsn::Mov { dst: var_reg, src: Operand::Reg(next, var_ty), ty: var_ty });
            self.emit(StaticInsn::Jump { target: head_lbl });
            if let Some(else_body) = &fi.else_body {
                for s in else_body {
                    self.lower_stmt(s);
                }
            }
            self.emit(StaticInsn::Label(end_lbl));
            self.emit(StaticInsn::Label(brk_lbl));
            return;
        }
        // 通用 for-in：迭代容器，降级 runtime。
        let iter = self.lower_expr(&fi.iterable);
        let cont_lbl = self.fresh_label("forin_cont");
        let brk_lbl = self.fresh_label("forin_break");
        let head_lbl = self.fresh_label("forin_head");
        let end_lbl = self.fresh_label("forin_end");

        // 迭代器状态：用两个虚拟寄存器存 (容器, 索引)。
        let iter_reg = self.fresh_reg(TypeHint::Dynamic);
        self.emit(StaticInsn::Mov { dst: Operand::Reg(iter_reg.clone(), TypeHint::Dynamic), src: iter, ty: TypeHint::Dynamic });
        let idx_reg = self.fresh_reg(TypeHint::I64);
        self.emit(StaticInsn::Mov { dst: Operand::Reg(idx_reg.clone(), TypeHint::I64), src: Operand::ImmI64(0), ty: TypeHint::I64 });

        self.declare_local(&fi.var.name, TypeHint::Dynamic);
        let var_reg = self.local_reg(&fi.var.name).map(|(r, _)| Operand::Reg(r, TypeHint::Dynamic)).unwrap_or(Operand::Null);

        self.emit(StaticInsn::Label(head_lbl.clone()));
        // if idx >= len(iter): goto end
        let len_dst = self.fresh_reg(TypeHint::I64);
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(len_dst.clone(), TypeHint::I64)),
            func: "vredrs_len".into(),
            args: vec![Operand::Reg(iter_reg.clone(), TypeHint::Dynamic)],
        });
        self.emit(StaticInsn::Branch {
            cond: Cond::Ge,
            lhs: Operand::Reg(idx_reg.clone(), TypeHint::I64),
            rhs: Operand::Reg(len_dst, TypeHint::I64),
            target: end_lbl.clone(),
        });
        // var = iter[idx]
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(var_reg.clone()),
            func: "vredrs_index".into(),
            args: vec![Operand::Reg(iter_reg.clone(), TypeHint::Dynamic), Operand::Reg(idx_reg.clone(), TypeHint::I64)],
        });
        self.loop_stack.push((cont_lbl.clone(), brk_lbl.clone()));
        for s in &fi.body {
            self.lower_stmt(s);
        }
        self.loop_stack.pop();
        self.emit(StaticInsn::Label(cont_lbl));
        // idx += 1
        let next_idx = self.fresh_reg(TypeHint::I64);
        self.emit(StaticInsn::Bin {
            op: BinOp::Add,
            dst: Operand::Reg(next_idx.clone(), TypeHint::I64),
            lhs: Operand::Reg(idx_reg.clone(), TypeHint::I64),
            rhs: Operand::ImmI64(1),
            ty: TypeHint::I64,
        });
        self.emit(StaticInsn::Mov { dst: Operand::Reg(idx_reg, TypeHint::I64), src: Operand::Reg(next_idx, TypeHint::I64), ty: TypeHint::I64 });
        self.emit(StaticInsn::Jump { target: head_lbl });
        if let Some(else_body) = &fi.else_body {
            for s in else_body {
                self.lower_stmt(s);
            }
        }
        self.emit(StaticInsn::Label(end_lbl));
        self.emit(StaticInsn::Label(brk_lbl));
    }

    fn lower_loop(&mut self, l: &ast::LoopStmt) {
        // loop { ... } — 无条件循环，只能 break 退出。手册 §3.3 要求 loop
        // 有显式 arm，不再静默跳过。
        let cont_lbl = self.fresh_label("loop_cont");
        let brk_lbl = self.fresh_label("loop_break");
        let head_lbl = self.fresh_label("loop_head");
        self.emit(StaticInsn::Label(head_lbl.clone()));
        self.loop_stack.push((cont_lbl.clone(), brk_lbl.clone()));
        for s in &l.body {
            self.lower_stmt(s);
        }
        self.loop_stack.pop();
        self.emit(StaticInsn::Label(cont_lbl));
        self.emit(StaticInsn::Jump { target: head_lbl });
        if let Some(else_body) = &l.else_body {
            // loop 的 else 块在 break 时执行（语义上少见，这里跟随 VM 行为）。
            for s in else_body {
                self.lower_stmt(s);
            }
        }
        self.emit(StaticInsn::Label(brk_lbl));
    }

    fn lower_match(&mut self, m: &ast::MatchStmt) {
        // match：求值 scrutinee，然后对每个 case 做相等性比较。
        // 修复 stage3：label 顺序错误（之前 Label(case_labels[i]) 被错误地放在
        // case i 的 Jump 之后，导致跳转目标错位）。现按"先 Label，再 Branch/body"
        // 的标准顺序排列。
        let scrut = self.lower_expr(&m.expr);
        let end_lbl = self.fresh_label("match_end");
        let n = m.cases.len();
        let mut case_labels = Vec::with_capacity(n);
        for i in 0..n {
            case_labels.push(self.fresh_label(&format!("match_c{}", i)));
        }
        let else_lbl = self.fresh_label("match_else");

        for (i, case) in m.cases.iter().enumerate() {
            // 每个 case 入口先发自己的 label，使上一 case 的"不匹配跳转"能落到此。
            self.emit(StaticInsn::Label(case_labels[i].clone()));
            match &case.pattern {
                Pattern::Literal(lp) => {
                    let lit = self.lower_expr(&lp.literal);
                    let skip = if i + 1 < n {
                        case_labels[i + 1].clone()
                    } else {
                        else_lbl.clone()
                    };
                    self.emit(StaticInsn::Branch {
                        cond: Cond::Ne,
                        lhs: scrut.clone(),
                        rhs: lit,
                        target: skip,
                    });
                    for s in &case.body {
                        self.lower_stmt(s);
                    }
                    self.emit(StaticInsn::Jump { target: end_lbl.clone() });
                }
                Pattern::Binding(bp) => {
                    // 绑定模式：把 scrut 赋给绑定变量（始终匹配）。
                    self.declare_local(&bp.name.name, scrut.ty());
                    if let Some((reg, ty)) = self.local_reg(&bp.name.name) {
                        self.emit(StaticInsn::Mov { dst: Operand::Reg(reg, ty), src: scrut.clone(), ty });
                    }
                    for s in &case.body {
                        self.lower_stmt(s);
                    }
                    self.emit(StaticInsn::Jump { target: end_lbl.clone() });
                }
                Pattern::Wildcard(_) => {
                    for s in &case.body {
                        self.lower_stmt(s);
                    }
                    self.emit(StaticInsn::Jump { target: end_lbl.clone() });
                }
                Pattern::Tuple(tp) => {
                    // 元组模式：(a, b) → 检查 scrutinee 长度 == tp.elements.len()
                    // 然后绑定每个子模式。
                    let n = tp.elements.len() as i64;
                    let len_reg = self.fresh_reg(TypeHint::Dynamic);
                    self.emit(StaticInsn::RuntimeCall {
                        dst: Some(Operand::Reg(len_reg.clone(), TypeHint::Dynamic)),
                        func: "vredrs_len".into(),
                        args: vec![scrut.clone()],
                    });
                    let skip = self.fresh_label("match_tuple_skip");
                    self.emit(StaticInsn::Branch {
                        cond: Cond::Ne,
                        lhs: Operand::Reg(len_reg, TypeHint::Dynamic),
                        rhs: Operand::ImmI64(n),
                        target: skip.clone(),
                    });
                    // 绑定子模式（简化：只支持 Binding 和 Wildcard 子模式）。
                    for (j, sub) in tp.elements.iter().enumerate() {
                        match sub {
                            Pattern::Binding(bp) => {
                                let elem = self.fresh_reg(TypeHint::Dynamic);
                                self.emit(StaticInsn::RuntimeCall {
                                    dst: Some(Operand::Reg(elem.clone(), TypeHint::Dynamic)),
                                    func: "vredrs_index".into(),
                                    args: vec![scrut.clone(), Operand::ImmI64(j as i64)],
                                });
                                self.declare_local(&bp.name.name, TypeHint::Dynamic);
                                if let Some((reg, _)) = self.local_reg(&bp.name.name) {
                                    self.emit(StaticInsn::Mov {
                                        dst: Operand::Reg(reg, TypeHint::Dynamic),
                                        src: Operand::Reg(elem, TypeHint::Dynamic),
                                        ty: TypeHint::Dynamic,
                                    });
                                }
                            }
                            Pattern::Wildcard(_) => {}
                            _ => {}
                        }
                    }
                    for s in &case.body {
                        self.lower_stmt(s);
                    }
                    self.emit(StaticInsn::Jump { target: end_lbl.clone() });
                    self.emit(StaticInsn::Label(skip));
                }
                Pattern::List(lp) => {
                    // 列表模式：检查长度，绑定元素。
                    let n = lp.elements.len() as i64;
                    let len_reg = self.fresh_reg(TypeHint::Dynamic);
                    self.emit(StaticInsn::RuntimeCall {
                        dst: Some(Operand::Reg(len_reg.clone(), TypeHint::Dynamic)),
                        func: "vredrs_len".into(),
                        args: vec![scrut.clone()],
                    });
                    let skip = self.fresh_label("match_list_skip");
                    self.emit(StaticInsn::Branch {
                        cond: Cond::Ne,
                        lhs: Operand::Reg(len_reg, TypeHint::Dynamic),
                        rhs: Operand::ImmI64(n),
                        target: skip.clone(),
                    });
                    for (j, sub) in lp.elements.iter().enumerate() {
                        if let Pattern::Binding(bp) = sub {
                            let elem = self.fresh_reg(TypeHint::Dynamic);
                            self.emit(StaticInsn::RuntimeCall {
                                dst: Some(Operand::Reg(elem.clone(), TypeHint::Dynamic)),
                                func: "vredrs_index".into(),
                                args: vec![scrut.clone(), Operand::ImmI64(j as i64)],
                            });
                            self.declare_local(&bp.name.name, TypeHint::Dynamic);
                            if let Some((reg, _)) = self.local_reg(&bp.name.name) {
                                self.emit(StaticInsn::Mov {
                                    dst: Operand::Reg(reg, TypeHint::Dynamic),
                                    src: Operand::Reg(elem, TypeHint::Dynamic),
                                    ty: TypeHint::Dynamic,
                                });
                            }
                        }
                    }
                    for s in &case.body {
                        self.lower_stmt(s);
                    }
                    self.emit(StaticInsn::Jump { target: end_lbl.clone() });
                    self.emit(StaticInsn::Label(skip));
                }
                Pattern::Or(op) => {
                    // Or 模式：a | b → 尝试第一个，失败则尝试第二个。
                    // 简化：始终匹配（fall-through）。
                    self.emit(StaticInsn::Comment(format!("or pattern case {}", i)));
                    for s in &case.body {
                        self.lower_stmt(s);
                    }
                    self.emit(StaticInsn::Jump { target: end_lbl.clone() });
                }
                _ => {
                    // 其他复杂模式（Dict/Struct/EnumVariant）：
                    // 降级为 fall-through（保守匹配）。
                    self.emit(StaticInsn::Comment(format!("complex pattern case {}", i)));
                    for s in &case.body {
                        self.lower_stmt(s);
                    }
                    self.emit(StaticInsn::Jump { target: end_lbl.clone() });
                }
            }
        }
        self.emit(StaticInsn::Label(else_lbl));
        if let Some(else_body) = &m.else_case {
            for s in else_body {
                self.lower_stmt(s);
            }
        }
        self.emit(StaticInsn::Label(end_lbl));
    }

    fn lower_try(&mut self, t: &ast::TryStmt) {
        // try/catch/finally：flag-based 异常协议。
        // 协议：vredrs_push_catch(catch_label) → 0=正常 / 1=从 throw 回来。
        //       try body 执行后，vredrs_check_throw() 返回是否有未处理的异常，
        //       若有则跳到 catch_label。try 块正常结束调 vredrs_pop_catch 弹出。
        let catch_lbl = self.fresh_label("catch");
        let end_lbl = self.fresh_label("try_end");
        // 注册 catch handler（setjmp 风格：push_catch 也可直接 longjmp 回来）。
        self.emit(StaticInsn::RuntimeCall {
            dst: None,
            func: "vredrs_push_catch".into(),
            args: vec![Operand::Label(catch_lbl.clone())],
        });
        // try body.
        for s in &t.try_body {
            self.lower_stmt(s);
        }
        // 检查 try body 末尾是否有挂起的异常（flag-based 路径）。
        let threw = self.fresh_reg(TypeHint::Bool);
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(threw.clone(), TypeHint::Bool)),
            func: "vredrs_check_throw".into(),
            args: vec![],
        });
        self.emit(StaticInsn::Branch {
            cond: Cond::Ne,
            lhs: Operand::Reg(threw, TypeHint::Bool),
            rhs: Operand::ImmBool(false),
            target: catch_lbl.clone(),
        });
        // 正常完成：弹出 catch handler。
        self.emit(StaticInsn::RuntimeCall {
            dst: None,
            func: "vredrs_pop_catch".into(),
            args: vec![],
        });
        self.emit(StaticInsn::Jump { target: end_lbl.clone() });
        // catch handler.
        self.emit(StaticInsn::Label(catch_lbl));
        // catch 进入时也要 pop 之前的 catch frame（避免栈累积）。
        self.emit(StaticInsn::RuntimeCall {
            dst: None,
            func: "vredrs_pop_catch".into(),
            args: vec![],
        });
        if let (Some(var), Some(body)) = (&t.catch_var, &t.catch_body) {
            let exc = self.fresh_reg(TypeHint::Dynamic);
            self.emit(StaticInsn::RuntimeCall {
                dst: Some(Operand::Reg(exc.clone(), TypeHint::Dynamic)),
                func: "vredrs_get_exception".into(),
                args: vec![],
            });
            self.declare_local(&var.name, TypeHint::Dynamic);
            if let Some((reg, _)) = self.local_reg(&var.name) {
                self.emit(StaticInsn::Mov { dst: Operand::Reg(reg, TypeHint::Dynamic), src: Operand::Reg(exc, TypeHint::Dynamic), ty: TypeHint::Dynamic });
            }
            for s in body {
                self.lower_stmt(s);
            }
        }
        self.emit(StaticInsn::Label(end_lbl));
        if let Some(fin) = &t.finally_body {
            self.emit(StaticInsn::Comment("finally".into()));
            for s in fin {
                self.lower_stmt(s);
            }
        }
    }

    fn lower_asm(&mut self, a: &ast::AsmStmt) {
        // 保留 GCC 约束信息（手册 §3.3 完成标准）。
        let inputs: Vec<crate::codegen::ir::AsmOperand> = a
            .inputs
            .iter()
            .map(|op| crate::codegen::ir::AsmOperand {
                constraint: op.constraint.clone(),
                operand: self.lower_expr(&op.expr),
            })
            .collect();
        let outputs: Vec<crate::codegen::ir::AsmOperand> = a
            .outputs
            .iter()
            .map(|op| crate::codegen::ir::AsmOperand {
                constraint: op.constraint.clone(),
                operand: self.lower_expr(&op.expr),
            })
            .collect();
        self.emit(StaticInsn::Asm {
            template: a.template.clone(),
            inputs,
            outputs,
            clobbers: vec!["memory".into(), "cc".into()],
        });
    }

    // ── 表达式 lowering ────────────────────────────────────

    fn lower_expr(&mut self, e: &Expr) -> Operand {
        match e {
            Expr::Integer(i) => Operand::ImmI64(i.value),
            Expr::Float(f) => Operand::ImmF64(f.value),
            Expr::Bool(b) => Operand::ImmBool(b.value),
            Expr::Null(_) => Operand::Null,
            Expr::String_(s) => {
                // 字符串字面量：可能含插值。
                // stage3：若有插值部分，调用 lower_string_interp 生成 runtime 拼接。
                if s.parts.iter().any(|p| matches!(p, ast::StringPart::Interpolation(_))) {
                    return self.lower_string_interp(&s.parts);
                }
                let text = flatten_string_parts(&s.parts);
                let idx = self.intern_str(&text);
                Operand::ImmStr(idx)
            }
            Expr::MultiLineString(s) => {
                if s.parts.iter().any(|p| matches!(p, ast::StringPart::Interpolation(_))) {
                    return self.lower_string_interp(&s.parts);
                }
                let text = flatten_string_parts(&s.parts);
                let idx = self.intern_str(&text);
                Operand::ImmStr(idx)
            }
            Expr::Identifier(id) => {
                // 局部变量或全局符号。
                if let Some((reg, ty)) = self.local_reg(&id.name) {
                    Operand::Reg(reg, ty)
                } else {
                    // 全局：通过 runtime 读取。
                    let dst = self.fresh_reg(TypeHint::Dynamic);
                    let name_idx = self.intern_str(&id.name);
                    self.emit(StaticInsn::RuntimeCall {
                        dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                        func: "vredrs_load_global".into(),
                        args: vec![Operand::ImmStr(name_idx)],
                    });
                    Operand::Reg(dst, TypeHint::Dynamic)
                }
            }
            Expr::Qualified(q) => {
                // a.b.c → runtime get_field。
                let path: String = q.parts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(".");
                let idx = self.intern_str(&path);
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_get_field".into(),
                    args: vec![Operand::ImmStr(idx)],
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::Binary(b) => self.lower_binary(b),
            Expr::Unary(u) => self.lower_unary(u),
            Expr::Call(c) => self.lower_call(c),
            Expr::MethodCall(mc) => self.lower_method_call(mc),
            Expr::Index(ix) => {
                let target = self.lower_expr(&ix.target);
                let idx = self.lower_expr(&ix.index);
                // stage3：字符串字面量/变量 key → vredrs_get_field（dict 语义）。
                // 整数 key → vredrs_index（list 语义）。
                let is_str_key = match &*ix.index {
                    Expr::String_(_) | Expr::MultiLineString(_) => true,
                    Expr::Identifier(id) => self.var_str.contains(&id.name),
                    _ => false,
                };
                let dst = self.fresh_reg(TypeHint::Dynamic);
                let func = if is_str_key { "vredrs_get_field" } else { "vredrs_index" };
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: func.into(),
                    args: vec![target, idx],
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::MemberAccess(m) => {
                let target = self.lower_expr(&m.target);
                let field_idx = self.intern_str(&m.member.name);
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_get_field".into(),
                    args: vec![target, Operand::ImmStr(field_idx)],
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::Ternary(t) => {
                // cond ? a : b → 分支 + phi。
                let else_lbl = self.fresh_label("ternary_else");
                let end_lbl = self.fresh_label("ternary_end");
                self.lower_cond_branch(&t.condition, else_lbl.clone(), true);
                let true_val = self.lower_expr(&t.true_branch);
                let true_ty = true_val.ty();
                let result = self.fresh_reg(true_ty);
                self.emit(StaticInsn::Mov { dst: Operand::Reg(result.clone(), true_ty), src: true_val, ty: true_ty });
                self.emit(StaticInsn::Jump { target: end_lbl.clone() });
                self.emit(StaticInsn::Label(else_lbl));
                let false_val = self.lower_expr(&t.false_branch);
                let false_ty = false_val.ty();
                self.emit(StaticInsn::Mov { dst: Operand::Reg(result.clone(), false_ty), src: false_val, ty: false_ty });
                self.emit(StaticInsn::Label(end_lbl));
                Operand::Reg(result, TypeHint::Mixed)
            }
            Expr::NullCoalesce(nc) => {
                let left = self.lower_expr(&nc.left);
                let left_ty = left.ty();
                let else_lbl = self.fresh_label("nc_else");
                let end_lbl = self.fresh_label("nc_end");
                self.emit(StaticInsn::Branch {
                    cond: Cond::Eq,
                    lhs: left.clone(),
                    rhs: Operand::Null,
                    target: else_lbl.clone(),
                });
                let result = self.fresh_reg(left_ty);
                self.emit(StaticInsn::Mov { dst: Operand::Reg(result.clone(), left_ty), src: left, ty: left_ty });
                self.emit(StaticInsn::Jump { target: end_lbl.clone() });
                self.emit(StaticInsn::Label(else_lbl));
                let right = self.lower_expr(&nc.right);
                let right_ty = right.ty();
                self.emit(StaticInsn::Mov { dst: Operand::Reg(result.clone(), right_ty), src: right, ty: right_ty });
                self.emit(StaticInsn::Label(end_lbl));
                Operand::Reg(result, TypeHint::Mixed)
            }
            Expr::Cast(c) => {
                // as 类型转换：若目标是静态类型，生成 Unbox；否则 Box。
                let v = self.lower_expr(&c.expr);
                let v_ty = v.ty();
                let target_ty = type_expr_to_hint(&c.type_expr);
                let dst = self.fresh_reg(target_ty);
                if v_ty != target_ty {
                    if target_ty.is_static() {
                        self.emit(StaticInsn::Unbox { dst: Operand::Reg(dst.clone(), target_ty), src: v, to: target_ty });
                    } else {
                        self.emit(StaticInsn::Box { dst: Operand::Reg(dst.clone(), target_ty), src: v, from: v_ty });
                    }
                } else {
                    self.emit(StaticInsn::Mov { dst: Operand::Reg(dst.clone(), target_ty), src: v, ty: target_ty });
                }
                Operand::Reg(dst, target_ty)
            }
            Expr::List(l) => {
                // 列表字面量：runtime 构造。第一个参数是元素数 n。
                let n = l.elements.len() as i64;
                let mut args = vec![Operand::ImmI64(n)];
                for el in &l.elements {
                    args.push(self.lower_expr(el));
                }
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_make_list".into(),
                    args,
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::Dict(d) => {
                let n = d.entries.len() as i64;
                let mut args = vec![Operand::ImmI64(n)];
                for (k, v) in &d.entries {
                    args.push(self.lower_expr(k));
                    args.push(self.lower_expr(v));
                }
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_make_dict".into(),
                    args,
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::Set(s) => {
                let n = s.elements.len() as i64;
                let mut args = vec![Operand::ImmI64(n)];
                for el in &s.elements {
                    args.push(self.lower_expr(el));
                }
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_make_set".into(),
                    args,
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::Tuple(t) => {
                let n = t.elements.len() as i64;
                let mut args = vec![Operand::ImmI64(n)];
                for el in &t.elements {
                    args.push(self.lower_expr(el));
                }
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_make_tuple".into(),
                    args,
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::Range(r) => {
                let start = match &r.start {
                    Some(e) => self.lower_expr(e),
                    None => Operand::ImmI64(0),
                };
                let end = match &r.end {
                    Some(e) => self.lower_expr(e),
                    None => Operand::ImmI64(0),
                };
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_make_range".into(),
                    args: vec![start, end, Operand::ImmBool(r.inclusive)],
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::Lambda(l) => {
                // stage3：lambda → lower_lambda_function 生成 `__lambda_N` 函数 +
                // vredrs_make_captures + vredrs_make_closure(fn_ptr, captures_ptr)。
                self.lower_lambda_function(l)
            }
            Expr::Spread(s) => {
                // 展开运算符：透传内部值（runtime 在调用点处理）。
                self.lower_expr(&s.expr)
            }
            Expr::Pipe(p) => {
                // a |> f → f(a)
                let left = self.lower_expr(&p.left);
                let right = self.lower_expr(&p.right);
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_pipe".into(),
                    args: vec![left, right],
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::Slice(sl) => {
                let target = self.lower_expr(&sl.target);
                let start = match &sl.start {
                    Some(e) => self.lower_expr(e),
                    None => Operand::ImmI64(0),
                };
                let end = match &sl.end {
                    Some(e) => self.lower_expr(e),
                    None => Operand::ImmI64(-1),
                };
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_slice".into(),
                    args: vec![target, start, end],
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::OptionalChain(oc) => {
                let target = self.lower_expr(&oc.target);
                let mut cur = target;
                for link in &oc.chain {
                    let dst = self.fresh_reg(TypeHint::Dynamic);
                    match link {
                        ast::OptionalChainLink::Member(id) => {
                            let f = self.intern_str(&id.name);
                            self.emit(StaticInsn::RuntimeCall {
                                dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                                func: "vredrs_optional_member".into(),
                                args: vec![cur, Operand::ImmStr(f)],
                            });
                        }
                        ast::OptionalChainLink::Call { method, args } => {
                            let m = self.intern_str(&method.name);
                            let mut a = vec![cur, Operand::ImmStr(m)];
                            for x in args {
                                a.push(self.lower_expr(x));
                            }
                            self.emit(StaticInsn::RuntimeCall {
                                dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                                func: "vredrs_optional_method".into(),
                                args: a,
                            });
                        }
                        ast::OptionalChainLink::Index(idx) => {
                            let i = self.lower_expr(idx);
                            self.emit(StaticInsn::RuntimeCall {
                                dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                                func: "vredrs_optional_index".into(),
                                args: vec![cur, i],
                            });
                        }
                    }
                    cur = Operand::Reg(dst, TypeHint::Dynamic);
                }
                cur
            }
            Expr::Postfix(p) => {
                let v = self.lower_expr(&p.operand);
                let dst = self.fresh_reg(TypeHint::Dynamic);
                // stage3：str_len 用于字符串，vredrs_len 用于 list/dict。
                let func = match p.operator {
                    ast::PostfixOp::Length => {
                        if self.infer_is_string(&p.operand) || v.ty() == TypeHint::Str {
                            "str_len"
                        } else {
                            "vredrs_len"
                        }
                    }
                    ast::PostfixOp::Reverse => "vredrs_reverse",
                    ast::PostfixOp::AscSort => "vredrs_sort_asc",
                    ast::PostfixOp::DescSort => "vredrs_sort_desc",
                };
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: func.into(),
                    args: vec![v],
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::TryPropagate(tp) => {
                let v = self.lower_expr(&tp.expr);
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_try_propagate".into(),
                    args: vec![v],
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::Await(a) => {
                let v = self.lower_expr(&a.expr);
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_await".into(),
                    args: vec![v],
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::Spawn(s) => {
                let v = self.lower_expr(&s.call);
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_spawn".into(),
                    args: vec![v],
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::Coro(c) => {
                let f = self.lower_expr(&c.function);
                let mut args = vec![f];
                for a in &c.args {
                    args.push(self.lower_expr(a));
                }
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_coro".into(),
                    args,
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::Resume(r) => {
                let h = self.lower_expr(&r.handle);
                let mut args = vec![h];
                for v in &r.values {
                    args.push(self.lower_expr(v));
                }
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_resume".into(),
                    args,
                });
                Operand::Reg(dst, TypeHint::Dynamic)
            }
            Expr::AssignExpr(a) => {
                let v = self.lower_expr(&a.value);
                self.store_to_assignee(&a.targets[0], v.clone());
                v
            }
            Expr::ListComprehension(lc) => {
                // stage3：展开成显式循环。生成空 list，遍历 iterable，
                // 满足 condition 则 push result_expr。
                return self.lower_list_comprehension(lc);
            }
            Expr::DictComprehension(dc) => {
                return self.lower_dict_comprehension(dc);
            }
            Expr::SetComprehension(sc) => {
                return self.lower_set_comprehension(sc);
            }
        }
    }

    /// 把 list comprehension `[expr for x in iter if cond]` 展开成显式循环。
    fn lower_list_comprehension(&mut self, lc: &ast::ListComprehension) -> Operand {
        self.emit(StaticInsn::Comment("list comprehension".into()));
        // 构造空 list。
        let list_dst = self.fresh_reg(TypeHint::Dynamic);
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(list_dst.clone(), TypeHint::Dynamic)),
            func: "vredrs_make_list".into(),
            args: vec![Operand::ImmI64(0)],
        });
        // 迭代器。
        let iter = self.lower_expr(&lc.iterable);
        let iter_reg = self.fresh_reg(TypeHint::Dynamic);
        self.emit(StaticInsn::Mov {
            dst: Operand::Reg(iter_reg.clone(), TypeHint::Dynamic),
            src: iter,
            ty: TypeHint::Dynamic,
        });
        let idx_reg = self.fresh_reg(TypeHint::I64);
        self.emit(StaticInsn::Mov {
            dst: Operand::Reg(idx_reg.clone(), TypeHint::I64),
            src: Operand::ImmI64(0),
            ty: TypeHint::I64,
        });
        self.declare_local(&lc.var.name, TypeHint::Dynamic);
        let var_reg = self.local_reg(&lc.var.name).map(|(r, _)| r).unwrap_or_else(|| "_".into());
        let head_lbl = self.fresh_label("lc_head");
        let end_lbl = self.fresh_label("lc_end");
        self.emit(StaticInsn::Label(head_lbl.clone()));
        let len_dst = self.fresh_reg(TypeHint::I64);
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(len_dst.clone(), TypeHint::I64)),
            func: "vredrs_len".into(),
            args: vec![Operand::Reg(iter_reg.clone(), TypeHint::Dynamic)],
        });
        self.emit(StaticInsn::Branch {
            cond: Cond::Ge,
            lhs: Operand::Reg(idx_reg.clone(), TypeHint::I64),
            rhs: Operand::Reg(len_dst, TypeHint::I64),
            target: end_lbl.clone(),
        });
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(var_reg.clone(), TypeHint::Dynamic)),
            func: "vredrs_index".into(),
            args: vec![Operand::Reg(iter_reg.clone(), TypeHint::Dynamic), Operand::Reg(idx_reg.clone(), TypeHint::I64)],
        });
        // 可选 condition。
        if let Some(cond) = &lc.condition {
            let cont_lbl = self.fresh_label("lc_cont");
            self.lower_cond_branch(cond, cont_lbl.clone(), true);
            let val = self.lower_expr(&lc.result_expr);
            let new_list = self.fresh_reg(TypeHint::Dynamic);
            self.emit(StaticInsn::RuntimeCall {
                dst: Some(Operand::Reg(new_list.clone(), TypeHint::Dynamic)),
                func: "vredrs_list_append".into(),
                args: vec![Operand::Reg(list_dst.clone(), TypeHint::Dynamic), val],
            });
            self.emit(StaticInsn::Mov { dst: Operand::Reg(list_dst.clone(), TypeHint::Dynamic), src: Operand::Reg(new_list, TypeHint::Dynamic), ty: TypeHint::Dynamic });
            self.emit(StaticInsn::Label(cont_lbl));
        } else {
            let val = self.lower_expr(&lc.result_expr);
            let new_list = self.fresh_reg(TypeHint::Dynamic);
            self.emit(StaticInsn::RuntimeCall {
                dst: Some(Operand::Reg(new_list.clone(), TypeHint::Dynamic)),
                func: "vredrs_list_append".into(),
                args: vec![Operand::Reg(list_dst.clone(), TypeHint::Dynamic), val],
            });
            self.emit(StaticInsn::Mov { dst: Operand::Reg(list_dst.clone(), TypeHint::Dynamic), src: Operand::Reg(new_list, TypeHint::Dynamic), ty: TypeHint::Dynamic });
        }
        // idx += 1
        let next_idx = self.fresh_reg(TypeHint::I64);
        self.emit(StaticInsn::Bin {
            op: BinOp::Add,
            dst: Operand::Reg(next_idx.clone(), TypeHint::I64),
            lhs: Operand::Reg(idx_reg.clone(), TypeHint::I64),
            rhs: Operand::ImmI64(1),
            ty: TypeHint::I64,
        });
        self.emit(StaticInsn::Mov {
            dst: Operand::Reg(idx_reg, TypeHint::I64),
            src: Operand::Reg(next_idx, TypeHint::I64),
            ty: TypeHint::I64,
        });
        self.emit(StaticInsn::Jump { target: head_lbl });
        self.emit(StaticInsn::Label(end_lbl));
        Operand::Reg(list_dst, TypeHint::Dynamic)
    }

    /// 把 dict comprehension `{k: v for x in iter if cond}` 展开成显式循环。
    fn lower_dict_comprehension(&mut self, dc: &ast::DictComprehension) -> Operand {
        self.emit(StaticInsn::Comment("dict comprehension".into()));
        let dict_dst = self.fresh_reg(TypeHint::Dynamic);
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(dict_dst.clone(), TypeHint::Dynamic)),
            func: "vredrs_make_dict".into(),
            args: vec![Operand::ImmI64(0)],
        });
        let iter = self.lower_expr(&dc.iterable);
        let iter_reg = self.fresh_reg(TypeHint::Dynamic);
        self.emit(StaticInsn::Mov {
            dst: Operand::Reg(iter_reg.clone(), TypeHint::Dynamic),
            src: iter,
            ty: TypeHint::Dynamic,
        });
        let idx_reg = self.fresh_reg(TypeHint::I64);
        self.emit(StaticInsn::Mov {
            dst: Operand::Reg(idx_reg.clone(), TypeHint::I64),
            src: Operand::ImmI64(0),
            ty: TypeHint::I64,
        });
        self.declare_local(&dc.var.name, TypeHint::Dynamic);
        let var_reg = self.local_reg(&dc.var.name).map(|(r, _)| r).unwrap_or_else(|| "_".into());
        let head_lbl = self.fresh_label("dc_head");
        let end_lbl = self.fresh_label("dc_end");
        self.emit(StaticInsn::Label(head_lbl.clone()));
        let len_dst = self.fresh_reg(TypeHint::I64);
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(len_dst.clone(), TypeHint::I64)),
            func: "vredrs_len".into(),
            args: vec![Operand::Reg(iter_reg.clone(), TypeHint::Dynamic)],
        });
        self.emit(StaticInsn::Branch {
            cond: Cond::Ge,
            lhs: Operand::Reg(idx_reg.clone(), TypeHint::I64),
            rhs: Operand::Reg(len_dst, TypeHint::I64),
            target: end_lbl.clone(),
        });
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(var_reg.clone(), TypeHint::Dynamic)),
            func: "vredrs_index".into(),
            args: vec![Operand::Reg(iter_reg.clone(), TypeHint::Dynamic), Operand::Reg(idx_reg.clone(), TypeHint::I64)],
        });
        let cond_skip_lbl = if let Some(cond) = &dc.condition {
            let skip = self.fresh_label("dc_skip");
            self.lower_cond_branch(cond, skip.clone(), true);
            Some(skip)
        } else {
            None
        };
        let key = self.lower_expr(&dc.key_expr);
        let val = self.lower_expr(&dc.value_expr);
        self.emit(StaticInsn::RuntimeCall {
            dst: None,
            func: "vredrs_set_field".into(),
            args: vec![Operand::Reg(dict_dst.clone(), TypeHint::Dynamic), key, val],
        });
        if let Some(skip) = cond_skip_lbl {
            self.emit(StaticInsn::Label(skip));
        }
        let next_idx = self.fresh_reg(TypeHint::I64);
        self.emit(StaticInsn::Bin {
            op: BinOp::Add,
            dst: Operand::Reg(next_idx.clone(), TypeHint::I64),
            lhs: Operand::Reg(idx_reg.clone(), TypeHint::I64),
            rhs: Operand::ImmI64(1),
            ty: TypeHint::I64,
        });
        self.emit(StaticInsn::Mov {
            dst: Operand::Reg(idx_reg, TypeHint::I64),
            src: Operand::Reg(next_idx, TypeHint::I64),
            ty: TypeHint::I64,
        });
        self.emit(StaticInsn::Jump { target: head_lbl });
        self.emit(StaticInsn::Label(end_lbl));
        Operand::Reg(dict_dst, TypeHint::Dynamic)
    }

    /// 把 set comprehension `{expr for x in iter if cond}` 展开成显式循环。
    fn lower_set_comprehension(&mut self, sc: &ast::SetComprehension) -> Operand {
        self.emit(StaticInsn::Comment("set comprehension".into()));
        let set_dst = self.fresh_reg(TypeHint::Dynamic);
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(set_dst.clone(), TypeHint::Dynamic)),
            func: "vredrs_make_set".into(),
            args: vec![Operand::ImmI64(0)],
        });
        let iter = self.lower_expr(&sc.iterable);
        let iter_reg = self.fresh_reg(TypeHint::Dynamic);
        self.emit(StaticInsn::Mov {
            dst: Operand::Reg(iter_reg.clone(), TypeHint::Dynamic),
            src: iter,
            ty: TypeHint::Dynamic,
        });
        let idx_reg = self.fresh_reg(TypeHint::I64);
        self.emit(StaticInsn::Mov {
            dst: Operand::Reg(idx_reg.clone(), TypeHint::I64),
            src: Operand::ImmI64(0),
            ty: TypeHint::I64,
        });
        self.declare_local(&sc.var.name, TypeHint::Dynamic);
        let var_reg = self.local_reg(&sc.var.name).map(|(r, _)| r).unwrap_or_else(|| "_".into());
        let head_lbl = self.fresh_label("sc_head");
        let end_lbl = self.fresh_label("sc_end");
        self.emit(StaticInsn::Label(head_lbl.clone()));
        let len_dst = self.fresh_reg(TypeHint::I64);
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(len_dst.clone(), TypeHint::I64)),
            func: "vredrs_len".into(),
            args: vec![Operand::Reg(iter_reg.clone(), TypeHint::Dynamic)],
        });
        self.emit(StaticInsn::Branch {
            cond: Cond::Ge,
            lhs: Operand::Reg(idx_reg.clone(), TypeHint::I64),
            rhs: Operand::Reg(len_dst, TypeHint::I64),
            target: end_lbl.clone(),
        });
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(var_reg.clone(), TypeHint::Dynamic)),
            func: "vredrs_index".into(),
            args: vec![Operand::Reg(iter_reg.clone(), TypeHint::Dynamic), Operand::Reg(idx_reg.clone(), TypeHint::I64)],
        });
        if let Some(cond) = &sc.condition {
            let skip = self.fresh_label("sc_skip");
            self.lower_cond_branch(cond, skip.clone(), true);
            let val = self.lower_expr(&sc.result_expr);
            self.emit(StaticInsn::RuntimeCall {
                dst: None,
                func: "vredrs_set_add".into(),
                args: vec![Operand::Reg(set_dst.clone(), TypeHint::Dynamic), val],
            });
            self.emit(StaticInsn::Label(skip));
        } else {
            let val = self.lower_expr(&sc.result_expr);
            self.emit(StaticInsn::RuntimeCall {
                dst: None,
                func: "vredrs_set_add".into(),
                args: vec![Operand::Reg(set_dst.clone(), TypeHint::Dynamic), val],
            });
        }
        let next_idx = self.fresh_reg(TypeHint::I64);
        self.emit(StaticInsn::Bin {
            op: BinOp::Add,
            dst: Operand::Reg(next_idx.clone(), TypeHint::I64),
            lhs: Operand::Reg(idx_reg.clone(), TypeHint::I64),
            rhs: Operand::ImmI64(1),
            ty: TypeHint::I64,
        });
        self.emit(StaticInsn::Mov {
            dst: Operand::Reg(idx_reg, TypeHint::I64),
            src: Operand::Reg(next_idx, TypeHint::I64),
            ty: TypeHint::I64,
        });
        self.emit(StaticInsn::Jump { target: head_lbl });
        self.emit(StaticInsn::Label(end_lbl));
        Operand::Reg(set_dst, TypeHint::Dynamic)
    }

    fn lower_binary(&mut self, b: &ast::BinaryExpr) -> Operand {
        // stage3：先尝试字符串静态分派（str_concat/str_repeat/str_eq/str_ne）。
        // 检查时机：在 lower 子表达式之前，因为需要看 AST 结构（不是操作数类型）。
        let left_is_str = self.infer_is_string(&b.left);
        let right_is_str = self.infer_is_string(&b.right);
        let lhs = self.lower_expr(&b.left);
        let rhs = self.lower_expr(&b.right);
        // stage3：字符串相关运算的静态分派。
        match b.operator {
            BinaryOp::Add if left_is_str || right_is_str || lhs.ty() == TypeHint::Str || rhs.ty() == TypeHint::Str => {
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "str_concat".into(),
                    args: vec![lhs, rhs],
                });
                return Operand::Reg(dst, TypeHint::Str);
            }
            BinaryOp::Mul if left_is_str || lhs.ty() == TypeHint::Str => {
                // str * n → str_repeat
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "str_repeat".into(),
                    args: vec![lhs, rhs],
                });
                return Operand::Reg(dst, TypeHint::Str);
            }
            BinaryOp::Eq if left_is_str || right_is_str || lhs.ty() == TypeHint::Str || rhs.ty() == TypeHint::Str => {
                let dst = self.fresh_reg(TypeHint::Bool);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Bool)),
                    func: "str_eq".into(),
                    args: vec![lhs, rhs],
                });
                return Operand::Reg(dst, TypeHint::Bool);
            }
            BinaryOp::Ne if left_is_str || right_is_str || lhs.ty() == TypeHint::Str || rhs.ty() == TypeHint::Str => {
                let dst = self.fresh_reg(TypeHint::Bool);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Bool)),
                    func: "str_ne".into(),
                    args: vec![lhs, rhs],
                });
                return Operand::Reg(dst, TypeHint::Bool);
            }
            _ => {}
        }
        // stage3：运算符重载（class 实例的 __add__/__sub__/...）。
        if let Some(class) = self.infer_class_from_expr(&b.left) {
            let dunder = match b.operator {
                BinaryOp::Add => Some("__add__"),
                BinaryOp::Sub => Some("__sub__"),
                BinaryOp::Mul => Some("__mul__"),
                BinaryOp::Div => Some("__div__"),
                BinaryOp::Mod => Some("__mod__"),
                BinaryOp::Eq => Some("__eq__"),
                BinaryOp::Ne => Some("__ne__"),
                BinaryOp::Lt => Some("__lt__"),
                BinaryOp::Le => Some("__le__"),
                BinaryOp::Gt => Some("__gt__"),
                BinaryOp::Ge => Some("__ge__"),
                _ => None,
            };
            if let Some(method) = dunder {
                if self.class_has_method(&class, method) {
                    let func = format!("{}_{}", class, method);
                    let dst = self.fresh_reg(TypeHint::Dynamic);
                    self.emit(StaticInsn::Call {
                        dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                        func,
                        args: vec![lhs, rhs],
                        ty: TypeHint::Dynamic,
                    });
                    return Operand::Reg(dst, TypeHint::Dynamic);
                }
            }
        }
        // 常量折叠：若两边都是整数字面量，直接计算。
        if let (Operand::ImmI64(a), Operand::ImmI64(c)) = (&lhs, &rhs) {
            if let Some(folded) = fold_i64(&b.operator, *a, *c) {
                return Operand::ImmI64(folded);
            }
        }
        // 逻辑短路运算：用分支实现。
        match b.operator {
            BinaryOp::And => {
                let end = self.fresh_label("and_end");
                let false_lbl = self.fresh_label("and_false");
                self.emit(StaticInsn::Branch { cond: Cond::Eq, lhs: lhs.clone(), rhs: Operand::ImmBool(false), target: false_lbl.clone() });
                let result = self.fresh_reg(TypeHint::Bool);
                self.emit(StaticInsn::Mov { dst: Operand::Reg(result.clone(), TypeHint::Bool), src: rhs, ty: TypeHint::Bool });
                self.emit(StaticInsn::Jump { target: end.clone() });
                self.emit(StaticInsn::Label(false_lbl));
                self.emit(StaticInsn::Mov { dst: Operand::Reg(result.clone(), TypeHint::Bool), src: Operand::ImmBool(false), ty: TypeHint::Bool });
                self.emit(StaticInsn::Label(end));
                return Operand::Reg(result, TypeHint::Bool);
            }
            BinaryOp::Or => {
                let end = self.fresh_label("or_end");
                let true_lbl = self.fresh_label("or_true");
                self.emit(StaticInsn::Branch { cond: Cond::Ne, lhs: lhs.clone(), rhs: Operand::ImmBool(false), target: true_lbl.clone() });
                let result = self.fresh_reg(TypeHint::Bool);
                self.emit(StaticInsn::Mov { dst: Operand::Reg(result.clone(), TypeHint::Bool), src: rhs, ty: TypeHint::Bool });
                self.emit(StaticInsn::Jump { target: end.clone() });
                self.emit(StaticInsn::Label(true_lbl));
                self.emit(StaticInsn::Mov { dst: Operand::Reg(result.clone(), TypeHint::Bool), src: Operand::ImmBool(true), ty: TypeHint::Bool });
                self.emit(StaticInsn::Label(end));
                return Operand::Reg(result, TypeHint::Bool);
            }
            _ => {}
        }
        // 比较运算：产出 bool。
        if let Some(cond) = binop_to_cond(&b.operator) {
            let ty = if lhs.ty().is_static() && rhs.ty().is_static() && lhs.ty() == rhs.ty() {
                TypeHint::Bool
            } else {
                TypeHint::Dynamic
            };
            let dst = self.fresh_reg(ty);
            if ty.is_dynamic() {
                // 动态比较：用 runtime。
                let dyn_func = match b.operator {
                    BinaryOp::Eq => "vredrs_value_eq",
                    BinaryOp::Ne => "vredrs_value_ne",
                    BinaryOp::Lt => "vredrs_value_lt",
                    BinaryOp::Le => "vredrs_value_le",
                    BinaryOp::Gt => "vredrs_value_gt",
                    BinaryOp::Ge => "vredrs_value_ge",
                    _ => "vredrs_value_eq",
                };
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: dyn_func.into(),
                    args: vec![lhs, rhs],
                });
            } else {
                self.emit(StaticInsn::Cmp { dst: Operand::Reg(dst.clone(), TypeHint::Bool), cond, lhs, rhs, ty: TypeHint::Bool });
            }
            return Operand::Reg(dst, ty);
        }
        // 算术/位运算。
        let lhs_ty = lhs.ty();
        let rhs_ty = rhs.ty();
        let is_static = lhs_ty.is_static() && rhs_ty.is_static() && lhs_ty == rhs_ty && !matches!(b.operator, BinaryOp::Is | BinaryOp::In | BinaryOp::Repeated);
        let ty = if is_static { lhs_ty } else { TypeHint::Dynamic };
        let op_arith = match b.operator {
            BinaryOp::Add => BinOp::Add,
            BinaryOp::Sub => BinOp::Sub,
            BinaryOp::Mul => BinOp::Mul,
            BinaryOp::Div => BinOp::Div,
            BinaryOp::Mod => BinOp::Mod,
            BinaryOp::BitAnd => BinOp::And,
            BinaryOp::BitOr => BinOp::Or,
            BinaryOp::BitXor => BinOp::Xor,
            BinaryOp::Shl => BinOp::Shl,
            BinaryOp::Shr => BinOp::Shr,
            BinaryOp::FloorDiv => BinOp::Div,
            BinaryOp::Power => {
                // Power: 调用 vredrs_pow_i64(base, exp) runtime 函数。
                let dst = self.fresh_reg(TypeHint::I64);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::I64)),
                    func: "vredrs_pow_i64".into(),
                    args: vec![lhs.clone(), rhs.clone()],
                });
                return Operand::Reg(dst, TypeHint::I64);
            }
            BinaryOp::Is | BinaryOp::In | BinaryOp::Repeated => {
                // 这些用 runtime。
                let dyn_func = match b.operator {
                    BinaryOp::Is => "vredrs_is",
                    BinaryOp::In => "vredrs_in",
                    BinaryOp::Repeated => "vredrs_repeated",
                    _ => "vredrs_value_eq",
                };
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: dyn_func.into(),
                    args: vec![lhs, rhs],
                });
                return Operand::Reg(dst, TypeHint::Dynamic);
            }
            _ => BinOp::Add,
        };
        let op = if ty.is_dynamic() { dyn_op_of(op_arith) } else { op_arith };
        let dst = self.fresh_reg(ty);
        self.emit(StaticInsn::Bin { op, dst: Operand::Reg(dst.clone(), ty), lhs, rhs, ty });
        Operand::Reg(dst, ty)
    }

    fn lower_unary(&mut self, u: &ast::UnaryExpr) -> Operand {
        let v = self.lower_expr(&u.operand);
        match u.operator {
            UnaryOp::Neg => {
                let ty = v.ty();
                let dst = self.fresh_reg(ty);
                let op = if ty.is_dynamic() { BinOp::SubDyn } else { BinOp::Sub };
                // 0 - v
                self.emit(StaticInsn::Bin {
                    op,
                    dst: Operand::Reg(dst.clone(), ty),
                    lhs: if ty == TypeHint::F64 { Operand::ImmF64(0.0) } else { Operand::ImmI64(0) },
                    rhs: v,
                    ty,
                });
                Operand::Reg(dst, ty)
            }
            UnaryOp::Not | UnaryOp::Bang => {
                let ty = v.ty();
                let dst = self.fresh_reg(TypeHint::Bool);
                if ty.is_static() {
                    self.emit(StaticInsn::Not { dst: Operand::Reg(dst.clone(), TypeHint::Bool), src: v, ty: TypeHint::Bool });
                } else {
                    self.emit(StaticInsn::RuntimeCall {
                        dst: Some(Operand::Reg(dst.clone(), TypeHint::Bool)),
                        func: "vredrs_not".into(),
                        args: vec![v],
                    });
                }
                Operand::Reg(dst, TypeHint::Bool)
            }
        }
    }

    fn lower_call(&mut self, c: &ast::CallExpr) -> Operand {
        // stage3：MemberAccess callee → 方法分派（obj.method 或 ClassName.method）。
        if let Expr::MemberAccess(m) = c.callee.as_ref() {
            return self.lower_method_call_from_member(m, &c.args);
        }
        // stage3：super.method() → Call ParentClass_method(self, args)。
        if let Expr::Identifier(id) = c.callee.as_ref() {
            if id.name == "super" {
                if let Some(class) = &self.current_class {
                    if let Some(parent) = self.class_parent(class) {
                        // 调用父类的同名方法（这里 callee 名为 "super"，
                        // 实际方法名需从外部上下文推断；此路径主要用于
                        // 通过 lower_method_call 的 super 分支处理）。
                        let args: Vec<Operand> = c.args.iter().map(|a| self.lower_expr(a)).collect();
                        let dst = self.fresh_reg(TypeHint::Dynamic);
                        self.emit(StaticInsn::Comment(format!("super call via {}", parent)));
                        self.emit(StaticInsn::RuntimeCall {
                            dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                            func: "vredrs_super_call".into(),
                            args,
                        });
                        return Operand::Reg(dst, TypeHint::Dynamic);
                    }
                }
            }
        }
        // 直接调用：callee 是 Identifier。
        if let Expr::Identifier(id) = c.callee.as_ref() {
            let func_name = &id.name;
            // stage3：generator 调用 → vredrs_gen_create(fn_ptr, args)。
            if self.generator_fns.contains(func_name) {
                let fn_ptr = Operand::Sym(func_name.clone());
                let mut args = vec![fn_ptr];
                for a in &c.args {
                    args.push(self.lower_expr(a));
                }
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_gen_create".into(),
                    args,
                });
                return Operand::Reg(dst, TypeHint::Dynamic);
            }
            // stage3：class 构造调用 ClassName(args) → vredrs_new_typed_object + Class_new(self, args)。
            if self.class_names.contains(func_name) {
                // 1. 创建 typed object（dict-like）。
                let obj = self.fresh_reg(TypeHint::Dynamic);
                let class_idx = self.intern_str(func_name);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(obj.clone(), TypeHint::Dynamic)),
                    func: "vredrs_new_typed_object".into(),
                    args: vec![Operand::ImmStr(class_idx)],
                });
                // 2. 若 class 有 new/__init__，调 Class_new(self, args...)。
                if let Some(info) = self.class_defs.get(func_name) {
                    if info.has_new {
                        let ctor_name = if info.methods.contains_key("new") {
                            format!("{}_new", func_name)
                        } else {
                            format!("{}___init__", func_name)
                        };
                        let mut args = vec![Operand::Reg(obj.clone(), TypeHint::Dynamic)];
                        for a in &c.args {
                            args.push(self.lower_expr(a));
                        }
                        self.emit(StaticInsn::Call {
                            dst: None,
                            func: ctor_name,
                            args,
                            ty: TypeHint::Dynamic,
                        });
                    }
                }
                return Operand::Reg(obj, TypeHint::Dynamic);
            }
            // Phase 5 验收：虚假特性 `__double__(x)` → 返回 x*2。
            if func_name == "__double__" && c.args.len() == 1 {
                let arg = self.lower_expr(&c.args[0]);
                let ty = arg.ty();
                let op = if ty.is_static() { BinOp::Mul } else { BinOp::MulDyn };
                let dst = self.fresh_reg(ty);
                self.emit(StaticInsn::Bin {
                    op,
                    dst: Operand::Reg(dst.clone(), ty),
                    lhs: arg,
                    rhs: Operand::ImmI64(2),
                    ty,
                });
                return Operand::Reg(dst, ty);
            }
            // Phase 5 验收：虚假特性 `__square__(x)` → 返回 x*x。
            if func_name == "__square__" && c.args.len() == 1 {
                let arg = self.lower_expr(&c.args[0]);
                let ty = arg.ty();
                let op = if ty.is_static() { BinOp::Mul } else { BinOp::MulDyn };
                let dst = self.fresh_reg(ty);
                self.emit(StaticInsn::Bin {
                    op,
                    dst: Operand::Reg(dst.clone(), ty),
                    lhs: arg.clone(),
                    rhs: arg,
                    ty,
                });
                return Operand::Reg(dst, ty);
            }
            // stage3：本地变量持有 lambda → CallDyn。
            if self.locals.contains_key(func_name) {
                // 若该变量不是已知函数，且不是 class，视为动态闭包调用。
                if !self.fn_sigs.contains_key(func_name) {
                    let callee = self.lower_expr(&c.callee);
                    let args: Vec<Operand> = c.args.iter().map(|a| self.lower_expr(a)).collect();
                    let dst = self.fresh_reg(TypeHint::Dynamic);
                    self.emit(StaticInsn::CallDyn {
                        dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                        callee,
                        args,
                        ty: TypeHint::Dynamic,
                    });
                    return Operand::Reg(dst, TypeHint::Dynamic);
                }
            }
            // stage3：builtin 路由（如 print/input/len/range 等映射到 stub 函数）。
            if let Some(stub) = builtin_to_stub(func_name) {
                let args: Vec<Operand> = c.args.iter().map(|a| self.lower_expr(a)).collect();
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: stub.into(),
                    args,
                });
                return Operand::Reg(dst, TypeHint::Dynamic);
            }
            let args: Vec<Operand> = c.args.iter().map(|a| self.lower_expr(a)).collect();
            // 查函数签名推断返回类型。
            let ret_ty = self
                .fn_sigs
                .get(func_name)
                .map(|s| s.returns)
                .unwrap_or(TypeHint::Dynamic);
            let dst = self.fresh_reg(ret_ty);
            self.emit(StaticInsn::Call {
                dst: Some(Operand::Reg(dst.clone(), ret_ty)),
                func: func_name.clone(),
                args,
                ty: ret_ty,
            });
            return Operand::Reg(dst, ret_ty);
        }
        // 动态调用：callee 是任意表达式。
        let callee = self.lower_expr(&c.callee);
        let args: Vec<Operand> = c.args.iter().map(|a| self.lower_expr(a)).collect();
        let dst = self.fresh_reg(TypeHint::Dynamic);
        self.emit(StaticInsn::CallDyn {
            dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
            callee,
            args,
            ty: TypeHint::Dynamic,
        });
        Operand::Reg(dst, TypeHint::Dynamic)
    }

    /// stage3：处理 `obj.method(args)` / `ClassName.method(args)` / `super.method(args)`
    /// 形式的调用（callee 是 MemberAccess）。
    fn lower_method_call_from_member(&mut self, m: &ast::MemberAccessExpr, call_args: &[Expr]) -> Operand {
        // super.method(...) → Call ParentClass_method(self, args)。
        if let Expr::Identifier(id) = m.target.as_ref() {
            if id.name == "super" {
                if let Some(class) = &self.current_class {
                    if let Some(parent) = self.class_parent(class) {
                        let func = format!("{}_{}", parent, m.member.name);
                        let mut args = Vec::new();
                        // self 作为第一个参数。
                        if let Some((reg, ty)) = self.local_reg("self") {
                            args.push(Operand::Reg(reg, ty));
                        } else {
                            args.push(Operand::Null);
                        }
                        for a in call_args {
                            args.push(self.lower_expr(a));
                        }
                        let dst = self.fresh_reg(TypeHint::Dynamic);
                        self.emit(StaticInsn::Call {
                            dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                            func,
                            args,
                            ty: TypeHint::Dynamic,
                        });
                        return Operand::Reg(dst, TypeHint::Dynamic);
                    }
                }
            }
            // ClassName.new(...) → 创建对象 + Call ownerClass_new(obj, args)。
            if self.class_names.contains(&id.name) && m.member.name == "new" {
                let class_name = &id.name;
                let class_idx = self.intern_str(class_name);
                let obj = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(obj.clone(), TypeHint::Dynamic)),
                    func: "vredrs_new_typed_object".into(),
                    args: vec![Operand::ImmStr(class_idx)],
                });
                // 沿继承链查找 new 方法的定义类。
                let owner = self.lookup_dispatch(class_name, "new")
                    .map(|e| e.class_name)
                    .unwrap_or_else(|| class_name.clone());
                let ctor = format!("{}_new", owner);
                let mut args = vec![Operand::Reg(obj.clone(), TypeHint::Dynamic)];
                for a in call_args {
                    args.push(self.lower_expr(a));
                }
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::Call {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: ctor,
                    args,
                    ty: TypeHint::Dynamic,
                });
                return Operand::Reg(dst, TypeHint::Dynamic);
            }
            // ClassName.method(...) → Call ownerClass_method(self, args)。
            if self.class_names.contains(&id.name) {
                let owner = self.lookup_dispatch(&id.name, &m.member.name)
                    .map(|e| e.class_name)
                    .unwrap_or_else(|| id.name.clone());
                let func = format!("{}_{}", owner, m.member.name);
                let mut args = Vec::new();
                if let Some((reg, ty)) = self.local_reg("self") {
                    args.push(Operand::Reg(reg, ty));
                } else {
                    args.push(Operand::Null);
                }
                for a in call_args {
                    args.push(self.lower_expr(a));
                }
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::Call {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func,
                    args,
                    ty: TypeHint::Dynamic,
                });
                return Operand::Reg(dst, TypeHint::Dynamic);
            }
        }
        // 普通对象方法：尝试静态分派（若 receiver 类型已知且 class 有该方法）。
        if let Some(class) = self.infer_class_from_expr(&m.target) {
            let dispatch = self.lookup_dispatch(&class, &m.member.name);
            if let Some(entry) = dispatch {
                let recv = self.lower_expr(&m.target);
                let mut args = vec![recv];
                for a in call_args {
                    args.push(self.lower_expr(a));
                }
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::Call {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: entry.func_name.clone(),
                    args,
                    ty: TypeHint::Dynamic,
                });
                return Operand::Reg(dst, TypeHint::Dynamic);
            }
        }
        // 运行时分派：vredrs_method_call(recv, method_name, args...)。
        let recv = self.lower_expr(&m.target);
        let method_idx = self.intern_str(&m.member.name);
        let mut args = vec![recv, Operand::ImmStr(method_idx)];
        for a in call_args {
            args.push(self.lower_expr(a));
        }
        let dst = self.fresh_reg(TypeHint::Dynamic);
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
            func: "vredrs_method_call".into(),
            args,
        });
        Operand::Reg(dst, TypeHint::Dynamic)
    }

    fn lower_method_call(&mut self, mc: &ast::MethodCallExpr) -> Operand {
        // stage3：与 lower_method_call_from_member 共享分派逻辑。
        // 把 MethodCallExpr 包装成等价的 MemberAccess + args 调用。
        let m = ast::MemberAccessExpr {
            target: mc.receiver.clone(),
            member: mc.method.clone(),
            span: mc.span.clone(),
        };
        self.lower_method_call_from_member(&m, &mc.args)
    }

    /// stage3：字符串插值 — 把 `"abc{expr}def"` 转成 runtime 拼接。
    /// 生成 vredrs_str_concat_n(n, part1, part2, ...) 调用。
    fn lower_string_interp(&mut self, parts: &[ast::StringPart]) -> Operand {
        let mut args: Vec<Operand> = Vec::new();
        let mut n: i64 = 0;
        for p in parts {
            match p {
                ast::StringPart::Text(t) => {
                    let idx = self.intern_str(t);
                    args.push(Operand::ImmStr(idx));
                    n += 1;
                }
                ast::StringPart::Interpolation(e) => {
                    // 把表达式求值结果转成字符串（runtime 负责）。
                    let v = self.lower_expr(e);
                    // 用 vredrs_to_str 把动态值转成字符串。
                    let s = self.fresh_reg(TypeHint::Dynamic);
                    self.emit(StaticInsn::RuntimeCall {
                        dst: Some(Operand::Reg(s.clone(), TypeHint::Dynamic)),
                        func: "vredrs_to_str".into(),
                        args: vec![v],
                    });
                    args.push(Operand::Reg(s, TypeHint::Dynamic));
                    n += 1;
                }
            }
        }
        let dst = self.fresh_reg(TypeHint::Dynamic);
        let mut full_args = vec![Operand::ImmI64(n)];
        full_args.append(&mut args);
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
            func: "vredrs_str_concat_n".into(),
            args: full_args,
        });
        Operand::Reg(dst, TypeHint::Dynamic)
    }

    /// stage3：lambda 闭包 lowering。
    /// 1. 生成唯一函数名 `__lambda_N`，把 lambda body 编译成 IR 函数（参数 = captures_ptr + params）。
    /// 2. 在当前位置 emit vredrs_make_captures + vredrs_make_closure(fn_ptr, captures_ptr)。
    fn lower_lambda_function(&mut self, l: &ast::LambdaExpr) -> Operand {
        let n = self.lambda_counter;
        self.lambda_counter += 1;
        let fn_name = format!("__lambda_{}", n);

        // 收集 captures：扫描 lambda body 中的标识符，若在当前 self.locals 中，
        // 且不是 lambda 的参数，则为 capture。
        let param_names: std::collections::HashSet<String> =
            l.params.iter().map(|p| p.name.name.clone()).collect();
        let mut captures: Vec<String> = Vec::new();
        self.collect_free_vars_expr(&l.body, &param_names, &mut captures);
        captures.sort();
        captures.dedup();

        // 在创建点 emit vredrs_make_captures(n, val1, val2, ...)。
        let n_captures = captures.len() as i64;
        let mut cap_args = vec![Operand::ImmI64(n_captures)];
        for cap in &captures {
            if let Some((reg, ty)) = self.locals.get(cap).cloned() {
                cap_args.push(Operand::Reg(reg, ty));
            } else {
                // 全局变量：通过 vredrs_load_global 读取。
                let cap_idx = self.intern_str(cap);
                let dst = self.fresh_reg(TypeHint::Dynamic);
                self.emit(StaticInsn::RuntimeCall {
                    dst: Some(Operand::Reg(dst.clone(), TypeHint::Dynamic)),
                    func: "vredrs_load_global".into(),
                    args: vec![Operand::ImmStr(cap_idx)],
                });
                cap_args.push(Operand::Reg(dst, TypeHint::Dynamic));
            }
        }
        let captures_ptr = self.fresh_reg(TypeHint::Dynamic);
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(captures_ptr.clone(), TypeHint::Dynamic)),
            func: "vredrs_make_captures".into(),
            args: cap_args,
        });

        // 构建闭包对象：vredrs_make_closure(fn_ptr, captures_ptr)。
        let fn_ptr = Operand::Sym(fn_name.clone());
        let closure_obj = self.fresh_reg(TypeHint::Dynamic);
        self.emit(StaticInsn::RuntimeCall {
            dst: Some(Operand::Reg(closure_obj.clone(), TypeHint::Dynamic)),
            func: "vredrs_make_closure".into(),
            args: vec![fn_ptr, Operand::Reg(captures_ptr, TypeHint::Dynamic)],
        });

        // 保存外层上下文。
        let saved_reg = self.reg_counter;
        let saved_label = self.label_counter;
        let saved_fn = std::mem::take(&mut self.cur_fn_name);
        let saved_locals = std::mem::take(&mut self.locals);
        let saved_params = std::mem::take(&mut self.params);
        let saved_body = std::mem::take(&mut self.body);
        let saved_decls = std::mem::take(&mut self.local_decls);
        let saved_loops = std::mem::take(&mut self.loop_stack);
        let saved_var_class = std::mem::take(&mut self.var_class);
        let saved_var_str = std::mem::take(&mut self.var_str);
        let saved_current = self.current_class.take();

        self.reg_counter = 0;
        self.label_counter = 0;
        self.cur_fn_name = fn_name.clone();

        let mut param_regs: Vec<String> = Vec::new();
        // 第一个参数：captures_ptr（Dynamic）。
        {
            let ty = TypeHint::Dynamic;
            let reg = self.fresh_reg(ty);
            self.locals.insert("__captures__".to_string(), (reg.clone(), ty));
            self.params.push(("__captures__".to_string(), ty));
            self.local_decls.push(("__captures__".to_string(), ty));
            param_regs.push(reg);
        }
        // 从 captures_ptr 读取每个 capture 值到局部变量。
        for (i, cap) in captures.iter().enumerate() {
            let ty = TypeHint::Dynamic;
            let reg = self.fresh_reg(ty);
            self.locals.insert(cap.clone(), (reg.clone(), ty));
            self.local_decls.push((cap.clone(), ty));
            self.emit(StaticInsn::RuntimeCall {
                dst: Some(Operand::Reg(reg.clone(), TypeHint::Dynamic)),
                func: "vredrs_get_capture".into(),
                args: vec![
                    Operand::Reg(self.locals["__captures__"].0.clone(), TypeHint::Dynamic),
                    Operand::ImmI64(i as i64),
                ],
            });
        }
        for p in &l.params {
            let ty = p
                .type_annotation
                .as_ref()
                .map(type_expr_to_hint)
                .unwrap_or(TypeHint::Dynamic);
            let reg = self.fresh_reg(ty);
            self.locals.insert(p.name.name.clone(), (reg.clone(), ty));
            self.params.push((p.name.name.clone(), ty));
            self.local_decls.push((p.name.name.clone(), ty));
            param_regs.push(reg);
        }

        // lambda body 是单个表达式：求值后 return。
        let body_val = self.lower_expr(&l.body);
        self.emit(StaticInsn::Ret { value: Some(body_val) });

        let return_ty = l
            .return_type
            .as_ref()
            .map(type_expr_to_hint)
            .unwrap_or(TypeHint::Dynamic);

        let func = IrFunction {
            name: fn_name.clone(),
            params: self.params.clone(),
            param_regs,
            return_ty,
            body: std::mem::take(&mut self.body),
            is_main: false,
            annotations: vec![],
            locals: self.local_decls.clone(),
        };
        self.lambda_fns.push(func);

        // 恢复外层上下文。
        self.reg_counter = saved_reg;
        self.label_counter = saved_label;
        self.cur_fn_name = saved_fn;
        self.locals = saved_locals;
        self.params = saved_params;
        self.body = saved_body;
        self.local_decls = saved_decls;
        self.loop_stack = saved_loops;
        self.var_class = saved_var_class;
        self.var_str = saved_var_str;
        self.current_class = saved_current;

        // closure_obj 已在保存前创建（make_captures + make_closure）。
        Operand::Reg(closure_obj, TypeHint::Dynamic)
    }
}

// ── 辅助函数 ──────────────────────────────────────────────

/// `TypeExpr` → `TypeHint`。无注解或未知类型 → `Dynamic`。
pub fn type_expr_to_hint(t: &TypeExpr) -> TypeHint {
    match t {
        TypeExpr::Basic(b, _) => match b {
            BasicType::Int => TypeHint::I64,
            BasicType::Float => TypeHint::F64,
            BasicType::Str => TypeHint::Str,
            BasicType::Bool => TypeHint::Bool,
            BasicType::Null => TypeHint::Dynamic,
            BasicType::Void => TypeHint::Unit,
            BasicType::Any => TypeHint::Dynamic,
        },
        TypeExpr::Pointer(_, _) => TypeHint::Ptr,
        TypeExpr::Borrow { .. } | TypeExpr::MutBorrow { .. } => TypeHint::Ptr,
        TypeExpr::UnsignedInt(_, _) => TypeHint::I64,
        TypeExpr::Array(_, _, _) | TypeExpr::Generic { .. } => TypeHint::Dynamic,
        TypeExpr::Named(_, _) => TypeHint::Dynamic,
        TypeExpr::Optional(_, _) => TypeHint::Dynamic,
        TypeExpr::Tuple(_, _) | TypeExpr::Function { .. } | TypeExpr::Channel(_, _) | TypeExpr::Result(..) => TypeHint::Dynamic,
    }
}

/// `BinaryOp` → `Cond`（仅比较运算）。
fn binop_to_cond(op: &BinaryOp) -> Option<Cond> {
    Some(match op {
        BinaryOp::Eq => Cond::Eq,
        BinaryOp::Ne => Cond::Ne,
        BinaryOp::Lt => Cond::Lt,
        BinaryOp::Le => Cond::Le,
        BinaryOp::Gt => Cond::Gt,
        BinaryOp::Ge => Cond::Ge,
        _ => return None,
    })
}

/// 常量折叠：对两个 i64 字面量执行二元运算。返回 None 表示无法折叠。
fn fold_i64(op: &BinaryOp, a: i64, c: i64) -> Option<i64> {
    Some(match op {
        BinaryOp::Add => a.wrapping_add(c),
        BinaryOp::Sub => a.wrapping_sub(c),
        BinaryOp::Mul => a.wrapping_mul(c),
        BinaryOp::Div => if c != 0 { a.wrapping_div(c) } else { return None },
        BinaryOp::Mod => if c != 0 { a.wrapping_rem(c) } else { return None },
        BinaryOp::And => a & c,
        BinaryOp::Or => a | c,
        BinaryOp::BitAnd => a & c,
        BinaryOp::BitOr => a | c,
        BinaryOp::BitXor => a ^ c,
        BinaryOp::Shl => a.wrapping_shl(c as u32),
        BinaryOp::Shr => a.wrapping_shr(c as u32),
        BinaryOp::Eq => (a == c) as i64,
        BinaryOp::Ne => (a != c) as i64,
        BinaryOp::Lt => (a < c) as i64,
        BinaryOp::Le => (a <= c) as i64,
        BinaryOp::Gt => (a > c) as i64,
        BinaryOp::Ge => (a >= c) as i64,
        BinaryOp::FloorDiv => if c != 0 { a.wrapping_div(c) } else { return None },
        _ => return None,
    })
}

fn negate_cond(c: Cond) -> Cond {
    match c {
        Cond::Eq => Cond::Ne,
        Cond::Ne => Cond::Eq,
        Cond::Lt => Cond::Ge,
        Cond::Le => Cond::Gt,
        Cond::Gt => Cond::Le,
        Cond::Ge => Cond::Lt,
    }
}

/// 静态算术 op → 对应的动态 op（调用 runtime）。
fn dyn_op_of(op: BinOp) -> BinOp {
    match op {
        BinOp::Add => BinOp::AddDyn,
        BinOp::Sub => BinOp::SubDyn,
        BinOp::Mul => BinOp::MulDyn,
        BinOp::Div => BinOp::DivDyn,
        BinOp::Mod => BinOp::ModDyn,
        BinOp::And => BinOp::And,
        BinOp::Or => BinOp::Or,
        BinOp::Xor => BinOp::Xor,
        BinOp::Shl => BinOp::Shl,
        BinOp::Shr => BinOp::Shr,
        other => other,
    }
}

/// 把字符串字面量的 parts（可能含插值）扁平化成纯文本。
/// 插值部分用 `{}` 占位（IR 层不展开插值，由 runtime 处理）。
fn flatten_string_parts(parts: &[ast::StringPart]) -> String {
    let mut out = String::new();
    for p in parts {
        match p {
            ast::StringPart::Text(t) => out.push_str(t),
            ast::StringPart::Interpolation(_) => out.push_str("{}"),
        }
    }
    out
}

// ── generator 检测（stage3-vtable）──────────────────────────

/// 检查函数体是否含 yield 语句（递归遍历嵌套块）。
pub fn fn_has_yield(body: &[Stmt]) -> bool {
    body.iter().any(stmt_has_yield)
}

/// 递归检查单个语句是否含 yield。
pub fn stmt_has_yield(s: &Stmt) -> bool {
    match s {
        Stmt::Yield(_) => true,
        Stmt::If(i) => {
            i.then_body.iter().any(stmt_has_yield)
                || i.else_body.as_ref().map(|b| b.iter().any(stmt_has_yield)).unwrap_or(false)
                || i.elif_chain.iter().any(|(_, body)| body.iter().any(stmt_has_yield))
        }
        Stmt::While(w) => w.body.iter().any(stmt_has_yield)
            || w.else_body.as_ref().map(|b| b.iter().any(stmt_has_yield)).unwrap_or(false),
        Stmt::ForRange(fr) => fr.body.iter().any(stmt_has_yield)
            || fr.else_body.as_ref().map(|b| b.iter().any(stmt_has_yield)).unwrap_or(false),
        Stmt::ForIn(fi) => fi.body.iter().any(stmt_has_yield)
            || fi.else_body.as_ref().map(|b| b.iter().any(stmt_has_yield)).unwrap_or(false),
        Stmt::Loop(l) => l.body.iter().any(stmt_has_yield)
            || l.else_body.as_ref().map(|b| b.iter().any(stmt_has_yield)).unwrap_or(false),
        Stmt::Match(m) => m.cases.iter().any(|c| c.body.iter().any(stmt_has_yield))
            || m.else_case.as_ref().map(|b| b.iter().any(stmt_has_yield)).unwrap_or(false),
        Stmt::Try(t) => t.try_body.iter().any(stmt_has_yield)
            || t.catch_body.as_ref().map(|b| b.iter().any(stmt_has_yield)).unwrap_or(false)
            || t.finally_body.as_ref().map(|b| b.iter().any(stmt_has_yield)).unwrap_or(false),
        Stmt::UnsafeBlock(u) => u.body.iter().any(stmt_has_yield),
        Stmt::DirectiveBlock(db) => db.body.iter().any(stmt_has_yield),
        Stmt::ScopeBlock(sb) => sb.body.iter().any(stmt_has_yield),
        Stmt::With(w) => w.body.iter().any(stmt_has_yield),
        Stmt::Defer(d) => stmt_has_yield(d.stmt.as_ref()),
        Stmt::Expr(_) | Stmt::Println(_) | Stmt::Assign(_) | Stmt::Return(_)
        | Stmt::Break(_) | Stmt::Continue(_) | Stmt::Throw(_) | Stmt::Panic(_)
        | Stmt::Assert(_) | Stmt::Asm(_) | Stmt::Spawn(_) | Stmt::SpawnThread(_)
        | Stmt::Flush(_) | Stmt::Input(_) | Stmt::Yield(_) | Stmt::Pon(_)
        | Stmt::TableAssign(_) | Stmt::Paste(_) | Stmt::Select(_) => false,
    }
}

// ── class 元数据辅助（stage3-vtable）────────────────────────

/// 从 `extends` 字段的 TypeExpr 提取父类名（仅 Named 类型支持）。
pub fn extract_class_name(t: &TypeExpr) -> Option<String> {
    match t {
        TypeExpr::Named(name, _) => Some(name.name.clone()),
        TypeExpr::Basic(b, _) => Some(format!("{:?}", b).to_lowercase()),
        _ => None,
    }
}

// ── builtin → stub 路由表（stage3-vtable）────────────────────

/// 把 builtin 函数名映射到 runtime stub 函数名。
/// 返回 None 表示该名字不是 builtin（按普通函数调用处理）。
pub fn builtin_to_stub(name: &str) -> Option<&'static str> {
    match name {
        "print" => Some("vredrs_print"),
        "println" => Some("vredrs_println"),
        "print_str" => Some("vredrs_print_str"),
        "println_str" => Some("vredrs_println_str"),
        "print_f64" => Some("vredrs_print_f64"),
        "println_f64" => Some("vredrs_println_f64"),
        "input" => Some("vredrs_input"),
        "len" => Some("vredrs_len"),
        "str_len" => Some("str_len"),
        "range" => Some("range"),
        "make_list" => Some("vredrs_make_list"),
        "make_dict" => Some("vredrs_make_dict"),
        "make_set" => Some("vredrs_make_set"),
        "make_tuple" => Some("vredrs_make_tuple"),
        "make_range" => Some("vredrs_make_range"),
        "append" | "list_append" => Some("vredrs_list_append"),
        "sum" => Some("sum"),
        "set_add" => Some("vredrs_set_add"),
        "index" => Some("vredrs_index"),
        "get_field" => Some("vredrs_get_field"),
        "set_field" => Some("vredrs_set_field"),
        "slice" => Some("vredrs_slice"),
        "str_concat" => Some("str_concat"),
        "str_repeat" => Some("str_repeat"),
        "str_eq" => Some("str_eq"),
        "str_ne" => Some("str_ne"),
        "to_str" | "str" => Some("vredrs_to_str"),
        "to_int" | "int" => Some("vredrs_to_int"),
        "to_float" | "float" => Some("vredrs_to_float"),
        "is" => Some("vredrs_is"),
        "in" => Some("vredrs_in"),
        "in_" => Some("vredrs_in"),
        "throw" => Some("vredrs_throw"),
        "panic" => Some("vredrs_panic"),
        "assert_fail" => Some("vredrs_assert_fail"),
        "yield" => Some("vredrs_yield"),
        "spawn" => Some("vredrs_spawn"),
        "spawn_thread" => Some("vredrs_spawn_thread"),
        "flush" => Some("vredrs_flush"),
        "reverse" => Some("vredrs_reverse"),
        "sort_asc" => Some("vredrs_sort_asc"),
        "sort_desc" => Some("vredrs_sort_desc"),
        "abs" => Some("vredrs_abs"),
        "max" => Some("vredrs_max"),
        "min" => Some("vredrs_min"),
        "sqrt" => Some("vredrs_sqrt"),
        "pow" => Some("vredrs_pow"),
        "file_exists" => Some("file_exists"),
        "read_file" => Some("read_file"),
        "write_file" => Some("write_file"),
        // 容器扩展
        "sorted" => Some("list_sorted"),
        "reversed" => Some("list_reversed"),
        "unique" => Some("list_unique"),
        "first" => Some("list_first"),
        "last" => Some("list_last"),
        "take" => Some("list_take"),
        "drop" => Some("list_drop"),
        "list_max" => Some("list_max"),
        "list_min" => Some("list_min"),
        "list_copy" | "clone" => Some("list_copy"),
        "list_concat" => Some("list_concat"),
        "list_contains" => Some("list_contains_val"),
        "list_index_of" => Some("list_index_of"),
        "is_empty" | "isEmpty" => Some("list_is_empty"),
        "pop" => Some("list_pop"),
        // 字符串扩展
        "startsWith" | "starts_with" => Some("starts_with"),
        "endsWith" | "ends_with" => Some("ends_with"),
        "indexOf" | "index_of" => Some("index_of"),
        "rfind" => Some("rfind"),
        "substring" | "substr" => Some("substring"),
        "replace" | "replaceAll" => Some("str_replace"),
        "trim" => Some("str_trim"),
        "repeat" => Some("str_repeat"),
        "charAt" | "char_at" => Some("char_at"),
        "upper" => Some("upper"),
        "lower" => Some("lower"),
        "contains" => Some("contains"),
        "split" => Some("split"),
        "concat" => Some("str_concat"),
        "str_len" => Some("str_len"),
        // 数学
        "abs" => Some("abs_i64"),
        "max" => Some("max_i64"),
        "min" => Some("min_i64"),
        "gcd" => Some("gcd_i64"),
        // 时间/系统
        "now" | "time" => Some("time_now"),
        "clockMs" | "clock_ms" => Some("clock_ms"),
        "sleep" => Some("sleep_secs"),
        "exit" => Some("sys_exit"),
        "getenv" | "getEnv" => Some("get_env"),
        "system" => Some("sys_system"),
        // I/O
        "readLine" | "read_line" => Some("read_line"),
        "typeof" | "typeOf" | "type_name" => Some("vredrs_typeof"),
        "random" => Some("random_int"),
        "randint" => Some("randint"),
        "seed" | "srand" => Some("seed_random"),
        "readFile" | "read_file_str" | "readFileStr" => Some("read_file_str"),
        "writeFile" | "write_file_str" | "writeFileStr" => Some("write_file_str"),
        "deleteFile" | "delete_file" => Some("delete_file"),
        "mkdir" | "makeDir" | "make_dir" => Some("make_dir"),
        "encodeHex" | "encode_hex" => Some("encode_hex"),
        "decodeHex" | "decode_hex" => Some("decode_hex"),
        "encodeBase64" | "encode_base64" | "b64encode" => Some("encode_base64"),
        "sprintf_int" | "sprintfInt" => Some("sprintf_int"),
        "format" | "str_format" => Some("str_format"),
        "jsonStringifyInt" | "json_stringify_int" => Some("json_stringify_int"),
        "jsonParseInt" | "json_parse_int" => Some("json_parse_int"),
        // 并发
        "channel" => Some("vredrs_make_channel"),
        "send" => Some("vredrs_send"),
        "recv" => Some("vredrs_recv"),
        "close" | "closeChannel" => Some("vredrs_close"),
        "mutex" => Some("vredrs_make_mutex"),
        "lock" => Some("vredrs_lock"),
        "unlock" => Some("vredrs_unlock"),
        _ => None,
    }
}

/// 对外入口：把程序 lower 成 IR 模块。
/// stage3：在 lowering 前先跑 monomorphization（泛型函数实例化），
/// 然后再走正常的 AST → IR 流程。
pub fn lower_program(program: &Program) -> IrModule {
    // stage3：跑 monomorphization（如果有泛型函数定义）。
    let has_generics = program.declarations.iter().any(|d| {
        if let TopLevel::FnDef(f) = d {
            !f.type_params.is_empty()
        } else {
            false
        }
    });
    if has_generics {
        let mut mono = crate::codegen::raw::monomorphization::Monomorphizer::new();
        let mono_prog = mono.run(program);
        Lowerer::new().lower(&mono_prog)
    } else {
        Lowerer::new().lower(program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Span;

    fn empty_span() -> Span {
        Span::dummy()
    }

    #[test]
    fn lower_simple_function() {
        // fn, add(a: int, b: int): int { return, a + b }
        let prog = Program {
            declarations: vec![TopLevel::FnDef(FnDef {
                annotations: vec![],
                name: Identifier { name: "add".into(), span: empty_span() },
                params: vec![
                    ast::FnParam { name: Identifier { name: "a".into(), span: empty_span() }, type_annotation: Some(TypeExpr::Basic(BasicType::Int, empty_span())), default_value: None, is_variadic: false, span: empty_span() },
                    ast::FnParam { name: Identifier { name: "b".into(), span: empty_span() }, type_annotation: Some(TypeExpr::Basic(BasicType::Int, empty_span())), default_value: None, is_variadic: false, span: empty_span() },
                ],
                return_type: Some(TypeExpr::Basic(BasicType::Int, empty_span())),
                body: vec![Stmt::Return(ast::ReturnStmt {
                    values: vec![Expr::Binary(ast::BinaryExpr {
                        left: Box::new(Expr::Identifier(Identifier { name: "a".into(), span: empty_span() })),
                        operator: BinaryOp::Add,
                        right: Box::new(Expr::Identifier(Identifier { name: "b".into(), span: empty_span() })),
                        span: empty_span(),
                    })],
                    span: empty_span(),
                })],
                is_constexpr: false,
                is_lazy: false,
                is_async: false,
                is_extern: false,
                extern_link: None,
                type_constraints: Default::default(),
                type_params: vec![],
                span: empty_span(),
            })],
            span: empty_span(),
        };
        let m = lower_program(&prog);
        assert_eq!(m.functions.len(), 1);
        let f = &m.functions[0];
        assert_eq!(f.name, "add");
        assert_eq!(f.params.len(), 2);
        assert_eq!(f.params[0].1, TypeHint::I64);
        // body 应包含 Bin{Add} 和 Ret
        let has_add = f.body.iter().any(|i| matches!(i, StaticInsn::Bin { op: BinOp::Add, .. }));
        assert!(has_add, "expected a static Add in IR body: {:?}", f.body);
        let has_ret = f.body.iter().any(|i| matches!(i, StaticInsn::Ret { .. }));
        assert!(has_ret);
    }

    #[test]
    fn lower_loop_has_explicit_arm() {
        // loop { break } — 手册 §3.3 要求 loop 有显式 arm，不再静默跳过。
        let prog = Program {
            declarations: vec![TopLevel::Statement(Stmt::Loop(ast::LoopStmt {
                label: None,
                body: vec![Stmt::Break(ast::BreakStmt { label: None, span: empty_span() })],
                else_body: None,
                span: empty_span(),
            }))],
            span: empty_span(),
        };
        let m = lower_program(&prog);
        let main = m.find_function("main").unwrap();
        // 应该有 Label（loop_head）和 Jump（回跳）以及 Break 的 Jump。
        let has_label = main.body.iter().any(|i| matches!(i, StaticInsn::Label(_)));
        let has_jump = main.body.iter().any(|i| matches!(i, StaticInsn::Jump { .. }));
        assert!(has_label, "loop should emit labels: {:?}", main.body);
        assert!(has_jump, "loop should emit jumps: {:?}", main.body);
    }

    #[test]
    fn lower_dynamic_uses_runtime() {
        // 无类型注解的 fib：x + y 应该用 AddDyn。
        let prog = Program {
            declarations: vec![TopLevel::FnDef(FnDef {
                annotations: vec![],
                name: Identifier { name: "fib".into(), span: empty_span() },
                params: vec![ast::FnParam { name: Identifier { name: "n".into(), span: empty_span() }, type_annotation: None, default_value: None, is_variadic: false, span: empty_span() }],
                return_type: None,
                body: vec![Stmt::Return(ast::ReturnStmt {
                    values: vec![Expr::Binary(ast::BinaryExpr {
                        left: Box::new(Expr::Identifier(Identifier { name: "n".into(), span: empty_span() })),
                        operator: BinaryOp::Add,
                        right: Box::new(Expr::Integer(ast::IntegerLiteral { value: 1, raw: "1".into(), span: empty_span() })),
                        span: empty_span(),
                    })],
                    span: empty_span(),
                })],
                is_constexpr: false, is_lazy: false, is_async: false, is_extern: false, extern_link: None,
                type_constraints: Default::default(), type_params: vec![], span: empty_span(),
            })],
            span: empty_span(),
        };
        let m = lower_program(&prog);
        let f = m.find_function("fib").unwrap();
        // n 是 Dynamic，1 是 I64 → 混合 → AddDyn。
        let has_dyn = f.body.iter().any(|i| matches!(i, StaticInsn::Bin { op: BinOp::AddDyn, .. }));
        assert!(has_dyn, "expected AddDyn for dynamic operand: {:?}", f.body);
    }

    #[test]
    fn lower_asm_preserves_constraints() {
        // asm!("add $0, $1", inputs=["r"(x)], outputs=["=r"(y)])
        let prog = Program {
            declarations: vec![TopLevel::Statement(Stmt::Asm(ast::AsmStmt {
                template: "add $0, $1".into(),
                inputs: vec![ast::AsmOperand { constraint: "r".into(), expr: Expr::Integer(ast::IntegerLiteral { value: 1, raw: "1".into(), span: empty_span() }), span: empty_span() }],
                outputs: vec![ast::AsmOperand { constraint: "=r".into(), expr: Expr::Identifier(Identifier { name: "y".into(), span: empty_span() }), span: empty_span() }],
                span: empty_span(),
            }))],
            span: empty_span(),
        };
        let m = lower_program(&prog);
        let main = m.find_function("main").unwrap();
        let has_asm = main.body.iter().any(|i| matches!(i, StaticInsn::Asm { .. }));
        assert!(has_asm);
        if let Some(StaticInsn::Asm { inputs, outputs, .. }) = main.body.iter().find_map(|i| if let StaticInsn::Asm { inputs, outputs, template, clobbers } = i { Some(i) } else { None }) {
            assert_eq!(inputs[0].constraint, "r");
            assert_eq!(outputs[0].constraint, "=r");
        }
    }
}
