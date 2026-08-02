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

/// Corpus-size tier retrieval defaults scale with. On a corpus of a few
/// dozen files the Medium constants return most of the repo for every
/// query (the project's own token benchmark measured explore at 0.2×–0.4×
/// grep's cost there); the smaller tiers shrink candidate pools, entry
/// limits and budgets to match what the corpus can actually distinguish.
/// `Medium` values are byte-identical to the historical constants, so
/// behavior on real-sized repos is unchanged by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorpusTier {
    /// < 200 chunks — a handful of files; grep territory.
    Tiny,
    /// 200–1,999 chunks — small project.
    Small,
    /// ≥ 2,000 chunks — everything the defaults were originally tuned on.
    Medium,
}

impl CorpusTier {
    pub fn from_chunks(chunks: i64) -> Self {
        match chunks {
            i64::MIN..=199 => CorpusTier::Tiny,
            200..=1999 => CorpusTier::Small,
            _ => CorpusTier::Medium,
        }
    }
}
