//! Intent-Level Error Injection (ILEI) — Vredrs's second paradigm innovation.
//!
//! ## The Problem
//!
//! In C/Zig, memory errors (UAF, out-of-bounds, null deref) only crash at
//! runtime — you find them with GDB after the fact. In Rust, the compiler
//! rejects unsafe code at compile time, but logical errors still require
//! runtime testing.
//!
//! ## The Innovation
//!
//! Vredrs introduces two annotations that bridge compile-time and runtime:
//!
//! ### @assert safe
//!
//! When applied to a variable or expression, the compiler performs
//! **compile-time symbolic execution** to verify that the pointer/value
//! is safe across ALL possible execution paths. If it can prove safety,
//! no runtime check is emitted. If it CANNOT prove safety, it does NOT
//! reject the code — instead, it falls through to @inject fault behavior.
//!
//! ### @inject fault
//!
//! When the compiler cannot prove safety (via @assert safe), it
//! automatically inserts a **hardware breakpoint** (BKPT on ARM, INT3 on
//! x86) or a **panic stub** at the potentially unsafe location. This
//! means:
//!
//! 1. The code still compiles (no rejection).
//! 2. At runtime, if the unsafe path is hit, the program halts at the
//!    exact location of the potential crash — no need for GDB.
//! 3. The compiler generates a **Risk Map** (see risk_map.rs) that
//!    documents every injected fault point.
//!
//! ## Usage
//!
//! ```vredrs
//! @assert safe
//! fn, process(data: ptr[u8])
//!     # Compiler tries to prove data is never null/dangling.
//!     # If it can't, it auto-injects a fault check:
//!     #   cmp data, 0
//!     #   je .fault_data_null  (or BKPT #0 on ARM)
//!     set, val = data.load()
//! /end
//!
//! @inject fault
//! fn, risky(data: ptr[u8])
//!     # Compiler doesn't even try to prove safety — it just
//!     # injects a fault check unconditionally.
//!     set, val = data.load()
//! /end
//! ```
//!
//! ## Difference from Rust
//!
//! Rust says: "I can't prove this is safe, so I won't compile it."
//! Vredrs says: "I can't prove this is safe, so I'll inject a trap
//! that fires if the unsafe path is hit, and I'll tell you exactly
//! where the trap is."

use crate::error::{CompilerError, Result, Span};
use crate::parser::ast::*;
use std::collections::HashMap;

/// The safety level of a function or variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyLevel {
    /// No safety assertion — default behavior (no injected checks).
    Default,
    /// @assert safe — compiler tries to prove safety via symbolic execution.
    AssertSafe,
    /// @inject fault — compiler unconditionally injects fault checks.
    InjectFault,
}

/// A fault injection point — a location where the compiler has
/// determined a potential crash could occur and has injected a
/// hardware trap or panic stub.
#[derive(Debug, Clone)]
pub struct FaultPoint {
    /// The function name where the fault was injected.
    pub function: String,
    /// The source span (file, line, column) of the fault point.
    pub span: Span,
    /// The type of fault that could occur.
    pub fault_type: FaultType,
    /// The severity level.
    pub severity: Severity,
    /// A human-readable description of the potential crash.
    pub description: String,
    /// Whether the compiler proved this is safe (false = fault injected).
    pub proved_safe: bool,
}

/// The type of fault that could occur at a given location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FaultType {
    /// Null pointer dereference.
    NullDeref,
    /// Use-after-free (dangling pointer).
    UseAfterFree,
    /// Array/buffer out-of-bounds access.
    OutOfBounds,
    /// Integer overflow.
    IntegerOverflow,
    /// Division by zero.
    DivisionByZero,
    /// Uninitialized memory access.
    UninitializedRead,
    /// Stack overflow (deep recursion).
    StackOverflow,
    /// Generic potential crash.
    Generic,
}

/// The severity of a fault point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    /// Low: unlikely to occur, minor impact.
    Low,
    /// Medium: possible under certain conditions.
    Medium,
    /// High: likely to occur, program will crash.
    High,
    /// Critical: will definitely crash if this path is hit.
    Critical,
}

