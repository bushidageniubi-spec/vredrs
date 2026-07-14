//! Compile-time Risk Map generation — Vredrs's third paradigm innovation.
//!
//! When the ILEI analyzer (ilei.rs) collects fault points, this module
//! generates a human-readable **Risk Map** — a text file that documents
//! every location in the source code where a potential crash could occur,
//! what type of crash it is, and how severe it is.
//!
//! The Risk Map is the "airbag dashboard" — it tells you exactly where
//! the airbags are before you drive. You can then choose to fix the
//! issues, accept the risks, or add more @assert safe annotations to
//! let the compiler prove safety.
//!
//! ## Output Format
//!
//! ```text
//! ═══════════════════════════════════════════════════════════════
//!  Vredrs Risk Map — Generated at compile time
//!  Source: main.vraw
//!  Total fault points: 3
//!  Critical: 1 | High: 1 | Medium: 1 | Low: 0
//! ═══════════════════════════════════════════════════════════════
//!
//!  [CRITICAL] main.vraw:15:5 — NullDeref
//!    Function: read_ptr
//!    Description: @assert safe FAILED: pointer 'p' is definitely null
//!    at this point — load will crash
//!    Action: Check pointer before use, or remove @assert safe
//!
//!  [HIGH] main.vraw:22:5 — DivisionByZero
//!    Function: divide
//!    Description: @inject fault: division-by-zero check injected
//!    Action: Verify divisor is non-zero, or keep the fault check
//!
//!  [MEDIUM] main.vraw:30:5 — OutOfBounds
//!    Function: process_array
//!    Description: @inject fault: bounds check injected for array access
//!    Action: Add bounds checking, or accept the fault check
//!
//! ═══════════════════════════════════════════════════════════════
//!  Summary:
//!    1 pointer safety issue(s) — consider @assert safe or null checks
//!    1 division safety issue(s) — verify divisors
//!    1 array safety issue(s) — add bounds checks
//! ═══════════════════════════════════════════════════════════════
//! ```

use crate::semantic::ilei::{FaultPoint, FaultType, Severity};

/// Generate a human-readable Risk Map from a list of fault points.
pub fn generate_risk_map(faults: &[FaultPoint], source_name: &str) -> String {
    let mut out = String::new();

    let critical = faults.iter().filter(|f| f.severity == Severity::Critical).count();
    let high = faults.iter().filter(|f| f.severity == Severity::High).count();
    let medium = faults.iter().filter(|f| f.severity == Severity::Medium).count();
    let low = faults.iter().filter(|f| f.severity == Severity::Low).count();

    // Header
    out.push_str("═══════════════════════════════════════════════════════════════\n");
    out.push_str(" Vredrs Risk Map — Generated at compile time\n");
    out.push_str(&format!(" Source: {}\n", source_name));
    out.push_str(&format!(" Total fault points: {}\n", faults.len()));
    out.push_str(&format!(" Critical: {} | High: {} | Medium: {} | Low: {}\n",
        critical, high, medium, low));
    out.push_str("═══════════════════════════════════════════════════════════════\n\n");

    if faults.is_empty() {
        out.push_str(" ✓ No fault points detected. All code paths verified safe.\n\n");
        out.push_str("═══════════════════════════════════════════════════════════════\n");
        return out;
    }

    // Sort by severity (Critical first).
    let mut sorted_faults: Vec<&FaultPoint> = faults.iter().collect();
    sorted_faults.sort_by(|a, b| {
        let sev_order = |s: &Severity| match s {
            Severity::Critical => 0,
            Severity::High => 1,
            Severity::Medium => 2,
            Severity::Low => 3,
        };
        sev_order(&a.severity).cmp(&sev_order(&b.severity))
    });

    // Detail each fault point.
    for f in &sorted_faults {
        let sev_label = match f.severity {
            Severity::Critical => "CRITICAL",
            Severity::High => "HIGH",
            Severity::Medium => "MEDIUM",
            Severity::Low => "LOW",
        };
        let type_label = match f.fault_type {
            FaultType::NullDeref => "NullDeref",
            FaultType::UseAfterFree => "UseAfterFree",
            FaultType::OutOfBounds => "OutOfBounds",
            FaultType::IntegerOverflow => "IntegerOverflow",
            FaultType::DivisionByZero => "DivisionByZero",
            FaultType::UninitializedRead => "UninitializedRead",
            FaultType::StackOverflow => "StackOverflow",
            FaultType::Generic => "Generic",
        };

        out.push_str(&format!(
            " [{}] {}:{}:{} — {}\n",
            sev_label, source_name, f.span.line, f.span.column, type_label
        ));
        out.push_str(&format!("   Function: {}\n", f.function));
        out.push_str(&format!("   Description: {}\n", f.description));

        let action = match f.fault_type {
            FaultType::NullDeref => "Check pointer before use, or remove @assert safe",
            FaultType::UseAfterFree => "Verify pointer lifetime, or use AOR arena promotion",
            FaultType::OutOfBounds => "Add bounds checking, or accept the fault check",
            FaultType::IntegerOverflow => "Use checked arithmetic, or accept the fault check",
            FaultType::DivisionByZero => "Verify divisor is non-zero, or keep the fault check",
            FaultType::UninitializedRead => "Initialize the variable before use",
            FaultType::StackOverflow => "Reduce recursion depth, or use iterative approach",
            FaultType::Generic => "Review the code path for potential issues",
        };
        out.push_str(&format!("   Action: {}\n", action));
        out.push('\n');
    }

    // Summary
    out.push_str("═══════════════════════════════════════════════════════════════\n");
    out.push_str(" Summary:\n");

    let null_count = faults.iter().filter(|f| f.fault_type == FaultType::NullDeref).count();
    let div_count = faults.iter().filter(|f| f.fault_type == FaultType::DivisionByZero).count();
    let oob_count = faults.iter().filter(|f| f.fault_type == FaultType::OutOfBounds).count();
    let ovf_count = faults.iter().filter(|f| f.fault_type == FaultType::IntegerOverflow).count();
    let uaf_count = faults.iter().filter(|f| f.fault_type == FaultType::UseAfterFree).count();

    if null_count > 0 {
        out.push_str(&format!("   {} pointer safety issue(s) — consider @assert safe or null checks\n", null_count));
    }
    if div_count > 0 {
        out.push_str(&format!("   {} division safety issue(s) — verify divisors\n", div_count));
    }
    if oob_count > 0 {
        out.push_str(&format!("   {} array safety issue(s) — add bounds checks\n", oob_count));
    }
    if ovf_count > 0 {
        out.push_str(&format!("   {} integer overflow issue(s) — use checked arithmetic\n", ovf_count));
    }
    if uaf_count > 0 {
        out.push_str(&format!("   {} use-after-free issue(s) — verify pointer lifetime\n", uaf_count));
    }
    if null_count + div_count + oob_count + ovf_count + uaf_count == 0 {
        out.push_str("   No categorized issues — all faults are generic\n");
    }

    out.push_str("═══════════════════════════════════════════════════════════════\n");

    out
}

