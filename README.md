# Vredrs 0.1.4

A modern, multi-paradigm programming language with a bytecode VM, an LLVM
native backend, a **raw** static native backend, and a **Cstar** bare-metal
backend — all sharing one frontend.

Vredrs ships with a stack-based **bytecode virtual machine** (`vredrs run`)
that executes `.veds` source files directly. The `build` command lowers the
AST to LLVM IR (via clang) for native executables, to raw machine code for
`.vraw` files, or to bare-metal firmware for `.cpps` files. A multi-mode
**build system** drives incremental, parallel, cached project builds.

**Author:** Alan Chen  
**License:** MIT (see [LICENSE](LICENSE))  
**Version:** 0.1.4

---

## Three File Types, Three Independent Backends

Vredrs uses file extensions to route source through the right backend. Each
backend is an **independent implementation** — they do not share codegen
pipelines.

| Extension | Backend | Output | Entry Point |
|-----------|---------|--------|-------------|
| `.veds` | Bytecode VM (`run`) / LLVM native (`build`) | Executable | `vredrs run` / `vredrs build <file>` |
| `.vraw` | **Raw backend** — x86_64 / AArch64 / ARM32 native codegen | Native ELF executable | `vredrs build <file> --raw` |
| `.cpps` | **Cstar backend** — bare-metal firmware | `.bin` / `.elf` firmware | `vredrs build <file> --raw` (`.cpps` auto-detected) |

> **raw ≠ Cstar.** The raw backend (`src/codegen/cstar/raw/` — `x86.rs`,
> `arm.rs`, `emitter.rs`) generates native executables from `.vraw` files
> with zero runtime overhead. The Cstar backend (`src/codegen/cstar/` —
> `codegen.rs`, `linear.rs`, `pir.rs`, `advanced.rs`) generates bare-metal
> firmware from `.cpps` files with linear types, DMA orchestration, and
> interrupt clustering. They are separate, independently-maintained
> implementations that happen to live under the same `cstar/` directory.

---

## Features

### Language (shared across all backends)
- **Bytecode VM** — AST → bytecode compiler + stack-based interpreter with
  call frames, generators, closures, and an `EvalAst`/`ExecAstStmt` fallback.
- **Function-body bytecode** — pure functions compile to bytecode (not
  AST-walked), making `fib(25)` faster than CPython 3.12.
- **Closures** — lambdas capture their lexical environment at creation time.
- **Classes & inheritance** — vtable-based dispatch, `super` calls,
  `ClassName.new(...)` and `ClassName(...)` constructors, `__init__` support.
- **Generators** — `yield` in any function; `resume` advances the generator.
- **Async/await** — `async fn` returns a coroutine; `await` polls it.
- **Exception handling** — `try`/`catch`/`finally`/`throw`.
- **Modules** — multi-file programs with `import`, recursive resolution,
  32-module bundled stdlib.
- **Containers** — List, Dict, Tuple, Set, String with full operations.
- **Comprehensions** — list, dict, and set comprehensions.
- **Operator overloading** — `__add__`, `__sub__`, `__mul__`, `__getitem__`,
  `__setitem__`, `__enter__`, `__exit__`, `__setattr__`, `__len__`,
  `__str__`, `__call__`, `__eq__`.
- **Context managers** — `with` statement via `__enter__`/`__exit__`.
- **String interpolation** — `"Hello, {name}!"`.
- **Optional chaining** — `obj?.field`, `obj?.method()`, `obj?.[index]`.
- **Null coalescing** — `a ?? b`.
- **Slice syntax** — `list[1:4]`, `str[:5]`, `arr[::-1]`.
- **Annotations** — `@route("/")` + `annotations(fn)` reflection.
- **Pattern matching** — literal, binding, tuple, list, dict, or-patterns.
- **Traits & impls** — `trait`/`impl` blocks (0.1.4) for ad-hoc polymorphism.
- **Destructors** — `dtor` blocks for resource cleanup.

### Raw static syntax (0.1.4 — for `.vraw` files)
- **Pointer types** — `ptr[T]` with `load()`/`store()` atomic operations.
- **Borrow types** — `&T` (shared borrow), `&mut T` (mutable borrow).
- **Fixed-width unsigned ints** — `u8`, `u16`, `u32`, `u64`.
- **Result type** — `Result[T, E]` for error propagation.
- **Linear type checking** — `ptr[T]` resources must be consumed before
  scope exit (enforced by `src/codegen/cstar/linear.rs`).

