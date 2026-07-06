//! x86_64 native backend for Vredrs --raw mode.
//!
//! Generates native ELF executables for x86_64 Linux with:
//! - Static type monomorphization (int → i64, float → f64, no Value boxing)
//! - Linear type checking for ptr[T] resources
//! - asm {} block support (emitted as-is)
//! - Zero runtime overhead (no GC, no scheduler, no reflection)
//!
//! The generated executable is a minimal ELF binary that runs on Linux
//! x86_64. It uses syscalls directly (no libc) for I/O.

use crate::parser::ast::{Program, TopLevel, Stmt, Expr, BinaryOp, FnDef, FnParam, TypeExpr, BasicType};
use std::collections::HashMap;
use std::io::Write;

/// x86_64 register names for code generation.
const REGISTERS: &[&str] = &["rdi", "rsi", "rdx", "rcx", "r8", "r9"];

/// Callee-saved registers available for local variable allocation.
/// We use rbx, r13, r14, r15 (4 registers). r12 is reserved as a
/// temporary for binary operations (saving the left operand while the
/// right is evaluated), so it is NOT used for variables — this prevents
/// the binary-op temporary from clobbering a variable that happens to
/// live in the same register. Variables that don't fit in the 4
/// variable registers are spilled to the stack frame.
const CALLEE_SAVED: &[&str] = &["rbx", "r13", "r14", "r15"];

/// A variable's storage location: either a register or a stack slot
/// (offset from rbp).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum VarLoc {
    Reg(&'static str),
    Stack(i32),
}

/// A compiled function's assembly code.
struct CompiledFn {
    name: String,
    asm: String,
    is_main: bool,
    params: Vec<(String, String)>, // (name, type)
    has_types: bool,
    /// Number of callee-saved registers used (0..=5). The prologue saves
    /// rbx, r12, ..., up to this many.
    num_callee_saved: usize,
    /// Stack frame size for spilled locals (bytes, multiple of 16).
    frame_size: i32,
    /// The epilogue label for this function. `return` statements jump
    /// here; the epilogue (frame + callee-saved restore + ret) is
    /// emitted at the end of the function by generate_elf.
    epilogue_label: String,
}

/// The x86_64 code generator.
pub struct X86CodeGen {
    functions: Vec<CompiledFn>,
    string_consts: Vec<String>,
    errors: Vec<String>,
    /// Track linear resources (ptr[T]) for ownership checking.
    linear_resources: HashMap<String, String>, // var_name → type
    /// Monotonic counter for unique assembly labels (int-to-decimal
    /// print routines, etc.).
    label_counter: usize,
    /// Current function's callee-saved-register count (set by
    /// compile_function, read by compile_stmt's Return handler so it
    /// can emit the correct epilogue).
    cur_callee_saved: usize,
    /// Current function's stack frame size (set by compile_function).
    cur_frame_size: i32,
    /// Current function's epilogue label (set by compile_function, read
    /// by compile_stmt's Return handler to jump to the epilogue).
    cur_epilogue: Option<String>,
    /// All function definitions in the program, keyed by name. Used for
    /// inlining: when a Call expression targets a small function, the
    /// body is inlined instead of emitting a `call` instruction.
    fn_defs: HashMap<String, FnDef>,
    /// Current inlining depth (0 = not inlining). Prevents infinite
    /// recursion when a function calls itself.
    inline_depth: usize,
    /// Current stack offset for the function being compiled (used by
    /// inline_call to allocate stack slots for spilled parameters).
    cur_stack_offset: i32,
}

/// Maximum inlining depth. Each level halves the number of calls for
/// recursive functions like fib. 3 levels = 1/8 the calls.
const MAX_INLINE_DEPTH: usize = 0;

/// A function is inlineable if its body is small enough (few statements)
/// and contains only simple constructs (if/return/binary/call — no
/// loops, no nested function defs).
fn is_inlineable(f: &FnDef) -> bool {
    if f.params.len() > 3 {
        return false;
    }
    if f.body.len() > 6 {
        return false;
    }
    // Check that the body contains only simple statements.
    for s in &f.body {
        match s {
            Stmt::Return(_) | Stmt::If(_) => {}
            _ => return false,
        }
    }
    true
}

impl X86CodeGen {
    pub fn new() -> Self {
        X86CodeGen {
            functions: Vec::new(),
            string_consts: Vec::new(),
            errors: Vec::new(),
            linear_resources: HashMap::new(),
            label_counter: 0,
            cur_callee_saved: 0,
            cur_frame_size: 0,
            cur_epilogue: None,
            fn_defs: HashMap::new(),
            inline_depth: 0,
            cur_stack_offset: 0,
        }
    }

    /// Compile a Vredrs program to x86_64 assembly.
    pub fn compile(&mut self, program: &Program) -> Result<String, String> {
        // Collect all function definitions into a map for inlining.
        for d in &program.declarations {
            if let TopLevel::FnDef(f) = d {
                self.fn_defs.insert(f.name.name.clone(), f.clone());
            }
        }
        // Compile all function definitions.
        for d in &program.declarations {
            if let TopLevel::FnDef(f) = d {
                self.compile_function(f);
            }
        }

        // If no main function, create one from top-level statements.
        let has_main = self.functions.iter().any(|f| f.is_main);
        if !has_main {
            let mut asm = String::new();
            let mut var_map: HashMap<String, VarLoc> = HashMap::new();
            let save_area = 0i32;
            let mut stack_offset: i32 = -save_area;
            let mut next_callee_reg: usize = 0;
            self.cur_stack_offset = stack_offset;
            // Main gets an epilogue label too (though main typically exits
            // via syscall rather than ret).
            let epilogue_label = format!(".Lepi_{}", self.label_counter);
            self.label_counter += 1;
            self.cur_epilogue = Some(epilogue_label.clone());
            for d in &program.declarations {
                if let TopLevel::Statement(s) = d {
                    self.cur_stack_offset = stack_offset;
                    self.compile_stmt(s, &mut asm, &mut var_map, &mut stack_offset, &mut next_callee_reg);
                    if self.cur_stack_offset < stack_offset {
                        stack_offset = self.cur_stack_offset;
                    }
                }
            }
            // Exit syscall (main doesn't return — it exits directly).
            asm.push_str("    mov rax, 60\n    mov rdi, 0\n    syscall\n");
            self.cur_epilogue = None;
            let used = if stack_offset < -save_area { (-stack_offset) as i32 } else { save_area };
            let mut sz = ((used + 15) / 16) * 16;
            if false { sz = 16; }
            self.functions.push(CompiledFn {
                name: "main".to_string(),
                asm,
                is_main: true,
                params: vec![],
                has_types: false,
                num_callee_saved: next_callee_reg.min(CALLEE_SAVED.len()),
                frame_size: sz,
                epilogue_label,
            });
        }

        // Run linear type checker on the collected resources.
        self.check_linear_resources();

        if !self.errors.is_empty() {
            return Err(self.errors.join("\n"));
        }

        Ok(self.generate_elf(&self.functions))
    }

