//! # Vredrs Compiler
//!
//! A dynamically-typed language with a native LLVM backend.
//!
//! ## Architecture
//!
//! - `frontend` (lexer, parser, semantic) — source text to typed AST
//! - `backend` (types, ir_emitter, driver) — AST to LLVM IR to native code
//! - `codegen` (llvm_full, interpreter, cstar) — code generation backends
//! - `runtime` (vredrs_runtime.c) — C runtime library for dynamic types
//!
//! ## Usage
//!
//! ```no_run
//! use vredrs_compiler::{compile_file, OutputType};
//! compile_file("main.veds", "app", OutputType::Exe).unwrap();
//! ```

#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(unused_assignments)]

pub mod backend;
pub mod bytecode;
pub mod codegen;
pub mod driver;
pub mod line_editor;
pub mod vpm;
pub mod fmt;
pub mod lsp;
pub mod dap;
pub mod error;
pub mod lexer;
pub mod parser;
pub mod semantic;
pub mod source_manager;

use crate::error::{CompilerError, Result};
use crate::parser::ast::Program;
use crate::parser::ast::TopLevel;
use crate::parser::ast::{BasicType, TypeExpr};
use crate::source_manager::SourceManager;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputType {
    Exe,
    LlvmIr,
    Cpp,
    Wasm,
    Python,
    Raw,
}

thread_local! {
    static SOURCE_MANAGER: std::cell::RefCell<SourceManager> = std::cell::RefCell::new(SourceManager::new());
}

/// 设置全局源码管理器的内容
/// Load source text into the global source manager for error rendering.
pub fn set_source(file_id: usize, path: String, content: String) {
    SOURCE_MANAGER.with(|mgr| {
        mgr.borrow_mut().load_file(file_id, path, content);
    });
}

/// 使用源码管理器渲染错误
/// Render a compiler error with source context from the global source manager.
pub fn render_error_with_source(err: &CompilerError) -> String {
    SOURCE_MANAGER.with(|mgr| err.render_colored(Some(&mgr.borrow())))
}

fn parse_program(source: &str, file_id: usize) -> Result<Program> {
    let mut lexer = lexer::Lexer::new(source, file_id);
    let tokens = lexer.tokenize()?;
    let mut parser = parser::Parser::new(tokens, file_id);
    parser.parse_program()
}

fn compile_to_llvm_ir(source: &str, output: &str, file_id: usize) -> Result<()> {
    let program = parse_program(source, file_id)?;

    // 语义分析（基础版本）
    let mut analyzer = semantic::SemanticAnalyzer::new();
    analyzer.analyze(&program)?;

    // Use the full LLVM backend (Phases 1-5).
    let mut gen = codegen::llvm_full::FullLlvmGen::new();
    let llvm_ir = gen.generate(&program)?;

    fs::write(output, &llvm_ir)
        .map_err(|e| CompilerError::io_error(format!("can't write LLVM IR: {}", e)))?;
    eprintln!("[vredrs] LLVM IR written to: {}", output);
    Ok(())
}

fn compile_to_cstar_raw_ir(source: &str, output: &str, file_id: usize) -> Result<()> {
    let output_path = Path::new(output);
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                CompilerError::io_error(format!("create raw output directory: {}", e))
            })?;
        }
    }

    let side_base = output_path.with_extension("");
    let bin_path = side_base.with_extension("bin");

    let program = match parse_program(source, file_id) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[vredrs] C* source parse failed: {}", e.message());
            return Err(e);
        }
    };

    // Try x86_64 native backend first (for local execution).
    let asm_path = side_base.with_extension("s");
    match codegen::cstar::raw::compile_to_x86_assembly(&program, &side_base) {
        Ok(()) => {
            eprintln!("[vredrs] Raw x86_64 native output written to: {}", side_base.display());
            // Also generate ARM firmware for QEMU.
            let _ = codegen::cstar::raw::build_firmware_with_program(&bin_path, &program);
            eprintln!("[vredrs] ARM firmware written to: {}", bin_path.display());
        }
        Err(e) => {
            eprintln!("[vredrs] x86 codegen error: {}", e);
            // Fall back to ARM-only.
            codegen::cstar::raw::build_firmware_with_program(&bin_path, &program).map_err(|e| {
                CompilerError::io_error(format!("can't write C* raw .bin: {}", e))
            })?;
            eprintln!("[vredrs] ARM firmware written to: {}", bin_path.display());
        }
    }

    // Generate LLVM IR text (for debugging).
    let ir = format!(
        "; Vredrs raw-mode output\n; x86_64 assembly: {}\n; ARM firmware: {}\n",
        asm_path.display(),
        bin_path.display()
    );
    fs::write(output, ir).map_err(|e| {
        CompilerError::io_error(format!("can't write raw IR: {}", e))
    })?;
    eprintln!("[vredrs] Raw IR written to: {}", output);
    Ok(())
}

