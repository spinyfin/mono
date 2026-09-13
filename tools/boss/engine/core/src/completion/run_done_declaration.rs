//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only for the shared types —
//! see [`super`] for the handler struct, shared types, traits, and free
//! helpers this module reaches via `use super::*`.
//!
//! ## Why this module exists
//!
//! `run_done` used to be a signal: `apply_run_done`
//! (`crate::work::proposal_apply`) stamped `run_done_declared_at` /
//! `run_done_outcome` on the execution row and returned — nothing re-entered
//! the completion path. The declaration was then read later as one conjunct
//! of the satisfied-deliverable gate's health-alone arm
//! (`WorkerCompletionHandler::evaluate_satisfied_deliverable_on_stop`), and
//! only for a run with a bound PR. A worker could declare completion
//! successfully, have that declaration applied, and still never finalize:
//! nothing guarantees another Stop boundary ever arrives to read the stamp.
//! Two executions did exactly that in one night, holding worker slots and
//! cube leases until a human killed them.
//!
//! [`WorkerCompletionHandler::finalize_declared_run_done`] is the fix, and it
//! is the same fix `review_report` / `review_verdict` acceptance already
//! applies (`app::proposals::handle_submit_proposal`'s
//! `finalize_reporting_member` block): the declaration IS the completion
//! signal, applied synchronously at submit — never a stamp waiting on a
//! driver-specific turn boundary that may never come.
//!
//! ## The hard rule this module is built around
//!
//! No check that can make a network call may sit in the termination path.
//! Both production incidents were a worker's declaration applying cleanly
//! and the *next* thing that had to happen — a `gh` fetch inside the
//! Stop-boundary satisfied-deliverable gate — failing or hanging. Moving the
//! declaration off the Stop boundary and onto the submission path only fixes
//! that if the submission path itself never blocks on GitHub. So:
//!
//! - Resolving the PR a `delivered` declaration binds to
//!   ([`WorkerCompletionHandler::resolve_declared_pr_url_no_network`]) reads
//!   only already-stored state: the task/chore's own `pr_url`, the
//!   revision-chain-root lookup, and the in-memory hook-stream staging cache
//!   / structured-output artifact this run itself already produced. It does
//!   NOT call the Layer-2 branch-verification check
//!   [`WorkerCompletionHandler::verified_staged_pr_url`] uses on the
//!   Stop-boundary path, because that check is a `gh` call.
//! - The actual termination writes
//!   ([`crate::work::WorkDb::record_worker_pr_completion`],
//!   [`WorkerCompletionHandler::finalize_no_op_completion`],
//!   [`WorkerCompletionHandler::finalize_idle_park`]) and the teardown
//!   sequence ([`WorkerCompletionHandler::finish_worker_teardown`]) touch
//!   only the local DB, the pane, the driver's local workspace state, and
//!   the `cube` CLI (a local subprocess, not GitHub).
//!
//! Evidence checks do not disappear — they become a **post-hoc audit**,
//! spawned after the execution is already terminal and torn down
//! ([`WorkerCompletionHandler::spawn_declared_delivery_audit`]), so a slow or
//! failing GitHub call can delay a flagged attention item, never a slot
//! release. A declaration the audit contradicts does not un-terminalize
//! anything; it only files
//! [`crate::completion::RUN_DONE_AUDIT_FLAGGED_ATTENTION_KIND`] for a human.
//!
//! ## What still runs at a Stop boundary
//!
//! This module does not touch the satisfied-deliverable gate, the run-done
//! backstop (`crate::run_done_backstop`), or the SHA-delta gate — all three
//! stay exactly as designed, in `completion::metadata_gate` /
//! `completion::stop`. They matter for:
//!
//! - a run that crashes between `apply_run_done` committing and this
//!   module's finalize call returning. Submission finalizes regardless of
//!   response delivery, so acknowledgement loss is not a recovery boundary;
//! - a run that never calls `boss propose done` at all — the backstop's
//!   hold/ask/park sequence is unchanged and is what catches those.

use super::*;

