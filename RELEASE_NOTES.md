# Vredrs — Release Notes

## 0.1.4 — Error System, Multi-Mode Build, Raw Static Syntax, raw ≠ Cstar

### Overview

This release introduces the **V-series error code system**, a **multi-mode
project build system**, **raw static syntax** (`ptr[T]` / `&T` / `&mut T` /
`u8`–`u64` / `Result[T,E]` / `trait` / `impl` / `dtor`), and firmly
establishes that the **raw backend and the Cstar backend are separate,
independently-implemented** code generators.

### New Features

#### V-series error codes
- 42 new error codes organized by phase and file type:
  - `V0001`–`V0999` — shared (lex `V0001`, syntax `V0010`/`V0020`, type
    `V0030`, scope `V0040`/`V0060`, semantic `V0050`, pattern `V0070`)
  - `V1000`–`V1999` — `.veds` interpreter/dynamic (`V1001` runtime type,
    `V1002` coroutine, `V1003` field, `V1100` VM state)
  - `V2000`–`V2999` — `.vraw` static compilation (`V2001` ownership,
    `V2002`/`V2003` borrow, `V2004` lifetime, `V2005` linear type,
    `V2006` static type, `V2007` constexpr, `V2008` layout, `V2009` raw mode)
  - `V3000`–`V3999` — `.cpps` bare-metal (`V3001` interrupt, `V3002` memory
    map, `V3003` DMA, `V3004` linker, `V3005` linear type, `V3006` layout)
  - `V4000`–`V4999` — shared file/IO/module (`V4001`–`V4004`)
  - `V5000`–`V5999` — shared config/build/toolchain (`V5001`–`V5004`)
- Each code has a category, a default suggestion, and a deterministic
  "Author insists" quote (stable hash of message + code).
- **ANSI colored rendering** (`render_colored()`): red category header,
  cyan file locator, white source line, red `^^^` marker, green
  suggestion, magenta quote. Respects the `NO_COLOR` environment variable
  (falls back to the plain renderer).
- `render_error_with_source()` in `lib.rs` wires the colored renderer into
  all CLI error paths.

#### Multi-mode project build system (`vredrs build <dir>`)
- **Incremental compilation** — FNV-1a content hash + mtime stored in
  `.vredrs-cache/<name>.cache`; unchanged files reuse the cached `.o`.
- **Parallel compilation** — `std::thread` worker pool, `--jobs N` / `-j N`
  (default = `available_parallelism()`).
- **LTO** — `--release` enables thin LTO via clang.
- **`--clean`** — clears `.vredrs-cache/` and `build/` before building.
- **`--strip`** — strips debug symbols from the final executable.
- **`--target <TRIPLE>` / `-t`** — cross-compilation; parses the triple to
  select x86_64 / aarch64 / arm codegen. Auto-detects the host arch when
  no target is given.
- File classification (`src/driver/file_classifier.rs`): `.veds` → LLVM,
  `.vraw` → raw native, `.cpps` → Cstar bare-metal.
- `BuildResult` reports `veds_count` / `vraw_count` / `cpps_count` /
  `compiled_count` / `cached_count` / `elapsed_ms`.

#### Raw static syntax (`.vraw` files)
- **Pointer types** — `ptr[T]` AST node (`ast::TypeExpr::Pointer`), with
  `load()` / `store()` / `load_acquire()` / `store_release()` semantics.
- **Borrow types** — `&T` (`ast::TypeExpr::Borrow`) and `&mut T`
  (`ast::TypeExpr::MutBorrow`), each with an optional lifetime annotation.
- **Fixed-width unsigned integers** — `u8`, `u16`, `u32`, `u64`
  (`ast::TypeExpr::UnsignedInt(width, span)`).
- **Result type** — `Result[T, E]` for error-propagating computations.
- **`trait` / `impl` / `dtor` blocks** — `TraitDef`, `ImplBlock`,
  `DtorBlock` AST variants + parser support. `impl Trait for Type` provides
  ad-hoc polymorphism; `dtor` blocks define resource cleanup.
- **Linear type checking** — `src/codegen/cstar/linear.rs` now hard-enforces
  that `ptr[T]` resources are consumed (moved or freed) before scope exit.

