use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use schemars::JsonSchema;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    CONTENT_LICENSE, CONTENT_LICENSE_URL, WIKI_ORIGIN,
    cache::{CACHE_ORIGIN, CacheDocument, CacheMetadata, CacheSnapshot, read_cache_dump},
    extract::{chunks_for_section, extract_page},
    model::{AliasManifest, PageManifest, SnapshotMetadata},
    snapshot::{read_json_lines, verify_snapshot},
};

const TEXT_CAP: usize = 16_000;
const SECTION_CAP: usize = 200;
const CANDIDATE_LIMIT: usize = 50;
const RRF_K: f32 = 60.0;
const CACHE_BATCH_SIZE: usize = 1_000;
const SQLITE_CACHE_KIB: i64 = 32 * 1024;
const LEXICAL_SQL: &str = "SELECT chunks_fts.rowid FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.rowid JOIN pages p ON p.id = c.page_id LEFT JOIN cache_entries ce ON ce.page_id = p.id WHERE chunks_fts MATCH ?1 AND (?2 IS NULL OR p.source_kind = ?2) AND (?3 IS NULL OR ce.kind = ?3 COLLATE NOCASE) ORDER BY bm25(chunks_fts, 10.0, 4.0, 1.0) LIMIT ?4";
const RECOVERY_WARNING: &str =
    "Derived retrieval text; lossless Parsoid HTML is retained in the Wiki database.";

pub struct IndexOptions {
    pub snapshot: PathBuf,
    pub database: PathBuf,
}

