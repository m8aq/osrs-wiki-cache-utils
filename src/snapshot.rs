use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use futures_util::{StreamExt, stream};
use reqwest::{Client, Response, StatusCode};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{sync::Mutex, time::Instant};

use crate::{
    CONTENT_LICENSE, CONTENT_LICENSE_URL, WIKI_ORIGIN,
    extract::{exclusion_reason, extract_page},
    model::{AliasManifest, ExcludedManifest, PageManifest, SnapshotMetadata},
};

const API_URL: &str = "https://oldschool.runescape.wiki/api.php";
const USER_AGENT: &str = "osrs-wiki-offline/0.1 (+https://github.com/Kiyogitpy/osrs-wiki-data)";
const NAMESPACES: [i64; 2] = [0, 120];

#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    pub output: PathBuf,
    pub requests_per_second: f64,
    pub concurrency: usize,
    pub titles: Vec<String>,
    pub shard_index: Option<usize>,
    pub shard_count: Option<usize>,
}

#[derive(Debug, Clone)]
struct EnumeratedPage {
    title: String,
    namespace: i64,
    page_id: i64,
    revision_id: u64,
    modified_at: String,
    touched_at: String,
}

enum FetchOutcome {
    Included(PageManifest),
    Excluded(ExcludedManifest),
}

#[derive(Clone)]
struct WikiHttp {
    client: Client,
    limiter: Arc<RateLimiter>,
}

struct RateLimiter {
    interval: Duration,
    next: Mutex<Instant>,
}

impl RateLimiter {
    async fn wait(&self) {
        let mut next = self.next.lock().await;
        let now = Instant::now();
        if *next > now {
            tokio::time::sleep_until(*next).await;
        }
        *next = Instant::now() + self.interval;
    }
}

