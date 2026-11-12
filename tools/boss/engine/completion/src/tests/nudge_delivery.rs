//! **Theme: the breaker judges a worker only on nudges the worker was given.**
//!
//! These pin the fix for a parked worker that was failed "after 3 nudges"
//! having been shown none of them. It had declared a wait, the wait lapsed,
//! and the recurring sweep resumed the ladder: the first nudge was written
//! into its pane and started no turn, and the next two were queued behind it
//! and never left the queue. Each was counted when it was queued, so the
//! fourth sweep tripped the breaker with "no new commit, PR, or state
//! change" against a worker that had been asked nothing.
//!
//! Three properties, each with its counterfactual:
//!
//! 1. A nudge that has not reached the worker holds the ladder where it is.
//!    No further nudge is issued behind it and none is counted.
//! 2. A nudge that never arrives is handed back, and a run of failed
//!    deliveries ends the attempt as a delivery failure.
//! 3. A worker that does receive its nudges and does nothing with them is
//!    still stopped at the cap, exactly as before.

use super::*;

const PROBE_ONE: &str = "probe-1";

fn pending(nudge_id: &str) -> NudgeDelivery {
    NudgeDelivery::Pending {
        nudge_id: nudge_id.to_owned(),
    }
}

fn failed(nudge_id: &str) -> NudgeDelivery {
    NudgeDelivery::Failed {
        nudge_id: nudge_id.to_owned(),
        detail: "written into the parked pane and Return pressed again twice, but the driver never started a turn"
            .to_owned(),
    }
}

/// Declare a wait for `execution_id` that has already lapsed, the way the
/// breaker tests model a deadline passing without a wall-clock sleep.
fn lapse_wait(waits: &crate::wait_registry::WaitRegistry, execution_id: &str) {
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    waits
        .declare(execution_id, "soak running".into(), None, 60, now - 61)
        .unwrap();
}

fn nudge_breaker_attention_filed(db: &WorkDb, execution_id: &str) -> bool {
    db.list_attention_items(execution_id)
        .unwrap()
        .iter()
        .any(|item| item.kind == NUDGE_BREAKER_ATTENTION_KIND)
}

/// The incident, sweep for sweep: a wait lapses, the sweep issues the first
/// nudge, that nudge is in flight and unanswered, and every later sweep finds
/// the worker still idle. The breaker must not trip, and must not even
/// advance, on nudges that were never delivered.
#[tokio::test]
async fn sweep_nudges_behind_an_unanswered_nudge_never_advance_or_trip_the_breaker() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let TestHarness {
        handler,
        probes,
        cube,
        pane,
        ..
    } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let waits = Arc::new(crate::wait_registry::WaitRegistry::new());
    let breaker = Arc::new(crate::nudge_breaker::NudgeBreaker::new());
    let handler = handler
        .with_wait_registry(waits.clone())
        .with_nudge_breaker(breaker.clone());

    // The worker declares a wait and ends its turn. Nothing is nudged.
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    waits
        .declare(&execution_id, "soak running".into(), None, 60, now)
        .unwrap();
    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::WorkerWaitPending { .. }
    ));

    // The wait lapses with the worker parked. The sweep issues nudge 1 and
    // asks for it to be delivered.
    lapse_wait(&waits, &execution_id);
    assert!(matches!(
        handler.recheck_background_nudge(&execution_id).await,
        Some(StopOutcome::AwaitingInput)
    ));
    assert_eq!(probes.snapshot().len(), 1);
    assert_eq!(breaker.counts(&execution_id), Some((1, 1)));

    // Nudge 1 was written into the pane and no turn started on it: it holds
    // the run's delivery slot, and nothing queued behind it can go out. A
    // written nudge is settled by its own delivery, not withdrawn.
    probes.set_delivery(pending(PROBE_ONE));
    probes.nudge_in_flight.store(true, std::sync::atomic::Ordering::SeqCst);

    // Far more sweeps than the cap. Before the fix sweeps 2 and 3 each queued
    // and counted a nudge, and sweep 4 failed the execution.
    let sweeps = crate::nudge_breaker::ABSOLUTE_MAX_NUDGES + 2;
    for sweep in 0..sweeps {
        let outcome = handler.recheck_background_nudge(&execution_id).await;
        assert!(
            matches!(outcome, Some(StopOutcome::NudgeDebounced)),
            "sweep {sweep}: an undelivered nudge must hold the ladder, got {outcome:?}",
        );
    }

    assert_eq!(
        probes.snapshot().len(),
        1,
        "no nudge may be queued behind one the worker has not been shown",
    );
    assert_eq!(
        breaker.counts(&execution_id),
        Some((1, 1)),
        "held sweeps must not advance the count",
    );
    assert_eq!(
        probes.deliver_snapshot().len(),
        1 + sweeps as usize,
        "every held sweep must ask for the outstanding nudge to be delivered",
    );
    assert!(
        handler.pending_background_nudge_execution_ids().contains(&execution_id),
        "the ladder must stay scheduled so it resumes once the nudge lands",
    );
    assert!(db.get_execution(&execution_id).unwrap().status.is_live());
    assert!(cube.release_calls.lock().await.is_empty());
    assert!(pane.calls.lock().await.is_empty());
    assert!(!nudge_breaker_attention_filed(&db, &execution_id));
}

