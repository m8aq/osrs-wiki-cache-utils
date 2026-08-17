use std::{
    cmp::{Ordering, Reverse},
    collections::{BinaryHeap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    CONTENT_LICENSE, CONTENT_LICENSE_URL, WIKI_ORIGIN,
    cache::{CACHE_ORIGIN, CacheDocument, CacheMetadata, CacheSnapshot, read_cache_dump},
    extract::{chunks_for_section, extract_page},
    model::{AliasManifest, PageManifest, SnapshotMetadata},
    snapshot::{read_json_lines, verify_snapshot},
};

const EMBEDDING_DIMENSION: usize = 384;
const EMBEDDING_BATCH_SIZE: usize = 16;
const TEXT_CAP: usize = 16_000;
const SECTION_CAP: usize = 200;
const CANDIDATE_LIMIT: usize = 50;
const RRF_K: f32 = 60.0;
const SQLITE_CACHE_KIB: i64 = 32 * 1024;
const LEXICAL_SQL: &str = "SELECT chunks_fts.rowid FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.rowid JOIN pages p ON p.id = c.page_id LEFT JOIN cache_entries ce ON ce.page_id = p.id WHERE chunks_fts MATCH ?1 AND (?2 IS NULL OR p.source_kind = ?2) AND (?3 IS NULL OR ce.kind = ?3 COLLATE NOCASE) ORDER BY bm25(chunks_fts, 10.0, 4.0, 1.0) LIMIT ?4";
const RECOVERY_WARNING: &str =
    "Derived retrieval text; lossless Parsoid HTML is retained in index.sqlite.";

pub struct IndexOptions {
    pub snapshot: PathBuf,
    pub database: PathBuf,
    pub model_cache: PathBuf,
    pub cache_dump: Option<PathBuf>,
    pub cache_commit: Option<String>,
}

struct LocalEmbedder(TextEmbedding);

impl LocalEmbedder {
    pub fn open(cache: &Path, progress: bool) -> Result<Self> {
        fs::create_dir_all(cache)?;
        let options = TextInitOptions::new(EmbeddingModel::BGESmallENV15Q)
            .with_cache_dir(cache.to_path_buf())
            .with_show_download_progress(progress);
        let model = TextEmbedding::try_new(options).context("load local embedding model")?;
        write_model_notice(cache)?;
        Ok(Self(model))
    }

    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let prefixed = texts
            .iter()
            .map(|text| {
                if text.starts_with("query: ") {
                    text.clone()
                } else {
                    format!("passage: {text}")
                }
            })
            .collect::<Vec<_>>();
        self.0
            .embed(&prefixed, Some(EMBEDDING_BATCH_SIZE))
            .context("embed text")
    }
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

struct RankedSearch {
    results: Vec<SearchResultRow>,
    total: usize,
    offset: usize,
    next_offset: Option<usize>,
    sources: Vec<SourceRef>,
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

struct PendingCacheChunk {
    page_id: i64,
    section_id: i64,
    ordinal: i64,
    title: String,
    heading: String,
    text: String,
}

pub struct SearchEngine {
    connection: Connection,
    embedder: LocalEmbedder,
    snapshot_date: String,
    cache: Option<CacheMetadata>,
}

#[derive(Clone, Copy, Debug)]
struct SemanticScore {
    chunk_id: i64,
    score: f32,
}

impl PartialEq for SemanticScore {
    fn eq(&self, other: &Self) -> bool {
        self.chunk_id == other.chunk_id && self.score.to_bits() == other.score.to_bits()
    }
}

impl Eq for SemanticScore {}

impl PartialOrd for SemanticScore {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SemanticScore {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| self.chunk_id.cmp(&other.chunk_id))
    }
}

pub fn build_index(options: IndexOptions) -> Result<()> {
    let cache = match (&options.cache_dump, &options.cache_commit) {
        (Some(root), Some(commit)) => Some(read_cache_dump(root, commit)?),
        (None, None) => None,
        _ => bail!("cache dump and cache commit must be provided together"),
    };
    eprintln!("index phase: load embedding model");
    let mut embedder = LocalEmbedder::open(&options.model_cache, true)?;
    if options.database.exists() && current_schema(&options.database)? {
        update_index(
            &options.snapshot,
            &options.database,
            cache.as_ref(),
            &mut embedder,
        )
    } else {
        build_new_index(
            &options.snapshot,
            &options.database,
            cache.as_ref(),
            &mut embedder,
        )
    }
}

fn build_new_index(
    snapshot: &Path,
    database: &Path,
    cache: Option<&CacheSnapshot>,
    embedder: &mut LocalEmbedder,
) -> Result<()> {
    eprintln!("index phase: verify snapshot");
    let metadata = verify_snapshot(snapshot)?;
    let pages: Vec<PageManifest> = read_json_lines(&snapshot.join("manifest.jsonl"))?;
    let raw_bytes = pages.iter().try_fold(0_u64, |total, page| {
        Ok::<_, anyhow::Error>(total + fs::metadata(snapshot.join(&page.path))?.len())
    })?;
    require_free_space(database, raw_bytes.saturating_mul(2) + 1024 * 1024 * 1024)?;
    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = database.with_extension("sqlite.part");
    let _ = fs::remove_file(&temporary);
    let mut connection = Connection::open(&temporary)?;
    create_schema(&connection)?;
    let aliases: Vec<AliasManifest> = read_json_lines(&snapshot.join("aliases.jsonl"))?;

    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES ('snapshot', ?1)",
        [serde_json::to_string(&metadata)?],
    )?;
    eprintln!("wiki index: 0/{}", pages.len());
    for (index, page) in pages.iter().enumerate() {
        insert_page(&transaction, snapshot, page, embedder)?;
        let completed = index + 1;
        if completed % 250 == 0 || completed == pages.len() {
            eprintln!("wiki index: {completed}/{}", pages.len());
        }
    }
    insert_aliases(&transaction, &aliases)?;
    if let Some(cache) = cache {
        insert_cache(&transaction, cache, embedder)?;
    }
    eprintln!("index phase: commit database");
    transaction.commit()?;
    eprintln!("index phase: rebuild full-text index");
    connection.execute("INSERT INTO chunks_fts(chunks_fts) VALUES ('rebuild')", [])?;
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
    cache: Option<&CacheSnapshot>,
    embedder: &mut LocalEmbedder,
) -> Result<()> {
    eprintln!("index phase: verify snapshot");
    let metadata = verify_snapshot(snapshot)?;
    let pages: Vec<PageManifest> = read_json_lines(&snapshot.join("manifest.jsonl"))?;
    let aliases: Vec<AliasManifest> = read_json_lines(&snapshot.join("aliases.jsonl"))?;
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

    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM aliases", [])?;
    for page_id in removed
        .iter()
        .chain(changed.iter().map(|page| &page.page_id))
    {
        transaction.execute("DELETE FROM chunks WHERE page_id = ?1", [page_id])?;
        transaction.execute("DELETE FROM sections WHERE page_id = ?1", [page_id])?;
        transaction.execute("DELETE FROM pages WHERE id = ?1", [page_id])?;
    }
    eprintln!("wiki index update: 0/{}", changed.len());
    for (index, page) in changed.iter().enumerate() {
        insert_page(&transaction, snapshot, page, embedder)?;
        let completed = index + 1;
        if completed % 250 == 0 || completed == changed.len() {
            eprintln!("wiki index update: {completed}/{}", changed.len());
        }
    }
    insert_aliases(&transaction, &aliases)?;
    let cache_counts = cache
        .map(|cache| update_cache(&transaction, cache, embedder))
        .transpose()?;
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES ('snapshot', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [serde_json::to_string(&metadata)?],
    )?;
    transaction.commit()?;
    connection.execute_batch("PRAGMA optimize;")?;

    let indexed: i64 = connection.query_row(
        "SELECT count(*) FROM pages WHERE source_kind = 'wiki'",
        [],
        |row| row.get(0),
    )?;
    if indexed != pages.len() as i64 {
        bail!(
            "index page count {indexed} does not match snapshot page count {}",
            pages.len()
        );
    }
    eprintln!(
        "index update: {} changed, {} removed, {} unchanged",
        changed.len(),
        removed.len(),
        pages.len() - changed.len()
    );
    if let Some((changed, removed, unchanged)) = cache_counts {
        eprintln!("cache update: {changed} changed, {removed} removed, {unchanged} unchanged");
    }
    Ok(())
}

