use super::execution::GotoTarget;
use super::*;

pub(super) struct FreshStart<'a> {
    pub repo_id: &'a str,
    pub immutable_target: Option<&'a str>,
    pub bound_pr: Option<u64>,
    pub task_pr: Option<u64>,
    pub title: &'a str,
}

/// Only an explicitly bound PR bypasses the optional task PR's open-state check.
async fn fresh_start_target<'a>(adapter: &dyn HostAdapter, start: &FreshStart<'a>) -> Option<GotoTarget<'a>> {
    if let Some(sha) = start.immutable_target {
        return Some(GotoTarget::Revision(sha));
    }
    if let Some(pr) = start.bound_pr {
        return Some(GotoTarget::Pr(pr));
    }
    if let Some(pr) = start.task_pr {
        match crate::host_adapter::recovery_pr_is_open(adapter, start.repo_id, pr).await {
            Ok(true) => return Some(GotoTarget::Pr(pr)),
            Ok(false) => {}
            Err(err) => tracing::warn!(?err, pr, "optional recovery PR is unavailable; starting from trunk"),
        }
    }
    None
}

pub(super) async fn position_fresh_workspace(
    adapter: &dyn HostAdapter,
    workspace: &Path,
    start: &FreshStart<'_>,
) -> Result<()> {
    match fresh_start_target(adapter, start).await {
        Some(GotoTarget::Revision(sha)) => adapter.goto_workspace_revision(workspace, sha).await,
        Some(GotoTarget::Pr(pr)) => adapter.goto_workspace(workspace, pr).await,
        None => adapter.start_fresh_workspace(workspace).await,
    }
}
