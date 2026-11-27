use super::deferred_review_admission::sweep_deferred_review_admission_with;
use super::host::{ConflictCleared, ConflictResponse, PopulationRequest, SweepHost};
use super::post_merge_review::maybe_trigger_post_merge_review;
use super::*;
use boss_engine_storage_values::attention_kinds::PR_BASE_REFUSED;

/// Cancel pending reviews of a PR that merged or closed: live review-guide
/// attempts, single-reviewer `pr_review` executions, and live pre-merge review
/// batches.
///
/// Bound review-guide
/// executions are cancelled (and force-released when a completion handler is
/// wired) so an Astra-high generation does not keep burning tokens after the
/// PR is gone. Unbound queued attempts are cancelled in the same pass.
pub(crate) async fn stop_live_review_guides(
    work_db: &WorkDb,
    completion_handler: Option<&dyn CompletionRequests>,
    chain_root_id: &str,
    pr_url: &str,
    reason: &str,
) {
    let mut execution_ids = match work_db.cancel_live_pr_review_guides_for_root(chain_root_id, pr_url, reason) {
        Ok(ids) => ids,
        Err(err) => {
            tracing::warn!(
                chain_root_id,
                pr_url,
                ?err,
                "merge poller: failed to cancel live review-guide attempts",
            );
            Vec::new()
        }
    };
    match work_db.cancel_single_pr_reviews_for_chain(chain_root_id, pr_url, reason) {
        Ok(ids) => execution_ids.extend(ids),
        Err(err) => tracing::warn!(
            chain_root_id,
            pr_url,
            ?err,
            "merge poller: failed to cancel pending single-reviewer PR reviews",
        ),
    }
    // Batch members are retired (and their panes torn down by the terminal
    // event) in the same transaction that fails the batch.
    match work_db.fail_pre_merge_review_batches_for_ended_pr(chain_root_id, pr_url) {
        Ok(batch_ids) if !batch_ids.is_empty() => tracing::info!(
            chain_root_id,
            pr_url,
            reason,
            batches = batch_ids.len(),
            "merge poller: failed pre-merge review batches of an ended PR",
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!(
            chain_root_id,
            pr_url,
            ?err,
            "merge poller: failed to fail pre-merge review batches of an ended PR",
        ),
    }
    let Some(handler) = completion_handler else {
        return;
    };
    for execution_id in execution_ids {
        handler.force_release(&execution_id).await;
    }
}

/// The engine services one sweep step may call. Each is absent when the sweep
/// runs bare — a pass that only observes and promotes.
#[derive(Clone, Copy)]
pub struct SweepHandlers<'a> {
    /// Cube workspaces, for the conflict ladder's inline fallback and retire path.
    pub cube_client: Option<&'a dyn CubeClient>,
    /// The completion service's requests (late-PR recheck, worker teardown, feature flags).
    pub completion_handler: Option<&'a dyn CompletionRequests>,
    /// Conflict handling, design-doc maintenance, population and the loop passes.
    pub host: Option<&'a dyn SweepHost>,
    /// Parked work-item ids preloaded for this pass. `None` looks up each
    /// candidate on its own.
    pub parked_ids: Option<&'a std::collections::HashSet<String>>,
}

/// Reconcile exactly one PR instead of sweeping every candidate (doc
/// `github-event-detection-webhooks-vs-polling-2026-07-08.md` §9 item 3).
/// This is the targeted entry point [`PrPollSchedule`]'s per-PR adaptive
/// timer and the `PrReconcileRequested` event-bus topic both drive:
/// rather than waking the whole reconciler because one PR needs
/// attention, reconcile just that PR.
///
/// Scopes every per-PR candidate list [`run_one_pass`] considers —
/// [`WorkDb::list_chores_pending_merge_check`],
/// [`WorkDb::list_chores_blocked_on_merge_conflict`],
/// [`WorkDb::list_chores_blocked_on_ci_failure`], and
/// [`WorkDb::list_chores_stranded_blocked_remediation`] — down to rows
/// matching `pr_url`, probes just that PR, and runs the same detection
/// paths [`sweep_one`] does, plus the merge-queue-rebounce and stalled-
/// reviewer checks. Deliberately NOT scoped here: the *execution*-keyed
/// candidate lists (`pending_pr_recheck`, `late_pr_candidates`,
/// `stranded_ci_attempts`) — a task with no `pr_url` yet by definition
/// can't be addressed by a `pr_url`-keyed entry point. Those stay on the
/// periodic full sweep, which remains the correctness backstop.
///
/// Returns the sweep outcome alongside the [`PollTier`] observed for
/// `pr_url` so the caller can reschedule its next adaptive poll. The
/// tier is `None` when `pr_url` is no longer a live candidate (merged,
/// closed, or otherwise dropped out of every list) — callers should stop
/// tracking it until the next full sweep re-discovers it.
///
/// A thin wrapper over [`reconcile_batch`]: the adaptive timer reconciles a
/// coalesced due set rather than one PR at a time, and a single-PR reconcile
/// is just that batch with one member.
pub async fn reconcile_one(
    work_db: &WorkDb,
    probe: &dyn MergeProbe,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
    completion_handler: Option<&dyn CompletionRequests>,
    host: Option<&dyn SweepHost>,
    pr_url: &str,
) -> (SweepOutcome, Option<PollTier>) {
    let urls = [pr_url.to_owned()];
    let (outcome, observations) =
        reconcile_batch(work_db, probe, publisher, cube_client, completion_handler, host, &urls).await;
    let tier = observations
        .into_iter()
        .find(|(url, _)| url == pr_url)
        .and_then(|(_, t)| t);
    (outcome, tier)
}

/// [`reconcile_one`] for a whole due set at once: the same scoped detection
/// paths, but **one** batched lifecycle probe and **one** batched
/// merge-queue dequeue query covering every PR in `pr_urls`.
///
/// This is the shape the cost argument turns on. GitHub bills a GraphQL
/// query `max(1, nodes/100)` points, so a query for one PR costs the same
/// single point as a query for the ~100 nodes' worth of PRs that fit
/// alongside it. Reconciling `N` due PRs one at a time therefore cost
/// `2N` points (a probe and a dequeue query each, both floored to 1),
/// against `ceil(51N/100) + ceil(20N/100)` for the batch — at N=25, 50
/// points collapse to 18. The per-PR path also re-ran all four candidate
/// list queries and the stalled-reviewer query once per PR; batching runs
/// each exactly once.
///
/// Returns the aggregate outcome plus one [`PollObservation`] per requested
/// URL, in the order requested. A URL with no live candidate on any list
/// yields `None` (it is not probed at all), exactly as the single-PR path
/// did.
pub async fn reconcile_batch(
    work_db: &WorkDb,
    probe: &dyn MergeProbe,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
    completion_handler: Option<&dyn CompletionRequests>,
    host: Option<&dyn SweepHost>,
    pr_urls: &[String],
) -> (SweepOutcome, Vec<PollObservation>) {
    let mut outcome = SweepOutcome::default();
    let wanted: std::collections::HashSet<&str> = pr_urls.iter().map(String::as_str).collect();
    if wanted.is_empty() {
        return (outcome, Vec::new());
    }
    if let Some(host) = host {
        super::disarm_drain::drain_pending_merge_disarms(work_db, probe, publisher, host).await;
    }

    let in_review = candidates_for_pr_urls(work_db.list_chores_pending_merge_check(), &wanted);
    let blocked_conflict = candidates_for_pr_urls(work_db.list_chores_blocked_on_merge_conflict(), &wanted);
    let blocked_ci = candidates_for_pr_urls(work_db.list_chores_blocked_on_ci_failure(), &wanted);
    let blocked_other = candidates_for_pr_urls(work_db.list_chores_blocked_other_with_pr(), &wanted);
    let stranded_blocked = candidates_for_pr_urls(work_db.list_chores_stranded_blocked_remediation(), &wanted);
    // Watched for a merge or a close only; see `run_one_pass`.
    let held_active = candidates_for_pr_urls(work_db.list_chores_held_active_with_pr(), &wanted);
    let orphaned_chains: Vec<crate::work::OrphanedRevisionChain> =
        match work_db.list_orphaned_in_review_revision_chains() {
            Ok(chains) => chains
                .into_iter()
                .filter(|chain| wanted.contains(chain.pr_url.as_str()))
                .collect(),
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "merge poller: targeted reconcile orphaned-chain list query failed"
                );
                Vec::new()
            }
        };

    // Probe only the requested URLs that still have a live candidate row.
    // A URL that has dropped off every list costs nothing here and is
    // reported as "no tier", which retires its adaptive slot.
    let mut probe_seen = std::collections::HashSet::new();
    let probe_urls: Vec<String> = in_review
        .iter()
        .chain(blocked_conflict.iter())
        .chain(blocked_ci.iter())
        .chain(blocked_other.iter())
        .chain(stranded_blocked.iter())
        .chain(held_active.iter())
        .map(|candidate| candidate.pr_url.clone())
        .chain(orphaned_chains.iter().map(|chain| chain.pr_url.clone()))
        .filter(|url| probe_seen.insert(url.clone()))
        .collect();
    if probe_urls.is_empty() {
        tracing::debug!(
            pr_count = pr_urls.len(),
            "merge poller: reconcile found no live candidate for any requested PR; \
             skipping until the next full sweep",
        );
        return (outcome, pr_urls.iter().map(|url| (url.clone(), None)).collect());
    }
    let source_observation_sequences = allocate_source_observation_sequences(work_db, completion_handler, &probe_urls);
    let probe_results = probe.probe_batch(&probe_urls).await;

    let mut seen = std::collections::HashSet::new();
    for candidate in in_review
        .iter()
        .chain(blocked_conflict.iter())
        .chain(blocked_ci.iter())
        .chain(blocked_other.iter())
    {
        if !seen.insert(candidate.work_item_id.clone()) {
            continue;
        }
        sweep_one(
            work_db,
            &probe_results,
            &source_observation_sequences,
            (publisher, "adaptive_batch"),
            SweepHandlers {
                cube_client,
                completion_handler,
                host,
                parked_ids: None,
            },
            candidate,
            &mut outcome,
        )
        .await;
    }
    for candidate in &held_active {
        // No open-PR upkeep for a held row: only a merge or a close is acted on.
        let terminal = matches!(
            probe_results.get(&candidate.pr_url),
            Some(Ok(probe)) if !matches!(probe.state, PrLifecycleState::Open(_))
        );
        if !terminal || !seen.insert(candidate.work_item_id.clone()) {
            continue;
        }
        sweep_one(
            work_db,
            &probe_results,
            &source_observation_sequences,
            (publisher, "adaptive_batch"),
            SweepHandlers {
                cube_client,
                completion_handler,
                host,
                parked_ids: None,
            },
            candidate,
            &mut outcome,
        )
        .await;
    }
    for chain in &orphaned_chains {
        if let Some(Ok(probe)) = probe_results.get(&chain.pr_url) {
            settle_orphaned_revision_chain(work_db, publisher, completion_handler, chain, probe, &mut outcome).await;
        }
    }
    for candidate in &stranded_blocked {
        sweep_stranded_blocked_remediation(work_db, &probe_results, publisher, host, candidate, &mut outcome).await;
    }
    // See the matching gate in `run_one_pass`: a `trunk_queue` product's
    // dequeue/eviction signal is owned by the Trunk queue poller, not
    // GitHub's merge-queue timeline. One batched dequeue query for the whole
    // due set, exactly like the full sweep's.
    run_merge_queue_rebounce_pass(work_db, publisher, &in_review, &blocked_ci, None, &mut outcome).await;

    let reviewer_stale_secs: u64 = 10 * 60;
    let skip_unbatched_review_holds = completion_handler
        .map(|handler| handler.review_batch_fanout_enabled())
        .unwrap_or(false);
    match work_db.list_tasks_with_stalled_reviewer(reviewer_stale_secs) {
        Ok(stalled) => {
            for (task_id, product_id, stalled_pr_url, review_result_written) in
                stalled.iter().filter(|(_, _, url, _)| wanted.contains(url.as_str()))
            {
                sweep_stalled_reviewer(
                    work_db,
                    publisher,
                    StalledReview {
                        task_id,
                        product_id,
                        pr_url: stalled_pr_url,
                        review_result_written: *review_result_written,
                        skip_unbatched_review_holds,
                    },
                    &GhPrStateChecker,
                    &mut outcome,
                )
                .await;
            }
        }
        Err(err) => tracing::warn!(?err, "merge poller: failed to list stalled reviewer tasks"),
    }
    if skip_unbatched_review_holds && let Some(handler) = completion_handler {
        let pool_size = handler.review_pool_size();
        sweep_deferred_review_admission_with(
            work_db,
            publisher,
            handler.review_batch_enqueuer(),
            Some(&wanted),
            pool_size,
            &mut outcome,
        )
        .await;
    }

    // Observe every *requested* URL, not just the probed ones, so a URL that
    // has left every candidate list reports `None` and loses its slot rather
    // than silently keeping one.
    (outcome, poll_observations(pr_urls, &probe_results))
}

