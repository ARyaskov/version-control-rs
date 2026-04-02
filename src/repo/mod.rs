mod storage;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use diffy::merge as diffy_merge;
use globset::{Glob, GlobSetBuilder};
use similar::{Algorithm, DiffTag, capture_diff_slices};
use walkdir::WalkDir;

use crate::error::{Result, VcsError};
use crate::types::{
    BlameLine, ChangeKind, ChangedPath, ChangedPathAction, Commit, Depth, FileChange, FileEntry,
    RevisionRange, TxnJournalEntry, TxnOp, TxnRecord,
};
use crate::wcdb::{ExternalDef, WcDb};

pub(crate) const VCRS_DIR: &str = ".vcrs";

#[derive(Debug, Clone)]
pub struct Repository {
    pub root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct MergeOutcome {
    pub changed: Vec<FileChange>,
    pub conflicts: Vec<String>,
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

        let repo = Self { root };
        storage::ensure_layout(&repo)?;
        repo.ensure_initialized()?;
        Ok(repo)
    }

    pub fn ensure_initialized(&self) -> Result<()> {
        storage::ensure_layout(self)?;
        let wcdb = self.wcdb()?;
        let _ = wcdb.max_revision()?;
        self.replay_or_abort_transactions()?;
        self.process_work_queue()?;
        if self.head_commit_id()?.is_none() {
            wcdb.set_head_revision(0)?;
            wcdb.set_base_revision(0)?;
            self.sync_wcdb()?;
        }
        Ok(())
    }

    pub fn head_commit_id(&self) -> Result<Option<String>> {
        storage::read_head(self)
    }