/// A nudge issued on a turn boundary is watched until it arrives. The
/// boundary path relies on the worker's reply to produce the next Stop, and a
/// nudge that does not land produces none, so without the watch nothing would
/// ever look at the execution again.
#[tokio::test]
async fn a_boundary_nudge_is_watched_until_it_arrives() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let breaker = Arc::new(crate::nudge_breaker::NudgeBreaker::new());
    let handler = handler.with_nudge_breaker(breaker.clone());

    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::AwaitingInput
    ));
    assert!(
        handler.pending_background_nudge_execution_ids().contains(&execution_id),
        "a nudge issued on a boundary must be watched until it is known to have arrived",
    );

    // Left queued on its own boundary (the run's delivery slot was taken).
    probes.set_delivery(pending(PROBE_ONE));
    assert!(matches!(
        handler.recheck_background_nudge(&execution_id).await,
        Some(StopOutcome::NudgeDebounced)
    ));
    assert_eq!(
        probes.deliver_snapshot(),
        [execution_id.as_str()],
        "an idle worker emits no hook to carry a queued nudge; the sweep must ask for it",
    );

    // It lands. The worker's reply produces the next Stop, so the sweep has
    // nothing left to watch and must not nudge again on its own.
    probes.set_delivery(NudgeDelivery::Settled);
    assert_eq!(handler.recheck_background_nudge(&execution_id).await, None);
    assert!(handler.pending_background_nudge_execution_ids().is_empty());
    assert_eq!(probes.snapshot().len(), 1);
    assert_eq!(breaker.counts(&execution_id), Some((1, 1)));
}

/// A Stop that arrives while the previous nudge is still on its way must not
/// stack another nudge behind it or count one. This is the shape an
/// engine-initiated interrupt produces: the boundary is the engine's own, and
/// the nudge queued at the previous one is still behind the probe being
/// delivered.
#[tokio::test]
async fn a_stop_while_the_previous_nudge_is_undelivered_stacks_and_counts_nothing() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let breaker = Arc::new(crate::nudge_breaker::NudgeBreaker::new());
    let handler = handler.with_nudge_breaker(breaker.clone());

    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::AwaitingInput
    ));
    probes.set_delivery(pending(PROBE_ONE));

    for _ in 0..(crate::nudge_breaker::ABSOLUTE_MAX_NUDGES + 2) {
        assert!(matches!(
            handler.on_stop(&execution_id).await,
            StopOutcome::NudgeDebounced
        ));
    }

    assert_eq!(probes.snapshot().len(), 1);
    assert_eq!(breaker.counts(&execution_id), Some((1, 1)));
    assert!(
        probes.deliver_snapshot().is_empty(),
        "a Stop delivers through its own fan-out; only the sweep asks out of band",
    );
    assert!(db.get_execution(&execution_id).unwrap().status.is_live());
    assert!(!nudge_breaker_attention_filed(&db, &execution_id));
}

/// The counterfactual that must keep holding: a worker that receives every
/// nudge and never changes state is stopped at the cap, for the reason the
/// breaker has always given.
#[tokio::test]
async fn a_worker_that_receives_its_nudges_and_stays_idle_still_trips_at_the_cap() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let TestHarness {
        handler, probes, cube, ..
    } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let breaker = Arc::new(crate::nudge_breaker::NudgeBreaker::new());
    let handler = handler.with_nudge_breaker(breaker.clone());
    probes.set_delivery(NudgeDelivery::Settled);

    for count in 1..=crate::nudge_breaker::DEFAULT_MAX_UNPRODUCTIVE_NUDGES {
        assert!(matches!(
            handler.on_stop(&execution_id).await,
            StopOutcome::AwaitingInput
        ));
        assert_eq!(breaker.counts(&execution_id), Some((count, count)));
    }

    let outcome = handler.on_stop(&execution_id).await;
    let StopOutcome::NudgeBreakerParked { reason } = outcome else {
        panic!("a worker that ignores delivered nudges must be stopped at the cap, got {outcome:?}");
    };
    assert!(
        reason.contains("after 3 nudges: no new commit, PR, or state change"),
        "delivered nudges that changed nothing are the unproductive case: {reason}",
    );
    assert_eq!(
        probes.snapshot().len(),
        crate::nudge_breaker::DEFAULT_MAX_UNPRODUCTIVE_NUDGES as usize,
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    assert!(nudge_breaker_attention_filed(&db, &execution_id));
}

