//! C* raw backend: Vredrs AST -> C* PIR -> linear check -> no-runtime LLVM IR.
//!
//! Phase 3 turns the raw backend into a physical compilation pipeline.  It
//! lowers the C* features described in `新增.txt`: `@pipeline`, `@patch`,
//! `@isr_group` with WCET checking, `@prefetch`, package/repo physical layout,
//! byte-level differential update metadata, section placement, extern cstar
//! declarations and multiline inline assembly.  The backend remains independent
//! from interpreter `Value`, GC, coroutine scheduling and reference counting.

use super::linear::LinearChecker;
use super::pir::{
    IsrEntry, PackageEntry, PatchPlan, PirExtern, PirFunction, PirNode, PirOperand, PirProgram,
    PirType, RepoConfig,
};
use crate::error::{CompilerError, Result};
use crate::parser::ast::*;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone)]
pub struct CstarPatchArtifact {
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct CstarArtifacts {
    pub llvm_ir: String,
    pub linker_script: Option<String>,
    pub package_header: Option<String>,
    pub layout_map: Option<String>,
    pub package_index: Option<Vec<u8>>,
    pub patches: Vec<CstarPatchArtifact>,
}

pub struct CstarCompiler {
    last_needs_linker_script: bool,
}

impl CstarCompiler {
    pub fn new() -> Self {
        CstarCompiler {
            last_needs_linker_script: false,
        }
    }

    pub fn generate_from_ast(&mut self, program: &Program) -> Result<String> {
        Ok(self.generate_artifacts_from_ast(program)?.llvm_ir)
    }

    pub fn generate_artifacts_from_ast(&mut self, program: &Program) -> Result<CstarArtifacts> {
        self.generate_artifacts_from_ast_with_source(program, "")
    }

    pub fn generate_artifacts_from_ast_with_source(
        &mut self,
        program: &Program,
        source: &str,
    ) -> Result<CstarArtifacts> {
        let mut pir = self.lower_program(program)?;
        self.merge_source_physical_config(&mut pir, source);
        LinearChecker::check(&pir)?;
        self.last_needs_linker_script = pir.needs_linker_script;
        let llvm_ir = CstarLlvmEmitter::new().emit(&pir)?;
        let linker_script = if pir.needs_linker_script {
            Some(default_linker_script())
        } else {
            None
        };
        let package_header = if !pir.packages.is_empty() {
            Some(emit_package_header(&pir.packages))
        } else {
            None
        };
        let layout_map = if !pir.packages.is_empty() {
            Some(emit_layout_map(&pir.packages))
        } else {
            None
        };
        let package_index = if !pir.packages.is_empty() {
            Some(emit_package_index(&pir.packages))
        } else {
            None
        };
        let patches = pir
            .patch_plans
            .iter()
            .map(|p| CstarPatchArtifact {
                path: p.output.clone(),
                bytes: emit_patch_bytes(p, &llvm_ir),
            })
            .collect();
        Ok(CstarArtifacts {
            llvm_ir,
            linker_script,
            package_header,
            layout_map,
            package_index,
            patches,
        })
    }

    pub fn last_needs_linker_script(&self) -> bool {
        self.last_needs_linker_script
    }

    fn lower_program(&mut self, program: &Program) -> Result<PirProgram> {
        let mut lower = PirLowerer::new();
        for item in &program.declarations {
            match item {
                // C*/raw: explicit unsafe blocks lower to the top-level raw entry path.
                TopLevel::Statement(Stmt::UnsafeBlock(block)) => lower.lower_unsafe_block(block)?,
                // C*/raw: imports are accepted here; package layout is generated from source/import metadata.
                TopLevel::Import(_) => {},
                // C*/raw: annotated functions lower to raw LLVM functions.
                TopLevel::FnDef(fndef) => lower.lower_function(fndef)?,
                TopLevel::LazyFnDef(lazy) => lower.lower_function(&lazy.fn_def)?,
                // C*/raw: extern cstar declarations are emitted as declarations only.
                TopLevel::ExternFnDef(ext) => lower.lower_extern(ext)?,
                TopLevel::StructDef(sd) => lower.lower_struct(sd)?,
                TopLevel::ConditionalCompile(cc) => {
                    for nested in &cc.then_body {
                        match nested {
                            TopLevel::Statement(Stmt::UnsafeBlock(block)) => lower.lower_unsafe_block(block)?,
                            TopLevel::Import(_) => {},
                            TopLevel::FnDef(fndef) => lower.lower_function(fndef)?,
                            TopLevel::ExternFnDef(ext) => lower.lower_extern(ext)?,
                            TopLevel::StructDef(sd) => lower.lower_struct(sd)?,
                            _ => return Err(CompilerError::codegen_error("C* raw conditional compile accepts imports, unsafe blocks, annotated functions, aligned structs, and extern cstar declarations only")),
                        }
                    }
                }
                other => return Err(CompilerError::codegen_error(format!(
                    "C* raw backend rejects top-level {}; use imports, unsafe blocks, @section/@isr_group/@pipeline/@prefetch/@patch functions, aligned structs, or extern cstar declarations",
                    top_level_kind(other)
                ))),
            }
        }
        Ok(lower.finish())
    }

    fn merge_source_physical_config(&mut self, pir: &mut PirProgram, source: &str) {
        let repo = parse_repo_config(source);
        let imports = parse_source_imports(source);
        if repo.enabled || !imports.is_empty() {
            let mut repo = repo;
            if repo.split_by.is_empty() {
                repo.split_by = "size".to_string();
            }
            pir.enable_repo(repo.clone());
            let packages = build_package_layout(&imports, &repo);
            for pkg in packages {
                pir.push(PirNode::PackageMarker {
                    name: pkg.name.clone(),
                    partition: pkg.partition.clone(),
                    size: pkg.size,
                    offset: pkg.offset,
                    checksum: pkg.checksum,
                });
                pir.push_package(pkg);
            }
        }
    }
}

struct PirLowerer {
    program: PirProgram,
    types: HashMap<String, PirType>,
}

impl PirLowerer {
    fn new() -> Self {
        Self {
            program: PirProgram::default(),
            types: HashMap::new(),
        }
    }
    fn finish(self) -> PirProgram {
        self.program
    }

    fn lower_unsafe_block(&mut self, block: &UnsafeBlock) -> Result<()> {
        for stmt in &block.body {
            self.lower_stmt_into(stmt, None)?;
        }
        Ok(())
    }

    fn lower_struct(&mut self, sd: &StructDef) -> Result<()> {
        if let Some(align) = annotation_int(&sd.annotations, "align")? {
            // C*/raw: `@align(N) struct` is represented as a package-layout marker.
            self.program.mark_linker_script();
            self.program.push(PirNode::PackageMarker {
                name: format!("struct_{}", sd.name.name),
                partition: format!("align{}", align),
                size: (sd.fields.len() as u64).max(1) * 8,
                offset: align as u64,
                checksum: stable_hash(&sd.name.name),
            });
            Ok(())
        } else {
            Err(CompilerError::codegen_error(format!("C* raw backend only accepts structs marked @align(N); '{}' has no physical alignment", sd.name.name)))
        }
    }

