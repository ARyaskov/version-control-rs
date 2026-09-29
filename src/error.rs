use std::io;

#[derive(thiserror::Error, Debug)]
pub enum VcsError {
    #[error("repository not found (expected .vcrs directory)")]
    RepositoryNotFound,
    #[error("commit '{0}' was not found")]
    CommitNotFound(String),
    #[error("blob '{0}' was not found in the object store")]
    BlobNotFound(String),
    #[error("revision '{0}' was not found")]
    RevisionNotFound(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("working copy is out of date: base r{base_rev}, head r{head_rev}")]
    OutOfDate { base_rev: i64, head_rev: i64 },
    #[error("working copy has local modifications; commit/revert before this operation")]
    WorkingCopyDirty,
    #[error("tree conflict at '{path}': {reason}")]
    TreeConflict { path: String, reason: String },
    #[error("unresolved conflict markers in: {paths}")]
    UnresolvedConflicts { paths: String },
    #[error("authorization denied for user '{user}' on '{path}' ({action})")]
    AuthzDenied {
        user: String,
        path: String,
        action: String,
    },
    #[error("lock conflict on '{path}', owned by '{owner}'")]
    LockConflict { path: String, owner: String },
    #[error("hook '{hook}' failed: {message}")]
    HookFailed { hook: String, message: String },
    #[error("path '{path}' requires lock (svn:needs-lock)")]
    NeedsLockRequired { path: String },
    #[error("invalid path outside repository: {0}")]
    PathOutsideRepository(String),
    #[error("path '{0}' does not exist")]
    PathNotFound(String),
    #[error("path '{0}' already exists")]
    PathExists(String),
    #[error("path '{0}' is not under version control")]
    NotVersioned(String),
    #[error("invalid repository path '{path}': {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error("server misconfiguration: {0}")]
    ServerMisconfigured(String),
    #[error("no staged changes to commit")]
    NoStagedChanges,
    #[error("hunk staging is not supported for path '{path}'")]
    HunkStagingUnsupported { path: String },
    #[error("hunk index {index} is out of range for path '{path}'")]
    InvalidHunkIndex { path: String, index: usize },
    #[error("failed to restore unstaged changes after staged commit: {0}")]
    RestoreFailed(String),
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("walkdir error: {0}")]
    Walkdir(#[from] walkdir::Error),
}

pub type Result<T> = std::result::Result<T, VcsError>;
