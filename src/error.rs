use crate::source_manager::SourceManager;
use std::fmt;
use std::hash::{Hash, Hasher};

/// 源码位置跨度
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub start: usize,   // 字节偏移起始
    pub end: usize,     // 字节偏移结束（不包含）
    pub line: usize,    // 行号（从1开始）
    pub column: usize,  // 列号（从1开始，字节偏移）
    pub file_id: usize, // 文件ID（用于多文件编译）
}

impl Span {
    pub fn new(start: usize, end: usize, line: usize, column: usize, file_id: usize) -> Self {
        Span {
            start,
            end,
            line,
            column,
            file_id,
        }
    }

    /// 创建一个虚拟位置（用于测试或内部生成）
    pub fn dummy() -> Self {
        Span {
            start: 0,
            end: 0,
            line: 0,
            column: 0,
            file_id: 0,
        }
    }

    /// 合并两个 Span，返回覆盖二者区间的 Span
    pub fn merge(&self, other: &Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
            line: self.line.min(other.line),
            column: if self.line == other.line {
                self.column.min(other.column)
            } else {
                self.column
            },
            file_id: self.file_id,
        }
    }

    pub fn display_file_name(&self) -> String {
        if self.file_id == 0 {
            "main.veds".to_string()
        } else {
            format!("file{}.veds", self.file_id)
        }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.file_id > 0 {
            write!(
                f,
                "{}:{}:{}",
                self.display_file_name(),
                self.line,
                self.column
            )
        } else {
            write!(f, "{}:{}", self.line, self.column)
        }
    }
}

