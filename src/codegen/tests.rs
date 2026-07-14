// Tests for the bytecode VM.
//
// All tests run through the bytecode VM (the sole execution engine).
// The test helper `run_code` compiles and executes source code via the
// VM, then returns a `VmRunner` for inspecting global variables.

use crate::bytecode::{Compiler, VM, Value};
use crate::lexer::Lexer;
use crate::parser::Parser;
use std::collections::HashMap;
use std::fs;

/// A wrapper around the VM's globals for test inspection.
pub struct VmRunner {
    globals: HashMap<String, Value>,
}

impl VmRunner {
    /// Look up a variable by name.
    pub fn get(&self, name: &str) -> Value {
        self.globals.get(name).cloned().unwrap_or(Value::Null)
    }
}

/// Execute Vredrs code via the bytecode VM and return a VmRunner.
fn run_code(source: &str) -> Result<VmRunner, String> {
    let mut lexer = Lexer::new(source, 0);
    let tokens = lexer
        .tokenize()
        .map_err(|e| format!("Lex error: {:?}", e))?;
    let mut parser = Parser::new(tokens, 0);
    let program = parser
        .parse_program()
        .map_err(|e| format!("Parse error: {:?}", e))?;
    let compiler = Compiler::new();
    let module = compiler
        .compile(&program)
        .map_err(|e| format!("Compile error: {:?}", e))?;
    let mut vm = VM::new(module).with_program(program);
    vm.run().map_err(|e| format!("Runtime error: {:?}", e))?;
    Ok(VmRunner {
        globals: vm.globals_clone(),
    })
}

/// Execute code and get a variable value.
fn run_and_get(source: &str, var_name: &str) -> Result<Value, String> {
    let runner = run_code(source)?;
    Ok(runner.get(var_name))
}

#[cfg(test)]
mod dict_tests {
    use super::*;

    #[test]
    fn test_dict_creation_empty() {
        let source = "set, d = dict()";
        let result = run_and_get(source, "d");
        assert!(result.is_ok());
        if let Value::Dict(map) = result.unwrap() {
            assert_eq!(map.len(), 0);
        } else {
            panic!("Expected Dict value");
        }
    }

    #[test]
    fn test_dict_literal() {
        let source = r#"set, person = {"name": "Alice", "age": 30}"#;
        let result = run_and_get(source, "person");
        assert!(result.is_ok());
        if let Value::Dict(map) = result.unwrap() {
            assert_eq!(map.len(), 2);
            assert!(map.contains_key("name"));
            assert!(map.contains_key("age"));
        } else {
            panic!("Expected Dict value");
        }
    }