/// Nudges that keep failing to arrive must still end the attempt, and say
/// what actually happened. Reporting an unreachable pane as an unresponsive
/// worker sends whoever reads the failure to the wrong place.
#[tokio::test]
async fn nudges_that_keep_failing_to_arrive_end_the_attempt_as_a_delivery_failure() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let TestHarness {
        handler, probes, cube, ..
    } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let breaker = Arc::new(crate::nudge_breaker::NudgeBreaker::new());
    let handler = handler.with_nudge_breaker(breaker.clone());

    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::AwaitingInput
    ));

    // Each nudge is written, starts no turn, and is settled as failed. The
    // sweep is what notices, because a nudge that did not land produces no
    // Stop. It hands the nudge back and issues the next.
    for attempt in 1..crate::nudge_breaker::DEFAULT_MAX_UNPRODUCTIVE_NUDGES {
        probes.set_delivery(failed(&format!("probe-{attempt}")));
        let outcome = handler.recheck_background_nudge(&execution_id).await;
        assert!(
            matches!(outcome, Some(StopOutcome::AwaitingInput)),
            "failed delivery {attempt} is under the bound: re-issue, got {outcome:?}",
        );
        assert_eq!(breaker.delivery_failures(&execution_id), attempt);
        assert_eq!(
            breaker.counts(&execution_id),
            Some((1, 1)),
            "only the nudge now outstanding is charged; the failed ones were handed back",
        );
    }

    probes.set_delivery(failed("probe-3"));
    let outcome = handler.recheck_background_nudge(&execution_id).await;
    let Some(StopOutcome::NudgeBreakerParked { reason }) = outcome else {
        panic!("a pane that cannot be nudged must still end the attempt, got {outcome:?}");
    };
    assert!(
        reason.contains("auto-nudge delivery failed 3 times in a row"),
        "the failure must name the delivery failure: {reason}",
    );
    assert!(
        reason.contains("the driver never started a turn"),
        "and carry the delivery path's own account of it: {reason}",
    );
    assert!(
        !reason.contains("no new commit, PR, or state change"),
        "a worker that was shown nothing must not be reported as unresponsive: {reason}",
    );
    assert_eq!(
        probes.snapshot().len(),
        crate::nudge_breaker::DEFAULT_MAX_UNPRODUCTIVE_NUDGES as usize,
        "the terminal must not issue another nudge",
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    let items = db.list_attention_items(&execution_id).unwrap();
    let item = items
        .iter()
        .find(|item| item.kind == NUDGE_BREAKER_ATTENTION_KIND)
        .expect("the delivery-failure terminal must be visible");
    assert_eq!(item.title, "Worker failed: auto-nudges could not be delivered");
    assert!(handler.pending_background_nudge_execution_ids().is_empty());
}

/// One failed delivery among delivered nudges is not a run of failures: the
/// replacement arriving ends it.
#[tokio::test]
async fn a_delivered_nudge_ends_a_run_of_failed_deliveries() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let breaker = Arc::new(crate::nudge_breaker::NudgeBreaker::new());
    let handler = handler.with_nudge_breaker(breaker.clone());

    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::AwaitingInput
    ));
    probes.set_delivery(failed(PROBE_ONE));
    assert!(matches!(
        handler.recheck_background_nudge(&execution_id).await,
        Some(StopOutcome::AwaitingInput)
    ));
    assert_eq!(breaker.delivery_failures(&execution_id), 1);

    // The replacement is on its way, then lands; the worker stops again
    // without a PR and is nudged on the strength of a nudge it did receive.
    probes.set_delivery(pending(PROBE_ONE));
    assert!(matches!(
        handler.recheck_background_nudge(&execution_id).await,
        Some(StopOutcome::NudgeDebounced)
    ));
    assert_eq!(breaker.delivery_failures(&execution_id), 1);
    probes.set_delivery(NudgeDelivery::Settled);
    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::AwaitingInput
    ));
    assert_eq!(breaker.delivery_failures(&execution_id), 0);
    assert_eq!(breaker.counts(&execution_id), Some((2, 2)));
}