impl WorkerCompletionHandler {
    /// Finalize a `run_done` declaration synchronously, at submit — the
    /// counterpart to `on_stop`/`on_stop_inner` for every other completion
    /// path, but reached from `app::proposals::handle_submit_proposal`
    /// instead of a driver Stop hook. See the module doc for why this must
    /// never make a network call.
    ///
    /// `outcome` is the worker's own declared
    /// [`boss_protocol::RunDoneOutcome`] — already durably stamped onto the
    /// execution row by `apply_run_done` in the same transaction the caller
    /// awaited to commit before calling this.
    ///
    /// Returns [`StopOutcome::AlreadyTerminal`] when the execution is not
    /// live (already finalized by a racing path, or — in principle only,
    /// since attribution requires a registered live worker — never started).
    /// Idempotent: every underlying write re-checks liveness inside its own
    /// transaction, so a replayed call (the same idempotency key resubmitted
    /// after a crash between apply and this call) safely no-ops instead of
    /// double-tearing-down.
    pub async fn finalize_declared_run_done(
        &self,
        execution_id: &str,
        outcome: boss_protocol::RunDoneOutcome,
    ) -> StopOutcome {
        let execution = match self.work_db.get_execution(execution_id) {
            Ok(execution) => execution,
            Err(err) => {
                tracing::error!(execution_id, ?err, "run_done finalize: execution lookup failed");
                return StopOutcome::DbError;
            }
        };
        if !execution.status.is_live() {
            return StopOutcome::AlreadyTerminal;
        }

        match outcome {
            boss_protocol::RunDoneOutcome::NoChangesNeeded => {
                let (contribution, attention) = self.declared_run_done_no_op_inputs(&execution);
                self.finalize_no_op_completion(&execution, contribution, attention)
                    .await
            }
            boss_protocol::RunDoneOutcome::Delivered => self.finalize_declared_delivery(&execution).await,
            boss_protocol::RunDoneOutcome::Blocked => self.finalize_declared_blocked(&execution).await,
        }
    }

    /// Resolve the PR a `delivered` declaration binds to, from sources that
    /// are all already-stored state — never a network call. In order:
    ///
    /// 1. The task/chore's own bound PR ([`Self::resolve_bound_pr_url`]:
    ///    `task.pr_url`, or `execution.pr_url` / the chain-root lookup for a
    ///    revision) — DB reads only.
    /// 2. The in-memory hook-stream staging cache, if a *publish-armed*
    ///    observation was captured this run (a `PostToolUse` `gh pr
    ///    create`/`cube pr create|update` hit) — an in-memory lookup, no I/O.
    /// 3. The structured-output PR-URL artifact the worker itself wrote this
    ///    run ([`Self::stage_pr_url_from_artifact`]) — a local file read.
    ///
    /// Deliberately does NOT call [`Self::verified_staged_pr_url`] (the
    /// Stop-boundary path's Layer-2 branch-name check) or the driver-prose
    /// transcript fallback: both exist on the Stop path as defense-in-depth
    /// for a signal this function's callers don't need to fully trust ahead
    /// of time — an unverified staged/artifact URL that turns out wrong is
    /// exactly what [`Self::spawn_declared_delivery_audit`]'s post-hoc check
    /// exists to catch, off the termination path.
    fn resolve_declared_pr_url_no_network(&self, execution: &crate::work::WorkExecution) -> Option<String> {
        if let Some(url) = self.resolve_bound_pr_url(execution) {
            return Some(url);
        }
        if let Some(entry) = self.staged_pr_urls.get_entry(&execution.id)
            && entry.finalization_armed
        {
            return Some(entry.pr_url);
        }
        if self.stage_pr_url_from_artifact(execution)
            && let Some(entry) = self.staged_pr_urls.get_entry(&execution.id)
        {
            return Some(entry.pr_url);
        }
        None
    }

