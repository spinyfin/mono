//! The engine's GitHub API usage telemetry sink.
//!
//! `boss_gh_telemetry` observes every GitHub call at the point of egress
//! but deliberately knows nothing about counters or databases. This
//! module is the engine's implementation of that seam: it fans each
//! observed call out to the metrics registry (so `bossctl metrics`
//! answers "how much, cumulatively") and to `state.db` (so
//! `bossctl metrics github` answers "how much, by whom, per hour").
//!
//! # Why both
//!
//! A counter alone cannot express a rate against a *window*: the GitHub
//! budget is 5000 points per rolling hour, and a monotonic total says
//! nothing about which hour the spend landed in. Persisted per-call rows
//! alone would mean no cheap live read and no survival of the framework's
//! restart-rehydrate. The two surfaces answer different questions and
//! both are needed to attribute a quota drain.
//!
//! # Cardinality
//!
//! Per-caller counters are *dynamically* registered, because the useful
//! axis is a product of runtime facts (which subsystem, which bucket)
//! rather than a compile-time-known set. They are kept additive rather
//! than multiplicative — `calls.<caller>.<api>` and `verb.<api>.<verb>`
//! as separate families rather than one
//! `<caller>.<api>.<verb>` cross-product — so the registry holds tens of
//! entries, not hundreds.

use std::sync::Arc;
use std::time::Duration;

use boss_gh_telemetry::{GhApi, GhCallSample, GhCallSink, GhOutcome};
use boss_metrics::Registry;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::merge_poller::sanitize_metric_name_component;
use crate::work::{GithubApiCallRow, WorkDb};

crate::register_counter!(
    GITHUB_API_CALLS_TOTAL,
    "github_api.calls.total",
    "GitHub API calls the engine made, across every subsystem and both API buckets."
);
crate::register_counter!(
    GITHUB_API_POINTS_TOTAL,
    "github_api.points.total",
    "GraphQL points the engine spent, summed from the rateLimit block on each response."
);
crate::register_counter!(
    GITHUB_API_RATE_LIMITED_TOTAL,
    "github_api.rate_limited.total",
    "GitHub API calls rejected for exceeding the rate limit."
);
crate::register_counter!(
    GITHUB_API_CALLS_UNATTRIBUTED,
    "github_api.calls.unattributed",
    "GitHub API calls made outside any caller scope — the size of the attribution blind spot."
);
crate::register_counter!(
    GITHUB_API_CALLS_WITHOUT_READING,
    "github_api.calls.without_reading",
    "GitHub API calls whose response carried no rateLimit reading — spend counted but not measured."
);
crate::register_gauge!(
    GITHUB_API_GRAPHQL_REMAINING,
    "github_api.graphql.remaining",
    "GraphQL points left in the current hourly window, as of the last response that reported it."
);
crate::register_gauge!(
    GITHUB_API_REST_REMAINING,
    "github_api.rest.remaining",
    "REST requests left in the current hourly window, as of the last response that reported it."
);

/// Register the static handles. Called from
/// [`crate::metrics_init::init_all`] at engine startup.
pub fn register_metrics(registry: &Registry) {
    registry.register_counter(&GITHUB_API_CALLS_TOTAL);
    registry.register_counter(&GITHUB_API_POINTS_TOTAL);
    registry.register_counter(&GITHUB_API_RATE_LIMITED_TOTAL);
    registry.register_counter(&GITHUB_API_CALLS_UNATTRIBUTED);
    registry.register_counter(&GITHUB_API_CALLS_WITHOUT_READING);
    registry.register_gauge(&GITHUB_API_GRAPHQL_REMAINING);
    registry.register_gauge(&GITHUB_API_REST_REMAINING);
}

