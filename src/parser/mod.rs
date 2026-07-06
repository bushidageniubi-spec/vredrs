pub mod ast;
#[cfg(test)]
mod tests;

use self::ast::*;
use crate::error::{CompilerError, Result, Span};
use crate::lexer::token::{Token, TokenKind};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Collected errors when running in multi-error mode. If non-empty,
    /// `parse_program` returns the first error after parsing as much as
    /// possible. Callers can inspect `collected_errors` for the full list.
    pub collected_errors: Vec<CompilerError>,
    /// When true, a parse error at top level does not immediately abort;
    /// instead the error is recorded and the parser skips to the next
    /// plausible top-level boundary (a line starting with a keyword like
    /// `fn`, `class`, `set`, etc. at column 0) and continues.
    recovery_mode: bool,
    /// Side-channel for generic type-parameter constraints collected by
    /// `parse_fn_params` when it sees `[T: Drawable, ...]`. The caller
    /// (`parse_fn_def`) reads and clears this after the params are parsed.
    pending_type_constraints: std::collections::HashMap<String, Vec<String>>,
}

impl Parser {
    pub fn new(tokens: Vec<Token>, _file_id: usize) -> Self {
        Parser {
            tokens,
            pos: 0,
            collected_errors: Vec::new(),
            recovery_mode: false,
            pending_type_constraints: std::collections::HashMap::new(),
        }
    }

    /// Enable multi-error collection mode. When enabled, `parse_program`
    /// will attempt to recover from top-level parse errors and collect
    /// them in `collected_errors` instead of returning on the first error.
    pub fn with_recovery(mut self) -> Self {
        self.recovery_mode = true;
        self
    }