impl WikiHttp {
    fn new(requests_per_second: f64) -> Result<Self> {
        if !requests_per_second.is_finite() || requests_per_second <= 0.0 {
            bail!("requests per second must be greater than zero");
        }
        Ok(Self {
            client: Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(45))
                .build()?,
            limiter: Arc::new(RateLimiter {
                interval: Duration::from_secs_f64(1.0 / requests_per_second),
                next: Mutex::new(Instant::now()),
            }),
        })
    }

    async fn get_json(&self, params: &[(String, String)]) -> Result<Value> {
        let value: Value = self
            .get(API_URL, Some(params))
            .await?
            .json()
            .await
            .context("decode MediaWiki JSON")?;
        if let Some(error) = value.get("error") {
            bail!("MediaWiki API error: {error}");
        }
        Ok(value)
    }

    async fn get_html(&self, revision_id: u64) -> Result<String> {
        let url = format!("{WIKI_ORIGIN}/rest.php/v1/revision/{revision_id}/html");
        self.get(&url, None)
            .await?
            .text()
            .await
            .context("decode Parsoid HTML")
    }

    async fn get(&self, url: &str, params: Option<&[(String, String)]>) -> Result<Response> {
        for attempt in 0..5 {
            self.limiter.wait().await;
            let request = self.client.get(url);
            let response = match params {
                Some(params) => request.query(params),
                None => request,
            }
            .send()
            .await;
            match response {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response)
                    if response.status() == StatusCode::TOO_MANY_REQUESTS
                        || response.status().is_server_error() =>
                {
                    let delay = retry_delay(response.headers().get("retry-after"), attempt);
                    tokio::time::sleep(delay).await;
                }
                Ok(response) => bail!("HTTP request returned {} for {url}", response.status()),
                Err(error) if attempt < 4 => {
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    if error.is_builder() {
                        return Err(error.into());
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        bail!("HTTP request failed after five attempts for {url}")
    }
}

pub async fn build_snapshot(options: SnapshotOptions) -> Result<SnapshotMetadata> {
    if options.concurrency == 0 {
        bail!("concurrency must be greater than zero");
    }
    let shard = validate_shard(options.shard_index, options.shard_count)?;
    fs::create_dir_all(options.output.join("pages/0"))?;
    fs::create_dir_all(options.output.join("pages/120"))?;
    let started_at = Utc::now().to_rfc3339();
    let previous_included: BTreeMap<i64, PageManifest> =
        read_json_lines_if_exists::<PageManifest>(&options.output.join("manifest.jsonl"))?
            .into_iter()
            .map(|page| (page.page_id, page))
            .collect();
    let previous_excluded: BTreeMap<i64, ExcludedManifest> =
        read_json_lines_if_exists::<ExcludedManifest>(&options.output.join("excluded.jsonl"))?
            .into_iter()
            .map(|page| (page.page_id, page))
            .collect();
    let http = WikiHttp::new(options.requests_per_second)?;
    let mut pages = if options.titles.is_empty() {
        enumerate_pages(&http).await?
    } else {
        resolve_titles(&http, &options.titles).await?
    };
    if let Some((index, count)) = shard {
        pages.retain(|page| page_shard(page.page_id, count) == index);
    }
    pages.sort_by(|left, right| left.title.cmp(&right.title));
    let aliases = enumerate_aliases(&http, &pages).await?;

    let output = options.output.clone();
    let mut fetches = stream::iter(pages.iter().cloned().map(|page| {
        let http = http.clone();
        let output = output.clone();
        let previous_included = previous_included.get(&page.page_id).cloned();
        let previous_excluded = previous_excluded.get(&page.page_id).cloned();
        async move {
            let title = page.title.clone();
            fetch_page(&http, &output, page, previous_included, previous_excluded)
                .await
                .with_context(|| format!("fetch {title}"))
        }
    }))
    .buffer_unordered(options.concurrency);

    let mut included = Vec::new();
    let mut excluded = Vec::new();
    let mut failures = Vec::new();
    let mut completed = 0;
    while let Some(result) = fetches.next().await {
        match result {
            Ok(FetchOutcome::Included(page)) => included.push(page),
            Ok(FetchOutcome::Excluded(page)) => excluded.push(page),
            Err(error) => failures.push(format!("{error:#}")),
        }
        completed += 1;
        if completed % 250 == 0 || completed == pages.len() {
            eprintln!("snapshot pages: {completed}/{}", pages.len());
        }
    }
    included.sort_by(|left, right| left.title.cmp(&right.title));
    excluded.sort_by(|left, right| left.title.cmp(&right.title));

    write_snapshot_progress(&options.output, &included, &excluded, &aliases, &failures)?;

    if options.titles.is_empty() {
        let current = included
            .iter()
            .map(|page| page.page_id)
            .collect::<BTreeSet<_>>();
        for page in previous_included.values() {
            if !current.contains(&page.page_id) {
                let _ = fs::remove_file(options.output.join(&page.path));
            }
        }
    }

    let metadata = SnapshotMetadata {
        snapshot_date: Utc::now().format("%Y%m%d").to_string(),
        started_at,
        completed_at: Utc::now().to_rfc3339(),
        wiki_origin: WIKI_ORIGIN.to_string(),
        namespaces: NAMESPACES.to_vec(),
        shard_index: shard.map(|value| value.0),
        shard_count: shard.map(|value| value.1),
        enumerated_pages: pages.len(),
        included_pages: included.len(),
        excluded_pages: excluded.len(),
        aliases: aliases.len(),
        content_license: CONTENT_LICENSE.to_string(),
        content_license_url: CONTENT_LICENSE_URL.to_string(),
    };
    write_json(&options.output.join("snapshot.json"), &metadata)?;
    write_attribution(&options.output)?;
    Ok(metadata)
}

fn write_snapshot_progress(
    output: &Path,
    included: &[PageManifest],
    excluded: &[ExcludedManifest],
    aliases: &[AliasManifest],
    failures: &[String],
) -> Result<()> {
    write_json_lines(&output.join("manifest.jsonl"), included)?;
    write_json_lines(&output.join("excluded.jsonl"), excluded)?;
    write_json_lines(&output.join("aliases.jsonl"), aliases)?;
    if failures.is_empty() {
        let _ = fs::remove_file(output.join("failures.txt"));
        return Ok(());
    }
    write_lines(&output.join("failures.txt"), failures)?;
    bail!(
        "snapshot incomplete: {} page(s) failed; rerun to resume",
        failures.len()
    )
}

fn validate_shard(index: Option<usize>, count: Option<usize>) -> Result<Option<(usize, usize)>> {
    match (index, count) {
        (None, None) => Ok(None),
        (Some(index), Some(count)) if count > 0 && index < count => Ok(Some((index, count))),
        (Some(_), Some(0)) => bail!("shard count must be greater than zero"),
        (Some(index), Some(count)) => bail!("shard index {index} must be less than count {count}"),
        _ => bail!("shard index and shard count must be provided together"),
    }
}

fn page_shard(page_id: i64, count: usize) -> usize {
    page_id.unsigned_abs() as usize % count
}

async fn enumerate_pages(http: &WikiHttp) -> Result<Vec<EnumeratedPage>> {
    let mut pages = Vec::new();
    for namespace in NAMESPACES {
        let mut continuation = None;
        loop {
            let mut params = vec![
                ("action".to_string(), "query".to_string()),
                ("format".to_string(), "json".to_string()),
                ("formatversion".to_string(), "2".to_string()),
                ("generator".to_string(), "allpages".to_string()),
                ("gapnamespace".to_string(), namespace.to_string()),
                ("gapfilterredir".to_string(), "nonredirects".to_string()),
                ("gaplimit".to_string(), "max".to_string()),
                ("prop".to_string(), "info|revisions".to_string()),
                ("rvprop".to_string(), "ids|timestamp".to_string()),
            ];
            if let Some(value) = continuation.take() {
                params.push(("gapcontinue".to_string(), value));
            }
            let value = http.get_json(&params).await?;
            parse_pages(&value, &mut pages)?;
            continuation = value
                .pointer("/continue/gapcontinue")
                .and_then(Value::as_str)
                .map(str::to_string);
            if continuation.is_none() {
                break;
            }
        }
    }
    Ok(pages)
}

async fn resolve_titles(http: &WikiHttp, titles: &[String]) -> Result<Vec<EnumeratedPage>> {
    let mut pages = Vec::new();
    for batch in titles.chunks(50) {
        let params = vec![
            ("action".to_string(), "query".to_string()),
            ("format".to_string(), "json".to_string()),
            ("formatversion".to_string(), "2".to_string()),
            ("titles".to_string(), batch.join("|")),
            ("redirects".to_string(), "1".to_string()),
            ("prop".to_string(), "info|revisions".to_string()),
            ("rvprop".to_string(), "ids|timestamp".to_string()),
        ];
        let value = http.get_json(&params).await?;
        parse_pages(&value, &mut pages)?;
    }
    if pages.len() != titles.len() {
        bail!(
            "resolved {} of {} requested titles",
            pages.len(),
            titles.len()
        );
    }
    Ok(pages)
}

fn parse_pages(value: &Value, output: &mut Vec<EnumeratedPage>) -> Result<()> {
    let pages = value
        .pointer("/query/pages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("MediaWiki response omitted query.pages"))?;
    for page in pages {
        if page.get("missing").is_some() {
            continue;
        }
        let revision = page
            .get("revisions")
            .and_then(Value::as_array)
            .and_then(|values| values.first())
            .ok_or_else(|| anyhow!("page omitted latest revision: {page}"))?;
        output.push(EnumeratedPage {
            title: required_string(page, "title")?,
            namespace: required_i64(page, "ns")?,
            page_id: required_i64(page, "pageid")?,
            revision_id: required_u64(revision, "revid")?,
            modified_at: required_string(revision, "timestamp")?,
            touched_at: required_string(page, "touched")?,
        });
    }
    Ok(())
}

async fn enumerate_aliases(
    http: &WikiHttp,
    pages: &[EnumeratedPage],
) -> Result<Vec<AliasManifest>> {
    let mut aliases = BTreeMap::new();
    for batch in pages.chunks(50) {
        let mut continuation = None;
        loop {
            let mut params = vec![
                ("action".to_string(), "query".to_string()),
                ("format".to_string(), "json".to_string()),
                ("formatversion".to_string(), "2".to_string()),
                (
                    "titles".to_string(),
                    batch
                        .iter()
                        .map(|page| page.title.as_str())
                        .collect::<Vec<_>>()
                        .join("|"),
                ),
                ("prop".to_string(), "redirects".to_string()),
                ("rdnamespace".to_string(), "0|120".to_string()),
                ("rdlimit".to_string(), "max".to_string()),
            ];
            if let Some(value) = continuation.take() {
                params.push(("rdcontinue".to_string(), value));
            }
            let value = http.get_json(&params).await?;
            for page in value
                .pointer("/query/pages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let target = required_string(page, "title")?;
                for redirect in page
                    .get("redirects")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(alias) = redirect.get("title").and_then(Value::as_str) {
                        aliases.insert(alias.to_string(), target.clone());
                    }
                }
            }
            continuation = value
                .pointer("/continue/rdcontinue")
                .and_then(Value::as_str)
                .map(str::to_string);
            if continuation.is_none() {
                break;
            }
        }
    }
    Ok(aliases
        .into_iter()
        .map(|(alias, target)| AliasManifest { alias, target })
        .collect())
}

async fn fetch_page(
    http: &WikiHttp,
    output: &Path,
    page: EnumeratedPage,
    previous_included: Option<PageManifest>,
    previous_excluded: Option<ExcludedManifest>,
) -> Result<FetchOutcome> {
    let relative_path = format!("pages/{}/{}.html", page.namespace, page.page_id);
    let path = output.join(&relative_path);
    let had_metadata = previous_included.is_some() || previous_excluded.is_some();

    if let Some(mut previous) = previous_excluded
        .filter(|previous| page_is_unchanged(previous.revision_id, &previous.touched_at, &page))
    {
        previous.title = page.title;
        previous.namespace = page.namespace;
        previous.touched_at = Some(page.touched_at);
        return Ok(FetchOutcome::Excluded(previous));
    }
    if let Some(mut previous) = previous_included
        .filter(|previous| page_is_unchanged(previous.revision_id, &previous.touched_at, &page))
        && path.exists()
        && sha256(&fs::read(&path)?) == previous.sha256
    {
        previous.title = page.title.clone();
        previous.namespace = page.namespace;
        previous.modified_at = page.modified_at;
        previous.touched_at = Some(page.touched_at);
        previous.revision_url = revision_url(&page.title, page.revision_id);
        previous.path = relative_path;
        return Ok(FetchOutcome::Included(previous));
    }

    let (html, fetched_at) = if !had_metadata && path.exists() {
        let html = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let extracted = extract_page(&html)?;
        if extracted.revision_id == Some(page.revision_id) {
            (html, Utc::now().to_rfc3339())
        } else {
            (
                http.get_html(page.revision_id).await?,
                Utc::now().to_rfc3339(),
            )
        }
    } else {
        (
            http.get_html(page.revision_id).await?,
            Utc::now().to_rfc3339(),
        )
    };
    let extracted = extract_page(&html)?;
    if extracted.revision_id != Some(page.revision_id) {
        bail!(
            "revision mismatch for {}: expected {}, got {:?}",
            page.title,
            page.revision_id,
            extracted.revision_id
        );
    }
    if let Some(reason) = exclusion_reason(&extracted.categories) {
        let _ = fs::remove_file(path);
        return Ok(FetchOutcome::Excluded(ExcludedManifest {
            title: page.title,
            namespace: page.namespace,
            page_id: page.page_id,
            revision_id: page.revision_id,
            touched_at: Some(page.touched_at),
            categories: extracted.categories,
            reason,
        }));
    }
    write_atomic(&path, html.as_bytes())?;
    Ok(FetchOutcome::Included(PageManifest {
        title: page.title.clone(),
        namespace: page.namespace,
        page_id: page.page_id,
        revision_id: page.revision_id,
        revision_url: revision_url(&page.title, page.revision_id),
        modified_at: page.modified_at,
        touched_at: Some(page.touched_at),
        fetched_at,
        categories: extracted.categories,
        path: relative_path,
        sha256: sha256(html.as_bytes()),
    }))
}

fn page_is_unchanged(revision_id: u64, touched_at: &Option<String>, page: &EnumeratedPage) -> bool {
    revision_id == page.revision_id && touched_at.as_deref() == Some(page.touched_at.as_str())
}

fn revision_url(title: &str, revision_id: u64) -> String {
    format!(
        "{WIKI_ORIGIN}/w/index.php?title={}&oldid={revision_id}",
        title.replace(' ', "_")
    )
}

pub fn verify_snapshot(root: &Path) -> Result<SnapshotMetadata> {
    let metadata: SnapshotMetadata = serde_json::from_reader(BufReader::new(
        File::open(root.join("snapshot.json")).context("open snapshot.json")?,
    ))?;
    let pages: Vec<PageManifest> = read_json_lines(&root.join("manifest.jsonl"))?;
    let excluded: Vec<ExcludedManifest> = read_json_lines(&root.join("excluded.jsonl"))?;
    let aliases: Vec<AliasManifest> = read_json_lines(&root.join("aliases.jsonl"))?;
    if pages.len() != metadata.included_pages
        || excluded.len() != metadata.excluded_pages
        || aliases.len() != metadata.aliases
        || pages.len() + excluded.len() != metadata.enumerated_pages
    {
        bail!("snapshot counts do not match metadata");
    }
    eprintln!("snapshot verify: 0/{}", pages.len());
    for (index, page) in pages.iter().enumerate() {
        let path = root.join(&page.path);
        let html = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        if sha256(&html) != page.sha256 {
            bail!("checksum mismatch for {}", page.title);
        }
        let text =
            String::from_utf8(html).with_context(|| format!("UTF-8 HTML for {}", page.title))?;
        if extract_page(&text)?.revision_id != Some(page.revision_id) {
            bail!("revision mismatch for {}", page.title);
        }
        let completed = index + 1;
        if completed % 1_000 == 0 || completed == pages.len() {
            eprintln!("snapshot verify: {completed}/{}", pages.len());
        }
    }
    Ok(metadata)
}

pub fn read_json_lines<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let reader =
        BufReader::new(File::open(path).with_context(|| format!("open {}", path.display()))?);
    reader
        .lines()
        .filter(|line| {
            line.as_ref()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(true)
        })
        .map(|line| Ok(serde_json::from_str(&line?)?))
        .collect()
}

fn read_json_lines_if_exists<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    if path.exists() {
        read_json_lines(path)
    } else {
        Ok(Vec::new())
    }
}

