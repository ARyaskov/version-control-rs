pub mod client;
pub mod diff;
pub mod error;
pub mod merge;
pub mod ra;
pub mod repo;
pub mod svn_http;
pub mod types;
pub mod wcdb;

pub use client::Client;
pub use error::{Result, VcsError};
pub use ra::{Capability, FileRaSession, RaSession, RemoteConfig, WireRequest, WireResponse};
pub use repo::MergeOutcome;
pub use types::{
    BlameLine, ChangeKind, ChangedPath, ChangedPathAction, Commit, Depth, DiffHunk, FileChange,
    FileEntry, RevisionRange, TxnRecord,
};
pub use wcdb::ExternalDef;
