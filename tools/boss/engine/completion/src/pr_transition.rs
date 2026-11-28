//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::finalize_passes::RemoteCollectionResult;
use super::*;

#[derive(serde::Deserialize)]
struct BatchReviewPrView {
    #[serde(rename = "baseRefOid")]
    base_sha: String,
    #[serde(rename = "headRefOid")]
    target_sha: String,
    #[serde(default)]
    additions: Option<i64>,
    #[serde(default)]
    deletions: Option<i64>,
}

/// Single routing decision for `delta_rereview`, shared across every automatic
/// pre-merge review trigger site (`finalize_pr_transition`,
/// `maybe_enqueue_declared_delivery_reviewer`, the merge poller's
/// deferred-admission sweep, and unpark reconcile; the last two reach it
/// through `CompletionRequests::delta_rereview_decision`).
///
/// Returns both:
/// - `arm`: the admission-time `delta_rereview` arm for this PR family
///   (`Treatment` / `Control` when `delta_rereview` is active in
///   `pre_merge_review`, `None` when off or held, or on DB read error).
/// - `single_reviewer_light`: `true` only when `arm == Some(Treatment)` AND
///   the PR family is on round 2 or later (`review_cycle >= 1`) with a
///   recorded `last_reviewed_sha` to compute a delta against.
pub fn delta_rereview_decision(
    feature_flags: &crate::feature_flags::FeatureFlagsStore,
    work_db: &crate::work::WorkDb,
    work_item_id: &str,
) -> boss_observation_contracts::DeltaRereviewDecision {
    let cycle_root_id = work_db.review_cycle_root_id(work_item_id);
    let (review_cycle, last_reviewed_sha) = match work_db.get_task_review_cycle_state(&cycle_root_id) {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!(
                work_item_id,
                cycle_root_id,
                ?err,
                "delta_rereview_decision: failed to read review cycle state; failing closed to full batch",
            );
            return boss_observation_contracts::DeltaRereviewDecision::default();
        }
    };
    let state = feature_flags.experiment_state(crate::feature_flags::DELTA_REREVIEW_EXPERIMENT);
    let bucket = crate::work::experiment_bucket(&cycle_root_id, crate::feature_flags::DELTA_REREVIEW_EXPERIMENT);
    let arm = match crate::feature_flags::assign_arm(state, bucket) {
        Some(crate::feature_flags::ExperimentArm::Treatment) => Some(boss_protocol::DeltaRereviewBatchArm::Treatment),
        Some(crate::feature_flags::ExperimentArm::Control) => Some(boss_protocol::DeltaRereviewBatchArm::Control),
        _ => None,
    };
    let is_repeat_round = review_cycle >= 1 && last_reviewed_sha.as_deref().is_some_and(|s| !s.is_empty());
    boss_observation_contracts::DeltaRereviewDecision {
        single_reviewer_light: is_repeat_round && arm == Some(boss_protocol::DeltaRereviewBatchArm::Treatment),
        arm,
    }
}

/// Convenience boolean projection of [`delta_rereview_decision`]: `true` when
/// this automatic pre-merge review should use the single-reviewer Light-tier
/// delta re-review path.
pub fn should_use_delta_rereview(
    feature_flags: &crate::feature_flags::FeatureFlagsStore,
    work_db: &crate::work::WorkDb,
    work_item_id: &str,
) -> bool {
    delta_rereview_decision(feature_flags, work_db, work_item_id).single_reviewer_light
}

/// Fetch and freeze the target metadata before handing it to the atomic DB
/// creation path. Callers can retain the legacy path if this cannot establish
/// an immutable target SHA. Shared by `finalize_pr_transition`,
/// `maybe_enqueue_declared_delivery_reviewer`, and the merge poller's
/// deferred-admission sweep.
pub(crate) async fn enqueue_review_batch_with_mode(
    work_db: &crate::work::WorkDb,
    work_item_id: &str,
    repo_remote_url: &str,
    pr_url: &str,
    review_pool_size: usize,
    decision: boss_observation_contracts::DeltaRereviewDecision,
) -> anyhow::Result<crate::work::ReviewBatchDispatch> {
    let root =
        boss_github::pr_files::fetch_pr_view_json(pr_url, "baseRefOid,headRefOid,files,additions,deletions").await?;
    let mut input = review_batch_input_from_metadata(work_db, work_item_id, pr_url, root)?;
    input.single_reviewer_light = decision.single_reviewer_light;
    input.delta_rereview_arm = decision.arm;
    work_db.create_pre_merge_review_batch_for_pool(input, repo_remote_url, review_pool_size)
}

/// Shared classification and immutable target validation for automatic and explicit starts.
pub fn review_batch_input_from_metadata(
    work_db: &crate::work::WorkDb,
    work_item_id: &str,
    pr_url: &str,
    root: serde_json::Value,
) -> anyhow::Result<crate::work::ReviewBatchCreateInput> {
    let view: BatchReviewPrView = serde_json::from_value(root.clone())?;
    if view.base_sha.is_empty() || view.target_sha.is_empty() {
        anyhow::bail!("GitHub PR metadata omitted immutable base or head SHA");
    }
    let pr_number = boss_github::pr_url::pr_number_from_url(pr_url)
        .ok_or_else(|| anyhow::anyhow!("could not parse pull request number from {pr_url:?}"))?
        .try_into()
        .map_err(|_| anyhow::anyhow!("pull request number does not fit the review-batch schema"))?;
    let cycle_root_id = work_db.review_cycle_root_id(work_item_id);
    let classification = crate::pr_review::classify_pr_review_metadata(&crate::pr_review::PrReviewMetadata {
        additions: view.additions,
        changed_files: Some(boss_github::pr_files::parse_changed_file_paths(&root)),
        deletions: view.deletions,
    });
    // The already-reviewed check runs inside `create_pre_merge_review_batch_for_pool`,
    // after its ExistingBatch/LegacyExecution checks and before the admission
    // gate — not here, ahead of them — so a batch still live for this exact
    // target (a leaf verdict already recorded while the others are still
    // running) is never misreported as "this head is settled".
    let legacy_task_id = (work_item_id != cycle_root_id).then(|| work_item_id.to_owned());
    let input = crate::work::ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root_id)
        .base_sha(view.base_sha)
        .classification(classification)
        .phase(boss_protocol::ReviewBatchPhase::PreMerge)
        .pr_number(pr_number)
        .pr_url(pr_url)
        .target_sha(view.target_sha)
        .maybe_legacy_task_id(legacy_task_id)
        .build();
    Ok(input)
}

/// Default enqueuer: fetch immutable SHAs from GitHub, then admit a batch.
///
/// It holds no feature-flag state: every trigger site computes the mode with
/// [`should_use_delta_rereview`] (via `CompletionRequests`) and passes it to
/// `enqueue_with_mode`, so the routing lives in one place. `enqueue` is the
/// full multi-leaf batch.
pub(crate) struct GhReviewBatchEnqueuer;

#[async_trait]
impl ReviewBatchEnqueuer for GhReviewBatchEnqueuer {
    async fn enqueue(
        &self,
        work_db: &crate::work::WorkDb,
        work_item_id: &str,
        repo_remote_url: &str,
        pr_url: &str,
        review_pool_size: usize,
    ) -> anyhow::Result<crate::work::ReviewBatchDispatch> {
        enqueue_review_batch_with_mode(
            work_db,
            work_item_id,
            repo_remote_url,
            pr_url,
            review_pool_size,
            boss_observation_contracts::DeltaRereviewDecision::default(),
        )
        .await
    }

    async fn enqueue_with_mode(
        &self,
        work_db: &crate::work::WorkDb,
        work_item_id: &str,
        repo_remote_url: &str,
        pr_url: &str,
        review_pool_size: usize,
        decision: boss_observation_contracts::DeltaRereviewDecision,
    ) -> anyhow::Result<crate::work::ReviewBatchDispatch> {
        enqueue_review_batch_with_mode(
            work_db,
            work_item_id,
            repo_remote_url,
            pr_url,
            review_pool_size,
            decision,
        )
        .await
    }
}

