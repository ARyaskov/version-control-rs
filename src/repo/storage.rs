//! Object store and commit files.
//!
//! Objects (file contents and directory trees) are content-addressed by the
//! blake3 hash of their uncompressed bytes and stored zstd-compressed as
//! `.vcrs/objects/<2 hex>/<62 hex>.z`. Objects written before 0.3 are stored
//! raw without the suffix and stay readable.
//!
//! A commit references its root tree; each tree lists one directory level,
//! so unchanged subtrees are shared between revisions and a commit costs
//! O(changed paths) instead of a full manifest of the repository.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::error::{Result, VcsError};
use crate::path::validate_rel_path;
use crate::types::{Commit, FileEntry};

use super::{Repository, VCRS_DIR};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// zstd level: fast, still a large win for source text.
const ZSTD_LEVEL: i32 = 3;
/// Header of a serialized tree object (part of the hashed bytes, so a tree id
/// can never be confused with the id of a differently-typed object).
const TREE_HEADER: &[u8] = b"vcrs-tree-v1\0";
/// Parsed trees kept in memory (they are immutable and content-addressed).
const TREE_CACHE_LIMIT: usize = 4096;
/// Parsed, verified commit records kept in memory.
const COMMIT_CACHE_LIMIT: usize = 4096;

/// Write `content` to `path` atomically and durably: write a uniquely-named
/// temp file in the same directory, fsync it, rename it over the destination
/// (an atomic replace on the same filesystem) and fsync the directory so the
/// rename itself survives a crash. Prevents torn or lost HEAD/commit/blob data.
fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| VcsError::PathOutsideRepository(path.display().to_string()))?;
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".vcrs-tmp-{}-{}.tmp", std::process::id(), n));
    let result = (|| -> std::io::Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        sync_dir(parent)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(VcsError::Io(e));
    }
    Ok(())
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    // Windows offers no portable directory fsync; NTFS journals the rename.
    Ok(())
}

fn vcrs(repo: &Repository) -> PathBuf {
    repo.root.join(VCRS_DIR)
}

pub(crate) fn objects_dir(repo: &Repository) -> PathBuf {
    vcrs(repo).join("objects")
}

pub(crate) fn commits_dir(repo: &Repository) -> PathBuf {
    vcrs(repo).join("commits")
}

pub fn ensure_layout(repo: &Repository) -> Result<()> {
    fs::create_dir_all(objects_dir(repo))?;
    fs::create_dir_all(commits_dir(repo))?;
    Ok(())
}

/// `(compressed, legacy raw)` locations of an object (`id` must be valid).
fn object_paths(repo: &Repository, id: &str) -> (PathBuf, PathBuf) {
    let (prefix, rest) = id.split_at(2);
    let dir = objects_dir(repo).join(prefix);
    (dir.join(format!("{rest}.z")), dir.join(rest))
}

/// Object id encoded in a file name inside `objects/<prefix>/`, if it is one.
pub(crate) fn object_id_from_file(prefix: &str, file_name: &str) -> Option<String> {
    if file_name.ends_with(".tmp") {
        return None;
    }
    let rest = file_name.strip_suffix(".z").unwrap_or(file_name);
    Some(format!("{prefix}{rest}"))
}

/// Store `bytes` as an object and return its id.
pub fn write_object(repo: &Repository, bytes: &[u8]) -> Result<String> {
    ensure_layout(repo)?;
    let id = hash_blob(bytes);
    let (compressed, legacy) = object_paths(repo, &id);
    if compressed.exists() || legacy.exists() {
        return Ok(id);
    }
    if let Some(dir) = compressed.parent() {
        fs::create_dir_all(dir)?;
    }
    atomic_write(&compressed, &zstd::bulk::compress(bytes, ZSTD_LEVEL)?)?;
    Ok(id)
}

pub fn object_exists(repo: &Repository, id: &str) -> bool {
    if validate_object_id(id).is_err() {
        return false;
    }
    let (compressed, legacy) = object_paths(repo, id);
    compressed.exists() || legacy.exists()
}

