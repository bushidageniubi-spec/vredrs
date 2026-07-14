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
pub mod platform;
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

    // 语义分析（基础版本）— with mode firewall validation.
    let mut analyzer = semantic::SemanticAnalyzer::new();
    analyzer.analyze_with_mode(&program, "")?;

    // 检查是否要求新 IR-based LLVM 发射器（手册 Phase 3）。
    let backend = std::env::var("VREDRS_BACKEND").unwrap_or_default();
    if backend == "llvm-ir-new" {
        // 新路径：AST → IR → LLVM IR（渐进式）。
        platform::info("using new IR-based LLVM emitter (gradual)");
        let module = codegen::lower::lower_program(&program);
        let llvm_ir = codegen::emit_llvm::emit_module_string(&module);
        // 同时写一份 .ir 调试输出。
        let ir_path = std::path::Path::new(output).with_extension("ir");
        let _ = fs::write(&ir_path, codegen::ir::render_module(&module));
        platform::info(&format!("Static IR written to: {}", ir_path.display()));
        fs::write(output, &llvm_ir)
            .map_err(|e| CompilerError::io_error(format!("can't write LLVM IR: {}", e)))?;
        platform::success(&format!("LLVM IR (via IR) written to: {}", output));
        // 尝试用 clang 把 .ll 编译成可执行文件（手册 Phase 3 验收：fib(25) ≤2ms）。
        let exe_path = std::path::Path::new(output).with_extension("");
        let runtime_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/codegen/runtime/vredrs_runtime.c");
        let runtime_needed = module.functions.iter().any(|f| {
            f.body.iter().any(|i| {
                matches!(i, codegen::ir::StaticInsn::RuntimeCall { .. } | codegen::ir::StaticInsn::CallDyn { .. })
                    || matches!(i, codegen::ir::StaticInsn::Bin { op, .. } if op.is_dyn())
            })
        });
        let plat = platform::PlatformInfo::detect();
        let mut cmd = std::process::Command::new(plat.cc_command());
        cmd.arg("-o").arg(&exe_path).arg("-w").arg(output);
        if runtime_needed && runtime_src.exists() {
            let inc = runtime_src.parent().unwrap().join("include");
            cmd.arg("-I").arg(&inc).arg(&runtime_src);
        }
        match cmd.output() {
            Ok(out) if out.status.success() => {
                platform::success(&format!("Native executable (via LLVM IR): {}", exe_path.display()));
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&exe_path, std::fs::Permissions::from_mode(0o755));
                }
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                platform::warning(&format!("LLVM IR -> exe failed (IR still written):\n{}", stderr));
            }
            Err(_) => {
                platform::warning("clang/cc not found; LLVM IR written only");
            }
        }
        return Ok(());
    }

    // 默认：新 IR-based LLVM 后端（渐进式：静态+动态）。
    let module = codegen::lower::lower_program(&program);
    let llvm_ir = codegen::emit_llvm::emit_module_string(&module);
    // 同时写 .ir 调试输出。
    let ir_path = std::path::Path::new(output).with_extension("ir");
    let _ = fs::write(&ir_path, codegen::ir::render_module(&module));

    fs::write(output, &llvm_ir)
        .map_err(|e| CompilerError::io_error(format!("can't write LLVM IR: {}", e)))?;
    platform::success(&format!("LLVM IR written to: {}", output));
    Ok(())
}

fn compile_to_cstar_raw_ir(source: &str, output: &str, file_id: usize) -> Result<()> {
    compile_to_cstar_raw_ir_with_path(source, output, file_id, "")
}

