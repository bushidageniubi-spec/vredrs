use super::symbol::*;
use crate::error::{CompilerError, Result, Span};
use crate::parser::ast::*;

pub struct TypeChecker {
    pub symtab: SymbolTable,
    /// The declared return type of the function currently being checked,
    /// converted to `TypeInfo` (recognizing the function's own type
    /// parameters). `None` means we are not inside a function body
    /// (or the function declares no return type, in which case we have
    /// nothing to check against).
    ///
    /// This is saved/restored around nested function definitions so a
    /// nested function's return type does not leak into the enclosing
    /// function. (Today the AST has no first-class nested `FnDef`
    /// statement, but lambdas and methods can still appear inside
    /// bodies; resetting on scope exit keeps the field honest.)
    current_return_type: Option<TypeInfo>,
}

impl TypeChecker {
    pub fn new(symtab: SymbolTable) -> Self {
        TypeChecker {
            symtab,
            current_return_type: None,
        }
    }

    pub fn check(mut self, prog: &Program) -> Result<()> {
        for d in &prog.declarations {
            self.check_top_level(d)?;
        }
        Ok(())
    }

    fn check_top_level(&mut self, tl: &TopLevel) -> Result<()> {
        match tl {
            TopLevel::FnDef(fd) => self.check_fn_def(fd),
            TopLevel::Statement(s) => {
                self.infer_stmt(s)?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn check_fn_def(&mut self, fd: &FnDef) -> Result<()> {
        self.symtab.enter_scope(ScopeKind::Function);
        // Build a set of type parameter names for this function.
        let type_param_set: std::collections::HashSet<String> =
            fd.type_params.iter().cloned().collect();
        // Helper: convert a TypeExpr to TypeInfo, recognizing type parameters.
        let convert_type = |te: &TypeExpr| -> TypeInfo {
            match te {
                TypeExpr::Named(id, _) if type_param_set.contains(&id.name) => {
                    TypeInfo::TypeVariable(id.name.clone())
                }
                _ => TypeInfo::from_ast_type(te),
            }
        };
        for p in &fd.params {
            let t = p
                .type_annotation
                .as_ref()
                .map(|te| convert_type(te))
                .unwrap_or(TypeInfo::Unknown);
            let _ = self.symtab.declare(
                &p.name.name,
                SymbolEntry::Variable {
                    name: p.name.name.clone(),
                    typ: Some(t),
                    mutable: false,
                    span: p.name.span.clone(),
                },
            );
        }
        // Save and install the declared return type so `Stmt::Return`
        // can compare each `return <expr>;` against it. We convert it
        // through the same `convert_type` helper so generic return types
        // like `fn foo<T>(x: T): T` are recognized as type variables
        // (and thus treated as compatible with anything). `None` means
        // the function declares no return type — in that case we leave
        // `current_return_type` as `None` and `Stmt::Return` will skip
        // the comparison.
        let saved_return_type = self.current_return_type.take();
        self.current_return_type = fd.return_type.as_ref().map(|te| convert_type(te));
        for s in &fd.body {
            self.infer_stmt(s)?;
        }
        // Restore the enclosing function's declared return type (or None
        // if we were at top level).
        self.current_return_type = saved_return_type;
        self.symtab.exit_scope();
        Ok(())
    }

    fn infer_stmt(&mut self, s: &Stmt) -> Result<Option<TypeInfo>> {
        match s {
            Stmt::Assign(a) => {
                if a.operator == AssignOp::Delete {
                    return Ok(None);
                }
                let vt = self.infer_expr(&a.value)?;
                for t in &a.targets {
                    if let Assignee::Identifier(id) = t {
                        // Check for type mismatch with existing declaration.
                        if let Some(existing) = self.symtab.lookup(&id.name) {
                            if let SymbolEntry::Variable { typ: Some(declared), .. } = existing {
                                // Skip type checking for type variables (generic type
                                // parameters). The actual type is determined during
                                // monomorphization in LLVM/Raw mode. In VM mode,
                                // type variables are treated as Any.
                                let is_type_var = matches!(declared, TypeInfo::TypeVariable(_));
                                if !is_type_var
                                    && *declared != TypeInfo::Unknown && vt != TypeInfo::Unknown
                                    && !declared.is_compatible_with(&vt)
                                {
                                    return Err(CompilerError::type_error(
                                        format!(
                                            "type mismatch: cannot assign {:?} to variable '{}' declared as {:?}",
                                            vt, id.name, declared
                                        ),
                                        id.span.clone(),
                                    ));
                                }
                            }
                        }
                        let _ = self.symtab.declare(
                            &id.name,
                            SymbolEntry::Variable {
                                name: id.name.clone(),
                                typ: Some(vt.clone()),
                                mutable: true,
                                span: id.span.clone(),
                            },
                        );
                    }
                }
                Ok(Some(vt))
            }
            Stmt::Return(rs) => {
                let mut types = vec![];
                for v in &rs.values {
                    types.push(self.infer_expr(v)?);
                }
                let inferred = if types.len() == 1 {
                    types[0].clone()
                } else if types.is_empty() {
                    // Bare `return;` — semantically a Void/unit return.
                    TypeInfo::Void
                } else {
                    TypeInfo::Tuple(types)
                };
                // Compare the inferred return type against the enclosing
                // function's declared return type (if any). We only report
                // a mismatch when BOTH types are concretely known — if
                // either is Unknown/Any we cannot reliably tell whether
                // they are incompatible, so we skip the check (mirrors
                // the existing `Stmt::Assign` policy). TypeVariable
                // (generic parameters) and Void are treated as "known"
                // here; `is_compatible_with` already accepts TypeVariable
                // against anything, and `Void` only matches `Void`.
                if let Some(declared) = &self.current_return_type {
                    let decl_known =
                        !matches!(declared, TypeInfo::Unknown | TypeInfo::Any);
                    let inf_known =
                        !matches!(inferred, TypeInfo::Unknown | TypeInfo::Any);
                    if decl_known && inf_known && !declared.is_compatible_with(&inferred) {
                        return Err(CompilerError::type_error(
                            format!(
                                "return type mismatch: function declares return type {:?}, but `return` expression has type {:?}",
                                declared, inferred
                            ),
                            rs.span.clone(),
                        ));
                    }
                }
                Ok(Some(inferred))
            }
            Stmt::Paste(ps) => {
                for a in &ps.args {
                    self.infer_expr(a)?;
                }
                Ok(None)
            }
            Stmt::Println(ps) => {
                for a in &ps.args {
                    self.infer_expr(a)?;
                }
                Ok(None)
            }
            Stmt::Expr(es) => {
                self.infer_expr(&es.expr)?;
                Ok(None)
            }
            Stmt::If(is_) => {
                self.infer_expr(&is_.condition)?;
                self.symtab.enter_scope(ScopeKind::Block);
                for s in &is_.then_body {
                    self.infer_stmt(s)?;
                }
                self.symtab.exit_scope();
                for (c, b) in &is_.elif_chain {
                    self.infer_expr(c)?;
                    self.symtab.enter_scope(ScopeKind::Block);
                    for s in b {
                        self.infer_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                if let Some(b) = &is_.else_body {
                    self.symtab.enter_scope(ScopeKind::Block);
                    for s in b {
                        self.infer_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                Ok(None)
            }
            Stmt::While(ws) => {
                self.infer_expr(&ws.condition)?;
                self.symtab.enter_scope(ScopeKind::Block);
                for s in &ws.body {
                    self.infer_stmt(s)?;
                }
                self.symtab.exit_scope();
                if let Some(eb) = &ws.else_body {
                    self.symtab.enter_scope(ScopeKind::Block);
                    for s in eb {
                        self.infer_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                Ok(None)
            }
            Stmt::ForIn(fi) => {
                self.infer_expr(&fi.iterable)?;
                self.symtab.enter_scope(ScopeKind::Block);
                let _ = self.symtab.declare(
                    &fi.var.name,
                    SymbolEntry::Variable {
                        name: fi.var.name.clone(),
                        typ: Some(TypeInfo::Unknown),
                        mutable: false,
                        span: fi.var.span.clone(),
                    },
                );
                for s in &fi.body {
                    self.infer_stmt(s)?;
                }
                self.symtab.exit_scope();
                if let Some(eb) = &fi.else_body {
                    self.symtab.enter_scope(ScopeKind::Block);
                    for s in eb {
                        self.infer_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                Ok(None)
            }
            Stmt::ForRange(fr) => {
                self.infer_expr(&fr.from)?;
                self.infer_expr(&fr.to)?;
                if let Some(step) = &fr.step {
                    self.infer_expr(step)?;
                }
                self.symtab.enter_scope(ScopeKind::Block);
                let _ = self.symtab.declare(
                    &fr.var.name,
                    SymbolEntry::Variable {
                        name: fr.var.name.clone(),
                        typ: Some(TypeInfo::Unknown),
                        mutable: false,
                        span: fr.var.span.clone(),
                    },
                );
                for s in &fr.body {
                    self.infer_stmt(s)?;
                }
                self.symtab.exit_scope();
                if let Some(eb) = &fr.else_body {
                    self.symtab.enter_scope(ScopeKind::Block);
                    for s in eb {
                        self.infer_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                Ok(None)
            }
            Stmt::Loop(l) => {
                self.symtab.enter_scope(ScopeKind::Block);
                for s in &l.body {
                    self.infer_stmt(s)?;
                }
                self.symtab.exit_scope();
                if let Some(eb) = &l.else_body {
                    self.symtab.enter_scope(ScopeKind::Block);
                    for s in eb {
                        self.infer_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                Ok(None)
            }
            Stmt::Try(t) => {
                self.symtab.enter_scope(ScopeKind::Block);
                for s in &t.try_body {
                    self.infer_stmt(s)?;
                }
                self.symtab.exit_scope();
                if let Some(cb) = &t.catch_body {
                    self.symtab.enter_scope(ScopeKind::Block);
                    if let Some(cv) = &t.catch_var {
                        let _ = self.symtab.declare(
                            &cv.name,
                            SymbolEntry::Variable {
                                name: cv.name.clone(),
                                typ: Some(TypeInfo::Unknown),
                                mutable: false,
                                span: cv.span.clone(),
                            },
                        );
                    }
                    for s in cb {
                        self.infer_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                if let Some(fb) = &t.finally_body {
                    self.symtab.enter_scope(ScopeKind::Block);
                    for s in fb {
                        self.infer_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                Ok(None)
            }
            Stmt::With(w) => {
                self.infer_expr(&w.manager)?;
                self.symtab.enter_scope(ScopeKind::Block);
                if let Some(v) = &w.var {
                    let _ = self.symtab.declare(
                        &v.name,
                        SymbolEntry::Variable {
                            name: v.name.clone(),
                            typ: Some(TypeInfo::Unknown),
                            mutable: false,
                            span: v.span.clone(),
                        },
                    );
                }
                for s in &w.body {
                    self.infer_stmt(s)?;
                }
                self.symtab.exit_scope();
                Ok(None)
            }
            Stmt::Match(m) => {
                self.infer_expr(&m.expr)?;
                for case in &m.cases {
                    self.symtab.enter_scope(ScopeKind::Block);
                    // Pattern bindings are not yet tracked in the symbol
                    // table (that requires pattern-aware declaration). We
                    // still walk the optional guard and the case body so
                    // type errors inside the case are caught.
                    if let Some(guard) = &case.guard {
                        self.infer_expr(guard)?;
                    }
                    for s in &case.body {
                        self.infer_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                if let Some(eb) = &m.else_case {
                    self.symtab.enter_scope(ScopeKind::Block);
                    for s in eb {
                        self.infer_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn infer_expr(&self, e: &Expr) -> Result<TypeInfo> {
        Ok(match e {
            Expr::Integer(_) => TypeInfo::Basic(BasicType::Int),
            Expr::Float(_) => TypeInfo::Basic(BasicType::Float),
            Expr::String_(_) | Expr::MultiLineString(_) => TypeInfo::Basic(BasicType::Str),
            Expr::Bool(_) => TypeInfo::Basic(BasicType::Bool),
            Expr::Null(_) => TypeInfo::Basic(BasicType::Null),
            Expr::Identifier(id) => match self.symtab.lookup(&id.name) {
                Some(SymbolEntry::Variable { typ: Some(t), .. }) => t.clone(),
                Some(SymbolEntry::Function { ret: Some(r), .. }) => r.clone(),
                _ => TypeInfo::Unknown,
            },
            Expr::Binary(be) => match be.operator {
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div
                | BinaryOp::FloorDiv | BinaryOp::Mod | BinaryOp::Power => {
                    // Result type depends on operand types: if either is
                    // Float, the result is Float; otherwise Int.
                    let lt = self.infer_expr(&be.left).unwrap_or(TypeInfo::Unknown);
                    let rt = self.infer_expr(&be.right).unwrap_or(TypeInfo::Unknown);
                    if matches!(lt, TypeInfo::Basic(BasicType::Float))
                        || matches!(rt, TypeInfo::Basic(BasicType::Float))
                    {
                        TypeInfo::Basic(BasicType::Float)
                    } else {
                        TypeInfo::Basic(BasicType::Int)
                    }
                }
                BinaryOp::Eq
                | BinaryOp::Ne
                | BinaryOp::Lt
                | BinaryOp::Gt
                | BinaryOp::Le
                | BinaryOp::Ge
                | BinaryOp::And
                | BinaryOp::Or => TypeInfo::Basic(BasicType::Bool),
                _ => TypeInfo::Unknown,
            },
            Expr::Call(ce) => {
                if let Expr::Identifier(id) = ce.callee.as_ref() {
                    if let Some(SymbolEntry::Function { ret: Some(r), .. }) =
                        self.symtab.lookup(&id.name)
                    {
                        r.clone()
                    } else {
                        TypeInfo::Unknown
                    }
                } else {
                    TypeInfo::Unknown
                }
            }
            Expr::Ternary(te) => {
                let tt = self.infer_expr(&te.true_branch)?;
                // The false branch must be inferred too — otherwise type
                // errors inside it (e.g. unknown function calls) would
                // silently slip through.
                let ft = self.infer_expr(&te.false_branch)?;
                if tt == ft {
                    tt
                } else if matches!(tt, TypeInfo::Unknown) {
                    ft
                } else if matches!(ft, TypeInfo::Unknown) {
                    tt
                } else if matches!(tt, TypeInfo::Any) || matches!(ft, TypeInfo::Any) {
                    TypeInfo::Any
                } else {
                    // Branches have incompatible known types. Return the
                    // true-branch type as the nominal type — callers
                    // typically only need an upper bound, and downstream
                    // code can coerce/widen as needed.
                    tt
                }
            }
            Expr::Cast(ce) => TypeInfo::from_ast_type(&ce.type_expr),
            _ => TypeInfo::Unknown,
        })
    }
}