    pub fn set_head_commit_id(&self, commit_id: &str) -> Result<()> {
        storage::write_head(self, commit_id)
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

    pub fn rebuild_revision_index(&self) -> Result<()> {
        let commits_dir = self.root.join(VCRS_DIR).join("commits");
        if !commits_dir.exists() {
            return Ok(());
        }
        let wcdb = self.wcdb()?;
        let mut commits = Vec::new();
        for entry in fs::read_dir(commits_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(path)?;
            let commit: Commit = serde_json::from_slice(&bytes)?;
            commits.push(commit);
        }
        commits.sort_by_key(|c| c.revision);
        let mut max_rev = 0_i64;
        for c in commits {
            max_rev = max_rev.max(c.revision);
            wcdb.upsert_revision(
                c.revision,
                &c.id,
                c.parent_revision,
                &c.author,
                &c.message,
                &c.created_at.to_rfc3339(),
                &serde_json::to_string(&c.changed_paths)?,
                &serde_json::to_string(&c.mergeinfo)?,
            )?;
        }
        wcdb.set_head_revision(max_rev)?;
        Ok(())
    }

    pub fn write_blob(&self, content: &[u8]) -> Result<String> {
        storage::write_blob(self, content)
    }

    pub fn read_blob(&self, blob_id: &str) -> Result<Vec<u8>> {
        storage::read_blob(self, blob_id)
    }

    pub fn snapshot_working_copy(&self) -> Result<Vec<FileEntry>> {
        let wcdb = self.wcdb()?;
        let depth = Depth::from_str(&wcdb.depth()?).unwrap_or(Depth::Infinity);
        let ambient = wcdb.ambient_depth_map()?;
        let ignore_patterns = self.collect_ignore_patterns(&wcdb)?;
        let externals = wcdb.list_externals()?;
        let mut external_paths = BTreeSet::new();
        for ex in externals {
            external_paths.insert(ex.path);
        }

        let mut entries = Vec::new();

        for entry in WalkDir::new(&self.root).follow_links(false) {
            let entry = entry?;
            let path = entry.path();

            if path == self.root.join(VCRS_DIR)
                || path.starts_with(self.root.join(VCRS_DIR))
                || path.starts_with(self.root.join(".git"))
                || path.starts_with(self.root.join("target"))
            {
                continue;
            }

            if entry.file_type().is_file() || entry.file_type().is_symlink() {
                let rel = path
                    .strip_prefix(&self.root)
                    .map_err(|_| VcsError::PathOutsideRepository(path.display().to_string()))?
                    .to_string_lossy()
                    .replace('\\', "/");

                if should_ignore(&rel, &ignore_patterns) || is_under_external(&rel, &external_paths)
                {
                    continue;
                }
                if !path_allowed_by_ambient_depth(&rel, depth, &ambient) {
                    continue;
                }

                let mut bytes = if entry.file_type().is_symlink() {
                    let target = fs::read_link(path)?;
                    format!("link {}", target.to_string_lossy()).into_bytes()
                } else {
                    fs::read(path)?
                };
                let mut is_binary = is_binary_content(&bytes);

                #[cfg(unix)]
                let executable = {
                    use std::os::unix::fs::PermissionsExt;
                    fs::metadata(path)?.permissions().mode() & 0o111 != 0
                };

                #[cfg(not(unix))]
                let executable = false;

                let mut props = infer_props(executable, is_binary);
                if entry.file_type().is_symlink() {
                    props.insert("svn:special".to_owned(), "*".to_owned());
                }
                for (k, v) in wcdb.file_props(&rel)? {
                    props.insert(k, v);
                }
                for (k, v) in wcdb.inherited_props_for_path(&rel)? {
                    props.entry(k).or_insert(v);
                }
                is_binary = effective_is_binary(is_binary, &props);
                if !is_binary && !has_svn_prop(&props, "svn:special") {
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        bytes = normalize_eol(text, props.get("svn:eol-style")).into_bytes();
                    }
                }
                if has_svn_prop(&props, "svn:keywords")
                    && !has_svn_prop(&props, "svn:special")
                    && let Ok(text) = std::str::from_utf8(&bytes)
                {
                    bytes = contract_keywords(text, &props).into_bytes();
                }
                let blob_id = self.write_blob(&bytes)?;

                entries.push(FileEntry {
                    path: rel,
                    blob_id,
                    executable,
                    is_binary,
                    props,
                    copy_from_path: None,
                    copy_from_rev: None,
                    node_id: None,
                    copy_id: None,
                    created_rev: None,
                });
            }
        }

        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
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
        mut revprops: BTreeMap<String, String>,
    ) -> Result<Commit> {
        self.ensure_initialized()?;
        let wcdb = self.wcdb()?;

        let base_rev = wcdb.base_revision()?;
        let head_rev = wcdb.head_revision()?;
        let has_pending_merges = !wcdb.pending_merges()?.is_empty();
        if base_rev != head_rev && !has_pending_merges {
            return Err(VcsError::OutOfDate { base_rev, head_rev });
        }

        let parent = self.head_commit()?;
        let mut snapshot = self.snapshot_working_copy()?;
        let changed = compute_changed_files(parent.as_ref().map(|c| c.files.as_slice()), &snapshot);
        let parent_revision = parent.as_ref().map(|c| c.revision);
        let pending_merges = if has_pending_merges {
            wcdb.take_pending_merges()?
        } else {
            Vec::new()
        };

        if changed.is_empty() && pending_merges.is_empty() {
            return Ok(parent.unwrap_or(Commit {
                id: String::new(),
                revision: head_rev,
                parent: None,
                parent_revision: None,
                author: author.to_owned(),
                message: message.to_owned(),
                created_at: Utc::now(),
                files: snapshot,
                changed_files: Vec::new(),
                mergeinfo: BTreeMap::new(),
                revprops: BTreeMap::new(),
                txn_id: None,
                changed_paths: Vec::new(),
            }));
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
        let changed_paths = build_changed_paths(parent.as_ref(), &snapshot, &changed);
        let inherited_mergeinfo = parent
            .as_ref()
            .map(|p| p.mergeinfo.clone())
            .unwrap_or_default();
        let mergeinfo = build_mergeinfo(&pending_merges, inherited_mergeinfo);
        revprops
            .entry("svn:date".to_owned())
            .or_insert_with(|| Utc::now().to_rfc3339());

        let txn = TxnRecord {
            id: id.clone(),
            next_rev,
            base_rev,
            head_rev,
            parent_id: parent_id.clone(),
            author: author.to_owned(),
            message: message.to_owned(),
            phase: "begun".to_owned(),
            started_at: Utc::now(),
            changed_files: changed.clone(),
            pending_merges: pending_merges.clone(),
        };
        self.begin_txn(&txn)?;
        if let Err(err) = self.run_hook("pre-commit", &[txn.id.as_str()]) {
            self.abort_txn(&txn)?;
            return Err(err);
        }
        self.update_txn_phase(&txn.id, "pre-commit-ok")?;

        let commit = Commit {
            id: id.clone(),
            revision: next_rev,
            parent: parent_id,
            parent_revision,
            author: author.to_owned(),
            message: message.to_owned(),
            created_at: Utc::now(),
            files: snapshot,
            changed_files: changed.clone(),
            mergeinfo: mergeinfo.clone(),
            revprops: revprops.clone(),
            txn_id: Some(txn.id.clone()),
            changed_paths: changed_paths.clone(),
        };

        let result: Result<()> = (|| {
            let old_head = self.head_commit_id()?;
            storage::write_commit(self, &commit)?;
            self.append_txn_journal(
                &txn.id,
                TxnOp::WriteCommit { id: id.clone() },
                TxnOp::DeleteCommit { id: id.clone() },
            )?;
            self.update_txn_phase(&txn.id, "commit-written")?;
            storage::write_head(self, &id)?;
            self.append_txn_journal(
                &txn.id,
                TxnOp::WriteHead {
                    old: old_head.clone(),
                    new: id.clone(),
                },
                TxnOp::WriteHead {
                    old: Some(id.clone()),
                    new: old_head.unwrap_or_default(),
                },
            )?;
            self.update_txn_phase(&txn.id, "head-written")?;

            wcdb.add_revision(
                next_rev,
                &id,
                parent_revision,
                author,
                message,
                &commit.created_at.to_rfc3339(),
                &serde_json::to_string(&changed_paths)?,
                &serde_json::to_string(&mergeinfo)?,
            )?;
            self.append_txn_journal(
                &txn.id,
                TxnOp::UpsertRevision { rev: next_rev },
                TxnOp::DeleteRevision { rev: next_rev },
            )?;
            for (source_path, merged_rev) in pending_merges {
                wcdb.add_merge_edge(next_rev, merged_rev, &source_path)?;
                self.append_txn_journal(
                    &txn.id,
                    TxnOp::AddMergeEdge {
                        target_rev: next_rev,
                        merged_rev,
                        source_path: source_path.clone(),
                    },
                    TxnOp::DeleteMergeEdge {
                        target_rev: next_rev,
                        merged_rev,
                        source_path,
                    },
                )?;
            }
            wcdb.set_base_revision(next_rev)?;
            self.append_txn_journal(
                &txn.id,
                TxnOp::SetBaseRevision {
                    old: base_rev,
                    new: next_rev,
                },
                TxnOp::SetBaseRevision {
                    old: next_rev,
                    new: base_rev,
                },
            )?;
            self.sync_wcdb()?;
            self.update_txn_phase(&txn.id, "wcdb-written")?;
            Ok(())
        })();

        if let Err(err) = result {
            self.abort_txn(&txn)?;
            return Err(err);
        }

        self.complete_txn(&txn.id)?;
        self.run_hook("post-commit", &[&next_rev.to_string(), &id])?;
        Ok(commit)
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
        self.sync_wcdb()?;
        let current = self.snapshot_working_copy()?;
        let base_rev = self.wcdb()?.base_revision()?;
        let base_files = if base_rev == 0 {
            Vec::new()
        } else {
            self.read_commit_by_revision(base_rev)
                .map(|c| c.files)
                .unwrap_or_default()
        };
        Ok(compute_changed_files(Some(&base_files), &current))
    }

    pub fn revert_to_head(&self, only_paths: &[String]) -> Result<Vec<FileChange>> {
        let Some(head) = self.head_commit()? else {
            return Ok(Vec::new());
        };

        let changed = self.restore_snapshot(&head.files, only_paths)?;
        self.sync_wcdb()?;
        Ok(changed)
    }

    pub fn copy_path(&self, src: &str, dst: &str) -> Result<()> {
        let src_abs = rel_to_abs(&self.root, src);
        let dst_abs = rel_to_abs(&self.root, dst);
        if let Some(parent) = dst_abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src_abs, dst_abs)?;
        self.sync_wcdb()
    }

