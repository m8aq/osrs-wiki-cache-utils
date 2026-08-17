mod cache;
mod extract;
mod index;
mod mcp;
mod model;
mod snapshot;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use index::{IndexOptions, SearchEngine, build_index, index_has_cache, verify_index};
use mcp::OfflineWiki;
use rmcp::{ServiceExt, transport::stdio};
use snapshot::{SnapshotOptions, build_snapshot, package_release, verify_snapshot};

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
        #[arg(long, requires = "shard_count")]
        shard_index: Option<usize>,
        #[arg(long, requires = "shard_index")]
        shard_count: Option<usize>,
    },
    Index {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        model_cache: PathBuf,
        #[arg(long, requires = "cache_commit")]
        cache_dump: Option<PathBuf>,
        #[arg(long, requires = "cache_dump")]
        cache_commit: Option<String>,
    },
    Serve {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        model_cache: PathBuf,
    },
    Search {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        model_cache: PathBuf,
        query: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
    CacheSearch {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        model_cache: PathBuf,
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
        model_cache: PathBuf,
        kind: String,
        id: String,
    },
    Verify {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        database: Option<PathBuf>,
    },
    Release {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        model_cache: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}

/// Runs the snapshot, indexing, search, release, or MCP command selected on the CLI.
pub async fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Snapshot {
            output,
            requests_per_second,
            concurrency,
            titles,
            shard_index,
            shard_count,
        } => {
            let metadata = build_snapshot(SnapshotOptions {
                output,
                requests_per_second,
                concurrency,
                titles,
                shard_index,
                shard_count,
            })
            .await?;
            eprintln!(
                "snapshot complete: {} included, {} excluded",
                metadata.included_pages, metadata.excluded_pages
            );
        }
        Command::Index {
            snapshot,
            database,
            model_cache,
            cache_dump,
            cache_commit,
        } => build_index(IndexOptions {
            snapshot,
            database,
            model_cache,
            cache_dump,
            cache_commit,
        })?,
        Command::Serve {
            database,
            model_cache,
        } => {
            let server = OfflineWiki::new(SearchEngine::open(&database, &model_cache)?);
            server.serve(stdio()).await?.waiting().await?;
        }
        Command::Search {
            database,
            model_cache,
            query,
            limit,
        } => {
            let output = SearchEngine::open(&database, &model_cache)?.search(&query, limit, 0)?;
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        Command::CacheSearch {
            database,
            model_cache,
            query,
            limit,
            kind,
        } => {
            let output = SearchEngine::open(&database, &model_cache)?.search_cache(
                &query,
                limit,
                0,
                kind.as_deref(),
            )?;
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        Command::CacheGet {
            database,
            model_cache,
            kind,
            id,
        } => {
            let output = SearchEngine::open(&database, &model_cache)?.cache_entry(&kind, &id)?;
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        Command::Verify { snapshot, database } => {
            let metadata = verify_snapshot(&snapshot)?;
            if let Some(database) = database {
                verify_index(&database)?;
            }
            eprintln!("verified snapshot {}", metadata.snapshot_date);
        }
        Command::Release {
            snapshot,
            database,
            model_cache,
            output,
        } => {
            verify_index(&database)?;
            if index_has_cache(&database)? {
                anyhow::bail!(
                    "cache-enhanced indexes cannot be included in the CC-licensed Wiki release"
                );
            }
            for artifact in package_release(&snapshot, &database, &model_cache, &output)? {
                eprintln!("created {}", artifact.display());
            }
        }
    }
    Ok(())
}