    fn lower_function(&mut self, fndef: &FnDef) -> Result<()> {
        let section = annotation_string(&fndef.annotations, "section")?;
        let pipeline = annotation_map(&fndef.annotations, "pipeline");
        let prefetch = annotation_map(&fndef.annotations, "prefetch");
        let patch = annotation_patch(&fndef.annotations)?;
        let isr = annotation_isr(&fndef.annotations)?;

        if section.is_none()
            && pipeline.is_none()
            && prefetch.is_none()
            && patch.is_none()
            && isr.is_none()
        {
            return Err(CompilerError::codegen_error(format!(
                "C* raw backend only lowers functions marked @section, @isr_group, @pipeline, @prefetch, or @patch; '{}' has no C* physical annotation",
                fndef.name.name
            )));
        }

        let mut body = Vec::new();
        if let Some(args) = &pipeline {
            // C*/raw: `@pipeline fn ...` -> PIR Pipeline marker.  The LLVM emitter
            // emits DMA/compute scheduling markers; later schedulers may optimize it.
            body.push(PirNode::Pipeline {
                function: fndef.name.name.clone(),
                priority: args
                    .get("priority")
                    .cloned()
                    .unwrap_or_else(|| "normal".to_string()),
                dma_stage: format!("{}_dma_window", fndef.name.name),
                compute_stage: format!("{}_compute_window", fndef.name.name),
            });
        }
        if let Some(args) = &prefetch {
            // C*/raw: `@prefetch(...) fn ...` -> `llvm.prefetch` before the loop/body.
            let first_ptr_param = fndef
                .params
                .iter()
                .find(|p| {
                    p.type_annotation
                        .as_ref()
                        .map(|t| type_expr_is_ptr(t))
                        .unwrap_or(false)
                })
                .map(|p| PirOperand::Var(p.name.name.clone()));
            body.push(PirNode::Prefetch {
                ptr: first_ptr_param,
                hint: args
                    .get("hint")
                    .cloned()
                    .unwrap_or_else(|| "sequential".to_string()),
                stride: args
                    .get("stride")
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(64),
            });
        }
        if let Some((base, output)) = &patch {
            // C*/raw: `@patch(base=..., output=...)` is kept both in PIR and side artifacts.
            body.push(PirNode::PatchMarker {
                function: fndef.name.name.clone(),
                base: base.clone(),
                output: output.clone(),
            });
            self.program.push_patch(PatchPlan {
                function: fndef.name.name.clone(),
                base: base.clone(),
                output: output.clone(),
            });
        }

        for stmt in &fndef.body {
            self.lower_stmt_into(stmt, Some(&mut body))?;
        }

        let params = fndef
            .params
            .iter()
            .map(|p| {
                let ty = if let Some(t) = &p.type_annotation {
                    self.pir_type_from_type_expr(t)?
                } else {
                    PirType::I64
                };
                Ok((p.name.name.clone(), ty))
            })
            .collect::<Result<Vec<_>>>()?;
        let ret_ty = if let Some(t) = &fndef.return_type {
            self.pir_type_from_type_expr(t)?
        } else {
            PirType::Void
        };

        if let Some(sec) = &section {
            if sec.starts_with('.') {
                self.program.mark_linker_script();
            }
        }
        if let Some((priority, budget_us)) = isr {
            let wcet_cycles = estimate_wcet_cycles(&body);
            let budget_cycles = (budget_us.max(0) as u64) * 100; // conservative 100MHz target default.
            if budget_cycles > 0 && wcet_cycles > budget_cycles {
                return Err(CompilerError::codegen_error(format!(
                    "C* @isr_group WCET violation: handler '{}' estimated {} cycles exceeds budget {}us ({} cycles)",
                    fndef.name.name, wcet_cycles, budget_us, budget_cycles
                )));
            }
            // C*/raw: `@isr_group` records a vector table entry plus WCET result.
            self.program.push_isr(IsrEntry {
                handler: fndef.name.name.clone(),
                priority,
                budget_us,
                wcet_cycles,
            });
        }
        self.program.push_function(PirFunction {
            name: fndef.name.name.clone(),
            params,
            ret_ty,
            body,
            section,
            is_pipeline: pipeline.is_some(),
            is_prefetch: prefetch.is_some(),
        });
        Ok(())
    }

    fn lower_extern(&mut self, ext: &ExternFnDef) -> Result<()> {
        let link = ext
            .link
            .clone()
            .or_else(|| ext.fn_def.extern_link.clone())
            .unwrap_or_else(|| "cstar".to_string());
        if link != "cstar" {
            return Err(CompilerError::codegen_error(format!(
                "C* raw backend only accepts extern cstar declarations, got extern {}",
                link
            )));
        }
        let params = ext
            .fn_def
            .params
            .iter()
            .map(|p| {
                if let Some(t) = &p.type_annotation {
                    self.pir_type_from_type_expr(t)
                } else {
                    Ok(PirType::I64)
                }
            })
            .collect::<Result<Vec<_>>>()?;
        let ret_ty = if let Some(t) = &ext.fn_def.return_type {
            self.pir_type_from_type_expr(t)?
        } else {
            PirType::Void
        };
        self.program.push_extern(PirExtern {
            name: ext.fn_def.name.name.clone(),
            params,
            ret_ty,
            link,
        });
        Ok(())
    }

    fn lower_stmt_into(
        &mut self,
        stmt: &Stmt,
        mut target: Option<&mut Vec<PirNode>>,
    ) -> Result<()> {
        match stmt {
            Stmt::Assign(assign) => self.lower_assign(assign, target.as_deref_mut()),
            Stmt::Expr(expr_stmt) => { self.lower_expr_effect(&expr_stmt.expr, target.as_deref_mut())?; Ok(()) }
            Stmt::Asm(asm) => {
                // C*/raw: `asm { "..." }` -> PIR AsmBlock -> LLVM inline asm sideeffect.
                self.push_node(target, PirNode::AsmBlock {
                    template: asm.template.clone(),
                    inputs: asm.inputs.iter().map(|op| self.lower_operand_expr(&op.expr)).collect::<Result<Vec<_>>>()?,
                    outputs: asm.outputs.iter().map(|op| op.constraint.clone()).collect(),
                });
                Ok(())
            }
            Stmt::UnsafeBlock(inner) => { for s in &inner.body { self.lower_stmt_into(s, target.as_deref_mut())?; } Ok(()) }
            Stmt::ForRange(fr) => self.lower_loop(&format!("for_range_{}", fr.var.name), &fr.body, target.as_deref_mut()),
            Stmt::ForIn(fi) => self.lower_loop(&format!("for_in_{}", fi.var.name), &fi.body, target.as_deref_mut()),
            Stmt::Loop(l) => self.lower_loop("loop", &l.body, target.as_deref_mut()),
            Stmt::Return(_) => Err(CompilerError::codegen_error("C* raw functions use implicit returns in phase 3; explicit return needs ABI-specific lowering and is rejected explicitly")),
            other => Err(CompilerError::codegen_error(format!(
                "C* raw backend phase 3 does not lower statement {}; use raw calls, assignment, unsafe, asm blocks, for loops in physical functions, or annotated empty functions",
                stmt_kind(other)
            ))),
        }
    }

