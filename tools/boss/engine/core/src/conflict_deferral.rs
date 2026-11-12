//! Just-in-time merge-conflict resolution: deciding when a detected conflict
//! is only recorded, and when it is handed to the resolver.
//!
//! Most conflict resolutions are wasted. A PR that is still weeks from
//! merging conflicts again after every resolution, and each one costs a
//! rebase, a worker, a build gate and a force-push that restarts CI
//! (`docs/investigations/merge-conflict-sources-and-remedies.md`, Finding 2
//! and idea E4). With deferral on, [`gate`] sits at the front of
//! `conflict_watch::on_conflict_detected_with` and answers one question for
//! a PR GitHub reports as conflicting: resolve now, or record and wait?
//!
//! # When a conflict waits
//!
//! Only when all of these hold:
//!
//! - the PR is in the rollout's treatment arm
//!   ([`ConflictDeferralArm::defers`]);
//! - no merge has been requested for it ([`merge_requested`]);
//! - its work item is `in_review`, with no resolution already in flight or
//!   owed (no live attempt, no live conflict revision, no `merge_conflict`
//!   signal). A row that is blocked or was moved by a person keeps the
//!   watcher's existing handling;
//! - this conflict has not already been released once.
//!
//! Then the conflict is written to the `conflict_resolutions` ledger as a
//! `deferred` row and nothing else happens: no parent flip, no rebase, no
//! resolver, no revision. The PR keeps reading as conflicting everywhere,
//! because it is.
//!
//! # When it stops waiting
//!
//! Every later detection pass re-asks the question, so a held conflict is
//! released on the first pass after any of the conditions above stops
//! holding. Releasing only ends the deferral record; the same pass then
//! falls through to the unchanged resolution path, so a released conflict
//! goes through the escalation ladder, the worker fallback, the churn guard
//! and the push gate exactly like one that was never deferred.
//!
//! # The readiness signal
//!
//! "Otherwise ready to merge" is read as *a merge has been requested*. That
//! is the one fact the engine holds that says a person has decided this PR
//! should land: nothing in the engine merges a PR on its own. Review and CI
//! state would be the alternative, but a PR is typically reviewed and green
//! within minutes of opening and can then sit unmerged for weeks, conflicting
//! over and over, so keying on them would resolve exactly the conflicts
//! deferral exists to skip.
//!
//! See `tools/boss/docs/conflict-resolution-deferral.md` for the operator
//! view.

use boss_execution_contracts::ExecutionPublisher;
use boss_feature_flags::ConflictDeferralArm;

use crate::merge_poller::{PrLifecycleProbe, stored_pr_number};
use crate::work::{
    ConflictDeferralEnd, ConflictDeferralRecord, ConflictResolution, ConflictResolutionInsertInput, PendingMergeCheck,
    WorkDb,
};

/// What the detection path should do with a conflicting PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    /// The conflict is recorded and waits. `recorded` is `true` on the pass
    /// that wrote the record and `false` on every repeat.
    Hold { recorded: bool },
    /// Resolve it now, through the normal path.
    Proceed,
}

/// The evidence that a merge has been requested for this PR, or `None`.
///
/// Four sources, any one of which is enough:
///
/// - GitHub reports the PR in its merge queue, or with auto-merge armed.
///   These cover a merge requested on GitHub directly.
/// - The engine holds an active GitHub merge intent for the PR: a Merge When
///   Ready that GitHub accepted. GitHub's queue and auto-merge fields lag
///   that request, so the stored intent is consulted as well.
/// - The engine holds an active Trunk merge intent for the work item, the
///   standing record that a person approved the merge on a Trunk-queue
///   product.
///
/// A store that cannot be read counts as a request: resolving a conflict
/// early is the established behaviour, and holding one back on a guess
/// could strand a merge someone is waiting for.
pub(crate) fn merge_requested(
    work_db: &WorkDb,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
) -> Option<&'static str> {
    if probe.in_merge_queue {
        return Some("github_merge_queue");
    }
    if probe.auto_merge_enabled {
        return Some("github_auto_merge");
    }
    match work_db.get_active_github_merge_intent(&candidate.work_item_id, &candidate.pr_url) {
        Ok(Some(_)) => return Some("github_merge_intent"),
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "conflict_deferral: could not read the GitHub merge intent; treating the PR as merge-requested",
            );
            return Some("merge_intent_unreadable");
        }
    }
    match work_db.get_active_trunk_merge_intent(&candidate.work_item_id) {
        Ok(Some(_)) => Some("trunk_merge_intent"),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "conflict_deferral: could not read the Trunk merge intent; treating the PR as merge-requested",
            );
            Some("merge_intent_unreadable")
        }
    }
}