#### raw ≠ Cstar — architectural separation
- **Raw backend** (`src/codegen/cstar/raw/`): `x86.rs` (x86_64 Intel-syntax
  assembly), `arm.rs` (AArch64 + ARM32), `emitter.rs` (shared dispatcher),
  `monomorphization.rs` (static type specialization). Generates native ELF
  executables from `.vraw` files. Zero GC, zero scheduler, zero runtime
  type tags.
- **Cstar backend** (`src/codegen/cstar/`): `codegen.rs`, `linear.rs`,
  `pir.rs` (Physical IR), `advanced.rs` (the 5 `@`-features). Generates
  bare-metal firmware from `.cpps` files. Linear types + DMA + interrupts.
- The two backends share **only** the frontend (lexer/parser/AST) and the
  directory name. Their codegen pipelines are entirely separate.

#### ARM AArch64 / ARM32 backend
- `src/codegen/cstar/raw/arm.rs` (464 lines) emits GNU-syntax assembly for
  both AArch64 (64-bit) and ARM32 (Thumb-2) targets.
- Covers MOV (immediate + register), ADD/SUB/MUL, LDR/STR (immediate +
  register offset), ORR/AND/EOR/MVN, B/BX/BL, PUSH/POP.
- `detect_arch()` in `build.rs` auto-selects the host architecture;
  `--target` overrides it for cross-compilation.

#### Cstar advanced features (`src/codegen/cstar/advanced.rs`, 725 lines)
- **`@pipeline(priority=...)`** — compile-time DMA pipeline orchestration;
  splits loops into compute windows + DMA transfer windows.
- **`@patch(base=..., output=...)`** — firmware differential update;
  computes byte-level diffs against a base firmware at compile time.
- **`@isr_group(budget_us=..., priority=...)`** — deterministic interrupt
  clustering; estimates WCET from instruction count and rejects over-budget
  handlers.
- **`@prefetch(hint=..., stride=...)`** — hybrid cache prefetch; emits
  `__builtin_prefetch` with stride-based windowing.
- **`@repo { ... }`** — physical package management; partitions packages
  by compiled size into SRAM / flash_fast / flash_normal / flash_archive,
  generates a `PKG_INDEX` offset table, and deduplicates by SHA-256 hash.

### Verification
- `cargo test --release`: **213 passed, 0 failed**.
- All **68 `.veds`** files (examples + tests) execute without runtime errors.
- 2 `.cpps` sample files compile to valid ARM firmware images.
- `vredrs build .` (project build) runs incremental + parallel correctly.
- Error rendering verified with and without `NO_COLOR`.

---

## 0.1.3 — Standard Library (32 modules)

### Overview
This release completed the full standard library: **32 modules**, all
implemented. 26 are native Rust (no external dependencies); 6 are stubs
that behave correctly without crashing in the single-threaded VM. See
[STDLIB_REPORT_0.1.3.md](STDLIB_REPORT_0.1.3.md) for the full module list.

### P0 fixes
1. `time.now().unix()` returned null → fixed: `time_now` returns a Time
   object with a full method dispatcher (unix/year/month/day/.../format).
2. `math.sin(PI/2)` returned 0.0 → fixed: verified 1.0; extended to 28
   trig/hyperbolic/log/rounding/power functions.
3. `vredrs_runtime.c` missing `math.h` → fixed: added `#include <math.h>`
   and `<time.h>`.
4. `--raw` crashed on ARM → fixed: `std::env::consts::ARCH` detection;
   non-x86_64 prints a hint and skips assemble/link.

### Modules (highlights)
- **math** — 28 functions + constants (pi/e/tau/inf/nan), Lanczos gamma.
- **time** — Time object + 12 methods + sleep + Duration constants
  (civil_from_days algorithm).
- **crypto** — sha256/sha1/md5 (FIPS 180-4) + AES-256-CBC (FIPS-197) +
  bcrypt-style salted SHA-256 password hashing.
- **regex** — self-implemented backtracking engine (`./\*/+/?/[...]/^$/\d\w\s`).
- **image** — PNM (P3/P6/P2/P5) + BMP load/save/resize/crop.
- **sync** — spawn/channel/send/receive/close/mutex/waitgroup + atomic
  submodule.
