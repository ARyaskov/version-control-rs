mod blame;
mod commit;
mod history;
mod lock;
mod settings;
mod storage;
mod update;
mod working;

use commit::*;
use update::*;
use working::*;

pub use lock::RepoLock;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use chrono::{DateTime, Utc};
use globset::{Glob, GlobSet, GlobSetBuilder};
use similar::{Algorithm, DiffTag, capture_diff_slices};
use walkdir::WalkDir;

use crate::content::*;
use crate::error::{Result, VcsError};
use crate::merge::{merge_props, three_way_text};
use crate::path::{is_reserved_component, rel_from_fs, safe_join};
use crate::types::{
    BlameLine, ChangeKind, ChangedPath, ChangedPathAction, Commit, Depth, FileChange, FileEntry,
    RevisionRange,
};
use crate::wcdb::{
    ConflictRecord, ExternalDef, PathLock, RevisionRow, ScheduleOp, Scheduled, StatEntry, WcDb,
};

pub(crate) const VCRS_DIR: &str = ".vcrs";

/// Working copies whose wc.db connection a thread keeps open.
const DB_CACHE_LIMIT: usize = 8;

thread_local! {
    static DB_CACHE: RefCell<Vec<(PathBuf, Rc<WcDb>)>> = const { RefCell::new(Vec::new()) };
}

#[derive(Debug, Clone)]
pub struct Repository {
    pub root: PathBuf,
    /// Whether `.vcrs/hooks/*` scripts are executed. Enabled for a local
    /// working copy; the HTTP server disables them unless explicitly asked.
    hooks_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct MergeOutcome {
    pub changed: Vec<FileChange>,
    pub conflicts: Vec<String>,
}

/// Changes applied directly to the repository HEAD (see
/// [`Repository::commit_edits`]).
#[derive(Debug, Clone, Default)]
pub struct TreeEdits {
    /// New content per path, in repository form (as sent by a client).
    pub puts: BTreeMap<String, Vec<u8>>,
    /// Property changes per path; `None` removes the property.
    pub props: BTreeMap<String, BTreeMap<String, Option<String>>>,
    /// Paths to delete (ignored when the same path is also put).
    pub deletes: BTreeSet<String>,
    /// Revision each edit was based on. A path changed in the repository
    /// after its base revision makes the commit fail as out of date instead
    /// of silently overwriting that change.
    pub bases: BTreeMap<String, i64>,
}

/// Where the content of a commit comes from.
#[derive(Debug, Clone, Copy)]
enum CommitSource<'a> {
    /// The working copy: it must be at HEAD; pending merges and the
    /// `committed` scheduled changes are consumed (`schedule` supplies copy
    /// history), BASE moves, local lock tokens are checked.
    WorkingCopy {
        schedule: &'a BTreeMap<String, Scheduled>,
        committed: &'a [String],
    },
    /// Direct edits to the repository; the working copy is not involved.
    Store,
}

/// Result of integrating a remote HEAD into the working copy.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PullOutcome {
    /// HEAD revision afterwards.
    pub head_revision: i64,
    /// Local commits replayed (and renumbered) on top of the remote history.
    pub rebased: usize,
    /// Local commits not yet present on the remote.
    pub ahead: usize,
}

/// Which content `resolve` keeps for a text conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveAccept {
    /// The file as it is now (after manual editing).
    Working,
    /// The local version from before the merge (`.mine`).
    MineFull,
    /// The incoming version (`.rNEW`).
    TheirsFull,
    /// The common ancestor (`.rOLD`).
    Base,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct GcStats {
    /// Objects (contents and trees) removed.
    pub removed: usize,
    pub kept: usize,
    pub bytes_freed: u64,
    /// Commit objects that were not part of the history.
    pub commits_removed: usize,
}

