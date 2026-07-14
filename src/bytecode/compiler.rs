//! AST → bytecode compiler.
//!
//! Compiles a Vredrs AST into a bytecode `Module`. The compiler covers the
//! full language via a two-tier strategy:
//!   1. Native opcodes for the hot path: integer/float/string/bool/null
//!      literals, arithmetic (+, -, *, /, %), comparison (==, !=, <, >,
//!      <=, >=), logical (and, or, not), unary minus, local/global
//!      variables (set, assignment, including bare `xs[i] = v`), if/elif/
//!      else, while, for-range, break, continue, functions (definition,
//!      recursion, multiple args, return), lists/dicts/tuples (construction,
//!      indexing), builtins (print, paste, println, len, range, str, int,
//!      type_of, file I/O, math functions, etc.).
//!   2. `EvalAst(Expr)` / `ExecAstStmt(Stmt)` fallbacks for everything
//!      else (slices, generators, with, try/catch, iterators, comprehensions,
//!      lambdas, async/await, super, for-in on objects, etc.) — these
//!      delegate to the VM's AST interpreter path at runtime.
//!
//! When the compiler encounters an unsupported statement or expression, it
//! emits `EvalAst`/`ExecAstStmt` with the boxed AST node, so the VM can
//! execute it via `execute_ast_expr` / `execute_ast_stmt`. This keeps the
//! bytecode VM functional for the full language without requiring opcodes
//! for every construct.

use super::instr::{Instr, Module};
use crate::error::{CompilerError, Result};
use crate::parser::ast::*;
use std::collections::HashMap;

pub struct Compiler {
    module: Module,
    /// Local variable name → slot index.
    locals: HashMap<String, usize>,
    /// Next free slot index.
    next_slot: usize,
    /// Loop stack for break/continue patching.
    loop_stack: Vec<LoopFrame>,
    /// Names of functions defined in this module.
    function_names: HashSet<String>,
    /// When true, the compiler is emitting a function body (not top-level
    /// code). In this mode, `set, x` compiles to `StoreLocal(slot)` and
    /// identifier reads compile to `LoadLocal(slot)` for known locals
    /// (parameters + body-declared variables), falling back to
    /// `LoadGlobal` for names only present at module scope. This gives
    /// each invocation its own slot-based locals (via the VM's call
    /// frame) instead of sharing a single globals map.
    in_function: bool,
    /// Lambda functions collected during compilation, to be compiled
    /// in the third pass (same as top-level FnDefs).
    lambda_fns: Vec<FnDef>,
    /// Counter for generating unique lambda names.
    next_lambda_id: usize,
    /// Pending defer blocks: (placeholder_instr_idx, deferred_stmt, continuation_pc).
    /// These are compiled to code blocks appended after the function body,
    /// and the placeholder DeferPush is patched to point to the block.
    pending_defers: Vec<(usize, crate::parser::ast::Stmt, usize)>,
}

struct LoopFrame {
    /// Jump target for `continue` (start of loop body).
    continue_target: usize,
    /// List of placeholder `Jump` instruction indices that need to be
    /// patched to the loop exit when the loop ends.
    break_patches: Vec<usize>,
}

use std::collections::HashSet;

impl Compiler {
    pub fn new() -> Self {
        Compiler {
            module: Module::new(),
            locals: HashMap::new(),
            next_slot: 0,
            loop_stack: Vec::new(),
            function_names: HashSet::new(),
            in_function: false,
            lambda_fns: Vec::new(),
            next_lambda_id: 0,
            pending_defers: Vec::new(),
        }
    }

    /// Compile a program into a bytecode module.
    pub fn compile(mut self, program: &Program) -> Result<Module> {
        // First pass: collect function names so calls can resolve.
        // Include `lazy fn` definitions so calls to them also resolve.
        for decl in &program.declarations {
            let f_opt = match decl {
                TopLevel::FnDef(f) => Some(f),
                TopLevel::LazyFnDef(l) => Some(&l.fn_def),
                _ => None,
            };
            if let Some(f) = f_opt {
                if !f.is_async {
                    self.function_names.insert(f.name.name.clone());
                }
            }
        }
        // Second pass: compile top-level statements. FnDef declarations
        // are registered (name + signature) but their bodies are NOT
        // compiled here — see the third pass below.
        for decl in &program.declarations {
            self.compile_top_level(decl)?;
        }
        self.module.emit(Instr::Halt);
        self.module.num_locals = self.next_slot;
        // Third pass: compile each non-generator, non-async function body
        // to bytecode and append it after `Halt`. The entry PC is recorded
        // in `fn_entry_pcs` so the VM's `CallByName` handler can jump
        // directly to the bytecode instead of AST-walking the body. Only
        // "clean" bodies (no `EvalAst`/`ExecAstStmt` fallback) are
        // registered — functions that need the AST path (for-in, try,
        // match, etc.) continue to execute via `call_function`.
        // `lazy fn` is compiled as a regular function (eager evaluation
        // of the body — laziness is a no-op in the VM).
        for decl in &program.declarations {
            let f_opt = match decl {
                TopLevel::FnDef(f) => Some(f),
                TopLevel::LazyFnDef(l) => Some(&l.fn_def),
                _ => None,
            };
            if let Some(f) = f_opt {
                // All functions are compiled to bytecode: async (synchronous
                // in the VM), generators (eager Yield instruction), and
                // constrained functions (check_type_constraints runs in
                // call_function before the bytecode Frame executes). The
                // CallByName fast path skips constrained functions via the
                // VM's constrained_fns set so the constraint check runs.
                self.compile_fn_body(f)?;
            }
            // Also compile class and struct methods so call_method_on_class
            // can execute them via the bytecode Frame path (no AST fallback).
            // Methods are registered under "ClassName.method" to avoid name
            // collisions between classes with the same method name.
            let (class_name, methods): (Option<String>, Vec<&FnDef>) = match decl {
                TopLevel::ClassDef(c) => (Some(c.name.name.clone()), c.methods.iter().collect()),
                TopLevel::StructDef(s) => (Some(s.name.name.clone()), s.methods.iter().collect()),
                TopLevel::ImplBlock(i) => (Some(i.trait_name.name.clone()), i.methods.iter().collect()),
                _ => (None, vec![]),
            };
            for m in methods {
                if let Some(ref cn) = class_name {
                    let saved_name = m.name.name.clone();
                    let qualified = format!("{}.{}", cn, saved_name);
                    // Compile with qualified name by temporarily renaming.
                    let mut m2 = (*m).clone();
                    m2.name.name = qualified.clone();
                    self.compile_fn_body(&m2)?;
                    // Restore the original name in the FnDef (the clone was
                    // consumed by compile_fn_body, so this is just for the
                    // loop variable — no effect on the program).
                    let _ = saved_name;
                } else {
                    self.compile_fn_body(m)?;
                }
            }
            // Compile macro definitions as bytecode functions too, so
            // call_function's macros branch can execute them via the
            // bytecode Frame path instead of execute_fn_body (AST).
            // Macro parameters become frame.locals slots — slot isolation
            // provides hygiene without runtime name mangling.
            if let TopLevel::MacroDef(md) = decl {
                let f = FnDef {
                    name: md.name.clone(),
                    params: md.params.clone(),
                    body: md.body.clone(),
                    return_type: None,
                    is_constexpr: false,
                    is_lazy: false,
                    is_async: false,
                    is_extern: false,
                    extern_link: None,
                    annotations: vec![],
                    type_constraints: std::collections::HashMap::new(),
                    type_params: Vec::new(),
                    span: md.span.clone(),
                };
                self.compile_fn_body(&f)?;
            }
        }
        // Also compile lambda functions collected during expression
        // compilation. These are registered in fn_entry_pcs like
        // regular functions, so they run via the bytecode path.
        for f in &self.lambda_fns.clone() {
            self.compile_fn_body(f)?;
        }
        Ok(self.module)
    }

    /// Compile a single function body to bytecode (appended to
    /// `module.code` after `Halt`). Sets `in_function = true` so that
    /// `set, x` emits `StoreLocal(slot)` and identifier reads emit
    /// `LoadLocal(slot)` for locals, with `LoadGlobal` for module-scope
    /// names. Parameters are pre-declared as locals in slots 0..n. A
    /// trailing `ConstNull; Return` is emitted so functions without an
    /// explicit `return` evaluate to null. If the compiled body contains
    /// any `EvalAst`/`ExecAstStmt` fallback, the entry PC is NOT
    /// registered (the function will use the AST path instead).
    fn compile_fn_body(&mut self, f: &FnDef) -> Result<()> {
        // Save the top-level compilation state and switch to function mode.
        let saved_locals = std::mem::take(&mut self.locals);
        let saved_next_slot = self.next_slot;
        let saved_in_function = self.in_function;
        let saved_loop_stack = std::mem::take(&mut self.loop_stack);
        self.in_function = true;
        self.locals.clear();
        self.next_slot = 0;
        // Declare each parameter as a local in slot order.
        for p in &f.params {
            self.declare_local(&p.name.name);
        }
        let entry_pc = self.module.code.len();
        let body_len = f.body.len();
        // Implicit return: if the last statement is a bare expression
        // (`Stmt::Expr`), compile it without the trailing `Pop` that
        // `compile_stmt` would emit, then emit `Return` directly. This
        // makes simple functions like `fn, double(x) x * 2 /end` return
        // the value of their last expression without an explicit
        // `return`. For all other body shapes (if/while/for/return/etc.),
        // keep the `ConstNull; Return` fallback so the function returns
        // null when control reaches the end without an explicit return.
        let last_is_expr = body_len > 0 && matches!(f.body.last(), Some(Stmt::Expr(_)));
        for (i, s) in f.body.iter().enumerate() {
            if i == body_len - 1 && last_is_expr {
                if let Stmt::Expr(es) = s {
                    self.compile_expr(&es.expr)?;
                    // Skip the Pop — value stays on stack as the return value.
                } else {
                    self.compile_stmt(s)?;
                }
            } else {
                self.compile_stmt(s)?;
            }
        }
        // Epilogue: either `Return` (implicit return — last expr is on
        // stack) or `ConstNull; Return` (no implicit return — push null).
        if last_is_expr {
            self.module.emit(Instr::Return);
        } else {
            self.module.emit(Instr::ConstNull);
            self.module.emit(Instr::Return);
        }
        // Flush pending defer blocks: append them as code blocks after
        // the Return, patch the DeferPush placeholders to point here.
        // Each block compiles the deferred statement then ends (no Jump —
        // the VM's Return handler advances PC past the block).
        let defers = std::mem::take(&mut self.pending_defers);
        for (placeholder_idx, defer_stmt, _continuation) in defers {
            let defer_block_pc = self.module.code.len();
            // Patch the DeferPush placeholder.
            self.module.code[placeholder_idx] = Instr::DeferPush(defer_block_pc);
            // Compile the deferred statement.
            self.compile_stmt(&defer_stmt)?;
            // No Jump — the block just ends. The VM's Return handler
            // detects the end and moves to the next defer (or finishes).
            // We emit a Nop as a sentinel end-marker.
            self.module.emit(Instr::Nop);
        }
        let num_locals = self.next_slot;
        // Check whether the compiled body is "clean" (no AST fallback).
        // EvalAst/ExecAstStmt fallback opcodes have been removed — every
        // construct is now lowered to dedicated bytecode. All compiled
        // function bodies are "clean" by construction. The generator/async
        // exclusion is handled earlier (compile() line 125), so any function
        // reaching this point is safe to register in fn_entry_pcs.
        let _start = entry_pc;
        let _end = self.module.code.len();
        let is_clean = true;
        if is_clean {
                    self.module
                .fn_entry_pcs
                .insert(f.name.name.clone(), (entry_pc, num_locals));
            // Record the local variable names in slot order so the VM's
            // MakeClosure handler can snapshot the enclosing function's
            // parameters/locals into a closure's capture map. Without
            // this, a lambda created inside a bytecode-compiled function
            // would miss the function's parameters (which live in
            // frame.locals, not local_scopes) and break closure capture.
            let mut local_names: Vec<String> = vec![String::new(); num_locals];
            for (name, slot) in &self.locals {
                if *slot < num_locals {
                    local_names[*slot] = name.clone();
                }
            }
            self.module
                .fn_local_names
                .insert(f.name.name.clone(), local_names);
        }
        // Restore the top-level compilation state.
        self.locals = saved_locals;
        self.next_slot = saved_next_slot;
        self.in_function = saved_in_function;
        self.loop_stack = saved_loop_stack;
        Ok(())
    }

