//! Vredrs interpreter — executes Vredrs programs directly from the AST.
//!
//! This module is the development-mode backend (`vredrs run`). For native
//! compilation, use `vredrs build` which goes through the LLVM backend.

pub mod value;
pub mod builtins;
pub mod module;

pub use value::*;
pub use value::{Value, RuntimeMethod, RuntimeException, RuntimeIterator, RuntimeChannel, CoroutineTask, CoroutineState, RuntimeFile, ExecFlow, NativeFn, interp_truthy, interp_fmt};

use crate::error::{CompilerError, Result};
use crate::parser::ast::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Interpreter {
    pub(crate) vars: Vec<HashMap<String, Value>>,
    pub(crate) builtins: HashSet<String>,
    pub(crate) exported: HashSet<String>,
    pub(crate) base_dir: PathBuf,
    pub(crate) module_cache: HashMap<String, HashMap<String, Value>>,
    pub(crate) loading_stack: Vec<String>,
    pub(crate) struct_fields: HashMap<String, Vec<String>>,
    pub(crate) class_fields: HashMap<String, Vec<(String, Option<Expr>)>>,
    pub(crate) class_extends: HashMap<String, Option<String>>,
    pub(crate) class_methods: HashMap<String, HashMap<String, Vec<RuntimeMethod>>>,
    pub(crate) channels: Vec<RuntimeChannel>,
    pub(crate) tasks: HashMap<usize, CoroutineTask>,
    pub(crate) ready_queue: VecDeque<usize>,
    pub(crate) next_coro_id: usize,
    pub(crate) current_coro: Option<usize>,
    pub(crate) files: Vec<RuntimeFile>,
    pub(crate) defer_stack: Vec<Vec<Stmt>>,
    pub(crate) call_stack: Vec<String>,
    pub(crate) depth: usize,
    pub(crate) max_depth: usize,
    pub(crate) annotation_meta: HashMap<String, Vec<Annotation>>,
}

impl Interpreter {
    pub fn new() -> Self {
        let base_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::with_base_dir(base_dir)
    }

    pub fn with_base_dir(base_dir: PathBuf) -> Self {
        let mut interp = Interpreter {
            vars: vec![HashMap::new()],
            builtins: HashSet::new(),
            exported: HashSet::new(),
            base_dir,
            module_cache: HashMap::new(),
            loading_stack: vec![],
            struct_fields: HashMap::new(),
            class_fields: HashMap::new(),
            class_extends: HashMap::new(),
            class_methods: HashMap::new(),
            channels: vec![],
            tasks: HashMap::new(),
            ready_queue: VecDeque::new(),
            next_coro_id: 0,
            current_coro: None,
            files: vec![],
            defer_stack: vec![],
            call_stack: vec![],
            depth: 0,
            max_depth: 4096,
            annotation_meta: HashMap::new(),
        };
        interp.install_builtins();
        interp
    }



    fn global(&mut self, name: &str, value: Value) {
        if let Some(scope) = self.vars.first_mut() {
            scope.insert(name.to_string(), value);
        }
    }

    pub fn get(&self, name: &str) -> Value {
        match self.lookup(name) {
            Some(v) => v,
            None => Value::Null,
        }
    }

