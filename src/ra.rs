use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::error::{Result, VcsError};
use crate::repo::{Repository, VCRS_DIR};
use crate::types::{BlameLine, ChangeKind, Commit};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    Read,
    Write,
    Locking,
    Mergeinfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WireRequest {
    GetHead,
    LogRange { start: i64, end: i64 },
    Cat { path: String, revision: i64 },
    Blame { path: String, revision: i64 },
    Lock { path: String },
    Unlock { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WireResponse {
    Ok,
    Head { revision: i64 },
    Cat { bytes: Vec<u8> },
    Blame { lines: Vec<BlameLine> },
    Error { message: String },
}

pub trait RaSession {
    fn capabilities(&self) -> &[Capability];
    fn checkout(&self, dest: &Path) -> Result<()>;
    fn pull(&self, wc_root: &Path) -> Result<()>;
    fn push(&self, wc_root: &Path) -> Result<()>;
    fn lock(&self, path: &str) -> Result<()>;
    fn unlock(&self, path: &str) -> Result<()>;
    fn blame(&self, path: &str, revision: Option<&str>) -> Result<Vec<BlameLine>>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub url: String,
    pub username: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FileRaSession {
    remote_root: PathBuf,
    username: String,
    caps: Vec<Capability>,
}

impl FileRaSession {
    pub fn from_url(url: &str, username: Option<&str>) -> Result<Self> {
        let remote_root = resolve_remote_root(url)?;
        if !remote_root.join(VCRS_DIR).exists() {
            return Err(VcsError::RepositoryNotFound);
        }
        Ok(Self {
            remote_root,
            username: username.unwrap_or("anonymous").to_owned(),
            caps: vec![
                Capability::Read,
                Capability::Write,
                Capability::Locking,
                Capability::Mergeinfo,
            ],
        })
    }

    pub fn save_config(wc_root: &Path, cfg: &RemoteConfig) -> Result<()> {
        let path = wc_root.join(VCRS_DIR).join("remote.json");
        fs::write(path, serde_json::to_vec_pretty(cfg)?)?;
        Ok(())
    }

    pub fn load_config(wc_root: &Path) -> Result<Option<RemoteConfig>> {
        let path = wc_root.join(VCRS_DIR).join("remote.json");
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path)?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    fn authorize(&self, action: Action, path: &str) -> Result<()> {
        let authz_path = self.remote_root.join(VCRS_DIR).join("authz.json");
        if !authz_path.exists() {
            return Ok(());
        }
        let bytes = fs::read(authz_path)?;
        let authz: AuthzFile = serde_json::from_slice(&bytes)?;
        let Some(rules) = authz.users.get(&self.username) else {
            return Err(VcsError::AuthzDenied {
                user: self.username.clone(),
                path: path.to_owned(),
                action: action.as_str().to_owned(),
            });
        };

        let allow = match action {
            Action::Read => is_allowed(&rules.read, path),
            Action::Write => is_allowed(&rules.write, path),
        };
        if allow {
            Ok(())
        } else {
            Err(VcsError::AuthzDenied {
                user: self.username.clone(),
                path: path.to_owned(),
                action: action.as_str().to_owned(),
            })
        }
    }
}

impl RaSession for FileRaSession {
    fn capabilities(&self) -> &[Capability] {
        &self.caps
    }

    fn checkout(&self, dest: &Path) -> Result<()> {
        fs::create_dir_all(dest)?;
        let repo = Repository::init(dest)?;
        Self::save_config(
            dest,
            &RemoteConfig {
                url: format!("file://{}", self.remote_root.display()),
                username: Some(self.username.clone()),
            },
        )?;
        self.pull(&repo.root)
    }

    fn pull(&self, wc_root: &Path) -> Result<()> {
        self.authorize(Action::Read, "/")?;
        let local = Repository::discover(wc_root)?;
        let remote = Repository::discover(&self.remote_root)?;

        copy_store(&remote.root, &local.root)?;
        if let Some(head) = remote.head_commit_id()? {
            local.set_head_commit_id(&head)?;
        }
        local.rebuild_revision_index()?;
        local.clear_local_lock_tokens()?;
        local.update_to_revision("HEAD")?;
        Ok(())
    }

    fn push(&self, wc_root: &Path) -> Result<()> {
        self.authorize(Action::Write, "/")?;
        let local = Repository::discover(wc_root)?;
        let remote = Repository::discover(&self.remote_root)?;

        let Some(local_head) = local.head_commit_id()? else {
            return Ok(());
        };
        if let Some(remote_head) = remote.head_commit_id()?
            && local.read_commit(&remote_head).is_err()
        {
            return Err(VcsError::OutOfDate {
                base_rev: local.wcdb_base_rev()?,
                head_rev: remote.wcdb_head_rev()?,
            });
        }

        let pending = collect_pending_commits(&local, remote.head_commit_id()?)?;
        let lock_path = self.remote_root.join(VCRS_DIR).join("locks.json");
        let locks = load_locks(&lock_path)?;
        for commit in &pending {
            let parent = match &commit.parent {
                Some(id) => Some(local.read_commit(id)?),
                None => None,
            };
            for ch in &commit.changed_files {
                if !ch.text_modified || ch.kind == ChangeKind::Added {
                    continue;
                }
                if !path_requires_lock(parent.as_ref(), commit, &ch.path) {
                    continue;
                }
                let owner_ok = locks.get(&ch.path).is_some_and(|o| o == &self.username);
                if !owner_ok {
                    return Err(VcsError::NeedsLockRequired {
                        path: ch.path.clone(),
                    });
                }
            }
        }

        copy_store(&local.root, &remote.root)?;
        remote.set_head_commit_id(&local_head)?;
        remote.rebuild_revision_index()?;
        Ok(())
    }

    fn lock(&self, path: &str) -> Result<()> {
        self.authorize(Action::Write, path)?;
        let lock_path = self.remote_root.join(VCRS_DIR).join("locks.json");
        let mut locks = load_locks(&lock_path)?;
        if let Some(owner) = locks.get(path)
            && owner != &self.username
        {
            return Err(VcsError::LockConflict {
                path: path.to_owned(),
                owner: owner.clone(),
            });
        }
        locks.insert(path.to_owned(), self.username.clone());
        save_locks(&lock_path, &locks)
    }

    fn unlock(&self, path: &str) -> Result<()> {
        self.authorize(Action::Write, path)?;
        let lock_path = self.remote_root.join(VCRS_DIR).join("locks.json");
        let mut locks = load_locks(&lock_path)?;
        if let Some(owner) = locks.get(path)
            && owner != &self.username
        {
            return Err(VcsError::LockConflict {
                path: path.to_owned(),
                owner: owner.clone(),
            });
        }
        locks.remove(path);
        save_locks(&lock_path, &locks)
    }

    fn blame(&self, path: &str, revision: Option<&str>) -> Result<Vec<BlameLine>> {
        self.authorize(Action::Read, path)?;
        Repository::discover(&self.remote_root)?.blame_file(path, revision)
    }
}

#[derive(Debug, Clone, Copy)]
enum Action {
    Read,
    Write,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Read => "read",
            Action::Write => "write",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct AuthzFile {
    #[serde(default)]
    users: BTreeMap<String, AuthzRules>,
}

#[derive(Debug, Default, Deserialize)]
struct AuthzRules {
    #[serde(default)]
    read: Vec<String>,
    #[serde(default)]
    write: Vec<String>,
}

fn resolve_remote_root(url: &str) -> Result<PathBuf> {
    if let Some(stripped) = url.strip_prefix("file://") {
        return Ok(PathBuf::from(stripped));
    }
    if let Some(stripped) = url.strip_prefix("svn://") {
        // Phase-1 compatibility: treat svn:// as filesystem path alias.
        return Ok(PathBuf::from(stripped));
    }
    Ok(PathBuf::from(url))
}

fn is_allowed(prefixes: &[String], path: &str) -> bool {
    prefixes
        .iter()
        .any(|p| p == "/" || path == p || path.starts_with(&format!("{p}/")))
}

fn copy_store(from_root: &Path, to_root: &Path) -> Result<()> {
    let from_vcrs = from_root.join(VCRS_DIR);
    let to_vcrs = to_root.join(VCRS_DIR);
    fs::create_dir_all(&to_vcrs)?;
    copy_tree(&from_vcrs.join("objects"), &to_vcrs.join("objects"))?;
    copy_tree(&from_vcrs.join("commits"), &to_vcrs.join("commits"))?;
    let from_head = from_vcrs.join("HEAD");
    if from_head.exists() {
        fs::copy(from_head, to_vcrs.join("HEAD"))?;
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(from).follow_links(false) {
        let entry = entry?;
        let src = entry.path();
        let rel = src
            .strip_prefix(from)
            .map_err(|_| VcsError::PathOutsideRepository(src.display().to_string()))?;
        let dst = to.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dst)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            if !dst.exists() {
                fs::copy(src, dst)?;
            }
        }
    }
    Ok(())
}

fn load_locks(path: &Path) -> Result<BTreeMap<String, String>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn save_locks(path: &Path, locks: &BTreeMap<String, String>) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(locks)?)?;
    Ok(())
}

fn collect_pending_commits(repo: &Repository, remote_head: Option<String>) -> Result<Vec<Commit>> {
    let mut out = Vec::new();
    let mut cur = repo.head_commit_id()?;
    while let Some(id) = cur {
        if remote_head.as_deref() == Some(id.as_str()) {
            break;
        }
        let c = repo.read_commit(&id)?;
        cur = c.parent.clone();
        out.push(c);
    }
    out.reverse();
    Ok(out)
}

fn path_requires_lock(parent: Option<&Commit>, commit: &Commit, path: &str) -> bool {
    let parent_has = parent
        .and_then(|p| p.files.iter().find(|f| f.path == path))
        .is_some_and(|f| {
            f.props
                .get("svn:needs-lock")
                .is_some_and(|v| !v.trim().is_empty())
        });
    let current_has = commit
        .files
        .iter()
        .find(|f| f.path == path)
        .is_some_and(|f| {
            f.props
                .get("svn:needs-lock")
                .is_some_and(|v| !v.trim().is_empty())
        });
    parent_has || current_has
}