/// How many rows the writer task accumulates before committing, and how
/// long it waits for a batch to fill.
///
/// A per-call transaction would put a synchronous sqlite commit on the
/// path of every GitHub request; a large batch would lose more on an
/// unclean shutdown. 2s / 256 rows keeps the commit rate to well under
/// one per second at the polling volumes this exists to measure, while
/// bounding loss to a couple of seconds of samples.
const WRITE_BATCH_MAX: usize = 256;
const WRITE_BATCH_INTERVAL: Duration = Duration::from_secs(2);

/// How long persisted call rows are kept, and how often the writer
/// prunes. 14 days comfortably spans the multi-day usage profile this
/// data exists to produce while bounding a table that gains a row per
/// GitHub call.
const RETENTION: Duration = Duration::from_secs(14 * 24 * 60 * 60);
const PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The installed [`GhCallSink`]: bumps in-memory metrics inline and hands
/// the row to the writer task.
///
/// `record` runs on the egress path of every GitHub call, so it does only
/// atomic increments and one unbounded-channel send. Unbounded is
/// deliberate: a bounded channel would either block the caller (making
/// telemetry a latency source for the traffic it measures) or drop
/// samples silently under exactly the burst conditions that matter most.
/// The drain rate is one sqlite batch every couple of seconds against a
/// source rate of at most a few calls per second, so the queue cannot run
/// away.
struct EngineGhCallSink {
    registry: Arc<Registry>,
    tx: UnboundedSender<GhCallSample>,
}

impl GhCallSink for EngineGhCallSink {
    fn record(&self, sample: GhCallSample) {
        record_metrics(&self.registry, &sample);
        // A send failure means the writer task is gone (engine shutting
        // down). Losing telemetry then is fine and must not be noisy.
        let _ = self.tx.send(sample);
    }
}

/// Apply one sample to the metrics registry.
///
/// Split out from the sink so the counter semantics are unit-testable
/// against a local `Registry` without a database or a running task.
pub(crate) fn record_metrics(registry: &Registry, sample: &GhCallSample) {
    let caller = sanitize_metric_name_component(&sample.caller);
    let api = sample.api.as_str();

    GITHUB_API_CALLS_TOTAL.inc(registry);
    registry.counter_inc_by_dynamic(
        &format!("github_api.calls.{caller}.{api}"),
        "GitHub API calls made by this subsystem against this API bucket.",
        1,
    );
    registry.counter_inc_by_dynamic(
        &format!("github_api.verb.{api}.{}", sanitize_metric_name_component(&sample.verb)),
        "GitHub API calls of this verb against this API bucket.",
        1,
    );

    if sample.caller == boss_gh_telemetry::UNATTRIBUTED {
        GITHUB_API_CALLS_UNATTRIBUTED.inc(registry);
    }

    match sample.rate_limit {
        Some(rl) => {
            // The gauge is meaningful even without a per-call cost (e.g. a
            // GraphQL-over-HTTP call whose headers carry remaining/limit
            // but not a recoverable point cost), so it updates regardless.
            if sample.api == GhApi::Graphql {
                GITHUB_API_GRAPHQL_REMAINING.set(registry, rl.remaining);
            } else if sample.api == GhApi::Rest {
                GITHUB_API_REST_REMAINING.set(registry, rl.remaining);
            }
            match rl.cost {
                Some(_) => {
                    let points = sample.points();
                    registry.counter_inc_by_dynamic(
                        &format!("github_api.points.{caller}.{api}"),
                        "Quota units this subsystem spent against this API bucket.",
                        points,
                    );
                    // Only GraphQL points roll into the headline total — the
                    // two buckets are metered separately and a blended sum
                    // maps to no real budget.
                    if sample.api == GhApi::Graphql {
                        GITHUB_API_POINTS_TOTAL.inc_by(registry, points);
                    }
                }
                // A reading with no per-call cost is still an unattributed
                // spend, not a free one — count it as unread so the gap is
                // visible in `github_api.calls.without_reading` instead of
                // silently landing as 0 points.
                None => GITHUB_API_CALLS_WITHOUT_READING.inc(registry),
            }
        }
        None => GITHUB_API_CALLS_WITHOUT_READING.inc(registry),
    }

    if sample.outcome != GhOutcome::Ok {
        registry.counter_inc_by_dynamic(
            &format!("github_api.errors.{caller}.{}", sample.outcome.as_str()),
            "GitHub API calls by this subsystem that did not succeed.",
            1,
        );
        if sample.outcome == GhOutcome::RateLimited {
            GITHUB_API_RATE_LIMITED_TOTAL.inc(registry);
        }
    }
}

