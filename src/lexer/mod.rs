pub mod token;

use self::token::{lookup_keyword, Token, TokenKind};
use crate::error::{CompilerError, Result, Span};

/// 词法分析器
pub struct Lexer {
    source: Vec<char>,
    pos: usize,
    line: usize,
    column: usize,
    file_id: usize,
    indent_stack: Vec<usize>,
    tokens: Vec<Token>,
    errored: bool,
    at_line_start: bool,
    paren_depth: usize,
}

impl Lexer {
    pub fn new(source: &str, file_id: usize) -> Self {
        Lexer {
            source: source.chars().collect(),
            pos: 0,
            line: 1,
            column: 1,
            file_id,
            indent_stack: vec![1],
            tokens: Vec::new(),
            errored: false,
            at_line_start: true,
            paren_depth: 0,
        }
    }

    /// 主入口：词法分析
    pub fn tokenize(&mut self) -> Result<Vec<Token>> {
        let len = self.source.len();

        while self.pos < len {
            if self.at_line_start {
                self.scan_line_start()?;
                continue;
            }

            let ch = self.current_char();

            match ch {
                ' ' | '\t' => {
                    self.advance();
                }
                '\n' => {
                    // Inside (), [], {} whitespace newlines are insignificant.
                    if self.paren_depth > 0 {
                        self.advance();
                        self.line += 1;
                        self.column = 1;
                    } else {
                        let start = self.pos;
                        self.advance();
                        self.add_token(TokenKind::Newline, start, self.pos, "\n");
                        self.line += 1;
                        self.column = 1;
                        self.at_line_start = true;
                    }
                }
                '\r' => {
                    self.advance();
                    if self.current_char() == '\n' {
                        if self.paren_depth > 0 {
                            self.advance();
                            self.line += 1;
                            self.column = 1;
                        } else {
                            let start = self.pos - 1;
                            self.advance();
                            self.add_token(TokenKind::Newline, start, self.pos, "\n");
                            self.line += 1;
                            self.column = 1;
                            self.at_line_start = true;
                        }
                    }
                }
                '"' | '\'' => {
                    self.scan_string()?;
                }
                '0'..='9' => {
                    self.scan_number()?;
                }
                '_' => {
                    // 单独下划线是后缀操作符或通配符
                    if self.peek_char().is_alphanumeric() || self.peek_char() == '_' {
                        self.scan_identifier_or_keyword();
                    } else {
                        self.scan_simple(TokenKind::Underscore);
                    }
                }
                'a'..='z' | 'A'..='Z' => {
                    self.scan_identifier_or_keyword();
                }
                c if c.is_alphabetic() && c > '\u{007f}' => {
                    self.scan_identifier_or_keyword();
                }
                '#' => self.scan_hash_or_comment()?,
                '/' => self.scan_slash_or_div(),
                '~' => self.scan_simple(TokenKind::Tilde),
                '^' => self.scan_simple(TokenKind::Caret),
                ',' => self.scan_simple(TokenKind::Comma),
                ';' => self.scan_simple(TokenKind::Semicolon),
                ':' => self.scan_simple(TokenKind::Colon),
                '(' => {
                    self.paren_depth += 1;
                    self.scan_simple(TokenKind::LParen);
                }
                ')' => {
                    if self.paren_depth > 0 {
                        self.paren_depth -= 1;
                    }
                    self.scan_simple(TokenKind::RParen);
                }
                '[' => {
                    self.paren_depth += 1;
                    self.scan_simple(TokenKind::LBracket);
                }
                ']' => {
                    if self.paren_depth > 0 {
                        self.paren_depth -= 1;
                    }
                    self.scan_simple(TokenKind::RBracket);
                }
                '{' => {
                    self.paren_depth += 1;
                    self.scan_simple(TokenKind::LBrace);
                }
                '}' => {
                    if self.paren_depth > 0 {
                        self.paren_depth -= 1;
                    }
                    self.scan_simple(TokenKind::RBrace);
                }
                '.' => self.scan_dot(),
                '?' => self.scan_question(),
                '|' => self.scan_pipe(),
                '!' => self.scan_bang_or_ne(),
                '=' => self.scan_assign_or_eq(),
                '<' => self.scan_lt_or_le(),
                '>' => self.scan_gt_or_ge(),
                '+' => self.scan_plus_or_assign(),
                '-' => self.scan_minus_or_assign(),
                '*' => self.scan_star_or_assign_or_power(),
                '%' => self.scan_percent_or_assign(),
                '&' => self.scan_simple(TokenKind::BitAnd),
                '@' => self.scan_simple(TokenKind::At),
                c => {
                    let span = self.span_at(self.pos, self.pos + 1);
                    self.errored = true;
                    return Err(CompilerError::lex_error(
                        format!("Unexpected character '{}'", c),
                        span,
                    ));
                }
            }
        }

        // 生成 EOF
        let eof_pos = self.source.len();
        self.generate_dedents(1)?;
        if !self.at_line_start {
            self.tokens.push(Token::new(
                TokenKind::Newline,
                Span::new(eof_pos, eof_pos, self.line, self.column, self.file_id),
                "\n".to_string(),
            ));
        }
        self.tokens.push(Token::new(
            TokenKind::EOF,
            Span::new(eof_pos, eof_pos, self.line, self.column, self.file_id),
            "".to_string(),
        ));
        Ok(std::mem::take(&mut self.tokens))
    }