fn write_json_lines<T: Serialize>(path: &Path, values: &[T]) -> Result<()> {
    let mut output = Vec::new();
    for value in values {
        serde_json::to_writer(&mut output, value)?;
        output.push(b'\n');
    }
    write_atomic(path, &output)
}

fn write_lines(path: &Path, values: &[String]) -> Result<()> {
    write_atomic(path, values.join("\n").as_bytes())
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(value)?)
}

fn write_attribution(root: &Path) -> Result<()> {
    write_atomic(
        &root.join("ATTRIBUTION.md"),
        format!(
            "# Attribution\n\nThis snapshot contains transformed content from the [Old School RuneScape Wiki]({WIKI_ORIGIN}/), credited to its contributors. Per-page canonical revision links are recorded in `manifest.jsonl`. Wiki content is licensed under [{CONTENT_LICENSE}]({CONTENT_LICENSE_URL}).\n"
        )
        .as_bytes(),
    )
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!(
        "{}.part",
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or("tmp")
    ));
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn retry_delay(value: Option<&reqwest::header::HeaderValue>, attempt: usize) -> Duration {
    value
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(1 << attempt))
}

fn required_string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing string field {key}: {value}"))
}

fn required_i64(value: &Value, key: &str) -> Result<i64> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("missing integer field {key}: {value}"))
}

