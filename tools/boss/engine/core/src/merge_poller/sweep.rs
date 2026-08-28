use super::*;

/// Outcome of one sweep. Used for logging and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct SweepOutcome {
    pub merged: usize,
    pub conflict_flagged: usize,
    pub conflict_cleared: usize,
    pub ci_flagged: usize,
    pub ci_cleared: usize,
    /// Number of `waiting_human` executions whose chore was missing a
    /// `pr_url` but whose workspace now resolves to a fresh PR. These
    /// are the rows the on-Stop hook missed (typically because GitHub's
    /// `commits/{sha}/pulls` index lagged a fresh `gh pr create`). The
    /// recheck moved them to `in_review` (or `done` if the PR was
    /// already merged).
    pub pr_recheck_recovered: usize,
    /// Number of `waiting_human` executions where this sweep ran a
    /// recheck but the detector still did not resolve a bindable PR
    /// (returned `None`, `Stale`, `EmptyDiff`, or errored). Mirrors
    /// the info-level log in `sweep_pending_pr` so callers (and tests)
    /// can assert the recheck path actually reached the executions in
    /// its candidate list, even when no transition fired.
    pub pr_recheck_unresolved: usize,
    /// Number of `in_review` PRs flipped to `blocked: ci_failure` due to
    /// a merge-queue `FAILED_CHECKS` dequeue event detected in this sweep.
    pub merge_queue_rebounced: usize,
    /// Number of stranded `ci_remediations` attempts (status `pending`,
    /// no live execution) for which a fresh execution was re-emitted.
    /// Covers the back-to-back dequeue scenario where two dequeue events
    /// arrive in the same sweep: the first flips the task (consuming the
    /// WHERE guard) and the second inserts a ci_remediations row but
    /// cannot create an execution because the task is already blocked.
    pub ci_remediation_redispatched: usize,
    /// Number of terminal executions (abandoned/completed/failed within
    /// the lookback window) whose task was still `active` with no `pr_url`
    /// but now has a detectable PR. These arise from the double-spawn race
    /// (Bug B): exec_A was abandoned while its pane was still running, and
    /// the normal `pending_pr_recheck` sweep (which only watches
    /// `waiting_human`) cannot recover them.
    pub late_pr_recovered: usize,
    /// Number of in-flight revision executions stopped (force-released +
    /// cancelled) because their parent PR merged or closed while they were
    /// queued or running. Each stopped execution corresponds to a revision
    /// task that was already blocked in the same DB transaction that
    /// transitioned the parent to `done`.
    pub revision_invalidated: usize,
    /// Number of live worker executions force-stopped because their task
    /// auto-transitioned back to `in_review` after the engine detected
    /// the PR's CI had gone green. The worker (typically still polling CI
    /// to see whether its own fix landed) has nothing useful left to do
    /// once the task reaches Review, so leaving it alive only ties up a
    /// slot (issue #898).
    pub worker_stopped_on_review: usize,
    /// Number of stranded `blocked` parents (NULL scalar `blocked_reason`,
    /// empty active-signal set, remediation-owned, bound PR) that this
    /// sweep re-canonicalised back into the standard
    /// `blocked: merge_conflict` / `blocked: ci_failure` loop after
    /// observing the PR is still dirty/red. Recovers the invariant
    /// violation where a parent rests `blocked` with no signal while its
    /// PR conflicts/fails (the PR #1077 strand).
    pub stranded_blocked_recanonicalized: usize,
    /// Number of reviewer-fallback passes with a durable `ReviewResult`
    /// released from Doing (`active`) to Review (`in_review`).
    pub reviewer_fallback_advanced: usize,
    /// Number of missing-result reviewer passes re-enqueued while their task
    /// remains in Doing awaiting a durable review result.
    pub reviewer_fallback_review_refired: usize,
    /// Number of `in_revision` comments reopened (comment-intent-
    /// classification design §"Reconciliation", task 2c) because the task
    /// that was addressing them — or a revision in its chain — had its PR
    /// closed without merging in this sweep.
    pub comments_reopened: usize,
    /// Number of `in_review` chores/project_tasks retired to `done`
    /// because their bound PR was closed **without** merging
    /// (`chore-lifecycle-pr-closed-unmerged.md`) — the on-close
    /// counterpart to `merged` above. Kept as a separate counter (rather
    /// than folded into `merged`) so operators can tell "shipped" from
    /// "abandoned" retires apart in the sweep summary log.
    pub closed_unmerged: usize,
    /// Number of Trunk merge-queue episodes adopted this sweep because
    /// Trunk's own head check reported an eviction for a `trunk_queue`
    /// product's PR that no active merge intent was tracking — i.e. a queue
    /// episode a human started through Trunk's own affordance rather than
    /// through Boss. See [`crate::trunk_queue_adopt`]. A non-zero value is
    /// the operator-visible counterpart to the `warn!` each adoption logs:
    /// these are evictions that, before adoption existed, were remediated by
    /// nobody.
    pub trunk_episodes_adopted: usize,
}

impl SweepOutcome {
    pub(crate) fn total_transitions(self) -> usize {
        self.merged
            + self.conflict_flagged
            + self.conflict_cleared
            + self.ci_flagged
            + self.ci_cleared
            + self.pr_recheck_recovered
            + self.merge_queue_rebounced
            + self.ci_remediation_redispatched
            + self.late_pr_recovered
            + self.revision_invalidated
            + self.worker_stopped_on_review
            + self.stranded_blocked_recanonicalized
            + self.comments_reopened
            + self.closed_unmerged
            + self.trunk_episodes_adopted
    }
}