    #[test]
    fn test_dict_get_existing_key() {
        let source = r#"
set, d = {"x": 10, "y": 20}
set, val = dict_get(d, "x")
"#;
        let result = run_and_get(source, "val");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Int(10) => {}
            other => panic!("Expected Int(10), got {:?}", other),
        }
    }

    #[test]
    fn test_dict_get_missing_key_with_default() {
        let source = r#"
set, d = {"x": 10}
set, val = dict_get(d, "z", 99)
"#;
        let result = run_and_get(source, "val");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Int(99) => {}
            other => panic!("Expected Int(99), got {:?}", other),
        }
    }

    #[test]
    fn test_dict_set_new_key() {
        let source = r#"
set, d = dict()
set, d = dict_set(d, "name", "Bob")
set, val = dict_get(d, "name")
"#;
        let result = run_and_get(source, "val");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Str(s) if s == "Bob" => {}
            other => panic!("Expected Str(Bob), got {:?}", other),
        }
    }

    #[test]
    fn test_dict_set_update_existing() {
        let source = r#"
set, d = {"count": 5}
set, d = dict_set(d, "count", 10)
set, val = dict_get(d, "count")
"#;
        let result = run_and_get(source, "val");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Int(10) => {}
            other => panic!("Expected Int(10), got {:?}", other),
        }
    }

    #[test]
    fn test_dict_keys() {
        let source = r#"
set, d = {"a": 1, "b": 2, "c": 3}
set, keys = dict_keys(d)
set, count = len(keys)
"#;
        let result = run_and_get(source, "count");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Int(3) => {}
            other => panic!("Expected Int(3), got {:?}", other),
        }
    }

    #[test]
    fn test_dict_values() {
        let source = r#"
set, d = {"x": 100, "y": 200}
set, vals = dict_values(d)
set, count = len(vals)
"#;
        let result = run_and_get(source, "count");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Int(2) => {}
            other => panic!("Expected Int(2), got {:?}", other),
        }
    }

    #[test]
    fn test_dict_has_key_true() {
        let source = r#"
set, d = {"key1": "value1"}
set, has = dict_has(d, "key1")
"#;
        let result = run_and_get(source, "has");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Bool(true) => {}
            other => panic!("Expected Bool(true), got {:?}", other),
        }
    }

    #[test]
    fn test_dict_has_key_false() {
        let source = r#"
set, d = {"key1": "value1"}
set, has = dict_has(d, "key2")
"#;
        let result = run_and_get(source, "has");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Bool(false) => {}
            other => panic!("Expected Bool(false), got {:?}", other),
        }
    }

    #[test]
    fn test_dict_len() {
        let source = r#"
set, d = {"a": 1, "b": 2, "c": 3, "d": 4}
set, size = len(d)
"#;
        let result = run_and_get(source, "size");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Int(4) => {}
            other => panic!("Expected Int(4), got {:?}", other),
        }
    }

    #[test]
    fn test_dict_mixed_types() {
        let source = r#"
set, d = dict()
set, d = dict_set(d, "int", 42)
set, d = dict_set(d, "float", 3.14)
set, d = dict_set(d, "string", "hello")
set, d = dict_set(d, "bool", true)
set, int_val = dict_get(d, "int")
set, float_val = dict_get(d, "float")
"#;
        let interp = run_code(source);
        assert!(interp.is_ok());
        let interp = interp.unwrap();

        match interp.get("int_val") {
            Value::Int(42) => {}
            other => panic!("Expected Int(42), got {:?}", other),
        }

        match interp.get("float_val") {
            Value::Float(f) if (f - 3.14).abs() < 0.001 => {}
            other => panic!("Expected Float(3.14), got {:?}", other),
        }
    }

    #[test]
    fn test_dict_special_keys() {
        let source = r#"
set, d = dict()
set, d = dict_set(d, "key with spaces", "val1")
set, d = dict_set(d, "key-with-dash", "val2")
set, d = dict_set(d, "key_underscore", "val3")
set, val = dict_get(d, "key with spaces")
"#;
        let result = run_and_get(source, "val");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Str(s) if s == "val1" => {}
            other => panic!("Expected Str(val1), got {:?}", other),
        }
    }

    #[test]
    fn test_dict_null_value() {
        let source = r#"
set, d = dict()
set, d = dict_set(d, "null_key", null)
set, val = dict_get(d, "null_key")
"#;
        let result = run_and_get(source, "val");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Null => {}
            other => panic!("Expected Null, got {:?}", other),
        }
    }

    #[test]
    fn test_dict_iteration() {
        let source = r#"
set, scores = {"Math": 95, "Physics": 88, "Chemistry": 92}
set, keys = dict_keys(scores)
set, total = 0
for, key, in, keys
    set, score = dict_get(scores, key)
    total = total + score
/end
"#;
        let result = run_and_get(source, "total");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Int(275) => {} // 95 + 88 + 92
            other => panic!("Expected Int(275), got {:?}", other),
        }
    }
}

#[cfg(test)]
mod file_io_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_file_write_and_read() {
        let test_file = "/tmp/vredrs_test_write_read.txt";
        let source = format!(
            r#"
set, content = "Hello, Vredrs!\nLine 2\n"
set, write_ok = write_file("{}", content)
set, read_content = read_file("{}")
"#,
            test_file, test_file
        );

        let interp = run_code(&source);
        assert!(interp.is_ok());
        let interp = interp.unwrap();

        // 检查写入是否成功
        match interp.get("write_ok") {
            Value::Bool(true) => {}
            other => panic!("Expected Bool(true) for write_ok, got {:?}", other),
        }

        // 检查读取内容
        match interp.get("read_content") {
            Value::Str(s) if s.contains("Hello, Vredrs!") => {}
            other => panic!("Expected content with 'Hello, Vredrs!', got {:?}", other),
        }

