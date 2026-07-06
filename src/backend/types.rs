//! Type system for the LLVM backend.
//!
//! Defines the IR-level type enum (`Ty`), value representation (`Val`),
//! function signatures (`Sig`), and class metadata. These types are shared
//! across all codegen submodules so there is no duplication of type-mapping
//! logic.

use crate::parser::ast::{BasicType, TypeExpr};

/// An IR-level type. Maps directly to an LLVM type string via [`Ty::ir`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ty {
    I64,
    F64,
    Bool,
    Str,
    List,
    Dict,
    Tuple,
    Set,
    Obj,
    Coro,
    Void,
    Ptr,
    /// Tagged dynamic value: `{ i8 tag, i64 payload }`.
    Value,
    /// First-class function pointer (closure without environment).
    /// IR representation is `i64` because we store the raw function pointer
    /// as an integer to allow tagged-value interop.
    Fn,
}

impl Ty {
    /// Return the LLVM IR type string for this type.
    pub fn ir(&self) -> &'static str {
        match self {
            Ty::I64 => "i64",
            Ty::F64 => "double",
            Ty::Bool => "i1",
            Ty::Str => "%vredrs.str*",
            Ty::List => "%vredrs.list*",
            Ty::Dict => "%vredrs.dict*",
            Ty::Tuple => "%vredrs.tuple*",
            Ty::Set => "%vredrs.dict*",
            Ty::Obj => "%vredrs.object*",
            Ty::Coro => "%vredrs.coro*",
            Ty::Void => "void",
            Ty::Ptr => "i8*",
            Ty::Value => "%vredrs.value",
            Ty::Fn => "i64",
        }
    }

    /// Whether this is a numeric type (int, float, or bool).
    pub fn is_numeric(&self) -> bool {
        matches!(self, Ty::I64 | Ty::F64 | Ty::Bool)
    }
}

/// A computed value during code generation: its IR type, SSA register name,
/// and optional class/async-fn metadata for dispatch.
#[derive(Debug, Clone)]
pub struct Val {
    pub ty: Ty,
    pub repr: String,
    pub class: Option<String>,
    pub async_fn: Option<String>,
}

impl Val {
    pub fn new(ty: Ty, repr: impl Into<String>) -> Self {
        Val {
            ty,
            repr: repr.into(),
            class: None,
            async_fn: None,
        }
    }

    pub fn with_class(mut self, c: impl Into<String>) -> Self {
        self.class = Some(c.into());
        self
    }

    pub fn with_async(mut self, a: impl Into<String>) -> Self {
        self.async_fn = Some(a.into());
        self
    }
}

/// A function or method signature.
#[derive(Debug, Clone)]
pub struct Sig {
    pub params: Vec<Ty>,
    pub ret: Ty,
}

/// Convert a Vredrs `TypeExpr` annotation to an IR `Ty`.
///
/// This is the single source of truth for type mapping — both the driver
/// and the codegen layer call this function, eliminating the previous
/// duplicate `ty_of_te` / `te_to_ty` pair.
pub fn type_expr_to_ty(ty: &TypeExpr) -> Ty {
    match ty {
        TypeExpr::Basic(BasicType::Int, _) => Ty::I64,
        TypeExpr::Basic(BasicType::Float, _) => Ty::F64,
        TypeExpr::Basic(BasicType::Bool, _) => Ty::Bool,
        TypeExpr::Basic(BasicType::Str, _) => Ty::Str,
        TypeExpr::Basic(BasicType::Void, _) => Ty::Void,
        TypeExpr::Basic(BasicType::Null, _) | TypeExpr::Basic(BasicType::Any, _) => Ty::Value,
        TypeExpr::Named(_, _) => Ty::Obj,
        TypeExpr::Optional(inner, _) => type_expr_to_ty(inner),
        TypeExpr::Generic { base, .. } => {
            if let TypeExpr::Basic(BasicType::Str, _) = base.as_ref() {
                Ty::Str
            } else if let TypeExpr::Named(id, _) = base.as_ref() {
                match id.name.as_str() {
                    "list" => Ty::List,
                    "dict" => Ty::Dict,
                    "tuple" => Ty::Tuple,
                    "set" => Ty::Set,
                    _ => Ty::Value,
                }
            } else {
                Ty::Value
            }
        }
        TypeExpr::Tuple(_, _) => Ty::Tuple,
        TypeExpr::Function { .. } => Ty::Value,
        _ => Ty::Value,
    }
}

