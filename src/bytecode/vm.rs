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
        // Order by discriminant tag first, then by content for primitives.
        use std::cmp::Ordering;
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
}

pub struct VM {
    module: Module,
    stack: Vec<Value>,
    frames: Vec<Frame>,
    globals: HashMap<String, Value>,
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
    /// Annotation table: fn_name → dict of annotations.
    annotation_table: HashMap<String, HashMap<String, Value>>,
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
            }],
            globals: HashMap::new(),
            pc: 0,
            iterators: Vec::new(),
            program: None,
            compiled_fns: HashMap::new(),
            base_dir: std::path::PathBuf::from("."),
            module_cache: HashMap::new(),
            frozen_set: std::collections::HashSet::new(),
            lambda_defs: Vec::new(),
            annotation_table: HashMap::new(),
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

    /// Run the bytecode module and return the final value on the stack.
    pub fn run(&mut self) -> Result<Value> {
        // Pre-process top-level declarations: imports and class definitions.
        self.preprocess_top_level()?;
        while self.pc < self.module.code.len() {
            let instr = self.module.code[self.pc].clone();
            self.pc += 1;
            match self.execute(&instr) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Return(v)) => return Ok(v),
                Err(e) => return Err(e),
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
                    _ => {}
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
                    .cloned()
                    .unwrap_or_default();
                let v = self.globals.get(&name).cloned().unwrap_or(Value::Null);
                self.push(v);
            }
            Instr::StoreGlobal(idx) => {
                let name = self
                    .module
                    .constants
                    .get(*idx)
                    .cloned()
                    .unwrap_or_default();
                let v = self.pop()?;
                self.globals.insert(name, v);
            }
            Instr::LoadSelf => {
                // Push the current `self` global (bound by call_method_on_class).
                let v = self.globals.get("self").cloned().unwrap_or(Value::Null);
                self.push(v);
            }
            Instr::LoadField(_) | Instr::StoreField(_) => {
                // Field load/store opcodes are unused — object field access
                // is handled via EvalAst→MemberAccess and the shared
                // Rc<RefCell<HashMap>> field map on Value::Object.
                self.pop()?;
                self.push(Value::Null);
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
                    _ => Value::Null,
                };
                self.push(result);
            }
            Instr::Mod => self.binop(|a, b| match (a, b) {
                (Value::Int(x), Value::Int(y)) => {
                    if y == 0 {
                        Value::Null
                    } else {
                        Value::Int(x % y)
                    }
                }
                _ => Value::Null,
            })?,
            Instr::Eq => self.binop(|a, b| Value::Bool(a == b))?,
            Instr::Ne => self.binop(|a, b| Value::Bool(a != b))?,
            Instr::Lt => self.binop(|a, b| match (a, b) {
                (Value::Int(x), Value::Int(y)) => Value::Bool(x < y),
                (Value::Float(x), Value::Float(y)) => Value::Bool(x < y),
                (Value::Str(x), Value::Str(y)) => Value::Bool(x < y),
                _ => Value::Null,
            })?,
            Instr::Gt => self.binop(|a, b| match (a, b) {
                (Value::Int(x), Value::Int(y)) => Value::Bool(x > y),
                (Value::Float(x), Value::Float(y)) => Value::Bool(x > y),
                (Value::Str(x), Value::Str(y)) => Value::Bool(x > y),
                _ => Value::Null,
            })?,
            Instr::Le => self.binop(|a, b| match (a, b) {
                (Value::Int(x), Value::Int(y)) => Value::Bool(x <= y),
                (Value::Float(x), Value::Float(y)) => Value::Bool(x <= y),
                (Value::Str(x), Value::Str(y)) => Value::Bool(x <= y),
                _ => Value::Null,
            })?,
            Instr::Ge => self.binop(|a, b| match (a, b) {
                (Value::Int(x), Value::Int(y)) => Value::Bool(x >= y),
                (Value::Float(x), Value::Float(y)) => Value::Bool(x >= y),
                (Value::Str(x), Value::Str(y)) => Value::Bool(x >= y),
                _ => Value::Null,
            })?,
            Instr::Neg => {
                let v = self.pop()?;
                let result = match v {
                    Value::Int(i) => Value::Int(-i),
                    Value::Float(f) => Value::Float(-f),
                    _ => Value::Null,
                };
                self.push(result);
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
            Instr::PushHandler(_) | Instr::PopHandler | Instr::Throw => {
                // Unused — try/catch is handled via ExecAstStmt fallback.
                if matches!(instr, Instr::Throw) {
                    let v = self.pop()?;
                    return Err(CompilerError::runtime_error(format!(
                        "unhandled throw: {}",
                        v.to_str()
                    )));
                }
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
            for (i, p) in fn_def.params.iter().enumerate() {
                if i < args.len() {
                    sub_vm.globals.insert(p.name.name.clone(), args[i].clone());
                }
            }
            let mut returned = Value::Null;
            let result = sub_vm.execute_fn_body(&fn_def.body);
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
        // Check for builtins (file_exists, read_file, read_dir, etc.).
        // This handles the case where a stdlib module (e.g. fs) re-exports
        // a builtin name as a Func value.
        if super::compiler::is_builtin(name) {
            let v = self.call_builtin_value(name, args)?;
            self.push(v);
            return Ok(());
        }
        // Look up the function in the AST program (or lambda table).
        let fn_def = match self.find_function(name) {
            Ok(f) => f.clone(),
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
                // Not in this program — check if it's an imported function
                // stored as a Module in globals.
                return self.call_imported_function(name, args);
            }
        };
        // Async functions are executed synchronously in the single-threaded VM.
        // The `is_async` flag is ignored; `await X` evaluates X inline.
        // Check for generator: if the function body contains yield, run it
        // eagerly to collect all yielded values into a list, then wrap
        // that list in a Generator value. This is the same eager-evaluation
        // model the AST interpreter uses.
        if self.fn_has_yield(&fn_def.body) {
            let mut yielded = Vec::new();
            // Save globals, bind params.
            let saved_globals = self.globals.clone();
            for (i, p) in fn_def.params.iter().enumerate() {
                if i < args.len() {
                    self.globals.insert(p.name.name.clone(), args[i].clone());
                }
            }
            // Execute the body, catching "yield" signals.
            let mut return_value: Option<Value> = None;
            for s in &fn_def.body {
                match self.execute_ast_stmt(s) {
                    Ok(()) => {}
                    Err(e) => {
                        let msg = e.message().to_string();
                        if msg == "yield" {
                            // The yielded value is on the stack.
                            let v = self.stack.pop().unwrap_or(Value::Null);
                            yielded.push(v);
                        } else if msg == "return" {
                            // Return ends the generator; capture the value.
                            return_value = Some(self.stack.pop().unwrap_or(Value::Null));
                            break;
                        } else {
                            self.globals = saved_globals;
                            return Err(e);
                        }
                    }
                }
            }
            self.globals = saved_globals;
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
        let saved_globals = self.globals.clone();
        for (i, p) in fn_def.params.iter().enumerate() {
            if i < args.len() {
                self.globals.insert(p.name.name.clone(), args[i].clone());
            }
        }
        let result = self.execute_fn_body(&fn_def.body);
        self.globals = saved_globals;
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
        for s in body {
            self.execute_ast_stmt(s)?;
        }
        Ok(())
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
                Err(CompilerError::runtime_error("return"))
            }
            Stmt::Assign(a) => {
                // Handle `del, target` (AssignOp::Delete) specially.
                if matches!(a.operator, crate::parser::ast::AssignOp::Delete) {
                    if let Some(target) = a.targets.first() {
                        use crate::parser::ast::Assignee;
                        match target {
                            Assignee::Identifier(id) => {
                                self.globals.remove(&id.name);
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
                            }
                            _ => {}
                        }
                    }
                    return Ok(());
                }
                self.execute_ast_expr(&a.value)?;
                if let Some(target) = a.targets.first() {
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
            Stmt::Break(_) => Err(CompilerError::runtime_error("break")),
            Stmt::Continue(_) => Err(CompilerError::runtime_error("continue")),
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
                if let Some(v) = &y.value {
                    self.execute_ast_expr(v)?;
                } else {
                    self.push(Value::Null);
                }
                Err(CompilerError::runtime_error("yield"))
            }
            Stmt::With(w) => self.execute_ast_with(w),
            Stmt::Match(m) => self.execute_ast_match(m),
            Stmt::Defer(d) => {
                // Defer runs at scope exit; in the VM we run it immediately
                // (simplification) to ensure side effects happen.
                self.execute_ast_stmt(&d.stmt)?;
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
                            self.globals.insert(cv.name.clone(), Value::Exception(exc_msg));
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
        let entered = self.call_dunder(&manager, "__enter__", vec![])?;
        if let Some(var) = &w.var {
            self.globals.insert(var.name.clone(), entered);
        }
        let body_result = self.execute_block(&w.body);
        // Call __exit__ on the manager.
        let _ = self.call_dunder(&manager, "__exit__", vec![Value::Null]);
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
                Some(exports)
            }
            "time" => {
                exports.insert("sleep".to_string(), Value::Func("time_sleep".to_string()));
                exports.insert("now".to_string(), Value::Func("time_now".to_string()));
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
            // fmt, json, collections, net, os, time are provided by .veds
            // files in src/runtime/std/. They fall through to the file-based
            // loader below.
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
            for s in &w.body {
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
                        return Err(e);
                    }
                }
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
                    self.globals.insert(f.var.name.clone(), v);
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
                                return Err(e);
                            }
                        }
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
            // Store the loop variable in globals (consistent with LoadGlobal).
            self.globals.insert(f.var.name.clone(), item);
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
                        return Err(e);
                    }
                }
            }
        }
        Ok(())
    }

    fn execute_ast_for_range(&mut self, f: &crate::parser::ast::ForRangeStmt) -> Result<()> {
        self.execute_ast_expr(&f.from)?;
        let start = self.pop()?.to_int()?;
        self.execute_ast_expr(&f.to)?;
        let end = self.pop()?.to_int()?;
        let step: i64 = 1; // ForRangeStmt has no step field in 0.1.1
        let mut i = start;
        while i < end {
            self.globals
                .insert(f.var.name.clone(), Value::Int(i));
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
                        return Err(e);
                    }
                }
            }
            i += step;
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
                    self.globals.insert(id.name.clone(), v);
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
                    if let Some(Value::Object(class, fields)) = self.globals.get(&id.name).cloned() {
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
                let v = self.globals.get(&id.name).cloned().unwrap_or(Value::Null);
                self.push(v);
                Ok(())
            }
            Expr::Binary(b) => {
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
                    // super.method(args): dispatch to parent class method.
                    if let Expr::Identifier(id) = m.target.as_ref() {
                        if id.name == "super" {
                            // Find the current class (the class of `self`).
                            if let Some(Value::Object(class, _)) = self.globals.get("self") {
                                let class = class.clone();
                                // Find the parent class.
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
                let mut items = Vec::new();
                let mut i = start;
                while i < end {
                    items.push(Value::Int(i));
                    i += 1;
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
                // Casts are no-ops in the dynamically-typed VM.
                self.execute_ast_expr(&c.expr)?;
                Ok(())
            }
            Expr::TryPropagate(t) => {
                self.execute_ast_expr(&t.expr)?;
                Ok(())
            }
            Expr::Spread(s) => {
                // Spread is contextual; in isolation, evaluate the inner expr.
                self.execute_ast_expr(&s.expr)?;
                Ok(())
            }
            Expr::Pipe(p) => {
                // x |> f  ==>  f(x)
                self.execute_ast_expr(&p.left)?;
                let x = self.pop()?;
                self.execute_ast_expr(&p.right)?;
                let f = self.pop()?;
                let v = self.call_value(&f, vec![x])?;
                self.push(v);
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
                    span: l.span.clone(),
                };
                self.lambda_defs.push(fn_def);
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
                        current = self.globals.get(&seg.name).cloned().unwrap_or(Value::Null);
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

    /// Instantiate a class: create object with fields, call new() if present.
    fn instantiate_class(&mut self, class: &str, args: Vec<Value>) -> Result<Value> {
        let mut fields = HashMap::new();
        self.collect_class_fields(class, &mut fields);
        let obj = Value::Object(class.to_string(), Rc::new(RefCell::new(fields)));
        if self.find_method(class, "new").is_ok() {
            let returned = self.call_method_on_class(class, obj.clone(), "new", args)?;
            if let Value::Object(_, _) = returned {
                return Ok(returned);
            }
        }
        Ok(obj)
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

    fn ast_binary(&self, l: Value, op: &BinaryOp, r: Value) -> Result<Value> {
        let v = match op {
            BinaryOp::Add => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Int(a + b),
                (Value::Float(a), Value::Float(b)) => Value::Float(a + b),
                (Value::Int(a), Value::Float(b)) => Value::Float(a as f64 + b),
                (Value::Float(a), Value::Int(b)) => Value::Float(a + b as f64),
                (Value::Str(a), Value::Str(b)) => Value::Str(format!("{}{}", a, b)),
                _ => Value::Null,
            },
            BinaryOp::Sub => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Int(a - b),
                (Value::Float(a), Value::Float(b)) => Value::Float(a - b),
                _ => Value::Null,
            },
            BinaryOp::Mul => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Int(a * b),
                (Value::Float(a), Value::Float(b)) => Value::Float(a * b),
                _ => Value::Null,
            },
            BinaryOp::Div | BinaryOp::FloorDiv => match (l, r) {
                (Value::Int(a), Value::Int(b)) => {
                    if b == 0 {
                        Value::Null
                    } else {
                        Value::Int(a / b)
                    }
                }
                (Value::Float(a), Value::Float(b)) => Value::Float(a / b),
                _ => Value::Null,
            },
            BinaryOp::Mod => match (l, r) {
                (Value::Int(a), Value::Int(b)) => {
                    if b == 0 {
                        Value::Null
                    } else {
                        Value::Int(a % b)
                    }
                }
                _ => Value::Null,
            },
            BinaryOp::Eq => Value::Bool(l == r),
            BinaryOp::Ne => Value::Bool(l != r),
            BinaryOp::Lt => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Bool(a < b),
                (Value::Float(a), Value::Float(b)) => Value::Bool(a < b),
                (Value::Str(a), Value::Str(b)) => Value::Bool(a < b),
                _ => Value::Null,
            },
            BinaryOp::Gt => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Bool(a > b),
                (Value::Float(a), Value::Float(b)) => Value::Bool(a > b),
                (Value::Str(a), Value::Str(b)) => Value::Bool(a > b),
                _ => Value::Null,
            },
            BinaryOp::Le => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Bool(a <= b),
                (Value::Float(a), Value::Float(b)) => Value::Bool(a <= b),
                (Value::Str(a), Value::Str(b)) => Value::Bool(a <= b),
                _ => Value::Null,
            },
            BinaryOp::Ge => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Value::Bool(a >= b),
                (Value::Float(a), Value::Float(b)) => Value::Bool(a >= b),
                (Value::Str(a), Value::Str(b)) => Value::Bool(a >= b),
                _ => Value::Null,
            },
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
        // Save globals, bind self + params.
        let saved_globals = self.globals.clone();
        // IMPORTANT: bind self to a COPY of receiver so that field mutations
        // via `self.field = value` persist on the receiver object.
        self.globals.insert("self".to_string(), receiver.clone());
        for (i, p) in fn_def.params.iter().enumerate() {
            if i < args.len() {
                self.globals.insert(p.name.name.clone(), args[i].clone());
            }
        }
        let result = self.execute_fn_body(&fn_def.body);
        // Mutations to `self.field` inside the method body persist via the
        // shared Rc<RefCell<HashMap>> on the original Object value, so we
        // don't need to capture and return a mutated self here.
        self.globals = saved_globals;
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
                let i = *i as usize;
                if i < t.len() {
                    Ok(t[i].clone())
                } else {
                    Err(CompilerError::runtime_error("tuple index out of bounds"))
                }
            }
            (Value::Str(s), Value::Int(i)) => {
                let chars: Vec<char> = s.chars().collect();
                let i = *i as usize;
                if i < chars.len() {
                    Ok(Value::Str(chars[i].to_string()))
                } else {
                    Err(CompilerError::runtime_error("string index out of bounds"))
                }
            }
            (Value::Dict(d), Value::Str(k)) => Ok(d.get(k).cloned().unwrap_or(Value::Null)),
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
                    let i = *i as usize;
                    if i < l.len() {
                        l[i] = val;
                    } else {
                        l.push(val);
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
                    let i = *i as usize;
                    if i < l.len() {
                        l.remove(i);
                    }
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
                let mut items = Vec::new();
                let mut i = start;
                while i < end {
                    items.push(Value::Int(i));
                    i += step;
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
                self.frozen_set.insert(v.to_str());
                Ok(v)
            }
            "is_frozen" => {
                let v = args.into_iter().next().unwrap_or(Value::Null);
                Ok(Value::Bool(self.frozen_set.contains(&v.to_str())))
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
                // Fall back to math_* family if the name matches.
                if name.starts_with("math_") {
                    return self.call_math_function(name, args);
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
