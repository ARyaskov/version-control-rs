use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Result, VcsError};
use crate::path::validate_rel_path;
use crate::types::{Commit, FileEntry};

use super::{Repository, VCRS_DIR};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `content` to `path` atomically: write a uniquely-named temp file in the
/// same directory, then rename it over the destination (an atomic replace on the
/// same filesystem). Prevents a torn write from corrupting HEAD/commit/blob.
fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| VcsError::PathOutsideRepository(path.display().to_string()))?;
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".vcrs-tmp-{}-{}.tmp", std::process::id(), n));
    fs::write(&tmp, content)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(VcsError::Io(e));
    }
    Ok(())
}

fn vcrs(repo: &Repository) -> PathBuf {
    repo.root.join(VCRS_DIR)
}

fn objects_dir(repo: &Repository) -> PathBuf {
    vcrs(repo).join("objects")
}

fn commits_dir(repo: &Repository) -> PathBuf {
    vcrs(repo).join("commits")
}

fn head_file(repo: &Repository) -> PathBuf {
    vcrs(repo).join("HEAD")
}

pub fn ensure_layout(repo: &Repository) -> Result<()> {
    fs::create_dir_all(objects_dir(repo))?;
    fs::create_dir_all(commits_dir(repo))?;
    if !head_file(repo).exists() {
        fs::write(head_file(repo), b"")?;
    }
    Ok(())
}

pub fn read_head(repo: &Repository) -> Result<Option<String>> {
    ensure_layout(repo)?;
    let head = fs::read_to_string(head_file(repo))?;
    let trimmed = head.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_owned()))
    }
}

pub fn write_head(repo: &Repository, commit_id: &str) -> Result<()> {
    ensure_layout(repo)?;
    atomic_write(&head_file(repo), commit_id.as_bytes())?;
    Ok(())
}

pub fn write_blob(repo: &Repository, content: &[u8]) -> Result<String> {
    ensure_layout(repo)?;
    let hash = blake3::hash(content).to_hex().to_string();
    let dir = objects_dir(repo).join(&hash[0..2]);
    let path = dir.join(&hash[2..]);
    fs::create_dir_all(&dir)?;
    if !path.exists() {
        atomic_write(&path, content)?;
    }
    Ok(hash)
}

/// Content-addressed blob id without touching the object store. Used by the
/// working-copy snapshot so read-only operations (status/diff/merge planning)
/// do not litter `.vcrs/objects` with blobs that no commit will ever reference.
pub fn hash_blob(content: &[u8]) -> String {
    blake3::hash(content).to_hex().to_string()
}

pub fn read_blob(repo: &Repository, blob_id: &str) -> Result<Vec<u8>> {
    let path = blob_path(repo, blob_id);
    if !path.exists() {
        return Err(VcsError::BlobNotFound(blob_id.to_owned()));
    }
    Ok(fs::read(path)?)
}

pub fn write_commit(repo: &Repository, commit: &Commit) -> Result<()> {
    ensure_layout(repo)?;
    let path = commit_path(repo, &commit.id);
    let json = serde_json::to_vec_pretty(commit)?;
    atomic_write(&path, &json)?;
    Ok(())
}

pub fn read_commit(repo: &Repository, id: &str) -> Result<Commit> {
    let path = commit_path(repo, id);
    if !path.exists() {
        return Err(VcsError::CommitNotFound(id.to_owned()));
    }
    parse_commit(&fs::read(path)?)
}

/// Deserialize a commit and reject any path that could escape the working copy
/// once materialized. Commits may come from an untrusted remote, so this is the
/// single gate every commit read goes through.
pub fn parse_commit(bytes: &[u8]) -> Result<Commit> {
    let commit: Commit = serde_json::from_slice(bytes)?;
    validate_commit_paths(&commit)?;
    Ok(commit)
}

fn validate_commit_paths(commit: &Commit) -> Result<()> {
    for f in &commit.files {
        validate_rel_path(&f.path)?;
        if let Some(src) = &f.copy_from_path {
            validate_rel_path(src)?;
        }
    }
    for ch in &commit.changed_files {
        validate_rel_path(&ch.path)?;
        for p in [&ch.copy_from, &ch.moved_from, &ch.moved_to]
            .into_iter()
            .flatten()
        {
            validate_rel_path(p)?;
        }
    }
    for cp in &commit.changed_paths {
        validate_rel_path(&cp.path)?;
        if let Some(src) = &cp.copyfrom_path {
            validate_rel_path(src)?;
        }
    }
    Ok(())
}

pub fn new_commit_id(
    parent: Option<&str>,
    message: &str,
    author: &str,
    files: &[FileEntry],
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(parent.unwrap_or_default().as_bytes());
    hasher.update(message.as_bytes());
    hasher.update(author.as_bytes());
    for file in files {
        hasher.update(file.path.as_bytes());
        hasher.update(file.blob_id.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn blob_path(repo: &Repository, blob_id: &str) -> PathBuf {
    let (prefix, rest) = blob_id.split_at(2);
    objects_dir(repo).join(prefix).join(rest)
}

fn commit_path(repo: &Repository, id: &str) -> PathBuf {
    commits_dir(repo).join(format!("{id}.json"))
}

#[allow(dead_code)]
fn _normalize(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
