//! A PR alias is disposable Cube state, not a durable recovery pointer.
use super::*;

/// Upper bound on one alias repair, covering Cube's GitHub lookup, any head
/// fetch and the guarded write. Cube bounds each of its own network commands.
const REPAIR_TIMEOUT: Duration = Duration::from_secs(300);

/// Rewrites `pr/<n>` from GitHub's current head. Cube owns that alias, so the
/// write goes through Cube's guarded writer (store-wide PR-bookmark lock that
/// also serializes workspace GC, the refusal to override a conflicted side
/// holding unique content, git import/export, fork-head fetches) rather than
/// a second engine-side writer.
#[async_trait]
pub(super) trait PrAliasRepair: Send + Sync {
    async fn repair(&self, store: &Path, pr: u64) -> Result<()>;
}

/// Turn Cube's deterministic refusals into definitive integrity failures that
/// block for a human; everything else (network, GitHub, timeouts) stays an
/// ordinary retryable failure.
fn classify_repair_error(err: anyhow::Error) -> anyhow::Error {
    use git_utils::pr_bookmark::{NO_PR_HEAD_PHRASE, UNSAFE_REPAIR_PHRASE};
    let detail = format!("{err:#}");
    if detail.contains(UNSAFE_REPAIR_PHRASE) || detail.contains(NO_PR_HEAD_PHRASE) {
        execution_bookmark::pointer_integrity_error(format!("cube refused to repair the PR alias: {detail}"))
    } else {
        err
    }
}

#[async_trait]
impl PrAliasRepair for LocalHostAdapter {
    async fn repair(&self, store: &Path, pr: u64) -> Result<()> {
        self.cube_client
            .repair_pr_bookmark(store, pr)
            .await
            .map_err(classify_repair_error)
    }
}

#[async_trait]
impl PrAliasRepair for SshHostAdapter {
    async fn repair(&self, store: &Path, pr: u64) -> Result<()> {
        crate::cube_commands::repair_pr_bookmark(self, store, pr, crate::cube_op_budget::REPAIR_PR_BOOKMARK)
            .await
            .map_err(classify_repair_error)
    }
}

pub(super) async fn reconcile(
    jj: &dyn Jj,
    repair: &dyn PrAliasRepair,
    record: &ExecutionBookmark,
    pr_bookmark: Option<&str>,
    trunk: &str,
) -> Result<Option<execution_bookmark::Reconciliation>> {
    use execution_bookmark::{PointerIntegrityKind, pointer_integrity_kind};
    let initial = execution_bookmark::reconcile(jj, record, pr_bookmark, trunk).await;
    if !initial
        .as_ref()
        .is_err_and(|err| pointer_integrity_kind(err) == Some(PointerIntegrityKind::PrHeadUnavailable))
    {
        return initial;
    }
    let pr = bound_pr(pr_bookmark)?;
    repair_with(jj, repair, record, pr, REPAIR_TIMEOUT).await?;
    after_repair(execution_bookmark::reconcile(jj, record, pr_bookmark, trunk).await, pr)
}

pub(super) async fn ensure(jj: &dyn Jj, repair: &dyn PrAliasRepair, record: &ExecutionBookmark, pr: u64) -> Result<()> {
    match execution_bookmark::resolve_bookmark_commit(jj, record, &format!("pr/{pr}")).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => repair_with(jj, repair, record, pr, REPAIR_TIMEOUT).await,
        Err(err) if execution_bookmark::is_pointer_integrity_error(&err) => {
            repair_with(jj, repair, record, pr, REPAIR_TIMEOUT).await
        }
        Err(err) => Err(err),
    }
}

pub(super) async fn restore_rebased(
    jj: &dyn Jj,
    repair: &dyn PrAliasRepair,
    record: &ExecutionBookmark,
    workspace: &Path,
    pr_bookmark: Option<&str>,
    base_branch: &str,
    upstream: &str,
) -> Result<execution_bookmark::RestoreReport> {
    let initial = execution_bookmark::restore_rebased(jj, record, workspace, pr_bookmark, base_branch, upstream).await;
    if !initial.as_ref().is_err_and(|err| {
        execution_bookmark::pointer_integrity_kind(err)
            == Some(execution_bookmark::PointerIntegrityKind::PrHeadUnavailable)
    }) {
        return initial;
    }
    let pr = bound_pr(pr_bookmark)?;
    repair_with(jj, repair, record, pr, REPAIR_TIMEOUT).await?;
    after_repair(
        execution_bookmark::restore_rebased(jj, record, workspace, pr_bookmark, base_branch, upstream).await,
        pr,
    )
}

fn bound_pr(pr_bookmark: Option<&str>) -> Result<u64> {
    let bookmark = pr_bookmark.ok_or_else(|| anyhow::anyhow!("missing bound PR"))?;
    Ok(bookmark
        .strip_prefix("pr/")
        .ok_or_else(|| anyhow::anyhow!("invalid PR anchor {bookmark}"))?
        .parse()?)
}

fn after_repair<T>(result: Result<T>, pr: u64) -> Result<T> {
    result.map_err(|err| {
        if execution_bookmark::pointer_integrity_kind(&err)
            == Some(execution_bookmark::PointerIntegrityKind::PrHeadUnavailable)
        {
            execution_bookmark::pointer_integrity_error(format!(
                "pr/{pr} was repaired from GitHub but was removed or conflicted again"
            ))
        } else {
            err
        }
    })
}

/// One repair, never a retry loop. Cube may fetch while repairing, which can
/// move the execution's publication pointers, so they are restored whether or
/// not the repair succeeded or timed out. Cube bounds its own repair well
/// inside `timeout` (see [`crate::cube_op_budget::REPAIR_PR_BOOKMARK`]), so
/// `timeout` is only a backstop and has normally stopped Cube before it fires.
async fn repair_with(
    jj: &dyn Jj,
    repair: &dyn PrAliasRepair,
    record: &ExecutionBookmark,
    pr: u64,
    timeout: Duration,
) -> Result<()> {
    let saved = execution_bookmark::snapshot_pointers(jj, record).await?;
    let repaired = tokio::time::timeout(timeout, repair.repair(&record.repo_path, pr)).await;
    execution_bookmark::restore_pointers(jj, record, &saved).await?;
    repaired
        .map_err(|_| anyhow::anyhow!("repair of pr/{pr} from GitHub timed out"))?
        .with_context(|| format!("cannot repair pr/{pr} from GitHub"))
}

#[cfg(test)]
#[path = "pr_anchor_tests.rs"]
mod tests;
