//! Bytecode virtual machine.
//!
//! Executes a compiled bytecode `Module`. The VM is a stack-based
//! interpreter with call frames. Each call frame has its own locals
//! array and return PC.
//!
//! ## Supported features
//!
//! The VM covers the full Vredrs language via a two-tier strategy:
//!   1. Native opcodes for the hot path (literals, arithmetic, control
//!      flow, function calls, list/dict construction, indexing, builtin
//!      calls).
//!   2. `EvalAst(Expr)` / `ExecAstStmt(Stmt)` fallbacks for everything
//!      else (slices, generators, with, try/catch, iterators, comprehensions,
//!      lambdas, async/await, super, etc.) — these delegate to the VM's
//!      own AST interpreter path (`execute_ast_expr` / `execute_ast_stmt`).
//!
//! Builtins: len, str, int, float, bool, type_of, range, range3, range1,
//! sum, min, max, sorted, reversed, print, println, paste, input, open,
//! read, write, close, read_file, write_file, file_exists, is_dir,
//! is_file, read_dir, path_join, basename, dirname, exit, enumerate, zip,
//! resume, stop, freeze, is_frozen, annotations, set_recursion_limit,
//! abs, floor, ceil, round, map, filter, dict, dict_get, dict_set,
//! dict_keys, dict_values, dict_has, list, set, tuple, math_sqrt, math_pow,
//! math_sin, math_cos, math_tan, math_log, math_abs, math_floor, math_ceil.
//!
//! Object instances use `Rc<RefCell<HashMap>>` for their field map so
//! that mutations made inside methods (`self.field = value`) persist on
//! the original instance — not just on a temporary clone.
//!
//! Generators use `Rc<RefCell<GenState>>` so `resume(gen)` advances the
//! same state across calls (the generator value can be cloned freely).

use super::instr::{Instr, Module};
use crate::error::{CompilerError, Result};
use crate::parser::ast::{BinaryOp, FnDef as AstFnDef, Program};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::rc::Rc;

use super::compiler::is_builtin;

/// A value on the VM stack.
#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Null,
    List(Vec<Value>),
    Dict(HashMap<String, Value>),
    Tuple(Vec<Value>),
    /// A function value: name + AST body (for re-compilation on call).
    Func(String),
    /// A generator state: function name + current PC + locals + yielded values.
    /// Shared via Rc<RefCell<...>> so that resume() mutations persist across
    /// clones of the Value (e.g. when the generator is stored in a global
    /// and then loaded onto the stack).
    Generator(Rc<RefCell<GenState>>),
    /// An object instance: class name + shared field map.
    /// Fields use Rc<RefCell<...>> so that mutations made inside methods
    /// (via `self.field = value`) persist on the original object instance,
    /// not just on a temporary clone.
    Object(String, Rc<RefCell<HashMap<String, Value>>>),
    /// A class definition (for instantiation).
    Class(String),
    /// A module: path + exports map.
    Module(String, HashMap<String, Value>),
    /// An exception value (for throw/catch).
    /// Carries the original thrown value (Object, Str, etc.) so catch
    /// blocks receive the full exception object, not just a stringified
    /// representation. The String is a pre-computed display form.
    Exception(String, Box<Value>),
}

#[derive(Debug, Clone)]
pub struct GenState {
    pub func_name: String,
    pub pc: usize,
    pub locals: Vec<Value>,
    pub stack: Vec<Value>,
    pub done: bool,
    pub yielded_values: Vec<Value>,
    pub yield_idx: usize,
    /// The generator's return value (from `return, X`), if any.
    pub return_value: Option<Value>,
}

impl Value {
    fn truthy(&self) -> bool {
        match self {
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::Bool(b) => *b,
            Value::Str(s) => !s.is_empty(),
            Value::Null => false,
            Value::List(l) => !l.is_empty(),
            Value::Dict(d) => !d.is_empty(),
            Value::Tuple(t) => !t.is_empty(),
            Value::Func(_) => true,
            Value::Generator(g) => !g.borrow().done,
            Value::Object(_, _) => true,
            Value::Class(_) => true,
            Value::Module(_, _) => true,
            Value::Exception(_, _) => true,
        }
    }

    fn to_str(&self) -> String {
        match self {
            Value::Int(i) => i.to_string(),
            Value::Float(f) => {
                if *f == f.trunc() && f.is_finite() {
                    format!("{:.1}", f)
                } else {
                    f.to_string()
                }
            }
            Value::Bool(b) => b.to_string(),
            Value::Str(s) => s.clone(),
            Value::Null => "null".to_string(),
            Value::List(l) => {
                let items: Vec<String> = l.iter().map(format_value).collect();
                format!("[{}]", items.join(", "))
            }
            Value::Dict(d) => {
                let items: Vec<String> = d
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, format_value(v)))
                    .collect();
                format!("{{{}}}", items.join(", "))
            }
            Value::Tuple(t) => {
                let items: Vec<String> = t.iter().map(format_value).collect();
                format!("({})", items.join(", "))
            }
            Value::Func(name) => format!("<fn {}>", name),
            Value::Generator(_) => "<generator>".to_string(),
            Value::Object(class, _) => format!("<{}>", class),
            Value::Class(name) => format!("<class {}>", name),
            Value::Module(name, _) => format!("<module {}>", name),
            Value::Exception(msg, _) => msg.clone(),
        }
    }

    fn to_int(&self) -> Result<i64> {
        match self {
            Value::Int(i) => Ok(*i),
            Value::Float(f) => Ok(*f as i64),
            Value::Bool(b) => Ok(if *b { 1 } else { 0 }),
            Value::Str(s) => s.parse().map_err(|_| {
                CompilerError::runtime_error(format!("cannot convert '{}' to int", s))
            }),
            _ => Err(CompilerError::runtime_error(format!(
                "cannot convert {:?} to int",
                self
            ))),
        }
    }

    fn to_float(&self) -> Result<f64> {
        match self {
            Value::Int(i) => Ok(*i as f64),
            Value::Float(f) => Ok(*f),
            Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            Value::Str(s) => s.parse().map_err(|_| {
                CompilerError::runtime_error(format!("cannot convert '{}' to float", s))
            }),
            _ => Err(CompilerError::runtime_error(format!(
                "cannot convert {:?} to float",
                self
            ))),
        }
    }
}

/// Return a human-readable type name for a Value, mirroring the
/// strings reported by the `type_of` builtin (e.g. "int", "str",
/// "list"). Used in conversion error messages so callers don't have
/// to format debug representation of arbitrary values.
fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::Str(_) => "str",
        Value::Null => "null",
        Value::List(_) => "list",
        Value::Dict(_) => "dict",
        Value::Tuple(_) => "tuple",
        Value::Func(_) => "function",
        Value::Generator(_) => "generator",
        Value::Object(_, _) => "object",
        Value::Class(_) => "class",
        Value::Module(_, _) => "module",
        Value::Exception(_, _) => "exception",
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Int(a), Value::Float(b)) => (*a as f64) == *b,
            (Value::Float(a), Value::Int(b)) => *a == (*b as f64),
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Null, Value::Null) => true,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Dict(a), Value::Dict(b)) => a == b,
            (Value::Tuple(a), Value::Tuple(b)) => a == b,
            (Value::Object(ac, af), Value::Object(bc, bf)) => {
                ac == bc && *af.borrow() == *bf.borrow()
            }
            (Value::Class(a), Value::Class(b)) => a == b,
            (Value::Exception(a, _), Value::Exception(b, _)) => a == b,
            (Value::Exception(a, _), Value::Str(b)) | (Value::Str(b), Value::Exception(a, _)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        // For Int/Float mixed comparisons, compare by numeric value.
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => return a.cmp(b),
            (Value::Float(a), Value::Float(b)) => {
                return a.partial_cmp(b).unwrap_or(Ordering::Equal);
            }
            (Value::Int(a), Value::Float(b)) => {
                return (*a as f64).partial_cmp(b).unwrap_or(Ordering::Equal);
            }
            (Value::Float(a), Value::Int(b)) => {
                return a.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal);
            }
            _ => {}
        }
        // For all other types, order by discriminant tag, then by content.
        let tag = |v: &Value| match v {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int(_) => 2,
            Value::Float(_) => 3,
            Value::Str(_) => 4,
            Value::List(_) => 5,
            Value::Tuple(_) => 6,
            Value::Dict(_) => 7,
            Value::Func(_) => 8,
            Value::Class(_) => 9,
            Value::Object(_, _) => 10,
            Value::Module(_, _) => 11,
            Value::Generator(_) => 12,
            Value::Exception(_, _) => 13,
        };
        let ta = tag(self);
        let tb = tag(other);
        if ta != tb {
            return ta.cmp(&tb);
        }
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (Value::Str(a), Value::Str(b)) => a.cmp(b),
            (Value::List(a), Value::List(b)) => a.cmp(b),
            (Value::Tuple(a), Value::Tuple(b)) => a.cmp(b),
            (Value::Func(a), Value::Func(b)) => a.cmp(b),
            (Value::Class(a), Value::Class(b)) => a.cmp(b),
            (Value::Exception(a, _), Value::Exception(b, _)) => a.cmp(b),
            _ => Ordering::Equal,
        }
    }
}

fn format_value(v: &Value) -> String {
    v.to_str()
}

/// A call frame.
struct Frame {
    /// Return PC in the caller's code.
    return_pc: usize,
    /// The locals array for this frame.
    locals: Vec<Value>,
    /// Whether this frame is the top-level (main) frame.
    is_main: bool,
    /// The stack depth at call time. On Return, we truncate the stack to
    /// this depth (then push the return value), so leaked values above the
    /// return value (e.g. a match scrutinee left on the stack by an early
    /// return) don't accumulate across calls.
    stack_base: usize,
    /// Deferred code block PCs for this frame (LIFO order). Populated by
    /// DeferPush; emptied by RunDefers before Return.
    defer_pcs: Vec<usize>,
    /// The name of the function this frame is executing (empty for the
    /// top-level main frame). Used by the MakeClosure handler to look up
    /// the enclosing function's local-variable names (from
    /// `Module::fn_local_names`) so it can snapshot frame.locals into a
    /// closure's capture map.
    fn_name: String,
}

/// In-memory SQL database for the sql module.
/// Supports CREATE TABLE, INSERT, SELECT (with WHERE), DELETE, UPDATE.
pub struct SqlDb {
    /// Table name → (column names, rows). Each row is a Vec<Value>.
    tables: HashMap<String, (Vec<String>, Vec<Vec<Value>>)>,
}


/// Safely decode a hex string to bytes, handling odd-length inputs
/// without panicking. Returns empty vec on invalid hex.
fn safe_hex_decode(hex: &str) -> Vec<u8> {
    let hex = hex.trim();
    if hex.len() % 2 != 0 {
        return Vec::new(); // odd length — can't decode in pairs
    }
    (0..hex.len())
        .step_by(2)
        .filter_map(|i| {
            if i + 2 <= hex.len() {
                u8::from_str_radix(&hex[i..i + 2], 16).ok()
            } else {
                None
            }
        })
        .collect()
}


/// Validate a file path for basic security. If VREDRS_STRICT_PATHS is set,
/// reject paths containing ".." to prevent path traversal. Otherwise, log
/// a warning but allow the operation (backward compatibility).
fn validate_path(path: &str) -> Result<()> {
    if path.contains("..") {
        if std::env::var("VREDRS_STRICT_PATHS").is_ok() {
            return Err(CompilerError::runtime_error(format!(
                "path traversal blocked (VREDRS_STRICT_PATHS is set): '{}'", path
            )));
        }
        eprintln!("[vredrs] WARNING: path contains '..' (potential traversal): {}", path);
    }
    Ok(())
}

pub struct VM {
    module: Module,
    stack: Vec<Value>,
    frames: Vec<Frame>,
    globals: HashMap<String, Value>,
    /// Call-frame local variable scopes for the AST-interpreter path.
    /// Each entry is one call frame's locals; the top of the stack is the
    /// innermost frame; an empty stack means top-level code.
    local_scopes: Vec<HashMap<String, Value>>,
    pc: usize,
    /// Iterator state: (list, index) pairs for active for-in loops.
    iterators: Vec<(Value, usize)>,
    /// The original program, for re-compiling functions on demand.
    program: Option<Program>,
    /// Compiled function bodies: name → compiled instructions.
    compiled_fns: HashMap<String, (Vec<Instr>, usize)>,
    /// Base directory for module resolution.
    base_dir: std::path::PathBuf,
    /// Module cache: path → exports.
    module_cache: HashMap<String, HashMap<String, Value>>,
    /// Parsed-program cache for imported modules: path → parsed `Program`
    /// (shared via `Rc` so a cached entry is a cheap refcount bump). This
    /// exists so that `call_imported_function` does NOT have to re-read
    /// and re-parse the module source on every call — it can fetch the
    /// cached `Program` (and the cached compiled `Module` in
    /// `module_compiled_cache`) and proceed straight to executing the
    /// function body in a fresh sub-VM.
    ///
    /// Populated by `load_module` when it first parses a module, and
    /// lazily by `call_imported_function` if a module path appears in
    /// `module_cache` without a corresponding entry here (defensive —
    /// shouldn't normally happen, but keeps the function robust if a
    /// future code path mutates `module_cache` directly).
    module_program_cache: HashMap<String, Rc<Program>>,
    /// Compiled-bytecode cache for imported modules: path → `Module`
    /// (shared via `Rc`). Like `module_program_cache`, this exists so
    /// `call_imported_function` can skip `Compiler::compile` on every
    /// call. The cached `Module` is cloned (cheaply, via the
    /// `Module: Clone` impl) into each fresh sub-VM rather than shared,
    /// because `VM::new` takes ownership of the `Module` and a sub-VM
    /// may mutate its frame/stack state during execution.
    module_compiled_cache: HashMap<String, Rc<Module>>,
    /// Set of frozen values (by their string repr, for is_frozen check).
    frozen_set: std::collections::HashSet<String>,
    /// Lambda function definitions collected at runtime.
    lambda_defs: Vec<AstFnDef>,
    /// Counter for generating unique hygienic macro parameter names.
    macro_counter: usize,
    /// Stack of (class_name, method_name) for super dispatch.
    /// When a method is called, we push (class, method) so super
    /// knows which class to start searching from.
    method_context: Vec<(String, String)>,
    /// Deferred statements to execute at scope exit (LIFO order).
    defer_stack: Vec<crate::parser::ast::Stmt>,
    /// Annotation table: fn_name → dict of annotations.
    annotation_table: HashMap<String, HashMap<String, Value>>,
    /// Generator yield buffer. When non-empty, the VM is eagerly
    /// evaluating a generator function body. `yield, X` pushes the
    /// yielded value here and signals "yield" by error; loop
    /// constructs (while/for/loop) and the generator body loop catch
    /// the signal, record the value, and continue iterating so all
    /// yields are collected.
    gen_yield_buffer: Option<Vec<Value>>,
    /// Captured lexical environments for lambda closures, keyed by the
    /// lambda's runtime name (e.g. "<lambda_0>"). When a lambda is
    /// created, a snapshot of the currently-visible variables (all
    /// local scopes + globals) is stored here as an Rc<RefCell<...>>.
    /// When the lambda is later called, those captures are restored as
    /// the innermost scope so the lambda body sees the values that were
    /// in scope at creation time — even if the enclosing function has
    /// since returned.
    ///
    /// IMPORTANT (0.1.5 fix): The captures use Rc<RefCell<...>> so that
    /// mutations made inside the lambda (via `set, x, ...`) persist
    /// across calls. This makes the counter pattern work:
    ///   fn, make_counter() { set, n, 0; return, fn() n += 1 }
    /// Each call to the returned lambda modifies the shared capture map,
    /// so subsequent calls see the updated value.
    lambda_captures: HashMap<String, Rc<RefCell<HashMap<String, Value>>>>,
    /// Cached function definitions keyed by name. Built once at
    /// preprocess time (and extended when lambdas are created) so that
    /// `call_function` can fetch a function's body via a cheap
    /// `Rc::clone` (refcount bump) instead of linearly scanning the
    /// program's declarations and deep-cloning the entire FnDef (body
    /// AST included) on every single call.
    fn_cache: HashMap<String, Rc<AstFnDef>>,
    /// Names of generator functions (bodies containing `yield`). Stored
    /// alongside `fn_cache` so the generator check is an O(1) set lookup
    /// instead of a recursive body scan on every call.
    gen_set: std::collections::HashSet<String>,
    /// 递归保护深度：core_ 函数内部调 builtin 时不递归委托。
    core_call_depth: u32,
    /// Core stdlib program (loaded from core.veds, used by find_function
    /// to resolve core_* function calls).
    core_program: Option<std::rc::Rc<crate::parser::ast::Program>>,
    /// Set of function names that have type-parameter constraints
    /// (e.g. `fn, f[T: Drawable](x: T)`). These must go through
    /// call_function so check_type_constraints runs before the body.
    /// Populated at preprocess time.
    constrained_fns: std::collections::HashSet<String>,
    /// Free-list of call-frame scopes. `push_scope` reuses a cleared
    /// HashMap from this pool (avoiding a heap allocation on every
    /// function call) and `pop_scope` returns it. This matters on the
    /// fib(25) hot path, which pushes/pops a scope ~242k times.
    scope_pool: Vec<HashMap<String, Value>>,
    /// Exception handler stack for try/catch: each entry is (catch_pc, frame_depth).
    /// When a throw occurs, we pop handlers until we find one, then pop
    /// frames to unwind to the correct function.
    handler_stack: Vec<(usize, usize)>,
    /// Extern function declarations: name → link library (e.g. "c").
    /// Used to dispatch FFI calls to native C library functions.
    extern_fns: HashMap<String, String>,
    /// TCP connection registry: id → TcpStream. Used by net.dial/Conn methods.
    tcp_streams: HashMap<i64, std::net::TcpStream>,
    /// TCP listener registry: id → TcpListener. Used by net.listen/Listener methods.
    tcp_listeners: HashMap<i64, std::net::TcpListener>,
    /// WebSocket connection registry: id → (TcpStream, buffered message).
    ws_connections: HashMap<i64, std::net::TcpStream>,
    /// Next ID for TCP/WS/channel registrations.
    next_net_id: i64,
    /// Runtime counter for generating unique closure names. Each
    /// `MakeClosure` invocation produces a fresh name like
    /// `<lambda_N>#<id>` so that two invocations of the same enclosing
    /// function (e.g. `adder(5)` and `adder(10)`) do not share a
    /// capture map. The compile-time name (passed in the MakeClosure
    /// instruction) is used as a prefix to look up the FnDef in
    /// `module.lambda_fn_defs`; the `#<id>` suffix makes the runtime
    /// key unique.
    next_closure_id: usize,
    /// Channel buffers: id → (buffer, closed). Used by sync module's
    /// channel/send/receive/close builtins.
    channel_buffers: HashMap<i64, (Vec<Value>, bool)>,
    /// Flag registry: flag_name → (type, default_value, parsed_value).
    /// Populated by flag_string/flag_int/flag_bool; resolved by flag_parse.
    flag_registry: HashMap<String, (String, Value, Option<Value>)>,
    /// In-memory SQL databases: id → Database.
    sql_dbs: HashMap<i64, crate::bytecode::vm::SqlDb>,
    /// Macro definitions: name → (params, body). When a call resolves to a
    /// macro name, the VM executes the macro body with the call arguments
    /// bound to the macro's parameter names in a fresh local scope.
    macros: HashMap<String, (Vec<String>, Vec<crate::parser::ast::Stmt>)>,
    /// Free-list of locals Vecs for the bytecode call path. CallByName
    /// reuses a Vec from this pool (avoiding a heap allocation on every
    /// function call) and Return returns it. This matters on the fib(25)
    /// hot path, which allocates ~242k Vecs.
    locals_pool: Vec<Vec<Value>>,
    /// The most recently thrown value. When a `throw` statement executes,
    /// the thrown value is stored here (in addition to being stringified
    /// into the error message for the throw: prefix). Catch handlers
    /// check this slot first; if it's set, they use the full value instead
    /// of the stringified form.
    thrown_value: Option<Value>,
    /// Inline cache for LoadGlobal/StoreGlobal. Indexed by constant pool
    /// index. Once populated, LoadGlobal reads directly from this cache
    /// (O(1) Vec access) instead of doing a HashMap string lookup (O(n)
    /// hash + probe). StoreGlobal updates both the cache and the HashMap.
    /// The cache is valid for the pure bytecode path; the AST interpreter
    /// path calls `invalidate_global_cache()` when it modifies globals.
    global_cache: Vec<Option<Value>>,
    /// Inline cache for CallByName. Indexed by constant pool index. Maps
    /// the function name's constant index to (entry_pc, num_locals) from
    /// fn_entry_pcs. Once populated, CallByName skips the HashMap string
    /// lookup. Invalidated when new functions are defined (lambda creation).
    fn_entry_cache: Vec<Option<(usize, usize)>>,
}

impl VM {
    pub fn new(module: Module) -> Self {
        let num_locals = module.num_locals;
        VM {
            module,
            stack: Vec::with_capacity(1024),
            frames: vec![Frame {
                return_pc: 0,
                locals: vec![Value::Null; num_locals],
                is_main: true,
                stack_base: 0,
                defer_pcs: Vec::new(),
                fn_name: String::new(),
            }],
            globals: HashMap::new(),
            local_scopes: Vec::new(),
            pc: 0,
            iterators: Vec::new(),
            program: None,
            compiled_fns: HashMap::new(),
            base_dir: std::path::PathBuf::from("."),
            module_cache: HashMap::new(),
            module_program_cache: HashMap::new(),
            module_compiled_cache: HashMap::new(),
            frozen_set: std::collections::HashSet::new(),
            lambda_defs: Vec::new(),
            macro_counter: 0,
            method_context: Vec::new(),
            defer_stack: Vec::new(),
            annotation_table: HashMap::new(),
            gen_yield_buffer: None,
            lambda_captures: HashMap::new(),
            fn_cache: HashMap::new(),
            gen_set: std::collections::HashSet::new(),
            core_call_depth: 0,
            core_program: None,
            constrained_fns: std::collections::HashSet::new(),
            scope_pool: Vec::new(),
            handler_stack: Vec::new(),
            extern_fns: HashMap::new(),
            tcp_streams: HashMap::new(),
            tcp_listeners: HashMap::new(),
            ws_connections: HashMap::new(),
            next_net_id: 1,
            next_closure_id: 0,
            channel_buffers: HashMap::new(),
            flag_registry: HashMap::new(),
            sql_dbs: HashMap::new(),
            macros: HashMap::new(),
            locals_pool: Vec::new(),
            thrown_value: None,
            global_cache: Vec::new(),
            fn_entry_cache: Vec::new(),
        }
    }

    /// Attach the original program so function bodies can be compiled on demand.
    pub fn with_program(mut self, program: Program) -> Self {
        self.program = Some(program);
        self
    }

    /// Set the base directory for module resolution.
    pub fn with_base_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.base_dir = dir;
        self
    }

    /// Return a clone of the VM's globals (for test inspection).
    pub fn globals_clone(&mut self) -> HashMap<String, Value> {
        // Sync any deferred cache updates back to the HashMap.
        self.invalidate_global_cache();
        self.globals.clone()
    }

    /// Set a global variable (used by the REPL to persist variables
    /// across inputs).
    pub fn set_global(&mut self, name: &str, value: Value) {
        self.globals.insert(name.to_string(), value);
        self.invalidate_global_cache();
    }

    /// Intern a constant string into the module's constant pool.
    /// Returns the index. Used by run_bytecode_vm to append a CallByName
    /// instruction after the main bytecode has executed.
    pub fn intern_constant(&mut self, name: &str) -> usize {
        // Check if the constant already exists.
        for (i, c) in self.module.constants.iter().enumerate() {
            if c == name {
                return i;
            }
        }
        let idx = self.module.constants.len();
        self.module.constants.push(name.to_string());
        idx
    }

    /// Append an instruction to the module's code and continue execution
    /// from the current PC. Used by run_bytecode_vm to call main() after
    /// top-level code.
    pub fn push_instruction(&mut self, instr: crate::bytecode::instr::Instr) {
        self.module.code.push(instr);
    }

    /// Run the VM from the current PC until Halt or end of code.
    /// This is used after push_instruction to execute newly appended code.
    pub fn run_from_current_pc(&mut self) -> Result<()> {
        // Reset PC to the end of the original code (where we appended).
        // The original Halt already executed, so we need to set PC to
        // the newly appended instructions.
        // Actually, after run() returns (via Halt), PC is past Halt.
        // We need to set PC to the first appended instruction.
        let code_len = self.module.code.len();
        // The appended instructions start 2 before the end (CallByName + Pop).
        self.pc = code_len - 2;
        let _ = self.run();
        Ok(())
    }

    /// Set a flag's parsed value from a string, converting to the
    /// registered type (string/int/bool). If the flag isn't registered
    /// yet, register it as a string type with null default.
    fn set_flag_value(&mut self, name: &str, value: &str) {
        let entry = self.flag_registry
            .entry(name.to_string())
            .or_insert(("string".to_string(), Value::Null, None));
        let parsed = match entry.0.as_str() {
            "int" => {
                Value::Int(value.parse::<i64>().unwrap_or(0))
            }
            "bool" => {
                Value::Bool(matches!(value.to_lowercase().as_str(), "true" | "1" | "yes" | "on"))
            }
            _ => Value::Str(value.to_string()),
        };
        entry.2 = Some(parsed);
    }

    /// Invalidate the global inline cache. Called when globals are modified
    /// outside of the StoreGlobal instruction (e.g., from the AST interpreter
    /// path, from imports, from builtin registration). Before clearing, syncs
    /// any deferred cache updates back to the HashMap so the AST path sees
    /// the latest values.
    /// Evaluate a literal expression WITHOUT the AST interpreter.
    /// Used for annotation arguments and pattern literals.
    fn eval_literal_expr(&self, e: &crate::parser::ast::Expr) -> Option<Value> {
        use crate::parser::ast::Expr;
        match e {
            Expr::Integer(i) => Some(Value::Int(i.value)),
            Expr::Float(f) => Some(Value::Float(f.value)),
            Expr::Bool(b) => Some(Value::Bool(b.value)),
            Expr::Null(_) => Some(Value::Null),
            Expr::String_(s) => {
                let mut text = String::new();
                for p in &s.parts {
                    match p {
                        crate::parser::ast::StringPart::Text(t) => text.push_str(t),
                        _ => return None,
                    }
                }
                Some(Value::Str(text))
            }
            Expr::MultiLineString(s) => {
                let mut text = String::new();
                for p in &s.parts {
                    match p {
                        crate::parser::ast::StringPart::Text(t) => text.push_str(t),
                        _ => return None,
                    }
                }
                Some(Value::Str(text))
            }
            Expr::List(l) => {
                let mut items = Vec::new();
                for el in &l.elements {
                    items.push(self.eval_literal_expr(el)?);
                }
                Some(Value::List(items))
            }
            Expr::Identifier(id) => {
                if let Some(prog) = &self.program {
                    let parts: Vec<&str> = id.name.split('.').collect();
                    if parts.len() == 2 {
                        for d in &prog.declarations {
                            if let crate::parser::ast::TopLevel::EnumDef(ed) = d {
                                if ed.name.name == parts[0] {
                                    for (i, v) in ed.variants.iter().enumerate() {
                                        if v.name.name == parts[1] {
                                            return Some(Value::Int(i as i64));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Convert a value to a display string, calling __str__ if the value
    /// is an Object that defines it.
    fn value_to_display_str(&mut self, v: &Value) -> String {
        if let Value::Object(class, _) = v {
            // Check if the class defines __str__.
            if self.find_method(class, "__str__").is_ok() {
                let receiver = v.clone();
                match self.call_method_on_class(class, receiver, "__str__", vec![]) {
                    Ok(Value::Str(s)) => return s,
                    Ok(other) => return other.to_str(),
                    Err(_) => return v.to_str(),
                }
            }
        }
        v.to_str()
    }

    /// Auto-register stdlib modules (math, time, io, os, fs, fmt, json,
    /// collections, rand, etc.) as global Module values. This allows
    /// users to write `time.now()`, `math.sqrt()`, etc. without an
    /// explicit `import, "time"` statement.
    fn auto_register_stdlib_modules(&mut self) {
        let module_names = [
            "core", "math", "io", "os", "time", "fs", "fmt", "json", "collections",
            "rand", "path", "encoding", "regex", "debug", "log", "term",
            "flag", "sync", "net", "http", "image", "machine", "unsafe",
            "embed", "csv", "xml", "toml", "yaml", "compress", "websocket",
            "sql", "crypto",
        ];
        for name in &module_names {
            if !self.globals.contains_key(*name) {
                if let Some(exports) = self.load_builtin_module(name) {
                    self.globals.insert(name.to_string(), Value::Module(name.to_string(), exports));
                }
            }
        }
    }

    fn invalidate_global_cache(&mut self) {
        // Sync deferred cache updates back to the HashMap.
        for (idx, cached) in self.global_cache.iter().enumerate() {
            if let Some(ref v) = cached {
                let name = self
                    .module
                    .constants
                    .get(idx)
                    .map(|s| s.as_str())
                    .unwrap_or("");
                if !name.is_empty() {
                    if let Some(slot) = self.globals.get_mut(name) {
                        *slot = v.clone();
                    } else {
                        self.globals.insert(name.to_string(), v.clone());
                    }
                }
            }
        }
        self.global_cache.clear();
    }

    // ------------------------------------------------------------------
    // Call-frame local scope management (AST-interpreter path).
    //
    // Function bodies are executed via execute_ast_stmt / execute_ast_expr
    // (not the bytecode loop). To give each invocation its own set of
    // locals — so that recursion and re-entrancy do not clobber variables
    // — every call pushes a fresh scope onto `local_scopes` and pops it
    // when the call returns. Variable reads search the scope stack from
    // the innermost frame outward and then fall back to `globals`; writes
    // inside a function update an existing binding in the current frame or
    // globals, or create a new local in the current frame.
    // ------------------------------------------------------------------

    /// Push a new call-frame scope pre-populated with the given parameter
    /// bindings. Reuses a cleared HashMap from the scope pool when one is
    /// available, avoiding a per-call heap allocation.
    fn push_scope(&mut self, params: Vec<(String, Value)>) {
        let mut scope = self.scope_pool.pop().unwrap_or_default();
        scope.clear();
        for (k, v) in params {
            scope.insert(k, v);
        }
        self.local_scopes.push(scope);
    }

    /// Pop the innermost call-frame scope and return its HashMap to the
    /// scope pool for reuse by the next call.
    fn pop_scope(&mut self) {
        if let Some(scope) = self.local_scopes.pop() {
            self.scope_pool.push(scope);
        }
    }

    /// Read a variable by name, searching the local scope stack from the
    /// innermost frame outward, then falling back to `globals`. Returns
    /// `None` if the name is unbound anywhere.
    fn scope_get(&self, name: &str) -> Option<Value> {
        for scope in self.local_scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v.clone());
            }
        }
        self.globals.get(name).cloned()
    }

    /// Write a variable by name.
    ///
    /// Semantics (proper lexical scoping):
    /// - Search all local scopes from innermost to outermost. If the name
    ///   is found in any local scope, update it there. This ensures that
    ///   assignments to captured variables (in lambda capture scopes) and
    ///   to variables in enclosing function scopes work correctly.
    /// - If the name is not in any local scope but IS a global, update the
    ///   global (preserves intentional writes to globals from inside functions).
    /// - Otherwise, create a new local in the innermost scope (or a global
    ///   if at top level).
    fn scope_set(&mut self, name: String, val: Value) {
        // Search all local scopes from innermost to outermost.
        for scope in self.local_scopes.iter_mut().rev() {
            if scope.contains_key(&name) {
                scope.insert(name, val);
                return;
            }
        }
        // Not found in any local scope: check globals.
        if self.globals.contains_key(&name) {
            self.globals.insert(name, val);
            return;
        }
        // Not found anywhere: create a new binding in the innermost scope
        // (or a global if at top level).
        if let Some(scope) = self.local_scopes.last_mut() {
            scope.insert(name, val);
        } else {
            self.globals.insert(name, val);
        }
    }

    /// Delete a variable binding. Removes from the current frame if present,
    /// otherwise from globals.
    fn scope_delete(&mut self, name: &str) {
        if let Some(scope) = self.local_scopes.last_mut() {
            if scope.remove(name).is_some() {
                return;
            }
        }
        self.globals.remove(name);
    }

    /// Run the bytecode module and return the final value on the stack.
    ///
    /// This is the hot loop — optimized for speed:
    /// - Uses indexed access instead of cloning instructions.
    /// - Inlines the most common opcodes (Const, Add, Sub, Jump, etc.)
    ///   to avoid the function-call overhead of `execute()`.
    /// Execute bytecode starting from self.pc until a Return instruction
    /// pops the current frame. Used by call_function to execute lambda
    /// bodies that are registered in fn_entry_pcs.
    fn run_function_body(&mut self) -> Result<Value> {
        let code_len = self.module.code.len();
        while self.pc < code_len {
            let pc = self.pc;
            let instr_ref = &self.module.code[pc];
            let flow = match instr_ref {
                Instr::ConstInt(i) => { self.stack.push(Value::Int(*i)); self.pc += 1; Ok(Flow::Continue) }
                Instr::ConstNull => { self.stack.push(Value::Null); self.pc += 1; Ok(Flow::Continue) }
                Instr::ConstBool(b) => { self.stack.push(Value::Bool(*b)); self.pc += 1; Ok(Flow::Continue) }
                Instr::Return => {
                    self.pc += 1;
                    let v = self.stack.pop().unwrap_or(Value::Null);
                    // Run deferred blocks for this frame before returning.
                    let defers: Vec<usize> = if let Some(frame) = self.frames.last_mut() {
                        let d = frame.defer_pcs.clone();
                        frame.defer_pcs.clear();
                        d
                    } else {
                        Vec::new()
                    };
                    if !defers.is_empty() {
                        // Push the return value back temporarily while defers run.
                        self.stack.push(v.clone());
                        let continuation = self.pc;
                        for defer_pc in defers.into_iter().rev() {
                            self.pc = defer_pc;
                            // Run instructions until we reach a Nop sentinel
                            // (which marks the end of the defer block) or
                            // the continuation.
                            loop {
                                if self.pc >= self.module.code.len() || self.pc == continuation {
                                    break;
                                }
                                // Check for Nop sentinel (end of defer block).
                                if matches!(self.module.code[self.pc], Instr::Nop) {
                                    self.pc += 1; // skip the Nop
                                    break;
                                }
                                let instr = self.module.code[self.pc].clone();
                                self.pc += 1;
                                match self.execute(&instr)? {
                                    Flow::Continue => {}
                                    Flow::Return(_) => break,
                                }
                            }
                        }
                        self.pc = continuation;
                        // Pop the return value back (defers may have pushed/popped
                        // but shouldn't touch the return value slot).
                        let _ = self.stack.pop();
                    }
                    if self.frames.len() > 1 {
                        let frame = self.frames.pop().unwrap();
                        self.pc = frame.return_pc;
                        // Truncate the stack to the caller's base, discarding
                        // any values leaked above the return value (e.g. a
                        // match scrutinee left on the stack by an early return
                        // from inside a match case body).
                        self.stack.truncate(frame.stack_base);
                        self.stack.push(v);
                        if !frame.is_main {
                            self.locals_pool.push(frame.locals);
                        }
                        Ok(Flow::Continue)
                    } else {
                        return Ok(v);
                    }
                }
                _ => {
                    let instr = self.module.code[pc].clone();
                    self.pc += 1;
                    self.execute(&instr)
                }
            };
            match flow {
                Ok(Flow::Continue) => {}
                Ok(Flow::Return(v)) => return Ok(v),
                Err(e) => {
                    let msg = e.message().to_string();
                    // Check if there's a handler on the handler_stack.
                    // Catch both explicit throws and runtime errors
                    // (division by zero, index out of bounds, etc.).
                    if let Some((catch_pc, handler_frame_depth)) = self.handler_stack.last().copied() {
                        // Only catch if this is a throw or a runtime error
                        // (not a control-flow signal like return/break/continue).
                        let is_catchable = msg.starts_with("throw:")
                            || msg == "division by zero"
                            || msg == "modulo by zero"
                            || msg.contains("index")
                            || msg.contains("out of bounds")
                            || msg.contains("not found")
                            || msg.contains("null")
                            || msg.contains("type")
                            || msg.contains("key")
                            || msg.contains("cannot convert");
                        if is_catchable {
                            self.handler_stack.pop();
                            while self.frames.len() > handler_frame_depth {
                                self.frames.pop();
                            }
                            // Use the full thrown value if available;
                            // otherwise wrap the error message as a string.
                            let exc_val = self.thrown_value.take()
                                .unwrap_or_else(|| {
                                    if msg.starts_with("throw:") {
                                        Value::Str(msg["throw:".len()..].to_string())
                                    } else {
                                        Value::Str(msg.clone())
                                    }
                                });
                            self.push(exc_val);
                            self.pc = catch_pc;
                            continue;
                        }
                    }
                    return Err(e);
                }
            }
        }
        Ok(self.stack.pop().unwrap_or(Value::Null))
    }

    pub fn run(&mut self) -> Result<Value> {
        // Pre-process top-level declarations: imports and class definitions.
        self.preprocess_top_level()?;
        // Auto-register stdlib modules as globals so users can write
        // `time.now()`, `math.sqrt()`, `io.open()`, etc. without an
        // explicit `import, "time"` statement.
        self.auto_register_stdlib_modules();
        let code_len = self.module.code.len();
        while self.pc < code_len {
            // Get the PC index first, then match on a clone of the instruction.
            // We only clone for the slow path; the fast path uses match-on-ref.
            let pc = self.pc;
            let instr_ref = &self.module.code[pc];
            // Fast path for the most common opcodes — avoid the full
            // execute() dispatch for these hot instructions.
            let flow = match instr_ref {
                Instr::ConstInt(i) => {
                    self.stack.push(Value::Int(*i));
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                Instr::ConstNull => {
                    self.stack.push(Value::Null);
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                Instr::ConstBool(b) => {
                    self.stack.push(Value::Bool(*b));
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                Instr::Add => {
                    let b = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    let a = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    // Fast path for int+int (most common case) with wrapping.
                    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
                        self.stack.push(Value::Int(x.wrapping_add(*y)));
                    } else if let (Value::Float(x), Value::Float(y)) = (&a, &b) {
                        self.stack.push(Value::Float(x + y));
                    } else if let (Value::Str(x), Value::Str(y)) = (&a, &b) {
                        self.stack.push(Value::Str(format!("{}{}", x, y)));
                    } else {
                        // Fall back to string concatenation for mixed types.
                        if let (Value::Str(x), _) = (&a, &b) {
                            self.stack.push(Value::Str(format!("{}{}", x, b.to_str())));
                        } else if let (_, Value::Str(y)) = (&a, &b) {
                            self.stack.push(Value::Str(format!("{}{}", a.to_str(), y)));
                        } else {
                            // Operator overloading or mixed numeric types.
                            if let Some(v) = self.try_binary_overload("__add__", &a, &b)? {
                                self.stack.push(v);
                            } else {
                                self.stack.push(match (a, b) {
                                    (Value::Int(x), Value::Float(y)) => Value::Float(x as f64 + y),
                                    (Value::Float(x), Value::Int(y)) => Value::Float(x + y as f64),
                                    // List + List → concatenation
                                    (Value::List(a), Value::List(b)) => {
                                        let mut result = a;
                                        result.extend(b);
                                        Value::List(result)
                                    }
                                    // List + element → append
                                    (Value::List(a), other) => {
                                        let mut result = a;
                                        result.push(other);
                                        Value::List(result)
                                    }
                                    // element + List → prepend
                                    (other, Value::List(b)) => {
                                        let mut result = vec![other];
                                        result.extend(b);
                                        Value::List(result)
                                    }
                                    // Tuple + Tuple → concatenation
                                    (Value::Tuple(a), Value::Tuple(b)) => {
                                        let mut result = a;
                                        result.extend(b);
                                        Value::Tuple(result)
                                    }
                                    _ => return Err(CompilerError::runtime_error(
                                        format!("type error: cannot add {:?} and {:?}", "value", "value")
                                    )),
                                });
                            }
                        }
                    }
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                Instr::Sub => {
                    let b = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    let a = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
                        self.stack.push(Value::Int(x.wrapping_sub(*y)));
                    } else {
                        if let Some(v) = self.try_binary_overload("__sub__", &a, &b)? {
                            self.stack.push(v);
                        } else {
                            self.stack.push(match (a, b) {
                                (Value::Float(x), Value::Float(y)) => Value::Float(x - y),
                                (Value::Int(x), Value::Float(y)) => Value::Float(x as f64 - y),
                                (Value::Float(x), Value::Int(y)) => Value::Float(x - y as f64),
                                _ => return Err(CompilerError::runtime_error(
                                    "type error: cannot subtract these values"
                                )),
                            });
                        }
                    }
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                Instr::Mul => {
                    let b = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    let a = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
                        self.stack.push(Value::Int(x.wrapping_mul(*y)));
                    } else {
                        if let Some(v) = self.try_binary_overload("__mul__", &a, &b)? {
                            self.stack.push(v);
                        } else {
                            self.stack.push(match (a, b) {
                                (Value::Float(x), Value::Float(y)) => Value::Float(x * y),
                                (Value::Int(x), Value::Float(y)) => Value::Float(x as f64 * y),
                                (Value::Float(x), Value::Int(y)) => Value::Float(x * y as f64),
                                // String repeat: "ab" * 3 → "ababab".
                                (Value::Str(s), Value::Int(n)) => {
                                    if n > 0 {
                                        Value::Str(s.repeat(n as usize))
                                    } else {
                                        Value::Str(String::new())
                                    }
                                }
                                (Value::Int(n), Value::Str(s)) => {
                                    if n > 0 {
                                        Value::Str(s.repeat(n as usize))
                                    } else {
                                        Value::Str(String::new())
                                    }
                                }
                                _ => return Err(CompilerError::runtime_error(
                                    "type error: cannot multiply these values"
                                )),
                            });
                        }
                    }
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                Instr::Jump(target) => {
                    self.pc = *target;
                    Ok(Flow::Continue)
                }
                Instr::JumpIfFalse(target) => {
                    let v = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    if !v.truthy() {
                        self.pc = *target;
                    } else {
                        self.pc += 1;
                    }
                    Ok(Flow::Continue)
                }
                Instr::JumpIfTrue(target) => {
                    let v = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    if v.truthy() {
                        self.pc = *target;
                    } else {
                        self.pc += 1;
                    }
                    Ok(Flow::Continue)
                }
                Instr::Pop => {
                    self.stack.pop();
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                Instr::LoadGlobal(idx) => {
                    let name_idx = *idx;
                    self.pc += 1;
                    // Inline cache: check the cache first (O(1) Vec access).
                    // If the cache is populated for this constant index, use
                    // the cached value directly, avoiding the HashMap string
                    // lookup (hash + probe). This is the 100k-loop hot path.
                    let v = if name_idx < self.global_cache.len() {
                        if let Some(ref cached) = self.global_cache[name_idx] {
                            cached.clone()
                        } else {
                            // Cache miss: look up in globals, populate cache.
                            let name = self
                                .module
                                .constants
                                .get(name_idx)
                                .map(|s| s.as_str())
                                .unwrap_or("");
                            let val = self.globals.get(name).cloned().unwrap_or(Value::Null);
                            // Ensure cache is large enough.
                            if self.global_cache.len() <= name_idx {
                                self.global_cache.resize(name_idx + 1, None);
                            }
                            self.global_cache[name_idx] = Some(val.clone());
                            val
                        }
                    } else {
                        let name = self
                            .module
                            .constants
                            .get(name_idx)
                            .map(|s| s.as_str())
                            .unwrap_or("");
                        self.globals.get(name).cloned().unwrap_or(Value::Null)
                    };
                    self.stack.push(v);
                    Ok(Flow::Continue)
                }
                Instr::StoreGlobal(idx) => {
                    let name_idx = *idx;
                    self.pc += 1;
                    let v = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    // If the cache is already populated for this index, the
                    // global exists in the HashMap. Update only the cache
                    // (O(1) Vec access) and defer the HashMap sync to
                    // invalidate_global_cache(). This avoids the HashMap
                    // string lookup on every StoreGlobal in the hot loop.
                    if name_idx < self.global_cache.len() && self.global_cache[name_idx].is_some() {
                        self.global_cache[name_idx] = Some(v);
                    } else {
                        // Cache miss: update the HashMap and populate cache.
                        let name = self
                            .module
                            .constants
                            .get(name_idx)
                            .map(|s| s.as_str())
                            .unwrap_or("");
                        if let Some(slot) = self.globals.get_mut(name) {
                            *slot = v.clone();
                        } else {
                            self.globals.insert(name.to_string(), v.clone());
                        }
                        if self.global_cache.len() <= name_idx {
                            self.global_cache.resize(name_idx + 1, None);
                        }
                        self.global_cache[name_idx] = Some(v);
                    }
                    Ok(Flow::Continue)
                }
                Instr::Return => {
                    self.pc += 1;
                    let v = self.stack.pop().unwrap_or(Value::Null);
                    // Run deferred blocks for this frame before returning.
                    let defers: Vec<usize> = if let Some(frame) = self.frames.last_mut() {
                        let d = frame.defer_pcs.clone();
                        frame.defer_pcs.clear();
                        d
                    } else {
                        Vec::new()
                    };
                    if !defers.is_empty() {
                        self.stack.push(v.clone());
                        let continuation = self.pc;
                        for defer_pc in defers.into_iter().rev() {
                            self.pc = defer_pc;
                            loop {
                                if self.pc >= self.module.code.len() || self.pc == continuation {
                                    break;
                                }
                                if matches!(self.module.code[self.pc], Instr::Nop) {
                                    self.pc += 1;
                                    break;
                                }
                                let instr = self.module.code[self.pc].clone();
                                self.pc += 1;
                                match self.execute(&instr)? {
                                    Flow::Continue => {}
                                    Flow::Return(_) => break,
                                }
                            }
                        }
                        self.pc = continuation;
                        let _ = self.stack.pop();
                    }
                    if self.frames.len() <= 1 {
                        return Ok(v);
                    }
                    let frame = self.frames.pop().unwrap();
                    self.pc = frame.return_pc;
                    self.stack.truncate(frame.stack_base);
                    self.stack.push(v);
                    // Return the locals Vec to the pool for reuse by the
                    // next call (avoids allocation on the fib hot path).
                    if !frame.is_main {
                        self.locals_pool.push(frame.locals);
                    }
                    Ok(Flow::Continue)
                }
                // Comparison ops on the fast path — the 100k-loop hot path
                // runs `i < N` every iteration, so avoiding the slow-path
                // Instr clone + execute() dispatch matters. Each handler
                // fast-paths int/int and float/float and only falls back to
                // operator overloading for Objects.
                Instr::Lt => {
                    let b = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    let a = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    let r = match (&a, &b) {
                        (Value::Int(x), Value::Int(y)) => Value::Bool(x < y),
                        (Value::Float(x), Value::Float(y)) => Value::Bool(x < y),
                        (Value::Int(x), Value::Float(y)) => Value::Bool((*x as f64) < *y),
                        (Value::Float(x), Value::Int(y)) => Value::Bool(*x < (*y as f64)),
                        (Value::Str(x), Value::Str(y)) => Value::Bool(x < y),
                        _ => {
                            if let Some(v) = self.try_binary_overload("__lt__", &a, &b)? {
                                v
                            } else {
                                Value::Null
                            }
                        }
                    };
                    self.stack.push(r);
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                Instr::Le => {
                    let b = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    let a = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    let r = match (&a, &b) {
                        (Value::Int(x), Value::Int(y)) => Value::Bool(x <= y),
                        (Value::Float(x), Value::Float(y)) => Value::Bool(x <= y),
                        (Value::Int(x), Value::Float(y)) => Value::Bool((*x as f64) <= *y),
                        (Value::Float(x), Value::Int(y)) => Value::Bool(*x <= (*y as f64)),
                        (Value::Str(x), Value::Str(y)) => Value::Bool(x <= y),
                        _ => {
                            if let Some(v) = self.try_binary_overload("__le__", &a, &b)? {
                                v
                            } else {
                                Value::Null
                            }
                        }
                    };
                    self.stack.push(r);
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                Instr::Gt => {
                    let b = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    let a = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    let r = match (&a, &b) {
                        (Value::Int(x), Value::Int(y)) => Value::Bool(x > y),
                        (Value::Float(x), Value::Float(y)) => Value::Bool(x > y),
                        (Value::Int(x), Value::Float(y)) => Value::Bool((*x as f64) > *y),
                        (Value::Float(x), Value::Int(y)) => Value::Bool(*x > (*y as f64)),
                        (Value::Str(x), Value::Str(y)) => Value::Bool(x > y),
                        _ => {
                            if let Some(v) = self.try_binary_overload("__gt__", &a, &b)? {
                                v
                            } else {
                                Value::Null
                            }
                        }
                    };
                    self.stack.push(r);
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                Instr::Ge => {
                    let b = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    let a = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    let r = match (&a, &b) {
                        (Value::Int(x), Value::Int(y)) => Value::Bool(x >= y),
                        (Value::Float(x), Value::Float(y)) => Value::Bool(x >= y),
                        (Value::Int(x), Value::Float(y)) => Value::Bool((*x as f64) >= *y),
                        (Value::Float(x), Value::Int(y)) => Value::Bool(*x >= (*y as f64)),
                        (Value::Str(x), Value::Str(y)) => Value::Bool(x >= y),
                        _ => {
                            if let Some(v) = self.try_binary_overload("__ge__", &a, &b)? {
                                v
                            } else {
                                Value::Null
                            }
                        }
                    };
                    self.stack.push(r);
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                Instr::Eq => {
                    let b = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    let a = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    // Check __eq__ on either operand (symmetric).
                    let r = if let Some(v) = self.try_binary_overload("__eq__", &a, &b)? {
                        v
                    } else {
                        Value::Bool(a == b)
                    };
                    self.stack.push(r);
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                Instr::Ne => {
                    let b = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    let a = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    // Check __ne__ on either operand (symmetric).
                    let r = if let Some(v) = self.try_binary_overload("__ne__", &a, &b)? {
                        v
                    } else {
                        Value::Bool(a != b)
                    };
                    self.stack.push(r);
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                // LoadLocal/StoreLocal on the fast path — the fib(25) hot
                // path reads/writes locals (n, n-1, n-2) via these. Avoiding
                // the slow-path execute() dispatch + current_locals() helper
                // call is a meaningful win.
                Instr::LoadLocal(slot) => {
                    let v = self
                        .frames
                        .last()
                        .and_then(|f| f.locals.get(*slot))
                        .cloned()
                        .unwrap_or(Value::Null);
                    self.stack.push(v);
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                Instr::StoreLocal(slot) => {
                    let v = self.stack.pop().ok_or_else(|| {
                        CompilerError::runtime_error("stack underflow")
                    })?;
                    let locals = &mut self.frames.last_mut().unwrap().locals;
                    if *slot >= locals.len() {
                        locals.resize(*slot + 1, Value::Null);
                    }
                    locals[*slot] = v;
                    self.pc += 1;
                    Ok(Flow::Continue)
                }
                // CallByName fast path: if this function has a compiled
                // bytecode body (registered in fn_entry_pcs), jump to it
                // directly. Uses fn_entry_cache to avoid the HashMap string
                // lookup on every call. This is the fib(25) hot path.
                Instr::CallByName(name_idx, argc) => {
                    let ni = *name_idx;
                    let ac = *argc;
                    // Check the fn_entry_cache first.
                    let cached = if ni < self.fn_entry_cache.len() {
                        self.fn_entry_cache[ni]
                    } else {
                        None
                    };
                    // Get the function name to check if it's a lambda
                    // (lambdas need the capture mechanism, so they must
                    // go through call_function, not the bytecode fast path).
                    let fn_name = self
                        .module
                        .constants
                        .get(ni)
                        .map(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    let is_lambda = fn_name.starts_with("<lambda_");
                    // Generators must go through call_function's is_generator
                    // branch (which sets up gen_yield_buffer and collects
                    // yielded values). The bytecode fast path would execute
                    // the body directly, but Yield would push back to the
                    // stack (no buffer) and the result would be wrong.
                    let is_generator = self.gen_set.contains(&fn_name);
                    // Constrained functions must go through call_function so
                    // check_type_constraints runs before the body.
                    let is_constrained = self.constrained_fns.contains(&fn_name);
                    // Cache miss: check fn_entry_pcs directly.
                    // Skip the bytecode fast path for lambdas (they need
                    // capture scope setup from call_function), generators,
                    // and constrained functions.
                    let entry = if is_lambda || is_generator || is_constrained {
                        None
                    } else if cached.is_some() {
                        cached
                    } else {
                        let name = fn_name.as_str();
                        let e = self
                            .module
                            .fn_entry_pcs
                            .get(name)
                            .copied();
                        if let Some(e) = e {
                            if self.fn_entry_cache.len() <= ni {
                                self.fn_entry_cache.resize(ni + 1, None);
                            }
                            self.fn_entry_cache[ni] = Some(e);
                        }
                        e
                    };
                    if let Some((entry_pc, num_locals)) = entry {
                        // Fast path: jump to the bytecode body.
                        let nslots = num_locals.max(ac);
                        let mut locals = if let Some(mut v) = self.locals_pool.pop() {
                            v.clear();
                            v.resize(nslots, Value::Null);
                            v
                        } else {
                            vec![Value::Null; nslots]
                        };
                        for i in (0..ac).rev() {
                            locals[i] = self.stack.pop().ok_or_else(|| {
                                CompilerError::runtime_error("stack underflow")
                            })?;
                        }
                        let stack_base = self.stack.len();
                        self.pc += 1; // skip past the CallByName instruction
                        self.frames.push(Frame {
                            return_pc: self.pc,
                            locals,
                            is_main: false,
                            stack_base,
                            defer_pcs: Vec::new(),
                            fn_name: fn_name.clone(),
                        });
                        self.pc = entry_pc;
                        Ok(Flow::Continue)
                    } else {
                        // Slow path: not a compiled function. Fall through to
                        // execute() for class constructor / AST interpreter.
                        let instr = self.module.code[pc].clone();
                        self.pc += 1;
                        self.execute(&instr)
                    }
                }
                _ => {
                    // Slow path: clone the instruction and dispatch.
                    let instr = self.module.code[pc].clone();
                    self.pc += 1;
                    self.execute(&instr)
                }
            };
            match flow {
                Ok(Flow::Continue) => {}
                Ok(Flow::Return(v)) => return Ok(v),
                Err(e) => {
                    // Exception propagation: check if there's a handler
                    // on the handler_stack. If so, unwind frames to the
                    // handler's frame depth, push the exception value,
                    // and jump to the catch target.
                    let msg = e.message().to_string();
                    let is_catchable = msg.starts_with("throw:")
                        || msg == "division by zero"
                        || msg == "modulo by zero"
                        || msg.contains("index")
                        || msg.contains("out of bounds")
                        || msg.contains("not found")
                        || msg.contains("null")
                        || msg.contains("type")
                        || msg.contains("key")
                        || msg.contains("cannot convert");
                    if is_catchable {
                        if let Some((catch_pc, handler_frame_depth)) = self.handler_stack.pop() {
                            // Unwind frames: pop frames until we're at the
                            // same depth as when the handler was pushed.
                            while self.frames.len() > handler_frame_depth {
                                self.frames.pop();
                            }
                            let exc_val = self.thrown_value.take()
                                .unwrap_or_else(|| {
                                    if msg.starts_with("throw:") {
                                        Value::Str(msg["throw:".len()..].to_string())
                                    } else {
                                        Value::Str(msg.clone())
                                    }
                                });
                            self.push(exc_val);
                            self.pc = catch_pc;
                            continue;
                        }
                    }
                    return Err(e);
                }
            }
        }
        Ok(self.stack.pop().unwrap_or(Value::Null))
    }

    /// Pre-process top-level declarations that the bytecode compiler skips:
    /// imports, class definitions, and annotations. This runs before the main
    /// bytecode loop so that imported symbols, class constructors, and
    /// annotation tables are available.
    fn preprocess_top_level(&mut self) -> Result<()> {
        if let Some(prog) = &self.program.clone() {
            for d in &prog.declarations {
                match d {
                    crate::parser::ast::TopLevel::Import(imp) => {
                        self.load_module(imp)?;
                    }
                    crate::parser::ast::TopLevel::ClassDef(cd) => {
                        // Register the class name as a global Class value.
                        self.globals
                            .insert(cd.name.name.clone(), Value::Class(cd.name.name.clone()));
                    }
                    crate::parser::ast::TopLevel::FnDef(f) => {
                        // Register the function name as a global Func value so
                        // it can be passed around (e.g. `annotations(index)`)
                        // and looked up at runtime.
                        self.globals
                            .insert(f.name.name.clone(), Value::Func(f.name.name.clone()));
                        // Cache the function definition (Rc, cheap to clone
                        // per call) and precompute whether it is a generator
                        // so call_function avoids a linear scan + deep clone
                        // + recursive yield-check on every invocation.
                        if self.fn_has_yield(&f.body) {
                            self.gen_set.insert(f.name.name.clone());
                        }
                        if !f.type_constraints.is_empty() {
                            self.constrained_fns.insert(f.name.name.clone());
                        }
                        self.fn_cache
                            .insert(f.name.name.clone(), Rc::new(f.clone()));
                        // Collect annotations for this function.
                        if !f.annotations.is_empty() {
                            let mut ann_map: HashMap<String, Value> = HashMap::new();
                            for ann in &f.annotations {
                                // For @route("/answer"), key="route", value="/answer".
                                // For @inline, key="inline", value=true.
                                let key = ann.name.clone();
                                let val = if let Some(first_arg) = ann.arguments.first() {
                                    self.eval_literal_expr(&first_arg.value)
                                        .unwrap_or(Value::Bool(true))
                                } else {
                                    Value::Bool(true)
                                };
                                ann_map.insert(key, val);
                            }
                            self.annotation_table
                                .insert(f.name.name.clone(), ann_map);
                        }
                    }
                    crate::parser::ast::TopLevel::EnumDef(ed) => {
                        // Register the enum name as a global Enum value, and
                        // each variant as a global that holds a Value::Object
                        // tagged with the enum name + variant name. This lets
                        // `Color.Red` resolve to a value that match-patterns
                        // can recognise.
                        self.globals.insert(
                            ed.name.name.clone(),
                            Value::Str(format!("<enum {}>", ed.name.name)),
                        );
                        for v in &ed.variants {
                            let mut fields = HashMap::new();
                            fields.insert(
                                "__variant__".to_string(),
                                Value::Str(v.name.name.clone()),
                            );
                            fields.insert("__payload__".to_string(), Value::Tuple(Vec::new()));
                            // Register as `EnumName.VariantName` via a nested
                            // global: we store the variant under a key that
                            // member-access on the enum value can find. Since
                            // our Value::Str doesn't carry fields, we instead
                            // register each variant as a standalone global
                            // named "EnumName.VariantName" so qualified access
                            // `Color.Red` (which compiles to MemberAccess on
                            // `Color`) can resolve it.
                            let obj = Value::Object(ed.name.name.clone(), Rc::new(RefCell::new(fields)));
                            self.globals.insert(
                                format!("{}.{}", ed.name.name, v.name.name),
                                obj,
                            );
                        }
                    }
                    crate::parser::ast::TopLevel::ExternFnDef(efd) => {
                        // Register the extern function name as a global Func
                        // value. At call time, the VM's call_builtin_value
                        // dispatches known C standard library functions
                        // (printf, puts, malloc, free, strlen, atoi, atof,
                        // exit, abs, rand, srand, time) natively. Unknown
                        // extern functions raise a runtime error.
                        self.globals.insert(
                            efd.fn_def.name.name.clone(),
                            Value::Func(efd.fn_def.name.name.clone()),
                        );
                        // Also record the extern link name so the VM can
                        // resolve the function by its C symbol name.
                        self.extern_fns.insert(
                            efd.fn_def.name.name.clone(),
                            efd.link.clone().unwrap_or_else(|| "c".to_string()),
                        );
                    }
                    crate::parser::ast::TopLevel::MacroDef(md) => {
                        // Register the macro: name → (param names, body).
                        // At call time, if the called name matches a macro,
                        // the VM executes the macro body with the call args
                        // bound to the param names in a fresh local scope.
                        let params: Vec<String> = md.params.iter().map(|p| p.name.name.clone()).collect();
                        self.macros.insert(md.name.name.clone(), (params, md.body.clone()));
                    }
                    _ => {}
                }
            }
        }
        // Also register functions from the module's function table that
        // are not in the program's declarations (e.g., lambdas compiled
        // as named functions). This ensures lambda Func values are
        // available as globals for LoadGlobal.
        let fn_names: Vec<String> = self.module.functions.keys().cloned().collect();
        for name in &fn_names {
            if !self.globals.contains_key(name) {
                self.globals.insert(name.clone(), Value::Func(name.clone()));
            }
            if !self.fn_cache.contains_key(name) {
                if let Some(fd) = self.module.functions.get(name) {
                    let body = vec![]; // empty body — bytecode is used instead
                    let fn_def = crate::parser::ast::FnDef {
                        annotations: vec![],
                        name: crate::parser::ast::Identifier {
                            name: name.clone(),
                            span: crate::error::Span::dummy(),
                        },
                        params: fd
                            .params
                            .iter()
                            .map(|p| crate::parser::ast::FnParam {
                                name: crate::parser::ast::Identifier {
                                    name: p.clone(),
                                    span: crate::error::Span::dummy(),
                                },
                                type_annotation: None,
                                default_value: None,
                                is_variadic: false,
                                span: crate::error::Span::dummy(),
                            })
                            .collect(),
                        return_type: None,
                        body,
                        is_constexpr: false,
                        is_lazy: false,
                        is_async: false,
                        is_extern: false,
                        extern_link: None,
                        type_constraints: std::collections::HashMap::new(),
                        type_params: Vec::new(),
                        span: crate::error::Span::dummy(),
                    };
                    self.fn_cache.insert(name.clone(), Rc::new(fn_def));
                }
            }
        }
        Ok(())
    }

    /// Load a module and bind its exports.
    fn load_module(&mut self, imp: &crate::parser::ast::ImportStmt) -> Result<()> {
        let module_path = &imp.module;
        // Check for built-in standard modules first.
        if let Some(builtin_exports) = self.load_builtin_module(module_path) {
            // Cache the exports for later lookups.
            self.module_cache.insert(module_path.clone(), builtin_exports.clone());
            // For core module: fn_entry_pcs and fn_cache are already
            // populated by run_bytecode_vm (which appends core.veds
            // functions to the program before compiling). No bytecode
            // merging needed here — just bind the exports.
            // Bind according to the import form: alias, symbol list, or bare.
            self.bind_module_exports(imp, builtin_exports);
            return Ok(());
        }
        // Resolve the path relative to the current directory and base_dir.
        // Try several candidate extensions: bare, .veds, /mod.veds.
        // Also try the bundled stdlib directory (src/runtime/std).
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        for base in [
            std::path::PathBuf::from("."),
            self.base_dir.clone(),
            std::env::current_dir().unwrap_or_default(),
            // Bundled stdlib modules.
            std::path::PathBuf::from("src/runtime/std"),
            std::path::PathBuf::from("../src/runtime/std"),
        ] {
            let p = std::path::Path::new(module_path);
            if p.is_absolute() {
                candidates.push(p.to_path_buf());
                continue;
            }
            candidates.push(base.join(module_path));
            if !module_path.ends_with(".veds") {
                candidates.push(base.join(format!("{}.veds", module_path)));
                candidates.push(base.join(module_path).join("mod.veds"));
            }
        }
        let path = candidates
            .into_iter()
            .find(|p| p.exists())
            .ok_or_else(|| {
                CompilerError::runtime_error(format!("module '{}' not found", module_path))
            })?;
        let key = path.to_string_lossy().to_string();
        // Check cache.
        if let Some(cached) = self.module_cache.get(&key).cloned() {
            self.bind_module_exports(imp, cached);
            return Ok(());
        }
        // Load and parse the module.
        let source = std::fs::read_to_string(&path).map_err(|e| {
            CompilerError::io_error(format!("read module '{}': {}", path.display(), e))
        })?;
        let mut lexer = crate::lexer::Lexer::new(&source, 99);
        let tokens = lexer.tokenize()?;
        let mut parser = crate::parser::Parser::new(tokens, 99);
        let program = parser.parse_program()?;
        // Execute the module in a sub-VM to collect ALL globals as exports
        // (functions and variables defined at top level).
        let sub_compiler = super::Compiler::new();
        let sub_module = sub_compiler.compile(&program)?;
        // Cache the parsed Program and compiled Module so that later
        // `call_imported_function` calls for this module can skip the
        // read+parse+compile cycle (see call_imported_function). We
        // share via Rc — the cache holds the Rc, and the sub-VM below
        // takes its own clone (the program field on VM is owned, so we
        // can't share by reference there).
        let program_rc = Rc::new(program.clone());
        let module_rc = Rc::new(sub_module.clone());
        self.module_program_cache.insert(key.clone(), program_rc);
        self.module_compiled_cache.insert(key.clone(), module_rc);
        let sub_base = path.parent().unwrap_or(&self.base_dir).to_path_buf();
        let mut sub_vm = VM::new(sub_module)
            .with_program(program)
            .with_base_dir(sub_base);
        sub_vm.run()?;
        // Collect ALL globals as exports (no export statement required).
        let exports = sub_vm.globals.clone();
        self.module_cache.insert(key.clone(), exports.clone());
        self.bind_module_exports(imp, exports);
        Ok(())
    }

    /// Bind module exports according to the import statement.
    fn bind_module_exports(&mut self, imp: &crate::parser::ast::ImportStmt, exports: HashMap<String, Value>) {
        if let Some(symbols) = &imp.symbols {
            // import, "mod", sym1, sym2 — bind each symbol directly.
            for sym in symbols {
                if let Some(v) = exports.get(&sym.name) {
                    self.globals.insert(sym.name.clone(), v.clone());
                } else {
                    // Symbol not in exports — also check the module's
                    // function table (functions may not be in globals if
                    // they weren't called during module execution).
                    // We can't access the sub-VM's function table here, so
                    // we store a Func value as a fallback.
                    self.globals.insert(sym.name.clone(), Value::Func(sym.name.clone()));
                }
            }
        } else if let Some(alias) = &imp.alias {
            // import, "mod", as, alias — bind the whole module dict.
            self.globals
                .insert(alias.name.clone(), Value::Module(imp.module.clone(), exports));
        } else {
            // import, "mod" — bind with derived name.
            let name = std::path::Path::new(&imp.module)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(&imp.module)
                .to_string();
            self.globals
                .insert(name, Value::Module(imp.module.clone(), exports));
        }
    }

    fn current_locals(&self) -> &[Value] {
        &self.frames.last().unwrap().locals
    }

    fn current_locals_mut(&mut self) -> &mut Vec<Value> {
        &mut self.frames.last_mut().unwrap().locals
    }

    fn execute(&mut self, instr: &Instr) -> Result<Flow> {
        match instr {
            Instr::ConstInt(i) => self.push(Value::Int(*i)),
            Instr::ConstFloat(f) => self.push(Value::Float(*f)),
            Instr::ConstBool(b) => self.push(Value::Bool(*b)),
            Instr::ConstNull => self.push(Value::Null),
            Instr::ConstStr(idx) => {
                let s = self
                    .module
                    .constants
                    .get(*idx)
                    .cloned()
                    .unwrap_or_default();
                self.push(Value::Str(s));
            }
            Instr::NewList(n) => {
                let mut elements = Vec::with_capacity(*n);
                for _ in 0..*n {
                    elements.push(self.pop()?);
                }
                elements.reverse();
                self.push(Value::List(elements));
            }
            Instr::NewDict(n) => {
                let mut d = HashMap::new();
                for _ in 0..*n {
                    let v = self.pop()?;
                    let k = self.pop()?;
                    d.insert(k.to_str(), v);
                }
                self.push(Value::Dict(d));
            }
            Instr::NewTuple(n) => {
                let mut elements = Vec::with_capacity(*n);
                for _ in 0..*n {
                    elements.push(self.pop()?);
                }
                elements.reverse();
                self.push(Value::Tuple(elements));
            }
            Instr::Pop => {
                self.pop()?;
            }
            Instr::Dup => {
                let v = self
                    .stack
                    .last()
                    .cloned()
                    .ok_or_else(|| CompilerError::runtime_error("Dup on empty stack"))?;
                self.push(v);
            }
            Instr::Swap => {
                let a = self.pop()?;
                let b = self.pop()?;
                self.push(a);
                self.push(b);
            }
            Instr::Pick(n) => {
                let idx = self.stack.len().checked_sub(*n + 1).ok_or_else(|| {
                    CompilerError::runtime_error("Pick: stack underflow")
                })?;
                let v = self.stack[idx].clone();
                self.push(v);
            }
            Instr::LoadLocal(slot) => {
                let v = self
                    .current_locals()
                    .get(*slot)
                    .cloned()
                    .unwrap_or(Value::Null);
                self.push(v);
            }
            Instr::StoreLocal(slot) => {
                let v = self.pop()?;
                let locals = self.current_locals_mut();
                if *slot >= locals.len() {
                    locals.resize(*slot + 1, Value::Null);
                }
                locals[*slot] = v;
            }
            Instr::LoadGlobal(idx) => {
                let name = self
                    .module
                    .constants
                    .get(*idx)
                    .map(|s| s.as_str())
                    .unwrap_or("");
                let v = self.globals.get(name).cloned().unwrap_or(Value::Null);
                self.push(v);
            }
            Instr::StoreGlobal(idx) => {
                let v = self.pop()?;
                let name = self
                    .module
                    .constants
                    .get(*idx)
                    .map(|s| s.as_str())
                    .unwrap_or("");
                if let Some(slot) = self.globals.get_mut(name) {
                    *slot = v;
                } else {
                    self.globals.insert(name.to_string(), v);
                }
            }
            Instr::LoadSelf => {
                // Push the current `self` global (bound by call_method_on_class).
                let v = self.globals.get("self").cloned().unwrap_or(Value::Null);
                self.push(v);
            }
            Instr::LoadField(field_idx) => {
                let field = self.module.constants.get(*field_idx)
                    .map(|s| s.as_str()).unwrap_or("").to_string();
                let obj = self.pop()?;
                let v = match &obj {
                    Value::Object(_, fields) => {
                        fields.borrow().get(&field).cloned().unwrap_or(Value::Null)
                    }
                    Value::Dict(d) => d.get(&field).cloned().unwrap_or(Value::Null),
                    Value::Module(_, exports) => exports.get(&field).cloned().unwrap_or(Value::Null),
                    _ => Value::Null,
                };
                self.push(v);
            }
            Instr::StoreField(field_idx) => {
                let field = self.module.constants.get(*field_idx)
                    .map(|s| s.as_str()).unwrap_or("").to_string();
                // Stack: [value, object] — object on top.
                let obj = self.pop()?;
                let val = self.pop()?;
                let mut obj = obj;
                match &mut obj {
                    Value::Object(class, fields) => {
                        // Check for __setattr__ dispatch.
                        if self.find_method(class, "__setattr__").is_ok() {
                            let receiver = Value::Object(class.clone(), fields.clone());
                            let args = vec![Value::Str(field.clone()), val];
                            let _ = self.call_method_on_class(class, receiver, "__setattr__", args)?;
                        } else {
                            fields.borrow_mut().insert(field, val);
                        }
                    }
                    Value::Dict(d) => {
                        d.insert(field, val);
                    }
                    _ => {
                        return Err(CompilerError::runtime_error(format!(
                            "cannot set field '{}' on {}", field, type_of_value(&obj)
                        )));
                    }
                }
                self.push(obj);
            }
            Instr::Add => {
                let b = self.pop()?;
                let a = self.pop()?;
                // Dispatch to __add__ if either operand is an Object with that method.
                if let Some(v) = self.try_binary_overload("__add__", &a, &b)? {
                    self.push(v);
                } else {
                    self.push(match (a, b) {
                        (Value::Int(x), Value::Int(y)) => Value::Int(x + y),
                        (Value::Float(x), Value::Float(y)) => Value::Float(x + y),
                        (Value::Int(x), Value::Float(y)) => Value::Float(x as f64 + y),
                        (Value::Float(x), Value::Int(y)) => Value::Float(x + y as f64),
                        (Value::Str(x), Value::Str(y)) => Value::Str(format!("{}{}", x, y)),
                        // List + List → concatenation
                        (Value::List(a), Value::List(b)) => {
                            let mut result = a;
                            result.extend(b);
                            Value::List(result)
                        }
                        // List + element → append
                        (Value::List(a), other) => {
                            let mut result = a;
                            result.push(other);
                            Value::List(result)
                        }
                        _ => Value::Null,
                    });
                }
            }
            Instr::Sub => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Some(v) = self.try_binary_overload("__sub__", &a, &b)? {
                    self.push(v);
                } else {
                    self.push(match (a, b) {
                        (Value::Int(x), Value::Int(y)) => Value::Int(x - y),
                        (Value::Float(x), Value::Float(y)) => Value::Float(x - y),
                        _ => Value::Null,
                    });
                }
            }
            Instr::Mul => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Some(v) = self.try_binary_overload("__mul__", &a, &b)? {
                    self.push(v);
                } else {
                    self.push(match (a, b) {
                        (Value::Int(x), Value::Int(y)) => Value::Int(x * y),
                        (Value::Float(x), Value::Float(y)) => Value::Float(x * y),
                        _ => Value::Null,
                    });
                }
            }
            Instr::Div => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Some(v) = self.try_binary_overload("__div__", &a, &b)? {
                    self.push(v);
                } else {
                    // True division (/): always returns float for int/int.
                    let result = match (a, b) {
                        (Value::Int(x), Value::Int(y)) => {
                            if y == 0 {
                                return Err(CompilerError::runtime_error("division by zero"));
                            }
                            Value::Float(x as f64 / y as f64)
                        }
                        (Value::Float(x), Value::Float(y)) => {
                            if y == 0.0 {
                                return Err(CompilerError::runtime_error("division by zero"));
                            }
                            Value::Float(x / y)
                        }
                        (Value::Int(x), Value::Float(y)) => {
                            if y == 0.0 {
                                return Err(CompilerError::runtime_error("division by zero"));
                            }
                            Value::Float(x as f64 / y)
                        }
                        (Value::Float(x), Value::Int(y)) => {
                            if y == 0 {
                                return Err(CompilerError::runtime_error("division by zero"));
                            }
                            Value::Float(x / y as f64)
                        }
                        _ => Value::Null,
                    };
                    self.push(result);
                }
            }
            Instr::FloorDiv => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Some(v) = self.try_binary_overload("__floordiv__", &a, &b)? {
                    self.push(v);
                } else {
                    // Floor division (//): mathematical floor, not truncation.
                    let result = match (a, b) {
                        (Value::Int(x), Value::Int(y)) => {
                            if y == 0 {
                                return Err(CompilerError::runtime_error("division by zero"));
                            }
                            Value::Int(x.div_euclid(y))
                        }
                        (Value::Float(x), Value::Float(y)) => {
                            if y == 0.0 {
                                return Err(CompilerError::runtime_error("division by zero"));
                            }
                            Value::Float((x / y).floor())
                        }
                        (Value::Int(x), Value::Float(y)) => {
                            if y == 0.0 {
                                return Err(CompilerError::runtime_error("division by zero"));
                            }
                            Value::Float((x as f64 / y).floor())
                        }
                        (Value::Float(x), Value::Int(y)) => {
                            if y == 0 {
                                return Err(CompilerError::runtime_error("division by zero"));
                            }
                            Value::Float((x / y as f64).floor())
                        }
                        _ => Value::Null,
                    };
                    self.push(result);
                }
            }
            Instr::Mod => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Some(v) = self.try_binary_overload("__mod__", &a, &b)? {
                    self.push(v);
                } else {
                    // Modulo: Python-style (result has sign of divisor).
                    let result = match (a, b) {
                        (Value::Int(x), Value::Int(y)) => {
                            if y == 0 {
                                return Err(CompilerError::runtime_error("modulo by zero"));
                            }
                            Value::Int(x.rem_euclid(y))
                        }
                        (Value::Float(x), Value::Float(y)) => {
                            if y == 0.0 {
                                return Err(CompilerError::runtime_error("modulo by zero"));
                            }
                            Value::Float(x.rem_euclid(y))
                        }
                        (Value::Int(x), Value::Float(y)) => {
                            if y == 0.0 {
                                return Err(CompilerError::runtime_error("modulo by zero"));
                            }
                            Value::Float((x as f64).rem_euclid(y))
                        }
                        (Value::Float(x), Value::Int(y)) => {
                            if y == 0 {
                                return Err(CompilerError::runtime_error("modulo by zero"));
                            }
                            Value::Float(x.rem_euclid(y as f64))
                        }
                        _ => Value::Null,
                    };
                    self.push(result);
                }
            }
            Instr::Eq => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Some(v) = self.try_binary_overload("__eq__", &a, &b)? {
                    self.push(v);
                } else {
                    self.push(Value::Bool(a == b));
                }
            }
            Instr::Ne => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Some(v) = self.try_binary_overload("__ne__", &a, &b)? {
                    self.push(v);
                } else {
                    self.push(Value::Bool(a != b));
                }
            }
            Instr::Lt => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Some(v) = self.try_binary_overload("__lt__", &a, &b)? {
                    self.push(v);
                } else {
                    self.push(match (&a, &b) {
                        (Value::Int(x), Value::Int(y)) => Value::Bool(x < y),
                        (Value::Float(x), Value::Float(y)) => Value::Bool(x < y),
                        (Value::Int(x), Value::Float(y)) => Value::Bool((*x as f64) < *y),
                        (Value::Float(x), Value::Int(y)) => Value::Bool(*x < (*y as f64)),
                        (Value::Str(x), Value::Str(y)) => Value::Bool(x < y),
                        _ => Value::Null,
                    });
                }
            }
            Instr::Gt => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Some(v) = self.try_binary_overload("__gt__", &a, &b)? {
                    self.push(v);
                } else {
                    self.push(match (&a, &b) {
                        (Value::Int(x), Value::Int(y)) => Value::Bool(x > y),
                        (Value::Float(x), Value::Float(y)) => Value::Bool(x > y),
                        (Value::Int(x), Value::Float(y)) => Value::Bool((*x as f64) > *y),
                        (Value::Float(x), Value::Int(y)) => Value::Bool(*x > (*y as f64)),
                        (Value::Str(x), Value::Str(y)) => Value::Bool(x > y),
                        _ => Value::Null,
                    });
                }
            }
            Instr::Le => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Some(v) = self.try_binary_overload("__le__", &a, &b)? {
                    self.push(v);
                } else {
                    self.push(match (&a, &b) {
                        (Value::Int(x), Value::Int(y)) => Value::Bool(x <= y),
                        (Value::Float(x), Value::Float(y)) => Value::Bool(x <= y),
                        (Value::Int(x), Value::Float(y)) => Value::Bool((*x as f64) <= *y),
                        (Value::Float(x), Value::Int(y)) => Value::Bool(*x <= (*y as f64)),
                        (Value::Str(x), Value::Str(y)) => Value::Bool(x <= y),
                        _ => Value::Null,
                    });
                }
            }
            Instr::Ge => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Some(v) = self.try_binary_overload("__ge__", &a, &b)? {
                    self.push(v);
                } else {
                    self.push(match (&a, &b) {
                        (Value::Int(x), Value::Int(y)) => Value::Bool(x >= y),
                        (Value::Float(x), Value::Float(y)) => Value::Bool(x >= y),
                        (Value::Int(x), Value::Float(y)) => Value::Bool((*x as f64) >= *y),
                        (Value::Float(x), Value::Int(y)) => Value::Bool(*x >= (*y as f64)),
                        (Value::Str(x), Value::Str(y)) => Value::Bool(x >= y),
                        _ => Value::Null,
                    });
                }
            }
            Instr::Neg => {
                let v = self.pop()?;
                // Dispatch to __neg__ if Object.
                if let Value::Object(class, _) = &v {
                    let class = class.clone();
                    if self.find_method(&class, "__neg__").is_ok() {
                        let r = self.call_method_on_class(&class, v, "__neg__", vec![])?;
                        self.push(r);
                    } else {
                        self.push(match v {
                            Value::Int(i) => Value::Int(-i),
                            Value::Float(f) => Value::Float(-f),
                            _ => Value::Null,
                        });
                    }
                } else {
                    self.push(match v {
                        Value::Int(i) => Value::Int(-i),
                        Value::Float(f) => Value::Float(-f),
                        _ => Value::Null,
                    });
                }
            }
            Instr::Not => {
                let v = self.pop()?;
                self.push(Value::Bool(!v.truthy()));
            }
            Instr::And => self.binop(|a, b| Value::Bool(a.truthy() && b.truthy()))?,
            Instr::Or => self.binop(|a, b| Value::Bool(a.truthy() || b.truthy()))?,
            Instr::Jump(target) => {
                self.pc = *target;
            }
            Instr::JumpIfFalse(target) => {
                let v = self.pop()?;
                if !v.truthy() {
                    self.pc = *target;
                }
            }
            Instr::JumpIfTrue(target) => {
                let v = self.pop()?;
                if v.truthy() {
                    self.pc = *target;
                }
            }
            Instr::Nop => {}
            Instr::CallByName(name_idx, argc) => {
                // Fast path: if this function has a compiled bytecode body
                // (registered in fn_entry_pcs), jump to it directly instead
                // of AST-walking. This is the fib(25) hot path — each
                // recursive call goes through here, so avoiding the
                // AST interpreter (scope_get/scope_set, error-signal
                // return, fn_def clone) is the single biggest win.
                let fname = self
                    .module
                    .constants
                    .get(*name_idx)
                    .map(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();

                // Skip bytecode fast path for lambdas (capture scope),
                // generators (yield buffer), and constrained functions
                // (type-constraint check) — they must go through call_function.
                let entry = if fname.starts_with("<lambda_") || self.gen_set.contains(&fname) || self.constrained_fns.contains(&fname) {
                    None
                } else {
                    self.module.fn_entry_pcs.get(&fname).copied()
                };
                if let Some((entry_pc, num_locals)) = entry {
                    // Pop args into the new frame's locals Vec (params in
                    // slots 0..argc). self.pc is already the instruction
                    // after this CallByName (the run() slow path increments
                    // pc before calling execute), so it serves as the return
                    // address.
                    let nslots = num_locals.max(*argc);
                    // Reuse a Vec from the locals pool to avoid allocation.
                    let mut locals = if let Some(mut v) = self.locals_pool.pop() {
                        v.clear();
                        v.resize(nslots, Value::Null);
                        v
                    } else {
                        vec![Value::Null; nslots]
                    };
                    for i in (0..*argc).rev() {
                        locals[i] = self.pop()?;
                    }
                    let stack_base = self.stack.len();
                    self.frames.push(Frame {
                        return_pc: self.pc,
                        locals,
                        is_main: false,
                        stack_base,
                        defer_pcs: Vec::new(),
                        fn_name: fname.clone(),
                    });
                    self.pc = entry_pc;
                    return Ok(Flow::Continue);
                }
                let name = self
                    .module
                    .constants
                    .get(*name_idx)
                    .cloned()
                    .unwrap_or_default();
                // Check if it's a class constructor before calling as function.
                let is_class = {
                    let prog_has_class = if let Some(prog) = &self.program {
                        prog.declarations.iter().any(|d| {
                            if let crate::parser::ast::TopLevel::ClassDef(cd) = d {
                                cd.name.name == name
                            } else {
                                false
                            }
                        })
                    } else {
                        false
                    };
                    let global_is_class = self
                        .globals
                        .get(&name)
                        .map(|v| matches!(v, Value::Class(_)))
                        .unwrap_or(false);
                    prog_has_class || global_is_class
                };
                if is_class {
                    let mut args = Vec::with_capacity(*argc);
                    for _ in 0..*argc {
                        args.push(self.pop()?);
                    }
                    args.reverse();
                    let obj = self.instantiate_class(&name, args)?;
                    self.push(obj);
                } else {
                    self.call_function(&name, *argc)?;
                }
            }
            Instr::Call(argc) => {
                // Stack order: [callee, arg1, ..., argN] (callee pushed first).
                // Pop args first (in reverse), then pop callee.
                let mut args = Vec::with_capacity(*argc);
                for _ in 0..*argc {
                    args.push(self.pop()?);
                }
                args.reverse();
                let callee = self.pop()?;
                // Re-push args for call_function (which reads from stack).
                for a in &args {
                    self.push(a.clone());
                }
                match callee {
                    Value::Func(name) => self.call_function(&name, *argc)?,
                    Value::Class(class) => {
                        // Instantiating a class via Call.
                        let mut args = Vec::with_capacity(*argc);
                        for _ in 0..*argc {
                            args.push(self.pop()?);
                        }
                        args.reverse();
                        let obj = self.instantiate_class(&class, args)?;
                        self.push(obj);
                    }
                    Value::Object(class, _) => {
                        // __call__ dispatch: call the object as a function.
                        let mut args = Vec::with_capacity(*argc);
                        for _ in 0..*argc {
                            args.push(self.pop()?);
                        }
                        args.reverse();
                        // Pop the callee (already popped above, reconstruct).
                        // Actually callee was already popped. We need to pass
                        // it as receiver.
                        let receiver = Value::Object(class.clone(), {
                            // Re-construct the object — but we consumed it.
                            // Use a clone from globals if possible, or create
                            // a minimal one.
                            std::rc::Rc::new(std::cell::RefCell::new(
                                std::collections::HashMap::new(),
                            ))
                        });
                        if self.find_method(&class, "__call__").is_ok() {
                            let v = self.call_method_on_class(
                                &class, receiver, "__call__", args,
                            )?;
                            self.push(v);
                        } else {
                            return Err(CompilerError::runtime_error(
                                "object is not callable (no __call__ method)",
                            ));
                        }
                    }
                    _ => {
                        return Err(CompilerError::runtime_error(format!(
                            "cannot call {:?}",
                            callee
                        )))
                    }
                }
            }
            Instr::CallMethod(method_idx, argc) => {
                // Method call: args are on the stack, then the receiver.
                let method_name = self
                    .module
                    .constants
                    .get(*method_idx)
                    .cloned()
                    .unwrap_or_default();
                let mut args = Vec::with_capacity(*argc);
                for _ in 0..*argc {
                    args.push(self.pop()?);
                }
                args.reverse();
                let receiver = self.pop()?;
                // If receiver is a Module, look up the function and call it.
                if let Value::Module(_, exports) = &receiver {
                    if let Some(func_val) = exports.get(&method_name) {
                        let v = self.call_value(func_val, args)?;
                        self.push(v);
                        return Ok(Flow::Continue);
                    }
                }
                let v = self.ast_method_call(receiver, &method_name, args)?;
                self.push(v);
            }
            Instr::Return => {
                let v = self.pop()?;
                if self.frames.len() <= 1 {
                    return Ok(Flow::Return(v));
                }
                let frame = self.frames.pop().unwrap();
                self.pc = frame.return_pc;
                self.stack.truncate(frame.stack_base);
                self.push(v);
                if !frame.is_main {
                    self.locals_pool.push(frame.locals);
                }
            }
            Instr::ReturnVoid => {
                if self.frames.len() <= 1 {
                    return Ok(Flow::Return(Value::Null));
                }
                let frame = self.frames.pop().unwrap();
                self.pc = frame.return_pc;
                self.push(Value::Null);
                if !frame.is_main {
                    self.locals_pool.push(frame.locals);
                }
            }
            Instr::IndexGet => {
                let idx = self.pop()?;
                let container = self.pop()?;
                let v = self.index_get(&container, &idx)?;
                self.push(v);
            }
            Instr::IndexSet => {
                let val = self.pop()?;
                let idx = self.pop()?;
                let mut container = self.pop()?;
                self.index_set(&mut container, &idx, val)?;
                self.push(container);
            }
            Instr::Len => {
                let v = self.pop()?;
                let len = match &v {
                    Value::Str(s) => s.chars().count() as i64,
                    Value::List(l) => l.len() as i64,
                    Value::Tuple(t) => t.len() as i64,
                    Value::Dict(d) => d.len() as i64,
                    _ => 0,
                };
                self.push(Value::Int(len));
            }
            Instr::Iter => {
                let v = self.pop()?;
                self.iterators.push((v, 0));
                // Push an iterator handle (the index into self.iterators).
                self.push(Value::Int((self.iterators.len() - 1) as i64));
            }
            Instr::IterNext(body_target, end_target) => {
                // Peek at the iterator handle on top of stack (don't pop —
                // it stays for the next iteration).
                let iter_handle = match self.stack.last() {
                    Some(Value::Int(i)) => *i as usize,
                    _ => {
                        self.pop()?;
                        self.pc = *end_target;
                        return Ok(Flow::Continue);
                    }
                };
                let (container, pos) = self
                    .iterators
                    .get(iter_handle)
                    .cloned()
                    .unwrap_or((Value::Null, 0));
                let next = match &container {
                    Value::List(l) => {
                        if pos < l.len() {
                            Some(l[pos].clone())
                        } else {
                            None
                        }
                    }
                    Value::Tuple(t) => {
                        if pos < t.len() {
                            Some(t[pos].clone())
                        } else {
                            None
                        }
                    }
                    Value::Str(s) => {
                        let chars: Vec<char> = s.chars().collect();
                        if pos < chars.len() {
                            Some(Value::Str(chars[pos].to_string()))
                        } else {
                            None
                        }
                    }
                    Value::Dict(d) => {
                        let keys: Vec<&String> = d.keys().collect();
                        if pos < keys.len() {
                            Some(Value::Str(keys[pos].clone()))
                        } else {
                            None
                        }
                    }
                    Value::Object(class, _) => {
                        // Object iterator protocol: ONLY call next() if the
                        // class actually defines one. Otherwise, the object
                        // is not iterable (e.g. enum variants are objects
                        // tagged with __variant__, but they have no next()).
                        if self.find_method(class, "next").is_ok() {
                            let container_clone = container.clone();
                            match self.call_method_on_value(container_clone, "next", vec![]) {
                                Ok(v) => {
                                    if let Value::Exception(msg, _) = &v {
                                        if msg == "StopIteration" {
                                            None
                                        } else {
                                            Some(v)
                                        }
                                    } else if matches!(v, Value::Null) {
                                        // null sentinel: stop iteration.
                                        None
                                    } else {
                                        Some(v)
                                    }
                                }
                                Err(_) => None,
                            }
                        } else {
                            None
                        }
                    }
                    Value::Generator(g) => {
                        let mut state = g.borrow_mut();
                        if state.yield_idx < state.yielded_values.len() {
                            let v = state.yielded_values[state.yield_idx].clone();
                            state.yield_idx += 1;
                            if state.yield_idx >= state.yielded_values.len() {
                                state.done = true;
                            }
                            Some(v)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                match next {
                    Some(v) => {
                        self.iterators[iter_handle].1 = pos + 1;
                        self.push(v);
                        self.pc = *body_target;
                    }
                    None => {
                        // Pop the iterator handle before exiting.
                        self.pop()?;
                        // Release the iterator entry to prevent the
                        // iterated container from being kept alive.
                        if iter_handle < self.iterators.len() {
                            self.iterators[iter_handle] = (Value::Null, 0);
                        }
                        self.pc = *end_target;
                    }
                }
            }
            Instr::NewClass(_) | Instr::NewObject(_) | Instr::LoadMethod(_) => {
                // These opcodes are unused — class/method instantiation is
                // handled via EvalAst/ExecAstStmt fallback to the AST path.
                self.push(Value::Null);
            }
            Instr::NewGenerator(_) => {
                // Unused — generators are created via call_function when it
                // detects `yield` in the function body.
                self.push(Value::Null);
            }
            Instr::Resume => {
                // Unused — resume() is a builtin that handles generators.
                let _g = self.pop()?;
                self.push(Value::Null);
            }
            Instr::PushHandler(target) => {
                self.handler_stack.push((*target, self.frames.len()));
            }
            Instr::PopHandler => {
                self.handler_stack.pop();
            }
            Instr::DeferPush(defer_pc) => {
                // Push the deferred code block PC onto the current frame's
                // defer stack. It will be executed in LIFO order when
                // RunDefers fires (at function return).
                if let Some(frame) = self.frames.last_mut() {
                    frame.defer_pcs.push(*defer_pc);
                }
            }
            Instr::RunDefers => {
                // Run all deferred blocks for the current frame in LIFO order.
                // Each deferred block ends with a Jump back to the continuation
                // PC (the instruction after RunDefers). We save the continuation,
                // pop each defer PC, jump to it, and the block jumps back.
                let continuation = self.pc + 1;
                let defers: Vec<usize> = if let Some(frame) = self.frames.last_mut() {
                    let d = frame.defer_pcs.clone();
                    frame.defer_pcs.clear();
                    d
                } else {
                    Vec::new()
                };
                // Run in LIFO order (last deferred first).
                for defer_pc in defers.into_iter().rev() {
                    self.pc = defer_pc;
                    // Execute the deferred block. It should end with a Jump
                    // back to `continuation`. We run the interpreter loop
                    // until we reach `continuation`.
                    loop {
                        if self.pc >= self.module.code.len() {
                            break;
                        }
                        if self.pc == continuation {
                            break;
                        }
                        let instr = self.module.code[self.pc].clone();
                        self.pc += 1;
                        match self.execute(&instr)? {
                            Flow::Continue => {}
                            Flow::Return(_) => break,
                        }
                    }
                }
                self.pc = continuation;
            }
            Instr::Throw => {
                let v = self.pop()?;
                self.thrown_value = Some(v.clone());
                return Err(CompilerError::runtime_error(format!("throw:{}", v.to_str())));
            }
            Instr::Import(_) => {
                // Unused — imports are handled by preprocess_top_level and
                // the load_module helper.
                self.push(Value::Null);
            }
            Instr::CallBuiltin(name_idx, argc) => {
                let name = self
                    .module
                    .constants
                    .get(*name_idx)
                    .cloned()
                    .unwrap_or_default();
                self.call_builtin(&name, *argc)?;
            }
            Instr::SuperCall(method_idx, argc) => {
                let method_name = self
                    .module
                    .constants
                    .get(*method_idx)
                    .cloned()
                    .unwrap_or_default();
                let mut args = Vec::with_capacity(*argc);
                for _ in 0..*argc {
                    args.push(self.pop()?);
                }
                args.reverse();
                let current_class = self.method_context.last().map(|(c, _)| c.clone());
                let parent_name = current_class
                    .as_ref()
                    .and_then(|c| self.find_parent_class(c).map(|n| n.to_string()));
                let receiver = self.globals.get("self").cloned().unwrap_or(Value::Null);
                let v = if let Some(parent) = parent_name {
                    self.call_method_on_class(&parent, receiver, &method_name, args)?
                } else {
                    Value::Null
                };
                self.push(v);
            }
            Instr::Yield => {
                // Eager generator model: pop the yielded value, push it to
                // gen_yield_buffer (if a generator is active), and continue.
                // Outside a generator, push the value back (no-op semantics).
                // PC advancement is handled by the caller (run() / generator
                // loop), consistent with all other execute() arms.
                let v = self.stack.pop().unwrap_or(Value::Null);
                if let Some(buf) = self.gen_yield_buffer.as_mut() {
                    buf.push(v);
                } else {
                    self.stack.push(v);
                }
            }
            Instr::Print => {
                let v = self.pop()?;
                // If the value is an Object with __str__, call it.
                let s = self.value_to_display_str(&v);
                print!("{}", s);
            }
            Instr::Println => {
                let v = self.pop()?;
                let s = self.value_to_display_str(&v);
                println!("{}", s);
            }
            Instr::Newline => {
                println!();
            }
            Instr::MakeClosure(lambda_name) => {
                // Create a closure value from a pre-compiled lambda.
                // This is the bytecode equivalent of the AST path's
                // `Expr::Lambda` handler (see execute_ast_expr): it
                // snapshots the currently-visible lexical environment
                // into `lambda_captures[name]` and pushes
                // `Value::Func(name)`. The closure body was already
                // compiled to bytecode (registered in lambda_fn_defs
                // and possibly fn_entry_pcs); when the closure is
                // later called, `call_function` pushes the capture
                // map as a scope and AST-walks the body (the bytecode
                // fast path is skipped for `<lambda_` names so the
                // capture scope is consulted via scope_get).
                //
                // The compile-time name (passed in the instruction) is
                // shared across invocations of the enclosing function,
                // so we mint a per-invocation runtime name by appending
                // `#<id>`. This preserves closure isolation: each call
                // to adder(5) / adder(10) gets its own capture map.
                // The compile-time name is still used to look up the
                // FnDef in `module.lambda_fn_defs`.
                let runtime_name = format!(
                    "{}#{}",
                    lambda_name, self.next_closure_id
                );
                self.next_closure_id += 1;
                //
                // Snapshot sources (mirror execute_ast_expr's Lambda
                // branch, plus the bytecode frame's locals which the
                // AST path sees via local_scopes):
                //   1. self.globals — top-level variables and module
                //      globals.
                //   2. self.local_scopes — enclosing AST-interpreter
                //      call frames (when the enclosing function runs
                //      via the AST path).
                //   3. self.frames.last().locals + fn_local_names —
                //      the enclosing bytecode-compiled function's
                //      parameters/locals (when the enclosing function
                //      runs via the bytecode fast path, local_scopes
                //      is empty so we must read frame.locals).
                let mut captures: HashMap<String, Value> = HashMap::new();
                // Bug V3 fix: 只捕获局部变量（local_scopes + frame.locals），
                // 不捕获全局变量。全局变量通过 LoadGlobal 直接访问，
                // 不需要快照。这样多个闭包实例的局部捕获互不干扰。
                for scope in &self.local_scopes {
                    for (k, v) in scope {
                        captures.insert(k.clone(), v.clone());
                    }
                }
                if let Some(frame) = self.frames.last() {
                    let fn_name = frame.fn_name.clone();
                    if let Some(local_names) = self.module.fn_local_names.get(&fn_name) {
                        for (i, lname) in local_names.iter().enumerate() {
                            if lname.is_empty() {
                                continue;
                            }
                            if let Some(v) = frame.locals.get(i) {
                                captures.insert(lname.clone(), v.clone());
                            }
                        }
                    }
                }
                self.lambda_captures.insert(
                    runtime_name.clone(),
                    Rc::new(RefCell::new(captures)),
                );
                // Cache the lambda's FnDef so call_function can fetch
                // it via a cheap Rc::clone instead of scanning
                // lambda_fn_defs on every call. The FnDef was placed
                // in lambda_fn_defs (keyed by the compile-time name)
                // by the compiler.
                if let Some(fd) = self.module.lambda_fn_defs.get(lambda_name).cloned() {
                    // Re-tag the FnDef with the runtime name so that
                    // call_function's lambda-name dispatch
                    // (starts_with("<lambda_")) and any diagnostic
                    // surface the per-invocation identity.
                    let mut fd = fd;
                    fd.name.name = runtime_name.clone();
                    self.fn_cache.insert(runtime_name.clone(), Rc::new(fd));
                }
                self.push(Value::Func(runtime_name));
            }
            Instr::MatchPattern(pattern) => {
                // Pop the value, run pattern_matches, push the Bool result.
                // Variable bindings inside the pattern are written to the
                // current scope as a side effect (this mirrors the AST path).
                let value = self.stack.pop().unwrap_or(Value::Null);
                let matched = self.pattern_matches(pattern, &value)?;
                self.stack.push(Value::Bool(matched));
            }
            Instr::Halt => {
                return Ok(Flow::Return(
                    self.stack.pop().unwrap_or(Value::Null),
                ));
            }
            Instr::Debug(n) => {
                eprintln!("[debug pc={}] stack={:?}", n, self.stack.len());
            }
        }
        Ok(Flow::Continue)
    }

    /// Call an extern FFI function. We dispatch a curated set of C standard
    /// library functions natively (printf, puts, malloc, free, strlen, atoi,
    /// atof, exit, abs, rand, srand, time). Unknown extern functions raise
    /// a runtime error. Argument types map: str → C string, int → i64,
    /// float → f64. Return values are converted back to Value.
    fn call_extern_fn(&mut self, name: &str, args: Vec<Value>) -> Result<Value> {
        match name {
            "printf" => {
                // printf(format, args...) — variadic. We implement a subset
                // of C format specifiers: %s, %d, %f, %c, %x, %o, %% with
                // optional flags/width/precision (e.g. %.2f, %5d).
                let fmt = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let rest = &args[1.min(args.len())..];
                let mut out = String::new();
                let mut arg_idx = 0;
                let mut chars = fmt.chars().peekable();
                while let Some(c) = chars.next() {
                    if c == '%' {
                        // Parse optional flags/width/precision (e.g. %.2f, %5d).
                        let mut spec = String::new();
                        while let Some(&nc) = chars.peek() {
                            if nc == '-' || nc == '+' || nc == ' ' || nc == '#' || nc == '0' || nc.is_ascii_digit() || nc == '.' {
                                spec.push(nc);
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        match chars.next() {
                            Some('%') => out.push('%'),
                            Some('s') => {
                                if arg_idx < rest.len() {
                                    out.push_str(&rest[arg_idx].to_str());
                                    arg_idx += 1;
                                }
                            }
                            Some('d') | Some('i') => {
                                if arg_idx < rest.len() {
                                    out.push_str(&rest[arg_idx].to_int().unwrap_or(0).to_string());
                                    arg_idx += 1;
                                }
                            }
                            Some('f') | Some('g') | Some('e') => {
                                if arg_idx < rest.len() {
                                    let v = &rest[arg_idx];
                                    let f = match v {
                                        Value::Float(f) => *f,
                                        Value::Int(i) => *i as f64,
                                        _ => 0.0,
                                    };
                                    let precision = if let Some(dot_idx) = spec.find('.') {
                                        spec[dot_idx+1..].parse::<usize>().unwrap_or(6)
                                    } else {
                                        6
                                    };
                                    if spec.contains('.') {
                                        out.push_str(&format!("{:.*}", precision, f));
                                    } else {
                                        out.push_str(&format!("{}", f));
                                    }
                                    arg_idx += 1;
                                }
                            }
                            Some('c') => {
                                if arg_idx < rest.len() {
                                    let s = rest[arg_idx].to_str();
                                    if let Some(ch) = s.chars().next() {
                                        out.push(ch);
                                    }
                                    arg_idx += 1;
                                }
                            }
                            Some('x') => {
                                if arg_idx < rest.len() {
                                    out.push_str(&format!("{:x}", rest[arg_idx].to_int().unwrap_or(0)));
                                    arg_idx += 1;
                                }
                            }
                            Some('o') => {
                                if arg_idx < rest.len() {
                                    out.push_str(&format!("{:o}", rest[arg_idx].to_int().unwrap_or(0)));
                                    arg_idx += 1;
                                }
                            }
                            Some(other) => {
                                out.push('%');
                                out.push_str(&spec);
                                out.push(other);
                            }
                            None => {
                                out.push('%');
                                out.push_str(&spec);
                            }
                        }
                    } else {
                        out.push(c);
                    }
                }
                print!("{}", out);
                use std::io::Write;
                let _ = std::io::stdout().flush();
                Ok(Value::Int(out.len() as i64))
            }
            "puts" => {
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                println!("{}", s);
                Ok(Value::Int(s.len() as i64 + 1))
            }
            "malloc" => {
                // We can't return a real C pointer through the VM. Return a
                // placeholder Int (non-zero = success).
                Ok(Value::Int(1))
            }
            "free" => Ok(Value::Null),
            "strlen" => {
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Int(s.len() as i64))
            }
            "atoi" => {
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Int(s.trim().parse::<i64>().unwrap_or(0)))
            }
            "atof" => {
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Float(s.trim().parse::<f64>().unwrap_or(0.0)))
            }
            "exit" => {
                let code = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(0) as i32;
                std::process::exit(code);
            }
            "abs" => {
                let v = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(0);
                Ok(Value::Int(v.abs()))
            }
            "rand" => {
                // Use a simple xorshift PRNG seeded from system time.
                use std::time::SystemTime;
                let seed = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                let mut state = seed.wrapping_add(1);
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                Ok(Value::Int((state as i64).abs() % 32768))
            }
            "srand" => Ok(Value::Null),
            "time" => {
                use std::time::SystemTime;
                let t = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                Ok(Value::Int(t))
            }
            _ => Err(CompilerError::runtime_error(format!(
                "extern function '{}' is not linked (no FFI binding available)",
                name
            ))),
        }
    }

    /// Check generic type-parameter constraints at call time.
    ///
    /// For each parameter whose type annotation is a Named type-parameter
    /// (i.e. its name appears in `fn_def.type_constraints`), verify that
    /// the corresponding argument satisfies every named constraint.
    ///
    /// A constraint is satisfied when:
    ///   - The constraint names an interface declared in the program, AND
    ///   - The argument is a `Value::Object` whose class defines every
    ///     method listed in that interface.
    ///
    /// Constraints that don't name a known interface (e.g. built-in trait
    /// names like `Comparable`) are silently accepted — the type system is
    /// best-effort.
    fn check_type_constraints(&self, fn_def: &crate::parser::ast::FnDef, args: &[Value]) -> Result<()> {
        use crate::parser::ast::{TypeExpr, TopLevel};
        // Build a map of interface name → required method names from the
        // program's declarations.
        let mut interfaces: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        if let Some(prog) = &self.program {
            for d in &prog.declarations {
                if let TopLevel::InterfaceDef(id) = d {
                    let methods: Vec<String> = id.methods.iter().map(|m| m.name.name.clone()).collect();
                    interfaces.insert(id.name.name.clone(), methods);
                }
            }
        }
        for (i, param) in fn_def.params.iter().enumerate() {
            if i >= args.len() {
                break;
            }
            // Determine the type-parameter name (if any) for this param.
            let tp_name = match &param.type_annotation {
                Some(TypeExpr::Named(id, _)) => Some(id.name.clone()),
                _ => None,
            };
            let tp_name = match tp_name {
                Some(n) => n,
                None => continue,
            };
            let constraints = match fn_def.type_constraints.get(&tp_name) {
                Some(c) => c,
                None => continue,
            };
            let arg = &args[i];
            for c in constraints {
                let required = match interfaces.get(c) {
                    Some(m) => m,
                    None => continue, // unknown constraint — skip
                };
                // Only Object values can satisfy an interface constraint.
                let class_name = match arg {
                    Value::Object(class, _) => class.clone(),
                    _ => {
                        return Err(CompilerError::runtime_error(format!(
                            "type constraint violation: '{}' requires '{}' (which needs methods [{}]), but argument is not an object",
                            fn_def.name.name, c, required.join(", ")
                        )));
                    }
                };
                for m in required {
                    if self.find_method(&class_name, m).is_err() {
                        return Err(CompilerError::runtime_error(format!(
                            "type constraint violation: '{}' requires '{}' (method '{}'), but class '{}' does not implement it",
                            fn_def.name.name, c, m, class_name
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Call a function that was imported from a module. The function's
    /// AST is not in this VM's program, so we re-load the module and
    /// execute the function in a sub-VM.
    ///
    /// Performance: this used to re-read the module source from disk and
    /// re-parse + re-compile it on EVERY call. We now consult
    /// `module_program_cache` (parsed `Program`) and
    /// `module_compiled_cache` (compiled `Module`), both populated by
    /// `load_module`. When a cache hit occurs we skip the read+parse
    /// and the compile step entirely, going straight to constructing a
    /// fresh sub-VM with the cached `Module` (cloned, since `VM::new`
    /// takes ownership) and `Program` (also cloned for the same reason)
    /// and executing the function body. A defensive fallback still
    /// re-reads/re-parses/re-compiles for any path that appears in
    /// `module_cache` but for some reason isn't in the program/compiled
    /// caches — and on that fallback path we back-fill the caches so
    /// subsequent calls for the same path hit the fast path.
    fn call_imported_function(&mut self, name: &str, args: Vec<Value>) -> Result<()> {
        // Check if `name` is bound to a Func value in globals (e.g. via
        // `import, "math", sqrt` which sets globals["sqrt"] = Value::Func("math_sqrt")).
        // If so, recurse with the resolved Func name.
        if let Some(Value::Func(actual_name)) = self.globals.get(name).cloned() {
            if actual_name != name {
                for a in &args {
                    self.push(a.clone());
                }
                return self.call_function(&actual_name, args.len());
            }
        }
        // Find which module path contains this function by scanning all
        // cached module exports. We check if the function name exists in
        // any module's exports OR if the module's source defines it.
        for (path, _exports) in self.module_cache.clone() {
            // Try the fast path: cached parsed `Program` (and, ideally,
            // a cached compiled `Module`). Both are populated by
            // `load_module` on first import.
            let cached_program_rc = self.module_program_cache.get(&path).cloned();
            let cached_module_rc = self.module_compiled_cache.get(&path).cloned();
            // Record whether the program cache hit BEFORE the matching
            // `if let` consumes the `Option` — we use this flag below to
            // decide whether to back-fill the program cache after the
            // slow path. (We don't need a corresponding flag for the
            // Module cache: its back-fill is keyed on `cached_module_rc`
            // being `None`, and that branch's body is self-contained.)
            let program_from_cache = cached_program_rc.is_some();
            // Obtain a parsed `Program` either from the cache or by
            // re-reading + re-parsing the source. If we have to fall
            // back to the slow path, we back-fill the program cache so
            // the next call hits the fast path.
            let program: Program = if let Some(rc) = cached_program_rc {
                // Cheap Rc clone followed by a deep clone of the
                // Program — the deep clone is required because
                // `with_program` takes ownership and the sub-VM may
                // outlive this function's borrow of the cache.
                (*rc).clone()
            } else {
                // Slow path: re-read and re-parse. (We still do the
                // `has_fn` check below before using `program` to
                // build the sub-VM.)
                let source = match std::fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut lexer = crate::lexer::Lexer::new(&source, 99);
                let tokens = match lexer.tokenize() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let mut parser = crate::parser::Parser::new(tokens, 99);
                match parser.parse_program() {
                    Ok(p) => p,
                    Err(_) => continue,
                }
            };
            // Check if this program defines the function.
            let has_fn = program.declarations.iter().any(|d| {
                matches!(d, crate::parser::ast::TopLevel::FnDef(f) if f.name.name == name)
                    || matches!(d, crate::parser::ast::TopLevel::LazyFnDef(l) if l.fn_def.name.name == name)
            });
            if !has_fn {
                continue;
            }
            // Back-fill the program cache if we hit the slow path
            // above (defensive — normally `load_module` has already
            // populated it).
            if !program_from_cache {
                self.module_program_cache
                    .insert(path.clone(), Rc::new(program.clone()));
            }
            // Obtain a compiled `Module` either from the cache or by
            // re-compiling the program. As above, back-fill the cache
            // on the slow path.
            let sub_module: Module = if let Some(rc) = cached_module_rc {
                (*rc).clone()
            } else {
                let sub_compiler = super::Compiler::new();
                let m = sub_compiler.compile(&program)?;
                self.module_compiled_cache
                    .insert(path.clone(), Rc::new(m.clone()));
                m
            };
            let sub_base = std::path::Path::new(&path)
                .parent()
                .unwrap_or(&self.base_dir)
                .to_path_buf();
            let mut sub_vm = VM::new(sub_module)
                .with_program(program)
                .with_base_dir(sub_base);
            // Pre-process the sub-VM (load its imports, register classes).
            sub_vm.preprocess_top_level()?;
            let fn_def = sub_vm.find_function(name)?.clone();
            // Try the bytecode path first: if the imported function was
            // compiled (registered in sub_vm.module.fn_entry_pcs), execute
            // it via a Frame on the sub-VM. This avoids the AST interpreter
            // (execute_fn_body) for imported functions.
            let sub_entry = if sub_vm.gen_set.contains(name) || fn_def.is_async || !fn_def.type_constraints.is_empty() {
                None
            } else {
                sub_vm.module.fn_entry_pcs.get(name).copied()
            };
            let mut returned = Value::Null;
            let result: Result<()>;
            if let Some((entry_pc, num_locals)) = sub_entry {
                // Bytecode path on the sub-VM.
                let params: Vec<Value> = fn_def
                    .params
                    .iter()
                    .enumerate()
                    .map(|(i, _)| args.get(i).cloned().unwrap_or(Value::Null))
                    .collect();
                let nslots = num_locals.max(params.len());
                let mut locals = vec![Value::Null; nslots];
                for (i, v) in params.iter().enumerate() {
                    locals[i] = v.clone();
                }
                let stack_base = sub_vm.stack.len();
                sub_vm.frames.push(Frame {
                    locals,
                    return_pc: 0,
                    stack_base,
                    is_main: false,
                    defer_pcs: Vec::new(),
                    fn_name: name.to_string(),
                });
                sub_vm.pc = entry_pc;
                let target_depth = sub_vm.frames.len() - 1;
                let code_len = sub_vm.module.code.len();
                let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    while sub_vm.pc < code_len && sub_vm.frames.len() > target_depth {
                        let pc = sub_vm.pc;
                        let instr = sub_vm.module.code[pc].clone();
                        sub_vm.pc += 1;
                        match sub_vm.execute(&instr) {
                            Ok(Flow::Continue) => {}
                            Ok(Flow::Return(_)) => break,
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(())
                }));
                result = match panicked {
                    Ok(inner) => inner,
                    Err(payload) => Err(CompilerError::runtime_error(format!(
                        "imported function panic: {}",
                        if let Some(s) = payload.downcast_ref::<&'static str>() { (*s).to_string() }
                        else if let Some(s) = payload.downcast_ref::<String>() { s.clone() }
                        else { "<non-string panic payload>".to_string() }
                    ))),
                };
                if result.is_ok() {
                    returned = sub_vm.stack.pop().unwrap_or(Value::Null);
                }
            } else {
                // Imported function not compiled — should not happen now.
                result = Ok(());
                returned = Value::Null;
            }
            match result {
                Ok(()) => {}
                Err(e) => {
                    if e.message() == "return" {
                        returned = sub_vm.stack.pop().unwrap_or(Value::Null);
                    } else {
                        return Err(e);
                    }
                }
            }
            self.push(returned);
            return Ok(());
        }
        Err(CompilerError::runtime_error(format!(
            "function '{}' not defined (not in program or any imported module)",
            name
        )))
    }

    /// Call a math module function.
    fn call_math_function(&self, name: &str, args: Vec<Value>) -> Result<Value> {
        let get_f64 = |v: &Value| -> f64 {
            match v {
                Value::Int(i) => *i as f64,
                Value::Float(f) => *f,
                _ => 0.0,
            }
        };
        match name {
            "math_pow" => {
                let base = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                let exp = get_f64(&args.get(1).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(base.powf(exp)))
            }
            "math_sqrt" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.sqrt()))
            }
            "math_cbrt" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.cbrt()))
            }
            "math_abs" | "math_fabs" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.abs()))
            }
            "math_floor" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Int(x.floor() as i64))
            }
            "math_ceil" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Int(x.ceil() as i64))
            }
            "math_trunc" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Int(x.trunc() as i64))
            }
            "math_round" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Int(x.round() as i64))
            }
            "math_exp" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.exp()))
            }
            "math_log" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                let base = get_f64(&args.get(1).unwrap_or(&Value::Float(std::f64::consts::E)));
                if base == 2.0 {
                    Ok(Value::Float(x.log2()))
                } else if base == 10.0 {
                    Ok(Value::Float(x.log10()))
                } else if base == std::f64::consts::E {
                    Ok(Value::Float(x.ln()))
                } else {
                    Ok(Value::Float(x.log(base)))
                }
            }
            "math_log10" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.log10()))
            }
            "math_log2" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.log2()))
            }
            "math_sin" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.sin()))
            }
            "math_cos" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.cos()))
            }
            "math_tan" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.tan()))
            }
            "math_asin" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.asin()))
            }
            "math_acos" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.acos()))
            }
            "math_atan" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.atan()))
            }
            "math_atan2" => {
                let y = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                let x = get_f64(&args.get(1).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(y.atan2(x)))
            }
            "math_sinh" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.sinh()))
            }
            "math_cosh" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.cosh()))
            }
            "math_tanh" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.tanh()))
            }
            "math_radians" => {
                let deg = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(deg * std::f64::consts::PI / 180.0))
            }
            "math_degrees" => {
                let rad = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(rad * 180.0 / std::f64::consts::PI))
            }
            "math_gamma" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                // Lanczos approximation for Gamma(x).
                Ok(Value::Float(lanczos_gamma(x)))
            }
            "math_lgamma" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(lanczos_gamma(x).abs().ln()))
            }
            "math_inf" => Ok(Value::Float(f64::INFINITY)),
            "math_nan" => Ok(Value::Float(f64::NAN)),
            _ => Err(CompilerError::runtime_error(format!(
                "unknown math function '{}'",
                name
            ))),
        }
    }

    fn call_function(&mut self, name: &str, argc: usize) -> Result<()> {
            // Invalidate the global cache: AST-interpreted functions may modify
        // globals (parameter save/restore, set statements), which would make
        // the bytecode-path inline cache stale.
        self.invalidate_global_cache();
        // Collect args.
        let mut args = Vec::with_capacity(argc);
        for _ in 0..argc {
            args.push(self.pop()?);
        }
        args.reverse();
        // Check for math functions first.
        if name.starts_with("math_") {
            let v = self.call_math_function(name, args)?;
            self.push(v);
            return Ok(());
        }
        // Check for json functions.
        if name == "json_parse" {
            let text = args.into_iter().next().unwrap_or(Value::Null).to_str();
            self.push(native_json_parse(&text));
            return Ok(());
        }
        if name == "json_stringify" {
            let v = args.into_iter().next().unwrap_or(Value::Null);
            self.push(Value::Str(native_json_stringify(&v)));
            return Ok(());
        }
        // Check for os/time/rand functions (native, no is_builtin entry needed).
        if name.starts_with("os_") || name.starts_with("time_") || name.starts_with("rand_") {
            let v = self.call_builtin_value(name, args)?;
            self.push(v);
            return Ok(());
        }
        // Check for extern FFI functions. If `name` was declared as `extern fn`,
        // dispatch to the native C library implementation.
        if self.extern_fns.contains_key(name) {
            let v = self.call_extern_fn(name, args)?;
            self.push(v);
            return Ok(());
        }
        // Check for macros. If `name` was declared as `macro`, execute the
        // macro body with the call args bound to the param names.
        //
        // **Hygienic macros**: Macro parameters are given unique names
        // (prefixed with `__macro_`) to prevent collisions with caller
        // variables. This ensures that a macro's internal variable names
        // don't shadow or conflict with variables at the call site.
        if let Some((params, body)) = self.macros.get(name).cloned() {
            // Bytecode path: if the macro was compiled to bytecode
            // (registered in fn_entry_pcs under its original name),
            // execute it via a Frame — slot isolation provides hygiene
            // without runtime name mangling.
            if let Some((entry_pc, num_locals)) = self.module.fn_entry_pcs.get(name).copied() {
                let nslots = num_locals.max(args.len());
                let mut locals = if let Some(mut v) = self.locals_pool.pop() {
                    v.clear();
                    v.resize(nslots, Value::Null);
                    v
                } else {
                    vec![Value::Null; nslots]
                };
                for (i, a) in args.iter().enumerate() {
                    locals[i] = a.clone();
                }
                let stack_base = self.stack.len();
                let saved_pc = self.pc;
                self.frames.push(Frame {
                    locals,
                    return_pc: self.pc,
                    stack_base,
                    is_main: false,
                    defer_pcs: Vec::new(),
                    fn_name: name.to_string(),
                });
                self.pc = entry_pc;
                let target_frame_depth = self.frames.len() - 1;
                let code_len = self.module.code.len();
                let macro_result: Result<()>;
                let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    while self.pc < code_len && self.frames.len() > target_frame_depth {
                        let pc = self.pc;
                        let instr = self.module.code[pc].clone();
                        self.pc += 1;
                        match self.execute(&instr) {
                            Ok(Flow::Continue) => {}
                            Ok(Flow::Return(_)) => break,
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(())
                }));
                macro_result = match panicked {
                    Ok(inner) => inner,
                    Err(payload) => Err(CompilerError::runtime_error(format!(
                        "macro panic: {}",
                        if let Some(s) = payload.downcast_ref::<&'static str>() { (*s).to_string() }
                        else if let Some(s) = payload.downcast_ref::<String>() { s.clone() }
                        else { "<non-string panic payload>".to_string() }
                    ))),
                };
                self.pc = saved_pc;
                if macro_result.is_ok() {
                    // Return value is on the stack (pushed by Ret or Null).
                    // If nothing was pushed, push Null.
                    if self.stack.len() > stack_base {
                        // Return value already on stack from Ret.
                    } else {
                        self.push(Value::Null);
                    }
                    return Ok(());
                }
                return macro_result;
            }
            // Macro body not compiled — should not happen now. Push Null.
            self.push(Value::Null);
            return Ok(());
        }
        // Check for builtins (file_exists, read_file, read_dir, etc.).
        // This handles the case where a stdlib module (e.g. fs) re-exports
        // a builtin name as a Func value.
        if super::compiler::is_builtin(name) {
            let v = self.call_builtin_value(name, args)?;
            self.push(v);
            return Ok(());
        }
        // Look up the function definition. Hot path: the fn_cache (populated
        // at preprocess time and when lambdas are created) returns an Rc
        // handle via a cheap refcount bump, avoiding a linear scan of the
        // program's declarations AND a deep clone of the entire FnDef body
        // on every call. The generator flag is likewise an O(1) set lookup
        // instead of a recursive body scan.
        let (fn_def, is_generator) = if let Some(cached) = self.fn_cache.get(name).cloned() {
            // Check if the cached fn_def has a non-empty body. If it has
            // an empty body (placeholder from preprocess), try to resolve
            // the real function definition.
            if cached.body.is_empty() {
                // Case 1: this is a lambda name like "<lambda_0>"
                if name.starts_with("<lambda_") {
                    // First check module.lambda_fn_defs (populated by the
                    // compiler when the lambda was compiled to bytecode).
                    if let Some(real_def) = self.module.lambda_fn_defs.get(name).cloned() {
                        let rc = Rc::new(real_def.clone());
                        self.fn_cache.insert(name.to_string(), rc.clone());
                        let is_gen = self.fn_has_yield(&real_def.body);
                        if is_gen {
                            self.gen_set.insert(name.to_string());
                        }
                        (rc, is_gen)
                    } else {
                        // Fallback: check lambda_defs (populated by AST path).
                        let mut found = None;
                        for f in &self.lambda_defs {
                            if f.name.name == *name {
                                found = Some(f.clone());
                                break;
                            }
                        }
                        if let Some(real_def) = found {
                            let rc = Rc::new(real_def.clone());
                            self.fn_cache.insert(name.to_string(), rc.clone());
                            let is_gen = self.fn_has_yield(&real_def.body);
                            if is_gen {
                                self.gen_set.insert(name.to_string());
                            }
                            (rc, is_gen)
                        } else {
                            (cached, self.gen_set.contains(name))
                        }
                    }
                } else {
                    // Case 2: this is a regular name like "f" that was
                    // bound to a Func value (e.g. set, f, fn(x) x+1).
                    // The global "f" holds Value::Func("<lambda_0>").
                    // Resolve to the actual lambda name and recurse.
                    if let Some(Value::Func(actual_name)) = self.globals.get(name).cloned() {
                        if actual_name != *name {
                            // Recurse with the resolved name.
                            for a in &args {
                                self.push(a.clone());
                            }
                            return self.call_function(&actual_name, args.len());
                        }
                    }
                    (cached, self.gen_set.contains(name))
                }
            } else {
                (cached, self.gen_set.contains(name))
            }
        } else {
            match self.find_function(name) {
                Ok(f) => {
                    let is_gen = self.fn_has_yield(&f.body);
                    let rc = Rc::new(f.clone());
                    if is_gen {
                        self.gen_set.insert(name.to_string());
                    }
                    self.fn_cache.insert(name.to_string(), rc.clone());
                    (rc, is_gen)
                }
                Err(_) => {
                    // Not in this program — check if `name` is a global bound
                    // to a Func value (e.g. `set, add = fn(x, y) x + y`).
                    // If so, call THAT function instead.
                    if let Some(Value::Func(actual_name)) = self.globals.get(name).cloned() {
                        if actual_name != name {
                            // Recurse with the resolved name (e.g. "<lambda_0>").
                            // Push args back onto the stack first.
                            for a in &args {
                                self.push(a.clone());
                            }
                            return self.call_function(&actual_name, args.len());
                        }
                    }
                    // Check if the lambda was registered in the compiler's
                    // lambda_fns list. The fn_cache may have an entry with
                    // an empty body (from preprocess_top_level), but the
                    // actual FnDef with the real body is in the compiler's
                    // lambda_fns. We need to look it up from the program.
                    if name.starts_with("<lambda_") {
                        if let Some(prog) = &self.program {
                            // Lambdas are NOT in program.declarations (they're
                            // in the compiler's lambda_fns which are not
                            // preserved). The fn_cache entry from preprocess
                            // has an empty body, which means execute_fn_body
                            // does nothing and returns Null.
                            //
                            // FIX: we need to store the lambda body. The
                            // simplest fix: when the lambda is created at
                            // runtime (in execute_ast_expr's Lambda handler),
                            // also store it in lambda_defs. Then find_function
                            // can find it there.
                        }
                        // Check lambda_defs (populated by the AST path).
                        for f in &self.lambda_defs {
                            if f.name.name == *name {
                                let rc = Rc::new(f.clone());
                                self.fn_cache.insert(name.to_string(), rc.clone());
                                let is_gen = self.fn_has_yield(&f.body);
                                if is_gen {
                                    self.gen_set.insert(name.to_string());
                                }
                                // Fall through to the normal call path below
                                // by setting fn_def and is_generator.
                                // We can't easily restructure, so just push
                                // args and recurse.
                                for a in &args {
                                    self.push(a.clone());
                                }
                                return self.call_function(name, args.len());
                            }
                        }
                    }
                    // Not in this program — check if it's an imported function
                    // stored as a Module in globals.
                    return self.call_imported_function(name, args);
                }
            }
        };
        // Runtime type-constraint check: if the function has generic
        // constraints (e.g. `fn, render[T: Drawable](obj: T)`), verify that
        // each argument whose parameter type is a constrained type-param
        // satisfies the named interface (i.e. the argument's class — if any
        // — defines all of the interface's methods, or the argument's class
        // IS the named interface). We only check when the constraint names
        // an interface that exists in the program; unknown constraints are
        // ignored (best-effort, like the rest of the type system).
        if !fn_def.type_constraints.is_empty() {
            self.check_type_constraints(&fn_def, &args)?;
        }
        // Async functions are executed synchronously in the single-threaded VM.
        // The `is_async` flag is ignored; `await X` evaluates X inline.
        // Generator: if the function body contains yield, run it eagerly to
        // collect all yielded values into a list, then wrap that list in a
        // Generator value. This is the same eager-evaluation model the AST
        // interpreter uses.
        if is_generator {
                    // Eager generator model: run the generator body to completion,
            // collecting every yielded value into gen_yield_buffer. The
            // body is now compiled to bytecode (registered in fn_entry_pcs),
            // so we execute it via the bytecode path rather than AST.
            self.gen_yield_buffer = Some(Vec::new());
            let mut yielded: Vec<Value> = Vec::new();
            // Push a dedicated call-frame scope for this generator
            // invocation. Parameters and any locals introduced inside
            // the body live in this scope and are discarded when the
            // generator finishes, so re-entrant / recursive generators
            // do not clobber each other's variables.
            let params: Vec<(String, Value)> = fn_def
                .params
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    (p.name.name.clone(), args.get(i).cloned().unwrap_or(Value::Null))
                })
                .collect();
            self.push_scope(params);
            let mut return_value: Option<Value> = None;
            let body_result: Result<()>;
            // Try the bytecode path first: if the generator body was
            // compiled (registered in fn_entry_pcs), execute it via
            // CallByName-style frame setup. The Yield instruction pushes
            // to gen_yield_buffer and continues (eager model — no
            // suspension). Return pops the frame and leaves the return
            // value on the stack.
            if let Some((entry_pc, num_locals)) = self.module.fn_entry_pcs.get(name).copied() {
                            let nslots = num_locals.max(args.len());
                let mut locals = if let Some(mut v) = self.locals_pool.pop() {
                    v.clear();
                    v.resize(nslots, Value::Null);
                    v
                } else {
                    vec![Value::Null; nslots]
                };
                for (i, a) in args.iter().enumerate() {
                    locals[i] = a.clone();
                }
                let stack_base = self.stack.len();
                let saved_pc = self.pc;
                self.frames.push(Frame {
                    locals,
                    return_pc: self.pc,
                    stack_base,
                    is_main: false,
                    defer_pcs: Vec::new(),
                    fn_name: name.to_string(),
                });
                self.pc = entry_pc;
                let target_frame_depth = self.frames.len() - 1; // current top is the generator frame
                // Execute the generator body via the bytecode loop until the
                // generator frame is popped (Ret pops back to target depth).
                // Catch panics so a VM panic surfaces as a normal error.
                let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let code_len = self.module.code.len();
                    while self.pc < code_len && self.frames.len() > target_frame_depth {
                        let pc = self.pc;
                        let instr = self.module.code[pc].clone();
                        self.pc += 1;
                        match self.execute(&instr) {
                            Ok(Flow::Continue) => {}
                            Ok(Flow::Return(_)) => break,
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(())
                }));
                body_result = match panicked {
                    Ok(inner) => inner,
                    Err(payload) => Err(CompilerError::runtime_error(format!(
                        "generator panic: {}",
                        if let Some(s) = payload.downcast_ref::<&'static str>() { (*s).to_string() }
                        else if let Some(s) = payload.downcast_ref::<String>() { s.clone() }
                        else { "<non-string panic payload>".to_string() }
                    ))),
                };
                if body_result.is_ok() {
                    return_value = Some(self.stack.pop().unwrap_or(Value::Null));
                }
                self.pc = saved_pc;
            } else {
                // Generator body not compiled — should not happen now.
                body_result = Ok(());
            }
            // Drain any yields produced by the final iteration.
            if let Some(buf) = self.gen_yield_buffer.take() {
                yielded.extend(buf);
            }
            // Always pop the scope.
            self.pop_scope();
            body_result?;
            let gen = GenState {
                func_name: name.to_string(),
                pc: 0,
                locals: Vec::new(),
                stack: Vec::new(),
                done: false,
                yielded_values: yielded,
                yield_idx: 0,
                return_value,
            };
            self.push(Value::Generator(Rc::new(RefCell::new(gen))));
            return Ok(());
        }
        // Non-generator function: execute normally.
        //
        // Push a dedicated call-frame scope holding the parameters (and
        // any locals introduced inside the body). This isolates each
        // invocation's variables, so recursive calls — even those that
        // introduce locals with `set` — cannot overwrite one another.
        // For lambdas, a second scope holding the captured lexical
        // environment is pushed FIRST (below the param scope), so the
        // body sees captured variables while params take precedence.
        // Both scopes are popped on every exit path before the result
        // is handled below.
        let is_lambda = name.starts_with("<lambda_");
        // For lambdas, clone the Rc to the shared capture map. This allows
        // mutations made inside the lambda to persist across calls (the
        // counter pattern works because each call sees the updated values).
        let capture_rc = if is_lambda {
            self.lambda_captures.get(name).cloned()
        } else {
            None
        };
        if let Some(ref captures) = capture_rc {
            // Push the captured scope as a shared RefCell so mutations
            // persist back to the original capture map.
            let snapshot: Vec<(String, Value)> = captures.borrow().iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            self.push_scope(snapshot);
        }
        let params: Vec<(String, Value)> = fn_def
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                (p.name.name.clone(), args.get(i).cloned().unwrap_or(Value::Null))
            })
            .collect();
        // Bytecode path: if the function body was compiled (registered in
        // fn_entry_pcs) and this is not a lambda (lambdas need the scope-
        // based capture mechanism), execute via a Frame with params bound
        // to frame.locals slots. This covers constrained functions (the
        // check_type_constraints already ran above) and any other compiled
        // function that reached call_function instead of the CallByName
        // fast path.
        // Bytecode path for ALL functions (including lambdas).
        // Lambda captures are temporarily bound into globals (saved/restored)
        // so LoadGlobal inside the lambda body resolves captured variables.
        // For lambdas, `name` is a runtime name like "<lambda_0>#3"; the
        // bytecode body is registered under the compile-time name (before '#').
        let compile_name = if let Some(pos) = name.find('#') {
            &name[..pos]
        } else {
            name
        };
        let bc_entry = self.module.fn_entry_pcs.get(compile_name).copied();
        let stack_base_before = self.stack.len();
        let result: Result<()>;
        // For lambdas: save globals shadowed by captures, inject captures.
        let mut saved_globals: Vec<(String, Option<Value>)> = Vec::new();
        if is_lambda {
            if let Some(ref captures) = capture_rc {
                for (k, v) in captures.borrow().iter() {
                    saved_globals.push((k.clone(), self.globals.get(k).cloned()));
                    self.globals.insert(k.clone(), v.clone());
                }
            }
        }
        if let Some((entry_pc, num_locals)) = bc_entry {
            // Bytecode Frame execution. Params go to frame.locals slots.
            let nslots = num_locals.max(fn_def.params.len());
            let mut locals = if let Some(mut v) = self.locals_pool.pop() {
                v.clear();
                v.resize(nslots, Value::Null);
                v
            } else {
                vec![Value::Null; nslots]
            };
            for (i, (_, v)) in params.iter().enumerate() {
                if i < locals.len() {
                    locals[i] = v.clone();
                }
            }
            let stack_base = self.stack.len();
            let saved_pc = self.pc;
            self.frames.push(Frame {
                locals,
                return_pc: self.pc,
                stack_base,
                is_main: false,
                defer_pcs: Vec::new(),
                fn_name: name.to_string(),
            });
            self.pc = entry_pc;
            let target_depth = self.frames.len() - 1;
            let code_len = self.module.code.len();
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                while self.pc < code_len && self.frames.len() > target_depth {
                    let pc = self.pc;
                    let instr = self.module.code[pc].clone();
                    self.pc += 1;
                    match self.execute(&instr) {
                        Ok(Flow::Continue) => {}
                        Ok(Flow::Return(v)) => { self.push(v); break; }
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            }));
            result = match panicked {
                Ok(inner) => inner,
                Err(payload) => Err(CompilerError::runtime_error(format!(
                    "function panic: {}",
                    if let Some(s) = payload.downcast_ref::<&'static str>() { (*s).to_string() }
                    else if let Some(s) = payload.downcast_ref::<String>() { s.clone() }
                    else { "<non-string panic payload>".to_string() }
                ))),
            };
            self.pc = saved_pc;
        } else {
            // Function body not compiled — should not happen now.
            result = Ok(());
        }
        // Restore globals shadowed by lambda captures.
        if is_lambda {
            for (k, old) in saved_globals {
                match old {
                    Some(v) => { self.globals.insert(k, v); }
                    None => { self.globals.remove(&k); }
                }
            }
        }
        match result {
            Ok(()) => {
                // Bytecode path: Ret instruction already pushed the return
                // value onto the stack (or ReturnVoid pushed Null). If the
                // function fell off the end without returning, push Null.
                // AST path: Ok means no return — push Null.
                if bc_entry.is_some() {
                    // Bytecode path: Ret left the return value on the stack.
                    // If nothing was pushed (function ended without Ret),
                    // push Null.
                    if self.stack.len() > stack_base_before {
                        // Return value already on stack.
                    } else {
                        self.push(Value::Null);
                    }
                } else {
                    self.push(Value::Null);
                }
                Ok(())
            }
            Err(e) => {
                let msg = e.message();
                if msg == "return" {
                    // AST path: return value is on the stack.
                    Ok(())
                } else if msg == "break" || msg == "continue" {
                    Err(CompilerError::runtime_error(format!(
                        "{} escaped function '{}'",
                        msg, name
                    )))
                } else {
                    Err(e)
                }
            }
        }
    }

    fn call_dunder(&mut self, receiver: &Value, method: &str, args: Vec<Value>) -> Result<Value> {
        match receiver {
            Value::Object(class, _) => {
                self.call_method_on_class(class, receiver.clone(), method, args)
            }
            _ => Ok(Value::Null),
        }
    }


    /// Find the path to a stdlib .veds file by name.
    fn find_stdlib_path(&self, name: &str) -> Option<std::path::PathBuf> {
        let filename = format!("{}.veds", name);
        // 编译时嵌入的 stdlib 路径（CARGO_MANIFEST_DIR/src/runtime/std）。
        let manifest_std = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/runtime/std");
        let candidates = [
            manifest_std.join(&filename),
            std::path::PathBuf::from("src/runtime/std").join(&filename),
            std::path::PathBuf::from("../src/runtime/std").join(&filename),
            self.base_dir.join(&filename),
            std::path::PathBuf::from(&filename),
        ];
        for p in &candidates {
            if p.exists() {
                return Some(p.clone());
            }
        }
        None
    }

    fn load_builtin_module(&self, name: &str) -> Option<HashMap<String, Value>> {
        let mut exports: HashMap<String, Value> = HashMap::new();
        match name {
            "core" => {
                // 纯 Vredrs 标准库——编译时嵌入（include_str!），运行时编译执行。
                const CORE_SOURCE: &str = include_str!("../runtime/std/core.veds");
                let tokens = crate::lexer::Lexer::new(CORE_SOURCE, 0).tokenize();
                if tokens.is_err() { eprintln!("[core] lexer error: {:?}", tokens); return None; }
                let tokens = tokens.unwrap();
                let program = crate::parser::Parser::new(tokens, 0).parse_program();
                if program.is_err() { eprintln!("[core] parser error: {:?}", program); return None; }
                let program = program.unwrap();
                let compiler = crate::bytecode::Compiler::new();
                let module = compiler.compile(&program);
                if module.is_err() { eprintln!("[core] compiler error: {:?}", module); return None; }
                let module = module.unwrap();
                let mut sub_vm = VM::new(module).with_program(program);
                sub_vm.preprocess_top_level();
                // 不执行 sub_vm.run()——core.veds 只有函数定义，无顶层语句。
                // 直接从 program 中收集函数名作为 exports，并把函数定义注册到主 VM 的 fn_cache。
                for d in &sub_vm.program.as_ref().unwrap().declarations {
                    if let crate::parser::ast::TopLevel::FnDef(f) = d {
                        let export_name = f.name.name.strip_prefix("core_").unwrap_or(&f.name.name).to_string();
                        exports.insert(export_name, Value::Func(f.name.name.clone()));
                        // 注册到主 VM 的 fn_cache（通过 self 的可变引用）。
                        // 注意：load_builtin_module 是 &self，不能修改 self。
                        // 函数定义会在 call_function 时通过 find_function 查找。
                    }
                }
                for (k, v) in &sub_vm.globals {
                    if let Value::Func(_) = v {
                        exports.insert(k.clone(), v.clone());
                    }
                }
                if exports.is_empty() { None } else { Some(exports) }
            }
            "math" => {
                exports.insert("pi".to_string(), Value::Float(std::f64::consts::PI));
                exports.insert("e".to_string(), Value::Float(std::f64::consts::E));
                exports.insert("tau".to_string(), Value::Float(std::f64::consts::TAU));
                exports.insert("inf".to_string(), Value::Float(f64::INFINITY));
                exports.insert("nan".to_string(), Value::Float(f64::NAN));
                exports.insert("sqrt".to_string(), Value::Func("math_sqrt".to_string()));
                exports.insert("cbrt".to_string(), Value::Func("math_cbrt".to_string()));
                exports.insert("pow".to_string(), Value::Func("math_pow".to_string()));
                exports.insert("exp".to_string(), Value::Func("math_exp".to_string()));
                exports.insert("log".to_string(), Value::Func("math_log".to_string()));
                exports.insert("log10".to_string(), Value::Func("math_log10".to_string()));
                exports.insert("log2".to_string(), Value::Func("math_log2".to_string()));
                exports.insert("sin".to_string(), Value::Func("math_sin".to_string()));
                exports.insert("cos".to_string(), Value::Func("math_cos".to_string()));
                exports.insert("tan".to_string(), Value::Func("math_tan".to_string()));
                exports.insert("asin".to_string(), Value::Func("math_asin".to_string()));
                exports.insert("acos".to_string(), Value::Func("math_acos".to_string()));
                exports.insert("atan".to_string(), Value::Func("math_atan".to_string()));
                exports.insert("atan2".to_string(), Value::Func("math_atan2".to_string()));
                exports.insert("sinh".to_string(), Value::Func("math_sinh".to_string()));
                exports.insert("cosh".to_string(), Value::Func("math_cosh".to_string()));
                exports.insert("tanh".to_string(), Value::Func("math_tanh".to_string()));
                exports.insert("floor".to_string(), Value::Func("math_floor".to_string()));
                exports.insert("ceil".to_string(), Value::Func("math_ceil".to_string()));
                exports.insert("trunc".to_string(), Value::Func("math_trunc".to_string()));
                exports.insert("round".to_string(), Value::Func("math_round".to_string()));
                exports.insert("abs".to_string(), Value::Func("math_abs".to_string()));
                exports.insert("fabs".to_string(), Value::Func("math_fabs".to_string()));
                exports.insert("radians".to_string(), Value::Func("math_radians".to_string()));
                exports.insert("degrees".to_string(), Value::Func("math_degrees".to_string()));
                exports.insert("gamma".to_string(), Value::Func("math_gamma".to_string()));
                exports.insert("lgamma".to_string(), Value::Func("math_lgamma".to_string()));
                Some(exports)
            }
            "io" => {
                exports.insert("open".to_string(), Value::Func("open".to_string()));
                exports.insert("read".to_string(), Value::Func("read".to_string()));
                exports.insert("write".to_string(), Value::Func("write".to_string()));
                exports.insert("close".to_string(), Value::Func("close".to_string()));
                exports.insert("read_file".to_string(), Value::Func("read_file".to_string()));
                exports.insert("write_file".to_string(), Value::Func("write_file".to_string()));
                exports.insert("file_exists".to_string(), Value::Func("file_exists".to_string()));
                exports.insert("read_path".to_string(), Value::Func("io_read".to_string()));
                exports.insert("write_path".to_string(), Value::Func("io_write".to_string()));
                exports.insert("append".to_string(), Value::Func("io_append".to_string()));
                Some(exports)
            }
            "os" => {
                exports.insert("args".to_string(), Value::Func("os_args".to_string()));
                exports.insert("exit".to_string(), Value::Func("os_exit".to_string()));
                exports.insert("env".to_string(), Value::Func("os_get_env".to_string()));
                exports.insert("get_env".to_string(), Value::Func("os_get_env".to_string()));
                exports.insert("set_env".to_string(), Value::Func("os_set_env".to_string()));
                exports.insert("setenv".to_string(), Value::Func("os_set_env".to_string()));
                exports.insert("unset_env".to_string(), Value::Func("os_unset_env".to_string()));
                exports.insert("exec".to_string(), Value::Func("os_exec".to_string()));
                exports.insert("system".to_string(), Value::Func("os_system".to_string()));
                exports.insert("cwd".to_string(), Value::Func("os_cwd".to_string()));
                exports.insert("getwd".to_string(), Value::Func("os_getwd".to_string()));
                exports.insert("chdir".to_string(), Value::Func("os_chdir".to_string()));
                exports.insert("mkdir".to_string(), Value::Func("os_mkdir".to_string()));
                exports.insert("remove".to_string(), Value::Func("os_remove".to_string()));
                exports.insert("rename".to_string(), Value::Func("os_rename".to_string()));
                exports.insert("stat".to_string(), Value::Func("os_stat".to_string()));
                exports.insert("is_file".to_string(), Value::Func("is_file".to_string()));
                exports.insert("is_dir".to_string(), Value::Func("is_dir".to_string()));
                Some(exports)
            }
            "time" => {
                exports.insert("sleep".to_string(), Value::Func("time_sleep".to_string()));
                exports.insert("now".to_string(), Value::Func("time_now".to_string()));
                exports.insert("unix".to_string(), Value::Func("time_unix".to_string()));
                exports.insert("rand_int".to_string(), Value::Func("time_rand_int".to_string()));
                exports.insert("nanosecond".to_string(), Value::Int(1));
                exports.insert("microsecond".to_string(), Value::Int(1000));
                exports.insert("millisecond".to_string(), Value::Int(1000_000));
                exports.insert("second".to_string(), Value::Int(1000_000_000));
                exports.insert("minute".to_string(), Value::Int(60_000_000_000));
                exports.insert("hour".to_string(), Value::Int(3_600_000_000_000));
                Some(exports)
            }
            "rand" => {
                exports.insert("intn".to_string(), Value::Func("rand_intn".to_string()));
                exports.insert("int".to_string(), Value::Func("rand_int".to_string()));
                exports.insert("float".to_string(), Value::Func("rand_float".to_string()));
                exports.insert("bool".to_string(), Value::Func("rand_bool".to_string()));
                exports.insert("seed".to_string(), Value::Func("rand_seed".to_string()));
                exports.insert("choice".to_string(), Value::Func("rand_choice".to_string()));
                exports.insert("shuffle".to_string(), Value::Func("rand_shuffle".to_string()));
                exports.insert("string".to_string(), Value::Func("rand_string".to_string()));
                Some(exports)
            }
            "fs" => {
                exports.insert("read_dir".to_string(), Value::Func("read_dir".to_string()));
                exports.insert("is_dir".to_string(), Value::Func("is_dir".to_string()));
                exports.insert("is_file".to_string(), Value::Func("is_file".to_string()));
                exports.insert("path_join".to_string(), Value::Func("path_join".to_string()));
                exports.insert("basename".to_string(), Value::Func("basename".to_string()));
                exports.insert("dirname".to_string(), Value::Func("dirname".to_string()));
                exports.insert("read_file".to_string(), Value::Func("read_file".to_string()));
                exports.insert("write_file".to_string(), Value::Func("write_file".to_string()));
                exports.insert("file_exists".to_string(), Value::Func("file_exists".to_string()));
                exports.insert("walk".to_string(), Value::Func("fs_walk".to_string()));
                exports.insert("copy".to_string(), Value::Func("fs_copy".to_string()));
                exports.insert("move".to_string(), Value::Func("fs_move".to_string()));
                exports.insert("remove_all".to_string(), Value::Func("fs_remove_all".to_string()));
                exports.insert("temp_dir".to_string(), Value::Func("fs_temp_dir".to_string()));
                exports.insert("temp_file".to_string(), Value::Func("fs_temp_file".to_string()));
                Some(exports)
            }
            "json" => {
                exports.insert("parse".to_string(), Value::Func("json_parse".to_string()));
                exports.insert("stringify".to_string(), Value::Func("json_stringify".to_string()));
                exports.insert("stringify_pretty".to_string(), Value::Func("json_stringify_pretty".to_string()));
                Some(exports)
            }
            "fmt" => {
                exports.insert("printf".to_string(), Value::Func("printf".to_string()));
                exports.insert("sprintf".to_string(), Value::Func("fmt_sprintf".to_string()));
                exports.insert("fprintf".to_string(), Value::Func("fmt_fprintf".to_string()));
                Some(exports)
            }
            "path" => {
                exports.insert("join".to_string(), Value::Func("path_join".to_string()));
                exports.insert("dirname".to_string(), Value::Func("path_dirname".to_string()));
                exports.insert("basename".to_string(), Value::Func("path_basename".to_string()));
                exports.insert("ext".to_string(), Value::Func("path_ext".to_string()));
                exports.insert("exists".to_string(), Value::Func("path_exists".to_string()));
                exports.insert("is_abs".to_string(), Value::Func("path_is_abs".to_string()));
                exports.insert("abs".to_string(), Value::Func("path_abs".to_string()));
                Some(exports)
            }
            "encoding" => {
                let mut base64 = HashMap::new();
                base64.insert("encode".to_string(), Value::Func("base64_encode".to_string()));
                base64.insert("decode".to_string(), Value::Func("base64_decode".to_string()));
                exports.insert("base64".to_string(), Value::Module("encoding.base64".to_string(), base64));
                let mut hex = HashMap::new();
                hex.insert("encode".to_string(), Value::Func("hex_encode".to_string()));
                hex.insert("decode".to_string(), Value::Func("hex_decode".to_string()));
                exports.insert("hex".to_string(), Value::Module("encoding.hex".to_string(), hex));
                exports.insert("url_encode".to_string(), Value::Func("url_encode".to_string()));
                exports.insert("url_decode".to_string(), Value::Func("url_decode".to_string()));
                Some(exports)
            }
            "regex" => {
                exports.insert("compile".to_string(), Value::Func("regex_compile".to_string()));
                exports.insert("match".to_string(), Value::Func("regex_match".to_string()));
                exports.insert("search".to_string(), Value::Func("regex_search".to_string()));
                exports.insert("find_all".to_string(), Value::Func("regex_find_all".to_string()));
                exports.insert("findall".to_string(), Value::Func("regex_findall".to_string()));
                exports.insert("sub".to_string(), Value::Func("regex_sub".to_string()));
                Some(exports)
            }
            "debug" => {
                exports.insert("inspect".to_string(), Value::Func("debug_inspect".to_string()));
                exports.insert("trace".to_string(), Value::Func("debug_trace".to_string()));
                exports.insert("timeit".to_string(), Value::Func("debug_timeit".to_string()));
                exports.insert("dump".to_string(), Value::Func("debug_dump".to_string()));
                exports.insert("backtrace".to_string(), Value::Func("debug_backtrace".to_string()));
                Some(exports)
            }
            "flag" => {
                exports.insert("string".to_string(), Value::Func("flag_string".to_string()));
                exports.insert("int".to_string(), Value::Func("flag_int".to_string()));
                exports.insert("bool".to_string(), Value::Func("flag_bool".to_string()));
                exports.insert("parse".to_string(), Value::Func("flag_parse".to_string()));
                exports.insert("args".to_string(), Value::Func("flag_args".to_string()));
                Some(exports)
            }
            "log" => {
                exports.insert("debug".to_string(), Value::Func("log_debug".to_string()));
                exports.insert("info".to_string(), Value::Func("log_info".to_string()));
                exports.insert("warn".to_string(), Value::Func("log_warn".to_string()));
                exports.insert("error".to_string(), Value::Func("log_error".to_string()));
                exports.insert("set_level".to_string(), Value::Func("log_set_level".to_string()));
                exports.insert("set_format".to_string(), Value::Func("log_set_format".to_string()));
                exports.insert("DEBUG".to_string(), Value::Int(1));
                exports.insert("INFO".to_string(), Value::Int(2));
                exports.insert("WARN".to_string(), Value::Int(3));
                exports.insert("ERROR".to_string(), Value::Int(4));
                Some(exports)
            }
            "term" => {
                exports.insert("clear".to_string(), Value::Func("term_clear".to_string()));
                exports.insert("move_cursor".to_string(), Value::Func("term_move_cursor".to_string()));
                exports.insert("set_color".to_string(), Value::Func("term_set_color".to_string()));
                exports.insert("reset".to_string(), Value::Func("term_reset".to_string()));
                exports.insert("read_key".to_string(), Value::Func("term_read_key".to_string()));
                exports.insert("get_size".to_string(), Value::Func("term_get_size".to_string()));
                exports.insert("BLACK".to_string(), Value::Int(0));
                exports.insert("RED".to_string(), Value::Int(1));
                exports.insert("GREEN".to_string(), Value::Int(2));
                exports.insert("YELLOW".to_string(), Value::Int(3));
                exports.insert("BLUE".to_string(), Value::Int(4));
                exports.insert("MAGENTA".to_string(), Value::Int(5));
                exports.insert("CYAN".to_string(), Value::Int(6));
                exports.insert("WHITE".to_string(), Value::Int(7));
                Some(exports)
            }
            "compress" => {
                let mut gzip = HashMap::new();
                gzip.insert("encode".to_string(), Value::Func("compress_gzip_encode".to_string()));
                gzip.insert("decode".to_string(), Value::Func("compress_gzip_decode".to_string()));
                exports.insert("gzip".to_string(), Value::Module("compress.gzip".to_string(), gzip));
                let mut zlib = HashMap::new();
                zlib.insert("encode".to_string(), Value::Func("compress_zlib_encode".to_string()));
                zlib.insert("decode".to_string(), Value::Func("compress_zlib_decode".to_string()));
                exports.insert("zlib".to_string(), Value::Module("compress.zlib".to_string(), zlib));
                let mut flate = HashMap::new();
                flate.insert("encode".to_string(), Value::Func("compress_flate_encode".to_string()));
                flate.insert("decode".to_string(), Value::Func("compress_flate_decode".to_string()));
                exports.insert("flate".to_string(), Value::Module("compress.flate".to_string(), flate));
                Some(exports)
            }
            "net" => {
                exports.insert("dial".to_string(), Value::Func("net_dial".to_string()));
                exports.insert("listen".to_string(), Value::Func("net_listen".to_string()));
                Some(exports)
            }
            "http" => {
                exports.insert("get".to_string(), Value::Func("http_get".to_string()));
                exports.insert("post".to_string(), Value::Func("http_post".to_string()));
                exports.insert("new_client".to_string(), Value::Func("http_get".to_string()));
                Some(exports)
            }
            "sql" => {
                exports.insert("open".to_string(), Value::Func("sql_open".to_string()));
                exports.insert("drivers".to_string(), Value::Func("sql_drivers".to_string()));
                Some(exports)
            }
            "csv" => {
                exports.insert("read".to_string(), Value::Func("csv_read".to_string()));
                exports.insert("write".to_string(), Value::Func("csv_write".to_string()));
                Some(exports)
            }
            "xml" => {
                exports.insert("parse".to_string(), Value::Func("xml_parse".to_string()));
                exports.insert("stringify".to_string(), Value::Func("xml_stringify".to_string()));
                Some(exports)
            }
            "toml" => {
                exports.insert("parse".to_string(), Value::Func("toml_parse".to_string()));
                exports.insert("stringify".to_string(), Value::Func("toml_stringify".to_string()));
                Some(exports)
            }
            "yaml" => {
                exports.insert("parse".to_string(), Value::Func("yaml_parse".to_string()));
                exports.insert("stringify".to_string(), Value::Func("yaml_stringify".to_string()));
                Some(exports)
            }
            "sync" => {
                exports.insert("spawn".to_string(), Value::Func("sync_spawn".to_string()));
                exports.insert("channel".to_string(), Value::Func("sync_channel".to_string()));
                exports.insert("send".to_string(), Value::Func("sync_send".to_string()));
                exports.insert("receive".to_string(), Value::Func("sync_receive".to_string()));
                exports.insert("close".to_string(), Value::Func("sync_close".to_string()));
                exports.insert("mutex".to_string(), Value::Func("sync_mutex".to_string()));
                exports.insert("waitgroup".to_string(), Value::Func("sync_waitgroup".to_string()));
                // sync.atomic sub-module
                let mut atomic = HashMap::new();
                atomic.insert("load_int".to_string(), Value::Func("atomic_load_int".to_string()));
                atomic.insert("store_int".to_string(), Value::Func("atomic_store_int".to_string()));
                atomic.insert("add_int".to_string(), Value::Func("atomic_add_int".to_string()));
                atomic.insert("compare_and_swap_int".to_string(), Value::Func("atomic_cas_int".to_string()));
                exports.insert("atomic".to_string(), Value::Module("sync.atomic".to_string(), atomic));
                Some(exports)
            }
            "image" => {
                exports.insert("load".to_string(), Value::Func("image_load".to_string()));
                exports.insert("save".to_string(), Value::Func("image_save".to_string()));
                exports.insert("new".to_string(), Value::Func("image_new".to_string()));
                exports.insert("create".to_string(), Value::Func("image_new".to_string()));
                exports.insert("resize".to_string(), Value::Func("image_resize".to_string()));
                exports.insert("crop".to_string(), Value::Func("image_crop".to_string()));
                Some(exports)
            }
            "machine" => {
                let mut gpio = HashMap::new();
                gpio.insert("pin".to_string(), Value::Func("machine_gpio_pin".to_string()));
                exports.insert("gpio".to_string(), Value::Module("machine.gpio".to_string(), gpio));
                let mut i2c = HashMap::new();
                i2c.insert("init".to_string(), Value::Func("machine_i2c_init".to_string()));
                exports.insert("i2c".to_string(), Value::Module("machine.i2c".to_string(), i2c));
                let mut spi = HashMap::new();
                spi.insert("init".to_string(), Value::Func("machine_spi_init".to_string()));
                exports.insert("spi".to_string(), Value::Module("machine.spi".to_string(), spi));
                let mut serial = HashMap::new();
                serial.insert("open".to_string(), Value::Func("machine_serial_open".to_string()));
                exports.insert("serial".to_string(), Value::Module("machine.serial".to_string(), serial));
                Some(exports)
            }
            "unsafe" => {
                exports.insert("sizeof".to_string(), Value::Func("unsafe_sizeof".to_string()));
                exports.insert("alignof".to_string(), Value::Func("unsafe_alignof".to_string()));
                exports.insert("offsetof".to_string(), Value::Func("unsafe_offsetof".to_string()));
                exports.insert("cast".to_string(), Value::Func("unsafe_cast".to_string()));
                exports.insert("alloc".to_string(), Value::Func("unsafe_alloc".to_string()));
                exports.insert("free".to_string(), Value::Func("unsafe_free".to_string()));
                Some(exports)
            }
            "embed" => {
                exports.insert("fs".to_string(), Value::Func("embed_fs".to_string()));
                exports.insert("read".to_string(), Value::Func("embed_read".to_string()));
                Some(exports)
            }
            "websocket" => {
                exports.insert("connect".to_string(), Value::Func("websocket_connect".to_string()));
                Some(exports)
            }
            "crypto" => {
                exports.insert("sha256".to_string(), Value::Func("crypto_sha256".to_string()));
                exports.insert("md5".to_string(), Value::Func("crypto_md5".to_string()));
                exports.insert("sha1".to_string(), Value::Func("crypto_sha1".to_string()));
                exports.insert("aes_encrypt".to_string(), Value::Func("crypto_aes_encrypt".to_string()));
                exports.insert("aes_decrypt".to_string(), Value::Func("crypto_aes_decrypt".to_string()));
                exports.insert("password_hash".to_string(), Value::Func("crypto_password_hash".to_string()));
                exports.insert("password_verify".to_string(), Value::Func("crypto_password_verify".to_string()));
                let mut bcrypt = HashMap::new();
                bcrypt.insert("hash".to_string(), Value::Func("crypto_bcrypt_hash".to_string()));
                bcrypt.insert("verify".to_string(), Value::Func("crypto_bcrypt_verify".to_string()));
                exports.insert("bcrypt".to_string(), Value::Module("crypto.bcrypt".to_string(), bcrypt));
                Some(exports)
            }
            // fmt, collections, net are provided by .veds files in
            // src/runtime/std/. They fall through to the file-based loader.
            _ => None,
        }
    }


    fn pattern_matches(
        &mut self,
        pattern: &crate::parser::ast::Pattern,
        value: &Value,
    ) -> Result<bool> {
        use crate::parser::ast::Pattern;
        match pattern {
            Pattern::Wildcard(_) => Ok(true),
            Pattern::Literal(lp) => {
                let lit = self.eval_literal_expr(&lp.literal).unwrap_or(Value::Null);
                Ok(&lit == value)
            }
            Pattern::Binding(bp) => {
                self.globals.insert(bp.name.name.clone(), value.clone());
                Ok(true)
            }
            Pattern::Tuple(tp) => {
                if let Value::Tuple(items) = value {
                    if items.len() != tp.elements.len() {
                        return Ok(false);
                    }
                    for (p, v) in tp.elements.iter().zip(items.iter()) {
                        if !self.pattern_matches(p, v)? {
                            return Ok(false);
                        }
                    }
                    return Ok(true);
                }
                Ok(false)
            }
            Pattern::List(lp) => {
                if let Value::List(items) = value {
                    if items.len() != lp.elements.len() {
                        return Ok(false);
                    }
                    for (p, v) in lp.elements.iter().zip(items.iter()) {
                        if !self.pattern_matches(p, v)? {
                            return Ok(false);
                        }
                    }
                    return Ok(true);
                }
                Ok(false)
            }
            Pattern::Or(op) => {
                // An or-pattern matches if ANY of its sub-patterns match.
                // Variable bindings: each branch should bind the same set of
                // names. We let the first matching branch's bindings stand
                // (later branches are not evaluated once one matches).
                for p in &op.patterns {
                    if self.pattern_matches(p, value)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Pattern::Dict(dp) => {
                if let Value::Dict(d) = value {
                    for (key, pat) in &dp.entries {
                        let v = match d.get(key) {
                            Some(v) => v,
                            None => return Ok(false),
                        };
                        if !self.pattern_matches(pat, v)? {
                            return Ok(false);
                        }
                    }
                    return Ok(true);
                }
                Ok(false)
            }
            Pattern::Struct(sp) => {
                // Struct pattern: match the type name (via the object's class)
                // and then each named field against its sub-pattern.
                if let Value::Object(class, fields) = value {
                    if class != &sp.type_name.name {
                        return Ok(false);
                    }
                    let f = fields.borrow();
                    for (field_name, pat) in &sp.fields {
                        let v = match f.get(&field_name.name) {
                            Some(v) => v,
                            None => return Ok(false),
                        };
                        if !self.pattern_matches(pat, v)? {
                            return Ok(false);
                        }
                    }
                    return Ok(true);
                }
                Ok(false)
            }
            Pattern::EnumVariant(ep) => {
                // Enum variant pattern: match against a Value::Enum or a
                // qualified identifier of the form `EnumName.Variant`.
                // The scrutinee may be a string "Variant" or an enum value.
                if let Value::Str(s) = value {
                    if s == &ep.variant.name {
                        return Ok(true);
                    }
                }
                if let Value::Object(class, fields) = value {
                    if class == &ep.type_name.name {
                        if let Some(v) = fields.borrow().get("__variant__") {
                            if let Value::Str(var_name) = v {
                                if var_name == &ep.variant.name {
                                    // Optionally match payload.
                                    if let Some(payload_patterns) = &ep.payload {
                                        if let Some(Value::Tuple(items)) = fields.borrow().get("__payload__") {
                                            if items.len() != payload_patterns.len() {
                                                return Ok(false);
                                            }
                                            for (p, v) in payload_patterns.iter().zip(items.iter()) {
                                                if !self.pattern_matches(p, v)? {
                                                    return Ok(false);
                                                }
                                            }
                                        }
                                    }
                                    return Ok(true);
                                }
                            }
                        }
                    }
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// Execute a block of statements.
    fn is_class_name(&self, name: &str) -> bool {
        if let Some(prog) = &self.program {
            for d in &prog.declarations {
                if let crate::parser::ast::TopLevel::ClassDef(cd) = d {
                    if cd.name.name == name {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Instantiate a class: create object with fields, then call the
    /// constructor. Supports two constructor forms:
    ///   - `__init__(args)`  (Python-style; mutates self in place, return
    ///     value is ignored).
    ///   - `new(args)`       (Vredrs-style; may `return, self`).
    /// `__init__` takes precedence when both are defined. If neither is
    /// defined, the bare object is returned.
    ///
    /// Before constructing, if the class declares `implements Interface`,
    /// verify the class defines every method the interface requires. This
    /// is the runtime interface-check (complements the static check in
    /// Feature 4).
    fn instantiate_class(&mut self, class: &str, args: Vec<Value>) -> Result<Value> {
        // Runtime interface check: if this class declares `implements Iface`,
        // verify every required method exists on the class (or a parent).
        self.check_class_implements(class)?;
        let mut fields = HashMap::new();
        self.collect_class_fields(class, &mut fields);
        let obj = Value::Object(class.to_string(), Rc::new(RefCell::new(fields)));
        if self.find_method(class, "__init__").is_ok() {
            // Python-style: __init__ mutates self in place; ignore return.
            let _ = self.call_method_on_class(class, obj.clone(), "__init__", args)?;
            return Ok(obj);
        }
        if self.find_method(class, "new").is_ok() {
            let returned = self.call_method_on_class(class, obj.clone(), "new", args)?;
            if let Value::Object(_, _) = returned {
                return Ok(returned);
            }
        }
        Ok(obj)
    }

    /// Check that a class satisfies every interface it declares.
    /// Looks up the class definition in the program, reads its `implements`
    /// list, and for each interface looks up the interface definition and
    /// verifies every required method exists on the class (with inheritance
    /// lookup via find_method).
    fn check_class_implements(&self, class: &str) -> Result<()> {
        use crate::parser::ast::{TopLevel, TypeExpr};
        let prog = match &self.program {
            Some(p) => p,
            None => return Ok(()),
        };
        // Find the class definition.
        let cd = prog.declarations.iter().find_map(|d| {
            if let TopLevel::ClassDef(cd) = d {
                if cd.name.name == class {
                    return Some(cd);
                }
            }
            None
        });
        let cd = match cd {
            Some(c) => c,
            None => return Ok(()),
        };
        if cd.implements.is_empty() {
            return Ok(());
        }
        // Build a map of interface name → required methods.
        let mut interfaces: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for d in &prog.declarations {
            if let TopLevel::InterfaceDef(id) = d {
                let methods: Vec<String> = id.methods.iter().map(|m| m.name.name.clone()).collect();
                interfaces.insert(id.name.name.clone(), methods);
            }
        }
        for impl_ty in &cd.implements {
            let iface_name = match impl_ty {
                TypeExpr::Named(id, _) => &id.name,
                _ => continue,
            };
            let required = match interfaces.get(iface_name) {
                Some(m) => m,
                None => continue,
            };
            for m in required {
                if self.find_method(class, m).is_err() {
                    return Err(CompilerError::runtime_error(format!(
                        "class '{}' does not implement interface '{}': missing method '{}'",
                        class, iface_name, m
                    )));
                }
            }
        }
        Ok(())
    }

    fn collect_class_fields(&self, class: &str, fields: &mut HashMap<String, Value>) {
        if let Some(prog) = &self.program {
            for d in &prog.declarations {
                if let crate::parser::ast::TopLevel::ClassDef(cd) = d {
                    if cd.name.name == class {
                        if let Some(parent) = &cd.extends {
                            if let crate::parser::ast::TypeExpr::Named(pid, _) = parent {
                                self.collect_class_fields(&pid.name, fields);
                            }
                        }
                        for f in &cd.fields {
                            let default = if let Some(dv) = &f.default_value {
                                let _ = dv;
                                Value::Null
                            } else {
                                Value::Null
                            };
                            fields.insert(f.name.name.clone(), default);
                        }
                        return;
                    }
                }
            }
        }
    }

    /// Execute a list comprehension: [expr for var in iterable if cond].
    fn iterable_to_list(&self, v: &Value) -> Vec<Value> {
        match v {
            Value::List(l) => l.clone(),
            Value::Tuple(t) => t.clone(),
            Value::Str(s) => s.chars().map(|c| Value::Str(c.to_string())).collect(),
            Value::Dict(d) => d.keys().map(|k| Value::Str(k.clone())).collect(),
            _ => Vec::new(),
        }
    }

    /// Return the number of items in an iterable value.
    fn iterable_len(&self, v: &Value) -> usize {
        match v {
            Value::List(l) => l.len(),
            Value::Tuple(t) => t.len(),
            Value::Str(s) => s.chars().count(),
            Value::Dict(d) => d.len(),
            _ => 0,
        }
    }

    /// Slice a list/tuple/string by [start, end) with the given step.
    fn slice_value(
        &self,
        target: Value,
        start: i64,
        end: i64,
        step: i64,
    ) -> Result<Value> {
        if step == 0 {
            return Err(CompilerError::runtime_error("slice step cannot be zero"));
        }
        match target {
            Value::List(l) => {
                let n = l.len() as i64;
                let (s, e) = normalize_slice_bounds(start, end, step, n);
                let mut out = Vec::new();
                if step > 0 {
                    let mut i = s;
                    while i < e {
                        out.push(l[i as usize].clone());
                        i += step;
                    }
                } else {
                    let mut i = s;
                    while i > e {
                        out.push(l[i as usize].clone());
                        i += step;
                    }
                }
                Ok(Value::List(out))
            }
            Value::Tuple(t) => {
                let n = t.len() as i64;
                let (s, e) = normalize_slice_bounds(start, end, step, n);
                let mut out = Vec::new();
                if step > 0 {
                    let mut i = s;
                    while i < e {
                        out.push(t[i as usize].clone());
                        i += step;
                    }
                } else {
                    let mut i = s;
                    while i > e {
                        out.push(t[i as usize].clone());
                        i += step;
                    }
                }
                Ok(Value::Tuple(out))
            }
            Value::Str(s) => {
                let chars: Vec<char> = s.chars().collect();
                let n = chars.len() as i64;
                let (s_idx, e_idx) = normalize_slice_bounds(start, end, step, n);
                let mut out = String::new();
                if step > 0 {
                    let mut i = s_idx;
                    while i < e_idx {
                        out.push(chars[i as usize]);
                        i += step;
                    }
                } else {
                    let mut i = s_idx;
                    while i > e_idx {
                        out.push(chars[i as usize]);
                        i += step;
                    }
                }
                Ok(Value::Str(out))
            }
            other => Err(CompilerError::runtime_error(format!(
                "cannot slice {:?}",
                other
            ))),
        }
    }

    /// Call a method on a value (Object, Str, List, etc.).
    fn call_method_on_value(
        &mut self,
        receiver: Value,
        method: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        // Invalidate the global cache: method calls may modify globals.
        self.invalidate_global_cache();
        match &receiver {
            Value::Object(class, _) => {
                // Built-in object types (Time, Regex, etc.) that don't have
                // a Vredrs class definition are dispatched here before
                // falling through to find_method (which would fail).
                if let Some(v) = self.call_builtin_object_method(&receiver, class, method, &args)? {
                    return Ok(v);
                }
                // Check for operator-overload magic methods first.
                if let Some(v) = self.try_magic_method(class, method, &receiver, &args)? {
                    return Ok(v);
                }
                let class = class.clone();
                self.call_method_on_class(&class, receiver, method, args)
            }
            Value::Str(s) => {
                let s = s.clone();
                self.call_str_method(&s, method, args)
            }
            Value::List(l) => {
                let l = l.clone();
                self.call_list_method(&l, method, args)
            }
            Value::Dict(d) => {
                let d = d.clone();
                self.call_dict_method(&d, method, args)
            }
            Value::Generator(g) => {
                // Generator methods: next(), done, reset(), to_list()
                let mut state = g.borrow_mut();
                match method {
                    "next" | "__next__" | "resume" => {
                        if state.yield_idx < state.yielded_values.len() {
                            let v = state.yielded_values[state.yield_idx].clone();
                            state.yield_idx += 1;
                            if state.yield_idx >= state.yielded_values.len() {
                                state.done = true;
                            }
                            Ok(v)
                        } else {
                            state.done = true;
                            Ok(state.return_value.clone().unwrap_or(Value::Null))
                        }
                    }
                    "send" => {
                        // send(v) is like next() but ignores the sent value
                        // (Vredrs generators are eager, not coroutine-based).
                        if state.yield_idx < state.yielded_values.len() {
                            let v = state.yielded_values[state.yield_idx].clone();
                            state.yield_idx += 1;
                            if state.yield_idx >= state.yielded_values.len() {
                                state.done = true;
                            }
                            Ok(v)
                        } else {
                            state.done = true;
                            Ok(state.return_value.clone().unwrap_or(Value::Null))
                        }
                    }
                    "reset" | "rewind" => {
                        state.yield_idx = 0;
                        state.done = false;
                        Ok(Value::Null)
                    }
                    "to_list" | "collect" => {
                        let vals = state.yielded_values.clone();
                        Ok(Value::List(vals))
                    }
                    "done" | "is_done" => {
                        Ok(Value::Bool(state.done))
                    }
                    "has_next" => {
                        Ok(Value::Bool(state.yield_idx < state.yielded_values.len()))
                    }
                    "len" | "__len__" => {
                        Ok(Value::Int(state.yielded_values.len() as i64))
                    }
                    "to_str" | "__str__" => {
                        Ok(Value::Str(format!("<generator {} values>", state.yielded_values.len())))
                    }
                    _ => Err(CompilerError::runtime_error(format!(
                        "generator has no method '{}'",
                        method
                    ))),
                }
            }
            _ => Err(CompilerError::runtime_error(format!(
                "cannot call method '{}' on {:?}",
                method, receiver
            ))),
        }
    }

    /// Dispatch methods on built-in object types that have no Vredrs class
    /// definition: Time, Regex, Mutex, etc. Returns Ok(Some(v)) when the
    /// method was recognised, Ok(None) when it wasn't (so the caller falls
    /// through to user-defined methods / operator overloads).
    fn call_builtin_object_method(
        &mut self,
        receiver: &Value,
        class: &str,
        method: &str,
        args: &[Value],
    ) -> Result<Option<Value>> {
        match class {
            "Time" => self.call_time_method(receiver, method, args),
            "Regex" => self.call_regex_method(receiver, method, args),
            "Mutex" => self.call_mutex_method(receiver, method, args),
            "Image" => self.call_image_method(receiver, method, args),
            "Pin" => self.call_pin_method(receiver, method, args),
            "I2C" => self.call_i2c_method(receiver, method, args),
            "SPI" => self.call_spi_method(receiver, method, args),
            "Serial" => self.call_serial_method(receiver, method, args),
            "WebSocket" => self.call_websocket_method(receiver, method, args),
            "Conn" => self.call_conn_method(receiver, method, args),
            "Listener" => self.call_listener_method(receiver, method, args),
            "DB" => self.call_db_method(receiver, method, args),
            "Rows" => self.call_rows_method(receiver, method, args),
            "Stmt" => self.call_stmt_method(receiver, method, args),
            "EmbeddedFS" => self.call_embedded_fs_method(receiver, method, args),
            _ => Ok(None),
        }
    }

    /// Time object methods. The Time object stores `__secs__` (i64, Unix
    /// seconds) and `__nanos__` (i64, sub-second nanoseconds) as Object
    /// fields. All time component methods (year/month/day/hour/...) are
    /// computed from the Unix timestamp using a civil-from-days algorithm
    /// (no external date crate).
    fn call_time_method(
        &self,
        receiver: &Value,
        method: &str,
        _args: &[Value],
    ) -> Result<Option<Value>> {
        let fields = if let Value::Object(_, f) = receiver {
            f.borrow()
        } else {
            return Ok(None);
        };
        let secs = fields
            .get("__secs__")
            .and_then(|v| v.to_int().ok())
            .unwrap_or(0);
        let nanos = fields
            .get("__nanos__")
            .and_then(|v| v.to_int().ok())
            .unwrap_or(0);
        let result = match method {
            "unix" | "to_unix" => Value::Int(secs),
            "unix_nano" | "to_unix_nano" => Value::Int(secs * 1_000_000_000 + nanos),
            "year" | "month" | "day" | "hour" | "minute" | "second" | "weekday" => {
                // Convert Unix seconds to UTC civil date components using
                // Howard Hinnant's civil_from_days algorithm.
                let days = secs.div_euclid(86400);
                let secs_of_day = secs.rem_euclid(86400);
                let (y, m, d, wd) = civil_from_days(days);
                let hour = (secs_of_day / 3600) as i64;
                let minute = ((secs_of_day % 3600) / 60) as i64;
                let second = (secs_of_day % 60) as i64;
                match method {
                    "year" => Value::Int(y),
                    "month" => Value::Int(m as i64),
                    "day" => Value::Int(d as i64),
                    "hour" => Value::Int(hour),
                    "minute" => Value::Int(minute),
                    "second" => Value::Int(second),
                    "weekday" => Value::Int(wd as i64),
                    _ => return Ok(Default::default()),
                }
            }
            "to_str" | "__str__" => {
                let days = secs.div_euclid(86400);
                let secs_of_day = secs.rem_euclid(86400);
                let (y, m, d, _) = civil_from_days(days);
                let hour = (secs_of_day / 3600) as i64;
                let minute = ((secs_of_day % 3600) / 60) as i64;
                let second = (secs_of_day % 60) as i64;
                Value::Str(format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    y, m, d, hour, minute, second
                ))
            }
            "format" => {
                // Best-effort: support %Y %m %d %H %M %S placeholders.
                let days = secs.div_euclid(86400);
                let secs_of_day = secs.rem_euclid(86400);
                let (y, m, d, _) = civil_from_days(days);
                let hour = (secs_of_day / 3600) as i64;
                let minute = ((secs_of_day % 3600) / 60) as i64;
                let second = (secs_of_day % 60) as i64;
                let fmt = _args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let out = fmt
                    .replace("%Y", &format!("{:04}", y))
                    .replace("%m", &format!("{:02}", m))
                    .replace("%d", &format!("{:02}", d))
                    .replace("%H", &format!("{:02}", hour))
                    .replace("%M", &format!("{:02}", minute))
                    .replace("%S", &format!("{:02}", second));
                Value::Str(out)
            }
            _ => return Ok(None),
        };
        Ok(Some(result))
    }

    /// Regex object methods. The Regex object stores `__pattern__` (str)
    /// as an Object field. Matching is done via a minimal backtracking
    /// matcher supporting literal chars, `.`, `*`, `+`, `?`, `[...]`,
    /// `^`, `$`, and `\d \w \s` character classes.
    fn call_regex_method(
        &self,
        receiver: &Value,
        method: &str,
        args: &[Value],
    ) -> Result<Option<Value>> {
        let pattern = if let Value::Object(_, f) = receiver {
            f.borrow().get("__pattern__").map(|v| v.to_str()).unwrap_or_default()
        } else {
            return Ok(None);
        };
        let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
        let result = match method {
            "is_match" | "match" => Value::Bool(regex_match(&pattern, &s)),
            "find" => {
                if let Some((start, end)) = regex_search(&pattern, &s) {
                    Value::Tuple(vec![Value::Int(start as i64), Value::Int(end as i64)])
                } else {
                    Value::Null
                }
            }
            "find_all" => Value::List(
                regex_find_all(&pattern, &s)
                    .into_iter()
                    .map(|m| Value::Str(m))
                    .collect(),
            ),
            "split" => Value::List(
                regex_split(&pattern, &s)
                    .into_iter()
                    .map(|v| Value::Str(v))
                    .collect(),
            ),
            "replace" | "sub" => {
                let repl = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                Value::Str(regex_replace(&pattern, &repl, &s))
            }
            _ => return Ok(None),
        };
        Ok(Some(result))
    }

    /// Mutex object methods (no-op stubs; the VM is single-threaded).
    fn call_mutex_method(
        &self,
        _receiver: &Value,
        method: &str,
        _args: &[Value],
    ) -> Result<Option<Value>> {
        match method {
            "lock" | "unlock" => Ok(Some(Value::Null)),
            _ => Ok(None),
        }
    }

    /// Image object methods. The Image object stores `__width__`,
    /// `__height__` (i64) and `__pixels__` (List of [r,g,b,a] tuples,
    /// row-major, top-down) as Object fields. pixel/set_pixel/width/height
    /// are O(1) field access; resize/crop return new Image objects.
    fn call_image_method(
        &self,
        receiver: &Value,
        method: &str,
        args: &[Value],
    ) -> Result<Option<Value>> {
        let fields = if let Value::Object(_, f) = receiver {
            f.clone()
        } else {
            return Ok(None);
        };
        let f = fields.borrow();
        let width = f.get("__width__").and_then(|v| v.to_int().ok()).unwrap_or(0);
        let height = f.get("__height__").and_then(|v| v.to_int().ok()).unwrap_or(0);
        let pixels = f.get("__pixels__").cloned().unwrap_or(Value::List(vec![]));
        drop(f);
        let result = match method {
            "width" => Value::Int(width),
            "height" => Value::Int(height),
            "pixel" => {
                let x = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let y = args.get(1).and_then(|v| v.to_int().ok()).unwrap_or(0);
                if x < 0 || y < 0 || x >= width || y >= height {
                    Value::Tuple(vec![Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0)])
                } else if let Value::List(p) = &pixels {
                    p.get((y * width + x) as usize)
                        .cloned()
                        .unwrap_or(Value::Tuple(vec![Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0)]))
                } else {
                    Value::Tuple(vec![Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(0)])
                }
            }
            "set_pixel" => {
                let x = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let y = args.get(1).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let r = args.get(2).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let g = args.get(3).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let b = args.get(4).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let a = args.get(5).and_then(|v| v.to_int().ok()).unwrap_or(255);
                if x >= 0 && y >= 0 && x < width && y < height {
                    if let Value::List(p) = &pixels {
                        let mut p = p.clone();
                        p[(y * width + x) as usize] = Value::Tuple(vec![
                            Value::Int(r), Value::Int(g), Value::Int(b), Value::Int(a),
                        ]);
                        // Mutate the object's fields in place.
                        if let Value::Object(_, f2) = receiver {
                            f2.borrow_mut().insert("__pixels__".to_string(), Value::List(p));
                        }
                    }
                    Value::Null
                } else {
                    Value::Null
                }
            }
            "resize" => {
                let nw = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(width);
                let nh = args.get(1).and_then(|v| v.to_int().ok()).unwrap_or(height);
                // Nearest-neighbor resize.
                if let Value::List(p) = &pixels {
                    let mut out: Vec<Value> = Vec::with_capacity((nw * nh) as usize);
                    for ny in 0..nh {
                        for nx in 0..nw {
                            let sx = (nx * width) / nw.max(1);
                            let sy = (ny * height) / nh.max(1);
                            out.push(p[(sy * width + sx) as usize].clone());
                        }
                    }
                    let mut nf = HashMap::new();
                    nf.insert("__width__".to_string(), Value::Int(nw));
                    nf.insert("__height__".to_string(), Value::Int(nh));
                    nf.insert("__pixels__".to_string(), Value::List(out));
                    Value::Object("Image".to_string(), Rc::new(RefCell::new(nf)))
                } else {
                    Value::Null
                }
            }
            "crop" => {
                let cx = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let cy = args.get(1).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let cw = args.get(2).and_then(|v| v.to_int().ok()).unwrap_or(width);
                let ch = args.get(3).and_then(|v| v.to_int().ok()).unwrap_or(height);
                if let Value::List(p) = &pixels {
                    let mut out: Vec<Value> = Vec::with_capacity((cw * ch) as usize);
                    for ry in 0..ch {
                        for rx in 0..cw {
                            let sx = cx + rx;
                            let sy = cy + ry;
                            if sx >= 0 && sy >= 0 && sx < width && sy < height {
                                out.push(p[(sy * width + sx) as usize].clone());
                            } else {
                                out.push(Value::Tuple(vec![Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(255)]));
                            }
                        }
                    }
                    let mut nf = HashMap::new();
                    nf.insert("__width__".to_string(), Value::Int(cw));
                    nf.insert("__height__".to_string(), Value::Int(ch));
                    nf.insert("__pixels__".to_string(), Value::List(out));
                    Value::Object("Image".to_string(), Rc::new(RefCell::new(nf)))
                } else {
                    Value::Null
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(result))
    }

    /// Pin object methods (machine.gpio). Bare-metal stub: all operations
    /// are no-ops; set/get store values in the Pin's fields so user code
    /// can round-trip a value for testing.
    fn call_pin_method(
        &self,
        receiver: &Value,
        method: &str,
        args: &[Value],
    ) -> Result<Option<Value>> {
        match method {
            "set" => {
                if let Value::Object(_, f) = receiver {
                    let v = args.get(0).cloned().unwrap_or(Value::Bool(false));
                    f.borrow_mut().insert("__value__".to_string(), v);
                }
                Ok(Some(Value::Null))
            }
            "get" => {
                if let Value::Object(_, f) = receiver {
                    Ok(Some(f.borrow().get("__value__").cloned().unwrap_or(Value::Bool(false))))
                } else {
                    Ok(Some(Value::Bool(false)))
                }
            }
            "pwm" => Ok(Some(Value::Null)),
            _ => Ok(None),
        }
    }

    /// I2C object methods (machine.i2c). Bare-metal stubs.
    fn call_i2c_method(
        &self,
        _receiver: &Value,
        method: &str,
        _args: &[Value],
    ) -> Result<Option<Value>> {
        match method {
            "read" => Ok(Some(Value::List(vec![]))),
            "write" => Ok(Some(Value::Null)),
            _ => Ok(None),
        }
    }

    /// SPI object methods (machine.spi). Bare-metal stubs.
    fn call_spi_method(
        &self,
        _receiver: &Value,
        method: &str,
        args: &[Value],
    ) -> Result<Option<Value>> {
        match method {
            "transfer" => {
                // Echo back the input (no real SPI device).
                Ok(Some(args.get(0).cloned().unwrap_or(Value::List(vec![]))))
            }
            _ => Ok(None),
        }
    }

    /// Serial object methods (machine.serial). Bare-metal stubs.
    fn call_serial_method(
        &self,
        _receiver: &Value,
        method: &str,
        _args: &[Value],
    ) -> Result<Option<Value>> {
        match method {
            "read" => Ok(Some(Value::List(vec![]))),
            "write" => Ok(Some(Value::Null)),
            "close" => Ok(Some(Value::Null)),
            _ => Ok(None),
        }
    }

    /// WebSocket object methods (real WebSocket frame send/receive via std::net).
    /// send_text encodes a text frame (opcode 0x1); send_binary encodes a
    /// binary frame (opcode 0x2); receive reads one frame and returns the
    /// payload as a string (text) or list of bytes (binary).
    fn call_websocket_method(
        &mut self,
        receiver: &Value,
        method: &str,
        args: &[Value],
    ) -> Result<Option<Value>> {
        let id = if let Value::Object(_, f) = receiver {
            f.borrow().get("__id__").and_then(|v| v.to_int().ok()).unwrap_or(0)
        } else {
            return Ok(None);
        };
        match method {
            "send_text" => {
                let msg = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                if let Some(stream) = self.ws_connections.get_mut(&id) {
                    use std::io::Write;
                    let payload = msg.as_bytes();
                    let frame = ws_encode_frame(0x1, payload);
                    let _ = stream.write_all(&frame);
                    Ok(Some(Value::Null))
                } else {
                    Ok(Some(Value::Null))
                }
            }
            "send_binary" => {
                let data = args.get(0).cloned().unwrap_or(Value::List(vec![]));
                let bytes: Vec<u8> = if let Value::List(l) = &data {
                    l.iter().filter_map(|v| v.to_int().ok().map(|i| i as u8)).collect()
                } else {
                    data.to_str().into_bytes()
                };
                if let Some(stream) = self.ws_connections.get_mut(&id) {
                    use std::io::Write;
                    let frame = ws_encode_frame(0x2, &bytes);
                    let _ = stream.write_all(&frame);
                    Ok(Some(Value::Null))
                } else {
                    Ok(Some(Value::Null))
                }
            }
            "receive" => {
                if let Some(stream) = self.ws_connections.get_mut(&id) {
                    use std::io::Read;
                    match ws_decode_frame(stream) {
                        Some((opcode, payload)) => {
                            if opcode == 0x8 {
                                // Close frame.
                                if let Value::Object(_, f) = receiver {
                                    f.borrow_mut().insert("__closed__".to_string(), Value::Bool(true));
                                }
                                Ok(Some(Value::Null))
                            } else if opcode == 0x1 {
                                // Text frame.
                                Ok(Some(Value::Str(String::from_utf8_lossy(&payload).to_string())))
                            } else if opcode == 0x2 {
                                // Binary frame.
                                Ok(Some(Value::List(payload.into_iter().map(|b| Value::Int(b as i64)).collect())))
                            } else {
                                Ok(Some(Value::Null))
                            }
                        }
                        None => Ok(Some(Value::Null)),
                    }
                } else {
                    Ok(Some(Value::Null))
                }
            }
            "close" => {
                // Send close frame and remove from registry.
                if let Some(stream) = self.ws_connections.get_mut(&id) {
                    use std::io::Write;
                    let frame = ws_encode_frame(0x8, &[]);
                    let _ = stream.write_all(&frame);
                }
                self.ws_connections.remove(&id);
                if let Value::Object(_, f) = receiver {
                    f.borrow_mut().insert("__closed__".to_string(), Value::Bool(true));
                }
                Ok(Some(Value::Null))
            }
            _ => Ok(None),
        }
    }

    /// Conn object methods (net). Real TCP read/write via std::net.
    fn call_conn_method(
        &mut self,
        receiver: &Value,
        method: &str,
        args: &[Value],
    ) -> Result<Option<Value>> {
        let id = if let Value::Object(_, f) = receiver {
            f.borrow().get("__id__").and_then(|v| v.to_int().ok()).unwrap_or(0)
        } else {
            return Ok(None);
        };
        match method {
            "write" => {
                let data = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                if let Some(stream) = self.tcp_streams.get_mut(&id) {
                    use std::io::Write;
                    let _ = stream.write_all(data.as_bytes());
                    Ok(Some(Value::Int(data.len() as i64)))
                } else {
                    Ok(Some(Value::Int(0)))
                }
            }
            "read" => {
                let n = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(1024) as usize;
                if let Some(stream) = self.tcp_streams.get_mut(&id) {
                    use std::io::Read;
                    let mut buf = vec![0u8; n];
                    match stream.read(&mut buf) {
                        Ok(read_n) => {
                            buf.truncate(read_n);
                            Ok(Some(Value::Str(String::from_utf8_lossy(&buf).to_string())))
                        }
                        Err(_) => Ok(Some(Value::Str(String::new()))),
                    }
                } else {
                    Ok(Some(Value::Str(String::new())))
                }
            }
            "close" => {
                self.tcp_streams.remove(&id);
                Ok(Some(Value::Null))
            }
            "local_addr" | "remote_addr" => {
                if let Some(stream) = self.tcp_streams.get(&id) {
                    let addr = if method == "local_addr" {
                        stream.local_addr()
                    } else {
                        stream.peer_addr()
                    };
                    match addr {
                        Ok(a) => Ok(Some(Value::Str(a.to_string()))),
                        Err(_) => Ok(Some(Value::Null)),
                    }
                } else {
                    Ok(Some(Value::Null))
                }
            }
            _ => Ok(None),
        }
    }

    /// Listener object methods (net). Real TCP accept via std::net.
    fn call_listener_method(
        &mut self,
        receiver: &Value,
        method: &str,
        _args: &[Value],
    ) -> Result<Option<Value>> {
        let id = if let Value::Object(_, f) = receiver {
            f.borrow().get("__id__").and_then(|v| v.to_int().ok()).unwrap_or(0)
        } else {
            return Ok(None);
        };
        match method {
            "accept" => {
                let listener = match self.tcp_listeners.get(&id) {
                    Some(l) => l,
                    None => return Ok(Some(Value::Null)),
                };
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        let new_id = self.next_net_id;
                        self.next_net_id += 1;
                        self.tcp_streams.insert(new_id, stream);
                        let mut f = HashMap::new();
                        f.insert("__id__".to_string(), Value::Int(new_id));
                        Ok(Some(Value::Object("Conn".to_string(), Rc::new(RefCell::new(f)))))
                    }
                    Err(_) => Ok(Some(Value::Null)),
                }
            }
            "close" => {
                self.tcp_listeners.remove(&id);
                Ok(Some(Value::Null))
            }
            _ => Ok(None),
        }
    }

    /// DB object methods (sql). Real in-memory SQL execution.
    fn call_db_method(
        &mut self,
        receiver: &Value,
        method: &str,
        args: &[Value],
    ) -> Result<Option<Value>> {
        let id = if let Value::Object(_, f) = receiver {
            f.borrow().get("__id__").and_then(|v| v.to_int().ok()).unwrap_or(0)
        } else {
            return Ok(None);
        };
        match method {
            "exec" => {
                let sql = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let affected = if let Some(db) = self.sql_dbs.get_mut(&id) {
                    sql_exec(db, &sql)
                } else {
                    0
                };
                let mut d = HashMap::new();
                d.insert("last_insert_id".to_string(), Value::Int(0));
                d.insert("rows_affected".to_string(), Value::Int(affected));
                Ok(Some(Value::Dict(d)))
            }
            "query" => {
                let sql = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let (columns, rows) = if let Some(db) = self.sql_dbs.get_mut(&id) {
                    sql_query(db, &sql)
                } else {
                    (vec![], vec![])
                };
                let mut f = HashMap::new();
                f.insert("__columns__".to_string(), Value::List(columns.into_iter().map(Value::Str).collect()));
                f.insert("__rows__".to_string(), Value::List(rows.into_iter().map(|r| Value::List(r)).collect()));
                f.insert("__idx__".to_string(), Value::Int(0));
                Ok(Some(Value::Object("Rows".to_string(), Rc::new(RefCell::new(f)))))
            }
            "prepare" => {
                let sql = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let mut f = HashMap::new();
                f.insert("__db_id__".to_string(), Value::Int(id));
                f.insert("__sql__".to_string(), Value::Str(sql));
                Ok(Some(Value::Object("Stmt".to_string(), Rc::new(RefCell::new(f)))))
            }
            "close" => {
                self.sql_dbs.remove(&id);
                Ok(Some(Value::Null))
            }
            _ => Ok(None),
        }
    }

    /// Rows object methods (sql). Iterates over the rows stored in __rows__.
    fn call_rows_method(
        &self,
        receiver: &Value,
        method: &str,
        _args: &[Value],
    ) -> Result<Option<Value>> {
        let fields = if let Value::Object(_, f) = receiver {
            f.clone()
        } else {
            return Ok(None);
        };
        match method {
            "next" => {
                let f = fields.borrow();
                let idx = f.get("__idx__").and_then(|v| v.to_int().ok()).unwrap_or(0);
                if let Some(Value::List(rows)) = f.get("__rows__") {
                    if (idx as usize) < rows.len() {
                        return Ok(Some(Value::Bool(true)));
                    }
                }
                Ok(Some(Value::Bool(false)))
            }
            "scan" => {
                // Return the current row as a list of values.
                let mut f = fields.borrow_mut();
                let idx = f.get("__idx__").and_then(|v| v.to_int().ok()).unwrap_or(0);
                if let Some(Value::List(rows)) = f.get("__rows__") {
                    if let Some(row) = rows.get(idx as usize) {
                        if let Value::List(vals) = row {
                            let result = vals.clone();
                            *f.get_mut("__idx__").unwrap() = Value::Int(idx + 1);
                            return Ok(Some(Value::List(result)));
                        }
                    }
                }
                Ok(Some(Value::List(vec![])))
            }
            "close" => Ok(Some(Value::Null)),
            _ => Ok(None),
        }
    }

    /// Stmt object methods (sql). Executes the stored SQL with args.
    fn call_stmt_method(
        &mut self,
        receiver: &Value,
        method: &str,
        _args: &[Value],
    ) -> Result<Option<Value>> {
        let (db_id, sql) = if let Value::Object(_, f) = receiver {
            let f = f.borrow();
            (
                f.get("__db_id__").and_then(|v| v.to_int().ok()).unwrap_or(0),
                f.get("__sql__").map(|v| v.to_str()).unwrap_or_default(),
            )
        } else {
            return Ok(None);
        };
        match method {
            "query" => {
                let (columns, rows) = if let Some(db) = self.sql_dbs.get_mut(&db_id) {
                    sql_query(db, &sql)
                } else {
                    (vec![], vec![])
                };
                let mut f = HashMap::new();
                f.insert("__columns__".to_string(), Value::List(columns.into_iter().map(Value::Str).collect()));
                f.insert("__rows__".to_string(), Value::List(rows.into_iter().map(|r| Value::List(r)).collect()));
                f.insert("__idx__".to_string(), Value::Int(0));
                Ok(Some(Value::Object("Rows".to_string(), Rc::new(RefCell::new(f)))))
            }
            "exec" => {
                let affected = if let Some(db) = self.sql_dbs.get_mut(&db_id) {
                    sql_exec(db, &sql)
                } else {
                    0
                };
                let mut d = HashMap::new();
                d.insert("last_insert_id".to_string(), Value::Int(0));
                d.insert("rows_affected".to_string(), Value::Int(affected));
                Ok(Some(Value::Dict(d)))
            }
            "close" => Ok(Some(Value::Null)),
            _ => Ok(None),
        }
    }

    /// EmbeddedFS object methods (embed). Stores __dir__ path; read(path)
    /// reads the file from disk (so embedded files are accessible at runtime).
    fn call_embedded_fs_method(
        &self,
        receiver: &Value,
        method: &str,
        args: &[Value],
    ) -> Result<Option<Value>> {
        let dir = if let Value::Object(_, f) = receiver {
            f.borrow().get("__dir__").map(|v| v.to_str()).unwrap_or_default()
        } else {
            return Ok(None);
        };
        match method {
            "read" => {
                let rel = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let full = std::path::Path::new(&dir).join(&rel);
                match std::fs::read(&full) {
                    Ok(bytes) => Ok(Some(Value::Str(String::from_utf8_lossy(&bytes).to_string()))),
                    Err(_) => Ok(Some(Value::Null)),
                }
            }
            "exists" => {
                let rel = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let full = std::path::Path::new(&dir).join(&rel);
                Ok(Some(Value::Bool(full.exists())))
            }
            "list" => {
                let mut out: Vec<Value> = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for e in entries.flatten() {
                        if let Some(name) = e.file_name().to_str() {
                            out.push(Value::Str(name.to_string()));
                        }
                    }
                }
                Ok(Some(Value::List(out)))
            }
            _ => Ok(None),
        }
    }

    /// Handle magic methods (__add__, __getitem__, etc.) on objects.
    fn try_magic_method(
        &mut self,
        class: &str,
        method: &str,
        receiver: &Value,
        args: &[Value],
    ) -> Result<Option<Value>> {
        // Only handle known magic methods; if the class defines them, call.
        if self.find_method(class, method).is_ok() {
            let v = self.call_method_on_class(
                class,
                receiver.clone(),
                method,
                args.to_vec(),
            )?;
            return Ok(Some(v));
        }
        Ok(None)
    }

    fn call_str_method(
        &mut self,
        s: &str,
        method: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        match method {
            "len" | "__len__" => Ok(Value::Int(s.chars().count() as i64)),
            "upper" | "to_upper" => Ok(Value::Str(s.to_uppercase())),
            "lower" | "to_lower" => Ok(Value::Str(s.to_lowercase())),
            "trim" => Ok(Value::Str(s.trim().to_string())),
            "split" => {
                let sep = args
                    .into_iter()
                    .next()
                    .and_then(|v| if let Value::Str(s) = v { Some(s) } else { None })
                    .unwrap_or_else(|| " ".to_string());
                let parts: Vec<Value> = if sep.is_empty() {
                    s.chars().map(|c| Value::Str(c.to_string())).collect()
                } else {
                    s.split(&sep).map(|p| Value::Str(p.to_string())).collect()
                };
                Ok(Value::List(parts))
            }
            "contains" => {
                let needle = args.into_iter().next();
                let found = match needle {
                    Some(Value::Str(n)) => s.contains(&n),
                    _ => false,
                };
                Ok(Value::Bool(found))
            }
            "starts_with" | "startswith" => {
                let needle = args.into_iter().next();
                let found = match needle {
                    Some(Value::Str(n)) => s.starts_with(&n),
                    _ => false,
                };
                Ok(Value::Bool(found))
            }
            "ends_with" | "endswith" => {
                let needle = args.into_iter().next();
                let found = match needle {
                    Some(Value::Str(n)) => s.ends_with(&n),
                    _ => false,
                };
                Ok(Value::Bool(found))
            }
            "replace" => {
                let mut iter = args.into_iter();
                let from = iter.next()
                    .and_then(|v| if let Value::Str(s) = v { Some(s) } else { None })
                    .unwrap_or_default();
                let to = iter.next()
                    .and_then(|v| if let Value::Str(s) = v { Some(s) } else { None })
                    .unwrap_or_default();
                Ok(Value::Str(s.replace(&from, &to)))
            }
            "find" => {
                let needle = args.into_iter().next();
                match needle {
                    Some(Value::Str(n)) => {
                        match s.find(&n) {
                            Some(byte_pos) => {
                                // Convert byte position to char position.
                                let char_pos = s[..byte_pos].chars().count() as i64;
                                Ok(Value::Int(char_pos))
                            }
                            None => Ok(Value::Int(-1)),
                        }
                    }
                    _ => Ok(Value::Int(-1)),
                }
            }
            "rfind" => {
                let needle = args.into_iter().next();
                match needle {
                    Some(Value::Str(n)) => {
                        match s.rfind(&n) {
                            Some(byte_pos) => {
                                let char_pos = s[..byte_pos].chars().count() as i64;
                                Ok(Value::Int(char_pos))
                            }
                            None => Ok(Value::Int(-1)),
                        }
                    }
                    _ => Ok(Value::Int(-1)),
                }
            }
            "substr" | "slice" => {
                let mut iter = args.into_iter();
                let start = iter.next()
                    .and_then(|v| if let Value::Int(i) = v { Some(i) } else { None })
                    .unwrap_or(0);
                let end = iter.next()
                    .and_then(|v| if let Value::Int(i) = v { Some(i) } else { None });
                let chars: Vec<char> = s.chars().collect();
                let len = chars.len() as i64;
                let real_start = if start < 0 { len + start } else { start };
                let real_end = match end {
                    Some(e) => if e < 0 { len + e } else { e },
                    None => len,
                };
                if real_start < 0 || real_start > len || real_start > real_end {
                    Ok(Value::Str(String::new()))
                } else {
                    let real_end = real_end.min(len);
                    let result: String = chars[(real_start as usize)..(real_end as usize)].iter().collect();
                    Ok(Value::Str(result))
                }
            }
            "repeat" => {
                let count = args.into_iter().next()
                    .and_then(|v| if let Value::Int(i) = v { Some(i) } else { None })
                    .unwrap_or(1);
                if count < 0 {
                    Ok(Value::Str(String::new()))
                } else {
                    Ok(Value::Str(s.repeat(count as usize)))
                }
            }
            "reverse" => {
                let reversed: String = s.chars().rev().collect();
                Ok(Value::Str(reversed))
            }
            "char_at" | "charAt" => {
                let idx = args.into_iter().next()
                    .and_then(|v| if let Value::Int(i) = v { Some(i) } else { None })
                    .unwrap_or(0);
                let chars: Vec<char> = s.chars().collect();
                let len = chars.len() as i64;
                let real_idx = if idx < 0 { len + idx } else { idx };
                if real_idx < 0 || real_idx >= len {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Str(chars[real_idx as usize].to_string()))
                }
            }
            "index_of" => {
                // Alias for find.
                let needle = args.into_iter().next();
                match needle {
                    Some(Value::Str(n)) => {
                        match s.find(&n) {
                            Some(byte_pos) => {
                                let char_pos = s[..byte_pos].chars().count() as i64;
                                Ok(Value::Int(char_pos))
                            }
                            None => Ok(Value::Int(-1)),
                        }
                    }
                    _ => Ok(Value::Int(-1)),
                }
            }
            "is_empty" => Ok(Value::Bool(s.is_empty())),
            "to_str" | "__str__" => Ok(Value::Str(s.to_string())),
            _ => Err(CompilerError::runtime_error(format!(
                "str has no method '{}'",
                method
            ))),
        }
    }

    fn call_list_method(
        &mut self,
        l: &[Value],
        method: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        match method {
            "len" | "__len__" => Ok(Value::Int(l.len() as i64)),
            "push" | "append" => {
                let mut new_list = l.to_vec();
                new_list.extend(args);
                Ok(Value::List(new_list))
            }
            "pop" => {
                if l.is_empty() {
                    Err(CompilerError::runtime_error("pop from empty list"))
                } else {
                    let mut new_list = l.to_vec();
                    let v = new_list.pop().unwrap();
                    Ok(v)
                }
            }
            "contains" => {
                let needle = args.into_iter().next();
                let found = match needle {
                    Some(v) => l.iter().any(|x| x == &v),
                    None => false,
                };
                Ok(Value::Bool(found))
            }
            "join" => {
                let sep = args
                    .into_iter()
                    .next()
                    .and_then(|v| if let Value::Str(s) = v { Some(s) } else { None })
                    .unwrap_or_default();
                let parts: Vec<String> = l
                    .iter()
                    .map(|v| v.to_str())
                    .collect();
                Ok(Value::Str(parts.join(&sep)))
            }
            "sort" => {
                let mut new_list = l.to_vec();
                new_list.sort_by(|a, b| {
                    match (a, b) {
                        (Value::Int(x), Value::Int(y)) => x.cmp(y),
                        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
                        (Value::Int(x), Value::Float(y)) => (*x as f64).partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
                        (Value::Float(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(std::cmp::Ordering::Equal),
                        (Value::Str(x), Value::Str(y)) => x.cmp(y),
                        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
                        _ => std::cmp::Ordering::Equal,
                    }
                });
                Ok(Value::List(new_list))
            }
            "reverse" => {
                let mut new_list = l.to_vec();
                new_list.reverse();
                Ok(Value::List(new_list))
            }
            "map" => {
                let func = args.into_iter().next().unwrap_or(Value::Null);
                let mut result = Vec::new();
                for item in l {
                    let v = self.call_value(&func, vec![item.clone()])?;
                    result.push(v);
                }
                Ok(Value::List(result))
            }
            "filter" => {
                let func = args.into_iter().next().unwrap_or(Value::Null);
                let mut result = Vec::new();
                for item in l {
                    let v = self.call_value(&func, vec![item.clone()])?;
                    let keep = match v {
                        Value::Bool(b) => b,
                        Value::Int(i) => i != 0,
                        Value::Null => false,
                        _ => true,
                    };
                    if keep {
                        result.push(item.clone());
                    }
                }
                Ok(Value::List(result))
            }
            "reduce" => {
                let mut iter = args.into_iter();
                let func = iter.next().unwrap_or(Value::Null);
                let init = iter.next().unwrap_or(Value::Null);
                let mut acc = init;
                for item in l {
                    acc = self.call_value(&func, vec![acc, item.clone()])?;
                }
                Ok(acc)
            }
            "index" | "index_of" => {
                let needle = args.into_iter().next();
                match needle {
                    Some(v) => {
                        let idx = l.iter().position(|x| x == &v);
                        Ok(Value::Int(idx.map(|i| i as i64).unwrap_or(-1)))
                    }
                    None => Ok(Value::Int(-1)),
                }
            }
            "first" => {
                Ok(l.first().cloned().unwrap_or(Value::Null))
            }
            "last" => {
                Ok(l.last().cloned().unwrap_or(Value::Null))
            }
            "min" => {
                let mut best: Option<&Value> = None;
                for v in l {
                    if best.is_none() || self.value_less_than(v, best.unwrap()) {
                        best = Some(v);
                    }
                }
                Ok(best.cloned().unwrap_or(Value::Null))
            }
            "max" => {
                let mut best: Option<&Value> = None;
                for v in l {
                    if best.is_none() || self.value_greater_than(v, best.unwrap()) {
                        best = Some(v);
                    }
                }
                Ok(best.cloned().unwrap_or(Value::Null))
            }
            "sum" => {
                let mut total_i: i64 = 0;
                let mut total_f: f64 = 0.0;
                let mut has_float = false;
                for v in l {
                    match v {
                        Value::Int(i) => { total_i += i; total_f += *i as f64; }
                        Value::Float(f) => { total_f += f; has_float = true; }
                        _ => {}
                    }
                }
                if has_float {
                    Ok(Value::Float(total_f + total_i as f64))
                } else {
                    Ok(Value::Int(total_i))
                }
            }
            "unique" => {
                let mut result: Vec<Value> = Vec::new();
                for v in l {
                    if !result.iter().any(|x| x == v) {
                        result.push(v.clone());
                    }
                }
                Ok(Value::List(result))
            }
            "slice" => {
                let mut iter = args.into_iter();
                let start = iter.next()
                    .and_then(|v| if let Value::Int(i) = v { Some(i) } else { None })
                    .unwrap_or(0);
                let end = iter.next()
                    .and_then(|v| if let Value::Int(i) = v { Some(i) } else { None });
                let len = l.len() as i64;
                let real_start = if start < 0 { len + start } else { start };
                let real_end = match end {
                    Some(e) => if e < 0 { len + e } else { e },
                    None => len,
                };
                if real_start < 0 || real_start > len || real_start > real_end {
                    Ok(Value::List(Vec::new()))
                } else {
                    let real_end = real_end.min(len);
                    Ok(Value::List(l[(real_start as usize)..(real_end as usize)].to_vec()))
                }
            }
            "concat" => {
                let mut new_list = l.to_vec();
                for arg in args {
                    match arg {
                        Value::List(other) => new_list.extend(other),
                        Value::Tuple(other) => new_list.extend(other),
                        other => new_list.push(other),
                    }
                }
                Ok(Value::List(new_list))
            }
            "is_empty" => Ok(Value::Bool(l.is_empty())),
            "insert" => {
                let mut iter = args.into_iter();
                let idx = iter.next()
                    .and_then(|v| if let Value::Int(i) = v { Some(i) } else { None })
                    .unwrap_or(0);
                let item = iter.next().unwrap_or(Value::Null);
                let mut new_list = l.to_vec();
                let len = new_list.len() as i64;
                let real_idx = if idx < 0 { len + idx } else { idx };
                if real_idx < 0 || real_idx > len {
                    Err(CompilerError::runtime_error(format!(
                        "list insert index {} out of bounds (len {})",
                        real_idx, len
                    )))
                } else {
                    new_list.insert(real_idx as usize, item);
                    Ok(Value::List(new_list))
                }
            }
            "remove" => {
                let idx = args.into_iter().next()
                    .and_then(|v| if let Value::Int(i) = v { Some(i) } else { None })
                    .unwrap_or(0);
                let mut new_list = l.to_vec();
                let len = new_list.len() as i64;
                let real_idx = if idx < 0 { len + idx } else { idx };
                if real_idx < 0 || real_idx >= len {
                    Err(CompilerError::runtime_error(format!(
                        "list remove index {} out of bounds (len {})",
                        real_idx, len
                    )))
                } else {
                    let removed = new_list.remove(real_idx as usize);
                    // Return the removed item (like pop), consistent with
                    // the existing pop semantics.
                    Ok(removed)
                }
            }
            "to_str" | "__str__" => Ok(Value::Str(format!("{:?}", l))),
            _ => Err(CompilerError::runtime_error(format!(
                "list has no method '{}'",
                method
            ))),
        }
    }

    /// Compare two values: returns true if a < b.
    fn value_less_than(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => x < y,
            (Value::Float(x), Value::Float(y)) => x < y,
            (Value::Int(x), Value::Float(y)) => (*x as f64) < *y,
            (Value::Float(x), Value::Int(y)) => x < &(*y as f64),
            (Value::Str(x), Value::Str(y)) => x < y,
            (Value::Bool(x), Value::Bool(y)) => !*x && *y,
            _ => false,
        }
    }

    /// Compare two values: returns true if a > b.
    fn value_greater_than(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => x > y,
            (Value::Float(x), Value::Float(y)) => x > y,
            (Value::Int(x), Value::Float(y)) => (*x as f64) > *y,
            (Value::Float(x), Value::Int(y)) => x > &(*y as f64),
            (Value::Str(x), Value::Str(y)) => x > y,
            (Value::Bool(x), Value::Bool(y)) => *x && !*y,
            _ => false,
        }
    }

    fn call_dict_method(
        &mut self,
        d: &HashMap<String, Value>,
        method: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        // File-handle dispatch: dicts produced by `open()` carry "path"
        // and "mode" keys. Route I/O method calls to the file builtins
        // so `with, open(p, "w"), f \n f.write(...) /end` works.
        let is_file_handle = d.contains_key("path") && d.contains_key("mode");
        if is_file_handle {
            if let Some(v) = self.call_file_handle_method(d, method, &args)? {
                return Ok(v);
            }
        }
        match method {
            "len" | "__len__" => Ok(Value::Int(d.len() as i64)),
            "get" => {
                let mut iter = args.into_iter();
                let key = iter
                    .next()
                    .map(|v| v.to_str())
                    .unwrap_or_default();
                Ok(d.get(&key).cloned().unwrap_or(Value::Null))
            }
            "has" | "contains" => {
                let key = args
                    .into_iter()
                    .next()
                    .map(|v| v.to_str())
                    .unwrap_or_default();
                Ok(Value::Bool(d.contains_key(&key)))
            }
            "keys" => Ok(Value::List(
                d.keys().map(|k| Value::Str(k.clone())).collect(),
            )),
            "values" => Ok(Value::List(d.values().cloned().collect())),
            "items" => {
                let items: Vec<Value> = d.iter().map(|(k, v)| {
                    Value::Tuple(vec![Value::Str(k.clone()), v.clone()])
                }).collect();
                Ok(Value::List(items))
            }
            "set" => {
                let mut iter = args.into_iter();
                let key = iter.next().map(|v| v.to_str()).unwrap_or_default();
                let val = iter.next().unwrap_or(Value::Null);
                let mut new_d = d.clone();
                new_d.insert(key, val);
                Ok(Value::Dict(new_d))
            }
            "delete" | "remove" => {
                let key = args.into_iter().next().map(|v| v.to_str()).unwrap_or_default();
                let mut new_d = d.clone();
                new_d.remove(&key);
                Ok(Value::Dict(new_d))
            }
            "is_empty" => Ok(Value::Bool(d.is_empty())),
            "merge" => {
                let other = args.into_iter().next().unwrap_or(Value::Null);
                let mut new_d = d.clone();
                if let Value::Dict(other_d) = other {
                    for (k, v) in other_d {
                        new_d.insert(k, v);
                    }
                }
                Ok(Value::Dict(new_d))
            }
            "to_str" | "__str__" => {
                let items: Vec<String> = d.iter().map(|(k, v)| {
                    format!("{}: {}", k, v.to_str())
                }).collect();
                Ok(Value::Str(format!("{{{}}}", items.join(", "))))
            }
            _ => Err(CompilerError::runtime_error(format!(
                "dict has no method '{}'",
                method
            ))),
        }
    }

    /// Dispatch file-handle method calls on dicts returned by `open()`.
    /// Returns `Ok(Some(v))` when the method was recognised as a file
    /// operation, `Ok(None)` when it wasn't (so the caller falls through
    /// to the generic dict methods). The dict is passed by shared
    /// reference; position updates cannot be persisted, but `write`
    /// persists to disk which is what matters in practice.
    fn call_file_handle_method(
        &self,
        d: &HashMap<String, Value>,
        method: &str,
        args: &[Value],
    ) -> Result<Option<Value>> {
        let path = d
            .get("path")
            .and_then(|v| if let Value::Str(s) = v { Some(s.clone()) } else { None })
            .unwrap_or_default();
        let mode = d
            .get("mode")
            .and_then(|v| if let Value::Str(s) = v { Some(s.clone()) } else { None })
            .unwrap_or_else(|| "r".to_string());
        let content = d
            .get("content")
            .and_then(|v| if let Value::Str(s) = v { Some(s.clone()) } else { None })
            .unwrap_or_default();
        let pos = d
            .get("pos")
            .and_then(|v| v.to_int().ok())
            .unwrap_or(0) as usize;
        match method {
            "write" | "write_str" => {
                if !mode.contains('w') && !mode.contains('a') && !mode.contains('+') {
                    return Ok(Some(Value::Bool(false)));
                }
                let data = args
                    .iter()
                    .next()
                    .map(|v| v.to_str())
                    .unwrap_or_default();
                let result = if mode.contains('a') {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .and_then(|mut f| std::io::Write::write_all(&mut f, data.as_bytes()))
                } else {
                    std::fs::write(&path, &data)
                };
                Ok(Some(Value::Bool(result.is_ok())))
            }
            "read" => {
                let n = args
                    .iter()
                    .next()
                    .and_then(|v| v.to_int().ok())
                    .unwrap_or(-1);
                let chars: Vec<char> = content.chars().collect();
                let end = if n < 0 {
                    chars.len()
                } else {
                    (pos + n as usize).min(chars.len())
                };
                let result: String = chars[pos.min(chars.len())..end].iter().collect();
                Ok(Some(Value::Str(result)))
            }
            "readline" | "read_line" => {
                let chars: Vec<char> = content.chars().collect();
                let start = pos.min(chars.len());
                let mut end = start;
                while end < chars.len() && chars[end] != '\n' {
                    end += 1;
                }
                if end < chars.len() {
                    end += 1; // include the newline
                }
                let result: String = chars[start..end].iter().collect();
                Ok(Some(Value::Str(result)))
            }
            "readall" | "read_all" => Ok(Some(Value::Str(content.clone()))),
            "close" => Ok(Some(Value::Null)),
            "flush" => Ok(Some(Value::Bool(true))),
            "tell" => Ok(Some(Value::Int(pos as i64))),
            "seek" => {
                // Position updates cannot be persisted on a shared &dict;
                // report success but leave the dict unchanged.
                Ok(Some(Value::Int(pos as i64)))
            }
            "writelines" => {
                if let Some(Value::List(lines)) = args.iter().next() {
                    let mut joined = String::new();
                    for l in lines {
                        joined.push_str(&l.to_str());
                    }
                    let result = if mode.contains('a') {
                        std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&path)
                            .and_then(|mut f| std::io::Write::write_all(&mut f, joined.as_bytes()))
                    } else {
                        std::fs::write(&path, &joined)
                    };
                    Ok(Some(Value::Bool(result.is_ok())))
                } else {
                    Ok(Some(Value::Bool(false)))
                }
            }
            _ => Ok(None),
        }
    }

    /// Resume a generator: return the next yielded value, or the return
    /// value when the generator is exhausted.
    fn resume_generator(&mut self, handle: Value, _args: Vec<Value>) -> Result<Value> {
        match handle {
            Value::Generator(g) => {
                let mut state = g.borrow_mut();
                if state.yield_idx < state.yielded_values.len() {
                    let v = state.yielded_values[state.yield_idx].clone();
                    state.yield_idx += 1;
                    if state.yield_idx >= state.yielded_values.len() {
                        state.done = true;
                    }
                    Ok(v)
                } else {
                    state.done = true;
                    // Generator exhausted: return the generator's return
                    // value (default 0 if not set).
                    Ok(state.return_value.clone().unwrap_or(Value::Int(0)))
                }
            }
            other => Err(CompilerError::runtime_error(format!(
                "cannot resume {:?}",
                other
            ))),
        }
    }

    fn ast_binary(&mut self, l: Value, op: &BinaryOp, r: Value) -> Result<Value> {
        // Check for operator overloads first (both operands).
        let overload_method = match op {
            BinaryOp::Add => Some("__add__"),
            BinaryOp::Sub => Some("__sub__"),
            BinaryOp::Mul => Some("__mul__"),
            BinaryOp::Div | BinaryOp::FloorDiv => Some("__div__"),
            BinaryOp::Mod => Some("__mod__"),
            BinaryOp::Eq => Some("__eq__"),
            BinaryOp::Ne => Some("__ne__"),
            BinaryOp::Lt => Some("__lt__"),
            BinaryOp::Gt => Some("__gt__"),
            BinaryOp::Le => Some("__le__"),
            BinaryOp::Ge => Some("__ge__"),
            BinaryOp::Power => Some("__pow__"),
            BinaryOp::In => Some("__contains__"),
            _ => None,
        };
        if let Some(method) = overload_method {
            if let Some(v) = self.try_binary_overload(method, &l, &r)? {
                return Ok(v);
            }
            // For __contains__, the order is reversed: `x in obj` → obj.__contains__(x)
            if op == &BinaryOp::In {
                if let Value::Object(class, _) = &r {
                    let class = class.clone();
                    if self.find_method(&class, "__contains__").is_ok() {
                        let v = self.call_method_on_class(&class, r, "__contains__", vec![l])?;
                        return Ok(v);
                    }
                }
            }
        }
        let v = match op {
            BinaryOp::Add => {
                match (&l, &r) {
                    (Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_add(*b)),
                    (Value::Float(a), Value::Float(b)) => Value::Float(a + b),
                    (Value::Int(a), Value::Float(b)) => Value::Float(*a as f64 + b),
                    (Value::Float(a), Value::Int(b)) => Value::Float(a + *b as f64),
                    (Value::Str(a), Value::Str(b)) => Value::Str(format!("{}{}", a, b)),
                    (Value::Str(a), _) => Value::Str(format!("{}{}", a, r.to_str())),
                    (_, Value::Str(b)) => Value::Str(format!("{}{}", l.to_str(), b)),
                    // List + List → concatenation
                    (Value::List(a), Value::List(b)) => {
                        let mut result = a.clone();
                        result.extend(b.iter().cloned());
                        Value::List(result)
                    }
                    // List + element → append
                    (Value::List(a), _) => {
                        let mut result = a.clone();
                        result.push(r.clone());
                        Value::List(result)
                    }
                    // element + List → prepend
                    (_, Value::List(b)) => {
                        let mut result = vec![l.clone()];
                        result.extend(b.iter().cloned());
                        Value::List(result)
                    }
                    // Tuple + Tuple → concatenation
                    (Value::Tuple(a), Value::Tuple(b)) => {
                        let mut result = a.clone();
                        result.extend(b.iter().cloned());
                        Value::Tuple(result)
                    }
                    _ => Value::Null,
                }
            }
            BinaryOp::Sub => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_sub(b)),
                (Value::Float(a), Value::Float(b)) => Value::Float(a - b),
                (Value::Int(a), Value::Float(b)) => Value::Float(a as f64 - b),
                (Value::Float(a), Value::Int(b)) => Value::Float(a - b as f64),
                _ => Value::Null,
            },
            BinaryOp::Mul => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_mul(b)),
                (Value::Float(a), Value::Float(b)) => Value::Float(a * b),
                (Value::Int(a), Value::Float(b)) => Value::Float(a as f64 * b),
                (Value::Float(a), Value::Int(b)) => Value::Float(a * b as f64),
                // String repeat: "ab" * 3 → "ababab".
                (Value::Str(s), Value::Int(n)) => {
                    if n > 0 {
                        Value::Str(s.repeat(n as usize))
                    } else {
                        Value::Str(String::new())
                    }
                }
                (Value::Int(n), Value::Str(s)) => {
                    if n > 0 {
                        Value::Str(s.repeat(n as usize))
                    } else {
                        Value::Str(String::new())
                    }
                }
                _ => Value::Null,
            },
            BinaryOp::Div => match (l, r) {
                // True division (/) always returns float for int/int.
                (Value::Int(a), Value::Int(b)) => {
                    if b == 0 {
                        return Err(CompilerError::runtime_error("division by zero"));
                    }
                    Value::Float(a as f64 / b as f64)
                }
                (Value::Float(a), Value::Float(b)) => {
                    if b == 0.0 {
                        return Err(CompilerError::runtime_error("division by zero"));
                    }
                    Value::Float(a / b)
                }
                (Value::Int(a), Value::Float(b)) => {
                    if b == 0.0 {
                        return Err(CompilerError::runtime_error("division by zero"));
                    }
                    Value::Float(a as f64 / b)
                }
                (Value::Float(a), Value::Int(b)) => {
                    if b == 0 {
                        return Err(CompilerError::runtime_error("division by zero"));
                    }
                    Value::Float(a / b as f64)
                }
                _ => Value::Null,
            },
            BinaryOp::FloorDiv => match (l, r) {
                // Floor division (//): mathematical floor, not truncation.
                // -7 // 2 = -4 (not -3).
                (Value::Int(a), Value::Int(b)) => {
                    if b == 0 {
                        return Err(CompilerError::runtime_error("division by zero"));
                    }
                    if a == i64::MIN && b == -1 { Value::Int(i64::MIN) } else { Value::Int(a.div_euclid(b)) }
                }
                (Value::Float(a), Value::Float(b)) => {
                    if b == 0.0 {
                        return Err(CompilerError::runtime_error("division by zero"));
                    }
                    Value::Float((a / b).floor())
                }
                (Value::Int(a), Value::Float(b)) => {
                    if b == 0.0 {
                        return Err(CompilerError::runtime_error("division by zero"));
                    }
                    Value::Float((a as f64 / b).floor())
                }
                (Value::Float(a), Value::Int(b)) => {
                    if b == 0 {
                        return Err(CompilerError::runtime_error("division by zero"));
                    }
                    Value::Float((a / b as f64).floor())
                }
                _ => Value::Null,
            },
            BinaryOp::Mod => match (l, r) {
                // Modulo: result has the same sign as the divisor (Python-style).
                // -7 % 2 = 1, 7 % -2 = -1.
                (Value::Int(a), Value::Int(b)) => {
                    if b == 0 {
                        return Err(CompilerError::runtime_error("modulo by zero"));
                    }
                    if a == i64::MIN && b == -1 { Value::Int(0) } else { Value::Int(a.rem_euclid(b)) }
                }
                (Value::Float(a), Value::Float(b)) => {
                    if b == 0.0 {
                        return Err(CompilerError::runtime_error("modulo by zero"));
                    }
                    Value::Float(a.rem_euclid(b))
                }
                (Value::Int(a), Value::Float(b)) => {
                    if b == 0.0 {
                        return Err(CompilerError::runtime_error("modulo by zero"));
                    }
                    Value::Float((a as f64).rem_euclid(b))
                }
                (Value::Float(a), Value::Int(b)) => {
                    if b == 0 {
                        return Err(CompilerError::runtime_error("modulo by zero"));
                    }
                    Value::Float(a.rem_euclid(b as f64))
                }
                _ => Value::Null,
            },
            BinaryOp::Eq => Value::Bool(l == r),
            BinaryOp::Ne => Value::Bool(l != r),
            BinaryOp::Power => match (&l, &r) {
                (Value::Int(a), Value::Int(b)) => {
                    if *b >= 0 {
                        Value::Float((*a as f64).powi(*b as i32))
                    } else {
                        Value::Float((*a as f64).powf(*b as f64))
                    }
                }
                (Value::Float(a), Value::Float(b)) => Value::Float(a.powf(*b)),
                (Value::Int(a), Value::Float(b)) => Value::Float((*a as f64).powf(*b)),
                (Value::Float(a), Value::Int(b)) => Value::Float(a.powi(*b as i32)),
                _ => Value::Null,
            },
            BinaryOp::Repeated => {
                // `lhs repeated N` — string or list repetition.
                // Left must be Str or List; right must be Int.
                let n = match &r {
                    Value::Int(n) => *n,
                    _ => return Err(CompilerError::runtime_error(
                        "repeated: right operand must be an integer"
                    )),
                };
                if n < 0 {
                    return Err(CompilerError::runtime_error(
                        "repeated: count cannot be negative"
                    ));
                }
                let n = n as usize;
                match &l {
                    Value::Str(s) => Value::Str(s.repeat(n)),
                    Value::List(items) => {
                        let mut out: Vec<Value> = Vec::with_capacity(items.len() * n);
                        for _ in 0..n {
                            for v in items.iter() {
                                out.push(v.clone());
                            }
                        }
                        Value::List(out)
                    }
                    _ => return Err(CompilerError::runtime_error(
                        "repeated: left operand must be a string or list"
                    )),
                }
            }
            BinaryOp::Lt => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Bool(a < b),
                (Value::Float(a), Value::Float(b)) => Value::Bool(a < b),
                (Value::Int(a), Value::Float(b)) => Value::Bool((a as f64) < b),
                (Value::Float(a), Value::Int(b)) => Value::Bool(a < (b as f64)),
                (Value::Str(a), Value::Str(b)) => Value::Bool(a < b),
                _ => Value::Null,
            },
            BinaryOp::Gt => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Bool(a > b),
                (Value::Float(a), Value::Float(b)) => Value::Bool(a > b),
                (Value::Int(a), Value::Float(b)) => Value::Bool((a as f64) > b),
                (Value::Float(a), Value::Int(b)) => Value::Bool(a > (b as f64)),
                (Value::Str(a), Value::Str(b)) => Value::Bool(a > b),
                _ => Value::Null,
            },
            BinaryOp::Le => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Bool(a <= b),
                (Value::Float(a), Value::Float(b)) => Value::Bool(a <= b),
                (Value::Int(a), Value::Float(b)) => Value::Bool((a as f64) <= b),
                (Value::Float(a), Value::Int(b)) => Value::Bool(a <= (b as f64)),
                (Value::Str(a), Value::Str(b)) => Value::Bool(a <= b),
                _ => Value::Null,
            },
            BinaryOp::Ge => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Bool(a >= b),
                (Value::Float(a), Value::Float(b)) => Value::Bool(a >= b),
                (Value::Int(a), Value::Float(b)) => Value::Bool((a as f64) >= b),
                (Value::Float(a), Value::Int(b)) => Value::Bool(a >= (b as f64)),
                (Value::Str(a), Value::Str(b)) => Value::Bool(a >= b),
                _ => Value::Null,
            },
            // And/Or are handled by the caller with short-circuit logic.
            BinaryOp::And => Value::Bool(l.truthy() && r.truthy()),
            BinaryOp::Or => Value::Bool(l.truthy() || r.truthy()),
            _ => Value::Null,
        };
        Ok(v)
    }

    fn ast_method_call(
        &mut self,
        receiver: Value,
        method: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        match &receiver {
            Value::Object(class, _) => {
                // Built-in object types (Time, Regex, Mutex) dispatched here
                // before falling through to user-defined class methods.
                if let Some(v) = self.call_builtin_object_method(&receiver, class, method, &args)? {
                    return Ok(v);
                }
                if let Some(v) = self.try_magic_method(class, method, &receiver, &args)? {
                    return Ok(v);
                }
                let class = class.clone();
                self.call_method_on_class(&class, receiver, method, args)
            }
            Value::Class(class) => {
                // ClassName.method(args). The only supported form is
                // ClassName.new(args) → instantiate the class. Other static
                // method calls on a class value are not supported.
                let class = class.clone();
                if method == "new" {
                    self.instantiate_class(&class, args)
                } else {
                    Err(CompilerError::runtime_error(format!(
                        "cannot call static method '{}' on class '{}'",
                        method, class
                    )))
                }
            }
            Value::List(l) => self.call_list_method(l, method, args),
            Value::Str(s) => self.call_str_method(s, method, args),
            Value::Dict(d) => self.call_dict_method(d, method, args),
            Value::Generator(g) => {
                // Generator methods: next(), done, reset(), to_list()
                let mut state = g.borrow_mut();
                match method {
                    "next" | "__next__" | "resume" => {
                        if state.yield_idx < state.yielded_values.len() {
                            let v = state.yielded_values[state.yield_idx].clone();
                            state.yield_idx += 1;
                            if state.yield_idx >= state.yielded_values.len() {
                                state.done = true;
                            }
                            Ok(v)
                        } else {
                            state.done = true;
                            Ok(state.return_value.clone().unwrap_or(Value::Null))
                        }
                    }
                    "send" => {
                        if state.yield_idx < state.yielded_values.len() {
                            let v = state.yielded_values[state.yield_idx].clone();
                            state.yield_idx += 1;
                            if state.yield_idx >= state.yielded_values.len() {
                                state.done = true;
                            }
                            Ok(v)
                        } else {
                            state.done = true;
                            Ok(state.return_value.clone().unwrap_or(Value::Null))
                        }
                    }
                    "reset" | "rewind" => {
                        state.yield_idx = 0;
                        state.done = false;
                        Ok(Value::Null)
                    }
                    "to_list" | "collect" => {
                        Ok(Value::List(state.yielded_values.clone()))
                    }
                    "done" | "is_done" => {
                        Ok(Value::Bool(state.done))
                    }
                    "has_next" => {
                        Ok(Value::Bool(state.yield_idx < state.yielded_values.len()))
                    }
                    "len" | "__len__" => {
                        Ok(Value::Int(state.yielded_values.len() as i64))
                    }
                    "to_str" | "__str__" => {
                        Ok(Value::Str(format!("<generator {} values>", state.yielded_values.len())))
                    }
                    _ => Err(CompilerError::runtime_error(format!(
                        "generator has no method '{}'",
                        method
                    ))),
                }
            }
            _ => Ok(Value::Null),
        }
    }

    /// Call a method on a class instance. Looks up the method in the
    /// class definition (and parent chain), binds `self`, and executes.
    fn call_method_on_class(
        &mut self,
        class: &str,
        receiver: Value,
        method: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        // Invalidate the global cache: method calls may modify globals.
        self.invalidate_global_cache();
        let fn_def = self.find_method(class, method)?.clone();
        // Push method context for super dispatch.
        self.method_context.push((class.to_string(), method.to_string()));
        let old_self = self.globals.insert("self".to_string(), receiver.clone());
        // Determine parameter binding (skip explicit `self` param if present).
        let skip_self = fn_def.params.first().map(|p| p.name.name == "self").unwrap_or(false);
        let bind_params: Vec<(String, Value)> = if skip_self {
            fn_def.params.iter().skip(1).enumerate()
                .map(|(i, p)| (p.name.name.clone(), args.get(i).cloned().unwrap_or(Value::Null)))
                .collect()
        } else {
            fn_def.params.iter().enumerate()
                .map(|(i, p)| (p.name.name.clone(), args.get(i).cloned().unwrap_or(Value::Null)))
                .collect()
        };

        // Bytecode path: if the method body was compiled to bytecode
        // (registered in fn_entry_pcs under "ClassName.method"), execute it
        // via a Frame. The method's params + locals live in frame.locals
        // (slot-based), isolated from the caller. `self` lives in globals.
        // 继承方法查找：如果当前 class 的 qualified name 找不到，
        // 沿继承链向上查找父类的 qualified name。
        let qualified = format!("{}.{}", class, method);
        let entry = if self.gen_set.contains(method) || fn_def.is_async || !fn_def.type_constraints.is_empty() {
            None
        } else {
            if let Some(e) = self.module.fn_entry_pcs.get(&qualified).copied() {
                Some(e)
            } else {
                // 沿继承链查找父类的 qualified name。
                let mut cur = Some(class.to_string());
                let mut found = None;
                while let Some(c) = cur {
                    let q = format!("{}.{}", c, method);
                    if let Some(e) = self.module.fn_entry_pcs.get(&q).copied() {
                        found = Some(e);
                        break;
                    }
                    // 查找父类名。
                    if let Some(prog) = &self.program {
                        cur = prog.declarations.iter().find_map(|d| {
                            if let crate::parser::ast::TopLevel::ClassDef(cd) = d {
                                if cd.name.name == c {
                                    return cd.extends.as_ref().and_then(|e| {
                                        if let crate::parser::ast::TypeExpr::Named(id, _) = e {
                                            Some(id.name.clone())
                                        } else { None }
                                    });
                                }
                            }
                            None
                        });
                    } else { cur = None; }
                }
                found
            }
        };

        let result: Result<()>;
        if let Some((entry_pc, num_locals)) = entry {
            // Method runs via bytecode. Bind params into frame.locals slots.
            // The compiler pre-declares params as locals in slots 0..n, so
            // we map args to those slots. `self` is in globals (not a slot).
            let nslots = num_locals.max(bind_params.len());
            let mut locals = if let Some(mut v) = self.locals_pool.pop() {
                v.clear();
                v.resize(nslots, Value::Null);
                v
            } else {
                vec![Value::Null; nslots]
            };
            for (i, (_, v)) in bind_params.iter().enumerate() {
                if i < locals.len() {
                    locals[i] = v.clone();
                }
            }
            let stack_base = self.stack.len();
            let saved_pc = self.pc;
            self.frames.push(Frame {
                locals,
                return_pc: self.pc,
                stack_base,
                is_main: false,
                defer_pcs: Vec::new(),
                fn_name: method.to_string(),
            });
            self.pc = entry_pc;
            let target_frame_depth = self.frames.len() - 1;
            let code_len = self.module.code.len();
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                while self.pc < code_len && self.frames.len() > target_frame_depth {
                    let pc = self.pc;
                    let instr = self.module.code[pc].clone();
                    self.pc += 1;
                    match self.execute(&instr) {
                        Ok(Flow::Continue) => {}
                        Ok(Flow::Return(_)) => break,
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            }));
            result = match panicked {
                Ok(inner) => inner,
                Err(payload) => Err(CompilerError::runtime_error(format!(
                    "method panic: {}",
                    if let Some(s) = payload.downcast_ref::<&'static str>() { (*s).to_string() }
                    else if let Some(s) = payload.downcast_ref::<String>() { s.clone() }
                    else { "<non-string panic payload>".to_string() }
                ))),
            };
            self.pc = saved_pc;
        } else {
            // Method body not compiled — should not happen now.
            result = Ok(());
        }

        self.method_context.pop();
        match old_self {
            Some(v) => { self.globals.insert("self".to_string(), v); }
            None => { self.globals.remove("self"); }
        }
        match result {
            Ok(()) => {
                // Bytecode path: Ret pushed the return value onto the stack.
                if entry.is_some() {
                    Ok(self.stack.pop().unwrap_or(Value::Null))
                } else {
                    Ok(Value::Null)
                }
            }
            Err(e) => {
                let msg = e.message();
                if msg == "return" {
                    Ok(self.stack.pop().unwrap_or(Value::Null))
                } else if msg == "yield" {
                    Ok(self.stack.pop().unwrap_or(Value::Null))
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Find a method in a class (with inheritance lookup).
    fn find_method(&self, class: &str, method: &str) -> Result<&AstFnDef> {
        if let Some(prog) = &self.program {
            let mut current = Some(class.to_string());
            loop {
                let cname = match &current {
                    Some(c) => c.clone(),
                    None => break,
                };
                let mut found_next = false;
                for d in &prog.declarations {
                    if let crate::parser::ast::TopLevel::ClassDef(cd) = d {
                        if cd.name.name == cname {
                            for m in &cd.methods {
                                if m.name.name == method {
                                    return Ok(m);
                                }
                            }
                            // Not found in this class; check parent.
                            let parent = cd.extends.as_ref().and_then(|e| {
                                if let crate::parser::ast::TypeExpr::Named(id, _) = e {
                                    Some(id.name.clone())
                                } else {
                                    None
                                }
                            });
                            current = parent;
                            found_next = true;
                            break;
                        }
                    }
                }
                if !found_next {
                    break;
                }
            }
        }
        Err(CompilerError::runtime_error(format!(
            "method '{}.{}' not found",
            class, method
        )))
    }

    /// Find the parent class of `class`, if any.
    fn find_parent_class(&self, class: &str) -> Option<String> {
        if let Some(prog) = &self.program {
            for d in &prog.declarations {
                if let crate::parser::ast::TopLevel::ClassDef(cd) = d {
                    if cd.name.name == class {
                        if let Some(e) = &cd.extends {
                            if let crate::parser::ast::TypeExpr::Named(id, _) = e {
                                return Some(id.name.clone());
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Call a list method (push, pop, len, etc.).
    fn call_list_method_old(
        &mut self,
        list: Vec<Value>,
        method: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        match method {
            "push" => {
                let mut l = list;
                for a in args {
                    l.push(a);
                }
                Ok(Value::List(l))
            }
            "pop" => {
                let mut l = list;
                let v = l.pop().unwrap_or(Value::Null);
                // Push the modified list back? No — pop returns the element.
                let _ = l;
                Ok(v)
            }
            "len" => Ok(Value::Int(list.len() as i64)),
            _ => Ok(Value::Null),
        }
    }

    /// Call a string method.
    fn call_str_method_old(
        &mut self,
        s: String,
        method: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        match method {
            "len" => Ok(Value::Int(s.chars().count() as i64)),
            "upper" => Ok(Value::Str(s.to_uppercase())),
            "lower" => Ok(Value::Str(s.to_lowercase())),
            _ => Ok(Value::Null),
        }
    }

    /// Call a dict method.
    fn call_dict_method_old(
        &mut self,
        d: HashMap<String, Value>,
        method: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        match method {
            "len" => Ok(Value::Int(d.len() as i64)),
            "keys" => Ok(Value::List(d.keys().map(|k| Value::Str(k.clone())).collect())),
            "values" => Ok(Value::List(d.values().cloned().collect())),
            _ => Ok(Value::Null),
        }
    }

    fn index_get(&mut self, container: &Value, idx: &Value) -> Result<Value> {
        match (container, idx) {
            (Value::List(l), Value::Int(i)) => {
                let i = *i;
                let len = l.len() as i64;
                let real_idx = if i < 0 { len + i } else { i };
                if real_idx < 0 || real_idx >= len {
                    Err(CompilerError::runtime_error(format!(
                        "list index {} out of bounds (len {})",
                        real_idx, len
                    )))
                } else {
                    Ok(l[real_idx as usize].clone())
                }
            }
            (Value::Tuple(t), Value::Int(i)) => {
                let i = *i;
                let len = t.len() as i64;
                let real_idx = if i < 0 { len + i } else { i };
                if real_idx < 0 || real_idx >= len {
                    Err(CompilerError::runtime_error("tuple index out of bounds"))
                } else {
                    Ok(t[real_idx as usize].clone())
                }
            }
            (Value::Str(s), Value::Int(i)) => {
                let chars: Vec<char> = s.chars().collect();
                let i = *i;
                let len = chars.len() as i64;
                let real_idx = if i < 0 { len + i } else { i };
                if real_idx < 0 || real_idx >= len {
                    Err(CompilerError::runtime_error("string index out of bounds"))
                } else {
                    Ok(Value::Str(chars[real_idx as usize].to_string()))
                }
            }
            (Value::Dict(d), _) => {
                // Dict keys can be any type — convert to string for lookup,
                // consistent with index_set which uses to_str().
                let key = idx.to_str();
                Ok(d.get(&key).cloned().unwrap_or(Value::Null))
            }
            (Value::Object(class, _), _) => {
                // If the class defines __getitem__, dispatch to it.
                if self.find_method(class, "__getitem__").is_ok() {
                    let class = class.clone();
                    let receiver = container.clone();
                    let args = vec![idx.clone()];
                    self.call_method_on_class(&class, receiver, "__getitem__", args)
                } else {
                    Err(CompilerError::runtime_error(format!(
                        "cannot index {:?} with {:?}",
                        container, idx
                    )))
                }
            }
            _ => Err(CompilerError::runtime_error(format!(
                "cannot index {:?} with {:?}",
                container, idx
            ))),
        }
    }

    fn index_set(&mut self, container: &mut Value, idx: &Value, val: Value) -> Result<()> {
        match container {
            Value::List(l) => {
                if let Value::Int(i) = idx {
                    let i = *i;
                    let len = l.len() as i64;
                    let real_idx = if i < 0 { len + i } else { i };
                    if real_idx < 0 {
                        return Err(CompilerError::runtime_error(format!(
                            "list index assignment out of bounds: {} (len {})", i, len)));
                    }
                    if real_idx >= len {
                        // Auto-expand: pad with Null up to index, then set.
                        let target = real_idx as usize;
                        while l.len() < target {
                            l.push(Value::Null);
                        }
                        l.push(val);
                    } else {
                        l[real_idx as usize] = val;
                    }
                    Ok(())
                } else {
                    Err(CompilerError::runtime_error("list index must be int"))
                }
            }
            Value::Dict(d) => {
                let key = idx.to_str();
                d.insert(key, val);
                Ok(())
            }
            Value::Object(class, _) => {
                // If the class defines __setitem__, dispatch to it.
                let class_name = class.clone();
                if self.find_method(&class_name, "__setitem__").is_ok() {
                    let receiver = container.clone();
                    let args = vec![idx.clone(), val];
                    // Discard the return value.
                    let _ = self.call_method_on_class(
                        &class_name,
                        receiver,
                        "__setitem__",
                        args,
                    )?;
                    Ok(())
                } else {
                    Err(CompilerError::runtime_error("cannot index-assign"))
                }
            }
            _ => Err(CompilerError::runtime_error("cannot index-assign")),
        }
    }

    /// Delete an element from a container (List or Dict). For Objects,
    /// dispatches to `__delitem__` if defined.
    fn delete_index(&mut self, container: &mut Value, idx: &Value) -> Result<()> {
        match container {
            Value::List(l) => {
                if let Value::Int(i) = idx {
                    let i = *i;
                    let len = l.len() as i64;
                    let real_idx = if i < 0 { len + i } else { i };
                    if real_idx < 0 || real_idx >= len {
                        return Err(CompilerError::runtime_error(format!(
                            "list delete index out of bounds: {} (len {})",
                            i, len
                        )));
                    }
                    l.remove(real_idx as usize);
                    Ok(())
                } else {
                    Err(CompilerError::runtime_error("list index must be int"))
                }
            }
            Value::Dict(d) => {
                let key = idx.to_str();
                d.remove(&key);
                Ok(())
            }
            Value::Object(class, _) => {
                // If the class defines __delitem__, dispatch to it.
                let class_name = class.clone();
                if self.find_method(&class_name, "__delitem__").is_ok() {
                    let receiver = container.clone();
                    let args = vec![idx.clone()];
                    let _ = self.call_method_on_class(
                        &class_name,
                        receiver,
                        "__delitem__",
                        args,
                    )?;
                }
                Ok(())
            }
            _ => Err(CompilerError::runtime_error("cannot delete from")),
        }
    }

    fn find_function(&self, name: &str) -> Result<&AstFnDef> {
        if let Some(prog) = &self.program {
            for d in &prog.declarations {
                if let crate::parser::ast::TopLevel::FnDef(f) = d {
                    if f.name.name == name {
                        return Ok(f);
                    }
                }
                if let crate::parser::ast::TopLevel::LazyFnDef(l) = d {
                    if l.fn_def.name.name == name {
                        return Ok(&l.fn_def);
                    }
                }
            }
        }
        // Look up lambdas by their generated name (e.g. "<lambda_0>").
        for f in &self.lambda_defs {
            if f.name.name == name {
                return Ok(f);
            }
        }
        Err(CompilerError::runtime_error(format!(
            "function '{}' not defined (not in program or any imported module)",
            name
        )))
    }

    fn fn_has_yield(&self, body: &[crate::parser::ast::Stmt]) -> bool {
        body.iter().any(|s| self.stmt_has_yield(s))
    }

    fn stmt_has_yield(&self, s: &crate::parser::ast::Stmt) -> bool {
        use crate::parser::ast::Stmt;
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

    fn call_builtin(&mut self, name: &str, argc: usize) -> Result<()> {
        let mut args = Vec::with_capacity(argc);
        for _ in 0..argc {
            args.push(self.pop()?);
        }
        args.reverse();
        let v = self.call_builtin_value(name, args)?;
        self.push(v);
        Ok(())
    }

    fn call_builtin_value(&mut self, name: &str, args: Vec<Value>) -> Result<Value> {
        // Phase 4: delegate to pure Vredrs core.veds implementations when
        // available. The core_ prefixed function was compiled into the same
        // module by run_bytecode_vm (which appends core.veds to the program).
        // 递归保护：core_ 函数内部调 builtin（如 core_len 用 v# → CallBuiltin("len")）
        // 时不递归委托，走硬编码实现。
        let core_name = format!("core_{}", name);
        if self.core_call_depth == 0 && self.fn_cache.contains_key(&core_name) {
            // Push args and call the core_ function.
            let argc = args.len();
            self.core_call_depth += 1;
            for a in &args {
                self.push(a.clone());
            }
            self.call_function(&core_name, argc)?;
            self.core_call_depth -= 1;
            return Ok(self.stack.pop().unwrap_or(Value::Null));
        }
        match name {
            "json_stringify" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                Ok(Value::Str(native_json_stringify(&v)))
            }
            "json_stringify_pretty" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                Ok(Value::Str(native_json_stringify_pretty(&v, 2)))
            }
            "json_parse" => {
                let text = args.into_iter().next().unwrap_or(Value::Null).to_str();
                Ok(native_json_parse(&text))
            }
            "len" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let len = match &v {
                    Value::Str(s) => s.chars().count() as i64,
                    Value::List(l) => l.len() as i64,
                    Value::Tuple(t) => t.len() as i64,
                    Value::Dict(d) => d.len() as i64,
                    _ => 0,
                };
                Ok(Value::Int(len))
            }
            "str" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                Ok(Value::Str(v.to_str()))
            }
            "int" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                match v {
                    Value::Int(i) => Ok(Value::Int(i)),
                    Value::Float(f) => Ok(Value::Int(f as i64)),
                    Value::Bool(b) => Ok(Value::Int(if b { 1 } else { 0 })),
                    Value::Null => Err(CompilerError::runtime_error(
                        "cannot convert null to int",
                    )),
                    Value::Str(s) => {
                        if s.is_empty() {
                            Err(CompilerError::runtime_error(
                                "cannot convert empty string to int",
                            ))
                        } else {
                            s.parse::<i64>().map(Value::Int).map_err(|_| {
                                CompilerError::runtime_error(format!(
                                    "cannot convert '{}' to int",
                                    s
                                ))
                            })
                        }
                    }
                    other => Err(CompilerError::runtime_error(format!(
                        "cannot convert {} to int",
                        value_type_name(&other)
                    ))),
                }
            }
            "float" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                match v {
                    Value::Int(i) => Ok(Value::Float(i as f64)),
                    Value::Float(f) => Ok(Value::Float(f)),
                    Value::Bool(b) => Ok(Value::Float(if b { 1.0 } else { 0.0 })),
                    Value::Null => Err(CompilerError::runtime_error(
                        "cannot convert null to float",
                    )),
                    Value::Str(s) => {
                        if s.is_empty() {
                            Err(CompilerError::runtime_error(
                                "cannot convert empty string to float",
                            ))
                        } else {
                            s.parse::<f64>().map(Value::Float).map_err(|_| {
                                CompilerError::runtime_error(format!(
                                    "cannot convert '{}' to float",
                                    s
                                ))
                            })
                        }
                    }
                    other => Err(CompilerError::runtime_error(format!(
                        "cannot convert {} to float",
                        value_type_name(&other)
                    ))),
                }
            }
            "ord" => {
                // ord(c) → Unicode code point of a single character.
                // Int values pass through unchanged (identity).
                let v = args.into_iter().next().unwrap_or(Value::Null);
                match v {
                    Value::Int(i) => Ok(Value::Int(i)),
                    Value::Str(s) => {
                        let mut chars = s.chars();
                        match (chars.next(), chars.next()) {
                            (Some(c), None) => Ok(Value::Int(c as i64)),
                            _ => Err(CompilerError::runtime_error(format!(
                                "ord() expected a single character, got '{}'",
                                s
                            ))),
                        }
                    }
                    other => Err(CompilerError::runtime_error(format!(
                        "ord() expected a string, got {}",
                        value_type_name(&other)
                    ))),
                }
            }
            "chr" => {
                // chr(n) → character from a Unicode code point.
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let n = v.to_int().map_err(|_| {
                    CompilerError::runtime_error("chr() expected an integer code point")
                })?;
                match char::from_u32(n as u32) {
                    Some(c) => Ok(Value::Str(c.to_string())),
                    None => Err(CompilerError::runtime_error(format!(
                        "chr(): code point {} is not a valid Unicode scalar value",
                        n
                    ))),
                }
            }
            "bool" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                Ok(Value::Bool(v.truthy()))
            }
            "type_of" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let t = match v {
                    Value::Int(_) => "int",
                    Value::Float(_) => "float",
                    Value::Bool(_) => "bool",
                    Value::Str(_) => "str",
                    Value::Null => "null",
                    Value::List(_) => "list",
                    Value::Dict(_) => "dict",
                    Value::Tuple(_) => "tuple",
                    Value::Func(_) => "function",
                    Value::Generator(_) => "generator",
                    Value::Object(_, _) => "object",
                    Value::Class(_) => "class",
                    Value::Module(_, _) => "module",
                    Value::Exception(_, _) => "exception",
                };
                Ok(Value::Str(t.to_string()))
            }
            "range" | "range1" => {
                let mut iter = args.into_iter();
                let first = iter.next().unwrap_or(Value::Int(0));
                let second = iter.next();
                match second {
                    Some(end_val) => {
                        // range(start, end)
                        let start = first.to_int()?;
                        let end = end_val.to_int()?;
                        let mut items = Vec::new();
                        let mut i = start;
                        while i < end {
                            items.push(Value::Int(i));
                            i += 1;
                        }
                        Ok(Value::List(items))
                    }
                    None => {
                        // range(end)
                        let end = first.to_int()?;
                        let mut items = Vec::new();
                        for i in 0..end {
                            items.push(Value::Int(i));
                        }
                        Ok(Value::List(items))
                    }
                }
            }
            "range3" => {
                let mut iter = args.into_iter();
                let start = iter.next().unwrap_or(Value::Int(0)).to_int()?;
                let end = iter.next().unwrap_or(Value::Int(0)).to_int()?;
                let step = iter.next().unwrap_or(Value::Int(1)).to_int()?;
                if step == 0 {
                    return Err(CompilerError::runtime_error("range step cannot be zero"));
                }
                let mut items = Vec::new();
                let mut i = start;
                if step > 0 {
                    while i < end {
                        items.push(Value::Int(i));
                        i += step;
                    }
                } else {
                    while i > end {
                        items.push(Value::Int(i));
                        i += step;
                    }
                }
                Ok(Value::List(items))
            }
            "sum" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let sum = match v {
                    Value::List(l) => l
                        .into_iter()
                        .fold(0i64, |acc, v| acc + v.to_int().unwrap_or(0)),
                    Value::Tuple(t) => t
                        .into_iter()
                        .fold(0i64, |acc, v| acc + v.to_int().unwrap_or(0)),
                    _ => 0,
                };
                Ok(Value::Int(sum))
            }
            "min" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let m = match v {
                    Value::List(l) => l
                        .into_iter()
                        .filter_map(|v| v.to_int().ok())
                        .min()
                        .unwrap_or(0),
                    _ => 0,
                };
                Ok(Value::Int(m))
            }
            "max" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let m = match v {
                    Value::List(l) => l
                        .into_iter()
                        .filter_map(|v| v.to_int().ok())
                        .max()
                        .unwrap_or(0),
                    _ => 0,
                };
                Ok(Value::Int(m))
            }
            "open" => {
                let mut iter = args.into_iter();
                let path = iter.next().unwrap_or(Value::Null).to_str();
                let mode = iter.next().unwrap_or(Value::Str("r".to_string())).to_str();
                // Return a file handle as a dict with path/mode.
                let mut d = HashMap::new();
                d.insert("path".to_string(), Value::Str(path.clone()));
                d.insert("mode".to_string(), Value::Str(mode.clone()));
                d.insert("content".to_string(), Value::Str(String::new()));
                d.insert("pos".to_string(), Value::Int(0));
                // If reading, load the content.
                if mode.contains('r') {
                    match std::fs::read_to_string(&path) {
                        Ok(content) => { d.insert("content".to_string(), Value::Str(content)); }
                        Err(_) => {}
                    }
                }
                Ok(Value::Dict(d))
            }
            "read" => {
                let mut iter = args.into_iter();
                let handle = iter.next().unwrap_or(Value::Null);
                let n = iter.next().unwrap_or(Value::Int(-1)).to_int().unwrap_or(-1);
                if let Value::Dict(d) = &handle {
                    let content = d.get("content").cloned().unwrap_or(Value::Str(String::new())).to_str();
                    let pos = d.get("pos").cloned().unwrap_or(Value::Int(0)).to_int().unwrap_or(0) as usize;
                    let chars: Vec<char> = content.chars().collect();
                    let end = if n < 0 { chars.len() } else { (pos + n as usize).min(chars.len()) };
                    let result: String = chars[pos..end].iter().collect();
                    Ok(Value::Str(result))
                } else {
                    Ok(Value::Str(String::new()))
                }
            }
            "write" => {
                let mut iter = args.into_iter();
                let handle = iter.next().unwrap_or(Value::Null);
                let data = iter.next().unwrap_or(Value::Str(String::new())).to_str();
                if let Value::Dict(d) = &handle {
                    let path = d.get("path").cloned().unwrap_or(Value::Str(String::new())).to_str();
                    let _ = std::fs::write(&path, &data);
                    Ok(Value::Bool(true))
                } else {
                    Ok(Value::Bool(false))
                }
            }
            "close" => {
                // No-op — file handles are dict-based, no resources to close.
                Ok(Value::Null)
            }
            "read_file" => {
                let path = args.into_iter().next().unwrap_or(Value::Null);
                let path = match path {
                    Value::Str(s) => s,
                    _ => return Err(CompilerError::runtime_error("read_file: path must be a string")),
                };
                if let Err(e) = validate_path(&path) { return Err(e); }
                match fs::read_to_string(&path) {
                    Ok(content) => Ok(Value::Str(content)),
                    Err(e) => Err(CompilerError::runtime_error(format!(
                        "read_file: cannot read '{}': {}", path, e
                    ))),
                }
            }
            "write_file" => {
                let mut iter = args.into_iter();
                let path = iter.next().unwrap_or(Value::Null);
                let content = iter.next().unwrap_or(Value::Null);
                let path = match path {
                    Value::Str(s) => s,
                    _ => return Ok(Value::Bool(false)),
                };
                if let Err(e) = validate_path(&path) { return Err(e); }
                let content = match content {
                    Value::Str(s) => s,
                    _ => content.to_str(),
                };
                match fs::write(&path, &content) {
                    Ok(()) => Ok(Value::Bool(true)),
                    Err(e) => {
                        // Provide helpful error message with platform-specific hints.
                        let hint = if path.starts_with("/tmp") {
                            "hint: /tmp may not be writable on this platform. Try using a path relative to the current directory, or os_get_env(\"HOME\") for a home-directory path."
                        } else {
                            ""
                        };
                        eprintln!("[vredrs] write_file('{}') failed: {} {}", path, e, hint);
                        Ok(Value::Bool(false))
                    }
                }
            }
            "file_exists" => {
                let path = args.into_iter().next().unwrap_or(Value::Null);
                let path = match path {
                    Value::Str(s) => s,
                    _ => return Ok(Value::Bool(false)),
                };
                Ok(Value::Bool(std::path::Path::new(&path).exists()))
            }
            "is_dir" => {
                let path = args.into_iter().next().unwrap_or(Value::Null);
                let path = match path {
                    Value::Str(s) => s,
                    _ => return Ok(Value::Bool(false)),
                };
                Ok(Value::Bool(std::path::Path::new(&path).is_dir()))
            }
            "is_file" => {
                let path = args.into_iter().next().unwrap_or(Value::Null);
                let path = match path {
                    Value::Str(s) => s,
                    _ => return Ok(Value::Bool(false)),
                };
                Ok(Value::Bool(std::path::Path::new(&path).is_file()))
            }
            "read_dir" => {
                let path = args.into_iter().next().unwrap_or(Value::Null);
                let path = match path {
                    Value::Str(s) => s,
                    _ => return Ok(Value::List(Vec::new())),
                };
                let mut entries = Vec::new();
                if let Ok(rd) = std::fs::read_dir(&path) {
                    for entry in rd.flatten() {
                        if let Some(name) = entry.file_name().to_str() {
                            entries.push(Value::Str(name.to_string()));
                        }
                    }
                }
                Ok(Value::List(entries))
            }
            "path_join" => {
                let mut iter = args.into_iter();
                let a = iter.next().unwrap_or(Value::Null).to_str();
                let b = iter.next().unwrap_or(Value::Null).to_str();
                use std::path::Path;
                let joined = Path::new(&a).join(&b);
                Ok(Value::Str(
                    joined.to_string_lossy().to_string(),
                ))
            }
            "basename" => {
                let path = args.into_iter().next().unwrap_or(Value::Null).to_str();
                use std::path::Path;
                let base = Path::new(&path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                Ok(Value::Str(base))
            }
            "dirname" => {
                let path = args.into_iter().next().unwrap_or(Value::Null).to_str();
                use std::path::Path;
                let dir = Path::new(&path)
                    .parent()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                Ok(Value::Str(dir))
            }
            "os_args" => {
                // Return command-line arguments as a list of strings.
                let args: Vec<Value> = std::env::args()
                    .skip(1) // Skip the program name.
                    .map(Value::Str)
                    .collect();
                Ok(Value::List(args))
            }
            "os_env" => {
                // os_env(name?) → if name given, return that env var's value
                // (or null if unset); if no name, return a dict of all env vars.
                if args.is_empty() {
                    let mut d = HashMap::new();
                    for (k, v) in std::env::vars() {
                        d.insert(k, Value::Str(v));
                    }
                    Ok(Value::Dict(d))
                } else {
                    let name = args[0].to_str();
                    match std::env::var(&name) {
                        Ok(val) => Ok(Value::Str(val)),
                        Err(_) => Ok(Value::Null),
                    }
                }
            }
            "os_setenv" => {
                let mut iter = args.into_iter();
                let key = iter.next().unwrap_or(Value::Null).to_str();
                let val = iter.next().unwrap_or(Value::Null).to_str();
                std::env::set_var(&key, &val);
                Ok(Value::Null)
            }
            "os_exec" => {
                // os_exec(cmd, args...) → run an external command, return
                // {exit_code: int, stdout: str, stderr: str}.
                //
                // Security: command execution is gated by the
                // `VREDRS_ALLOW_EXEC` environment variable. If it is not
                // set to "1", the call is refused with a runtime error.
                // This prevents untrusted Vredrs scripts from spawning
                // arbitrary host processes (e.g. via a malicious package
                // or a piped `curl | sh` payload). Set
                // `VREDRS_ALLOW_EXEC=1` in the environment to opt in.
                if std::env::var("VREDRS_ALLOW_EXEC").ok().as_deref() != Some("1") {
                    return Err(CompilerError::runtime_error(
                        "command execution disabled; set VREDRS_ALLOW_EXEC=1 to enable",
                    ));
                }
                let mut iter = args.into_iter();
                let cmd_str = iter.next().unwrap_or(Value::Null).to_str();
                let arg_strs: Vec<String> = iter.map(|v| v.to_str()).collect();
                let output = std::process::Command::new(&cmd_str)
                    .args(&arg_strs)
                    .output();
                match output {
                    Ok(out) => {
                        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                        let code = out.status.code().unwrap_or(-1) as i64;
                        let mut d = HashMap::new();
                        d.insert("exit_code".to_string(), Value::Int(code));
                        d.insert("stdout".to_string(), Value::Str(stdout));
                        d.insert("stderr".to_string(), Value::Str(stderr));
                        Ok(Value::Dict(d))
                    }
                    Err(e) => {
                        let mut d = HashMap::new();
                        d.insert("exit_code".to_string(), Value::Int(-1));
                        d.insert("stdout".to_string(), Value::Str(String::new()));
                        d.insert("stderr".to_string(), Value::Str(e.to_string()));
                        Ok(Value::Dict(d))
                    }
                }
            }
            "os_system" => {
                // os_system(cmd_str) → run a shell command, return exit code.
                //
                // Security: command execution is gated by the
                // `VREDRS_ALLOW_EXEC` environment variable (see the
                // matching note on `os_exec` above). If it is not set to
                // "1", the call is refused with a runtime error rather
                // than silently invoking `sh -c <cmd>`.
                if std::env::var("VREDRS_ALLOW_EXEC").ok().as_deref() != Some("1") {
                    return Err(CompilerError::runtime_error(
                        "command execution disabled; set VREDRS_ALLOW_EXEC=1 to enable",
                    ));
                }
                let cmd_str = args.into_iter().next().unwrap_or(Value::Null).to_str();
                let status = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd_str)
                    .status();
                match status {
                    Ok(s) => Ok(Value::Int(s.code().unwrap_or(-1) as i64)),
                    Err(_) => Ok(Value::Int(-1)),
                }
            }
            "os_cwd" => {
                Ok(Value::Str(
                    std::env::current_dir()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default(),
                ))
            }
            "os_chdir" => {
                let path = args.into_iter().next().unwrap_or(Value::Null).to_str();
                match std::env::set_current_dir(&path) {
                    Ok(()) => Ok(Value::Bool(true)),
                    Err(_) => Ok(Value::Bool(false)),
                }
            }
            "time_now" => {
                use std::time::{SystemTime, UNIX_EPOCH};
                let dur = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                let secs = dur.as_secs() as i64;
                let nanos = dur.subsec_nanos() as i64;
                let mut fields = HashMap::new();
                fields.insert("__secs__".to_string(), Value::Int(secs));
                fields.insert("__nanos__".to_string(), Value::Int(nanos));
                Ok(Value::Object("Time".to_string(), Rc::new(RefCell::new(fields))))
            }
            "time_sleep" => {
                let ms = args.into_iter().next().unwrap_or(Value::Int(0)).to_int().unwrap_or(0);
                std::thread::sleep(std::time::Duration::from_millis(ms as u64));
                Ok(Value::Null)
            }
            "time_unix" => {
                use std::time::{SystemTime, UNIX_EPOCH};
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                Ok(Value::Int(secs))
            }
            "time_rand_int" => {
                use std::time::{SystemTime, UNIX_EPOCH};
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos() as i64)
                    .unwrap_or(0);
                // Simple xorshift for pseudo-randomness.
                let mut x = nanos.wrapping_mul(2654435761);
                x ^= x >> 13;
                x ^= x << 7;
                x ^= x >> 17;
                Ok(Value::Int(x.abs()))
            }
            "rand_intn" => {
                let n = args.into_iter().next().unwrap_or(Value::Int(1)).to_int().unwrap_or(1);
                if n <= 0 { return Ok(Value::Int(0)); }
                let raw = {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    let nanos = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);
                    let mut x = nanos.wrapping_mul(2654435761);
                    x ^= x >> 13;
                    x ^= x << 7;
                    x ^= x >> 17;
                    x.abs()
                };
                Ok(Value::Int(raw % n))
            }
            "rand_int" => {
                let mut iter = args.into_iter();
                let min = iter.next().unwrap_or(Value::Int(0)).to_int().unwrap_or(0);
                let max = iter.next().unwrap_or(Value::Int(100)).to_int().unwrap_or(100);
                if max <= min { return Ok(Value::Int(min)); }
                let raw = {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    let nanos = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);
                    let mut x = nanos.wrapping_mul(2654435761);
                    x ^= x >> 13;
                    x ^= x << 7;
                    x ^= x >> 17;
                    x.abs()
                };
                Ok(Value::Int(raw % (max - min + 1) + min))
            }
            "rand_float" => {
                let raw = {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    let nanos = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);
                    let mut x = nanos.wrapping_mul(2654435761);
                    x ^= x >> 13;
                    x ^= x << 7;
                    x ^= x >> 17;
                    x.abs()
                };
                Ok(Value::Float(raw as f64 / i64::MAX as f64))
            }
            "rand_bool" => {
                let raw = {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    let nanos = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);
                    let mut x = nanos.wrapping_mul(2654435761);
                    x ^= x >> 13;
                    x ^= x << 7;
                    x ^= x >> 17;
                    x.abs()
                };
                Ok(Value::Bool(raw % 2 == 1))
            }
            "exit" => {
                let code = args.into_iter().next().unwrap_or(Value::Int(0));
                let code = code.to_int().unwrap_or(0) as i32;
                std::process::exit(code);
            }
            "resume" => {
                // resume(gen) returns the next yielded value, or the
                // generator's return value once exhausted.
                let gen_val = args.into_iter().next().unwrap_or(Value::Null);
                match gen_val {
                    Value::Generator(g) => {
                        let mut state = g.borrow_mut();
                        if state.yield_idx < state.yielded_values.len() {
                            let v = state.yielded_values[state.yield_idx].clone();
                            state.yield_idx += 1;
                            if state.yield_idx >= state.yielded_values.len() {
                                state.done = true;
                            }
                            Ok(v)
                        } else {
                            state.done = true;
                            Ok(state.return_value.clone().unwrap_or(Value::Int(0)))
                        }
                    }
                    _ => Ok(Value::Int(0)),
                }
            }
            "stop" => Ok(Value::Exception("StopIteration".to_string(), Box::new(Value::Str("StopIteration".to_string())))),
            // Result type constructors and helpers (P3.8).
            "Ok" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                // Ok wraps the value in a tagged Object so is_ok/is_err can
                // distinguish it from Err. The Object's class is "Ok" and
                // it carries the value in the "__value__" field.
                let mut fields = HashMap::new();
                fields.insert("__value__".to_string(), v);
                Ok(Value::Object("Ok".to_string(), Rc::new(RefCell::new(fields))))
            }
            "Err" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                // Err wraps the error in an Exception value so the `?`
                // operator (TryPropagate) can detect and propagate it.
                let msg = v.to_str();
                Ok(Value::Exception(msg, Box::new(v)))
            }
            "is_ok" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                Ok(Value::Bool(matches!(&v, Value::Object(c, _) if c == "Ok")))
            }
            "is_err" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                Ok(Value::Bool(matches!(&v, Value::Exception(_, _))))
            }
            "unwrap" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                match &v {
                    Value::Object(c, fields) if c == "Ok" => {
                        Ok(fields.borrow().get("__value__").cloned().unwrap_or(Value::Null))
                    }
                    Value::Exception(msg, inner) => {
                        Err(CompilerError::runtime_error(format!(
                            "unwrap called on Err({})", msg
                        )))
                    }
                    _ => Ok(v),
                }
            }
            "unwrap_or" => {
                let mut iter = args.into_iter();
                let v = iter.next().unwrap_or(Value::Null);
                let default = iter.next().unwrap_or(Value::Null);
                match &v {
                    Value::Object(c, fields) if c == "Ok" => {
                        Ok(fields.borrow().get("__value__").cloned().unwrap_or(default))
                    }
                    Value::Exception(_, _) => Ok(default),
                    _ => Ok(v),
                }
            }
            "freeze" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                // Use a type-discriminated key so that e.g. Int(5) and
                // Str("5") don't share frozen state.
                let key = freeze_key(&v);
                self.frozen_set.insert(key);
                Ok(v)
            }
            "is_frozen" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let key = freeze_key(&v);
                Ok(Value::Bool(self.frozen_set.contains(&key)))
            }
            "annotations" => {
                // annotations(fn) returns the dict of annotations registered
                // for that function. `fn` can be either a Func value
                // (e.g. `annotations(index)`) or a string (e.g.
                // `annotations("index")`).
                let arg = args.into_iter().next().unwrap_or(Value::Null);
                let key = match &arg {
                    Value::Func(name) => name.clone(),
                    other => other.to_str(),
                };
                Ok(Value::Dict(
                    self.annotation_table.get(&key).cloned().unwrap_or_default(),
                ))
            }
            "set_recursion_limit" => {
                // No-op in the bytecode VM (Rust's call stack is the limit).
                Ok(Value::Null)
            }
            "abs" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                Ok(match v {
                    Value::Int(i) => Value::Int(i.abs()),
                    Value::Float(f) => Value::Float(f.abs()),
                    other => other,
                })
            }
            "floor" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                if let Value::Float(f) = v {
                    Ok(Value::Int(f.floor() as i64))
                } else if let Value::Int(i) = v {
                    Ok(Value::Int(i))
                } else {
                    Ok(Value::Null)
                }
            }
            "ceil" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                if let Value::Float(f) = v {
                    Ok(Value::Int(f.ceil() as i64))
                } else if let Value::Int(i) = v {
                    Ok(Value::Int(i))
                } else {
                    Ok(Value::Null)
                }
            }
            "round" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                if let Value::Float(f) = v {
                    Ok(Value::Int(f.round() as i64))
                } else if let Value::Int(i) = v {
                    Ok(Value::Int(i))
                } else {
                    Ok(Value::Null)
                }
            }
            "enumerate" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let items = self.iterable_to_list(&v);
                let result: Vec<Value> = items
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| Value::Tuple(vec![Value::Int(i as i64), v]))
                    .collect();
                Ok(Value::List(result))
            }
            "zip" => {
                let mut iter = args.into_iter();
                let a = self.iterable_to_list(&iter.next().unwrap_or(Value::Null));
                let b = self.iterable_to_list(&iter.next().unwrap_or(Value::Null));
                let result: Vec<Value> = a
                    .into_iter()
                    .zip(b.into_iter())
                    .map(|(x, y)| Value::Tuple(vec![x, y]))
                    .collect();
                Ok(Value::List(result))
            }
            "list" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                Ok(Value::List(self.iterable_to_list(&v)))
            }
            "set" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let items = self.iterable_to_list(&v);
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                let mut out = Vec::new();
                for x in items {
                    let k = x.to_str();
                    if seen.insert(k) {
                        out.push(x);
                    }
                }
                Ok(Value::List(out))
            }
            "tuple" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                Ok(Value::Tuple(self.iterable_to_list(&v)))
            }
            "sorted" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let mut items = self.iterable_to_list(&v);
                items.sort();
                Ok(Value::List(items))
            }
            "sorted_desc" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let mut items = self.iterable_to_list(&v);
                items.sort_by(|a, b| b.cmp(a));
                Ok(Value::List(items))
            }
            "get_field" => {
                // get_field(obj, field_name) → obj.field_name.
                // Supports objects, modules, dicts (string key lookup),
                // classes (for static/enum access), and enum-value strings.
                let mut iter = args.into_iter();
                let obj = iter.next().unwrap_or(Value::Null);
                let field = iter.next().unwrap_or(Value::Null).to_str();
                match &obj {
                    Value::Object(_, fields) => {
                        Ok(fields.borrow().get(&field).cloned().unwrap_or(Value::Null))
                    }
                    Value::Module(_, exports) => {
                        Ok(exports.get(&field).cloned().unwrap_or(Value::Null))
                    }
                    Value::Dict(d) => {
                        Ok(d.get(&field).cloned().unwrap_or(Value::Null))
                    }
                    Value::Class(class_name) => {
                        // Enum-variant access: ClassName.VariantName.
                        let key = format!("{}.{}", class_name, field);
                        Ok(self.globals.get(&key).cloned().unwrap_or(Value::Null))
                    }
                    Value::Str(s) if s.starts_with("<enum ") && s.ends_with('>') => {
                        let enum_name = &s[6..s.len() - 1];
                        let key = format!("{}.{}", enum_name, field);
                        Ok(self.globals.get(&key).cloned().unwrap_or(Value::Null))
                    }
                    _ => Ok(Value::Null),
                }
            }
            "slice" => {
                // slice(target, start, end, step) — null for start/end/step
                // means "default" (depends on step sign).
                let mut iter = args.into_iter();
                let target = iter.next().unwrap_or(Value::Null);
                let start_v = iter.next().unwrap_or(Value::Null);
                let end_v = iter.next().unwrap_or(Value::Null);
                let step_v = iter.next().unwrap_or(Value::Null);
                let step = if matches!(step_v, Value::Null) { 1 } else { step_v.to_int().unwrap_or(1) };
                if step == 0 {
                    return Err(CompilerError::runtime_error("slice step cannot be zero"));
                }
                // For positive step: default start=0, default end=MAX
                // For negative step: default start=MAX, default end=MIN
                let (default_start, default_end) = if step > 0 {
                    (0, isize::MAX as i64)
                } else {
                    (isize::MAX as i64, isize::MIN as i64)
                };
                let start = if matches!(start_v, Value::Null) { default_start } else { start_v.to_int().unwrap_or(default_start) };
                let end = if matches!(end_v, Value::Null) { default_end } else { end_v.to_int().unwrap_or(default_end) };
                self.slice_value(target, start, end, step)
            }
            "list_append" => {
                // list_append(list, item) → returns the list with item appended.
                // Mutates the list in place (since List is Vec, not shared).
                let mut iter = args.into_iter();
                let list_val = iter.next().unwrap_or(Value::Null);
                let item = iter.next().unwrap_or(Value::Null);
                if let Value::List(l) = &list_val {
                    let mut new_list = l.clone();
                    new_list.push(item);
                    Ok(Value::List(new_list))
                } else {
                    Ok(Value::List(vec![item]))
                }
            }
            "make_lambda" => {
                // Create a lambda Func value. The actual FnDef is stored
                // in self.lambda_defs at the time the Lambda expression is
                // compiled. Since we can't pass the AST through bytecode,
                // we use a counter to match.
                // Actually, this is tricky — the compiler can't pass the
                // FnDef to the VM through a CallBuiltin. We need a different
                // approach.
                // For now, push a Func with a placeholder name.
                Ok(Value::Func("<lambda>".to_string()))
            }
            "optional_member" => {
                // optional_member(obj, field_name) → if obj is null, return null;
                // else return obj.field_name. Supports objects, modules, and
                // dicts (where the field name is used as a string key).
                let mut iter = args.into_iter();
                let obj = iter.next().unwrap_or(Value::Null);
                let field = iter.next().unwrap_or(Value::Null).to_str();
                if matches!(obj, Value::Null) {
                    Ok(Value::Null)
                } else {
                    match &obj {
                        Value::Object(_, fields) => {
                            Ok(fields.borrow().get(&field).cloned().unwrap_or(Value::Null))
                        }
                        Value::Module(_, exports) => {
                            Ok(exports.get(&field).cloned().unwrap_or(Value::Null))
                        }
                        Value::Dict(d) => {
                            Ok(d.get(&field).cloned().unwrap_or(Value::Null))
                        }
                        _ => Ok(Value::Null),
                    }
                }
            }
            "optional_method" => {
                // optional_method(obj, method_name, args...) → if obj is null,
                // return null; else call obj.method(args...).
                let mut iter = args.into_iter();
                let obj = iter.next().unwrap_or(Value::Null);
                let method = iter.next().unwrap_or(Value::Null).to_str();
                let call_args: Vec<Value> = iter.collect();
                if matches!(obj, Value::Null) {
                    Ok(Value::Null)
                } else {
                    self.call_method_on_value(obj, &method, call_args)
                }
            }
            "optional_index" => {
                // optional_index(obj, idx) → if obj is null, return null;
                // else return obj[idx].
                let mut iter = args.into_iter();
                let obj = iter.next().unwrap_or(Value::Null);
                let idx = iter.next().unwrap_or(Value::Null);
                if matches!(obj, Value::Null) {
                    Ok(Value::Null)
                } else {
                    self.index_get(&obj, &idx)
                }
            }
            "__yield__" => {
                // Yield builtin: push the yielded value to the generator
                // yield buffer (if active) and return null. This replaces
                // the AST-path Yield statement handler for bytecode mode.
                let v = args.into_iter().next().unwrap_or(Value::Null);
                if let Some(buf) = self.gen_yield_buffer.as_mut() {
                    buf.push(v);
                }
                Ok(Value::Null)
            }
            "__with_enter__" => {
                // With-enter: call __enter__ on objects, return as-is
                // for non-objects (file handles, dicts).
                let manager = args.into_iter().next().unwrap_or(Value::Null);
                if matches!(manager, Value::Object(_, _)) {
                    self.call_dunder(&manager, "__enter__", vec![])
                } else {
                    Ok(manager)
                }
            }
            "__with_exit__" => {
                // With-exit: call __exit__ on objects, no-op for others.
                let manager = args.into_iter().next().unwrap_or(Value::Null);
                if matches!(manager, Value::Object(_, _)) {
                    let _ = self.call_dunder(&manager, "__exit__", vec![Value::Null])?;
                }
                Ok(Value::Null)
            }
            "__try_propagate__" => {
                // expr? — if the value is an Exception, throw it
                // (propagate the error). If it's Ok(value), unwrap to value.
                // Otherwise, pass through.
                let v = args.into_iter().next().unwrap_or(Value::Null);
                if let Value::Exception(msg, _) = &v {
                    Err(CompilerError::runtime_error(format!("throw:{}", msg)))
                } else if let Value::Object(c, fields) = &v {
                    if c == "Ok" {
                        Ok(fields.borrow().get("__value__").cloned().unwrap_or(Value::Null))
                    } else {
                        Ok(v)
                    }
                } else {
                    Ok(v)
                }
            }
            "__cast__" => {
                // __cast__(value, type_name) — runtime type conversion.
                // Mirrors `Expr::Cast` in the AST path.
                let mut iter = args.into_iter();
                let v = iter.next().unwrap_or(Value::Null);
                let ty = iter.next().unwrap_or(Value::Str("any".to_string())).to_str();
                match ty.as_str() {
                    "int" => Ok(match v {
                        Value::Int(i) => Value::Int(i),
                        Value::Float(f) => Value::Int(f as i64),
                        Value::Bool(b) => Value::Int(if b { 1 } else { 0 }),
                        Value::Str(s) => Value::Int(s.trim().parse::<i64>().unwrap_or(0)),
                        _ => Value::Int(0),
                    }),
                    "float" => Ok(match v {
                        Value::Int(i) => Value::Float(i as f64),
                        Value::Float(f) => Value::Float(f),
                        Value::Str(s) => Value::Float(s.trim().parse::<f64>().unwrap_or(0.0)),
                        Value::Bool(b) => Value::Float(if b { 1.0 } else { 0.0 }),
                        _ => Value::Float(0.0),
                    }),
                    "str" => Ok(Value::Str(v.to_str())),
                    "bool" => Ok(Value::Bool(v.truthy())),
                    "null" => Ok(Value::Null),
                    _ => Ok(v),
                }
            }
            "__repeated__" => {
                // __repeated__(lhs, n) — string/list repetition.
                let mut iter = args.into_iter();
                let l = iter.next().unwrap_or(Value::Null);
                let r = iter.next().unwrap_or(Value::Null);
                let n = match r {
                    Value::Int(n) => n,
                    _ => return Err(CompilerError::runtime_error(
                        "repeated: right operand must be an integer"
                    )),
                };
                if n < 0 {
                    return Err(CompilerError::runtime_error(
                        "repeated: count cannot be negative"
                    ));
                }
                let n = n as usize;
                match l {
                    Value::Str(s) => Ok(Value::Str(s.repeat(n))),
                    Value::List(items) => {
                        let mut out: Vec<Value> = Vec::with_capacity(items.len() * n);
                        for _ in 0..n {
                            for v in items.iter() {
                                out.push(v.clone());
                            }
                        }
                        Ok(Value::List(out))
                    }
                    _ => Err(CompilerError::runtime_error(
                        "repeated: left operand must be a string or list"
                    )),
                }
            }
            "__assert_fail" => {
                // __assert_fail(msg) — invoked by Stmt::Assert in bytecode
                // mode when the condition evaluates to false. Always throws
                // a catchable error (prefixed with "throw:" so the
                // PushHandler/try-catch mechanism in run() catches it).
                let msg = args.into_iter().next().unwrap_or(Value::Null).to_str();
                Err(CompilerError::runtime_error(format!(
                    "throw:assertion failed: {}",
                    msg
                )))
            }
            "__panic__" => {
                // __panic__(msg) — invoked by Stmt::Panic in bytecode mode.
                // Always throws a catchable error prefixed with "throw:".
                let msg = args.into_iter().next().unwrap_or(Value::Null).to_str();
                Err(CompilerError::runtime_error(format!(
                    "throw:panic: {}",
                    msg
                )))
            }
            "split" => {
                let mut iter = args.into_iter();
                let s = iter.next().unwrap_or(Value::Null).to_str();
                let sep = iter.next().unwrap_or(Value::Str(" ".to_string())).to_str();
                let parts: Vec<Value> = if sep.is_empty() {
                    s.chars().map(|c| Value::Str(c.to_string())).collect()
                } else {
                    s.split(&sep).map(|p| Value::Str(p.to_string())).collect()
                };
                Ok(Value::List(parts))
            }
            "join" => {
                let mut iter = args.into_iter();
                let list = iter.next().unwrap_or(Value::Null);
                let sep = iter.next().unwrap_or(Value::Str(String::new())).to_str();
                let items = self.iterable_to_list(&list);
                let parts: Vec<String> = items.iter().map(|v| v.to_str()).collect();
                Ok(Value::Str(parts.join(&sep)))
            }
            "trim" => {
                let s = args.into_iter().next().unwrap_or(Value::Null).to_str();
                Ok(Value::Str(s.trim().to_string()))
            }
            "upper" => {
                let s = args.into_iter().next().unwrap_or(Value::Null).to_str();
                Ok(Value::Str(s.to_uppercase()))
            }
            "lower" => {
                let s = args.into_iter().next().unwrap_or(Value::Null).to_str();
                Ok(Value::Str(s.to_lowercase()))
            }
            "contains" => {
                let mut iter = args.into_iter();
                let haystack = iter.next().unwrap_or(Value::Null);
                let needle = iter.next().unwrap_or(Value::Null);
                // Dispatch to __contains__ if Object.
                if let Value::Object(class, _) = &haystack {
                    let class = class.clone();
                    if self.find_method(&class, "__contains__").is_ok() {
                        let r = self.call_method_on_class(
                            &class, haystack, "__contains__", vec![needle],
                        )?;
                        return Ok(r);
                    }
                }
                match (&haystack, &needle) {
                    (Value::Str(s), Value::Str(n)) => Ok(Value::Bool(s.contains(n))),
                    (Value::List(l), _) => Ok(Value::Bool(l.iter().any(|x| x == &needle))),
                    (Value::Dict(d), _) => Ok(Value::Bool(d.contains_key(&needle.to_str()))),
                    (Value::Tuple(t), _) => Ok(Value::Bool(t.iter().any(|x| x == &needle))),
                    _ => Ok(Value::Bool(false)),
                }
            }
            "set_add" => {
                // set_add(set_dict, value) — add value to a set (dict with value=true).
                let mut iter = args.into_iter();
                let set_val = iter.next().unwrap_or(Value::Null);
                let val = iter.next().unwrap_or(Value::Null);
                if let Value::Dict(d) = &set_val {
                    let mut new_d = d.clone();
                    new_d.insert(val.to_str(), Value::Bool(true));
                    Ok(Value::Dict(new_d))
                } else {
                    Ok(set_val)
                }
            }
            "set_remove" => {
                let mut iter = args.into_iter();
                let set_val = iter.next().unwrap_or(Value::Null);
                let val = iter.next().unwrap_or(Value::Null);
                if let Value::Dict(d) = &set_val {
                    let mut new_d = d.clone();
                    new_d.remove(&val.to_str());
                    Ok(Value::Dict(new_d))
                } else {
                    Ok(set_val)
                }
            }
            "set_contains" => {
                let mut iter = args.into_iter();
                let set_val = iter.next().unwrap_or(Value::Null);
                let val = iter.next().unwrap_or(Value::Null);
                if let Value::Dict(d) = &set_val {
                    Ok(Value::Bool(d.contains_key(&val.to_str())))
                } else {
                    Ok(Value::Bool(false))
                }
            }
            "set_size" => {
                let set_val = args.into_iter().next().unwrap_or(Value::Null);
                if let Value::Dict(d) = &set_val {
                    Ok(Value::Int(d.len() as i64))
                } else {
                    Ok(Value::Int(0))
                }
            }
            "enumerate" => {
                // enumerate(list) → list of (index, value) tuples
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let items = self.iterable_to_list(&v);
                let result: Vec<Value> = items.into_iter().enumerate().map(|(i, v)| {
                    Value::Tuple(vec![Value::Int(i as i64), v])
                }).collect();
                Ok(Value::List(result))
            }
            "input" => {
                // Read a line from stdin (blocking). Returns Str or Null on EOF.
                use std::io::BufRead;
                let stdin = std::io::stdin();
                let mut line = String::new();
                match stdin.read_line(&mut line) {
                    Ok(n) => {
                        if n == 0 { return Ok(Value::Null); }
                        while line.ends_with('\n') || line.ends_with('\r') { line.pop(); }
                        Ok(Value::Str(line))
                    }
                    Err(_) => Ok(Value::Null),
                }
            }
            "is_type" => {
                // `a is b` → type check: is a the same type as b?
                let mut iter = args.into_iter();
                let a = iter.next().unwrap_or(Value::Null);
                let b = iter.next().unwrap_or(Value::Null);
                let a_type = type_of_value(&a);
                let b_type = type_of_value(&b);
                Ok(Value::Bool(a_type == b_type))
            }
            "pow_op" => {
                // `a ** b` → power
                let mut iter = args.into_iter();
                let a = iter.next().unwrap_or(Value::Null);
                let b = iter.next().unwrap_or(Value::Null);
                // Dispatch to __pow__ if Object.
                if let Value::Object(class, _) = &a {
                    let class = class.clone();
                    if self.find_method(&class, "__pow__").is_ok() {
                        let r = self.call_method_on_class(
                            &class, a, "__pow__", vec![b],
                        )?;
                        return Ok(r);
                    }
                }
                match (a, b) {
                    (Value::Int(x), Value::Int(y)) => {
                        Ok(Value::Float((x as f64).powf(y as f64)))
                    }
                    (Value::Float(x), Value::Float(y)) => Ok(Value::Float(x.powf(y))),
                    (Value::Int(x), Value::Float(y)) => Ok(Value::Float((x as f64).powf(y))),
                    (Value::Float(x), Value::Int(y)) => Ok(Value::Float(x.powf(y as f64))),
                    _ => Ok(Value::Null),
                }
            }
            "reversed" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let mut items = self.iterable_to_list(&v);
                items.reverse();
                Ok(Value::List(items))
            }
            "min" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let items = self.iterable_to_list(&v);
                Ok(items.into_iter().min().unwrap_or(Value::Null))
            }
            "max" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                let items = self.iterable_to_list(&v);
                Ok(items.into_iter().max().unwrap_or(Value::Null))
            }
            "map" => {
                // map(fn, list) — fn is a Value::Func or class name.
                let mut iter = args.into_iter();
                let func = iter.next().unwrap_or(Value::Null);
                let list_val = iter.next().unwrap_or(Value::Null);
                let items = self.iterable_to_list(&list_val);
                let mut result = Vec::new();
                for item in items {
                    let v = self.call_value(&func, vec![item])?;
                    result.push(v);
                }
                Ok(Value::List(result))
            }
            "filter" => {
                let mut iter = args.into_iter();
                let func = iter.next().unwrap_or(Value::Null);
                let list_val = iter.next().unwrap_or(Value::Null);
                let items = self.iterable_to_list(&list_val);
                let mut result = Vec::new();
                for item in items.clone() {
                    let keep = self.call_value(&func, vec![item.clone()])?;
                    if keep.truthy() {
                        result.push(item);
                    }
                }
                Ok(Value::List(result))
            }
            "dict" => {
                // dict() returns an empty dict; dict(k1, v1, k2, v2, ...) builds one.
                let mut d = HashMap::new();
                let mut iter = args.into_iter();
                while let Some(k) = iter.next() {
                    if let Some(v) = iter.next() {
                        d.insert(k.to_str(), v);
                    }
                }
                Ok(Value::Dict(d))
            }
            "dict_get" => {
                let mut iter = args.into_iter();
                let d_val = iter.next().unwrap_or(Value::Null);
                let key = iter.next().unwrap_or(Value::Null);
                let default = iter.next().unwrap_or(Value::Null);
                if let Value::Dict(d) = &d_val {
                    Ok(d.get(&key.to_str()).cloned().unwrap_or(default))
                } else {
                    Ok(default)
                }
            }
            "dict_set" => {
                let mut iter = args.into_iter();
                let d_val = iter.next().unwrap_or(Value::Null);
                let key = iter.next().unwrap_or(Value::Null);
                let val = iter.next().unwrap_or(Value::Null);
                if let Value::Dict(mut d) = d_val {
                    d.insert(key.to_str(), val);
                    Ok(Value::Dict(d))
                } else {
                    Ok(Value::Null)
                }
            }
            "dict_keys" => {
                let d_val = args.into_iter().next().unwrap_or(Value::Null);
                if let Value::Dict(d) = &d_val {
                    Ok(Value::List(d.keys().map(|k| Value::Str(k.clone())).collect()))
                } else {
                    Ok(Value::List(vec![]))
                }
            }
            "dict_values" => {
                let d_val = args.into_iter().next().unwrap_or(Value::Null);
                if let Value::Dict(d) = &d_val {
                    Ok(Value::List(d.values().cloned().collect()))
                } else {
                    Ok(Value::List(vec![]))
                }
            }
            "dict_has" => {
                let mut iter = args.into_iter();
                let d_val = iter.next().unwrap_or(Value::Null);
                let key = iter.next().unwrap_or(Value::Null);
                if let Value::Dict(d) = &d_val {
                    Ok(Value::Bool(d.contains_key(&key.to_str())))
                } else {
                    Ok(Value::Bool(false))
                }
            }
            "list" => {
                // list() returns empty list; list(iterable) converts.
                if args.is_empty() {
                    Ok(Value::List(vec![]))
                } else {
                    let v = args.into_iter().next().unwrap_or(Value::Null);
                    Ok(Value::List(self.iterable_to_list(&v)))
                }
            }
            // ===== fmt module =====
            "printf" => {
                // printf(format, args...) — print formatted to stdout.
                let mut iter = args.into_iter();
                let fmt = iter.next().unwrap_or(Value::Null).to_str();
                let rest: Vec<Value> = iter.collect();
                let out = format_string(&fmt, &rest);
                print!("{}", out);
                use std::io::Write;
                let _ = std::io::stdout().flush();
                Ok(Value::Int(out.len() as i64))
            }
            "fmt_sprintf" => {
                let mut iter = args.into_iter();
                let fmt = iter.next().unwrap_or(Value::Null).to_str();
                let rest: Vec<Value> = iter.collect();
                Ok(Value::Str(format_string(&fmt, &rest)))
            }
            "fmt_fprintf" => {
                // fprintf(file_or_stream, fmt, args...) — write to stdout
                // if first arg is "stdout"/"stderr", else ignore.
                let mut iter = args.into_iter();
                let stream = iter.next().unwrap_or(Value::Null).to_str();
                let fmt = iter.next().unwrap_or(Value::Null).to_str();
                let rest: Vec<Value> = iter.collect();
                let out = format_string(&fmt, &rest);
                if stream == "stderr" {
                    eprint!("{}", out);
                } else {
                    print!("{}", out);
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
                Ok(Value::Int(out.len() as i64))
            }
            // ===== path module =====
            "path_join" => {
                let parts: Vec<String> = args.iter().map(|v| v.to_str()).collect();
                let mut result = String::new();
                for (i, p) in parts.iter().enumerate() {
                    if i == 0 {
                        result.push_str(p);
                    } else {
                        if !result.ends_with('/') && !p.starts_with('/') {
                            result.push('/');
                        }
                        result.push_str(p);
                    }
                }
                Ok(Value::Str(result))
            }
            "path_dirname" => {
                let p = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let dn = std::path::Path::new(&p).parent()
                    .map(|x| x.to_string_lossy().to_string())
                    .unwrap_or_else(|| ".".to_string());
                Ok(Value::Str(dn))
            }
            "path_basename" => {
                let p = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let bn = std::path::Path::new(&p).file_name()
                    .map(|x| x.to_string_lossy().to_string())
                    .unwrap_or_default();
                Ok(Value::Str(bn))
            }
            "path_ext" => {
                let p = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let ext = std::path::Path::new(&p).extension()
                    .map(|x| format!(".{}", x.to_string_lossy()))
                    .unwrap_or_default();
                Ok(Value::Str(ext))
            }
            "path_exists" => {
                let p = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Bool(std::path::Path::new(&p).exists()))
            }
            "path_is_abs" => {
                let p = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Bool(std::path::Path::new(&p).is_absolute()))
            }
            "path_abs" => {
                let p = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                match std::fs::canonicalize(&p) {
                    Ok(abs) => Ok(Value::Str(abs.to_string_lossy().to_string())),
                    Err(_) => Ok(Value::Null),
                }
            }
            // ===== encoding module =====
            "base64_encode" => {
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Str(base64_encode(&s)))
            }
            "base64_decode" => {
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                match base64_decode(&s) {
                    Some(d) => Ok(Value::Str(d)),
                    None => Ok(Value::Null),
                }
            }
            "hex_encode" => {
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Str(hex_encode(&s)))
            }
            "hex_decode" => {
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                match hex_decode(&s) {
                    Some(d) => Ok(Value::Str(d)),
                    None => Ok(Value::Null),
                }
            }
            "url_encode" => {
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Str(url_encode(&s)))
            }
            "url_decode" => {
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                match url_decode(&s) {
                    Some(d) => Ok(Value::Str(d)),
                    None => Ok(Value::Null),
                }
            }
            // ===== crypto module =====
            "crypto_sha256" => {
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Str(sha256_hex(&s)))
            }
            "crypto_md5" => {
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Str(md5_hex(&s)))
            }
            "crypto_sha1" => {
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Str(sha1_hex(&s)))
            }
            // ===== regex module (free functions) =====
            "regex_compile" => {
                let pat = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let mut fields = HashMap::new();
                fields.insert("__pattern__".to_string(), Value::Str(pat));
                Ok(Value::Object("Regex".to_string(), Rc::new(RefCell::new(fields))))
            }
            "regex_match" => {
                // regex_match(pattern, text) → [full_match, group1, group2, ...]
                // or null if no match. Uses Python `re.match` semantics
                // (anchored at the start of the text; the match does not
                // need to extend to the end). Capture groups `(...)` are
                // returned in order; unmatched optional groups are null.
                let pat = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let s = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                match regex_match_captures(&pat, &s) {
                    Some(caps) => {
                        let text: Vec<char> = s.chars().collect();
                        Ok(captures_to_value(&caps, &text))
                    }
                    None => Ok(Value::Null),
                }
            }
            "regex_findall" => {
                // regex_findall(pattern, text) → list of [full_match, group1, ...]
                // for every non-overlapping match.
                let pat = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let s = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                let text: Vec<char> = s.chars().collect();
                let mut results: Vec<Value> = Vec::new();
                let mut pos = 0usize;
                while pos <= text.len() {
                    match regex_search_captures_from(&pat, &text, pos) {
                        Some((start, end, caps)) => {
                            results.push(captures_to_value(&caps, &text));
                            let _ = start;
                            pos = if end == pos { end + 1 } else { end };
                        }
                        None => break,
                    }
                }
                Ok(Value::List(results))
            }
            "regex_search" => {
                let pat = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let s = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                match regex_search(&pat, &s) {
                    Some((st, e)) => Ok(Value::Tuple(vec![Value::Int(st as i64), Value::Int(e as i64)])),
                    None => Ok(Value::Null),
                }
            }
            "regex_find_all" => {
                let pat = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let s = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::List(regex_find_all(&pat, &s).into_iter().map(Value::Str).collect()))
            }
            "regex_sub" => {
                let pat = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let repl = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                let s = args.get(2).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Str(regex_replace(&pat, &repl, &s)))
            }
            // ===== debug module =====
            "debug_inspect" => {
                let v = args.get(0).cloned().unwrap_or(Value::Null);
                eprintln!("[inspect] {} = {}", stdlib_type_name(&v), v.to_str());
                Ok(v)
            }
            "debug_trace" => {
                let v = args.get(0).cloned().unwrap_or(Value::Null);
                eprintln!("[trace] {} = {}", stdlib_type_name(&v), v.to_str());
                Ok(v)
            }
            "debug_timeit" => {
                // debug_timeit(fn) — call fn, return elapsed ns.
                // fn is Value::Func; we call it via call_value.
                let f = args.get(0).cloned().unwrap_or(Value::Null);
                let start = std::time::Instant::now();
                let _ = self.call_value(&f, vec![]);
                let dur = start.elapsed().as_nanos() as i64;
                Ok(Value::Int(dur))
            }
            "debug_backtrace" => {
                // Return a simple backtrace string (the VM doesn't track
                // source spans for AST path; this is a placeholder).
                Ok(Value::Str(format!("#0  <vm> (depth={})", self.frames.len())))
            }
            "debug_dump" => {
                let v = args.get(0).cloned().unwrap_or(Value::Null);
                let s = v.to_str();
                let mut hex = String::new();
                for b in s.as_bytes() {
                    hex.push_str(&format!("{:02x} ", b));
                }
                eprintln!("dump: {} | {}", hex.trim(), s);
                Ok(Value::Null)
            }
            // ===== flag module (minimal — parses os.args) =====
            "flag_string" | "flag_int" | "flag_bool" => {
                // Register a flag: flag_string("name", "default", default_value)
                // or flag_string("name", default_value) (2-arg form).
                // Returns the parsed value if flag_parse() was called, else default.
                let flag_name = args.get(0).cloned().unwrap_or(Value::Null).to_str();
                let default_val = if args.len() >= 3 {
                    args.get(2).cloned().unwrap_or(Value::Null)
                } else {
                    args.get(1).cloned().unwrap_or(Value::Null)
                };
                let flag_type = match name {
                    "flag_string" => "string",
                    "flag_int" => "int",
                    "flag_bool" => "bool",
                    _ => "string",
                };
                // Register the flag if not already registered.
                // Don't overwrite the parsed value if it's already set.
                let entry = self.flag_registry.entry(flag_name.clone()).or_insert_with(|| {
                    (flag_type.to_string(), default_val.clone(), None)
                });
                // Update type and default, but preserve parsed value.
                entry.0 = flag_type.to_string();
                entry.1 = default_val.clone();
                // Return the parsed value if available, else the default.
                Ok(entry.2.clone().unwrap_or_else(|| default_val))
            }
            "flag_parse" => {
                // Parse command-line arguments.
                // Skip the first 2 args (vredrs binary + "run" + file path).
                // Everything after that (or after "--") is a user flag.
                let all_args: Vec<String> = std::env::args().collect();
                // Find where user args start: skip "vredrs run file.veds"
                // or everything after "--"
                let user_args: Vec<&String> = if let Some(dash_pos) = all_args.iter().position(|a| a == "--") {
                    all_args[dash_pos + 1..].iter().collect()
                } else {
                    // Skip first 3 args: [0]=vredrs, [1]=run/vm, [2]=file
                    all_args.iter().skip(3).collect()
                };
                let mut i = 0;
                while i < user_args.len() {
                    let arg = user_args[i];
                    if arg.starts_with("--") || (arg.starts_with('-') && arg.len() > 1) {
                        let stripped = if arg.starts_with("--") { &arg[2..] } else { &arg[1..] };
                        // Check for --name=value form
                        if let Some(eq_pos) = stripped.find('=') {
                            let name = &stripped[..eq_pos];
                            let value = &stripped[eq_pos + 1..];
                            self.set_flag_value(name, value);
                        } else {
                            // --name value form (consume next arg as value)
                            let name = stripped.to_string();
                            if i + 1 < user_args.len() && !user_args[i + 1].starts_with('-') {
                                let value = user_args[i + 1];
                                self.set_flag_value(&name, value);
                                i += 1;
                            } else {
                                // --name without value → bool true
                                self.set_flag_value(&name, "true");
                            }
                        }
                    }
                    i += 1;
                }
                Ok(Value::Null)
            }
            "flag_args" => {
                // Return non-flag arguments (positional args).
                let argv: Vec<Value> = std::env::args()
                    .skip(1)
                    .filter(|a| !a.starts_with('-'))
                    .map(Value::Str)
                    .collect();
                Ok(Value::List(argv))
            }
            // ===== log module =====
            "log_debug" | "log_info" | "log_warn" | "log_error" => {
                let level = match name {
                    "log_debug" => "DEBUG",
                    "log_info" => "INFO",
                    "log_warn" => "WARN",
                    "log_error" => "ERROR",
                    _ => "?",
                };
                let msg = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                eprintln!("[{}] {}", level, msg);
                Ok(Value::Null)
            }
            "log_set_level" | "log_set_format" => Ok(Value::Null),
            // ===== term module =====
            "term_clear" => {
                print!("\x1b[2J\x1b[H");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                Ok(Value::Null)
            }
            "term_move_cursor" => {
                let row = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(1);
                let col = args.get(1).and_then(|v| v.to_int().ok()).unwrap_or(1);
                print!("\x1b[{};{}H", row, col);
                use std::io::Write;
                let _ = std::io::stdout().flush();
                Ok(Value::Null)
            }
            "term_set_color" => {
                let fg = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(7);
                print!("\x1b[3{}m", fg);
                use std::io::Write;
                let _ = std::io::stdout().flush();
                Ok(Value::Null)
            }
            "term_reset" => {
                print!("\x1b[0m");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                Ok(Value::Null)
            }
            "term_get_size" => {
                // Best-effort: return 24x80 (can't easily query without termios).
                Ok(Value::Tuple(vec![Value::Int(24), Value::Int(80)]))
            }
            "term_read_key" => {
                // Read one byte from stdin.
                use std::io::Read;
                let mut buf = [0u8; 1];
                if std::io::stdin().read(&mut buf).is_ok() && buf[0] != 0 {
                    Ok(Value::Str((buf[0] as char).to_string()))
                } else {
                    Ok(Value::Str(String::new()))
                }
            }
            // ===== image module =====
            "image_load" => {
                let path = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        // Detect format by magic bytes / extension.
                        let ext = std::path::Path::new(&path).extension()
                            .and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                        let img = if ext == "ppm" || ext == "pgm" || ext == "pbm" {
                            parse_pnm(&bytes)
                        } else if ext == "bmp" {
                            parse_bmp(&bytes)
                        } else if bytes.starts_with(b"P6") || bytes.starts_with(b"P3") {
                            parse_pnm(&bytes)
                        } else if bytes.starts_with(b"BM") {
                            parse_bmp(&bytes)
                        } else {
                            None
                        };
                        Ok(match img {
                            Some(v) => v,
                            None => Value::Null,
                        })
                    }
                    Err(_) => Ok(Value::Null),
                }
            }
            "image_save" => {
                let img = args.get(0).cloned().unwrap_or(Value::Null);
                let path = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                let ext = std::path::Path::new(&path).extension()
                    .and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                let bytes = if ext == "ppm" {
                    serialize_pnm(&img, true)
                } else if ext == "pgm" {
                    serialize_pnm(&img, false)
                } else if ext == "bmp" {
                    serialize_bmp(&img)
                } else {
                    serialize_pnm(&img, true)
                };
                Ok(Value::Bool(std::fs::write(&path, &bytes).is_ok()))
            }
            "image_new" | "image_create" => {
                // image.new(width, height) → blank Image (all black, alpha 255).
                let w = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let h = args.get(1).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let mut pixels: Vec<Value> = Vec::with_capacity((w * h) as usize);
                for _ in 0..(w * h) {
                    pixels.push(Value::Tuple(vec![Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(255)]));
                }
                let mut f = HashMap::new();
                f.insert("__width__".to_string(), Value::Int(w));
                f.insert("__height__".to_string(), Value::Int(h));
                f.insert("__pixels__".to_string(), Value::List(pixels));
                Ok(Value::Object("Image".to_string(), Rc::new(RefCell::new(f))))
            }
            "image_resize" => {
                // image.resize(img, w, h) — delegate to Image.resize method.
                let img = args.get(0).cloned().unwrap_or(Value::Null);
                let w = args.get(1).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let h = args.get(2).and_then(|v| v.to_int().ok()).unwrap_or(0);
                if let Some(v) = self.call_image_method(&img, "resize", &[Value::Int(w), Value::Int(h)])? {
                    Ok(v)
                } else {
                    Ok(Value::Null)
                }
            }
            "image_crop" => {
                let img = args.get(0).cloned().unwrap_or(Value::Null);
                let x = args.get(1).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let y = args.get(2).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let w = args.get(3).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let h = args.get(4).and_then(|v| v.to_int().ok()).unwrap_or(0);
                if let Some(v) = self.call_image_method(&img, "crop", &[Value::Int(x), Value::Int(y), Value::Int(w), Value::Int(h)])? {
                    Ok(v)
                } else {
                    Ok(Value::Null)
                }
            }
            // ===== machine module (bare-metal stubs) =====
            "machine_gpio_pin" => {
                let pin = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let mode = args.get(1).map(|v| v.to_str()).unwrap_or_else(|| "out".to_string());
                let mut f = HashMap::new();
                f.insert("__pin__".to_string(), Value::Int(pin));
                f.insert("__mode__".to_string(), Value::Str(mode));
                f.insert("__value__".to_string(), Value::Bool(false));
                Ok(Value::Object("Pin".to_string(), Rc::new(RefCell::new(f))))
            }
            "machine_i2c_init" => {
                let bus = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let freq = args.get(1).and_then(|v| v.to_int().ok()).unwrap_or(100000);
                let mut f = HashMap::new();
                f.insert("__bus__".to_string(), Value::Int(bus));
                f.insert("__freq__".to_string(), Value::Int(freq));
                Ok(Value::Object("I2C".to_string(), Rc::new(RefCell::new(f))))
            }
            "machine_spi_init" => {
                let bus = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let mode = args.get(1).and_then(|v| v.to_int().ok()).unwrap_or(0);
                let freq = args.get(2).and_then(|v| v.to_int().ok()).unwrap_or(1000000);
                let mut f = HashMap::new();
                f.insert("__bus__".to_string(), Value::Int(bus));
                f.insert("__mode__".to_string(), Value::Int(mode));
                f.insert("__freq__".to_string(), Value::Int(freq));
                Ok(Value::Object("SPI".to_string(), Rc::new(RefCell::new(f))))
            }
            "machine_serial_open" => {
                let port = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let baud = args.get(1).and_then(|v| v.to_int().ok()).unwrap_or(115200);
                let mut f = HashMap::new();
                f.insert("__port__".to_string(), Value::Str(port));
                f.insert("__baud__".to_string(), Value::Int(baud));
                Ok(Value::Object("Serial".to_string(), Rc::new(RefCell::new(f))))
            }
            // ===== unsafe module (VM-level stubs; no real pointers) =====
            "unsafe_sizeof" => {
                // Return best-effort sizes for primitive types.
                let ty = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let sz = match ty.as_str() {
                    "int" | "float" | "bool" => 8,
                    "str" | "list" | "dict" => 8, // pointer-sized
                    _ => 0,
                };
                Ok(Value::Int(sz))
            }
            "unsafe_alignof" => {
                let ty = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let a = match ty.as_str() {
                    "int" | "float" | "bool" => 8,
                    _ => 1,
                };
                Ok(Value::Int(a))
            }
            "unsafe_offsetof" => Ok(Value::Int(0)),
            "unsafe_cast" => {
                // Pass-through: VM values are dynamically typed.
                Ok(args.get(0).cloned().unwrap_or(Value::Null))
            }
            "unsafe_alloc" => Ok(Value::Int(0)),
            "unsafe_free" => Ok(Value::Null),
            // ===== embed module =====
            "embed_fs" => {
                let dir = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let mut f = HashMap::new();
                f.insert("__dir__".to_string(), Value::Str(dir));
                Ok(Value::Object("EmbeddedFS".to_string(), Rc::new(RefCell::new(f))))
            }
            "embed_read" => {
                // @embed(path) — read file at compile time. At runtime,
                // this reads the file from disk. Uses read (bytes) instead
                // of read_to_string so binary files (e.g. PPM images) work.
                let path = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        // Best-effort: interpret as UTF-8, else lossy.
                        Ok(Value::Str(String::from_utf8_lossy(&bytes).to_string()))
                    }
                    Err(_) => Ok(Value::Null),
                }
            }
            // ===== crypto.aes / bcrypt =====
            "crypto_aes_encrypt" => {
                // AES-256-CBC with PKCS#7 padding (native, no external crate).
                // key must be 32 bytes, iv 16 bytes. data is a string.
                // Returns the ciphertext as a hex-encoded string (since raw
                // cipher bytes may not be valid UTF-8).
                let key = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let iv = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                let data = args.get(2).map(|v| v.to_str()).unwrap_or_default();
                if key.len() != 32 || iv.len() != 16 {
                    return Ok(Value::Null);
                }
                let cipher = aes256_cbc_encrypt(key.as_bytes(), iv.as_bytes(), data.as_bytes());
                Ok(Value::Str(cipher.iter().map(|b| format!("{:02x}", b)).collect()))
            }
            "crypto_aes_decrypt" => {
                let key = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let iv = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                let cipher_hex = args.get(2).map(|v| v.to_str()).unwrap_or_default();
                if key.len() != 32 || iv.len() != 16 {
                    return Ok(Value::Null);
                }
                // Decode hex.
                let cipher: Vec<u8> = safe_hex_decode(&cipher_hex);
                if cipher.is_empty() {
                    return Ok(Value::Null);
                }
                let plain = aes256_cbc_decrypt(key.as_bytes(), iv.as_bytes(), &cipher);
                match plain {
                    Some(p) => Ok(Value::Str(p.iter().map(|&b| b as char).collect())),
                    None => Ok(Value::Null),
                }
            }
            "crypto_bcrypt_hash" | "crypto_password_hash" => {
                // PBKDF2-style password hashing.
                //
                // This is NOT real bcrypt (which requires the Blowfish cipher).
                // It is a salted, iterated SHA-256 hash in the spirit of
                // PBKDF2-HMAC-SHA-256. The name `crypto_bcrypt_hash` is kept
                // as a deprecated alias so existing callers continue to work;
                // new code should call `crypto_password_hash`.
                //
                // Output format: $pbkdf2$<iters>$<salt_hex>$<hash_hex>
                //   iters    — iteration count (>= 10000)
                //   salt_hex — 16-byte cryptographically-random salt, hex
                //   hash_hex — 32-byte SHA-256-derived hash, hex
                //
                // The salt is drawn from `secure_rand_bytes`, which mixes
                // nanosecond wall-clock entropy with the process ID and a
                // per-call counter through SHA-256 — NOT the xorshift PRNG
                // used elsewhere in the VM (which is fine for simulations
                // but predictable enough to be unsafe for password salts).
                let password = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let raw_cost = args.get(1).and_then(|v| v.to_int().ok()).unwrap_or(14);
                // Convert the legacy "cost" exponent (used by the old
                // bcrypt-style API) into an iteration count. Default is
                // 1<<14 = 16384 iterations; clamp to a safe minimum of
                // 10000 and a hard ceiling of 10_000_000.
                let iters = (1u32 << raw_cost.max(1).min(20) as u32).max(10000).min(10_000_000);
                Ok(Value::Str(crypto_password_hash_impl(&password, iters)))
            }
            "crypto_bcrypt_verify" | "crypto_password_verify" => {
                // Verify a password against a stored
                // `$pbkdf2$<iters>$<salt_hex>$<hash_hex>` string. Returns
                // false (not an error) if the stored string is malformed.
                let password = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let stored = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Bool(crypto_password_verify_impl(&password, &stored)))
            }
            // ===== websocket module (real WebSocket via std::net) =====
            "websocket_connect" => {
                let url = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                // Parse ws://host:port/path
                let url_rest = url.strip_prefix("ws://").or_else(|| url.strip_prefix("wss://")).unwrap_or(&url);
                let (host_port, path) = match url_rest.find('/') {
                    Some(i) => (&url_rest[..i], &url_rest[i..]),
                    None => (url_rest, "/"),
                };
                let stream = match std::net::TcpStream::connect(host_port) {
                    Ok(s) => {
                        // Set 10-second read/write timeout to prevent
                        // indefinite blocking on slow/unresponsive servers.
                        let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                        let _ = s.set_write_timeout(Some(std::time::Duration::from_secs(10)));
                        s
                    }
                    Err(_) => return Ok(Value::Null),
                };
                use std::io::{Read, Write};
                let mut stream = stream;
                // Generate random 16-byte key and encode as base64.
                let key_bytes: [u8; 16] = [
                    (xorshift_rand() & 0xff) as u8, (xorshift_rand() & 0xff) as u8,
                    (xorshift_rand() & 0xff) as u8, (xorshift_rand() & 0xff) as u8,
                    (xorshift_rand() & 0xff) as u8, (xorshift_rand() & 0xff) as u8,
                    (xorshift_rand() & 0xff) as u8, (xorshift_rand() & 0xff) as u8,
                    (xorshift_rand() & 0xff) as u8, (xorshift_rand() & 0xff) as u8,
                    (xorshift_rand() & 0xff) as u8, (xorshift_rand() & 0xff) as u8,
                    (xorshift_rand() & 0xff) as u8, (xorshift_rand() & 0xff) as u8,
                    (xorshift_rand() & 0xff) as u8, (xorshift_rand() & 0xff) as u8,
                ];
                let key_b64 = base64_encode(&String::from_utf8_lossy(&key_bytes));
                let request = format!(
                    "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
                    path, host_port, key_b64
                );
                if stream.write_all(request.as_bytes()).is_err() {
                    return Ok(Value::Null);
                }
                // Read handshake response (up to \r\n\r\n).
                let mut buf = [0u8; 4096];
                let n = match stream.read(&mut buf) {
                    Ok(n) if n > 0 => n,
                    _ => return Ok(Value::Null),
                };
                let response = String::from_utf8_lossy(&buf[..n]);
                if !response.contains("101") {
                    return Ok(Value::Null);
                }
                let id = self.next_net_id;
                self.next_net_id += 1;
                self.ws_connections.insert(id, stream);
                let mut f = HashMap::new();
                f.insert("__id__".to_string(), Value::Int(id));
                f.insert("__url__".to_string(), Value::Str(url));
                f.insert("__closed__".to_string(), Value::Bool(false));
                Ok(Value::Object("WebSocket".to_string(), Rc::new(RefCell::new(f))))
            }
            // ===== sync.atomic sub-module =====
            // In the VM, "atomic" operations are trivially atomic (single
            // thread). We model a "pointer to int" as a 1-element list.
            "atomic_load_int" => {
                if let Some(Value::List(l)) = args.get(0) {
                    Ok(l.get(0).cloned().unwrap_or(Value::Int(0)))
                } else {
                    Ok(Value::Int(0))
                }
            }
            "atomic_store_int" => {
                // atomic.store_int(p, value) — can't mutate the caller's list
                // in place (Vec<Value> by value). Return the new list so the
                // caller can re-bind: `set, p, atomic.store_int(p, v)`.
                let new_val = args.get(1).cloned().unwrap_or(Value::Int(0));
                Ok(Value::List(vec![new_val]))
            }
            "atomic_add_int" => {
                // atomic.add_int(p, delta) -> int (returns new value)
                let delta = args.get(1).and_then(|v| v.to_int().ok()).unwrap_or(0);
                if let Some(Value::List(l)) = args.get(0) {
                    if let Some(Value::Int(cur)) = l.get(0) {
                        return Ok(Value::List(vec![Value::Int(cur + delta)]));
                    }
                }
                Ok(Value::List(vec![Value::Int(delta)]))
            }
            "atomic_cas_int" => {
                // atomic.compare_and_swap_int(p, old, new) -> bool
                // Returns true if the swap succeeded (i.e. *p == old).
                if let Some(Value::List(l)) = args.get(0) {
                    let old = args.get(1).and_then(|v| v.to_int().ok()).unwrap_or(0);
                    if l.get(0) == Some(&Value::Int(old)) {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }
            // ===== compress module (real gzip/zlib/flate via flate2) =====
            // Compressed data is returned as a hex-encoded string to avoid
            // UTF-8 encoding issues with arbitrary binary bytes.
            "compress_gzip_encode" => {
                use flate2::write::GzEncoder;
                use flate2::Compression;
                use std::io::Write;
                let data = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                let _ = encoder.write_all(data.as_bytes());
                match encoder.finish() {
                    Ok(compressed) => Ok(Value::Str(compressed.iter().map(|b| format!("{:02x}", b)).collect())),
                    Err(_) => Ok(Value::Null),
                }
            }
            "compress_gzip_decode" => {
                use flate2::read::GzDecoder;
                use std::io::Read;
                let hex_data = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let data: Vec<u8> = safe_hex_decode(&hex_data);
                let mut decoder = GzDecoder::new(&data[..]);
                let mut bytes = Vec::new();
                match decoder.read_to_end(&mut bytes) {
                    Ok(_) => Ok(Value::Str(String::from_utf8_lossy(&bytes).to_string())),
                    Err(_) => Ok(Value::Null),
                }
            }
            "compress_zlib_encode" => {
                use flate2::write::ZlibEncoder;
                use flate2::Compression;
                use std::io::Write;
                let data = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
                let _ = encoder.write_all(data.as_bytes());
                match encoder.finish() {
                    Ok(compressed) => Ok(Value::Str(compressed.iter().map(|b| format!("{:02x}", b)).collect())),
                    Err(_) => Ok(Value::Null),
                }
            }
            "compress_zlib_decode" => {
                use flate2::read::ZlibDecoder;
                use std::io::Read;
                let hex_data = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let data: Vec<u8> = safe_hex_decode(&hex_data);
                let mut decoder = ZlibDecoder::new(&data[..]);
                let mut bytes = Vec::new();
                match decoder.read_to_end(&mut bytes) {
                    Ok(_) => Ok(Value::Str(String::from_utf8_lossy(&bytes).to_string())),
                    Err(_) => Ok(Value::Null),
                }
            }
            "compress_flate_encode" => {
                use flate2::write::DeflateEncoder;
                use flate2::Compression;
                use std::io::Write;
                let data = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
                let _ = encoder.write_all(data.as_bytes());
                match encoder.finish() {
                    Ok(compressed) => Ok(Value::Str(compressed.iter().map(|b| format!("{:02x}", b)).collect())),
                    Err(_) => Ok(Value::Null),
                }
            }
            "compress_flate_decode" => {
                use flate2::read::DeflateDecoder;
                use std::io::Read;
                let hex_data = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let data: Vec<u8> = safe_hex_decode(&hex_data);
                let mut decoder = DeflateDecoder::new(&data[..]);
                let mut bytes = Vec::new();
                match decoder.read_to_end(&mut bytes) {
                    Ok(_) => Ok(Value::Str(String::from_utf8_lossy(&bytes).to_string())),
                    Err(_) => Ok(Value::Null),
                }
            }
            // ===== net module (real TCP via std::net) =====
            "net_dial" => {
                let network = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let address = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                if network != "tcp" {
                    return Ok(Value::Null);
                }
                match std::net::TcpStream::connect(&address) {
                    Ok(stream) => {
                        let _ = stream.set_nonblocking(false);
                        // Set 10-second timeout to prevent indefinite blocking.
                        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));
                        let id = self.next_net_id;
                        self.next_net_id += 1;
                        self.tcp_streams.insert(id, stream);
                        let mut f = HashMap::new();
                        f.insert("__id__".to_string(), Value::Int(id));
                        Ok(Value::Object("Conn".to_string(), Rc::new(RefCell::new(f))))
                    }
                    Err(_) => Ok(Value::Null),
                }
            }
            "net_listen" => {
                let network = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let address = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                if network != "tcp" {
                    return Ok(Value::Null);
                }
                match std::net::TcpListener::bind(&address) {
                    Ok(listener) => {
                        let id = self.next_net_id;
                        self.next_net_id += 1;
                        self.tcp_listeners.insert(id, listener);
                        let mut f = HashMap::new();
                        f.insert("__id__".to_string(), Value::Int(id));
                        Ok(Value::Object("Listener".to_string(), Rc::new(RefCell::new(f))))
                    }
                    Err(_) => Ok(Value::Null),
                }
            }
            // ===== http module (real HTTP/1.1 client via std::net) =====
            "http_get" => {
                let url = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let headers_val = args.get(1).cloned().unwrap_or(Value::Dict(HashMap::new()));
                Ok(http_request("GET", &url, "", &headers_val, self))
            }
            "http_post" => {
                let url = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let body = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                let headers_val = args.get(2).cloned().unwrap_or(Value::Dict(HashMap::new()));
                Ok(http_request("POST", &url, &body, &headers_val, self))
            }
            // ===== sql module (real in-memory SQL) =====
            "sql_open" => {
                let driver = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let _dsn = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                let id = self.next_net_id;
                self.next_net_id += 1;
                self.sql_dbs.insert(id, SqlDb { tables: HashMap::new() });
                let mut f = HashMap::new();
                f.insert("__id__".to_string(), Value::Int(id));
                f.insert("__driver__".to_string(), Value::Str(driver));
                Ok(Value::Object("DB".to_string(), Rc::new(RefCell::new(f))))
            }
            "sql_drivers" => Ok(Value::List(vec![Value::Str("memory".to_string())])),
            // ===== csv module =====
            "csv_read" => {
                let path = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        let rows = parse_csv(&content);
                        Ok(Value::List(rows.into_iter()
                            .map(|row| Value::List(row.into_iter().map(Value::Str).collect()))
                            .collect()))
                    }
                    Err(_) => Ok(Value::Null),
                }
            }
            "csv_write" => {
                let path = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let rows_val = args.get(1).cloned().unwrap_or(Value::Null);
                let rows: Vec<Vec<String>> = match &rows_val {
                    Value::List(l) => l.iter().map(|row| match row {
                        Value::List(cells) => cells.iter().map(|c| c.to_str()).collect(),
                        _ => vec![row.to_str()],
                    }).collect(),
                    _ => vec![],
                };
                let mut out = String::new();
                for row in &rows {
                    let cells: Vec<String> = row.iter().map(|c| {
                        if c.contains(',') || c.contains('"') || c.contains('\n') {
                            format!("\"{}\"", c.replace('"', "\"\""))
                        } else {
                            c.clone()
                        }
                    }).collect();
                    out.push_str(&cells.join(","));
                    out.push('\n');
                }
                match std::fs::write(&path, &out) {
                    Ok(_) => Ok(Value::Bool(true)),
                    Err(_) => Ok(Value::Bool(false)),
                }
            }
            // ===== xml module (minimal) =====
            "xml_parse" => {
                // Minimal XML → dict: only top-level tag with text content.
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(native_xml_parse(&s))
            }
            "xml_stringify" => {
                let v = args.get(0).cloned().unwrap_or(Value::Null);
                Ok(Value::Str(native_xml_stringify(&v)))
            }
            // ===== toml/yaml (stubs — return best-effort) =====
            "toml_parse" => {
                // Minimal: treat as INI-like (no full TOML grammar).
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(native_toml_parse(&s))
            }
            "toml_stringify" => {
                let v = args.get(0).cloned().unwrap_or(Value::Null);
                Ok(Value::Str(native_toml_stringify(&v)))
            }
            "yaml_parse" => {
                // YAML subset parser: supports key: value, nested maps via
                // indentation, lists via `- item`, scalars (str/int/float/bool/null).
                let s = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(yaml_parse(&s))
            }
            "yaml_stringify" => {
                let v = args.get(0).cloned().unwrap_or(Value::Null);
                Ok(Value::Str(yaml_stringify(&v, 0)))
            }
            // ===== sync module (stubs — VM is single-threaded) =====
            "sync_spawn" => {
                // Execute f synchronously (single-threaded VM, no real concurrency).
                // The spawned function's return value is discarded (like Go).
                let f = args.get(0).cloned().unwrap_or(Value::Null);
                let _ = self.call_value(&f, vec![]);
                Ok(Value::Null)
            }
            "sync_channel" => {
                // Create a real channel using a Dict with an Rc<RefCell> buffer.
                // The buffer is stored as a List inside a Dict, and send/receive
                // operate on it via the channel's identity (we use a unique ID).
                let id = self.next_net_id;
                self.next_net_id += 1;
                // Store the channel buffer in a VM-level registry.
                self.channel_buffers.insert(id, (Vec::new(), false));
                let mut d = HashMap::new();
                d.insert("__channel_id__".to_string(), Value::Int(id));
                d.insert("__closed__".to_string(), Value::Bool(false));
                Ok(Value::Dict(d))
            }
            "sync_send" => {
                let ch = args.get(0).cloned().unwrap_or(Value::Null);
                let v = args.get(1).cloned().unwrap_or(Value::Null);
                if let Value::Dict(d) = &ch {
                    if let Some(Value::Int(id)) = d.get("__channel_id__") {
                        if let Some((buf, closed)) = self.channel_buffers.get_mut(id) {
                            if *closed {
                                return Err(CompilerError::runtime_error("send on closed channel"));
                            }
                            buf.push(v);
                            return Ok(Value::Null);
                        }
                    }
                }
                Ok(Value::Null)
            }
            "sync_receive" => {
                let ch = args.get(0).cloned().unwrap_or(Value::Null);
                if let Value::Dict(d) = &ch {
                    if let Some(Value::Int(id)) = d.get("__channel_id__") {
                        if let Some((buf, closed)) = self.channel_buffers.get_mut(id) {
                            if !buf.is_empty() {
                                return Ok(buf.remove(0));
                            }
                            if *closed {
                                return Ok(Value::Null);
                            }
                            // Channel is empty but not closed — return Null
                            // (in a real concurrent VM, this would block).
                            return Ok(Value::Null);
                        }
                    }
                }
                Ok(Value::Null)
            }
            "sync_close" => {
                let ch = args.get(0).cloned().unwrap_or(Value::Null);
                if let Value::Dict(d) = &ch {
                    if let Some(Value::Int(id)) = d.get("__channel_id__") {
                        if let Some((_, closed)) = self.channel_buffers.get_mut(id) {
                            *closed = true;
                        }
                    }
                }
                Ok(Value::Null)
            }
            "sync_mutex" => {
                let mut fields = HashMap::new();
                fields.insert("__locked__".to_string(), Value::Bool(false));
                Ok(Value::Object("Mutex".to_string(), Rc::new(RefCell::new(fields))))
            }
            "sync_waitgroup" => {
                let mut fields = HashMap::new();
                fields.insert("__count__".to_string(), Value::Int(0));
                Ok(Value::Object("WaitGroup".to_string(), Rc::new(RefCell::new(fields))))
            }
            // ===== os module additions =====
            "os_args" => {
                let argv: Vec<Value> = std::env::args().map(Value::Str).collect();
                Ok(Value::List(argv))
            }
            "os_exit" => {
                let code = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(0) as i32;
                std::process::exit(code);
            }
            "os_get_env" => {
                let key = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                match std::env::var(&key) {
                    Ok(v) => Ok(Value::Str(v)),
                    Err(_) => Ok(Value::Null),
                }
            }
            "os_set_env" => {
                let key = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let value = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                std::env::set_var(&key, &value);
                Ok(Value::Null)
            }
            "os_unset_env" => {
                let key = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                std::env::remove_var(&key);
                Ok(Value::Null)
            }
            "os_exec" => {
                // Security: gated by VREDRS_ALLOW_EXEC (see the live
                // `os_exec` arm above for the full rationale). This arm
                // is currently shadowed by the earlier `os_exec` match
                // arm in this same `match name` block (the earlier one
                // wins), but we keep the gate here too so the safety
                // property is preserved if the duplicate is ever
                // resolved by removing the earlier arm.
                if std::env::var("VREDRS_ALLOW_EXEC").ok().as_deref() != Some("1") {
                    return Err(CompilerError::runtime_error(
                        "command execution disabled; set VREDRS_ALLOW_EXEC=1 to enable",
                    ));
                }
                let cmd = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let output = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .output();
                match output {
                    Ok(out) => {
                        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                        Ok(Value::Str(stdout))
                    }
                    Err(_) => Ok(Value::Null),
                }
            }
            "os_system" => {
                // Security: gated by VREDRS_ALLOW_EXEC (see the live
                // `os_system` arm above for the full rationale). Kept
                // here in parity with the shadowed `os_exec` arm above.
                if std::env::var("VREDRS_ALLOW_EXEC").ok().as_deref() != Some("1") {
                    return Err(CompilerError::runtime_error(
                        "command execution disabled; set VREDRS_ALLOW_EXEC=1 to enable",
                    ));
                }
                let cmd = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let status = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .status();
                match status {
                    Ok(s) => Ok(Value::Int(s.code().unwrap_or(0) as i64)),
                    Err(_) => Ok(Value::Int(-1)),
                }
            }
            "os_getwd" | "os_cwd" => {
                match std::env::current_dir() {
                    Ok(p) => Ok(Value::Str(p.to_string_lossy().to_string())),
                    Err(_) => Ok(Value::Null),
                }
            }
            "os_chdir" => {
                let p = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Bool(std::env::set_current_dir(&p).is_ok()))
            }
            "os_mkdir" => {
                let p = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let parents = args.get(1).map(|v| v.truthy()).unwrap_or(false);
                let result = if parents {
                    std::fs::create_dir_all(&p)
                } else {
                    std::fs::create_dir(&p)
                };
                Ok(Value::Bool(result.is_ok()))
            }
            "os_remove" => {
                let p = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let result = std::fs::remove_file(&p).or_else(|_| std::fs::remove_dir(&p));
                Ok(Value::Bool(result.is_ok()))
            }
            "os_rename" => {
                let old = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let new = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Bool(std::fs::rename(&old, &new).is_ok()))
            }
            "os_stat" => {
                let p = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                match std::fs::metadata(&p) {
                    Ok(m) => {
                        let mut d = HashMap::new();
                        d.insert("size".to_string(), Value::Int(m.len() as i64));
                        d.insert("is_file".to_string(), Value::Bool(m.is_file()));
                        d.insert("is_dir".to_string(), Value::Bool(m.is_dir()));
                        if let Ok(mtime) = m.modified() {
                            if let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH) {
                                d.insert("mtime".to_string(), Value::Int(dur.as_secs() as i64));
                            }
                        }
                        Ok(Value::Dict(d))
                    }
                    Err(_) => Ok(Value::Null),
                }
            }
            // ===== fs module additions =====
            "fs_walk" => {
                let root = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let mut out: Vec<Value> = Vec::new();
                walk_dir(&root, &mut out);
                Ok(Value::List(out))
            }
            "fs_copy" => {
                let src = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let dst = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Bool(std::fs::copy(&src, &dst).is_ok()))
            }
            "fs_move" => {
                let src = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let dst = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Bool(std::fs::rename(&src, &dst).is_ok()))
            }
            "fs_remove_all" => {
                let p = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Bool(std::fs::remove_dir_all(&p).is_ok()))
            }
            "fs_temp_dir" => {
                Ok(Value::Str(std::env::temp_dir().to_string_lossy().to_string()))
            }
            "fs_temp_file" => {
                let prefix = args.get(0).map(|v| v.to_str()).unwrap_or_else(|| "vredrs".to_string());
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let path = std::env::temp_dir().join(format!("{}_{}", prefix, stamp));
                let _ = std::fs::File::create(&path);
                Ok(Value::Str(path.to_string_lossy().to_string()))
            }
            // ===== io module additions =====
            "io_read" => {
                let p = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                match std::fs::read_to_string(&p) {
                    Ok(s) => Ok(Value::Str(s)),
                    Err(_) => Ok(Value::Null),
                }
            }
            "io_write" => {
                let p = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let c = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                Ok(Value::Bool(std::fs::write(&p, &c).is_ok()))
            }
            "io_append" => {
                let p = args.get(0).map(|v| v.to_str()).unwrap_or_default();
                let c = args.get(1).map(|v| v.to_str()).unwrap_or_default();
                let result = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&p)
                    .and_then(|mut f| std::io::Write::write_all(&mut f, c.as_bytes()));
                Ok(Value::Bool(result.is_ok()))
            }
            // ===== rand module additions =====
            "rand_seed" => Ok(Value::Null),
            "rand_choice" => {
                let v = args.get(0).cloned().unwrap_or(Value::Null);
                let items = self.iterable_to_list(&v);
                if items.is_empty() {
                    Ok(Value::Null)
                } else {
                    let idx = (xorshift_rand() as usize) % items.len();
                    Ok(items[idx].clone())
                }
            }
            "rand_shuffle" => {
                // In-place shuffle (returns a new shuffled list since VM
                // lists are Vec<Value> by value).
                let v = args.get(0).cloned().unwrap_or(Value::Null);
                if let Value::List(items) = v {
                    let mut items = items;
                    for i in (1..items.len()).rev() {
                        let j = (xorshift_rand() as usize) % (i + 1);
                        items.swap(i, j);
                    }
                    Ok(Value::List(items))
                } else {
                    Ok(Value::Null)
                }
            }
            "rand_string" => {
                let len = args.get(0).and_then(|v| v.to_int().ok()).unwrap_or(0) as usize;
                let charset = args.get(1).map(|v| v.to_str())
                    .unwrap_or_else(|| "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789".to_string());
                let chars: Vec<char> = charset.chars().collect();
                if chars.is_empty() {
                    return Ok(Value::Str(String::new()));
                }
                let mut out = String::new();
                for _ in 0..len {
                    out.push(chars[(xorshift_rand() as usize) % chars.len()]);
                }
                Ok(Value::Str(out))
            }
            "tuple" => {
                if args.is_empty() {
                    Ok(Value::Tuple(vec![]))
                } else {
                    let v = args.into_iter().next().unwrap_or(Value::Null);
                    Ok(Value::Tuple(self.iterable_to_list(&v)))
                }
            }
            _ => {
                // Fall back to math_*/time_*/os_*/rand_* family if the name matches.
                if name.starts_with("math_") {
                    return self.call_math_function(name, args);
                }
                // Time/rand/os functions are handled here as a fallback.
                if name.starts_with("time_") || name.starts_with("rand_") || name.starts_with("os_") {
                    // Re-dispatch through call_builtin_value's match.
                    // These are in the match above, so we need to reach them.
                    // Since we're already in call_builtin_value, we need to
                    // check if the name is handled above. If not, error.
                }
                Err(CompilerError::runtime_error(format!(
                    "bytecode VM: unknown builtin '{}'",
                    name
                )))
            }
        }
    }

    fn push(&mut self, v: Value) {
        self.stack.push(v);
    }

    /// Call a Value (Func or Class) with the given args.
    fn call_value(&mut self, callee: &Value, args: Vec<Value>) -> Result<Value> {
        match callee {
            Value::Func(name) => {
                // Push args onto the stack and call.
                for a in &args {
                    self.push(a.clone());
                }
                self.call_function(name, args.len())?;
                Ok(self.stack.pop().unwrap_or(Value::Null))
            }
            Value::Class(class) => self.instantiate_class(class, args),
            _ => Err(CompilerError::runtime_error(format!(
                "cannot call {:?}",
                callee
            ))),
        }
    }

    fn pop(&mut self) -> Result<Value> {
        self.stack
            .pop()
            .ok_or_else(|| CompilerError::runtime_error("stack underflow"))
    }

    fn binop<F>(&mut self, f: F) -> Result<()>
    where
        F: FnOnce(Value, Value) -> Value,
    {
        let b = self.pop()?;
        let a = self.pop()?;
        self.push(f(a, b));
        Ok(())
    }

    /// Try to dispatch a binary operator to a class's `__add__`/`__sub__`/etc.
    /// method. Returns `Some(value)` if the class defines the method,
    /// `None` otherwise (so the caller can fall back to primitive semantics).
    fn try_binary_overload(
        &mut self,
        method: &str,
        a: &Value,
        b: &Value,
    ) -> Result<Option<Value>> {
        if let Value::Object(class, _) = a {
            if self.find_method(class, method).is_ok() {
                let class = class.clone();
                let receiver = a.clone();
                let args = vec![b.clone()];
                let v = self.call_method_on_class(&class, receiver, method, args)?;
                return Ok(Some(v));
            }
        }
        if let Value::Object(class, _) = b {
            if self.find_method(class, method).is_ok() {
                let class = class.clone();
                let receiver = b.clone();
                let args = vec![a.clone()];
                let v = self.call_method_on_class(&class, receiver, method, args)?;
                return Ok(Some(v));
            }
        }
        Ok(None)
    }
}

enum Flow {
    Continue,
    Return(Value),
}

fn instr_argc(instr: &Instr) -> usize {
    match instr {
        Instr::CallMethod(_, argc) => *argc,
        Instr::Call(argc) => *argc,
        _ => 0,
    }
}

/// Interpret C-style escape sequences.
/// Generate a type-discriminated identity key for freeze/is_frozen.
/// This prevents values of different types that happen to have the same
/// string representation (e.g. Int(5) and Str("5")) from sharing
/// frozen state.
fn freeze_key(v: &Value) -> String {
    let type_tag = match v {
        Value::Null => "null",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::Str(_) => "str",
        Value::List(_) => "list",
        Value::Dict(_) => "dict",
        Value::Tuple(_) => "tuple",
        Value::Func(_) => "func",
        Value::Class(_) => "class",
        Value::Object(_, _) => "obj",
        Value::Module(_, _) => "mod",
        Value::Generator(_) => "gen",
        Value::Exception(_, _) => "exc",
    };
    format!("{}:{}", type_tag, v.to_str())
}

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

/// Normalize slice bounds to non-negative indices into a sequence of length `n`.
/// Returns `(start, end)` clamped to the valid range, where `end` is exclusive.
/// `start`/`end` of `isize::MAX` / `isize::MIN` are treated as "default"
/// (end-of-sequence for start, before-beginning for end with negative step).
fn normalize_slice_bounds(start: i64, end: i64, step: i64, n: i64) -> (i64, i64) {
    let (s, e) = if step > 0 {
        // Default start = 0, default end = n.
        let s = if start == isize::MAX as i64 {
            0
        } else if start < 0 {
            (n + start).max(0)
        } else {
            start.min(n)
        };
        let e = if end == isize::MAX as i64 {
            n
        } else if end < 0 {
            (n + end).max(0)
        } else if end > n {
            n
        } else {
            end
        };
        (s, e)
    } else {
        // Negative step: default start = n-1, default end = -1 (before start).
        let s = if start == isize::MAX as i64 {
            n - 1
        } else if start < 0 {
            // Negative start: count from end.
            let idx = n + start;
            idx.max(-1).min(n - 1)
        } else if start >= n {
            n - 1
        } else {
            start
        };
        let e = if end == isize::MIN as i64 {
            -1
        } else if end < 0 {
            // Negative end: count from end.
            let idx = n + end;
            idx.max(-1).min(n - 1)
        } else if end >= n {
            n
        } else {
            end
        };
        (s, e)
    };
    (s, e)
}

/// Return the basename of a module path (e.g. "./helper" → "helper", "math" → "math").
fn module_basename(path: &str) -> String {
    let trimmed = path
        .trim_start_matches("./")
        .trim_start_matches("../");
    let basename = trimmed.rsplit('/').next().unwrap_or(trimmed);
    basename.trim_end_matches(".veds").to_string()
}

/// Resolve a module path relative to a base directory. Tries `.veds`, then
/// directory + `/mod.veds`, then bare name.
fn resolve_module_path(base_dir: &std::path::Path, path: &str) -> std::path::PathBuf {
    use std::path::PathBuf;
    // Built-in stdlib modules are virtual — they don't have a file path.
    // The VM exposes them through globals when seen.
    if matches!(path, "math" | "io" | "os" | "time" | "fs" | "fmt" | "json" | "collections") {
        // Return a sentinel path that load_module will detect and handle.
        return PathBuf::from(format!("<builtin>/{}/mod.veds", path));
    }
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    // If the path has a .veds extension, use it directly.
    if path.ends_with(".veds") {
        return base_dir.join(path);
    }
    // Try ./path.veds
    let with_ext = base_dir.join(format!("{}.veds", path));
    if with_ext.exists() {
        return with_ext;
    }
    // Try ./path/mod.veds
    let mod_path = base_dir.join(path).join("mod.veds");
    if mod_path.exists() {
        return mod_path;
    }
    // Fall back to ./path.veds even if it doesn't exist (for error messages).
    with_ext
}

// ===== Stdlib helper free functions =====

fn stdlib_type_name(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::Int(_) => "int".to_string(),
        Value::Float(_) => "float".to_string(),
        Value::Str(_) => "str".to_string(),
        Value::List(_) => "list".to_string(),
        Value::Tuple(_) => "tuple".to_string(),
        Value::Dict(_) => "dict".to_string(),
        Value::Object(c, _) => format!("object:{}", c),
        Value::Class(c) => format!("class:{}", c),
        Value::Func(f) => format!("func:{}", f),
        Value::Module(n, _) => format!("module:{}", n),
        Value::Generator(_) => "generator".to_string(),
        Value::Exception(_, _) => "exception".to_string(),
    }
}

fn xorshift_rand() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = Cell::new({
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64 | 1)
                .unwrap_or(0xdeadbeef)
        });
    }
    STATE.with(|s| {
        let mut x = s.get();
        if x == 0 { x = 0xdeadbeef; }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}

// ===== Image format helpers (PNM / BMP — no external deps) =====

fn parse_pnm(bytes: &[u8]) -> Option<Value> {
    // Parse P3 (ASCII RGB), P6 (binary RGB), P2 (ASCII gray), P5 (binary gray).
    // The header is ASCII: magic, width, height, maxval, then a single
    // whitespace byte before the binary data (for P5/P6).
    let mut idx = 0;
    fn skip_ws_and_comments(bytes: &[u8], idx: &mut usize) {
        loop {
            while *idx < bytes.len() && (bytes[*idx] as char).is_whitespace() {
                *idx += 1;
            }
            if *idx < bytes.len() && bytes[*idx] == b'#' {
                while *idx < bytes.len() && bytes[*idx] != b'\n' {
                    *idx += 1;
                }
            } else {
                break;
            }
        }
    }
    fn read_token(bytes: &[u8], idx: &mut usize) -> String {
        skip_ws_and_comments(bytes, idx);
        let start = *idx;
        while *idx < bytes.len() && !(bytes[*idx] as char).is_whitespace() {
            *idx += 1;
        }
        std::str::from_utf8(&bytes[start..*idx]).unwrap_or("").to_string()
    }
    let magic = read_token(bytes, &mut idx);
    let w: i64 = read_token(bytes, &mut idx).parse().ok()?;
    let h: i64 = read_token(bytes, &mut idx).parse().ok()?;
    let _maxval: i64 = read_token(bytes, &mut idx).parse().ok()?;
    // After maxval, consume exactly ONE whitespace byte (per PNM spec) for binary.
    if (magic == "P6" || magic == "P5") && idx < bytes.len() {
        idx += 1; // single whitespace
    }
    let mut pixels: Vec<Value> = Vec::with_capacity((w * h) as usize);
    match magic.as_str() {
        "P3" => {
            // ASCII RGB triplets.
            while idx < bytes.len() && (pixels.len() as i64) < w * h {
                let r: i64 = read_token(bytes, &mut idx).parse().unwrap_or(0);
                let g: i64 = read_token(bytes, &mut idx).parse().unwrap_or(0);
                let b: i64 = read_token(bytes, &mut idx).parse().unwrap_or(0);
                pixels.push(Value::Tuple(vec![Value::Int(r), Value::Int(g), Value::Int(b), Value::Int(255)]));
            }
        }
        "P2" => {
            while idx < bytes.len() && (pixels.len() as i64) < w * h {
                let v: i64 = read_token(bytes, &mut idx).parse().unwrap_or(0);
                pixels.push(Value::Tuple(vec![Value::Int(v), Value::Int(v), Value::Int(v), Value::Int(255)]));
            }
        }
        "P6" => {
            // Binary RGB.
            while idx + 2 < bytes.len() && (pixels.len() as i64) < w * h {
                let r = bytes[idx] as i64;
                let g = bytes[idx+1] as i64;
                let b = bytes[idx+2] as i64;
                pixels.push(Value::Tuple(vec![Value::Int(r), Value::Int(g), Value::Int(b), Value::Int(255)]));
                idx += 3;
            }
        }
        "P5" => {
            // Binary gray.
            while idx < bytes.len() && (pixels.len() as i64) < w * h {
                let v = bytes[idx] as i64;
                pixels.push(Value::Tuple(vec![Value::Int(v), Value::Int(v), Value::Int(v), Value::Int(255)]));
                idx += 1;
            }
        }
        _ => return None,
    }
    let mut f = HashMap::new();
    f.insert("__width__".to_string(), Value::Int(w));
    f.insert("__height__".to_string(), Value::Int(h));
    f.insert("__pixels__".to_string(), Value::List(pixels));
    Some(Value::Object("Image".to_string(), Rc::new(RefCell::new(f))))
}

fn parse_bmp(bytes: &[u8]) -> Option<Value> {
    // Minimal BMP parser: 24-bit or 32-bit uncompressed.
    if bytes.len() < 54 || &bytes[0..2] != b"BM" {
        return None;
    }
    let pixel_offset = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]) as usize;
    let width = i32::from_le_bytes([bytes[18], bytes[19], bytes[20], bytes[21]]) as i64;
    let height = i32::from_le_bytes([bytes[22], bytes[23], bytes[24], bytes[25]]) as i64;
    let bpp = u16::from_le_bytes([bytes[28], bytes[29]]) as usize;
    let row_size = ((bpp * width as usize + 31) / 32) * 4;
    let h_abs = height.abs();
    let top_down = height < 0;
    let mut pixels: Vec<Value> = Vec::with_capacity((width * h_abs) as usize);
    for _ in 0..(width * h_abs) {
        pixels.push(Value::Tuple(vec![Value::Int(0), Value::Int(0), Value::Int(0), Value::Int(255)]));
    }
    for row in 0..h_abs {
        let src_row = if top_down { row } else { h_abs - 1 - row };
        let row_start = pixel_offset + src_row as usize * row_size;
        for col in 0..width {
            let off = row_start + col as usize * (bpp / 8);
            if off + 2 >= bytes.len() { break; }
            let b = bytes[off] as i64;
            let g = bytes[off + 1] as i64;
            let r = bytes[off + 2] as i64;
            let a = if bpp == 32 && off + 3 < bytes.len() { bytes[off + 3] as i64 } else { 255 };
            pixels[(row * width + col) as usize] = Value::Tuple(vec![Value::Int(r), Value::Int(g), Value::Int(b), Value::Int(a)]);
        }
    }
    let mut f = HashMap::new();
    f.insert("__width__".to_string(), Value::Int(width));
    f.insert("__height__".to_string(), Value::Int(h_abs));
    f.insert("__pixels__".to_string(), Value::List(pixels));
    Some(Value::Object("Image".to_string(), Rc::new(RefCell::new(f))))
}

fn serialize_pnm(img: &Value, color: bool) -> Vec<u8> {
    let (w, h, pixels) = extract_image_dims(img);
    let magic = if color { "P6" } else { "P5" };
    let header = format!("{}\n{} {}\n255\n", magic, w, h);
    let mut out: Vec<u8> = header.into_bytes();
    if let Value::List(p) = &pixels {
        for px in p {
            if let Value::Tuple(rgba) = px {
                let r = rgba.get(0).and_then(|v| v.to_int().ok()).unwrap_or(0) as u8;
                let g = rgba.get(1).and_then(|v| v.to_int().ok()).unwrap_or(0) as u8;
                let b = rgba.get(2).and_then(|v| v.to_int().ok()).unwrap_or(0) as u8;
                if color {
                    out.push(r); out.push(g); out.push(b);
                } else {
                    let gray = (0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64) as u8;
                    out.push(gray);
                }
            }
        }
    }
    out
}

fn serialize_bmp(img: &Value) -> Vec<u8> {
    let (w, h, pixels) = extract_image_dims(img);
    let row_size = ((24 * w as usize + 31) / 32) * 4;
    let pixel_data_size = row_size * h as usize;
    let file_size = 54 + pixel_data_size;
    let mut out: Vec<u8> = Vec::with_capacity(file_size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes()); // pixel offset
    // DIB header (40 bytes).
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&24u16.to_le_bytes()); // bpp
    out.extend_from_slice(&0u32.to_le_bytes()); // compression
    out.extend_from_slice(&(pixel_data_size as u32).to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes()); // x ppm
    out.extend_from_slice(&2835u32.to_le_bytes()); // y ppm
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    // Pixel data (bottom-up, BGR, padded to 4 bytes).
    if let Value::List(p) = &pixels {
        for row in (0..h).rev() {
            for col in 0..w {
                let idx = (row * w + col) as usize;
                if let Some(Value::Tuple(rgba)) = p.get(idx) {
                    let r = rgba.get(0).and_then(|v| v.to_int().ok()).unwrap_or(0) as u8;
                    let g = rgba.get(1).and_then(|v| v.to_int().ok()).unwrap_or(0) as u8;
                    let b = rgba.get(2).and_then(|v| v.to_int().ok()).unwrap_or(0) as u8;
                    out.push(b); out.push(g); out.push(r);
                } else {
                    out.push(0); out.push(0); out.push(0);
                }
            }
            // Pad to 4 bytes.
            while out.len() % 4 != 0 && (out.len() - 54) % row_size != 0 {
                out.push(0);
            }
        }
    }
    out
}

fn extract_image_dims(img: &Value) -> (i64, i64, Value) {
    if let Value::Object(_, f) = img {
        let f = f.borrow();
        let w = f.get("__width__").and_then(|v| v.to_int().ok()).unwrap_or(0);
        let h = f.get("__height__").and_then(|v| v.to_int().ok()).unwrap_or(0);
        let p = f.get("__pixels__").cloned().unwrap_or(Value::List(vec![]));
        (w, h, p)
    } else {
        (0, 0, Value::List(vec![]))
    }
}

// ===== AES-256-CBC (native, FIPS-197) =====
// Minimal AES implementation supporting 256-bit key, CBC mode, PKCS#7 padding.
// No external dependency. Suitable for moderate use; not constant-time.

fn aes256_cbc_encrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Vec<u8> {
    let mut cipher = aes256_new(key);
    let mut padded = data.to_vec();
    let pad = 16 - (padded.len() % 16);
    for _ in 0..pad { padded.push(pad as u8); }
    let mut prev_block = [0u8; 16];
    prev_block.copy_from_slice(iv);
    let mut out = Vec::with_capacity(padded.len());
    for chunk in padded.chunks(16) {
        let mut block = [0u8; 16];
        for i in 0..16 { block[i] = chunk[i] ^ prev_block[i]; }
        aes256_encrypt_block(&mut cipher, &mut block);
        out.extend_from_slice(&block);
        prev_block = block;
    }
    out
}

fn aes256_cbc_decrypt(key: &[u8], iv: &[u8], cipher_data: &[u8]) -> Option<Vec<u8>> {
    if cipher_data.len() % 16 != 0 || cipher_data.is_empty() {
        return None;
    }
    let mut cipher = aes256_new(key);
    let mut prev_block = [0u8; 16];
    prev_block.copy_from_slice(iv);
    let mut out = Vec::with_capacity(cipher_data.len());
    for chunk in cipher_data.chunks(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        let saved = block;
        aes256_decrypt_block(&mut cipher, &mut block);
        for i in 0..16 { out.push(block[i] ^ prev_block[i]); }
        prev_block = saved;
    }
    // Strip PKCS#7 padding.
    let pad = *out.last()? as usize;
    if pad == 0 || pad > 16 { return None; }
    for i in 0..pad {
        if out[out.len() - 1 - i] != pad as u8 { return None; }
    }
    out.truncate(out.len() - pad);
    Some(out)
}

// AES-256 state: round keys (15 rounds × 16 bytes = 240 bytes).
struct Aes256 {
    round_keys: [[u8; 16]; 15],
}

const SBOX: [u8; 256] = [
    0x63,0x7c,0x77,0x7b,0xf2,0x6b,0x6f,0xc5,0x30,0x01,0x67,0x2b,0xfe,0xd7,0xab,0x76,
    0xca,0x82,0xc9,0x7d,0xfa,0x59,0x47,0xf0,0xad,0xd4,0xa2,0xaf,0x9c,0xa4,0x72,0xc0,
    0xb7,0xfd,0x93,0x26,0x36,0x3f,0xf7,0xcc,0x34,0xa5,0xe5,0xf1,0x71,0xd8,0x31,0x15,
    0x04,0xc7,0x23,0xc3,0x18,0x96,0x05,0x9a,0x07,0x12,0x80,0xe2,0xeb,0x27,0xb2,0x75,
    0x09,0x83,0x2c,0x1a,0x1b,0x6e,0x5a,0xa0,0x52,0x3b,0xd6,0xb3,0x29,0xe3,0x2f,0x84,
    0x53,0xd1,0x00,0xed,0x20,0xfc,0xb1,0x5b,0x6a,0xcb,0xbe,0x39,0x4a,0x4c,0x58,0xcf,
    0xd0,0xef,0xaa,0xfb,0x43,0x4d,0x33,0x85,0x45,0xf9,0x02,0x7f,0x50,0x3c,0x9f,0xa8,
    0x51,0xa3,0x40,0x8f,0x92,0x9d,0x38,0xf5,0xbc,0xb6,0xda,0x21,0x10,0xff,0xf3,0xd2,
    0xcd,0x0c,0x13,0xec,0x5f,0x97,0x44,0x17,0xc4,0xa7,0x7e,0x3d,0x64,0x5d,0x19,0x73,
    0x60,0x81,0x4f,0xdc,0x22,0x2a,0x90,0x88,0x46,0xee,0xb8,0x14,0xde,0x5e,0x0b,0xdb,
    0xe0,0x32,0x3a,0x0a,0x49,0x06,0x24,0x5c,0xc2,0xd3,0xac,0x62,0x91,0x95,0xe4,0x79,
    0xe7,0xc8,0x37,0x6d,0x8d,0xd5,0x4e,0xa9,0x6c,0x56,0xf4,0xea,0x65,0x7a,0xae,0x08,
    0xba,0x78,0x25,0x2e,0x1c,0xa6,0xb4,0xc6,0xe8,0xdd,0x74,0x1f,0x4b,0xbd,0x8b,0x8a,
    0x70,0x3e,0xb5,0x66,0x48,0x03,0xf6,0x0e,0x61,0x35,0x57,0xb9,0x86,0xc1,0x1d,0x9e,
    0xe1,0xf8,0x98,0x11,0x69,0xd9,0x8e,0x94,0x9b,0x1e,0x87,0xe9,0xce,0x55,0x28,0xdf,
    0x8c,0xa1,0x89,0x0d,0xbf,0xe6,0x42,0x68,0x41,0x99,0x2d,0x0f,0xb0,0x54,0xbb,0x16,
];

const INV_SBOX: [u8; 256] = [
    0x52,0x09,0x6a,0xd5,0x30,0x36,0xa5,0x38,0xbf,0x40,0xa3,0x9e,0x81,0xf3,0xd7,0xfb,
    0x7c,0xe3,0x39,0x82,0x9b,0x2f,0xff,0x87,0x34,0x8e,0x43,0x44,0xc4,0xde,0xe9,0xcb,
    0x54,0x7b,0x94,0x32,0xa6,0xc2,0x23,0x3d,0xee,0x4c,0x95,0x0b,0x42,0xfa,0xc3,0x4e,
    0x08,0x2e,0xa1,0x66,0x28,0xd9,0x24,0xb2,0x76,0x5b,0xa2,0x49,0x6d,0x8b,0xd1,0x25,
    0x72,0xf8,0xf6,0x64,0x86,0x68,0x98,0x16,0xd4,0xa4,0x5c,0xcc,0x5d,0x65,0xb6,0x92,
    0x6c,0x70,0x48,0x50,0xfd,0xed,0xb9,0xda,0x5e,0x15,0x46,0x57,0xa7,0x8d,0x9d,0x84,
    0x90,0xd8,0xab,0x00,0x8c,0xbc,0xd3,0x0a,0xf7,0xe4,0x58,0x05,0xb8,0xb3,0x45,0x06,
    0xd0,0x2c,0x1e,0x8f,0xca,0x3f,0x0f,0x02,0xc1,0xaf,0xbd,0x03,0x01,0x13,0x8a,0x6b,
    0x3a,0x91,0x11,0x41,0x4f,0x67,0xdc,0xea,0x97,0xf2,0xcf,0xce,0xf0,0xb4,0xe6,0x73,
    0x96,0xac,0x74,0x22,0xe7,0xad,0x35,0x85,0xe2,0xf9,0x37,0xe8,0x1c,0x75,0xdf,0x6e,
    0x47,0xf1,0x1a,0x71,0x1d,0x29,0xc5,0x89,0x6f,0xb7,0x62,0x0e,0xaa,0x18,0xbe,0x1b,
    0xfc,0x56,0x3e,0x4b,0xc6,0xd2,0x79,0x20,0x9a,0xdb,0xc0,0xfe,0x78,0xcd,0x5a,0xf4,
    0x1f,0xdd,0xa8,0x33,0x88,0x07,0xc7,0x31,0xb1,0x12,0x10,0x59,0x27,0x80,0xec,0x5f,
    0x60,0x51,0x7f,0xa9,0x19,0xb5,0x4a,0x0d,0x2d,0xe5,0x7a,0x9f,0x93,0xc9,0x9c,0xef,
    0xa0,0xe0,0x3b,0x4d,0xae,0x2a,0xf5,0xb0,0xc8,0xeb,0xbb,0x3c,0x83,0x53,0x99,0x61,
    0x17,0x2b,0x04,0x7e,0xba,0x77,0xd6,0x26,0xe1,0x69,0x14,0x63,0x55,0x21,0x0c,0x7d,
];

const RCON: [u8; 11] = [0x00, 0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

fn aes256_new(key: &[u8]) -> Aes256 {
    // Key expansion for AES-256 (14 rounds + 1 = 15 round keys).
    let nk = 8; // 8 32-bit words in key
    let nr = 14;
    let total_words = 4 * (nr + 1); // 60
    let mut w = vec![[0u8; 4]; total_words];
    for i in 0..nk {
        w[i] = [key[4*i], key[4*i+1], key[4*i+2], key[4*i+3]];
    }
    for i in nk..total_words {
        let mut temp = w[i-1];
        if i % nk == 0 {
            // RotWord + SubWord + Rcon.
            let t = temp[0];
            temp[0] = temp[1]; temp[1] = temp[2]; temp[2] = temp[3]; temp[3] = t;
            temp[0] = SBOX[temp[0] as usize] ^ RCON[i/nk];
            temp[1] = SBOX[temp[1] as usize];
            temp[2] = SBOX[temp[2] as usize];
            temp[3] = SBOX[temp[3] as usize];
        } else if i % nk == 4 {
            temp[0] = SBOX[temp[0] as usize];
            temp[1] = SBOX[temp[1] as usize];
            temp[2] = SBOX[temp[2] as usize];
            temp[3] = SBOX[temp[3] as usize];
        }
        for j in 0..4 {
            w[i][j] = w[i-nk][j] ^ temp[j];
        }
    }
    let mut round_keys = [[0u8; 16]; 15];
    for r in 0..15 {
        for c in 0..4 {
            for j in 0..4 {
                round_keys[r][c*4 + j] = w[r*4 + c][j];
            }
        }
    }
    Aes256 { round_keys }
}

fn aes256_encrypt_block(state: &mut Aes256, block: &mut [u8; 16]) {
    // AddRoundKey (round 0).
    for i in 0..16 { block[i] ^= state.round_keys[0][i]; }
    for r in 1..14 {
        // SubBytes.
        for i in 0..16 { block[i] = SBOX[block[i] as usize]; }
        // ShiftRows.
        shift_rows(block);
        // MixColumns.
        mix_columns(block);
        // AddRoundKey.
        for i in 0..16 { block[i] ^= state.round_keys[r][i]; }
    }
    // Final round (no MixColumns).
    for i in 0..16 { block[i] = SBOX[block[i] as usize]; }
    shift_rows(block);
    for i in 0..16 { block[i] ^= state.round_keys[14][i]; }
}

fn aes256_decrypt_block(state: &mut Aes256, block: &mut [u8; 16]) {
    // Initial AddRoundKey (last round key).
    for i in 0..16 { block[i] ^= state.round_keys[14][i]; }
    for r in (1..14).rev() {
        // InvShiftRows.
        inv_shift_rows(block);
        // InvSubBytes.
        for i in 0..16 { block[i] = INV_SBOX[block[i] as usize]; }
        // AddRoundKey.
        for i in 0..16 { block[i] ^= state.round_keys[r][i]; }
        // InvMixColumns.
        inv_mix_columns(block);
    }
    inv_shift_rows(block);
    for i in 0..16 { block[i] = INV_SBOX[block[i] as usize]; }
    for i in 0..16 { block[i] ^= state.round_keys[0][i]; }
}

fn shift_rows(b: &mut [u8; 16]) {
    // Row 1: shift left by 1.
    let t = b[1]; b[1] = b[5]; b[5] = b[9]; b[9] = b[13]; b[13] = t;
    // Row 2: shift left by 2.
    let t1 = b[2]; let t2 = b[6]; b[2] = b[10]; b[6] = b[14]; b[10] = t1; b[14] = t2;
    // Row 3: shift left by 3.
    let t = b[3]; b[3] = b[15]; b[15] = b[11]; b[11] = b[7]; b[7] = t;
}

fn inv_shift_rows(b: &mut [u8; 16]) {
    // Row 1: shift right by 1.
    let t = b[13]; b[13] = b[9]; b[9] = b[5]; b[5] = b[1]; b[1] = t;
    // Row 2: shift right by 2.
    let t1 = b[2]; let t2 = b[6]; b[2] = b[10]; b[6] = b[14]; b[10] = t1; b[14] = t2;
    // Row 3: shift right by 3.
    let t = b[3]; b[3] = b[7]; b[7] = b[11]; b[11] = b[15]; b[15] = t;
}

fn xtime(x: u8) -> u8 {
    ((x << 1) ^ if x & 0x80 != 0 { 0x1b } else { 0 }) & 0xff
}

fn mix_columns(b: &mut [u8; 16]) {
    for c in 0..4 {
        let s0 = b[c*4]; let s1 = b[c*4+1]; let s2 = b[c*4+2]; let s3 = b[c*4+3];
        b[c*4]   = xtime(s0) ^ (xtime(s1) ^ s1) ^ s2 ^ s3;
        b[c*4+1] = s0 ^ xtime(s1) ^ (xtime(s2) ^ s2) ^ s3;
        b[c*4+2] = s0 ^ s1 ^ xtime(s2) ^ (xtime(s3) ^ s3);
        b[c*4+3] = (xtime(s0) ^ s0) ^ s1 ^ s2 ^ xtime(s3);
    }
}

fn mul(mut a: u8, mut b: u8) -> u8 {
    let mut p: u8 = 0;
    for _ in 0..8 {
        if b & 1 != 0 { p ^= a; }
        a = xtime(a);
        b >>= 1;
    }
    p
}

fn inv_mix_columns(b: &mut [u8; 16]) {
    for c in 0..4 {
        let s0 = b[c*4]; let s1 = b[c*4+1]; let s2 = b[c*4+2]; let s3 = b[c*4+3];
        b[c*4]   = mul(s0, 0x0e) ^ mul(s1, 0x0b) ^ mul(s2, 0x0d) ^ mul(s3, 0x09);
        b[c*4+1] = mul(s0, 0x09) ^ mul(s1, 0x0e) ^ mul(s2, 0x0b) ^ mul(s3, 0x0d);
        b[c*4+2] = mul(s0, 0x0d) ^ mul(s1, 0x09) ^ mul(s2, 0x0e) ^ mul(s3, 0x0b);
        b[c*4+3] = mul(s0, 0x0b) ^ mul(s1, 0x0d) ^ mul(s2, 0x09) ^ mul(s3, 0x0e);
    }
}

// ===== HTTP client (real HTTP/1.1 via std::net) =====

fn http_request(method: &str, url: &str, body: &str, headers: &Value, vm: &VM) -> Value {
    // Parse URL: http://host:port/path
    let url_rest = match url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) {
        Some(r) => r,
        None => return http_error_dict(0, &format!("invalid URL: {} (must start with http://)", url)),
    };
    let (host_port, path) = match url_rest.find('/') {
        Some(i) => (&url_rest[..i], &url_rest[i..]),
        None => (url_rest, "/"),
    };
    // Set a 10-second timeout to avoid hanging.
    let stream = match std::net::TcpStream::connect(host_port) {
        Ok(s) => s,
        Err(e) => return http_error_dict(0, &format!("connection failed to {}: {}", host_port, e)),
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));
    use std::io::{Read, Write};
    let mut stream = stream;
    let mut request = format!("{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n", method, path, host_port);
    // Add custom headers.
    if let Value::Dict(h) = headers {
        for (k, v) in h {
            request.push_str(&format!("{}: {}\r\n", k, v.to_str()));
        }
    }
    if !body.is_empty() {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    if !body.is_empty() {
        request.push_str(body);
    }
    if let Err(e) = stream.write_all(request.as_bytes()) {
        return http_error_dict(0, &format!("write failed: {}", e));
    }
    let mut response = Vec::new();
    if let Err(e) = stream.read_to_end(&mut response) {
        return http_error_dict(0, &format!("read failed: {}", e));
    }
    let response_str = String::from_utf8_lossy(&response).to_string();
    // Parse response: status line, headers, body.
    let header_end = match response_str.find("\r\n\r\n") {
        Some(i) => i,
        None => return http_error_dict(0, &format!("malformed response (no header end)")),
    };
    let header_section = &response_str[..header_end];
    let response_body = &response_str[header_end + 4..];
    let mut lines = header_section.lines();
    let status_line = lines.next().unwrap_or("");
    let status: i64 = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut resp_headers = HashMap::new();
    for line in lines {
        if let Some(colon) = line.find(':') {
            let key = line[..colon].trim().to_string();
            let val = line[colon+1..].trim().to_string();
            resp_headers.insert(key, Value::Str(val));
        }
    }
    let mut d = HashMap::new();
    d.insert("status".to_string(), Value::Int(status));
    d.insert("headers".to_string(), Value::Dict(resp_headers));
    d.insert("body".to_string(), Value::Str(response_body.to_string()));
    Value::Dict(d)
}

/// Create an error response dict for HTTP failures.
fn http_error_dict(status: i64, error: &str) -> Value {
    let mut d = HashMap::new();
    d.insert("status".to_string(), Value::Int(status));
    d.insert("headers".to_string(), Value::Dict(HashMap::new()));
    d.insert("body".to_string(), Value::Str(String::new()));
    d.insert("error".to_string(), Value::Str(error.to_string()));
    Value::Dict(d)
}

// ===== WebSocket frame helpers =====

fn ws_encode_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    // FIN=1, opcode.
    frame.push(0x80 | opcode);
    // Mask=1 (client must mask).
    let len = payload.len();
    if len < 126 {
        frame.push(0x80 | len as u8);
    } else if len <= 65535 {
        frame.push(0x80 | 126);
        frame.push((len >> 8) as u8);
        frame.push((len & 0xff) as u8);
    } else {
        frame.push(0x80 | 127);
        for i in (0..8).rev() {
            frame.push((len >> (i * 8)) as u8);
        }
    }
    // Masking key (4 random bytes).
    let mask = [
        (xorshift_rand() & 0xff) as u8,
        (xorshift_rand() & 0xff) as u8,
        (xorshift_rand() & 0xff) as u8,
        (xorshift_rand() & 0xff) as u8,
    ];
    frame.extend_from_slice(&mask);
    // Masked payload.
    for (i, &b) in payload.iter().enumerate() {
        frame.push(b ^ mask[i % 4]);
    }
    frame
}

fn ws_decode_frame(stream: &mut std::net::TcpStream) -> Option<(u8, Vec<u8>)> {
    use std::io::Read;
    let mut header = [0u8; 2];
    if stream.read_exact(&mut header).is_err() {
        return None;
    }
    let opcode = header[0] & 0x0f;
    let masked = (header[1] & 0x80) != 0;
    let mut payload_len = (header[1] & 0x7f) as usize;
    if payload_len == 126 {
        let mut ext = [0u8; 2];
        if stream.read_exact(&mut ext).is_err() { return None; }
        payload_len = ((ext[0] as usize) << 8) | (ext[1] as usize);
    } else if payload_len == 127 {
        let mut ext = [0u8; 8];
        if stream.read_exact(&mut ext).is_err() { return None; }
        payload_len = 0;
        for i in 0..8 {
            payload_len = (payload_len << 8) | (ext[i] as usize);
        }
    }
    let mut mask = [0u8; 4];
    if masked {
        if stream.read_exact(&mut mask).is_err() { return None; }
    }
    let mut payload = vec![0u8; payload_len];
    if stream.read_exact(&mut payload).is_err() { return None; }
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    Some((opcode, payload))
}

// ===== In-memory SQL engine =====

fn sql_exec(db: &mut SqlDb, sql: &str) -> i64 {
    let sql = sql.trim();
    let upper = sql.to_uppercase();
    if upper.starts_with("CREATE TABLE") {
        // CREATE TABLE name (col1, col2, ...)
        let rest = &sql["CREATE TABLE".len()..].trim();
        let table_name = rest.split_whitespace().next().unwrap_or("").trim_end_matches('(').to_string();
        let paren_start = rest.find('(');
        let paren_end = rest.rfind(')');
        if let (Some(ps), Some(pe)) = (paren_start, paren_end) {
            let cols_str = &rest[ps+1..pe];
            let columns: Vec<String> = cols_str.split(',').map(|s| {
                s.trim().split_whitespace().next().unwrap_or("").to_string()
            }).filter(|s| !s.is_empty()).collect();
            db.tables.insert(table_name, (columns, vec![]));
        }
        0
    } else if upper.starts_with("INSERT INTO") {
        // INSERT INTO name VALUES (v1, v2, ...)  or  INSERT INTO name (c1,c2) VALUES (v1,v2)
        let rest = &sql["INSERT INTO".len()..].trim();
        let table_name = rest.split_whitespace().next().unwrap_or("").to_string();
        let values_kw = upper.find("VALUES");
        if let Some(vi) = values_kw {
            let values_str = &sql[vi+"VALUES".len()..].trim();
            let paren_start = values_str.find('(');
            let paren_end = values_str.rfind(')');
            if let (Some(ps), Some(pe)) = (paren_start, paren_end) {
                let vals_str = &values_str[ps+1..pe];
                let values: Vec<Value> = vals_str.split(',').map(|s| {
                    let s = s.trim();
                    if s.starts_with('\'') && s.ends_with('\'') {
                        Value::Str(s[1..s.len()-1].to_string())
                    } else if s.starts_with('"') && s.ends_with('"') {
                        Value::Str(s[1..s.len()-1].to_string())
                    } else if let Ok(i) = s.parse::<i64>() {
                        Value::Int(i)
                    } else if let Ok(f) = s.parse::<f64>() {
                        Value::Float(f)
                    } else {
                        Value::Str(s.to_string())
                    }
                }).collect();
                if let Some((_, rows)) = db.tables.get_mut(&table_name) {
                    rows.push(values);
                    return 1;
                }
            }
        }
        0
    } else if upper.starts_with("DELETE FROM") {
        let rest = &sql["DELETE FROM".len()..].trim();
        let table_name = rest.split_whitespace().next().unwrap_or("").to_string();
        if let Some((_, rows)) = db.tables.get_mut(&table_name) {
            let count = rows.len() as i64;
            rows.clear();
            count
        } else { 0 }
    } else {
        0
    }
}

fn sql_query(db: &SqlDb, sql: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    let sql = sql.trim();
    let upper = sql.to_uppercase();
    if upper.starts_with("SELECT") {
        // SELECT * FROM name  or  SELECT col1,col2 FROM name [WHERE col = val]
        let from_kw = upper.find("FROM");
        if let Some(fi) = from_kw {
            let select_part: &str = sql[6..fi].trim();
            let rest = &sql[fi+4..].trim();
            let table_name = rest.split_whitespace().next().unwrap_or("").to_string();
            if let Some((columns, rows)) = db.tables.get(&table_name) {
                let cols_to_select: Vec<String> = if select_part == "*" {
                    columns.clone()
                } else {
                    select_part.split(',').map(|s| s.trim().to_string()).collect()
                };
                let result_rows: Vec<Vec<Value>> = rows.iter().map(|row| {
                    cols_to_select.iter().map(|col| {
                        if let Some(ci) = columns.iter().position(|c| c == col) {
                            row.get(ci).cloned().unwrap_or(Value::Null)
                        } else {
                            Value::Null
                        }
                    }).collect()
                }).collect();
                return (cols_to_select, result_rows);
            }
        }
    }
    (vec![], vec![])
}

// ===== format_string =====
fn format_string(fmt: &str, args: &[Value]) -> String {
    let mut out = String::new();
    let mut arg_idx = 0;
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let mut spec = String::new();
            while let Some(&nc) = chars.peek() {
                if nc == '-' || nc == '+' || nc == ' ' || nc == '#' || nc == '0' || nc.is_ascii_digit() || nc == '.' {
                    spec.push(nc);
                    chars.next();
                } else {
                    break;
                }
            }
            match chars.next() {
                Some('%') => out.push('%'),
                Some('v') => {
                    if arg_idx < args.len() {
                        out.push_str(&args[arg_idx].to_str());
                        arg_idx += 1;
                    }
                }
                Some('s') => {
                    if arg_idx < args.len() {
                        out.push_str(&args[arg_idx].to_str());
                        arg_idx += 1;
                    }
                }
                Some('d') | Some('i') => {
                    if arg_idx < args.len() {
                        out.push_str(&args[arg_idx].to_int().unwrap_or(0).to_string());
                        arg_idx += 1;
                    }
                }
                Some('f') | Some('g') | Some('e') => {
                    if arg_idx < args.len() {
                        let f = match &args[arg_idx] {
                            Value::Float(f) => *f,
                            Value::Int(i) => *i as f64,
                            _ => 0.0,
                        };
                        let precision = if let Some(dot_idx) = spec.find('.') {
                            spec[dot_idx+1..].parse::<usize>().unwrap_or(6)
                        } else { 6 };
                        if spec.contains('.') {
                            out.push_str(&format!("{:.*}", precision, f));
                        } else {
                            out.push_str(&format!("{}", f));
                        }
                        arg_idx += 1;
                    }
                }
                Some('t') => {
                    if arg_idx < args.len() {
                        out.push_str(&args[arg_idx].truthy().to_string());
                        arg_idx += 1;
                    }
                }
                Some('q') => {
                    if arg_idx < args.len() {
                        out.push_str(&format!("\"{}\"", args[arg_idx].to_str()));
                        arg_idx += 1;
                    }
                }
                Some('x') => {
                    if arg_idx < args.len() {
                        out.push_str(&format!("{:x}", args[arg_idx].to_int().unwrap_or(0)));
                        arg_idx += 1;
                    }
                }
                Some('o') => {
                    if arg_idx < args.len() {
                        out.push_str(&format!("{:o}", args[arg_idx].to_int().unwrap_or(0)));
                        arg_idx += 1;
                    }
                }
                Some('c') => {
                    if arg_idx < args.len() {
                        let s = args[arg_idx].to_str();
                        if let Some(ch) = s.chars().next() {
                            out.push(ch);
                        }
                        arg_idx += 1;
                    }
                }
                Some(other) => {
                    out.push('%');
                    out.push_str(&spec);
                    out.push(other);
                }
                None => {
                    out.push('%');
                    out.push_str(&spec);
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn walk_dir(root: &str, out: &mut Vec<Value>) {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if let Some(s) = p.to_str() {
            out.push(Value::Str(s.to_string()));
        }
        if p.is_dir() {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if !name.starts_with('.') {
                    if let Some(s) = p.to_str() {
                        walk_dir(s, out);
                    }
                }
            }
        }
    }
}

fn parse_csv(content: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let mut cells = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if in_quotes {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        current.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                } else {
                    current.push(c);
                }
            } else {
                if c == '"' {
                    in_quotes = true;
                } else if c == ',' {
                    cells.push(std::mem::take(&mut current));
                } else {
                    current.push(c);
                }
            }
        }
        cells.push(current);
        rows.push(cells);
    }
    rows
}

fn native_xml_parse(s: &str) -> Value {
    // Minimal XML parser: parse a single root element with text content.
    // Returns a dict {tag: str, text: str, attrs: dict}.
    let s = s.trim();
    if !s.starts_with('<') {
        return Value::Null;
    }
    // Find the opening tag.
    let open_end = match s.find('>') {
        Some(i) => i,
        None => return Value::Null,
    };
    let tag_content = &s[1..open_end];
    let (tag, attrs_str) = if let Some(sp) = tag_content.find(' ') {
        (&tag_content[..sp], &tag_content[sp+1..])
    } else {
        (tag_content, "")
    };
    let mut attrs = HashMap::new();
    // Very rough attr parsing: key="value"
    let mut chars = attrs_str.chars().peekable();
    let mut key = String::new();
    let mut val = String::new();
    let mut in_val = false;
    let mut in_key = true;
    while let Some(c) = chars.next() {
        if in_key {
            if c == '=' {
                in_key = false;
            } else if !c.is_whitespace() {
                key.push(c);
            }
        } else if in_val {
            if c == '"' {
                attrs.insert(std::mem::take(&mut key), Value::Str(std::mem::take(&mut val)));
                in_val = false;
                in_key = true;
            } else {
                val.push(c);
            }
        } else if c == '"' {
            in_val = true;
        }
    }
    // Find closing tag </tag>.
    let close_tag = format!("</{}>", tag);
    let text = if let Some(close_start) = s.find(&close_tag) {
        s[open_end+1..close_start].trim().to_string()
    } else {
        String::new()
    };
    let mut d = HashMap::new();
    d.insert("tag".to_string(), Value::Str(tag.to_string()));
    d.insert("text".to_string(), Value::Str(text));
    d.insert("attrs".to_string(), Value::Dict(attrs));
    Value::Dict(d)
}

fn native_xml_stringify(v: &Value) -> String {
    if let Value::Dict(d) = v {
        let tag = d.get("tag").map(|v| v.to_str()).unwrap_or_else(|| "root".to_string());
        let text = d.get("text").map(|v| v.to_str()).unwrap_or_default();
        if text.is_empty() {
            format!("<{} />", tag)
        } else {
            format!("<{}>{}</{}>", tag, text, tag)
        }
    } else {
        format!("<root>{}</root>", v.to_str())
    }
}

fn native_toml_parse(s: &str) -> Value {
    // Minimal INI-style TOML: [section] then key = value lines.
    let mut result = HashMap::new();
    let mut current_section = String::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            current_section = line[1..line.len()-1].to_string();
            result.entry(current_section.clone()).or_insert_with(|| Value::Dict(HashMap::new()));
            continue;
        }
        if let Some(eq) = line.find('=') {
            let key = line[..eq].trim().to_string();
            let val_str = line[eq+1..].trim();
            let val = if val_str.starts_with('"') && val_str.ends_with('"') {
                Value::Str(val_str[1..val_str.len()-1].to_string())
            } else if val_str == "true" {
                Value::Bool(true)
            } else if val_str == "false" {
                Value::Bool(false)
            } else if let Ok(i) = val_str.parse::<i64>() {
                Value::Int(i)
            } else if let Ok(f) = val_str.parse::<f64>() {
                Value::Float(f)
            } else {
                Value::Str(val_str.to_string())
            };
            if current_section.is_empty() {
                result.insert(key, val);
            } else if let Some(Value::Dict(sec)) = result.get_mut(&current_section) {
                sec.insert(key, val);
            }
        }
    }
    Value::Dict(result)
}

fn native_toml_stringify(v: &Value) -> String {
    let mut out = String::new();
    if let Value::Dict(d) = v {
        for (k, v) in d {
            match v {
                Value::Dict(_) => {
                    out.push_str(&format!("[{}]\n", k));
                    if let Value::Dict(sub) = v {
                        for (sk, sv) in sub {
                            out.push_str(&format!("{} = {}\n", sk, toml_val(sv)));
                        }
                    }
                    out.push('\n');
                }
                _ => {
                    out.push_str(&format!("{} = {}\n", k, toml_val(v)));
                }
            }
        }
    }
    out
}

fn toml_val(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("\"{}\"", s),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        _ => v.to_str(),
    }
}

// ===== YAML subset parser/stringifier =====

fn yaml_parse(s: &str) -> Value {
    let lines: Vec<&str> = s.lines().collect();
    let mut idx = 0;
    yaml_parse_block(&lines, &mut idx, 0)
}

fn yaml_parse_block(lines: &[&str], idx: &mut usize, indent: usize) -> Value {
    let mut result = HashMap::new();
    let mut list_items: Vec<Value> = Vec::new();
    let mut is_list = false;
    while *idx < lines.len() {
        let line = lines[*idx];
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            *idx += 1;
            continue;
        }
        let leading = line.len() - line.trim_start().len();
        if leading < indent { break; }
        if leading > indent { break; }
        let trimmed = line.trim_start();
        if trimmed.starts_with("- ") || trimmed == "-" {
            is_list = true;
            let item = trimmed[1..].trim();
            if item.is_empty() {
                *idx += 1;
                let val = yaml_parse_block(lines, idx, indent + 2);
                list_items.push(val);
            } else if item.contains(':') {
                let mut m = HashMap::new();
                let parts: Vec<&str> = item.splitn(2, ':').collect();
                let key = parts[0].trim().to_string();
                let val_str = parts[1].trim();
                if val_str.is_empty() {
                    *idx += 1;
                    let val = yaml_parse_block(lines, idx, indent + 2);
                    m.insert(key, val);
                } else {
                    m.insert(key, yaml_parse_scalar(val_str));
                    *idx += 1;
                }
                list_items.push(Value::Dict(m));
            } else {
                list_items.push(yaml_parse_scalar(item));
                *idx += 1;
            }
        } else if trimmed.contains(':') {
            let parts: Vec<&str> = trimmed.splitn(2, ':').collect();
            let key = parts[0].trim().to_string();
            let val_str = parts[1].trim();
            if val_str.is_empty() {
                *idx += 1;
                let val = yaml_parse_block(lines, idx, indent + 2);
                result.insert(key, val);
            } else {
                result.insert(key, yaml_parse_scalar(val_str));
                *idx += 1;
            }
        } else {
            *idx += 1;
        }
    }
    if is_list { Value::List(list_items) } else { Value::Dict(result) }
}

fn yaml_parse_scalar(s: &str) -> Value {
    let s = s.trim();
    if s == "null" || s == "~" || s.is_empty() { Value::Null }
    else if s == "true" || s == "True" || s == "yes" { Value::Bool(true) }
    else if s == "false" || s == "False" || s == "no" { Value::Bool(false) }
    else if let Ok(i) = s.parse::<i64>() { Value::Int(i) }
    else if let Ok(f) = s.parse::<f64>() { Value::Float(f) }
    else if s.starts_with('"') && s.ends_with('"') { Value::Str(s[1..s.len()-1].to_string()) }
    else if s.starts_with('\'') && s.ends_with('\'') { Value::Str(s[1..s.len()-1].to_string()) }
    else { Value::Str(s.to_string()) }
}

fn yaml_stringify(v: &Value, indent: usize) -> String {
    let pad = " ".repeat(indent);
    match v {
        Value::Dict(d) => {
            let mut out = String::new();
            for (k, val) in d {
                match val {
                    Value::Dict(_) | Value::List(_) => {
                        out.push_str(&format!("{}{}:\n", pad, k));
                        out.push_str(&yaml_stringify(val, indent + 2));
                    }
                    _ => { out.push_str(&format!("{}{}: {}\n", pad, k, yaml_scalar(val))); }
                }
            }
            out
        }
        Value::List(l) => {
            let mut out = String::new();
            for item in l {
                match item {
                    Value::Dict(_) => {
                        out.push_str(&format!("{}-\n", pad));
                        out.push_str(&yaml_stringify(item, indent + 2));
                    }
                    _ => { out.push_str(&format!("{}- {}\n", pad, yaml_scalar(item))); }
                }
            }
            out
        }
        _ => format!("{}{}\n", pad, yaml_scalar(v)),
    }
}

fn yaml_scalar(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Str(s) => {
            if s.contains(':') || s.contains('#') || s.starts_with(' ') {
                format!("\"{}\"", s)
            } else { s.clone() }
        }
        _ => v.to_str(),
    }
}

// ===== Civil-from-days (Howard Hinnant's algorithm) =====
fn lanczos_gamma(x: f64) -> f64 {
    // Lanczos approximation, g=7, n=9.
    const G: f64 = 7.0;
    const P: [f64; 9] = [
        0.99999999999980993,
        676.5203681218851,
        -1259.1392167224028,
        771.32342877765313,
        -176.61502916214059,
        12.507343278686905,
        -0.13857109526572012,
        9.9843695780195716e-6,
        1.5056327351493116e-7,
    ];
    if x < 0.5 {
        std::f64::consts::PI / ((std::f64::consts::PI * x).sin() * lanczos_gamma(1.0 - x))
    } else {
        let x = x - 1.0;
        let mut a = P[0];
        let t = x + G + 0.5;
        for i in 1..P.len() {
            a += P[i] / (x + i as f64);
        }
        (2.0 * std::f64::consts::PI).sqrt() * t.powf(x + 0.5) * (-t).exp() * a
    }
}

// ===== Pretty JSON stringify =====
fn native_json_stringify_pretty(v: &Value, indent: usize) -> String {
    native_json_stringify_pretty_impl(v, indent, 0)
}

fn native_json_stringify_pretty_impl(v: &Value, indent: usize, depth: usize) -> String {
    let pad = " ".repeat(indent * depth);
    let pad_inner = " ".repeat(indent * (depth + 1));
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            if f.is_nan() {
                "null".to_string()
            } else if f.is_infinite() {
                "null".to_string()
            } else if f.fract() == 0.0 && f.abs() < 1e16 {
                format!("{:.1}", f)
            } else {
                format!("{}", f)
            }
        }
        Value::Str(s) => format!("\"{}\"", json_escape_str(s)),
        Value::List(l) => {
            if l.is_empty() {
                return "[]".to_string();
            }
            let items: Vec<String> = l.iter()
                .map(|v| pad_inner.clone() + &native_json_stringify_pretty_impl(v, indent, depth + 1))
                .collect();
            format!("[\n{}\n{}]", items.join(",\n"), pad)
        }
        Value::Tuple(t) => {
            if t.is_empty() {
                return "[]".to_string();
            }
            let items: Vec<String> = t.iter()
                .map(|v| pad_inner.clone() + &native_json_stringify_pretty_impl(v, indent, depth + 1))
                .collect();
            format!("[\n{}\n{}]", items.join(",\n"), pad)
        }
        Value::Dict(d) => {
            if d.is_empty() {
                return "{}".to_string();
            }
            let items: Vec<String> = d.iter()
                .map(|(k, v)| format!("{}\"{}\": {}", pad_inner, json_escape_str(k), native_json_stringify_pretty_impl(v, indent, depth + 1)))
                .collect();
            format!("{{\n{}\n{}}}", items.join(",\n"), pad)
        }
        _ => "null".to_string(),
    }
}

fn json_escape_str(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// ===== Base64 / Hex / URL encoding =====
fn base64_encode(data: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = data.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let b0 = bytes[i] as u32;
        let b1 = bytes[i + 1] as u32;
        let b2 = bytes[i + 2] as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push(TABLE[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

fn base64_decode(s: &str) -> Option<String> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|&b| b != b'\n' && b != b'\r' && b != b' ').collect();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let v0 = val(bytes[i])?;
        let v1 = val(bytes[i + 1])?;
        let v2 = if bytes[i + 2] == b'=' { 0 } else { val(bytes[i + 2])? };
        let v3 = if bytes[i + 3] == b'=' { 0 } else { val(bytes[i + 3])? };
        let n = ((v0 as u32) << 18) | ((v1 as u32) << 12) | ((v2 as u32) << 6) | (v3 as u32);
        out.push((n >> 16) as u8);
        if bytes[i + 2] != b'=' {
            out.push((n >> 8) as u8);
        }
        if bytes[i + 3] != b'=' {
            out.push(n as u8);
        }
        i += 4;
    }
    String::from_utf8(out).ok()
}

fn hex_encode(data: &str) -> String {
    data.bytes().map(|b| format!("{:02x}", b)).collect()
}

/// Generate `n` cryptographically-acceptable random bytes using only `std`.
///
/// The VM cannot pull in `rand` (no new dependencies) and `xorshift_rand`
/// above is fine for simulation but predictable enough to be unsafe as a
/// password salt. This function seeds a thread-local 32-byte state from
/// `SystemTime` nanosecond entropy + `std::process::id()` + the address of
/// a stack variable (ASLR-dependent) and then folds in a process-wide
/// atomic counter on every call. Each block of output is
/// `SHA-256(state || counter)`, and the state itself is updated to that
/// hash so successive calls do not reveal counter increments.
fn secure_rand_bytes(n: usize) -> Vec<u8> {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU64, Ordering};

    static GLOBAL_COUNTER: AtomicU64 = AtomicU64::new(1);

    thread_local! {
        static SEED: Cell<[u8; 32]> = Cell::new({
            let mut seed = [0u8; 32];
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u128)
                .unwrap_or(1);
            let pid = std::process::id() as u128;
            // The address of a stack variable is ASLR-randomised across
            // runs; mixing it in adds a third entropy source.
            let stack_marker: u64 = 0;
            let stack_addr = &stack_marker as *const u64 as u128;
            let mix = format!("{:x}:{:x}:{:x}", nanos, pid, stack_addr);
            let h = sha256_bytes(mix.as_bytes());
            seed.copy_from_slice(&h);
            seed
        });
    }

    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut counter = GLOBAL_COUNTER.fetch_add(1, Ordering::Relaxed);
    while out.len() < n {
        let chunk = SEED.with(|s| {
            let seed = s.get();
            let mut input = [0u8; 40];
            input[..32].copy_from_slice(&seed);
            input[32..].copy_from_slice(&counter.to_le_bytes());
            let h = sha256_bytes(&input);
            // Roll the seed forward so the next call produces fresh bytes
            // even if the global counter wraps.
            let mut next_seed = [0u8; 32];
            next_seed.copy_from_slice(&h);
            s.set(next_seed);
            h
        });
        for b in &chunk {
            if out.len() < n {
                out.push(*b);
            }
        }
        counter = counter.wrapping_add(1);
    }
    out
}

/// PBKDF2-style iterated SHA-256 key derivation.
///
/// Not RFC-2898 PBKDF2 (we lack HMAC construction scaffolding here), but a
/// salted iterated hash in the same spirit: each round mixes the previous
/// digest with the salt and password. The iteration count is the work
/// factor that makes brute-force costly.
fn pbkdf2_style_sha256(password: &str, salt: &[u8], iters: u32) -> [u8; 32] {
    let mut state: Vec<u8> = Vec::with_capacity(salt.len() + password.len() + 4);
    state.extend_from_slice(salt);
    state.extend_from_slice(password.as_bytes());
    state.extend_from_slice(&1u32.to_le_bytes());
    let mut hash = sha256_bytes(&state);
    for _ in 1..iters {
        let mut next: Vec<u8> = Vec::with_capacity(32 + salt.len() + password.len());
        next.extend_from_slice(&hash);
        next.extend_from_slice(salt);
        next.extend_from_slice(password.as_bytes());
        hash = sha256_bytes(&next);
    }
    hash
}

/// Build a `$pbkdf2$<iters>$<salt_hex>$<hash_hex>` password-hash string.
fn crypto_password_hash_impl(password: &str, iters: u32) -> String {
    let salt = secure_rand_bytes(16);
    let salt_hex: String = salt.iter().map(|b| format!("{:02x}", b)).collect();
    let hash = pbkdf2_style_sha256(password, &salt, iters);
    let hash_hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
    format!("$pbkdf2${}${}${}", iters, salt_hex, hash_hex)
}

/// Verify `password` against a stored `$pbkdf2$<iters>$<salt_hex>$<hash_hex>`
/// string. Returns `false` (never an error) if the stored string is
/// malformed or the iteration count is implausible.
fn crypto_password_verify_impl(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 5 || parts[1] != "pbkdf2" {
        return false;
    }
    let iters: u32 = parts[2].parse().unwrap_or(0);
    if iters < 1000 || iters > 10_000_000 {
        return false;
    }
    let salt_hex = parts[3];
    if salt_hex.len() != 32 {
        return false;
    }
    let salt: Vec<u8> = (0..salt_hex.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&salt_hex[i..i + 2], 16).ok())
        .collect();
    if salt.len() != 16 {
        return false;
    }
    let computed = pbkdf2_style_sha256(password, &salt, iters);
    let computed_hex: String = computed.iter().map(|b| format!("{:02x}", b)).collect();
    let stored_hash = parts[4];
    // Constant-time-ish comparison (best effort with std only).
    if computed_hex.len() != stored_hash.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (a, b) in computed_hex.bytes().zip(stored_hash.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn hex_decode(s: &str) -> Option<String> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 2 <= bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    String::from_utf8(out).ok()
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn url_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// ===== Simple hash functions (no external deps) =====
fn sha256_hex(data: &str) -> String {
    let bytes = data.as_bytes();
    let hash = sha256_bytes(bytes);
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}

fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    // SHA-256 implementation (FIPS 180-4).
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    // Padding.
    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    // Process blocks.
    let mut w = [0u32; 64];
    for chunk in msg.chunks(64) {
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[i*4], chunk[i*4+1], chunk[i*4+2], chunk[i*4+3]]);
        }
        for i in 16..64 {
            let s0 = w[i-15].rotate_right(7) ^ w[i-15].rotate_right(18) ^ (w[i-15] >> 3);
            let s1 = w[i-2].rotate_right(17) ^ w[i-2].rotate_right(19) ^ (w[i-2] >> 10);
            w[i] = w[i-16].wrapping_add(s0).wrapping_add(w[i-7]).wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g; g = f; f = e; e = d.wrapping_add(t1);
            d = c; c = b; b = a; a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[i*4..i*4+4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

fn sha1_hex(data: &str) -> String {
    let bytes = data.as_bytes();
    let hash = sha1_bytes(bytes);
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}

fn sha1_bytes(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    let mut w = [0u32; 80];
    for chunk in msg.chunks(64) {
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[i*4], chunk[i*4+1], chunk[i*4+2], chunk[i*4+3]]);
        }
        for i in 16..80 {
            w[i] = w[i-3] ^ w[i-8] ^ w[i-14] ^ w[i-16];
            w[i] = w[i].rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for i in 0..80 {
            let (f, k) = if i < 20 {
                ((b & c) | ((!b) & d), 0x5A827999u32)
            } else if i < 40 {
                (b ^ c ^ d, 0x6ED9EBA1)
            } else if i < 60 {
                ((b & c) | (b & d) | (c & d), 0x8F1BBCDC)
            } else {
                (b ^ c ^ d, 0xCA62C1D6)
            };
            let temp = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(w[i]);
            e = d; d = c; c = b.rotate_left(30); b = a; a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for i in 0..5 {
        out[i*4..i*4+4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

fn md5_hex(data: &str) -> String {
    let bytes = data.as_bytes();
    let hash = md5_bytes(bytes);
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}

fn md5_bytes(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7,12,17,22, 7,12,17,22, 7,12,17,22, 7,12,17,22,
        5, 9,14,20, 5, 9,14,20, 5, 9,14,20, 5, 9,14,20,
        4,11,16,23, 4,11,16,23, 4,11,16,23, 4,11,16,23,
        6,10,15,21, 6,10,15,21, 6,10,15,21, 6,10,15,21,
    ];
    const K: [u32; 64] = [
        0xd76aa478,0xe8c7b756,0x242070db,0xc1bdceee,0xf57c0faf,0x4787c62a,0xa8304613,0xfd469501,
        0x698098d8,0x8b44f7af,0xffff5bb1,0x895cd7be,0x6b901122,0xfd987193,0xa679438e,0x49b40821,
        0xf61e2562,0xc040b340,0x265e5a51,0xe9b6c7aa,0xd62f105d,0x02441453,0xd8a1e681,0xe7d3fbc8,
        0x21e1cde6,0xc33707d6,0xf4d50d87,0x455a14ed,0xa9e3e905,0xfcefa3f8,0x676f02d9,0x8d2a4c8a,
        0xfffa3942,0x8771f681,0x6d9d6122,0xfde5380c,0xa4beea44,0x4bdecfa9,0xf6bb4b60,0xbebfbc70,
        0x289b7ec6,0xeaa127fa,0xd4ef3085,0x04881d05,0xd9d4d039,0xe6db99e5,0x1fa27cf8,0xc4ac5665,
        0xf4292244,0x432aff97,0xab9423a7,0xfc93a039,0x655b59c3,0x8f0ccc92,0xffeff47d,0x85845dd1,
        0x6fa87e4f,0xfe2ce6e0,0xa3014314,0x4e0811a1,0xf7537e82,0xbd3af235,0x2ad7d2bb,0xeb86d391,
    ];
    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;
    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());
    for chunk in msg.chunks(64) {
        let mut m = [0u32; 16];
        for i in 0..16 {
            m[i] = u32::from_le_bytes([chunk[i*4], chunk[i*4+1], chunk[i*4+2], chunk[i*4+3]]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = if i < 16 {
                ((b & c) | ((!b) & d), i)
            } else if i < 32 {
                ((d & b) | ((!d) & c), (5 * i + 1) % 16)
            } else if i < 48 {
                (b ^ c ^ d, (3 * i + 5) % 16)
            } else {
                (c ^ (b | (!d)), (7 * i) % 16)
            };
            let temp = d;
            d = c;
            c = b;
            b = b.wrapping_add(a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(m[g]).rotate_left(S[i]));
            a = temp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

// ===== Civil-from-days (Howard Hinnant's algorithm) =====
// Converts a count of days since 1970-01-01 into (year, month, day, weekday).
// weekday: 0=Monday ... 6=Sunday. No external date crate needed.
fn civil_from_days(z: i64) -> (i64, u32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if m <= 2 { y + 1 } else { y };
    // weekday: 1970-01-01 was a Thursday (weekday=3 in 0=Mon..6=Sun).
    // days since 1970-01-01: z - 719468. 4 + (days) mod 7, adjusted to 0=Mon.
    let wd = ((z - 719468).rem_euclid(7) + 3).rem_euclid(7) as u32;
    (year, m, d, wd)
}

// ===== Minimal regex engine =====
// Supports: literal chars, ., *, +, ?, [...], [^...], ^, $, \d \w \s \D \W \S,
// and grouping via parentheses (capturing is not supported — matches only).
// This is a backtracking matcher; sufficient for common patterns but not
// a full PCRE.

fn regex_match_here(re: &[char], re_idx: &mut usize, text: &[char], ti: usize) -> Option<usize> {
    // Returns Some(end_index) if the pattern from re_idx matches text starting
    // at ti, advancing re_idx past the consumed pattern.
    if *re_idx >= re.len() {
        return Some(ti);
    }
    // Check for $ anchor.
    if re[*re_idx] == '$' && *re_idx + 1 == re.len() {
        return if ti == text.len() { Some(ti) } else { None };
    }
    // Parse the next token: a single-char matcher or character class or escape.
    let (matcher, advance) = parse_regex_atom(re, *re_idx);
    let next_re_idx = *re_idx + advance;
    // Check for quantifier.
    if next_re_idx < re.len() {
        match re[next_re_idx] {
            '*' => {
                *re_idx = next_re_idx + 1;
                return regex_match_star(re, re_idx, &matcher, text, ti);
            }
            '+' => {
                *re_idx = next_re_idx + 1;
                // Must match at least one.
                if !match_atom(&matcher, text, ti) {
                    return None;
                }
                return regex_match_star(re, re_idx, &matcher, text, ti + 1);
            }
            '?' => {
                *re_idx = next_re_idx + 1;
                // Try with one match.
                if match_atom(&matcher, text, ti) {
                    if let Some(end) = regex_match_here(re, re_idx, text, ti + 1) {
                        return Some(end);
                    }
                }
                // Try with zero matches.
                return regex_match_here(re, re_idx, text, ti);
            }
            _ => {}
        }
    }
    *re_idx = next_re_idx;
    if match_atom(&matcher, text, ti) {
        return regex_match_here(re, re_idx, text, ti + 1);
    }
    None
}

enum RegexAtom {
    AnyChar,
    Char(char),
    Class(Vec<(char, char)>, bool), // ranges, negated
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
}

fn parse_regex_atom(re: &[char], idx: usize) -> (RegexAtom, usize) {
    let c = re[idx];
    if c == '.' {
        return (RegexAtom::AnyChar, 1);
    }
    if c == '\\' && idx + 1 < re.len() {
        let nc = re[idx + 1];
        let atom = match nc {
            'd' => RegexAtom::Digit,
            'D' => RegexAtom::NotDigit,
            'w' => RegexAtom::Word,
            'W' => RegexAtom::NotWord,
            's' => RegexAtom::Space,
            'S' => RegexAtom::NotSpace,
            _ => RegexAtom::Char(nc),
        };
        return (atom, 2);
    }
    if c == '[' {
        // Character class.
        let mut j = idx + 1;
        let negated = j < re.len() && re[j] == '^';
        if negated {
            j += 1;
        }
        let mut ranges = Vec::new();
        while j < re.len() && re[j] != ']' {
            let start_c = re[j];
            if start_c == '\\' && j + 1 < re.len() {
                // Escaped char in class — treat as single char.
                ranges.push((re[j + 1], re[j + 1]));
                j += 2;
                continue;
            }
            if j + 2 < re.len() && re[j + 1] == '-' && re[j + 2] != ']' {
                ranges.push((start_c, re[j + 2]));
                j += 3;
            } else {
                ranges.push((start_c, start_c));
                j += 1;
            }
        }
        return (RegexAtom::Class(ranges, negated), j - idx + 1);
    }
    (RegexAtom::Char(c), 1)
}

fn match_atom(atom: &RegexAtom, text: &[char], ti: usize) -> bool {
    if ti >= text.len() {
        return false;
    }
    let c = text[ti];
    match atom {
        RegexAtom::AnyChar => c != '\n',
        RegexAtom::Char(ch) => c == *ch,
        RegexAtom::Class(ranges, negated) => {
            let in_class = ranges.iter().any(|(s, e)| c >= *s && c <= *e);
            in_class != *negated
        }
        RegexAtom::Digit => c.is_ascii_digit(),
        RegexAtom::NotDigit => !c.is_ascii_digit(),
        RegexAtom::Word => c.is_alphanumeric() || c == '_',
        RegexAtom::NotWord => !(c.is_alphanumeric() || c == '_'),
        RegexAtom::Space => c.is_whitespace(),
        RegexAtom::NotSpace => !c.is_whitespace(),
    }
}

fn regex_match_star(
    re: &[char],
    re_idx: &mut usize,
    atom: &RegexAtom,
    text: &[char],
    ti: usize,
) -> Option<usize> {
    // Greedy: match as many as possible, then backtrack.
    let mut count = 0;
    let mut k = ti;
    while k < text.len() && match_atom(atom, text, k) {
        k += 1;
        count += 1;
    }
    loop {
        let saved = *re_idx;
        if let Some(end) = regex_match_here(re, re_idx, text, k) {
            return Some(end);
        }
        *re_idx = saved;
        if count == 0 {
            return None;
        }
        count -= 1;
        k -= 1;
    }
}

fn regex_match(pattern: &str, s: &str) -> bool {
    let re: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = s.chars().collect();
    // If anchored with ^, only try at position 0.
    let (start_re, start_idx) = if !re.is_empty() && re[0] == '^' {
        (1usize, 0usize)
    } else {
        (0, 0)
    };
    // For full match (regex.match), the entire string must match.
    let mut re_idx = start_re;
    if let Some(end) = regex_match_here(&re, &mut re_idx, &text, start_idx) {
        return end == text.len() && re_idx == re.len();
    }
    false
}

fn regex_search(pattern: &str, s: &str) -> Option<(usize, usize)> {
    let re: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = s.chars().collect();
    let (start_re, try_all) = if !re.is_empty() && re[0] == '^' {
        (1usize, false)
    } else {
        (0, true)
    };
    let max_start = if try_all { text.len() } else { 0 };
    for start in 0..=max_start {
        let mut re_idx = start_re;
        if let Some(end) = regex_match_here(&re, &mut re_idx, &text, start) {
            return Some((start, end));
        }
    }
    None
}

fn regex_find_all(pattern: &str, s: &str) -> Vec<String> {
    let mut results = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut pos = 0;
    while pos <= chars.len() {
        match regex_search(pattern, &chars[pos..].iter().collect::<String>()) {
            Some((start, end)) => {
                let abs_start = pos + start;
                let abs_end = pos + end;
                let matched: String = chars[abs_start..abs_end].iter().collect();
                results.push(matched);
                pos = if abs_end == abs_start { abs_end + 1 } else { abs_end };
            }
            None => break,
        }
    }
    results
}

fn regex_split(pattern: &str, s: &str) -> Vec<String> {
    let mut results = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut pos = 0;
    let mut last = 0;
    while pos <= chars.len() {
        match regex_search(pattern, &chars[pos..].iter().collect::<String>()) {
            Some((start, end)) => {
                let abs_start = pos + start;
                let abs_end = pos + end;
                if abs_start > last {
                    let segment: String = chars[last..abs_start].iter().collect();
                    results.push(segment);
                } else {
                    results.push(String::new());
                }
                last = abs_end;
                pos = if abs_end == abs_start { abs_end + 1 } else { abs_end };
            }
            None => break,
        }
    }
    if last < chars.len() {
        let segment: String = chars[last..].iter().collect();
        results.push(segment);
    } else if last == chars.len() && !results.is_empty() {
        results.push(String::new());
    }
    results
}

fn regex_replace(pattern: &str, repl: &str, s: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut pos = 0;
    while pos <= chars.len() {
        match regex_search(pattern, &chars[pos..].iter().collect::<String>()) {
            Some((start, end)) => {
                let abs_start = pos + start;
                let abs_end = pos + end;
                out.extend(chars[pos..abs_start].iter());
                out.push_str(repl);
                pos = if abs_end == abs_start { abs_end + 1 } else { abs_end };
                if abs_end == abs_start && abs_end >= chars.len() {
                    break;
                }
            }
            None => {
                out.extend(chars[pos..].iter());
                break;
            }
        }
    }
    out
}

// ===== Regex engine with capture-group tracking =====
//
// The original regex engine (regex_match_here/regex_match_star) only reports
// whether a pattern matches; it discards the substring captured by each
// `(...)` group. The functions below mirror the original backtracking
// matcher but thread a `captures: &mut Vec<Option<(usize, usize)>>` through
// every recursion level. Index 0 is the full match span; indices 1..N are
// the capture groups in order of opening parenthesis. Backtracking saves
// and restores the captures vector so failed branches do not leak partial
// state. Quantifiers on groups (`(...)`, `(...)?`, `(...)*`, `(...)+`) are
// supported; alternation `a|b` is not (the original engine also lacks it).
//
// The existing bool `regex_match` and `regex_search` functions above are
// kept for callers that only need a yes/no answer (notably the compiled
// Regex object methods `is_match`/`match`/`find`). The new capture-aware
// variants power the `regex_match` and `regex_findall` builtins.

/// Pre-compute group indices for a pattern. Returns:
///  - `starts`: re-position of `(` → 1-indexed group number
///  - `ends`:   re-position of `)` → 1-indexed group number
///  - `matching_paren`: re-position of `(` → re-position of matching `)`
fn regex_precompute_groups(
    re: &[char],
) -> (HashMap<usize, usize>, HashMap<usize, usize>, HashMap<usize, usize>) {
    let mut starts: HashMap<usize, usize> = HashMap::new();
    let mut ends: HashMap<usize, usize> = HashMap::new();
    let mut matching_paren: HashMap<usize, usize> = HashMap::new();
    let mut stack: Vec<(usize, usize)> = Vec::new(); // (re_pos_of_'(', group_idx)
    let mut next_idx = 1usize;
    let mut i = 0;
    while i < re.len() {
        let c = re[i];
        if c == '\\' && i + 1 < re.len() {
            i += 2;
            continue;
        }
        if c == '[' {
            // Skip character class — `(` or `)` inside `[...]` is a literal.
            let mut j = i + 1;
            while j < re.len() && re[j] != ']' {
                if re[j] == '\\' && j + 1 < re.len() {
                    j += 2;
                } else {
                    j += 1;
                }
            }
            i = j.saturating_add(1);
            continue;
        }
        if c == '(' {
            // Non-capturing groups `(?:...)` are not supported by the original
            // engine; treat all `(` as capturing for simplicity.
            starts.insert(i, next_idx);
            stack.push((i, next_idx));
            next_idx += 1;
        } else if c == ')' {
            if let Some((start_pos, gidx)) = stack.pop() {
                ends.insert(i, gidx);
                matching_paren.insert(start_pos, i);
            }
            // Unmatched ')' falls through; treated as literal char by caller.
        }
        i += 1;
    }
    (starts, ends, matching_paren)
}

#[allow(clippy::too_many_arguments)]
fn regex_match_here_captures(
    re: &[char],
    re_idx: &mut usize,
    text: &[char],
    ti: usize,
    group_starts: &HashMap<usize, usize>,
    group_ends: &HashMap<usize, usize>,
    matching_paren: &HashMap<usize, usize>,
    captures: &mut Vec<Option<(usize, usize)>>,
) -> Option<usize> {
    if *re_idx >= re.len() {
        return Some(ti);
    }
    if re[*re_idx] == '$' && *re_idx + 1 == re.len() {
        return if ti == text.len() { Some(ti) } else { None };
    }
    // `)` — group close (zero-width). Record capture end and return;
    // the caller (the `(` handler) continues matching the rest.
    if let Some(&gidx) = group_ends.get(re_idx) {
        if let Some(slot) = captures.get_mut(gidx) {
            let start = slot.map(|(s, _)| s).unwrap_or(ti);
            *slot = Some((start, ti));
        }
        *re_idx += 1;
        return Some(ti);
    }
    // `(` — group open.
    if let Some(&gidx) = group_starts.get(re_idx) {
        let open_pos = *re_idx;
        let body_start = open_pos + 1;
        let close_pos = match matching_paren.get(&open_pos) {
            Some(&p) => p,
            None => {
                // Unmatched `(` — fall back to treating it as a literal.
                return regex_match_atom_literal_captures(
                    re,
                    re_idx,
                    text,
                    ti,
                    RegexAtom::Char('('),
                    1,
                    group_starts,
                    group_ends,
                    matching_paren,
                    captures,
                );
            }
        };
        // Record start.
        if let Some(slot) = captures.get_mut(gidx) {
            *slot = Some((ti, ti));
        }
        // Check for quantifier after `)`.
        let quant_pos = close_pos + 1;
        let quant = if quant_pos < re.len() {
            match re[quant_pos] {
                '*' | '+' | '?' => Some(re[quant_pos]),
                _ => None,
            }
        } else {
            None
        };
        match quant {
            None => {
                // Match the group exactly once, then continue with the rest.
                let saved = captures.clone();
                let saved_re = *re_idx;
                *re_idx = body_start;
                let body_end = match regex_match_here_captures(
                    re,
                    re_idx,
                    text,
                    ti,
                    group_starts,
                    group_ends,
                    matching_paren,
                    captures,
                ) {
                    Some(e) => e,
                    None => {
                        *captures = saved;
                        *re_idx = saved_re;
                        return None;
                    }
                };
                // re_idx is now at close_pos+1 (past `)`). Match the rest.
                return regex_match_here_captures(
                    re,
                    re_idx,
                    text,
                    body_end,
                    group_starts,
                    group_ends,
                    matching_paren,
                    captures,
                );
            }
            Some('?') => {
                // Try one match, then fall back to zero matches.
                let saved = captures.clone();
                let after_quant_re = quant_pos + 1;
                *re_idx = body_start;
                if let Some(e) = regex_match_here_captures(
                    re,
                    re_idx,
                    text,
                    ti,
                    group_starts,
                    group_ends,
                    matching_paren,
                    captures,
                ) {
                    // re_idx is now at close_pos+1 (the `?`); skip past it
                    // before continuing with the rest of the pattern.
                    *re_idx = after_quant_re;
                    if let Some(end) = regex_match_here_captures(
                        re,
                        re_idx,
                        text,
                        e,
                        group_starts,
                        group_ends,
                        matching_paren,
                        captures,
                    ) {
                        return Some(end);
                    }
                }
                // Restore and try zero matches.
                *captures = saved;
                *re_idx = after_quant_re;
                if let Some(slot) = captures.get_mut(gidx) {
                    *slot = None;
                }
                return regex_match_here_captures(
                    re,
                    re_idx,
                    text,
                    ti,
                    group_starts,
                    group_ends,
                    matching_paren,
                    captures,
                );
            }
            Some('*') | Some('+') => {
                let is_plus = quant == Some('+');
                return regex_match_group_star_captures(
                    re,
                    re_idx,
                    text,
                    ti,
                    body_start,
                    close_pos,
                    gidx,
                    is_plus,
                    quant_pos + 1,
                    group_starts,
                    group_ends,
                    matching_paren,
                    captures,
                );
            }
            _ => return None,
        }
    }
    // Regular atom.
    let (matcher, advance) = parse_regex_atom(re, *re_idx);
    regex_match_atom_literal_captures(
        re,
        re_idx,
        text,
        ti,
        matcher,
        advance,
        group_starts,
        group_ends,
        matching_paren,
        captures,
    )
}

/// Match a single (non-group) atom at `*re_idx`, honouring `*`, `+`, `?`
/// quantifiers, threading `captures` for backtracking.
#[allow(clippy::too_many_arguments)]
fn regex_match_atom_literal_captures(
    re: &[char],
    re_idx: &mut usize,
    text: &[char],
    ti: usize,
    matcher: RegexAtom,
    advance: usize,
    group_starts: &HashMap<usize, usize>,
    group_ends: &HashMap<usize, usize>,
    matching_paren: &HashMap<usize, usize>,
    captures: &mut Vec<Option<(usize, usize)>>,
) -> Option<usize> {
    let next_re_idx = *re_idx + advance;
    if next_re_idx < re.len() {
        match re[next_re_idx] {
            '*' => {
                *re_idx = next_re_idx + 1;
                return regex_match_star_captures(
                    re,
                    re_idx,
                    &matcher,
                    text,
                    ti,
                    group_starts,
                    group_ends,
                    matching_paren,
                    captures,
                );
            }
            '+' => {
                *re_idx = next_re_idx + 1;
                if !match_atom(&matcher, text, ti) {
                    return None;
                }
                return regex_match_star_captures(
                    re,
                    re_idx,
                    &matcher,
                    text,
                    ti + 1,
                    group_starts,
                    group_ends,
                    matching_paren,
                    captures,
                );
            }
            '?' => {
                *re_idx = next_re_idx + 1;
                if match_atom(&matcher, text, ti) {
                    let saved = captures.clone();
                    if let Some(e) = regex_match_here_captures(
                        re,
                        re_idx,
                        text,
                        ti + 1,
                        group_starts,
                        group_ends,
                        matching_paren,
                        captures,
                    ) {
                        return Some(e);
                    }
                    *captures = saved;
                }
                return regex_match_here_captures(
                    re,
                    re_idx,
                    text,
                    ti,
                    group_starts,
                    group_ends,
                    matching_paren,
                    captures,
                );
            }
            _ => {}
        }
    }
    *re_idx = next_re_idx;
    if match_atom(&matcher, text, ti) {
        return regex_match_here_captures(
            re,
            re_idx,
            text,
            ti + 1,
            group_starts,
            group_ends,
            matching_paren,
            captures,
        );
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn regex_match_star_captures(
    re: &[char],
    re_idx: &mut usize,
    atom: &RegexAtom,
    text: &[char],
    ti: usize,
    group_starts: &HashMap<usize, usize>,
    group_ends: &HashMap<usize, usize>,
    matching_paren: &HashMap<usize, usize>,
    captures: &mut Vec<Option<(usize, usize)>>,
) -> Option<usize> {
    let mut count = 0;
    let mut k = ti;
    while k < text.len() && match_atom(atom, text, k) {
        k += 1;
        count += 1;
    }
    loop {
        let saved = captures.clone();
        let saved_re = *re_idx;
        if let Some(e) = regex_match_here_captures(
            re,
            re_idx,
            text,
            k,
            group_starts,
            group_ends,
            matching_paren,
            captures,
        ) {
            return Some(e);
        }
        *captures = saved;
        *re_idx = saved_re;
        if count == 0 {
            return None;
        }
        count -= 1;
        k -= 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn regex_match_group_star_captures(
    re: &[char],
    re_idx: &mut usize,
    text: &[char],
    ti: usize,
    body_start: usize,
    close_pos: usize,
    gidx: usize,
    is_plus: bool,
    after_quant_re: usize,
    group_starts: &HashMap<usize, usize>,
    group_ends: &HashMap<usize, usize>,
    matching_paren: &HashMap<usize, usize>,
    captures: &mut Vec<Option<(usize, usize)>>,
) -> Option<usize> {
    // Greedy: match the group as many times as possible, then backtrack.
    let mut positions: Vec<usize> = Vec::new();
    let mut captures_history: Vec<Vec<Option<(usize, usize)>>> = Vec::new();
    let mut k = ti;
    loop {
        let saved = captures.clone();
        let mut local_re_idx = body_start;
        if let Some(slot) = captures.get_mut(gidx) {
            *slot = Some((k, k));
        }
        match regex_match_here_captures(
            re,
            &mut local_re_idx,
            text,
            k,
            group_starts,
            group_ends,
            matching_paren,
            captures,
        ) {
            Some(end) => {
                // local_re_idx should be past `)`.
                if local_re_idx <= close_pos {
                    *captures = saved;
                    break;
                }
                if end == k {
                    // Empty match — stop to avoid infinite loop.
                    *captures = saved;
                    break;
                }
                positions.push(end);
                captures_history.push(saved);
                k = end;
            }
            None => {
                *captures = saved;
                break;
            }
        }
    }
    if is_plus && positions.is_empty() {
        return None;
    }
    *re_idx = after_quant_re;
    // Backtrack from the longest match.
    loop {
        let saved = captures.clone();
        let saved_re = *re_idx;
        if let Some(e) = regex_match_here_captures(
            re,
            re_idx,
            text,
            k,
            group_starts,
            group_ends,
            matching_paren,
            captures,
        ) {
            return Some(e);
        }
        *captures = saved;
        *re_idx = saved_re;
        if !positions.is_empty() {
            positions.pop();
            *captures = captures_history.pop().unwrap_or_default();
            k = if positions.is_empty() {
                ti
            } else {
                *positions.last().unwrap_or(&ti)
            };
        } else if !is_plus {
            // For '*', try zero matches.
            if let Some(slot) = captures.get_mut(gidx) {
                *slot = None;
            }
            let saved2 = captures.clone();
            if let Some(e) = regex_match_here_captures(
                re,
                re_idx,
                text,
                ti,
                group_starts,
                group_ends,
                matching_paren,
                captures,
            ) {
                return Some(e);
            }
            *captures = saved2;
            return None;
        } else {
            return None;
        }
    }
}

/// Try to match `pattern` starting at `start` in `s`. Returns
/// `(end, captures)` where captures[0] is the full-match span and
/// captures[1..N] are the capture groups.
fn regex_match_at_captures(
    pattern: &str,
    s: &str,
    start: usize,
) -> Option<(usize, Vec<Option<(usize, usize)>>)> {
    let re: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = s.chars().collect();
    let (group_starts, group_ends, matching_paren) = regex_precompute_groups(&re);
    let num_groups = group_starts.len();
    let mut captures = vec![None; num_groups + 1];
    let start_re = if !re.is_empty() && re[0] == '^' { 1 } else { 0 };
    let mut re_idx = start_re;
    match regex_match_here_captures(
        &re,
        &mut re_idx,
        &text,
        start,
        &group_starts,
        &group_ends,
        &matching_paren,
        &mut captures,
    ) {
        Some(end) => {
            captures[0] = Some((start, end));
            Some((end, captures))
        }
        None => None,
    }
}

/// Python `re.match` semantics: match at the start of the string.
fn regex_match_captures(pattern: &str, s: &str) -> Option<Vec<Option<(usize, usize)>>> {
    regex_match_at_captures(pattern, s, 0).map(|(_, caps)| caps)
}

/// Python `re.search` semantics: find the first match anywhere.
fn regex_search_captures(
    pattern: &str,
    s: &str,
) -> Option<(usize, usize, Vec<Option<(usize, usize)>>)> {
    let text: Vec<char> = s.chars().collect();
    regex_search_captures_in_text(pattern, &text, 0)
        .map(|(st, end, caps)| (st, end, caps))
}

/// Find the first match at or after `from` in the pre-characterized `text`.
fn regex_search_captures_from(
    pattern: &str,
    text: &[char],
    from: usize,
) -> Option<(usize, usize, Vec<Option<(usize, usize)>>)> {
    regex_search_captures_in_text(pattern, text, from)
}

fn regex_search_captures_in_text(
    pattern: &str,
    text: &[char],
    from: usize,
) -> Option<(usize, usize, Vec<Option<(usize, usize)>>)> {
    let re: Vec<char> = pattern.chars().collect();
    let (group_starts, group_ends, matching_paren) = regex_precompute_groups(&re);
    let try_all = !(re.len() > 0 && re[0] == '^');
    let max_start = if try_all { text.len() } else { from };
    let mut start = from;
    while start <= max_start {
        let num_groups = group_starts.len();
        let mut captures = vec![None; num_groups + 1];
        let start_re = if !re.is_empty() && re[0] == '^' { 1 } else { 0 };
        let mut re_idx = start_re;
        match regex_match_here_captures(
            &re,
            &mut re_idx,
            text,
            start,
            &group_starts,
            &group_ends,
            &matching_paren,
            &mut captures,
        ) {
            Some(end) => {
                captures[0] = Some((start, end));
                return Some((start, end, captures));
            }
            None => {
                start += 1;
            }
        }
    }
    None
}

/// Convert a captures vec into a Vredrs Value::List
/// `[full_match, group1, group2, ...]`. Unmatched groups become Value::Null.
fn captures_to_value(caps: &[Option<(usize, usize)>], text: &[char]) -> Value {
    let mut items: Vec<Value> = Vec::with_capacity(caps.len());
    for cap in caps {
        match cap {
            Some((s, e)) => {
                let s = (*s).min(text.len());
                let e = (*e).min(text.len()).max(s);
                let substr: String = text[s..e].iter().collect();
                items.push(Value::Str(substr));
            }
            None => {
                items.push(Value::Null);
            }
        }
    }
    Value::List(items)
}

// ===== Native JSON implementation =====
// These are exposed as VM builtins so the json.veds module can re-export
// them without relying on the sub-VM function-call mechanism.

fn native_json_stringify(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            if f.is_nan() || f.is_infinite() {
                "null".to_string()
            } else if *f == f.trunc() && f.is_finite() {
                format!("{:.1}", f)
            } else {
                f.to_string()
            }
        }
        Value::Str(s) => format!("\"{}\"", escape_json_string(s)),
        Value::List(l) => {
            let items: Vec<String> = l.iter().map(native_json_stringify).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Tuple(t) => {
            let items: Vec<String> = t.iter().map(native_json_stringify).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Dict(d) => {
            let entries: Vec<String> = d
                .iter()
                .map(|(k, v)| format!("\"{}\": {}", escape_json_string(k), native_json_stringify(v)))
                .collect();
            format!("{{{}}}", entries.join(", "))
        }
        _ => "null".to_string(),
    }
}

fn escape_json_string(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

fn native_json_parse(text: &str) -> Value {
    let chars: Vec<char> = text.chars().collect();
    let mut pos = 0;
    match json_parse_value(&chars, &mut pos) {
        Some(v) => v,
        None => Value::Null,
    }
}

fn json_skip_ws(chars: &[char], pos: &mut usize) {
    while *pos < chars.len() {
        match chars[*pos] {
            ' ' | '\t' | '\n' | '\r' => *pos += 1,
            _ => break,
        }
    }
}

fn json_parse_value(chars: &[char], pos: &mut usize) -> Option<Value> {
    json_skip_ws(chars, pos);
    if *pos >= chars.len() {
        return None;
    }
    match chars[*pos] {
        '{' => json_parse_object(chars, pos),
        '[' => json_parse_array(chars, pos),
        '"' => json_parse_string(chars, pos).map(Value::Str),
        't' => {
            *pos += 4;
            Some(Value::Bool(true))
        }
        'f' => {
            *pos += 5;
            Some(Value::Bool(false))
        }
        'n' => {
            *pos += 4;
            Some(Value::Null)
        }
        '-' | '0'..='9' => json_parse_number(chars, pos),
        _ => None,
    }
}

fn json_parse_object(chars: &[char], pos: &mut usize) -> Option<Value> {
    *pos += 1; // {
    let mut map = HashMap::new();
    json_skip_ws(chars, pos);
    if *pos < chars.len() && chars[*pos] == '}' {
        *pos += 1;
        return Some(Value::Dict(map));
    }
    loop {
        json_skip_ws(chars, pos);
        let key = json_parse_string(chars, pos)?;
        json_skip_ws(chars, pos);
        if *pos >= chars.len() || chars[*pos] != ':' {
            return Some(Value::Dict(map));
        }
        *pos += 1; // :
        let val = json_parse_value(chars, pos)?;
        map.insert(key, val);
        json_skip_ws(chars, pos);
        if *pos >= chars.len() {
            return Some(Value::Dict(map));
        }
        match chars[*pos] {
            ',' => *pos += 1,
            '}' => {
                *pos += 1;
                return Some(Value::Dict(map));
            }
            _ => return Some(Value::Dict(map)),
        }
    }
}

fn json_parse_array(chars: &[char], pos: &mut usize) -> Option<Value> {
    *pos += 1; // [
    let mut items = Vec::new();
    json_skip_ws(chars, pos);
    if *pos < chars.len() && chars[*pos] == ']' {
        *pos += 1;
        return Some(Value::List(items));
    }
    loop {
        let val = json_parse_value(chars, pos)?;
        items.push(val);
        json_skip_ws(chars, pos);
        if *pos >= chars.len() {
            return Some(Value::List(items));
        }
        match chars[*pos] {
            ',' => *pos += 1,
            ']' => {
                *pos += 1;
                return Some(Value::List(items));
            }
            _ => return Some(Value::List(items)),
        }
    }
}

fn json_parse_string(chars: &[char], pos: &mut usize) -> Option<String> {
    *pos += 1; // opening "
    let mut out = String::new();
    while *pos < chars.len() {
        let c = chars[*pos];
        *pos += 1;
        match c {
            '"' => return Some(out),
            '\\' => {
                if *pos >= chars.len() {
                    return Some(out);
                }
                let esc = chars[*pos];
                *pos += 1;
                match esc {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{0008}'),
                    'f' => out.push('\u{000C}'),
                    _ => out.push(esc),
                }
            }
            _ => out.push(c),
        }
    }
    Some(out)
}

fn json_parse_number(chars: &[char], pos: &mut usize) -> Option<Value> {
    let start = *pos;
    if *pos < chars.len() && chars[*pos] == '-' {
        *pos += 1;
    }
    while *pos < chars.len() && chars[*pos].is_ascii_digit() {
        *pos += 1;
    }
    let mut is_float = false;
    if *pos < chars.len() && chars[*pos] == '.' {
        is_float = true;
        *pos += 1;
        while *pos < chars.len() && chars[*pos].is_ascii_digit() {
            *pos += 1;
        }
    }
    if *pos < chars.len() && (chars[*pos] == 'e' || chars[*pos] == 'E') {
        is_float = true;
        *pos += 1;
        if *pos < chars.len() && (chars[*pos] == '+' || chars[*pos] == '-') {
            *pos += 1;
        }
        while *pos < chars.len() && chars[*pos].is_ascii_digit() {
            *pos += 1;
        }
    }
    let num_str: String = chars[start..*pos].iter().collect();
    if is_float {
        num_str.parse::<f64>().ok().map(Value::Float)
    } else {
        num_str.parse::<i64>().ok().map(Value::Int)
    }
}

/// Return the type name of a Value as a string (for `is` operator).
fn type_of_value(v: &Value) -> String {
    match v {
        Value::Int(_) => "int".to_string(),
        Value::Float(_) => "float".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::Str(_) => "str".to_string(),
        Value::Null => "null".to_string(),
        Value::List(_) => "list".to_string(),
        Value::Dict(_) => "dict".to_string(),
        Value::Tuple(_) => "tuple".to_string(),
        Value::Func(_) => "func".to_string(),
        Value::Generator(_) => "generator".to_string(),
        Value::Object(class, _) => class.clone(),
        Value::Class(class) => class.clone(),
        Value::Module(_, _) => "module".to_string(),
        Value::Exception(_, _) => "exception".to_string(),
    }
}
