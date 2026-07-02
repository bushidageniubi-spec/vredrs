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
pub mod interpreter;
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
    SOURCE_MANAGER.with(|mgr| err.render_with_source(Some(&mgr.borrow())))
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

    // Attempt to parse the Cstar source. If parsing succeeds, compile
    // the user code to ARM instructions and embed it in the .bin.
    // If parsing fails, generate a stub firmware.
    let side_base = output_path.with_extension("");
    let bin_path = side_base.with_extension("bin");

    let program = match parse_program(source, file_id) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[vredrs] C* source parse failed (generating stub .bin): {}", e.message());
            codegen::cstar::raw::build_firmware(&bin_path).map_err(|e2| {
                CompilerError::io_error(format!("can't write C* raw .bin: {}", e2))
            })?;
            eprintln!("[vredrs] C* raw .bin written to: {}", bin_path.display());
            let ir = "; Cstar raw LLVM IR\n; Source parsing failed; .bin is a stub firmware.\n";
            fs::write(output, ir).map_err(|e2| {
                CompilerError::io_error(format!("can't write C* raw LLVM IR: {}", e2))
            })?;
            eprintln!("[vredrs] C* raw LLVM IR written to: {}", output);
            return Ok(());
        }
    };

    // Compile the user's Cstar code to ARM and generate the .bin.
    codegen::cstar::raw::build_firmware_with_program(&bin_path, &program).map_err(|e| {
        CompilerError::io_error(format!("can't write C* raw .bin: {}", e))
    })?;
    eprintln!("[vredrs] C* raw .bin written to: {} (user code compiled to ARM)", bin_path.display());

    // C* raw backend: PIR lowering, linear ownership check, etc.
    let mut cstar = codegen::cstar::CstarCompiler::new();
    let artifacts = match cstar.generate_artifacts_from_ast_with_source(&program, source) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[vredrs] C* codegen failed (firmware .bin still generated): {}", e.message());
            let ir = "; Cstar raw LLVM IR\n; Codegen failed; .bin is a stub firmware.\n";
            fs::write(output, ir).map_err(|e| {
                CompilerError::io_error(format!("can't write C* raw LLVM IR: {}", e))
            })?;
            eprintln!("[vredrs] C* raw LLVM IR written to: {}", output);
            return Ok(());
        }
    };

    fs::write(output, &artifacts.llvm_ir)
        .map_err(|e| CompilerError::io_error(format!("can't write C* raw LLVM IR: {}", e)))?;

    if let Some(ld) = artifacts.linker_script {
        let ld_path = side_base.with_extension("ld");
        fs::write(&ld_path, ld)
            .map_err(|e| CompilerError::io_error(format!("can't write C* linker script: {}", e)))?;
    }
    if let Some(header) = artifacts.package_header {
        let h_path = side_base.with_file_name("flash_layout.h");
        fs::write(&h_path, header).map_err(|e| {
            CompilerError::io_error(format!("can't write C* package header: {}", e))
        })?;
    }
    if let Some(map) = artifacts.layout_map {
        let map_path = side_base.with_file_name("layout.map");
        fs::write(&map_path, map)
            .map_err(|e| CompilerError::io_error(format!("can't write C* layout map: {}", e)))?;
    }
    if let Some(index) = artifacts.package_index {
        let idx_path = side_base.with_file_name("pkg_index.bin");
        fs::write(&idx_path, index)
            .map_err(|e| CompilerError::io_error(format!("can't write C* package index: {}", e)))?;
    }
    for patch in artifacts.patches {
        let patch_path = output_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(&patch.path);
        fs::write(&patch_path, patch.bytes).map_err(|e| {
            CompilerError::io_error(format!(
                "can't write C* patch artifact '{}': {}",
                patch_path.display(),
                e
            ))
        })?;
    }

    eprintln!("[vredrs] C* raw LLVM IR written to: {}", output);
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

/// 解释执行 Vredrs 源文件（开发模式）
/// Interpret a Vredrs source file (development mode, no compilation).
pub fn run_interpreter(path: &str) -> Result<()> {
    let source = fs::read_to_string(path)
        .map_err(|e| CompilerError::io_error(format!("can't read '{}': {}", path, e)))?;

    // 加载源码到全局管理器
    let file_id = 0;
    set_source(file_id, path.to_string(), source.clone());

    let original_dir = std::env::current_dir().ok();
    if let Some(dir) = Path::new(path).parent() {
        if !dir.as_os_str().is_empty() {
            let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
            let _ = std::env::set_current_dir(dir);
        }
    }
    let result = run_interpreter_source(&source, file_id);
    if let Some(dir) = original_dir {
        let _ = std::env::set_current_dir(dir);
    }

    // 如果出错，使用增强的错误显示
    if let Err(ref err) = result {
        eprintln!("{}", render_error_with_source(err));
        return result;
    }

    result
}

/// 直接解释执行源代码字符串。
/// Interpret Vredrs source text directly.
pub fn run_interpreter_source(source: &str, file_id: usize) -> Result<()> {
    let program = parse_program(source, file_id)?;
    let mut analyzer = semantic::SemanticAnalyzer::new();
    analyzer.analyze(&program)?;
    let mut interpreter = interpreter::Interpreter::new();
    interpreter.run(&program)
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
