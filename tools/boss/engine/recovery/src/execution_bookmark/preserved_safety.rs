//! Apply ownership checks uniformly to journal and forgotten-workspace tips.
use super::*;

pub(super) async fn finalize_tip(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    workspace: &Path,
    rev: &str,
    tip: &str,
    ownership: Option<BoundaryOwnership>,
    snapshot: Option<&Path>,
) -> Result<WorkspaceTip> {
    let mut resolved = visible_historical_tip(jj, &record.repo_path, tip, ownership.is_none()).await?;
    if let Some(workspace) = snapshot {
        resolved = snapshot_free_unproven(jj, &record.repo_path, workspace, resolved).await?;
    }
    // The live registered tip leads: it is what a human inspects first.
    let mut unclaimed = Vec::new();
    let mut unsafe_commits = Vec::new();
    if let Some(rewrite) = resolved.rewritten_to.as_ref() {
        unsafe_commits.push(rewrite.clone());
    }
    if resolved.snapshot_hid.is_none()
        && ownership == Some(BoundaryOwnership::FreeUnproven)
        && let Some(extra) = unclaimed_registered_work(jj, &record.repo_path, rev, resolved.commit.as_deref()).await?
    {
        unclaimed.push(extra);
    }
    if snapshot.is_none()
        && ownership.is_some()
        && jj.workspace_exists(workspace).await?
        && let Some(commit) = resolved.commit.clone()
        && pending_working_copy_boundary(jj, &record.repo_path, rev, &commit).await?
    {
        // Not snapshotted: the boundary commit may still be the working-copy
        // commit of a later holder whose edits are pending on disk. jj moves
        // a bookmark along with the rewrite its next command snapshots, so
        // a bookmark here would end up on those edits.
        let work = log(jj, &record.repo_path, &unpublished(&commit), "change_id ++ \"\\n\"").await?;
        if !work.trim().is_empty() {
            unsafe_commits.push(commit.clone());
            unclaimed.push(Unclaimed {
                note: format!(
                    "the lease boundary tip {commit} is still the workspace's working-copy commit, which may now hold a later holder's pending edits; excluding it from automatic restore and retaining it only in an inspection archive"
                ),
                count: work.lines().count() as u64,
                commit,
            });
            resolved.commit = None;
        }
    }
    if let Some(hidden) = resolved.snapshot_hid.as_deref() {
        let visible = resolved.rewritten_to.as_deref().unwrap_or(hidden);
        unsafe_commits.push(visible.to_owned());
        let work = log(jj, &record.repo_path, &unpublished(visible), "change_id ++ \"\n\"").await?;
        let count = work.lines().count() as u64;
        unclaimed.push(Unclaimed {
                note: format!(
                    "the lease boundary tip {hidden} was rewritten to {visible} by snapshotting a working copy in its ancestry; the successor holds post-boundary edits of unproven ownership, so neither commit is restorable; the successor is retained only in an inspection archive and the boundary commit stays recoverable by id"
                ),
                commit: visible.to_owned(),
                count,
            });
    } else if let Some(rewrite) = resolved.rewritten_to
        && !unclaimed.iter().any(|seen| seen.commit == rewrite)
    {
        let work = log(jj, &record.repo_path, &unpublished(&rewrite), "change_id ++ \"\n\"").await?;
        let count = work.lines().count() as u64;
        if count > 0 {
            unclaimed.push(Unclaimed {
                    note: format!(
                        "the lease boundary tip {tip} was rewritten to {rewrite} ({count} unpublished change(s)) afterwards; retaining it only in an inspection archive because the rewrite may belong to a later holder"
                    ),
                    commit: rewrite,
                    count,
                });
        }
    }
    Ok(WorkspaceTip {
        commit: resolved.commit,
        unclaimed,
        terminal_inspect: None,
        unsafe_commits,
    })
}

/// Keep rejected work in inspection archives, separate from restorable work.
/// Freeze the rejected identity in the name: the archive target can follow
/// rewrites or move to a parent on abandonment without changing the verdict.
pub(super) async fn quarantine(jj: &dyn Jj, record: &ExecutionBookmark, commits: &[String]) -> Result<()> {
    for commit in commits {
        let change = log(jj, &record.repo_path, commit, "change_id").await?;
        let name = format!(
            "{}unsafe/{}/{commit}",
            preserved_archive_prefix(&record.execution_id),
            change.trim()
        );
        point_bookmark(jj, &record.repo_path, &name, commit, false).await?;
    }
    Ok(())
}

pub(in super::super) async fn safe_candidates(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    candidates: Vec<String>,
) -> Result<Vec<String>> {
    let prefix = format!("{}unsafe/", preserved_archive_prefix(&record.execution_id));
    let names = jj
        .run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "bookmark",
                "list",
                &format!("glob:{prefix}*/*"),
                "-T",
                "name ++ \"\\n\"",
            ],
        )
        .await?;
    let changes: Vec<_> = names
        .lines()
        .filter_map(|name| name.strip_prefix(&prefix)?.split_once('/').map(|(change, _)| change))
        .collect();
    if changes.is_empty() {
        return Ok(candidates);
    }
    let mut safe = Vec::new();
    for candidate in candidates {
        // Compare identities in the candidate's actual history, including a
        // hidden recorded original, rather than only visible change variants.
        let ancestors = log(jj, &record.repo_path, &format!("::{candidate}"), "change_id ++ \"\\n\"").await?;
        if !ancestors.lines().any(|change| changes.contains(&change)) {
            safe.push(candidate);
        }
    }
    Ok(safe)
}

/// A prior rejection also applies when a retry cannot resolve the journal.
pub(super) async fn reject_unsafe_outcome(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    mut outcome: PreserveOutcome,
) -> Result<PreserveOutcome> {
    if let Some(tip) = outcome.tip.as_ref()
        && safe_candidates(jj, record, vec![tip.commit_id.clone()])
            .await?
            .is_empty()
    {
        let name = preserved_name(&record.execution_id);
        if !log_change(jj, &record.repo_path, &revision(&name))
            .await?
            .trim()
            .is_empty()
        {
            forget_preserved(jj, &record.repo_path, &name).await?;
        }
        outcome.tip = None;
    }
    Ok(outcome)
}