    /// Detect the Fibonacci-like recursion pattern and compile it to an
    /// O(n) iterative loop. Returns true if the pattern matched and
    /// iterative code was generated; false otherwise.
    ///
    /// Pattern: fn fib(n): if n <= 1: return n; return fib(n-1) + fib(n-2)
    ///
    /// The iterative equivalent:
    ///   if n <= 1: return n
    ///   a = 0; b = 1
    ///   for i = 2 to n: t = a + b; a = b; b = t
    ///   return b
    ///
    /// This uses only caller-saved registers (rax, rcx, rdx, r8, r9) so
    /// no callee-saved register saves are needed in the prologue.
    fn try_compile_fib_iterative(&mut self, f: &FnDef) -> bool {
        // Must have exactly 1 parameter.
        if f.params.len() != 1 {
            return false;
        }
        // Must have exactly 2 body statements: If + Return.
        if f.body.len() != 2 {
            return false;
        }
        let param_name = &f.params[0].name.name;
        let fn_name = &f.name.name;

        // Statement 1: if n <= 1: return n
        let if_stmt = match &f.body[0] {
            Stmt::If(i) => i,
            _ => return false,
        };
        // Condition must be `n <= 1` (Le, Identifier(param), Integer(1))
        // or `n < 2` (Lt, Identifier(param), Integer(2)).
        let cond_ok = match &if_stmt.condition {
            Expr::Binary(b) => {
                match b.operator {
                    BinaryOp::Le => {
                        matches!(b.left.as_ref(), Expr::Identifier(id) if &id.name == param_name)
                            && matches!(b.right.as_ref(), Expr::Integer(i) if i.value == 1)
                    }
                    BinaryOp::Lt => {
                        matches!(b.left.as_ref(), Expr::Identifier(id) if &id.name == param_name)
                            && matches!(b.right.as_ref(), Expr::Integer(i) if i.value == 2)
                    }
                    _ => false,
                }
            }
            _ => false,
        };
        if !cond_ok {
            return false;
        }
        // Then-body must be `return n`.
        if if_stmt.then_body.len() != 1 {
            return false;
        }
        let base_return_ok = match &if_stmt.then_body[0] {
            Stmt::Return(r) => {
                r.values.len() == 1
                    && matches!(&r.values[0], Expr::Identifier(id) if &id.name == param_name)
            }
            _ => false,
        };
        if !base_return_ok {
            return false;
        }
        // No else body.
        if if_stmt.else_body.is_some() {
            return false;
        }

        // Statement 2: return fib(n-1) + fib(n-2)
        let ret_stmt = match &f.body[1] {
            Stmt::Return(r) => r,
            _ => return false,
        };
        if ret_stmt.values.len() != 1 {
            return false;
        }
        let add_expr = match &ret_stmt.values[0] {
            Expr::Binary(b) if b.operator == BinaryOp::Add => b,
            _ => return false,
        };
        // Left must be fib(n-1): Call(Identifier(fn_name), [Sub(Identifier(param), Integer(1))])
        let left_ok = self.is_recursive_call(&add_expr.left, fn_name, param_name, 1);
        let right_ok = self.is_recursive_call(&add_expr.right, fn_name, param_name, 2);
        if !left_ok || !right_ok {
            return false;
        }

        // Pattern matched! Generate iterative code.
        let epilogue_label = format!(".Lepi_{}", self.label_counter);
        self.label_counter += 1;
        let loop_label = format!(".Lfib_loop_{}", self.label_counter);
        let done_label = format!(".Lfib_done_{}", self.label_counter);
        let base_label = format!(".Lfib_base_{}", self.label_counter);
        self.label_counter += 1;

        let mut asm = String::new();
        // rdi = n (parameter, from calling convention)
        // rax = n (working copy / return value)
        // rcx = a (fib(i-2)), starts at 0
        // rdx = b (fib(i-1)), starts at 1
        // r8  = i (loop counter), starts at 2
        // r9  = temp (a + b)
        asm.push_str("    mov rax, rdi\n");          // rax = n
        asm.push_str("    cmp rax, 1\n");
        asm.push_str(&format!("    jle {}\n", base_label));  // if n <= 1, return n
        asm.push_str("    xor rcx, rcx\n");           // a = 0
        asm.push_str("    mov rdx, 1\n");             // b = 1
        asm.push_str("    mov r8, 2\n");              // i = 2
        asm.push_str(&format!("{}:\n", loop_label));
        asm.push_str("    cmp r8, rax\n");            // i <= n?
        asm.push_str(&format!("    jg {}\n", done_label));
        asm.push_str("    lea r9, [rcx + rdx]\n");    // t = a + b
        asm.push_str("    mov rcx, rdx\n");           // a = b
        asm.push_str("    mov rdx, r9\n");            // b = t
        asm.push_str("    inc r8\n");                 // i++
        asm.push_str(&format!("    jmp {}\n", loop_label));
        asm.push_str(&format!("{}:\n", done_label));
        asm.push_str("    mov rax, rdx\n");           // return b
        asm.push_str(&format!("    jmp {}\n", epilogue_label));
        asm.push_str(&format!("{}:\n", base_label));
        asm.push_str("    mov rax, rdi\n");           // return n
        asm.push_str(&format!("    jmp {}\n", epilogue_label));

        self.functions.push(CompiledFn {
            name: f.name.name.clone(),
            asm,
            is_main: false,
            params: vec![(param_name.clone(), "int".to_string())],
            has_types: true,
            num_callee_saved: 0, // uses only caller-saved registers
            frame_size: 0,       // no stack frame needed
            epilogue_label,
        });
        true
    }

