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
                    // Another thread's commit may already have included this
                    // file; then there is nothing left to commit.
                    match client.commit(&format!("t{t} c{i}"), "a") {
                        Ok(_) | Err(version_control_rs::VcsError::NothingToCommit) => {}
                        Err(e) => panic!("{e}"),
                    }
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

fn merge_fixture(root: &Path) -> Client {
    let client = Client::init(root).unwrap();
    write(root, "f.txt", "1\n");
    add(&client, &["f.txt"]);
    client.commit("r1", "a").unwrap();
    write(root, "f.txt", "2\n");
    client.commit("r2", "a").unwrap();
    write(root, "g.txt", "g\n");
    add(&client, &["g.txt"]);
    client.commit("r3", "a").unwrap();
    client
}

#[test]
fn merging_an_old_revision_does_not_revert_newer_changes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = merge_fixture(root);
    // r2 is already part of BASE (r3): merging it again is a no-op, and in
    // particular must not delete g.txt that was added later.
    let outcome = client.merge("2", false, false).unwrap();
    assert!(outcome.conflicts.is_empty());
    assert!(root.join("g.txt").exists());
    assert!(client.status().unwrap().is_empty());
}

#[test]
fn merge_cherry_picks_the_delta_of_one_revision() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = merge_fixture(root);
    write(root, "f.txt", "1\n");
    client.commit("r4 revert f", "a").unwrap();

    client.merge("2", false, false).unwrap();
    assert_eq!(read(root, "f.txt"), "2\n");
    assert!(root.join("g.txt").exists());
    let c5 = client.commit("r5 re-apply r2", "a").unwrap();
    assert_eq!(c5.mergeinfo.get("/").map(String::as_str), Some("2"));
}

#[test]
fn reverse_range_merge_undoes_a_revision() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = merge_fixture(root);
    client.merge("3:2", false, false).unwrap();
    assert!(!root.join("g.txt").exists());
    let st = client.status().unwrap();
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].path, "g.txt");
    assert_eq!(st[0].kind, ChangeKind::Deleted);
}

#[test]
fn inherited_properties_are_not_baked_into_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    client
        .set_inherited_property("", "team:owner", "platform")
        .unwrap();
    write(root, "a.txt", "a\n");
    add(&client, &["a.txt"]);
    let c1 = client.commit("r1", "a").unwrap();
    let entry = c1.files.iter().find(|f| f.path == "a.txt").unwrap();
    assert!(!entry.props.contains_key("team:owner"), "{:?}", entry.props);
    assert_eq!(client.get_property("a.txt", "team:owner").unwrap(), None);
    assert!(client.status().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn clearing_the_executable_bit_removes_the_property() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "run.sh", "#!/bin/sh\n");
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    add(&client, &["run.sh"]);
    let c1 = client.commit("r1", "a").unwrap();
    assert!(c1.files[0].props.contains_key("svn:executable"));

    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    let st = client.status().unwrap();
    assert_eq!(st.len(), 1);
    assert!(st[0].props_modified);
    let c2 = client.commit("r2", "a").unwrap();
    assert!(!c2.files[0].props.contains_key("svn:executable"));
    assert!(!c2.files[0].executable);
}

#[test]
fn update_applies_and_merges_property_changes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "f.txt", "1\n");
    add(&client, &["f.txt"]);
    client.commit("r1", "a").unwrap();
    client.set_property("f.txt", "x:review", "done").unwrap();
    write(root, "f.txt", "2\n");
    client.commit("r2", "a").unwrap();

    client.update_to_revision("1").unwrap();
    assert_eq!(client.get_property("f.txt", "x:review").unwrap(), None);
    client.set_property("f.txt", "x:local", "yes").unwrap();
    client.update_to_revision("HEAD").unwrap();

    assert!(client.conflicts().unwrap().is_empty());
    assert_eq!(read(root, "f.txt"), "2\n");
    assert_eq!(
        client.get_property("f.txt", "x:review").unwrap().as_deref(),
        Some("done")
    );
    assert_eq!(
        client.get_property("f.txt", "x:local").unwrap().as_deref(),
        Some("yes")
    );
    let st = client.status().unwrap();
    assert_eq!(st.len(), 1);
    assert!(st[0].props_modified && !st[0].text_modified);
}