    pub fn move_path(&self, src: &str, dst: &str) -> Result<()> {
        let src_abs = rel_to_abs(&self.root, src);
        let dst_abs = rel_to_abs(&self.root, dst);
        if let Some(parent) = dst_abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(src_abs, dst_abs)?;
        self.sync_wcdb()
    }

    pub fn update_to_revision(&self, revision: &str) -> Result<Vec<FileChange>> {
        self.update_to_revision_with_depth(revision, None)
    }

    pub fn update_to_revision_with_depth(
        &self,
        revision: &str,
        depth: Option<Depth>,
    ) -> Result<Vec<FileChange>> {
        if let Some(d) = depth {
            self.set_depth(d)?;
            self.wcdb()?.set_ambient_depth("", depth_to_str(d), true)?;
        }
        let target_rev = self.resolve_revision_spec(revision)?;
        let target = self.read_commit_by_revision(target_rev)?;
        let outcome = self.apply_revision_with_conflicts(target_rev, &target.files, None, true)?;
        if let Some(d) = depth {
            self.prune_working_to_depth(d)?;
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

    pub fn merge_from_revision(
        &self,
        revision: &str,
        dry_run: bool,
        record_only: bool,
    ) -> Result<MergeOutcome> {
        let (scope_path, target_rev) = self.resolve_peg_spec(revision)?;
        let target = self.read_commit_by_revision(target_rev)?;

        if record_only {
            let mut work = BTreeMap::new();
            work.insert("record-only".to_owned(), target_rev.to_string());
            self.wcdb()?.enqueue_work(&serde_json::to_string(&work)?)?;
            self.wcdb()?
                .add_pending_merge(scope_path.as_deref().unwrap_or("/"), target_rev)?;
            return Ok(MergeOutcome {
                changed: Vec::new(),
                conflicts: Vec::new(),
            });
        }

        if dry_run {
            let status = self.status()?;
            return Ok(MergeOutcome {
                changed: status,
                conflicts: Vec::new(),
            });
        }

        let outcome = self.apply_revision_with_conflicts(
            target_rev,
            &target.files,
            scope_path.as_deref(),
            false,
        )?;
        self.wcdb()?
            .add_pending_merge(scope_path.as_deref().unwrap_or("/"), target_rev)?;
        self.sync_wcdb()?;
        Ok(outcome)
    }

    pub fn resolve_revision_spec(&self, spec: &str) -> Result<i64> {
        let wcdb = self.wcdb()?;
        if spec.eq_ignore_ascii_case("HEAD") {
            return wcdb.head_revision();
        }
        let rev: i64 = spec
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
        let wcdb = self.wcdb()?;
        wcdb.set_file_prop(path, name, value)?;
        let abs = rel_to_abs(&self.root, path);
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
        self.wcdb()?.set_inherited_prop(scope_path, name, value)
    }

    pub fn list_inherited_properties(&self) -> Result<Vec<(String, String, String)>> {
        self.wcdb()?.list_inherited_props()
    }

    pub fn set_changelist(&self, path: &str, changelist: Option<&str>) -> Result<()> {
        self.wcdb()?.set_changelist(path, changelist)
    }

    pub fn list_changelists(&self) -> Result<Vec<(String, String)>> {
        self.wcdb()?.list_changelists()
    }

    pub fn set_depth(&self, depth: Depth) -> Result<()> {
        self.wcdb()?.set_depth(depth_to_str(depth))
    }

    pub fn depth(&self) -> Result<Depth> {
        Ok(Depth::from_str(&self.wcdb()?.depth()?).unwrap_or(Depth::Infinity))
    }

    pub fn get_property(&self, path: &str, name: &str) -> Result<Option<String>> {
        let wcdb = self.wcdb()?;
        Ok(wcdb.file_props(path)?.remove(name))
    }

    pub fn del_property(&self, path: &str, name: &str) -> Result<()> {
        let wcdb = self.wcdb()?;
        wcdb.delete_file_prop(path, name)?;
        let abs = rel_to_abs(&self.root, path);
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
        let wcdb = self.wcdb()?;
        match token {
            Some(t) => wcdb.set_lock_token(path, t, owner)?,
            None => wcdb.clear_lock_token(path)?,
        }
        if self.path_requires_lock(path)? {
            let abs = rel_to_abs(&self.root, path);
            if abs.exists() {
                set_readonly_if_supported(&abs, token.is_none())?;
            }
        }
        Ok(())
    }

    pub fn clear_local_lock_tokens(&self) -> Result<()> {
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

    fn begin_txn(&self, txn: &TxnRecord) -> Result<()> {
        let dir = self.root.join(VCRS_DIR).join("transactions");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", txn.id));
        fs::write(path, serde_json::to_vec_pretty(txn)?)?;
        fs::write(
            dir.join(format!("{}.journal.json", txn.id)),
            serde_json::to_vec_pretty(&Vec::<TxnJournalEntry>::new())?,
        )?;
        Ok(())
    }

    fn update_txn_phase(&self, txn_id: &str, phase: &str) -> Result<()> {
        let path = self
            .root
            .join(VCRS_DIR)
            .join("transactions")
            .join(format!("{txn_id}.json"));
        if !path.exists() {
            return Ok(());
        }
        let bytes = fs::read(&path)?;
        let mut txn: TxnRecord = serde_json::from_slice(&bytes)?;
        txn.phase = phase.to_owned();
        fs::write(path, serde_json::to_vec_pretty(&txn)?)?;
        Ok(())
    }

    fn abort_txn(&self, txn: &TxnRecord) -> Result<()> {
        let entries = self.read_txn_journal(&txn.id)?;
        for entry in entries.into_iter().rev() {
            let _ = self.apply_txn_op(&entry.undo);
        }
        for (path, rev) in &txn.pending_merges {
            let _ = self.wcdb()?.add_pending_merge(path, *rev);
        }
        self.complete_txn(&txn.id)
    }

    fn complete_txn(&self, txn_id: &str) -> Result<()> {
        let tx_path = self
            .root
            .join(VCRS_DIR)
            .join("transactions")
            .join(format!("{txn_id}.json"));
        if tx_path.exists() {
            fs::remove_file(tx_path)?;
        }
        let journal_path = self
            .root
            .join(VCRS_DIR)
            .join("transactions")
            .join(format!("{txn_id}.journal.json"));
        if journal_path.exists() {
            fs::remove_file(journal_path)?;
        }
        Ok(())
    }

    fn append_txn_journal(&self, txn_id: &str, redo: TxnOp, undo: TxnOp) -> Result<()> {
        let mut entries = self.read_txn_journal(txn_id)?;
        entries.push(TxnJournalEntry { redo, undo });
        let path = self
            .root
            .join(VCRS_DIR)
            .join("transactions")
            .join(format!("{txn_id}.journal.json"));
        fs::write(path, serde_json::to_vec_pretty(&entries)?)?;
        Ok(())
    }

    fn read_txn_journal(&self, txn_id: &str) -> Result<Vec<TxnJournalEntry>> {
        let path = self
            .root
            .join(VCRS_DIR)
            .join("transactions")
            .join(format!("{txn_id}.journal.json"));
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn apply_txn_op(&self, op: &TxnOp) -> Result<()> {
        match op {
            TxnOp::WriteHead { new, .. } => {
                storage::write_head(self, new)?;
            }
            TxnOp::WriteCommit { .. } => {}
            TxnOp::DeleteCommit { id } => {
                let p = self
                    .root
                    .join(VCRS_DIR)
                    .join("commits")
                    .join(format!("{id}.json"));
                if p.exists() {
                    fs::remove_file(p)?;
                }
            }
            TxnOp::UpsertRevision { .. } => {}
            TxnOp::DeleteRevision { rev } => {
                self.wcdb()?.delete_revision(*rev)?;
            }
            TxnOp::SetBaseRevision { new, .. } => {
                self.wcdb()?.set_base_revision(*new)?;
            }
            TxnOp::AddMergeEdge {
                target_rev,
                merged_rev,
                source_path,
            } => {
                self.wcdb()?
                    .add_merge_edge(*target_rev, *merged_rev, source_path)?;
            }
            TxnOp::DeleteMergeEdge {
                target_rev,
                merged_rev,
                source_path,
            } => {
                self.wcdb()?
                    .delete_merge_edge(*target_rev, *merged_rev, source_path)?;
            }
        }
        Ok(())
    }

    fn run_hook(&self, hook_name: &str, args: &[&str]) -> Result<()> {
        let hooks = self.root.join(VCRS_DIR).join("hooks");
        let candidates = [
            hooks.join(hook_name),
            hooks.join(format!("{hook_name}.sh")),
            hooks.join(format!("{hook_name}.ps1")),
        ];
        let Some(path) = candidates.iter().find(|p| p.exists()) else {
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

    fn replay_or_abort_transactions(&self) -> Result<()> {
        let tx_dir = self.root.join(VCRS_DIR).join("transactions");
        if !tx_dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(tx_dir)? {
            let entry = entry?;
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let bytes = fs::read(&p)?;
            let txn: TxnRecord = match serde_json::from_slice(&bytes) {
                Ok(t) => t,
                Err(_) => {
                    let _ = fs::remove_file(&p);
                    continue;
                }
            };
            let commit_path = self
                .root
                .join(VCRS_DIR)
                .join("commits")
                .join(format!("{}.json", txn.id));
            if commit_path.exists() {
                let entries = self.read_txn_journal(&txn.id).unwrap_or_default();
                for entry in entries {
                    let _ = self.apply_txn_op(&entry.redo);
                }
                let _ = self.rebuild_revision_index();
                let _ = self.sync_wcdb();
                let _ = self.complete_txn(&txn.id);
            } else {
                let _ = self.abort_txn(&txn);
            }
        }
        Ok(())
    }

    fn process_work_queue(&self) -> Result<()> {
        let wcdb = self.wcdb()?;
        while let Some(work) = wcdb.dequeue_work()? {
            let parsed: serde_json::Value =
                serde_json::from_str(&work).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(mark) = parsed.get("record-only").and_then(|v| v.as_str())
                && let Ok(rev) = mark.parse::<i64>()
            {
                wcdb.add_pending_merge("/", rev)?;
            }
        }
        Ok(())
    }

    fn prune_working_to_depth(&self, depth: Depth) -> Result<()> {
        for entry in WalkDir::new(&self.root).follow_links(false) {
            let entry = entry?;
            if !entry.file_type().is_file() && !entry.file_type().is_symlink() {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&self.root)
                .map_err(|_| VcsError::PathOutsideRepository(entry.path().display().to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            if rel.starts_with(".vcrs/") || rel.starts_with(".git/") || rel.starts_with("target/") {
                continue;
            }
            if !path_allowed_by_ambient_depth(&rel, depth, &self.wcdb()?.ambient_depth_map()?) {
                let _ = remove_path_if_exists(entry.path());
            }
        }
        Ok(())
    }

    fn apply_revision_with_conflicts(
        &self,
        rev: i64,
        target_files: &[FileEntry],
        scope_path: Option<&str>,
        advance_base: bool,
    ) -> Result<MergeOutcome> {
        let target_meta = self.read_commit_by_revision(rev).ok();
        let wcdb = self.wcdb()?;
        let depth = Depth::from_str(&wcdb.depth()?).unwrap_or(Depth::Infinity);
        let ambient = wcdb.ambient_depth_map()?;
        let base_rev = wcdb.base_revision()?;
        let base_files = if base_rev == 0 {
            Vec::new()
        } else {
            self.read_commit_by_revision(base_rev)?.files
        };
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

        wcdb.acquire_lock("", i64::MAX)?;
        wcdb.clear_conflicts()?;

        for path in &keys {
            if let Some(scope) = scope_path {
                if !path_in_scope(path, scope) {
                    continue;
                }
            }
            if !path_allowed_by_ambient_depth(path, depth, &ambient) {
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

            if local_changed && changed_entry(w, t) {
                // conflict
                if let (Some(be), Some(we), Some(te)) = (b, w, t) {
                    if !(be.is_binary || we.is_binary || te.is_binary) {
                        let merged = three_way_merge_text(
                            &String::from_utf8_lossy(&self.read_blob(&be.blob_id)?),
                            &String::from_utf8_lossy(&self.read_blob(&we.blob_id)?),
                            &String::from_utf8_lossy(&self.read_blob(&te.blob_id)?),
                        );

                        let abs = rel_to_abs(&self.root, path);
                        if let Some(parent) = abs.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        let mine_path = format!("{}.mine", abs.display());
                        let old_path = format!("{}.rOLD", abs.display());
                        let new_path = format!("{}.rNEW", abs.display());
                        fs::write(&mine_path, self.read_blob(&we.blob_id)?)?;
                        fs::write(&old_path, self.read_blob(&be.blob_id)?)?;
                        fs::write(&new_path, self.read_blob(&te.blob_id)?)?;
                        fs::write(&abs, merged.as_bytes())?;

                        wcdb.set_text_conflict_markers(path, &old_path, &new_path, &mine_path)?;
                        conflicts.push(path.to_string());
                        continue;
                    }
                }

                let local_op = op_kind(b, w);
                let incoming_op = op_kind(b, t);
                let reason = classify_tree_conflict(local_op, incoming_op);
                wcdb.set_tree_conflict(path, &reason)?;
                conflicts.push(path.to_string());
                continue;
            }

            // no conflict, apply target state
            match t {
                Some(te) => {
                    let abs = rel_to_abs(&self.root, path);
                    if let Some(parent) = abs.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let raw = self.read_blob(&te.blob_id)?;
                    write_entry_to_working(
                        &abs,
                        te,
                        &raw,
                        target_meta.as_ref().map(|c| c.revision),
                        target_meta.as_ref().map(|c| c.author.as_str()),
                        target_meta.as_ref().map(|c| c.created_at),
                        wcdb.has_lock_token(path)?,
                    )?;
                }
                None => {
                    let abs = rel_to_abs(&self.root, path);
                    if abs.exists() {
                        remove_path_if_exists(&abs)?;
                    }
                }
            }
        }

        wcdb.release_lock("")?;
        let changed = self.status()?;
        if conflicts.is_empty() && advance_base {
            wcdb.set_base_revision(rev)?;
        }

        Ok(MergeOutcome { changed, conflicts })
    }

    fn restore_snapshot(
        &self,
        snapshot: &[FileEntry],
        only_paths: &[String],
    ) -> Result<Vec<FileChange>> {
        let filter: Option<BTreeSet<&str>> = if only_paths.is_empty() {
            None
        } else {
            Some(only_paths.iter().map(String::as_str).collect())
        };

        let before = self.snapshot_working_copy()?;
        let before_map: BTreeMap<&str, &FileEntry> =
            before.iter().map(|e| (e.path.as_str(), e)).collect();
        let target_map: BTreeMap<&str, &FileEntry> =
            snapshot.iter().map(|e| (e.path.as_str(), e)).collect();

        for (path, entry) in &target_map {
            if filter.as_ref().is_some_and(|f| !f.contains(path)) {
                continue;
            }
            let abs = rel_to_abs(&self.root, path);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)?;
            }
            let blob = self.read_blob(&entry.blob_id)?;
            let head = self.head_commit()?;
            write_entry_to_working(
                &abs,
                entry,
                &blob,
                head.as_ref().map(|c| c.revision),
                head.as_ref().map(|c| c.author.as_str()),
                head.as_ref().map(|c| c.created_at),
                self.wcdb()?.has_lock_token(path)?,
            )?;
        }

        for path in before_map.keys() {
            if filter.as_ref().is_some_and(|f| !f.contains(path)) {
                continue;
            }
            if !target_map.contains_key(path) {
                let abs = rel_to_abs(&self.root, path);
                if abs.exists() {
                    remove_path_if_exists(&abs)?;
                }
            }
        }

        let after = self.snapshot_working_copy()?;
        Ok(compute_changed_files(Some(&before), &after))
    }

    fn sync_wcdb(&self) -> Result<()> {
        let wcdb = self.wcdb()?;
        let base_rev = wcdb.base_revision()?;
        let base_files = if base_rev == 0 {
            Vec::new()
        } else {
            self.read_commit_by_revision(base_rev)
                .map(|c| c.files)
                .unwrap_or_default()
        };
        let working = self.snapshot_working_copy()?;
        let changes = compute_changed_files(Some(&base_files), &working);
        wcdb.replace_nodes(&base_files, &working, &changes)?;
        wcdb.replace_file_props_from_entries(&working)
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
                    });
                }
            }
            (None, None) => {}
        }
    }

    for (blob_id, add_indices) in &added_by_blob {
        if let Some(del_indices) = deleted_by_blob.get(blob_id) {
            let pair_count = add_indices.len().min(del_indices.len());
            for i in 0..pair_count {
                let add_idx = add_indices[i];
                let del_idx = del_indices[i];
                let from_path = out[del_idx].path.clone();
                let to_path = out[add_idx].path.clone();

                out[add_idx].moved_from = Some(from_path.clone());
                out[add_idx].copy_from = Some(from_path.clone());
                out[del_idx].moved_to = Some(to_path);
            }
        }
    }

    let mut base_by_blob: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for f in base {
        base_by_blob
            .entry(f.blob_id.as_str())
            .or_default()
            .push(f.path.as_str());
    }

    for change in &mut out {
        if change.kind != ChangeKind::Added || change.copy_from.is_some() {
            continue;
        }

        let Some(newf) = now_map.get(change.path.as_str()) else {
            continue;
        };

        if let Some(candidates) = base_by_blob.get(newf.blob_id.as_str()) {
            if let Some(source) = candidates
                .iter()
                .copied()
                .find(|p| !deleted_paths.contains(*p) && *p != change.path)
            {
                change.copy_from = Some(source.to_owned());
            }
        }
    }

    out
}

fn build_changed_paths(
    parent: Option<&Commit>,
    snapshot: &[FileEntry],
    changes: &[FileChange],
) -> Vec<ChangedPath> {
    let parent_rev = parent.map(|p| p.revision);
    let snap_map: BTreeMap<&str, &FileEntry> =
        snapshot.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut out = Vec::new();
    for ch in changes {
        let action = match ch.kind {
            ChangeKind::Added => {
                if ch.copy_from.is_some() {
                    ChangedPathAction::Replace
                } else {
                    ChangedPathAction::Add
                }
            }
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
        let _entry = snap_map.get(path.as_str()).copied();
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

fn rel_to_abs(root: &Path, rel: &str) -> PathBuf {
    let mut abs = root.to_path_buf();
    for part in rel.split('/') {
        abs.push(part);
    }
    abs
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

fn three_way_merge_text(base: &str, ours: &str, theirs: &str) -> String {
    match diffy_merge(base, ours, theirs) {
        Ok(text) => text,
        Err(conflict) => conflict,
    }
}

fn should_ignore(path: &str, patterns: &[String]) -> bool {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        if let Ok(glob) = Glob::new(p) {
            builder.add(glob);
        }
    }
    let Ok(set) = builder.build() else {
        return false;
    };
    set.is_match(path)
}

fn is_under_external(path: &str, external_paths: &BTreeSet<String>) -> bool {
    external_paths
        .iter()
        .any(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")))
}

fn is_binary_content(bytes: &[u8]) -> bool {
    if bytes.contains(&0) {
        return true;
    }
    std::str::from_utf8(bytes).is_err()
}

fn infer_props(executable: bool, is_binary: bool) -> BTreeMap<String, String> {
    let mut props = BTreeMap::new();
    if executable {
        props.insert("svn:executable".to_owned(), "*".to_owned());
    }
    if is_binary {
        props.insert(
            "svn:mime-type".to_owned(),
            "application/octet-stream".to_owned(),
        );
    } else {
        props.insert("svn:eol-style".to_owned(), "native".to_owned());
    }
    props
}

fn has_svn_prop(props: &BTreeMap<String, String>, name: &str) -> bool {
    props.get(name).is_some_and(|v| !v.trim().is_empty())
}

fn write_entry_to_working(
    abs: &Path,
    entry: &FileEntry,
    raw: &[u8],
    revision: Option<i64>,
    author: Option<&str>,
    date: Option<DateTime<Utc>>,
    has_lock_token: bool,
) -> Result<()> {
    remove_path_if_exists(abs)?;
    if has_svn_prop(&entry.props, "svn:special")
        && let Ok(text) = std::str::from_utf8(raw)
        && let Some(target) = text.strip_prefix("link ")
    {
        if try_create_symlink(abs, target.trim()).is_ok() {
            return Ok(());
        }
    }

    let mut out = raw.to_vec();
    let effective_binary = effective_is_binary(entry.is_binary, &entry.props);
    if !effective_binary
        && !has_svn_prop(&entry.props, "svn:special")
        && let Ok(text) = std::str::from_utf8(&out)
    {
        out = apply_eol_style_for_working(text, entry.props.get("svn:eol-style")).into_bytes();
    }
    if has_svn_prop(&entry.props, "svn:keywords")
        && let Ok(text) = std::str::from_utf8(&out)
    {
        out = expand_keywords(
            text,
            &entry.props,
            revision.unwrap_or(0),
            author.unwrap_or("unknown"),
            date.unwrap_or_else(Utc::now),
        )
        .into_bytes();
    }
    fs::write(abs, out)?;
    set_executable_if_supported(abs, entry.executable)?;
    if has_svn_prop(&entry.props, "svn:needs-lock") && !has_lock_token {
        set_readonly_if_supported(abs, true)?;
    }
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let md = fs::symlink_metadata(path)?;
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

fn collapse_keyword(input: &str, name: &str) -> String {
    let needle = format!("${name}:");
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while let Some(pos) = input[i..].find(&needle) {
        let abs = i + pos;
        out.push_str(&input[i..abs]);
        if let Some(end_rel) = input[abs..].find('$') {
            let end = abs + end_rel;
            out.push_str(&format!("${name}$"));
            i = end + 1;
        } else {
            out.push_str(&input[abs..]);
            i = input.len();
            break;
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
