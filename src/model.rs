use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageManifest {
    pub title: String,
    pub namespace: i64,
    pub page_id: i64,
    pub revision_id: u64,
    pub revision_url: String,
    pub modified_at: String,
    #[serde(default)]
    pub touched_at: Option<String>,
    pub fetched_at: String,
    pub categories: Vec<String>,
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExcludedManifest {
    pub title: String,
    pub namespace: i64,
    pub page_id: i64,
    pub revision_id: u64,
    #[serde(default)]
    pub touched_at: Option<String>,
    pub categories: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AliasManifest {
    pub alias: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMetadata {
    pub snapshot_date: String,
    pub started_at: String,
    pub completed_at: String,
    pub wiki_origin: String,
    pub namespaces: Vec<i64>,
    pub enumerated_pages: usize,
    pub included_pages: usize,
    pub excluded_pages: usize,
    pub aliases: usize,
    pub content_license: String,
    pub content_license_url: String,
}

#[derive(Debug, Clone)]
pub struct ExtractedPage {
    pub revision_id: Option<u64>,
    pub categories: Vec<String>,
    pub sections: Vec<ExtractedSection>,
}

#[derive(Debug, Clone)]
pub struct ExtractedSection {
    pub index: i64,
    pub level: usize,
    pub heading: String,
    pub blocks: Vec<String>,
}