    pub fn parse_program(&mut self) -> Result<Program> {
        let mut decls = vec![];
        while !self.is_at_end() {
            self.skip_newlines();
            if self.is_at_end() {
                break;
            }
            if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                let e = self.err("Unexpected /end");
                if self.recovery_mode {
                    self.collected_errors.push(e);
                    self.advance();
                    continue;
                }
                return Err(e);
            }
            match self.parse_top_level() {
                Ok(d) => decls.push(d),
                Err(e) => {
                    if self.recovery_mode {
                        self.collected_errors.push(e);
                        self.skip_to_top_level();
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        if let Some(first) = self.collected_errors.first().cloned() {
            return Err(first);
        }
        Ok(Program {
            declarations: decls,
            span: Span::dummy(),
        })
    }

    /// Skip tokens until we reach a plausible top-level boundary: a line
    /// that starts with a declaration keyword at the beginning of a line
    /// (not indented). This is a heuristic recovery point.
    fn skip_to_top_level(&mut self) {
        while !self.is_at_end() {
            // Skip newlines and indents/dedents.
            if matches!(
                self.current_kind(),
                TokenKind::Newline | TokenKind::Indent | TokenKind::Dedent | TokenKind::Comment
            ) {
                self.advance();
                continue;
            }
            // Check if this looks like a top-level keyword.
            if matches!(
                self.current_kind(),
                TokenKind::Fn
                    | TokenKind::Async
                    | TokenKind::Class
                    | TokenKind::Struct
                    | TokenKind::Enum
                    | TokenKind::Import
                    | TokenKind::Export
                    | TokenKind::Set
                    | TokenKind::Constexpr
                    | TokenKind::Lazy
                    | TokenKind::Type
                    | TokenKind::Interface
                    | TokenKind::Extern
                    | TokenKind::Trait
                    | TokenKind::Macro
            ) {
                // Check that we're at the start of a line (previous token
                // was a newline or we're at position 0).
                if self.pos == 0
                    || self
                        .tokens
                        .get(self.pos.saturating_sub(1))
                        .map(|t| matches!(t.kind, TokenKind::Newline))
                        .unwrap_or(true)
                {
                    return;
                }
            }
            self.advance();
        }
    }

    fn parse_top_level(&mut self) -> Result<TopLevel> {
        let annotations = self.parse_annotations();
        match self.current_kind() {
            TokenKind::Import => Ok(TopLevel::Import(self.parse_import()?)),
            TokenKind::Export => Ok(TopLevel::Export(self.parse_export()?)),
            TokenKind::Extern => self.parse_extern_fn_def(),
            TokenKind::Fn | TokenKind::Async => {
                let mut fd = self.parse_fn_def()?;
                fd.annotations = annotations;
                Ok(TopLevel::FnDef(fd))
            }
            TokenKind::Struct => {
                let mut sd = self.parse_struct_def()?;
                sd.annotations = annotations;
                Ok(TopLevel::StructDef(sd))
            }
            TokenKind::Class => {
                let mut cd = self.parse_class_def()?;
                cd.annotations = annotations;
                Ok(TopLevel::ClassDef(cd))
            }
            TokenKind::Enum => {
                let mut ed = self.parse_enum_def()?;
                ed.annotations = annotations;
                Ok(TopLevel::EnumDef(ed))
            }
            TokenKind::Type => Ok(TopLevel::TypeAlias(self.parse_type_alias()?)),
            TokenKind::Interface => Ok(TopLevel::InterfaceDef(self.parse_interface_def()?)),
            TokenKind::Constexpr => Ok(TopLevel::ConstExpr(self.parse_constexpr()?)),
            TokenKind::Lazy => self.parse_lazy(),
            TokenKind::Marker => Ok(TopLevel::MarkerTrait(self.parse_marker_trait()?)),
            TokenKind::Macro => Ok(TopLevel::MacroDef(self.parse_macro_def()?)),
            TokenKind::Plugin => Ok(TopLevel::PluginDef(self.parse_plugin_def()?)),
            TokenKind::Test => Ok(TopLevel::TestBlock(self.parse_test_block()?)),
            TokenKind::Bench => Ok(TopLevel::BenchBlock(self.parse_bench_block()?)),
            TokenKind::Trait => Ok(TopLevel::TraitDef(self.parse_trait_def()?)),
            TokenKind::Impl => Ok(TopLevel::ImplBlock(self.parse_impl_block()?)),
            TokenKind::Dtor => Ok(TopLevel::DtorBlock(self.parse_dtor_block()?)),
            TokenKind::Unsafe => {
                // `unsafe` is both a keyword (unsafe block) and a module name.
                // If followed by `.member`, treat it as a module access, not
                // an unsafe block.
                if self.peek_kind() == Some(TokenKind::Dot) {
                    let stmt = self.parse_stmt()?;
                    Ok(TopLevel::Statement(stmt))
                } else {
                    Ok(TopLevel::Statement(Stmt::UnsafeBlock(self.parse_unsafe()?)))
                }
            }
            TokenKind::DirectiveOrScope => {
                if self.lex() == "/end" {
                    self.advance();
                    Err(self.err("Unexpected /end"))
                } else {
                    Ok(TopLevel::Statement(self.parse_directive()?))
                }
            }
            _ => {
                let stmt = self.parse_stmt()?;
                Ok(TopLevel::Statement(stmt))
            }
        }
    }

    fn parse_annotations(&mut self) -> Vec<Annotation> {
        let mut annots = vec![];
        while self.current_kind() == TokenKind::At {
            self.advance();
            if self.current_kind() != TokenKind::Identifier {
                break;
            }
            let name = if let Ok(id) = self.parse_id() {
                id
            } else {
                break;
            };
            let mut args = vec![];
            if self.current_kind() == TokenKind::LParen {
                self.advance();
                while self.current_kind() != TokenKind::RParen && !self.is_at_end() {
                    let n = if self.current_kind() == TokenKind::Identifier
                        && self.peek_kind() == Some(TokenKind::Assign)
                    {
                        let s = self.lex().to_string();
                        self.advance();
                        self.advance();
                        Some(s)
                    } else {
                        None
                    };
                    let v = if let Ok(e) = self.parse_expr() {
                        e
                    } else {
                        break;
                    };
                    args.push(AnnotationArg {
                        name: n,
                        value: v,
                        span: Span::dummy(),
                    });
                    if self.current_kind() == TokenKind::Comma {
                        self.advance();
                    }
                }
                if self.current_kind() == TokenKind::RParen {
                    self.advance();
                }
            } else if self.current_kind() == TokenKind::LBrace {
                // C* physical annotations such as @repo { ... } and @package { ... }
                // are accepted by the shared Vredrs parser as a single literal block
                // argument.  The C* backend also scans the source text for the full
                // configuration, so this preserves parseability without affecting the
                // standard Vredrs backend.
                let mut depth = 0usize;
                let mut raw = String::new();
                loop {
                    if self.is_at_end() {
                        break;
                    }
                    if self.current_kind() == TokenKind::LBrace {
                        depth += 1;
                        if depth > 1 {
                            raw.push_str(self.lex());
                            raw.push(' ');
                        }
                        self.advance();
                        continue;
                    }
                    if self.current_kind() == TokenKind::RBrace {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            self.advance();
                            break;
                        }
                        raw.push_str(self.lex());
                        raw.push(' ');
                        self.advance();
                        continue;
                    }
                    raw.push_str(self.lex());
                    raw.push(' ');
                    self.advance();
                }
                args.push(AnnotationArg {
                    name: Some("__block".to_string()),
                    value: Expr::String_(StringLiteral {
                        parts: vec![StringPart::Text(raw)],
                        span: Span::dummy(),
                    }),
                    span: Span::dummy(),
                });
            }
            annots.push(Annotation {
                name: name.name,
                arguments: args,
                span: name.span,
            });
            self.skip_newlines();
        }
        annots
    }

    fn parse_import(&mut self) -> Result<ImportStmt> {
        self.expect_kw(TokenKind::Import)?;
        self.expect(TokenKind::Comma)?;
        let module = self.expect_str()?;
        let mut alias = None;
        let mut symbols = None;
        if self.current_kind() == TokenKind::Comma {
            self.advance();
            if self.current_kind() == TokenKind::As {
                self.advance();
                // Optional comma between `as` and the alias identifier
                // (Vredrs's comma-delimited style).
                self.eat_comma();
                alias = Some(self.parse_id()?);
            } else {
                let mut s = vec![self.parse_id()?];
                while self.current_kind() == TokenKind::Comma {
                    self.advance();
                    s.push(self.parse_id()?);
                }
                symbols = Some(s);
            }
        }
        self.nl();
        Ok(ImportStmt {
            module,
            alias,
            symbols,
            span: Span::dummy(),
        })
    }
    fn parse_export(&mut self) -> Result<ExportStmt> {
        self.expect_kw(TokenKind::Export)?;
        self.expect(TokenKind::Comma)?;
        let mut s = vec![self.parse_id()?];
        while self.current_kind() == TokenKind::Comma {
            self.advance();
            s.push(self.parse_id()?);
        }
        self.nl();
        Ok(ExportStmt {
            symbols: s,
            span: Span::dummy(),
        })
    }

    fn parse_extern_fn_def(&mut self) -> Result<TopLevel> {
        self.expect_kw(TokenKind::Extern)?;
        self.eat_comma();
        let link = if self.current_kind() == TokenKind::Identifier {
            let id = self.parse_id()?;
            self.eat_comma();
            Some(id.name)
        } else {
            Some("cstar".to_string())
        };
        // Parse just the function signature (name, params, optional return
        // type) — extern declarations have no body. We parse the signature
        // manually instead of calling parse_fn_def (which would greedily
        // consume the next statement as the body).
        self.expect_kw(TokenKind::Fn)?;
        self.expect(TokenKind::Comma)?;
        let name = self.parse_id()?;
        let params = self.parse_fn_params()?;
        let ret = if self.current_kind() == TokenKind::Colon {
            self.advance();
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        // Consume any trailing /end (optional for extern declarations).
        self.skip_newlines();
        if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
            self.advance();
        }
        let fn_def = FnDef {
            annotations: vec![],
            name,
            params,
            return_type: ret,
            body: vec![], // extern functions have no body
            is_constexpr: false,
            is_lazy: false,
            is_async: false,
            is_extern: true,
            extern_link: link.clone(),
            type_constraints: std::collections::HashMap::new(),
            span: Span::dummy(),
        };
        Ok(TopLevel::ExternFnDef(ExternFnDef {
            link,
            fn_def,
            span: Span::dummy(),
        }))
    }

    fn parse_fn_def(&mut self) -> Result<FnDef> {
        let is_async = self.current_kind() == TokenKind::Async;
        if is_async {
            self.advance();
        }
        self.expect_kw(TokenKind::Fn)?;
        self.expect(TokenKind::Comma)?;
        let name = self.parse_id()?;
        let params = self.parse_fn_params()?;
        let ret = if self.current_kind() == TokenKind::Colon {
            self.advance();
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        self.skip_newlines();
        let body = if self.current_kind() == TokenKind::Semicolon {
            self.advance();
            self.parse_until_nl()?
        } else if self.current_kind() == TokenKind::Indent {
            self.parse_block()?
        } else if self.current_kind() == TokenKind::LBrace {
            self.parse_block()?
        } else if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
            vec![]
        } else if self.current_kind() == TokenKind::Dedent {
            vec![]
        } else {
            vec![self.parse_stmt()?]
        };
        // Always consume trailing /end if present
        self.skip_newlines();
        if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
            self.advance();
        }
        Ok(FnDef {
            annotations: vec![],
            name,
            params,
            return_type: ret,
            body,
            is_constexpr: false,
            is_lazy: false,
            is_async,
            is_extern: false,
            extern_link: None,
            type_constraints: std::mem::take(&mut self.pending_type_constraints),
            span: Span::dummy(),
        })
    }
    fn parse_fn_params(&mut self) -> Result<Vec<FnParam>> {
        // Handle optional [TypeParams] before parens (generic fn).
        // Each type param can have constraints: `[T: Drawable, U: Comparable]`.
        // We collect the constraints into a side-channel that the caller
        // (parse_fn_def) reads via `pending_type_constraints`.
        self.pending_type_constraints.clear();
        if self.current_kind() == TokenKind::LBracket {
            self.advance();
            while self.current_kind() != TokenKind::RBracket && !self.is_at_end() {
                // Parse a type-parameter name.
                if self.current_kind() == TokenKind::Identifier {
                    let tp_name = self.parse_id()?;
                    let mut constraints: Vec<String> = Vec::new();
                    if self.current_kind() == TokenKind::Colon {
                        self.advance();
                        // Parse one or more constraint names separated by `+`.
                        loop {
                            if self.current_kind() == TokenKind::Identifier {
                                let c = self.parse_id()?;
                                constraints.push(c.name);
                            }
                            if self.current_kind() == TokenKind::Plus {
                                self.advance();
                                continue;
                            }
                            break;
                        }
                    }
                    self.pending_type_constraints.insert(tp_name.name, constraints);
                }
                if self.current_kind() == TokenKind::Comma {
                    self.advance();
                    continue;
                }
                break;
            }
            self.expect(TokenKind::RBracket)?;
        }
        self.expect(TokenKind::LParen)?;
        let mut v = vec![];
        if self.current_kind() == TokenKind::RParen {
            self.advance();
            return Ok(v);
        }
        loop {
            self.skip_newlines();
            let mut va = false;
            if self.current_kind() == TokenKind::DotDotDot {
                self.advance();
                va = true;
            }
            if self.is_at_nl() || self.is_at_end() || self.current_kind() == TokenKind::RParen {
                break;
            }
            if self.current_kind() != TokenKind::Identifier
                && self.current_kind() != TokenKind::SelfKw
            {
                break;
            }
            let name = if self.current_kind() == TokenKind::SelfKw {
                let s = self.span();
                self.advance();
                Identifier {
                    name: "self".to_string(),
                    span: s,
                }
            } else {
                self.parse_id()?
            };
            if self.current_kind() == TokenKind::DotDotDot {
                self.advance();
                va = true;
            }
            let ta = if self.current_kind() == TokenKind::Colon {
                self.advance();
                Some(self.parse_type_expr()?)
            } else {
                None
            };
            let dv = if self.current_kind() == TokenKind::Assign {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            v.push(FnParam {
                name,
                type_annotation: ta,
                default_value: dv,
                is_variadic: va,
                span: Span::dummy(),
            });
            // Variadic parameter must be the last – stop here
            if va {
                break;
            }
            // Continue if comma (more params), otherwise break
            if self.current_kind() != TokenKind::Comma {
                break;
            }
            self.advance();
            // skip newlines between params
            self.skip_newlines();
        }
        self.expect(TokenKind::RParen)?;
        Ok(v)
    }

    fn parse_struct_def(&mut self) -> Result<StructDef> {
        self.expect_kw(TokenKind::Struct)?;
        self.expect(TokenKind::Comma)?;
        let name = self.parse_id()?;
        self.nl();
        let mut fields = vec![];
        if self.current_kind() == TokenKind::Indent {
            self.advance();
            loop {
                self.skip_newlines();
                if self.current_kind() == TokenKind::Dedent || self.is_at_end() {
                    break;
                }
                if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                    self.advance();
                    break;
                }
                let annotations = self.parse_annotations();
                if self.current_kind() == TokenKind::Identifier {
                    let fname = self.parse_id()?;
                    let mut ta = None;
                    if self.current_kind() == TokenKind::Comma {
                        self.advance();
                        // Next could be a type — always try to parse it
                        if !self.is_at_nl() && !self.is_at_end() {
                            ta = Some(self.parse_type_expr()?);
                        }
                    } else if self.current_kind() == TokenKind::Colon {
                        self.advance();
                        ta = Some(self.parse_type_expr()?);
                    }
                    self.nl();
                    fields.push(StructField {
                        annotations,
                        name: fname,
                        type_annotation: ta,
                        default_value: None,
                        visibility: Visibility::Private,
                        span: Span::dummy(),
                    });
                } else {
                    // skip unexpected
                    break;
                }
            }
            if self.current_kind() == TokenKind::Dedent {
                self.advance();
            }
            self.skip_newlines();
            if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                self.advance();
            }
        }
        Ok(StructDef {
            annotations: vec![],
            name,
            fields,
            methods: vec![],
            span: Span::dummy(),
        })
    }
    fn parse_class_def(&mut self) -> Result<ClassDef> {
        self.expect_kw(TokenKind::Class)?;
        self.expect(TokenKind::Comma)?;
        let name = self.parse_id()?;
        let mut ext = None;
        let mut imp = vec![];
        while self.current_kind() == TokenKind::Comma {
            self.advance();
            if self.current_kind() == TokenKind::Extends {
                self.advance();
                self.eat_comma();
                ext = Some(self.parse_type_expr()?);
            } else if self.current_kind() == TokenKind::Implements {
                self.advance();
                self.eat_comma();
                imp.push(self.parse_type_expr()?);
            } else {
                break;
            }
        }
        self.nl();
        let mut fields = vec![];
        let mut methods = vec![];
        if self.current_kind() == TokenKind::Indent {
            self.advance();
            loop {
                self.skip_newlines();
                if self.current_kind() == TokenKind::Dedent || self.is_at_end() {
                    break;
                }
                if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                    self.advance();
                    break;
                }
                let annotations = self.parse_annotations();
                let mut vis = Visibility::Private;
                if self.current_kind() == TokenKind::Private {
                    self.advance();
                    vis = Visibility::Private;
                    if self.current_kind() == TokenKind::Comma {
                        self.advance();
                    }
                } else if self.current_kind() == TokenKind::Public {
                    self.advance();
                    vis = Visibility::Public;
                    if self.current_kind() == TokenKind::Comma {
                        self.advance();
                    }
                }
                if self.current_kind() == TokenKind::Fn || self.current_kind() == TokenKind::Async {
                    let mut fd = self.parse_fn_def()?;
                    fd.annotations = annotations;
                    methods.push(fd);
                } else if self.current_kind() == TokenKind::Identifier {
                    let id = self.parse_id()?;
                    self.nl();
                    fields.push(ClassField {
                        annotations,
                        name: id,
                        type_annotation: None,
                        default_value: None,
                        visibility: vis,
                        span: Span::dummy(),
                    });
                } else {
                    break; // unexpected token, stop loop
                }
            }
            if self.current_kind() == TokenKind::Dedent {
                self.advance();
            }
            self.skip_newlines();
            if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                self.advance();
            }
        }
        Ok(ClassDef {
            annotations: vec![],
            name,
            extends: ext,
            implements: imp,
            fields,
            methods,
            span: Span::dummy(),
        })
    }
    fn parse_interface_def(&mut self) -> Result<InterfaceDef> {
        self.expect_kw(TokenKind::Interface)?;
        self.expect(TokenKind::Comma)?;
        let name = self.parse_id()?;
        let mut ext = vec![];
        while self.current_kind() == TokenKind::Comma {
            self.advance();
            if self.current_kind() == TokenKind::Extends {
                self.advance();
                ext.push(self.parse_type_expr()?);
                continue;
            }
            break;
        }
        self.nl();
        let mut methods = vec![];
        if self.current_kind() == TokenKind::Indent {
            self.advance();
            loop {
                self.skip_newlines();
                if self.current_kind() == TokenKind::Dedent || self.is_at_end() {
                    break;
                }
                if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                    self.advance();
                    break;
                }
                if self.current_kind() == TokenKind::Fn {
                    let fn_def = self.parse_fn_def()?;
                    let im = InterfaceMethod {
                        name: fn_def.name.clone(),
                        params: fn_def.params.clone(),
                        return_type: fn_def.return_type.clone(),
                        default_body: Some(fn_def.body.clone()),
                        span: fn_def.span.clone(),
                    };
                    methods.push(im);
                } else {
                    break;
                }
            }
            if self.current_kind() == TokenKind::Dedent {
                self.advance();
            }
            self.skip_newlines();
            if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                self.advance();
            }
        }
        Ok(InterfaceDef {
            annotations: vec![],
            name,
            extends: ext,
            methods,
            span: Span::dummy(),
        })
    }
    fn parse_enum_def(&mut self) -> Result<EnumDef> {
        self.expect_kw(TokenKind::Enum)?;
        self.expect(TokenKind::Comma)?;
        let name = self.parse_id()?;
        self.nl();
        let mut vars = vec![];
        if self.current_kind() == TokenKind::Indent {
            self.advance();
            loop {
                self.skip_newlines();
                if self.current_kind() == TokenKind::Dedent || self.is_at_end() {
                    break;
                }
                if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                    break;
                }
                if self.current_kind() != TokenKind::Identifier {
                    break;
                }
                let vn = self.parse_id()?;
                let p = if self.current_kind() == TokenKind::LParen {
                    self.advance();
                    let mut payload_types = vec![];
                    while self.current_kind() != TokenKind::RParen && !self.is_at_end() {
                        payload_types.push(self.parse_type_expr()?);
                        if self.current_kind() == TokenKind::Comma {
                            self.advance();
                        }
                    }
                    self.expect(TokenKind::RParen)?;
                    if !payload_types.is_empty() {
                        Some(EnumVariantPayload::Tuple(payload_types))
                    } else {
                        None
                    }
                } else {
                    None
                };
                vars.push(EnumVariant {
                    name: vn,
                    payload: p,
                    span: Span::dummy(),
                });
                self.nl();
            }
            if self.current_kind() == TokenKind::Dedent {
                self.advance();
            }
        }
        self.skip_newlines();
        if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
            self.advance();
        }
        Ok(EnumDef {
            annotations: vec![],
            name,
            variants: vars,
            span: Span::dummy(),
        })
    }
    fn parse_type_alias(&mut self) -> Result<TypeAlias> {
        self.expect_kw(TokenKind::Type)?;
        self.expect(TokenKind::Comma)?;
        let name = self.parse_id()?;
        self.expect(TokenKind::Assign)?;
        let t = if self.current_kind() == TokenKind::Struct
            || self.current_kind() == TokenKind::Identifier
        {
            self.parse_type_expr()?
        } else {
            // fallback: parse any expression and wrap as basic type
            let _ = self.parse_expr()?;
            TypeExpr::Basic(BasicType::Any, Span::dummy())
        };
        self.nl();
        Ok(TypeAlias {
            name,
            target: t,
            span: Span::dummy(),
        })
    }
    fn parse_constexpr(&mut self) -> Result<ConstExpr> {
        self.expect_kw(TokenKind::Constexpr)?;
        self.expect(TokenKind::Comma)?;
        let name = self.parse_id()?;
        self.expect(TokenKind::Assign)?;
        let val = self.parse_expr()?;
        self.nl();
        Ok(ConstExpr {
            name,
            value: val,
            type_annotation: None,
            span: Span::dummy(),
        })
    }
    fn parse_lazy(&mut self) -> Result<TopLevel> {
        self.advance(); // eat lazy
        self.eat_comma(); // optional comma
        match self.current_kind() {
            TokenKind::Set => {
                self.advance();
                self.eat_comma();
                let name = self.parse_id()?;
                self.expect(TokenKind::Assign)?;
                let val = self.parse_expr()?;
                self.nl();
                Ok(TopLevel::LazyDef(LazyDef {
                    name,
                    value: val,
                    type_annotation: None,
                    span: Span::dummy(),
                }))
            }
            TokenKind::Fn => {
                let mut f = self.parse_fn_def()?;
                f.is_lazy = true;
                Ok(TopLevel::LazyFnDef(LazyFnDef {
                    span: Span::dummy(),
                    fn_def: f,
                }))
            }
            _ => Err(self.err("Expected set or fn after lazy")),
        }
    }
    fn parse_macro_def(&mut self) -> Result<MacroDef> {
        self.expect_kw(TokenKind::Macro)?;
        self.expect(TokenKind::Comma)?;
        let name = self.parse_id()?;
        let params = self.parse_fn_params()?;
        let body = self.parse_block()?;
        Ok(MacroDef {
            name,
            params,
            body,
            span: Span::dummy(),
        })
    }
    fn parse_plugin_def(&mut self) -> Result<PluginDef> {
        self.expect_kw(TokenKind::Plugin)?;
        self.expect(TokenKind::Comma)?;
        let name = self.expect_str()?;
        let body = self.parse_block()?;
        Ok(PluginDef {
            name,
            body,
            span: Span::dummy(),
        })
    }

    /// Parse a `test, "name" ... /end` block.
    /// The name must be a string literal; the body is a normal statement
    /// block. Asserts inside the body become pass/fail checks.
    fn parse_test_block(&mut self) -> Result<crate::parser::ast::TestBlock> {
        self.expect_kw(TokenKind::Test)?;
        self.expect(TokenKind::Comma)?;
        let name = self.expect_str()?;
        self.skip_newlines();
        let body = self.parse_block()?;
        Ok(crate::parser::ast::TestBlock {
            name,
            body,
            span: Span::dummy(),
        })
    }

    /// Parse a `bench, "name" [iterations] ... /end` block.
    /// The name must be a string literal; an optional integer literal after
    /// the name (preceded by a comma) sets the iteration count (default 1).
    fn parse_bench_block(&mut self) -> Result<crate::parser::ast::BenchBlock> {
        self.expect_kw(TokenKind::Bench)?;
        self.expect(TokenKind::Comma)?;
        let name = self.expect_str()?;
        // Optional iteration count: `bench, "name", 1000`.
        let iterations = if self.current_kind() == TokenKind::Comma {
            self.advance();
            if self.current_kind() == TokenKind::IntegerLiteral {
                let t = self.adv_tok();
                t.lexeme.parse::<i64>().unwrap_or(1)
            } else {
                1
            }
        } else {
            1
        };
        self.skip_newlines();
        let body = self.parse_block()?;
        Ok(crate::parser::ast::BenchBlock {
            name,
            iterations,
            body,
            span: Span::dummy(),
        })
    }
    fn parse_marker_trait(&mut self) -> Result<MarkerTrait> {
        self.expect_kw(TokenKind::Marker)?;
        self.expect_kw(TokenKind::Trait)?;
        let name = self.parse_id()?;
        self.nl();
        Ok(MarkerTrait {
            name,
            span: Span::dummy(),
        })
    }
    fn parse_trait_def(&mut self) -> Result<crate::parser::ast::TraitDef> {
        self.expect_kw(TokenKind::Trait)?;
        self.expect(TokenKind::Comma)?;
        let name = self.parse_id()?;
        let mut extends = vec![];
        if self.current_kind() == TokenKind::Extends || self.lex() == "extends" {
            self.advance(); self.eat_comma();
            loop { extends.push(self.parse_type_expr()?); if self.current_kind() == TokenKind::Comma { self.advance(); continue; } break; }
        }
        self.nl();
        let mut methods = vec![];
        if self.current_kind() == TokenKind::Indent {
            self.advance();
            loop {
                self.skip_newlines();
                if self.current_kind() == TokenKind::Dedent || self.is_at_end() { break; }
                if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" { self.advance(); break; }
                if self.current_kind() == TokenKind::Fn || self.current_kind() == TokenKind::Async {
                    let fd = self.parse_fn_def()?;
                    methods.push(crate::parser::ast::TraitMethod { name: fd.name.clone(), params: fd.params.clone(), return_type: fd.return_type.clone(), default_body: if !fd.body.is_empty() { Some(fd.body.clone()) } else { None }, span: Span::dummy() });
                } else { break; }
            }
            if self.current_kind() == TokenKind::Dedent { self.advance(); }
        }
        self.skip_newlines();
        if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" { self.advance(); }
        Ok(crate::parser::ast::TraitDef { annotations: vec![], name, extends, methods, span: Span::dummy() })
    }

    fn parse_impl_block(&mut self) -> Result<crate::parser::ast::ImplBlock> {
        self.expect_kw(TokenKind::Impl)?;
        self.expect(TokenKind::Comma)?;
        let trait_name = self.parse_id()?;
        let target_type = if self.lex() == "for" { self.advance(); self.eat_comma(); self.parse_type_expr()? } else { crate::parser::ast::TypeExpr::Named(trait_name.clone(), Span::dummy()) };
        self.nl();
        let mut methods = vec![];
        if self.current_kind() == TokenKind::Indent {
            self.advance();
            loop {
                self.skip_newlines();
                if self.current_kind() == TokenKind::Dedent || self.is_at_end() { break; }
                if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" { self.advance(); break; }
                if self.current_kind() == TokenKind::Fn || self.current_kind() == TokenKind::Async { methods.push(self.parse_fn_def()?); } else { break; }
            }
            if self.current_kind() == TokenKind::Dedent { self.advance(); }
        }
        self.skip_newlines();
        if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" { self.advance(); }
        Ok(crate::parser::ast::ImplBlock { trait_name, target_type, methods, span: Span::dummy() })
    }

    fn parse_dtor_block(&mut self) -> Result<crate::parser::ast::DtorBlock> {
        self.expect_kw(TokenKind::Dtor)?;
        self.expect(TokenKind::Comma)?;
        let type_name = self.parse_id()?;
        self.nl();
        let body = self.parse_block()?;
        Ok(crate::parser::ast::DtorBlock { type_name, body, span: Span::dummy() })
    }

    fn parse_unsafe(&mut self) -> Result<UnsafeBlock> {
        if self.current_kind() == TokenKind::Unsafe {
            self.advance();
            self.eat_comma();
        }
        let body = self.parse_block()?;
        Ok(UnsafeBlock {
            body,
            span: Span::dummy(),
        })
    }

    fn parse_stmt(&mut self) -> Result<Stmt> {
        self.skip_newlines();
        if self.is_at_end() {
            return Err(self.err("EOF"));
        }
        if matches!(self.current_kind(), TokenKind::Dedent | TokenKind::Indent) {
            return Err(self.err("Unexpected indent/dedent"));
        }
        match self.current_kind() {
            TokenKind::Set => {
                self.advance();
                self.parse_assign()
            }
            TokenKind::Paste => {
                self.advance();
                if self.current_kind() == TokenKind::LParen {
                    self.advance();
                    let a = self.parse_args()?;
                    self.nl();
                    Ok(Stmt::Paste(PasteStmt {
                        args: a,
                        span: Span::dummy(),
                    }))
                } else {
                    self.expect(TokenKind::Comma)?;
                    let a = self.parse_exprs_semi()?;
                    Ok(Stmt::Paste(PasteStmt {
                        args: a,
                        span: Span::dummy(),
                    }))
                }
            }
            TokenKind::Println => {
                self.advance();
                self.eat_comma(); // optional comma
                if self.is_at_nl() || self.is_at_end() {
                    return Ok(Stmt::Println(PrintlnStmt {
                        args: vec![],
                        span: Span::dummy(),
                    }));
                }
                let a = self.parse_exprs_semi()?;
                Ok(Stmt::Println(PrintlnStmt {
                    args: a,
                    span: Span::dummy(),
                }))
            }
            TokenKind::Input => {
                let saved = self.pos;
                self.advance();
                if self.current_kind() == TokenKind::Comma {
                    self.advance();
                    // Next must be identifier or string (variable)
                    if self.current_kind() == TokenKind::Identifier
                        || self.current_kind() == TokenKind::StringLiteral
                    {
                        let t = self.parse_assignee()?;
                        let p = if self.current_kind() == TokenKind::Comma {
                            self.advance();
                            Some(self.expect_str()?)
                        } else {
                            None
                        };
                        self.nl();
                        return Ok(Stmt::Input(InputStmt {
                            target: t,
                            prompt: p,
                            span: Span::dummy(),
                        }));
                    }
                }
                // Treat as expression variable
                self.pos = saved; // rewind to 'input'
                let e = self.parse_expr()?;
                self.nl();
                Ok(Stmt::Expr(ExprStmt {
                    expr: e,
                    span: Span::dummy(),
                }))
            }
            TokenKind::Send => {
                self.advance();
                if self.current_kind() == TokenKind::LParen {
                    self.advance();
                    let ch = self.parse_expr()?;
                    self.expect(TokenKind::Comma)?;
                    let val = self.parse_expr()?;
                    self.expect(TokenKind::RParen)?;
                    Ok(Stmt::Expr(ExprStmt {
                        expr: Expr::Call(CallExpr {
                            callee: Box::new(Expr::Identifier(Identifier {
                                name: "send".into(),
                                span: Span::dummy(),
                            })),
                            args: vec![ch, val],
                            span: Span::dummy(),
                        }),
                        span: Span::dummy(),
                    }))
                } else {
                    self.expect(TokenKind::Comma)?;
                    let ch = self.parse_expr()?;
                    self.expect(TokenKind::Comma)?;
                    let val = self.parse_expr()?;
                    Ok(Stmt::Expr(ExprStmt {
                        expr: Expr::Call(CallExpr {
                            callee: Box::new(Expr::Identifier(Identifier {
                                name: "send".into(),
                                span: Span::dummy(),
                            })),
                            args: vec![ch, val],
                            span: Span::dummy(),
                        }),
                        span: Span::dummy(),
                    }))
                }
            }
            TokenKind::Receive => {
                self.advance();
                if self.current_kind() == TokenKind::LParen {
                    self.advance();
                    let ch = self.parse_expr()?;
                    self.expect(TokenKind::RParen)?;
                    Ok(Stmt::Expr(ExprStmt {
                        expr: Expr::Call(CallExpr {
                            callee: Box::new(Expr::Identifier(Identifier {
                                name: "receive".into(),
                                span: Span::dummy(),
                            })),
                            args: vec![ch],
                            span: Span::dummy(),
                        }),
                        span: Span::dummy(),
                    }))
                } else {
                    self.expect(TokenKind::Comma)?;
                    let ch = self.parse_expr()?;
                    Ok(Stmt::Expr(ExprStmt {
                        expr: Expr::Call(CallExpr {
                            callee: Box::new(Expr::Identifier(Identifier {
                                name: "receive".into(),
                                span: Span::dummy(),
                            })),
                            args: vec![ch],
                            span: Span::dummy(),
                        }),
                        span: Span::dummy(),
                    }))
                }
            }
            TokenKind::Return => {
                self.advance();
                self.eat_comma();
                let mut values = Vec::new();
                if !self.is_at_nl()
                    && !self.is_at_end()
                    && !matches!(
                        self.current_kind(),
                        TokenKind::Dedent | TokenKind::DirectiveOrScope
                    )
                {
                    values.push(self.parse_expr()?);
                    while self.current_kind() == TokenKind::Comma {
                        self.advance();
                        if self.is_at_nl() || self.is_at_end() {
                            break;
                        }
                        values.push(self.parse_expr()?);
                    }
                }
                self.nl();
                Ok(Stmt::Return(ReturnStmt {
                    values,
                    span: Span::dummy(),
                }))
            }
            TokenKind::Throw => {
                self.advance();
                self.expect(TokenKind::Comma)?;
                let v = self.parse_expr()?;
                self.nl();
                Ok(Stmt::Throw(ThrowStmt {
                    value: v,
                    span: Span::dummy(),
                }))
            }
            TokenKind::Defer => {
                self.advance();
                self.expect(TokenKind::Comma)?;
                let s = self.parse_stmt()?;
                Ok(Stmt::Defer(DeferStmt {
                    stmt: Box::new(s),
                    span: Span::dummy(),
                }))
            }
            TokenKind::Assert => {
                self.advance();
                self.expect(TokenKind::Comma)?;
                let c = self.parse_expr()?;
                let m = if self.current_kind() == TokenKind::Comma {
                    self.advance();
                    Some(self.expect_str()?)
                } else {
                    None
                };
                self.nl();
                Ok(Stmt::Assert(AssertStmt {
                    condition: c,
                    message: m,
                    span: Span::dummy(),
                }))
            }
            TokenKind::Spawn => {
                self.advance();
                self.eat_comma();
                // If next is 'fn', parse as lambda with statement body
                if self.current_kind() == TokenKind::Fn {
                    let _fn_pos = self.pos;
                    self.advance();
                    let p = self.parse_fn_params()?;
                    // Try to parse as expression first (backtrack if fails)
                    let saved = self.pos;
                    match self.parse_expr() {
                        Ok(body_expr) => {
                            let call = Expr::Lambda(LambdaExpr {
                                params: p,
                                body: Box::new(body_expr),
                                return_type: None,
                                span: Span::dummy(),
                            });
                            self.nl();
                            return Ok(Stmt::Spawn(SpawnStmt {
                                call,
                                span: Span::dummy(),
                            }));
                        }
                        Err(_) => {
                            // Restore to after params, try parsing statement as body
                            self.pos = saved;
                            let stmt = self.parse_stmt()?;
                            let body_expr = match &stmt {
                                Stmt::Expr(es) => es.expr.clone(),
                                Stmt::Println(ps) => Expr::Call(CallExpr {
                                    callee: Box::new(Expr::Identifier(Identifier {
                                        name: "println".to_string(),
                                        span: Span::dummy(),
                                    })),
                                    args: ps.args.clone(),
                                    span: Span::dummy(),
                                }),
                                Stmt::Paste(ps) => Expr::Call(CallExpr {
                                    callee: Box::new(Expr::Identifier(Identifier {
                                        name: "paste".to_string(),
                                        span: Span::dummy(),
                                    })),
                                    args: ps.args.clone(),
                                    span: Span::dummy(),
                                }),
                                _ => Expr::Null(NullLiteral {
                                    span: Span::dummy(),
                                }),
                            };
                            let call = Expr::Lambda(LambdaExpr {
                                params: p,
                                body: Box::new(body_expr),
                                return_type: None,
                                span: Span::dummy(),
                            });
                            self.nl();
                            return Ok(Stmt::Spawn(SpawnStmt {
                                call,
                                span: Span::dummy(),
                            }));
                        }
                    }
                } else {
                    let c = self.parse_expr()?;
                    self.nl();
                    Ok(Stmt::Spawn(SpawnStmt {
                        call: c,
                        span: Span::dummy(),
                    }))
                }
            }
            TokenKind::Yield => {
                self.advance();
                let v = if self.eat_comma() {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                self.nl();
                Ok(Stmt::Yield(YieldStmt {
                    value: v,
                    span: Span::dummy(),
                }))
            }
            TokenKind::Break => {
                self.advance();
                self.skip_newlines();
                let l = if self.current_kind() == TokenKind::Identifier {
                    let lbl = self.parse_id()?;
                    Some(lbl)
                } else {
                    None
                };
                self.nl();
                Ok(Stmt::Break(BreakStmt {
                    label: l,
                    span: Span::dummy(),
                }))
            }
            TokenKind::Continue => {
                self.advance();
                self.skip_newlines();
                let l = if self.current_kind() == TokenKind::Identifier {
                    let lbl = self.parse_id()?;
                    Some(lbl)
                } else {
                    None
                };
                self.nl();
                Ok(Stmt::Continue(ContinueStmt {
                    label: l,
                    span: Span::dummy(),
                }))
            }
            TokenKind::If => {
                self.advance();
                self.expect(TokenKind::Comma)?;
                self.parse_if()
            }
            TokenKind::While => {
                self.advance();
                self.expect(TokenKind::Comma)?;
                let c = self.parse_expr()?;
                let b = self.parse_block()?;
                Ok(Stmt::While(WhileStmt {
                    label: None,
                    condition: c,
                    body: b,
                    else_body: None,
                    span: Span::dummy(),
                }))
            }
            TokenKind::For => {
                self.advance();
                self.expect(TokenKind::Comma)?;
                self.parse_for()
            }
            TokenKind::Loop => {
                self.advance();
                let b = self.parse_block()?;
                Ok(Stmt::Loop(LoopStmt {
                    label: None,
                    body: b,
                    else_body: None,
                    span: Span::dummy(),
                }))
            }
            TokenKind::Match => {
                self.advance();
                self.expect(TokenKind::Comma)?;
                self.parse_match()
            }
            TokenKind::Try => {
                self.advance();
                self.parse_try()
            }
            TokenKind::With => {
                self.advance();
                self.parse_with()
            }
            TokenKind::Select => {
                self.advance();
                self.parse_select()
            }
            TokenKind::Pon => {
                self.advance();
                self.expect(TokenKind::Semicolon)?;
                let s = self.parse_stmt()?;
                if let Stmt::Assign(a) = s {
                    Ok(Stmt::Pon(PonStmt {
                        assign: a,
                        span: Span::dummy(),
                    }))
                } else {
                    Ok(Stmt::Pon(PonStmt {
                        assign: AssignStmt {
                            targets: vec![],
                            value: Expr::Null(NullLiteral {
                                span: Span::dummy(),
                            }),
                            operator: AssignOp::Simple,
                            span: Span::dummy(),
                        },
                        span: Span::dummy(),
                    }))
                }
            }
            TokenKind::DirectiveOrScope => self.parse_directive(),
            TokenKind::Identifier => {
                let id = self.parse_id()?;
                // flush, panic, spawn_thread special statements
                if id.name == "flush" {
                    self.nl();
                    return Ok(Stmt::Flush(FlushStmt { span: id.span }));
                }
                if id.name == "asm" {
                    self.eat_comma();
                    let template = if self.current_kind() == TokenKind::StringLiteral {
                        self.expect_str()?
                    } else {
                        id.name.clone()
                    };
                    self.nl();
                    return Ok(Stmt::Asm(AsmStmt {
                        template,
                        inputs: vec![],
                        outputs: vec![],
                        span: id.span,
                    }));
                }
                if id.name == "panic" {
                    let m = if self.eat_comma() {
                        self.parse_expr()?
                    } else {
                        Expr::Null(NullLiteral {
                            span: Span::dummy(),
                        })
                    };
                    self.nl();
                    return Ok(Stmt::Panic(PanicStmt {
                        message: m,
                        span: id.span,
                    }));
                }
                if id.name == "spawn_thread" {
                    let call = if self.eat_comma() {
                        self.parse_expr()?
                    } else {
                        Expr::Null(NullLiteral {
                            span: Span::dummy(),
                        })
                    };
                    self.nl();
                    return Ok(Stmt::SpawnThread(SpawnThreadStmt {
                        call,
                        span: id.span,
                    }));
                }
                if id.name == "del" || id.name == "delete" {
                    self.eat_comma();
                    let target = self.parse_assignee()?;
                    self.nl();
                    return Ok(Stmt::Assign(AssignStmt {
                        targets: vec![target],
                        value: Expr::Null(NullLiteral {
                            span: Span::dummy(),
                        }),
                        operator: AssignOp::Delete,
                        span: Span::dummy(),
                    }));
                }
                // Check for inline TableAssign: identifier; identifier; ... /= value; value; ...
                if self.current_kind() == TokenKind::Semicolon {
                    let saved_pos = self.pos;
                    let mut col_names = vec![id.clone()];
                    while self.current_kind() == TokenKind::Semicolon {
                        self.advance();
                        self.skip_newlines();
                        if self.current_kind() == TokenKind::Identifier {
                            col_names.push(self.parse_id()?);
                        } else {
                            self.pos = saved_pos;
                            break;
                        }
                    }
                    if col_names.len() >= 1 && self.current_kind() == TokenKind::TableAssign {
                        self.advance(); // eat /=
                        self.skip_newlines();
                        let mut row_data = vec![];
                        while !self.is_at_nl() && !self.is_at_end() {
                            row_data.push(self.parse_expr()?);
                            if self.current_kind() == TokenKind::Semicolon {
                                self.advance();
                            } else {
                                break;
                            }
                        }
                        self.nl();
                        return Ok(Stmt::TableAssign(TableAssignStmt {
                            column_names: col_names,
                            rows: vec![row_data],
                            span: Span::dummy(),
                        }));
                    }
                } // In /set fill-block, identifier followed by comma means assignment: name, value
                  // EXCEPT: send/receive are builtin function calls with comma-separated args
                if id.name == "send" || id.name == "receive" {
                    let mut args = vec![];
                    while self.current_kind() == TokenKind::Comma {
                        self.advance();
                        self.skip_newlines();
                        args.push(self.parse_expr()?);
                    }
                    self.nl();
                    return Ok(Stmt::Expr(ExprStmt {
                        expr: Expr::Call(CallExpr {
                            callee: Box::new(Expr::Identifier(id)),
                            args,
                            span: Span::dummy(),
                        }),
                        span: Span::dummy(),
                    }));
                }
                if self.current_kind() == TokenKind::Comma {
                    self.advance(); // eat comma
                    if self.is_at_nl() || self.is_at_end() {
                        self.nl();
                        return Ok(Stmt::Assign(AssignStmt {
                            targets: vec![Assignee::Identifier(id)],
                            value: Expr::Null(NullLiteral {
                                span: Span::dummy(),
                            }),
                            operator: AssignOp::Simple,
                            span: Span::dummy(),
                        }));
                    }
                    let v = self.parse_expr()?;
                    self.nl();
                    return Ok(Stmt::Assign(AssignStmt {
                        targets: vec![Assignee::Identifier(id)],
                        value: v,
                        operator: AssignOp::Simple,
                        span: Span::dummy(),
                    }));
                }
                // Labeled loops: identifier starting with single quote (lexer gives Identifier for 'label)
                if id.name.starts_with("'")
                    && (self.current_kind() == TokenKind::Colon
                        || self.peek_kind() == Some(TokenKind::Colon))
                {
                    self.advance(); // eat colon
                    let label = Identifier {
                        name: id.name[1..].to_string(),
                        span: id.span.clone(),
                    };
                    self.skip_newlines();
                    match self.current_kind() {
                        TokenKind::For => {
                            self.advance();
                            self.expect(TokenKind::Comma)?;
                            let mut f = self.parse_for()?;
                            match &mut f {
                                Stmt::ForIn(fi) => fi.label = Some(label),
                                Stmt::ForRange(fr) => fr.label = Some(label),
                                _ => {}
                            }
                            return Ok(f);
                        }
                        TokenKind::While => {
                            self.advance();
                            self.expect(TokenKind::Comma)?;
                            let c = self.parse_expr()?;
                            let b = self.parse_block()?;
                            return Ok(Stmt::While(WhileStmt {
                                label: Some(label),
                                condition: c,
                                body: b,
                                else_body: None,
                                span: Span::dummy(),
                            }));
                        }
                        TokenKind::Loop => {
                            self.advance();
                            let b = self.parse_block()?;
                            return Ok(Stmt::Loop(LoopStmt {
                                label: Some(label),
                                body: b,
                                else_body: None,
                                span: Span::dummy(),
                            }));
                        }
                        _ => return Err(self.err("Expected for/while/loop after label")),
                    }
                }
                // Normal identifier expressions: check for assignment operators, function calls, member access
                if self.is_assign_token() {
                    let op = self.parse_assign_op_current();
                    self.advance();
                    let v = self.parse_expr()?;
                    self.nl();
                    return Ok(Stmt::Assign(AssignStmt {
                        targets: vec![Assignee::Identifier(id)],
                        value: v,
                        operator: op,
                        span: Span::dummy(),
                    }));
                }
                if self.current_kind() == TokenKind::LParen {
                    self.advance();
                    let mut expr = Expr::Call(CallExpr {
                        callee: Box::new(Expr::Identifier(id)),
                        args: self.parse_args()?,
                        span: Span::dummy(),
                    });
                    while self.current_kind() == TokenKind::Dot {
                        self.advance();
                        let member = self.parse_id()?;
                        if self.current_kind() == TokenKind::LParen {
                            self.advance();
                            expr = Expr::Call(CallExpr {
                                callee: Box::new(Expr::MemberAccess(MemberAccessExpr {
                                    target: Box::new(expr),
                                    member,
                                    span: Span::dummy(),
                                })),
                                args: self.parse_args()?,
                                span: Span::dummy(),
                            });
                        } else {
                            expr = Expr::MemberAccess(MemberAccessExpr {
                                target: Box::new(expr),
                                member,
                                span: Span::dummy(),
                            });
                        }
                    }
                    if self.current_kind() == TokenKind::As {
                        self.advance();
                        let ty = self.parse_type_expr()?;
                        expr = Expr::Cast(CastExpr {
                            expr: Box::new(expr),
                            type_expr: ty,
                            span: Span::dummy(),
                        });
                    }
                    self.nl();
                    return Ok(Stmt::Expr(ExprStmt {
                        expr,
                        span: Span::dummy(),
                    }));
                }
                if self.current_kind() == TokenKind::Dot
                    || self.current_kind() == TokenKind::LBracket
                    || self.current_kind() == TokenKind::QuestionDot
                {
                    let mut expr = Expr::Identifier(id);
                    loop {
                        match self.current_kind() {
                            TokenKind::Dot => {
                                self.advance();
                                let member = self.parse_id()?;
                                expr = Expr::MemberAccess(MemberAccessExpr {
                                    target: Box::new(expr),
                                    member,
                                    span: Span::dummy(),
                                });
                            }
                            TokenKind::LBracket => {
                                self.advance();
                                let mut start = None;
                                let mut end = None;
                                let mut step = None;
                                if self.current_kind() == TokenKind::Colon {
                                    self.advance();
                                    if self.current_kind() != TokenKind::Colon
                                        && self.current_kind() != TokenKind::RBracket
                                    {
                                        end = Some(Box::new(self.parse_expr()?));
                                    }
                                    if self.current_kind() == TokenKind::Colon {
                                        self.advance();
                                        if self.current_kind() != TokenKind::RBracket {
                                            step = Some(Box::new(self.parse_expr()?));
                                        }
                                    }
                                    self.expect(TokenKind::RBracket)?;
                                    expr = Expr::Slice(SliceExpr {
                                        target: Box::new(expr),
                                        start,
                                        end,
                                        step,
                                        span: Span::dummy(),
                                    });
                                } else {
                                    let first = self.parse_expr()?;
                                    if self.current_kind() == TokenKind::Colon {
                                        start = Some(Box::new(first));
                                        self.advance();
                                        if self.current_kind() != TokenKind::Colon
                                            && self.current_kind() != TokenKind::RBracket
                                        {
                                            end = Some(Box::new(self.parse_expr()?));
                                        }
                                        if self.current_kind() == TokenKind::Colon {
                                            self.advance();
                                            if self.current_kind() != TokenKind::RBracket {
                                                step = Some(Box::new(self.parse_expr()?));
                                            }
                                        }
                                        self.expect(TokenKind::RBracket)?;
                                        expr = Expr::Slice(SliceExpr {
                                            target: Box::new(expr),
                                            start,
                                            end,
                                            step,
                                            span: Span::dummy(),
                                        });
                                    } else {
                                        self.expect(TokenKind::RBracket)?;
                                        expr = Expr::Index(IndexExpr {
                                            target: Box::new(expr),
                                            index: Box::new(first),
                                            span: Span::dummy(),
                                        });
                                    }
                                }
                            }
                            TokenKind::QuestionDot => {
                                self.advance();
                                // Parse the link following `?.` and append
                                // it to the optional chain (wrapping `expr`
                                // in a fresh OptionalChain when needed).
                                let link = self.parse_optional_chain_link()?;
                                expr = match expr {
                                    Expr::OptionalChain(mut oc) => {
                                        oc.chain.push(link);
                                        Expr::OptionalChain(oc)
                                    }
                                    other => Expr::OptionalChain(OptionalChainExpr {
                                        target: Box::new(other),
                                        chain: vec![link],
                                        span: Span::dummy(),
                                    }),
                                };
                            }
                            _ => break,
                        }
                        if self.current_kind() == TokenKind::LParen {
                            self.advance();
                            expr = Expr::Call(CallExpr {
                                callee: Box::new(expr),
                                args: self.parse_args()?,
                                span: Span::dummy(),
                            });
                        }
                    }
                    if self.is_assign_token() {
                        let op = self.parse_assign_op_current();
                        self.advance();
                        let v = self.parse_expr()?;
                        self.nl();
                        let target = self.expr_to_assignee(expr)?;
                        return Ok(Stmt::Assign(AssignStmt {
                            targets: vec![target],
                            value: v,
                            operator: op,
                            span: Span::dummy(),
                        }));
                    }
                    self.nl();
                    return Ok(Stmt::Expr(ExprStmt {
                        expr,
                        span: Span::dummy(),
                    }));
                }
                // Nothing special: just an identifier expression
                self.nl();
                Ok(Stmt::Expr(ExprStmt {
                    expr: Expr::Identifier(id),
                    span: Span::dummy(),
                }))
            }
            _ => {
                let e = self.parse_expr()?;
                if self.is_assign_token() {
                    let op = self.parse_assign_op_current();
                    self.advance();
                    let v = self.parse_expr()?;
                    self.nl();
                    let target = self.expr_to_assignee(e)?;
                    Ok(Stmt::Assign(AssignStmt {
                        targets: vec![target],
                        value: v,
                        operator: op,
                        span: Span::dummy(),
                    }))
                } else {
                    self.nl();
                    Ok(Stmt::Expr(ExprStmt {
                        expr: e,
                        span: Span::dummy(),
                    }))
                }
            }
        }
    }

    fn eat_comma(&mut self) -> bool {
        if self.current_kind() == TokenKind::Comma {
            self.advance();
            true
        } else {
            false
        }
    }

    fn parse_assign(&mut self) -> Result<Stmt> {
        // Handle /set followed by /directive or /end or /= (table assign)
        if self.current_kind() == TokenKind::DirectiveOrScope {
            return self.parse_directive();
        }
        if self.current_kind() == TokenKind::TableAssign {
            return self.parse_table_assign();
        }
        self.eat_comma(); // optional comma after set
        if self.current_kind() == TokenKind::DirectiveOrScope {
            return self.parse_directive();
        }
        if self.is_at_nl() || self.is_at_end() {
            return Ok(Stmt::Assign(AssignStmt {
                targets: vec![],
                value: Expr::Null(NullLiteral {
                    span: Span::dummy(),
                }),
                operator: AssignOp::Simple,
                span: Span::dummy(),
            }));
        }
        // Parse single target. Use parse_prec instead of parse_expr so
        // that a slice colon in the value expression (e.g. `foo[0:2]`)
        // isn't accidentally consumed by the assignee's LBracket handler.
        let target = self.parse_assignee()?;
        let mut targets = vec![target];
        // Check for multi-target (comma followed by identifier, then = or += etc.)
        // We use simple lookahead: comma, identifier, then = => multi-target
        if self.current_kind() == TokenKind::Comma {
            let saved = self.pos;
            let mut extra = vec![];
            let mut is_multi = false;
            loop {
                if self.current_kind() != TokenKind::Comma {
                    break;
                }
                let pk = self.peek_kind();
                if pk == Some(TokenKind::Identifier) || pk == Some(TokenKind::Underscore) {
                    self.advance(); // comma
                    extra.push(self.parse_assignee()?);
                    if matches!(
                        self.current_kind(),
                        TokenKind::Assign
                            | TokenKind::PlusAssign
                            | TokenKind::MinusAssign
                            | TokenKind::StarAssign
                            | TokenKind::SlashAssign
                            | TokenKind::PercentAssign
                    ) {
                        is_multi = true;
                        break;
                    }
                } else {
                    break;
                }
            }
            if is_multi {
                targets.extend(extra);
            } else {
                // Not multi-target; backtrack and treat comma as value separator
                self.pos = saved;
            }
        }
        // Parse assignment operator + value
        if matches!(
            self.current_kind(),
            TokenKind::Assign
                | TokenKind::PlusAssign
                | TokenKind::MinusAssign
                | TokenKind::StarAssign
                | TokenKind::SlashAssign
                | TokenKind::PercentAssign
        ) {
            let op = self.parse_assign_op_current();
            self.advance();
            // For multi-target assignment, parse the RHS as a tuple.
            let v = if targets.len() > 1 {
                let first = self.parse_expr()?;
                if self.current_kind() == TokenKind::Comma {
                    let mut elements = vec![first];
                    while self.current_kind() == TokenKind::Comma {
                        self.advance();
                        if self.is_at_nl() || self.is_at_end() {
                            break;
                        }
                        elements.push(self.parse_expr()?);
                    }
                    Expr::Tuple(TupleLiteral {
                        elements,
                        span: Span::dummy(),
                    })
                } else {
                    first
                }
            } else {
                self.parse_expr()?
            };
            self.nl();
            return Ok(Stmt::Assign(AssignStmt {
                targets,
                value: v,
                operator: op,
                span: Span::dummy(),
            }));
        } else {
            // comma before value
            if self.current_kind() == TokenKind::Comma {
                self.advance();
            }
            // Use a higher precedence floor so slice/index colon (which is
            // precedence 0 in `prec()`) doesn't terminate parsing.
            // The slice/index handlers live in parse_prec's LBracket arm,
            // so we need parse_prec(1) to keep going through the colon.
            // Actually the issue is that `prec(Colon) = 0`, and parse_prec(0)
            // stops when the next token's precedence is < 0 (never). But the
            // LBracket handler is in the postfix loop, which only runs if
            // `p >= min`. With min=0 and Colon's prec=0, the loop continues.
            // The real issue: parse_expr() calls parse_prec(0), and the
            // LBracket handler is reached. But after consuming `[`, it
            // calls parse_expr() recursively for the start — which sees
            // `0` then `:` (prec 0). With min=0, `0 >= 0` so it parses 0,
            // then sees `:` (prec 0 >= 0), tries to parse `:` as a binary
            // op... but Colon isn't in the binop match. So it breaks.
            // That should be fine. The problem must be elsewhere.
            // Let's just call parse_expr and see.
            let v = self.parse_expr()?;
            self.nl();
            return Ok(Stmt::Assign(AssignStmt {
                targets,
                value: v,
                operator: AssignOp::Simple,
                span: Span::dummy(),
            }));
        }
    }

    fn parse_table_assign(&mut self) -> Result<Stmt> {
        // Called when current token is TableAssign (/=) or after consuming /=
        self.skip_newlines();
        let mut cols = vec![];
        let mut rows = vec![];
        // Check for indented body
        let has_indent = self.current_kind() == TokenKind::Indent;
        if has_indent {
            self.advance();
            self.skip_newlines();
        }
        // Read column names
        if !self.is_at_nl()
            && !self.is_at_end()
            && self.current_kind() != TokenKind::Slash
            && self.current_kind() != TokenKind::Dedent
        {
            loop {
                if self.current_kind() == TokenKind::Identifier {
                    cols.push(self.parse_id()?);
                    if self.current_kind() == TokenKind::Semicolon {
                        self.advance();
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.skip_newlines();
        // Read data rows
        loop {
            if self.current_kind() == TokenKind::Dedent || self.is_at_end() {
                break;
            }
            if self.current_kind() == TokenKind::Slash {
                self.advance();
                break;
            }
            if self.current_kind() == TokenKind::DirectiveOrScope {
                if self.lex() == "/" || self.lex() == "/end" {
                    self.advance();
                    break;
                }
                break;
            }
            if self.is_at_nl() {
                self.skip_newlines();
                continue;
            }
            let mut row_data = vec![];
            while !self.is_at_nl() && !self.is_at_end() {
                row_data.push(self.parse_expr()?);
                if self.current_kind() == TokenKind::Semicolon {
                    self.advance();
                } else {
                    break;
                }
            }
            if !row_data.is_empty() {
                rows.push(row_data);
            }
            self.skip_newlines();
        }
        if self.current_kind() == TokenKind::Dedent {
            self.advance();
        }
        self.skip_newlines();
        // Consume trailing /
        if self.current_kind() == TokenKind::Slash {
            self.advance();
        } else if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/" {
            self.advance();
        }
        Ok(Stmt::TableAssign(TableAssignStmt {
            column_names: cols,
            rows,
            span: Span::dummy(),
        }))
    }
    fn is_assign_token(&self) -> bool {
        matches!(
            self.current_kind(),
            TokenKind::Assign
                | TokenKind::PlusAssign
                | TokenKind::MinusAssign
                | TokenKind::StarAssign
                | TokenKind::SlashAssign
                | TokenKind::PercentAssign
        )
    }

    fn parse_assign_op_current(&self) -> AssignOp {
        match self.current_kind() {
            TokenKind::PlusAssign => AssignOp::Plus,
            TokenKind::MinusAssign => AssignOp::Minus,
            TokenKind::StarAssign => AssignOp::Star,
            TokenKind::SlashAssign => AssignOp::Slash,
            TokenKind::PercentAssign => AssignOp::Percent,
            _ => AssignOp::Simple,
        }
    }

    fn expr_to_assignee(&self, expr: Expr) -> Result<Assignee> {
        match expr {
            Expr::Identifier(id) => Ok(Assignee::Identifier(id)),
            Expr::Qualified(q) => Ok(Assignee::Qualified(q)),
            Expr::Index(i) => Ok(Assignee::Index(i)),
            Expr::MemberAccess(m) => Ok(Assignee::Member(m)),
            Expr::Tuple(t) => {
                let mut items = Vec::with_capacity(t.elements.len());
                for e in t.elements {
                    items.push(self.expr_to_assignee(e)?);
                }
                Ok(Assignee::Tuple(items))
            }
            _ => Err(self.err("Expected assignable target")),
        }
    }

    fn parse_assignee(&mut self) -> Result<Assignee> {
        let id = match self.current_kind() {
            TokenKind::Identifier => self.parse_id()?,
            TokenKind::SelfKw => {
                let span = self.span();
                self.advance();
                Identifier {
                    name: "self".to_string(),
                    span,
                }
            }
            TokenKind::Underscore => {
                let span = self.span();
                self.advance();
                Identifier {
                    name: "_".to_string(),
                    span,
                }
            }
            _ => return Err(self.err("Expected assignment target")),
        };

        let mut expr = Expr::Identifier(id.clone());
        let mut target = Assignee::Identifier(id.clone());

        loop {
            match self.current_kind() {
                TokenKind::Dot => {
                    self.advance();
                    let member = self.parse_id()?;
                    let member_expr = Expr::MemberAccess(MemberAccessExpr {
                        target: Box::new(expr.clone()),
                        member: member.clone(),
                        span: Span::dummy(),
                    });
                    target = Assignee::Member(MemberAccessExpr {
                        target: Box::new(expr),
                        member,
                        span: Span::dummy(),
                    });
                    expr = member_expr;
                }
                TokenKind::LBracket => {
                    self.advance();
                    // Use parse_prec(1) instead of parse_expr() so the slice
                    // colon (precedence 0) terminates the inner expression
                    // instead of being consumed as part of a higher-precedence
                    // binary op. This makes `foo[0:2]` work in assignment
                    // targets (e.g. `set, x, foo[0:2]`).
                    let index = self.parse_prec(1)?;
                    // If we see a colon, this is a slice target (rare but
                    // supported): `foo[0:2] = [1, 2]`. Parse the rest.
                    if self.current_kind() == TokenKind::Colon {
                        self.advance();
                        let mut end = None;
                        if self.current_kind() != TokenKind::Colon
                            && self.current_kind() != TokenKind::RBracket
                        {
                            end = Some(self.parse_prec(1)?);
                        }
                        let mut step = None;
                        if self.current_kind() == TokenKind::Colon {
                            self.advance();
                            if self.current_kind() != TokenKind::RBracket {
                                step = Some(self.parse_prec(1)?);
                            }
                        }
                        self.expect(TokenKind::RBracket)?;
                        let slice_expr = Expr::Slice(SliceExpr {
                            target: Box::new(expr.clone()),
                            start: Some(Box::new(index)),
                            end: end.map(Box::new),
                            step: step.map(Box::new),
                            span: Span::dummy(),
                        });
                        // Slices aren't valid assignment targets, but we
                        // store them as Index for VM-level handling.
                        target = Assignee::Index(IndexExpr {
                            target: Box::new(expr.clone()),
                            index: Box::new(slice_expr),
                            span: Span::dummy(),
                        });
                        expr = Expr::Identifier(id.clone());
                        continue;
                    }
                    self.expect(TokenKind::RBracket)?;
                    let index_expr = Expr::Index(IndexExpr {
                        target: Box::new(expr.clone()),
                        index: Box::new(index.clone()),
                        span: Span::dummy(),
                    });
                    target = Assignee::Index(IndexExpr {
                        target: Box::new(expr),
                        index: Box::new(index),
                        span: Span::dummy(),
                    });
                    expr = index_expr;
                }
                _ => break,
            }
        }

        Ok(target)
    }
    fn parse_if(&mut self) -> Result<Stmt> {
        let cond = self.parse_expr()?;
        self.skip_newlines();
        self.skip_newlines();
        let then_body = if self.current_kind() == TokenKind::Then {
            self.advance();
            self.skip_newlines();
            vec![self.parse_stmt()?]
        } else {
            self.parse_block()?
        };
        let mut elif = vec![];
        let mut el = None;
        loop {
            self.skip_newlines();
            if self.current_kind() == TokenKind::Elif {
                self.advance();
                self.expect(TokenKind::Comma)?;
                let c = self.parse_expr()?;
                let b = self.parse_block()?;
                elif.push((c, b));
            } else if self.current_kind() == TokenKind::Else {
                self.advance();
                el = Some(self.parse_block()?);
                break;
            } else {
                break;
            }
        }
        Ok(Stmt::If(IfStmt {
            condition: cond,
            then_body,
            elif_chain: elif,
            else_body: el,
            span: Span::dummy(),
        }))
    }
    fn parse_for(&mut self) -> Result<Stmt> {
        let var = self.parse_id()?;
        self.expect(TokenKind::Comma)?;
        self.skip_newlines();
        // Check for multi-variable for-in: `for, i, t, in, enumerate(list)`
        // If after the first var + comma there's another identifier (not `in`/`from`),
        // we collect all vars and wrap the iterable in an enumerate-like pattern.
        let mut vars = vec![var.clone()];
        while self.current_kind() == TokenKind::Identifier
            && self.lex() != "in"
            && self.lex() != "from"
        {
            vars.push(self.parse_id()?);
            if self.current_kind() == TokenKind::Comma {
                self.advance();
                self.skip_newlines();
            } else {
                break;
            }
        }
        if self.current_kind() == TokenKind::In {
            self.advance();
            self.expect(TokenKind::Comma)?;
            let it = self.parse_expr()?;
            let b = self.parse_block()?;
            if vars.len() == 1 {
                Ok(Stmt::ForIn(ForInStmt {
                    label: None,
                    var: vars.into_iter().next().unwrap(),
                    iterable: it,
                    body: b,
                    else_body: None,
                    span: Span::dummy(),
                }))
            } else {
                // Multi-variable: wrap body to destructure tuple elements.
                // For `for, i, t, in, enumerate(list)`, we iterate the list
                // (each element should be a tuple), and assign vars[0] = element[0],
                // vars[1] = element[1], etc.
                let primary_var = vars[0].clone();
                let extra_vars: Vec<Identifier> = vars[1..].to_vec();
                let mut new_body: Vec<Stmt> = Vec::new();
                for (idx, v) in extra_vars.iter().enumerate() {
                    new_body.push(Stmt::Assign(AssignStmt {
                        targets: vec![Assignee::Identifier(v.clone())],
                        value: Expr::Index(IndexExpr {
                            target: Box::new(Expr::Identifier(primary_var.clone())),
                            index: Box::new(Expr::Integer(IntegerLiteral {
                                value: (idx + 1) as i64,
                                raw: (idx + 1).to_string(),
                                span: Span::dummy(),
                            })),
                            span: Span::dummy(),
                        }),
                        operator: AssignOp::Simple,
                        span: Span::dummy(),
                    }));
                    // Also reassign primary_var to its first element
                    if idx == 0 {
                        // Only for the first extra var: primary = primary[0]
                    }
                }
                // Prepend the destructure assignments to the body
                // primary_var = primary_var[0], extra_vars[0] = original_primary[1], etc.
                // Actually, for enumerate, the element is (index, value).
                // So: i = element[0], t = element[1]
                // We need to save the element first.
                let temp_name = format!("__for_tuple_{}", primary_var.name);
                let mut destructure: Vec<Stmt> = Vec::new();
                destructure.push(Stmt::Assign(AssignStmt {
                    targets: vec![Assignee::Identifier(Identifier { name: temp_name.clone(), span: Span::dummy() })],
                    value: Expr::Identifier(primary_var.clone()),
                    operator: AssignOp::Simple,
                    span: Span::dummy(),
                }));
                for (idx, v) in vars.iter().enumerate() {
                    destructure.push(Stmt::Assign(AssignStmt {
                        targets: vec![Assignee::Identifier(v.clone())],
                        value: Expr::Index(IndexExpr {
                            target: Box::new(Expr::Identifier(Identifier { name: temp_name.clone(), span: Span::dummy() })),
                            index: Box::new(Expr::Integer(IntegerLiteral {
                                value: idx as i64,
                                raw: idx.to_string(),
                                span: Span::dummy(),
                            })),
                            span: Span::dummy(),
                        }),
                        operator: AssignOp::Simple,
                        span: Span::dummy(),
                    }));
                }
                destructure.extend(b);
                Ok(Stmt::ForIn(ForInStmt {
                    label: None,
                    var: primary_var,
                    iterable: it,
                    body: destructure,
                    else_body: None,
                    span: Span::dummy(),
                }))
            }
        } else if self.current_kind() == TokenKind::From {
            self.advance();
            self.expect(TokenKind::Comma)?;
            let f = self.parse_expr()?;
            self.expect_ident("to")?;
            let t = self.parse_expr()?;
            // Optional step clause: `step, N` or `step N`.
            let step = if self.current_kind() == TokenKind::Step {
                self.advance();
                if self.current_kind() == TokenKind::Comma {
                    self.advance();
                }
                Some(Box::new(self.parse_expr()?))
            } else if self.current_kind() == TokenKind::Comma {
                self.advance();
                if self.current_kind() == TokenKind::Step {
                    self.advance();
                    Some(Box::new(self.parse_expr()?))
                } else {
                    None
                }
            } else {
                None
            };
            self.skip_newlines();
            let b = self.parse_block()?;
            Ok(Stmt::ForRange(ForRangeStmt {
                label: None,
                var,
                from: f,
                to: t,
                step,
                body: b,
                else_body: None,
                span: Span::dummy(),
            }))
        } else {
            Err(self.err("Expected in/from"))
        }
    }
    fn parse_match(&mut self) -> Result<Stmt> {
        let e = self.parse_expr()?;
        self.nl();
        let mut cases = vec![];
        let mut el = None;
        if self.current_kind() == TokenKind::Indent {
            self.advance();
            loop {
                self.skip_newlines();
                if matches!(self.current_kind(), TokenKind::Dedent | TokenKind::EOF) {
                    break;
                }
                if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                    break;
                }
                match self.current_kind() {
                    TokenKind::Case => {
                        self.advance();
                        self.eat_comma();
                        let p = self.parse_pattern()?;
                        let g = if self.current_kind() == TokenKind::If {
                            self.advance();
                            Some(self.parse_expr()?)
                        } else {
                            None
                        };
                        // Body of case: must be indented
                        let b = self.parse_block()?; // parse_block handles the body, doesn't consume /end
                        cases.push(MatchCase {
                            pattern: p,
                            guard: g,
                            body: b,
                            span: Span::dummy(),
                        });
                    }
                    TokenKind::Else => {
                        self.advance();
                        el = Some(self.parse_block()?);
                        break;
                    }
                    _ => break,
                }
            }
            if self.current_kind() == TokenKind::Dedent {
                self.advance();
            }
            self.skip_newlines();
            if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                self.advance();
            }
        }
        Ok(Stmt::Match(MatchStmt {
            expr: e,
            cases,
            else_case: el,
            span: Span::dummy(),
        }))
    }
    fn parse_try(&mut self) -> Result<Stmt> {
        let tb = self.parse_block()?;
        let mut cv = None;
        let mut cb = None;
        let mut fb = None;
        self.skip_newlines();
        if self.current_kind() == TokenKind::Catch {
            self.advance();
            self.skip_newlines();
            self.eat_comma();
            if self.current_kind() == TokenKind::Identifier {
                cv = Some(self.parse_id()?);
            }
            cb = Some(self.parse_block()?);
            self.skip_newlines();
        }
        if self.current_kind() == TokenKind::Finally {
            self.advance();
            fb = Some(self.parse_block()?);
        }
        self.skip_newlines();
        if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
            self.advance();
        }
        Ok(Stmt::Try(TryStmt {
            try_body: tb,
            catch_var: cv,
            catch_body: cb,
            finally_body: fb,
            span: Span::dummy(),
        }))
    }
    fn parse_with(&mut self) -> Result<Stmt> {
        self.eat_comma();
        let manager = self.parse_expr()?;
        // Vredrs syntax: `with, manager`  or  `with, manager, as, var`
        // (commas are required between tokens by the language's comma-delimited style).
        let var = if self.current_kind() == TokenKind::Comma {
            self.advance();
            if self.current_kind() == TokenKind::As {
                self.advance();
                // optional comma between `as` and the identifier
                self.eat_comma();
            }
            Some(self.parse_id()?)
        } else if self.current_kind() == TokenKind::As {
            self.advance();
            self.eat_comma();
            Some(self.parse_id()?)
        } else {
            None
        };
        self.skip_newlines();
        let body = self.parse_block()?;
        Ok(Stmt::With(WithStmt {
            manager,
            var,
            body,
            span: Span::dummy(),
        }))
    }

    fn parse_select(&mut self) -> Result<Stmt> {
        self.nl();
        let mut cases = vec![];
        let mut def = None;
        if self.current_kind() == TokenKind::Indent {
            self.advance();
            loop {
                self.skip_newlines();
                if matches!(self.current_kind(), TokenKind::Dedent | TokenKind::EOF) {
                    break;
                }
                if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                    break;
                }
                match self.current_kind() {
                    TokenKind::Case => {
                        self.advance();
                        self.eat_comma();
                        let d = self.parse_sel_dir()?;
                        self.nl();
                        let b = self.parse_block()?;
                        cases.push(SelectCase {
                            direction: d,
                            body: b,
                            span: Span::dummy(),
                        });
                    }
                    TokenKind::Identifier if self.lex() == "default" => {
                        self.advance();
                        self.skip_newlines();
                        def = Some(self.parse_block()?);
                        break;
                    }
                    TokenKind::Else => {
                        self.advance();
                        def = Some(self.parse_block()?);
                        break;
                    }
                    _ => break,
                }
            }
            if self.current_kind() == TokenKind::Dedent {
                self.advance();
            }
            self.skip_newlines();
            if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                self.advance();
            }
        }
        Ok(Stmt::Select(SelectStmt {
            cases,
            default_case: def,
            span: Span::dummy(),
        }))
    }

    fn parse_sel_dir(&mut self) -> Result<SelectDirection> {
        match self.current_kind() {
            TokenKind::Receive => {
                self.advance();
                // receive(ch) or receive(ch), v
                if self.current_kind() == TokenKind::LParen {
                    self.advance();
                    let ch = self.parse_expr()?;
                    self.expect(TokenKind::RParen)?;
                    let v = if self.current_kind() == TokenKind::Comma {
                        self.advance();
                        Some(self.parse_id()?)
                    } else {
                        None
                    };
                    Ok(SelectDirection::Receive {
                        channel: ch,
                        var: v,
                    })
                } else {
                    Err(self.err("Expected ( after receive"))
                }
            }
            TokenKind::Send => {
                self.advance();
                if self.current_kind() == TokenKind::LParen {
                    self.advance();
                    let ch = self.parse_expr()?;
                    self.expect(TokenKind::Comma)?;
                    let val = self.parse_expr()?;
                    self.expect(TokenKind::RParen)?;
                    Ok(SelectDirection::Send {
                        channel: ch,
                        value: val,
                    })
                } else {
                    Err(self.err("Expected ( after send"))
                }
            }
            _ if self.current_kind() == TokenKind::Identifier && self.lex() == "after" => {
                self.advance();
                self.expect(TokenKind::LParen)?;
                let d = self.parse_expr()?;
                self.expect(TokenKind::RParen)?;
                Ok(SelectDirection::After(d))
            }
            _ => Err(self.err("Expected receive/send/after")),
        }
    }

    fn parse_directive(&mut self) -> Result<Stmt> {
        let t = self.cur_tok().clone();
        self.advance();
        if t.lexeme == "/end" {
            return Err(self.err("Unexpected /end"));
        }
        let dir_name = &t.lexeme[1..];

        if t.kind == TokenKind::TableAssign || t.lexeme == "/=" {
            return self.parse_table_assign();
        }

        if t.is_scope_start() {
            self.skip_newlines();
            let body = self.parse_scope_body()?;
            // Consume the closing /
            self.skip_newlines();
            if self.current_kind() == TokenKind::Slash {
                self.advance();
            } else if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/" {
                self.advance();
            }
            return Ok(Stmt::ScopeBlock(ScopeBlock {
                prefix: dir_name.to_string(),
                body,
                span: Span::dummy(),
            }));
        }

        let is_fill = matches!(dir_name, "set" | "paste" | "println" | "import" | "export");

        self.skip_newlines();
        let body = if is_fill {
            self.parse_fill_block()?
        } else {
            self.parse_block()?
        };
        Ok(Stmt::DirectiveBlock(DirectiveBlock {
            directive: dir_name.to_string(),
            body,
            span: Span::dummy(),
        }))
    }

    fn parse_scope_body(&mut self) -> Result<Vec<Stmt>> {
        let mut s = vec![];
        if self.current_kind() == TokenKind::Indent {
            self.advance();
            loop {
                self.skip_newlines();
                if self.current_kind() == TokenKind::Dedent || self.is_at_end() {
                    break;
                }
                if self.current_kind() == TokenKind::Slash {
                    break;
                }
                if self.current_kind() == TokenKind::DirectiveOrScope {
                    let l = self.lex();
                    if l == "/end" {
                        break;
                    }
                    if l == "/" {
                        break;
                    }
                    if l.starts_with('/') {
                        s.push(self.parse_directive()?);
                        continue;
                    }
                    break;
                }
                s.push(self.parse_stmt()?);
            }
            if self.current_kind() == TokenKind::Dedent {
                self.advance();
            }
            self.skip_newlines();
            if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                self.advance();
            } else if self.is_at_end() {
                return Err(self.err("Unclosed block, expected /end"));
            }
        } else {
            // No Indent found, but fill block must have indented body
            return Err(self.err("Unclosed block, expected /end"));
        }
        Ok(s)
    }

    fn parse_fill_block(&mut self) -> Result<Vec<Stmt>> {
        let mut s = vec![];
        if self.current_kind() == TokenKind::Indent {
            self.advance();
            loop {
                self.skip_newlines();
                if self.is_at_end() {
                    return Err(self.err("Unclosed block, expected /end"));
                }
                if self.current_kind() == TokenKind::Dedent {
                    break;
                }
                if self.current_kind() == TokenKind::DirectiveOrScope {
                    let l = self.lex();
                    if l == "/end" || l == "/" {
                        break;
                    }
                    s.push(self.parse_directive()?);
                    continue;
                }
                if self.current_kind() == TokenKind::TableAssign {
                    // Backtrack to extract column names from previously parsed exprs
                    let mut cols = vec![];
                    while let Some(stmt) = s.last() {
                        match stmt {
                            Stmt::Expr(expr_stmt) => {
                                if let Expr::Identifier(id) = &expr_stmt.expr {
                                    cols.push(id.clone());
                                    s.pop();
                                } else {
                                    break;
                                }
                            }
                            _ => break,
                        }
                    }
                    cols.reverse();
                    let table = match self.parse_table_assign()? {
                        Stmt::TableAssign(mut t) => {
                            t.column_names = cols;
                            t
                        }
                        other => {
                            return Err(self.err("Expected TableAssign after /= in /set block"))
                        }
                    };
                    s.push(Stmt::TableAssign(table));
                    continue;
                }
                s.push(self.parse_stmt()?);
            }
            if self.current_kind() == TokenKind::Dedent {
                self.advance();
            }
            self.skip_newlines();
            if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                self.advance();
            } else if self.is_at_end() {
                return Err(self.err("Unclosed block, expected /end"));
            }
        } else {
            // No Indent found, but fill block must have indented body
            return Err(self.err("Unclosed block, expected /end"));
        }
        Ok(s)
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>> {
        if self.current_kind() == TokenKind::Semicolon {
            self.advance();
            return self.parse_until_nl();
        }
        self.skip_newlines();
        let mut s = vec![];
        if self.current_kind() == TokenKind::Indent {
            self.advance();
            loop {
                self.skip_newlines();
                if self.is_at_end() {
                    return Err(self.err("Unclosed block, expected /end"));
                }
                if self.current_kind() == TokenKind::Dedent {
                    break;
                }
                if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                    break;
                }
                if self.current_kind() == TokenKind::DirectiveOrScope {
                    s.push(self.parse_directive()?);
                    continue;
                }
                s.push(self.parse_stmt()?);
            }
            if self.current_kind() == TokenKind::Dedent {
                self.advance();
            }
            self.skip_newlines();
            if self.current_kind() == TokenKind::DirectiveOrScope && self.lex() == "/end" {
                self.advance();
            }
        } else if self.current_kind() == TokenKind::LBrace {
            self.advance();
            while self.current_kind() != TokenKind::RBrace && !self.is_at_end() {
                s.push(self.parse_stmt()?);
                self.skip_newlines();
            }
            self.expect(TokenKind::RBrace)?;
        }
        Ok(s)
    }

    pub fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_prec(0)
    }
    fn parse_prec(&mut self, min: usize) -> Result<Expr> {
        let mut left = self.parse_prefix()?;
        loop {
            if self.is_at_nl() {
                break;
            }
            let p = prec(&self.current_kind());
            if p < min {
                break;
            }
            match self.current_kind() {
                TokenKind::As => {
                    self.advance();
                    let ty = self.parse_type_expr()?;
                    // C* and FFI-facing Vredrs use `expr as Type` for physical casts.
                    // The AST stores it explicitly so raw backends can lower ptr[T] without Value boxing.
                    left = Expr::Cast(CastExpr {
                        expr: Box::new(left),
                        type_expr: ty,
                        span: Span::dummy(),
                    });
                }
                TokenKind::Question => {
                    self.advance();
                    // `expr?` is try-propagation when followed by a delimiter; otherwise parse `cond ? a : b`.
                    if matches!(
                        self.current_kind(),
                        TokenKind::EOF
                            | TokenKind::Newline
                            | TokenKind::Comma
                            | TokenKind::Semicolon
                            | TokenKind::RParen
                            | TokenKind::RBracket
                            | TokenKind::RBrace
                            | TokenKind::Dedent
                    ) {
                        left = Expr::TryPropagate(TryPropagateExpr {
                            expr: Box::new(left),
                            span: Span::dummy(),
                        });
                    } else {
                        let t = self.parse_expr()?;
                        self.expect(TokenKind::Colon)?;
                        let f = self.parse_prec(p)?;
                        left = Expr::Ternary(TernaryExpr {
                            condition: Box::new(left),
                            true_branch: Box::new(t),
                            false_branch: Box::new(f),
                            span: Span::dummy(),
                        });
                    }
                }
                TokenKind::DotDot => {
                    self.advance();
                    let end = if !self.is_at_nl()
                        && self.current_kind() != TokenKind::Dedent
                        && self.current_kind() != TokenKind::EOF
                    {
                        Some(Box::new(self.parse_prec(p)?))
                    } else {
                        None
                    };
                    let step = if self.current_kind() == TokenKind::Comma {
                        self.advance();
                        if self.current_kind() == TokenKind::Step {
                            self.advance();
                            Some(Box::new(self.parse_expr()?))
                        } else {
                            None
                        }
                    } else if self.current_kind() == TokenKind::Step {
                        self.advance();
                        Some(Box::new(self.parse_expr()?))
                    } else {
                        None
                    };
                    left = Expr::Range(RangeExpr {
                        start: Some(Box::new(left)),
                        end,
                        inclusive: false,
                        step,
                        span: Span::dummy(),
                    });
                }
                TokenKind::DotDotDot => {
                    self.advance();
                    let end = if !self.is_at_nl()
                        && self.current_kind() != TokenKind::Dedent
                        && self.current_kind() != TokenKind::EOF
                    {
                        Some(Box::new(self.parse_prec(p)?))
                    } else {
                        None
                    };
                    let step = if self.current_kind() == TokenKind::Comma {
                        self.advance();
                        if self.current_kind() == TokenKind::Step {
                            self.advance();
                            Some(Box::new(self.parse_expr()?))
                        } else {
                            None
                        }
                    } else if self.current_kind() == TokenKind::Step {
                        self.advance();
                        Some(Box::new(self.parse_expr()?))
                    } else {
                        None
                    };
                    left = Expr::Range(RangeExpr {
                        start: Some(Box::new(left)),
                        end,
                        inclusive: true,
                        step,
                        span: Span::dummy(),
                    });
                }
                TokenKind::Hash => {
                    self.advance();
                    left = Expr::Postfix(PostfixExpr {
                        operand: Box::new(left),
                        operator: PostfixOp::Length,
                        span: Span::dummy(),
                    });
                }
                TokenKind::Tilde => {
                    self.advance();
                    left = Expr::Postfix(PostfixExpr {
                        operand: Box::new(left),
                        operator: PostfixOp::Reverse,
                        span: Span::dummy(),
                    });
                }
                TokenKind::Caret => {
                    self.advance();
                    left = Expr::Postfix(PostfixExpr {
                        operand: Box::new(left),
                        operator: PostfixOp::AscSort,
                        span: Span::dummy(),
                    });
                }
                TokenKind::Underscore => {
                    self.advance();
                    left = Expr::Postfix(PostfixExpr {
                        operand: Box::new(left),
                        operator: PostfixOp::DescSort,
                        span: Span::dummy(),
                    });
                }
                TokenKind::QuestionQuestion => {
                    self.advance();
                    let r = self.parse_prec(p)?;
                    left = Expr::NullCoalesce(NullCoalesceExpr {
                        left: Box::new(left),
                        right: Box::new(r),
                        span: Span::dummy(),
                    });
                }
                TokenKind::PipeArrow => {
                    self.advance();
                    // Pipe is left-associative: a |> f |> g  =>  (a |> f) |> g
                    let r = self.parse_prec(p + 1)?;
                    left = Expr::Pipe(PipeExpr {
                        left: Box::new(left),
                        right: Box::new(r),
                        span: Span::dummy(),
                    });
                }
                TokenKind::QuestionDot => {
                    self.advance();
                    // Parse the link following `?.` (member / method call /
                    // index) and append it to the optional chain. If `left`
                    // is already an OptionalChain (e.g. `a?.b?.c`), append to
                    // its existing chain so the whole expression short-
                    // circuits on a null target; otherwise wrap `left` in a
                    // fresh OptionalChain.
                    let link = self.parse_optional_chain_link()?;
                    left = match left {
                        Expr::OptionalChain(mut oc) => {
                            oc.chain.push(link);
                            Expr::OptionalChain(oc)
                        }
                        other => Expr::OptionalChain(OptionalChainExpr {
                            target: Box::new(other),
                            chain: vec![link],
                            span: Span::dummy(),
                        }),
                    };
                }
                TokenKind::Dot => {
                    self.advance();
                    let member = self.parse_id()?;
                    left = Expr::MemberAccess(MemberAccessExpr {
                        target: Box::new(left),
                        member,
                        span: Span::dummy(),
                    });
                    if self.current_kind() == TokenKind::LParen {
                        self.advance();
                        left = Expr::Call(CallExpr {
                            callee: Box::new(left),
                            args: self.parse_args()?,
                            span: Span::dummy(),
                        });
                    }
                }
                TokenKind::LBracket => {
                    self.advance();
                    let mut start = None;
                    let mut end = None;
                    let mut step = None;
                    let is_slice;
                    if self.current_kind() == TokenKind::Colon {
                        is_slice = true;
                        self.advance();
                    } else {
                        // Use parse_prec(1) instead of parse_expr() so the
                        // slice's `:` (which has precedence 0) terminates
                        // the inner expression instead of being consumed
                        // by a higher-precedence binary op.
                        let first = self.parse_prec(1)?;
                        if self.current_kind() == TokenKind::Colon {
                            is_slice = true;
                            start = Some(Box::new(first));
                            self.advance();
                        } else {
                            self.expect(TokenKind::RBracket)?;
                            left = Expr::Index(IndexExpr {
                                target: Box::new(left),
                                index: Box::new(first),
                                span: Span::dummy(),
                            });
                            continue;
                        }
                    }
                    if self.current_kind() != TokenKind::Colon
                        && self.current_kind() != TokenKind::RBracket
                    {
                        end = Some(Box::new(self.parse_prec(1)?));
                    }
                    if self.current_kind() == TokenKind::Colon {
                        self.advance();
                        if self.current_kind() != TokenKind::RBracket {
                            step = Some(Box::new(self.parse_prec(1)?));
                        }
                    }
                    self.expect(TokenKind::RBracket)?;
                    if is_slice {
                        left = Expr::Slice(SliceExpr {
                            target: Box::new(left),
                            start,
                            end,
                            step,
                            span: Span::dummy(),
                        });
                    }
                }
                TokenKind::Plus
                | TokenKind::Minus
                | TokenKind::Star
                | TokenKind::Slash
                | TokenKind::Percent
                | TokenKind::FloorDiv
                | TokenKind::Eq
                | TokenKind::Ne
                | TokenKind::Lt
                | TokenKind::Gt
                | TokenKind::Le
                | TokenKind::Ge
                | TokenKind::Is
                | TokenKind::In
                | TokenKind::And
                | TokenKind::Or => {
                    let op_kind = self.current_kind();
                    self.advance();
                    let op = binop(&op_kind);
                    let r = self.parse_prec(p + 1)?;
                    left = Expr::Binary(BinaryExpr {
                        left: Box::new(left),
                        operator: op,
                        right: Box::new(r),
                        span: Span::dummy(),
                    });
                }
                // Power (**) is right-associative: 2 ** 3 ** 2 = 2 ** (3 ** 2).
                TokenKind::Power => {
                    self.advance();
                    let r = self.parse_prec(p)?; // same precedence → right-assoc
                    left = Expr::Binary(BinaryExpr {
                        left: Box::new(left),
                        operator: BinaryOp::Power,
                        right: Box::new(r),
                        span: Span::dummy(),
                    });
                }
                TokenKind::Repeated => {
                    // `expr repeated N` — string/list repetition. Has the
                    // same precedence as `*` (8), left-associative.
                    self.advance();
                    let r = self.parse_prec(p + 1)?;
                    left = Expr::Binary(BinaryExpr {
                        left: Box::new(left),
                        operator: BinaryOp::Repeated,
                        right: Box::new(r),
                        span: Span::dummy(),
                    });
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_prefix(&mut self) -> Result<Expr> {
        match self.current_kind() {
            TokenKind::IntegerLiteral => {
                let t = self.adv_tok();
                let value = parse_i64_literal(&t.lexeme).unwrap_or(0);
                Ok(Expr::Integer(IntegerLiteral {
                    value,
                    raw: t.lexeme,
                    span: t.span,
                }))
            }
            TokenKind::FloatLiteral => {
                let t = self.adv_tok();
                Ok(Expr::Float(FloatLiteral {
                    value: t.lexeme.parse().unwrap_or(0.0),
                    raw: t.lexeme,
                    span: t.span,
                }))
            }
            TokenKind::StringLiteral => {
                let t = self.adv_tok();
                let parts = self.parse_string_parts(&t.lexeme)?;
                Ok(Expr::String_(StringLiteral {
                    parts,
                    span: t.span,
                }))
            }
            TokenKind::MultiLineString => {
                let t = self.adv_tok();
                let parts = self.parse_string_parts(&t.lexeme)?;
                Ok(Expr::MultiLineString(MultiLineStringLiteral {
                    parts,
                    span: t.span,
                }))
            }
            TokenKind::TrueKw => {
                let s = self.span();
                self.advance();
                Ok(Expr::Bool(BoolLiteral {
                    value: true,
                    span: s,
                }))
            }
            TokenKind::FalseKw => {
                let s = self.span();
                self.advance();
                Ok(Expr::Bool(BoolLiteral {
                    value: false,
                    span: s,
                }))
            }
            TokenKind::Null => {
                let s = self.span();
                self.advance();
                Ok(Expr::Null(NullLiteral { span: s }))
            }
            TokenKind::Channel | TokenKind::Send | TokenKind::Receive | TokenKind::Identifier | TokenKind::Input => {
                let id = self.parse_id()?;
                if self.current_kind() == TokenKind::LParen {
                    self.advance();
                    let a = self.parse_args()?;
                    Ok(Expr::Call(CallExpr {
                        callee: Box::new(Expr::Identifier(id)),
                        args: a,
                        span: Span::dummy(),
                    }))
                } else {
                    Ok(Expr::Identifier(id))
                }
            }
            TokenKind::Struct => {
                self.advance();
                if self.current_kind() == TokenKind::LBrace {
                    self.advance();
                    let mut fld = vec![];
                    while self.current_kind() != TokenKind::RBrace && !self.is_at_end() {
                        let fnm = self.parse_id()?;
                        self.expect(TokenKind::Colon)?;
                        let ft = self.parse_type_expr()?;
                        fld.push(StructField {
                            annotations: vec![],
                            name: fnm,
                            type_annotation: Some(ft),
                            default_value: None,
                            visibility: Visibility::Public,
                            span: Span::dummy(),
                        });
                        if self.current_kind() == TokenKind::Comma {
                            self.advance();
                        }
                    }
                    self.expect(TokenKind::RBrace)?;
                    Ok(Expr::Identifier(Identifier {
                        name: "struct".to_string(),
                        span: Span::dummy(),
                    }))
                } else {
                    Ok(Expr::Identifier(Identifier {
                        name: "struct".to_string(),
                        span: Span::dummy(),
                    }))
                }
            }
            TokenKind::LParen => {
                self.advance();
                if self.current_kind() == TokenKind::RParen {
                    self.advance();
                    Ok(Expr::Tuple(TupleLiteral {
                        elements: vec![],
                        span: Span::dummy(),
                    }))
                } else {
                    let f = self.parse_expr()?;
                    if self.current_kind() == TokenKind::Comma {
                        let mut v = vec![f];
                        while self.current_kind() == TokenKind::Comma {
                            self.advance();
                            // Allow trailing comma in tuples.
                            if self.current_kind() == TokenKind::RParen {
                                break;
                            }
                            v.push(self.parse_expr()?);
                        }
                        self.expect(TokenKind::RParen)?;
                        Ok(Expr::Tuple(TupleLiteral {
                            elements: v,
                            span: Span::dummy(),
                        }))
                    } else {
                        self.expect(TokenKind::RParen)?;
                        Ok(f)
                    }
                }
            }
            TokenKind::LBracket => {
                self.advance();
                if self.current_kind() == TokenKind::RBracket {
                    self.advance();
                    Ok(Expr::List(ListLiteral {
                        elements: vec![],
                        span: Span::dummy(),
                    }))
                } else {
                    let f = self.parse_expr()?;
                    if self.current_kind() == TokenKind::Comma {
                        let mut v = vec![f];
                        while self.current_kind() == TokenKind::Comma {
                            self.advance();
                            // Allow trailing comma: `[1, 2, 3,]`
                            if self.current_kind() == TokenKind::RBracket {
                                break;
                            }
                            v.push(self.parse_expr()?);
                        }
                        self.expect(TokenKind::RBracket)?;
                        Ok(Expr::List(ListLiteral {
                            elements: v,
                            span: Span::dummy(),
                        }))
                    } else if self.current_kind() == TokenKind::For {
                        self.advance();
                        let var = self.parse_id()?;
                        if self.current_kind() == TokenKind::In {
                            self.advance();
                        }
                        let iter = self.parse_expr()?;
                        let mut cond = None;
                        if self.current_kind() == TokenKind::If {
                            self.advance();
                            cond = Some(Box::new(self.parse_expr()?));
                        }
                        self.expect(TokenKind::RBracket)?;
                        Ok(Expr::ListComprehension(Box::new(ListComprehension {
                            var,
                            iterable: Box::new(iter),
                            condition: cond,
                            result_expr: Box::new(f),
                            span: Span::dummy(),
                        })))
                    } else {
                        self.expect(TokenKind::RBracket)?;
                        Ok(Expr::List(ListLiteral {
                            elements: vec![f],
                            span: Span::dummy(),
                        }))
                    }
                }
            }
            TokenKind::LBrace => {
                self.advance();
                if self.current_kind() == TokenKind::RBrace {
                    self.advance();
                    Ok(Expr::Dict(DictLiteral {
                        entries: vec![],
                        span: Span::dummy(),
                    }))
                } else {
                    // Parse the first key. Use parse_prec(1) so a bare
                    // identifier `name` in `{name: value}` is treated as
                    // a string key, not a variable reference. (Without
                    // this, `name` would be parsed as an Identifier expr,
                    // which evaluates to the variable's value — usually
                    // Null if undefined.)
                    //
                    // Actually, we WANT bare identifiers in dict literals
                    // to be treated as string keys (like JavaScript object
                    // literals). So if the first token is an Identifier
                    // followed by a Colon, convert it to a string literal.
                    let f = if self.current_kind() == TokenKind::Identifier
                        && self.peek_kind() == Some(TokenKind::Colon)
                    {
                        let id = self.parse_id()?;
                        Expr::String_(StringLiteral {
                            parts: vec![StringPart::Text(id.name)],
                            span: id.span,
                        })
                    } else {
                        self.parse_expr()?
                    };
                    if self.current_kind() == TokenKind::Colon {
                        self.advance();
                        let val = self.parse_expr()?;
                        if self.current_kind() == TokenKind::Comma {
                            let mut e = vec![(f, val)];
                            while self.current_kind() == TokenKind::Comma {
                                self.advance();
                                // Allow trailing comma in dict literal.
                                if self.current_kind() == TokenKind::RBrace {
                                    break;
                                }
                                // Each subsequent key: bare identifier → string.
                                let k = if self.current_kind() == TokenKind::Identifier
                                    && self.peek_kind() == Some(TokenKind::Colon)
                                {
                                    let id = self.parse_id()?;
                                    Expr::String_(StringLiteral {
                                        parts: vec![StringPart::Text(id.name)],
                                        span: id.span,
                                    })
                                } else {
                                    self.parse_expr()?
                                };
                                self.expect(TokenKind::Colon)?;
                                let v = self.parse_expr()?;
                                e.push((k, v));
                            }
                            self.expect(TokenKind::RBrace)?;
                            Ok(Expr::Dict(DictLiteral {
                                entries: e,
                                span: Span::dummy(),
                            }))
                        } else if self.current_kind() == TokenKind::For {
                            self.advance();
                            let var = self.parse_id()?;
                            if self.current_kind() == TokenKind::In {
                                self.advance();
                            }
                            let iter = self.parse_expr()?;
                            let mut cond = None;
                            if self.current_kind() == TokenKind::If {
                                self.advance();
                                cond = Some(Box::new(self.parse_expr()?));
                            }
                            self.expect(TokenKind::RBrace)?;
                            Ok(Expr::DictComprehension(Box::new(DictComprehension {
                                var,
                                iterable: Box::new(iter),
                                condition: cond,
                                key_expr: Box::new(f),
                                value_expr: Box::new(val),
                                span: Span::dummy(),
                            })))
                        } else {
                            self.expect(TokenKind::RBrace)?;
                            Ok(Expr::Dict(DictLiteral {
                                entries: vec![(f, val)],
                                span: Span::dummy(),
                            }))
                        }
                    } else if self.current_kind() == TokenKind::For {
                        self.advance();
                        let var = self.parse_id()?;
                        if self.current_kind() == TokenKind::In {
                            self.advance();
                        }
                        let iter = self.parse_expr()?;
                        let mut cond = None;
                        if self.current_kind() == TokenKind::If {
                            self.advance();
                            cond = Some(Box::new(self.parse_expr()?));
                        }
                        self.expect(TokenKind::RBrace)?;
                        Ok(Expr::SetComprehension(Box::new(SetComprehension {
                            var,
                            iterable: Box::new(iter),
                            condition: cond,
                            result_expr: Box::new(f),
                            span: Span::dummy(),
                        })))
                    } else {
                        let mut e = vec![f];
                        while self.current_kind() == TokenKind::Comma {
                            self.advance();
                            // Allow trailing comma in set literal.
                            if self.current_kind() == TokenKind::RBrace {
                                break;
                            }
                            e.push(self.parse_expr()?);
                        }
                        self.expect(TokenKind::RBrace)?;
                        Ok(Expr::Set(SetLiteral {
                            elements: e,
                            span: Span::dummy(),
                        }))
                    }
                }
            }
            TokenKind::Minus => {
                let s = self.span();
                self.advance();
                let o = self.parse_prec(100)?;
                Ok(Expr::Unary(UnaryExpr {
                    operator: UnaryOp::Neg,
                    operand: Box::new(o),
                    span: s,
                }))
            }
            TokenKind::Not => {
                let s = self.span();
                self.advance();
                let o = self.parse_prec(100)?;
                Ok(Expr::Unary(UnaryExpr {
                    operator: UnaryOp::Not,
                    operand: Box::new(o),
                    span: s,
                }))
            }
            TokenKind::Fn => {
                self.advance();
                let p = self.parse_fn_params()?;
                // Lambda body: try expression first, fallback to single statement
                let b = match self.parse_expr() {
                    Ok(e) => e,
                    Err(_) => {
                        // Try parsing as a statement then convert to expression
                        if let Ok(stmt) = self.parse_stmt() {
                            match stmt {
                                Stmt::Expr(es) => es.expr,
                                Stmt::Println(ps) => Expr::Call(CallExpr {
                                    callee: Box::new(Expr::Identifier(Identifier {
                                        name: "println".to_string(),
                                        span: Span::dummy(),
                                    })),
                                    args: ps.args,
                                    span: Span::dummy(),
                                }),
                                Stmt::Paste(ps) => Expr::Call(CallExpr {
                                    callee: Box::new(Expr::Identifier(Identifier {
                                        name: "paste".to_string(),
                                        span: Span::dummy(),
                                    })),
                                    args: ps.args,
                                    span: Span::dummy(),
                                }),
                                Stmt::Spawn(ss) => Expr::Spawn(SpawnExpr {
                                    call: Box::new(ss.call),
                                    span: Span::dummy(),
                                }),
                                _ => Expr::Null(NullLiteral {
                                    span: Span::dummy(),
                                }),
                            }
                        } else {
                            Expr::Null(NullLiteral {
                                span: Span::dummy(),
                            })
                        }
                    }
                };
                Ok(Expr::Lambda(LambdaExpr {
                    params: p,
                    body: Box::new(b),
                    return_type: None,
                    span: Span::dummy(),
                }))
            }
            TokenKind::Coro => {
                self.advance();
                self.expect(TokenKind::LParen)?;
                let f = self.parse_expr()?;
                let a = if self.current_kind() == TokenKind::Comma {
                    self.advance();
                    let a = self.parse_args_no_paren()?;
                    self.expect(TokenKind::RParen)?;
                    a
                } else {
                    self.expect(TokenKind::RParen)?;
                    vec![]
                };
                Ok(Expr::Coro(CoroExpr {
                    function: Box::new(f),
                    args: a,
                    span: Span::dummy(),
                }))
            }
            TokenKind::Await => {
                let sp = self.span();
                self.advance();
                let e = self.parse_prec(100)?;
                Ok(Expr::Await(AwaitExpr {
                    expr: Box::new(e),
                    span: sp,
                }))
            }
            TokenKind::Resume => {
                self.advance();
                self.expect(TokenKind::LParen)?;
                let h = self.parse_expr()?;
                let v = if self.current_kind() == TokenKind::Comma {
                    self.advance();
                    let values = self.parse_args_no_paren()?;
                    self.expect(TokenKind::RParen)?;
                    values
                } else {
                    self.expect(TokenKind::RParen)?;
                    vec![]
                };
                Ok(Expr::Resume(ResumeExpr {
                    handle: Box::new(h),
                    values: v,
                    span: Span::dummy(),
                }))
            }
            TokenKind::DotDotDot => {
                let s = self.span();
                self.advance();
                if self.is_at_nl()
                    || self.is_at_end()
                    || self.current_kind() == TokenKind::RBracket
                    || self.current_kind() == TokenKind::RParen
                {
                    // ... in function params means variadic, but here in prefix it could be rest pattern
                    Ok(Expr::Identifier(Identifier {
                        name: "...".to_string(),
                        span: s,
                    }))
                } else {
                    let e = self.parse_expr()?;
                    Ok(Expr::Spread(SpreadExpr {
                        expr: Box::new(e),
                        span: s,
                    }))
                }
            }
            _ => {
                let t = self.adv_tok();
                Ok(Expr::Identifier(Identifier {
                    name: t.lexeme,
                    span: t.span,
                }))
            }
        }
    }

    fn parse_pattern(&mut self) -> Result<Pattern> {
        let first = self.parse_pattern_atom()?;
        // Check for or-pattern: `pat | pat | pat`.
        // The `|` character is lexed as an Identifier with lexeme "|".
        if self.current_kind() == TokenKind::Identifier && self.lex() == "|" {
            let mut patterns = vec![first];
            while self.current_kind() == TokenKind::Identifier && self.lex() == "|" {
                self.advance();
                patterns.push(self.parse_pattern_atom()?);
            }
            return Ok(Pattern::Or(OrPattern {
                patterns,
                span: Span::dummy(),
            }));
        }
        Ok(first)
    }

    fn parse_pattern_atom(&mut self) -> Result<Pattern> {
        match self.current_kind() {
            TokenKind::Underscore => {
                let s = self.span();
                self.advance();
                Ok(Pattern::Wildcard(s))
            }
            TokenKind::IntegerLiteral
            | TokenKind::StringLiteral
            | TokenKind::TrueKw
            | TokenKind::FalseKw
            | TokenKind::Null => {
                let e = self.parse_prefix()?;
                Ok(Pattern::Literal(LiteralPattern {
                    literal: Box::new(e),
                    span: Span::dummy(),
                }))
            }
            TokenKind::LParen => {
                self.advance();
                let mut v = vec![];
                if self.current_kind() != TokenKind::RParen {
                    v.push(self.parse_pattern()?);
                    while self.current_kind() == TokenKind::Comma {
                        self.advance();
                        v.push(self.parse_pattern()?);
                    }
                }
                self.expect(TokenKind::RParen)?;
                Ok(Pattern::Tuple(TuplePattern {
                    elements: v,
                    span: Span::dummy(),
                }))
            }
            TokenKind::LBracket => {
                self.advance();
                let mut v = vec![];
                let mut r = None;
                while self.current_kind() != TokenKind::RBracket {
                    if self.current_kind() == TokenKind::Star {
                        self.advance();
                        r = Some(self.parse_id()?);
                        break;
                    }
                    v.push(self.parse_pattern()?);
                    if self.current_kind() == TokenKind::Comma {
                        self.advance();
                    }
                }
                self.expect(TokenKind::RBracket)?;
                Ok(Pattern::List(ListPattern {
                    elements: v,
                    rest: r,
                    span: Span::dummy(),
                }))
            }
            TokenKind::Identifier => {
                let id = self.parse_id()?;
                // Qualified enum variant: `Color.Red` or `Color.Red(...)`.
                if self.current_kind() == TokenKind::Dot {
                    self.advance();
                    let variant = self.parse_id()?;
                    let payload = if self.current_kind() == TokenKind::LParen {
                        self.advance();
                        let mut p = vec![];
                        if self.current_kind() != TokenKind::RParen {
                            p.push(self.parse_pattern()?);
                            while self.current_kind() == TokenKind::Comma {
                                self.advance();
                                p.push(self.parse_pattern()?);
                            }
                        }
                        self.expect(TokenKind::RParen)?;
                        Some(p)
                    } else {
                        None
                    };
                    return Ok(Pattern::EnumVariant(EnumVariantPattern {
                        type_name: id,
                        variant,
                        payload,
                        span: Span::dummy(),
                    }));
                }
                if self.current_kind() == TokenKind::LParen {
                    self.advance();
                    let mut p = vec![];
                    if self.current_kind() != TokenKind::RParen {
                        p.push(self.parse_pattern()?);
                        while self.current_kind() == TokenKind::Comma {
                            self.advance();
                            p.push(self.parse_pattern()?);
                        }
                    }
                    self.expect(TokenKind::RParen)?;
                    Ok(Pattern::EnumVariant(EnumVariantPattern {
                        type_name: Identifier {
                            name: String::new(),
                            span: Span::dummy(),
                        },
                        variant: id,
                        payload: Some(p),
                        span: Span::dummy(),
                    }))
                } else {
                    Ok(Pattern::Binding(BindingPattern {
                        name: id,
                        span: Span::dummy(),
                    }))
                }
            }
            TokenKind::IntegerLiteral
            | TokenKind::FloatLiteral
            | TokenKind::StringLiteral => {
                // Negative-number patterns: `-1 | -2` — we already parsed
                // the atom above; fall through to literal handling.
                let e = self.parse_prefix()?;
                Ok(Pattern::Literal(LiteralPattern {
                    literal: Box::new(e),
                    span: Span::dummy(),
                }))
            }
            _ => Err(self.err("Expected pattern")),
        }
    }

    fn parse_type_expr(&mut self) -> Result<TypeExpr> {
        match self.current_kind() {
            TokenKind::BitAnd => {
                self.advance();
                let is_mut = if self.current_kind() == TokenKind::Identifier && self.lex() == "mut" { self.advance(); true } else { false };
                let lifetime = if self.current_kind() == TokenKind::Identifier && self.lex().starts_with('\'') { let lt = self.lex().to_string(); self.advance(); Some(lt) } else { None };
                let inner = self.parse_type_expr()?;
                if is_mut { Ok(TypeExpr::MutBorrow { inner: Box::new(inner), lifetime, span: Span::dummy() }) }
                else { Ok(TypeExpr::Borrow { inner: Box::new(inner), lifetime, span: Span::dummy() }) }
            }
            TokenKind::Struct => {
                self.advance();
                if self.current_kind() == TokenKind::LBrace {
                    self.advance();
                    while self.current_kind() != TokenKind::RBrace && !self.is_at_end() {
                        // field: name, type (comma-separated, no colon)
                        let fname = self.parse_id()?;
                        let _ftype = if self.current_kind() == TokenKind::Comma {
                            self.advance();
                            self.parse_type_expr()?
                        } else if self.current_kind() == TokenKind::Colon {
                            self.advance();
                            self.parse_type_expr()?
                        } else {
                            TypeExpr::Basic(BasicType::Any, Span::dummy())
                        };
                        if self.current_kind() == TokenKind::Comma {
                            self.advance();
                        }
                    }
                    self.expect(TokenKind::RBrace)?;
                }
                Ok(TypeExpr::Basic(BasicType::Any, Span::dummy()))
            }
            TokenKind::Identifier => {
                let n = self.parse_id()?;
                let base = match n.name.as_str() {
                    "int" => TypeExpr::Basic(BasicType::Int, n.span.clone()),
                    "float" => TypeExpr::Basic(BasicType::Float, n.span.clone()),
                    "str" => TypeExpr::Basic(BasicType::Str, n.span.clone()),
                    "bool" => TypeExpr::Basic(BasicType::Bool, n.span.clone()),
                    "null" => TypeExpr::Basic(BasicType::Null, n.span.clone()),
                    "void" => TypeExpr::Basic(BasicType::Void, n.span.clone()),
                    "any" => TypeExpr::Basic(BasicType::Any, n.span.clone()),
                    "u8" => TypeExpr::UnsignedInt(8, n.span.clone()),
                    "u16" => TypeExpr::UnsignedInt(16, n.span.clone()),
                    "u32" => TypeExpr::UnsignedInt(32, n.span.clone()),
                    "u64" => TypeExpr::UnsignedInt(64, n.span.clone()),
                    "ptr" => {
                        if self.current_kind() == TokenKind::LBracket {
                            self.advance();
                            let inner = self.parse_type_expr()?;
                            self.expect(TokenKind::RBracket)?;
                            return Ok(TypeExpr::Pointer(Box::new(inner), Span::dummy()));
                        }
                        TypeExpr::Named(n.clone(), n.span.clone())
                    }
                    "Result" => {
                        if self.current_kind() == TokenKind::LBracket {
                            self.advance();
                            let ok_ty = self.parse_type_expr()?;
                            let err_ty = if self.current_kind() == TokenKind::Comma { self.advance(); self.parse_type_expr()? } else { TypeExpr::Basic(BasicType::Str, Span::dummy()) };
                            self.expect(TokenKind::RBracket)?;
                            return Ok(TypeExpr::Result(Box::new(ok_ty), Box::new(err_ty), Span::dummy()));
                        }
                        TypeExpr::Named(n.clone(), n.span.clone())
                    }
                    _ => TypeExpr::Named(n.clone(), n.span.clone()),
                };
                // Check for generics: Base[Args]
                if self.current_kind() == TokenKind::LBracket {
                    self.advance();
                    let mut args = vec![];
                    while self.current_kind() != TokenKind::RBracket && !self.is_at_end() {
                        args.push(self.parse_type_expr()?);
                        if self.current_kind() == TokenKind::Comma {
                            self.advance();
                        }
                    }
                    self.expect(TokenKind::RBracket)?;
                    let g = TypeExpr::Generic {
                        base: Box::new(base),
                        args,
                        span: Span::dummy(),
                    };
                    if self.current_kind() == TokenKind::Question {
                        self.advance();
                        Ok(TypeExpr::Optional(Box::new(g), Span::dummy()))
                    } else {
                        Ok(g)
                    }
                } else if self.current_kind() == TokenKind::Question {
                    self.advance();
                    Ok(TypeExpr::Optional(Box::new(base), Span::dummy()))
                } else {
                    Ok(base)
                }
            }
            TokenKind::Fn => {
                self.advance();
                self.expect(TokenKind::LParen)?;
                let mut p = vec![];
                if self.current_kind() != TokenKind::RParen {
                    p.push(self.parse_type_expr()?);
                    while self.current_kind() == TokenKind::Comma {
                        self.advance();
                        p.push(self.parse_type_expr()?);
                    }
                }
                self.expect(TokenKind::RParen)?;
                self.expect(TokenKind::Colon)?;
                let r = Box::new(self.parse_type_expr()?);
                Ok(TypeExpr::Function {
                    params: p,
                    return_type: r,
                    span: Span::dummy(),
                })
            }
            _ => Err(self.err("Expected type")),
        }
    }

    fn parse_args(&mut self) -> Result<Vec<Expr>> {
        let mut a = vec![];
        if self.current_kind() == TokenKind::RParen {
            self.advance();
            return Ok(a);
        }
        a.push(self.parse_expr()?);
        while self.current_kind() == TokenKind::Comma {
            self.advance();
            if self.current_kind() == TokenKind::RParen {
                break;
            }
            a.push(self.parse_expr()?);
        }
        self.expect(TokenKind::RParen)?;
        Ok(a)
    }

    /// Parse a single optional-chain link that immediately follows the `?.`
    /// operator. The link is one of:
    ///   - `?.field`        → `OptionalChainLink::Member(field)`
    ///   - `?.method(args)` → `OptionalChainLink::Call { method, args }`
    ///   - `?[index]`       → `OptionalChainLink::Index(index)`
    ///
    /// The `?.` token itself has already been consumed by the caller.
    fn parse_optional_chain_link(&mut self) -> Result<OptionalChainLink> {
        if self.current_kind() == TokenKind::LBracket {
            self.advance();
            let idx = self.parse_expr()?;
            self.expect(TokenKind::RBracket)?;
            Ok(OptionalChainLink::Index(idx))
        } else {
            let member = self.parse_id()?;
            if self.current_kind() == TokenKind::LParen {
                self.advance();
                let args = self.parse_args()?;
                Ok(OptionalChainLink::Call { method: member, args })
            } else {
                Ok(OptionalChainLink::Member(member))
            }
        }
    }

    fn parse_args_no_paren(&mut self) -> Result<Vec<Expr>> {
        let mut a = vec![];
        a.push(self.parse_expr()?);
        while self.current_kind() == TokenKind::Comma {
            self.advance();
            a.push(self.parse_expr()?);
        }
        Ok(a)
    }
    fn parse_exprs_semi(&mut self) -> Result<Vec<Expr>> {
        let mut a = vec![];
        if !self.is_at_nl() && !self.is_at_end() {
            a.push(self.parse_expr()?);
            while self.current_kind() == TokenKind::Semicolon {
                self.advance();
                a.push(self.parse_expr()?);
            }
        }
        self.nl();
        Ok(a)
    }
    fn parse_exprs_comma(&mut self) -> Result<Vec<Expr>> {
        let mut a = vec![];
        if !self.is_at_nl() && !self.is_at_end() {
            a.push(self.parse_expr()?);
            while self.current_kind() == TokenKind::Comma {
                self.advance();
                if self.is_at_nl() || self.is_at_end() {
                    break;
                }
                a.push(self.parse_expr()?);
            }
        }
        self.nl();
        Ok(a)
    }
    fn parse_until_nl(&mut self) -> Result<Vec<Stmt>> {
        let mut v = vec![];
        while !self.is_at_nl() && !self.is_at_end() {
            v.push(self.parse_stmt()?);
        }
        Ok(v)
    }

    fn cur_tok(&self) -> &Token {
        &self.tokens[self.pos]
    }
    fn current_kind(&self) -> TokenKind {
        if self.pos < self.tokens.len() {
            self.tokens[self.pos].kind.clone()
        } else {
            TokenKind::EOF
        }
    }
    fn lex(&self) -> &str {
        if self.pos < self.tokens.len() {
            &self.tokens[self.pos].lexeme
        } else {
            ""
        }
    }
    fn peek_kind(&self) -> Option<TokenKind> {
        if self.pos + 1 < self.tokens.len() {
            Some(self.tokens[self.pos + 1].kind.clone())
        } else {
            None
        }
    }
    fn advance(&mut self) {
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
    }
    fn adv_tok(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        self.pos += 1;
        t
    }
    fn span(&self) -> Span {
        if self.pos < self.tokens.len() {
            self.tokens[self.pos].span.clone()
        } else {
            Span::dummy()
        }
    }
    fn is_at_end(&self) -> bool {
        self.pos >= self.tokens.len() || self.current_kind() == TokenKind::EOF
    }
    fn is_at_nl(&self) -> bool {
        self.is_at_end()
            || matches!(
                self.current_kind(),
                TokenKind::Newline
                    | TokenKind::Comment
                    | TokenKind::DocComment
                    | TokenKind::MultilineComment
            )
    }
    fn skip_newlines(&mut self) {
        while matches!(
            self.current_kind(),
            TokenKind::Newline
                | TokenKind::Comment
                | TokenKind::DocComment
                | TokenKind::MultilineComment
        ) {
            self.advance();
        }
    }
    fn nl(&mut self) {
        if self.current_kind() == TokenKind::Semicolon {
            self.advance();
        }
        self.skip_newlines();
    }
    fn expect(&mut self, k: TokenKind) -> Result<()> {
        if self.current_kind() == k {
            self.advance();
            Ok(())
        } else {
            Err(self.err(&format!("Expected {:?}", k)))
        }
    }
    fn expect_kw(&mut self, k: TokenKind) -> Result<()> {
        self.expect(k)
    }
    fn expect_ident(&mut self, s: &str) -> Result<()> {
        if self.current_kind() == TokenKind::Identifier && self.lex() == s {
            self.advance();
            Ok(())
        } else {
            Err(self.err(&format!("Expected '{}'", s)))
        }
    }
    fn expect_str(&mut self) -> Result<String> {
        match self.current_kind() {
            TokenKind::StringLiteral => {
                let l = self.lex().to_string();
                self.advance();
                Ok(l)
            }
            _ => Err(self.err("Expected string")),
        }
    }
    fn parse_id(&mut self) -> Result<Identifier> {
        let t = self.adv_tok();
        Ok(Identifier {
            name: t.lexeme,
            span: t.span,
        })
    }
    fn parse_string_parts(&mut self, raw: &str) -> Result<Vec<StringPart>> {
        let mut parts = Vec::new();
        let mut text_buf = String::new();
        let mut chars = raw.chars().peekable();
        let mut in_brace = false;
        let mut expr_buf = String::new();
        let mut brace_depth = 0;

        while let Some(ch) = chars.next() {
            if ch == '{' {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    text_buf.push('{');
                    continue;
                }
                if in_brace {
                    brace_depth += 1;
                    expr_buf.push(ch);
                    continue;
                }
                in_brace = true;
                brace_depth = 1;
                if !text_buf.is_empty() {
                    parts.push(StringPart::Text(text_buf));
                    text_buf = String::new();
                }
            } else if ch == '}' {
                if chars.peek() == Some(&'}') {
                    chars.next();
                    text_buf.push('}');
                    continue;
                }
                if in_brace {
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        in_brace = false;
                        let expr_str = expr_buf.trim();
                        if !expr_str.is_empty() {
                            let file_id = self.tokens[self.pos.min(self.tokens.len() - 1)]
                                .span
                                .file_id;
                            let mut sub_lexer = crate::lexer::Lexer::new(expr_str, file_id);
                            match sub_lexer.tokenize() {
                                Ok(sub_tokens) => {
                                    let mut sub_parser = Parser::new(sub_tokens, 0);
                                    match sub_parser.parse_expr() {
                                        Ok(expr) => {
                                            parts.push(StringPart::Interpolation(expr));
                                        }
                                        Err(_) => {
                                            // Interpolation parse failed: treat the
                                            // original {expr} as literal text.
                                            text_buf.push('{');
                                            text_buf.push_str(expr_str);
                                            text_buf.push('}');
                                        }
                                    }
                                }
                                Err(_) => {
                                    // Lex failed: treat as literal text.
                                    text_buf.push('{');
                                    text_buf.push_str(expr_str);
                                    text_buf.push('}');
                                }
                            }
                        }
                        expr_buf = String::new();
                    } else {
                        expr_buf.push(ch);
                    }
                } else {
                    text_buf.push(ch);
                }
            } else if in_brace {
                expr_buf.push(ch);
            } else {
                text_buf.push(ch);
            }
        }

        if in_brace && !expr_buf.is_empty() {
            // Unterminated interpolation: treat as literal text.
            text_buf.push('{');
            text_buf.push_str(&expr_buf);
        }
        if !text_buf.is_empty() {
            parts.push(StringPart::Text(text_buf));
        }
        if parts.is_empty() {
            parts.push(StringPart::Text(String::new()));
        }
        Ok(parts)
    }

    fn err(&self, m: &str) -> CompilerError {
        CompilerError::parse_error(m.to_string(), self.span())
    }
}

fn parse_i64_literal(raw: &str) -> Option<i64> {
    let cleaned: String = raw.chars().filter(|c| *c != '_').collect();
    let (sign, body) = if let Some(rest) = cleaned.strip_prefix('-') {
        (-1i64, rest)
    } else {
        (1i64, cleaned.as_str())
    };
    let parsed = if let Some(rest) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i64::from_str_radix(rest, 16).ok()
    } else if let Some(rest) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
        i64::from_str_radix(rest, 2).ok()
    } else if let Some(rest) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
        i64::from_str_radix(rest, 8).ok()
    } else {
        body.parse::<i64>().ok()
    }?;
    parsed.checked_mul(sign)
}

