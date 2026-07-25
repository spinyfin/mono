use super::*;

/// Adaptive-polling urgency tier for a single PR (doc `github-event-
/// detection-webhooks-vs-polling-2026-07-08.md` §9 item 3: "adaptive
/// per-PR poll intervals driven by task status, replacing the single
/// 60s global tick"). Derived straight from a PR's own last-observed
/// lifecycle signals — no extra GitHub round trip needed, since
/// [`reconcile_one`] already probed the PR to detect merges, conflicts,
/// and CI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollTier {
    /// Something is actively moving — CI still running, the PR is
    /// merge-queued, or mergeability is conflicting/still being
    /// recomputed. Check back soon.
    Hot,
    /// Steady state: CI has reached a terminal result and the PR merges
    /// cleanly. Nothing GitHub-side is expected to change without a
    /// human action (approving, pushing a fix), so back off.
    Cold,
}

impl PollTier {
    /// How long to wait before reconciling this PR again. Stretched by
    /// [`rate_limit_throttle_factor`] when the hourly GitHub quota is
    /// running low, so hot PRs back off from their normal 40s cadence
    /// right alongside the full sweep instead of being the adaptive
    /// layer that keeps draining an already-low budget.
    ///
    /// Hot was 15s; at ~47 open PRs each hot cycle re-probes the whole set,
    /// so a 15s cadence was a structural driver of the personal-token
    /// GraphQL exhaustion. 40s still catches CI/merge-queue transitions
    /// promptly (those settle over minutes, not seconds) while cutting the
    /// hot re-probe rate by ~2.7x.
    pub fn interval(self) -> Duration {
        let base = match self {
            PollTier::Hot => Duration::from_secs(40),
            PollTier::Cold => Duration::from_secs(180),
        };
        let throttle = rate_limit_throttle_factor();
        if throttle > 1.0 { base.mul_f64(throttle) } else { base }
    }
}

/// Classify a probed PR's [`PollTier`] from its lifecycle state, or `None`
/// when the PR has reached a terminal state and should be dropped from the
/// adaptive schedule entirely rather than re-probed.
///
/// A `Merged` / `ClosedUnmerged` PR has already been transitioned out of
/// every candidate list by the sweep that observed it, so a further
/// adaptive probe spends GraphQL quota to re-confirm a fact that can no
/// longer change. Returning `None` lets [`PrPollSchedule::reschedule`] stop
/// tracking it immediately (the periodic full sweep remains the backstop if
/// anything ever needs re-discovery), so terminal PRs cost zero between
/// sweeps instead of one trailing Cold probe apiece.
pub(crate) fn poll_tier_for_probe(probe: &PrLifecycleProbe) -> Option<PollTier> {
    match &probe.state {
        PrLifecycleState::Open(open) => {
            if probe.in_merge_queue
                || open.mergeability != OpenPrMergeability::Clean
                || matches!(open.ci, OpenPrCiStatus::InFlight)
            {
                Some(PollTier::Hot)
            } else {
                Some(PollTier::Cold)
            }
        }
        PrLifecycleState::Merged | PrLifecycleState::ClosedUnmerged => None,
    }
}

/// Closed set of conclusion strings that count as "failure" for the
/// required-check predicate (design §Q1). `ACTION_REQUIRED` is a
/// special case: the worker can't approve manual workflows, so we
/// surface it as a failure but the engine's pre-triage immediately
/// flags it `manual_action_required` (design §Q4). `ERROR` is the
/// legacy-commit-status equivalent of `FAILURE` (StatusContext leaves
/// — see [`normalize_leaf`]) and lands in the same bucket.
pub(crate) fn is_failure_conclusion(c: &str) -> bool {
    matches!(
        c.to_ascii_uppercase().as_str(),
        "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "STARTUP_FAILURE" | "ACTION_REQUIRED" | "STALE"
    )
}

