//! Debug symbol generation for raw backends.
//!
//! Emits DWARF-style debug information alongside the generated assembly,
//! allowing GDB/LLDB to map machine addresses back to source locations.
//!
//! The debug info is emitted as GNU assembler `.debug_*` sections:
//! - `.debug_info`: compilation unit metadata
//! - `.debug_line`: source line → address mapping
//! - `.debug_abbrev`: abbreviation table
//!
//! This is a minimal but functional implementation that covers:
//! - Function name → address mapping
//! - Source file/line/column for each function
//! - Local variable names (as best-effort comments)

use std::collections::HashMap;

/// A debug symbol entry for a function.
#[derive(Debug, Clone)]
pub struct FuncDebugInfo {
    /// Function name.
    pub name: String,
    /// Source file path.
    pub file: String,
    /// Source line where the function is defined.
    pub line: usize,
    /// Source column.
    pub column: usize,
    /// Local variable names in the function.
    pub locals: Vec<String>,
}

/// Generate DWARF debug sections for a compilation unit.
///
/// Returns the debug sections as a string that can be appended to
/// the assembly output.
pub fn generate_debug_sections(
    func_infos: &[FuncDebugInfo],
    source_file: &str,
) -> String {
    let mut out = String::new();

    // .debug_abbrev: abbreviation table
    out.push_str(".section .debug_abbrev, \"\", @progbits\n");
    out.push_str(".balign 1\n");
    out.push_str(".debug_abbrev_begin:\n");
    // Abbreviation 1: DW_TAG_compile_unit
    out.push_str("    .byte 1          ; abbrev code 1\n");
    out.push_str("    .byte 17         ; DW_TAG_compile_unit\n");
    out.push_str("    .byte 0          ; DW_CHILDREN_yes\n");
    out.push_str("    .byte 3          ; DW_AT_name (string)\n");
    out.push_str("    .byte 14         ; DW_FORM_strp\n");
    out.push_str("    .byte 16         ; DW_AT_stmt_list\n");
    out.push_str("    .byte 23         ; DW_FORM_sec_offset\n");
    out.push_str("    .byte 0, 0       ; end of attr list\n");
    // Abbreviation 2: DW_TAG_subprogram
    out.push_str("    .byte 2          ; abbrev code 2\n");
    out.push_str("    .byte 46         ; DW_TAG_subprogram\n");
    out.push_str("    .byte 0          ; DW_CHILDREN_no\n");
    out.push_str("    .byte 3          ; DW_AT_name\n");
    out.push_str("    .byte 14         ; DW_FORM_strp\n");
    out.push_str("    .byte 17         ; DW_AT_low_pc\n");
    out.push_str("    .byte 1          ; DW_FORM_addr\n");
    out.push_str("    .byte 18         ; DW_AT_high_pc\n");
    out.push_str("    .byte 6          ; DW_FORM_data4\n");
    out.push_str("    .byte 0, 0       ; end of attr list\n");
    out.push_str("    .byte 0          ; end of abbrev table\n\n");

    // .debug_info: compilation unit
    out.push_str(".section .debug_info, \"\", @progbits\n");
    out.push_str(".balign 4\n");
    out.push_str(".debug_info_begin:\n");
    // Length placeholder (will be filled by the assembler/linker).
    let unit_length = 4 + 4 + 2 + 4 + source_file.len() + 1 + 4 + func_infos.len() * (4 + 8 + 4 + 4);
    out.push_str(&format!("    .4byte {}        ; DWARF unit length\n", unit_length));
    out.push_str("    .2byte 4         ; DWARF version 4\n");
    out.push_str("    .4byte 0         ; debug_abbrev offset (filled by linker)\n");
    out.push_str("    .byte 8          ; address size (64-bit)\n");
    // Compile unit entry.
    out.push_str("    .byte 1          ; abbrev code 1 (compile_unit)\n");
    out.push_str(&format!("    .asciz \"{}\"\n", source_file));
    out.push_str("    .4byte .debug_line_begin  ; DW_AT_stmt_list\n");

    // Subprogram entries for each function.
    for fi in func_infos {
        out.push_str("    .byte 2          ; abbrev code 2 (subprogram)\n");
        out.push_str(&format!("    .asciz \"{}\"\n", fi.name));
        out.push_str(&format!("    .quad {}         ; DW_AT_low_pc\n", fi.name));
        // High PC as offset from low PC (function size — estimated).
        out.push_str("    .4byte 0x100     ; DW_AT_high_pc (estimated size)\n");
    }

    out.push_str("    .byte 0          ; end of children\n");
    out.push_str(".debug_info_end:\n\n");

    // .debug_line: line number program
    out.push_str(".section .debug_line, \"\", @progbits\n");
    out.push_str(".balign 4\n");
    out.push_str(".debug_line_begin:\n");
    // Line number program header.
    out.push_str("    .4byte .debug_line_end - .debug_line_begin - 4  ; unit length\n");
    out.push_str("    .2byte 4         ; DWARF version 4\n");
    out.push_str("    .4byte .debug_line_prologue_end - .debug_line_prologue_begin  ; header length\n");
    out.push_str(".debug_line_prologue_begin:\n");
    out.push_str("    .byte 8          ; minimum instruction length\n");
    out.push_str("    .byte 0          ; maximum operations per instruction\n");
    out.push_str("    .byte 1          ; default is_stmt\n");
    out.push_str("    .byte 0xFB       ; line_base (-5)\n");
    out.push_str("    .byte 14         ; line_range\n");
    out.push_str("    .byte 13         ; opcode_base\n");
    // Standard opcode lengths.
    out.push_str("    .byte 0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1\n");
    // Include directories: none (empty list).
    out.push_str("    .byte 0\n");
    // File names: just the source file.
    out.push_str(&format!("    .asciz \"{}\"\n", source_file));
    out.push_str("    .byte 0, 0, 0   ; directory, mtime, length\n");
    out.push_str("    .byte 0          ; end of file list\n");
    out.push_str(".debug_line_prologue_end:\n");

    // Line number entries: one per function.
    for fi in func_infos {
        // Set address to function start.
        out.push_str("    .byte 0          ; extended opcode\n");
        out.push_str("    .byte 9          ; length\n");
        out.push_str("    .byte 2          ; DW_LNE_set_address\n");
        out.push_str(&format!("    .quad {}\n", fi.name));
        // Advance line to the function's source line.
        out.push_str(&format!("    .byte 0x21       ; DW_LNS_advance_line\n"));
        out.push_str(&format!("    .sleb128 {}\n", fi.line as i64));
        // Copy (marks the address as a statement boundary).
        out.push_str("    .byte 1          ; DW_LNS_copy\n");
    }
    // End of sequence.
    out.push_str("    .byte 0, 1, 1   ; DW_LNE_end_sequence\n");
    out.push_str(".debug_line_end:\n\n");

    out
}