fn prec(k: &TokenKind) -> usize {
    match k {
        TokenKind::DotDot | TokenKind::DotDotDot => 0,
        TokenKind::As => 1,
        TokenKind::Question => 1,
        TokenKind::PipeArrow => 2,
        TokenKind::QuestionQuestion => 3,
        TokenKind::Or => 4,
        TokenKind::And => 5,
        TokenKind::Eq
        | TokenKind::Ne
        | TokenKind::Lt
        | TokenKind::Gt
        | TokenKind::Le
        | TokenKind::Ge
        | TokenKind::Is
        | TokenKind::In => 6,
        TokenKind::Plus | TokenKind::Minus => 7,
        TokenKind::Star
        | TokenKind::Slash
        | TokenKind::Percent
        | TokenKind::FloorDiv
        | TokenKind::Repeated => 8,
        TokenKind::Power => 9,
        TokenKind::Hash
        | TokenKind::Tilde
        | TokenKind::Caret
        | TokenKind::Underscore
        | TokenKind::QuestionDot => 8,
        TokenKind::Dot | TokenKind::LBracket => 9,
        _ => 0,
    }
}

fn binop(k: &TokenKind) -> BinaryOp {
    match k {
        TokenKind::And => BinaryOp::And,
        TokenKind::Or => BinaryOp::Or,
        TokenKind::Eq => BinaryOp::Eq,
        TokenKind::Ne => BinaryOp::Ne,
        TokenKind::Lt => BinaryOp::Lt,
        TokenKind::Gt => BinaryOp::Gt,
        TokenKind::Le => BinaryOp::Le,
        TokenKind::Ge => BinaryOp::Ge,
        TokenKind::Is => BinaryOp::Is,
        TokenKind::In => BinaryOp::In,
        TokenKind::Plus => BinaryOp::Add,
        TokenKind::Minus => BinaryOp::Sub,
        TokenKind::Star => BinaryOp::Mul,
        TokenKind::Slash => BinaryOp::Div,
        TokenKind::Percent => BinaryOp::Mod,
        TokenKind::FloorDiv => BinaryOp::FloorDiv,
        TokenKind::Power => BinaryOp::Power,
        _ => BinaryOp::Add,
    }
}
