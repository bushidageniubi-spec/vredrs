use super::symbol::*;
use crate::error::{CompilerError, Result, Span};
use crate::parser::ast::*;

pub struct Resolver {
    pub symtab: SymbolTable,
    errors: Vec<CompilerError>,
}

impl Resolver {
    pub fn new() -> Self {
        let mut st = SymbolTable::new();
        let s = Span::dummy();
        st.declare(
            "paste",
            SymbolEntry::Function {
                name: "paste".into(),
                params: vec![],
                ret: None,
                is_async: false,
                span: s.clone(),
            },
        )
        .ok();
        st.declare(
            "println",
            SymbolEntry::Function {
                name: "println".into(),
                params: vec![],
                ret: None,
                is_async: false,
                span: s.clone(),
            },
        )
        .ok();
        st.declare(
            "input",
            SymbolEntry::Function {
                name: "input".into(),
                params: vec![],
                ret: Some(TypeInfo::Basic(BasicType::Str)),
                is_async: false,
                span: s.clone(),
            },
        )
        .ok();
        st.declare(
            "int",
            SymbolEntry::Function {
                name: "int".into(),
                params: vec![],
                ret: Some(TypeInfo::Basic(BasicType::Int)),
                is_async: false,
                span: s.clone(),
            },
        )
        .ok();
        st.declare(
            "float",
            SymbolEntry::Function {
                name: "float".into(),
                params: vec![],
                ret: Some(TypeInfo::Basic(BasicType::Float)),
                is_async: false,
                span: s.clone(),
            },
        )
        .ok();
        st.declare(
            "str",
            SymbolEntry::Function {
                name: "str".into(),
                params: vec![],
                ret: Some(TypeInfo::Basic(BasicType::Str)),
                is_async: false,
                span: s.clone(),
            },
        )
        .ok();
        Resolver {
            symtab: st,
            errors: vec![],
        }
    }

    pub fn resolve(mut self, prog: &Program) -> Result<SymbolTable> {
        for d in &prog.declarations {
            let _ = self.resolve_top_level(d);
        }
        if !self.errors.is_empty() {
            return Err(self.errors.remove(0));
        }
        Ok(self.symtab)
    }

    fn resolve_top_level(&mut self, tl: &TopLevel) -> Result<()> {
        match tl {
            TopLevel::FnDef(fd) => self.resolve_fn_def(fd),
            TopLevel::StructDef(sd) => {
                self.symtab
                    .declare_type(&sd.name.name, TypeInfo::Named(sd.name.name.clone()));
                Ok(())
            }
            TopLevel::ClassDef(cd) => {
                self.symtab
                    .declare_type(&cd.name.name, TypeInfo::Named(cd.name.name.clone()));
                self.symtab.enter_scope(ScopeKind::Class);
                for f in &cd.fields {
                    let t = f.type_annotation.as_ref().map(TypeInfo::from_ast_type);
                    self.symtab.declare(
                        &f.name.name,
                        SymbolEntry::Variable {
                            name: f.name.name.clone(),
                            typ: t,
                            mutable: false,
                            span: f.span.clone(),
                        },
                    )?;
                }
                for m in &cd.methods {
                    self.resolve_fn_def(m)?;
                }
                self.symtab.exit_scope();
                Ok(())
            }
            TopLevel::InterfaceDef(id) => {
                self.symtab
                    .declare_type(&id.name.name, TypeInfo::Named(id.name.name.clone()));
                Ok(())
            }
            TopLevel::EnumDef(ed) => {
                self.symtab
                    .declare_type(&ed.name.name, TypeInfo::Named(ed.name.name.clone()));
                Ok(())
            }
            TopLevel::TypeAlias(ta) => {
                let ti = TypeInfo::from_ast_type(&ta.target);
                self.symtab.declare_type(&ta.name.name, ti);
                Ok(())
            }
            TopLevel::ConstExpr(ce) => {
                self.resolve_expr(&ce.value)?;
                let t = ce.type_annotation.as_ref().map(TypeInfo::from_ast_type);
                self.symtab.declare(
                    &ce.name.name,
                    SymbolEntry::Variable {
                        name: ce.name.name.clone(),
                        typ: t,
                        mutable: false,
                        span: ce.span.clone(),
                    },
                )?;
                Ok(())
            }
            TopLevel::LazyDef(ld) => {
                self.resolve_expr(&ld.value)?;
                let t = ld.type_annotation.as_ref().map(TypeInfo::from_ast_type);
                self.symtab.declare(
                    &ld.name.name,
                    SymbolEntry::Variable {
                        name: ld.name.name.clone(),
                        typ: t,
                        mutable: false,
                        span: ld.span.clone(),
                    },
                )?;
                Ok(())
            }
            TopLevel::Statement(s) => self.resolve_stmt(s),
            _ => Ok(()),
        }
    }

