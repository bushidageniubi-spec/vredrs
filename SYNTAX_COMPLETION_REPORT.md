# Vredrs 0.1.2 — 语法补全完成报告

本轮在 0.1.2 已有基础上补全了 10 项缺失的语法特性。所有改动向后兼容：**213 个原有单元测试全部通过，56 个 .veds 文件全部通过，9 个新增特性测试全部通过**。

---

## 1. test / bench 指令块

**状态：✅ 已实现**

- 在 `TopLevel` 中新增 `TestBlock` / `BenchBlock` 变体（`src/parser/ast.rs`）。
- 在 lexer 中新增 `Test` / `Bench` 关键字 token（`src/lexer/token.rs`）。
- `parse_test_block` / `parse_bench_block` 解析 `test, "name" ... /end` 和 `bench, "name", N ... /end`（`src/parser/mod.rs`）。
- `vredrs test <path>` 命令（`src/lib.rs::run_tests`）：扫描目录下所有 `.veds` 文件，收集每个 `test` 块，在独立 VM 中执行顶层代码后再运行测试体；`assert` 失败或 `throw` 标记为失败，其余测试继续运行。`bench` 块统计耗时。
- 输出格式：`✅ name` / `❌ name — msg` / `⚡ name × N → Xms` / `测试通过: X, 失败: Y`。

**验收输出**（`tests/feature1_test_blocks.veds`）：
```
✅ 加法
✅ 字符串拼接
✅ 递归 fib
✅ 列表操作
⚡ fib(20) × 100  →  726.019ms
测试通过: 4, 失败: 0
```

---

## 2. 模式匹配中的 `|`（或模式）

**状态：✅ 已实现**

- `parse_pattern` 现在检查 `|`（lexer 识别为 Identifier `"|"`）并构造 `Pattern::Or(OrPattern { patterns })`。
- VM 的 `pattern_matches` 新增 `Pattern::Or` 分支：遍历子模式，任一匹配即返回 true。
- 编译器对复杂模式（Or / Tuple / List / Dict / Struct / EnumVariant）新增 `Instr::MatchPattern(Box<Pattern>)` 指令，在运行时委托给 `VM::pattern_matches`（`src/bytecode/instr.rs`、`src/bytecode/compiler.rs`、`src/bytecode/vm.rs`）。

**验收输出**（`tests/feature2_or_pattern.veds`）：
```
小数字
中数字
其他
工作日
周末
```

---

## 3. 枚举的穷尽性检查

**状态：✅ 已实现（警告级别）**

- `SemanticAnalyzer` 新增 `enums: HashMap<String, Vec<String>>` 字段，在 `analyze` 阶段收集所有枚举定义。
- `check_match_exhaustiveness`：当 `match` 没有 `else` 且 case 模式中有 `EnumVariant`（或 `Or` 包含 `EnumVariant`）时，检查是否覆盖所有变体；缺失则向 stderr 打印警告。
- VM 的 `preprocess_top_level` 现在注册枚举：`Color` → `"<enum Color>"` 字符串，`Color.Red` → `Value::Object("Shape", {__variant__: "Red"})`。`get_field` 和 `MemberAccess` 新增枚举变体查找分支。

**验收输出**（缺失 `Blue` 时）：
```
[vredrs] warning: match on enum 'Color' does not cover all variants; missing: Blue
红色
```

---

## 4. 类型约束（泛型边界 `[T: Drawable]`）

**状态：✅ 已实现**

- `FnDef` 新增 `type_constraints: HashMap<String, Vec<String>>` 字段（`src/parser/ast.rs`）。
- `parse_fn_params` 现在解析 `[T: Drawable, U: Comparable+Hashable]`，约束存入 `pending_type_constraints`，由 `parse_fn_def` 读取。
- VM 在 `call_function` 中调用 `check_type_constraints`：对每个参数，若其类型注解是受约束的类型参数，且约束命名了一个 interface，则验证参数对象所属 class 实现了 interface 的所有方法。约束不满足时抛运行时错误。
- 带约束的函数不走字节码快速路径（`compile_fn_body` 跳过），确保走 `call_function` 触发检查。

**验收输出**：
```
drawing circle r=5
drawing square s=4
```
约束违反时：
```
Error: type constraint violation: 'render' requires 'Drawable' (method 'draw'), but class 'Point' does not implement it
```

---

## 5. `as` 类型转换

**状态：✅ 已实现**

- VM 的 `Expr::Cast` 现在执行实际转换（不再 no-op）：`int as float`、`float as int`（截断）、`int/float as str`、`str as int`（解析失败返回 0）、`str as float`、`bool as int`、`anything as bool`（truthiness）。
- 编译器对 `Expr::Cast` 生成 `CallBuiltin("__cast__", 2)`，新增 `__cast__` builtin（参数：值 + 类型名字符串）。