/// Convert an optional annotation to a `Ty`, defaulting to `Ty::Value`
/// (tagged dynamic) when no annotation is present.
pub fn annot_to_ty(ty: Option<&TypeExpr>) -> Ty {
    match ty {
        Some(t) => type_expr_to_ty(t),
        None => Ty::Value,
    }
}

/// Extract the class name from a type annotation, if any.
pub fn class_of_annot(ty: Option<&TypeExpr>) -> Option<String> {
    match ty {
        Some(TypeExpr::Named(id, _)) => Some(id.name.clone()),
        Some(TypeExpr::Optional(inner, _)) => class_of_annot(Some(inner)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Span;
    use crate::parser::ast::*;

    #[test]
    fn primitive_types_map_correctly() {
        assert_eq!(
            type_expr_to_ty(&TypeExpr::Basic(BasicType::Int, Span::dummy())),
            Ty::I64
        );
        assert_eq!(
            type_expr_to_ty(&TypeExpr::Basic(BasicType::Float, Span::dummy())),
            Ty::F64
        );
        assert_eq!(
            type_expr_to_ty(&TypeExpr::Basic(BasicType::Bool, Span::dummy())),
            Ty::Bool
        );
        assert_eq!(
            type_expr_to_ty(&TypeExpr::Basic(BasicType::Str, Span::dummy())),
            Ty::Str
        );
        assert_eq!(
            type_expr_to_ty(&TypeExpr::Basic(BasicType::Void, Span::dummy())),
            Ty::Void
        );
    }

    #[test]
    fn null_and_any_become_value() {
        assert_eq!(
            type_expr_to_ty(&TypeExpr::Basic(BasicType::Null, Span::dummy())),
            Ty::Value
        );
        assert_eq!(
            type_expr_to_ty(&TypeExpr::Basic(BasicType::Any, Span::dummy())),
            Ty::Value
        );
    }

    #[test]
    fn named_types_become_obj() {
        let id = Identifier {
            name: "MyClass".into(),
            span: Span::dummy(),
        };
        assert_eq!(
            type_expr_to_ty(&TypeExpr::Named(id, Span::dummy())),
            Ty::Obj
        );
    }

    #[test]
    fn generic_containers_map_correctly() {
        let mk = |name: &str| TypeExpr::Generic {
            base: Box::new(TypeExpr::Named(
                Identifier {
                    name: name.into(),
                    span: Span::dummy(),
                },
                Span::dummy(),
            )),
            args: vec![],
            span: Span::dummy(),
        };
        assert_eq!(type_expr_to_ty(&mk("list")), Ty::List);
        assert_eq!(type_expr_to_ty(&mk("dict")), Ty::Dict);
        assert_eq!(type_expr_to_ty(&mk("tuple")), Ty::Tuple);
        assert_eq!(type_expr_to_ty(&mk("set")), Ty::Set);
    }

    #[test]
    fn annot_defaults_to_value() {
        assert_eq!(annot_to_ty(None), Ty::Value);
    }

    #[test]
    fn class_extraction() {
        let id = Identifier {
            name: "Foo".into(),
            span: Span::dummy(),
        };
        assert_eq!(
            class_of_annot(Some(&TypeExpr::Named(id, Span::dummy()))),
            Some("Foo".into())
        );
        assert_eq!(class_of_annot(None), None);
    }

    #[test]
    fn ir_strings_match_llvm_syntax() {
        assert_eq!(Ty::I64.ir(), "i64");
        assert_eq!(Ty::F64.ir(), "double");
        assert_eq!(Ty::Bool.ir(), "i1");
        assert_eq!(Ty::Str.ir(), "%vredrs.str*");
        assert_eq!(Ty::Void.ir(), "void");
    }

    #[test]
    fn is_numeric_covers_three_types() {
        assert!(Ty::I64.is_numeric());
        assert!(Ty::F64.is_numeric());
        assert!(Ty::Bool.is_numeric());
        assert!(!Ty::Str.is_numeric());
        assert!(!Ty::Obj.is_numeric());
    }

    #[test]
    fn val_builder_methods_chain() {
        let v = Val::new(Ty::Obj, "%t0")
            .with_class("Animal")
            .with_async("foo");
        assert_eq!(v.ty, Ty::Obj);
        assert_eq!(v.class.as_deref(), Some("Animal"));
        assert_eq!(v.async_fn.as_deref(), Some("foo"));
    }
}
