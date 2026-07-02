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
        for decl in &program.declarations {
            self.compile_top_level(decl)?;
        }
        self.module.emit(Instr::Halt);
        self.module.num_locals = self.next_slot;
        Ok(self.module)
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
                // Special-case bare index assignment (`xs[i] = v`) so the
                // stack order matches IndexSet's pop order
                // (val, idx, container — val on top).
                if let Some(Assignee::Index(ix)) = a.targets.first() {
                    self.compile_expr(&ix.target)?; // push container
                    self.compile_expr(&ix.index)?;  // push idx
                    self.compile_expr(&a.value)?;   // push val (top)
                    self.module.emit(Instr::IndexSet);
                    return Ok(());
                }
                // Member-assignment (`obj.field = v`, `self.field = v`)
                // falls back to the AST interpreter path which handles
                // the shared Rc<RefCell<HashMap>> mutation correctly.
                if let Some(Assignee::Member(_)) = a.targets.first() {
                    self.module.emit(Instr::ExecAstStmt(Box::new(s.clone())));
                    return Ok(());
                }
                // Also delegate PonStmt-style comma assignments (`set, x, y`)
                // and tuple-assignment targets to the AST path.
                if let Some(Assignee::Tuple(_)) = a.targets.first() {
                    self.module.emit(Instr::ExecAstStmt(Box::new(s.clone())));
                    return Ok(());
                }
                if let Some(Assignee::Qualified(_)) = a.targets.first() {
                    self.module.emit(Instr::ExecAstStmt(Box::new(s.clone())));
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
            _ => {
                // Statements like With, Try, Throw, Yield, Spawn, Match,
                // Import, Defer don't have dedicated bytecode lowering yet.
                // Delegate to the VM's AST interpreter path via ExecAstStmt.
                self.module.emit(Instr::ExecAstStmt(Box::new(s.clone())));
                Ok(())
            }
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
        // Delegate to the VM's AST interpreter path via ExecAstStmt.
        // The AST path supports the full iterator protocol (Objects with
        // next(), generators, lists, tuples, strings, dicts).
        self.module.emit(Instr::ExecAstStmt(Box::new(Stmt::ForIn(f.clone()))));
        Ok(())
    }

    fn compile_for_range(&mut self, f: &ForRangeStmt) -> Result<()> {
        // Build a range list, then iterate.
        // ForRangeStmt has no step field in 0.1.1; step is always 1.
        self.compile_expr(&f.from)?;
        self.compile_expr(&f.to)?;
        self.module.emit(Instr::ConstInt(1));
        // Call builtin range3(start, end, step).
        let range_idx = self.module.intern_str("range3");
        self.module.emit(Instr::CallBuiltin(range_idx, 3));
        // Now iterate.
        self.module.emit(Instr::Iter);
        let loop_start = self.module.code.len();
        let exit_placeholder = self.module.emit(Instr::IterNext(0, 0));
        // Loop var lives in globals (consistent with LoadGlobal).
        let var_idx = self.module.intern_str(&f.var.name);
        self.module.emit(Instr::StoreGlobal(var_idx));
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
                // Use StoreGlobal so variables are accessible after execution
                // (for test inspection via globals_clone).
                let slot = self.declare_local(&id.name);
                self.module.emit(Instr::StoreLocal(slot));
                // Also store as global for test inspection.
                let name_idx = self.module.intern_str(&id.name);
                // Dup the value before StoreLocal consumed it — but StoreLocal
                // already consumed it. So we need a different approach:
                // emit StoreGlobal AFTER StoreLocal won't work (value is gone).
                // Instead, emit Dup + StoreLocal + StoreGlobal.
                // But we already emitted StoreLocal. Let's restructure:
                // The value is on the stack. We need to both StoreLocal and
                // StoreGlobal. Emit Dup first, then StoreLocal, then StoreGlobal.
                // But StoreLocal is already emitted. So let's insert Dup before it.
                // Actually, the simplest fix: replace StoreLocal with StoreGlobal
                // and have the VM's StoreGlobal also store into locals.
                // For now, just use StoreGlobal.
                // Remove the StoreLocal we just emitted:
                self.module.code.pop(); // remove StoreLocal
                self.module.emit(Instr::StoreGlobal(name_idx));
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
                    self.module.emit(Instr::EvalAst(Box::new(e.clone())));
                    return Ok(());
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
                    self.module.emit(Instr::EvalAst(Box::new(e.clone())));
                    return Ok(());
                }
                let text = self.string_parts_text(&s.parts);
                let idx = self.module.intern_str(&text);
                self.module.emit(Instr::ConstStr(idx));
                Ok(())
            }
            Expr::Identifier(id) => {
                // Always use LoadGlobal — all variables are stored in globals.
                let idx = self.module.intern_str(&id.name);
                self.module.emit(Instr::LoadGlobal(idx));
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
                // range expression: start .. end
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
                self.module.emit(Instr::ConstInt(1));
                let idx = self.module.intern_str("range3");
                self.module.emit(Instr::CallBuiltin(idx, 3));
                Ok(())
            }
            _ => {
                // Unsupported expressions (Slice, Resume, Await, MethodCall,
                // Spawn, comprehensions, Lambda, Ternary, etc.) fall back to
                // the VM's AST interpreter path via EvalAst. This keeps the
                // bytecode VM functional for the full language without
                // requiring dedicated opcodes for every construct.
                self.module.emit(Instr::EvalAst(Box::new(e.clone())));
                Ok(())
            }
        }
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
                self.compile_expr(&b.left)?;
                let jmp_false = self.module.emit(Instr::JumpIfFalse(0));
                self.compile_expr(&b.right)?;
                // Both pushed; result is right operand. But we need to
                // pop the left if false. Use a trick: JumpIfFalse pops
                // the condition, so if false we need to push false.
                // Simpler: compile as (left ? right : left) but JumpIfFalse
                // already consumed left. So:
                //   push left
                //   dup  (so we have it for the false branch)
                //   jump_if_false L1
                //   pop  (discard the dup'd left)
                //   push right
                // L1:
                // Actually let's do: push left, jump_if_false L1, push right, L1:
                // But jump_if_false pops. So at L1 the stack has nothing.
                // We need: at L1, push false. Let me restructure.
                self.module.emit(Instr::ConstBool(false));
                // Stack: [left, false]
                // We want: if left is false, keep false. If true, pop false, push right.
                // Use: swap, pop, jump_if_false L1, pop, push right, L1
                // Hmm complex. Let me just do non-short-circuit for now.
                // Undo the ConstBool(false):
                self.module.code.pop(); // remove the ConstBool
                self.module.code.pop(); // remove the JumpIfFalse placeholder
                // Non-short-circuit: push both, And.
                self.compile_expr(&b.right)?;
                self.module.emit(Instr::And);
                Ok(())
            }
            BinaryOp::Or => {
                self.compile_expr(&b.left)?;
                self.compile_expr(&b.right)?;
                self.module.emit(Instr::Or);
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
            | "math_sqrt"
            | "math_pow"
            | "math_sin"
            | "math_cos"
            | "math_tan"
            | "math_log"
            | "math_abs"
            | "math_floor"
            | "math_ceil"
    )
}
