//! The crate's error type (`CubeError`), its exit-code mapping, and the
//! `RunResult` envelope every command returns.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

pub(super) type Result<T> = std::result::Result<T, CubeError>;

#[derive(Debug, Clone)]
pub struct RunResult {
    pub message: String,
    pub payload: Value,
}

impl RunResult {
    pub(super) fn new(message: impl Into<String>, payload: impl Serialize) -> Result<Self> {
        Ok(Self {
            message: message.into(),
            payload: serde_json::to_value(payload)?,
        })
    }
}

#[derive(Debug, Error)]
pub enum CubeError {
    #[error("{0}")]
    InvalidArgument(String),
    #[error("{0}")]
    NotImplemented(String),
    #[error("repo `{0}` is not configured")]
    RepoNotFound(String),
    #[error("no free workspace is available for repo `{0}`")]
    NoAvailableWorkspace(String),
    #[error("workspace `{0}` is not tracked")]
    WorkspaceNotFound(String),
    #[error("lease `{0}` is not tracked")]
    LeaseNotFound(String),
    #[error("change `{0}` is not tracked")]
    ChangeNotFound(String),
    #[error("setup step `{step}` failed: {error}")]
    SetupStepFailed { step: String, error: String },
    #[error("failed to access Cube metadata: {0}")]
    Storage(#[source] rusqlite::Error),
    #[error("failed to create workspace directory `{path}`: {source}")]
    WorkspaceDirCreate { path: PathBuf, source: io::Error },
    #[error("failed to read workspace directory `{path}`: {source}")]
    WorkspaceDirRead { path: PathBuf, source: io::Error },
    #[error("failed to remove workspace directory `{path}`: {source}")]
    WorkspaceDirRemove { path: PathBuf, source: io::Error },
    #[error("failed to create repo source directory `{path}`: {source}")]
    RepoSourceDirCreate { path: PathBuf, source: io::Error },
    #[error("failed to open state database at `{path}`: {source}")]
    StateDbIo { path: PathBuf, source: io::Error },
    #[error("failed to write audit log entry at `{path}`: {source}")]
    AuditLogIo { path: PathBuf, source: io::Error },
    #[error("failed to acquire repo lock at `{path}`: {source}")]
    LockIo { path: PathBuf, source: io::Error },
    #[error("I/O error: {0}")]
    Io(#[source] io::Error),
    #[error(
        "command `{program} {}` failed{}{}",
        args.join(" "),
        status
            .map(|code| format!(" with exit code {code}"))
            .unwrap_or_default(),
        if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        }
    )]
    CommandFailed {
        program: String,
        args: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    #[error(
        "command `{program} {}` did not complete within {timeout_secs}s and was killed",
        args.join(" ")
    )]
    CommandTimedOut {
        program: String,
        args: Vec<String>,
        timeout_secs: u64,
    },
    #[error("failed to serialize output: {0}")]
    Json(#[from] serde_json::Error),
    #[error("workspace `{workspace_path}` is stale and could not be auto-recovered: {cause}")]
    StaleRecoveryFailed { workspace_path: PathBuf, cause: String },
    /// The lease handler tried to reclaim a workspace whose previous
    /// lease had expired (so cube flipped it back to `free`), but the
    /// workspace's `@` still has the prior holder's uncommitted /
    /// non-main work. A destructive `jj new <main>` would have silently
    /// destroyed it — most likely from underneath a worker whose lease
    /// expired but who is still active. Surface this loudly instead.
    /// Operators recover with `cube workspace force-release` after
    /// confirming the prior worker is genuinely gone.
    #[error(
        "workspace `{workspace_path}` was reclaimed from an expired lease (prior holder: {prior_holder}, \
         lease: {prior_lease_id}) but its working copy still has uncommitted work; refusing to \
         destructively reset it. Use `cube workspace force-release --lease {prior_lease_id}` to \
         acknowledge data loss and re-attempt the lease."
    )]
    LeaseExpiredWorkspaceDirty {
        workspace_path: PathBuf,
        prior_lease_id: String,
        prior_holder: String,
    },
}

/// Stable substring jj prints when a working copy is stale relative to
/// the shared op log. Verified against the version pinned in
/// `tools/jj/` — the wording has been stable across releases.
pub(super) const JJ_STALE_SIGNATURE: &str = "working copy is stale";

/// Alternate stale signature emitted by newer jj versions when the op-log
/// entry for the working copy cannot be read at all (distinct from "the copy
/// IS stale" phrasing). Also fixed by `jj workspace update-stale`.
/// Seen in the wild as: "Could not read working copy's operation. Hint: Run
/// jj workspace update-stale to recover."
pub(super) const JJ_STALE_OP_SIGNATURE: &str = "could not read working copy's operation";

/// Stable substring jj prints when the repo was loaded at an operation
/// that is a sibling of the working copy's operation (op-log divergence).
/// Both the stale-working-copy and op-log-diverged cases are fixed by
/// `jj workspace update-stale`. The wording has been stable across releases.
pub(super) const JJ_OP_DIVERGED_SIGNATURE: &str = "seems to be a sibling";

/// Stable substring jj prints when a jj repo does not exist in the
/// current directory. If a `.git/` directory is present alongside the
/// missing `.jj/`, `jj git init --colocate` can recover the workspace.
pub(super) const JJ_NO_JJ_REPO_SIGNATURE: &str = "there is no jj repo";

/// Stable substring jj prints from `jj bookmark track <name>@<remote>`
/// when the named remote bookmark does not exist in the repo (e.g.
/// asking it to track `main@origin` in a repo that uses `master`). Lets
/// cube swallow this specific failure during the post-clone "promote
/// the default branch" step without papering over other jj errors.
pub(super) const JJ_NO_REMOTE_BOOKMARK_SIGNATURE: &str = "no such remote bookmark";

/// Stable substring jj prints when a revset references a revision that
/// does not exist — e.g. `jj bookmark set master -r master@origin` in a
/// workspace whose recorded default branch has no matching `@origin`
/// remote bookmark. Lets cube tolerate a misconfigured default branch
/// during the on-lease fast-forward without bricking the lease.
pub(super) const JJ_REVISION_DOESNT_EXIST_SIGNATURE: &str = "doesn't exist";

impl CubeError {
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::InvalidArgument(_) | Self::NotImplemented(_) => ExitCode::from(2),
            Self::RepoNotFound(_) => ExitCode::from(3),
            Self::NoAvailableWorkspace(_) => ExitCode::from(4),
            Self::WorkspaceNotFound(_) | Self::LeaseNotFound(_) | Self::ChangeNotFound(_) => ExitCode::from(5),
            Self::SetupStepFailed { .. } => ExitCode::from(6),
            Self::Storage(_)
            | Self::Io(_)
            | Self::WorkspaceDirCreate { .. }
            | Self::WorkspaceDirRead { .. }
            | Self::WorkspaceDirRemove { .. }
            | Self::RepoSourceDirCreate { .. }
            | Self::StateDbIo { .. }
            | Self::AuditLogIo { .. }
            | Self::LockIo { .. }
            | Self::CommandFailed { .. }
            | Self::CommandTimedOut { .. }
            | Self::Json(_)
            | Self::StaleRecoveryFailed { .. } => ExitCode::FAILURE,
            // Surfaced as its own exit code so the engine's heartbeat
            // failure path can detect "I lost a lease and the workspace
            // still has work" specifically and surface it as a
            // `WorkAttentionItem` rather than a generic lease failure.
            Self::LeaseExpiredWorkspaceDirty { .. } => ExitCode::from(7),
        }
    }
}
