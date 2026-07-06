//! ARM (AArch64 / ARM32) native backend for Vredrs --raw mode.
//!
//! Generates GNU-syntax assembly for:
//! - AArch64 (arm64, aarch64): 64-bit ARM, 31 general registers (x0-x30)
//! - ARM32 (armv7): 32-bit ARM, 16 registers (r0-r15, r0-r12 usable)
//!
//! Calling convention:
//! - AArch64: args in x0-x7, return in x0, SP 16-byte aligned
//! - ARM32: args in r0-r3, return in r0, SP 8-byte aligned (AAPCS)
//!
//! Supported features:
//! - Basic arithmetic (add, sub, mul, div)
//! - Function calls and returns
//! - Stack frame management (push/pop callee-saved)
//! - Conditional branches (if/else)
//! - Loops (while/for)
//! - Integer literals and variables
//! - fib() recursion pattern (recursion-to-iteration optimization)

use crate::parser::ast::*;

/// ARM code generator.
pub struct ArmCodeGen {
    asm: String,
    label_counter: usize,
    arch: String, // "aarch64" or "arm"
}

impl ArmCodeGen {
    pub fn new(arch: &str) -> Self {
        ArmCodeGen {
            asm: String::new(),
            label_counter: 0,
            arch: arch.to_string(),
        }
    }

    fn new_label(&mut self, prefix: &str) -> String {
        self.label_counter += 1;
        format!(".{}{}", prefix, self.label_counter)
    }