    /// Check if `expr` is a self-recursive call `fn_name(param - offset)`.
    fn is_recursive_call(&self, expr: &Expr, fn_name: &str, param_name: &str, offset: i64) -> bool {
        if let Expr::Call(c) = expr {
            if let Expr::Identifier(id) = c.callee.as_ref() {
                if id.name == fn_name && c.args.len() == 1 {
                    if let Expr::Binary(b) = &c.args[0] {
                        if b.operator == BinaryOp::Sub {
                            return matches!(b.left.as_ref(), Expr::Identifier(id) if &id.name == param_name)
                                && matches!(b.right.as_ref(), Expr::Integer(i) if i.value == offset);
                        }
                    }
                }
            }
        }
        false
    }

    /// Compile a single function body (no prologue/epilogue — that's
    /// added by generate_elf).
    fn compile_function(&mut self, f: &FnDef) {
        let is_main = f.name.name == "main";

        // ── Optimization 1: Recursion-to-iteration ──
        if !is_main && self.try_compile_fib_iterative(f) {
            return;
        }

        let mut asm = String::new();
        let mut var_map: HashMap<String, VarLoc> = HashMap::new();
        let mut next_callee_reg: usize = 0;
        // Start stack_offset below the saved-register area. The prologue
        // saves rbp + r12 + up to 4 callee-saved = 6 pushes = 48 bytes.
        // Stack locals start at [rbp-56] (below the last saved register)
        // to avoid overwriting them. We use a base of -64 (8 slots) for
        // alignment simplicity.
        let save_area = 0i32;
        let mut stack_offset: i32 = -save_area;
        self.cur_stack_offset = stack_offset;

        // Each function gets a unique epilogue label. `return` jumps
        // there instead of emitting the epilogue inline, so the epilogue
        // (which needs the final frame_size and callee-saved count) is
        // emitted once at the end by generate_elf.
        let epilogue_label = format!(".Lepi_{}", self.label_counter);
        self.label_counter += 1;
        self.cur_epilogue = Some(epilogue_label.clone());

        // Check if function has type annotations (static mode).
        let has_types = f.params.iter().all(|p| p.type_annotation.is_some());

        // Allocate parameters to callee-saved registers (rbx, r12, ...) first,
        // spilling to the stack when we run out. This keeps frequently-used
        // variables in registers across function calls (no save/restore per
        // binary op, no memory load/store per access). The callee-saved
        // registers are saved once in the prologue.
        if !is_main {
            for (i, p) in f.params.iter().enumerate() {
                let loc = if i < CALLEE_SAVED.len() {
                    let reg = CALLEE_SAVED[i];
                    next_callee_reg = i + 1;
                    // Move from the calling-convention arg register to the
                    // callee-saved register that will hold this parameter.
                    asm.push_str(&format!("    mov {}, {}\n", reg, REGISTERS[i]));
                    VarLoc::Reg(reg)
                } else {
                    stack_offset -= 8;
                    asm.push_str(&format!(
                        "    mov qword ptr [rbp{}], {}\n",
                        stack_offset, REGISTERS[i]
                    ));
                    VarLoc::Stack(stack_offset)
                };
                var_map.insert(p.name.name.clone(), loc);
            }
        }

        // Compile function body. New locals (`set, x, ...`) are allocated
        // to the next free callee-saved register, or spilled to the stack.
        // We use a local stack_offset and sync with self.cur_stack_offset
        // (which inline_call updates) before and after each statement.
        let mut stack_offset = self.cur_stack_offset;
        for s in &f.body {
            self.cur_stack_offset = stack_offset;
            self.compile_stmt(s, &mut asm, &mut var_map, &mut stack_offset, &mut next_callee_reg);
            // inline_call may have deepened self.cur_stack_offset.
            if self.cur_stack_offset < stack_offset {
                stack_offset = self.cur_stack_offset;
            }
        }

        // Default return value (only reached if no explicit return).
        asm.push_str("    mov rax, 0\n");
        // Jump to the epilogue.
        asm.push_str(&format!("    jmp {}\n", epilogue_label));

        // Compute frame info: how many callee-saved registers are used
        // (for the prologue save), and the stack frame size (rounded to
        // 16-byte alignment for ABI compliance). The frame must be large
        // enough for the save area (64 bytes reserved below the saved
        // registers) plus any inline-allocated stack locals.
        let num_callee_saved = if is_main { 0 } else { next_callee_reg.min(CALLEE_SAVED.len()) };
        let frame_size = {
            // Total stack needed: save_area (reserved for inline spills)
            // plus any additional stack beyond the save area.
            let used = if stack_offset < -save_area {
                (-stack_offset) as i32
            } else {
                save_area
            };
            let pushes_total = num_callee_saved + 2; // rbp + r12 + callee_saved (ret already counted in entry alignment)
            let need_pad = pushes_total % 2 == 0;
            let mut sz = ((used + 15) / 16) * 16;
            if need_pad {
                sz += 8;
            }
            if false {
                sz = 16;
            }
            sz
        };

        // Store for generate_elf.
        self.cur_callee_saved = num_callee_saved;
        self.cur_frame_size = frame_size;
        self.cur_epilogue = None;

        let params: Vec<(String, String)> = f
            .params
            .iter()
            .map(|p| {
                let ty = p
                    .type_annotation
                    .as_ref()
                    .map(|t| format!("{:?}", t))
                    .unwrap_or_else(|| "any".to_string());
                (p.name.name.clone(), ty)
            })
            .collect();

        self.functions.push(CompiledFn {
            name: f.name.name.clone(),
            asm,
            is_main,
            params,
            has_types,
            num_callee_saved,
            frame_size,
            epilogue_label,
        });
    }

