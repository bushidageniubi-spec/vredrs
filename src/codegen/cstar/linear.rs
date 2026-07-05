//! Linear ownership checker for C* PIR.
//!
//! Every `ptr[T]` and every `AllocLinear` result is a MustConsume resource.
//! The resource must be consumed by `free`, `consume`, or `move` before leaving
//! the raw scope. Use-after-move, double-free and leaking pointers are hard
//! codegen errors.

use super::pir::{PirNode, PirProgram, PirType};
use crate::error::{CompilerError, Result};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinearState {
    MustConsume,
    Moved,
    Consumed,
}

#[derive(Debug, Clone)]
pub struct LinearResource {
    pub ty: PirType,
    pub state: LinearState,
    pub origin: String,
}

#[derive(Debug, Default)]
pub struct LinearChecker {
    resources: HashMap<String, LinearResource>,
}

impl LinearChecker {
    pub fn new() -> Self {
        Self {
            resources: HashMap::new(),
        }
    }

    pub fn check(program: &PirProgram) -> Result<()> {
        let mut checker = Self::new();
        checker.check_program(program)
    }

    fn check_program(&mut self, program: &PirProgram) -> Result<()> {
        for node in &program.nodes {
            self.check_node(node)?;
        }
        self.finish_scope("top-level unsafe scope")?;
        for function in &program.functions {
            let mut fn_checker = LinearChecker::new();
            for node in &function.body {
                fn_checker.check_node(node)?;
            }
            fn_checker.finish_scope(&format!("function {}", function.name))?;
        }
        Ok(())
    }

    fn finish_scope(&self, label: &str) -> Result<()> {
        let leaks: Vec<String> = self
            .resources
            .iter()
            .filter(|(_, r)| r.state == LinearState::MustConsume)
            .map(|(name, r)| format!("{} allocated at {}", name, r.origin))
            .collect();
        if !leaks.is_empty() {
            return Err(CompilerError::codegen_error(format!(
                "C* linear resource leak in {}: ptr resources must be explicitly free/consume/move before leaving scope: {}",
                label, leaks.join(", ")
            )));
        }
        Ok(())
    }

