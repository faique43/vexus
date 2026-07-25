use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, QueryCursor};

use vexus_core::model::{FileIndex, NewSymbol, SymbolKind};

use crate::lang::Lang;

pub fn parse_file(lang: &Lang, rel_path: &str, source: &str) -> FileIndex {
    let mut idx = FileIndex::default();
    let module_qual = rel_path
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(rel_path)
        .replace('/', ".");
    let stem = module_qual.rsplit('.').next().unwrap_or(&module_qual).to_string();
    let total_lines = source.lines().count().max(1) as u32;
    idx.symbols.push(NewSymbol {
        name: stem, qualname: module_qual.clone(), kind: SymbolKind::Module,
        sig: None, start_line: 1, end_line: total_lines, parent: None, arity: None,
    });

    let mut parser = Parser::new();
    if parser.set_language(&lang.grammar()).is_err() {
        return idx; // structural-only degradation: module symbol still useful
    }
    let Some(tree) = parser.parse(source, None) else { return idx };

    // Collect definition captures: (def node, name, kind hint, params node)
    let query = lang.symbols_query();
    let mut defs: Vec<(Node, String, &str, Option<Node>)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        let mut def_node = None;
        let mut def_kind = "";
        let mut name = None;
        let mut params = None;
        for cap in m.captures {
            let cap_name = &query.capture_names()[cap.index as usize];
            match *cap_name {
                "def.function" => { def_node = Some(cap.node); def_kind = "function"; }
                "def.class" => { def_node = Some(cap.node); def_kind = "class"; }
                "def.struct" => { def_node = Some(cap.node); def_kind = "struct"; }
                "def.enum" => { def_node = Some(cap.node); def_kind = "enum"; }
                "def.trait" => { def_node = Some(cap.node); def_kind = "trait"; }
                "def.interface" => { def_node = Some(cap.node); def_kind = "interface"; }
                "def.const" => { def_node = Some(cap.node); def_kind = "const"; }
                "def.type" => { def_node = Some(cap.node); def_kind = "type"; }
                "def.name" => name = Some(cap.node.utf8_text(source.as_bytes()).unwrap_or("").to_string()),
                "def.params" => params = Some(cap.node),
                _ => {}
            }
        }
        if let (Some(node), Some(name)) = (def_node, name) {
            defs.push((node, name, def_kind, params));
        }
    }
    // Document order so parents precede children (store relies on this).
    defs.sort_by_key(|(n, ..)| n.start_byte());

    for (node, name, kind_hint, params) in defs {
        // Walk up to the nearest enclosing def already in idx.symbols.
        let parent = enclosing_symbol(&idx, node);
        let parent_kind = idx.symbols[parent].kind;
        let kind = match kind_hint {
            "function" if matches!(parent_kind, SymbolKind::Class | SymbolKind::Struct
                | SymbolKind::Enum | SymbolKind::Trait | SymbolKind::Interface) => SymbolKind::Method,
            "function" => SymbolKind::Function,
            "class" => SymbolKind::Class,
            "struct" => SymbolKind::Struct,
            "enum" => SymbolKind::Enum,
            "trait" => SymbolKind::Trait,
            "interface" => SymbolKind::Interface,
            "const" => SymbolKind::Const,
            "type" => SymbolKind::Type,
            _ => SymbolKind::Function,
        };
        let qualname = format!("{}.{}", idx.symbols[parent].qualname, name);
        let sig = source.lines().nth(node.start_position().row).map(|l| l.trim().to_string());
        let arity = params.map(|p| count_params(p, source));
        idx.symbols.push(NewSymbol {
            name, qualname, kind, sig,
            start_line: node.start_position().row as u32 + 1,
            end_line: node.end_position().row as u32 + 1,
            parent: Some(parent), arity,
        });
    }
    idx
}

/// Index (into idx.symbols) of the innermost symbol whose line range strictly
/// contains `node`, defaulting to the module symbol (0).
fn enclosing_symbol(idx: &FileIndex, node: Node) -> usize {
    let line = node.start_position().row as u32 + 1;
    let mut best = 0usize;
    for (i, s) in idx.symbols.iter().enumerate().skip(1) {
        let contains = s.start_line <= line && line <= s.end_line
            && !(s.start_line == line); // a def does not enclose itself
        if contains && (s.start_line > idx.symbols[best].start_line || best == 0) {
            best = i;
        }
    }
    best
}

/// Count named parameter children, excluding self/cls receivers.
fn count_params(params: Node, source: &str) -> u32 {
    let mut n = 0;
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        let text = child.utf8_text(source.as_bytes()).unwrap_or("");
        if text == "self" || text == "cls" {
            continue;
        }
        n += 1;
    }
    n
}