fn temp_build_dir() -> PathBuf {
    let mut base = std::env::temp_dir();
    let stamp = format!(
        "vredrs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    base.push(stamp);
    base
}

fn build_native_executable(
    source_path: &str,
    source: &str,
    output: &str,
    _file_id: usize,
) -> Result<()> {
    let pipeline = backend::driver::BuildPipeline::new();
    pipeline.run(source_path, source, output)
}

/// 编译源文件并输出到指定路径。
/// Compile a Vredrs source file to the specified output format.
///
/// # Arguments
/// * `path` - Path to the `.veds` source file
/// * `output` - Output file path
/// * `output_type` - `Exe` for native executable, `LlvmIr` for `.ll` file
pub fn compile_file(path: &str, output: &str, output_type: OutputType) -> Result<()> {
    let source = fs::read_to_string(path)
        .map_err(|e| CompilerError::io_error(format!("can't read '{}': {}", path, e)))?;

    // 加载源码到全局管理器
    let file_id = 0;
    set_source(file_id, path.to_string(), source.clone());

    let result = match output_type {
        OutputType::Exe => build_native_executable(path, &source, output, file_id),
        OutputType::LlvmIr => compile_to_llvm_ir(&source, output, file_id),
        OutputType::Cpp => Err(CompilerError::codegen_error(
            "unsupported backend 'cpp'; supported backends are native and llvm-ir",
        )),
        OutputType::Wasm => Err(CompilerError::codegen_error(
            "unsupported backend 'wasm'; supported backends are native and llvm-ir",
        )),
        OutputType::Python => Err(CompilerError::codegen_error(
            "unsupported backend 'python'; supported backends are native and llvm-ir",
        )),
        OutputType::Raw => compile_to_cstar_raw_ir(&source, output, file_id),
    };

    // 如果出错，使用增强的错误显示
    if let Err(ref err) = result {
        eprintln!("{}", render_error_with_source(err));
        return result;
    }

    result
}

/// Execute a .veds file using the bytecode VM.
///
/// The bytecode VM supports the full language subset that the 10 example
/// programs exercise: arithmetic, control flow, functions (with recursion),
/// classes (with inheritance and methods), generators, exceptions, modules,
/// comprehensions, and all builtins.
pub fn run_bytecode_vm(path: &str) -> Result<()> {
    let source = fs::read_to_string(path)
        .map_err(|e| CompilerError::io_error(format!("can't read '{}': {}", path, e)))?;
    let file_id = 0;
    set_source(file_id, path.to_string(), source.clone());
    let original_dir = std::env::current_dir().ok();
    let base_dir = if let Some(dir) = Path::new(path).parent() {
        if !dir.as_os_str().is_empty() {
            let canonical = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
            let _ = std::env::set_current_dir(&canonical);
            canonical
        } else {
            std::path::PathBuf::from(".")
        }
    } else {
        std::path::PathBuf::from(".")
    };
    let result = (|| {
        let program = parse_program(&source, file_id)?;
        let compiler = bytecode::Compiler::new();
        let module = compiler.compile(&program)?;
        let mut vm = bytecode::VM::new(module)
            .with_program(program)
            .with_base_dir(base_dir);
        vm.run()?;
        Ok(())
    })();
    // Always restore the working directory, even on error.
    if let Some(dir) = original_dir {
        let _ = std::env::set_current_dir(dir);
    }
    result
}

pub fn compile_source(source: &str) -> Result<Vec<lexer::token::Token>> {
    let mut lexer = lexer::Lexer::new(source, 0);
    lexer.tokenize()
}

/// Execute Vredrs source text directly via the bytecode VM (for benchmarks).
pub fn run_bytecode_vm_source(source: &str) -> Result<()> {
    let file_id = 0;
    let program = parse_program(source, file_id)?;
    let compiler = bytecode::Compiler::new();
    let module = compiler.compile(&program)?;
    let mut vm = bytecode::VM::new(module).with_program(program);
    vm.run().map(|_| ())
}

/// Run the interactive REPL (read-eval-print loop).
///
/// Behaviour mirrors a Python-style REPL:
/// - `>>>` is the primary prompt; `...` is the continuation prompt for
///   multi-line block input (if/for/fn/class/with/try/while/loop).
/// - Variables and functions defined in one input persist across
///   subsequent inputs (the VM's globals are carried forward).
/// - A bare expression like `1 + 2` is evaluated and its result printed.
/// - `exit()` or Ctrl+D quits.
/// - `help()` prints usage.
/// - Command history: Up/Down arrows navigate previous inputs.
/// - Tab completion for builtins and known globals.
/// - Left/Right arrows move the cursor within the current line.
pub fn run_repl() -> Result<()> {
    use std::io::Write;

    let file_id = 0;
    let mut stdout = std::io::stdout();
    let mut editor = line_editor::LineEditor::new();

    // Accumulated function/class definitions from previous inputs, so
    // that `fn, foo() … /end` entered earlier remains callable later.
    let mut accumulated_decls: Vec<TopLevel> = Vec::new();
    // Persisted globals across inputs.
    let mut saved_globals: std::collections::HashMap<String, bytecode::vm::Value> =
        std::collections::HashMap::new();

    writeln!(
        stdout,
        "Vredrs 0.1.2 REPL — type 'exit()' or Ctrl+D to quit, 'help()' for help."
    )
    .ok();
    let _ = stdout.flush();

    loop {
        // Read a (possibly multi-line) input using the line editor.
        let mut input = String::new();
        let line = match editor.read_line(">>> ") {
            Some(l) => l,
            None => {
                // Ctrl+D / EOF.
                writeln!(stdout).ok();
                break;
            }
        };
        input.push_str(&line);
        input.push('\n');

        // Multi-line continuation: if the input contains a block keyword
        // that is not yet closed by /end, keep reading with the
        // continuation prompt until /end is seen.
        while needs_continuation(&input) {
            let cont = match editor.read_line("... ") {
                Some(l) => l,
                None => break,
            };
            input.push_str(&cont);
            input.push('\n');
        }

        let trimmed = input.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Exit.
        if trimmed == "exit()" || trimmed == "exit" || trimmed == "quit()" || trimmed == "quit" {
            break;
        }
        // Help.
        if trimmed == "help()" || trimmed == "help" {
            print_repl_help(&mut stdout);
            continue;
        }

        // Determine whether the input is a bare expression or a statement.
        let source = if is_bare_expression(trimmed) {
            format!("println, {}\n", trimmed)
        } else {
            input.clone()
        };

        // Parse the input.
        let program = match parse_program(&source, file_id) {
            Ok(p) => p,
            Err(e) => {
                writeln!(stdout, "Error: {}", e.message()).ok();
                continue;
            }
        };

        // Merge accumulated function/class definitions with the new input.
        let mut all_decls = Vec::new();
        let mut new_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for d in &program.declarations {
            if let TopLevel::FnDef(f) = d {
                new_names.insert(f.name.name.clone());
            } else if let TopLevel::ClassDef(c) = d {
                new_names.insert(c.name.name.clone());
            }
            all_decls.push(d.clone());
        }
        for d in &accumulated_decls {
            let name = match d {
                TopLevel::FnDef(f) => Some(f.name.name.clone()),
                TopLevel::ClassDef(c) => Some(c.name.name.clone()),
                _ => None,
            };
            if let Some(n) = name {
                if !new_names.contains(&n) {
                    all_decls.push(d.clone());
                }
            }
        }

        let full_program = Program {
            declarations: all_decls,
            span: crate::error::Span::dummy(),
        };

        // Compile and run.
        let compiler = bytecode::Compiler::new();
        let module = match compiler.compile(&full_program) {
            Ok(m) => m,
            Err(e) => {
                writeln!(stdout, "Error: {}", e.message()).ok();
                continue;
            }
        };
        let mut vm = bytecode::VM::new(module).with_program(full_program);
        // Restore persisted globals.
        for (k, v) in &saved_globals {
            vm.set_global(k, v.clone());
        }
        match vm.run() {
            Ok(_) => {
                saved_globals = vm.globals_clone();
                for d in &program.declarations {
                    if matches!(d, TopLevel::FnDef(_)) || matches!(d, TopLevel::ClassDef(_)) {
                        accumulated_decls.push(d.clone());
                    }
                }
            }
            Err(e) => {
                let msg = e.message();
                if msg != "return" {
                    writeln!(stdout, "Error: {}", msg).ok();
                }
                saved_globals = vm.globals_clone();
            }
        }
        let _ = stdout.flush();
    }

    Ok(())
}

/// Heuristic: does the input need multi-line continuation? Returns true if
/// the input contains a block keyword (if, for, fn, class, with, try,
/// while, loop) that is not yet closed by a matching `/end`. We track a
/// simple depth counter: each block-opening keyword increments it, each
/// `/end` line decrements it. The input needs continuation while depth > 0.
fn needs_continuation(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return false;
    }
    let block_keywords = [
        "if,", "for,", "fn,", "class,", "with,", "try,", "while,", "loop,", "match,",
    ];
    let mut depth: i32 = 0;
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // A `/end` line closes one block.
        if line == "/end" || line.starts_with("/end") {
            depth -= 1;
            continue;
        }
        // A block-opening keyword opens one block. We check the first
        // token of the line (up to the first comma or whitespace).
        for kw in &block_keywords {
            if line.starts_with(kw) {
                depth += 1;
                break;
            }
        }
    }
    depth > 0
}

