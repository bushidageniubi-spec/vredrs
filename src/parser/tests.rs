use super::*;
use crate::lexer::Lexer;

fn parse(source: &str) -> Result<Program> {
    let mut lexer = Lexer::new(source, 0);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens, 0);
    parser.parse_program()
}

fn assert_parse_ok(source: &str) {
    match parse(source) {
        Ok(p) => assert!(
            !p.declarations.is_empty() || source.trim().is_empty(),
            "Expected non-empty program for: {}",
            source
        ),
        Err(e) => panic!("Parse failed for '{}': {:?}", source, e),
    }
}

fn assert_parse_err(source: &str) {
    match parse(source) {
        Ok(_) => panic!("Expected parse error for: {}", source),
        Err(_) => {} // expected
    }
}

// ============================================================================
// 基础语句
// ============================================================================

#[test]
fn test_paste_simple() {
    assert_parse_ok("paste, \"hello\"");
}

#[test]
fn test_paste_multiple_args() {
    assert_parse_ok("paste, \"a\"; \"b\"; 42");
}

#[test]
fn test_println() {
    assert_parse_ok("println, \"hello world\"");
}

#[test]
fn test_set_simple() {
    assert_parse_ok("set, x, 10");
}

#[test]
fn test_set_multi_target() {
    assert_parse_ok("set, a, b = 1, 2");
}

#[test]
fn test_set_with_operator() {
    assert_parse_ok("set, x += 1");
}

#[test]
fn test_set_variable_assign() {
    assert_parse_ok("x = 42");
}

// ============================================================================
// 指令块
// ============================================================================

#[test]
fn test_directive_block_set() {
    assert_parse_ok("/set\n    name, \"Alice\"\n    age, 30\n/end");
}

#[test]
fn test_directive_block_paste() {
    assert_parse_ok("/paste\n    \"hello\"\n    \"world\"\n/end");
}

#[test]
fn test_scope_prefix_block() {
    assert_parse_ok(
        "/set\n    /config.\n        host, \"localhost\"\n        port, 8080\n    /\n/end",
    );
}

#[test]
fn test_nested_scope_blocks() {
    assert_parse_ok("/set\n    /a.b.\n        /c.\n            value, 1\n        /\n    /\n/end");
}

#[test]
fn test_pon_placeholder() {
    assert_parse_ok("pon; name, \"Alan\"");
}

// ============================================================================
// 表格赋值
// ============================================================================

#[test]
fn test_table_assign() {
    assert_parse_ok("/set\n    a; b; c\n    /=\n        1; 2; 3\n        4; 5; 6\n    /\n/end");
}

// ============================================================================
// 控制流
// ============================================================================

#[test]
fn test_if_simple() {
    assert_parse_ok("if, x > 0 then println, \"positive\"");
}

#[test]
fn test_if_block() {
    assert_parse_ok("if, x > 0\n    println, \"positive\"\n/end");
}

#[test]
fn test_if_elif_else() {
    assert_parse_ok("if, x > 0\n    println, \"pos\"\nelif, x < 0\n    println, \"neg\"\nelse\n    println, \"zero\"\n/end");
}

#[test]
fn test_while_loop() {
    assert_parse_ok("while, i < 10\n    i += 1\n/end");
}

#[test]
fn test_for_in() {
    assert_parse_ok("for, x, in, 1..10\n    println, x\n/end");
}

#[test]
fn test_for_from_to() {
    assert_parse_ok("for, i, in, range(0, 5)\n    println, i\n/end");
}

#[test]
fn test_loop_forever() {
    assert_parse_ok("loop\n    println, \"forever\"\n/end");
}

#[test]
fn test_break_continue() {
    assert_parse_ok("for, i, in, 1..10\n    if, i > 5 then break\n/end");
    assert_parse_ok("for, i, in, 1..10\n    if, i == 3 then continue\n/end");
}

#[test]
fn test_labeled_loop() {
    assert_parse_ok("outer: for, i, in, 1..10\n    for, j, in, 1..10\n        if, i*j > 50 then break outer\n    /end\n/end");
}

#[test]
fn test_match_simple() {
    assert_parse_ok("match, x\n    case, 0\n        println, \"zero\"\n    case, _\n        println, \"other\"\n/end");
}

#[test]
fn test_match_with_guard() {
    assert_parse_ok("match, x\n    case, n if n > 0\n        println, \"positive\"\n/end");
}

#[test]
fn test_try_catch() {
    assert_parse_ok("try\n    risky()\ncatch, err\n    println, err\n/end");
}

#[test]
fn test_try_finally() {
    assert_parse_ok("try\n    f = open(\"f.txt\")\nfinally\n    f.close()\n/end");
}

