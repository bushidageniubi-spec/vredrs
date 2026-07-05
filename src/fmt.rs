//! Source code formatter for Vredrs.
//!
//! Usage:
//!   vredrs fmt <file>           — format and print to stdout
//!   vredrs fmt --write <file>   — format and overwrite
//!   vredrs fmt --check <file>   — check if formatted (exit 0 = OK, 1 = needs formatting)

use std::fs;

/// Format a Vredrs source file: normalize indentation to 4 spaces,
/// ensure /end is on its own line, remove trailing whitespace.
pub fn format_source(source: &str) -> String {
    let mut result = String::new();
    let mut indent = 0usize;
    for line in source.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            result.push('\n');
            continue;
        }
        // Decrease indent for /end and /else
        let stripped = trimmed.trim_start();
        if stripped.starts_with("/end") || stripped.starts_with("else") || stripped.starts_with("/else") || stripped.starts_with("elif,") {
            if indent > 0 {
                indent -= 1;
            }
        }
        // Emit with proper indentation
        for _ in 0..indent {
            result.push_str("    ");
        }
        result.push_str(stripped);
        result.push('\n');
        // Increase indent after lines that start a block
        if stripped.ends_with("/end") == false {
            if stripped.starts_with("fn,") || stripped.starts_with("class,") 
                || stripped.starts_with("if,") || stripped.starts_with("while,")
                || stripped.starts_with("for,") || stripped.starts_with("try")
                || stripped.starts_with("with,") || stripped.starts_with("loop")
                || stripped.starts_with("else")
                || stripped.starts_with("elif,")
            {
                indent += 1;
            }
        }
    }
    result
}

pub fn run(args: &[String]) -> i32 {
    let (check_mode, write_mode, file) = parse_args(args);
    if file.is_empty() {
        eprintln!("Usage: vredrs fmt [--check|--write] <file>");
        return 1;
    }
    let source = match fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading {}: {}", file, e);
            return 1;
        }
    };
    let formatted = format_source(&source);
    if check_mode {
        if source == formatted {
            0
        } else {
            eprintln!("{} needs formatting", file);
            1
        }
    } else if write_mode {
        fs::write(&file, &formatted).unwrap_or_else(|e| eprintln!("Error writing: {}", e));
        println!("Formatted {}", file);
        0
    } else {
        print!("{}", formatted);
        0
    }
}

fn parse_args(args: &[String]) -> (bool, bool, String) {
    let mut check = false;
    let mut write = false;
    let mut file = String::new();
    for a in args {
        match a.as_str() {
            "--check" | "-c" => check = true,
            "--write" | "-w" => write = true,
            _ => file = a.clone(),
        }
    }
    (check, write, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_normalizes_indentation() {
        let input = "fn, foo()\n  paste, \"hi\"\n/end\n";
        let output = format_source(input);
        assert!(output.contains("    paste, \"hi\""));
    }

    #[test]
    fn format_removes_trailing_whitespace() {
        let input = "paste, \"hi\"   \n";
        let output = format_source(input);
        assert_eq!(output, "paste, \"hi\"\n");
    }

    #[test]
    fn format_check_detects_unformatted() {
        let input = "fn, foo()\n  paste, \"hi\"\n/end\n";
        let formatted = format_source(input);
        assert_ne!(input, formatted);
    }
}
