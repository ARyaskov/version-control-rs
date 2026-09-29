//! End-to-end tests against the public library API in throwaway repositories.

use std::fs;
use std::path::Path;

use version_control_rs::repo::Repository;
use version_control_rs::{ChangeKind, Client};

fn write(root: &Path, rel: &str, content: &str) {
    let abs = root.join(rel);
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(abs, content).unwrap();
}

fn add(client: &Client, paths: &[&str]) {
    let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
    client.add(&paths).unwrap();
}

fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
}

#[test]
fn commit_status_cat_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();

    write(root, "a.txt", "alpha\nbeta\n");
    add(&client, &["a.txt"]);
    let c1 = client.commit("r1", "alice").unwrap();
    assert_eq!(c1.revision, 1);
    assert!(client.status().unwrap().is_empty(), "clean after commit");

    write(root, "a.txt", "alpha\nBETA\n");
    let st = client.status().unwrap();
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].kind, ChangeKind::Modified);

    let c2 = client.commit("r2", "bob").unwrap();
    assert_eq!(c2.revision, 2);
    let bytes = client.cat_revision_file("2", "a.txt").unwrap();
    assert_eq!(bytes, b"alpha\nBETA\n");
}

#[test]
fn update_and_revert() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "f.txt", "v1\n");
    add(&client, &["f.txt"]);
    client.commit("r1", "a").unwrap();
    write(root, "f.txt", "v2\n");
    client.commit("r2", "a").unwrap();

    client.update_to_revision("1").unwrap();
    assert_eq!(read(root, "f.txt"), "v1\n");
    client.update_to_revision("HEAD").unwrap();
    assert_eq!(read(root, "f.txt"), "v2\n");

    write(root, "f.txt", "dirty\n");
    client.revert(&[]).unwrap();
    assert_eq!(read(root, "f.txt"), "v2\n");
}

#[test]
fn staged_commit_preserves_unstaged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "a.txt", "a\n");
    write(root, "b.txt", "b\n");
    add(&client, &["a.txt", "b.txt"]);
    client.commit("r1", "a").unwrap();

    write(root, "a.txt", "A\n");
    write(root, "b.txt", "B\n");
    client.stage_paths(&["a.txt".to_owned()]).unwrap();
    client.commit_staged("only a", "a", false).unwrap();

    // b.txt keeps its unstaged change on disk and is NOT in the commit.
    assert_eq!(read(root, "b.txt"), "B\n");
    assert_eq!(client.cat_revision_file("2", "a.txt").unwrap(), b"A\n");
    assert_eq!(client.cat_revision_file("2", "b.txt").unwrap(), b"b\n");
}

#[test]
fn commit_rejects_conflict_markers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "c.txt", "ok\n");
    add(&client, &["c.txt"]);
    client.commit("r1", "a").unwrap();
    write(
        root,
        "c.txt",
        "<<<<<<< .mine\nmine\n=======\ntheirs\n>>>>>>> .theirs\n",
    );
    let err = client.commit("bad", "a").unwrap_err();
    assert!(
        err.to_string().contains("conflict"),
        "expected conflict error, got: {err}"
    );
}

#[test]
fn rename_detected_but_not_for_empty_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "unique.txt", "a distinctive line of content\n");
    write(root, "empty1.txt", "");
    add(&client, &["unique.txt", "empty1.txt"]);
    client.commit("r1", "a").unwrap();

    // Rename the unique file and the empty file.
    fs::remove_file(root.join("unique.txt")).unwrap();
    write(root, "renamed.txt", "a distinctive line of content\n");
    fs::remove_file(root.join("empty1.txt")).unwrap();
    write(root, "empty2.txt", "");
    add(&client, &["renamed.txt", "empty2.txt"]);

    let st = client.status().unwrap();
    let renamed = st.iter().find(|c| c.path == "renamed.txt").unwrap();
    assert_eq!(
        renamed.moved_from.as_deref(),
        Some("unique.txt"),
        "unique content should be detected as a rename"
    );
    let empty_add = st.iter().find(|c| c.path == "empty2.txt").unwrap();
    assert!(
        empty_add.moved_from.is_none() && empty_add.copy_from.is_none(),
        "empty files must not be paired as a move/copy"
    );
}

#[test]
fn eol_style_not_auto_injected() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "t.txt", "line1\nline2\n");
    add(&client, &["t.txt"]);
    client.commit("r1", "a").unwrap();

    let repo = Repository::discover(root).unwrap();
    let head = repo.head_commit().unwrap().unwrap();
    let entry = head.files.iter().find(|f| f.path == "t.txt").unwrap();
    assert!(
        !entry.props.contains_key("svn:eol-style"),
        "eol-style must not be auto-injected, got props: {:?}",
        entry.props
    );
    assert!(!entry.props.contains_key("svn:mime-type"));
}

