#[cfg(feature = "serve-http")]
pub mod auth;
pub mod client;
mod content;
pub mod diff;
pub mod error;
pub mod merge;
pub mod path;
pub mod ra;
pub mod repo;
#[cfg(feature = "serve-http")]
pub mod svn_http;
pub mod types;
pub mod wcdb;

pub use client::Client;
pub use error::{Result, VcsError};
pub use ra::{Capability, FileRaSession, RaSession, RemoteConfig};
pub use repo::{GcStats, MergeOutcome, PullOutcome, ResolveAccept, TreeEdits};
pub use types::{
    BlameLine, ChangeKind, ChangedPath, ChangedPathAction, Commit, Depth, DiffHunk, FileChange,
    FileEntry, RevisionRange,
};
pub use wcdb::{ConflictRecord, ExternalDef, PathLock};
