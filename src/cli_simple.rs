//! CLI parser for Vredrs — no external dependencies.

use std::env;

#[derive(Debug, Clone)]
pub enum Commands {
    Run { file: String },
    Vm { file: String },
    Repl,
    Build { file: String, output: String, backend: String },
    BuildDir { dir: String, release: bool, jobs: usize, clean: bool, strip: bool, target: Option<String> },
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
            "repl" => Cli { command: Commands::Repl },
            "build" => {
                if args.len() < 3 { eprintln!("Usage: vredrs build <file|dir> [options]"); std::process::exit(1); }
                let file = args[2].clone();
                if std::path::Path::new(&file).is_dir() {
                    let mut release = false;
                    let mut jobs = 0; // 0 = auto
                    let mut clean = false;
                    let mut strip = false;
                    let mut target: Option<String> = None;
                    let mut i = 3;
                    while i < args.len() {
                        match args[i].as_str() {
                            "--release" => { release = true; i += 1; }
                            "--clean" => { clean = true; i += 1; }
                            "--strip" => { strip = true; i += 1; }
                            "--jobs" | "-j" => { if i+1 < args.len() { jobs = args[i+1].parse().unwrap_or(0); i += 2; } else { i += 1; } }
                            "--target" | "-t" => { if i+1 < args.len() { target = Some(args[i+1].clone()); i += 2; } else { i += 1; } }
                            _ => { eprintln!("Unknown option: {}", args[i]); i += 1; }
                        }
                    }
                    Cli { command: Commands::BuildDir { dir: file, release, jobs, clean, strip, target } }
                } else {
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
        println!("vredrs 0.1.4 — The Vredrs Language Compiler\n");
        println!("USAGE:");
        println!("    vredrs <COMMAND> [OPTIONS]\n");
        println!("COMMANDS:");
        println!("    run <file>              Execute a .veds file (bytecode VM)");
        println!("    vm  <file>              Execute a .veds file (bytecode VM, alias)");
        println!("    repl                    Interactive REPL (history, tab, multi-line)");
        println!("    build <file> [opts]     Compile a single file to native executable");
        println!("    build <dir>  [opts]     Multi-mode project build (incremental/parallel)");
        println!("    test <path>             Run test/bench blocks");
        println!("    mod <init|add|rm|list|tree|update>  Package manager");
        println!("    fmt [opts] <file>       Format source code");
        println!("    lsp                     Language server (stdio)");
        println!("    dap                     Debug adapter (stdio)\n");
        println!("SINGLE-FILE BUILD OPTIONS:");
        println!("    -o, --output <file>     Output path");
        println!("    -b, --backend <name>    native | llvm-ir | raw");
        println!("    --raw                   Shortcut for -b raw\n");
        println!("PROJECT BUILD OPTIONS (vredrs build <dir>):");
        println!("    --release               Enable LTO + optimizations");
        println!("    --clean                 Clear cache and build artifacts");
        println!("    --strip                 Strip debug symbols");
        println!("    -j, --jobs <N>          Parallel compile jobs (0 = auto)");
        println!("    -t, --target <TRIPLE>   Cross-compile target\n");
        println!("FMT OPTIONS:");
        println!("    --write, -w             Overwrite file");
        println!("    --check, -c             Check only (exit 1 if needs formatting)\n");
        println!("EXAMPLES:");
        println!("    vredrs run main.veds");
        println!("    vredrs build main.veds -o app");
        println!("    vredrs build raw_prog.vraw -o raw_app --raw");
        println!("    vredrs build . --release --jobs 8");
        println!("    vredrs build . --clean");
        println!("    vredrs repl");
        println!("    vredrs mod init && vredrs mod add utils");
        println!("    vredrs fmt --write main.veds");
    }
}
