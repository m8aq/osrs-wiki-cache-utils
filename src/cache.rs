use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const CACHE_ORIGIN: &str = "https://github.com/Joshua-F/osrs-dumps";

#[derive(Debug, Clone)]
pub(crate) struct CacheDocument {
    pub kind: String,
    pub id: String,
    pub symbol: String,
    pub path: String,
    pub title: String,
    pub content: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CacheMetadata {
    pub commit: String,
    pub committed_at: String,
    pub indexed_at: String,
    pub source_url: String,
    pub entries: usize,
}

#[derive(Debug)]
pub(crate) struct CacheSnapshot {
    pub metadata: CacheMetadata,
    pub documents: Vec<CacheDocument>,
}

pub(crate) fn read_cache_dump(root: &Path, expected_commit: &str) -> Result<CacheSnapshot> {
    let expected_commit = expected_commit.to_ascii_lowercase();
    if expected_commit.len() != 40 || !expected_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("cache commit must be a full 40-character Git SHA");
    }
    let commit = git(root, &["rev-parse", "HEAD"])?;
    if commit != expected_commit {
        bail!("cache dump is at commit {commit}, expected {expected_commit}");
    }
    if !git(root, &["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        bail!("cache dump has modified tracked files");
    }

    let mut documents = read_documents(root)?;
    documents.sort_by(|left, right| (&left.kind, &left.id).cmp(&(&right.kind, &right.id)));

    let mut keys = HashSet::with_capacity(documents.len());
    for document in &documents {
        if !keys.insert((&document.kind, &document.id)) {
            bail!(
                "duplicate cache entry {}/{} from {}",
                document.kind,
                document.id,
                document.path
            );
        }
    }
    if documents.is_empty() {
        bail!("cache dump contains no searchable documents");
    }

    Ok(CacheSnapshot {
        metadata: CacheMetadata {
            committed_at: git(root, &["show", "-s", "--format=%cI", &commit])?,
            indexed_at: Utc::now().to_rfc3339(),
            source_url: CACHE_ORIGIN.to_string(),
            entries: documents.len(),
            commit,
        },
        documents,
    })
}

fn read_documents(root: &Path) -> Result<Vec<CacheDocument>> {
    let mut documents = Vec::new();
    read_config_documents(root, &mut documents)?;
    read_file_documents(root, "interface", &mut documents)?;
    read_file_documents(root, "script", &mut documents)?;
    read_symbol_documents(root, &mut documents)?;
    Ok(documents)
}

fn read_config_documents(root: &Path, documents: &mut Vec<CacheDocument>) -> Result<()> {
    for path in files(&root.join("config"))? {
        let content = read_text(&path)?;
        let relative = relative_path(root, &path)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("non-UTF-8 cache path {}", path.display()))?;
        let kind = format!("config/{}", name.strip_prefix("dump.").unwrap_or(name));
        let records = if content.starts_with("// ") {
            split_records(&content, "// ")
        } else {
            split_records(&content, "[")
        };
        for record in records {
            let symbol = record_symbol(record).unwrap_or_else(|| name.to_string());
            let id = record
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("// "))
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .unwrap_or(&symbol)
                .to_string();
            documents.push(document(&kind, &id, &symbol, &relative, record));
        }
    }
    Ok(())
}

fn read_file_documents(
    root: &Path,
    directory: &str,
    documents: &mut Vec<CacheDocument>,
) -> Result<()> {
    for path in files(&root.join(directory))? {
        let content = read_text(&path)?;
        let relative = relative_path(root, &path)?;
        let file_stem = path
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("non-UTF-8 cache path {}", path.display()))?;
        let first_id = content
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("// "))
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let (id, symbol) = if directory == "interface" {
            (
                first_id
                    .and_then(|id| id.split(':').next())
                    .unwrap_or(file_stem),
                file_stem,
            )
        } else {
            (
                first_id.unwrap_or(file_stem),
                file_stem
                    .strip_prefix("[clientscript,")
                    .and_then(|name| name.strip_suffix(']'))
                    .unwrap_or(file_stem),
            )
        };
        documents.push(document(directory, id, symbol, &relative, &content));
    }
    Ok(())
}

fn read_symbol_documents(root: &Path, documents: &mut Vec<CacheDocument>) -> Result<()> {
    let directory = root.join("symbols");
    for path in files(&directory)? {
        let content = read_text(&path)?;
        let relative = relative_path(root, &path)?;
        let id = path
            .strip_prefix(&directory)?
            .with_extension("")
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        documents.push(document("symbols", &id, &id, &relative, &content));
    }
    Ok(())
}