        // 清理测试文件
        let _ = fs::remove_file(test_file);
    }

    #[test]
    fn test_file_exists_true() {
        let test_file = "/tmp/vredrs_test_exists.txt";
        // 先创建文件
        let _ = fs::write(test_file, "test");

        let source = format!(
            r#"
set, exists = file_exists("{}")
"#,
            test_file
        );

        let result = run_and_get(&source, "exists");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Bool(true) => {}
            other => panic!("Expected Bool(true), got {:?}", other),
        }

        // 清理
        let _ = fs::remove_file(test_file);
    }

    #[test]
    fn test_file_exists_false() {
        let source = r#"
set, exists = file_exists("/nonexistent/path/file.txt")
"#;
        let result = run_and_get(source, "exists");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Bool(false) => {}
            other => panic!("Expected Bool(false), got {:?}", other),
        }
    }

    #[test]
    fn test_file_overwrite() {
        let test_file = "/tmp/vredrs_test_overwrite.txt";
        let source = format!(
            r#"
set, ok1 = write_file("{}", "First content")
set, ok2 = write_file("{}", "Second content")
set, final_content = read_file("{}")
"#,
            test_file, test_file, test_file
        );

        let interp = run_code(&source);
        assert!(interp.is_ok());
        let interp = interp.unwrap();

        match interp.get("final_content") {
            Value::Str(s) if s == "Second content" => {}
            other => panic!("Expected 'Second content', got {:?}", other),
        }

        // 清理
        let _ = fs::remove_file(test_file);
    }

    #[test]
    fn test_file_read_nonexistent_errors() {
        let source = r#"
set, content = read_file("/nonexistent/file.txt")
"#;
        let result = run_and_get(source, "content");
        assert!(
            result.is_err(),
            "missing files must be explicit errors, not Null"
        );
    }

    #[test]
    fn test_file_multiline_content() {
        let test_file = "/tmp/vredrs_test_multiline.txt";
        let source = format!(
            r#"
set, content = "Line 1\nLine 2\nLine 3\n"
set, ok = write_file("{}", content)
set, read_back = read_file("{}")
set, length = len(read_back)
"#,
            test_file, test_file
        );

        let interp = run_code(&source);
        assert!(interp.is_ok());
        let interp = interp.unwrap();

        match interp.get("length") {
            Value::Int(n) if n > 20 => {} // 至少包含所有内容
            other => panic!("Expected length > 20, got {:?}", other),
        }

        // 清理
        let _ = fs::remove_file(test_file);
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    #[test]
    fn test_dict_and_file_combined() {
        let test_file = "/tmp/vredrs_config.txt";
        let source = format!(
            r#"
# 创建配置字典
set, config = dict()
set, config = dict_set(config, "app_name", "TestApp")
set, config = dict_set(config, "version", "1.0")
set, config = dict_set(config, "port", "8080")

# 序列化字典为文本
set, keys = dict_keys(config)
set, serialized = ""
for, key, in, keys
    set, val = dict_get(config, key)
    # 简单拼接
/end

# 写入文件
set, write_ok = write_file("{}", "app_name=TestApp")

# 验证文件存在
set, exists = file_exists("{}")
"#,
            test_file, test_file
        );

        let interp = run_code(&source);
        assert!(interp.is_ok());
        let interp = interp.unwrap();

        match interp.get("exists") {
            Value::Bool(true) => {}
            other => panic!("Expected Bool(true), got {:?}", other),
        }

        // 清理
        let _ = fs::remove_file(test_file);
    }

    #[test]
    fn test_dict_with_list_values() {
        let source = r#"
set, data = dict()
set, data = dict_set(data, "numbers", [1, 2, 3, 4, 5])
set, data = dict_set(data, "strings", ["a", "b", "c"])
set, nums = dict_get(data, "numbers")
set, count = len(nums)
"#;
        let result = run_and_get(source, "count");
        assert!(result.is_ok());
        match result.unwrap() {
            Value::Int(5) => {}
            other => panic!("Expected Int(5), got {:?}", other),
        }
    }

    #[test]
    fn test_nested_operations() {
        let source = r#"
# 创建嵌套结构
set, company = dict()
set, company = dict_set(company, "name", "TechCorp")
set, company = dict_set(company, "employees", 100)

set, employee = dict()
set, employee = dict_set(employee, "name", "Alice")
set, employee = dict_set(employee, "role", "Engineer")

# 访问数据
set, company_name = dict_get(company, "name")
set, emp_name = dict_get(employee, "name")
set, emp_count = dict_get(company, "employees")
"#;
        let interp = run_code(source);
        assert!(interp.is_ok());
        let interp = interp.unwrap();

        match interp.get("company_name") {
            Value::Str(s) if s == "TechCorp" => {}
            other => panic!("Expected 'TechCorp', got {:?}", other),
        }

        match interp.get("emp_name") {
            Value::Str(s) if s == "Alice" => {}
            other => panic!("Expected 'Alice', got {:?}", other),
        }
    }
}

