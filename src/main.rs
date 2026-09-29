use std::path::PathBuf;

use base64::Engine;
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use version_control_rs::{
    ChangeKind, ChangedPathAction, Client, Depth, FileChange, ResolveAccept, Result, VcsError,
};

#[derive(Parser, Debug)]
#[command(name = "vcrs")]
#[command(about = "SVN-like version control in Rust")]
struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Init(InitArgs),
    /// Put files or directories under version control.
    Add(StageArgs),
    /// Schedule versioned files for deletion.
    #[command(alias = "remove", alias = "delete")]
    Rm(RmArgs),
    #[command(alias = "co")]
    Checkout(CheckoutArgs),
    Switch(SwitchArgs),
    Pull,
    Push,
    Lock(LockArgs),
    Unlock(UnlockArgs),
    Copy(PathPairArgs),
    Move(PathPairArgs),
    #[command(alias = "st")]
    Status,
    Staged,
    #[command(alias = "di")]
    Diff(DiffArgs),
    Cat(CatArgs),
    #[command(alias = "ci")]
    Commit(CommitArgs),
    Log(LogArgs),
    Blame(BlameArgs),
    Revert(RevertArgs),
    #[command(alias = "up")]
    Update(UpdateArgs),
    Changed(ChangedArgs),
    /// Mark conflicts as resolved.
    Resolve(ResolveArgs),
    Merge(MergeArgs),
    Stage(StageArgs),
    Unstage(StageArgs),
    StageAll,
    StageClear,
    Hunks(HunksArgs),
    StageHunks(HunkSelectArgs),
    UnstageHunks(HunkSelectArgs),
    PropSet(PropSetArgs),
    PropGet(PropGetArgs),
    PropDel(PropDelArgs),
    IPropSet(IPropSetArgs),
    IPropList,
    Changelist(ChangelistArgs),
    Ignore(IgnoreArgs),
    Externals(ExternalsArgs),
    Gc,
    #[cfg(feature = "serve-http")]
    ServeHttp(ServeHttpArgs),
    /// Set a server password (read from stdin), stored as an Argon2 hash.
    #[cfg(feature = "serve-http")]
    Passwd(PasswdArgs),
}

#[derive(Args, Debug)]
struct InitArgs {
    #[arg(default_value = ".")]
    path: PathBuf,
}

#[derive(Args, Debug)]
struct DiffArgs {
    #[arg(long)]
    path: Option<String>,
    #[arg(long, value_name = "PATH@REV")]
    peg: Option<String>,
}

#[derive(Args, Debug)]
struct CatArgs {
    #[arg(value_name = "PATH@REV")]
    peg: String,
}

#[derive(Args, Debug)]
struct CommitArgs {
    #[arg(short = 'm', long)]
    message: String,
    #[arg(long, default_value = "unknown")]
    author: String,
    #[arg(long)]
    push: bool,
    #[arg(long)]
    all: bool,
}

#[derive(Args, Debug)]
struct LogArgs {
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(short = 'r', long)]
    range: Option<String>,
    #[arg(short = 'v', long)]
    verbose: bool,
    #[arg(long)]
    include_merged: bool,
    #[arg(long)]
    only_merged: bool,
}

#[derive(Args, Debug)]
struct CheckoutArgs {
    url: String,
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(long)]
    username: Option<String>,
    #[arg(long)]
    depth: Option<String>,
}

#[derive(Args, Debug)]
struct SwitchArgs {
    url: String,
    #[arg(long)]
    username: Option<String>,
}

#[derive(Args, Debug)]
struct LockArgs {
    path: String,
}

#[derive(Args, Debug)]
struct UnlockArgs {
    path: String,
}

#[derive(Args, Debug)]
struct BlameArgs {
    path: String,
    #[arg(short = 'r', long)]
    revision: Option<String>,
}

#[derive(Args, Debug)]
struct RevertArgs {
    paths: Vec<String>,
}

#[derive(Args, Debug)]
struct UpdateArgs {
    #[arg(short = 'r', long, default_value = "HEAD")]
    revision: String,
    #[arg(long)]
    depth: Option<String>,
}

#[derive(Args, Debug)]
struct ChangedArgs {
    #[arg(short = 'r', long)]
    revision: String,
}