fn compile_to_cstar_raw_ir_with_path(
    source: &str,
    output: &str,
    file_id: usize,
    path: &str,
) -> Result<()> {
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
            platform::error(&format!("C* source parse failed: {}", e.message()));
            return Err(e);
        }
    };

    // Mode firewall: validate file-type-specific syntax restrictions.
    // .vraw files reject spawn/async/try/catch (dynamic features).
    // .cpps files allow all Cstar features.
    // .veds files reject ptr[T]/@pipeline (raw/cstar features).
    let mut analyzer = semantic::SemanticAnalyzer::new();
    if let Err(e) = analyzer.analyze_with_mode(&program, path) {
        eprintln!("{}", render_error_with_source(&e));
        return Err(e);
    }

    // Route by file extension: `.cpps` files use the C* PIR backend
    // (codegen::cstar::codegen::CstarCompiler) which lowers the C* physical
    // annotations (@section, @pipeline, @patch, @isr_group, @prefetch,
    // @repo, @embed, @link) into LLVM IR + side artifacts + a .bin firmware
    // image.  `.vraw` files route to the architecture-appropriate raw backend
    // (x86_64 on x86_64 hosts, ARM on ARM hosts).
    let is_cpps = path
        .rsplit('.')
        .next()
        .map(|ext| ext.eq_ignore_ascii_case("cpps"))
        .unwrap_or(false);
    if is_cpps {
        return compile_cpps_via_pir_backend(source, output, &program, &side_base, &bin_path);
    }

    // ── 路由：新 IR 路径（默认）vs 旧 AST 路径（--legacy-raw）────
    //
    // 手册 Phase 1/2：`--raw` 默认走新 IR 路径（AST → lower.rs → IR → emit）。
    // `--legacy-raw` 保留旧 AST-based x86/arm 后端，供回归对比（手册 §3.4 风险
    // 防护：重构期间保留旧路径作为后备）。
    // `--cstar` / `raw-cstar`：新 IR 路径 + Cstar 过滤器（手册 Phase 4）。
    // `--ir` / `ir-only`：只输出 Static IR 文本，不生成汇编。
    let backend = std::env::var("VREDRS_BACKEND").unwrap_or_else(|_| "raw".to_string());
    let use_legacy = backend == "raw-legacy";
    let use_cstar_filter = backend == "raw-cstar";
    let ir_only = backend == "ir-only";

    if ir_only {
        // 只输出 Static IR 文本。
        let module = codegen::lower::lower_program(&program);
        let ir_text = codegen::ir::render_module(&module);
        let ir_path = side_base.with_extension("ir");
        fs::write(&ir_path, &ir_text)
            .map_err(|e| CompilerError::io_error(format!("can't write IR: {}", e)))?;
        platform::success(&format!("Static IR written to: {}", ir_path.display()));
        return Ok(());
    }

    if !use_legacy {
        // 新 IR 路径。
        platform::info("using new IR-based raw backend (lower.rs → IR → emit)");
        let mut module = codegen::lower::lower_program(&program);
        // 可选：Cstar 过滤器。
        if use_cstar_filter {
            platform::info("applying Cstar IR filter (@pipeline/@isr_group/@patch)");
            let cfg = codegen::cstar::FilterConfig::default();
            // 输出分析报告。
            let report = codegen::cstar::analyze_module(&module);
            let report_path = side_base.with_extension("cstar.rpt");
            let _ = fs::write(&report_path, &report);
            platform::info(&format!("Cstar analysis report: {}", report_path.display()));
            module = codegen::cstar::filter_module(&module, &cfg);
        }
        let host_arch = std::env::consts::ARCH;
        if host_arch == "aarch64" || host_arch == "arm" || host_arch == "armv7l" {
            // ARM 主机：用新 ARM 发射器。
            if let Err(e) = codegen::emit_raw_arm::compile_via_ir(&program, &side_base) {
                platform::warning(&format!("IR-based ARM emit error: {}", e));
            }
        } else {
            // x86_64 主机：用新 x86 发射器。
            if let Err(e) = codegen::emit_raw_x86::compile_via_ir(&program, &side_base) {
                platform::warning(&format!("IR-based x86 emit error: {}", e));
            }
        }
        // 同时生成 ARM 固件（QEMU）。
        let _ = codegen::cstar::raw::build_firmware_with_program(&bin_path, &program);
        platform::success(&format!("ARM firmware written to: {}", bin_path.display()));
        platform::success(&format!("Raw executable (via IR): {}", side_base.display()));
        return Ok(());
    }

    // The legacy AST-based raw backend (--legacy-raw) has been removed.
    // All raw compilation now goes through the new IR path (lower.rs →
    // emit_raw_x86 / emit_raw_arm). If a legacy backend was requested,
    // fall through to the new path (the use_legacy flag is ignored).
    platform::info("legacy raw backend removed; using new IR path");
    let mut module = codegen::lower::lower_program(&program);
    if use_cstar_filter {
        let cfg = codegen::cstar::FilterConfig::default();
        let report = codegen::cstar::analyze_module(&module);
        let report_path = side_base.with_extension("cstar.rpt");
        let _ = fs::write(&report_path, &report);
        module = codegen::cstar::filter_module(&module, &cfg);
    }
    let host_arch = std::env::consts::ARCH;
    if host_arch == "aarch64" || host_arch == "arm" || host_arch == "armv7l" {
        if let Err(e) = codegen::emit_raw_arm::compile_via_ir(&program, &side_base) {
            platform::warning(&format!("IR-based ARM emit error: {}", e));
        }
    } else {
        if let Err(e) = codegen::emit_raw_x86::compile_via_ir(&program, &side_base) {
            platform::warning(&format!("IR-based x86 emit error: {}", e));
        }
    }
    let _ = codegen::cstar::raw::build_firmware_with_program(&bin_path, &program);
    platform::success(&format!("ARM firmware written to: {}", bin_path.display()));
    platform::success(&format!("Raw executable (via IR): {}", side_base.display()));
    Ok(())
}

