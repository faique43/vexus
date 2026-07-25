use vexus_core::model::{estimate_tokens, FileIndex, NewChunk, NewSymbol, SymbolKind};

const MAX_TOKENS: u32 = 512;

/// Lines (1-based) whose trimmed text marks doc/comment/decorator/attribute content.
fn is_annotation_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#')
        || t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with('*')
        || t.starts_with('@')
}

/// True if `ancestor` is a strict ancestor of `of` by walking `parent` links.
fn is_ancestor(ancestor: usize, of: usize, symbols: &[NewSymbol]) -> bool {
    let mut cur = symbols[of].parent;
    while let Some(p) = cur {
        if p == ancestor {
            return true;
        }
        cur = symbols[p].parent;
    }
    false
}

/// Extend `start` upward over contiguous annotation lines whose innermost owner (if any)
/// is an ancestor of `sym_index` — i.e. lines only "covered" by a containing symbol (or
/// uncovered) may still be claimed; lines owned by an unrelated symbol may not.
fn extend_start(
    sym_index: usize,
    start: u32,
    owner: &[Option<usize>],
    lines: &[&str],
    symbols: &[NewSymbol],
) -> u32 {
    let mut s = start;
    while s > 1 {
        let prev = (s - 2) as usize; // 0-based index of line s-1
        if prev >= owner.len() || prev >= lines.len() {
            break;
        }
        if lines[prev].trim().is_empty() || !is_annotation_line(lines[prev]) {
            break;
        }
        if let Some(o) = owner[prev] {
            if o != sym_index && !is_ancestor(o, sym_index, symbols) {
                break;
            }
        }
        s -= 1;
    }
    s
}

