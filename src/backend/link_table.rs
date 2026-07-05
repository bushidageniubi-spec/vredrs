//! Cross-module symbol table construction.
//!
//! Scans all modules' ASTs to build a unified table of function signatures
//! and class metadata. This table is shared (via `Arc`) with each module's
//! code generator so cross-module calls resolve correctly.

use crate::backend::types::{Sig, Ty, type_expr_to_ty, annot_to_ty};
use crate::parser::ast::{FnDef, Program, TopLevel, TypeExpr};
use std::collections::HashMap;
use std::sync::Arc;

/// Shared, immutable symbol table for cross-module resolution.
pub struct LinkTable {
    pub sigs: Arc<HashMap<String, Sig>>,
    pub classes: Arc<HashMap<String, ClassMeta>>,
}

/// Class metadata: parent class name and ordered method list.
pub type ClassMeta = (Option<String>, Vec<(String, Sig)>);

impl LinkTable {
    /// Build a link table by scanning all module ASTs.
    pub fn from_programs(programs: &[Program]) -> Self {
        let mut sigs: HashMap<String, Sig> = HashMap::new();
        let mut classes: HashMap<String, ClassMeta> = HashMap::new();

        // First pass: collect all class definitions and function signatures.
        for prog in programs {
            for decl in &prog.declarations {
                if let TopLevel::FnDef(f) = decl {
                    sigs.insert(f.name.name.clone(), sig_of(f));
                }
            }
            collect_class_metadata(prog, &mut classes);
        }

        // Second pass: resolve inherited methods for classes whose parents
        // were in a later module (not yet processed during the first pass).
        // Repeat until no more parents are discovered.
        let mut changed = true;
        while changed {
            changed = false;
            let class_names: Vec<String> = classes.keys().cloned().collect();
            for name in &class_names {
                for prog in programs {
                    for decl in &prog.declarations {
                        if let TopLevel::ClassDef(cd) = decl {
                            if cd.name.name == *name {
                                let prev_methods = classes.get(name).map(|c| c.1.len()).unwrap_or(0);
                                collect_class_metadata(prog, &mut classes);
                                let new_methods = classes.get(name).map(|c| c.1.len()).unwrap_or(0);
                                if new_methods > prev_methods {
                                    changed = true;
                                }
                            }
                        }
                    }
                }
            }
        }

        LinkTable {
            sigs: Arc::new(sigs),
            classes: Arc::new(classes),
        }
    }
}

/// Extract a function signature from a FnDef AST node.
fn sig_of(f: &FnDef) -> Sig {
    let params: Vec<Ty> = f
        .params
        .iter()
        .map(|p| annot_to_ty(p.type_annotation.as_ref()))
        .collect();
    let ret = f
        .return_type
        .as_ref()
        .map(type_expr_to_ty)
        .unwrap_or(Ty::Value);
    Sig { params, ret }
}

/// Collect class metadata (parent + method signatures) from a program.
///
/// Inherited methods from parent classes are prepended to the method list
/// so that vtable indices are consistent across the inheritance chain.
fn collect_class_metadata(program: &Program, out: &mut HashMap<String, ClassMeta>) {
    // First pass: register all class names and their parents.
    let mut parents: HashMap<String, Option<String>> = HashMap::new();
    for decl in &program.declarations {
        if let TopLevel::ClassDef(c) = decl {
            let parent = match &c.extends {
                Some(TypeExpr::Named(id, _)) => Some(id.name.clone()),
                _ => None,
            };
            parents.insert(c.name.name.clone(), parent);
        }
    }

    // Second pass: build method lists with inheritance.
    for decl in &program.declarations {
        if let TopLevel::ClassDef(c) = decl {
            let name = c.name.name.clone();
            let parent = parents.get(&name).cloned().flatten();
            let mut methods: Vec<(String, Sig)> = Vec::new();

            // Inherited methods first (so child overrides by index).
            if let Some(pn) = &parent {
                if let Some((_, parent_methods)) = out.get(pn) {
                    methods = parent_methods.clone();
                }
            }

            for m in &c.methods {
                let sig = sig_of(m);
                if let Some(idx) = methods.iter().position(|(n, _)| n == &m.name.name) {
                    methods[idx] = (m.name.name.clone(), sig);
                } else {
                    methods.push((m.name.name.clone(), sig));
                }
            }

            out.insert(name, (parent, methods));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Span;
    use crate::parser::ast::*;

    #[test]
    fn sig_of_extracts_params_and_return() {
        let f = FnDef {
            annotations: vec![],
            name: Identifier { name: "test".into(), span: Span::dummy() },
            params: vec![
                FnParam {
                    name: Identifier { name: "x".into(), span: Span::dummy() },
                    type_annotation: Some(TypeExpr::Basic(BasicType::Int, Span::dummy())),
                    default_value: None,
                    is_variadic: false,
                    span: Span::dummy(),
                },
            ],
            return_type: Some(TypeExpr::Basic(BasicType::Str, Span::dummy())),
            body: vec![],
            is_constexpr: false,
            is_lazy: false,
            is_async: false,
            is_extern: false,
            extern_link: None,
            type_constraints: std::collections::HashMap::new(),
            span: Span::dummy(),
        };
        let sig = sig_of(&f);
        assert_eq!(sig.params.len(), 1);
        assert_eq!(sig.params[0], Ty::I64);
        assert_eq!(sig.ret, Ty::Str);
    }

    #[test]
    fn empty_program_produces_empty_table() {
        let prog = Program {
            declarations: vec![],
            span: Span::dummy(),
        };
        let lt = LinkTable::from_programs(&[prog]);
        assert!(lt.sigs.is_empty());
        assert!(lt.classes.is_empty());
    }
}