### Cstar bare-metal syntax (for `.cpps` files)
- **`volatile`** — prevents compiler optimization of hardware-mapped reads.
- **`align(N)`** — forces struct/buffer alignment (DMA descriptors, cache lines).
- **`asm { ... }`** — inline assembly for target architecture.
- **`@section("name")`** — places code/data in a specific linker section.
- **`@pipeline`** — compile-time DMA pipeline orchestration.
- **`@patch`** — firmware differential (byte-level) update generation.
- **`@isr_group`** — deterministic interrupt clustering with WCET budgets.
- **`@prefetch`** — hybrid memory cache prefetch hints.
- **`@repo`** — physical package management (size-based Flash/SRAM partitioning).

### Toolchain
- **Bytecode VM** (`run`/`vm`) — the default execution engine.
- **REPL** (`repl`) — interactive console with history, tab completion,
  multi-line blocks, cursor editing.
- **Native build** (`build <file>`) — LLVM IR → clang → executable.
- **Raw build** (`build <file> --raw`) — direct x86_64/ARM codegen.
- **Project build** (`build <dir>`) — multi-mode build system (0.1.4).
- **Package manager** (`mod`) — init/add/rm/list/tree/update.
- **Formatter** (`fmt`) — 4-space indentation, `--check`/`--write`.
- **LSP** — language server (completion, hover, definition, diagnostics).
- **DAP** — debug adapter protocol.
- **Test runner** (`test`) — `test`/`bench` blocks with assert/throw.

### Error system (0.1.4)
- **V-series error codes** — phase-and-file-type-specific codes:
  - `V0001`–`V0999`: shared (lex/syntax/semantic/scope/pattern)
  - `V1000`–`V1999`: `.veds` interpreter/dynamic
  - `V2000`–`V2999`: `.vraw` static compilation (ownership/borrow/lifetime/linear)
  - `V3000`–`V3999`: `.cpps` bare-metal (interrupt/memory-map/DMA/linker)
  - `V4000`–`V4999`: shared file/IO/module
  - `V5000`–`V5999`: shared config/build/toolchain
- **ANSI colored rendering** — with `NO_COLOR` environment variable support.
- **Source-context diagnostics** — file:line:column, source line, `^^^`
  marker, suggestion, and an "Author insists" quote.

---

## Installation