impl WorkerCompletionHandler {
    /// Stop hook path, `"pr_recheck"` for the merge-poller's
    /// fallback sweep — so operators can see which path closed a
    /// given chore.
    pub(super) async fn finalize_pr_transition(
        &self,
        execution_id: &str,
        pr_url: String,
        target: WorkerPrCompletionTarget,
        source: &'static str,
    ) -> StopOutcome {
        // Read once and reuse below for the reviewer-enqueue check, so both
        // spots agree on the same execution snapshot instead of racing two
        // independent reads. Captured before `record_worker_pr_completion`
        // below nulls `workspace_path` in the same transaction that
        // terminalizes the execution — this IS the path that terminalizes a
        // parked-live (`waiting_human` / `running`) execution on PR success,
        // so it owns driver teardown. A failed execution lookup is retryable:
        // no teardown or PR transition may run without enforcing the guards.
        let execution = match self.work_db.get_execution(execution_id) {
            Ok(execution) => execution,
            Err(err) => {
                tracing::warn!(execution_id, ?err, "pr completion: execution lookup failed; retrying");
                return StopOutcome::DbError;
            }
        };
        if let Some(conflict) = self.foreign_pr_binding_conflict(&execution, &pr_url) {
            return self
                .refuse_foreign_pr_binding(&execution, &pr_url, &conflict, source)
                .await;
        }
        // A merged PR can no longer be retargeted, so the base policy only
        // applies to a PR that is still open.
        if matches!(target, WorkerPrCompletionTarget::Done) {
            self.resolve_pr_base_refusal_attention(&execution.id);
        } else if let Some(outcome) = self.guard_pr_base(&execution, &pr_url).await {
            return outcome;
        }
        let workspace_path = execution.workspace_path.clone();

        let mut target = target;
        let mut merged = matches!(target, WorkerPrCompletionTarget::Done);

        // incident-004 AI-3: a revision may terminalize to PendingReview /
        // InReview only with an explicit associated change (head SHA moved
        // attributably, or observed PR metadata mutation) or an explicit
        // nothing-to-do outcome. Silence after a mid-turn reap — no push,
        // no metadata marker, no no-changes-needed — must not read as
        // review-ready. Couples the existing `sha_unchanged` observation
        // (reviewer noop skip) to status success; runs before mid-turn
        // reap accounting so a refused finalize never looks like a reap.
        if !merged
            && matches!(target, WorkerPrCompletionTarget::InReview)
            && execution.kind == ExecutionKind::RevisionImplementation
        {
            match self
                .evaluate_revision_review_contribution(execution_id, &execution, &pr_url, source)
                .await
            {
                RevisionReviewGate::Allow(reason) => {
                    if reason == RevisionContributionReason::Indeterminate {
                        tracing::warn!(
                            execution_id,
                            pr_url = %pr_url,
                            source,
                            contribution = reason.as_str(),
                            "revision contribution gate: allowing InReview terminalization with \
                             NO usable SHA-delta / metadata / no-op evidence — contribution was \
                             never established (missing baseline or head fetch failed)",
                        );
                    } else {
                        tracing::info!(
                            execution_id,
                            pr_url = %pr_url,
                            source,
                            contribution = reason.as_str(),
                            "revision contribution gate: allowing InReview terminalization",
                        );
                    }
                }
                RevisionReviewGate::Refuse { reason } => {
                    tracing::warn!(
                        execution_id,
                        pr_url = %pr_url,
                        source,
                        refuse_reason = reason,
                        "revision contribution gate: refusing InReview / PendingReview \
                         terminalization — no head movement, no metadata-fix confirmation, and \
                         no explicit no-changes-needed (couples sha_unchanged to status success; \
                         incident-004 AI-3)",
                    );
                    return StopOutcome::AwaitingInput;
                }
            }
        }

        // Every PR-terminalization source funnels through here, so this is
        // the one place that can prevent a recheck or Stop race from reaping
        // a worker that is still mid-turn. Read BEFORE
        // `begin_teardown`/`finish_worker_teardown` can release the pane and
        // clear the live-state slot out from under this check.
        if self.observed_mid_turn(execution_id) {
            if self.should_defer_mid_turn_pr_completion(execution_id, source) {
                tracing::info!(
                    execution_id,
                    source,
                    kind = %execution.kind,
                    "pr completion: worker is still mid-turn; deferring terminalization to its activity boundary",
                );
                return StopOutcome::AwaitingInput;
            }
            self.record_mid_turn_reap(&execution, source).await;
        }

        let mut verified_head = None;
        if !merged {
            match self.conflict_delivery_decision(&execution, Some(&pr_url)).await {
                ConflictDeliveryDecision::Failed(outcome) => return outcome,
                ConflictDeliveryDecision::Merged => {
                    target = WorkerPrCompletionTarget::Done;
                    merged = true;
                }
                ConflictDeliveryDecision::ClosedUnmerged => {
                    target = WorkerPrCompletionTarget::DoneUnmerged;
                }
                ConflictDeliveryDecision::Proceed { verified_head: head } => verified_head = head,
            }
        }

        // Forensic teardown head — captured before any durable reviewer
        // enqueue or terminalization write. Fail-open: this snapshot is not
        // consulted by the mid-turn guard, so a transient `gh` failure or an
        // unparseable URL must not block PR completion engine-wide. When the
        // direct read fails, the head GitHub reported while verifying a
        // revision's delivery stands in, so the column is empty only when
        // neither source had one.
        let pr_head_after = self.fetch_pr_head_after(&execution, &pr_url).await.or(verified_head);

        // The head of the batch a revision is about to be held for. The hold
        // is released by the review of exactly that head, so it is recorded
        // on the run with the write that takes the hold: see
        // `super::revision_review_hold`.
        let mut review_hold_head: Option<String> = None;

        // A revision's review hold rests on a head. When neither the read
        // above nor the Stop boundary's own stamp has one, this completion
        // cannot say which review the push would be held for, and a hold
        // taken anyway is one nothing releases. It fails here, before
        // anything is admitted, the way a declared delivery does. Every
        // other kind keeps the fail-open behaviour described above.
        if !merged
            && matches!(target, WorkerPrCompletionTarget::InReview)
            && pr_head_after.is_none()
            && self.enable_revision_triggered_reviews
            && execution.kind == ExecutionKind::RevisionImplementation
            && !super::run_done_declaration::review_skipped_before_hold(
                &self.work_db,
                &execution.work_item_id,
                self.max_review_cycles,
            )
            && !matches!(
                self.work_db.get_revision_stop_contributed_head(execution_id),
                Ok(Some(ref stamped)) if !stamped.is_empty()
            )
        {
            return self.fail_delivery_with_unreadable_head(&execution, &pr_url).await;
        }

        // For reviewer-triggering executions with a fresh
        // (non-merged) PR, try to enqueue an independent reviewer pass
        // instead of immediately advancing to human Review.
        // This also checks the cycle bound first — if review_cycle has
        // already reached max_review_cycles, skip the reviewer and proceed
        // to InReview with a sticky attention item for the human.
        // If the pr_review execution cannot be created (DB error), fall back
        // to the normal InReview path so the task is never left stuck.
        let enqueued_reviewer = if !merged && matches!(target, WorkerPrCompletionTarget::InReview) {
            match &execution {
                // A parked row atrophies: the run that was in flight when the
                // owner parked it records its PR and advances to `in_review`,
                // with no AI review; scheduling it after un-park is part of the
                // un-park reconciliation (design section 1.5).
                producing
                    if super::run_done_declaration::reviewer_suppressed_by_park(
                        &self.work_db,
                        &producing.work_item_id,
                    ) =>
                {
                    tracing::info!(
                        execution_id,
                        work_item_id = %producing.work_item_id,
                        trigger = source,
                        "row is parked: advancing to in_review without a reviewer pass",
                    );
                    false
                }
                producing
                    if should_enqueue_reviewer_for_primary(&producing.kind)
                        || (producing.kind == ExecutionKind::RevisionImplementation
                            && self.enable_revision_triggered_reviews) =>
                {
                    // 2026-07-01 revision-review experiment: distinguish the
                    // trigger for logging/observability (kill-switch gates
                    // only the revision arm above; a primary push is always
                    // reviewed).
                    let trigger = if producing.kind == ExecutionKind::RevisionImplementation {
                        "revision_push"
                    } else {
                        "primary_push"
                    };
                    // Read cycle state once — used by both
                    // the no-op gate and the cycle-bound check below.
                    // Tracked on the review-cycle root (chain root for a
                    // revision, the task itself otherwise) so cycle-bound and
                    // no-op-skip state accumulates across the whole revision
                    // chain instead of resetting on every fresh revision task
                    // row — see `WorkDb::review_cycle_root_id`.
                    let max_cycles = self.max_review_cycles;
                    let cycle_root_id = self.work_db.review_cycle_root_id(&producing.work_item_id);
                    let (review_cycle, last_reviewed_sha) =
                        match self.work_db.get_task_review_cycle_state(&cycle_root_id) {
                            Ok(state) => state,
                            Err(err) => {
                                // Fail open: treat as cycle=0, no prior SHA so both
                                // gates pass through (don't skip on uncertainty).
                                tracing::warn!(
                                    execution_id,
                                    work_item_id = %producing.work_item_id,
                                    cycle_root_id,
                                    ?err,
                                    "could not read review_cycle; assuming bound not reached",
                                );
                                (0i64, None)
                            }
                        };

                    // No-op / trivial-diff skip gate. Runs before
                    // the cycle-bound check so a pure rebase doesn't consume a
                    // cycle slot or surface an attention item.
                    //
                    // `check_pure_rebase_skip` runs first: it is keyed off the
                    // conflict/CI-fix attempt row rather than
                    // `review_cycle`/`last_reviewed_sha`, so it also catches a
                    // pure rebase that lands before the PR's very first review
                    // — a case `check_noop_skip_outcome`'s rule 1 always treats as
                    // "never skip" (right for genuinely new content, wrong for
                    // a rebase that contributes none). It is a no-op for any
                    // producer that isn't a conflict-resolution / CI-fix push.
                    let pure_rebase_gate = self.check_pure_rebase_skip(&pr_url, producing, &cycle_root_id).await;
                    let noop_skip = match pure_rebase_gate.skip_reason {
                        Some(reason) => super::finalize_passes::NoopSkipOutcome {
                            reason: Some(reason),
                            head_sha: pure_rebase_gate.post_head.clone(),
                        },
                        None => {
                            self.check_noop_skip_outcome(
                                &pr_url,
                                producing,
                                review_cycle,
                                last_reviewed_sha.as_deref(),
                                pure_rebase_gate.post_head,
                            )
                            .await
                        }
                    };
                    let skip_head_sha = noop_skip.head_sha.as_deref().or(pr_head_after.as_deref());

                    if let Some(skip_reason) = noop_skip.reason {
                        tracing::info!(
                            execution_id,
                            work_item_id = %producing.work_item_id,
                            skip_reason,
                            trigger,
                            "pr_review noop skip: advancing to in_review without reviewer pass",
                        );
                        if let Err(err) = self.work_db.record_skip_review_verdict(
                            execution_id,
                            &cycle_root_id,
                            skip_head_sha,
                            crate::work::REVIEW_GATE_OUTCOME_SKIPPED_NOOP,
                        ) {
                            tracing::warn!(
                                execution_id,
                                cycle_root_id,
                                ?err,
                                "failed to record skipped_noop verdict on chain root",
                            );
                        }
                        false
                    } else {
                        // Cycle bound check.
                        let cycle_bound_reached = (review_cycle as usize) >= max_cycles;

                        if cycle_bound_reached {
                            tracing::info!(
                                execution_id,
                                work_item_id = %producing.work_item_id,
                                max_review_cycles = max_cycles,
                                trigger,
                                "pr_review cycle bound reached; skipping reviewer \
                                 and advancing to in_review",
                            );
                            if let Err(err) = self.work_db.record_skip_review_verdict(
                                execution_id,
                                &cycle_root_id,
                                skip_head_sha,
                                crate::work::REVIEW_GATE_OUTCOME_SKIPPED_CYCLE_CAP,
                            ) {
                                tracing::warn!(
                                    execution_id,
                                    cycle_root_id,
                                    ?err,
                                    "failed to record skipped_cycle_cap verdict on chain root",
                                );
                            }
                            // Surface a sticky attention item so the human can see
                            // the cycle limit was hit when they open the PR card.
                            let _ = self.work_db.create_attention_item(CreateAttentionItemInput {
                                work_item_id: Some(producing.work_item_id.clone()),
                                kind: "pr_review_cycle_bound".to_owned(),
                                title: format!("Automated reviewer: cycle limit ({max_cycles}) reached"),
                                body_markdown: format!(
                                    "The automated reviewer completed {max_cycles} \
                                     cycle(s) on this PR without resolving all findings. \
                                     The PR has been advanced to human Review.\n\n\
                                     See the most recent revision task for the outstanding \
                                     findings from the last automated review cycle."
                                ),
                                execution_id: None,
                                status: None,
                                resolved_at: None,
                            });
                            false
                        } else {
                            // Dedup-and-insert atomically, closing the race where
                            // two independent completion triggers (the Stop-hook path
                            // and the merge-poller's `pr_recheck` sweep) each observe
                            // the producing execution as not-yet-terminal around the
                            // same moment and would otherwise both enqueue a `pr_review`
                            // execution for the same unchanged head sha.
                            if self.feature_flags.is_enabled("review_batch_fanout") {
                                let decision = delta_rereview_decision(
                                    &self.feature_flags,
                                    &self.work_db,
                                    &producing.work_item_id,
                                );
                                match self
                                    .review_batch_enqueuer
                                    .enqueue_with_mode(
                                        &self.work_db,
                                        &producing.work_item_id,
                                        &producing.repo_remote_url,
                                        &pr_url,
                                        self.review_pool_size,
                                        decision,
                                    )
                                    .await
                                {
                                    Ok(crate::work::ReviewBatchDispatch::Created { batch, executions }) => {
                                        tracing::info!(
                                            execution_id,
                                            batch_id = %batch.id,
                                            leaf_executions = executions.len(),
                                            pr_url = %pr_url,
                                            producing_kind = %producing.kind,
                                            trigger,
                                            "review batch leaf executions enqueued; holding producing task for reviewer pass",
                                        );
                                        self.publisher.kick_scheduler();
                                        if producing.kind == ExecutionKind::RevisionImplementation {
                                            review_hold_head = Some(batch.target_sha.clone());
                                        }
                                        true
                                    }
                                    Ok(crate::work::ReviewBatchDispatch::ExistingBatch { batch, executions }) => {
                                        let decided = matches!(
                                            batch.status,
                                            boss_protocol::ReviewBatchStatus::Completed
                                                | boss_protocol::ReviewBatchStatus::Failed
                                        );
                                        if decided && producing.kind == ExecutionKind::RevisionImplementation {
                                            // Another row's admission created the batch
                                            // for this head and it has already reached
                                            // its decision. A hold taken now would wait
                                            // for a decision that is not coming, so the
                                            // revision goes to `in_review` as a batch's
                                            // decision would have sent it.
                                            tracing::info!(
                                                execution_id,
                                                batch_id = %batch.id,
                                                batch_status = %batch.status,
                                                target_sha = %batch.target_sha,
                                                pr_url = %pr_url,
                                                "review batch for this head has already reached its decision; \
                                                 advancing the revision to in_review without a hold",
                                            );
                                            false
                                        } else {
                                            tracing::info!(
                                                execution_id,
                                                batch_id = %batch.id,
                                                leaf_executions = executions.len(),
                                                pr_url = %pr_url,
                                                "review batch already enqueued for immutable target",
                                            );
                                            if producing.kind == ExecutionKind::RevisionImplementation {
                                                review_hold_head = Some(batch.target_sha.clone());
                                            }
                                            true
                                        }
                                    }
                                    Ok(crate::work::ReviewBatchDispatch::LegacyExecution(review_exec)) => {
                                        tracing::info!(
                                            execution_id,
                                            review_execution_id = %review_exec.id,
                                            pr_url = %pr_url,
                                            "legacy reviewer already owns this target; preserving mode separation",
                                        );
                                        true
                                    }
                                    Ok(crate::work::ReviewBatchDispatch::AdmissionDeferred) => {
                                        tracing::info!(
                                            execution_id,
                                            pr_url = %pr_url,
                                            "pre-merge review admission deferred (pool full, or held by focus / \
                                             all-spawns pause); holding the task pending review — the deferred-\
                                             admission sweep retries once a reservation is free and the PR may \
                                             dispatch",
                                        );
                                        file_admission_deferred_attention(
                                            &self.work_db,
                                            &producing.work_item_id,
                                            &pr_url,
                                        );
                                        true
                                    }
                                    Ok(crate::work::ReviewBatchDispatch::AlreadyReviewed) => {
                                        tracing::info!(
                                            execution_id,
                                            pr_url = %pr_url,
                                            "current head already has an informative review verdict; holding the \
                                             task pending review — the deferred-admission sweep advances it to \
                                             in_review on the next pass without dispatching another reviewer",
                                        );
                                        // File the same marker a deferral would, so the
                                        // candidate query's marker arm surfaces this hold
                                        // on the very next sweep pass rather than waiting
                                        // out the 10-minute staleness cutoff — matching
                                        // what the log line above actually promises.
                                        file_admission_deferred_attention(
                                            &self.work_db,
                                            &producing.work_item_id,
                                            &pr_url,
                                        );
                                        true
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            execution_id,
                                            ?error,
                                            "failed to create immutable review batch; using legacy reviewer path",
                                        );
                                        // The failed `enqueue_review_batch` call could not resolve the
                                        // current target SHA, so we cannot probe `review_batch_for_target`
                                        // directly. Instead check for any non-terminal pre_merge batch
                                        // already open for this cycle root: if one exists, a legacy
                                        // execution here would let old-mode and batch-mode finalizers act
                                        // on the same target, which must never happen.
                                        match self.work_db.review_batches_for_cycle_root(&producing.work_item_id) {
                                            Err(error) => {
                                                // Fail safe, not fail open: a query error must never be
                                                // read as "no live batch exists", or this fallback would
                                                // mint a legacy reviewer on top of a batch it simply
                                                // failed to see, recreating the double-ownership this
                                                // guard exists to prevent. Leaving the target alone this
                                                // pass is recoverable — the next `pr_recheck` sweep
                                                // retries the same check.
                                                tracing::warn!(
                                                    execution_id,
                                                    ?error,
                                                    pr_url = %pr_url,
                                                    "failed to query existing review batches for cycle root; \
                                                     skipping legacy reviewer this pass to avoid double ownership",
                                                );
                                                true
                                            }
                                            Ok(batches) => {
                                                let existing_batch = batches.into_iter().find(|batch| {
                                                    batch.phase == boss_protocol::ReviewBatchPhase::PreMerge
                                                        && !matches!(
                                                            batch.status,
                                                            boss_protocol::ReviewBatchStatus::Completed
                                                                | boss_protocol::ReviewBatchStatus::Failed
                                                        )
                                                });
                                                if let Some(batch) = existing_batch {
                                                    tracing::info!(
                                                        execution_id,
                                                        batch_id = %batch.id,
                                                        batch_target_sha = %batch.target_sha,
                                                        pr_url = %pr_url,
                                                        "a live pre_merge batch is open for this cycle root (target \
                                                         SHA unknown after the failed fetch); skipping the legacy \
                                                         reviewer to preserve mode separation — the next \
                                                         pr_recheck pass will create a batch for the current head \
                                                         if needed",
                                                    );
                                                    true
                                                } else {
                                                    match self.work_db.create_pr_review_execution_dedup(
                                                        &producing.work_item_id,
                                                        &producing.repo_remote_url,
                                                    ) {
                                                        Ok((review_exec, _)) => {
                                                            self.publisher.kick_scheduler();
                                                            tracing::info!(
                                                                execution_id,
                                                                review_execution_id = %review_exec.id,
                                                                "legacy reviewer enqueued after batch metadata \
                                                                 failure",
                                                            );
                                                            true
                                                        }
                                                        Err(legacy_error)
                                                            if legacy_error
                                                                .is::<crate::work::LiveReviewBatchOwnsCycle>() =>
                                                        {
                                                            true
                                                        }
                                                        Err(legacy_error) => {
                                                            tracing::warn!(
                                                                execution_id,
                                                                ?legacy_error,
                                                                "failed to create legacy reviewer after batch \
                                                                 metadata failure",
                                                            );
                                                            false
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                match self.work_db.create_pr_review_execution_dedup(
                                    &producing.work_item_id,
                                    &producing.repo_remote_url,
                                ) {
                                    Ok((review_exec, true)) => {
                                        tracing::info!(
                                            execution_id,
                                            review_execution_id = %review_exec.id,
                                            pr_url = %pr_url,
                                            producing_kind = %producing.kind,
                                            trigger,
                                            "pr_review execution enqueued; \
                                             holding producing task for reviewer pass",
                                        );
                                        self.publisher.kick_scheduler();
                                        true
                                    }
                                    Ok((review_exec, false)) => {
                                        tracing::info!(
                                            execution_id,
                                            review_execution_id = %review_exec.id,
                                            pr_url = %pr_url,
                                            producing_kind = %producing.kind,
                                            trigger,
                                            "pr_review execution already enqueued/in-flight for this \
                                             item; reusing instead of dispatching a duplicate review",
                                        );
                                        true
                                    }
                                    Err(err) if err.is::<crate::work::LiveReviewBatchOwnsCycle>() => true,
                                    Err(err) => {
                                        tracing::warn!(
                                            execution_id,
                                            ?err,
                                            "failed to create pr_review execution; \
                                     falling back to immediate in_review",
                                        );
                                        false
                                    }
                                }
                            }
                        }
                    } // closes the `} else {` for the noop skip gate
                }
                _ => false, // non-reviewer-triggering execution; advance to in_review as normal
            }
        } else {
            false
        };

        let effective_target = if enqueued_reviewer {
            WorkerPrCompletionTarget::PendingReview
        } else {
            target
        };

        // Point the revision at the head of the batch it is about to be held
        // for, before the write that takes the hold. The Stop boundary's own
        // stamp is a separate read of the head: it can be missing (the read
        // failed) or name an earlier head than the one admission just read,
        // and a hold keyed on either is one this batch's decision would not
        // release.
        let review_hold_head = review_hold_head.filter(|_| enqueued_reviewer);
        let holds_revision = enqueued_reviewer && execution.kind == ExecutionKind::RevisionImplementation;
        // When admission named no batch (it was deferred, or the pull request
        // could not be read), a hold the Stop boundary did not stamp falls
        // back to the head the run finished at, so a batch admitted for that
        // head by any other path still releases the revision.
        let stamped_at_stop = holds_revision
            && matches!(
                self.work_db.get_revision_stop_contributed_head(execution_id),
                Ok(Some(ref stamped)) if !stamped.is_empty()
            );
        let head_to_record = match review_hold_head.as_deref() {
            Some(head) => Some(head),
            None if holds_revision && !stamped_at_stop => pr_head_after.as_deref(),
            None => None,
        };
        if let Some(head) = head_to_record
            && let Err(err) = self.work_db.set_revision_stop_contributed_head(execution_id, head)
        {
            tracing::warn!(
                execution_id,
                head,
                ?err,
                "pr completion: failed to record the head this revision is held for",
            );
        }

        // Mark the teardown in flight BEFORE the write that terminalizes
        // this execution. From here until `finish_worker_teardown` drops
        // the guard, `terminal_work_sweep` can see that this pane already
        // has an owner and must not reclaim it — without the mark, the
        // window between "row is terminal" and "pane is gone" is exactly
        // that sweep's reap precondition, and on 2026-07-30 it reaped and
        // SIGKILLed live workers inside it. Dropped automatically on both
        // early returns below.
        let teardown = self.begin_teardown(execution_id);

        let record_started = std::time::Instant::now();
        let completion = match self.work_db.record_worker_pr_completion(
            execution_id,
            &pr_url,
            pr_head_after.as_deref(),
            None,
            effective_target,
            None,
        ) {
            Ok(Some(completion)) => completion,
            Ok(None) => return StopOutcome::AlreadyTerminal,
            Err(err) => {
                tracing::error!(execution_id, source, ?err, "pr completion: failed to record");
                return StopOutcome::DbError;
            }
        };
        tracing::info!(
            execution_id,
            source,
            target = ?effective_target,
            pr_head_after = pr_head_after.as_deref().unwrap_or("(unread)"),
            review_hold_head = review_hold_head.as_deref().unwrap_or("(none)"),
            elapsed_ms = record_started.elapsed().as_millis(),
            "pr completion: execution terminalized; teardown in flight",
        );
        // The batch was admitted before the completion write, so it can have
        // reached its decision in between, while the run was still live and
        // its decision could not find the revision. Settling after the write
        // releases the revision in that case.
        let released_by_settling = match review_hold_head.as_deref() {
            Some(head) => {
                super::revision_review_hold::settle_revision_hold(
                    &self.work_db,
                    self.publisher.as_ref(),
                    &completion.execution.work_item_id,
                    head,
                    source,
                )
                .await
            }
            None => false,
        };
        let enqueued_reviewer = enqueued_reviewer && !released_by_settling;
        self.reconcile_review_guide_source_for_execution(
            execution_id,
            &pr_url,
            crate::work::PrSourceCaptureTrigger::Completion,
        );
        // Clear the staged URL now that the DB write succeeded.
        // Deliberately ordered after `record_worker_pr_completion` so a
        // failed DB write leaves the cache intact for the worker's next
        // Stop. For primary implementations the next merge-poller sweep
        // also retries from this URL; a revision's sweep defers instead,
        // while its worker is observed mid-turn within
        // `staged_pr_mid_turn_defer_secs` of `staged_at` — once that horizon
        // expires (or the worker is not observed live), it recovers from
        // the exact head stamped by the Stop that observed the push (see
        // `revision_stop_contributed_head` and `recheck_for_pr`'s exact-head
        // gate).
        self.staged_pr_urls.forget(execution_id);
        // The worker contributed a PR — reset any accumulated nudge
        // count so a later unrelated nudge cycle starts clean.
        self.nudge_breaker.forget(execution_id);
        self.build_wait_tracker.forget(execution_id);
        self.background_children_tracker.forget(execution_id);
        self.hold_registry.release(execution_id);
        // Stop → pr_transition termination path: the call above just moved
        // the execution to `completed` (see `record_worker_pr_completion`),
        // so this path owns the whole teardown — pane, driver state, cube
        // lease, in that order. See `super::teardown` for why the ordering
        // is what it is (in particular why driver teardown must not run
        // while the worker process may still be alive).
        self.finish_worker_teardown(
            execution_id,
            &completion.execution.work_item_id,
            completion.released_lease_id.as_deref(),
            workspace_path.as_deref().map(std::path::Path::new),
            source,
            teardown,
        )
        .await;
        let binding_mismatch = completion.binding_mismatch;
        if binding_mismatch {
            self.note_stale_merge_binding_mismatch(
                &completion.execution,
                work_item_pr_url(&completion.work_item),
                &pr_url,
                source,
            )
            .await;
            merged = false;
        }
        let product_id = completion.work_item.product_id().to_string();
        let work_item_id = work_item_id(&completion.work_item);
        let publish_reason = match (merged, source) {
            (true, "pr_recheck") => "worker_pr_merged_recheck",
            (false, "pr_recheck") => "worker_pr_completed_recheck",
            (true, _) => "worker_pr_merged",
            (false, _) => "worker_pr_completed",
        };
        self.publisher
            .publish(
                &completion.execution.id,
                &completion.execution.work_item_id,
                completion.execution.status.as_str(),
                publish_reason,
            )
            .await;
        self.publisher
            .publish_work_item_changed(&product_id, &work_item_id, publish_reason)
            .await;
        // Doc-link auto-population. Two independent pointers:
        //  - every leaf work item -> the TASK's own `doc_*` pointer
        //    (`design_detector::on_task_doc_pr_*`). Independent of kind.
        //  - design tasks WITH a project (and design_postmortem) ALSO
        //    populate the PROJECT's `design_doc_*` pointer
        //    (`design_detector::on_design_pr_*`). That is a separate
        //    concept; this change does not merge the two.
        // The routing decision is logged ABOVE the per-branch dispatch so a
        // skip/proceed is ALWAYS visible without entering a gated block.
        // Detector errors are logged inside the detector — they must not
        // surface here because they'd mask the successful PR transition.
        //
        // `design_postmortem` completions skip the generic best-effort
        // followups mechanism entirely (see the guard below) in favour of
        // the mandatory, stronger `postmortem_followups` path — computed
        // once here so both spots agree on the kind check.
        let is_design_postmortem = matches!(
            &completion.work_item,
            WorkItem::Task(t) | WorkItem::Chore(t) if t.kind == TaskKind::DesignPostmortem
        );
        if let WorkItem::Task(ref task) | WorkItem::Chore(ref task) = completion.work_item {
            let produces_project_design =
                matches!(task.kind, TaskKind::Design | TaskKind::DesignPostmortem) && task.project_id.is_some();
            let decision = if produces_project_design {
                "per-task-doc+project-design-doc"
            } else {
                "per-task-doc"
            };
            tracing::info!(
                execution_id,
                work_item_id = %task.id,
                kind = %task.kind,
                project_id = ?task.project_id,
                merged,
                decision,
                "doc-detection: routing PR completion"
            );

            if merged && !binding_mismatch {
                // Worker merged directly during its session; the detector
                // fetches base_ref_name from the PR (unknown here).
                self.collaborators
                    .design_detection
                    .on_task_doc_pr_merged(&self.work_db, &task.id, &task.product_id, &pr_url, None)
                    .await;
            } else if !binding_mismatch {
                self.collaborators
                    .design_detection
                    .on_task_doc_pr_detected(&self.work_db, &task.id, &task.product_id, &pr_url)
                    .await;
            }
            // The earlier `publish_work_item_changed` above ran BEFORE
            // this detector call, so the client's refetch it triggers
            // can race the doc-pointer write and land the doc link
            // absent — leaving it visible only when some later,
            // unrelated event happens to refetch the tree. Publish a
            // second invalidation now that the pointer write (or its
            // no-op) has actually completed, mirroring the manual
            // `boss task set-doc` path (`app/work_items.rs`, reason
            // `task_doc_pointer_set`).
            self.publisher
                .publish_work_item_runtime_changed(&task.product_id, &task.id, "task_doc_pointer_set")
                .await;
        }

        // Per-project design-doc pointer + design-doc questions pipeline:
        // `kind=design` tasks WITH a project, and `kind=design_postmortem`
        // tasks (always project-scoped) which re-sync the same pointer
        // after editing the project's existing design doc.
        if let WorkItem::Task(ref task) | WorkItem::Chore(ref task) = completion.work_item
            && matches!(task.kind, TaskKind::Design | TaskKind::DesignPostmortem)
            && let Some(ref project_id) = task.project_id
        {
            if merged && !binding_mismatch {
                // Worker merged directly during its session; update
                // the branch to main (base_ref_name unknown here,
                // so the detector will fetch it from the PR).
                self.collaborators
                    .design_detection
                    .on_design_pr_merged(&self.work_db, &task.id, &task.product_id, project_id, &pr_url, None)
                    .await;

                // Postmortem-surfaced uncompleted work must become real
                // task rows, not a mention in the doc — see
                // `postmortem_followups` for why this is a stronger,
                // mandatory-artifact path distinct from the generic
                // best-effort `FOLLOWUPS:` mechanism below (which is
                // skipped entirely for this kind, see that block).
                if task.kind == TaskKind::DesignPostmortem {
                    // Collect the remote worker's postmortem-followups artifact
                    // before reading it, mirroring the review pass —
                    // a remote postmortem worker's artifact otherwise sits
                    // uncollected and unreaped on the host. Best-effort: a
                    // collection failure here does not block the PR
                    // transition — `reconcile_postmortem_followups` already
                    // treats a missing/malformed artifact as an
                    // operator-visible error on the postmortem task itself,
                    // which still fires correctly when the read below simply
                    // finds nothing.
                    if let RemoteCollectionResult::Failed { host_id, reason } = self
                        .collect_remote_structured_output(
                            &completion.execution,
                            crate::structured_output::StructuredOutputKind::PostmortemFollowups,
                        )
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            host_id = %host_id,
                            error = %reason,
                            "postmortem followups: remote structured-output collection failed; \
                             continuing without the artifact",
                        );
                    }
                    crate::postmortem_followups::reconcile_postmortem_followups(
                        &self.work_db,
                        &task.id,
                        &task.product_id,
                        project_id,
                        execution_id,
                        Some(&self.structured_output_dir),
                    )
                    .await;
                }
            } else if !binding_mismatch {
                self.collaborators
                    .design_detection
                    .on_design_pr_detected(&self.work_db, &task.id, &task.product_id, project_id, &pr_url)
                    .await;
            }
            // See the matching comment on the per-task doc branch above:
            // the earlier `publish_work_item_changed` predates this
            // detector's pointer write, so publish a follow-up
            // invalidation now that it has actually landed.
            self.publisher
                .publish_work_item_changed(&task.product_id, &task.id, "design_doc_pointer_set")
                .await;

            // Attentions creation pipeline (design: attentions.md).
            // A design worker may ship a sibling `<slug>.attentions.json`
            // question manifest; parse it off the PR branch and upsert
            // the question group. Idempotent across re-detections.
            let questions_result = self
                .collaborators
                .attentions_detection
                .reconcile_design_doc_questions(&self.work_db, &task.id, project_id, &pr_url, merged)
                .await;
            if let Some((ref group, ref created)) = questions_result {
                self.publish_attentions_created(group, created).await;
            } else if self.feature_flags.is_enabled("attentions_questions_backstop") {
                // Primary found no manifest; fall back to the extraction
                // backstop which reads the doc's "Risks / open questions"
                // section (flagged `confidence_source = extracted`).
                if let Some((group, created)) = self
                    .collaborators
                    .attentions_detection
                    .extract_doc_questions_backstop(&self.work_db, &task.id, project_id, &pr_url, merged)
                    .await
                {
                    self.publish_attentions_created(&group, &created).await;
                }
            }
        }

        // Followups: any completing implementation worker may surface
        // out-of-scope follow-on work. PRIMARY (design implementation task
        // 10): a `followup_task` proposal — `boss propose followup-task`
        // already upserted the member into the `followup` attention group
        // synchronously at submission time
        // (`crate::work::proposal_apply::stage_followup_task_in_transaction`),
        // for each follow-up submitted that way. LEGACY (counted fallback for
        // whatever a proposal did not already cover, or unconditionally when
        // the seam is off): the engine-owned structured-output artifact (a
        // `FollowupEntry` JSON array), falling back to a `FOLLOWUPS:` block
        // scraped from the transcript tail, and finally the
        // `attentions_followups_backstop` LLM pass. A no-op (no artifact / no
        // transcript / no block) when absent; idempotent across re-runs via
        // the store's content dedup.
        //
        // Follow-ups are inherently multi-item — the prompt sanctions a
        // worker mixing channels within one run (e.g. `boss propose
        // followup-task` for item 1, then falling back to the artifact for
        // item 2 if the CLI call fails) — so unlike the worker-signal /
        // deferred-scope seams, this is NOT a single execution-scoped
        // skip-or-run gate: [`WorkDb::reconcile_attentions`]' member-level
        // `content_key` dedup (keyed on `proposed_name` for the `followup`
        // kind) already runs against every existing member of the group,
        // proposal-staged members included, so the legacy chain always runs
        // and lands only the entries a proposal did not already stage —
        // exactly the "filter to uncovered entries" behaviour, driven off
        // the same group the proposal path writes into rather than a
        // separate in-memory predicate.
        //
        // Skipped for `design_postmortem`: its worker prompt never asks for
        // a `FollowupEntry`-shaped artifact (only the stronger
        // `postmortem_followups` schema above), so this would only ever
        // find nothing (or, if reusing the same artifact path, fail to
        // parse it as `FollowupEntry` and log spurious noise).
        if !is_design_postmortem {
            let followup_proposals_first = self.feature_flags.is_enabled("worker_proposals")
                && self.feature_flags.is_enabled("followup_proposals_seam");
            let transcript_path = self.work_db.transcript_path_for_execution(execution_id).ok().flatten();
            // Same generalisation as the postmortem branch above: collect a
            // remote worker's Followups artifact before reading it. Best
            // effort — followups are optional by design (module doc on
            // `reconcile_task_followups`), so a collection failure here must
            // never block the PR transition; it just falls through to the
            // transcript-sentinel / backstop channels below exactly as an
            // absent artifact already does.
            if let RemoteCollectionResult::Failed { host_id, reason } = self
                .collect_remote_structured_output(
                    &completion.execution,
                    crate::structured_output::StructuredOutputKind::Followups,
                )
                .await
            {
                tracing::warn!(
                    execution_id,
                    host_id = %host_id,
                    error = %reason,
                    "followups: remote structured-output collection failed; continuing without the artifact",
                );
            }
            let followups_result = self
                .collaborators
                .attentions_detection
                .reconcile_task_followups(
                    &self.work_db,
                    &work_item_id,
                    execution_id,
                    Some(&self.structured_output_dir),
                    transcript_path.as_deref(),
                )
                .await;
            if let Some((ref group, ref created)) = followups_result {
                if followup_proposals_first {
                    for _ in created {
                        self.record_followup_fallback_hit(&completion.execution, "reconcile_task_followups");
                    }
                }
                self.publish_attentions_created(group, created).await;
            } else if self.feature_flags.is_enabled("attentions_followups_backstop") {
                // Primary found no FOLLOWUPS: block; fall back to the supervisor
                // extraction backstop (flagged `confidence_source = extracted`).
                if let Some((group, created)) = self
                    .collaborators
                    .attentions_detection
                    .extract_followups_backstop(&self.work_db, &work_item_id, execution_id, transcript_path.as_deref())
                    .await
                {
                    if followup_proposals_first {
                        for _ in &created {
                            self.record_followup_fallback_hit(&completion.execution, "attentions_followups_backstop");
                        }
                    }
                    self.publish_attentions_created(&group, &created).await;
                }
            }
        }
        // Reap every engine-owned structured-output artifact this execution
        // produced (followups, PR URL) regardless of outcome — they live in
        // the system temp dir, but delete eagerly rather than waiting on OS
        // reaping.
        crate::structured_output::clear_all(&self.structured_output_dir, execution_id);

        if binding_mismatch {
            StopOutcome::PrDetected { pr_url }
        } else if merged {
            tracing::info!(
                execution_id,
                work_item_id = %work_item_id,
                pr_url = %pr_url,
                source,
                "pr completion: PR already merged; moved work item to done"
            );
            StopOutcome::PrMerged { pr_url }
        } else if matches!(target, WorkerPrCompletionTarget::DoneUnmerged) {
            tracing::info!(execution_id, work_item_id = %work_item_id, pr_url = %pr_url, source,
                "pr completion: PR closed without merging; moved work item to done");
            StopOutcome::PrDetected { pr_url }
        } else if enqueued_reviewer {
            tracing::info!(
                execution_id,
                work_item_id = %work_item_id,
                pr_url = %pr_url,
                source,
                "pr completion: PR detected; reviewer enqueued — \
                 producing task held in active pending review pass",
            );
            StopOutcome::ReviewerEnqueued { pr_url }
        } else {
            tracing::info!(
                execution_id,
                work_item_id = %work_item_id,
                pr_url = %pr_url,
                source,
                "pr completion: PR detected; moved work item to in_review"
            );
            // Pre-fetch CI status so the Review card has a real icon from
            // the first frame. The fetch is fire-and-forget: if it fails or
            // the probe is slow the UI falls back to the in-progress default
            // and the merge-poller sweep picks it up on its next pass.
            let probe = self.merge_probe.clone();
            let work_db = self.work_db.clone();
            let writer = self.pr_poll_state_writer.clone();
            let candidate = PendingMergeCheck {
                work_item_id: work_item_id.clone(),
                product_id: product_id.clone(),
                pr_url: pr_url.clone(),
            };
            boss_task_scope::spawn(async move {
                match probe.probe(&candidate.pr_url).await {
                    Ok(lifecycle_probe) => {
                        writer
                            .update_pr_poll_state(&work_db, &candidate, &lifecycle_probe, "completion")
                            .await;
                    }
                    Err(err) => {
                        tracing::debug!(
                            work_item_id = %candidate.work_item_id,
                            ?err,
                            "pr completion: on-transition CI pre-fetch failed; \
                             merge poller will retry on next sweep",
                        );
                    }
                }
            });
            StopOutcome::PrDetected { pr_url }
        }
    }

    /// True when the live-state registry reports [`boss_protocol::WorkerActivity::Working`]
    /// for `execution_id`'s slot right now — i.e. the worker is mid-turn,
    /// not between turns. `WaitingForInput` is deliberately treated as
    /// parked rather than mid-turn: it can represent a pending notification
    /// after Stop, so treating it as active could retain a genuinely parked
    /// worker for the full staged-recheck horizon. Returns `false` when no
    /// registry is wired (unit tests) or the run has no live slot: an
    /// unobservable execution is never reported as mid-turn, so this
    /// undercounts rather than fabricating reaps.
    pub(super) fn observed_mid_turn(&self, execution_id: &str) -> bool {
        use boss_protocol::WorkerActivity;

        self.live_worker_states
            .as_ref()
            .and_then(|registry| registry.activity_for_run(execution_id))
            .is_some_and(|activity| activity == WorkerActivity::Working)
    }

    /// Whether a mid-turn PR finalization must defer. Every source is
    /// bounded so a wedged-but-alive worker (`WorkerActivity::Working` never
    /// decays via `downgrade_stale_activity`, and `stale_worker_sweep` skips
    /// slots with a tool in flight) cannot leave the execution active forever.
    ///
    /// - Staged recheck measures the horizon from
    ///   [`crate::pr_url_capture::StagedPrUrlEntry::staged_at`].
    /// - Every other source (detector, `stop_satisfied_clean`, …) measures
    ///   the same [`Self::staged_pr_mid_turn_defer_secs`] horizon from
    ///   `LiveWorkerState::last_event_at`, falling back to the execution's
    ///   `started_at`. Once the horizon expires, finalization proceeds and
    ///   [`Self::record_mid_turn_reap`] fires.
    fn should_defer_mid_turn_pr_completion(&self, execution_id: &str, source: &str) -> bool {
        if source == "pr_recheck_staged" {
            return self.should_defer_staged_pr_recheck(execution_id);
        }
        self.should_defer_mid_turn_within_activity_horizon(execution_id)
    }

    /// True while the mid-turn activity anchor is still inside the shared
    /// deferral horizon. Anchor preference: live-state `last_event_at`, then
    /// execution `started_at`. Missing/unparseable anchors fail open (do not
    /// defer) so terminalization cannot hang without a clock.
    fn should_defer_mid_turn_within_activity_horizon(&self, execution_id: &str) -> bool {
        let horizon_secs = self.staged_pr_mid_turn_defer_secs.max(0);
        if horizon_secs == 0 {
            return false;
        }
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let anchor_epoch = self
            .live_worker_states
            .as_ref()
            .and_then(|registry| registry.last_event_at_for_run(execution_id))
            .and_then(|ts| boss_engine_utils::iso8601::parse_iso8601_to_epoch(&ts))
            .or_else(|| {
                self.work_db.get_execution(execution_id).ok().and_then(|execution| {
                    execution
                        .started_at
                        .as_deref()
                        .and_then(boss_engine_utils::iso8601::parse_iso8601_to_epoch)
                })
            });
        match anchor_epoch {
            Some(epoch) => now.saturating_sub(epoch) < horizon_secs,
            None => false,
        }
    }

    /// Fetch the head that is about to be recorded on a PR-completion
    /// teardown. Forensic only: parse failures and `gh` errors log at WARN
    /// and return `None` so terminalization still proceeds.
    pub(super) async fn fetch_pr_head_after(
        &self,
        execution: &crate::work::WorkExecution,
        pr_url: &str,
    ) -> Option<String> {
        let repo_slug = match parse_repo_slug(&execution.repo_remote_url) {
            Ok(repo_slug) => repo_slug,
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    pr_url,
                    ?err,
                    "pr completion: cannot parse repo slug for pr_head_after; recording None",
                );
                return None;
            }
        };
        let Some(pr_number) = pr_number_from_url(pr_url) else {
            tracing::warn!(
                execution_id = %execution.id,
                pr_url,
                "pr completion: cannot parse PR number for pr_head_after; recording None",
            );
            return None;
        };
        match self
            .branch_verifier
            .fetch_pr_head_oid_fresh(&repo_slug, pr_number)
            .await
        {
            Ok(head) => Some(head),
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    pr_url,
                    ?err,
                    "pr completion: fresh pr_head_after read failed; recording None",
                );
                None
            }
        }
    }

