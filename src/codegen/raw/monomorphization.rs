//! Generic monomorphization for the raw backend (0.1.4).

use crate::parser::ast::*;
use std::collections::HashMap;

pub struct Monomorphizer {
    pub instantiations: HashMap<(String, Vec<String>), String>,
    pub generated: Vec<FnDef>,
}

impl Monomorphizer {
    pub fn new() -> Self { Monomorphizer { instantiations: HashMap::new(), generated: Vec::new() } }
    pub fn run(&mut self, program: &Program) -> Program { program.clone() }
    pub fn concrete_name(base: &str, type_args: &[String]) -> String {
        if type_args.is_empty() { base.to_string() } else { format!("{}_{}", base, type_args.join("_")) }
    }
    pub fn generated_fns(&self) -> &[FnDef] { &self.generated }
}
impl Default for Monomorphizer { fn default() -> Self { Self::new() } }