/// Filter a candidate-list query result down to rows whose PR is in
/// `wanted`, logging (not propagating) a query failure — [`reconcile_batch`]
/// is best-effort per list, matching [`run_one_pass`]'s own error handling.
pub(crate) fn candidates_for_pr_urls(
    result: Result<Vec<PendingMergeCheck>>,
    wanted: &std::collections::HashSet<&str>,
) -> Vec<PendingMergeCheck> {
    match result {
        Ok(items) => items
            .into_iter()
            .filter(|c| wanted.contains(c.pr_url.as_str()))
            .collect(),
        Err(err) => {
            tracing::warn!(?err, "merge poller: targeted reconcile candidate list query failed");
            Vec::new()
        }
    }
}

/// Stop every in-flight `revision_implementation` execution belonging to
/// revisions of `chain_root_id` now that the parent PR has merged.
///
/// The DB transaction in `mark_chore_pr_merged` already resolves each
/// revision task (via `block_pending_revisions_on_parent_close`): a live
/// PR-review revision becomes an autostart followup, while other revision
/// kinds are archived or blocked according to their lifecycle. This function
/// handles the execution side: force-release each cube workspace lease,
/// cancel the obsolete execution, and reconcile any converted followup.
///
/// When `completion_handler` is `None` (tests, cold-path wiring) this
/// function cannot perform execution-side cleanup and is a no-op.
pub async fn stop_active_revision_executions(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    completion_handler: Option<&dyn CompletionRequests>,
    chain_root_id: &str,
    outcome: &mut SweepOutcome,
) {
    let Some(handler) = completion_handler else {
        return;
    };
    let executions = match work_db.list_active_revision_executions_for_chain(chain_root_id) {
        Ok(execs) => execs,
        Err(err) => {
            tracing::warn!(
                chain_root_id,
                ?err,
                "merge poller: failed to list active revision executions for chain; \
                 revision tasks are already blocked but their leases may not be released",
            );
            return;
        }
    };
    for execution in &executions {
        tracing::info!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            chain_root_id,
            "merge poller: stopping revision execution — parent PR merged",
        );
        // Persist the exact engine-assigned workspace before force_release
        // clears the live lease columns. A converted review followup uses
        // this durable pointer to reclaim uncommitted edits; no workspace
        // name inference is involved.
        if let Err(err) = work_db.preserve_execution_workspace_preference(&execution.id) {
            tracing::warn!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                ?err,
                "merge poller: failed to preserve revision workspace preference",
            );
        }
        // Release the pane and cube workspace lease without altering
        // execution status (force_release does not change status).
        handler.force_release(&execution.id).await;
        // Now mark the obsolete revision execution terminal. The task has
        // already reached its parent-close state in the DB transaction, so
        // cancel's active-to-todo demotion does not overwrite that decision.
        let prior_status = execution.status.clone();
        let cancel_result = work_db.cancel_execution_with(
            &execution.id,
            crate::work::CancelExecutionOpts {
                reason: Some("parent PR merged".to_owned()),
                queued_only: false,
                record_failure_reason: false,
            },
        );
        match &cancel_result {
            Ok(cancelled) => {
                outcome.revision_invalidated += 1;
                handler
                    .record_parent_pr_merge_cancellation(cancelled, prior_status, chain_root_id)
                    .await;
            }
            Err(err) => {
                // The execution may have already moved to a terminal state
                // (raced with the worker finishing, or a prior sweep). That
                // is not itself a problem — the reconcile below still runs
                // and picks up the now-terminal row — so log at debug.
                tracing::debug!(
                    execution_id = %execution.id,
                    ?err,
                    "merge poller: cancel_execution failed for revision (may already be terminal)",
                );
            }
        }
    }

    // Reconcile once for the chain root's product after lease cleanup,
    // including when there were no leased executions. A pre-run review
    // revision is abandoned during conversion and never appears in the
    // lease query. Terminal executions (including cancellation races) can
    // now receive a fresh followup execution through ordinary admission.
    match work_db.get_work_item(chain_root_id) {
        Ok(work_item) => {
            let product_id = work_item.product_id().to_owned();
            match work_db.reconcile_product_executions(&product_id) {
                Ok(result) => {
                    tracing::info!(
                        chain_root_id,
                        product_id,
                        created = result.created.len(),
                        updated = result.updated.len(),
                        "merge poller: reconciled executions after parent PR merge",
                    );
                    publisher.kick_scheduler();
                }
                Err(err) => tracing::warn!(
                    chain_root_id,
                    product_id,
                    ?err,
                    "merge poller: failed to reconcile followups after parent PR merge",
                ),
            }
        }
        Err(err) => tracing::warn!(
            chain_root_id,
            ?err,
            "merge poller: failed to resolve chain root after parent PR merge",
        ),
    }
}

/// Stop the live worker execution for `work_item_id` after its task
/// auto-transitioned back to `in_review` because the engine detected its
/// PR's CI had gone green (`on_ci_resolved`).
///
/// The worker that was running the task has nothing useful left to do:
/// the task reaching Review means its job is done. In the observed bug
/// (issue #898) the worker sat in `waiting_for_input`, polling CI checks
/// for the very fix the engine had already observed as green, holding a
/// worker slot indefinitely. We force-stop it regardless of what it is
/// doing — cancel the execution row and release its cube lease + pane.
///
/// `CompletionRequests::force_stop_execution` only demotes a task
/// that is still `active`; since the task is now `in_review`, that guard
/// does not fire and the task stays in Review. Idempotent: a no-op when
/// no live execution exists or `completion_handler` is `None` (tests /
/// cold-path wiring).
pub(crate) async fn stop_worker_on_review_transition(
    work_db: &WorkDb,
    completion_handler: Option<&dyn CompletionRequests>,
    work_item_id: &str,
    outcome: &mut SweepOutcome,
) {
    let Some(handler) = completion_handler else {
        return;
    };
    // `exclude_id = ""` matches no real execution, so this returns the
    // genuinely-live worker for the task (not a phantom terminal row left
    // by a re-dispatch storm — see `get_live_execution_for_work_item`).
    let execution = match work_db.get_live_execution_for_work_item(work_item_id, "") {
        Ok(Some(exec)) => exec,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(
                work_item_id,
                ?err,
                "merge poller: failed to look up live execution to stop after Review transition; \
                 task is in_review but a worker may still be holding a slot",
            );
            return;
        }
    };
    tracing::info!(
        execution_id = %execution.id,
        work_item_id,
        "merge poller: stopping worker — task auto-transitioned to in_review (CI green)",
    );
    handler.force_stop_execution(&execution.id).await;
    outcome.worker_stopped_on_review += 1;
}

/// Re-run PR detection against an execution that the on-Stop hook
/// classified as having no PR but whose chore is still `active` (i.e.,
/// the worker stopped, the engine missed the PR-open transition, and
/// the chore is stuck in `active`). Delegates to
/// `CompletionRequests::recheck_for_pr`, which transitions the
/// chore on `Fresh`/`Merged` and stays quiet on the no-PR / stale-PR
/// branches so the poller doesn't spam probes or awaiting-input events.
pub(crate) async fn sweep_pending_pr(handler: &dyn CompletionRequests, execution_id: &str, outcome: &mut SweepOutcome) {
    match handler.recheck_for_pr(execution_id).await {
        StopOutcome::PrDetected { pr_url } => {
            outcome.pr_recheck_recovered += 1;
            tracing::info!(
                execution_id,
                pr_url = %pr_url,
                "merge poller: recovered missed PR-open for waiting_human worker",
            );
        }
        StopOutcome::PrMerged { pr_url } => {
            outcome.pr_recheck_recovered += 1;
            tracing::info!(
                execution_id,
                pr_url = %pr_url,
                "merge poller: recovered missed PR-open (PR already merged) for waiting_human worker",
            );
        }
        // The PR was detected and an independent reviewer pass
        // was enqueued; the producing task is held in active. Count this as a
        // recovery since the producing execution is finalized and progressing.
        StopOutcome::ReviewerEnqueued { pr_url } => {
            outcome.pr_recheck_recovered += 1;
            tracing::info!(
                execution_id,
                pr_url = %pr_url,
                "merge poller: recovered missed PR-open; reviewer enqueued, \
                 producing task held for review pass",
            );
        }
        // Quiet branches — still no PR, transient detector failure,
        // or the execution moved on between list and recheck. Log at
        // info so a worker stuck in `waiting_human` with `pr_url=null`
        // leaves a breadcrumb on every sweep instead of failing
        // silently. Without this, the 2026-05-13 three-concurrent-
        // workers regression (where Worf/Crusher/Troi pushed real PRs
        // but the engine never bound them) had zero engine-log
        // evidence — the merge poller was running, the candidate query
        // listed the executions, but the recheck loop's silent return
        // hid the fact that `detect_pr` was returning Stale/None on
        // every pass.
        quiet @ (StopOutcome::AwaitingInput
        | StopOutcome::DetectorFailed
        | StopOutcome::StalePr { .. }
        | StopOutcome::EmptyDiffPr { .. }) => {
            outcome.pr_recheck_unresolved += 1;
            tracing::info!(
                execution_id,
                outcome = ?quiet,
                "merge poller: PR-detection recheck did not resolve this pass — \
                 worker still listed as waiting_human with no `pr_url`; \
                 will retry on next sweep (see `pr_detect:` log above for \
                 the underlying detector classification)",
            );
        }
        // These six are genuinely silent — the execution moved on
        // between `list` and `recheck` (raced with on-Stop / manual
        // intervention), hit a transient DB error, the running-
        // status gate (AI #6) skipped the fallback because the worker
        // is still alive, or the human flipped the
        // `detect_pr_cold_fallback` feature flag OFF (AI #5). No log
        // on these: they're not stuck-worker indicators.
        StopOutcome::AlreadyTerminal
        | StopOutcome::UnknownExecution
        | StopOutcome::SupersededInWorkspace
        | StopOutcome::NoWorkspace
        | StopOutcome::RunningNoStagedPr
        | StopOutcome::FallbackDisabledByFlag
        // `recheck_for_pr` never parks via the breaker (only the on-Stop
        // path nudges); covered here for exhaustiveness. SignalAlreadyCleared
        // is also only reachable via on-Stop, not recheck_for_pr.
        | StopOutcome::NudgeBreakerParked { .. }
        | StopOutcome::NudgeDebounced
        | StopOutcome::SignalAlreadyCleared { .. }
        // DeliverableSatisfied is only reachable via the on-Stop path
        // (try_finalize_satisfied_deliverable_on_stop); covered for exhaustiveness.
        | StopOutcome::DeliverableSatisfied { .. }
        // The run-done gate and its backstop are on-Stop only, by
        // construction: `recheck_for_pr`/`recheck_for_pr_late` run for
        // executions whose worker may have died without a clean Stop, and
        // "no declaration" is exactly what a dead worker looks like — so
        // the sweep must never reach a verdict on it. Covered for
        // exhaustiveness.
        | StopOutcome::AwaitingRunDoneDeclaration { .. }
        | StopOutcome::RunUndeclaredParked { .. }
        // FlakyRetriggered is only reachable via the on-Stop path (it gates
        // on `execution.kind == "ci_remediation"`), never from a recheck.
        | StopOutcome::FlakyRetriggered { .. }
        // Maint task 6: an automation_triage outcome only comes from the
        // on-Stop detector, never from a PR-detection recheck.
        | StopOutcome::AutomationTriage { .. }
        // P3b: an answer_agent outcome only comes from the on-Stop
        // finalizer, never from a PR-detection recheck.
        | StopOutcome::AnswerAgent { .. }
        // A progression outcome only comes from the on-Stop finalizer.
        | StopOutcome::Progression
        // A review-guide outcome only comes from the on-Stop finalizer,
        // never from a PR-detection recheck (review-guide executions never
        // produce a PR at all).
        | StopOutcome::ReviewGuide { .. }
        // ReviewerEnqueued is handled in its own arm above.
        // ReviewPassCompleted/ReviewPassRevisionCreated/ReviewPassAwaitingResult
        // only come from on-Stop (reviewer finalisation).
        | StopOutcome::ReviewPassCompleted { .. }
        | StopOutcome::ReviewPassRevisionCreated { .. }
        | StopOutcome::ReviewPassAwaitingResult
        // The no-op terminal is only reachable on the on-Stop path
        // (it reads the worker's `run_done` declaration),
        // never from a PR-detection recheck.
        | StopOutcome::NoChangesNeeded { .. }
        // EscalationPending is only reachable via `nudge_or_park` on the
        // on-Stop path, never from a PR-detection recheck.
        | StopOutcome::EscalationPending { .. }
        // BuildWaitPending is only reachable via `nudge_or_park` on the
        // on-Stop path (it reads the worker's Stop-boundary transcript for
        // the build-wait heuristic), never from a PR-detection recheck.
        | StopOutcome::BuildWaitPending { .. }
        // BackgroundChildrenPending is only reachable via `nudge_or_park`
        // on the on-Stop path (it probes the worker's live process tree),
        // never from a PR-detection recheck.
        | StopOutcome::BackgroundChildrenPending { .. }
        // Held is only reachable via `nudge_or_park` on the on-Stop path
        // (it checks the operator hold registry), never from a
        // PR-detection recheck.
        | StopOutcome::Held { .. }
        | StopOutcome::WorkerWaitPending { .. }
        // DriverTerminalError and RemoteCollectionFailed are only reachable
        // via `on_stop_inner`'s early gate or a kind finalizer, which `recheck_for_pr` never
        // receives (it has no live Stop event to read a reason from) —
        // covered for exhaustiveness.
        | StopOutcome::DriverTerminalError { .. }
        | StopOutcome::RemoteCollectionFailed { .. }
        | StopOutcome::DbError
        // DeferredForProbeTurn is only reachable via `on_stop_inner`'s
        // probe-delivery gate on the on-Stop path (it depends on this same
        // boundary's pre-completion probe pass), never from a PR-detection
        // recheck — covered for exhaustiveness.
        | StopOutcome::DeferredForProbeTurn
        // A `run_done` declaration finalizes synchronously from the
        // SubmitProposal RPC handler (`app::proposals`), never from
        // `recheck_for_pr` — covered for exhaustiveness.
        | StopOutcome::RunDoneDeclaredWithoutDelivery { .. }
        // The foreign-PR guard already failed the execution and filed its
        // attention item; nothing further for the sweep to do.
        | StopOutcome::ForeignPrRefused { .. } => {}
        // The base guard filed its attention item and parked the execution
        // awaiting a retarget; the next sweep re-evaluates it.
        StopOutcome::PrBaseRefused { pr_url, .. } => {
            outcome.pr_recheck_unresolved += 1;
            tracing::info!(
                execution_id,
                pr_url = %pr_url,
                "merge poller: PR-detection recheck refused by the base policy — PR must be retargeted to the default branch",
            );
        }
    }
}

