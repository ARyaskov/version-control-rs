//! History: log, the revision index, integrating remote history (pull)
//! and upgrades of older repository layouts.

use super::*;

impl Repository {
    /// The newest `limit` commits, newest first. Only metadata is loaded:
    /// `files` is empty (use [`Repository::read_commit`] for file lists).
    pub fn log(&self, limit: usize) -> Result<Vec<Commit>> {
        let mut out = Vec::new();
        let mut next = self.head_commit_id()?;

        while let Some(id) = next {
            if out.len() >= limit {
                break;
            }
            let mut commit = storage::read_commit_header(self, &id)?;
            commit.files.clear();
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
            if let Ok(mut commit) = self.read_commit_header_by_revision(rev) {
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

    pub fn commit_changed_files(&self, revision: &str) -> Result<Vec<FileChange>> {
        let rev = self.resolve_revision_spec(revision)?;
        Ok(self.read_commit_header_by_revision(rev)?.changed_files)
    }

    pub fn commit_changed_paths(&self, revision: &str) -> Result<Vec<ChangedPath>> {
        let rev = self.resolve_revision_spec(revision)?;
        Ok(self.read_commit_header_by_revision(rev)?.changed_paths)
    }

    /// Commit ids from `head` back to the root (newest first).
    pub(super) fn chain_ids(&self, head: Option<&str>) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut cur = head.map(str::to_owned);
        while let Some(id) = cur {
            if out.contains(&id) {
                return Err(VcsError::Protocol(format!("cycle in history at {id}")));
            }
            cur = storage::read_commit_header(self, &id)?.parent;
            out.push(id);
        }
        Ok(out)
    }

    /// True when `ancestor` is `descendant` or one of its parents. Merely
    /// having the object locally is not enough: after an aborted pull the
    /// remote HEAD can be present without being part of the local history.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        Ok(self
            .chain_ids(Some(descendant))?
            .iter()
            .any(|id| id == ancestor))
    }

    /// Bring the local history and working copy to the remote HEAD whose
    /// objects were already copied in. Local commits that were never pushed
    /// are replayed on top of it (and renumbered) instead of being dropped;
    /// if one of them conflicts with the remote changes, nothing is changed.
    /// The working copy must be clean.
    pub(crate) fn integrate_remote_head(&self, remote_head: &str) -> Result<PullOutcome> {
        let _lock = self.lock()?;
        let remote_chain = self.chain_ids(Some(remote_head))?;
        let local_chain = self.chain_ids(self.head_commit_id()?.as_deref())?;

        if local_chain.first().is_none_or(|l| remote_chain.contains(l)) {
            // Fast-forward (or already up to date).
            self.set_head_commit_id(remote_head)?;
            self.update_to_revision("HEAD")?;
            return Ok(PullOutcome {
                head_revision: self.wcdb()?.head_revision()?,
                ..PullOutcome::default()
            });
        }
        if let Some(pos) = local_chain.iter().position(|id| id == remote_head) {
            // Local history already contains the remote: nothing to pull.
            self.update_to_revision("HEAD")?;
            return Ok(PullOutcome {
                head_revision: self.wcdb()?.head_revision()?,
                ahead: pos,
                ..PullOutcome::default()
            });
        }

        // Diverged: replay the local-only commits onto the remote HEAD.
        let local_only: Vec<&String> = local_chain
            .iter()
            .take_while(|id| !remote_chain.contains(id))
            .collect();
        let wcdb = self.wcdb()?;
        let old_base = self.base_files(&wcdb)?;
        let mut new_parent = storage::read_commit(self, remote_head)?;
        for id in local_only.iter().rev() {
            let local = storage::read_commit(self, id)?;
            let old_parent_files = match &local.parent {
                Some(p) => storage::read_commit(self, p)?.files,
                None => Vec::new(),
            };
            new_parent = self.rebase_commit(&local, &old_parent_files, &new_parent)?;
        }

        // The working copy is clean at the old local HEAD: move it to the new
        // tip first (BASE still names the old tree), then re-index.
        self.apply_tree_delta(
            &old_base,
            &new_parent.files,
            Some(&new_parent),
            None,
            true,
            false,
        )?;
        self.set_head_commit_id(&new_parent.id)?;
        self.sync_wcdb()?;
        Ok(PullOutcome {
            head_revision: new_parent.revision,
            rebased: local_only.len(),
            ahead: local_only.len(),
        })
    }