    /// Record when a mid-turn finalization proceeds past its deferral horizon:
    /// bump the aggregate and per-source counters and, for revision
    /// implementations, file a sticky attention item on the execution. This
    /// is the explicit escape hatch for a wedged mid-tool worker that never
    /// reaches Stop — not a successful clean-boundary teardown.
    ///
    /// Deliberately does not touch [`crate::live_worker_state::LiveWorkerStateRegistry::release_slot`]'s
    /// own `activity` log line — that remains the forensic record this
    /// only adds a first-class signal alongside, per the postmortem's
    /// explicit forbidden-workaround list.
    async fn record_mid_turn_reap(&self, execution: &crate::work::WorkExecution, source: &'static str) {
        MID_TURN_REAP_TOTAL.inc(&self.metrics);
        self.metrics.counter_inc_by_dynamic(
            &format!("completion.mid_turn_reap.{source}.count"),
            "PR-completion finalizations on this source that observed the producing execution \
             mid-turn (activity=working) rather than idle at finalize time.",
            1,
        );
        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            source,
            "pr completion: finalizing via {source} while producing execution is mid-turn \
             (activity=working) — worker will be reaped before its remaining turn runs",
        );
        if execution.kind != ExecutionKind::RevisionImplementation {
            return;
        }
        let body = format!(
            "The engine finalized this execution's PR via `{source}` while the worker's live \
             activity was still `working` — it had not reached its own Stop boundary. The pane \
             is being torn down immediately after this, so any remaining steps in the worker's \
             current turn (unpushed commits, PR description/comment updates, etc.) will not run.\n\n\
             See tools/boss/docs/postmortems/incident-004-live-revision-workers-reaped-mid-turn.md \
             for the incident this class of reap comes from."
        );
        if let Err(err) = self
            .file_execution_attention(
                execution,
                MID_TURN_REAP_ATTENTION_KIND,
                "Worker reaped mid-turn during PR-completion finalize",
                body,
            )
            .await
        {
            tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "mid-turn reap: failed to file attention item",
            );
        }
    }

    /// Push an `AttentionCreated` event per newly-created member on the
    /// owning product's work-tree topic so the Notifications window and the
    /// design-doc viewer live-update (mirrors the `CreateAttention` RPC
    /// handler). No-op for an empty `created` set.
    pub(super) async fn publish_attentions_created(&self, group: &AttentionGroup, created: &[Attention]) {
        for attention in created {
            self.publisher
                .publish_frontend_event_on_product(
                    &group.product_id,
                    FrontendEvent::AttentionCreated {
                        attention: attention.clone(),
                        group: group.clone(),
                    },
                )
                .await;
        }
    }

    /// Read the newest *Applied* `pr_created` proposal and return its URL
    /// only after verifying the execution branch or revision chain-root
    /// binding. `on_stop_inner`/`recheck_for_pr` then short-circuit the
    /// staging-cache / driver-prose / cold-reconstruction ladder and
    /// finalize from that durable row, which survives an engine restart
    /// between the worker's `cube pr create`/`cube pr update` and this
    /// Stop — unlike the in-memory `StagedPrUrlCache` the primary path
    /// reads.
    ///
    /// [`crate::work::proposal_apply::apply_pr_created`] validates URL shape
    /// and repository at submission time. This read independently verifies
    /// the URL still belongs to the work item (any primary-execution branch,
    /// including a predecessor's, or the resolved chain-root PR for a
    /// revision) before returning it, matching the legacy staged-URL path's
    /// safety boundary.
    ///
    /// Returns `(None, row_existed)` when no operative `pr_created`
    /// proposal is available — the caller then runs the legacy ladder and,
    /// if that ladder actually finalizes, records its fallback hit.
    /// `row_existed` mirrors
    /// [`super::finalize_passes::WorkerCompletionHandler::automation_outcome_from_proposal`]'s
    /// discipline: `false` when no `pr_created` proposal row exists at all
    /// (or the lookup itself errored — fail open to the legacy ladder
    /// rather than silently dropping finalization), `true` when a row
    /// exists but was left in a state this read cannot use.
    ///
    /// A `Rejected` row never changed binding state, so it must not
    /// supersede an earlier `Applied` declaration: the sequence "declare
    /// PR #1 (`Applied`) → declare a conflicting PR #2 (`Rejected`)"
    /// still has a usable URL. [`crate::work::WorkDb::list_worker_proposals_for_execution`]
    /// orders by `created_at DESC`; this read takes the newest `Applied`
    /// row, not merely `proposals.first()`.
    pub(super) async fn pr_created_from_proposal(
        &self,
        execution: &crate::work::WorkExecution,
    ) -> (Option<String>, bool) {
        let execution_id = &execution.id;
        let proposals = match self
            .work_db
            .list_worker_proposals_for_execution(execution_id, ProposalKind::PrCreated)
        {
            Ok(proposals) => proposals,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "pr_created_proposals_seam: failed to look up proposals for this execution; \
                     falling back to the legacy staging-cache / cold-reconstruction ladder",
                );
                return (None, false);
            }
        };
        let row_existed = !proposals.is_empty();
        let Some(applied) = proposals.iter().find(|p| p.state == ProposalState::Applied) else {
            if let Some(latest) = proposals.first() {
                if latest.state == ProposalState::Rejected {
                    tracing::info!(
                        execution_id,
                        proposal_id = %latest.id,
                        state = %latest.state,
                        reason = latest.decision_reason.as_deref().unwrap_or("no reason recorded"),
                        "pr_created_proposals_seam: newest pr_created proposal was rejected at submission \
                         and no Applied row remains; falling back to the legacy ladder",
                    );
                } else {
                    tracing::warn!(
                        execution_id,
                        proposal_id = %latest.id,
                        state = %latest.state,
                        "pr_created_proposals_seam: newest pr_created proposal is unexpectedly not \
                         Applied/Rejected and no Applied row remains; falling back to the legacy ladder",
                    );
                }
            }
            return (None, row_existed);
        };

        match serde_json::from_str::<PrCreatedProposalPayload>(&applied.payload_json) {
            Ok(payload) => {
                let pr_url = payload.pr_url;
                if self.pr_created_url_matches_execution(execution, &pr_url).await {
                    (Some(pr_url), true)
                } else {
                    (None, true)
                }
            }
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    proposal_id = %applied.id,
                    ?err,
                    "pr_created_proposals_seam: Applied pr_created proposal payload_json did not \
                     deserialize; falling back to the legacy ladder",
                );
                (None, true)
            }
        }
    }

    /// Apply the same execution-to-PR association rule used by the staged
    /// URL path before trusting a durable worker declaration. A proposal is
    /// not authority to rebind a task: it must name a primary-execution
    /// branch of this work item (this execution or a predecessor), or, for
    /// a revision, the chain root's already-bound PR.
    async fn pr_created_url_matches_execution(&self, execution: &crate::work::WorkExecution, pr_url: &str) -> bool {
        matches!(
            self.check_pr_url_deliverable(execution, pr_url, "pr_created_proposals_seam")
                .await,
            PrUrlDeliverable::Associated { .. }
        )
    }
}

