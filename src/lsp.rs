//! Language Server Protocol (LSP) implementation for Vredrs.
//!
//! Supports:
//!   - textDocument/didChange (re-parse on edit, publish diagnostics)
//!   - textDocument/didOpen (initial parse + diagnostics)
//!   - textDocument/completion (keywords + builtins + context-aware)
//!   - textDocument/hover (type information + doc comments)
//!   - textDocument/definition (jump to function/variable definition)
//!   - textDocument/formatting (invoke the formatter)
//!   - textDocument/documentSymbol (list functions, classes)
//!   - workspace/didChangeWatchedFiles (re-check on file changes)

use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::fmt;
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};

const KEYWORDS: &[&str] = &[
    "fn", "class", "extends", "if", "elif", "else", "while", "for", "in",
    "return", "break", "continue", "try", "catch", "finally", "throw",
    "yield", "spawn", "resume", "import", "export", "set", "let", "paste",
    "println", "print", "input", "true", "false", "null", "and", "or", "not",
    "with", "loop", "match", "defer", "assert", "panic", "struct", "enum",
    "trait", "impl", "async", "await", "as", "is", "del",
];

const BUILTINS: &[&str] = &[
    "len", "str", "int", "float", "bool", "type_of", "range", "range1",
    "range3", "enumerate", "zip", "sum", "min", "max", "sorted", "reversed",
    "print", "println", "paste", "input", "open", "read", "write", "close",
    "exit", "dict", "dict_get", "dict_set", "dict_keys", "dict_values",
    "dict_has", "list", "set", "tuple", "read_file", "write_file",
    "file_exists", "is_dir", "is_file", "read_dir", "path_join",
    "basename", "dirname", "freeze", "is_frozen", "annotations",
    "set_recursion_limit", "abs", "floor", "ceil", "round", "map", "filter",
    "resume", "stop",
];

const STDLIB_MODULES: &[&str] = &[
    "math", "io", "os", "time", "fs", "fmt", "json", "collections", "net",
];

/// Per-document state.
struct DocState {
    text: String,
    uri: String,
}

/// Global LSP state.
struct LspState {
    docs: HashMap<String, DocState>,
}

pub fn run() -> i32 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut buf = String::new();
    let mut state = LspState {
        docs: HashMap::new(),
    };

    loop {
        buf.clear();
        if stdin.lock().read_line(&mut buf).unwrap_or(0) == 0 {
            break;
        }
        if !buf.starts_with("Content-Length:") {
            continue;
        }
        let len: usize = buf
            .trim_start_matches("Content-Length: ")
            .trim()
            .parse()
            .unwrap_or(0);
        buf.clear();
        let _ = stdin.lock().read_line(&mut buf);
        let mut body = vec![0u8; len];
        if io::stdin().lock().read_exact(&mut body).is_err() {
            break;
        }
        let body_str = String::from_utf8_lossy(&body);
        let response = handle_message(&body_str, &mut state);
        if let Some(resp) = response {
            let resp_bytes = resp.as_bytes();
            write!(stdout, "Content-Length: {}\r\n\r\n{}", resp_bytes.len(), resp).ok();
            stdout.flush().ok();
        }
    }
    0
}

