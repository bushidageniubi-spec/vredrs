//! Code generation backends.
//!
//! ## 架构（手册 Phase 2 目标）
//!
//! ```text
//! AST → lower.rs → Static IR → [VM / LLVM / Raw / Cstar]
//! ```
//!
//! 所有后端共享同一个 [`ir::IrModule`] 输入。新增语法只需修改
//! [`lower`]（以及 VM 字节码编译器，若需要字节码路径）。

pub mod ir;
pub mod lower;
pub mod emit_raw_x86;
pub mod emit_raw_arm;
pub mod emit_llvm;
pub mod llvm;
pub mod llvm_full;
pub mod cstar;
pub mod raw;

#[cfg(test)]
mod tests;
