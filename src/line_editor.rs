//! Minimal line editor for the REPL — no external dependencies.
//!
//! Provides:
//! - Raw-mode character-at-a-time input (via `stty` to set/unset canonical
//!   mode, since the standard library does not expose termios).
//! - Command history with Up/Down navigation.
//! - Left/right cursor movement within the current line.
//! - Tab completion for builtins and globals.
//! - Ctrl+C cancels the current line; Ctrl+D on an empty line exits.
//!
//! The editor reads from stdin one byte at a time and writes the prompt
//! and echo to stdout. It is intentionally minimal — no multi-line
//! editing within a single read (the REPL handles multi-line input at a
//! higher level via continuation prompts).

use std::io::{self, BufRead, Read, Write};

/// Built-in function names and keywords available for Tab completion.
const BUILTIN_NAMES: &[&str] = &[
    // Builtins
    "len", "str", "int", "float", "bool", "type_of", "range", "sum",
    "min", "max", "sorted", "reversed", "map", "filter", "print",
    "println", "paste", "input", "open", "read", "write", "close",
    "read_file", "write_file", "file_exists", "is_dir", "is_file",
    "read_dir", "path_join", "basename", "dirname", "dict_keys",
    "dict_values", "dict_has", "dict_get", "dict_set", "split",
    "join", "trim", "upper", "lower", "contains", "freeze",
    "is_frozen", "exit", "enumerate", "zip", "list", "set", "tuple",
    "abs", "floor", "ceil", "round", "annotations", "assert",
    // Keywords
    "if", "elif", "else", "for", "fn", "class", "return", "break",
    "continue", "throw", "try", "catch", "finally", "with", "match",
    "import", "export", "set", "del", "defer", "yield", "spawn",
    "loop", "while", "struct", "enum", "trait", "impl", "dtor",
    "type", "const", "constexpr", "lazy", "macro", "extern",
    "test", "bench", "interface", "unsafe", "async", "await",
    "resume", "select", "case",
    // Stdlib module names
    "math", "io", "os", "time", "fs", "fmt", "json", "rand",
    "regex", "crypto", "sync", "net", "http", "image",
    "collections", "encoding", "csv", "xml", "toml", "yaml",
    "debug", "log", "term", "flag", "path", "compress",
    "websocket", "sql", "machine", "embed", "unsafe",
];

/// Stdlib module names for completion.
const STDLIB_MODULES: &[&str] = &[
    "math", "io", "os", "time", "fs", "fmt", "json", "rand",
    "regex", "crypto", "sync", "net", "http", "image",
    "collections", "encoding", "csv", "xml", "toml", "yaml",
    "debug", "log", "term", "flag", "path", "compress",
    "websocket", "sql", "machine", "embed",
];

/// The line editor state: the current input buffer, cursor position,
/// and command history.
pub struct LineEditor {
    /// All previously-entered lines (most recent last).
    history: Vec<String>,
    /// Index into history when navigating with Up/Down. `None` means
    /// "not navigating history" (editing a fresh line).
    history_idx: Option<usize>,
    /// Saved fresh line when the user starts navigating history, so
    /// Down past the end restores it.
    saved_line: String,
}

impl LineEditor {
    pub fn new() -> Self {
        LineEditor {
            history: Vec::new(),
            history_idx: None,
            saved_line: String::new(),
        }
    }

    /// Read a line with the given prompt. Returns `None` on Ctrl+D on an
    /// empty line (EOF). The `globals` map provides Tab-completion
    /// candidates for user-defined variables. When stdin is not a TTY
    /// (e.g. piped input), falls back to simple line-buffered reads.
    pub fn read_line(
        &mut self,
        prompt: &str,
        globals: &std::collections::HashMap<String, crate::bytecode::vm::Value>,
    ) -> Option<String> {
        let mut stdout = io::stdout();
        let _ = write!(stdout, "{}", prompt);
        let _ = stdout.flush();

        // If stdin is not a TTY, use simple line-buffered reads.
        if !is_tty() {
            let mut line = String::new();
            let n = io::stdin().lock().read_line(&mut line).unwrap_or(0);
            if n == 0 {
                return None;
            }
            // Strip the trailing newline.
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            if !line.trim().is_empty() {
                self.history.push(line.clone());
            }
            return Some(line);
        }

        // Enter raw mode via stty so we get character-at-a-time input.
        let was_raw = enable_raw_mode();
        let result = self.read_line_inner(&mut stdout, globals, prompt);
        // Restore the terminal regardless of how we exit.
        if was_raw {
            disable_raw_mode();
        }
        let _ = writeln!(stdout);
        result
    }

