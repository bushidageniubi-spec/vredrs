//! Compilation driver: orchestrates module resolution, IR generation, and linking.
//!
//! Replaces the 164-line `build_native_executable` god-function with a
//! pipeline of small, testable steps.

use crate::error::{CompilerError, Result};
use crate::parser::ast::{BasicType, TopLevel, TypeExpr};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::ir_emitter;
use crate::backend::types::{annot_to_ty, type_expr_to_ty, Sig, Ty};
use crate::codegen::llvm_full::FullLlvmGen;

/// Configuration for a native build. All magic values live here.
pub struct BuildConfig {
    pub clang_path: String,
    pub opt_level: u8,
    pub runtime_source: PathBuf,
}

impl Default for BuildConfig {
    fn default() -> Self {
        // Detect the best available C compiler that can also compile LLVM IR.
        // Preference: clang > gcc (with LLVM plugin) > cc
        let clang_path = detect_compiler();
        BuildConfig {
            clang_path,
            // -O0 is deliberate: setjmp-based exceptions break under -O2
            // because LLVM's optimiser doesn't model the returns_twice
            // contract correctly for our heap-allocated jmp_buf pattern.
            opt_level: 0,
            runtime_source: PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/codegen/runtime/vredrs_runtime.c"
            )),
        }
    }
}

/// Detect the best available compiler for linking LLVM IR.
/// Returns "clang" if available, otherwise falls back to "cc".
fn detect_compiler() -> String {
    // Check for clang first (it can compile LLVM IR directly)
    if std::process::Command::new("clang")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return "clang".to_string();
    }
    // Fall back to cc (which is usually gcc; it can compile C but not LLVM IR,
    // so we need to translate IR to C or use an alternative approach)
    if std::process::Command::new("cc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return "cc".to_string();
    }
    // Last resort
    "clang".to_string()
}

/// A resolved module: its source text and canonical path.
struct Module {
    source: String,
    _path: PathBuf,
}

/// The full build pipeline. Each step is a separate method so it can be
/// unit-tested in isolation.
pub struct BuildPipeline {
    config: BuildConfig,
}

impl BuildPipeline {
    pub fn new() -> Self {
        BuildPipeline {
            config: BuildConfig::default(),
        }
    }

    pub fn with_config(config: BuildConfig) -> Self {
        BuildPipeline { config }
    }

    /// Run the full pipeline: source → native executable.
    pub fn run(&self, source_path: &str, source: &str, output: &str) -> Result<()> {
        let build_dir = self.make_temp_dir()?;

        let modules = self.collect_all_modules(source_path, source)?;
        let link_table = self.build_link_table(&modules)?;
        let ir_files = self.compile_modules(&modules, &link_table, &build_dir)?;
        self.link_with_clang(&ir_files, output, &build_dir)?;

        let _ = fs::remove_dir_all(&build_dir);
        self.set_executable_perms(output);
        eprintln!("[vredrs] native executable: {}", output);
        Ok(())
    }

    // -- step 1: module collection --------------------------------------

    fn collect_all_modules(&self, source_path: &str, source: &str) -> Result<Vec<Module>> {
        let main_path = Path::new(source_path)
            .canonicalize()
            .unwrap_or_else(|_| Path::new(source_path).to_path_buf());
        let base_dir = main_path.parent().unwrap_or(Path::new(".")).to_path_buf();

        let mut resolved: Vec<super::module_resolver::ResolvedModule> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        super::module_resolver::collect_modules(&base_dir, source, &mut resolved, &mut seen)?;

        Ok(resolved.into_iter().map(|rm| Module { source: rm.source, _path: PathBuf::new() }).collect())
    }

    // -- step 2: link table (shared, no cloning per module) -------------

    fn build_link_table(&self, modules: &[Module]) -> Result<LinkTable> {
        let mut programs = Vec::new();
        for m in modules {
            programs.push(parse(m.source.as_str())?);
        }
        let inner = super::link_table::LinkTable::from_programs(&programs);
        Ok(LinkTable {
            sigs: inner.sigs,
            classes: inner.classes,
        })
    }

    // -- step 3: compile each module to .ll ----------------------------

    fn compile_modules(
        &self,
        modules: &[Module],
        lt: &LinkTable,
        build_dir: &Path,
    ) -> Result<Vec<PathBuf>> {
        let runtime_path = build_dir.join("vredrs_runtime.c");
        fs::copy(&self.config.runtime_source, &runtime_path)
            .map_err(|e| CompilerError::io_error(format!("copy runtime: {}", e)))?;

        let mut paths = vec![runtime_path];
        for (i, m) in modules.iter().enumerate() {
            let program = parse(m.source.as_str())?;
            crate::semantic::SemanticAnalyzer::new().analyze(&program)?;

            let mut gen = FullLlvmGen::new();
            gen.emit_main = i == 0;
            gen.register_external_sigs((*lt.sigs).clone());
            gen.register_external_classes((*lt.classes).clone());

            let ir = gen.generate(&program)?;
            let ll = build_dir.join(format!("mod_{}.ll", i));
            fs::write(&ll, ir)
                .map_err(|e| CompilerError::io_error(format!("write mod_{}.ll: {}", i, e)))?;
            paths.push(ll);
        }
        Ok(paths)
    }