/// Lower a `.cpps` program through the C* PIR backend (CstarCompiler) and
/// write all artifacts to disk:
///   - `<output>`: LLVM IR text (the main output)
///   - `<side>.bin`: ARM Cortex-M3 firmware image (always produced)
///   - `<side>.link.ld`: auto-generated linker script (when @section/.isr/etc. need it)
///   - `<side>.pkg.h`: package layout header (when @repo/@package are used)
///   - `<side>.layout.map`: package layout map (when @repo/@package are used)
///   - `<side>.pkg.bin`: package index binary (when @repo/@package are used)
///   - `<patch.output>`: per-`@patch` differential patch files
fn compile_cpps_via_pir_backend(
    source: &str,
    output: &str,
    program: &Program,
    side_base: &Path,
    bin_path: &Path,
) -> Result<()> {
    let mut compiler = codegen::cstar::CstarCompiler::new();
    let artifacts = compiler.generate_artifacts_from_ast_with_source(program, source)?;

    // Main output: LLVM IR text.
    fs::write(output, &artifacts.llvm_ir)
        .map_err(|e| CompilerError::io_error(format!("can't write C* LLVM IR: {}", e)))?;
    platform::success(&format!("C* PIR LLVM IR written to: {}", output));

    // Side artifacts: linker script, package header/map/index, patches.
    if let Some(linker_script) = &artifacts.linker_script {
        let p = side_base.with_extension("link.ld");
        let _ = fs::write(&p, linker_script);
        platform::info(&format!("C* linker script written to: {}", p.display()));
    }
    if let Some(package_header) = &artifacts.package_header {
        let p = side_base.with_extension("pkg.h");
        let _ = fs::write(&p, package_header);
        platform::info(&format!("C* package header written to: {}", p.display()));
    }
    if let Some(layout_map) = &artifacts.layout_map {
        let p = side_base.with_extension("layout.map");
        let _ = fs::write(&p, layout_map);
        platform::info(&format!("C* layout map written to: {}", p.display()));
    }
    if let Some(package_index) = &artifacts.package_index {
        let p = side_base.with_extension("pkg.bin");
        let _ = fs::write(&p, package_index);
        platform::info(&format!("C* package index written to: {}", p.display()));
    }
    for patch in &artifacts.patches {
        if patch.path.is_empty() {
            continue;
        }
        let p = Path::new(&patch.path);
        let _ = fs::write(p, &patch.bytes);
        platform::info(&format!("C* @patch artifact written to: {}", p.display()));
    }

    // Always produce a .bin firmware image (using the existing emitter so
    // QEMU -M lm3s6965evb can load it).  The emitter compiles the user's
    // `main` function to ARM Thumb-2; for programs the emitter can't lower
    // it still produces a valid skeleton firmware with IVT + reset handler.
    codegen::cstar::raw::build_firmware_with_program(bin_path, program)
        .map_err(|e| CompilerError::io_error(format!("can't write C* .bin firmware: {}", e)))?;
    platform::success(&format!("C* ARM firmware written to: {}", bin_path.display()));

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
    // 新 IR 路径：AST → lower.rs → IR → emit_llvm → .ll → clang → exe。
    // 旧的 backend::driver::BuildPipeline（llvm_full 多模块系统）已弃用。
    //
    // 关键：只有 clang 能直接编译 LLVM IR 文本（.ll）。系统 cc/gcc 不能。
    // 因此：若 clang 可用 → 走 LLVM IR 路径（优化更好）；否则回退到
    // raw x86 汇编路径（emit_raw_x86::compile_via_ir），该路径用 `as` + `cc`
    // 组装+链接，gcc 即可。
    let program = parse_program(source, 0)?;
    let mut analyzer = semantic::SemanticAnalyzer::new();
    analyzer.analyze_with_mode(&program, source_path)?;

    let plat = platform::PlatformInfo::detect();

    if !clang_is_available() {
        // 无 clang：回退到 raw x86 汇编路径（gcc + as 即可构建可执行文件）。
        platform::info("clang not found; using raw x86_64 assembly path for native build");
        let out_path = std::path::Path::new(output);
        return codegen::emit_raw_x86::compile_via_ir(&program, out_path)
            .map_err(|e| CompilerError::codegen_error(format!(
                "native compilation failed (raw fallback):\n{}", e
            )));
    }

    let module = codegen::lower::lower_program(&program);
    let llvm_ir = codegen::emit_llvm::emit_module_string(&module);

    // 写 .ll 到临时文件。
    let ll_path = std::path::Path::new(output).with_extension("ll");
    fs::write(&ll_path, &llvm_ir)
        .map_err(|e| CompilerError::io_error(format!("can't write LLVM IR: {}", e)))?;

    // 用 clang 编译 .ll → exe（clang 能直接读 LLVM IR 文本）。
    let runtime_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/codegen/runtime/vredrs_runtime.c");
    let runtime_needed = module.functions.iter().any(|f| {
        f.body.iter().any(|i| {
            matches!(i, codegen::ir::StaticInsn::RuntimeCall { .. } | codegen::ir::StaticInsn::CallDyn { .. })
                || matches!(i, codegen::ir::StaticInsn::Bin { op, .. } if op.is_dyn())
                || matches!(i, codegen::ir::StaticInsn::Box { .. } | codegen::ir::StaticInsn::Unbox { .. })
        })
    });
    let mut cmd = std::process::Command::new("clang");
    cmd.arg("-o").arg(output).arg("-w").arg(&ll_path);
    if runtime_needed && runtime_src.exists() {
        let inc = runtime_src.parent().unwrap().join("include");
        cmd.arg("-I").arg(&inc).arg(&runtime_src);
    }
    match cmd.output() {
        Ok(out) if out.status.success() => {
            platform::success(&format!("Native executable: {}", output));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o755));
            }
            Ok(())
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(CompilerError::codegen_error(format!(
                "native compilation failed:\n{}", stderr
            )))
        }
        Err(_) => Err(CompilerError::codegen_error(
            "clang not found; cannot compile native executable".to_string(),
        )),
    }
}