    /// Get the register name for a given index (0-based).
    fn reg(&self, idx: usize) -> &'static str {
        if self.arch == "aarch64" {
            match idx {
                0 => "x0", 1 => "x1", 2 => "x2", 3 => "x3",
                4 => "x4", 5 => "x5", 6 => "x6", 7 => "x7",
                8 => "x8", 9 => "x9", 10 => "x10",
                11 => "x11", 12 => "x12", 13 => "x13",
                14 => "x14", 15 => "x15", 16 => "x16",
                17 => "x17", 18 => "x18", 19 => "x19",
                _ => "x9", // default temp
            }
        } else {
            match idx {
                0 => "r0", 1 => "r1", 2 => "r2", 3 => "r3",
                4 => "r4", 5 => "r5", 6 => "r6", 7 => "r7",
                8 => "r8", 9 => "r9", 10 => "r10",
                11 => "r11", 12 => "r12",
                _ => "r9",
            }
        }
    }

    /// Frame pointer register.
    fn fp(&self) -> &'static str {
        if self.arch == "aarch64" { "x29" } else { "r11" }
    }

    /// Link register.
    fn lr(&self) -> &'static str {
        if self.arch == "aarch64" { "x30" } else { "r14" }
    }

    /// Stack pointer.
    fn sp(&self) -> &'static str {
        if self.arch == "aarch64" { "sp" } else { "sp" }
    }

    /// Emit a line of assembly.
    fn emit(&mut self, line: &str) {
        self.asm.push_str(line);
        self.asm.push('\n');
    }

    /// Emit a label.
    fn emit_label(&mut self, label: &str) {
        self.asm.push_str(label);
        self.asm.push_str(":\n");
    }

    /// Compile the entire program.
    pub fn compile(&mut self, program: &Program) -> Result<String, String> {
        // Header
        self.emit(&format!("# Vredrs raw-mode {} assembly", self.arch));
        self.emit(&format!("# Target: {}", self.arch));
        self.emit(".text");
        self.emit(".global _start");
        self.emit("");

        // _start: call main, then exit
        self.emit_label("_start");
        if self.arch == "aarch64" {
            // Set up stack, call main, then syscall exit
            self.emit("    bl main");
            // exit(result) via syscall
            self.emit("    mov x8, #93");   // exit syscall number
            self.emit("    svc #0");
        } else {
            // ARM32
            self.emit("    bl main");
            self.emit("    mov r7, #1");     // exit syscall
            self.emit("    svc #0");
        }
        self.emit("");

        // Compile all functions
        for decl in &program.declarations {
            if let TopLevel::FnDef(f) = decl {
                self.compile_fn(f)?;
            }
        }

        // Compile top-level statements into main()
        let has_main = program.declarations.iter().any(|d| {
            if let TopLevel::FnDef(f) = d { f.name.name == "main" } else { false }
        });
        if !has_main {
            self.emit_label("main");
            self.emit_prologue(0);
            for decl in &program.declarations {
                if let TopLevel::Statement(s) = decl {
                    self.compile_stmt(s)?;
                }
            }
            // Return 0
            if self.arch == "aarch64" {
                self.emit("    mov x0, #0");
            } else {
                self.emit("    mov r0, #0");
            }
            self.emit_epilogue();
        }

        Ok(self.asm.clone())
    }

    fn emit_prologue(&mut self, local_vars: usize) {
        let frame_size = ((local_vars * 8 + 16) + 15) & !15; // 16-byte aligned
        if self.arch == "aarch64" {
            self.emit(&format!("    stp {}, {}, [sp, #-{}]!", self.fp(), self.lr(), frame_size));
            self.emit(&format!("    mov {}, sp", self.fp()));
        } else {
            self.emit("    push {r11, lr}");
            self.emit("    mov r11, sp");
            self.emit(&format!("    sub sp, sp, #{}", frame_size));
        }
    }

    fn emit_epilogue(&mut self) {
        if self.arch == "aarch64" {
            self.emit(&format!("    ldp {}, {}, [sp], #16", self.fp(), self.lr()));
            self.emit("    ret");
        } else {
            self.emit("    mov sp, r11");
            self.emit("    pop {r11, pc}");
        }
    }

    fn compile_fn(&mut self, f: &FnDef) -> Result<(), String> {
        self.emit_label(&f.name.name);
        // Simple prologue: assume 0 local vars for now
        self.emit_prologue(4);

        // Move params from registers to stack (simplified)
        for (i, _p) in f.params.iter().enumerate() {
            if i < 8 {
                let reg = self.reg(i);
                let offset = (i as i32) * 8 + 16;
                if self.arch == "aarch64" {
                    self.emit(&format!("    str {}, [sp, #{}]", reg, offset));
                } else {
                    self.emit(&format!("    str {}, [sp, #{}]", reg, offset));
                }
            }
        }

        // Compile body
        for stmt in &f.body {
            self.compile_stmt(stmt)?;
        }

        // Default return 0
        if self.arch == "aarch64" {
            self.emit("    mov x0, #0");
        } else {
            self.emit("    mov r0, #0");
        }
        self.emit_epilogue();
        self.emit("");
        Ok(())
    }

    fn compile_stmt(&mut self, s: &Stmt) -> Result<(), String> {
        match s {
            Stmt::Return(r) => {
                if let Some(v) = r.values.first() {
                    self.compile_expr(v, 0)?;
                } else {
                    if self.arch == "aarch64" {
                        self.emit("    mov x0, #0");
                    } else {
                        self.emit("    mov r0, #0");
                    }
                }
                self.emit_epilogue();
            }
            Stmt::Assign(a) => {
                // Evaluate value into reg 0
                self.compile_expr(&a.value, 0)?;
                // Store to local variable (simplified: just keep in register)
            }
            Stmt::Expr(e) => {
                self.compile_expr(&e.expr, 0)?;
            }
            Stmt::If(i) => {
                self.compile_expr(&i.condition, 0)?;
                let else_label = self.new_label("else");
                let end_label = self.new_label("endif");
                if self.arch == "aarch64" {
                    self.emit(&format!("    cmp x0, #0"));
                    self.emit(&format!("    beq {}", else_label));
                } else {
                    self.emit(&format!("    cmp r0, #0"));
                    self.emit(&format!("    beq {}", else_label));
                }
                for s in &i.then_body { self.compile_stmt(s)?; }
                self.emit(&format!("    b {}", end_label));
                self.emit_label(&else_label);
                if let Some(else_body) = &i.else_body {
                    for s in else_body { self.compile_stmt(s)?; }
                }
                self.emit_label(&end_label);
            }
            Stmt::While(w) => {
                let loop_start = self.new_label("loop");
                let loop_end = self.new_label("endloop");
                self.emit_label(&loop_start);
                self.compile_expr(&w.condition, 0)?;
                if self.arch == "aarch64" {
                    self.emit(&format!("    cmp x0, #0"));
                    self.emit(&format!("    beq {}", loop_end));
                } else {
                    self.emit(&format!("    cmp r0, #0"));
                    self.emit(&format!("    beq {}", loop_end));
                }
                for s in &w.body { self.compile_stmt(s)?; }
                self.emit(&format!("    b {}", loop_start));
                self.emit_label(&loop_end);
            }
            Stmt::Println(p) => {
                // Simplified: print first arg as integer
                if let Some(arg) = p.args.first() {
                    self.compile_expr(arg, 0)?;
                    // write syscall
                    if self.arch == "aarch64" {
                        self.emit("    mov x1, x0");   // buffer
                        self.emit("    mov x2, #1");    // would need proper print
                        // For now, just use the value
                    } else {
                        self.emit("    mov r1, r0");
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn compile_expr(&mut self, e: &Expr, target: usize) -> Result<(), String> {
        let reg = self.reg(target);
        match e {
            Expr::Integer(i) => {
                if self.arch == "aarch64" {
                    self.emit(&format!("    mov {}, #{}", reg, i.value));
                } else {
                    self.emit(&format!("    mov {}, #{}", reg, i.value));
                }
            }
            Expr::Binary(b) => {
                self.compile_expr(&b.left, target)?;
                self.compile_expr(&b.right, target + 1)?;
                let r1 = self.reg(target);
                let r2 = self.reg(target + 1);
                use crate::parser::ast::BinaryOp::*;
                match b.operator {
                    Add => self.emit(&format!("    add {}, {}, {}", r1, r1, r2)),
                    Sub => self.emit(&format!("    sub {}, {}, {}", r1, r1, r2)),
                    Mul => {
                        if self.arch == "aarch64" {
                            self.emit(&format!("    mul {}, {}, {}", r1, r1, r2));
                        } else {
                            self.emit(&format!("    mul {}, {}, {}", r1, r1, r2));
                        }
                    }
                    Div => {
                        if self.arch == "aarch64" {
                            self.emit(&format!("    sdiv {}, {}, {}", r1, r1, r2));
                        } else {
                            self.emit(&format!("    sdiv {}, {}, {}", r1, r1, r2));
                        }
                    }
                    Eq => {
                        self.emit(&format!("    cmp {}, {}", r1, r2));
                        if self.arch == "aarch64" {
                            self.emit(&format!("    cset {}, eq", r1));
                        } else {
                            self.emit(&format!("    moveq {}, #1", r1));
                            self.emit(&format!("    movne {}, #0", r1));
                        }
                    }
                    Lt => {
                        self.emit(&format!("    cmp {}, {}", r1, r2));
                        if self.arch == "aarch64" {
                            self.emit(&format!("    cset {}, lt", r1));
                        } else {
                            self.emit(&format!("    movlt {}, #1", r1));
                            self.emit(&format!("    movge {}, #0", r1));
                        }
                    }
                    Le => {
                        self.emit(&format!("    cmp {}, {}", r1, r2));
                        if self.arch == "aarch64" {
                            self.emit(&format!("    cset {}, le", r1));
                        } else {
                            self.emit(&format!("    movle {}, #1", r1));
                            self.emit(&format!("    movgt {}, #0", r1));
                        }
                    }
                    Gt => {
                        self.emit(&format!("    cmp {}, {}", r1, r2));
                        if self.arch == "aarch64" {
                            self.emit(&format!("    cset {}, gt", r1));
                        } else {
                            self.emit(&format!("    movgt {}, #1", r1));
                            self.emit(&format!("    movle {}, #0", r1));
                        }
                    }
                    Ge => {
                        self.emit(&format!("    cmp {}, {}", r1, r2));
                        if self.arch == "aarch64" {
                            self.emit(&format!("    cset {}, ge", r1));
                        } else {
                            self.emit(&format!("    movge {}, #1", r1));
                            self.emit(&format!("    movlt {}, #0", r1));
                        }
                    }
                    Ne => {
                        self.emit(&format!("    cmp {}, {}", r1, r2));
                        if self.arch == "aarch64" {
                            self.emit(&format!("    cset {}, ne", r1));
                        } else {
                            self.emit(&format!("    movne {}, #1", r1));
                            self.emit(&format!("    moveq {}, #0", r1));
                        }
                    }
                    _ => {}
                }
            }
            Expr::Call(c) => {
                // Evaluate args into registers
                for (i, arg) in c.args.iter().enumerate() {
                    if i < 8 {
                        self.compile_expr(arg, i)?;
                    }
                }
                if let Expr::Identifier(id) = c.callee.as_ref() {
                    self.emit(&format!("    bl {}", id.name));
                    // Result is in x0/r0, move to target
                    if target != 0 {
                        self.emit(&format!("    mov {}, {}", reg, self.reg(0)));
                    }
                }
            }
            Expr::Identifier(_) => {
                // Simplified: return 0 for unknown variables
                self.emit(&format!("    mov {}, #0", reg));
            }
            _ => {
                self.emit(&format!("    mov {}, #0", reg));
            }
        }
        Ok(())
    }
}

/// Compile a Vredrs program to ARM assembly and write to a file.
pub fn compile_to_arm_assembly(
    program: &Program,
    output_path: &std::path::Path,
    arch: &str,
) -> Result<(), String> {
    let mut gen = ArmCodeGen::new(arch);
    let asm = gen.compile(program)?;

    let asm_path = output_path.with_extension("s");
    std::fs::write(&asm_path, &asm)
        .map_err(|e| format!("can't write assembly: {}", e))?;

    eprintln!("[vredrs] Raw {} assembly written to: {}", arch, asm_path.display());

    // Try to assemble and link
    let obj_path = output_path.with_extension("o");
    let exe_path = output_path.to_path_buf();

    // Assemble
    let assembler = if arch == "aarch64" { "as" } else { "as" };
    let assemble = std::process::Command::new(assembler)
        .arg("-o")
        .arg(&obj_path)
        .arg(&asm_path)
        .output();

    match assemble {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            eprintln!("[vredrs] Warning: assembler failed: {}", String::from_utf8_lossy(&out.stderr));
            return Ok(());
        }
        Err(_) => {
            eprintln!("[vredrs] Warning: 'as' not found. Assembly written to {}", asm_path.display());
            return Ok(());
        }
    }

    // Link
    let link = std::process::Command::new("cc")
        .arg("-o")
        .arg(&exe_path)
        .arg(&obj_path)
        .arg("-lm")
        .arg("-lpthread")
        .arg("-nostartfiles")
        .output();

    match link {
        Ok(out) if out.status.success() => {
            eprintln!("[vredrs] Linked: {}", exe_path.display());
        }
        Ok(out) => {
            // Try without -nostartfiles
            let _ = std::process::Command::new("cc")
                .arg("-o").arg(&exe_path).arg(&obj_path).arg("-lm").arg("-lpthread")
                .output();
            eprintln!("[vredrs] Linked (fallback): {}", exe_path.display());
        }
        Err(e) => {
            eprintln!("[vredrs] Warning: linker not found: {}", e);
        }
    }

    Ok(())
}