### Prerequisites
- **Rust** 1.70+ (install via [rustup](https://rustup.rs))
- **clang** 15+ (for LLVM IR compilation to native executable)
- **GNU `as` + `ld`** (for raw x86_64/ARM backend; ships with binutils)
- **LLVM** 15+ (ships with clang on most platforms)

### Build
```bash
git clone https://github.com/alanchen/vredrs.git
cd vredrs
cargo build --release
```

The `vredrs` binary will be at `target/release/vredrs`.

---

## Quick Start

Create `hello.veds`:
```
paste, "Hello, World!\n"
set, nums, [1, 2, 3, 4, 5]
paste, "sum = {sum(nums)}\n"
for, n, in, nums
    paste, "{n} "
/end
paste, "\n"
```

Run via the bytecode VM:
```bash
./target/release/vredrs run hello.veds
```

Compile to native executable:
```bash
./target/release/vredrs build hello.veds -o hello
./hello
```

Raw native build (x86_64/ARM direct codegen):
```bash
./target/release/vredrs build raw_program.vraw -o raw_app --raw
```

Project build (incremental + parallel):
```bash
./target/release/vredrs build . --release --jobs 8
```

Output:
```
Hello, World!
sum = 15
1 2 3 4 5
```

---

## The Multi-Mode Build System (0.1.4)

`vredrs build <dir>` scans a project directory, classifies files by
extension, compiles each with the appropriate backend, and links the
results. It supports incremental compilation, parallelism, caching, LTO,
and cross-compilation.

### Build options
```
vredrs build <dir> [OPTIONS]

OPTIONS:
    --release            Enable LTO and optimizations
    --clean              Clear cache and build artifacts before building
    --strip              Strip debug symbols from output
    -j, --jobs <N>       Number of parallel compile jobs (0 = auto-detect)
    -t, --target <TRIPLE>  Cross-compile target (e.g. aarch64-unknown-linux-gnu)
```

### How it works
1. **Scan** — walks the directory, classifies files as `.veds` / `.vraw` /
   `.cpps` / other.
2. **Cache check** — for each file, computes an FNV-1a content hash + mtime
   and compares against `.vredrs-cache/<name>.cache`. Unchanged files reuse
   the cached `.o`.
3. **Parallel compile** — changed files are compiled in parallel using
   `std::thread` (up to `--jobs` workers, default = CPU count).
4. **Link** — all `.o` files are linked into a single executable (or
   firmware image for `.cpps`).
5. **Strip** (optional) — `--strip` removes debug symbols.

### Architecture detection
The build system auto-detects the host architecture (`x86_64`, `aarch64`,
`arm`). Use `--target` to cross-compile:
```bash
vredrs build . --target aarch64-unknown-linux-gnu
vredrs build . --target arm-unknown-linux-gnueabihf
```

---

## Syntax Overview

### Variables & Assignment
```
set, x, 42
set, name, "Alice"
set, pi, 3.14
set, flag, true
set, items, [1, 2, 3]
set, config, {"port": 8080, "host": "localhost"}
```
`=` may also be used: `set, x = 42`.

### Functions
```
fn, add(a, b)
    return, a + b
/end

fn, factorial(n)
    if, n <= 1
        return, 1
    /end
    return, n * factorial(n - 1)
/end
```

### Lambdas (with closure capture)
```
set, add, fn(x, y) x + y
set, doubled, map(fn(x) x * 2, [1, 2, 3])

fn, adder(n)
    return, fn(x) x + n         # captures n
/end
set, add5, adder(5)
paste, add5(3)                  # 8
```

### Classes
```
class, Animal
    fn, __init__(name)
        set, self.name, name
    /end
    fn, speak()
        paste, self.name
        paste, " makes a sound\n"
    /end
/end

class, Dog, extends, Animal
    fn, speak()
        paste, self.name
        paste, " barks\n"
    /end
/end

set, d, Dog.new("Rex")          # ClassName.new(...) constructor
set, d2, Dog("Buddy")           # ClassName(...) shorthand
d.speak()
```

Both `__init__` (Python-style, mutates self) and `new` (Vredrs-style,
returns self) constructors are supported. `__init__` takes precedence.

### Exceptions
```
try
    throw, "something went wrong"
catch, e
    paste, "caught: {e}\n"
finally
    paste, "cleanup\n"
/end
```

### Generators & Async
```
fn, counter()
    yield, 1
    yield, 2
    yield, 3
/end

set, c, counter()
paste, resume(c)   # 1
paste, resume(c)   # 2

async fn, fetch()
    return, 42
/end
set, val, await fetch()
paste, "{val}\n"   # 42
```

### Context Managers
```
with, open("data.txt", "w"), f
    f.write("hello")
/end
```

### Slices, Interpolation & Optional Chaining
```
set, lst, [1, 2, 3, 4, 5]
set, sub, lst[1:4]          # [2, 3, 4]
set, rev, lst[::-1]         # [5, 4, 3, 2, 1]
set, s, "hello world"
set, head, s[:5]            # "hello"
set, name, "Vredrs"
set, msg, "Hello, {name}!"  # "Hello, Vredrs!"
set, rep, "ab" * 3          # "ababab"

set, obj, {"user": {"name": "Alice"}}
set, n, obj?.user?.name     # "Alice"
set, m, obj?.address?.city  # null (short-circuits)
```

### Annotations
```
@route("/")
fn, index()
    return, "home"
/end
set, anns, annotations(index)
paste, anns["route"]        # "/"
```

### Modules
```
import, "math_lib.veds", square
paste, square(5)   # 25

import, "math", sqrt, pow
paste, sqrt(16)    # 4.0
```

### Traits & Impls (0.1.4)
```
trait, Drawable
    fn, draw(self)
    /end
/end

impl, Drawable, for, Circle
    fn, draw(self)
        paste, "drawing circle\n"
    /end
/end
```

### Raw static types (0.1.4 — `.vraw` files)
```
set, reg_base, 0x40020000 as ptr[u32]
set, val, reg_base.load()           # atomic read
reg_base.store(0x1234)              # atomic write

fn, process(data: &mut [u8], len: u32)
    # &mut T mutable borrow; u32 fixed-width unsigned
/end
```

### Cstar bare-metal (`.cpps` files)
```
@section(".text.boot")
fn, _start()
    set, reg, 0x40020000 as ptr[u32]
    reg.store(0x01)
/end

@pipeline(priority="cpu_bound")
fn, process_data(src: ptr[u8], dst: ptr[u8], len: u32)
    for, i, in, range(0, len)
        dst.store(i, src.load(i) * 2)
    /end
/end

@isr_group(budget_us=50, priority="high")
fn, uart_isr()
    # interrupt handler with WCET budget
/end
```

### Comprehensions
```
set, nums, [1, 2, 3, 4, 5]
set, squares, [x * x for x in nums]
set, evens, [x for x in nums if x % 2 == 0]
```

---

## Standard Library

32 modules, all implemented (26 native Rust + 6 stubs that behave correctly
without crashing). Key builtins:

| Function | Description |
|----------|-------------|
| `len(x)` | Length of string/list/dict/tuple |
| `str(x)` / `int(x)` / `float(x)` / `bool(x)` | Type conversions |
| `range(a, b)` / `range(a, b, step)` | Integer ranges |
| `sum(l)` / `min(l)` / `max(l)` | Aggregates |
| `sorted(l)` / `reversed(l)` | Sorted/reversed copies |
| `map(fn, l)` / `filter(fn, l)` | Higher-order |
| `enumerate(l)` / `zip(a, b)` | Iteration helpers |
| `print(x)` / `paste(x)` / `println(x)` | Output |
| `input(prompt)` | Read a line from stdin |
| `open(path, mode)` / `read` / `write` / `close` | File I/O |
| `read_file(path)` / `write_file(path, content)` | Whole-file I/O |
| `file_exists(path)` / `read_dir(path)` / `is_dir` / `is_file` | Filesystem |
| `split(s, sep)` / `join(l, sep)` / `trim` / `upper` / `lower` | Strings |
| `contains(haystack, needle)` | Membership test |
| `type_of(x)` / `freeze(x)` / `is_frozen(x)` | Introspection |
| `abs` / `floor` / `ceil` / `round` | Numeric |

Standard library modules (import by short name): `io`, `fs`, `math`, `fmt`,
`collections`, `json`, `os`, `time`, `rand`, `path`, `encoding`, `crypto`,
`regex`, `csv`, `xml`, `toml`, `debug`, `log`, `term`, `flag`, `sync`,
`image`, `machine`, `unsafe`, `embed`, `net`, `http`, `sql`, `websocket`,
`compress`, `yaml`, `testing`.

---

## CLI Usage

```
vredrs <COMMAND> [OPTIONS]

COMMANDS:
    run <file>              Execute a .veds file (bytecode VM)
    vm  <file>              Execute a .veds file (bytecode VM, alias)
    repl                    Interactive REPL (history, tab, multi-line)
    build <file> [opts]     Compile a single file to native executable
    build <dir>  [opts]     Multi-mode project build (incremental/parallel)
    test <path>             Run test/bench blocks
    mod <init|add|rm|list|tree|update>  Package manager
    fmt [opts] <file>       Format source code
    lsp                     Language server (stdio)
    dap                     Debug adapter (stdio)

SINGLE-FILE BUILD OPTIONS:
    -o, --output <file>     Output path
    -b, --backend <name>    native | llvm-ir | raw
    --raw                   Shortcut for -b raw

PROJECT BUILD OPTIONS:
    --release               Enable LTO + optimizations
    --clean                 Clear cache and build artifacts
    --strip                 Strip debug symbols
    -j, --jobs <N>          Parallel compile jobs (0 = auto)
    -t, --target <TRIPLE>   Cross-compile target

FMT OPTIONS:
    --write, -w             Overwrite file
    --check, -c             Check only (exit 1 if needs formatting)

EXAMPLES:
    vredrs run main.veds
    vredrs build main.veds -o app
    vredrs build main.veds -o output.ll -b llvm-ir
    vredrs build raw_prog.vraw -o raw_app --raw
    vredrs build . --release --jobs 8
    vredrs build . --clean
    vredrs build . --target aarch64-unknown-linux-gnu
    vredrs repl
    vredrs fmt --write main.veds
```

---

## File Suffixes

| Suffix | Meaning |
|--------|---------|
| `.veds` | Vredrs source — dynamic (VM) or native (LLVM) |
| `.vraw` | Vredrs raw source — static native (raw backend) |
| `.cpps` | Cstar source — bare-metal firmware (Cstar backend) |
| `vrs.toml` | Package manager configuration |
| `.vredrs-cache/` | Incremental build cache (mtime + hash) |

---

## Running Tests

```bash
cargo test --release --lib   # unit tests (213 tests)
./tests/run_tests.sh         # integration tests (builds & runs .veds files)
```

---

## Architecture

Vredrs has five main components:

1. **Frontend** (`src/lexer/`, `src/parser/`, `src/semantic/`) — tokenizes,
   parses, and semantically checks Vredrs source into an AST. The parser
   collects multiple errors before reporting. Supports trait/impl/dtor,
   `&T`/`&mut T`/`ptr[T]`/`u8`–`u64`/`Result[T,E]` type syntax.

2. **Bytecode VM** (`src/bytecode/`) — the default execution engine for
   `vredrs run`. Lowers the AST to compact bytecode (`compiler.rs`) and
   executes it on a stack-based interpreter (`vm.rs`) with call frames,
   generators, closures, exceptions, and function-body bytecode for the
   hot path.

3. **LLVM Backend** (`src/codegen/llvm/`, `src/codegen/llvm_full.rs`) —
   lowers the AST to LLVM IR text for `vredrs build <file.veds>`. Splits
   across 10 part files for maintainability.

4. **Raw Backend** (`src/codegen/cstar/raw/`) — **independent** native
   codegen for `.vraw` files. `x86.rs` emits x86_64 Intel-syntax assembly;
   `arm.rs` emits AArch64/ARM32 assembly; `emitter.rs` is the shared
   dispatcher. Linear type checking (`linear.rs`) enforces `ptr[T]`
   consumption. No GC, no scheduler, no runtime type tags.

5. **Cstar Backend** (`src/codegen/cstar/codegen.rs`, `linear.rs`,
   `pir.rs`, `advanced.rs`) — **independent** bare-metal backend for
   `.cpps` files. Lowers the AST through a Physical IR (PIR) to firmware.
   Advanced features: `@pipeline` (DMA orchestration), `@patch` (differential
   updates), `@isr_group` (interrupt clustering with WCET), `@prefetch`
   (cache hints), `@repo` (size-based Flash/SRAM partitioning).

6. **C Runtime** (`src/codegen/runtime/vredrs_runtime.c`,
   `src/codegen/runtime/include/vredrs.h`) — heap-allocated containers,
   object/vtable infrastructure, exception handling (setjmp/longjmp), and
   standard library functions for the LLVM native backend.

7. **Build Driver** (`src/driver/`) — `build.rs` implements the multi-mode
   project build (incremental cache, parallel compile, LTO, cross-compile);
   `file_classifier.rs` maps extensions to backends.

### Tagged Value System (LLVM native backend)
All dynamic values use a tagged union:
```c
typedef struct { int8_t tag; int64_t payload; } vredrs_value;
```
This allows containers to hold mixed-type elements without IR-level type
inference.

### Error Rendering (0.1.4)
Errors render with ANSI colors by default (red category header, cyan file
locator, white source line, red marker, green suggestion, magenta quote).
Set the `NO_COLOR` environment variable to disable color. Example:
```
-- Syntax Error ------------------------------------------- [V0010]

  +--[main.veds:3:5]
  |
  3 | /end
    |     ^^^^
    |
    \--  Unexpected /end

  Suggestion: Add the missing /end for the open block.
-- The Author insists: "If you have time to debug, you have time to /end."
```

---

## Documentation

- [SYNTAX_REFERENCE.txt](SYNTAX_REFERENCE.txt) — complete syntax reference
- [ERROR_HANDBOOK.txt](ERROR_HANDBOOK.txt) — error codes (C-series + V-series) and fixes
- [RELEASE_NOTES.md](RELEASE_NOTES.md) — version history and known limitations
- [新增.txt](新增.txt) — Cstar backend design document (Chinese)
- [语法.txt](语法.txt) — language overview (Chinese)

---

## Limitations (0.1.4)

- **Nested function definitions** — `fn` inside another `fn` body is not
  supported in the bytecode path; use top-level functions or lambdas.
- **Generator evaluation model** — generators are evaluated eagerly when
  created, so infinite generators are not possible.
- **AST fallbacks** — a few constructs (defer, complex match) still use the
  AST-interpreter path; pure functions use the bytecode fast path.
- **Native optimization level** — LLVM builds use `-O0` because
  setjmp-based exceptions break under `-O2`. The raw backend applies its
  own optimizations (recursion-to-iteration, register allocation).
- **Memory management** — refcounting for objects in the native backend;
  the bytecode VM uses Rust's ownership/Borrow semantics.
- **Termux/Android** — link with `-lm` for math functions (`fmod`, etc.).
  The build system auto-detects the host architecture.

---

## License

MIT Open Source License — see [LICENSE](LICENSE).

Copyright (c) 2026 Alan Chen.