/// Closed set of conclusion strings that count as "successful enough
/// to ignore" for the required-check predicate. `NEUTRAL` and
/// `SKIPPED` do not gate merge per branch protection; `SUCCESS` is
/// the happy path.
pub(crate) fn is_pass_conclusion(c: &str) -> bool {
    matches!(c.to_ascii_uppercase().as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED",)
}

/// A "no PR is due" placeholder wait — long enough to never race the
/// periodic full-sweep interval or a kick, short enough to stay well
/// under `tokio::time::sleep`'s max duration.
pub(crate) const NO_PR_DUE_WAIT: Duration = Duration::from_secs(60 * 60 * 24 * 365);

/// In-memory next-poll-time tracker driving the per-PR adaptive interval
/// (doc `github-event-detection-webhooks-vs-polling-2026-07-08.md` §9
/// item 3), replacing the single global tick with a per-PR schedule:
/// hot PRs (CI running, merge-queued) get reconciled on a short cadence
/// while cold ones (steady-state, awaiting a human) back off.
///
/// Purely in-memory and best-effort — it is reseeded from the DB's own
/// candidate lists after every periodic full sweep (see [`spawn_loop`]),
/// which remains the correctness backstop, so a dropped, evicted, or
/// (after a restart) forgotten entry only means the PR is picked up on
/// the next full sweep — never lost.
#[derive(Default)]
pub(crate) struct PrPollSchedule {
    next_poll_at: HashMap<String, Instant>,
}

impl PrPollSchedule {
    /// Earliest scheduled poll across every tracked PR, if any.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.next_poll_at.values().min().copied()
    }

    /// Remove and return every PR whose scheduled poll has arrived.
    pub(crate) fn drain_due(&mut self, now: Instant) -> Vec<String> {
        let due: Vec<String> = self
            .next_poll_at
            .iter()
            .filter(|&(_, &at)| at <= now)
            .map(|(url, _)| url.clone())
            .collect();
        for url in &due {
            self.next_poll_at.remove(url);
        }
        due
    }

    /// Record the tier observed for `pr_url`, scheduling its next poll.
    /// `None` (the PR dropped out of every candidate list) stops tracking
    /// it until a full sweep rediscovers it.
    pub(crate) fn reschedule(&mut self, pr_url: &str, tier: Option<PollTier>, now: Instant) {
        match tier {
            Some(tier) => {
                self.next_poll_at.insert(pr_url.to_owned(), now + tier.interval());
            }
            None => {
                self.next_poll_at.remove(pr_url);
            }
        }
    }

    /// Seed a default (hot) entry for every PR in `pr_urls` that isn't
    /// already tracked — called after a full sweep so newly-discovered
    /// PRs get an adaptive slot immediately instead of waiting for
    /// another full-sweep interval. Existing entries are left alone: a
    /// fresh default must not clobber a tier already learned from a real
    /// probe via [`reconcile_one`].
    pub(crate) fn seed_defaults(&mut self, pr_urls: impl IntoIterator<Item = String>, now: Instant) {
        for url in pr_urls {
            self.next_poll_at
                .entry(url)
                .or_insert_with(|| now + PollTier::Hot.interval());
        }
    }
}

/// Distinct PR urls across every per-PR candidate list [`run_one_pass`]
/// considers — a cheap, GitHub-call-free local DB read used only to seed
/// [`PrPollSchedule`] after a full sweep.
pub(crate) fn current_pr_candidate_urls(work_db: &WorkDb) -> Vec<String> {
    let mut urls = std::collections::HashSet::new();
    let lists = [
        work_db.list_chores_pending_merge_check(),
        work_db.list_chores_blocked_on_merge_conflict(),
        work_db.list_chores_blocked_on_ci_failure(),
        work_db.list_chores_stranded_blocked_remediation(),
    ];
    for list in lists {
        match list {
            Ok(items) => urls.extend(items.into_iter().map(|c| c.pr_url)),
            Err(err) => tracing::warn!(
                ?err,
                "merge poller: failed to list candidates while seeding poll schedule"
            ),
        }
    }
    urls.into_iter().collect()
}