    fn read_line_inner(
        &mut self,
        stdout: &mut impl Write,
        globals: &std::collections::HashMap<String, crate::bytecode::vm::Value>,
        prompt: &str,
    ) -> Option<String> {
        let mut buf = String::new();
        let mut cursor: usize = 0; // byte offset within buf
        let stdin = io::stdin();
        let mut byte = [0u8; 1];
        // UTF-8 multi-byte buffer: accumulate bytes until we have a
        // complete character, then insert it.
        let mut utf8_buf: Vec<u8> = Vec::new();

        loop {
            if stdin.lock().read(&mut byte).unwrap_or(0) == 0 {
                // EOF (Ctrl+D with no input pending).
                if buf.is_empty() {
                    return None;
                }
                // Ctrl+D with input: act like Enter on some terminals.
                let line = buf.clone();
                if !line.trim().is_empty() {
                    self.history.push(line.clone());
                }
                return Some(line);
            }
            let c = byte[0];
            match c {
                // Enter (\r or \n) — submit the line.
                b'\r' | b'\n' => {
                    let line = buf.clone();
                    if !line.trim().is_empty() {
                        self.history.push(line.clone());
                    }
                    self.history_idx = None;
                    self.saved_line.clear();
                    return Some(line);
                }
                // Ctrl+C (0x03) — cancel the current line.
                0x03 => {
                    let _ = write!(stdout, "^C\n");
                    let _ = stdout.flush();
                    self.history_idx = None;
                    self.saved_line.clear();
                    return Some(String::new());
                }
                // Ctrl+D (0x04) on empty line — EOF.
                0x04 if buf.is_empty() => {
                    return None;
                }
                // Backspace (0x08) or Delete (0x7F).
                0x08 | 0x7F => {
                    if cursor > 0 {
                        // Find the previous char boundary.
                        let prev = buf[..cursor].char_indices().last().map(|(i, _)| i).unwrap_or(0);
                        buf.remove(prev);
                        cursor = prev;
                        self.redraw(stdout, prompt, &buf, cursor);
                    }
                }
                // Escape sequence — could be arrow keys, Home, End, Delete.
                0x1B => {
                    // Read the rest of the escape sequence: `[` then a char.
                    let mut seq = [0u8; 2];
                    if stdin.lock().read(&mut seq[0..1]).unwrap_or(0) == 1 && seq[0] == b'[' {
                        if stdin.lock().read(&mut seq[1..2]).unwrap_or(0) == 1 {
                            match seq[1] {
                                // Up arrow — previous history.
                                b'A' => {
                                    if self.history.is_empty() {
                                        continue;
                                    }
                                    if self.history_idx.is_none() {
                                        self.saved_line = buf.clone();
                                        self.history_idx = Some(self.history.len());
                                    }
                                    if let Some(idx) = &mut self.history_idx {
                                        if *idx > 0 {
                                            *idx -= 1;
                                            buf = self.history[*idx].clone();
                                            cursor = buf.len();
                                            self.redraw(stdout, prompt, &buf, cursor);
                                        }
                                    }
                                }
                                // Down arrow — next history.
                                b'B' => {
                                    if let Some(idx) = &mut self.history_idx {
                                        *idx += 1;
                                        if *idx >= self.history.len() {
                                            self.history_idx = None;
                                            buf = self.saved_line.clone();
                                            cursor = buf.len();
                                        } else {
                                            buf = self.history[*idx].clone();
                                            cursor = buf.len();
                                        }
                                        self.redraw(stdout, prompt, &buf, cursor);
                                    }
                                }
                                // Right arrow — move cursor right.
                                b'C' => {
                                    if cursor < buf.len() {
                                        // Advance by one char.
                                        let next = buf[cursor..]
                                            .char_indices()
                                            .nth(1)
                                            .map(|(i, _)| cursor + i)
                                            .unwrap_or(buf.len());
                                        cursor = next;
                                        let _ = write!(stdout, "\x1B[C");
                                        let _ = stdout.flush();
                                    }
                                }
                                // Left arrow — move cursor left.
                                b'D' => {
                                    if cursor > 0 {
                                        let prev = buf[..cursor]
                                            .char_indices()
                                            .last()
                                            .map(|(i, _)| i)
                                            .unwrap_or(0);
                                        cursor = prev;
                                        let _ = write!(stdout, "\x1B[D");
                                        let _ = stdout.flush();
                                    }
                                }
                                // Home (1~) and End (4~).
                                b'1' => {
                                    // Read the trailing ~.
                                    let mut tilde = [0u8; 1];
                                    let _ = stdin.lock().read(&mut tilde);
                                    cursor = 0;
                                    self.redraw(stdout, prompt, &buf, cursor);
                                }
                                b'4' => {
                                    let mut tilde = [0u8; 1];
                                    let _ = stdin.lock().read(&mut tilde);
                                    cursor = buf.len();
                                    self.redraw(stdout, prompt, &buf, cursor);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                // Tab (0x09) — completion.
                0x09 => {
                    // Build the candidate list: builtins + keywords +
                    // stdlib modules + current globals.
                    let mut candidates: Vec<&str> = BUILTIN_NAMES.to_vec();
                    let global_keys: Vec<String> =
                        globals.keys().map(|k| k.as_str().to_string()).collect();
                    let global_refs: Vec<&str> =
                        global_keys.iter().map(|s| s.as_str()).collect();
                    candidates.extend_from_slice(&global_refs);
                    if let Some(suffix) = Self::complete(&buf, &candidates) {
                        // Insert the completion suffix at the cursor.
                        for c in suffix.chars() {
                            buf.insert(cursor, c);
                            cursor += 1;
                        }
                        self.redraw(stdout, prompt, &buf, cursor);
                    }
                }
                // Regular printable character or UTF-8 byte.
                c if c >= 0x20 => {
                    // Check if this is a UTF-8 continuation byte.
                    if c >= 0x80 {
                        // Multi-byte UTF-8 character.
                        if utf8_buf.is_empty() {
                            // Start of a multi-byte sequence.
                            utf8_buf.push(c);
                        } else {
                            // Continuation byte.
                            utf8_buf.push(c);
                        }
                        // Try to decode the accumulated bytes.
                        if let Ok(s) = std::str::from_utf8(&utf8_buf) {
                            // Complete character — insert it.
                            for ch in s.chars() {
                                let ch_len = ch.len_utf8();
                                buf.insert_str(cursor, &ch.to_string());
                                cursor += ch_len;
                            }
                            utf8_buf.clear();
                            self.redraw(stdout, prompt, &buf, cursor);
                        }
                        // If decoding fails, we need more bytes — keep waiting.
                    } else {
                        // ASCII character — insert directly.
                        buf.insert(cursor, c as char);
                        cursor += 1;
                        self.redraw(stdout, prompt, &buf, cursor);
                    }
                }
                _ => {}
            }
        }
    }

    /// Redraw the current line: move to the start of the prompt area,
    /// clear to end of line, write the prompt + buffer, then position the cursor.
    fn redraw(&self, stdout: &mut impl Write, prompt: &str, buf: &str, cursor: usize) {
        // \r moves to column 0; \x1B[K clears to end of line.
        let _ = write!(stdout, "\r\x1B[K{}{}", prompt, buf);
        // Move cursor back to the right position.
        let chars_after_cursor = buf[cursor..].chars().count();
        if chars_after_cursor > 0 {
            let _ = write!(stdout, "\x1B[{}D", chars_after_cursor);
        }
        let _ = stdout.flush();
    }

    /// Add a line to the history (used by the REPL when it assembles a
    /// multi-line input from several reads).
    pub fn add_history(&mut self, line: &str) {
        if !line.trim().is_empty() {
            self.history.push(line.to_string());
        }
    }

    /// Return candidate completions for the current word at the end of
    /// `input`. A "word" here is the maximal suffix of alphanumeric
    /// characters and underscores. Returns the common prefix of all
    /// matching candidates from `candidates`.
    pub fn complete(input: &str, candidates: &[&str]) -> Option<String> {
        // Find the start of the current word.
        let word_start = input
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
            .last()
            .map(|(i, _)| i)
            .unwrap_or(input.len());
        let word = &input[word_start..];
        if word.is_empty() {
            return None;
        }
        let matches: Vec<&str> = candidates
            .iter()
            .filter(|c| c.starts_with(word))
            .copied()
            .collect();
        if matches.is_empty() {
            return None;
        }
        // Find the common prefix.
        let mut prefix = matches[0].to_string();
        for m in &matches[1..] {
            let len = prefix
                .char_indices()
                .zip(m.chars())
                .take_while(|((_, a), b)| a == b)
                .count();
            prefix.truncate(len);
        }
        if prefix.len() > word.len() {
            Some(prefix[word.len()..].to_string())
        } else if matches.len() > 1 {
            // Show all matches.
            None
        } else {
            // Single exact match — add a space.
            Some(" ".to_string())
        }
    }
}

/// Enable terminal raw mode by invoking `stty`. Returns true if the
/// terminal was put into raw mode (so the caller should restore it).
/// Uses `stty` rather than direct termios syscalls to avoid needing the
/// `libc` or `nix` crates (no new dependencies).
fn enable_raw_mode() -> bool {
    // Only enable raw mode if stdin is a TTY.
    if !is_tty() {
        return false;
    }
    let result = std::process::Command::new("stty")
        .arg("-echo")
        .arg("raw")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    result.map(|s| s.success()).unwrap_or(false)
}

/// Disable terminal raw mode (restore canonical mode + echo).
fn disable_raw_mode() {
    let _ = std::process::Command::new("stty")
        .arg("echo")
        .arg("cooked")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Check if stdin is a TTY by trying to read its attributes via `stty`.
fn is_tty() -> bool {
    let result = std::process::Command::new("stty")
        .arg("-a")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    result.map(|s| s.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_complete_single_match() {
        // "println" is the only candidate starting with "println".
        let candidates = ["println", "print", "paste", "len", "range"];
        let result = LineEditor::complete("println", &candidates);
        assert_eq!(result, Some(" ".to_string()));
    }

    #[test]
    fn test_complete_multiple_matches_common_prefix() {
        // "pri" matches "println" and "print"; common prefix is "print",
        // so the extension beyond "pri" is "nt".
        let candidates = ["println", "print", "paste", "len", "range"];
        let result = LineEditor::complete("pri", &candidates);
        assert_eq!(result, Some("nt".to_string()));
    }

    #[test]
    fn test_complete_no_match() {
        let candidates = ["println", "print", "paste"];
        let result = LineEditor::complete("xyz", &candidates);
        assert_eq!(result, None);
    }

    #[test]
    fn test_complete_empty_word() {
        let candidates = ["println", "print"];
        let result = LineEditor::complete("", &candidates);
        assert_eq!(result, None);
    }

    #[test]
    fn test_complete_exact_match_adds_space() {
        let candidates = ["println", "print"];
        let result = LineEditor::complete("println", &candidates);
        assert_eq!(result, Some(" ".to_string()));
    }

    #[test]
    fn test_history_add_and_navigate() {
        let mut editor = LineEditor::new();
        editor.add_history("first");
        editor.add_history("second");
        assert_eq!(editor.history.len(), 2);
        assert_eq!(editor.history[0], "first");
        assert_eq!(editor.history[1], "second");
    }

    #[test]
    fn test_history_ignores_empty() {
        let mut editor = LineEditor::new();
        editor.add_history("");
        editor.add_history("   ");
        assert_eq!(editor.history.len(), 0);
    }
}
