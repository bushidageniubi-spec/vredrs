//! LLVM backend submodules.
//!
//! This module organizes the LLVM IR generator by feature area. The main
//! `FullLlvmGen` struct and its `impl` live in `../llvm_full.rs`; this
//! module provides helper functions that don't need access to the
//! generator's mutable state.

pub mod helpers;