#[cfg(test)]
mod already_reviewed_at_head_tests {
    use crate::test_support::{create_test_chore_manual, create_test_product};
    use crate::work::{
        CreateExecutionInput, REVIEW_GATE_OUTCOME_COMPLETED_CLEAN, REVIEW_GATE_OUTCOME_GAVE_UP, ReviewVerdictInput,
        WorkDb,
    };
    use boss_protocol::ExecutionKind;

    fn insert_verdict(db: &WorkDb, work_item_id: &str, head_sha: &str, gate_outcome: &'static str) {
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(work_item_id.to_owned())
                    .kind(ExecutionKind::PrReview)
                    .build(),
            )
            .unwrap();
        WorkDb::insert_review_verdict_in_tx(
            &db.connect().unwrap(),
            &execution.id,
            work_item_id,
            &ReviewVerdictInput {
                head_sha: Some(head_sha.to_owned()),
                findings_count: 0,
                revision_warranted: false,
                gate_outcome,
            },
        )
        .unwrap();
    }

    fn already_reviewed_at_head(db: &WorkDb, work_item_id: &str, target_sha: &str) -> bool {
        WorkDb::already_reviewed_at_head_in_tx(&db.connect().unwrap(), work_item_id, target_sha).unwrap()
    }

    /// A legacy reviewer's completed, informative verdict — keyed by the
    /// task itself — matches when its head sha is the current target sha.
    #[test]
    fn matches_a_legacy_verdict_keyed_by_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let product = create_test_product(&db);
        let task = create_test_chore_manual(&db, product.id, "already-reviewed-legacy");
        insert_verdict(&db, &task.id, "head-sha", REVIEW_GATE_OUTCOME_COMPLETED_CLEAN);

        assert!(already_reviewed_at_head(&db, &task.id, "head-sha"));
        assert!(
            !already_reviewed_at_head(&db, &task.id, "some-other-sha"),
            "a verdict for a different head must not match"
        );
    }

    /// A batch leaf's verdict is keyed by the cycle root; passing that id
    /// (as callers now do — see [`enqueue_review_batch`]) matches.
    #[test]
    fn matches_a_batch_leaf_verdict_keyed_by_the_cycle_root() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let product = create_test_product(&db);
        let root = create_test_chore_manual(&db, product.id, "cycle-root");
        insert_verdict(&db, &root.id, "head-sha", REVIEW_GATE_OUTCOME_COMPLETED_CLEAN);

        assert!(already_reviewed_at_head(&db, &root.id, "head-sha"));
    }

    /// A non-informative outcome (e.g. `gave_up`) is not positive evidence
    /// of anything and must not suppress a fresh reviewer pass.
    #[test]
    fn does_not_match_a_non_informative_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let product = create_test_product(&db);
        let task = create_test_chore_manual(&db, product.id, "gave-up");
        insert_verdict(&db, &task.id, "head-sha", REVIEW_GATE_OUTCOME_GAVE_UP);

        assert!(!already_reviewed_at_head(&db, &task.id, "head-sha"));
    }
}

