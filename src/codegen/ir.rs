//! # Static IR — 所有后端共享的中间表示
//!
//! 这是《Vredrs 架构优化手册》Phase 2 定义的核心组件。
//!
//! ## 设计目标
//!
//! - **单一输入点**：`lower.rs` 是唯一把 AST 翻译成 IR 的地方，后端不再啃 AST。
//! - **与后端无关**：`Operand::Reg` 使用抽象寄存器名（`v0`, `v1`...），由各后端
//!   的寄存器分配器映射到具体物理寄存器（x86 的 `rax`/`rbx`、ARM 的 `r0`/`r1`、
//!   LLVM 的 `%0`/`%1`）。
//! - **渐进式类型标记**（手册 Phase 3 修正版）：每条指令携带 `TypeHint`，区分
//!   - `I64`/`F64`/`Bool`/`Ptr`/`Str`：已知静态类型，后端可生成原生运算
//!   - `Dynamic`：未知类型，后端降级为 runtime 调用（`vredrs_value_add` 等）
//!   - `Mixed`：带类型提示但最终由后端决定是否静态化
//! - **Cstar 可过滤**（手册 Phase 4）：IR 中包含 `Prefetch`/`PipelineMarker`/
//!   `IsrEntry`/`WcetNote` 等调度指令，由 `cstar::filter` 插入或改写。
//!
//! ## 指令集规模
//!
//! 约 30 种指令，足以表达所有静态可编译的 AST 节点（参见手册 §4.2）。

use std::fmt;

/// 抽象寄存器名。由 `lower.rs` 分配（`v0`, `v1`, ...），由各后端的寄存器
/// 分配器映射到物理寄存器或 LLVM 虚拟寄存器。
pub type Reg = String;
/// 标签名（如 `.L0`, `.L1`）。
pub type Label = String;

/// 类型提示。手册 Phase 3 修正：LLVM 是渐进式后端，IR 必须保留类型标记但不
/// 强制要求所有操作都有静态类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeHint {
    /// 已知 64 位有符号整数。
    I64,
    /// 已知 64 位浮点数。
    F64,
    /// 已知布尔（在 Raw/LLVM 中用 i64 的 0/1 表示）。
    Bool,
    /// 已知指针（ptr[T]）。
    Ptr,
    /// 字符串切片（指针 + 长度）。
    Str,
    /// 单元类型 `()`，用于无返回值的函数。
    Unit,
    /// 动态 Value（VM 语义）。后端应降级为 runtime 调用。
    Dynamic,
    /// 混合：带类型提示但最终由后端决定是否静态化。
    Mixed,
}

impl TypeHint {
    /// 是否为已知静态类型（后端可生成原生运算）。
    pub fn is_static(self) -> bool {
        matches!(self, TypeHint::I64 | TypeHint::F64 | TypeHint::Bool | TypeHint::Ptr | TypeHint::Str | TypeHint::Unit)
    }
    /// 是否需要降级为 runtime 调用。
    pub fn is_dynamic(self) -> bool {
        matches!(self, TypeHint::Dynamic | TypeHint::Mixed)
    }
}

impl fmt::Display for TypeHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeHint::I64 => write!(f, "i64"),
            TypeHint::F64 => write!(f, "f64"),
            TypeHint::Bool => write!(f, "bool"),
            TypeHint::Ptr => write!(f, "ptr"),
            TypeHint::Str => write!(f, "str"),
            TypeHint::Unit => write!(f, "unit"),
            TypeHint::Dynamic => write!(f, "dyn"),
            TypeHint::Mixed => write!(f, "mix"),
        }
    }
}

/// 抽象操作数。不依赖具体后端。
#[derive(Debug, Clone)]
pub enum Operand {
    /// 虚拟寄存器 + 类型提示。
    Reg(Reg, TypeHint),
    /// 64 位整数立即数。
    ImmI64(i64),
    /// 64 位浮点立即数。
    ImmF64(f64),
    /// 字符串常量（引用模块级字符串池的索引）。
    ImmStr(usize),
    /// 布尔立即数。
    ImmBool(bool),
    /// 空值。
    Null,
    /// 标签引用（用于跳转表等）。
    Label(Label),
    /// 全局符号/函数名（用于 `Call` 的直接调用）。
    Sym(String),
}

