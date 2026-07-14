# Vredrs 1.4.5 — The Unified IR Release

**Version:** 1.4.5  
**Release Date:** 2026-07-14  
**Status:** Stable (production-ready for x86_64 and ARM)

---

Vredrs is a modern, multi‑paradigm programming language with a bytecode VM, an LLVM native backend, a **unified Static IR backend**, and a **Cstar bare‑metal backend** — all sharing one frontend.

Vredrs ships with a stack‑based **bytecode virtual machine** (`vredrs run`) that executes `.veds` source files directly. The `build` command now uses a **unified Static IR pipeline** (`lower.rs` → `ir.rs`) to generate:
- LLVM IR (via `clang`) for native executables,
- Raw machine code for `.vraw` files (x86_64 / AArch64 / ARM32),
- Bare‑metal firmware for `.cpps` files via the **Cstar** backend.

A multi‑mode **build system** drives incremental, parallel, cached project builds.

**Author:** Alan Chen  
**License:** MIT (see [LICENSE](LICENSE))  
**Version:** 1.4.5

---

## Three File Types, Three Independent Backends

Vredrs uses file extensions to route source through the right backend. Each backend is an **independent implementation** — they do not share codegen pipelines.

| Extension | Backend | Output | Entry Point |
|-----------|---------|--------|-------------|
| `.veds` | Bytecode VM (`run`) / LLVM native (`build`) | Executable | `vredrs run` / `vredrs build <file>` |
| `.vraw` | **Unified IR → Raw backend** — x86_64 / AArch64 / ARM32 native codegen | Native ELF executable | `vredrs build <file> --raw` |
| `.cpps` | **Cstar backend** — bare‑metal firmware (PIR → LLVM IR → `.bin`) | `.bin` / `.elf` firmware | `vredrs build <file> --cstar` |

> **raw ≠ Cstar.** The raw backend (`src/codegen/emit_raw_x86.rs`, `emit_raw_arm.rs`) generates native executables from `.vraw` files via the **Static IR** pipeline. The Cstar backend (`src/codegen/cstar/`) lowers `.cpps` files through a Physical IR (PIR) with linear types, DMA orchestration, and interrupt clustering. They are separate, independently‑maintained implementations that now share the same frontend IR.

---

## Features

### Language (shared across all backends)

- **Bytecode VM** — AST → bytecode compiler + stack‑based interpreter with call frames, generators, closures, exceptions, `Result` + `?`, and function‑body bytecode for the hot path (fib(25) faster than CPython 3.12).
- **Closures** — lambdas capture their lexical environment at creation time; each invocation gets its own capture map (**now properly isolated**; multiple closures no longer share state).
- **Classes & inheritance** — vtable‑based dispatch, `super` calls, `ClassName.new(...)` and `ClassName(...)` constructors, `__init__` support (fixed for deep inheritance chains).
- **Generators** — `yield` in any function; `next()` / `has_next()` / `reset()` / `to_list()` / `send()` methods.
- **Async/await** — `async fn` returns a coroutine; `await` polls it (synchronous in the VM).
- **Exception handling** — `try` / `catch` / `finally` / `throw` / `panic`.
- **Result + ? operator** — `Ok(v)` / `Err(e)`, `expr?` propagation, `is_ok` / `is_err` / `unwrap` / `unwrap_or` builtins.
- **Modules** — multi‑file programs with `import`, recursive resolution, 32‑module bundled stdlib.
- **Containers** — List, Dict, Tuple, Set, String with full method support.
- **Comprehensions** — list, dict, and set comprehensions.
- **Operator overloading** — `__add__`, `__sub__`, `__mul__`, `__getitem__`, `__setitem__`, `__enter__`, `__exit__`, `__setattr__`, `__len__`, `__str__`, `__call__`, `__eq__`.
- **Context managers** — `with` statement via `__enter__` / `__exit__`.
- **String interpolation** — `"Hello, {name}!"`.
- **Optional chaining** — `obj?.field`, `obj?.method()`, `obj?.[index]`.
- **Null coalescing** — `a ?? b`.
- **Slice syntax** — `list[1:4]`, `str[:5]`, `arr[::-1]`.
- **Annotations** — `@route("/")` + `annotations(fn)` reflection.
- **Pattern matching** — literal, binding, tuple, list, dict, or‑patterns.
- **Traits & impls** — `trait` / `impl` blocks for ad‑hoc polymorphism.
- **Destructors** — `dtor` blocks for resource cleanup.
- **Generics** — `fn, identity[T](x: T): T` with compile‑time monomorphization (LLVM + Raw backends). Generic structs `struct, Box[T]` and enums `enum, Option[T]` also supported. VM mode ignores generics (runtime dispatch).

### Raw static syntax (for `.vraw` files)