    // ========== 辅助方法 ==========

    fn current_char(&self) -> char {
        if self.pos < self.source.len() {
            self.source[self.pos]
        } else {
            '\0'
        }
    }

    fn peek_char(&self) -> char {
        if self.pos + 1 < self.source.len() {
            self.source[self.pos + 1]
        } else {
            '\0'
        }
    }

    fn peek_char_n(&self, n: usize) -> char {
        let idx = self.pos + n;
        if idx < self.source.len() {
            self.source[idx]
        } else {
            '\0'
        }
    }

    fn advance(&mut self) {
        if self.pos < self.source.len() {
            let ch = self.source[self.pos];
            if ch == '\n' {
                // column reset handled elsewhere
            } else {
                self.column += 1;
            }
            self.pos += 1;
        }
    }

    fn span_at(&self, start: usize, end: usize) -> Span {
        Span::new(start, end, self.line, self.column, self.file_id)
    }

    fn add_token(&mut self, kind: TokenKind, start: usize, end: usize, lexeme: &str) {
        let span = Span::new(start, end, self.line, self.column, self.file_id);
        self.tokens.push(Token::new(kind, span, lexeme.to_string()));
    }

    fn add_token_at(&mut self, kind: TokenKind, start: usize, end: usize, column: usize) {
        let span = Span::new(start, end, self.line, column, self.file_id);
        let lexeme: String = self.source[start..end].iter().collect();
        self.tokens.push(Token::new(kind, span, lexeme));
    }

    fn scan_simple(&mut self, kind: TokenKind) {
        let start = self.pos;
        let ch = self.current_char();
        self.advance();
        self.add_token(kind, start, self.pos, &ch.to_string());
    }

    // ========== 缩进管理 ==========

    fn generate_indents(&mut self, new_indent: usize) -> Result<()> {
        let current = *self.indent_stack.last().unwrap();
        if new_indent > current {
            self.indent_stack.push(new_indent);
            let pos = self.pos;
            self.add_token_at(TokenKind::Indent, pos, pos, new_indent);
        } else if new_indent < current {
            self.generate_dedents(new_indent)?;
        }
        Ok(())
    }

    fn generate_dedents(&mut self, target: usize) -> Result<()> {
        while let Some(&current) = self.indent_stack.last() {
            if current <= target {
                break;
            }
            self.indent_stack.pop();
            let pos = self.pos;
            self.add_token_at(TokenKind::Dedent, pos, pos, target);
        }
        if *self.indent_stack.last().unwrap() != target {
            let span = self.span_at(self.pos, self.pos + 1);
            return Err(CompilerError::lex_error(
                format!(
                    "Inconsistent indentation: expected {}, got {}",
                    self.indent_stack.last().unwrap(),
                    target
                ),
                span,
            ));
        }
        Ok(())
    }

