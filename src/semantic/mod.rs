//! vredrs 语义分析器

pub mod linearity;
pub mod resolver;
pub mod symbol;
pub mod type_checker;

use crate::error::{CompilerError, Result, Span};
use crate::parser::ast::*;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub type_info: Option<TypeInfo>,
    pub span: Span,
    pub is_used: bool,
    pub is_mutable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolKind {
    Variable,
    Function,
    Struct,
    Class,
    Enum,
    Interface,
    Module,
    Const,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeInfo {
    Int,
    Float,
    Str,
    Bool,
    Null,
    List(Box<TypeInfo>),
    Dict(Box<TypeInfo>, Box<TypeInfo>),
    Tuple(Vec<TypeInfo>),
    Function(Vec<TypeInfo>, Box<TypeInfo>),
    Named(String),
    Any,
    Unknown,
}

pub struct Scope {
    symbols: HashMap<String, Symbol>,
    parent: Option<Box<Scope>>,
    level: usize,
}

impl Scope {
    pub fn new(level: usize) -> Self {
        Scope {
            symbols: HashMap::new(),
            parent: None,
            level,
        }
    }

    pub fn define(&mut self, name: String, symbol: Symbol) -> Result<()> {
        if self.symbols.contains_key(&name) {
            return Err(CompilerError::semantic_error(
                format!("Duplicate definition of '{}'", name),
                symbol.span.clone(),
            ));
        }
        self.symbols.insert(name, symbol);
        Ok(())
    }

    pub fn lookup(&self, name: &str) -> Option<&Symbol> {
        if let Some(sym) = self.symbols.get(name) {
            return Some(sym);
        }
        if let Some(parent) = &self.parent {
            return parent.lookup(name);
        }
        None
    }

    pub fn lookup_mut(&mut self, name: &str) -> Option<&mut Symbol> {
        if self.symbols.contains_key(name) {
            return self.symbols.get_mut(name);
        }
        if let Some(parent) = &mut self.parent {
            return parent.lookup_mut(name);
        }
        None
    }
}

pub struct SemanticAnalyzer {
    current_scope: Scope,
    scope_stack: Vec<Scope>,
    errors: Vec<CompilerError>,
    pon_stmts: Vec<PonStmt>,
    scope_blocks: Vec<ScopeBlock>,
    in_function: bool,
    in_loop: usize,
}

impl SemanticAnalyzer {
    pub fn new() -> Self {
        SemanticAnalyzer {
            current_scope: Scope::new(0),
            scope_stack: vec![],
            errors: vec![],
            pon_stmts: vec![],
            scope_blocks: vec![],
            in_function: false,
            in_loop: 0,
        }
    }

    pub fn analyze(&mut self, program: &Program) -> Result<()> {
        let resolver = resolver::Resolver::new();
        let symtab = resolver.resolve(program)?;
        type_checker::TypeChecker::new(symtab).check(program)?;
        linearity::LinearityChecker::new().check(program)?;
        self.validate_program(program)?;
        if let Some(err) = self.errors.pop() {
            return Err(err);
        }
        Ok(())
    }

    fn validate_program(&mut self, program: &Program) -> Result<()> {
        for item in &program.declarations {
            self.validate_top_level(item)?;
        }
        Ok(())
    }

    fn validate_top_level(&mut self, item: &TopLevel) -> Result<()> {
        match item {
            TopLevel::FnDef(fd) => self.validate_fn(fd),
            TopLevel::LazyFnDef(lfd) => self.validate_fn(&lfd.fn_def),
            TopLevel::ClassDef(cd) => {
                if let Some(parent) = &cd.extends {
                    if let TypeExpr::Named(id, _) = parent {
                        if id.name == cd.name.name {
                            return Err(CompilerError::semantic_error(
                                format!("class '{}' cannot extend itself", cd.name.name),
                                cd.span.clone(),
                            ));
                        }
                    }
                }
                for m in &cd.methods {
                    self.validate_fn(m)?;
                }
                Ok(())
            }
            TopLevel::StructDef(sd) => {
                for m in &sd.methods {
                    self.validate_fn(m)?;
                }
                Ok(())
            }
            TopLevel::Statement(stmt) => self.validate_stmt(stmt),
            TopLevel::ConditionalCompile(cc) => {
                for x in &cc.then_body {
                    self.validate_top_level(x)?;
                }
                if let Some(xs) = &cc.else_body {
                    for x in xs {
                        self.validate_top_level(x)?;
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn validate_fn(&mut self, fd: &FnDef) -> Result<()> {
        let old_fn = self.in_function;
        let old_loop = self.in_loop;
        self.in_function = true;
        self.in_loop = 0;
        for stmt in &fd.body {
            self.validate_stmt(stmt)?;
        }
        self.in_function = old_fn;
        self.in_loop = old_loop;
        Ok(())
    }

    fn validate_stmt(&mut self, stmt: &Stmt) -> Result<()> {
        match stmt {
            Stmt::Return(r) => {
                if !self.in_function {
                    return Err(CompilerError::semantic_error(
                        "return outside function",
                        r.span.clone(),
                    ));
                }
                for e in &r.values {
                    self.validate_expr(e)?;
                }
            }
            Stmt::Break(b) => {
                if self.in_loop == 0 {
                    return Err(CompilerError::semantic_error(
                        "break outside loop",
                        b.span.clone(),
                    ));
                }
            }
            Stmt::Continue(c) => {
                if self.in_loop == 0 {
                    return Err(CompilerError::semantic_error(
                        "continue outside loop",
                        c.span.clone(),
                    ));
                }
            }
            Stmt::Yield(y) => {
                if !self.in_function {
                    return Err(CompilerError::semantic_error(
                        "yield outside function/coroutine",
                        y.span.clone(),
                    ));
                }
                if let Some(v) = &y.value {
                    self.validate_expr(v)?;
                }
            }
            Stmt::Assign(a) => {
                if a.operator != AssignOp::Delete {
                    self.validate_expr(&a.value)?;
                }
                for t in &a.targets {
                    self.validate_assignee(t)?;
                }
            }
            Stmt::Pon(p) => {
                self.pon_stmts.push(p.clone());
                self.validate_stmt(&Stmt::Assign(p.assign.clone()))?;
            }
            Stmt::Paste(p) => {
                for e in &p.args {
                    self.validate_expr(e)?;
                }
            }
            Stmt::Println(p) => {
                for e in &p.args {
                    self.validate_expr(e)?;
                }
            }
            Stmt::Input(_) | Stmt::Flush(_) => {}
            Stmt::Throw(t) => {
                self.validate_expr(&t.value)?;
            }
            Stmt::Defer(d) => {
                self.validate_stmt(&d.stmt)?;
            }
            Stmt::Assert(a) => {
                self.validate_expr(&a.condition)?;
            }
            Stmt::Panic(p) => {
                self.validate_expr(&p.message)?;
            }
            Stmt::Spawn(s) => {
                self.validate_expr(&s.call)?;
            }
            Stmt::SpawnThread(s) => {
                self.validate_expr(&s.call)?;
            }
            Stmt::If(i) => {
                self.validate_expr(&i.condition)?;
                for s in &i.then_body {
                    self.validate_stmt(s)?;
                }
                for (c, b) in &i.elif_chain {
                    self.validate_expr(c)?;
                    for s in b {
                        self.validate_stmt(s)?;
                    }
                }
                if let Some(b) = &i.else_body {
                    for s in b {
                        self.validate_stmt(s)?;
                    }
                }
            }
            Stmt::While(w) => {
                self.validate_expr(&w.condition)?;
                self.in_loop += 1;
                for s in &w.body {
                    self.validate_stmt(s)?;
                }
                if let Some(b) = &w.else_body {
                    for s in b {
                        self.validate_stmt(s)?;
                    }
                }
                self.in_loop -= 1;
            }
            Stmt::ForIn(f) => {
                self.validate_expr(&f.iterable)?;
                self.in_loop += 1;
                for s in &f.body {
                    self.validate_stmt(s)?;
                }
                if let Some(b) = &f.else_body {
                    for s in b {
                        self.validate_stmt(s)?;
                    }
                }
                self.in_loop -= 1;
            }
            Stmt::ForRange(f) => {
                self.validate_expr(&f.from)?;
                self.validate_expr(&f.to)?;
                self.in_loop += 1;
                for s in &f.body {
                    self.validate_stmt(s)?;
                }
                if let Some(b) = &f.else_body {
                    for s in b {
                        self.validate_stmt(s)?;
                    }
                }
                self.in_loop -= 1;
            }
            Stmt::Loop(l) => {
                self.in_loop += 1;
                for s in &l.body {
                    self.validate_stmt(s)?;
                }
                if let Some(b) = &l.else_body {
                    for s in b {
                        self.validate_stmt(s)?;
                    }
                }
                self.in_loop -= 1;
            }
            Stmt::Match(m) => {
                self.validate_expr(&m.expr)?;
                for c in &m.cases {
                    if let Some(g) = &c.guard {
                        self.validate_expr(g)?;
                    }
                    for s in &c.body {
                        self.validate_stmt(s)?;
                    }
                }
                if let Some(b) = &m.else_case {
                    for s in b {
                        self.validate_stmt(s)?;
                    }
                }
            }
            Stmt::Try(t) => {
                for s in &t.try_body {
                    self.validate_stmt(s)?;
                }
                if let Some(b) = &t.catch_body {
                    for s in b {
                        self.validate_stmt(s)?;
                    }
                }
                if let Some(b) = &t.finally_body {
                    for s in b {
                        self.validate_stmt(s)?;
                    }
                }
            }
            Stmt::Select(s) => {
                for c in &s.cases {
                    for s in &c.body {
                        self.validate_stmt(s)?;
                    }
                }
                if let Some(b) = &s.default_case {
                    for s in b {
                        self.validate_stmt(s)?;
                    }
                }
            }
            Stmt::With(w) => {
                self.validate_expr(&w.manager)?;
                for s in &w.body {
                    self.validate_stmt(s)?;
                }
            }
            Stmt::UnsafeBlock(b) => {
                for s in &b.body {
                    self.validate_stmt(s)?;
                }
            }
            Stmt::DirectiveBlock(b) => {
                for s in &b.body {
                    self.validate_stmt(s)?;
                }
            }
            Stmt::ScopeBlock(b) => {
                self.scope_blocks.push(b.clone());
                for s in &b.body {
                    self.validate_stmt(s)?;
                }
            }
            Stmt::Expr(e) => {
                self.validate_expr(&e.expr)?;
            }
            Stmt::TableAssign(t) => {
                for row in &t.rows {
                    for e in row {
                        self.validate_expr(e)?;
                    }
                }
            }
            Stmt::Asm(_) => {}
        }
        Ok(())
    }

    fn validate_assignee(&mut self, target: &Assignee) -> Result<()> {
        match target {
            Assignee::Identifier(_) | Assignee::Qualified(_) => Ok(()),
            Assignee::Member(m) => self.validate_expr(&m.target),
            Assignee::Index(i) => {
                self.validate_expr(&i.target)?;
                self.validate_expr(&i.index)
            }
            Assignee::Tuple(items) => {
                for item in items {
                    self.validate_assignee(item)?;
                }
                Ok(())
            }
        }
    }

    fn validate_expr(&mut self, expr: &Expr) -> Result<()> {
        match expr {
            Expr::Binary(b) => {
                self.validate_expr(&b.left)?;
                self.validate_expr(&b.right)?;
            }
            Expr::Unary(u) => self.validate_expr(&u.operand)?,
            Expr::Postfix(p) => self.validate_expr(&p.operand)?,
            Expr::Call(c) => {
                self.validate_expr(&c.callee)?;
                for a in &c.args {
                    self.validate_expr(a)?;
                }
            }
            Expr::MethodCall(m) => {
                self.validate_expr(&m.receiver)?;
                for a in &m.args {
                    self.validate_expr(a)?;
                }
            }
            Expr::Index(i) => {
                self.validate_expr(&i.target)?;
                self.validate_expr(&i.index)?;
            }
            Expr::Slice(sl) => {
                self.validate_expr(&sl.target)?;
                if let Some(e) = &sl.start {
                    self.validate_expr(e)?;
                }
                if let Some(e) = &sl.end {
                    self.validate_expr(e)?;
                }
                if let Some(e) = &sl.step {
                    self.validate_expr(e)?;
                }
            }
            Expr::MemberAccess(m) => self.validate_expr(&m.target)?,
            Expr::OptionalChain(o) => {
                self.validate_expr(&o.target)?;
                for l in &o.chain {
                    match l {
                        OptionalChainLink::Call { args, .. } => {
                            for a in args {
                                self.validate_expr(a)?;
                            }
                        }
                        OptionalChainLink::Index(e) => self.validate_expr(e)?,
                        _ => {}
                    }
                }
            }
            Expr::Spread(s) => self.validate_expr(&s.expr)?,
            Expr::Ternary(t) => {
                self.validate_expr(&t.condition)?;
                self.validate_expr(&t.true_branch)?;
                self.validate_expr(&t.false_branch)?;
            }
            Expr::Lambda(l) => self.validate_expr(&l.body)?,
            Expr::Spawn(s) => self.validate_expr(&s.call)?,
            Expr::Coro(c) => {
                self.validate_expr(&c.function)?;
                for a in &c.args {
                    self.validate_expr(a)?;
                }
            }
            Expr::Resume(r) => {
                self.validate_expr(&r.handle)?;
                for v in &r.values {
                    self.validate_expr(v)?;
                }
            }
            Expr::Await(a) => self.validate_expr(&a.expr)?,
            Expr::Cast(c) => self.validate_expr(&c.expr)?,
            Expr::TryPropagate(t) => {
                if !self.in_function {
                    return Err(CompilerError::semantic_error(
                        "? error propagation outside function",
                        t.span.clone(),
                    ));
                }
                self.validate_expr(&t.expr)?;
            }
            Expr::Range(r) => {
                if let Some(s) = &r.start {
                    self.validate_expr(s)?;
                }
                if let Some(e) = &r.end {
                    self.validate_expr(e)?;
                }
                if let Some(s) = &r.step {
                    self.validate_expr(s)?;
                }
            }
            Expr::List(l) => {
                for e in &l.elements {
                    self.validate_expr(e)?;
                }
            }
            Expr::ListComprehension(l) => {
                self.validate_expr(&l.result_expr)?;
                self.validate_expr(&l.iterable)?;
                if let Some(c) = &l.condition {
                    self.validate_expr(c)?;
                }
            }
            Expr::Dict(d) => {
                for (k, v) in &d.entries {
                    self.validate_expr(k)?;
                    self.validate_expr(v)?;
                }
            }
            Expr::DictComprehension(d) => {
                self.validate_expr(&d.key_expr)?;
                self.validate_expr(&d.value_expr)?;
                self.validate_expr(&d.iterable)?;
                if let Some(c) = &d.condition {
                    self.validate_expr(c)?;
                }
            }
            Expr::Set(s) => {
                for e in &s.elements {
                    self.validate_expr(e)?;
                }
            }
            Expr::SetComprehension(s) => {
                self.validate_expr(&s.result_expr)?;
                self.validate_expr(&s.iterable)?;
                if let Some(c) = &s.condition {
                    self.validate_expr(c)?;
                }
            }
            Expr::Tuple(t) => {
                for e in &t.elements {
                    self.validate_expr(e)?;
                }
            }
            Expr::Pipe(p) => {
                self.validate_expr(&p.left)?;
                self.validate_expr(&p.right)?;
            }
            Expr::NullCoalesce(n) => {
                self.validate_expr(&n.left)?;
                self.validate_expr(&n.right)?;
            }
            Expr::String_(s) => {
                for p in &s.parts {
                    if let StringPart::Interpolation(e) = p {
                        self.validate_expr(e)?;
                    }
                }
            }
            Expr::MultiLineString(s) => {
                for p in &s.parts {
                    if let StringPart::Interpolation(e) = p {
                        self.validate_expr(e)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}