/// Run late-PR detection against a terminal execution (abandoned /
/// completed / failed within the recent lookback window) whose task is
/// still `active` with no `pr_url`. Delegates to
/// `CompletionRequests::recheck_for_pr_late`, which bypasses the
/// `AlreadyTerminal` gate and calls
/// [`WorkDb::bind_pr_to_active_task_from_terminal_execution`] directly
/// on a positive detection result.
pub(crate) async fn sweep_late_pr(
    handler: &dyn CompletionRequests,
    candidate: &LatePrCandidate,
    outcome: &mut SweepOutcome,
) {
    match handler.recheck_for_pr_late(candidate).await {
        StopOutcome::PrDetected { pr_url } | StopOutcome::PrMerged { pr_url } => {
            outcome.late_pr_recovered += 1;
            tracing::info!(
                execution_id = %candidate.execution_id,
                work_item_id = %candidate.work_item_id,
                pr_url = %pr_url,
                "merge poller: late PR bound to active task (double-spawn recovery)",
            );
        }
        // No PR yet or stale — retry next sweep, no log spam.
        StopOutcome::AwaitingInput
        | StopOutcome::StalePr { .. }
        | StopOutcome::EmptyDiffPr { .. }
        | StopOutcome::DetectorFailed => {
            tracing::debug!(
                execution_id = %candidate.execution_id,
                work_item_id = %candidate.work_item_id,
                "merge poller: late-PR recheck did not resolve — will retry next sweep",
            );
        }
        // Genuinely silent: execution/task moved on between list and recheck.
        StopOutcome::AlreadyTerminal
        | StopOutcome::UnknownExecution
        | StopOutcome::SupersededInWorkspace
        | StopOutcome::NoWorkspace
        | StopOutcome::RunningNoStagedPr
        | StopOutcome::FallbackDisabledByFlag
        // `recheck_for_pr_late` never parks via the breaker; covered for
        // exhaustiveness. SignalAlreadyCleared is only reachable via on-Stop.
        | StopOutcome::NudgeBreakerParked { .. }
        | StopOutcome::NudgeDebounced
        | StopOutcome::SignalAlreadyCleared { .. }
        // DeliverableSatisfied is only reachable via the on-Stop path
        // (try_finalize_satisfied_deliverable_on_stop); covered for exhaustiveness.
        | StopOutcome::DeliverableSatisfied { .. }
        // The run-done gate and its backstop are on-Stop only, by
        // construction: `recheck_for_pr`/`recheck_for_pr_late` run for
        // executions whose worker may have died without a clean Stop, and
        // "no declaration" is exactly what a dead worker looks like — so
        // the sweep must never reach a verdict on it. Covered for
        // exhaustiveness.
        | StopOutcome::AwaitingRunDoneDeclaration { .. }
        | StopOutcome::RunUndeclaredParked { .. }
        // FlakyRetriggered is only reachable via the on-Stop path (it gates
        // on `execution.kind == "ci_remediation"`), never from a recheck.
        | StopOutcome::FlakyRetriggered { .. }
        // Maint task 6: an automation_triage outcome only comes from the
        // on-Stop detector, never from a PR-detection recheck.
        | StopOutcome::AutomationTriage { .. }
        // P3b: an answer_agent outcome only comes from the on-Stop
        // finalizer, never from a late-PR recheck.
        | StopOutcome::AnswerAgent { .. }
        // A progression outcome only comes from the on-Stop finalizer.
        | StopOutcome::Progression
        // A review-guide outcome only comes from the on-Stop finalizer,
        // never from a late-PR recheck (review-guide executions never
        // produce a PR at all).
        | StopOutcome::ReviewGuide { .. }
        // reviewer-related outcomes are handled on the on-Stop
        // path; covered here for exhaustiveness.
        | StopOutcome::ReviewerEnqueued { .. }
        | StopOutcome::ReviewPassCompleted { .. }
        | StopOutcome::ReviewPassRevisionCreated { .. }
        | StopOutcome::ReviewPassAwaitingResult
        // The no-op terminal is only reachable on the on-Stop path,
        // never from a late-PR recheck.
        | StopOutcome::NoChangesNeeded { .. }
        // EscalationPending is only reachable via `nudge_or_park` on the
        // on-Stop path, never from a late-PR recheck.
        | StopOutcome::EscalationPending { .. }
        // BuildWaitPending is only reachable via `nudge_or_park` on the
        // on-Stop path, never from a late-PR recheck.
        | StopOutcome::BuildWaitPending { .. }
        // BackgroundChildrenPending is only reachable via `nudge_or_park`
        // on the on-Stop path, never from a late-PR recheck.
        | StopOutcome::BackgroundChildrenPending { .. }
        // Held is only reachable via `nudge_or_park` on the on-Stop path,
        // never from a late-PR recheck.
        | StopOutcome::Held { .. }
        | StopOutcome::WorkerWaitPending { .. }
        // DriverTerminalError and RemoteCollectionFailed are only reachable
        // from the on-Stop path, never from a late-PR recheck.
        | StopOutcome::DriverTerminalError { .. }
        | StopOutcome::RemoteCollectionFailed { .. }
        | StopOutcome::DbError
        // DeferredForProbeTurn is only reachable via `on_stop_inner`'s
        // probe-delivery gate on the on-Stop path, never from a late-PR
        // recheck — covered for exhaustiveness.
        | StopOutcome::DeferredForProbeTurn
        // A `run_done` declaration finalizes synchronously from the
        // SubmitProposal RPC handler (`app::proposals`), never from a
        // late-PR recheck — covered for exhaustiveness.
        | StopOutcome::RunDoneDeclaredWithoutDelivery { .. }
        // The late-PR recheck binds through `bind_pr_to_active_task_from_terminal_execution`,
        // never through `finalize_pr_transition`, so the foreign-PR guard's
        // refusal cannot surface here — covered for exhaustiveness.
        | StopOutcome::ForeignPrRefused { .. } => {}
        // Late-PR recheck applies the base policy before binding. A refused
        // PR stays unbound for a later pass (or the abandoned-branch sweep).
        StopOutcome::PrBaseRefused { pr_url, .. } => {
            tracing::info!(
                execution_id = %candidate.execution_id,
                work_item_id = %candidate.work_item_id,
                pr_url = %pr_url,
                "merge poller: late-PR recheck refused by the base policy — PR must be retargeted to the default branch",
            );
        }
    }
}

