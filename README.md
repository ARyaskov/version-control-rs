# version-control-rs

SVN-like version control engine in Rust (library + CLI).

## Implemented features

- SQLite working-copy database (`.vcrs/wc.db`) with SVN-inspired tables:
  - `nodes` (BASE/WORKING split)
  - `actual_node` (text/prop mods, conflict markers, tree-conflicts)
  - `wc_lock`, `work_queue`
  - `revisions` (numeric `rN` model)
  - `ignore_rules`, `externals`, `file_props`
- Commit/update/status/diff/revert with revision-aware behavior
- Out-of-date commit protection (`base rN` vs `head rN`)
- 3-way merge on update/merge: non-overlapping edits merge cleanly; overlapping
  ones leave conflict markers plus `.mine`, `.rOLD`, `.rNEW` artifacts. Conflicts
  are recorded in wc.db (`C` in `status`), block commits and further updates of
  the working copy until `vcrs resolve`.
- Properties (`prop-set`, `prop-get`, `prop-del`)
- Ignore rules (`ignore add/list`) and externals metadata (`externals set/list`)
- Log by range with verbose changed paths (`log -r N:M -v`)
- HTTP server on Actix (`serve-http`) with SVN/WebDAV-compatible discovery/report and commit activity skeleton
  - supports `MKACTIVITY/CHECKOUT/PUT/PROPPATCH/MERGE`
  - supports `svndiff` decode/apply in `PUT` (`SVN\\0/\\1` stream)
  - supports `update-report` replay skeleton with `txdelta` payloads

## CLI

Binary: `vcrs`

```bash
vcrs init .
vcrs checkout file:///srv/repos/demo ./demo   # remotes: file:// URLs or paths
vcrs switch file:///srv/repos/other
vcrs pull
vcrs push

vcrs add src/main.rs docs/      # put files/directories under version control
vcrs add .                      # add every unversioned, non-ignored file
vcrs rm old.txt [--keep-local]  # schedule a versioned file for deletion

vcrs status                     # M/A/D for versioned changes, ? for unversioned files
vcrs staged
vcrs diff
vcrs diff --path src/main.rs
vcrs cat src/main.rs@5

vcrs stage src/main.rs README.md
vcrs unstage README.md
vcrs stage-all
vcrs stage-clear
vcrs hunks src/main.rs
vcrs stage-hunks src/main.rs 0 2
vcrs unstage-hunks src/main.rs 2
vcrs commit -m "message" --author "you"
vcrs commit -m "message" --author "you" --all
vcrs commit -m "message" --author "you" --push

vcrs log --limit 20
vcrs log -r 1:10 -v
vcrs log -r 1:HEAD --include-merged
vcrs changed -r 7
vcrs blame src/main.rs
vcrs blame src/main.rs -r 7

vcrs update -r HEAD
vcrs revert [optional/path ...]
vcrs merge -r 7 [--dry-run] [--record-only]   # cherry-pick the change made by r7
vcrs merge -r 3:7                             # apply the changes from r3 to r7
vcrs merge -r 7:6                             # undo r7
vcrs resolve src/main.rs [--accept working|mine-full|theirs-full|base]
vcrs lock path/to/file
vcrs unlock path/to/file
vcrs copy src/lib.rs src/lib_copy.rs
vcrs move src/old.rs src/new.rs

vcrs prop-set --path src/main.rs --name svn:eol-style --value LF
vcrs prop-get --path src/main.rs --name svn:eol-style
vcrs prop-del --path src/main.rs --name svn:eol-style
vcrs iprop-set --scope trunk --name svn:global-ignores --value target
vcrs iprop-list

vcrs changelist set src/main.rs ui
vcrs changelist clear src/main.rs
vcrs changelist list

vcrs ignore add "*.tmp"
vcrs ignore list

vcrs externals set vendor/lib https://example.com/svn/lib --revision 123
vcrs externals list

vcrs passwd alice --repo .       # password read from stdin, stored hashed
vcrs serve-http --repo . --host 127.0.0.1 --port 3690
```

Common aliases:

- `vcrs checkout` -> `vcrs co`
- `vcrs status` -> `vcrs st`
- `vcrs diff` -> `vcrs di`
- `vcrs commit` -> `vcrs ci`
- `vcrs update` -> `vcrs up`

### Versioned files

Only files put under version control with `vcrs add` (or received from a
checkout/update) are tracked; everything else is reported as `?` by `status`
and is never committed, reverted or deleted by `vcrs`. `copy`/`move` record
history explicitly, `rm` schedules a deletion, and deleting a versioned file
from disk is committed as a deletion. `revert` restores versioned files to
BASE and un-schedules additions (the files stay on disk, unversioned).

Ignore rules come from `vcrs ignore add <glob>` and a `.vcrsignore` file (one
glob per line, `#` comments). Nothing is ignored by convention — add build
directories yourself (e.g. `target/**`, `node_modules`). Ignored directories are
not descended into.