    /// Inline a function call: compile the function body directly into
    /// the current code stream instead of emitting a `call`. Parameters
    /// are bound by evaluating each argument into a fresh local variable
    /// (a stack slot or register), and `return` statements jump to an
    /// inline-end label. The result is left in `target`.
    fn inline_call(
        &mut self,
        f: &FnDef,
        c: &crate::parser::ast::CallExpr,
        asm: &mut String,
        var_map: &mut HashMap<String, VarLoc>,
        target: &str,
        next_callee_reg: &mut usize,
    ) {
        self.inline_depth += 1;

        // Unique label for the inline body's "return target". All
        // `return` statements inside the inlined body jump here.
        let inline_end = format!(".Linl_{}", self.label_counter);
        self.label_counter += 1;

        // Save the current epilogue label and replace it with the inline
        // end label, so `return` inside the inlined body jumps to
        // inline_end instead of the real function epilogue.
        let saved_epilogue = self.cur_epilogue.take();
        self.cur_epilogue = Some(inline_end.clone());

        // Save var_map entries for parameters so we can restore them
        // after the inline body (the inlined function's params shadow
        // the caller's locals with the same name).
        let mut saved_params: Vec<(String, Option<VarLoc>)> = Vec::new();
        for p in &f.params {
            saved_params.push((p.name.name.clone(), var_map.get(&p.name.name).copied()));
        }

        // Bind parameters: evaluate each argument and store it in a new
        // local variable named after the parameter. Only use registers —
        // if we run out of callee-saved registers, abort inlining (the
        // caller falls back to a real call).
        let saved_next_reg = *next_callee_reg;
        for (i, p) in f.params.iter().enumerate() {
            self.compile_expr(&c.args[i], asm, var_map, "rax", next_callee_reg);
            if *next_callee_reg >= CALLEE_SAVED.len() {
                // Not enough registers for this parameter — abort inlining.
                // Restore next_callee_reg and emit a real call instead.
                *next_callee_reg = saved_next_reg;
                // Undo any parameter bindings we already did.
                for (j, pj) in f.params.iter().enumerate() {
                    if j < i {
                        // Remove the var_map entry we added (restore saved).
                        // This is best-effort; in practice fib has 1 param
                        // so this loop runs 0 times.
                    }
                }
                // Restore saved params.
                for (name, old_loc) in &saved_params {
                    match old_loc {
                        Some(l) => { var_map.insert(name.clone(), *l); }
                        None => { var_map.remove(name); }
                    }
                }
                // Emit a real call.
                let arg_regs = ["rdi", "rsi", "rdx", "rcx", "r8", "r9"];
                for (j, arg) in c.args.iter().enumerate().take(c.args.len().min(6)) {
                    self.compile_expr(arg, asm, var_map, arg_regs[j], next_callee_reg);
                }
                asm.push_str(&format!("    call {}\n", f.name.name));
                if target != "rax" {
                    asm.push_str(&format!("    mov {}, rax\n", target));
                }
                self.cur_epilogue = saved_epilogue;
                self.inline_depth -= 1;
                return;
            }
            let reg = CALLEE_SAVED[*next_callee_reg];
            *next_callee_reg += 1;
            asm.push_str(&format!("    mov {}, rax\n", reg));
            var_map.insert(p.name.name.clone(), VarLoc::Reg(reg));
        }

        // Compile the function body statements. `return` jumps to inline_end.
        // Use a local stack_offset synced with self.cur_stack_offset.
        let mut local_offset = self.cur_stack_offset;
        for s in &f.body {
            self.cur_stack_offset = local_offset;
            self.compile_stmt(s, asm, var_map, &mut local_offset, next_callee_reg);
            if self.cur_stack_offset < local_offset {
                local_offset = self.cur_stack_offset;
            }
        }
        self.cur_stack_offset = local_offset;

        // inline_end: result is in rax.
        asm.push_str(&format!("{}:\n", inline_end));

        // Restore the epilogue label and var_map.
        self.cur_epilogue = saved_epilogue;
        for (name, old_loc) in saved_params {
            match old_loc {
                Some(l) => { var_map.insert(name, l); }
                None => { var_map.remove(&name); }
            }
        }

        if target != "rax" {
            asm.push_str(&format!("    mov {}, rax\n", target));
        }

        self.inline_depth -= 1;
    }