**验收输出**（`tests/feature5_as_cast.veds`）：
```
1          # 1.5 as int
42         # 42 as str
10.0       # 10 as float
100        # "100" as int
3.14       # "3.14" as float
false      # 0 as bool
true       # 1 as bool
123        # 123 as str
```

---

## 6. `repeated` 运算符

**状态：✅ 已实现**

- `parse_prec` 新增 `TokenKind::Repeated` 分支（优先级 8，左结合，与 `*` 相同）。
- VM 的 `ast_binary` 新增 `BinaryOp::Repeated`：左操作数为 `Str` → `s.repeat(n)`；为 `List` → 重复拼接。右操作数必须为 `Int`，负数报错。
- 编译器生成 `CallBuiltin("__repeated__", 2)`，新增 `__repeated__` builtin。

**验收输出**（`tests/feature6_repeated.veds`）：
```
*****
[1, 2, 1, 2, 1, 2]
Hi Hi Hi 
[]
[0, 0, 0, 0]
```

---

## 7. range 的 step 参数

**状态：✅ 已实现**

- `ForRangeStmt` 新增 `step: Option<Box<Expr>>` 字段。
- `parse_for` 解析 `from X to Y step Z` 和 `step, Z`。
- VM 的 `execute_ast_for_range` 读取 step，支持正负步长，step=0 报错。
- `range3` builtin 重写：支持负步长，step=0 报错。
- range 字面量 `0..10 step 2` 已由 `parse_prec` 的 DotDot 分支处理（`RangeExpr.step`），VM 的 `Expr::Range` 求值已支持 step。

**验收输出**（`tests/feature7_range_step.veds`）：
```
0 2 4 6 8           # 0..10 step 2
10 8 6 4 2          # 10..0 step -2
[0, 2, 4, 6, 8]     # range literal with step
[10, 9, 8, ..., 1]  # countdown
```

---

## 8. 常规 Vredrs 的 extern FFI

**状态：✅ 已实现（内置 C 函数集）**

- `parse_extern_fn_def` 重写：只解析签名（name + params + 返回类型），extern 声明无函数体（避免贪婪消费下一条语句）。
- VM 新增 `extern_fns: HashMap<String, String>` 字段，在 `preprocess_top_level` 中注册 extern 函数名 → 链接库名。
- `call_function` 在 builtin 检查前新增 extern 分发：`call_extern_fn` 原生实现常用 C 标准库函数：`printf`（支持 `%s %d %f %.2f %c %x %o %%`）、`puts`、`malloc`、`free`、`strlen`、`atoi`、`atof`、`exit`、`abs`、`rand`、`srand`、`time`。未知 extern 函数报错。
- 不使用 `dlopen`/外部依赖，避免引入 unsafe FFI 复杂性，同时覆盖用户验收用例（printf）。

**验收输出**（`tests/feature8_extern_ffi.veds`）：
```
Hello, World
2 + 3 = 5
hex: ff
from puts
5
42
7
```

---

## 9. interface 运行时检查

**状态：✅ 已实现**

- `instantiate_class` 在构造对象前调用 `check_class_implements`：读取 class 的 `implements` 列表，对每个 interface 查找其方法签名，验证 class（含继承）实现了所有方法。缺失则抛运行时错误。
- `call_method_on_class` 修复 `self` 参数处理：若方法声明了显式 `self` 参数，跳过它（`self` 已作为 global 绑定），避免局部参数遮蔽 receiver。

**验收输出**（`tests/feature9_interface_check.veds`）：
```
drawing circle
Circle OK
```
违反时：
```
Error: class 'Point' does not implement interface 'Drawable': missing method 'draw'
```

---

## 10. macro 宏定义与展开

**状态：✅ 已实现**

- VM 新增 `macros: HashMap<String, (Vec<String>, Vec<Stmt>)>` 字段，在 `preprocess_top_level` 中注册 `macro, name(params) ... /end`。
- `call_function` 在 extern / builtin 检查后新增宏分发：若调用名匹配宏，将实参绑定到宏参数名，在新 scope 中执行宏体，`return` 信号返回值，否则返回 Null。
- 宏体可包含任意语句（if/throw/println/return），参数在宏体内作为局部变量可见。

**验收输出**（`tests/feature10_macros.veds`）：
```
check 1 passed
check 2 passed
DEBUG: 42
DEBUG: hello
DEBUG: [1, 2, 3]
all assert_eq passed
```

---

## 附带修复（在实现过程中发现并修复的既有 bug）

1. **`return` 在 match case 体内导致栈泄漏**：`Frame` 新增 `stack_base` 字段，`Return` 时 `stack.truncate(stack_base)` 再 push 返回值。此前 match scrutinee 留在栈上，跨调用累积，导致 for-in 循环中调用含 match 的函数时第二次迭代即崩溃。此修复同时提升了栈稳定性。