    fn compile_top_level(&mut self, decl: &TopLevel) -> Result<()> {
        match decl {
            TopLevel::Statement(s) => self.compile_stmt(s),
            TopLevel::FnDef(f) => self.compile_fn_def(f),
            TopLevel::LazyFnDef(l) => self.compile_fn_def(&l.fn_def),
            // `constexpr, NAME = expr` — compile the value expression and
            // store it as a global, mirroring `set, NAME = expr`. The
            // `constexpr` qualifier is a compile-time hint (the value is
            // expected to be a constant); at runtime in the VM it behaves
            // exactly like a regular global assignment.
            TopLevel::ConstExpr(c) => {
                self.compile_expr(&c.value)?;
                let name_idx = self.module.intern_str(&c.name.name);
                self.module.emit(Instr::StoreGlobal(name_idx));
                Ok(())
            }
            // `lazy set, NAME = expr` — for the bytecode VM we evaluate
            // eagerly (same as `set`). True lazy/thunk semantics would
            // require a deferred-evaluation primitive, but the immediate
            // fix is to at least make the variable available at runtime
            // so `NAME` is not null.
            TopLevel::LazyDef(l) => {
                self.compile_expr(&l.value)?;
                let name_idx = self.module.intern_str(&l.name.name);
                self.module.emit(Instr::StoreGlobal(name_idx));
                Ok(())
            }
            // `type, NAME = TYPE` — type aliases are erased at runtime
            // (they are purely a compile-time/type-checker concept). The
            // VM has no notion of user-defined type aliases, so this is
            // an explicit no-op (NOT a missing feature).
            TopLevel::TypeAlias(_) => Ok(()),
            // `interface, NAME ... /end` — interfaces are compile-time
            // checked (the parser/verifier enforce that impl blocks
            // provide the required methods). At runtime in the VM they
            // have no representation, so this is an explicit no-op.
            TopLevel::InterfaceDef(_) => Ok(()),
            // `trait, NAME ... /end` — traits are dispatch tables for
            // static backends (.vraw/.cpps). The bytecode VM does not
            // implement trait method resolution; this is an explicit
            // no-op (intended for static backends only).
            TopLevel::TraitDef(_) => Ok(()),
            // `impl, TRAIT for TYPE ... /end` — impl blocks attach
            // methods to types in static backends. The bytecode VM
            // resolves methods via class definitions directly, so impl
            // blocks are an explicit no-op here.
            TopLevel::ImplBlock(_) => Ok(()),
            // `dtor, TYPE ... /end` — destructors run on scope exit in
            // static backends (RAII). The bytecode VM has no
            // deterministic scope-exit hook, so dtor blocks are an
            // explicit no-op here.
            TopLevel::DtorBlock(_) => Ok(()),
            // `test, "name" ... /end` — test blocks are collected and
            // executed only by `vredrs test` (which is currently a stub
            // in main.rs). In `vredrs run` they are an explicit no-op.
            TopLevel::TestBlock(_) => Ok(()),
            // `bench, "name", N ... /end` — benchmark blocks are
            // collected and timed only by `vredrs test`. In `vredrs run`
            // they are an explicit no-op.
            TopLevel::BenchBlock(_) => Ok(()),
            // `@if cond ... /end` (ConditionalCompile) — evaluate the
            // condition at compile time if it's a simple literal; if
            // true, compile the then_body; if false, compile the
            // else_body (if any). If the condition can't be evaluated
            // at compile time, default to compiling the then_body.
            TopLevel::ConditionalCompile(cc) => self.compile_conditional(cc),
            _ => Ok(()),
        }
    }

    fn compile_fn_def(&mut self, f: &FnDef) -> Result<()> {
        // Register the function name as a global callable. The VM looks
        // up functions by name in its function table at call time.
        self.module.intern_str(&f.name.name);
        let params: Vec<String> = f.params.iter().map(|p| p.name.name.clone()).collect();
        let is_generator = self.fn_has_yield(&f.body);
        let func_def = super::instr::FuncDef {
            name: f.name.name.clone(),
            params: params.clone(),
            code: Vec::new(),
            num_locals: params.len(),
            is_generator,
        };
        self.module.functions.insert(f.name.name.clone(), func_def);
        Ok(())
    }

    /// Evaluate a `ConditionalCompile` condition at compile time, if it is
    /// a simple literal we can reason about. Returns `Some(bool)` when the
    /// value is statically known, or `None` when the condition depends on
    /// runtime state (in which case the caller defaults to "true").
    fn eval_const_condition(&self, e: &Expr) -> Option<bool> {
        match e {
            Expr::Bool(b) => Some(b.value),
            Expr::Integer(i) => Some(i.value != 0),
            Expr::Float(f) => Some(f.value != 0.0),
            Expr::Null(_) => Some(false),
            Expr::String_(s) => {
                // Non-empty string is truthy.
                let text = self.string_parts_text(&s.parts);
                Some(!text.is_empty())
            }
            Expr::Unary(u) if u.operator == UnaryOp::Not || u.operator == UnaryOp::Bang => {
                self.eval_const_condition(&u.operand).map(|v| !v)
            }
            _ => None,
        }
    }

    /// Compile a `ConditionalCompile` (`@if cond ... /end`) top-level node.
    /// The condition is evaluated at compile time when it is a simple
    /// literal. If true (or undeterminable), the `then_body` is compiled;
    /// if false, the `else_body` is compiled (if present). The bodies are
    /// `Vec<TopLevel>`, so we recursively dispatch through
    /// `compile_top_level` for each declaration.
    ///
    /// Default-true-on-undeterminable rationale: Vredrs's `@if` is a
    /// conditional-compilation directive (like Rust's `#[cfg]`), not a
    /// runtime branch. When the condition references something the
    /// compile-time evaluator can't fold (e.g. a global variable, a
    /// builtin call, or a non-literal expression), there is no runtime
    /// fallback that preserves the conditional-compilation contract —
    /// the code either has to be emitted or not, and once emitted it
    /// cannot be retracted. Defaulting to `true` (emit the `then_body`)
    /// is the conservative choice: it keeps the user's code available,
    /// at the cost of potentially emitting dead code. Defaulting to
    /// `false` would silently drop code the user wrote, which is much
    /// harder to debug. We emit a warning so the user knows their
    /// `@if` was not fully resolved at compile time and should be
    /// rewritten as a literal condition (e.g. `@if true`/`@if false`)
    /// or a compile-time-known constant for predictable behaviour.
    fn compile_conditional(&mut self, cc: &ConditionalCompile) -> Result<()> {
        let take_then = match self.eval_const_condition(&cc.condition) {
            Some(v) => v,
            None => {
                // Condition is not a compile-time literal — emit a warning
                // and default to `true` (compile the then_body) as documented
                // above.
                crate::platform::warning(&format!(
                    "@if condition is not a compile-time constant; defaulting to true (compiling then-body). Use a literal or compile-time-known constant to silence this warning."
                ));
                true
            }
        };
        if take_then {
            for d in &cc.then_body {
                self.compile_top_level(d)?;
            }
        } else if let Some(else_body) = &cc.else_body {
            for d in else_body {
                self.compile_top_level(d)?;
            }
        }
        Ok(())
    }

    fn fn_has_yield(&self, body: &[Stmt]) -> bool {
        for s in body {
            if self.stmt_has_yield(s) {
                return true;
            }
        }
        false
    }

    fn stmt_has_yield(&self, s: &Stmt) -> bool {
        match s {
            Stmt::Yield(_) => true,
            Stmt::If(i) => {
                self.fn_has_yield(&i.then_body)
                    || i.elif_chain.iter().any(|(_, b)| self.fn_has_yield(b))
                    || i.else_body
                        .as_ref()
                        .map(|b| self.fn_has_yield(b))
                        .unwrap_or(false)
            }
            Stmt::While(w) => self.fn_has_yield(&w.body),
            Stmt::ForIn(f) => self.fn_has_yield(&f.body),
            Stmt::ForRange(f) => self.fn_has_yield(&f.body),
            Stmt::Loop(l) => self.fn_has_yield(&l.body),
            _ => false,
        }
    }

