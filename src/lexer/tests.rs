use super::*;

/// 辅助函数：对源码进行词法分析，返回 token 列表
fn tokenize(source: &str) -> Vec<Token> {
    let mut lexer = Lexer::new(source, 0);
    lexer.tokenize().unwrap_or_else(|e| {
        panic!("Lex failed: {:?}", e);
    })
}

/// 辅助函数：只返回 token 的 kind 序列（忽略 Newline/Indent/Dedent/EOF/Comment/DocComment/MultilineComment）
fn kinds(tokens: &[Token]) -> Vec<TokenKind> {
    tokens
        .iter()
        .filter(|t| {
            !matches!(
                t.kind,
                TokenKind::Newline
                    | TokenKind::Indent
                    | TokenKind::Dedent
                    | TokenKind::EOF
                    | TokenKind::Comment
                    | TokenKind::DocComment
                    | TokenKind::MultilineComment
            )
        })
        .map(|t| t.kind.clone())
        .collect()
}

#[test]
fn test_empty_source() {
    let tokens = tokenize("");
    assert_eq!(tokens.last().unwrap().kind, TokenKind::EOF);
}

#[test]
fn test_simple_paste() {
    let source = r#"paste, "Hello, World!""#;
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Paste);
    assert_eq!(k[1], TokenKind::Comma);
    assert_eq!(k[2], TokenKind::StringLiteral);
}

#[test]
fn test_set_instruction() {
    let source = "set, x, 10";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Set);
    assert_eq!(k[1], TokenKind::Comma);
    assert_eq!(k[2], TokenKind::Identifier); // x
    assert_eq!(k[3], TokenKind::Comma);
    assert_eq!(k[4], TokenKind::IntegerLiteral); // 10
}

#[test]
fn test_multiple_args_with_semicolon() {
    let source = r#"paste, "Hello"; " "; "world""#;
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Paste);
    assert_eq!(k[1], TokenKind::Comma);
    assert_eq!(k[2], TokenKind::StringLiteral); // "Hello"
    assert_eq!(k[3], TokenKind::Semicolon);
    assert_eq!(k[4], TokenKind::StringLiteral); // " "
    assert_eq!(k[5], TokenKind::Semicolon);
    assert_eq!(k[6], TokenKind::StringLiteral); // "world"
}

#[test]
fn test_keywords() {
    let keywords = vec![
        "and",
        "as",
        "assert",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "channel",
        "class",
        "close",
        "constexpr",
        "continue",
        "coro",
        "defer",
        "elif",
        "else",
        "enum",
        "export",
        "extends",
        "false",
        "finally",
        "fn",
        "for",
        "from",
        "if",
        "impl",
        "import",
        "in",
        "input",
        "interface",
        "is",
        "lazy",
        "loop",
        "macro",
        "marker",
        "match",
        "new",
        "not",
        "null",
        "or",
        "paste",
        "plugin",
        "pon",
        "println",
        "private",
        "public",
        "receive",
        "repeated",
        "resume",
        "return",
        "select",
        "self",
        "send",
        "set",
        "spawn",
        "step",
        "struct",
        "throw",
        "then",
        "trait",
        "true",
        "try",
        "type",
        "unsafe",
        "while",
        "yield",
    ];

    for kw in keywords {
        let source = format!("{} x", kw);
        let tokens = tokenize(&source);
        let k = kinds(&tokens);
        assert!(
            k[0] != TokenKind::Identifier,
            "Expected keyword token for '{}', got Identifier",
            kw
        );
    }
}

#[test]
fn test_numbers() {
    // 整数
    let tokens = tokenize("set, x, 42");
    let k = kinds(&tokens);
    assert_eq!(k[4], TokenKind::IntegerLiteral);

    // 浮点数
    let tokens = tokenize("set, x, 3.14");
    let k = kinds(&tokens);
    assert_eq!(k[4], TokenKind::FloatLiteral);

    // 科学计数法
    let tokens = tokenize("set, x, 1e10");
    let k = kinds(&tokens);
    assert_eq!(k[4], TokenKind::FloatLiteral);

    // 十六进制
    let tokens = tokenize("set, x, 0xFF");
    let k = kinds(&tokens);
    assert_eq!(k[4], TokenKind::IntegerLiteral);
}

#[test]
fn test_string_escape() {
    let source = r#"paste, "Hello\nWorld\t!""#;
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[2], TokenKind::StringLiteral);
}