    fn lower_loop(
        &mut self,
        label: &str,
        body_src: &[Stmt],
        target: Option<&mut Vec<PirNode>>,
    ) -> Result<()> {
        let mut body = Vec::new();
        for s in body_src {
            self.lower_stmt_into(s, Some(&mut body))?;
        }
        self.push_node(
            target,
            PirNode::Loop {
                label: label.to_string(),
                body,
            },
        );
        Ok(())
    }

    fn push_node(&mut self, target: Option<&mut Vec<PirNode>>, node: PirNode) {
        if let Some(body) = target {
            body.push(node);
        } else {
            self.program.push(node);
        }
    }

    fn lower_assign(
        &mut self,
        assign: &AssignStmt,
        target: Option<&mut Vec<PirNode>>,
    ) -> Result<()> {
        if assign.operator != AssignOp::Simple {
            return Err(CompilerError::codegen_error(
                "C* raw backend supports only simple assignment in physical code",
            ));
        }
        if assign.targets.len() != 1 {
            return Err(CompilerError::codegen_error(
                "C* raw backend supports exactly one assignment target",
            ));
        }
        match &assign.targets[0] {
            Assignee::Identifier(id) => {
                self.lower_identifier_assign(&id.name, &assign.value, target)
            }
            Assignee::Index(index) => {
                let ptr = self.lower_operand_expr(&index.target)?;
                let value = self.lower_operand_expr(&assign.value)?;
                self.push_node(
                    target,
                    PirNode::Store {
                        ptr,
                        value,
                        ty: PirType::I64,
                        volatile: false,
                    },
                );
                Ok(())
            }
            _ => Err(CompilerError::codegen_error(
                "C* raw assignment target must be an identifier or ptr[index] store",
            )),
        }
    }

    fn lower_identifier_assign(
        &mut self,
        dst: &str,
        value_expr: &Expr,
        target: Option<&mut Vec<PirNode>>,
    ) -> Result<()> {
        match value_expr {
            Expr::Cast(cast) => {
                let ty = self.pir_type_from_type_expr(&cast.type_expr)?;
                if let Expr::Call(call) = cast.expr.as_ref() {
                    if self.call_name(call.callee.as_ref())? == "malloc" {
                        let bytes = call.args.get(0).ok_or_else(|| {
                            CompilerError::codegen_error("C* malloc requires byte-size argument")
                        })?;
                        self.push_node(
                            target,
                            PirNode::AllocLinear {
                                dst: dst.to_string(),
                                bytes: self.lower_operand_expr(bytes)?,
                                ty: ty.clone(),
                            },
                        );
                        self.types.insert(dst.to_string(), ty);
                        return Ok(());
                    }
                    if self.call_name(call.callee.as_ref())? == "move" {
                        let src = self.single_var_arg(call, "move")?;
                        self.push_node(
                            target,
                            PirNode::Move {
                                dst: dst.to_string(),
                                src: src.clone(),
                                ty: ty.clone(),
                            },
                        );
                        self.types.insert(dst.to_string(), ty);
                        return Ok(());
                    }
                }
                Err(CompilerError::codegen_error("C* cast assignment currently supports only malloc(n) as ptr[T] or move(p) as ptr[T]"))
            }
            Expr::MethodCall(mc) if mc.method.name.starts_with("load") => {
                let ptr = self.lower_operand_expr(&mc.receiver)?;
                self.push_node(
                    target,
                    PirNode::Load {
                        dst: dst.to_string(),
                        ptr,
                        ty: PirType::I64,
                        volatile: mc.method.name.contains("acquire"),
                    },
                );
                self.types.insert(dst.to_string(), PirType::I64);
                Ok(())
            }
            Expr::Call(call) => {
                let func = self.call_name(call.callee.as_ref())?;
                let ret_ty = if func == "malloc" {
                    PirType::ptr_erased()
                } else {
                    PirType::I64
                };
                let args = call
                    .args
                    .iter()
                    .map(|a| self.lower_operand_expr(a))
                    .collect::<Result<Vec<_>>>()?;
                self.push_node(
                    target,
                    PirNode::Call {
                        dst: Some(dst.to_string()),
                        func,
                        args,
                        ret_ty: ret_ty.clone(),
                    },
                );
                self.types.insert(dst.to_string(), ret_ty);
                Ok(())
            }
            other => {
                let value = self.lower_operand_expr(other)?;
                let ptr = PirOperand::Var(dst.to_string());
                self.push_node(
                    target,
                    PirNode::Store {
                        ptr,
                        value,
                        ty: PirType::I64,
                        volatile: false,
                    },
                );
                self.types.insert(dst.to_string(), PirType::I64);
                Ok(())
            }
        }
    }

    fn lower_expr_effect(&mut self, expr: &Expr, target: Option<&mut Vec<PirNode>>) -> Result<()> {
        match expr {
            Expr::Cast(cast) => {
                if let Expr::Call(call) = cast.expr.as_ref() {
                    self.lower_call_effect(call, target)
                } else {
                    Err(CompilerError::codegen_error(
                        "C* effect cast must wrap a raw call such as free(p) as ptr[u8]",
                    ))
                }
            }
            Expr::Call(call) => self.lower_call_effect(call, target),
            Expr::MethodCall(mc) => self.lower_method_effect(mc, target),
            other => Err(CompilerError::codegen_error(format!(
                "C* raw effect expression is unsupported in phase 3: {}",
                expr_kind(other)
            ))),
        }
    }

    fn lower_method_effect(
        &mut self,
        mc: &MethodCallExpr,
        target: Option<&mut Vec<PirNode>>,
    ) -> Result<()> {
        let name = mc.method.name.as_str();
        if name.starts_with("store") {
            let ptr = self.lower_operand_expr(&mc.receiver)?;
            let val = mc.args.get(0).ok_or_else(|| {
                CompilerError::codegen_error("C* ptr.store requires one value argument")
            })?;
            self.push_node(
                target,
                PirNode::Store {
                    ptr,
                    value: self.lower_operand_expr(val)?,
                    ty: PirType::I64,
                    volatile: name.contains("release"),
                },
            );
            Ok(())
        } else {
            Err(CompilerError::codegen_error(format!(
                "C* raw method '{}' is not a supported ptr atomic operation",
                name
            )))
        }
    }

