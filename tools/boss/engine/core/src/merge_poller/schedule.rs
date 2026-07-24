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

/// Targeted companion to the broad [`Notify`]-based `kick` accepted by
/// [`spawn_loop`]. Where the broad kick just says "sweep everything now",
/// a targeted kick additionally names the PR that prompted it — the shape
/// the doc's push-event and adaptive-per-PR-timer follow-ups
/// (`tools/boss/docs/investigations/
/// github-event-detection-webhooks-vs-polling-2026-07-08.md` §9 items 3–4)
/// need to reconcile exactly one PR instead of triggering a full sweep.
///
/// Firing it reconciles exactly the named PR(s) via [`reconcile_one`] —
/// not a full [`run_one_pass`] sweep — subject to the same quiesce window
/// as the broad kick.
#[derive(Clone, Default)]
pub struct PrReconcilerTargetedKick {
    notify: Arc<Notify>,
    pending: Arc<Mutex<Vec<String>>>,
}

impl PrReconcilerTargetedKick {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request an immediate pass to reconcile `pr_url`. Subject to the same
    /// quiesce window as the broad kick (see [`spawn_loop`]).
    pub fn kick(&self, pr_url: impl Into<String>) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(pr_url.into());
        self.notify.notify_one();
    }

    pub(crate) async fn notified(&self) {
        self.notify.notified().await;
    }

    pub(crate) fn drain_pending(&self) -> Vec<String> {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }
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
/// `targeted_kick` is the [`PrReconcilerTargetedKick`] companion — same
/// quiesce window, but it reconciles just the named PR(s) via
/// [`reconcile_one`] rather than triggering a full sweep.
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
    // (broad kick, targeted kick) — bundled to keep the parameter count
    // under clippy::too_many_arguments.
    kicks: (Arc<Notify>, PrReconcilerTargetedKick),
) -> tokio::task::JoinHandle<()> {
    let (cube_client, completion_handler, trunk_queue_api) = handlers;
    let (kick, targeted_kick) = kicks;
    tokio::spawn(async move {
        let quiesce_window = Duration::from_secs(15);
        let mut schedule = PrPollSchedule::default();
        let mut trunk_probe = crate::trunk_queue_poller::TrunkQueueProbe::new();
        let mut spec_schedule = crate::speculative_conflict::SpeculativeCheckSchedule::default();
        let mut stacking_schedule = crate::stacked_pr_structuring::StackingSchedule::default();
        let stacking_fetcher = crate::stacked_pr_structuring::GhPrChangedFiles;
        loop {
            // Refresh the shared-token budget from GitHub before probing so
            // this sweep's cadence reflects spend by every consumer (siblings,
            // the release job, ad-hoc `gh`) — not just the poller's own last
            // batch — and so the first sweep on boot isn't blind at the
            // `i64::MAX` sentinel. Free (0 GraphQL points) and best-effort.
            refresh_rate_limit_budget().await;
            let outcome = run_one_pass(
                work_db.as_ref(),
                probe.as_ref(),
                publisher.as_ref(),
                Some(cube_client.as_ref()),
                Some(completion_handler.as_ref()),
            )
            .await;
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
                        crate::speculative_conflict::run_speculative_pass(
                            work_db.as_ref(),
                            cube_client.as_ref(),
                            &metrics,
                            &mut spec_schedule,
                            &candidates,
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
                        crate::stacked_pr_structuring::run_stacking_pass(
                            work_db.as_ref(),
                            publisher.as_ref(),
                            &stacking_fetcher,
                            &metrics,
                            &mut stacking_schedule,
                            &candidates,
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
                        let outcome = trunk_probe.run_pass(
                            &crate::trunk_queue_poller::TrunkSweepContext {
                                work_db: work_db.as_ref(),
                                publisher: publisher.as_ref(),
                                api: trunk_queue_api.as_ref(),
                            },
                            Instant::now(),
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
                            let (outcome, tier) = reconcile_one(
                                work_db.as_ref(),
                                probe.as_ref(),
                                publisher.as_ref(),
                                Some(cube_client.as_ref()),
                                Some(completion_handler.as_ref()),
                                &pr_url,
                            )
                            .await;
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
                    _ = targeted_kick.notified() => {
                        let pr_urls = targeted_kick.drain_pending();
                        let since_last = last_run_at.elapsed();
                        if since_last < quiesce_window {
                            tracing::debug!(
                                ?pr_urls,
                                since_last_ms = since_last.as_millis(),
                                quiesce_ms = quiesce_window.as_millis(),
                                "merge poller: targeted kick within quiesce window, absorbing",
                            );
                        } else {
                            tracing::debug!(
                                ?pr_urls,
                                since_last_ms = since_last.as_millis(),
                                "merge poller: targeted kick → reconciling named PR(s)",
                            );
                            for pr_url in pr_urls {
                                let (outcome, tier) = reconcile_one(
                                    work_db.as_ref(),
                                    probe.as_ref(),
                                    publisher.as_ref(),
                                    Some(cube_client.as_ref()),
                                    Some(completion_handler.as_ref()),
                                    &pr_url,
                                )
                                .await;
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
                                        "merge poller: targeted-kick reconcile transitions",
                                    );
                                }
                                schedule.reschedule(&pr_url, tier, Instant::now());
                            }
                        }
                        // continue listening; targeted reconcile is not a full sweep
                    }
                }
            }
        }
    })
}
