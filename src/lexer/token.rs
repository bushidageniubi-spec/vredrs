use crate::error::Span;

/// vredrs 1.0 所有 Token 类型
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TokenKind {
    // ===== 关键字 =====
    And,        // and
    As,         // as
    Assert,     // assert
    Async,      // async
    Await,      // await
    Break,      // break
    Case,       // case
    Catch,      // catch
    Channel,    // channel
    Class,      // class
    Close,      // close
    Constexpr,  // constexpr
    Continue,   // continue
    Coro,       // coro
    Defer,      // defer
    Dtor,       // dtor
    Directive,  // directive
    Elif,       // elif
    Else,       // else
    Enum,       // enum
    Export,     // export
    Extern,     // extern
    Extends,    // extends
    FalseKw,    // false
    Finally,    // finally
    Fn,         // fn
    For,        // for
    From,       // from
    If,         // if
    Impl,       // impl
    Implements, // implements
    Import,     // import
    In,         // in
    Input,      // input
    Interface,  // interface
    Is,         // is
    Lazy,       // lazy
    Loop,       // loop
    Macro,      // macro
    Marker,     // marker
    Match,      // match
    New,        // new
    Not,        // not
    Null,       // null
    Or,         // or
    Paste,      // paste
    Plugin,     // plugin
    Pon,        // pon
    Println,    // println
    Private,    // private
    Public,     // public
    Receive,    // receive
    Repeated,   // repeated
    Resume,     // resume
    Return,     // return
    Select,     // select
    SelfKw,     // self
    Send,       // send
    Set,        // set
    Spawn,      // spawn
    Step,       // step
    Struct,     // struct
    Test,       // test
    Bench,      // bench
    Throw,      // throw
    Then,       // then
    Trait,      // trait
    TrueKw,     // true
    Try,        // try
    Type,       // type
    Unsafe,     // unsafe
    While,      // while
    With,       // with
    Yield,      // yield

    // ===== 字面量 =====
    IntegerLiteral,
    FloatLiteral,
    StringLiteral,   // 原始内容（不含引号），含转义和插值语法
    MultiLineString, // 原始内容（不含三引号）

    // ===== 标识符 =====
    Identifier,

    // ===== 指令/作用域块（仅出现在行首） =====
    DirectiveOrScope, // /set, /end, /alan., /config.server. 等
    TableAssign,      // /= （仅出现在行首的 /set 块内）

    // ===== 分隔符号 =====
    Comma,            // ,
    Semicolon,        // ;
    Colon,            // :
    Dot,              // .
    DotDot,           // ..
    DotDotDot,        // ...
    At,               // @
    Question,         // ?
    QuestionDot,      // ?.
    QuestionQuestion, // ??
    PipeArrow,        // |>
    Bang,             // !
    Hash,             // # （后缀操作符，在非行首时出现）
    Tilde,            // ~
    Caret,            // ^
    Underscore,       // _ （通配符或后缀操作符）
    /// Lifetime parameter: `'a`, `'b`, `'static`, etc.
    /// In Vredrs, lifetimes are optional annotations that the AOR
    /// (Adaptive Ownership Regions) system uses as hints. The compiler
    /// does NOT require them — when omitted, AOR infers or falls back
    /// to arena allocation. When present, they guide the borrow checker
    /// to use a tighter scope for variable release.
    Lifetime(String),

    // ===== 括号 =====
    LParen,   // (
    RParen,   // )
    LBracket, // [
    RBracket, // ]
    LBrace,   // {
    RBrace,   // }

    // ===== 运算符 =====
    Plus,          // +
    Minus,         // -
    BitAnd,        // & (borrow, 0.1.4)
    Star,          // *
    Slash,         // / （除法或作用域结束）
    Percent,       // %
    Power,         // **
    FloorDiv,      // //
    Eq,            // ==
    Ne,            // !=
    Lt,            // <
    Gt,            // >
    Le,            // <=
    Ge,            // >=
    Assign,        // =
    PlusAssign,    // +=
    MinusAssign,   // -=
    StarAssign,    // *=
    SlashAssign,   // /=
    PercentAssign, // %=

    // ===== 注释 =====
    Comment,          // # 单行注释
    MultilineComment, // #* ... *#
    DocComment,       // ## 文档注释

    // ===== 换行与缩进 =====
    Newline,
    Indent,
    Dedent,

    // ===== 特殊 =====
    EOF,
}

/// 一个词法单元
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    pub lexeme: String,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span, lexeme: String) -> Self {
        Token { kind, span, lexeme }
    }

    /// 是否是指令块或作用域块的开始
    pub fn is_directive_start(&self, name: &str) -> bool {
        if self.kind != TokenKind::DirectiveOrScope {
            return false;
        }
        // lexeme 如 "/set" 或 "/alan."
        let content = &self.lexeme[1..]; // 去掉 '/'
        content == name
    }

    /// 是否是作用域块开始（/对象名.）
    pub fn is_scope_start(&self) -> bool {
        if self.kind != TokenKind::DirectiveOrScope {
            return false;
        }
        self.lexeme.ends_with('.') && self.lexeme.len() > 2
    }

    /// 获取作用域块名称（去掉 / 和 .）
    pub fn scope_name(&self) -> Option<&str> {
        if self.is_scope_start() {
            let content = &self.lexeme[1..]; // 去掉 /
            Some(&content[..content.len() - 1]) // 去掉 .
        } else {
            None
        }
    }

    /// 是否是指令块结束 /end
    pub fn is_block_end(&self) -> bool {
        self.kind == TokenKind::DirectiveOrScope && self.lexeme == "/end"
    }
}