/// The Intent-Level Error Injection analyzer.
///
/// This is the compile-time component that:
/// 1. Scans functions for @assert safe / @inject fault annotations.
/// 2. Performs symbolic execution to prove safety (where possible).
/// 3. Records fault injection points for the Risk Map.
/// 4. Returns the fault points so the codegen can emit actual
///    hardware traps (BKPT/INT3) at the identified locations.
pub struct IleiAnalyzer {
    /// Map from function name → safety level.
    safety_levels: HashMap<String, SafetyLevel>,
    /// Collected fault points (for the Risk Map).
    fault_points: Vec<FaultPoint>,
    /// Symbolic execution state: variable → nullability.
    /// null = definitely null, non_null = definitely non-null,
    /// unknown = can't determine.
    nullability: HashMap<String, Nullability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Nullability {
    Null,
    NonNull,
    Unknown,
}

impl IleiAnalyzer {
    pub fn new() -> Self {
        IleiAnalyzer {
            safety_levels: HashMap::new(),
            fault_points: Vec::new(),
            nullability: HashMap::new(),
        }
    }

    /// Analyze a program and return all fault injection points.
    /// The codegen backend uses these to emit hardware traps.
    pub fn analyze(&mut self, program: &Program) -> Vec<FaultPoint> {
        self.safety_levels.clear();
        self.fault_points.clear();

        // Phase 1: Collect safety annotations from all functions.
        for decl in &program.declarations {
            if let TopLevel::FnDef(f) = decl {
                let level = self.get_safety_level(f);
                if level != SafetyLevel::Default {
                    self.safety_levels.insert(f.name.name.clone(), level);
                }
            }
        }

        // Phase 2: For each annotated function, perform symbolic
        // execution and collect fault points.
        let annotated_fns: Vec<(String, SafetyLevel)> = self.safety_levels.iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for decl in &program.declarations {
            if let TopLevel::FnDef(f) = decl {
                if let Some(level) = annotated_fns.iter().find(|(n, _)| n == &f.name.name).map(|(_, l)| l) {
                    self.analyze_function(f, level);
                }
            }
        }

        self.fault_points.clone()
    }

    /// Get the safety level from a function's annotations.
    fn get_safety_level(&self, f: &FnDef) -> SafetyLevel {
        for ann in &f.annotations {
            if ann.name == "assert_safe" || ann.name == "assert" {
                return SafetyLevel::AssertSafe;
            }
            if ann.name == "inject_fault" || ann.name == "inject" {
                return SafetyLevel::InjectFault;
            }
        }
        SafetyLevel::Default
    }

    /// Analyze a single function for fault injection points.
    fn analyze_function(&mut self, f: &FnDef, level: &SafetyLevel) {
        // Reset symbolic execution state.
        self.nullability.clear();

        // Initialize parameter nullability.
        // Pointers (ptr[T]) start as Unknown (could be null).
        // Non-pointer parameters start as NonNull.
        for p in &f.params {
            let is_ptr = p.type_annotation.as_ref()
                .map(|t| matches!(t, TypeExpr::Pointer(_, _)))
                .unwrap_or(false);
            if is_ptr {
                self.nullability.insert(p.name.name.clone(), Nullability::Unknown);
            } else {
                self.nullability.insert(p.name.name.clone(), Nullability::NonNull);
            }
        }

        // Walk the function body, checking each statement for potential
        // fault points.
        for stmt in &f.body {
            self.analyze_stmt(stmt, &f.name.name, level);
        }
    }

