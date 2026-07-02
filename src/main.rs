#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(unused_variables)]

mod cli_simple;

use cli_simple::{Cli, Commands};
use vredrs_compiler::{compile_file, run_interpreter};

fn main() {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Run { file } => {
            eprintln!("[vredrs] executing (VM): {}", file);
            // VM is the only execution engine. No AST interpreter fallback.
            if let Err(e) = vredrs_compiler::run_bytecode_vm(file) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Vm { file } => {
            eprintln!("[vredrs] bytecode VM: {}", file);
            if let Err(e) = vredrs_compiler::run_bytecode_vm(file) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Build {
            file,
            output,
            backend,
        } => {
            eprintln!("[vredrs] building: {} (backend: {})", file, backend);
            let output_type = match backend.as_str() {
                "native" => vredrs_compiler::OutputType::Exe,
                "llvm-ir" => vredrs_compiler::OutputType::LlvmIr,
                "raw" | "cstar-raw" => vredrs_compiler::OutputType::Raw,
                other => {
                    eprintln!("Unsupported backend: {}", other);
                    std::process::exit(2);
                }
            };
            let output = if matches!(
                output_type,
                vredrs_compiler::OutputType::LlvmIr | vredrs_compiler::OutputType::Raw
            ) && !output.ends_with(".ll")
            {
                format!("{}.ll", output)
            } else {
                output.clone()
            };
            if let Err(e) = compile_file(file, &output, output_type) {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Test { path } => {
            eprintln!("[vredrs] test: {}", path);
            println!("[vredrs] test command not fully implemented yet");
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
