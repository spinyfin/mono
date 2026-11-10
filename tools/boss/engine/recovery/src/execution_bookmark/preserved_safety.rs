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
    // The registered working-copy commit before any snapshot: when it is a
    // strict ancestor of the boundary, a later holder's pending edits land in
    // its change, so that change must be frozen too.
    let registered = if ownership.is_some() && jj.workspace_exists(workspace).await? {
        log_change(jj, &record.repo_path, rev)
            .await?
            .lines()
            .next()
            .map(str::to_owned)
    } else {
        None
    };
    if let Some(workspace) = snapshot {
        resolved = snapshot_free_unproven(jj, &record.repo_path, workspace, resolved).await?;
    }
    // The live registered tip leads: it is what a human inspects first.
    let mut unclaimed = Vec::new();
    let mut unsafe_commits = Vec::new();
    if ownership.is_some() && !commit_is_visible(jj, &record.repo_path, tip).await? {
        unsafe_commits.extend(rewritten_ancestors(jj, &record.repo_path, tip).await?);
        if resolved.rewritten_to.is_none()
            && let Some(successor) = unsafe_commits.first()
        {
            unclaimed.push(Unclaimed {
                note: format!("the lease boundary tip {tip} is hidden and its unpublished ancestry was rewritten afterwards; retaining successor {successor} only in an inspection archive because the edits may belong to a later holder"),
                commit: successor.clone(),
                count: unsafe_commits.len() as u64,
            });
        }
    }
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
            if let Some(registered) = registered.as_ref().filter(|registered| **registered != commit) {
                unsafe_commits.push(registered.clone());
            }
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
        if let Some(registered) = registered.as_deref() {
            // The commit that received the pending edits: the snapshot's
            // visible successor of the registered working-copy change.
            let change = log(jj, &record.repo_path, registered, "change_id").await?;
            let successors = log_change(jj, &record.repo_path, &change_revset(change.trim())).await?;
            if let Some(successor) = successors.lines().find(|id| !id.is_empty())
                && !unsafe_commits.iter().any(|seen| seen == successor)
            {
                unsafe_commits.push(successor.to_owned());
            }
        }
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

/// Freeze changed identities throughout the hidden boundary's unpublished
/// ancestry, including ancestors rewritten before teardown began.
async fn rewritten_ancestors(jj: &dyn Jj, repo: &Path, tip: &str) -> Result<Vec<String>> {
    let originals = log(
        jj,
        repo,
        &format!("::{tip} ~ ::remote_bookmarks()"),
        "commit_id ++ \" \" ++ change_id ++ \"\\n\"",
    )
    .await?;
    let mut rewritten = Vec::new();
    for line in originals.lines() {
        let Some((original, change)) = line.split_once(' ') else {
            continue;
        };
        let visible = log_change(jj, repo, &format!("{} ~ ::remote_bookmarks()", change_revset(change))).await?;
        for successor in visible.lines().filter(|id| *id != original) {
            if !rewritten.iter().any(|seen| seen == successor) {
                rewritten.push(successor.to_owned());
            }
        }
    }
    Ok(rewritten)
}

