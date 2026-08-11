//! Incident-004 AI-3: revision implementations may terminalize to
//! PendingReview / InReview only with an explicit associated change or an
//! explicit nothing-to-do outcome.
//!
//! The merge-poller fast path historically terminalized a reaped mid-turn
//! revision toward review while the engine *separately* logged
//! `pr_review noop skip … skip_reason: 'sha_unchanged'`. Both facts were
//! recorded; they were never coupled. This module is the coupling: every
//! path into [`WorkerCompletionHandler::finalize_pr_transition`] for a
//! revision must pass one of the positive signals before the status
//! write, and the engine logs which one carried the decision.
//!
//! Prior work this couples to (not replaces):
//! - `check_noop_skip` / `sha_unchanged` in `finalize_passes.rs` — still
//!   skips the reviewer; now also blocks false-success terminalization.
//! - Metadata-only finalize (`metadata_gate.rs`, issue #1252) — positive
//!   evidence via `metadata_fix_confirmed_at`.
//! - `NO_CHANGES_NEEDED` / `worker_signalled_no_op` — explicit worker
//!   outcome; silence after a mid-turn reap is not this.
//! - `health_alone_satisfies_deliverable` — Stop-path refuse for the
//!   satisfied-deliverable gate; this module covers the same class of
//!   defect on every finalize source (staged recheck, SHA-delta, etc.).
//! - Post-push ownership (#2726): a push or staged PR URL alone does not
//!   mean a revision is done; this gate enforces that on the status write.

use super::*;

/// Positive evidence that a `revision_implementation` may advance toward
/// review. The engine must be able to say which signal carried the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RevisionContributionReason {
    /// Bound PR head moved relative to this execution's `pr_head_before`.
    HeadShaMoved,
    /// A prior Stop for this execution stamped `revision_stop_contributed_head`
    /// — the SHA-delta arm attributed a push to this run.
    RevisionStopContributedHead,
    /// `on_stop` stamped `metadata_fix_confirmed_at` after observing a
    /// PR title/body delta (metadata-only revisions are legitimate).
    MetadataFixConfirmed,
    /// Worker emitted the sanctioned `NO_CHANGES_NEEDED` marker.
    ExplicitNoOp,
    /// Merge-conflict-provenance revision whose bound PR is now mergeable
    /// (conflict cleared — by this run or elsewhere). Matches the
    /// satisfied-deliverable gate's merge-conflict arm.
    ConflictCleared,
    /// CI-fix revision whose *targeted* check is no longer failing
    /// (`stop_ci_cleared` path). Whole-PR CI may still be dirty.
    CiSignalCleared,
    /// Bound PR is already accepted into GitHub's merge queue (or auto-merge
    /// armed) and not UNMERGEABLE — deliverable has left the worker's hands.
    QueuedForMerge,
    /// Caller already ran the satisfied-deliverable gate and only invokes
    /// finalize on arms that are not mere open+healthy+CI-clean with
    /// ProvenAbsent (merged / queue / conflict-cleared).
    SatisfiedDeliverable,
    /// SHA baseline missing or head fetch failed — cannot prove absence.
    /// Allowed with a loud warning (mirrors `health_alone_satisfies_deliverable`'s
    /// Indeterminate arm) so a permanently-missing baseline cannot strand a
    /// revision forever.
    Indeterminate,
}

impl RevisionContributionReason {
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Self::HeadShaMoved => "head_sha_moved",
            Self::RevisionStopContributedHead => "revision_stop_contributed_head",
            Self::MetadataFixConfirmed => "metadata_fix_confirmed",
            Self::ExplicitNoOp => "explicit_no_op",
            Self::ConflictCleared => "conflict_cleared",
            Self::CiSignalCleared => "ci_signal_cleared",
            Self::QueuedForMerge => "queued_for_merge",
            Self::SatisfiedDeliverable => "satisfied_deliverable",
            Self::Indeterminate => "indeterminate",
        }
    }
}

/// Outcome of the AI-3 contribution gate for a revision headed to review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RevisionReviewGate {
    /// At least one positive signal; proceed to terminalize.
    Allow(RevisionContributionReason),
    /// Proven non-contribution (head unchanged, no metadata marker, no
    /// explicit no-op). Must not reach PendingReview / InReview.
    Refuse {
        /// Machine-stable reason for logs and tests (`sha_unchanged`, etc.).
        reason: &'static str,
    },
}

/// Finalize `source` strings that are only emitted after a caller has
/// already established a non-silence contribution. These are not user
/// flags — they are internal path labels. Staged recheck / mid-turn reap
/// paths use other sources and still face the evidence checks below.
fn reason_for_pre_authorized_source(source: &str) -> Option<RevisionContributionReason> {
    match source {
        "stop_conflict_cleared" => Some(RevisionContributionReason::ConflictCleared),
        "stop_ci_cleared" => Some(RevisionContributionReason::CiSignalCleared),
        "stop_satisfied_clean" | "stop_satisfied_merged" => Some(RevisionContributionReason::SatisfiedDeliverable),
        "metadata_only_fix" => Some(RevisionContributionReason::MetadataFixConfirmed),
        // SHA-delta callers stamp attribution / prove movement before
        // calling finalize; accept without a second round trip.
        "stop_sha_delta" | "pr_recheck_sha_delta" => Some(RevisionContributionReason::HeadShaMoved),
        _ => None,
    }
}