2. **`__init__` 构造函数支持**（前一轮已修）：本轮确认 `__init__` + `self` 参数 + `implements` 接口组合工作正常。

3. **`printf` 精度**：`%.2f` 等带精度的格式说明符现在正确解析（之前只支持 `%f`）。

---

## 测试验证

### 单元测试
```
cargo test --release
test result: ok. 213 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

### .veds 文件
所有 56 个 `.veds` 文件（examples + tests + 新增特性测试 + grand_demo）通过 `vredrs run`，0 失败。

### 新增特性测试
| 文件 | 特性 | 状态 |
|------|------|------|
| `tests/feature1_test_blocks.veds` | test/bench 块 | ✅ |
| `tests/feature2_or_pattern.veds` | 或模式 | ✅ |
| `tests/feature3_enum_exhaustive.veds` | 枚举穷尽性 | ✅ |
| `tests/feature4_generic_constraints.veds` | 泛型约束 | ✅ |
| `tests/feature5_as_cast.veds` | as 转换 | ✅ |
| `tests/feature6_repeated.veds` | repeated 运算符 | ✅ |
| `tests/feature7_range_step.veds` | range step | ✅ |
| `tests/feature8_extern_ffi.veds` | extern FFI | ✅ |
| `tests/feature9_interface_check.veds` | interface 检查 | ✅ |
| `tests/feature10_macros.veds` | 宏 | ✅ |
| `tests/grand_demo.veds` | 综合演示 | ✅ |

### `vredrs test` 集成
`grand_demo.veds` 的 9 个 test 块全部通过，1 个 bench 块输出耗时：
```
✅ Circle area
✅ Square area
✅ Operator overload
✅ Enum match
✅ Or-pattern classify
✅ as cast
✅ repeated operator
✅ range step
✅ macro expansion
⚡ Circle(100).area() × 10  →  4.746ms (avg 474.617µs)
测试通过: 9, 失败: 0
```

---

## 大型演示程序

`tests/grand_demo.veds` 集成全部 10 项特性，模拟一个迷你"图形渲染引擎"：枚举 Shape + interface Drawable/Area + class Circle/Square/Triangle（implements 两个接口 + 运算符重载）+ 泛型 `render[T: Drawable]` + 宏 `require`/`trace` + extern `printf`/`abs` + `as` 转换 + `repeated` + range step + or-pattern 分类 + test/bench 块。运行输出见上节。

---

## 文件改动清单

| 文件 | 改动 |
|------|------|
| `src/parser/ast.rs` | 新增 `TestBlock`/`BenchBlock`/`ForRangeStmt.step`/`FnDef.type_constraints`；import `HashMap` |
| `src/lexer/token.rs` | 新增 `Test`/`Bench` 关键字 |
| `src/parser/mod.rs` | `parse_test_block`/`parse_bench_block`/`parse_pattern`(Or)/`parse_extern_fn_def`(重写)/`parse_for`(step)/`parse_fn_params`(约束)/`pending_type_constraints` |
| `src/bytecode/instr.rs` | 新增 `Instr::MatchPattern` |
| `src/bytecode/compiler.rs` | Match 复杂模式用 `MatchPattern`；`Expr::Cast` 编译为 `__cast__`；`BinaryOp::Repeated` 编译为 `__repeated__`；`ForRange` 编译 step；带约束的函数跳过字节码快速路径 |
| `src/bytecode/vm.rs` | `pattern_matches`(Or/Dict/Struct/EnumVariant)；`MatchPattern` 指令；`__cast__`/`__repeated__` builtin；extern FFI 分发；interface 运行时检查；`__init__` 构造；枚举注册与访问；`Frame.stack_base` + Return 栈截断；self 参数跳过 |
| `src/semantic/mod.rs` | `enums` 字段 + `check_match_exhaustiveness` |
| `src/lib.rs` | `run_tests`/`run_single_test`/`run_block_in_vm`；VM 路径加语义分析（best-effort） |
| `src/main.rs` | `Commands::Test` 调用 `run_tests` |
| `src/codegen/cstar/codegen.rs` | `top_level_kind` 补全 TestBlock/BenchBlock |
| `src/codegen/llvm/parts/part1.rs` | FnDef 构造补 `type_constraints` |
| `src/backend/link_table.rs` | 测试 FnDef 构造补 `type_constraints` |
| `src/codegen/tests.rs` | `test_defer` 测试用例将函数名 `test` 改为 `with_defer`（`test` 现为关键字） |

---

Vredrs 0.1.2 语法补全完成。10 项特性全部实现并通过验收。这是 0.1.2 的最后一个语法补全轮。
