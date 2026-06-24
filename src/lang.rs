//! Language detection and per-language tree-sitter wiring.

use std::path::Path;

use tree_sitter::{Language, Node};

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

    /// Is this node a function/method call?
    pub fn is_call_node(self, node_kind: &str) -> bool {
        match self {
            Lang::Python => node_kind == "call",
            _ => node_kind == "call_expression",
        }
    }

    /// Extract `(callee_name, is_method)` from a call node's `function` field,
    /// unwrapping method/field/scoped access to the final identifier.
    /// `is_method` is true for receiver-style calls (`x.foo()`).
    /// `(callee_name, is_method, receiver_text)`. `receiver_text` is the object
    /// the method is called on (`self`/`this`/a variable/a qualifier), used for
    /// type-aware resolution.
    pub fn callee_name_of(
        self,
        call_node: Node,
        source: &[u8],
    ) -> Option<(String, bool, Option<String>)> {
        let func = call_node.child_by_field_name("function")?;
        let (name_node, is_method, recv) = match func.kind() {
            "identifier" | "field_identifier" | "property_identifier" | "type_identifier" => {
                (func, false, None)
            }
            "field_expression" => (
                func.child_by_field_name("field")?,
                true,
                func.child_by_field_name("value"),
            ), // Rust a.b()
            "selector_expression" => (
                func.child_by_field_name("field")?,
                true,
                func.child_by_field_name("operand"),
            ), // Go pkg.F()
            "member_expression" => (
                func.child_by_field_name("property")?,
                true,
                func.child_by_field_name("object"),
            ), // JS/TS a.b()
            "attribute" => (
                func.child_by_field_name("attribute")?,
                true,
                func.child_by_field_name("object"),
            ), // Python a.b()
            "scoped_identifier" => (
                func.child_by_field_name("name")?,
                false,
                func.child_by_field_name("path"),
            ), // Rust a::b()
            _ => return None,
        };
        let name = name_node.utf8_text(source).ok()?.to_string();
        let receiver = recv
            .and_then(|n| n.utf8_text(source).ok())
            .map(|s| s.to_string());
        Some((name, is_method, receiver))
    }

    /// If this node defines a type that owns methods, its type name (generics
    /// stripped). Used to tag methods with their owner type.
    pub fn type_container_name(self, node: Node, source: &[u8]) -> Option<String> {
        let name_node = match node.kind() {
            "impl_item" => node.child_by_field_name("type")?, // Rust  impl T
            "class_declaration" => node.child_by_field_name("name")?, // TS/JS class T
            "class_definition" => node.child_by_field_name("name")?, // Python class T
            _ => return None,
        };
        let text = name_node.utf8_text(source).ok()?;
        Some(text.split('<').next().unwrap_or(text).trim().to_string())
    }

    /// Is this node an import/use statement?
    pub fn is_import_node(self, node_kind: &str) -> bool {
        match self {
            Lang::Python => node_kind == "import_statement" || node_kind == "import_from_statement",
            Lang::Go => node_kind == "import_spec",
            Lang::Rust => node_kind == "use_declaration",
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => node_kind == "import_statement",
        }
    }

    /// Extract the raw import source (module path / specifier), quotes stripped.
    /// Resolution to a file happens in `graph::GraphBatch::build`.
    pub fn import_source(self, node: Node, source: &[u8]) -> Option<String> {
        let raw = match self {
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
                node.child_by_field_name("source")?.utf8_text(source).ok()?
            }
            Lang::Python => node
                .child_by_field_name("module_name")
                .or_else(|| node.child_by_field_name("name"))?
                .utf8_text(source)
                .ok()?,
            Lang::Go => node
                .child_by_field_name("path")
                .or_else(|| node.named_child(0))?
                .utf8_text(source)
                .ok()?,
            Lang::Rust => node.named_child(0)?.utf8_text(source).ok()?,
        };
        Some(raw.trim_matches(['"', '\'', '`']).to_string())
    }
}
