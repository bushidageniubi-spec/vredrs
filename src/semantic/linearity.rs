//! Flow analysis: unused variable detection and unreachable code detection.
//!
//! This pass walks the AST and emits warnings for:
//! - Variables declared but never used (C0208)
//! - Code after an always-terminating statement (C0207)
//!
//! Warnings are collected and returned; they do not fail compilation.

use crate::error::{CompilerError, Result, Span};
use crate::parser::ast::*;
use std::collections::HashSet;

pub struct LinearityChecker {
    warnings: Vec<CompilerError>,
}

impl LinearityChecker {
    pub fn new() -> Self {
        LinearityChecker {
            warnings: Vec::new(),
        }
    }

    pub fn check(&mut self, prog: &Program) -> Result<()> {
        self.warnings.clear();
        self.check_program(prog);
        // Warnings are now stored on `self` and accessible via `warnings()`.
        // They do not fail compilation — callers (LSP/CLI) can opt to surface them.
        Ok(())
    }

    /// Return the warnings collected by the most recent `check` call.
    /// Each entry is a `CompilerError` created via
    /// `CompilerError::semantic_warning`; the `message` field already
    /// carries a `warning: ` prefix.
    pub fn warnings(&self) -> &[CompilerError] {
        &self.warnings
    }

    fn check_program(&mut self, prog: &Program) {
        for d in &prog.declarations {
            self.check_top_level(d);
        }
    }

    fn check_top_level(&mut self, d: &TopLevel) {
        match d {
            TopLevel::FnDef(f) => self.check_fn(f),
            TopLevel::LazyFnDef(l) => self.check_fn(&l.fn_def),
            TopLevel::ClassDef(c) => {
                for m in &c.methods {
                    self.check_fn(m);
                }
            }
            TopLevel::StructDef(s) => {
                for m in &s.methods {
                    self.check_fn(m);
                }
            }
            TopLevel::Statement(s) => self.check_stmt(s),
            TopLevel::ConditionalCompile(cc) => {
                for x in &cc.then_body {
                    self.check_top_level(x);
                }
                if let Some(else_body) = &cc.else_body {
                    for x in else_body {
                        self.check_top_level(x);
                    }
                }
            }
            _ => {}
        }
    }

    fn check_fn(&mut self, f: &FnDef) {
        let mut used: HashSet<String> = HashSet::new();
        // Parameters that start with _ are intentionally unused.
        for p in &f.params {
            if !p.name.name.starts_with('_') {
                used.insert(p.name.name.clone()); // mark as "declared, check usage"
            }
        }
        let mut collector = UsageCollector {
            used: HashSet::new(),
        };
        for s in &f.body {
            collector.collect_stmt(s);
        }
        // Report unused parameters.
        for p in &f.params {
            if !p.name.name.starts_with('_') && !collector.used.contains(&p.name.name) {
                self.warnings.push(CompilerError::semantic_warning(
                    format!("unused parameter '{}'", p.name.name),
                    p.span.clone(),
                ));
            }
        }
        // Check for unreachable code in the body.
        self.check_unreachable(&f.body);
    }

    fn check_stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::If(i) => {
                self.check_unreachable(&i.then_body);
                for (_, body) in &i.elif_chain {
                    self.check_unreachable(body);
                }
                if let Some(else_body) = &i.else_body {
                    self.check_unreachable(else_body);
                }
            }
            Stmt::While(w) => self.check_unreachable(&w.body),
            Stmt::ForIn(f) => self.check_unreachable(&f.body),
            Stmt::ForRange(f) => self.check_unreachable(&f.body),
            Stmt::Loop(l) => self.check_unreachable(&l.body),
            Stmt::Try(t) => {
                self.check_unreachable(&t.try_body);
                if let Some(cb) = &t.catch_body {
                    self.check_unreachable(cb);
                }
                if let Some(fb) = &t.finally_body {
                    self.check_unreachable(fb);
                }
            }
            Stmt::With(w) => self.check_unreachable(&w.body),
            _ => {}
        }
    }

    /// Walk a statement list and report any statements that appear after
    /// an always-terminating statement (return, break, continue, throw,
    /// panic, or a loop with no exit condition).
    fn check_unreachable(&mut self, body: &[Stmt]) {
        let mut terminated = false;
        for s in body {
            if terminated {
                self.warnings.push(CompilerError::semantic_warning(
                    "unreachable code after return/break/continue/throw".to_string(),
                    Span::dummy(),
                ));
                return;
            }
            if self.stmt_always_terminates(s) {
                terminated = true;
            }
        }
    }

    /// Returns true if executing this statement always transfers control
    /// elsewhere (return, break, continue, throw, panic, infinite loop).
    fn stmt_always_terminates(&self, s: &Stmt) -> bool {
        match s {
            Stmt::Return(_)
            | Stmt::Break(_)
            | Stmt::Continue(_)
            | Stmt::Throw(_)
            | Stmt::Panic(_) => true,
            Stmt::If(i) => {
                // If there's an else and both branches terminate.
                if let Some(else_body) = &i.else_body {
                    self.block_terminates(&i.then_body)
                        && self.block_terminates(else_body)
                        && i.elif_chain.iter().all(|(_, b)| self.block_terminates(b))
                } else {
                    false
                }
            }
            Stmt::Loop(l) => {
                // A `loop` with no `break` in the body is infinite.
                !self.block_has_break(&l.body)
            }
            _ => false,
        }
    }

    fn block_terminates(&self, body: &[Stmt]) -> bool {
        body.iter().any(|s| self.stmt_always_terminates(s))
    }

    fn block_has_break(&self, body: &[Stmt]) -> bool {
        body.iter().any(|s| self.stmt_has_break(s))
    }

    fn stmt_has_break(&self, s: &Stmt) -> bool {
        match s {
            Stmt::Break(_) => true,
            Stmt::If(i) => {
                self.block_has_break(&i.then_body)
                    || i.elif_chain.iter().any(|(_, b)| self.block_has_break(b))
                    || i.else_body
                        .as_ref()
                        .map(|b| self.block_has_break(b))
                        .unwrap_or(false)
            }
            Stmt::While(_) | Stmt::Loop(_) => {
                // Nested loops have their own break; don't count.
                false
            }
            _ => false,
        }
    }
}