#[cfg(test)]
mod observation_seam_tests {
    use super::fixtures::{PR, lifecycle_probe};
    use super::*;
    use crate::test_support::*;
    use tokio::sync::{Mutex, Notify};

    struct PrefetchProbe {
        fail: bool,
        called: Notify,
    }

    #[async_trait]
    impl MergeProbe for PrefetchProbe {
        async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe> {
            assert_eq!(pr_url, PR);
            self.called.notify_one();
            if self.fail {
                anyhow::bail!("probe unavailable");
            }
            Ok(lifecycle_probe())
        }
    }

    #[derive(Default)]
    struct RecordingWriter {
        calls: Mutex<Vec<(PendingMergeCheck, String, String)>>,
        called: Notify,
    }

    #[async_trait]
    impl PrPollStateWriter for RecordingWriter {
        async fn update_pr_poll_state(
            &self,
            db: &WorkDb,
            candidate: &PendingMergeCheck,
            probe: &PrLifecycleProbe,
            caller: &str,
        ) {
            // The asynchronous pre-fetch must run after the transition commits.
            let item = serde_json::to_value(db.get_work_item(&candidate.work_item_id).unwrap()).unwrap();
            assert_eq!(item["status"], "in_review");
            self.calls
                .lock()
                .await
                .push((candidate.clone(), caller.into(), probe.url.clone()));
            self.called.notify_one();
        }
    }

