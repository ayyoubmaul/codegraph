//! Parse a single source file into definitions ([`Symbol`]), unresolved call
//! sites ([`CallRef`]), and import statements ([`ImportRef`]).
//!
//! A depth-first walk tracks the nearest enclosing definition *and* the
//! enclosing type. Definitions inside a type (impl/class) are tagged with that
//! type as their `owner`; method calls on `self`/`this` carry that type as the
//! receiver type, so resolution can be type-aware.

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
    collect(tree.root_node(), source, lang, rel_path, None, None, &mut result);
    Ok(result)
}

fn collect(
    node: Node,
    source: &[u8],
    lang: Lang,
    rel_path: &str,
    enclosing: Option<&str>,
    enclosing_type: Option<&str>,
    out: &mut ParseResult,
) {
    // The definition this node opens, becoming its descendants' scope.
    let mut opened_def: Option<String> = None;
    // The type this node opens (impl/class), becoming descendants' owner type.
    let opened_type: Option<String> = lang.type_container_name(node, source);

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
                    owner: enclosing_type.map(String::from),
                });
            }
        }
    } else if lang.is_call_node(node.kind()) {
        if let Some(caller) = enclosing {
            if let Some((callee, is_method, receiver)) = lang.callee_name_of(node, source) {
                // Type-aware: `self`/`this` calls target the enclosing type.
                let receiver_type = match receiver.as_deref() {
                    Some("self") | Some("this") => enclosing_type.map(String::from),
                    _ => None,
                };
                out.calls.push(CallRef {
                    caller_id: caller.to_string(),
                    callee_name: callee,
                    file: rel_path.to_string(),
                    is_method,
                    receiver_type,
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
    let child_type = opened_type.as_deref().or(enclosing_type);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, lang, rel_path, child_scope, child_type, out);
    }
}