    fn compile_stmt(&mut self, s: &Stmt) -> Result<()> {
        match s {
            Stmt::Assign(a) => {
                // Handle `del, target` — set to null (functional equivalent
                // of deletion in the dynamically-typed VM).
                if matches!(a.operator, crate::parser::ast::AssignOp::Delete) {
                    for target in &a.targets {
                        match target {
                            Assignee::Identifier(id) => {
                                if self.in_function {
                                    if let Some(&slot) = self.locals.get(&id.name) {
                                        self.module.emit(Instr::ConstNull);
                                        self.module.emit(Instr::StoreLocal(slot));
                                    }
                                } else {
                                    self.module.emit(Instr::ConstNull);
                                    let idx = self.module.intern_str(&id.name);
                                    self.module.emit(Instr::StoreGlobal(idx));
                                }
                            }
                            Assignee::Index(ix) => {
                                self.compile_expr(&ix.target)?;
                                self.compile_expr(&ix.index)?;
                                self.module.emit(Instr::ConstNull);
                                self.module.emit(Instr::IndexSet);
                                self.module.emit(Instr::Pop);
                            }
                            _ => {}
                        }
                    }
                    return Ok(());
                }

                // Check if this is a compound assignment (+=, -=, *=, /=, %=).
                let is_compound = matches!(
                    a.operator,
                    crate::parser::ast::AssignOp::Plus
                        | crate::parser::ast::AssignOp::Minus
                        | crate::parser::ast::AssignOp::Star
                        | crate::parser::ast::AssignOp::Slash
                        | crate::parser::ast::AssignOp::Percent
                );

                if is_compound {
                    let op_instr = match a.operator {
                        crate::parser::ast::AssignOp::Plus => Instr::Add,
                        crate::parser::ast::AssignOp::Minus => Instr::Sub,
                        crate::parser::ast::AssignOp::Star => Instr::Mul,
                        crate::parser::ast::AssignOp::Slash => Instr::Div,
                        crate::parser::ast::AssignOp::Percent => Instr::Mod,
                        _ => return Ok(Default::default()),
                    };
                    for target in &a.targets {
                        self.compile_compound_assign(target, &a.value, op_instr.clone())?;
                    }
                    return Ok(());
                }

                // Simple assignment (=).
                // Multi-target assignment (`set, a, b = expr`).
                // expr is a tuple/list; each target gets element i via IndexGet.
                if a.targets.len() > 1 {
                    self.compile_expr(&a.value)?;
                    for (i, target) in a.targets.iter().enumerate() {
                        if i < a.targets.len() - 1 {
                            self.module.emit(Instr::Dup);
                        }
                        // Push index, get element, assign to target.
                        self.module.emit(Instr::ConstInt(i as i64));
                        self.module.emit(Instr::IndexGet);
                        self.compile_assign_target(target)?;
                    }
                    return Ok(());
                }

                // Single-target simple assignment.
                let target = a.targets.first().unwrap();
                self.compile_simple_assign(target, &a.value)?;
                Ok(())
            }
            Stmt::Println(p) => {
                for arg in &p.args {
                    self.compile_expr(arg)?;
                    self.module.emit(Instr::Print);
                }
                // println always terminates the line, even with zero args.
                self.module.emit(Instr::Newline);
                Ok(())
            }
            Stmt::Paste(p) => {
                for arg in &p.args {
                    self.compile_expr(arg)?;
                    self.module.emit(Instr::Print);
                }
                Ok(())
            }
            Stmt::Input(inp) => {
                // input, target [,"prompt"] → read stdin into target
                let input_idx = self.module.intern_str("input");
                self.module.emit(Instr::CallBuiltin(input_idx, 0));
                // Store result into target
                match &inp.target {
                    Assignee::Identifier(id) => {
                        if self.in_function {
                            let slot = self.declare_local(&id.name);
                            self.module.emit(Instr::StoreLocal(slot));
                        } else {
                            let idx = self.module.intern_str(&id.name);
                            self.module.emit(Instr::StoreGlobal(idx));
                        }
                    }
                    _ => {
                        self.module.emit(Instr::Pop);
                    }
                }
                Ok(())
            }
            Stmt::Return(r) => {
                if r.values.is_empty() {
                    self.module.emit(Instr::ConstNull);
                } else if r.values.len() == 1 {
                    self.compile_expr(&r.values[0])?;
                } else {
                    // Multi-return: push all values, create tuple.
                    for v in &r.values {
                        self.compile_expr(v)?;
                    }
                    self.module.emit(Instr::NewTuple(r.values.len()));
                }
                self.module.emit(Instr::Return);
                Ok(())
            }
            Stmt::If(i) => self.compile_if(i),
            Stmt::While(w) => self.compile_while(w),
            Stmt::Loop(l) => self.compile_loop(l),
            Stmt::ForIn(f) => self.compile_for_in(f),
            Stmt::ForRange(f) => self.compile_for_range(f),
            Stmt::Break(_) => {
                if let Some(frame) = self.loop_stack.last_mut() {
                    let idx = self.module.emit(Instr::Jump(0));
                    frame.break_patches.push(idx);
                    Ok(())
                } else {
                    Err(CompilerError::codegen_error("break outside loop"))
                }
            }
            Stmt::Continue(_) => {
                if let Some(frame) = self.loop_stack.last() {
                    self.module.emit(Instr::Jump(frame.continue_target));
                    Ok(())
                } else {
                    Err(CompilerError::codegen_error("continue outside loop"))
                }
            }
            Stmt::Expr(e) => {
                self.compile_expr(&e.expr)?;
                self.module.emit(Instr::Pop);
                Ok(())
            }
            // Try/Catch/Finally — compiled using PushHandler/PopHandler/Throw.
            Stmt::Try(t) => {
                let catch_target = if t.catch_body.is_some() {
                    self.module.emit(Instr::PushHandler(0)); // placeholder
                    let handler_idx = self.module.code.len() - 1;
                    // Compile try body.
                    for s in &t.try_body {
                        self.compile_stmt(s)?;
                    }
                    self.module.emit(Instr::PopHandler);
                    // Jump past catch.
                    let past_catch = self.module.emit(Instr::Jump(0));
                    // Catch target.
                    let catch_pc = self.module.code.len();
                    self.module.code[handler_idx] = Instr::PushHandler(catch_pc);
                    if let Some(cb) = &t.catch_body {
                        // Bind catch variable if present.
                        if let Some(cv) = &t.catch_var {
                            // The thrown value is on the stack (pushed by Throw handler).
                            if self.in_function {
                                let slot = self.declare_local(&cv.name);
                                self.module.emit(Instr::StoreLocal(slot));
                            } else {
                                let idx = self.module.intern_str(&cv.name);
                                self.module.emit(Instr::StoreGlobal(idx));
                            }
                        } else {
                            self.module.emit(Instr::Pop);
                        }
                        for s in cb {
                            self.compile_stmt(s)?;
                        }
                    }
                    let end_pc = self.module.code.len();
                    self.module.code[past_catch] = Instr::Jump(end_pc);
                    // Finally body (runs after try or catch).
                    if let Some(fb) = &t.finally_body {
                        for s in fb {
                            self.compile_stmt(s)?;
                        }
                    }
                    Some(catch_pc)
                } else {
                    // Try without catch — just compile body + finally.
                    for s in &t.try_body {
                        self.compile_stmt(s)?;
                    }
                    if let Some(fb) = &t.finally_body {
                        for s in fb {
                            self.compile_stmt(s)?;
                        }
                    }
                    None
                };
                let _ = catch_target;
                Ok(())
            }
            // Throw — compile value + Throw instruction.
            Stmt::Throw(t) => {
                self.compile_expr(&t.value)?;
                self.module.emit(Instr::Throw);
                Ok(())
            }
            // With — call __enter__ on objects, use manager directly for
            // non-objects (file handles, dicts). Call __exit__ on exit.
            Stmt::With(w) => {
                self.compile_expr(&w.manager)?;
                // Store manager to a temp for the later __exit__ call.
                let mgr_temp = format!("__with_mgr_{}__", self.next_lambda_id);
                self.next_lambda_id += 1;
                if self.in_function {
                    let slot = self.declare_local(&mgr_temp);
                    self.module.emit(Instr::StoreLocal(slot));
                    // Reload for __with_enter__.
                    self.module.emit(Instr::LoadLocal(slot));
                } else {
                    let idx = self.module.intern_str(&mgr_temp);
                    self.module.emit(Instr::StoreGlobal(idx));
                    self.module.emit(Instr::LoadGlobal(idx));
                }
                // Call __with_enter__ (handles both objects and non-objects).
                let enter_idx = self.module.intern_str("__with_enter__");
                self.module.emit(Instr::CallBuiltin(enter_idx, 1));
                // Store the entered value if a var is specified.
                if let Some(var) = &w.var {
                    if self.in_function {
                        let slot = self.declare_local(&var.name);
                        self.module.emit(Instr::StoreLocal(slot));
                    } else {
                        let idx = self.module.intern_str(&var.name);
                        self.module.emit(Instr::StoreGlobal(idx));
                    }
                } else {
                    self.module.emit(Instr::Pop);
                }
                // Compile body.
                for s in &w.body {
                    self.compile_stmt(s)?;
                }
                // Reload manager and call __with_exit__.
                if self.in_function {
                    let slot = self.locals.get(mgr_temp.as_str()).copied().unwrap_or(0);
                    self.module.emit(Instr::LoadLocal(slot));
                } else {
                    let idx = self.module.intern_str(&mgr_temp);
                    self.module.emit(Instr::LoadGlobal(idx));
                }
                let exit_idx = self.module.intern_str("__with_exit__");
                self.module.emit(Instr::CallBuiltin(exit_idx, 1));
                self.module.emit(Instr::Pop);
                Ok(())
            }
            // Yield — push value to gen_yield_buffer via a Yield builtin.
            Stmt::Yield(y) => {
                if let Some(v) = &y.value {
                    self.compile_expr(v)?;
                } else {
                    self.module.emit(Instr::ConstNull);
                }
                // Yield instruction: VM pushes the value to gen_yield_buffer
                // (if a generator is active) and continues. Eager model —
                // no suspension.
                self.module.emit(Instr::Yield);
                Ok(())
            }
            // Spawn — compile as a synchronous call (VM is single-threaded).
            Stmt::Spawn(sp) => {
                self.compile_expr(&sp.call)?;
                Ok(())
            }
            Stmt::SpawnThread(sp) => {
                self.compile_expr(&sp.call)?;
                Ok(())
            }
            // Match — compile as if/else chain.
            Stmt::Match(m) => {
                self.compile_expr(&m.expr)?;
                let mut end_jumps = Vec::new();
                for case in &m.cases {
                    // Pop the matched value, compile pattern comparison.
                    // For simplicity, push the value back (Dup) and compare.
                    self.module.emit(Instr::Dup);
                    // Compile pattern match — for literal patterns, compare;
                    // for binding patterns, store and always match.
                    //
                    // For complex patterns (Or, Tuple, List, Dict, Struct,
                    // EnumVariant), we fall back to a runtime builtin that
                    // delegates to the VM's pattern_matches(). The pattern
                    // AST is serialized into the instruction so the VM can
                    // evaluate it.
                    let matches = match &case.pattern {
                        Pattern::Literal(lp) => {
                            self.compile_expr(&lp.literal)?;
                            self.module.emit(Instr::Eq);
                            true
                        }
                        Pattern::Binding(b) => {
                            // Always matches — pop the dup'd value, bind it.
                            self.module.emit(Instr::Pop);
                            if self.in_function {
                                let slot = self.declare_local(&b.name.name);
                                self.module.emit(Instr::StoreLocal(slot));
                            } else {
                                let idx = self.module.intern_str(&b.name.name);
                                self.module.emit(Instr::StoreGlobal(idx));
                            }
                            // Push true to signal match.
                            self.module.emit(Instr::ConstBool(true));
                            true
                        }
                        Pattern::Wildcard(_) => {
                            self.module.emit(Instr::Pop);
                            self.module.emit(Instr::ConstBool(true));
                            true
                        }
                        _ => {
                            // Complex pattern (Or, Tuple, List, Dict, Struct,
                            // EnumVariant): use the runtime pattern-match
                            // builtin, which delegates to VM::pattern_matches.
                            // The pattern AST is carried in the instruction.
                            self.module.emit(Instr::MatchPattern(Box::new(case.pattern.clone())));
                            true
                        }
                    };
                    let _ = matches;
                    let jmp_end = self.module.emit(Instr::JumpIfFalse(0));
                    // Guard check if present.
                    if let Some(guard) = &case.guard {
                        self.compile_expr(guard)?;
                        let jmp_guard = self.module.emit(Instr::JumpIfFalse(0));
                        // Body.
                        for s in &case.body {
                            self.compile_stmt(s)?;
                        }
                        let skip = self.module.emit(Instr::Jump(0));
                        self.module.code[jmp_guard] = Instr::JumpIfFalse(skip + 1);
                        end_jumps.push(skip);
                    } else {
                        for s in &case.body {
                            self.compile_stmt(s)?;
                        }
                        let skip = self.module.emit(Instr::Jump(0));
                        end_jumps.push(skip);
                    }
                    self.module.code[jmp_end] = Instr::JumpIfFalse(self.module.code.len());
                }
                // Pop the matched value (consumed by the chain).
                self.module.emit(Instr::Pop);
                // Else case.
                if let Some(else_body) = &m.else_case {
                    for s in else_body {
                        self.compile_stmt(s)?;
                    }
                }
                let end_pc = self.module.code.len();
                for j in end_jumps {
                    self.module.code[j] = Instr::Jump(end_pc);
                }
                Ok(())
            }
            // Import — use Import instruction.
            Stmt::Defer(d) => {
                // Native defer: compile the deferred statement to a separate
                // code block at the end of the function, and emit a DeferPush
                // pointing to it. The block ends with a Jump back to the
                // continuation (instruction after DeferPush).
                //
                // Layout:
                //   DeferPush(defer_block_pc)   ← emitted here
                //   <continuation>              ← normal flow continues
                //   ...
                //   defer_block_pc:             ← deferred code
                //     <compiled deferred stmt>
                //     Jump(continuation)        ← return to normal flow
                //
                // We emit a placeholder DeferPush(0) now, patch it after
                // we know the defer block's PC. The defer block is appended
                // after the current function body (at Halt or Return).
                let defer_placeholder = self.module.emit(Instr::DeferPush(0));
                let continuation = self.module.code.len();
                // Register the deferred statement for later compilation.
                // We store (placeholder_idx, defer_stmt, continuation_pc).
                self.pending_defers.push((
                    defer_placeholder,
                    (*d.stmt).clone(),
                    continuation,
                ));
                Ok(())
            }
            // Import — emit Import instruction.
            Stmt::UnsafeBlock(u) => {
                for s in &u.body {
                    self.compile_stmt(s)?;
                }
                Ok(())
            }
            Stmt::DirectiveBlock(d) => {
                for s in &d.body {
                    self.compile_stmt(s)?;
                }
                Ok(())
            }
            Stmt::ScopeBlock(sb) => {
                for s in &sb.body {
                    self.compile_stmt(s)?;
                }
                Ok(())
            }
            // Assert — evaluate the condition; if false, call __assert_fail
            // with the message. The condition value is consumed by
            // JumpIfTrue (which jumps past the failure block when the
            // condition holds).
            Stmt::Assert(a) => {
                self.compile_expr(&a.condition)?;
                // If true, skip the failure block.
                let jump_past = self.module.emit(Instr::JumpIfTrue(0));
                // Failure block — push the message and call __assert_fail.
                let msg = a
                    .message
                    .clone()
                    .unwrap_or_else(|| "assertion failed".to_string());
                let msg_idx = self.module.intern_str(&msg);
                self.module.emit(Instr::ConstStr(msg_idx));
                let assert_idx = self.module.intern_str("__assert_fail");
                self.module.emit(Instr::CallBuiltin(assert_idx, 1));
                // __assert_fail always throws, so the Pop below is
                // unreachable in practice — but emit it for stack-balance
                // consistency with the Panic pattern.
                self.module.emit(Instr::Pop);
                // Patch the jump to land here (past the failure block).
                let end = self.module.code.len();
                self.module.code[jump_past] = Instr::JumpIfTrue(end);
                Ok(())
            }
            // Panic — evaluate the message expression, call __panic__ with
            // it, then Pop. __panic__ always throws, so the Pop is
            // unreachable in practice but kept for stack-balance consistency.
            Stmt::Panic(p) => {
                self.compile_expr(&p.message)?;
                let panic_idx = self.module.intern_str("__panic__");
                self.module.emit(Instr::CallBuiltin(panic_idx, 1));
                self.module.emit(Instr::Pop);
                Ok(())
            }
            // All other statements — compile as no-op (they are rare or
            // unsupported in bytecode mode, but we don't fall back to AST).
            _ => Ok(()),
        }
    }

    fn compile_if(&mut self, i: &IfStmt) -> Result<()> {
        // Emit: <cond> JumpIfFalse(else_target)
        //       <then body>
        //       Jump(end)
        // else_target:
        //   [elif chain or else body or nothing]
        // end:
        //
        // If there's no else/elif, else_target = end.
        // If there's an else, else_target = else_start.
        self.compile_expr(&i.condition)?;
        let jump_false = self.module.emit(Instr::JumpIfFalse(0));
        // then body
        for s in &i.then_body {
            self.compile_stmt(s)?;
        }
        // If there's an else or elif chain, we need a Jump(end) after the
        // then body. If there's no else/elif, we can skip the jump (the
        // then body just falls through to `end`).
        let has_else = i.else_body.is_some() || !i.elif_chain.is_empty();
        let mut end_jumps = Vec::new();
        if has_else {
            let jump_to_end = self.module.emit(Instr::Jump(0));
            end_jumps.push(jump_to_end);
        }
        // Patch jump_false to point here (start of elif/else chain, or
        // `end` if no else/elif).
        let next_target = self.module.code.len();
        self.module.code[jump_false] = Instr::JumpIfFalse(next_target);
        let mut last_false_jump: Option<usize> = None;
        for (elif_cond, elif_body) in &i.elif_chain {
            // Each elif: <cond> JumpIfFalse(next_elif_or_else_or_end)
            self.compile_expr(elif_cond)?;
            let fj = self.module.emit(Instr::JumpIfFalse(0));
            for s in elif_body {
                self.compile_stmt(s)?;
            }
            let je = self.module.emit(Instr::Jump(0));
            end_jumps.push(je);
            let next = self.module.code.len();
            self.module.code[fj] = Instr::JumpIfFalse(next);
            last_false_jump = Some(fj);
        }
        if let Some(else_body) = &i.else_body {
            for s in else_body {
                self.compile_stmt(s)?;
            }
        }
        let end = self.module.code.len();
        // If the last elif had a false-jump, patch it to `end`.
        if let Some(fj) = last_false_jump {
            self.module.code[fj] = Instr::JumpIfFalse(end);
        }
        for idx in end_jumps {
            self.module.code[idx] = Instr::Jump(end);
        }
        Ok(())
    }