impl Operand {
    /// 该操作数的类型提示。
    pub fn ty(&self) -> TypeHint {
        match self {
            Operand::Reg(_, t) => *t,
            Operand::ImmI64(_) => TypeHint::I64,
            Operand::ImmF64(_) => TypeHint::F64,
            Operand::ImmStr(_) => TypeHint::Str,
            Operand::ImmBool(_) => TypeHint::Bool,
            Operand::Null => TypeHint::Dynamic,
            Operand::Label(_) | Operand::Sym(_) => TypeHint::Ptr,
        }
    }
    /// 是否为寄存器。
    pub fn is_reg(&self) -> bool {
        matches!(self, Operand::Reg(..))
    }
    /// 是否为立即数。
    pub fn is_imm(&self) -> bool {
        matches!(self, Operand::ImmI64(_) | Operand::ImmF64(_) | Operand::ImmBool(_) | Operand::Null)
    }
    /// 若为寄存器，返回其名字。
    pub fn reg_name(&self) -> Option<&str> {
        if let Operand::Reg(n, _) = self {
            Some(n.as_str())
        } else {
            None
        }
    }
}

/// 比较条件码。用于 `Branch`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cond {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// 二元运算种类。区分静态版本与动态（runtime）版本，呼应手册 Phase 3 的
/// "渐进式"语义：静态路径用 `Add` 等，动态路径用 `AddDyn`（后端翻译为
/// `vredrs_value_add` 调用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    /// 动态加法（调用 runtime `vredrs_value_add`）。
    AddDyn,
    SubDyn,
    MulDyn,
    DivDyn,
    ModDyn,
    EqDyn,
    NeDyn,
    LtDyn,
    LeDyn,
    GtDyn,
    GeDyn,
}

impl BinOp {
    /// 是否为动态版本。
    pub fn is_dyn(self) -> bool {
        matches!(
            self,
            BinOp::AddDyn | BinOp::SubDyn | BinOp::MulDyn | BinOp::DivDyn | BinOp::ModDyn
                | BinOp::EqDyn | BinOp::NeDyn | BinOp::LtDyn | BinOp::LeDyn | BinOp::GtDyn | BinOp::GeDyn
        )
    }
}

/// 内联汇编操作数（保留 GCC 约束信息，手册 §3.3 完成标准之一）。
#[derive(Debug, Clone)]
pub struct AsmOperand {
    pub constraint: String,
    pub operand: Operand,
}

/// 静态 IR 指令。约 30 种，覆盖手册 §4.2 的指令集设计。
#[derive(Debug, Clone)]
pub enum StaticInsn {
    // ── 元信息 ──────────────────────────────────────────────
    /// 标签定义。
    Label(Label),
    /// 注释（保留在最终输出中，便于调试；也用于 Cstar 差分元数据）。
    Comment(String),
    /// 源码行号标记（供 DWARF 调试信息使用）。
    SrcLoc { line: u32, col: u32 },

    // ── 数据移动 ────────────────────────────────────────────
    /// `dst = src`，带类型标记。
    Mov { dst: Operand, src: Operand, ty: TypeHint },
    /// 静态→动态装箱（有注解表达式赋值给无注解变量时自动装箱）。
    Box { dst: Operand, src: Operand, from: TypeHint },
    /// 动态→静态拆箱（无注解变量参与静态运算前自动拆箱）。
    Unbox { dst: Operand, src: Operand, to: TypeHint },

    // ── 算术/逻辑 ──────────────────────────────────────────
    /// 二元运算：`dst = lhs op rhs`。
    Bin { op: BinOp, dst: Operand, lhs: Operand, rhs: Operand, ty: TypeHint },
    /// 一元负号。
    Neg { dst: Operand, src: Operand, ty: TypeHint },
    /// 逻辑非 / 按位取反。
    Not { dst: Operand, src: Operand, ty: TypeHint },
    /// 比较，产出 bool 结果到 `dst`。
    Cmp { dst: Operand, cond: Cond, lhs: Operand, rhs: Operand, ty: TypeHint },

    // ── 控制流 ──────────────────────────────────────────────
    /// 条件跳转：若 `cond(lhs, rhs)` 成立则跳转到 `target`。
    /// 这是手册 §4.2 的 `Branch` 形态——把比较与跳转合并，便于后端生成
    /// `cmp + jcc` 或 `br i1`。
    Branch { cond: Cond, lhs: Operand, rhs: Operand, target: Label },
    /// 若 `cond` 为真则跳转（`cond` 已是 bool）。
    BranchTrue { cond: Operand, target: Label },
    /// 无条件跳转。
    Jump { target: Label },

