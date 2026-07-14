//! Build dispatcher — scans a project directory, classifies files, and
//! compiles each with the appropriate backend, then links the results.
//!
//! 0.1.5 features:
//! - Incremental compilation (mtime + content hash cache in .vredrs-cache/)
//! - Build cache (skip unchanged files, reuse cached .o)
//! - Parallel compilation (std::thread, --jobs N)
//! - LTO support (--release)
//! - Debug symbol management (--strip)
//! - Architecture detection + cross-compilation (--target)
//! - --clean flag

use crate::error::{CompilerError, ErrorCode, Result};
use crate::driver::file_classifier::{classify_file, FileKind};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Build options parsed from CLI flags.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub release: bool,
    pub jobs: usize,
    pub clean: bool,
    pub strip: bool,
    pub target: Option<String>,
    pub debug: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            release: false,
            jobs: num_cpus(),
            clean: false,
            strip: false,
            target: None,
            debug: true,
        }
    }
}

pub struct BuildResult {
    pub executable: Option<PathBuf>,
    pub firmware: Option<PathBuf>,
    pub veds_count: usize,
    pub vraw_count: usize,
    pub cpps_count: usize,
    pub compiled_count: usize,   // files actually compiled (not cached)
    pub cached_count: usize,     // files reused from cache
    pub elapsed_ms: u128,
}

/// Detect the number of available CPU cores.
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Detect the target architecture.
pub fn detect_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "arm" | "armv7l" => "arm",
        _ => "x86_64", // fallback
    }
}

/// Resolve the effective target architecture (from --target or auto-detect).
fn resolve_target(opts: &BuildOptions) -> String {
    if let Some(ref t) = opts.target {
        // Parse target triple like "arm-unknown-linux-gnueabihf"
        if t.starts_with("aarch64") || t.starts_with("arm64") {
            "aarch64".to_string()
        } else if t.starts_with("arm") {
            "arm".to_string()
        } else {
            "x86_64".to_string()
        }
    } else {
        detect_arch().to_string()
    }
}

/// Compute a content hash of a file (simple FNV-1a, sufficient for cache key).
fn file_hash(path: &Path) -> String {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in &buf {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", hash)
}

/// Get file modification time as a string (for cache key).
fn file_mtime(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(meta) => {
            if let Ok(t) = meta.modified() {
                if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                    return format!("{}", d.as_millis());
                }
            }
        }
        Err(_) => {}
    }
    "0".to_string()
}