    fn resolve_fn_def(&mut self, fd: &FnDef) -> Result<()> {
        let params: Vec<_> = fd
            .params
            .iter()
            .map(|p| {
                (
                    p.name.name.clone(),
                    p.type_annotation
                        .as_ref()
                        .map(TypeInfo::from_ast_type)
                        .unwrap_or(TypeInfo::Unknown),
                )
            })
            .collect();
        let ret = fd.return_type.as_ref().map(TypeInfo::from_ast_type);
        self.symtab.declare(
            &fd.name.name,
            SymbolEntry::Function {
                name: fd.name.name.clone(),
                params: params.clone(),
                ret: ret.clone(),
                is_async: fd.is_async,
                span: fd.name.span.clone(),
            },
        )?;
        self.symtab.enter_scope(ScopeKind::Function);
        for (n, _) in &params {
            let _ = self.symtab.declare(
                n,
                SymbolEntry::Variable {
                    name: n.clone(),
                    typ: None,
                    mutable: false,
                    span: Span::dummy(),
                },
            );
        }
        for s in &fd.body {
            self.resolve_stmt(s)?;
        }
        self.symtab.exit_scope();
        Ok(())
    }

    fn resolve_stmt(&mut self, s: &Stmt) -> Result<()> {
        match s {
            Stmt::Assign(a) => {
                if a.operator != AssignOp::Delete {
                    self.resolve_expr(&a.value)?;
                }
                for t in &a.targets {
                    self.resolve_assignee(t)?;
                    if a.operator != AssignOp::Delete {
                        if let Assignee::Identifier(id) = t {
                            let _ = self.symtab.declare(
                                &id.name,
                                SymbolEntry::Variable {
                                    name: id.name.clone(),
                                    typ: None,
                                    mutable: true,
                                    span: id.span.clone(),
                                },
                            );
                        }
                    }
                }
                Ok(())
            }
            Stmt::Paste(ps) => {
                for a in &ps.args {
                    self.resolve_expr(a)?;
                }
                Ok(())
            }
            Stmt::Println(ps) => {
                for a in &ps.args {
                    self.resolve_expr(a)?;
                }
                Ok(())
            }
            Stmt::If(is_) => {
                self.resolve_expr(&is_.condition)?;
                self.symtab.enter_scope(ScopeKind::Block);
                for s in &is_.then_body {
                    self.resolve_stmt(s)?;
                }
                self.symtab.exit_scope();
                for (c, b) in &is_.elif_chain {
                    self.resolve_expr(c)?;
                    self.symtab.enter_scope(ScopeKind::Block);
                    for s in b {
                        self.resolve_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                if let Some(b) = &is_.else_body {
                    self.symtab.enter_scope(ScopeKind::Block);
                    for s in b {
                        self.resolve_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                Ok(())
            }
            Stmt::While(ws) => {
                self.resolve_expr(&ws.condition)?;
                self.symtab.enter_scope(ScopeKind::Block);
                for s in &ws.body {
                    self.resolve_stmt(s)?;
                }
                self.symtab.exit_scope();
                Ok(())
            }
            Stmt::ForIn(fi) => {
                self.resolve_expr(&fi.iterable)?;
                self.symtab.enter_scope(ScopeKind::Block);
                let _ = self.symtab.declare(
                    &fi.var.name,
                    SymbolEntry::Variable {
                        name: fi.var.name.clone(),
                        typ: None,
                        mutable: false,
                        span: fi.var.span.clone(),
                    },
                );
                for s in &fi.body {
                    self.resolve_stmt(s)?;
                }
                self.symtab.exit_scope();
                Ok(())
            }
            Stmt::ForRange(fr) => {
                self.resolve_expr(&fr.from)?;
                self.resolve_expr(&fr.to)?;
                self.symtab.enter_scope(ScopeKind::Block);
                let _ = self.symtab.declare(
                    &fr.var.name,
                    SymbolEntry::Variable {
                        name: fr.var.name.clone(),
                        typ: None,
                        mutable: false,
                        span: fr.var.span.clone(),
                    },
                );
                for s in &fr.body {
                    self.resolve_stmt(s)?;
                }
                self.symtab.exit_scope();
                Ok(())
            }
            Stmt::Match(ms) => {
                self.resolve_expr(&ms.expr)?;
                for c in &ms.cases {
                    self.symtab.enter_scope(ScopeKind::Block);
                    self.resolve_pattern(&c.pattern)?;
                    if let Some(g) = &c.guard {
                        self.resolve_expr(g)?;
                    }
                    for s in &c.body {
                        self.resolve_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                if let Some(b) = &ms.else_case {
                    self.symtab.enter_scope(ScopeKind::Block);
                    for s in b {
                        self.resolve_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                Ok(())
            }
            Stmt::Expr(es) => self.resolve_expr(&es.expr),
            Stmt::DirectiveBlock(db) => {
                self.symtab.enter_scope(ScopeKind::Block);
                for s in &db.body {
                    self.resolve_stmt(s)?;
                }
                self.symtab.exit_scope();
                Ok(())
            }
            Stmt::ScopeBlock(sb) => {
                self.symtab.enter_scope(ScopeKind::Block);
                for s in &sb.body {
                    self.resolve_stmt(s)?;
                }
                self.symtab.exit_scope();
                Ok(())
            }
            Stmt::Defer(ds) => self.resolve_stmt(&ds.stmt),
            Stmt::Throw(ts) => self.resolve_expr(&ts.value),
            Stmt::Spawn(ss) => self.resolve_expr(&ss.call),
            Stmt::Yield(ys) => {
                if let Some(v) = &ys.value {
                    self.resolve_expr(v)?;
                }
                Ok(())
            }
            Stmt::Try(ts) => {
                self.symtab.enter_scope(ScopeKind::Block);
                for s in &ts.try_body {
                    self.resolve_stmt(s)?;
                }
                self.symtab.exit_scope();
                if let Some(v) = &ts.catch_var {
                    self.symtab.enter_scope(ScopeKind::Block);
                    let _ = self.symtab.declare(
                        &v.name,
                        SymbolEntry::Variable {
                            name: v.name.clone(),
                            typ: None,
                            mutable: false,
                            span: v.span.clone(),
                        },
                    );
                    if let Some(b) = &ts.catch_body {
                        for s in b {
                            self.resolve_stmt(s)?;
                        }
                    }
                    self.symtab.exit_scope();
                }
                if let Some(b) = &ts.finally_body {
                    self.symtab.enter_scope(ScopeKind::Block);
                    for s in b {
                        self.resolve_stmt(s)?;
                    }
                    self.symtab.exit_scope();
                }
                Ok(())
            }
            Stmt::With(ws) => {
                self.resolve_expr(&ws.manager)?;
                self.symtab.enter_scope(ScopeKind::Block);
                if let Some(v) = &ws.var {
                    let _ = self.symtab.declare(
                        &v.name,
                        SymbolEntry::Variable {
                            name: v.name.clone(),
                            typ: None,
                            mutable: false,
                            span: v.span.clone(),
                        },
                    );
                }
                for s in &ws.body {
                    self.resolve_stmt(s)?;
                }
                self.symtab.exit_scope();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn resolve_assignee(&mut self, target: &Assignee) -> Result<()> {
        match target {
            Assignee::Identifier(_) | Assignee::Qualified(_) => Ok(()),
            Assignee::Member(m) => self.resolve_expr(&m.target),
            Assignee::Index(i) => {
                self.resolve_expr(&i.target)?;
                self.resolve_expr(&i.index)
            }
            Assignee::Tuple(items) => {
                for item in items {
                    self.resolve_assignee(item)?;
                }
                Ok(())
            }
        }
    }

    fn resolve_expr(&mut self, e: &Expr) -> Result<()> {
        match e {
            Expr::Identifier(id) => {
                let _ = self.symtab.lookup(&id.name);
                Ok(())
            }
            Expr::Binary(be) => {
                self.resolve_expr(&be.left)?;
                self.resolve_expr(&be.right)?;
                Ok(())
            }
            Expr::Unary(ue) => {
                self.resolve_expr(&ue.operand)?;
                Ok(())
            }
            Expr::Call(ce) => {
                self.resolve_expr(&ce.callee)?;
                for a in &ce.args {
                    self.resolve_expr(a)?;
                }
                Ok(())
            }
            Expr::MethodCall(mc) => {
                self.resolve_expr(&mc.receiver)?;
                for a in &mc.args {
                    self.resolve_expr(a)?;
                }
                Ok(())
            }
            Expr::MemberAccess(ma) => {
                self.resolve_expr(&ma.target)?;
                Ok(())
            }
            Expr::Index(ie) => {
                self.resolve_expr(&ie.target)?;
                self.resolve_expr(&ie.index)?;
                Ok(())
            }
            Expr::Slice(se) => {
                self.resolve_expr(&se.target)?;
                if let Some(e) = &se.start {
                    self.resolve_expr(e)?;
                }
                if let Some(e) = &se.end {
                    self.resolve_expr(e)?;
                }
                if let Some(e) = &se.step {
                    self.resolve_expr(e)?;
                }
                Ok(())
            }
            Expr::Await(ae) => self.resolve_expr(&ae.expr),
            Expr::Cast(ce) => self.resolve_expr(&ce.expr),
            Expr::List(ll) => {
                for e in &ll.elements {
                    self.resolve_expr(e)?;
                }
                Ok(())
            }
            Expr::Dict(dl) => {
                for (k, v) in &dl.entries {
                    self.resolve_expr(k)?;
                    self.resolve_expr(v)?;
                }
                Ok(())
            }
            Expr::Ternary(te) => {
                self.resolve_expr(&te.condition)?;
                self.resolve_expr(&te.true_branch)?;
                self.resolve_expr(&te.false_branch)?;
                Ok(())
            }
            Expr::Lambda(le) => {
                self.symtab.enter_scope(ScopeKind::Function);
                for p in &le.params {
                    let _ = self.symtab.declare(
                        &p.name.name,
                        SymbolEntry::Variable {
                            name: p.name.name.clone(),
                            typ: None,
                            mutable: false,
                            span: p.span.clone(),
                        },
                    );
                }
                self.resolve_expr(&le.body)?;
                self.symtab.exit_scope();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn resolve_pattern(&mut self, p: &Pattern) -> Result<()> {
        match p {
            Pattern::Binding(bp) => {
                let _ = self.symtab.declare(
                    &bp.name.name,
                    SymbolEntry::Variable {
                        name: bp.name.name.clone(),
                        typ: None,
                        mutable: false,
                        span: bp.name.span.clone(),
                    },
                );
                Ok(())
            }
            Pattern::Tuple(tp) => {
                for p in &tp.elements {
                    self.resolve_pattern(p)?;
                }
                Ok(())
            }
            Pattern::List(lp) => {
                for p in &lp.elements {
                    self.resolve_pattern(p)?;
                }
                if let Some(r) = &lp.rest {
                    let _ = self.symtab.declare(
                        &r.name,
                        SymbolEntry::Variable {
                            name: r.name.clone(),
                            typ: None,
                            mutable: false,
                            span: r.span.clone(),
                        },
                    );
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