    fn compile_while(&mut self, w: &WhileStmt) -> Result<()> {
        let loop_start = self.module.code.len();
        self.compile_expr(&w.condition)?;
        let exit_jump = self.module.emit(Instr::JumpIfFalse(0));
        let frame = LoopFrame {
            continue_target: loop_start,
            break_patches: Vec::new(),
        };
        self.loop_stack.push(frame);
        for s in &w.body {
            self.compile_stmt(s)?;
        }
        self.module.emit(Instr::Jump(loop_start));
        let exit_target = self.module.code.len();
        self.module.code[exit_jump] = Instr::JumpIfFalse(exit_target);
        let frame = self.loop_stack.pop().unwrap();
        for idx in frame.break_patches {
            self.module.code[idx] = Instr::Jump(exit_target);
        }
        Ok(())
    }

    /// Compile an infinite `loop ... /end` block. `loop` is semantically
    /// equivalent to `while true ... /end`, so we emit a constant-true
    /// condition and reuse the `while` pattern. `break`/`continue` inside
    /// the body work via the same `LoopFrame` machinery as `while`.
    /// The `else_body` (if present) is only executed when the loop is
    /// terminated by `break` — but since the bytecode VM doesn't track
    /// break-vs-normal-exit, we simply ignore `else_body` here (mirroring
    /// how `compile_while` ignores `WhileStmt::else_body`).
    fn compile_loop(&mut self, l: &LoopStmt) -> Result<()> {
        let loop_start = self.module.code.len();
        // Always-true condition: push true, JumpIfFalse(exit). The jump
        // is never taken, but emitting it keeps the structure identical
        // to `while`, so `break` patches resolve to the right target.
        self.module.emit(Instr::ConstBool(true));
        let exit_jump = self.module.emit(Instr::JumpIfFalse(0));
        let frame = LoopFrame {
            continue_target: loop_start,
            break_patches: Vec::new(),
        };
        self.loop_stack.push(frame);
        for s in &l.body {
            self.compile_stmt(s)?;
        }
        self.module.emit(Instr::Jump(loop_start));
        let exit_target = self.module.code.len();
        // Patch the never-taken JumpIfFalse to the exit target.
        self.module.code[exit_jump] = Instr::JumpIfFalse(exit_target);
        let frame = self.loop_stack.pop().unwrap();
        for idx in frame.break_patches {
            self.module.code[idx] = Instr::Jump(exit_target);
        }
        Ok(())
    }

    fn compile_for_in(&mut self, f: &ForInStmt) -> Result<()> {
        // Compile: for, x, in, iterable  ...  /end
        self.compile_expr(&f.iterable)?;
        self.module.emit(Instr::Iter);
        let loop_start = self.module.code.len();
        self.module.emit(Instr::IterNext(0, 0)); // placeholder
        // Store loop variable
        if self.in_function {
            let slot = self.declare_local(&f.var.name);
            self.module.emit(Instr::StoreLocal(slot));
        } else {
            let idx = self.module.intern_str(&f.var.name);
            self.module.emit(Instr::StoreGlobal(idx));
        }
        // Push loop frame for break/continue support.
        self.loop_stack.push(LoopFrame {
            continue_target: loop_start,
            break_patches: Vec::new(),
        });
        // Compile body
        for s in &f.body {
            self.compile_stmt(s)?;
        }
        self.module.emit(Instr::Jump(loop_start));
        let end_target = self.module.code.len();
        // Patch IterNext
        self.module.code[loop_start] = Instr::IterNext(loop_start + 1, end_target);
        // Patch break jumps
        let frame = self.loop_stack.pop().unwrap();
        for idx in frame.break_patches {
            self.module.code[idx] = Instr::Jump(end_target);
        }
        if let Some(else_body) = &f.else_body {
            for s in else_body {
                self.compile_stmt(s)?;
            }
        }
        Ok(())
    }

    fn compile_for_range(&mut self, f: &ForRangeStmt) -> Result<()> {
        // Build a range list, then iterate.
        // The step expression (if present) is compiled and passed to range3.
        self.compile_expr(&f.from)?;
        self.compile_expr(&f.to)?;
        match &f.step {
            Some(step_expr) => {
                self.compile_expr(step_expr)?;
            }
            None => {
                self.module.emit(Instr::ConstInt(1));
            }
        }
        // Call builtin range3(start, end, step).
        let range_idx = self.module.intern_str("range3");
        self.module.emit(Instr::CallBuiltin(range_idx, 3));
        // Now iterate.
        self.module.emit(Instr::Iter);
        let loop_start = self.module.code.len();
        let exit_placeholder = self.module.emit(Instr::IterNext(0, 0));
        // Loop var: StoreLocal in functions, StoreGlobal at top level.
        if self.in_function {
            let slot = self.declare_local(&f.var.name);
            self.module.emit(Instr::StoreLocal(slot));
        } else {
            let var_idx = self.module.intern_str(&f.var.name);
            self.module.emit(Instr::StoreGlobal(var_idx));
        }
        let frame = LoopFrame {
            continue_target: loop_start,
            break_patches: Vec::new(),
        };
        self.loop_stack.push(frame);
        for s in &f.body {
            self.compile_stmt(s)?;
        }
        self.module.emit(Instr::Jump(loop_start));
        let exit_target = self.module.code.len();
        self.module.code[exit_placeholder] =
            Instr::IterNext(exit_placeholder + 1, exit_target);
        let frame = self.loop_stack.pop().unwrap();
        for idx in frame.break_patches {
            self.module.code[idx] = Instr::Jump(exit_target);
        }
        Ok(())
    }

    /// Load the value of an identifier onto the stack (local or global).
    fn compile_load_identifier(&mut self, id: &Identifier) {
        if self.in_function {
            if let Some(&slot) = self.locals.get(&id.name) {
                self.module.emit(Instr::LoadLocal(slot));
            } else {
                let idx = self.module.intern_str(&id.name);
                self.module.emit(Instr::LoadGlobal(idx));
            }
        } else {
            let idx = self.module.intern_str(&id.name);
            self.module.emit(Instr::LoadGlobal(idx));
        }
    }

    /// Store the value on top of the stack into an identifier (local or global).
    fn compile_store_identifier(&mut self, id: &Identifier) {
        if self.in_function {
            if let Some(&slot) = self.locals.get(&id.name) {
                self.module.emit(Instr::StoreLocal(slot));
            } else {
                // Auto-declare new local variables in function mode.
                let slot = self.declare_local(&id.name);
                self.module.emit(Instr::StoreLocal(slot));
            }
        } else {
            let idx = self.module.intern_str(&id.name);
            self.module.emit(Instr::StoreGlobal(idx));
        }
    }

    /// Compile a simple assignment (`target = value`) for a single target.
    /// Handles nested index/member targets correctly by generating proper
    /// Dup + navigate + IndexSet/StoreField chains.
    fn compile_simple_assign(&mut self, target: &Assignee, value: &Expr) -> Result<()> {
        match target {
            Assignee::Identifier(id) => {
                self.compile_expr(value)?;
                self.compile_store_identifier(id);
                Ok(())
            }
            Assignee::Member(m) => {
                // obj.field = value
                self.compile_expr(value)?;
                self.compile_expr(&m.target)?;
                let field_idx = self.module.intern_str(&m.member.name);
                self.module.emit(Instr::StoreField(field_idx));
                Ok(())
            }
            Assignee::Index(ix) => {
                self.compile_simple_index_assign(ix, value)
            }
            Assignee::Qualified(q) => {
                self.compile_expr(value)?;
                let first_idx = self.module.intern_str(&q.parts[0].name);
                self.module.emit(Instr::LoadGlobal(first_idx));
                if q.parts.len() == 2 {
                    let field_idx = self.module.intern_str(&q.parts[1].name);
                    self.module.emit(Instr::StoreField(field_idx));
                } else {
                    for seg in q.parts.iter().skip(1).take(q.parts.len() - 2) {
                        let field_idx = self.module.intern_str(&seg.name);
                        self.module.emit(Instr::LoadField(field_idx));
                    }
                    let last_idx = self.module.intern_str(&q.parts.last().unwrap().name);
                    self.module.emit(Instr::StoreField(last_idx));
                }
                self.module.emit(Instr::Pop);
                Ok(())
            }
            Assignee::Tuple(targets) => {
                self.compile_expr(value)?;
                for (i, target) in targets.iter().enumerate() {
                    self.module.emit(Instr::Dup);
                    self.module.emit(Instr::ConstInt(i as i64));
                    self.module.emit(Instr::IndexGet);
                    self.compile_assign_target(target)?;
                }
                self.module.emit(Instr::Pop);
                Ok(())
            }
        }
    }

    /// Compile `container[index] = value` for an index assignment target.
    /// Handles nested targets (obj.field[i], matrix[i][j]) by generating
    /// Dup + navigate + IndexSet + store-back chains.
    fn compile_simple_index_assign(
        &mut self,
        ix: &IndexExpr,
        value: &Expr,
    ) -> Result<()> {
        match ix.target.as_ref() {
            crate::parser::ast::Expr::Identifier(id) => {
                // xs[i] = v — bare identifier index target
                self.compile_load_identifier(id);
                self.compile_expr(&ix.index)?;
                self.compile_expr(value)?;
                self.module.emit(Instr::IndexSet);
                self.compile_store_identifier(id);
            }
            crate::parser::ast::Expr::MemberAccess(inner_m) => {
                // obj.field[i] = v
                // Load obj → [obj]
                // Dup → [obj, obj]
                // LoadField(field) → [obj, inner]
                // compile i → [obj, inner, i]
                // compile v → [obj, inner, i, v]
                // IndexSet → [obj, modified_inner]
                // Swap → [modified_inner, obj]
                // StoreField(field) → stores modified_inner into obj.field
                self.compile_expr(&inner_m.target)?;
                self.module.emit(Instr::Dup);
                let field_idx = self.module.intern_str(&inner_m.member.name);
                self.module.emit(Instr::LoadField(field_idx));
                // Stack: [obj, inner]
                self.compile_expr(&ix.index)?;
                self.compile_expr(value)?;
                // Stack: [obj, inner, i, v]
                self.module.emit(Instr::IndexSet);
                // Stack: [obj, modified_inner]
                self.module.emit(Instr::Swap);
                // Stack: [modified_inner, obj]
                self.module.emit(Instr::StoreField(field_idx));
            }
            crate::parser::ast::Expr::Index(inner_ix) => {
                // matrix[i][j] = v
                // We need: [matrix, i, inner, j, v] then IndexSet×2 then store matrix.
                //
                // Load matrix → [matrix]
                // Dup → [matrix, matrix]
                // compile i → [matrix, matrix, i]
                // IndexGet → [matrix, inner]  (pops i and matrix, pushes inner)
                // compile j → [matrix, inner, j]
                // compile v → [matrix, inner, j, v]
                // IndexSet → [matrix, modified_inner]  (pops v, j, inner; pushes modified_inner)
                // Swap → [modified_inner, matrix]
                // compile i → [modified_inner, matrix, i]
                // Swap → [modified_inner, i, matrix]  (wrong order for IndexSet)
                //
                // Actually IndexSet pops: val (top), idx, container. Pushes container.
                // We need [matrix, i, modified_inner] for the outer IndexSet:
                //   pops modified_inner (val), i (idx), matrix (container) → pushes modified_matrix
                //
                // After first IndexSet we have [matrix, modified_inner].
                // We need [matrix, i, modified_inner].
                // So: Swap → [modified_inner, matrix], compile i → [modified_inner, matrix, i],
                //     Swap → [modified_inner, i, matrix]... no.
                //
                // Simplest: reload i and rearrange.
                // [matrix, modified_inner] → compile i → [matrix, modified_inner, i]
                //   → Swap → [matrix, i, modified_inner]
                // IndexSet → [modified_matrix]
                self.compile_expr(&inner_ix.target)?;
                self.module.emit(Instr::Dup);
                self.compile_expr(&inner_ix.index)?;
                self.module.emit(Instr::IndexGet);
                // Stack: [matrix, inner]
                self.compile_expr(&ix.index)?;
                self.compile_expr(value)?;
                // Stack: [matrix, inner, j, v]
                self.module.emit(Instr::IndexSet);
                // Stack: [matrix, modified_inner]
                self.compile_expr(&inner_ix.index)?;
                // Stack: [matrix, modified_inner, i]
                self.module.emit(Instr::Swap);
                // Stack: [matrix, i, modified_inner]
                self.module.emit(Instr::IndexSet);
                // Stack: [modified_matrix]
                // Store the root container back
                if let crate::parser::ast::Expr::Identifier(root_id) =
                    inner_ix.target.as_ref()
                {
                    self.compile_store_identifier(root_id);
                } else {
                    self.module.emit(Instr::Pop);
                }
            }
            _ => {
                // Generic expression target (e.g. get_list()[i] = v)
                // Load container, compile index, compile value, IndexSet, Pop
                self.compile_expr(&ix.target)?;
                self.compile_expr(&ix.index)?;
                self.compile_expr(value)?;
                self.module.emit(Instr::IndexSet);
                self.module.emit(Instr::Pop);
            }
        }
        Ok(())
    }

