use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::types::{FileChange, FileEntry};

/// Bump when the schema below changes so existing working copies re-run the
/// idempotent `CREATE TABLE IF NOT EXISTS` block exactly once.
const SCHEMA_VERSION: i64 = 1;

#[derive(Debug)]
pub struct WcDb {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct ExternalDef {
    pub path: String,
    pub target_url: String,
    pub revision: Option<String>,
}

impl WcDb {
    pub fn open(repo_root: &Path) -> Result<Self> {
        let db_path = repo_root.join(".vcrs").join("wc.db");
        let conn = Connection::open(&db_path)?;
        // Concurrency / durability tuning. WAL lets readers and a writer coexist,
        // and busy_timeout avoids spurious "database is locked" errors when the
        // HTTP server handles overlapping requests against the same wc.db.
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;\n             PRAGMA synchronous = NORMAL;\n             PRAGMA foreign_keys = ON;",
        )?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    fn with_write_tx<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    {
        let tx = self.conn.unchecked_transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    fn init_schema(&self) -> Result<()> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }

        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS meta (
                k TEXT PRIMARY KEY,
                v TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS nodes (
                path TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                base_blob_id TEXT,
                working_blob_id TEXT,
                base_props_json TEXT NOT NULL,
                working_props_json TEXT NOT NULL,
                status TEXT NOT NULL,
                is_binary INTEGER NOT NULL,
                copy_from_path TEXT,
                moved_from_path TEXT,
                moved_to_path TEXT,
                changelist TEXT,
                inherited_props_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS actual_node (
                path TEXT PRIMARY KEY,
                text_mod INTEGER NOT NULL DEFAULT 0,
                prop_mod INTEGER NOT NULL DEFAULT 0,
                conflict_old TEXT,
                conflict_new TEXT,
                conflict_working TEXT,
                tree_conflict TEXT,
                changelist TEXT
            );

            CREATE TABLE IF NOT EXISTS wc_lock (
                path TEXT PRIMARY KEY,
                locked_levels INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS work_queue (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                work_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS ignore_rules (
                scope_path TEXT NOT NULL,
                pattern TEXT NOT NULL,
                inherited INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (scope_path, pattern)
            );

            CREATE TABLE IF NOT EXISTS externals (
                path TEXT PRIMARY KEY,
                target_url TEXT NOT NULL,
                revision TEXT
            );

            CREATE TABLE IF NOT EXISTS file_props (
                path TEXT NOT NULL,
                name TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (path, name)
            );

            CREATE TABLE IF NOT EXISTS inherited_props (
                scope_path TEXT NOT NULL,
                name TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (scope_path, name)
            );

            CREATE TABLE IF NOT EXISTS revisions (
                rev INTEGER PRIMARY KEY,
                commit_id TEXT UNIQUE NOT NULL,
                parent_rev INTEGER,
                author TEXT NOT NULL,
                message TEXT NOT NULL,
                created_at TEXT NOT NULL,
                changed_paths_json TEXT NOT NULL,
                mergeinfo_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS merge_edges (
                target_rev INTEGER NOT NULL,
                merged_rev INTEGER NOT NULL,
                source_path TEXT NOT NULL,
                PRIMARY KEY (target_rev, merged_rev, source_path)
            );

            CREATE TABLE IF NOT EXISTS pending_merges (
                source_path TEXT NOT NULL,
                merged_rev INTEGER NOT NULL,
                PRIMARY KEY (source_path, merged_rev)
            );

            CREATE TABLE IF NOT EXISTS lock_tokens (
                path TEXT PRIMARY KEY,
                token TEXT NOT NULL,
                owner TEXT
            );

            CREATE TABLE IF NOT EXISTS ambient_depth (
                path TEXT PRIMARY KEY,
                depth TEXT NOT NULL,
                sticky INTEGER NOT NULL DEFAULT 1
            );
            "#,
        )?;

        self.set_meta_if_missing("base_revision", "0")?;
        self.set_meta_if_missing("head_revision", "0")?;
        self.set_meta_if_missing("depth", "infinity")?;
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    pub fn base_revision(&self) -> Result<i64> {
        Ok(self
            .meta("base_revision")?
            .unwrap_or_else(|| "0".to_owned())
            .parse()
            .unwrap_or(0))
    }

    pub fn head_revision(&self) -> Result<i64> {
        Ok(self
            .meta("head_revision")?
            .unwrap_or_else(|| "0".to_owned())
            .parse()
            .unwrap_or(0))
    }

    pub fn set_base_revision(&self, rev: i64) -> Result<()> {
        self.set_meta("base_revision", &rev.to_string())
    }

    pub fn set_head_revision(&self, rev: i64) -> Result<()> {
        self.set_meta("head_revision", &rev.to_string())
    }

    pub fn depth(&self) -> Result<String> {
        Ok(self.meta("depth")?.unwrap_or_else(|| "infinity".to_owned()))
    }

    pub fn set_depth(&self, depth: &str) -> Result<()> {
        self.set_meta("depth", depth)
    }

    pub fn set_ambient_depth(&self, path: &str, depth: &str, sticky: bool) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO ambient_depth(path, depth, sticky) VALUES(?1,?2,?3)",
                params![path, depth, sticky as i64],
            )?;
            Ok(())
        })
    }