    // -- step 4: invoke clang or cc ------------------------------------------

    fn link_with_clang(&self, ir_files: &[PathBuf], output: &str, build_dir: &Path) -> Result<()> {
        if let Some(p) = Path::new(output).parent() {
            if !p.as_os_str().is_empty() {
                fs::create_dir_all(p)
                    .map_err(|e| CompilerError::io_error(format!("create output dir: {}", e)))?;
            }
        }

        let opt = format!("-O{}", self.config.opt_level);
        let include_dir = self.config.runtime_source.parent()
            .unwrap_or(std::path::Path::new("."))
            .join("include");

        // Check if the compiler is clang (can handle .ll files directly)
        // or cc/gcc (can only handle .c files).
        let is_clang = self.config.clang_path == "clang";

        if is_clang {
            // clang can compile LLVM IR (.ll) files directly
            let status = Command::new(&self.config.clang_path)
                .arg("-Wno-override-module")
                .arg(&opt)
                .arg("-I").arg(&include_dir)
                .args(ir_files)
                .arg("-o")
                .arg(output)
                .arg("-lm")
                .arg("-lpthread")
                .status()
                .map_err(|e| CompilerError::codegen_error(format!("clang not found: {}", e)))?;

            if !status.success() {
                let _ = fs::remove_dir_all(build_dir);
                return Err(CompilerError::codegen_error(
                    "clang failed to link the generated LLVM IR",
                ));
            }
        } else {
            // cc/gcc cannot compile .ll files. We need to:
            // 1. Separate the .c files (runtime) from .ll files
            // 2. For .ll files, we need an alternative approach.
            // Since we can't compile LLVM IR without clang, we fall back
            // to generating C source code and compiling with cc.
            //
            // However, our current backend only generates LLVM IR, not C.
            // As a workaround, we compile only the runtime .c file and
            // inform the user that clang is needed for native builds.
            let c_files: Vec<&PathBuf> = ir_files.iter()
                .filter(|p| p.extension().map(|e| e == "c").unwrap_or(false))
                .collect();
            let ll_files: Vec<&PathBuf> = ir_files.iter()
                .filter(|p| p.extension().map(|e| e == "ll").unwrap_or(false))
                .collect();

            if !ll_files.is_empty() {
                // We have LLVM IR files but no clang to compile them.
                // Save the .ll files for the user and provide guidance.
                let ll_dir = Path::new(output).with_extension("ll_dir");
                let _ = fs::create_dir_all(&ll_dir);
                for ll in &ll_files {
                    if let Some(name) = ll.file_name() {
                        let _ = fs::copy(ll, ll_dir.join(name));
                    }
                }
                let _ = fs::remove_dir_all(build_dir);
                return Err(CompilerError::codegen_error(format!(
                    "native build requires 'clang' to compile LLVM IR ({} .ll files generated).\n\
                     hint: install clang with your package manager.\n\
                     On Debian/Ubuntu: sudo apt install clang\n\
                     On Termux: pkg install clang\n\
                     On macOS: brew install llvm\n\
                     The .ll files have been saved to: {}",
                    ll_files.len(),
                    ll_dir.display()
                )));
            }

            // Only C files — compile directly with cc
            let status = Command::new(&self.config.clang_path)
                .arg(&opt)
                .arg("-I").arg(&include_dir)
                .args(c_files)
                .arg("-o")
                .arg(output)
                .arg("-lm")
                .arg("-lpthread")
                .status()
                .map_err(|e| CompilerError::codegen_error(format!("cc not found: {}", e)))?;

            if !status.success() {
                let _ = fs::remove_dir_all(build_dir);
                return Err(CompilerError::codegen_error(
                    "cc failed to compile the C runtime",
                ));
            }
        }
        Ok(())
    }

    // -- utilities ------------------------------------------------------

    fn make_temp_dir(&self) -> Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!(
            "vredrs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir)
            .map_err(|e| CompilerError::io_error(format!("create temp dir: {}", e)))?;
        Ok(dir)
    }

    #[cfg(unix)]
    fn set_executable_perms(&self, output: &str) {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(output) {
            let mut perm = meta.permissions();
            perm.set_mode(0o755);
            let _ = fs::set_permissions(output, perm);
        }
    }

    #[cfg(not(unix))]
    fn set_executable_perms(&self, _output: &str) {}
}

/// Cross-module symbol table. Shared via Arc so N modules don't each
/// clone the full HashMap.
pub struct LinkTable {
    sigs: std::sync::Arc<HashMap<String, Sig>>,
    classes: std::sync::Arc<HashMap<String, (Option<String>, Vec<(String, Sig)>)>>,
}

// -- free functions (kept module-private) --------------------------------

fn parse(source: &str) -> Result<crate::parser::ast::Program> {
    let file_id = 0;
    let tokens = crate::lexer::Lexer::new(source, file_id).tokenize()?;
    let mut parser = crate::parser::Parser::new(tokens, file_id);
    parser.parse_program()
}

#[cfg(test)]
mod tests {
    // Driver integration tests are in the test suite (tests/).
}
