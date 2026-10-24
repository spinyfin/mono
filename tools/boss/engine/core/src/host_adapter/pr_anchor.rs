//! A PR alias is disposable Cube state, not a durable recovery pointer.
use super::*;

pub(super) async fn reconcile(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    pr_bookmark: Option<&str>,
    trunk: &str,
) -> Result<Option<execution_bookmark::Reconciliation>> {
    reconcile_with_gh(jj, record, pr_bookmark, trunk, Path::new("gh")).await
}

async fn reconcile_with_gh(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    pr_bookmark: Option<&str>,
    trunk: &str,
    gh: &Path,
) -> Result<Option<execution_bookmark::Reconciliation>> {
    use execution_bookmark::{PointerIntegrityKind, pointer_integrity_kind};
    let initial = execution_bookmark::reconcile(jj, record, pr_bookmark, trunk).await;
    if !initial
        .as_ref()
        .is_err_and(|err| pointer_integrity_kind(err) == Some(PointerIntegrityKind::PrHeadUnavailable))
    {
        return initial;
    }
    let bookmark = pr_bookmark.ok_or_else(|| anyhow::anyhow!("missing bound PR"))?;
    let pr: u64 = bookmark
        .strip_prefix("pr/")
        .ok_or_else(|| anyhow::anyhow!("invalid PR anchor {bookmark}"))?
        .parse()?;
    repair_with_gh(jj, record, pr, gh).await?;
    execution_bookmark::reconcile(jj, record, pr_bookmark, trunk).await
}

pub(super) async fn ensure(jj: &dyn Jj, record: &ExecutionBookmark, pr: u64) -> Result<()> {
    match execution_bookmark::resolve_bookmark_commit(jj, record, &format!("pr/{pr}")).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => repair(jj, record, pr).await,
        Err(err) if execution_bookmark::is_pointer_integrity_error(&err) => repair(jj, record, pr).await,
        Err(err) => Err(err),
    }
}

pub(super) async fn restore_rebased(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    pr_bookmark: Option<&str>,
    base_branch: &str,
    upstream: &str,
) -> Result<execution_bookmark::RestoreReport> {
    restore_with_gh(
        jj,
        record,
        workspace,
        pr_bookmark,
        base_branch,
        upstream,
        Path::new("gh"),
    )
    .await
}

async fn restore_with_gh(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    pr_bookmark: Option<&str>,
    base_branch: &str,
    upstream: &str,
    gh: &Path,
) -> Result<execution_bookmark::RestoreReport> {
    let initial = execution_bookmark::restore_rebased(jj, record, workspace, pr_bookmark, base_branch, upstream).await;
    if !initial.as_ref().is_err_and(|err| {
        execution_bookmark::pointer_integrity_kind(err)
            == Some(execution_bookmark::PointerIntegrityKind::PrHeadUnavailable)
    }) {
        return initial;
    }
    let bookmark = pr_bookmark.ok_or_else(|| anyhow::anyhow!("missing bound PR"))?;
    let pr = bookmark
        .strip_prefix("pr/")
        .ok_or_else(|| anyhow::anyhow!("invalid PR anchor {bookmark}"))?
        .parse()?;
    repair_with_gh(jj, record, pr, gh).await?;
    execution_bookmark::restore_rebased(jj, record, workspace, pr_bookmark, base_branch, upstream).await
}

async fn repair(jj: &dyn Jj, record: &ExecutionBookmark, pr: u64) -> Result<()> {
    repair_with_gh(jj, record, pr, Path::new("gh")).await
}

async fn repair_with_gh(jj: &dyn Jj, record: &ExecutionBookmark, pr: u64, gh: &Path) -> Result<()> {
    let bookmark = format!("pr/{pr}");
    // One repair, never a retry loop. The host's jj runner imports the head;
    // existing gh authentication supplies the authoritative GitHub identity.
    let metadata = async {
        let remotes = jj.run(&record.repo_path, &["git", "remote", "list"]).await?;
        let (remote, slug) = git_utils::repo_slug::parse_github_remote(&remotes)
            .ok_or_else(|| anyhow::anyhow!("recovery store has no GitHub remote"))?;
        let (head_ref, head_sha) = git_utils::gh_cli::fetch_pr_head(gh, &slug, pr).await.map_err(|err| {
            if err.is::<git_utils::gh_cli::PrHeadUnavailable>() {
                execution_bookmark::pointer_integrity_error(format!("GitHub head for {bookmark} is unavailable"))
            } else {
                err
            }
        })?;
        Ok::<_, anyhow::Error>((remote, head_ref, head_sha))
    };
    let (remote, head_ref, head_sha) = tokio::time::timeout(std::time::Duration::from_secs(120), metadata)
        .await
        .map_err(|_| anyhow::anyhow!("repair of {bookmark} from GitHub timed out"))?
        .with_context(|| format!("cannot repair {bookmark} from GitHub"))?;
    // The jj runner bounds each command. Do not cancel the fetch/restore
    // sequence as a whole: even a failed fetch must restore saved pointers.
    execution_bookmark::repair_pr_anchor(jj, record, pr, &remote, &head_ref, &head_sha)
        .await
        .with_context(|| format!("cannot import GitHub head for {bookmark}"))?;
    Ok(())
}

#[cfg(test)]
#[path = "pr_anchor_tests.rs"]
mod tests;