#[test]
fn unified_diff_has_hunk_headers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "f.txt", "1\n2\n3\n4\n5\n6\n7\n8\n");
    add(&client, &["f.txt"]);
    client.commit("r1", "a").unwrap();
    write(root, "f.txt", "1\n2\n3\nFOUR\n5\n6\n7\n8\n");

    let patch = client.diff(None, None).unwrap();
    assert!(
        patch.contains("@@"),
        "expected unified hunk header in: {patch}"
    );
    assert!(patch.contains("-4"));
    assert!(patch.contains("+FOUR"));
}

#[test]
fn gc_removes_unreferenced_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "keep.txt", "referenced content\n");
    add(&client, &["keep.txt"]);
    client.commit("r1", "a").unwrap();

    // Inject an orphan blob.
    let repo = Repository::discover(root).unwrap();
    repo.write_blob(b"totally unreferenced orphan blob")
        .unwrap();

    let stats = client.gc().unwrap();
    assert!(stats.removed >= 1, "expected at least one orphan removed");
    assert!(stats.kept >= 1, "referenced blob must be kept");
    // The referenced file is still readable after gc.
    assert_eq!(
        client.cat_revision_file("1", "keep.txt").unwrap(),
        b"referenced content\n"
    );
}

#[test]
fn symbolic_revisions_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "f.txt", "1\n");
    add(&client, &["f.txt"]);
    client.commit("r1", "a").unwrap();
    write(root, "f.txt", "2\n");
    client.commit("r2", "a").unwrap();

    let repo = Repository::discover(root).unwrap();
    assert_eq!(repo.resolve_revision_spec("HEAD").unwrap(), 2);
    assert_eq!(repo.resolve_revision_spec("BASE").unwrap(), 2);
    assert_eq!(repo.resolve_revision_spec("COMMITTED").unwrap(), 2);
    assert_eq!(repo.resolve_revision_spec("PREV").unwrap(), 1);
}

#[test]
fn checkout_rejects_paths_escaping_the_working_copy() {
    let dir = tempfile::tempdir().unwrap();
    let remote = dir.path().join("remote");
    let client = Client::init(&remote).unwrap();
    write(&remote, "payload.txt", "x\n");
    add(&client, &["payload.txt"]);
    let c = client.commit("r1", "a").unwrap();

    // Tamper with the stored commit the way a malicious remote could.
    let commit_file = remote.join(".vcrs/commits").join(format!("{}.json", c.id));
    let json = fs::read_to_string(&commit_file)
        .unwrap()
        .replace("\"payload.txt\"", "\"../escaped.txt\"");
    fs::write(&commit_file, json).unwrap();

    let dest = dir.path().join("wc");
    let res = Client::checkout_remote(&format!("file://{}", remote.display()), &dest, None);
    assert!(res.is_err(), "crafted history must be rejected");
    assert!(!dir.path().join("escaped.txt").exists());
}

