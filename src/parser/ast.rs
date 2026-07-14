//! vredrs 1.0 抽象语法树（AST）定义
//!
//! 本文件包含编译器前端所需的全部 AST 节点类型。
//! 设计原则：
//! 1. 每个语法构造对应一个或多个 AST 节点
//! 2. 所有节点携带 Span 信息用于错误报告
//! 3. 节点类型尽量扁平，避免过度嵌套
//! 4. 使用 Box 处理递归类型以避免无限大小

use crate::error::Span;
use std::collections::HashMap;

// ============================================================================
// 一、程序顶层
// ============================================================================

/// 整个源文件的根节点
#[derive(Debug, Clone)]
pub struct Program {
    pub declarations: Vec<TopLevel>,
    pub span: Span,
}

/// 顶层声明：可以是任何顶层语句或声明
#[derive(Debug, Clone)]
pub enum TopLevel {
    Import(ImportStmt),
    Export(ExportStmt),
    FnDef(FnDef),
    StructDef(StructDef),
    ClassDef(ClassDef),
    InterfaceDef(InterfaceDef),
    EnumDef(EnumDef),
    TypeAlias(TypeAlias),
    ConstExpr(ConstExpr),
    LazyDef(LazyDef),
    LazyFnDef(LazyFnDef),
    MacroDef(MacroDef),
    PluginDef(PluginDef),
    MarkerTrait(MarkerTrait),
    ExternFnDef(ExternFnDef),
    ConditionalCompile(ConditionalCompile),
    /// `test, "name" ... /end` — a named test block collected by `vredrs test`.
    TestBlock(TestBlock),
    /// `bench, "name" ... /end` — a named benchmark block collected by
    /// `vredrs test` (and reported as timing, not pass/fail).
    BenchBlock(BenchBlock),
    TraitDef(TraitDef),
    ImplBlock(ImplBlock),
    DtorBlock(DtorBlock),
    Statement(Stmt),
}