impl WorkerCompletionHandler {
    /// Evaluate whether a `revision_implementation` may terminalize toward
    /// PendingReview / InReview.
    ///
    /// Call only for revisions with a non-merged review target. Primary
    /// implementations and merged-to-done targets do not use this gate.
    ///
    /// Order of evidence (first match wins):
    /// 0. Finalize `source` already established contribution (signal-cleared,
    ///    satisfied-deliverable, SHA-delta, metadata_only_fix).
    /// 1. `metadata_fix_confirmed_at` — observed PR metadata mutation.
    /// 2. `revision_stop_contributed_head` differing from baseline.
    /// 3. Explicit `NO_CHANGES_NEEDED` on the worker transcript.
    /// 4. SHA-delta: head moved → allow; inapplicable → indeterminate.
    /// 5. Non-push deliverable side-states (conflict cleared / merge queue).
    /// 6. Else refuse (`sha_unchanged`).
    pub(super) async fn evaluate_revision_review_contribution(
        &self,
        execution_id: &str,
        execution: &crate::work::WorkExecution,
        pr_url: &str,
        source: &'static str,
    ) -> RevisionReviewGate {
        // 0. Callers that already proved a non-silence outcome.
        if let Some(reason) = reason_for_pre_authorized_source(source) {
            return RevisionReviewGate::Allow(reason);
        }

        // 1. Metadata-only path already stamped positive evidence at a real
        // Stop boundary (or the merge poller is finalizing after CI greened
        // on that stamp). Absence of a push is expected and legitimate.
        match self.work_db.execution_metadata_fix_confirmed(execution_id) {
            Ok(true) => {
                return RevisionReviewGate::Allow(RevisionContributionReason::MetadataFixConfirmed);
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "revision contribution gate: metadata_fix_confirmed read failed; \
                     continuing other evidence checks",
                );
            }
        }

        // 2. Stop-attributed push head. Count only when the stamped head is a
        // real movement relative to `pr_head_before` — `stop_staged` stamps
        // whatever the live head is today, including an unchanged head, so
        // bare presence would re-open the silence-as-success hole. When no
        // baseline exists, attribution alone is accepted (cannot prove
        // absence without a reference point).
        match self.work_db.get_revision_stop_contributed_head(execution_id) {
            Ok(Some(stamped)) => {
                let baseline = execution.pr_head_before.as_deref().filter(|s| !s.is_empty());
                match baseline {
                    Some(before) if before == stamped.as_str() => {
                        // Stamp equals baseline — not a contribution. Fall through.
                    }
                    _ => {
                        return RevisionReviewGate::Allow(RevisionContributionReason::RevisionStopContributedHead);
                    }
                }
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "revision contribution gate: revision_stop_contributed_head read failed; \
                     continuing other evidence checks",
                );
            }
        }

        // 3. Explicit worker outcome — never inferred from silence.
        if self.worker_signalled_no_op(execution_id).await {
            return RevisionReviewGate::Allow(RevisionContributionReason::ExplicitNoOp);
        }

        // 4. Head SHA movement vs dispatch-time (or absorbed) baseline.
        match self.evaluate_sha_delta_gate(execution_id, execution).await {
            ShaDeltaGateOutcome::Contributed { .. } => {
                return RevisionReviewGate::Allow(RevisionContributionReason::HeadShaMoved);
            }
            ShaDeltaGateOutcome::NoContribution { .. } => {
                // Proven non-push. Before refusing, accept non-push
                // deliverable side-states below.
            }
            ShaDeltaGateOutcome::Inapplicable => {
                return RevisionReviewGate::Allow(RevisionContributionReason::Indeterminate);
            }
        }

        // 5. Non-push deliverable side-states (conflict cleared / merge queue).
        // These are not "silence" — the PR's lifecycle advanced past the
        // worker's job — and must keep working alongside AI-3.
        if let Some(reason) = self
            .revision_non_push_deliverable_reason(execution_id, execution, pr_url)
            .await
        {
            return RevisionReviewGate::Allow(reason);
        }

        RevisionReviewGate::Refuse {
            reason: "sha_unchanged",
        }
    }

    /// Probe the bound PR for non-push success states the satisfied-deliverable
    /// gate already honors for a no-push revision. Returns `None` when the PR
    /// is merely open+healthy (or the probe fails) — that must not pass AI-3.
    async fn revision_non_push_deliverable_reason(
        &self,
        execution_id: &str,
        execution: &crate::work::WorkExecution,
        pr_url: &str,
    ) -> Option<RevisionContributionReason> {
        let probe = match self.merge_probe.probe(pr_url).await {
            Ok(p) => p,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    pr_url,
                    ?err,
                    "revision contribution gate: merge probe failed while checking non-push \
                     deliverable states; treating as not applicable",
                );
                return None;
            }
        };

        // Queued for merge (not UNMERGEABLE): GitHub owns the rest.
        let queued_for_merge = probe.in_merge_queue && probe.merge_queue_entry_state.as_deref() != Some("UNMERGEABLE");
        if queued_for_merge {
            return Some(RevisionContributionReason::QueuedForMerge);
        }

        // Merge-conflict-provenance revision whose conflict is gone.
        // Mergeability alone is the completion signal for this kind (CI is
        // a separate concern it was never asked to fix).
        if self.is_merge_conflict_revision(execution)
            && let PrLifecycleState::Open(ref open) = probe.state
            && matches!(open.mergeability, OpenPrMergeability::Clean)
        {
            return Some(RevisionContributionReason::ConflictCleared);
        }

        None
    }
}
