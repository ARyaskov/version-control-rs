//! The working copy: versioned snapshot, scheduling (add/rm/copy/move),
//! status, revert, and materializing files from the repository.

use super::*;

impl Repository {
    /// Read-only view of the working copy: computes blob ids by hashing without
    /// writing to the object store. Use this for status/diff/merge planning.
    pub fn snapshot_working_copy(&self) -> Result<Vec<FileEntry>> {
        self.collect_working(false)
    }

    /// Like [`snapshot_working_copy`] but persists each file's content as a blob
    /// in the object store. Use this only when producing a commit.
    pub(super) fn materialize_working_copy(&self) -> Result<Vec<FileEntry>> {
        self.collect_working(true)
    }

    /// Versioned files of the working copy: BASE paths not scheduled for
    /// deletion plus scheduled additions. Unversioned files are never part of
    /// the snapshot; a versioned file missing from disk is simply absent (and
    /// therefore reported as deleted).
    pub(super) fn collect_working(&self, persist: bool) -> Result<Vec<FileEntry>> {
        let wcdb = self.wcdb()?;
        let base = self.base_files(&wcdb)?;
        let schedule = wcdb.schedule()?;
        let scope = WcScope::load(&wcdb)?;
        // One SQL round-trip each instead of two per file.
        let all_file_props = wcdb.all_file_props()?;
        let all_inherited = wcdb.all_inherited_props()?;
        let empty_props = BTreeMap::new();
        // Files whose size, mtime and normalization-relevant properties are
        // unchanged since they were last hashed are not read again. Files
        // modified within the last seconds are not cached (their mtime could
        // still change without a visible difference: "racily clean").
        let cache = wcdb.stat_cache()?;
        let racy_cutoff = std::time::SystemTime::now() - RACY_WINDOW;
        let mut fresh = Vec::new();

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
            let stamp = StatEntry {
                size: md.len() as i64,
                mtime_ns: mtime_ns(&md),
                props_key: props_fingerprint(file_props, &inherited, is_symlink, executable),
                blob_id: String::new(),
                is_binary: false,
            };
            if let Some(hit) = cache.get(&rel)
                && hit.same_stamp(&stamp)
                && (!persist || storage::object_exists(self, &hit.blob_id))
            {
                let props = node_props(file_props, is_symlink, executable);
                entries.push(FileEntry {
                    path: rel,
                    blob_id: hit.blob_id.clone(),
                    executable: has_svn_prop(&props, "svn:executable"),
                    is_binary: hit.is_binary,
                    props,
                    copy_from_path: None,
                    copy_from_rev: None,
                    node_id: None,
                    copy_id: None,
                    created_rev: None,
                });
                continue;
            }

            let raw = if is_symlink {
                let target = fs::read_link(&abs)?;
                format!("link {}", target.to_string_lossy()).into_bytes()
            } else {
                fs::read(&abs)?
            };
            let entry = self.finalize_entry(
                &rel,
                raw,
                EntryInputs {
                    is_symlink,
                    executable,
                    file_props,
                    inherited: &inherited,
                },
                persist,
            )?;
            if md.modified().is_ok_and(|m| m < racy_cutoff) {
                fresh.push((
                    rel,
                    StatEntry {
                        blob_id: entry.blob_id.clone(),
                        is_binary: entry.is_binary,
                        ..stamp
                    },
                ));
            }
            entries.push(entry);
        }
        if !fresh.is_empty() {
            wcdb.update_stat_cache(&fresh)?;
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
            .walk_files(&self.root, Some(&ignore))?
            .into_iter()
            .filter(|rel| {
                !versioned.contains(rel)
                    && !ignore.is_match(rel)
                    && !is_under_external(rel, &scope.externals)
            })
            .collect())
    }

    /// Every file or symlink below `dir` with a representable repository
    /// path, sorted. Metadata directories are pruned, and so are directories
    /// matched by `ignore` (other than `dir` itself): an ignored
    /// `node_modules` is never descended into.
    pub(super) fn walk_files(&self, dir: &Path, ignore: Option<&GlobSet>) -> Result<Vec<String>> {
        let root = self.root.clone();
        let start = dir.to_path_buf();
        let walker = WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(move |e| {
                if is_excluded_path(e.path(), &root) {
                    return false;
                }
                match ignore {
                    Some(ignore) if e.file_type().is_dir() && e.path() != start => {
                        rel_from_fs(&root, e.path()).is_none_or(|rel| !is_ignored_dir(ignore, &rel))
                    }
                    _ => true,
                }
            });
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
            .base_files(&*self.wcdb()?)?
            .into_iter()
            .find(|f| f.path == path))
    }

    /// Repository-form bytes of a working file (eol normalized, keywords
    /// contracted) — what a commit would store.
    pub fn working_repo_bytes(&self, path: &str) -> Result<Vec<u8>> {
        self.normalized_working_bytes(path)
    }

    /// Files of the BASE revision (empty before the first update/commit).
    pub(super) fn base_files(&self, wcdb: &WcDb) -> Result<Vec<FileEntry>> {
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
                self.walk_files(&abs, Some(&ignore))?
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
    pub(super) fn working_entry(
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
            EntryInputs {
                is_symlink,
                executable,
                file_props: &file_props,
                inherited: &inherited,
            },
            persist,
        )
    }

    pub(super) fn finalize_entry(
        &self,
        rel: &str,
        raw: Vec<u8>,
        inputs: EntryInputs<'_>,
        persist: bool,
    ) -> Result<FileEntry> {
        let (bytes, props, is_binary) = repo_form(
            raw,
            inputs.is_symlink,
            inputs.file_props,
            inputs.inherited,
            inputs.executable,
        );
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
    pub(super) fn normalized_working_bytes(&self, rel: &str) -> Result<Vec<u8>> {
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
    pub(super) fn copy_origin(&self, wcdb: &WcDb, path: &str) -> Result<Option<String>> {
        match wcdb.schedule()?.get(path) {
            Some(s) if s.op == ScheduleOp::Add => Ok(s.copy_from.clone()),
            Some(_) => Err(VcsError::NotVersioned(path.to_owned())),
            None if self.base_files(wcdb)?.iter().any(|f| f.path == path) => {
                Ok(Some(path.to_owned()))
            }
            None => Err(VcsError::NotVersioned(path.to_owned())),
        }
    }

    pub(super) fn copy_endpoints(&self, src: &str, dst: &str) -> Result<(PathBuf, PathBuf)> {
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

    /// After the depth was narrowed, remove BASE files that fell out of the
    /// working-copy scope — only unmodified ones. Local modifications, scheduled
    /// changes and unversioned files are never deleted.
    pub(super) fn prune_working_to_depth(&self) -> Result<()> {
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

    pub(super) fn sync_wcdb(&self) -> Result<()> {
        let working = self.snapshot_working_copy()?;
        self.sync_wcdb_from(&working)?;
        Ok(())
    }

    /// Refresh wc.db node/prop tables from an already-computed working snapshot
    /// and return the change set relative to the base revision.
    pub(super) fn sync_wcdb_from(&self, working: &[FileEntry]) -> Result<Vec<FileChange>> {
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
        let versioned = versioned_paths(&self.base_files(&wcdb)?, &schedule);
        wcdb.retain_file_props(&versioned)?;
        wcdb.retain_stat_cache(&versioned)?;
        Ok(changes)
    }

    pub(super) fn collect_ignore_patterns(&self, wcdb: &WcDb) -> Result<Vec<String>> {
        // Metadata directories are pruned structurally; everything else is
        // configuration (no built-in "target" or other project conventions).
        let mut patterns = Vec::new();

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

/// Everything besides the raw bytes that determines a working file's entry.
pub(super) struct EntryInputs<'a> {
    pub(super) is_symlink: bool,
    pub(super) executable: bool,
    pub(super) file_props: &'a BTreeMap<String, String>,
    pub(super) inherited: &'a BTreeMap<String, String>,
}

/// Which BASE paths the working copy materializes (sparse depth, externals).
pub(super) struct WcScope {
    pub(super) depth: Depth,
    pub(super) ambient: BTreeMap<String, (String, bool)>,
    pub(super) externals: BTreeSet<String>,
}

/// The versioned set: BASE paths not scheduled for deletion plus scheduled
/// additions.
pub(super) fn versioned_paths(
    base: &[FileEntry],
    schedule: &BTreeMap<String, Scheduled>,
) -> BTreeSet<String> {
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
pub(super) fn reconcile_schedule(wcdb: &WcDb, new_base: &[FileEntry]) -> Result<()> {
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
pub(super) fn mark_conflicts(
    changes: &mut Vec<FileChange>,
    conflicts: &BTreeMap<String, ConflictRecord>,
) {
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
pub(super) fn apply_scheduled_copies(
    changes: &mut [FileChange],
    schedule: &BTreeMap<String, Scheduled>,
) {
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
pub(super) fn empty_blob_id() -> String {
    blake3::hash(&[]).to_hex().to_string()
}

pub(super) fn build_ignore_globset(patterns: &[String]) -> GlobSet {
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
pub(super) fn is_excluded_path(path: &Path, root: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    if rel.as_os_str().is_empty() {
        return false;
    }
    // Metadata directories are pruned at any depth (nested checkouts).
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    is_reserved_component(name)
}

/// A directory is ignored when a pattern matches it or would match every
/// file inside it (`build/**`, `**/node_modules/**`, `tmp/*`).
pub(super) fn is_ignored_dir(ignore: &GlobSet, rel: &str) -> bool {
    ignore.is_match(rel) || ignore.is_match(format!("{rel}/{IGNORE_PROBE}"))
}

/// A file name no ignore pattern plausibly targets on its own, used to ask
/// whether a pattern covers a whole directory.
pub(super) const IGNORE_PROBE: &str = "\u{1}vcrs-probe";

/// Files modified this recently are re-hashed instead of trusting the stat
/// cache (timestamp granularity could hide a same-size rewrite).
pub(super) const RACY_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

pub(super) fn mtime_ns(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64)
}

/// Everything normalization of a file depends on besides its content.
pub(super) fn props_fingerprint(
    file_props: &BTreeMap<String, String>,
    inherited: &BTreeMap<String, String>,
    is_symlink: bool,
    executable: bool,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(
        &serde_json::to_vec(&(file_props, inherited, is_symlink, executable)).unwrap_or_default(),
    );
    hasher.finalize().to_hex().to_string()
}

/// Resolve inherited properties for `path` from the pre-loaded scope map,
/// shallow scopes first so deeper scopes win (matching `inherited_props_for_path`).
pub(super) fn resolve_inherited(
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

pub(super) fn is_under_external(path: &str, external_paths: &BTreeSet<String>) -> bool {
    external_paths
        .iter()
        .any(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")))
}

/// Revision metadata and working-copy state needed to materialize a file.
pub(super) struct CheckoutMeta<'a> {
    pub(super) revision: Option<i64>,
    pub(super) author: Option<&'a str>,
    pub(super) date: Option<DateTime<Utc>>,
    pub(super) has_lock_token: bool,
    /// Inherited properties of the path (normalization only).
    pub(super) inherited: &'a BTreeMap<String, String>,
}

pub(super) fn write_entry_to_working(
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

pub(super) fn remove_path_if_exists(path: &Path) -> Result<()> {
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

/// Write a conflict artifact next to `abs` (e.g. `file.mine`), replacing any
/// existing entry without following a symbolic link planted at that name.
pub(super) fn write_sibling(abs: &Path, suffix: &str, content: &[u8]) -> Result<String> {
    let mut name = abs.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    let target = abs.with_file_name(name);
    remove_path_if_exists(&target)?;
    fs::write(&target, content)?;
    Ok(target.display().to_string())
}

#[cfg(unix)]
pub(super) fn try_create_symlink(path: &Path, target: &str) -> Result<()> {
    std::os::unix::fs::symlink(target, path)?;
    Ok(())
}

#[cfg(windows)]
pub(super) fn try_create_symlink(path: &Path, target: &str) -> Result<()> {
    let target_path = Path::new(target);
    let md = fs::metadata(target_path).ok();
    if md.as_ref().is_some_and(|m| m.is_dir()) {
        std::os::windows::fs::symlink_dir(target, path)?;
    } else {
        std::os::windows::fs::symlink_file(target, path)?;
    }
    Ok(())
}

pub(super) fn set_readonly_if_supported(path: &Path, readonly: bool) -> Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(readonly);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(unix)]
pub(super) fn set_executable_if_supported(path: &Path, executable: bool) -> Result<()> {
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
pub(super) fn set_executable_if_supported(_path: &Path, _executable: bool) -> Result<()> {
    Ok(())
}

/// Make the executable bit of a (non-symlink) working file match its
/// `svn:executable` property.
pub(super) fn sync_executable_bit(abs: &Path, props: &BTreeMap<String, String>) -> Result<()> {
    if fs::symlink_metadata(abs).is_ok_and(|m| m.is_file()) {
        set_executable_if_supported(abs, has_svn_prop(props, "svn:executable"))?;
    }
    Ok(())
}

pub(super) fn path_allowed_by_ambient_depth(
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
            && let Ok(parsed) = d.parse::<Depth>()
        {
            current_depth = parsed;
        }
    }
    false
}

pub(super) fn depth_to_str(depth: Depth) -> &'static str {
    match depth {
        Depth::Empty => "empty",
        Depth::Files => "files",
        Depth::Immediates => "immediates",
        Depth::Infinity => "infinity",
    }
}

impl WcScope {
    pub(super) fn load(wcdb: &WcDb) -> Result<Self> {
        Ok(Self {
            depth: wcdb.depth()?.parse().unwrap_or(Depth::Infinity),
            ambient: wcdb.ambient_depth_map()?,
            externals: wcdb.list_externals()?.into_iter().map(|e| e.path).collect(),
        })
    }

    pub(super) fn contains(&self, path: &str) -> bool {
        !is_under_external(path, &self.externals)
            && path_allowed_by_ambient_depth(path, self.depth, &self.ambient)
    }
}