fn cap_content(content: String) -> String {
    if estimate_tokens(&content) <= MAX_TOKENS {
        return content;
    }
    let mut out = String::new();
    for line in content.lines() {
        if estimate_tokens(&out) + estimate_tokens(line) > MAX_TOKENS - 4 {
            if out.is_empty() {
                // A single line alone busts the budget: hard-truncate it at the char
                // budget (char-boundary safe via `chars().take()`) rather than dropping
                // it entirely and emitting only the truncation marker.
                let budget_chars = (MAX_TOKENS as usize) * 4;
                let truncated: String = line.chars().take(budget_chars).collect();
                out.push_str(&truncated);
                out.push('\n');
            }
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("… (truncated)\n");
    out
}

pub fn build_chunks(idx: &mut FileIndex, source: &str) {
    let lines: Vec<&str> = source.lines().collect();
    let slice = |start: u32, end: u32| -> String {
        let s = (start.max(1) - 1) as usize;
        let e = (end as usize).min(lines.len());
        if s >= e {
            return String::new();
        }
        let mut out = lines[s..e].join("\n");
        out.push('\n');
        out
    };

    let mut chunks = Vec::new();

    // 1. Build `covered` bitmap from non-module symbol ranges (used for the preamble),
    //    and an `owner` map recording the innermost (most specific) symbol covering each
    //    line (used for doc-extension ancestor checks). `owner` is filled largest-range
    //    first so nested (smaller) ranges overwrite their containers for shared lines.
    let mut covered = vec![false; lines.len()];
    let mut owner: Vec<Option<usize>> = vec![None; lines.len()];
    let mut by_size: Vec<usize> = (1..idx.symbols.len()).collect();
    by_size.sort_by_key(|&i| {
        let s = &idx.symbols[i];
        std::cmp::Reverse(s.end_line.saturating_sub(s.start_line))
    });
    for &i in &by_size {
        let s = &idx.symbols[i];
        let start = (s.start_line as usize - 1).min(lines.len());
        let end = (s.end_line as usize).min(lines.len());
        if start < end {
            covered[start..end].fill(true);
            for slot in owner[start..end].iter_mut() {
                *slot = Some(i);
            }
        }
    }

    // 2. Doc extension for ALL leaf/container symbols first, so `covered`/`owner` are
    //    final before the preamble is computed. Extended lines are claimed by the
    //    extending symbol (marked covered, owner set to that symbol).
    let mut ext_starts: Vec<u32> = vec![0; idx.symbols.len()];
    for (i, sym) in idx.symbols.iter().enumerate().skip(1) {
        if !matches!(
            sym.kind,
            SymbolKind::Function
                | SymbolKind::Method
                | SymbolKind::Const
                | SymbolKind::Type
                | SymbolKind::Class
                | SymbolKind::Struct
                | SymbolKind::Enum
                | SymbolKind::Trait
                | SymbolKind::Interface
        ) {
            continue;
        }
        let ext_start = extend_start(i, sym.start_line, &owner, &lines, &idx.symbols);
        ext_starts[i] = ext_start;
        let start = (ext_start as usize - 1).min(covered.len());
        let end = ((sym.start_line as usize).saturating_sub(1)).min(covered.len());
        if start < end {
            covered[start..end].fill(true);
            for slot in owner[start..end].iter_mut() {
                *slot = Some(i);
            }
        }
    }

    // 3. Module preamble: one chunk per contiguous run of uncovered, non-blank lines.
    let mut run_start: Option<usize> = None;
    for i in 0..=lines.len() {
        let uncovered_nonblank = i < lines.len() && !covered[i] && !lines[i].trim().is_empty();
        if uncovered_nonblank {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else if let Some(rs) = run_start.take() {
            let run_text: String = lines[rs..i].iter().map(|l| format!("{l}\n")).collect();
            // Punctuation-only runs (a lone `}`, `);`, etc. left over between
            // symbols) carry no searchable content and just pollute the
            // index — skip them rather than emitting a chunk for them.
            if run_text.chars().any(|c| c.is_alphanumeric()) {
                chunks.push(NewChunk {
                    symbol: Some(0),
                    start_line: (rs + 1) as u32,
                    end_line: i as u32,
                    content: cap_content(run_text),
                });
            }
        }
    }

    // 4. Symbol chunks (leaf bodies / container skeletons), using the doc-extended start.
    for (i, sym) in idx.symbols.iter().enumerate().skip(1) {
        match sym.kind {
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Const | SymbolKind::Type => {
                let ext_start = ext_starts[i];
                let content = slice(ext_start, sym.end_line);
                if estimate_tokens(&content) <= MAX_TOKENS {
                    chunks.push(NewChunk {
                        symbol: Some(i),
                        start_line: ext_start,
                        end_line: sym.end_line,
                        content,
                    });
                } else {
                    split_oversized(
                        &mut chunks,
                        i,
                        sym.sig.as_deref(),
                        ext_start,
                        sym.end_line,
                        &lines,
                    );
                }
            }
            SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Trait
            | SymbolKind::Interface => {
                let ext_start = ext_starts[i];
                // Skeleton: doc lines (if extended) + own sig + direct children sigs.
                let mut content = String::new();
                if ext_start < sym.start_line {
                    content.push_str(&slice(ext_start, sym.start_line - 1));
                }
                if let Some(sig) = &sym.sig {
                    content.push_str(sig);
                    content.push('\n');
                }
                for child in idx.symbols.iter().filter(|c| c.parent == Some(i)) {
                    if let Some(sig) = &child.sig {
                        content.push_str("    ");
                        content.push_str(sig);
                        content.push('\n');
                    }
                }
                if !content.trim().is_empty() {
                    chunks.push(NewChunk {
                        symbol: Some(i),
                        start_line: ext_start,
                        end_line: sym.end_line,
                        content: cap_content(content),
                    });
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
    end_line: u32,
    lines: &[&str],
) {
    let end = end_line.min(lines.len() as u32);
    let mut piece_start = start_line;
    let mut buf = String::new();
    let mut first = true;
    let flush = |chunks: &mut Vec<NewChunk>, buf: &mut String, from: u32, to: u32, first: bool| {
        if buf.trim().is_empty() {
            return;
        }
        let content = match sig {
            Some(s) if !first => format!("{}\n{}", s, std::mem::take(buf)),
            _ => std::mem::take(buf),
        };
        chunks.push(NewChunk {
            symbol: Some(sym_index),
            start_line: from,
            end_line: to,
            content,
        });
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
        NewSymbol {
            name: name.into(),
            qualname: format!("m.{name}"),
            kind,
            sig: Some(format!("sig-of-{name}")),
            start_line: start,
            end_line: end,
            parent,
            arity: None,
        }
    }

    #[test]
    fn leaf_container_preamble() {
        let source = "import x\n\nclass C:\n    def a(self):\n        pass\n    def b(self):\n        pass\n";
        //            line1      2  3           4               5       6               7
        let mut idx = FileIndex {
            symbols: vec![
                NewSymbol {
                    name: "m".into(),
                    qualname: "m".into(),
                    kind: SymbolKind::Module,
                    sig: None,
                    start_line: 1,
                    end_line: 7,
                    parent: None,
                    arity: None,
                },
                f("C", SymbolKind::Class, 3, 7, Some(0)),
                f("a", SymbolKind::Method, 4, 5, Some(1)),
                f("b", SymbolKind::Method, 6, 7, Some(1)),
            ],
            edges: vec![],
            chunks: vec![],
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
    fn doc_comments_extend_leaf_and_container_chunks() {
        let source = "\
import x

# helper docs line 3
# more docs line 4
def helped():
    pass

@decorator
def decorated():
    pass
";
        let mut idx = FileIndex {
            symbols: vec![
                NewSymbol {
                    name: "m".into(),
                    qualname: "m".into(),
                    kind: SymbolKind::Module,
                    sig: None,
                    start_line: 1,
                    end_line: 10,
                    parent: None,
                    arity: None,
                },
                f("helped", SymbolKind::Function, 5, 6, Some(0)),
                f("decorated", SymbolKind::Function, 9, 10, Some(0)),
            ],
            edges: vec![],
            chunks: vec![],
        };
        crate::chunk::build_chunks(&mut idx, source);

        let helped = idx.chunks.iter().find(|c| c.symbol == Some(1)).unwrap();
        assert_eq!(helped.start_line, 3); // extended over both comment lines
        assert!(helped.content.starts_with("# helper docs line 3"));

        let dec = idx.chunks.iter().find(|c| c.symbol == Some(2)).unwrap();
        assert_eq!(dec.start_line, 8); // extended over @decorator
        assert!(dec.content.starts_with("@decorator"));

        // preamble must NOT contain the claimed doc lines
        let pre: Vec<_> = idx.chunks.iter().filter(|c| c.symbol == Some(0)).collect();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0].content, "import x\n");
        assert_eq!((pre[0].start_line, pre[0].end_line), (1, 1));
    }

    #[test]
    fn nested_method_doc_comment_joins_method_chunk() {
        let source = "class C:\n    # doc for a\n    def a(self):\n        pass\n";
        let mut idx = FileIndex {
            symbols: vec![
                NewSymbol {
                    name: "m".into(),
                    qualname: "m".into(),
                    kind: SymbolKind::Module,
                    sig: None,
                    start_line: 1,
                    end_line: 4,
                    parent: None,
                    arity: None,
                },
                f("C", SymbolKind::Class, 1, 4, Some(0)),
                NewSymbol {
                    name: "a".into(),
                    qualname: "m.C.a".into(),
                    kind: SymbolKind::Method,
                    sig: Some("def a(self):".into()),
                    start_line: 3,
                    end_line: 4,
                    parent: Some(1),
                    arity: Some(0),
                },
            ],
            edges: vec![],
            chunks: vec![],
        };
        crate::chunk::build_chunks(&mut idx, source);
        let a = idx.chunks.iter().find(|c| c.symbol == Some(2)).unwrap();
        assert_eq!(a.start_line, 2);
        assert!(a.content.starts_with("    # doc for a"));
    }

    #[test]
    fn punctuation_only_preamble_run_is_skipped() {
        // A lone `}` line between two functions (e.g. leftover from an
        // enclosing brace that isn't itself tracked as a symbol) must not
        // become its own chunk — it has no alphanumeric content to search on.
        let source = "fn a() {\n    pass\n}\n\n}\n\nfn b() {\n    pass\n}\n";
        //             1              2      3  4  5  6              7      8      9
        let mut idx = FileIndex {
            symbols: vec![
                NewSymbol {
                    name: "m".into(),
                    qualname: "m".into(),
                    kind: SymbolKind::Module,
                    sig: None,
                    start_line: 1,
                    end_line: 9,
                    parent: None,
                    arity: None,
                },
                f("a", SymbolKind::Function, 1, 3, Some(0)),
                f("b", SymbolKind::Function, 7, 9, Some(0)),
            ],
            edges: vec![],
            chunks: vec![],
        };
        crate::chunk::build_chunks(&mut idx, source);

        let pre: Vec<_> = idx.chunks.iter().filter(|c| c.symbol == Some(0)).collect();
        assert!(
            pre.is_empty(),
            "lone '}}' preamble line must not produce a chunk: {pre:?}"
        );
    }

    #[test]
    fn preamble_split_into_contiguous_runs_with_true_ranges() {
        let source =
            "import a\n\ndef f1():\n    pass\n\nCONST = 1\nOTHER = 2\n\ndef f2():\n    pass\n";
        //            1         2  3          4        5  6          7          8  9          10
        let mut idx = FileIndex {
            symbols: vec![
                NewSymbol {
                    name: "m".into(),
                    qualname: "m".into(),
                    kind: SymbolKind::Module,
                    sig: None,
                    start_line: 1,
                    end_line: 10,
                    parent: None,
                    arity: None,
                },
                f("f1", SymbolKind::Function, 3, 4, Some(0)),
                f("f2", SymbolKind::Function, 9, 10, Some(0)),
            ],
            edges: vec![],
            chunks: vec![],
        };
        crate::chunk::build_chunks(&mut idx, source);

        let pre: Vec<_> = idx.chunks.iter().filter(|c| c.symbol == Some(0)).collect();
        assert_eq!(pre.len(), 2);
        assert_eq!((pre[0].start_line, pre[0].end_line), (1, 1));
        assert_eq!(pre[0].content, "import a\n");
        assert_eq!((pre[1].start_line, pre[1].end_line), (6, 7));
        assert_eq!(pre[1].content, "CONST = 1\nOTHER = 2\n");
    }

    #[test]
    fn container_and_preamble_chunks_are_capped() {
        // 600 one-line consts ≈ >512 tokens of preamble
        let source: String = (0..600).map(|i| format!("CONST_{i:04} = {i}\n")).collect();
        let mut idx = FileIndex {
            symbols: vec![NewSymbol {
                name: "m".into(),
                qualname: "m".into(),
                kind: SymbolKind::Module,
                sig: None,
                start_line: 1,
                end_line: 600,
                parent: None,
                arity: None,
            }],
            edges: vec![],
            chunks: vec![],
        };
        crate::chunk::build_chunks(&mut idx, &source);
        for c in &idx.chunks {
            assert!(vexus_core::model::estimate_tokens(&c.content) <= 512 + 8);
        }
        assert!(idx
            .chunks
            .iter()
            .any(|c| c.content.ends_with("… (truncated)\n")));
    }

    #[test]
    fn cap_content_hard_truncates_a_single_oversized_line() {
        // A single 3000-char line alone busts the 512-token budget; it must be
        // hard-truncated at the char budget rather than dropped entirely.
        let line: String = "x".repeat(3000);
        let content = format!("{line}\n");

        let out = super::cap_content(content);

        assert!(out.ends_with("… (truncated)\n"));
        let budget_chars = 512 * 4;
        let expected_prefix: String = line.chars().take(budget_chars).collect();
        assert!(out.starts_with(&expected_prefix));
        assert!(vexus_core::model::estimate_tokens(&out) <= 512 + 8);
    }

    #[test]
    fn oversized_leaf_splits_with_sig_prefix() {
        // 300 lines * ~12 chars = ~900 tokens -> 2+ pieces
        let body: String = (0..300).map(|i| format!("    line_{i:04};\n")).collect();
        let source = format!("fn big() {{\n{body}}}\n");
        let total_lines = source.lines().count() as u32;
        let mut idx = FileIndex {
            symbols: vec![
                NewSymbol {
                    name: "m".into(),
                    qualname: "m".into(),
                    kind: SymbolKind::Module,
                    sig: None,
                    start_line: 1,
                    end_line: total_lines,
                    parent: None,
                    arity: None,
                },
                NewSymbol {
                    name: "big".into(),
                    qualname: "m.big".into(),
                    kind: SymbolKind::Function,
                    sig: Some("fn big() {".into()),
                    start_line: 1,
                    end_line: total_lines,
                    parent: Some(0),
                    arity: Some(0),
                },
            ],
            edges: vec![],
            chunks: vec![],
        };
        crate::chunk::build_chunks(&mut idx, &source);
        let pieces: Vec<_> = idx.chunks.iter().filter(|c| c.symbol == Some(1)).collect();
        assert!(pieces.len() >= 2);
        for p in &pieces {
            assert!(vexus_core::model::estimate_tokens(&p.content) <= 512 + 16);
            // sig prefix slack
        }
        for p in &pieces[1..] {
            assert!(p.content.starts_with("fn big() {"));
        }
    }

    #[test]
    fn oversized_followed_by_function_respects_end_line() {
        // Oversized function followed by another function
        let big_body: String = (0..300)
            .map(|i| format!("    big_line_{i:04};\n"))
            .collect();
        let source = format!("fn big() {{\n{big_body}}}\n\ndef small():\n    pass\n");
        let total_lines = source.lines().count() as u32;
        let big_end = big_body.lines().count() as u32 + 2; // +2 for fn big() { and }
        let small_start = big_end + 2;
        let small_end = total_lines;

        let mut idx = FileIndex {
            symbols: vec![
                NewSymbol {
                    name: "m".into(),
                    qualname: "m".into(),
                    kind: SymbolKind::Module,
                    sig: None,
                    start_line: 1,
                    end_line: total_lines,
                    parent: None,
                    arity: None,
                },
                NewSymbol {
                    name: "big".into(),
                    qualname: "m.big".into(),
                    kind: SymbolKind::Function,
                    sig: Some("fn big() {".into()),
                    start_line: 1,
                    end_line: big_end,
                    parent: Some(0),
                    arity: Some(0),
                },
                f(
                    "small",
                    SymbolKind::Function,
                    small_start,
                    small_end,
                    Some(0),
                ),
            ],
            edges: vec![],
            chunks: vec![],
        };
        crate::chunk::build_chunks(&mut idx, &source);

        // Verify oversized chunks don't exceed their symbol's end_line
        let big_chunks: Vec<_> = idx.chunks.iter().filter(|c| c.symbol == Some(1)).collect();
        assert!(!big_chunks.is_empty());
        for chunk in &big_chunks {
            assert!(
                chunk.end_line <= big_end,
                "oversized chunk end_line {} exceeds symbol end_line {}",
                chunk.end_line,
                big_end
            );
            assert!(
                !chunk.content.contains("small"),
                "oversized chunk contains next function"
            );
            assert!(
                !chunk.content.contains("pass"),
                "oversized chunk contains small's body"
            );
        }

        // Verify second function has its own chunk
        let small_chunks: Vec<_> = idx.chunks.iter().filter(|c| c.symbol == Some(2)).collect();
        assert_eq!(small_chunks.len(), 1);
        assert!(small_chunks[0].content.contains("pass"));
    }
}
