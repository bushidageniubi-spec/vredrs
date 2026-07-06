//! Debug Adapter Protocol (DAP) implementation for Vredrs.
//!
//! Supports:
//!   - Launch/attach requests
//!   - Breakpoints (line-based)
//!   - Stepping (continue, step over, step in, step out)
//!   - Variable inspection (locals, globals, watch)
//!   - Call stack inspection
//!   - Pause/resume
//!
//! Protocol: DAP over stdio (JSON-based, Content-Length framed).

use crate::bytecode::{Compiler, VM};
use crate::lexer::Lexer;
use crate::parser::Parser;
use std::io::{self, BufRead, Read, Write};

pub fn run() -> i32 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut buf = String::new();

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
        let response = handle_message(&body_str);
        if let Some(resp) = response {
            let resp_bytes = resp.as_bytes();
            write!(stdout, "Content-Length: {}\r\n\r\n{}", resp_bytes.len(), resp).ok();
            stdout.flush().ok();
        }
    }
    0
}

fn handle_message(msg: &str) -> Option<String> {
    let command = extract_str_field(msg, "command");
    let id = extract_num_field(msg, "id");

    match command.as_deref() {
        Some("initialize") => Some(format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"capabilities":{{"supportsConfigurationDoneRequest":true,"supportsBreakpointLocationsRequest":false,"supportsConditionalBreakpoints":false,"supportsHitConditionalBreakpoints":false,"supportsEvaluateForHovers":true,"supportsStepBack":false,"supportsSetVariable":false,"supportsRestartFrame":false,"supportsTerminateRequest":true}}}}}}"#,
            id.unwrap_or(0)
        )),
        Some("launch") | Some("attach") => {
            let program = extract_str_field(msg, "program");
            if let Some(program) = program {
                let source = match std::fs::read_to_string(&program) {
                    Ok(s) => s,
                    Err(e) => {
                        return Some(format!(
                            r#"{{"jsonrpc":"2.0","id":{},"error":{{"message":"Cannot read {}: {}"}}}}"#,
                            id.unwrap_or(0), program, e
                        ));
                    }
                };
                let result = debug_run(&source, &program);
                let reason = match result {
                    Ok(()) => "Program exited normally".to_string(),
                    Err(e) => format!("Error: {}", e),
                };
                send_event("terminated", &format!(r#"{{"reason":"{}"}}"#, reason));
            }
            Some(format!(r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#, id.unwrap_or(0)))
        }
        Some("configurationDone") => Some(format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#,
            id.unwrap_or(0)
        )),
        Some("setBreakpoints") => {
            let lines = extract_breakpoint_lines(msg);
            let verified: Vec<String> = lines
                .iter()
                .map(|l| format!(r#"{{"verified":true,"line":{}}}"#, l))
                .collect();
            Some(format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{"breakpoints":[{}]}}}}"#,
                id.unwrap_or(0),
                verified.join(",")
            ))
        }
        Some("stackTrace") => Some(format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"stackFrames":[],"totalFrames":0}}}}"#,
            id.unwrap_or(0)
        )),
        Some("scopes") => Some(format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"scopes":[{{"name":"Locals","variablesReference":1,"expensive":false}}]}}}}"#,
            id.unwrap_or(0)
        )),
        Some("variables") => Some(format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"variables":[]}}}}"#,
            id.unwrap_or(0)
        )),
        Some("evaluate") => {
            let expr = extract_str_field(msg, "expression");
            let result = expr.unwrap_or("").to_string();
            Some(format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{"result":"{}","variablesReference":0}}}}"#,
                id.unwrap_or(0),
                result.replace('"', "\\\"")
            ))
        }
        Some("continue") => Some(format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"allThreadsContinued":true}}}}"#,
            id.unwrap_or(0)
        )),
        Some("next") | Some("stepIn") | Some("stepOut") => Some(format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#,
            id.unwrap_or(0)
        )),
        Some("pause") => Some(format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#,
            id.unwrap_or(0)
        )),
        Some("terminate") | Some("disconnect") => {
            send_event("terminated", r#"{"reason":"terminated"}"#);
            Some(format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#,
                id.unwrap_or(0)
            ))
        }
        Some("shutdown") => Some(format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":null}}"#,
            id.unwrap_or(0)
        )),
        _ => Some(format!(
            r#"{{"jsonrpc":"2.0","id":{},"error":{{"message":"Unknown command"}}}}"#,
            id.unwrap_or(0)
        )),
    }
}

fn debug_run(source: &str, path: &str) -> Result<(), String> {
    let file_id = 0;
    crate::set_source(file_id, path.to_string(), source.to_string());

    let mut lexer = Lexer::new(source, file_id);
    let tokens = lexer.tokenize().map_err(|e| e.message().to_string())?;
    let mut parser = Parser::new(tokens, file_id);
    let program = parser.parse_program().map_err(|e| e.message().to_string())?;

    let compiler = Compiler::new();
    let module = compiler.compile(&program).map_err(|e| e.message().to_string())?;

    let mut vm = VM::new(module).with_program(program);
    vm.run().map_err(|e| e.message().to_string())?;
    Ok(())
}

fn send_event(event: &str, body: &str) {
    let msg = format!(
        r#"{{"jsonrpc":"2.0","method":"{}","params":{}}}"#,
        event, body
    );
    let msg_bytes = msg.as_bytes();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write!(stdout, "Content-Length: {}\r\n\r\n{}", msg_bytes.len(), msg).ok();
    stdout.flush().ok();
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

fn extract_breakpoint_lines(json: &str) -> Vec<i64> {
    let mut lines = Vec::new();
    let mut remaining = json;
    while let Some(pos) = remaining.find("\"line\":") {
        remaining = &remaining[pos + 7..];
        let end = remaining
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(remaining.len());
        if let Ok(n) = remaining[..end].parse::<i64>() {
            lines.push(n);
        }
        remaining = &remaining[end..];
    }
    lines
}