    /// `delivered`: terminalize against a resolvable PR
    /// ([`Self::resolve_declared_pr_url_no_network`]), or fall back to
    /// [`Self::finalize_declared_delivery_without_pr`] when none exists.
    ///
    /// Mirrors [`Self::finalize_pr_transition`]'s termination write and
    /// teardown exactly, but never makes a network call inline — see the
    /// module doc. Every network-touching step that function layers on top
    /// (reviewer-batch enqueue, doc-link detection) instead runs in
    /// [`Self::spawn_declared_delivery_post_effects`], the same best-effort,
    /// fire-and-forget background spawn [`Self::spawn_declared_delivery_audit`]
    /// already uses: the execution is already terminal and torn down by the
    /// time either task runs, so a slow or failing `gh` call can delay those
    /// side effects, never a slot release.
    async fn finalize_declared_delivery(&self, execution: &crate::work::WorkExecution) -> StopOutcome {
        let Some(pr_url) = self.resolve_declared_pr_url_no_network(execution) else {
            return self.finalize_declared_delivery_without_pr(execution).await;
        };

        // Captured before `record_worker_pr_completion` nulls
        // `workspace_path` in the same transaction that terminalizes the
        // execution — this path owns driver teardown.
        let workspace_path = execution.workspace_path.clone();
        // Marked before the terminalizing write — see `super::teardown`.
        let teardown = self.begin_teardown(&execution.id);
        // The local completion write must preserve the same hold used by
        // Stop-triggered reviewer admission. Network-dependent refinement
        // and enqueue happen after teardown below.
        let reviewer_triggering = should_enqueue_reviewer_for_primary(&execution.kind)
            || (execution.kind == ExecutionKind::RevisionImplementation && self.enable_revision_triggered_reviews);
        let target = if reviewer_triggering {
            WorkerPrCompletionTarget::PendingReview
        } else {
            WorkerPrCompletionTarget::InReview
        };
        let completion =
            match self
                .work_db
                .record_worker_pr_completion(&execution.id, &pr_url, None, None, target, None)
            {
                Ok(Some(completion)) => completion,
                Ok(None) => return StopOutcome::AlreadyTerminal,
                Err(err) => {
                    tracing::error!(
                        execution_id = %execution.id,
                        ?err,
                        "run_done finalize (delivered): failed to record PR completion",
                    );
                    return StopOutcome::DbError;
                }
            };
        self.staged_pr_urls.forget(&execution.id);
        self.nudge_breaker.forget(&execution.id);
        self.build_wait_tracker.forget(&execution.id);
        self.background_children_tracker.forget(&execution.id);
        self.hold_registry.release(&execution.id);
        self.finish_worker_teardown(
            &execution.id,
            &completion.execution.work_item_id,
            completion.released_lease_id.as_deref(),
            workspace_path.as_deref().map(std::path::Path::new),
            "run_done_declared",
            teardown,
        )
        .await;
        let product_id = completion.work_item.product_id().to_string();
        let work_item_id = completion.execution.work_item_id.clone();
        self.publisher
            .publish(
                &completion.execution.id,
                &work_item_id,
                completion.execution.status.as_str(),
                "worker_run_done_delivered",
            )
            .await;
        self.publisher
            .publish_work_item_changed(&product_id, &work_item_id, "worker_run_done_delivered")
            .await;
        tracing::info!(
            execution_id = %execution.id,
            work_item_id = %work_item_id,
            pr_url = %pr_url,
            "run_done finalize: declared `delivered`, bound to a resolvable PR — terminalized at submit",
        );

        self.spawn_declared_delivery_audit(execution, &work_item_id, &pr_url);
        self.spawn_declared_delivery_post_effects(execution, &work_item_id, &pr_url);

        StopOutcome::PrDetected { pr_url }
    }

