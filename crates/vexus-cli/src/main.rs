mod pipeline;

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "vexus",
    version,
    about = "Local code intelligence for coding agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build or update the index for a repo
    Index { path: Option<PathBuf> },
    /// Show index freshness and counts
    Status { path: Option<PathBuf> },
    /// Keyword search over indexed chunks
    Search {
        query: String,
        path: Option<PathBuf>,
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
}

fn db_path(root: &Path) -> PathBuf {
    root.join(".vexus/index.db")
}

fn open_store(root: &Path) -> Result<vexus_core::Store> {
    let store = vexus_core::Store::open(&db_path(root))?;
    // self-ignoring dir, like target/
    std::fs::write(root.join(".vexus/.gitignore"), "*\n")?;
    Ok(store)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Index { path } => {
            let root = path.unwrap_or_else(|| PathBuf::from("."));
            let mut store = open_store(&root)?;
            let r = pipeline::index_repo(&root, &mut store)?;
            println!(
                "indexed: {}  unchanged: {}  skipped: {}  removed: {}  failed: {}",
                r.indexed,
                r.skipped_unchanged,
                r.skipped_unsupported,
                r.removed,
                r.failed.len()
            );
            for f in &r.failed {
                eprintln!("failed: {f}");
            }
        }
        Cmd::Status { path } => {
            let root = path.unwrap_or_else(|| PathBuf::from("."));
            if !db_path(&root).exists() {
                println!("no index — run: vexus index");
                return Ok(());
            }
            let store = vexus_core::Store::open(&db_path(&root))?;
            let c = store.counts()?;
            println!(
                "files: {}  symbols: {}  edges: {}  chunks: {}",
                c.files, c.symbols, c.edges, c.chunks
            );
        }
        Cmd::Search { query, path, limit } => {
            let root = path.unwrap_or_else(|| PathBuf::from("."));
            if !db_path(&root).exists() {
                println!("no index — run: vexus index");
                return Ok(());
            }
            let store = vexus_core::Store::open(&db_path(&root))?;
            for h in store.search_keyword(&query, limit)? {
                let qual = h.qualname.unwrap_or_else(|| "(preamble)".into());
                println!(
                    "{}  {}:{}-{}  {:.2}\n    {}",
                    qual, h.path, h.start_line, h.end_line, h.score, h.excerpt
                );
            }
        }
    }
    Ok(())
}