    // ========== 行首处理 ==========

    fn scan_line_start(&mut self) -> Result<()> {
        // 消费空白并计算缩进
        while self.pos < self.source.len() {
            match self.current_char() {
                ' ' => {
                    self.advance();
                }
                '\t' => {
                    self.advance();
                    self.column += 3;
                }
                _ => break,
            }
        }

        if self.pos >= self.source.len() {
            return Ok(());
        }

        let ch = self.current_char();

        // 空行：消费换行，保持 at_line_start = true
        if ch == '\n' {
            self.advance();
            self.line += 1;
            self.column = 1;
            return Ok(());
        }
        if ch == '\r' {
            self.advance();
            if self.current_char() == '\n' {
                self.advance();
            }
            self.line += 1;
            self.column = 1;
            return Ok(());
        }

        // 注释行：消费整个注释（含换行符），保持 at_line_start = true
        if ch == '#' {
            if self.peek_char() == '*' {
                // 多行注释
                let _ = self.scan_multiline_comment();
                return Ok(());
            }
            // 单行注释或文档注释
            self.scan_comment_line();
            return Ok(());
        }

        // 正常行：生成缩进 token
        let indent = self.column;
        self.generate_indents(indent)?;

        // /= 表格赋值（必须在 / 指令之前检查）
        if self.current_char() == '/' && self.peek_char() == '=' {
            self.advance();
            self.advance();
            let start = self.pos - 2;
            self.add_token(TokenKind::TableAssign, start, self.pos, "/=");
            self.at_line_start = false;
            return Ok(());
        }

        // 指令块或作用域块
        if self.current_char() == '/' {
            self.scan_directive_or_scope()?;
            self.at_line_start = false;
            return Ok(());
        }

        self.at_line_start = false;
        Ok(())
    }

    fn scan_directive_or_scope(&mut self) -> Result<()> {
        let start = self.pos;
        self.advance(); // 跳过 '/'

        let mut lexeme = String::from("/");
        while self.pos < self.source.len() {
            let ch = self.current_char();
            if ch.is_whitespace() || ch == '\n' || ch == '\r' {
                break;
            }
            if ch.is_alphanumeric() || ch == '_' || ch == '.' {
                lexeme.push(ch);
                self.advance();
            } else {
                break;
            }
        }

        self.add_token(TokenKind::DirectiveOrScope, start, self.pos, &lexeme);
        Ok(())
    }

    // ========== 注释 ==========

    /// 扫描单行注释（不含行尾换行符），不生成 token。
    /// 换行符留给主循环处理，这样行末注释不会吞掉语句的结束换行。
    fn scan_comment_line(&mut self) {
        // 跳过 #
        self.advance();
        if self.current_char() == '#' {
            self.advance();
        }
        // 读取到行尾（不消费换行符）
        while self.pos < self.source.len()
            && self.current_char() != '\n'
            && self.current_char() != '\r'
        {
            self.advance();
        }
        // 不消费换行符 — 让主循环处理它，这样 set, x, 10 # comment
        // 后面会正确生成 Newline token。
    }

    /// 扫描多行注释 #* ... *#
    fn scan_multiline_comment(&mut self) -> Result<()> {
        let start = self.pos;
        let start_span = self.span_at(start, start + 2);
        self.advance(); // #
        self.advance(); // *

        let mut depth: i32 = 1;
        loop {
            if self.pos >= self.source.len() {
                self.errored = true;
                return Err(CompilerError::lex_error(
                    "Unterminated multiline comment",
                    start_span,
                ));
            }
            if self.current_char() == '#' && self.peek_char() == '*' {
                self.advance();
                self.advance();
                depth += 1;
            } else if self.current_char() == '*' && self.peek_char() == '#' {
                self.advance();
                self.advance();
                depth -= 1;
                if depth == 0 {
                    break;
                }
            } else if self.current_char() == '\n' {
                self.advance();
                self.line += 1;
                self.column = 1;
            } else {
                self.advance();
            }
        }

        let end = self.pos;
        let lexeme: String = self.source[start..end].iter().collect();
        self.add_token(TokenKind::MultilineComment, start, end, &lexeme);
        Ok(())
    }

