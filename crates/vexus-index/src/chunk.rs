use vexus_core::model::{estimate_tokens, FileIndex, NewChunk, SymbolKind};

const MAX_TOKENS: u32 = 512;

pub fn build_chunks(idx: &mut FileIndex, source: &str) {
    let lines: Vec<&str> = source.lines().collect();
    let slice = |start: u32, end: u32| -> String {
        let s = (start.max(1) - 1) as usize;
        let e = (end as usize).min(lines.len());
        if s >= e { return String::new() }
        let mut out = lines[s..e].join("\n");
        out.push('\n');
        out
    };

    let mut chunks = Vec::new();

    // 1. Module preamble: lines not covered by any non-module symbol.
    let mut covered = vec![false; lines.len()];
    for s in idx.symbols.iter().skip(1) {
        for l in (s.start_line as usize - 1)..(s.end_line as usize).min(lines.len()) {
            covered[l] = true;
        }
    }
    let preamble: String = lines.iter().enumerate()
        .filter(|(i, _)| !covered[*i])
        .map(|(_, l)| format!("{l}\n"))
        .collect();
    if !preamble.trim().is_empty() {
        chunks.push(NewChunk { symbol: Some(0), start_line: 1,
            end_line: lines.len() as u32, content: preamble });
    }

    for (i, sym) in idx.symbols.iter().enumerate().skip(1) {
        match sym.kind {
            SymbolKind::Function | SymbolKind::Method => {
                let content = slice(sym.start_line, sym.end_line);
                if estimate_tokens(&content) <= MAX_TOKENS {
                    chunks.push(NewChunk { symbol: Some(i), start_line: sym.start_line,
                        end_line: sym.end_line, content });
                } else {
                    split_oversized(&mut chunks, i, sym.sig.as_deref(), sym.start_line, &lines);
                }
            }
            SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum
            | SymbolKind::Trait | SymbolKind::Interface => {
                // Skeleton: own sig + direct children sigs.
                let mut content = String::new();
                if let Some(sig) = &sym.sig { content.push_str(sig); content.push('\n'); }
                for child in idx.symbols.iter().filter(|c| c.parent == Some(i)) {
                    if let Some(sig) = &child.sig {
                        content.push_str("    ");
                        content.push_str(sig);
                        content.push('\n');
                    }
                }
                if !content.trim().is_empty() {
                    chunks.push(NewChunk { symbol: Some(i), start_line: sym.start_line,
                        end_line: sym.end_line, content });
                }
            }
            _ => {}
        }
    }
    idx.chunks = chunks;
}

fn split_oversized(
    chunks: &mut Vec<NewChunk>,
    sym_index: usize,
    sig: Option<&str>,
    start_line: u32,
    lines: &[&str],
) {
    let end = lines.len() as u32;
    let mut piece_start = start_line;
    let mut buf = String::new();
    let mut first = true;
    let flush = |chunks: &mut Vec<NewChunk>, buf: &mut String, from: u32, to: u32, first: bool| {
        if buf.trim().is_empty() { return }
        let content = if first || sig.is_none() {
            std::mem::take(buf)
        } else {
            format!("{}\n{}", sig.unwrap(), std::mem::take(buf))
        };
        chunks.push(NewChunk { symbol: Some(sym_index), start_line: from, end_line: to, content });
    };
    for ln in start_line..=end {
        let line = lines[(ln - 1) as usize];
        if estimate_tokens(&buf) + estimate_tokens(line) > MAX_TOKENS && !buf.is_empty() {
            flush(chunks, &mut buf, piece_start, ln - 1, first);
            first = false;
            piece_start = ln;
        }
        buf.push_str(line);
        buf.push('\n');
    }
    flush(chunks, &mut buf, piece_start, end, first);
}

#[cfg(test)]
mod tests {
    use vexus_core::model::*;

    fn f(name: &str, kind: SymbolKind, start: u32, end: u32, parent: Option<usize>) -> NewSymbol {
        NewSymbol { name: name.into(), qualname: format!("m.{name}"), kind,
            sig: Some(format!("sig-of-{name}")), start_line: start, end_line: end,
            parent, arity: None }
    }

    #[test]
    fn leaf_container_preamble() {
        let source = "import x\n\nclass C:\n    def a(self):\n        pass\n    def b(self):\n        pass\n";
        //            line1      2  3           4               5       6               7
        let mut idx = FileIndex {
            symbols: vec![
                NewSymbol { name: "m".into(), qualname: "m".into(), kind: SymbolKind::Module,
                    sig: None, start_line: 1, end_line: 7, parent: None, arity: None },
                f("C", SymbolKind::Class, 3, 7, Some(0)),
                f("a", SymbolKind::Method, 4, 5, Some(1)),
                f("b", SymbolKind::Method, 6, 7, Some(1)),
            ],
            edges: vec![], chunks: vec![],
        };
        crate::chunk::build_chunks(&mut idx, source);

        // preamble (module), skeleton (class), two leaf bodies
        assert_eq!(idx.chunks.len(), 4);
        let pre = &idx.chunks[0];
        assert_eq!(pre.symbol, Some(0));
        assert!(pre.content.contains("import x"));
        assert!(!pre.content.contains("class C"));

        let skel = idx.chunks.iter().find(|c| c.symbol == Some(1)).unwrap();
        assert!(skel.content.contains("sig-of-C"));
        assert!(skel.content.contains("sig-of-a"));
        assert!(!skel.content.contains("pass")); // no bodies in skeleton

        let a = idx.chunks.iter().find(|c| c.symbol == Some(2)).unwrap();
        assert_eq!(a.content, "    def a(self):\n        pass\n");
    }

    #[test]
    fn oversized_leaf_splits_with_sig_prefix() {
        // 300 lines * ~12 chars = ~900 tokens -> 2+ pieces
        let body: String = (0..300).map(|i| format!("    line_{i:04};\n")).collect();
        let source = format!("fn big() {{\n{body}}}\n");
        let total_lines = source.lines().count() as u32;
        let mut idx = FileIndex {
            symbols: vec![
                NewSymbol { name: "m".into(), qualname: "m".into(), kind: SymbolKind::Module,
                    sig: None, start_line: 1, end_line: total_lines, parent: None, arity: None },
                NewSymbol { name: "big".into(), qualname: "m.big".into(), kind: SymbolKind::Function,
                    sig: Some("fn big() {".into()), start_line: 1, end_line: total_lines,
                    parent: Some(0), arity: Some(0) },
            ],
            edges: vec![], chunks: vec![],
        };
        crate::chunk::build_chunks(&mut idx, &source);
        let pieces: Vec<_> = idx.chunks.iter().filter(|c| c.symbol == Some(1)).collect();
        assert!(pieces.len() >= 2);
        for p in &pieces {
            assert!(vexus_core::model::estimate_tokens(&p.content) <= 512 + 16); // sig prefix slack
        }
        for p in &pieces[1..] {
            assert!(p.content.starts_with("fn big() {"));
        }
    }
}