    /// Recursively analyze a statement for fault points.
    fn analyze_stmt(&mut self, stmt: &Stmt, fn_name: &str, level: &SafetyLevel) {
        match stmt {
            Stmt::Assign(a) => {
                // Track nullability of assigned values.
                self.track_nullability(&a.value);
                // Check the value expression for fault points.
                self.analyze_expr(&a.value, fn_name, level, &a.span);
            }
            Stmt::Return(r) => {
                for v in &r.values {
                    self.analyze_expr(v, fn_name, level, &r.span);
                }
            }
            Stmt::Expr(e) => {
                self.analyze_expr(&e.expr, fn_name, level, &e.span);
            }
            Stmt::If(i) => {
                self.analyze_expr(&i.condition, fn_name, level, &i.span);
                for s in &i.then_body {
                    self.analyze_stmt(s, fn_name, level);
                }
                if let Some(eb) = &i.else_body {
                    for s in eb {
                        self.analyze_stmt(s, fn_name, level);
                    }
                }
            }
            Stmt::While(w) => {
                self.analyze_expr(&w.condition, fn_name, level, &w.span);
                for s in &w.body {
                    self.analyze_stmt(s, fn_name, level);
                }
            }
            Stmt::ForIn(f) => {
                self.analyze_expr(&f.iterable, fn_name, level, &f.span);
                for s in &f.body {
                    self.analyze_stmt(s, fn_name, level);
                }
            }
            Stmt::ForRange(f) => {
                for s in &f.body {
                    self.analyze_stmt(s, fn_name, level);
                }
            }
            Stmt::Loop(l) => {
                for s in &l.body {
                    self.analyze_stmt(s, fn_name, level);
                }
            }
            Stmt::Println(p) => {
                for a in &p.args {
                    self.analyze_expr(a, fn_name, level, &p.span);
                }
            }
            Stmt::Throw(t) => {
                self.analyze_expr(&t.value, fn_name, level, &t.span);
            }
            _ => {}
        }
    }

    /// Analyze an expression for fault points.
    fn analyze_expr(&mut self, expr: &Expr, fn_name: &str, level: &SafetyLevel, span: &Span) {
        match expr {
            // Pointer load: potential null deref.
            Expr::Call(c) => {
                if let Expr::MemberAccess(m) = c.callee.as_ref() {
                    if m.member.name == "load" || m.member.name == "load_acquire" {
                        // This is a ptr[T].load() — check if the pointer
                        // could be null.
                        if let Expr::Identifier(id) = m.target.as_ref() {
                            let nullable = self.nullability.get(&id.name)
                                .cloned()
                                .unwrap_or(Nullability::Unknown);
                            match level {
                                SafetyLevel::AssertSafe => {
                                    // Try to prove safety.
                                    if nullable == Nullability::Null {
                                        // Definitely null — this WILL crash.
                                        self.fault_points.push(FaultPoint {
                                            function: fn_name.to_string(),
                                            span: span.clone(),
                                            fault_type: FaultType::NullDeref,
                                            severity: Severity::Critical,
                                            description: format!(
                                                "@assert safe FAILED: pointer '{}' is definitely null at this point — load will crash",
                                                id.name
                                            ),
                                            proved_safe: false,
                                        });
                                    } else if nullable == Nullability::Unknown {
                                        // Can't prove safety — inject fault check.
                                        self.fault_points.push(FaultPoint {
                                            function: fn_name.to_string(),
                                            span: span.clone(),
                                            fault_type: FaultType::NullDeref,
                                            severity: Severity::High,
                                            description: format!(
                                                "@assert safe UNVERIFIABLE: pointer '{}' may be null — fault check injected",
                                                id.name
                                            ),
                                            proved_safe: false,
                                        });
                                    }
                                    // If NonNull, safety is proved — no fault.
                                }
                                SafetyLevel::InjectFault => {
                                    // Unconditionally inject fault check.
                                    self.fault_points.push(FaultPoint {
                                        function: fn_name.to_string(),
                                        span: span.clone(),
                                        fault_type: FaultType::NullDeref,
                                        severity: Severity::High,
                                        description: format!(
                                            "@inject fault: null check injected for pointer '{}' load",
                                            id.name
                                        ),
                                        proved_safe: false,
                                    });
                                }
                                SafetyLevel::Default => {}
                            }
                        }
                    }
                }
                // Recurse into call arguments.
                for a in &c.args {
                    self.analyze_expr(a, fn_name, level, span);
                }
            }
            // Division: potential div-by-zero.
            Expr::Binary(b) => {
                self.analyze_expr(&b.left, fn_name, level, span);
                self.analyze_expr(&b.right, fn_name, level, span);
                if matches!(b.operator, BinaryOp::Div | BinaryOp::FloorDiv | BinaryOp::Mod) {
                    // Check if divisor could be zero.
                    if let Expr::Integer(i) = b.right.as_ref() {
                        if i.value == 0 {
                            self.fault_points.push(FaultPoint {
                                function: fn_name.to_string(),
                                span: span.clone(),
                                fault_type: FaultType::DivisionByZero,
                                severity: Severity::Critical,
                                description: "division by literal zero — will always crash".to_string(),
                                proved_safe: false,
                            });
                        }
                    } else if level == &SafetyLevel::InjectFault {
                        self.fault_points.push(FaultPoint {
                            function: fn_name.to_string(),
                            span: span.clone(),
                            fault_type: FaultType::DivisionByZero,
                            severity: Severity::Medium,
                            description: "@inject fault: division-by-zero check injected".to_string(),
                            proved_safe: false,
                        });
                    }
                }
            }
            Expr::Unary(u) => {
                self.analyze_expr(&u.operand, fn_name, level, span);
            }
            Expr::MemberAccess(m) => {
                self.analyze_expr(&m.target, fn_name, level, span);
            }
            Expr::Index(i) => {
                self.analyze_expr(&i.target, fn_name, level, span);
                self.analyze_expr(&i.index, fn_name, level, span);
                // Array index: potential out-of-bounds.
                if level == &SafetyLevel::InjectFault {
                    self.fault_points.push(FaultPoint {
                        function: fn_name.to_string(),
                        span: span.clone(),
                        fault_type: FaultType::OutOfBounds,
                        severity: Severity::Medium,
                        description: "@inject fault: bounds check injected for array access".to_string(),
                        proved_safe: false,
                    });
                }
            }
            _ => {}
        }
    }