// ============================================================================
// 函数定义
// ============================================================================

#[test]
fn test_fn_simple() {
    assert_parse_ok("fn, add(a, b)\n    return, a + b\n/end");
}

#[test]
fn test_fn_single_line() {
    assert_parse_ok("fn, square(x); return, x * x");
}

#[test]
fn test_fn_with_default() {
    assert_parse_ok("fn, greet(name, greeting=\"Hello\")\n    println, greeting; name\n/end");
}

#[test]
fn test_fn_variadic() {
    assert_parse_ok("fn, sum(args...)\n    return, args#\n/end");
}

#[test]
fn test_fn_multi_return() {
    assert_parse_ok("fn, divmod(a, b)\n    return, a//b, a%b\n/end");
}

#[test]
fn test_async_fn() {
    assert_parse_ok("async fn, fetch(url)\n    return, url\n/end");
}

// ============================================================================
// 结构体/类/接口/枚举
// ============================================================================

#[test]
fn test_struct_def() {
    assert_parse_ok("struct, Point\n    x, int\n    y, int\n/end");
}

#[test]
fn test_class_def() {
    assert_parse_ok("class, Animal\n    private, name\n    fn, new(name)\n        self.name = name\n    /end\n/end");
}

#[test]
fn test_class_extends() {
    assert_parse_ok(
        "class, Dog, extends, Animal\n    fn, speak()\n        return, \"Woof\"\n    /end\n/end",
    );
}

#[test]
fn test_interface_def() {
    assert_parse_ok("interface, Drawable\n    fn, draw(self)\n/end");
}

#[test]
fn test_enum_def() {
    assert_parse_ok("enum, Color\n    Red\n    Green\n    Blue\n/end");
}

#[test]
fn test_enum_with_payload() {
    assert_parse_ok("enum, Result\n    Ok(value)\n    Err(msg)\n/end");
}

// ============================================================================
// 表达式
// ============================================================================

#[test]
fn test_integer_float() {
    assert_parse_ok("set, x, 42");
    assert_parse_ok("set, x, 3.14");
    assert_parse_ok("set, x, 0xFF");
}

#[test]
fn test_string() {
    assert_parse_ok("set, s, \"hello\"");
}

#[test]
fn test_bool_null() {
    assert_parse_ok("set, a, true");
    assert_parse_ok("set, b, false");
    assert_parse_ok("set, c, null");
}

#[test]
fn test_list_literal() {
    assert_parse_ok("set, lst, [1, 2, 3]");
}

#[test]
fn test_dict_literal() {
    assert_parse_ok("set, d, {\"a\": 1, \"b\": 2}");
}

#[test]
fn test_set_literal() {
    assert_parse_ok("set, s, {1, 2, 3}");
}

#[test]
fn test_tuple_literal() {
    assert_parse_ok("set, t, (1, \"hello\", true)");
}

#[test]
fn test_range() {
    assert_parse_ok("set, r, 1..10");
    assert_parse_ok("set, r, 1...10");
    assert_parse_ok("set, r, 1..10 step 2");
}

#[test]
fn test_arithmetic() {
    assert_parse_ok("set, x, a + b * c");
    assert_parse_ok("set, x, (a + b) * c");
}

#[test]
fn test_comparison() {
    assert_parse_ok("set, flag, a > b and c <= d");
}

#[test]
fn test_function_call() {
    assert_parse_ok("println(add(1, 2))");
}

#[test]
fn test_method_call() {
    assert_parse_ok("set, x, obj.method(a, b)");
}

#[test]
fn test_index_access() {
    assert_parse_ok("set, x, arr[0]");
}

#[test]
fn test_member_access() {
    assert_parse_ok("set, x, obj.field");
}

#[test]
fn test_ternary() {
    assert_parse_ok("set, x, a > b ? a : b");
}

#[test]
fn test_pipe_operator() {
    assert_parse_ok("set, result, data |> json.parse |> process");
}

#[test]
fn test_null_coalesce() {
    assert_parse_ok("set, name, input ?? \"default\"");
}

#[test]
fn test_optional_chaining() {
    assert_parse_ok("set, name, user?.profile?.name");
}

#[test]
fn test_try_propagate() {
    assert_parse_ok("set, x, f() ?");
}

#[test]
fn test_spread() {
    assert_parse_ok("set, combined, [0, ...lst, 10]");
}

#[test]
fn test_lambda() {
    assert_parse_ok("set, square, fn(x) x * x");
}

#[test]
fn test_coro() {
    assert_parse_ok("set, gen, coro(counter, 10)");
}

#[test]
fn test_resume() {
    assert_parse_ok("set, val, resume(gen)");
}

