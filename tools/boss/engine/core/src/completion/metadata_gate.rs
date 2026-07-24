//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

impl WorkerCompletionHandler {
    /// On-Stop arm of the metadata-only CI-fix finalize gate (issue
    /// #1252). Called from `on_stop_inner`'s `NoContribution` branch —
    /// i.e. at a *real* Stop boundary where the bound PR head did not move
    /// this run.
    ///
    /// Detects whether this revision produced an operator-visible
    /// PR-metadata delta (the live PR body differs from the
    /// `pr_body_before` snapshot taken at run start). If so it:
    ///   - stamps the `metadata_fix_confirmed_at` marker — positive
    ///     evidence (real Stop boundary + operator-visible delta) that the
    ///     merge poller consumes when CI greens *after* this Stop, and
    ///   - finalizes immediately if CI is already green (returning the
    ///     finalize outcome); otherwise returns `AwaitingInput` (recorded,
    ///     awaiting CI — deliberately NOT a nudge, because a metadata-only
    ///     fix has nothing to push to the existing PR).
    ///
    /// Returns `None` when this is not a metadata-only fix (not a
    /// revision, no baseline snapshot, fetch failure, or the body is
    /// unchanged) so the caller falls through to its normal nudge: head
    /// unchanged AND body unchanged means the worker contributed nothing
    /// this run, which must NOT be mistaken for a clean no-op completion.
    pub(super) async fn try_finalize_metadata_only_fix_on_stop(
        &self,
        execution_id: &str,
        execution: &crate::work::WorkExecution,
        bound_pr_url: &str,
    ) -> Option<StopOutcome> {
        if execution.kind != ExecutionKind::RevisionImplementation {
            return None;
        }
        // Baseline body snapshot from run start. `None` means no baseline
        // (new-PR flow, or the start-of-run fetch failed) — we cannot prove
        // a delta, so fall through to the normal nudge.
        let before = match self.work_db.get_execution_pr_body_before(execution_id) {
            Ok(Some(body)) => body,
            Ok(None) => return None,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    ?err,
                    "metadata-fix on-stop: pr_body_before read failed; falling through to nudge",
                );
                return None;
            }
        };
        let repo_slug = parse_repo_slug(&execution.repo_remote_url).ok()?;
        let pr_number = pr_number_from_url(bound_pr_url)?;
        let current = match self.branch_verifier.fetch_pr_body(&repo_slug, pr_number).await {
            Ok(body) => body,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    bound_pr_url,
                    ?err,
                    "metadata-fix on-stop: live PR body fetch failed; falling through to nudge",
                );
                return None;
            }
        };
        if current == before {
            // No operator-visible delta: head unchanged AND body unchanged.
            // The worker contributed nothing this run — let the caller nudge.
            return None;
        }
        // Operator-visible PR-metadata delta produced at a real Stop
        // boundary. Persist the positive-evidence marker BEFORE attempting
        // to finalize so a transient probe failure still lets the merge
        // poller finalize once CI goes green.
        if let Err(err) = self.work_db.mark_execution_metadata_fix_confirmed(execution_id) {
            tracing::warn!(
                execution_id,
                ?err,
                "metadata-fix on-stop: failed to persist confirmation marker",
            );
        }
        if let Some(outcome) = self
            .finalize_metadata_only_revision_if_ready(execution_id, bound_pr_url)
            .await
        {
            return Some(outcome);
        }
        // Delta recorded but CI not yet green: return quietly (no nudge —
        // there is nothing to push). The merge poller's `recheck_for_pr`
        // finalizes once CI goes green, gated on the marker just stamped.
        tracing::info!(
            execution_id,
            bound_pr_url,
            "stop event: PR-metadata-only CI fix recorded; awaiting CI to go green before \
             finalizing (issue #1252)",
        );
        Some(StopOutcome::AwaitingInput)
    }

    /// Finalize a metadata-only CI-fix revision IF its bound PR is now in
    /// a demonstrably-healthy state. Probes the bound parent PR and
    /// decides from its live state:
    ///   - open with clean CI → the fix landed → finalize to `in_review`;
    ///   - already merged      → finalize to `done`;
    ///   - CI still failing / in-flight, closed-unmerged, or the probe
    ///     failed → return `None` (caller leaves it for a later sweep).
    ///
    /// Callers MUST first establish the positive evidence that this is a
    /// legitimate no-code-change completion: a *real* Stop boundary that
    /// observed an operator-visible PR-metadata delta. `on_stop` is itself
    /// that boundary; the merge poller gates on the
    /// `metadata_fix_confirmed_at` marker `on_stop` stamps. This helper
    /// only re-checks CI — it deliberately does NOT re-derive the
    /// Stop/delta evidence, so the regression-prone "head unchanged + CI
    /// green" inference (#1262) can never be reached without it.
    /// Idempotent against an already-finalized execution
    /// (`finalize_pr_transition` returns `AlreadyTerminal` for a non-live
    /// row).
    pub(super) async fn finalize_metadata_only_revision_if_ready(
        &self,
        execution_id: &str,
        bound_pr_url: &str,
    ) -> Option<StopOutcome> {
        let probe = match self.merge_probe.probe(bound_pr_url).await {
            Ok(p) => p,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    bound_pr_url,
                    ?err,
                    "metadata-fix finalize: bound-PR probe failed; will retry on a later sweep",
                );
                return None;
            }
        };
        let target = match &probe.state {
            // Require BOTH clean CI and clean mergeability: a metadata-only
            // edit must not finalize a PR that still carries a *separate*
            // blocking signal (e.g. a merge conflict on a conflict-resolution
            // revision the worker did not actually rebase). Only a genuinely
            // review-ready PR advances.
            PrLifecycleState::Open(open)
                if open.mergeability == OpenPrMergeability::Clean && matches!(open.ci, OpenPrCiStatus::Clean) =>
            {
                tracing::info!(
                    execution_id,
                    bound_pr_url,
                    "metadata-fix finalize: bound PR open, mergeable, with clean CI and a \
                     Stop-confirmed PR-metadata delta — finalizing the revision to in_review \
                     (issue #1252)",
                );
                WorkerPrCompletionTarget::InReview
            }
            PrLifecycleState::Merged => {
                tracing::info!(
                    execution_id,
                    bound_pr_url,
                    "metadata-fix finalize: bound PR already merged — finalizing the revision \
                     to done (issue #1252)",
                );
                WorkerPrCompletionTarget::Done
            }
            // CI still failing / in-flight, or PR closed-unmerged: the fix
            // has not demonstrably landed. Leave it; a later sweep re-probes.
            _ => return None,
        };
        Some(
            self.finalize_pr_transition(execution_id, bound_pr_url.to_owned(), target, "metadata_only_fix")
                .await,
        )
    }

    /// Deliverable-satisfied finalization gate (zombie-worker / "nothing
    /// left to do" loop fix).
    ///
    /// Called in the `NoContribution` branch of `on_stop_inner` after all
    /// per-kind signal-clearing and metadata-only gates have returned
    /// `None`. Probes the bound PR; if it is already in a satisfactory
    /// state — open with CI clean and no merge conflict, or already merged
    /// — the worker's deliverable is complete regardless of whether it
    /// pushed new commits this run. Finalizes immediately instead of
    /// nudging, preventing the spin loop where workers park at the Stop
    /// boundary emitting "nothing left to do" and hold their pool slot
    /// indefinitely until manually reaped.
    ///
    /// Returns `Some(DeliverableSatisfied { pr_url })` when finalized (CI
    /// clean open PR or already-merged), or `None` when the PR is not yet
    /// satisfied (CI in-flight, CI failing, merge conflict) or the probe
    /// fails (safe fallback to the normal nudge path).
    ///
    /// Safety: intentionally NOT called from `recheck_for_pr`. The merge-
    /// poller sweep runs for `waiting_human` executions even when the
    /// worker died without a clean Stop (crash, network cut). Applying
    /// "head unchanged + CI clean → finalize" there would reap dead
    /// workers that still need reconciliation — the exact race rolled back
    /// in #1262. The `on_stop` path is safe because the Stop hook fires
    /// only when the worker completed a turn (real activity, not a crash).
    pub(super) async fn try_finalize_satisfied_deliverable_on_stop(
        &self,
        execution_id: &str,
        execution: &crate::work::WorkExecution,
        bound_pr_url: &str,
    ) -> Option<StopOutcome> {
        let probe = match self.merge_probe.probe(bound_pr_url).await {
            Ok(p) => p,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    bound_pr_url,
                    ?err,
                    "satisfied-deliverable gate: PR probe failed; falling through to nudge"
                );
                return None;
            }
        };

        // A merge-conflict-provenance revision's entire job is clearing the
        // conflict — CI passing is a separate concern it was never asked to
        // fix. Requiring CI clean here as well means a revision whose
        // conflict already cleared (independently, e.g. via the periodic
        // merge-poller sweep retiring the `conflict_resolutions` ledger row
        // before this Stop) sits nudging forever whenever CI happens to be
        // in-flight or unrelated-failing at that moment. For this kind,
        // mergeability alone is the completion signal.
        let merge_conflict_revision = self.is_merge_conflict_revision(execution);

        // A PR that GitHub has already accepted into its merge queue (or
        // armed for auto-merge) is, from the worker's point of view, done:
        // GitHub — not the worker — owns getting it to `main` from here.
        // The merge queue re-runs required checks against a synthetic ref
        // before merging, so `open.ci` legitimately reads `InFlight` (not
        // `Clean`) for the entire time the PR sits queued; requiring CI
        // clean here as well means a revision that pushed its fix and
        // re-enqueued for auto-merge gets nudged to "push to the existing
        // PR" on every Stop until the circuit breaker parks it — even
        // though there is nothing left to push (2026-07-14 incident,
        // exec_18c21b03972f3920_49 / spinyfin/mono#1980: the revision
        // pushed, re-enqueued, and reported done; the gate only recognised
        // `Clean` CI, so it nudged three times before the breaker tripped
        // and abandoned an already-finished execution). `UNMERGEABLE` is
        // excluded: that state means the queue itself rejected the PR
        // (e.g. a check failed inside the queue run), which is a real
        // problem the gate must not paper over — it falls through to the
        // normal nudge/park path like any other non-clean CI state.
        let queued_for_merge = probe.in_merge_queue && probe.merge_queue_entry_state.as_deref() != Some("UNMERGEABLE");

        let inner = match probe.state {
            PrLifecycleState::Merged => {
                tracing::info!(
                    execution_id,
                    bound_pr_url,
                    "satisfied-deliverable gate: PR already merged — finalizing without nudge"
                );
                self.finalize_pr_transition(
                    execution_id,
                    bound_pr_url.to_owned(),
                    WorkerPrCompletionTarget::Done,
                    "stop_satisfied_merged",
                )
                .await
            }
            PrLifecycleState::Open(ref open)
                if mergeability_satisfies_deliverable(open.mergeability, merge_conflict_revision)
                    && (matches!(open.ci, OpenPrCiStatus::Clean) || merge_conflict_revision || queued_for_merge) =>
            {
                tracing::info!(
                    execution_id,
                    bound_pr_url,
                    kind = %execution.kind,
                    merge_conflict_revision,
                    ci_status = ?open.ci,
                    in_merge_queue = probe.in_merge_queue,
                    merge_queue_entry_state = ?probe.merge_queue_entry_state,
                    "satisfied-deliverable gate: PR open with no conflict (CI clean, CI irrelevant \
                     for a merge-conflict revision, or already queued for auto-merge) — finalizing \
                     without nudge"
                );
                self.finalize_pr_transition(
                    execution_id,
                    bound_pr_url.to_owned(),
                    WorkerPrCompletionTarget::InReview,
                    "stop_satisfied_clean",
                )
                .await
            }
            _ => {
                tracing::debug!(
                    execution_id,
                    bound_pr_url,
                    state = ?probe.state,
                    "satisfied-deliverable gate: PR not yet satisfied (CI in-flight / failing / conflict); falling through to nudge"
                );
                return None;
            }
        };

        // Map through DeliverableSatisfied so logs and tests can
        // identify this path distinctly from a fresh-push finalization.
        // ReviewerEnqueued is also a successful finalization — the PR
        // advanced to InReview and a reviewer pass was triggered; the
        // deliverable is still satisfied.
        Some(match inner {
            StopOutcome::PrDetected { pr_url }
            | StopOutcome::PrMerged { pr_url }
            | StopOutcome::ReviewerEnqueued { pr_url } => StopOutcome::DeliverableSatisfied { pr_url },
            other => other,
        })
    }

    /// Evaluate the resume-bounce SHA-delta gate. The gate uses the
    /// chore's bound `pr_url` (set by an earlier run's on-Stop
    /// machinery) as the authoritative PR identifier — never
    /// branch-search — and verifies "this run contributed" by
    /// comparing the bound PR's current head SHA against the
    /// snapshot in `execution.pr_head_before`. See [`Self::on_execution_started`]
    /// for the snapshot path.
    pub(super) async fn evaluate_sha_delta_gate(
        &self,
        execution_id: &str,
        execution: &crate::work::WorkExecution,
    ) -> ShaDeltaGateOutcome {
        // The chore-bound PR URL is the only authoritative identifier
        // permitted here. No branch search.
        let work_item = match self.work_db.get_work_item(&execution.work_item_id) {
            Ok(item) => item,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    work_item_id = %execution.work_item_id,
                    ?err,
                    "sha-delta gate: work item lookup failed; treating as inapplicable"
                );
                return ShaDeltaGateOutcome::Inapplicable;
            }
        };
        let bound_pr_url = match work_item {
            WorkItem::Task(task) | WorkItem::Chore(task) => {
                // Primary: the task's own pr_url (structured field only).
                // Fallback for revision_implementation: use execution.pr_url
                // (set to the chain root's PR URL at dispatch time), because
                // revision tasks always have task.pr_url = NULL by design.
                let from_task = crate::runner::task_bound_pr_url(&task).map(str::to_owned);
                match from_task {
                    Some(url) => url,
                    None if execution.kind == ExecutionKind::RevisionImplementation => {
                        // Primary: execution.pr_url stamped at dispatch time.
                        // Fallback: chain-root lookup for executions where it
                        // was not stamped.
                        match execution
                            .pr_url
                            .clone()
                            .filter(|u| !u.is_empty())
                            .or_else(|| self.work_db.get_revision_chain_root_pr_url(&task.id))
                        {
                            Some(url) => url,
                            None => return ShaDeltaGateOutcome::Inapplicable,
                        }
                    }
                    None => return ShaDeltaGateOutcome::Inapplicable,
                }
            }
            _ => return ShaDeltaGateOutcome::Inapplicable,
        };
        let pr_head_before = match execution.pr_head_before.as_deref() {
            Some(s) if !s.is_empty() => s.to_owned(),
            _ => {
                tracing::debug!(
                    execution_id,
                    bound_pr_url = %bound_pr_url,
                    "sha-delta gate: bound PR present but pr_head_before snapshot missing; falling through"
                );
                return ShaDeltaGateOutcome::Inapplicable;
            }
        };
        let repo_slug = match parse_repo_slug(&execution.repo_remote_url) {
            Ok(slug) => slug,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    repo_remote_url = %execution.repo_remote_url,
                    ?err,
                    "sha-delta gate: cannot parse repo slug; falling through"
                );
                return ShaDeltaGateOutcome::Inapplicable;
            }
        };
        let pr_number = match pr_number_from_url(&bound_pr_url) {
            Some(n) => n,
            None => {
                tracing::warn!(
                    execution_id,
                    bound_pr_url = %bound_pr_url,
                    "sha-delta gate: cannot parse PR number from bound URL; falling through"
                );
                return ShaDeltaGateOutcome::Inapplicable;
            }
        };
        let head_now = match self.branch_verifier.fetch_pr_head_oid(&repo_slug, pr_number).await {
            Ok(oid) => oid,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    bound_pr_url = %bound_pr_url,
                    ?err,
                    "sha-delta gate: fetch headRefOid failed; falling through to cold-path detector"
                );
                return ShaDeltaGateOutcome::Inapplicable;
            }
        };
        if head_now == pr_head_before {
            tracing::info!(
                execution_id,
                bound_pr_url = %bound_pr_url,
                pr_head_before = %pr_head_before,
                "sha-delta gate: bound PR head unchanged — worker did not contribute"
            );
            ShaDeltaGateOutcome::NoContribution {
                pr_url: bound_pr_url,
                head_now,
            }
        } else {
            tracing::info!(
                execution_id,
                bound_pr_url = %bound_pr_url,
                pr_head_before = %pr_head_before,
                head_now = %head_now,
                "sha-delta gate: bound PR head moved — contribution verified"
            );
            ShaDeltaGateOutcome::Contributed {
                pr_url: bound_pr_url,
                head_now,
            }
        }
    }
}