#[cfg(test)]
mod industrial_runtime_tests {
    use super::*;
    use std::env;
    use std::fs;

    #[test]
    fn test_import_alias_keeps_module_scope_isolated() {
        let dir = env::temp_dir().join(format!("vredrs_mod_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        fs::write(
            dir.join("helper.veds"),
            "set, secret = 41\nexport, secret\n",
        )
        .unwrap();
        let old = env::current_dir().unwrap();
        env::set_current_dir(&dir).unwrap();
        let result = run_code("import, \"./helper\", as, h\nset, answer = h.secret + 1\n");
        env::set_current_dir(old).unwrap();
        let _ = fs::remove_dir_all(&dir);
        let interp = result.unwrap();
        assert!(matches!(interp.get("answer"), Value::Int(42)));
        assert!(matches!(interp.get("secret"), Value::Null));
    }

    #[test]
    fn test_builtin_math_module() {
        let interp = run_code("import, \"math\"\nset, x = math.pow(2, 5)\n").unwrap();
        match interp.get("x") {
            Value::Float(v) => assert!((v - 32.0).abs() < 0.0001),
            other => panic!("expected float, got {:?}", other),
        }
    }

    #[test]
    fn test_inheritance_and_super() {
        let src = r#"
class, Animal
    fn, speak()
        return, "animal"
    /end
/end
class, Dog, extends, Animal
    fn, speak()
        return, super.speak() + " dog"
    /end
/end
set, d = Dog()
set, s = d.speak()
"#;
        let interp = run_code(src).unwrap();
        assert!(matches!(interp.get("s"), Value::Str(s) if s == "animal dog"));
    }

    #[test]
    fn test_error_is_not_silent_null() {
        let result = run_code("set, x = 1 / 0\n");
        assert!(result.is_err());
    }

    #[test]
    fn test_iterator_next_protocol_for_for_loop() {
        let src = r#"
class, Counter
    value
    fn, new(start)
        set, self.value = start
    /end
    fn, next()
        if, self.value >= 3
            return, stop()
        /end
        set, current = self.value
        set, self.value = self.value + 1
        return, current
    /end
/end
set, c = Counter(0)
set, total = 0
for, x, in, c
    total = total + x
/end
"#;
        let interp = run_code(src).unwrap();
        assert!(matches!(interp.get("total"), Value::Int(3)));
    }
}

#[cfg(test)]
mod industrial_language_gap_tests {
    use super::*;

    #[test]
    fn test_slice_syntax_lists_and_strings() {
        let src = r#"
set, xs = [0, 1, 2, 3, 4]
set, mid = xs[1:4]
set, rev = xs[::-1]
set, head = "abcdef"[:3]
"#;
        let interp = run_code(src).unwrap();
        assert!(matches!(interp.get("mid"), Value::List(v) if v.len() == 3));
        assert!(matches!(interp.get("rev"), Value::List(v) if v.len() == 5));
        assert!(matches!(interp.get("head"), Value::Str(s) if s == "abc"));
    }

    #[test]
    fn test_operator_overload_and_getitem() {
        let src = r#"
class, Box
    value
    fn, new(v)
        set, self.value = v
    /end
    fn, __add__(other)
        return, Box(self.value + other.value)
    /end
    fn, __getitem__(idx)
        return, self.value + idx
    /end
/end
set, a = Box(10)
set, b = Box(5)
set, c = a + b
set, x = a[7]
"#;
        let interp = run_code(src).unwrap();
        assert!(matches!(interp.get("x"), Value::Int(17)));
        match interp.get("c") {
            Value::Object(_, fields) => {
                assert!(matches!(fields.borrow().get("value"), Some(Value::Int(15))))
            }
            other => panic!("expected object, got {:?}", other),
        }
    }

    #[test]
    fn test_with_close_and_freeze() {
        let src = r#"
class, CM
    closed
    fn, new()
        set, self.closed = false
    /end
    fn, __enter__()
        return, "entered"
    /end
    fn, __exit__(err)
        set, self.closed = true
        return, false
    /end
/end
set, cm = CM()
set, inside = null
with, cm, as, v
    set, inside = v
/end
set, xs = freeze([1, 2, 3])
set, frozen = is_frozen(xs)
"#;
        let interp = run_code(src).unwrap();
        assert!(matches!(interp.get("frozen"), Value::Bool(true)));
        assert!(matches!(interp.get("inside"), Value::Str(s) if s == "entered"));
        match interp.get("cm") {
            Value::Object(_, fields) => assert!(matches!(
                fields.borrow().get("closed"),
                Some(Value::Bool(true))
            )),
            other => panic!("expected object, got {:?}", other),
        }
    }

