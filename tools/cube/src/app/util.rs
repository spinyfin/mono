//! Small shared helpers: lease-holder identity, lock paths, and the clock.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::paths;
use crate::store::{Store, WorkspaceListFilter};

use crate::app::errors::{CubeError, Result};

pub(super) fn holder_identity() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
    format!("{user}@{host}:{}", std::process::id())
}

pub(super) fn resolve_release_lease(
    store: &mut Store,
    workspace: Option<String>,
    lease: Option<String>,
    repo: Option<String>,
) -> Result<String> {
    if let Some(lease) = lease {
        return Ok(lease);
    }
    let workspace_id = workspace.ok_or_else(|| {
        CubeError::InvalidArgument("release requires a workspace id positional or --lease".to_string())
    })?;
    let matches = store.list_workspaces_filtered(&WorkspaceListFilter {
        repo: repo.as_deref(),
        workspace_id: Some(&workspace_id),
        ..Default::default()
    })?;
    match matches.as_slice() {
        [] => Err(CubeError::WorkspaceNotFound(workspace_id)),
        [single] => single.lease_id.clone().ok_or_else(|| {
            CubeError::InvalidArgument(format!(
                "workspace `{}/{}` is not currently leased",
                single.repo, single.workspace_id
            ))
        }),
        many => {
            let repos = many.iter().map(|r| r.repo.as_str()).collect::<Vec<_>>().join(", ");
            Err(CubeError::InvalidArgument(format!(
                "workspace id `{workspace_id}` matches multiple repos ({repos}); disambiguate with --repo"
            )))
        }
    }
}

pub(super) fn repo_lock_path(repo: &str, database_path: Option<&Path>) -> Result<PathBuf> {
    match database_path.and_then(Path::parent) {
        Some(parent) => Ok(paths::repo_lock_path_in(parent, repo)),
        None => paths::repo_lock_path(repo),
    }
}

pub(super) fn current_epoch_s() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| CubeError::Io(io::Error::other(e)))?
        .as_secs() as i64)
}