- **embed** — `embed.fs(dir)` returns a virtual filesystem with
  read/exists/list.

---

## 0.1.2 — Bytecode VM, LLVM Backend Split, Raw Mode

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

## Patch Round (0.1.2 — bug-fix pass)

The following bugs were found by systematic runtime testing and fixed on top
of the 0.1.2 release. All 187 unit tests and all 33 `.veds` example/test
files continue to pass.

### Optional chaining (`?.`) — parse + execute + compile

`obj?.user?.name` built an `OptionalChainExpr` with an **empty** `chain`
list; the `.user` / `.name` accesses were parsed as separate
`MemberAccess` nodes outside the chain, so the optional-chain executor never
ran and the program crashed or returned wrong values. Fixed by parsing the
link following `?.` (`field` / `method(args)` / `[index]`) and appending it
to the chain. Added `Value::Dict` support to `optional_member` (and to
`get_field` / `MemberAccess`) so chaining works on dict literals like
`{"user": {"name": "Alice"}}`. (See `test_optional_chain.veds`.)

### Call-frame isolation for recursion

Function bodies run via the AST-interpreter path. Previously `call_function`
and `call_method_on_class` only saved/restored **parameter** values in
`globals`; any other local written with `set` leaked into `globals` and was
overwritten by recursive calls — e.g. `fib` written with `set, a, fib(n-1);
set, b, fib(n-2); return, a + b` returned `5` instead of `55`. Fixed by
adding a `local_scopes: Vec<HashMap<String, Value>>` call-frame stack with
`push_scope`/`pop_scope`/`scope_get`/`scope_set`/`scope_delete` helpers, and
redirecting all AST-interpreter variable access through them. Each
invocation now gets its own scope. (See `test_recursion.veds`.)

### Generator yielding inside loops

Generators are eagerly evaluated, but a `yield` inside a `while`/`for`/`loop`
body escaped the loop on the first yield (the "yield" error propagated out
of the loop construct, terminating it), so only the first value was
collected — e.g. `fib_gen()` printed `1 0 0 0 0 0 0 0` instead of
`1 1 2 3 5 8 13 21`. Fixed by introducing a `gen_yield_buffer` on the VM:
`yield, X` records the value in the buffer; the loop constructs catch the
"yield" signal and **continue executing the remaining statements of the
current iteration** (so loop variables advance), then proceed to the next
iteration. (See `test_generator.veds`.)

### `ClassName.new()` constructor

`Counter.new()` returned `null` because the `receiver.method(args)` dispatch
path in `execute_ast_expr`'s `Call` handler ran first and
`ast_method_call` had no arm for `Value::Class` receivers (it fell through
to the default `Ok(Value::Null)`). The dedicated `ClassName.new(args)` arm
that calls `instantiate_class` was unreachable. Fixed by adding a
`Value::Class` arm to `ast_method_call` that dispatches `new` to
`instantiate_class`. `Counter()` and `Counter.new()` now behave identically.
(See `test_class_new.veds`.)

### Lambda closure capture

A lambda `fn(x) x + n` only captured the *name* `n` dynamically: when the
enclosing function returned, its parameter scope was popped, so `n`
resolved to `null` at call time — e.g. `adder(5)(3)` returned `null`
instead of `8`. Fixed by snapshotting the visible lexical environment (all
local scopes + globals) at lambda-creation time into a `lambda_captures`
map, and restoring it as the innermost scope (below the param scope) when
the lambda is called. (See `test_closure_capture.veds`.)

### String repeat via `*`

`"ab" * 3` returned `null` because the `Mul` opcode only handled numeric
operands. Added `str * int` / `int * str` arms to both the bytecode `Instr::Mul`
handler and the AST `ast_binary` `BinaryOp::Mul` handler, producing the
repeated string (empty string for `n <= 0`). (See `test_string_repeat.veds`.)

## Test Results

All 187 unit tests pass. All `.veds` example and test files (33 files across
`examples/`, `tests/`, `tests/acceptance/`, `tests/basic/`, `tests/std/`,
`tests/bugs/`) execute without runtime errors.