#[derive(Args, Debug)]
struct MergeArgs {
    /// `N` merges the change made by rN (cherry-pick), `A:B` the changes from
    /// rA to rB (`B:A` undoes them); prefix with `path@` to limit the scope.
    #[arg(short = 'r', long)]
    revision: String,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    record_only: bool,
}

#[derive(Args, Debug)]
struct StageArgs {
    paths: Vec<String>,
}

#[derive(Args, Debug)]
struct ResolveArgs {
    /// Conflicted paths (all conflicts when omitted).
    paths: Vec<String>,
    /// Content to keep: working, mine-full, theirs-full or base.
    #[arg(long, default_value = "working")]
    accept: String,
}

#[derive(Args, Debug)]
struct RmArgs {
    paths: Vec<String>,
    /// Keep the files on disk (they become unversioned).
    #[arg(long)]
    keep_local: bool,
}

#[derive(Args, Debug)]
struct HunksArgs {
    path: String,
}

#[derive(Args, Debug)]
struct HunkSelectArgs {
    path: String,
    indices: Vec<usize>,
}

#[derive(Args, Debug)]
struct PropSetArgs {
    #[arg(long)]
    path: String,
    #[arg(long)]
    name: String,
    #[arg(long)]
    value: String,
}

#[derive(Args, Debug)]
struct PropGetArgs {
    #[arg(long)]
    path: String,
    #[arg(long)]
    name: String,
}

#[derive(Args, Debug)]
struct PropDelArgs {
    #[arg(long)]
    path: String,
    #[arg(long)]
    name: String,
}

#[derive(Args, Debug)]
struct IPropSetArgs {
    #[arg(long)]
    scope: String,
    #[arg(long)]
    name: String,
    #[arg(long)]
    value: String,
}

#[derive(Args, Debug)]
struct PathPairArgs {
    src: String,
    dst: String,
}

#[derive(Args, Debug)]
struct ChangelistArgs {
    #[command(subcommand)]
    command: ChangelistCmd,
}

#[derive(Subcommand, Debug)]
enum ChangelistCmd {
    Set { path: String, name: String },
    Clear { path: String },
    List,
}

#[derive(Args, Debug)]
struct IgnoreArgs {
    #[command(subcommand)]
    command: IgnoreCmd,
}

#[derive(Subcommand, Debug)]
enum IgnoreCmd {
    Add { pattern: String },
    List,
}

#[derive(Args, Debug)]
struct ExternalsArgs {
    #[command(subcommand)]
    command: ExternalsCmd,
}

#[cfg(feature = "serve-http")]
#[derive(Args, Debug)]
struct ServeHttpArgs {
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 3690)]
    port: u16,
    /// Execute `.vcrs/hooks` scripts for commits received over HTTP.
    #[arg(long)]
    enable_hooks: bool,
    /// Accept writes from unauthenticated clients (only relevant without
    /// .vcrs/passwd.json).
    #[arg(long)]
    allow_anonymous_write: bool,
    /// Serve plain HTTP on a non-loopback address (prefer a TLS reverse proxy).
    #[arg(long)]
    allow_insecure_http: bool,
}

#[cfg(feature = "serve-http")]
#[derive(Args, Debug)]
struct PasswdArgs {
    /// Account to create or update in .vcrs/passwd.json.
    user: String,
    #[arg(long, default_value = ".")]
    repo: PathBuf,
}

#[derive(Subcommand, Debug)]
enum ExternalsCmd {
    Set {
        path: String,
        target_url: String,
        #[arg(long)]
        revision: Option<String>,
    },
    List,
}

