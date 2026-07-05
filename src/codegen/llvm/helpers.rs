//! Helper functions for JSON encoding used by the `annotations()` builtin.
//!
//! These are extracted from `llvm_full.rs` for clarity. They are pure
//! functions that operate on AST nodes and produce JSON strings.

use crate::parser::ast::{Annotation, Expr, StringPart};

/// Format a float as an LLVM IR compatible string.
pub fn format_float(v: f64) -> String {
    if v == 0.0 {
        "0.000000e+00".to_string()
    } else {
        format!("{:.17e}", v)
    }
}

/// Interpret C-style escape sequences in a string literal's raw text.
pub fn interpret_escapes(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// JSON-encode a string literal for embedding in a JSON constant.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Minimal JSON object parser for the `annotations()` builtin.
/// Accepts a flat object `{ "key": value, ... }` where value is a JSON
/// string, integer, float, bool, or null. Returns a Vec of (key, raw_value_json)
/// pairs in source order.
pub fn parse_simple_json(json: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes: Vec<char> = json.chars().collect();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != '{' {
        return out;
    }
    i += 1;
    loop {
        while i < bytes.len() && bytes[i].is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] == '}' {
            break;
        }
        if bytes[i] != '"' {
            break;
        }
        i += 1;
        let key_start = i;
        while i < bytes.len() && bytes[i] != '"' {
            if bytes[i] == '\\' {
                i += 2;
            } else {
                i += 1;
            }
        }
        let key_raw: String = bytes[key_start..i].iter().collect();
        let key = key_raw.replace("\\\"", "\"").replace("\\\\", "\\");
        if i < bytes.len() {
            i += 1;
        }
        while i < bytes.len() && (bytes[i].is_whitespace() || bytes[i] == ':') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let val_start = i;
        let value: String;
        match bytes[i] {
            '"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != '"' {
                    if bytes[i] == '\\' {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if i < bytes.len() {
                    i += 1;
                }
                let raw: String = bytes[val_start..i].iter().collect();
                value = raw;
            }
            '{' | '[' => {
                let open = bytes[i];
                let close = if open == '{' { '}' } else { ']' };
                let mut depth = 1;
                i += 1;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == open {
                        depth += 1;
                    } else if bytes[i] == close {
                        depth -= 1;
                    } else if bytes[i] == '"' {
                        i += 1;
                        while i < bytes.len() && bytes[i] != '"' {
                            if bytes[i] == '\\' {
                                i += 2;
                            } else {
                                i += 1;
                            }
                        }
                    }
                    if depth > 0 {
                        i += 1;
                    }
                }
                if i < bytes.len() {
                    i += 1;
                }
                let raw: String = bytes[val_start..i].iter().collect();
                value = json_escape(&raw);
            }
            _ => {
                while i < bytes.len()
                    && !bytes[i].is_whitespace()
                    && bytes[i] != ','
                    && bytes[i] != '}'
                {
                    i += 1;
                }
                let raw: String = bytes[val_start..i].iter().collect();
                value = raw;
            }
        }
        out.push((key, value));
        while i < bytes.len() && (bytes[i].is_whitespace() || bytes[i] == ',') {
            i += 1;
        }
    }
    out
}

/// Encode a function's annotation list as a JSON object string.
/// Each annotation contributes a key (its name) with the first positional
/// argument as the value. Annotations with no args map to `true`.
pub fn encode_annotations_json(anns: &[Annotation]) -> String {
    let mut map: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for a in anns {
        if a.arguments.is_empty() {
            map.entry(a.name.clone())
                .or_default()
                .push("true".to_string());
            continue;
        }
        let arg = &a.arguments[0];
        let val_json = json_value_of_expr(&arg.value);
        map.entry(a.name.clone()).or_default().push(val_json);
    }
    let mut out = String::from("{");
    let mut first = true;
    for (k, vs) in &map {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&json_escape(k));
        out.push(':');
        if vs.len() == 1 {
            out.push_str(&vs[0]);
        } else {
            out.push('[');
            for (i, v) in vs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(v);
            }
            out.push(']');
        }
    }
    out.push('}');
    out
}

/// Convert a literal expression to its JSON value form.
fn json_value_of_expr(e: &Expr) -> String {
    match e {
        Expr::Integer(i) => i.value.to_string(),
        Expr::Float(f) => format_float(f.value),
        Expr::Bool(b) => {
            if b.value {
                "true".into()
            } else {
                "false".into()
            }
        }
        Expr::Null(_) => "null".into(),
        Expr::String_(s) => {
            let mut t = String::new();
            for p in &s.parts {
                if let StringPart::Text(txt) = p {
                    t.push_str(&interpret_escapes(txt));
                }
            }
            json_escape(&t)
        }
        Expr::MultiLineString(s) => {
            let mut t = String::new();
            for p in &s.parts {
                if let StringPart::Text(txt) = p {
                    t.push_str(&interpret_escapes(txt));
                }
            }
            json_escape(&t)
        }
        _ => json_escape("<expr>"),
    }
}
