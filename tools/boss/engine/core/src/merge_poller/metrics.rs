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

/// Increment the per-product, per-class conflict counter for one
/// newly-classified conflict event (Layer 0 telemetry, T5 — see
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

/// Lowercase and replace any character outside the registry's
/// allowed charset (`a-z 0-9 . _`) with `_`, so an arbitrary
/// `product_id` or `conflict_class` can't produce an invalid dynamic
/// metric name or let two distinct products collide on one counter.
///
/// `pub(crate)` so other per-product dynamic-counter call sites (e.g.
/// [`crate::speculative_conflict`]) reuse the same sanitization instead of
/// duplicating it.
pub(crate) fn sanitize_metric_name_component(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "_".to_owned()
    } else {
        sanitized
    }
}
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

/// Register all merge-poller counter handles with `registry`. Called
/// from [`crate::metrics_init::init_all`] at engine startup.
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
    let previous = RATE_LIMIT_REMAINING.swap(remaining, Ordering::Relaxed);
    if remaining < RATE_LIMIT_LOW_WATER && previous >= RATE_LIMIT_LOW_WATER {
        tracing::warn!(
            remaining,
            low_water = RATE_LIMIT_LOW_WATER,
            throttle_factor = throttle_factor_for(remaining, RATE_LIMIT_LOW_WATER),
            "merge poller: GitHub API quota running low — stretching poll cadence",
        );
    } else if remaining >= RATE_LIMIT_LOW_WATER && previous < RATE_LIMIT_LOW_WATER {
        tracing::info!(
            remaining,
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
/// The batched probe already folds `rateLimit { remaining }` into its
/// response ([`build_batch_query`]), but that only refreshes the budget
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
    let Ok(output) = gh_output(&["api", "graphql", "-f", "query={ rateLimit { remaining } }"]).await else {
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
