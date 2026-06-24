//! Language detection and per-language tree-sitter wiring.

use std::path::Path;

use tree_sitter::Language;

use crate::symbol::SymbolKind;

/// A source language we can parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    Go,
    TypeScript,
    Tsx,
    JavaScript,
}

impl Lang {
    /// Detect a language from a file's extension, if supported.
    pub fn from_path(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?;
        Some(match ext {
            "rs" => Lang::Rust,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            _ => return None,
        })
    }

    /// The tree-sitter grammar for this language.
    pub fn language(self) -> Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            // JSX-capable grammars cover .tsx and plain .js/.jsx.
            Lang::Tsx | Lang::JavaScript => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }

    // Used by later slices (graph/semantic output); kept here as the canonical name.
    #[allow(dead_code)]
    pub fn display_name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
        }
    }

    /// Map a tree-sitter node kind to a [`SymbolKind`], if it's a definition
    /// we care about. Returns `None` for everything else.
    pub fn symbol_kind(self, node_kind: &str) -> Option<SymbolKind> {
        use SymbolKind::*;
        let kind = match self {
            Lang::Rust => match node_kind {
                "function_item" => Function,
                "struct_item" => Struct,
                "enum_item" => Enum,
                "trait_item" => Trait,
                "mod_item" => Module,
                _ => return None,
            },
            Lang::Python => match node_kind {
                "function_definition" => Function,
                "class_definition" => Class,
                _ => return None,
            },
            Lang::Go => match node_kind {
                "function_declaration" => Function,
                "method_declaration" => Method,
                "type_spec" => Type,
                _ => return None,
            },
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => match node_kind {
                "function_declaration" => Function,
                "method_definition" => Method,
                "class_declaration" => Class,
                "interface_declaration" => Interface,
                _ => return None,
            },
        };
        Some(kind)
    }
}