    // ── 内存 ────────────────────────────────────────────────
    /// `dst = *(addr)`，读取 `size` 字节。
    Load { dst: Operand, addr: Operand, size: u8, ty: TypeHint },
    /// `*(addr) = value`，写入 `size` 字节。
    Store { addr: Operand, value: Operand, size: u8, ty: TypeHint },
    /// 栈分配：`dst = alloca size`，对齐 `align`。
    Alloca { dst: Operand, size: Operand, align: u8 },
    /// 栈释放（手动管理，用于 ptr[T] 区域）。
    StackFree { ptr: Operand, size: Operand },

    // ── 函数调用 ────────────────────────────────────────────
    /// 直接调用：`dst = func(args...)`。
    Call { dst: Option<Operand>, func: String, args: Vec<Operand>, ty: TypeHint },
    /// 动态分派调用（接收者是动态值）：`dst = callee(args...)`。
    CallDyn { dst: Option<Operand>, callee: Operand, args: Vec<Operand>, ty: TypeHint },
    /// 运行时辅助调用（动态特性降级路径，如 `vredrs_value_add`）。
    RuntimeCall { dst: Option<Operand>, func: String, args: Vec<Operand> },
    /// 返回。
    Ret { value: Option<Operand> },

    // ── 栈 ──────────────────────────────────────────────────
    Push { src: Operand },
    Pop { dst: Operand },

    // ── 内联汇编 ────────────────────────────────────────────
    /// GCC 风格内联汇编。手册 §3.3 要求保留 `"=r"`/`"r"` 约束信息。
    Asm { template: String, inputs: Vec<AsmOperand>, outputs: Vec<AsmOperand>, clobbers: Vec<String> },

    // ── Cstar 调度指令（Phase 4 由 filter 插入）────────────
    /// DMA 预取：从 `addr` 预取数据。
    Prefetch { addr: Operand, hint: String },
    /// 流水线标记（`@pipeline` 注解的函数入口）。
    PipelineMarker { name: String },
    /// ISR 入口标记（`@isr_group` 注解的函数）。
    IsrEntry { group: String, priority: u32 },
    /// WCET 注释（编译期计算的循环周期数 + 预算）。
    WcetNote { cycles: u64, budget: u64 },
    /// 差分元数据（嵌入符号偏移信息，供 `@patch` 使用）。
    DiffMeta { symbol: String, offset: u64 },
}

/// IR 函数。
#[derive(Debug, Clone)]
pub struct IrFunction {
    pub name: String,
    pub params: Vec<(String, TypeHint)>,
    /// 每个参数对应的虚拟寄存器名（如 "v0", "v1"），供后端定位栈槽。
    pub param_regs: Vec<String>,
    pub return_ty: TypeHint,
    pub body: Vec<StaticInsn>,
    pub is_main: bool,
    /// 函数上的注解名（`@pipeline`, `@isr_group`, `@patch`, `@repo`...）。
    pub annotations: Vec<String>,
    /// 局部变量名→类型提示（供后端做寄存器分配与类型推断）。
    pub locals: Vec<(String, TypeHint)>,
}

/// IR 模块：一个源文件 lowered 后的完整 IR。
#[derive(Debug, Clone, Default)]
pub struct IrModule {
    pub functions: Vec<IrFunction>,
    /// 字符串常量池。`Operand::ImmStr(idx)` 引用此处。
    pub string_pool: Vec<String>,
    /// 全局变量（名 + 类型）。
    pub globals: Vec<(String, TypeHint)>,
    /// 入口函数名（通常是 `main`）。
    pub entry: Option<String>,
}

impl IrModule {
    pub fn new() -> Self {
        Self::default()
    }
    /// 把字符串加入池中，返回索引。
    pub fn intern_string(&mut self, s: &str) -> usize {
        if let Some(i) = self.string_pool.iter().position(|x| x == s) {
            return i;
        }
        let i = self.string_pool.len();
        self.string_pool.push(s.to_string());
        i
    }
    /// 查找函数。
    pub fn find_function(&self, name: &str) -> Option<&IrFunction> {
        self.functions.iter().find(|f| f.name == name)
    }
}