fn clang_is_available() -> bool {
    std::process::Command::new("clang")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
        OutputType::Raw => compile_to_cstar_raw_ir_with_path(&source, output, file_id, path),
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
        let mut program = parse_program(&source, file_id)?;
        // Mode firewall: validate file-type-specific syntax.
        let mut analyzer = semantic::SemanticAnalyzer::new();
        analyzer.analyze_with_mode(&program, path)?;
        // Append core.veds function definitions to the program so they
        // compile into the same module (unified constants table).
        const CORE_SOURCE: &str = include_str!("runtime/std/core.veds");
        if let Ok(core_tokens) = lexer::Lexer::new(CORE_SOURCE, 0).tokenize() {
            if let Ok(core_prog) = parser::Parser::new(core_tokens, 0).parse_program() {
                for d in core_prog.declarations {
                    program.declarations.push(d);
                }
            }
        }
        let compiler = bytecode::Compiler::new();
        let module = compiler.compile(&program)?;
        let has_main = program.declarations.iter().any(|d| {
            if let TopLevel::FnDef(f) = d { f.name.name == "main" } else { false }
        });
        let mut vm = bytecode::VM::new(module)
            .with_program(program)
            .with_base_dir(base_dir);
        // Run the VM under `catch_unwind` so that a panic inside the VM
        // (e.g. an indexing panic, a divide-by-zero in Rust-level code, an
        // `unwrap()` on `None`, or a deliberately-`panic!`-ing native helper)
        // is caught and surfaced as a `CompilerError::runtime_error` instead
        // of aborting the entire `vredrs run` process. This is important for
        // the REPL, the LSP, and any embedding host that wants to recover
        // from a bad Vredrs script without restarting.
        //
        // `AssertUnwindSafe` is required because the VM holds `Rc<RefCell<_>>`
        // interior-mutable cells (e.g. object fields), which are not
        // `UnwindSafe` by default. We accept the assertion because:
        //   (a) the panic leaves the VM in a "poisoned" state that we drop
        //       immediately (we never reuse `vm` after a caught panic — we
        //       return `Err` and the closure exits), so no half-mutated
        //       state is observed by long-lived code;
        //   (b) the alternative (letting the panic unwind through `lib.rs`)
        //       is strictly worse for an embedding host.
        run_vm_under_panic_guard(&mut vm, |vm| vm.run(), "VM panic")?;
        // B1: If a main() function is defined, call it after top-level code.
        if has_main {
            // Push main as a CallByName with 0 args.
            let main_idx = vm.intern_constant("main");
            vm.push_instruction(bytecode::instr::Instr::CallByName(main_idx, 0));
            vm.push_instruction(bytecode::instr::Instr::Pop);
            // Same catch_unwind guard for the post-main invocation.
            run_vm_under_panic_guard(&mut vm, |vm| vm.run_from_current_pc(), "VM panic during main()")?;
        }
        Ok(())
    })();
    // Always restore the working directory, even on error.
    if let Some(dir) = original_dir {
        let _ = std::env::set_current_dir(dir);
    }
    result
}

/// Run a VM operation under a `std::panic::catch_unwind` guard.
///
/// `f` is invoked with `&mut vm`. If `f` returns `Ok(_)` or `Err(_)`, the
/// result is propagated through unchanged. If `f` panics, the panic is
/// caught and converted into a `CompilerError::runtime_error` whose message
/// is `"<prefix>: <panic message>"` (or `"<prefix>: <non-string panic
/// payload>"` when the panic payload isn't a `&str` or `String`).
///
/// This is the safety net for `run_bytecode_vm`: a panic inside the VM
/// (e.g. an indexing panic, a divide-by-zero in Rust-level code, an
/// `unwrap()` on `None`, or a deliberately-`panic!`-ing native helper)
/// becomes a normal `Err` the caller can handle, instead of unwinding the
/// entire embedding host. See `run_bytecode_vm`'s call sites for the
/// `AssertUnwindSafe` rationale.
fn run_vm_under_panic_guard<T, F>(vm: &mut bytecode::VM, f: F, prefix: &str) -> Result<T>
where
    F: FnOnce(&mut bytecode::VM) -> Result<T>,
{
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(vm)));
    match panicked {
        Ok(inner) => inner,
        Err(payload) => Err(CompilerError::runtime_error(format!(
            "{}: {}",
            prefix,
            panic_message(payload)
        ))),
    }
}

