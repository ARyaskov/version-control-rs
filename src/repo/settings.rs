//! Working-copy settings and repository locks: properties, changelists,
//! depth, ignore rules, externals, lock tokens and path locks.

use super::*;

impl Repository {
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

    pub(super) fn path_requires_lock(&self, path: &str) -> Result<bool> {
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

    /// Take the repository-level lock on `path` for `owner`; locking a path
    /// you already hold returns the existing lock.
    pub fn lock_path(&self, path: &str, owner: &str) -> Result<PathLock> {
        crate::path::validate_rel_path(path)?;
        let now = Utc::now();
        let mut hasher = blake3::Hasher::new();
        hasher.update(path.as_bytes());
        hasher.update(owner.as_bytes());
        hasher.update(&now.timestamp_nanos_opt().unwrap_or_default().to_le_bytes());
        hasher.update(&std::process::id().to_le_bytes());
        let candidate = PathLock {
            owner: owner.to_owned(),
            token: format!("opaquelocktoken:{}", hasher.finalize().to_hex()),
            created_at: now.to_rfc3339(),
        };
        let held = self.wcdb()?.acquire_path_lock(path, &candidate)?;
        if held.owner != owner {
            return Err(VcsError::LockConflict {
                path: path.to_owned(),
                owner: held.owner,
            });
        }
        Ok(held)
    }

    /// Release `owner`'s repository-level lock on `path`.
    pub fn unlock_path(&self, path: &str, owner: &str) -> Result<()> {
        match self.wcdb()?.release_path_lock(path, owner)? {
            Some(holder) => Err(VcsError::LockConflict {
                path: path.to_owned(),
                owner: holder,
            }),
            None => Ok(()),
        }
    }

    /// Repository-level locks by path.
    pub fn path_locks(&self) -> Result<BTreeMap<String, PathLock>> {
        self.wcdb()?.path_locks()
    }

    /// Refuse a commit by `author` that touches a path locked by someone
    /// else, or modifies/deletes an `svn:needs-lock` file `author` has not
    /// locked. `parent` is the tree the changes apply to.
    pub(crate) fn check_path_locks(
        &self,
        author: &str,
        parent: Option<&[FileEntry]>,
        new_files: &[FileEntry],
        changed: &[FileChange],
    ) -> Result<()> {
        let locks = self.path_locks()?;
        let needs_lock = |files: Option<&[FileEntry]>, path: &str| {
            files
                .and_then(|fs| fs.iter().find(|f| f.path == path))
                .is_some_and(|f| has_svn_prop(&f.props, "svn:needs-lock"))
        };
        for ch in changed {
            let held_by_author = match locks.get(&ch.path) {
                Some(lock) if lock.owner != author => {
                    return Err(VcsError::LockConflict {
                        path: ch.path.clone(),
                        owner: lock.owner.clone(),
                    });
                }
                Some(_) => true,
                None => false,
            };
            let requires = ch.kind != ChangeKind::Added
                && (needs_lock(parent, &ch.path) || needs_lock(Some(new_files), &ch.path));
            if requires && !held_by_author {
                return Err(VcsError::NeedsLockRequired {
                    path: ch.path.clone(),
                });
            }
        }
        Ok(())
    }
}
