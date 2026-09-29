use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use similar::TextDiff;

use crate::diff::unified_diff;
use crate::error::{Result, VcsError};
use crate::path::safe_join;
use crate::ra::{FileRaSession, RaSession, RemoteConfig};
use crate::repo::{GcStats, MergeOutcome, Repository};
use crate::types::{
    BlameLine, ChangeKind, ChangedPath, Commit, Depth, DiffHunk, FileChange, FileEntry,
    RevisionRange,
};
use crate::wcdb::ExternalDef;

const STAGE_FILE: &str = ".vcrs/client-stage-index.json";

#[derive(Debug, Clone)]
pub struct Client {
    repo: Repository,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StageIndex {
    version: u32,
    #[serde(default)]
    staged_files: BTreeSet<String>,
    #[serde(default)]
    staged_hunks: BTreeMap<String, BTreeSet<usize>>,
}

impl Default for StageIndex {
    fn default() -> Self {
        Self {
            version: 2,
            staged_files: BTreeSet::new(),
            staged_hunks: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct ComputedHunk {
    index: usize,
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
    preview: String,
}

impl Client {
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        let repo = Repository::init(path.as_ref())?;
        Ok(Self { repo })
    }

    pub fn discover(path: impl AsRef<Path>) -> Result<Self> {
        let repo = Repository::discover(path.as_ref())?;
        Ok(Self { repo })
    }

    /// Enable or disable execution of `.vcrs/hooks` scripts for operations
    /// performed through this client.
    pub fn with_hooks(mut self, enabled: bool) -> Self {
        self.repo.set_hooks_enabled(enabled);
        self
    }

    pub fn root(&self) -> &Path {
        &self.repo.root
    }

    pub fn status(&self) -> Result<Vec<FileChange>> {
        self.repo.status()
    }

    /// Remove unreferenced blobs from the object store.
    pub fn gc(&self) -> Result<GcStats> {
        self.repo.gc()
    }

    pub fn commit(&self, message: &str, author: &str) -> Result<Commit> {
        self.repo.commit(message, author)
    }

    pub fn commit_and_push(&self, message: &str, author: &str) -> Result<Commit> {
        let _lock = self.repo.lock()?;
        let commit = self.repo.commit(message, author)?;
        if let Some(cfg) = self.remote_config()? {
            let ra = FileRaSession::from_url(&cfg.url, cfg.username.as_deref())?;
            ra.push(&self.repo.root)?;
        }
        Ok(commit)
    }

    pub fn staged_paths(&self) -> Result<Vec<String>> {
        let _lock = self.repo.lock()?;
        let changed = self.status_path_set()?;
        let mut idx = self.load_stage_index()?;
        idx.retain_changed(&changed);
        self.save_stage_index(&idx)?;
        Ok(idx.all_paths().into_iter().collect())
    }

    pub fn staged_status(&self) -> Result<Vec<FileChange>> {
        let staged: BTreeSet<String> = self.staged_paths()?.into_iter().collect();
        Ok(self
            .status()?
            .into_iter()
            .filter(|c| staged.contains(&c.path))
            .collect())
    }

    pub fn stage_all(&self) -> Result<Vec<String>> {
        let _lock = self.repo.lock()?;
        let mut idx = self.load_stage_index()?;
        idx.staged_files = self.status_path_set()?;
        idx.staged_hunks.clear();
        self.save_stage_index(&idx)?;
        Ok(idx.staged_files.into_iter().collect())
    }

    pub fn clear_staging(&self) -> Result<()> {
        let _lock = self.repo.lock()?;
        self.save_stage_index(&StageIndex::default())?;
        Ok(())
    }

    pub fn stage_paths(&self, paths: &[String]) -> Result<Vec<String>> {
        let _lock = self.repo.lock()?;
        let allowed = self.status_path_set()?;
        let mut idx = self.load_stage_index()?;
        for path in paths {
            let norm = normalize_rel(path);
            if allowed.contains(&norm) {
                idx.staged_files.insert(norm.clone());
                idx.staged_hunks.remove(&norm);
            }
        }
        self.save_stage_index(&idx)?;
        Ok(idx.all_paths().into_iter().collect())
    }

    pub fn unstage_paths(&self, paths: &[String]) -> Result<Vec<String>> {
        let _lock = self.repo.lock()?;
        let mut idx = self.load_stage_index()?;
        for path in paths {
            let norm = normalize_rel(path);
            idx.staged_files.remove(&norm);
            idx.staged_hunks.remove(&norm);
        }
        self.save_stage_index(&idx)?;
        Ok(idx.all_paths().into_iter().collect())
    }

    pub fn hunks(&self, path: &str) -> Result<Vec<DiffHunk>> {
        let norm = normalize_rel(path);
        let change = self
            .status()?
            .into_iter()
            .find(|c| c.path == norm)
            .ok_or_else(|| VcsError::HunkStagingUnsupported { path: norm.clone() })?;
        ensure_hunk_supported(&change)?;

        let (base, working) = self.load_text_pair(&norm)?;
        let computed = compute_hunks(&base, &working);
        let idx = self.load_stage_index()?;
        let selected = idx.staged_hunks.get(&norm).cloned().unwrap_or_default();

        Ok(computed
            .into_iter()
            .map(|h| DiffHunk {
                index: h.index,
                old_start: h.old_start + 1,
                old_len: h.old_end.saturating_sub(h.old_start),
                new_start: h.new_start + 1,
                new_len: h.new_end.saturating_sub(h.new_start),
                preview: h.preview,
                staged: selected.contains(&h.index),
            })
            .collect())
    }

    pub fn stage_hunks(&self, path: &str, indices: &[usize]) -> Result<Vec<usize>> {
        let _lock = self.repo.lock()?;
        let norm = normalize_rel(path);
        let hunks = self.hunks(&norm)?;
        let max = hunks.len();
        let mut idx = self.load_stage_index()?;
        idx.staged_files.remove(&norm);
        let set = idx.staged_hunks.entry(norm.clone()).or_default();
        for i in indices {
            if *i >= max {
                return Err(VcsError::InvalidHunkIndex {
                    path: norm,
                    index: *i,
                });
            }
            set.insert(*i);
        }
        let out: Vec<usize> = set.iter().copied().collect();
        if set.is_empty() {
            idx.staged_hunks.remove(&norm);
        }
        self.save_stage_index(&idx)?;
        Ok(out)
    }

    pub fn unstage_hunks(&self, path: &str, indices: &[usize]) -> Result<Vec<usize>> {
        let _lock = self.repo.lock()?;
        let norm = normalize_rel(path);
        let mut idx = self.load_stage_index()?;
        let mut remove_entry = false;
        let out = if let Some(set) = idx.staged_hunks.get_mut(&norm) {
            for i in indices {
                set.remove(i);
            }
            if set.is_empty() {
                remove_entry = true;
            }
            set.iter().copied().collect()
        } else {
            Vec::new()
        };
        if remove_entry {
            idx.staged_hunks.remove(&norm);
        }
        self.save_stage_index(&idx)?;
        Ok(out)
    }

    pub fn commit_staged(&self, message: &str, author: &str, push: bool) -> Result<Commit> {
        let _lock = self.repo.lock()?;
        let status = self.status()?;
        if status.is_empty() {
            return Err(VcsError::NoStagedChanges);
        }

        let mut change_map = BTreeMap::new();
        for ch in &status {
            change_map.insert(ch.path.clone(), ch.clone());
        }
        let changed_paths: BTreeSet<String> = change_map.keys().cloned().collect();

        let mut idx = self.load_stage_index()?;
        idx.retain_changed(&changed_paths);

        let staged_full: BTreeSet<String> = idx.staged_files.clone();
        let mut staged_partial = BTreeMap::<String, String>::new();

        for (path, hset) in idx.staged_hunks.clone() {
            if staged_full.contains(&path) || hset.is_empty() {
                continue;
            }
            let ch = change_map
                .get(&path)
                .ok_or_else(|| VcsError::HunkStagingUnsupported { path: path.clone() })?;
            ensure_hunk_supported(ch)?;

            let (base, working) = self.load_text_pair(&path)?;
            let hunks = compute_hunks(&base, &working);
            for i in &hset {
                if *i >= hunks.len() {
                    return Err(VcsError::InvalidHunkIndex {
                        path: path.clone(),
                        index: *i,
                    });
                }
            }
            let staged_text = apply_selected_hunks(&base, &working, &hunks, &hset);
            if staged_text != base {
                staged_partial.insert(path.clone(), staged_text);
            }
        }

        if staged_full.is_empty() && staged_partial.is_empty() {
            self.save_stage_index(&idx)?;
            return Err(VcsError::NoStagedChanges);
        }

        // Build the committed tree from the staged subset without ever mutating
        // the working copy, so an interruption cannot lose unstaged changes.
        let commit = self
            .repo
            .commit_selective(&staged_full, &staged_partial, message, author)?;

        if push && let Some(cfg) = self.remote_config()? {
            let ra = FileRaSession::from_url(&cfg.url, cfg.username.as_deref())?;
            ra.push(&self.repo.root)?;
        }

        idx.staged_files.clear();
        idx.staged_hunks.clear();
        self.save_stage_index(&idx)?;
        Ok(commit)
    }

    pub fn log(&self, limit: usize) -> Result<Vec<Commit>> {
        self.repo.log(limit)
    }

    pub fn log_range(
        &self,
        range_spec: &str,
        verbose_paths: bool,
        include_merged: bool,
        only_merged: bool,
    ) -> Result<Vec<Commit>> {
        let range = self.repo.parse_revision_range(range_spec)?;
        self.repo
            .log_range(range, verbose_paths, include_merged, only_merged)
    }

    pub fn parse_revision_range(&self, range_spec: &str) -> Result<RevisionRange> {
        self.repo.parse_revision_range(range_spec)
    }

    pub fn revert(&self, paths: &[String]) -> Result<Vec<FileChange>> {
        self.repo.revert_to_head(paths)
    }

    pub fn update_to_revision(&self, revision: &str) -> Result<Vec<FileChange>> {
        self.repo.update_to_revision(revision)
    }

    pub fn update_to_revision_with_depth(
        &self,
        revision: &str,
        depth: Option<Depth>,
    ) -> Result<Vec<FileChange>> {
        self.repo.update_to_revision_with_depth(revision, depth)
    }

    pub fn merge_from_revision(
        &self,
        revision: &str,
        dry_run: bool,
        record_only: bool,
    ) -> Result<MergeOutcome> {
        self.repo
            .merge_from_revision(revision, dry_run, record_only)
    }

    pub fn merge(&self, revision: &str, dry_run: bool, record_only: bool) -> Result<MergeOutcome> {
        self.merge_from_revision(revision, dry_run, record_only)
    }

    pub fn changed_files_in_revision(&self, revision: &str) -> Result<Vec<FileChange>> {
        self.repo.commit_changed_files(revision)
    }

    pub fn changed_paths_in_revision(&self, revision: &str) -> Result<Vec<ChangedPath>> {
        self.repo.commit_changed_paths(revision)
    }

    pub fn blame(&self, path: &str, revision: Option<&str>) -> Result<Vec<BlameLine>> {
        self.repo.blame_file(path, revision)
    }

    pub fn set_property(&self, path: &str, name: &str, value: &str) -> Result<()> {
        self.repo.set_property(path, name, value)
    }

    pub fn get_property(&self, path: &str, name: &str) -> Result<Option<String>> {
        self.repo.get_property(path, name)
    }

    pub fn del_property(&self, path: &str, name: &str) -> Result<()> {
        self.repo.del_property(path, name)
    }

    pub fn add_ignore(&self, pattern: &str) -> Result<()> {
        self.repo.add_ignore(pattern)
    }

    pub fn list_ignores(&self) -> Result<Vec<(String, String, bool)>> {
        self.repo.list_ignores()
    }

    pub fn set_external(
        &self,
        path: &str,
        target_url: &str,
        revision: Option<String>,
    ) -> Result<()> {
        self.repo.set_external(path, target_url, revision)
    }

    pub fn list_externals(&self) -> Result<Vec<ExternalDef>> {
        self.repo.list_externals()
    }

    pub fn set_inherited_property(&self, scope_path: &str, name: &str, value: &str) -> Result<()> {
        self.repo.set_inherited_property(scope_path, name, value)
    }

    pub fn list_inherited_properties(&self) -> Result<Vec<(String, String, String)>> {
        self.repo.list_inherited_properties()
    }

    pub fn set_changelist(&self, path: &str, changelist: Option<&str>) -> Result<()> {
        self.repo.set_changelist(path, changelist)
    }

    pub fn list_changelists(&self) -> Result<Vec<(String, String)>> {
        self.repo.list_changelists()
    }

    pub fn set_depth(&self, depth: Depth) -> Result<()> {
        self.repo.set_depth(depth)
    }

    pub fn depth(&self) -> Result<Depth> {
        self.repo.depth()
    }

    pub fn copy_path(&self, src: &str, dst: &str) -> Result<()> {
        self.repo.copy_path(src, dst)
    }

    pub fn move_path(&self, src: &str, dst: &str) -> Result<()> {
        self.repo.move_path(src, dst)
    }

    pub fn diff_working(&self, path_filter: Option<&str>) -> Result<Vec<(String, String)>> {
        let status = self.repo.status()?;
        if status.is_empty() {
            return Ok(Vec::new());
        }

        let root = self.repo.root.clone();
        let current = self.repo.snapshot_working_copy()?;
        let current_map: BTreeMap<&str, &FileEntry> =
            current.iter().map(|f| (f.path.as_str(), f)).collect();

        let head = self.repo.head_commit()?;
        let head_map: BTreeMap<&str, &FileEntry> = head
            .as_ref()
            .map(|c| c.files.iter().map(|f| (f.path.as_str(), f)).collect())
            .unwrap_or_default();

        let mut out = Vec::new();
        for change in status {
            if let Some(filter) = path_filter
                && !change.path.contains(filter)
            {
                continue;
            }

            let old_entry = head_map.get(change.path.as_str()).copied();
            let new_entry = current_map.get(change.path.as_str()).copied();

            let old_text = match old_entry {
                Some(entry) => {
                    String::from_utf8_lossy(&self.repo.read_blob(&entry.blob_id)?).to_string()
                }
                None => String::new(),
            };

            let current_abs = safe_join(&root, &change.path)?;
            let new_text = if current_abs.exists() {
                String::from_utf8_lossy(&std::fs::read(&current_abs)?).to_string()
            } else {
                String::new()
            };

            let old_label = format!("a/{}", change.path);
            let new_label = format!("b/{}", change.path);

            let mut rendered = String::new();
            if change.text_modified {
                let old_binary = old_entry.is_some_and(|e| e.is_binary);
                let new_binary = new_entry.is_some_and(|e| e.is_binary);
                if change.is_binary || old_binary || new_binary {
                    rendered.push_str(&render_binary_diff_notice(&change.path));
                } else {
                    rendered.push_str(&unified_diff(&old_text, &new_text, &old_label, &new_label));
                }
            }

            if change.props_modified {
                rendered.push_str(&render_property_diff(
                    old_entry.map(|e| &e.props),
                    new_entry.map(|e| &e.props),
                    &change.path,
                ));
            }

            if !rendered.is_empty() {
                out.push((change.path, rendered));
            }
        }

        Ok(out)
    }

    pub fn diff_peg(&self, peg_spec: &str) -> Result<String> {
        let (path, rev) = parse_peg_path(peg_spec)?;
        let old_entry = self
            .repo
            .file_entry_at_revision(rev, &path)?
            .ok_or_else(|| VcsError::CommitNotFound(format!("r{rev}:{path}")))?;

        let current_abs = safe_join(&self.repo.root, &path)?;
        let new_bytes = if current_abs.exists() {
            std::fs::read(&current_abs)?
        } else {
            Vec::new()
        };
        let old_bytes = self.repo.read_blob(&old_entry.blob_id)?;

        if old_entry.is_binary || is_binary_content(&new_bytes) {
            return Ok(render_binary_diff_notice(&path));
        }

        let old_text = String::from_utf8_lossy(&old_bytes).to_string();
        let new_text = String::from_utf8_lossy(&new_bytes).to_string();
        Ok(unified_diff(
            &old_text,
            &new_text,
            &format!("a/{path}@{rev}"),
            &format!("b/{path}"),
        ))
    }

    pub fn diff(&self, path: Option<&str>, peg: Option<&str>) -> Result<String> {
        if let Some(peg) = peg {
            return self.diff_peg(peg);
        }
        let mut out = String::new();
        for (_path, patch) in self.diff_working(path)? {
            out.push_str(&patch);
        }
        Ok(out)
    }

    pub fn cat_revision_file(&self, revision: &str, path: &str) -> Result<Vec<u8>> {
        let rev = self.repo.resolve_revision_spec(revision)?;
        let commit = self.repo.read_commit_by_revision(rev)?;
        let file = commit
            .files
            .iter()
            .find(|f| f.path == path)
            .ok_or_else(|| VcsError::CommitNotFound(format!("r{rev}:{path}")))?;
        self.repo.read_blob(&file.blob_id)
    }

    pub fn cat_peg(&self, peg_spec: &str) -> Result<Vec<u8>> {
        let (path, rev) = parse_peg_path(peg_spec)?;
        self.cat_revision_file(&rev.to_string(), &path)
    }

    pub fn checkout_remote(
        remote_url: &str,
        dest: impl AsRef<Path>,
        username: Option<&str>,
    ) -> Result<Self> {
        let ra = FileRaSession::from_url(remote_url, username)?;
        ra.checkout(dest.as_ref())?;
        Self::discover(dest)
    }

    pub fn set_remote(&self, url: &str, username: Option<&str>) -> Result<()> {
        FileRaSession::save_config(
            &self.repo.root,
            &RemoteConfig {
                url: url.to_owned(),
                username: username.map(|s| s.to_owned()),
            },
        )
    }

    pub fn remote_config(&self) -> Result<Option<RemoteConfig>> {
        FileRaSession::load_config(&self.repo.root)
    }

    pub fn switch_remote(&self, url: &str, username: Option<&str>) -> Result<()> {
        self.set_remote(url, username)?;
        self.pull_remote()
    }

    pub fn pull_remote(&self) -> Result<()> {
        if !self.status()?.is_empty() {
            return Err(VcsError::WorkingCopyDirty);
        }
        let cfg = self.remote_config()?.ok_or(VcsError::RepositoryNotFound)?;
        let ra = FileRaSession::from_url(&cfg.url, cfg.username.as_deref())?;
        ra.pull(&self.repo.root)
    }

    pub fn push_remote(&self) -> Result<()> {
        let cfg = self.remote_config()?.ok_or(VcsError::RepositoryNotFound)?;
        let ra = FileRaSession::from_url(&cfg.url, cfg.username.as_deref())?;
        ra.push(&self.repo.root)
    }

    pub fn pull(&self) -> Result<()> {
        self.pull_remote()
    }

    pub fn push(&self) -> Result<()> {
        self.push_remote()
    }

    pub fn lock_remote(&self, path: &str) -> Result<()> {
        let cfg = self.remote_config()?.ok_or(VcsError::RepositoryNotFound)?;
        let ra = FileRaSession::from_url(&cfg.url, cfg.username.as_deref())?;
        ra.lock(path)?;
        let owner = cfg.username.as_deref().unwrap_or("anonymous");
        self.repo
            .set_lock_token_local(path, Some(&format!("token:{path}")), Some(owner))
    }

    pub fn unlock_remote(&self, path: &str) -> Result<()> {
        let cfg = self.remote_config()?.ok_or(VcsError::RepositoryNotFound)?;
        let ra = FileRaSession::from_url(&cfg.url, cfg.username.as_deref())?;
        ra.unlock(path)?;
        self.repo.set_lock_token_local(path, None, None)
    }

    pub fn lock(&self, path: &str) -> Result<()> {
        self.lock_remote(path)
    }

    pub fn unlock(&self, path: &str) -> Result<()> {
        self.unlock_remote(path)
    }

    pub fn at(path: PathBuf) -> Result<Self> {
        Self::discover(path)
    }

    fn stage_file_path(&self) -> PathBuf {
        self.root().join(STAGE_FILE)
    }

    fn load_stage_index(&self) -> Result<StageIndex> {
        let path = self.stage_file_path();
        if !path.exists() {
            return Ok(StageIndex::default());
        }
        let bytes = fs::read(path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn save_stage_index(&self, idx: &StageIndex) -> Result<()> {
        let path = self.stage_file_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_vec_pretty(idx)?)?;
        Ok(())
    }

    fn status_path_set(&self) -> Result<BTreeSet<String>> {
        Ok(self.status()?.into_iter().map(|c| c.path).collect())
    }

    fn load_text_pair(&self, path: &str) -> Result<(String, String)> {
        let base_bytes = match self.cat_revision_file("HEAD", path) {
            Ok(bytes) => bytes,
            Err(VcsError::CommitNotFound(_)) => Vec::new(),
            Err(e) => return Err(e),
        };
        let base = String::from_utf8(base_bytes).map_err(|_| VcsError::HunkStagingUnsupported {
            path: path.to_owned(),
        })?;

        let abs = safe_join(self.root(), path)?;
        let working_bytes = if abs.exists() {
            fs::read(abs)?
        } else {
            Vec::new()
        };
        let working =
            String::from_utf8(working_bytes).map_err(|_| VcsError::HunkStagingUnsupported {
                path: path.to_owned(),
            })?;
        Ok((base, working))
    }
}

impl StageIndex {
    fn retain_changed(&mut self, changed: &BTreeSet<String>) {
        self.staged_files.retain(|p| changed.contains(p));
        self.staged_hunks
            .retain(|p, hs| changed.contains(p) && !hs.is_empty());
    }

    fn all_paths(&self) -> BTreeSet<String> {
        let mut out = self.staged_files.clone();
        out.extend(self.staged_hunks.keys().cloned());
        out
    }
}

fn ensure_hunk_supported(change: &FileChange) -> Result<()> {
    if change.kind != ChangeKind::Modified || change.is_binary || !change.text_modified {
        return Err(VcsError::HunkStagingUnsupported {
            path: change.path.clone(),
        });
    }
    Ok(())
}

fn normalize_rel(path: &str) -> String {
    crate::path::normalize_rel(path)
}

fn split_lines_keep_eol(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split_inclusive('\n').map(|s| s.to_owned()).collect()
}

fn compute_hunks(base: &str, working: &str) -> Vec<ComputedHunk> {
    let diff = TextDiff::from_lines(base, working);
    let mut out = Vec::new();
    for (idx, group) in diff.grouped_ops(3).iter().enumerate() {
        let Some(first) = group.first() else {
            continue;
        };
        let Some(last) = group.last() else {
            continue;
        };
        let old_start = first.old_range().start;
        let old_end = last.old_range().end;
        let new_start = first.new_range().start;
        let new_end = last.new_range().end;

        let mut preview = String::new();
        for op in group {
            for change in diff.iter_changes(op) {
                let sign = match change.tag() {
                    similar::ChangeTag::Equal => ' ',
                    similar::ChangeTag::Delete => '-',
                    similar::ChangeTag::Insert => '+',
                };
                preview.push(sign);
                preview.push_str(change.value());
            }
        }

        out.push(ComputedHunk {
            index: idx,
            old_start,
            old_end,
            new_start,
            new_end,
            preview,
        });
    }
    out
}

fn apply_selected_hunks(
    base: &str,
    working: &str,
    hunks: &[ComputedHunk],
    selected: &BTreeSet<usize>,
) -> String {
    let mut out_lines = split_lines_keep_eol(base);
    let working_lines = split_lines_keep_eol(working);
    let mut offset: isize = 0;

    for h in hunks {
        if !selected.contains(&h.index) {
            continue;
        }
        // Clamp every index: the working file may have changed since the hunks
        // were computed, so out-of-range slices must not panic.
        let len = out_lines.len();
        let start = ((h.old_start as isize + offset).max(0) as usize).min(len);
        let end = ((h.old_end as isize + offset).max(0) as usize).clamp(start, len);
        let replacement: Vec<String> = working_lines
            .get(h.new_start..h.new_end)
            .map(<[String]>::to_vec)
            .unwrap_or_default();
        out_lines.splice(start..end, replacement);
        offset +=
            h.new_end as isize - h.new_start as isize - (h.old_end as isize - h.old_start as isize);
    }

    out_lines.concat()
}

fn render_binary_diff_notice(path: &str) -> String {
    format!(
        "Index: {path}\n===================================================================\nCannot display: file marked as binary.\n\n"
    )
}

fn parse_peg_path(spec: &str) -> Result<(String, i64)> {
    let Some(at) = spec.rfind('@') else {
        return Err(VcsError::RevisionNotFound(format!(
            "peg revision missing in '{spec}'"
        )));
    };
    if at == 0 || at + 1 >= spec.len() {
        return Err(VcsError::RevisionNotFound(format!(
            "invalid peg revision in '{spec}'"
        )));
    }
    let path = spec[..at].to_owned();
    let rev: i64 = spec[at + 1..]
        .parse()
        .map_err(|_| VcsError::RevisionNotFound(spec.to_owned()))?;
    Ok((path, rev))
}

fn is_binary_content(bytes: &[u8]) -> bool {
    // Sniff only the leading window (like svn) rather than the whole file.
    let n = bytes.len().min(8192);
    let window = &bytes[..n];
    if window.contains(&0) {
        return true;
    }
    match std::str::from_utf8(window) {
        Ok(_) => false,
        Err(e) => !(n < bytes.len() && e.error_len().is_none()),
    }
}

fn render_property_diff(
    old_props: Option<&BTreeMap<String, String>>,
    new_props: Option<&BTreeMap<String, String>>,
    path: &str,
) -> String {
    let empty = BTreeMap::new();
    let old_props = old_props.unwrap_or(&empty);
    let new_props = new_props.unwrap_or(&empty);

    let mut keys = BTreeSet::new();
    keys.extend(old_props.keys().cloned());
    keys.extend(new_props.keys().cloned());

    let mut lines = Vec::new();
    for key in keys {
        let old = old_props.get(&key);
        let new = new_props.get(&key);
        if old == new {
            continue;
        }
        lines.push(format!("Modified: {key}"));
        lines.push(format!(
            "   - {}",
            old.map(String::as_str).unwrap_or("(none)")
        ));
        lines.push(format!(
            "   + {}",
            new.map(String::as_str).unwrap_or("(none)")
        ));
    }

    if lines.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str(&format!("Property changes on: {path}\n"));
    out.push_str("___________________________________________________________________\n");
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    out.push('\n');
    out
}