    /// `delivered` with no PR the engine could resolve without a network
    /// call. Terminalizes anyway — the declaration is definitive, not a
    /// claim the engine verifies before accepting — but leaves the
    /// task/chore's own status untouched (there is nothing to bind it to)
    /// and flags the mismatch for a human via
    /// [`crate::completion::RUN_DONE_AUDIT_FLAGGED_ATTENTION_KIND`].
    ///
    /// Reuses [`Self::finalize_idle_park`]'s exact mechanics — `abandoned`,
    /// lease/pane released, `autostart` cleared so the rescan does not
    /// immediately re-dispatch onto a task whose worker just made an
    /// unverifiable claim — because that is precisely the right shape here:
    /// there is no positive evidence to advance the task on, only a
    /// worker-declared end to the run.
    async fn finalize_declared_delivery_without_pr(&self, execution: &crate::work::WorkExecution) -> StopOutcome {
        let detail = format!(
            "Execution `{}` declared `boss propose done --outcome delivered`, but the engine \
             could not resolve any PR for it without a network call — no bound task/chore \
             `pr_url`, no `execution.pr_url` (revision chain root), and nothing staged from the \
             hook stream or structured-output artifact this run. The declaration is still \
             terminal: the run has ended and its slot and lease are released. The task/chore's \
             own status is left untouched pending review.",
            execution.id
        );
        self.finalize_idle_park(execution, &detail).await;
        if let Err(err) = self
            .file_execution_attention(
                execution,
                RUN_DONE_AUDIT_FLAGGED_ATTENTION_KIND,
                "Declared `delivered` with no PR the engine could resolve",
                detail.clone(),
            )
            .await
        {
            tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "run_done finalize (delivered, no PR): failed to file attention item",
            );
        }
        tracing::warn!(
            execution_id = %execution.id,
            "run_done finalize: declared `delivered` but no PR was resolvable — terminalized \
             without a task-status change",
        );
        StopOutcome::RunDoneDeclaredWithoutDelivery { detail }
    }

    /// `blocked`: the run is over without delivering. Terminalizes with the
    /// same idle-park mechanics as
    /// [`Self::finalize_declared_delivery_without_pr`] and for the same
    /// reason — no positive evidence to advance the task/chore on — but
    /// files [`crate::completion::RUN_DONE_BLOCKED_ATTENTION_KIND`] instead,
    /// distinct from the companion (still-live-run) `blocked` proposal's own
    /// [`crate::worker_escalation::WORKER_BLOCKED_ATTENTION_KIND`] attention: that one
    /// says "the run hit a blocker and is asking for help while it keeps
    /// going", this one says "the run itself has ended".
    async fn finalize_declared_blocked(&self, execution: &crate::work::WorkExecution) -> StopOutcome {
        let detail = format!(
            "Execution `{}` declared `boss propose done --outcome blocked` — the run is over \
             without delivering. If a companion `boss propose blocked --reason ...` proposal was \
             also submitted, its own attention item carries the blocker's explanation. The \
             declaration is terminal: the run has ended and its slot and lease are released. The \
             task/chore's own status is left untouched, and `autostart` has been cleared so the \
             automated rescan will not immediately re-dispatch a replacement worker onto it.",
            execution.id
        );
        self.finalize_idle_park(execution, &detail).await;
        if let Err(err) = self
            .file_execution_attention(
                execution,
                RUN_DONE_BLOCKED_ATTENTION_KIND,
                "Run ended: worker declared itself blocked",
                detail.clone(),
            )
            .await
        {
            tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "run_done finalize (blocked): failed to file attention item",
            );
        }
        tracing::warn!(
            execution_id = %execution.id,
            "run_done finalize: declared `blocked` — terminalized without a task-status change",
        );
        StopOutcome::RunDoneDeclaredWithoutDelivery { detail }
    }

    /// Post-hoc audit for a `delivered` declaration that WAS terminalized
    /// against a resolvable PR: spawned entirely off the termination path
    /// (the execution is already terminal, its pane and lease already
    /// released, by the time this task ever polls anything), so a slow or
    /// failing GitHub call can delay a flagged attention item, never a slot
    /// release. Best-effort and fire-and-forget: this handler doesn't even
    /// observe whether the spawned task finished.
    ///
    /// No-ops when this execution has no `pr_head_before` dispatch-time
    /// snapshot to compare against — there is nothing to audit against, and
    /// silently accepting the declaration is correct (refusing would only
    /// punish executions that predate reliable snapshotting).
    fn spawn_declared_delivery_audit(&self, execution: &crate::work::WorkExecution, work_item_id: &str, pr_url: &str) {
        let Some(pr_head_before) = execution.pr_head_before.clone().filter(|s| !s.is_empty()) else {
            return;
        };
        let branch_verifier = self.branch_verifier.clone();
        let work_db = self.work_db.clone();
        let publisher = self.publisher.clone();
        let execution_id = execution.id.clone();
        let work_item_id = work_item_id.to_owned();
        let repo_remote_url = execution.repo_remote_url.clone();
        let pr_url = pr_url.to_owned();
        tokio::spawn(async move {
            audit_declared_delivery(
                branch_verifier.as_ref(),
                &work_db,
                publisher.as_ref(),
                &execution_id,
                &work_item_id,
                &repo_remote_url,
                &pr_head_before,
                &pr_url,
            )
            .await;
        });
    }

    /// Best-effort, fire-and-forget background spawn for the side effects
    /// [`Self::finalize_declared_delivery`] must not perform inline: the
    /// reviewer-batch enqueue and doc-link detection
    /// [`Self::finalize_pr_transition`] layers onto the same termination
    /// write on the Stop-triggered path. It also reconciles the documented
    /// design-question and follow-up fallback channels after teardown.
    fn spawn_declared_delivery_post_effects(
        &self,
        execution: &crate::work::WorkExecution,
        work_item_id: &str,
        pr_url: &str,
    ) {
        let work_db = Arc::clone(&self.work_db);
        let publisher = Arc::clone(&self.publisher);
        let branch_verifier = Arc::clone(&self.branch_verifier);
        let review_batch_enqueuer = Arc::clone(&self.review_batch_enqueuer);
        let feature_flags = Arc::clone(&self.feature_flags);
        let execution_kind = execution.kind.clone();
        let enable_revision_triggered_reviews = self.enable_revision_triggered_reviews;
        let max_review_cycles = self.max_review_cycles;
        let min_review_changed_lines = self.min_review_changed_lines;
        let review_pool_size = self.review_pool_size;
        let structured_output_dir = self.structured_output_dir.clone();
        let execution = execution.clone();
        let work_item_id = work_item_id.to_owned();
        let repo_remote_url = execution.repo_remote_url.clone();
        let pr_url = pr_url.to_owned();
        tokio::spawn(async move {
            run_declared_delivery_post_effects(
                &work_db,
                publisher.as_ref(),
                branch_verifier.as_ref(),
                review_batch_enqueuer.as_ref(),
                &feature_flags,
                execution_kind,
                enable_revision_triggered_reviews,
                max_review_cycles,
                min_review_changed_lines,
                review_pool_size,
                &structured_output_dir,
                &execution,
                &work_item_id,
                &repo_remote_url,
                &pr_url,
            )
            .await;
        });
    }
}

