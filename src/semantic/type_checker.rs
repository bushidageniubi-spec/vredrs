use super::symbol::*;
use crate::error::{CompilerError, Result, Span};
use crate::parser::ast::*;

pub struct TypeChecker {
    pub symtab: SymbolTable,
}

impl TypeChecker {
    pub fn new(symtab: SymbolTable) -> Self {
        TypeChecker { symtab }
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
        for p in &fd.params {
            let t = p
                .type_annotation
                .as_ref()
                .map(TypeInfo::from_ast_type)
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
        for s in &fd.body {
            self.infer_stmt(s)?;
        }
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
                Ok(Some(if types.len() == 1 {
                    types[0].clone()
                } else {
                    TypeInfo::Tuple(types)
                }))
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
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div => {
                    TypeInfo::Basic(BasicType::Int)
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
            Expr::Ternary(te) => self.infer_expr(&te.true_branch)?,
            Expr::Cast(ce) => TypeInfo::from_ast_type(&ce.type_expr),
            _ => TypeInfo::Unknown,
        })
    }
}