/// How long a batched probe snapshot may be used to write lifecycle
/// transitions before it must be re-taken.
///
/// A pass takes ONE batched probe up front and then walks every candidate
/// against it. That is fine when the pass takes a second; it is a
/// correctness problem when it doesn't. In the incident this constant
/// exists for, PR #2346 merged ten seconds after a pass took its snapshot,
/// and the pass — stuck behind 32 minutes of inline conflict remediation —
/// went on writing state from that half-hour-old snapshot. Sized to the
/// nominal full-sweep interval: a snapshot older than one whole sweep
/// cadence is by definition something the next sweep should have replaced.
pub(crate) const PROBE_SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(60);

/// Re-probes allowed within a single pass before the pass gives up on its
/// remaining candidates. Bounded so a pathologically slow pass can't turn
/// into an unbounded GitHub-quota amplifier; abandoning the remainder is
/// safe because every candidate list is rebuilt from the DB next pass.
pub(crate) const MAX_PROBE_REFRESHES_PER_PASS: u8 = 2;

/// The batched probe result for one pass, plus the age bookkeeping that
/// keeps a long pass from transitioning work items off a stale view of
/// GitHub. See [`PROBE_SNAPSHOT_MAX_AGE`].
pub(crate) struct ProbeSnapshot {
    pub(crate) results: HashMap<String, std::result::Result<PrLifecycleProbe, String>>,
    /// `tokio::time::Instant`, not `std::time::Instant`, so the ageing logic
    /// is drivable from tests under a paused clock (`tokio::time::advance`)
    /// instead of only by a real 60-second wall-clock stall. Identical to the
    /// std clock outside a paused runtime.
    taken_at: tokio::time::Instant,
    refreshes_left: u8,
}

impl ProbeSnapshot {
    pub(crate) fn new(results: HashMap<String, std::result::Result<PrLifecycleProbe, String>>) -> Self {
        Self {
            results,
            taken_at: tokio::time::Instant::now(),
            refreshes_left: MAX_PROBE_REFRESHES_PER_PASS,
        }
    }

    /// Returns `true` when it is safe to write state from `self.results`.
    ///
    /// Fresh snapshot → `true` immediately (the overwhelmingly common case,
    /// and free — no clock-driven GitHub traffic). Stale snapshot → re-probe
    /// and return `true`. Stale snapshot with the per-pass refresh budget
    /// already spent → `false`, and the caller must stop: writing off a
    /// snapshot that old is worse than deferring to the next pass.
    ///
    /// `remaining_urls` is called **only** on the re-probe path and must
    /// yield just the candidates the pass has not walked yet. Re-probing the
    /// pass's whole original URL set would re-fetch every candidate already
    /// transitioned earlier in the pass, multiplying GitHub batch cost on
    /// precisely the path that is already slow enough to have gone stale.
    /// Nothing is lost by dropping the processed prefix: those entries are
    /// never read again, and every candidate list is rebuilt from the DB next
    /// pass. It is a closure rather than a slice so the common (fresh) case
    /// allocates nothing.
    pub(crate) async fn ensure_fresh(
        &mut self,
        probe: &dyn MergeProbe,
        remaining_urls: impl FnOnce() -> Vec<String>,
    ) -> bool {
        let age = self.taken_at.elapsed();
        if age <= PROBE_SNAPSHOT_MAX_AGE {
            return true;
        }
        if self.refreshes_left == 0 {
            tracing::warn!(
                snapshot_age_ms = age.as_millis(),
                max_age_ms = PROBE_SNAPSHOT_MAX_AGE.as_millis(),
                "merge poller: probe snapshot stale and the per-pass re-probe budget is spent; \
                 abandoning the rest of this pass rather than transitioning off a stale snapshot",
            );
            return false;
        }
        self.refreshes_left -= 1;
        let urls = remaining_urls();
        tracing::warn!(
            snapshot_age_ms = age.as_millis(),
            max_age_ms = PROBE_SNAPSHOT_MAX_AGE.as_millis(),
            pr_count = urls.len(),
            "merge poller: probe snapshot went stale mid-pass; re-probing the unprocessed remainder \
             before writing further state",
        );
        self.results = probe.probe_batch(&urls).await;
        self.taken_at = tokio::time::Instant::now();
        true
    }
}

/// The still-unprocessed candidates' PR urls, de-duplicated and in walk
/// order — what [`ProbeSnapshot::ensure_fresh`] re-probes when it must.
fn remaining_probe_urls<'a>(remaining: impl IntoIterator<Item = &'a PendingMergeCheck>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    remaining
        .into_iter()
        .map(|c| c.pr_url.clone())
        .filter(|url| seen.insert(url.clone()))
        .collect()
}

