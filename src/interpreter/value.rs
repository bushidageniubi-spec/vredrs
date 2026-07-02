//! Value types for the Vredrs interpreter.
//!
//! Defines the `Value` enum, `RuntimeMethod`, `RuntimeException`,
//! `RuntimeIterator`, and helper functions `interp_truthy` / `interp_fmt`.

use crate::error::Result;
use crate::parser::ast::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::Interpreter;

/// A native function pointer that takes an interpreter and arguments.
pub type NativeFn = fn(&mut Interpreter, Vec<Value>) -> Result<Value>;

/// A compiled method bound to a class.
#[derive(Debug, Clone)]
pub struct RuntimeMethod {
    pub name: String,
    pub params: Vec<(String, bool)>,
    pub body: Vec<Stmt>,
    pub owner: String,
}

/// The tagged-union value type used throughout the interpreter.
#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Dict(HashMap<String, Value>),
    Set(HashMap<String, Value>),
    Struct(String, HashMap<String, Value>),
    Object(String, Rc<RefCell<HashMap<String, Value>>>),
    Class(String),
    Module(String, HashMap<String, Value>),
    Function(String, Vec<(String, bool)>, Vec<Stmt>, Vec<HashMap<String, Value>>),
    AsyncFunction(String, Vec<(String, bool)>, Vec<Stmt>, Vec<HashMap<String, Value>>),
    BoundMethod(Box<Value>, RuntimeMethod),
    Native(NativeFn),
    Nf(fn(Vec<Value>) -> Value),
    Channel(usize),
    Coroutine(usize),
    FileHandle(usize),
    Iterator(Rc<RefCell<RuntimeIterator>>),
    Error(RuntimeException),
    Frozen(Box<Value>),
    StopIteration,
}

/// A runtime exception with backtrace and optional cause chain.
#[derive(Debug, Clone)]
pub struct RuntimeException {
    pub message: String,
    pub backtrace: Vec<String>,
    pub cause: Option<Box<RuntimeException>>,
}

impl RuntimeException {
    pub fn new(message: impl Into<String>, backtrace: Vec<String>) -> Self {
        RuntimeException { message: message.into(), backtrace, cause: None }
    }

    pub fn chained(message: impl Into<String>, backtrace: Vec<String>, cause: RuntimeException) -> Self {
        RuntimeException { message: message.into(), backtrace, cause: Some(Box::new(cause)) }
    }

    pub fn render(&self) -> String {
        let mut out = self.message.clone();
        if !self.backtrace.is_empty() {
            out.push_str("\nstack backtrace:");
            for frame in self.backtrace.iter().rev() {
                out.push_str("\n  at ");
                out.push_str(frame);
            }
        }
        let mut cause = self.cause.as_ref();
        while let Some(c) = cause {
            out.push_str("\ncaused by: ");
            out.push_str(&c.message);
            cause = c.cause.as_ref();
        }
        out
    }
}

/// An iterator over a vector of values.
#[derive(Debug, Clone)]
pub struct RuntimeIterator {
    pub items: Vec<Value>,
    pub index: usize,
}

impl RuntimeIterator {
    pub fn new(items: Vec<Value>) -> Self {
        RuntimeIterator { items, index: 0 }
    }

    pub fn next(&mut self) -> Value {
        if self.index >= self.items.len() {
            Value::StopIteration
        } else {
            let v = self.items[self.index].clone();
            self.index += 1;
            v
        }
    }
}

/// A multi-producer / multi-consumer channel.
#[derive(Debug)]
pub struct RuntimeChannel {
    pub queue: std::collections::VecDeque<Value>,
    pub closed: bool,
    pub waiting_receivers: std::collections::VecDeque<usize>,
}

/// A coroutine task in the scheduler.
#[derive(Debug, Clone)]
pub struct CoroutineTask {
    pub id: usize,
    pub func: Value,
    pub args: Vec<Value>,
    pub state: CoroutineState,
    pub last: Value,
}

/// Coroutine lifecycle states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoroutineState {
    Ready,
    WaitingChannel(usize),
    Finished,
}

/// A file handle managed by the interpreter.
#[derive(Debug)]
pub struct RuntimeFile {
    pub file: std::fs::File,
    pub path: String,
    pub closed: bool,
}

/// Control flow signal returned by statement execution.
#[derive(Debug, Clone)]
pub enum ExecFlow {
    None,
    Value(Value),
    Return(Value),
    Break,
    Continue,
    Yield(Value),
}