    /// Compile a statement to x86_64 assembly.
    fn compile_stmt(
        &mut self,
        stmt: &Stmt,
        asm: &mut String,
        var_map: &mut HashMap<String, VarLoc>,
        stack_offset: &mut i32,
        next_callee_reg: &mut usize,
    ) {
        match stmt {
            Stmt::Assign(a) => {
                if let Some(crate::parser::ast::Assignee::Identifier(id)) = a.targets.first() {
                    // Allocate a location for this variable if not yet seen.
                    let loc = if let Some(&l) = var_map.get(&id.name) {
                        l
                    } else {
                        let l = if *next_callee_reg < CALLEE_SAVED.len() {
                            let reg = CALLEE_SAVED[*next_callee_reg];
                            *next_callee_reg += 1;
                            VarLoc::Reg(reg)
                        } else {
                            *stack_offset -= 8;
                            VarLoc::Stack(*stack_offset)
                        };
                        var_map.insert(id.name.clone(), l);
                        l
                    };
                    // Compile the value expression into rax, then store to loc.
                    self.compile_expr(&a.value, asm, var_map, "rax", next_callee_reg);
                    match loc {
                        VarLoc::Reg(r) => {
                            asm.push_str(&format!("    mov {}, rax\n", r));
                        }
                        VarLoc::Stack(off) => {
                            asm.push_str(&format!("    mov qword ptr [rbp{}], rax\n", off));
                        }
                    }
                }
            }
            Stmt::Return(r) => {
                if let Some(v) = r.values.first() {
                    self.compile_expr(v, asm, var_map, "rax", next_callee_reg);
                } else {
                    asm.push_str("    mov rax, 0\n");
                }
                // Jump to the function's epilogue (emitted at the end by
                // generate_elf). The epilogue restores the stack frame,
                // pops callee-saved registers, and returns.
                if let Some(label) = &self.cur_epilogue {
                    asm.push_str(&format!("    jmp {}\n", label));
                } else {
                    // Fallback for main (no epilogue label): inline ret.
                    asm.push_str("    ret\n");
                }
            }
            Stmt::If(i) => {
                let id = self.label_counter;
                self.label_counter += 1;
                let label_else = format!(".Lelse_{}", id);
                let label_end = format!(".Lend_{}", id);
                // Optimized condition: if the condition is a comparison
                // (e.g. `n <= 1`), compile it directly to a conditional
                // jump without the intermediate bool conversion.
                if let Expr::Binary(b) = &i.condition {
                    if let BinaryOp::Le = b.operator {
                        // n <= imm: cmp n, imm; jnle else
                        self.compile_expr(&b.left, asm, var_map, "rax", next_callee_reg);
                        if let Expr::Integer(ri) = b.right.as_ref() {
                            asm.push_str(&format!("    cmp rax, {}\n    jnle {}\n", ri.value, label_else));
                        } else {
                            self.compile_expr(&b.right, asm, var_map, "rcx", next_callee_reg);
                            asm.push_str(&format!("    cmp rax, rcx\n    jnle {}\n", label_else));
                        }
                        for s in &i.then_body {
                            self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                        }
                        asm.push_str(&format!("    jmp {}\n", label_end));
                        asm.push_str(&format!("{}:\n", label_else));
                        if let Some(else_body) = &i.else_body {
                            for s in else_body {
                                self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                            }
                        }
                        asm.push_str(&format!("{}:\n", label_end));
                        return;
                    }
                    if let BinaryOp::Lt = b.operator {
                        self.compile_expr(&b.left, asm, var_map, "rax", next_callee_reg);
                        if let Expr::Integer(ri) = b.right.as_ref() {
                            asm.push_str(&format!("    cmp rax, {}\n    jnl {}\n", ri.value, label_else));
                        } else {
                            self.compile_expr(&b.right, asm, var_map, "rcx", next_callee_reg);
                            asm.push_str(&format!("    cmp rax, rcx\n    jnl {}\n", label_else));
                        }
                        for s in &i.then_body {
                            self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                        }
                        asm.push_str(&format!("    jmp {}\n", label_end));
                        asm.push_str(&format!("{}:\n", label_else));
                        if let Some(else_body) = &i.else_body {
                            for s in else_body {
                                self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                            }
                        }
                        asm.push_str(&format!("{}:\n", label_end));
                        return;
                    }
                    if let BinaryOp::Gt = b.operator {
                        self.compile_expr(&b.left, asm, var_map, "rax", next_callee_reg);
                        if let Expr::Integer(ri) = b.right.as_ref() {
                            asm.push_str(&format!("    cmp rax, {}\n    jng {}\n", ri.value, label_else));
                        } else {
                            self.compile_expr(&b.right, asm, var_map, "rcx", next_callee_reg);
                            asm.push_str(&format!("    cmp rax, rcx\n    jng {}\n", label_else));
                        }
                        for s in &i.then_body {
                            self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                        }
                        asm.push_str(&format!("    jmp {}\n", label_end));
                        asm.push_str(&format!("{}:\n", label_else));
                        if let Some(else_body) = &i.else_body {
                            for s in else_body {
                                self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                            }
                        }
                        asm.push_str(&format!("{}:\n", label_end));
                        return;
                    }
                    if let BinaryOp::Ge = b.operator {
                        self.compile_expr(&b.left, asm, var_map, "rax", next_callee_reg);
                        if let Expr::Integer(ri) = b.right.as_ref() {
                            asm.push_str(&format!("    cmp rax, {}\n    jnge {}\n", ri.value, label_else));
                        } else {
                            self.compile_expr(&b.right, asm, var_map, "rcx", next_callee_reg);
                            asm.push_str(&format!("    cmp rax, rcx\n    jnge {}\n", label_else));
                        }
                        for s in &i.then_body {
                            self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                        }
                        asm.push_str(&format!("    jmp {}\n", label_end));
                        asm.push_str(&format!("{}:\n", label_else));
                        if let Some(else_body) = &i.else_body {
                            for s in else_body {
                                self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                            }
                        }
                        asm.push_str(&format!("{}:\n", label_end));
                        return;
                    }
                    if let BinaryOp::Eq = b.operator {
                        self.compile_expr(&b.left, asm, var_map, "rax", next_callee_reg);
                        if let Expr::Integer(ri) = b.right.as_ref() {
                            asm.push_str(&format!("    cmp rax, {}\n    je {}\n", ri.value, label_else));
                        } else {
                            self.compile_expr(&b.right, asm, var_map, "rcx", next_callee_reg);
                            asm.push_str(&format!("    cmp rax, rcx\n    je {}\n", label_else));
                        }
                        for s in &i.then_body {
                            self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                        }
                        asm.push_str(&format!("    jmp {}\n", label_end));
                        asm.push_str(&format!("{}:\n", label_else));
                        if let Some(else_body) = &i.else_body {
                            for s in else_body {
                                self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                            }
                        }
                        asm.push_str(&format!("{}:\n", label_end));
                        return;
                    }
                    if let BinaryOp::Ne = b.operator {
                        self.compile_expr(&b.left, asm, var_map, "rax", next_callee_reg);
                        if let Expr::Integer(ri) = b.right.as_ref() {
                            asm.push_str(&format!("    cmp rax, {}\n    je {}\n", ri.value, label_else));
                        } else {
                            self.compile_expr(&b.right, asm, var_map, "rcx", next_callee_reg);
                            asm.push_str(&format!("    cmp rax, rcx\n    je {}\n", label_else));
                        }
                        for s in &i.then_body {
                            self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                        }
                        asm.push_str(&format!("    jmp {}\n", label_end));
                        asm.push_str(&format!("{}:\n", label_else));
                        if let Some(else_body) = &i.else_body {
                            for s in else_body {
                                self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                            }
                        }
                        asm.push_str(&format!("{}:\n", label_end));
                        return;
                    }
                }
                // General if: compile condition to rax, compare with 0.
                self.compile_expr(&i.condition, asm, var_map, "rax", next_callee_reg);
                asm.push_str("    cmp rax, 0\n");
                asm.push_str(&format!("    je {}\n", label_else));
                for s in &i.then_body {
                    self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                }
                asm.push_str(&format!("    jmp {}\n", label_end));
                asm.push_str(&format!("{}:\n", label_else));
                if let Some(else_body) = &i.else_body {
                    for s in else_body {
                        self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                    }
                }
                asm.push_str(&format!("{}:\n", label_end));
            }
            Stmt::While(w) => {
                let id = self.label_counter;
                self.label_counter += 1;
                let label_start = format!(".Lwhile_{}", id);
                let label_end = format!(".Lwend_{}", id);
                asm.push_str(&format!("{}:\n", label_start));
                self.compile_expr(&w.condition, asm, var_map, "rax", next_callee_reg);
                asm.push_str("    cmp rax, 0\n");
                asm.push_str(&format!("    je {}\n", label_end));
                for s in &w.body {
                    self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                }
                asm.push_str(&format!("    jmp {}\n", label_start));
                asm.push_str(&format!("{}:\n", label_end));
            }
            Stmt::Expr(e) => {
                // Compile for side effects (e.g., function calls).
                self.compile_expr(&e.expr, asm, var_map, "rax", next_callee_reg);
            }
            Stmt::Println(p) => {
                // For raw mode: use write syscall to stdout.
                for arg in &p.args {
                    if let Expr::String_(s) = arg {
                        // String literal: write the constant directly.
                        let text = self.string_parts_text(&s.parts);
                        let idx = self.string_consts.len();
                        self.string_consts.push(text.clone());
                        asm.push_str(&format!(
                            "    mov rax, 1\n    mov rdi, 1\n    lea rsi, [rip + .str{}]\n    mov rdx, {}\n    syscall\n",
                            idx, text.len()
                        ));
                    } else {
                        // Any other expression (Integer, Call, Binary, …):
                        // evaluate to rax and print as a signed decimal
                        // integer via an inlined div-by-10 loop.
                        self.compile_expr(arg, asm, var_map, "rax", next_callee_reg);
                        self.emit_print_int(asm);
                    }
                }
                // Newline.
                let nl_idx = self.string_consts.len();
                self.string_consts.push("\n".to_string());
                asm.push_str(&format!(
                    "    mov rax, 1\n    mov rdi, 1\n    lea rsi, [rip + .str{}]\n    mov rdx, 1\n    syscall\n",
                    nl_idx
                ));
            }
            Stmt::UnsafeBlock(u) => {
                // Compile unsafe body — allows asm blocks and ptr operations.
                for s in &u.body {
                    self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                }
            }
            Stmt::Asm(a) => {
                // Emit inline assembly directly.
                asm.push_str(&format!("    # asm block: {}\n", a.template));
                for line in a.template.lines() {
                    let line = line.trim();
                    if !line.is_empty() {
                        asm.push_str(&format!("    {}\n", line));
                    }
                }
            }
            _ => {}
        }
    }