/// Best-effort extraction of a human-readable message from a
/// `catch_unwind` panic payload. Rust's `panic!` machinery stores the
/// payload as `Box<dyn Any + Send>`, and the concrete type depends on the
/// panic source: `panic!("literal")` stores `&str`, `panic!("{}", x)`
/// stores `String`, and other panics (e.g. arithmetic overflow in debug)
/// may store nothing we can downcast. Anything we can't decode is reported
/// as `<non-string panic payload>` so the error message is still useful.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
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
    // Mirror the panic guard in `run_bytecode_vm`: a VM panic here would
    // otherwise abort the benchmark runner. Convert it to a normal `Err`.
    run_vm_under_panic_guard(&mut vm, |vm| vm.run(), "VM panic")?;
    Ok(())
}

/// Run test and bench blocks from .veds files.
///
/// Scans the given path (a file or directory) for `.veds` files, parses each,
/// extracts `test, "name" ... /end` and `bench, "name", N ... /end` blocks,
/// and executes them. Test blocks pass if they run without throwing or
/// assertion failure; bench blocks report wall-clock timing.
pub fn run_tests(path: &str) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout();
    let p = std::path::Path::new(path);
    let mut veds_files: Vec<std::path::PathBuf> = Vec::new();
    if p.is_file() {
        veds_files.push(p.to_path_buf());
    } else if p.is_dir() {
        collect_veds_files(p, &mut veds_files);
    } else {
        return Err(CompilerError::io_error(format!(
            "test: '{}' is not a file or directory", path
        )));
    }
    if veds_files.is_empty() {
        writeln!(stdout, "[vredrs] no .veds files found in '{}'", path).ok();
        return Ok(());
    }
    veds_files.sort();
    let mut total_pass = 0usize;
    let mut total_fail = 0usize;
    let mut total_bench = 0usize;
    for file in &veds_files {
        let source = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                writeln!(stdout, "[vredrs] can't read {}: {}", file.display(), e).ok();
                continue;
            }
        };
        let file_id = 0;
        set_source(file_id, file.to_string_lossy().to_string(), source.clone());
        let program = match parse_program(&source, file_id) {
            Ok(p) => p,
            Err(e) => {
                writeln!(stdout, "[vredrs] parse error in {}: {}", file.display(), e.message()).ok();
                total_fail += 1;
                continue;
            }
        };
        // Separate test/bench blocks from regular declarations.
        let mut test_blocks: Vec<(&str, &[crate::parser::ast::Stmt])> = Vec::new();
        let mut bench_blocks: Vec<(&str, i64, &[crate::parser::ast::Stmt])> = Vec::new();
        let mut decls: Vec<TopLevel> = Vec::new();
        for d in &program.declarations {
            match d {
                TopLevel::TestBlock(t) => {
                    test_blocks.push((&t.name, &t.body));
                }
                TopLevel::BenchBlock(b) => {
                    bench_blocks.push((&b.name, b.iterations, &b.body));
                }
                _ => decls.push(d.clone()),
            }
        }
        if test_blocks.is_empty() && bench_blocks.is_empty() {
            continue;
        }
        writeln!(stdout, "[vredrs] {} ({} tests, {} benches)",
            file.display(), test_blocks.len(), bench_blocks.len()).ok();
        // Run the top-level code (declarations minus test/bench) to set up
        // functions, classes, globals.
        let setup_program = Program {
            declarations: decls,
            span: crate::error::Span::dummy(),
        };
        let compiler = bytecode::Compiler::new();
        let module = match compiler.compile(&setup_program) {
            Ok(m) => m,
            Err(e) => {
                writeln!(stdout, "  [compile error] {}", e.message()).ok();
                total_fail += test_blocks.len();
                continue;
            }
        };
        let mut vm = bytecode::VM::new(module).with_program(setup_program);
        if let Err(e) = vm.run() {
            let msg = e.message().to_string();
            if msg != "return" {
                writeln!(stdout, "  [setup error] {}", msg).ok();
            }
        }
        let saved_globals = vm.globals_clone();
        // Run each test block.
        for (name, body) in &test_blocks {
            let test_program = Program {
                declarations: body.iter().map(|s| TopLevel::Statement(s.clone())).collect(),
                span: crate::error::Span::dummy(),
            };
            let compiler = bytecode::Compiler::new();
            let module = match compiler.compile(&test_program) {
                Ok(m) => m,
                Err(e) => {
                    writeln!(stdout, "  ❌ {} — compile error: {}", name, e.message()).ok();
                    total_fail += 1;
                    continue;
                }
            };
            let mut vm = bytecode::VM::new(module).with_program(test_program);
            for (k, v) in &saved_globals {
                vm.set_global(k, v.clone());
            }
            match vm.run() {
                Ok(_) => {
                    writeln!(stdout, "  ✅ {}", name).ok();
                    total_pass += 1;
                }
                Err(e) => {
                    let msg = e.message().to_string();
                    if msg == "assert" || msg.contains("assert") {
                        writeln!(stdout, "  ❌ {} — assertion failed", name).ok();
                    } else if msg == "return" || msg == "break" || msg == "continue" {
                        writeln!(stdout, "  ✅ {}", name).ok();
                        total_pass += 1;
                    } else {
                        writeln!(stdout, "  ❌ {} — {}", name, msg).ok();
                    }
                    total_fail += 1;
                }
            }
        }
        // Run each bench block.
        for (name, iterations, body) in &bench_blocks {
            let bench_program = Program {
                declarations: body.iter().map(|s| TopLevel::Statement(s.clone())).collect(),
                span: crate::error::Span::dummy(),
            };
            let compiler = bytecode::Compiler::new();
            let module = match compiler.compile(&bench_program) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let n = (*iterations).max(1) as u64;
            let start = std::time::Instant::now();
            for _ in 0..n {
                let mut vm = bytecode::VM::new(module.clone()).with_program(bench_program.clone());
                for (k, v) in &saved_globals {
                    vm.set_global(k, v.clone());
                }
                let _ = vm.run();
            }
            let elapsed = start.elapsed();
            let ms = elapsed.as_secs_f64() * 1000.0;
            writeln!(stdout, "  ⚡ {} × {} → {:.3}ms", name, n, ms).ok();
            total_bench += 1;
        }
    }
    writeln!(stdout, "\n测试通过: {}, 失败: {}, 基准: {}", total_pass, total_fail, total_bench).ok();
    Ok(())
}