/// Run one full lifecycle sweep over every chore and project_task
/// the poller cares about (in_review with a PR, plus rows currently
/// blocked on merge_conflict so we can detect resolution, plus
/// `waiting_human` executions whose chore is still missing a
/// `pr_url`). Returns per-bucket counters so callers can log a
/// one-line summary.
///
/// `cube_client` is threaded into the conflict-watch retire path so
/// `on_resolved` can release the cube workspace lease the resolution
/// worker held (design Q5). Pass `None` for sweeps that don't need to
/// drive lease release — pre-Phase-3 wiring, tests, etc.
///
/// `completion_handler` is threaded in so the pending-PR-detection
/// recheck can reuse the on-Stop transition path
/// (`record_worker_pr_completion` + cube release + pane teardown + event
/// publish). Pass `None` for pre-`completion_handler` wiring and tests that
/// exercise only the in-review and conflict paths.
///
/// `remediation` is the bounded background runner conflict remediation is
/// handed to. It is what keeps this function a *detection* pass: with a
/// queue wired, observing a conflict records the attempt, publishes, and
/// enqueues — it never awaits the escalation ladder's cube lease + rebase +
/// push. Pass `None` (tests, pre-Phase wiring) to keep the pre-existing
/// inline-ladder behaviour.
pub async fn run_one_pass(
    work_db: &WorkDb,
    probe: &dyn MergeProbe,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
    completion_handler: Option<&WorkerCompletionHandler>,
    remediation: Option<&ConflictRemediationQueue>,
) -> SweepOutcome {
    run_one_pass_observed(work_db, probe, publisher, cube_client, completion_handler, remediation)
        .await
        .0
}