    #[test]
    fn test_async_await_annotations_and_recursion_limit() {
        let src = r#"
@route("/answer")
async fn, answer()
    return, 42
/end
set_recursion_limit(128)
set, result = await answer()
set, meta = annotations("answer")
"#;
        let interp = run_code(src).unwrap();
        assert!(matches!(interp.get("result"), Value::Int(42)));
        // annotations() returns a flat dict: {"route": "/answer"}.
        match interp.get("meta") {
            Value::Dict(d) => {
                assert!(matches!(d.get("route"), Some(Value::Str(s)) if s == "/answer"));
            }
            other => panic!("expected dict, got {:?}", other),
        }
    }
}

#[cfg(test)]
mod magic_assignment_dispatch_tests {
    use super::*;

    #[test]
    fn test_bare_index_assignment_dispatches_setitem() {
        let src = r#"
class, Bag
    seen
    fn, new()
        set, self.seen = null
    /end
    fn, __setitem__(idx, value)
        set, self.seen = idx + value
    /end
/end
set, b = Bag()
b[4] = 6
set, out = b.seen
"#;
        let interp = run_code(src).unwrap();
        assert!(matches!(interp.get("out"), Value::Int(10)));
    }

    #[test]
    fn test_bare_member_assignment_dispatches_setattr() {
        let src = r#"
class, Guard
    fn, __setattr__(name, value)
        throw, "__setattr__ called"
    /end
/end
set, g = Guard()
g.x = 2
"#;
        let result = run_code(src);
        assert!(
            result.is_err(),
            "member assignment must call __setattr__ when present"
        );
    }

    #[test]
    fn test_delete_dispatches_delitem() {
        let src = r#"
class, Guard
    fn, __delitem__(idx)
        throw, "__delitem__ called"
    /end
/end
set, g = Guard()
del, g[0]
"#;
        let result = run_code(src);
        assert!(
            result.is_err(),
            "del, obj[index] must call __delitem__ when present"
        );
    }
}

#[cfg(test)]
mod lambda_map_filter_tests {
    use super::*;

    #[test]
    fn test_lambda_call() {
        let src = "set, f, fn(x) x + 1\nprintln, f(10)";
        let result = run_code(src);
        assert!(result.is_ok());
        let runner = result.unwrap();
        // f(10) should return 11, which gets printed
        // We can't easily capture stdout, but we can check globals
        let f = runner.get("f");
        assert!(matches!(f, Value::Func(_)));
    }

    #[test]
    fn test_map_with_lambda() {
        let src = r#"set, nums, [1, 2, 3]
set, doubled, map(fn(x) x * 2, nums)
assert, doubled == [2, 4, 6]"#;
        let result = run_code(src);
        assert!(result.is_ok(), "map with lambda should work");
        if let Ok(runner) = result {
            let doubled = runner.get("doubled");
            match doubled {
                Value::List(l) => {
                    assert_eq!(l.len(), 3);
                    assert_eq!(l[0], Value::Int(2));
                    assert_eq!(l[1], Value::Int(4));
                    assert_eq!(l[2], Value::Int(6));
                }
                _ => panic!("expected List, got {:?}", doubled),
            }
        }
    }

    #[test]
    fn test_filter_with_lambda() {
        let src = r#"set, nums, [1, 2, 3, 4, 5]
set, evens, filter(fn(x) x % 2 == 0, nums)
assert, evens == [2, 4]"#;
        let result = run_code(src);
        assert!(result.is_ok(), "filter with lambda should work");
        if let Ok(runner) = result {
            let evens = runner.get("evens");
            match evens {
                Value::List(l) => {
                    assert_eq!(l.len(), 2);
                    assert_eq!(l[0], Value::Int(2));
                    assert_eq!(l[1], Value::Int(4));
                }
                _ => panic!("expected List, got {:?}", evens),
            }
        }
    }