/// The body of [`WorkerCompletionHandler::spawn_declared_delivery_audit`]'s
/// background task: a free function (not a method) so it needs only the
/// specific cloned collaborators it touches, not a whole `Arc<Self>` — the
/// same shape [`WorkerCompletionHandler::finalize_pr_transition`]'s own
/// on-transition CI pre-fetch spawn already uses.
///
/// Compares the bound PR's current head against this run's dispatch-time
/// snapshot, the same comparison [`WorkerCompletionHandler::evaluate_sha_delta_gate`]
/// makes on the Stop-boundary path. Unchanged means the declaration is
/// contradicted by the evidence — nothing this execution did moved the PR —
/// so a flagged attention is filed. Any failure along the way (unparseable
/// repo/PR, a failed `gh` call) is logged at DEBUG and swallowed: this is a
/// best-effort audit, not a correctness requirement, and the execution it
/// would have flagged is already terminal regardless.
#[allow(clippy::too_many_arguments)]
pub(super) async fn audit_declared_delivery(
    branch_verifier: &dyn BranchVerifier,
    work_db: &crate::work::WorkDb,
    publisher: &dyn ExecutionPublisher,
    execution_id: &str,
    work_item_id: &str,
    repo_remote_url: &str,
    pr_head_before: &str,
    pr_url: &str,
) {
    let repo_slug = match parse_repo_slug(repo_remote_url) {
        Ok(slug) => slug,
        Err(err) => {
            tracing::debug!(
                execution_id,
                ?err,
                "run_done audit: cannot parse repo slug; skipping audit"
            );
            return;
        }
    };
    let Some(pr_number) = pr_number_from_url(pr_url) else {
        tracing::debug!(
            execution_id,
            pr_url,
            "run_done audit: cannot parse PR number; skipping audit"
        );
        return;
    };
    let head_now = match branch_verifier.fetch_pr_head_oid(&repo_slug, pr_number).await {
        Ok(oid) => oid,
        Err(err) => {
            tracing::debug!(
                execution_id,
                ?err,
                "run_done audit: head fetch failed; skipping audit for this declaration",
            );
            return;
        }
    };
    if head_now != pr_head_before {
        return;
    }
    tracing::warn!(
        execution_id,
        work_item_id,
        pr_url,
        "run_done audit: execution declared `delivered` but the bound PR's head is unchanged \
         from this run's dispatch-time snapshot — filing a flagged attention for human review",
    );
    let body = format!(
        "This execution declared `boss propose done --outcome delivered`, and the run was \
         finalized on that declaration immediately — the declaration is the completion signal, \
         never a gate the engine held open pending verification (see the design doc's \"Run \
         completion\" section). A post-hoc audit then compared the bound PR's head against this \
         run's dispatch-time snapshot and found **no movement**: {pr_url} is still at \
         `{pr_head_before}`.\n\n\
         This does not undo the completion — the execution has already ended and its slot/lease \
         were already released — but the declaration itself looks contradicted by the evidence. \
         Read the transcript and decide whether the work actually landed."
    );
    match work_db.create_attention_item(CreateAttentionItemInput {
        execution_id: Some(execution_id.to_owned()),
        work_item_id: None,
        kind: RUN_DONE_AUDIT_FLAGGED_ATTENTION_KIND.to_owned(),
        status: None,
        title: "Declared `delivered` but the PR head did not move".to_owned(),
        body_markdown: body,
        resolved_at: None,
    }) {
        Ok(item) => {
            if let Ok(work_item) = work_db.get_work_item(work_item_id) {
                let product_id = work_item.product_id().to_string();
                publisher
                    .publish_frontend_event_on_product(&product_id, FrontendEvent::AttentionItemCreated { item })
                    .await;
            }
        }
        Err(err) => tracing::warn!(
            execution_id,
            ?err,
            "run_done audit: failed to file flagged attention item"
        ),
    }
}