// ============================================================================
// 后缀操作符
// ============================================================================

#[test]
fn test_postfix_length() {
    assert_parse_ok("set, n, lst#");
}

#[test]
fn test_postfix_reverse() {
    assert_parse_ok("set, rev, lst~");
}

#[test]
fn test_postfix_asc_sort() {
    assert_parse_ok("set, asc, lst^");
}

#[test]
fn test_postfix_desc_sort() {
    assert_parse_ok("set, desc, lst_");
}

// ============================================================================
// 并发
// ============================================================================

#[test]
fn test_spawn() {
    assert_parse_ok("spawn, fn() println, \"hello\"");
}

#[test]
fn test_send_receive() {
    // send/receive are function-call style
    assert_parse_ok("set, ch, channel(int, 0)");
    assert_parse_ok("send(ch, 42)");
    assert_parse_ok("set, v, receive(ch)");
}

#[test]
fn test_select() {
    assert_parse_ok("select\n    case, receive(ch), v\n        println, v\n    default\n        println, \"nop\"\n/end");
}

// ============================================================================
// 其他语句
// ============================================================================

#[test]
fn test_defer() {
    assert_parse_ok("defer, f.close()");
}

#[test]
fn test_assert() {
    assert_parse_ok("assert, x > 0, \"x must be positive\"");
}

#[test]
fn test_throw() {
    assert_parse_ok("throw, \"error occurred\"");
}

#[test]
fn test_import_export() {
    assert_parse_ok("import, \"math\"");
    assert_parse_ok("import, \"json\", as, j");
    assert_parse_ok("import, \"os\", get_env, exit");
    assert_parse_ok("export, add, sub");
}

#[test]
fn test_type_alias() {
    assert_parse_ok("type, Point3D = struct{x, y, z}");
}

#[test]
fn test_constexpr() {
    assert_parse_ok("constexpr, PI = 3.14159");
}

#[test]
fn test_lazy_def() {
    assert_parse_ok("lazy, set, data = read(\"bigfile.txt\")");
}

#[test]
fn test_annotations() {
    assert_parse_ok("@deprecated\nfn, old_func()\n/end");
    assert_parse_ok("@inline\nfn, double(x)\n    return, x * 2\n/end");
}

// ============================================================================
// 类型表达式
// ============================================================================

#[test]
fn test_type_basic() {
    assert_parse_ok("fn, f(x: int, y: float): str\n    return, \"ok\"\n/end");
}

#[test]
fn test_type_generic() {
    assert_parse_ok("fn, f[T](x: T): T\n    return, x\n/end");
}

#[test]
fn test_type_optional() {
    assert_parse_ok("fn, f(x: str?)\n/end");
}

// ============================================================================
// 错误情况
// ============================================================================

#[test]
fn test_error_unclosed_block() {
    assert_parse_err("/set\n    x, 10");
}

#[test]
fn test_error_unexpected_end() {
    assert_parse_err("/end");
}

// ============================================================================
// 完整小程序
// ============================================================================

#[test]
fn test_full_program() {
    let src = r#"
import, "math"

fn, add(a, b)
    return, a + b
/end

set, x, 10
set, y = 20
paste, "Sum: "; add(x, y)

/set
    name, "vredrs"
    version, 2.0
/end

if, x > 5
    println, "big"
elif, x > 0
    println, "small"
else
    println, "zero"
/end

for, i, in, 0..5
    println, i
/end

match, x
    case, 0
        println, "zero"
    case, _ if x > 0
        println, "positive"
/end
"#;
    assert_parse_ok(src);
}

#[test]
fn parses_bare_member_and_index_assignment() {
    use crate::lexer::Lexer;
    use crate::parser::ast::{Assignee, Stmt};
    use crate::parser::Parser;

    let src = "a[0] = 1\na.x = 2\n";
    let mut lexer = Lexer::new(src, 0);
    let tokens = lexer.tokenize().unwrap();
    let mut parser = Parser::new(tokens, 0);
    let program = parser.parse_program().unwrap();
    assert_eq!(program.declarations.len(), 2);
    match &program.declarations[0] {
        crate::parser::ast::TopLevel::Statement(Stmt::Assign(a)) => {
            assert!(matches!(a.targets.get(0), Some(Assignee::Index(_))))
        }
        other => panic!("expected index assignment, got {:?}", other),
    }
    match &program.declarations[1] {
        crate::parser::ast::TopLevel::Statement(Stmt::Assign(a)) => {
            assert!(matches!(a.targets.get(0), Some(Assignee::Member(_))))
        }
        other => panic!("expected member assignment, got {:?}", other),
    }
}