#[test]
fn test_multiline_string() {
    let source = "\"\"\"\nline 1\nline 2\n\"\"\"";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::MultiLineString);
}

#[test]
fn test_comments() {
    // 单行注释
    let source = "# this is a comment\nset, x, 10";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Set);

    // 文档注释
    let source = "## doc comment\nset, x, 10";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Set);
}

#[test]
fn test_multiline_comment() {
    let source = "#* this is a\n   multiline comment *#\nset, x, 10";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Set);
}

#[test]
fn test_directive_block_set() {
    let source = "/set\n    x, 10\n    y, 20\n/end";
    let tokens = tokenize(source);
    let k = kinds(&tokens);

    // DirectiveOrScope: /set, Indent, Identifier x, Comma, IntegerLiteral 10,
    // Newline, Identifier y, Comma, IntegerLiteral 20, Newline, Dedent, DirectiveOrScope: /end
    assert_eq!(k[0], TokenKind::DirectiveOrScope); // /set
    assert_eq!(k[1], TokenKind::Identifier); // x
    assert_eq!(k[2], TokenKind::Comma);
    assert_eq!(k[3], TokenKind::IntegerLiteral); // 10
    assert_eq!(k[4], TokenKind::Identifier); // y
    assert_eq!(k[5], TokenKind::Comma);
    assert_eq!(k[6], TokenKind::IntegerLiteral); // 20
    assert_eq!(k[7], TokenKind::DirectiveOrScope); // /end
}

#[test]
fn test_if_statement() {
    let source = "if, x > 0\n    paste, \"positive\"\n/end";
    let tokens = tokenize(source);
    let k = kinds(&tokens);

    assert_eq!(k[0], TokenKind::If);
    assert_eq!(k[1], TokenKind::Comma);
    assert_eq!(k[2], TokenKind::Identifier); // x
    assert_eq!(k[3], TokenKind::Gt);
    assert_eq!(k[4], TokenKind::IntegerLiteral); // 0
    assert_eq!(k[5], TokenKind::Paste); // paste
    assert_eq!(k[6], TokenKind::Comma);
    assert_eq!(k[7], TokenKind::StringLiteral); // "positive"
    assert_eq!(k[8], TokenKind::DirectiveOrScope); // /end
}

#[test]
fn test_pipe_operator() {
    let source = "x |> f";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Identifier); // x
    assert_eq!(k[1], TokenKind::PipeArrow); // |>
    assert_eq!(k[2], TokenKind::Identifier); // f
}

#[test]
fn test_optional_chaining() {
    let source = "user?.name ?? \"default\"";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Identifier); // user
    assert_eq!(k[1], TokenKind::QuestionDot); // ?.
    assert_eq!(k[2], TokenKind::Identifier); // name
    assert_eq!(k[3], TokenKind::QuestionQuestion); // ??
    assert_eq!(k[4], TokenKind::StringLiteral); // "default"
}

#[test]
fn test_ternary() {
    let source = "x > 0 ? 1 : 0";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Identifier); // x
    assert_eq!(k[1], TokenKind::Gt);
    assert_eq!(k[2], TokenKind::IntegerLiteral); // 0
    assert_eq!(k[3], TokenKind::Question);
    assert_eq!(k[4], TokenKind::IntegerLiteral); // 1
    assert_eq!(k[5], TokenKind::Colon);
    assert_eq!(k[6], TokenKind::IntegerLiteral); // 0
}

#[test]
fn test_suffix_operators() {
    // #
    let tokens = tokenize("lst#");
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Identifier);
    assert_eq!(k[1], TokenKind::Hash);

    // ~
    let tokens = tokenize("lst~");
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Identifier);
    assert_eq!(k[1], TokenKind::Tilde);

    // ^
    let tokens = tokenize("lst^");
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Identifier);
    assert_eq!(k[1], TokenKind::Caret);

    // _ 是后缀操作符，在标识符后出现
    let tokens = tokenize("lst_");
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Identifier);
    assert_eq!(k[1], TokenKind::Underscore);
}

#[test]
fn test_range_operators() {
    // ..
    let source = "1..5";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::IntegerLiteral);
    assert_eq!(k[1], TokenKind::DotDot);
    assert_eq!(k[2], TokenKind::IntegerLiteral);

    // ...
    let source = "1...5";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[1], TokenKind::DotDotDot);
}