#[cfg(unix)]
#[test]
fn update_does_not_follow_dangling_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wc");
    let outside = dir.path().join("outside.txt");
    let client = Client::init(&root).unwrap();
    write(&root, "f.txt", "v1\n");
    add(&client, &["f.txt"]);
    client.commit("r1", "a").unwrap();
    write(&root, "f.txt", "v2\n");
    client.commit("r2", "a").unwrap();
    client.update_to_revision("1").unwrap();

    fs::remove_file(root.join("f.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("f.txt")).unwrap();
    let _ = client.update_to_revision("HEAD");
    assert!(!outside.exists(), "write must not follow the symlink");
}

#[cfg(unix)]
#[test]
fn hooks_can_be_disabled() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    let hook = root.join(".vcrs/hooks/pre-commit");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(&hook, "#!/bin/sh\necho rejected >&2\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    write(root, "a.txt", "a\n");
    add(&client, &["a.txt"]);
    assert!(client.commit("r1", "a").is_err(), "enabled hook rejects");
    let client = client.with_hooks(false);
    assert_eq!(client.commit("r1", "a").unwrap().revision, 1);
}

#[test]
fn concurrent_commits_are_serialized() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    Client::init(&root).unwrap();
    let workers: Vec<_> = (0..4)
        .map(|t| {
            let root = root.clone();
            std::thread::spawn(move || {
                let client = Client::discover(&root).unwrap();
                for i in 0..5 {
                    let path = format!("t{t}/f{i}.txt");
                    write(&root, &path, &format!("{t}-{i}\n"));
                    client.add(&[path]).unwrap();
                    client.commit(&format!("t{t} c{i}"), "a").unwrap();
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }

    let client = Client::discover(&root).unwrap();
    let log = client.log(1000).unwrap();
    let head = log.first().unwrap().revision;
    assert_eq!(log.len() as i64, head, "history must be a gap-free chain");
    for (i, c) in log.iter().rev().enumerate() {
        assert_eq!(c.revision, i as i64 + 1);
    }
    assert!(client.status().unwrap().is_empty());
}

#[test]
fn legacy_head_file_and_journal_are_migrated() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "a.txt", "1\n");
    add(&client, &["a.txt"]);
    client.commit("r1", "a").unwrap();
    write(root, "a.txt", "2\n");
    let c2 = client.commit("r2", "a").unwrap();
    drop(client);

    // Recreate the pre-0.3 layout: HEAD file, an empty revision index and a
    // leftover journal of an interrupted transaction.
    fs::write(root.join(".vcrs/HEAD"), &c2.id).unwrap();
    let db = rusqlite::Connection::open(root.join(".vcrs/wc.db")).unwrap();
    db.execute("DELETE FROM revisions", []).unwrap();
    drop(db);
    fs::create_dir_all(root.join(".vcrs/transactions")).unwrap();
    fs::write(root.join(".vcrs/transactions/x.journal.json"), "[]").unwrap();

    let client = Client::discover(root).unwrap();
    assert_eq!(client.log(10).unwrap().len(), 2);
    assert!(!root.join(".vcrs/HEAD").exists());
    assert!(!root.join(".vcrs/transactions").exists());
    assert_eq!(client.cat_revision_file("2", "a.txt").unwrap(), b"2\n");
}

#[test]
fn unpublished_commit_object_does_not_move_head() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "a.txt", "1\n");
    add(&client, &["a.txt"]);
    let c1 = client.commit("r1", "a").unwrap();

    // Simulate a crash after the commit object was written but before the
    // SQLite commit point: an orphan r2 object on disk.
    let mut orphan = c1.clone();
    orphan.id = "f".repeat(64);
    orphan.revision = 2;
    orphan.parent = Some(c1.id.clone());
    fs::write(
        root.join(".vcrs/commits")
            .join(format!("{}.json", orphan.id)),
        serde_json::to_vec(&orphan).unwrap(),
    )
    .unwrap();

    let client = Client::discover(root).unwrap();
    let log = client.log(10).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].id, c1.id);

    // The next real commit takes r2 and supersedes the orphan.
    write(root, "a.txt", "2\n");
    let c2 = client.commit("r2", "a").unwrap();
    assert_eq!(c2.revision, 2);
    assert_eq!(client.log(10).unwrap()[0].id, c2.id);
}

#[test]
fn unversioned_files_are_listed_but_never_committed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "a.txt", "a\n");
    add(&client, &["a.txt"]);
    client.commit("r1", "a").unwrap();

    write(root, "secret.env", "TOKEN=x\n");
    assert!(client.status().unwrap().is_empty());
    assert_eq!(client.unversioned().unwrap(), vec!["secret.env".to_owned()]);
    write(root, "a.txt", "A\n");
    client.commit("r2", "a").unwrap();
    assert!(client.cat_revision_file("2", "secret.env").is_err());
}

#[test]
fn revert_never_touches_unversioned_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "a.txt", "a\n");
    add(&client, &["a.txt"]);
    client.commit("r1", "a").unwrap();

    write(root, "notes.txt", "my notes\n");
    write(root, "a.txt", "changed\n");
    write(root, "new.txt", "new\n");
    add(&client, &["new.txt"]);
    client.revert(&[]).unwrap();

    assert_eq!(read(root, "a.txt"), "a\n");
    assert_eq!(read(root, "notes.txt"), "my notes\n");
    // A reverted addition stays on disk, just unversioned again.
    assert_eq!(read(root, "new.txt"), "new\n");
    assert!(client.status().unwrap().is_empty());
    assert_eq!(
        client.unversioned().unwrap(),
        vec!["new.txt".to_owned(), "notes.txt".to_owned()]
    );
}

#[test]
fn rm_and_move_are_recorded_explicitly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "a.txt", "alpha\n");
    write(root, "gone.txt", "bye\n");
    add(&client, &["a.txt", "gone.txt"]);
    client.commit("r1", "a").unwrap();

    client.remove(&["gone.txt".to_owned()], false).unwrap();
    assert!(!root.join("gone.txt").exists());
    client.move_path("a.txt", "b.txt").unwrap();
    let st = client.status().unwrap();
    let moved = st.iter().find(|c| c.path == "b.txt").unwrap();
    assert_eq!(moved.moved_from.as_deref(), Some("a.txt"));

    let c2 = client.commit("r2", "a").unwrap();
    let paths: Vec<&str> = c2.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["b.txt"]);
    let b = c2.files.iter().find(|f| f.path == "b.txt").unwrap();
    assert_eq!(b.copy_from_path.as_deref(), Some("a.txt"));
}