pub(crate) async fn sweep_one(
    work_db: &WorkDb,
    probe_results: &HashMap<String, std::result::Result<PrLifecycleProbe, String>>,
    source_observation_sequences: &HashMap<String, i64>,
    publication: (&dyn ExecutionPublisher, &'static str),
    handlers: SweepHandlers<'_>,
    candidate: &PendingMergeCheck,
    outcome: &mut SweepOutcome,
) {
    let (publisher, caller) = publication;
    let SweepHandlers {
        cube_client,
        completion_handler,
        host,
        parked_ids,
    } = handlers;
    let probe_result = match probe_results.get(&candidate.pr_url) {
        Some(Ok(state)) => state.clone(),
        Some(Err(err)) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                err,
                "merge poller: probe failed; will retry next pass",
            );
            return;
        }
        None => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "merge poller: no batched probe result for this PR; will retry next pass",
            );
            return;
        }
    };
    let base_attentions = match open_base_refusals(work_db, candidate) {
        Some(items) => items,
        None => return,
    };
    // A closed-unmerged PR never landed anywhere, so its base needs no check:
    // it falls through to the ordinary closed-unmerged retirement.
    if !matches!(probe_result.state, PrLifecycleState::ClosedUnmerged)
        && refuse_unlanded_base(work_db, publisher, candidate, &probe_result, &base_attentions).await
    {
        update_pr_poll_state(work_db, publisher, candidate, &probe_result, None, caller).await;
        return;
    }
    // A binding-repair refusal can describe a different PR. Only clear evidence
    // about this probe, and avoid a write when there is nothing open to clear.
    let belongs_to_probe = |body: &str| {
        body.strip_prefix(&candidate.pr_url)
            .is_some_and(|rest| rest.starts_with(' ') || rest.starts_with(':'))
    };
    if !matches!(probe_result.state, PrLifecycleState::ClosedUnmerged)
        && !base_attentions.is_empty()
        && base_attentions.iter().all(|item| belongs_to_probe(&item.body_markdown))
        && let Err(err) = work_db.resolve_external_tracker_attention(&candidate.work_item_id, PR_BASE_REFUSED)
    {
        tracing::warn!(?err, "merge poller: could not clear PR base refusal");
    }
    // Populated in the Open arm so the post-match poll-state write can
    // reuse the reaper's active-attempt snapshot instead of re-querying.
    // `None` outside Open (or if the reaper was not run); `Some(…)` is the
    // still-active merge-queue remediation after the reaper (inner `None`
    // when none remains).
    let mut known_active_queue_failure: Option<Option<CiRemediation>> = None;
    // Read before any transition: a merge moves the row to `done`, after which
    // it no longer reads as parked. The batched set is a fast skip for rows
    // already parked at load; a row parked since then is confirmed on the
    // mint/resume paths below.
    let parked = match parked_ids {
        Some(ids) => ids.contains(&candidate.work_item_id),
        None => work_db.is_parked_or_assume_not(&candidate.work_item_id, "merge poller sweep"),
    };
    let parked_for_action = || parked || work_db.is_parked_or_assume_not(&candidate.work_item_id, "merge poller sweep");
    match &probe_result.state {
        PrLifecycleState::Merged => {
            let _ = work_db.clear_ci_merge_rebase(&candidate.work_item_id);
            match work_db.retire_github_merge_intent(&candidate.work_item_id, &candidate.pr_url, "merged") {
                Ok(true) => tracing::info!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    "merge poller: retired GitHub merge intent after observed merge",
                ),
                Ok(false) => {}
                Err(err) => tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    ?err,
                    "merge poller: failed to retire GitHub merge intent after observed merge",
                ),
            }
            end_conflict_deferral(work_db, candidate, crate::work::ConflictDeferralEnd::PrMerged);
            if mark_merged(work_db, publisher, host, candidate, &probe_result).await {
                outcome.merged += 1;
                // Clean up any pending/running ci_remediations rows and emit
                // CiFailureCleared so the macOS kanban clears the "ci failing"
                // badge. Without this, a task that was blocked on CI when its
                // PR merged leaves a pending row that causes the badge to
                // reappear on every app restart / list-refresh.
                ci_watch::on_pr_merged(work_db, publisher, candidate).await;
            }
            // Invalidate any in-flight revision executions whose parent
            // just merged.  `block_pending_revisions_on_parent_close`
            // already ran inside `mark_chore_pr_merged`'s transaction;
            // here we force-release their cube leases and mark them
            // terminal so the scheduler doesn't try to redispatch.
            stop_active_revision_executions(work_db, publisher, completion_handler, &candidate.work_item_id, outcome)
                .await;
            stop_live_review_guides(
                work_db,
                completion_handler,
                &candidate.work_item_id,
                &candidate.pr_url,
                "parent PR merged",
            )
            .await;
            // Deep-production eligibility + merge-SHA batch creation for the
            // landed-code post-merge review. Independent of `mark_merged`'s
            // own bookkeeping outcome (a human-driven row that stays in
            // Review still merged its PR) and of `stored_pr_number` above —
            // this is a fixed-cost DB check plus, at most, one cheap insert
            // and a scheduler kick, never a worker spawn, so it cannot make
            // the merge poller's sweep pass block on the reviewer actually
            // running.
            //
            // Runs for a parked row too: the merge is a terminal fact, the row
            // is `done` now and no later path would trigger this review, so
            // skipping it would drop the review permanently.
            let cycle_root_id = work_db.review_cycle_root_id(&candidate.work_item_id);
            let slim = host.is_some_and(|host| host.slim_post_merge_review(work_db, &cycle_root_id));
            maybe_trigger_post_merge_review(work_db, publisher, candidate, &probe_result, slim).await;
        }
        // Only parked rows probed in full reach this arm (blocked, or with a
        // merge in flight). Parked `in_review` rows without a merge in flight
        // are shallow and stay out of the walk: they are watched for merge or
        // close, and their poll-state badges are no longer refreshed. Open-PR
        // upkeep still does not run while parked.
        PrLifecycleState::Open(_) if parked => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "merge poller: row is parked; skipping open-PR upkeep",
            );
            update_pr_poll_state(work_db, publisher, candidate, &probe_result, None, caller).await;
            return;
        }
        PrLifecycleState::Open(open) => {
            if let (Some(handler), Some(base_sha), Some(head_sha), Some(observation_sequence)) = (
                completion_handler,
                probe_result.base_ref_oid.clone(),
                probe_result.head_ref_oid.clone(),
                source_observation_sequences.get(&candidate.pr_url).copied(),
            ) {
                handler.reconcile_review_guide_source_from_probe(
                    &candidate.work_item_id,
                    &candidate.pr_url,
                    &base_sha,
                    &head_sha,
                    observation_sequence,
                );
            }
            known_active_queue_failure =
                Some(reap_superseded_merge_queue_attempt(work_db, publisher, candidate, &probe_result).await);
            // Design §Q1: conflict pre-empts CI — the conflict-resolver
            // owns the slot first, and CI will be re-evaluated against
            // the new base once the rebase pushes. Both clean drives
            // every retire path (conflict + CI), each gated by its own
            // WHERE guard so an irrelevant retire is a cheap no-op.
            let mergeability = open.mergeability;
            let ci = &open.ci;
            match mergeability {
                OpenPrMergeability::Conflict => {
                    // Phase 3 cutover: the conflict producer creates an
                    // engine-triggered revision via the shared
                    // `create_revision` gate (R4 reuse). We are inside the
                    // `Open` arm with `mergeability = Conflict`, so the PR is
                    // known-open; feed that observation to the gate via a
                    // static checker rather than a redundant `gh pr view`.
                    //
                    // Escalation ladder: enabled while the
                    // `conflict_ladder_mechanical_rebase` flag is on (its
                    // default). Set to false (or no completion handler) →
                    // `Disabled` takes the worker-only path.
                    //
                    // When it IS on, the ladder must never be awaited here.
                    // This function is the detection path: `run_one_pass`
                    // awaits it one candidate at a time on the poller's single
                    // task, so a rung-1 rebase awaited inline (cube lease +
                    // real rebase + push-gate, observed at 1.5–8.75 minutes
                    // per PR) stops every other PR's merge/conflict state from
                    // being observed for that entire window — the 32-minute
                    // detection blackout `e64dda13b` (#1968) introduced.
                    // `Deferred` hands the ladder to the bounded background
                    // remediator and returns immediately.
                    //
                    // Which remediator runs (the host's bounded background
                    // queue, an inline ladder, or none) is the host's call;
                    // the sweep supplies only the flag and the cube client.
                    let ladder_enabled = completion_handler
                        .map(|h| h.mechanical_rebase_enabled())
                        .unwrap_or(false);
                    if parked_for_action() {
                        tracing::debug!(
                            work_item_id = %candidate.work_item_id,
                            pr_url = %candidate.pr_url,
                            "merge poller: row is parked; skipping conflict remediation",
                        );
                    } else if let Some(host) = host
                        && host
                            .on_conflict_detected(
                                work_db,
                                publisher,
                                ConflictResponse {
                                    ladder_enabled,
                                    cube_client,
                                },
                                &crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open),
                                candidate,
                                &probe_result,
                            )
                            .await
                    {
                        outcome.conflict_flagged += 1;
                    }
                }
                OpenPrMergeability::Unknown => {
                    // GitHub's `mergeable` field is `UNKNOWN` — the mergeability
                    // check is still being computed asynchronously (typically right
                    // after a base-branch move or a race with the recompute cycle).
                    // Treat as INDETERMINATE: skip conflict detection AND the
                    // merge_conflict retire path so we don't emit phantom
                    // blocked→in_review transitions. CI signals are on a separate
                    // axis and are still processed normally.
                    tracing::debug!(
                        work_item_id = %candidate.work_item_id,
                        pr_url = %candidate.pr_url,
                        "merge poller: mergeable=UNKNOWN (GitHub recomputing); \
                         skipping conflict-watch transitions — retaining prior state \
                         until next sweep returns a definitive MERGEABLE or CONFLICTING",
                    );
                    maybe_clear_blocked(
                        work_db,
                        publisher,
                        SweepHandlers {
                            cube_client,
                            completion_handler,
                            host,
                            parked_ids,
                        },
                        candidate,
                        &probe_result,
                        (ci, false), // mergeability_clean=false: skip merge_conflict retire on UNKNOWN
                        outcome,
                    )
                    .await;
                    dispatch_ci_axis(work_db, publisher, candidate, &probe_result, ci, outcome).await;
                }
                OpenPrMergeability::Clean => {
                    // Polymorphic clear dispatch (design §Q5 Phase 10 #31):
                    // walk the `task_blocked_signals` side table and ask
                    // each active reason's retire path to act if its
                    // probe condition holds. Each per-reason handler is
                    // still idempotent on its own (WHERE-guarded), so
                    // this is purely a refactor of the dispatch from
                    // "call every retire path unconditionally" to "call
                    // only the retire paths whose signals are still
                    // observed as active." The detect side stays where
                    // it is — detection is signal-specific (a `Failing`
                    // CI status can't fire the conflict watcher) and
                    // doesn't need the side-table read.
                    maybe_clear_blocked(
                        work_db,
                        publisher,
                        SweepHandlers {
                            cube_client,
                            completion_handler,
                            host,
                            parked_ids,
                        },
                        candidate,
                        &probe_result,
                        (ci, true), // mergeability_clean=true: merge_conflict retire is safe
                        outcome,
                    )
                    .await;
                    // CI-side detect: a `Failing` rollup still needs
                    // its own fan-out regardless of what the side-table
                    // says, because the chore is currently `in_review`
                    // (no signal in the table yet) on the first failure.
                    // `InFlight` supersedes-failure and in-flight tracking
                    // are also independent of mergeability.
                    dispatch_ci_axis(work_db, publisher, candidate, &probe_result, ci, outcome).await;
                }
            }
        }
        PrLifecycleState::ClosedUnmerged => {
            let _ = work_db.clear_ci_merge_rebase(&candidate.work_item_id);
            match work_db.retire_github_merge_intent(&candidate.work_item_id, &candidate.pr_url, "closed") {
                Ok(true) => tracing::info!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    "merge poller: retired GitHub merge intent after observed unmerged close",
                ),
                Ok(false) => {}
                Err(err) => tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    ?err,
                    "merge poller: failed to retire GitHub merge intent after observed unmerged close",
                ),
            }
            end_conflict_deferral(work_db, candidate, crate::work::ConflictDeferralEnd::PrClosed);
            // The sweep-wide clearer skips ClosedUnmerged so a stacked close
            // can retire. Resolve a refusal that belongs to this PR: the
            // row is done and the PR never landed.
            if !base_attentions.is_empty()
                && base_attentions.iter().all(|item| belongs_to_probe(&item.body_markdown))
                && let Err(err) = work_db.resolve_external_tracker_attention(&candidate.work_item_id, PR_BASE_REFUSED)
            {
                tracing::warn!(?err, "merge poller: could not clear PR base refusal");
            }
            // Comment reconciliation is a narrower, comment-only signal
            // (comment-intent-classification design §"Reconciliation" /
            // §Risks — the "reopen on abandon" half of task 2c): a comment
            // whose `[Revise]` batch never shipped because this PR closed
            // unmerged should not sit at `in_revision` forever, so reopen it
            // onto the `[Revise]` banner. Runs before the status retire
            // below so the reopen always observes the pre-retire comment
            // state, independent of whether the retire itself fires.
            match work_db.reopen_comments_for_closed_unmerged_pr(&candidate.work_item_id) {
                Ok(reopened) if reopened > 0 => {
                    outcome.comments_reopened += reopened;
                    publisher
                        .publish_work_item_changed(
                            &candidate.product_id,
                            &candidate.work_item_id,
                            "comments_reopened_on_pr_closed_unmerged",
                        )
                        .await;
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(
                        work_item_id = %candidate.work_item_id,
                        pr_url = %candidate.pr_url,
                        ?err,
                        "merge poller: failed to reopen comments for closed-unmerged PR",
                    );
                }
            }
            // `chore-lifecycle-pr-closed-unmerged.md`: PR closure without a
            // merge is a definitive human signal that this attempt is over.
            // Retire the row to `done` (the parallel path to `mark_merged`
            // above) — no redo execution is spawned; a human moves the row
            // back to `todo`/`active` manually if the work should be redone.
            if mark_closed_unmerged(work_db, publisher, candidate).await {
                outcome.closed_unmerged += 1;
            }
            stop_live_review_guides(
                work_db,
                completion_handler,
                &candidate.work_item_id,
                &candidate.pr_url,
                "PR closed unmerged",
            )
            .await;
        }
    }
    // For every open (or just-probed) PR, persist the CI + review poll
    // state so the macOS kanban can render indicators with tooltips.
    // We do this unconditionally after the lifecycle routing above so
    // the columns stay fresh even when no status transition fired.
    // Merged / closed-unmerged probes are skipped — the row will
    // transition away from `in_review` and the indicators become moot.
    if let PrLifecycleState::Open(open) = &probe_result.state {
        retire_moot_revisions(work_db, publisher, host, candidate, &probe_result, open).await;
        update_pr_poll_state(
            work_db,
            publisher,
            candidate,
            &probe_result,
            known_active_queue_failure.as_ref().map(|inner| inner.as_ref()),
            caller,
        )
        .await;
        if open.mergeability == OpenPrMergeability::Clean
            && let Some(host) = host
            && !parked
        {
            host.resume_merge_after_conflict(work_db, publisher, candidate).await;
        }
        // Trunk merge-queue episodes Boss did not initiate. Runs for every
        // open PR regardless of which mergeability arm above fired: a
        // merge-side eviction lands on a CONFLICTING PR and a test-failure
        // eviction on a clean one, and both need the episode enumerated
        // before the Trunk lane can tell them apart. Costs one `Option`
        // test on any PR without a failed Trunk check, which is all of them
        // outside a Trunk repo.
        if crate::trunk_queue_adopt::adopt_unattributed_trunk_queue_episode(work_db, candidate, &probe_result) {
            outcome.trunk_episodes_adopted += 1;
        }
        // After adopt so a freshly-inserted intent owns the episode and
        // this path stands down. Direct-mechanism products never adopt;
        // the failed Trunk check on the probe is then the only signal,
        // and we mint from it here. Conflict and in-flight CI are
        // declined inside the callee (waiting vs stuck).
        if ci_watch::on_trunk_head_check_eviction_detected(
            work_db,
            crate::queue_ci_contract::DetectionContext {
                publisher,
                fetcher: &crate::queue_ci_contract::ProviderFetcher,
            },
            candidate,
            &probe_result,
        )
        .await
        {
            outcome.ci_flagged += 1;
        }
    }
}

/// Reap an active GitHub merge-queue remediation once the PR head advances
/// beyond the head captured by the ordered dequeue timeline. This is
/// deliberately independent of the current CI rollup: PR-head checks are a
/// different surface and are normally green throughout a queue-side failure.
///
/// Do not compare the synthetic queue commit to the PR head through Git
/// ancestry. GitHub's squash-mode queue commit has only the base commit as
/// its parent, so the compare API reports `diverged` even when that exact PR
/// head produced the failing merged result.
///
/// Returns the still-active merge-queue remediation after the reaper runs
/// (or `None` when none was active / the reaper retired it), so
/// [`update_pr_poll_state`] can reuse the row without a second sqlite
/// connection on the same sweep pass.
async fn reap_superseded_merge_queue_attempt(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    probe_result: &PrLifecycleProbe,
) -> Option<CiRemediation> {
    let active = match work_db.active_ci_remediation_for_work_item(&candidate.work_item_id) {
        Ok(Some(active)) if active.failure_kind.as_deref() == Some("merge_queue_rebounce") => active,
        Ok(_) => return None,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "merge poller: failed to inspect active merge-queue remediation",
            );
            return None;
        }
    };
    let Some(current_head_sha) = probe_result.head_ref_oid.as_deref() else {
        ci_watch::record_declined_evaluation(candidate, &active, None, "merge_queue_reaper_missing_current_head");
        return Some(active);
    };
    // Rows created before the queue detector began storing the PR head used
    // the synthetic commit as both fields. There is no reliable way to
    // reconstruct the tested PR head from such a squash commit. Fail closed
    // while a linked revision is still live (do not re-mint mid-fix). A
    // concluded linked revision is the only safe escape: a never-linked or
    // unreadable row remains pinned and is retried on the next sweep.
    if active.before_commit_sha.as_deref() == Some(active.head_sha_at_trigger.as_str()) {
        if legacy_merge_queue_attempt_may_retire(work_db, &active) {
            let retired = ci_watch::retire_superseded_merge_queue_attempt(
                work_db,
                publisher,
                candidate,
                &active,
                ci_watch::MergeQueueAttemptRetirementReason::LegacyTriggerRevisionConcluded,
            )
            .await;
            return if retired { None } else { Some(active) };
        }
        ci_watch::record_declined_evaluation(
            candidate,
            &active,
            Some(current_head_sha),
            "merge_queue_reaper_legacy_trigger_has_no_pr_head",
        );
        return Some(active);
    }
    if crate::work::merge_queue_rebounce_pr_head(&active.head_sha_at_trigger) == current_head_sha {
        ci_watch::record_declined_evaluation(
            candidate,
            &active,
            Some(current_head_sha),
            "merge_queue_trigger_head_unchanged",
        );
        return Some(active);
    }
    let retired = ci_watch::retire_superseded_merge_queue_attempt(
        work_db,
        publisher,
        candidate,
        &active,
        ci_watch::MergeQueueAttemptRetirementReason::PrHeadAdvanced { current_head_sha },
    )
    .await;
    if retired { None } else { Some(active) }
}