/// Flatten a sample into its persisted row.
pub(crate) fn row_for(sample: GhCallSample) -> GithubApiCallRow {
    GithubApiCallRow {
        started_at_ms: sample.started_at_ms,
        caller: sample.caller,
        api: sample.api.as_str().to_owned(),
        verb: sample.verb,
        endpoint: sample.endpoint,
        outcome: sample.outcome.as_str().to_owned(),
        duration_ms: sample.duration_ms,
        points_cost: sample.rate_limit.and_then(|rl| rl.cost),
        points_remaining: sample.rate_limit.map(|rl| rl.remaining),
        points_limit: sample.rate_limit.map(|rl| rl.limit),
        reset_at_ms: sample.rate_limit.and_then(|rl| rl.reset_at_ms),
    }
}

/// Install the process-wide sink and spawn the batching writer task.
///
/// Called once from engine startup, before any subsystem loop is
/// spawned, so no GitHub call the engine makes escapes the count.
pub fn install(registry: Arc<Registry>, work_db: Arc<WorkDb>) -> tokio::task::JoinHandle<()> {
    let (tx, rx) = unbounded_channel::<GhCallSample>();
    boss_gh_telemetry::install_sink(Arc::new(EngineGhCallSink { registry, tx }));
    spawn_writer(work_db, rx)
}

/// Spawn the batching writer over an already-created channel.
///
/// Split from [`install`] so the persistence half can be exercised
/// without touching the process-wide sink. That matters: the sink is
/// global, and installing one from a unit test captures every `gh` call
/// every *other* concurrently-running test in the same binary makes,
/// which is a race, not a test.
fn spawn_writer(
    work_db: Arc<WorkDb>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<GhCallSample>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut pending: Vec<GithubApiCallRow> = Vec::with_capacity(WRITE_BATCH_MAX);
        let mut flush_tick = tokio::time::interval(WRITE_BATCH_INTERVAL);
        flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut prune_tick = tokio::time::interval(PRUNE_INTERVAL);
        prune_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                received = rx.recv() => {
                    match received {
                        Some(sample) => {
                            pending.push(row_for(sample));
                            if pending.len() >= WRITE_BATCH_MAX {
                                flush(&work_db, &mut pending);
                            }
                        }
                        None => {
                            // Every sender dropped — the engine is going
                            // away. Commit what's in hand and stop.
                            flush(&work_db, &mut pending);
                            return;
                        }
                    }
                }
                _ = flush_tick.tick() => flush(&work_db, &mut pending),
                _ = prune_tick.tick() => {
                    let cutoff = boss_gh_telemetry::now_ms() - RETENTION.as_millis() as i64;
                    match work_db.prune_github_api_calls(cutoff) {
                        Ok(0) => {}
                        Ok(deleted) => tracing::debug!(deleted, "github_api_usage: pruned expired call rows"),
                        Err(err) => tracing::warn!(?err, "github_api_usage: prune failed"),
                    }
                }
            }
        }
    })
}

