//! Build system — directory scanning, file classification, and multi-backend dispatch.

pub mod file_classifier;
pub mod build;

pub use build::{build_project, build_project_simple, BuildResult, BuildOptions};
pub use file_classifier::{FileKind, classify_file};
