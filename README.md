# Vredrs 0.1.1

A modern, dynamically-typed programming language with a native LLVM backend.

Vredrs compiles to LLVM IR, which is then lowered to a native executable by clang. It features classes with inheritance, generators, exception handling, modules, lambdas, string interpolation, slices, annotation reflection, and a comprehensive standard library.

**Author:** Alan Chen  
**License:** MTI (see [LICENSE](LICENSE))  
**Version:** 0.1.1

## Features

- **Native compilation** — compiles to LLVM IR → native executable via clang
- **Dynamic typing** — tagged value system (`%vredrs.value = { i8 tag, i64 payload }`)
- **Classes & inheritance** — vtable-based dispatch, dynamic fields, `super` calls
- **Generators** — `yield` in any function creates a generator coroutine
- **Async/await** — `async fn` returns a coroutine; `await` polls it
- **Exception handling** — `try`/`catch`/`finally`/`throw` via setjmp/longjmp
- **Modules** — multi-file programs with `import`, recursive resolution
- **Containers** — List, Dict, Tuple, Set, String with full operations
- **Comprehensions** — list, dict, and set comprehensions
- **Operator overloading** — `__add__`, `__getitem__`, `__call__`, etc.
- **Context managers** — `with` statement via `__enter__`/`__exit__` (also works with file handles)
- **Lambdas** — `fn(x, y) x + y` (parameter-only captures in 0.1.1)
- **String interpolation** — `"Hello, {name}!"`
- **Slice syntax** — `list[1:4]`, `str[:5]`, `arr[::-1]`
- **Annotation reflection** — `@route("/")` + `annotations(fn)` returns a dict
- **map/filter** — higher-order builtins accepting lambdas
- **Standard library** — `len`, `str`, `int`, `range`, `print`, file I/O, math, fs, json, and more

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
set, nums = [1, 2, 3, 4, 5]
paste, "sum = {sum(nums)}\n"
for, n, in, nums
    paste, "{n} "
/end
paste, "\n"
```

Run via interpreter:
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
set, x = 42
set, name = "Alice"
set, pi = 3.14
set, flag = true
set, items = [1, 2, 3]
set, config = {"port": 8080, "host": "localhost"}
```

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

### Lambdas

```
set, add = fn(x, y) x + y
set, result = add(3, 4)        # 7
set, doubled = map(fn(x) x * 2, [1, 2, 3])
```

### Classes

```
class, Animal
    fn, new(name)
        set, self.name = name
    /end
    fn, speak()
        paste, "{self.name} makes a sound\n"
    /end
/end

class, Dog, extends, Animal
    fn, speak()
        paste, "{self.name} barks\n"
    /end
/end

set, d = Dog("Rex")
d.speak()
```

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

set, c = counter()
paste, resume(c)   # 1
paste, resume(c)   # 2

async fn, fetch()
    return, 42
/end
set, val = await fetch()
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

### Slices & Interpolation

```
set, lst = [1, 2, 3, 4, 5]
set, sub = lst[1:4]       # [2, 3, 4]
set, rev = lst[::-1]      # [5, 4, 3, 2, 1]
set, s = "hello world"
set, head = s[:5]         # "hello"
set, name = "Vredrs"
set, msg = "Hello, {name}!"  # "Hello, Vredrs!"
```

### Annotations

```
@route("/")
fn, index()
    return, "home"
/end
set, anns = annotations(index)
paste, anns["route"]      # "/"
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

### Comprehensions

```
set, nums = [1, 2, 3, 4, 5]
set, squares = [x * x for x in nums]
set, evens = [x for x in nums if x % 2 == 0]
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
| `annotations(fn)` | Dict of function's annotations |
| `freeze(x)` | Make value immutable |
| `exit(code)` | Exit program |

Standard library modules (import by name): `io`, `fs`, `math`, `fmt`, `collections`, `json`, `os`, `time`.

## CLI Usage

```
vredrs <COMMAND> [OPTIONS]

COMMANDS:
    run <file>              Execute a .veds file (interpreter mode)
    build <file> [opts]     Compile to native executable
    test <path>             Run tests
    mod <init|add|rm|list>  Package manager
    fmt [opts] <file>       Format source code
    lsp                     Language server (stdio)
    help                    Print help

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
cargo test --lib              # unit tests (194 tests)
./tests/run_tests.sh          # integration tests
```

## Architecture

Vredrs has four main components:

1. **Frontend** (`src/lexer/`, `src/parser/`, `src/semantic/`) — tokenizes, parses, and type-checks Vredrs source code into an AST. The parser collects multiple errors before reporting.

2. **Interpreter** (`src/interpreter/`) — AST-walking interpreter for development mode (`vredrs run`).

3. **LLVM Backend** (`src/codegen/llvm_full.rs`) — lowers the AST to LLVM IR text. Emits typed LLVM instructions for each language construct.

4. **C Runtime** (`src/codegen/runtime/vredrs_runtime.c`) — provides heap-allocated containers (string, list, dict, tuple), object/vtable infrastructure, exception handling (setjmp/longjmp), and all standard library functions. Linked with the generated LLVM IR by clang.

### Tagged Value System

All dynamic values use a tagged union:
```c
typedef struct { int8_t tag; int64_t payload; } vredrs_value;
```

Tag constants: 0=nil, 1=i64, 2=f64, 3=bool, 4=str, 5=list, 6=dict, 7=tuple, 8=object, 9=coroutine.

This allows containers to hold mixed-type elements without IR-level type inference.

## Documentation

- [SYNTAX_REFERENCE.txt](SYNTAX_REFERENCE.txt) — complete syntax reference
- [ERROR_HANDBOOK.txt](ERROR_HANDBOOK.txt) — error codes and fixes
- [RELEASE_NOTES.md](RELEASE_NOTES.md) — version history and known limitations

## Limitations (0.1.1)

- **Lambda captures** — closures may only reference their own parameters; capturing outer-scope variables is not yet implemented.
- **Backend module split** — `llvm_full.rs` is a single 6,500-line file; a future release will split it into per-feature submodules.
- **Bytecode VM** — the interpreter is still AST-walking; a self-hosted bytecode VM is planned.
- **Cstar raw firmware** — `vredrs build --raw` for bare-metal targets is experimental.
- **Optimization level** — native builds use `-O0` because setjmp-based exceptions break under `-O2`.
- **String methods** — only `len`, `upper`, `lower` are wired to the native backend; `split`, `join`, `replace` require interpreter mode.
- **Memory management** — simple refcounting for objects; strings, lists, and dicts are not fully GC'd.

## License

MTI Open Source License — see [LICENSE](LICENSE).

Copyright (c) 2026 Alan Chen.