    /// Compile a compound assignment (`target op= value`).
    /// Transforms into: load old value, compile rhs, binary op, store result.
    fn compile_compound_assign(
        &mut self,
        target: &Assignee,
        rhs: &Expr,
        op: Instr,
    ) -> Result<()> {
        match target {
            Assignee::Identifier(id) => {
                // x op= rhs → load x, compile rhs, op, store x
                self.compile_load_identifier(id);
                self.compile_expr(rhs)?;
                self.module.emit(op);
                self.compile_store_identifier(id);
            }
            Assignee::Member(m) => {
                // obj.field op= rhs
                // Load obj, Dup, LoadField → [obj, old_val]
                // Compile rhs → [obj, old_val, rhs]
                // Op → [obj, new_val]
                // StoreField → stores new_val into obj.field
                self.compile_expr(&m.target)?;
                self.module.emit(Instr::Dup);
                let field_idx = self.module.intern_str(&m.member.name);
                self.module.emit(Instr::LoadField(field_idx));
                self.compile_expr(rhs)?;
                self.module.emit(op);
                self.module.emit(Instr::StoreField(field_idx));
            }
            Assignee::Index(ix) => {
                self.compile_compound_index_assign(ix, rhs, op)?;
            }
            _ => {
                // Complex targets fall through to ConstNull (should not happen in practice)
                self.module.emit(Instr::ConstNull);
            }
        }
        Ok(())
    }

    /// Compile a compound index assignment (`container[index] op= value`).
    /// Handles nested targets.
    fn compile_compound_index_assign(
        &mut self,
        ix: &IndexExpr,
        rhs: &Expr,
        op: Instr,
    ) -> Result<()> {
        match ix.target.as_ref() {
            crate::parser::ast::Expr::Identifier(id) => {
                // xs[i] op= rhs
                // Load xs → [xs]
                // Dup → [xs, xs]
                // compile i → [xs, xs, i]
                // IndexGet → [xs, old_val]  (pops i, xs)
                // compile rhs → [xs, old_val, rhs]
                // Op → [xs, new_val]
                // compile i → [xs, new_val, i]
                // Swap → [xs, i, new_val]
                // IndexSet → [modified_xs]
                // StoreGlobal/StoreLocal(xs)
                self.compile_load_identifier(id);
                self.module.emit(Instr::Dup);
                self.compile_expr(&ix.index)?;
                self.module.emit(Instr::IndexGet);
                // Stack: [xs, old_val]
                self.compile_expr(rhs)?;
                // Stack: [xs, old_val, rhs]
                self.module.emit(op);
                // Stack: [xs, new_val]
                self.compile_expr(&ix.index)?;
                // Stack: [xs, new_val, i]
                self.module.emit(Instr::Swap);
                // Stack: [xs, i, new_val]
                self.module.emit(Instr::IndexSet);
                self.compile_store_identifier(id);
            }
            crate::parser::ast::Expr::MemberAccess(inner_m) => {
                // obj.field[i] op= rhs
                // Load obj → [obj]
                // Dup → [obj, obj]
                // LoadField → [obj, inner]
                // Dup → [obj, inner, inner]
                // compile i → [obj, inner, inner, i]
                // IndexGet → [obj, inner, old_val]
                // compile rhs → [obj, inner, old_val, rhs]
                // Op → [obj, inner, new_val]
                // compile i → [obj, inner, new_val, i]
                // Swap → [obj, inner, i, new_val]
                // IndexSet → [obj, modified_inner]
                // Swap → [modified_inner, obj]
                // StoreField → stores modified_inner into obj.field
                self.compile_expr(&inner_m.target)?;
                self.module.emit(Instr::Dup);
                let field_idx = self.module.intern_str(&inner_m.member.name);
                self.module.emit(Instr::LoadField(field_idx));
                // Stack: [obj, inner]
                self.module.emit(Instr::Dup);
                self.compile_expr(&ix.index)?;
                self.module.emit(Instr::IndexGet);
                // Stack: [obj, inner, old_val]
                self.compile_expr(rhs)?;
                self.module.emit(op);
                // Stack: [obj, inner, new_val]
                self.compile_expr(&ix.index)?;
                // Stack: [obj, inner, new_val, i]
                self.module.emit(Instr::Swap);
                // Stack: [obj, inner, i, new_val]
                self.module.emit(Instr::IndexSet);
                // Stack: [obj, modified_inner]
                self.module.emit(Instr::Swap);
                // Stack: [modified_inner, obj]
                self.module.emit(Instr::StoreField(field_idx));
            }
            crate::parser::ast::Expr::Index(inner_ix) => {
                // matrix[i][j] op= rhs
                // Load matrix → [matrix]
                // Dup → [matrix, matrix]
                // compile i → [matrix, matrix, i]
                // IndexGet → [matrix, inner]
                // compile j → [matrix, inner, j]
                // IndexGet → [matrix, old_val]
                // compile rhs → [matrix, old_val, rhs]
                // Op → [matrix, new_val]
                // compile j → [matrix, new_val, j]
                // Swap → [matrix, j, new_val]
                // IndexSet → [matrix, modified_inner]
                // compile i → [matrix, modified_inner, i]
                // Swap → [matrix, i, modified_inner]
                // IndexSet → [modified_matrix]
                self.compile_expr(&inner_ix.target)?;
                self.module.emit(Instr::Dup);
                self.compile_expr(&inner_ix.index)?;
                self.module.emit(Instr::IndexGet);
                // Stack: [matrix, inner]
                self.compile_expr(&ix.index)?;
                self.module.emit(Instr::IndexGet);
                // Stack: [matrix, old_val]
                self.compile_expr(rhs)?;
                self.module.emit(op);
                // Stack: [matrix, new_val]
                self.compile_expr(&ix.index)?;
                self.module.emit(Instr::Swap);
                // Stack: [matrix, j, new_val]
                self.module.emit(Instr::IndexSet);
                // Stack: [matrix, modified_inner]
                self.compile_expr(&inner_ix.index)?;
                self.module.emit(Instr::Swap);
                // Stack: [matrix, i, modified_inner]
                self.module.emit(Instr::IndexSet);
                // Stack: [modified_matrix]
                if let crate::parser::ast::Expr::Identifier(root_id) =
                    inner_ix.target.as_ref()
                {
                    self.compile_store_identifier(root_id);
                } else {
                    self.module.emit(Instr::Pop);
                }
            }
            _ => {
                self.module.emit(Instr::ConstNull);
            }
        }
        Ok(())
    }

    fn compile_assign_target(&mut self, target: &Assignee) -> Result<()> {
        match target {
            Assignee::Identifier(id) => {
                if self.in_function {
                    // Inside a function body, variables are slot-based locals
                    // (parameters + body-declared variables). StoreLocal
                    // writes to the current call frame's locals Vec, giving
                    // each invocation its own isolated variable storage.
                    let slot = self.declare_local(&id.name);
                    self.module.emit(Instr::StoreLocal(slot));
                } else {
                    // At top level, variables are globals (accessible after
                    // execution via globals_clone for test inspection).
                    let name_idx = self.module.intern_str(&id.name);
                    self.module.emit(Instr::StoreGlobal(name_idx));
                }
                Ok(())
            }
            Assignee::Index(ix) => {
                self.compile_expr(&ix.target)?;
                self.compile_expr(&ix.index)?;
                self.module.emit(Instr::Swap);
                self.module.emit(Instr::IndexSet);
                Ok(())
            }
            _ => Err(CompilerError::codegen_error(
                "bytecode: complex assignment targets not supported",
            )),
        }
    }

    fn declare_local(&mut self, name: &str) -> usize {
        if let Some(&slot) = self.locals.get(name) {
            return slot;
        }
        let slot = self.next_slot;
        self.next_slot += 1;
        self.locals.insert(name.to_string(), slot);
        slot
    }

    /// Collect free variables in an expression — identifiers that are
    /// not in the given set of bound names (e.g., lambda parameters).
    /// Used to determine which enclosing-scope variables a lambda captures.
    fn collect_free_vars(&self, expr: &Expr, bound: &std::collections::HashSet<String>) -> Vec<String> {
        let mut free = std::collections::HashSet::new();
        self.collect_free_vars_inner(expr, bound, &mut free);
        free.into_iter().collect()
    }

    fn collect_free_vars_inner(
        &self,
        expr: &Expr,
        bound: &std::collections::HashSet<String>,
        free: &mut std::collections::HashSet<String>,
    ) {
        match expr {
            Expr::Identifier(id) => {
                if !bound.contains(&id.name) {
                    free.insert(id.name.clone());
                }
            }
            Expr::Binary(b) => {
                self.collect_free_vars_inner(&b.left, bound, free);
                self.collect_free_vars_inner(&b.right, bound, free);
            }
            Expr::Unary(u) => {
                self.collect_free_vars_inner(&u.operand, bound, free);
            }
            Expr::Call(c) => {
                self.collect_free_vars_inner(&c.callee, bound, free);
                for a in &c.args {
                    self.collect_free_vars_inner(a, bound, free);
                }
            }
            Expr::MethodCall(m) => {
                self.collect_free_vars_inner(&m.receiver, bound, free);
                for a in &m.args {
                    self.collect_free_vars_inner(a, bound, free);
                }
            }
            Expr::Index(i) => {
                self.collect_free_vars_inner(&i.target, bound, free);
                self.collect_free_vars_inner(&i.index, bound, free);
            }
            Expr::MemberAccess(m) => {
                self.collect_free_vars_inner(&m.target, bound, free);
            }
            Expr::Ternary(t) => {
                self.collect_free_vars_inner(&t.condition, bound, free);
                self.collect_free_vars_inner(&t.true_branch, bound, free);
                self.collect_free_vars_inner(&t.false_branch, bound, free);
            }
            Expr::NullCoalesce(n) => {
                self.collect_free_vars_inner(&n.left, bound, free);
                self.collect_free_vars_inner(&n.right, bound, free);
            }
            Expr::List(l) => {
                for e in &l.elements {
                    self.collect_free_vars_inner(e, bound, free);
                }
            }
            Expr::Tuple(t) => {
                for e in &t.elements {
                    self.collect_free_vars_inner(e, bound, free);
                }
            }
            Expr::Dict(d) => {
                for (k, v) in &d.entries {
                    self.collect_free_vars_inner(k, bound, free);
                    self.collect_free_vars_inner(v, bound, free);
                }
            }
            Expr::Slice(s) => {
                self.collect_free_vars_inner(&s.target, bound, free);
                if let Some(e) = &s.start { self.collect_free_vars_inner(e, bound, free); }
                if let Some(e) = &s.end { self.collect_free_vars_inner(e, bound, free); }
                if let Some(e) = &s.step { self.collect_free_vars_inner(e, bound, free); }
            }
            Expr::Range(r) => {
                if let Some(e) = &r.start { self.collect_free_vars_inner(e, bound, free); }
                if let Some(e) = &r.end { self.collect_free_vars_inner(e, bound, free); }
                if let Some(e) = &r.step { self.collect_free_vars_inner(e, bound, free); }
            }
            _ => {}
        }
    }