/// 所有 vredrs 关键字 -> TokenKind 映射
pub fn lookup_keyword(word: &str) -> Option<TokenKind> {
    match word {
        "and" => Some(TokenKind::And),
        "as" => Some(TokenKind::As),
        "assert" => Some(TokenKind::Assert),
        "async" => Some(TokenKind::Async),
        "await" => Some(TokenKind::Await),
        "break" => Some(TokenKind::Break),
        "case" => Some(TokenKind::Case),
        "catch" => Some(TokenKind::Catch),
        "channel" => Some(TokenKind::Channel),
        "class" => Some(TokenKind::Class),
        "close" => Some(TokenKind::Close),
        "constexpr" => Some(TokenKind::Constexpr),
        "continue" => Some(TokenKind::Continue),
        "coro" => Some(TokenKind::Coro),
        "defer" => Some(TokenKind::Defer),
        "dtor" => Some(TokenKind::Dtor),
        "directive" => Some(TokenKind::Directive),
        "elif" => Some(TokenKind::Elif),
        "else" => Some(TokenKind::Else),
        "enum" => Some(TokenKind::Enum),
        "export" => Some(TokenKind::Export),
        "extern" => Some(TokenKind::Extern),
        "extends" => Some(TokenKind::Extends),
        "false" => Some(TokenKind::FalseKw),
        "finally" => Some(TokenKind::Finally),
        "fn" => Some(TokenKind::Fn),
        "for" => Some(TokenKind::For),
        "from" => Some(TokenKind::From),
        "if" => Some(TokenKind::If),
        "impl" => Some(TokenKind::Impl),
        "implements" => Some(TokenKind::Implements),
        "import" => Some(TokenKind::Import),
        "in" => Some(TokenKind::In),
        "input" => Some(TokenKind::Input),
        "interface" => Some(TokenKind::Interface),
        "is" => Some(TokenKind::Is),
        "lazy" => Some(TokenKind::Lazy),
        "loop" => Some(TokenKind::Loop),
        "macro" => Some(TokenKind::Macro),
        "marker" => Some(TokenKind::Marker),
        "match" => Some(TokenKind::Match),
        "new" => Some(TokenKind::New),
        "not" => Some(TokenKind::Not),
        "null" => Some(TokenKind::Null),
        "or" => Some(TokenKind::Or),
        "paste" => Some(TokenKind::Paste),
        "plugin" => Some(TokenKind::Plugin),
        "pon" => Some(TokenKind::Pon),
        "println" => Some(TokenKind::Println),
        "private" => Some(TokenKind::Private),
        "public" => Some(TokenKind::Public),
        "receive" => Some(TokenKind::Receive),
        "repeated" => Some(TokenKind::Repeated),
        "resume" => Some(TokenKind::Resume),
        "ret" => Some(TokenKind::Return),
        "return" => Some(TokenKind::Return),
        "select" => Some(TokenKind::Select),
        "self" => Some(TokenKind::SelfKw),
        "send" => Some(TokenKind::Send),
        "set" => Some(TokenKind::Set),
        "spawn" => Some(TokenKind::Spawn),
        "step" => Some(TokenKind::Step),
        "struct" => Some(TokenKind::Struct),
        "test" => Some(TokenKind::Test),
        "bench" => Some(TokenKind::Bench),
        "throw" => Some(TokenKind::Throw),
        "then" => Some(TokenKind::Then),
        "trait" => Some(TokenKind::Trait),
        "true" => Some(TokenKind::TrueKw),
        "try" => Some(TokenKind::Try),
        "type" => Some(TokenKind::Type),
        "unsafe" => Some(TokenKind::Unsafe),
        "while" => Some(TokenKind::While),
        "with" => Some(TokenKind::With),
        "yield" => Some(TokenKind::Yield),
        _ => None,
    }
}

impl Token {
    /// 获取 token 的描述（用于错误消息）
    pub fn describe(&self) -> String {
        match &self.kind {
            TokenKind::Identifier => format!("identifier '{}'", self.lexeme),
            TokenKind::IntegerLiteral => format!("integer '{}'", self.lexeme),
            TokenKind::FloatLiteral => format!("float '{}'", self.lexeme),
            TokenKind::StringLiteral => "string literal".to_string(),
            TokenKind::MultiLineString => "multiline string".to_string(),
            TokenKind::DirectiveOrScope => format!("directive '{}'", self.lexeme),
            TokenKind::TableAssign => "table assignment '/='".to_string(),
            TokenKind::Comment => "comment".to_string(),
            TokenKind::MultilineComment => "multiline comment".to_string(),
            TokenKind::DocComment => "doc comment".to_string(),
            TokenKind::Newline => "newline".to_string(),
            TokenKind::Indent => "indent".to_string(),
            TokenKind::Dedent => "dedent".to_string(),
            TokenKind::EOF => "end of file".to_string(),
            _ => format!("'{}'", self.lexeme),
        }
    }
}