/// Multiple of the configured sweep interval after which a detection pass
/// is abandoned mid-flight.
///
/// A pass is *supposed* to be bounded by GitHub round trips: one batched
/// probe, then local DB work. Nothing in it should ever approach three
/// whole sweep cadences. When it does, something is awaiting work that
/// does not belong on the detection path (before this fix, a cube lease +
/// rebase + push per conflicting PR, serially), and the right response is
/// to cut the pass loose and let the next one re-list from the DB — every
/// candidate list is rebuilt from scratch each pass, so nothing is lost.
/// Three rather than one so a genuinely slow-but-progressing pass (a large
/// candidate set behind a sluggish GitHub) still completes.
pub(crate) const PASS_TIMEOUT_MULTIPLIER: u32 = 3;

/// Floor for the pass budget, so a short configured interval (tests, a
/// tuned-down cadence) can't produce a budget a normal pass would trip.
pub(crate) const MIN_PASS_TIMEOUT: Duration = Duration::from_secs(120);

/// Budget for a single targeted [`reconcile_one`]. Much tighter than a full
/// pass: it probes exactly one PR. It runs inside the wait `select!`, so
/// time spent here is also time the trunk observer, the activation kick,
/// and every other PR's adaptive timer are not being serviced.
pub(crate) const RECONCILE_ONE_TIMEOUT: Duration = Duration::from_secs(60);

/// Hard time budget for one full detection pass at `interval` cadence.
pub(crate) fn pass_budget(interval: Duration) -> Duration {
    interval.saturating_mul(PASS_TIMEOUT_MULTIPLIER).max(MIN_PASS_TIMEOUT)
}

/// Run one detection pass under a hard time budget, and make an overrun
/// loud either way.
///
/// Returns `None` when the budget was exceeded and the pass was dropped
/// mid-flight. Dropping the future cancels it at its next await point; each
/// state write inside a pass is its own synchronous DB transaction, so a
/// cancelled pass leaves no half-written transition — just unprocessed
/// candidates, which the next pass re-lists.
///
/// The silent version of this was the whole problem: a pass that ran 32
/// minutes over its 60-second cadence produced no log line at all, so the
/// only evidence of the blackout was the *absence* of `merge_poller` output
/// in the trace. A pass that exceeds its own cadence must be loud.
pub(crate) async fn run_pass_within_budget<F>(
    label: &'static str,
    budget: Duration,
    cadence: Duration,
    metrics: &Registry,
    pass: F,
) -> Option<SweepOutcome>
where
    F: std::future::Future<Output = SweepOutcome>,
{
    // `tokio::time::Instant` rather than `std::time::Instant`: identical in
    // production, but it tracks the same clock as the `timeout` below, so
    // the overrun accounting is testable under a paused runtime clock.
    let started = tokio::time::Instant::now();
    match tokio::time::timeout(budget, pass).await {
        Ok(outcome) => {
            let elapsed = started.elapsed();
            if elapsed > cadence {
                PASS_OVERRUN.inc_by(metrics, 1);
                tracing::warn!(
                    label,
                    elapsed_ms = elapsed.as_millis(),
                    cadence_ms = cadence.as_millis(),
                    budget_ms = budget.as_millis(),
                    "merge poller: detection pass took longer than its own cadence — every tracked PR's \
                     lifecycle went unobserved for that whole window",
                );
            }
            Some(outcome)
        }
        Err(_) => {
            PASS_TIMED_OUT.inc_by(metrics, 1);
            PASS_OVERRUN.inc_by(metrics, 1);
            tracing::warn!(
                label,
                budget_ms = budget.as_millis(),
                cadence_ms = cadence.as_millis(),
                "merge poller: detection pass exceeded its time budget and was abandoned mid-flight; \
                 remaining candidates are re-listed by the next pass",
            );
            None
        }
    }
}

