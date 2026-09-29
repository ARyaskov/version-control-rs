use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, VcsError};
use crate::repo::{PullOutcome, Repository, VCRS_DIR};
use crate::types::{BlameLine, Commit};
use crate::wcdb::PathLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    Read,
    Write,
    Locking,
    Mergeinfo,
}

pub trait RaSession {
    fn capabilities(&self) -> &[Capability];
    fn checkout(&self, dest: &Path) -> Result<()>;
    fn pull(&self, wc_root: &Path) -> Result<PullOutcome>;
    fn push(&self, wc_root: &Path) -> Result<()>;
    fn lock(&self, path: &str) -> Result<PathLock>;
    fn unlock(&self, path: &str) -> Result<()>;
    fn blame(&self, path: &str, revision: Option<&str>) -> Result<Vec<BlameLine>>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub url: String,
    pub username: Option<String>,
}

/// Access to a repository through the local filesystem (`file://` URLs or
/// plain paths). Whoever can open the directory can read and write it: access
/// control for `file://` is the filesystem's permissions (authz.json and
/// passwd.json apply to the HTTP server only), and the username is only used
/// as lock owner and default author.
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
        self.pull(&repo.root).map(|_| ())
    }

    fn pull(&self, wc_root: &Path) -> Result<PullOutcome> {
        let local = Repository::discover(wc_root)?;
        let remote = Repository::discover(&self.remote_root)?;
        let _local_lock = local.lock()?;
        let _remote_lock = remote.lock()?;

        let outcome = match remote.head_commit_id()? {
            Some(head) => {
                transfer_history(&remote, &local, &head)?;
                // Fast-forwards, or replays unpushed local commits on top.
                local.integrate_remote_head(&head)?
            }
            None => PullOutcome::default(),
        };
        local.clear_local_lock_tokens()?;
        Ok(outcome)
    }

    fn push(&self, wc_root: &Path) -> Result<()> {
        let local = Repository::discover(wc_root)?;
        let remote = Repository::discover(&self.remote_root)?;
        // Holding the remote lock across the ancestry check and the HEAD update
        // makes the push a compare-and-swap: no other push can slip in between.
        let _local_lock = local.lock()?;
        let _remote_lock = remote.lock()?;

        let Some(local_head) = local.head_commit_id()? else {
            return Ok(());
        };
        // Fast-forward only: the remote HEAD must be part of the local
        // history (having its object locally is not enough).
        if let Some(remote_head) = remote.head_commit_id()?
            && !local.is_ancestor(&remote_head, &local_head)?
        {
            return Err(VcsError::NonFastForward);
        }

        // Every commit being published must respect the remote's locks.
        let pending = collect_pending_commits(&local, remote.head_commit_id()?)?;
        for commit in &pending {
            let parent_files = match &commit.parent {
                Some(id) => local.read_commit(id)?.files,
                None => Vec::new(),
            };
            remote.check_path_locks(
                &self.username,
                Some(&parent_files),
                &commit.files,
                &commit.changed_files,
            )?;
        }

        transfer_history(&local, &remote, &local_head)?;
        remote.set_head_commit_id(&local_head)?;
        Ok(())
    }

    fn lock(&self, path: &str) -> Result<PathLock> {
        Repository::discover(&self.remote_root)?.lock_path(path, &self.username)
    }

    fn unlock(&self, path: &str) -> Result<()> {
        Repository::discover(&self.remote_root)?.unlock_path(path, &self.username)
    }

    fn blame(&self, path: &str, revision: Option<&str>) -> Result<Vec<BlameLine>> {
        Repository::discover(&self.remote_root)?.blame_file(path, revision)
    }
}

/// Remote repositories are reached through the filesystem: `file://` URLs
/// and plain paths. Other schemes (the `serve-http` protocol, `svn://`) have
/// no client implementation and are rejected instead of being mistaken for
/// local paths.
fn resolve_remote_root(url: &str) -> Result<PathBuf> {
    if let Some(stripped) = url.strip_prefix("file://") {
        return Ok(PathBuf::from(stripped));
    }
    if let Some((scheme, _)) = url.split_once("://") {
        return Err(VcsError::UnsupportedUrl(format!(
            "{scheme}:// remotes are not supported by the client; use a file:// URL or a path"
        )));
    }
    Ok(PathBuf::from(url))
}

/// Copy the history ending at `head` from `from` to `to`: only the commits
/// `to` lacks (walking back to the first one it has), oldest first, each with
/// the objects it needs — verified on read, so corruption does not spread.
/// Oldest-first order keeps the invariant that a present commit implies all
/// of its ancestors are present, even if the transfer is interrupted.
fn transfer_history(from: &Repository, to: &Repository, head: &str) -> Result<()> {
    let mut missing = Vec::new();
    let mut cur = Some(head.to_owned());
    while let Some(id) = cur {
        if to.has_commit(&id) {
            break;
        }
        cur = from.read_commit_header(&id)?.parent;
        missing.push(id);
    }
    for id in missing.iter().rev() {
        to.transfer_commit_from(from, id)?;
    }
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