/// Body of [`WorkerCompletionHandler::spawn_declared_delivery_post_effects`]'s
/// background task: reviewer-batch enqueue, then doc-link detection. A free
/// function (not a method) for the same reason [`audit_declared_delivery`]
/// is one — it needs only the specific cloned collaborators it touches, not
/// a whole `Arc<Self>`.
#[allow(clippy::too_many_arguments)]
async fn run_declared_delivery_post_effects(
    work_db: &crate::work::WorkDb,
    publisher: &dyn ExecutionPublisher,
    branch_verifier: &dyn BranchVerifier,
    review_batch_enqueuer: &dyn ReviewBatchEnqueuer,
    feature_flags: &crate::feature_flags::FeatureFlagsStore,
    execution_kind: ExecutionKind,
    enable_revision_triggered_reviews: bool,
    max_review_cycles: usize,
    min_review_changed_lines: u64,
    review_pool_size: usize,
    structured_output_dir: &std::path::Path,
    execution: &crate::work::WorkExecution,
    work_item_id: &str,
    repo_remote_url: &str,
    pr_url: &str,
) {
    let reviewer_triggering = should_enqueue_reviewer_for_primary(&execution_kind)
        || (execution_kind == ExecutionKind::RevisionImplementation && enable_revision_triggered_reviews);
    if reviewer_triggering {
        maybe_enqueue_declared_delivery_reviewer(
            work_db,
            publisher,
            branch_verifier,
            review_batch_enqueuer,
            feature_flags,
            max_review_cycles,
            min_review_changed_lines,
            review_pool_size,
            execution,
            work_item_id,
            repo_remote_url,
            pr_url,
        )
        .await;
    }
    run_declared_delivery_doc_link_detection(
        work_db,
        publisher,
        feature_flags,
        structured_output_dir,
        execution,
        work_item_id,
        pr_url,
    )
    .await;
}

/// Release the `PendingReview` hold [`WorkerCompletionHandler::finalize_declared_delivery`]
/// took, using `cycle_root_id` as the verdict source — this arm's decision
/// not to enqueue a reviewer rests on a `pr_review_verdicts` row (the cycle
/// bound or a no-op skip both require `review_cycle > 0`, i.e. at least one
/// prior review ran and recorded a verdict), and for a revision that verdict
/// lives on the review-cycle root, not the task row itself (see
/// [`crate::work::WorkDb::advance_pending_review_task_to_in_review_with_verdict_source`]).
/// Logs at `warn!` instead of silently discarding the result: a release that
/// fails to match leaves the task stranded in `active` with no live
/// execution and nothing left to un-stick it.
fn release_pending_review_hold_with_verdict(work_db: &crate::work::WorkDb, work_item_id: &str, cycle_root_id: &str) {
    match work_db.advance_pending_review_task_to_in_review_with_verdict_source(work_item_id, cycle_root_id) {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            work_item_id,
            cycle_root_id,
            "run_done post-effects: PendingReview hold did not release (no matching verdict under \
             cycle_root_id, or a live non-review execution is blocking) — task remains stranded in \
             `active`",
        ),
        Err(err) => tracing::warn!(
            work_item_id,
            cycle_root_id,
            ?err,
            "run_done post-effects: failed to release PendingReview hold",
        ),
    }
}

/// Release the `PendingReview` hold with no verdict-existence requirement —
/// for arms whose decision not to enqueue a reviewer is NOT justified by an
/// already-recorded verdict: a first delivery (`review_cycle == 0`, no
/// verdict can exist yet) or a legacy-reviewer-creation failure. See
/// [`crate::work::WorkDb::advance_held_pending_review_task_to_in_review`].
fn release_pending_review_hold_unconditional(work_db: &crate::work::WorkDb, work_item_id: &str) {
    match work_db.advance_held_pending_review_task_to_in_review(work_item_id) {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            work_item_id,
            "run_done post-effects: PendingReview hold did not release (task not active+pr_url, or a \
             live non-review execution is blocking) — task remains stranded in `active`",
        ),
        Err(err) => tracing::warn!(
            work_item_id,
            ?err,
            "run_done post-effects: failed to release PendingReview hold",
        ),
    }
}