/// Whether a pre-head-key merge-queue remediation may leave the fail-closed
/// legacy branch once its linked fix revision concludes. Keeps a missing,
/// unreadable, or still-live revision fail-closed so a legacy queue failure
/// cannot be cleared without evidence that its remediation ran.
fn legacy_merge_queue_attempt_may_retire(work_db: &WorkDb, active: &CiRemediation) -> bool {
    let Some(revision_id) = active.revision_task_id.as_deref() else {
        return false;
    };
    match work_db.get_work_item(revision_id) {
        Ok(WorkItem::Task(task) | WorkItem::Chore(task)) => matches!(
            task.status,
            crate::work::TaskStatus::InReview | crate::work::TaskStatus::Done | crate::work::TaskStatus::Archived
        ),
        Ok(_) => false,
        Err(err) => {
            tracing::warn!(
                revision_task_id = revision_id,
                ?err,
                "merge poller: failed to read legacy merge-queue fix revision; retaining attempt",
            );
            false
        }
    }
}

/// Reconcile a single stranded `blocked` parent (NULL scalar
/// `blocked_reason`, empty active-signal set, remediation-owned, bound PR)
/// surfaced by [`WorkDb::list_chores_stranded_blocked_remediation`].
///
/// The invariant this restores: a parent must never rest `blocked` with an
/// empty signal set while its PR is still dirty/red. Such a parent is
/// invisible to every scalar-reason-keyed candidate list, so we re-probe it
/// here, and when the PR is still dirty/red we re-canonicalise it back into
/// the standard `blocked: merge_conflict` / `blocked: ci_failure` state
/// (re-arming the side-table signal) and immediately drive the normal
/// detection path so a fresh remediation revision (re)spawns this sweep —
/// even when a previous revision for an earlier conflict already sits in
/// `review`/`done`. A clean+green PR is left untouched: it is not the
/// dirty/red invariant violation, and the row's block is owned by whatever
/// non-remediation flow parked it.
///
/// Rows still gated by an unsatisfied prerequisite are skipped — those are
/// owned by the dependency-unblock sweep, not the remediation flow, and
/// re-canonicalising one could lose its dependency block when the conflict
/// later resolves.
pub(crate) async fn sweep_stranded_blocked_remediation(
    work_db: &WorkDb,
    probe_results: &HashMap<String, std::result::Result<PrLifecycleProbe, String>>,
    publisher: &dyn ExecutionPublisher,
    host: Option<&dyn SweepHost>,
    candidate: &PendingMergeCheck,
    outcome: &mut SweepOutcome,
) {
    match work_db.gating_prereqs_for(&candidate.work_item_id) {
        Ok(gating) if !gating.is_empty() => {
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                gating = gating.len(),
                "merge poller: stranded-blocked parent is dependency-gated; leaving for dep sweep",
            );
            return;
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "merge poller: failed to read gating prereqs for stranded-blocked parent; skipping",
            );
            return;
        }
    }
    let probe_result = match probe_results.get(&candidate.pr_url) {
        Some(Ok(state)) => state.clone(),
        Some(Err(err)) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                err,
                "merge poller: stranded-blocked probe failed; will retry next pass",
            );
            return;
        }
        None => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "merge poller: no batched probe result for this stranded-blocked PR; will retry next pass",
            );
            return;
        }
    };
    let PrLifecycleState::Open(open) = &probe_result.state else {
        // Merged / closed-unmerged: not a dirty-PR strand. Leave the row
        // for the lifecycle paths that own those transitions.
        return;
    };
    // Design §Q1: conflict pre-empts CI. A conflicting PR re-canonicalises
    // to merge_conflict; a clean-but-failing PR re-canonicalises to
    // ci_failure. A clean+green PR is not a dirty/red violation — leave it.
    match open.mergeability {
        OpenPrMergeability::Conflict => {
            reconcile_stranded(
                work_db,
                publisher,
                host,
                candidate,
                &probe_result,
                SignalKind::MergeConflict,
                outcome,
            )
            .await;
        }
        OpenPrMergeability::Unknown => {
            // GitHub is mid-recompute — skip re-canonicalization. The
            // stranded-blocked reconciler fires on the next sweep once
            // GitHub returns a definitive CONFLICTING or MERGEABLE result.
            tracing::debug!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                "merge poller: stranded-blocked probe returned mergeable=UNKNOWN; \
                 deferring re-canonicalization to next sweep",
            );
        }
        OpenPrMergeability::Clean => {
            if matches!(open.ci, OpenPrCiStatus::Failing { .. }) {
                reconcile_stranded(
                    work_db,
                    publisher,
                    host,
                    candidate,
                    &probe_result,
                    SignalKind::CiFailure,
                    outcome,
                )
                .await;
            }
        }
    }
}

/// A PR that merged or closed is no longer "conflicted, waiting for a merge
/// request": close the ledger record of a conflict that was being held back,
/// so a `deferred` row always describes an open PR.
fn end_conflict_deferral(work_db: &WorkDb, candidate: &PendingMergeCheck, end: crate::work::ConflictDeferralEnd) {
    if let Err(err) = work_db.end_conflict_deferral(&candidate.work_item_id, end) {
        tracing::warn!(
            work_item_id = %candidate.work_item_id,
            ?err,
            "merge poller: failed to end the conflict deferral of a finished PR",
        );
    }
}

/// Shared body of [`sweep_stranded_blocked_remediation`]: re-canonicalise
/// the NULL-reason blocked parent into `kind`'s canonical blocked state and
/// drive the matching detection path so a revision (re)spawns.
pub(crate) async fn reconcile_stranded(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    host: Option<&dyn SweepHost>,
    candidate: &PendingMergeCheck,
    probe_result: &PrLifecycleProbe,
    kind: SignalKind,
    outcome: &mut SweepOutcome,
) {
    let recanonicalized = match kind {
        SignalKind::MergeConflict => {
            work_db.recanonicalize_blocked_merge_conflict(&candidate.work_item_id, &candidate.pr_url)
        }
        SignalKind::CiFailure => work_db.recanonicalize_blocked_ci_failure(&candidate.work_item_id, &candidate.pr_url),
    };
    match recanonicalized {
        Ok(Some(_)) => {}
        Ok(None) => {
            // WHERE-guard miss: the row was moved or re-claimed by another
            // path between listing and now. Nothing to do.
            return;
        }
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                reason = kind.reason(),
                ?err,
                "merge poller: failed to re-canonicalise stranded-blocked parent",
            );
            return;
        }
    }
    outcome.stranded_blocked_recanonicalized += 1;
    tracing::info!(
        work_item_id = %candidate.work_item_id,
        pr_url = %candidate.pr_url,
        reason = kind.reason(),
        "merge poller: re-canonicalised stranded blocked parent; driving detection to (re)spawn revision",
    );
    publisher
        .publish_work_item_changed(
            &candidate.product_id,
            &candidate.work_item_id,
            "stranded_remediation_recovered",
        )
        .await;
    // Drive the standard detection so a fresh revision spawns this sweep.
    // The parent is now `blocked: <reason>`, so detection takes the re-arm
    // path; the PR is known-open, so feed that to the create gate via a
    // static checker rather than a redundant `gh pr view`.
    let checker = crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open);
    match kind {
        SignalKind::MergeConflict => {
            // Recovery re-arm path (not a fresh conflict): drive detection to
            // re-spawn the worker revision. The mechanical rung is only for
            // fresh conflicts, so pass `None` — no engine-direct rebase here.
            if let Some(host) = host
                && host
                    .on_conflict_detected(
                        work_db,
                        publisher,
                        ConflictResponse::WORKER_ONLY,
                        &checker,
                        candidate,
                        probe_result,
                    )
                    .await
            {
                outcome.conflict_flagged += 1;
            }
        }
        SignalKind::CiFailure => {
            let failures = match &probe_result.state {
                PrLifecycleState::Open(open) => match &open.ci {
                    OpenPrCiStatus::Failing { failures } => failures.clone(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            if ci_watch::on_ci_failure_detected(work_db, publisher, &checker, candidate, probe_result, &failures).await
            {
                outcome.ci_flagged += 1;
            }
        }
    }
}

/// Dispatch CI signals (failure detection and in-flight tracking) independent
/// of mergeability. Called from both the `Unknown` and `Clean` mergeability
/// arms in `sweep_one` — CI is an orthogonal axis and is handled identically
/// regardless of whether mergeability is known.
pub(crate) async fn dispatch_ci_axis(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    probe_result: &PrLifecycleProbe,
    ci: &OpenPrCiStatus,
    outcome: &mut SweepOutcome,
) {
    if let OpenPrCiStatus::Failing { failures } = ci
        && ci_watch::on_ci_failure_detected(
            work_db,
            publisher,
            &crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open),
            candidate,
            probe_result,
            failures,
        )
        .await
    {
        outcome.ci_flagged += 1;
    }
    if matches!(ci, OpenPrCiStatus::Clean)
        && let Ok(Some(rebase)) = work_db.get_ci_merge_rebase(&candidate.work_item_id, &candidate.pr_url)
        && let Some(head_sha) = probe_result.head_ref_oid.as_deref()
        && (rebase.head_sha_before.as_deref() != Some(head_sha) || rebase.rebased_head_sha.as_deref() == Some(head_sha))
    {
        let _ = work_db.clear_ci_merge_rebase(&candidate.work_item_id);
    }
    if matches!(ci, OpenPrCiStatus::InFlight) {
        if ci_watch::on_ci_in_flight_supersedes_failure(
            work_db,
            publisher,
            candidate,
            &probe_result.labels,
            probe_result.head_ref_oid.as_deref(),
        )
        .await
        {
            outcome.ci_cleared += 1;
        }
        ci_watch::on_ci_in_flight(work_db, publisher, candidate, probe_result).await;
    }
}

/// Close engine-created conflict / CI-fix revisions made moot by the probe.
///
/// `on_resolved` / `on_ci_resolved` only run while the parent still carries a
/// blocking signal and only see the *active* attempt, so a revision parked
/// after its attempt ended (e.g. `retriggered`, `failed`) keeps the parent in
/// Review > Blocked on a PR that has since recovered. Runs once per open
/// probe, outside the mergeability routing, so a PR that is both conflicting
/// and CI-green still has its stale CI-fix revision closed. The conflict-side
/// sweep needs a definitive `Clean` mergeability; `Unknown` and `Conflict`
/// leave conflict revisions alone. Publishes a work-item invalidation when
/// anything was archived, since neither retire path notifies the board.
async fn retire_moot_revisions(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    host: Option<&dyn SweepHost>,
    candidate: &PendingMergeCheck,
    probe_result: &PrLifecycleProbe,
    open: &OpenPrStatus,
) {
    let mut closed = 0;
    if open.mergeability == OpenPrMergeability::Clean
        && let Some(host) = host
    {
        closed += host.retire_moot_conflict_revisions(work_db, candidate, &probe_result.labels);
    }
    if matches!(open.ci, OpenPrCiStatus::Clean) {
        closed += ci_watch::retire_moot_ci_fix_revisions(work_db, candidate, &probe_result.labels);
    }
    if closed > 0 {
        publisher
            .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "moot_revisions_closed")
            .await;
    }
}

