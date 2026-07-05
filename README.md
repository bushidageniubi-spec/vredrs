# Vredrs 0.1.2

A modern, dynamically-typed programming language with a bytecode VM, an LLVM
backend, and a raw bare-metal backend.

Vredrs ships with a stack-based **bytecode virtual machine** (`vredrs run` /
`vredrs vm`) that executes `.veds` source files directly — this is the
default and only execution engine for `run`. The `build` command lowers the
AST to LLVM IR (via clang) for native executables, or to a raw Cstar image
for bare-metal targets.

**Author:** Alan Chen  
**License:** MTI (see [LICENSE](LICENSE))  
**Version:** 0.1.2

## Features

- **Bytecode VM** — AST → bytecode compiler + stack-based interpreter with
  call frames, generators, and an `EvalAst`/`ExecAstStmt` fallback for the
  long tail of language constructs.
- **Call-frame isolation** — each function invocation gets its own local
  variable scope, so recursion (even with locals introduced via `set`) and
  re-entrant method calls do not clobber each other's variables.
- **Closures** — lambdas (`fn(x) x + n`) capture their lexical environment
  at creation time; the captured bindings remain visible after the enclosing
  function returns.
- **Classes & inheritance** — vtable-based dispatch, dynamic fields, `super`
  calls, `ClassName.new(...)` and `ClassName(...)` constructor forms.
- **Generators** — `yield` in any function creates a generator; yields inside
  `while`/`for`/`loop` bodies are all collected (eager evaluation).
- **Async/await** — `async fn` returns a coroutine; `await` polls it
  (executed synchronously in the single-threaded VM).
- **Exception handling** — `try`/`catch`/`finally`/`throw`.
- **Modules** — multi-file programs with `import`, recursive resolution,
  bundled stdlib (`math`, `io`, `fs`, `fmt`, `collections`, `json`, `os`,
  `time`).
- **Containers** — List, Dict, Tuple, Set, String with full operations.
- **Comprehensions** — list, dict, and set comprehensions.
- **Operator overloading** — `__add__`, `__sub__`, `__mul__`, `__getitem__`,
  `__setitem__`, `__delitem__`, `__enter__`, `__exit__`, `__setattr__`,
  `__len__`, `__str__`, `__call__`.
- **Context managers** — `with` statement via `__enter__`/`__exit__` (also
  works with file handles).
- **Lambdas** — `fn(x, y) x + y` with full lexical-environment capture.
- **String interpolation** — `"Hello, {name}!"`.
- **String repeat** — `"ab" * 3` produces `"ababab"` (and `3 * "ab"`).
- **Optional chaining** — `obj?.field`, `obj?.method(args)`, `obj?.[index]`,
  with short-circuit to `null` on a null target. Works on objects, modules,
  and dicts.
- **Null coalescing** — `a ?? b`.
- **Slice syntax** — `list[1:4]`, `str[:5]`, `arr[::-1]`.
- **Annotation reflection** — `@route("/")` + `annotations(fn)` returns a
  dict.
- **map/filter** — higher-order builtins accepting lambdas.
- **Standard library** — `len`, `str`, `int`, `range`, `print`, file I/O,
  math, fs, json, and more.

## Installation

### Prerequisites