fn insert_page(
    transaction: &Transaction<'_>,
    snapshot: &Path,
    page: &PageManifest,
    embedder: &mut LocalEmbedder,
) -> Result<()> {
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
            embedder,
        )?;
    }
    Ok(())
}

fn insert_cache(
    transaction: &Transaction<'_>,
    cache: &CacheSnapshot,
    embedder: &mut LocalEmbedder,
) -> Result<()> {
    let mut page_id = -1_i64;
    let mut pending = Vec::new();
    for document in &cache.documents {
        insert_cache_document(transaction, cache, document, page_id, &mut pending)?;
        if pending.len() >= EMBEDDING_BATCH_SIZE {
            insert_cache_chunks(transaction, &mut pending, embedder)?;
        }
        page_id -= 1;
    }
    insert_cache_chunks(transaction, &mut pending, embedder)?;
    transaction.execute(
        "INSERT INTO meta(key, value) VALUES ('cache', ?1)",
        [serde_json::to_string(&cache.metadata)?],
    )?;
    Ok(())
}

fn update_cache(
    transaction: &Transaction<'_>,
    cache: &CacheSnapshot,
    embedder: &mut LocalEmbedder,
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
    let mut pending = Vec::new();
    for (key, document) in &changed {
        let page_id = existing.get(*key).map(|old| old.0).unwrap_or_else(|| {
            let id = next_id;
            next_id -= 1;
            id
        });
        insert_cache_document(transaction, cache, document, page_id, &mut pending)?;
        if pending.len() >= EMBEDDING_BATCH_SIZE {
            insert_cache_chunks(transaction, &mut pending, embedder)?;
        }
    }
    insert_cache_chunks(transaction, &mut pending, embedder)?;

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
    pending: &mut Vec<PendingCacheChunk>,
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
    pending.extend(
        chunks_for_section(&document.title, &document.kind, &blocks)
            .into_iter()
            .enumerate()
            .map(|(ordinal, text)| PendingCacheChunk {
                page_id,
                section_id,
                ordinal: ordinal as i64,
                title: document.title.clone(),
                heading: document.kind.clone(),
                text,
            }),
    );
    Ok(())
}