/// Polymorphic retire dispatch (design §Q5 / Phase 10 #31).
///
/// The merge poller's `Clean`-mergeability branch used to call every
/// per-signal retire path unconditionally (conflict-watch on_resolved
/// and ci-watch on_ci_resolved, in sequence). That worked because each
/// retire path was already WHERE-guarded against its own row state, so
/// running it against a chore that wasn't blocked on that reason was a
/// cheap no-op.
///
/// With the `task_blocked_signals` side table in place, we can do
/// better: read the active signal set first, and dispatch only to the
/// retire paths whose signals are still observed. Same end state, but:
///
///   - the dispatch is now self-documenting — adding a new
///     `blocked_reason` (review_feedback, dependency, …) becomes a
///     single match arm here rather than a new unconditional `await`
///     bolted onto the sweep;
///   - failure to add a per-reason `should_clear` arm becomes loud
///     (`_ => false` falls through with a warn), instead of silently
///     never clearing the signal;
///   - the per-signal probe condition is centralised, so the
///     `merge_conflict ⇒ Clean mergeability` and `ci_failure ⇒ Clean
///     ci` couplings live in one place that the design's snippet maps
///     to directly.
///
/// A read of the side table when there are no active signals is one
/// `SELECT … WHERE cleared_at IS NULL` returning zero rows; cheaper
/// than the unconditional UPDATEs the old dispatch always sent.
/// `mergeability_clean` mirrors the `ci_clean` gate for the CI arm: when
/// `false` (i.e. `mergeable=UNKNOWN`), the `merge_conflict` retire path
/// is skipped — GitHub is still computing mergeability, so we must not
/// act on the absence of a CONFLICTING signal as evidence that the PR is
/// now clean.
pub async fn maybe_clear_blocked(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    handlers: SweepHandlers<'_>,
    candidate: &PendingMergeCheck,
    // Supplies `labels`, `raw_mergeable`, `raw_merge_state_status`.
    probe_result: &PrLifecycleProbe,
    // (ci, mergeability_clean) — bundled to keep the parameter count
    // under clippy::too_many_arguments.
    ci_status: (&OpenPrCiStatus, bool),
    outcome: &mut SweepOutcome,
) {
    let SweepHandlers {
        cube_client,
        completion_handler,
        host,
        parked_ids: _,
    } = handlers;
    let labels = &probe_result.labels;
    let raw_mergeable = probe_result.raw_mergeable.as_str();
    let raw_merge_state_status = probe_result.raw_merge_state_status.as_str();
    let (ci, mergeability_clean) = ci_status;
    let signals = match work_db.active_blocked_signals(&candidate.work_item_id) {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "merge poller: failed to read active blocked signals; skipping clear dispatch",
            );
            return;
        }
    };
    // Drift guard: if `task_blocked_signals` is empty but the task
    // still has a non-null `blocked_reason`, the signals table and the
    // scalar got out of sync (e.g. the polymorphic-clear path cleared the
    // signal row before the parent task was cleared). Fall back to the
    // `blocked_reason` scalar so the retire path can still fire on a Clean
    // probe, preventing the task from being stuck blocked indefinitely.
    let signals = if signals.is_empty() {
        match work_db.task_blocked_reason(&candidate.work_item_id) {
            Ok(Some(reason)) => {
                tracing::debug!(
                    work_item_id = %candidate.work_item_id,
                    %reason,
                    "merge poller: task_blocked_signals empty but blocked_reason set; using blocked_reason as fallback",
                );
                vec![boss_protocol::BlockedSignal {
                    work_item_id: candidate.work_item_id.clone(),
                    reason,
                    attempt_id: None,
                    created_at: String::new(),
                    cleared_at: None,
                }]
            }
            Ok(None) => return, // task not blocked or no reason — nothing to do
            Err(err) => {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    ?err,
                    "merge poller: failed to read blocked_reason for drift fallback; skipping clear dispatch",
                );
                return;
            }
        }
    } else {
        signals
    };

    // merge_conflict probe condition: mergeability must be definitive `Clean`.
    // `Unknown` (GitHub recomputing) is skipped — see `mergeability_clean` param.
    // CI's probe condition is `OpenPrCiStatus::Clean`; `InFlight` and `Failing`
    // decline to retire.
    let ci_clean = matches!(ci, OpenPrCiStatus::Clean);

    for signal in signals {
        match signal.reason.as_str() {
            "merge_conflict" => {
                if !mergeability_clean {
                    // GitHub returned `mergeable=UNKNOWN` — mergeability is still
                    // being computed asynchronously. Do not fire the conflict-watch
                    // retire path: treating UNKNOWN as clean caused phantom
                    // blocked→in_review transitions (the conflict_watch flap).
                    // Re-poll next sweep for a definitive MERGEABLE or CONFLICTING.
                    tracing::debug!(
                        work_item_id = %candidate.work_item_id,
                        pr_url = %candidate.pr_url,
                        "merge poller: deferring merge_conflict retire — \
                         mergeable=UNKNOWN (GitHub recomputing); re-polling next sweep",
                    );
                    continue;
                }
                tracing::debug!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    "merge poller: dispatching conflict_watch::on_resolved \
                     (mergeable=MERGEABLE/definitive Clean)",
                );
                if let Some(host) = host
                    && host
                        .on_conflict_resolved(
                            work_db,
                            publisher,
                            cube_client,
                            candidate,
                            ConflictCleared {
                                labels,
                                raw_mergeable,
                                raw_merge_state_status,
                            },
                        )
                        .await
                {
                    outcome.conflict_cleared += 1;
                }
            }
            // `ci_flaky_retriggered` belongs here, not in the `other =>`
            // debug arm: it is stamped by `mark_ci_remediation_retriggered`
            // on the SAME axis as `ci_failure` (the worker re-ran a failing
            // build rather than pushing), and `ci_watch::on_ci_resolved` is
            // its retire path too. It usually rides alongside a `ci_failure`
            // row, which is the only reason the omission was survivable —
            // but when it is the only active signal, falling to `other`
            // meant nothing ever dispatched and the flake tag stuck to the
            // card after CI had gone green.
            "ci_failure" | "ci_failure_exhausted" | "ci_flaky_retriggered" => {
                if !ci_clean {
                    continue;
                }
                if ci_watch::on_ci_resolved(work_db, publisher, candidate, labels).await {
                    outcome.ci_cleared += 1;
                    // The task just auto-transitioned back to `in_review`
                    // because its PR's CI went green. Stop the worker that
                    // was running it — it has nothing useful left to do (it
                    // is typically still polling CI for the very fix the
                    // engine already observed) and otherwise holds its slot
                    // indefinitely (issue #898).
                    stop_worker_on_review_transition(work_db, completion_handler, &candidate.work_item_id, outcome)
                        .await;
                }
            }
            other => {
                // Unknown / future blocked_reason values
                // (`review_feedback`, `dependency`, …) — those flows
                // own their own retire paths. We log once at debug so
                // an unwired reason doesn't silently leak past the
                // sweep, but don't treat the situation as an error.
                tracing::debug!(
                    work_item_id = %candidate.work_item_id,
                    reason = other,
                    "merge poller: no retire-path arm for blocked_reason; leaving for owning flow",
                );
            }
        }
    }
}

/// Derive the `ci_required_state` string from a probe's CI status.
pub(crate) fn ci_state_str(ci: &OpenPrCiStatus) -> &'static str {
    match ci {
        OpenPrCiStatus::Clean => "success",
        OpenPrCiStatus::InFlight => "in_progress",
        OpenPrCiStatus::Failing { .. } => "fail",
    }
}

/// Derive the `pr_mergeable_state` string from a probe's raw GitHub
/// mergeability (mono#2303). This is the *only* signal that reflects
/// whether the PR's head actually merges cleanly into its base — CI status
/// and merge-queue/auto-merge arming are both silent on that question, so a
/// client must not infer mergeability from either of them.
pub fn mergeable_state_str(mergeability: OpenPrMergeability) -> &'static str {
    match mergeability {
        OpenPrMergeability::Clean => "mergeable",
        OpenPrMergeability::Conflict => "conflicting",
        OpenPrMergeability::Unknown => "unknown",
    }
}

/// Build a compact JSON detail blob for failing CI checks (list of
/// `{"name": "...", "conclusion": "..."}` objects). Returns `None`
/// when the check list is empty so we don't write `"[]"` to the DB.
pub(crate) fn ci_detail_json(ci: &OpenPrCiStatus) -> Option<String> {
    let OpenPrCiStatus::Failing { failures } = ci else {
        return None;
    };
    if failures.is_empty() {
        return None;
    }
    let items: Vec<serde_json::Value> = failures
        .iter()
        .map(|f| {
            serde_json::json!({
                "name": f.name,
                "conclusion": f.conclusion,
            })
        })
        .collect();
    serde_json::to_string(&items).ok()
}

/// Build a compact JSON detail blob for reviewer logins. Returns `None`
/// when the list is empty.
pub(crate) fn review_detail_json(reviewers: &[String]) -> Option<String> {
    if reviewers.is_empty() {
        return None;
    }
    serde_json::to_string(reviewers).ok()
}

/// Adapter that routes completion's on-transition CI pre-fetch into
/// [`update_pr_poll_state`]. Holds the publisher so observation contracts
/// do not depend on execution-contracts.
pub struct MergePollerPrPollStateWriter {
    publisher: Arc<dyn ExecutionPublisher>,
}

impl MergePollerPrPollStateWriter {
    pub fn new(publisher: Arc<dyn ExecutionPublisher>) -> Self {
        Self { publisher }
    }
}

#[async_trait]
impl PrPollStateWriter for MergePollerPrPollStateWriter {
    async fn update_pr_poll_state(
        &self,
        work_db: &WorkDb,
        candidate: &PendingMergeCheck,
        probe: &PrLifecycleProbe,
        caller: &str,
    ) {
        update_pr_poll_state(work_db, self.publisher.as_ref(), candidate, probe, None, caller).await;
    }
}