/// [`run_pass_within_budget`]'s counterpart for the targeted per-PR
/// reconcile, which returns a tier alongside its outcome. `None` means the
/// reconcile blew [`RECONCILE_ONE_TIMEOUT`] and was dropped.
pub(crate) async fn reconcile_one_within_budget<F>(
    metrics: &Registry,
    reconcile: F,
    pr_url: &str,
) -> Option<(SweepOutcome, Option<PollTier>)>
where
    F: std::future::Future<Output = (SweepOutcome, Option<PollTier>)>,
{
    match tokio::time::timeout(RECONCILE_ONE_TIMEOUT, reconcile).await {
        Ok(result) => Some(result),
        Err(_) => {
            PASS_TIMED_OUT.inc_by(metrics, 1);
            PASS_OVERRUN.inc_by(metrics, 1);
            tracing::warn!(
                pr_url,
                budget_ms = RECONCILE_ONE_TIMEOUT.as_millis(),
                "merge poller: targeted per-PR reconcile exceeded its time budget and was abandoned; \
                 the periodic full sweep remains the backstop for this PR",
            );
            None
        }
    }
}

/// Increment every per-sweep metric from `outcome`. Shared by the
/// periodic full sweep and the targeted [`reconcile_one`] paths in
/// [`spawn_loop`] so adaptive/targeted transitions are counted exactly
/// like full-sweep ones.
pub(crate) fn record_sweep_metrics(metrics: &Registry, outcome: &SweepOutcome) {
    MERGED.inc_by(metrics, outcome.merged as u64);
    CONFLICT_FLAGGED.inc_by(metrics, outcome.conflict_flagged as u64);
    CONFLICT_CLEARED.inc_by(metrics, outcome.conflict_cleared as u64);
    PR_RECHECK_RECOVERED.inc_by(metrics, outcome.pr_recheck_recovered as u64);
    PR_RECHECK_UNRESOLVED.inc_by(metrics, outcome.pr_recheck_unresolved as u64);
    MERGE_QUEUE_REBOUNCED.inc_by(metrics, outcome.merge_queue_rebounced as u64);
    LATE_PR_RECOVERED.inc_by(metrics, outcome.late_pr_recovered as u64);
    REVISION_INVALIDATED.inc_by(metrics, outcome.revision_invalidated as u64);
    WORKER_STOPPED_ON_REVIEW.inc_by(metrics, outcome.worker_stopped_on_review as u64);
    COMMENTS_REOPENED.inc_by(metrics, outcome.comments_reopened as u64);
}

