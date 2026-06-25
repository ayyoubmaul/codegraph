//! Parse a single source file into definitions ([`Symbol`]), unresolved call
//! sites ([`CallRef`]), and import statements ([`ImportRef`]).
//!
//! A depth-first walk tracks: the enclosing definition; the enclosing type
//! (impl/class) so definitions get an `owner`; and a per-function variable→type
//! map built from typed parameters and the Go method receiver. Method calls on
//! `self`/`this` or on a typed variable carry the receiver type, enabling
//! type-aware resolution.

use std::collections::HashMap;

use tree_sitter::{Node, Parser};

use crate::graph::{CallRef, GraphBatch, ImportRef};
use crate::lang::Lang;
use crate::symbol::{Symbol, SymbolKind};

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
    let no_vars = HashMap::new();
    collect(
        tree.root_node(),
        source,
        lang,
        rel_path,
        None,
        None,
        &no_vars,
        &mut result,
    );
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn collect(
    node: Node,
    source: &[u8],
    lang: Lang,
    rel_path: &str,
    enclosing: Option<&str>,
    enclosing_type: Option<&str>,
    var_types: &HashMap<String, String>,
    out: &mut ParseResult,
) {
    let mut opened_def: Option<String> = None;
    let opened_type: Option<String> = lang.type_container_name(node, source);

    // Entering a function/method: build its variable→type scope (params + Go
    // receiver). Descendant calls resolve receivers against this map.
    let is_fn = matches!(
        lang.symbol_kind(node.kind()),
        Some(SymbolKind::Function | SymbolKind::Method)
    );
    let local_vars: Option<HashMap<String, String>> = if is_fn {
        let mut m = HashMap::new();
        for (n, t) in lang.param_types(node, source) {
            m.insert(n, t);
        }
        if let Some((rn, rt)) = lang.go_receiver(node, source) {
            m.insert(rn, rt);
        }
        for (n, t) in lang.local_var_types(node, source) {
            m.insert(n, t);
        }
        Some(m)
    } else {
        None
    };
    let child_vars: &HashMap<String, String> = local_vars.as_ref().unwrap_or(var_types);

    if let Some(kind) = lang.symbol_kind(node.kind()) {
        if let Some(name_node) = node.child_by_field_name("name") {
            if let Ok(name) = name_node.utf8_text(source) {
                let start_line = node.start_position().row + 1;
                opened_def = Some(GraphBatch::def_id(rel_path, name, start_line));
                // Owner: Go method → receiver type; else the enclosing type.
                let owner = lang
                    .go_receiver(node, source)
                    .map(|(_, t)| t)
                    .or_else(|| enclosing_type.map(String::from));
                out.symbols.push(Symbol {
                    kind,
                    name: name.to_string(),
                    file: rel_path.to_string(),
                    start_line,
                    end_line: node.end_position().row + 1,
                    owner,
                });
            }
        }
    } else if lang.is_call_node(node.kind()) {
        if let Some(caller) = enclosing {
            if let Some((callee, is_method, receiver)) = lang.callee_name_of(node, source) {
                let receiver_type = match receiver.as_deref() {
                    Some("self") | Some("this") => enclosing_type.map(String::from),
                    Some(r) => var_types.get(r).cloned().or_else(|| {
                        // Capitalized receiver (`Type::new`, `Type.make()`) is a
                        // type/class reference → resolve as that type.
                        r.chars()
                            .next()
                            .is_some_and(|c| c.is_uppercase())
                            .then(|| r.to_string())
                    }),
                    None => None,
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
        collect(
            child,
            source,
            lang,
            rel_path,
            child_scope,
            child_type,
            child_vars,
            out,
        );
    }
}
