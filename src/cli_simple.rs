//! CLI parser for Vredrs — no external dependencies.

use std::env;

#[derive(Debug, Clone)]
pub enum Commands {
    Run { file: String },
    Vm { file: String },
    Build { file: String, output: String, backend: String },
    Test { path: String },
    Mod { args: Vec<String> },
    Fmt { args: Vec<String> },
    Lsp,
    Dap,
    Help,
}

pub struct Cli {
    pub command: Commands,
}

impl Cli {
    pub fn parse() -> Self {
        let args: Vec<String> = env::args().collect();
        if args.len() < 2 {
            return Cli { command: Commands::Help };
        }
        match args[1].as_str() {
            "run" => {
                if args.len() < 3 { eprintln!("Usage: vredrs run <file>"); std::process::exit(1); }
                Cli { command: Commands::Run { file: args[2].clone() } }
            }
            "vm" => {
                if args.len() < 3 { eprintln!("Usage: vredrs vm <file>"); std::process::exit(1); }
                Cli { command: Commands::Vm { file: args[2].clone() } }
            }
            "build" => {
                if args.len() < 3 { eprintln!("Usage: vredrs build <file> [options]"); std::process::exit(1); }
                let file = args[2].clone();
                let mut output = "a.out".to_string();
                let mut backend = "native".to_string();
                let mut i = 3;
                while i < args.len() {
                    match args[i].as_str() {
                        "--raw" => { backend = "raw".to_string(); i += 1; }
                        "-o" | "--output" => { if i+1 < args.len() { output = args[i+1].clone(); i += 2; } else { eprintln!("--output requires a value"); std::process::exit(1); } }
                        "-b" | "--backend" => { if i+1 < args.len() { backend = args[i+1].clone(); i += 2; } else { eprintln!("--backend requires a value"); std::process::exit(1); } }
                        _ => { eprintln!("Unknown option: {}", args[i]); std::process::exit(2); }
                    }
                }
                Cli { command: Commands::Build { file, output, backend } }
            }
            "test" => {
                if args.len() < 3 { eprintln!("Usage: vredrs test <path>"); std::process::exit(1); }
                Cli { command: Commands::Test { path: args[2].clone() } }
            }
            "mod" => {
                let mod_args: Vec<String> = args[2..].to_vec();
                Cli { command: Commands::Mod { args: mod_args } }
            }
            "fmt" => {
                let fmt_args: Vec<String> = args[2..].to_vec();
                Cli { command: Commands::Fmt { args: fmt_args } }
            }
            "lsp" => Cli { command: Commands::Lsp },
            "dap" => Cli { command: Commands::Dap },
            "help" | "--help" | "-h" => Cli { command: Commands::Help },
            _ => { eprintln!("Unknown command: {}", args[1]); eprintln!("Try 'vredrs help'"); std::process::exit(1); }
        }
    }

    pub fn print_help() {
        println!("vredrs 1.0 — The Vredrs Language Compiler\n");
        println!("USAGE:");
        println!("    vredrs <COMMAND> [OPTIONS]\n");
        println!("COMMANDS:");
        println!("    run <file>              Execute a .veds file (interpreter)");
        println!("    vm  <file>              Execute a .veds file (bytecode VM)");
        println!("    build <file> [opts]     Compile to native executable");
        println!("    test <path>             Run tests");
        println!("    mod <init|add|rm|list|tree|update>  Package manager");
        println!("    fmt [opts] <file>       Format source code");
        println!("    lsp                     Language server (stdio)");
        println!("    dap                     Debug adapter (stdio)\n");
        println!("BUILD OPTIONS:");
        println!("    -o, --output <file>     Output path");
        println!("    -b, --backend <name>    native | llvm-ir | raw\n");
        println!("FMT OPTIONS:");
        println!("    --write, -w             Overwrite file");
        println!("    --check, -c             Check only (exit 1 if needs formatting)\n");
        println!("EXAMPLES:");
        println!("    vredrs run main.veds");
        println!("    vredrs build main.veds -o app");
        println!("    vredrs mod init && vredrs mod add utils");
        println!("    vredrs fmt --write main.veds");
    }
}