#[test]
fn keyword_contraction_never_eats_following_text() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    let text = "Version $Rev: pending\nline two\ncosts $5\n";
    write(root, "k.txt", text);
    add(&client, &["k.txt"]);
    client.set_property("k.txt", "svn:keywords", "Rev").unwrap();
    client.commit("r1", "a").unwrap();
    assert_eq!(
        client.cat_revision_file("1", "k.txt").unwrap(),
        text.as_bytes()
    );
}

#[test]
fn diff_is_against_base_not_head() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "f.txt", "v1\n");
    add(&client, &["f.txt"]);
    client.commit("r1", "a").unwrap();
    write(root, "f.txt", "v2\n");
    client.commit("r2", "a").unwrap();
    client.update_to_revision("1").unwrap();
    assert_eq!(client.diff(None, None).unwrap(), "");

    write(root, "f.txt", "v1b\n");
    let patch = client.diff(None, None).unwrap();
    assert!(
        patch.contains("-v1\n") && patch.contains("+v1b\n"),
        "{patch}"
    );
    assert!(!patch.contains("v2"), "{patch}");
}

#[test]
fn diff_ignores_eol_translation_and_filters_by_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "src/a.txt", "1\n2\n3\n");
    write(root, "srcfoo.txt", "x\n");
    add(&client, &["src/a.txt", "srcfoo.txt"]);
    client
        .set_property("src/a.txt", "svn:eol-style", "CRLF")
        .unwrap();
    client.commit("r1", "a").unwrap();
    assert_eq!(read(root, "src/a.txt"), "1\r\n2\r\n3\r\n");

    write(root, "src/a.txt", "1\r\nTWO\r\n3\r\n");
    write(root, "srcfoo.txt", "y\n");
    let patch = client.diff(Some("src"), None).unwrap();
    assert!(!patch.contains("srcfoo"), "{patch}");
    let removed = patch
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .count();
    assert_eq!(removed, 1, "only the edited line differs:\n{patch}");
}

#[test]
fn stale_hunk_selection_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    let text = |two: &str, twenty_eight: &str| -> String {
        (1..=30)
            .map(|i| match i {
                2 => format!("{two}\n"),
                28 => format!("{twenty_eight}\n"),
                _ => format!("{i}\n"),
            })
            .collect()
    };
    write(root, "f.txt", &text("2", "28"));
    add(&client, &["f.txt"]);
    client.commit("r1", "a").unwrap();

    write(root, "f.txt", &text("TWO", "TWENTY-EIGHT"));
    assert_eq!(client.hunks("f.txt").unwrap().len(), 2);
    client.stage_hunks("f.txt", &[1]).unwrap();

    // Editing the staged hunk afterwards must not silently commit another one.
    write(root, "f.txt", &text("TWO", "XXVIII"));
    let err = client.commit_staged("partial", "a", false).unwrap_err();
    assert!(err.to_string().contains("stage its hunks again"), "{err}");

    // Re-staging picks the current content.
    client.stage_hunks("f.txt", &[1]).unwrap();
    client.commit_staged("partial", "a", false).unwrap();
    let committed = String::from_utf8(client.cat_revision_file("2", "f.txt").unwrap()).unwrap();
    assert_eq!(committed, text("2", "XXVIII"));
}

#[test]
fn empty_commit_is_an_error_not_a_fake_commit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    assert!(matches!(
        client.commit("nothing", "a"),
        Err(version_control_rs::VcsError::NothingToCommit)
    ));
}

#[test]
fn record_only_merge_is_committed_once_and_respects_out_of_date() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = merge_fixture(root);
    client.merge("sub@2", false, true).unwrap();
    // Reopen: the old work queue re-added the merge with scope "/".
    let client = Client::discover(root).unwrap();
    let c4 = client.commit("record", "a").unwrap();
    assert_eq!(c4.mergeinfo.get("sub").map(String::as_str), Some("2"));
    assert!(!c4.mergeinfo.contains_key("/"), "{:?}", c4.mergeinfo);

    // A stale working copy cannot commit even with recorded merges.
    client.update_to_revision("3").unwrap();
    client.merge("1", false, true).unwrap();
    assert!(matches!(
        client.commit("stale", "a"),
        Err(version_control_rs::VcsError::OutOfDate { .. })
    ));
}

