//! Circuit breaker for the app session's worker-pane **spawn capability**.
//!
//! ## The incident this guards against
//!
//! On 2026-07-05 the laptop woke from sleep and every worker-pane spawn
//! silently produced no shell for 1.5+ hours: `ghostty_surface_new` returned
//! NULL (no active display), the app parked the pane in a surface-less
//! placeholder, and the engine saw `shell_pid=0` with zero hook events. The
//! [`crate::spawn_ack_sweep`] reaped each execution after 60s and redispatched
//! it, only to fail identically. The per-work-item churn guard in
//! [`crate::orphan_sweep`] could not stop this: it counts terminal executions
//! **per work item**, and the failures were spread across many different work
//! items, so no single item reached its own churn threshold. The fleet churned
//! for hours until the orphan guard eventually parked items one by one.
//!
//! ## What this adds
//!
//! A cross-work-item aggregator. Every never-started spawn — whether inferred
//! by the 60s [`crate::spawn_ack_sweep`] or reported proactively by the app via
//! `ReportWorkerSpawnFailed` (the fast-fail NACK) — feeds
//! [`SpawnHealthTracker::record_failure`]. When
//! [`SPAWN_HEALTH_DISTINCT_WORK_ITEM_THRESHOLD`] **distinct** work items have
//! failed to spawn a shell within [`SPAWN_HEALTH_WINDOW_SECS`], the app
//! session's spawn path — not any one work item — is treated as broken:
//! [`trip_spawn_capability_circuit`] pauses dispatch (so the engine stops
//! burning spawn attempts against a dead app path) and raises a single loud
//! `app_spawn_capability_unhealthy` attention item, instead of independently
//! churning each work item into its own churn guard.
//!
//! Recovery is human-in-the-loop by design: dispatch stays paused until an
//! operator resolves the app (e.g. wakes the display / restarts it) and
//! unpauses. A subsequent successful spawn — a real shell pid reported via
//! `UpdateWorkerShellPid` — calls [`SpawnHealthTracker::record_success`] and
//! resets the breaker so it can trip again on the next outage.

use std::collections::HashSet;
use std::sync::Mutex;

use boss_protocol::CreateAttentionItemInput;

use crate::app::handler_helpers::{
    METADATA_KEY_DISPATCH_PAUSE_ORIGIN, METADATA_KEY_DISPATCH_PAUSED, METADATA_KEY_DISPATCH_PAUSED_SINCE,
};
use crate::coordinator::{DispatchPauseOrigin, ExecutionCoordinator};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::work::WorkDb;

/// Number of **distinct** work items whose spawn must fail inside the window
/// before the breaker trips. Set above the noise of a single flaky spawn but
/// low enough to catch a systemic outage in the first sweep pass — the
/// 2026-07-05 incident failed spawns across three chores plus the automation
/// fleet within minutes.
pub const SPAWN_HEALTH_DISTINCT_WORK_ITEM_THRESHOLD: usize = 3;

/// Sliding window (seconds) over which distinct failing work items are counted.
pub const SPAWN_HEALTH_WINDOW_SECS: i64 = 300;

/// Kind string for the single loud attention item raised when the breaker
/// trips. Stable — external tooling and the app's attention pane pin it.
pub const SPAWN_CAPABILITY_ATTENTION_KIND: &str = "app_spawn_capability_unhealthy";

/// Cross-work-item failure aggregator for the app spawn path.
///
/// Holds a bounded sliding window of `(work_item_id, epoch_secs)` failures and
/// counts distinct work items in-window. Cheap to share (`Arc`): a
/// `std::sync::Mutex` guards the small vector and is never held across an
/// `.await`.
#[derive(Debug)]
pub struct SpawnHealthTracker {
    /// `(work_item_id, epoch_secs)` of recent spawn failures, pruned to the
    /// window on every `record_failure`.
    recent: Mutex<Vec<(String, i64)>>,
    threshold: usize,
    window_secs: i64,
}

impl Default for SpawnHealthTracker {
    fn default() -> Self {
        Self::with_config(SPAWN_HEALTH_DISTINCT_WORK_ITEM_THRESHOLD, SPAWN_HEALTH_WINDOW_SECS)
    }
}