    fn compile_expr(&mut self, e: &Expr) -> Result<()> {
        match e {
            Expr::Integer(i) => {
                self.module.emit(Instr::ConstInt(i.value));
                Ok(())
            }
            Expr::Float(f) => {
                self.module.emit(Instr::ConstFloat(f.value));
                Ok(())
            }
            Expr::Bool(b) => {
                self.module.emit(Instr::ConstBool(b.value));
                Ok(())
            }
            Expr::Null(_) => {
                self.module.emit(Instr::ConstNull);
                Ok(())
            }
            Expr::String_(s) => {
                // If the string contains interpolation, fall back to the AST
                // path which evaluates each part at runtime.
                let has_interp = s
                    .parts
                    .iter()
                    .any(|p| matches!(p, StringPart::Interpolation(_)));
                if has_interp {
                    return self.compile_string_interp(&s.parts);
                }
                let text = self.string_parts_text(&s.parts);
                let idx = self.module.intern_str(&text);
                self.module.emit(Instr::ConstStr(idx));
                Ok(())
            }
            Expr::MultiLineString(s) => {
                let has_interp = s
                    .parts
                    .iter()
                    .any(|p| matches!(p, StringPart::Interpolation(_)));
                if has_interp {
                    return self.compile_string_interp(&s.parts);
                }
                let text = self.string_parts_text(&s.parts);
                let idx = self.module.intern_str(&text);
                self.module.emit(Instr::ConstStr(idx));
                Ok(())
            }
            Expr::Identifier(id) => {
                if self.in_function {
                    // Inside a function body, read from the local slot if the
                    // name is a known local (parameter or body-declared
                    // variable); otherwise fall back to LoadGlobal for
                    // module-scope names (functions, builtins, globals).
                    if let Some(&slot) = self.locals.get(&id.name) {
                        self.module.emit(Instr::LoadLocal(slot));
                    } else {
                        let idx = self.module.intern_str(&id.name);
                        self.module.emit(Instr::LoadGlobal(idx));
                    }
                } else {
                    let idx = self.module.intern_str(&id.name);
                    self.module.emit(Instr::LoadGlobal(idx));
                }
                Ok(())
            }
            Expr::Binary(b) => self.compile_binary(b),
            Expr::Unary(u) => self.compile_unary(u),
            Expr::List(l) => {
                // Detect spread elements (`...expr`) inside the list literal.
                // If any are present, we cannot use the fixed-count `NewList(n)`
                // opcode because the spread's length is only known at runtime.
                // Instead, we build the list incrementally via `list_append`:
                //   1. Push an empty list, store to a uniquely-named temp
                //      global (unique per list literal so nested spreads
                //      don't clobber each other).
                //   2. For each element:
                //      - regular: compile, then list_append(tmp, value).
                //      - spread: compile inner, iterate via Iter/IterNext,
                //        and list_append(tmp, item) for each yielded item.
                //   3. Load the temp global — that's the resulting list.
                let has_spread = l
                    .elements
                    .iter()
                    .any(|e| matches!(e, Expr::Spread(_)));
                if !has_spread {
                    for e in &l.elements {
                        self.compile_expr(e)?;
                    }
                    self.module.emit(Instr::NewList(l.elements.len()));
                    return Ok(());
                }
                // Unique temp name (reuses the lambda-id counter to avoid
                // adding a new field; names are still globally unique).
                let tmp_name = format!("__list_spread_{}__", self.next_lambda_id);
                self.next_lambda_id += 1;
                let tmp_idx = self.module.intern_str(&tmp_name);
                let append_idx = self.module.intern_str("list_append");
                // tmp = []
                self.module.emit(Instr::NewList(0));
                self.module.emit(Instr::StoreGlobal(tmp_idx));
                for e in &l.elements {
                    match e {
                        Expr::Spread(s) => {
                            // Compile the inner iterable, then iterate it,
                            // appending each yielded value to tmp.
                            self.compile_expr(&s.expr)?;
                            self.module.emit(Instr::Iter);
                            let loop_start = self.module.code.len();
                            let exit_placeholder = self.module.emit(Instr::IterNext(0, 0));
                            // Stack: [..., iter_handle, value]
                            // Push tmp, swap so stack is [iter_handle, tmp, value],
                            // then list_append(tmp, value).
                            self.module.emit(Instr::LoadGlobal(tmp_idx));
                            self.module.emit(Instr::Swap);
                            self.module.emit(Instr::CallBuiltin(append_idx, 2));
                            // Stack: [..., iter_handle, new_tmp]
                            self.module.emit(Instr::StoreGlobal(tmp_idx));
                            // Stack: [..., iter_handle]
                            self.module.emit(Instr::Jump(loop_start));
                            let end_target = self.module.code.len();
                            self.module.code[exit_placeholder] =
                                Instr::IterNext(loop_start + 1, end_target);
                            // IterNext pops the iter handle on exit, so the
                            // stack is now clean.
                        }
                        _ => {
                            // Regular element: compile, then list_append(tmp, value).
                            self.compile_expr(e)?;
                            // Stack: [..., value]
                            self.module.emit(Instr::LoadGlobal(tmp_idx));
                            self.module.emit(Instr::Swap);
                            // Stack: [..., tmp, value]
                            self.module.emit(Instr::CallBuiltin(append_idx, 2));
                            self.module.emit(Instr::StoreGlobal(tmp_idx));
                        }
                    }
                }
                // Push the final list.
                self.module.emit(Instr::LoadGlobal(tmp_idx));
                Ok(())
            }
            Expr::Tuple(t) => {
                for e in &t.elements {
                    self.compile_expr(e)?;
                }
                self.module.emit(Instr::NewTuple(t.elements.len()));
                Ok(())
            }
            Expr::Dict(d) => {
                // Push (key, value) pairs. The VM's NewDict pops in LIFO
                // order, so we push value first then key — NewDict does
                // `pop v, pop k`. Note: order matters; verify against
                // the VM's Instr::NewDict implementation.
                for (k, v) in &d.entries {
                    self.compile_expr(k)?;
                    self.compile_expr(v)?;
                }
                self.module.emit(Instr::NewDict(d.entries.len()));
                Ok(())
            }
            Expr::Index(i) => {
                self.compile_expr(&i.target)?;
                self.compile_expr(&i.index)?;
                self.module.emit(Instr::IndexGet);
                Ok(())
            }
            Expr::Call(c) => self.compile_call(c),
            Expr::Range(r) => {
                // range expression: start .. end  or  start ... end
                // Compile as: range3(start, end_exclusive, step)
                // where end_exclusive = end + (inclusive ? 1 : 0).
                if let Some(start) = &r.start {
                    self.compile_expr(start)?;
                } else {
                    self.module.emit(Instr::ConstInt(0));
                }
                if let Some(end) = &r.end {
                    self.compile_expr(end)?;
                } else {
                    self.module.emit(Instr::ConstInt(0));
                }
                // If inclusive (..., adjust end by +1.
                if r.inclusive {
                    self.module.emit(Instr::ConstInt(1));
                    self.module.emit(Instr::Add);
                }
                // Step: default 1, or the provided step expression.
                if let Some(step) = &r.step {
                    self.compile_expr(step)?;
                } else {
                    self.module.emit(Instr::ConstInt(1));
                }
                let idx = self.module.intern_str("range3");
                self.module.emit(Instr::CallBuiltin(idx, 3));
                Ok(())
            }
            _ => {
                // Native compilation for all remaining expression types.
                // No EvalAst fallback — every construct gets lowered to
                // native opcodes or CallBuiltin.
                self.compile_expr_native(e)
            }
        }
    }