    fn lookup(&self, name: &str) -> Option<Value> {
        for scope in self.vars.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v.clone());
            }
        }
        None
    }

    fn require_var(&self, name: &str) -> Result<Value> {
        self.lookup(name)
            .ok_or_else(|| self.runtime_error(format!("undefined symbol '{}'", name)))
    }

    fn set(&mut self, name: &str, value: Value) {
        for scope in self.vars.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), value);
                return;
            }
        }
        if let Some(scope) = self.vars.last_mut() {
            scope.insert(name.to_string(), value);
        }
    }

    fn define_local(&mut self, name: &str, value: Value) {
        if let Some(scope) = self.vars.last_mut() {
            scope.insert(name.to_string(), value);
        }
    }

    fn push(&mut self) {
        self.vars.push(HashMap::new());
    }
    fn pop(&mut self) {
        if self.vars.len() > 1 {
            self.vars.pop();
        }
    }

    pub fn run(&mut self, program: &Program) -> Result<()> {
        for d in &program.declarations {
            self.collect_top_level(d)?;
        }
        for d in &program.declarations {
            if matches!(
                d,
                TopLevel::FnDef(_)
                    | TopLevel::StructDef(_)
                    | TopLevel::ClassDef(_)
                    | TopLevel::EnumDef(_)
                    | TopLevel::InterfaceDef(_)
            ) {
                continue;
            }
            let flow = self.top(d)?;
            match flow {
                ExecFlow::None | ExecFlow::Value(_) => {}
                ExecFlow::Return(_) => return Err(self.runtime_error("return outside function")),
                ExecFlow::Break => return Err(self.runtime_error("break outside loop")),
                ExecFlow::Continue => return Err(self.runtime_error("continue outside loop")),
                ExecFlow::Yield(_) => self.run_ready_coroutines()?,
            }
        }
        self.run_ready_coroutines()?;
        Ok(())
    }

    fn collect_top_level(&mut self, tl: &TopLevel) -> Result<()> {
        match tl {
            TopLevel::FnDef(fd) => {
                self.record_annotations(&fd.name.name, &fd.annotations);
                let ps = Self::params_from(&fd.params);
                let value = if fd.is_async {
                    Value::AsyncFunction(
                        fd.name.name.clone(),
                        ps,
                        fd.body.clone(),
                        vec![self.current_capture()],
                    )
                } else {
                    Value::Function(
                        fd.name.name.clone(),
                        ps,
                        fd.body.clone(),
                        vec![self.current_capture()],
                    )
                };
                self.global(&fd.name.name, value);
            }
            TopLevel::LazyFnDef(lfd) => {
                let fd = &lfd.fn_def;
                self.record_annotations(&fd.name.name, &fd.annotations);
                let ps = Self::params_from(&fd.params);
                let value = if fd.is_async {
                    Value::AsyncFunction(
                        fd.name.name.clone(),
                        ps,
                        fd.body.clone(),
                        vec![self.current_capture()],
                    )
                } else {
                    Value::Function(
                        fd.name.name.clone(),
                        ps,
                        fd.body.clone(),
                        vec![self.current_capture()],
                    )
                };
                self.global(&fd.name.name, value);
            }
            TopLevel::StructDef(sd) => {
                self.record_annotations(&sd.name.name, &sd.annotations);
                let fields: Vec<String> = sd.fields.iter().map(|f| f.name.name.clone()).collect();
                self.struct_fields.insert(sd.name.name.clone(), fields);
                self.global(&sd.name.name, Value::Class(sd.name.name.clone()));
                for method in &sd.methods {
                    self.register_method(&sd.name.name, method);
                }
            }
            TopLevel::ClassDef(cd) => {
                self.record_annotations(&cd.name.name, &cd.annotations);
                let parent = cd.extends.as_ref().and_then(Self::type_name);
                self.class_extends.insert(cd.name.name.clone(), parent);
                let fields: Vec<(String, Option<Expr>)> = cd
                    .fields
                    .iter()
                    .map(|f| (f.name.name.clone(), f.default_value.clone()))
                    .collect();
                self.class_fields.insert(cd.name.name.clone(), fields);
                self.global(&cd.name.name, Value::Class(cd.name.name.clone()));
                for method in &cd.methods {
                    self.register_method(&cd.name.name, method);
                }
            }
            TopLevel::EnumDef(ed) => {
                self.record_annotations(&ed.name.name, &ed.annotations);
                let ename = ed.name.name.clone();
                self.global(&ename, Value::Class(ename.clone()));
                for variant in &ed.variants {
                    let full = format!("{}::{}", ename, variant.name.name);
                    let full_clone = full.clone();
                    self.global(&full, Value::Struct(full_clone, HashMap::new()));
                }
            }
            TopLevel::InterfaceDef(id) => {
                self.global(&id.name.name, Value::Class(id.name.name.clone()))
            }
            _ => {}
        }
        Ok(())
    }

    fn register_method(&mut self, owner: &str, fd: &FnDef) {
        self.record_annotations(&format!("{}.{}", owner, fd.name.name), &fd.annotations);
        let method = RuntimeMethod {
            name: fd.name.name.clone(),
            params: Self::params_from(&fd.params),
            body: fd.body.clone(),
            owner: owner.to_string(),
        };
        self.class_methods
            .entry(owner.to_string())
            .or_default()
            .entry(method.name.clone())
            .or_default()
            .push(method);
    }

    fn record_annotations(&mut self, name: &str, annotations: &[Annotation]) {
        if !annotations.is_empty() {
            self.annotation_meta
                .insert(name.to_string(), annotations.to_vec());
        }
    }

    fn params_from(params: &[FnParam]) -> Vec<(String, bool)> {
        params
            .iter()
            .map(|p| (p.name.name.clone(), p.is_variadic))
            .collect()
    }

    fn type_name(ty: &TypeExpr) -> Option<String> {
        match ty {
            TypeExpr::Named(id, _) => Some(id.name.clone()),
            TypeExpr::Basic(_, _) => None,
            TypeExpr::Generic { base, .. } => Self::type_name(base),
            TypeExpr::Optional(inner, _) => Self::type_name(inner),
            _ => None,
        }
    }

    fn current_capture(&self) -> HashMap<String, Value> {
        let mut out = HashMap::new();
        for scope in &self.vars {
            for (k, v) in scope {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }

    fn top(&mut self, tl: &TopLevel) -> Result<ExecFlow> {
        match tl {
            TopLevel::Import(imp) => {
                self.import_module(imp)?;
                Ok(ExecFlow::None)
            }
            TopLevel::Export(exp) => {
                for s in &exp.symbols {
                    self.exported.insert(s.name.clone());
                }
                Ok(ExecFlow::None)
            }
            TopLevel::ConstExpr(ce) => {
                let v = self.expr(&ce.value)?;
                self.define_local(&ce.name.name, v);
                Ok(ExecFlow::None)
            }
            TopLevel::LazyDef(ld) => {
                let v = self.expr(&ld.value)?;
                self.define_local(&ld.name.name, v);
                Ok(ExecFlow::None)
            }
            TopLevel::Statement(stmt) => self.stmt(stmt),
            TopLevel::ConditionalCompile(cc) => {
                if interp_truthy(&self.expr(&cc.condition)?) {
                    for d in &cc.then_body {
                        self.collect_top_level(d)?;
                    }
                    for d in &cc.then_body {
                        self.top(d)?;
                    }
                } else if let Some(body) = &cc.else_body {
                    for d in body {
                        self.collect_top_level(d)?;
                    }
                    for d in body {
                        self.top(d)?;
                    }
                }
                Ok(ExecFlow::None)
            }
            _ => Ok(ExecFlow::None),
        }
    }







    fn stmt(&mut self, stmt: &Stmt) -> Result<ExecFlow> {
        match stmt {
            Stmt::Assign(a) => self.assign_stmt(a),
            Stmt::Pon(p) => self.assign_stmt(&p.assign),
            Stmt::TableAssign(t) => self.table_assign(t),
            Stmt::Paste(p) => {
                self.print_args(&p.args, false)?;
                Ok(ExecFlow::None)
            }
            Stmt::Println(p) => {
                self.print_args(&p.args, true)?;
                Ok(ExecFlow::None)
            }
            Stmt::Flush(_) => {
                io::stdout()
                    .flush()
                    .map_err(|e| self.runtime_error(format!("flush failed: {}", e)))?;
                Ok(ExecFlow::None)
            }
            Stmt::Input(i) => self.input_stmt(i),
            Stmt::Return(r) => {
                let value = if r.values.is_empty() {
                    Value::Null
                } else if r.values.len() == 1 {
                    self.expr(&r.values[0])?
                } else {
                    Value::Tuple(self.eval_exprs(&r.values)?)
                };
                Ok(ExecFlow::Return(value))
            }
            Stmt::Throw(t) => {
                let v = self.expr(&t.value)?;
                Err(self.throw_value(v))
            }
            Stmt::Defer(d) => {
                if self.defer_stack.is_empty() {
                    self.defer_stack.push(vec![]);
                }
                if let Some(stack) = self.defer_stack.last_mut() {
                    stack.push((*d.stmt).clone());
                }
                Ok(ExecFlow::None)
            }
            Stmt::Assert(a) => {
                if !interp_truthy(&self.expr(&a.condition)?) {
                    return Err(self.runtime_error(
                        a.message
                            .clone()
                            .unwrap_or_else(|| "assertion failed".to_string()),
                    ));
                }
                Ok(ExecFlow::None)
            }
            Stmt::Panic(p) => {
                let pv = self.expr(&p.message)?;
                let msg = interp_fmt(&pv);
                Err(self.runtime_error(format!("panic: {}", msg)))
            }
            Stmt::Spawn(s) => {
                let handle = self.spawn_expr(&s.call)?;
                Ok(ExecFlow::Value(handle))
            }
            Stmt::SpawnThread(s) => {
                let _ = self.expr(&s.call)?;
                Ok(ExecFlow::None)
            }
            Stmt::Yield(y) => {
                let v = if let Some(expr) = &y.value {
                    self.expr(expr)?
                } else {
                    Value::Null
                };
                Ok(ExecFlow::Yield(v))
            }
            Stmt::If(i) => self.if_stmt(i),
            Stmt::While(w) => self.while_stmt(w),
            Stmt::ForIn(f) => self.for_in_stmt(f),
            Stmt::ForRange(f) => self.for_range_stmt(f),
            Stmt::Loop(l) => self.loop_stmt(l),
            Stmt::Break(_) => Ok(ExecFlow::Break),
            Stmt::Continue(_) => Ok(ExecFlow::Continue),
            Stmt::Match(m) => self.match_stmt(m),
            Stmt::Try(t) => self.try_stmt(t),
            Stmt::With(w) => self.with_stmt(w),
            Stmt::Select(s) => self.select_stmt(s),
            Stmt::UnsafeBlock(b) => self.run_block(&b.body),
            Stmt::DirectiveBlock(b) => self.run_block(&b.body),
            Stmt::ScopeBlock(b) => self.run_scope_block(b),
            Stmt::Expr(e) => self.expr(&e.expr).map(ExecFlow::Value),
            Stmt::Asm(_) => {
                Err(self.runtime_error("inline assembly is not executable by the interpreter"))
            }
        }
    }

    fn run_block(&mut self, body: &[Stmt]) -> Result<ExecFlow> {
        self.push();
        let result = self.run_block_no_scope(body);
        self.pop();
        result
    }

    fn run_block_no_scope(&mut self, body: &[Stmt]) -> Result<ExecFlow> {
        let mut last = ExecFlow::None;
        for stmt in body {
            let flow = self.stmt(stmt)?;
            match flow {
                ExecFlow::None => last = ExecFlow::None,
                ExecFlow::Value(v) => last = ExecFlow::Value(v),
                ExecFlow::Return(_) | ExecFlow::Break | ExecFlow::Continue | ExecFlow::Yield(_) => {
                    return Ok(flow)
                }
            }
        }
        Ok(last)
    }

    fn print_args(&mut self, args: &[Expr], newline: bool) -> Result<()> {
        let vals = self.eval_exprs(args)?;
        let text = vals.iter().map(interp_fmt).collect::<Vec<_>>().join("");
        if newline {
            println!("{}", text);
        } else {
            print!("{}", text);
        }
        Ok(())
    }

    fn eval_exprs(&mut self, exprs: &[Expr]) -> Result<Vec<Value>> {
        let mut vals = Vec::with_capacity(exprs.len());
        for e in exprs {
            vals.push(self.expr(e)?);
        }
        Ok(vals)
    }

    fn assign_stmt(&mut self, a: &AssignStmt) -> Result<ExecFlow> {
        if a.operator == AssignOp::Delete {
            for target in &a.targets {
                self.delete_target(target)?;
            }
            return Ok(ExecFlow::None);
        }
        let value = self.expr(&a.value)?;
        for target in &a.targets {
            self.assign_target(target, value.clone(), &a.operator)?;
        }
        Ok(ExecFlow::Value(value))
    }

    fn assign_target(&mut self, target: &Assignee, value: Value, op: &AssignOp) -> Result<()> {
        match target {
            Assignee::Identifier(id) => {
                let new_value = self.apply_assign_op(self.lookup(&id.name), value, op)?;
                self.set(&id.name, new_value);
            }
            Assignee::Qualified(q) => {
                let name = q
                    .parts
                    .iter()
                    .map(|p| p.name.clone())
                    .collect::<Vec<_>>()
                    .join("::");
                let new_value = self.apply_assign_op(self.lookup(&name), value, op)?;
                self.set(&name, new_value);
            }
            Assignee::Member(m) => {
                let current = if *op == AssignOp::Simple {
                    None
                } else {
                    Some(self.expr(&Expr::MemberAccess(m.clone()))?)
                };
                let new_value = self.apply_assign_op(current, value, op)?;
                self.assign_member(&m.target, &m.member.name, new_value)?;
            }
            Assignee::Index(i) => {
                let current = if *op == AssignOp::Simple {
                    None
                } else {
                    Some(self.expr(&Expr::Index(i.clone()))?)
                };
                let new_value = self.apply_assign_op(current, value, op)?;
                self.assign_index(&i.target, &i.index, new_value)?;
            }
            Assignee::Tuple(items) => {
                let values = match value {
                    Value::Tuple(v) | Value::List(v) => v,
                    other => {
                        return Err(self
                            .runtime_error(format!("cannot destructure {}", interp_fmt(&other))))
                    }
                };
                if items.len() != values.len() {
                    return Err(self.runtime_error("destructuring arity mismatch"));
                }
                for (sub, val) in items.iter().zip(values.into_iter()) {
                    self.assign_target(sub, val, &AssignOp::Simple)?;
                }
            }
        }
        Ok(())
    }

    fn apply_assign_op(
        &mut self,
        current: Option<Value>,
        value: Value,
        op: &AssignOp,
    ) -> Result<Value> {
        match op {
            AssignOp::Simple => Ok(value),
            AssignOp::Plus
            | AssignOp::Minus
            | AssignOp::Star
            | AssignOp::Slash
            | AssignOp::Percent => {
                let lhs = current.ok_or_else(|| {
                    self.runtime_error("compound assignment requires an existing value")
                })?;
                let bop = match op {
                    AssignOp::Plus => BinaryOp::Add,
                    AssignOp::Minus => BinaryOp::Sub,
                    AssignOp::Star => BinaryOp::Mul,
                    AssignOp::Slash => BinaryOp::Div,
                    AssignOp::Percent => BinaryOp::Mod,
                    AssignOp::Simple | AssignOp::Delete => unreachable!(),
                };
                self.binary_values(lhs, bop, value)
            }
            AssignOp::Delete => unreachable!(),
        }
    }

    fn assign_member(&mut self, target: &Expr, member: &str, value: Value) -> Result<()> {
        let obj = self.expr(target)?;
        if matches!(obj, Value::Frozen(_)) {
            return Err(self.runtime_error("cannot assign member on frozen value"));
        }
        if self.has_method(&obj, "__setattr__", 2) {
            let _ = self.call_method(
                obj,
                "__setattr__",
                vec![Value::Str(member.to_string()), value],
            )?;
            return Ok(());
        }
        match obj {
            Value::Object(_, fields) => {
                fields.borrow_mut().insert(member.to_string(), value);
                Ok(())
            }
            Value::Struct(name, mut fields) => {
                fields.insert(member.to_string(), value);
                if let Expr::Identifier(id) = target {
                    self.set(&id.name, Value::Struct(name, fields));
                }
                Ok(())
            }
            Value::Dict(mut d) => {
                d.insert(member.to_string(), value);
                if let Expr::Identifier(id) = target {
                    self.set(&id.name, Value::Dict(d));
                }
                Ok(())
            }
            Value::Frozen(_) => Err(self.runtime_error("cannot assign member on frozen value")),
            Value::Module(_, _) => Err(self.runtime_error("cannot assign into imported module")),
            other => Err(self.runtime_error(format!(
                "cannot assign member '{}' on {}",
                member,
                self.type_name_of(&other)
            ))),
        }
    }

    fn assign_index(&mut self, target: &Expr, index: &Expr, value: Value) -> Result<()> {
        let obj = self.expr(target)?;
        if matches!(obj, Value::Frozen(_)) {
            return Err(self.runtime_error("cannot index-assign into frozen value"));
        }
        let index_value = self.expr(index)?;
        if self.has_method(&obj, "__setitem__", 2) {
            let _ = self.call_method(obj, "__setitem__", vec![index_value, value])?;
            return Ok(());
        }
        match obj {
            Value::List(mut items) => {
                let idx = index_value.to_index()?;
                if idx >= items.len() {
                    return Err(self.runtime_error(format!("list index {} out of range", idx)));
                }
                items[idx] = value;
                if let Expr::Identifier(id) = target {
                    self.set(&id.name, Value::List(items));
                }
                Ok(())
            }
            Value::Dict(mut d) => {
                let key = self.value_to_key(&index_value);
                d.insert(key, value);
                if let Expr::Identifier(id) = target {
                    self.set(&id.name, Value::Dict(d));
                }
                Ok(())
            }
            Value::Object(_, fields) => {
                let key = self.value_to_key(&index_value);
                fields.borrow_mut().insert(key, value);
                Ok(())
            }
            other => Err(self.runtime_error(format!(
                "cannot index-assign into {}",
                self.type_name_of(&other)
            ))),
        }
    }

    fn delete_target(&mut self, target: &Assignee) -> Result<()> {
        match target {
            Assignee::Identifier(id) => {
                for scope in self.vars.iter_mut().rev() {
                    if scope.remove(&id.name).is_some() {
                        return Ok(());
                    }
                }
                Err(self.runtime_error(format!("cannot delete unknown variable '{}'", id.name)))
            }
            Assignee::Qualified(q) => {
                let name = q
                    .parts
                    .iter()
                    .map(|p| p.name.clone())
                    .collect::<Vec<_>>()
                    .join("::");
                for scope in self.vars.iter_mut().rev() {
                    if scope.remove(&name).is_some() {
                        return Ok(());
                    }
                }
                Err(self.runtime_error(format!("cannot delete unknown variable '{}'", name)))
            }
            Assignee::Member(m) => self.delete_member(&m.target, &m.member.name),
            Assignee::Index(i) => self.delete_index(&i.target, &i.index),
            Assignee::Tuple(items) => {
                for item in items {
                    self.delete_target(item)?;
                }
                Ok(())
            }
        }
    }

    fn delete_member(&mut self, target: &Expr, member: &str) -> Result<()> {
        let obj = self.expr(target)?;
        if matches!(obj, Value::Frozen(_)) {
            return Err(self.runtime_error("cannot delete member on frozen value"));
        }
        if self.has_method(&obj, "__delattr__", 1) {
            let _ = self.call_method(obj, "__delattr__", vec![Value::Str(member.to_string())])?;
            return Ok(());
        }
        match obj {
            Value::Object(_, fields) => {
                fields.borrow_mut().remove(member);
                Ok(())
            }
            Value::Struct(name, mut fields) => {
                fields.remove(member);
                if let Expr::Identifier(id) = target {
                    self.set(&id.name, Value::Struct(name, fields));
                }
                Ok(())
            }
            Value::Dict(mut d) => {
                d.remove(member);
                if let Expr::Identifier(id) = target {
                    self.set(&id.name, Value::Dict(d));
                }
                Ok(())
            }
            Value::Module(_, _) => Err(self.runtime_error("cannot delete from imported module")),
            other => Err(self.runtime_error(format!(
                "cannot delete member '{}' on {}",
                member,
                self.type_name_of(&other)
            ))),
        }
    }

    fn delete_index(&mut self, target: &Expr, index: &Expr) -> Result<()> {
        let obj = self.expr(target)?;
        if matches!(obj, Value::Frozen(_)) {
            return Err(self.runtime_error("cannot index-delete from frozen value"));
        }
        let index_value = self.expr(index)?;
        if self.has_method(&obj, "__delitem__", 1) {
            let _ = self.call_method(obj, "__delitem__", vec![index_value])?;
            return Ok(());
        }
        match obj {
            Value::List(mut items) => {
                let idx = index_value.to_index()?;
                if idx >= items.len() {
                    return Err(self.runtime_error(format!("list index {} out of range", idx)));
                }
                items.remove(idx);
                if let Expr::Identifier(id) = target {
                    self.set(&id.name, Value::List(items));
                }
                Ok(())
            }
            Value::Dict(mut d) => {
                let key = self.value_to_key(&index_value);
                d.remove(&key);
                if let Expr::Identifier(id) = target {
                    self.set(&id.name, Value::Dict(d));
                }
                Ok(())
            }
            Value::Object(_, fields) => {
                let key = self.value_to_key(&index_value);
                fields.borrow_mut().remove(&key);
                Ok(())
            }
            other => Err(self.runtime_error(format!(
                "cannot index-delete from {}",
                self.type_name_of(&other)
            ))),
        }
    }

    fn table_assign(&mut self, t: &TableAssignStmt) -> Result<ExecFlow> {
        for row in &t.rows {
            let col_names: Vec<String> = if t.column_names.is_empty() {
                row.iter()
                    .enumerate()
                    .map(|(i, _)| format!("_{}", i))
                    .collect()
            } else {
                t.column_names.iter().map(|id| id.name.clone()).collect()
            };
            let vals = row
                .iter()
                .take(col_names.len())
                .map(|e| self.expr(e))
                .collect::<Result<Vec<_>>>()?;
            for (name, val) in col_names.into_iter().zip(vals.into_iter()) {
                self.set(&name, val);
            }
        }
        Ok(ExecFlow::None)
    }

    fn input_stmt(&mut self, i: &InputStmt) -> Result<ExecFlow> {
        if let Some(prompt) = &i.prompt {
            print!("{}", prompt);
            io::stdout().flush().ok();
        }
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| self.runtime_error(format!("input failed: {}", e)))?;
        let val = Value::Str(line.trim_end_matches(['\n', '\r']).to_string());
        self.assign_target(&i.target, val.clone(), &AssignOp::Simple)?;
        Ok(ExecFlow::Value(val))
    }

    fn if_stmt(&mut self, i: &IfStmt) -> Result<ExecFlow> {
        if interp_truthy(&self.expr(&i.condition)?) {
            return self.run_block(&i.then_body);
        }
        for (cond, body) in &i.elif_chain {
            if interp_truthy(&self.expr(cond)?) {
                return self.run_block(body);
            }
        }
        if let Some(body) = &i.else_body {
            return self.run_block(body);
        }
        Ok(ExecFlow::None)
    }

    fn while_stmt(&mut self, w: &WhileStmt) -> Result<ExecFlow> {
        let mut ran = false;
        while interp_truthy(&self.expr(&w.condition)?) {
            ran = true;
            match self.run_block(&w.body)? {
                ExecFlow::None | ExecFlow::Value(_) => {}
                ExecFlow::Continue => continue,
                ExecFlow::Break => return Ok(ExecFlow::None),
                other => return Ok(other),
            }
        }
        if !ran {
            if let Some(body) = &w.else_body {
                return self.run_block(body);
            }
        }
        Ok(ExecFlow::None)
    }

    fn for_in_stmt(&mut self, f: &ForInStmt) -> Result<ExecFlow> {
        let iterable = self.expr(&f.iterable)?;
        let mut iter = self.make_iterator(iterable)?;
        let mut ran = false;
        loop {
            let next = self.next_value(&mut iter)?;
            if matches!(next, Value::StopIteration) {
                break;
            }
            ran = true;
            self.push();
            self.define_local(&f.var.name, next);
            let flow = self.run_block_no_scope(&f.body)?;
            self.pop();
            match flow {
                ExecFlow::None | ExecFlow::Value(_) => {}
                ExecFlow::Continue => continue,
                ExecFlow::Break => return Ok(ExecFlow::None),
                other => return Ok(other),
            }
        }
        if !ran {
            if let Some(body) = &f.else_body {
                return self.run_block(body);
            }
        }
        Ok(ExecFlow::None)
    }

    fn for_range_stmt(&mut self, f: &ForRangeStmt) -> Result<ExecFlow> {
        let start = self.expr(&f.from)?.to_int()?;
        let end = self.expr(&f.to)?.to_int()?;
        let mut ran = false;
        for i in start..end {
            ran = true;
            self.push();
            self.define_local(&f.var.name, Value::Int(i));
            let flow = self.run_block_no_scope(&f.body)?;
            self.pop();
            match flow {
                ExecFlow::None | ExecFlow::Value(_) => {}
                ExecFlow::Continue => continue,
                ExecFlow::Break => return Ok(ExecFlow::None),
                other => return Ok(other),
            }
        }
        if !ran {
            if let Some(body) = &f.else_body {
                return self.run_block(body);
            }
        }
        Ok(ExecFlow::None)
    }

    fn loop_stmt(&mut self, l: &LoopStmt) -> Result<ExecFlow> {
        loop {
            match self.run_block(&l.body)? {
                ExecFlow::None | ExecFlow::Value(_) => {}
                ExecFlow::Continue => continue,
                ExecFlow::Break => return Ok(ExecFlow::None),
                other => return Ok(other),
            }
        }
    }

    fn match_stmt(&mut self, m: &MatchStmt) -> Result<ExecFlow> {
        let value = self.expr(&m.expr)?;
        for case in &m.cases {
            if self.pattern_matches(&case.pattern, &value) {
                if let Some(g) = &case.guard {
                    if !interp_truthy(&self.expr(g)?) {
                        continue;
                    }
                }
                return self.run_block(&case.body);
            }
        }
        if let Some(body) = &m.else_case {
            return self.run_block(body);
        }
        Ok(ExecFlow::None)
    }

    fn try_stmt(&mut self, t: &TryStmt) -> Result<ExecFlow> {
        let try_result = self.run_block(&t.try_body);
        let mut result = match try_result {
            Ok(flow) => Ok(flow),
            Err(err) => {
                if let Some(body) = &t.catch_body {
                    self.push();
                    if let Some(var) = &t.catch_var {
                        self.define_local(&var.name, self.error_value_from(&err));
                    }
                    let caught = self.run_block_no_scope(body);
                    self.pop();
                    caught
                } else {
                    Err(err)
                }
            }
        };
        if let Some(finally_body) = &t.finally_body {
            let finally_result = self.run_block(finally_body);
            if finally_result.is_err()
                || matches!(
                    finally_result,
                    Ok(ExecFlow::Return(_)
                        | ExecFlow::Break
                        | ExecFlow::Continue
                        | ExecFlow::Yield(_))
                )
            {
                result = finally_result;
            }
        }
        result
    }

    fn with_stmt(&mut self, w: &WithStmt) -> Result<ExecFlow> {
        let manager = self.expr(&w.manager)?;
        let entered = if self.has_method(&manager, "__enter__", 0) {
            self.call_method(manager.clone(), "__enter__", vec![])?
        } else {
            manager.clone()
        };
        self.push();
        if let Some(var) = &w.var {
            self.define_local(&var.name, entered);
        }
        let body_result = self.run_block_no_scope(&w.body);
        self.pop();
        let exit_result = self.context_exit(
            manager,
            body_result.as_ref().err().map(|e| e.message.clone()),
        );
        match body_result {
            Ok(flow) => {
                exit_result?;
                Ok(flow)
            }
            Err(err) => match exit_result {
                Ok(true) => Ok(ExecFlow::None),
                Ok(false) => Err(err),
                Err(exit_err) => Err(exit_err),
            },
        }
    }

    fn context_exit(&mut self, manager: Value, error: Option<String>) -> Result<bool> {
        if self.has_method(&manager, "__exit__", 1) || self.has_method(&manager, "__exit__", 0) {
            // Match the method's arity: pass the error (or null placeholder)
            // to 1-arg __exit__, pass nothing to 0-arg __exit__.
            let wants_arg = self.has_method(&manager, "__exit__", 1);
            let args: Vec<Value> = if wants_arg {
                vec![error
                    .map(Value::Str)
                    .unwrap_or(Value::Null)]
            } else {
                vec![]
            };
            let v = self.call_method(manager, "__exit__", args)?;
            return Ok(interp_truthy(&v));
        }
        match manager {
            Value::FileHandle(id) => {
                let _ = Self::native_close(self, vec![Value::FileHandle(id)])?;
                Ok(false)
            }
            other if self.has_method(&other, "close", 0) => {
                let _ = self.call_method(other, "close", vec![])?;
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    fn select_stmt(&mut self, s: &SelectStmt) -> Result<ExecFlow> {
        for case in &s.cases {
            match &case.direction {
                SelectDirection::Send { channel, value } => {
                    let ch = self.expr(channel)?;
                    let val = self.expr(value)?;
                    if self.channel_send_value(ch, val).is_ok() {
                        return self.run_block(&case.body);
                    }
                }
                SelectDirection::Receive { channel, var } => {
                    let ch = self.expr(channel)?;
                    match self.channel_try_receive(ch)? {
                        Some(v) => {
                            self.push();
                            if let Some(id) = var {
                                self.define_local(&id.name, v);
                            }
                            let res = self.run_block_no_scope(&case.body);
                            self.pop();
                            return res;
                        }
                        None => {}
                    }
                }
                SelectDirection::After(expr) => {
                    let millis = self.expr(expr)?.to_int()?.max(0) as u64;
                    std::thread::sleep(std::time::Duration::from_millis(millis));
                    return self.run_block(&case.body);
                }
            }
        }
        if let Some(body) = &s.default_case {
            return self.run_block(body);
        }
        Ok(ExecFlow::None)
    }

    fn run_scope_block(&mut self, b: &ScopeBlock) -> Result<ExecFlow> {
        self.push();
        let prefix = b.prefix.clone();
        let flow = self.run_block_no_scope(&b.body)?;
        let scope = self.vars.pop().unwrap_or_default();
        for (k, v) in scope {
            self.set(&format!("{}.{}", prefix, k), v);
        }
        Ok(flow)
    }

    fn expr(&mut self, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Integer(v) => Ok(Value::Int(v.value)),
            Expr::Float(v) => Ok(Value::Float(v.value)),
            Expr::String_(s) => self.string_parts(&s.parts),
            Expr::MultiLineString(s) => self.string_parts(&s.parts),
            Expr::Bool(v) => Ok(Value::Bool(v.value)),
            Expr::Null(_) => Ok(Value::Null),
            Expr::Identifier(id) => self.require_var(&id.name),
            Expr::Qualified(q) => {
                let joined = q
                    .parts
                    .iter()
                    .map(|p| p.name.clone())
                    .collect::<Vec<_>>()
                    .join("::");
                if let Some(v) = self.lookup(&joined) {
                    return Ok(v);
                }
                if q.parts.len() >= 2 {
                    let first = &q.parts[0].name;
                    let mut val = self.require_var(first)?;
                    for part in q.parts.iter().skip(1) {
                        val = self.member_value(val, &part.name)?;
                    }
                    Ok(val)
                } else {
                    self.require_var(&joined)
                }
            }
            Expr::Binary(b) => {
                if b.operator == BinaryOp::And {
                    let l = self.expr(&b.left)?;
                    return if interp_truthy(&l) {
                        Ok(Value::Bool(interp_truthy(&self.expr(&b.right)?)))
                    } else {
                        Ok(Value::Bool(false))
                    };
                }
                if b.operator == BinaryOp::Or {
                    let l = self.expr(&b.left)?;
                    return if interp_truthy(&l) {
                        Ok(Value::Bool(true))
                    } else {
                        Ok(Value::Bool(interp_truthy(&self.expr(&b.right)?)))
                    };
                }
                let l = self.expr(&b.left)?;
                let r = self.expr(&b.right)?;
                self.binary_values(l, b.operator.clone(), r)
            }
            Expr::Unary(u) => {
                let v = self.expr(&u.operand)?;
                match u.operator {
                    UnaryOp::Neg => {
                        if self.has_method(&v, "__neg__", 0) {
                            return self.call_method(v, "__neg__", vec![]);
                        }
                        match v {
                            Value::Int(n) => Ok(Value::Int(-n)),
                            Value::Float(f) => Ok(Value::Float(-f)),
                            other => Err(self.runtime_error(format!(
                                "cannot negate {}",
                                self.type_name_of(&other)
                            ))),
                        }
                    }
                    UnaryOp::Not | UnaryOp::Bang => {
                        if self.has_method(&v, "__not__", 0) {
                            return self.call_method(v, "__not__", vec![]);
                        }
                        Ok(Value::Bool(!interp_truthy(&v)))
                    }
                }
            }
            Expr::Postfix(p) => {
                let v = self.expr(&p.operand)?;
                match p.operator {
                    PostfixOp::Length => Ok(Value::Int(self.len_value(v)? as i64)),
                    PostfixOp::Reverse => self.reversed_value(v),
                    PostfixOp::AscSort => self.sorted_value(v, false),
                    PostfixOp::DescSort => self.sorted_value(v, true),
                }
            }
            Expr::Call(c) => self.call_expr(c),
            Expr::MethodCall(m) => {
                let receiver = self.expr(&m.receiver)?;
                let args = self.eval_exprs(&m.args)?;
                self.call_method(receiver, &m.method.name, args)
            }
            Expr::Index(i) => {
                let target = self.expr(&i.target)?;
                let index = self.expr(&i.index)?;
                self.index_value(target, index)
            }
            Expr::Slice(sl) => {
                let target = self.expr(&sl.target)?;
                self.slice_value(target, sl)
            }
            Expr::MemberAccess(m) => {
                if let Expr::Identifier(id) = m.target.as_ref() {
                    if id.name == "super" {
                        return self.super_member(&m.member.name);
                    }
                }
                let target = self.expr(&m.target)?;
                self.member_value(target, &m.member.name)
            }
            Expr::OptionalChain(o) => self.optional_chain(o),
            Expr::Spread(s) => self.expr(&s.expr),
            Expr::Ternary(t) => {
                if interp_truthy(&self.expr(&t.condition)?) {
                    self.expr(&t.true_branch)
                } else {
                    self.expr(&t.false_branch)
                }
            }
            Expr::Lambda(l) => Ok(Value::Function(
                "<lambda>".to_string(),
                Self::params_from(&l.params),
                vec![Stmt::Return(ReturnStmt {
                    values: vec![(*l.body).clone()],
                    span: l.span.clone(),
                })],
                vec![self.current_capture()],
            )),
            Expr::Spawn(s) => self.spawn_expr(&s.call),
            Expr::Coro(c) => {
                let func = self.expr(&c.function)?;
                let args = self.eval_exprs(&c.args)?;
                Ok(self.spawn_task(func, args))
            }
            Expr::Resume(r) => {
                let handle = self.expr(&r.handle)?;
                let values = self.eval_exprs(&r.values)?;
                self.resume_value(handle, values)
            }
            Expr::Await(a) => {
                let v = self.expr(&a.expr)?;
                self.await_value(v)
            }
            Expr::Cast(c) => self.expr(&c.expr),
            Expr::TryPropagate(t) => {
                let v = self.expr(&t.expr)?;
                match v {
                    Value::Error(e) => Err(CompilerError::codegen_error(
                        RuntimeException::chained(
                            "error propagated by ?",
                            self.call_stack.clone(),
                            e,
                        )
                        .render(),
                    )),
                    other => Ok(other),
                }
            }
            Expr::Range(r) => self.range_expr(r),
            Expr::List(l) => Ok(Value::List(self.eval_exprs(&l.elements)?)),
            Expr::ListComprehension(l) => self.list_comprehension(l),
            Expr::Dict(d) => {
                let mut out = HashMap::new();
                for (k, v) in &d.entries {
                    let kv = self.expr(k)?;
                    let key = self.value_to_key(&kv);
                    out.insert(key, self.expr(v)?);
                }
                Ok(Value::Dict(out))
            }
            Expr::DictComprehension(d) => self.dict_comprehension(d),
            Expr::Set(s) => {
                let mut out = HashMap::new();
                for e in &s.elements {
                    let val = self.expr(e)?;
                    out.insert(self.value_to_key(&val), val);
                }
                Ok(Value::Set(out))
            }
            Expr::SetComprehension(s) => self.set_comprehension(s),
            Expr::Tuple(t) => Ok(Value::Tuple(self.eval_exprs(&t.elements)?)),
            Expr::Pipe(p) => {
                let left = self.expr(&p.left)?;
                if let Expr::Call(c) = p.right.as_ref() {
                    let mut args = vec![left];
                    args.extend(self.eval_exprs(&c.args)?);
                    let callee = self.expr(&c.callee)?;
                    self.call_value(callee, args)
                } else {
                    let callee = self.expr(&p.right)?;
                    self.call_value(callee, vec![left])
                }
            }
            Expr::NullCoalesce(n) => {
                let left = self.expr(&n.left)?;
                if matches!(left, Value::Null) {
                    self.expr(&n.right)
                } else {
                    Ok(left)
                }
            }
        }
    }

    fn string_parts(&mut self, parts: &[StringPart]) -> Result<Value> {
        let mut out = String::new();
        for p in parts {
            match p {
                StringPart::Text(s) => out.push_str(&interpret_escapes(s)),
                StringPart::Interpolation(e) => out.push_str(&interp_fmt(&self.expr(e)?)),
            }
        }
        Ok(Value::Str(out))
    }

    fn call_expr(&mut self, c: &CallExpr) -> Result<Value> {
        if let Expr::MemberAccess(m) = c.callee.as_ref() {
            if let Expr::Identifier(id) = m.target.as_ref() {
                if id.name == "super" {
                    let args = self.collect_args(&c.args)?;
                    return self.call_super_method(&m.member.name, args);
                }
            }
        }
        let callee = self.expr(&c.callee)?;
        let args = self.collect_args(&c.args)?;
        self.call_value(callee, args)
    }

    fn collect_args(&mut self, args: &[Expr]) -> Result<Vec<Value>> {
        let mut out = vec![];
        for arg in args {
            if let Expr::Spread(s) = arg {
                match self.expr(&s.expr)? {
                    Value::List(v) | Value::Tuple(v) => out.extend(v),
                    Value::Iterator(it) => loop {
                        let n = it.borrow_mut().next();
                        if matches!(n, Value::StopIteration) {
                            break;
                        }
                        out.push(n);
                    },
                    other => {
                        return Err(self
                            .runtime_error(format!("cannot spread {}", self.type_name_of(&other))))
                    }
                }
            } else {
                out.push(self.expr(arg)?);
            }
        }
        Ok(out)
    }

    fn call_value(&mut self, callee: Value, args: Vec<Value>) -> Result<Value> {
        match callee {
            Value::Native(f) => f(self, args),
            Value::Nf(f) => Ok(f(args)),
            Value::Function(name, params, body, captures) => {
                self.call_function(&name, &params, &body, &captures, args)
            }
            Value::AsyncFunction(name, params, body, captures) => {
                Ok(self.spawn_task(Value::Function(name, params, body, captures), args))
            }
            Value::BoundMethod(receiver, method) => self.call_bound_method(*receiver, method, args),
            Value::Class(name) => self.construct_class(&name, args),
            Value::Struct(name, fields) if name.contains("::") => Ok(Value::Struct(name, fields)),
            other => {
                if self.has_method(&other, "__call__", args.len()) {
                    self.call_method(other, "__call__", args)
                } else {
                    Err(self
                        .runtime_error(format!("{} is not callable", self.type_name_of(&other))))
                }
            }
        }
    }

    fn call_function(
        &mut self,
        name: &str,
        params: &[(String, bool)],
        body: &[Stmt],
        captures: &[HashMap<String, Value>],
        args: Vec<Value>,
    ) -> Result<Value> {
        // If the function body contains yield, collect all yield values
        // eagerly into an iterator, and return a Coroutine wrapping it.
        if body_has_yield(body) {
            return self.call_generator(name, params, body, captures, args);
        }
        self.depth += 1;
        if self.depth > self.max_depth {
            self.depth -= 1;
            return Err(self.runtime_error("recursion depth exceeded"));
        }
        self.call_stack.push(name.to_string());
        self.push();
        if let Some(cap) = captures.first() {
            for (k, v) in cap {
                if !self.builtins.contains(k) && self.lookup(k).is_none() {
                    self.define_local(k, v.clone());
                }
            }
        }
        self.bind_params(params, args)?;
        self.defer_stack.push(vec![]);
        let flow = self.run_block_no_scope(body);
        let defer_result = self.run_defers();
        self.pop();
        self.call_stack.pop();
        self.depth -= 1;
        defer_result?;
        match flow? {
            ExecFlow::Return(v) | ExecFlow::Value(v) => Ok(v),
            ExecFlow::Yield(_) => Err(self.runtime_error("yield escaped function (not a coroutine)")),
            ExecFlow::None => Ok(Value::Null),
            ExecFlow::Break => Err(self.runtime_error("break escaped function")),
            ExecFlow::Continue => Err(self.runtime_error("continue escaped function")),
        }
    }

    /// Call a generator function: run the body, collecting yield values
    /// into an iterator. Returns a Coroutine value that wraps the iterator.
    fn call_generator(
        &mut self,
        name: &str,
        params: &[(String, bool)],
        body: &[Stmt],
        captures: &[HashMap<String, Value>],
        args: Vec<Value>,
    ) -> Result<Value> {
        self.depth += 1;
        if self.depth > self.max_depth {
            self.depth -= 1;
            return Err(self.runtime_error("recursion depth exceeded"));
        }
        self.call_stack.push(name.to_string());
        self.push();
        if let Some(cap) = captures.first() {
            for (k, v) in cap {
                if !self.builtins.contains(k) && self.lookup(k).is_none() {
                    self.define_local(k, v.clone());
                }
            }
        }
        self.bind_params(params, args)?;
        self.defer_stack.push(vec![]);
        // Run the body, collecting yield values.
        let mut yielded = Vec::new();
        for stmt in body {
            match self.stmt(stmt) {
                Ok(ExecFlow::Yield(v)) => yielded.push(v),
                Ok(ExecFlow::Return(v)) => {
                    // A return statement ends the generator.
                    // The return value is the final value (discarded for generators).
                    let _ = v;
                    break;
                }
                Ok(ExecFlow::None) | Ok(ExecFlow::Value(_)) => {}
                Ok(ExecFlow::Break) => break,
                Ok(ExecFlow::Continue) => {}
                Err(e) => {
                    let defer_result = self.run_defers();
                    self.pop();
                    self.call_stack.pop();
                    self.depth -= 1;
                    defer_result?;
                    return Err(e);
                }
            }
        }
        let defer_result = self.run_defers();
        self.pop();
        self.call_stack.pop();
        self.depth -= 1;
        defer_result?;
        // Return a Coroutine wrapping an iterator over the yielded values.
        Ok(Value::Iterator(Rc::new(RefCell::new(RuntimeIterator::new(yielded)))))
    }

    fn bind_params(&mut self, params: &[(String, bool)], args: Vec<Value>) -> Result<()> {
        let mut used = 0;
        for (i, (name, variadic)) in params.iter().enumerate() {
            if *variadic {
                self.define_local(name, Value::List(args.iter().skip(i).cloned().collect()));
                used = args.len();
                return Ok(());
            }
            let arg = args
                .get(i)
                .cloned()
                .ok_or_else(|| self.runtime_error(format!("missing argument '{}'", name)))?;
            self.define_local(name, arg);
            used += 1;
        }
        if used < args.len() {
            return Err(self.runtime_error(format!(
                "too many arguments: expected {}, got {}",
                params.len(),
                args.len()
            )));
        }
        Ok(())
    }

    fn run_defers(&mut self) -> Result<()> {
        let stack = self.defer_stack.pop().unwrap_or_default();
        for stmt in stack.iter().rev() {
            match self.stmt(stmt)? {
                ExecFlow::None | ExecFlow::Value(_) => {}
                _ => return Err(self.runtime_error("defer cannot return/break/continue/yield")),
            }
        }
        Ok(())
    }

    fn construct_class(&mut self, name: &str, args: Vec<Value>) -> Result<Value> {
        if self.struct_fields.contains_key(name) && !self.class_fields.contains_key(name) {
            let mut fields = HashMap::new();
            if let Some(order) = self.struct_fields.get(name) {
                for (idx, field) in order.iter().enumerate() {
                    fields.insert(
                        field.clone(),
                        args.get(idx).cloned().unwrap_or_else(|| Value::Null),
                    );
                }
            }
            return Ok(Value::Struct(name.to_string(), fields));
        }
        let mut fields = HashMap::new();
        for cname in self.mro(name)?.into_iter().rev() {
            if let Some(items) = self.class_fields.get(&cname).cloned() {
                for (fname, default_expr) in items {
                    let val = if let Some(expr) = default_expr {
                        self.expr(&expr)?
                    } else {
                        Value::Null
                    };
                    fields.insert(fname, val);
                }
            }
        }
        let obj = Value::Object(name.to_string(), Rc::new(RefCell::new(fields)));
        if let Some(method) = self
            .lookup_method(name, "new", args.len())
            .or_else(|| self.lookup_method(name, "init", args.len()))
        {
            // Call new()/init() for its side effects. The return value is
            // only used if it is an Object/Struct (i.e. the user explicitly
            // returned a different instance); otherwise we keep `obj` as
            // the constructed value. This matches Python's `__init__`
            // semantics where the constructor returns the new instance
            // regardless of what __init__ returns.
            let ret = self.call_bound_method(obj.clone(), method, args)?;
            match ret {
                Value::Object(_, _) | Value::Struct(_, _) => return Ok(ret),
                _ => {}
            }
        }
        Ok(obj)
    }

    fn call_method(&mut self, receiver: Value, name: &str, args: Vec<Value>) -> Result<Value> {
        match receiver.clone() {
            Value::Object(class, _) | Value::Struct(class, _) => {
                let method = self
                    .lookup_method(&class, name, args.len())
                    .ok_or_else(|| {
                        self.runtime_error(format!("method '{}.{}' not found", class, name))
                    })?;
                self.call_bound_method(receiver, method, args)
            }
            Value::List(mut list) => self.call_list_method(&mut list, name, args),
            Value::Dict(mut d) => self.call_dict_method(&mut d, name, args),
            Value::Set(mut s) => self.call_set_method(&mut s, name, args),
            Value::Str(s) => self.call_str_method(&s, name, args),
            Value::Frozen(_)
                if matches!(
                    name,
                    "__setitem__" | "__setattr__" | "__delitem__" | "__delattr__"
                ) =>
            {
                Err(self.runtime_error(format!(
                    "frozen value cannot use mutating special method '{}'",
                    name
                )))
            }
            Value::Frozen(inner) if name.starts_with("__") => self.call_method(*inner, name, args),
            Value::Frozen(_) => {
                Err(self.runtime_error(format!("frozen value has no mutable method '{}'", name)))
            }
            other => Err(self.runtime_error(format!(
                "{} has no method '{}'",
                self.type_name_of(&other),
                name
            ))),
        }
    }

    fn call_bound_method(
        &mut self,
        receiver: Value,
        method: RuntimeMethod,
        args: Vec<Value>,
    ) -> Result<Value> {
        self.depth += 1;
        if self.depth > self.max_depth {
            self.depth -= 1;
            return Err(self.runtime_error("recursion depth exceeded"));
        }
        self.call_stack
            .push(format!("{}.{}", method.owner, method.name));
        self.push();
        self.define_local("self", receiver.clone());
        self.define_local("super", Value::Struct(method.owner.clone(), HashMap::new()));
        self.bind_params(&method.params, args)?;
        self.defer_stack.push(vec![]);
        let flow = self.run_block_no_scope(&method.body);
        let defer_result = self.run_defers();
        self.pop();
        self.call_stack.pop();
        self.depth -= 1;
        defer_result?;
        match flow? {
            ExecFlow::Return(v) | ExecFlow::Value(v) | ExecFlow::Yield(v) => Ok(v),
            ExecFlow::None => Ok(Value::Null),
            ExecFlow::Break => Err(self.runtime_error("break escaped method")),
            ExecFlow::Continue => Err(self.runtime_error("continue escaped method")),
        }
    }

    fn lookup_method(&self, class: &str, name: &str, argc: usize) -> Option<RuntimeMethod> {
        for cname in self.mro(class).ok()? {
            if let Some(methods) = self.class_methods.get(&cname).and_then(|m| m.get(name)) {
                if let Some(exact) = methods
                    .iter()
                    .find(|m| Self::method_accepts(&m.params, argc, true))
                {
                    return Some(exact.clone());
                }
                if let Some(compatible) = methods
                    .iter()
                    .find(|m| Self::method_accepts(&m.params, argc, false))
                {
                    return Some(compatible.clone());
                }
            }
        }
        None
    }

    /// Look up a method by name only, ignoring arity. Used when binding a
    /// method to a value (`obj.method`) before the call args are known.
    /// Returns the first definition in MRO order; if there are overloads,
    /// the one with the most parameters wins (so calls with N args can still
    /// dispatch correctly via `lookup_method` at call time).
    fn lookup_method_any(&self, class: &str, name: &str) -> Option<RuntimeMethod> {
        for cname in self.mro(class).ok()? {
            if let Some(methods) = self.class_methods.get(&cname).and_then(|m| m.get(name)) {
                if let Some(first) = methods.first() {
                    return Some(first.clone());
                }
            }
        }
        None
    }

    fn has_method(&self, receiver: &Value, name: &str, argc: usize) -> bool {
        match receiver {
            Value::Object(class, _) | Value::Struct(class, _) => {
                self.lookup_method(class, name, argc).is_some()
            }
            Value::Frozen(inner) => self.has_method(inner, name, argc),
            _ => false,
        }
    }

    fn method_accepts(params: &[(String, bool)], argc: usize, exact: bool) -> bool {
        if let Some(pos) = params.iter().position(|(_, variadic)| *variadic) {
            return argc >= pos;
        }
        if exact {
            params.len() == argc
        } else {
            params.len() <= argc
        }
    }

    fn mro(&self, class: &str) -> Result<Vec<String>> {
        let mut out = vec![];
        let mut seen = HashSet::new();
        let mut current = Some(class.to_string());
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return Err(self.runtime_error(format!("inheritance cycle involving '{}'", name)));
            }
            out.push(name.clone());
            current = self.class_extends.get(&name).cloned().unwrap_or(None);
        }
        Ok(out)
    }

    fn super_member(&mut self, name: &str) -> Result<Value> {
        let self_obj = self.require_var("self")?;
        let class = match self_obj.clone() {
            Value::Object(c, _) | Value::Struct(c, _) => c,
            _ => return Err(self.runtime_error("super used outside class method")),
        };
        let parent = self
            .class_extends
            .get(&class)
            .cloned()
            .unwrap_or(None)
            .ok_or_else(|| self.runtime_error(format!("class '{}' has no super class", class)))?;
        let method = self
            .lookup_method(&parent, name, 0)
            .ok_or_else(|| self.runtime_error(format!("super method '{}' not found", name)))?;
        Ok(Value::BoundMethod(Box::new(self_obj), method))
    }

    fn call_super_method(&mut self, name: &str, args: Vec<Value>) -> Result<Value> {
        let self_obj = self.require_var("self")?;
        let class = match self_obj.clone() {
            Value::Object(c, _) | Value::Struct(c, _) => c,
            _ => return Err(self.runtime_error("super used outside class method")),
        };
        let parent = self
            .class_extends
            .get(&class)
            .cloned()
            .unwrap_or(None)
            .ok_or_else(|| self.runtime_error(format!("class '{}' has no super class", class)))?;
        let method = self
            .lookup_method(&parent, name, args.len())
            .ok_or_else(|| self.runtime_error(format!("super method '{}' not found", name)))?;
        self.call_bound_method(self_obj, method, args)
    }

    fn member_value(&mut self, target: Value, member: &str) -> Result<Value> {
        match target {
            Value::Object(class, fields) => {
                if let Some(v) = fields.borrow().get(member).cloned() {
                    return Ok(v);
                }
                // Look up the method with ANY arity. The bound method will
                // be re-dispatched with the actual argc at call time.
                if let Some(method) = self.lookup_method_any(&class, member) {
                    return Ok(Value::BoundMethod(
                        Box::new(Value::Object(class, fields)),
                        method,
                    ));
                }
                Err(self.runtime_error(format!("object '{}' has no member '{}'", class, member)))
            }
            Value::Struct(name, fields) => {
                if let Some(v) = fields.get(member).cloned() {
                    return Ok(v);
                }
                if let Some(v) = self.lookup(&format!("{}::{}", name, member)) {
                    return Ok(v);
                }
                if let Some(method) = self.lookup_method_any(&name, member) {
                    return Ok(Value::BoundMethod(
                        Box::new(Value::Struct(name, fields)),
                        method,
                    ));
                }
                Err(self.runtime_error(format!("struct '{}' has no member '{}'", name, member)))
            }
            Value::Module(_, exports) => exports
                .get(member)
                .cloned()
                .ok_or_else(|| self.runtime_error(format!("module has no symbol '{}'", member))),
            Value::Dict(d) => d
                .get(member)
                .cloned()
                .ok_or_else(|| self.runtime_error(format!("dict has no key '{}'", member))),
            Value::List(l) if member == "len" => Ok(Value::Int(l.len() as i64)),
            Value::Str(s) if member == "len" => Ok(Value::Int(s.len() as i64)),
            Value::Tuple(t) if member == "len" => Ok(Value::Int(t.len() as i64)),
            Value::Set(s) if member == "len" => Ok(Value::Int(s.len() as i64)),
            Value::Frozen(inner) => self.member_value(*inner, member),
            other => Err(self.runtime_error(format!(
                "{} has no member '{}'",
                self.type_name_of(&other),
                member
            ))),
        }
    }

    fn optional_chain(&mut self, o: &OptionalChainExpr) -> Result<Value> {
        let mut value = self.expr(&o.target)?;
        if matches!(value, Value::Null) {
            return Ok(Value::Null);
        }
        for link in &o.chain {
            value = match link {
                OptionalChainLink::Member(id) => match self.member_value(value, &id.name) {
                    Ok(v) => v,
                    Err(_) => return Ok(Value::Null),
                },
                OptionalChainLink::Call { method, args } => {
                    let args = self.eval_exprs(args)?;
                    match self.call_method(value, &method.name, args) {
                        Ok(v) => v,
                        Err(_) => return Ok(Value::Null),
                    }
                }
                OptionalChainLink::Index(idx) => {
                    let i = self.expr(idx)?;
                    match self.index_value(value, i) {
                        Ok(v) => v,
                        Err(_) => return Ok(Value::Null),
                    }
                }
            };
            if matches!(value, Value::Null) {
                return Ok(Value::Null);
            }
        }
        Ok(value)
    }

    fn index_value(&mut self, target: Value, index: Value) -> Result<Value> {
        if self.has_method(&target, "__getitem__", 1) {
            return self.call_method(target, "__getitem__", vec![index]);
        }
        match target {
            Value::Frozen(inner) => self.index_value(*inner, index),
            Value::List(v) => {
                let idx = index.to_index()?;
                v.get(idx)
                    .cloned()
                    .ok_or_else(|| self.runtime_error(format!("list index {} out of range", idx)))
            }
            Value::Tuple(v) => {
                let idx = index.to_index()?;
                v.get(idx)
                    .cloned()
                    .ok_or_else(|| self.runtime_error(format!("tuple index {} out of range", idx)))
            }
            Value::Str(s) => {
                let idx = index.to_index()?;
                s.chars()
                    .nth(idx)
                    .map(|c| Value::Str(c.to_string()))
                    .ok_or_else(|| self.runtime_error(format!("string index {} out of range", idx)))
            }
            Value::Dict(d) => {
                let key = self.value_to_key(&index);
                d.get(&key)
                    .cloned()
                    .ok_or_else(|| self.runtime_error(format!("dict key '{}' not found", key)))
            }
            Value::Object(_, fields) => {
                let key = self.value_to_key(&index);
                fields
                    .borrow()
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| self.runtime_error(format!("object key '{}' not found", key)))
            }
            other => Err(self.runtime_error(format!("cannot index {}", self.type_name_of(&other)))),
        }
    }

    fn slice_value(&mut self, target: Value, sl: &SliceExpr) -> Result<Value> {
        if self.has_method(&target, "__slice__", 3) {
            let a = match &sl.start {
                Some(e) => self.expr(e)?,
                None => Value::Null,
            };
            let b = match &sl.end {
                Some(e) => self.expr(e)?,
                None => Value::Null,
            };
            let c = match &sl.step {
                Some(e) => self.expr(e)?,
                None => Value::Null,
            };
            return self.call_method(target, "__slice__", vec![a, b, c]);
        }
        match target {
            Value::Frozen(inner) => self.slice_value(*inner, sl),
            Value::List(v) => {
                let idxs = self.slice_indices(v.len(), &sl.start, &sl.end, &sl.step)?;
                Ok(Value::List(
                    idxs.into_iter().map(|i| v[i].clone()).collect(),
                ))
            }
            Value::Tuple(v) => {
                let idxs = self.slice_indices(v.len(), &sl.start, &sl.end, &sl.step)?;
                Ok(Value::Tuple(
                    idxs.into_iter().map(|i| v[i].clone()).collect(),
                ))
            }
            Value::Str(text) => {
                let chars: Vec<char> = text.chars().collect();
                let idxs = self.slice_indices(chars.len(), &sl.start, &sl.end, &sl.step)?;
                Ok(Value::Str(idxs.into_iter().map(|i| chars[i]).collect()))
            }
            other => Err(self.runtime_error(format!("cannot slice {}", self.type_name_of(&other)))),
        }
    }

    fn slice_indices(
        &mut self,
        len: usize,
        start: &Option<Box<Expr>>,
        end: &Option<Box<Expr>>,
        step: &Option<Box<Expr>>,
    ) -> Result<Vec<usize>> {
        let len_i = len as i64;
        let step_i = match step {
            Some(e) => self.expr(e)?.to_int()?,
            None => 1,
        };
        if step_i == 0 {
            return Err(self.runtime_error("slice step cannot be zero"));
        }
        let mut s = match start {
            Some(e) => self.expr(e)?.to_int()?,
            None => {
                if step_i > 0 {
                    0
                } else {
                    len_i - 1
                }
            }
        };
        let mut e = match end {
            Some(e) => self.expr(e)?.to_int()?,
            None => {
                if step_i > 0 {
                    len_i
                } else {
                    -1
                }
            }
        };
        if s < 0 {
            s += len_i;
        }
        if e < 0 && end.is_some() {
            e += len_i;
        }
        if step_i > 0 {
            s = s.clamp(0, len_i);
            e = e.clamp(0, len_i);
        } else {
            if s >= len_i {
                s = len_i - 1;
            }
            if s < -1 {
                s = -1;
            }
            if e >= len_i {
                e = len_i - 1;
            }
            if e < -1 {
                e = -1;
            }
        }
        let mut out = vec![];
        let mut i = s;
        if step_i > 0 {
            while i < e {
                if i >= 0 && i < len_i {
                    out.push(i as usize);
                }
                i += step_i;
            }
        } else {
            while i > e {
                if i >= 0 && i < len_i {
                    out.push(i as usize);
                }
                i += step_i;
            }
        }
        Ok(out)
    }

    fn binary_values(&mut self, l: Value, op: BinaryOp, r: Value) -> Result<Value> {
        if let Some(v) = self.try_operator_overload(&l, &op, &r)? {
            return Ok(v);
        }
        use BinaryOp::*;
        match op {
            Add => match (l, r) {
                (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a + b)),
                (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
                (Value::Int(a), Value::Float(b)) => Ok(Value::Float(a as f64 + b)),
                (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a + b as f64)),
                (Value::Str(a), b) => Ok(Value::Str(format!("{}{}", a, interp_fmt(&b)))),
                (a, Value::Str(b)) => Ok(Value::Str(format!("{}{}", interp_fmt(&a), b))),
                (Value::List(mut a), Value::List(b)) => {
                    a.extend(b);
                    Ok(Value::List(a))
                }
                (a, b) => Err(self.runtime_error(format!(
                    "cannot add {} and {}",
                    self.type_name_of(&a),
                    self.type_name_of(&b)
                ))),
            },
            Sub | Mul | Div | FloorDiv | Mod | Power => self.numeric_binary(l, op, r),
            Eq => Ok(Value::Bool(self.values_equal(&l, &r))),
            Ne => Ok(Value::Bool(!self.values_equal(&l, &r))),
            Lt | Gt | Le | Ge => self.compare_values(l, op, r),
            Is => Ok(Value::Bool(self.type_name_of(&l) == self.type_name_of(&r))),
            In => Ok(Value::Bool(self.contains_value(&r, &l))),
            BitAnd | BitOr | BitXor | Shl | Shr => self.bitwise_binary(l, op, r),
            Repeated => self.repeated_value(l, r),
            And | Or => unreachable!(),
        }
    }

    fn try_operator_overload(
        &mut self,
        l: &Value,
        op: &BinaryOp,
        r: &Value,
    ) -> Result<Option<Value>> {
        let name = match op {
            BinaryOp::Add => "__add__",
            BinaryOp::Sub => "__sub__",
            BinaryOp::Mul => "__mul__",
            BinaryOp::Div => "__truediv__",
            BinaryOp::FloorDiv => "__floordiv__",
            BinaryOp::Mod => "__mod__",
            BinaryOp::Power => "__pow__",
            BinaryOp::Eq => "__eq__",
            BinaryOp::Ne => "__ne__",
            BinaryOp::Lt => "__lt__",
            BinaryOp::Gt => "__gt__",
            BinaryOp::Le => "__le__",
            BinaryOp::Ge => "__ge__",
            BinaryOp::In => "__contains__",
            _ => return Ok(None),
        };
        if matches!(op, BinaryOp::In) {
            if self.has_method(r, name, 1) {
                return self.call_method(r.clone(), name, vec![l.clone()]).map(Some);
            }
            return Ok(None);
        }
        if self.has_method(l, name, 1) {
            return self.call_method(l.clone(), name, vec![r.clone()]).map(Some);
        }
        Ok(None)
    }

    fn numeric_binary(&self, l: Value, op: BinaryOp, r: Value) -> Result<Value> {
        let both_int = matches!((&l, &r), (Value::Int(_), Value::Int(_)));
        let a = l.to_float()?;
        let b = r.to_float()?;
        match op {
            BinaryOp::Sub => {
                if both_int {
                    Ok(Value::Int(a as i64 - b as i64))
                } else {
                    Ok(Value::Float(a - b))
                }
            }
            BinaryOp::Mul => {
                if both_int {
                    Ok(Value::Int(a as i64 * b as i64))
                } else {
                    Ok(Value::Float(a * b))
                }
            }
            BinaryOp::Div => {
                if b == 0.0 {
                    Err(self.runtime_error("division by zero"))
                } else {
                    Ok(Value::Float(a / b))
                }
            }
            BinaryOp::FloorDiv => {
                if b == 0.0 {
                    Err(self.runtime_error("floor division by zero"))
                } else {
                    Ok(Value::Int((a / b).floor() as i64))
                }
            }
            BinaryOp::Mod => {
                if b == 0.0 {
                    Err(self.runtime_error("modulo by zero"))
                } else if both_int {
                    Ok(Value::Int((a as i64) % (b as i64)))
                } else {
                    Ok(Value::Float(a % b))
                }
            }
            BinaryOp::Power => Ok(Value::Float(a.powf(b))),
            _ => unreachable!(),
        }
    }

    fn bitwise_binary(&self, l: Value, op: BinaryOp, r: Value) -> Result<Value> {
        let a = l.to_int()?;
        let b = r.to_int()?;
        Ok(Value::Int(match op {
            BinaryOp::BitAnd => a & b,
            BinaryOp::BitOr => a | b,
            BinaryOp::BitXor => a ^ b,
            BinaryOp::Shl => a << b,
            BinaryOp::Shr => a >> b,
            _ => unreachable!(),
        }))
    }

    fn compare_values(&self, l: Value, op: BinaryOp, r: Value) -> Result<Value> {
        let ord = match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => a.partial_cmp(b),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
            (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
            (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
            _ => None,
        }
        .ok_or_else(|| {
            self.runtime_error(format!(
                "cannot compare {} and {}",
                self.type_name_of(&l),
                self.type_name_of(&r)
            ))
        })?;
        Ok(Value::Bool(match op {
            BinaryOp::Lt => ord.is_lt(),
            BinaryOp::Gt => ord.is_gt(),
            BinaryOp::Le => ord.is_lt() || ord.is_eq(),
            BinaryOp::Ge => ord.is_gt() || ord.is_eq(),
            _ => unreachable!(),
        }))
    }

    fn repeated_value(&self, l: Value, r: Value) -> Result<Value> {
        let n = r.to_int()?.max(0) as usize;
        match l {
            Value::Str(s) => Ok(Value::Str(s.repeat(n))),
            Value::List(v) => {
                let mut out = Vec::with_capacity(v.len() * n);
                for _ in 0..n {
                    out.extend(v.clone());
                }
                Ok(Value::List(out))
            }
            other => {
                Err(self.runtime_error(format!("cannot repeat {}", self.type_name_of(&other))))
            }
        }
    }

    fn contains_value(&self, haystack: &Value, needle: &Value) -> bool {
        match haystack {
            Value::List(v) | Value::Tuple(v) => v.iter().any(|x| self.values_equal(x, needle)),
            Value::Str(s) => s.contains(&interp_fmt(needle)),
            Value::Dict(d) | Value::Set(d) => d.contains_key(&self.value_to_key(needle)),
            Value::Object(_, f) => f.borrow().contains_key(&self.value_to_key(needle)),
            Value::Frozen(inner) => self.contains_value(inner, needle),
            _ => false,
        }
    }

    fn values_equal(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => x == y,
            (Value::Float(x), Value::Float(y)) => (*x - *y).abs() < f64::EPSILON,
            (Value::Int(x), Value::Float(y)) => (*x as f64 - *y).abs() < f64::EPSILON,
            (Value::Float(x), Value::Int(y)) => (*x - *y as f64).abs() < f64::EPSILON,
            (Value::Str(x), Value::Str(y)) => x == y,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Null, Value::Null) => true,
            (Value::List(x), Value::List(y)) | (Value::Tuple(x), Value::Tuple(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .zip(y.iter())
                        .all(|(a, b)| self.values_equal(a, b))
            }
            (Value::List(x), Value::Tuple(y)) | (Value::Tuple(x), Value::List(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .zip(y.iter())
                        .all(|(a, b)| self.values_equal(a, b))
            }
            (Value::Dict(x), Value::Dict(y)) | (Value::Set(x), Value::Set(y)) => {
                if x.len() != y.len() {
                    return false;
                }
                x.iter().all(|(k, v)| {
                    y.get(k).map_or(false, |yv| self.values_equal(v, yv))
                })
            }
            (Value::Frozen(x), y) => self.values_equal(x, y),
            (x, Value::Frozen(y)) => self.values_equal(x, y),
            _ => false,
        }
    }

    fn len_value(&mut self, value: Value) -> Result<usize> {
        if self.has_method(&value, "__len__", 0) {
            return Ok(self.call_method(value, "__len__", vec![])?.to_int()?.max(0) as usize);
        }
        self.len_of(&value)
    }

    fn len_of(&self, value: &Value) -> Result<usize> {
        match value {
            Value::Str(s) => Ok(s.chars().count()),
            Value::List(v) | Value::Tuple(v) => Ok(v.len()),
            Value::Dict(d) | Value::Set(d) | Value::Module(_, d) => Ok(d.len()),
            Value::Object(_, f) => Ok(f.borrow().len()),
            Value::Frozen(inner) => self.len_of(inner),
            other => Err(self.runtime_error(format!(
                "len() does not support {}",
                self.type_name_of(other)
            ))),
        }
    }

    fn type_name_of(&self, value: &Value) -> String {
        match value {
            Value::Int(_) => "int".into(),
            Value::Float(_) => "float".into(),
            Value::Str(_) => "str".into(),
            Value::Bool(_) => "bool".into(),
            Value::Null => "null".into(),
            Value::List(_) => "list".into(),
            Value::Tuple(_) => "tuple".into(),
            Value::Dict(_) => "dict".into(),
            Value::Set(_) => "set".into(),
            Value::Struct(n, _) => n.clone(),
            Value::Object(n, _) => n.clone(),
            Value::Class(n) => format!("class {}", n),
            Value::Module(n, _) => format!("module {}", n),
            Value::Function(n, _, _, _) => format!("fn {}", n),
            Value::AsyncFunction(n, _, _, _) => format!("async fn {}", n),
            Value::BoundMethod(_, m) => format!("method {}.{}", m.owner, m.name),
            Value::Native(_) | Value::Nf(_) => "native_fn".into(),
            Value::Channel(_) => "channel".into(),
            Value::Coroutine(_) => "coroutine".into(),
            Value::FileHandle(_) => "file".into(),
            Value::Iterator(_) => "iterator".into(),
            Value::Error(_) => "error".into(),
            Value::Frozen(inner) => format!("frozen {}", self.type_name_of(inner)),
            Value::StopIteration => "StopIteration".into(),
        }
    }

    fn value_to_key(&self, value: &Value) -> String {
        match value {
            Value::Str(s) => s.clone(),
            Value::Int(n) => n.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => "null".to_string(),
            Value::Frozen(inner) => format!("frozen:{}", self.value_to_key(inner)),
            other => interp_fmt(other),
        }
    }

    fn range_expr(&mut self, r: &RangeExpr) -> Result<Value> {
        let start = if let Some(s) = &r.start {
            self.expr(s)?.to_int()?
        } else {
            0
        };
        let end = if let Some(e) = &r.end {
            self.expr(e)?.to_int()?
        } else {
            start
        };
        let step = if let Some(s) = &r.step {
            self.expr(s)?.to_int()?
        } else {
            1
        };
        if step == 0 {
            return Err(self.runtime_error("range step cannot be zero"));
        }
        let mut out = vec![];
        let mut i = start;
        if step > 0 {
            while if r.inclusive { i <= end } else { i < end } {
                out.push(Value::Int(i));
                i += step;
            }
        } else {
            while if r.inclusive { i >= end } else { i > end } {
                out.push(Value::Int(i));
                i += step;
            }
        }
        Ok(Value::List(out))
    }

    fn make_iterator(&mut self, value: Value) -> Result<Value> {
        if self.has_method(&value, "__iter__", 0) {
            let iter = self.call_method(value, "__iter__", vec![])?;
            return self.make_iterator(iter);
        }
        match value {
            v @ Value::Iterator(_) => Ok(v),
            Value::Frozen(inner) => self.make_iterator(*inner),
            Value::List(v) | Value::Tuple(v) => Ok(Value::Iterator(Rc::new(RefCell::new(
                RuntimeIterator::new(v),
            )))),
            Value::Str(s) => Ok(Value::Iterator(Rc::new(RefCell::new(
                RuntimeIterator::new(s.chars().map(|c| Value::Str(c.to_string())).collect()),
            )))),
            Value::Dict(d) => Ok(Value::Iterator(Rc::new(RefCell::new(
                RuntimeIterator::new(
                    d.into_iter()
                        .map(|(k, v)| Value::Tuple(vec![Value::Str(k), v]))
                        .collect(),
                ),
            )))),
            Value::Set(s) => Ok(Value::Iterator(Rc::new(RefCell::new(
                RuntimeIterator::new(s.into_values().collect()),
            )))),
            Value::Object(class, fields) => {
                let obj = Value::Object(class.clone(), fields.clone());
                if let Some(method) = self.lookup_method(&class, "next", 0) {
                    return Ok(Value::BoundMethod(Box::new(obj), method));
                }
                Err(self.runtime_error(format!(
                    "object '{}' is not iterable: missing next()",
                    class
                )))
            }
            other => {
                Err(self.runtime_error(format!("{} is not iterable", self.type_name_of(&other))))
            }
        }
    }

    fn next_value(&mut self, iter: &mut Value) -> Result<Value> {
        match iter {
            Value::Iterator(it) => Ok(it.borrow_mut().next()),
            Value::BoundMethod(receiver, method) => {
                self.call_bound_method((**receiver).clone(), method.clone(), vec![])
            }
            other => {
                Err(self.runtime_error(format!("{} is not an iterator", self.type_name_of(other))))
            }
        }
    }

    fn list_comprehension(&mut self, c: &ListComprehension) -> Result<Value> {
        let iter_src = self.expr(&c.iterable)?;
        let mut iter = self.make_iterator(iter_src)?;
        let mut out = vec![];
        self.push();
        loop {
            let item = self.next_value(&mut iter)?;
            if matches!(item, Value::StopIteration) {
                break;
            }
            self.set(&c.var.name, item);
            if let Some(cond) = &c.condition {
                if !interp_truthy(&self.expr(cond)?) {
                    continue;
                }
            }
            out.push(self.expr(&c.result_expr)?);
        }
        self.pop();
        Ok(Value::List(out))
    }

    fn dict_comprehension(&mut self, c: &DictComprehension) -> Result<Value> {
        let iter_src = self.expr(&c.iterable)?;
        let mut iter = self.make_iterator(iter_src)?;
        let mut out = HashMap::new();
        self.push();
        loop {
            let item = self.next_value(&mut iter)?;
            if matches!(item, Value::StopIteration) {
                break;
            }
            self.set(&c.var.name, item);
            if let Some(cond) = &c.condition {
                if !interp_truthy(&self.expr(cond)?) {
                    continue;
                }
            }
            let key_val = self.expr(&c.key_expr)?;
            let key = self.value_to_key(&key_val);
            let val = self.expr(&c.value_expr)?;
            out.insert(key, val);
        }
        self.pop();
        Ok(Value::Dict(out))
    }

    fn set_comprehension(&mut self, c: &SetComprehension) -> Result<Value> {
        let list = self.list_comprehension(&ListComprehension {
            result_expr: c.result_expr.clone(),
            var: c.var.clone(),
            iterable: c.iterable.clone(),
            condition: c.condition.clone(),
            span: c.span.clone(),
        })?;
        let mut out = HashMap::new();
        if let Value::List(items) = list {
            for v in items {
                out.insert(self.value_to_key(&v), v);
            }
        }
        Ok(Value::Set(out))
    }

    fn pattern_matches(&self, pattern: &Pattern, value: &Value) -> bool {
        match pattern {
            Pattern::Wildcard(_) => true,
            Pattern::Binding(_) => true,
            Pattern::Literal(l) => match l.literal.as_ref() {
                Expr::Integer(i) => matches!(value, Value::Int(n) if *n == i.value),
                Expr::String_(s) => {
                    let text = s
                        .parts
                        .iter()
                        .filter_map(|p| {
                            if let StringPart::Text(t) = p {
                                Some(t.clone())
                            } else {
                                None
                            }
                        })
                        .collect::<String>();
                    matches!(value, Value::Str(v) if *v == text)
                }
                Expr::Bool(b) => matches!(value, Value::Bool(v) if *v == b.value),
                Expr::Null(_) => matches!(value, Value::Null),
                _ => false,
            },
            Pattern::Tuple(t) => {
                if let Value::Tuple(items) = value {
                    items.len() == t.elements.len()
                        && t.elements
                            .iter()
                            .zip(items)
                            .all(|(p, v)| self.pattern_matches(p, v))
                } else {
                    false
                }
            }
            Pattern::List(l) => {
                if let Value::List(items) = value {
                    items.len() >= l.elements.len()
                        && l.elements
                            .iter()
                            .zip(items)
                            .all(|(p, v)| self.pattern_matches(p, v))
                } else {
                    false
                }
            }
            Pattern::Dict(d) => {
                if let Value::Dict(map) = value {
                    d.entries.iter().all(|(k, p)| {
                        map.get(k)
                            .map(|v| self.pattern_matches(p, v))
                            .unwrap_or(false)
                    })
                } else {
                    false
                }
            }
            Pattern::Struct(s) => match value {
                Value::Struct(n, fields) if n == &s.type_name.name => {
                    s.fields.iter().all(|(id, pat)| {
                        fields
                            .get(&id.name)
                            .map(|v| self.pattern_matches(pat, v))
                            .unwrap_or(false)
                    })
                }
                Value::Object(n, fields) if n == &s.type_name.name => {
                    let f = fields.borrow();
                    s.fields.iter().all(|(id, pat)| {
                        f.get(&id.name)
                            .map(|v| self.pattern_matches(pat, v))
                            .unwrap_or(false)
                    })
                }
                _ => false,
            },
            Pattern::EnumVariant(e) => {
                matches!(value, Value::Struct(n, _) if *n == format!("{}::{}", e.type_name.name, e.variant.name))
            }
            Pattern::Or(o) => o.patterns.iter().any(|p| self.pattern_matches(p, value)),
        }
    }

    fn reversed_value(&self, v: Value) -> Result<Value> {
        match v {
            Value::Str(s) => Ok(Value::Str(s.chars().rev().collect())),
            Value::List(mut l) => {
                l.reverse();
                Ok(Value::List(l))
            }
            Value::Tuple(mut t) => {
                t.reverse();
                Ok(Value::Tuple(t))
            }
            Value::Frozen(inner) => self.reversed_value(*inner),
            other => Err(self.runtime_error(format!(
                "reversed() does not support {}",
                self.type_name_of(&other)
            ))),
        }
    }

    fn sorted_value(&self, v: Value, desc: bool) -> Result<Value> {
        match v {
            Value::List(mut l) => {
                l.sort_by(|a, b| {
                    if desc {
                        interp_fmt(b).cmp(&interp_fmt(a))
                    } else {
                        interp_fmt(a).cmp(&interp_fmt(b))
                    }
                });
                Ok(Value::List(l))
            }
            Value::Frozen(inner) => self.sorted_value(*inner, desc),
            other => Err(self.runtime_error(format!(
                "sorted() does not support {}",
                self.type_name_of(&other)
            ))),
        }
    }

    fn call_list_method(
        &mut self,
        list: &mut Vec<Value>,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        match name {
            "len" => Ok(Value::Int(list.len() as i64)),
            "push" | "append" => {
                let v = args
                    .get(0)
                    .cloned()
                    .ok_or_else(|| self.runtime_error("push requires value"))?;
                list.push(v);
                Ok(Value::List(list.clone()))
            }
            "pop" => list
                .pop()
                .ok_or_else(|| self.runtime_error("pop from empty list")),
            "reverse" => {
                list.reverse();
                Ok(Value::List(list.clone()))
            }
            "sort" => {
                list.sort_by(|a, b| interp_fmt(a).cmp(&interp_fmt(b)));
                Ok(Value::List(list.clone()))
            }
            _ => Err(self.runtime_error(format!("list has no method '{}'", name))),
        }
    }

    fn call_dict_method(
        &mut self,
        dict: &mut HashMap<String, Value>,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        match name {
            "len" => Ok(Value::Int(dict.len() as i64)),
            "keys" => Ok(Value::List(dict.keys().cloned().map(Value::Str).collect())),
            "values" => Ok(Value::List(dict.values().cloned().collect())),
            "get" => {
                let key = args
                    .get(0)
                    .map(|v| self.value_to_key(v))
                    .ok_or_else(|| self.runtime_error("dict.get requires key"))?;
                Ok(dict
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| args.get(1).cloned().unwrap_or_else(|| Value::Null)))
            }
            "set" => {
                let key = args
                    .get(0)
                    .map(|v| self.value_to_key(v))
                    .ok_or_else(|| self.runtime_error("dict.set requires key"))?;
                let val = args
                    .get(1)
                    .cloned()
                    .ok_or_else(|| self.runtime_error("dict.set requires value"))?;
                dict.insert(key, val);
                Ok(Value::Dict(dict.clone()))
            }
            "has" | "contains" => {
                let key = args
                    .get(0)
                    .map(|v| self.value_to_key(v))
                    .ok_or_else(|| self.runtime_error("dict.has requires key"))?;
                Ok(Value::Bool(dict.contains_key(&key)))
            }
            _ => Err(self.runtime_error(format!("dict has no method '{}'", name))),
        }
    }

    fn call_set_method(
        &mut self,
        set: &mut HashMap<String, Value>,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value> {
        match name {
            "len" => Ok(Value::Int(set.len() as i64)),
            "add" => {
                let val = args
                    .get(0)
                    .cloned()
                    .ok_or_else(|| self.runtime_error("set.add requires value"))?;
                set.insert(self.value_to_key(&val), val);
                Ok(Value::Set(set.clone()))
            }
            "contains" => {
                let val = args
                    .get(0)
                    .ok_or_else(|| self.runtime_error("set.contains requires value"))?;
                Ok(Value::Bool(set.contains_key(&self.value_to_key(val))))
            }
            _ => Err(self.runtime_error(format!("set has no method '{}'", name))),
        }
    }

    fn call_str_method(&self, s: &str, name: &str, args: Vec<Value>) -> Result<Value> {
        match name {
            "len" => Ok(Value::Int(s.chars().count() as i64)),
            "reverse" => Ok(Value::Str(s.chars().rev().collect())),
            "contains" => Ok(Value::Bool(
                args.get(0)
                    .map(|v| s.contains(&interp_fmt(v)))
                    .unwrap_or(false),
            )),
            "split" => {
                let sep = args.get(0).map(interp_fmt).unwrap_or_else(|| " ".into());
                Ok(Value::List(
                    s.split(&sep).map(|x| Value::Str(x.to_string())).collect(),
                ))
            }
            "trim" => Ok(Value::Str(s.trim().to_string())),
            _ => Err(self.runtime_error(format!("str has no method '{}'", name))),
        }
    }

    fn spawn_expr(&mut self, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Call(c) => {
                let func = self.expr(&c.callee)?;
                let args = self.collect_args(&c.args)?;
                Ok(self.spawn_task(func, args))
            }
            _ => {
                let func = self.expr(expr)?;
                Ok(self.spawn_task(func, vec![]))
            }
        }
    }

    fn spawn_task(&mut self, func: Value, args: Vec<Value>) -> Value {
        let id = self.next_coro_id;
        self.next_coro_id += 1;
        let task = CoroutineTask {
            id,
            func,
            args,
            state: CoroutineState::Ready,
            last: Value::Null,
        };
        self.tasks.insert(id, task);
        self.ready_queue.push_back(id);
        Value::Coroutine(id)
    }

    fn resume_value(&mut self, handle: Value, _values: Vec<Value>) -> Result<Value> {
        match handle {
            Value::Coroutine(id) => {
                // Legacy coroutine: run one step.
                self.run_one_coroutine(id)
            }
            Value::Iterator(it) => {
                // Generator: return the next yielded value, or 0 if exhausted.
                let next = it.borrow_mut().next();
                match next {
                    Value::StopIteration => Ok(Value::Int(0)),
                    v => Ok(v),
                }
            }
            other => Err(self.runtime_error(format!(
                "resume expects coroutine, got {}",
                self.type_name_of(&other)
            ))),
        }
    }

    fn await_value(&mut self, value: Value) -> Result<Value> {
        if self.has_method(&value, "__await__", 0) {
            let awaited = self.call_method(value, "__await__", vec![])?;
            return self.await_value(awaited);
        }
        match value {
            Value::Coroutine(id) => {
                let mut safety = 0usize;
                loop {
                    safety += 1;
                    if safety > 100_000 {
                        return Err(self.runtime_error("await exceeded scheduler safety limit"));
                    }
                    let out = self.run_one_coroutine(id)?;
                    match self.tasks.get(&id).map(|t| t.state.clone()) {
                        Some(CoroutineState::Finished) => return Ok(out),
                        Some(CoroutineState::WaitingChannel(_)) => {
                            self.run_ready_coroutines()?;
                        }
                        Some(CoroutineState::Ready) => {}
                        None => {
                            return Err(self.runtime_error(format!("coroutine {} not found", id)))
                        }
                    }
                }
            }
            other => Ok(other),
        }
    }

    fn run_ready_coroutines(&mut self) -> Result<()> {
        let mut safety = 0usize;
        while let Some(id) = self.ready_queue.pop_front() {
            safety += 1;
            if safety > 100_000 {
                return Err(self.runtime_error("coroutine scheduler exceeded safety limit"));
            }
            let _ = self.run_one_coroutine(id)?;
        }
        Ok(())
    }

    fn run_one_coroutine(&mut self, id: usize) -> Result<Value> {
        let (func, args) = match self.tasks.get(&id) {
            Some(t) if t.state == CoroutineState::Finished => return Ok(t.last.clone()),
            Some(t) => (t.func.clone(), t.args.clone()),
            None => return Err(self.runtime_error(format!("coroutine {} not found", id))),
        };
        self.current_coro = Some(id);
        let result = self.call_value(func, args);
        self.current_coro = None;
        match result {
            Ok(v) => {
                if let Some(t) = self.tasks.get_mut(&id) {
                    t.last = v.clone();
                    t.state = CoroutineState::Finished;
                }
                Ok(v)
            }
            Err(e) => {
                let msg = e.message.clone();
                if let Some(ch_id) = msg
                    .strip_prefix("__vredrs_wait_channel:")
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    if let Some(t) = self.tasks.get_mut(&id) {
                        t.state = CoroutineState::WaitingChannel(ch_id);
                    }
                    Ok(Value::Coroutine(id))
                } else {
                    Err(e)
                }
            }
        }
    }

    fn wake_channel_receivers(&mut self, ch: usize) {
        if let Some(channel) = self.channels.get_mut(ch) {
            while let Some(id) = channel.waiting_receivers.pop_front() {
                if let Some(task) = self.tasks.get_mut(&id) {
                    if matches!(task.state, CoroutineState::WaitingChannel(c) if c == ch) {
                        task.state = CoroutineState::Ready;
                        self.ready_queue.push_back(id);
                    }
                }
            }
        }
    }

    fn channel_send_value(&mut self, ch: Value, value: Value) -> Result<()> {
        let id = match ch {
            Value::Channel(id) => id,
            Value::Int(id) if id >= 0 => id as usize,
            other => {
                return Err(self.runtime_error(format!(
                    "send expects channel, got {}",
                    self.type_name_of(&other)
                )))
            }
        };
        if id >= self.channels.len() {
            return Err(self.runtime_error(format!("channel {} not found", id)));
        }
        {
            let channel = &mut self.channels[id];
            if channel.closed {
                return Err(CompilerError::codegen_error("send on closed channel"));
            }
            channel.queue.push_back(value);
        }
        self.wake_channel_receivers(id);
        Ok(())
    }

    fn channel_try_receive(&mut self, ch: Value) -> Result<Option<Value>> {
        let id = match ch {
            Value::Channel(id) => id,
            Value::Int(id) if id >= 0 => id as usize,
            other => {
                return Err(self.runtime_error(format!(
                    "receive expects channel, got {}",
                    self.type_name_of(&other)
                )))
            }
        };
        if id >= self.channels.len() {
            return Err(self.runtime_error(format!("channel {} not found", id)));
        }
        Ok(self.channels[id].queue.pop_front())
    }

    fn channel_receive_value(&mut self, ch: Value) -> Result<Value> {
        let id = match ch {
            Value::Channel(id) => id,
            Value::Int(id) if id >= 0 => id as usize,
            other => {
                return Err(self.runtime_error(format!(
                    "receive expects channel, got {}",
                    self.type_name_of(&other)
                )))
            }
        };
        if id >= self.channels.len() {
            return Err(self.runtime_error(format!("channel {} not found", id)));
        }
        if let Some(v) = self.channels[id].queue.pop_front() {
            return Ok(v);
        }
        if let Some(coro_id) = self.current_coro {
            self.channels[id].waiting_receivers.push_back(coro_id);
            return Err(CompilerError::codegen_error(format!(
                "__vredrs_wait_channel:{}",
                id
            )));
        }
        Err(self.runtime_error(format!("channel {} has no value available", id)))
    }

    fn error_value_from(&self, err: &CompilerError) -> Value {
        // If the error message is a plain string (from `throw, "msg"`),
        // bind it as a Str so the catch variable shows the text directly.
        // Otherwise wrap in RuntimeException for structured access.
        if !err.message.contains("\n") && !err.message.contains("stack backtrace") {
            Value::Str(err.message.clone())
        } else {
            Value::Error(RuntimeException::new(
                err.message.clone(),
                self.call_stack.clone(),
            ))
        }
    }

    fn throw_value(&self, value: Value) -> CompilerError {
        match value {
            Value::Error(e) => CompilerError::codegen_error(e.render()),
            other => CompilerError::codegen_error(
                RuntimeException::new(interp_fmt(&other), self.call_stack.clone()).render(),
            ),
        }
    }

    fn runtime_error(&self, message: impl Into<String>) -> CompilerError {
        CompilerError::codegen_error(
            RuntimeException::new(message, self.call_stack.clone()).render(),
        )
    }

    // Native functions -------------------------------------------------------
    fn native_len(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let v = args
            .get(0)
            .cloned()
            .ok_or_else(|| this.runtime_error("len requires one argument"))?;
        Ok(Value::Int(this.len_value(v)? as i64))
    }
    fn native_str(_this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Ok(Value::Str(args.get(0).map(interp_fmt).unwrap_or_default()))
    }
    fn native_int(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Ok(Value::Int(
            args.get(0)
                .ok_or_else(|| this.runtime_error("int requires one argument"))?
                .to_int()?,
        ))
    }
    fn native_float(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Ok(Value::Float(
            args.get(0)
                .ok_or_else(|| this.runtime_error("float requires one argument"))?
                .to_float()?,
        ))
    }
    fn native_bool(_this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Ok(Value::Bool(args.get(0).map(interp_truthy).unwrap_or(false)))
    }
    fn native_type_of(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Ok(Value::Str(
            args.get(0)
                .map(|v| this.type_name_of(v))
                .unwrap_or_else(|| "null".into()),
        ))
    }
    fn native_range(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let (start, end, step) = match args.len() {
            0 => (0, 0, 1),
            1 => (0, args[0].to_int()?, 1),
            2 => (args[0].to_int()?, args[1].to_int()?, 1),
            _ => (args[0].to_int()?, args[1].to_int()?, args[2].to_int()?),
        };
        if step == 0 {
            return Err(this.runtime_error("range step cannot be zero"));
        }
        let mut out = vec![];
        let mut i = start;
        if step > 0 {
            while i < end {
                out.push(Value::Int(i));
                i += step;
            }
        } else {
            while i > end {
                out.push(Value::Int(i));
                i += step;
            }
        }
        Ok(Value::List(out))
    }
    fn native_enumerate(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let mut iter = this.make_iterator(
            args.get(0)
                .cloned()
                .ok_or_else(|| this.runtime_error("enumerate requires iterable"))?,
        )?;
        let mut out = vec![];
        let mut i = 0;
        loop {
            let n = this.next_value(&mut iter)?;
            if matches!(n, Value::StopIteration) {
                break;
            }
            out.push(Value::Tuple(vec![Value::Int(i), n]));
            i += 1;
        }
        Ok(Value::List(out))
    }
    fn native_zip(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let mut iters = args
            .into_iter()
            .map(|v| this.make_iterator(v))
            .collect::<Result<Vec<_>>>()?;
        let mut out = vec![];
        loop {
            let mut row = vec![];
            for it in iters.iter_mut() {
                let n = this.next_value(it)?;
                if matches!(n, Value::StopIteration) {
                    return Ok(Value::List(out));
                }
                row.push(n);
            }
            out.push(Value::Tuple(row));
        }
    }
    fn native_map(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let func = args
            .get(0)
            .cloned()
            .ok_or_else(|| this.runtime_error("map requires function"))?;
        let mut iter = this.make_iterator(
            args.get(1)
                .cloned()
                .ok_or_else(|| this.runtime_error("map requires iterable"))?,
        )?;
        let mut out = vec![];
        loop {
            let n = this.next_value(&mut iter)?;
            if matches!(n, Value::StopIteration) {
                break;
            }
            out.push(this.call_value(func.clone(), vec![n])?);
        }
        Ok(Value::List(out))
    }
    fn native_filter(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let func = args
            .get(0)
            .cloned()
            .ok_or_else(|| this.runtime_error("filter requires function"))?;
        let mut iter = this.make_iterator(
            args.get(1)
                .cloned()
                .ok_or_else(|| this.runtime_error("filter requires iterable"))?,
        )?;
        let mut out = vec![];
        loop {
            let n = this.next_value(&mut iter)?;
            if matches!(n, Value::StopIteration) {
                break;
            }
            if interp_truthy(&this.call_value(func.clone(), vec![n.clone()])?) {
                out.push(n);
            }
        }
        Ok(Value::List(out))
    }
    fn native_sum(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let mut iter = this.make_iterator(
            args.get(0)
                .cloned()
                .ok_or_else(|| this.runtime_error("sum requires iterable"))?,
        )?;
        let mut acc = Value::Int(0);
        loop {
            let n = this.next_value(&mut iter)?;
            if matches!(n, Value::StopIteration) {
                break;
            }
            acc = this.binary_values(acc, BinaryOp::Add, n)?;
        }
        Ok(acc)
    }
    fn native_min(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        this.minmax(args, false)
    }
    fn native_max(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        this.minmax(args, true)
    }
    fn minmax(&mut self, args: Vec<Value>, max: bool) -> Result<Value> {
        let mut iter = self.make_iterator(
            args.get(0)
                .cloned()
                .ok_or_else(|| self.runtime_error("min/max requires iterable"))?,
        )?;
        let mut best = self.next_value(&mut iter)?;
        if matches!(best, Value::StopIteration) {
            return Err(self.runtime_error("min/max of empty iterable"));
        }
        loop {
            let n = self.next_value(&mut iter)?;
            if matches!(n, Value::StopIteration) {
                break;
            }
            let cmp = self.compare_values(
                n.clone(),
                if max { BinaryOp::Gt } else { BinaryOp::Lt },
                best.clone(),
            )?;
            if interp_truthy(&cmp) {
                best = n;
            }
        }
        Ok(best)
    }
    fn native_sorted(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let v = args
            .get(0)
            .cloned()
            .ok_or_else(|| this.runtime_error("sorted requires iterable"))?;
        this.sorted_value(v, false)
    }
    fn native_reversed(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let v = args
            .get(0)
            .cloned()
            .ok_or_else(|| this.runtime_error("reversed requires iterable"))?;
        this.reversed_value(v)
    }
    fn native_print(_this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        print!(
            "{}",
            args.iter().map(interp_fmt).collect::<Vec<_>>().join("")
        );
        Ok(Value::Null)
    }
    fn native_println(_this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        println!(
            "{}",
            args.iter().map(interp_fmt).collect::<Vec<_>>().join("")
        );
        Ok(Value::Null)
    }
    fn native_input(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        if let Some(prompt) = args.first() {
            print!("{}", interp_fmt(prompt));
            io::stdout().flush().ok();
        }
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| this.runtime_error(format!("input failed: {}", e)))?;
        Ok(Value::Str(line.trim_end_matches(['\n', '\r']).to_string()))
    }
    fn native_open(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let path = args
            .get(0)
            .and_then(|v| {
                if let Value::Str(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| this.runtime_error("open requires path string"))?;
        let mode = args.get(1).map(interp_fmt).unwrap_or_else(|| "r".into());
        let mut opts = OpenOptions::new();
        match mode.as_str() {
            "r" => {
                opts.read(true);
            }
            "w" => {
                opts.write(true).create(true).truncate(true);
            }
            "a" => {
                opts.append(true).create(true);
            }
            "r+" => {
                opts.read(true).write(true);
            }
            "w+" => {
                opts.read(true).write(true).create(true).truncate(true);
            }
            _ => return Err(this.runtime_error(format!("unsupported file mode '{}'", mode))),
        }
        let file = opts
            .open(&path)
            .map_err(|e| this.runtime_error(format!("open '{}': {}", path, e)))?;
        let id = this.files.len();
        this.files.push(RuntimeFile {
            file,
            path,
            closed: false,
        });
        Ok(Value::FileHandle(id))
    }
    fn native_read(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        match args
            .get(0)
            .cloned()
            .ok_or_else(|| this.runtime_error("read requires file handle or path"))?
        {
            Value::FileHandle(id) => {
                if id >= this.files.len() {
                    return Err(this.runtime_error("invalid file handle"));
                }
                if this.files[id].closed {
                    let path = this.files[id].path.clone();
                    return Err(this.runtime_error(format!("read '{}': file closed", path)));
                }
                let path = this.files[id].path.clone();
                let mut s = String::new();
                match this.files[id].file.read_to_string(&mut s) {
                    Ok(_) => Ok(Value::Str(s)),
                    Err(e) => Err(this.runtime_error(format!("read '{}': {}", path, e))),
                }
            }
            Value::Str(path) => fs::read_to_string(&path)
                .map(Value::Str)
                .map_err(|e| this.runtime_error(format!("read '{}': {}", path, e))),
            other => Err(this.runtime_error(format!(
                "read expects file or path, got {}",
                this.type_name_of(&other)
            ))),
        }
    }
    fn native_write(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let data = args
            .get(1)
            .map(interp_fmt)
            .ok_or_else(|| this.runtime_error("write requires data"))?;
        match args
            .get(0)
            .cloned()
            .ok_or_else(|| this.runtime_error("write requires file handle or path"))?
        {
            Value::FileHandle(id) => {
                if id >= this.files.len() {
                    return Err(this.runtime_error("invalid file handle"));
                }
                if this.files[id].closed {
                    let path = this.files[id].path.clone();
                    return Err(this.runtime_error(format!("write '{}': file closed", path)));
                }
                let path = this.files[id].path.clone();
                match this.files[id].file.write_all(data.as_bytes()) {
                    Ok(_) => Ok(Value::Int(data.len() as i64)),
                    Err(e) => Err(this.runtime_error(format!("write '{}': {}", path, e))),
                }
            }
            Value::Str(path) => {
                fs::write(&path, data.as_bytes())
                    .map_err(|e| this.runtime_error(format!("write '{}': {}", path, e)))?;
                Ok(Value::Int(data.len() as i64))
            }
            other => Err(this.runtime_error(format!(
                "write expects file or path, got {}",
                this.type_name_of(&other)
            ))),
        }
    }
    fn native_close(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let id = match args.get(0) {
            Some(Value::FileHandle(id)) => *id,
            _ => return Err(this.runtime_error("close requires file handle")),
        };
        if id >= this.files.len() {
            return Err(this.runtime_error("invalid file handle"));
        }
        this.files[id].closed = true;
        Ok(Value::Bool(true))
    }
    fn native_exit(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let code = args.get(0).map(|v| v.to_int()).transpose()?.unwrap_or(0);
        Err(this.runtime_error(format!("exit({})", code)))
    }
    fn native_dict(_this: &mut Interpreter, _args: Vec<Value>) -> Result<Value> {
        Ok(Value::Dict(HashMap::new()))
    }
    fn native_dict_get(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let d = match args.get(0) {
            Some(Value::Dict(d)) => d,
            _ => return Err(this.runtime_error("dict_get requires dict")),
        };
        let key = args
            .get(1)
            .map(|v| this.value_to_key(v))
            .ok_or_else(|| this.runtime_error("dict_get requires key"))?;
        Ok(d.get(&key)
            .cloned()
            .unwrap_or_else(|| args.get(2).cloned().unwrap_or_else(|| Value::Null)))
    }
    fn native_dict_set(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let mut d = match args.get(0).cloned() {
            Some(Value::Dict(d)) => d,
            _ => return Err(this.runtime_error("dict_set requires dict")),
        };
        let key = args
            .get(1)
            .map(|v| this.value_to_key(v))
            .ok_or_else(|| this.runtime_error("dict_set requires key"))?;
        let val = args
            .get(2)
            .cloned()
            .ok_or_else(|| this.runtime_error("dict_set requires value"))?;
        d.insert(key, val);
        Ok(Value::Dict(d))
    }
    fn native_dict_keys(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        match args.get(0) {
            Some(Value::Dict(d)) => Ok(Value::List(d.keys().cloned().map(Value::Str).collect())),
            _ => Err(this.runtime_error("dict_keys requires dict")),
        }
    }
    fn native_dict_values(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        match args.get(0) {
            Some(Value::Dict(d)) => Ok(Value::List(d.values().cloned().collect())),
            _ => Err(this.runtime_error("dict_values requires dict")),
        }
    }
    fn native_dict_has(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let d = match args.get(0) {
            Some(Value::Dict(d)) => d,
            _ => return Err(this.runtime_error("dict_has requires dict")),
        };
        let key = args
            .get(1)
            .map(|v| this.value_to_key(v))
            .ok_or_else(|| this.runtime_error("dict_has requires key"))?;
        Ok(Value::Bool(d.contains_key(&key)))
    }
    fn native_read_file(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Self::native_read(this, args)
    }
    fn native_write_file(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Self::native_write(this, args).map(|_| Value::Bool(true))
    }
    fn native_file_exists(_this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Ok(Value::Bool(
            args.get(0)
                .map(interp_fmt)
                .map(|p| Path::new(&p).exists())
                .unwrap_or(false),
        ))
    }
    fn native_channel(this: &mut Interpreter, _args: Vec<Value>) -> Result<Value> {
        let id = this.channels.len();
        this.channels.push(RuntimeChannel {
            queue: VecDeque::new(),
            closed: false,
            waiting_receivers: VecDeque::new(),
        });
        Ok(Value::Channel(id))
    }
    fn native_send(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let ch = args
            .get(0)
            .cloned()
            .ok_or_else(|| this.runtime_error("send requires channel"))?;
        let val = args
            .get(1)
            .cloned()
            .ok_or_else(|| this.runtime_error("send requires value"))?;
        this.channel_send_value(ch, val)?;
        Ok(Value::Bool(true))
    }
    fn native_receive(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let ch = args
            .get(0)
            .cloned()
            .ok_or_else(|| this.runtime_error("receive requires channel"))?;
        this.channel_receive_value(ch)
    }
    fn native_spawn(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let func = args
            .get(0)
            .cloned()
            .ok_or_else(|| this.runtime_error("spawn requires function"))?;
        Ok(this.spawn_task(func, args.into_iter().skip(1).collect()))
    }
    fn native_resume(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let h = args
            .get(0)
            .cloned()
            .ok_or_else(|| this.runtime_error("resume requires coroutine"))?;
        this.resume_value(h, args.into_iter().skip(1).collect())
    }
    fn native_scheduler_run(this: &mut Interpreter, _args: Vec<Value>) -> Result<Value> {
        this.run_ready_coroutines()?;
        Ok(Value::Null)
    }
    fn native_iter(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let v = args
            .get(0)
            .cloned()
            .ok_or_else(|| this.runtime_error("iter requires value"))?;
        this.make_iterator(v)
    }
    fn native_next(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let mut v = args
            .get(0)
            .cloned()
            .ok_or_else(|| this.runtime_error("next requires iterator"))?;
        this.next_value(&mut v)
    }
    fn native_stop(_this: &mut Interpreter, _args: Vec<Value>) -> Result<Value> {
        Ok(Value::StopIteration)
    }
    fn annotations_to_value(&mut self, annotations: &[Annotation]) -> Result<Value> {
        let mut out = vec![];
        for ann in annotations {
            let mut d = HashMap::new();
            d.insert("name".to_string(), Value::Str(ann.name.clone()));
            let mut args = vec![];
            for a in &ann.arguments {
                let mut ad = HashMap::new();
                ad.insert(
                    "name".to_string(),
                    a.name.clone().map(Value::Str).unwrap_or(Value::Null),
                );
                let val = self
                    .annotation_arg_value(&a.value)
                    .unwrap_or_else(|_| Value::Str("<expr>".to_string()));
                ad.insert("value".to_string(), val);
                args.push(Value::Dict(ad));
            }
            d.insert("args".to_string(), Value::List(args));
            out.push(Value::Dict(d));
        }
        Ok(Value::List(out))
    }

    fn annotation_arg_value(&mut self, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Integer(i) => Ok(Value::Int(i.value)),
            Expr::Float(f) => Ok(Value::Float(f.value)),
            Expr::String_(s) => self.string_parts(&s.parts),
            Expr::MultiLineString(s) => self.string_parts(&s.parts),
            Expr::Bool(b) => Ok(Value::Bool(b.value)),
            Expr::Null(_) => Ok(Value::Null),
            _ => self.expr(expr),
        }
    }

    fn native_hash(_this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        use std::hash::{Hash, Hasher};
        let s = args.get(0).map(interp_fmt).unwrap_or_default();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        s.hash(&mut h);
        Ok(Value::Str(format!("{:016x}", h.finish())))
    }

    fn native_freeze(_this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Ok(Value::Frozen(Box::new(
            args.get(0).cloned().unwrap_or(Value::Null),
        )))
    }
    fn native_is_frozen(_this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Ok(Value::Bool(matches!(args.get(0), Some(Value::Frozen(_)))))
    }
    fn native_set_recursion_limit(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let n = args
            .get(0)
            .ok_or_else(|| this.runtime_error("set_recursion_limit requires an integer"))?
            .to_int()?;
        if n < 16 {
            return Err(this.runtime_error("recursion limit must be at least 16"));
        }
        this.max_depth = n as usize;
        Ok(Value::Int(n))
    }
    fn native_annotations(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        if args.is_empty() {
            // No-arg form: return a dict mapping each annotated name to its
            // flat annotation dict (see below).
            let mut out = HashMap::new();
            for (name, anns) in this.annotation_meta.clone() {
                out.insert(name, Self::flat_annotations_dict(this, &anns)?);
            }
            return Ok(Value::Dict(out));
        }
        let name = match args.get(0).cloned().unwrap_or(Value::Null) {
            Value::Str(s) => s,
            Value::Function(n, _, _, _) | Value::AsyncFunction(n, _, _, _) => n,
            Value::BoundMethod(_, m) => format!("{}.{}", m.owner, m.name),
            other => {
                return Err(this.runtime_error(format!(
                    "annotations expects name/function/method, got {}",
                    this.type_name_of(&other)
                )))
            }
        };
        let anns = this.annotation_meta.get(&name).cloned().unwrap_or_default();
        Self::flat_annotations_dict(this, &anns)
    }

    /// Build a flat dict from a function's annotation list, matching the
    /// native backend's `gen_annotations_dict` semantics: each annotation
    /// contributes a key (its name) with the first positional argument as
    /// the value. Annotations with no args map to `true`.
    fn flat_annotations_dict(
        this: &mut Interpreter,
        anns: &[Annotation],
    ) -> Result<Value> {
        let mut out: HashMap<String, Value> = HashMap::new();
        for ann in anns {
            if ann.arguments.is_empty() {
                out.insert(ann.name.clone(), Value::Bool(true));
                continue;
            }
            let val = this
                .annotation_arg_value(&ann.arguments[0].value)
                .unwrap_or_else(|_| Value::Str("<expr>".to_string()));
            // If the key already exists, convert to a list of values.
            match out.get(&ann.name) {
                Some(Value::List(_)) => {
                    if let Some(Value::List(v)) = out.get_mut(&ann.name) {
                        v.push(val);
                    }
                }
                Some(existing) => {
                    let existing = existing.clone();
                    out.insert(ann.name.clone(), Value::List(vec![existing, val]));
                }
                None => {
                    out.insert(ann.name.clone(), val);
                }
            }
        }
        Ok(Value::Dict(out))
    }

    fn native_read_dir(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let path = args
            .get(0)
            .map(interp_fmt)
            .ok_or_else(|| this.runtime_error("read_dir requires path"))?;
        let mut out = vec![];
        let entries = fs::read_dir(&path)
            .map_err(|e| this.runtime_error(format!("read_dir '{}': {}", path, e)))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| this.runtime_error(format!("read_dir '{}': {}", path, e)))?;
            out.push(Value::Str(entry.path().to_string_lossy().to_string()));
        }
        Ok(Value::List(out))
    }
    fn native_is_dir(_this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Ok(Value::Bool(
            args.get(0)
                .map(interp_fmt)
                .map(|p| Path::new(&p).is_dir())
                .unwrap_or(false),
        ))
    }
    fn native_is_file(_this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Ok(Value::Bool(
            args.get(0)
                .map(interp_fmt)
                .map(|p| Path::new(&p).is_file())
                .unwrap_or(false),
        ))
    }
    fn native_path_join(_this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let mut p = PathBuf::new();
        for a in args {
            p.push(interp_fmt(&a));
        }
        Ok(Value::Str(p.to_string_lossy().to_string()))
    }
    fn native_basename(_this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let p = args.get(0).map(interp_fmt).unwrap_or_default();
        Ok(Value::Str(
            Path::new(&p)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string(),
        ))
    }
    fn native_dirname(_this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let p = args.get(0).map(interp_fmt).unwrap_or_default();
        Ok(Value::Str(
            Path::new(&p)
                .parent()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
        ))
    }
    fn native_math_sqrt(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Ok(Value::Float(
            args.get(0)
                .ok_or_else(|| this.runtime_error("sqrt requires number"))?
                .to_float()?
                .sqrt(),
        ))
    }
    fn native_math_pow(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Ok(Value::Float(
            args.get(0)
                .ok_or_else(|| this.runtime_error("pow requires base"))?
                .to_float()?
                .powf(
                    args.get(1)
                        .ok_or_else(|| this.runtime_error("pow requires exponent"))?
                        .to_float()?,
                ),
        ))
    }
    fn native_math_sin(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        Ok(Value::Float(
            args.get(0)
                .ok_or_else(|| this.runtime_error("sin requires number"))?
                .to_float()?
                .sin(),
        ))
    }
    fn native_os_args(_this: &mut Interpreter, _args: Vec<Value>) -> Result<Value> {
        Ok(Value::List(std::env::args().map(Value::Str).collect()))
    }
    fn native_os_env(_this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        if let Some(Value::Str(k)) = args.get(0) {
            Ok(std::env::var(k)
                .map(Value::Str)
                .unwrap_or_else(|_| Value::Null))
        } else {
            Ok(Value::Dict(
                std::env::vars().map(|(k, v)| (k, Value::Str(v))).collect(),
            ))
        }
    }
    fn native_time_sleep(this: &mut Interpreter, args: Vec<Value>) -> Result<Value> {
        let ms = args
            .get(0)
            .ok_or_else(|| this.runtime_error("sleep requires milliseconds"))?
            .to_int()?
            .max(0) as u64;
        std::thread::sleep(std::time::Duration::from_millis(ms));
        Ok(Value::Null)
    }
    fn native_time_now(this: &mut Interpreter, _args: Vec<Value>) -> Result<Value> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| this.runtime_error(format!("time error: {}", e)))?;
        Ok(Value::Float(now.as_secs_f64()))
    }
}