/// Clock a test moves by hand, for the one bound here that is about time.
#[derive(Clone)]
struct HandClock(Arc<std::sync::Mutex<std::time::Instant>>);

impl HandClock {
    fn new() -> Self {
        Self(Arc::new(std::sync::Mutex::new(std::time::Instant::now())))
    }

    fn advance(&self, by: std::time::Duration) {
        *self.0.lock().expect("HandClock mutex poisoned") += by;
    }

    fn now_fn(&self) -> Arc<dyn Fn() -> std::time::Instant + Send + Sync> {
        let inner = self.0.clone();
        Arc::new(move || *inner.lock().expect("HandClock mutex poisoned"))
    }
}

/// Not counting an undelivered nudge must not become a way to wait forever.
/// A nudge that cannot even be written (the pane refuses every attempt) is
/// given up at the delivery deadline, counted as a delivery failure, and
/// replaced; a run of those ends the attempt like any other delivery failure.
#[tokio::test]
async fn a_nudge_that_can_never_be_written_is_given_up_at_the_delivery_deadline() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let TestHarness {
        handler, probes, cube, ..
    } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let breaker = Arc::new(crate::nudge_breaker::NudgeBreaker::new());
    let clock = HandClock::new();
    let handler = handler.with_nudge_breaker(breaker.clone()).with_now_fn(clock.now_fn());
    let short_of_deadline = crate::nudge_breaker::NUDGE_DELIVERY_DEADLINE - std::time::Duration::from_secs(1);

    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::AwaitingInput
    ));

    let mut outcome = None;
    for attempt in 1..=crate::nudge_breaker::DEFAULT_MAX_UNPRODUCTIVE_NUDGES {
        // Still queued: every sweep's attempt to write it is refused.
        probes.set_delivery(pending(&format!("probe-{attempt}")));
        clock.advance(short_of_deadline);
        assert!(
            matches!(
                handler.recheck_background_nudge(&execution_id).await,
                Some(StopOutcome::NudgeDebounced)
            ),
            "inside the deadline a queued nudge is still on its way",
        );
        assert_eq!(breaker.delivery_failures(&execution_id), attempt - 1);

        clock.advance(std::time::Duration::from_secs(1));
        outcome = handler.recheck_background_nudge(&execution_id).await;
        if attempt < crate::nudge_breaker::DEFAULT_MAX_UNPRODUCTIVE_NUDGES {
            assert!(
                matches!(outcome, Some(StopOutcome::AwaitingInput)),
                "under the bound the nudge is replaced, got {outcome:?}",
            );
            assert_eq!(
                breaker.delivery_failures(&execution_id),
                attempt,
                "giving the nudge up is one delivery failure",
            );
            assert_eq!(breaker.counts(&execution_id), Some((1, 1)));
        }
    }

    let Some(StopOutcome::NudgeBreakerParked { reason }) = outcome else {
        panic!("a pane that can never be written to must still end the attempt, got {outcome:?}");
    };
    assert!(
        reason.contains("auto-nudge delivery failed 3 times in a row")
            && reason.contains("could not be written into the worker's pane"),
        "{reason}",
    );
    assert!(!reason.contains("no new commit, PR, or state change"), "{reason}");
    assert_eq!(
        probes.snapshot().len(),
        crate::nudge_breaker::DEFAULT_MAX_UNPRODUCTIVE_NUDGES as usize,
    );
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
}

/// A nudge the engine discarded before writing it was never shown to the
/// worker, so it is not counted, and nothing failed, so it is not a strike.
#[tokio::test]
async fn a_withdrawn_nudge_is_neither_counted_nor_a_delivery_failure() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let TestHarness { handler, probes, .. } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    let breaker = Arc::new(crate::nudge_breaker::NudgeBreaker::new());
    let handler = handler.with_nudge_breaker(breaker.clone());

    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::AwaitingInput
    ));
    assert_eq!(breaker.counts(&execution_id), Some((1, 1)));

    probes.set_delivery(NudgeDelivery::Withdrawn {
        nudge_id: PROBE_ONE.to_owned(),
    });
    assert!(matches!(
        handler.on_stop(&execution_id).await,
        StopOutcome::AwaitingInput
    ));
    assert_eq!(
        breaker.counts(&execution_id),
        Some((1, 1)),
        "the withdrawn nudge came off the count before its replacement went on",
    );
    assert_eq!(breaker.delivery_failures(&execution_id), 0);
    assert_eq!(probes.snapshot().len(), 2);
}