/// Reviewer-enqueue decision for a declared-delivery completion — the
/// background counterpart to the inline gate in
/// [`super::pr_transition::WorkerCompletionHandler::finalize_pr_transition`].
/// Shares the exact pure-rebase and no-op skip rules that gate uses
/// ([`super::finalize_passes::pure_rebase_skip_gate`],
/// [`super::finalize_passes::noop_skip_reason`]) so the two completion paths
/// can never silently diverge on the same input.
#[allow(clippy::too_many_arguments)]
pub(super) async fn maybe_enqueue_declared_delivery_reviewer(
    work_db: &crate::work::WorkDb,
    publisher: &dyn ExecutionPublisher,
    branch_verifier: &dyn BranchVerifier,
    review_batch_enqueuer: &dyn ReviewBatchEnqueuer,
    feature_flags: &crate::feature_flags::FeatureFlagsStore,
    max_review_cycles: usize,
    min_review_changed_lines: u64,
    review_pool_size: usize,
    execution: &crate::work::WorkExecution,
    work_item_id: &str,
    repo_remote_url: &str,
    pr_url: &str,
) {
    let cycle_root_id = work_db.review_cycle_root_id(work_item_id);
    let (review_cycle, last_reviewed_sha) = match work_db.get_task_review_cycle_state(&cycle_root_id) {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!(
                work_item_id,
                cycle_root_id,
                ?err,
                "run_done post-effects: could not read review_cycle; assuming bound not reached",
            );
            (0i64, None)
        }
    };

    // No-op / trivial-diff / pure-rebase skip gate, in the same order
    // `finalize_pr_transition` runs it: `pure_rebase_skip_gate` first (it is
    // independent of `review_cycle` / `last_reviewed_sha`, so it also
    // catches a pure rebase landing before the PR's very first review),
    // then `noop_skip_reason` for the sha_unchanged / empty_diff /
    // trivial_diff rules.
    let pure_rebase_gate =
        super::finalize_passes::pure_rebase_skip_gate(work_db, branch_verifier, pr_url, execution, &cycle_root_id)
            .await;
    let noop_skip_reason = match pure_rebase_gate.skip_reason {
        Some(reason) => Some(reason),
        None => {
            super::finalize_passes::noop_skip_reason(
                branch_verifier,
                pr_url,
                execution,
                review_cycle,
                last_reviewed_sha.as_deref(),
                pure_rebase_gate.post_head,
                min_review_changed_lines,
            )
            .await
        }
    };

    if let Some(skip_reason) = noop_skip_reason {
        tracing::info!(
            work_item_id,
            skip_reason,
            "run_done post-effects: pr_review noop skip; advancing to in_review without reviewer pass",
        );
        release_pending_review_hold_with_verdict(work_db, work_item_id, &cycle_root_id);
        return;
    }

    if (review_cycle as usize) >= max_review_cycles {
        tracing::info!(
            work_item_id,
            max_review_cycles,
            "run_done post-effects: pr_review cycle bound reached; skipping reviewer",
        );
        let _ = work_db.create_attention_item(CreateAttentionItemInput {
            work_item_id: Some(work_item_id.to_owned()),
            kind: "pr_review_cycle_bound".to_owned(),
            title: format!("Automated reviewer: cycle limit ({max_review_cycles}) reached"),
            body_markdown: format!(
                "The automated reviewer completed {max_review_cycles} cycle(s) on this PR \
                 without resolving all findings. The PR has been advanced to human Review.\n\n\
                 See the most recent revision task for the outstanding findings from the last \
                 automated review cycle."
            ),
            execution_id: None,
            status: None,
            resolved_at: None,
        });
        release_pending_review_hold_with_verdict(work_db, work_item_id, &cycle_root_id);
        return;
    }

    if feature_flags.is_enabled("review_batch_fanout") {
        match review_batch_enqueuer
            .enqueue(work_db, work_item_id, repo_remote_url, pr_url, review_pool_size)
            .await
        {
            Ok(crate::work::ReviewBatchDispatch::AdmissionDeferred)
            | Ok(crate::work::ReviewBatchDispatch::AlreadyReviewed) => {
                file_admission_deferred_attention(work_db, work_item_id, pr_url);
            }
            Ok(dispatch) => {
                if execution.kind == ExecutionKind::RevisionImplementation {
                    let target_sha = match &dispatch {
                        crate::work::ReviewBatchDispatch::Created { batch, .. }
                        | crate::work::ReviewBatchDispatch::ExistingBatch { batch, .. } => {
                            Some(batch.target_sha.as_str())
                        }
                        _ => None,
                    };
                    if let Some(target_sha) = target_sha
                        && let Err(err) = work_db.set_revision_stop_contributed_head(&execution.id, target_sha)
                    {
                        tracing::warn!(
                            execution_id = %execution.id,
                            target_sha,
                            ?err,
                            "run_done post-effects: failed to stamp revision contributed head for review batch",
                        );
                    }
                }
                tracing::info!(
                    work_item_id,
                    pr_url,
                    "run_done post-effects: review batch enqueued ({dispatch:?})"
                );
                publisher.kick_scheduler();
            }
            Err(error) => {
                tracing::warn!(
                    work_item_id,
                    ?error,
                    "run_done post-effects: failed to create immutable review batch; falling back to legacy reviewer",
                );
                if let Err(err) = work_db.create_pr_review_execution_dedup(work_item_id, repo_remote_url) {
                    tracing::warn!(
                        work_item_id,
                        ?err,
                        "run_done post-effects: failed to create legacy reviewer after batch failure",
                    );
                    release_pending_review_hold_unconditional(work_db, work_item_id);
                } else {
                    publisher.kick_scheduler();
                }
            }
        }
    } else {
        match work_db.create_pr_review_execution_dedup(work_item_id, repo_remote_url) {
            Ok(_) => publisher.kick_scheduler(),
            Err(err) => {
                tracing::warn!(
                    work_item_id,
                    ?err,
                    "run_done post-effects: failed to create legacy pr_review execution"
                );
                release_pending_review_hold_unconditional(work_db, work_item_id);
            }
        }
    }
}

