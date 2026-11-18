use super::*;

crate::register_counter!(
    MERGED,
    "merge_poller.merged",
    "PRs transitioned to merged in one sweep."
);
crate::register_counter!(
    CONFLICT_FLAGGED,
    "merge_poller.conflict_flagged",
    "PRs flipped to blocked:merge_conflict in one sweep."
);
crate::register_counter!(
    CONFLICT_CLEARED,
    "merge_poller.conflict_cleared",
    "PRs cleared from blocked:merge_conflict in one sweep."
);

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
    /// PendingReview tasks that had been admission-deferred and for which
    /// this pass successfully created (or reused) a pre-merge batch.
    pub review_admission_recovered: usize,
    /// PendingReview tasks still waiting on a review-pool reservation
    /// after this pass retried admission.
    pub review_admission_still_deferred: usize,
    /// Non-terminal review batches this pass failed because their cycle
    /// root is gone or their members have been inert past the stale bound.
    pub review_batches_reaped: usize,
    /// Point-in-time reserved review-pool units sampled at the end of the pass.
    pub reserved_review_units: i64,
    /// Point-in-time count of PendingReview tasks waiting on admission.
    pub deferred_pre_merge_count: usize,
}

impl SweepOutcome {
    pub fn total_transitions(self) -> usize {
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
            + self.review_admission_recovered
            + self.review_batches_reaped
    }
}

/// Increment the per-product, per-class conflict counter for one
/// newly-classified conflict event (Layer 0 telemetry — see
/// `tools/boss/docs/designs/merge-conflict-reduction-and-fast-
/// resolution-for-parallel-tasks.md`, "Counters — must be scopable
/// per-product"). `product_id` isn't a fixed set known at compile
/// time, so this can't be a `register_counter!` static handle like
/// [`CONFLICT_FLAGGED`] — it dynamically registers (and increments)
/// `conflict.<product>.<class>.classified` via
/// [`crate::metrics::Registry::counter_inc_by_dynamic`]. Call once
/// per `conflict_resolutions` row the moment its `conflict_class`
/// becomes known (diagnosis-set time on the review-watch path,
/// record time on the producer-rebase path) so it fires exactly
/// once per event, alongside `CONFLICT_FLAGGED`.
pub fn record_conflict_class_counter(registry: &Registry, product_id: &str, conflict_class: &str) {
    let name = format!(
        "conflict.{}.{}.classified",
        sanitize_metric_name_component(product_id),
        sanitize_metric_name_component(conflict_class),
    );
    registry.counter_inc_by_dynamic(&name, "Conflict events classified into this class for this product.", 1);
}

pub use boss_metrics::sanitize_metric_name_component;
crate::register_counter!(
    PR_RECHECK_RECOVERED,
    "merge_poller.pr_recheck_recovered",
    "Missed PR-open transitions recovered by recheck in one sweep."
);
crate::register_counter!(
    PR_RECHECK_UNRESOLVED,
    "merge_poller.pr_recheck_unresolved",
    "PR-detection rechecks that still found no bindable PR in one sweep."
);
crate::register_counter!(
    MERGE_QUEUE_REBOUNCED,
    "merge_poller.merge_queue_rebounced",
    "PRs flipped to blocked:ci_failure due to a merge-queue FAILED_CHECKS dequeue in one sweep."
);
crate::register_counter!(
    LATE_PR_RECOVERED,
    "merge_poller.late_pr_recovered",
    "Late PRs bound to active tasks from terminal executions (double-spawn recovery) in one sweep."
);
crate::register_counter!(
    REVISION_INVALIDATED,
    "merge_poller.revision_invalidated",
    "Pending/active revision executions stopped because their parent PR merged or closed in one sweep."
);
crate::register_counter!(
    WORKER_STOPPED_ON_REVIEW,
    "merge_poller.worker_stopped_on_review",
    "Live worker executions stopped because their task auto-transitioned to in_review (CI detected green) in one sweep."
);
crate::register_counter!(
    COMMENTS_REOPENED,
    "merge_poller.comments_reopened",
    "in_revision comments reopened because their task's PR closed without merging in one sweep."
);
crate::register_counter!(
    TRUNK_EPISODES_ADOPTED,
    "merge_poller.trunk_episodes_adopted",
    "Trunk merge-queue evictions on episodes Boss did not initiate, adopted into the Trunk queue lane so they can be remediated."
);
crate::register_counter!(
    PASS_OVERRUN,
    "merge_poller.pass_overrun",
    "Detection passes that took longer than their own sweep cadence."
);
crate::register_counter!(
    PASS_TIMED_OUT,
    "merge_poller.pass_timed_out",
    "Detection passes abandoned mid-flight after exceeding the pass timeout."
);
crate::register_counter!(
    ADAPTIVE_BATCHES,
    "merge_poller.adaptive_batches",
    "Batched adaptive reconciles issued — the GraphQL round-trip pair count of the adaptive path."
);
crate::register_counter!(
    ADAPTIVE_PRS_RECONCILED,
    "merge_poller.adaptive_prs_reconciled",
    "PRs covered by batched adaptive reconciles."
);
crate::register_gauge!(
    ADAPTIVE_TRACKED,
    "merge_poller.adaptive_tracked",
    "PRs currently holding an adaptive poll slot (those whose tier polls faster than the full sweep)."
);
crate::register_gauge!(
    REVIEW_POOL_RESERVED_UNITS,
    "review_pool.reserved_units",
    "Weighted reservation units currently held by non-terminal review batches with a live cycle root."
);
crate::register_gauge!(
    REVIEW_POOL_DEFERRED_PRE_MERGE,
    "review_pool.deferred_pre_merge",
    "PendingReview tasks waiting for a review-pool reservation (no live pre-merge batch)."
);
crate::register_counter!(
    REVIEW_ADMISSION_DEFERRED,
    "review_pool.admission_deferred",
    "Pre-merge review admissions deferred because the pool was at reservation capacity."
);
crate::register_counter!(
    REVIEW_ADMISSION_RECOVERED,
    "review_pool.admission_recovered",
    "Previously deferred pre-merge reviews that were admitted on a later sweep."
);
crate::register_counter!(
    REVIEW_BATCH_REAPED,
    "review_pool.batch_reaped",
    "Non-terminal review batches failed because their cycle root is gone or members went inert."
);

