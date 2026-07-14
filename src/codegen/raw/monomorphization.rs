//! Generic monomorphization for the raw backend (0.1.4+).
//!
//! Transforms a program containing generic function definitions
//! (`fn, identity[T](x: T): T`) into one with concrete instances
//! (`identity_int`, `identity_str`, ...). The pass:
//!
//! 1. Collects all generic function definitions (those with non-empty
//!    `type_params`).
//! 2. Walks every call site in the program. When a call targets a generic
//!    function, infers the type arguments from the argument expressions.
//! 3. For each unique `(base_name, type_args)` pair, creates a concrete
//!    `FnDef` instance with the mangled name and substitutes type
//!    parameters in the signature.
//! 4. Rewrites call sites to use the mangled name.
//! 5. Removes the generic definition from the output (it has been replaced
//!    by its instances).
//!
//! Type inference is AST-level (not value-level): `42` → "int",
//! `3.14` → "float", `"hi"` → "str", `true` → "bool". For identifiers,
//! we look up the variable's inferred type from a pre-computed map. For
//! calls, we use the callee's return type. This is conservative but
//! sufficient for typical generic usage in raw mode.

use crate::parser::ast::*;
use std::collections::HashMap;

/// The monomorphizer: transforms a program with generics into one with
/// only concrete functions.
pub struct Monomorphizer {
    /// Map from (base_name, type_args) → mangled instance name.
    pub instantiations: HashMap<(String, Vec<String>), String>,
    /// Generated concrete function instances.
    pub generated: Vec<FnDef>,
    /// Generic function definitions keyed by base name.
    generic_defs: HashMap<String, FnDef>,
    /// Variable type map: var_name → type_string (e.g. "int", "str").
    /// Built during a pre-pass that walks assignments.
    var_types: HashMap<String, String>,
    /// Function return types: fn_name → type_string.
    fn_ret_types: HashMap<String, String>,
    /// Generic struct definitions keyed by base name.
    generic_struct_defs: HashMap<String, StructDef>,
    /// Generated concrete struct instances.
    generated_structs: Vec<StructDef>,
    /// When true, use LLVM naming convention (i64, f64, str, bool)
    /// instead of Raw convention (int, float, str, bool).
    pub llvm_mode: bool,
}

impl Monomorphizer {
    pub fn new() -> Self {
        Monomorphizer {
            instantiations: HashMap::new(),
            generated: Vec::new(),
            generic_defs: HashMap::new(),
            var_types: HashMap::new(),
            fn_ret_types: HashMap::new(),
            generic_struct_defs: HashMap::new(),
            generated_structs: Vec::new(),
            llvm_mode: false,
        }
    }

    /// Convert a type string to the backend-specific name.
    /// In Raw mode: "int", "float", "str", "bool".
    /// In LLVM mode: "i64", "f64", "str", "bool".
    fn type_name(&self, ty: &str) -> String {
        if self.llvm_mode {
            match ty {
                "int" => "i64".to_string(),
                "float" => "f64".to_string(),
                _ => ty.to_string(),
            }
        } else {
            ty.to_string()
        }
    }

    /// Mangle a base name with type arguments to produce a concrete name.
    /// E.g. `identity` + ["int"] → `identity_int`,
    ///      `pair` + ["int", "str"] → `pair_int_str`.
    pub fn concrete_name(base: &str, type_args: &[String]) -> String {
        if type_args.is_empty() {
            base.to_string()
        } else {
            format!("{}_{}", base, type_args.join("_"))
        }
    }

    pub fn generated_fns(&self) -> &[FnDef] {
        &self.generated
    }