#[test]
fn copies_are_additions_with_history() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "a.txt", "a\n");
    add(&client, &["a.txt"]);
    client.commit("r1", "a").unwrap();
    client.copy_path("a.txt", "b.txt").unwrap();
    let c2 = client.commit("copy", "a").unwrap();
    let cp = c2.changed_paths.iter().find(|p| p.path == "b.txt").unwrap();
    assert_eq!(cp.action, version_control_rs::ChangedPathAction::Add);
    assert_eq!(cp.copyfrom_path.as_deref(), Some("a.txt"));
    assert_eq!(cp.copyfrom_rev, Some(1));
}

#[cfg(unix)]
#[test]
fn failing_post_commit_hook_does_not_fail_the_commit() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    let hook = root.join(".vcrs/hooks/post-commit");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(&hook, "#!/bin/sh\necho boom >&2\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    write(root, "a.txt", "a\n");
    add(&client, &["a.txt"]);
    assert_eq!(client.commit("r1", "a").unwrap().revision, 1);
    assert!(read(root, ".vcrs/hooks.log").contains("boom"));
}

fn two_clones(dir: &Path) -> (String, Client, Client) {
    let srv = dir.join("srv");
    let server = Client::init(&srv).unwrap();
    write(&srv, "f.txt", "a\nb\nc\n");
    add(&server, &["f.txt"]);
    server.commit("r1", "s").unwrap();
    let url = format!("file://{}", srv.display());
    let a = Client::checkout_remote(&url, dir.join("A"), None).unwrap();
    let b = Client::checkout_remote(&url, dir.join("B"), None).unwrap();
    (url, a, b)
}

#[test]
fn pull_replays_unpushed_local_commits_instead_of_dropping_them() {
    let dir = tempfile::tempdir().unwrap();
    let (_url, a, b) = two_clones(dir.path());
    write(a.root(), "a.txt", "from A\n");
    add(&a, &["a.txt"]);
    a.commit("A r2", "alice").unwrap();
    a.push().unwrap();

    write(b.root(), "b.txt", "from B\n");
    add(&b, &["b.txt"]);
    b.commit("B r2", "bob").unwrap();
    assert!(b.push().is_err(), "diverged push must be refused");

    let outcome = b.pull().unwrap();
    assert_eq!(outcome.rebased, 1);
    let log = b.log(10).unwrap();
    let messages: Vec<&str> = log.iter().map(|c| c.message.as_str()).collect();
    assert_eq!(messages, vec!["B r2", "A r2", "r1"]);
    assert_eq!(log[0].revision, 3);
    assert_eq!(read(b.root(), "a.txt"), "from A\n");
    assert_eq!(read(b.root(), "b.txt"), "from B\n");
    assert!(b.status().unwrap().is_empty());

    b.push().unwrap();
    a.pull().unwrap();
    assert_eq!(read(a.root(), "b.txt"), "from B\n");
    assert_eq!(a.log(10).unwrap().len(), 3);
}

#[test]
fn conflicting_pull_changes_nothing_and_push_stays_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (_url, a, b) = two_clones(dir.path());
    write(a.root(), "f.txt", "a\nA\nc\n");
    a.commit("A edit", "alice").unwrap();
    a.push().unwrap();

    write(b.root(), "f.txt", "a\nB\nc\n");
    let b_commit = b.commit("B edit", "bob").unwrap();
    let err = b.pull().unwrap_err();
    assert!(
        matches!(err, version_control_rs::VcsError::Diverged { .. }),
        "{err}"
    );
    assert_eq!(b.log(10).unwrap()[0].id, b_commit.id);
    assert_eq!(read(b.root(), "f.txt"), "a\nB\nc\n");

    // The remote HEAD object was fetched, but it is not an ancestor: pushing
    // must not overwrite the remote history.
    assert!(b.push().is_err());
    let srv = Client::discover(dir.path().join("srv")).unwrap();
    assert_eq!(srv.log(1).unwrap()[0].message, "A edit");
}