fn main() {
    let cli = Cli::parse();
    let json_output = cli.json;
    if let Err(err) = run(cli) {
        if json_output {
            let _ = print_json(json!({
                "ok": false,
                "error": err.to_string(),
            }));
        } else {
            eprintln!("error: {err}");
        }
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let json_output = cli.json;
    match cli.command {
        Commands::Init(args) => {
            let client = Client::init(args.path)?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "init",
                    "path": client.root().display().to_string(),
                }))?;
            } else {
                println!("Initialized repository at {}", client.root().display());
            }
        }
        Commands::Add(args) => {
            let client = Client::discover(".")?;
            let mut paths = Vec::new();
            for p in &args.paths {
                let rel = resolve_user_path(&client, p)?;
                // "vcrs add ." adds the whole tree.
                paths.push(if rel.is_empty() { ".".to_owned() } else { rel });
            }
            let added = if paths.iter().any(|p| p == ".") {
                client.add(&client.unversioned()?)?
            } else {
                client.add(&paths)?
            };
            if json_output {
                print_json(json!({"ok": true, "command": "add", "added": added}))?;
            } else {
                for p in added {
                    println!("A  {p}");
                }
            }
        }
        Commands::Rm(args) => {
            let client = Client::discover(".")?;
            let paths = args
                .paths
                .iter()
                .map(|p| repo_path(&client, p))
                .collect::<Result<Vec<_>>>()?;
            let removed = client.remove(&paths, args.keep_local)?;
            if json_output {
                print_json(json!({"ok": true, "command": "rm", "removed": removed}))?;
            } else {
                for p in removed {
                    println!("D  {p}");
                }
            }
        }
        Commands::Resolve(args) => {
            let client = Client::discover(".")?;
            let accept = match args.accept.as_str() {
                "working" => ResolveAccept::Working,
                "mine-full" => ResolveAccept::MineFull,
                "theirs-full" => ResolveAccept::TheirsFull,
                "base" => ResolveAccept::Base,
                other => {
                    return Err(VcsError::Protocol(format!(
                        "unknown --accept value '{other}' (expected working, mine-full, theirs-full or base)"
                    )));
                }
            };
            let paths = repo_paths(&client, &args.paths)?;
            let resolved = client.resolve(&paths, accept)?;
            if json_output {
                print_json(json!({"ok": true, "command": "resolve", "resolved": resolved}))?;
            } else {
                for p in resolved {
                    println!("Resolved conflict on {p}");
                }
            }
        }
        Commands::Checkout(args) => {
            let client = Client::checkout_remote(&args.url, &args.path, args.username.as_deref())?;
            if let Some(depth) = args.depth.as_deref().and_then(Depth::from_str) {
                client.set_depth(depth)?;
            }
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "checkout",
                    "url": args.url,
                    "path": client.root().display().to_string(),
                }))?;
            } else {
                println!("Checked out {} into {}", args.url, client.root().display());
            }
        }
        Commands::Switch(args) => {
            let client = Client::discover(".")?;
            client.switch_remote(&args.url, args.username.as_deref())?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "switch",
                    "url": args.url,
                }))?;
            } else {
                println!("Switched remote to {}", args.url);
            }
        }
        Commands::Pull => {
            let client = Client::discover(".")?;
            let outcome = client.pull_remote()?;
            if json_output {
                print_json(json!({"ok": true, "command": "pull", "outcome": outcome}))?;
            } else {
                println!("Updated from remote to r{}", outcome.head_revision);
                if outcome.rebased > 0 {
                    println!(
                        "Replayed {} local commit(s) on top of the remote history; push to publish",
                        outcome.rebased
                    );
                } else if outcome.ahead > 0 {
                    println!("{} local commit(s) not pushed yet", outcome.ahead);
                }
            }
        }
        Commands::Push => {
            let client = Client::discover(".")?;
            client.push_remote()?;
            if json_output {
                print_json(json!({"ok": true, "command": "push"}))?;
            } else {
                println!("Pushed to remote");
            }
        }
        Commands::Lock(args) => {
            let client = Client::discover(".")?;
            client.lock_remote(&repo_path(&client, &args.path)?)?;
            if json_output {
                print_json(json!({"ok": true, "command": "lock", "path": args.path}))?;
            } else {
                println!("Locked {}", args.path);
            }
        }
        Commands::Unlock(args) => {
            let client = Client::discover(".")?;
            client.unlock_remote(&repo_path(&client, &args.path)?)?;
            if json_output {
                print_json(json!({"ok": true, "command": "unlock", "path": args.path}))?;
            } else {
                println!("Unlocked {}", args.path);
            }
        }
        Commands::Copy(args) => {
            let client = Client::discover(".")?;
            client.copy_path(
                &repo_path(&client, &args.src)?,
                &repo_path(&client, &args.dst)?,
            )?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "copy",
                    "src": args.src,
                    "dst": args.dst,
                }))?;
            } else {
                println!("A  {} (from {})", args.dst, args.src);
            }
        }
        Commands::Move(args) => {
            let client = Client::discover(".")?;
            client.move_path(
                &repo_path(&client, &args.src)?,
                &repo_path(&client, &args.dst)?,
            )?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "move",
                    "src": args.src,
                    "dst": args.dst,
                }))?;
            } else {
                println!("A  {} (from {})", args.dst, args.src);
                println!("D  {}", args.src);
            }
        }
        Commands::Status => {
            let client = Client::discover(".")?;
            let status = client.status()?;
            let unversioned = client.unversioned()?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "status",
                    "clean": status.is_empty(),
                    "items": status,
                    "unversioned": unversioned,
                }))?;
            } else if status.is_empty() && unversioned.is_empty() {
                println!("Working copy clean");
            } else {
                for ch in status {
                    println!("{}", format_status_line(&ch));
                }
                for path in unversioned {
                    println!("?  {path}");
                }
            }
        }
        Commands::Staged => {
            let client = Client::discover(".")?;
            let items = client.staged_status()?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "staged",
                    "clean": items.is_empty(),
                    "items": items,
                }))?;
            } else if items.is_empty() {
                println!("No staged changes");
            } else {
                for ch in items {
                    println!("{}", format_status_line(&ch));
                }
            }
        }
        Commands::Diff(args) => {
            let client = Client::discover(".")?;
            let filter = match args.path.as_deref() {
                Some(p) => Some(resolve_user_path(&client, p)?).filter(|p| !p.is_empty()),
                None => None,
            };
            let peg = args
                .peg
                .as_deref()
                .map(|p| repo_peg(&client, p))
                .transpose()?;
            let patch = client.diff(filter.as_deref(), peg.as_deref())?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "diff",
                    "path": args.path,
                    "peg": args.peg,
                    "patch": patch,
                }))?;
            } else {
                print!("{patch}");
            }
        }
        Commands::Cat(args) => {
            let client = Client::discover(".")?;
            let bytes = client.cat_peg(&repo_peg(&client, &args.peg)?)?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "cat",
                    "peg": args.peg,
                    "encoding": "base64",
                    "content": base64::engine::general_purpose::STANDARD.encode(&bytes),
                }))?;
            } else {
                use std::io::Write;
                std::io::stdout().write_all(&bytes)?;
            }
        }
        Commands::Commit(args) => {
            let client = Client::discover(".")?;
            let result = if args.all {
                if args.push {
                    client.commit_and_push(&args.message, &args.author)
                } else {
                    client.commit(&args.message, &args.author)
                }
            } else {
                client.commit_staged(&args.message, &args.author, args.push)
            };
            let commit = match result {
                Err(VcsError::NothingToCommit) | Err(VcsError::NoStagedChanges) => {
                    let message = if args.all {
                        "No local changes to commit"
                    } else {
                        "No staged changes to commit (stage paths or use --all)"
                    };
                    if json_output {
                        print_json(json!({
                            "ok": true,
                            "command": "commit",
                            "noop": true,
                            "message": message,
                        }))?;
                    } else {
                        println!("{message}");
                    }
                    return Ok(());
                }
                other => other?,
            };
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "commit",
                    "commit": commit,
                    "pushed": args.push,
                    "mode": if args.all { "all" } else { "staged" },
                }))?;
            } else {
                println!(
                    "Committed revision r{} ({})",
                    commit.revision,
                    short_id(&commit.id)
                );
                println!("Changed files: {}", commit.changed_files.len());
                if args.push {
                    println!("Pushed to configured remote");
                }
            }
        }
        Commands::Log(args) => {
            let client = Client::discover(".")?;
            let mut commits = if let Some(range) = args.range {
                client.log_range(&range, args.verbose, args.include_merged, args.only_merged)?
            } else if args.include_merged || args.only_merged {
                client.log_range(
                    "1:HEAD",
                    args.verbose,
                    args.include_merged,
                    args.only_merged,
                )?
            } else {
                client.log(args.limit)?
            };
            if commits.len() > args.limit {
                commits.truncate(args.limit);
            }
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "log",
                    "items": commits,
                }))?;
            } else {
                for c in commits {
                    println!(
                        "r{} | {} | {}",
                        c.revision,
                        c.author,
                        c.created_at.to_rfc3339()
                    );
                    println!("\n{}\n", c.message);
                    if args.verbose {
                        if !c.changed_paths.is_empty() {
                            for cp in c.changed_paths {
                                let act = match cp.action {
                                    ChangedPathAction::Add => 'A',
                                    ChangedPathAction::Modify => 'M',
                                    ChangedPathAction::Delete => 'D',
                                    ChangedPathAction::Replace => 'R',
                                };
                                if let Some(src) = cp.copyfrom_path {
                                    println!(
                                        "   {} {} (from {}:{})",
                                        act,
                                        cp.path,
                                        src,
                                        cp.copyfrom_rev.unwrap_or(0)
                                    );
                                } else {
                                    println!("   {} {}", act, cp.path);
                                }
                            }
                        } else {
                            for ch in c.changed_files {
                                println!("   {}", format_status_line(&ch));
                            }
                        }
                        println!();
                    }
                }
            }
        }
        Commands::Revert(args) => {
            let client = Client::discover(".")?;
            let changed = client.revert(&repo_paths(&client, &args.paths)?)?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "revert",
                    "requested_paths": args.paths,
                    "reverted": changed,
                }))?;
            } else {
                println!("Reverted {} path(s)", changed.len());
            }
        }
        Commands::Blame(args) => {
            let client = Client::discover(".")?;
            let lines = client.blame(&repo_path(&client, &args.path)?, args.revision.as_deref())?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "blame",
                    "path": args.path,
                    "revision": args.revision,
                    "lines": lines,
                }))?;
            } else {
                for line in lines {
                    println!("{:>6} {:<12} {}", line.revision, line.author, line.content);
                }
            }
        }
        Commands::Update(args) => {
            let client = Client::discover(".")?;
            let depth = args.depth.as_deref().and_then(Depth::from_str);
            let changed = client.update_to_revision_with_depth(&args.revision, depth)?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "update",
                    "revision": args.revision,
                    "changed": changed,
                }))?;
            } else {
                println!("Updated to {}", args.revision);
                println!("Applied {} change(s)", changed.len());
                for (path, c) in client.conflicts()? {
                    println!(
                        "  C {path} ({})",
                        c.reason.as_deref().unwrap_or("text conflict")
                    );
                }
            }
        }
        Commands::Changed(args) => {
            let client = Client::discover(".")?;
            let changed_paths = client.changed_paths_in_revision(&args.revision)?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "changed",
                    "revision": args.revision,
                    "items": changed_paths,
                }))?;
            } else if !changed_paths.is_empty() {
                for cp in changed_paths {
                    let act = match cp.action {
                        ChangedPathAction::Add => 'A',
                        ChangedPathAction::Modify => 'M',
                        ChangedPathAction::Delete => 'D',
                        ChangedPathAction::Replace => 'R',
                    };
                    if let Some(src) = cp.copyfrom_path {
                        println!(
                            "{} {} (from {}:{})",
                            act,
                            cp.path,
                            src,
                            cp.copyfrom_rev.unwrap_or(0)
                        );
                    } else {
                        println!("{} {}", act, cp.path);
                    }
                }
            } else {
                let changed = client.changed_files_in_revision(&args.revision)?;
                for ch in changed {
                    println!("{}", format_status_line(&ch));
                }
            }
        }
        Commands::Merge(args) => {
            let client = Client::discover(".")?;
            let result = client.merge(&args.revision, args.dry_run, args.record_only)?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "merge",
                    "revision": args.revision,
                    "dry_run": args.dry_run,
                    "record_only": args.record_only,
                    "touched": result.changed,
                    "conflicts": result.conflicts,
                }))?;
            } else {
                println!("Merge touched {} path(s)", result.changed.len());
                if !result.conflicts.is_empty() {
                    println!("Conflicts:");
                    for c in result.conflicts {
                        println!("  C {}", c);
                    }
                }
            }
        }
        Commands::Stage(args) => {
            let client = Client::discover(".")?;
            let paths = repo_paths(&client, &args.paths)?;
            let staged = if paths.is_empty() && !args.paths.is_empty() {
                client.stage_all()?
            } else {
                client.stage_paths(&paths)?
            };
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "stage",
                    "staged": staged,
                }))?;
            } else {
                println!("Staged {} path(s)", staged.len());
            }
        }
        Commands::Unstage(args) => {
            let client = Client::discover(".")?;
            let paths = repo_paths(&client, &args.paths)?;
            let staged = if paths.is_empty() && !args.paths.is_empty() {
                client.clear_staging()?;
                Vec::new()
            } else {
                client.unstage_paths(&paths)?
            };
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "unstage",
                    "staged": staged,
                }))?;
            } else {
                println!("Remaining staged {} path(s)", staged.len());
            }
        }
        Commands::StageAll => {
            let client = Client::discover(".")?;
            let staged = client.stage_all()?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "stage-all",
                    "staged": staged,
                }))?;
            } else {
                println!("Staged all changes ({})", staged.len());
            }
        }
        Commands::StageClear => {
            let client = Client::discover(".")?;
            client.clear_staging()?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "stage-clear",
                }))?;
            } else {
                println!("Cleared staging index");
            }
        }
        Commands::Hunks(args) => {
            let client = Client::discover(".")?;
            let hunks = client.hunks(&repo_path(&client, &args.path)?)?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "hunks",
                    "path": args.path,
                    "items": hunks,
                }))?;
            } else if hunks.is_empty() {
                println!("No hunks");
            } else {
                for h in hunks {
                    let mark = if h.staged { "*" } else { " " };
                    println!(
                        "[{mark}] #{} @@ -{},{} +{},{} @@",
                        h.index, h.old_start, h.old_len, h.new_start, h.new_len
                    );
                }
            }
        }
        Commands::StageHunks(args) => {
            let client = Client::discover(".")?;
            let selected = client.stage_hunks(&repo_path(&client, &args.path)?, &args.indices)?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "stage-hunks",
                    "path": args.path,
                    "selected": selected,
                }))?;
            } else {
                println!("Staged hunks for {}: {:?}", args.path, selected);
            }
        }
        Commands::UnstageHunks(args) => {
            let client = Client::discover(".")?;
            let selected = client.unstage_hunks(&repo_path(&client, &args.path)?, &args.indices)?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "unstage-hunks",
                    "path": args.path,
                    "selected": selected,
                }))?;
            } else {
                println!("Remaining staged hunks for {}: {:?}", args.path, selected);
            }
        }
        Commands::PropSet(args) => {
            let client = Client::discover(".")?;
            client.set_property(&repo_path(&client, &args.path)?, &args.name, &args.value)?;
            if json_output {
                print_json(
                    json!({"ok": true, "command": "prop-set", "path": args.path, "name": args.name}),
                )?;
            } else {
                println!("Set property {} on {}", args.name, args.path);
            }
        }
        Commands::PropGet(args) => {
            let client = Client::discover(".")?;
            let value = client.get_property(&repo_path(&client, &args.path)?, &args.name)?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "prop-get",
                    "path": args.path,
                    "name": args.name,
                    "value": value,
                }))?;
            } else {
                match value {
                    Some(v) => println!("{v}"),
                    None => println!("(none)"),
                }
            }
        }
        Commands::PropDel(args) => {
            let client = Client::discover(".")?;
            client.del_property(&repo_path(&client, &args.path)?, &args.name)?;
            if json_output {
                print_json(
                    json!({"ok": true, "command": "prop-del", "path": args.path, "name": args.name}),
                )?;
            } else {
                println!("Deleted property {} on {}", args.name, args.path);
            }
        }
        Commands::IPropSet(args) => {
            let client = Client::discover(".")?;
            client.set_inherited_property(
                &resolve_user_path(&client, &args.scope)?,
                &args.name,
                &args.value,
            )?;
            if json_output {
                print_json(
                    json!({"ok": true, "command": "iprop-set", "scope": args.scope, "name": args.name}),
                )?;
            } else {
                println!("Set inherited property {} on {}", args.name, args.scope);
            }
        }
        Commands::IPropList => {
            let client = Client::discover(".")?;
            let items = client.list_inherited_properties()?;
            if json_output {
                print_json(json!({"ok": true, "command": "iprop-list", "items": items}))?;
            } else {
                for (scope, name, value) in items {
                    println!("{scope} {name}={value}");
                }
            }
        }
        Commands::Changelist(args) => {
            let client = Client::discover(".")?;
            match args.command {
                ChangelistCmd::Set { path, name } => {
                    client.set_changelist(&repo_path(&client, &path)?, Some(&name))?;
                    if json_output {
                        print_json(
                            json!({"ok": true, "command": "changelist-set", "path": path, "name": name}),
                        )?;
                    } else {
                        println!("Path {} -> changelist {}", path, name);
                    }
                }
                ChangelistCmd::Clear { path } => {
                    client.set_changelist(&repo_path(&client, &path)?, None)?;
                    if json_output {
                        print_json(
                            json!({"ok": true, "command": "changelist-clear", "path": path}),
                        )?;
                    } else {
                        println!("Cleared changelist for {}", path);
                    }
                }
                ChangelistCmd::List => {
                    let items = client.list_changelists()?;
                    if json_output {
                        print_json(
                            json!({"ok": true, "command": "changelist-list", "items": items}),
                        )?;
                    } else {
                        for (path, list) in items {
                            println!("{} {}", list, path);
                        }
                    }
                }
            }
        }
        Commands::Ignore(args) => {
            let client = Client::discover(".")?;
            match args.command {
                IgnoreCmd::Add { pattern } => {
                    client.add_ignore(&pattern)?;
                    if json_output {
                        print_json(
                            json!({"ok": true, "command": "ignore-add", "pattern": pattern}),
                        )?;
                    } else {
                        println!("Added ignore: {pattern}");
                    }
                }
                IgnoreCmd::List => {
                    let items = client.list_ignores()?;
                    if json_output {
                        print_json(json!({"ok": true, "command": "ignore-list", "items": items}))?;
                    } else {
                        for (scope, pattern, inherited) in items {
                            println!(
                                "{} {}{}",
                                if inherited { "I" } else { "L" },
                                scope,
                                if scope.is_empty() {
                                    format!(": {pattern}")
                                } else {
                                    format!("/{scope}: {pattern}")
                                }
                            );
                        }
                    }
                }
            }
        }
        Commands::Externals(args) => {
            let client = Client::discover(".")?;
            match args.command {
                ExternalsCmd::Set {
                    path,
                    target_url,
                    revision,
                } => {
                    client.set_external(&repo_path(&client, &path)?, &target_url, revision)?;
                    if json_output {
                        print_json(
                            json!({"ok": true, "command": "externals-set", "path": path, "target_url": target_url}),
                        )?;
                    } else {
                        println!("External set: {} -> {}", path, target_url);
                    }
                }
                ExternalsCmd::List => {
                    let items = client.list_externals()?;
                    if json_output {
                        let rows: Vec<_> = items
                            .iter()
                            .map(|e| {
                                json!({
                                    "path": e.path,
                                    "target_url": e.target_url,
                                    "revision": e.revision,
                                })
                            })
                            .collect();
                        print_json(
                            json!({"ok": true, "command": "externals-list", "items": rows}),
                        )?;
                    } else {
                        for e in items {
                            println!(
                                "{} -> {} {}",
                                e.path,
                                e.target_url,
                                e.revision.unwrap_or_else(|| "HEAD".to_owned())
                            );
                        }
                    }
                }
            }
        }
        Commands::Gc => {
            let client = Client::discover(".")?;
            let stats = client.gc()?;
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "gc",
                    "removed": stats.removed,
                    "kept": stats.kept,
                    "bytes_freed": stats.bytes_freed,
                }))?;
            } else {
                println!(
                    "Removed {} unreferenced blob(s), freed {} bytes ({} kept)",
                    stats.removed, stats.bytes_freed, stats.kept
                );
            }
        }
        #[cfg(feature = "serve-http")]
        Commands::Passwd(args) => {
            let mut password = String::new();
            std::io::stdin().read_line(&mut password)?;
            let password = password.trim_end_matches(['\r', '\n']);
            if password.is_empty() {
                return Err(VcsError::Protocol(
                    "empty password (pipe it on stdin, e.g. `read -s P; echo \"$P\" | vcrs passwd alice`)".to_owned(),
                ));
            }
            let client = Client::discover(&args.repo)?;
            let path = client.root().join(".vcrs").join("passwd.json");
            let mut doc: serde_json::Value = if path.exists() {
                serde_json::from_slice(&std::fs::read(&path)?)?
            } else {
                json!({"users": {}})
            };
            let hash = version_control_rs::auth::hash_password(password)?;
            doc["users"][&args.user] = serde_json::Value::String(hash);
            std::fs::write(&path, serde_json::to_vec_pretty(&doc)?)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            if json_output {
                print_json(json!({"ok": true, "command": "passwd", "user": args.user}))?;
            } else {
                println!("Password for '{}' stored in {}", args.user, path.display());
            }
        }
        #[cfg(feature = "serve-http")]
        Commands::ServeHttp(args) => {
            let bind = format!("{}:{}", args.host, args.port);
            if json_output {
                print_json(json!({
                    "ok": true,
                    "command": "serve-http",
                    "repo": args.repo.display().to_string(),
                    "bind": bind,
                    "note": "SVN/DAV compatibility is partial; write operations are scaffolded",
                }))?;
            } else {
                println!(
                    "Starting vcrs HTTP server at http://{} (repo: {})",
                    bind,
                    args.repo.display()
                );
                println!("SVN/DAV compatibility is partial (read/discovery/report foundation).");
            }
            let options = version_control_rs::svn_http::ServeOptions {
                enable_hooks: args.enable_hooks,
                allow_anonymous_write: args.allow_anonymous_write,
                allow_insecure_http: args.allow_insecure_http,
            };
            version_control_rs::svn_http::serve_http(args.repo, &bind, options)?;
        }
    }

    Ok(())
}