impl Repository {
    pub fn discover(start: &Path) -> Result<Self> {
        let start_abs = fs::canonicalize(start)?;
        for dir in start_abs.ancestors() {
            if dir.join(VCRS_DIR).is_dir() {
                let repo = Self {
                    root: dir.to_path_buf(),
                    hooks_enabled: true,
                };
                repo.ensure_initialized()?;
                return Ok(repo);
            }
        }
        Err(VcsError::RepositoryNotFound)
    }

    pub fn init(path: &Path) -> Result<Self> {
        let root = if path.exists() {
            fs::canonicalize(path)?
        } else {
            fs::create_dir_all(path)?;
            fs::canonicalize(path)?
        };

        let repo = Self {
            root,
            hooks_enabled: true,
        };
        storage::ensure_layout(&repo)?;
        repo.ensure_initialized()?;
        Ok(repo)
    }

    pub fn ensure_initialized(&self) -> Result<()> {
        let _lock = self.lock()?;
        storage::ensure_layout(self)?;
        let wcdb = self.wcdb()?;
        self.migrate_legacy_layout()?;
        self.migrate_explicit_props(&wcdb)?;
        if self.head_commit_id()?.is_none() {
            wcdb.set_base_revision(0)?;
            self.sync_wcdb()?;
        }
        Ok(())
    }

    /// Take the exclusive repository lock (re-entrant on the current thread).
    /// Held by every mutating operation and by crash recovery.
    pub fn lock(&self) -> Result<RepoLock> {
        RepoLock::acquire(&self.root)
    }

    /// Enable or disable execution of repository hook scripts.
    pub fn set_hooks_enabled(&mut self, enabled: bool) {
        self.hooks_enabled = enabled;
    }

    /// Resolve a repository-relative path under the working-copy root, refusing
    /// invalid paths and paths that traverse a symbolic link.
    fn abs_path(&self, rel: &str) -> Result<PathBuf> {
        safe_join(&self.root, rel)
    }

    /// The newest indexed revision's commit. The revision index in wc.db is
    /// the single source of truth for HEAD (updated in the commit transaction).
    pub fn head_commit_id(&self) -> Result<Option<String>> {
        self.wcdb()?.head_commit_id()
    }

    /// Point HEAD at `commit_id` (whose objects must already be present),
    /// re-indexing the revisions along its parent chain.
    pub fn set_head_commit_id(&self, commit_id: &str) -> Result<()> {
        let _lock = self.lock()?;
        self.index_chain(Some(commit_id))
    }

    pub fn head_commit(&self) -> Result<Option<Commit>> {
        match self.head_commit_id()? {
            Some(id) => Ok(Some(storage::read_commit(self, &id)?)),
            None => Ok(None),
        }
    }

    pub fn read_commit(&self, id: &str) -> Result<Commit> {
        storage::read_commit(self, id)
    }

    /// Commit metadata of revision `rev` without its file list (cheap: the
    /// tree is not expanded).
    pub fn read_commit_header_by_revision(&self, rev: i64) -> Result<Commit> {
        let commit_id = self
            .wcdb()?
            .commit_for_revision(rev)?
            .ok_or_else(|| VcsError::RevisionNotFound(rev.to_string()))?;
        let mut commit = storage::read_commit_header(self, &commit_id)?;
        commit.files.clear();
        Ok(commit)
    }

    pub(crate) fn has_commit(&self, id: &str) -> bool {
        storage::commit_exists(self, id)
    }

    pub(crate) fn read_commit_header(&self, id: &str) -> Result<Commit> {
        storage::read_commit_header(self, id)
    }

    /// Copy commit `id` and every object it needs from `from`, verifying each
    /// object on the way.
    pub(crate) fn transfer_commit_from(&self, from: &Repository, id: &str) -> Result<()> {
        storage::transfer_commit(from, self, id)
    }

    pub fn read_commit_by_revision(&self, rev: i64) -> Result<Commit> {
        let wcdb = self.wcdb()?;
        let commit_id = wcdb
            .commit_for_revision(rev)?
            .ok_or_else(|| VcsError::RevisionNotFound(rev.to_string()))?;
        self.read_commit(&commit_id)
    }