    /// 扫描行中的 #（后缀操作符）
    fn scan_hash_or_comment(&mut self) -> Result<()> {
        let start = self.pos;
        self.advance(); // #

        if self.current_char() == '*' {
            // 多行注释在行中
            self.pos = start;
            self.scan_multiline_comment()?;
            return Ok(());
        }

        // Check if this is a comment (## or # followed by space or EOL)
        // vs. the postfix # operator (x#).
        // Heuristic: if # is at end of input or followed by a non-space
        // char, it's the postfix # operator. If followed by space, #, or
        // newline, it's a comment.
        if self.pos >= self.source.len() {
            // # at EOF → postfix operator
            self.add_token(TokenKind::Hash, start, self.pos, "#");
            return Ok(());
        }
        if self.current_char() == '#'
            || self.current_char() == ' '
            || self.current_char() == '\t'
            || self.current_char() == '\n'
            || self.current_char() == '\r'
        {
            // This is a comment — scan it (without consuming newline).
            self.scan_comment_line();
            return Ok(());
        }

        // 后缀操作符 #
        self.add_token(TokenKind::Hash, start, self.pos, "#");
        Ok(())
    }

    /// 扫描 / 除法或作用域结束符
    fn scan_slash_or_div(&mut self) {
        let start = self.pos;
        self.advance();

        if self.current_char() == '/' {
            self.advance();
            self.add_token(TokenKind::FloorDiv, start, self.pos, "//");
        } else if self.current_char() == '=' {
            self.advance();
            self.add_token(TokenKind::TableAssign, start, self.pos, "/=");
        } else {
            self.add_token(TokenKind::Slash, start, self.pos, "/");
        }
    }

    // ========== 字符串 ==========

    fn scan_string(&mut self) -> Result<()> {
        let start = self.pos;
        let start_span = self.span_at(start, start + 1);
        let quote = self.current_char(); // " or '
        self.advance(); // consume opening quote

        // 三引号检测 (only for double quotes)
        if quote == '"' && self.current_char() == '"' && self.peek_char() == '"' {
            self.advance();
            self.advance();
            if self.current_char() == '\n' {
                self.advance();
                self.line += 1;
                self.column = 1;
            } else if self.current_char() == '\r' && self.peek_char() == '\n' {
                self.advance();
                self.advance();
                self.line += 1;
                self.column = 1;
            }
            return self.scan_multiline_string(start);
        }

        // 普通字符串 (double or single quote)
        let mut content = String::new();
        loop {
            if self.pos >= self.source.len() {
                return Err(CompilerError::lex_error(
                    "Unterminated string literal",
                    start_span,
                ));
            }
            let ch = self.current_char();
            if ch == '\\' {
                self.advance();
                let esc = self.current_char();
                content.push('\\');
                content.push(esc);
                self.advance();
            } else if ch == quote {
                self.advance();
                break;
            } else if ch == '\n' {
                return Err(CompilerError::lex_error(
                    "Newline in string literal",
                    self.span_at(self.pos, self.pos + 1),
                ));
            } else {
                content.push(ch);
                self.advance();
            }
        }

        self.add_token(TokenKind::StringLiteral, start, self.pos, &content);
        Ok(())
    }

    fn scan_multiline_string(&mut self, start: usize) -> Result<()> {
        loop {
            if self.pos >= self.source.len() {
                return Err(CompilerError::lex_error(
                    "Unterminated multiline string",
                    self.span_at(start, start + 3),
                ));
            }
            if self.current_char() == '"' && self.peek_char() == '"' && self.peek_char_n(2) == '"' {
                self.advance();
                self.advance();
                self.advance();
                break;
            } else if self.current_char() == '\n' {
                self.advance();
                self.line += 1;
                self.column = 1;
            } else {
                self.advance();
            }
        }

        let content: String = self.source[start + 3..self.pos - 3].iter().collect();
        self.add_token(TokenKind::MultiLineString, start, self.pos, &content);
        Ok(())
    }

    // ========== 数字 ==========