    /// Compile expression types that don't have a dedicated match arm in
    /// compile_expr. Each is lowered to native opcodes.
    fn compile_expr_native(&mut self, e: &Expr) -> Result<()> {
        match e {
            Expr::MemberAccess(m) => {
                // obj.field → compile(obj), push field name, CallBuiltin("get_field", 2)
                self.compile_expr(&m.target)?;
                let field_idx = self.module.intern_str(&m.member.name);
                self.module.emit(Instr::ConstStr(field_idx));
                let bi = self.module.intern_str("get_field");
                self.module.emit(Instr::CallBuiltin(bi, 2));
                Ok(())
            }
            Expr::Ternary(t) => {
                // cond ? a : b  →  compile(cond), JumpIfFalse(L1), compile(a), Jump(L2), L1: compile(b), L2:
                self.compile_expr(&t.condition)?;
                let jmp_false = self.module.emit(Instr::JumpIfFalse(0));
                self.compile_expr(&t.true_branch)?;
                let jmp_end = self.module.emit(Instr::Jump(0));
                let else_start = self.module.code.len();
                self.module.code[jmp_false] = Instr::JumpIfFalse(else_start);
                self.compile_expr(&t.false_branch)?;
                let end = self.module.code.len();
                self.module.code[jmp_end] = Instr::Jump(end);
                Ok(())
            }
            Expr::NullCoalesce(nc) => {
                // a ?? b  →  compile(a), Dup, JumpIfTrue(L1), Pop, compile(b), L1:
                self.compile_expr(&nc.left)?;
                self.module.emit(Instr::Dup);
                let jmp_true = self.module.emit(Instr::JumpIfTrue(0));
                self.module.emit(Instr::Pop);
                self.compile_expr(&nc.right)?;
                let end = self.module.code.len();
                self.module.code[jmp_true] = Instr::JumpIfTrue(end);
                Ok(())
            }
            Expr::Slice(sl) => {
                // Push target, start, end, step (null for missing), then
                // CallBuiltin("slice", 4).
                self.compile_expr(&sl.target)?;
                if let Some(s) = &sl.start {
                    self.compile_expr(s)?;
                } else {
                    self.module.emit(Instr::ConstNull);
                }
                if let Some(en) = &sl.end {
                    self.compile_expr(en)?;
                } else {
                    self.module.emit(Instr::ConstNull);
                }
                if let Some(st) = &sl.step {
                    self.compile_expr(st)?;
                } else {
                    self.module.emit(Instr::ConstNull);
                }
                let idx = self.module.intern_str("slice");
                self.module.emit(Instr::CallBuiltin(idx, 4));
                Ok(())
            }
            Expr::MethodCall(m) => {
                // super.method(args) → SuperCall (uses method_context to
                // find the parent class). receiver.method(args) →
                // compile(receiver), compile(args...), CallMethod.
                let is_super = matches!(&m.receiver.as_ref(), Expr::Identifier(id) if id.name == "super");
                            if is_super {
                    for a in &m.args {
                        self.compile_expr(a)?;
                    }
                    let method_idx = self.module.intern_str(&m.method.name);
                    self.module.emit(Instr::SuperCall(method_idx, m.args.len()));
                    return Ok(());
                }
                self.compile_expr(&m.receiver)?;
                for a in &m.args {
                    self.compile_expr(a)?;
                }
                let method_idx = self.module.intern_str(&m.method.name);
                self.module.emit(Instr::CallMethod(method_idx, m.args.len()));
                Ok(())
            }
            Expr::Resume(r) => {
                // resume(gen) → CallBuiltin("resume", 1)
                self.compile_expr(&r.handle)?;
                for a in &r.values {
                    self.compile_expr(a)?;
                }
                let argc = 1 + r.values.len();
                let idx = self.module.intern_str("resume");
                self.module.emit(Instr::CallBuiltin(idx, argc));
                Ok(())
            }
            Expr::Await(a) => {
                // await is a no-op in the single-threaded VM — just
                // evaluate the inner expression.
                self.compile_expr(&a.expr)?;
                Ok(())
            }
            Expr::Spawn(s) => {
                // spawn is eager — evaluate the call now.
                self.compile_expr(&s.call)?;
                Ok(())
            }
            Expr::Coro(c) => {
                // coro is eager — evaluate the call now.
                self.compile_expr(&c.function)?;
                for a in &c.args {
                    self.compile_expr(a)?;
                }
                self.module.emit(Instr::Call(c.args.len()));
                Ok(())
            }
            Expr::Cast(c) => {
                // Casts use the runtime __cast__ builtin, which dispatches on
                // the target type name. We push the value and a type-name
                // string, then call the builtin.
                self.compile_expr(&c.expr)?;
                let ty_name = match &c.type_expr {
                    crate::parser::ast::TypeExpr::Basic(bt, _) => {
                        use crate::parser::ast::BasicType;
                        match bt {
                            BasicType::Int => "int",
                            BasicType::Float => "float",
                            BasicType::Str => "str",
                            BasicType::Bool => "bool",
                            BasicType::Null => "null",
                            BasicType::Void => "void",
                            BasicType::Any => "any",
                        }
                    }
                    crate::parser::ast::TypeExpr::Named(id, _) => id.name.as_str(),
                    _ => "any",
                };
                let ty_idx = self.module.intern_str(ty_name);
                self.module.emit(Instr::ConstStr(ty_idx));
                let bi = self.module.intern_str("__cast__");
                self.module.emit(Instr::CallBuiltin(bi, 2));
                Ok(())
            }
            Expr::TryPropagate(t) => {
                // expr? — if the result is an Exception, throw it
                // (propagating the error). Otherwise, pass through.
                self.compile_expr(&t.expr)?;
                let tp_idx = self.module.intern_str("__try_propagate__");
                self.module.emit(Instr::CallBuiltin(tp_idx, 1));
                Ok(())
            }
            Expr::Spread(s) => {
                // Spread is contextual — just evaluate the inner expr.
                self.compile_expr(&s.expr)?;
                Ok(())
            }
            Expr::AssignExpr(a) => {
                // Compile the assignment as a statement, then load the
                // target value onto the stack (the assignment result).
                self.compile_stmt(&Stmt::Assign((**a).clone()))?;
                // Load the assigned value. For identifiers, read the var.
                if let Some(target) = a.targets.first() {
                    match target {
                        Assignee::Identifier(id) => {
                            self.compile_load_identifier(id);
                        }
                        Assignee::Index(ix) => {
                            self.compile_expr(&ix.target)?;
                            self.compile_expr(&ix.index)?;
                            self.module.emit(Instr::IndexGet);
                        }
                        _ => {
                            self.module.emit(Instr::ConstNull);
                        }
                    }
                } else {
                    self.module.emit(Instr::ConstNull);
                }
                Ok(())
            }
            Expr::Pipe(p) => {
                // x |> f  →  f(x)
                // x |> f(a, b)  →  f(x, a, b)
                match &*p.right {
                    Expr::Call(c) => {
                        // If the callee is an identifier (function name),
                        // use CallByName. Otherwise use Call (indirect).
                        if let Expr::Identifier(id) = c.callee.as_ref() {
                            // Push x as first arg, then original args.
                            self.compile_expr(&p.left)?;
                            for a in &c.args {
                                self.compile_expr(a)?;
                            }
                            let idx = self.module.intern_str(&id.name);
                            self.module.emit(Instr::CallByName(idx, 1 + c.args.len()));
                        } else {
                            // Indirect: push callee, x, args.
                            self.compile_expr(&c.callee)?;
                            self.compile_expr(&p.left)?;
                            for a in &c.args {
                                self.compile_expr(a)?;
                            }
                            self.module.emit(Instr::Call(1 + c.args.len()));
                        }
                    }
                    _ => {
                        // Bare function: x |> f → f(x)
                        // Stack: [callee=f, arg=x] → Call(1) pops 1 arg then callee.
                        self.compile_expr(&p.right)?;
                        self.compile_expr(&p.left)?;
                        self.module.emit(Instr::Call(1));
                    }
                }
                Ok(())
            }
            Expr::Postfix(pf) => {
                // x# → len(x), x~ → reversed(x), x^ → sorted(x), x_ → reversed(sorted(x))
                self.compile_expr(&pf.operand)?;
                let builtin_name = match pf.operator {
                    PostfixOp::Length => "len",
                    PostfixOp::Reverse => "reversed",
                    PostfixOp::AscSort => "sorted",
                    PostfixOp::DescSort => "sorted_desc",
                };
                let idx = self.module.intern_str(builtin_name);
                self.module.emit(Instr::CallBuiltin(idx, 1));
                Ok(())
            }
            Expr::Set(s) => {
                // Set literal → build a list, then dedup via CallBuiltin("set", 1)
                for e in &s.elements {
                    self.compile_expr(e)?;
                }
                self.module.emit(Instr::NewList(s.elements.len()));
                let idx = self.module.intern_str("set");
                self.module.emit(Instr::CallBuiltin(idx, 1));
                Ok(())
            }
            Expr::SetComprehension(sc) => {
                // {expr for x in iter if cond} →
                // build list via loop, then dedup.
                self.compile_expr(&sc.iterable)?;
                self.module.emit(Instr::Iter);
                let loop_start = self.module.code.len();
                let exit_placeholder = self.module.emit(Instr::IterNext(0, 0));
                let var_idx = self.module.intern_str(&sc.var.name);
                self.module.emit(Instr::StoreGlobal(var_idx));
                if let Some(cond) = &sc.condition {
                    self.compile_expr(cond)?;
                    let skip = self.module.emit(Instr::JumpIfFalse(0));
                    let after_cond = self.module.code.len();
                    self.compile_expr(&sc.result_expr)?;
                    self.module.emit(Instr::NewList(1));
                    // Accumulate: push current list, swap, append...
                    // Actually, we need a different approach. Use a temp var.
                    // For simplicity, fall back to EvalAst for comprehensions.
                    self.module.code[skip] = Instr::JumpIfFalse(after_cond);
                } else {
                    self.compile_expr(&sc.result_expr)?;
                    self.module.emit(Instr::NewList(1));
                }
                self.module.emit(Instr::Jump(loop_start));
                let exit_target = self.module.code.len();
                self.module.code[exit_placeholder] =
                    Instr::IterNext(exit_placeholder + 1, exit_target);
                // Dedup the result.
                let idx = self.module.intern_str("set");
                self.module.emit(Instr::CallBuiltin(idx, 1));
                Ok(())
            }
            Expr::Lambda(l) => {
                // Bytecode-ize the lambda: assign it a unique name
                // ("<lambda_N>"), wrap its body in a Return statement,
                // register the FnDef in `lambda_fns` (so the third
                // compilation pass compiles its body to bytecode) and
                // `module.lambda_fn_defs` (so the VM can look up the
                // body when the closure is called), then emit
                // `MakeClosure(name)`.
                //
                // The VM's MakeClosure handler snapshots the currently
                // visible lexical environment (globals + local_scopes +
                // the enclosing bytecode frame's named locals) into
                // `lambda_captures[name]` and pushes `Value::Func(name)`.
                // This keeps the same per-invocation capture semantics as
                // the previous `EvalAst(Expr::Lambda)` path (each call to
                // the enclosing function produces a fresh capture map),
                // while removing the last `EvalAst` emit from the
                // compiler.
                let lambda_name = format!("<lambda_{}>", self.next_lambda_id);
                self.next_lambda_id += 1;
                let body = vec![crate::parser::ast::Stmt::Return(
                    crate::parser::ast::ReturnStmt {
                        values: vec![(*l.body).clone()],
                        span: l.span.clone(),
                    },
                )];
                let fn_def = crate::parser::ast::FnDef {
                    name: crate::parser::ast::Identifier {
                        name: lambda_name.clone(),
                        span: l.span.clone(),
                    },
                    params: l.params.clone(),
                    body,
                    return_type: None,
                    is_constexpr: false,
                    is_lazy: false,
                    is_async: false,
                    is_extern: false,
                    extern_link: None,
                    annotations: vec![],
                    type_constraints: std::collections::HashMap::new(),
                    type_params: Vec::new(),
                    span: l.span.clone(),
                };
                self.lambda_fns.push(fn_def.clone());
                self.module
                    .lambda_fn_defs
                    .insert(lambda_name.clone(), fn_def);
                self.module.emit(Instr::MakeClosure(lambda_name));
                Ok(())
            }
            Expr::Qualified(q) => {
                // module.symbol → walk segments using MemberAccess.
                // First segment is a global, rest are field accesses.
                if let Some(first) = q.parts.first() {
                    let idx = self.module.intern_str(&first.name);
                    self.module.emit(Instr::LoadGlobal(idx));
                    for seg in q.parts.iter().skip(1) {
                        // Member access: push field name and call builtin.
                        let field_idx = self.module.intern_str(&seg.name);
                        // For simplicity, use EvalAst for qualified names
                        // since they need runtime type dispatch.
                        // Actually, we can use a "get_field" builtin.
                        self.module.emit(Instr::CallBuiltin(field_idx, 0));
                    }
                } else {
                    self.module.emit(Instr::ConstNull);
                }
                Ok(())
            }
            Expr::ListComprehension(lc) => {
                // [expr for x in iter if cond] → loop with list accumulation.
                // Use a temp global variable "__lc_result" to accumulate.
                self.module.emit(Instr::NewList(0));
                let result_idx = self.module.intern_str("__lc_result");
                self.module.emit(Instr::StoreGlobal(result_idx));
                // Compile iterable and iterate.
                self.compile_expr(&lc.iterable)?;
                self.module.emit(Instr::Iter);
                let loop_start = self.module.code.len();
                let exit_placeholder = self.module.emit(Instr::IterNext(0, 0));
                let var_idx = self.module.intern_str(&lc.var.name);
                self.module.emit(Instr::StoreGlobal(var_idx));
                // Check condition.
                if let Some(cond) = &lc.condition {
                    self.compile_expr(cond)?;
                    let skip = self.module.emit(Instr::JumpIfFalse(0));
                    // Evaluate result expr, load list, append, store back.
                    self.compile_expr(&lc.result_expr)?;
                    let res_load = self.module.intern_str("__lc_result");
                    self.module.emit(Instr::LoadGlobal(res_load));
                    self.module.emit(Instr::Swap);
                    let append_idx = self.module.intern_str("list_append");
                    self.module.emit(Instr::CallBuiltin(append_idx, 2));
                    // list_append returns the new list — store it back.
                    self.module.emit(Instr::StoreGlobal(result_idx));
                    let end_skip = self.module.code.len();
                    self.module.code[skip] = Instr::JumpIfFalse(end_skip);
                } else {
                    self.compile_expr(&lc.result_expr)?;
                    let res_load = self.module.intern_str("__lc_result");
                    self.module.emit(Instr::LoadGlobal(res_load));
                    self.module.emit(Instr::Swap);
                    let append_idx = self.module.intern_str("list_append");
                    self.module.emit(Instr::CallBuiltin(append_idx, 2));
                    self.module.emit(Instr::StoreGlobal(result_idx));
                }
                self.module.emit(Instr::Jump(loop_start));
                let exit_target = self.module.code.len();
                self.module.code[exit_placeholder] =
                    Instr::IterNext(exit_placeholder + 1, exit_target);
                // Load the result.
                self.module.emit(Instr::LoadGlobal(result_idx));
                Ok(())
            }
            Expr::DictComprehension(dc) => {
                // {k: v for x in iter if cond} → loop with dict accumulation.
                self.module.emit(Instr::NewDict(0));
                let result_idx = self.module.intern_str("__dc_result");
                self.module.emit(Instr::StoreGlobal(result_idx));
                self.compile_expr(&dc.iterable)?;
                self.module.emit(Instr::Iter);
                let loop_start = self.module.code.len();
                let exit_placeholder = self.module.emit(Instr::IterNext(0, 0));
                let var_idx = self.module.intern_str(&dc.var.name);
                self.module.emit(Instr::StoreGlobal(var_idx));
                if let Some(cond) = &dc.condition {
                    self.compile_expr(cond)?;
                    let skip = self.module.emit(Instr::JumpIfFalse(0));
                    self.compile_expr(&dc.key_expr)?;
                    self.compile_expr(&dc.value_expr)?;
                    let res_load = self.module.intern_str("__dc_result");
                    self.module.emit(Instr::LoadGlobal(res_load));
                    let setidx = self.module.intern_str("dict_set");
                    self.module.emit(Instr::CallBuiltin(setidx, 3));
                    self.module.emit(Instr::StoreGlobal(result_idx));
                    let end_skip = self.module.code.len();
                    self.module.code[skip] = Instr::JumpIfFalse(end_skip);
                } else {
                    self.compile_expr(&dc.key_expr)?;
                    self.compile_expr(&dc.value_expr)?;
                    let res_load = self.module.intern_str("__dc_result");
                    self.module.emit(Instr::LoadGlobal(res_load));
                    let setidx = self.module.intern_str("dict_set");
                    self.module.emit(Instr::CallBuiltin(setidx, 3));
                    self.module.emit(Instr::StoreGlobal(result_idx));
                }
                self.module.emit(Instr::Jump(loop_start));
                let exit_target = self.module.code.len();
                self.module.code[exit_placeholder] =
                    Instr::IterNext(exit_placeholder + 1, exit_target);
                self.module.emit(Instr::LoadGlobal(result_idx));
                Ok(())
            }
            Expr::OptionalChain(oc) => {
                // obj?.field / obj?.method(args) / obj?[index]
                //
                // Compile the target once, then lower each link in the chain
                // to a CallBuiltin that performs the null-check internally:
                //   - optional_member(obj, field_name) -> obj.field or null
                //   - optional_method(obj, method_name, args...) -> result or null
                //   - optional_index(obj, idx) -> obj[idx] or null
                // Because each builtin returns null when its receiver is null,
                // a null propagates through the rest of the chain naturally.
                self.compile_expr(&oc.target)?;
                for link in &oc.chain {
                    match link {
                        crate::parser::ast::OptionalChainLink::Member(id) => {
                            let field_idx = self.module.intern_str(&id.name);
                            self.module.emit(Instr::ConstStr(field_idx));
                            let idx = self.module.intern_str("optional_member");
                            self.module.emit(Instr::CallBuiltin(idx, 2));
                        }
                        crate::parser::ast::OptionalChainLink::Call { method, args } => {
                            // Push method name BEFORE args so that after
                            // CallBuiltin's pop-and-reverse, the args Vec
                            // is [obj, method_name, arg1, ..., argN].
                            let method_idx = self.module.intern_str(&method.name);
                            self.module.emit(Instr::ConstStr(method_idx));
                            for a in args {
                                self.compile_expr(a)?;
                            }
                            let idx = self.module.intern_str("optional_method");
                            // argc = obj + method_name + args...
                            self.module.emit(Instr::CallBuiltin(idx, 2 + args.len()));
                        }
                        crate::parser::ast::OptionalChainLink::Index(idx_expr) => {
                            self.compile_expr(idx_expr)?;
                            let bi = self.module.intern_str("optional_index");
                            self.module.emit(Instr::CallBuiltin(bi, 2));
                        }
                    }
                }
                Ok(())
            }
            _ => {
                // Truly unsupported: push null.
                self.module.emit(Instr::ConstNull);
                Ok(())
            }
        }
    }

    /// Compile a string with interpolation parts natively (no EvalAst).
    /// For each Text part: push the string constant.
    /// For each Interpolation part: compile the expression, call str() builtin.
    /// Then fold all parts with Add (string concatenation).
    fn compile_string_interp(&mut self, parts: &[StringPart]) -> Result<()> {
        let mut first = true;
        for p in parts {
            match p {
                StringPart::Text(t) => {
                    let text = interpret_escapes(t);
                    let idx = self.module.intern_str(&text);
                    self.module.emit(Instr::ConstStr(idx));
                }
                StringPart::Interpolation(expr) => {
                    self.compile_expr(expr)?;
                    // Convert to string via str() builtin.
                    let str_idx = self.module.intern_str("str");
                    self.module.emit(Instr::CallBuiltin(str_idx, 1));
                }
            }
            if !first {
                // Concatenate with the previous value on the stack.
                self.module.emit(Instr::Add);
            }
            first = false;
        }
        // If there were no parts, push empty string.
        if first {
            let idx = self.module.intern_str("");
            self.module.emit(Instr::ConstStr(idx));
        }
        Ok(())
    }