impl Value {
    fn to_int(&self) -> Result<i64> {
        match self {
            Value::Int(v) => Ok(*v),
            Value::Float(f) => Ok(*f as i64),
            Value::Bool(true) => Ok(1),
            Value::Bool(false) => Ok(0),
            Value::Str(s) => s.parse::<i64>().map_err(|_| {
                CompilerError::codegen_error(format!("cannot convert '{}' to int", s))
            }),
            Value::Frozen(inner) => inner.to_int(),
            other => Err(CompilerError::codegen_error(format!(
                "cannot convert {} to int",
                interp_fmt(other)
            ))),
        }
    }
    fn to_float(&self) -> Result<f64> {
        match self {
            Value::Int(v) => Ok(*v as f64),
            Value::Float(f) => Ok(*f),
            Value::Bool(true) => Ok(1.0),
            Value::Bool(false) => Ok(0.0),
            Value::Str(s) => s.parse::<f64>().map_err(|_| {
                CompilerError::codegen_error(format!("cannot convert '{}' to float", s))
            }),
            Value::Frozen(inner) => inner.to_float(),
            other => Err(CompilerError::codegen_error(format!(
                "cannot convert {} to float",
                interp_fmt(other)
            ))),
        }
    }
    fn to_index(&self) -> Result<usize> {
        let n = self.to_int()?;
        if n < 0 {
            Err(CompilerError::codegen_error(format!(
                "negative index {}",
                n
            )))
        } else {
            Ok(n as usize)
        }
    }
}


