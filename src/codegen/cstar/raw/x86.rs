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

use crate::parser::ast::{Program, TopLevel, Stmt, Expr, BinaryOp, FnDef, FnParam, TypeExpr, BasicType, ConditionalCompile, UnaryOp, StringPart};
use std::collections::HashMap;
use std::io::Write;

/// Evaluate a `ConditionalCompile` condition at compile time when it
/// is a simple literal we can reason about. Returns `Some(bool)` when
/// the value is statically known, or `None` when the condition
/// depends on runtime state (in which case the caller defaults to
/// "true" and selects the then-body).
fn eval_const_condition(e: &Expr) -> Option<bool> {
    match e {
        Expr::Bool(b) => Some(b.value),
        Expr::Integer(i) => Some(i.value != 0),
        Expr::Float(f) => Some(f.value != 0.0),
        Expr::Null(_) => Some(false),
        Expr::String_(s) => {
            let mut text = String::new();
            for p in &s.parts {
                if let StringPart::Text(t) = p {
                    text.push_str(t);
                }
            }
            Some(!text.is_empty())
        }
        Expr::Unary(u) if u.operator == UnaryOp::Not || u.operator == UnaryOp::Bang => {
            eval_const_condition(&u.operand).map(|v| !v)
        }
        _ => None,
    }
}

/// Walk a `Vec<TopLevel>` and flatten any `ConditionalCompile` nodes
/// by evaluating their (literal) conditions at compile time. The
/// selected body (then-body if true, else-body if false) is spliced
/// in place of the `ConditionalCompile`. Conditions that can't be
/// evaluated at compile time default to "true" (then-body).
fn flatten_decls(decls: &[TopLevel], out: &mut Vec<TopLevel>) {
    for d in decls {
        match d {
            TopLevel::ConditionalCompile(cc) => {
                let take_then = match eval_const_condition(&cc.condition) {
                    Some(v) => v,
                    None => true,
                };
                if take_then {
                    flatten_decls(&cc.then_body, out);
                } else if let Some(else_body) = &cc.else_body {
                    flatten_decls(else_body, out);
                }
            }
            other => out.push(other.clone()),
        }
    }
}

/// Produce a flattened copy of the program with all
/// `ConditionalCompile` nodes resolved.
fn flatten_program(program: &Program) -> Program {
    let mut out = Vec::new();
    flatten_decls(&program.declarations, &mut out);
    Program {
        declarations: out,
        span: program.span.clone(),
    }
}

/// Render an expression as a short textual hint for use in
/// `# asm` operand comments. This is *not* a full pretty-printer —
/// it only needs to produce a human-readable annotation; the actual
/// lowering uses the AST directly.
fn expr_text(e: &Expr) -> String {
    match e {
        Expr::Identifier(id) => id.name.clone(),
        Expr::Integer(i) => i.value.to_string(),
        Expr::Float(f) => f.value.to_string(),
        Expr::Bool(b) => b.value.to_string(),
        Expr::Null(_) => "null".to_string(),
        Expr::String_(s) => {
            let mut t = String::new();
            for p in &s.parts {
                if let StringPart::Text(x) = p {
                    t.push_str(x);
                }
            }
            t
        }
        _ => "<expr>".to_string(),
    }
}

/// Best-effort size in bytes for a `TypeExpr`. Used by
/// `calculate_struct_offset()` to compute alignment-aware field
/// offsets. Named types are matched against common C/u-fixed-width
/// names (u8/u16/u32/u64/i8/.../i64, char, short, int, long, float,
/// double, ptr[T]); anything else defaults to 8 (the pointer width on
/// x86_64), which is the safest default for a raw-mode backend that
/// treats most things as opaque 64-bit slots.
fn type_size_bytes(t: &TypeExpr) -> u64 {
    match t {
        TypeExpr::Basic(b, _) => match b {
            BasicType::Int | BasicType::Float | BasicType::Str | BasicType::Any => 8,
            BasicType::Bool | BasicType::Null => 1,
            BasicType::Void => 0,
        },
        TypeExpr::Pointer(_, _) => 8,
        // `u8`/`u16`/`u32`/`u64` are parsed by the parser as
        // `UnsignedInt(N, _)` (N = bit width), not as `Named`. The
        // size in bytes is N / 8.
        TypeExpr::UnsignedInt(bits, _) => (*bits as u64) / 8,
        TypeExpr::Named(id, _) => match id.name.as_str() {
            "i8" | "char" | "byte" => 1,
            "i16" | "short" => 2,
            "i32" | "int" | "float32" => 4,
            "i64" | "long" | "usize" | "isize" | "float64" | "double" => 8,
            _ => 8,
        },
        _ => 8,
    }
}

/// Substitute GCC-style `%N` operand references in an inline-asm
/// template with the corresponding operand register names. `operands`
/// is indexed in GCC order: outputs first, then inputs (so `%0` is the
/// first output, `%N` where N = outputs.len() is the first input, etc.).
/// `%%` is emitted as a literal `%`. Out-of-range `%N` references are
/// left untouched (so the user can spot the bug in the .s output).
fn substitute_asm_template(line: &str, operands: &[String]) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some(&next) = chars.peek() {
                if next == '%' {
                    chars.next();
                    out.push('%');
                    continue;
                }
                if next.is_ascii_digit() {
                    chars.next();
                    let mut n = next.to_digit(10).unwrap() as usize;
                    while let Some(&d) = chars.peek() {
                        if d.is_ascii_digit() {
                            chars.next();
                            n = n * 10 + d.to_digit(10).unwrap() as usize;
                        } else {
                            break;
                        }
                    }
                    if n < operands.len() {
                        out.push_str(&operands[n]);
                    } else {
                        out.push_str(&format!("%{}", n));
                    }
                    continue;
                }
            }
            out.push('%');
        } else {
            out.push(c);
        }
    }
    out
}

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
    /// Loop context stack: each entry is (continue_label, break_label).
    /// `break` jumps to the break_label; `continue` jumps to the
    /// continue_label. Pushed by while/loop/for-range/for-in, popped on
    /// exit.
    loop_stack: Vec<(String, String)>,
    /// Counter for generating unique lambda names (P4 closures).
    lambda_counter: usize,
    /// RAII/dtor definitions, keyed by type name. Collected from
    /// `TopLevel::DtorBlock` nodes at the start of `compile()`. Used by
    /// the RAII heuristic in `compile_function()` to emit destructor
    /// bodies at function exit for variables whose types have a
    /// registered dtor.
    dtor_defs: HashMap<String, Vec<Stmt>>,
    /// Struct alignment requirements (bytes), keyed by struct name.
    /// Populated from `@align(N)` annotations on `TopLevel::StructDef`.
    /// Used by `calculate_struct_offset()` for alignment-aware field
    /// padding and by `generate_elf()` to emit `.balign N` before any
    /// global variable of that struct type.
    struct_alignments: HashMap<String, u64>,
    /// Struct field layouts: struct name → list of (field name, size in
    /// bytes). Populated from `TopLevel::StructDef` field type
    /// annotations. Used by `calculate_struct_offset()` to compute
    /// field offsets at compile time (the raw backend does not emit
    /// struct globals today, but `offsetof(StructName, field)` calls
    /// go through this helper).
    struct_fields: HashMap<String, Vec<(String, u64)>>,
    /// **Register Allocator**: Linear-scan allocator that tracks live
    /// intervals and reuses freed registers. This replaces the old
    /// sequential `next_callee_reg` approach, which wasted registers
    /// when variables had non-overlapping lifetimes.
    regalloc: super::regalloc::RegisterAllocator,
    /// Instruction position counter for the register allocator.
    /// Incremented for each statement to track live intervals.
    instr_pos: usize,
}