- **Rust** 1.70+ (install via [rustup](https://rustup.rs))
- **clang** 15+ (for LLVM IR compilation to native executable)
- **LLVM** 15+ (ships with clang on most platforms)

### Build

```bash
git clone https://github.com/alanchen/vredrs.git
cd vredrs
cargo build --release
```

The `vredrs` binary will be at `target/release/vredrs`.

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

Output:
```
Hello, World!
sum = 15
1 2 3 4 5
```

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

`=` may also be used as the assignment operator: `set, x = 42`.

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

Function bodies have their own call frame: parameters and any locals
introduced with `set` are isolated per invocation, so recursive calls do not
overwrite each other's variables.

### Lambdas (with closure capture)

```
set, add, fn(x, y) x + y
set, result, add(3, 4)          # 7
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
    fn, new(name)
        set, self.name, name
        return, self
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

Both `Dog.new("Rex")` and `Dog("Rex")` instantiate the class; `self.field =
value` mutations persist across method calls via shared field maps.

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

fn, fib_gen()
    set, a, 0
    set, b, 1
    while, b < 100
        yield, b
        set, tmp, a + b
        set, a, b
        set, b, tmp
    /end
/end
# All yields inside the while loop are collected.

async fn, fetch()
    return, 42
/end
set, val, await fetch()
paste, "{val}\n"   # 42
```

### Context Managers

```
# File handle form
with, open("data.txt", "w"), f
    f.write("hello")
/end

# Object form
class, CM
    fn, __enter__()
        return, "entered"
    /end
    fn, __exit__(err)
        paste, "exited\n"
    /end
/end
with, CM(), v
    paste, "{v}\n"
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

`math_lib.veds`:
```
fn, square(x)
    return, x * x
/end
export, square
```

`main.veds`:
```
import, "math_lib.veds", square
paste, square(5)   # 25
```

Bundled stdlib modules are imported by short name:
```
import, "math", sqrt, pow
paste, sqrt(16)    # 4.0
```

### Comprehensions

```
set, nums, [1, 2, 3, 4, 5]
set, squares, [x * x for x in nums]
set, evens, [x for x in nums if x % 2 == 0]
```

## Standard Library

| Function | Description |
|----------|-------------|
| `len(x)` | Length of string/list/dict/tuple |
| `str(x)` | Convert to string |
| `int(x)` | Convert to integer |
| `float(x)` | Convert to float |
| `bool(x)` | Convert to boolean |
| `range(a, b)` | List of integers from a to b-1 |
| `sum(l)` | Sum of list elements |
| `min(l)` / `max(l)` | Minimum/maximum of list |
| `sorted(l)` | Sorted copy of list |
| `reversed(l)` | Reversed copy of list |
| `map(fn, l)` | Apply fn to each element |
| `filter(fn, l)` | Keep elements where fn is truthy |
| `print(x)` / `paste(x)` | Print without newline |
| `println(x)` | Print with newline |
| `input(prompt)` | Read a line from stdin |
| `open(path, mode)` | Open a file |
| `read(handle, n)` | Read from file |
| `write(handle, str)` | Write to file |
| `close(handle)` | Close file handle |
| `read_file(path)` | Read entire file |
| `write_file(path, content)` | Write entire file |
| `file_exists(path)` | Check if file exists |
| `read_dir(path)` | List directory entries |
| `is_dir(path)` / `is_file(path)` | Path type checks |
| `path_join(a, b)` / `basename(p)` / `dirname(p)` | Path helpers |
| `dict_keys(d)` / `dict_values(d)` / `dict_has(d, k)` | Dict helpers |
| `dict_get(d, k)` / `dict_set(d, k, v)` | Dict access |
| `split(s, sep)` / `join(l, sep)` | String split/join |
| `trim(s)` / `upper(s)` / `lower(s)` | String transforms |
| `contains(haystack, needle)` | Membership test |
| `annotations(fn)` | Dict of function's annotations |
| `freeze(x)` / `is_frozen(x)` | Make / test immutability |
| `type_of(x)` | Type name as a string |
| `abs(x)` / `floor(x)` / `ceil(x)` / `round(x)` | Numeric helpers |
| `exit(code)` | Exit program |

Standard library modules (import by name): `io`, `fs`, `math`, `fmt`,
`collections`, `json`, `os`, `time`.

## CLI Usage

```
vredrs <COMMAND> [OPTIONS]

COMMANDS:
    run <file>              Execute a .veds file (bytecode VM)
    vm  <file>              Execute a .veds file (bytecode VM, alias)
    build <file> [opts]     Compile to native executable
    test <path>             Run tests
    mod <init|add|rm|list|tree|update>  Package manager
    fmt [opts] <file>       Format source code
    lsp                     Language server (stdio)
    dap                     Debug adapter (stdio)

BUILD OPTIONS:
    -o, --output <file>     Output path
    -b, --backend <name>    native | llvm-ir | raw

FMT OPTIONS:
    --write, -w             Overwrite file
    --check, -c             Check only (exit 1 if needs formatting)

EXAMPLES:
    vredrs run main.veds
    vredrs build main.veds -o app
    vredrs build main.veds -o output.ll -b llvm-ir
    vredrs fmt --write main.veds
```

## File Suffixes

| Suffix | Meaning |
|--------|---------|
| `.veds` | Vredrs source file (application / library / stdlib) |
| `.cpps` | Cstar bare-metal source file |
| `vrs.toml` | Package manager configuration |

## Running Tests

```bash
cargo test --release --lib   # unit tests (187 tests)
./tests/run_tests.sh         # integration tests (builds & runs .veds files)
```

## Architecture

Vredrs has four main components:

1. **Frontend** (`src/lexer/`, `src/parser/`, `src/semantic/`) — tokenizes,
   parses, and type-checks Vredrs source code into an AST. The parser
   collects multiple errors before reporting.

2. **Bytecode VM** (`src/bytecode/`) — the default execution engine for
   `vredrs run`. Lowers the AST to a compact bytecode (`compiler.rs`) and
   executes it on a stack-based interpreter (`vm.rs`) with call frames,
   generators, closures, and an `EvalAst`/`ExecAstStmt` fallback for
   constructs without dedicated opcodes.

3. **LLVM Backend** (`src/codegen/llvm/`, `src/codegen/llvm_full.rs`) —
   lowers the AST to LLVM IR text for `vredrs build`. Emits typed LLVM
   instructions for each language construct.

4. **C Runtime** (`src/codegen/runtime/vredrs_runtime.c`,
   `src/codegen/runtime/include/vredrs.h`) — provides heap-allocated
   containers (string, list, dict, tuple), object/vtable infrastructure,
   exception handling (setjmp/longjmp), and the standard library functions
   for the native backend. Linked with the generated LLVM IR by clang.

### Tagged Value System (native backend)

All dynamic values use a tagged union:
```c
typedef struct { int8_t tag; int64_t payload; } vredrs_value;
```

This allows containers to hold mixed-type elements without IR-level type
inference.

## Documentation

- [SYNTAX_REFERENCE.txt](SYNTAX_REFERENCE.txt) — complete syntax reference
- [ERROR_HANDBOOK.txt](ERROR_HANDBOOK.txt) — error codes and fixes
- [RELEASE_NOTES.md](RELEASE_NOTES.md) — version history and known limitations

## Limitations (0.1.2)

- **Nested function definitions** — `fn` inside another `fn` body is not
  supported; use top-level functions or lambdas (`fn(x) ...`).
- **Generator evaluation model** — generators are evaluated eagerly when
  created (all yields collected up front), so infinite generators are not
  possible.
- **Backend module split** — `llvm_full.rs` is a single large file; a future
  release will split it into per-feature submodules.
- **Cstar raw firmware** — `vredrs build --raw` for bare-metal targets is
  experimental.
- **Optimization level** — native builds use `-O0` because setjmp-based
  exceptions break under `-O2`.
- **Memory management** — simple refcounting for objects; strings, lists,
  and dicts are not fully GC'd in the native backend (the bytecode VM uses
  Rust's ownership/Borrow semantics).

## License

MTI Open Source License — see [LICENSE](LICENSE).

Copyright (c) 2026 Alan Chen.