/// Commit `pending` and clear it. A DB failure is logged and the rows are
/// dropped rather than retried forever: usage telemetry must never
/// backpressure or wedge the engine over its own bookkeeping.
fn flush(work_db: &WorkDb, pending: &mut Vec<GithubApiCallRow>) {
    if pending.is_empty() {
        return;
    }
    if let Err(err) = work_db.insert_github_api_calls(pending) {
        tracing::warn!(
            ?err,
            rows = pending.len(),
            "github_api_usage: failed to persist call rows; dropping this batch",
        );
    }
    pending.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_gh_telemetry::RateLimit;

    fn sample(caller: &str, api: GhApi, outcome: GhOutcome, rate_limit: Option<RateLimit>) -> GhCallSample {
        GhCallSample {
            caller: caller.to_owned(),
            api,
            verb: "api graphql".to_owned(),
            endpoint: "graphql".to_owned(),
            started_at_ms: 1_000,
            duration_ms: 20,
            outcome,
            rate_limit,
        }
    }

    fn rl(cost: i64, remaining: i64) -> Option<RateLimit> {
        Some(RateLimit {
            cost: Some(cost),
            remaining,
            limit: 5000,
            reset_at_ms: Some(1_784_912_400_000),
        })
    }

    fn rl_no_cost(remaining: i64) -> Option<RateLimit> {
        Some(RateLimit {
            cost: None,
            remaining,
            limit: 5000,
            reset_at_ms: Some(1_784_912_400_000),
        })
    }

    fn registry() -> Registry {
        let registry = Registry::new();
        register_metrics(&registry);
        registry
    }

    #[test]
    fn attributes_points_to_the_calling_subsystem() {
        let registry = registry();
        record_metrics(
            &registry,
            &sample("merge_poller.sweep", GhApi::Graphql, GhOutcome::Ok, rl(26, 4974)),
        );
        record_metrics(
            &registry,
            &sample("merge_poller.adaptive", GhApi::Graphql, GhOutcome::Ok, rl(2, 4972)),
        );

        assert_eq!(
            registry.counter_value("github_api.points.merge_poller_sweep.graphql"),
            Some(26)
        );
        assert_eq!(
            registry.counter_value("github_api.points.merge_poller_adaptive.graphql"),
            Some(2),
            "the two poller paths must not share a bucket — separating them is the point",
        );
        assert_eq!(registry.counter_value("github_api.points.total"), Some(28));
        assert_eq!(registry.counter_value("github_api.calls.total"), Some(2));
    }

    #[test]
    fn rest_points_stay_out_of_the_graphql_total() {
        // The two budgets are independent; a blended total would map to
        // no real quota and would misreport how close GraphQL is to
        // exhaustion.
        let registry = registry();
        record_metrics(&registry, &sample("ci_watch", GhApi::Rest, GhOutcome::Ok, rl(1, 4900)));

        assert_eq!(registry.counter_value("github_api.points.ci_watch.rest"), Some(1));
        assert_eq!(registry.counter_value("github_api.points.total"), Some(0));
        assert_eq!(registry.gauge_value("github_api.rest.remaining"), Some(4900));
        assert_eq!(registry.gauge_value("github_api.graphql.remaining"), Some(0));
    }

    #[test]
    fn points_bucket_separates_graphql_from_rest_for_the_same_caller() {
        // external_tracker issues both `graphql` and `rest_get`/`rest_patch`
        // calls through the same `execute_gh`; blending its GraphQL points
        // and REST request count into one `github_api.points.<caller>`
        // number would map to no real budget, exactly the objection this
        // module raises against a blended `github_api.points.total`.
        let registry = registry();
        record_metrics(
            &registry,
            &sample("external_tracker", GhApi::Graphql, GhOutcome::Ok, rl(5, 4995)),
        );
        record_metrics(
            &registry,
            &sample("external_tracker", GhApi::Rest, GhOutcome::Ok, rl(1, 4999)),
        );

        assert_eq!(
            registry.counter_value("github_api.points.external_tracker.graphql"),
            Some(5)
        );
        assert_eq!(
            registry.counter_value("github_api.points.external_tracker.rest"),
            Some(1),
            "REST request count must stay in its own bucket, not summed with GraphQL points",
        );
    }

    #[test]
    fn a_reading_with_no_cost_updates_the_gauge_but_counts_as_unread() {
        // A GraphQL-over-HTTP call's headers give remaining/limit but not
        // a recoverable per-call point cost — the gauge should still move,
        // but the spend must show up as unattributed rather than as a
        // silent 0.
        let registry = registry();
        record_metrics(
            &registry,
            &sample("external_tracker", GhApi::Graphql, GhOutcome::Ok, rl_no_cost(4_950)),
        );

        assert_eq!(registry.gauge_value("github_api.graphql.remaining"), Some(4_950));
        assert_eq!(registry.counter_value("github_api.points.total"), Some(0));
        assert_eq!(
            registry.counter_value("github_api.points.external_tracker.graphql"),
            None,
            "no points counter should be created for a cost-less reading",
        );
        assert_eq!(registry.counter_value("github_api.calls.without_reading"), Some(1));
    }

    #[test]
    fn remaining_gauge_tracks_the_latest_graphql_reading() {
        let registry = registry();
        record_metrics(
            &registry,
            &sample("merge_poller.sweep", GhApi::Graphql, GhOutcome::Ok, rl(26, 4974)),
        );
        record_metrics(
            &registry,
            &sample("merge_poller.sweep", GhApi::Graphql, GhOutcome::Ok, rl(26, 227)),
        );
        assert_eq!(registry.gauge_value("github_api.graphql.remaining"), Some(227));
    }

    #[test]
    fn unattributed_calls_are_counted_as_such() {
        let registry = registry();
        record_metrics(
            &registry,
            &sample(boss_gh_telemetry::UNATTRIBUTED, GhApi::Cli, GhOutcome::Ok, None),
        );
        assert_eq!(registry.counter_value("github_api.calls.unattributed"), Some(1));
        assert_eq!(
            registry.counter_value("github_api.calls.without_reading"),
            Some(1),
            "a call with no rateLimit block is spend we counted but did not measure",
        );
    }

    #[test]
    fn rate_limited_calls_are_split_from_generic_errors() {
        let registry = registry();
        record_metrics(
            &registry,
            &sample("merge_poller.sweep", GhApi::Graphql, GhOutcome::RateLimited, None),
        );
        record_metrics(
            &registry,
            &sample("merge_poller.sweep", GhApi::Graphql, GhOutcome::Error, None),
        );

        assert_eq!(registry.counter_value("github_api.rate_limited.total"), Some(1));
        assert_eq!(
            registry.counter_value("github_api.errors.merge_poller_sweep.rate_limited"),
            Some(1),
        );
        assert_eq!(
            registry.counter_value("github_api.errors.merge_poller_sweep.error"),
            Some(1),
        );
    }

    #[test]
    fn successful_calls_register_no_error_counter() {
        let registry = registry();
        record_metrics(&registry, &sample("ci_watch", GhApi::Rest, GhOutcome::Ok, rl(1, 4000)));
        assert_eq!(registry.counter_value("github_api.errors.ci_watch.error"), None);
    }

    #[test]
    fn verb_counter_is_sanitized_into_a_valid_metric_name() {
        // `counter_inc_by_dynamic` panics on a name outside `a-z0-9._`,
        // and verbs legitimately contain a space ("api graphql").
        let registry = registry();
        record_metrics(
            &registry,
            &sample("merge_poller.sweep", GhApi::Graphql, GhOutcome::Ok, rl(1, 10)),
        );
        assert_eq!(registry.counter_value("github_api.verb.graphql.api_graphql"), Some(1));
    }

    #[test]
    fn row_carries_the_quota_reading_into_the_persisted_shape() {
        let row = row_for(sample(
            "merge_poller.adaptive",
            GhApi::Graphql,
            GhOutcome::Ok,
            rl(2, 4972),
        ));
        assert_eq!(row.caller, "merge_poller.adaptive");
        assert_eq!(row.api, "graphql");
        assert_eq!(row.points_cost, Some(2));
        assert_eq!(row.points_remaining, Some(4972));
        assert_eq!(row.points_limit, Some(5000));
        assert_eq!(row.reset_at_ms, Some(1_784_912_400_000));
        assert_eq!(row.started_at_ms, 1_000, "timestamps stay integer epoch ms");
    }

    #[tokio::test]
    async fn writer_task_carries_recorded_calls_into_state_db() {
        // The channel half of the seam, end to end: a sample handed to the
        // writer must land in `github_api_calls` with its caller and cost
        // intact. Driven through a local channel rather than the global
        // sink, so it cannot capture (or be perturbed by) `gh` calls made
        // by other tests running concurrently in this binary.
        let db = Arc::new(WorkDb::open_in_memory().expect("open db"));
        let (tx, rx) = unbounded_channel::<GhCallSample>();
        let handle = spawn_writer(db.clone(), rx);

        // Fill a whole batch, so the size trigger is the commit path under
        // test — the one that actually carries volume in production.
        //
        // Stamp the samples with a live timestamp rather than taking the
        // fixture's epoch-1970 one: the writer's prune tick is ready on its
        // very first `select!` pass (a tokio `interval` yields its first
        // tick immediately), and prune deletes everything older than
        // RETENTION. A 1970 `started_at_ms` is committed and then deleted
        // out from under the assertions below.
        let started_at_ms = boss_gh_telemetry::now_ms();
        for _ in 0..WRITE_BATCH_MAX {
            let mut s = sample(
                "merge_poller.adaptive",
                GhApi::Graphql,
                GhOutcome::Ok,
                rl(2, 4972),
            );
            s.started_at_ms = started_at_ms;
            tx.send(s).expect("writer task is alive");
        }

        // Close the channel and await the writer's own completion signal:
        // on `None` it commits whatever it still holds and returns, so the
        // join is the point at which every sample is durably in the table.
        //
        // Polling the table instead races the 2s flush timer, which is free
        // to fire mid-drain and commit a *partial* batch; the remainder then
        // sits in `pending` until the next tick, and a reader that stops at
        // the first non-empty result sees a short count.
        drop(tx);
        handle
            .await
            .expect("writer task exits cleanly once its senders drop");

        let buckets = db.github_api_usage_by_caller(0).expect("query");
        assert_eq!(buckets.len(), 1, "the recorded calls must reach state.db");
        assert_eq!(buckets[0].caller, "merge_poller.adaptive");
        assert_eq!(buckets[0].calls, WRITE_BATCH_MAX as i64);
        assert_eq!(buckets[0].points, 2 * WRITE_BATCH_MAX as i64);
    }

    #[tokio::test]
    async fn writer_task_commits_what_it_holds_when_the_sender_drops() {
        // A partial batch must not be lost on shutdown: when every sender
        // is gone the writer commits what it has before returning.
        let db = Arc::new(WorkDb::open_in_memory().expect("open db"));
        let (tx, rx) = unbounded_channel::<GhCallSample>();
        let handle = spawn_writer(db.clone(), rx);

        // Live timestamp for the same reason as the test above: a row
        // stamped in 1970 is outside RETENTION, and the writer's prune tick
        // is entitled to delete it the moment it is committed.
        let mut s = sample("ci_watch", GhApi::Rest, GhOutcome::Ok, rl(1, 4000));
        s.started_at_ms = boss_gh_telemetry::now_ms();
        tx.send(s).expect("writer task is alive");
        drop(tx);
        handle.await.expect("writer task exits cleanly when its senders drop");

        let buckets = db.github_api_usage_by_caller(0).expect("query");
        assert_eq!(buckets.len(), 1, "a partial batch must be committed, not dropped");
        assert_eq!(buckets[0].calls, 1);
    }

    #[test]
    fn row_without_a_reading_uses_null_not_zero() {
        // NULL and 0 mean very different things here: 0 remaining is
        // "budget exhausted", NULL is "we didn't get told".
        let row = row_for(sample("completion", GhApi::Cli, GhOutcome::Ok, None));
        assert_eq!(row.points_cost, None);
        assert_eq!(row.points_remaining, None);
    }
}