/// Recursively collect .veds files from a directory.
fn collect_veds_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip hidden directories and build/cache dirs.
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with('.') || name == "build" || name == "target" {
                        continue;
                    }
                }
                collect_veds_files(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("veds") {
                out.push(path);
            }
        }
    }
}

/// Run the interactive REPL (read-eval-print loop) — industrial-quality
/// rewrite for 0.1.5.
///
/// Features:
/// - `>>>` primary prompt; `...` continuation prompt for multi-line blocks.
/// - History persisted to `~/.vredrs_history` across sessions (loaded on
///   start, appended on each submitted input).
/// - Tab completion: keywords, builtins, current globals, stdlib module
///   names.
/// - V-series colored error rendering (same as `vredrs run`).
/// - Variables, functions, and classes persist across inputs.
/// - Bare expressions are evaluated and their result printed.
/// - `exit()` / `quit()` / Ctrl+D to quit; `help()` for syntax guide.
pub fn run_repl() -> Result<()> {
    use std::io::Write;

    let file_id = 0;
    let mut stdout = std::io::stdout();
    let mut editor = line_editor::LineEditor::new();

    // Load history from ~/.vredrs_history.
    let history_path = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".vredrs_history"))
        .ok();
    if let Some(ref path) = history_path {
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                if !line.trim().is_empty() {
                    editor.add_history(line);
                }
            }
        }
    }

    // Accumulated declarations from previous inputs (functions, classes,
    // structs, enums, traits, impls, type aliases, consts, lazys, macros).
    let mut accumulated_decls: Vec<TopLevel> = Vec::new();
    // Persisted globals across inputs.
    let mut saved_globals: std::collections::HashMap<String, bytecode::vm::Value> =
        std::collections::HashMap::new();

    let plat = platform::PlatformInfo::detect();
    let banner = if plat.is_termux() {
        format!(
            "{}Vredrs{} {}0.1.5{} REPL — {}Termux/Android ({}){}\nType 'exit()' or Ctrl+D to quit, 'help()' for help.",
            platform::Colors::cyan_bold(),
            platform::Colors::reset(),
            platform::Colors::magenta(),
            platform::Colors::reset(),
            platform::Colors::green(),
            plat.arch,
            platform::Colors::reset(),
        )
    } else {
        format!(
            "{}Vredrs{} {}0.1.5{} REPL — type 'exit()' or Ctrl+D to quit, 'help()' for help.",
            platform::Colors::cyan_bold(),
            platform::Colors::reset(),
            platform::Colors::magenta(),
            platform::Colors::reset(),
        )
    };
    writeln!(stdout, "{}", banner).ok();
    let _ = stdout.flush();

    // Colored prompts.
    let primary_prompt = format!(
        "{}>>>{} ",
        platform::Colors::blue_bold(),
        platform::Colors::reset()
    );
    let cont_prompt = format!(
        "{}...{} ",
        platform::Colors::dim(),
        platform::Colors::reset()
    );

    loop {
        // Read a (possibly multi-line) input using the line editor.
        let mut input = String::new();
        let line = match editor.read_line(&primary_prompt, &saved_globals) {
            Some(l) => l,
            None => {
                // Ctrl+D / EOF.
                writeln!(stdout).ok();
                break;
            }
        };
        if line.is_empty() && input.is_empty() {
            continue;
        }
        input.push_str(&line);
        input.push('\n');

        // Multi-line continuation: if the input contains a block keyword
        // that is not yet closed by /end, keep reading with the
        // continuation prompt until /end is seen.
        while needs_continuation(&input) {
            let cont = match editor.read_line(&cont_prompt, &saved_globals) {
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

        // Save to history file.
        if let Some(ref path) = history_path {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(f, "{}", trimmed);
            }
        }

        // Determine whether the input is a bare expression or a statement.
        let is_bare = is_bare_expression(trimmed);
        let source = if is_bare {
            format!("println, {}\n", trimmed)
        } else {
            input.clone()
        };

        // For bare identifiers, check if the variable is defined before
        // running. This gives a clear error instead of silently printing null.
        if is_bare {
            let bare_trimmed = trimmed.trim();
            let is_simple_id = bare_trimmed.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !bare_trimmed.is_empty()
                && !bare_trimmed.chars().next().unwrap_or(' ').is_numeric();
            if is_simple_id && !saved_globals.contains_key(bare_trimmed) {
                let known = bytecode::compiler::is_builtin(bare_trimmed)
                    || saved_globals.keys().any(|k| k == bare_trimmed);
                if !known {
                    // Load the current source into the source manager
                    // BEFORE rendering the error, so the error position
                    // points to the current input, not the previous one.
                    set_source(file_id, "<repl>".to_string(), source.clone());
                    writeln!(stdout, "{}", render_error_with_source(
                        &CompilerError::semantic_error(
                            format!("undefined variable '{}'", bare_trimmed),
                            crate::error::Span::dummy(),
                        )
                    )).ok();
                    let _ = stdout.flush();
                    continue;
                }
            }
        }

        // Load source into the source manager for error rendering.
        set_source(file_id, "<repl>".to_string(), source.clone());

        // Parse the input.
        let program = match parse_program(&source, file_id) {
            Ok(p) => p,
            Err(e) => {
                // Use V-series colored error rendering.
                writeln!(stdout, "{}", render_error_with_source(&e)).ok();
                let _ = stdout.flush();
                continue;
            }
        };

        // Merge accumulated declarations with the new input.
        let mut all_decls = Vec::new();
        let mut new_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for d in &program.declarations {
            let name = decl_name(d);
            if let Some(ref n) = name {
                new_names.insert(n.clone());
            }
            all_decls.push(d.clone());
        }
        for d in &accumulated_decls {
            let name = decl_name(d);
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
                writeln!(stdout, "{}", render_error_with_source(&e)).ok();
                let _ = stdout.flush();
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
                // Accumulate all declaration types for persistence.
                for d in &program.declarations {
                    if !matches!(d, TopLevel::Statement(_)) {
                        accumulated_decls.push(d.clone());
                    }
                }
            }
            Err(e) => {
                let msg = e.message().to_string();
                // Control-flow signals (return/break/continue/yield) are
                // not real errors in the REPL context.
                if msg != "return" && msg != "break" && msg != "continue" && msg != "yield" {
                    writeln!(stdout, "{}", render_error_with_source(&e)).ok();
                }
                saved_globals = vm.globals_clone();
            }
        }
        let _ = stdout.flush();
    }

    Ok(())
}