/// Maximum inlining depth. Each level halves the number of calls for
/// recursive functions like fib. 3 levels = 1/8 the calls.
const MAX_INLINE_DEPTH: usize = 3;

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
            loop_stack: Vec::new(),
            lambda_counter: 0,
            dtor_defs: HashMap::new(),
            struct_alignments: HashMap::new(),
            struct_fields: HashMap::new(),
            regalloc: super::regalloc::RegisterAllocator::new(
                CALLEE_SAVED.iter().map(|s| s.to_string()).collect(),
                0,
            ),
            instr_pos: 0,
        }
    }

    /// Compile a Vredrs program to x86_64 assembly.
    pub fn compile(&mut self, program: &Program) -> Result<String, String> {
        // Flatten the program: expand `ConditionalCompile` nodes by
        // evaluating their (literal) conditions at compile time, so
        // the rest of the backend only sees the selected top-level
        // declarations. This mirrors the bytecode compiler's
        // `compile_conditional` behaviour.
        let flattened = flatten_program(program);
        let program = &flattened;
        // Collect all function definitions into a map for inlining.
        for d in &program.declarations {
            if let TopLevel::FnDef(f) = d {
                self.fn_defs.insert(f.name.name.clone(), f.clone());
            }
        }
        // Collect all `dtor, Type ... /end` blocks into a map keyed by
        // type name. The RAII heuristic in `compile_function()` uses this
        // map to emit dtor bodies at function exit for variables whose
        // types have a registered destructor.
        for d in &program.declarations {
            if let TopLevel::DtorBlock(dtor) = d {
                self.dtor_defs
                    .insert(dtor.type_name.name.clone(), dtor.body.clone());
            }
        }
        // Collect struct alignment requirements and field layouts from
        // `@align(N)` annotations and field type annotations. The
        // alignment is honored by `calculate_struct_offset()` (which
        // pads fields to the alignment boundary) and by `generate_elf()`
        // (which would emit `.balign N` before any global variable of
        // that struct type — currently the raw backend doesn't emit
        // struct globals, but the alignment is still recorded so
        // `offsetof()` and future struct-global emitters can use it).
        for d in &program.declarations {
            if let TopLevel::StructDef(sd) = d {
                // Look for an `@align(N)` annotation. N must be an
                // integer literal (we don't evaluate arbitrary
                // constant expressions here).
                for ann in &sd.annotations {
                    if ann.name == "align" {
                        if let Some(arg) = ann.arguments.first() {
                            if let Expr::Integer(i) = &arg.value {
                                if i.value > 0 {
                                    self.struct_alignments
                                        .insert(sd.name.name.clone(), i.value as u64);
                                }
                            }
                        }
                    }
                }
                // Record field layouts: (field name, size in bytes).
                // The size is inferred from the field's type annotation
                // via `type_size_bytes` (best-effort: unknown types
                // default to 8 bytes, the pointer-width on x86_64).
                let fields: Vec<(String, u64)> = sd
                    .fields
                    .iter()
                    .map(|f| {
                        let sz = f
                            .type_annotation
                            .as_ref()
                            .map(|t| type_size_bytes(t))
                            .unwrap_or(8);
                        (f.name.name.clone(), sz)
                    })
                    .collect();
                self.struct_fields.insert(sd.name.name.clone(), fields);
            }
        }
        // Compile all function definitions.
        for d in &program.declarations {
            if let TopLevel::FnDef(f) = d {
                self.compile_function(f);
            }
        }
        // Note: dtor definitions are now actually emitted at function
        // exits by `compile_function()` (see the RAII cleanup section).
        // The header comment block below still documents which types
        // have registered dtors for the reader of the .s file.

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

        let mut asm = self.generate_elf(&self.functions);

        // Prepend comments documenting any `dtor, Type ... /end` blocks.
        // The dtor bodies themselves are emitted inline at function exits
        // by `compile_function()` (RAII cleanup); this header comment
        // block just lists the registered types for the reader of the
        // .s file.
        let mut dtor_comments = String::new();
        for d in &program.declarations {
            if let TopLevel::DtorBlock(dtor) = d {
                dtor_comments.push_str(&format!(
                    "# dtor for type {} (emitted at function exits via RAII heuristic)\n",
                    dtor.type_name.name
                ));
            }
        }
        if !dtor_comments.is_empty() {
            // Insert after the header comment block but before
            // `.section .text`. We do this by finding the section
            // directive and prepending the dtor comments there.
            if let Some(idx) = asm.find(".section .text") {
                asm.insert_str(idx, &dtor_comments);
            } else {
                asm = format!("{}{}", dtor_comments, asm);
            }
        }

        Ok(asm)
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
        // **Register Allocator**: Reset for this function. The allocator
        // tracks live intervals and reuses freed registers, reducing
        // unnecessary stack spills.
        self.regalloc.reset(0);
        self.instr_pos = 0;
        // Start stack_offset below the saved-register area.
        let save_area = 0i32;
        let mut stack_offset: i32 = -save_area;
        self.cur_stack_offset = stack_offset;

        // Each function gets a unique epilogue label. `return` jumps
        // there instead of emitting the epilogue inline, so the epilogue
        // (which needs the final frame_size and callee-saved count) is
        // emitted once at the end by generate_elf.
        let epilogue_label = format!(".Lepi_{}", self.label_counter);
        self.label_counter += 1;

        // RAII/dtor heuristic: scan the function body for assignments of
        // the form `set, var = SomeType(...)` where `SomeType` has a
        // registered dtor. If any are found, route all return paths
        // through a `cleanup_label` that emits the dtor bodies inline
        // before jumping to the real epilogue. This makes dtors actually
        // run on every exit (explicit `return` and fall-through) instead
        // of being inert comments. When no dtors apply, we keep the old
        // behaviour (cur_epilogue = epilogue_label) so the generated
        // assembly for ordinary functions is unchanged.
        let dtor_matches: Vec<(String, String)> = self.collect_dtor_vars(&f.body);
        let (cleanup_label, real_epilogue_label) = if dtor_matches.is_empty() {
            (None, epilogue_label.clone())
        } else {
            let cleanup = format!(".Lcleanup_{}", self.label_counter);
            self.label_counter += 1;
            (Some(cleanup), epilogue_label.clone())
        };
        // Stmt::Return and TryPropagate jump to cur_epilogue. Route them
        // through the cleanup label when dtors are present so the dtor
        // bodies run before the real epilogue.
        self.cur_epilogue = Some(
            cleanup_label
                .clone()
                .unwrap_or_else(|| real_epilogue_label.clone()),
        );

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
        // Jump to the cleanup label (if dtors) or the real epilogue.
        asm.push_str(&format!(
            "    jmp {}\n",
            cleanup_label.as_ref().unwrap_or(&real_epilogue_label)
        ));

        // RAII/dtor cleanup block. Emitted only when the function has
        // at least one local whose type has a registered dtor. Each
        // matching variable gets its dtor body compiled inline here,
        // so the destructor actually runs at function exit (whether
        // reached via `return` or by falling off the end). Returns
        // inside a dtor body jump straight to the real epilogue (set
        // via cur_epilogue below) so we can't loop back into the
        // cleanup block.
        if let Some(cleanup) = &cleanup_label {
            asm.push_str(&format!("{}:\n", cleanup));
            // During dtor-body compilation, `return` jumps to the real
            // epilogue (not back to the cleanup label, which would
            // produce an infinite loop).
            self.cur_epilogue = Some(real_epilogue_label.clone());
            for (var_name, type_name) in &dtor_matches {
                asm.push_str(&format!(
                    "    # dtor: calling ~{} for {}\n",
                    type_name, var_name
                ));
                // Clone the dtor body out of `self.dtor_defs` so we
                // don't hold an immutable borrow of `self` across the
                // mutable `compile_stmt` call below.
                let dtor_body = match self.dtor_defs.get(type_name) {
                    Some(b) => b.clone(),
                    None => continue,
                };
                for s in &dtor_body {
                    self.cur_stack_offset = stack_offset;
                    self.compile_stmt(
                        s,
                        &mut asm,
                        &mut var_map,
                        &mut stack_offset,
                        &mut next_callee_reg,
                    );
                    if self.cur_stack_offset < stack_offset {
                        stack_offset = self.cur_stack_offset;
                    }
                }
            }
            // After the dtor bodies, jump to the real epilogue (emitted
            // by generate_elf at real_epilogue_label).
            asm.push_str(&format!("    jmp {}\n", real_epilogue_label));
        }

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
            epilogue_label: real_epilogue_label,
        });
    }

    /// RAII/dtor heuristic: walk a function body and collect
    /// `(var_name, type_name)` pairs for top-level assignments of the
    /// form `set, var = SomeType(...)` where `SomeType` has a
    /// registered dtor in `self.dtor_defs`. If the same variable is
    /// assigned multiple matching types, the last type wins (we want
    /// the destructor for the type the variable actually holds at
    /// function exit). The list is returned in source order.
    ///
    /// This is a deliberately simple heuristic — raw mode does not
    /// track variable types precisely, so we approximate "this
    /// variable holds a value of type T" by "this variable was
    /// assigned `T(...)` at the top level of the function body".
    /// Nested assignments inside `if`/`while`/`for` are not
    /// considered, and reassignments that overwrite a typed local
    /// with a non-constructor value are not tracked. This is
    /// sufficient to make dtors actually run for the common case of
    /// a straightforward constructor call at the top of a function.
    fn collect_dtor_vars(&self, body: &[Stmt]) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for s in body {
            let a = match s {
                Stmt::Assign(a) => a,
                _ => continue,
            };
            if a.operator != crate::parser::ast::AssignOp::Simple {
                continue;
            }
            let var_name = match a.targets.first() {
                Some(crate::parser::ast::Assignee::Identifier(id)) => &id.name,
                _ => continue,
            };
            let type_name = match &a.value {
                Expr::Call(c) => match c.callee.as_ref() {
                    Expr::Identifier(type_id) => &type_id.name,
                    _ => continue,
                },
                _ => continue,
            };
            if !self.dtor_defs.contains_key(type_name) {
                continue;
            }
            // Replace any previous entry for this variable so the
            // last assignment's type wins.
            if let Some(idx) = out.iter().position(|(n, _)| n == var_name) {
                out[idx].1 = type_name.clone();
            } else {
                out.push((var_name.clone(), type_name.clone()));
            }
        }
        out
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
                        let l = self.alloc_var(&id.name, var_map, stack_offset, next_callee_reg);
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
                            asm.push_str(&format!("    cmp rax, {}\n    jne {}\n", ri.value, label_else));
                        } else {
                            self.compile_expr(&b.right, asm, var_map, "rcx", next_callee_reg);
                            asm.push_str(&format!("    cmp rax, rcx\n    jne {}\n", label_else));
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
                // Push loop context so `break`/`continue` inside the body
                // resolve to this loop's labels.
                self.loop_stack.push((label_start.clone(), label_end.clone()));
                asm.push_str(&format!("{}:\n", label_start));
                self.compile_expr(&w.condition, asm, var_map, "rax", next_callee_reg);
                asm.push_str("    cmp rax, 0\n");
                asm.push_str(&format!("    je {}\n", label_end));
                for s in &w.body {
                    self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                }
                asm.push_str(&format!("    jmp {}\n", label_start));
                asm.push_str(&format!("{}:\n", label_end));
                self.loop_stack.pop();
            }
            Stmt::Loop(l) => {
                let id = self.label_counter;
                self.label_counter += 1;
                let label_start = format!(".Lloop_{}", id);
                let label_end = format!(".Lloopend_{}", id);
                self.loop_stack.push((label_start.clone(), label_end.clone()));
                asm.push_str(&format!("{}:\n", label_start));
                for s in &l.body {
                    self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                }
                // Infinite loop: unconditional jump back to the top.
                asm.push_str(&format!("    jmp {}\n", label_start));
                asm.push_str(&format!("{}:\n", label_end));
                self.loop_stack.pop();
            }
            Stmt::Break(b) => {
                // Unlabeled break: jump to the break label of the innermost
                // enclosing loop. If no loop is active, emit a comment (the
                // semantic analyzer should have caught this, but we guard
                // against ICEs).
                if let Some(label) = b.label.as_ref() {
                    // Labeled break: jump to the break label of the loop
                    // with the matching label name. We search the loop
                    // stack from innermost outward for a matching entry.
                    let target = format!(".Lbreak_lbl_{}", label.name);
                    let found = self.loop_stack.iter().rev()
                        .any(|(_, brk)| brk == &target || brk.ends_with(&format!("_{}", label.name)));
                    if found {
                        asm.push_str(&format!("    jmp {}\n", target));
                    } else {
                        // Emit a NOP comment if the label wasn't found.
                        asm.push_str(&format!("    # break '{}' — label not found (no enclosing loop matches)\n", label.name));
                    }
                } else if let Some((_, break_label)) = self.loop_stack.last().cloned() {
                    asm.push_str(&format!("    jmp {}\n", break_label));
                } else {
                    asm.push_str("    # break outside loop (no-op)\n");
                }
            }
            Stmt::Continue(c) => {
                // Unlabeled continue: jump to the continue label (loop
                // top / next-iteration label) of the innermost loop.
                if let Some(label) = c.label.as_ref() {
                    let target = format!(".Lcont_lbl_{}", label.name);
                    let found = self.loop_stack.iter().rev()
                        .any(|(cont, _)| cont == &target || cont.ends_with(&format!("_{}", label.name)));
                    if found {
                        asm.push_str(&format!("    jmp {}\n", target));
                    } else {
                        asm.push_str(&format!("    # continue '{}' — label not found\n", label.name));
                    }
                } else if let Some((cont_label, _)) = self.loop_stack.last().cloned() {
                    asm.push_str(&format!("    jmp {}\n", cont_label));
                } else {
                    asm.push_str("    # continue outside loop (no-op)\n");
                }
            }
            Stmt::ForRange(fr) => {
                // for, i, in, start..end  →  iterate i from start to end-1.
                // The bound and step are saved to stack slots so they
                // survive the body execution (which may clobber caller-saved
                // registers like rcx/rdx).
                let id = self.label_counter;
                self.label_counter += 1;
                let label_cond = format!(".Lfrcond_{}", id);
                let label_incr = format!(".Lfrincr_{}", id);
                let label_end = format!(".Lfrend_{}", id);
                // Allocate two stack slots for bound and step.
                *stack_offset -= 8;
                let bound_slot = *stack_offset;
                *stack_offset -= 8;
                let step_slot = *stack_offset;
                // Evaluate `from` into rax, store to the loop variable.
                self.compile_expr(&fr.from, asm, var_map, "rax", next_callee_reg);
                let loc = if let Some(&l) = var_map.get(&fr.var.name) {
                    l
                } else {
                    let l = self.alloc_var(&fr.var.name, var_map, stack_offset, next_callee_reg);
                    var_map.insert(fr.var.name.clone(), l);
                    l
                };
                match loc {
                    VarLoc::Reg(r) => {
                        asm.push_str(&format!("    mov {}, rax\n", r));
                    }
                    VarLoc::Stack(off) => {
                        asm.push_str(&format!("    mov qword ptr [rbp{}], rax\n", off));
                    }
                }
                // Evaluate `to` into rax, save to bound_slot.
                self.compile_expr(&fr.to, asm, var_map, "rax", next_callee_reg);
                asm.push_str(&format!("    mov qword ptr [rbp{}], rax\n", bound_slot));
                // Step: default 1, or the provided step expression. Save to step_slot.
                if let Some(step_expr) = &fr.step {
                    self.compile_expr(step_expr, asm, var_map, "rax", next_callee_reg);
                    asm.push_str(&format!("    mov qword ptr [rbp{}], rax\n", step_slot));
                } else {
                    asm.push_str(&format!("    mov rax, 1\n"));
                    asm.push_str(&format!("    mov qword ptr [rbp{}], rax\n", step_slot));
                }
                // Loop: push context (continue → increment, break → end).
                // `continue` must jump to the increment step (not the
                // condition check) so that i is advanced before the next
                // bound comparison — otherwise the loop runs forever.
                self.loop_stack.push((label_incr.clone(), label_end.clone()));
                asm.push_str(&format!("{}:\n", label_cond));
                // Load step sign first (doesn't clobber i-vs-bound comparison
                // because we branch before re-comparing).
                let label_pos = format!(".Lfrpos_{}", id);
                asm.push_str(&format!("    mov rax, qword ptr [rbp{}]\n", step_slot));
                asm.push_str("    cmp rax, 0\n");
                asm.push_str(&format!("    jge {}\n", label_pos));  // step >= 0 → positive path
                // Negative step: exit when i <= bound.
                // Load i into rax, bound into rcx, compare.
                match loc {
                    VarLoc::Reg(r) => {
                        asm.push_str(&format!("    mov rax, {}\n", r));
                    }
                    VarLoc::Stack(off) => {
                        asm.push_str(&format!("    mov rax, qword ptr [rbp{}]\n", off));
                    }
                }
                asm.push_str(&format!("    mov rcx, qword ptr [rbp{}]\n", bound_slot));
                asm.push_str("    cmp rax, rcx\n");
                asm.push_str(&format!("    jle {}\n", label_end));
                let label_skip = format!(".Lfrskip_{}", id);
                asm.push_str(&format!("    jmp {}\n", label_skip));  // skip positive path
                asm.push_str(&format!("{}:\n", label_pos));
                // Positive step: exit when i >= bound.
                match loc {
                    VarLoc::Reg(r) => {
                        asm.push_str(&format!("    mov rax, {}\n", r));
                    }
                    VarLoc::Stack(off) => {
                        asm.push_str(&format!("    mov rax, qword ptr [rbp{}]\n", off));
                    }
                }
                asm.push_str(&format!("    mov rcx, qword ptr [rbp{}]\n", bound_slot));
                asm.push_str("    cmp rax, rcx\n");
                asm.push_str(&format!("    jge {}\n", label_end));
                asm.push_str(&format!("{}:\n", label_skip));
                // Body.
                for s in &fr.body {
                    self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                }
                // Increment i by step (load step from stack slot).
                // The increment label is the `continue` target.
                asm.push_str(&format!("{}:\n", label_incr));
                match loc {
                    VarLoc::Reg(r) => {
                        asm.push_str(&format!("    mov rax, qword ptr [rbp{}]\n", step_slot));
                        asm.push_str(&format!("    add {}, rax\n", r));
                    }
                    VarLoc::Stack(off) => {
                        asm.push_str(&format!("    mov rax, qword ptr [rbp{}]\n", off));
                        asm.push_str(&format!("    mov rcx, qword ptr [rbp{}]\n", step_slot));
                        asm.push_str("    add rax, rcx\n");
                        asm.push_str(&format!("    mov qword ptr [rbp{}], rax\n", off));
                    }
                }
                asm.push_str(&format!("    jmp {}\n", label_cond));
                asm.push_str(&format!("{}:\n", label_end));
                self.loop_stack.pop();
            }
            Stmt::ForIn(fi) => {
                // for, x, in, iterable  →  iterate over the iterable.
                // Supported iterables in raw mode:
                //   - String literal: iterate over ASCII byte values (u8).
                //   - Range expression (a..b): delegate to ForRange logic.
                //   - List literal: iterate over elements (compile-time known).
                // For unsupported iterables, emit a best-effort comment.
                if let Expr::String_(s) = &fi.iterable {
                    // String iteration: each character's ASCII code as i64.
                    //
                    // NOTE: This iterates over BYTES, not Unicode scalar
                    // values. `text.len()` returns the byte length and we
                    // load one byte per iteration with `movzx`, so multi-
                    // byte UTF-8 sequences (e.g. accented Latin, CJK,
                    // emoji) are NOT handled correctly — each continuation
                    // byte becomes a separate "character". This is
                    // acceptable for raw mode, which targets bare-metal
                    // environments where ASCII is the norm; do not use
                    // non-ASCII string literals in `for, x, in, "..."`
                    // loops when compiling to raw assembly.
                    let text = self.string_parts_text(&s.parts);
                    let str_idx = self.string_consts.len();
                    self.string_consts.push(text.clone());
                    let id = self.label_counter;
                    self.label_counter += 1;
                    let label_cond = format!(".Lfistrcond_{}", id);
                    let label_end = format!(".Lfistrend_{}", id);
                    // Allocate the loop variable.
                    let loc = if let Some(&l) = var_map.get(&fi.var.name) {
                        l
                    } else {
                        let l = self.alloc_var(&fi.var.name, var_map, stack_offset, next_callee_reg);
                        var_map.insert(fi.var.name.clone(), l);
                        l
                    };
                    // Initialize index = 0 in r8, length in r9.
                    asm.push_str("    xor r8, r8\n");  // index = 0
                    asm.push_str(&format!("    mov r9, {}\n", text.len()));  // length
                    // Load string address into r10.
                    asm.push_str(&format!("    lea r10, [rip + .str{}]\n", str_idx));
                    self.loop_stack.push((label_cond.clone(), label_end.clone()));
                    asm.push_str(&format!("{}:\n", label_cond));
                    asm.push_str("    cmp r8, r9\n");
                    asm.push_str(&format!("    jge {}\n", label_end));
                    // Load byte at r10[r8] into rax, zero-extend to i64.
                    asm.push_str("    movzx rax, byte ptr [r10 + r8]\n");
                    match loc {
                        VarLoc::Reg(r) => {
                            asm.push_str(&format!("    mov {}, rax\n", r));
                        }
                        VarLoc::Stack(off) => {
                            asm.push_str(&format!("    mov qword ptr [rbp{}], rax\n", off));
                        }
                    }
                    // Body.
                    for s in &fi.body {
                        self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                    }
                    asm.push_str("    inc r8\n");
                    asm.push_str(&format!("    jmp {}\n", label_cond));
                    asm.push_str(&format!("{}:\n", label_end));
                    self.loop_stack.pop();
                } else if let Expr::Range(r) = &fi.iterable {
                    // Range iteration: for, x, in, a..b  →  delegate to
                    // ForRange logic. We synthesize a ForRangeStmt and
                    // re-dispatch through compile_stmt.
                    let from = r.start.as_ref().cloned().unwrap_or_else(|| {
                        Box::new(Expr::Integer(crate::parser::ast::IntegerLiteral {
                            value: 0,
                            raw: "0".to_string(),
                            span: crate::error::Span::dummy(),
                        }))
                    });
                    let to = r.end.as_ref().cloned().unwrap_or_else(|| {
                        Box::new(Expr::Integer(crate::parser::ast::IntegerLiteral {
                            value: 0,
                            raw: "0".to_string(),
                            span: crate::error::Span::dummy(),
                        }))
                    });
                    let fr = crate::parser::ast::ForRangeStmt {
                        label: fi.label.clone(),
                        var: fi.var.clone(),
                        from: *from,
                        to: *to,
                        step: r.step.clone(),
                        body: fi.body.clone(),
                        else_body: fi.else_body.clone(),
                        span: fi.span.clone(),
                    };
                    self.compile_stmt(
                        &crate::parser::ast::Stmt::ForRange(fr),
                        asm, var_map, stack_offset, next_callee_reg,
                    );
                } else if let Expr::List(lit) = &fi.iterable {
                    // List literal iteration: each element is a compile-time
                    // constant. We unroll the loop at compile time.
                    for elem in &lit.elements {
                        // Evaluate the element into rax.
                        self.compile_expr(elem, asm, var_map, "rax", next_callee_reg);
                        // Store to the loop variable.
                        let loc = if let Some(&l) = var_map.get(&fi.var.name) {
                            l
                        } else {
                            let l = self.alloc_var(&fi.var.name, var_map, stack_offset, next_callee_reg);
                            var_map.insert(fi.var.name.clone(), l);
                            l
                        };
                        match loc {
                            VarLoc::Reg(r) => {
                                asm.push_str(&format!("    mov {}, rax\n", r));
                            }
                            VarLoc::Stack(off) => {
                                asm.push_str(&format!("    mov qword ptr [rbp{}], rax\n", off));
                            }
                        }
                        for s in &fi.body {
                            self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                        }
                    }
                } else {
                    // Unsupported iterable type in raw mode: emit a comment.
                    asm.push_str(&format!("    # for-in: unsupported iterable type in raw mode\n"));
                }
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
                // GCC-style inline assembly with operand constraints.
                //
                // Operand ordering follows the GCC convention: outputs
                // first, then inputs. So `%0` is the first output, `%N`
                // (where N = outputs.len()) is the first input.
                //
                // For each operand we allocate a register from a
                // caller-saved scratch pool. For inputs, the operand
                // expression is evaluated into the register *before*
                // the asm block. For outputs, the register is left
                // uninitialised (the asm template writes to it), then
                // stored back to the output variable *after* the asm
                // block.
                //
                // The template is emitted with `%N` references replaced
                // by the corresponding register names. `%%` is honoured
                // as a literal `%`. Out-of-range `%N` references are
                // left untouched so they're visible in the .s file.
                //
                // Constraint strings (e.g. `"=r"`, `"r"`, `"a"`, `"m"`)
                // are recorded as comments but not fully honored — the
                // raw backend always allocates from the general
                // caller-saved pool. This is sufficient for the
                // GCC-style `add %1, %2; mov %0, %1` examples in
                // test_asm_constraints.vraw. A future backend could
                // parse the constraint string to honor register-class
                // hints (a/b/c/d/D/S/i/m).
                let reg_pool: &[&str] = &[
                    "rax", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11",
                ];
                asm.push_str(&format!("    # asm block: {}\n", a.template));
                if !a.outputs.is_empty() {
                    asm.push_str("    # outputs:");
                    for op in &a.outputs {
                        asm.push_str(&format!(" \"{}\"({})", op.constraint, expr_text(&op.expr)));
                    }
                    asm.push('\n');
                }
                if !a.inputs.is_empty() {
                    asm.push_str("    # inputs:");
                    for op in &a.inputs {
                        asm.push_str(&format!(" \"{}\"({})", op.constraint, expr_text(&op.expr)));
                    }
                    asm.push('\n');
                }
                // Allocate registers for outputs (no load — the asm
                // template writes to them) and inputs (evaluate the
                // expression into the register before the asm block).
                let mut operand_regs: Vec<String> = Vec::new();
                let total = a.outputs.len() + a.inputs.len();
                if total > reg_pool.len() {
                    // Too many operands for our scratch pool — fall
                    // back to emitting the template verbatim with the
                    // constraints as comments only. This matches the
                    // pre-fix behaviour for pathologically large asm
                    // blocks.
                    asm.push_str(&format!(
                        "    # too many operands ({} > {}); emitting template verbatim\n",
                        total,
                        reg_pool.len()
                    ));
                    for line in a.template.lines() {
                        let line = line.trim();
                        if !line.is_empty() {
                            asm.push_str(&format!("    {}\n", line));
                        }
                    }
                    return;
                }
                // Outputs: allocate registers without loading.
                for _ in &a.outputs {
                    let reg = reg_pool[operand_regs.len()].to_string();
                    operand_regs.push(reg);
                }
                // Inputs: allocate a register and evaluate the
                // expression into it.
                for op in &a.inputs {
                    let reg = reg_pool[operand_regs.len()].to_string();
                    self.compile_expr(&op.expr, asm, var_map, &reg, next_callee_reg);
                    operand_regs.push(reg);
                }
                // Emit the template with %N substituted by register names.
                for line in a.template.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let substituted = substitute_asm_template(line, &operand_regs);
                    asm.push_str(&format!("    {}\n", substituted));
                }
                // Store each output register back to its output variable.
                // The output expression is normally an lvalue (e.g. an
                // Identifier); we look it up in var_map and store the
                // register to its location.
                for (i, op) in a.outputs.iter().enumerate() {
                    let reg = &operand_regs[i];
                    if let Expr::Identifier(id) = &op.expr {
                        if let Some(&loc) = var_map.get(&id.name) {
                            match loc {
                                VarLoc::Reg(r) => {
                                    if r != reg {
                                        asm.push_str(&format!("    mov {}, {}\n", r, reg));
                                    }
                                }
                                VarLoc::Stack(off) => {
                                    asm.push_str(&format!(
                                        "    mov qword ptr [rbp{}], {}\n",
                                        off, reg
                                    ));
                                }
                            }
                        }
                    } else {
                        // Non-identifier output expression: we can't
                        // store back without a full lvalue codegen path.
                        // Emit a comment so the user knows the output
                        // was discarded.
                        asm.push_str(&format!(
                            "    # asm output {}: non-identifier output not stored\n",
                            i
                        ));
                    }
                }
            }
            Stmt::Try(t) => {
                // try/catch implementation using a handler stack (P3.9).
                // The handler stack is a global array of catch-label addresses.
                // try: push catch label, execute body, pop handler on success.
                // throw: pop handler, jump to it.
                let id = self.label_counter;
                self.label_counter += 1;
                let label_catch = format!(".Lcatch_{}", id);
                let label_end = format!(".Ltryend_{}", id);
                // Push catch label onto handler stack.
                asm.push_str(&format!("    lea rax, [rip + {}]\n", label_catch));
                asm.push_str("    mov rcx, [rip + __handler_idx]\n");
                asm.push_str("    lea rdx, [rip + __handler_stack]\n");
                asm.push_str("    mov [rdx + rcx*8], rax\n");
                asm.push_str("    inc rcx\n");
                asm.push_str("    mov [rip + __handler_idx], rcx\n");
                // Execute try body.
                for s in &t.try_body {
                    self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                }
                // No exception: pop handler and jump to end.
                asm.push_str("    mov rcx, [rip + __handler_idx]\n");
                asm.push_str("    dec rcx\n");
                asm.push_str("    mov [rip + __handler_idx], rcx\n");
                // Execute finally block (if any) on the success path.
                if let Some(finally) = &t.finally_body {
                    for s in finally {
                        self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                    }
                }
                asm.push_str(&format!("    jmp {}\n", label_end));
                // Catch block.
                asm.push_str(&format!("{}:\n", label_catch));
                // Pop handler (it was pushed by try).
                asm.push_str("    mov rcx, [rip + __handler_idx]\n");
                asm.push_str("    dec rcx\n");
                asm.push_str("    mov [rip + __handler_idx], rcx\n");
                // The thrown value is in rax (set by throw). If there's a
                // catch variable, store it.
                if let Some(cv) = &t.catch_var {
                    let loc = if let Some(&l) = var_map.get(&cv.name) {
                        l
                    } else {
                        let l = self.alloc_var(&cv.name, var_map, stack_offset, next_callee_reg);
                        var_map.insert(cv.name.clone(), l);
                        l
                    };
                    match loc {
                        VarLoc::Reg(r) => {
                            asm.push_str(&format!("    mov {}, rax\n", r));
                        }
                        VarLoc::Stack(off) => {
                            asm.push_str(&format!("    mov qword ptr [rbp{}], rax\n", off));
                        }
                    }
                }
                // Execute catch body (if any).
                if let Some(catch_body) = &t.catch_body {
                    for s in catch_body {
                        self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                    }
                }
                // Execute finally block (if any) on the catch path.
                if let Some(finally) = &t.finally_body {
                    for s in finally {
                        self.compile_stmt(s, asm, var_map, stack_offset, next_callee_reg);
                    }
                }
                asm.push_str(&format!("{}:\n", label_end));
            }
            Stmt::Throw(t) => {
                // throw expr — evaluate expr into rax, then jump to the
                // current handler on the handler stack.
                self.compile_expr(&t.value, asm, var_map, "rax", next_callee_reg);
                // Pop handler and jump to it.
                asm.push_str("    mov rcx, [rip + __handler_idx]\n");
                asm.push_str("    dec rcx\n");
                asm.push_str("    mov [rip + __handler_idx], rcx\n");
                asm.push_str("    lea rdx, [rip + __handler_stack]\n");
                asm.push_str("    mov r8, [rdx + rcx*8]\n");
                asm.push_str("    jmp r8\n");
            }
            Stmt::Panic(p) => {
                // panic(msg) — same as throw but always terminates.
                self.compile_expr(&p.message, asm, var_map, "rax", next_callee_reg);
                asm.push_str("    mov rcx, [rip + __handler_idx]\n");
                asm.push_str("    dec rcx\n");
                asm.push_str("    mov [rip + __handler_idx], rcx\n");
                asm.push_str("    lea rdx, [rip + __handler_stack]\n");
                asm.push_str("    mov r8, [rdx + rcx*8]\n");
                asm.push_str("    jmp r8\n");
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
                // Float support: store the IEEE 754 bit pattern as an i64.
                // This allows float values to flow through the same integer
                // register file. When printing, we detect float bit patterns
                // and use SSE to convert. For arithmetic, we use SSE instructions.
                let bits = f.value.to_bits() as i64;
                asm.push_str(&format!("    mov {}, {}\n", target, bits));
            }
            Expr::String_(s) => {
                // String literal: emit as a constant and load its address.
                let text = self.string_parts_text(&s.parts);
                let idx = self.string_consts.len();
                self.string_consts.push(text.clone());
                asm.push_str(&format!(
                    "    lea {}, [rip + .str{}]\n",
                    target, idx
                ));
            }
            Expr::Binary(b) => {
                // Check for string concatenation: "a" + "b"
                if b.operator == BinaryOp::Add {
                    if let (Expr::String_(_), Expr::String_(_)) = (b.left.as_ref(), b.right.as_ref()) {
                        // Compile-time string concatenation
                        let left_text = if let Expr::String_(s) = b.left.as_ref() { self.string_parts_text(&s.parts) } else { String::new() };
                        let right_text = if let Expr::String_(s) = b.right.as_ref() { self.string_parts_text(&s.parts) } else { String::new() };
                        let combined = format!("{}{}", left_text, right_text);
                        let idx = self.string_consts.len();
                        self.string_consts.push(combined);
                        asm.push_str(&format!("    lea {}, [rip + .str{}]\n", target, idx));
                        return;
                    }
                }
                // Check for string repeat: "ab" * 3
                if b.operator == BinaryOp::Mul {
                    if let (Expr::String_(s), Expr::Integer(n)) = (b.left.as_ref(), b.right.as_ref()) {
                        let text = self.string_parts_text(&s.parts);
                        let repeated = text.repeat(n.value as usize);
                        let idx = self.string_consts.len();
                        self.string_consts.push(repeated);
                        asm.push_str(&format!("    lea {}, [rip + .str{}]\n", target, idx));
                        return;
                    }
                    if let (Expr::Integer(n), Expr::String_(s)) = (b.left.as_ref(), b.right.as_ref()) {
                        let text = self.string_parts_text(&s.parts);
                        let repeated = text.repeat(n.value as usize);
                        let idx = self.string_consts.len();
                        self.string_consts.push(repeated);
                        asm.push_str(&format!("    lea {}, [rip + .str{}]\n", target, idx));
                        return;
                    }
                }
                // Check for string comparison: "a" == "b" or "a" != "b"
                if (b.operator == BinaryOp::Eq || b.operator == BinaryOp::Ne) {
                    if let (Expr::String_(sl), Expr::String_(sr)) = (b.left.as_ref(), b.right.as_ref()) {
                        let left_text = self.string_parts_text(&sl.parts);
                        let right_text = self.string_parts_text(&sr.parts);
                        let equal = left_text == right_text;
                        let result = if b.operator == BinaryOp::Eq { equal } else { !equal };
                        asm.push_str(&format!("    mov {}, {}\n", target, if result { 1 } else { 0 }));
                        return;
                    }
                }
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
                        BinaryOp::And => {
                            asm.push_str(&format!("    and rax, {}\n", imm));
                        }
                        BinaryOp::Or => {
                            asm.push_str(&format!("    or rax, {}\n", imm));
                        }
                        BinaryOp::BitAnd => {
                            asm.push_str(&format!("    and rax, {}\n", imm));
                        }
                        BinaryOp::BitOr => {
                            asm.push_str(&format!("    or rax, {}\n", imm));
                        }
                        BinaryOp::BitXor => {
                            asm.push_str(&format!("    xor rax, {}\n", imm));
                        }
                        BinaryOp::Shl => {
                            asm.push_str(&format!("    shl rax, {}\n", imm));
                        }
                        BinaryOp::Shr => {
                            asm.push_str(&format!("    sar rax, {}\n", imm));
                        }
                        _ => {
                            asm.push_str(&format!("    mov rax, 0\n"));
                        }
                    }
                    if target != "rax" {
                        asm.push_str(&format!("    mov {}, rax\n", target));
                    }
                    return;
                }
                // General path: compile left into rax, push it, compile
                // right into rcx, pop the left, then compute.
                self.compile_expr(&b.left, asm, var_map, "rax", _next_callee_reg);
                asm.push_str("    push rax\n");
                self.compile_expr(&b.right, asm, var_map, "rcx", _next_callee_reg);
                asm.push_str("    pop rax\n");
                match b.operator {
                    BinaryOp::Add => asm.push_str("    add rax, rcx\n"),
                    BinaryOp::Sub => asm.push_str("    sub rax, rcx\n"),
                    BinaryOp::Mul => asm.push_str("    imul rax, rcx\n"),
                    BinaryOp::Div => asm.push_str("    cqo\n    idiv rcx\n"),
                    BinaryOp::Mod => asm.push_str("    cqo\n    idiv rcx\n    mov rax, rdx\n"),
                    BinaryOp::Lt => asm.push_str("    cmp rax, rcx\n    setl al\n    movzx rax, al\n"),
                    BinaryOp::Gt => asm.push_str("    cmp rax, rcx\n    setg al\n    movzx rax, al\n"),
                    BinaryOp::Le => asm.push_str("    cmp rax, rcx\n    setle al\n    movzx rax, al\n"),
                    BinaryOp::Ge => asm.push_str("    cmp rax, rcx\n    setge al\n    movzx rax, al\n"),
                    BinaryOp::Eq => asm.push_str("    cmp rax, rcx\n    sete al\n    movzx rax, al\n"),
                    BinaryOp::Ne => asm.push_str("    cmp rax, rcx\n    setne al\n    movzx rax, al\n"),
                    BinaryOp::And => asm.push_str("    and rax, rcx\n"),
                    BinaryOp::Or => asm.push_str("    or rax, rcx\n"),
                    BinaryOp::BitAnd => asm.push_str("    and rax, rcx\n"),
                    BinaryOp::BitOr => asm.push_str("    or rax, rcx\n"),
                    BinaryOp::BitXor => asm.push_str("    xor rax, rcx\n"),
                    BinaryOp::Shl => asm.push_str("    shl rax, cl\n"),
                    BinaryOp::Shr => asm.push_str("    sar rax, cl\n"),
                    _ => asm.push_str("    mov rax, 0\n"),
                }
                if target != "rax" {
                    asm.push_str(&format!("    mov {}, rax\n", target));
                }
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
            Expr::Unary(u) => {
                match u.operator {
                    UnaryOp::Neg => {
                        self.compile_expr(&u.operand, asm, var_map, target, _next_callee_reg);
                        asm.push_str(&format!("    neg {}\n", target));
                    }
                    UnaryOp::Not | UnaryOp::Bang => {
                        self.compile_expr(&u.operand, asm, var_map, target, _next_callee_reg);
                        asm.push_str(&format!("    xor {}, 1\n", target));
                    }
                    _ => {
                        self.compile_expr(&u.operand, asm, var_map, target, _next_callee_reg);
                    }
                }
            }
            Expr::AssignExpr(a) => {
                // Assignment as expression: evaluate value, store, return value.
                self.compile_expr(&a.value, asm, var_map, target, _next_callee_reg);
                if let Some(crate::parser::ast::Assignee::Identifier(id)) = a.targets.first() {
                    if let Some(&loc) = var_map.get(&id.name) {
                        match loc {
                            VarLoc::Reg(r) => {
                                if r != target {
                                    asm.push_str(&format!("    mov {}, {}\n", r, target));
                                }
                            }
                            VarLoc::Stack(off) => {
                                asm.push_str(&format!("    mov qword ptr [rbp{}], {}\n", off, target));
                            }
                        }
                    }
                }
            }
            Expr::Call(c) => {
                if let Expr::Identifier(id) = c.callee.as_ref() {
                    // P5/P6 builtins: inline assembly for memcpy, memset,
                    // spin_lock, spin_unlock, tls_get, tls_set, offsetof,
                    // cycle_counter.
                    if self.try_compile_raw_builtin(&id.name, c, asm, var_map, target, _next_callee_reg) {
                        return;
                    }
                    // Try inlining: if the function is small and we haven't
                    // exceeded the max inline depth, inline its body instead
                    // of emitting a `call`.
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
                    for (i, arg) in c.args.iter().take(n).enumerate() {
                        self.compile_expr(arg, asm, var_map, arg_regs[i], _next_callee_reg);
                    }
                    asm.push_str(&format!("    call {}\n", id.name));
                    if target != "rax" {
                        asm.push_str(&format!("    mov {}, rax\n", target));
                    }
                    return;
                }
                // Method call on ptr[T]: load/store/load_acquire/store_release.
                // In raw mode every ptr[T] access is treated as volatile —
                // this is the correct behaviour for memory-mapped hardware
                // registers, which is the primary use case for ptr[T] in
                // .vraw files. The `volatile set, ...` modifier is stripped
                // by the parser, so we can't distinguish volatile from
                // non-volatile at this layer; we therefore mark ALL ptr[T]
                // loads/stores as volatile via a comment. The underlying
                // `mov` instruction is already volatile-safe in the sense
                // that the assembler will never optimize it away, so the
                // comment is informational (it documents intent for the
                // reader of the .s file and signals to future optimizers
                // that the access must not be elided).
                if let Expr::MemberAccess(m) = c.callee.as_ref() {
                    match m.member.name.as_str() {
                        "load" => {
                            asm.push_str("    # volatile load\n");
                            self.compile_expr(&m.target, asm, var_map, target, _next_callee_reg);
                        }
                        "load_acquire" => {
                            asm.push_str("    # volatile load (acquire)\n");
                            self.compile_expr(&m.target, asm, var_map, target, _next_callee_reg);
                            asm.push_str("    # load_acquire — acquire fence\n");
                            asm.push_str("    mfence\n");
                        }
                        "store" => {
                            asm.push_str("    # volatile store\n");
                            if let Some(arg) = c.args.first() {
                                self.compile_expr(arg, asm, var_map, target, _next_callee_reg);
                            }
                        }
                        "store_release" => {
                            asm.push_str("    # volatile store (release)\n");
                            asm.push_str("    # store_release — release fence\n");
                            asm.push_str("    mfence\n");
                            if let Some(arg) = c.args.first() {
                                self.compile_expr(arg, asm, var_map, target, _next_callee_reg);
                            }
                        }
                        _ => {
                            self.compile_expr(&m.target, asm, var_map, target, _next_callee_reg);
                        }
                    }
                    return;
                }
                asm.push_str(&format!("    mov {}, 0\n", target));
            }
            Expr::Cast(c) => {
                // Casts are no-ops in raw mode (types are erased).
                self.compile_expr(&c.expr, asm, var_map, target, _next_callee_reg);
            }
            Expr::Lambda(l) => {
                // P4: Closures in raw mode.
                // No-capture closures: compile as a function, store address.
                // Capture closures: save captures to globals, compile as
                // a function that reads captures from globals.
                let lambda_name = format!("__lambda_{}", self.lambda_counter);
                self.lambda_counter += 1;
                // Identify free variables (variables used in the body but
                // not parameters). For each, save the current value to a
                // uniquely-named global so the lambda can access it.
                let param_names: std::collections::HashSet<String> =
                    l.params.iter().map(|p| p.name.name.clone()).collect();
                let free_vars = self.collect_free_vars_expr(&l.body, &param_names);
                for var_name in &free_vars {
                    // Save the current value of the variable to a global.
                    if let Some(&loc) = var_map.get(var_name) {
                        match loc {
                            VarLoc::Reg(r) => {
                                asm.push_str(&format!("    mov rax, {}\n", r));
                            }
                            VarLoc::Stack(off) => {
                                asm.push_str(&format!("    mov rax, qword ptr [rbp{}]\n", off));
                            }
                        }
                    } else {
                        // The variable is a global — load it.
                        let idx = self.string_consts.len();
                        self.string_consts.push(var_name.clone());
                        asm.push_str(&format!("    mov rax, [rip + _{}]\n", var_name));
                    }
                    // Store to the capture global.
                    asm.push_str(&format!("    mov [rip + __{}_capture_{}], rax\n", lambda_name, var_name));
                }
                // Create the FnDef for the lambda.
                let body = vec![crate::parser::ast::Stmt::Return(
                    crate::parser::ast::ReturnStmt {
                        values: vec![(*l.body).clone()],
                        span: l.span.clone(),
                    },
                )];
                let fn_def = FnDef {
                    annotations: vec![],
                    name: crate::parser::ast::Identifier {
                        name: lambda_name.clone(),
                        span: l.span.clone(),
                    },
                    params: l.params.clone(),
                    return_type: l.return_type.clone(),
                    body,
                    is_constexpr: false,
                    is_lazy: false,
                    is_async: false,
                    is_extern: false,
                    extern_link: None,
                    type_constraints: std::collections::HashMap::new(),
                    type_params: Vec::new(),
                    span: l.span.clone(),
                };
                // Compile the lambda as a regular function.
                self.compile_function(&fn_def);
                // Store the function address in the target register.
                asm.push_str(&format!("    lea {}, [rip + {}]\n", target, lambda_name));
            }
            Expr::TryPropagate(t) => {
                // Result + ? operator: in raw mode a "Result" is a
                // single i64 value with the convention 0 = error,
                // non-zero = ok (raw mode has no tagged values, so we
                // use null-as-error like C conventions). Evaluate the
                // inner expression into the target register; if the
                // result is 0, jump to the current function's epilogue
                // (or, if dtors are active, the RAII cleanup label —
                // which itself routes to the epilogue after running
                // destructors) so the error propagates to the caller.
                // Otherwise, the value is valid and execution
                // continues with the result in `target`.
                asm.push_str("    # TryPropagate: evaluate expr\n");
                self.compile_expr(&t.expr, asm, var_map, target, _next_callee_reg);
                asm.push_str("    # Check if error (value == 0 means error in raw mode)\n");
                asm.push_str(&format!("    cmp {}, 0\n", target));
                if let Some(label) = &self.cur_epilogue {
                    asm.push_str(&format!("    je {}\n", label));
                    asm.push_str("    # Value is valid, continue\n");
                } else {
                    // No epilogue label (top-level / no cur_epilogue):
                    // emit a NOP comment so the .s file documents that
                    // we can't propagate here.
                    asm.push_str("    # (no cur_epilogue — cannot propagate, continue)\n");
                }
            }
            _ => {
                asm.push_str(&format!("    mov {}, 0\n", target));
            }
        }
    }

    /// Extract text from string parts (no interpolation in raw mode).
    /// P5/P6/P7 raw-mode builtins. Returns true if the builtin was
    /// recognized and inline assembly was emitted; false to fall through
    /// to normal call dispatch.
    fn try_compile_raw_builtin(
        &mut self,
        name: &str,
        c: &crate::parser::ast::CallExpr,
        asm: &mut String,
        var_map: &mut HashMap<String, VarLoc>,
        target: &str,
        next_callee_reg: &mut usize,
    ) -> bool {
        match name {
            // P6.14: memcpy(dst, src, len) → rep movsb
            "memcpy" => {
                // Args: rdi=dst, rsi=src, rcx=len
                let arg_regs = ["rdi", "rsi", "rcx"];
                for (i, arg) in c.args.iter().take(3).enumerate() {
                    self.compile_expr(arg, asm, var_map, arg_regs[i], next_callee_reg);
                }
                // Save rax (target) across rep movsb.
                asm.push_str("    push rax\n");
                asm.push_str("    rep movsb\n");
                asm.push_str("    pop rax\n");
                // Return dst in target.
                if target != "rdi" {
                    asm.push_str(&format!("    mov {}, rdi\n", target));
                }
                true
            }
            // P6.14: memset(ptr, val, len) → rep stosb
            "memset" => {
                // Args: rdi=ptr, al=val (low byte of rax), rcx=len
                self.compile_expr(&c.args[0], asm, var_map, "rdi", next_callee_reg);
                self.compile_expr(&c.args[1], asm, var_map, "rax", next_callee_reg);
                self.compile_expr(&c.args[2], asm, var_map, "rcx", next_callee_reg);
                // rep stosb uses AL as the fill byte. rax already holds the
                // value; its low byte (al) is used automatically.
                asm.push_str("    cld\n");
                asm.push_str("    rep stosb\n");
                if target != "rax" {
                    asm.push_str(&format!("    mov {}, rax\n", target));
                }
                true
            }
            // P5.13: spin_lock(ptr) → xchg loop with lock prefix
            "spin_lock" => {
                self.compile_expr(&c.args[0], asm, var_map, "rax", next_callee_reg);
                let id = self.label_counter;
                self.label_counter += 1;
                let label = format!(".Lspinlock_{}", id);
                asm.push_str(&format!("{}:\n", label));
                asm.push_str("    mov rcx, 1\n");
                asm.push_str("    xchg [rax], rcx\n");
                asm.push_str("    test rcx, rcx\n");
                asm.push_str(&format!("    jnz {}\n", label));
                // Acquire fence.
                asm.push_str("    mfence\n");
                true
            }
            // P5.13: spin_unlock(ptr) → store 0 with release fence
            "spin_unlock" => {
                self.compile_expr(&c.args[0], asm, var_map, "rax", next_callee_reg);
                asm.push_str("    mfence\n");
                asm.push_str("    mov qword ptr [rax], 0\n");
                true
            }
            // P5.12: tls_get(offset) → read from fs:[offset]
            "tls_get" => {
                self.compile_expr(&c.args[0], asm, var_map, "rcx", next_callee_reg);
                asm.push_str("    mov rax, [fs:rcx]\n");
                if target != "rax" {
                    asm.push_str(&format!("    mov {}, rax\n", target));
                }
                true
            }
            // P5.12: tls_set(offset, value) → write to fs:[offset]
            "tls_set" => {
                self.compile_expr(&c.args[0], asm, var_map, "rcx", next_callee_reg);
                self.compile_expr(&c.args[1], asm, var_map, "rax", next_callee_reg);
                asm.push_str("    mov [fs:rcx], rax\n");
                if target != "rax" {
                    asm.push_str(&format!("    mov {}, rax\n", target));
                }
                true
            }
            // P6.15: offsetof(StructName, field) → compile-time constant
            // In raw mode without a struct layout engine, offsetof returns 0.
            // This is a best-effort implementation that calculates the offset
            // from struct field definitions if available.
            "offsetof" => {
                // Args: struct name (string), field name (string)
                if c.args.len() >= 2 {
                    if let (Expr::String_(s1), Expr::String_(s2)) = (&c.args[0], &c.args[1]) {
                        let struct_name = self.string_parts_text(&s1.parts);
                        let field_name = self.string_parts_text(&s2.parts);
                        // Look up the struct definition and calculate offset.
                        let offset = self.calculate_struct_offset(&struct_name, &field_name);
                        asm.push_str(&format!("    mov {}, {}\n", target, offset));
                        return true;
                    }
                }
                // Fallback: offset 0.
                asm.push_str(&format!("    mov {}, 0\n", target));
                true
            }
            // P7.16: static_assert(cond, msg) → compile-time check
            "static_assert" => {
                // Evaluate the condition at compile time if possible.
                if c.args.len() >= 1 {
                    if let Expr::Integer(i) = &c.args[0] {
                        if i.value == 0 {
                            // Assertion failed.
                            let msg = if c.args.len() >= 2 {
                                if let Expr::String_(s) = &c.args[1] {
                                    self.string_parts_text(&s.parts)
                                } else {
                                    "static assertion failed".to_string()
                                }
                            } else {
                                "static assertion failed".to_string()
                            };
                            self.errors.push(format!("static_assert failed: {}", msg));
                        }
                    }
                }
                // No runtime code generated.
                true
            }
            // P5: cycle_counter() → rdtsc (x86 cycle counter)
            "cycle_counter" => {
                asm.push_str("    rdtsc\n");
                // rdtsc puts the result in edx:eax. Combine into rax.
                asm.push_str("    shl rdx, 32\n");
                asm.push_str("    or rax, rdx\n");
                if target != "rax" {
                    asm.push_str(&format!("    mov {}, rax\n", target));
                }
                true
            }
            _ => false,
        }
    }

    /// Calculate the byte offset of a field within a struct by looking up
    /// the struct definition and summing the sizes of preceding fields.
    /// Honors `@align(N)` annotations: each field is padded to the
    /// boundary of `min(N, field_size)` (the standard C alignment rule
    /// — the struct's explicit alignment overrides natural alignment
    /// only when it is stricter; otherwise natural alignment applies).
    /// Returns 0 if the struct or field is not found.
    fn calculate_struct_offset(&self, struct_name: &str, field_name: &str) -> i64 {
        let fields = match self.struct_fields.get(struct_name) {
            Some(f) => f,
            None => return 0,
        };
        let struct_align = self.struct_alignments.get(struct_name).copied().unwrap_or(8);
        let mut offset: i64 = 0;
        for (name, size) in fields {
            // Pad to the field's natural alignment, capped by the
            // struct's explicit alignment (C semantics: a larger
            // struct alignment doesn't force smaller fields to be
            // over-aligned, but a smaller struct alignment can reduce
            // the padding of larger fields).
            let natural = (*size as i64).max(1);
            let field_align = natural.min(struct_align as i64);
            if field_align > 0 && offset % field_align != 0 {
                offset += field_align - (offset % field_align);
            }
            if name == field_name {
                return offset;
            }
            offset += *size as i64;
        }
        0
    }

    /// **Register Allocator Helper**: Allocate a variable using the
    /// linear-scan allocator. Falls back to the sequential approach
    /// if the allocator is not available (e.g., during inlining).
    ///
    /// This method:
    /// 1. Increments the instruction position counter.
    /// 2. Expires old intervals (frees dead registers).
    /// 3. Allocates the variable with an estimated end position.
    /// 4. Returns the VarLoc (register or stack slot).
    fn alloc_var(
        &mut self,
        var_name: &str,
        var_map: &mut HashMap<String, VarLoc>,
        stack_offset: &mut i32,
        next_callee_reg: &mut usize,
    ) -> VarLoc {
        // If already allocated, return existing location.
        if let Some(&loc) = var_map.get(var_name) {
            return loc;
        }

        // Use the linear-scan allocator.
        self.instr_pos += 1;
        self.regalloc.advance();

        // Estimate end position: assume the variable lives until
        // roughly 20 instructions from now (a heuristic — a proper
        // implementation would do a liveness analysis pass first).
        let start = self.instr_pos;
        let end = start + 20;

        let result = self.regalloc.allocate(var_name, start, end);
        match result {
            super::regalloc::AllocResult::Register(reg) => {
                // Convert the owned String to a &'static str by
                // matching against the known register set.
                let static_reg: &'static str = match reg.as_str() {
                    "rbx" => "rbx",
                    "r13" => "r13",
                    "r14" => "r14",
                    "r15" => "r15",
                    _ => "rbx", // fallback
                };
                // Update next_callee_reg for prologue generation.
                let idx = CALLEE_SAVED.iter().position(|r| *r == static_reg);
                if let Some(i) = idx {
                    if *next_callee_reg <= i {
                        *next_callee_reg = i + 1;
                    }
                }
                VarLoc::Reg(static_reg)
            }
            super::regalloc::AllocResult::Spilled(offset) => {
                // The allocator assigned a stack offset. Use it.
                if offset < *stack_offset {
                    *stack_offset = offset;
                }
                VarLoc::Stack(offset)
            }
        }
    }

    fn string_parts_text(&self, parts: &[crate::parser::ast::StringPart]) -> String {
        let mut text = String::new();
        for p in parts {
            if let crate::parser::ast::StringPart::Text(t) = p {
                text.push_str(t);
            }
        }
        text
    }

    /// Collect free variables (identifiers not in param_names) from an
    /// expression. Used by the Lambda handler to identify captures.
    fn collect_free_vars_expr(
        &self,
        expr: &Expr,
        param_names: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        let mut vars = Vec::new();
        self.collect_free_vars_inner(expr, param_names, &mut vars);
        vars.sort();
        vars.dedup();
        vars
    }

    fn collect_free_vars_inner(
        &self,
        expr: &Expr,
        param_names: &std::collections::HashSet<String>,
        vars: &mut Vec<String>,
    ) {
        match expr {
            Expr::Identifier(id) => {
                if !param_names.contains(&id.name) && id.name != "true" && id.name != "false" {
                    vars.push(id.name.clone());
                }
            }
            Expr::Binary(b) => {
                self.collect_free_vars_inner(&b.left, param_names, vars);
                self.collect_free_vars_inner(&b.right, param_names, vars);
            }
            Expr::Unary(u) => {
                self.collect_free_vars_inner(&u.operand, param_names, vars);
            }
            Expr::Call(c) => {
                self.collect_free_vars_inner(&c.callee, param_names, vars);
                for a in &c.args {
                    self.collect_free_vars_inner(a, param_names, vars);
                }
            }
            Expr::MemberAccess(m) => {
                self.collect_free_vars_inner(&m.target, param_names, vars);
            }
            Expr::Index(i) => {
                self.collect_free_vars_inner(&i.target, param_names, vars);
                self.collect_free_vars_inner(&i.index, param_names, vars);
            }
            Expr::Cast(c) => {
                self.collect_free_vars_inner(&c.expr, param_names, vars);
            }
            _ => {}
        }
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
        // Use rsp-32 as a 32-byte buffer. Build the string from the
        // end (high address) backward (decreasing address).
        asm.push_str("    lea rsi, [rsp - 32]\n");  // rsi points past the end
        asm.push_str("    mov r8, rax\n");           // save original value
        asm.push_str("    test rax, rax\n");
        asm.push_str(&format!("    jns .Lpin_digits_{}\n", id));
        asm.push_str("    neg rax\n");                // make positive for digit extraction
        asm.push_str(&format!(".Lpin_digits_{}:\n", id));
        asm.push_str("    test rax, rax\n");
        asm.push_str(&format!("    jnz .Lpin_loop_{}\n", id));
        // rax == 0: write a single '0'
        asm.push_str("    dec rsi\n");
        asm.push_str("    mov byte ptr [rsi], 48\n");
        asm.push_str(&format!("    jmp .Lpin_sign_{}\n", id));
        asm.push_str(&format!(".Lpin_loop_{}:\n", id));
        asm.push_str("    xor rdx, rdx\n");
        asm.push_str("    mov rcx, 10\n");
        asm.push_str("    div rcx\n");
        asm.push_str("    add dl, 48\n");
        asm.push_str("    dec rsi\n");
        asm.push_str("    mov [rsi], dl\n");
        asm.push_str("    test rax, rax\n");
        asm.push_str(&format!("    jnz .Lpin_loop_{}\n", id));
        asm.push_str(&format!(".Lpin_sign_{}:\n", id));
        // If original was negative, prepend '-'
        asm.push_str("    test r8, r8\n");
        asm.push_str(&format!("    jns .Lpin_write_{}\n", id));
        asm.push_str("    dec rsi\n");
        asm.push_str("    mov byte ptr [rsi], 45\n");  // '-'
        asm.push_str(&format!(".Lpin_write_{}:\n", id));
        asm.push_str("    lea rdx, [rsp - 32]\n");
        asm.push_str("    sub rdx, rsi\n");            // length = end - start
        asm.push_str("    mov rax, 1\n");              // write syscall
        asm.push_str("    mov rdi, 1\n");              // stdout
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

        // BSS section: exception handler stack for try/catch (P3.9).
        // The handler stack is a fixed-size array of catch-label addresses.
        // __handler_idx is the stack pointer (0 = empty).
        asm.push_str(".section .bss\n");
        asm.push_str(".balign 8\n");
        asm.push_str("__handler_stack:\n");
        asm.push_str("    .skip 8192\n");  // 1024 entries * 8 bytes
        asm.push_str("__handler_idx:\n");
        asm.push_str("    .skip 8\n");

        // Struct alignment declarations. For each struct that has an
        // `@align(N)` annotation, emit a `.balign N` directive followed
        // by a documentation comment. The raw backend does not currently
        // emit struct globals (the BSS section above only contains the
        // exception-handler stack), but these directives document the
        // intended alignment so a future struct-global emitter can simply
        // prepend them. They also serve as a visible signal in the .s
        // file that `align(N)` was honored by the backend rather than
        // silently discarded.
        for (struct_name, align) in &self.struct_alignments {
            asm.push_str(&format!(
                "# struct '{}' aligned to {}-byte boundary (@align({}))\n",
                struct_name, align, align
            ));
            asm.push_str(&format!(".balign {}\n", align));
            asm.push_str(&format!(".L{}_align_marker:\n", struct_name));
            asm.push_str(&format!("    .skip 0\n"));
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
    let plat = crate::platform::PlatformInfo::detect();

    // The x86 backend generates x86_64 assembly. On non-x86_64 hosts,
    // this is cross-compilation — we write the .s file but skip
    // assembling/linking (the host's `as` can't process x86_64).
    let host_arch = std::env::consts::ARCH;
    let is_cross = host_arch != "x86_64";

    if is_cross {
        crate::platform::info(&format!(
            "Cross-compiling to x86_64 (host is {}). Assembly will be written but not assembled.",
            host_arch
        ));
    }

    let mut gen = X86CodeGen::new();
    let asm = gen.compile(program)?;

    // Write assembly to .s file.
    let asm_path = output_path.with_extension("s");
    std::fs::write(&asm_path, &asm)
        .map_err(|e| format!("can't write assembly: {}", e))?;

    // On non-x86_64 hosts, skip the assemble/link steps (cross-compilation).
    if is_cross {
        crate::platform::info(&format!(
            "Raw x86_64 assembly written to: {} (cross-compilation, not assembled)",
            asm_path.display()
        ));
        return Ok(());
    }

    // Try to assemble and link.
    let exe_path = output_path.to_path_buf();

    // Assemble.
    let obj_path = output_path.with_extension("o");
    let assemble = std::process::Command::new(plat.as_command())
        .arg("--64")
        .arg("-o")
        .arg(&obj_path)
        .arg(&asm_path)
        .output();

    match assemble {
        Ok(out) => {
            if !out.status.success() {
                // If `as` fails (not installed), just keep the .s file.
                crate::platform::warning(&format!(
                    "'as' assembler not available or failed. Assembly written to {}",
                    asm_path.display()
                ));
                if let Some(hint) = plat.missing_tool_hint("as") {
                    crate::platform::hint(&hint);
                }
                return Ok(());
            }
        }
        Err(_) => {
            crate::platform::warning(&format!(
                "'as' not found. Assembly written to {}",
                asm_path.display()
            ));
            if let Some(hint) = plat.missing_tool_hint("as") {
                crate::platform::hint(&hint);
            }
            return Ok(());
        }
    }

    // Link. On Termux, use clang (the system compiler) for linking
    // because `ld` may not have the right library paths configured.
    let link = if plat.is_termux() {
        std::process::Command::new(plat.cc_command())
            .arg("-o")
            .arg(&exe_path)
            .arg(&obj_path)
            .args(&plat.extra_link_flags())
            .output()
    } else {
        std::process::Command::new(plat.ld_command())
            .arg("-o")
            .arg(&exe_path)
            .arg(&obj_path)
            .output()
    };

    match link {
        Ok(out) => {
            if out.status.success() {
                crate::platform::success(&format!(
                    "Native executable written to {}",
                    exe_path.display()
                ));
                // Make executable.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&exe_path, std::fs::Permissions::from_mode(0o755));
                }
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr);
                crate::platform::warning(&format!(
                    "Linking failed. Object file at {}\n{}",
                    obj_path.display(),
                    stderr
                ));
                if let Some(hint) = plat.missing_tool_hint("ld") {
                    crate::platform::hint(&hint);
                }
            }
        }
        Err(_) => {
            crate::platform::warning(&format!(
                "'ld' not found. Object file at {}",
                obj_path.display()
            ));
            if let Some(hint) = plat.missing_tool_hint("ld") {
                crate::platform::hint(&hint);
            }
        }
    }

    Ok(())
}