#[test]
fn test_scope_block() {
    let source = "/set\n    /user.\n        name, \"Alice\"\n        age, 30\n    /\n/end";
    let tokens = tokenize(source);
    let k = kinds(&tokens);

    // /set, Indent, /user., Indent, name, Comma, String, Newline, age, Comma, Int,
    // Newline, Dedent, /, Newline, Dedent, /end
    assert_eq!(k[0], TokenKind::DirectiveOrScope); // /set
    assert_eq!(k[1], TokenKind::DirectiveOrScope); // /user.
    let user_token = &tokens.iter().find(|t| t.lexeme == "/user.").unwrap();
    assert!(user_token.is_scope_start());
    assert_eq!(user_token.scope_name(), Some("user"));
}

#[test]
fn test_unicode_identifier() {
    let source = "set, 名字, \"vredrs\"";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[2], TokenKind::Identifier); // 名字
}

#[test]
fn test_operators() {
    let source = "+ - * / // % ** == != < > <= >= = += -= *= /= %=";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Plus);
    assert_eq!(k[1], TokenKind::Minus);
    assert_eq!(k[2], TokenKind::Star);
    assert_eq!(k[3], TokenKind::Slash);
    assert_eq!(k[4], TokenKind::FloorDiv);
    assert_eq!(k[5], TokenKind::Percent);
    assert_eq!(k[6], TokenKind::Power);
    assert_eq!(k[7], TokenKind::Eq);
    assert_eq!(k[8], TokenKind::Ne);
    assert_eq!(k[9], TokenKind::Lt);
    assert_eq!(k[10], TokenKind::Gt);
    assert_eq!(k[11], TokenKind::Le);
    assert_eq!(k[12], TokenKind::Ge);
    assert_eq!(k[13], TokenKind::Assign);
    assert_eq!(k[14], TokenKind::PlusAssign);
    assert_eq!(k[15], TokenKind::MinusAssign);
    assert_eq!(k[16], TokenKind::StarAssign);
    assert_eq!(k[17], TokenKind::TableAssign);
    assert_eq!(k[18], TokenKind::PercentAssign);
}

#[test]
fn test_brackets() {
    let source = "( ) [ ] { }";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::LParen);
    assert_eq!(k[1], TokenKind::RParen);
    assert_eq!(k[2], TokenKind::LBracket);
    assert_eq!(k[3], TokenKind::RBracket);
    assert_eq!(k[4], TokenKind::LBrace);
    assert_eq!(k[5], TokenKind::RBrace);
}

#[test]
fn test_fn_definition() {
    let source = "fn, add(a, b)\n    return, a + b\n/end";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Fn);
    assert_eq!(k[1], TokenKind::Comma);
    assert_eq!(k[2], TokenKind::Identifier); // add
    assert_eq!(k[3], TokenKind::LParen);
    assert_eq!(k[4], TokenKind::Identifier); // a
    assert_eq!(k[5], TokenKind::Comma);
    assert_eq!(k[6], TokenKind::Identifier); // b
    assert_eq!(k[7], TokenKind::RParen);
    assert_eq!(k[8], TokenKind::Return);
}

#[test]
fn test_annotation() {
    let source = "@deprecated(\"use new_func\")\nfn, old()\n/end";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::At);
    assert_eq!(k[1], TokenKind::Identifier); // deprecated
}

#[test]
fn test_pon() {
    let source = "pon; name, \"Alice\"";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Pon);
    assert_eq!(k[1], TokenKind::Semicolon);
}

#[test]
fn test_match_expression() {
    let source = "match, value\n    case, 0; paste, \"zero\"\n    case, _; paste, \"other\"\n/end";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Match);
    assert_eq!(k[1], TokenKind::Comma);
    assert_eq!(k[2], TokenKind::Identifier); // value
    assert_eq!(k[3], TokenKind::Case);
    assert_eq!(k[4], TokenKind::Comma);
    assert_eq!(k[5], TokenKind::IntegerLiteral); // 0
    assert_eq!(k[6], TokenKind::Semicolon);
    assert_eq!(k[7], TokenKind::Paste);
}

#[test]
fn test_channel_and_concurrency() {
    let source = "set, ch = channel(int, 0)\nsend(ch, 42)\nset, v = receive(ch)";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Set);
    // find channel keyword
    assert!(k.contains(&TokenKind::Channel));
    assert!(k.contains(&TokenKind::Send));
    assert!(k.contains(&TokenKind::Receive));
}

#[test]
fn test_spawn_coroutine() {
    let source = "spawn, fn()\n    paste, \"hello\"\n/end";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[0], TokenKind::Spawn);
    assert_eq!(k[1], TokenKind::Comma);
    assert_eq!(k[2], TokenKind::Fn);
}