#[test]
fn path_locks_are_exclusive_and_enforced_on_commit() {
    use version_control_rs::TreeEdits;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let client = Client::init(&root).unwrap();
    write(&root, "a.txt", "a\n");
    write(&root, "doc.bin", "d\n");
    add(&client, &["a.txt", "doc.bin"]);
    client
        .set_property("doc.bin", "svn:needs-lock", "*")
        .unwrap();
    client.commit("r1", "a").unwrap();

    // Concurrent LOCK requests: exactly one user wins.
    let winners: Vec<_> = (0..8)
        .map(|i| {
            let root = root.clone();
            std::thread::spawn(move || {
                Repository::discover(&root)
                    .unwrap()
                    .lock_path("a.txt", &format!("user{i}"))
                    .is_ok()
            })
        })
        .map(|t| t.join().unwrap())
        .collect();
    assert_eq!(winners.iter().filter(|w| **w).count(), 1);

    let repo = Repository::discover(&root).unwrap();
    let holder = repo.path_locks().unwrap()["a.txt"].owner.clone();
    let edit = |path: &str| TreeEdits {
        puts: [(path.to_owned(), b"changed\n".to_vec())].into(),
        ..TreeEdits::default()
    };
    assert!(matches!(
        repo.commit_edits(&edit("a.txt"), "m", "intruder"),
        Err(version_control_rs::VcsError::LockConflict { .. })
    ));
    assert_eq!(
        repo.commit_edits(&edit("a.txt"), "m", &holder)
            .unwrap()
            .revision,
        2
    );

    // svn:needs-lock files require holding the lock.
    assert!(matches!(
        repo.commit_edits(&edit("doc.bin"), "m", "bob"),
        Err(version_control_rs::VcsError::NeedsLockRequired { .. })
    ));
    repo.lock_path("doc.bin", "bob").unwrap();
    assert!(
        repo.unlock_path("doc.bin", "eve").is_err(),
        "only the owner unlocks"
    );
    assert_eq!(
        repo.commit_edits(&edit("doc.bin"), "m", "bob")
            .unwrap()
            .revision,
        3
    );
    repo.unlock_path("doc.bin", "bob").unwrap();
}

#[test]
fn legacy_json_locks_are_imported() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    Client::init(root).unwrap();
    fs::write(root.join(".vcrs/locks.json"), r#"{"a.txt":"alice"}"#).unwrap();
    let repo = Repository::discover(root).unwrap();
    assert_eq!(repo.path_locks().unwrap()["a.txt"].owner, "alice");
    assert!(!root.join(".vcrs/locks.json").exists());
}

#[test]
fn commits_store_a_tree_not_a_manifest_and_gc_drops_orphans() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    for i in 0..20 {
        write(root, &format!("dir/f{i}.txt"), &format!("{i}\n"));
    }
    add(&client, &["dir"]);
    client.commit("r1", "a").unwrap();
    write(root, "dir/f3.txt", "changed\n");
    let c2 = client.commit("r2", "a").unwrap();
    assert_eq!(c2.files.len(), 20, "readers still get the full file list");

    let raw = read(root, &format!(".vcrs/commits/{}.json", c2.id));
    assert!(raw.contains("\"tree\""), "{raw}");
    assert!(!raw.contains("\"files\""), "no per-commit manifest: {raw}");
    assert!(
        raw.len() < 2000,
        "commit size is O(changes): {} bytes",
        raw.len()
    );

    // An orphan commit object (e.g. from an interrupted commit) is collected.
    let mut orphan = c2.clone();
    orphan.id = "e".repeat(64);
    fs::write(
        root.join(".vcrs/commits")
            .join(format!("{}.json", orphan.id)),
        serde_json::to_vec(&orphan).unwrap(),
    )
    .unwrap();
    let stats = client.gc().unwrap();
    assert_eq!(stats.commits_removed, 1);
    assert_eq!(client.cat_revision_file("1", "dir/f3.txt").unwrap(), b"3\n");
    assert_eq!(
        client.cat_revision_file("2", "dir/f3.txt").unwrap(),
        b"changed\n"
    );
}

#[test]
fn tampered_commits_and_objects_are_detected() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "a.txt", "original\n");
    add(&client, &["a.txt"]);
    let c1 = client.commit("honest message", "alice").unwrap();

    // Rewrite the author of the stored commit.
    let commit_file = root.join(".vcrs/commits").join(format!("{}.json", c1.id));
    let original = fs::read_to_string(&commit_file).unwrap();
    fs::write(&commit_file, original.replace("\"alice\"", "\"mallory\"")).unwrap();
    let err = client.log(10).unwrap_err();
    assert!(
        matches!(err, version_control_rs::VcsError::CorruptObject(_)),
        "{err}"
    );
    fs::write(&commit_file, &original).unwrap();

    // Replace the stored content of a.txt with other (valid) data.
    let blob = &c1.files[0].blob_id;
    let object = root
        .join(".vcrs/objects")
        .join(&blob[..2])
        .join(format!("{}.z", &blob[2..]));
    fs::write(&object, zstd::bulk::compress(b"forged\n", 3).unwrap()).unwrap();
    let err = client.cat_revision_file("1", "a.txt").unwrap_err();
    assert!(
        matches!(err, version_control_rs::VcsError::CorruptObject(_)),
        "{err}"
    );
}