`status` keeps a stat cache (size, mtime, normalization settings) in `wc.db`
and only re-reads files whose metadata changed.

### Staging workflow

`vcrs` supports an explicit staging area stored in `.vcrs/client-stage-index.json`.

- `stage [paths ...]` stages one or more changed paths
- `unstage [paths ...]` removes paths from the staging area
- `stage-all` stages all current changes
- `stage-clear` clears the staging area completely
- `staged` shows only staged changes
- `hunks <path>` lists per-file diff hunks and marks staged ones with `*`
- `stage-hunks <path> <indices...>` stages selected hunks for a file
- `unstage-hunks <path> <indices...>` removes selected hunks from the staged set
- `commit -m ...` commits staged changes by default
- `commit --all -m ...` bypasses the staging area and commits all working-copy changes
- `commit --push -m ...` pushes after a successful commit

### Command reference

- Working copy and remotes: `init`, `checkout`, `switch`, `pull`, `push`
- Local file operations: `add`, `rm`, `copy`, `move`, `revert`, `lock`, `unlock`
- Inspection: `status`, `staged`, `diff`, `cat`, `log`, `changed`, `blame`, `hunks`
- History and integration: `commit`, `update`, `merge`, `resolve`
- Properties and grouping: `prop-set`, `prop-get`, `prop-del`, `iprop-set`, `iprop-list`, `changelist set|clear|list`
- Metadata rules: `ignore add|list`, `externals set|list`
- Server mode: `serve-http`

### Server security

- **Authentication.** Accounts live in `.vcrs/passwd.json` as Argon2 hashes;
  create them with `vcrs passwd <user>` (password on stdin). Plain-text
  entries are refused at startup. Without `passwd.json` every client is
  anonymous (the `SVN-UserName` header is ignored) and writes are refused
  unless the server runs with `--allow-anonymous-write`.
- **Authorization.** `.vcrs/authz.json` grants `read`/`write` path prefixes per
  user (`{"users": {"alice": {"read": ["/"], "write": ["/docs"]}}}`). Rules are
  checked against the real file paths of every request, including the
  contents of update/log reports and each path of a commit. `authz.json`
  without `passwd.json` is refused.
- **Transport.** Basic credentials are only protected by TLS. The server binds
  to loopback by default and refuses other addresses unless
  `--allow-insecure-http` is given; put a TLS-terminating reverse proxy in
  front for network access.
- **Hooks** are not executed for HTTP commits unless `--enable-hooks`.
- **Locks** (`LOCK`/`vcrs lock`) are stored in the repository database, taken
  atomically, and enforced on commit: nobody else can commit a locked path,
  and `svn:needs-lock` files require holding the lock.
- **`file://` access** is plain filesystem access: whoever can open the
  repository directory can read and write it, and the username is only used
  as lock owner. `authz.json`/`passwd.json` apply to the HTTP server only.

### Remotes

`checkout`, `switch`, `pull`, `push`, `lock` and `unlock` work with remote
repositories on a filesystem path (`file:///path` or a plain path, e.g. a shared
or mounted directory). There is no client for the HTTP protocol yet: URLs with
other schemes (`http://`, `https://`, `svn://`) are rejected. The HTTP server
below is for SVN/WebDAV clients.

`serve-http` exposes a partial SVN/DAV compatibility layer intended as a foundation.
Read/discovery/report paths are implemented first; full interoperability with a stock `svn` client
for all write operations requires additional DeltaV/SVN wire semantics.

## Library usage

```rust
use version_control_rs::Client;

fn demo() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::discover(".")?;

    let status = client.status()?;
    if !status.is_empty() {
        let commit = client.commit("sync", "alice")?;
        println!("new revision: r{}", commit.revision);
    }

    Ok(())
}
```

## Architecture notes

- Repository metadata is stored in `.vcrs/`
- Objects are content-addressed by `blake3` and stored zstd-compressed under
  `.vcrs/objects/` (`<2 hex>/<62 hex>.z`): file contents and directory trees.
  Each tree lists one directory level, so unchanged subtrees are shared
  between revisions.
- Commits are JSON documents under `.vcrs/commits/` that reference their root
  tree (a commit's size is proportional to what it changed, not to the size of
  the repository).
- SQLite WC database: `.vcrs/wc.db` — revision index (the source of truth for
  HEAD), BASE, scheduled additions/deletions, conflicts, properties, locks.
- Every mutating operation holds an exclusive lock on `.vcrs/lock`; a commit
  becomes visible in a single SQLite transaction after its objects are durable.
- `vcrs gc` removes objects and commit files that are not part of the history.
- Repositories written by 0.2 (uncompressed objects, full manifests in
  commits, `HEAD` file) are read and migrated transparently.

## License 

Apache 2.0