    /// Re-create `commit` (made on top of `old_parent_files`) on top of
    /// `new_parent`, merging its changes three-way against what the new
    /// parent changed. Fails without writing anything visible on conflict.
    pub(super) fn rebase_commit(
        &self,
        commit: &Commit,
        old_parent_files: &[FileEntry],
        new_parent: &Commit,
    ) -> Result<Commit> {
        let base: BTreeMap<&str, &FileEntry> = old_parent_files
            .iter()
            .map(|f| (f.path.as_str(), f))
            .collect();
        let theirs: BTreeMap<&str, &FileEntry> =
            commit.files.iter().map(|f| (f.path.as_str(), f)).collect();
        let mut result: BTreeMap<String, FileEntry> = new_parent
            .files
            .iter()
            .map(|f| (f.path.clone(), f.clone()))
            .collect();
        let keys: BTreeSet<&str> = base.keys().chain(theirs.keys()).copied().collect();
        let mut conflicts = Vec::new();
        for path in keys {
            let b = base.get(path).copied();
            let t = theirs.get(path).copied();
            if !changed_entry(b, t) {
                continue;
            }
            let o = result.get(path).cloned();
            if !changed_entry(b, o.as_ref()) {
                match t {
                    Some(te) => {
                        result.insert(path.to_owned(), te.clone());
                    }
                    None => {
                        result.remove(path);
                    }
                }
                continue;
            }
            if !changed_entry(o.as_ref(), t) {
                continue;
            }
            match (b, o, t) {
                (Some(be), Some(oe), Some(te))
                    if !(be.is_binary || oe.is_binary || te.is_binary) =>
                {
                    let read = |e: &FileEntry| -> Result<String> {
                        String::from_utf8(self.read_blob(&e.blob_id)?)
                            .map_err(|_| VcsError::Protocol(format!("{path} is not UTF-8")))
                    };
                    let (text, clean) = three_way_text(&read(be)?, &read(&oe)?, &read(te)?);
                    let (props, prop_conflicts) = merge_props(&be.props, &oe.props, &te.props);
                    if !clean || !prop_conflicts.is_empty() {
                        conflicts.push(path.to_owned());
                        continue;
                    }
                    let mut merged = te.clone();
                    merged.blob_id = storage::write_blob(self, text.as_bytes())?;
                    merged.executable = has_svn_prop(&props, "svn:executable");
                    merged.props = props;
                    result.insert(path.to_owned(), merged);
                }
                _ => conflicts.push(path.to_owned()),
            }
        }
        if !conflicts.is_empty() {
            return Err(VcsError::Diverged {
                paths: conflicts.join(", "),
            });
        }

        let revision = new_parent.revision + 1;
        let mut files: Vec<FileEntry> = result.into_values().collect();
        for f in &mut files {
            if f.copy_from_path.is_some() && theirs.contains_key(f.path.as_str()) {
                f.copy_from_rev = Some(new_parent.revision);
            }
        }
        let changed = compute_changed_files(Some(&new_parent.files), &files);
        assign_node_identity(Some(new_parent), &mut files, revision);
        // Mergeinfo: the new parent's plus whatever this commit added.
        let mut mergeinfo = new_parent.mergeinfo.clone();
        let old_parent_mergeinfo = match &commit.parent {
            Some(p) => storage::read_commit(self, p)?.mergeinfo,
            None => BTreeMap::new(),
        };
        let mut added = Vec::new();
        for (path, revs) in &commit.mergeinfo {
            let before: BTreeSet<i64> = old_parent_mergeinfo
                .get(path)
                .map(|v| parse_mergeinfo_value(v).into_iter().collect())
                .unwrap_or_default();
            for rev in parse_mergeinfo_value(revs) {
                if !before.contains(&rev) {
                    added.push((path.clone(), rev));
                }
            }
        }
        mergeinfo = build_mergeinfo(&added, mergeinfo);

        let mut rebased = Commit {
            id: String::new(),
            format: storage::COMMIT_FORMAT,
            tree: Some(storage::write_tree(self, &files)?),
            revision,
            parent: Some(new_parent.id.clone()),
            parent_revision: Some(new_parent.revision),
            author: commit.author.clone(),
            message: commit.message.clone(),
            created_at: commit.created_at,
            changed_paths: build_changed_paths(Some(new_parent), &changed),
            changed_files: changed,
            files,
            mergeinfo,
            revprops: commit.revprops.clone(),
            txn_id: None,
        };
        rebased.id = storage::compute_commit_id(&rebased)?;
        storage::write_commit(self, &rebased)?;
        Ok(rebased)
    }

    /// Rebuild the revision index (revisions + merge edges) from the parent
    /// chain ending at `head`. Only commits on that chain are indexed, so an
    /// unreferenced object can never shadow a revision number.
    pub(super) fn index_chain(&self, head: Option<&str>) -> Result<()> {
        let mut chain = Vec::new();
        let mut seen = BTreeSet::new();
        let mut cur = head.map(str::to_owned);
        while let Some(id) = cur {
            if !seen.insert(id.clone()) {
                return Err(VcsError::Protocol(format!("cycle in history at {id}")));
            }
            let c = storage::read_commit_header(self, &id)?;
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

    /// Upgrade metadata written by versions before 0.3: HEAD lived in a file
    /// updated separately from the revision index, with a JSON undo/redo
    /// journal around it. The revision index is rebuilt from that HEAD's
    /// parent chain (the authoritative history) and both files are retired;
    /// leftover commit objects of interrupted transactions are unreferenced and
    /// collected by gc.
    pub(super) fn migrate_legacy_layout(&self) -> Result<()> {
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
        // Locks used to live in a JSON file rewritten without atomicity.
        let locks_file = vcrs.join("locks.json");
        if locks_file.exists() {
            let legacy: BTreeMap<String, String> =
                serde_json::from_slice(&fs::read(&locks_file)?).unwrap_or_default();
            for (path, owner) in legacy {
                if crate::path::validate_rel_path(&path).is_ok() {
                    let _ = self.lock_path(&path, &owner);
                }
            }
            fs::remove_file(&locks_file)?;
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
    pub(super) fn migrate_explicit_props(&self, wcdb: &WcDb) -> Result<()> {
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
}
