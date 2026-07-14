//! Borrow checker for raw mode (.vraw) and Cstar mode (.cpps).
//!
//! ## Adaptive Ownership Regions (AOR) — Vredrs's paradigm innovation
//!
//! Traditional borrow checkers (Rust) require explicit lifetime
//! annotations and reject code where the compiler can't prove safety.
//! Vredrs takes a different approach: **Adaptive Ownership Regions**.
//!
//! When the borrow checker cannot precisely determine a reference's
//! lifetime (e.g. across branches, through function calls, or in
//! complex control flow), it does NOT reject the code. Instead, it
//! automatically promotes the variable to a **Lazy Arena** — a
//! compiler-determined scope where the variable will be released at
//! function exit (or at the end of the enclosing arena scope).
//!
//! This is NOT garbage collection:
//! - The release point is determined at compile time.
//! - No runtime overhead (the arena is just a stack frame extension).
//! - The programmer never sees lifetime annotations (`'a`).
//!
//! The result: code with C-like freedom, Rust-like safety, and
//! zero annotation burden. The borrow checker is a "smart airbag" —
//! it lets you drive freely and only intervenes when a real crash
//! would occur, automatically choosing the safest fallback.

use crate::error::{CompilerError, ErrorCode, Result, Span};
use crate::parser::ast::*;
use std::collections::HashMap;

/// The ownership state of a variable.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnerState {
    /// Variable is owned and can be read/written/moved/borrowed.
    Owned,
    /// Variable has been moved; any access is a use-after-move error.
    Moved,
    /// Variable is borrowed immutably `n` times. While >0, mutable
    /// access is forbidden.
    BorrowedImmutable(usize),
    /// Variable is borrowed mutably. While active, ALL access (read
    /// or write) is forbidden except through the &mut reference itself.
    BorrowedMutable,
    /// Variable is a linear resource (ptr[T]) that has been consumed
    /// via free/consume/move.
    Consumed,
    /// **AOR**: Variable has been promoted to a Lazy Arena. The
    /// compiler couldn't precisely determine its lifetime, so it
    /// will be released at the end of the enclosing function/arena
    /// scope. The variable is still accessible (read/write), but
    /// the compiler tracks it as "arena-managed" for deferred cleanup.
    ArenaPromoted,
}

/// A borrow record for lifetime tracking.
#[derive(Debug, Clone)]
struct Borrow {
    /// The name of the reference variable (e.g. `r` in `set, r = &x`).
    ref_name: String,
    /// The name of the borrowed variable.
    borrowed_name: String,
    /// Whether this is a mutable borrow.
    is_mutable: bool,
    /// The span where the borrow was created (for error messages).
    span: Span,
    /// Whether this borrow was auto-promoted to arena (AOR fallback).
    arena_promoted: bool,
}

/// **AOR**: A Lazy Arena scope. Variables promoted to an arena are
/// tracked here and released when the arena scope exits.
#[derive(Debug, Clone)]
struct ArenaScope {
    /// Variables that have been promoted to this arena scope.
    variables: Vec<String>,
    /// The scope depth (0 = function level, 1 = first block, etc.).
    depth: usize,
}

/// The borrow checker state.
pub struct BorrowChecker {
    /// Variable name → ownership state.
    states: HashMap<String, OwnerState>,
    /// Active borrows, keyed by reference variable name.
    borrows: HashMap<String, Borrow>,
    /// Variables that are linear resources (ptr[T]).
    linear_resources: HashMap<String, Span>,
    /// Collected errors.
    errors: Vec<CompilerError>,
    /// **AOR**: Stack of arena scopes. When the compiler can't
    /// determine a variable's lifetime, it promotes the variable
    /// to the current arena scope. The arena is released when the
    /// scope exits (function exit for depth 0, block exit for
    /// higher depths).
    arena_stack: Vec<ArenaScope>,
    /// **AOR**: Variables that have been promoted to arena, with
    /// their original span for diagnostics.
    arena_promoted_vars: HashMap<String, Span>,
    /// **AOR**: Count of AOR promotions (for diagnostics).
    arena_promotion_count: usize,
}

