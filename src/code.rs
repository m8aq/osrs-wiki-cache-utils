use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use flate2::read::GzDecoder;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const PLUGINHUB_SEARCHER_ORIGIN: &str = "https://github.com/JZomDev/pluginhub-searcher";
pub(crate) const RUNELITE_ORIGIN: &str = "https://github.com/runelite/runelite";
type PluginParts = (PathBuf, Vec<String>, HashMap<String, String>);

#[derive(Debug, Clone)]
pub(crate) struct CodeDocument {
    pub kind: String,
    pub id: String,
    pub symbol: String,
    pub path: String,
    pub title: String,
    pub content: String,
    pub sha256: String,
    pub commit: String,
    pub committed_at: String,
    pub revision_url: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodeMetadata {
    pub pluginhub_commit: String,
    pub pluginhub_committed_at: String,
    pub runelite_commit: String,
    pub runelite_committed_at: String,
    pub indexed_at: String,
    pub pluginhub_source_url: String,
    pub runelite_source_url: String,
    pub plugins: usize,
    pub entries: usize,
    pub pluginhub_tooling_commit: String,
    pub http_api_commit: String,
}

#[derive(Debug)]
pub(crate) struct CodeSnapshot {
    pub metadata: CodeMetadata,
    pub documents: Vec<CodeDocument>,
}

#[derive(Deserialize)]
struct SplitDescriptor {
    zipname: String,
    content: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PluginRecord {
    internal_name: String,
    repository: String,
    commit: String,
    files: Vec<PluginFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PluginFile {
    file_name: String,
    file_path: String,
    content: String,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn read_code_sources(
    pluginhub_root: &Path,
    pluginhub_commit: &str,
    runelite_root: &Path,
    runelite_commit: &str,
    tooling_root: &Path,
    tooling_commit: &str,
    http_api_root: &Path,
    http_api_commit: &str,
) -> Result<CodeSnapshot> {
    let pluginhub_commit =
        validate_checkout(pluginhub_root, pluginhub_commit, "Plugin Hub source")?;
    let pluginhub_committed_at = git(
        pluginhub_root,
        &["show", "-s", "--format=%cI", &pluginhub_commit],
    )?;
    let (runelite_commit, runelite_committed_at, runelite) = read_git_java_documents(
        runelite_root,
        runelite_commit,
        &["runelite-api", "runelite-client"],
        None,
        "RuneLite",
        RUNELITE_ORIGIN,
    )?;
    let (pluginhub_tooling_commit, _, tooling) = read_git_java_documents(
        tooling_root,
        tooling_commit,
        &["."],
        Some("pluginhub-tooling"),
        "Plugin Hub tooling",
        "https://github.com/runelite/plugin-hub-tooling",
    )?;
    let (http_api_commit, _, http_api) = read_git_java_documents(
        http_api_root,
        http_api_commit,
        &["http-api"],
        Some("runelite-http-api"),
        "RuneLite HTTP API",
        "https://github.com/runelite/api.runelite.net",
    )?;
    let (mut documents, plugins) = read_pluginhub(pluginhub_root, &pluginhub_committed_at)?;
    documents.extend(runelite);
    documents.extend(tooling);
    documents.extend(http_api);
    documents.sort_by(|left, right| (&left.kind, &left.id).cmp(&(&right.kind, &right.id)));
    if documents.is_empty() {
        bail!("code sources contain no searchable documents");
    }
    Ok(CodeSnapshot {
        metadata: CodeMetadata {
            pluginhub_commit,
            pluginhub_committed_at,
            runelite_commit,
            runelite_committed_at,
            indexed_at: Utc::now().to_rfc3339(),
            pluginhub_source_url: PLUGINHUB_SEARCHER_ORIGIN.to_string(),
            runelite_source_url: RUNELITE_ORIGIN.to_string(),
            plugins,
            entries: documents.len(),
            pluginhub_tooling_commit,
            http_api_commit,
        },
        documents,
    })
}

fn read_git_java_documents(
    root: &Path,
    expected_commit: &str,
    roots: &[&str],
    kind: Option<&str>,
    title: &str,
    origin: &str,
) -> Result<(String, String, Vec<CodeDocument>)> {
    let commit = validate_checkout(root, expected_commit, title)?;
    let committed_at = git(root, &["show", "-s", "--format=%cI", &commit])?;
    let mut arguments = vec!["ls-tree", "-r", "--name-only", commit.as_str(), "--"];
    arguments.extend_from_slice(roots);
    let paths = git(root, &arguments)?;
    let mut documents = Vec::new();
    for path in paths.lines() {
        validate_path(path)?;
        if !is_java(path) {
            continue;
        }
        let bytes =
            fs::read(root.join(path)).with_context(|| format!("read {title} file {path}"))?;
        if bytes.contains(&0) {
            bail!("{title} Java file contains NUL bytes: {path}");
        }
        let content = String::from_utf8(bytes)
            .with_context(|| format!("decode UTF-8 {title} Java file {path}"))?;
        let kind = kind.unwrap_or_else(|| path.split('/').next().unwrap_or(path));
        let document_title = if title == "RuneLite" {
            format!("RuneLite {kind}")
        } else {
            title.to_string()
        };
        documents.push(code_document(
            kind,
            path,
            path,
            &document_title,
            content,
            &commit,
            &committed_at,
            origin,
        ));
    }
    Ok((commit, committed_at, documents))
}

fn read_pluginhub(root: &Path, committed_at: &str) -> Result<(Vec<CodeDocument>, usize)> {
    let (directory, parts, expected) = plugin_parts(root)?;
    let mut documents = Vec::new();
    let mut plugins = HashSet::new();
    let mut keys = HashSet::new();
    for part in parts {
        let records = read_plugin_part(&directory, &part)?;
        for plugin in records {
            validate_sha(&plugin.commit, "plugin commit")?;
            if plugin.internal_name.is_empty() || !plugins.insert(plugin.internal_name.clone()) {
                bail!(
                    "empty or duplicate Plugin Hub name {}",
                    plugin.internal_name
                );
            }
            if !plugin.repository.starts_with("https://github.com/")
                || !plugin.repository.ends_with(".git")
            {
                bail!("unsupported plugin repository URL {}", plugin.repository);
            }
            if expected.get(&plugin.internal_name) != Some(&plugin.commit) {
                bail!(
                    "split manifest commit mismatch for {}",
                    plugin.internal_name
                );
            }
            let origin = plugin.repository.trim_end_matches(".git");
            for file in plugin.files {
                validate_path(&file.file_path)?;
                if !is_java(&file.file_path) {
                    continue;
                }
                if file.file_name.is_empty()
                    || !keys.insert((plugin.internal_name.clone(), file.file_path.clone()))
                {
                    bail!(
                        "empty filename or duplicate path for {}",
                        plugin.internal_name
                    );
                }
                let kind = format!("pluginhub/{}", plugin.internal_name);
                documents.push(CodeDocument {
                    kind,
                    id: file.file_path.clone(),
                    symbol: file.file_name,
                    path: format!("{}/{}", plugin.internal_name, file.file_path),
                    title: format!("Plugin Hub {}: {}", plugin.internal_name, file.file_path),
                    sha256: sha256(&file.content),
                    content: file.content,
                    commit: plugin.commit.clone(),
                    committed_at: committed_at.to_string(),
                    revision_url: format!("{origin}/commit/{}", plugin.commit),
                    url: format!(
                        "{origin}/blob/{}/{}",
                        plugin.commit,
                        encode_path(&file.file_path)
                    ),
                });
            }
        }
    }
    if expected.len() != plugins.len() {
        bail!("Plugin Hub split manifest and parts contain different plugin counts");
    }
    Ok((documents, plugins.len()))
}

fn plugin_parts(root: &Path) -> Result<PluginParts> {
    let directory = root.join("plugins");
    let split_path = directory.join("plugins_splits.json");
    let parts: Vec<SplitDescriptor> = serde_json::from_reader(BufReader::new(
        File::open(&split_path).context("open Plugin Hub split manifest")?,
    ))
    .context("parse Plugin Hub split manifest")?;
    if parts.is_empty() {
        bail!("Plugin Hub split manifest is empty");
    }
    let mut expected = HashMap::new();
    let names = parts
        .into_iter()
        .map(|part| {
            for (name, commit) in part.content {
                if expected.insert(name.clone(), commit).is_some() {
                    bail!("duplicate plugin {name} in split manifest");
                }
            }
            Ok(part.zipname)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((directory, names, expected))
}

fn read_plugin_part(directory: &Path, part: &str) -> Result<Vec<PluginRecord>> {
    validate_path(part)?;
    if !part.ends_with(".json.gz") {
        bail!("Plugin Hub part is not gzip JSON: {part}");
    }
    let path = directory.join(part);
    let records: Vec<PluginRecord> =
        serde_json::from_reader(BufReader::new(GzDecoder::new(File::open(&path)?)))
            .with_context(|| format!("parse Plugin Hub part {}", path.display()))?;
    if records.is_empty() {
        bail!("Plugin Hub part {part} is empty");
    }
    Ok(records)
}

#[allow(clippy::too_many_arguments)]
fn code_document(
    kind: &str,
    id: &str,
    path: &str,
    title: &str,
    content: String,
    commit: &str,
    committed_at: &str,
    origin: &str,
) -> CodeDocument {
    let symbol = id.rsplit('/').next().unwrap_or(id);
    CodeDocument {
        kind: kind.to_string(),
        id: id.to_string(),
        symbol: symbol.to_string(),
        path: path.to_string(),
        title: format!("{title}: {id}"),
        sha256: sha256(&content),
        content,
        commit: commit.to_string(),
        committed_at: committed_at.to_string(),
        revision_url: format!("{origin}/commit/{commit}"),
        url: format!("{origin}/blob/{commit}/{}", encode_path(id)),
    }
}

fn is_java(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("java"))
}

fn validate_checkout(root: &Path, expected: &str, label: &str) -> Result<String> {
    validate_sha(expected, &format!("{label} commit"))?;
    let expected = expected.to_ascii_lowercase();
    let actual = git(root, &["rev-parse", "HEAD"])?;
    if actual != expected {
        bail!("{label} is at commit {actual}, expected {expected}");
    }
    if !git(root, &["status", "--porcelain"])?.is_empty() {
        bail!("{label} has working tree changes");
    }
    Ok(expected)
}

fn validate_sha(value: &str, label: &str) -> Result<()> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{label} must be a full 40-character Git SHA");
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "..")
    {
        bail!("unsafe source path {path:?}");
    }
    Ok(())
}

fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| utf8_percent_encode(segment, NON_ALPHANUMERIC).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn sha256(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

fn git(root: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .with_context(|| format!("run git in {}", root.display()))?;
    if !output.status.success() {
        bail!(
            "git {} failed in {}: {}",
            arguments.join(" "),
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{Compression, write::GzEncoder};
    use serde_json::json;

    use super::*;

    #[test]
    fn rejects_unsafe_paths() {
        assert!(validate_path("src/main/Test.java").is_ok());
        assert!(validate_path("../Test.java").is_err());
        assert!(validate_path("/Test.java").is_err());
        assert!(validate_path("src\\Test.java").is_err());
    }

    #[test]
    fn encodes_each_url_path_segment() {
        assert_eq!(encode_path("src/A B.java"), "src/A%20B%2Ejava");
    }

    #[test]
    fn reads_current_split_format_without_changing_empty_content() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("plugins")).unwrap();
        let commit = "0123456789abcdef0123456789abcdef01234567";
        fs::write(
            root.path().join("plugins/plugins_splits.json"),
            serde_json::to_vec(&json!([{
                "zipname": "plugins_0.json.gz",
                "content": {"example": commit}
            }]))
            .unwrap(),
        )
        .unwrap();
        let file = File::create(root.path().join("plugins/plugins_0.json.gz")).unwrap();
        let mut gzip = GzEncoder::new(file, Compression::default());
        gzip.write_all(
            &serde_json::to_vec(&json!([{
                "internalName": "example",
                "repository": "https://github.com/example/example.git",
                "commit": commit,
                "files": [{
                    "fileName": "Empty.java",
                    "filePath": "src/Empty.java",
                    "content": ""
                }]
            }]))
            .unwrap(),
        )
        .unwrap();
        gzip.finish().unwrap();

        let (documents, plugins) = read_pluginhub(root.path(), "2026-01-01T00:00:00Z").unwrap();
        assert_eq!(plugins, 1);
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].content, "");
        assert_eq!(documents[0].id, "src/Empty.java");
        assert_eq!(documents[0].commit, commit);
    }
}