/// Write the Risk Map to a file (e.g., `output.riskmap`).
pub fn write_risk_map(faults: &[FaultPoint], source_name: &str, output_path: &str) -> std::io::Result<()> {
    let content = generate_risk_map(faults, source_name);
    std::fs::write(output_path, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::ilei::{analyze_program, FaultType, Severity};
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(src: &str) -> crate::parser::ast::Program {
        let mut lexer = Lexer::new(src, 0);
        let tokens = lexer.tokenize().expect("tokenize");
        let mut parser = Parser::new(tokens, 0);
        parser.parse_program().expect("parse")
    }

    #[test]
    fn test_empty_risk_map() {
        let map = generate_risk_map(&[], "test.vraw");
        assert!(map.contains("No fault points detected"));
        assert!(map.contains("✓"));
    }

    #[test]
    fn test_risk_map_with_faults() {
        let src = r#"
@inject_fault
fn, risky(p: ptr[u8])
    set, val, p.load()
/end
"#;
        let prog = parse(src);
        let faults = analyze_program(&prog);
        assert!(!faults.is_empty());
        let map = generate_risk_map(&faults, "test.vraw");
        assert!(map.contains("NullDeref"));
        assert!(map.contains("risky"));
        assert!(map.contains("pointer safety"));
        assert!(map.contains("Summary"));
    }

    #[test]
    fn test_severity_sorting() {
        let faults = vec![
            FaultPoint {
                function: "low_fn".to_string(),
                span: crate::error::Span::dummy(),
                fault_type: FaultType::Generic,
                severity: Severity::Low,
                description: "low".to_string(),
                proved_safe: false,
            },
            FaultPoint {
                function: "critical_fn".to_string(),
                span: crate::error::Span::dummy(),
                fault_type: FaultType::NullDeref,
                severity: Severity::Critical,
                description: "critical".to_string(),
                proved_safe: false,
            },
        ];
        let map = generate_risk_map(&faults, "test.vraw");
        // Critical should appear before Low.
        let crit_pos = map.find("CRITICAL").unwrap_or(usize::MAX);
        let low_pos = map.find("[LOW]").unwrap_or(usize::MAX);
        assert!(crit_pos < low_pos, "Critical should come before Low");
    }
}
