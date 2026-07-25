//! Data model types.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Module,
    Class,
    Struct,
    Enum,
    Trait,
    Interface,
    Function,
    Method,
    Type,
    Const,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Class => "class",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Interface => "interface",
            Self::Function => "function",
            Self::Method => "method",
            Self::Type => "type",
            Self::Const => "const",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    Calls,
    Imports,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Calls => "calls",
            Self::Imports => "imports",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Exact,
    NameArity,
    NameOnly,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::NameArity => "name_arity",
            Self::NameOnly => "name_only",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewSymbol {
    pub name: String,
    pub qualname: String,
    pub kind: SymbolKind,
    pub sig: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    /// Index into the same FileIndex's `symbols` Vec.
    pub parent: Option<usize>,
    pub arity: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct NewEdge {
    /// Index into the same FileIndex's `symbols` Vec.
    pub src: usize,
    pub kind: EdgeKind,
    pub dst_name: String,
    pub dst_arity: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct NewChunk {
    /// Index into the same FileIndex's `symbols` Vec.
    pub symbol: Option<usize>,
    pub start_line: u32,
    pub end_line: u32,
    pub content: String,
}

#[derive(Debug, Clone, Default)]
pub struct FileIndex {
    pub symbols: Vec<NewSymbol>,
    pub edges: Vec<NewEdge>,
    pub chunks: Vec<NewChunk>,
}

pub fn estimate_tokens(text: &str) -> u32 {
    (text.chars().count() / 4) as u32
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Counts {
    pub files: i64,
    pub symbols: i64,
    pub edges: i64,
    pub chunks: i64,
}