/// Inputs for building the standalone decoded game-cache search database.
pub struct CacheIndexOptions {
    pub database: PathBuf,
    pub cache_dump: PathBuf,
    pub cache_commit: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceRef {
    pub kind: String,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<u64>,
    pub revision_url: String,
    pub fetched_at: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Provenance {
    pub sources: Vec<SourceRef>,
    pub attribution: String,
    pub license: String,
    pub license_url: String,
    pub transformed: bool,
    pub snapshot_date: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchResultRow {
    pub source: String,
    pub title: String,
    pub page_id: i64,
    pub snippet: String,
    pub url: String,
    pub section: String,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchOutput {
    pub results: Vec<SearchResultRow>,
    pub total: usize,
    pub offset: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
/// One ranked result set spanning every source present in the index.
pub struct UnifiedSearchOutput {
    pub results: Vec<SearchResultRow>,
    pub total: usize,
    pub offset: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    pub provenance: Vec<Provenance>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SectionSummary {
    pub index: i64,
    pub name: String,
    pub level: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PageOutput {
    pub title: String,
    pub content: String,
    pub total_characters: usize,
    pub truncated: bool,
    pub sections: Vec<SectionSummary>,
    pub warnings: Vec<String>,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SectionsOutput {
    pub title: String,
    pub sections: Vec<SectionSummary>,
    pub total: usize,
    pub returned: usize,
    pub truncated: bool,
    pub warnings: Vec<String>,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SectionOutput {
    pub title: String,
    pub section: i64,
    pub content: String,
    pub total_characters: usize,
    pub truncated: bool,
    pub warnings: Vec<String>,
    pub provenance: Provenance,
}

/// One bounded decoded game-cache record with pinned source provenance.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntryOutput {
    pub kind: String,
    pub id: String,
    pub symbol: String,
    pub title: String,
    pub content: String,
    pub total_characters: usize,
    pub truncated: bool,
    pub path: String,
    pub commit: String,
    pub url: String,
    pub warnings: Vec<String>,
    pub provenance: Provenance,
}

struct PageRow {
    id: i64,
    source_kind: String,
    title: String,
    revision_id: u64,
    revision_url: String,
    fetched_at: String,
    url: String,
}

pub struct SearchEngine {
    connection: Connection,
    cache_connection: Option<Connection>,
    snapshot_date: String,
    cache: Option<CacheMetadata>,
}

pub fn build_index(options: IndexOptions) -> Result<()> {
    eprintln!("index phase: verify snapshot");
    let metadata = verify_snapshot(&options.snapshot)?;
    let pages: Vec<PageManifest> = read_json_lines(&options.snapshot.join("manifest.jsonl"))?;
    let aliases: Vec<AliasManifest> = read_json_lines(&options.snapshot.join("aliases.jsonl"))?;
    if options.database.exists() && current_schema(&options.database)? {
        update_index(
            &options.snapshot,
            &options.database,
            &metadata,
            &pages,
            &aliases,
        )
    } else {
        build_new_index(
            &options.snapshot,
            &options.database,
            &metadata,
            &pages,
            &aliases,
        )
    }
}

fn build_new_index(
    snapshot: &Path,
    database: &Path,
    metadata: &SnapshotMetadata,
    pages: &[PageManifest],
    aliases: &[AliasManifest],
) -> Result<()> {
    let raw_bytes = pages.iter().try_fold(0_u64, |total, page| {
        Ok::<_, anyhow::Error>(total + fs::metadata(snapshot.join(&page.path))?.len())
    })?;
    require_free_space(database, raw_bytes + 512 * 1024 * 1024)?;
    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = database.with_extension("sqlite.part");
    let mut connection = Connection::open(&temporary)?;
    if current_schema(&temporary)? {
        let stored: String =
            connection.query_row("SELECT value FROM meta WHERE key = 'snapshot'", [], |row| {
                row.get(0)
            })?;
        if stored != serde_json::to_string(metadata)? {
            bail!(
                "partial index belongs to another snapshot; remove {} to rebuild",
                temporary.display()
            );
        }
    } else {
        drop(connection);
        let _ = fs::remove_file(&temporary);
        connection = Connection::open(&temporary)?;
        create_schema(&connection)?;
        connection.execute(
            "INSERT INTO meta(key, value) VALUES ('snapshot', ?1)",
            [serde_json::to_string(metadata)?],
        )?;
    }
    let existing = indexed_page_ids(&connection, "wiki")?;
    eprintln!("wiki index: {}/{}", existing.len(), pages.len());
    for (index, page) in pages.iter().enumerate() {
        if !existing.contains(&page.page_id) {
            let transaction = connection.transaction()?;
            insert_page(&transaction, snapshot, page)?;
            transaction.commit()?;
        }
        let completed = index + 1;
        if completed % 250 == 0 || completed == pages.len() {
            eprintln!("wiki index: {completed}/{}", pages.len());
        }
    }
    {
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM aliases", [])?;
        insert_aliases(&transaction, aliases)?;
        transaction.commit()?;
    }
    verify_page_count(&connection, "wiki", pages.len())?;
    eprintln!("index phase: optimize database");
    connection.execute_batch("PRAGMA optimize;")?;
    drop(connection);
    fs::rename(&temporary, database)?;
    eprintln!("index complete: {} Wiki pages", pages.len());
    Ok(())
}

fn update_index(
    snapshot: &Path,
    database: &Path,
    metadata: &SnapshotMetadata,
    pages: &[PageManifest],
    aliases: &[AliasManifest],
) -> Result<()> {
    let mut connection = Connection::open(database)?;
    let existing = {
        let mut statement = connection
            .prepare("SELECT id, revision_id, touched_at, content_sha256, title FROM pages WHERE source_kind = 'wiki'")?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    (
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ),
                ))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?
    };
    let current_ids = pages
        .iter()
        .map(|page| page.page_id)
        .collect::<HashSet<_>>();
    let changed = pages
        .iter()
        .filter(|page| {
            existing.get(&page.page_id).is_none_or(|old| {
                old.0 != i64::try_from(page.revision_id).unwrap_or(i64::MAX)
                    || old.1 != page.touched_at
                    || old.2 != page.sha256
                    || old.3 != page.title
            })
        })
        .collect::<Vec<_>>();
    let removed = existing
        .keys()
        .filter(|page_id| !current_ids.contains(page_id))
        .copied()
        .collect::<Vec<_>>();

    for page_id in &removed {
        let transaction = connection.transaction()?;
        delete_page(&transaction, *page_id)?;
        transaction.commit()?;
    }
    eprintln!("wiki index update: 0/{}", changed.len());
    for (index, page) in changed.iter().enumerate() {
        let transaction = connection.transaction()?;
        delete_page(&transaction, page.page_id)?;
        insert_page(&transaction, snapshot, page)?;
        transaction.commit()?;
        let completed = index + 1;
        if completed % 250 == 0 || completed == changed.len() {
            eprintln!("wiki index update: {completed}/{}", changed.len());
        }
    }
    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM aliases", [])?;
    insert_aliases(&transaction, aliases)?;
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES ('snapshot', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [serde_json::to_string(metadata)?],
    )?;
    transaction.commit()?;
    connection.execute_batch("PRAGMA optimize;")?;

    verify_page_count(&connection, "wiki", pages.len())?;
    eprintln!(
        "index update: {} changed, {} removed, {} unchanged",
        changed.len(),
        removed.len(),
        pages.len() - changed.len()
    );
    Ok(())
}

/// Builds or incrementally updates the standalone decoded game-cache index.
pub fn build_cache_index(options: CacheIndexOptions) -> Result<()> {
    let cache = read_cache_dump(&options.cache_dump, &options.cache_commit)?;
    if options.database.exists() && current_schema(&options.database)? {
        let mut connection = Connection::open(&options.database)?;
        let transaction = connection.transaction()?;
        let counts = update_cache(&transaction, &cache)?;
        transaction.commit()?;
        connection.execute_batch("PRAGMA optimize;")?;
        verify_page_count(&connection, "cache", cache.documents.len())?;
        eprintln!(
            "cache update: {} changed, {} removed, {} unchanged",
            counts.0, counts.1, counts.2
        );
        return Ok(());
    }

    if let Some(parent) = options.database.parent() {
        fs::create_dir_all(parent)?;
    }
    let required = cache
        .documents
        .iter()
        .map(|document| document.content.len() as u64)
        .sum::<u64>()
        .saturating_mul(2);
    require_free_space(&options.database, required)?;
    let temporary = options.database.with_extension("sqlite.part");
    let mut connection = Connection::open(&temporary)?;
    if current_schema(&temporary)? {
        let stored: CacheMetadata = serde_json::from_str(&connection.query_row(
            "SELECT value FROM meta WHERE key = 'cache'",
            [],
            |row| row.get::<_, String>(0),
        )?)?;
        if stored.commit != cache.metadata.commit {
            bail!(
                "partial cache index belongs to commit {}; remove {} to rebuild",
                stored.commit,
                temporary.display()
            );
        }
    } else {
        drop(connection);
        let _ = fs::remove_file(&temporary);
        connection = Connection::open(&temporary)?;
        create_schema(&connection)?;
        connection.execute(
            "INSERT INTO meta(key, value) VALUES ('cache', ?1)",
            [serde_json::to_string(&cache.metadata)?],
        )?;
    }
    let existing = indexed_cache_keys(&connection)?;
    eprintln!("cache index: {}/{}", existing.len(), cache.documents.len());
    for (batch_index, batch) in cache.documents.chunks(CACHE_BATCH_SIZE).enumerate() {
        let transaction = connection.transaction()?;
        for (offset, document) in batch.iter().enumerate() {
            if existing.contains(&(document.kind.clone(), document.id.clone())) {
                continue;
            }
            let index = batch_index * CACHE_BATCH_SIZE + offset;
            insert_cache_document(&transaction, &cache, document, -1 - index as i64)?;
        }
        transaction.commit()?;
        eprintln!(
            "cache index: {}/{}",
            ((batch_index + 1) * CACHE_BATCH_SIZE).min(cache.documents.len()),
            cache.documents.len()
        );
    }
    verify_page_count(&connection, "cache", cache.documents.len())?;
    connection.execute_batch("PRAGMA optimize;")?;
    drop(connection);
    fs::rename(&temporary, &options.database)?;
    eprintln!("cache index complete: {} entries", cache.documents.len());
    Ok(())
}

fn insert_page(transaction: &Transaction<'_>, snapshot: &Path, page: &PageManifest) -> Result<()> {
    let html = fs::read_to_string(snapshot.join(&page.path))?;
    let extracted = extract_page(&html)?;
    let raw_content = zstd::stream::encode_all(html.as_bytes(), 9)?;
    transaction.execute(
        "INSERT INTO pages(id, source_kind, namespace, title, revision_id, revision_url, modified_at, touched_at, fetched_at, url, categories_json, content_sha256, raw_content_zstd) VALUES (?1, 'wiki', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            page.page_id,
            page.namespace,
            page.title,
            i64::try_from(page.revision_id).context("revision ID exceeds SQLite integer")?,
            page.revision_url,
            page.modified_at,
            page.touched_at,
            page.fetched_at,
            page_url(&page.title),
            serde_json::to_string(&page.categories)?,
            page.sha256,
            raw_content,
        ],
    )?;
    for section in extracted.sections {
        let content = section.blocks.join("\n\n");
        insert_section(
            transaction,
            page.page_id,
            section.index,
            section.level,
            &page.title,
            &section.heading,
            &content,
            &section.blocks,
        )?;
    }
    Ok(())
}

fn update_cache(
    transaction: &Transaction<'_>,
    cache: &CacheSnapshot,
) -> Result<(usize, usize, usize)> {
    let existing = {
        let mut statement = transaction.prepare(
            "SELECT ce.kind, ce.entry_id, ce.page_id, p.content_sha256, p.title FROM cache_entries ce JOIN pages p ON p.id = ce.page_id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                    (
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ),
                ))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?
    };
    let current = cache
        .documents
        .iter()
        .map(|document| ((document.kind.clone(), document.id.clone()), document))
        .collect::<HashMap<_, _>>();
    let changed = current
        .iter()
        .filter(|(key, document)| {
            existing
                .get(*key)
                .is_none_or(|old| old.1 != document.sha256 || old.2 != document.title)
        })
        .collect::<Vec<_>>();
    let removed = existing
        .iter()
        .filter(|(key, _)| !current.contains_key(*key))
        .collect::<Vec<_>>();

    for (_, old) in &removed {
        delete_page(transaction, old.0)?;
    }
    for (key, _) in &changed {
        if let Some(old) = existing.get(*key) {
            delete_page(transaction, old.0)?;
        }
    }
    let mut next_id: i64 = transaction.query_row("SELECT min(id) - 1 FROM pages", [], |row| {
        Ok(row.get::<_, Option<i64>>(0)?.unwrap_or(-1).min(-1))
    })?;
    for (key, document) in &changed {
        let page_id = existing.get(*key).map(|old| old.0).unwrap_or_else(|| {
            let id = next_id;
            next_id -= 1;
            id
        });
        insert_cache_document(transaction, cache, document, page_id)?;
    }

    let commit_url = cache_commit_url(&cache.metadata.commit);
    for (key, document) in &current {
        let Some(old) = existing.get(key) else {
            continue;
        };
        if old.1 != document.sha256 || old.2 != document.title {
            continue;
        }
        let page_id = existing[key].0;
        let url = cache_entry_url(&cache.metadata.commit, &document.path);
        transaction.execute(
            "UPDATE pages SET revision_url = ?1, modified_at = ?2, fetched_at = ?3, url = ?4 WHERE id = ?5",
            params![commit_url, cache.metadata.committed_at, cache.metadata.indexed_at, url, page_id],
        )?;
        transaction.execute(
            "UPDATE cache_entries SET symbol = ?1, path = ?2, commit_sha = ?3 WHERE page_id = ?4",
            params![
                document.symbol,
                document.path,
                cache.metadata.commit,
                page_id
            ],
        )?;
    }
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES ('cache', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [serde_json::to_string(&cache.metadata)?],
    )?;
    Ok((
        changed.len(),
        removed.len(),
        cache.documents.len() - changed.len(),
    ))
}

fn insert_cache_document(
    transaction: &Transaction<'_>,
    cache: &CacheSnapshot,
    document: &CacheDocument,
    page_id: i64,
) -> Result<()> {
    let commit_url = cache_commit_url(&cache.metadata.commit);
    let url = cache_entry_url(&cache.metadata.commit, &document.path);
    let raw_content = zstd::stream::encode_all(document.content.as_bytes(), 9)?;
    transaction.execute(
        "INSERT INTO pages(id, source_kind, namespace, title, revision_id, revision_url, modified_at, touched_at, fetched_at, url, categories_json, content_sha256, raw_content_zstd) VALUES (?1, 'cache', -1, ?2, 0, ?3, ?4, NULL, ?5, ?6, '[]', ?7, ?8)",
        params![
            page_id,
            document.title,
            commit_url,
            cache.metadata.committed_at,
            cache.metadata.indexed_at,
            url,
            document.sha256,
            raw_content,
        ],
    )?;
    transaction.execute(
        "INSERT INTO cache_entries(page_id, kind, entry_id, symbol, path, commit_sha) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![page_id, document.kind, document.id, document.symbol, document.path, cache.metadata.commit],
    )?;
    transaction.execute(
        "INSERT INTO sections(page_id, section_index, level, heading, content) VALUES (?1, 0, 1, ?2, ?3)",
        params![page_id, document.kind, document.content],
    )?;
    let section_id = transaction.last_insert_rowid();
    let blocks = document
        .content
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    for (ordinal, text) in chunks_for_section(&document.title, &document.kind, &blocks)
        .into_iter()
        .enumerate()
    {
        transaction.execute(
            "INSERT INTO chunks(page_id, section_id, ordinal, title, heading, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![page_id, section_id, ordinal as i64, document.title, document.kind, text],
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_section(
    transaction: &Transaction<'_>,
    page_id: i64,
    section_index: i64,
    level: usize,
    title: &str,
    heading: &str,
    content: &str,
    blocks: &[String],
) -> Result<()> {
    transaction.execute(
        "INSERT INTO sections(page_id, section_index, level, heading, content) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![page_id, section_index, level as i64, heading, content],
    )?;
    let section_id = transaction.last_insert_rowid();
    let texts = chunks_for_section(title, heading, blocks);
    for (ordinal, text) in texts.into_iter().enumerate() {
        transaction.execute(
            "INSERT INTO chunks(page_id, section_id, ordinal, title, heading, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![page_id, section_id, ordinal as i64, title, heading, text],
        )?;
    }
    Ok(())
}

fn delete_page(transaction: &Transaction<'_>, page_id: i64) -> Result<()> {
    transaction.execute("DELETE FROM cache_entries WHERE page_id = ?1", [page_id])?;
    transaction.execute("DELETE FROM chunks WHERE page_id = ?1", [page_id])?;
    transaction.execute("DELETE FROM sections WHERE page_id = ?1", [page_id])?;
    transaction.execute("DELETE FROM pages WHERE id = ?1", [page_id])?;
    Ok(())
}

fn cache_commit_url(commit: &str) -> String {
    format!("{CACHE_ORIGIN}/commit/{commit}")
}

fn cache_entry_url(commit: &str, path: &str) -> String {
    let path = path
        .split('/')
        .map(|segment| utf8_percent_encode(segment, NON_ALPHANUMERIC).to_string())
        .collect::<Vec<_>>()
        .join("/");
    format!("{CACHE_ORIGIN}/blob/{commit}/{path}")
}

fn insert_aliases(transaction: &Transaction<'_>, aliases: &[AliasManifest]) -> Result<()> {
    eprintln!("alias index: 0/{}", aliases.len());
    for (index, alias) in aliases.iter().enumerate() {
        transaction.execute(
            "INSERT OR IGNORE INTO aliases(alias, page_id) SELECT ?1, id FROM pages WHERE title = ?2 COLLATE NOCASE ORDER BY title = ?2 DESC LIMIT 1",
            params![alias.alias, alias.target],
        )?;
        let completed = index + 1;
        if completed % 10_000 == 0 || completed == aliases.len() {
            eprintln!("alias index: {completed}/{}", aliases.len());
        }
    }
    Ok(())
}

fn current_schema(database: &Path) -> Result<bool> {
    let connection = Connection::open(database)?;
    let required: i64 = connection.query_row(
        "SELECT count(*) FROM pragma_table_info('pages') WHERE name IN ('touched_at', 'source_kind', 'content_sha256', 'raw_content_zstd')",
        [],
        |row| row.get(0),
    )?;
    let redundant: i64 = connection.query_row(
        "SELECT (SELECT count(*) FROM pragma_table_info('pages') WHERE name = 'wiki_page_id') + (SELECT count(*) FROM pragma_table_info('sections') WHERE name = 'parsoid_section_id')",
        [],
        |row| row.get(0),
    )?;
    let cache_entries: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'cache_entries'",
        [],
        |row| row.get(0),
    )?;
    let unique_page_indexes: i64 = connection.query_row(
        "SELECT count(*) FROM pragma_index_list('pages') WHERE [unique] = 1",
        [],
        |row| row.get(0),
    )?;
    let embedding_columns: i64 = connection.query_row(
        "SELECT count(*) FROM pragma_table_info('chunks') WHERE name = 'embedding'",
        [],
        |row| row.get(0),
    )?;
    Ok(required == 4
        && redundant == 0
        && cache_entries == 1
        && unique_page_indexes == 0
        && embedding_columns == 0)
}

fn indexed_page_ids(connection: &Connection, source_kind: &str) -> Result<HashSet<i64>> {
    let mut statement = connection.prepare("SELECT id FROM pages WHERE source_kind = ?1")?;
    Ok(statement
        .query_map([source_kind], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn indexed_cache_keys(connection: &Connection) -> Result<HashSet<(String, String)>> {
    let mut statement = connection.prepare("SELECT kind, entry_id FROM cache_entries")?;
    Ok(statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

fn verify_page_count(connection: &Connection, source_kind: &str, expected: usize) -> Result<()> {
    let indexed: i64 = connection.query_row(
        "SELECT count(*) FROM pages WHERE source_kind = ?1",
        [source_kind],
        |row| row.get(0),
    )?;
    if indexed != expected as i64 {
        bail!("{source_kind} page count {indexed} does not match expected count {expected}");
    }
    Ok(())
}

fn require_free_space(path: &Path, required: u64) -> Result<()> {
    let existing = path.ancestors().find(|path| path.exists()).unwrap_or(path);
    let output = Command::new("df")
        .args(["-Pk"])
        .arg(existing)
        .env("LC_ALL", "C")
        .output();
    let Some(available) = output
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| parse_df_available(&String::from_utf8_lossy(&output.stdout)))
    else {
        return Ok(());
    };
    if available < required {
        bail!(
            "insufficient disk space for index build: {:.1} GiB available, {:.1} GiB required",
            available as f64 / 1024_f64.powi(3),
            required as f64 / 1024_f64.powi(3)
        );
    }
    Ok(())
}

fn parse_df_available(output: &str) -> Option<u64> {
    output
        .lines()
        .last()?
        .split_whitespace()
        .nth(3)?
        .parse::<u64>()
        .ok()
        .map(|kilobytes| kilobytes * 1024)
}

fn validate_search(query: &str, limit: usize) -> Result<()> {
    if query.trim().is_empty() || limit == 0 || limit > 20 {
        bail!("query must be non-empty and limit must be between 1 and 20");
    }
    Ok(())
}

impl SearchEngine {
    pub fn open(database: &Path, cache_database: Option<&Path>) -> Result<Self> {
        let connection = Connection::open_with_flags(
            database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.pragma_update(None, "cache_size", -SQLITE_CACHE_KIB)?;
        connection.pragma_update(None, "mmap_size", 0)?;
        let metadata: String =
            connection.query_row("SELECT value FROM meta WHERE key = 'snapshot'", [], |row| {
                row.get(0)
            })?;
        let metadata: SnapshotMetadata = serde_json::from_str(&metadata)?;
        let cache_connection = cache_database
            .map(|database| {
                let connection = Connection::open_with_flags(
                    database,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                connection.pragma_update(None, "cache_size", -SQLITE_CACHE_KIB)?;
                connection.pragma_update(None, "mmap_size", 0)?;
                Ok::<_, anyhow::Error>(connection)
            })
            .transpose()?;
        let cache = cache_connection
            .as_ref()
            .map(|connection| -> Result<String> {
                Ok(connection.query_row(
                    "SELECT value FROM meta WHERE key = 'cache'",
                    [],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .transpose()?
            .map(|value| serde_json::from_str(&value))
            .transpose()?;
        Ok(Self {
            connection,
            cache_connection,
            snapshot_date: metadata.snapshot_date,
            cache,
        })
    }

    pub fn search(&self, query: &str, limit: usize, offset: usize) -> Result<SearchOutput> {
        self.search_source(query, limit, offset, "wiki", None)
    }

    /// Searches decoded, revision-pinned game-cache records, optionally by kind.
    pub fn search_cache(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
        kind: Option<&str>,
    ) -> Result<SearchOutput> {
        if self.cache.is_none() {
            bail!("cache data is not configured");
        }
        let kind = kind.map(str::trim);
        if kind == Some("") {
            bail!("cache kind must be non-empty");
        }
        self.search_source(query, limit, offset, "cache", kind)
    }

    /// Searches all indexed Wiki and game-cache content in one ranking.
    pub fn search_unified(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<UnifiedSearchOutput> {
        validate_search(query, limit)?;
        let wiki = self.rank_rows(&self.connection, query, "wiki", None)?;
        let cache = self
            .cache_connection
            .as_ref()
            .map(|connection| self.rank_rows(connection, query, "cache", None))
            .transpose()?
            .unwrap_or_default();
        let mut rows = wiki.into_iter().chain(cache).collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.source.cmp(&right.source))
                .then_with(|| left.title.cmp(&right.title))
        });
        let total = rows.len();
        let results = rows
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();
        let next_offset = (offset + results.len() < total).then_some(offset + results.len());
        let (wiki_rows, cache_rows): (Vec<_>, Vec<_>) =
            results.iter().partition(|row| row.source == "wiki");
        let wiki_sources = wiki_rows
            .into_iter()
            .map(|row| {
                self.page_row_by_id(&self.connection, row.page_id)
                    .map(|page| source(&page))
            })
            .collect::<Result<Vec<_>>>()?;
        let cache_sources = cache_rows
            .into_iter()
            .map(|row| {
                self.page_row_by_id(
                    self.cache_connection
                        .as_ref()
                        .expect("cache result needs database"),
                    row.page_id,
                )
                .map(|page| source(&page))
            })
            .collect::<Result<Vec<_>>>()?;
        let provenance = [
            (!wiki_sources.is_empty()).then(|| self.provenance("wiki", wiki_sources)),
            (!cache_sources.is_empty()).then(|| self.provenance("cache", cache_sources)),
        ]
        .into_iter()
        .flatten()
        .collect();
        Ok(UnifiedSearchOutput {
            results,
            total,
            offset,
            next_offset,
            provenance,
        })
    }

    fn search_source(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
        source_kind: &str,
        cache_kind: Option<&str>,
    ) -> Result<SearchOutput> {
        validate_search(query, limit)?;
        let connection = if source_kind == "wiki" {
            &self.connection
        } else {
            self.cache_connection
                .as_ref()
                .ok_or_else(|| anyhow!("cache data is not configured"))?
        };
        let rows = self.rank_rows(connection, query, source_kind, cache_kind)?;
        let total = rows.len();
        let results = rows
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();
        let next_offset = (offset + results.len() < total).then_some(offset + results.len());
        let sources = results
            .iter()
            .map(|row| {
                self.page_row_by_id(connection, row.page_id)
                    .map(|page| source(&page))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(SearchOutput {
            results,
            total,
            offset,
            next_offset,
            provenance: self.provenance(source_kind, sources),
        })
    }

    fn rank_rows(
        &self,
        connection: &Connection,
        query: &str,
        source_kind: &str,
        cache_kind: Option<&str>,
    ) -> Result<Vec<SearchResultRow>> {
        let query = query.trim();
        let lexical = self.lexical_candidates(connection, query, source_kind, cache_kind)?;
        let exact_pages = self
            .resolve_page_id(connection, query, source_kind, cache_kind)?
            .into_iter()
            .collect::<Vec<_>>();
        let mut scores: HashMap<i64, f32> = HashMap::new();
        for (rank, chunk_id) in lexical.iter().enumerate() {
            *scores.entry(*chunk_id).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
        }
        for page_id in exact_pages {
            let chunk_id: Option<i64> = connection
                .query_row(
                    "SELECT id FROM chunks WHERE page_id = ?1 ORDER BY id LIMIT 1",
                    [page_id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(chunk_id) = chunk_id {
                *scores.entry(chunk_id).or_default() += 1.0;
            }
        }
        let mut ranked = scores.into_iter().collect::<Vec<_>>();
        ranked.sort_by(|left, right| right.1.partial_cmp(&left.1).unwrap_or(Ordering::Equal));

        let mut seen_pages = HashSet::new();
        let mut rows = Vec::new();
        for (chunk_id, score) in ranked {
            let row = connection.query_row(
                "SELECT p.id, p.source_kind, p.title, p.url, s.heading, c.text, ce.kind, ce.entry_id, ce.symbol FROM chunks c JOIN pages p ON p.id = c.page_id JOIN sections s ON s.id = c.section_id LEFT JOIN cache_entries ce ON ce.page_id = p.id WHERE c.id = ?1",
                [chunk_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, String>(5)?, row.get::<_, Option<String>>(6)?, row.get::<_, Option<String>>(7)?, row.get::<_, Option<String>>(8)?)),
            )?;
            if seen_pages.insert(row.0) {
                rows.push(SearchResultRow {
                    page_id: row.0,
                    source: row.1,
                    title: row.2,
                    url: row.3,
                    section: row.4,
                    snippet: cap_chars(&row.5, 600).0,
                    score,
                    kind: row.6,
                    id: row.7,
                    symbol: row.8,
                });
            }
        }
        Ok(rows)
    }

    pub fn page(&self, title: &str) -> Result<PageOutput> {
        let page = self.page_row(title)?;
        let sections = self.section_rows(&self.connection, page.id)?;
        let content = sections
            .iter()
            .map(|(_, _, heading, content)| format!("## {heading}\n\n{content}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let total_characters = content.chars().count();
        let (content, truncated) = cap_chars(&content, TEXT_CAP);
        let section_total = sections.len();
        let summaries = sections
            .iter()
            .take(SECTION_CAP)
            .map(|(index, level, heading, _)| SectionSummary {
                index: *index,
                name: heading.clone(),
                level: *level,
                anchor: None,
            })
            .collect::<Vec<_>>();
        let mut warnings = vec![RECOVERY_WARNING.to_string()];
        if truncated {
            warnings.push("Page content truncated at 16,000 characters; use get_wiki_sections and get_wiki_section.".to_string());
        }
        if section_total > SECTION_CAP {
            warnings.push("Section summary truncated at 200 entries.".to_string());
        }
        Ok(PageOutput {
            title: page.title.clone(),
            content,
            total_characters,
            truncated,
            sections: summaries,
            warnings,
            provenance: self.provenance("wiki", vec![source(&page)]),
        })
    }

    pub fn sections(&self, title: &str) -> Result<SectionsOutput> {
        let page = self.page_row(title)?;
        let rows = self.section_rows(&self.connection, page.id)?;
        let total = rows.len();
        let sections = rows
            .into_iter()
            .take(SECTION_CAP)
            .map(|(index, level, name, _)| SectionSummary {
                index,
                name,
                level,
                anchor: None,
            })
            .collect::<Vec<_>>();
        let truncated = total > sections.len();
        Ok(SectionsOutput {
            title: page.title.clone(),
            returned: sections.len(),
            sections,
            total,
            truncated,
            warnings: std::iter::once(RECOVERY_WARNING.to_string())
                .chain(truncated.then(|| "Section list truncated at 200 entries.".to_string()))
                .collect(),
            provenance: self.provenance("wiki", vec![source(&page)]),
        })
    }

    pub fn section(&self, title: &str, section: i64) -> Result<SectionOutput> {
        let page = self.page_row(title)?;
        let content: String = self
            .connection
            .query_row(
                "SELECT content FROM sections WHERE page_id = ?1 AND section_index = ?2",
                params![page.id, section],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("section not found"))?;
        let total_characters = content.chars().count();
        let (content, truncated) = cap_chars(&content, TEXT_CAP);
        Ok(SectionOutput {
            title: page.title.clone(),
            section,
            content,
            total_characters,
            truncated,
            warnings: std::iter::once(RECOVERY_WARNING.to_string())
                .chain(
                    truncated
                        .then(|| "Section content truncated at 16,000 characters.".to_string()),
                )
                .collect(),
            provenance: self.provenance("wiki", vec![source(&page)]),
        })
    }

    /// Returns one exact decoded cache record by the kind and ID from cache search results.
    pub fn cache_entry(&self, kind: &str, id: &str) -> Result<CacheEntryOutput> {
        if kind.trim().is_empty() || id.trim().is_empty() {
            bail!("cache entry kind and ID must be non-empty");
        }
        let connection = self
            .cache_connection
            .as_ref()
            .ok_or_else(|| anyhow!("cache data is not configured"))?;
        let row = connection
            .query_row(
                "SELECT p.id, p.title, p.url, ce.symbol, ce.path, ce.commit_sha, s.content FROM cache_entries ce JOIN pages p ON p.id = ce.page_id JOIN sections s ON s.page_id = p.id AND s.section_index = 0 WHERE ce.kind = ?1 COLLATE NOCASE AND ce.entry_id = ?2 COLLATE NOCASE",
                params![kind.trim(), id.trim()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| anyhow!("cache entry not found"))?;
        let total_characters = row.6.chars().count();
        let (content, truncated) = cap_chars(&row.6, TEXT_CAP);
        let page = self.page_row_by_id(connection, row.0)?;
        Ok(CacheEntryOutput {
            kind: kind.trim().to_string(),
            id: id.trim().to_string(),
            symbol: row.3,
            title: row.1,
            content,
            total_characters,
            truncated,
            path: row.4,
            commit: row.5,
            url: row.2,
            warnings: truncated
                .then(|| {
                    "Cache entry truncated at 16,000 characters; use the pinned source URL for the complete record."
                        .to_string()
                })
                .into_iter()
                .collect(),
            provenance: self.provenance("cache", vec![source(&page)]),
        })
    }

    fn lexical_candidates(
        &self,
        connection: &Connection,
        query: &str,
        source_kind: &str,
        cache_kind: Option<&str>,
    ) -> Result<Vec<i64>> {
        let query = fts_query(query);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let mut statement = connection.prepare(LEXICAL_SQL)?;
        Ok(statement
            .query_map(
                params![query, source_kind, cache_kind, CANDIDATE_LIMIT as i64],
                |row| row.get(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn resolve_page_id(
        &self,
        connection: &Connection,
        title: &str,
        source_kind: &str,
        cache_kind: Option<&str>,
    ) -> Result<Option<i64>> {
        let sql = if source_kind == "wiki" {
            "SELECT id FROM (SELECT id, CASE WHEN title = ?1 THEN 0 ELSE 1 END AS priority FROM pages WHERE source_kind = 'wiki' AND title = ?1 COLLATE NOCASE UNION ALL SELECT a.page_id, 2 FROM aliases a JOIN pages p ON p.id = a.page_id WHERE p.source_kind = 'wiki' AND a.alias = ?1 COLLATE NOCASE) ORDER BY priority LIMIT 1"
        } else {
            "SELECT p.id FROM pages p JOIN cache_entries ce ON ce.page_id = p.id WHERE p.source_kind = 'cache' AND (p.title = ?1 COLLATE NOCASE OR ce.symbol = ?1 COLLATE NOCASE OR ce.entry_id = ?1 COLLATE NOCASE) AND (?2 IS NULL OR ce.kind = ?2 COLLATE NOCASE) LIMIT 1"
        };
        if source_kind == "wiki" {
            connection
                .query_row(sql, [title], |row| row.get(0))
                .optional()
                .map_err(Into::into)
        } else {
            connection
                .query_row(sql, params![title, cache_kind], |row| row.get(0))
                .optional()
                .map_err(Into::into)
        }
    }

    fn page_row(&self, title: &str) -> Result<PageRow> {
        let page_id = self
            .resolve_page_id(&self.connection, title, "wiki", None)?
            .ok_or_else(|| anyhow!("page not found"))?;
        self.page_row_by_id(&self.connection, page_id)
    }

    fn page_row_by_id(&self, connection: &Connection, page_id: i64) -> Result<PageRow> {
        connection
            .query_row(
                "SELECT id, source_kind, title, revision_id, revision_url, fetched_at, url FROM pages WHERE id = ?1",
                [page_id],
                |row| {
                    Ok(PageRow {
                        id: row.get(0)?,
                        source_kind: row.get(1)?,
                        title: row.get(2)?,
                        revision_id: u64::try_from(row.get::<_, i64>(3)?).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?,
                        revision_url: row.get(4)?,
                        fetched_at: row.get(5)?,
                        url: row.get(6)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    fn section_rows(
        &self,
        connection: &Connection,
        page_id: i64,
    ) -> Result<Vec<(i64, usize, String, String)>> {
        let mut statement = connection.prepare(
            "SELECT section_index, level, heading, content FROM sections WHERE page_id = ?1 ORDER BY id",
        )?;
        Ok(statement
            .query_map([page_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, i64>(1)? as usize,
                    row.get(2)?,
                    row.get(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn provenance(&self, source_kind: &str, sources: Vec<SourceRef>) -> Provenance {
        if source_kind == "cache" {
            let cache = self.cache.as_ref();
            Provenance {
                sources,
                attribution: "Jagex game-cache data; decoded dump repository contributors"
                    .to_string(),
                license: "No license declared by the dump repository".to_string(),
                license_url: CACHE_ORIGIN.to_string(),
                transformed: true,
                snapshot_date: cache
                    .and_then(|metadata| metadata.committed_at.get(..10))
                    .unwrap_or("unknown")
                    .to_string(),
            }
        } else {
            Provenance {
                sources,
                attribution: "Old School RuneScape Wiki contributors".to_string(),
                license: CONTENT_LICENSE.to_string(),
                license_url: CONTENT_LICENSE_URL.to_string(),
                transformed: true,
                snapshot_date: self.snapshot_date.clone(),
            }
        }
    }
}

/// Verifies the Wiki index and compares every stored page to its snapshot manifest.
pub fn verify_index(database: &Path, snapshot: &Path) -> Result<()> {
    let connection =
        Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    verify_database(&connection, "wiki")?;
    let metadata: SnapshotMetadata = serde_json::from_str(&connection.query_row(
        "SELECT value FROM meta WHERE key = 'snapshot'",
        [],
        |row| row.get::<_, String>(0),
    )?)?;
    let pages: Vec<PageManifest> = read_json_lines(&snapshot.join("manifest.jsonl"))?;
    verify_page_count(&connection, "wiki", pages.len())?;
    let expected = pages
        .into_iter()
        .map(|page| (page.page_id, page))
        .collect::<HashMap<_, _>>();
    let mut statement = connection.prepare(
        "SELECT id, title, revision_id, content_sha256, raw_content_zstd FROM pages WHERE source_kind = 'wiki' ORDER BY id",
    )?;
    let mut rows = statement.query([])?;
    let mut verified = 0;
    eprintln!("Wiki index verify: 0/{}", expected.len());
    while let Some(row) = rows.next()? {
        let page_id = row.get::<_, i64>(0)?;
        let page = expected
            .get(&page_id)
            .ok_or_else(|| anyhow!("indexed Wiki page {page_id} is absent from manifest"))?;
        let revision_id =
            u64::try_from(row.get::<_, i64>(2)?).context("indexed revision ID is negative")?;
        if row.get::<_, String>(1)? != page.title
            || revision_id != page.revision_id
            || row.get::<_, String>(3)? != page.sha256
        {
            bail!("indexed metadata mismatch for {}", page.title);
        }
        let raw = zstd::stream::decode_all(row.get_ref(4)?.as_blob()?)?;
        if format!("{:x}", Sha256::digest(&raw)) != page.sha256 {
            bail!("indexed raw HTML checksum mismatch for {}", page.title);
        }
        verified += 1;
        if verified % 1_000 == 0 || verified == expected.len() {
            eprintln!("Wiki index verify: {verified}/{}", expected.len());
        }
    }
    if metadata.included_pages != expected.len() {
        bail!("indexed snapshot metadata count does not match manifest");
    }
    Ok(())
}

/// Verifies the standalone decoded game-cache index.
pub fn verify_cache_index(database: &Path) -> Result<()> {
    let connection =
        Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    verify_database(&connection, "cache")?;
    let metadata: CacheMetadata = serde_json::from_str(&connection.query_row(
        "SELECT value FROM meta WHERE key = 'cache'",
        [],
        |row| row.get::<_, String>(0),
    )?)?;
    verify_page_count(&connection, "cache", metadata.entries)
}

fn verify_database(connection: &Connection, source_kind: &str) -> Result<()> {
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("SQLite integrity check failed: {integrity}");
    }
    let pages: i64 = connection.query_row(
        "SELECT count(*) FROM pages WHERE source_kind = ?1",
        [source_kind],
        |row| row.get(0),
    )?;
    let chunks: i64 = connection.query_row("SELECT count(*) FROM chunks", [], |row| row.get(0))?;
    if pages == 0 || chunks == 0 {
        bail!("index is empty");
    }
    let fts_chunks: i64 =
        connection.query_row("SELECT count(*) FROM chunks_fts", [], |row| row.get(0))?;
    if chunks != fts_chunks {
        bail!("FTS row count {fts_chunks} does not match chunk count {chunks}");
    }
    Ok(())
}

fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "PRAGMA journal_mode = DELETE;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE pages(
           id INTEGER PRIMARY KEY,
           source_kind TEXT NOT NULL CHECK(source_kind IN ('wiki', 'cache')),
           namespace INTEGER NOT NULL,
           title TEXT NOT NULL,
           revision_id INTEGER NOT NULL,
           revision_url TEXT NOT NULL,
           modified_at TEXT NOT NULL,
           touched_at TEXT,
           fetched_at TEXT NOT NULL,
           url TEXT NOT NULL,
           categories_json TEXT NOT NULL,
           content_sha256 TEXT NOT NULL,
           raw_content_zstd BLOB NOT NULL
         );
         CREATE TABLE sections(
           id INTEGER PRIMARY KEY,
           page_id INTEGER NOT NULL REFERENCES pages(id),
           section_index INTEGER NOT NULL,
           level INTEGER NOT NULL,
           heading TEXT NOT NULL,
           content TEXT NOT NULL,
           UNIQUE(page_id, section_index)
         );
         CREATE TABLE chunks(
           id INTEGER PRIMARY KEY,
           page_id INTEGER NOT NULL REFERENCES pages(id),
           section_id INTEGER NOT NULL REFERENCES sections(id),
           ordinal INTEGER NOT NULL,
           title TEXT NOT NULL,
           heading TEXT NOT NULL,
           text TEXT NOT NULL
         );
         CREATE TABLE aliases(alias TEXT PRIMARY KEY COLLATE NOCASE, page_id INTEGER NOT NULL REFERENCES pages(id));
         CREATE TABLE cache_entries(
           page_id INTEGER PRIMARY KEY REFERENCES pages(id),
           kind TEXT NOT NULL,
           entry_id TEXT NOT NULL,
           symbol TEXT NOT NULL,
           path TEXT NOT NULL,
           commit_sha TEXT NOT NULL,
           UNIQUE(kind, entry_id)
         );
         CREATE INDEX pages_source ON pages(source_kind);
         CREATE INDEX pages_title ON pages(title COLLATE NOCASE);
         CREATE INDEX cache_lookup ON cache_entries(kind, entry_id);
         CREATE INDEX chunks_page ON chunks(page_id);
         CREATE INDEX sections_page ON sections(page_id, section_index);
         CREATE VIRTUAL TABLE chunks_fts USING fts5(title, heading, text, content='chunks', content_rowid='id', tokenize='porter unicode61 remove_diacritics 2');
         CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
           INSERT INTO chunks_fts(rowid, title, heading, text) VALUES (new.id, new.title, new.heading, new.text);
         END;
         CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
           INSERT INTO chunks_fts(chunks_fts, rowid, title, heading, text) VALUES ('delete', old.id, old.title, old.heading, old.text);
         END;",
    )?;
    Ok(())
}

fn fts_query(query: &str) -> String {
    query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{token}\"*"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn cap_chars(value: &str, limit: usize) -> (String, bool) {
    if value.chars().count() <= limit {
        return (value.to_string(), false);
    }
    (value.chars().take(limit).collect(), true)
}

fn source(page: &PageRow) -> SourceRef {
    SourceRef {
        kind: if page.source_kind == "wiki" {
            "page".to_string()
        } else {
            "cacheEntry".to_string()
        },
        title: page.title.clone(),
        url: page.url.clone(),
        revision_id: (page.source_kind == "wiki").then_some(page.revision_id),
        revision_url: page.revision_url.clone(),
        fetched_at: page.fetched_at.clone(),
    }
}

fn page_url(title: &str) -> String {
    format!("{WIKI_ORIGIN}/w/{}", title.replace(' ', "_"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_input_is_quoted() {
        assert_eq!(fts_query("bow (charged)"), "\"bow\"* OR \"charged\"*");
    }

    #[test]
    fn fts_tracks_incremental_chunk_changes() {
        let connection = Connection::open_in_memory().unwrap();
        create_schema(&connection).unwrap();
        connection
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        connection
            .execute(
                "INSERT INTO chunks(id, page_id, section_id, ordinal, title, heading, text) VALUES (1, 1, 1, 0, 'Bow', 'Stats', 'ranged strength')",
                [],
            )
            .unwrap();
        let count: i64 = connection
            .query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'ranged'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        connection
            .execute("DELETE FROM chunks WHERE id = 1", [])
            .unwrap();
        let count: i64 = connection
            .query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'ranged'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn schema_allows_case_distinct_wiki_titles() {
        let mut connection = Connection::open_in_memory().unwrap();
        create_schema(&connection).unwrap();
        for (page_id, title) in [
            (1_i64, "Asgarnian Ale (barrel)"),
            (2, "Asgarnian ale (barrel)"),
        ] {
            connection
                .execute(
                    "INSERT INTO pages(id, source_kind, namespace, title, revision_id, revision_url, modified_at, fetched_at, url, categories_json, content_sha256, raw_content_zstd) VALUES (?1, 'wiki', 0, ?2, 0, '', '', '', '', '[]', '', X'')",
                    params![page_id, title],
                )
                .unwrap();
        }
        let count: i64 = connection
            .query_row("SELECT count(*) FROM pages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);

        let transaction = connection.transaction().unwrap();
        insert_aliases(
            &transaction,
            &[AliasManifest {
                alias: "Lowercase barrel".to_string(),
                target: "Asgarnian ale (barrel)".to_string(),
            }],
        )
        .unwrap();
        transaction.commit().unwrap();
        let target: i64 = connection
            .query_row(
                "SELECT page_id FROM aliases WHERE alias = 'Lowercase barrel'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(target, 2);
    }

    #[test]
    fn lexical_search_filters_wiki_and_cache_chunks() {
        let connection = Connection::open_in_memory().unwrap();
        create_schema(&connection).unwrap();
        for (page_id, source_kind, title) in
            [(1_i64, "wiki", "Abyssal whip"), (-1, "cache", "Cache whip")]
        {
            connection
                .execute(
                    "INSERT INTO pages(id, source_kind, namespace, title, revision_id, revision_url, modified_at, touched_at, fetched_at, url, categories_json, content_sha256, raw_content_zstd) VALUES (?1, ?2, 0, ?3, 0, '', '', NULL, '', '', '[]', '', X'')",
                    params![page_id, source_kind, title],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO sections(id, page_id, section_index, level, heading, content) VALUES (?1, ?2, 0, 1, 'Data', 'abyssal whip')",
                    params![if page_id == 1 { 1 } else { 2 }, page_id],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO chunks(id, page_id, section_id, ordinal, title, heading, text) VALUES (?1, ?2, ?3, 0, ?4, 'Data', 'abyssal whip')",
                    params![
                        if page_id == 1 { 1 } else { 2 },
                        page_id,
                        if page_id == 1 { 1 } else { 2 },
                        title
                    ],
                )
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO cache_entries(page_id, kind, entry_id, symbol, path, commit_sha) VALUES (-1, 'config/obj', '4151', 'abyssal_whip', '', '')",
                [],
            )
            .unwrap();

        let cache_chunk: i64 = connection
            .query_row(
                LEXICAL_SQL,
                params!["whip", "cache", Option::<String>::None, 50],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cache_chunk, 2);

        let filtered: i64 = connection
            .query_row(
                LEXICAL_SQL,
                params!["whip", "cache", "CONFIG/OBJ", 50],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(filtered, 2);

        let missing: Option<i64> = connection
            .query_row(
                LEXICAL_SQL,
                params!["whip", "cache", "config/loc", 50],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(missing, None);

        let mut statement = connection.prepare(LEXICAL_SQL).unwrap();
        let unified = statement
            .query_map(
                params!["whip", Option::<String>::None, Option::<String>::None, 50],
                |row| row.get(0),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<i64>>>()
            .unwrap();
        assert_eq!(unified.len(), 2);
    }

    #[test]
    fn unified_search_reads_separate_wiki_and_cache_databases() {
        let root = tempfile::tempdir().unwrap();
        let wiki_path = root.path().join("wiki.sqlite");
        let cache_path = root.path().join("cache.sqlite");
        let wiki = Connection::open(&wiki_path).unwrap();
        create_schema(&wiki).unwrap();
        let snapshot = SnapshotMetadata {
            snapshot_date: "2026-08-17".to_string(),
            started_at: String::new(),
            completed_at: String::new(),
            wiki_origin: WIKI_ORIGIN.to_string(),
            namespaces: vec![0, 120],
            shard_index: None,
            shard_count: None,
            enumerated_pages: 1,
            included_pages: 1,
            excluded_pages: 0,
            aliases: 0,
            content_license: CONTENT_LICENSE.to_string(),
            content_license_url: CONTENT_LICENSE_URL.to_string(),
        };
        wiki.execute(
            "INSERT INTO meta(key, value) VALUES ('snapshot', ?1)",
            [serde_json::to_string(&snapshot).unwrap()],
        )
        .unwrap();
        insert_search_fixture(&wiki, 1, "wiki", "Abyssal whip");

        let cache = Connection::open(&cache_path).unwrap();
        create_schema(&cache).unwrap();
        let cache_metadata = CacheMetadata {
            commit: "0".repeat(40),
            committed_at: "2026-08-17T00:00:00Z".to_string(),
            indexed_at: "2026-08-17T00:00:00Z".to_string(),
            source_url: CACHE_ORIGIN.to_string(),
            entries: 1,
        };
        cache
            .execute(
                "INSERT INTO meta(key, value) VALUES ('cache', ?1)",
                [serde_json::to_string(&cache_metadata).unwrap()],
            )
            .unwrap();
        insert_search_fixture(&cache, -1, "cache", "Cache abyssal whip");
        cache
            .execute(
                "INSERT INTO cache_entries(page_id, kind, entry_id, symbol, path, commit_sha) VALUES (-1, 'config/obj', '4151', 'abyssal_whip', '', '')",
                [],
            )
            .unwrap();
        drop(wiki);
        drop(cache);

        let engine = SearchEngine::open(&wiki_path, Some(&cache_path)).unwrap();
        let output = engine.search_unified("abyssal whip", 5, 0).unwrap();
        assert_eq!(output.results.len(), 2);
        assert!(output.results.iter().any(|row| row.source == "wiki"));
        assert!(output.results.iter().any(|row| row.source == "cache"));
    }

    #[test]
    fn verification_rejects_changed_indexed_html() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("wiki.sqlite");
        let connection = Connection::open(&database).unwrap();
        create_schema(&connection).unwrap();
        let raw = b"<html>pinned</html>";
        let sha256 = format!("{:x}", Sha256::digest(raw));
        let snapshot = SnapshotMetadata {
            snapshot_date: "2026-08-17".to_string(),
            started_at: String::new(),
            completed_at: String::new(),
            wiki_origin: WIKI_ORIGIN.to_string(),
            namespaces: vec![0],
            shard_index: None,
            shard_count: None,
            enumerated_pages: 1,
            included_pages: 1,
            excluded_pages: 0,
            aliases: 0,
            content_license: CONTENT_LICENSE.to_string(),
            content_license_url: CONTENT_LICENSE_URL.to_string(),
        };
        connection
            .execute(
                "INSERT INTO meta(key, value) VALUES ('snapshot', ?1)",
                [serde_json::to_string(&snapshot).unwrap()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO pages(id, source_kind, namespace, title, revision_id, revision_url, modified_at, touched_at, fetched_at, url, categories_json, content_sha256, raw_content_zstd) VALUES (1, 'wiki', 0, 'Pinned', 7, '', '', NULL, '', '', '[]', ?1, ?2)",
                params![sha256, zstd::stream::encode_all(raw.as_slice(), 1).unwrap()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sections(page_id, section_index, level, heading, content) VALUES (1, 0, 1, 'Lead', 'pinned')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO chunks(page_id, section_id, ordinal, title, heading, text) VALUES (1, 1, 0, 'Pinned', 'Lead', 'pinned')",
                [],
            )
            .unwrap();
        let manifest = PageManifest {
            title: "Pinned".to_string(),
            namespace: 0,
            page_id: 1,
            revision_id: 7,
            revision_url: String::new(),
            modified_at: String::new(),
            touched_at: None,
            fetched_at: String::new(),
            categories: Vec::new(),
            path: "pages/0/1.html".to_string(),
            sha256,
        };
        fs::write(
            root.path().join("manifest.jsonl"),
            format!("{}\n", serde_json::to_string(&manifest).unwrap()),
        )
        .unwrap();
        drop(connection);

        verify_index(&database, root.path()).unwrap();
        let connection = Connection::open(&database).unwrap();
        connection
            .execute(
                "UPDATE pages SET raw_content_zstd = ?1 WHERE id = 1",
                [zstd::stream::encode_all(b"changed".as_slice(), 1).unwrap()],
            )
            .unwrap();
        drop(connection);
        assert!(verify_index(&database, root.path()).is_err());
    }

    fn insert_search_fixture(
        connection: &Connection,
        page_id: i64,
        source_kind: &str,
        title: &str,
    ) {
        connection
            .execute(
                "INSERT INTO pages(id, source_kind, namespace, title, revision_id, revision_url, modified_at, touched_at, fetched_at, url, categories_json, content_sha256, raw_content_zstd) VALUES (?1, ?2, 0, ?3, 0, '', '', NULL, '', '', '[]', '', X'')",
                params![page_id, source_kind, title],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sections(page_id, section_index, level, heading, content) VALUES (?1, 0, 1, 'Data', 'abyssal whip')",
                [page_id],
            )
            .unwrap();
        let section_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO chunks(page_id, section_id, ordinal, title, heading, text) VALUES (?1, ?2, 0, ?3, 'Data', 'abyssal whip')",
                params![page_id, section_id, title],
            )
            .unwrap();
    }

    #[test]
    fn parses_posix_df_available_space() {
        let output = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk 1000 400 600 40% /\n";
        assert_eq!(parse_df_available(output), Some(600 * 1024));
    }
}
