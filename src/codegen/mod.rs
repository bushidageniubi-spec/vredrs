//! Code generation backends.

pub mod llvm;
pub mod llvm_full;
pub mod cstar;
pub mod raw;

#[cfg(test)]
mod tests;