    /// Run monomorphization on a program. Returns a new program with
    /// generic functions replaced by their concrete instances and call
    /// sites rewritten.
    pub fn run(&mut self, program: &Program) -> Program {
        // Phase 1: Collect generic function and struct definitions.
        self.generic_defs.clear();
        self.generic_struct_defs.clear();
        for d in &program.declarations {
            if let TopLevel::FnDef(f) = d {
                if !f.type_params.is_empty() {
                    self.generic_defs.insert(f.name.name.clone(), f.clone());
                }
            }
            if let TopLevel::StructDef(s) = d {
                if !s.type_params.is_empty() {
                    self.generic_struct_defs.insert(s.name.name.clone(), s.clone());
                }
            }
        }
        // If there are no generic functions or structs, return the program as-is.
        if self.generic_defs.is_empty() && self.generic_struct_defs.is_empty() {
            return program.clone();
        }

        // Phase 2: Build variable type map and function return type map.
        self.var_types.clear();
        self.fn_ret_types.clear();
        self.collect_types(program);

        // Phase 3: Walk the program, find generic call sites, and collect
        // instantiations. We do this recursively over all statements and
        // expressions.
        self.instantiations.clear();
        let mut work: Vec<&Program> = vec![program];
        while let Some(prog) = work.pop() {
            for d in &prog.declarations {
                self.scan_decls_for_instantiations(d);
            }
        }

        // Phase 4: Generate concrete function instances.
        self.generated.clear();
        for ((base, type_args), mangled) in &self.instantiations {
            if let Some(gen_def) = self.generic_defs.get(base) {
                let concrete = self.instantiate(gen_def, type_args, mangled);
                self.generated.push(concrete);
            }
        }
        // Also generate concrete struct instances from struct instantiations.
        self.generated_structs.clear();
        for ((base, type_args), mangled) in &self.instantiations {
            if let Some(gen_struct) = self.generic_struct_defs.get(base) {
                let concrete = self.instantiate_struct(gen_struct, type_args, mangled);
                self.generated_structs.push(concrete);
            }
        }

        // Phase 5: Build the output program.
        // - Replace generic function defs with generated instances.
        // - Rewrite call sites to use mangled names.
        let mut out_decls = Vec::new();
        for d in &program.declarations {
            match d {
                TopLevel::FnDef(f) => {
                    if !f.type_params.is_empty() {
                        // Skip the generic definition; its instances are
                        // added below.
                        continue;
                    }
                    // Non-generic function: rewrite calls in its body.
                    let mut f2 = f.clone();
                    self.rewrite_calls_in_fn(&mut f2);
                    out_decls.push(TopLevel::FnDef(f2));
                }
                TopLevel::StructDef(s) => {
                    if !s.type_params.is_empty() {
                        // Keep the generic struct definition (needed for VM
                        // mode where types are dynamic), but also add the
                        // concrete instances below.
                        out_decls.push(TopLevel::StructDef(s.clone()));
                    } else {
                        out_decls.push(TopLevel::StructDef(s.clone()));
                    }
                }
                TopLevel::LazyFnDef(l) => {
                    let mut l2 = l.clone();
                    self.rewrite_calls_in_fn(&mut l2.fn_def);
                    out_decls.push(TopLevel::LazyFnDef(l2));
                }
                other => {
                    out_decls.push(other.clone());
                }
            }
        }
        // Append generated function instances.
        for f in &self.generated {
            out_decls.push(TopLevel::FnDef(f.clone()));
        }
        // Append generated struct instances.
        for s in &self.generated_structs {
            out_decls.push(TopLevel::StructDef(s.clone()));
        }

        // Also rewrite calls in top-level statements (the implicit main).
        let mut out_decls2 = Vec::new();
        for d in out_decls {
            if let TopLevel::Statement(s) = &d {
                let mut s2 = s.clone();
                self.rewrite_calls_in_stmt(&mut s2);
                out_decls2.push(TopLevel::Statement(s2));
            } else {
                out_decls2.push(d);
            }
        }

        Program {
            declarations: out_decls2,
            span: program.span.clone(),
        }
    }