/// Resolve a path typed on the command line (relative to the current
/// directory, or absolute) to a validated repository-relative path. The
/// working-copy root itself resolves to an empty string.
fn resolve_user_path(client: &Client, input: &str) -> Result<String> {
    let cwd = std::fs::canonicalize(std::env::current_dir()?)?;
    version_control_rs::path::user_path_to_rel(client.root(), &cwd, input)
}

/// Like [`resolve_user_path`] but the path must name an entry below the root.
fn repo_path(client: &Client, input: &str) -> Result<String> {
    let rel = resolve_user_path(client, input)?;
    if rel.is_empty() {
        return Err(VcsError::InvalidPath {
            path: input.to_owned(),
            reason: "the working-copy root is not a file",
        });
    }
    Ok(rel)
}

/// Resolve a list of paths; naming the root selects everything (empty list).
fn repo_paths(client: &Client, inputs: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for input in inputs {
        let rel = resolve_user_path(client, input)?;
        if rel.is_empty() {
            return Ok(Vec::new());
        }
        out.push(rel);
    }
    Ok(out)
}

/// Resolve the path part of a `PATH@REV` peg specification.
fn repo_peg(client: &Client, spec: &str) -> Result<String> {
    match spec.rsplit_once('@') {
        Some((path, rev)) if !path.is_empty() => Ok(format!("{}@{rev}", repo_path(client, path)?)),
        _ => Ok(spec.to_owned()),
    }
}

