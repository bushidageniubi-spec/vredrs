//! Module loading, caching, and circular dependency detection.
//!
//! Handles `import` statements by resolving module paths, loading source
//! files, executing them in a sub-interpreter, and caching the exports.

use super::*;
use crate::error::{CompilerError, Result};
use crate::parser::ast::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::fs;

impl Interpreter {
    /// Handle an `import` statement: load the module and bind its exports.
    pub(crate) fn import_module(&mut self, imp: &ImportStmt) -> Result<()> {
        let module = self.load_module(&imp.module)?;
        if let Some(symbols) = &imp.symbols {
            for sym in symbols {
                let value = module.get(&sym.name).cloned().ok_or_else(|| {
                    self.runtime_error(format!(
                        "module '{}' has no exported symbol '{}'",
                        imp.module, sym.name
                    ))
                })?;
                self.define_local(&sym.name, value);
            }
            return Ok(());
        }
        let bind_name = imp
            .alias
            .as_ref()
            .map(|a| a.name.clone())
            .unwrap_or_else(|| Self::module_bind_name(&imp.module));
        let bn = bind_name.clone();
        self.define_local(&bind_name, Value::Module(bn, module));
        Ok(())
    }

    /// Derive a local binding name from a module path.
    pub fn module_bind_name(module: &str) -> String {
        let trimmed = module
            .trim_end_matches(".veds")
            .trim_end_matches(".cpps");
        Path::new(trimmed)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(trimmed)
            .split('.')
            .last()
            .unwrap_or(trimmed)
            .trim_start_matches('.')
            .to_string()
    }

    /// Load a module by name, using the cache if available.
    pub(crate) fn load_module(&mut self, module: &str) -> Result<HashMap<String, Value>> {
        if let Some(std) = self.builtin_module(module) {
            return Ok(std);
        }
        let path = self.resolve_module_path(module)?;
        let key = path.to_string_lossy().to_string();
        if let Some(cached) = self.module_cache.get(&key) {
            return Ok(cached.clone());
        }
        if self.loading_stack.iter().any(|p| p == &key) {
            let chain = self
                .loading_stack
                .iter()
                .cloned()
                .chain(std::iter::once(key.clone()))
                .collect::<Vec<_>>()
                .join(" -> ");
            return Err(self.runtime_error(format!("circular import detected: {}", chain)));
        }
        self.loading_stack.push(key.clone());
        let source = fs::read_to_string(&path)
            .map_err(|e| CompilerError::io_error(format!("import '{}': {}", path.display(), e)))?;
        let mut lexer = crate::lexer::Lexer::new(&source, self.loading_stack.len());
        let tokens = lexer.tokenize()?;
        let mut parser = crate::parser::Parser::new(tokens, self.loading_stack.len());
        let program = parser.parse_program()?;
        let base = path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
        let mut sub = Interpreter::with_base_dir(base);
        sub.module_cache = self.module_cache.clone();
        sub.loading_stack = self.loading_stack.clone();
        sub.run(&program)?;
        self.module_cache.extend(sub.module_cache.clone());
        let exports = sub.module_exports();
        self.module_cache.insert(key.clone(), exports.clone());
        self.loading_stack.pop();
        Ok(exports)
    }

    /// Resolve a module name to a filesystem path.
    pub(crate) fn resolve_module_path(&self, module: &str) -> Result<PathBuf> {
        let raw = module.replace("::", "/");
        let mut candidates = vec![];
        let p = PathBuf::from(&raw);
        if p.is_absolute() {
            candidates.push(p.clone());
        } else {
            candidates.push(self.base_dir.join(&raw));
        }
        if !raw.ends_with(".veds") && !raw.ends_with(".cpps") {
            let dotted = raw.replace('.', "/");
            candidates.push(self.base_dir.join(format!("{}.veds", raw)));
            candidates.push(self.base_dir.join(format!("{}.veds", dotted)));
        }
        for c in &candidates {
            if c.exists() {
                return Ok(fs::canonicalize(c).unwrap_or_else(|_| c.clone()));
            }
        }
        Err(self.runtime_error(format!(
            "module '{}' not found relative to {}",
            module,
            self.base_dir.display()
        )))
    }

    /// Collect all exports from the global scope.
    pub(crate) fn module_exports(&self) -> HashMap<String, Value> {
        let global = self.vars.first().cloned().unwrap_or_default();
        let mut exports = HashMap::new();
        if !self.exported.is_empty() {
            for name in &self.exported {
                if let Some(v) = global.get(name) {
                    exports.insert(name.clone(), v.clone());
                }
            }
        } else {
            for (name, value) in global {
                if !self.builtins.contains(&name) {
                    exports.insert(name, value);
                }
            }
        }
        exports
    }

    /// Return a built-in module (math, os, time) if the name matches.
    pub(crate) fn builtin_module(&self, module: &str) -> Option<HashMap<String, Value>> {
        match module {
            "math" => Some(HashMap::from([
                ("sqrt".to_string(), Value::Native(Interpreter::native_math_sqrt)),
                ("pow".to_string(), Value::Native(Interpreter::native_math_pow)),
                ("sin".to_string(), Value::Native(Interpreter::native_math_sin)),
                ("PI".to_string(), Value::Float(std::f64::consts::PI)),
                ("E".to_string(), Value::Float(std::f64::consts::E)),
            ])),
            "os" => Some(HashMap::from([
                ("args".to_string(), Value::Native(Interpreter::native_os_args)),
                ("env".to_string(), Value::Native(Interpreter::native_os_env)),
            ])),
            "time" => Some(HashMap::from([
                ("now".to_string(), Value::Native(Interpreter::native_time_now)),
                ("sleep".to_string(), Value::Native(Interpreter::native_time_sleep)),
            ])),
            _ => None,
        }
    }
}
