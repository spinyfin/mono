//! Worker instructions for verified workspace handoffs and fresh fallbacks.
use crate::work::{WorkExecution, WorkItem};
use boss_protocol::{ExecutionKind, TaskKind};
use std::path::Path;

use boss_engine_recovery::recovery_apply::{RecoveryReport, RecoverySource};

pub(super) fn recovery_block(report: &RecoveryReport) -> Option<String> {
    let state = match report.source {
        RecoverySource::BlockedInPlace => {
            "The engine verified and re-leased the deliberately blocked predecessor's exact workspace. Its described commits and working copy are your starting point. Before any checkout, reset, or PR-positioning command, inspect `jj status`, `jj diff`, and `jj log -r '::@' -n 10`. Read the inherited changes and reconcile them with this brief and current main. Keep the inherited checkout while doing so; generic PR checkout instructions below do not override this recovery handoff."
        }
        RecoverySource::BlockedFresh => {
            "The deliberately blocked predecessor's workspace was unavailable or its ownership/state could not be verified. You have a fresh checkout; do not assume its local work was recovered. Continue from the brief and the existing PR when present."
        }
        _ => return None,
    };
    Some(format!(
        "## BLOCKED WORKSPACE RECOVERY\n\n{state}\n\nPredecessor: `{}`. Re-run the required build and tests in your own leased workspace before pushing any inherited or new commits. The previous worker's validation does not satisfy your gate.\n\n",
        report.from_execution_id,
    ))
}

/// Explain the exact workspace handoff for a review revision converted to a
/// followup because its parent PR merged mid-run. This keys primarily on the
/// dedicated execution shape written by `reconcile_work_item_execution` — a
/// chore-implementation followup with a soft dirty-workspace preference.
/// Blocked-run reports take precedence at the call site. Task kind alone
/// cannot identify a merge-cancel conversion:
/// `resolve_revision_on_parent_close` (work/chain_helpers.rs) falls back
/// from `TaskKind::Followup` to plain `TaskKind::Chore` when the chain
/// root's PR URL is missing or unparseable, and that chore-fallback
/// conversion still inherits the same workspace/allow_dirty shape, so it
/// needs this brief too.
pub(super) fn merge_cancelled_review_recovery_block(
    execution: &WorkExecution,
    work_item: &WorkItem,
    workspace_path: &Path,
) -> Option<String> {
    let task = match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) if matches!(task.kind, TaskKind::Followup | TaskKind::Chore) => {
            task
        }
        _ => return None,
    };
    if execution.kind != ExecutionKind::ChoreImplementation || !execution.allow_dirty || !execution.prefer_is_soft {
        return None;
    }
    let preferred = execution.preferred_workspace_id.as_deref()?;
    let origin = task
        .origin_pr_number
        .map(|number| format!("PR #{number}"))
        .unwrap_or_else(|| "the merged origin PR".to_owned());
    let current = execution.cube_workspace_id.as_deref();

    // `reconcile_workspace_recovery` (coordinator/execution.rs) already
    // resolved whether the re-leased workspace's dirty state was actually
    // confirmed — it writes this marker with `RecoverySource::CubeInPlace`
    // only when `lease.dirty_verified == Some(true)`, before this prompt is
    // composed. Same-workspace alone is not proof: cube can re-lease the
    // same workspace after resetting it, or the followup can sit `ready`
    // long enough (pool saturation, dependency gating) for an unrelated
    // task to lease, dirty, and release that workspace first — in which
    // case `--allow-dirty` hands this worker a foreign working copy, not
    // its own cancelled review draft.
    let verified_in_place = current == Some(preferred)
        && boss_engine_recovery::recovery_apply::RecoveryReport::read_for(workspace_path, &execution.id)
            .is_some_and(|report| report.source == boss_engine_recovery::recovery_apply::RecoverySource::CubeInPlace);

    let mut block = String::from("## MERGE-CANCELLED REVIEW RECOVERY\n\n");
    if verified_in_place {
        block.push_str(&format!(
            "This followup was created after {origin} merged while its review-revision worker was mid-run. \
             The engine re-leased that worker's exact workspace (`{preferred}`) without resetting it. \
             Its working copy remains on the merged PR's revision base and may contain partial, \
             uncommitted edits from the cancelled turn.\n\n\
             Inspect before changing the checkout:\n\n\
             ```\n\
             jj status\n\
             jj diff --stat\n\
             jj diff\n\
             ```\n\n\
             Do not trust or discard those edits. They were cut off mid-turn and were never compiled or \
             tested. Reconcile them against this followup and current `main`, then run the required \
             validation before opening the fresh PR.\n\n",
        ));
    } else if current == Some(preferred) {
        block.push_str(&format!(
            "This followup was created after {origin} merged while its review-revision worker was mid-run, \
             and the engine re-leased that worker's exact workspace (`{preferred}`). However, the engine \
             has no confirmed record of what this lease actually returned: it may have been reset (no \
             edits present), or it may have been leased and dirtied by an unrelated task in between and \
             then released back to the pool before landing here. Do not assume the working copy is your \
             own cancelled review draft either way.\n\n\
             Check before doing anything else:\n\n\
             ```\n\
             jj status\n\
             jj diff --stat\n\
             ```\n\n\
             If it holds nothing, proceed as a fresh start from current `main`. If it holds edits, verify \
             they actually belong to this followup's own history (check the log against {origin}'s revision \
             base) before building on them — an edit set from an unrelated task must not be folded into \
             this PR.\n\n",
        ));
    } else {
        let current = current.unwrap_or("an unrecorded fallback workspace");
        block.push_str(&format!(
            "This followup was created after {origin} merged while its review-revision worker was mid-run. \
             The engine preferred the cancelled worker's workspace (`{preferred}`), but cube could not \
             lease it and dispatched this execution on `{current}` instead. This is a fresh-workspace \
             fallback: no partial edits were inherited here. An unverified draft may still remain in \
             `{preferred}` on the merged PR's base; proceed from current `main` in this workspace and do \
             not assume that draft was validated or delivered.\n\n",
        ));
    }
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_handoffs_require_fresh_validation_and_distinguish_fallback() {
        for source in [RecoverySource::BlockedInPlace, RecoverySource::BlockedFresh] {
            let report = RecoveryReport {
                for_execution_id: "new".into(),
                from_execution_id: "prior".into(),
                source,
                applied: None,
                patch_error: None,
            };
            let block = recovery_block(&report).unwrap();
            assert!(block.contains("Re-run the required build and tests in your own leased workspace"));
            if report.source == RecoverySource::BlockedInPlace {
                assert!(block.contains("described commits"));
                assert!(block.contains("generic PR checkout instructions below do not override"));
            } else {
                assert!(block.contains("do not assume its local work was recovered"));
            }
        }
    }
}