/// Persist CI + review + merge-queue poll state and emit a change event
/// when any field flips value. Called from `sweep_one` for every open PR and
/// through [`MergePollerPrPollStateWriter`] after completion's initial CI fetch.
///
/// `known_active_queue_failure` lets the sweep reaper hand off its already-
/// loaded merge-queue remediation (or `Some(None)` when it just retired one)
/// so this function does not open a second sqlite connection for the same
/// work item on the same pass. Pass `None` when the caller did not run the
/// reaper — this function self-fetches in that case (e.g. the completion adapter).
pub async fn update_pr_poll_state(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
    known_active_queue_failure: Option<Option<&CiRemediation>>,
    caller: &str,
) {
    let PrLifecycleState::Open(open) = &probe.state else {
        return;
    };

    let fetched_active;
    let active_queue_failure = match known_active_queue_failure {
        Some(known) => known.filter(|a| a.failure_kind.as_deref() == Some("merge_queue_rebounce")),
        None => {
            fetched_active = match work_db.active_ci_remediation_for_work_item(&candidate.work_item_id) {
                Ok(Some(attempt)) if attempt.failure_kind.as_deref() == Some("merge_queue_rebounce") => Some(attempt),
                Ok(_) => None,
                Err(err) => {
                    tracing::warn!(
                        work_item_id = %candidate.work_item_id,
                        ?err,
                        "merge poller: failed to inspect queue-side CI state; preserving PR-head classification",
                    );
                    None
                }
            };
            fetched_active.as_ref()
        }
    };
    // The PR-head rollup and the merge-queue merged-result rollup are
    // distinct CI surfaces. While an attributed queue-side remediation is
    // active, the authoritative combined state is failing even though the
    // PR-head rollup is normally green. Do not let the latter overwrite the
    // former on every lifecycle poll.
    let ci_state = if active_queue_failure.is_some() {
        "fail"
    } else {
        ci_state_str(&open.ci)
    };
    let review_state = probe.review.as_db_str();
    let mergeable_state = mergeable_state_str(open.mergeability);
    let ci_detail = active_queue_failure
        .map(|attempt| queue_failure_detail_json(&attempt.failed_checks))
        .unwrap_or_else(|| ci_detail_json(&open.ci));
    let review_detail = review_detail_json(probe.review.reviewers());
    let raw_merge_queue_state = merge_queue_state_str(probe.in_merge_queue, probe.auto_merge_enabled);
    let raw_merge_queue_detail = merge_queue_detail_json(probe);

    // mono#2023: GitHub keeps auto-merge armed (and a merge-queue
    // entry alive) while required checks are red — "merge when ready" just
    // waits, it never disarms. Left alone, that strands the card in the
    // macOS kanban's "Merging" section (`merge_queue_state != None` is what
    // gates `isInMergingSection`) right next to a red CI chip, which reads
    // as contradictory and hides that a fix revision may already be running
    // (`ci_watch`'s `blocked: ci_failure` → revision flow keeps `status` at
    // `in_review` the whole time, so nothing else would move the card).
    //
    // Demote the *lane* signal only — never GitHub's own arming, which must
    // stay untouched to avoid churning notifications or racing the real
    // merge queue — whenever this same poll observes failing required CI.
    // Once CI recovers to success/in_progress, this override lifts and the
    // next poll recomputes `merge_queue_state` from the untouched raw probe
    // as usual, so the card returns to Merging with no separate re-arm path.
    //
    // Scoped to the `auto_merge_enabled` bucket only — never `"queued"`.
    // `renumber_merge_queue`'s invariant (below) requires that a queued
    // member's position never races between "kept" and "excluded" outside
    // of a genuine `"queued"` state transition; blanket-demoting a queued
    // row on a transient required-check failure would violate that while
    // GitHub may still be actively merging it.
    let (merge_queue_state, merge_queue_detail) =
        if ci_state == "fail" && raw_merge_queue_state == Some("auto_merge_enabled") {
            (None, None)
        } else {
            (raw_merge_queue_state, raw_merge_queue_detail)
        };

    // A `trunk_queue`-mechanism task never actually sits in GitHub's native
    // merge queue or arms GitHub auto-merge, so this probe's
    // `in_merge_queue`/`auto_merge_enabled` reads are always false for it —
    // yet `handle_trunk_queue_merge` optimistically writes
    // `merge_queue_state = "queued"` right after a successful Trunk submit
    // so the card shows in the Merging lane. Without this gate, the very
    // next sweep would wipe that write back to NULL, bouncing the card out
    // of Merging within one poll interval. `preserve_merge_queue_state`
    // leaves the stored merge-queue columns exactly as `handle_trunk_queue_merge`
    // (or the Trunk queue poller, once it lands) left them.
    let preserve_trunk_merge_queue_state = is_trunk_queue_product(work_db, &candidate.product_id);
    // A direct GitHub merge intent is per-row, not a product-wide exception:
    // the first probe after `gh pr merge --auto --squash` can legitimately
    // contain neither a queue entry nor auto-merge request while GitHub
    // materializes its derived state. Preserve only that empty observation;
    // a positive observation remains the fresh detailed projection, and an
    // observed dequeue timeline event retires the intent separately.
    //
    // An absent `headRefOid` is not evidence the submitted head departed —
    // every other `head_ref_oid` consumer in this crate fails closed on it.
    // Only an observed, differing head retires the intent as `head_advanced`.
    //
    // Do not drop preserve on red CI: writing NULL is destructive, and a
    // later green empty probe would then preserve that hole. The demotion
    // above owns display for `auto_merge_enabled` only; a queued row (this
    // intent's projection) must stay queued through a transient required-
    // check failure, matching `renumber_merge_queue`'s membership invariant.
    let preserve_github_merge_queue_state = raw_merge_queue_state.is_none()
        && match work_db.get_active_github_merge_intent(&candidate.work_item_id, &candidate.pr_url) {
            Ok(Some(intent)) => match probe.head_ref_oid.as_deref() {
                None => true,
                Some(sha) if sha == intent.head_sha => true,
                Some(sha) => {
                    let is_stale_pre_rebase_head = work_db
                        .get_ci_merge_rebase(&candidate.work_item_id, &candidate.pr_url)
                        .ok()
                        .flatten()
                        .is_some_and(|rebase| {
                            rebase.head_sha_before.as_deref() == Some(sha)
                                && rebase.rebased_head_sha.as_deref() == Some(intent.head_sha.as_str())
                        });
                    if is_stale_pre_rebase_head {
                        true
                    } else {
                        match work_db.retire_github_merge_intent(
                            &candidate.work_item_id,
                            &candidate.pr_url,
                            "head_advanced",
                        ) {
                            Ok(_) => false,
                            Err(err) => {
                                tracing::warn!(
                                    work_item_id = %candidate.work_item_id,
                                    pr_url = %candidate.pr_url,
                                    ?err,
                                    "merge poller: failed to retire GitHub merge intent after head advanced",
                                );
                                true
                            }
                        }
                    }
                }
            },
            Ok(None) => false,
            Err(err) => {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    ?err,
                    "merge poller: failed to inspect GitHub merge intent; treating empty probe as unprotected",
                );
                false
            }
        };
    if preserve_github_merge_queue_state {
        // A prior poll may already have written NULL (the old CI-fail gate).
        // Re-assert `'queued'` so recovery does not depend on GitHub's
        // projection having materialized; preserve then keeps the restored
        // row (or the original queued detail) untouched.
        let detail = boss_engine_storage_values::merge_queue_section::queued_no_position_detail();
        match work_db.reassert_github_merge_queue_if_cleared(&candidate.work_item_id, &detail) {
            Ok(true) => {
                publisher
                    .publish_work_item_changed(
                        &candidate.product_id,
                        &candidate.work_item_id,
                        "merge_queue_lane_reasserted",
                    )
                    .await;
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    ?err,
                    "merge poller: failed to re-assert GitHub merge-queue lane for a live intent",
                );
            }
        }
    }
    let preserve_merge_queue_state = preserve_trunk_merge_queue_state || preserve_github_merge_queue_state;

    let outcome = match work_db.update_task_pr_poll_state(
        &candidate.work_item_id,
        PrPollStateInput {
            caller,
            ci_required_state: ci_state,
            review_required_state: review_state,
            ci_required_detail: ci_detail.as_deref(),
            review_required_detail: review_detail.as_deref(),
            merge_queue_state,
            merge_queue_detail: merge_queue_detail.as_deref(),
            preserve_merge_queue_state,
            pr_mergeable_state: mergeable_state,
            pr_merge_state_status: (!probe.raw_merge_state_status.is_empty())
                .then_some(probe.raw_merge_state_status.as_str()),
            pr_head_sha: probe.head_ref_oid.as_deref(),
            pr_updated_at: probe.updated_at.as_deref(),
        },
    ) {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "merge poller: failed to update PR poll state",
            );
            return;
        }
    };

    if outcome.changed {
        // State changed — emit event so the macOS kanban refreshes the
        // card's CI / review / merging indicators within the poll interval.
        // `update_task_pr_poll_state` rewrites only this task's CI / review /
        // mergeability / merge-queue columns, so the change is runtime-class.
        publisher
            .publish_work_item_runtime_changed(&candidate.product_id, &candidate.work_item_id, "pr_poll_state_updated")
            .await;
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            ci_state,
            review_state,
            in_merge_queue = probe.in_merge_queue,
            "merge poller: PR poll state changed",
        );
    }

    // Every Merging-lane transition is INFO in both directions. The normal
    // sweep-changed trace is DEBUG and is not emitted in production, so it
    // cannot diagnose a `Some → None` bounce that is visible on the board.
    let effective_merge_queue_state = if preserve_merge_queue_state {
        outcome.prior_merge_queue_state.as_deref()
    } else {
        merge_queue_state
    };
    if outcome.prior_merge_queue_state.as_deref() != effective_merge_queue_state {
        tracing::info!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            prior_merge_queue_state = outcome.prior_merge_queue_state.as_deref(),
            new_merge_queue_state = effective_merge_queue_state,
            in_merge_queue = probe.in_merge_queue,
            auto_merge_enabled = probe.auto_merge_enabled,
            ci_state,
            "merge poller: card Merging lane state changed",
        );
    }

    // Whole-queue renumbering (mono#1997): this write only ever touched
    // `candidate`'s own row, so a sibling still sitting on a now-stale
    // position from before this PR entered/exited/reordered would be left
    // showing a duplicate or missing badge until its own next individual
    // probe. Any transition into or out of `"queued"` — or a position/
    // state change while still queued — can shift where every OTHER queued
    // member of this product belongs, so re-derive the whole set whenever
    // this row's queue membership moved on either side of the update.
    // Never for a `preserve_merge_queue_state` task: its own row's
    // merge-queue columns were untouched by this write (whatever
    // `merge_queue_state`/`outcome.prior_merge_queue_state` read here reflect
    // the untouched GitHub probe, not the trunk-owned stored value), and a
    // whole-product renumbering pass is a GitHub-native-queue concept that
    // does not apply to a `trunk_queue` product's rows.
    let now_queued = merge_queue_state == Some("queued");
    let was_queued = outcome.prior_merge_queue_state.as_deref() == Some("queued");
    if !preserve_merge_queue_state && outcome.changed && (now_queued || was_queued) {
        renumber_merge_queue(work_db, publisher, &candidate.product_id).await;
    }

    // Badge reconciliation (issue #1151): the macOS "ci failing" chip is
    // driven by `ci_remediations` rows and is cleared only by an explicit
    // `CiFailureCleared` / `CiRemediationSucceeded` event. The blocked-signal
    // retire path (`ci_watch::on_ci_resolved`, dispatched from
    // `maybe_clear_blocked`) emits one of those — but only when the chore is
    // still `blocked` or carries an active CI signal/attempt to retire. When
    // CI goes green at a *new* head and the engine's own block was already
    // quiesced (or never armed a side-table signal), no retire fires and the
    // chip is stranded on an earlier head's failure (a prior leak). The poll
    // observes the truth — `fail → success` at the current head — so broadcast
    // `CiFailureCleared` here as a head-keyed safety net. Like every clear it
    // carries the PR's newest attempt row, so the app's copy of that attempt
    // cannot keep reading as "still fixing" afterwards. This is idempotent
    // with any retire-path clear that already ran this sweep: the macOS
    // handler simply drops the chip (a no-op if already gone) and leaves the
    // "ci auto-fixed" badge untouched. Restricted to `fail → success` so it
    // never clobbers an active attempt's in-flight badge on `fail → in_progress`
    // (that transition is owned by `on_ci_in_flight_supersedes_failure`, #901).
    if outcome.prior_ci_state.as_deref() == Some("fail") && ci_state == "success" {
        crate::remediation_push::publish_ci_failure_cleared(work_db, publisher, candidate).await;
        tracing::info!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            "merge poller: CI recovered to success at current head; \
             broadcast CiFailureCleared to clear any stale ci-failing badge",
        );
    }
}

fn queue_failure_detail_json(failed_checks: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(failed_checks) {
        Ok(serde_json::Value::Array(items)) if !items.is_empty() => serde_json::to_string(&items).ok(),
        _ => serde_json::to_string(&vec![serde_json::json!({
            "name": "merge queue",
            "conclusion": "FAILURE",
        })])
        .ok(),
    }
}

/// Open `pr_base_refused` attentions for the item, or `None` when they could
/// not be read.
fn open_base_refusals(
    work_db: &WorkDb,
    candidate: &PendingMergeCheck,
) -> Option<Vec<boss_protocol::WorkAttentionItem>> {
    match work_db.list_attention_items_for_work_item(&candidate.work_item_id) {
        Ok(items) => Some(
            items
                .into_iter()
                .filter(|item| item.kind == PR_BASE_REFUSED && item.status == "open")
                .collect(),
        ),
        Err(err) => {
            tracing::warn!(?err, "merge poller: could not read PR base refusals");
            None
        }
    }
}

/// When the probe shows the PR targets a branch other than the repository
/// default, raise (once per unchanged observation) the `pr_base_refused`
/// attention and return `true`: the PR must not complete the work item or
/// release its dependents.
async fn refuse_unlanded_base(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
    open_refusals: &[boss_protocol::WorkAttentionItem],
) -> bool {
    let Some(reason) = &probe.base_branch_error else {
        return false;
    };
    let state = match &probe.state {
        PrLifecycleState::Open(_) => "open",
        PrLifecycleState::Merged => "merged",
        PrLifecycleState::ClosedUnmerged => "closed without merging",
    };
    let detail = format!(
        "{} was observed as {state} with base `{}`. It cannot complete this work item or release dependents. {reason}",
        candidate.pr_url,
        probe.base_ref_name.as_deref().unwrap_or("unknown"),
    );
    // Do not re-raise and republish an unchanged observation every pass.
    if !open_refusals.iter().any(|item| item.body_markdown == detail) {
        match work_db.upsert_refreshing_work_item_attention(
            &candidate.work_item_id,
            PR_BASE_REFUSED,
            "PR has not landed in the default branch",
            &detail,
        ) {
            Ok(()) => {
                publisher
                    .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, PR_BASE_REFUSED)
                    .await
            }
            Err(err) => tracing::warn!(?err, "merge poller: could not surface PR base refusal"),
        }
    }
    true
}

pub(crate) async fn mark_merged(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    host: Option<&dyn SweepHost>,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
) -> bool {
    // Every merge-completion caller shares this guard: a merge into a branch
    // other than the default does not land the work.
    if probe.base_branch_error.is_some() {
        if let Some(open) = open_base_refusals(work_db, candidate) {
            refuse_unlanded_base(work_db, publisher, candidate, probe, &open).await;
        }
        return false;
    }
    let observation = match work_db.observe_chore_pr_merged(&candidate.work_item_id, &candidate.pr_url) {
        Ok(observation) => observation,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "merge poller: failed to mark work item merged",
            );
            return false;
        }
    };
    if let Some(prior) = observation.cleared_queue_state.as_deref() {
        tracing::info!(
            work_item_id = %candidate.work_item_id,
            pr_url = %candidate.pr_url,
            prior_merge_queue_state = prior,
            new_merge_queue_state = Option::<&str>::None,
            in_merge_queue = probe.in_merge_queue,
            auto_merge_enabled = probe.auto_merge_enabled,
            "merge poller: card Merging lane state changed",
        );
    }
    let Some(updated) = observation.completed_task else {
        if observation.changed {
            publisher
                .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "pr_merge_observed")
                .await;
        }
        return false;
    };
    publisher
        .publish_work_item_changed(&candidate.product_id, &updated.id, "pr_merged")
        .await;
    // Kick the scheduler so any auto-unblocked dependents (whose
    // executions were just promoted to `ready` by the dep cascade)
    // are dispatched promptly rather than waiting for the next
    // external event or reconciler tick.
    publisher.kick_scheduler();
    tracing::info!(
        work_item_id = %updated.id,
        kind = %updated.kind,
        pr_url = %candidate.pr_url,
        "merge poller: PR merged; work item moved to done",
    );
    // Auto-populate the per-task doc pointer on merge for every kind.
    // The project-level design-doc pointer is a separate mechanism
    // (the `kind=design` + project_id arm below). Errors are logged
    // inside the detector.
    let Some(host) = host else {
        return true;
    };
    host.on_task_doc_pr_merged(
        work_db,
        &updated.id,
        &candidate.product_id,
        &candidate.pr_url,
        probe.base_ref_name.as_deref(),
    )
    .await;
    if matches!(updated.kind, TaskKind::Design | TaskKind::DesignPostmortem)
        && let Some(ref project_id) = updated.project_id
    {
        host.on_design_pr_merged(
            work_db,
            &updated.id,
            &candidate.product_id,
            project_id,
            &candidate.pr_url,
            probe.base_ref_name.as_deref(),
        )
        .await;

        // Auto-populate the project's implementation tasks from the merged
        // design doc. `on_design_pr_merged` above has just
        // written the project's design-doc pointer, so the Populator reads
        // it fresh. The enqueue is cheap and synchronous — it spawns the
        // multi-second Planner call on a background task so the poller loop
        // never blocks — and a no-op unless the capability was installed at
        // engine startup (so unit tests never reach the network).
        //
        // Only the initial `design` task triggers this: a postmortem's PR
        // updates the *existing* doc to reflect what already shipped, not a
        // fresh "Proposed implementation task breakdown" the Populator
        // should materialise into new work — re-running it here would spawn
        // a duplicate task batch off a doc that was never meant to seed one.
        if updated.kind == TaskKind::Design {
            host.enqueue_from_merge(
                work_db,
                PopulationRequest {
                    project_id: project_id.clone(),
                    product_id: candidate.product_id.clone(),
                    design_task_id: updated.id.clone(),
                    pr_url: candidate.pr_url.clone(),
                },
            );
        }
    }
    true
}

