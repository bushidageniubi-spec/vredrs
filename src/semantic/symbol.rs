use crate::error::{CompilerError, Span};
use crate::parser::ast::*;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct SymbolTable {
    scopes: Vec<Scope>,
    pub types: HashMap<String, TypeInfo>,
}

impl SymbolTable {
    pub fn new() -> Self {
        SymbolTable {
            scopes: vec![Scope::new(ScopeKind::Global)],
            types: HashMap::new(),
        }
    }
    pub fn enter_scope(&mut self, kind: ScopeKind) {
        self.scopes.push(Scope::new(kind));
    }
    pub fn exit_scope(&mut self) {
        self.scopes.pop();
    }
    pub fn declare(&mut self, name: &str, sym: SymbolEntry) -> Result<(), CompilerError> {
        if self.scopes.is_empty() {
            self.scopes.push(Scope::new(ScopeKind::Global));
        }
        let scope = match self.scopes.last_mut() {
            Some(scope) => scope,
            None => {
                return Err(CompilerError::semantic_error(
                    "symbol table has no active scope",
                    Span::dummy(),
                ))
            }
        };
        if scope.symbols.contains_key(name) {
            Err(CompilerError::semantic_error(
                format!("duplicate symbol '{}'", name),
                sym.span(),
            ))
        } else {
            scope.symbols.insert(name.to_string(), sym);
            Ok(())
        }
    }
    pub fn lookup(&self, name: &str) -> Option<&SymbolEntry> {
        for scope in self.scopes.iter().rev() {
            if let Some(s) = scope.symbols.get(name) {
                return Some(s);
            }
        }
        None
    }
    pub fn declare_type(&mut self, name: &str, info: TypeInfo) {
        self.types.insert(name.to_string(), info);
    }
}

#[derive(Debug, Clone)]
pub struct Scope {
    pub kind: ScopeKind,
    pub symbols: HashMap<String, SymbolEntry>,
}
impl Scope {
    pub fn new(kind: ScopeKind) -> Self {
        Scope {
            kind,
            symbols: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeKind {
    Global,
    Function,
    Block,
    Class,
    Struct,
    Interface,
    Enum,
}

#[derive(Debug, Clone)]
pub enum SymbolEntry {
    Variable {
        name: String,
        typ: Option<TypeInfo>,
        mutable: bool,
        span: Span,
    },
    Function {
        name: String,
        params: Vec<(String, TypeInfo)>,
        ret: Option<TypeInfo>,
        is_async: bool,
        span: Span,
    },
    Type {
        name: String,
        info: TypeInfo,
        span: Span,
    },
}

impl SymbolEntry {
    pub fn span(&self) -> Span {
        match self {
            SymbolEntry::Variable { span, .. }
            | SymbolEntry::Function { span, .. }
            | SymbolEntry::Type { span, .. } => span.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeInfo {
    Basic(BasicType),
    Named(String),
    Generic(Box<TypeInfo>, Vec<TypeInfo>),
    Optional(Box<TypeInfo>),
    Function(Vec<TypeInfo>, Box<TypeInfo>),
    Channel(Box<TypeInfo>),
    List(Box<TypeInfo>),
    Dict(Box<TypeInfo>, Box<TypeInfo>),
    Set(Box<TypeInfo>),
    Tuple(Vec<TypeInfo>),
    Any,
    Unknown,
    Void,
    TypeVariable(String),
}

impl TypeInfo {
    pub fn from_ast_type(te: &TypeExpr) -> Self {
        match te {
            TypeExpr::Basic(bt, _) => match bt {
                BasicType::Void => TypeInfo::Void,
                BasicType::Any => TypeInfo::Any,
                _ => TypeInfo::Basic(bt.clone()),
            },
            TypeExpr::Named(id, _) => match id.name.as_str() {
                "int" => TypeInfo::Basic(BasicType::Int),
                "float" => TypeInfo::Basic(BasicType::Float),
                "str" => TypeInfo::Basic(BasicType::Str),
                "bool" => TypeInfo::Basic(BasicType::Bool),
                "null" => TypeInfo::Basic(BasicType::Null),
                "void" => TypeInfo::Void,
                "any" => TypeInfo::Any,
                _ => TypeInfo::Named(id.name.clone()),
            },
            TypeExpr::Generic { base, args, .. } => {
                let base_ty = TypeInfo::from_ast_type(base);
                let arg_tys: Vec<TypeInfo> = args.iter().map(TypeInfo::from_ast_type).collect();
                match (&base_ty, arg_tys.as_slice()) {
                    (TypeInfo::Named(name), [item]) if name == "list" => {
                        TypeInfo::List(Box::new(item.clone()))
                    }
                    (TypeInfo::Named(name), [item]) if name == "set" => {
                        TypeInfo::Set(Box::new(item.clone()))
                    }
                    (TypeInfo::Named(name), [key, value]) if name == "dict" => {
                        TypeInfo::Dict(Box::new(key.clone()), Box::new(value.clone()))
                    }
                    _ => TypeInfo::Generic(Box::new(base_ty), arg_tys),
                }
            }
            TypeExpr::Optional(inner, _) => {
                TypeInfo::Optional(Box::new(TypeInfo::from_ast_type(inner)))
            }
            TypeExpr::Function {
                params,
                return_type,
                ..
            } => TypeInfo::Function(
                params.iter().map(TypeInfo::from_ast_type).collect(),
                Box::new(TypeInfo::from_ast_type(return_type)),
            ),
            TypeExpr::Tuple(types, _) => {
                TypeInfo::Tuple(types.iter().map(TypeInfo::from_ast_type).collect())
            }
            TypeExpr::Channel(inner, _) => {
                TypeInfo::Channel(Box::new(TypeInfo::from_ast_type(inner)))
            }
            _ => TypeInfo::Unknown,
        }
    }
}