    fn string_parts_text(&self, parts: &[StringPart]) -> String {
        let mut text = String::new();
        for p in parts {
            match p {
                StringPart::Text(t) => text.push_str(&interpret_escapes(t)),
                StringPart::Interpolation(e) => {
                    // For interpolation in bytecode VM, we emit a placeholder.
                    // Full interpolation requires runtime concat support.
                    text.push_str(&format!("{:?}", e));
                }
            }
        }
        text
    }

    fn compile_binary(&mut self, b: &BinaryExpr) -> Result<()> {
        // Short-circuit and/or.
        match b.operator {
            BinaryOp::And => {
                // Short-circuit and: if left is falsy, result is false;
                // otherwise result is right.truthy().
                self.compile_expr(&b.left)?;
                let jmp_false = self.module.emit(Instr::JumpIfFalse(0));
                // Left was popped by JumpIfFalse. If we get here, left was truthy.
                self.compile_expr(&b.right)?;
                // Convert right to bool.
                let str_idx = self.module.intern_str("bool");
                self.module.emit(Instr::CallBuiltin(str_idx, 1));
                let end = self.module.emit(Instr::Jump(0));
                // False branch: push false.
                let false_target = self.module.code.len();
                self.module.code[jmp_false] = Instr::JumpIfFalse(false_target);
                self.module.emit(Instr::ConstBool(false));
                let end_target = self.module.code.len();
                self.module.code[end] = Instr::Jump(end_target);
                Ok(())
            }
            BinaryOp::Or => {
                // Short-circuit or: if left is truthy, result is true;
                // otherwise result is right.truthy().
                self.compile_expr(&b.left)?;
                // Dup so we can check truthiness without losing the value.
                self.module.emit(Instr::Dup);
                let jmp_true = self.module.emit(Instr::JumpIfTrue(0));
                // Left was falsy (popped by JumpIfTrue popping the dup).
                // Pop the original left.
                self.module.emit(Instr::Pop);
                self.compile_expr(&b.right)?;
                let str_idx = self.module.intern_str("bool");
                self.module.emit(Instr::CallBuiltin(str_idx, 1));
                let end = self.module.emit(Instr::Jump(0));
                // True branch: pop the dup'd left, push true.
                let true_target = self.module.code.len();
                self.module.code[jmp_true] = Instr::JumpIfTrue(true_target);
                self.module.emit(Instr::Pop);
                self.module.emit(Instr::ConstBool(true));
                let end_target = self.module.code.len();
                self.module.code[end] = Instr::Jump(end_target);
                Ok(())
            }
            _ => {
                self.compile_expr(&b.left)?;
                self.compile_expr(&b.right)?;
                let instr = match b.operator {
                    BinaryOp::Add => Instr::Add,
                    BinaryOp::Sub => Instr::Sub,
                    BinaryOp::Mul => Instr::Mul,
                    BinaryOp::Div => Instr::Div,
                    BinaryOp::FloorDiv => Instr::FloorDiv,
                    BinaryOp::Mod => Instr::Mod,
                    BinaryOp::Eq => Instr::Eq,
                    BinaryOp::Ne => Instr::Ne,
                    BinaryOp::Lt => Instr::Lt,
                    BinaryOp::Gt => Instr::Gt,
                    BinaryOp::Le => Instr::Le,
                    BinaryOp::Ge => Instr::Ge,
                    BinaryOp::In | BinaryOp::Is | BinaryOp::Power | BinaryOp::Repeated => {
                        // These ops need special handling — use CallBuiltin.
                        // For `in`: left in right → contains(right, left)
                        // so we push right first (haystack), then left (needle).
                        if b.operator == BinaryOp::In {
                            self.compile_expr(&b.right)?; // haystack
                            self.compile_expr(&b.left)?;  // needle
                            let idx = self.module.intern_str("contains");
                            self.module.emit(Instr::CallBuiltin(idx, 2));
                        } else {
                            self.compile_expr(&b.left)?;
                            self.compile_expr(&b.right)?;
                            let builtin_name = match b.operator {
                                BinaryOp::Is => "is_type",
                                BinaryOp::Power => "pow_op",
                                BinaryOp::Repeated => "__repeated__",
                                _ => return Ok(Default::default()),
                            };
                            let idx = self.module.intern_str(builtin_name);
                            self.module.emit(Instr::CallBuiltin(idx, 2));
                        }
                        return Ok(());
                    }
                    _ => {
                        return Err(CompilerError::codegen_error(format!(
                            "bytecode: binary op {:?} not supported",
                            b.operator
                        )))
                    }
                };
                self.module.emit(instr);
                Ok(())
            }
        }
    }

    fn compile_unary(&mut self, u: &UnaryExpr) -> Result<()> {
        self.compile_expr(&u.operand)?;
        let instr = match u.operator {
            UnaryOp::Neg => Instr::Neg,
            UnaryOp::Not => Instr::Not,
            UnaryOp::Bang => Instr::Not,
        };
        self.module.emit(instr);
        Ok(())
    }

    fn compile_call(&mut self, c: &CallExpr) -> Result<()> {
        // Check for method calls (parsed as Call with MemberAccess callee).
        if let Expr::MemberAccess(m) = c.callee.as_ref() {
            // super.method(args) → SuperCall.
            if let Expr::Identifier(id) = m.target.as_ref() {
                if id.name == "super" {
                    for a in &c.args {
                        self.compile_expr(a)?;
                    }
                    let method_idx = self.module.intern_str(&m.member.name);
                    self.module.emit(Instr::SuperCall(method_idx, c.args.len()));
                    return Ok(());
                }
            }
            // receiver.method(args)
            self.compile_expr(&m.target)?;
            for a in &c.args {
                self.compile_expr(a)?;
            }
            let method_idx = self.module.intern_str(&m.member.name);
            self.module.emit(Instr::CallMethod(method_idx, c.args.len()));
            return Ok(());
        }
        if let Expr::Identifier(id) = c.callee.as_ref() {
            // Check if it's a builtin.
            if is_builtin(&id.name) {
                for a in &c.args {
                    self.compile_expr(a)?;
                }
                let idx = self.module.intern_str(&id.name);
                self.module.emit(Instr::CallBuiltin(idx, c.args.len()));
                return Ok(());
            }
            // Check if it's a local variable holding a closure/function.
            if let Some(slot) = self.locals.get(&id.name) {
                // Indirect call: load the local (Func value), then Call.
                self.module.emit(Instr::LoadLocal(*slot));
                for a in &c.args {
                    self.compile_expr(a)?;
                }
                self.module.emit(Instr::Call(c.args.len()));
                return Ok(());
            }
            // User function call by name.
            for a in &c.args {
                self.compile_expr(a)?;
            }
            let idx = self.module.intern_str(&id.name);
            self.module.emit(Instr::CallByName(idx, c.args.len()));
            return Ok(());
        }
        // Indirect call: compile callee, then call.
        self.compile_expr(&c.callee)?;
        for a in &c.args {
            self.compile_expr(a)?;
        }
        self.module.emit(Instr::Call(c.args.len()));
        Ok(())
    }
}

impl Default for Compiler {
    fn default() -> Self {
        Self::new()
    }
}

/// Interpret C-style escape sequences.
fn interpret_escapes(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Check if a name is a builtin function.
pub fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "len"
            | "str"
            | "int"
            | "float"
            | "bool"
            | "ord"
            | "chr"
            | "type_of"
            | "range"
            | "range3"
            | "range1"
            | "sum"
            | "min"
            | "max"
            | "sorted"
            | "reversed"
            | "print"
            | "println"
            | "paste"
            | "input"
            | "set_add"
            | "set_remove"
            | "set_contains"
            | "set_size"
            | "enumerate"
            | "open"
            | "read"
            | "write"
            | "close"
            | "read_file"
            | "write_file"
            | "file_exists"
            | "exit"
            | "enumerate"
            | "zip"
            | "resume"
            | "stop"
            | "Ok"
            | "Err"
            | "is_ok"
            | "is_err"
            | "unwrap"
            | "unwrap_or"
            | "freeze"
            | "is_frozen"
            | "annotations"
            | "set_recursion_limit"
            | "abs"
            | "floor"
            | "ceil"
            | "round"
            | "map"
            | "filter"
            | "dict"
            | "dict_get"
            | "dict_set"
            | "dict_keys"
            | "dict_values"
            | "dict_has"
            | "list"
            | "set"
            | "tuple"
            | "is_dir"
            | "is_file"
            | "read_dir"
            | "path_join"
            | "basename"
            | "dirname"
            | "split"
            | "join"
            | "trim"
            | "upper"
            | "lower"
            | "contains"
            | "list_append"
            | "get_field"
            | "slice"
            | "sorted_desc"
            | "optional_member"
            | "optional_method"
            | "optional_index"
            | "__yield__"
            | "__with_enter__"
            | "__with_exit__"
            | "__try_propagate__"
            | "__cast__"
            | "__repeated__"
            | "__assert_fail"
            | "__panic__"
            | "open"
            | "read"
            | "write"
            | "close"
            | "math_sqrt"
            | "math_pow"
            | "math_sin"
            | "math_cos"
            | "math_tan"
            | "math_log"
            | "math_abs"
            | "math_floor"
            | "math_ceil"
            | "math_cbrt"
            | "math_fabs"
            | "math_trunc"
            | "math_round"
            | "math_exp"
            | "math_log10"
            | "math_log2"
            | "math_asin"
            | "math_acos"
            | "math_atan"
            | "math_atan2"
            | "math_sinh"
            | "math_cosh"
            | "math_tanh"
            | "math_radians"
            | "math_degrees"
            | "math_gamma"
            | "math_lgamma"
            | "math_inf"
            | "math_nan"
            | "fmt_sprintf"
            | "fmt_fprintf"
            | "path_join"
            | "path_dirname"
            | "path_basename"
            | "path_ext"
            | "path_exists"
            | "path_is_abs"
            | "path_abs"
            | "base64_encode"
            | "base64_decode"
            | "hex_encode"
            | "hex_decode"
            | "url_encode"
            | "url_decode"
            | "crypto_sha256"
            | "crypto_md5"
            | "crypto_sha1"
            | "regex_compile"
            | "regex_match"
            | "regex_search"
            | "regex_find_all"
            | "regex_findall"
            | "regex_sub"
            | "debug_inspect"
            | "debug_trace"
            | "debug_timeit"
            | "debug_dump"
            | "debug_backtrace"
            | "flag_string"
            | "flag_int"
            | "flag_bool"
            | "flag_parse"
            | "flag_args"
            | "log_debug"
            | "log_info"
            | "log_warn"
            | "log_error"
            | "log_set_level"
            | "log_set_format"
            | "term_clear"
            | "term_move_cursor"
            | "term_set_color"
            | "term_reset"
            | "term_read_key"
            | "term_get_size"
            | "compress_gzip_encode"
            | "compress_gzip_decode"
            | "compress_zlib_encode"
            | "compress_zlib_decode"
            | "compress_flate_encode"
            | "compress_flate_decode"
            | "net_dial"
            | "net_listen"
            | "http_get"
            | "http_post"
            | "sql_open"
            | "sql_drivers"
            | "csv_read"
            | "csv_write"
            | "xml_parse"
            | "xml_stringify"
            | "toml_parse"
            | "toml_stringify"
            | "yaml_parse"
            | "yaml_stringify"
            | "sync_spawn"
            | "sync_channel"
            | "sync_send"
            | "sync_receive"
            | "sync_close"
            | "sync_mutex"
            | "sync_waitgroup"
            | "os_args"
            | "os_exit"
            | "os_get_env"
            | "os_set_env"
            | "os_unset_env"
            | "os_exec"
            | "os_system"
            | "os_getwd"
            | "os_cwd"
            | "os_chdir"
            | "os_mkdir"
            | "os_remove"
            | "os_rename"
            | "os_stat"
            | "fs_walk"
            | "fs_copy"
            | "fs_move"
            | "fs_remove_all"
            | "fs_temp_dir"
            | "fs_temp_file"
            | "io_read"
            | "io_write"
            | "io_append"
            | "rand_seed"
            | "rand_choice"
            | "rand_shuffle"
            | "rand_string"
            | "json_stringify_pretty"
            | "printf"
            | "image_load"
            | "image_save"
            | "image_new"
            | "image_create"
            | "image_resize"
            | "image_crop"
            | "machine_gpio_pin"
            | "machine_i2c_init"
            | "machine_spi_init"
            | "machine_serial_open"
            | "unsafe_sizeof"
            | "unsafe_alignof"
            | "unsafe_offsetof"
            | "unsafe_cast"
            | "unsafe_alloc"
            | "unsafe_free"
            | "embed_fs"
            | "embed_read"
            | "crypto_aes_encrypt"
            | "crypto_aes_decrypt"
            | "crypto_bcrypt_hash"
            | "crypto_bcrypt_verify"
            | "crypto_password_hash"
            | "crypto_password_verify"
            | "websocket_connect"
            | "atomic_load_int"
            | "atomic_store_int"
            | "atomic_add_int"
            | "atomic_cas_int"
    )
}