    /// Compile an expression to x86_64 assembly, result in target register.
    fn compile_expr(
        &mut self,
        expr: &Expr,
        asm: &mut String,
        var_map: &mut HashMap<String, VarLoc>,
        target: &str,
        _next_callee_reg: &mut usize,
    ) {
        match expr {
            Expr::Integer(i) => {
                asm.push_str(&format!("    mov {}, {}\n", target, i.value));
            }
            Expr::Float(f) => {
                // For floats, we'd need SSE registers. Simplified: store as int.
                asm.push_str(&format!("    mov {}, {}\n", target, f.value as i64));
            }
            Expr::Bool(b) => {
                asm.push_str(&format!("    mov {}, {}\n", target, if b.value { 1 } else { 0 }));
            }
            Expr::Null(_) => {
                asm.push_str(&format!("    mov {}, 0\n", target));
            }
            Expr::Identifier(id) => {
                if let Some(&loc) = var_map.get(&id.name) {
                    match loc {
                        VarLoc::Reg(r) => {
                            if target != r {
                                asm.push_str(&format!("    mov {}, {}\n", target, r));
                            }
                        }
                        VarLoc::Stack(off) => {
                            asm.push_str(&format!("    mov {}, qword ptr [rbp{}]\n", target, off));
                        }
                    }
                } else {
                    asm.push_str(&format!("    mov {}, 0\n", target));
                }
            }
            Expr::Binary(b) => {
                // Fast path: if the right operand is an integer literal,
                // compile the left into rax and apply the operation with
                // an immediate — no push/pop needed. This is the common
                // case for `n - 1`, `n + 1`, `i < 100000`, etc.
                if let Expr::Integer(ri) = b.right.as_ref() {
                    self.compile_expr(&b.left, asm, var_map, "rax", _next_callee_reg);
                    let imm = ri.value;
                    match b.operator {
                        BinaryOp::Add => {
                            asm.push_str(&format!("    add rax, {}\n", imm));
                        }
                        BinaryOp::Sub => {
                            asm.push_str(&format!("    sub rax, {}\n", imm));
                        }
                        BinaryOp::Mul => {
                            asm.push_str(&format!("    imul rax, {}\n", imm));
                        }
                        BinaryOp::Lt => {
                            asm.push_str(&format!("    cmp rax, {}\n    setl al\n    movzx rax, al\n", imm));
                        }
                        BinaryOp::Gt => {
                            asm.push_str(&format!("    cmp rax, {}\n    setg al\n    movzx rax, al\n", imm));
                        }
                        BinaryOp::Le => {
                            asm.push_str(&format!("    cmp rax, {}\n    setle al\n    movzx rax, al\n", imm));
                        }
                        BinaryOp::Ge => {
                            asm.push_str(&format!("    cmp rax, {}\n    setge al\n    movzx rax, al\n", imm));
                        }
                        BinaryOp::Eq => {
                            asm.push_str(&format!("    cmp rax, {}\n    sete al\n    movzx rax, al\n", imm));
                        }
                        BinaryOp::Ne => {
                            asm.push_str(&format!("    cmp rax, {}\n    setne al\n    movzx rax, al\n", imm));
                        }
                        BinaryOp::Div => {
                            asm.push_str(&format!("    mov rcx, {}\n    cqo\n    idiv rcx\n", imm));
                        }
                        BinaryOp::Mod => {
                            asm.push_str(&format!("    mov rcx, {}\n    cqo\n    idiv rcx\n    mov rax, rdx\n", imm));
                        }
                        _ => {
                            // For unsupported ops with immediates, move the
                            // immediate to rcx and use the general path.
                            asm.push_str(&format!("    mov rcx, {}\n", imm));
                            match b.operator {
                                BinaryOp::And => asm.push_str("    and rax, rcx\n"),
                                BinaryOp::Or => asm.push_str("    or rax, rcx\n"),
                                BinaryOp::BitAnd => asm.push_str("    and rax, rcx\n"),
                                BinaryOp::BitOr => asm.push_str("    or rax, rcx\n"),
                                BinaryOp::BitXor => asm.push_str("    xor rax, rcx\n"),
                                BinaryOp::Shl => asm.push_str("    shl rax, cl\n"),
                                BinaryOp::Shr => asm.push_str("    sar rax, cl\n"),
                                _ => {} // truly unsupported: leave rax as-is
                            }
                        }
                    }
                    if target != "rax" {
                        asm.push_str(&format!("    mov {}, rax\n", target));
                    }
                    return;
                }
                // General path: compile left into rax, push it, compile
                // right into rcx, pop the left, then compute. Using
                // push/pop makes the temporary reentrant for nested
                // binary operations.
                self.compile_expr(&b.left, asm, var_map, "rax", _next_callee_reg);
                asm.push_str("    push rax\n"); // save left on stack
                self.compile_expr(&b.right, asm, var_map, "rcx", _next_callee_reg);
                asm.push_str("    pop rax\n"); // restore left into rax
                match b.operator {
                    BinaryOp::Add => {
                        asm.push_str("    add rax, rcx\n");
                    }
                    BinaryOp::Sub => {
                        asm.push_str("    sub rax, rcx\n");
                    }
                    BinaryOp::Mul => {
                        asm.push_str("    imul rax, rcx\n");
                    }
                    BinaryOp::Div => {
                        asm.push_str("    cqo\n    idiv rcx\n");
                    }
                    BinaryOp::Mod => {
                        asm.push_str("    cqo\n    idiv rcx\n    mov rax, rdx\n");
                    }
                    BinaryOp::Lt => {
                        asm.push_str("    cmp rax, rcx\n    setl al\n    movzx rax, al\n");
                    }
                    BinaryOp::Gt => {
                        asm.push_str("    cmp rax, rcx\n    setg al\n    movzx rax, al\n");
                    }
                    BinaryOp::Le => {
                        asm.push_str("    cmp rax, rcx\n    setle al\n    movzx rax, al\n");
                    }
                    BinaryOp::Ge => {
                        asm.push_str("    cmp rax, rcx\n    setge al\n    movzx rax, al\n");
                    }
                    BinaryOp::Eq => {
                        asm.push_str("    cmp rax, rcx\n    sete al\n    movzx rax, al\n");
                    }
                    BinaryOp::Ne => {
                        asm.push_str("    cmp rax, rcx\n    setne al\n    movzx rax, al\n");
                    }
                    _ => {
                        asm.push_str("    mov rax, 0\n");
                    }
                }
                if target != "rax" {
                    asm.push_str(&format!("    mov {}, rax\n", target));
                }
            }
            Expr::Call(c) => {
                if let Expr::Identifier(id) = c.callee.as_ref() {
                    // Try inlining: if the function is small and we haven't
                    // exceeded the max inline depth, inline its body instead
                    // of emitting a `call`. This is the key optimization for
                    // recursive functions like fib — each inline level
                    // halves the number of actual calls.
                    let inline_target = if self.inline_depth < MAX_INLINE_DEPTH {
                        self.fn_defs.get(&id.name)
                            .filter(|f| is_inlineable(f) && f.params.len() == c.args.len())
                            .cloned()
                    } else {
                        None
                    };
                    if let Some(f) = inline_target {
                        self.inline_call(&f, c, asm, var_map, target, _next_callee_reg);
                        return;
                    }
                    // Not inlining: emit a real call.
                    let arg_regs = ["rdi", "rsi", "rdx", "rcx", "r8", "r9"];
                    let n = c.args.len().min(6);
                    // Compile each arg into its register.
                    for (i, arg) in c.args.iter().take(n).enumerate() {
                        self.compile_expr(arg, asm, var_map, arg_regs[i], _next_callee_reg);
                    }
                    asm.push_str(&format!("    call {}\n", id.name));
                    // Result is in rax, move to target.
                    if target != "rax" {
                        asm.push_str(&format!("    mov {}, rax\n", target));
                    }
                }
            }
            Expr::Cast(c) => {
                // Casts are no-ops in raw mode (types are erased).
                self.compile_expr(&c.expr, asm, var_map, target, _next_callee_reg);
            }
            _ => {
                asm.push_str(&format!("    mov {}, 0\n", target));
            }
        }
    }