#[test]
fn update_never_overwrites_an_unversioned_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "a.txt", "a\n");
    add(&client, &["a.txt"]);
    client.commit("r1", "a").unwrap();
    write(root, "b.txt", "from r2\n");
    add(&client, &["b.txt"]);
    client.commit("r2", "a").unwrap();

    client.update_to_revision("1").unwrap();
    assert!(!root.join("b.txt").exists());
    write(root, "b.txt", "my local file\n");
    let outcome = client.update_to_revision("HEAD");
    // The obstruction is reported, and the local file survives either way.
    let _ = outcome;
    assert_eq!(read(root, "b.txt"), "my local file\n");
}

#[test]
fn narrowing_depth_keeps_local_work() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "top.txt", "t\n");
    write(root, "sub/clean.txt", "c\n");
    write(root, "sub/edited.txt", "e\n");
    add(&client, &["top.txt", "sub"]);
    client.commit("r1", "a").unwrap();

    write(root, "sub/edited.txt", "local edit\n");
    write(root, "sub/untracked.txt", "u\n");
    client
        .update_to_revision_with_depth("HEAD", Some(version_control_rs::Depth::Files))
        .unwrap();

    assert!(
        !root.join("sub/clean.txt").exists(),
        "unmodified file pruned"
    );
    assert_eq!(read(root, "sub/edited.txt"), "local edit\n");
    assert_eq!(read(root, "sub/untracked.txt"), "u\n");
    // Out-of-depth files are neither reported as deleted nor dropped by a commit.
    assert!(client.status().unwrap().is_empty());
    write(root, "top.txt", "T\n");
    let c2 = client.commit("r2", "a").unwrap();
    assert!(c2.files.iter().any(|f| f.path == "sub/clean.txt"));
}

fn conflicting_update(root: &Path) -> Client {
    let client = Client::init(root).unwrap();
    write(root, "f.txt", "a\nb\nc\n");
    add(&client, &["f.txt"]);
    client.commit("r1", "a").unwrap();
    write(root, "f.txt", "a\nB\nc\n");
    client.commit("r2", "a").unwrap();
    client.update_to_revision("1").unwrap();
    write(root, "f.txt", "a\nX\nc\n");
    client.update_to_revision("HEAD").unwrap();
    client
}

#[test]
fn conflicts_persist_and_block_commit_until_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = conflicting_update(root);

    // BASE moved to r2 despite the conflict, and the record survives status.
    let repo = Repository::discover(root).unwrap();
    assert_eq!(repo.resolve_revision_spec("BASE").unwrap(), 2);
    for _ in 0..2 {
        let st = client.status().unwrap();
        assert!(st.iter().any(|c| c.path == "f.txt" && c.conflicted));
    }
    assert!(client.conflicts().unwrap().contains_key("f.txt"));
    // Artifacts are unversioned, never committed.
    assert!(
        client
            .unversioned()
            .unwrap()
            .contains(&"f.txt.mine".to_owned())
    );
    assert!(client.commit("attempt", "a").is_err());
    assert!(
        client.update_to_revision("HEAD").is_err(),
        "update waits for resolve"
    );

    write(root, "f.txt", "a\nB+X\nc\n");
    client
        .resolve(
            &["f.txt".to_owned()],
            version_control_rs::ResolveAccept::Working,
        )
        .unwrap();
    assert!(!root.join("f.txt.mine").exists());
    assert!(client.conflicts().unwrap().is_empty());
    let c3 = client.commit("resolved", "a").unwrap();
    assert_eq!(c3.revision, 3);
    assert_eq!(
        client.cat_revision_file("3", "f.txt").unwrap(),
        b"a\nB+X\nc\n"
    );
}

#[test]
fn resolve_can_take_the_incoming_version() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = conflicting_update(root);
    client
        .resolve(&[], version_control_rs::ResolveAccept::TheirsFull)
        .unwrap();
    assert_eq!(read(root, "f.txt"), "a\nB\nc\n");
    assert!(client.status().unwrap().is_empty());
}

#[test]
fn non_overlapping_edits_merge_cleanly_on_update() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "f.txt", "1\n2\n3\n4\n5\n6\n7\n8\n");
    add(&client, &["f.txt"]);
    client.commit("r1", "a").unwrap();
    write(root, "f.txt", "ONE\n2\n3\n4\n5\n6\n7\n8\n");
    client.commit("r2", "a").unwrap();
    client.update_to_revision("1").unwrap();

    write(root, "f.txt", "1\n2\n3\n4\n5\n6\n7\nEIGHT\n");
    client.update_to_revision("HEAD").unwrap();
    assert!(client.conflicts().unwrap().is_empty());
    assert_eq!(read(root, "f.txt"), "ONE\n2\n3\n4\n5\n6\n7\nEIGHT\n");
    let st = client.status().unwrap();
    assert_eq!(st.len(), 1);
    assert!(!st[0].conflicted);
}