impl SpawnHealthTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with explicit tuning (tests use tight values).
    pub fn with_config(threshold: usize, window_secs: i64) -> Self {
        Self {
            recent: Mutex::new(Vec::new()),
            threshold: threshold.max(1),
            window_secs: window_secs.max(1),
        }
    }

    /// Record one never-started spawn for `work_item_id` at `now_epoch_secs`.
    ///
    /// Prunes failures older than the window, appends this one, and returns
    /// the number of **distinct** work items that have failed in-window when
    /// that count has reached the threshold (a candidate trip); otherwise
    /// `None`.
    ///
    /// Level-triggered: once at/over the threshold this returns `Some` on
    /// *every* subsequent failure. Idempotency of the resulting action lives
    /// in [`trip_spawn_capability_circuit`] (which no-ops when dispatch is
    /// already paused), so the loud signal fires exactly once per outage.
    pub fn record_failure(&self, work_item_id: &str, now_epoch_secs: i64) -> Option<usize> {
        let mut recent = self.recent.lock().unwrap();
        let cutoff = now_epoch_secs - self.window_secs;
        recent.retain(|(_, ts)| *ts >= cutoff);
        recent.push((work_item_id.to_owned(), now_epoch_secs));
        let distinct: HashSet<&str> = recent.iter().map(|(w, _)| w.as_str()).collect();
        let distinct = distinct.len();
        (distinct >= self.threshold).then_some(distinct)
    }

    /// Reset the breaker. Called when a spawn provably worked (a real shell
    /// pid was reported) or a fresh app session registered, so stale
    /// pre-recovery failures no longer count toward a trip.
    pub fn record_success(&self) {
        self.recent.lock().unwrap().clear();
    }

    /// Window length in seconds (for the trip event's `details`).
    pub fn window_secs(&self) -> i64 {
        self.window_secs
    }
}