#[test]
fn malformed_ids_are_errors_not_panics() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    for id in ["", "a", "../../etc/passwd", &"G".repeat(64)] {
        assert!(repo.read_blob(id).is_err(), "{id:?}");
        assert!(repo.read_commit(id).is_err(), "{id:?}");
    }
}

fn set_mtime(path: &Path, when: std::time::SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

#[test]
fn stat_cache_avoids_rehashing_unchanged_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "a.txt", "aaaa\n");
    add(&client, &["a.txt"]);
    client.commit("r1", "a").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    set_mtime(&root.join("a.txt"), old);
    assert!(client.status().unwrap().is_empty()); // hashes once, caches

    // Same size and mtime: the cached hash is trusted (the file is not read).
    write(root, "a.txt", "bbbb\n");
    set_mtime(&root.join("a.txt"), old);
    assert!(client.status().unwrap().is_empty());

    // A real modification changes the mtime and is detected.
    write(root, "a.txt", "cccc\n");
    assert_eq!(client.status().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn ignored_directories_are_not_descended() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    client.add_ignore("node_modules").unwrap();
    write(root, "node_modules/pkg/index.js", "x\n");
    write(root, "target/out.txt", "build output\n");
    let locked = root.join("node_modules/pkg");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    // Walking into the unreadable ignored directory would fail.
    let unversioned = client.unversioned();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    // `target` is not special without an ignore rule.
    assert_eq!(unversioned.unwrap(), vec!["target/out.txt".to_owned()]);
}

#[test]
fn blame_restarts_after_delete_and_credits_merged_lines() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let client = Client::init(root).unwrap();
    write(root, "f.txt", "a\n");
    add(&client, &["f.txt"]);
    client.commit("r1", "ann").unwrap();
    client.remove(&["f.txt".to_owned()], false).unwrap();
    client.commit("r2 delete", "ann").unwrap();
    write(root, "f.txt", "a\nb\n");
    add(&client, &["f.txt"]);
    client.commit("r3 re-add", "bob").unwrap();
    let blame = client.blame("f.txt", None).unwrap();
    assert!(blame.iter().all(|l| l.revision == 3), "{blame:?}");

    // r4 adds a line, r5 reverts it (and shifts every line down), r6
    // cherry-picks r4 back: the line is credited to its original author, not
    // to the merge commit, although its line number differs from r4.
    write(root, "f.txt", "a\nb\nc\n");
    client.commit("r4", "carol").unwrap();
    write(root, "f.txt", "z\na\nb\n");
    client.commit("r5 revert", "dave").unwrap();
    client.merge("4", false, false).unwrap();
    client.commit("r6 re-apply r4", "erin").unwrap();
    let blame = client.blame("f.txt", None).unwrap();
    let c = blame.iter().find(|l| l.content == "c").unwrap();
    assert_eq!((c.revision, c.author.as_str()), (4, "carol"));
    let b = blame.iter().find(|l| l.content == "b").unwrap();
    assert_eq!(b.revision, 3);
}

#[test]
fn pull_transfers_only_reachable_verified_objects() {
    let dir = tempfile::tempdir().unwrap();
    let (_url, a, b) = two_clones(dir.path());
    // Garbage in the remote store: an orphan commit and a corrupt object.
    let srv = dir.path().join("srv/.vcrs");
    fs::write(
        srv.join("commits").join(format!("{}.json", "d".repeat(64))),
        "{}",
    )
    .unwrap();
    fs::create_dir_all(srv.join("objects/dd")).unwrap();
    fs::write(
        srv.join("objects/dd").join(format!("{}.z", "d".repeat(62))),
        "junk",
    )
    .unwrap();

    write(a.root(), "n.txt", "new\n");
    add(&a, &["n.txt"]);
    a.commit("A r2", "alice").unwrap();
    a.push().unwrap();
    b.pull().unwrap();
    assert_eq!(read(b.root(), "n.txt"), "new\n");
    let local = b.root().join(".vcrs");
    assert!(
        !local
            .join("commits")
            .join(format!("{}.json", "d".repeat(64)))
            .exists()
    );
    assert!(!local.join("objects/dd").exists());
}
