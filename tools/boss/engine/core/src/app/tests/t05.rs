// Unit tests for two isolatable pieces of pure logic in `app.rs` that
// carry documented behavioral contracts but had no direct coverage:
//
//   1. `AppChannelHealth` — the engine→app push-channel liveness state
//      machine (consecutive-failure streak + failure-time snapshot).
//   2. `AppSessionHandle::allocate_request_id` — the monotonic
//      `eng-req-N` request-id allocator.
//
// Both types need no sockets or IO, so these tests are deterministic and
// self-contained. Assertions target the observable contract (returned
// counts, snapshot values, allocated ids) rather than atomic internals.
use super::*;

/// Build a `QueueStats` with a distinctive depth/age so a snapshot can be
/// attributed to the specific failure that produced it.
fn queue_stats(depth: usize, oldest_age_ms: u64) -> QueueStats {
    QueueStats {
        depth,
        priority_depth: 0,
        oldest_age_ms,
        slow: false,
        closed: false,
    }
}

// --- AppChannelHealth: consecutive-failure streak ---

#[test]
fn record_failure_returns_incrementing_consecutive_count() {
    let health = AppChannelHealth::default();
    let stats = queue_stats(10, 100);

    // Each failure returns the *new* running count, starting at 1.
    assert_eq!(health.record_failure(&stats), 1);
    assert_eq!(health.record_failure(&stats), 2);
    assert_eq!(health.record_failure(&stats), 3);
}

#[test]
fn record_success_resets_the_streak_to_zero() {
    let health = AppChannelHealth::default();
    let stats = queue_stats(10, 100);

    health.record_failure(&stats);
    health.record_failure(&stats);
    assert_eq!(health.snapshot().consecutive_failures, 2);

    health.record_success();
    assert_eq!(health.snapshot().consecutive_failures, 0);

    // After recovery the count starts over from 1, not from where it left off.
    assert_eq!(health.record_failure(&stats), 1);
}

#[test]
fn channel_becomes_unhealthy_at_the_streak_threshold() {
    let health = AppChannelHealth::default();
    let stats = queue_stats(10, 100);

    // One failure is below the threshold: still considered healthy.
    let after_one = health.record_failure(&stats);
    assert!(
        after_one < APP_CHANNEL_UNHEALTHY_STREAK,
        "a single failure must stay below the unhealthy streak",
    );

    // The second consecutive failure reaches the threshold: unhealthy.
    let after_two = health.record_failure(&stats);
    assert!(
        after_two >= APP_CHANNEL_UNHEALTHY_STREAK,
        "two consecutive failures must reach the unhealthy streak",
    );
}

#[test]
fn one_success_restores_health_below_the_threshold() {
    let health = AppChannelHealth::default();
    let stats = queue_stats(10, 100);

    // Drive the channel unhealthy.
    health.record_failure(&stats);
    let unhealthy = health.record_failure(&stats);
    assert!(unhealthy >= APP_CHANNEL_UNHEALTHY_STREAK);

    // A single successful round-trip clears the streak back to healthy.
    health.record_success();
    assert!(
        health.snapshot().consecutive_failures < APP_CHANNEL_UNHEALTHY_STREAK,
        "one success must restore health below the unhealthy streak",
    );
}

// --- AppChannelHealth: failure-time snapshot ---

#[test]
fn snapshot_reflects_the_most_recent_failure_stats() {
    let health = AppChannelHealth::default();

    // Fresh health carries no failure stats.
    let initial = health.snapshot();
    assert_eq!(initial.consecutive_failures, 0);
    assert_eq!(initial.last_queue_depth, 0);
    assert_eq!(initial.last_oldest_age_ms, 0);

    health.record_failure(&queue_stats(42, 500));
    let first = health.snapshot();
    assert_eq!(first.last_queue_depth, 42);
    assert_eq!(first.last_oldest_age_ms, 500);

    // A later failure overwrites the snapshot with its own stats; the
    // depth/age track the *most recent* failure, not the worst-ever.
    health.record_failure(&queue_stats(7, 20));
    let second = health.snapshot();
    assert_eq!(second.last_queue_depth, 7);
    assert_eq!(second.last_oldest_age_ms, 20);
    assert_eq!(second.consecutive_failures, 2);
}

#[test]
fn success_clears_the_streak_but_leaves_last_failure_stats() {
    let health = AppChannelHealth::default();

    health.record_failure(&queue_stats(99, 300));
    health.record_success();

    // record_success only touches the streak; the last observed depth/age
    // remain for the health body to report.
    let snap = health.snapshot();
    assert_eq!(snap.consecutive_failures, 0);
    assert_eq!(snap.last_queue_depth, 99);
    assert_eq!(snap.last_oldest_age_ms, 300);
}

// --- AppSessionHandle::allocate_request_id ---

#[test]
fn allocate_request_id_is_monotonic_and_unique() {
    let mut handle = AppSessionHandle::new("sess-1".to_owned(), make_session_sink());

    let ids: Vec<String> = (0..5).map(|_| handle.allocate_request_id()).collect();

    // The documented sequence: eng-req-1, eng-req-2, ...
    assert_eq!(
        ids,
        vec!["eng-req-1", "eng-req-2", "eng-req-3", "eng-req-4", "eng-req-5",]
    );

    // And every id is distinct.
    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "allocated request ids must be unique");
}

#[test]
fn allocate_request_id_sequences_are_independent_per_handle() {
    // Each handle owns its own counter starting at 1, so a fresh session
    // does not inherit another session's position in the sequence.
    let mut a = AppSessionHandle::new("sess-a".to_owned(), make_session_sink());
    let mut b = AppSessionHandle::new("sess-b".to_owned(), make_session_sink());

    assert_eq!(a.allocate_request_id(), "eng-req-1");
    assert_eq!(a.allocate_request_id(), "eng-req-2");

    // `b` is unaffected by activity on `a`.
    assert_eq!(b.allocate_request_id(), "eng-req-1");
}
