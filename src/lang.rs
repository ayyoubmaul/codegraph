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

    /// `(param_name, base_type_name)` for a function/method's typed parameters
    /// (Rust/Go/TS/Python). Untyped params are skipped.
    pub fn param_types(self, fn_node: Node, source: &[u8]) -> Vec<(String, String)> {
        let Some(params) = fn_node.child_by_field_name("parameters") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut cursor = params.walk();
        for p in params.children(&mut cursor) {
            let (name, ty) = match p.kind() {
                "parameter" => (p.child_by_field_name("pattern"), p.child_by_field_name("type")),
                "parameter_declaration" => {
                    (p.child_by_field_name("name"), p.child_by_field_name("type"))
                }
                "required_parameter" | "optional_parameter" => {
                    (p.child_by_field_name("pattern"), p.child_by_field_name("type"))
                }
                "typed_parameter" | "typed_default_parameter" => {
                    (p.named_child(0), p.child_by_field_name("type"))
                }
                _ => continue,
            };
            if let (Some(name), Some(ty)) = (name, ty) {
                if let (Ok(n), Some(t)) = (name.utf8_text(source), first_type_name(ty, source)) {
                    out.push((n.to_string(), t));
                }
            }
        }
        out
    }

    /// Go method receiver `(name, type)` — `func (c *Cache) m()` → `(c, Cache)`.
    pub fn go_receiver(self, method_node: Node, source: &[u8]) -> Option<(String, String)> {
        let recv = method_node.child_by_field_name("receiver")?;
        let mut cursor = recv.walk();
        for p in recv.children(&mut cursor) {
            if p.kind() == "parameter_declaration" {
                let name = p.child_by_field_name("name")?.utf8_text(source).ok()?;
                let ty = first_type_name(p.child_by_field_name("type")?, source)?;
                return Some((name.to_string(), ty));
            }
        }
        None
    }

    /// `(var_name, base_type)` for locals declared in this function body, where
    /// the type is inferable from an annotation or a constructor/literal.
    pub fn local_var_types(self, fn_node: Node, source: &[u8]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        collect_decls(fn_node, source, &mut out);
        out
    }
}

/// First `type_identifier` (or `identifier`) under `node` — the base type name,
/// unwrapping references/pointers/generics.
fn first_type_name(node: Node, source: &[u8]) -> Option<String> {
    find_kind(node, "type_identifier", source)
        .or_else(|| find_kind(node, "identifier", source))
        .map(|t| t.split('<').next().unwrap_or(&t).trim().to_string())
}

fn find_kind(node: Node, kind: &str, source: &[u8]) -> Option<String> {
    if node.kind() == kind {
        return node.utf8_text(source).ok().map(String::from);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(t) = find_kind(child, kind, source) {
            return Some(t);
        }
    }
    None
}

/// Walk a function body collecting `(var, type)` from typed/constructed locals.
fn collect_decls(node: Node, source: &[u8], out: &mut Vec<(String, String)>) {
    match node.kind() {
        "let_declaration" => {
            // Rust: let x: T = ..  /  let x = T::new(..)  /  let x = T { .. }
            if let Some(name) = pattern_ident(node.child_by_field_name("pattern"), source) {
                let ty = node
                    .child_by_field_name("type")
                    .and_then(|t| first_type_name(t, source))
                    .or_else(|| node.child_by_field_name("value").and_then(|v| ctor_type(v, source)));
                if let Some(t) = ty {
                    out.push((name, t));
                }
            }
        }
        "variable_declarator" => {
            // TS: const x: T = ..  /  const x = new T(..)
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
            {
                let ty = node
                    .child_by_field_name("type")
                    .and_then(|t| first_type_name(t, source))
                    .or_else(|| node.child_by_field_name("value").and_then(|v| ctor_type(v, source)));
                if let Some(t) = ty {
                    out.push((name.to_string(), t));
                }
            }
        }
        "var_spec" | "const_spec" => {
            // Go: var x T
            if let (Some(name), Some(t)) = (
                node.child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok()),
                node.child_by_field_name("type")
                    .and_then(|t| first_type_name(t, source)),
            ) {
                out.push((name.to_string(), t));
            }
        }
        "short_var_declaration" => {
            // Go: x := T{..}  /  x := NewT(..)
            if let (Some(left), Some(right)) = (
                node.child_by_field_name("left"),
                node.child_by_field_name("right"),
            ) {
                let mut lc = left.walk();
                let mut rc = right.walk();
                let names: Vec<_> = left.named_children(&mut lc).collect();
                let vals: Vec<_> = right.named_children(&mut rc).collect();
                for (nm, val) in names.iter().zip(vals.iter()) {
                    if let (Ok(name), Some(t)) = (nm.utf8_text(source), ctor_type(*val, source)) {
                        out.push((name.to_string(), t));
                    }
                }
            }
        }
        "assignment" => {
            // Python: x: T = ..  /  x = T(..)
            if let Some(name) = node.child_by_field_name("left").and_then(|n| {
                (n.kind() == "identifier")
                    .then(|| n.utf8_text(source).ok())
                    .flatten()
            }) {
                let ty = node
                    .child_by_field_name("type")
                    .and_then(|t| first_type_name(t, source))
                    .or_else(|| node.child_by_field_name("right").and_then(|v| ctor_type(v, source)));
                if let Some(t) = ty {
                    out.push((name.to_string(), t));
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_decls(child, source, out);
    }
}

/// Infer a base type from an initializer: `T::new(..)`, `T{..}`, `new T(..)`,
/// `NewT(..)` (Go), `T(..)` (capitalized → constructor).
fn ctor_type(value: Node, source: &[u8]) -> Option<String> {
    match value.kind() {
        "call_expression" | "call" => {
            let func = value.child_by_field_name("function")?;
            match func.kind() {
                "scoped_identifier" => first_type_name(func.child_by_field_name("path")?, source),
                "type_identifier" => first_type_name(func, source),
                _ => ctor_name_to_type(func.utf8_text(source).ok()?),
            }
        }
        "struct_expression" => first_type_name(value.child_by_field_name("name")?, source),
        "composite_literal" => first_type_name(value.child_by_field_name("type")?, source),
        "new_expression" => first_type_name(value.child_by_field_name("constructor")?, source),
        "reference_expression" | "unary_expression" | "parenthesized_expression"
        | "await_expression" => {
            let mut cursor = value.walk();
            value.children(&mut cursor).find_map(|c| ctor_type(c, source))
        }
        _ => None,
    }
}

/// `NewCache`/`newCache` → `Cache`; a capitalized call name → itself (a class).
fn ctor_name_to_type(name: &str) -> Option<String> {
    let last = name.rsplit(['.', ':']).next().unwrap_or(name);
    if let Some(rest) = last.strip_prefix("New").or_else(|| last.strip_prefix("new")) {
        if rest.chars().next().is_some_and(|c| c.is_uppercase()) {
            return Some(rest.to_string());
        }
    }
    last.chars()
        .next()
        .is_some_and(|c| c.is_uppercase())
        .then(|| last.to_string())
}

fn pattern_ident(pattern: Option<Node>, source: &[u8]) -> Option<String> {
    let p = pattern?;
    match p.kind() {
        "identifier" => p.utf8_text(source).ok().map(String::from),
        "mut_pattern" => p
            .named_child(0)
            .and_then(|c| c.utf8_text(source).ok())
            .map(String::from),
        _ => None,
    }
}