    fn lower_call_effect(
        &mut self,
        call: &CallExpr,
        target: Option<&mut Vec<PirNode>>,
    ) -> Result<()> {
        let func = self.call_name(call.callee.as_ref())?;
        let args = call
            .args
            .iter()
            .map(|a| self.lower_operand_expr(a))
            .collect::<Result<Vec<_>>>()?;
        self.push_node(
            target,
            PirNode::Call {
                dst: None,
                func,
                args,
                ret_ty: PirType::Void,
            },
        );
        Ok(())
    }

    fn lower_operand_expr(&self, expr: &Expr) -> Result<PirOperand> {
        match expr {
            Expr::Integer(i) => Ok(PirOperand::Int(i.value)),
            Expr::Bool(b) => Ok(PirOperand::Bool(b.value)),
            Expr::Null(_) => Ok(PirOperand::NullPtr),
            Expr::Identifier(id) => Ok(PirOperand::Var(id.name.clone())),
            Expr::Cast(cast) => self.lower_operand_expr(&cast.expr),
            _ => Err(CompilerError::codegen_error(format!(
                "C* raw backend supports only integer/bool/null/identifier operands, got {}",
                expr_kind(expr)
            ))),
        }
    }

    fn pir_type_from_type_expr(&self, ty: &TypeExpr) -> Result<PirType> {
        match ty {
            TypeExpr::Basic(BasicType::Bool, _) => Ok(PirType::I1),
            TypeExpr::Basic(BasicType::Int, _) => Ok(PirType::I64),
            TypeExpr::Basic(BasicType::Float, _) => Ok(PirType::F64),
            TypeExpr::Basic(BasicType::Void, _) | TypeExpr::Basic(BasicType::Null, _) => {
                Ok(PirType::Void)
            }
            TypeExpr::Named(id, _) => match id.name.as_str() {
                "u8" | "i8" => Ok(PirType::I8),
                "i32" | "u32" => Ok(PirType::I32),
                "i64" | "u64" | "int" => Ok(PirType::I64),
                "void" => Ok(PirType::Void),
                other => Err(CompilerError::codegen_error(format!(
                    "C* unknown physical type '{}'",
                    other
                ))),
            },
            TypeExpr::Generic { base, args, .. } => {
                if let TypeExpr::Named(id, _) = base.as_ref() {
                    if id.name == "ptr" {
                        let inner = args.get(0).ok_or_else(|| {
                            CompilerError::codegen_error("C* ptr[T] requires one type argument")
                        })?;
                        return Ok(PirType::Ptr(Box::new(self.pir_type_from_type_expr(inner)?)));
                    }
                }
                Err(CompilerError::codegen_error(
                    "C* raw backend supports only ptr[T] generic physical type",
                ))
            }
            _ => Err(CompilerError::codegen_error(
                "C* raw backend unsupported type expression",
            )),
        }
    }

    fn call_name(&self, callee: &Expr) -> Result<String> {
        match callee {
            Expr::Identifier(id) => Ok(id.name.clone()),
            _ => Err(CompilerError::codegen_error(
                "C* raw calls must use direct identifiers",
            )),
        }
    }

    fn single_var_arg(&self, call: &CallExpr, name: &str) -> Result<String> {
        match call.args.get(0) {
            Some(Expr::Identifier(id)) => Ok(id.name.clone()),
            _ => Err(CompilerError::codegen_error(format!(
                "C* {} requires a variable pointer argument",
                name
            ))),
        }
    }
}

struct CstarLlvmEmitter {
    out: String,
    names: HashMap<String, String>,
    tmp: usize,
}

impl CstarLlvmEmitter {
    fn new() -> Self {
        Self {
            out: String::new(),
            names: HashMap::new(),
            tmp: 0,
        }
    }

    fn emit(mut self, program: &PirProgram) -> Result<String> {
        self.out
            .push_str("; Vredrs C* raw backend LLVM IR - phase 3\n");
        self.out.push_str(
            "; No interpreter Value, GC, coroutine scheduler or refcount runtime is linked.\n",
        );
        self.out
            .push_str("target triple = \"x86_64-unknown-linux-gnu\"\n\n");
        self.out.push_str("declare i8* @malloc(i64)\n");
        self.out.push_str("declare void @free(i8*)\n");
        self.out
            .push_str("declare void @llvm.prefetch.p0i8(i8*, i32, i32, i32)\n");
        for ext in &program.externs {
            self.emit_extern(ext);
        }
        if !program.externs.is_empty() {
            self.out.push('\n');
        }
        for isr in &program.isr_entries {
            self.emit_isr_entry(isr);
        }
        for pkg in &program.packages {
            self.emit_package_entry(pkg);
        }
        for patch in &program.patch_plans {
            self.emit_patch_entry(patch);
        }
        if !program.isr_entries.is_empty()
            || !program.packages.is_empty()
            || !program.patch_plans.is_empty()
        {
            self.out.push('\n');
        }
        for f in &program.functions {
            self.emit_function(f)?;
        }
        if !program.nodes.is_empty() {
            self.out.push_str("define i32 @main() {\nentry:\n");
            for node in &program.nodes {
                self.emit_node(node)?;
            }
            self.out.push_str("  ret i32 0\n}\n");
        }
        Ok(self.out)
    }

    fn emit_extern(&mut self, ext: &PirExtern) {
        // C*/raw: `extern cstar fn, name(...)` -> LLVM declaration only.
        let args = ext
            .params
            .iter()
            .map(|t| t.llvm())
            .collect::<Vec<_>>()
            .join(", ");
        self.out.push_str(&format!(
            "declare {} @{}({})\n",
            ext.ret_ty.llvm(),
            sanitize_symbol(&ext.name),
            args
        ));
    }

    fn emit_isr_entry(&mut self, isr: &IsrEntry) {
        // C*/raw: `@isr_group(...)` -> physical ISR vector record in `.isr`.
        self.out.push_str(&format!(
            "@__cstar_isr_{} = global {{ i8*, i64, i64, i64 }} {{ i8* bitcast (void ()* @{} to i8*), i64 {}, i64 {}, i64 {} }}, section \".isr\", align 8\n",
            sanitize_symbol(&isr.handler), sanitize_symbol(&isr.handler), isr.priority, isr.budget_us, isr.wcet_cycles
        ));
    }

    fn emit_package_entry(&mut self, pkg: &PackageEntry) {
        // C*/raw: package manager physical placement record emitted to `.pkgmeta`.
        self.out.push_str(&format!(
            "@__cstar_pkg_{} = constant {{ i64, i64, i64 }} {{ i64 {}, i64 {}, i64 {} }}, section \".pkgmeta\", align 8 ; {}:{}\n",
            sanitize_symbol(&pkg.name), pkg.offset, pkg.size, pkg.checksum, pkg.name, pkg.partition
        ));
    }