    #[test]
    fn test_map_with_named_function() {
        let src = r#"fn, dbl(x)
    return, x * 2
/end
set, r, map(dbl, [1, 2, 3])
assert, r == [2, 4, 6]"#;
        let result = run_code(src);
        assert!(result.is_ok(), "map with named function should work");
    }

    #[test]
    fn test_lambda_closure_capture() {
        let src = r#"fn, adder(n)
    return, fn(x) x + n
/end
set, add5, adder(5)
set, r, add5(3)
assert, r == 8"#;
        let result = run_code(src);
        assert!(result.is_ok(), "lambda closure capture should work");
        if let Ok(runner) = result {
            assert_eq!(runner.get("r"), Value::Int(8));
        }
    }
}

#[cfg(test)]
mod pipe_tests {
    use super::*;

    #[test]
    fn test_pipe_bare_function() {
        let src = r#"fn, dbl(x)
    return, x * 2
/end
set, r, 5 |> dbl
assert, r == 10"#;
        let result = run_code(src);
        assert!(result.is_ok(), "pipe with bare function should work");
        if let Ok(runner) = result {
            assert_eq!(runner.get("r"), Value::Int(10));
        }
    }

    #[test]
    fn test_pipe_with_args() {
        let src = r#"fn, add(a, b)
    return, a + b
/end
set, r, 5 |> add(10)
assert, r == 15"#;
        let result = run_code(src);
        assert!(result.is_ok(), "pipe with args should work");
        if let Ok(runner) = result {
            assert_eq!(runner.get("r"), Value::Int(15));
        }
    }

    #[test]
    fn test_pipe_chain() {
        let src = r#"fn, dbl(x)
    return, x * 2
/end
fn, inc(x)
    return, x + 1
/end
set, r, 5 |> dbl |> inc
assert, r == 11"#;
        let result = run_code(src);
        assert!(result.is_ok(), "pipe chain should work");
        if let Ok(runner) = result {
            assert_eq!(runner.get("r"), Value::Int(11));
        }
    }
}

#[cfg(test)]
mod defer_tests {
    use super::*;

    #[test]
    fn test_defer_runs_after_body() {
        // defer should run AFTER the body, in LIFO order.
        // We can't easily capture stdout, but we can verify that
        // defer doesn't run BEFORE the body by checking that the
        // function's return value is correct.
        let src = r#"fn, with_defer()
    defer, set, x, 1
    return, 42
/end
set, r, with_defer()
assert, r == 42"#;
        let result = run_code(src);
        assert!(result.is_ok(), "defer should not prevent return value");
        if let Ok(runner) = result {
            assert_eq!(runner.get("r"), Value::Int(42));
        }
    }
}

#[cfg(test)]
mod exception_tests {
    use super::*;

    #[test]
    fn test_try_catch_basic() {
        let src = r#"try
    throw, "error1"
catch, e
    assert, e == "error1"
/end"#;
        let result = run_code(src);
        assert!(result.is_ok(), "try/catch should catch throw");
    }

    #[test]
    fn test_nested_try_catch() {
        let src = r#"fn, deep()
    try
        throw, "deep"
    catch, e
        throw, "rethrown: " + e
    /end
/end
try
    deep()
catch, e2
    assert, e2 == "rethrown: deep"
/end"#;
        let result = run_code(src);
        assert!(result.is_ok(), "nested try/catch should propagate re-thrown errors");
    }
}

#[cfg(test)]
mod range_tests {
    use super::*;

    #[test]
    fn test_inclusive_range() {
        let src = r#"set, r, 1 ... 5
assert, r == [1, 2, 3, 4, 5]"#;
        let result = run_code(src);
        assert!(result.is_ok(), "inclusive range should include end value");
        if let Ok(runner) = result {
            let r = runner.get("r");
            match r {
                Value::List(l) => {
                    assert_eq!(l.len(), 5, "inclusive range 1...5 should have 5 elements");
                    assert_eq!(l[4], Value::Int(5), "inclusive range should include end value");
                }
                _ => panic!("expected List, got {:?}", r),
            }
        }
    }

    #[test]
    fn test_exclusive_range() {
        let src = r#"set, r, 1 .. 5
assert, r == [1, 2, 3, 4]"#;
        let result = run_code(src);
        assert!(result.is_ok(), "exclusive range should not include end value");
        if let Ok(runner) = result {
            let r = runner.get("r");
            match r {
                Value::List(l) => {
                    assert_eq!(l.len(), 4, "exclusive range 1..5 should have 4 elements");
                    assert_eq!(l[3], Value::Int(4), "exclusive range should not include end value");
                }
                _ => panic!("expected List, got {:?}", r),
            }
        }
    }

