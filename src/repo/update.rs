//! Applying repository changes to the working copy: update, merge and
//! conflict handling.

use super::*;

impl Repository {
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
            &self.base_files(&*self.wcdb()?)?,
            &target.files,
            Some(&target),
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
        let right_commit = if right_rev == 0 {
            None
        } else {
            Some(self.read_commit_by_revision(right_rev)?)
        };
        let right = right_commit
            .as_ref()
            .map(|c| c.files.clone())
            .unwrap_or_default();
        let outcome = self.apply_tree_delta(
            &left,
            &right,
            right_commit.as_ref(),
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
    pub(super) fn resolve_merge_spec(&self, spec: &str) -> Result<(Option<String>, i64, i64)> {
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

    /// Apply the change from tree `left` to tree `right` to the working copy,
    /// three-way against local modifications.
    /// - update: `left` is BASE; afterwards BASE becomes `rev` (`advance_base`);
    /// - merge: `left`/`right` are the merge-source revisions; BASE stays and
    ///   the result is a local modification whose additions and deletions are
    ///   scheduled for the next commit.
    pub(super) fn apply_tree_delta(
        &self,
        left: &[FileEntry],
        target_files: &[FileEntry],
        target_meta: Option<&Commit>,
        scope_path: Option<&str>,
        advance_base: bool,
        dry_run: bool,
    ) -> Result<MergeOutcome> {
        let rev = target_meta.map_or(0, |c| c.revision);
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
                            revision: target_meta.map(|c| c.revision),
                            author: target_meta.map(|c| c.author.as_str()),
                            date: target_meta.map(|c| c.created_at),
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
    pub(super) fn merge_or_conflict(
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
                    let (merged, clean) = three_way_text(base_s, mine_s, theirs_s);
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

    pub(super) fn write_conflict_artifacts(
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChangeOp {
    None,
    Add,
    Delete,
    Modify,
}

pub(super) fn changed_entry(a: Option<&FileEntry>, b: Option<&FileEntry>) -> bool {
    match (a, b) {
        (None, None) => false,
        (Some(_), None) | (None, Some(_)) => true,
        (Some(a), Some(b)) => {
            a.blob_id != b.blob_id || a.props != b.props || a.executable != b.executable
        }
    }
}

/// Describe the incoming change a merge/update would apply for one path (base ->
/// target), used to report `merge --dry-run` without touching the working copy.
pub(super) fn incoming_change(
    path: &str,
    b: Option<&FileEntry>,
    t: Option<&FileEntry>,
) -> FileChange {
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

pub(super) fn op_kind(base: Option<&FileEntry>, other: Option<&FileEntry>) -> ChangeOp {
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

pub(super) fn classify_tree_conflict(local: ChangeOp, incoming: ChangeOp) -> String {
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

pub(super) fn split_peg(spec: &str) -> Option<(&str, &str)> {
    let at = spec.rfind('@')?;
    if at == 0 || at + 1 >= spec.len() {
        return None;
    }
    let (path, rev) = spec.split_at(at);
    Some((path, &rev[1..]))
}

pub(super) fn path_in_scope(path: &str, scope: &str) -> bool {
    if scope == "/" || scope.is_empty() {
        return true;
    }
    path == scope || path.starts_with(&format!("{scope}/"))
}