    fn emit_patch_entry(&mut self, patch: &PatchPlan) {
        // C*/raw: `@patch` metadata emitted to `.patchmeta`; side artifact writes bytes.
        self.out.push_str(&format!(
            "@__cstar_patch_{} = constant {{ i64, i64 }} {{ i64 {}, i64 {} }}, section \".patchmeta\", align 8 ; base={} output={}\n",
            sanitize_symbol(&patch.function), stable_hash(&patch.base), stable_hash(&patch.output), patch.base, patch.output
        ));
    }

    fn emit_function(&mut self, f: &PirFunction) -> Result<()> {
        self.names.clear();
        // C*/raw: annotated FnDef -> raw LLVM function, optionally placed in `@section`.
        let args = f
            .params
            .iter()
            .map(|(name, ty)| format!("{} %{}", ty.llvm(), sanitize_symbol(name)))
            .collect::<Vec<_>>()
            .join(", ");
        let section = f
            .section
            .as_ref()
            .map(|s| format!(" section \"{}\"", escape_attr(s)))
            .unwrap_or_default();
        self.out.push_str(&format!(
            "define {} @{}({}){} {{\nentry:\n",
            f.ret_ty.llvm(),
            sanitize_symbol(&f.name),
            args,
            section
        ));
        for (name, _) in &f.params {
            self.names
                .insert(name.clone(), format!("%{}", sanitize_symbol(name)));
        }
        if f.is_pipeline {
            self.out.push_str(
                "  ; C* function has @pipeline: DMA and compute windows are emitted below.\n",
            );
        }
        if f.is_prefetch {
            self.out
                .push_str("  ; C* function has @prefetch: llvm.prefetch is emitted below.\n");
        }
        for node in &f.body {
            self.emit_node(node)?;
        }
        match f.ret_ty {
            PirType::Void => self.out.push_str("  ret void\n"),
            PirType::I1 => self.out.push_str("  ret i1 0\n"),
            PirType::I8 => self.out.push_str("  ret i8 0\n"),
            PirType::I32 => self.out.push_str("  ret i32 0\n"),
            PirType::I64 => self.out.push_str("  ret i64 0\n"),
            PirType::F64 => self.out.push_str("  ret double 0.0\n"),
            PirType::Ptr(_) => self
                .out
                .push_str(&format!("  ret {} null\n", f.ret_ty.llvm())),
        }
        self.out.push_str("}\n\n");
        Ok(())
    }

    fn emit_node(&mut self, node: &PirNode) -> Result<()> {
        match node {
            PirNode::AllocLinear { dst, bytes, ty } => {
                // LLVM sequence for C*: `set, p = malloc(n) as ptr[T]`.
                let b = self.operand_i64(bytes)?;
                let raw = self.next_tmp();
                self.out
                    .push_str(&format!("  {} = call i8* @malloc(i64 {})\n", raw, b));
                let repr = if ty.llvm() == "i8*" {
                    raw
                } else {
                    let cast = self.next_tmp();
                    self.out.push_str(&format!(
                        "  {} = bitcast i8* {} to {}\n",
                        cast,
                        raw,
                        ty.llvm()
                    ));
                    cast
                };
                self.names.insert(dst.clone(), repr);
            }
            PirNode::Call {
                dst,
                func,
                args,
                ret_ty,
            } => {
                match func.as_str() {
                    "free" | "consume" => {
                        // LLVM sequence for C*: `free(p)`/`consume(p)` linear release.
                        let arg = args.get(0).ok_or_else(|| {
                            CompilerError::codegen_error("C* free requires one argument")
                        })?;
                        let ptr = self.operand_ptr_erased(arg)?;
                        self.out
                            .push_str(&format!("  call void @free(i8* {})\n", ptr));
                    }
                    other => {
                        let rendered = self.render_args(args)?;
                        if ret_ty == &PirType::Void {
                            self.out.push_str(&format!(
                                "  call void @{}({})\n",
                                sanitize_symbol(other),
                                rendered
                            ));
                        } else {
                            let res = self.next_tmp();
                            self.out.push_str(&format!(
                                "  {} = call {} @{}({})\n",
                                res,
                                ret_ty.llvm(),
                                sanitize_symbol(other),
                                rendered
                            ));
                            if let Some(dst) = dst {
                                self.names.insert(dst.clone(), res);
                            }
                        }
                    }
                }
            }
            PirNode::Move { dst, src, .. } => {
                // LLVM sequence for C*: `move(p)` is a register ownership transfer.
                let val = self.names.get(src).cloned().ok_or_else(|| {
                    CompilerError::codegen_error(format!(
                        "C* move source '{}' has no LLVM value",
                        src
                    ))
                })?;
                self.names.insert(dst.clone(), val);
            }
            PirNode::Consume { var } => {
                // LLVM sequence for C*: `consume(p)` without memory release.
                if !self.names.contains_key(var) {
                    return Err(CompilerError::codegen_error(format!(
                        "C* consume target '{}' has no LLVM value",
                        var
                    )));
                }
            }
            PirNode::AsmBlock {
                template,
                inputs,
                outputs,
            } => {
                // LLVM sequence for C*: multiline `asm { ... }` -> LLVM inline asm sideeffect.
                self.emit_inline_asm(template, inputs, outputs)?;
            }
            PirNode::Pipeline {
                function,
                priority,
                dma_stage,
                compute_stage,
            } => {
                // LLVM sequence for C*: `@pipeline` -> DMA + compute interleave markers.
                self.out.push_str(&format!(
                    "  ; C* @pipeline function={} priority={} dma={} compute={}\n",
                    function, priority, dma_stage, compute_stage
                ));
                self.out
                    .push_str("  call void asm sideeffect \"# cstar dma window\", \"\"()\n");
                self.out
                    .push_str("  call void asm sideeffect \"# cstar compute window\", \"\"()\n");
            }
            PirNode::Prefetch { ptr, hint, stride } => {
                // LLVM sequence for C*: `@prefetch` -> llvm.prefetch intrinsic.
                let p = if let Some(op) = ptr {
                    self.operand_ptr_erased(op)?
                } else {
                    "null".to_string()
                };
                let locality = if hint == "sequential" { 3 } else { 1 };
                self.out.push_str(&format!(
                    "  ; C* @prefetch hint={} stride={}\n",
                    hint, stride
                ));
                self.out.push_str(&format!(
                    "  call void @llvm.prefetch.p0i8(i8* {}, i32 0, i32 {}, i32 1)\n",
                    p, locality
                ));
            }
            PirNode::Loop { label, body } => {
                // LLVM sequence for C*: physical loop marker; body is emitted once as a schedulable region.
                self.out
                    .push_str(&format!("  ; C* loop region {} begins\n", label));
                for n in body {
                    self.emit_node(n)?;
                }
                self.out
                    .push_str(&format!("  ; C* loop region {} ends\n", label));
            }
            PirNode::Load {
                dst,
                ptr,
                ty,
                volatile,
            } => {
                // LLVM sequence for C*: pointer load/load_acquire.
                let p = self.operand_with_type(ptr, &PirType::Ptr(Box::new(ty.clone())))?;
                let t = self.next_tmp();
                let vol = if *volatile { " volatile" } else { "" };
                self.out.push_str(&format!(
                    "  {} = load{} {}, {} {}, align 1\n",
                    t,
                    vol,
                    ty.llvm(),
                    PirType::Ptr(Box::new(ty.clone())).llvm(),
                    p
                ));
                self.names.insert(dst.clone(), t);
            }
            PirNode::Store {
                ptr,
                value,
                ty,
                volatile,
            } => {
                // LLVM sequence for C*: pointer store/store_release.
                let p = self.operand_with_type(ptr, &PirType::Ptr(Box::new(ty.clone())))?;
                let v = self.operand_with_type(value, ty)?;
                let vol = if *volatile { " volatile" } else { "" };
                self.out.push_str(&format!(
                    "  store{} {} {}, {} {}, align 1\n",
                    vol,
                    ty.llvm(),
                    v,
                    PirType::Ptr(Box::new(ty.clone())).llvm(),
                    p
                ));
            }
            PirNode::PatchMarker {
                function,
                base,
                output,
            } => {
                // LLVM sequence for C*: `@patch` marker kept in IR for link-time tooling.
                self.out.push_str(&format!(
                    "  ; C* @patch function={} base={} output={}\n",
                    function, base, output
                ));
            }
            PirNode::PackageMarker {
                name,
                partition,
                size,
                offset,
                checksum,
            } => {
                // LLVM sequence for C*: package manager marker kept in IR near raw entry.
                self.out.push_str(&format!(
                    "  ; C* package {} partition={} size={} offset={} checksum={}\n",
                    name, partition, size, offset, checksum
                ));
            }
        }
        Ok(())
    }

