mod lock;
mod storage;

pub use lock::RepoLock;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use diffy::merge as diffy_merge;
use globset::{Glob, GlobSet, GlobSetBuilder};
use similar::{Algorithm, DiffTag, capture_diff_slices};
use walkdir::WalkDir;

use crate::error::{Result, VcsError};
use crate::path::{is_reserved_component, rel_from_fs, safe_join};
use crate::types::{
    BlameLine, ChangeKind, ChangedPath, ChangedPathAction, Commit, Depth, FileChange, FileEntry,
    RevisionRange,
};
use crate::wcdb::{ConflictRecord, ExternalDef, RevisionRow, ScheduleOp, Scheduled, WcDb};

pub(crate) const VCRS_DIR: &str = ".vcrs";

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
    pub removed: usize,
    pub kept: usize,
    pub bytes_freed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeOp {
    None,
    Add,
    Delete,
    Modify,
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

    pub fn read_commit_by_revision(&self, rev: i64) -> Result<Commit> {
        let wcdb = self.wcdb()?;
        let commit_id = wcdb
            .commit_for_revision(rev)?
            .ok_or_else(|| VcsError::RevisionNotFound(rev.to_string()))?;
        self.read_commit(&commit_id)
    }

    pub fn file_entry_at_revision(&self, rev: i64, path: &str) -> Result<Option<FileEntry>> {
        let commit = self.read_commit_by_revision(rev)?;
        Ok(commit.files.into_iter().find(|f| f.path == path))
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

    /// Sweep blobs in the object store that no commit references. Runs under
    /// the repository lock, so no commit can be in progress.
    pub fn gc(&self) -> Result<GcStats> {
        let _lock = self.lock()?;
        let commits_dir = self.root.join(VCRS_DIR).join("commits");
        let mut referenced: BTreeSet<String> = BTreeSet::new();
        if commits_dir.exists() {
            for entry in fs::read_dir(&commits_dir)? {
                let entry = entry?;
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let commit = storage::parse_commit(&fs::read(&p)?)?;
                for f in &commit.files {
                    referenced.insert(f.blob_id.clone());
                }
            }
        }

        let objects_dir = self.root.join(VCRS_DIR).join("objects");
        let mut stats = GcStats::default();
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
            for blob_entry in fs::read_dir(&prefix_path)? {
                let bp = blob_entry?.path();
                if !bp.is_file() {
                    continue;
                }
                let Some(rest) = bp.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                // Skip our own atomic-write temp files.
                if rest.ends_with(".tmp") {
                    continue;
                }
                let hash = format!("{prefix}{rest}");
                if referenced.contains(&hash) {
                    stats.kept += 1;
                } else {
                    let size = fs::metadata(&bp).map(|m| m.len()).unwrap_or(0);
                    fs::remove_file(&bp)?;
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

    /// Read-only view of the working copy: computes blob ids by hashing without
    /// writing to the object store. Use this for status/diff/merge planning.
    pub fn snapshot_working_copy(&self) -> Result<Vec<FileEntry>> {
        self.collect_working(false)
    }

    /// Like [`snapshot_working_copy`] but persists each file's content as a blob
    /// in the object store. Use this only when producing a commit.
    fn materialize_working_copy(&self) -> Result<Vec<FileEntry>> {
        self.collect_working(true)
    }

    /// Versioned files of the working copy: BASE paths not scheduled for
    /// deletion plus scheduled additions. Unversioned files are never part of
    /// the snapshot; a versioned file missing from disk is simply absent (and
    /// therefore reported as deleted).
    fn collect_working(&self, persist: bool) -> Result<Vec<FileEntry>> {
        let wcdb = self.wcdb()?;
        let base = self.base_files(&wcdb)?;
        let schedule = wcdb.schedule()?;
        let scope = WcScope::load(&wcdb)?;
        // One SQL round-trip each instead of two per file.
        let all_file_props = wcdb.all_file_props()?;
        let all_inherited = wcdb.all_inherited_props()?;
        let empty_props = BTreeMap::new();

        let mut entries = Vec::new();
        for rel in versioned_paths(&base, &schedule) {
            // BASE paths outside the working-copy depth are not materialized;
            // explicitly scheduled additions always count.
            if !schedule.contains_key(&rel) && !scope.contains(&rel) {
                continue;
            }
            // Missing, replaced by a directory, or hidden behind a symlinked
            // parent: not present in the working copy.
            let Ok(abs) = self.abs_path(&rel) else {
                continue;
            };
            let Ok(md) = fs::symlink_metadata(&abs) else {
                continue;
            };
            if md.is_dir() {
                continue;
            }

            let is_symlink = md.file_type().is_symlink();
            let raw = if is_symlink {
                let target = fs::read_link(&abs)?;
                format!("link {}", target.to_string_lossy()).into_bytes()
            } else {
                fs::read(&abs)?
            };

            // A symlink is marked by svn:special; the exec bit on the link
            // itself (conventionally 0o777) is meaningless, so never infer it.
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                !is_symlink && md.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;

            let file_props = all_file_props.get(&rel).unwrap_or(&empty_props);
            let inherited = resolve_inherited(&rel, &all_inherited);
            entries.push(self.finalize_entry(
                &rel, raw, is_symlink, executable, file_props, &inherited, persist,
            )?);
        }
        Ok(entries)
    }

    /// Files present on disk that are neither versioned nor ignored.
    pub fn unversioned(&self) -> Result<Vec<String>> {
        let wcdb = self.wcdb()?;
        let versioned = versioned_paths(&self.base_files(&wcdb)?, &wcdb.schedule()?);
        let ignore = build_ignore_globset(&self.collect_ignore_patterns(&wcdb)?);
        let scope = WcScope::load(&wcdb)?;
        Ok(self
            .walk_files(&self.root)?
            .into_iter()
            .filter(|rel| {
                !versioned.contains(rel)
                    && !ignore.is_match(rel)
                    && !is_under_external(rel, &scope.externals)
            })
            .collect())
    }

    /// Every file or symlink below `dir` with a representable repository
    /// path, sorted; metadata directories are pruned.
    fn walk_files(&self, dir: &Path) -> Result<Vec<String>> {
        let root = self.root.clone();
        let walker = WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(move |e| !is_excluded_path(e.path(), &root));
        let mut out = Vec::new();
        for entry in walker {
            let entry = entry?;
            if entry.file_type().is_dir() {
                continue;
            }
            if let Some(rel) = rel_from_fs(&self.root, entry.path()) {
                out.push(rel);
            }
        }
        out.sort();
        Ok(out)
    }

    /// The BASE revision's entry for `path`, if the file exists there.
    pub fn base_file_entry(&self, path: &str) -> Result<Option<FileEntry>> {
        Ok(self
            .base_files(&self.wcdb()?)?
            .into_iter()
            .find(|f| f.path == path))
    }

    /// Repository-form bytes of a working file (eol normalized, keywords
    /// contracted) — what a commit would store.
    pub fn working_repo_bytes(&self, path: &str) -> Result<Vec<u8>> {
        self.normalized_working_bytes(path)
    }

    /// Files of the BASE revision (empty before the first update/commit).
    fn base_files(&self, wcdb: &WcDb) -> Result<Vec<FileEntry>> {
        let base_rev = wcdb.base_revision()?;
        if base_rev == 0 {
            return Ok(Vec::new());
        }
        Ok(self.read_commit_by_revision(base_rev)?.files)
    }

    /// Put paths under version control. Directories are added recursively,
    /// skipping ignored files; naming an ignored file explicitly adds it.
    /// Re-adding a path scheduled for deletion cancels the deletion.
    pub fn add(&self, paths: &[String]) -> Result<Vec<String>> {
        let _lock = self.lock()?;
        let wcdb = self.wcdb()?;
        let base: BTreeSet<String> = self
            .base_files(&wcdb)?
            .into_iter()
            .map(|f| f.path)
            .collect();
        let ignore = build_ignore_globset(&self.collect_ignore_patterns(&wcdb)?);
        let mut added = BTreeSet::new();
        for path in paths {
            let abs = self.abs_path(path)?;
            let md =
                fs::symlink_metadata(&abs).map_err(|_| VcsError::PathNotFound(path.clone()))?;
            let candidates = if md.is_dir() {
                self.walk_files(&abs)?
                    .into_iter()
                    .filter(|rel| !ignore.is_match(rel))
                    .collect()
            } else {
                vec![path.clone()]
            };
            let schedule = wcdb.schedule()?;
            for rel in candidates {
                match schedule.get(&rel) {
                    Some(s) if s.op == ScheduleOp::Delete => {
                        wcdb.clear_schedule(std::slice::from_ref(&rel))?;
                        added.insert(rel);
                    }
                    Some(_) => {}
                    None if base.contains(&rel) => {}
                    None => {
                        wcdb.set_schedule(&rel, ScheduleOp::Add, None)?;
                        added.insert(rel);
                    }
                }
            }
        }
        self.sync_wcdb()?;
        Ok(added.into_iter().collect())
    }

    /// Schedule versioned paths (a directory means everything below it) for
    /// deletion; scheduled additions are simply un-scheduled. Unless
    /// `keep_local`, the files are removed from disk too.
    pub fn remove(&self, paths: &[String], keep_local: bool) -> Result<Vec<String>> {
        let _lock = self.lock()?;
        let wcdb = self.wcdb()?;
        let schedule = wcdb.schedule()?;
        let versioned = versioned_paths(&self.base_files(&wcdb)?, &schedule);
        let mut removed = Vec::new();
        for path in paths {
            crate::path::validate_rel_path(path)?;
            let targets: Vec<&String> = versioned
                .iter()
                .filter(|v| path_in_scope(v, path))
                .collect();
            if targets.is_empty() {
                return Err(VcsError::NotVersioned(path.clone()));
            }
            for rel in targets {
                if schedule.get(rel).is_some_and(|s| s.op == ScheduleOp::Add) {
                    wcdb.clear_schedule(std::slice::from_ref(rel))?;
                } else {
                    wcdb.set_schedule(rel, ScheduleOp::Delete, None)?;
                }
                if !keep_local && let Ok(abs) = self.abs_path(rel) {
                    remove_path_if_exists(&abs)?;
                }
                removed.push(rel.clone());
            }
        }
        self.sync_wcdb()?;
        Ok(removed)
    }

    /// Build a single committed file entry from the working copy. `content`
    /// overrides the on-disk bytes (used for partial-hunk staging); pass `None`
    /// to read the file. Always persists the resulting blob.
    fn working_entry(
        &self,
        rel: &str,
        content: Option<Vec<u8>>,
        persist: bool,
    ) -> Result<FileEntry> {
        let abs = self.abs_path(rel)?;
        let md = fs::symlink_metadata(&abs)?;
        let is_symlink = md.file_type().is_symlink();
        let raw = if is_symlink {
            let target = fs::read_link(&abs)?;
            format!("link {}", target.to_string_lossy()).into_bytes()
        } else {
            match content {
                Some(bytes) => bytes,
                None => fs::read(&abs)?,
            }
        };
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            !is_symlink && md.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;

        let wcdb = self.wcdb()?;
        let file_props = wcdb.file_props(rel)?;
        let inherited = wcdb.inherited_props_for_path(rel)?;
        self.finalize_entry(
            rel,
            raw,
            is_symlink,
            executable,
            &file_props,
            &inherited,
            persist,
        )
    }

    fn finalize_entry(
        &self,
        rel: &str,
        raw: Vec<u8>,
        is_symlink: bool,
        executable: bool,
        file_props: &BTreeMap<String, String>,
        inherited: &BTreeMap<String, String>,
        persist: bool,
    ) -> Result<FileEntry> {
        let (bytes, props, is_binary) =
            repo_form(raw, is_symlink, file_props, inherited, executable);
        let executable = has_svn_prop(&props, "svn:executable");
        let blob_id = if persist {
            self.write_blob(&bytes)?
        } else {
            storage::hash_blob(&bytes)
        };
        Ok(FileEntry {
            path: rel.to_owned(),
            blob_id,
            executable,
            is_binary,
            props,
            copy_from_path: None,
            copy_from_rev: None,
            node_id: None,
            copy_id: None,
            created_rev: None,
        })
    }

    /// Repository-form (normalized) bytes for a working file, matching exactly
    /// what the snapshot would store as its blob. Used when a merge needs the
    /// working content but the snapshot did not persist it.
    fn normalized_working_bytes(&self, rel: &str) -> Result<Vec<u8>> {
        let abs = self.abs_path(rel)?;
        let md = fs::symlink_metadata(&abs)?;
        let is_symlink = md.file_type().is_symlink();
        let raw = if is_symlink {
            let target = fs::read_link(&abs)?;
            format!("link {}", target.to_string_lossy()).into_bytes()
        } else {
            fs::read(&abs)?
        };
        let wcdb = self.wcdb()?;
        let file_props = wcdb.file_props(rel)?;
        let inherited = wcdb.inherited_props_for_path(rel)?;
        let (bytes, _props, _is_binary) =
            repo_form(raw, is_symlink, &file_props, &inherited, false);
        Ok(bytes)
    }

    pub fn commit(&self, message: &str, author: &str) -> Result<Commit> {
        let mut revprops = BTreeMap::new();
        revprops.insert("svn:author".to_owned(), author.to_owned());
        revprops.insert("svn:log".to_owned(), message.to_owned());
        self.commit_with_revprops(message, author, revprops)
    }

    pub fn commit_with_revprops(
        &self,
        message: &str,
        author: &str,
        revprops: BTreeMap<String, String>,
    ) -> Result<Commit> {
        let _lock = self.lock()?;
        self.ensure_initialized()?;
        let schedule = self.wcdb()?.schedule()?;
        let tree = self.working_commit_tree()?;
        let committed: Vec<String> = schedule.keys().cloned().collect();
        self.commit_entries(tree, message, author, revprops, &schedule, &committed)
    }

    /// The tree a full commit records: the versioned working files plus BASE
    /// files outside the working-copy depth, carried over unchanged (a sparse
    /// checkout must not delete what it did not materialize).
    fn working_commit_tree(&self) -> Result<Vec<FileEntry>> {
        let wcdb = self.wcdb()?;
        let scope = WcScope::load(&wcdb)?;
        let schedule = wcdb.schedule()?;
        let mut tree: BTreeMap<String, FileEntry> = self
            .materialize_working_copy()?
            .into_iter()
            .map(|f| (f.path.clone(), f))
            .collect();
        for f in self.base_files(&wcdb)? {
            if !scope.contains(&f.path) && !schedule.contains_key(&f.path) {
                tree.entry(f.path.clone()).or_insert(f);
            }
        }
        Ok(tree.into_values().collect())
    }

    /// Commit an explicit, already-persisted set of file entries as the next
    /// revision. The working copy is never read or mutated here, so staged and
    /// partial commits cannot lose unstaged work to a crash.
    fn commit_entries(
        &self,
        mut snapshot: Vec<FileEntry>,
        message: &str,
        author: &str,
        mut revprops: BTreeMap<String, String>,
        schedule: &BTreeMap<String, Scheduled>,
        committed_schedule: &[String],
    ) -> Result<Commit> {
        let wcdb = self.wcdb()?;

        let base_rev = wcdb.base_revision()?;
        let head_rev = wcdb.head_revision()?;
        // Recorded merges do not exempt a stale working copy: committing on
        // top of an outdated BASE would silently discard newer revisions.
        if base_rev != head_rev {
            return Err(VcsError::OutOfDate { base_rev, head_rev });
        }

        let parent = self.head_commit()?;
        let mut changed =
            compute_changed_files(parent.as_ref().map(|c| c.files.as_slice()), &snapshot);
        apply_scheduled_copies(&mut changed, schedule);
        let parent_revision = parent.as_ref().map(|c| c.revision);
        // Consumed atomically with the revision itself (see record_commit), so
        // a failed or interrupted commit never loses recorded merges.
        let pending_merges = wcdb.pending_merges()?;

        if changed.is_empty() && pending_merges.is_empty() {
            return Err(VcsError::NothingToCommit);
        }

        // Refuse to commit paths with a recorded, unresolved conflict, and
        // (as a safety net) text that still contains conflict markers.
        let recorded = wcdb.conflicts()?;
        let mut conflicted: Vec<String> = changed
            .iter()
            .filter(|ch| recorded.contains_key(&ch.path))
            .map(|ch| ch.path.clone())
            .collect();
        for ch in &changed {
            if ch.kind == ChangeKind::Deleted || ch.is_binary || !ch.text_modified {
                continue;
            }
            if let Some(entry) = snapshot.iter().find(|f| f.path == ch.path)
                && has_conflict_markers(&self.read_blob(&entry.blob_id)?)
            {
                conflicted.push(ch.path.clone());
            }
        }
        if !conflicted.is_empty() {
            return Err(VcsError::UnresolvedConflicts {
                paths: conflicted.join(", "),
            });
        }

        let parent_id = parent.as_ref().map(|c| c.id.clone());
        let next_rev = wcdb.max_revision()? + 1;
        let id = storage::new_commit_id(parent_id.as_deref(), message, author, &snapshot);
        let snapshot_map: BTreeMap<&str, &FileEntry> =
            snapshot.iter().map(|f| (f.path.as_str(), f)).collect();
        for ch in &changed {
            if ch.kind != ChangeKind::Modified || !ch.text_modified {
                continue;
            }
            let Some(entry) = snapshot_map.get(ch.path.as_str()).copied() else {
                continue;
            };
            if has_svn_prop(&entry.props, "svn:needs-lock") && !wcdb.has_lock_token(&ch.path)? {
                return Err(VcsError::NeedsLockRequired {
                    path: ch.path.clone(),
                });
            }
        }
        for ch in &changed {
            if ch.kind == ChangeKind::Added
                && let Some(src) = &ch.copy_from
                && let Some(entry) = snapshot.iter_mut().find(|f| f.path == ch.path)
            {
                entry.copy_from_path = Some(src.clone());
                entry.copy_from_rev = parent_revision;
            }
        }
        assign_node_identity(parent.as_ref(), &mut snapshot, next_rev);
        let changed_paths = build_changed_paths(parent.as_ref(), &changed);
        let inherited_mergeinfo = parent
            .as_ref()
            .map(|p| p.mergeinfo.clone())
            .unwrap_or_default();
        let mergeinfo = build_mergeinfo(&pending_merges, inherited_mergeinfo);
        revprops
            .entry("svn:date".to_owned())
            .or_insert_with(|| Utc::now().to_rfc3339());

        let commit = Commit {
            id: id.clone(),
            revision: next_rev,
            parent: parent_id,
            parent_revision,
            author: author.to_owned(),
            message: message.to_owned(),
            created_at: Utc::now(),
            files: snapshot,
            changed_files: changed,
            mergeinfo,
            revprops,
            txn_id: None,
            changed_paths,
        };

        // Commit protocol (the repository lock is held throughout):
        //   1. blobs are already durable (fsync'd by the object store);
        //   2. the commit object is written durably but is not referenced yet;
        //   3. the pre-commit hook may inspect `.vcrs/commits/<id>.json`;
        //   4. one SQLite transaction publishes the revision, moves BASE and
        //      consumes pending merges — this is the single commit point.
        // A crash before (4) leaves only an unreferenced object for gc.
        storage::write_commit(self, &commit)?;
        if let Err(err) = self.run_hook("pre-commit", &[id.as_str()]) {
            let _ = storage::delete_commit(self, &id);
            return Err(err);
        }
        wcdb.record_commit(
            &RevisionRow::from_commit(&commit)?,
            &pending_merges,
            next_rev,
            committed_schedule,
        )?;

        // Working-copy node metadata is derived data: the commit is already
        // durable, and the next status/sync repairs it if this refresh fails.
        let _ = self.sync_wcdb();
        // The revision is published: a failing post-commit hook must not turn
        // a successful commit into an error. Its output is kept for review.
        if let Err(err) = self.run_hook("post-commit", &[&next_rev.to_string(), &id]) {
            self.log_hook_failure(&err);
        }
        Ok(commit)
    }

    /// Commit only a staged subset without touching the working copy.
    /// `staged_full` paths take their current working content (or are deleted if
    /// gone from disk); `staged_partial` maps a path to its partially-applied
    /// text; every other path keeps its HEAD content.
    pub fn commit_selective(
        &self,
        staged_full: &BTreeSet<String>,
        staged_partial: &BTreeMap<String, String>,
        message: &str,
        author: &str,
    ) -> Result<Commit> {
        let _lock = self.lock()?;
        self.ensure_initialized()?;
        let wcdb = self.wcdb()?;
        let schedule = wcdb.schedule()?;
        let mut result: BTreeMap<String, FileEntry> = self
            .base_files(&wcdb)?
            .into_iter()
            .map(|f| (f.path.clone(), f))
            .collect();

        for path in staged_full {
            let scheduled = schedule.get(path).map(|s| s.op);
            if scheduled.is_none() && !result.contains_key(path) {
                return Err(VcsError::NotVersioned(path.clone()));
            }
            let abs = self.abs_path(path)?;
            if scheduled != Some(ScheduleOp::Delete) && fs::symlink_metadata(&abs).is_ok() {
                result.insert(path.clone(), self.working_entry(path, None, true)?);
            } else {
                result.remove(path);
            }
        }
        for (path, text) in staged_partial {
            result.insert(
                path.clone(),
                self.working_entry(path, Some(text.clone().into_bytes()), true)?,
            );
        }
        let committed: Vec<String> = staged_full
            .iter()
            .chain(staged_partial.keys())
            .filter(|p| schedule.contains_key(*p))
            .cloned()
            .collect();

        let snapshot: Vec<FileEntry> = result.into_values().collect();
        let mut revprops = BTreeMap::new();
        revprops.insert("svn:author".to_owned(), author.to_owned());
        revprops.insert("svn:log".to_owned(), message.to_owned());
        self.commit_entries(snapshot, message, author, revprops, &schedule, &committed)
    }

    pub fn log(&self, limit: usize) -> Result<Vec<Commit>> {
        let mut out = Vec::new();
        let mut next = self.head_commit_id()?;

        while let Some(id) = next {
            if out.len() >= limit {
                break;
            }
            let commit = storage::read_commit(self, &id)?;
            next = commit.parent.clone();
            out.push(commit);
        }

        Ok(out)
    }

    pub fn log_range(
        &self,
        range: RevisionRange,
        verbose_paths: bool,
        include_merged: bool,
        only_merged: bool,
    ) -> Result<Vec<Commit>> {
        let merged_revs = self.wcdb()?.merged_revisions_set()?;
        let mut out = Vec::new();
        let mut rev = range.end;
        while rev >= range.start {
            let is_merged = merged_revs.contains(&rev);
            if only_merged && !is_merged {
                if rev == i64::MIN {
                    break;
                }
                rev -= 1;
                continue;
            }
            if !include_merged && !only_merged && is_merged {
                if rev == i64::MIN {
                    break;
                }
                rev -= 1;
                continue;
            }
            if let Ok(mut commit) = self.read_commit_by_revision(rev) {
                if !verbose_paths {
                    commit.changed_files.clear();
                    commit.changed_paths.clear();
                }
                out.push(commit);
            }
            if rev == i64::MIN {
                break;
            }
            rev -= 1;
        }
        Ok(out)
    }

    pub fn status(&self) -> Result<Vec<FileChange>> {
        let _lock = self.lock()?;
        // Single working-copy scan that both refreshes wc.db and returns the
        // change set (the old path scanned the tree twice).
        let working = self.snapshot_working_copy()?;
        self.sync_wcdb_from(&working)
    }

    /// Revert local changes to the BASE revision: restore modified and missing
    /// files and their properties, cancel scheduled deletions, and un-schedule
    /// additions (the files stay on disk, unversioned). Unversioned files are
    /// never touched. `only_paths` limits the revert to those paths or
    /// directories; empty means everything.
    pub fn revert_to_head(&self, only_paths: &[String]) -> Result<Vec<FileChange>> {
        let _lock = self.lock()?;
        let before = self.status()?;
        let wcdb = self.wcdb()?;
        let base_rev = wcdb.base_revision()?;
        let base_commit = if base_rev == 0 {
            None
        } else {
            Some(self.read_commit_by_revision(base_rev)?)
        };
        let base_map: BTreeMap<&str, &FileEntry> = base_commit
            .iter()
            .flat_map(|c| c.files.iter())
            .map(|f| (f.path.as_str(), f))
            .collect();
        let selected =
            |p: &str| only_paths.is_empty() || only_paths.iter().any(|o| path_in_scope(p, o));

        let unschedule: Vec<String> = wcdb
            .schedule()?
            .into_keys()
            .filter(|p| selected(p))
            .collect();
        wcdb.clear_schedule(&unschedule)?;

        let mut reverted = Vec::new();
        for ch in before {
            if !selected(&ch.path) {
                continue;
            }
            if let Some(entry) = base_map.get(ch.path.as_str()) {
                let abs = self.abs_path(&ch.path)?;
                if let Some(parent) = abs.parent() {
                    fs::create_dir_all(parent)?;
                }
                let blob = self.read_blob(&entry.blob_id)?;
                let inherited = wcdb.inherited_props_for_path(&ch.path)?;
                write_entry_to_working(
                    &abs,
                    entry,
                    &blob,
                    &CheckoutMeta {
                        revision: base_commit.as_ref().map(|c| c.revision),
                        author: base_commit.as_ref().map(|c| c.author.as_str()),
                        date: base_commit.as_ref().map(|c| c.created_at),
                        has_lock_token: wcdb.has_lock_token(&ch.path)?,
                        inherited: &inherited,
                    },
                )?;
                wcdb.replace_props_for_path(&ch.path, &entry.props)?;
            }
            reverted.push(ch);
        }
        self.sync_wcdb()?;
        Ok(reverted)
    }

    /// Copy a versioned file, scheduling the copy for addition with history.
    pub fn copy_path(&self, src: &str, dst: &str) -> Result<()> {
        let _lock = self.lock()?;
        let wcdb = self.wcdb()?;
        let origin = self.copy_origin(&wcdb, src)?;
        let (src_abs, dst_abs) = self.copy_endpoints(src, dst)?;
        let md = fs::symlink_metadata(&src_abs)?;
        if md.file_type().is_symlink() {
            try_create_symlink(&dst_abs, &fs::read_link(&src_abs)?.to_string_lossy())?;
        } else {
            fs::copy(&src_abs, &dst_abs)?;
        }
        wcdb.set_schedule(dst, ScheduleOp::Add, origin.as_deref())?;
        self.sync_wcdb()
    }

    /// Move a versioned file: the destination is scheduled for addition with
    /// history and the source for deletion.
    pub fn move_path(&self, src: &str, dst: &str) -> Result<()> {
        let _lock = self.lock()?;
        let wcdb = self.wcdb()?;
        let origin = self.copy_origin(&wcdb, src)?;
        let (src_abs, dst_abs) = self.copy_endpoints(src, dst)?;
        fs::rename(&src_abs, &dst_abs)?;
        wcdb.set_schedule(dst, ScheduleOp::Add, origin.as_deref())?;
        if wcdb
            .schedule()?
            .get(src)
            .is_some_and(|s| s.op == ScheduleOp::Add)
        {
            wcdb.clear_schedule(&[src.to_owned()])?;
        } else {
            wcdb.set_schedule(src, ScheduleOp::Delete, None)?;
        }
        self.sync_wcdb()
    }

    /// The history a copy of `path` records: the path itself when it is in
    /// BASE, the original source when it is itself a scheduled copy.
    fn copy_origin(&self, wcdb: &WcDb, path: &str) -> Result<Option<String>> {
        match wcdb.schedule()?.get(path) {
            Some(s) if s.op == ScheduleOp::Add => Ok(s.copy_from.clone()),
            Some(_) => Err(VcsError::NotVersioned(path.to_owned())),
            None if self.base_files(wcdb)?.iter().any(|f| f.path == path) => {
                Ok(Some(path.to_owned()))
            }
            None => Err(VcsError::NotVersioned(path.to_owned())),
        }
    }

    fn copy_endpoints(&self, src: &str, dst: &str) -> Result<(PathBuf, PathBuf)> {
        let src_abs = self.abs_path(src)?;
        let dst_abs = self.abs_path(dst)?;
        let md =
            fs::symlink_metadata(&src_abs).map_err(|_| VcsError::PathNotFound(src.to_owned()))?;
        if md.is_dir() {
            return Err(VcsError::InvalidPath {
                path: src.to_owned(),
                reason: "copying or moving directories is not supported",
            });
        }
        if fs::symlink_metadata(&dst_abs).is_ok() {
            return Err(VcsError::PathExists(dst.to_owned()));
        }
        if let Some(parent) = dst_abs.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok((src_abs, dst_abs))
    }

    pub fn update_to_revision(&self, revision: &str) -> Result<Vec<FileChange>> {
        self.update_to_revision_with_depth(revision, None)
    }

    pub fn update_to_revision_with_depth(
        &self,
        revision: &str,
        depth: Option<Depth>,
    ) -> Result<Vec<FileChange>> {
        let _lock = self.lock()?;
        if let Some(d) = depth {
            self.set_depth(d)?;
            self.wcdb()?.set_ambient_depth("", depth_to_str(d), true)?;
        }
        let target_rev = self.resolve_revision_spec(revision)?;
        let target = self.read_commit_by_revision(target_rev)?;
        let outcome = self.apply_tree_delta(
            &self.base_files(&self.wcdb()?)?,
            &target.files,
            target_rev,
            None,
            true,
            false,
        )?;
        if depth.is_some() {
            self.prune_working_to_depth()?;
        }
        self.sync_wcdb()?;
        Ok(outcome.changed)
    }

    pub fn commit_changed_files(&self, revision: &str) -> Result<Vec<FileChange>> {
        let rev = self.resolve_revision_spec(revision)?;
        Ok(self.read_commit_by_revision(rev)?.changed_files)
    }

    pub fn commit_changed_paths(&self, revision: &str) -> Result<Vec<ChangedPath>> {
        let rev = self.resolve_revision_spec(revision)?;
        Ok(self.read_commit_by_revision(rev)?.changed_paths)
    }

    pub fn blame_file(&self, path: &str, revision: Option<&str>) -> Result<Vec<BlameLine>> {
        let target_rev = match revision {
            Some(spec) => self.resolve_revision_spec(spec)?,
            None => self.wcdb()?.head_revision()?,
        };
        if target_rev <= 0 {
            return Ok(Vec::new());
        }
        let mut lines = self.blame_file_at_revision(path, target_rev)?;
        self.apply_merge_awareness(path, target_rev, &mut lines)?;
        Ok(lines)
    }

    fn blame_file_at_revision(&self, path: &str, target_rev: i64) -> Result<Vec<BlameLine>> {
        let mut previous_lines: Vec<String> = Vec::new();
        let mut attributions: Vec<BlameLine> = Vec::new();
        let mut seen_any = false;

        for rev in 1..=target_rev {
            let commit = self.read_commit_by_revision(rev)?;
            let Some(file) = commit.files.iter().find(|f| f.path == path) else {
                continue;
            };
            if file.is_binary {
                return Err(VcsError::CommitNotFound(format!(
                    "blame for binary file r{rev}:{path}"
                )));
            }

            let text = String::from_utf8_lossy(&self.read_blob(&file.blob_id)?).to_string();
            let new_lines: Vec<String> = text.lines().map(|s| s.to_owned()).collect();

            if !seen_any {
                if let (Some(src_path), Some(src_rev)) = (&file.copy_from_path, file.copy_from_rev)
                {
                    if src_rev > 0 {
                        let src_blame = self.blame_file_at_revision(src_path, src_rev)?;
                        let src_text = self
                            .file_entry_at_revision(src_rev, src_path)?
                            .map(|e| self.read_blob(&e.blob_id))
                            .transpose()?
                            .map(|b| String::from_utf8_lossy(&b).to_string())
                            .unwrap_or_default();
                        let src_lines: Vec<String> =
                            src_text.lines().map(|s| s.to_owned()).collect();
                        let rebased = apply_diff_to_blame(
                            &src_blame,
                            &src_lines,
                            &new_lines,
                            &commit.author,
                            rev,
                            commit.created_at,
                        );
                        attributions = rebased;
                    } else {
                        attributions = base_blame_for_lines(
                            &new_lines,
                            rev,
                            &commit.author,
                            commit.created_at,
                        );
                    }
                } else {
                    attributions =
                        base_blame_for_lines(&new_lines, rev, &commit.author, commit.created_at);
                }
                previous_lines = new_lines;
                seen_any = true;
                continue;
            }

            attributions = apply_diff_to_blame(
                &attributions,
                &previous_lines,
                &new_lines,
                &commit.author,
                rev,
                commit.created_at,
            );
            previous_lines = new_lines;
        }

        Ok(attributions)
    }

    fn apply_merge_awareness(
        &self,
        path: &str,
        target_rev: i64,
        lines: &mut [BlameLine],
    ) -> Result<()> {
        let edges = self.wcdb()?.merge_edges_up_to(target_rev)?;
        if edges.is_empty() {
            return Ok(());
        }
        for (_target, merged_rev, source_path) in edges {
            let source_file = if source_path == "/" || source_path.is_empty() {
                path.to_owned()
            } else {
                if !path_in_scope(path, &source_path) {
                    continue;
                }
                path.to_owned()
            };
            let Some(entry) = self.file_entry_at_revision(merged_rev, &source_file)? else {
                continue;
            };
            if entry.is_binary {
                continue;
            }
            let text = String::from_utf8_lossy(&self.read_blob(&entry.blob_id)?).to_string();
            let src_lines: Vec<&str> = text.lines().collect();
            let merged_commit = self.read_commit_by_revision(merged_rev)?;
            for (idx, line) in lines.iter_mut().enumerate() {
                let Some(src_line) = src_lines.get(idx).copied() else {
                    continue;
                };
                if line.content == src_line && merged_rev < line.revision {
                    line.revision = merged_rev;
                    line.author = merged_commit.author.clone();
                    line.created_at = merged_commit.created_at;
                }
            }
        }
        Ok(())
    }

    /// Merge the change(s) named by `revision` into the working copy:
    /// `N` is the change made by rN (the delta rN-1 -> rN, i.e. a
    /// cherry-pick), `A:B` the delta from rA to rB (`B:A` with B > A undoes
    /// it); either may be prefixed with `path@` to limit the merge scope.
    /// The result is a local modification to commit; mergeinfo for the merged
    /// revisions is recorded with that commit.
    pub fn merge_from_revision(
        &self,
        revision: &str,
        dry_run: bool,
        record_only: bool,
    ) -> Result<MergeOutcome> {
        let _lock = self.lock()?;
        let (scope_path, left_rev, right_rev) = self.resolve_merge_spec(revision)?;
        let scope = scope_path.as_deref().unwrap_or("/").to_owned();
        // Only forward merges add mergeinfo; undoing a change records nothing.
        let merged_revs: Vec<i64> = (left_rev + 1..=right_rev).collect();

        if record_only {
            // A dry run must not persist anything, even with --record-only.
            if !dry_run {
                let wcdb = self.wcdb()?;
                for rev in &merged_revs {
                    wcdb.add_pending_merge(&scope, *rev)?;
                }
            }
            return Ok(MergeOutcome {
                changed: Vec::new(),
                conflicts: Vec::new(),
            });
        }

        let left = if left_rev == 0 {
            Vec::new()
        } else {
            self.read_commit_by_revision(left_rev)?.files
        };
        let right = if right_rev == 0 {
            Vec::new()
        } else {
            self.read_commit_by_revision(right_rev)?.files
        };
        let outcome = self.apply_tree_delta(
            &left,
            &right,
            right_rev,
            scope_path.as_deref(),
            false,
            dry_run,
        )?;
        if dry_run {
            return Ok(outcome);
        }
        // Only record mergeinfo when the merge applied cleanly; a conflicted
        // merge is not yet integrated and must be resolved + re-evaluated.
        if outcome.conflicts.is_empty() {
            let wcdb = self.wcdb()?;
            for rev in &merged_revs {
                wcdb.add_pending_merge(&scope, *rev)?;
            }
        }
        self.sync_wcdb()?;
        Ok(outcome)
    }

    /// Parse a merge source into `(scope path, left revision, right revision)`.
    fn resolve_merge_spec(&self, spec: &str) -> Result<(Option<String>, i64, i64)> {
        let (path, revs) = match split_peg(spec) {
            Some((path, revs)) => (Some(path.to_owned()), revs),
            None => (None, spec),
        };
        let rev_or_zero = |s: &str| -> Result<i64> {
            if s.trim() == "0" {
                Ok(0)
            } else {
                self.resolve_revision_spec(s)
            }
        };
        let (left, right) = match revs.split_once(':') {
            Some((a, b)) => (rev_or_zero(a)?, rev_or_zero(b)?),
            None => {
                let n = self.resolve_revision_spec(revs)?;
                (n - 1, n)
            }
        };
        if left == right {
            return Err(VcsError::RevisionNotFound(format!(
                "empty merge range '{spec}'"
            )));
        }
        Ok((path, left, right))
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

    pub fn set_property(&self, path: &str, name: &str, value: &str) -> Result<()> {
        let _lock = self.lock()?;
        let wcdb = self.wcdb()?;
        wcdb.set_file_prop(path, name, value)?;
        let abs = self.abs_path(path)?;
        if abs.exists() {
            if name == "svn:executable" {
                set_executable_if_supported(&abs, !value.trim().is_empty())?;
            } else if name == "svn:eol-style"
                && let Ok(text) = fs::read_to_string(&abs)
            {
                fs::write(
                    &abs,
                    apply_eol_style_for_working(&text, Some(&value.to_owned())),
                )?;
            }
        }
        if name == "svn:needs-lock" && !wcdb.has_lock_token(path)? {
            if abs.exists() {
                set_readonly_if_supported(&abs, true)?;
            }
        }
        self.sync_wcdb()
    }

    pub fn set_inherited_property(&self, scope_path: &str, name: &str, value: &str) -> Result<()> {
        let _lock = self.lock()?;
        self.wcdb()?.set_inherited_prop(scope_path, name, value)
    }

    pub fn list_inherited_properties(&self) -> Result<Vec<(String, String, String)>> {
        self.wcdb()?.list_inherited_props()
    }

    pub fn set_changelist(&self, path: &str, changelist: Option<&str>) -> Result<()> {
        let _lock = self.lock()?;
        self.wcdb()?.set_changelist(path, changelist)
    }

    pub fn list_changelists(&self) -> Result<Vec<(String, String)>> {
        self.wcdb()?.list_changelists()
    }

    pub fn set_depth(&self, depth: Depth) -> Result<()> {
        let _lock = self.lock()?;
        self.wcdb()?.set_depth(depth_to_str(depth))
    }

    pub fn depth(&self) -> Result<Depth> {
        Ok(Depth::from_str(&self.wcdb()?.depth()?).unwrap_or(Depth::Infinity))
    }

    /// A property of the working file: explicit properties plus those that
    /// mirror the file itself (`svn:executable`, `svn:special`). Inherited
    /// properties are not node properties and are listed by `iprop-list`.
    pub fn get_property(&self, path: &str, name: &str) -> Result<Option<String>> {
        if let Ok(abs) = self.abs_path(path)
            && fs::symlink_metadata(&abs).is_ok_and(|m| !m.is_dir())
        {
            return Ok(self.working_entry(path, None, false)?.props.remove(name));
        }
        Ok(self.wcdb()?.file_props(path)?.remove(name))
    }

    pub fn del_property(&self, path: &str, name: &str) -> Result<()> {
        let _lock = self.lock()?;
        let wcdb = self.wcdb()?;
        wcdb.delete_file_prop(path, name)?;
        let abs = self.abs_path(path)?;
        if abs.exists() && name == "svn:executable" {
            set_executable_if_supported(&abs, false)?;
        }
        if name == "svn:needs-lock" {
            if abs.exists() {
                set_readonly_if_supported(&abs, false)?;
            }
        }
        self.sync_wcdb()
    }

    pub fn set_lock_token_local(
        &self,
        path: &str,
        token: Option<&str>,
        owner: Option<&str>,
    ) -> Result<()> {
        let _lock = self.lock()?;
        let wcdb = self.wcdb()?;
        match token {
            Some(t) => wcdb.set_lock_token(path, t, owner)?,
            None => wcdb.clear_lock_token(path)?,
        }
        if self.path_requires_lock(path)? {
            let abs = self.abs_path(path)?;
            if abs.exists() {
                set_readonly_if_supported(&abs, token.is_none())?;
            }
        }
        Ok(())
    }

    pub fn clear_local_lock_tokens(&self) -> Result<()> {
        let _lock = self.lock()?;
        self.wcdb()?.clear_all_lock_tokens()
    }

    fn path_requires_lock(&self, path: &str) -> Result<bool> {
        let wc_props = self.wcdb()?.file_props(path)?;
        if has_svn_prop(&wc_props, "svn:needs-lock") {
            return Ok(true);
        }
        if let Some(head) = self.head_commit()?
            && let Some(entry) = head.files.iter().find(|f| f.path == path)
            && has_svn_prop(&entry.props, "svn:needs-lock")
        {
            return Ok(true);
        }
        Ok(false)
    }

    pub fn add_ignore(&self, pattern: &str) -> Result<()> {
        let _lock = self.lock()?;
        self.wcdb()?.add_ignore_rule("", pattern, false)
    }

    pub fn list_ignores(&self) -> Result<Vec<(String, String, bool)>> {
        self.wcdb()?.list_ignore_rules()
    }

    pub fn set_external(
        &self,
        path: &str,
        target_url: &str,
        revision: Option<String>,
    ) -> Result<()> {
        let _lock = self.lock()?;
        self.wcdb()?.set_external(&ExternalDef {
            path: path.to_owned(),
            target_url: target_url.to_owned(),
            revision,
        })
    }

    pub fn list_externals(&self) -> Result<Vec<ExternalDef>> {
        self.wcdb()?.list_externals()
    }

    fn wcdb(&self) -> Result<WcDb> {
        WcDb::open(&self.root)
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

    /// Upgrade metadata written by versions before 0.3: HEAD lived in a file
    /// updated separately from the revision index, with a JSON undo/redo
    /// journal around it. The revision index is rebuilt from that HEAD's
    /// parent chain (the authoritative history) and both files are retired;
    /// leftover commit objects of interrupted transactions are unreferenced and
    /// collected by gc.
    fn migrate_legacy_layout(&self) -> Result<()> {
        let vcrs = self.root.join(VCRS_DIR);
        let head_file = vcrs.join("HEAD");
        if head_file.exists() {
            let head = fs::read_to_string(&head_file)?;
            let head = head.trim();
            if !head.is_empty() {
                self.index_chain(Some(head))?;
            }
            fs::remove_file(&head_file)?;
        }
        let transactions = vcrs.join("transactions");
        if transactions.exists() {
            fs::remove_dir_all(transactions)?;
        }
        Ok(())
    }

    /// Before 0.3 the node-property table was rewritten from the effective
    /// properties on every status, so it accumulated inherited and inferred
    /// values that could then never be removed. Reset it once to the BASE
    /// properties (keeping those of scheduled additions).
    fn migrate_explicit_props(&self, wcdb: &WcDb) -> Result<()> {
        if wcdb.meta_value("props_model")?.as_deref() == Some("explicit") {
            return Ok(());
        }
        let base = self.base_files(wcdb)?;
        let schedule = wcdb.schedule()?;
        for f in &base {
            wcdb.replace_props_for_path(&f.path, &f.props)?;
        }
        wcdb.retain_file_props(&versioned_paths(&base, &schedule))?;
        wcdb.set_meta_value("props_model", "explicit")
    }

    /// Rebuild the revision index (revisions + merge edges) from the parent
    /// chain ending at `head`. Only commits on that chain are indexed, so an
    /// unreferenced object can never shadow a revision number.
    fn index_chain(&self, head: Option<&str>) -> Result<()> {
        let mut chain = Vec::new();
        let mut seen = BTreeSet::new();
        let mut cur = head.map(str::to_owned);
        while let Some(id) = cur {
            if !seen.insert(id.clone()) {
                return Err(VcsError::Protocol(format!("cycle in history at {id}")));
            }
            let c = storage::read_commit(self, &id)?;
            cur = c.parent.clone();
            chain.push(c);
        }
        chain.reverse();

        let mut rows = Vec::with_capacity(chain.len());
        let mut edges = Vec::new();
        let mut prev_mergeinfo = BTreeMap::new();
        for (idx, c) in chain.iter().enumerate() {
            if c.revision != idx as i64 + 1 {
                return Err(VcsError::Protocol(format!(
                    "history is not numbered sequentially: commit {} is r{} at position {}",
                    c.id,
                    c.revision,
                    idx + 1
                )));
            }
            for (path, revs) in &c.mergeinfo {
                let before: BTreeSet<i64> = prev_mergeinfo
                    .get(path)
                    .map(|v: &String| parse_mergeinfo_value(v).into_iter().collect())
                    .unwrap_or_default();
                for merged in parse_mergeinfo_value(revs) {
                    if !before.contains(&merged) {
                        edges.push((c.revision, merged, path.clone()));
                    }
                }
            }
            prev_mergeinfo = c.mergeinfo.clone();
            rows.push(RevisionRow::from_commit(c)?);
        }
        self.wcdb()?.replace_revisions(&rows, &edges)
    }

    /// After the depth was narrowed, remove BASE files that fell out of the
    /// working-copy scope — only unmodified ones. Local modifications, scheduled
    /// changes and unversioned files are never deleted.
    fn prune_working_to_depth(&self) -> Result<()> {
        let wcdb = self.wcdb()?;
        let scope = WcScope::load(&wcdb)?;
        let schedule = wcdb.schedule()?;
        for f in self.base_files(&wcdb)? {
            if scope.contains(&f.path) || schedule.contains_key(&f.path) {
                continue;
            }
            let Ok(abs) = self.abs_path(&f.path) else {
                continue;
            };
            if !fs::symlink_metadata(&abs).is_ok_and(|m| !m.is_dir()) {
                continue;
            }
            if self.working_entry(&f.path, None, false)?.blob_id == f.blob_id {
                remove_path_if_exists(&abs)?;
            }
        }
        Ok(())
    }

    /// Apply the change from tree `left` to tree `right` to the working copy,
    /// three-way against local modifications.
    /// - update: `left` is BASE; afterwards BASE becomes `rev` (`advance_base`);
    /// - merge: `left`/`right` are the merge-source revisions; BASE stays and
    ///   the result is a local modification whose additions and deletions are
    ///   scheduled for the next commit.
    fn apply_tree_delta(
        &self,
        left: &[FileEntry],
        target_files: &[FileEntry],
        rev: i64,
        scope_path: Option<&str>,
        advance_base: bool,
        dry_run: bool,
    ) -> Result<MergeOutcome> {
        let target_meta = self.read_commit_by_revision(rev).ok();
        let wcdb = self.wcdb()?;
        let scope = WcScope::load(&wcdb)?;
        let base_files = left;
        let actual_base = self.base_files(&wcdb)?;
        let base_paths: BTreeSet<&str> = actual_base.iter().map(|f| f.path.as_str()).collect();
        let versioned = versioned_paths(&actual_base, &wcdb.schedule()?);
        let working = self.snapshot_working_copy()?;

        let base_map: BTreeMap<&str, &FileEntry> =
            base_files.iter().map(|f| (f.path.as_str(), f)).collect();
        let working_map: BTreeMap<&str, &FileEntry> =
            working.iter().map(|f| (f.path.as_str(), f)).collect();
        let target_map: BTreeMap<&str, &FileEntry> =
            target_files.iter().map(|f| (f.path.as_str(), f)).collect();

        let mut keys = BTreeSet::new();
        keys.extend(base_map.keys().copied());
        keys.extend(working_map.keys().copied());
        keys.extend(target_map.keys().copied());

        let mut conflicts = Vec::new();
        let mut planned: Vec<FileChange> = Vec::new();

        // The working copy has a single BASE revision, so a path cannot be
        // skipped while BASE moves on: unresolved conflicts must go first.
        if !dry_run {
            let pending = wcdb.conflicts()?;
            if !pending.is_empty() {
                return Err(VcsError::UnresolvedConflicts {
                    paths: pending.into_keys().collect::<Vec<_>>().join(", "),
                });
            }
        }

        for path in &keys {
            if let Some(scope) = scope_path {
                if !path_in_scope(path, scope) {
                    continue;
                }
            }
            if !scope.contains(path) {
                continue;
            }
            let b = base_map.get(path).copied();
            let w = working_map.get(path).copied();
            let t = target_map.get(path).copied();

            let local_changed = changed_entry(b, w);
            let incoming_changed = changed_entry(b, t);

            if !incoming_changed {
                continue;
            }

            // An unversioned file stands where an addition arrives: never
            // overwrite it. Identical content is adopted as is.
            if b.is_none()
                && w.is_none()
                && let Some(te) = t
                && let Ok(abs) = self.abs_path(path)
                && fs::symlink_metadata(&abs).is_ok()
            {
                let identical = fs::symlink_metadata(&abs)?.is_file()
                    && self
                        .working_entry(path, None, false)
                        .is_ok_and(|e| e.blob_id == te.blob_id);
                if !identical {
                    conflicts.push(path.to_string());
                    if dry_run {
                        planned.push(incoming_change(path, b, t));
                    } else {
                        wcdb.set_tree_conflict(path, "local unversioned, incoming add")?;
                    }
                    continue;
                }
                planned.push(incoming_change(path, b, t));
                if !dry_run && !advance_base {
                    wcdb.set_schedule(path, ScheduleOp::Add, None)?;
                }
                continue;
            }

            if local_changed && changed_entry(w, t) {
                if dry_run {
                    conflicts.push(path.to_string());
                    planned.push(incoming_change(path, b, t));
                    continue;
                }
                if self.merge_or_conflict(&wcdb, path, b, w, t, advance_base)? {
                    planned.push(incoming_change(path, b, t));
                } else {
                    conflicts.push(path.to_string());
                }
                continue;
            }

            // no conflict, apply target state
            planned.push(incoming_change(path, b, t));
            if dry_run {
                continue;
            }
            // A merge changes the working copy relative to BASE, so additions
            // and deletions it brings are scheduled for the next commit.
            if !advance_base {
                match t {
                    Some(_) if !versioned.contains(*path) => {
                        wcdb.set_schedule(path, ScheduleOp::Add, None)?;
                    }
                    None if base_paths.contains(*path) => {
                        wcdb.set_schedule(path, ScheduleOp::Delete, None)?;
                    }
                    None => wcdb.clear_schedule(&[path.to_string()])?,
                    _ => {}
                }
            }
            match t {
                Some(te) => {
                    let abs = self.abs_path(path)?;
                    if let Some(parent) = abs.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let raw = self.read_blob(&te.blob_id)?;
                    let inherited = wcdb.inherited_props_for_path(path)?;
                    write_entry_to_working(
                        &abs,
                        te,
                        &raw,
                        &CheckoutMeta {
                            revision: target_meta.as_ref().map(|c| c.revision),
                            author: target_meta.as_ref().map(|c| c.author.as_str()),
                            date: target_meta.as_ref().map(|c| c.created_at),
                            has_lock_token: wcdb.has_lock_token(path)?,
                            inherited: &inherited,
                        },
                    )?;
                    // Local properties were unmodified (or already equal), so
                    // the incoming ones become the working properties.
                    wcdb.replace_props_for_path(path, &te.props)?;
                }
                None => {
                    remove_path_if_exists(&self.abs_path(path)?)?;
                    wcdb.replace_props_for_path(path, &BTreeMap::new())?;
                }
            }
        }

        if dry_run {
            return Ok(MergeOutcome {
                changed: planned,
                conflicts,
            });
        }

        // BASE moves even when conflicts were recorded (as in svn): conflicted
        // paths carry their merge state and block commits until resolved.
        if advance_base {
            wcdb.set_base_revision(rev)?;
            reconcile_schedule(&wcdb, target_files)?;
        }

        Ok(MergeOutcome {
            changed: planned,
            conflicts,
        })
    }

    /// Combine a local change `w` and an incoming change `t` (both relative to
    /// `b`). Text content is merged three-way: a clean merge is written and
    /// returns `true`; overlapping edits leave conflict markers plus
    /// `.mine`/`.rOLD`/`.rNEW` artifacts. Binary content keeps the local file
    /// with the same artifacts, and structural clashes (edit vs delete, ...)
    /// become tree conflicts. Every conflict is recorded in wc.db and blocks
    /// commits of that path until `resolve`.
    fn merge_or_conflict(
        &self,
        wcdb: &WcDb,
        path: &str,
        b: Option<&FileEntry>,
        w: Option<&FileEntry>,
        t: Option<&FileEntry>,
        advance_base: bool,
    ) -> Result<bool> {
        let abs = self.abs_path(path)?;
        match (w, t) {
            (Some(we), Some(te)) => {
                // Properties merge key by key, independently of the content.
                let empty = BTreeMap::new();
                let (props, prop_conflicts) =
                    merge_props(b.map_or(&empty, |x| &x.props), &we.props, &te.props);
                wcdb.replace_props_for_path(path, &props)?;
                let prop_conflict = !prop_conflicts.is_empty();
                if prop_conflict {
                    wcdb.set_tree_conflict(
                        path,
                        &format!("property conflict: {}", prop_conflicts.join(", ")),
                    )?;
                }
                if we.blob_id == te.blob_id {
                    sync_executable_bit(&abs, &props)?;
                    return Ok(!prop_conflict);
                }
                let base = match b {
                    Some(be) => self.read_blob(&be.blob_id)?,
                    None => Vec::new(),
                };
                let mine = self.normalized_working_bytes(path)?;
                let theirs = self.read_blob(&te.blob_id)?;
                let binary = we.is_binary || te.is_binary || b.is_some_and(|x| x.is_binary);
                if !binary
                    && let (Ok(base_s), Ok(mine_s), Ok(theirs_s)) = (
                        std::str::from_utf8(&base),
                        std::str::from_utf8(&mine),
                        std::str::from_utf8(&theirs),
                    )
                {
                    let (merged, clean) = three_way_merge_text(base_s, mine_s, theirs_s);
                    if clean {
                        remove_path_if_exists(&abs)?;
                        fs::write(&abs, merged.as_bytes())?;
                        sync_executable_bit(&abs, &props)?;
                        return Ok(!prop_conflict);
                    }
                    self.write_conflict_artifacts(path, &abs, &base, &mine, &theirs)?;
                    remove_path_if_exists(&abs)?;
                    fs::write(&abs, merged.as_bytes())?;
                    sync_executable_bit(&abs, &props)?;
                } else {
                    // Binary: the working file keeps the local version.
                    self.write_conflict_artifacts(path, &abs, &base, &mine, &theirs)?;
                }
                wcdb.set_text_conflict(
                    path,
                    &format!("{path}.rOLD"),
                    &format!("{path}.rNEW"),
                    &format!("{path}.mine"),
                )?;
                Ok(false)
            }
            (Some(_), None) => {
                // Local edit, incoming delete: keep the edited file versioned.
                if advance_base {
                    wcdb.set_schedule(path, ScheduleOp::Add, None)?;
                }
                wcdb.set_tree_conflict(path, "local edit, incoming delete")?;
                Ok(false)
            }
            (None, _) => {
                let reason = classify_tree_conflict(op_kind(b, w), op_kind(b, t));
                wcdb.set_tree_conflict(path, &reason)?;
                Ok(false)
            }
        }
    }

    fn write_conflict_artifacts(
        &self,
        path: &str,
        abs: &Path,
        base: &[u8],
        mine: &[u8],
        theirs: &[u8],
    ) -> Result<()> {
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        // Artifact names derive from a validated path; re-validate the full
        // sibling names anyway before writing.
        for suffix in [".mine", ".rOLD", ".rNEW"] {
            crate::path::validate_rel_path(&format!("{path}{suffix}"))?;
        }
        write_sibling(abs, ".mine", mine)?;
        write_sibling(abs, ".rOLD", base)?;
        write_sibling(abs, ".rNEW", theirs)?;
        Ok(())
    }

    /// Unresolved conflicts recorded in the working copy.
    pub fn conflicts(&self) -> Result<BTreeMap<String, ConflictRecord>> {
        self.wcdb()?.conflicts()
    }

    /// Mark conflicts on `paths` (all when empty) as resolved, choosing the
    /// content to keep, and delete the conflict artifacts.
    pub fn resolve(&self, paths: &[String], accept: ResolveAccept) -> Result<Vec<String>> {
        let _lock = self.lock()?;
        let wcdb = self.wcdb()?;
        let conflicts = wcdb.conflicts()?;
        let selected: Vec<(&String, &ConflictRecord)> = conflicts
            .iter()
            .filter(|(p, _)| paths.is_empty() || paths.iter().any(|o| path_in_scope(p, o)))
            .collect();
        if selected.is_empty() && !paths.is_empty() {
            return Err(VcsError::NotVersioned(paths.join(", ")));
        }
        let mut resolved = Vec::new();
        for (path, record) in selected {
            let artifacts = [&record.mine_file, &record.old_file, &record.new_file];
            let chosen = match accept {
                ResolveAccept::Working => None,
                ResolveAccept::MineFull => record.mine_file.as_ref(),
                ResolveAccept::TheirsFull => record.new_file.as_ref(),
                ResolveAccept::Base => record.old_file.as_ref(),
            };
            if accept != ResolveAccept::Working {
                let Some(source) = chosen else {
                    return Err(VcsError::TreeConflict {
                        path: path.clone(),
                        reason: format!(
                            "{}; only --accept working applies to tree conflicts",
                            record.reason.as_deref().unwrap_or("tree conflict")
                        ),
                    });
                };
                let content = fs::read(self.abs_path(source)?)?;
                let abs = self.abs_path(path)?;
                remove_path_if_exists(&abs)?;
                fs::write(&abs, content)?;
            }
            for artifact in artifacts.into_iter().flatten() {
                if let Ok(abs) = self.abs_path(artifact) {
                    remove_path_if_exists(&abs)?;
                }
            }
            wcdb.clear_conflict(path)?;
            resolved.push(path.clone());
        }
        self.sync_wcdb()?;
        Ok(resolved)
    }

    fn sync_wcdb(&self) -> Result<()> {
        let working = self.snapshot_working_copy()?;
        self.sync_wcdb_from(&working)?;
        Ok(())
    }

    /// Refresh wc.db node/prop tables from an already-computed working snapshot
    /// and return the change set relative to the base revision.
    fn sync_wcdb_from(&self, working: &[FileEntry]) -> Result<Vec<FileChange>> {
        let wcdb = self.wcdb()?;
        let schedule = wcdb.schedule()?;
        let scope = WcScope::load(&wcdb)?;
        // BASE files outside the working-copy depth are not materialized and
        // must not be reported as deleted.
        let base_files: Vec<FileEntry> = self
            .base_files(&wcdb)?
            .into_iter()
            .filter(|f| scope.contains(&f.path) || schedule.contains_key(&f.path))
            .collect();
        let mut changes = compute_changed_files(Some(&base_files), working);
        apply_scheduled_copies(&mut changes, &schedule);
        mark_conflicts(&mut changes, &wcdb.conflicts()?);
        wcdb.replace_nodes(&base_files, working, &changes)?;
        // Explicit properties are owned by prop-set/update/revert; only drop
        // the ones of paths that left version control.
        wcdb.retain_file_props(&versioned_paths(&self.base_files(&wcdb)?, &schedule))?;
        Ok(changes)
    }

    fn collect_ignore_patterns(&self, wcdb: &WcDb) -> Result<Vec<String>> {
        let mut patterns = vec![
            ".vcrs/**".to_owned(),
            ".git/**".to_owned(),
            "target/**".to_owned(),
        ];

        for (_scope, p, _inherited) in wcdb.list_ignore_rules()? {
            patterns.push(p);
        }

        let ignore_file = self.root.join(".vcrsignore");
        if ignore_file.exists() {
            for line in fs::read_to_string(ignore_file)?.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    patterns.push(trimmed.to_owned());
                }
            }
        }

        Ok(patterns)
    }
}

/// Which BASE paths the working copy materializes (sparse depth, externals).
struct WcScope {
    depth: Depth,
    ambient: BTreeMap<String, (String, bool)>,
    externals: BTreeSet<String>,
}

impl WcScope {
    fn load(wcdb: &WcDb) -> Result<Self> {
        Ok(Self {
            depth: Depth::from_str(&wcdb.depth()?).unwrap_or(Depth::Infinity),
            ambient: wcdb.ambient_depth_map()?,
            externals: wcdb.list_externals()?.into_iter().map(|e| e.path).collect(),
        })
    }

    fn contains(&self, path: &str) -> bool {
        !is_under_external(path, &self.externals)
            && path_allowed_by_ambient_depth(path, self.depth, &self.ambient)
    }
}

/// The versioned set: BASE paths not scheduled for deletion plus scheduled
/// additions.
fn versioned_paths(base: &[FileEntry], schedule: &BTreeMap<String, Scheduled>) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = base
        .iter()
        .filter(|f| {
            schedule
                .get(&f.path)
                .is_none_or(|s| s.op != ScheduleOp::Delete)
        })
        .map(|f| f.path.clone())
        .collect();
    out.extend(
        schedule
            .iter()
            .filter(|(_, s)| s.op == ScheduleOp::Add)
            .map(|(p, _)| p.clone()),
    );
    out
}

/// After BASE moved, drop schedule entries the new BASE already satisfies:
/// additions that now exist and deletions that are gone.
fn reconcile_schedule(wcdb: &WcDb, new_base: &[FileEntry]) -> Result<()> {
    let present: BTreeSet<&str> = new_base.iter().map(|f| f.path.as_str()).collect();
    let stale: Vec<String> = wcdb
        .schedule()?
        .into_iter()
        .filter(|(p, s)| (s.op == ScheduleOp::Add) == present.contains(p.as_str()))
        .map(|(p, _)| p)
        .collect();
    wcdb.clear_schedule(&stale)
}

/// Flag conflicted paths in a change set; a conflicted path without a content
/// change (e.g. a tree conflict whose file matches BASE) is still listed.
fn mark_conflicts(changes: &mut Vec<FileChange>, conflicts: &BTreeMap<String, ConflictRecord>) {
    for ch in changes.iter_mut() {
        ch.conflicted = conflicts.contains_key(&ch.path);
    }
    for path in conflicts.keys() {
        if !changes.iter().any(|c| &c.path == path) {
            changes.push(FileChange {
                path: path.clone(),
                kind: ChangeKind::Modified,
                text_modified: false,
                props_modified: false,
                is_binary: false,
                copy_from: None,
                moved_from: None,
                moved_to: None,
                conflicted: true,
            });
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
}

/// Explicit copy/move records (from `copy`/`move`) win over the content-based
/// rename/copy heuristics.
fn apply_scheduled_copies(changes: &mut [FileChange], schedule: &BTreeMap<String, Scheduled>) {
    let deleted: BTreeSet<String> = changes
        .iter()
        .filter(|c| c.kind == ChangeKind::Deleted)
        .map(|c| c.path.clone())
        .collect();
    let mut moves = Vec::new();
    for ch in changes.iter_mut() {
        if ch.kind == ChangeKind::Deleted {
            continue;
        }
        // Added: a copy/move; Modified: a BASE path deleted and replaced by a copy.
        if let Some(src) = schedule.get(&ch.path).and_then(|s| s.copy_from.clone()) {
            ch.moved_from = deleted.contains(&src).then(|| src.clone());
            if ch.moved_from.is_some() {
                moves.push((src.clone(), ch.path.clone()));
            }
            ch.copy_from = Some(src);
        }
    }
    for (from, to) in moves {
        if let Some(del) = changes
            .iter_mut()
            .find(|c| c.kind == ChangeKind::Deleted && c.path == from)
        {
            del.moved_to = Some(to);
        }
    }
}

pub(crate) fn compute_changed_files(
    base: Option<&[FileEntry]>,
    now: &[FileEntry],
) -> Vec<FileChange> {
    let empty: &[FileEntry] = &[];
    let base = base.unwrap_or(empty);

    let base_map: BTreeMap<&str, &FileEntry> = base.iter().map(|f| (f.path.as_str(), f)).collect();
    let now_map: BTreeMap<&str, &FileEntry> = now.iter().map(|f| (f.path.as_str(), f)).collect();

    let mut keys: BTreeSet<&str> = BTreeSet::new();
    keys.extend(base_map.keys().copied());
    keys.extend(now_map.keys().copied());

    let mut out = Vec::new();
    let mut added_by_blob: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut deleted_by_blob: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut deleted_paths: BTreeSet<String> = BTreeSet::new();

    for path in keys {
        match (base_map.get(path), now_map.get(path)) {
            (None, Some(newf)) => {
                let idx = out.len();
                out.push(FileChange {
                    path: path.to_owned(),
                    kind: ChangeKind::Added,
                    text_modified: true,
                    props_modified: false,
                    is_binary: newf.is_binary,
                    copy_from: None,
                    moved_from: None,
                    moved_to: None,
                    conflicted: false,
                });
                added_by_blob
                    .entry(newf.blob_id.clone())
                    .or_default()
                    .push(idx);
            }
            (Some(oldf), None) => {
                let idx = out.len();
                out.push(FileChange {
                    path: path.to_owned(),
                    kind: ChangeKind::Deleted,
                    text_modified: true,
                    props_modified: false,
                    is_binary: oldf.is_binary,
                    copy_from: None,
                    moved_from: None,
                    moved_to: None,
                    conflicted: false,
                });
                deleted_by_blob
                    .entry(oldf.blob_id.clone())
                    .or_default()
                    .push(idx);
                deleted_paths.insert(path.to_owned());
            }
            (Some(oldf), Some(newf)) => {
                let text_modified = oldf.blob_id != newf.blob_id;
                let props_modified = oldf.props != newf.props || oldf.executable != newf.executable;
                if text_modified || props_modified {
                    out.push(FileChange {
                        path: path.to_owned(),
                        kind: ChangeKind::Modified,
                        text_modified,
                        props_modified,
                        is_binary: oldf.is_binary || newf.is_binary,
                        copy_from: None,
                        moved_from: None,
                        moved_to: None,
                        conflicted: false,
                    });
                }
            }
            (None, None) => {}
        }
    }

    // Rename detection: only when the content is unique on BOTH sides (exactly
    // one add and one delete sharing the blob) and non-empty. Pairing ambiguous
    // duplicates (e.g. several empty files) produces bogus "moved from" results.
    let empty = empty_blob_id();
    for (blob_id, add_indices) in &added_by_blob {
        if *blob_id == empty {
            continue;
        }
        if let Some(del_indices) = deleted_by_blob.get(blob_id)
            && add_indices.len() == 1
            && del_indices.len() == 1
        {
            let add_idx = add_indices[0];
            let del_idx = del_indices[0];
            let from_path = out[del_idx].path.clone();
            let to_path = out[add_idx].path.clone();

            out[add_idx].moved_from = Some(from_path.clone());
            out[add_idx].copy_from = Some(from_path.clone());
            out[del_idx].moved_to = Some(to_path);
        }
    }

    let mut base_by_blob: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for f in base {
        base_by_blob
            .entry(f.blob_id.as_str())
            .or_default()
            .push(f.path.as_str());
    }

    // Copy detection: only when exactly one surviving base file has the new
    // file's content (and it is non-empty). Multiple candidates are ambiguous.
    for change in &mut out {
        if change.kind != ChangeKind::Added || change.copy_from.is_some() {
            continue;
        }

        let Some(newf) = now_map.get(change.path.as_str()) else {
            continue;
        };
        if newf.blob_id == empty {
            continue;
        }

        if let Some(candidates) = base_by_blob.get(newf.blob_id.as_str()) {
            let live: Vec<&str> = candidates
                .iter()
                .copied()
                .filter(|p| !deleted_paths.contains(*p) && *p != change.path)
                .collect();
            if live.len() == 1 {
                change.copy_from = Some(live[0].to_owned());
            }
        }
    }

    out
}

/// blake3 hash of empty content — used to exclude trivial/empty files from
/// rename/copy heuristics.
fn empty_blob_id() -> String {
    blake3::hash(&[]).to_hex().to_string()
}

fn build_changed_paths(parent: Option<&Commit>, changes: &[FileChange]) -> Vec<ChangedPath> {
    let parent_rev = parent.map(|p| p.revision);
    let mut out = Vec::new();
    for ch in changes {
        let action = match ch.kind {
            // An addition with history is still an addition (svn "A +"); a
            // replacement is a path deleted and re-added from another source.
            ChangeKind::Added => ChangedPathAction::Add,
            ChangeKind::Modified if ch.copy_from.is_some() => ChangedPathAction::Replace,
            ChangeKind::Modified => ChangedPathAction::Modify,
            ChangeKind::Deleted => ChangedPathAction::Delete,
        };
        let copyfrom_rev = if ch.copy_from.is_some() {
            parent_rev
        } else {
            None
        };
        let copyfrom_path = ch.copy_from.clone();
        let props_modified = ch.props_modified;
        let text_modified = ch.text_modified;
        let path = ch.path.clone();
        out.push(ChangedPath {
            path,
            action,
            copyfrom_path,
            copyfrom_rev,
            text_modified,
            props_modified,
        });
    }
    out
}

fn assign_node_identity(parent: Option<&Commit>, snapshot: &mut [FileEntry], next_rev: i64) {
    let parent_map: BTreeMap<&str, &FileEntry> = parent
        .map(|p| p.files.iter().map(|f| (f.path.as_str(), f)).collect())
        .unwrap_or_default();

    for f in snapshot {
        if let Some(prev) = parent_map.get(f.path.as_str()) {
            f.node_id = prev.node_id.clone().or_else(|| Some(prev.blob_id.clone()));
            f.copy_id = prev.copy_id.clone().or_else(|| f.node_id.clone());
            f.created_rev = prev.created_rev.or(Some(next_rev - 1));
            continue;
        }
        if let Some(src) = &f.copy_from_path
            && let Some(src_prev) = parent_map.get(src.as_str())
        {
            let src_node = src_prev
                .node_id
                .clone()
                .unwrap_or_else(|| src_prev.blob_id.clone());
            let src_copy = src_prev.copy_id.clone().unwrap_or_else(|| src_node.clone());
            f.copy_id = Some(src_copy);
            f.node_id = Some(
                blake3::hash(format!("{src_node}:{next_rev}:{}", f.path).as_bytes())
                    .to_hex()
                    .to_string(),
            );
            f.created_rev = Some(next_rev);
            continue;
        }
        let seed = format!("{}:{next_rev}:{}", f.path, f.blob_id);
        let node = blake3::hash(seed.as_bytes()).to_hex().to_string();
        f.node_id = Some(node.clone());
        f.copy_id = Some(node);
        f.created_rev = Some(next_rev);
    }
}

/// Write a conflict artifact next to `abs` (e.g. `file.mine`), replacing any
/// existing entry without following a symbolic link planted at that name.
fn write_sibling(abs: &Path, suffix: &str, content: &[u8]) -> Result<String> {
    let mut name = abs.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    let target = abs.with_file_name(name);
    remove_path_if_exists(&target)?;
    fs::write(&target, content)?;
    Ok(target.display().to_string())
}

fn changed_entry(a: Option<&FileEntry>, b: Option<&FileEntry>) -> bool {
    match (a, b) {
        (None, None) => false,
        (Some(_), None) | (None, Some(_)) => true,
        (Some(a), Some(b)) => {
            a.blob_id != b.blob_id || a.props != b.props || a.executable != b.executable
        }
    }
}

/// Three-way text merge: `(merged, true)` when the edits do not overlap,
/// `(text with conflict markers, false)` otherwise.
fn three_way_merge_text(base: &str, ours: &str, theirs: &str) -> (String, bool) {
    match diffy_merge(base, ours, theirs) {
        Ok(text) => (text, true),
        Err(conflict) => (conflict, false),
    }
}

/// Describe the incoming change a merge/update would apply for one path (base ->
/// target), used to report `merge --dry-run` without touching the working copy.
fn incoming_change(path: &str, b: Option<&FileEntry>, t: Option<&FileEntry>) -> FileChange {
    let kind = match (b, t) {
        (None, Some(_)) => ChangeKind::Added,
        (Some(_), None) => ChangeKind::Deleted,
        _ => ChangeKind::Modified,
    };
    let text_modified = match (b, t) {
        (Some(bb), Some(tt)) => bb.blob_id != tt.blob_id,
        _ => true,
    };
    let props_modified = match (b, t) {
        (Some(bb), Some(tt)) => bb.props != tt.props || bb.executable != tt.executable,
        _ => false,
    };
    let is_binary = t.or(b).is_some_and(|e| e.is_binary);
    FileChange {
        path: path.to_owned(),
        kind,
        text_modified,
        props_modified,
        is_binary,
        copy_from: None,
        moved_from: None,
        moved_to: None,
        conflicted: false,
    }
}

fn build_ignore_globset(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        if let Ok(glob) = Glob::new(p) {
            builder.add(glob);
        }
    }
    builder.build().unwrap_or_else(|_| GlobSet::empty())
}

/// True when the path is a directory we never track, so WalkDir can prune the
/// whole subtree instead of stat-ing every blob/build artifact.
fn is_excluded_path(path: &Path, root: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let Some(first) = rel.components().next().and_then(|c| c.as_os_str().to_str()) else {
        return false;
    };
    // Metadata directories are pruned at any depth (nested checkouts); the
    // build directory only at the top level.
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    is_reserved_component(name) || first == "target"
}

/// Resolve inherited properties for `path` from the pre-loaded scope map,
/// shallow scopes first so deeper scopes win (matching `inherited_props_for_path`).
fn resolve_inherited(
    path: &str,
    all_inherited: &BTreeMap<String, BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if all_inherited.is_empty() {
        return out;
    }
    let mut current = String::new();
    let mut scopes = vec![String::new()];
    for seg in path.split('/') {
        if current.is_empty() {
            current.push_str(seg);
        } else {
            current.push('/');
            current.push_str(seg);
        }
        scopes.push(current.clone());
    }
    for scope in scopes {
        if let Some(props) = all_inherited.get(&scope) {
            for (k, v) in props {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

/// Apply repository-form normalization (eol, keyword contraction, symlink/binary
/// handling) to raw working bytes, returning the normalized bytes, the node
/// property set, and whether the content is treated as binary.
///
/// Node properties are the explicit ones, except that `svn:executable` and
/// `svn:special` mirror the file itself (on Unix, where the filesystem can
/// express them): `chmod -x` removes `svn:executable`, replacing a symlink by
/// a file removes `svn:special`. Inherited properties steer normalization but
/// are never recorded on the node.
fn repo_form(
    mut raw: Vec<u8>,
    is_symlink: bool,
    file_props: &BTreeMap<String, String>,
    inherited: &BTreeMap<String, String>,
    executable: bool,
) -> (Vec<u8>, BTreeMap<String, String>, bool) {
    let detected_binary = is_binary_content(&raw);
    let mut props = file_props.clone();
    if cfg!(unix) {
        if executable {
            props.insert("svn:executable".to_owned(), "*".to_owned());
        } else {
            props.remove("svn:executable");
        }
    }
    if is_symlink {
        props.insert("svn:special".to_owned(), "*".to_owned());
    } else if cfg!(unix) {
        props.remove("svn:special");
    }
    let effective = with_inherited(&props, inherited);
    let is_binary = effective_is_binary(detected_binary, &effective);
    // Only normalize line endings when svn:eol-style is set; without it,
    // content is stored byte-for-byte (svn semantics).
    if !is_binary
        && !has_svn_prop(&effective, "svn:special")
        && has_svn_prop(&effective, "svn:eol-style")
        && let Ok(text) = std::str::from_utf8(&raw)
    {
        raw = normalize_eol(text, effective.get("svn:eol-style")).into_bytes();
    }
    if has_svn_prop(&effective, "svn:keywords")
        && !has_svn_prop(&effective, "svn:special")
        && let Ok(text) = std::str::from_utf8(&raw)
    {
        raw = contract_keywords(text, &effective).into_bytes();
    }
    (raw, props, is_binary)
}

/// Node properties overlaid with inherited ones (node properties win).
fn with_inherited(
    props: &BTreeMap<String, String>,
    inherited: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = props.clone();
    for (k, v) in inherited {
        out.entry(k.clone()).or_insert_with(|| v.clone());
    }
    out
}

/// Three-way merge of property maps, key by key. Returns the merged map and
/// the keys changed differently on both sides (the local value is kept).
fn merge_props(
    base: &BTreeMap<String, String>,
    mine: &BTreeMap<String, String>,
    theirs: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, Vec<String>) {
    let keys: BTreeSet<&String> = base
        .keys()
        .chain(mine.keys())
        .chain(theirs.keys())
        .collect();
    let mut merged = BTreeMap::new();
    let mut conflicts = Vec::new();
    for key in keys {
        let (b, m, t) = (base.get(key), mine.get(key), theirs.get(key));
        let value = if m == t || t == b {
            m
        } else if m == b {
            t
        } else {
            conflicts.push(key.clone());
            m
        };
        if let Some(v) = value {
            merged.insert(key.clone(), v.clone());
        }
    }
    (merged, conflicts)
}

/// Make the executable bit of a (non-symlink) working file match its
/// `svn:executable` property.
fn sync_executable_bit(abs: &Path, props: &BTreeMap<String, String>) -> Result<()> {
    if fs::symlink_metadata(abs).is_ok_and(|m| m.is_file()) {
        set_executable_if_supported(abs, has_svn_prop(props, "svn:executable"))?;
    }
    Ok(())
}

/// Heuristic detection of unresolved SVN conflict markers in committed content.
fn has_conflict_markers(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut start = false;
    let mut end = false;
    for line in text.lines() {
        if line.starts_with("<<<<<<<") {
            start = true;
        } else if line.starts_with(">>>>>>>") {
            end = true;
        }
    }
    start && end
}

fn is_under_external(path: &str, external_paths: &BTreeSet<String>) -> bool {
    external_paths
        .iter()
        .any(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")))
}

/// Number of leading bytes inspected for binary detection (svn uses a similar
/// small window rather than scanning whole files).
const BINARY_SNIFF_LEN: usize = 8192;

fn is_binary_content(bytes: &[u8]) -> bool {
    let n = bytes.len().min(BINARY_SNIFF_LEN);
    let window = &bytes[..n];
    if window.contains(&0) {
        return true;
    }
    match std::str::from_utf8(window) {
        Ok(_) => false,
        // When the file is larger than the window, a None error_len means the
        // window was cut mid-character — that is a truncation artifact, not a
        // binary byte, so treat it as text.
        Err(e) => !(n < bytes.len() && e.error_len().is_none()),
    }
}

fn has_svn_prop(props: &BTreeMap<String, String>, name: &str) -> bool {
    props.get(name).is_some_and(|v| !v.trim().is_empty())
}

/// Revision metadata and working-copy state needed to materialize a file.
struct CheckoutMeta<'a> {
    revision: Option<i64>,
    author: Option<&'a str>,
    date: Option<DateTime<Utc>>,
    has_lock_token: bool,
    /// Inherited properties of the path (normalization only).
    inherited: &'a BTreeMap<String, String>,
}

fn write_entry_to_working(
    abs: &Path,
    entry: &FileEntry,
    raw: &[u8],
    meta: &CheckoutMeta<'_>,
) -> Result<()> {
    remove_path_if_exists(abs)?;
    let props = with_inherited(&entry.props, meta.inherited);
    if has_svn_prop(&props, "svn:special")
        && let Ok(text) = std::str::from_utf8(raw)
        && let Some(target) = text.strip_prefix("link ")
        && try_create_symlink(abs, target.trim()).is_ok()
    {
        return Ok(());
    }

    let mut out = raw.to_vec();
    let effective_binary = effective_is_binary(entry.is_binary, &props);
    if !effective_binary
        && !has_svn_prop(&props, "svn:special")
        && has_svn_prop(&props, "svn:eol-style")
        && let Ok(text) = std::str::from_utf8(&out)
    {
        out = apply_eol_style_for_working(text, props.get("svn:eol-style")).into_bytes();
    }
    if has_svn_prop(&props, "svn:keywords")
        && let Ok(text) = std::str::from_utf8(&out)
    {
        out = expand_keywords(
            text,
            &props,
            meta.revision.unwrap_or(0),
            meta.author.unwrap_or("unknown"),
            meta.date.unwrap_or_else(Utc::now),
        )
        .into_bytes();
    }
    fs::write(abs, out)?;
    set_executable_if_supported(abs, entry.executable)?;
    if has_svn_prop(&props, "svn:needs-lock") && !meta.has_lock_token {
        set_readonly_if_supported(abs, true)?;
    }
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    // symlink_metadata, not exists(): a dangling symlink must be removed too,
    // otherwise the following write would follow it outside the working copy.
    let Ok(md) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if md.file_type().is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn keyword_names(props: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(value) = props.get("svn:keywords") {
        for name in value.split(|c: char| c.is_whitespace() || c == ',') {
            let n = name.trim();
            if !n.is_empty() {
                out.insert(n.to_owned());
            }
        }
    }
    out
}

fn contract_keywords(text: &str, props: &BTreeMap<String, String>) -> String {
    let mut out = text.to_owned();
    for k in keyword_names(props) {
        out = collapse_keyword(&out, &k);
    }
    out
}

fn effective_is_binary(detected_binary: bool, props: &BTreeMap<String, String>) -> bool {
    if let Some(mt) = props.get("svn:mime-type") {
        let mt = mt.trim().to_ascii_lowercase();
        if mt.starts_with("text/") || mt == "application/xml" || mt == "application/json" {
            return false;
        }
        if !mt.is_empty() {
            return true;
        }
    }
    detected_binary
}

fn normalize_eol(text: &str, eol_style: Option<&String>) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    match eol_style.map(|s| s.as_str()) {
        Some("CRLF") | Some("crlf") => normalized.replace('\n', "\r\n"),
        Some("CR") | Some("cr") => normalized.replace('\n', "\r"),
        _ => normalized,
    }
}

fn apply_eol_style_for_working(text: &str, eol_style: Option<&String>) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    match eol_style.map(|s| s.as_str()) {
        Some("LF") | Some("lf") => normalized,
        Some("CRLF") | Some("crlf") => normalized.replace('\n', "\r\n"),
        Some("CR") | Some("cr") => normalized.replace('\n', "\r"),
        Some("native") | Some("NATIVE") => {
            #[cfg(windows)]
            {
                normalized.replace('\n', "\r\n")
            }
            #[cfg(not(windows))]
            {
                normalized
            }
        }
        _ => normalized,
    }
}

/// Longest expanded keyword svn recognizes (`$Name: value $`), in bytes.
const MAX_KEYWORD_LEN: usize = 255;

/// Contract every expanded `$Name: ... $` back to `$Name$`. The closing `$`
/// must be on the same line and within [`MAX_KEYWORD_LEN`] bytes (svn's
/// rule); anything else is ordinary text and is left untouched, so a stray
/// `$Rev:` can never swallow the content up to some later `$`.
fn collapse_keyword(input: &str, name: &str) -> String {
    let needle = format!("${name}:");
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while let Some(pos) = input[i..].find(&needle) {
        let start = i + pos;
        let after = start + needle.len();
        let line_end = input[after..].find('\n').map_or(input.len(), |n| after + n);
        let closing = input[after..line_end].find('$').map(|n| after + n);
        match closing {
            Some(end) if end + 1 - start <= MAX_KEYWORD_LEN => {
                out.push_str(&input[i..start]);
                out.push_str(&format!("${name}$"));
                i = end + 1;
            }
            _ => {
                out.push_str(&input[i..after]);
                i = after;
            }
        }
    }
    out.push_str(&input[i..]);
    out
}

fn expand_keywords(
    text: &str,
    props: &BTreeMap<String, String>,
    rev: i64,
    author: &str,
    date: DateTime<Utc>,
) -> String {
    let mut out = text.to_owned();
    for key in keyword_names(props) {
        let value = match key.as_str() {
            "Rev" => rev.to_string(),
            "Author" => author.to_owned(),
            "Date" => date.to_rfc3339(),
            "Id" => format!("{rev} {author} {}", date.to_rfc3339()),
            _ => continue,
        };
        out = out.replace(&format!("${key}$"), &format!("${key}: {value} $"));
    }
    out
}

#[cfg(unix)]
fn try_create_symlink(path: &Path, target: &str) -> Result<()> {
    std::os::unix::fs::symlink(target, path)?;
    Ok(())
}

#[cfg(windows)]
fn try_create_symlink(path: &Path, target: &str) -> Result<()> {
    let target_path = Path::new(target);
    let md = fs::metadata(target_path).ok();
    if md.as_ref().is_some_and(|m| m.is_dir()) {
        std::os::windows::fs::symlink_dir(target, path)?;
    } else {
        std::os::windows::fs::symlink_file(target, path)?;
    }
    Ok(())
}

fn set_readonly_if_supported(path: &Path, readonly: bool) -> Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(readonly);
    fs::set_permissions(path, perms)?;
    Ok(())
}

fn split_peg(spec: &str) -> Option<(&str, &str)> {
    let at = spec.rfind('@')?;
    if at == 0 || at + 1 >= spec.len() {
        return None;
    }
    let (path, rev) = spec.split_at(at);
    Some((path, &rev[1..]))
}

fn path_in_scope(path: &str, scope: &str) -> bool {
    if scope == "/" || scope.is_empty() {
        return true;
    }
    path == scope || path.starts_with(&format!("{scope}/"))
}

fn op_kind(base: Option<&FileEntry>, other: Option<&FileEntry>) -> ChangeOp {
    match (base, other) {
        (None, None) => ChangeOp::None,
        (None, Some(_)) => ChangeOp::Add,
        (Some(_), None) => ChangeOp::Delete,
        (Some(a), Some(b)) => {
            if a.blob_id != b.blob_id || a.props != b.props || a.executable != b.executable {
                ChangeOp::Modify
            } else {
                ChangeOp::None
            }
        }
    }
}

fn classify_tree_conflict(local: ChangeOp, incoming: ChangeOp) -> String {
    match (local, incoming) {
        (ChangeOp::Modify, ChangeOp::Delete) => "local-edit incoming-delete".to_owned(),
        (ChangeOp::Delete, ChangeOp::Modify) => "local-delete incoming-edit".to_owned(),
        (ChangeOp::Add, ChangeOp::Add) => "add-add".to_owned(),
        (ChangeOp::Delete, ChangeOp::Add) => "delete-add".to_owned(),
        (ChangeOp::Add, ChangeOp::Delete) => "add-delete".to_owned(),
        (ChangeOp::Modify, ChangeOp::Modify) => "edit-edit".to_owned(),
        (a, b) => format!("conflict-{a:?}-{b:?}"),
    }
}

fn build_mergeinfo(
    pending_merges: &[(String, i64)],
    mut inherited: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut grouped: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for (path, value) in &inherited {
        for rev in parse_mergeinfo_value(value) {
            grouped.entry(path.clone()).or_default().push(rev);
        }
    }
    for (path, rev) in pending_merges {
        grouped.entry(path.clone()).or_default().push(*rev);
    }
    let mut out = BTreeMap::new();
    for (path, mut revs) in grouped {
        revs.sort_unstable();
        revs.dedup();
        let value = compress_rev_ranges(&revs);
        out.insert(path, value);
    }
    inherited.clear();
    out
}

fn parse_mergeinfo_value(value: &str) -> Vec<i64> {
    let mut out = Vec::new();
    for part in value.split(',') {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        if let Some((a, b)) = t.split_once('-') {
            let start = a.trim().parse::<i64>().ok();
            let end = b.trim().parse::<i64>().ok();
            if let (Some(s), Some(e)) = (start, end) {
                for r in s.min(e)..=s.max(e) {
                    out.push(r);
                }
                continue;
            }
        }
        if let Ok(r) = t.parse::<i64>() {
            out.push(r);
        }
    }
    out
}

fn compress_rev_ranges(revs: &[i64]) -> String {
    if revs.is_empty() {
        return String::new();
    }
    let mut parts = Vec::new();
    let mut start = revs[0];
    let mut prev = revs[0];
    for &r in &revs[1..] {
        if r == prev + 1 {
            prev = r;
            continue;
        }
        if start == prev {
            parts.push(start.to_string());
        } else {
            parts.push(format!("{start}-{prev}"));
        }
        start = r;
        prev = r;
    }
    if start == prev {
        parts.push(start.to_string());
    } else {
        parts.push(format!("{start}-{prev}"));
    }
    parts.join(",")
}

fn depth_to_str(depth: Depth) -> &'static str {
    match depth {
        Depth::Empty => "empty",
        Depth::Files => "files",
        Depth::Immediates => "immediates",
        Depth::Infinity => "infinity",
    }
}

fn path_allowed_by_ambient_depth(
    path: &str,
    root_depth: Depth,
    ambient: &BTreeMap<String, (String, bool)>,
) -> bool {
    if path.is_empty() {
        return false;
    }
    let parts: Vec<&str> = path.split('/').collect();
    let mut current_prefix = String::new();
    let mut current_depth = root_depth;
    for (idx, part) in parts.iter().enumerate() {
        let is_last = idx + 1 == parts.len();
        if is_last {
            return match current_depth {
                Depth::Empty => false,
                Depth::Files => parts.len() == 1,
                Depth::Immediates => parts.len() <= 2,
                Depth::Infinity => true,
            };
        }
        match current_depth {
            Depth::Empty | Depth::Files => return false,
            Depth::Immediates => {
                // allow entering one level; deeper levels require explicit ambient entry.
                if idx > 0 {
                    return false;
                }
            }
            Depth::Infinity => {}
        }
        if current_prefix.is_empty() {
            current_prefix.push_str(part);
        } else {
            current_prefix.push('/');
            current_prefix.push_str(part);
        }
        if let Some((d, sticky)) = ambient.get(&current_prefix)
            && *sticky
            && let Some(parsed) = Depth::from_str(d)
        {
            current_depth = parsed;
        }
    }
    false
}

fn base_blame_for_lines(
    lines: &[String],
    rev: i64,
    author: &str,
    created_at: chrono::DateTime<Utc>,
) -> Vec<BlameLine> {
    lines
        .iter()
        .enumerate()
        .map(|(idx, line)| BlameLine {
            line_no: idx + 1,
            revision: rev,
            author: author.to_owned(),
            content: line.clone(),
            created_at,
        })
        .collect()
}

fn apply_diff_to_blame(
    prior: &[BlameLine],
    old_lines: &[String],
    new_lines: &[String],
    author: &str,
    rev: i64,
    created_at: chrono::DateTime<Utc>,
) -> Vec<BlameLine> {
    let mut next = Vec::new();
    let ops = capture_diff_slices(Algorithm::Myers, old_lines, new_lines);
    for op in ops {
        match op.tag() {
            DiffTag::Equal => {
                for idx in op.old_range() {
                    if let Some(prev) = prior.get(idx) {
                        next.push(prev.clone());
                    }
                }
            }
            DiffTag::Delete => {}
            DiffTag::Insert | DiffTag::Replace => {
                for idx in op.new_range() {
                    next.push(BlameLine {
                        line_no: idx + 1,
                        revision: rev,
                        author: author.to_owned(),
                        content: new_lines.get(idx).cloned().unwrap_or_default(),
                        created_at,
                    });
                }
            }
        }
    }
    for (idx, item) in next.iter_mut().enumerate() {
        item.line_no = idx + 1;
        item.content = new_lines.get(idx).cloned().unwrap_or_default();
    }
    next
}

#[cfg(unix)]
fn set_executable_if_supported(path: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = fs::metadata(path)?.permissions();
    let mut mode = perms.mode();
    if executable {
        mode |= 0o111;
    } else {
        mode &= !0o111;
    }
    perms.set_mode(mode);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable_if_supported(_path: &Path, _executable: bool) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_collapse_is_line_bounded() {
        assert_eq!(collapse_keyword("id $Rev: 12 $ end", "Rev"), "id $Rev$ end");
        // An unterminated keyword must not consume the following lines.
        let text = "price $Rev: none\nkeep this line\ncost $5\n";
        assert_eq!(collapse_keyword(text, "Rev"), text);
        // Over-long "values" are not keywords either.
        let long = format!("$Rev: {} $", "x".repeat(300));
        assert_eq!(collapse_keyword(&long, "Rev"), long);
        // Several keywords on one line.
        assert_eq!(
            collapse_keyword("$Rev: 1 $ and $Rev: 2 $", "Rev"),
            "$Rev$ and $Rev$"
        );
    }
}