/// [`run_one_pass`], plus the per-PR [`PollTier`] its probe implies for
/// every candidate the pass considered.
///
/// The tiers are free: the pass already probed every one of these PRs, and
/// throwing the classification away is what forced the adaptive layer to
/// spend a probe of its own just to *discover* that a PR is cold. Handing
/// them to [`PrPollSchedule::apply_sweep_observations`] lets the schedule be
/// derived from the sweep rather than guessed and corrected.
///
/// The observation list covers every URL the pass *listed*, not only the
/// ones it walked: the batched probe is taken up front for the whole set, so
/// a pass abandoned mid-walk still classified them all. An entry the
/// snapshot no longer holds (a mid-pass re-probe replaces the results map
/// with just the unprocessed remainder) is reported as `None` — no adaptive
/// slot, and the next sweep re-observes it.
pub async fn run_one_pass_observed(
    work_db: &WorkDb,
    probe: &dyn MergeProbe,
    publisher: &dyn ExecutionPublisher,
    cube_client: Option<&dyn CubeClient>,
    completion_handler: Option<&WorkerCompletionHandler>,
    remediation: Option<&ConflictRemediationQueue>,
) -> (SweepOutcome, Vec<PollObservation>) {
    let in_review = match work_db.list_chores_pending_merge_check() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list pending merge checks");
            Vec::new()
        }
    };
    let blocked_conflict = match work_db.list_chores_blocked_on_merge_conflict() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list chores blocked on merge_conflict",);
            Vec::new()
        }
    };
    let blocked_ci = match work_db.list_chores_blocked_on_ci_failure() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list chores blocked on ci_failure",);
            Vec::new()
        }
    };
    let pending_pr_recheck = match work_db.list_executions_pending_pr_detection() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list executions pending PR detection",);
            Vec::new()
        }
    };
    // Stop-boundary nudges held for delegated background work must be
    // revisited by a recurring path: the worker may never emit another Stop.
    // The handler snapshots the exact suppressed intents so this also covers
    // revision executions with an already-bound PR, not just the no-PR query
    // above.
    let pending_background_nudges = completion_handler
        .map(WorkerCompletionHandler::pending_background_nudge_execution_ids)
        .unwrap_or_default();
    let stranded_ci_attempts = match work_db.list_stranded_ci_remediation_attempts() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list stranded ci remediation attempts",);
            Vec::new()
        }
    };
    let stranded_blocked = match work_db.list_chores_stranded_blocked_remediation() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(
                ?err,
                "merge poller: failed to list stranded blocked remediation parents",
            );
            Vec::new()
        }
    };
    // Late-PR candidates (Bug B recovery): terminal executions within
    // the last 60 min whose task is still `active` with no `pr_url`.
    // These arise from the double-spawn race where the orphan sweep
    // abandons exec_A while its pane is still running. The normal
    // `pending_pr_recheck` sweep (which only watches `waiting_human`)
    // cannot recover them; this sweep fills the gap.
    let late_pr_candidates: Vec<LatePrCandidate> = if completion_handler.is_some() {
        match work_db.list_recently_terminal_executions_pending_pr_detection(3600) {
            Ok(items) => items,
            Err(err) => {
                tracing::warn!(?err, "merge poller: failed to list late PR candidates",);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let total = in_review.len()
        + blocked_conflict.len()
        + blocked_ci.len()
        + pending_pr_recheck.len()
        + pending_background_nudges.len()
        + stranded_ci_attempts.len()
        + stranded_blocked.len()
        + late_pr_candidates.len();
    if total == 0 {
        // No candidates at all: an empty observation set, which retires every
        // remaining adaptive slot rather than leaving orphans behind.
        return (SweepOutcome::default(), Vec::new());
    }
    tracing::debug!(
        in_review = in_review.len(),
        blocked_conflict = blocked_conflict.len(),
        blocked_ci = blocked_ci.len(),
        pending_pr_recheck = pending_pr_recheck.len(),
        pending_background_nudges = pending_background_nudges.len(),
        stranded_ci_attempts = stranded_ci_attempts.len(),
        stranded_blocked = stranded_blocked.len(),
        late_pr_candidates = late_pr_candidates.len(),
        "merge poller: sweep started",
    );
    let mut outcome = SweepOutcome::default();
    // Batch every PR this pass will probe into one round trip
    // (`MergeProbe::probe_batch`) instead of one `gh pr view` per PR —
    // covers both the `sweep_one` candidates below and `stranded_blocked`,
    // since both loops probe the same way.
    let mut probe_url_seen = std::collections::HashSet::new();
    let probe_urls: Vec<String> = in_review
        .iter()
        .chain(blocked_conflict.iter())
        .chain(blocked_ci.iter())
        .chain(stranded_blocked.iter())
        .map(|candidate| candidate.pr_url.clone())
        .filter(|url| probe_url_seen.insert(url.clone()))
        .collect();
    let mut snapshot = ProbeSnapshot::new(probe.probe_batch(&probe_urls).await);
    // The pass's candidate walk, materialised so a mid-pass re-probe can be
    // scoped to the part of it that has not happened yet — see
    // [`ProbeSnapshot::ensure_fresh`].
    let sweep_walk: Vec<&PendingMergeCheck> = in_review
        .iter()
        .chain(blocked_conflict.iter())
        .chain(blocked_ci.iter())
        .collect();
    // De-duplicate by work_item_id: a chore that's both pending and
    // blocked-on-CI (shouldn't happen but defensive) only gets one
    // probe per sweep.
    let mut seen = std::collections::HashSet::new();
    for idx in 0..sweep_walk.len() {
        let candidate = sweep_walk[idx];
        if !seen.insert(candidate.work_item_id.clone()) {
            continue;
        }
        // Never transition off a snapshot that has gone stale under a
        // long-running pass — see [`ProbeSnapshot`].
        if !snapshot
            .ensure_fresh(probe, || {
                remaining_probe_urls(sweep_walk[idx..].iter().copied().chain(stranded_blocked.iter()))
            })
            .await
        {
            break;
        }
        sweep_one(
            work_db,
            &snapshot.results,
            publisher,
            (cube_client, completion_handler, remediation),
            candidate,
            &mut outcome,
        )
        .await;
    }
    if let Some(handler) = completion_handler {
        for execution_id in &pending_pr_recheck {
            sweep_pending_pr(handler, execution_id, &mut outcome).await;
        }
        for execution_id in &pending_background_nudges {
            if let Some(recheck_outcome) = handler.recheck_background_nudge(execution_id).await {
                tracing::info!(
                    execution_id,
                    outcome = ?recheck_outcome,
                    "merge poller: recurring background-child nudge recheck completed",
                );
            }
        }
    } else if !pending_pr_recheck.is_empty() {
        tracing::debug!(
            count = pending_pr_recheck.len(),
            "merge poller: pending PR-detection candidates skipped (no completion_handler wired)",
        );
    }
    // Rescue stranded ci_remediations attempts: `pending` rows with no live
    // execution. These arise when two dequeue events land in the same sweep —
    // the first flips the task (consuming the WHERE guard on
    // `mark_chore_blocked_ci_failure`) and the second inserts a ci_remediations
    // row but cannot create an execution. Re-emit a fresh execution so a worker
    // is dispatched without waiting for the task to return to `in_review`.
    for attempt in &stranded_ci_attempts {
        if ci_watch::rescue_stranded_ci_remediation_attempt(work_db, publisher, attempt).await {
            outcome.ci_remediation_redispatched += 1;
        }
    }
    // Reconcile stranded `blocked` parents (NULL scalar reason, empty
    // active-signal set, remediation-owned, bound PR). These fell out of
    // every scalar-reason-keyed candidate list above, so a still-dirty PR
    // would otherwise never be re-probed and no remediation revision would
    // ever (re)spawn — the invariant violation where a parent rests
    // `blocked` with no signal while its PR conflicts/fails. Re-probe each,
    // re-canonicalise a still-dirty row back into the standard
    // merge_conflict / ci_failure loop, and let the normal detection path
    // spawn a fresh revision.
    for idx in 0..stranded_blocked.len() {
        if !snapshot
            .ensure_fresh(probe, || remaining_probe_urls(stranded_blocked[idx..].iter()))
            .await
        {
            break;
        }
        sweep_stranded_blocked_remediation(
            work_db,
            &snapshot.results,
            publisher,
            &stranded_blocked[idx],
            &mut outcome,
        )
        .await;
    }
    // Late-PR sweep (Bug B): recover terminal executions whose pane
    // pushed a PR after the execution was marked abandoned.
    if let Some(handler) = completion_handler {
        for candidate in &late_pr_candidates {
            sweep_late_pr(handler, candidate, &mut outcome).await;
        }
    }
    // Merge-queue rebounce pass: for every `in_review` PR and every
    // `blocked: ci_failure` PR, poll the GitHub timeline for
    // `RemovedFromMergeQueueEvent` rows with `reason=FAILED_CHECKS`.
    // This is a separate pass from the probe loop above — the probe
    // covers per-PR CI and merge-conflict signals, while this pass
    // specifically looks for queue dequeues. Including `blocked_ci`
    // candidates ensures that a second dequeue (on a PR already blocked
    // by a prior dequeue) inserts a ci_remediations row so the stranded
    // rescue above can dispatch an execution for it.
    // The `INSERT OR IGNORE` idempotency on `ci_remediations` ensures
    // that events already processed on a prior sweep are no-ops.
    //
    // Every candidate's dequeue events are fetched in one batched GraphQL
    // round trip (`fetch_merge_queue_dequeue_events_batch`) instead of one
    // `gh api graphql` call per candidate — this was the dominant
    // per-pass request volume behind the hourly GitHub quota exhaustion
    // (O(open rows) unbatched requests, every 60s, on top of everything
    // else sharing the token).
    // A `trunk_queue`-mechanism product's dequeue/eviction signal is owned
    // by the Trunk queue poller, not GitHub's merge-queue timeline — its
    // PRs never actually sit in GitHub's native queue (see
    // `is_trunk_queue_product`), so a `RemovedFromMergeQueueEvent` lookup
    // here would either find nothing or, worse, race the Trunk-owned
    // coordination state with a GitHub-side signal that was never armed
    // for this product.
    run_merge_queue_rebounce_pass(work_db, publisher, &in_review, &blocked_ci, &mut outcome).await;

    // reviewer-fallback sweep: tasks held in `active` (PendingReview)
    // while waiting for an AI reviewer pass that has since finished or timed
    // out. A pass that recorded a ReviewResult may advance to Review; a pass
    // that produced no result is re-fired and stays in Doing. Timeout: 10
    // minutes — long enough for the reviewer to complete normally, short
    // enough to surface and recover a wedged pass promptly.
    let reviewer_stale_secs: u64 = 10 * 60;
    let stalled_candidates = match work_db.list_tasks_with_stalled_reviewer(reviewer_stale_secs) {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list stalled reviewer tasks");
            Vec::new()
        }
    };
    for (task_id, product_id, pr_url, review_result_written) in &stalled_candidates {
        sweep_stalled_reviewer(
            work_db,
            publisher,
            StalledReview {
                task_id,
                product_id,
                pr_url,
                review_result_written: *review_result_written,
            },
            &GhPrStateChecker,
            &mut outcome,
        )
        .await;
    }

    let observations = poll_observations(&probe_urls, &snapshot.results);
    (outcome, observations)
}

/// Classify each probed URL into the tier that should drive its adaptive
/// polling. Shared by the full sweep and the batched adaptive reconcile so
/// both feed [`PrPollSchedule`] through identical rules: a URL whose probe
/// failed, or which the snapshot no longer holds, yields `None` (no adaptive
/// slot — the full sweep re-probes it).
pub(crate) fn poll_observations(
    pr_urls: &[String],
    results: &HashMap<String, std::result::Result<PrLifecycleProbe, String>>,
) -> Vec<PollObservation> {
    pr_urls
        .iter()
        .map(|url| {
            let tier = results
                .get(url)
                .and_then(|r| r.as_ref().ok())
                .and_then(poll_tier_for_probe);
            (url.clone(), tier)
        })
        .collect()
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
    completion_handler: Option<&WorkerCompletionHandler>,
    remediation: Option<&ConflictRemediationQueue>,
    pr_url: &str,
) -> (SweepOutcome, Option<PollTier>) {
    let urls = [pr_url.to_owned()];
    let (outcome, observations) = reconcile_batch(
        work_db,
        probe,
        publisher,
        cube_client,
        completion_handler,
        remediation,
        &urls,
    )
    .await;
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
    completion_handler: Option<&WorkerCompletionHandler>,
    remediation: Option<&ConflictRemediationQueue>,
    pr_urls: &[String],
) -> (SweepOutcome, Vec<PollObservation>) {
    let mut outcome = SweepOutcome::default();
    let wanted: std::collections::HashSet<&str> = pr_urls.iter().map(String::as_str).collect();
    if wanted.is_empty() {
        return (outcome, Vec::new());
    }

    let in_review = candidates_for_pr_urls(work_db.list_chores_pending_merge_check(), &wanted);
    let blocked_conflict = candidates_for_pr_urls(work_db.list_chores_blocked_on_merge_conflict(), &wanted);
    let blocked_ci = candidates_for_pr_urls(work_db.list_chores_blocked_on_ci_failure(), &wanted);
    let stranded_blocked = candidates_for_pr_urls(work_db.list_chores_stranded_blocked_remediation(), &wanted);

    // Probe only the requested URLs that still have a live candidate row.
    // A URL that has dropped off every list costs nothing here and is
    // reported as "no tier", which retires its adaptive slot.
    let mut probe_seen = std::collections::HashSet::new();
    let probe_urls: Vec<String> = in_review
        .iter()
        .chain(blocked_conflict.iter())
        .chain(blocked_ci.iter())
        .chain(stranded_blocked.iter())
        .map(|candidate| candidate.pr_url.clone())
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
    let probe_results = probe.probe_batch(&probe_urls).await;

    let mut seen = std::collections::HashSet::new();
    for candidate in in_review.iter().chain(blocked_conflict.iter()).chain(blocked_ci.iter()) {
        if !seen.insert(candidate.work_item_id.clone()) {
            continue;
        }
        sweep_one(
            work_db,
            &probe_results,
            publisher,
            (cube_client, completion_handler, remediation),
            candidate,
            &mut outcome,
        )
        .await;
    }
    for candidate in &stranded_blocked {
        sweep_stranded_blocked_remediation(work_db, &probe_results, publisher, candidate, &mut outcome).await;
    }
    // See the matching gate in `run_one_pass`: a `trunk_queue` product's
    // dequeue/eviction signal is owned by the Trunk queue poller, not
    // GitHub's merge-queue timeline. One batched dequeue query for the whole
    // due set, exactly like the full sweep's.
    run_merge_queue_rebounce_pass(work_db, publisher, &in_review, &blocked_ci, &mut outcome).await;

    let reviewer_stale_secs: u64 = 10 * 60;
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
                    },
                    &GhPrStateChecker,
                    &mut outcome,
                )
                .await;
            }
        }
        Err(err) => tracing::warn!(?err, "merge poller: failed to list stalled reviewer tasks"),
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
/// The DB transaction in `mark_chore_pr_merged` already blocked the
/// revision tasks (via `block_pending_revisions_on_parent_close`).  This
/// function handles the execution side: force-release each cube workspace
/// lease so the slot is freed, then cancel the execution row so the
/// dispatcher treats it as terminal.
///
/// When `completion_handler` is `None` (tests, cold-path wiring) this
/// function is a no-op; the tasks are already blocked in the DB, and the
/// scheduler will not redispatch them on the next reconcile cycle.
pub(crate) async fn stop_active_revision_executions(
    work_db: &WorkDb,
    completion_handler: Option<&WorkerCompletionHandler>,
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
        // Release the pane and cube workspace lease without altering
        // execution status (force_release does not change status).
        handler.force_release(&execution.id).await;
        // Now mark the execution terminal so the dispatcher won't try to
        // re-schedule it.  `cancel_execution` resets task status to `todo`
        // only when it's currently `active`; since the task is already
        // `blocked` (set in the DB transaction), that guard won't fire.
        match work_db.cancel_execution(&execution.id) {
            Ok(_) => {
                outcome.revision_invalidated += 1;
            }
            Err(err) => {
                // The execution may have already moved to a terminal state
                // (raced with the worker finishing, or a prior sweep).
                // Log at debug — not a concern since the lease is released.
                tracing::debug!(
                    execution_id = %execution.id,
                    ?err,
                    "merge poller: cancel_execution failed for revision (may already be terminal)",
                );
            }
        }
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
/// [`WorkerCompletionHandler::force_stop_execution`] only demotes a task
/// that is still `active`; since the task is now `in_review`, that guard
/// does not fire and the task stays in Review. Idempotent: a no-op when
/// no live execution exists or `completion_handler` is `None` (tests /
/// cold-path wiring).
pub(crate) async fn stop_worker_on_review_transition(
    work_db: &WorkDb,
    completion_handler: Option<&WorkerCompletionHandler>,
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
/// [`WorkerCompletionHandler::recheck_for_pr`], which transitions the
/// chore on `Fresh`/`Merged` and stays quiet on the no-PR / stale-PR
/// branches so the poller doesn't spam probes or awaiting-input events.
pub(crate) async fn sweep_pending_pr(
    handler: &WorkerCompletionHandler,
    execution_id: &str,
    outcome: &mut SweepOutcome,
) {
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
        // FlakyRetriggered is only reachable via the on-Stop path (it gates
        // on `execution.kind == "ci_remediation"`), never from a recheck.
        | StopOutcome::FlakyRetriggered { .. }
        // Maint task 6: an automation_triage outcome only comes from the
        // on-Stop detector, never from a PR-detection recheck.
        | StopOutcome::AutomationTriage { .. }
        // P3b: an answer_agent outcome only comes from the on-Stop
        // finalizer, never from a PR-detection recheck.
        | StopOutcome::AnswerAgent { .. }
        // ReviewerEnqueued is handled in its own arm above.
        // ReviewPassCompleted/ReviewPassRevisionCreated/ReviewPassAwaitingResult
        // only come from on-Stop (reviewer finalisation).
        | StopOutcome::ReviewPassCompleted { .. }
        | StopOutcome::ReviewPassRevisionCreated { .. }
        | StopOutcome::ReviewPassAwaitingResult
        // The no-op terminal is only reachable on the on-Stop path
        // (it reads the worker's transcript for the NO_CHANGES_NEEDED marker),
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
        // DriverTerminalError is only reachable via `on_stop_inner`'s early
        // gate on the driver-supplied TurnEnd, which `recheck_for_pr` never
        // receives (it has no live Stop event to read a reason from) —
        // covered for exhaustiveness.
        | StopOutcome::DriverTerminalError { .. }
        | StopOutcome::DbError
        // DeferredForProbeTurn is only reachable via `on_stop_inner`'s
        // probe-delivery gate on the on-Stop path (it depends on this same
        // boundary's pre-completion probe pass), never from a PR-detection
        // recheck — covered for exhaustiveness.
        | StopOutcome::DeferredForProbeTurn => {}
    }
}

/// Run late-PR detection against a terminal execution (abandoned /
/// completed / failed within the recent lookback window) whose task is
/// still `active` with no `pr_url`. Delegates to
/// [`WorkerCompletionHandler::recheck_for_pr_late`], which bypasses the
/// `AlreadyTerminal` gate and calls
/// [`WorkDb::bind_pr_to_active_task_from_terminal_execution`] directly
/// on a positive detection result.
pub(crate) async fn sweep_late_pr(
    handler: &WorkerCompletionHandler,
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
        // FlakyRetriggered is only reachable via the on-Stop path (it gates
        // on `execution.kind == "ci_remediation"`), never from a recheck.
        | StopOutcome::FlakyRetriggered { .. }
        // Maint task 6: an automation_triage outcome only comes from the
        // on-Stop detector, never from a PR-detection recheck.
        | StopOutcome::AutomationTriage { .. }
        // P3b: an answer_agent outcome only comes from the on-Stop
        // finalizer, never from a late-PR recheck.
        | StopOutcome::AnswerAgent { .. }
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
        // DriverTerminalError is only reachable via `on_stop_inner`'s early
        // gate on the driver-supplied TurnEnd, never from a late-PR recheck.
        | StopOutcome::DriverTerminalError { .. }
        | StopOutcome::DbError
        // DeferredForProbeTurn is only reachable via `on_stop_inner`'s
        // probe-delivery gate on the on-Stop path, never from a late-PR
        // recheck — covered for exhaustiveness.
        | StopOutcome::DeferredForProbeTurn => {}
    }
}