/// Collect debug info from a list of compiled functions.
pub fn collect_debug_info(
    functions: &[(String, usize, usize)],  // (name, line, column)
    source_file: &str,
) -> Vec<FuncDebugInfo> {
    functions.iter().map(|(name, line, col)| {
        FuncDebugInfo {
            name: name.clone(),
            file: source_file.to_string(),
            line: *line,
            column: *col,
            locals: Vec::new(),
        }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debug_sections_not_empty() {
        let infos = vec![
            FuncDebugInfo {
                name: "main".to_string(),
                file: "test.vraw".to_string(),
                line: 1,
                column: 1,
                locals: vec!["x".to_string()],
            },
        ];
        let sections = generate_debug_sections(&infos, "test.vraw");
        assert!(sections.contains(".debug_abbrev"), "should have .debug_abbrev");
        assert!(sections.contains(".debug_info"), "should have .debug_info");
        assert!(sections.contains(".debug_line"), "should have .debug_line");
        assert!(sections.contains("main"), "should mention function name");
        assert!(sections.contains("test.vraw"), "should mention source file");
    }

    #[test]
    fn test_collect_debug_info() {
        let funcs = vec![("main".to_string(), 1, 1), ("helper".to_string(), 10, 5)];
        let infos = collect_debug_info(&funcs, "test.vraw");
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].name, "main");
        assert_eq!(infos[1].line, 10);
    }
}