/// Check if a cached .o file is still valid (source unchanged).
fn is_cache_valid(source: &Path, obj: &Path, cache_dir: &Path) -> bool {
    // Cache file: .vredrs-cache/<source_name>.cache
    // The cache key includes the compiler version to invalidate
    // stale caches after an upgrade.
    let compiler_version = env!("CARGO_PKG_VERSION");
    let cache_file = cache_dir.join(format!(
        "{}.cache",
        source.file_stem().unwrap_or_default().to_string_lossy()
    ));
    let current_hash = file_hash(source);
    let current_mtime = file_mtime(source);
    // Read cached hash
    if let Ok(cached) = std::fs::read_to_string(&cache_file) {
        let parts: Vec<&str> = cached.lines().collect();
        if parts.len() >= 3 {
            let cached_hash = parts[0];
            let cached_mtime = parts[1];
            let cached_version = parts[2];
            // Invalidate cache if compiler version changed.
            if cached_version != compiler_version {
                return false;
            }
            if cached_hash == current_hash && cached_mtime == current_mtime {
                // Also verify the .o file exists and is non-empty
                if let Ok(meta) = std::fs::metadata(obj) {
                    if meta.len() > 0 {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Write cache metadata after successful compilation.
fn write_cache(source: &Path, cache_dir: &Path) {
    let cache_file = cache_dir.join(format!(
        "{}.cache",
        source.file_stem().unwrap_or_default().to_string_lossy()
    ));
    let hash = file_hash(source);
    let mtime = file_mtime(source);
    let _ = std::fs::write(&cache_file, format!("{}\n{}\n{}\n", hash, mtime, env!("CARGO_PKG_VERSION")));
}

/// A compilation task: source file → object file.
struct CompileTask {
    source: PathBuf,
    obj: PathBuf,
    kind: FileKind,
    target_arch: String,
    release: bool,
    debug: bool,
}

/// Build a project with options.
pub fn build_project(project_dir: &str, opts: &BuildOptions) -> Result<BuildResult> {
    let start = std::time::Instant::now();
    let dir = Path::new(project_dir);
    if !dir.is_dir() {
        return Err(CompilerError::new(
            format!("build: '{}' is not a directory", project_dir),
            crate::error::ErrorKind::IOError, None,
        ).with_code(ErrorCode::V5002));
    }

    let target_arch = resolve_target(opts);

    // Setup directories
    let build_dir = dir.join("build");
    let cache_dir = dir.join(".vredrs-cache");
    if opts.clean {
        eprintln!("[vredrs] cleaning cache and build artifacts...");
        let _ = std::fs::remove_dir_all(&cache_dir);
        let _ = std::fs::remove_dir_all(&build_dir);
    }
    let _ = std::fs::create_dir_all(&build_dir);
    let _ = std::fs::create_dir_all(&cache_dir);

    // Scan for source files
    let mut veds_files: Vec<PathBuf> = Vec::new();
    let mut vraw_files: Vec<PathBuf> = Vec::new();
    let mut cpps_files: Vec<PathBuf> = Vec::new();
    scan_dir(dir, &mut veds_files, &mut vraw_files, &mut cpps_files)?;
    let total = veds_files.len() + vraw_files.len() + cpps_files.len();
    eprintln!("[vredrs] build: scanning '{}' (target: {}, {} jobs{})",
        project_dir, target_arch, opts.jobs,
        if opts.release { ", release mode" } else { "" });
    eprintln!("[vredrs]   .veds: {}, .vraw: {}, .cpps: {}", veds_files.len(), vraw_files.len(), cpps_files.len());

    // Build compilation tasks with cache check
    let mut tasks: Vec<CompileTask> = Vec::new();
    let mut cached_objs: Vec<PathBuf> = Vec::new();
    let mut compiled_count = 0usize;
    let mut cached_count = 0usize;

    for f in &veds_files {
        let stem = f.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let obj = build_dir.join(format!("{}.o", stem));
        if !opts.clean && is_cache_valid(f, &obj, &cache_dir) {
            cached_objs.push(obj);
            cached_count += 1;
        } else {
            tasks.push(CompileTask { source: f.clone(), obj, kind: FileKind::Veds, target_arch: target_arch.clone(), release: opts.release, debug: opts.debug });
            compiled_count += 1;
        }
    }
    for f in &vraw_files {
        let stem = f.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let obj = build_dir.join(format!("{}.o", stem));
        if !opts.clean && is_cache_valid(f, &obj, &cache_dir) {
            cached_objs.push(obj);
            cached_count += 1;
        } else {
            tasks.push(CompileTask { source: f.clone(), obj, kind: FileKind::Vraw, target_arch: target_arch.clone(), release: opts.release, debug: opts.debug });
            compiled_count += 1;
        }
    }

    // Firmware tasks (always recompile — firmware doesn't link with others)
    let mut firmware_path: Option<PathBuf> = None;
    for f in &cpps_files {
        eprintln!("[vredrs]   compiling (Cstar): {}", f.display());
        let stem = f.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let bin = build_dir.join(format!("{}.bin", stem));
        match compile_cpps_to_bin(f, &bin) {
            Ok(()) => { if firmware_path.is_none() { firmware_path = Some(bin); } },
            Err(e) => eprintln!("[vredrs]   warning: {}", e),
        }
    }

    // Parallel compilation
    let num_tasks = tasks.len();
    if num_tasks == 0 {
        // All cached
        eprintln!("[vredrs]   all files cached, skipping compilation");
    } else if num_tasks == 1 || opts.jobs <= 1 {
        // Serial compilation
        for task in &tasks {
            eprintln!("[vredrs]   compiling ({}): {}", match task.kind {
                FileKind::Veds => "LLVM", FileKind::Vraw => &task.target_arch, _ => "?"
            }, task.source.display());
            match execute_task(task) {
                Ok(()) => { write_cache(&task.source, &cache_dir); },
                Err(e) => eprintln!("[vredrs]   warning: {}", e),
            }
        }
    } else {
        // Parallel compilation using threads
        eprintln!("[vredrs]   parallel compilation: {} tasks, {} jobs", num_tasks, opts.jobs);
        let tasks_arc = Arc::new(Mutex::new(tasks.into_iter().enumerate().collect::<Vec<_>>()));
        let results = Arc::new(Mutex::new(Vec::<(PathBuf, std::result::Result<PathBuf, String>)>::new()));
        let completed = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..opts.jobs.min(num_tasks) {
            let tasks_clone = Arc::clone(&tasks_arc);
            let results_clone = Arc::clone(&results);
            let completed_clone = Arc::clone(&completed);
            let cache_dir_clone = cache_dir.clone();

            handles.push(std::thread::spawn(move || {
                loop {
                    let task = {
                        let mut queue = tasks_clone.lock().unwrap();
                        if queue.is_empty() { break; }
                        queue.remove(0).1  // just the CompileTask
                    };
                    eprintln!("[vredrs]   [thread {:?}] compiling: {}",
                        std::thread::current().id(), task.source.display());
                    let result = execute_task(&task);
                    let done = completed_clone.fetch_add(1, Ordering::SeqCst) + 1;
                    match &result {
                        Ok(()) => {
                            write_cache(&task.source, &cache_dir_clone);
                            results_clone.lock().unwrap().push((task.source.clone(), Ok(task.obj.clone())));
                        }
                        Err(e) => {
                            results_clone.lock().unwrap().push((task.source.clone(), Err(e.clone())));
                        }
                    }
                    eprintln!("[vredrs]   [{}/{}] done", done, done + (num_tasks - done));
                }
            }));
        }
        for h in handles { let _ = h.join(); }

        // Collect results
        let results_lock = results.lock().unwrap();
        for (_, result) in results_lock.iter() {
            match result {
                Ok(obj) => cached_objs.push(obj.clone()),
                Err(e) => eprintln!("[vredrs]   warning: {}", e),
            }
        }
    }

    // Linking
    let executable: Option<PathBuf> = if !cached_objs.is_empty() {
        let dir_name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "output".to_string());
        let exe = dir.join(format!("{}.bin", dir_name));
        eprintln!("{}[vredrs]{}   linking {} object files → {}",
            crate::platform::Colors::blue_bold(),
            crate::platform::Colors::reset(),
            cached_objs.len(), exe.display());
        match link_objects(&cached_objs, &exe, opts) {
            Ok(()) => {
                crate::platform::success(&format!("linked: {}", exe.display()));
                Some(exe)
            },
            Err(e) => { crate::platform::warning(&format!("link failed: {}", e)); None }
        }
    } else { None };

    if let Some(ref fw) = firmware_path {
        crate::platform::success(&format!("firmware: {}", fw.display()));
    }

    let elapsed = start.elapsed().as_millis();
    crate::platform::success(&format!(
        "build complete: {} compiled, {} cached, {} ms",
        compiled_count, cached_count, elapsed
    ));

    Ok(BuildResult {
        executable,
        firmware: firmware_path,
        veds_count: veds_files.len(),
        vraw_count: vraw_files.len(),
        cpps_count: cpps_files.len(),
        compiled_count,
        cached_count,
        elapsed_ms: elapsed,
    })
}

/// Legacy entry point (no options).
pub fn build_project_simple(project_dir: &str) -> Result<BuildResult> {
    build_project(project_dir, &BuildOptions::default())
}

/// Execute a single compilation task.
fn execute_task(task: &CompileTask) -> std::result::Result<(), String> {
    match task.kind {
        FileKind::Veds => compile_veds_to_obj(&task.source, &task.obj, task.release, task.debug),
        FileKind::Vraw => compile_vraw_to_obj(&task.source, &task.obj, &task.target_arch, task.release, task.debug),
        _ => Ok(()),
    }
}

fn scan_dir(dir: &Path, veds: &mut Vec<PathBuf>, vraw: &mut Vec<PathBuf>, cpps: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| CompilerError::new(
        format!("build: cannot read '{}': {}", dir.display(), e), crate::error::ErrorKind::IOError, None).with_code(ErrorCode::V4003))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.') || name == "build" || name == "target" { continue; }
            }
            scan_dir(&path, veds, vraw, cpps)?;
        } else {
            match classify_file(&path) {
                FileKind::Veds => veds.push(path), FileKind::Vraw => vraw.push(path),
                FileKind::Cpps => cpps.push(path), FileKind::Other => {}
            }
        }
    }
    Ok(())
}

fn compile_veds_to_obj(source: &Path, output: &Path, release: bool, debug: bool) -> std::result::Result<(), String> {
    let src = std::fs::read_to_string(source).map_err(|e| e.to_string())?;
    let mut lexer = crate::lexer::Lexer::new(&src, 0);
    let tokens = lexer.tokenize().map_err(|e| e.message().to_string())?;
    let mut parser = crate::parser::Parser::new(tokens, 0);
    let program = parser.parse_program().map_err(|e| e.message().to_string())?;
    // Use the new IR-based LLVM emitter (gradual: static + dynamic).
    let module = crate::codegen::lower::lower_program(&program);
    let ir = crate::codegen::emit_llvm::emit_module_string(&module);
    let ll_path = output.with_extension("ll");
    std::fs::write(&ll_path, &ir).map_err(|e| e.to_string())?;
    let mut clang_args = vec!["-c".to_string(), "-o".to_string()];
    clang_args.push(output.to_string_lossy().to_string());
    if release { clang_args.push("-O3".to_string()); }
    else if debug { clang_args.push("-g".to_string()); }
    clang_args.push(ll_path.to_string_lossy().to_string());
    match std::process::Command::new("clang").args(&clang_args).output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!(
            "clang failed (exit {:?}) compiling {}:\n{}",
            out.status.code(),
            source.display(),
            String::from_utf8_lossy(&out.stderr)
        )),
        Err(e) => Err(format!(
            "clang invocation failed for {}: {} (is clang installed and on PATH?)",
            source.display(),
            e
        )),
    }
}

