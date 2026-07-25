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
];

pub fn for_path(path: &Path) -> Option<&'static Lang> {
    let ext = path.extension()?.to_str()?;
    LANGS.iter().find(|l| l.extensions.contains(&ext))
}

pub fn all() -> &'static [Lang] {
    LANGS
}
