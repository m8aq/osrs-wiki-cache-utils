mod cache;
mod code;
mod extract;
mod index;
mod mcp;
mod model;
mod snapshot;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use index::{
    SearchEngine, build_cache_index, build_code_index, build_index, verify_cache_index,
    verify_code_index, verify_index,
};
use mcp::OfflineWiki;
use rmcp::{ServiceExt, transport::stdio};
use snapshot::{build_snapshot, verify_snapshot};

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
    CodeIndex {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        pluginhub_repo: PathBuf,
        #[arg(long)]
        pluginhub_commit: String,
        #[arg(long)]
        runelite_repo: PathBuf,
        #[arg(long)]
        runelite_commit: String,
        #[arg(long)]
        tooling_repo: PathBuf,
        #[arg(long)]
        tooling_commit: String,
        #[arg(long)]
        http_api_repo: PathBuf,
        #[arg(long)]
        http_api_commit: String,
    },
    Serve {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        cache_database: Option<PathBuf>,
        #[arg(long)]
        code_database: Option<PathBuf>,
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
    CodeSearch {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        code_database: PathBuf,
        query: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        #[arg(long)]
        kind: Option<String>,
    },
    CodeGet {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        code_database: PathBuf,
        kind: String,
        id: String,
    },
    Verify {
        #[arg(long)]
        snapshot: Option<PathBuf>,
        #[arg(long)]
        database: Option<PathBuf>,
        #[arg(long)]
        cache_database: Option<PathBuf>,
        #[arg(long)]
        code_database: Option<PathBuf>,
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
            let metadata =
                build_snapshot(&output, requests_per_second, concurrency, &titles).await?;
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
        Command::CodeIndex {
            database,
            pluginhub_repo,
            pluginhub_commit,
            runelite_repo,
            runelite_commit,
            tooling_repo,
            tooling_commit,
            http_api_repo,
            http_api_commit,
        } => build_code_index(
            &database,
            &pluginhub_repo,
            &pluginhub_commit,
            &runelite_repo,
            &runelite_commit,
            &tooling_repo,
            &tooling_commit,
            &http_api_repo,
            &http_api_commit,
        )?,
        Command::Serve {
            database,
            cache_database,
            code_database,
        } => {
            let server = OfflineWiki::new(SearchEngine::open(
                &database,
                cache_database.as_deref(),
                code_database.as_deref(),
            )?);
            server.serve(stdio()).await?.waiting().await?;
        }
        Command::Search {
            database,
            query,
            limit,
        } => {
            let output = SearchEngine::open(&database, None, None)?.search(&query, limit)?;
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        Command::CacheSearch {
            database,
            cache_database,
            query,
            limit,
            kind,
        } => {
            let output = SearchEngine::open(&database, Some(&cache_database), None)?.search_cache(
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
            let output = SearchEngine::open(&database, Some(&cache_database), None)?
                .cache_entry(&kind, &id)?;
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        Command::CodeSearch {
            database,
            code_database,
            query,
            limit,
            kind,
        } => {
            let output = SearchEngine::open(&database, None, Some(&code_database))?.search_code(
                &query,
                limit,
                kind.as_deref(),
            )?;
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        Command::CodeGet {
            database,
            code_database,
            kind,
            id,
        } => {
            let output = SearchEngine::open(&database, None, Some(&code_database))?
                .code_entry(&kind, &id)?;
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        Command::Verify {
            snapshot,
            database,
            cache_database,
            code_database,
        } => {
            let metadata = snapshot.as_deref().map(verify_snapshot).transpose()?;
            if let Some(database) = database {
                let snapshot = snapshot
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("--snapshot is required with --database"))?;
                verify_index(&database, snapshot)?;
            }
            if let Some(database) = cache_database {
                verify_cache_index(&database)?;
            }
            if let Some(database) = code_database {
                verify_code_index(&database)?;
            }
            if let Some(metadata) = metadata {
                eprintln!("verified snapshot {}", metadata.snapshot_date);
            }
        }
    }
    Ok(())
}