/// Keep rejected work in inspection archives, separate from restorable work.
/// Freeze the rejected identity in the name: the archive target can follow
/// rewrites or move to a parent on abandonment without changing the verdict.
pub(super) async fn quarantine(jj: &dyn Jj, record: &ExecutionBookmark, commits: &[String]) -> Result<()> {
    for commit in commits {
        // Inspection must never revive a version hidden by a later snapshot.
        if !commit_is_visible(jj, &record.repo_path, commit).await? {
            continue;
        }
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
        let dropped = tip.commit_id.clone();
        // Dropping the primary name must not lose the only name: keep the
        // commit in an inspection archive and say so.
        quarantine(jj, record, std::slice::from_ref(&dropped)).await?;
        let name = preserved_name(&record.execution_id);
        if !log_change(jj, &record.repo_path, &revision(&name))
            .await?
            .trim()
            .is_empty()
        {
            forget_preserved(jj, &record.repo_path, &name).await?;
        }
        outcome.tip = None;
        let note = format!(
            "the preserved tip {dropped} descends from work already quarantined as possibly belonging to a later holder; excluding it from automatic restore and retaining it only in an inspection archive"
        );
        match outcome.inspect.as_mut() {
            Some(inspect) => {
                if !inspect.reason.contains(&dropped) {
                    inspect.reason.push_str(&format!("; {note}"));
                }
            }
            None => {
                outcome.inspect = Some(PreserveInspect {
                    existing_commit_id: Some(dropped),
                    workspace_change_id: None,
                    workspace_commit_id: None,
                    workspace_count: None,
                    reason: note,
                });
            }
        }
    }
    Ok(outcome)
}

/// Does `variant` (a rewrite of `commit` sharing its change id) carry the same
/// content? An empty interdiff, ignoring bookkeeping paths, proves it.
async fn same_change_content(jj: &dyn Jj, repo: &Path, commit: &str, variant: &str) -> Result<bool> {
    let interdiff = jj
        .run(
            repo,
            &[
                "--ignore-working-copy",
                "interdiff",
                "--from",
                commit,
                "--to",
                variant,
                "--git",
            ],
        )
        .await?;
    Ok(crate::recovery_apply::filter_bookkeeping(&interdiff)
        .text
        .trim()
        .is_empty())
}

/// A bookmark follows its change through every rewrite a later holder makes
/// (`jj squash`, `jj absorb`, `jj diffedit`). Only a pure rebase, which leaves
/// the added and removed lines untouched, may move `preserved/<exec>` off the
/// commit teardown recorded; any other evolution may carry foreign edits.
/// Content is compared with `same_change_content` (an `interdiff`,
/// so paths, modes and binary changes count), for every commit of the recorded
/// ancestry including empty ones: a later holder can squash text into an empty
/// boundary. A rebase onto a moved base compares equal even when it leaves
/// the commit conflicted; the restore reports those conflicts.
pub(in super::super) async fn reject_content_evolution(
    jj: &dyn Jj,
    record: &ExecutionBookmark,
    pointer: &str,
    recorded: &PreservedTip,
) -> Result<()> {
    if pointer == recorded.commit_id {
        return Ok(());
    }
    // A recorded commit that no longer resolves cannot be compared.
    if log_change(jj, &record.repo_path, &recorded.commit_id)
        .await?
        .trim()
        .is_empty()
    {
        return Ok(());
    }
    // Only an evolution of the recorded commit can carry a later holder's
    // edits under the same name; unrelated changes are other teardowns.
    let evolution = jj
        .run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "evolog",
                "--no-graph",
                "-r",
                pointer,
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        )
        .await?;
    if !evolution.lines().any(|id| id == recorded.commit_id) {
        return Ok(());
    }
    let stack = log(
        jj,
        &record.repo_path,
        &format!("::{} ~ ::remote_bookmarks()", recorded.commit_id),
        "change_id ++ \" \" ++ commit_id ++ \"\\n\"",
    )
    .await?;
    for line in stack.lines() {
        let Some((change, original)) = line.split_once(' ') else {
            continue;
        };
        let moved = log_change(
            jj,
            &record.repo_path,
            &format!("({}) & ::{pointer}", change_revset(change)),
        )
        .await?;
        let mut unchanged = true;
        for candidate in moved.lines().filter(|id| !id.is_empty()) {
            unchanged &=
                candidate == original || same_change_content(jj, &record.repo_path, original, candidate).await?;
        }
        if !unchanged {
            return Err(pointer_integrity_error(format!(
                "{} moved off the recorded commit {} to {pointer} and change {change} has different content; it may hold a later holder's edits, so it is not restaged",
                preserved_name(&record.execution_id),
                recorded.commit_id
            )));
        }
    }
    Ok(())
}
