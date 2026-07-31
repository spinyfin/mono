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
        let current = match self
            .branch_verifier
            .fetch_pr_title_and_body(&repo_slug, pr_number)
            .await
        {
            Ok((_title, body)) => body,
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
    /// `contribution` is what the SHA-delta gate established about *this
    /// run* moments earlier. It is load-bearing: for a
    /// `revision_implementation` the "open + mergeable + CI clean" arm is
    /// the state the run was DISPATCHED INTO, so it proves nothing on its
    /// own and [`health_alone_satisfies_deliverable`] refuses it when the
    /// gate proved the head did not move. See that function for the full
    /// argument and for why the merged / merge-queue / conflict-cleared
    /// arms are unaffected.
    ///
    /// Safety: intentionally NOT called from `recheck_for_pr`. The merge-
    /// poller sweep runs for `waiting_human` executions even when the
    /// worker died without a clean Stop (crash, network cut). Applying
    /// "head unchanged + CI clean → finalize" there would reap dead
    /// workers that still need reconciliation — the exact race rolled back
    /// in #1262. The `on_stop` path is safe because the Stop hook fires
    /// only when the worker completed a turn (real activity, not a crash).
    ///
    /// Return shape: this method used to return `Option<StopOutcome>`, and
    /// was widened to [`SatisfiedDeliverableOutcome`] rather than wrapped,
    /// because `None` now has to mean two different things to the caller.
    /// "The PR looks satisfied and the only thing missing is the worker's
    /// own declaration" demands a different response from "the PR is not
    /// satisfied": the first goes to [`crate::run_done_backstop`], which
    /// holds quietly, while the second falls through to the ordinary nudge,
    /// still the right answer for a PR with failing CI or an unresolved
    /// conflict. Collapsing them — as the `Option` form must — would put an
    /// undeclared run straight into the nudge loop, which is precisely the
    /// treatment a mid-investigation worker must not get.
    pub(super) async fn evaluate_satisfied_deliverable_on_stop(
        &self,
        execution_id: &str,
        execution: &crate::work::WorkExecution,
        bound_pr_url: &str,
        contribution: ContributionEvidence,
    ) -> SatisfiedDeliverableOutcome {
        let probe = match self.merge_probe.probe(bound_pr_url).await {
            Ok(p) => p,
            Err(err) => {
                tracing::debug!(
                    execution_id,
                    bound_pr_url,
                    ?err,
                    "satisfied-deliverable gate: PR probe failed; falling through to nudge"
                );
                return SatisfiedDeliverableOutcome::NotSatisfied;
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

        // Whether a merely-healthy PR may carry the finalize decision on
        // its own for this run — false for a revision the SHA-delta gate
        // just proved contributed nothing. The two arms below are ordered
        // so a refusal on THIS ground is reported distinctly from the
        // ordinary "PR isn't healthy yet" fallthrough: they are different
        // findings and an operator reading the log must be able to tell
        // them apart.
        let health_alone_satisfies = health_alone_satisfies_deliverable(&execution.kind, contribution);

        // Whether the health-alone arm may carry the finalize decision
        // *at all* for this run, on top of `health_alone_satisfies`'s
        // per-kind evidence question. With `run_done_proposals_seam` on,
        // it may not until the worker has said so itself.
        //
        // Scoped to the health-alone arm deliberately. The merged,
        // merge-queue and conflict-cleared arms below are untouched by this
        // gate, because each of them is a real state change or proof the
        // deliverable has passed out of the worker's hands — waiting for a
        // declaration there would hang a run over a PR that has already
        // merged, which no declaration can un-merge. It is only the
        // "open + mergeable + CI clean" predicate that is trivially true
        // for a run dispatched into an already-healthy PR, and therefore
        // only that one that needs the worker to distinguish "delivered"
        // from "has not started".
        //
        // Only `delivered` satisfies the arm — not merely "some declaration
        // exists". The other two outcomes are the worker actively
        // contradicting the reading it would take:
        //
        // - `no_changes_needed` says the run produced nothing, so
        //   finalizing it as a delivery would advance the work item on a
        //   claim of the opposite, and skip the no-op terminal whose whole
        //   job is to file the attention that makes an unaddressed finding
        //   visible.
        // - `blocked` says the run stopped without delivering. Reading a
        //   healthy PR over that would be the original bug committed with
        //   the worker's own words available and ignored.
        //
        // Hence two booleans rather than one. `declared_delivered` gates the
        // health-alone arm; `declared_at_all` is what separates the two
        // ways of not satisfying it — a run that said nothing goes to the
        // backstop (`AwaitingDeclaration`), a run that declared a
        // non-delivery outcome does not (it already answered; there is
        // nothing to wait for) and instead falls through as `NotSatisfied`
        // to the paths that handle those claims: the no-op terminal reads a
        // `no_changes_needed` declaration directly, and a `blocked` run's
        // companion `blocked` proposal already suppresses the nudge loop.
        //
        // Both booleans are read only by the health-alone arm, so the
        // merged / merge-queue / conflict-cleared arms stay reachable for a
        // run whose declaration was `no_changes_needed` or `blocked` — a PR
        // that merged after the worker gave up still finalizes to `done`.
        let declaration_required = self.feature_flags.is_enabled("worker_proposals")
            && self.feature_flags.is_enabled("run_done_proposals_seam");
        let (declared_delivered, declared_at_all) = if declaration_required {
            match self.work_db.execution_run_done_outcome(execution_id) {
                Ok(outcome) => (
                    outcome == Some(boss_protocol::RunDoneOutcome::Delivered),
                    outcome.is_some(),
                ),
                Err(err) => {
                    // Fails OPEN, matching every other proposals-first read
                    // in this subsystem: a storage error must not be able to
                    // hold a finished run open forever. The legacy inference
                    // carries the decision for this Stop and the WARN says
                    // it did.
                    tracing::warn!(
                        execution_id,
                        ?err,
                        "run_done gate: declaration lookup failed; allowing the legacy health-alone \
                         inference to carry this Stop rather than holding the run open",
                    );
                    (true, true)
                }
            }
        } else {
            (true, true)
        };

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
                    && (merge_conflict_revision
                        || queued_for_merge
                        || (matches!(open.ci, OpenPrCiStatus::Clean)
                            && health_alone_satisfies
                            && declared_delivered)) =>
            {
                // A health-alone finalize with no SHA baseline rests on a
                // precondition nothing verified — say so at WARN rather
                // than burying it in the ordinary finalize line. It stays
                // permitted (refusing it re-opens the stuck-revision dead
                // end, where a revision whose dispatch-time snapshot
                // failed can never be finalized by any Stop), but an
                // operator reading back a surprising completion must be
                // able to see which evidence carried it.
                if contribution == ContributionEvidence::Indeterminate
                    && !merge_conflict_revision
                    && !queued_for_merge
                    && execution.kind == ExecutionKind::RevisionImplementation
                {
                    tracing::warn!(
                        execution_id,
                        bound_pr_url,
                        kind = %execution.kind,
                        "satisfied-deliverable gate: finalizing a revision on bound-PR health alone \
                         with NO usable SHA-delta baseline — whether this run contributed was never \
                         established (see the sha-delta gate's own log line for why it was \
                         inconclusive)"
                    );
                }
                tracing::info!(
                    execution_id,
                    bound_pr_url,
                    kind = %execution.kind,
                    merge_conflict_revision,
                    ci_status = ?open.ci,
                    in_merge_queue = probe.in_merge_queue,
                    merge_queue_entry_state = ?probe.merge_queue_entry_state,
                    ?contribution,
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
            // Refused on the missing DECLARATION: the PR is open,
            // mergeable and CI-clean, and (unlike the arm below) nothing
            // contradicts the idea that this run delivered — but the run
            // has not said so, and the engine no longer says it on the
            // worker's behalf.
            //
            // Not a nudge and not a finalize: this returns
            // `AwaitingDeclaration`, which the caller routes to
            // [`crate::run_done_backstop`]. That is the whole behavioural
            // change — the state the incident's revisions were in when the
            // engine terminalized them 78 seconds in now holds the run
            // open, quietly, until either a declaration arrives or the
            // backstop's silence horizon says nobody is home.
            //
            // Reachable only when the CI-clean/health-alone predicate is
            // what would have satisfied the gate: the `merge_conflict_revision`
            // and `queued_for_merge` arms above match first and are
            // deliberately exempt from the declaration requirement.
            //
            // `!declared_at_all`, not `!declared_delivered`: a run that
            // declared `no_changes_needed` or `blocked` has answered the
            // question this arm would wait on. Sending it to the backstop
            // would hold it for an answer it already gave, so it skips this
            // arm and falls through as `NotSatisfied` to the paths that
            // handle those claims.
            PrLifecycleState::Open(ref open)
                if !declared_at_all
                    && health_alone_satisfies
                    && mergeability_satisfies_deliverable(open.mergeability, merge_conflict_revision)
                    && matches!(open.ci, OpenPrCiStatus::Clean) =>
            {
                RUN_DONE_GATE_HELD.inc(&self.metrics);
                tracing::info!(
                    execution_id,
                    bound_pr_url,
                    kind = %execution.kind,
                    ci_status = ?open.ci,
                    ?contribution,
                    "satisfied-deliverable gate: bound PR is open, mergeable and CI-clean, but this \
                     run has not declared itself done (`boss propose done`) — holding instead of \
                     finalizing on the PR's state, which is what this run was dispatched into"
                );
                return SatisfiedDeliverableOutcome::AwaitingDeclaration;
            }
            // Refused on EVIDENCE, not on the PR being unhealthy: the PR
            // is open, mergeable and CI-clean, but that is precisely the
            // state this revision was dispatched into and the SHA-delta
            // gate proved its head did not move. Loud, because it is the
            // difference between "delivered" and "never started", and
            // because the run this closes was finalized as delivered 1.7 s
            // after the engine logged that it had contributed nothing.
            PrLifecycleState::Open(ref open)
                if !health_alone_satisfies
                    && mergeability_satisfies_deliverable(open.mergeability, merge_conflict_revision)
                    && matches!(open.ci, OpenPrCiStatus::Clean) =>
            {
                tracing::warn!(
                    execution_id,
                    bound_pr_url,
                    kind = %execution.kind,
                    ci_status = ?open.ci,
                    ?contribution,
                    "satisfied-deliverable gate: bound PR is open, mergeable and CI-clean — but \
                     that is the state this revision was DISPATCHED INTO, and the sha-delta gate \
                     proved its head did not move this run. Refusing to finalize a run that \
                     contributed nothing; falling through to the nudge/park path, which suppresses \
                     for a worker still doing background work and otherwise bounds itself"
                );
                return SatisfiedDeliverableOutcome::NotSatisfied;
            }
            _ => {
                tracing::debug!(
                    execution_id,
                    bound_pr_url,
                    state = ?probe.state,
                    "satisfied-deliverable gate: PR not yet satisfied (CI in-flight / failing / conflict); falling through to nudge"
                );
                return SatisfiedDeliverableOutcome::NotSatisfied;
            }
        };

        // Map through DeliverableSatisfied so logs and tests can
        // identify this path distinctly from a fresh-push finalization.
        // ReviewerEnqueued is also a successful finalization — the PR
        // advanced to InReview and a reviewer pass was triggered; the
        // deliverable is still satisfied.
        SatisfiedDeliverableOutcome::Finalized(match inner {
            StopOutcome::PrDetected { pr_url }
            | StopOutcome::PrMerged { pr_url }
            | StopOutcome::ReviewerEnqueued { pr_url } => StopOutcome::DeliverableSatisfied { pr_url },
            other => other,
        })
    }

    /// Run the run-done backstop for an execution whose Stop reached
    /// [`SatisfiedDeliverableOutcome::AwaitingDeclaration`]: the bound PR
    /// looks satisfied but the worker has not declared its run finished.
    ///
    /// See [`crate::run_done_backstop`] for the full argument. In short:
    /// hold quietly while the worker shows life or the silence horizon has
    /// not elapsed, then ask (bounded by the existing circuit breaker),
    /// then — when the ask goes unanswered and the breaker parks the run —
    /// stamp `run_undeclared_at` and file a distinct attention so the
    /// ending is legible as "we never found out" rather than as a success.
    ///
    /// Never finalizes and never reaps. The only terminal-ish outcome it
    /// can reach is a park, which leaves the execution `waiting_human` for
    /// a human to redirect or re-dispatch.
    pub(super) async fn await_run_done_declaration(
        &self,
        execution: &crate::work::WorkExecution,
        bound_pr_url: &str,
    ) -> StopOutcome {
        let descendant_count = self.background_activity_probe.live_descendant_count(&execution.id);
        let decision = crate::run_done_backstop::decide(
            &self.run_done_silence_tracker,
            &execution.id,
            &execution.kind,
            descendant_count,
            boss_engine_utils::epoch_time::now_epoch_secs(),
        );
        match decision {
            crate::run_done_backstop::BackstopDecision::HoldingWorkerActive { descendant_count } => {
                tracing::info!(
                    execution_id = %execution.id,
                    bound_pr_url,
                    descendant_count,
                    "run_done backstop: no declaration yet, but the worker's process tree still has \
                     live descendants — it is working, not stalled. Holding quietly (no probe, no \
                     breaker, no finalize)",
                );
                StopOutcome::AwaitingRunDoneDeclaration {
                    reason: format!("worker has {descendant_count} live background child process(es)"),
                }
            }
            crate::run_done_backstop::BackstopDecision::HoldingWithinHorizon {
                waited_secs,
                horizon_secs,
            } => {
                tracing::debug!(
                    execution_id = %execution.id,
                    bound_pr_url,
                    waited_secs,
                    horizon_secs,
                    kind = %execution.kind,
                    "run_done backstop: no declaration yet and no sign of activity, but within the \
                     silence horizon — holding quietly",
                );
                StopOutcome::AwaitingRunDoneDeclaration {
                    reason: format!("silent for {waited_secs}s of a {horizon_secs}s declaration horizon"),
                }
            }
            crate::run_done_backstop::BackstopDecision::Ask {
                waited_secs,
                horizon_secs,
            } => {
                RUN_DONE_BACKSTOP_ASKED.inc(&self.metrics);
                tracing::warn!(
                    execution_id = %execution.id,
                    bound_pr_url,
                    waited_secs,
                    horizon_secs,
                    kind = %execution.kind,
                    "run_done backstop: silence horizon elapsed with no `boss propose done` \
                     declaration and no sign of worker activity — asking the worker whether it is \
                     finished",
                );
                // The fingerprint is keyed on the *question*, not on any PR
                // state, so the breaker counts "asks that changed nothing"
                // — which is exactly the unproductive thing here. Keying it
                // on the PR URL (as the push-to-existing nudge does) would
                // collide with that nudge's own fingerprint and let one
                // path's asks consume the other's cap.
                let outcome = self
                    .nudge_or_park(
                        execution,
                        crate::run_done_backstop::PROBE_DECLARE_DONE,
                        &format!("run_done_undeclared:{}", execution.id),
                        Some(bound_pr_url),
                        StopOutcome::AwaitingRunDoneDeclaration {
                            reason: format!("asked for a declaration after {waited_secs}s of silence"),
                        },
                    )
                    .await;
                match outcome {
                    StopOutcome::NudgeBreakerParked { reason } => {
                        self.record_run_undeclared_park(execution, bound_pr_url, waited_secs, &reason)
                            .await;
                        StopOutcome::RunUndeclaredParked { reason, waited_secs }
                    }
                    other => other,
                }
            }
        }
    }

    /// Stamp and surface a park that happened because a run never declared
    /// itself done.
    ///
    /// Two records, because the requirement is that this ending be
    /// distinguishable from a declared one *both* in stored state and to a
    /// human looking at the row:
    ///
    /// - `work_executions.run_undeclared_at` — the durable, queryable half.
    ///   A declared run has `run_done_declared_at`/`run_done_outcome` set
    ///   and this NULL; a backstopped run the reverse. Neither is inferred
    ///   from the absence of the other.
    /// - a [`RUN_UNDECLARED_ATTENTION_KIND`] attention — the human half,
    ///   distinct from the nudge-breaker item the park itself files
    ///   because "we never found out whether this run finished" is a
    ///   different thing to tell someone than "we asked and nothing
    ///   changed".
    ///
    /// Both are best-effort: a failure here must not turn a park into an
    /// unhandled error, but it is logged loudly, because a park whose
    /// provenance was not recorded is exactly the silent ending this
    /// design exists to eliminate.
    async fn record_run_undeclared_park(
        &self,
        execution: &crate::work::WorkExecution,
        bound_pr_url: &str,
        waited_secs: i64,
        park_reason: &str,
    ) {
        RUN_DONE_BACKSTOP_PARKED.inc(&self.metrics);
        if let Err(err) = self.work_db.mark_execution_run_undeclared(&execution.id) {
            tracing::error!(
                execution_id = %execution.id,
                ?err,
                "run_done backstop: failed to stamp run_undeclared_at — this park will be \
                 indistinguishable in stored state from a declared completion",
            );
        }
        let already_filed = self
            .work_db
            .list_attention_items(&execution.id)
            .map(|items| {
                items
                    .iter()
                    .any(|i| i.kind == RUN_UNDECLARED_ATTENTION_KIND && i.status != "resolved")
            })
            .unwrap_or(false);
        if already_filed {
            return;
        }
        let body = format!(
            "This run ended without the worker ever declaring it finished.\n\n\
             - execution: `{execution_id}`\n\
             - work item: `{work_item_id}`\n\
             - bound PR: {bound_pr_url}\n\
             - silent for: {waited_secs}s before the engine asked\n\n\
             The engine asked the worker to run `boss propose done` and got no declaration back \
             before the auto-nudge breaker tripped ({park_reason}).\n\n\
             **This is NOT a completion.** The run is parked, not `completed`, precisely because \
             nobody established that it finished: the bound PR's state cannot answer that question \
             for a run dispatched into an already-open PR, and the engine no longer pretends \
             otherwise. Decide what actually happened — read the transcript, check whether the PR \
             carries this run's work — and either re-dispatch or close it by hand.",
            execution_id = execution.id,
            work_item_id = execution.work_item_id,
        );
        if let Err(err) = self
            .file_execution_attention(
                execution,
                RUN_UNDECLARED_ATTENTION_KIND,
                "Run ended without declaring itself done",
                body,
            )
            .await
        {
            tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "run_done backstop: failed to file the undeclared-run attention item (non-fatal)",
            );
        }
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
