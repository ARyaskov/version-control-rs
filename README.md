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
- 3-way merge baseline with conflict artifacts:
  - `.mine`, `.rOLD`, `.rNEW`
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
vcrs status
vcrs diff
vcrs commit -m "message" --author "you"

vcrs log --limit 20
vcrs log -r 1:10 -v
vcrs changed -r 7

vcrs update -r HEAD
vcrs revert [optional/path ...]
vcrs merge -r 7 [--dry-run] [--record-only]

vcrs prop-set --path src/main.rs --name svn:eol-style --value LF
vcrs prop-get --path src/main.rs --name svn:eol-style
vcrs prop-del --path src/main.rs --name svn:eol-style

vcrs ignore add "*.tmp"
vcrs ignore list

vcrs externals set vendor/lib https://example.com/svn/lib --revision 123
vcrs externals list

vcrs serve-http --repo . --host 127.0.0.1 --port 3690
```

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
- Content-addressed blobs (`blake3`) are stored under `.vcrs/objects/`
- Commits are JSON documents under `.vcrs/commits/`
- SQLite WC database: `.vcrs/wc.db`
- Numeric revisions are tracked in WC DB (`revisions` table) and linked to commit IDs.


## License 

Apache 2.0