/// 报错码
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    C0001,
    C0002,
    C0003,
    C0010,
    C0020,
    C0021,
    C0030,
    C0031,
    C0040,
    C0041,
    C0050,
    C0051,
    C0060,
    C0061,
    C0070,
    C0080,
    C0081,
    C0090,
    I1001,
    I1002,
    I1003,
    I1004,
    I1005,
    I1006,
    R2001,
    R2002,
    R2003,
    R2004,
    R2005,
    R2006,
    // ── 0.1.4 V-series error codes ──
    // V0001-V0999: Shared (lex/syntax/basic semantic/scope/pattern)
    V0001, V0010, V0020, V0030, V0040, V0050, V0060, V0070,
    // V1000-V1999: .veds interpreter/dynamic
    V1001, V1002, V1003, V1100,
    // V2000-V2999: .vraw static compilation
    V2001, V2002, V2003, V2004, V2005, V2006, V2007, V2008, V2009,
    // V3000-V3999: .cpps bare-metal/hardware
    V3001, V3002, V3003, V3004, V3005, V3006,
    // V4000-V4999: shared file/IO/module
    V4001, V4002, V4003, V4004,
    // V5000-V5999: shared config/build/toolchain
    V5001, V5002, V5003, V5004,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::C0001 => "C0001",
            ErrorCode::C0002 => "C0002",
            ErrorCode::C0003 => "C0003",
            ErrorCode::C0010 => "C0010",
            ErrorCode::C0020 => "C0020",
            ErrorCode::C0021 => "C0021",
            ErrorCode::C0030 => "C0030",
            ErrorCode::C0031 => "C0031",
            ErrorCode::C0040 => "C0040",
            ErrorCode::C0041 => "C0041",
            ErrorCode::C0050 => "C0050",
            ErrorCode::C0051 => "C0051",
            ErrorCode::C0060 => "C0060",
            ErrorCode::C0061 => "C0061",
            ErrorCode::C0070 => "C0070",
            ErrorCode::C0080 => "C0080",
            ErrorCode::C0081 => "C0081",
            ErrorCode::C0090 => "C0090",
            ErrorCode::I1001 => "I1001",
            ErrorCode::I1002 => "I1002",
            ErrorCode::I1003 => "I1003",
            ErrorCode::I1004 => "I1004",
            ErrorCode::I1005 => "I1005",
            ErrorCode::I1006 => "I1006",
            ErrorCode::R2001 => "R2001",
            ErrorCode::R2002 => "R2002",
            ErrorCode::R2003 => "R2003",
            ErrorCode::R2004 => "R2004",
            ErrorCode::R2005 => "R2005",
            ErrorCode::R2006 => "R2006",
            ErrorCode::V0001 => "V0001", ErrorCode::V0010 => "V0010",
            ErrorCode::V0020 => "V0020", ErrorCode::V0030 => "V0030",
            ErrorCode::V0040 => "V0040", ErrorCode::V0050 => "V0050",
            ErrorCode::V0060 => "V0060", ErrorCode::V0070 => "V0070",
            ErrorCode::V1001 => "V1001", ErrorCode::V1002 => "V1002",
            ErrorCode::V1003 => "V1003", ErrorCode::V1100 => "V1100",
            ErrorCode::V2001 => "V2001", ErrorCode::V2002 => "V2002",
            ErrorCode::V2003 => "V2003", ErrorCode::V2004 => "V2004",
            ErrorCode::V2005 => "V2005", ErrorCode::V2006 => "V2006",
            ErrorCode::V2007 => "V2007", ErrorCode::V2008 => "V2008",
            ErrorCode::V2009 => "V2009",
            ErrorCode::V3001 => "V3001", ErrorCode::V3002 => "V3002",
            ErrorCode::V3003 => "V3003", ErrorCode::V3004 => "V3004",
            ErrorCode::V3005 => "V3005", ErrorCode::V3006 => "V3006",
            ErrorCode::V4001 => "V4001", ErrorCode::V4002 => "V4002",
            ErrorCode::V4003 => "V4003", ErrorCode::V4004 => "V4004",
            ErrorCode::V5001 => "V5001", ErrorCode::V5002 => "V5002",
            ErrorCode::V5003 => "V5003", ErrorCode::V5004 => "V5004",
        }
    }

    pub fn category(&self) -> &'static str {
        match self {
            ErrorCode::C0001 | ErrorCode::C0002 | ErrorCode::C0003 => "Lexical Error",
            ErrorCode::C0010 | ErrorCode::C0020 | ErrorCode::C0021 => "Syntax Error",
            ErrorCode::C0030
            | ErrorCode::C0031
            | ErrorCode::C0050
            | ErrorCode::C0051
            | ErrorCode::C0060
            | ErrorCode::C0061
            | ErrorCode::C0070 => "Semantic Error",
            ErrorCode::C0040 | ErrorCode::C0041 => "Type Error",
            ErrorCode::C0080 | ErrorCode::C0081 | ErrorCode::C0090 => "Codegen Error",
            ErrorCode::I1001
            | ErrorCode::I1002
            | ErrorCode::I1003
            | ErrorCode::I1004
            | ErrorCode::I1005
            | ErrorCode::I1006 => "Interpretation Error",
            ErrorCode::R2001
            | ErrorCode::R2002
            | ErrorCode::R2003
            | ErrorCode::R2004
            | ErrorCode::R2005
            | ErrorCode::R2006 => "Runtime Error",
            ErrorCode::V0001 => "Lexical Error",
            ErrorCode::V0010 | ErrorCode::V0020 => "Syntax Error",
            ErrorCode::V0030 => "Type Error",
            ErrorCode::V0040 | ErrorCode::V0060 => "Scope Error",
            ErrorCode::V0050 => "Semantic Error", ErrorCode::V0070 => "Pattern Error",
            ErrorCode::V1001 | ErrorCode::V1002 | ErrorCode::V1003 | ErrorCode::V1100 => "Runtime Error",
            ErrorCode::V2001 => "Ownership Error",
            ErrorCode::V2002 | ErrorCode::V2003 => "Borrow Error",
            ErrorCode::V2004 => "Lifetime Error", ErrorCode::V2005 => "Linear Type Error",
            ErrorCode::V2006 => "Static Type Error", ErrorCode::V2007 => "Constexpr Error",
            ErrorCode::V2008 => "Layout Error", ErrorCode::V2009 => "Raw Mode Error",
            ErrorCode::V3001 => "Interrupt Error", ErrorCode::V3002 => "Memory Map Error",
            ErrorCode::V3003 => "DMA Error", ErrorCode::V3004 => "Linker Error",
            ErrorCode::V3005 => "Linear Type Error", ErrorCode::V3006 => "Layout Error",
            ErrorCode::V4001 | ErrorCode::V4002 | ErrorCode::V4003 | ErrorCode::V4004 => "File/Module Error",
            ErrorCode::V5001 | ErrorCode::V5002 | ErrorCode::V5003 | ErrorCode::V5004 => "Build Error",
        }
    }
}