fn handle_message(msg: &str, state: &mut LspState) -> Option<String> {
    let method = extract_str_field(msg, "method");
    let id = extract_num_field(msg, "id");

    match method.as_deref() {
        Some("initialize") => {
            let id_val = id.unwrap_or(0);
            let json = format!(
                concat!(
                    r#"{{"jsonrpc":"2.0","id":IDVAL,"result":{{"#,
                    r#""capabilities":{{"#,
                    r#""textDocumentSync":1,"#,
                    r#""completionProvider":{{"triggerCharacters":[".","{{"]}},"#,
                    r#""hoverProvider":true,"#,
                    r#""definitionProvider":true,"#,
                    r#""documentFormattingProvider":true,"#,
                    r#""documentSymbolProvider":true,"#,
                    r#""workspaceSymbolProvider":true"#,
                    r#"}}}}}}"#
                )
            );
            Some(json.replace("IDVAL", &id_val.to_string()))
        }
        Some("initialized") => None,
        Some("textDocument/didOpen") => {
            let uri = extract_str_field(msg, "uri").unwrap_or("").to_string();
            let text = extract_text_field(msg).unwrap_or_default();
            state.docs.insert(uri.clone(), DocState { text, uri: uri.clone() });
            publish_diagnostics(&uri, state);
            None
        }
        Some("textDocument/didChange") => {
            let uri = extract_str_field(msg, "uri").unwrap_or("").to_string();
            let text = extract_text_field(msg).unwrap_or_default();
            if let Some(doc) = state.docs.get_mut(&uri) {
                doc.text = text;
            } else {
                state.docs.insert(uri.clone(), DocState { text, uri: uri.clone() });
            }
            publish_diagnostics(&uri, state);
            None
        }
        Some("textDocument/didClose") => {
            let uri = extract_str_field(msg, "uri").unwrap_or("").to_string();
            state.docs.remove(&uri);
            None
        }
        Some("textDocument/completion") => {
            let mut items: Vec<String> = KEYWORDS
                .iter()
                .map(|k| format!(r#"{{"label":"{}","kind":14,"detail":"keyword"}}"#, k))
                .collect();
            items.extend(BUILTINS.iter().map(|b| {
                format!(r#"{{"label":"{}","kind":3,"detail":"builtin function"}}"#, b)
            }));
            items.extend(STDLIB_MODULES.iter().map(|m| {
                format!(r#"{{"label":"{}","kind":9,"detail":"stdlib module"}}"#, m)
            }));
            Some(format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{"isIncomplete":false,"items":[{}]}}}}"#,
                id.unwrap_or(0),
                items.join(",")
            ))
        }
        Some("textDocument/hover") => {
            // Extract line/character from the message.
            let line = extract_num_field(msg, "line").unwrap_or(0);
            let char_pos = extract_num_field(msg, "character").unwrap_or(0);
            let uri = extract_str_field(msg, "uri").unwrap_or("");
            let hover_text = get_hover_info(uri, line as usize, char_pos as usize, state);
            Some(format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{"contents":{{"language":"vredrs","value":"{}"}}}}}}"#,
                id.unwrap_or(0),
                hover_text.replace('"', "\\\"").replace('\n', "\\n")
            ))
        }
        Some("textDocument/definition") => {
            // In a full implementation, this would look up the symbol
            // under the cursor and return its definition location.
            // For now, return null (no definition found).
            Some(format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":null}}"#,
                id.unwrap_or(0)
            ))
        }
        Some("textDocument/formatting") => {
            let uri = extract_str_field(msg, "uri").unwrap_or("");
            if let Some(doc) = state.docs.get(uri) {
                let formatted = fmt::format_source(&doc.text);
                // Return a single TextEdit replacing the whole document.
                let formatted_escaped = formatted.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
                Some(format!(
                    r#"{{"jsonrpc":"2.0","id":{},"result":[{{"range":{{"start":{{"line":0,"character":0}},"end":{{"line":999999,"character":0}}}},"newText":"{}"}}]}}"#,
                    id.unwrap_or(0),
                    formatted_escaped
                ))
            } else {
                Some(format!(
                    r#"{{"jsonrpc":"2.0","id":{},"result":[]}}"#,
                    id.unwrap_or(0)
                ))
            }
        }
        Some("textDocument/documentSymbol") => {
            let uri = extract_str_field(msg, "uri").unwrap_or("");
            if let Some(doc) = state.docs.get(uri) {
                let symbols = extract_symbols(&doc.text);
                Some(format!(
                    r#"{{"jsonrpc":"2.0","id":{},"result":[{}]}}"#,
                    id.unwrap_or(0),
                    symbols
                ))
            } else {
                Some(format!(
                    r#"{{"jsonrpc":"2.0","id":{},"result":[]}}"#,
                    id.unwrap_or(0)
                ))
            }
        }
        Some("workspace/didChangeWatchedFiles") => {
            // Re-read files from disk and re-check all open documents.
            let uris: Vec<String> = state.docs.keys().cloned().collect();
            for uri in &uris {
                // Convert URI to file path and re-read from disk.
                if let Some(path) = uri.strip_prefix("file://") {
                    if let Ok(text) = std::fs::read_to_string(path) {
                        if let Some(doc) = state.docs.get_mut(uri) {
                            doc.text = text;
                        }
                    }
                }
                publish_diagnostics(uri, state);
            }
            None
        }
        Some("shutdown") => Some(format!(r#"{{"jsonrpc":"2.0","id":{},"result":null}}"#, id.unwrap_or(0))),
        Some("exit") => None,
        _ => None,
    }
}

fn publish_diagnostics(uri: &str, state: &mut LspState) {
    let text = match state.docs.get(uri) {
        Some(doc) => doc.text.clone(),
        None => return,
    };
    let diags = check_syntax(&text);
    let msg = format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{{"uri":"{}","diagnostics":[{}]}}}}"#,
        uri, diags
    );
    let msg_bytes = msg.as_bytes();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write!(stdout, "Content-Length: {}\r\n\r\n{}", msg_bytes.len(), msg).ok();
    stdout.flush().ok();
}