    async fn check_completion_prefetch(fail: bool) {
        let (dir, db) = open_db_arc();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "PR pre-fetch");
        let execution = create_ready_chore_execution(&db, chore.id.clone());
        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker",
                "mono",
                "lease",
                "workspace",
                dir.path().to_str().unwrap(),
            )
            .unwrap();
        finish_run_worker_pane_alive(&db, &execution.id, &run.id, None);
        let writer = Arc::new(RecordingWriter::default());
        let probe = Arc::new(PrefetchProbe {
            fail,
            called: Notify::new(),
        });
        let handler = super::super::tests::integration_handler(db.clone(), PR)
            .with_max_review_cycles(0)
            .with_merge_probe(probe.clone())
            .with_pr_poll_state_writer(writer.clone());
        let outcome = handler
            .finalize_pr_transition(&execution.id, PR.into(), WorkerPrCompletionTarget::InReview, "stop")
            .await;
        assert_eq!(outcome, StopOutcome::PrDetected { pr_url: PR.into() });
        tokio::time::timeout(Duration::from_secs(5), probe.called.notified())
            .await
            .unwrap();
        if fail {
            // This current-thread test resumes after the probe's immediate error
            // return and its caller's error arm have completed in the same poll.
            assert!(writer.calls.lock().await.is_empty());
        } else {
            tokio::time::timeout(Duration::from_secs(5), writer.called.notified())
                .await
                .unwrap();
            let calls = writer.calls.lock().await;
            assert_eq!(calls.len(), 1);
            let (candidate, caller, url) = &calls[0];
            assert_eq!(candidate.work_item_id, chore.id);
            assert_eq!(candidate.product_id, product.id);
            assert_eq!(candidate.pr_url, PR);
            assert_eq!(caller, "completion");
            assert_eq!(url, PR);
        }
    }

    #[derive(Default)]
    struct RecordingBatchEnqueuer {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl ReviewBatchEnqueuer for RecordingBatchEnqueuer {
        async fn enqueue(
            &self,
            db: &WorkDb,
            work_item_id: &str,
            repo_remote_url: &str,
            pr_url: &str,
            review_pool_size: usize,
        ) -> anyhow::Result<crate::work::ReviewBatchDispatch> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let input = review_batch_input_from_metadata(
                db,
                work_item_id,
                pr_url,
                serde_json::json!({
                    "baseRefOid": "base-sha",
                    "headRefOid": "head-sha",
                    "files": [{"path": "src/lib.rs"}],
                    "additions": 12,
                    "deletions": 3
                }),
            )?;
            db.create_pre_merge_review_batch_for_pool(input, repo_remote_url, review_pool_size)
        }
    }

    /// Exercise both scheduling modes with the same parked and unparked fixture.
    async fn check_parked_completion(park: bool, fanout: bool) {
        let (dir, db) = open_db_arc();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "parked completion");
        let execution = create_ready_chore_execution(&db, chore.id.clone());
        let (execution, run) = db
            .start_execution_run(
                &execution.id,
                "worker",
                "mono",
                "lease",
                "workspace",
                dir.path().to_str().unwrap(),
            )
            .unwrap();
        finish_run_worker_pane_alive(&db, &execution.id, &run.id, None);
        if park {
            db.park_work_item_as_actor(&chore.id, true, Some("not now"), "human")
                .unwrap();
        }
        let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            dir.path().join("feature-flags.toml"),
        ));
        flags.load().unwrap();
        flags.set("review_batch_fanout", fanout).unwrap();
        let enqueuer = Arc::new(RecordingBatchEnqueuer::default());
        let handler = super::super::tests::integration_handler(db.clone(), PR)
            .with_feature_flags(flags)
            .with_review_batch_enqueuer(enqueuer.clone());
        let outcome = handler
            .finalize_pr_transition(&execution.id, PR.into(), WorkerPrCompletionTarget::InReview, "stop")
            .await;
        let has_reviewer = db
            .list_executions(Some(&chore.id))
            .unwrap()
            .iter()
            .any(|execution| execution.kind == ExecutionKind::PrReview);
        let batches = db.review_batches_for_cycle_root(&chore.id).unwrap();
        assert_eq!(
            enqueuer.calls.load(std::sync::atomic::Ordering::Relaxed),
            usize::from(fanout && !park),
            "only an unparked row in fanout mode may call the batch enqueuer"
        );
        if park {
            assert_eq!(outcome, StopOutcome::PrDetected { pr_url: PR.into() });
            assert!(!has_reviewer, "a parked row must not get a pr_review execution");
            assert!(batches.is_empty(), "a parked row must not get a review batch");
            let status = match db.get_work_item(&chore.id).unwrap() {
                WorkItem::Task(t) | WorkItem::Chore(t) => t.status,
                other => panic!("expected a task/chore, got {other:?}"),
            };
            assert_eq!(status, boss_protocol::TaskStatus::InReview);
        } else {
            assert_eq!(outcome, StopOutcome::ReviewerEnqueued { pr_url: PR.into() });
            if fanout {
                assert_eq!(batches.len(), 1, "control: fanout creates a review batch");
            } else {
                assert!(has_reviewer, "control: legacy mode creates a pr_review execution");
                assert!(batches.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn parked_row_completion_schedules_no_reviewer() {
        check_parked_completion(false, false).await;
        check_parked_completion(true, false).await;
    }

    #[tokio::test]
    async fn parked_row_completion_schedules_no_review_batch() {
        check_parked_completion(false, true).await;
        check_parked_completion(true, true).await;
    }

    #[tokio::test]
    async fn completion_prefetch_calls_writer_after_in_review_transition() {
        check_completion_prefetch(false).await;
    }

    #[tokio::test]
    async fn completion_prefetch_does_not_write_after_probe_failure() {
        check_completion_prefetch(true).await;
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod fixtures {
    use super::*;
    use boss_observation_contracts::PrReviewState;

    pub const PR: &str = "https://github.com/spinyfin/mono/pull/42";

    pub fn lifecycle_probe() -> PrLifecycleProbe {
        PrLifecycleProbe::builder()
            .url(PR)
            .state(PrLifecycleState::Open(boss_observation_contracts::OpenPrStatus::clean()))
            .labels(Vec::new())
            .review(PrReviewState::Required)
            .build()
    }
}
