//! Parse a single source file into a flat list of [`Symbol`]s.
//!
//! v1 walks the tree-sitter syntax tree and pulls out top-level and nested
//! definitions by their `name:` field. Call edges and imports come in a later
//! slice once the graph store lands.

use tree_sitter::{Node, Parser};

use crate::lang::Lang;
use crate::symbol::Symbol;

/// Parse `source` (raw bytes) of the given language, returning every definition.
pub fn parse_file(rel_path: &str, source: &[u8], lang: Lang) -> anyhow::Result<Vec<Symbol>> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang.language())
        .map_err(|e| anyhow::anyhow!("set_language failed: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter returned no tree"))?;

    let mut symbols = Vec::new();
    collect(tree.root_node(), source, lang, rel_path, &mut symbols);
    Ok(symbols)
}

/// Depth-first walk: any node whose kind maps to a [`SymbolKind`] and that has
/// a `name:` field becomes a symbol.
fn collect(node: Node, source: &[u8], lang: Lang, rel_path: &str, out: &mut Vec<Symbol>) {
    if let Some(kind) = lang.symbol_kind(node.kind()) {
        if let Some(name_node) = node.child_by_field_name("name") {
            if let Ok(name) = name_node.utf8_text(source) {
                out.push(Symbol {
                    kind,
                    name: name.to_string(),
                    file: rel_path.to_string(),
                    start_line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                });
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, lang, rel_path, out);
    }
}
