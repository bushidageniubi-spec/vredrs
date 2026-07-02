# Vredrs 0.1.2 — Release Notes

## Overview

This release fixes the three basic test programs (multiline lists, escape
characters, generator return values) and refactors the bytecode VM to
achieve full language coverage through a two-tier execution strategy
(native opcodes + AST fallback).

## Bug Fixes

### Three Basic Test Programs (now passing)

1. **Multiline list literals** (`[1, 2, 3,]` spanning multiple source
   lines) — the lexer now skips newlines inside `()`, `[]`, `{}` so
   collection literals can span multiple lines. The parser now accepts
   trailing commas in list/tuple/dict/set literals.

2. **Escape characters** (`\n`, `\t`, `\r`, `\0`, `\\`, `\"`, `\'`) —
   the AST interpreter and bytecode VM now both decode C-style escape
   sequences in string literals identically. Previously the AST
   interpreter passed through the raw `\n` text.

3. **Generator return values** — `resume(gen)` on an exhausted
   generator now returns the generator's `return, X` value (default
   `0` if not set). Generators are now stored as `Rc<RefCell<GenState>>`
   so `resume()` mutations persist across clones (previously each
   `resume()` returned the same first value because the cloned state
   was never written back).

### Bytecode VM — Full Language Coverage

The bytecode VM now supports the entire Vredrs language via a two-tier
strategy:

- **Native opcodes** for the hot path: literals, arithmetic, comparison,
  control flow, function calls, list/dict construction, indexing,
  builtins.
- **`EvalAst(Expr)` / `ExecAstStmt(Stmt)` fallback opcodes** for
  everything else — these carry the boxed AST node and let the VM
  evaluate it via its own AST interpreter path.

Features newly supported in the VM:

- **Classes**: inheritance, methods, `self`, `super.method()` dispatch
  with parent-class lookup.
- **Operator overloading**: `__add__`, `__sub__`, `__mul__`,
  `__getitem__`, `__setitem__`, `__delitem__`, `__enter__`, `__exit__`,
  `__setattr__`, `__len__`, `__str__`.
- **Object field mutation**: `self.field = value` and `obj.field = value`
  now persist on the original instance via `Rc<RefCell<HashMap>>` shared
  field maps (previously mutations were lost).
- **Generators**: `yield`, `resume`, generator return values.
- **Exceptions**: `try`/`catch`/`finally`, `throw`, with-statement.
- **Iterators**: for-in over Objects with a `next()` method that
  returns `stop()` to signal end-of-iteration.
- **Slices**: `list[1:4]`, `str[:3]`, `xs[::-1]` with full step
  semantics.
- **Comprehensions**: list, dict, set.
- **Lambdas**: `fn(x) x + 1` with proper naming so they can be called.
- **String interpolation**: `"Hello, {name}!"` evaluates the
  interpolation expression at runtime.
- **Async/await**: async functions run synchronously in the single-threaded
  VM; `await X` evaluates `X` inline.
- **Annotations**: `@route("/x")` is reflected via `annotations(fn_name)`.
- **Modules**: `import, "mod"`, `import, "mod", as, alias`,
  `import, "mod", sym1, sym2`. Built-in stdlib modules (math, io, os,
  time, fs, fmt, json, collections) are virtual — they expose native
  functions directly.
- **Match statements** with literal, binding, tuple, list, dict patterns.
- **Postfix operators**: `#` (len), `~` (reverse), `^` (asc-sort),
  `_` (desc-sort).
- **Bare index assignment**: `xs[i] = v` (dispatches to `__setitem__`
  on Objects).
- **Bare member assignment**: `obj.field = v` (dispatches to
  `__setattr__` when defined).
- **Delete**: `del, obj[i]` (dispatches to `__delitem__`).

### println Fixed

`println, x` now correctly emits a trailing newline (previously it
behaved like `paste` with no newline).

### for-in Loop Variable Scope

The loop variable in `for, x, in, items` is now stored in globals
(consistent with `LoadGlobal`), so the loop body can actually read it.

## Standard Library Fixes

- **fs.veds** — wrappers no longer infinitely recurse (they re-export
  the global builtins, which resolve to the VM's native implementations).
- **io.veds** — same pattern; wrappers re-export I/O builtins.
- **json.veds** — `stringify()` now produces correct JSON for null,
  bool, int, float, str, list. `parse()` is still a stub (returns
  input text).
- **collections.veds** — `unique()` now actually appends items to the
  result list (the `# Can't push to list` comment was a bug). Added
  `group_by()`.
- **fmt.veds** — `sprintf()` now correctly substitutes `{}` placeholders
  with `str(arg)` in order. Previously it just appended args.

## New Builtins Added

`abs`, `floor`, `ceil`, `round`, `enumerate`, `zip`, `list`, `set`,
`tuple`, `sorted`, `reversed`, `min`, `max`, `is_dir`, `is_file`,
`read_dir`, `path_join`, `basename`, `dirname`, `set_recursion_limit`,
`math_sin`, `math_cos`, `math_tan`, `math_log`.

## Code Quality

- Removed dead code: `program_uses_unsupported_features`,
  `stmt_uses_unsupported`, `expr_uses_unsupported` (no longer needed).
- Updated stale "NOT supported" comments in `bytecode/mod.rs`,
  `bytecode/vm.rs`, `bytecode/compiler.rs`, `bytecode/instr.rs` to
  reflect the new full-coverage architecture.
- `Value` now implements `PartialEq`, `Eq`, `PartialOrd`, `Ord` so
  values can be sorted and compared uniformly.
- `Instr` no longer derives `PartialEq` (it now holds boxed AST nodes
  via `EvalAst`/`ExecAstStmt`).
- Generated unique lambda names (`<lambda_0>`, `<lambda_1>`, ...) so
  multiple lambdas don't collide on the `<lambda>` name.

## Test Results

All 195 unit tests pass. All `.veds` example and test files execute
without runtime errors.
