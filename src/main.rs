mod cache;
mod extract;
mod index;
mod mcp;
mod model;
mod snapshot;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use index::{SearchEngine, build_cache_index, build_index, verify_cache_index, verify_index};
use mcp::OfflineWiki;
use rmcp::{ServiceExt, transport::stdio};
use snapshot::{SnapshotOptions, build_snapshot, verify_snapshot};

pub(crate) const WIKI_ORIGIN: &str = "https://oldschool.runescape.wiki";
pub(crate) const CONTENT_LICENSE: &str = "CC BY-NC-SA 3.0";
pub(crate) const CONTENT_LICENSE_URL: &str = "https://creativecommons.org/licenses/by-nc-sa/3.0/";

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Snapshot {
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 2.0)]
        requests_per_second: f64,
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        #[arg(long = "title")]
        titles: Vec<String>,
    },
    Index {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        database: PathBuf,
    },
    CacheIndex {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        cache_dump: PathBuf,
        #[arg(long)]
        cache_commit: String,
    },
    Serve {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        cache_database: Option<PathBuf>,
    },
    Search {
        #[arg(long)]
        database: PathBuf,
        query: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
    CacheSearch {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        cache_database: PathBuf,
        query: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        #[arg(long)]
        kind: Option<String>,
    },
    CacheGet {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        cache_database: PathBuf,
        kind: String,
        id: String,
    },
    Verify {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        database: Option<PathBuf>,
        #[arg(long)]
        cache_database: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Snapshot {
            output,
            requests_per_second,
            concurrency,
            titles,
        } => {
            let metadata = build_snapshot(SnapshotOptions {
                output,
                requests_per_second,
                concurrency,
                titles,
            })
            .await?;
            eprintln!(
                "snapshot complete: {} included, {} excluded",
                metadata.included_pages, metadata.excluded_pages
            );
        }
        Command::Index { snapshot, database } => build_index(&snapshot, &database)?,
        Command::CacheIndex {
            database,
            cache_dump,
            cache_commit,
        } => build_cache_index(&database, &cache_dump, &cache_commit)?,
        Command::Serve {
            database,
            cache_database,
        } => {
            let server =
                OfflineWiki::new(SearchEngine::open(&database, cache_database.as_deref())?);
            server.serve(stdio()).await?.waiting().await?;
        }
        Command::Search {
            database,
            query,
            limit,
        } => {
            let output = SearchEngine::open(&database, None)?.search(&query, limit)?;
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        Command::CacheSearch {
            database,
            cache_database,
            query,
            limit,
            kind,
        } => {
            let output = SearchEngine::open(&database, Some(&cache_database))?.search_cache(
                &query,
                limit,
                kind.as_deref(),
            )?;
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        Command::CacheGet {
            database,
            cache_database,
            kind,
            id,
        } => {
            let output =
                SearchEngine::open(&database, Some(&cache_database))?.cache_entry(&kind, &id)?;
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        Command::Verify {
            snapshot,
            database,
            cache_database,
        } => {
            let metadata = verify_snapshot(&snapshot)?;
            if let Some(database) = database {
                verify_index(&database, &snapshot)?;
            }
            if let Some(database) = cache_database {
                verify_cache_index(&database)?;
            }
            eprintln!("verified snapshot {}", metadata.snapshot_date);
        }
    }
    Ok(())
}