/// A named test block.
///
/// ```vredrs
/// test, "加法"
///     assert, 1 + 1 == 2
/// /end
/// ```
#[derive(Debug, Clone)]
pub struct TestBlock {
    pub name: String,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// A named benchmark block. The body is run and the wall-clock duration is
/// printed; no pass/fail semantics.
#[derive(Debug, Clone)]
pub struct BenchBlock {
    pub name: String,
    pub iterations: i64,
    pub body: Vec<Stmt>,
    pub span: Span,
}

// ============================================================================
// 二、导入导出
// ============================================================================

#[derive(Debug, Clone)]
pub struct ImportStmt {
    pub module: String,
    pub alias: Option<Identifier>,
    pub symbols: Option<Vec<Identifier>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ExportStmt {
    pub symbols: Vec<Identifier>,
    pub span: Span,
}

// ============================================================================
// 三、声明
// ============================================================================

#[derive(Debug, Clone)]
pub struct FnDef {
    pub annotations: Vec<Annotation>,
    pub name: Identifier,
    pub params: Vec<FnParam>,
    pub return_type: Option<TypeExpr>,
    pub body: Vec<Stmt>,
    pub is_constexpr: bool,
    pub is_lazy: bool,
    pub is_async: bool,
    pub is_extern: bool,
    pub extern_link: Option<String>,
    /// Generic type-parameter constraints: for `fn, render[T: Drawable](obj: T)`,
    /// this maps "T" → "Drawable". At runtime, the VM checks that the argument
    /// has the required interface methods (if the constraint names an interface).
    pub type_constraints: HashMap<String, Vec<String>>,
    /// Generic type-parameter names in declaration order (e.g. `["T", "U"]`
    /// for `fn, f[T, U](...)`). Empty for non-generic functions. The
    /// constraints for each name are in `type_constraints`.
    pub type_params: Vec<String>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FnParam {
    pub name: Identifier,
    pub type_annotation: Option<TypeExpr>,
    pub default_value: Option<Expr>,
    pub is_variadic: bool, // args...
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructDef {
    pub annotations: Vec<Annotation>,
    pub name: Identifier,
    pub fields: Vec<StructField>,
    pub methods: Vec<FnDef>,
    /// Generic type-parameter names (e.g. `T`, `U` in `struct, Pair[T, U]`).
    /// Constraints are stored in `type_constraints`.
    pub type_params: Vec<String>,
    /// Generic type-parameter constraints (same shape as FnDef.type_constraints).
    pub type_constraints: HashMap<String, Vec<String>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructField {
    pub annotations: Vec<Annotation>,
    pub name: Identifier,
    pub type_annotation: Option<TypeExpr>,
    pub default_value: Option<Expr>,
    pub visibility: Visibility,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ClassDef {
    pub annotations: Vec<Annotation>,
    pub name: Identifier,
    pub extends: Option<TypeExpr>,
    pub implements: Vec<TypeExpr>,
    pub fields: Vec<ClassField>,
    pub methods: Vec<FnDef>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ClassField {
    pub annotations: Vec<Annotation>,
    pub name: Identifier,
    pub type_annotation: Option<TypeExpr>,
    pub default_value: Option<Expr>,
    pub visibility: Visibility,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct InterfaceDef {
    pub annotations: Vec<Annotation>,
    pub name: Identifier,
    pub extends: Vec<TypeExpr>,
    pub methods: Vec<InterfaceMethod>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct InterfaceMethod {
    pub name: Identifier,
    pub params: Vec<FnParam>,
    pub return_type: Option<TypeExpr>,
    pub default_body: Option<Vec<Stmt>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumDef {
    pub annotations: Vec<Annotation>,
    pub name: Identifier,
    pub variants: Vec<EnumVariant>,
    /// Generic type-parameter names (e.g. `T` in `enum, Option[T]`).
    pub type_params: Vec<String>,
    /// Generic type-parameter constraints.
    pub type_constraints: HashMap<String, Vec<String>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub name: Identifier,
    pub payload: Option<EnumVariantPayload>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum EnumVariantPayload {
    /// 元组变体：Ok(value)
    Tuple(Vec<TypeExpr>),
    /// 结构体变体：Person{name: str, age: int}
    Struct(Vec<StructField>),
}

#[derive(Debug, Clone)]
pub struct TypeAlias {
    pub name: Identifier,
    pub target: TypeExpr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ConstExpr {
    pub name: Identifier,
    pub value: Expr,
    pub type_annotation: Option<TypeExpr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct LazyDef {
    pub name: Identifier,
    pub value: Expr,
    pub type_annotation: Option<TypeExpr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct LazyFnDef {
    pub fn_def: FnDef,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MacroDef {
    pub name: Identifier,
    pub params: Vec<FnParam>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct PluginDef {
    pub name: String,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MarkerTrait {
    pub name: Identifier,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ExternFnDef {
    pub link: Option<String>,
    pub fn_def: FnDef,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ConditionalCompile {
    pub condition: Expr,
    pub then_body: Vec<TopLevel>,
    pub else_body: Option<Vec<TopLevel>>,
    pub span: Span,
}

// ============================================================================
// 四、注解
// ============================================================================

#[derive(Debug, Clone)]
pub struct Annotation {
    pub name: String,
    pub arguments: Vec<AnnotationArg>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct AnnotationArg {
    pub name: Option<String>,
    pub value: Expr,
    pub span: Span,
}

// ============================================================================
// 五、可见性
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
}

impl Default for Visibility {
    fn default() -> Self {
        Visibility::Private
    }
}

// ============================================================================
// 六、语句
// ============================================================================

#[derive(Debug, Clone)]
pub enum Stmt {
    Assign(AssignStmt),
    Pon(PonStmt),
    TableAssign(TableAssignStmt),
    Paste(PasteStmt),
    Println(PrintlnStmt),
    Flush(FlushStmt),
    Input(InputStmt),
    Return(ReturnStmt),
    Throw(ThrowStmt),
    Defer(DeferStmt),
    Assert(AssertStmt),
    Panic(PanicStmt),
    Spawn(SpawnStmt),
    SpawnThread(SpawnThreadStmt),
    Yield(YieldStmt),
    If(IfStmt),
    While(WhileStmt),
    ForIn(ForInStmt),
    ForRange(ForRangeStmt),
    Loop(LoopStmt),
    Break(BreakStmt),
    Continue(ContinueStmt),
    Match(MatchStmt),
    Try(TryStmt),
    With(WithStmt),
    Select(SelectStmt),
    UnsafeBlock(UnsafeBlock),
    Asm(AsmStmt),
    DirectiveBlock(DirectiveBlock),
    ScopeBlock(ScopeBlock),
    Expr(ExprStmt),
}

#[derive(Debug, Clone)]
pub struct AssignStmt {
    pub targets: Vec<Assignee>,
    pub value: Expr,
    pub operator: AssignOp,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Assignee {
    Identifier(Identifier),
    Qualified(QualifiedName),
    Index(IndexExpr),
    Member(MemberAccessExpr),
    Tuple(Vec<Assignee>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignOp {
    Simple,  // =
    Plus,    // +=
    Minus,   // -=
    Star,    // *=
    Slash,   // /=
    Percent, // %=
    Delete,  // del, target
}

#[derive(Debug, Clone)]
pub struct PonStmt {
    pub assign: AssignStmt,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TableAssignStmt {
    pub column_names: Vec<Identifier>,
    pub rows: Vec<Vec<Expr>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct PasteStmt {
    pub args: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct PrintlnStmt {
    pub args: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FlushStmt {
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct InputStmt {
    pub target: Assignee,
    pub prompt: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ReturnStmt {
    pub values: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ThrowStmt {
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct DeferStmt {
    pub stmt: Box<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct AssertStmt {
    pub condition: Expr,
    pub message: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct PanicStmt {
    pub message: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SpawnStmt {
    pub call: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SpawnThreadStmt {
    pub call: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct YieldStmt {
    pub value: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct IfStmt {
    pub condition: Expr,
    pub then_body: Vec<Stmt>,
    pub elif_chain: Vec<(Expr, Vec<Stmt>)>,
    pub else_body: Option<Vec<Stmt>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct WhileStmt {
    pub label: Option<Identifier>,
    pub condition: Expr,
    pub body: Vec<Stmt>,
    pub else_body: Option<Vec<Stmt>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ForInStmt {
    pub label: Option<Identifier>,
    pub var: Identifier,
    pub iterable: Expr,
    pub body: Vec<Stmt>,
    pub else_body: Option<Vec<Stmt>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ForRangeStmt {
    pub label: Option<Identifier>,
    pub var: Identifier,
    pub from: Expr,
    pub to: Expr,
    /// Optional step expression for `for, i, in, 1..10 step 2`. Defaults to 1.
    /// Stored as an Option<Expr> (not Box<Expr>) for simpler cloning.
    pub step: Option<Box<Expr>>,
    pub body: Vec<Stmt>,
    pub else_body: Option<Vec<Stmt>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct LoopStmt {
    pub label: Option<Identifier>,
    pub body: Vec<Stmt>,
    pub else_body: Option<Vec<Stmt>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct BreakStmt {
    pub label: Option<Identifier>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ContinueStmt {
    pub label: Option<Identifier>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MatchStmt {
    pub expr: Expr,
    pub cases: Vec<MatchCase>,
    pub else_case: Option<Vec<Stmt>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MatchCase {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TryStmt {
    pub try_body: Vec<Stmt>,
    pub catch_var: Option<Identifier>,
    pub catch_body: Option<Vec<Stmt>>,
    pub finally_body: Option<Vec<Stmt>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct WithStmt {
    pub manager: Expr,
    pub var: Option<Identifier>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SelectStmt {
    pub cases: Vec<SelectCase>,
    pub default_case: Option<Vec<Stmt>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SelectCase {
    pub direction: SelectDirection,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum SelectDirection {
    Send {
        channel: Expr,
        value: Expr,
    },
    Receive {
        channel: Expr,
        var: Option<Identifier>,
    },
    After(Expr), // after(duration)
}

#[derive(Debug, Clone)]
pub struct UnsafeBlock {
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct AsmStmt {
    pub template: String,
    pub inputs: Vec<AsmOperand>,
    pub outputs: Vec<AsmOperand>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct AsmOperand {
    pub constraint: String,
    pub expr: Expr,
    pub span: Span,
}

/// 指令块：/指令名 ... /end
#[derive(Debug, Clone)]
pub struct DirectiveBlock {
    pub directive: String,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// 作用域前缀块：/对象名. ... /
#[derive(Debug, Clone)]
pub struct ScopeBlock {
    pub prefix: String, // "config.server" 这样的多级路径
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// 表达式语句（包装裸表达式，如函数调用）
#[derive(Debug, Clone)]
pub struct ExprStmt {
    pub expr: Expr,
    pub span: Span,
}

// ============================================================================
// 七、表达式
// ============================================================================

#[derive(Debug, Clone)]
pub enum Expr {
    Pipe(PipeExpr),
    NullCoalesce(NullCoalesceExpr),
    Binary(BinaryExpr),
    Unary(UnaryExpr),
    Postfix(PostfixExpr),
    Call(CallExpr),
    MethodCall(MethodCallExpr),
    Index(IndexExpr),
    Slice(SliceExpr),
    MemberAccess(MemberAccessExpr),
    OptionalChain(OptionalChainExpr),
    Spread(SpreadExpr),
    /// An assignment used as an expression (e.g. in lambda bodies).
    /// Executes the assignment and returns the assigned value.
    AssignExpr(Box<AssignStmt>),
    Ternary(TernaryExpr),
    Lambda(LambdaExpr),
    Spawn(SpawnExpr),
    Coro(CoroExpr),
    Resume(ResumeExpr),
    Await(AwaitExpr),
    Cast(CastExpr),
    TryPropagate(TryPropagateExpr),
    Range(RangeExpr),
    // 字面量和基本
    Integer(IntegerLiteral),
    Float(FloatLiteral),
    String_(StringLiteral),
    MultiLineString(MultiLineStringLiteral),
    Bool(BoolLiteral),
    Null(NullLiteral),
    List(ListLiteral),
    ListComprehension(Box<ListComprehension>),
    Dict(DictLiteral),
    DictComprehension(Box<DictComprehension>),
    Set(SetLiteral),
    SetComprehension(Box<SetComprehension>),
    Tuple(TupleLiteral),
    Identifier(Identifier),
    Qualified(QualifiedName),
}

#[derive(Debug, Clone)]
pub struct PipeExpr {
    pub left: Box<Expr>,
    pub right: Box<Expr>, // 函数调用或被调表达式
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct NullCoalesceExpr {
    pub left: Box<Expr>,
    pub right: Box<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct BinaryExpr {
    pub left: Box<Expr>,
    pub operator: BinaryOp,
    pub right: Box<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryOp {
    // 逻辑
    And,
    Or,
    // 比较
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Is,
    In,
    // 位运算
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    // 算术
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Power,
    // 范围（...仅在range里用，这里指重复运算符）
    Repeated,
}

#[derive(Debug, Clone)]
pub struct UnaryExpr {
    pub operator: UnaryOp,
    pub operand: Box<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,  // -
    Not,  // not
    Bang, // !
}

#[derive(Debug, Clone)]
pub struct PostfixExpr {
    pub operand: Box<Expr>,
    pub operator: PostfixOp,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostfixOp {
    Length,   // #
    Reverse,  // ~
    AscSort,  // ^
    DescSort, // _
}

#[derive(Debug, Clone)]
pub struct CallExpr {
    pub callee: Box<Expr>,
    pub args: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MethodCallExpr {
    pub receiver: Box<Expr>,
    pub method: Identifier,
    pub args: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct IndexExpr {
    pub target: Box<Expr>,
    pub index: Box<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SliceExpr {
    pub target: Box<Expr>,
    pub start: Option<Box<Expr>>,
    pub end: Option<Box<Expr>>,
    pub step: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MemberAccessExpr {
    pub target: Box<Expr>,
    pub member: Identifier,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct OptionalChainExpr {
    pub target: Box<Expr>,
    pub chain: Vec<OptionalChainLink>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum OptionalChainLink {
    Member(Identifier),
    Call { method: Identifier, args: Vec<Expr> },
    Index(Expr),
}

#[derive(Debug, Clone)]
pub struct SpreadExpr {
    pub expr: Box<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TernaryExpr {
    pub condition: Box<Expr>,
    pub true_branch: Box<Expr>,
    pub false_branch: Box<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct LambdaExpr {
    pub params: Vec<FnParam>,
    pub body: Box<Expr>, // 单个表达式就是函数体
    pub return_type: Option<TypeExpr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SpawnExpr {
    pub call: Box<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct CoroExpr {
    pub function: Box<Expr>,
    pub args: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ResumeExpr {
    pub handle: Box<Expr>,
    pub values: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct AwaitExpr {
    pub expr: Box<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct CastExpr {
    pub expr: Box<Expr>,
    pub type_expr: TypeExpr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TryPropagateExpr {
    pub expr: Box<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct RangeExpr {
    pub start: Option<Box<Expr>>,
    pub end: Option<Box<Expr>>,
    pub inclusive: bool, // true for ... (闭区间), false for ..
    pub step: Option<Box<Expr>>,
    pub span: Span,
}

// ============================================================================
// 八、字面量
// ============================================================================

#[derive(Debug, Clone)]
pub struct IntegerLiteral {
    pub value: i64, // 暂用i64，后续大整数用big.Int
    pub raw: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FloatLiteral {
    pub value: f64,
    pub raw: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StringLiteral {
    pub parts: Vec<StringPart>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum StringPart {
    Text(String),
    Interpolation(Expr),
}

#[derive(Debug, Clone)]
pub struct MultiLineStringLiteral {
    pub parts: Vec<StringPart>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct BoolLiteral {
    pub value: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct NullLiteral {
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ListLiteral {
    pub elements: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ListComprehension {
    pub result_expr: Box<Expr>,
    pub var: Identifier,
    pub iterable: Box<Expr>,
    pub condition: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct DictLiteral {
    pub entries: Vec<(Expr, Expr)>, // (key, value)
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct DictComprehension {
    pub key_expr: Box<Expr>,
    pub value_expr: Box<Expr>,
    pub var: Identifier,
    pub iterable: Box<Expr>,
    pub condition: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SetLiteral {
    pub elements: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SetComprehension {
    pub result_expr: Box<Expr>,
    pub var: Identifier,
    pub iterable: Box<Expr>,
    pub condition: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TupleLiteral {
    pub elements: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Identifier {
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct QualifiedName {
    pub parts: Vec<Identifier>,
    pub span: Span,
}

// ============================================================================
// 九、模式（Pattern）
// ============================================================================

#[derive(Debug, Clone)]
pub enum Pattern {
    Wildcard(Span),
    Literal(LiteralPattern),
    Binding(BindingPattern),
    Tuple(TuplePattern),
    List(ListPattern),
    Dict(DictPattern),
    Struct(StructPattern),
    EnumVariant(EnumVariantPattern),
    Or(OrPattern),
}

#[derive(Debug, Clone)]
pub struct LiteralPattern {
    pub literal: Box<Expr>, // 只允许字面量
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct BindingPattern {
    pub name: Identifier,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TuplePattern {
    pub elements: Vec<Pattern>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ListPattern {
    pub elements: Vec<Pattern>,
    pub rest: Option<Identifier>, // *rest
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct DictPattern {
    pub entries: Vec<(String, Pattern)>, // {"key": pattern}
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructPattern {
    pub type_name: Identifier,
    pub fields: Vec<(Identifier, Pattern)>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumVariantPattern {
    pub type_name: Identifier,
    pub variant: Identifier,
    pub payload: Option<Vec<Pattern>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct OrPattern {
    pub patterns: Vec<Pattern>,
    pub span: Span,
}

// ============================================================================
// 九b、raw 静态语法节点（0.1.4）
// ============================================================================

#[derive(Debug, Clone)]
pub struct TraitDef {
    pub annotations: Vec<Annotation>,
    pub name: Identifier,
    pub extends: Vec<TypeExpr>,
    pub methods: Vec<TraitMethod>,
    pub span: Span,
}
#[derive(Debug, Clone)]
pub struct TraitMethod {
    pub name: Identifier,
    pub params: Vec<FnParam>,
    pub return_type: Option<TypeExpr>,
    pub default_body: Option<Vec<Stmt>>,
    pub span: Span,
}
#[derive(Debug, Clone)]
pub struct ImplBlock {
    pub trait_name: Identifier,
    pub target_type: TypeExpr,
    pub methods: Vec<FnDef>,
    pub span: Span,
}
#[derive(Debug, Clone)]
pub struct DtorBlock {
    pub type_name: Identifier,
    pub body: Vec<Stmt>,
    pub span: Span,
}

// ============================================================================
// 十、类型表达式
// ============================================================================

#[derive(Debug, Clone)]
pub enum TypeExpr {
    /// 基本类型：int, float, str, bool, null
    Basic(BasicType, Span),
    /// 用户类型：Point, Animal
    Named(Identifier, Span),
    /// 泛型类型：list[int], dict[str, int], Pair[T, U]
    Generic {
        base: Box<TypeExpr>,
        args: Vec<TypeExpr>,
        span: Span,
    },
    /// 函数类型：fn(int, int): int
    Function {
        params: Vec<TypeExpr>,
        return_type: Box<TypeExpr>,
        span: Span,
    },
    /// 可空类型：int?, str?
    Optional(Box<TypeExpr>, Span),
    /// 元组类型：(int, str)
    Tuple(Vec<TypeExpr>, Span),
    /// 数组类型：[int; N]
    Array(Box<TypeExpr>, Option<Box<Expr>>, Span),
    /// 通道类型：channel(int)
    Channel(Box<TypeExpr>, Span),
    // ── 0.1.4 raw 静态类型 ──
    Borrow { inner: Box<TypeExpr>, lifetime: Option<String>, span: Span },
    MutBorrow { inner: Box<TypeExpr>, lifetime: Option<String>, span: Span },
    Pointer(Box<TypeExpr>, Span),
    UnsignedInt(u8, Span),
    Result(Box<TypeExpr>, Box<TypeExpr>, Span),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BasicType {
    Int,
    Float,
    Str,
    Bool,
    Null,
    Void,
    Any,
}