/// Doc-link and design-question reconciliation for a declared-delivery
/// completion, after the local termination write has completed.
async fn run_declared_delivery_doc_link_detection(
    work_db: &crate::work::WorkDb,
    publisher: &dyn ExecutionPublisher,
    feature_flags: &crate::feature_flags::FeatureFlagsStore,
    structured_output_dir: &std::path::Path,
    execution: &crate::work::WorkExecution,
    work_item_id: &str,
    pr_url: &str,
) {
    let Ok(work_item) = work_db.get_work_item(work_item_id) else {
        return;
    };
    let (WorkItem::Task(task) | WorkItem::Chore(task)) = &work_item else {
        return;
    };
    design_detector::on_task_doc_pr_detected(work_db, &task.id, &task.product_id, pr_url).await;
    publisher
        .publish_work_item_changed(&task.product_id, &task.id, "task_doc_pointer_set")
        .await;

    if matches!(task.kind, TaskKind::Design | TaskKind::DesignPostmortem)
        && let Some(ref project_id) = task.project_id
    {
        design_detector::on_design_pr_detected(work_db, &task.id, &task.product_id, project_id, pr_url).await;
        publisher
            .publish_work_item_changed(&task.product_id, &task.id, "design_doc_pointer_set")
            .await;

        if let Some((group, created)) =
            attentions_detector::reconcile_design_doc_questions(work_db, &task.id, project_id, pr_url, false).await
        {
            for attention in created {
                publisher
                    .publish_frontend_event_on_product(
                        &task.product_id,
                        FrontendEvent::AttentionCreated {
                            attention,
                            group: group.clone(),
                        },
                    )
                    .await;
            }
        } else if feature_flags.is_enabled("attentions_questions_backstop")
            && let Some((group, created)) =
                attentions_detector::extract_doc_questions_backstop(work_db, &task.id, project_id, pr_url, false).await
        {
            for attention in created {
                publisher
                    .publish_frontend_event_on_product(
                        &task.product_id,
                        FrontendEvent::AttentionCreated {
                            attention,
                            group: group.clone(),
                        },
                    )
                    .await;
            }
        }
    }

    let transcript_path = work_db.transcript_path_for_execution(&execution.id).ok().flatten();
    let proposals_first =
        feature_flags.is_enabled("worker_proposals") && feature_flags.is_enabled("followup_proposals_seam");
    if let Some((group, created)) = attentions_detector::reconcile_task_followups(
        work_db,
        work_item_id,
        &execution.id,
        Some(structured_output_dir),
        transcript_path.as_deref(),
    )
    .await
    {
        if proposals_first {
            tracing::info!(execution_id = %execution.id, count = created.len(), "run_done post-effects: reconciled uncovered follow-up fallback entries");
        }
        for attention in created {
            publisher
                .publish_frontend_event_on_product(
                    &task.product_id,
                    FrontendEvent::AttentionCreated {
                        attention,
                        group: group.clone(),
                    },
                )
                .await;
        }
    } else if feature_flags.is_enabled("attentions_followups_backstop")
        && let Some((group, created)) = attentions_detector::extract_followups_backstop(
            work_db,
            work_item_id,
            &execution.id,
            transcript_path.as_deref(),
        )
        .await
    {
        for attention in created {
            publisher
                .publish_frontend_event_on_product(
                    &task.product_id,
                    FrontendEvent::AttentionCreated {
                        attention,
                        group: group.clone(),
                    },
                )
                .await;
        }
    }
}