    pub fn ambient_depth_map(&self) -> Result<BTreeMap<String, (String, bool)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, depth, sticky FROM ambient_depth")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, String>(1)?, r.get::<_, i64>(2)? != 0),
            ))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (k, v) = row?;
            out.insert(k, v);
        }
        Ok(out)
    }

    pub fn replace_nodes(
        &self,
        base: &[FileEntry],
        working: &[FileEntry],
        changes: &[FileChange],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare("SELECT path, changelist, inherited_props_json FROM nodes")?;
        let old_rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut old_meta: BTreeMap<String, (Option<String>, String)> = BTreeMap::new();
        for row in old_rows {
            let (path, changelist, inherited_props_json) = row?;
            old_meta.insert(path, (changelist, inherited_props_json));
        }
        drop(stmt);

        tx.execute("DELETE FROM nodes", [])?;
        tx.execute("DELETE FROM actual_node", [])?;

        let base_map: BTreeMap<&str, &FileEntry> =
            base.iter().map(|f| (f.path.as_str(), f)).collect();
        let working_map: BTreeMap<&str, &FileEntry> =
            working.iter().map(|f| (f.path.as_str(), f)).collect();
        let change_map: BTreeMap<&str, &FileChange> =
            changes.iter().map(|c| (c.path.as_str(), c)).collect();

        let mut keys: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        keys.extend(base_map.keys().copied());
        keys.extend(working_map.keys().copied());

        for path in keys {
            let b = base_map.get(path).copied();
            let w = working_map.get(path).copied();
            let c = change_map.get(path).copied();

            let status = if b.is_none() && w.is_some() {
                "added"
            } else if b.is_some() && w.is_none() {
                "deleted"
            } else if c.is_some() {
                "modified"
            } else {
                "normal"
            };

            let base_props =
                serde_json::to_string(&b.map(|x| &x.props).cloned().unwrap_or_default())?;
            let working_props =
                serde_json::to_string(&w.map(|x| &x.props).cloned().unwrap_or_default())?;
            let (changelist, inherited_props) = old_meta
                .get(path)
                .cloned()
                .unwrap_or((None, "{}".to_owned()));
            let is_binary = w.or(b).is_some_and(|x| x.is_binary) as i64;

            tx.execute(
                "INSERT INTO nodes(path,kind,base_blob_id,working_blob_id,base_props_json,working_props_json,status,is_binary,copy_from_path,moved_from_path,moved_to_path,changelist,inherited_props_json)
                 VALUES(?1,'file',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    path,
                    b.map(|x| x.blob_id.as_str()),
                    w.map(|x| x.blob_id.as_str()),
                    base_props,
                    working_props,
                    status,
                    is_binary,
                    c.and_then(|x| x.copy_from.as_deref()),
                    c.and_then(|x| x.moved_from.as_deref()),
                    c.and_then(|x| x.moved_to.as_deref()),
                    changelist,
                    inherited_props,
                ],
            )?;

            if let Some(change) = c {
                tx.execute(
                    "INSERT INTO actual_node(path,text_mod,prop_mod,conflict_old,conflict_new,conflict_working,tree_conflict,changelist)
                     VALUES(?1,?2,?3,NULL,NULL,NULL,NULL,NULL)",
                    params![path, change.text_modified as i64, change.props_modified as i64],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    pub fn set_tree_conflict(&self, path: &str, reason: &str) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT INTO actual_node(path,text_mod,prop_mod,tree_conflict)
                 VALUES(?1,0,0,?2)
                 ON CONFLICT(path) DO UPDATE SET tree_conflict=excluded.tree_conflict",
                params![path, reason],
            )?;
            Ok(())
        })
    }

    pub fn set_text_conflict_markers(
        &self,
        path: &str,
        old_marker: &str,
        new_marker: &str,
        working_marker: &str,
    ) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT INTO actual_node(path,text_mod,prop_mod,conflict_old,conflict_new,conflict_working)
                 VALUES(?1,1,0,?2,?3,?4)
                 ON CONFLICT(path) DO UPDATE SET
                   text_mod=1,
                   conflict_old=excluded.conflict_old,
                   conflict_new=excluded.conflict_new,
                   conflict_working=excluded.conflict_working",
                params![path, old_marker, new_marker, working_marker],
            )?;
            Ok(())
        })
    }

    pub fn clear_conflicts(&self) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "UPDATE actual_node SET conflict_old=NULL, conflict_new=NULL, conflict_working=NULL, tree_conflict=NULL",
                [],
            )?;
            Ok(())
        })
    }

    pub fn acquire_lock(&self, path: &str, levels: i64) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT INTO wc_lock(path, locked_levels) VALUES(?1, ?2)
                 ON CONFLICT(path) DO UPDATE SET locked_levels=excluded.locked_levels",
                params![path, levels],
            )?;
            Ok(())
        })
    }

    pub fn release_lock(&self, path: &str) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute("DELETE FROM wc_lock WHERE path=?1", params![path])?;
            Ok(())
        })
    }

    pub fn enqueue_work(&self, work_json: &str) -> Result<i64> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT INTO work_queue(work_json) VALUES(?1)",
                params![work_json],
            )?;
            Ok(tx.last_insert_rowid())
        })
    }

    pub fn dequeue_work(&self) -> Result<Option<String>> {
        self.with_write_tx(|tx| {
            let row: Option<(i64, String)> = {
                let mut stmt =
                    tx.prepare("SELECT id, work_json FROM work_queue ORDER BY id LIMIT 1")?;
                stmt.query_row([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?
            };
            if let Some((id, work)) = row {
                tx.execute("DELETE FROM work_queue WHERE id=?1", params![id])?;
                Ok(Some(work))
            } else {
                Ok(None)
            }
        })
    }

    pub fn add_ignore_rule(&self, scope_path: &str, pattern: &str, inherited: bool) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO ignore_rules(scope_path, pattern, inherited) VALUES(?1,?2,?3)",
                params![scope_path, pattern, inherited as i64],
            )?;
            Ok(())
        })
    }

    pub fn list_ignore_rules(&self) -> Result<Vec<(String, String, bool)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT scope_path, pattern, inherited FROM ignore_rules")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? != 0,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn set_external(&self, ext: &ExternalDef) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO externals(path, target_url, revision) VALUES(?1,?2,?3)",
                params![ext.path, ext.target_url, ext.revision],
            )?;
            Ok(())
        })
    }

    pub fn set_file_prop(&self, path: &str, name: &str, value: &str) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO file_props(path, name, value) VALUES(?1,?2,?3)",
                params![path, name, value],
            )?;
            Ok(())
        })
    }

    pub fn replace_file_props_from_entries(&self, entries: &[FileEntry]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM file_props", [])?;
        for e in entries {
            for (name, value) in &e.props {
                tx.execute(
                    "INSERT INTO file_props(path, name, value) VALUES(?1,?2,?3)",
                    params![e.path, name, value],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_file_prop(&self, path: &str, name: &str) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "DELETE FROM file_props WHERE path=?1 AND name=?2",
                params![path, name],
            )?;
            Ok(())
        })
    }

    pub fn file_props(&self, path: &str) -> Result<BTreeMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, value FROM file_props WHERE path=?1")?;
        let rows = stmt.query_map(params![path], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (k, v) = row?;
            out.insert(k, v);
        }
        Ok(out)
    }

    /// Load every per-file property in one query, grouped by path. Used by the
    /// working-copy snapshot to avoid one SQL round-trip per file.
    pub fn all_file_props(&self) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, name, value FROM file_props")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for row in rows {
            let (p, n, v) = row?;
            out.entry(p).or_default().insert(n, v);
        }
        Ok(out)
    }

    pub fn set_inherited_prop(&self, scope_path: &str, name: &str, value: &str) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO inherited_props(scope_path, name, value) VALUES(?1,?2,?3)",
                params![scope_path, name, value],
            )?;
            Ok(())
        })
    }

    pub fn inherited_props_for_path(&self, path: &str) -> Result<BTreeMap<String, String>> {
        let scopes = scope_chain(path);
        let mut out = BTreeMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT name, value FROM inherited_props WHERE scope_path=?1")?;
        for scope in scopes {
            let rows = stmt.query_map(params![scope], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (k, v) = row?;
                out.insert(k, v);
            }
        }
        Ok(out)
    }

    /// Load every inherited property in one query, grouped by scope path. The
    /// snapshot resolves inheritance for each file in-memory from this map.
    pub fn all_inherited_props(&self) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT scope_path, name, value FROM inherited_props")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for row in rows {
            let (s, n, v) = row?;
            out.entry(s).or_default().insert(n, v);
        }
        Ok(out)
    }

    pub fn list_inherited_props(&self) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT scope_path, name, value FROM inherited_props ORDER BY scope_path, name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn set_changelist(&self, path: &str, changelist: Option<&str>) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "UPDATE nodes SET changelist=?2 WHERE path=?1",
                params![path, changelist],
            )?;
            tx.execute(
                "INSERT INTO actual_node(path,text_mod,prop_mod,changelist)
                 VALUES(?1,0,0,?2)
                 ON CONFLICT(path) DO UPDATE SET changelist=excluded.changelist",
                params![path, changelist],
            )?;
            Ok(())
        })
    }

    pub fn list_changelists(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, changelist FROM nodes WHERE changelist IS NOT NULL ORDER BY changelist, path",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn set_lock_token(&self, path: &str, token: &str, owner: Option<&str>) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO lock_tokens(path, token, owner) VALUES(?1,?2,?3)",
                params![path, token, owner],
            )?;
            Ok(())
        })
    }

    pub fn clear_lock_token(&self, path: &str) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute("DELETE FROM lock_tokens WHERE path=?1", params![path])?;
            Ok(())
        })
    }

    pub fn has_lock_token(&self, path: &str) -> Result<bool> {
        let mut stmt = self
            .conn
            .prepare("SELECT token, owner FROM lock_tokens WHERE path=?1 LIMIT 1")?;
        let found: Option<(String, Option<String>)> = stmt
            .query_row(params![path], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        Ok(found.is_some_and(|(token, owner)| {
            !token.trim().is_empty() && owner.is_some_and(|o| !o.trim().is_empty())
        }))
    }

    pub fn clear_all_lock_tokens(&self) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute("DELETE FROM lock_tokens", [])?;
            Ok(())
        })
    }

    pub fn list_externals(&self) -> Result<Vec<ExternalDef>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, target_url, revision FROM externals ORDER BY path")?;
        let rows = stmt.query_map([], |r| {
            Ok(ExternalDef {
                path: r.get(0)?,
                target_url: r.get(1)?,
                revision: r.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn add_revision(
        &self,
        rev: i64,
        commit_id: &str,
        parent_rev: Option<i64>,
        author: &str,
        message: &str,
        created_at: &str,
        changed_paths_json: &str,
        mergeinfo_json: &str,
    ) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT INTO revisions(rev, commit_id, parent_rev, author, message, created_at, changed_paths_json, mergeinfo_json)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    rev,
                    commit_id,
                    parent_rev,
                    author,
                    message,
                    created_at,
                    changed_paths_json,
                    mergeinfo_json
                ],
            )?;
            tx.execute(
                "INSERT INTO meta(k, v) VALUES(?1, ?2)
                 ON CONFLICT(k) DO UPDATE SET v=excluded.v",
                params!["head_revision", rev.to_string()],
            )?;
            Ok(())
        })
    }

    pub fn upsert_revision(
        &self,
        rev: i64,
        commit_id: &str,
        parent_rev: Option<i64>,
        author: &str,
        message: &str,
        created_at: &str,
        changed_paths_json: &str,
        mergeinfo_json: &str,
    ) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT INTO revisions(rev, commit_id, parent_rev, author, message, created_at, changed_paths_json, mergeinfo_json)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(rev) DO UPDATE SET
                   commit_id=excluded.commit_id,
                   parent_rev=excluded.parent_rev,
                   author=excluded.author,
                   message=excluded.message,
                   created_at=excluded.created_at,
                   changed_paths_json=excluded.changed_paths_json,
                   mergeinfo_json=excluded.mergeinfo_json",
                params![
                    rev,
                    commit_id,
                    parent_rev,
                    author,
                    message,
                    created_at,
                    changed_paths_json,
                    mergeinfo_json
                ],
            )?;
            Ok(())
        })
    }

    pub fn delete_revision(&self, rev: i64) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute("DELETE FROM revisions WHERE rev=?1", params![rev])?;
            Ok(())
        })
    }

    pub fn add_merge_edge(
        &self,
        target_rev: i64,
        merged_rev: i64,
        source_path: &str,
    ) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO merge_edges(target_rev, merged_rev, source_path) VALUES(?1,?2,?3)",
                params![target_rev, merged_rev, source_path],
            )?;
            Ok(())
        })
    }

    pub fn delete_merge_edge(
        &self,
        target_rev: i64,
        merged_rev: i64,
        source_path: &str,
    ) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "DELETE FROM merge_edges WHERE target_rev=?1 AND merged_rev=?2 AND source_path=?3",
                params![target_rev, merged_rev, source_path],
            )?;
            Ok(())
        })
    }

    pub fn merged_revisions_set(&self) -> Result<std::collections::BTreeSet<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT merged_rev FROM merge_edges")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        let mut out = std::collections::BTreeSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    pub fn merge_edges_up_to(&self, max_target_rev: i64) -> Result<Vec<(i64, i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT target_rev, merged_rev, source_path
             FROM merge_edges
             WHERE target_rev<=?1
             ORDER BY target_rev, merged_rev, source_path",
        )?;
        let rows = stmt.query_map(params![max_target_rev], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn add_pending_merge(&self, source_path: &str, merged_rev: i64) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO pending_merges(source_path, merged_rev) VALUES(?1,?2)",
                params![source_path, merged_rev],
            )?;
            Ok(())
        })
    }

    pub fn pending_merges(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_path, merged_rev FROM pending_merges ORDER BY source_path, merged_rev",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn take_pending_merges(&self) -> Result<Vec<(String, i64)>> {
        self.with_write_tx(|tx| {
            let out = {
                let mut stmt = tx.prepare(
                    "SELECT source_path, merged_rev FROM pending_merges ORDER BY source_path, merged_rev",
                )?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                out
            };
            tx.execute("DELETE FROM pending_merges", [])?;
            Ok(out)
        })
    }

    pub fn revision_for_commit(&self, commit_id: &str) -> Result<Option<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT rev FROM revisions WHERE commit_id=?1")?;
        let rev = stmt
            .query_row(params![commit_id], |r| r.get(0))
            .optional()?;
        Ok(rev)
    }

    pub fn commit_for_revision(&self, rev: i64) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT commit_id FROM revisions WHERE rev=?1")?;
        let id = stmt.query_row(params![rev], |r| r.get(0)).optional()?;
        Ok(id)
    }

    pub fn max_revision(&self) -> Result<i64> {
        let mut stmt = self
            .conn
            .prepare("SELECT COALESCE(MAX(rev), 0) FROM revisions")?;
        let max: i64 = stmt.query_row([], |r| r.get(0))?;
        Ok(max)
    }

    fn meta(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT v FROM meta WHERE k=?1")?;
        let value = stmt.query_row(params![key], |r| r.get(0)).optional()?;
        Ok(value)
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT INTO meta(k, v) VALUES(?1, ?2)
                 ON CONFLICT(k) DO UPDATE SET v=excluded.v",
                params![key, value],
            )?;
            Ok(())
        })
    }

    fn set_meta_if_missing(&self, key: &str, value: &str) -> Result<()> {
        self.with_write_tx(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO meta(k, v) VALUES(?1, ?2)",
                params![key, value],
            )?;
            Ok(())
        })
    }
}

/// All scope prefixes that apply to `path`, from the repository root ("") down
/// to the file's immediate parent, in inheritance order (shallow first).
fn scope_chain(path: &str) -> Vec<String> {
    let mut scopes = vec![String::new()];
    let mut current = String::new();
    for seg in path.split('/') {
        if current.is_empty() {
            current.push_str(seg);
        } else {
            current.push('/');
            current.push_str(seg);
        }
        scopes.push(current.clone());
    }
    scopes
}
