use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CubeRepoHandle {
    pub repo_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CubeWorkspaceLease {
    pub lease_id: String,
    pub workspace_id: String,
    pub workspace_path: PathBuf,
    /// Cube's positive recovery check, reported only on an `--allow-dirty`
    /// reclaim:
    ///
    /// - `Some(true)`  — the workspace still held work that exists on no
    ///   remote, i.e. cube recovered the dead worker's tree in place. This is
    ///   the good path: the work is live, with its jj operation log intact.
    /// - `Some(false)` — the lease succeeded but there was nothing left to
    ///   recover (the tree had already been reset). The caller must fall back
    ///   to the engine's saved recovery patch.
    /// - `None`        — not a recovery lease; the field does not apply.
    ///
    /// The distinction matters because lease success alone never proved
    /// recovery: a caller that inferred it would start from an empty tree
    /// while believing it was resuming.
    pub dirty_verified: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CubeChangeHandle {
    pub change_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder, serde::Serialize, serde::Deserialize)]
#[builder(on(String, into))]
pub struct CubeWorkspaceStatus {
    pub workspace_id: String,
    pub workspace_path: PathBuf,
    pub state: String,
    pub lease_id: Option<String>,
    pub holder: Option<String>,
    pub task: Option<String>,
    /// Exact task identity recorded by Cube when the previous lease ended.
    #[serde(default)]
    pub last_task: Option<String>,
    pub leased_at_epoch_s: Option<i64>,
    pub lease_expires_at_epoch_s: Option<i64>,
}

/// Pool-config view of a repo as returned by `cube repo list --json`.
/// Supplies pool defaults to the hub scheduler and registered paths to the executor.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder, serde::Serialize, serde::Deserialize)]
#[builder(on(String, into))]
pub struct CubeRepoSummary {
    #[serde(rename = "repo")]
    pub repo_id: String,
    pub origin: String,
    pub main_branch: String,
    pub workspace_root: PathBuf,
    pub workspace_prefix: String,
    #[serde(default)]
    pub source: Option<PathBuf>,
}

/// Registry ownership used by the host executor; the hub's legacy status view
/// remains the same flattened status type.
#[derive(serde::Deserialize)]
pub struct RegisteredWorkspace {
    #[serde(default)]
    pub repo: String,
    #[serde(flatten)]
    pub status: CubeWorkspaceStatus,
}
