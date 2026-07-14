//! Bytecode instruction set for the Vredrs virtual machine.
//!
//! The instruction set covers the hot-path constructs that the compiler
//! lowers natively: literals, arithmetic, comparison, control flow,
//! function calls, container construction/indexing, iteration, and
//! builtins.
//!
//! All language constructs have dedicated opcodes — the former
//! `EvalAst`/`ExecAstStmt` AST-fallback opcodes have been removed.
//! Lambdas use `MakeClosure`; try/catch uses `PushHandler`/`PopHandler`/
//! `Throw`; comprehensions, slices, and method calls are lowered to
//! builtin calls or dedicated opcodes.

use crate::parser::ast::*;
use std::collections::HashMap;

/// A bytecode instruction: opcode + operands.
#[derive(Debug, Clone)]
pub enum Instr {
    // --- Constants and literals ---
    ConstInt(i64),
    ConstFloat(f64),
    ConstBool(bool),
    ConstNull,
    ConstStr(usize),
    NewList(usize),
    NewDict(usize),
    NewTuple(usize),

    // --- Stack operations ---
    Pop,
    Dup,
    Swap,
    Pick(usize),

    // --- Local variable access ---
    LoadLocal(usize),
    StoreLocal(usize),
    LoadGlobal(usize),
    StoreGlobal(usize),
    /// Load the current `self` (for methods).
    LoadSelf,
    /// Load a field of the object on top of the stack.
    LoadField(usize),
    /// Store top of stack into a field of the object below it.
    StoreField(usize),

    // --- Arithmetic and comparison ---
    Add, Sub, Mul, Div, FloorDiv, Mod,
    Eq, Ne, Lt, Gt, Le, Ge,
    Neg, Not,
    And, Or,

    // --- Control flow ---
    Jump(usize),
    JumpIfFalse(usize),
    JumpIfTrue(usize),
    Nop,

    // --- Function calls and returns ---
    /// Call a function by name index in constant pool. Operand: (name_idx, argc).
    CallByName(usize, usize),
    /// Call the value on the stack. Operand: argc.
    Call(usize),
    /// Call a method on the object. Operand: (method_name_idx, argc).
    CallMethod(usize, usize),
    Return,
    /// Return from a function without a value (pushes Null first).
    ReturnVoid,

    // --- Container operations ---
    IndexGet,
    IndexSet,
    /// Pop a container; push its length.
    Len,

    // --- Iteration ---
    /// Push an iterator over the value on top of stack.
    /// For lists/tuples: index-based. For dicts: keys. For ranges: integers.
    Iter,
    /// Pop iterator; if it has a next value, push it and jump to target.
    /// Otherwise jump to the second target.
    /// Operands: (next_target, end_target)
    IterNext(usize, usize),

    // --- Classes and objects ---
    /// Define a class. Operand: class name index.
    /// Pops field values and method pointers off the stack.
    NewClass(usize),
    /// Instantiate an object of a class. Operand: argc for `new()`.
    NewObject(usize),
    /// Load a method bound to the object on top of stack.
    LoadMethod(usize),

    // --- Generators ---
    /// Create a generator from a function. Operand: function name index.
    NewGenerator(usize),
    /// Resume a generator. Pushes the next yielded value.
    Resume,

    // --- Exceptions ---
    /// Push a try/catch handler. Operand: catch_target.
    PushHandler(usize),
    /// Pop the current try/catch handler.
    PopHandler,
    /// Throw the value on top of stack.
    Throw,

    // --- Defer (native, no AST fallback) ---
    /// Push a deferred code block onto the current frame's defer stack.
    /// Operand: the PC of the deferred code block. The VM records this
    /// PC; when the function returns (or throws), deferred blocks run in
    /// LIFO order before the frame is popped.
    DeferPush(usize),
    /// Run all deferred blocks for the current frame in LIFO order.
    /// Emitted just before Return/Throw. Each deferred block ends with
    /// a Jump back to the RunDefers continuation.
    RunDefers,

    // --- Modules ---
    /// Import a module by path index. Pushes the module's exports dict.
    Import(usize),

    // --- Builtins ---
    /// Call a builtin by name index. Operand: argc.
    CallBuiltin(usize, usize),
    /// Super call: call the parent class's method by name index.
    /// Args are on the stack. Uses method_context to find the parent.
    SuperCall(usize, usize),
    /// Yield a value in a generator function (eager model: push to
    /// `gen_yield_buffer` and continue; outside a generator, push back).
    Yield,
    /// Print the top of stack without newline.
    Print,
    /// Print the top of stack with newline.
    Println,
    /// Emit a literal newline to stdout (used by `println` with no args,
    /// or after a sequence of `Print` calls).
    Newline,