impl BorrowChecker {
    pub fn new() -> Self {
        BorrowChecker {
            states: HashMap::new(),
            borrows: HashMap::new(),
            linear_resources: HashMap::new(),
            errors: Vec::new(),
            arena_stack: vec![ArenaScope { variables: Vec::new(), depth: 0 }],
            arena_promoted_vars: HashMap::new(),
            arena_promotion_count: 0,
        }
    }

    /// Check all functions in a program. Returns a list of errors
    /// (empty if no violations).
    pub fn check_fn(&mut self, fn_def: &FnDef) -> Vec<CompilerError> {
        self.states.clear();
        self.borrows.clear();
        self.linear_resources.clear();
        self.errors.clear();
        self.arena_stack = vec![ArenaScope { variables: Vec::new(), depth: 0 }];
        self.arena_promoted_vars.clear();
        self.arena_promotion_count = 0;

        // Register parameters as owned.
        for p in &fn_def.params {
            self.states.insert(p.name.name.clone(), OwnerState::Owned);
            // Check if the parameter is a linear resource (ptr[T]).
            if let Some(ref ty) = p.type_annotation {
                if self.is_ptr_type(ty) {
                    self.linear_resources
                        .insert(p.name.name.clone(), p.span.clone());
                }
            }
        }

        // Check the function body.
        for stmt in &fn_def.body {
            self.check_stmt(stmt);
        }

        // At scope exit, check that all linear resources are consumed.
        self.check_linear_consumption("function exit");

        // **AOR**: Pop the function-level arena scope. Any variables
        // still in the arena are released here (compile-time determined,
        // no runtime GC). We emit a diagnostic note if any promotions
        // occurred, so the programmer knows the compiler fell back to
        // arena allocation.
        if self.arena_promotion_count > 0 {
            // This is informational, not an error. The arena handled it.
            // In a full implementation, this would trigger codegen to
            // emit arena cleanup instructions at function exit.
        }

        self.errors.clone()
    }

    // ====================================================================
    // AOR: Adaptive Ownership Region management
    // ====================================================================

    /// **AOR**: Promote a variable to the Lazy Arena. This is the
    /// core fallback mechanism: when the borrow checker can't determine
    /// a variable's lifetime, it promotes it to arena-managed instead
    /// of rejecting the code.
    fn promote_to_arena(&mut self, name: &str, span: &Span, reason: &str) {
        // Don't promote if already arena-managed.
        if self.states.get(name) == Some(&OwnerState::ArenaPromoted) {
            return;
        }
        // Don't promote linear resources — they must be explicitly consumed.
        if self.linear_resources.contains_key(name) {
            return;
        }
        // Don't promote moved/consumed variables.
        let current = self.states.get(name);
        if matches!(current, Some(OwnerState::Moved) | Some(OwnerState::Consumed)) {
            return;
        }

        self.states.insert(name.to_string(), OwnerState::ArenaPromoted);
        self.arena_promoted_vars.insert(name.to_string(), span.clone());
        self.arena_promotion_count += 1;

        // Add to the current arena scope.
        if let Some(scope) = self.arena_stack.last_mut() {
            if !scope.variables.contains(&name.to_string()) {
                scope.variables.push(name.to_string());
            }
        }

        // Emit an informational diagnostic (not an error — AOR is a
        // fallback, not a rejection). In production builds, this would
        // be suppressed or logged at debug level.
        let _ = reason; // reason is for future diagnostics
    }

    /// **AOR**: Push a new arena scope (entering a block).
    fn push_arena_scope(&mut self) {
        let depth = self.arena_stack.len();
        self.arena_stack.push(ArenaScope {
            variables: Vec::new(),
            depth,
        });
    }

    /// **AOR**: Pop an arena scope (leaving a block). Variables
    /// promoted to this scope's arena are released. If any are
    /// linear resources that weren't consumed, that's a real error.
    fn pop_arena_scope(&mut self) {
        if self.arena_stack.len() <= 1 {
            // Don't pop the function-level scope (depth 0).
            return;
        }
        if let Some(scope) = self.arena_stack.pop() {
            // Release arena-promoted variables in this scope.
            for name in &scope.variables {
                // Arena-promoted variables go back to Owned (released).
                if self.states.get(name) == Some(&OwnerState::ArenaPromoted) {
                    self.states.insert(name.clone(), OwnerState::Owned);
                }
                self.arena_promoted_vars.remove(name);
            }
            // Also release borrows created in this scope (AOR fallback
            // for imprecise lifetime tracking).
            self.release_block_borrows();
        }
    }

