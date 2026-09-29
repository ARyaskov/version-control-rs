use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub blob_id: String,
    pub executable: bool,
    #[serde(default)]
    pub is_binary: bool,
    #[serde(default)]
    pub props: BTreeMap<String, String>,
    #[serde(default)]
    pub copy_from_path: Option<String>,
    #[serde(default)]
    pub copy_from_rev: Option<i64>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub copy_id: Option<String>,
    #[serde(default)]
    pub created_rev: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    pub id: String,
    #[serde(default)]
    pub revision: i64,
    pub parent: Option<String>,
    #[serde(default)]
    pub parent_revision: Option<i64>,
    pub author: String,
    pub message: String,
    pub created_at: DateTime<Utc>,
    pub files: Vec<FileEntry>,
    pub changed_files: Vec<FileChange>,
    #[serde(default)]
    pub mergeinfo: BTreeMap<String, String>,
    #[serde(default)]
    pub revprops: BTreeMap<String, String>,
    #[serde(default)]
    pub txn_id: Option<String>,
    #[serde(default)]
    pub changed_paths: Vec<ChangedPath>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
    #[serde(default)]
    pub text_modified: bool,
    #[serde(default)]
    pub props_modified: bool,
    #[serde(default)]
    pub is_binary: bool,
    #[serde(default)]
    pub copy_from: Option<String>,
    #[serde(default)]
    pub moved_from: Option<String>,
    #[serde(default)]
    pub moved_to: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RevisionRange {
    pub start: i64,
    pub end: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlameLine {
    pub line_no: usize,
    pub revision: i64,
    pub author: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Depth {
    Empty,
    Files,
    Immediates,
    Infinity,
}

impl Depth {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "empty" => Some(Self::Empty),
            "files" => Some(Self::Files),
            "immediates" => Some(Self::Immediates),
            "infinity" => Some(Self::Infinity),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangedPathAction {
    Add,
    Modify,
    Delete,
    Replace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedPath {
    pub path: String,
    pub action: ChangedPathAction,
    #[serde(default)]
    pub copyfrom_path: Option<String>,
    #[serde(default)]
    pub copyfrom_rev: Option<i64>,
    #[serde(default)]
    pub text_modified: bool,
    #[serde(default)]
    pub props_modified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffHunk {
    pub index: usize,
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub preview: String,
    pub staged: bool,
}
