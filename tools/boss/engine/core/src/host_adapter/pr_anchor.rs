//! A PR alias is disposable Cube state, not a durable recovery pointer.
use super::*;

pub(super) async fn reconcile(
    jj: &dyn Jj,
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
    let bookmark = pr_bookmark.ok_or_else(|| anyhow::anyhow!("missing bound PR"))?;
    let pr: u64 = bookmark
        .strip_prefix("pr/")
        .ok_or_else(|| anyhow::anyhow!("invalid PR anchor {bookmark}"))?
        .parse()?;
    repair(jj, record, pr).await?;
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

async fn repair(jj: &dyn Jj, record: &ExecutionBookmark, pr: u64) -> Result<()> {
    let bookmark = format!("pr/{pr}");
    // One repair, never a retry loop. The host's jj runner imports the head;
    // existing gh authentication supplies the authoritative GitHub identity.
    let metadata = async {
        let remotes = jj.run(&record.repo_path, &["git", "remote", "list"]).await?;
        let (remote, slug) = git_utils::repo_slug::parse_github_remote(&remotes)
            .ok_or_else(|| anyhow::anyhow!("recovery store has no GitHub remote"))?;
        let head_ref = git_utils::gh_cli::fetch_pr_head_ref(&slug, pr).await?;
        let head_sha = git_utils::gh_cli::fetch_pr_head_oid_fresh(&slug, pr).await?;
        Ok::<_, anyhow::Error>((remote, head_ref, head_sha))
    };
    let (remote, head_ref, head_sha) = tokio::time::timeout(std::time::Duration::from_secs(120), metadata)
        .await
        .map_err(|_| {
            execution_bookmark::pointer_integrity_error(format!(
                "repair of {bookmark} from GitHub timed out; revision chain needs operator attention"
            ))
        })?
        .map_err(|err| {
            execution_bookmark::pointer_integrity_error(format!(
                "cannot repair {bookmark} from GitHub: {err:#}; revision chain needs operator attention"
            ))
        })?;
    // The jj runner bounds each command. Do not cancel the fetch/restore
    // sequence as a whole: even a failed fetch must restore saved pointers.
    execution_bookmark::repair_pr_anchor(jj, record, pr, &remote, &head_ref, &head_sha)
        .await
        .map_err(|err| {
            execution_bookmark::pointer_integrity_error(format!("cannot import GitHub head for {bookmark}: {err:#}"))
        })?;
    Ok(())
}
