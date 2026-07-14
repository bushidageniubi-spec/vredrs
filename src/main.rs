#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_variables)]

mod cli_simple;

use cli_simple::{Cli, Commands};
use vredrs_compiler::compile_file;
use vredrs_compiler::platform;

fn main() {
    let cli = Cli::parse();
    let plat = platform::PlatformInfo::detect();

    match &cli.command {
        Commands::Run { file } => {
            platform::info(&format!("executing (VM): {}", file));
            if let Err(e) = vredrs_compiler::run_bytecode_vm(file) {
                eprintln!("{}", vredrs_compiler::render_error_with_source(&e));
                print_termux_hint_if_needed(&plat, &e);
                std::process::exit(1);
            }
        }
        Commands::Vm { file } => {
            platform::info(&format!("bytecode VM: {}", file));
            if let Err(e) = vredrs_compiler::run_bytecode_vm(file) {
                eprintln!("{}", vredrs_compiler::render_error_with_source(&e));
                print_termux_hint_if_needed(&plat, &e);
                std::process::exit(1);
            }
        }
        Commands::Repl => {
            if let Err(e) = vredrs_compiler::run_repl() {
                platform::error(&e.message());
                std::process::exit(1);
            }
        }
        Commands::Build {
            file,
            output,
            backend,
        } => {
            platform::info(&format!("building: {} (backend: {})", file, backend));
            if backend == "raw" || backend == "cstar-raw" {
                platform::print_platform_info(&plat);
                if plat.is_arm_device() && !plat.is_termux() {
                    platform::hint(&format!(
                        "ARM device detected ({}). The raw backend emits {} assembly.",
                        plat.arch,
                        if plat.arch == "aarch64" { "AArch64" } else { "ARM32" }
                    ));
                }
            }
            let output_type = match backend.as_str() {
                "native" => vredrs_compiler::OutputType::Exe,
                "llvm-ir" | "llvm-ir-new" | "llvm-legacy" => vredrs_compiler::OutputType::LlvmIr,
                "raw" | "cstar-raw" | "raw-legacy" | "raw-cstar" | "ir-only" => vredrs_compiler::OutputType::Raw,
                other => {
                    platform::error(&format!("Unsupported backend: {}", other));
                    std::process::exit(2);
                }
            };
            // 把 backend 字符串传给 lib.rs 供路由使用。
            std::env::set_var("VREDRS_BACKEND", &backend);
            let output = if matches!(
                output_type,
                vredrs_compiler::OutputType::LlvmIr
            ) && !output.ends_with(".ll")
            {
                format!("{}.ll", output)
            } else {
                output.clone()
            };
            if let Err(e) = compile_file(file, &output, output_type) {
                eprintln!("{}", vredrs_compiler::render_error_with_source(&e));
                print_termux_hint_if_needed(&plat, &e);
                std::process::exit(1);
            }
        }
        Commands::BuildDir { dir, release, jobs, clean, strip, target } => {
            let opts = vredrs_compiler::driver::build::BuildOptions {
                release: *release,
                jobs: if *jobs > 0 { *jobs } else { 0 },
                clean: *clean,
                strip: *strip,
                target: target.clone(),
                debug: !*strip,
            };
            platform::print_platform_info(&plat);
            match vredrs_compiler::driver::build::build_project(dir, &opts) {
                Ok(result) => {
                    if let Some(exe) = &result.executable {
                        platform::success(&format!("executable: {}", exe.display()));
                    }
                    if let Some(fw) = &result.firmware {
                        platform::success(&format!("firmware: {}", fw.display()));
                    }
                }
                Err(e) => {
                    eprintln!("{}", vredrs_compiler::render_error_with_source(&e));
                    print_termux_hint_if_needed(&plat, &e);
                    std::process::exit(1);
                }
            }
        }
        Commands::Test { path } => {
            platform::info(&format!("test: {}", path));
            if let Err(e) = vredrs_compiler::run_tests(path) {
                eprintln!("{}", vredrs_compiler::render_error_with_source(&e));
                std::process::exit(1);
            }
        }
        Commands::Mod { args } => {
            std::process::exit(vredrs_compiler::vpm::run(args));
        }
        Commands::Fmt { args } => {
            std::process::exit(vredrs_compiler::fmt::run(args));
        }
        Commands::Lsp => {
            std::process::exit(vredrs_compiler::lsp::run());
        }
        Commands::Dap => {
            std::process::exit(vredrs_compiler::dap::run());
        }
        Commands::Help => {
            Cli::print_help();
        }
    }
}

/// Print a Termux-specific hint when a tool is missing.
fn print_termux_hint_if_needed(plat: &platform::PlatformInfo, e: &vredrs_compiler::error::CompilerError) {
    let msg = e.message();
    // Check for common tool-missing patterns.
    if msg.contains("clang") || msg.contains("linker not found") {
        if let Some(hint) = plat.missing_tool_hint("clang") {
            platform::hint(&hint);
        }
    } else if msg.contains("'as'") || msg.contains("assembler") {
        if let Some(hint) = plat.missing_tool_hint("as") {
            platform::hint(&hint);
        }
    } else if msg.contains("'ld'") || msg.contains("linker") {
        if let Some(hint) = plat.missing_tool_hint("ld") {
            platform::hint(&hint);
        }
    }
    // Termux-specific: if the error mentions libm or fmod, hint about -lm.
    if plat.is_termux() && (msg.contains("fmod") || msg.contains("libm") || msg.contains("math")) {
        platform::hint("On Termux, math functions require linking with -lm (already added by default). If the error persists, try: pkg install clang");
    }
}
