//! Comprehensive LLVM IR backend for the Vredrs compiler (Phases 1-5).
//!
//! Implements: module imports, dynamic containers (list/dict/tuple/str/set),
//! class inheritance with dynamic fields, coroutines & exceptions, and
//! standard-library builtins — all lowered to calls against the C runtime
//! in `runtime/vredrs_runtime.c`.

use crate::error::{CompilerError, Result};
use crate::parser::ast::*;
use std::collections::HashMap;

// Free-standing helpers (JSON encoding, escape interpretation, float
// formatting) live in the `llvm::helpers` submodule.
use super::llvm::helpers::{
    encode_annotations_json, format_float, interpret_escapes, json_escape,
    parse_simple_json,
};

// Types are defined in backend::types and re-exported here for
// backward compatibility with code that imports from codegen::llvm_full.
pub use crate::backend::types::{annot_to_ty, class_of_annot, type_expr_to_ty, Sig, Ty, Val};

#[derive(Debug, Clone)]
pub struct FieldInfo {
    pub name: String,
    pub ty: Ty,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub name: String,
    pub index: usize,
    pub llvm_name: String,
    pub sig: Sig,
    pub fn_def: FnDef,
    pub inherited: bool,
}

#[derive(Debug, Clone)]
pub struct ClassInfo {
    pub name: String,
    pub parent: Option<String>,
    pub fields: Vec<FieldInfo>,
    pub methods: Vec<MethodInfo>,
    pub method_idx: HashMap<String, usize>,
}

#[derive(Debug, Clone)]
pub struct LocalSlot {
    pub ty: Ty,
    pub ptr: String,
    pub class: Option<String>,
    pub async_fn: Option<String>,
    /// If this slot holds a closure, the signature of that closure. Set when
    /// the slot is initialized from a `fn(...)...` lambda expression so that
    /// indirect call sites can emit the right LLVM function type.
    pub closure_sig: Option<Sig>,
}

/// A lambda lifted to a top-level function. We collect these during codegen
/// and emit their bodies after all top-level functions are emitted.
#[derive(Debug, Clone)]
pub struct LiftedLambda {
    /// LLVM symbol name, e.g. `__lambda_0`.
    pub name: String,
    /// Parameter types in order (env ptr appended at the end is implicit).
    pub param_types: Vec<Ty>,
    /// Inferred return type.
    pub ret_ty: Ty,
    /// Parameter names + bodies from the AST.
    pub params: Vec<FnParam>,
    pub body: Vec<Stmt>,
    /// Single-expression body (if the lambda was `fn(args) expr`). If non-empty
    /// this is preferred over `body`.
    pub expr_body: Option<Expr>,
}

#[derive(Debug, Clone)]
struct LoopCtx {
    cont_lbl: usize,
    brk_lbl: usize,
}

pub struct FullLlvmGen {
    buf: String,
    var_n: usize,
    lbl_n: usize,
    local_n: usize,
    str_n: usize,
    strs: HashMap<String, String>,
    functions: HashMap<String, Sig>,
    classes: HashMap<String, ClassInfo>,
    loop_stack: Vec<LoopCtx>,
    builtins: std::collections::HashSet<&'static str>,
    /// Names of imports — used to emit `declare` for external symbols.
    pub imports: Vec<ImportStmt>,
    /// Function signatures from other modules (cross-module calls).
    external_sigs: HashMap<String, Sig>,
    /// External class definitions: name -> (parent, method names in vtable order).
    /// Used so the main module can construct objects of classes defined in
    /// imported modules.
    external_classes: HashMap<String, (Option<String>, Vec<(String, Sig)>)>,
    /// If false, this module is a library (imported by another module) and
    /// should NOT emit a `main` function.
    pub emit_main: bool,
    /// Lambda expressions encountered during codegen. These are lifted to
    /// top-level functions and emitted after all user functions.
    lambdas: Vec<LiftedLambda>,
    /// Monotonic counter for generating unique lambda symbols.
    lambda_n: usize,
    /// Function annotations collected during `collect_function_sigs`. The key
    /// is the function name; the value is a JSON-encoded string of the
    /// annotation list (used by the `annotations()` builtin).
    fn_annotations: HashMap<String, String>,
    /// Emitted lambda function bodies, accumulated during codegen and
    /// appended to the module buffer at the end of `generate()`.
    lambda_bodies: Vec<String>,
    /// Closure signatures keyed by SSA register name. Populated when a
    /// `gen_lambda` call returns a Ty::Fn value, so indirect call sites can
    /// look up the param/return types.
    closure_sigs: HashMap<String, Sig>,
    /// Capture lists keyed by lifted-lambda name (e.g. `__lambda_0`). Each
    /// entry is the ordered list of free-variable names that the lambda
    /// captures from its enclosing scope. The env pointer passed to the
    /// lambda points at an array of `%vredrs.value` slots in this order.
    lambda_captures: HashMap<String, Vec<String>>,
}


include!("llvm/parts/part1.rs");
include!("llvm/parts/part2.rs");
include!("llvm/parts/part3.rs");
include!("llvm/parts/part4.rs");
include!("llvm/parts/part5.rs");
include!("llvm/parts/part6.rs");
include!("llvm/parts/part7.rs");
include!("llvm/parts/part8.rs");
include!("llvm/parts/part9.rs");
include!("llvm/parts/part10.rs");