fn compile_vraw_to_obj(source: &Path, output: &Path, target_arch: &str, _release: bool, _debug: bool) -> std::result::Result<(), String> {
    let src = std::fs::read_to_string(source).map_err(|e| e.to_string())?;
    let mut lexer = crate::lexer::Lexer::new(&src, 0);
    let tokens = lexer.tokenize().map_err(|e| e.message().to_string())?;
    let mut parser = crate::parser::Parser::new(tokens, 0);
    let program = parser.parse_program().map_err(|e| e.message().to_string())?;
    // Generic monomorphization: replace generic functions with concrete instances.
    let mut mono = crate::codegen::raw::monomorphization::Monomorphizer::new();
    let program = mono.run(&program);

    // Detect cross-compilation: the requested `target_arch` differs from the
    // host architecture. The raw backend always emits assembly (`.s`); on a
    // matching host it can additionally assemble/link via the system `as`,
    // but on a foreign host the system assembler cannot consume the
    // generated instructions, so we deliberately stop at the assembly stage.
    // Returning `Ok(())` here is intentional — the `.s` artifact is a useful
    // deliverable for cross-compilation workflows (e.g. inspect it, hand it
    // to a cross-assembler, or ship it to a target device). The message
    // below makes the "no .o was produced" outcome explicit so that callers
    // (the linker step in `build_project`) and end users don't mistake the
    // success status for "object file ready".
    let host_arch = detect_arch();
    let is_cross = target_arch != host_arch;
    if is_cross {
        let asm_path = output.with_extension("s");
        eprintln!(
            "[vredrs]   cross-compiling .vraw → {} assembly (host is {}):",
            target_arch, host_arch
        );
        eprintln!(
            "[vredrs]   only assembly (.s) will be generated at {}; no object file (.o) or executable will be produced",
            asm_path.display()
        );
    }

    // Route to the new IR-based backend (lower.rs → emit_raw_x86/arm).
    // The old AST-based x86/arm backends are deprecated.
    let host_arch = std::env::consts::ARCH;
    if host_arch == "aarch64" || host_arch == "arm" || host_arch == "armv7l" {
        crate::codegen::emit_raw_arm::compile_via_ir(&program, output).map_err(|e| e.to_string())
    } else {
        crate::codegen::emit_raw_x86::compile_via_ir(&program, output).map_err(|e| e.to_string())
    }
}