/// merged or developed a conflict while the engine was offline gets
/// reconciled on boot. The sweep runs inside the spawned task so
/// engine startup isn't blocked on `gh`; subsequent full sweeps are
/// gated behind `interval`, which remains the correctness backstop
/// (doc `github-event-detection-webhooks-vs-polling-2026-07-08.md` §8):
/// it re-discovers any PR the adaptive/targeted paths below missed.
///
/// Between full sweeps, an in-memory [`PrPollSchedule`] drives a
/// per-PR adaptive timer (doc §9 item 3) that calls [`reconcile_one`]
/// on just the PR that's due, instead of every PR sharing the single
/// `interval` tick — hot PRs (CI running, merge-queued) get reconciled
/// on a short cadence, cold ones back off. The schedule is reseeded
/// with a default entry for every newly-discovered PR after each full
/// sweep.
///
/// `kick` is a shared [`Notify`] the caller can fire (via
/// [`Notify::notify_one`]) to request an immediate out-of-band full
/// sweep. Kicks received within the 15 s quiesce window after the most
/// recent full sweep are silently dropped — the periodic tick will pick
/// up the change soon enough and rapid window-toggle events don't
/// result in repeated GitHub API calls.
///
/// `pr_reconcile_requests` is the keyed companion to the broad `kick`:
/// a subscription to the [`Event::PrReconcileRequested`] topic. Same
/// quiesce window as the broad kick, but each received event reconciles
/// just its named PR via [`reconcile_one`] rather than triggering a full
/// sweep.
///
/// The Trunk merge-queue observer
/// ([`crate::trunk_queue_poller::TrunkQueueProbe`]) rides this same loop
/// rather than running free: it gets its own arm in the wait `select!`,
/// driven by its own cadence tiers, so it inherits this task's lifetime
/// and publisher plumbing while keeping its 15 s/30 s cadence independent
/// of the 60 s full-sweep tick.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    probe: Arc<dyn MergeProbe>,
    publisher: Arc<dyn ExecutionPublisher>,
    // (cube_client, completion_handler, trunk_queue_api) — bundled to keep
    // the parameter count under clippy::too_many_arguments.
    handlers: (
        Arc<dyn CubeClient>,
        Arc<WorkerCompletionHandler>,
        Arc<dyn crate::trunk_queue_poller::TrunkQueueApi>,
    ),
    interval: Duration,
    metrics: Arc<Registry>,
    // (broad kick, keyed PrReconcileRequested subscription) — bundled to
    // keep the parameter count under clippy::too_many_arguments.
    kicks: (Arc<Notify>, boss_event_bus::Subscription),
) -> tokio::task::JoinHandle<()> {
    let (cube_client, completion_handler, trunk_queue_api) = handlers;
    let (kick, mut pr_reconcile_requests) = kicks;
    tokio::spawn(async move {
        let quiesce_window = Duration::from_secs(15);
        let mut schedule = PrPollSchedule::default();
        let mut pr_requests_closed = false;
        let mut trunk_probe = crate::trunk_queue_poller::TrunkQueueProbe::new();
        let mut spec_schedule = crate::speculative_conflict::SpeculativeCheckSchedule::default();
        let mut stacking_schedule = crate::stacked_pr_structuring::StackingSchedule::default();
        let stacking_fetcher = crate::stacked_pr_structuring::GhPrChangedFiles;
        // Conflict remediation runs HERE — on its own bounded tasks — not on
        // the detection path below. See `crate::conflict_remediation` for why
        // (short version: the escalation ladder leases a cube workspace and
        // rebases, which awaited inline turned a 60-second sweep into a
        // 32-minute detection blackout).
        let remediation = ConflictRemediationQueue::new(Arc::new(LadderRemediationJob::new(
            work_db.clone(),
            publisher.clone(),
            cube_client.clone(),
        )));
        let budget = pass_budget(interval);
        loop {
            // Refresh the shared-token budget from GitHub before probing so
            // this sweep's cadence reflects spend by every consumer (siblings,
            // the release job, ad-hoc `gh`) — not just the poller's own last
            // batch — and so the first sweep on boot isn't blind at the
            // `i64::MAX` sentinel. Free (0 GraphQL points) and best-effort.
            gh_scope(callers::MERGE_POLLER_BUDGET_REFRESH, refresh_rate_limit_budget()).await;
            // Scoped so every GitHub call this pass makes — including the
            // ones inside helpers the adaptive path also uses — is
            // attributed to the batched full sweep rather than blended
            // with the per-PR path below. The two have very different
            // per-point efficiency, and a single "merge_poller" bucket
            // would confirm whichever culprit you already suspected.
            let outcome = run_pass_within_budget(
                "full_sweep",
                budget,
                interval,
                &metrics,
                gh_scope(
                    callers::MERGE_POLLER_SWEEP,
                    run_one_pass(
                        work_db.as_ref(),
                        probe.as_ref(),
                        publisher.as_ref(),
                        Some(cube_client.as_ref()),
                        Some(completion_handler.as_ref()),
                        Some(&remediation),
                    ),
                ),
            )
            .await
            .unwrap_or_default();
            let last_run_at = Instant::now();
            record_sweep_metrics(&metrics, &outcome);
            if outcome.total_transitions() > 0 || outcome.pr_recheck_unresolved > 0 {
                tracing::info!(
                    merged = outcome.merged,
                    conflict_flagged = outcome.conflict_flagged,
                    conflict_cleared = outcome.conflict_cleared,
                    ci_flagged = outcome.ci_flagged,
                    ci_cleared = outcome.ci_cleared,
                    pr_recheck_recovered = outcome.pr_recheck_recovered,
                    pr_recheck_unresolved = outcome.pr_recheck_unresolved,
                    merge_queue_rebounced = outcome.merge_queue_rebounced,
                    late_pr_recovered = outcome.late_pr_recovered,
                    revision_invalidated = outcome.revision_invalidated,
                    worker_stopped_on_review = outcome.worker_stopped_on_review,
                    comments_reopened = outcome.comments_reopened,
                    "merge poller: sweep transitions",
                );
            }

            // Seed a default adaptive slot for every PR this full sweep
            // just considered. Existing entries (already scheduled by a
            // prior `reconcile_one` call below) are left alone — a fresh
            // default must not clobber a tier already learned from a
            // real probe.
            schedule.seed_defaults(current_pr_candidate_urls(work_db.as_ref()), last_run_at);

            // Layer 4: piggyback the speculative conflict-prediction
            // sweep on this same full-sweep cadence. Gated by its own
            // feature flag (default OFF) — off, this is a single cheap
            // local-DB read with no cube/GitHub activity.
            if completion_handler.speculative_conflict_prediction_enabled() {
                match work_db.list_chores_pending_merge_check() {
                    Ok(candidates) => {
                        gh_scope(
                            callers::SPECULATIVE_CONFLICT,
                            crate::speculative_conflict::run_speculative_pass(
                                work_db.as_ref(),
                                cube_client.as_ref(),
                                &metrics,
                                &mut spec_schedule,
                                &candidates,
                            ),
                        )
                        .await;
                    }
                    Err(err) => {
                        tracing::warn!(?err, "merge poller: failed to list candidates for speculative sweep");
                    }
                }
            }

            // Layer 4: stacked-PR auto-structuring. Also piggybacks on
            // the full-sweep cadence and its own default-OFF feature flag —
            // off, this block is skipped entirely (not even the local-DB
            // read below runs). On, `stacking_schedule.pass_due` gates the
            // local-DB read too, so throttled ticks (most of them —
            // `run_stacking_pass` self-throttles to at most one
            // `gh`-fetching pass per its own interval) do neither the DB
            // read nor the sweep; co-scheduling it here is safe regardless
            // of how often the loop ticks.
            if completion_handler.stacked_pr_auto_structuring_enabled() && stacking_schedule.pass_due(Instant::now()) {
                match work_db.list_chores_pending_merge_check() {
                    Ok(candidates) => {
                        gh_scope(
                            callers::STACKED_PR_STRUCTURING,
                            crate::stacked_pr_structuring::run_stacking_pass(
                                work_db.as_ref(),
                                publisher.as_ref(),
                                &stacking_fetcher,
                                &metrics,
                                &mut stacking_schedule,
                                &candidates,
                            ),
                        )
                        .await;
                    }
                    Err(err) => {
                        tracing::warn!(?err, "merge poller: failed to list candidates for stacking sweep");
                    }
                }
            }

            // Wait for the periodic full-sweep interval, an activation
            // kick, a targeted kick, or the next PR's adaptive poll time —
            // whichever comes first. Kicks received within the quiesce
            // window are silently absorbed — the inner loop keeps
            // listening so the first kick that arrives after the window
            // has elapsed triggers a pass immediately. The adaptive-timer
            // and targeted-kick arms never `break 'wait`: reconciling one
            // PR is not a full sweep, so neither resets `last_run_at` or
            // the full-sweep quiesce clock — they just reschedule that PR
            // and keep waiting.
            'wait: loop {
                let now = Instant::now();
                let elapsed = now.duration_since(last_run_at);
                // Stretch the full-sweep cadence alongside the per-PR
                // adaptive tiers (`PollTier::interval`) once the hourly
                // GitHub quota is running low — see
                // `rate_limit_throttle_factor`. A no-op (factor 1.0) once
                // quota is healthy.
                let throttle = rate_limit_throttle_factor();
                let effective_interval = if throttle > 1.0 {
                    interval.mul_f64(throttle)
                } else {
                    interval
                };
                let remaining_interval = effective_interval.saturating_sub(elapsed);
                let pr_wait = schedule
                    .next_due()
                    .map(|at| at.saturating_duration_since(now))
                    .unwrap_or(NO_PR_DUE_WAIT);
                // The Trunk observer's own cadence: 15s while an entry is
                // testing, 30s while entries only wait, and a bare
                // local-DB rescan when nothing is enqueued. Deliberately
                // NOT folded into `remaining_interval`: it must be able to
                // tick faster than the 60s full sweep without dragging the
                // GitHub probe (and its API quota) along with it.
                let trunk_wait = trunk_probe.next_wake_at(now).saturating_duration_since(now);
                tokio::select! {
                    _ = tokio::time::sleep(remaining_interval) => {
                        break 'wait;
                    }
                    _ = tokio::time::sleep(trunk_wait) => {
                        // Rides the poller's loop but ticks on its own
                        // faster cadence, so it gets its own bucket
                        // rather than inflating the sweep's.
                        let outcome = gh_scope(
                            callers::TRUNK_QUEUE_POLLER,
                            trunk_probe.run_pass(
                                &crate::trunk_queue_poller::TrunkSweepContext {
                                    work_db: work_db.as_ref(),
                                    publisher: publisher.as_ref(),
                                    api: trunk_queue_api.as_ref(),
                                },
                                Instant::now(),
                            ),
                        ).await;
                        crate::trunk_queue_poller::record_pass_metrics(&metrics, &outcome);
                        if outcome.is_noteworthy() {
                            tracing::info!(
                                queues_probed = outcome.queues_probed,
                                entry_lookups = outcome.entry_lookups,
                                state_writes = outcome.state_writes,
                                intents_retired = outcome.intents_retired,
                                probe_failures = outcome.probe_failures,
                                attentions_filed = outcome.attentions_filed,
                                "merge poller: trunk queue pass",
                            );
                        }
                        // continue listening; a Trunk pass is not a full sweep
                    }
                    _ = tokio::time::sleep(pr_wait) => {
                        for pr_url in schedule.drain_due(Instant::now()) {
                            // Bounded like the full sweep: this runs inside
                            // the wait `select!`, so an unbounded reconcile
                            // stalls the trunk observer, the activation kick,
                            // and every other PR's adaptive timer with it.
                            //
                            // The un-batched per-PR path: one reconcile
                            // probes a single PR, so its cost per PR is
                            // far higher than the batched sweep's. Its
                            // own scope is what makes that comparable
                            // from data instead of arguable from code.
                            let Some((outcome, tier)) = reconcile_one_within_budget(
                                &metrics,
                                gh_scope(
                                    callers::MERGE_POLLER_ADAPTIVE,
                                    reconcile_one(
                                        work_db.as_ref(),
                                        probe.as_ref(),
                                        publisher.as_ref(),
                                        Some(cube_client.as_ref()),
                                        Some(completion_handler.as_ref()),
                                        Some(&remediation),
                                        &pr_url,
                                    ),
                                ),
                                &pr_url,
                            )
                            .await
                            else {
                                // Timed out: drop this PR's adaptive slot for
                                // now; the periodic full sweep re-seeds it.
                                continue;
                            };
                            record_sweep_metrics(&metrics, &outcome);
                            if outcome.total_transitions() > 0 {
                                tracing::info!(
                                    pr_url,
                                    merged = outcome.merged,
                                    conflict_flagged = outcome.conflict_flagged,
                                    conflict_cleared = outcome.conflict_cleared,
                                    ci_flagged = outcome.ci_flagged,
                                    ci_cleared = outcome.ci_cleared,
                                    merge_queue_rebounced = outcome.merge_queue_rebounced,
                                    "merge poller: adaptive per-PR reconcile transitions",
                                );
                            }
                            schedule.reschedule(&pr_url, tier, Instant::now());
                        }
                        // continue listening in this same wait loop
                    }
                    _ = kick.notified() => {
                        let since_last = last_run_at.elapsed();
                        if since_last >= quiesce_window {
                            tracing::debug!(
                                since_last_ms = since_last.as_millis(),
                                "merge poller: activation kick → immediate sweep",
                            );
                            break 'wait;
                        }
                        tracing::debug!(
                            since_last_ms = since_last.as_millis(),
                            quiesce_ms = quiesce_window.as_millis(),
                            "merge poller: kick within quiesce window, absorbing",
                        );
                        // continue listening; periodic sleep arm will eventually fire
                    }
                    event = pr_reconcile_requests.recv(), if !pr_requests_closed => {
                        match event {
                            Some(Event::PrReconcileRequested { pr_url }) => {
                                let since_last = last_run_at.elapsed();
                                if since_last < quiesce_window {
                                    tracing::debug!(
                                        pr_url,
                                        since_last_ms = since_last.as_millis(),
                                        quiesce_ms = quiesce_window.as_millis(),
                                        "merge poller: PrReconcileRequested within quiesce window, absorbing",
                                    );
                                } else {
                                    tracing::debug!(
                                        pr_url,
                                        since_last_ms = since_last.as_millis(),
                                        "merge poller: PrReconcileRequested → reconciling named PR",
                                    );
                                    let Some((outcome, tier)) = reconcile_one_within_budget(
                                        &metrics,
                                        gh_scope(
                                            callers::MERGE_POLLER_REQUESTED,
                                            reconcile_one(
                                                work_db.as_ref(),
                                                probe.as_ref(),
                                                publisher.as_ref(),
                                                Some(cube_client.as_ref()),
                                                Some(completion_handler.as_ref()),
                                                Some(&remediation),
                                                &pr_url,
                                            ),
                                        ),
                                        &pr_url,
                                    )
                                    .await
                                    else {
                                        continue;
                                    };
                                    record_sweep_metrics(&metrics, &outcome);
                                    if outcome.total_transitions() > 0 {
                                        tracing::info!(
                                            pr_url,
                                            merged = outcome.merged,
                                            conflict_flagged = outcome.conflict_flagged,
                                            conflict_cleared = outcome.conflict_cleared,
                                            ci_flagged = outcome.ci_flagged,
                                            ci_cleared = outcome.ci_cleared,
                                            merge_queue_rebounced = outcome.merge_queue_rebounced,
                                            "merge poller: PrReconcileRequested reconcile transitions",
                                        );
                                    }
                                    schedule.reschedule(&pr_url, tier, Instant::now());
                                }
                            }
                            Some(_) => {
                                tracing::warn!(
                                    "merge poller: pr_reconcile_requests subscription received an \
                                     unexpected event kind (the subscribing filter should have excluded it)",
                                );
                            }
                            None => {
                                pr_requests_closed = true;
                                tracing::warn!(
                                    "merge poller: pr_reconcile_requests subscription closed \
                                     (event bus dropped) — keyed PR reconcile requests will no \
                                     longer be delivered; the periodic full sweep remains the backstop",
                                );
                            }
                        }
                        // continue listening; a keyed PR reconcile is not a full sweep
                    }
                }
            }
        }
    })
}