- **Pointer types** — `ptr[T]` with `load()` / `store()` / `load_acquire()` / `store_release()` atomic operations.
- **Borrow types** — `&T` (shared borrow), `&mut T` (mutable borrow).
- **Fixed‑width unsigned ints** — `u8`, `u16`, `u32`, `u64`.
- **Result type** — `Result[T, E]` for error propagation with `?` operator.
- **Linear type checking** — `ptr[T]` resources must be consumed before scope exit (enforced by `src/semantic/borrow_checker.rs`).
- **Control flow** — `loop`, `break` / `continue` (labeled and unlabeled), `for, i, in, a..b` (range), `for, ch, in, "string"`, `for, x, in, [list]`.
- **try/catch/throw/panic** — handler‑stack‑based exception handling with `.bss`‑resident `__handler_stack` array.
- **Closures** — no‑capture and capture closures compiled to independent functions; captures now use per‑call snapshots (fixed isolation).
- **Builtins** — `memcpy` / `memset`, `spin_lock` / `spin_unlock`, `tls_get` / `tls_set`, `cycle_counter`, `offsetof`, `static_assert`.
- **NEON/FPU** — float arithmetic via NEON (AArch64) and VFP (ARM32).
- **Recursion‑to‑iteration** — `fib(n)` pattern detected at compile time and lowered to an O(n) iterative loop.
- **Function inlining** — small functions inlined up to 3 levels deep.

### Cstar bare‑metal syntax (for `.cpps` files)

- `volatile`, `align(N)`, `asm { ... }`, `@section("name")`.
- `@pipeline` — compile‑time DMA pipeline orchestration.
- `@patch` — firmware differential (byte‑level) update generation.
- `@isr_group` — deterministic interrupt clustering with WCET budgets.
- `@prefetch` — hybrid memory cache prefetch hints.
- `@repo` — physical package management (size‑based Flash/SRAM partitioning).
- `@vector_table` — auto‑generated interrupt vector table.
- `@memory_map` — dynamic linker script generation.
- `@stack(size=N)` / `@heap(size=N)` — stack/heap size configuration.
- `@critical` — critical section with interrupt disable/restore.
- `@scheduler(tick_ms, max_tasks, stack_size)` — cooperative scheduler.
- `@breakpoint(addr=0xNNN)` — hardware data watchpoint registration.
- **Hardware drivers** — GPIO, UART, I2C, SPI, Flash, assert/log via UART.

### Toolchain

- **Bytecode VM** (`run` / `vm`) — the default execution engine.
- **REPL** (`repl`) — interactive console with history, tab completion, multi‑line blocks, cursor editing, and coloured prompts.
- **Native build** (`build <file>`) — LLVM IR → clang → executable (new IR‑based path).
- **Raw build** (`build <file> --raw`) — Static IR → direct x86_64/ARM codegen (fixed ARM and LLVM emitter bugs).
- **Cstar build** (`build <file> --cstar`) — Static IR → Cstar PIR → LLVM IR → firmware + linker script + patches.
- **IR dump** (`build <file> --ir`) — write the Static IR as a human‑readable `.ir` file.
- **Project build** (`build <dir>`) — multi‑mode build system with incremental cache and parallel compilation.
- **Package manager** (`mod`) — init / add / rm / list / tree / update.
- **Formatter** (`fmt`) — 4‑space indentation, `--check` / `--write`.
- **LSP** — language server (completion, hover, definition, diagnostics, formatting, document symbols).
- **DAP** — debug adapter protocol.
- **Test runner** (`test`) — `test` / `bench` blocks with assert/throw.
- **Platform detection** — auto‑detects Termux / WSL / macOS / Linux, coloured output, package manager hints.

### Error system

- **V‑series error codes** — phase‑and‑file‑type‑specific codes:
  - `V0001`–`V0999`: shared (lex/syntax/semantic/scope/pattern)
  - `V1000`–`V1999`: `.veds` interpreter/dynamic
  - `V2000`–`V2999`: `.vraw` static compilation (ownership/borrow/lifetime/linear)
  - `V3000`–`V3999`: `.cpps` bare‑metal (interrupt/memory‑map/DMA/linker)
  - `V4000`–`V4999`: shared file/IO/module
  - `V5000`–`V5999`: shared config/build/toolchain
- **ANSI coloured rendering** — with `NO_COLOR` environment variable support.
- **Source‑context diagnostics** — file:line:column, source line, `^^^` marker, suggestion, and an "Author insists" quote.

---

## Installation

### Prerequisites

- **Rust** 1.70+ (install via [rustup](https://rustup.rs))
- **clang** 15+ (for LLVM IR compilation; also used for linking on Termux)
- **GNU `as` + `ld`** (for raw x86_64/ARM backend; ships with binutils)
- **LLVM** 15+ (ships with clang on most platforms)

### Build

```bash
git clone https://github.com/bushidageniubi-spec/vredrs/vredrs.git
cd vredrs
cargo build --release