/// Decide whether `candidate`'s conflict waits or is resolved now, recording
/// or ending the deferral as needed. See the module docs for the rules.
///
/// Never an error: when anything about the decision cannot be read, the
/// answer is [`Gate::Proceed`], because the one outcome this must not
/// produce is a conflict that is neither resolved nor recorded.
pub(crate) async fn gate(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
    arm: ConflictDeferralArm,
) -> Gate {
    if !arm.defers() {
        // Deferral is off for this PR (flag off, or the control arm). The
        // only thing left to do is release a conflict deferred earlier, so
        // switching the flag off drains every held conflict within a pass.
        let held = work_db
            .open_conflict_deferral(&candidate.work_item_id)
            .unwrap_or_else(|err| {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    ?err,
                    "conflict_deferral: could not look up an open deferral; proceeding to resolve",
                );
                None
            });
        if held.is_some() {
            end(work_db, publisher, candidate, ConflictDeferralEnd::DeferralDisabled, "").await;
        }
        return Gate::Proceed;
    }

    let state = match work_db.conflict_deferral_state(&candidate.work_item_id, &candidate.pr_url) {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_deferral: could not read deferral state; proceeding to resolve",
            );
            return Gate::Proceed;
        }
    };

    let release = if let Some(signal) = merge_requested(work_db, candidate, probe) {
        Some((ConflictDeferralEnd::MergeRequested, signal))
    } else if state.resolution_owed {
        Some((
            ConflictDeferralEnd::ResolutionStarted,
            "a resolution is already in flight or owed",
        ))
    } else if work_db
        .has_live_conflict_revision_for_pr(&candidate.pr_url)
        .unwrap_or(true)
    {
        Some((
            ConflictDeferralEnd::ResolutionStarted,
            "a conflict revision is running or queued",
        ))
    } else {
        None
    };
    if let Some((reason, detail)) = release {
        if state.open.is_some() {
            end(work_db, publisher, candidate, reason, detail).await;
        }
        return Gate::Proceed;
    }

    if !state.in_review {
        // Blocked, or moved by a person: the watcher's own guards decide what
        // happens to such a row, as they always have. Nothing is recorded
        // here, and a record made while the item was in review is left open
        // until a resolution is seen to start.
        return Gate::Proceed;
    }
    if state.open.is_some() {
        return Gate::Hold { recorded: false };
    }
    if state.conflict_already_released() {
        // This same conflict was handed to the resolver earlier (a person
        // asked, or its resolution attempt did not land). It stays with the
        // resolver; deferring it again would silently undo that request.
        return Gate::Proceed;
    }

    let record = work_db.record_conflict_deferral(ConflictResolutionInsertInput {
        product_id: candidate.product_id.clone(),
        work_item_id: candidate.work_item_id.clone(),
        pr_url: candidate.pr_url.clone(),
        pr_number: stored_pr_number(&candidate.pr_url).unwrap_or(0),
        head_branch: probe.head_ref_name.as_deref().unwrap_or("").to_owned(),
        base_branch: probe.base_ref_name.as_deref().unwrap_or("").to_owned(),
        base_sha_at_trigger: probe.base_ref_oid.clone(),
        head_sha_before: probe.head_ref_oid.clone(),
    });
    match record {
        Ok(ConflictDeferralRecord::Recorded(row)) => {
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                deferral_id = %row.id,
                base_ref_oid = ?probe.base_ref_oid,
                head_ref_oid = ?probe.head_ref_oid,
                "conflict_deferral: PR conflicts with base and no merge has been requested; \
                 conflict recorded, resolution deferred until a merge is requested or a human asks",
            );
            publisher
                .publish_work_item_changed(&row.product_id, &row.work_item_id, "conflict_deferred")
                .await;
            Gate::Hold { recorded: true }
        }
        Ok(ConflictDeferralRecord::AlreadyOpen(_)) => Gate::Hold { recorded: false },
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "conflict_deferral: failed to record the deferral; resolving now rather than \
                 holding an unrecorded conflict",
            );
            Gate::Proceed
        }
    }
}

/// Label a freshly inserted resolution attempt with the arm its PR is in, so
/// the two arms of the rollout can be compared from the ledger alone.
pub(crate) fn label_attempt(work_db: &WorkDb, attempt: &ConflictResolution, arm: ConflictDeferralArm) {
    let Some(label) = arm.ledger_label() else {
        return;
    };
    if let Err(err) = work_db.stamp_conflict_resolution_deferral_arm(&attempt.id, label) {
        tracing::warn!(
            attempt_id = %attempt.id,
            ?err,
            "conflict_deferral: failed to label the attempt with its rollout arm",
        );
    }
}

/// GitHub reports the PR mergeable again: a conflict that was being held
/// back is gone without anyone resolving it. Ends the deferral record and
/// returns `true` when there was one.
pub(crate) fn on_conflict_cleared(work_db: &WorkDb, candidate: &PendingMergeCheck) -> bool {
    match work_db.end_conflict_deferral(&candidate.work_item_id, ConflictDeferralEnd::ConflictCleared) {
        Ok(Some(row)) => {
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                deferral_id = %row.id,
                "conflict_deferral: PR is mergeable again; deferred conflict cleared without a resolution run",
            );
            true
        }
        Ok(None) => false,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "conflict_deferral: failed to end the deferral of a PR that is mergeable again",
            );
            false
        }
    }
}

async fn end(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    reason: ConflictDeferralEnd,
    detail: &str,
) {
    match work_db.end_conflict_deferral(&candidate.work_item_id, reason) {
        Ok(Some(row)) => {
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                deferral_id = %row.id,
                reason = reason.as_str(),
                detail,
                "conflict_deferral: deferred conflict released to the resolver",
            );
            publisher
                .publish_work_item_changed(&row.product_id, &row.work_item_id, "conflict_deferral_ended")
                .await;
        }
        Ok(None) => {}
        Err(err) => tracing::warn!(
            work_item_id = %candidate.work_item_id,
            reason = reason.as_str(),
            ?err,
            "conflict_deferral: failed to end the deferral; the conflict is resolved regardless",
        ),
    }
}

/// The arm `candidate`'s PR is in under the live deferral flags.
pub(crate) fn arm_for(
    flags: &boss_feature_flags::FeatureFlagsStore,
    candidate: &PendingMergeCheck,
) -> ConflictDeferralArm {
    let pr_number = stored_pr_number(&candidate.pr_url).and_then(|number| u64::try_from(number).ok());
    flags.conflict_deferral_arm(pr_number)
}