/// Record one batched adaptive reconcile of `size` PRs.
///
/// The ratio of [`ADAPTIVE_PRS_RECONCILED`] to [`ADAPTIVE_BATCHES`] is the
/// number this whole change turns on: it is the average due-set size, and
/// GraphQL's 1-point-per-query floor means the adaptive path's cost is
/// proportional to the *batch* count, not the PR count. A ratio pinned at
/// 1.0 would mean the coalescing window is buying nothing and the batching
/// is decorative.
///
/// The mean alone can hide a bimodal distribution (many 1s plus a few large
/// batches average out to a healthy-looking number), so the size histogram
/// goes out as its own dynamically-registered counter family. Bucketed
/// rather than one counter per exact size, to keep the registry's entry
/// count bounded by a fleet that can legitimately reach dozens of open PRs.
pub fn record_adaptive_batch(registry: &Registry, size: usize) {
    if size == 0 {
        return;
    }
    ADAPTIVE_BATCHES.inc(registry);
    ADAPTIVE_PRS_RECONCILED.inc_by(registry, size as u64);
    registry.counter_inc_by_dynamic(
        &format!("merge_poller.adaptive_batch_size.{}", adaptive_batch_size_bucket(size)),
        "Batched adaptive reconciles whose due set fell in this size bucket.",
        1,
    );
}

/// Bucket label for a due-set size. Exact for the sizes where the batching
/// question is actually decided (1 vs 2), coarser above.
pub(crate) fn adaptive_batch_size_bucket(size: usize) -> &'static str {
    match size {
        0 => "0",
        1 => "1",
        2 => "2",
        3..=5 => "3_5",
        6..=10 => "6_10",
        11..=25 => "11_25",
        _ => "26_plus",
    }
}

/// Register all merge-poller counter handles with `registry`. Called
/// from `metrics_init::init_all` at engine startup.
pub fn init(registry: &Registry) {
    registry.register_counter(&MERGED);
    registry.register_counter(&CONFLICT_FLAGGED);
    registry.register_counter(&CONFLICT_CLEARED);
    registry.register_counter(&PR_RECHECK_RECOVERED);
    registry.register_counter(&PR_RECHECK_UNRESOLVED);
    registry.register_counter(&MERGE_QUEUE_REBOUNCED);
    registry.register_counter(&LATE_PR_RECOVERED);
    registry.register_counter(&REVISION_INVALIDATED);
    registry.register_counter(&WORKER_STOPPED_ON_REVIEW);
    registry.register_counter(&COMMENTS_REOPENED);
    registry.register_counter(&TRUNK_EPISODES_ADOPTED);
    registry.register_counter(&PASS_OVERRUN);
    registry.register_counter(&PASS_TIMED_OUT);
    registry.register_counter(&ADAPTIVE_BATCHES);
    registry.register_counter(&ADAPTIVE_PRS_RECONCILED);
    registry.register_gauge(&ADAPTIVE_TRACKED);
    registry.register_gauge(&REVIEW_POOL_RESERVED_UNITS);
    registry.register_gauge(&REVIEW_POOL_DEFERRED_PRE_MERGE);
    registry.register_counter(&REVIEW_ADMISSION_DEFERRED);
    registry.register_counter(&REVIEW_ADMISSION_RECOVERED);
    registry.register_counter(&REVIEW_BATCH_REAPED);
}

