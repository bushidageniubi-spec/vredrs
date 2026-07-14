//! vredrs 语义分析器

pub mod linearity;
pub mod resolver;
pub mod symbol;
pub mod type_checker;
pub mod borrow_checker;
pub mod ilei;
pub mod risk_map;

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
    /// Enum definitions collected during validation, used for exhaustiveness
    /// checking of `match` statements whose scrutinee is an enum value.
    /// Maps enum name → list of variant names.
    enums: HashMap<String, Vec<String>>,
    /// Variable → enum-name bindings inferred from assignments of the form
    /// `set, x, EnumName.Variant` or `set, x, EnumName.Variant(args)`.
    /// Used by `check_match_exhaustiveness` to determine the enum type of
    /// a scrutinee when the case patterns don't carry a qualified
    /// `EnumName.Variant` prefix (e.g. when patterns are bare bindings or
    /// literals). The map is local to the current function body — it is
    /// cleared at function entry so bindings from one function don't leak
    /// into another.
    var_enum_bindings: HashMap<String, String>,
    /// Interface/trait definitions collected during validation, used
    /// for compile-time constraint checking of generic functions.
    /// Maps interface name → list of required method names.
    interfaces: HashMap<String, Vec<String>>,
    /// Class → list of method names (for constraint checking).
    class_methods: HashMap<String, Vec<String>>,
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
            enums: HashMap::new(),
            var_enum_bindings: HashMap::new(),
            interfaces: HashMap::new(),
            class_methods: HashMap::new(),
        }
    }

    pub fn analyze(&mut self, program: &Program) -> Result<()> {
        self.analyze_with_mode(program, "")
    }

    /// Analyze with a file path for mode-specific syntax validation.
    /// `.veds` files reject ptr[T], @pipeline, @isr_group, etc.
    /// `.vraw` files reject spawn, async, try/catch, GC features.
    /// `.cpps` files allow all Cstar annotations.
    pub fn analyze_with_mode(&mut self, program: &Program, file_path: &str) -> Result<()> {
        // Determine file mode from extension.
        let mode = if file_path.ends_with(".veds") {
            "veds"
        } else if file_path.ends_with(".vraw") {
            "vraw"
        } else if file_path.ends_with(".cpps") {
            "cpps"
        } else {
            "veds" // default
        };
        // Mode firewall: validate that file-type-specific syntax is not
        // used in the wrong mode.
        self.check_mode_restrictions(program, mode)?;

        let resolver = resolver::Resolver::new();
        let symtab = resolver.resolve(program)?;
        type_checker::TypeChecker::new(symtab).check(program)?;
        linearity::LinearityChecker::new().check(program)?;

        // For .vraw and .cpps files, run the borrow checker (ownership,
        // aliasing XOR mutability, lifetime, linear type consumption).
        // This enforces Rust-like memory safety for static backends.
        if mode == "vraw" || mode == "cpps" {
            borrow_checker::check_program_strict(program)?;
        }

        // Collect enum definitions before validation so match-exhaustiveness
        // can look them up.
        for item in &program.declarations {
            if let TopLevel::EnumDef(ed) = item {
                let variants: Vec<String> = ed.variants.iter().map(|v| v.name.name.clone()).collect();
                self.enums.insert(ed.name.name.clone(), variants);
            }
            // Collect interface/trait definitions for compile-time
            // constraint checking of generic functions.
            if let TopLevel::InterfaceDef(id) = item {
                let methods: Vec<String> = id.methods.iter().map(|m| m.name.name.clone()).collect();
                self.interfaces.insert(id.name.name.clone(), methods);
            }
            if let TopLevel::TraitDef(td) = item {
                let methods: Vec<String> = td.methods.iter().map(|m| m.name.name.clone()).collect();
                self.interfaces.insert(td.name.name.clone(), methods);
            }
            // Collect class method names for constraint checking.
            if let TopLevel::ClassDef(cd) = item {
                let methods: Vec<String> = cd.methods.iter().map(|m| m.name.name.clone()).collect();
                self.class_methods.insert(cd.name.name.clone(), methods);
            }
        }
        self.validate_program(program)?;
        if let Some(err) = self.errors.pop() {
            return Err(err);
        }

        // **ILEI**: Run Intent-Level Error Injection analysis on .vraw
        // and .cpps files. This scans for @assert safe and @inject fault
        // annotations, performs symbolic execution, and collects fault
        // points. The fault points are stored for the codegen backend
        // to emit hardware traps, and the Risk Map is printed as a
        // diagnostic.
        if mode == "vraw" || mode == "cpps" {
            let faults = ilei::analyze_program(program);
            if !faults.is_empty() {
                // Generate and print the Risk Map to stderr.
                let risk_map = risk_map::generate_risk_map(&faults, file_path);
                eprintln!("{}", risk_map);
            }
        }

        Ok(())
    }

    /// Check mode-specific syntax restrictions.
    /// - `.veds` mode rejects: ptr[T], @pipeline, @isr_group, @patch, @prefetch, @repo, @section, volatile, align
    /// - `.vraw` mode rejects: spawn, async, try/catch (dynamic features)
    /// - `.cpps` mode: all Cstar features allowed
    fn check_mode_restrictions(&self, program: &Program, mode: &str) -> Result<()> {
        match mode {
            "veds" => {
                // .veds files must not contain raw-mode or Cstar-mode syntax.
                for d in &program.declarations {
                    self.check_veds_restriction(d)?;
                }
            }
            "vraw" => {
                // .vraw files must not contain dynamic-mode syntax.
                for d in &program.declarations {
                    self.check_vraw_restriction(d)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn check_veds_restriction(&self, d: &TopLevel) -> Result<()> {
        match d {
            TopLevel::FnDef(f) => {
                for ann in &f.annotations {
                    let name = ann.name.as_str();
                    if matches!(name, "pipeline" | "isr_group" | "patch" | "prefetch" | "repo" | "section" | "embed" | "link") {
                        return Err(CompilerError::semantic_error(
                            format!(".veds mode does not support @{} (use .vraw or .cpps)", name),
                            f.span.clone(),
                        ).with_code(crate::error::ErrorCode::V2009));
                    }
                }
                // Check for ptr[T] in params/return type.
                for p in &f.params {
                    if let Some(ref ty) = p.type_annotation {
                        self.check_no_raw_types(ty)?;
                    }
                }
                if let Some(ref ty) = f.return_type {
                    self.check_no_raw_types(ty)?;
                }
            }
            TopLevel::Statement(s) => {
                self.check_veds_stmt_restriction(s)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn check_no_raw_types(&self, ty: &TypeExpr) -> Result<()> {
        match ty {
            TypeExpr::Pointer(_, _) => {
                return Err(CompilerError::semantic_error(
                    ".veds mode does not support ptr[T] (use .vraw)".to_string(),
                    crate::error::Span::dummy(),
                ).with_code(crate::error::ErrorCode::V2009));
            }
            TypeExpr::Borrow { inner, .. } | TypeExpr::MutBorrow { inner, .. } => {
                self.check_no_raw_types(inner)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn check_veds_stmt_restriction(&self, s: &Stmt) -> Result<()> {
        match s {
            Stmt::UnsafeBlock(_) => {
                return Err(CompilerError::semantic_error(
                    ".veds mode does not support unsafe blocks (use .vraw)".to_string(),
                    crate::error::Span::dummy(),
                ).with_code(crate::error::ErrorCode::V2009));
            }
            Stmt::Asm(_) => {
                return Err(CompilerError::semantic_error(
                    ".veds mode does not support asm blocks (use .vraw)".to_string(),
                    crate::error::Span::dummy(),
                ).with_code(crate::error::ErrorCode::V2009));
            }
            _ => {}
        }
        Ok(())
    }

    fn check_vraw_restriction(&self, d: &TopLevel) -> Result<()> {
        match d {
            TopLevel::FnDef(f) => {
                if f.is_async {
                    return Err(CompilerError::semantic_error(
                        ".vraw mode does not support async (dynamic feature)".to_string(),
                        f.span.clone(),
                    ).with_code(crate::error::ErrorCode::V2009));
                }
                for s in &f.body {
                    self.check_vraw_stmt_restriction(s)?;
                }
            }
            TopLevel::Statement(s) => {
                self.check_vraw_stmt_restriction(s)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn check_vraw_stmt_restriction(&self, s: &Stmt) -> Result<()> {
        match s {
            Stmt::Spawn(_) | Stmt::SpawnThread(_) => {
                return Err(CompilerError::semantic_error(
                    ".vraw mode does not support spawn (dynamic feature)".to_string(),
                    crate::error::Span::dummy(),
                ).with_code(crate::error::ErrorCode::V2009));
            }
            // try/catch is now supported in .vraw mode via the handler
            // stack mechanism in the raw backend.
            Stmt::Select(_) => {
                return Err(CompilerError::semantic_error(
                    ".vraw mode does not support select (dynamic feature)".to_string(),
                    crate::error::Span::dummy(),
                ).with_code(crate::error::ErrorCode::V2009));
            }
            _ => {}
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
        // Reset variable→enum bindings at function entry so bindings
        // from a previous function don't leak into this one. The map
        // is repopulated as we walk the function body's assignments.
        self.var_enum_bindings.clear();

        // **Compile-time trait constraint checking**: If the function
        // has generic type constraints (e.g. `fn, draw[T: Drawable](obj: T)`),
        // verify that any call to a method on `obj` that is required by
        // the constraint interface is actually defined. This is a
        // compile-time check — it doesn't verify the actual argument
        // type (that's done at runtime in VM mode), but it catches
        // obvious errors like calling a method that isn't in the
        // interface.
        if !fd.type_constraints.is_empty() {
            self.check_fn_constraints(fd);
        }

        for stmt in &fd.body {
            self.validate_stmt(stmt)?;
        }
        self.in_function = old_fn;
        self.in_loop = old_loop;
        Ok(())
    }

    /// Check that generic type constraints are satisfied at compile time.
    /// For each constraint `[T: Drawable]`, verify that:
    /// 1. The interface `Drawable` exists in the program.
    /// 2. Any method calls on parameters of type `T` use method names
    ///    that exist in the `Drawable` interface.
    /// This is a best-effort check — it catches typos and missing
    /// interface definitions, but doesn't verify that the actual
    /// argument type implements the interface (that's a runtime check
    /// in VM mode, and a monomorphization-time check in LLVM/Raw mode).
    fn check_fn_constraints(&mut self, fd: &FnDef) {
        for (type_param, constraints) in &fd.type_constraints {
            for constraint_name in constraints {
                // Check if the interface/trait exists.
                if !self.interfaces.contains_key(constraint_name) {
                    // Unknown constraint — skip (might be a built-in
                    // constraint or from another module).
                    continue;
                }
                let required_methods = self.interfaces.get(constraint_name).cloned().unwrap_or_default();

                // Find parameters that have this type parameter as their
                // type annotation.
                for p in &fd.params {
                    let is_type_param = match &p.type_annotation {
                        Some(TypeExpr::Named(id, _)) => id.name == *type_param,
                        _ => false,
                    };
                    if !is_type_param {
                        continue;
                    }

                    // Scan the function body for method calls on this
                    // parameter and verify they exist in the interface.
                    for stmt in &fd.body {
                        self.check_method_calls_against_interface(
                            stmt,
                            &p.name.name,
                            &required_methods,
                            constraint_name,
                        );
                    }
                }
            }
        }
    }

    /// Recursively check method calls on a variable against an interface's
    /// required methods. Reports an error if a called method is not in
    /// the interface.
    fn check_method_calls_against_interface(
        &mut self,
        stmt: &Stmt,
        var_name: &str,
        required_methods: &[String],
        interface_name: &str,
    ) {
        match stmt {
            Stmt::Expr(e) => {
                self.check_expr_method_calls(&e.expr, var_name, required_methods, interface_name);
            }
            Stmt::Assign(a) => {
                self.check_expr_method_calls(&a.value, var_name, required_methods, interface_name);
            }
            Stmt::If(i) => {
                self.check_expr_method_calls(&i.condition, var_name, required_methods, interface_name);
                for s in &i.then_body {
                    self.check_method_calls_against_interface(s, var_name, required_methods, interface_name);
                }
                if let Some(eb) = &i.else_body {
                    for s in eb {
                        self.check_method_calls_against_interface(s, var_name, required_methods, interface_name);
                    }
                }
            }
            Stmt::While(w) => {
                self.check_expr_method_calls(&w.condition, var_name, required_methods, interface_name);
                for s in &w.body {
                    self.check_method_calls_against_interface(s, var_name, required_methods, interface_name);
                }
            }
            Stmt::Return(r) => {
                for v in &r.values {
                    self.check_expr_method_calls(v, var_name, required_methods, interface_name);
                }
            }
            _ => {}
        }
    }

    /// Check if an expression contains a method call on `var_name` that
    /// is not in `required_methods`.
    fn check_expr_method_calls(
        &mut self,
        expr: &Expr,
        var_name: &str,
        required_methods: &[String],
        interface_name: &str,
    ) {
        match expr {
            Expr::MethodCall(mc) => {
                if let Expr::Identifier(id) = mc.receiver.as_ref() {
                    if id.name == var_name {
                        // This is a method call on the constrained
                        // parameter. Check if the method is in the
                        // interface.
                        if !required_methods.contains(&mc.method.name) {
                            self.errors.push(
                                CompilerError::semantic_error(
                                    format!(
                                        "type constraint violation: method '{}' is not in interface '{}' (required methods: [{}])",
                                        mc.method.name, interface_name,
                                        required_methods.join(", ")
                                    ),
                                    mc.span.clone(),
                                ),
                            );
                        }
                    }
                }
                // Recurse into args.
                for a in &mc.args {
                    self.check_expr_method_calls(a, var_name, required_methods, interface_name);
                }
            }
            Expr::Binary(b) => {
                self.check_expr_method_calls(&b.left, var_name, required_methods, interface_name);
                self.check_expr_method_calls(&b.right, var_name, required_methods, interface_name);
            }
            Expr::Call(c) => {
                for a in &c.args {
                    self.check_expr_method_calls(a, var_name, required_methods, interface_name);
                }
            }
            _ => {}
        }
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
                // Track variable → enum-name bindings for match
                // exhaustiveness. We look at the assigned value: if it
                // is `EnumName.Variant` (a MemberAccess whose target is
                // an Identifier matching a known enum), or a call
                // `EnumName.Variant(args)` (a Call whose callee is such
                // a MemberAccess, for tuple-variant constructors), we
                // record (var_name → enum_name) so a later
                // `match, var_name, ...` can be checked for
                // exhaustiveness even when its case patterns don't
                // carry the qualified `EnumName.Variant` prefix.
                if a.operator == AssignOp::Simple {
                    if let Some(Assignee::Identifier(id)) = a.targets.first() {
                        if let Some(enum_name) = self.expr_enum_name(&a.value) {
                            self.var_enum_bindings
                                .insert(id.name.clone(), enum_name);
                        }
                    }
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
                // Exhaustiveness check: if the scrutinee is an enum value
                // (qualified identifier `EnumName.Variant` or a value known
                // to be of an enum type), warn about missing variants. We
                // only warn (not error) to stay permissive — but if there's
                // no else case and variants are missing, we report it.
                self.check_match_exhaustiveness(m);
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

    /// Check whether a `match` statement covers all variants of an enum.
    /// If the scrutinee expression is an identifier whose name matches a
    /// known enum (e.g. `match, c, ...` where `c` was set from `Color.Red`),
    /// or a qualified enum access, collect the variant names mentioned in
    /// `case` patterns and warn about any missing ones. The check is
    /// skipped when an `else` case is present (it acts as a catch-all).
    ///
    /// This emits a warning to stderr rather than failing compilation, so
    /// existing programs that intentionally handle only a subset still run.
    /// The check runs in all modes (veds, vraw, cpps) because it is
    /// invoked from `validate_stmt`, which is called by `analyze_with_mode`
    /// regardless of file mode.
    fn check_match_exhaustiveness(&self, m: &MatchStmt) {
        // If there's an else case, the match is exhaustive by definition.
        if m.else_case.is_some() {
            return;
        }
        // Determine the enum name. Try several sources in order:
        //   1. The scrutinee itself is a qualified `EnumName.Variant`
        //      expression (immediate enum value).
        //   2. The scrutinee is an Identifier with a recorded
        //      variable→enum binding (from a prior
        //      `set, x, EnumName.Variant` assignment).
        //   3. Any case uses a qualified `EnumName.Variant` pattern
        //      (existing heuristic).
        let mut enum_name: Option<String> = None;
        if let Some(n) = self.expr_enum_name(&m.expr) {
            enum_name = Some(n);
        }
        if enum_name.is_none() {
            if let Expr::Identifier(id) = &m.expr {
                if let Some(n) = self.var_enum_bindings.get(&id.name) {
                    enum_name = Some(n.clone());
                }
            }
        }
        if enum_name.is_none() {
            for c in &m.cases {
                if let Pattern::EnumVariant(ep) = &c.pattern {
                    if !ep.type_name.name.is_empty() {
                        enum_name = Some(ep.type_name.name.clone());
                        break;
                    }
                }
                // Or-patterns may contain EnumVariant sub-patterns.
                if let Pattern::Or(op) = &c.pattern {
                    for p in &op.patterns {
                        if let Pattern::EnumVariant(ep) = p {
                            if !ep.type_name.name.is_empty() {
                                enum_name = Some(ep.type_name.name.clone());
                                break;
                            }
                        }
                    }
                    if enum_name.is_some() {
                        break;
                    }
                }
            }
        }
        let enum_name = match enum_name {
            Some(n) => n,
            None => return,
        };
        let all_variants = match self.enums.get(&enum_name) {
            Some(v) => v,
            None => return,
        };
        // Collect covered variant names.
        let mut covered: std::collections::HashSet<&String> = std::collections::HashSet::new();
        for c in &m.cases {
            let mut to_check: Vec<&Pattern> = vec![&c.pattern];
            while let Some(p) = to_check.pop() {
                match p {
                    Pattern::EnumVariant(ep) => {
                        covered.insert(&ep.variant.name);
                    }
                    Pattern::Or(op) => {
                        for sub in &op.patterns {
                            to_check.push(sub);
                        }
                    }
                    Pattern::Wildcard(_) | Pattern::Binding(_) => {
                        // Catch-all — exhaustive by definition.
                        return;
                    }
                    _ => {}
                }
            }
        }
        // Find missing variants.
        let missing: Vec<&String> = all_variants.iter().filter(|v| !covered.contains(*v)).collect();
        if !missing.is_empty() {
            eprintln!(
                "[vredrs] warning: match on enum '{}' does not cover all variants; missing: {}",
                enum_name,
                missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            );
        }
    }

    /// If `e` is `EnumName.Variant` or `EnumName.Variant(args)`, return
    /// the enum name (as a cloned `String` to avoid lifetime
    /// entanglement between `&self` and `&e`). Returns `None` for any
    /// other shape. Used by `check_match_exhaustiveness` to detect the
    /// enum type of a scrutinee or assigned value. The lookup is
    /// against the enums collected in `self.enums` so non-enum
    /// MemberAccess (e.g. `struct.field` or `module.func`) is correctly
    /// rejected.
    fn expr_enum_name(&self, e: &Expr) -> Option<String> {
        let type_name: &String = match e {
            // EnumName.Variant (unit variant)
            Expr::MemberAccess(m) => {
                if let Expr::Identifier(id) = m.target.as_ref() {
                    &id.name
                } else {
                    return None;
                }
            }
            // EnumName.Variant(args) (tuple or struct variant constructor)
            Expr::Call(c) => {
                if let Expr::MemberAccess(m) = c.callee.as_ref() {
                    if let Expr::Identifier(id) = m.target.as_ref() {
                        &id.name
                    } else {
                        return None;
                    }
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        if self.enums.contains_key(type_name) {
            Some(type_name.clone())
        } else {
            None
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