    /// The entry of `path` at revision `rev`, reading only the trees along
    /// the path (not the whole revision).
    pub fn file_entry_at_revision(&self, rev: i64, path: &str) -> Result<Option<FileEntry>> {
        let commit_id = self
            .wcdb()?
            .commit_for_revision(rev)?
            .ok_or_else(|| VcsError::RevisionNotFound(rev.to_string()))?;
        let commit = storage::read_commit_header(self, &commit_id)?;
        match &commit.tree {
            Some(tree) => storage::tree_lookup(self, tree, path),
            None => Ok(commit.files.into_iter().find(|f| f.path == path)),
        }
    }

    /// Re-derive the revision index from the current HEAD's parent chain.
    pub fn rebuild_revision_index(&self) -> Result<()> {
        let _lock = self.lock()?;
        let head = self.head_commit_id()?;
        self.index_chain(head.as_deref())
    }

    pub fn write_blob(&self, content: &[u8]) -> Result<String> {
        let _lock = self.lock()?;
        storage::write_blob(self, content)
    }

    /// Remove commit objects that are not part of the history (interrupted or
    /// rejected commits, commits superseded by a pull that replayed them) and
    /// every object no remaining commit references. Runs under the repository
    /// lock, so no commit can be in progress.
    pub fn gc(&self) -> Result<GcStats> {
        let _lock = self.lock()?;
        let mut stats = GcStats::default();
        let history: BTreeSet<String> = self
            .chain_ids(self.head_commit_id()?.as_deref())?
            .into_iter()
            .collect();

        let mut live: BTreeSet<String> = BTreeSet::new();
        for id in &history {
            let commit = storage::read_commit_header(self, id)?;
            match &commit.tree {
                Some(tree) => storage::collect_tree_objects(self, tree, &mut live)?,
                None => live.extend(commit.files.iter().map(|f| f.blob_id.clone())),
            }
        }

        let commits_dir = storage::commits_dir(self);
        if commits_dir.exists() {
            for entry in fs::read_dir(&commits_dir)? {
                let path = entry?.path();
                let Some(id) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".json"))
                else {
                    continue;
                };
                if !history.contains(id) {
                    fs::remove_file(&path)?;
                    stats.commits_removed += 1;
                }
            }
        }

        let objects_dir = storage::objects_dir(self);
        if !objects_dir.exists() {
            return Ok(stats);
        }
        for prefix_entry in fs::read_dir(&objects_dir)? {
            let prefix_path = prefix_entry?.path();
            if !prefix_path.is_dir() {
                continue;
            }
            let Some(prefix) = prefix_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let prefix = prefix.to_owned();
            for object_entry in fs::read_dir(&prefix_path)? {
                let op = object_entry?.path();
                if !op.is_file() {
                    continue;
                }
                // Skips our own atomic-write temp files.
                let Some(id) = op
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| storage::object_id_from_file(&prefix, n))
                else {
                    continue;
                };
                if live.contains(&id) {
                    stats.kept += 1;
                } else {
                    let size = fs::metadata(&op).map(|m| m.len()).unwrap_or(0);
                    fs::remove_file(&op)?;
                    stats.removed += 1;
                    stats.bytes_freed += size;
                }
            }
        }
        Ok(stats)
    }

    pub fn read_blob(&self, blob_id: &str) -> Result<Vec<u8>> {
        storage::read_blob(self, blob_id)
    }

    pub fn resolve_revision_spec(&self, spec: &str) -> Result<i64> {
        let wcdb = self.wcdb()?;
        let s = spec.trim();
        // SVN symbolic revisions. BASE = the revision the working copy is at;
        // COMMITTED ~= BASE for a whole-tree request; PREV = the revision before
        // BASE. HEAD = latest committed revision.
        if s.eq_ignore_ascii_case("HEAD") {
            return wcdb.head_revision();
        }
        if s.eq_ignore_ascii_case("BASE") || s.eq_ignore_ascii_case("COMMITTED") {
            return wcdb.base_revision();
        }
        if s.eq_ignore_ascii_case("PREV") {
            return Ok((wcdb.base_revision()? - 1).max(0));
        }
        let rev: i64 = s
            .parse()
            .map_err(|_| VcsError::RevisionNotFound(spec.to_owned()))?;
        if wcdb.commit_for_revision(rev)?.is_none() {
            return Err(VcsError::RevisionNotFound(spec.to_owned()));
        }
        Ok(rev)
    }

    pub fn resolve_peg_spec(&self, spec: &str) -> Result<(Option<String>, i64)> {
        if let Some((path, rev)) = split_peg(spec) {
            return Ok((Some(path.to_owned()), self.resolve_revision_spec(rev)?));
        }
        Ok((None, self.resolve_revision_spec(spec)?))
    }

    pub fn parse_revision_range(&self, spec: &str) -> Result<RevisionRange> {
        if let Some((a, b)) = spec.split_once(':') {
            let start = self.resolve_revision_spec(a.trim())?;
            let end = self.resolve_revision_spec(b.trim())?;
            return Ok(RevisionRange {
                start: start.min(end),
                end: start.max(end),
            });
        }
        let rev = self.resolve_revision_spec(spec.trim())?;
        Ok(RevisionRange {
            start: rev,
            end: rev,
        })
    }

    pub fn wcdb_base_rev(&self) -> Result<i64> {
        self.wcdb()?.base_revision()
    }

    pub fn wcdb_head_rev(&self) -> Result<i64> {
        self.wcdb()?.head_revision()
    }

    /// The working copy's database connection. Connections are cached per
    /// thread and working copy: opening SQLite and checking the schema on
    /// every call dominated small operations.
    fn wcdb(&self) -> Result<Rc<WcDb>> {
        DB_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if let Some(pos) = cache.iter().position(|(root, _)| root == &self.root) {
                // A working copy deleted and re-created at the same path gets
                // a fresh connection.
                if self.root.join(VCRS_DIR).join("wc.db").exists() {
                    return Ok(cache[pos].1.clone());
                }
                cache.remove(pos);
            }
            let db = Rc::new(WcDb::open(&self.root)?);
            if cache.len() >= DB_CACHE_LIMIT {
                cache.remove(0);
            }
            cache.push((self.root.clone(), db.clone()));
            Ok(db)
        })
    }

    /// Append a hook failure to `.vcrs/hooks.log` (best effort).
    fn log_hook_failure(&self, err: &VcsError) {
        use std::io::Write as _;
        let path = self.root.join(VCRS_DIR).join("hooks.log");
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{} {err}", Utc::now().to_rfc3339());
        }
    }

    fn run_hook(&self, hook_name: &str, args: &[&str]) -> Result<()> {
        if !self.hooks_enabled {
            return Ok(());
        }
        let hooks = self.root.join(VCRS_DIR).join("hooks");
        let candidates = [
            hooks.join(hook_name),
            hooks.join(format!("{hook_name}.sh")),
            hooks.join(format!("{hook_name}.ps1")),
        ];
        // Only a regular file counts: a symlink could point anywhere.
        let Some(path) = candidates
            .iter()
            .find(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_file()))
        else {
            return Ok(());
        };
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        let output = if ext.eq_ignore_ascii_case("ps1") {
            Command::new("powershell")
                .arg("-ExecutionPolicy")
                .arg("Bypass")
                .arg("-File")
                .arg(path)
                .args(args)
                .output()?
        } else {
            Command::new(path).args(args).output()?
        };
        if output.status.success() {
            return Ok(());
        }
        Err(VcsError::HookFailed {
            hook: hook_name.to_owned(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}