/// Check if a statement list contains a yield statement (making it a generator).
fn body_has_yield(body: &[Stmt]) -> bool {
    body.iter().any(stmt_has_yield)
}

fn stmt_has_yield(s: &Stmt) -> bool {
    match s {
        Stmt::Yield(_) => true,
        Stmt::If(i) => {
            i.then_body.iter().any(stmt_has_yield)
                || i.elif_chain.iter().any(|(_, b)| b.iter().any(stmt_has_yield))
                || i.else_body.as_ref().map(|b| b.iter().any(stmt_has_yield)).unwrap_or(false)
        }
        Stmt::While(w) => w.body.iter().any(stmt_has_yield),
        Stmt::ForIn(f) => f.body.iter().any(stmt_has_yield),
        Stmt::ForRange(f) => f.body.iter().any(stmt_has_yield),
        Stmt::Loop(l) => l.body.iter().any(stmt_has_yield),
        Stmt::Try(t) => {
            t.try_body.iter().any(stmt_has_yield)
                || t.catch_body.as_ref().map(|b| b.iter().any(stmt_has_yield)).unwrap_or(false)
        }
        Stmt::With(w) => w.body.iter().any(stmt_has_yield),
        Stmt::Match(m) => m.cases.iter().any(|c| c.body.iter().any(stmt_has_yield)),
        Stmt::Defer(d) => stmt_has_yield(&d.stmt),
        _ => false,
    }
}

/// Interpret C-style escape sequences in a string literal's text content.
/// This mirrors the bytecode VM's `interpret_escapes` so both execution
/// engines handle `\n`, `\t`, `\r`, `\0`, `\\`, `\"`, `\'` identically.
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