    fn check_node(&mut self, node: &PirNode) -> Result<()> {
        match node {
            PirNode::AllocLinear { dst, ty, .. } => {
                if !ty.is_ptr() {
                    return Err(CompilerError::codegen_error(format!(
                        "C* AllocLinear for '{}' must produce ptr[T], got {:?}",
                        dst, ty
                    )));
                }
                if let Some(existing) = self.resources.get(dst) {
                    if existing.state == LinearState::MustConsume {
                        return Err(CompilerError::codegen_error(format!(
                            "C* variable '{}' is reassigned while previous linear resource is still live", dst
                        )));
                    }
                }
                self.resources.insert(
                    dst.clone(),
                    LinearResource {
                        ty: ty.clone(),
                        state: LinearState::MustConsume,
                        origin: "malloc/alloc-linear".to_string(),
                    },
                );
            }
            PirNode::Move { dst, src, ty } => {
                self.ensure_live(src, "move source")?;
                self.mark(src, LinearState::Moved, "move source")?;
                if ty.is_ptr() {
                    // Check if dst already holds an unconsumed resource.
                    if let Some(existing) = self.resources.get(dst) {
                        if existing.state == LinearState::MustConsume {
                            return Err(CompilerError::codegen_error(format!(
                                "C* move into '{}' which still holds an unconsumed linear resource (from {})",
                                dst, existing.origin
                            )));
                        }
                    }
                    self.resources.insert(
                        dst.clone(),
                        LinearResource {
                            ty: ty.clone(),
                            state: LinearState::MustConsume,
                            origin: format!("move({})", src),
                        },
                    );
                }
            }
            PirNode::Consume { var } => {
                self.ensure_live(var, "consume")?;
                self.mark(var, LinearState::Consumed, "consume")?;
            }
            PirNode::Call {
                func,
                args,
                ret_ty,
                dst,
            } => {
                for arg in args {
                    if let Some(name) = arg.as_var() {
                        self.ensure_not_moved_or_consumed(name, func)?;
                    }
                }
                match func.as_str() {
                    "free" | "consume" => {
                        let var = args.get(0).and_then(|a| a.as_var()).ok_or_else(|| {
                            CompilerError::codegen_error(format!(
                                "C* {} requires a variable ptr argument",
                                func
                            ))
                        })?;
                        self.ensure_live(var, func)?;
                        self.mark(var, LinearState::Consumed, func)?;
                    }
                    _ => {}
                }
                if let Some(dst) = dst {
                    if ret_ty.is_ptr() {
                        // Check if dst already holds an unconsumed resource.
                        if let Some(existing) = self.resources.get(dst) {
                            if existing.state == LinearState::MustConsume {
                                return Err(CompilerError::codegen_error(format!(
                                    "C* call result into '{}' which still holds an unconsumed linear resource (from {})",
                                    dst, existing.origin
                                )));
                            }
                        }
                        self.resources.insert(
                            dst.clone(),
                            LinearResource {
                                ty: ret_ty.clone(),
                                state: LinearState::MustConsume,
                                origin: format!("call {}", func),
                            },
                        );
                    }
                }
            }
            PirNode::Load { ptr, .. } => {
                if let Some(name) = ptr.as_var() {
                    self.ensure_not_moved_or_consumed(name, "load ptr")?;
                }
            }
            PirNode::Store { ptr, value, .. } => {
                if let Some(name) = ptr.as_var() {
                    self.ensure_not_moved_or_consumed(name, "store ptr")?;
                }
                if let Some(name) = value.as_var() {
                    self.ensure_not_moved_or_consumed(name, "store value")?;
                }
            }
            PirNode::AsmBlock { inputs, .. } => {
                for input in inputs {
                    if let Some(name) = input.as_var() {
                        self.ensure_not_moved_or_consumed(name, "asm input")?;
                    }
                }
            }
            PirNode::Prefetch { ptr, .. } => {
                if let Some(op) = ptr {
                    if let Some(name) = op.as_var() {
                        self.ensure_not_moved_or_consumed(name, "prefetch ptr")?;
                    }
                }
            }
            PirNode::Loop { body, .. } => {
                for node in body {
                    self.check_node(node)?;
                }
            }
            PirNode::Pipeline { .. }
            | PirNode::PatchMarker { .. }
            | PirNode::PackageMarker { .. } => {
                // Physical metadata nodes do not create or consume linear resources.
            }
        }
        Ok(())
    }

    fn ensure_live(&self, name: &str, op: &str) -> Result<()> {
        match self.resources.get(name) {
            Some(r) if r.state == LinearState::MustConsume => Ok(()),
            Some(r) => Err(CompilerError::codegen_error(format!(
                "C* linear error: cannot {} '{}'; resource is already {:?}",
                op, name, r.state
            ))),
            None => Err(CompilerError::codegen_error(format!(
                "C* linear error: cannot {} '{}'; variable is not a live linear ptr resource",
                op, name
            ))),
        }
    }

    fn ensure_not_moved_or_consumed(&self, name: &str, op: &str) -> Result<()> {
        if let Some(r) = self.resources.get(name) {
            if r.state != LinearState::MustConsume {
                return Err(CompilerError::codegen_error(format!(
                    "C* linear error: {} uses '{}' after {:?}",
                    op, name, r.state
                )));
            }
        }
        Ok(())
    }

    fn mark(&mut self, name: &str, state: LinearState, op: &str) -> Result<()> {
        let res = self.resources.get_mut(name).ok_or_else(|| {
            CompilerError::codegen_error(format!(
                "C* linear error: {} of unknown resource '{}'",
                op, name
            ))
        })?;
        if res.state != LinearState::MustConsume {
            return Err(CompilerError::codegen_error(format!(
                "C* linear error: {} of '{}' after {:?}",
                op, name, res.state
            )));
        }
        res.state = state;
        Ok(())
    }
}