// ── GitHub API quota budget ─────────────────────────────────────────────
//
// Every hour the token this poller shares with ci_watch, conflict_watch,
// review flows, and worker `gh` calls resets to a fixed request budget.
// Per-row polling (one request per candidate per pass) can exceed that
// budget before the hour is out, blinding the poller for the remainder —
// see the design note on `fetch_merge_queue_dequeue_events_batch` for the
// specific per-row cost this closes. The state below is the other half of
// that fix: react to the *remaining* budget GitHub reports, rather than
// only reducing request volume and hoping it's now always enough.

/// Process-wide GitHub API rate-limit budget, refreshed by [`record_rate_limit`]
/// from the `rateLimit { remaining }` field folded into every batched
/// merge-poller GraphQL query ([`build_batch_query`]). [`PollTier::interval`]
/// and the full-sweep wait in [`spawn_loop`] read [`rate_limit_throttle_factor`]
/// to stretch their cadence once the hourly quota is running low, instead of
/// continuing to poll at full speed until GitHub starts returning 403s.
///
/// A process-wide static rather than a field threaded through `MergeProbe`:
/// every test double implements that trait without a live `gh` transport to
/// report real quota from, and the budget is a fact about the shared
/// token/process, not about any one probe instance. `i64::MAX` is the
/// "no data yet" sentinel — the poller never throttles blind before its
/// first real reading.
pub(crate) static RATE_LIMIT_REMAINING: AtomicI64 = AtomicI64::new(i64::MAX);

/// Requests reserved as headroom for every OTHER GitHub consumer sharing
/// this token (ci_watch, conflict_watch, review flows, worker `gh` calls,
/// and out-of-process users of the same personal token such as the
/// `boss-release` job) — the poller starts stretching its own cadence once
/// its visibility into `remaining` drops below this, instead of being the
/// caller that finally trips the 403 for everyone.
///
/// Sized to exceed the cost of a single full sweep, not just one round
/// trip: at ~47 open PRs and the trimmed [`PR_PROBE_FIELDS`] node counts a
/// sweep spends on the order of tens of GraphQL points, and one drained
/// batch can overshoot a narrow reserve between two `remaining` readings.
/// A wide reserve means the poller has already stretched its cadence hard
/// *before* it can strand a sibling — the `boss-release` `gh release list`
/// exhaustion (personal token, GraphQL 5000/5000) this remediation targets
/// was exactly that starvation. Reserving ~1500 of the hourly 5000 leaves
/// the release job and ad-hoc `gh` a dependable slice.
pub(crate) const RATE_LIMIT_LOW_WATER: i64 = 1500;

/// Multiplier the poller applies to its base poll intervals once
/// `remaining` GraphQL quota drops below `low_water`. Above the low-water
/// mark this is `1.0` (no behaviour change from today). Below it, the
/// multiplier scales linearly up to `8.0` as `remaining` approaches zero,
/// so a badly-drained budget backs off hard but the poller still
/// eventually polls rather than stalling completely.
///
/// Pure function of the two readings so the threshold math is
/// unit-testable without touching the process-wide atomic.
pub(crate) fn throttle_factor_for(remaining: i64, low_water: i64) -> f64 {
    if low_water <= 0 || remaining >= low_water {
        return 1.0;
    }
    if remaining <= 0 {
        return 8.0;
    }
    let severity = 1.0 - (remaining as f64 / low_water as f64);
    1.0 + severity * 7.0
}

/// Current poll-cadence multiplier from the last-observed GitHub quota
/// reading. See [`throttle_factor_for`].
pub(crate) fn rate_limit_throttle_factor() -> f64 {
    throttle_factor_for(RATE_LIMIT_REMAINING.load(Ordering::Relaxed), RATE_LIMIT_LOW_WATER)
}

/// Parse the `rateLimit { remaining }` field a batched GraphQL response
/// carries alongside its `data`, if present. `None` when the field is
/// absent (an older `gh`, or a response that never got that far) — the
/// caller leaves the budget unchanged rather than resetting it to
/// "unknown".
pub(crate) fn parse_rate_limit_remaining(body: &serde_json::Value) -> Option<i64> {
    body["data"]["rateLimit"]["remaining"].as_i64()
}

