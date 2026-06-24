//! Core data types for extracted code symbols.

use serde::Serialize;

/// The kind of a definition we extract from source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Class,
    Interface,
    Type,
    Module,
}

/// A single named definition located in a source file.
#[derive(Debug, Clone, Serialize)]
pub struct Symbol {
    pub kind: SymbolKind,
    pub name: String,
    /// Repo-relative path, with forward slashes.
    pub file: String,
    /// 1-based, inclusive line range.
    pub start_line: usize,
    pub end_line: usize,
}