#[test]
fn test_table_assign() {
    // /= 作为表格赋值开始符（在 /set 块内）
    let source = "/=\n1; \"Alice\"; 95\n/";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    // /= 是一个 token，因为它在行首
    assert_eq!(k[0], TokenKind::TableAssign);
}

#[test]
fn test_error_unterminated_string() {
    let source = "\"unterminated";
    let mut lexer = Lexer::new(source, 0);
    let result = lexer.tokenize();
    assert!(result.is_err());
}

#[test]
fn test_error_unterminated_multiline_string() {
    let source = "\"\"\"\nunterminated multiline";
    let mut lexer = Lexer::new(source, 0);
    let result = lexer.tokenize();
    assert!(result.is_err());
}

#[test]
fn test_select_with_cases() {
    let source = "select\n    case, receive(ch), v; paste, v\n    else; paste, \"none\"\n/end";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert!(k.contains(&TokenKind::Select));
    assert!(k.contains(&TokenKind::Case));
    assert!(k.contains(&TokenKind::Else));
}

#[test]
fn test_async_await() {
    let source =
        "async fn, fetch(url)\n    set, resp = await http.get(url)\n    return, resp\n/end";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert!(k.contains(&TokenKind::Async));
    assert!(k.contains(&TokenKind::Await));
}

#[test]
fn test_linear_type() {
    let source = "type, FileHandle = linear struct\n/end";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert!(k.contains(&TokenKind::Type));
    assert!(k.contains(&TokenKind::Struct));
}

#[test]
fn test_trait_and_impl() {
    let source = "trait Drawable\n    fn, draw(self)\n/end\nimpl Drawable for Circle\n/end";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert!(k.contains(&TokenKind::Trait));
    assert!(k.contains(&TokenKind::Impl));
    assert!(k.contains(&TokenKind::For));
}

#[test]
fn test_nullish_coalescing() {
    let source = "a ?? b";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert_eq!(k[1], TokenKind::QuestionQuestion);
}

#[test]
fn test_repeated_keyword() {
    let source = "\"*\" repeated 5";
    let tokens = tokenize(source);
    let k = kinds(&tokens);
    assert!(k.contains(&TokenKind::Repeated));
}

#[test]
fn test_full_program() {
    let source = r##"
## 测试程序
fn, main()
    paste, "Hello, vredrs!"
    set, x = 42
    if, x > 0
        paste, "positive"
    else
        paste, "non-positive"
    /end
    for, i, in, 1..5
        paste, i
    /end
/end
"##;
    let mut lexer = Lexer::new(source, 0);
    let result = lexer.tokenize();
    assert!(result.is_ok(), "Full program should lex without errors");
    let tokens = result.unwrap();
    let k = kinds(&tokens);
    assert!(k.contains(&TokenKind::Fn));
    assert!(k.contains(&TokenKind::If));
    assert!(k.contains(&TokenKind::Else));
    assert!(k.contains(&TokenKind::For));
    assert!(k.contains(&TokenKind::In));
    assert!(k.contains(&TokenKind::DotDot));
}

#[test]
fn test_magic_method_identifiers_keep_closing_double_underscore() {
    let tokens = tokenize("fn, __add__(other)\n/end");
    let id = tokens
        .iter()
        .find(|t| t.lexeme == "__add__")
        .expect("__add__ must be lexed as one token");
    assert_eq!(id.kind, TokenKind::Identifier);
    assert!(!tokens.iter().any(|t| t.lexeme == "__add_"));

    let tokens = tokenize("set, x = obj.__getitem__(0)");
    let id = tokens
        .iter()
        .find(|t| t.lexeme == "__getitem__")
        .expect("__getitem__ must be lexed as one token");
    assert_eq!(id.kind, TokenKind::Identifier);
}

#[test]
fn test_normal_trailing_underscore_still_postfix_operator() {
    let tokens = tokenize("lst_");
    let k = kinds(&tokens);
    assert_eq!(k, vec![TokenKind::Identifier, TokenKind::Underscore]);
    let visible: Vec<_> = tokens
        .iter()
        .filter(|t| !matches!(t.kind, TokenKind::Newline | TokenKind::EOF))
        .map(|t| (t.kind.clone(), t.lexeme.clone()))
        .collect();
    assert_eq!(visible[0], (TokenKind::Identifier, "lst".to_string()));
    assert_eq!(visible[1], (TokenKind::Underscore, "_".to_string()));
}