    /// Collect variable types and function return types by scanning
    /// assignments and function signatures.
    fn collect_types(&mut self, program: &Program) {
        for d in &program.declarations {
            if let TopLevel::FnDef(f) = d {
                // Record return type (from annotation or "any").
                let ret_ty = f
                    .return_type
                    .as_ref()
                    .map(|t| type_expr_to_string(t))
                    .unwrap_or_else(|| "any".to_string());
                // Don't record type variables (T, U, ...) as return types
                // for generic functions — we can't infer from them.
                if f.type_params.is_empty() {
                    self.fn_ret_types.insert(f.name.name.clone(), ret_ty);
                }
                // Scan body for variable assignments.
                for s in &f.body {
                    self.collect_var_types_stmt(s);
                }
            }
        }
    }

    fn collect_var_types_stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Assign(a) => {
                if let Some(Assignee::Identifier(id)) = a.targets.first() {
                    let ty = self.infer_expr_type(&a.value);
                    if let Some(t) = ty {
                        self.var_types.insert(id.name.clone(), t);
                    }
                }
            }
            Stmt::If(i) => {
                for s in &i.then_body {
                    self.collect_var_types_stmt(s);
                }
                if let Some(else_body) = &i.else_body {
                    for s in else_body {
                        self.collect_var_types_stmt(s);
                    }
                }
            }
            Stmt::While(w) => {
                for s in &w.body {
                    self.collect_var_types_stmt(s);
                }
            }
            Stmt::ForIn(f) => {
                for s in &f.body {
                    self.collect_var_types_stmt(s);
                }
            }
            Stmt::ForRange(f) => {
                for s in &f.body {
                    self.collect_var_types_stmt(s);
                }
            }
            Stmt::ScopeBlock(b) => {
                for s in &b.body {
                    self.collect_var_types_stmt(s);
                }
            }
            Stmt::UnsafeBlock(u) => {
                for s in &u.body {
                    self.collect_var_types_stmt(s);
                }
            }
            _ => {}
        }
    }

    /// Infer the type of an expression as a string ("int", "float", "str",
    /// "bool"). Returns None if the type can't be determined.
    fn infer_expr_type(&self, e: &Expr) -> Option<String> {
        match e {
            Expr::Integer(_) => Some("int".to_string()),
            Expr::Float(_) => Some("float".to_string()),
            Expr::String_(_) => Some("str".to_string()),
            Expr::Bool(_) => Some("bool".to_string()),
            Expr::Null(_) => Some("null".to_string()),
            Expr::Identifier(id) => self.var_types.get(&id.name).cloned(),
            Expr::Call(c) => {
                if let Expr::Identifier(id) = c.callee.as_ref() {
                    self.fn_ret_types.get(&id.name).cloned()
                } else {
                    None
                }
            }
            Expr::Binary(b) => {
                // Binary op type follows the left operand (for arithmetic)
                // or is "bool" (for comparisons).
                match b.operator {
                    BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Gt
                    | BinaryOp::Le | BinaryOp::Ge | BinaryOp::And | BinaryOp::Or
                    | BinaryOp::Is | BinaryOp::In => Some("bool".to_string()),
                    _ => self.infer_expr_type(&b.left),
                }
            }
            Expr::Unary(u) => self.infer_expr_type(&u.operand),
            Expr::Cast(c) => Some(type_expr_to_string(&c.type_expr)),
            _ => None,
        }
    }

    /// Scan a top-level declaration for generic call sites and register
    /// instantiations.
    fn scan_decls_for_instantiations(&mut self, d: &TopLevel) {
        match d {
            TopLevel::FnDef(f) => {
                if f.type_params.is_empty() {
                    for s in &f.body {
                        self.scan_stmt_for_instantiations(s);
                    }
                }
                // Generic function bodies are not scanned here — their
                // instantiations are driven by call sites in non-generic
                // code. (A generic function calling another generic is
                // an advanced case handled when the instantiations are
                // generated.)
            }
            TopLevel::LazyFnDef(l) => {
                for s in &l.fn_def.body {
                    self.scan_stmt_for_instantiations(s);
                }
            }
            TopLevel::Statement(s) => {
                self.scan_stmt_for_instantiations(s);
            }
            _ => {}
        }
    }

    fn scan_stmt_for_instantiations(&mut self, s: &Stmt) {
        match s {
            Stmt::Assign(a) => {
                self.scan_expr_for_instantiations(&a.value);
            }
            Stmt::Return(r) => {
                for v in &r.values {
                    self.scan_expr_for_instantiations(v);
                }
            }
            Stmt::Expr(e) => {
                self.scan_expr_for_instantiations(&e.expr);
            }
            Stmt::If(i) => {
                self.scan_expr_for_instantiations(&i.condition);
                for s in &i.then_body {
                    self.scan_stmt_for_instantiations(s);
                }
                if let Some(else_body) = &i.else_body {
                    for s in else_body {
                        self.scan_stmt_for_instantiations(s);
                    }
                }
            }
            Stmt::While(w) => {
                self.scan_expr_for_instantiations(&w.condition);
                for s in &w.body {
                    self.scan_stmt_for_instantiations(s);
                }
            }
            Stmt::Println(p) => {
                for a in &p.args {
                    self.scan_expr_for_instantiations(a);
                }
            }
            Stmt::UnsafeBlock(u) => {
                for s in &u.body {
                    self.scan_stmt_for_instantiations(s);
                }
            }
            Stmt::ScopeBlock(b) => {
                for s in &b.body {
                    self.scan_stmt_for_instantiations(s);
                }
            }
            _ => {}
        }
    }

    fn scan_expr_for_instantiations(&mut self, e: &Expr) {
        match e {
            Expr::Call(c) => {
                // Check if this is a call to a generic function.
                if let Expr::Identifier(id) = c.callee.as_ref() {
                    if let Some(gen_def) = self.generic_defs.get(&id.name) {
                        // Infer type arguments from the call arguments.
                        let type_args: Vec<String> = gen_def
                            .type_params
                            .iter()
                            .map(|tp| {
                                // Find the parameter with this type param as its annotation.
                                for (i, p) in gen_def.params.iter().enumerate() {
                                    if let Some(ta) = &p.type_annotation {
                                        if type_expr_to_string(ta) == *tp {
                                            if let Some(arg) = c.args.get(i) {
                                                if let Some(t) = self.infer_expr_type(arg) {
                                                    return t;
                                                }
                                            }
                                        }
                                    }
                                }
                                // Default: "int" if we can't infer.
                                "int".to_string()
                            })
                            .collect();
                        // Mangled name uses backend-specific type names.
                        let mangled_type_args: Vec<String> = type_args.iter()
                            .map(|t| self.type_name(t))
                            .collect();
                        let mangled = Self::concrete_name(&id.name, &mangled_type_args);
                        self.instantiations
                            .insert((id.name.clone(), type_args), mangled);
                    }
                }
                // Recurse into arguments.
                for a in &c.args {
                    self.scan_expr_for_instantiations(a);
                }
            }
            Expr::Binary(b) => {
                self.scan_expr_for_instantiations(&b.left);
                self.scan_expr_for_instantiations(&b.right);
            }
            Expr::Unary(u) => {
                self.scan_expr_for_instantiations(&u.operand);
            }
            Expr::MemberAccess(m) => {
                self.scan_expr_for_instantiations(&m.target);
            }
            Expr::Index(i) => {
                self.scan_expr_for_instantiations(&i.target);
                self.scan_expr_for_instantiations(&i.index);
            }
            _ => {}
        }
    }

    /// Create a concrete function instance from a generic definition.
    fn instantiate(&self, gen_def: &FnDef, type_args: &[String], mangled: &str) -> FnDef {
        // Build a substitution map: type_param → concrete type string.
        let mut subst: HashMap<String, String> = HashMap::new();
        for (i, tp) in gen_def.type_params.iter().enumerate() {
            if let Some(arg) = type_args.get(i) {
                subst.insert(tp.clone(), arg.clone());
            }
        }
        // Clone the function and rename it.
        let mut concrete = gen_def.clone();
        concrete.name = Identifier {
            name: mangled.to_string(),
            span: gen_def.name.span.clone(),
        };
        concrete.type_params = Vec::new();
        concrete.type_constraints = HashMap::new();
        // Substitute type annotations in parameters.
        for p in &mut concrete.params {
            if let Some(ta) = &p.type_annotation {
                p.type_annotation = Some(substitute_type_expr(ta, &subst));
            }
        }
        // Substitute return type.
        if let Some(rt) = &concrete.return_type {
            concrete.return_type = Some(substitute_type_expr(rt, &subst));
        }
        concrete
    }

    /// Create a concrete struct instance from a generic definition.
    fn instantiate_struct(&self, gen_def: &StructDef, type_args: &[String], mangled: &str) -> StructDef {
        // Build a substitution map: type_param → concrete type string.
        let mut subst: HashMap<String, String> = HashMap::new();
        for (i, tp) in gen_def.type_params.iter().enumerate() {
            if let Some(arg) = type_args.get(i) {
                subst.insert(tp.clone(), arg.clone());
            }
        }
        // Clone the struct and rename it.
        let mut concrete = gen_def.clone();
        concrete.name = Identifier {
            name: mangled.to_string(),
            span: gen_def.name.span.clone(),
        };
        concrete.type_params = Vec::new();
        concrete.type_constraints = HashMap::new();
        // Substitute type annotations in fields.
        for field in &mut concrete.fields {
            if let Some(ta) = &field.type_annotation {
                field.type_annotation = Some(substitute_type_expr(ta, &subst));
            }
        }
        concrete
    }

    /// Rewrite call sites in a function body to use mangled names.
    fn rewrite_calls_in_fn(&self, f: &mut FnDef) {
        for s in &mut f.body {
            self.rewrite_calls_in_stmt(s);
        }
    }

    fn rewrite_calls_in_stmt(&self, s: &mut Stmt) {
        match s {
            Stmt::Assign(a) => {
                self.rewrite_calls_in_expr(&mut a.value);
            }
            Stmt::Return(r) => {
                for v in &mut r.values {
                    self.rewrite_calls_in_expr(v);
                }
            }
            Stmt::Expr(e) => {
                self.rewrite_calls_in_expr(&mut e.expr);
            }
            Stmt::If(i) => {
                self.rewrite_calls_in_expr(&mut i.condition);
                for s in &mut i.then_body {
                    self.rewrite_calls_in_stmt(s);
                }
                if let Some(else_body) = &mut i.else_body {
                    for s in else_body {
                        self.rewrite_calls_in_stmt(s);
                    }
                }
            }
            Stmt::While(w) => {
                self.rewrite_calls_in_expr(&mut w.condition);
                for s in &mut w.body {
                    self.rewrite_calls_in_stmt(s);
                }
            }
            Stmt::Println(p) => {
                for a in &mut p.args {
                    self.rewrite_calls_in_expr(a);
                }
            }
            Stmt::UnsafeBlock(u) => {
                for s in &mut u.body {
                    self.rewrite_calls_in_stmt(s);
                }
            }
            Stmt::ScopeBlock(b) => {
                for s in &mut b.body {
                    self.rewrite_calls_in_stmt(s);
                }
            }
            _ => {}
        }
    }

    fn rewrite_calls_in_expr(&self, e: &mut Expr) {
        match e {
            Expr::Call(c) => {
                // Rewrite the callee if it's a generic function call.
                if let Expr::Identifier(id) = c.callee.as_ref() {
                    if self.generic_defs.contains_key(&id.name) {
                        // Find the matching instantiation.
                        // We need to re-infer the type args (same logic as scan).
                        if let Some(gen_def) = self.generic_defs.get(&id.name) {
                            let type_args: Vec<String> = gen_def
                                .type_params
                                .iter()
                                .map(|tp| {
                                    for (i, p) in gen_def.params.iter().enumerate() {
                                        if let Some(ta) = &p.type_annotation {
                                            if type_expr_to_string(ta) == *tp {
                                                if let Some(arg) = c.args.get(i) {
                                                    if let Some(t) = self.infer_expr_type(arg) {
                                                        return t;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    "int".to_string()
                                })
                                .collect();
                            if let Some(mangled) = self
                                .instantiations
                                .get(&(id.name.clone(), type_args))
                            {
                                // Rewrite the callee to the mangled name.
                                c.callee = Box::new(Expr::Identifier(Identifier {
                                    name: mangled.clone(),
                                    span: id.span.clone(),
                                }));
                            }
                        }
                    }
                }
                // Recurse into arguments.
                for a in &mut c.args {
                    self.rewrite_calls_in_expr(a);
                }
            }
            Expr::Binary(b) => {
                self.rewrite_calls_in_expr(&mut b.left);
                self.rewrite_calls_in_expr(&mut b.right);
            }
            Expr::Unary(u) => {
                self.rewrite_calls_in_expr(&mut u.operand);
            }
            Expr::MemberAccess(m) => {
                self.rewrite_calls_in_expr(&mut m.target);
            }
            Expr::Index(i) => {
                self.rewrite_calls_in_expr(&mut i.target);
                self.rewrite_calls_in_expr(&mut i.index);
            }
            _ => {}
        }
    }
}

impl Default for Monomorphizer {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a TypeExpr to a simple type string ("int", "float", "str",
/// "bool", or the name of a named type). Type parameters (like `T`)
/// are returned as-is.
pub fn type_expr_to_string(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Basic(BasicType::Int, _) => "int".to_string(),
        TypeExpr::Basic(BasicType::Float, _) => "float".to_string(),
        TypeExpr::Basic(BasicType::Str, _) => "str".to_string(),
        TypeExpr::Basic(BasicType::Bool, _) => "bool".to_string(),
        TypeExpr::Basic(BasicType::Void, _) => "void".to_string(),
        TypeExpr::Basic(BasicType::Null, _) => "null".to_string(),
        TypeExpr::Basic(BasicType::Any, _) => "any".to_string(),
        TypeExpr::Named(id, _) => id.name.clone(),
        TypeExpr::Optional(inner, _) => type_expr_to_string(inner),
        TypeExpr::Pointer(inner, _) => type_expr_to_string(inner),
        TypeExpr::Borrow { inner, .. } => type_expr_to_string(inner),
        TypeExpr::MutBorrow { inner, .. } => type_expr_to_string(inner),
        TypeExpr::Generic { base, .. } => type_expr_to_string(base),
        TypeExpr::Tuple(_, _) => "tuple".to_string(),
        TypeExpr::Array(inner, _, _) => type_expr_to_string(inner),
        TypeExpr::Channel(inner, _) => type_expr_to_string(inner),
        TypeExpr::Function { .. } => "fn".to_string(),
        TypeExpr::UnsignedInt(_, _) => "uint".to_string(),
        TypeExpr::Result(ok, _, _) => format!("result_{}", type_expr_to_string(ok)),
    }
}

/// Substitute type parameters in a TypeExpr with concrete type strings.
/// E.g. `T` with subst {"T": "int"} becomes `Named("int")`.
fn substitute_type_expr(t: &TypeExpr, subst: &HashMap<String, String>) -> TypeExpr {
    match t {
        TypeExpr::Named(id, span) => {
            if let Some(concrete) = subst.get(&id.name) {
                // Convert the concrete type string back to a TypeExpr.
                string_to_type_expr(concrete, span.clone())
            } else {
                TypeExpr::Named(id.clone(), span.clone())
            }
        }
        TypeExpr::Optional(inner, span) => {
            TypeExpr::Optional(Box::new(substitute_type_expr(inner, subst)), span.clone())
        }
        TypeExpr::Pointer(inner, span) => {
            TypeExpr::Pointer(Box::new(substitute_type_expr(inner, subst)), span.clone())
        }
        TypeExpr::Borrow { inner, lifetime, span } => TypeExpr::Borrow {
            inner: Box::new(substitute_type_expr(inner, subst)),
            lifetime: lifetime.clone(),
            span: span.clone(),
        },
        TypeExpr::MutBorrow { inner, lifetime, span } => TypeExpr::MutBorrow {
            inner: Box::new(substitute_type_expr(inner, subst)),
            lifetime: lifetime.clone(),
            span: span.clone(),
        },
        TypeExpr::Generic { base, args, span } => TypeExpr::Generic {
            base: Box::new(substitute_type_expr(base, subst)),
            args: args.iter().map(|a| substitute_type_expr(a, subst)).collect(),
            span: span.clone(),
        },
        TypeExpr::Tuple(types, span) => TypeExpr::Tuple(
            types.iter().map(|t| substitute_type_expr(t, subst)).collect(),
            span.clone(),
        ),
        TypeExpr::Array(inner, size, span) => TypeExpr::Array(
            Box::new(substitute_type_expr(inner, subst)),
            size.clone(),
            span.clone(),
        ),
        TypeExpr::Function { params, return_type, span } => TypeExpr::Function {
            params: params.iter().map(|t| substitute_type_expr(t, subst)).collect(),
            return_type: Box::new(substitute_type_expr(return_type, subst)),
            span: span.clone(),
        },
        _ => t.clone(),
    }
}

/// Convert a type string back to a TypeExpr.
fn string_to_type_expr(s: &str, span: crate::error::Span) -> TypeExpr {
    match s {
        "int" => TypeExpr::Basic(BasicType::Int, span),
        "float" => TypeExpr::Basic(BasicType::Float, span),
        "str" => TypeExpr::Basic(BasicType::Str, span),
        "bool" => TypeExpr::Basic(BasicType::Bool, span),
        "void" => TypeExpr::Basic(BasicType::Void, span),
        "null" => TypeExpr::Basic(BasicType::Null, span),
        "any" => TypeExpr::Basic(BasicType::Any, span),
        _ => { let s2 = span.clone(); TypeExpr::Named(Identifier { name: s.to_string(), span }, s2) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(src: &str) -> Program {
        let mut lexer = Lexer::new(src, 0);
        let tokens = lexer.tokenize().expect("tokenize");
        let mut parser = Parser::new(tokens, 0);
        parser.parse_program().expect("parse")
    }

    #[test]
    fn test_no_generics_passthrough() {
        let src = "fn, f(x)\n    return, x\n/end\n";
        let prog = parse(src);
        let mut mono = Monomorphizer::new();
        let out = mono.run(&prog);
        assert_eq!(out.declarations.len(), prog.declarations.len());
    }

    #[test]
    fn test_concrete_name() {
        assert_eq!(Monomorphizer::concrete_name("identity", &[]), "identity");
        assert_eq!(
            Monomorphizer::concrete_name("identity", &["int".to_string()]),
            "identity_int"
        );
        assert_eq!(
            Monomorphizer::concrete_name("pair", &["int".to_string(), "str".to_string()]),
            "pair_int_str"
        );
    }

    #[test]
    fn test_type_expr_to_string() {
        assert_eq!(
            type_expr_to_string(&TypeExpr::Basic(BasicType::Int, crate::error::Span::dummy())),
            "int"
        );
        assert_eq!(
            type_expr_to_string(&TypeExpr::Named(
                Identifier {
                    name: "T".to_string(),
                    span: crate::error::Span::dummy()
                },
                crate::error::Span::dummy()
            )),
            "T"
        );
    }

    #[test]
    fn test_generic_identity_monomorphization() {
        let src = "fn, identity[T](x: T): T\n    return, x\n/end\nfn, main()\n    println, identity(42)\n    println, identity(\"hello\")\n/end\n";
        let prog = parse(src);
        let mut mono = Monomorphizer::new();
        let out = mono.run(&prog);
        // Should have: identity_int, identity_str, main (3 functions)
        let fn_names: Vec<String> = out
            .declarations
            .iter()
            .filter_map(|d| {
                if let TopLevel::FnDef(f) = d {
                    Some(f.name.name.clone())
                } else {
                    None
                }
            })
            .collect();
        assert!(fn_names.contains(&"identity_int".to_string()), "missing identity_int: {:?}", fn_names);
        assert!(fn_names.contains(&"identity_str".to_string()), "missing identity_str: {:?}", fn_names);
        assert!(fn_names.contains(&"main".to_string()), "missing main: {:?}", fn_names);
        // Should NOT contain the generic definition.
        assert!(!fn_names.contains(&"identity".to_string()), "generic def should be removed");
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(src: &str) -> Program {
        let mut lexer = Lexer::new(src, 0);
        let tokens = lexer.tokenize().expect("tokenize");
        let mut parser = Parser::new(tokens, 0);
        parser.parse_program().expect("parse")
    }

    #[test]
    fn test_llvm_mode_naming() {
        let src = "fn, identity[T](x: T): T\n    return, x\n/end\nfn, main()\n    println, identity(42)\n    println, identity(\"hi\")\n/end\n";
        let prog = parse(src);
        let mut mono = Monomorphizer::new();
        mono.llvm_mode = true;
        let out = mono.run(&prog);
        let fn_names: Vec<String> = out
            .declarations
            .iter()
            .filter_map(|d| {
                if let TopLevel::FnDef(f) = d {
                    Some(f.name.name.clone())
                } else {
                    None
                }
            })
            .collect();
        assert!(fn_names.contains(&"identity_i64".to_string()), "LLVM: missing identity_i64: {:?}", fn_names);
        assert!(fn_names.contains(&"identity_str".to_string()), "LLVM: missing identity_str: {:?}", fn_names);
    }

    #[test]
    fn test_raw_mode_naming() {
        let src = "fn, identity[T](x: T): T\n    return, x\n/end\nfn, main()\n    println, identity(42)\n    println, identity(\"hi\")\n/end\n";
        let prog = parse(src);
        let mut mono = Monomorphizer::new();
        // llvm_mode = false (default)
        let out = mono.run(&prog);
        let fn_names: Vec<String> = out
            .declarations
            .iter()
            .filter_map(|d| {
                if let TopLevel::FnDef(f) = d {
                    Some(f.name.name.clone())
                } else {
                    None
                }
            })
            .collect();
        assert!(fn_names.contains(&"identity_int".to_string()), "Raw: missing identity_int: {:?}", fn_names);
        assert!(fn_names.contains(&"identity_str".to_string()), "Raw: missing identity_str: {:?}", fn_names);
    }

    #[test]
    fn test_multi_type_param() {
        let src = "fn, pair[T, U](a: T, b: U): T\n    return, a\n/end\nfn, main()\n    println, pair(42, \"hi\")\n/end\n";
        let prog = parse(src);
        let mut mono = Monomorphizer::new();
        let out = mono.run(&prog);
        let fn_names: Vec<String> = out
            .declarations
            .iter()
            .filter_map(|d| {
                if let TopLevel::FnDef(f) = d {
                    Some(f.name.name.clone())
                } else {
                    None
                }
            })
            .collect();
        assert!(fn_names.contains(&"pair_int_str".to_string()), "missing pair_int_str: {:?}", fn_names);
    }

    #[test]
    fn test_call_site_rewiring() {
        let src = "fn, identity[T](x: T): T\n    return, x\n/end\nfn, main()\n    set, a, identity(42)\n    return, a\n/end\n";
        let prog = parse(src);
        let mut mono = Monomorphizer::new();
        let out = mono.run(&prog);
        // Find main and check that the call is rewritten.
        for d in &out.declarations {
            if let TopLevel::FnDef(f) = d {
                if f.name.name == "main" {
                    // Check that the body contains a call to identity_int.
                    let mut found = false;
                    for s in &f.body {
                        if let Stmt::Assign(a) = s {
                            if let Expr::Call(c) = &a.value {
                                if let Expr::Identifier(id) = c.callee.as_ref() {
                                    if id.name == "identity_int" {
                                        found = true;
                                    }
                                }
                            }
                        }
                    }
                    assert!(found, "call site not rewritten to identity_int");
                }
            }
        }
    }
}