fn required_u64(value: &Value, key: &str) -> Result<u64> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("missing unsigned field {key}: {value}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_wins() {
        let value = reqwest::header::HeaderValue::from_static("7");
        assert_eq!(retry_delay(Some(&value), 0), Duration::from_secs(7));
    }

    #[test]
    fn json_lines_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("aliases.jsonl");
        let values = vec![AliasManifest {
            alias: "Bowfa".to_string(),
            target: "Bow of Faerdhinen".to_string(),
        }];
        write_json_lines(&path, &values).unwrap();
        let read: Vec<AliasManifest> = read_json_lines(&path).unwrap();
        assert_eq!(read[0].target, values[0].target);
    }

    #[test]
    fn failed_snapshot_checkpoints_successful_pages() {
        let directory = tempfile::tempdir().unwrap();
        let included = vec![PageManifest {
            title: "Included".to_string(),
            namespace: 0,
            page_id: 1,
            revision_id: 2,
            revision_url: "revision".to_string(),
            modified_at: "modified".to_string(),
            touched_at: Some("touched".to_string()),
            fetched_at: "fetched".to_string(),
            categories: Vec::new(),
            path: "pages/0/1.html".to_string(),
            sha256: "hash".to_string(),
        }];
        let excluded = vec![ExcludedManifest {
            title: "Excluded".to_string(),
            namespace: 0,
            page_id: 3,
            revision_id: 4,
            touched_at: Some("touched".to_string()),
            categories: vec!["Historical content".to_string()],
            reason: "Historical content".to_string(),
        }];
        let aliases = vec![AliasManifest {
            alias: "Alias".to_string(),
            target: "Included".to_string(),
        }];
        let failures = vec!["fetch Failed: HTTP 500".to_string()];

        assert!(
            write_snapshot_progress(directory.path(), &included, &excluded, &aliases, &failures)
                .is_err()
        );
        assert_eq!(
            read_json_lines::<PageManifest>(&directory.path().join("manifest.jsonl")).unwrap()[0]
                .page_id,
            1
        );
        assert_eq!(
            read_json_lines::<ExcludedManifest>(&directory.path().join("excluded.jsonl")).unwrap()
                [0]
            .page_id,
            3
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("failures.txt")).unwrap(),
            failures[0]
        );
    }

    #[test]
    fn touched_change_invalidates_same_revision() {
        let page = EnumeratedPage {
            title: "Example".to_string(),
            namespace: 0,
            page_id: 1,
            revision_id: 2,
            modified_at: "2026-01-01T00:00:00Z".to_string(),
            touched_at: "2026-01-02T00:00:00Z".to_string(),
        };
        assert!(page_is_unchanged(
            2,
            &Some("2026-01-02T00:00:00Z".to_string()),
            &page
        ));
        assert!(!page_is_unchanged(
            2,
            &Some("2026-01-01T00:00:00Z".to_string()),
            &page
        ));
        assert!(!page_is_unchanged(3, &Some(page.touched_at.clone()), &page));
    }

    #[test]
    fn shards_partition_page_ids_exactly_once() {
        for page_id in 1..10_000 {
            let memberships = (0..32)
                .filter(|index| page_shard(page_id, 32) == *index)
                .count();
            assert_eq!(memberships, 1);
        }
        assert_eq!(validate_shard(Some(31), Some(32)).unwrap(), Some((31, 32)));
        assert!(validate_shard(Some(32), Some(32)).is_err());
        assert!(validate_shard(Some(0), None).is_err());
    }
}
