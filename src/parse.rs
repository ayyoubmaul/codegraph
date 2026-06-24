//! Parse a single source file into definitions ([`Symbol`]), unresolved call
//! sites ([`CallRef`]), and import statements ([`ImportRef`]).
//!
//! A depth-first walk tracks the nearest enclosing definition: any node whose
//! kind maps to a [`SymbolKind`] becomes a symbol and the enclosing scope for
//! its descendants; any call node emits a `CallRef` from that enclosing
//! definition; any import node emits an `ImportRef`. Name/import resolution
//! happens later, in `graph::GraphBatch::build`.

use tree_sitter::{Node, Parser};

use crate::graph::{CallRef, GraphBatch, ImportRef};
use crate::lang::Lang;
use crate::symbol::Symbol;

/// Everything extracted from one file.
#[derive(Debug, Default)]
pub struct ParseResult {
    pub symbols: Vec<Symbol>,
    pub calls: Vec<CallRef>,
    pub imports: Vec<ImportRef>,
}

/// Parse `source` of the given language into definitions, calls, and imports.
pub fn parse_file(rel_path: &str, source: &[u8], lang: Lang) -> anyhow::Result<ParseResult> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang.language())
        .map_err(|e| anyhow::anyhow!("set_language failed: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("tree-sitter returned no tree"))?;

    let mut result = ParseResult::default();
    collect(tree.root_node(), source, lang, rel_path, None, &mut result);
    Ok(result)
}

fn collect(
    node: Node,
    source: &[u8],
    lang: Lang,
    rel_path: &str,
    enclosing: Option<&str>,
    out: &mut ParseResult,
) {
    // The definition (if any) this node opens, becoming its descendants' scope.
    let mut opened_def: Option<String> = None;

    if let Some(kind) = lang.symbol_kind(node.kind()) {
        if let Some(name_node) = node.child_by_field_name("name") {
            if let Ok(name) = name_node.utf8_text(source) {
                let start_line = node.start_position().row + 1;
                opened_def = Some(GraphBatch::def_id(rel_path, name, start_line));
                out.symbols.push(Symbol {
                    kind,
                    name: name.to_string(),
                    file: rel_path.to_string(),
                    start_line,
                    end_line: node.end_position().row + 1,
                });
            }
        }
    } else if lang.is_call_node(node.kind()) {
        if let Some(caller) = enclosing {
            if let Some((callee, is_method)) = lang.callee_name_of(node, source) {
                out.calls.push(CallRef {
                    caller_id: caller.to_string(),
                    callee_name: callee,
                    file: rel_path.to_string(),
                    is_method,
                });
            }
        }
    } else if lang.is_import_node(node.kind()) {
        if let Some(src) = lang.import_source(node, source) {
            out.imports.push(ImportRef {
                file: rel_path.to_string(),
                source: src,
            });
        }
    }

    let child_scope = opened_def.as_deref().or(enclosing);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, lang, rel_path, child_scope, out);
    }
}
