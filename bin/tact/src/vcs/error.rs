use std::path::PathBuf;
use thiserror::Error;

/// A failure to read a checkout or capture its changes.
#[derive(Debug, Error)]
pub enum VcsError {
    #[error("the repository is managed by jj, which is not installed or not on the PATH")]
    JjMissing,
    #[error("jj failed: {0}")]
    Jj(String),
    #[error("could not create scratch space for the review: {0}")]
    Scratch(#[source] std::io::Error),
    #[error("failed to start git: {0}")]
    StartGit(#[source] std::io::Error),
    #[error("failed to read git output: {0}")]
    ReadGit(#[source] std::io::Error),
    #[error("failed to wait for git: {0}")]
    WaitGit(#[source] std::io::Error),
    #[error("git output task failed: {0}")]
    GitOutputTask(#[source] tokio::task::JoinError),
    /// Git ran and exited unsuccessfully; the payload is its trimmed standard error.
    #[error("git command failed: {0}")]
    GitFailed(String),
    #[error("review workspace is not in a Git repository: {0}")]
    NotRepository(PathBuf),
    #[error("could not determine the branch base; configure an upstream for the current branch")]
    BaseNotFound,
    #[error("could not find a merge base between HEAD and `{0}`")]
    InvalidBase(String),
    #[error("the selected review range {from}..{to} is invalid for {target_count} targets")]
    InvalidRange {
        from: usize,
        to: usize,
        target_count: usize,
    },
    #[error("git returned invalid commit metadata for the review range")]
    InvalidCommitMetadata,
    /// The working tree differed between two consecutive captures, so no consistent snapshot
    /// could be taken.
    #[error("workspace kept changing while the review snapshot was collected")]
    WorkspaceChangedDuringSnapshot,
    #[error("review diff is {actual} bytes, exceeding the {maximum}-byte limit")]
    TooLarge { actual: usize, maximum: usize },
    #[error("git output was not valid UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("git returned a path that is not valid UTF-8: {0}")]
    PathUtf8(#[from] std::str::Utf8Error),
}