/// 编译器错误
#[derive(Debug, Clone)]
pub struct CompilerError {
    pub message: String,
    pub span: Option<Span>,
    pub kind: ErrorKind,
    pub code: ErrorCode,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    LexError,
    ParseError,
    SemanticError,
    TypeError,
    CodegenError,
    RuntimeError,
    IOError,
    InternalError,
}

impl ErrorKind {
    fn default_code(&self) -> ErrorCode {
        match self {
            ErrorKind::LexError => ErrorCode::C0001,
            ErrorKind::ParseError => ErrorCode::C0020,
            ErrorKind::SemanticError => ErrorCode::C0050,
            ErrorKind::TypeError => ErrorCode::C0040,
            ErrorKind::CodegenError => ErrorCode::C0080,
            ErrorKind::RuntimeError => ErrorCode::R2002,
            ErrorKind::IOError => ErrorCode::R2003,
            ErrorKind::InternalError => ErrorCode::C0090,
        }
    }
}

impl CompilerError {
    pub fn new(message: impl Into<String>, kind: ErrorKind, span: Option<Span>) -> Self {
        let msg = message.into();
        let code = Self::infer_code(&kind, &msg);
        CompilerError {
            message: msg,
            span,
            kind,
            code,
            suggestion: None,
        }
    }