/// Read an object and verify that its content still hashes to its id.
pub fn read_object(repo: &Repository, id: &str) -> Result<Vec<u8>> {
    validate_object_id(id)?;
    let (compressed, legacy) = object_paths(repo, id);
    let bytes = if compressed.exists() {
        zstd::stream::decode_all(fs::File::open(compressed)?)
            .map_err(|_| VcsError::CorruptObject(id.to_owned()))?
    } else if legacy.exists() {
        fs::read(legacy)?
    } else {
        return Err(VcsError::BlobNotFound(id.to_owned()));
    };
    if hash_blob(&bytes) != id {
        return Err(VcsError::CorruptObject(id.to_owned()));
    }
    Ok(bytes)
}

pub fn write_blob(repo: &Repository, content: &[u8]) -> Result<String> {
    write_object(repo, content)
}

/// Content-addressed blob id without touching the object store. Used by the
/// working-copy snapshot so read-only operations (status/diff/merge planning)
/// do not litter `.vcrs/objects` with blobs that no commit will ever reference.
pub fn hash_blob(content: &[u8]) -> String {
    blake3::hash(content).to_hex().to_string()
}

pub fn read_blob(repo: &Repository, blob_id: &str) -> Result<Vec<u8>> {
    read_object(repo, blob_id)
}

/// One directory level: files carry their metadata (with an empty `path`,
/// the position in the tree names them), subdirectories their tree id.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TreeEntry {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TreeObject {
    entries: Vec<TreeEntry>,
}

enum Node {
    File(FileEntry),
    Dir(BTreeMap<String, Node>),
}

/// Store the directory hierarchy of `files` as tree objects and return the
/// root tree id. Existing (unchanged) subtrees are not rewritten.
pub fn write_tree(repo: &Repository, files: &[FileEntry]) -> Result<String> {
    let clash = |path: &str| VcsError::Protocol(format!("'{path}' is both a file and a directory"));
    let mut root: BTreeMap<String, Node> = BTreeMap::new();
    for f in files {
        validate_rel_path(&f.path)?;
        let mut parts: Vec<&str> = f.path.split('/').collect();
        let name = parts.pop().unwrap_or_default();
        let mut dir = &mut root;
        for part in parts {
            dir = match dir
                .entry(part.to_owned())
                .or_insert_with(|| Node::Dir(BTreeMap::new()))
            {
                Node::Dir(d) => d,
                Node::File(_) => return Err(clash(&f.path)),
            };
        }
        let mut entry = f.clone();
        entry.path = String::new();
        if dir.insert(name.to_owned(), Node::File(entry)).is_some() {
            return Err(clash(&f.path));
        }
    }
    write_dir(repo, &root)
}

fn write_dir(repo: &Repository, dir: &BTreeMap<String, Node>) -> Result<String> {
    let mut entries = Vec::with_capacity(dir.len());
    for (name, node) in dir {
        entries.push(match node {
            Node::File(f) => TreeEntry {
                name: name.clone(),
                tree: None,
                file: Some(f.clone()),
            },
            Node::Dir(d) => TreeEntry {
                name: name.clone(),
                tree: Some(write_dir(repo, d)?),
                file: None,
            },
        });
    }
    let mut bytes = TREE_HEADER.to_vec();
    serde_json::to_writer(&mut bytes, &TreeObject { entries })?;
    write_object(repo, &bytes)
}

fn tree_cache() -> &'static Mutex<HashMap<String, Arc<TreeObject>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<TreeObject>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn read_tree_object(repo: &Repository, id: &str) -> Result<Arc<TreeObject>> {
    if let Some(tree) = tree_cache().lock().ok().and_then(|c| c.get(id).cloned()) {
        return Ok(tree);
    }
    let bytes = read_object(repo, id)?;
    let json = bytes
        .strip_prefix(TREE_HEADER)
        .ok_or_else(|| VcsError::Protocol(format!("object {id} is not a tree")))?;
    let tree: TreeObject = serde_json::from_slice(json)?;
    for entry in &tree.entries {
        // A name is exactly one valid path component.
        if entry.name.contains('/') || entry.tree.is_some() == entry.file.is_some() {
            return Err(VcsError::Protocol(format!("malformed entry in tree {id}")));
        }
        validate_rel_path(&entry.name)?;
    }
    let tree = Arc::new(tree);
    if let Ok(mut cache) = tree_cache().lock() {
        if cache.len() >= TREE_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(id.to_owned(), tree.clone());
    }
    Ok(tree)
}