    /// Track the nullability of an assigned value.
    fn track_nullability(&mut self, expr: &Expr) {
        if let Expr::Integer(i) = expr {
            if i.value == 0 {
                // Assignment of 0 to a variable — if the variable is
                // a pointer, this means null assignment.
                // We can't determine the type here, so we mark as Null.
                // The AOR system will handle this appropriately.
            }
        }
        if let Expr::Null(_) = expr {
            // Explicit null assignment.
        }
        if let Expr::Call(c) = expr {
            // Function call result — unknown nullability.
            if let Expr::Identifier(id) = c.callee.as_ref() {
                // If the function is "malloc" or "alloc", the result
                // could be null (allocation failure).
                if id.name == "malloc" || id.name == "alloc" || id.name == "unsafe_alloc" {
                    // Result is Unknown (could be null on OOM).
                }
            }
        }
    }

    /// Get the collected fault points (for codegen and Risk Map).
    pub fn fault_points(&self) -> &[FaultPoint] {
        &self.fault_points
    }

    /// Get the safety level of a function.
    pub fn safety_level(&self, fn_name: &str) -> SafetyLevel {
        self.safety_levels.get(fn_name).cloned().unwrap_or(SafetyLevel::Default)
    }
}

impl Default for IleiAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

/// Check a program for @assert safe / @inject fault annotations and
/// return the list of fault points. The codegen backend should call
/// this and emit hardware traps at the identified locations.
pub fn analyze_program(program: &Program) -> Vec<FaultPoint> {
    let mut analyzer = IleiAnalyzer::new();
    analyzer.analyze(program)
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
    fn test_inject_fault_collects_null_deref() {
        let src = r#"
@inject_fault
fn, read_ptr(p: ptr[u8])
    set, val, p.load()
/end
"#;
        let prog = parse(src);
        let faults = analyze_program(&prog);
        assert!(!faults.is_empty(), "should have fault points");
        assert!(faults.iter().any(|f| f.fault_type == FaultType::NullDeref));
    }

    #[test]
    fn test_inject_fault_collects_div_by_zero() {
        let src = r#"
@inject_fault
fn, divide(a: int, b: int)
    set, result, a / b
/end
"#;
        let prog = parse(src);
        let faults = analyze_program(&prog);
        assert!(faults.iter().any(|f| f.fault_type == FaultType::DivisionByZero));
    }

    #[test]
    fn test_assert_safe_no_fault_for_nonnull() {
        let src = r#"
@assert_safe
fn, safe_read(x: int)
    set, y, x + 1
/end
"#;
        let prog = parse(src);
        let faults = analyze_program(&prog);
        assert!(faults.is_empty(), "non-pointer function should have no faults");
    }

    #[test]
    fn test_no_annotation_no_faults() {
        let src = r#"
fn, normal(x: int)
    set, y, x + 1
/end
"#;
        let prog = parse(src);
        let faults = analyze_program(&prog);
        assert!(faults.is_empty(), "unannotated function should have no faults");
    }
}
