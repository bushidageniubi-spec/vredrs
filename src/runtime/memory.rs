use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum RuntimeValue {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
    Array(Vec<RuntimeValue>),
    Dict(HashMap<String, RuntimeValue>),
    Function(fn(Vec<RuntimeValue>) -> RuntimeValue),
    Object(Rc<RefCell<ObjectHeader>>),
}

#[derive(Debug)]
pub struct ObjectHeader {
    pub fields: HashMap<String, RuntimeValue>,
    pub ref_count: usize,
}

pub fn arc_retain(obj: &Rc<RefCell<ObjectHeader>>) {
    obj.borrow_mut().ref_count += 1;
}

pub fn arc_release(obj: &Rc<RefCell<ObjectHeader>>) -> bool {
    let mut header = obj.borrow_mut();
    if header.ref_count > 0 { header.ref_count -= 1; }
    header.ref_count == 0
}

pub struct LinearResource {
    pub consumed: bool,
    pub data: Option<RuntimeValue>,
}

impl LinearResource {
    pub fn new(val: RuntimeValue) -> Self { LinearResource { consumed: false, data: Some(val) } }
    pub fn consume(&mut self) -> Option<RuntimeValue> {
        if self.consumed { None } else { self.consumed = true; self.data.take() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_arc_retain_release() {
        let obj = Rc::new(RefCell::new(ObjectHeader { fields: HashMap::new(), ref_count: 1 }));
        arc_retain(&obj);
        assert_eq!(obj.borrow().ref_count, 2);
        assert!(!arc_release(&obj));
        assert_eq!(obj.borrow().ref_count, 1);
        assert!(arc_release(&obj));
    }

    #[test]
    fn test_linear_consume() {
        let mut res = LinearResource::new(RuntimeValue::Int(42));
        assert_eq!(res.consume().map(|v| v.as_int()), Some(42));
        assert!(res.consume().is_none());
    }
}

impl RuntimeValue {
    pub fn as_int(&self) -> i64 { match self { RuntimeValue::Int(v) => *v, RuntimeValue::Float(f) => *f as i64, RuntimeValue::Bool(true) => 1, _ => 0 } }
    pub fn as_bool(&self) -> bool { match self { RuntimeValue::Bool(b) => *b, RuntimeValue::Int(v) => *v != 0, RuntimeValue::Null => false, _ => true } }
}