/// IR 可视化（用于 `.ir` 调试输出与 Cstar 只读分析报告）。
pub fn render_module(m: &IrModule) -> String {
    let mut out = String::new();
    out.push_str("; Vredrs Static IR\n");
    if !m.string_pool.is_empty() {
        out.push_str("; -- string pool --\n");
        for (i, s) in m.string_pool.iter().enumerate() {
            out.push_str(&format!(";  @str{} = {:?}\n", i, s));
        }
    }
    if !m.globals.is_empty() {
        out.push_str("; -- globals --\n");
        for (n, t) in &m.globals {
            out.push_str(&format!(";  {} : {}\n", n, t));
        }
    }
    for f in &m.functions {
        out.push_str(&format!("\nfn {}(", f.name));
        let mut first = true;
        for (n, t) in &f.params {
            if !first {
                out.push_str(", ");
            }
            first = false;
            out.push_str(&format!("{}: {}", n, t));
        }
        out.push_str(&format!(") -> {} {{\n", f.return_ty));
        if !f.annotations.is_empty() {
            out.push_str(&format!("; annotations: {}\n", f.annotations.join(", ")));
        }
        for insn in &f.body {
            out.push_str("  ");
            out.push_str(&render_insn(insn));
            out.push('\n');
        }
        out.push_str("}\n");
    }
    out
}

/// 单条指令的可视化。
pub fn render_insn(i: &StaticInsn) -> String {
    use StaticInsn::*;
    match i {
        Label(l) => format!("{}:", l),
        Comment(c) => format!("; {}", c),
        SrcLoc { line, col } => format!("; srcloc {}:{}", line, col),
        Mov { dst, src, ty } => format!("mov.{} {} = {}", ty, op(dst), op(src)),
        Box { dst, src, from } => format!("box {} = {}({})", op(dst), op(src), from),
        Unbox { dst, src, to } => format!("unbox {} = {}({})", op(dst), op(src), to),
        Bin { op: bop, dst, lhs, rhs, ty } => format!("{}.{} {} = {} {}", op_name(*bop), ty, op(dst), op(lhs), op(rhs)),
        Neg { dst, src, ty } => format!("neg.{} {} = {}", ty, op(dst), op(src)),
        Not { dst, src, ty } => format!("not.{} {} = {}", ty, op(dst), op(src)),
        Cmp { dst, cond, lhs, rhs, ty } => format!("cmp.{} {} = {} {:?} {}", ty, op(dst), op(lhs), cond, op(rhs)),
        Branch { cond, lhs, rhs, target } => format!("br {:?} {} {} -> {}", cond, op(lhs), op(rhs), target),
        BranchTrue { cond, target } => format!("brt {} -> {}", op(cond), target),
        Jump { target } => format!("jmp {}", target),
        Load { dst, addr, size, ty } => format!("load.{} {} = *[{}:{}]", ty, op(dst), op(addr), size),
        Store { addr, value, size, ty } => format!("store.{} *[{}:{}] = {}", ty, op(addr), size, op(value)),
        Alloca { dst, size, align } => format!("alloca {} = [{}], align {}", op(dst), op(size), align),
        StackFree { ptr, size } => format!("stackfree [{}] size {}", op(ptr), op(size)),
        Call { dst, func, args, ty } => {
            let a: Vec<String> = args.iter().map(op).collect();
            match dst {
                Some(d) => format!("call.{} {} = {}({})", ty, op(d), func, a.join(", ")),
                None => format!("call.{} {}({})", ty, func, a.join(", ")),
            }
        }
        CallDyn { dst, callee, args, ty } => {
            let a: Vec<String> = args.iter().map(op).collect();
            match dst {
                Some(d) => format!("calldyn.{} {} = {}({})", ty, op(d), op(callee), a.join(", ")),
                None => format!("calldyn.{} {}({})", ty, op(callee), a.join(", ")),
            }
        }
        RuntimeCall { dst, func, args } => {
            let a: Vec<String> = args.iter().map(op).collect();
            match dst {
                Some(d) => format!("rt {} = {}({})", op(d), func, a.join(", ")),
                None => format!("rt {}({})", func, a.join(", ")),
            }
        }
        Ret { value } => match value {
            Some(v) => format!("ret {}", op(v)),
            None => "ret".to_string(),
        },
        Push { src } => format!("push {}", op(src)),
        Pop { dst } => format!("pop {}", op(dst)),
        Asm { template, inputs, outputs, clobbers } => {
            let i: Vec<String> = inputs.iter().map(|x| format!("{}({})", x.constraint, op(&x.operand))).collect();
            let o: Vec<String> = outputs.iter().map(|x| format!("{}({})", x.constraint, op(&x.operand))).collect();
            format!("asm {:?} ins=[{}] outs=[{}] clob=[{}]", template, i.join(", "), o.join(", "), clobbers.join(", "))
        }
        Prefetch { addr, hint } => format!("prefetch {} hint={}", op(addr), hint),
        PipelineMarker { name } => format!("@pipeline {}", name),
        IsrEntry { group, priority } => format!("@isr_group {} prio={}", group, priority),
        WcetNote { cycles, budget } => format!("; wcet cycles={} budget={}", cycles, budget),
        DiffMeta { symbol, offset } => format!("; diffmeta {} +{:#x}", symbol, offset),
    }
}