pub(crate) async fn sweep_one(
    work_db: &WorkDb,
    probe_results: &HashMap<String, std::result::Result<PrLifecycleProbe, String>>,
    publisher: &dyn ExecutionPublisher,
    // (cube_client, completion_handler, remediation) — bundled to keep the
    // parameter count under clippy::too_many_arguments.
    handlers: (
        Option<&dyn CubeClient>,
        Option<&WorkerCompletionHandler>,
        Option<&ConflictRemediationQueue>,
    ),
    candidate: &PendingMergeCheck,
    outcome: &mut SweepOutcome,
) {
    let (cube_client, completion_handler, remediation) = handlers;
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
    // Populated in the Open arm so the post-match poll-state write can
    // reuse the reaper's active-attempt snapshot instead of re-querying.
    // `None` outside Open (or if the reaper was not run); `Some(…)` is the
    // still-active merge-queue remediation after the reaper (inner `None`
    // when none remains).
    let mut known_active_queue_failure: Option<Option<CiRemediation>> = None;
    match &probe_result.state {
        PrLifecycleState::Merged => {
            if mark_merged(work_db, publisher, completion_handler, candidate, &probe_result).await {
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
            stop_active_revision_executions(work_db, completion_handler, &candidate.work_item_id, outcome).await;
        }
        PrLifecycleState::Open(open) => {
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
                    // Escalation ladder: enabled only when the
                    // `conflict_ladder_mechanical_rebase` flag is on. Off (or
                    // no completion handler) → `Disabled` preserves the
                    // worker-only path exactly.
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
                    let ladder_enabled = completion_handler
                        .map(|h| h.mechanical_rebase_enabled())
                        .unwrap_or(false);
                    let mode = match (ladder_enabled, remediation, cube_client) {
                        (false, _, _) => conflict_watch::ConflictRemediationMode::Disabled,
                        (true, Some(queue), _) => conflict_watch::ConflictRemediationMode::Deferred(queue),
                        // No remediator wired (tests, pre-Phase wiring): fall
                        // back to the pre-existing inline behaviour rather
                        // than silently dropping the ladder.
                        (true, None, Some(cube)) => conflict_watch::ConflictRemediationMode::Inline(cube),
                        (true, None, None) => conflict_watch::ConflictRemediationMode::Disabled,
                    };
                    if conflict_watch::on_conflict_detected_with(
                        work_db,
                        publisher,
                        mode,
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
                        (cube_client, completion_handler),
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
                        (cube_client, completion_handler),
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
        }
    }
    // For every open (or just-probed) PR, persist the CI + review poll
    // state so the macOS kanban can render indicators with tooltips.
    // We do this unconditionally after the lifecycle routing above so
    // the columns stay fresh even when no status transition fired.
    // Merged / closed-unmerged probes are skipped — the row will
    // transition away from `in_review` and the indicators become moot.
    if matches!(probe_result.state, PrLifecycleState::Open(_)) {
        update_pr_poll_state(
            work_db,
            publisher,
            candidate,
            &probe_result,
            known_active_queue_failure.as_ref().map(|inner| inner.as_ref()),
        )
        .await;
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

/// Shared body of [`sweep_stranded_blocked_remediation`]: re-canonicalise
/// the NULL-reason blocked parent into `kind`'s canonical blocked state and
/// drive the matching detection path so a revision (re)spawns.
pub(crate) async fn reconcile_stranded(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
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
            if conflict_watch::on_conflict_detected(work_db, publisher, None, &checker, candidate, probe_result).await {
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
pub(crate) async fn maybe_clear_blocked(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    // (cube_client, completion_handler) — bundled to keep the parameter
    // count under clippy::too_many_arguments.
    handlers: (Option<&dyn CubeClient>, Option<&WorkerCompletionHandler>),
    candidate: &PendingMergeCheck,
    // Supplies `labels`, `raw_mergeable`, `raw_merge_state_status`.
    probe_result: &PrLifecycleProbe,
    // (ci, mergeability_clean) — bundled to keep the parameter count
    // under clippy::too_many_arguments.
    ci_status: (&OpenPrCiStatus, bool),
    outcome: &mut SweepOutcome,
) {
    let (cube_client, completion_handler) = handlers;
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
                if conflict_watch::on_resolved(
                    work_db,
                    publisher,
                    cube_client,
                    candidate,
                    labels,
                    raw_mergeable,
                    raw_merge_state_status,
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
pub(crate) fn mergeable_state_str(mergeability: OpenPrMergeability) -> &'static str {
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

/// Persist CI + review + merge-queue poll state and emit a change event
/// when any field flips value. Called from `sweep_one` for every open PR and
/// from `completion.rs` after the on-transition initial CI fetch.
///
/// `known_active_queue_failure` lets the sweep reaper hand off its already-
/// loaded merge-queue remediation (or `Some(None)` when it just retired one)
/// so this function does not open a second sqlite connection for the same
/// work item on the same pass. Pass `None` when the caller did not run the
/// reaper — this function self-fetches in that case (e.g. `completion.rs`).
pub(crate) async fn update_pr_poll_state(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
    known_active_queue_failure: Option<Option<&CiRemediation>>,
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
    let preserve_merge_queue_state = is_trunk_queue_product(work_db, &candidate.product_id);

    let outcome = match work_db.update_task_pr_poll_state(
        &candidate.work_item_id,
        PrPollStateInput {
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
        publisher
            .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "pr_poll_state_updated")
            .await;
        tracing::debug!(
            work_item_id = %candidate.work_item_id,
            ci_state,
            review_state,
            in_merge_queue = probe.in_merge_queue,
            "merge poller: PR poll state changed",
        );
    }

    // Lane-demotion trace (mono#2023): one log line each way so a
    // card bouncing between Merging and Review because CI is flapping
    // (fail → success → fail) is diagnosable from the trace, mirroring the
    // existing blocked↔in_review flap logging convention used elsewhere in
    // this file and in ci_watch.rs.
    if outcome.prior_merge_queue_state.as_deref() != merge_queue_state {
        if merge_queue_state.is_none() && ci_state == "fail" && raw_merge_queue_state.is_some() {
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                prior_merge_queue_state = outcome.prior_merge_queue_state.as_deref(),
                raw_merge_queue_state,
                "merge poller: demoting card from Merging back to Review; required CI is failing while GitHub auto-merge/queue is still armed",
            );
        } else if merge_queue_state.is_some() && outcome.prior_merge_queue_state.is_none() {
            tracing::info!(
                work_item_id = %candidate.work_item_id,
                pr_url = %candidate.pr_url,
                merge_queue_state,
                ci_state,
                "merge poller: card entering Merging section; auto-merge/queue armed and required CI not failing",
            );
        }
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
    // `CiFailureCleared` here as a head-keyed safety net. This is idempotent
    // with any retire-path clear that already ran this sweep: the macOS
    // handler simply drops the chip (a no-op if already gone) and leaves the
    // "ci auto-fixed" badge untouched. Restricted to `fail → success` so it
    // never clobbers an active attempt's in-flight badge on `fail → in_progress`
    // (that transition is owned by `on_ci_in_flight_supersedes_failure`, #901).
    if outcome.prior_ci_state.as_deref() == Some("fail") && ci_state == "success" {
        publisher
            .publish_frontend_event_on_product(
                &candidate.product_id,
                boss_protocol::FrontendEvent::CiFailureCleared {
                    product_id: candidate.product_id.clone(),
                    work_item_id: candidate.work_item_id.clone(),
                    pr_url: candidate.pr_url.clone(),
                },
            )
            .await;
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

pub(crate) async fn mark_merged(
    work_db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    _completion_handler: Option<&WorkerCompletionHandler>,
    candidate: &PendingMergeCheck,
    probe: &PrLifecycleProbe,
) -> bool {
    let updated = match work_db.mark_chore_pr_merged(&candidate.work_item_id, &candidate.pr_url) {
        Ok(Some(task)) => task,
        Ok(None) => return false,
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
    design_detector::on_task_doc_pr_merged(
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
        design_detector::on_design_pr_merged(
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
            crate::populator::enqueue_from_merge(
                work_db,
                crate::populator::PopulateContext {
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

/// Extract the PR number from a GitHub PR URL as an `i64` for DB storage.
///
/// Re-exported from the persistence layer, which owns the stored `i64`
/// representation; it in turn is a thin adaptor over the canonical
/// [`boss_github::pr_url::pr_number_from_url`] helper (including tolerance
/// for the `/pull/<N>/files`, `?query`, and `#fragment` decorations).
/// Returns `None` for any non-canonical URL.
pub(crate) use crate::work::stored_pr_number;

/// Argument bundle for [`sweep_stalled_reviewer`]; `review_result_written`
/// says the latest review execution recorded an informative `ReviewResult`.
pub(crate) struct StalledReview<'a> {
    pub(crate) task_id: &'a str,
    pub(crate) product_id: &'a str,
    pub(crate) pr_url: &'a str,
    pub(crate) review_result_written: bool,
}

/// Reviewer-fallback: reconcile a task held in `active` after its latest AI
/// reviewer pass either finished without advancing it (missed Stop hook) or
/// ran past the stale threshold. A pass with a durable `ReviewResult` may
/// advance to `in_review`; a pass without one stays in Doing and is re-fired.
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
pub(crate) async fn sweep_stalled_reviewer(
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
    } = stalled;
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
                kind: crate::pr_review_recovery::PR_REVIEW_DIED_ATTENTION_KIND.to_owned(),
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