/// Collects all identifier names that are read (used) in a statement list.
struct UsageCollector {
    used: HashSet<String>,
}

impl UsageCollector {
    fn collect_stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Assign(a) => {
                self.collect_expr(&a.value);
            }
            Stmt::If(i) => {
                self.collect_expr(&i.condition);
                for s in &i.then_body {
                    self.collect_stmt(s);
                }
                for (cond, body) in &i.elif_chain {
                    self.collect_expr(cond);
                    for s in body {
                        self.collect_stmt(s);
                    }
                }
                if let Some(else_body) = &i.else_body {
                    for s in else_body {
                        self.collect_stmt(s);
                    }
                }
            }
            Stmt::While(w) => {
                self.collect_expr(&w.condition);
                for s in &w.body {
                    self.collect_stmt(s);
                }
            }
            Stmt::ForIn(f) => {
                self.collect_expr(&f.iterable);
                for s in &f.body {
                    self.collect_stmt(s);
                }
            }
            Stmt::Return(r) => {
                for v in &r.values {
                    self.collect_expr(v);
                }
            }
            Stmt::Println(p) => {
                for v in &p.args {
                    self.collect_expr(v);
                }
            }
            Stmt::Expr(e) => self.collect_expr(&e.expr),
            Stmt::ForRange(f) => {
                self.collect_expr(&f.from);
                self.collect_expr(&f.to);
                if let Some(step) = &f.step {
                    self.collect_expr(step);
                }
                for s in &f.body {
                    self.collect_stmt(s);
                }
                if let Some(else_body) = &f.else_body {
                    for s in else_body {
                        self.collect_stmt(s);
                    }
                }
            }
            Stmt::Loop(l) => {
                for s in &l.body {
                    self.collect_stmt(s);
                }
                if let Some(else_body) = &l.else_body {
                    for s in else_body {
                        self.collect_stmt(s);
                    }
                }
            }
            Stmt::Try(t) => {
                for s in &t.try_body {
                    self.collect_stmt(s);
                }
                if let Some(cb) = &t.catch_body {
                    for s in cb {
                        self.collect_stmt(s);
                    }
                }
                if let Some(fb) = &t.finally_body {
                    for s in fb {
                        self.collect_stmt(s);
                    }
                }
            }
            Stmt::With(w) => {
                self.collect_expr(&w.manager);
                for s in &w.body {
                    self.collect_stmt(s);
                }
            }
            Stmt::Match(m) => {
                self.collect_expr(&m.expr);
                for case in &m.cases {
                    // Pattern bindings introduce new names, not uses, so
                    // they are intentionally not collected here. Only the
                    // optional guard and the case body are walked.
                    if let Some(guard) = &case.guard {
                        self.collect_expr(guard);
                    }
                    for s in &case.body {
                        self.collect_stmt(s);
                    }
                }
                if let Some(else_case) = &m.else_case {
                    for s in else_case {
                        self.collect_stmt(s);
                    }
                }
            }
            _ => {}
        }
    }

    fn collect_expr(&mut self, e: &Expr) {
        match e {
            Expr::Identifier(id) => {
                self.used.insert(id.name.clone());
            }
            Expr::Binary(b) => {
                self.collect_expr(&b.left);
                self.collect_expr(&b.right);
            }
            Expr::Call(c) => {
                self.collect_expr(&c.callee);
                for a in &c.args {
                    self.collect_expr(a);
                }
            }
            Expr::MemberAccess(m) => self.collect_expr(&m.target),
            Expr::MethodCall(m) => {
                self.collect_expr(&m.receiver);
                for a in &m.args {
                    self.collect_expr(a);
                }
            }
            Expr::Index(i) => {
                self.collect_expr(&i.target);
                self.collect_expr(&i.index);
            }
            Expr::Slice(s) => {
                self.collect_expr(&s.target);
                if let Some(e) = &s.start {
                    self.collect_expr(e);
                }
                if let Some(e) = &s.end {
                    self.collect_expr(e);
                }
                if let Some(e) = &s.step {
                    self.collect_expr(e);
                }
            }
            Expr::List(l) => {
                for e in &l.elements {
                    self.collect_expr(e);
                }
            }
            Expr::Tuple(t) => {
                for e in &t.elements {
                    self.collect_expr(e);
                }
            }
            Expr::Dict(d) => {
                for (k, v) in &d.entries {
                    self.collect_expr(k);
                    self.collect_expr(v);
                }
            }
            _ => {}
        }
    }
}

impl Default for LinearityChecker {
    fn default() -> Self {
        Self::new()
    }
}