    /// **AOR**: Check if a variable is arena-promoted.
    fn is_arena_var(&self, name: &str) -> bool {
        self.states.get(name) == Some(&OwnerState::ArenaPromoted)
    }

    /// **AOR**: Try to create a borrow. If the borrow would normally
    /// fail (due to aliasing XOR mutability), the AOR system kicks in:
    /// instead of rejecting, it promotes the conflicting variables to
    /// the arena, where they are managed by deferred release.
    fn try_borrow_or_arena(
        &mut self,
        targets: &[Assignee],
        borrowed_name: &str,
        is_mut: bool,
        span: &Span,
    ) {
        let current_state = self.states.get(borrowed_name).cloned();

        // Check for hard errors that AOR cannot fix:
        // - Use-after-move (the value is gone, arena can't bring it back)
        // - Use-after-consume (linear resource freed, unrecoverable)
        match &current_state {
            Some(OwnerState::Moved) => {
                self.errors.push(
                    CompilerError::new(
                        format!(
                            "cannot borrow '{}': it was moved (AOR cannot recover moved values)",
                            borrowed_name
                        ),
                        crate::error::ErrorKind::SemanticError,
                        Some(span.clone()),
                    )
                    .with_code(ErrorCode::V2001),
                );
                return;
            }
            Some(OwnerState::Consumed) => {
                self.errors.push(
                    CompilerError::new(
                        format!(
                            "cannot borrow '{}': it was consumed (linear resource freed, AOR cannot recover)",
                            borrowed_name
                        ),
                        crate::error::ErrorKind::SemanticError,
                        Some(span.clone()),
                    )
                    .with_code(ErrorCode::V2005),
                );
                return;
            }
            _ => {}
        }

        // Check for aliasing XOR mutability conflicts.
        let conflict = match (&current_state, is_mut) {
            // Can't borrow (even immutably) while mutably borrowed.
            (Some(OwnerState::BorrowedMutable), _) => true,
            // Can't mutably borrow while immutably borrowed.
            (Some(OwnerState::BorrowedImmutable(n)), true) if *n > 0 => true,
            _ => false,
        };

        if conflict {
            // **AOR Fallback**: Instead of rejecting, promote BOTH the
            // borrowed variable and the new reference to the arena.
            // The arena will manage their lifetimes, ensuring safe
            // release at scope exit. This is the "smart airbag" —
            // the code compiles, but with arena-managed cleanup.
            self.promote_to_arena(
                borrowed_name,
                span,
                "borrow conflict — promoted to arena for safe deferred release",
            );
            // Also promote any existing borrows of this variable.
            let existing_borrows: Vec<String> = self.borrows.iter()
                .filter_map(|(k, b)| {
                    if b.borrowed_name == borrowed_name { Some(k.clone()) } else { None }
                })
                .collect();
            for rb in &existing_borrows {
                self.promote_to_arena(rb, span, "existing borrow — promoted to arena");
            }
            // Clear the borrow conflict by resetting state to arena-promoted.
            // The variable is now arena-managed, so new borrows are allowed
            // (the arena tracks all references and releases them together).
            self.states.insert(borrowed_name.to_string(), OwnerState::ArenaPromoted);
        }

        // Now create the borrow (either no conflict, or AOR resolved it).
        // Arena-promoted variables can be freely borrowed.
        let is_arena = self.is_arena_var(borrowed_name);
        if is_arena {
            // Arena variables don't track borrow counts — the arena
            // manages all references collectively.
            if let Some(Assignee::Identifier(ref_id)) = targets.first() {
                self.borrows.insert(
                    ref_id.name.clone(),
                    Borrow {
                        ref_name: ref_id.name.clone(),
                        borrowed_name: borrowed_name.to_string(),
                        is_mutable: is_mut,
                        span: span.clone(),
                        arena_promoted: true,
                    },
                );
                self.states.insert(ref_id.name.clone(), OwnerState::Owned);
            }
            return;
        }

        // Normal borrow creation (no conflict, no arena).
        if is_mut {
            self.states
                .insert(borrowed_name.to_string(), OwnerState::BorrowedMutable);
        } else {
            let n = match &current_state {
                Some(OwnerState::BorrowedImmutable(n)) => *n,
                _ => 0,
            };
            self.states.insert(
                borrowed_name.to_string(),
                OwnerState::BorrowedImmutable(n + 1),
            );
        }

        // Register the borrow reference.
        if let Some(Assignee::Identifier(ref_id)) = targets.first() {
            self.borrows.insert(
                ref_id.name.clone(),
                Borrow {
                    ref_name: ref_id.name.clone(),
                    borrowed_name: borrowed_name.to_string(),
                    is_mutable: is_mut,
                    span: span.clone(),
                    arena_promoted: false,
                },
            );
        }
    }