    fn scan_number(&mut self) -> Result<()> {
        let start = self.pos;
        let mut is_float = false;

        if self.current_char() == '0' && matches!(self.peek_char(), 'x' | 'X') {
            self.advance();
            self.advance();
            while self.pos < self.source.len() && self.current_char().is_ascii_hexdigit() {
                self.advance();
            }
            let lexeme: String = self.source[start..self.pos].iter().collect();
            self.add_token(TokenKind::IntegerLiteral, start, self.pos, &lexeme);
            return Ok(());
        }
        if self.current_char() == '0' && matches!(self.peek_char(), 'o' | 'O') {
            self.advance();
            self.advance();
            while self.pos < self.source.len() && matches!(self.current_char(), '0'..='7') {
                self.advance();
            }
            let lexeme: String = self.source[start..self.pos].iter().collect();
            self.add_token(TokenKind::IntegerLiteral, start, self.pos, &lexeme);
            return Ok(());
        }
        if self.current_char() == '0' && matches!(self.peek_char(), 'b' | 'B') {
            self.advance();
            self.advance();
            while self.pos < self.source.len() && matches!(self.current_char(), '0' | '1') {
                self.advance();
            }
            let lexeme: String = self.source[start..self.pos].iter().collect();
            self.add_token(TokenKind::IntegerLiteral, start, self.pos, &lexeme);
            return Ok(());
        }

        while self.pos < self.source.len() && self.current_char().is_ascii_digit() {
            self.advance();
        }
        if self.current_char() == '.' && self.peek_char().is_ascii_digit() {
            is_float = true;
            self.advance();
            while self.pos < self.source.len() && self.current_char().is_ascii_digit() {
                self.advance();
            }
        }
        if self.current_char() == 'e' || self.current_char() == 'E' {
            is_float = true;
            self.advance();
            if self.current_char() == '+' || self.current_char() == '-' {
                self.advance();
            }
            while self.pos < self.source.len() && self.current_char().is_ascii_digit() {
                self.advance();
            }
        }

        let lexeme: String = self.source[start..self.pos].iter().collect();
        let kind = if is_float {
            TokenKind::FloatLiteral
        } else {
            TokenKind::IntegerLiteral
        };
        self.add_token(kind, start, self.pos, &lexeme);
        Ok(())
    }

    // ========== 标识符/关键字 ==========

    fn scan_identifier_or_keyword(&mut self) {
        let start = self.pos;
        while self.pos < self.source.len() {
            let ch = self.current_char();
            if Self::is_identifier_continue(ch) && ch != '_' {
                self.advance();
            } else if ch == '_' {
                let next = self.peek_char();
                if Self::is_identifier_continue(next) {
                    self.advance();
                } else if self.is_magic_identifier_closing_underscore(start) {
                    // Keep the final `_` of double-underscore special names inside the
                    // identifier.  Without this, names such as `__add__` were lexed as
                    // Identifier("__add_") + Underscore, which broke operator overloads.
                    self.advance();
                } else {
                    // A normal trailing `_` remains the postfix descending-sort operator
                    // or a wildcard token, e.g. `lst_` => Identifier("lst") + Underscore.
                    break;
                }
            } else {
                break;
            }
        }
        let lexeme: String = self.source[start..self.pos].iter().collect();
        // 单独的 _ 是通配符/后缀操作符 token
        if lexeme == "_" {
            self.add_token(TokenKind::Underscore, start, self.pos, "_");
            return;
        }
        if let Some(kw) = lookup_keyword(&lexeme) {
            self.add_token(kw, start, self.pos, &lexeme);
        } else {
            self.add_token(TokenKind::Identifier, start, self.pos, &lexeme);
        }
    }

    fn is_identifier_continue(ch: char) -> bool {
        ch == '_' || ch.is_alphanumeric() || (ch > '\u{007f}' && ch.is_alphabetic())
    }

    fn is_magic_identifier_closing_underscore(&self, start: usize) -> bool {
        self.current_char() == '_'
            && self.pos + 1 <= self.source.len()
            && start + 1 < self.source.len()
            && self.source[start] == '_'
            && self.source[start + 1] == '_'
            && self.pos > start + 1
            && self.source[self.pos - 1] == '_'
    }

