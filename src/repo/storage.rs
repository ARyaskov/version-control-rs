use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Result, VcsError};
use crate::types::{Commit, FileEntry};

use super::{Repository, VCRS_DIR};

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
    fs::write(head_file(repo), commit_id.as_bytes())?;
    Ok(())
}

pub fn write_blob(repo: &Repository, content: &[u8]) -> Result<String> {
    ensure_layout(repo)?;
    let hash = blake3::hash(content).to_hex().to_string();
    let dir = objects_dir(repo).join(&hash[0..2]);
    let path = dir.join(&hash[2..]);
    fs::create_dir_all(&dir)?;
    if !path.exists() {
        fs::write(path, content)?;
    }
    Ok(hash)
}

pub fn read_blob(repo: &Repository, blob_id: &str) -> Result<Vec<u8>> {
    let path = blob_path(repo, blob_id);
    if !path.exists() {
        return Err(VcsError::CommitNotFound(format!("blob:{blob_id}")));
    }
    Ok(fs::read(path)?)
}

pub fn write_commit(repo: &Repository, commit: &Commit) -> Result<()> {
    ensure_layout(repo)?;
    let path = commit_path(repo, &commit.id);
    let json = serde_json::to_vec_pretty(commit)?;
    fs::write(path, json)?;
    Ok(())
}

pub fn read_commit(repo: &Repository, id: &str) -> Result<Commit> {
    let path = commit_path(repo, id);
    if !path.exists() {
        return Err(VcsError::CommitNotFound(id.to_owned()));
    }
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
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