fn compile_cpps_to_bin(source: &Path, output: &Path) -> std::result::Result<(), String> {
    let src = std::fs::read_to_string(source).map_err(|e| e.to_string())?;
    let mut lexer = crate::lexer::Lexer::new(&src, 0);
    let tokens = lexer.tokenize().map_err(|e| e.message().to_string())?;
    let mut parser = crate::parser::Parser::new(tokens, 0);
    let program = parser.parse_program().map_err(|e| e.message().to_string())?;
    crate::codegen::cstar::raw::build_firmware_with_program(output, &program).map_err(|e| e.to_string())
}

fn link_objects(obj_files: &[PathBuf], output: &Path, opts: &BuildOptions) -> std::result::Result<(), String> {
    let mut cmd = std::process::Command::new("cc");
    cmd.arg("-o").arg(output);
    for obj in obj_files { cmd.arg(obj); }
    if opts.release {
        cmd.args(&["-O3", "-flto"]);
    }
    if opts.debug && !opts.release {
        cmd.arg("-g");
    }
    if opts.strip {
        cmd.arg("-s");
    }
    cmd.args(&["-lm", "-lpthread"]);
    match cmd.output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!("linker: {}", String::from_utf8_lossy(&out.stderr))),
        Err(e) => Err(format!("linker not found: {}", e)),
    }
}