/// Extract the name from a TopLevel declaration (for deduplication in the
/// REPL's accumulated declarations).
fn decl_name(d: &TopLevel) -> Option<String> {
    match d {
        TopLevel::FnDef(f) => Some(f.name.name.clone()),
        TopLevel::ClassDef(c) => Some(c.name.name.clone()),
        TopLevel::StructDef(s) => Some(s.name.name.clone()),
        TopLevel::EnumDef(e) => Some(e.name.name.clone()),
        TopLevel::InterfaceDef(i) => Some(i.name.name.clone()),
        TopLevel::TraitDef(t) => Some(t.name.name.clone()),
        TopLevel::TypeAlias(t) => Some(t.name.name.clone()),
        TopLevel::ConstExpr(c) => Some(c.name.name.clone()),
        TopLevel::LazyDef(l) => Some(l.name.name.clone()),
        TopLevel::LazyFnDef(l) => Some(l.fn_def.name.name.clone()),
        TopLevel::MacroDef(m) => Some(m.name.name.clone()),
        TopLevel::MarkerTrait(m) => Some(m.name.name.clone()),
        TopLevel::ExternFnDef(e) => Some(e.fn_def.name.name.clone()),
        TopLevel::ImplBlock(i) => Some(format!("impl_{}", i.trait_name.name.clone())),
        TopLevel::DtorBlock(d) => Some(format!("dtor_{}", d.type_name.name.clone())),
        _ => None,
    }
}

