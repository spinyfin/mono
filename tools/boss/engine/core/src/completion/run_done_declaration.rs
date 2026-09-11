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
//! - a run that crashed or lost its channel between `apply_run_done`
//!   committing and this module's finalize call running (the finalize call
//!   happens after the RPC response is confirmed delivered — see
//!   `app::proposals::handle_submit_proposal` — so a crash in that narrow
//!   window is the one case a later Stop, or the merge poller, must still
//!   recover);
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
            boss_protocol::RunDoneOutcome::NoChangesNeeded => self.finalize_no_op_completion(&execution).await,
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
    ///
    /// [deferred-scope]: the background reviewer-enqueue path does not
    /// replicate `finalize_pr_transition`'s pure-rebase-specific no-op nuance
    /// (`check_pure_rebase_skip`, which reads the conflict/CI-fix attempt row)
    /// — it only applies the plain SHA-unchanged / empty-diff / trivial-diff
    /// checks. A `delivered` declaration for a pure-rebase push may therefore
    /// consume one extra reviewer cycle it would have skipped on the
    /// Stop-triggered path. It also does not replicate the followups /
    /// attentions-questions reconciliation `finalize_pr_transition` performs:
    /// proposal-submitted followups (`boss propose followup-task`) already
    /// land synchronously at submission time regardless of this path (see
    /// `proposal_apply::stage_followup_task_in_transaction`); the
    /// artifact/transcript-backstop followup channels and the design-doc
    /// questions detector do not yet run for a declared-delivery completion.
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
        let completion = match self.work_db.record_worker_pr_completion(
            &execution.id,
            &pr_url,
            None,
            None,
            WorkerPrCompletionTarget::InReview,
            None,
        ) {
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
    /// write on the Stop-triggered path. See that method's doc for what is
    /// intentionally narrower here.
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
    work_item_id: &str,
    repo_remote_url: &str,
    pr_url: &str,
) {
    let reviewer_triggering = should_enqueue_reviewer_for_primary(&execution_kind)
        || (execution_kind == ExecutionKind::RevisionImplementation && enable_revision_triggered_reviews);
    if reviewer_triggering {
        maybe_enqueue_declared_delivery_reviewer(
            work_db,
            branch_verifier,
            review_batch_enqueuer,
            feature_flags,
            max_review_cycles,
            min_review_changed_lines,
            review_pool_size,
            work_item_id,
            repo_remote_url,
            pr_url,
        )
        .await;
    }
    run_declared_delivery_doc_link_detection(work_db, publisher, work_item_id, pr_url).await;
}

/// Reviewer-enqueue decision for a declared-delivery completion — the
/// background counterpart to the inline gate in
/// [`super::pr_transition::WorkerCompletionHandler::finalize_pr_transition`].
/// See [`WorkerCompletionHandler::finalize_declared_delivery`]'s doc for the
/// two respects in which this is deliberately narrower.
#[allow(clippy::too_many_arguments)]
async fn maybe_enqueue_declared_delivery_reviewer(
    work_db: &crate::work::WorkDb,
    branch_verifier: &dyn BranchVerifier,
    review_batch_enqueuer: &dyn ReviewBatchEnqueuer,
    feature_flags: &crate::feature_flags::FeatureFlagsStore,
    max_review_cycles: usize,
    min_review_changed_lines: u64,
    review_pool_size: usize,
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
        return;
    }

    if let Some(last_sha) = last_reviewed_sha.as_deref()
        && review_cycle > 0
        && let Ok(repo_slug) = parse_repo_slug(repo_remote_url)
        && let Some(pr_number) = pr_number_from_url(pr_url)
    {
        match branch_verifier.fetch_pr_head_oid(&repo_slug, pr_number).await {
            Ok(current_head) if current_head == last_sha => {
                tracing::info!(
                    work_item_id,
                    "run_done post-effects: pr_review noop skip (sha_unchanged)"
                );
                return;
            }
            Ok(current_head) => match branch_verifier
                .fetch_diff_line_count(&repo_slug, last_sha, &current_head)
                .await
            {
                Ok(0) => {
                    tracing::info!(work_item_id, "run_done post-effects: pr_review noop skip (empty_diff)");
                    return;
                }
                Ok(diff_lines) if min_review_changed_lines > 0 && diff_lines < min_review_changed_lines => {
                    tracing::info!(
                        work_item_id,
                        "run_done post-effects: pr_review noop skip (trivial_diff)"
                    );
                    return;
                }
                Ok(_) => {}
                Err(err) => tracing::debug!(
                    work_item_id,
                    ?err,
                    "run_done post-effects: diff line count fetch failed; proceeding with review",
                ),
            },
            Err(err) => tracing::debug!(
                work_item_id,
                ?err,
                "run_done post-effects: pr head fetch failed; proceeding with review",
            ),
        }
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
                tracing::info!(
                    work_item_id,
                    pr_url,
                    "run_done post-effects: review batch enqueued ({dispatch:?})"
                );
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
                }
            }
        }
    } else if let Err(err) = work_db.create_pr_review_execution_dedup(work_item_id, repo_remote_url) {
        tracing::warn!(
            work_item_id,
            ?err,
            "run_done post-effects: failed to create legacy pr_review execution"
        );
    }
}

/// Doc-link auto-population for a declared-delivery completion — the
/// background counterpart to the per-task / per-project doc-link block in
/// [`super::pr_transition::WorkerCompletionHandler::finalize_pr_transition`].
/// Does not perform the attentions-questions / followups reconciliation that
/// function also runs — see
/// [`WorkerCompletionHandler::finalize_declared_delivery`]'s doc.
async fn run_declared_delivery_doc_link_detection(
    work_db: &crate::work::WorkDb,
    publisher: &dyn ExecutionPublisher,
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
    }
}