fn print_json(value: serde_json::Value) -> Result<()> {
    let text = serde_json::to_string_pretty(&value).map_err(VcsError::Serde)?;
    println!("{text}");
    Ok(())
}

fn format_status_line(ch: &FileChange) -> String {
    let text = text_status_char(ch);
    let prop = if ch.props_modified { 'M' } else { ' ' };

    let mut trailer = String::new();
    if let Some(src) = &ch.moved_from {
        trailer.push_str(&format!(" (moved from {src})"));
    } else if let Some(src) = &ch.copy_from {
        trailer.push_str(&format!(" (copied from {src})"));
    }
    if let Some(dst) = &ch.moved_to {
        trailer.push_str(&format!(" (moved to {dst})"));
    }
    if ch.is_binary {
        trailer.push_str(" [binary]");
    }
    format!("{text}{prop} {}{trailer}", ch.path)
}

fn text_status_char(ch: &FileChange) -> char {
    if ch.conflicted {
        return 'C';
    }
    match ch.kind {
        ChangeKind::Added => 'A',
        ChangeKind::Deleted => 'D',
        ChangeKind::Modified => {
            if ch.text_modified {
                'M'
            } else {
                ' '
            }
        }
    }
}

fn short_id(id: &str) -> &str {
    let n = 12.min(id.len());
    &id[..n]
}