    // ========== 运算符与符号 ==========

    fn scan_dot(&mut self) {
        let start = self.pos;
        self.advance();
        if self.current_char() == '.' {
            self.advance();
            if self.current_char() == '.' {
                self.advance();
                self.add_token(TokenKind::DotDotDot, start, self.pos, "...");
            } else {
                self.add_token(TokenKind::DotDot, start, self.pos, "..");
            }
        } else {
            self.add_token(TokenKind::Dot, start, self.pos, ".");
        }
    }

    fn scan_question(&mut self) {
        let start = self.pos;
        self.advance();
        if self.current_char() == '.' {
            self.advance();
            self.add_token(TokenKind::QuestionDot, start, self.pos, "?.");
        } else if self.current_char() == '?' {
            self.advance();
            self.add_token(TokenKind::QuestionQuestion, start, self.pos, "??");
        } else {
            self.add_token(TokenKind::Question, start, self.pos, "?");
        }
    }

    fn scan_pipe(&mut self) {
        let start = self.pos;
        self.advance();
        if self.current_char() == '>' {
            self.advance();
            self.add_token(TokenKind::PipeArrow, start, self.pos, "|>");
        } else {
            self.add_token(TokenKind::Identifier, start, self.pos, "|");
        }
    }

    fn scan_bang_or_ne(&mut self) {
        let start = self.pos;
        self.advance();
        if self.current_char() == '=' {
            self.advance();
            self.add_token(TokenKind::Ne, start, self.pos, "!=");
        } else {
            self.add_token(TokenKind::Bang, start, self.pos, "!");
        }
    }

    fn scan_assign_or_eq(&mut self) {
        let start = self.pos;
        self.advance();
        if self.current_char() == '=' {
            self.advance();
            self.add_token(TokenKind::Eq, start, self.pos, "==");
        } else {
            self.add_token(TokenKind::Assign, start, self.pos, "=");
        }
    }

    fn scan_lt_or_le(&mut self) {
        let start = self.pos;
        self.advance();
        if self.current_char() == '=' {
            self.advance();
            self.add_token(TokenKind::Le, start, self.pos, "<=");
        } else {
            self.add_token(TokenKind::Lt, start, self.pos, "<");
        }
    }

    fn scan_gt_or_ge(&mut self) {
        let start = self.pos;
        self.advance();
        if self.current_char() == '=' {
            self.advance();
            self.add_token(TokenKind::Ge, start, self.pos, ">=");
        } else {
            self.add_token(TokenKind::Gt, start, self.pos, ">");
        }
    }

    fn scan_plus_or_assign(&mut self) {
        let start = self.pos;
        self.advance();
        if self.current_char() == '=' {
            self.advance();
            self.add_token(TokenKind::PlusAssign, start, self.pos, "+=");
        } else {
            self.add_token(TokenKind::Plus, start, self.pos, "+");
        }
    }

    fn scan_minus_or_assign(&mut self) {
        let start = self.pos;
        self.advance();
        if self.current_char() == '=' {
            self.advance();
            self.add_token(TokenKind::MinusAssign, start, self.pos, "-=");
        } else {
            self.add_token(TokenKind::Minus, start, self.pos, "-");
        }
    }

    fn scan_star_or_assign_or_power(&mut self) {
        let start = self.pos;
        self.advance();
        if self.current_char() == '=' {
            self.advance();
            self.add_token(TokenKind::StarAssign, start, self.pos, "*=");
        } else if self.current_char() == '*' {
            self.advance();
            self.add_token(TokenKind::Power, start, self.pos, "**");
        } else {
            self.add_token(TokenKind::Star, start, self.pos, "*");
        }
    }

    fn scan_percent_or_assign(&mut self) {
        let start = self.pos;
        self.advance();
        if self.current_char() == '=' {
            self.advance();
            self.add_token(TokenKind::PercentAssign, start, self.pos, "%=");
        } else {
            self.add_token(TokenKind::Percent, start, self.pos, "%");
        }
    }
}

#[cfg(test)]
mod tests;