    /// Return the error message.
    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }

    /// Set a specific error code (overrides inferred code).
    pub fn with_code(mut self, code: ErrorCode) -> Self {
        self.code = code;
        self
    }

    fn infer_code(kind: &ErrorKind, message: &str) -> ErrorCode {
        let m = message.to_ascii_lowercase();
        match kind {
            ErrorKind::LexError => {
                if m.contains("unterminated string") || m.contains("missing closing") {
                    ErrorCode::C0002
                } else if m.contains("directive") {
                    ErrorCode::C0001
                } else {
                    ErrorCode::C0001
                }
            }
            ErrorKind::ParseError => {
                if m.contains("missing /end") || m.contains("unclosed") {
                    ErrorCode::C0020
                } else if m.contains("indent") {
                    ErrorCode::C0010
                } else {
                    ErrorCode::C0021
                }
            }
            ErrorKind::SemanticError => {
                if m.contains("orphaned pon") || m.contains("pon") {
                    ErrorCode::C0030
                } else if m.contains("duplicate") {
                    ErrorCode::C0051
                } else if m.contains("undefined")
                    || m.contains("not declared")
                    || m.contains("unknown symbol")
                {
                    ErrorCode::C0050
                } else {
                    ErrorCode::C0050
                }
            }
            ErrorKind::TypeError => {
                if m.contains("mismatch") {
                    ErrorCode::C0040
                } else {
                    ErrorCode::C0041
                }
            }
            ErrorKind::CodegenError => {
                if m.contains("unsupported") {
                    ErrorCode::C0081
                } else {
                    ErrorCode::C0080
                }
            }
            ErrorKind::RuntimeError => {
                if m.contains("assert") {
                    ErrorCode::R2001
                } else if m.contains("panic") {
                    ErrorCode::R2002
                } else if m.contains("file") && m.contains("not found") {
                    ErrorCode::R2003
                } else if m.contains("memory") || m.contains("alloc") {
                    ErrorCode::R2004
                } else {
                    ErrorCode::R2005
                }
            }
            ErrorKind::IOError => ErrorCode::R2003,
            ErrorKind::InternalError => ErrorCode::C0090,
        }
    }

    fn default_suggestion(&self) -> &'static str {
        match self.code {
            ErrorCode::C0001 => "Did you mean `/set`, `/if`, `/paste`, or another built-in directive?",
            ErrorCode::C0002 => "Add the missing closing double quote.",
            ErrorCode::C0003 => "Check number format - ensure integers and floats are valid.",
            ErrorCode::C0010 => "Align indentation with the surrounding block.",
            ErrorCode::C0020 => "Add `/end` for the open block.",
            ErrorCode::C0021 => "Complete the statement or add missing punctuation.",
            ErrorCode::C0030 => "Either create a matching empty scope block or replace `pon;` with a direct assignment.",
            ErrorCode::C0031 => "Check block nesting and directive placement.",
            ErrorCode::C0040 => "Change the value or update the type annotation so both sides match.",
            ErrorCode::C0041 => "Check the inferred type and the declared type.",
            ErrorCode::C0050 => "Declare the symbol before use or import it from the correct module.",
            ErrorCode::C0051 => "Rename one of the duplicate declarations.",
            ErrorCode::C0060 => "Review the function signature and call arguments.",
            ErrorCode::C0061 => "Check the operator or keyword for the current context.",
            ErrorCode::C0070 => "Check module references, exports, and import paths.",
            ErrorCode::C0080 => "Try a supported backend or lower the requested target.",
            ErrorCode::C0081 => "The current backend does not support this construct yet.",
            ErrorCode::C0090 => "This looks like an internal compiler bug. Reduce the test case and report it.",
            ErrorCode::I1001 => "Define the function before calling it, or import it.",
            ErrorCode::I1002 => "Check the index against the list length.",
            ErrorCode::I1003 => "Ensure the divisor is non-zero.",
            ErrorCode::I1004 => "Convert only values that are valid numbers.",
            ErrorCode::I1005 => "Do not send to a closed channel.",
            ErrorCode::I1006 => "Verify coroutine state and resume logic.",
            ErrorCode::R2001 => "Ensure the condition holds before asserting.",
            ErrorCode::R2002 => "Investigate the fatal branch before the panic call.",
            ErrorCode::R2003 => "Check the file path and working directory.",
            ErrorCode::R2004 => "Reduce the allocation size or free memory sooner.",
            ErrorCode::R2005 => "Inspect the external command and its stderr output.",
            ErrorCode::R2006 => "Check OS-level permissions and resource limits.",
            ErrorCode::V0001 => "Check for illegal characters or unclosed strings.",
            ErrorCode::V0010 => "Add the missing /end for the open block.",
            ErrorCode::V0020 => "Align indentation with the surrounding block.",
            ErrorCode::V0030 => "Ensure both sides have compatible types.",
            ErrorCode::V0040 => "Declare the symbol before use or import it.",
            ErrorCode::V0050 => "Rename one of the duplicate declarations.",
            ErrorCode::V0060 => "Check variable scope and visibility.",
            ErrorCode::V0070 => "Add a catch-all case or cover all variants.",
            ErrorCode::V1001 => "Check the runtime type of the value.",
            ErrorCode::V1002 => "Verify coroutine state and resume logic.",
            ErrorCode::V1003 => "Use a declared field instead of adding a new one.",
            ErrorCode::V1100 => "Inspect the VM state and call stack.",
            ErrorCode::V2001 => "Use `borrow(&data)` instead of moving the value.",
            ErrorCode::V2002 => "Ensure no other borrows are active.",
            ErrorCode::V2003 => "Drop the mutable borrow first.",
            ErrorCode::V2004 => "Ensure the borrowed value lives long enough.",
            ErrorCode::V2005 => "Do not use a value after it has been moved.",
            ErrorCode::V2006 => "Ensure the value matches the declared type.",
            ErrorCode::V2007 => "Only use constant expressions in constexpr.",
            ErrorCode::V2008 => "Check struct field alignment and offset.",
            ErrorCode::V2009 => "This dynamic feature is not allowed in raw mode.",
            ErrorCode::V3001 => "Check interrupt vector and handler.",
            ErrorCode::V3002 => "Verify memory map configuration.",
            ErrorCode::V3003 => "Check DMA channel and buffer alignment.",
            ErrorCode::V3004 => "Review linker script and segment layout.",
            ErrorCode::V3005 => "Do not use a linear resource after consumption.",
            ErrorCode::V3006 => "Check packet field offsets and sizes.",
            ErrorCode::V4001 => "Check the file path and working directory.",
            ErrorCode::V4002 => "Verify the module name and import path.",
            ErrorCode::V4003 => "Check file permissions and disk space.",
            ErrorCode::V4004 => "Remove circular import dependencies.",
            ErrorCode::V5001 => "Check vrs.toml configuration syntax.",
            ErrorCode::V5002 => "Ensure the build directory exists.",
            ErrorCode::V5003 => "Install the required toolchain.",
            ErrorCode::V5004 => "Use a supported backend.",
        }
    }

    fn default_quote(&self) -> &'static str {
        let pool: &[&str] = &[
            "When in doubt, wrap it in a /set block.",
            "If you have time to debug, you have time to /end.",
            "pon is not a bug, it's a feature waiting for a home.",
            "Types are just suggestions. But sometimes, they're not.",
            "If you can't say it right, just paste it.",
            "Spaces are the breath of vredrs. Breathe evenly.",
            "The compiler is your friend. A sarcastic one.",
            "Every variable deserves a set before it can shine.",
            "Zero is a concept, not a divisor.",
            "Closed channels are like closed doors – don't knock.",
            "If the file is missing, you might be looking in the wrong dimension.",
            "Memory is not infinite. But your ideas can be.",
            "External tools are guests. Treat them with respect, and logs.",
            "Not everything that looks like a number is one.",
            "Assertions are the conscience of your code.",
            "Panic is the emergency brake. Use it sparingly.",
            "If you can't find it, define it.",
            "Count before you reach. # is your friend.",
        ];
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.message.hash(&mut hasher);
        self.code.hash(&mut hasher);
        let idx = (hasher.finish() as usize) % pool.len();
        pool[idx]
    }

    pub fn lex_error(message: impl Into<String>, span: Span) -> Self {
        let mut err = CompilerError {
            message: message.into(),
            span: Some(span),
            kind: ErrorKind::LexError,
            code: ErrorCode::C0001,
            suggestion: None,
        };
        err.code = Self::infer_code(&err.kind, &err.message);
        err
    }

    pub fn parse_error(message: impl Into<String>, span: Span) -> Self {
        let mut err = CompilerError {
            message: message.into(),
            span: Some(span),
            kind: ErrorKind::ParseError,
            code: ErrorCode::C0020,
            suggestion: None,
        };
        err.code = Self::infer_code(&err.kind, &err.message);
        err
    }

    pub fn semantic_error(message: impl Into<String>, span: Span) -> Self {
        let mut err = CompilerError {
            message: message.into(),
            span: Some(span),
            kind: ErrorKind::SemanticError,
            code: ErrorCode::C0050,
            suggestion: None,
        };
        err.code = Self::infer_code(&err.kind, &err.message);
        err
    }

    /// A semantic warning. Warnings have the same structure as errors but
    /// callers can choose to not fail compilation on them.
    pub fn semantic_warning(message: impl Into<String>, span: Span) -> Self {
        let mut err = CompilerError {
            message: format!("warning: {}", message.into()),
            span: Some(span),
            kind: ErrorKind::SemanticError,
            code: ErrorCode::C0050,
            suggestion: None,
        };
        err.code = Self::infer_code(&err.kind, &err.message);
        err
    }

    pub fn type_error(message: impl Into<String>, span: Span) -> Self {
        let mut err = CompilerError {
            message: message.into(),
            span: Some(span),
            kind: ErrorKind::TypeError,
            code: ErrorCode::C0040,
            suggestion: None,
        };
        err.code = Self::infer_code(&err.kind, &err.message);
        err
    }

    pub fn codegen_error(message: impl Into<String>) -> Self {
        let msg = message.into();
        CompilerError {
            code: Self::infer_code(&ErrorKind::CodegenError, &msg),
            message: msg,
            span: None,
            kind: ErrorKind::CodegenError,
            suggestion: None,
        }
    }

    /// A runtime error: something went wrong during program execution.
    pub fn runtime_error(message: impl Into<String>) -> Self {
        let msg = message.into();
        CompilerError {
            code: Self::infer_code(&ErrorKind::RuntimeError, &msg),
            message: msg,
            span: None,
            kind: ErrorKind::RuntimeError,
            suggestion: None,
        }
    }

    /// A lightweight control-flow signal used internally by the VM for
    /// `return` / `break` / `continue` / `yield`. Unlike `runtime_error`,
    /// this skips the `infer_code` string scanning (which allocates a
    /// lowercased copy of the message and runs multiple `contains` passes)
    /// — the signal name is short and the code is irrelevant since callers
    /// dispatch on `message()`. This is on the fib(25) hot path where the
    /// signal is constructed once per recursive call (~240k times), so
    /// avoiding the scan is a meaningful win.
    pub fn control_signal(name: &'static str) -> Self {
        CompilerError {
            code: ErrorCode::R2002,
            message: name.to_string(),
            span: None,
            kind: ErrorKind::RuntimeError,
            suggestion: None,
        }
    }

    pub fn io_error(message: impl Into<String>) -> Self {
        let msg = message.into();
        CompilerError {
            code: ErrorCode::R2003,
            message: msg,
            span: None,
            kind: ErrorKind::IOError,
            suggestion: None,
        }
    }

    fn render_source_stub(&self, source_mgr: Option<&SourceManager>) -> String {
        if let Some(span) = &self.span {
            let file_display = span.display_file_name();
            let file = if let Some(mgr) = source_mgr {
                mgr.get_path(span.file_id).unwrap_or(&file_display)
            } else {
                &file_display
            };
            let line = span.line.max(1);
            let col = span.column.max(1);
            format!("  +--[{}:{}:{}]", file, line, col)
        } else {
            "  +--[<unknown>:0:0]".to_string()
        }
    }

    fn render_markers(&self, line_content: &str) -> String {
        if let Some(span) = &self.span {
            let col = span.column.max(1);
            let width = if span.end > span.start {
                (span.end - span.start).max(1)
            } else {
                1
            };

            // 计算列位置（考虑 Tab 和宽字符）
            let display_col = line_content.chars().take(col.saturating_sub(1)).count();

            let mut s = String::new();
            for _ in 0..display_col {
                s.push(' ');
            }
            if width == 1 {
                s.push('^');
            } else {
                s.push_str(&"^".repeat(width.min(10)));
            }
            s
        } else {
            String::new()
        }
    }

    pub fn render(&self) -> String {
        self.render_with_source(None)
    }

    /// Render with ANSI colors. Respects the `NO_COLOR` environment variable.
    pub fn render_colored(&self, source_mgr: Option<&SourceManager>) -> String {
        if std::env::var("NO_COLOR").is_ok() {
            return self.render_with_source(source_mgr);
        }
        self.render_with_source_colored(source_mgr)
    }

    fn render_with_source_colored(&self, source_mgr: Option<&SourceManager>) -> String {
        let cat = self.code.category();
        let code = self.code.as_str();
        let rb = "\x1b[1;31m"; let y = "\x1b[33m"; let c = "\x1b[36m";
        let w = "\x1b[37m"; let g = "\x1b[32m"; let m = "\x1b[35m"; let r = "\x1b[0m";
        let mut out = String::new();
        let tl = cat.len() + code.len() + 6;
        let dashes = "-".repeat(66_usize.saturating_sub(tl));
        out.push_str(&format!("{}-- {} {} [{}{}]{}\n\n", rb, cat, dashes, y, code, r));
        if let Some(span) = &self.span {
            let fd = span.display_file_name();
            let file = if let Some(mgr) = source_mgr { mgr.get_path(span.file_id).unwrap_or(&fd) } else { &fd };
            let line = span.line.max(1); let col = span.column.max(1);
            out.push_str(&format!("  {}+--[{}:{}:{}]{}\n", c, file, line, col, r));
            out.push_str(&format!("  {}|{}\n", c, r));
            let lc = if let Some(mgr) = source_mgr { mgr.get_line(span.file_id, line).unwrap_or("<source unavailable>") } else { "<source unavailable>" };
            let ln = format!("{:>3}", line);
            out.push_str(&format!("{}{} |{} {}{}\n", c, ln, r, w, lc));
            let mk = self.render_markers(lc);
            out.push_str(&format!("    {}|{} {}{}{}\n", c, r, rb, mk, r));
            out.push_str(&format!("    {}|{}\n", c, r));
            out.push_str(&format!("    {}\\--  {}{}\n\n", rb, r, &self.message));
            out.push_str(&format!("  {}Suggestion:{} {}\n", g, r, self.suggestion.as_deref().unwrap_or(self.default_suggestion())));
            out.push_str(&format!("{}-- The Author insists:{} \"{}\"\n", m, r, self.default_quote()));
        } else {
            out.push_str(&format!("  {}\\--  {}{}\n\n", rb, r, &self.message));
            out.push_str(&format!("  {}Suggestion:{} {}\n", g, r, self.suggestion.as_deref().unwrap_or(self.default_suggestion())));
            out.push_str(&format!("{}-- The Author insists:{} \"{}\"\n", m, r, self.default_quote()));
        }
        out
    }

    pub fn render_with_source(&self, source_mgr: Option<&SourceManager>) -> String {
        let cat = self.code.category();
        let code = self.code.as_str();
        let mut out = String::new();

        // 标题行
        let title_len = cat.len() + code.len() + 6;
        let dashes = "-".repeat(66_usize.saturating_sub(title_len));
        out.push_str(&format!("-- {} {} [{}]\n\n", cat, dashes, code));

        out.push_str(&format!("{}\n", self.render_source_stub(source_mgr)));
        out.push_str("  |\n");

        if let Some(span) = &self.span {
            let line_num = span.line.max(1);

            // 尝试从 SourceManager 获取源码行
            let line_content = if let Some(mgr) = source_mgr {
                mgr.get_line(span.file_id, line_num)
                    .unwrap_or("<source unavailable>")
            } else {
                "<source unavailable>"
            };

            // 格式化行号，保持对齐
            let line_num_str = format!("{:>3}", line_num);
            out.push_str(&format!("{} | {}\n", line_num_str, line_content));
            out.push_str(&format!("    | {}\n", self.render_markers(line_content)));
            out.push_str("    |\n");
            out.push_str(&format!("    \\--  {}\n\n", self.message));
            out.push_str(&format!(
                "  Suggestion: {}\n",
                self.suggestion
                    .as_deref()
                    .unwrap_or(self.default_suggestion())
            ));
            out.push_str(&format!(
                "-- The Author insists: \"{}\"\n",
                self.default_quote()
            ));
        } else {
            out.push_str(&format!("  \\--  {}\n\n", self.message));
            out.push_str(&format!(
                "  Suggestion: {}\n",
                self.suggestion
                    .as_deref()
                    .unwrap_or(self.default_suggestion())
            ));
            out.push_str(&format!(
                "-- The Author insists: \"{}\"\n",
                self.default_quote()
            ));
        }
        out
    }
}

impl fmt::Display for CompilerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

impl std::error::Error for CompilerError {}

/// 编译器 Result 类型别名
pub type Result<T> = std::result::Result<T, CompilerError>;