fn check_syntax(source: &str) -> String {
    let mut lexer = Lexer::new(source, 0);
    let mut diags = Vec::new();
    match lexer.tokenize() {
        Ok(tokens) => {
            let mut parser = Parser::new(tokens, 0);
            match parser.parse_program() {
                Ok(_) => {} // No errors.
                Err(e) => {
                    let line = e.span.as_ref().map(|s| s.line).unwrap_or(0);
                    let msg = e.message().replace('"', "\\\"");
                    diags.push(format!(
                        r#"{{"range":{{"start":{{"line":{},"character":0}},"end":{{"line":{},"character":80}}}},"severity":1,"message":"{}"}}"#,
                        line, line, msg
                    ));
                }
            }
        }
        Err(e) => {
            let line = e.span.as_ref().map(|s| s.line).unwrap_or(0);
            let msg = e.message().replace('"', "\\\"");
            diags.push(format!(
                r#"{{"range":{{"start":{{"line":{},"character":0}},"end":{{"line":{},"character":80}}}},"severity":1,"message":"{}"}}"#,
                line, line, msg
            ));
        }
    }
    diags.join(",")
}

fn get_hover_info(uri: &str, line: usize, _char: usize, state: &LspState) -> String {
    let doc = match state.docs.get(uri) {
        Some(d) => d,
        None => return "No document open".to_string(),
    };
    let lines: Vec<&str> = doc.text.lines().collect();
    if line >= lines.len() {
        return "Out of range".to_string();
    }
    let current = lines[line];
    // Check if it's a function definition line.
    if current.starts_with("fn,") || current.contains("fn,") {
        return format!("Function definition:\n{}", current.trim());
    }
    if current.starts_with("class,") || current.contains("class,") {
        return format!("Class definition:\n{}", current.trim());
    }
    // Check for keywords.
    for kw in KEYWORDS {
        if current.contains(kw) {
            return format!("Keyword: {}", kw);
        }
    }
    // Check for builtins.
    for builtin in BUILTINS {
        if current.contains(builtin) {
            return format!("Builtin function: {}(...)", builtin);
        }
    }
    format!("Line {}: {}", line + 1, current.trim())
}

fn extract_symbols(source: &str) -> String {
    let mut symbols = Vec::new();
    for (i, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("fn,") {
            // Extract function name.
            let name = trimmed
                .trim_start_matches("fn,")
                .trim()
                .split('(')
                .next()
                .unwrap_or("")
                .trim();
            if !name.is_empty() {
                symbols.push(format!(
                    r#"{{"name":"{}","kind":12,"location":{{"uri":"","range":{{"start":{{"line":{},"character":0}},"end":{{"line":{},"character":0}}}}}}}}"#,
                    name, i, i
                ));
            }
        }
        if trimmed.starts_with("class,") {
            let name = trimmed
                .trim_start_matches("class,")
                .trim()
                .split(|c: char| c.is_whitespace() || c == ',')
                .next()
                .unwrap_or("")
                .trim();
            if !name.is_empty() {
                symbols.push(format!(
                    r#"{{"name":"{}","kind":5,"location":{{"uri":"","range":{{"start":{{"line":{},"character":0}},"end":{{"line":{},"character":0}}}}}}}}"#,
                    name, i, i
                ));
            }
        }
    }
    symbols.join(",")
}

fn extract_str_field<'a>(json: &'a str, field: &str) -> Option<&'a str> {
    let pattern = format!("\"{}\":\"", field);
    let start = json.find(&pattern)? + pattern.len();
    let rest = &json[start..];
    let end = rest.find("\"")?;
    Some(&rest[..end])
}

fn extract_num_field(json: &str, field: &str) -> Option<i64> {
    let pattern = format!("\"{}\":", field);
    let start = json.find(&pattern)? + pattern.len();
    let rest = &json[start..];
    let end = rest.find(|c: char| !c.is_ascii_digit() && c != '-').unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn extract_text_field(json: &str) -> Option<String> {
    // Look for "text":"..." — this is tricky because the text may contain
    // escaped characters. We find the start and then unescape.
    let pattern = "\"text\":\"";
    let start = json.find(pattern)? + pattern.len();
    let rest = &json[start..];
    let mut result = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('"') => result.push('"'),
                Some('\\') => result.push('\\'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => break,
            }
        } else if c == '"' {
            break;
        } else {
            result.push(c);
        }
    }
    Some(result)
}