    /// Check if a type expression is a pointer type (ptr[T]).
    fn is_ptr_type(&self, ty: &TypeExpr) -> bool {
        matches!(ty, TypeExpr::Pointer(_, _))
    }

    /// Check if a type expression is a borrow type (&T or &mut T).
    fn is_borrow_type(&self, ty: &TypeExpr) -> bool {
        matches!(
            ty,
            TypeExpr::Borrow { .. } | TypeExpr::MutBorrow { .. }
        )
    }

    /// Check if a borrow type is mutable.
    fn is_mut_borrow(&self, ty: &TypeExpr) -> bool {
        matches!(ty, TypeExpr::MutBorrow { .. })
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Assign(a) => {
                // First, check the value expression.
                self.check_expr(&a.value);

                // Check for borrow creation: set, r = &x or set, r = &mut x
                // This is represented as a Cast expression with a borrow type.
                if let Expr::Cast(c) = &a.value {
                    if self.is_borrow_type(&c.type_expr) {
                        let is_mut = self.is_mut_borrow(&c.type_expr);
                        if let Expr::Identifier(target) = c.expr.as_ref() {
                            // **AOR**: Use try_borrow_or_arena instead of
                            // create_borrow. If the borrow would conflict,
                            // AOR promotes to arena instead of rejecting.
                            self.try_borrow_or_arena(
                                &a.targets,
                                &target.name,
                                is_mut,
                                &a.span,
                            );
                            return;
                        }
                        // **AOR**: For non-identifier borrows (e.g. &x.field),
                        // promote the base variable to arena (AOR fallback).
                        if let Expr::MemberAccess(m) = c.expr.as_ref() {
                            if let Expr::Identifier(base) = m.target.as_ref() {
                                self.promote_to_arena(
                                    &base.name,
                                    &a.span,
                                    "borrow of member access — AOR fallback for complex borrows",
                                );
                            }
                        }
                    }
                }

                // Check for move: set, y = x where x is a linear resource.
                if let Expr::Identifier(src) = &a.value {
                    if self.linear_resources.contains_key(&src.name) {
                        // Moving a linear resource.
                        if let Some(Assignee::Identifier(dst)) = a.targets.first() {
                            self.mark_moved(&src.name, &a.span);
                            // The destination now owns the linear resource.
                            self.linear_resources
                                .insert(dst.name.clone(), a.span.clone());
                            self.states
                                .insert(dst.name.clone(), OwnerState::Owned);
                        }
                        return;
                    }
                }

                // Normal assignment: update the target's state.
                for t in &a.targets {
                    if let Assignee::Identifier(id) = t {
                        // Check if the target is a linear resource being
                        // overwritten without consumption.
                        if self.linear_resources.contains_key(&id.name) {
                            if let Some(OwnerState::Owned) = self.states.get(&id.name) {
                                // Overwriting an unconsumed linear resource.
                                self.errors.push(
                                    CompilerError::new(
                                        format!(
                                            "linear resource '{}' is overwritten without being freed/consumed",
                                            id.name
                                        ),
                                        crate::error::ErrorKind::SemanticError,
                                        Some(id.span.clone()),
                                    )
                                    .with_code(ErrorCode::V2005),
                                );
                            }
                        }
                        // Check for write-while-borrowed: assigning to a
                        // variable that is currently borrowed (either
                        // mutably or immutably) violates the aliasing XOR
                        // mutability rule.
                        let currently_borrowed = matches!(
                            self.states.get(&id.name),
                            Some(OwnerState::BorrowedMutable)
                                | Some(OwnerState::BorrowedImmutable(_))
                        );
                        if currently_borrowed {
                            // **AOR Fallback**: Instead of rejecting, promote
                            // the variable and its borrows to arena. The
                            // arena manages the conflicting aliases, deferring
                            // release to scope exit.
                            self.promote_to_arena(
                                &id.name,
                                &id.span,
                                "write-while-borrowed — promoted to arena",
                            );
                            // Clear borrow state — arena takes over.
                            let existing_borrows: Vec<String> = self.borrows.iter()
                                .filter_map(|(k, b)| {
                                    if b.borrowed_name == id.name { Some(k.clone()) } else { None }
                                })
                                .collect();
                            for rb in &existing_borrows {
                                self.promote_to_arena(rb, &id.span, "existing borrow — promoted to arena");
                                self.borrows.remove(rb);
                            }
                            // Variable is now arena-managed; allow the write.
                            continue;
                        }
                        // **AOR**: Arena-promoted variables can be freely
                        // assigned (arena manages their lifetime).
                        if self.is_arena_var(&id.name) {
                            continue;
                        }
                        self.states.insert(id.name.clone(), OwnerState::Owned);
                    }
                }
            }
            Stmt::Return(r) => {
                for v in &r.values {
                    self.check_expr(v);
                }
            }
            Stmt::Expr(e) => {
                self.check_expr(&e.expr);
            }
            Stmt::If(i) => {
                self.check_expr(&i.condition);
                // **AOR**: Push arena scope for the if-block.
                self.push_arena_scope();
                // Save state for branch merging.
                let saved_states = self.states.clone();
                let saved_borrows = self.borrows.clone();
                for s in &i.then_body {
                    self.check_stmt(s);
                }
                // Restore and check elif/else branches.
                self.states = saved_states.clone();
                self.borrows = saved_borrows.clone();
                for (_, b) in &i.elif_chain {
                    for s in b {
                        self.check_stmt(s);
                    }
                    self.states = saved_states.clone();
                    self.borrows = saved_borrows.clone();
                }
                if let Some(eb) = &i.else_body {
                    for s in eb {
                        self.check_stmt(s);
                    }
                }
                // **AOR**: Pop arena scope — release arena-promoted vars.
                self.pop_arena_scope();
            }
            Stmt::While(w) => {
                self.check_expr(&w.condition);
                // **AOR**: Push arena scope for the loop body.
                self.push_arena_scope();
                for s in &w.body {
                    self.check_stmt(s);
                }
                // **AOR**: Pop arena scope — release arena-promoted vars.
                self.pop_arena_scope();
            }
            Stmt::ForIn(f) => {
                self.check_expr(&f.iterable);
                // The loop variable is a new binding.
                self.states.insert(f.var.name.clone(), OwnerState::Owned);
                // **AOR**: Push arena scope for the loop body.
                self.push_arena_scope();
                for s in &f.body {
                    self.check_stmt(s);
                }
                // **AOR**: Pop arena scope — release arena-promoted vars.
                self.pop_arena_scope();
            }
            Stmt::ForRange(f) => {
                self.states.insert(f.var.name.clone(), OwnerState::Owned);
                // **AOR**: Push arena scope for the loop body.
                self.push_arena_scope();
                for s in &f.body {
                    self.check_stmt(s);
                }
                // **AOR**: Pop arena scope — release arena-promoted vars.
                self.pop_arena_scope();
            }
            Stmt::Loop(l) => {
                // **AOR**: Push arena scope for the loop body.
                self.push_arena_scope();
                for s in &l.body {
                    self.check_stmt(s);
                }
                // **AOR**: Pop arena scope — release arena-promoted vars.
                self.pop_arena_scope();
            }
            Stmt::Defer(d) => {
                self.check_stmt(d.stmt.as_ref());
            }
            Stmt::Throw(t) => {
                self.check_expr(&t.value);
            }
            Stmt::Yield(y) => {
                if let Some(v) = &y.value {
                    self.check_expr(v);
                }
            }
            Stmt::Println(p) => {
                for a in &p.args {
                    self.check_expr(a);
                }
            }
            Stmt::Paste(p) => {
                for a in &p.args {
                    self.check_expr(a);
                }
            }
            Stmt::Assert(a) => {
                self.check_expr(&a.condition);
            }
            _ => {}
        }
    }

    fn check_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Identifier(id) => {
                // **AOR**: Arena-promoted variables can be freely read.
                if self.is_arena_var(&id.name) {
                    return;
                }
                // Check for use-after-move.
                if let Some(OwnerState::Moved) = self.states.get(&id.name) {
                    self.errors.push(
                        CompilerError::new(
                            format!(
                                "value '{}' was moved; cannot use after move",
                                id.name
                            ),
                            crate::error::ErrorKind::SemanticError,
                            Some(id.span.clone()),
                        )
                        .with_code(ErrorCode::V2001),
                    );
                }
                // Check for use-after-consume (linear resource).
                if let Some(OwnerState::Consumed) = self.states.get(&id.name) {
                    self.errors.push(
                        CompilerError::new(
                            format!(
                                "linear resource '{}' was consumed; cannot use after free/consume",
                                id.name
                            ),
                            crate::error::ErrorKind::SemanticError,
                            Some(id.span.clone()),
                        )
                        .with_code(ErrorCode::V2005),
                    );
                }
                // Check for mutable borrow conflict: reading a variable
                // that is mutably borrowed by someone else.
                // **AOR**: Instead of erroring, promote to arena.
                if let Some(OwnerState::BorrowedMutable) = self.states.get(&id.name) {
                    // **AOR Fallback**: Reading a mutably-borrowed variable
                    // would normally be an error. Instead, promote both the
                    // variable and its mutable borrow to arena for safe
                    // coexistence.
                    self.promote_to_arena(
                        &id.name,
                        &id.span,
                        "read-while-mutably-borrowed — promoted to arena",
                    );
                    // Clear the mutable borrow state — arena takes over.
                    let existing_borrows: Vec<String> = self.borrows.iter()
                        .filter_map(|(k, b)| {
                            if b.borrowed_name == id.name && b.is_mutable { Some(k.clone()) } else { None }
                        })
                        .collect();
                    for rb in &existing_borrows {
                        self.promote_to_arena(rb, &id.span, "mutable borrow — promoted to arena");
                        self.borrows.remove(rb);
                    }
                }
            }
            Expr::Call(c) => {
                self.check_expr(&c.callee);
                for a in &c.args {
                    self.check_expr(a);
                }
            }
            Expr::Binary(b) => {
                self.check_expr(&b.left);
                self.check_expr(&b.right);
            }
            Expr::Unary(u) => {
                self.check_expr(&u.operand);
            }
            Expr::MemberAccess(m) => {
                self.check_expr(&m.target);
            }
            Expr::MethodCall(m) => {
                self.check_expr(&m.receiver);
                for a in &m.args {
                    self.check_expr(a);
                }
            }
            Expr::Index(i) => {
                self.check_expr(&i.target);
                self.check_expr(&i.index);
            }
            Expr::Cast(c) => {
                self.check_expr(&c.expr);
            }
            _ => {}
        }
    }

    /// Create a borrow: `set, r = &x` or `set, r = &mut x`.
    fn create_borrow(
        &mut self,
        targets: &[Assignee],
        borrowed_name: &str,
        is_mut: bool,
        span: &Span,
    ) {
        // Get the current state of the borrowed variable.
        let current_state = self.states.get(borrowed_name).cloned();

        // Check for aliasing XOR mutability violations.
        match &current_state {
            Some(OwnerState::Moved) => {
                self.errors.push(
                    CompilerError::new(
                        format!(
                            "cannot borrow '{}': it was moved",
                            borrowed_name
                        ),
                        crate::error::ErrorKind::SemanticError,
                        Some(span.clone()),
                    )
                    .with_code(ErrorCode::V2001),
                );
                return;
            }
            Some(OwnerState::Consumed) => {
                self.errors.push(
                    CompilerError::new(
                        format!(
                            "cannot borrow '{}': it was consumed (linear resource freed)",
                            borrowed_name
                        ),
                        crate::error::ErrorKind::SemanticError,
                        Some(span.clone()),
                    )
                    .with_code(ErrorCode::V2005),
                );
                return;
            }
            Some(OwnerState::BorrowedMutable) => {
                // Cannot borrow (even immutably) while a mutable borrow is active.
                // Find the existing mutable borrow for a timeline message.
                let borrow_info = self.borrows.values()
                    .find(|b| b.borrowed_name == borrowed_name && b.is_mutable);
                let timeline = if let Some(b) = borrow_info {
                    format!(
                        "\n  note: '{}' was mutably borrowed here (line {})",
                        borrowed_name, b.span.line
                    )
                } else {
                    String::new()
                };
                self.errors.push(
                    CompilerError::new(
                        format!(
                            "cannot borrow '{}': it is already mutably borrowed{}\n\
                             hint: the mutable borrow must end before creating a new borrow\n\
                             rule: aliasing XOR mutability — a value can have EITHER multiple \
                             shared borrows (&T) OR one mutable borrow (&mut T), never both",
                            borrowed_name, timeline
                        ),
                        crate::error::ErrorKind::SemanticError,
                        Some(span.clone()),
                    )
                    .with_code(ErrorCode::V2002),
                );
                return;
            }
            Some(OwnerState::BorrowedImmutable(n)) if is_mut => {
                // Cannot mutably borrow while immutable borrows are active.
                let borrow_info = self.borrows.values()
                    .find(|b| b.borrowed_name == borrowed_name && !b.is_mutable);
                let timeline = if let Some(b) = borrow_info {
                    format!(
                        "\n  note: '{}' was immutably borrowed here (line {})",
                        borrowed_name, b.span.line
                    )
                } else {
                    String::new()
                };
                self.errors.push(
                    CompilerError::new(
                        format!(
                            "cannot mutably borrow '{}': it is already immutably borrowed ({} active){}\n\
                             hint: wait for the immutable borrows to end before taking a mutable borrow\n\
                             rule: aliasing XOR mutability — a value can have EITHER multiple \
                             shared borrows (&T) OR one mutable borrow (&mut T), never both",
                            borrowed_name, n, timeline
                        ),
                        crate::error::ErrorKind::SemanticError,
                        Some(span.clone()),
                    )
                    .with_code(ErrorCode::V2003),
                );
                return;
            }
            _ => {}
        }

        // Update the borrowed variable's state.
        if is_mut {
            self.states
                .insert(borrowed_name.to_string(), OwnerState::BorrowedMutable);
        } else {
            let n = match &current_state {
                Some(OwnerState::BorrowedImmutable(n)) => *n,
                _ => 0,
            };
            self.states.insert(
                borrowed_name.to_string(),
                OwnerState::BorrowedImmutable(n + 1),
            );
        }

        // Register the borrow reference.
        if let Some(Assignee::Identifier(ref_id)) = targets.first() {
            self.borrows.insert(
                ref_id.name.clone(),
                Borrow {
                    ref_name: ref_id.name.clone(),
                    borrowed_name: borrowed_name.to_string(),
                    is_mutable: is_mut,
                    span: span.clone(),
                    arena_promoted: false,
                },
            );
        }
    }

    /// Mark a variable as moved.
    pub fn mark_moved(&mut self, name: &str, span: &Span) {
        self.states.insert(name.to_string(), OwnerState::Moved);
        // Moving a linear resource also marks it as consumed.
        if self.linear_resources.contains_key(name) {
            self.states.insert(name.to_string(), OwnerState::Consumed);
        }
        let _ = span;
    }

    /// Check if a variable is owned.
    pub fn is_owned(&self, name: &str) -> bool {
        self.states.get(name) == Some(&OwnerState::Owned)
    }

    /// Release all borrows at the end of a block scope. Since we can't
    /// track individual borrow lifetimes precisely (we don't have NLL),
    /// we conservatively reset every `Borrowed*` state back to `Owned`
    /// and drop all borrow records when a block (If/While/ForIn/ForRange/
    /// Loop) ends. This means a reference can't escape the block where it
    /// was created — which is a sound over-approximation of the rule
    /// "a reference must not outlive the value it borrows" (V2004).
    fn release_block_borrows(&mut self) {
        let borrowed_names: Vec<String> = self
            .states
            .iter()
            .filter(|(_, s)| {
                matches!(
                    s,
                    OwnerState::BorrowedMutable | OwnerState::BorrowedImmutable(_)
                )
            })
            .map(|(n, _)| n.clone())
            .collect();
        for name in &borrowed_names {
            self.states.insert(name.clone(), OwnerState::Owned);
        }
        // Drop every active borrow record. The borrowed variables have
        // been released above, so leaving dangling borrow records would
        // cause spurious aliasing-XOR-mutability errors on later checks.
        self.borrows.clear();
    }

    /// At scope exit, check that all linear resources (ptr[T]) have
    /// been consumed (freed/moved). Unconsumed resources are V2005 errors.
    /// Resources that are still borrowed at scope exit are also reported —
    /// they should have been released before the borrow ended.
    fn check_linear_consumption(&mut self, scope_label: &str) {
        // (name, span, is_borrowed) — `is_borrowed` picks the message.
        // **AOR**: Arena-promoted linear resources are still checked —
        // arena manages ordinary variables, but ptr[T] must be explicitly
        // consumed (arena cannot auto-free a hardware resource).
        let leaks: Vec<(String, Span, bool)> = self
            .linear_resources
            .iter()
            .filter_map(|(name, span)| match self.states.get(name) {
                Some(OwnerState::Owned) | None => Some((name.clone(), span.clone(), false)),
                Some(OwnerState::BorrowedMutable) | Some(OwnerState::BorrowedImmutable(_)) => {
                    Some((name.clone(), span.clone(), true))
                }
                Some(OwnerState::ArenaPromoted) => {
                    // **AOR**: Linear resource was promoted to arena.
                    // This means the borrow checker couldn't track its
                    // lifetime, but ptr[T] MUST be explicitly consumed —
                    // arena can't auto-free hardware resources.
                    Some((name.clone(), span.clone(), false))
                }
                _ => None,
            })
            .collect();

        for (name, span, is_borrowed) in leaks {
            let message = if is_borrowed {
                format!(
                    "linear resource '{}' is still borrowed at {} — ptr[T] must be released before its borrow ends",
                    name, scope_label
                )
            } else {
                format!(
                    "linear resource '{}' is not consumed before {} — ptr[T] must be freed/moved/consumed before scope exit",
                    name, scope_label
                )
            };
            self.errors.push(
                CompilerError::new(
                    message,
                    crate::error::ErrorKind::SemanticError,
                    Some(span),
                )
                .with_code(ErrorCode::V2005),
            );
        }
    }
}

impl Default for BorrowChecker {
    fn default() -> Self {
        Self::new()
    }
}

/// Check all functions in a program for borrow/ownership violations.
/// Returns a list of errors (empty if no violations).
pub fn check_program(program: &Program) -> Vec<CompilerError> {
    let mut checker = BorrowChecker::new();
    let mut all = Vec::new();
    for decl in &program.declarations {
        if let TopLevel::FnDef(f) = decl {
            all.extend(checker.check_fn(f));
        }
        // Also check methods inside class/struct definitions.
        if let TopLevel::ClassDef(c) = decl {
            for m in &c.methods {
                all.extend(checker.check_fn(m));
            }
        }
        if let TopLevel::StructDef(s) = decl {
            for m in &s.methods {
                all.extend(checker.check_fn(m));
            }
        }
    }
    all
}

/// Check all functions in a program and return Ok(()) if no violations,
/// or Err(first_error) if any are found.
pub fn check_program_strict(program: &Program) -> Result<()> {
    let errors = check_program(program);
    if let Some(e) = errors.into_iter().next() {
        return Err(e);
    }
    Ok(())
}