fn document(kind: &str, id: &str, symbol: &str, path: &str, content: &str) -> CacheDocument {
    CacheDocument {
        kind: kind.to_string(),
        id: id.to_string(),
        symbol: symbol.to_string(),
        path: path.to_string(),
        title: format!("Cache {kind}: {symbol} ({id})"),
        content: content.to_string(),
        sha256: format!("{:x}", Sha256::digest(content.as_bytes())),
    }
}

fn split_records<'a>(content: &'a str, marker: &str) -> Vec<&'a str> {
    let mut starts = vec![0];
    let boundary = format!("\n{marker}");
    starts.extend(content.match_indices(&boundary).map(|(index, _)| index + 1));
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            &content[*start..starts.get(index + 1).copied().unwrap_or(content.len())]
        })
        .filter(|record| !record.trim().is_empty())
        .collect()
}

fn record_symbol(record: &str) -> Option<String> {
    record.lines().find_map(|line| {
        line.strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
            .map(str::to_string)
    })
}

fn files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("read cache directory {}", directory.display()))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    Ok(files)
}

fn read_text(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("read UTF-8 cache dump {}", path.display()))
}

fn relative_path(root: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(root)?
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
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
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn splits_config_records_without_changing_raw_content() {
        let content = "// 1\n[first]\nname=One\n\n// 2\n[second]\nname=Two\n";
        let records = split_records(content, "// ");
        assert_eq!(records.concat(), content);
        assert_eq!(record_symbol(records[0]).as_deref(), Some("first"));
        assert_eq!(record_symbol(records[1]).as_deref(), Some("second"));
    }

    #[test]
    fn splits_uncommented_texture_records() {
        let content = "[material_0]\nsprite=door\n\n[material_1]\nsprite=water\n";
        let records = split_records(content, "[");
        assert_eq!(records.concat(), content);
        assert_eq!(record_symbol(records[1]).as_deref(), Some("material_1"));
    }

    #[test]
    fn reads_each_supported_dump_shape() {
        let root = tempdir().unwrap();
        for directory in ["config", "interface", "script", "symbols/constant"] {
            fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        fs::write(
            root.path().join("config/dump.obj"),
            "// 4151\n[abyssal_whip]\nname=Abyssal whip\n",
        )
        .unwrap();
        fs::write(
            root.path().join("config/dump.texture"),
            "[material_0]\nsprite=door\n",
        )
        .unwrap();
        fs::write(
            root.path().join("interface/ge_offers.if3"),
            "// 465:0\n[universe]\ntype=layer\n",
        )
        .unwrap();
        fs::write(
            root.path().join("script/[clientscript,ge_init].cs2"),
            "// 123\n[clientscript,ge_init]()\nreturn;\n",
        )
        .unwrap();
        fs::write(
            root.path().join("symbols/constant/iftype.sym"),
            "iftype_layer\t0\n",
        )
        .unwrap();

        let mut documents = read_documents(root.path()).unwrap();
        documents.sort_by(|left, right| (&left.kind, &left.id).cmp(&(&right.kind, &right.id)));
        assert_eq!(documents.len(), 5);
        assert!(documents.iter().any(|document| {
            document.kind == "config/obj"
                && document.id == "4151"
                && document.symbol == "abyssal_whip"
        }));
        assert!(
            documents
                .iter()
                .any(|document| { document.kind == "interface" && document.id == "465" })
        );
        assert!(documents.iter().any(|document| {
            document.kind == "script" && document.id == "123" && document.symbol == "ge_init"
        }));
        assert!(
            documents
                .iter()
                .any(|document| { document.kind == "symbols" && document.id == "constant/iftype" })
        );
    }

    #[test]
    #[ignore = "requires OSRS_DUMP_PATH and OSRS_DUMP_COMMIT"]
    fn reads_a_real_pinned_dump() {
        let root = PathBuf::from(std::env::var_os("OSRS_DUMP_PATH").unwrap());
        let commit = std::env::var("OSRS_DUMP_COMMIT").unwrap();
        let snapshot = read_cache_dump(&root, &commit).unwrap();
        assert!(snapshot.documents.len() > 100_000);
        assert!(snapshot.documents.iter().any(|document| {
            document.kind == "config/obj" && document.symbol == "abyssal_whip"
        }));
        assert!(snapshot.documents.iter().any(|document| {
            document.kind == "script" && document.symbol == "ge_history_addline"
        }));
    }
}
