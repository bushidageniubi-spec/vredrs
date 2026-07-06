//! File classification — maps file extensions to compilation modes.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Veds,
    Vraw,
    Cpps,
    Other,
}

pub fn classify_file(path: &Path) -> FileKind {
    match path.extension().and_then(|e| e.to_str()) {
        Some("veds") => FileKind::Veds,
        Some("vraw") => FileKind::Vraw,
        Some("cpps") => FileKind::Cpps,
        _ => FileKind::Other,
    }
}
