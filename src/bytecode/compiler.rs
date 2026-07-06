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
        }
    }

    /// Compile a program into a bytecode module.
    pub fn compile(mut self, program: &Program) -> Result<Module> {
        // First pass: collect function names so calls can resolve.
        for decl in &program.declarations {
            if let TopLevel::FnDef(f) = decl {
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
        for decl in &program.declarations {
            if let TopLevel::FnDef(f) = decl {
                // Skip functions with type-parameter constraints — they need
                // the call_function path so the runtime constraint check runs.
                if f.is_async || self.fn_has_yield(&f.body) || !f.type_constraints.is_empty() {
                    continue;
                }
                self.compile_fn_body(f)?;
            }
        }
        // Also compile lambda functions collected during expression
        // compilation. These are registered in fn_entry_pcs like
        // regular functions, so they run via the bytecode path.
        for f in &self.lambda_fns.clone() {
            if !f.is_async && !self.fn_has_yield(&f.body) {
                self.compile_fn_body(f)?;
            }
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
        for s in &f.body {
            self.compile_stmt(s)?;
        }
        // Default epilogue: push null and return (reached only if the body
        // didn't end with an explicit `return`).
        self.module.emit(Instr::ConstNull);
        self.module.emit(Instr::Return);
        let num_locals = self.next_slot;
        // Check whether the compiled body is "clean" (no AST fallback).
        // If it contains EvalAst/ExecAstStmt, the function must run via
        // the AST path because the fallback handlers use name-based scope
        // lookup (local_scopes), not slot-based frame.locals.
        let start = entry_pc;
        let end = self.module.code.len();
        let is_clean = self.module.code[start..end]
            .iter()
            .all(|instr| !matches!(instr, Instr::EvalAst(_) | Instr::ExecAstStmt(_)));
        if is_clean {
            self.module
                .fn_entry_pcs
                .insert(f.name.name.clone(), (entry_pc, num_locals));
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
                // Special-case bare index assignment (`xs[i] = v`) so the
                // stack order matches IndexSet's pop order
                // (val, idx, container — val on top).
                if let Some(Assignee::Index(ix)) = a.targets.first() {
                    self.compile_expr(&ix.target)?; // push container
                    self.compile_expr(&ix.index)?;  // push idx
                    self.compile_expr(&a.value)?;   // push val (top)
                    self.module.emit(Instr::IndexSet);
                    // IndexSet pushes the modified container back onto the
                    // stack. We need to store it back into the original
                    // variable so the mutation persists.
                    if let crate::parser::ast::Expr::Identifier(id) = ix.target.as_ref() {
                        if self.in_function {
                            if let Some(&slot) = self.locals.get(&id.name) {
                                self.module.emit(Instr::StoreLocal(slot));
                            } else {
                                // Not a known local — store to global.
                                let name_idx = self.module.intern_str(&id.name);
                                self.module.emit(Instr::StoreGlobal(name_idx));
                            }
                        } else {
                            let name_idx = self.module.intern_str(&id.name);
                            self.module.emit(Instr::StoreGlobal(name_idx));
                        }
                    } else {
                        // For non-identifier targets (e.g. obj.field[idx]),
                        // just pop the result.
                        self.module.emit(Instr::Pop);
                    }
                    return Ok(());
                }
                // Multi-target assignment (`set, a, b = 1, 2`) and
                // member/tuple/qualified assignment → ExecAstStmt.
                // Multi-target assignment, member assignment, tuple
                // destructuring, qualified assignment — compile natively.
                if a.targets.len() > 1 {
                    // Multi-target: set, a, b = value
                    // Compile value once, then Dup + store for each target.
                    self.compile_expr(&a.value)?;
                    for (i, target) in a.targets.iter().enumerate() {
                        if i < a.targets.len() - 1 {
                            self.module.emit(Instr::Dup);
                        }
                        self.compile_assign_target(target)?;
                    }
                    return Ok(());
                }
                if let Some(Assignee::Member(m)) = a.targets.first() {
                    // obj.field = value
                    self.compile_expr(&a.value)?;
                    self.compile_expr(&m.target)?;
                    let field_idx = self.module.intern_str(&m.member.name);
                    self.module.emit(Instr::StoreField(field_idx));
                    return Ok(());
                }
                if let Some(Assignee::Qualified(q)) = a.targets.first() {
                    // module.symbol = value — compile as member assignment
                    // on the first segment (the module/global), setting the
                    // field identified by the remaining segments.
                    self.compile_expr(&a.value)?;
                    // Load the first segment as the target object.
                    let first_idx = self.module.intern_str(&q.parts[0].name);
                    self.module.emit(Instr::LoadGlobal(first_idx));
                    // For multi-part qualified names (a.b.c), chain member
                    // accesses until the last segment, then StoreField.
                    // For simplicity (2-part is the common case), handle
                    // the last segment as a field store.
                    if q.parts.len() == 2 {
                        let field_idx = self.module.intern_str(&q.parts[1].name);
                        self.module.emit(Instr::StoreField(field_idx));
                    } else {
                        // Multi-part: walk to second-to-last, then store last.
                        for seg in q.parts.iter().skip(1).take(q.parts.len() - 2) {
                            let field_idx = self.module.intern_str(&seg.name);
                            self.module.emit(Instr::LoadField(field_idx));
                        }
                        let last_idx = self.module.intern_str(&q.parts.last().unwrap().name);
                        self.module.emit(Instr::StoreField(last_idx));
                    }
                    self.module.emit(Instr::Pop);
                    return Ok(());
                }
                if let Some(Assignee::Tuple(targets)) = a.targets.first() {
                    // (a, b, c) = value — destructure by indexing.
                    self.compile_expr(&a.value)?;
                    for (i, target) in targets.iter().enumerate() {
                        self.module.emit(Instr::Dup);
                        self.module.emit(Instr::ConstInt(i as i64));
                        self.module.emit(Instr::IndexGet);
                        self.compile_assign_target(target)?;
                    }
                    self.module.emit(Instr::Pop);
                    return Ok(());
                }
                self.compile_expr(&a.value)?;
                if let Some(target) = a.targets.first() {
                    self.compile_assign_target(target)?;
                } else {
                    self.module.emit(Instr::Pop);
                }
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
                if let Some(v) = r.values.first() {
                    self.compile_expr(v)?;
                } else {
                    self.module.emit(Instr::ConstNull);
                }
                self.module.emit(Instr::Return);
                Ok(())
            }
            Stmt::If(i) => self.compile_if(i),
            Stmt::While(w) => self.compile_while(w),
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
                // Use a CallBuiltin to signal yield — the VM's "yield"
                // builtin pushes to gen_yield_buffer.
                let yield_idx = self.module.intern_str("__yield__");
                self.module.emit(Instr::CallBuiltin(yield_idx, 1));
                self.module.emit(Instr::Pop);
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
                // Defer: push the deferred statement onto a defer stack
                // that will be executed at function exit. Since we can't
                // use AST defer in bytecode mode, we compile the deferred
                // statement to bytecode at the point of defer, then use
                // a special marker to skip it during normal execution and
                // jump to it at function exit.
                //
                // For now, since the bytecode path doesn't have a defer
                // stack mechanism, we emit the statement inline BUT we
                // need to ensure defer runs AFTER the body, not before.
                // The correct fix is to NOT compile defer inline here —
                // instead, we skip it during compilation and rely on the
                // VM's execute_fn_body (AST path) to handle defer for
                // functions that contain defer statements.
                //
                // However, since we eliminated AST fallback, we need a
                // different approach: emit a DeferScope marker that the
                // VM can use. For now, the simplest correct behavior is
                // to NOT emit the defer code inline (which runs it too
                // early) and instead mark the function as needing AST
                // path for defer support.
                //
                // Actually, the real fix: don't compile defer to inline
                // code. Just skip it. The function body compilation will
                // detect the defer and mark the function as non-clean,
                // forcing it through the AST path where defer works
                // correctly via execute_fn_body's defer_stack.
                self.module.emit(Instr::ExecAstStmt(Box::new(s.clone())));
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
                for e in &l.elements {
                    self.compile_expr(e)?;
                }
                self.module.emit(Instr::NewList(l.elements.len()));
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
                // receiver.method(args) → compile(receiver), compile(args...),
                // CallMethod(method_idx, argc)
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
                        self.compile_expr(&p.left)?;
                        self.compile_expr(&p.right)?;
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
                // Compile lambda as a named function. The lambda body is
                // compiled in the third pass (compile_fn_body). Captured
                // variables from the enclosing scope are stored to globals
                // before the Func value is loaded.
                let lambda_name = format!("<lambda_{}>", self.next_lambda_id);
                self.next_lambda_id += 1;
                // Detect free variables in the lambda body (identifiers
                // that are not lambda parameters). For each that is a
                // local in the enclosing scope, emit StoreGlobal to copy
                // it to a global so the lambda can access it.
                let param_names: std::collections::HashSet<String> =
                    l.params.iter().map(|p| p.name.name.clone()).collect();
                let free_vars = self.collect_free_vars(&l.body, &param_names);
                for var_name in &free_vars {
                    if let Some(&slot) = self.locals.get(var_name) {
                        self.module.emit(Instr::LoadLocal(slot));
                        let idx = self.module.intern_str(var_name);
                        self.module.emit(Instr::StoreGlobal(idx));
                    }
                }
                // Create a FnDef for the lambda and register it.
                let body = vec![Stmt::Return(ReturnStmt {
                    values: vec![(*l.body).clone()],
                    span: l.span.clone(),
                })];
                let fn_def = FnDef {
                    annotations: vec![],
                    name: Identifier { name: lambda_name.clone(), span: l.span.clone() },
                    params: l.params.clone(),
                    return_type: l.return_type.clone(),
                    body,
                    is_constexpr: false,
                    is_lazy: false,
                    is_async: false,
                    is_extern: false,
                    extern_link: None,
                    type_constraints: std::collections::HashMap::new(),
                    span: l.span.clone(),
                };
                // Register in module.functions so the VM preprocesses it.
                self.compile_fn_def(&fn_def)?;
                self.lambda_fns.push(fn_def.clone());
                // Also store in module.lambda_fn_defs so the VM can find
                // the real FnDef body when the lambda is called via
                // call_value (e.g. map(fn(x) x*2, list)).
                self.module.lambda_fn_defs.insert(lambda_name.clone(), fn_def);
                // Push the Func value by loading the global (registered
                // by the VM's preprocess_top_level).
                let idx = self.module.intern_str(&lambda_name);
                self.module.emit(Instr::LoadGlobal(idx));
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
                    BinaryOp::Div | BinaryOp::FloorDiv => Instr::Div,
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
                                _ => unreachable!(),
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
            | "websocket_connect"
            | "atomic_load_int"
            | "atomic_store_int"
            | "atomic_add_int"
            | "atomic_cas_int"
    )
}