    // --- Closures ---
    /// Create a closure value from a pre-compiled lambda function.
    /// Operand: the lambda's runtime name (e.g. "<lambda_0>"). The
    /// corresponding FnDef is stored in `Module::lambda_fn_defs` (keyed
    /// by the same name) so the VM can find the body when the closure
    /// is later called. At creation time, the VM snapshots the currently
    /// visible lexical environment (globals + local_scopes + the
    /// enclosing bytecode frame's named locals) into `lambda_captures`
    /// under the same name; when the closure is called, those captures
    /// are pushed as a scope so the body sees the snapshotted values.
    /// This replaces the former `EvalAst(Expr::Lambda)` emit and keeps
    /// the closure-capture semantics identical to the AST path.
    MakeClosure(String),

    // --- Misc ---
    Halt,
    /// No-op marker for debugging.
    Debug(usize),
    /// Pattern-match the value on top of stack against the given pattern.
    /// Pops the value, pushes a Bool indicating whether the pattern matched.
    /// Used by `match` cases with complex patterns (Or, Tuple, List, Dict,
    /// Struct, EnumVariant) that don't have dedicated bytecode lowering.
    /// Variable bindings are written to the current scope as a side effect.
    MatchPattern(Box<crate::parser::ast::Pattern>),
}

/// A compiled bytecode module: constant pool + instruction sequence +
/// function table.
#[derive(Debug, Clone)]
pub struct Module {
    /// String constants referenced by ConstStr, LoadGlobal, StoreGlobal,
    /// CallByName, etc.
    pub constants: Vec<String>,
    /// The instruction sequence (top-level code, followed by function
    /// bodies after the `Halt` instruction).
    pub code: Vec<Instr>,
    /// Maximum stack depth (for pre-allocation).
    pub max_stack: usize,
    /// Number of local variable slots for top-level code.
    pub num_locals: usize,
    /// Compiled functions: name → (code, num_params, num_locals).
    pub functions: HashMap<String, FuncDef>,
    /// Class definitions: name → (field_names, method_names).
    pub classes: HashMap<String, ClassDef>,
    /// Function-name → (entry PC, num_locals) for functions whose bodies
    /// have been compiled to bytecode (appended after `Halt`). Only
    /// "clean" functions — those whose compiled body contains no
    /// `EvalAst`/`ExecAstStmt` fallback — are registered here. When the
    /// VM's `CallByName` handler finds a name in this map, it jumps to
    /// the bytecode directly instead of AST-walking the body.
    pub fn_entry_pcs: HashMap<String, (usize, usize)>,
    /// Lambda function definitions collected during compilation. The VM
    /// uses these to look up the real FnDef body when a lambda is called
    /// via call_value (e.g. map(fn(x) x*2, list)). The key is the lambda
    /// name (e.g. "<lambda_0>").
    pub lambda_fn_defs: HashMap<String, crate::parser::ast::FnDef>,
    /// Local variable names (in slot order) for each function whose body
    /// was compiled to bytecode and registered in `fn_entry_pcs`. The VM
    /// uses this when executing `MakeClosure` inside a bytecode-compiled
    /// function: it pairs the names with the current frame's `locals`
    /// Vec to snapshot the enclosing function's parameters/locals into
    /// the closure's capture map (mirroring what the AST interpreter
    /// path captures from `local_scopes`). Keyed by function name.
    /// Top-level code is not represented here (top-level `set` writes to
    /// globals, which are already captured by the globals snapshot).
    pub fn_local_names: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct FuncDef {
    pub name: String,
    pub params: Vec<String>,
    pub code: Vec<Instr>,
    pub num_locals: usize,
    pub is_generator: bool,
}

#[derive(Debug, Clone)]
pub struct ClassDef {
    pub name: String,
    pub parent: Option<String>,
    pub fields: Vec<String>,
    pub methods: Vec<String>,
}

impl Module {
    pub fn new() -> Self {
        Module {
            constants: Vec::new(),
            code: Vec::new(),
            max_stack: 1024,
            num_locals: 0,
            functions: HashMap::new(),
            classes: HashMap::new(),
            fn_entry_pcs: HashMap::new(),
            lambda_fn_defs: HashMap::new(),
            fn_local_names: HashMap::new(),
        }
    }

    /// Add a string constant and return its index.
    pub fn intern_str(&mut self, s: &str) -> usize {
        if let Some(idx) = self.constants.iter().position(|c| c == s) {
            return idx;
        }
        let idx = self.constants.len();
        self.constants.push(s.to_string());
        idx
    }

    /// Append an instruction and return its index (for jump targets).
    pub fn emit(&mut self, instr: Instr) -> usize {
        let idx = self.code.len();
        self.code.push(instr);
        idx
    }
}

impl Default for Module {
    fn default() -> Self {
        Self::new()
    }
}