    #[test]
    fn test_power_precedence() {
        // 2 ** 3 ** 2 should be 2 ** (3 ** 2) = 2 ** 9 = 512 (right-assoc)
        let src = r#"set, r, 2 ** 3 ** 2
assert, r == 512"#;
        let result = run_code(src);
        assert!(result.is_ok(), "power should be right-associative");
        if let Ok(runner) = result {
            assert_eq!(runner.get("r"), Value::Float(512.0));
        }
    }
}

#[cfg(test)]
mod comprehension_tests {
    use super::*;

    #[test]
    fn test_list_comprehension_with_filter() {
        let src = r#"set, nums, [1, 2, 3, 4, 5]
set, evens, [x for x in nums if x % 2 == 0]
assert, evens == [2, 4]"#;
        let result = run_code(src);
        assert!(result.is_ok(), "list comprehension with filter should work");
        if let Ok(runner) = result {
            let evens = runner.get("evens");
            match evens {
                Value::List(l) => {
                    assert_eq!(l.len(), 2);
                    assert_eq!(l[0], Value::Int(2));
                    assert_eq!(l[1], Value::Int(4));
                }
                _ => panic!("expected List, got {:?}", evens),
            }
        }
    }

    #[test]
    fn test_list_comprehension_no_filter() {
        let src = r#"set, squares, [x * x for x in [1, 2, 3]]
assert, squares == [1, 4, 9]"#;
        let result = run_code(src);
        assert!(result.is_ok(), "list comprehension without filter should work");
        if let Ok(runner) = result {
            let squares = runner.get("squares");
            match squares {
                Value::List(l) => {
                    assert_eq!(l.len(), 3);
                    assert_eq!(l[0], Value::Int(1));
                    assert_eq!(l[1], Value::Int(4));
                    assert_eq!(l[2], Value::Int(9));
                }
                _ => panic!("expected List, got {:?}", squares),
            }
        }
    }
}

#[cfg(test)]
mod operator_overload_tests {
    use super::*;

    #[test]
    fn test_add_overload() {
        let src = r#"class, Vec2
    fn, new(x, y)
        set, self.x, x
        set, self.y, y
        return, self
    /end
    fn, __add__(other)
        return, Vec2.new(self.x + other.x, self.y + other.y)
    /end
/end
set, v1, Vec2.new(1, 2)
set, v2, Vec2.new(3, 4)
set, v3, v1 + v2
assert, v3.x == 4
assert, v3.y == 6"#;
        let result = run_code(src);
        assert!(result.is_ok(), "operator overload __add__ should work");
        if let Ok(runner) = result {
            let v3 = runner.get("v3");
            match v3 {
                Value::Object(_, fields) => {
                    let f = fields.borrow();
                    assert_eq!(f.get("x"), Some(&Value::Int(4)));
                    assert_eq!(f.get("y"), Some(&Value::Int(6)));
                }
                _ => panic!("expected Object, got {:?}", v3),
            }
        }
    }

    #[test]
    fn test_eq_overload_symmetric() {
        // Test that 5 == obj and obj == 5 both work
        let src = r#"class, Box
    fn, new(v)
        set, self.v, v
        return, self
    /end
    fn, __eq__(other)
        return, self.v == other
    /end
/end
set, b, Box.new(5)
assert, b == 5
assert, 5 == b"#;
        let result = run_code(src);
        assert!(result.is_ok(), "== overload should be symmetric");
    }
}

#[cfg(test)]
mod with_statement_tests {
    use super::*;

    #[test]
    fn test_with_object() {
        let src = r#"class, CM
    fn, __enter__()
        return, "entered"
    /end
    fn, __exit__(err)
        set, self.exited, true
    /end
/end
set, cm, CM()
with, cm, v
    assert, v == "entered"
/end
assert, cm.exited == true"#;
        let result = run_code(src);
        assert!(result.is_ok(), "with statement should call __enter__/__exit__");
    }
}

#[cfg(test)]
mod llvm_closure_capture_tests {
    use crate::codegen::llvm_full::FullLlvmGen;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    /// Parse + lower a source string through the full LLVM backend, returning
    /// the produced LLVM IR text (or the error message).
    fn lower_to_llvm(source: &str) -> String {
        let mut lexer = Lexer::new(source, 0);
        let tokens = match lexer.tokenize() {
            Ok(t) => t,
            Err(e) => return format!("<lex error: {}>", e.message()),
        };
        let mut parser = Parser::new(tokens, 0);
        let program = match parser.parse_program() {
            Ok(p) => p,
            Err(e) => return format!("<parse error: {}>", e.message()),
        };
        let mut gen = FullLlvmGen::new();
        match gen.generate(&program) {
            Ok(ir) => ir,
            Err(e) => format!("<codegen error: {}>", e.message()),
        }
    }

