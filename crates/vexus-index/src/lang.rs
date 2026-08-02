use std::path::Path;

use tree_sitter::{Language, Query};

pub struct Lang {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    grammar: fn() -> Language,
    symbols_scm: &'static str,
    edges_scm: &'static str,
}

impl Lang {
    pub fn grammar(&self) -> Language {
        (self.grammar)()
    }
    pub fn symbols_query(&self) -> Query {
        Query::new(&self.grammar(), self.symbols_scm).expect("valid symbols query")
    }
    pub fn edges_query(&self) -> Query {
        Query::new(&self.grammar(), self.edges_scm).expect("valid edges query")
    }
}

static LANGS: &[Lang] = &[
    Lang {
        name: "python",
        extensions: &["py"],
        grammar: || tree_sitter_python::LANGUAGE.into(),
        symbols_scm: include_str!("../queries/python.scm"),
        edges_scm: include_str!("../queries/python_edges.scm"),
    },
    Lang {
        name: "typescript",
        extensions: &["ts", "tsx"],
        grammar: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        symbols_scm: include_str!("../queries/typescript.scm"),
        edges_scm: include_str!("../queries/typescript_edges.scm"),
    },
    Lang {
        name: "rust",
        extensions: &["rs"],
        grammar: || tree_sitter_rust::LANGUAGE.into(),
        symbols_scm: include_str!("../queries/rust.scm"),
        edges_scm: include_str!("../queries/rust_edges.scm"),
    },
    // Plain JS gets its own grammar rather than riding LANGUAGE_TSX: the TS
    // grammar diverges on a few legacy JS constructs and Flow-annotated
    // files, and JSX needs the JS grammar's own JSX support anyway.
    Lang {
        name: "javascript",
        extensions: &["js", "jsx", "mjs", "cjs"],
        grammar: || tree_sitter_javascript::LANGUAGE.into(),
        symbols_scm: include_str!("../queries/javascript.scm"),
        edges_scm: include_str!("../queries/javascript_edges.scm"),
    },
    Lang {
        name: "go",
        extensions: &["go"],
        grammar: || tree_sitter_go::LANGUAGE.into(),
        symbols_scm: include_str!("../queries/go.scm"),
        edges_scm: include_str!("../queries/go_edges.scm"),
    },
    Lang {
        name: "java",
        extensions: &["java"],
        grammar: || tree_sitter_java::LANGUAGE.into(),
        symbols_scm: include_str!("../queries/java.scm"),
        edges_scm: include_str!("../queries/java_edges.scm"),
    },
    // `.h` maps to C for now; if that misparses a C++-heavy codebase's
    // headers, the graceful degradation path (module-only symbol) applies.
    Lang {
        name: "c",
        extensions: &["c", "h"],
        grammar: || tree_sitter_c::LANGUAGE.into(),
        symbols_scm: include_str!("../queries/c.scm"),
        edges_scm: include_str!("../queries/c_edges.scm"),
    },
    Lang {
        name: "cpp",
        extensions: &["cpp", "cc", "cxx", "hpp", "hh"],
        grammar: || tree_sitter_cpp::LANGUAGE.into(),
        symbols_scm: include_str!("../queries/cpp.scm"),
        edges_scm: include_str!("../queries/cpp_edges.scm"),
    },
    Lang {
        name: "c_sharp",
        extensions: &["cs"],
        grammar: || tree_sitter_c_sharp::LANGUAGE.into(),
        symbols_scm: include_str!("../queries/c_sharp.scm"),
        edges_scm: include_str!("../queries/c_sharp_edges.scm"),
    },
    Lang {
        name: "kotlin",
        extensions: &["kt", "kts"],
        grammar: || tree_sitter_kotlin_ng::LANGUAGE.into(),
        symbols_scm: include_str!("../queries/kotlin.scm"),
        edges_scm: include_str!("../queries/kotlin_edges.scm"),
    },
    Lang {
        name: "swift",
        extensions: &["swift"],
        grammar: || tree_sitter_swift::LANGUAGE.into(),
        symbols_scm: include_str!("../queries/swift.scm"),
        edges_scm: include_str!("../queries/swift_edges.scm"),
    },
    Lang {
        name: "ruby",
        extensions: &["rb"],
        grammar: || tree_sitter_ruby::LANGUAGE.into(),
        symbols_scm: include_str!("../queries/ruby.scm"),
        edges_scm: include_str!("../queries/ruby_edges.scm"),
    },
    // LANGUAGE_PHP (not _ONLY): real-world .php files open with <?php and
    // can interleave HTML.
    Lang {
        name: "php",
        extensions: &["php"],
        grammar: || tree_sitter_php::LANGUAGE_PHP.into(),
        symbols_scm: include_str!("../queries/php.scm"),
        edges_scm: include_str!("../queries/php_edges.scm"),
    },
    Lang {
        name: "scala",
        extensions: &["scala", "sc"],
        grammar: || tree_sitter_scala::LANGUAGE.into(),
        symbols_scm: include_str!("../queries/scala.scm"),
        edges_scm: include_str!("../queries/scala_edges.scm"),
    },
    Lang {
        name: "elixir",
        extensions: &["ex", "exs"],
        grammar: || tree_sitter_elixir::LANGUAGE.into(),
        symbols_scm: include_str!("../queries/elixir.scm"),
        edges_scm: include_str!("../queries/elixir_edges.scm"),
    },
];

pub fn for_path(path: &Path) -> Option<&'static Lang> {
    let ext = path.extension()?.to_str()?;
    LANGS.iter().find(|l| l.extensions.contains(&ext))
}

pub fn all() -> &'static [Lang] {
    LANGS
}