/// Act on a tripped spawn-capability breaker: pause dispatch and raise the one
/// loud signal.
///
/// Idempotent only once dispatch is already paused with review-exemption OFF
/// (i.e. a prior breaker trip, or a human pause that has already been
/// escalated) — that is a no-op, so repeated failures while the app is
/// wedged never spam attention items. But an *operator* pause exempts
/// `pr_review` executions ([`ExecutionCoordinator::dispatch_pause_exempts_reviews`]),
/// so if the app spawn path is also broken during an operator pause, reviews
/// would otherwise keep dispatching into the dead path and keep tripping this
/// function forever. In that case this still escalates: it re-pauses with
/// [`DispatchPauseOrigin::Breaker`] (clearing the review exemption) and
/// raises the attention item, rather than skipping as a duplicate.
///
/// The pause is persisted through the same metadata keys the human toggle
/// (`handle_set_dispatch_paused`) uses, so an engine restart mid-outage does
/// not resume churning.
///
/// Pauses with [`DispatchPauseOrigin::Breaker`], which — unlike an operator
/// pause — does NOT exempt `pr_review` executions: the app's spawn path
/// itself is broken here, so dispatching a review would just burn another
/// attempt against the same dead path.
pub async fn trip_spawn_capability_circuit(
    work_db: &WorkDb,
    coordinator: &ExecutionCoordinator,
    dispatch_events: &dyn DispatchEventSink,
    tripping_execution_id: &str,
    tripping_work_item_id: &str,
    distinct_work_items: usize,
    now_epoch_secs: i64,
) {
    if coordinator.is_dispatch_paused() && !coordinator.dispatch_pause_exempts_reviews() {
        tracing::debug!(
            tripping_execution_id,
            "spawn-capability breaker: dispatch already paused (non-exempt); skipping duplicate trip",
        );
        return;
    }

    let now_u64 = now_epoch_secs.max(0) as u64;
    coordinator.set_dispatch_paused(true, now_u64, DispatchPauseOrigin::Breaker);
    if let Err(err) = work_db
        .set_metadata(METADATA_KEY_DISPATCH_PAUSED, "1")
        .and_then(|()| work_db.set_metadata(METADATA_KEY_DISPATCH_PAUSED_SINCE, &now_u64.to_string()))
        .and_then(|()| {
            work_db.set_metadata(
                METADATA_KEY_DISPATCH_PAUSE_ORIGIN,
                DispatchPauseOrigin::Breaker.as_metadata_str(),
            )
        })
    {
        tracing::warn!(
            ?err,
            "spawn-capability breaker: failed to persist dispatch pause to state.db — \
             applied in-memory but will revert on engine restart",
        );
    }

    let window_secs = SPAWN_HEALTH_WINDOW_SECS;
    tracing::error!(
        distinct_work_items,
        window_secs,
        tripping_execution_id,
        tripping_work_item_id,
        "app spawn capability unhealthy: {distinct_work_items} distinct work items failed to \
         start a worker shell within {window_secs}s; pausing dispatch and raising attention",
    );

    let body = format!(
        "The Boss app accepted worker-pane spawn requests but no shell ever came up for \
         **{distinct_work_items} different work items** within {window_secs}s — the app session's \
         pane-spawn path is unhealthy (most often `ghostty_surface_new` returning NULL after the \
         machine slept, i.e. no active display).\n\n\
         Dispatch has been **paused** to stop the engine from burning spawn attempts against a \
         dead app path. Each affected execution was reaped (see the `spawn_nack` / \
         `spawn_ack_timeout` events in `dispatch-events/current.jsonl`).\n\n\
         **To recover:** make sure the Boss app is foreground with an active display (wake the \
         screen / reconnect a monitor), confirm new panes spawn, then resume dispatch \
         (`bossctl dispatch resume` or the app's dispatch toggle). The breaker resets automatically \
         once a worker reports a real shell pid."
    );
    if let Err(err) = work_db.create_attention_item(CreateAttentionItemInput {
        body_markdown: body,
        kind: SPAWN_CAPABILITY_ATTENTION_KIND.to_owned(),
        title: "App worker-pane spawn capability is unhealthy".to_owned(),
        execution_id: Some(tripping_execution_id.to_owned()),
        resolved_at: None,
        status: None,
        work_item_id: None,
    }) {
        tracing::warn!(
            ?err,
            tripping_execution_id,
            "spawn-capability breaker: failed to raise attention item (dispatch still paused)",
        );
    }

    dispatch_events
        .emit(
            DispatchEvent::new(Stage::SpawnCapabilityUnhealthy, Outcome::Error, tripping_execution_id)
                .with_work_item(tripping_work_item_id)
                .with_details(serde_json::json!({
                    "distinct_work_items": distinct_work_items,
                    "window_secs": window_secs,
                })),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trips_when_threshold_distinct_work_items_fail_in_window() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        assert_eq!(
            tracker.record_failure("wi-1", 1000),
            None,
            "1 distinct: below threshold"
        );
        assert_eq!(
            tracker.record_failure("wi-2", 1001),
            None,
            "2 distinct: below threshold"
        );
        assert_eq!(
            tracker.record_failure("wi-3", 1002),
            Some(3),
            "3rd distinct work item trips the breaker"
        );
    }

    #[test]
    fn same_work_item_repeated_does_not_trip() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        // One work item failing many times is a per-item problem for the
        // churn guard, not a systemic spawn-capability outage.
        for t in 0..10 {
            assert_eq!(
                tracker.record_failure("wi-hot", 1000 + t),
                None,
                "a single repeatedly-failing work item must never trip the cross-item breaker",
            );
        }
    }

    #[test]
    fn failures_outside_window_do_not_count() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        // Two failures long ago fall out of the window before the later ones.
        assert_eq!(tracker.record_failure("wi-1", 0), None);
        assert_eq!(tracker.record_failure("wi-2", 1), None);
        // These three land ~1000s later; the first two are pruned, so only
        // wi-3/wi-4 are in-window — 2 distinct, still below threshold.
        assert_eq!(tracker.record_failure("wi-3", 1000), None);
        assert_eq!(
            tracker.record_failure("wi-4", 1001),
            None,
            "stale failures outside the window must not push the distinct count over threshold",
        );
        // A third fresh distinct item now trips.
        assert_eq!(tracker.record_failure("wi-5", 1002), Some(3));
    }

    #[test]
    fn success_resets_the_breaker() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        tracker.record_failure("wi-1", 1000);
        tracker.record_failure("wi-2", 1000);
        tracker.record_success();
        // After a successful spawn the window is empty again, so it takes a
        // fresh threshold's worth of distinct failures to re-trip.
        assert_eq!(tracker.record_failure("wi-3", 1001), None);
        assert_eq!(tracker.record_failure("wi-4", 1001), None);
        assert_eq!(tracker.record_failure("wi-5", 1001), Some(3));
    }
}