    /// Bug 3: lambdas must capture free variables. The IR must contain a
    /// closure struct alloca `{ ptr, ptr }` and an env alloca when captures
    /// are present, plus a load of each captured variable.
    #[test]
    fn llvm_lambda_captures_free_variable() {
        let src = "set, x = 10\nset, f = fn(y) x + y\nprintln, f(5)";
        let ir = lower_to_llvm(src);
        assert!(
            !ir.starts_with("<"),
            "lowering failed: {}",
            ir
        );
        // The lambda body references `x` (a free variable from the enclosing
        // scope). The IR must allocate an env array for the capture.
        assert!(
            ir.contains("alloca [1 x %vredrs.value]"),
            "expected env alloca for 1 capture: {}",
            ir
        );
        // The closure struct { ptr, ptr } must be allocated.
        assert!(
            ir.contains("alloca { ptr, ptr }"),
            "expected closure struct alloca: {}",
            ir
        );
        // The lifted lambda function must load the captured variable from
        // %__env (GEP into the env array).
        assert!(
            ir.contains("getelementptr inbounds [1 x %vredrs.value], ptr %__env"),
            "expected GEP into %__env in lambda body: {}",
            ir
        );
        // The lambda function definition with %__env parameter must exist.
        assert!(
            ir.contains("define %vredrs.value @__lambda_"),
            "expected lifted lambda function: {}",
            ir
        );
    }

    /// Bug 3: closures without captures still work (env is null). The IR
    /// must still allocate the closure struct (for uniform call-site code)
    /// but the env slot stores `null`.
    #[test]
    fn llvm_lambda_without_captures_still_works() {
        let src = "set, f = fn(x) x + 1\nprintln, f(10)";
        let ir = lower_to_llvm(src);
        assert!(!ir.starts_with("<"), "lowering failed: {}", ir);
        // Closure struct is always allocated (uniform representation).
        assert!(
            ir.contains("alloca { ptr, ptr }"),
            "expected closure struct alloca even without captures: {}",
            ir
        );
        // No env array alloca when there are no captures.
        assert!(
            !ir.contains("alloca [1 x %vredrs.value]"),
            "no env alloca expected without captures: {}",
            ir
        );
        // Env slot stores null.
        assert!(
            ir.contains("store ptr null, ptr"),
            "expected 'store ptr null' for env without captures: {}",
            ir
        );
    }

    /// Bug 3: closures passed to map must propagate the env pointer. The IR
    /// must call the closure with `ptr <env>` (not `ptr null`) when captures
    /// are present.
    #[test]
    fn llvm_map_with_capturing_closure_passes_env() {
        let src = "set, factor = 3\nset, r = map(fn(x) x * factor, [1, 2, 3])";
        let ir = lower_to_llvm(src);
        assert!(!ir.starts_with("<"), "lowering failed: {}", ir);
        // The closure captures `factor` → env array alloca.
        assert!(
            ir.contains("alloca [1 x %vredrs.value]"),
            "expected env alloca for map closure capturing factor: {}",
            ir
        );
        // The lambda body must load `factor` from %__env.
        assert!(
            ir.contains("getelementptr inbounds [1 x %vredrs.value], ptr %__env"),
            "expected GEP into %__env in map closure: {}",
            ir
        );
    }

    /// Bug 3: multiple captures are laid out in order. The IR must allocate
    /// an env array sized to the number of captures.
    #[test]
    fn llvm_lambda_multiple_captures() {
        let src = "set, a = 1\nset, b = 2\nset, c = 3\nset, f = fn(x) x + a + b + c";
        let ir = lower_to_llvm(src);
        assert!(!ir.starts_with("<"), "lowering failed: {}", ir);
        // 3 captures → env array of size 3.
        assert!(
            ir.contains("alloca [3 x %vredrs.value]"),
            "expected env alloca for 3 captures: {}",
            ir
        );
    }
}