/// On-close counterpart to [`mark_merged`]: retire a chore whose bound PR
/// was closed **without** merging. Parallel structure, deliberately much
/// thinner — a closed-unmerged PR has no base ref to auto-populate a doc
/// pointer from and no dependent Populator enqueue, since nothing shipped.
///
/// `chore-lifecycle-pr-closed-unmerged.md` — PR closure is a definitive
/// human signal that this attempt is over; the engine must NOT spawn a redo
/// execution here (that would contradict the human's decision to close
/// without merging). If the work should be redone, a human moves the row
/// back to `todo`/`active` manually.
pub(crate) async fn mark_closed_unmerged(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
) -> bool {
    let updated = match work_db.mark_chore_pr_closed_unmerged(&candidate.work_item_id, &candidate.pr_url) {
        Ok(Some(task)) => task,
        Ok(None) => return false,
        Err(err) => {
            tracing::warn!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                ?err,
                "merge poller: failed to mark work item closed-unmerged",
            );
            return false;
        }
    };
    publisher
        .publish_work_item_changed(&candidate.product_id, &updated.id, "pr_closed_unmerged")
        .await;
    publisher.kick_scheduler();
    tracing::info!(
        work_item_id = %updated.id,
        kind = %updated.kind,
        pr_url = %candidate.pr_url,
        "merge poller: PR closed without merge; work item retired to done \
         (closed-unmerged — not a merge)",
    );
    true
}

/// Settle the `in_review` revisions of a chain whose root is terminal or
/// deleted, once a probe reports their PR merged or closed. A no-op while the
/// PR is open.
///
/// The root's own merged or closed transition is what settles a chain's
/// revisions; this is that step for a chain whose root will never make it.
/// A merge into a branch other than the default did not land the work, so it
/// is recorded as a close: the root that would have carried a base refusal
/// is not there to carry it.
pub(crate) async fn settle_orphaned_revision_chain(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    completion_handler: Option<&dyn CompletionRequests>,
    chain: &crate::work::OrphanedRevisionChain,
    probe: &PrLifecycleProbe,
    outcome: &mut SweepOutcome,
) {
    let (close, reason, guide_reason) = match &probe.state {
        PrLifecycleState::Open(_) => return,
        PrLifecycleState::Merged if probe.base_branch_error.is_none() => {
            (crate::work::PrClose::Merged, "pr_merged", "parent PR merged")
        }
        PrLifecycleState::Merged | PrLifecycleState::ClosedUnmerged => (
            crate::work::PrClose::ClosedUnmerged,
            "pr_closed_unmerged",
            "PR closed unmerged",
        ),
    };
    let settled = match work_db.settle_orphaned_revisions_after_pr_close(&chain.chain_root_id, &chain.pr_url, close) {
        Ok(settled) => settled,
        Err(err) => {
            tracing::warn!(
                chain_root_id = %chain.chain_root_id,
                pr_url = %chain.pr_url,
                ?err,
                "merge poller: failed to settle revisions left in review by a closed PR",
            );
            return;
        }
    };
    stop_live_review_guides(
        work_db,
        completion_handler,
        &chain.chain_root_id,
        &chain.pr_url,
        guide_reason,
    )
    .await;
    if settled.is_empty() {
        return;
    }
    tracing::info!(
        chain_root_id = %chain.chain_root_id,
        pr_url = %chain.pr_url,
        basis = close.orphaned_revision_basis(),
        revisions = ?settled,
        "merge poller: PR ended with its owning row already terminal or deleted; revisions in review moved to done",
    );
    for revision_id in &settled {
        publisher
            .publish_work_item_changed(&chain.product_id, revision_id, reason)
            .await;
    }
    publisher.kick_scheduler();
    outcome.orphaned_revisions_settled += settled.len();
}

/// Extract the PR number from a GitHub PR URL as an `i64` for DB storage.
///
/// Re-exported from the persistence layer, which owns the stored `i64`
/// representation; it in turn is a thin adaptor over the canonical
/// [`boss_github::pr_url::pr_number_from_url`] helper (including tolerance
/// for the `/pull/<N>/files`, `?query`, and `#fragment` decorations).
/// Returns `None` for any non-canonical URL.
pub use crate::work::stored_pr_number;

/// Argument bundle for [`sweep_stalled_reviewer`]; `review_result_written`
/// says the latest review execution recorded an informative `ReviewResult`.
/// A live pre-merge batch retains the hold even when this is true, regardless
/// of the fan-out flag; its consolidated verdict owns advancement.
pub struct StalledReview<'a> {
    pub task_id: &'a str,
    pub product_id: &'a str,
    pub pr_url: &'a str,
    pub review_result_written: bool,
    /// When true (review-batch fan-out), a hold with no live pre-merge batch
    /// and no non-terminal `pr_review` is owned by
    /// [`sweep_deferred_review_admission`] so a previous cycle's verdict
    /// cannot skip review of a new head.
    pub skip_unbatched_review_holds: bool,
}

/// Reviewer-fallback: reconcile a task held in `active` after its latest AI
/// reviewer pass either finished without advancing it (missed Stop hook) or
/// ran past the stale threshold. A pass with a durable `ReviewResult` may
/// advance to `in_review`; a pass without one stays in Doing and is re-fired.
/// A live pre-merge batch suppresses both advancement and re-firing, including
/// when a prior execution has written a result; the batch owns its verdict.
///
/// Incident (2026-07-04, PR spinyfin/mono#1766): this fallback used
/// to ONLY unstick the kanban lane, silently leaving the PR with no completed
/// AI review — indistinguishable in the UI from a task that was reviewed and
/// found clean. Missing-result passes are now re-enqueued through
/// `WorkDb::request_pr_review` (the same path `bossctl review start` and the
/// dead-review recovery sweep use) and get a
/// `pr_review_died_without_findings` attention item. Re-enqueue is
/// best-effort: a still-running timed-out execution must first be reaped by
/// liveness reconciliation, after which the standalone recovery sweep picks
/// it up. A successful re-fire records one attention item; a churn-guard trip
/// parks the task for a human instead of looping indefinitely.
pub async fn sweep_stalled_reviewer(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    stalled: StalledReview<'_>,
    pr_checker: &dyn PrStateChecker,
    outcome: &mut SweepOutcome,
) {
    let StalledReview {
        task_id,
        product_id,
        pr_url,
        review_result_written,
        skip_unbatched_review_holds,
    } = stalled;
    let cycle_root = match work_db.resolve_review_cycle_root(task_id) {
        Ok(root) => root,
        Err(error) => {
            tracing::warn!(task_id, ?error, "reviewer-fallback: failed to resolve review cycle");
            return;
        }
    };
    match work_db.has_live_pre_merge_review_batch(&cycle_root) {
        // Batch recovery owns retries, quorum and the consolidated verdict.
        // A dead leaf does not authorize a memberless legacy review, even
        // when every member execution is terminal or fan-out was disabled.
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(task_id, ?error, "reviewer-fallback: failed to check batch ownership");
            return;
        }
    }
    if skip_unbatched_review_holds && !has_nonterminal_pr_review(work_db, task_id, &cycle_root) {
        return;
    }
    if review_result_written {
        match work_db.advance_pending_review_task_to_in_review(task_id) {
            Ok(true) => {
                tracing::info!(
                    task_id,
                    pr_url,
                    "merge poller: reviewer-fallback advanced task from active to in_review \
                     after confirming a durable ReviewResult",
                );
                publisher
                    .publish_work_item_changed(product_id, task_id, "reviewer_fallback_advanced")
                    .await;
                outcome.reviewer_fallback_advanced += 1;
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(
                    task_id,
                    pr_url,
                    ?err,
                    "merge poller: reviewer-fallback failed to advance reviewed task to in_review",
                );
            }
        }
        return;
    }

    let churn_cutoff =
        boss_engine_utils::epoch_time::now_epoch_secs() - crate::work::ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS;
    let recent_uninformative = match work_db.count_recent_uninformative_pr_review_executions(task_id, churn_cutoff) {
        Ok(count) => count,
        Err(err) => {
            tracing::warn!(
                task_id,
                pr_url,
                ?err,
                "merge poller: failed to count unproductive review attempts"
            );
            return;
        }
    };
    if recent_uninformative >= crate::work::ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
        let failing_ids = work_db
            .list_recent_uninformative_pr_review_execution_ids(task_id, churn_cutoff)
            .unwrap_or_default();
        work_db.file_churn_guard_parked_attention(
            task_id,
            "merge_poller_reviewer_fallback",
            recent_uninformative,
            &failing_ids,
            "unproductive pr_review executions",
        );
        tracing::warn!(
            task_id,
            pr_url,
            recent_uninformative,
            threshold = crate::work::ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD,
            "merge poller: reviewer-fallback churn guard parked task without re-firing review",
        );
        return;
    }

    let prior_pr_review_id = match work_db.list_executions(Some(task_id)) {
        Ok(executions) => executions
            .into_iter()
            .rev()
            .find(|execution| execution.kind == boss_protocol::ExecutionKind::PrReview)
            .map(|execution| execution.id),
        Err(err) => {
            tracing::warn!(
                task_id,
                pr_url,
                ?err,
                "merge poller: failed to list executions before reviewer-fallback re-fire"
            );
            return;
        }
    };

    match work_db.request_pr_review(task_id, pr_checker) {
        Ok(execution) => {
            if prior_pr_review_id.as_deref() == Some(execution.id.as_str()) {
                // `request_pr_review` is idempotent: a `ready` row sitting in
                // the review-pool queue is returned as Ok without inserting
                // anything. That is the `review_queued` state, not a death,
                // and must not file `pr_review_died_without_findings` or bump
                // the re-fire counter on every 60s sweep.
                tracing::debug!(
                    task_id,
                    pr_url,
                    execution_id = %execution.id,
                    "merge poller: reviewer-fallback observed an already-queued pr_review; not a re-fire"
                );
                return;
            }
            tracing::info!(
                task_id,
                pr_url,
                execution_id = %execution.id,
                "merge poller: reviewer-fallback re-enqueued a fresh pr_review execution; \
                 task remains in Doing until a ReviewResult is recorded",
            );
            publisher
                .publish_work_item_changed(product_id, task_id, "reviewer_fallback_review_refired")
                .await;
            outcome.reviewer_fallback_review_refired += 1;
            let body = "The automated reviewer for this PR did not complete a review pass. Its \
                 `pr_review` execution either finished without writing a `ReviewResult`, or remained \
                 running past the 10-minute stale threshold. This is distinct from \"reviewed, no \
                 findings.\" The task remains in Doing, and a fresh review has been re-enqueued; \
                 dismiss this item once that pass completes."
                .to_owned();
            if let Err(err) = work_db.create_attention_item(CreateAttentionItemInput {
                execution_id: None,
                work_item_id: Some(task_id.to_owned()),
                kind: boss_engine_storage_values::attention_kinds::PR_REVIEW_DIED_ATTENTION_KIND.to_owned(),
                status: None,
                title: "Automated review did not complete — replacement pending".to_owned(),
                body_markdown: body,
                resolved_at: None,
            }) {
                tracing::warn!(
                    task_id,
                    ?err,
                    "merge poller: failed to file review-missing attention item"
                );
            }
        }
        Err(err) => {
            // Best-effort: the stale reviewer may still be nominally running.
            // The standalone recovery sweep re-attempts once liveness
            // reconciliation reaps it.
            tracing::warn!(
                task_id,
                pr_url,
                error = %err,
                "merge poller: reviewer-fallback could not re-enqueue a review",
            );
        }
    }
}

fn has_nonterminal_pr_review(work_db: &WorkDb, task_id: &str, cycle_root_id: &str) -> bool {
    let is_live_review = |work_item_id: &str| {
        work_db
            .list_executions(Some(work_item_id))
            .ok()
            .map(|executions| {
                executions.iter().any(|execution| {
                    execution.kind == boss_protocol::ExecutionKind::PrReview && !execution.status.is_terminal()
                })
            })
            .unwrap_or(false)
    };
    is_live_review(task_id) || (cycle_root_id != task_id && is_live_review(cycle_root_id))
}

pub(crate) fn sample_review_pool_gauges(work_db: &WorkDb, outcome: &mut SweepOutcome) {
    match work_db.review_pool_reserved_units() {
        Ok((pre, post)) => outcome.reserved_review_units = pre + post,
        Err(err) => tracing::warn!(?err, "merge poller: failed to sample review-pool reservations"),
    }
    match work_db.list_tasks_awaiting_pre_merge_review_admission() {
        Ok(items) => outcome.deferred_pre_merge_count = items.len(),
        Err(err) => tracing::warn!(?err, "merge poller: failed to sample deferred pre-merge holds"),
    }
}