    fn emit_inline_asm(
        &mut self,
        template: &str,
        inputs: &[PirOperand],
        outputs: &[String],
    ) -> Result<()> {
        let escaped = escape_asm(template);
        let mut constraints = Vec::new();
        constraints.extend(outputs.iter().cloned());
        for _ in inputs {
            constraints.push("r".to_string());
        }
        let constraint_text = constraints.join(",");
        let args = self.render_args(inputs)?;
        if outputs.is_empty() {
            if args.is_empty() {
                self.out.push_str(&format!(
                    "  call void asm sideeffect inteldialect \"{}\", \"{}\"()\n",
                    escaped, constraint_text
                ));
            } else {
                self.out.push_str(&format!(
                    "  call void asm sideeffect inteldialect \"{}\", \"{}\"({})\n",
                    escaped, constraint_text, args
                ));
            }
        } else {
            let res = self.next_tmp();
            if args.is_empty() {
                self.out.push_str(&format!(
                    "  {} = call i64 asm sideeffect inteldialect \"{}\", \"{}\"()\n",
                    res, escaped, constraint_text
                ));
            } else {
                self.out.push_str(&format!(
                    "  {} = call i64 asm sideeffect inteldialect \"{}\", \"{}\"({})\n",
                    res, escaped, constraint_text, args
                ));
            }
        }
        Ok(())
    }

    fn render_args(&mut self, args: &[PirOperand]) -> Result<String> {
        let mut out = Vec::new();
        for arg in args {
            match arg {
                PirOperand::Int(i) => out.push(format!("i64 {}", i)),
                PirOperand::Bool(b) => out.push(format!("i1 {}", if *b { 1 } else { 0 })),
                PirOperand::NullPtr => out.push("i8* null".to_string()),
                PirOperand::Var(v) => {
                    let val = self.names.get(v).cloned().ok_or_else(|| {
                        CompilerError::codegen_error(format!(
                            "C* variable '{}' has no LLVM value",
                            v
                        ))
                    })?;
                    out.push(format!("i8* {}", self.cast_existing_to_i8_ptr(val)));
                }
            }
        }
        Ok(out.join(", "))
    }