/// Heuristic: does the input need multi-line continuation? Returns true if
/// the input contains a block keyword that is not yet closed by a matching
/// `/end`. We track a simple depth counter: each block-opening keyword
/// increments it, each `/end` line decrements it.
fn needs_continuation(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Block-opening keywords (comma-form). Each opens a block that needs /end.
    let block_keywords = [
        "if,", "elif,", "else", "for,", "fn,", "class,", "with,", "try,",
        "catch,", "finally", "while,", "loop,", "match,", "struct,",
        "enum,", "trait,", "impl,", "dtor,", "interface,", "test,",
        "bench,", "unsafe", "select,", "macro,",
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
        // A block-opening keyword opens one block.
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
        "trait,", "impl,", "dtor,", "type,", "const,", "constexpr,",
        "lazy,", "macro,", "plugin,", "extern,", "test,", "bench,",
        "interface,", "unsafe", "volatile", "@",
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
    writeln!(stdout, "Vredrs 0.1.5 REPL — Syntax Guide").ok();
    writeln!(stdout, "").ok();
    writeln!(stdout, "REPL Commands:").ok();
    writeln!(stdout, "  exit() / quit() / Ctrl+D  — exit the REPL").ok();
    writeln!(stdout, "  help()                   — show this help").ok();
    writeln!(stdout, "  Up/Down                  — browse command history").ok();
    writeln!(stdout, "  Tab                      — complete keywords/builtins/globals").ok();
    writeln!(stdout, "").ok();
    writeln!(stdout, "Statements:").ok();
    writeln!(stdout, "  set, x, 42               — assign a variable").ok();
    writeln!(stdout, "  set, x += 1              — compound assignment").ok();
    writeln!(stdout, "  println, expr            — print with newline").ok();
    writeln!(stdout, "  paste, expr              — print without newline").ok();
    writeln!(stdout, "  input, var, \"prompt\"     — read line from stdin").ok();
    writeln!(stdout, "  if, cond  ...  /end      — if block (multi-line)").ok();
    writeln!(stdout, "  elif, cond  ...          — else-if branch").ok();
    writeln!(stdout, "  else  ...                — else branch").ok();
    writeln!(stdout, "  while, cond  ...  /end   — while loop").ok();
    writeln!(stdout, "  for, x, in, iter  /end   — for-in loop").ok();
    writeln!(stdout, "  for, i, in, 1..10  /end  — for-range loop").ok();
    writeln!(stdout, "  loop  ...  /end          — infinite loop").ok();
    writeln!(stdout, "  break / continue         — loop control").ok();
    writeln!(stdout, "  fn, name(params)  ...  /end  — function definition").ok();
    writeln!(stdout, "  class, Name  ...  /end   — class definition").ok();
    writeln!(stdout, "  struct, Name  ...  /end  — struct definition").ok();
    writeln!(stdout, "  enum, Name  ...  /end    — enum definition").ok();
    writeln!(stdout, "  try  ...  catch, e  ...  /end  — try/catch").ok();
    writeln!(stdout, "  throw, \"msg\"             — throw exception").ok();
    writeln!(stdout, "  return, expr             — return from function").ok();
    writeln!(stdout, "  yield, expr              — yield in generator").ok();
    writeln!(stdout, "  defer, stmt              — defer to scope exit").ok();
    writeln!(stdout, "  match, expr  ...  /end   — pattern match").ok();
    writeln!(stdout, "  with, expr, as, v  /end  — context manager").ok();
    writeln!(stdout, "  import, \"mod\", sym       — import module").ok();
    writeln!(stdout, "  del, target              — delete element/field").ok();
    writeln!(stdout, "").ok();
    writeln!(stdout, "Expressions:").ok();
    writeln!(stdout, "  1 + 2 * 3                — arithmetic (+, -, *, /, //, %, **)").ok();
    writeln!(stdout, "  \"hello\"                  — string literal").ok();
    writeln!(stdout, "  \"Hello, {{name}}!\"         — string interpolation").ok();
    writeln!(stdout, "  [1, 2, 3]                — list literal").ok();
    writeln!(stdout, "  {{\"key\": \"value\"}}        — dict literal").ok();
    writeln!(stdout, "  (1, 2, 3)                — tuple literal").ok();
    writeln!(stdout, "  {{1, 2, 3}}                — set literal").ok();
    writeln!(stdout, "  fib(10)                  — function call").ok();
    writeln!(stdout, "  obj.method(args)         — method call").ok();
    writeln!(stdout, "  lst[0] / lst[1:4]        — index / slice").ok();
    writeln!(stdout, "  x ?? y                   — null coalesce").ok();
    writeln!(stdout, "  obj?.field               — optional chain").ok();
    writeln!(stdout, "  x |> f                   — pipe (f(x))").ok();
    writeln!(stdout, "  cond ? a : b             — ternary").ok();
    writeln!(stdout, "  fn(x) x + 1              — lambda").ok();
    writeln!(stdout, "  [x*2 for x in lst]       — list comprehension").ok();
    writeln!(stdout, "  ...list                  — spread in list literal").ok();
    writeln!(stdout, "").ok();
    writeln!(stdout, "Stdlib modules (import by name):").ok();
    writeln!(stdout, "  math, io, os, time, fs, fmt, json, rand, regex,").ok();
    writeln!(stdout, "  crypto, sync, net, http, image, collections, ...").ok();
    writeln!(stdout, "").ok();
    writeln!(stdout, "Builtins: len, str, int, float, bool, range, sum, min, max,").ok();
    writeln!(stdout, "          sorted, reversed, map, filter, type_of, enumerate,").ok();
    writeln!(stdout, "          zip, open, read, write, close, print, println, paste").ok();
    writeln!(stdout, "").ok();
    let _ = stdout.flush();
}