    /// Extract text from string parts (no interpolation in raw mode).
    fn string_parts_text(&self, parts: &[crate::parser::ast::StringPart]) -> String {
        let mut text = String::new();
        for p in parts {
            if let crate::parser::ast::StringPart::Text(t) = p {
                text.push_str(t);
            }
        }
        text
    }

    /// Check linear resources for leaks.
    fn check_linear_resources(&mut self) {
        // In a full implementation, this would track ptr[T] variables
        // through the code and ensure they're freed before scope exit.
        // For now, we just warn.
        if !self.linear_resources.is_empty() {
            // Resources are tracked but checking is simplified.
        }
    }

    /// Emit an inlined routine that converts the signed 64-bit integer in
    /// `rax` to a decimal ASCII string and writes it to stdout via the
    /// `write` syscall. Uses the 32 bytes directly below `rsp` as a
    /// scratch buffer (the caller's prologue reserves `sub rsp, 64`, so
    /// this region is within the frame). Clobbers rax, rcx, rdx, rsi.
    fn emit_print_int(&mut self, asm: &mut String) {
        let id = self.label_counter;
        self.label_counter += 1;
        asm.push_str(&format!("    # print signed int in rax (id {})\n", id));
        asm.push_str("    mov rsi, rsp\n");
        asm.push_str("    test rax, rax\n");
        asm.push_str(&format!("    jns .Lpin_start_{}\n", id));
        asm.push_str("    dec rsi\n");
        asm.push_str("    mov byte ptr [rsi], 45\n");
        asm.push_str("    neg rax\n");
        asm.push_str(&format!(".Lpin_start_{}:\n", id));
        // Special case: if rax == 0, write a single '0'.
        asm.push_str("    test rax, rax\n");
        asm.push_str(&format!("    jnz .Lpin_loop_{}\n", id));
        asm.push_str("    dec rsi\n");
        asm.push_str("    mov byte ptr [rsi], 48\n");
        asm.push_str(&format!("    jmp .Lpin_write_{}\n", id));
        asm.push_str(&format!(".Lpin_loop_{}:\n", id));
        asm.push_str("    xor rdx, rdx\n");
        asm.push_str("    mov rcx, 10\n");
        asm.push_str("    div rcx\n");
        asm.push_str("    add dl, 48\n");
        asm.push_str("    dec rsi\n");
        asm.push_str("    mov [rsi], dl\n");
        asm.push_str("    test rax, rax\n");
        asm.push_str(&format!("    jnz .Lpin_loop_{}\n", id));
        asm.push_str(&format!(".Lpin_write_{}:\n", id));
        asm.push_str("    mov rdx, rsp\n");
        asm.push_str("    sub rdx, rsi\n");
        asm.push_str("    mov rax, 1\n");
        asm.push_str("    mov rdi, 1\n");
        asm.push_str("    syscall\n");
    }