fn insert_cache_chunks(
    transaction: &Transaction<'_>,
    pending: &mut Vec<PendingCacheChunk>,
    embedder: &mut LocalEmbedder,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let texts = pending
        .iter()
        .map(|chunk| chunk.text.clone())
        .collect::<Vec<_>>();
    let mut vectors = embedder.embed(&texts)?;
    if vectors.len() != pending.len() {
        bail!("cache embedding count mismatch");
    }
    for (chunk, vector) in pending.drain(..).zip(vectors.iter_mut()) {
        normalize(vector)?;
        transaction.execute(
            "INSERT INTO chunks(page_id, section_id, ordinal, title, heading, text, embedding) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![chunk.page_id, chunk.section_id, chunk.ordinal, chunk.title, chunk.heading, chunk.text, vector_bytes(vector)],
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
    embedder: &mut LocalEmbedder,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO sections(page_id, section_index, level, heading, content) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![page_id, section_index, level as i64, heading, content],
    )?;
    let section_id = transaction.last_insert_rowid();
    let texts = chunks_for_section(title, heading, blocks);
    let mut vectors = embedder.embed(&texts)?;
    if vectors.len() != texts.len() {
        bail!("embedding count mismatch for {title}");
    }
    for (ordinal, (text, vector)) in texts.into_iter().zip(vectors.iter_mut()).enumerate() {
        normalize(vector)?;
        transaction.execute(
            "INSERT INTO chunks(page_id, section_id, ordinal, title, heading, text, embedding) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![page_id, section_id, ordinal as i64, title, heading, text, vector_bytes(vector)],
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
            "INSERT OR IGNORE INTO aliases(alias, page_id) SELECT ?1, id FROM pages WHERE title = ?2 COLLATE NOCASE",
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
    Ok(required == 4 && redundant == 0 && cache_entries == 1)
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

impl SearchEngine {
    pub fn open(database: &Path, model_cache: &Path) -> Result<Self> {
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
        let cache = connection
            .query_row("SELECT value FROM meta WHERE key = 'cache'", [], |row| {
                row.get::<_, String>(0)
            })
            .optional()?
            .map(|value| serde_json::from_str(&value))
            .transpose()?;
        Ok(Self {
            connection,
            embedder: LocalEmbedder::open(model_cache, false)?,
            snapshot_date: metadata.snapshot_date,
            cache,
        })
    }

    pub fn search(&mut self, query: &str, limit: usize, offset: usize) -> Result<SearchOutput> {
        self.search_source(query, limit, offset, "wiki", None)
    }

    /// Searches decoded, revision-pinned game-cache records, optionally by kind.
    pub fn search_cache(
        &mut self,
        query: &str,
        limit: usize,
        offset: usize,
        kind: Option<&str>,
    ) -> Result<SearchOutput> {
        if self.cache.is_none() {
            bail!("cache data is not present in this index");
        }
        let kind = kind.map(str::trim);
        if kind == Some("") {
            bail!("cache kind must be non-empty");
        }
        self.search_source(query, limit, offset, "cache", kind)
    }

    /// Searches all indexed Wiki and game-cache content in one ranking.
    pub fn search_unified(
        &mut self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<UnifiedSearchOutput> {
        let ranked = self.search_ranked(query, limit, offset, None, None)?;
        let (wiki, cache): (Vec<_>, Vec<_>) = ranked
            .sources
            .into_iter()
            .partition(|source| source.kind == "page");
        let provenance = [
            (!wiki.is_empty()).then(|| self.provenance("wiki", wiki)),
            (!cache.is_empty()).then(|| self.provenance("cache", cache)),
        ]
        .into_iter()
        .flatten()
        .collect();
        Ok(UnifiedSearchOutput {
            results: ranked.results,
            total: ranked.total,
            offset: ranked.offset,
            next_offset: ranked.next_offset,
            provenance,
        })
    }

    fn search_source(
        &mut self,
        query: &str,
        limit: usize,
        offset: usize,
        source_kind: &str,
        cache_kind: Option<&str>,
    ) -> Result<SearchOutput> {
        let ranked = self.search_ranked(query, limit, offset, Some(source_kind), cache_kind)?;
        Ok(SearchOutput {
            results: ranked.results,
            total: ranked.total,
            offset: ranked.offset,
            next_offset: ranked.next_offset,
            provenance: self.provenance(source_kind, ranked.sources),
        })
    }

    fn search_ranked(
        &mut self,
        query: &str,
        limit: usize,
        offset: usize,
        source_kind: Option<&str>,
        cache_kind: Option<&str>,
    ) -> Result<RankedSearch> {
        let query = query.trim();
        if query.is_empty() || limit == 0 || limit > 20 {
            bail!("query must be non-empty and limit must be between 1 and 20");
        }
        let lexical = self.lexical_candidates(query, source_kind, cache_kind)?;
        let semantic = self.semantic_candidates(query, source_kind, cache_kind)?;
        let exact_pages: Vec<i64> = match source_kind {
            Some(source_kind) => self
                .resolve_page_id(query, source_kind, cache_kind)?
                .into_iter()
                .collect(),
            None => [
                self.resolve_page_id(query, "wiki", None)?,
                self.cache
                    .is_some()
                    .then(|| self.resolve_page_id(query, "cache", None))
                    .transpose()?
                    .flatten(),
            ]
            .into_iter()
            .flatten()
            .collect(),
        };
        let mut scores: HashMap<i64, f32> = HashMap::new();
        for (rank, chunk_id) in lexical.iter().enumerate() {
            *scores.entry(*chunk_id).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
        }
        for (rank, chunk_id) in semantic.iter().enumerate() {
            *scores.entry(*chunk_id).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
        }
        for page_id in exact_pages {
            let chunk_id: Option<i64> = self
                .connection
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
            let row = self.connection.query_row(
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
        let total = rows.len();
        let results = rows
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();
        let next_offset = (offset + results.len() < total).then_some(offset + results.len());
        let sources = results
            .iter()
            .map(|row| self.page_row_by_id(row.page_id).map(|page| source(&page)))
            .collect::<Result<Vec<_>>>()?;
        Ok(RankedSearch {
            results,
            total,
            offset,
            next_offset,
            sources,
        })
    }

    pub fn page(&self, title: &str) -> Result<PageOutput> {
        let page = self.page_row(title)?;
        let sections = self.section_rows(page.id)?;
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
        let rows = self.section_rows(page.id)?;
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
        let row = self
            .connection
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
        let page = self.page_row_by_id(row.0)?;
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
        query: &str,
        source_kind: Option<&str>,
        cache_kind: Option<&str>,
    ) -> Result<Vec<i64>> {
        let query = fts_query(query);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(LEXICAL_SQL)?;
        Ok(statement
            .query_map(
                params![query, source_kind, cache_kind, CANDIDATE_LIMIT as i64],
                |row| row.get(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn semantic_candidates(
        &mut self,
        query: &str,
        source_kind: Option<&str>,
        cache_kind: Option<&str>,
    ) -> Result<Vec<i64>> {
        let mut vectors = self.embedder.embed(&[format!("query: {query}")])?;
        let vector = vectors
            .first_mut()
            .ok_or_else(|| anyhow!("embedding model returned no query vector"))?;
        normalize(vector)?;
        let mut statement = self
            .connection
            .prepare("SELECT c.id, c.embedding FROM chunks c JOIN pages p ON p.id = c.page_id LEFT JOIN cache_entries ce ON ce.page_id = p.id WHERE (?1 IS NULL OR p.source_kind = ?1) AND (?2 IS NULL OR ce.kind = ?2 COLLATE NOCASE)")?;
        let mut rows = statement.query(params![source_kind, cache_kind])?;
        let mut best = BinaryHeap::with_capacity(CANDIDATE_LIMIT + 1);
        // ponytail: stream the exact scan to bound RAM; add ANN only if full-corpus p95 exceeds 500 ms.
        while let Some(row) = rows.next()? {
            let candidate = SemanticScore {
                chunk_id: row.get(0)?,
                score: dot_bytes(vector, row.get_ref(1)?.as_blob()?)?,
            };
            if best.len() < CANDIDATE_LIMIT {
                best.push(Reverse(candidate));
            } else if candidate > best.peek().expect("non-empty candidate heap").0 {
                best.pop();
                best.push(Reverse(candidate));
            }
        }
        let mut best = best
            .into_iter()
            .map(|candidate| candidate.0)
            .collect::<Vec<_>>();
        best.sort_by(|left, right| right.cmp(left));
        Ok(best
            .into_iter()
            .map(|candidate| candidate.chunk_id)
            .collect())
    }

    fn resolve_page_id(
        &self,
        title: &str,
        source_kind: &str,
        cache_kind: Option<&str>,
    ) -> Result<Option<i64>> {
        let sql = if source_kind == "wiki" {
            "SELECT id FROM pages WHERE source_kind = 'wiki' AND title = ?1 COLLATE NOCASE UNION ALL SELECT a.page_id FROM aliases a JOIN pages p ON p.id = a.page_id WHERE p.source_kind = 'wiki' AND a.alias = ?1 COLLATE NOCASE LIMIT 1"
        } else {
            "SELECT p.id FROM pages p JOIN cache_entries ce ON ce.page_id = p.id WHERE p.source_kind = 'cache' AND (p.title = ?1 COLLATE NOCASE OR ce.symbol = ?1 COLLATE NOCASE) AND (?2 IS NULL OR ce.kind = ?2 COLLATE NOCASE) LIMIT 1"
        };
        if source_kind == "wiki" {
            self.connection
                .query_row(sql, [title], |row| row.get(0))
                .optional()
                .map_err(Into::into)
        } else {
            self.connection
                .query_row(sql, params![title, cache_kind], |row| row.get(0))
                .optional()
                .map_err(Into::into)
        }
    }

    fn page_row(&self, title: &str) -> Result<PageRow> {
        let page_id = self
            .resolve_page_id(title, "wiki", None)?
            .ok_or_else(|| anyhow!("page not found"))?;
        self.page_row_by_id(page_id)
    }

    fn page_row_by_id(&self, page_id: i64) -> Result<PageRow> {
        self.connection
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

    fn section_rows(&self, page_id: i64) -> Result<Vec<(i64, usize, String, String)>> {
        let mut statement = self.connection.prepare(
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

pub fn verify_index(database: &Path) -> Result<()> {
    let connection =
        Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("SQLite integrity check failed: {integrity}");
    }
    let invalid: i64 = connection.query_row(
        "SELECT count(*) FROM chunks WHERE length(embedding) != ?1",
        [(EMBEDDING_DIMENSION * 4) as i64],
        |row| row.get(0),
    )?;
    if invalid != 0 {
        bail!("{invalid} chunk embeddings have the wrong dimension");
    }
    let pages: i64 = connection.query_row(
        "SELECT count(*) FROM pages WHERE source_kind = 'wiki'",
        [],
        |row| row.get(0),
    )?;
    let chunks: i64 = connection.query_row("SELECT count(*) FROM chunks", [], |row| row.get(0))?;
    if pages == 0 || chunks == 0 {
        bail!("index is empty");
    }
    let cache_metadata = connection
        .query_row("SELECT value FROM meta WHERE key = 'cache'", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()?;
    let cache_entries: i64 =
        connection.query_row("SELECT count(*) FROM cache_entries", [], |row| row.get(0))?;
    match cache_metadata {
        Some(metadata) => {
            let metadata: CacheMetadata = serde_json::from_str(&metadata)?;
            if cache_entries != metadata.entries as i64 {
                bail!(
                    "cache entry count {cache_entries} does not match metadata count {}",
                    metadata.entries
                );
            }
        }
        None if cache_entries != 0 => bail!("cache entries exist without cache metadata"),
        None => {}
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
           title TEXT NOT NULL UNIQUE COLLATE NOCASE,
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
           text TEXT NOT NULL,
           embedding BLOB NOT NULL
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
         CREATE INDEX cache_lookup ON cache_entries(kind, entry_id);
         CREATE INDEX chunks_page ON chunks(page_id);
         CREATE INDEX sections_page ON sections(page_id, section_index);
         CREATE VIRTUAL TABLE chunks_fts USING fts5(title, heading, text, content='chunks', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
         CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
           INSERT INTO chunks_fts(rowid, title, heading, text) VALUES (new.id, new.title, new.heading, new.text);
         END;
         CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
           INSERT INTO chunks_fts(chunks_fts, rowid, title, heading, text) VALUES ('delete', old.id, old.title, old.heading, old.text);
         END;",
    )?;
    Ok(())
}

fn normalize(vector: &mut [f32]) -> Result<()> {
    if vector.len() != EMBEDDING_DIMENSION {
        bail!(
            "expected {EMBEDDING_DIMENSION}-value embedding, got {}",
            vector.len()
        );
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm == 0.0 {
        bail!("embedding has invalid norm");
    }
    for value in vector {
        *value /= norm;
    }
    Ok(())
}

fn dot_bytes(left: &[f32], right: &[u8]) -> Result<f32> {
    if right.len() != left.len() * 4 {
        bail!("embedding byte length does not match query dimension");
    }
    let score = left
        .iter()
        .zip(right.chunks_exact(4))
        .map(|(left, right)| left * f32::from_le_bytes(right.try_into().expect("four-byte chunk")))
        .sum::<f32>();
    if !score.is_finite() {
        bail!("embedding produced a non-finite score");
    }
    Ok(score)
}

fn vector_bytes(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn fts_query(query: &str) -> String {
    query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{token}\"*"))
        .collect::<Vec<_>>()
        .join(" ")
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

fn write_model_notice(cache: &Path) -> Result<()> {
    fs::write(
        cache.join("MODEL_NOTICE.md"),
        "# Local embedding model\n\nThe index uses `Qdrant/bge-small-en-v1.5-onnx-Q` through FastEmbed. The model and FastEmbed are Apache-2.0 licensed. Source: https://huggingface.co/Qdrant/bge-small-en-v1.5-onnx-Q\n",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_product_reads_embedding_blob_without_allocating() {
        let stored = vector_bytes(&[4.0, 5.0, 6.0]);
        assert_eq!(dot_bytes(&[1.0, 2.0, 3.0], &stored).unwrap(), 32.0);
        assert!(dot_bytes(&[1.0], &stored).is_err());
    }

    #[test]
    fn fts_input_is_quoted() {
        assert_eq!(fts_query("bow (charged)"), "\"bow\"* \"charged\"*");
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
                "INSERT INTO chunks(id, page_id, section_id, ordinal, title, heading, text, embedding) VALUES (1, 1, 1, 0, 'Bow', 'Stats', 'ranged strength', ?1)",
                [vec![0_u8; EMBEDDING_DIMENSION * 4]],
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
                    "INSERT INTO chunks(id, page_id, section_id, ordinal, title, heading, text, embedding) VALUES (?1, ?2, ?3, 0, ?4, 'Data', 'abyssal whip', ?5)",
                    params![
                        if page_id == 1 { 1 } else { 2 },
                        page_id,
                        if page_id == 1 { 1 } else { 2 },
                        title,
                        vec![0_u8; EMBEDDING_DIMENSION * 4]
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
    fn parses_posix_df_available_space() {
        let output = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk 1000 400 600 40% /\n";
        assert_eq!(parse_df_available(output), Some(600 * 1024));
    }
}
