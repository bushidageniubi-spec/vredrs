//! Bytecode virtual machine module.
//!
//! This is the bytecode VM for Vredrs. It defines:
//! - `instr::Instr` — the 32+ opcode instruction set
//! - `instr::Module` — a compiled bytecode module (constant pool + code)
//! - `compiler::Compiler` — AST → bytecode compiler
//! - `vm::VM` — the stack-based execution engine
//!
//! ## Architecture
//!
//! The compiler lowers the AST to bytecode for the common cases (literals,
//! arithmetic, control flow, function calls, list/dict construction). For
//! constructs that don't have dedicated opcodes (slices, generators, with,
//! try/catch, iterators, comprehensions, lambdas, async/await, etc.), the
//! compiler emits `EvalAst(Expr)` or `ExecAstStmt(Stmt)` instructions that
//! delegate to the VM's AST interpreter path. This gives the VM full
//! coverage of the language without requiring opcodes for every construct.
//!
//! ## Supported language features
//!
//! - All arithmetic, comparison, logical, bitwise operators
//! - All control flow: if/elif/else, while, for-in, for-range, loop, break,
//!   continue, match
//! - Functions (recursion, multiple args, default values, generators with
//!   yield/resume, async/await — async runs synchronously in the VM)
//! - Classes (inheritance, methods, self, super, operator overloading via
//!   __add__/__sub__/__mul__/__getitem__/__setitem__/__enter__/__exit__)
//! - Lists, tuples, dicts, sets, strings (with full slice semantics)
//! - Comprehensions (list, dict, set)
//! - Exceptions (try/catch/finally/throw, with-statement)
//! - Modules (import with alias, symbol lists, builtin stdlib modules:
//!   math, io, os, time, fs, fmt, json, collections)
//! - Lambdas, ternary, null-coalesce, optional chain, pipe, postfix ops
//! - Annotations (`@route("/x")` reflected via `annotations(fn_name)`)
//! - All standard builtins (len, str, int, float, bool, type_of, range,
//!   enumerate, zip, sum, min, max, sorted, reversed, map, filter, dict,
//!   freeze, is_frozen, set_recursion_limit, etc.)
//! - File I/O (open, read, write, close, read_file, write_file,
//!   file_exists, read_dir, is_dir, is_file, path_join, basename, dirname)

pub mod compiler;
pub mod instr;
pub mod vm;

pub use compiler::Compiler;
pub use instr::{Instr, Module};
pub use vm::{VM, Value};
