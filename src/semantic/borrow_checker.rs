//! Borrow checker for raw mode (0.1.4).

use crate::error::{CompilerError, ErrorCode, Result, Span};
use crate::parser::ast::*;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnerState { Owned, Moved, BorrowedImmutable(usize), BorrowedMutable }

pub struct BorrowChecker {
    states: HashMap<String, OwnerState>,
    errors: Vec<CompilerError>,
}

impl BorrowChecker {
    pub fn new() -> Self { BorrowChecker { states: HashMap::new(), errors: Vec::new() } }
    pub fn check_fn(&mut self, fn_def: &FnDef) -> Vec<CompilerError> {
        self.states.clear();
        for p in &fn_def.params { self.states.insert(p.name.name.clone(), OwnerState::Owned); }
        for stmt in &fn_def.body { self.check_stmt(stmt); }
        self.errors.clone()
    }
    fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Assign(a) => { self.check_expr(&a.value); for t in &a.targets { if let Assignee::Identifier(id) = t { self.states.insert(id.name.clone(), OwnerState::Owned); } } }
            Stmt::Return(r) => { for v in &r.values { self.check_expr(v); } }
            Stmt::Expr(e) => { self.check_expr(&e.expr); }
            Stmt::If(i) => { self.check_expr(&i.condition); for s in &i.then_body { self.check_stmt(s); } for (_,b) in &i.elif_chain { for s in b { self.check_stmt(s); } } if let Some(eb) = &i.else_body { for s in eb { self.check_stmt(s); } } }
            Stmt::While(w) => { self.check_expr(&w.condition); for s in &w.body { self.check_stmt(s); } }
            _ => {}
        }
    }
    fn check_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Identifier(id) => { if let Some(OwnerState::Moved) = self.states.get(&id.name) { self.errors.push(CompilerError::new(format!("value '{}' was moved", id.name), crate::error::ErrorKind::SemanticError, Some(id.span.clone())).with_code(ErrorCode::V2001)); } }
            Expr::Call(c) => { for a in &c.args { self.check_expr(a); } }
            Expr::Binary(b) => { self.check_expr(&b.left); self.check_expr(&b.right); }
            _ => {}
        }
    }
    pub fn mark_moved(&mut self, name: &str) { self.states.insert(name.to_string(), OwnerState::Moved); }
    pub fn is_owned(&self, name: &str) -> bool { self.states.get(name) == Some(&OwnerState::Owned) }
}
impl Default for BorrowChecker { fn default() -> Self { Self::new() } }

pub fn check_program(program: &Program) -> Vec<CompilerError> {
    let mut checker = BorrowChecker::new();
    let mut all = Vec::new();
    for decl in &program.declarations { if let TopLevel::FnDef(f) = decl { all.extend(checker.check_fn(f)); } }
    all
}