/// Test truthiness of a value (Python-like semantics).
pub fn interp_truthy(v: &Value) -> bool {
    match v {
        Value::Null | Value::StopIteration => false,
        Value::Int(i) => *i != 0,
        Value::Float(f) => *f != 0.0,
        Value::Bool(b) => *b,
        Value::Str(s) => !s.is_empty(),
        Value::List(l) => !l.is_empty(),
        Value::Tuple(t) => !t.is_empty(),
        Value::Dict(d) => !d.is_empty(),
        Value::Set(s) => !s.is_empty(),
        _ => true,
    }
}

/// Format a value for display (print/paste).
pub fn interp_fmt(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            if *f == (*f as i64) as f64 && f.is_finite() {
                format!("{}", *f as i64)
            } else {
                format!("{}", f)
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::Str(s) => s.clone(),
        Value::List(l) => {
            let items: Vec<String> = l.iter().map(interp_fmt).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Tuple(t) => {
            let items: Vec<String> = t.iter().map(interp_fmt).collect();
            if items.len() == 1 {
                format!("({},)", items[0])
            } else {
                format!("({})", items.join(", "))
            }
        }
        Value::Dict(d) => {
            let items: Vec<String> = d.iter().map(|(k, v)| format!("{}: {}", k, interp_fmt(v))).collect();
            format!("{{{}}}", items.join(", "))
        }
        Value::Set(s) => {
            let items: Vec<String> = s.values().map(interp_fmt).collect();
            format!("{{{}}}", items.join(", "))
        }
        Value::Struct(name, fields) => {
            let items: Vec<String> = fields.iter().map(|(k, v)| format!("{}: {}", k, interp_fmt(v))).collect();
            format!("{} {{{}}}", name, items.join(", "))
        }
        Value::Object(name, fields) => {
            let f = fields.borrow();
            let items: Vec<String> = f.iter().map(|(k, v)| format!("{}: {}", k, interp_fmt(v))).collect();
            format!("<{} {}>", name, items.join(", "))
        }
        Value::Class(name) => format!("<class {}>", name),
        Value::Module(name, _) => format!("<module {}>", name),
        Value::Function(name, _, _, _) => format!("<fn {}>", name),
        Value::AsyncFunction(name, _, _, _) => format!("<async fn {}>", name),
        Value::BoundMethod(_, m) => format!("<method {}>", m.name),
        Value::Native(_) => "<native>".to_string(),
        Value::Nf(_) => "<native>".to_string(),
        Value::Channel(_) => "<channel>".to_string(),
        Value::Coroutine(_) => "<coroutine>".to_string(),
        Value::FileHandle(_) => "<file>".to_string(),
        Value::Iterator(_) => "<iterator>".to_string(),
        Value::Error(e) => format!("<error: {}>", e.message),
        Value::Frozen(inner) => format!("<frozen {}>", interp_fmt(inner)),
        Value::StopIteration => "<stop>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthy_int() {
        assert!(interp_truthy(&Value::Int(1)));
        assert!(!interp_truthy(&Value::Int(0)));
        assert!(!interp_truthy(&Value::Null));
        assert!(interp_truthy(&Value::Str("x".into())));
        assert!(!interp_truthy(&Value::Str("".into())));
        assert!(interp_truthy(&Value::List(vec![Value::Int(1)])));
        assert!(!interp_truthy(&Value::List(vec![])));
    }

    #[test]
    fn fmt_int_and_float() {
        assert_eq!(interp_fmt(&Value::Int(42)), "42");
        assert_eq!(interp_fmt(&Value::Float(3.14)), "3.14");
        assert_eq!(interp_fmt(&Value::Float(3.0)), "3");
        assert_eq!(interp_fmt(&Value::Bool(true)), "true");
        assert_eq!(interp_fmt(&Value::Null), "null");
    }

    #[test]
    fn fmt_list_and_tuple() {
        assert_eq!(interp_fmt(&Value::List(vec![Value::Int(1), Value::Int(2)])), "[1, 2]");
        assert_eq!(interp_fmt(&Value::Tuple(vec![Value::Int(42)])), "(42,)");
    }

    #[test]
    fn fmt_dict_and_set() {
        let mut d = HashMap::new();
        d.insert("a".into(), Value::Int(1));
        assert!(interp_fmt(&Value::Dict(d)).contains("a: 1"));
    }

    #[test]
    fn runtime_iterator_next() {
        let mut it = RuntimeIterator::new(vec![Value::Int(10), Value::Int(20)]);
        assert_eq!(interp_fmt(&it.next()), "10");
        assert_eq!(interp_fmt(&it.next()), "20");
        assert!(matches!(it.next(), Value::StopIteration));
    }
}