/// All files below tree `id`, with full paths, sorted by path.
pub fn read_tree_files(repo: &Repository, id: &str) -> Result<Vec<FileEntry>> {
    fn walk(repo: &Repository, id: &str, prefix: &str, out: &mut Vec<FileEntry>) -> Result<()> {
        for entry in &read_tree_object(repo, id)?.entries {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            match (&entry.tree, &entry.file) {
                (Some(sub), _) => walk(repo, sub, &path, out)?,
                (None, Some(file)) => {
                    let mut file = file.clone();
                    file.path = path;
                    out.push(file);
                }
                (None, None) => {}
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(repo, id, "", &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The entry for `path` in tree `id`, reading only the trees on its way.
pub fn tree_lookup(repo: &Repository, id: &str, path: &str) -> Result<Option<FileEntry>> {
    let mut tree_id = id.to_owned();
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        let tree = read_tree_object(repo, &tree_id)?;
        let Some(entry) = tree.entries.iter().find(|e| e.name == part) else {
            return Ok(None);
        };
        match (&entry.tree, &entry.file, parts.peek().is_some()) {
            (Some(sub), _, true) => tree_id = sub.clone(),
            (None, Some(file), false) => {
                let mut file = file.clone();
                file.path = path.to_owned();
                return Ok(Some(file));
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// Add tree `id` and every object below it to `live` (subtrees already in
/// `live` are not walked again).
pub fn collect_tree_objects(
    repo: &Repository,
    id: &str,
    live: &mut BTreeSet<String>,
) -> Result<()> {
    if !live.insert(id.to_owned()) {
        return Ok(());
    }
    for entry in &read_tree_object(repo, id)?.entries {
        match (&entry.tree, &entry.file) {
            (Some(sub), _) => collect_tree_objects(repo, sub, live)?,
            (None, Some(file)) => {
                live.insert(file.blob_id.clone());
            }
            (None, None) => {}
        }
    }
    Ok(())
}

/// Persist a commit: its file list goes into tree objects and the commit
/// file records only the root tree id.
pub fn write_commit(repo: &Repository, commit: &Commit) -> Result<()> {
    ensure_layout(repo)?;
    let mut record = commit.clone();
    if record.tree.is_none() {
        record.tree = Some(write_tree(repo, &commit.files)?);
    }
    record.files.clear();
    let path = commit_path(repo, &commit.id)?;
    atomic_write(&path, &serde_json::to_vec_pretty(&record)?)?;
    Ok(())
}

/// Remove a commit object that was never published (e.g. rejected by the
/// pre-commit hook).
pub fn delete_commit(repo: &Repository, id: &str) -> Result<()> {
    let path = commit_path(repo, id)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Read a commit with its complete file list.
pub fn read_commit(repo: &Repository, id: &str) -> Result<Commit> {
    let mut commit = read_commit_header(repo, id)?;
    if let Some(tree) = &commit.tree {
        commit.files = read_tree_files(repo, tree)?;
        validate_commit_paths(&commit)?;
    }
    Ok(commit)
}

/// Read a commit without expanding its tree (`files` stays empty for
/// tree-based commits): cheap enough for walking history.
pub fn read_commit_header(repo: &Repository, id: &str) -> Result<Commit> {
    let path = commit_path(repo, id)?;
    let Ok(md) = fs::metadata(&path) else {
        return Err(VcsError::CommitNotFound(id.to_owned()));
    };
    // Commit files are immutable: a parsed, verified copy is reused as long as
    // the file's size and mtime are unchanged (any rewrite is re-verified).
    let stamp = (md.len(), md.modified().ok());
    if let Some((cached_stamp, commit)) = commit_cache()
        .lock()
        .ok()
        .and_then(|c| c.get(&path).cloned())
        && cached_stamp == stamp
    {
        return Ok((*commit).clone());
    }
    let commit = verify_commit_bytes(&fs::read(&path)?, id)?;
    if let Ok(mut cache) = commit_cache().lock() {
        if cache.len() >= COMMIT_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(path, (stamp, Arc::new(commit.clone())));
    }
    Ok(commit)
}

/// Parse a commit file's bytes and check that they hold commit `id`: the
/// recorded id must match and, for verifiable formats, still be the hash of
/// the record.
fn verify_commit_bytes(bytes: &[u8], id: &str) -> Result<Commit> {
    let commit = parse_commit(bytes)?;
    if commit.id != id
        || (commit.format >= COMMIT_FORMAT && compute_commit_id(&commit)? != commit.id)
    {
        return Err(VcsError::CorruptObject(id.to_owned()));
    }
    Ok(commit)
}

type CommitCache = HashMap<PathBuf, ((u64, Option<std::time::SystemTime>), Arc<Commit>)>;

fn commit_cache() -> &'static Mutex<CommitCache> {
    static CACHE: OnceLock<Mutex<CommitCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn commit_exists(repo: &Repository, id: &str) -> bool {
    commit_path(repo, id).is_ok_and(|p| p.exists())
}

/// Copy commit `id` of `from` and the objects it needs into `to`. Objects are
/// verified as they are read. A tree already present in `to` is complete
/// (children are always stored before their parent), so it is not walked;
/// the commit file is written last.
pub fn transfer_commit(from: &Repository, to: &Repository, id: &str) -> Result<()> {
    // Verify exactly the bytes that are copied.
    let bytes =
        fs::read(commit_path(from, id)?).map_err(|_| VcsError::CommitNotFound(id.to_owned()))?;
    let commit = verify_commit_bytes(&bytes, id)?;
    match &commit.tree {
        Some(tree) => transfer_tree(from, to, tree)?,
        None => {
            for f in &commit.files {
                copy_object(from, to, &f.blob_id)?;
            }
        }
    }
    ensure_layout(to)?;
    atomic_write(&commit_path(to, id)?, &bytes)
}

fn transfer_tree(from: &Repository, to: &Repository, id: &str) -> Result<()> {
    if object_exists(to, id) {
        return Ok(());
    }
    for entry in &read_tree_object(from, id)?.entries {
        match (&entry.tree, &entry.file) {
            (Some(sub), _) => transfer_tree(from, to, sub)?,
            (None, Some(file)) => copy_object(from, to, &file.blob_id)?,
            (None, None) => {}
        }
    }
    copy_object(from, to, id)
}

fn copy_object(from: &Repository, to: &Repository, id: &str) -> Result<()> {
    if object_exists(to, id) {
        return Ok(());
    }
    write_object(to, &read_object(from, id)?)?;
    Ok(())
}

/// Deserialize a commit file and reject any path that could escape the
/// working copy once materialized. Commits may come from an untrusted remote,
/// so every commit read goes through this gate (tree entries are validated
/// when trees are read).
pub fn parse_commit(bytes: &[u8]) -> Result<Commit> {
    let commit: Commit = serde_json::from_slice(bytes)?;
    validate_object_id(&commit.id)?;
    for id in [&commit.parent, &commit.tree].into_iter().flatten() {
        validate_object_id(id)?;
    }
    for f in &commit.files {
        validate_object_id(&f.blob_id)?;
    }
    validate_commit_paths(&commit)?;
    Ok(commit)
}

fn validate_commit_paths(commit: &Commit) -> Result<()> {
    for f in &commit.files {
        validate_rel_path(&f.path)?;
        if let Some(src) = &f.copy_from_path {
            validate_rel_path(src)?;
        }
    }
    for ch in &commit.changed_files {
        validate_rel_path(&ch.path)?;
        for p in [&ch.copy_from, &ch.moved_from, &ch.moved_to]
            .into_iter()
            .flatten()
        {
            validate_rel_path(p)?;
        }
    }
    for cp in &commit.changed_paths {
        validate_rel_path(&cp.path)?;
        if let Some(src) = &cp.copyfrom_path {
            validate_rel_path(src)?;
        }
    }
    Ok(())
}

/// Current commit record format (see [`Commit::format`]).
pub const COMMIT_FORMAT: u32 = 2;

/// Id of a commit: blake3 over a domain tag and the canonical JSON of the
/// record without its id and expanded file list — i.e. over the root tree
/// and every metadata field. Tampering with a stored commit changes it.
pub fn compute_commit_id(commit: &Commit) -> Result<String> {
    let mut record = commit.clone();
    record.id.clear();
    record.files.clear();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"vcrs-commit-v2\0");
    hasher.update(&serde_json::to_vec(&record)?);
    Ok(hasher.finalize().to_hex().to_string())
}

/// Object and commit ids are 64 lowercase hex digits. Checked before an id
/// is turned into a path (ids come from commit files, HEAD and remotes).
pub fn validate_object_id(id: &str) -> Result<()> {
    if id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(VcsError::InvalidObjectId(id.to_owned()))
    }
}

fn commit_path(repo: &Repository, id: &str) -> Result<PathBuf> {
    validate_object_id(id)?;
    Ok(commits_dir(repo).join(format!("{id}.json")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, blob: &str) -> FileEntry {
        FileEntry {
            path: path.to_owned(),
            blob_id: blob.to_owned(),
            executable: false,
            is_binary: false,
            props: BTreeMap::new(),
            copy_from_path: None,
            copy_from_rev: None,
            node_id: None,
            copy_id: None,
            created_rev: None,
        }
    }

    #[test]
    fn trees_roundtrip_and_share_unchanged_subtrees() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let blob = write_blob(&repo, b"x").unwrap();
        let v1 = vec![
            entry("a.txt", &blob),
            entry("src/lib.rs", &blob),
            entry("src/util/x.rs", &blob),
        ];
        let t1 = write_tree(&repo, &v1).unwrap();
        assert_eq!(read_tree_files(&repo, &t1).unwrap().len(), 3);
        assert_eq!(
            tree_lookup(&repo, &t1, "src/util/x.rs")
                .unwrap()
                .unwrap()
                .blob_id,
            blob
        );
        assert!(tree_lookup(&repo, &t1, "src/missing.rs").unwrap().is_none());

        // Changing a top-level file keeps the `src` subtree object.
        let mut v2 = v1.clone();
        v2[0].blob_id = write_blob(&repo, b"y").unwrap();
        let t2 = write_tree(&repo, &v2).unwrap();
        let (mut live1, mut live2) = (BTreeSet::new(), BTreeSet::new());
        collect_tree_objects(&repo, &t1, &mut live1).unwrap();
        collect_tree_objects(&repo, &t2, &mut live2).unwrap();
        assert!(
            live1.intersection(&live2).count() >= 2,
            "src subtree shared"
        );
    }

    #[test]
    fn malicious_tree_entries_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let blob = write_blob(&repo, b"x").unwrap();
        for name in ["..", ".vcrs", "a/b", ""] {
            let mut bytes = TREE_HEADER.to_vec();
            let tree = TreeObject {
                entries: vec![TreeEntry {
                    name: name.to_owned(),
                    tree: None,
                    file: Some(entry("", &blob)),
                }],
            };
            serde_json::to_writer(&mut bytes, &tree).unwrap();
            let id = write_object(&repo, &bytes).unwrap();
            assert!(read_tree_files(&repo, &id).is_err(), "{name:?}");
        }
    }

    #[test]
    fn objects_are_compressed_and_legacy_raw_objects_readable() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let content = "line\n".repeat(1000);
        let id = write_blob(&repo, content.as_bytes()).unwrap();
        let (compressed, legacy) = object_paths(&repo, &id);
        assert!(compressed.exists() && !legacy.exists());
        assert!(fs::metadata(&compressed).unwrap().len() < 200);
        assert_eq!(read_blob(&repo, &id).unwrap(), content.as_bytes());

        // A pre-0.3 object: raw bytes, no suffix.
        let old = b"legacy object";
        let old_id = hash_blob(old);
        let (_, raw) = object_paths(&repo, &old_id);
        fs::create_dir_all(raw.parent().unwrap()).unwrap();
        fs::write(&raw, old).unwrap();
        assert_eq!(read_blob(&repo, &old_id).unwrap(), old);
    }
}
