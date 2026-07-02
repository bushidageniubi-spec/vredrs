//! Recursive module resolution with cycle detection.
//!
//! Given an entry source file, resolves all transitively imported modules
//! and returns them in dependency order (imports before importers).

use crate::error::{CompilerError, Result};
use crate::parser::ast::{Program, TopLevel};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::fs;

/// A resolved module: its source text and a content hash for deduplication.
pub struct ResolvedModule {
    pub source: String,
    pub hash: String,
}

/// Recursively collect the main source and all imported modules.
///
/// Cycle detection is via a `seen` set of content hashes. If a module's
/// hash is already in the set, it is skipped — this naturally handles
/// circular imports by including each unique module text only once.
pub fn collect_modules(
    base_dir: &Path,
    source: &str,
    out: &mut Vec<ResolvedModule>,
    seen: &mut HashSet<String>,
) -> Result<()> {
    let hash = fnv1a(source);
    if !seen.insert(hash.clone()) {
        return Ok(());
    }
    out.push(ResolvedModule {
        source: source.to_string(),
        hash,
    });

    let program = parse(source)?;
    for decl in &program.declarations {
        if let TopLevel::Import(imp) = decl {
            let path = resolve_import_path(base_dir, &imp.module)?;
            if let Ok(src) = fs::read_to_string(&path) {
                let sub_base = path.parent().unwrap_or(base_dir).to_path_buf();
                collect_modules(&sub_base, &src, out, seen)?;
            }
        }
    }
    Ok(())
}

/// Resolve a module name to a filesystem path.
///
/// Tries the following extensions and directory layouts:
/// - `module` (as-is, for absolute paths)
/// - `module.veds`
/// - `module` with `.` replaced by `/`
pub fn resolve_import_path(base_dir: &Path, module: &str) -> Result<PathBuf> {
    let raw = module.replace("::", "/");
    let dotted = raw.replace('.', "/");

    for candidate in [
        base_dir.join(&raw),
        base_dir.join(format!("{}.veds", raw)),
        base_dir.join(format!("{}.veds", dotted)),
    ] {
        if candidate.exists() {
            return Ok(candidate.canonicalize().unwrap_or(candidate));
        }
    }
    Err(CompilerError::io_error(format!(
        "module '{}' not found relative to {}",
        module,
        base_dir.display()
    )))
}

fn parse(source: &str) -> Result<Program> {
    let tokens = crate::lexer::Lexer::new(source, 0).tokenize()?;
    let mut parser = crate::parser::Parser::new(tokens, 0);
    parser.parse_program()
}

/// FNV-1a 64-bit hash for module deduplication.
fn fnv1a(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("m{:016x}", h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_is_deterministic() {
        assert_eq!(fnv1a("hello"), fnv1a("hello"));
        assert_ne!(fnv1a("hello"), fnv1a("world"));
    }

    #[test]
    fn fnv1a_distinguishes_inputs() {
        let a = fnv1a("module A");
        let b = fnv1a("module B");
        assert_ne!(a, b);
    }

    #[test]
    fn resolve_import_finds_existing_file() {
        let dir = std::env::temp_dir();
        let test_file = dir.join("vredrs_test_resolve.veds");
        fs::write(&test_file, "test").unwrap();
        let resolved = resolve_import_path(&dir, "vredrs_test_resolve.veds").unwrap();
        assert!(resolved.ends_with("vredrs_test_resolve.veds"));
        let _ = fs::remove_file(&test_file);
    }

    #[test]
    fn resolve_import_errors_on_missing() {
        let result = resolve_import_path(Path::new("/nonexistent"), "missing_module");
        assert!(result.is_err());
    }

    #[test]
    fn collect_modules_deduplicates() {
        let src = r#"paste, "hello\n""#;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        collect_modules(Path::new("."), src, &mut out, &mut seen).unwrap();
        assert_eq!(out.len(), 1);

        // Collecting the same source again should not add a duplicate.
        let mut out2 = Vec::new();
        let mut seen2 = seen.clone();
        collect_modules(Path::new("."), src, &mut out2, &mut seen2).unwrap();
        assert_eq!(out2.len(), 0);
    }
}