fn op(o: &Operand) -> String {
    match o {
        Operand::Reg(n, t) => format!("{}:{}", n, t),
        Operand::ImmI64(v) => v.to_string(),
        Operand::ImmF64(v) => format!("{}f", v),
        Operand::ImmStr(i) => format!("@str{}", i),
        Operand::ImmBool(b) => b.to_string(),
        Operand::Null => "null".to_string(),
        Operand::Label(l) => l.clone(),
        Operand::Sym(s) => format!("@{}", s),
    }
}

fn op_name(o: BinOp) -> &'static str {
    match o {
        BinOp::Add => "add",
        BinOp::Sub => "sub",
        BinOp::Mul => "mul",
        BinOp::Div => "div",
        BinOp::Mod => "mod",
        BinOp::And => "and",
        BinOp::Or => "or",
        BinOp::Xor => "xor",
        BinOp::Shl => "shl",
        BinOp::Shr => "shr",
        BinOp::AddDyn => "add.dyn",
        BinOp::SubDyn => "sub.dyn",
        BinOp::MulDyn => "mul.dyn",
        BinOp::DivDyn => "div.dyn",
        BinOp::ModDyn => "mod.dyn",
        BinOp::EqDyn => "eq.dyn",
        BinOp::NeDyn => "ne.dyn",
        BinOp::LtDyn => "lt.dyn",
        BinOp::LeDyn => "le.dyn",
        BinOp::GtDyn => "gt.dyn",
        BinOp::GeDyn => "ge.dyn",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_string_dedup() {
        let mut m = IrModule::new();
        let a = m.intern_string("hello");
        let b = m.intern_string("hello");
        let c = m.intern_string("world");
        assert_eq!(a, 0);
        assert_eq!(b, 0);
        assert_eq!(c, 1);
    }

    #[test]
    fn type_hint_classification() {
        assert!(TypeHint::I64.is_static());
        assert!(TypeHint::F64.is_static());
        assert!(!TypeHint::Dynamic.is_static());
        assert!(TypeHint::Dynamic.is_dynamic());
        assert!(TypeHint::Mixed.is_dynamic());
    }

    #[test]
    fn render_insn_smoke() {
        let m = IrModule {
            functions: vec![IrFunction {
                name: "f".into(),
                params: vec![("x".into(), TypeHint::I64)],
                param_regs: vec!["v0".into()],
                return_ty: TypeHint::I64,
                body: vec![
                    StaticInsn::Bin { op: BinOp::Add, dst: Operand::Reg("v1".into(), TypeHint::I64), lhs: Operand::Reg("v0".into(), TypeHint::I64), rhs: Operand::ImmI64(1), ty: TypeHint::I64 },
                    StaticInsn::Ret { value: Some(Operand::Reg("v1".into(), TypeHint::I64)) },
                ],
                is_main: false,
                annotations: vec![],
                locals: vec![],
            }],
            string_pool: vec![],
            globals: vec![],
            entry: Some("f".into()),
        };
        let s = render_module(&m);
        assert!(s.contains("fn f(x: i64) -> i64"));
        assert!(s.contains("add.i64"));
        assert!(s.contains("ret"));
    }

    #[test]
    fn operand_ty_inference() {
        assert_eq!(Operand::ImmI64(5).ty(), TypeHint::I64);
        assert_eq!(Operand::ImmF64(1.5).ty(), TypeHint::F64);
        assert_eq!(Operand::ImmBool(true).ty(), TypeHint::Bool);
        assert_eq!(Operand::Null.ty(), TypeHint::Dynamic);
    }
}