/// Heuristic: is the input a bare expression (not a statement)?
/// A bare expression is something that doesn't start with a statement
/// keyword and looks like an expression. We try to parse it as a program;
/// if it fails, it's not a bare expression. But to avoid double-parsing,
/// we use a keyword heuristic: if the first token is a statement keyword
/// (set, println, paste, if, for, fn, etc.), it's a statement; otherwise
/// it's a bare expression.
fn is_bare_expression(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return false;
    }
    let stmt_keywords = [
        "set,", "println,", "paste,", "if,", "for,", "fn,", "class,", "with,",
        "try,", "while,", "loop,", "match,", "return,", "break,", "continue,",
        "throw,", "defer,", "assert,", "panic,", "yield,", "spawn,", "input,",
        "flush,", "del,", "import,", "export,", "async", "struct,", "enum,",
        "trait,", "impl,", "type,", "const,", "lazy,", "macro,", "plugin,",
        "extern,", "@",
    ];
    for kw in &stmt_keywords {
        if trimmed.starts_with(kw) {
            return false;
        }
    }
    // Also treat lines that are just a label or directive as statements.
    if trimmed.starts_with('/') {
        return false;
    }
    true
}

fn print_repl_help(stdout: &mut impl std::io::Write) {
    writeln!(stdout, "Vredrs REPL — available commands and syntax:").ok();
    writeln!(stdout, "  exit() / quit() / Ctrl+D  — exit the REPL").ok();
    writeln!(stdout, "  help()                   — show this help").ok();
    writeln!(stdout, "").ok();
    writeln!(stdout, "Statements:").ok();
    writeln!(stdout, "  set, x, 42               — assign a variable").ok();
    writeln!(stdout, "  println, expr            — print with newline").ok();
    writeln!(stdout, "  paste, expr              — print without newline").ok();
    writeln!(stdout, "  if, cond  ...  /end      — if block (multi-line)").ok();
    writeln!(stdout, "  while, cond  ...  /end   — while loop").ok();
    writeln!(stdout, "  for, x, in, iter  /end   — for-in loop").ok();
    writeln!(stdout, "  fn, name(params)  ...  /end  — function definition").ok();
    writeln!(stdout, "  class, Name  ...  /end   — class definition").ok();
    writeln!(stdout, "  try  ...  catch, e  ...  /end  — try/catch").ok();
    writeln!(stdout, "  throw, \"msg\"             — throw exception").ok();
    writeln!(stdout, "  return, expr             — return from function").ok();
    writeln!(stdout, "").ok();
    writeln!(stdout, "Expressions:").ok();
    writeln!(stdout, "  1 + 2 * 3                — arithmetic").ok();
    writeln!(stdout, "  \"hello\"                  — string literal").ok();
    writeln!(stdout, "  [1, 2, 3]                — list literal").ok();
    writeln!(stdout, "  {{\"key\": \"value\"}}        — dict literal").ok();
    writeln!(stdout, "  fib(10)                  — function call").ok();
    writeln!(stdout, "  x ?? y                   — null coalesce").ok();
    writeln!(stdout, "  obj?.field               — optional chain").ok();
    writeln!(stdout, "").ok();
    writeln!(stdout, "Builtins: len, str, int, float, range, sum, min, max,").ok();
    writeln!(stdout, "          sorted, reversed, map, filter, type_of, ...").ok();
    writeln!(stdout, "").ok();
    let _ = stdout.flush();
}