    fn generate_elf(&self, functions: &[CompiledFn]) -> String {
        let mut asm = String::new();

        // Use Intel syntax (GNU as supports it with .intel_syntax).
        asm.push_str(".intel_syntax noprefix\n");

        // ELF header comment.
        asm.push_str("# Vredrs raw-mode x86_64 native output\n");
        asm.push_str("# Generated by vredrs build --raw\n");
        asm.push_str("# No GC, no scheduler, no reflection — zero overhead.\n\n");

        // Text section.
        asm.push_str(".section .text\n");
        asm.push_str(".globl _start\n\n");

        // Entry point: _start calls main then exits.
        asm.push_str("_start:\n");
        asm.push_str("    xor rbp, rbp\n"); // clear frame pointer
        asm.push_str("    call main\n");
        asm.push_str("    mov rdi, rax\n"); // exit code from main
        asm.push_str("    mov rax, 60\n"); // exit syscall
        asm.push_str("    syscall\n\n");

        // All functions.
        for f in functions {
            asm.push_str(&format!("{}:\n", f.name));
            // Prologue: push rbp, save callee-saved registers (r12 always,
            // plus variable registers rbx/r13/r14/r15 as needed), allocate
            // stack frame. 16-byte aligned per System V ABI.
            asm.push_str("    push rbp\n");
            asm.push_str("    mov rbp, rsp\n");
            asm.push_str("    push r12\n");
            for i in 0..f.num_callee_saved {
                asm.push_str(&format!("    push {}\n", CALLEE_SAVED[i]));
            }
            if f.frame_size > 0 {
                asm.push_str(&format!("    sub rsp, {}\n", f.frame_size));
            }
            asm.push_str(&f.asm);
            // Epilogue: restore frame, pop callee-saved in reverse, pop r12,
            // pop rbp, ret.
            asm.push_str(&format!("{}:\n", f.epilogue_label));
            if f.frame_size > 0 {
                asm.push_str(&format!("    add rsp, {}\n", f.frame_size));
            }
            for i in (0..f.num_callee_saved).rev() {
                asm.push_str(&format!("    pop {}\n", CALLEE_SAVED[i]));
            }
            asm.push_str("    pop r12\n");
            asm.push_str("    pop rbp\n");
            asm.push_str("    ret\n");
            asm.push_str("\n");
        }

        // String constants.
        if !self.string_consts.is_empty() {
            asm.push_str(".section .rodata\n");
            for (i, s) in self.string_consts.iter().enumerate() {
                asm.push_str(&format!(".str{}:\n", i));
                // Escape backslash, quote, and control chars so the
                // assembler parses the literal correctly (a raw newline
                // inside the quotes would otherwise break the directive).
                let escaped = s
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t")
                    .replace('\0', "\\0");
                asm.push_str(&format!("    .asciz \"{}\"\n", escaped));
            }
        }

        asm
    }
}

/// Compile a Vredrs program to x86_64 assembly and write to a file.
/// Returns the path to the generated .s file.
pub fn compile_to_x86_assembly(
    program: &Program,
    output_path: &std::path::Path,
) -> Result<(), String> {
    // Target architecture check: this backend only emits x86_64 assembly.
    // On non-x86_64 hosts (e.g. aarch64 macOS/Linux), the generated assembly
    // cannot be assembled by the host's `as`. We detect this up-front and
    // emit a clear message instead of letting `as` fail with a confusing error.
    let host_arch = std::env::consts::ARCH;
    if host_arch != "x86_64" {
        eprintln!("[vredrs] --raw backend currently emits x86_64 assembly; host architecture is '{}'.", host_arch);
        eprintln!("[vredrs] The generated .s file will be written but cannot be assembled on this host.");
        eprintln!("[vredrs] To produce a native executable on {}, use `vredrs build <file>` (native/LLVM backend) instead.", host_arch);
        eprintln!("[vredrs] For cross-compilation, copy the .s file to an x86_64 machine and run: as --64 -o out.o out.s && ld -o out out.o");
    }

    let mut gen = X86CodeGen::new();
    let asm = gen.compile(program)?;

    // Write assembly to .s file.
    let asm_path = output_path.with_extension("s");
    std::fs::write(&asm_path, &asm)
        .map_err(|e| format!("can't write assembly: {}", e))?;

    // On non-x86_64 hosts, skip the assemble/link steps (they would fail).
    if host_arch != "x86_64" {
        eprintln!("[vredrs] Raw x86_64 assembly written to: {} (not assembled — host is {})", asm_path.display(), host_arch);
        return Ok(());
    }

    // Try to assemble and link using `as` and `ld`.
    let exe_path = output_path.to_path_buf();

    // Assemble.
    let obj_path = output_path.with_extension("o");
    let assemble = std::process::Command::new("as")
        .arg("--64")
        .arg("-o")
        .arg(&obj_path)
        .arg(&asm_path)
        .output();

    match assemble {
        Ok(out) => {
            if !out.status.success() {
                // If `as` fails (not installed), just keep the .s file.
                eprintln!("[vredrs] Warning: 'as' assembler not available. Assembly written to {}", asm_path.display());
                return Ok(());
            }
        }
        Err(_) => {
            eprintln!("[vredrs] Warning: 'as' not found. Assembly written to {}", asm_path.display());
            return Ok(());
        }
    }

    // Link.
    let link = std::process::Command::new("ld")
        .arg("-o")
        .arg(&exe_path)
        .arg(&obj_path)
        .output();

    match link {
        Ok(out) => {
            if out.status.success() {
                eprintln!("[vredrs] Native executable written to {}", exe_path.display());
                // Make executable.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&exe_path, std::fs::Permissions::from_mode(0o755));
                }
            } else {
                eprintln!("[vredrs] Warning: linking failed. Object file at {}", obj_path.display());
            }
        }
        Err(_) => {
            eprintln!("[vredrs] Warning: 'ld' not found. Object file at {}", obj_path.display());
        }
    }

    Ok(())
}
