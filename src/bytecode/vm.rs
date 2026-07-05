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
    Exception(String),
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
            Value::Exception(_) => true,
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
            Value::Exception(msg) => msg.clone(),
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
            (Value::Exception(a), Value::Exception(b)) => a == b,
            (Value::Exception(a), Value::Str(b)) | (Value::Str(b), Value::Exception(a)) => a == b,
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
            Value::Exception(_) => 13,
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
            (Value::Exception(a), Value::Exception(b)) => a.cmp(b),
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
    /// Set of frozen values (by their string repr, for is_frozen check).
    frozen_set: std::collections::HashSet<String>,
    /// Lambda function definitions collected at runtime.
    lambda_defs: Vec<AstFnDef>,
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
    /// local scopes + globals) is stored here. When the lambda is later
    /// called, those captures are restored as the innermost scope so
    /// the lambda body sees the values that were in scope at creation
    /// time — even if the enclosing function has since returned.
    lambda_captures: HashMap<String, HashMap<String, Value>>,
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
    /// Macro definitions: name → (params, body). When a call resolves to a
    /// macro name, the VM executes the macro body with the call arguments
    /// bound to the macro's parameter names in a fresh local scope.
    macros: HashMap<String, (Vec<String>, Vec<crate::parser::ast::Stmt>)>,
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
            }],
            globals: HashMap::new(),
            local_scopes: Vec::new(),
            pc: 0,
            iterators: Vec::new(),
            program: None,
            compiled_fns: HashMap::new(),
            base_dir: std::path::PathBuf::from("."),
            module_cache: HashMap::new(),
            frozen_set: std::collections::HashSet::new(),
            lambda_defs: Vec::new(),
            method_context: Vec::new(),
            defer_stack: Vec::new(),
            annotation_table: HashMap::new(),
            gen_yield_buffer: None,
            lambda_captures: HashMap::new(),
            fn_cache: HashMap::new(),
            gen_set: std::collections::HashSet::new(),
            scope_pool: Vec::new(),
            handler_stack: Vec::new(),
            extern_fns: HashMap::new(),
            macros: HashMap::new(),
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
    pub fn globals_clone(&self) -> HashMap<String, Value> {
        self.globals.clone()
    }

    /// Set a global variable (used by the REPL to persist variables
    /// across inputs).
    pub fn set_global(&mut self, name: &str, value: Value) {
        self.globals.insert(name.to_string(), value);
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
    /// Semantics (mirrors the previous "everything is global" behaviour
    /// while adding true local isolation for new variables):
    /// - Inside a function (scope stack non-empty):
    ///     * if the name is already a local in the current frame → update it;
    ///     * else if the name is a global → update the global (preserves
    ///       intentional writes to globals from inside functions);
    ///     * otherwise create a new local in the current frame.
    /// - At top level (scope stack empty): write to `globals`.
    fn scope_set(&mut self, name: String, val: Value) {
        if let Some(scope) = self.local_scopes.last_mut() {
            if scope.contains_key(&name) {
                scope.insert(name, val);
                return;
            }
            if self.globals.contains_key(&name) {
                self.globals.insert(name, val);
                return;
            }
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
                    if self.frames.len() > 1 {
                        let frame = self.frames.pop().unwrap();
                        self.pc = frame.return_pc;
                        // Truncate the stack to the caller's base, discarding
                        // any values leaked above the return value (e.g. a
                        // match scrutinee left on the stack by an early return
                        // from inside a match case body).
                        self.stack.truncate(frame.stack_base);
                        self.stack.push(v);
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
                    if msg.starts_with("throw:") {
                        if let Some((catch_pc, handler_frame_depth)) = self.handler_stack.pop() {
                            while self.frames.len() > handler_frame_depth {
                                self.frames.pop();
                            }
                            let exc_val = Value::Str(msg["throw:".len()..].to_string());
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
                    // Fast path for int+int (most common case).
                    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
                        self.stack.push(Value::Int(x + y));
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
                                    _ => Value::Null,
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
                        self.stack.push(Value::Int(x - y));
                    } else {
                        if let Some(v) = self.try_binary_overload("__sub__", &a, &b)? {
                            self.stack.push(v);
                        } else {
                            self.stack.push(match (a, b) {
                                (Value::Float(x), Value::Float(y)) => Value::Float(x - y),
                                (Value::Int(x), Value::Float(y)) => Value::Float(x as f64 - y),
                                (Value::Float(x), Value::Int(y)) => Value::Float(x - y as f64),
                                _ => Value::Null,
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
                        self.stack.push(Value::Int(x * y));
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
                                _ => Value::Null,
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
                    // Borrow the constant name as a &str instead of cloning
                    // the String on every global read (the 100k-loop hot
                    // path reads globals several times per iteration). The
                    // lookup is confined to a block so the &str borrow of
                    // self.module ends before the mutable stack push.
                    let v = {
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
                    // Borrow the constant name; update the existing slot in
                    // place (no key allocation) when the global is already
                    // bound — the common case inside a loop.
                    let name = self
                        .module
                        .constants
                        .get(name_idx)
                        .map(|s| s.as_str())
                        .unwrap_or("");
                    if let Some(slot) = self.globals.get_mut(name) {
                        *slot = v;
                    } else {
                        self.globals.insert(name.to_string(), v);
                    }
                    Ok(Flow::Continue)
                }
                Instr::Return => {
                    self.pc += 1;
                    let v = self.stack.pop().unwrap_or(Value::Null);
                    if self.frames.len() <= 1 {
                        return Ok(v);
                    }
                    let frame = self.frames.pop().unwrap();
                    self.pc = frame.return_pc;
                    self.stack.truncate(frame.stack_base);
                    self.stack.push(v);
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
                    if msg.starts_with("throw:") {
                        if let Some((catch_pc, handler_frame_depth)) = self.handler_stack.pop() {
                            // Unwind frames: pop frames until we're at the
                            // same depth as when the handler was pushed.
                            while self.frames.len() > handler_frame_depth {
                                self.frames.pop();
                            }
                            let exc_val = Value::Str(msg["throw:".len()..].to_string());
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
                                    // Evaluate the annotation argument expression
                                    // (must be a literal — best-effort).
                                    if let Err(_) = self.execute_ast_expr(&first_arg.value) {
                                        Value::Bool(true)
                                    } else {
                                        match self.pop() {
                                            Ok(v) => v,
                                            Err(_) => Value::Bool(true),
                                        }
                                    }
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
                match &obj {
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
                        // Dicts are value types — can't mutate in place.
                        // Push the value back (best effort).
                    }
                    _ => {}
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
                    let result = match (a, b) {
                        (Value::Int(x), Value::Int(y)) => {
                            if y == 0 {
                                return Err(CompilerError::runtime_error("division by zero"));
                            }
                            Value::Int(x / y)
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
            Instr::Mod => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Some(v) = self.try_binary_overload("__mod__", &a, &b)? {
                    self.push(v);
                } else {
                    self.push(match (a, b) {
                        (Value::Int(x), Value::Int(y)) => {
                            if y == 0 { Value::Null } else { Value::Int(x % y) }
                        }
                        (Value::Float(x), Value::Float(y)) => {
                            if y == 0.0 { Value::Null } else { Value::Float(x % y) }
                        }
                        (Value::Int(x), Value::Float(y)) => {
                            if y == 0.0 { Value::Null } else { Value::Float((x as f64) % y) }
                        }
                        (Value::Float(x), Value::Int(y)) => {
                            if y == 0 { Value::Null } else { Value::Float(x % (y as f64)) }
                        }
                        _ => Value::Null,
                    });
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
                let entry = self
                    .module
                    .fn_entry_pcs
                    .get(
                        self.module
                            .constants
                            .get(*name_idx)
                            .map(|s| s.as_str())
                            .unwrap_or(""),
                    )
                    .copied();
                if let Some((entry_pc, num_locals)) = entry {
                    // Pop args into the new frame's locals Vec (params in
                    // slots 0..argc). self.pc is already the instruction
                    // after this CallByName (the run() slow path increments
                    // pc before calling execute), so it serves as the return
                    // address.
                    let nslots = num_locals.max(*argc);
                    let mut locals = vec![Value::Null; nslots];
                    for i in (0..*argc).rev() {
                        locals[i] = self.pop()?;
                    }
                    let stack_base = self.stack.len();
                    self.frames.push(Frame {
                        return_pc: self.pc,
                        locals,
                        is_main: false,
                        stack_base,
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
                let callee = self.pop()?;
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
            }
            Instr::ReturnVoid => {
                if self.frames.len() <= 1 {
                    return Ok(Flow::Return(Value::Null));
                }
                let frame = self.frames.pop().unwrap();
                self.pc = frame.return_pc;
                self.push(Value::Null);
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
                                    if let Value::Exception(msg) = &v {
                                        if msg == "StopIteration" {
                                            None
                                        } else {
                                            Some(v)
                                        }
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
            Instr::Throw => {
                let v = self.pop()?;
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
            Instr::Print => {
                let v = self.pop()?;
                print!("{}", v.to_str());
            }
            Instr::Println => {
                let v = self.pop()?;
                println!("{}", v.to_str());
            }
            Instr::Newline => {
                println!();
            }
            Instr::EvalAst(expr) => {
                // Fallback: evaluate an AST expression using the VM's
                // AST interpreter path. The result is pushed onto the stack.
                self.execute_ast_expr(expr)?;
            }
            Instr::ExecAstStmt(stmt) => {
                // Fallback: execute an AST statement using the VM's
                // AST interpreter path.
                self.execute_ast_stmt(stmt)?;
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
            // Re-load the module source and check if it defines this function.
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
            let program = match parser.parse_program() {
                Ok(p) => p,
                Err(_) => continue,
            };
            // Check if this program defines the function.
            let has_fn = program.declarations.iter().any(|d| {
                matches!(d, crate::parser::ast::TopLevel::FnDef(f) if f.name.name == name)
                    || matches!(d, crate::parser::ast::TopLevel::LazyFnDef(l) if l.fn_def.name.name == name)
            });
            if !has_fn {
                continue;
            }
            // Found the module — compile and execute the function.
            let sub_compiler = super::Compiler::new();
            let sub_module = sub_compiler.compile(&program)?;
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
            // Bind the parameters in a dedicated call-frame scope on the
            // sub-VM so the imported function's locals are isolated from
            // its own recursive calls (the sub-VM's call_function pushes
            // further scopes for each recursive invocation).
            let params: Vec<(String, Value)> = fn_def
                .params
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    (p.name.name.clone(), args.get(i).cloned().unwrap_or(Value::Null))
                })
                .collect();
            sub_vm.push_scope(params);
            let mut returned = Value::Null;
            let result = sub_vm.execute_fn_body(&fn_def.body);
            sub_vm.pop_scope();
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
            "math_abs" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.abs()))
            }
            "math_floor" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.floor()))
            }
            "math_ceil" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.ceil()))
            }
            "json_stringify" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                Ok(Value::Str(native_json_stringify(&v)))
            }
            "json_parse" => {
                let text = args.into_iter().next().unwrap_or(Value::Null).to_str();
                Ok(native_json_parse(&text))
            }
            "math_sin" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.sin()))
            }
            "math_cos" => {
                let x = get_f64(&args.get(0).unwrap_or(&Value::Int(0)));
                Ok(Value::Float(x.cos()))
            }
            _ => Err(CompilerError::runtime_error(format!(
                "unknown math function '{}'",
                name
            ))),
        }
    }

    fn call_function(&mut self, name: &str, argc: usize) -> Result<()> {
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
        if let Some((params, body)) = self.macros.get(name).cloned() {
            let scope_params: Vec<(String, Value)> = params.iter()
                .enumerate()
                .map(|(i, p)| (p.clone(), args.get(i).cloned().unwrap_or(Value::Null)))
                .collect();
            self.push_scope(scope_params);
            let result = self.execute_fn_body(&body);
            self.pop_scope();
            match result {
                Ok(()) => {
                    self.push(Value::Null);
                    return Ok(());
                }
                Err(e) => {
                    let msg = e.message();
                    if msg == "return" {
                        // Return value is on the stack.
                        return Ok(());
                    }
                    return Err(e);
                }
            }
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
            // Eagerly evaluate the generator body, collecting every value
            // yielded (including those yielded inside while/for/loop
            // bodies) into `gen_yield_buffer`. `yield, X` signals "yield"
            // by error; the loop constructs and the body loop below catch
            // the signal, drain the buffer into `yielded`, and continue.
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
            // Execute the body, catching "yield" signals.
            let mut return_value: Option<Value> = None;
            let body_result: Result<()> = (|| {
                for s in &fn_def.body {
                    match self.execute_ast_stmt(s) {
                        Ok(()) => {}
                        Err(e) => {
                            let msg = e.message().to_string();
                            if msg == "yield" {
                                if let Some(buf) = self.gen_yield_buffer.as_mut() {
                                    yielded.append(buf);
                                }
                            } else if msg == "return" {
                                return_value = Some(self.stack.pop().unwrap_or(Value::Null));
                                break;
                            } else {
                                return Err(e);
                            }
                        }
                    }
                }
                Ok(())
            })();
            // Drain any yields produced by the final iteration of a loop
            // whose last statement was a yield.
            if let Some(buf) = self.gen_yield_buffer.take() {
                yielded.extend(buf);
            }
            // Always pop the scope, whether the body completed, returned,
            // yielded to completion, or errored.
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
        if is_lambda {
            if let Some(captures) = self.lambda_captures.get(name).cloned() {
                self.push_scope(captures.into_iter().collect());
            }
        }
        let params: Vec<(String, Value)> = fn_def
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                (p.name.name.clone(), args.get(i).cloned().unwrap_or(Value::Null))
            })
            .collect();
        self.push_scope(params);
        let result = self.execute_fn_body(&fn_def.body);
        self.pop_scope();
        if is_lambda {
            self.pop_scope();
        }
        match result {
            Ok(()) => {
                self.push(Value::Null);
                Ok(())
            }
            Err(e) => {
                let msg = e.message();
                if msg == "return" {
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

    fn execute_fn_body(&mut self, body: &[crate::parser::ast::Stmt]) -> Result<()> {
        // Record the defer stack position before executing the body.
        let defer_marker = self.defer_stack.len();
        let result = (|| {
            for s in body {
                self.execute_ast_stmt(s)?;
            }
            Ok(())
        })();
        // Run deferred statements in LIFO order (reverse of push order).
        // Only run the defers that were pushed during this function body.
        while self.defer_stack.len() > defer_marker {
            if let Some(deferred) = self.defer_stack.pop() {
                // Ignore errors from deferred statements (best-effort).
                let _ = self.execute_ast_stmt(&deferred);
            }
        }
        result
    }

    /// Public entry point for executing a single AST statement (used by
    /// `vredrs test` to run test/bench bodies against an already-running VM).
    /// This delegates to the internal execute_ast_stmt; the wrapper exists
    /// only to expose the method outside the crate.
    pub fn execute_ast_stmt_for_test(&mut self, s: &crate::parser::ast::Stmt) -> Result<()> {
        self.execute_ast_stmt(s)
    }

    fn execute_ast_stmt(&mut self, s: &crate::parser::ast::Stmt) -> Result<()> {
        use crate::parser::ast::Stmt;
        match s {
            Stmt::Return(r) => {
                if let Some(v) = r.values.first() {
                    self.execute_ast_expr(v)?;
                } else {
                    self.push(Value::Null);
                }
                // Signal return via error; call_function catches it.
                Err(CompilerError::control_signal("return"))
            }
            Stmt::Assign(a) => {
                // Handle `del, target` (AssignOp::Delete) specially.
                if matches!(a.operator, crate::parser::ast::AssignOp::Delete) {
                    if let Some(target) = a.targets.first() {
                        use crate::parser::ast::Assignee;
                        match target {
                            Assignee::Identifier(id) => {
                                self.scope_delete(&id.name);
                            }
                            Assignee::Index(ix) => {
                                self.execute_ast_expr(&ix.target)?;
                                let mut container = self.pop()?;
                                self.execute_ast_expr(&ix.index)?;
                                let idx = self.pop()?;
                                // Dispatch to __delitem__ if defined.
                                if let Value::Object(class, _) = &container {
                                    let class_name = class.clone();
                                    if self.find_method(&class_name, "__delitem__").is_ok() {
                                        let receiver = container.clone();
                                        let args = vec![idx];
                                        let _ = self.call_method_on_class(
                                            &class_name,
                                            receiver,
                                            "__delitem__",
                                            args,
                                        )?;
                                        return Ok(());
                                    }
                                }
                                self.delete_index(&mut container, &idx)?;
                                // Store the modified container back into the
                                // original variable (List is a value type,
                                // not a reference).
                                if let crate::parser::ast::Expr::Identifier(id) = ix.target.as_ref() {
                                    self.scope_set(id.name.clone(), container);
                                }
                            }
                            _ => {}
                        }
                    }
                    return Ok(());
                }
                self.execute_ast_expr(&a.value)?;
                if a.targets.len() > 1 {
                    // Multi-target: destructure the tuple/list value.
                    let val = self.pop()?;
                    let items = match &val {
                        Value::Tuple(t) => t.clone(),
                        Value::List(l) => l.clone(),
                        _ => vec![val],
                    };
                    for (i, target) in a.targets.iter().enumerate() {
                        let v = items.get(i).cloned().unwrap_or(Value::Null);
                        self.push(v);
                        self.ast_assign_target(target)?;
                    }
                } else if let Some(target) = a.targets.first() {
                    self.ast_assign_target(target)?;
                } else {
                    self.pop()?;
                }
                Ok(())
            }
            Stmt::Println(p) => {
                for arg in &p.args {
                    self.execute_ast_expr(arg)?;
                    let v = self.pop()?;
                    print!("{}", v.to_str());
                }
                println!();
                Ok(())
            }
            Stmt::Paste(p) => {
                for arg in &p.args {
                    self.execute_ast_expr(arg)?;
                    let v = self.pop()?;
                    print!("{}", v.to_str());
                }
                Ok(())
            }
            Stmt::If(i) => self.execute_ast_if(i),
            Stmt::While(w) => self.execute_ast_while(w),
            Stmt::ForIn(f) => self.execute_ast_for_in(f),
            Stmt::ForRange(f) => self.execute_ast_for_range(f),
            Stmt::Break(_) => Err(CompilerError::control_signal("break")),
            Stmt::Continue(_) => Err(CompilerError::control_signal("continue")),
            Stmt::Expr(e) => {
                self.execute_ast_expr(&e.expr)?;
                self.pop()?;
                Ok(())
            }
            Stmt::Throw(t) => {
                self.execute_ast_expr(&t.value)?;
                let v = self.pop()?;
                Err(CompilerError::runtime_error(format!("throw:{}", v.to_str())))
            }
            Stmt::Try(t) => self.execute_ast_try(t),
            Stmt::Yield(y) => {
                // Evaluate the yielded value (or null) and record it in the
                // generator yield buffer if one is active, then signal
                // "yield" by error so the surrounding loop / generator body
                // loop can record-and-continue.
                let v = if let Some(v) = &y.value {
                    self.execute_ast_expr(v)?;
                    self.pop()?
                } else {
                    Value::Null
                };
                if let Some(buf) = self.gen_yield_buffer.as_mut() {
                    buf.push(v);
                } else {
                    // yield outside a generator: push the value back so the
                    // behaviour matches the previous "signal-only" model.
                    self.push(v);
                }
                Err(CompilerError::control_signal("yield"))
            }
            Stmt::With(w) => self.execute_ast_with(w),
            Stmt::Match(m) => self.execute_ast_match(m),
            Stmt::Defer(d) => {
                // Defer: push onto a defer stack to execute at scope exit.
                // We store the deferred statement; execute_fn_body pops and
                // runs them in reverse order after the body completes.
                self.defer_stack.push((*d.stmt).clone());
                Ok(())
            }
            Stmt::Spawn(sp) => {
                // Spawn a coroutine: evaluate the call eagerly.
                self.execute_ast_expr(&sp.call)?;
                self.pop()?;
                Ok(())
            }
            Stmt::SpawnThread(sp) => {
                // VM is single-threaded; run the call eagerly.
                self.execute_ast_expr(&sp.call)?;
                self.pop()?;
                Ok(())
            }
            Stmt::Assert(a) => {
                self.execute_ast_expr(&a.condition)?;
                let v = self.pop()?;
                if !v.truthy() {
                    let msg = a.message.clone().unwrap_or_else(|| "assertion failed".to_string());
                    return Err(CompilerError::runtime_error(msg));
                }
                Ok(())
            }
            Stmt::Panic(p) => {
                self.execute_ast_expr(&p.message)?;
                let v = self.pop()?;
                Err(CompilerError::runtime_error(format!("panic:{}", v.to_str())))
            }
            Stmt::Loop(l) => {
                loop {
                    match self.execute_block(&l.body) {
                        Ok(()) => {}
                        Err(e) => {
                            let m = e.message();
                            if m == "break" {
                                break;
                            } else if m == "continue" {
                                continue;
                            } else {
                                return Err(e);
                            }
                        }
                    }
                }
                Ok(())
            }
            Stmt::UnsafeBlock(u) => self.execute_block(&u.body),
            Stmt::DirectiveBlock(d) => self.execute_block(&d.body),
            Stmt::ScopeBlock(s) => self.execute_block(&s.body),
            _ => Ok(()),
        }
    }

    /// Execute a try/catch/finally statement.
    fn execute_ast_try(&mut self, t: &crate::parser::ast::TryStmt) -> Result<()> {
        let try_result = self.execute_block(&t.try_body);
        match try_result {
            Ok(()) => {
                // Try succeeded; run finally if present.
                if let Some(fb) = &t.finally_body {
                    self.execute_block(fb)?;
                }
                Ok(())
            }
            Err(e) => {
                let msg = e.message().to_string();
                // Check if this is a throw (starts with "throw:").
                if msg.starts_with("throw:") {
                    let exc_msg = msg["throw:".len()..].to_string();
                    // Run catch if present.
                    if let Some(cb) = &t.catch_body {
                        // Bind the catch variable.
                        if let Some(cv) = &t.catch_var {
                            self.scope_set(cv.name.clone(), Value::Exception(exc_msg));
                        }
                        let catch_result = self.execute_block(cb);
                        // Run finally.
                        if let Some(fb) = &t.finally_body {
                            self.execute_block(fb)?;
                        }
                        return catch_result;
                    }
                    // No catch; run finally then re-throw.
                    if let Some(fb) = &t.finally_body {
                        self.execute_block(fb)?;
                    }
                    return Err(e);
                }
                // Non-throw error (break/continue/return): run finally then propagate.
                if let Some(fb) = &t.finally_body {
                    self.execute_block(fb)?;
                }
                Err(e)
            }
        }
    }

    /// Execute a with-statement (context manager).
    fn execute_ast_with(&mut self, w: &crate::parser::ast::WithStmt) -> Result<()> {
        self.execute_ast_expr(&w.manager)?;
        let manager = self.pop()?;
        // Call __enter__ if it's an object with that method.
        // For non-objects (dicts, file handles), use the manager itself.
        let entered = if matches!(manager, Value::Object(_, _)) {
            self.call_dunder(&manager, "__enter__", vec![])?
        } else {
            manager.clone()
        };
        if let Some(var) = &w.var {
            self.scope_set(var.name.clone(), entered);
        }
        let body_result = self.execute_block(&w.body);
        // Call __exit__ on the manager (if it's an object).
        if matches!(manager, Value::Object(_, _)) {
            let _ = self.call_dunder(&manager, "__exit__", vec![Value::Null]);
        }
        body_result
    }

    /// Call a dunder method on an object (e.g. __enter__, __exit__).
    /// Returns the method's return value, or Null if not found.
    fn call_dunder(&mut self, receiver: &Value, method: &str, args: Vec<Value>) -> Result<Value> {
        match receiver {
            Value::Object(class, _) => {
                self.call_method_on_class(class, receiver.clone(), method, args)
            }
            _ => Ok(Value::Null),
        }
    }

    /// Load a module by path: parse it, execute it in a fresh scope, collect exports.
    fn load_module_by_path(&mut self, path: &str) -> Result<HashMap<String, Value>> {
        // Check cache first.
        if let Some(cached) = self.module_cache.get(path) {
            return Ok(cached.clone());
        }
        // Built-in stdlib modules.
        if let Some(exports) = self.load_builtin_module(path) {
            self.module_cache.insert(path.to_string(), exports.clone());
            return Ok(exports);
        }
        // Resolve the module path relative to base_dir.
        let resolved = resolve_module_path(&self.base_dir, path);
        let source = std::fs::read_to_string(&resolved).map_err(|e| {
            CompilerError::runtime_error(format!(
                "module '{}' not found ({}: {})",
                path,
                resolved.display(),
                e
            ))
        })?;
        // Parse the module source.
        let mut lex = crate::lexer::Lexer::new(&source, 0);
        let tokens = lex.tokenize()?;
        let mut parser = crate::parser::Parser::new(tokens, 0);
        let program = parser.parse_program()?;
        // Save current globals; execute the module in a fresh scope so its
        // top-level definitions don't leak into the importer.
        let saved_globals = self.globals.clone();
        self.globals.clear();
        // Execute top-level statements. TopLevel::Import and TopLevel::Export
        // are NOT Stmt variants; skip them here.
        for d in &program.declarations {
            match d {
                crate::parser::ast::TopLevel::Statement(s) => {
                    let _ = self.execute_ast_stmt(s);
                }
                crate::parser::ast::TopLevel::FnDef(f) => {
                    // Make the function available as a Func global.
                    self.globals.insert(
                        f.name.name.clone(),
                        Value::Func(f.name.name.clone()),
                    );
                }
                _ => {}
            }
        }
        // Collect exported symbols; if no explicit exports, expose all globals.
        let mut exports: HashMap<String, Value> = HashMap::new();
        let mut has_explicit_exports = false;
        for d in &program.declarations {
            if let crate::parser::ast::TopLevel::Export(exp) = d {
                has_explicit_exports = true;
                for sym in &exp.symbols {
                    if let Some(v) = self.globals.get(&sym.name) {
                        exports.insert(sym.name.clone(), v.clone());
                    }
                }
            }
        }
        if !has_explicit_exports {
            // Expose all top-level fn definitions and globals.
            for d in &program.declarations {
                if let crate::parser::ast::TopLevel::FnDef(f) = d {
                    if let Some(v) = self.globals.get(&f.name.name) {
                        exports.insert(f.name.name.clone(), v.clone());
                    }
                }
            }
            // Also expose simple globals (variables).
            for (k, v) in &self.globals {
                if !k.starts_with("__") {
                    exports.insert(k.clone(), v.clone());
                }
            }
        }
        // Restore the importer's globals.
        self.globals = saved_globals;
        // Cache the exports.
        self.module_cache.insert(path.to_string(), exports.clone());
        Ok(exports)
    }

    /// Return exports for a built-in stdlib module, if `name` is one.
    fn load_builtin_module(&self, name: &str) -> Option<HashMap<String, Value>> {
        let mut exports: HashMap<String, Value> = HashMap::new();
        match name {
            "math" => {
                exports.insert("pi".to_string(), Value::Float(std::f64::consts::PI));
                exports.insert("e".to_string(), Value::Float(std::f64::consts::E));
                exports.insert("sqrt".to_string(), Value::Func("math_sqrt".to_string()));
                exports.insert("pow".to_string(), Value::Func("math_pow".to_string()));
                exports.insert("sin".to_string(), Value::Func("math_sin".to_string()));
                exports.insert("cos".to_string(), Value::Func("math_cos".to_string()));
                exports.insert("tan".to_string(), Value::Func("math_tan".to_string()));
                exports.insert("log".to_string(), Value::Func("math_log".to_string()));
                exports.insert("abs".to_string(), Value::Func("abs".to_string()));
                exports.insert("floor".to_string(), Value::Func("floor".to_string()));
                exports.insert("ceil".to_string(), Value::Func("ceil".to_string()));
                exports.insert("round".to_string(), Value::Func("round".to_string()));
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
                Some(exports)
            }
            "os" => {
                exports.insert("args".to_string(), Value::Func("os_args".to_string()));
                exports.insert("env".to_string(), Value::Func("os_env".to_string()));
                exports.insert("setenv".to_string(), Value::Func("os_setenv".to_string()));
                exports.insert("exec".to_string(), Value::Func("os_exec".to_string()));
                exports.insert("system".to_string(), Value::Func("os_system".to_string()));
                exports.insert("cwd".to_string(), Value::Func("os_cwd".to_string()));
                exports.insert("chdir".to_string(), Value::Func("os_chdir".to_string()));
                Some(exports)
            }
            "time" => {
                exports.insert("sleep".to_string(), Value::Func("time_sleep".to_string()));
                exports.insert("now".to_string(), Value::Func("time_now".to_string()));
                exports.insert("unix".to_string(), Value::Func("time_unix".to_string()));
                exports.insert("rand_int".to_string(), Value::Func("time_rand_int".to_string()));
                Some(exports)
            }
            "rand" => {
                exports.insert("intn".to_string(), Value::Func("rand_intn".to_string()));
                exports.insert("int".to_string(), Value::Func("rand_int".to_string()));
                exports.insert("float".to_string(), Value::Func("rand_float".to_string()));
                exports.insert("bool".to_string(), Value::Func("rand_bool".to_string()));
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
                Some(exports)
            }
            "json" => {
                exports.insert("parse".to_string(), Value::Func("json_parse".to_string()));
                exports.insert("stringify".to_string(), Value::Func("json_stringify".to_string()));
                Some(exports)
            }
            // fmt, collections, net are provided by .veds files in
            // src/runtime/std/. They fall through to the file-based loader.
            _ => None,
        }
    }

    /// Execute a match statement by evaluating each case's pattern.
    fn execute_ast_match(&mut self, m: &crate::parser::ast::MatchStmt) -> Result<()> {
        self.execute_ast_expr(&m.expr)?;
        let scrutinee = self.pop()?;
        for case in &m.cases {
            if self.pattern_matches(&case.pattern, &scrutinee)? {
                if let Some(guard) = &case.guard {
                    self.execute_ast_expr(guard)?;
                    let g = self.pop()?;
                    if !g.truthy() {
                        continue;
                    }
                }
                return self.execute_block(&case.body);
            }
        }
        if let Some(else_body) = &m.else_case {
            return self.execute_block(else_body);
        }
        Ok(())
    }

    /// Test whether a pattern matches a value. Returns true on match.
    fn pattern_matches(
        &mut self,
        pattern: &crate::parser::ast::Pattern,
        value: &Value,
    ) -> Result<bool> {
        use crate::parser::ast::Pattern;
        match pattern {
            Pattern::Wildcard(_) => Ok(true),
            Pattern::Literal(lp) => {
                self.execute_ast_expr(&lp.literal)?;
                let lit = self.pop()?;
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
    fn execute_block(&mut self, body: &[crate::parser::ast::Stmt]) -> Result<()> {
        for s in body {
            self.execute_ast_stmt(s)?;
        }
        Ok(())
    }

    fn execute_ast_if(&mut self, i: &crate::parser::ast::IfStmt) -> Result<()> {
        self.execute_ast_expr(&i.condition)?;
        let cond = self.pop()?;
        if cond.truthy() {
            for s in &i.then_body {
                self.execute_ast_stmt(s)?;
            }
            return Ok(());
        }
        for (elif_cond, elif_body) in &i.elif_chain {
            self.execute_ast_expr(elif_cond)?;
            let c = self.pop()?;
            if c.truthy() {
                for s in elif_body {
                    self.execute_ast_stmt(s)?;
                }
                return Ok(());
            }
        }
        if let Some(else_body) = &i.else_body {
            for s in else_body {
                self.execute_ast_stmt(s)?;
            }
        }
        Ok(())
    }

    fn execute_ast_while(&mut self, w: &crate::parser::ast::WhileStmt) -> Result<()> {
        loop {
            self.execute_ast_expr(&w.condition)?;
            let cond = self.pop()?;
            if !cond.truthy() {
                break;
            }
            let mut do_continue = false;
            for s in &w.body {
                match self.execute_ast_stmt(s) {
                    Ok(()) => {}
                    Err(e) => {
                        let msg = e.message();
                        if msg == "break" {
                            return Ok(());
                        }
                        if msg == "continue" {
                            do_continue = true;
                            break;
                        }
                        if msg == "yield" {
                            // Yield inside the loop: the value is already in
                            // the generator buffer. Continue executing the
                            // remaining statements of this iteration so the
                            // loop variables advance normally.
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
            if do_continue {
                continue;
            }
        }
        Ok(())
    }

    fn execute_ast_for_in(&mut self, f: &crate::parser::ast::ForInStmt) -> Result<()> {
        self.execute_ast_expr(&f.iterable)?;
        let iterable = self.pop()?;
        // If the iterable is an Object with a `next` method, use the iterator
        // protocol: call next() until it returns a `stop()` exception.
        if let Value::Object(class, _) = &iterable {
            if self.find_method(class, "next").is_ok() {
                let class = class.clone();
                loop {
                    let v = self.call_method_on_class(
                        &class,
                        iterable.clone(),
                        "next",
                        vec![],
                    )?;
                    // Check for stop() signal (Exception "StopIteration").
                    if let Value::Exception(msg) = &v {
                        if msg == "StopIteration" {
                            break;
                        }
                    }
                    self.scope_set(f.var.name.clone(), v);
                    let mut do_continue = false;
                    for s in &f.body {
                        match self.execute_ast_stmt(s) {
                            Ok(()) => {}
                            Err(e) => {
                                let msg = e.message();
                                if msg == "break" {
                                    return Ok(());
                                }
                                if msg == "continue" {
                                    do_continue = true;
                                    break;
                                }
                                if msg == "yield" {
                                    continue;
                                }
                                return Err(e);
                            }
                        }
                    }
                    if do_continue {
                        continue;
                    }
                }
                return Ok(());
            }
        }
        let items = match &iterable {
            Value::List(l) => l.clone(),
            Value::Tuple(t) => t.clone(),
            Value::Str(s) => s
                .chars()
                .map(|c| Value::Str(c.to_string()))
                .collect(),
            Value::Dict(d) => d.keys().map(|k| Value::Str(k.clone())).collect(),
            Value::Generator(g) => {
                // Iterate the generator's pre-collected yielded values.
                g.borrow().yielded_values.clone()
            }
            _ => Vec::new(),
        };
        for item in items {
            // Store the loop variable in the current scope (function-local
            // when inside a call frame, otherwise global).
            self.scope_set(f.var.name.clone(), item);
            let mut do_continue = false;
            for s in &f.body {
                match self.execute_ast_stmt(s) {
                    Ok(()) => {}
                    Err(e) => {
                        let msg = e.message();
                        if msg == "break" {
                            return Ok(());
                        }
                        if msg == "continue" {
                            do_continue = true;
                            break;
                        }
                        if msg == "yield" {
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
            if do_continue {
                continue;
            }
        }
        Ok(())
    }

    fn execute_ast_for_range(&mut self, f: &crate::parser::ast::ForRangeStmt) -> Result<()> {
        self.execute_ast_expr(&f.from)?;
        let start = self.pop()?.to_int()?;
        self.execute_ast_expr(&f.to)?;
        let end = self.pop()?.to_int()?;
        // Evaluate the optional step expression. Defaults to 1.
        let step: i64 = if let Some(step_expr) = &f.step {
            self.execute_ast_expr(step_expr)?;
            self.pop()?.to_int()?
        } else {
            1
        };
        if step == 0 {
            return Err(CompilerError::runtime_error("for-range step cannot be zero"));
        }
        let mut i = start;
        if step > 0 {
            while i < end {
                self.scope_set(f.var.name.clone(), Value::Int(i));
                for s in &f.body {
                    match self.execute_ast_stmt(s) {
                        Ok(()) => {}
                        Err(e) => {
                            let msg = e.message();
                            if msg == "break" {
                                return Ok(());
                            }
                            if msg == "continue" {
                                break;
                            }
                            if msg == "yield" {
                                continue;
                            }
                            return Err(e);
                        }
                    }
                }
                i += step;
            }
        } else {
            while i > end {
                self.scope_set(f.var.name.clone(), Value::Int(i));
                for s in &f.body {
                    match self.execute_ast_stmt(s) {
                        Ok(()) => {}
                        Err(e) => {
                            let msg = e.message();
                            if msg == "break" {
                                return Ok(());
                            }
                            if msg == "continue" {
                                break;
                            }
                            if msg == "yield" {
                                continue;
                            }
                            return Err(e);
                        }
                    }
                }
                i += step;
            }
        }
        Ok(())
    }

    fn ast_assign_target(&mut self, target: &crate::parser::ast::Assignee) -> Result<()> {
        use crate::parser::ast::Assignee;
        match target {
            Assignee::Identifier(id) => {
                let v = self.pop()?;
                if id.name == "self" {
                    // Assignment to self — no-op (self is already bound).
                } else {
                    self.scope_set(id.name.clone(), v);
                }
                Ok(())
            }
            Assignee::Index(ix) => {
                let val = self.pop()?;
                let idx = self.pop()?;
                let mut container = self.pop()?;
                self.index_set(&mut container, &idx, val)?;
                Ok(())
            }
            Assignee::Member(m) => {
                // self.field = value OR obj.field = value
                let val = self.pop()?;
                // If the target is `self`, modify the self object in globals.
                if let crate::parser::ast::Expr::Identifier(id) = m.target.as_ref() {
                    if id.name == "self" {
                        if let Some(Value::Object(class, _)) = self.globals.get("self").cloned() {
                            // Dispatch to __setattr__ if defined.
                            if self.find_method(&class, "__setattr__").is_ok() {
                                let receiver = self.globals.get("self").cloned().unwrap_or(Value::Null);
                                let args = vec![Value::Str(m.member.name.clone()), val];
                                let _ = self.call_method_on_class(&class, receiver, "__setattr__", args)?;
                                return Ok(());
                            }
                            if let Some(Value::Object(_, fields)) = self.globals.get("self") {
                                fields.borrow_mut().insert(m.member.name.clone(), val);
                            }
                        }
                        return Ok(());
                    }
                    // Also check if there's a global with this name that
                    // holds an Object — its fields are shared via Rc<RefCell>,
                    // so we can mutate in place.
                    if let Some(Value::Object(class, fields)) = self.scope_get(&id.name) {
                        // Dispatch to __setattr__ if defined.
                        if self.find_method(&class, "__setattr__").is_ok() {
                            let receiver = Value::Object(class.clone(), fields.clone());
                            let args = vec![Value::Str(m.member.name.clone()), val];
                            let _ = self.call_method_on_class(&class, receiver, "__setattr__", args)?;
                            return Ok(());
                        }
                        fields.borrow_mut().insert(m.member.name.clone(), val);
                        return Ok(());
                    }
                }
                // General case: evaluate target, dispatch to __setattr__
                // if defined, otherwise mutate the shared field map.
                self.execute_ast_expr(&m.target)?;
                let obj = self.pop()?;
                if let Value::Object(class, fields) = &obj {
                    if self.find_method(class, "__setattr__").is_ok() {
                        let class_name = class.clone();
                        let fields_clone = fields.clone();
                        let args = vec![Value::Str(m.member.name.clone()), val];
                        let _ = self.call_method_on_class(
                            &class_name,
                            Value::Object(class_name.clone(), fields_clone),
                            "__setattr__",
                            args,
                        )?;
                    } else {
                        fields.borrow_mut().insert(m.member.name.clone(), val);
                    }
                }
                Ok(())
            }
            _ => {
                self.pop()?;
                Ok(())
            }
        }
    }

    fn execute_ast_expr(&mut self, e: &crate::parser::ast::Expr) -> Result<()> {
        use crate::parser::ast::{BinaryOp, Expr, UnaryOp};
        match e {
            Expr::Integer(i) => {
                self.push(Value::Int(i.value));
                Ok(())
            }
            Expr::Float(f) => {
                self.push(Value::Float(f.value));
                Ok(())
            }
            Expr::Bool(b) => {
                self.push(Value::Bool(b.value));
                Ok(())
            }
            Expr::Null(_) => {
                self.push(Value::Null);
                Ok(())
            }
            Expr::String_(s) => {
                let mut text = String::new();
                for p in &s.parts {
                    match p {
                        crate::parser::ast::StringPart::Text(t) => {
                            text.push_str(&interpret_escapes(t));
                        }
                        crate::parser::ast::StringPart::Interpolation(expr) => {
                            // Evaluate the interpolated expression and append
                            // its string representation.
                            self.execute_ast_expr(expr)?;
                            let v = self.pop()?;
                            text.push_str(&v.to_str());
                        }
                    }
                }
                self.push(Value::Str(text));
                Ok(())
            }
            Expr::MultiLineString(s) => {
                let mut text = String::new();
                for p in &s.parts {
                    match p {
                        crate::parser::ast::StringPart::Text(t) => {
                            text.push_str(&interpret_escapes(t));
                        }
                        crate::parser::ast::StringPart::Interpolation(expr) => {
                            self.execute_ast_expr(expr)?;
                            let v = self.pop()?;
                            text.push_str(&v.to_str());
                        }
                    }
                }
                self.push(Value::Str(text));
                Ok(())
            }
            Expr::Identifier(id) => {
                let v = self.scope_get(&id.name).unwrap_or(Value::Null);
                self.push(v);
                Ok(())
            }
            Expr::Binary(b) => {
                // Short-circuit evaluation for and/or.
                match b.operator {
                    BinaryOp::And => {
                        self.execute_ast_expr(&b.left)?;
                        let l = self.pop()?;
                        if !l.truthy() {
                            self.push(Value::Bool(false));
                            return Ok(());
                        }
                        self.execute_ast_expr(&b.right)?;
                        let r = self.pop()?;
                        self.push(Value::Bool(r.truthy()));
                        return Ok(());
                    }
                    BinaryOp::Or => {
                        self.execute_ast_expr(&b.left)?;
                        let l = self.pop()?;
                        if l.truthy() {
                            self.push(Value::Bool(true));
                            return Ok(());
                        }
                        self.execute_ast_expr(&b.right)?;
                        let r = self.pop()?;
                        self.push(Value::Bool(r.truthy()));
                        return Ok(());
                    }
                    _ => {}
                }
                self.execute_ast_expr(&b.left)?;
                self.execute_ast_expr(&b.right)?;
                let r = self.pop()?;
                let l = self.pop()?;
                let v = self.ast_binary(l, &b.operator, r)?;
                self.push(v);
                Ok(())
            }
            Expr::Unary(u) => {
                self.execute_ast_expr(&u.operand)?;
                let v = self.pop()?;
                let result = match u.operator {
                    UnaryOp::Neg => match v {
                        Value::Int(i) => Value::Int(-i),
                        Value::Float(f) => Value::Float(-f),
                        _ => Value::Null,
                    },
                    UnaryOp::Not | UnaryOp::Bang => Value::Bool(!v.truthy()),
                };
                self.push(result);
                Ok(())
            }
            Expr::List(l) => {
                let mut elements = Vec::new();
                for e in &l.elements {
                    self.execute_ast_expr(e)?;
                    elements.push(self.pop()?);
                }
                self.push(Value::List(elements));
                Ok(())
            }
            Expr::Tuple(t) => {
                let mut elements = Vec::new();
                for e in &t.elements {
                    self.execute_ast_expr(e)?;
                    elements.push(self.pop()?);
                }
                self.push(Value::Tuple(elements));
                Ok(())
            }
            Expr::Dict(d) => {
                let mut map = HashMap::new();
                for (k, v) in &d.entries {
                    self.execute_ast_expr(k)?;
                    let key = self.pop()?;
                    self.execute_ast_expr(v)?;
                    let val = self.pop()?;
                    map.insert(key.to_str(), val);
                }
                self.push(Value::Dict(map));
                Ok(())
            }
            Expr::Index(i) => {
                self.execute_ast_expr(&i.target)?;
                self.execute_ast_expr(&i.index)?;
                let idx = self.pop()?;
                let container = self.pop()?;
                let v = self.index_get(&container, &idx)?;
                self.push(v);
                Ok(())
            }
            Expr::Call(c) => {
                // Method call?
                if let Expr::MemberAccess(m) = c.callee.as_ref() {
                    // super.method(args): dispatch to parent of the class
                    // where the current method was defined (not self's class).
                    if let Expr::Identifier(id) = m.target.as_ref() {
                        if id.name == "super" {
                            // Get the current method context (class where
                            // the currently-executing method was defined).
                            let current_class = self.method_context.last()
                                .map(|(c, _)| c.clone());
                            if let Some(class) = current_class {
                                // Find the parent of the current class.
                                let parent_name = self
                                    .find_parent_class(&class)
                                    .map(|n| n.to_string());
                                if let Some(parent) = parent_name {
                                    let mut args = Vec::new();
                                    for a in &c.args {
                                        self.execute_ast_expr(a)?;
                                        args.push(self.pop()?);
                                    }
                                    let receiver = self
                                        .globals
                                        .get("self")
                                        .cloned()
                                        .unwrap_or(Value::Null);
                                    let v = self.call_method_on_class(
                                        &parent,
                                        receiver,
                                        &m.member.name,
                                        args,
                                    )?;
                                    self.push(v);
                                    return Ok(());
                                }
                            }
                        }
                    }
                    // receiver.method(args)
                    self.execute_ast_expr(&m.target)?;
                    let receiver = self.pop()?;
                    // If receiver is a Module, look up the method as a Func.
                    if let Value::Module(_, exports) = &receiver {
                        if let Some(func_val) = exports.get(&m.member.name) {
                            let mut args = Vec::new();
                            for a in &c.args {
                                self.execute_ast_expr(a)?;
                                args.push(self.pop()?);
                            }
                            let v = self.call_value(func_val, args)?;
                            self.push(v);
                            return Ok(());
                        }
                    }
                    let mut args = Vec::new();
                    for a in &c.args {
                        self.execute_ast_expr(a)?;
                        args.push(self.pop()?);
                    }
                    let v = self.ast_method_call(receiver, &m.member.name, args)?;
                    self.push(v);
                    return Ok(());
                }
                if let Expr::Identifier(id) = c.callee.as_ref() {
                    // Builtin?
                    if is_builtin(&id.name) {
                        let mut args = Vec::new();
                        for a in &c.args {
                            self.execute_ast_expr(a)?;
                            args.push(self.pop()?);
                        }
                        let v = self.call_builtin_value(&id.name, args)?;
                        self.push(v);
                        return Ok(());
                    }
                    // Class constructor? Check if the name is a class in the
                    // program OR a Class value in globals (set by preprocess).
                    let is_class = {
                        let prog_has_class = if let Some(prog) = &self.program {
                            prog.declarations.iter().any(|d| {
                                if let crate::parser::ast::TopLevel::ClassDef(cd) = d {
                                    cd.name.name == id.name
                                } else {
                                    false
                                }
                            })
                        } else {
                            false
                        };
                        let global_is_class = self
                            .globals
                            .get(&id.name)
                            .map(|v| matches!(v, Value::Class(_)))
                            .unwrap_or(false);
                        prog_has_class || global_is_class
                    };
                    if is_class {
                        let mut args = Vec::new();
                        for a in &c.args {
                            self.execute_ast_expr(a)?;
                            args.push(self.pop()?);
                        }
                        let obj = self.instantiate_class(&id.name, args)?;
                        self.push(obj);
                        return Ok(());
                    }
                    // User function: push args, then delegate to call_function.
                    for a in &c.args {
                        self.execute_ast_expr(a)?;
                    }
                    self.call_function(&id.name, c.args.len())?;
                    return Ok(());
                }
                // Method call on a class name (e.g. Animal.new(...))?
                if let Expr::MemberAccess(m) = c.callee.as_ref() {
                    if let Expr::Identifier(class_id) = m.target.as_ref() {
                        if self.is_class_name(&class_id.name)
                            || self
                                .globals
                                .get(&class_id.name)
                                .map(|v| matches!(v, Value::Class(_)))
                                .unwrap_or(false)
                        {
                            // This is ClassName.method(args) — but for constructors
                            // it's ClassName.new(args). Treat as class instantiation.
                            if m.member.name == "new" {
                                let mut args = Vec::new();
                                for a in &c.args {
                                    self.execute_ast_expr(a)?;
                                    args.push(self.pop()?);
                                }
                                let obj = self.instantiate_class(&class_id.name, args)?;
                                self.push(obj);
                                return Ok(());
                            }
                        }
                    }
                }
                Err(CompilerError::runtime_error("unsupported call"))
            }
            Expr::MemberAccess(m) => {
                // Field access on object.
                self.execute_ast_expr(&m.target)?;
                let obj = self.pop()?;
                match &obj {
                    Value::Object(_, fields) => {
                        let v = fields
                            .borrow()
                            .get(&m.member.name)
                            .cloned()
                            .unwrap_or(Value::Null);
                        self.push(v);
                        Ok(())
                    }
                    Value::Module(_, exports) => {
                        let v = exports.get(&m.member.name).cloned().unwrap_or(Value::Null);
                        self.push(v);
                        Ok(())
                    }
                    Value::Dict(d) => {
                        let v = d.get(&m.member.name).cloned().unwrap_or(Value::Null);
                        self.push(v);
                        Ok(())
                    }
                    Value::Class(class_name) => {
                        // ClassName.X — could be an enum-variant accessor
                        // (if ClassName is an enum), or a static member.
                        // Look up the global "ClassName.X" first (enum variant).
                        let key = format!("{}.{}", class_name, m.member.name);
                        if let Some(v) = self.globals.get(&key) {
                            self.push(v.clone());
                            return Ok(());
                        }
                        // Otherwise, treat as a static method/field (null).
                        self.push(Value::Null);
                        Ok(())
                    }
                    Value::Str(s) if s.starts_with("<enum ") && s.ends_with('>') => {
                        // Enum-value accessor: `Color.Red` where `Color` was
                        // registered as `<enum Color>`. Look up the global
                        // "Color.Red" which holds the variant object.
                        let enum_name = &s[6..s.len() - 1];
                        let key = format!("{}.{}", enum_name, m.member.name);
                        if let Some(v) = self.globals.get(&key) {
                            self.push(v.clone());
                            return Ok(());
                        }
                        self.push(Value::Null);
                        Ok(())
                    }
                    _ => {
                        self.push(Value::Null);
                        Ok(())
                    }
                }
            }
            Expr::ListComprehension(lc) => {
                self.execute_list_comprehension(lc)?;
                Ok(())
            }
            Expr::DictComprehension(dc) => {
                self.execute_dict_comprehension(dc)?;
                Ok(())
            }
            Expr::Range(r) => {
                let start = if let Some(s) = &r.start {
                    self.execute_ast_expr(s)?;
                    self.pop()?.to_int()?
                } else {
                    0
                };
                let end = if let Some(e) = &r.end {
                    self.execute_ast_expr(e)?;
                    self.pop()?.to_int()?
                } else {
                    0
                };
                let step = if let Some(st) = &r.step {
                    self.execute_ast_expr(st)?;
                    self.pop()?.to_int()?
                } else {
                    1
                };
                let mut items = Vec::new();
                let mut i = start;
                if step > 0 {
                    let limit = if r.inclusive { end + 1 } else { end };
                    while i < limit {
                        items.push(Value::Int(i));
                        i += step;
                    }
                } else if step < 0 {
                    let limit = if r.inclusive { end - 1 } else { end };
                    while i > limit {
                        items.push(Value::Int(i));
                        i += step;
                    }
                }
                self.push(Value::List(items));
                Ok(())
            }
            Expr::Slice(sl) => {
                self.execute_ast_expr(&sl.target)?;
                let target = self.pop()?;
                // Compute step first; it affects the defaults for start/end.
                let step = if let Some(st) = &sl.step {
                    self.execute_ast_expr(st)?;
                    self.pop()?.to_int()?
                } else {
                    1
                };
                let (start, end) = if step > 0 {
                    let s = if let Some(s) = &sl.start {
                        self.execute_ast_expr(s)?;
                        self.pop()?.to_int()?
                    } else {
                        0
                    };
                    let e = if let Some(e) = &sl.end {
                        self.execute_ast_expr(e)?;
                        let v = self.pop()?;
                        if matches!(v, Value::Null) {
                            isize::MAX as i64
                        } else {
                            v.to_int()?
                        }
                    } else {
                        isize::MAX as i64
                    };
                    (s, e)
                } else {
                    // Negative step: default start is end-of-sequence,
                    // default end is before-the-beginning.
                    let s = if let Some(s) = &sl.start {
                        self.execute_ast_expr(s)?;
                        self.pop()?.to_int()?
                    } else {
                        isize::MAX as i64
                    };
                    let e = if let Some(e) = &sl.end {
                        self.execute_ast_expr(e)?;
                        let v = self.pop()?;
                        if matches!(v, Value::Null) {
                            isize::MIN as i64
                        } else {
                            v.to_int()?
                        }
                    } else {
                        isize::MIN as i64
                    };
                    (s, e)
                };
                self.push(self.slice_value(target, start, end, step)?);
                Ok(())
            }
            Expr::MethodCall(m) => {
                self.execute_ast_expr(&m.receiver)?;
                let receiver = self.pop()?;
                let mut args = Vec::new();
                for a in &m.args {
                    self.execute_ast_expr(a)?;
                    args.push(self.pop()?);
                }
                let v = self.call_method_on_value(receiver, &m.method.name, args)?;
                self.push(v);
                Ok(())
            }
            Expr::Resume(r) => {
                self.execute_ast_expr(&r.handle)?;
                let handle = self.pop()?;
                let mut args = Vec::new();
                for a in &r.values {
                    self.execute_ast_expr(a)?;
                    args.push(self.pop()?);
                }
                let v = self.resume_generator(handle, args)?;
                self.push(v);
                Ok(())
            }
            Expr::Await(a) => {
                // The bytecode VM runs single-threaded; await is equivalent
                // to evaluating the inner expression directly. If the inner
                // expression is a coroutine call, executing it returns the
                // coroutine's result.
                self.execute_ast_expr(&a.expr)?;
                Ok(())
            }
            Expr::Spawn(s) => {
                // Spawn a coroutine: evaluate the call eagerly and wrap the
                // result in a generator-like value (single yielded value).
                self.execute_ast_expr(&s.call)?;
                Ok(())
            }
            Expr::Coro(c) => {
                self.execute_ast_expr(&c.function)?;
                let func = self.pop()?;
                let mut args = Vec::new();
                for a in &c.args {
                    self.execute_ast_expr(a)?;
                    args.push(self.pop()?);
                }
                let v = self.call_value(&func, args)?;
                self.push(v);
                Ok(())
            }
            Expr::Ternary(t) => {
                self.execute_ast_expr(&t.condition)?;
                let cond = self.pop()?;
                if cond.truthy() {
                    self.execute_ast_expr(&t.true_branch)?;
                } else {
                    self.execute_ast_expr(&t.false_branch)?;
                }
                Ok(())
            }
            Expr::NullCoalesce(nc) => {
                self.execute_ast_expr(&nc.left)?;
                let l = self.pop()?;
                if matches!(l, Value::Null) {
                    self.execute_ast_expr(&nc.right)?;
                } else {
                    self.push(l);
                }
                Ok(())
            }
            Expr::OptionalChain(oc) => {
                self.execute_ast_expr(&oc.target)?;
                let mut current = self.pop()?;
                if matches!(current, Value::Null) {
                    self.push(Value::Null);
                    return Ok(());
                }
                // Walk the chain. If any intermediate is null, push null.
                for link in &oc.chain {
                    if matches!(current, Value::Null) {
                        self.push(Value::Null);
                        return Ok(());
                    }
                    current = match link {
                        crate::parser::ast::OptionalChainLink::Member(id) => {
                            match &current {
                                Value::Object(_, fields) => fields
                                    .borrow()
                                    .get(&id.name)
                                    .cloned()
                                    .unwrap_or(Value::Null),
                                Value::Module(_, exports) => exports
                                    .get(&id.name)
                                    .cloned()
                                    .unwrap_or(Value::Null),
                                Value::Dict(d) => d
                                    .get(&id.name)
                                    .cloned()
                                    .unwrap_or(Value::Null),
                                _ => Value::Null,
                            }
                        }
                        crate::parser::ast::OptionalChainLink::Call {
                            method,
                            args,
                        } => {
                            let mut arg_vals = Vec::new();
                            for a in args {
                                self.execute_ast_expr(a)?;
                                arg_vals.push(self.pop()?);
                            }
                            self.call_method_on_value(
                                current.clone(),
                                &method.name,
                                arg_vals,
                            )?
                        }
                        crate::parser::ast::OptionalChainLink::Index(idx_expr) => {
                            self.execute_ast_expr(idx_expr)?;
                            let idx = self.pop()?;
                            self.index_get(&current, &idx)?
                        }
                    };
                }
                self.push(current);
                Ok(())
            }
            Expr::Cast(c) => {
                // Casts convert the value to the named type:
                //   int as float → f64
                //   float as int → truncated i64
                //   int/float as str → decimal text
                //   str as int → parsed i64 (0 if unparseable)
                //   str as float → parsed f64 (0.0 if unparseable)
                //   bool as int → 0/1
                //   anything as bool → truthiness
                // Unknown target types pass the value through unchanged.
                self.execute_ast_expr(&c.expr)?;
                let v = self.pop()?;
                let result = match &c.type_expr {
                    crate::parser::ast::TypeExpr::Basic(bt, _) => {
                        use crate::parser::ast::BasicType;
                        match bt {
                            BasicType::Int => match v {
                                Value::Int(i) => Value::Int(i),
                                Value::Float(f) => Value::Int(f as i64),
                                Value::Bool(b) => Value::Int(if b { 1 } else { 0 }),
                                Value::Str(s) => Value::Int(s.trim().parse::<i64>().unwrap_or(0)),
                                _ => Value::Int(0),
                            },
                            BasicType::Float => match v {
                                Value::Int(i) => Value::Float(i as f64),
                                Value::Float(f) => Value::Float(f),
                                Value::Str(s) => Value::Float(s.trim().parse::<f64>().unwrap_or(0.0)),
                                Value::Bool(b) => Value::Float(if b { 1.0 } else { 0.0 }),
                                _ => Value::Float(0.0),
                            },
                            BasicType::Str => Value::Str(v.to_str()),
                            BasicType::Bool => Value::Bool(v.truthy()),
                            BasicType::Null => Value::Null,
                            _ => v,
                        }
                    }
                    _ => v, // unknown cast — pass through
                };
                self.push(result);
                Ok(())
            }
            Expr::TryPropagate(t) => {
                self.execute_ast_expr(&t.expr)?;
                let v = self.pop()?;
                if let Value::Exception(msg) = &v {
                    // Propagate the exception by signaling a throw.
                    return Err(CompilerError::runtime_error(format!("throw:{}", msg)));
                }
                self.push(v);
                Ok(())
            }
            Expr::Spread(s) => {
                // Spread is contextual; in isolation, evaluate the inner expr.
                self.execute_ast_expr(&s.expr)?;
                Ok(())
            }
            Expr::Pipe(p) => {
                // x |> f  ==>  f(x)
                // x |> f(a, b)  ==>  f(x, a, b)
                self.execute_ast_expr(&p.left)?;
                let x = self.pop()?;
                match &*p.right {
                    Expr::Call(c) => {
                        // Evaluate the callee and original args, then
                        // prepend x to the argument list.
                        self.execute_ast_expr(&c.callee)?;
                        let f = self.pop()?;
                        let mut args = vec![x];
                        for a in &c.args {
                            self.execute_ast_expr(a)?;
                            args.push(self.pop()?);
                        }
                        let v = self.call_value(&f, args)?;
                        self.push(v);
                    }
                    _ => {
                        self.execute_ast_expr(&p.right)?;
                        let f = self.pop()?;
                        let v = self.call_value(&f, vec![x])?;
                        self.push(v);
                    }
                }
                Ok(())
            }
            Expr::Postfix(p) => {
                self.execute_ast_expr(&p.operand)?;
                let v = self.pop()?;
                let result = match &p.operator {
                    crate::parser::ast::PostfixOp::Length => {
                        Value::Int(self.iterable_len(&v) as i64)
                    }
                    crate::parser::ast::PostfixOp::Reverse => {
                        let items = self.iterable_to_list(&v);
                        let mut rev = items;
                        rev.reverse();
                        Value::List(rev)
                    }
                    crate::parser::ast::PostfixOp::AscSort => {
                        let mut items = self.iterable_to_list(&v);
                        items.sort_by(|a, b| a.cmp(b));
                        Value::List(items)
                    }
                    crate::parser::ast::PostfixOp::DescSort => {
                        let mut items = self.iterable_to_list(&v);
                        items.sort_by(|a, b| b.cmp(a));
                        Value::List(items)
                    }
                };
                self.push(result);
                Ok(())
            }
            Expr::Set(s) => {
                let mut seen_keys: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                let mut items = Vec::new();
                for e in &s.elements {
                    self.execute_ast_expr(e)?;
                    let v = self.pop()?;
                    let key = v.to_str();
                    if seen_keys.insert(key) {
                        items.push(v);
                    }
                }
                self.push(Value::List(items));
                Ok(())
            }
            Expr::SetComprehension(sc) => {
                self.execute_ast_expr(&sc.iterable)?;
                let iter_val = self.pop()?;
                let items = self.iterable_to_list(&iter_val);
                let mut result = Vec::new();
                let saved_globals = self.globals.clone();
                for item in items {
                    self.globals.insert(sc.var.name.clone(), item);
                    let include = if let Some(cond) = &sc.condition {
                        self.execute_ast_expr(cond)?;
                        self.pop()?.truthy()
                    } else {
                        true
                    };
                    if include {
                        self.execute_ast_expr(&sc.result_expr)?;
                        result.push(self.pop()?);
                    }
                }
                self.globals = saved_globals;
                self.push(Value::List(result));
                Ok(())
            }
            Expr::Lambda(l) => {
                // Lambdas are represented as Func values with a unique name
                // like "<lambda_0>", "<lambda_1>", etc. The corresponding
                // FnDef is stored in self.lambda_defs (keyed by name) so
                // call_function can find it.
                let lambda_name = format!("<lambda_{}>", self.lambda_defs.len());
                // Capture the currently-visible lexical environment so the
                // lambda can reference enclosing variables (including the
                // enclosing function's parameters/locals) even after that
                // function has returned. We snapshot every binding visible
                // through the scope stack + globals at creation time.
                let mut captures: HashMap<String, Value> = HashMap::new();
                for (k, v) in &self.globals {
                    captures.insert(k.clone(), v.clone());
                }
                for scope in &self.local_scopes {
                    for (k, v) in scope {
                        captures.insert(k.clone(), v.clone());
                    }
                }
                self.lambda_captures.insert(lambda_name.clone(), captures);
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
                    span: l.span.clone(),
                };
                self.lambda_defs.push(fn_def.clone());
                // Cache the lambda as an Rc so call_function can fetch it
                // via a cheap refcount bump. Lambdas never contain yield
                // (the body is a single expression), so they are not added
                // to gen_set.
                self.fn_cache
                    .insert(lambda_name.clone(), Rc::new(fn_def));
                self.push(Value::Func(lambda_name));
                Ok(())
            }
            Expr::Qualified(q) => {
                // Qualified names like module.symbol: resolve by walking
                // globals, treating each segment as a module/field access.
                let mut current = Value::Null;
                let mut first = true;
                for seg in &q.parts {
                    if first {
                        current = self.scope_get(&seg.name).unwrap_or(Value::Null);
                        first = false;
                    } else {
                        let next: Value = match &current {
                            Value::Module(_, exports) => {
                                exports.get(&seg.name).cloned().unwrap_or(Value::Null)
                            }
                            Value::Object(_, fields) => {
                                fields.borrow().get(&seg.name).cloned().unwrap_or(Value::Null)
                            }
                            _ => Value::Null,
                        };
                        current = next;
                    }
                }
                self.push(current);
                Ok(())
            }
            _ => {
                self.push(Value::Null);
                Ok(())
            }
        }
    }

    /// Check if a name is a class defined in the program.
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
    fn execute_list_comprehension(&mut self, lc: &crate::parser::ast::ListComprehension) -> Result<()> {
        self.execute_ast_expr(&lc.iterable)?;
        let iterable = self.pop()?;
        let items = self.iterable_to_list(&iterable);
        let mut result = Vec::new();
        let saved_globals = self.globals.clone();
        for item in items {
            self.globals.insert(lc.var.name.clone(), item.clone());
            // Check condition if present.
            if let Some(cond) = &lc.condition {
                self.execute_ast_expr(cond)?;
                let c = self.pop()?;
                if !c.truthy() {
                    continue;
                }
            }
            self.execute_ast_expr(&lc.result_expr)?;
            result.push(self.pop()?);
        }
        self.globals = saved_globals;
        self.push(Value::List(result));
        Ok(())
    }

    /// Execute a dict comprehension: {k: v for var in iterable if cond}.
    fn execute_dict_comprehension(&mut self, dc: &crate::parser::ast::DictComprehension) -> Result<()> {
        self.execute_ast_expr(&dc.iterable)?;
        let iterable = self.pop()?;
        let items = self.iterable_to_list(&iterable);
        let mut result = HashMap::new();
        let saved_globals = self.globals.clone();
        for item in items {
            self.globals.insert(dc.var.name.clone(), item.clone());
            if let Some(cond) = &dc.condition {
                self.execute_ast_expr(cond)?;
                let c = self.pop()?;
                if !c.truthy() {
                    continue;
                }
            }
            self.execute_ast_expr(&dc.key_expr)?;
            let key = self.pop()?;
            self.execute_ast_expr(&dc.value_expr)?;
            let val = self.pop()?;
            result.insert(key.to_str(), val);
        }
        self.globals = saved_globals;
        self.push(Value::Dict(result));
        Ok(())
    }

    /// Convert any iterable value to a Vec of Values.
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
        match &receiver {
            Value::Object(class, _) => {
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
            _ => Err(CompilerError::runtime_error(format!(
                "cannot call method '{}' on {:?}",
                method, receiver
            ))),
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
            "upper" => Ok(Value::Str(s.to_uppercase())),
            "lower" => Ok(Value::Str(s.to_lowercase())),
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
            "starts_with" => {
                let needle = args.into_iter().next();
                let found = match needle {
                    Some(Value::Str(n)) => s.starts_with(&n),
                    _ => false,
                };
                Ok(Value::Bool(found))
            }
            "ends_with" => {
                let needle = args.into_iter().next();
                let found = match needle {
                    Some(Value::Str(n)) => s.ends_with(&n),
                    _ => false,
                };
                Ok(Value::Bool(found))
            }
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
            "push" => {
                // Mutations on a list value happen via globals; we return
                // a new list with the item appended (immutable semantics
                // since the VM uses globals, not references).
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
                    // Return the popped value; the new list is lost (caller
                    // should reassign if needed).
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
            "to_str" | "__str__" => Ok(Value::Str(format!("{:?}", l))),
            _ => Err(CompilerError::runtime_error(format!(
                "list has no method '{}'",
                method
            ))),
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
                    .and_then(|v| if let Value::Str(s) = v { Some(s) } else { None })
                    .unwrap_or_default();
                Ok(d.get(&key).cloned().unwrap_or(Value::Null))
            }
            "has" | "contains" => {
                let key = args
                    .into_iter()
                    .next()
                    .and_then(|v| if let Value::Str(s) = v { Some(s) } else { None })
                    .unwrap_or_default();
                Ok(Value::Bool(d.contains_key(&key)))
            }
            "keys" => Ok(Value::List(
                d.keys().map(|k| Value::Str(k.clone())).collect(),
            )),
            "values" => Ok(Value::List(d.values().cloned().collect())),
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
                    (Value::Int(a), Value::Int(b)) => Value::Int(a + b),
                    (Value::Float(a), Value::Float(b)) => Value::Float(a + b),
                    (Value::Int(a), Value::Float(b)) => Value::Float(*a as f64 + b),
                    (Value::Float(a), Value::Int(b)) => Value::Float(a + *b as f64),
                    (Value::Str(a), Value::Str(b)) => Value::Str(format!("{}{}", a, b)),
                    (Value::Str(a), _) => Value::Str(format!("{}{}", a, r.to_str())),
                    (_, Value::Str(b)) => Value::Str(format!("{}{}", l.to_str(), b)),
                    _ => Value::Null,
                }
            }
            BinaryOp::Sub => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Int(a - b),
                (Value::Float(a), Value::Float(b)) => Value::Float(a - b),
                (Value::Int(a), Value::Float(b)) => Value::Float(a as f64 - b),
                (Value::Float(a), Value::Int(b)) => Value::Float(a - b as f64),
                _ => Value::Null,
            },
            BinaryOp::Mul => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Int(a * b),
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
            BinaryOp::Div | BinaryOp::FloorDiv => match (l, r) {
                (Value::Int(a), Value::Int(b)) => {
                    if b == 0 { Value::Null } else { Value::Int(a / b) }
                }
                (Value::Float(a), Value::Float(b)) => Value::Float(a / b),
                (Value::Int(a), Value::Float(b)) => Value::Float(a as f64 / b),
                (Value::Float(a), Value::Int(b)) => Value::Float(a / b as f64),
                _ => Value::Null,
            },
            BinaryOp::Mod => match (l, r) {
                (Value::Int(a), Value::Int(b)) => {
                    if b == 0 { Value::Null } else { Value::Int(a % b) }
                }
                (Value::Float(a), Value::Float(b)) => Value::Float(a % b),
                (Value::Int(a), Value::Float(b)) => Value::Float(a as f64 % b),
                (Value::Float(a), Value::Int(b)) => Value::Float(a % b as f64),
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
        let fn_def = self.find_method(class, method)?.clone();
        // Push method context for super dispatch.
        self.method_context.push((class.to_string(), method.to_string()));
        // `self` is read directly from `globals` in several places (e.g.
        // MemberAccess on the `self` identifier, super dispatch), so it is
        // still save/restored in globals. Method parameters and any locals
        // introduced inside the body, however, live in a dedicated
        // call-frame scope so that re-entrant / recursive method calls do
        // not clobber each other's variables.
        let old_self = self.globals.insert("self".to_string(), receiver.clone());
        // Bind parameters into a new call-frame scope. If the method declares
        // an explicit `self` parameter (e.g. `fn, draw(self)`), skip it —
        // `self` is already bound above as a global, and treating it as a
        // local parameter would shadow the actual receiver. Callers never
        // pass `self` as an explicit argument, so the argument list maps 1:1
        // to the params after a leading `self` (if present).
        let skip_self = fn_def.params.first().map(|p| p.name.name == "self").unwrap_or(false);
        let params: Vec<(String, Value)> = if skip_self {
            fn_def.params.iter().skip(1).enumerate()
                .map(|(i, p)| (p.name.name.clone(), args.get(i).cloned().unwrap_or(Value::Null)))
                .collect()
        } else {
            fn_def.params.iter().enumerate()
                .map(|(i, p)| (p.name.name.clone(), args.get(i).cloned().unwrap_or(Value::Null)))
                .collect()
        };
        self.push_scope(params);
        let result = self.execute_fn_body(&fn_def.body);
        // Pop the call-frame scope (locals are discarded) and the method
        // context, then restore the previous `self` binding.
        self.pop_scope();
        self.method_context.pop();
        match old_self {
            Some(v) => { self.globals.insert("self".to_string(), v); }
            None => { self.globals.remove("self"); }
        }
        match result {
            Ok(()) => Ok(Value::Null),
            Err(e) => {
                let msg = e.message();
                if msg == "return" {
                    // Return value is on the stack.
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
                    if real_idx < 0 || real_idx >= len {
                        return Err(CompilerError::runtime_error(format!(
                            "list index assignment out of bounds: {} (len {})",
                            i, len
                        )));
                    }
                    l[real_idx as usize] = val;
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
        match name {
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
                Ok(Value::Int(v.to_int().unwrap_or(0)))
            }
            "float" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                Ok(Value::Float(v.to_float().unwrap_or(0.0)))
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
                    Value::Exception(_) => "exception",
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
                let content = match content {
                    Value::Str(s) => s,
                    _ => content.to_str(),
                };
                match fs::write(&path, &content) {
                    Ok(()) => Ok(Value::Bool(true)),
                    Err(_) => Ok(Value::Bool(false)),
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
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                Ok(Value::Float(secs))
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
            "stop" => Ok(Value::Exception("StopIteration".to_string())),
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
                // (propagate the error). Otherwise, pass through.
                let v = args.into_iter().next().unwrap_or(Value::Null);
                if let Value::Exception(msg) = &v {
                    Err(CompilerError::runtime_error(format!("throw:{}", msg)))
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
            "set" => {
                // set() returns empty dict (used as set); set(iterable) builds one.
                let mut d = HashMap::new();
                for arg in args {
                    for item in self.iterable_to_list(&arg) {
                        d.insert(item.to_str(), Value::Null);
                    }
                }
                Ok(Value::Dict(d))
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
        Value::Exception(_) => "exc",
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
        Value::Exception(_) => "exception".to_string(),
    }
}
