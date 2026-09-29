//! Creating revisions: from the working copy, from staged subsets and
//! from direct edits (HTTP server).

use super::*;

impl Repository {
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
        self.commit_entries(
            tree,
            message,
            author,
            revprops,
            CommitSource::WorkingCopy {
                schedule: &schedule,
                committed: &committed,
            },
        )
    }

    /// The tree a full commit records: the versioned working files plus BASE
    /// files outside the working-copy depth, carried over unchanged (a sparse
    /// checkout must not delete what it did not materialize).
    pub(super) fn working_commit_tree(&self) -> Result<Vec<FileEntry>> {
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
    pub(super) fn commit_entries(
        &self,
        mut snapshot: Vec<FileEntry>,
        message: &str,
        author: &str,
        mut revprops: BTreeMap<String, String>,
        source: CommitSource<'_>,
    ) -> Result<Commit> {
        let wcdb = self.wcdb()?;
        let from_wc = matches!(source, CommitSource::WorkingCopy { .. });
        let empty = BTreeMap::new();
        let (schedule, committed_schedule): (&BTreeMap<String, Scheduled>, &[String]) = match source
        {
            CommitSource::WorkingCopy {
                schedule,
                committed,
            } => (schedule, committed),
            CommitSource::Store => (&empty, &[]),
        };

        if from_wc {
            let base_rev = wcdb.base_revision()?;
            let head_rev = wcdb.head_revision()?;
            // Recorded merges do not exempt a stale working copy: committing
            // on top of an outdated BASE would silently discard newer revisions.
            if base_rev != head_rev {
                return Err(VcsError::OutOfDate { base_rev, head_rev });
            }
        }

        let parent = self.head_commit()?;
        let mut changed =
            compute_changed_files(parent.as_ref().map(|c| c.files.as_slice()), &snapshot);
        apply_scheduled_copies(&mut changed, schedule);
        let parent_revision = parent.as_ref().map(|c| c.revision);
        // Consumed atomically with the revision itself (see record_commit), so
        // a failed or interrupted commit never loses recorded merges.
        let pending_merges = if from_wc {
            wcdb.pending_merges()?
        } else {
            Vec::new()
        };

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
        let snapshot_map: BTreeMap<&str, &FileEntry> =
            snapshot.iter().map(|f| (f.path.as_str(), f)).collect();
        for ch in &changed {
            if ch.kind != ChangeKind::Modified || !ch.text_modified {
                continue;
            }
            let Some(entry) = snapshot_map.get(ch.path.as_str()).copied() else {
                continue;
            };
            if from_wc
                && has_svn_prop(&entry.props, "svn:needs-lock")
                && !wcdb.has_lock_token(&ch.path)?
            {
                return Err(VcsError::NeedsLockRequired {
                    path: ch.path.clone(),
                });
            }
        }
        // Repository-level locks: nobody commits over someone else's lock. For
        // direct (server) commits svn:needs-lock also requires holding one;
        // working-copy commits check their local lock token above.
        let locks = self.path_locks()?;
        if !from_wc {
            self.check_path_locks(
                author,
                parent.as_ref().map(|c| c.files.as_slice()),
                &snapshot,
                &changed,
            )?;
        } else if let Some(ch) = changed
            .iter()
            .find(|ch| locks.get(&ch.path).is_some_and(|l| l.owner != author))
        {
            return Err(VcsError::LockConflict {
                path: ch.path.clone(),
                owner: locks[&ch.path].owner.clone(),
            });
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

        let mut commit = Commit {
            id: String::new(),
            format: storage::COMMIT_FORMAT,
            revision: next_rev,
            parent: parent_id,
            parent_revision,
            author: author.to_owned(),
            message: message.to_owned(),
            created_at: Utc::now(),
            tree: Some(storage::write_tree(self, &snapshot)?),
            files: snapshot,
            changed_files: changed,
            mergeinfo,
            revprops,
            txn_id: None,
            changed_paths,
        };
        // The id commits to the whole record (tree, parent, metadata), so any
        // later modification of the stored commit is detected on read.
        commit.id = storage::compute_commit_id(&commit)?;
        let id = commit.id.clone();

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
            from_wc.then_some(next_rev),
            committed_schedule,
        )?;

        // Working-copy node metadata is derived data: the commit is already
        // durable, and the next status/sync repairs it if this refresh fails.
        if from_wc {
            let _ = self.sync_wcdb();
        }
        // The revision is published: a failing post-commit hook must not turn
        // a successful commit into an error. Its output is kept for review.
        if let Err(err) = self.run_hook("post-commit", &[&next_rev.to_string(), &id]) {
            self.log_hook_failure(&err);
        }
        Ok(commit)
    }

    /// Commit `edits` on top of HEAD directly in the repository, the way the
    /// HTTP server applies a remote client's changes. The working copy of this
    /// repository is neither read nor modified, so a rejected commit leaves no
    /// trace and the local working state never blocks remote commits.
    pub fn commit_edits(&self, edits: &TreeEdits, message: &str, author: &str) -> Result<Commit> {
        let _lock = self.lock()?;
        let head = self.head_commit()?;
        let head_rev = head.as_ref().map_or(0, |c| c.revision);
        let mut tree: BTreeMap<String, FileEntry> = head
            .map(|c| c.files)
            .unwrap_or_default()
            .into_iter()
            .map(|f| (f.path.clone(), f))
            .collect();

        // Out-of-date check per path, under the lock: the path must be the
        // same at HEAD as at the revision the client edited.
        for (path, &base_rev) in &edits.bases {
            if base_rev == head_rev {
                continue;
            }
            let at_base = if base_rev == 0 {
                None
            } else {
                self.file_entry_at_revision(base_rev, path)?
            };
            if changed_entry(at_base.as_ref(), tree.get(path)) {
                return Err(VcsError::OutOfDate { base_rev, head_rev });
            }
        }

        for path in &edits.deletes {
            crate::path::validate_rel_path(path)?;
            if !edits.puts.contains_key(path) && tree.remove(path).is_none() {
                return Err(VcsError::PathNotFound(path.clone()));
            }
        }
        let mut contents: BTreeMap<&str, &[u8]> = BTreeMap::new();
        for (path, content) in &edits.puts {
            crate::path::validate_rel_path(path)?;
            let blob_id = storage::write_blob(self, content)?;
            let entry = tree.entry(path.clone()).or_insert_with(|| FileEntry {
                path: path.clone(),
                blob_id: String::new(),
                executable: false,
                is_binary: false,
                props: BTreeMap::new(),
                copy_from_path: None,
                copy_from_rev: None,
                node_id: None,
                copy_id: None,
                created_rev: None,
            });
            entry.blob_id = blob_id;
            contents.insert(path, content);
        }
        for (path, changes) in &edits.props {
            let entry = tree
                .get_mut(path)
                .ok_or_else(|| VcsError::PathNotFound(path.clone()))?;
            for (name, value) in changes {
                match value {
                    Some(v) => entry.props.insert(name.clone(), v.clone()),
                    None => entry.props.remove(name),
                };
            }
        }
        // Derived flags of every touched entry follow its final content/props.
        for path in edits.puts.keys().chain(edits.props.keys()) {
            if let Some(entry) = tree.get_mut(path) {
                let detected = match contents.get(path.as_str()) {
                    Some(bytes) => is_binary_content(bytes),
                    None => is_binary_content(&self.read_blob(&entry.blob_id)?),
                };
                entry.is_binary = effective_is_binary(detected, &entry.props);
                entry.executable = has_svn_prop(&entry.props, "svn:executable");
            }
        }

        let mut revprops = BTreeMap::new();
        revprops.insert("svn:author".to_owned(), author.to_owned());
        revprops.insert("svn:log".to_owned(), message.to_owned());
        self.commit_entries(
            tree.into_values().collect(),
            message,
            author,
            revprops,
            CommitSource::Store,
        )
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
        self.commit_entries(
            snapshot,
            message,
            author,
            revprops,
            CommitSource::WorkingCopy {
                schedule: &schedule,
                committed: &committed,
            },
        )
    }
}

pub(super) fn build_changed_paths(
    parent: Option<&Commit>,
    changes: &[FileChange],
) -> Vec<ChangedPath> {
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

pub(super) fn assign_node_identity(
    parent: Option<&Commit>,
    snapshot: &mut [FileEntry],
    next_rev: i64,
) {
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

pub(super) fn build_mergeinfo(
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

pub(super) fn parse_mergeinfo_value(value: &str) -> Vec<i64> {
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

pub(super) fn compress_rev_ranges(revs: &[i64]) -> String {
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