    fn operand_i64(&self, op: &PirOperand) -> Result<String> {
        match op {
            PirOperand::Int(i) => Ok(i.to_string()),
            _ => Err(CompilerError::codegen_error(
                "C* expected i64 integer operand",
            )),
        }
    }
    fn operand_ptr_erased(&mut self, op: &PirOperand) -> Result<String> {
        match op {
            PirOperand::NullPtr => Ok("null".to_string()),
            PirOperand::Var(v) => {
                let val = self.names.get(v).cloned().ok_or_else(|| {
                    CompilerError::codegen_error(format!(
                        "C* ptr variable '{}' has no LLVM value",
                        v
                    ))
                })?;
                Ok(self.cast_existing_to_i8_ptr(val))
            }
            _ => Err(CompilerError::codegen_error("C* expected pointer operand")),
        }
    }
    fn operand_with_type(&mut self, op: &PirOperand, ty: &PirType) -> Result<String> {
        match op {
            PirOperand::Int(i) => Ok(i.to_string()),
            PirOperand::Bool(b) => Ok(if *b { "1".to_string() } else { "0".to_string() }),
            PirOperand::NullPtr => Ok("null".to_string()),
            PirOperand::Var(v) => {
                let val = self.names.get(v).cloned().ok_or_else(|| {
                    CompilerError::codegen_error(format!("C* variable '{}' has no LLVM value", v))
                })?;
                if ty.is_ptr() && ty.llvm() != "i8*" {
                    let cast = self.next_tmp();
                    let ptr_val = self.cast_existing_to_i8_ptr(val);
                    self.out.push_str(&format!(
                        "  {} = bitcast i8* {} to {}\n",
                        cast,
                        ptr_val,
                        ty.llvm()
                    ));
                    Ok(cast)
                } else {
                    Ok(val)
                }
            }
        }
    }
    fn cast_existing_to_i8_ptr(&mut self, val: String) -> String {
        val
    }
    fn next_tmp(&mut self) -> String {
        self.tmp += 1;
        format!("%cstar{}", self.tmp)
    }
}

fn annotation_map(annotations: &[Annotation], name: &str) -> Option<HashMap<String, String>> {
    annotations.iter().find(|a| a.name == name).map(|a| {
        let mut m = HashMap::new();
        for (idx, arg) in a.arguments.iter().enumerate() {
            let key = arg.name.clone().unwrap_or_else(|| idx.to_string());
            let val = string_or_int_from_expr(&arg.value).unwrap_or_default();
            m.insert(key, val);
        }
        m
    })
}

fn annotation_string(annotations: &[Annotation], name: &str) -> Result<Option<String>> {
    for a in annotations {
        if a.name == name {
            let first = a.arguments.get(0).ok_or_else(|| {
                CompilerError::codegen_error(format!("@{} requires a string argument", name))
            })?;
            return string_from_expr(&first.value).map(Some);
        }
    }
    Ok(None)
}
fn annotation_int(annotations: &[Annotation], name: &str) -> Result<Option<i64>> {
    for a in annotations {
        if a.name == name {
            let first = a.arguments.get(0).ok_or_else(|| {
                CompilerError::codegen_error(format!("@{} requires an integer argument", name))
            })?;
            return int_from_expr(&first.value).map(Some);
        }
    }
    Ok(None)
}
fn annotation_patch(annotations: &[Annotation]) -> Result<Option<(String, String)>> {
    for a in annotations {
        if a.name == "patch" {
            let m = annotation_map(annotations, "patch").unwrap_or_default();
            return Ok(Some((
                m.get("base")
                    .cloned()
                    .unwrap_or_else(|| "firmware_base.bin".to_string()),
                m.get("output")
                    .cloned()
                    .unwrap_or_else(|| "update.patch".to_string()),
            )));
        }
    }
    Ok(None)
}
fn annotation_isr(annotations: &[Annotation]) -> Result<Option<(i64, i64)>> {
    for a in annotations {
        if a.name == "isr_group" {
            let mut priority = 0;
            let mut budget_us = 0;
            for arg in &a.arguments {
                match arg.name.as_deref() {
                    Some("priority") => priority = priority_from_expr(&arg.value)?,
                    Some("budget_us") | Some("budget") => budget_us = int_from_expr(&arg.value)?,
                    None => budget_us = int_from_expr(&arg.value)?,
                    Some(other) => {
                        return Err(CompilerError::codegen_error(format!(
                            "@isr_group unknown parameter '{}'",
                            other
                        )))
                    }
                }
            }
            return Ok(Some((priority, budget_us)));
        }
    }
    Ok(None)
}

fn string_or_int_from_expr(expr: &Expr) -> Result<String> {
    match expr {
        Expr::Integer(i) => Ok(i.value.to_string()),
        _ => string_from_expr(expr),
    }
}
fn string_from_expr(expr: &Expr) -> Result<String> {
    match expr {
        Expr::String_(s) => string_parts_to_plain(&s.parts),
        Expr::MultiLineString(s) => string_parts_to_plain(&s.parts),
        Expr::Identifier(id) => Ok(id.name.clone()),
        _ => Err(CompilerError::codegen_error(
            "C* annotation argument must be a string literal",
        )),
    }
}
fn string_parts_to_plain(parts: &[StringPart]) -> Result<String> {
    let mut out = String::new();
    for part in parts {
        match part {
            StringPart::Text(t) => out.push_str(t),
            _ => {
                return Err(CompilerError::codegen_error(
                    "C* annotation strings must be literal text without interpolation",
                ))
            }
        }
    }
    Ok(out)
}
fn int_from_expr(expr: &Expr) -> Result<i64> {
    match expr {
        Expr::Integer(i) => Ok(i.value),
        _ => Err(CompilerError::codegen_error(
            "C* annotation integer argument must be an integer literal",
        )),
    }
}
fn priority_from_expr(expr: &Expr) -> Result<i64> {
    match expr {
        Expr::Integer(i) => Ok(i.value),
        Expr::String_(s) => {
            let v = string_from_expr(&Expr::String_(s.clone()))?;
            Ok(match v.as_str() {
                "high" => 0,
                "medium" | "normal" => 5,
                "low" => 10,
                _ => 5,
            })
        }
        Expr::Identifier(id) => Ok(match id.name.as_str() {
            "high" => 0,
            "medium" | "normal" => 5,
            "low" => 10,
            _ => 5,
        }),
        _ => Err(CompilerError::codegen_error(
            "C* @isr_group priority must be integer or high/medium/low",
        )),
    }
}
fn type_expr_is_ptr(t: &TypeExpr) -> bool {
    matches!(t, TypeExpr::Generic { base, .. } if matches!(base.as_ref(), TypeExpr::Named(id, _) if id.name == "ptr"))
}
fn estimate_wcet_cycles(nodes: &[PirNode]) -> u64 {
    nodes
        .iter()
        .map(|n| match n {
            PirNode::AsmBlock { template, .. } => template.lines().count().max(1) as u64 * 4,
            PirNode::Call { .. } => 12,
            PirNode::Load { .. } | PirNode::Store { .. } => 3,
            PirNode::Loop { body, .. } => {
                // Conservative WCET: assume up to 1000 iterations.
                // The previous *2 was a systematic underestimate that
                // let dangerous ISR code pass the safety check.
                16 + estimate_wcet_cycles(body) * 1000
            }
            PirNode::Pipeline { .. } => 20,
            PirNode::Prefetch { .. } => 2,
            _ => 1,
        })
        .sum()
}

fn parse_repo_config(source: &str) -> RepoConfig {
    let mut repo = RepoConfig {
        enabled: source.contains("@repo"),
        sources: Vec::new(),
        split_by: "size".to_string(),
        output_header: "flash_layout.h".to_string(),
        output_map: "layout.map".to_string(),
        output_binary: "pkg_index.bin".to_string(),
    };
    for line in source.lines() {
        let t = line.trim();
        if t.contains("http") || t.contains("local:") {
            for part in extract_quoted(t) {
                repo.sources.push(part);
            }
        }
        if t.contains("header") {
            if let Some(v) = extract_quoted(t).last() {
                repo.output_header = v.clone();
            }
        }
        if t.contains("map") {
            if let Some(v) = extract_quoted(t).last() {
                repo.output_map = v.clone();
            }
        }
        if t.contains("binary") {
            if let Some(v) = extract_quoted(t).last() {
                repo.output_binary = v.clone();
            }
        }
    }
    repo
}
fn parse_source_imports(source: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for line in source.lines() {
        let t = line.trim();
        if t.starts_with("import") {
            let names = extract_quoted(t);
            if let Some(name) = names.get(0) {
                let forced = t
                    .split(" to ")
                    .nth(1)
                    .map(|s| s.trim().trim_matches(',').to_string());
                out.push((name.clone(), forced));
            }
        }
    }
    out
}
fn extract_quoted(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_q = false;
    let mut buf = String::new();
    for ch in s.chars() {
        if ch == '"' {
            if in_q {
                out.push(buf.clone());
                buf.clear();
            }
            in_q = !in_q;
        } else if in_q {
            buf.push(ch);
        }
    }
    out
}
fn build_package_layout(
    imports: &[(String, Option<String>)],
    repo: &RepoConfig,
) -> Vec<PackageEntry> {
    let mut cursors: HashMap<String, u64> = HashMap::new();
    cursors.insert("sram".to_string(), 0x2000_0000);
    cursors.insert("flash_fast".to_string(), 0x0800_2000);
    cursors.insert("flash_normal".to_string(), 0x0801_0000);
    cursors.insert("flash_archive".to_string(), 0x0810_0000);
    let mut source = if imports.is_empty() && repo.enabled {
        vec![("self".to_string(), None)]
    } else {
        imports.to_vec()
    };
    source.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (name, forced) in source {
        let size = estimate_package_size(&name);
        let partition = forced.unwrap_or_else(|| classify_package(size));
        let offset = *cursors.get(&partition).unwrap_or(&0x0800_0000);
        cursors.insert(partition.clone(), offset + align_to(size, 256));
        out.push(PackageEntry {
            checksum: stable_hash(&name) ^ size,
            name,
            partition,
            size,
            offset,
        });
    }
    out
}
fn estimate_package_size(name: &str) -> u64 {
    512 + (name.len() as u64 * 257)
}
fn classify_package(size: u64) -> String {
    if size <= 4096 {
        "sram".to_string()
    } else if size <= 32768 {
        "flash_fast".to_string()
    } else if size <= 524288 {
        "flash_normal".to_string()
    } else {
        "flash_archive".to_string()
    }
}
fn align_to(x: u64, a: u64) -> u64 {
    ((x + a - 1) / a) * a
}

fn emit_package_header(pkgs: &[PackageEntry]) -> String {
    let mut s = String::from("/* Auto-generated C* package layout. */\nstruct CstarPkgEntry { const char* name; unsigned long long load_addr; unsigned long long size; unsigned long long checksum; };\nstatic const struct CstarPkgEntry CSTAR_PKG_INDEX[] = {\n");
    for p in pkgs {
        s.push_str(&format!(
            "  {{\"{}\", 0x{:x}, {}, 0x{:x}}}, /* {} */\n",
            p.name, p.offset, p.size, p.checksum, p.partition
        ));
    }
    s.push_str("};\n");
    s
}
fn emit_layout_map(pkgs: &[PackageEntry]) -> String {
    let mut s = String::from("=== C* Package Layout ===\n");
    for p in pkgs {
        s.push_str(&format!(
            "{} 0x{:08x} size={} checksum=0x{:x} partition={}\n",
            p.name, p.offset, p.size, p.checksum, p.partition
        ));
    }
    s
}
fn emit_package_index(pkgs: &[PackageEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in pkgs {
        out.extend_from_slice(&(p.name.len() as u32).to_le_bytes());
        out.extend_from_slice(p.name.as_bytes());
        out.extend_from_slice(&p.offset.to_le_bytes());
        out.extend_from_slice(&p.size.to_le_bytes());
        out.extend_from_slice(&p.checksum.to_le_bytes());
    }
    out
}
fn emit_patch_bytes(plan: &PatchPlan, llvm_ir: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"CSTAR-PATCH\n");
    // Hash only the function's own IR (the substring between
    // "define ... @function_name" and the next "define" or EOF),
    // not the entire module — so different functions get different
    // hashes even when compiled in the same module.
    let func_ir = extract_function_ir(llvm_ir, &plan.function);
    out.extend_from_slice(
        format!(
            "function={}\nbase={}\noutput={}\nnew_hash=0x{:x}\n",
            plan.function,
            plan.base,
            plan.output,
            stable_hash(&func_ir)
        )
        .as_bytes(),
    );
    out
}

/// Extract the LLVM IR text for a single function from the full module IR.
fn extract_function_ir(llvm_ir: &str, function_name: &str) -> String {
    let define_prefix = format!("define {} @{}", "", function_name);
    // Search for "define ... @function_name(" in the IR.
    let mut start = None;
    for (i, line) in llvm_ir.lines().enumerate() {
        if line.contains(&format!("@{}", function_name)) && line.starts_with("define") {
            start = Some(i);
            break;
        }
    }
    match start {
        Some(s) => {
            let lines: Vec<&str> = llvm_ir.lines().collect();
            let mut end = lines.len();
            for (j, line) in lines.iter().enumerate().skip(s + 1) {
                if line.starts_with("define ") {
                    end = j;
                    break;
                }
            }
            lines[s..end].join("\n")
        }
        None => function_name.to_string(), // fallback: hash the name
    }
}

fn default_linker_script() -> String {
    r#"/* Auto-generated by vredrs C* raw backend. */
SECTIONS
{
  . = 0x100000;
  .text.boot : { KEEP(*(.text.boot)) *(.text.boot.*) }
  .isr : { KEEP(*(.isr)) *(.isr.*) }
  .pkgmeta : { KEEP(*(.pkgmeta)) *(.pkgmeta.*) }
  .patchmeta : { KEEP(*(.patchmeta)) *(.patchmeta.*) }
  .text : { *(.text*) }
  .rodata : { *(.rodata*) }
  .data : { *(.data*) }
  .bss : { *(.bss*) *(COMMON) }
}
"#
    .to_string()
}

fn escape_asm(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\0A")
}
fn escape_attr(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
fn sanitize_symbol(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
fn stable_hash<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

fn stmt_kind(stmt: &Stmt) -> &'static str {
    match stmt {
        Stmt::Assign(_) => "Assign",
        Stmt::Expr(_) => "Expr",
        Stmt::UnsafeBlock(_) => "UnsafeBlock",
        Stmt::Asm(_) => "Asm",
        Stmt::If(_) => "If",
        Stmt::While(_) => "While",
        Stmt::ForIn(_) => "ForIn",
        Stmt::ForRange(_) => "ForRange",
        Stmt::Loop(_) => "Loop",
        Stmt::Return(_) => "Return",
        _ => "Other",
    }
}
fn expr_kind(expr: &Expr) -> &'static str {
    match expr {
        Expr::Integer(_) => "Integer",
        Expr::Bool(_) => "Bool",
        Expr::Null(_) => "Null",
        Expr::Identifier(_) => "Identifier",
        Expr::Call(_) => "Call",
        Expr::MethodCall(_) => "MethodCall",
        Expr::Cast(_) => "Cast",
        _ => "Other",
    }
}
fn top_level_kind(t: &TopLevel) -> &'static str {
    match t {
        TopLevel::Import(_) => "Import",
        TopLevel::Export(_) => "Export",
        TopLevel::FnDef(_) => "FnDef",
        TopLevel::StructDef(_) => "StructDef",
        TopLevel::ClassDef(_) => "ClassDef",
        TopLevel::InterfaceDef(_) => "InterfaceDef",
        TopLevel::EnumDef(_) => "EnumDef",
        TopLevel::TypeAlias(_) => "TypeAlias",
        TopLevel::ConstExpr(_) => "ConstExpr",
        TopLevel::LazyDef(_) => "LazyDef",
        TopLevel::LazyFnDef(_) => "LazyFnDef",
        TopLevel::MacroDef(_) => "MacroDef",
        TopLevel::PluginDef(_) => "PluginDef",
        TopLevel::MarkerTrait(_) => "MarkerTrait",
        TopLevel::ExternFnDef(_) => "ExternFnDef",
        TopLevel::ConditionalCompile(_) => "ConditionalCompile",
        TopLevel::TestBlock(_) => "TestBlock",
        TopLevel::BenchBlock(_) => "BenchBlock",
        TopLevel::TraitDef(_) => "TraitDef",
        TopLevel::ImplBlock(_) => "ImplBlock",
        TopLevel::DtorBlock(_) => "DtorBlock",
        TopLevel::Statement(_) => "Statement",
    }
}