/// Record a fresh `remaining` reading from a batched GraphQL response,
/// logging once when it crosses the throttle threshold in either
/// direction — not on every still-low pass, which would otherwise spam
/// the trace at up to one line per sweep for as long as the quota stays
/// drained.
pub(crate) fn record_rate_limit(body: &serde_json::Value) {
    let Some(remaining) = parse_rate_limit_remaining(body) else {
        return;
    };
    record_rate_limit_remaining(remaining, "graphql_response");
}

/// Fold an already-extracted `remaining` reading into the process-wide
/// budget, logging on a threshold crossing exactly like
/// [`record_rate_limit`] does for a parsed response body.
///
/// **Every** write to [`RATE_LIMIT_REMAINING`] must go through here.
/// A direct `RATE_LIMIT_REMAINING.store(...)` silently moves the
/// multiplier that [`rate_limit_throttle_factor`] hands to every poll
/// interval without leaving a line in the trace, which makes the
/// `throttle_factor` values recorded elsewhere an unfaithful record of
/// the multiplier actually in force — the batched-probe rate-limit
/// rejection path used to do exactly that, forcing the factor to 8.0
/// while emitting no "quota running low" line at all.
///
/// `source` names the observation ("graphql_response", the synthetic
/// "rate_limit_rejection", …) so a threshold crossing is attributable to
/// the call that caused it rather than just "something set it".
pub(crate) fn record_rate_limit_remaining(remaining: i64, source: &str) {
    let previous = RATE_LIMIT_REMAINING.swap(remaining, Ordering::Relaxed);
    if remaining < RATE_LIMIT_LOW_WATER && previous >= RATE_LIMIT_LOW_WATER {
        tracing::warn!(
            remaining,
            source,
            low_water = RATE_LIMIT_LOW_WATER,
            throttle_factor = throttle_factor_for(remaining, RATE_LIMIT_LOW_WATER),
            "merge poller: GitHub API quota running low — stretching poll cadence",
        );
    } else if remaining >= RATE_LIMIT_LOW_WATER && previous < RATE_LIMIT_LOW_WATER {
        tracing::info!(
            remaining,
            source,
            "merge poller: GitHub API quota recovered — resuming normal poll cadence",
        );
    }
}

/// Proactively read GitHub's shared GraphQL quota and fold it into the
/// process-wide budget before a full sweep, so the poller's cadence /
/// throttle decisions reflect spend by EVERY consumer sharing this token —
/// `ci_watch`, `conflict_watch`, worker `gh` calls, and out-of-process
/// users of the same personal token like the `boss-release` job — not only
/// the poller's own last batched probe.
///
/// The batched probe already folds [`boss_gh_telemetry::RATE_LIMIT_SELECTION`]
/// into its response ([`build_batch_query`]), but that only refreshes the budget
/// when the poller *itself* issues a query. Between two sweeps — and across
/// the long Cold waits when little is moving — a sibling or the release job
/// can drain the reserve invisibly, and the next sweep then fires its whole
/// ~47-PR batch at full cadence on stale "healthy" state before its own
/// response reveals the drain. Reading the live number up front closes that
/// window: the sweep sees the true shared budget and stretches its cadence
/// (via [`rate_limit_throttle_factor`]) before spending, and it also
/// replaces the `i64::MAX` cold-start sentinel on the very first sweep so
/// the poller is never blind-at-full-speed on boot.
///
/// Querying `rateLimit` is itself free — GitHub charges it 0 points and
/// does not count it as a call — so this is a zero-quota refresh.
/// Best-effort: a spawn failure, non-zero exit, or unparseable body leaves
/// the budget unchanged (see [`record_rate_limit`]), exactly like a batched
/// response that never carried the field.
pub(crate) async fn refresh_rate_limit_budget() {
    let query = format!("query={{ {} }}", boss_gh_telemetry::RATE_LIMIT_SELECTION);
    let Ok(output) = gh_output(&["api", "graphql", "-f", &query]).await else {
        return;
    };
    if !output.status.success() {
        return;
    }
    if let Ok(body) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
        record_rate_limit(&body);
    }
}

/// Whether a `gh` failure's stderr indicates the request was itself
/// rejected for exceeding the GitHub API rate limit, as opposed to a
/// network/transport failure or a genuine query error. Distinguishing
/// this in the trace is what let the hourly-blindness pattern go
/// unnoticed until a manual trace sweep — both failure modes previously
/// surfaced as identical "probe failed" debug lines.
pub(crate) fn is_rate_limit_error(stderr: &str) -> bool {
    stderr.to_ascii_lowercase().contains("rate limit")
}
