//! A detection pass that runs out of time must still record merges, must
//! not strand the same candidates on every pass, and must say where its
//! time went.
//!
//! The failure these guard against: passes were being abandoned at their
//! budget on every cycle, and each one re-listed its candidates oldest
//! `updated_at` first — so the tail it never reached was always the most
//! recently updated rows, among them a PR that had merged half an hour
//! earlier and whose card never left the Merging lane.
//!
//! Time is tokio's virtual clock throughout: "open-PR upkeep is slow" is a
//! publisher that sleeps, not a real stall.

use boss_protocol::FrontendEvent;

use super::*;

/// How long [`SlowUpkeepPublisher`] holds each open-PR poll-state publish.
/// Three of these fit in the 180s budget; the fourth is cut off.
const UPKEEP: Duration = Duration::from_secs(50);
const BUDGET: Duration = Duration::from_secs(180);
const CADENCE: Duration = Duration::from_secs(60);

/// Publisher that makes open-PR upkeep slow: every runtime-class
/// invalidation (the poll-state write `sweep_one` publishes for an open PR
/// whose state changed) records its work item and then takes [`UPKEEP`].
/// Merges publish through `publish_work_item_changed` and stay fast.
#[derive(Default)]
struct SlowUpkeepPublisher {
    inner: RecordingPublisher,
    upkept: std::sync::Mutex<Vec<String>>,
}

impl SlowUpkeepPublisher {
    /// Work items whose upkeep has started since the last call, in order.
    fn take_upkept(&self) -> Vec<String> {
        std::mem::take(&mut *self.upkept.lock().unwrap())
    }
}

#[async_trait]
impl ExecutionPublisher for SlowUpkeepPublisher {
    async fn publish(&self, execution_id: &str, work_item_id: &str, status: &str, reason: &str) {
        self.inner.publish(execution_id, work_item_id, status, reason).await;
    }

    async fn publish_work_item_changed(&self, product_id: &str, work_item_id: &str, reason: &str) {
        self.inner
            .publish_work_item_changed(product_id, work_item_id, reason)
            .await;
    }

    async fn publish_work_item_runtime_changed(&self, product_id: &str, work_item_id: &str, reason: &str) {
        self.upkept.lock().unwrap().push(work_item_id.to_owned());
        self.inner
            .publish_work_item_runtime_changed(product_id, work_item_id, reason)
            .await;
        tokio::time::sleep(UPKEEP).await;
    }

    async fn publish_frontend_event_on_product(&self, product_id: &str, event: FrontendEvent) {
        self.inner.publish_frontend_event_on_product(product_id, event).await;
    }
}

/// `count` `in_review` chores with a PR each, in the order the poller lists
/// them.
fn tracked_prs(db: &WorkDb, count: usize) -> Vec<PendingMergeCheck> {
    let product = create_test_product_with_repo(db, "Fairness", Some("git@github.com:foo/bar.git"));
    for i in 1..=count {
        chore_in_review_for_product(
            db,
            &product.id,
            &format!("Card {i}"),
            &format!("https://github.com/foo/bar/pull/{i}"),
        );
    }
    let listed = db.list_chores_pending_merge_check().unwrap();
    assert_eq!(listed.len(), count);
    listed
}

/// Give every listed PR an open probe whose CI state is `ci`. Alternating
/// it between passes makes each pass's poll-state write a real change, so
/// every walked candidate pays [`UPKEEP`] on every pass.
fn set_all_open(stub: &StubProbe, listed: &[PendingMergeCheck], ci: OpenPrCiStatus) {
    for candidate in listed {
        let mut open = OpenPrStatus::clean();
        open.ci = ci.clone();
        stub.set(&candidate.pr_url, PrLifecycleState::Open(open));
    }
}

fn is_done(db: &WorkDb, work_item_id: &str) -> bool {
    match db.get_work_item(work_item_id).unwrap() {
        WorkItem::Chore(task) => task.status == TaskStatus::Done,
        other => panic!("expected chore, got {other:?}"),
    }
}

/// One budgeted pass, exactly as the loop runs it.
async fn budgeted_pass(
    db: &WorkDb,
    probe: &StubProbe,
    publisher: &SlowUpkeepPublisher,
    metrics: &Registry,
    state: &PassState,
) -> Option<(SweepOutcome, Vec<PollObservation>)> {
    run_pass_within_budget(
        "test",
        BUDGET,
        CADENCE,
        metrics,
        state.trace(),
        run_one_pass_tracked(
            db,
            probe,
            publisher,
            SweepHandlers {
                cube_client: None,
                completion_handler: None,
                host: None,
            },
            state,
        ),
    )
    .await
}

/// The reported failure, end to end. The merged PR is the *last* candidate
/// the poller lists, behind eight open PRs whose upkeep costs more than the
/// whole budget. The pass is abandoned — and the merge is recorded anyway,
/// because terminal facts are walked before any open-PR upkeep.
#[tokio::test(start_paused = true)]
async fn a_merge_listed_last_is_recorded_by_a_pass_that_is_then_abandoned() {
    let (_dir, db) = open_db();
    let metrics = Registry::new();
    crate::init(&metrics);
    let listed = tracked_prs(&db, 9);
    let stub = StubProbe::new();
    set_all_open(&stub, &listed, OpenPrCiStatus::Clean);
    let merged = listed.last().unwrap();
    stub.set(&merged.pr_url, PrLifecycleState::Merged);
    let publisher = SlowUpkeepPublisher::default();
    let state = PassState::default();

    let completed = budgeted_pass(&db, &stub, &publisher, &metrics, &state).await;

    assert!(completed.is_none(), "eight 50s upkeeps cannot fit a 180s budget");
    assert!(
        is_done(&db, &merged.work_item_id),
        "a merged PR must be recorded even when it is listed behind more open-PR upkeep than the pass has time for",
    );

    // The trace survives the dropped pass and names where it was cut off.
    let timings = state.trace().timings();
    let terminal = timings
        .phases
        .iter()
        .find(|phase| phase.phase == PassPhase::TerminalWalk)
        .expect("the terminal walk finished");
    assert_eq!((terminal.items_done, terminal.items_total), (1, 1));
    let cut_off = timings.in_flight.as_ref().expect("the pass was cut off mid-phase");
    assert_eq!(cut_off.phase, PassPhase::OpenWalk);
    assert_eq!(cut_off.items_total, 8);
    assert!(cut_off.items_done < 8, "{cut_off:?}");
    assert!(cut_off.item.is_some(), "the candidate in flight must be named");
    assert!(timings.breakdown().contains("open_walk="), "{}", timings.breakdown());
    assert!(timings.breakdown().contains("[cut off at "), "{}", timings.breakdown());

    // And it is published, so the phase is readable without the log.
    assert_eq!(metrics.gauge_value("merge_poller.pass_duration_ms"), Some(180_000));
    assert!(metrics.gauge_value("merge_poller.phase_ms.open_walk").unwrap() > 100_000);
    assert!(metrics.gauge_value("merge_poller.items_unfinished").unwrap() > 0);
    assert_eq!(metrics.counter_value("merge_poller.passes"), Some(1));
    assert_eq!(metrics.counter_value("merge_poller.pass_timed_out"), Some(1));
}

/// Progress across abandoned passes. Nine open PRs, each costing 50s of
/// upkeep on every pass, against a 180s budget: no single pass can cover
/// them. Each pass must start with what the previous one did not reach, so
/// three passes attend to all nine.
#[tokio::test(start_paused = true)]
async fn successive_abandoned_passes_attend_to_every_open_candidate() {
    let (_dir, db) = open_db();
    let metrics = Registry::new();
    crate::init(&metrics);
    let listed = tracked_prs(&db, 9);
    let stub = StubProbe::new();
    let publisher = SlowUpkeepPublisher::default();
    let state = PassState::default();
    let ci_by_pass = [OpenPrCiStatus::Clean, OpenPrCiStatus::InFlight, OpenPrCiStatus::Clean];

    let mut attended: Vec<Vec<String>> = Vec::new();
    for ci in ci_by_pass {
        set_all_open(&stub, &listed, ci);
        let completed = budgeted_pass(&db, &stub, &publisher, &metrics, &state).await;
        assert!(completed.is_none(), "every pass here runs out of budget");
        attended.push(publisher.take_upkept());
    }

    for (n, pass) in attended.iter().enumerate() {
        assert!(!pass.is_empty(), "pass {n} must make progress: {attended:?}");
    }
    let (first, second) = (&attended[0], &attended[1]);
    assert!(
        second.iter().all(|id| !first.contains(id)),
        "the second pass must start with candidates the first did not reach, not re-walk its head: {attended:?}",
    );
    let all: std::collections::HashSet<&str> = attended.iter().flatten().map(String::as_str).collect();
    assert_eq!(
        all.len(),
        listed.len(),
        "three budget-bound passes must between them attend to every open candidate: {attended:?}",
    );
}

/// The control for the test above: with no memory carried between passes,
/// a budget-bound pass walks the same head every time and the tail is
/// never attended to. This is the starvation the rotation removes.
#[tokio::test(start_paused = true)]
async fn without_carried_state_every_abandoned_pass_walks_the_same_head() {
    let (_dir, db) = open_db();
    let metrics = Registry::new();
    crate::init(&metrics);
    let listed = tracked_prs(&db, 9);
    let stub = StubProbe::new();
    let publisher = SlowUpkeepPublisher::default();

    let mut attended: Vec<Vec<String>> = Vec::new();
    for ci in [OpenPrCiStatus::Clean, OpenPrCiStatus::InFlight] {
        set_all_open(&stub, &listed, ci);
        let fresh = PassState::default();
        assert!(budgeted_pass(&db, &stub, &publisher, &metrics, &fresh).await.is_none());
        attended.push(publisher.take_upkept());
    }

    assert_eq!(attended[0], attended[1], "{attended:?}");
    assert!(attended[0].len() < listed.len());
}

// ── Rotation ───────────────────────────────────────────────────────────

#[test]
fn rotation_orders_never_attended_first_then_least_recently_attended() {
    let mut rotation = Rotation::default();
    rotation.stamp("b");
    rotation.stamp("a");

    let order = rotation.order(vec!["a", "b", "new-1", "new-2"], |key| key);

    assert_eq!(
        order,
        vec!["new-1", "new-2", "b", "a"],
        "never-attended candidates keep list order and go first; the rest follow oldest attention first",
    );
}

/// A candidate is stamped when the pass starts on it. If handling it is
/// what exhausts the budget, it must go to the back of the next pass — not
/// stay at the front and cut every pass off in the same place.
#[test]
fn a_candidate_cut_off_mid_flight_goes_to_the_back_of_the_next_pass() {
    let mut rotation = Rotation::default();
    for key in ["a", "b"] {
        rotation.stamp(key);
    }
    // The pass started on "poison" and was dropped inside it; "c" and "d"
    // were never reached.
    rotation.stamp("poison");

    let order = rotation.order(vec!["a", "b", "poison", "c", "d"], |key| key);

    assert_eq!(order, vec!["c", "d", "a", "b", "poison"]);
}

#[test]
fn rotation_forgets_candidates_that_left_the_list() {
    let mut rotation = Rotation::default();
    for key in ["kept", "merged"] {
        rotation.stamp(key);
    }

    rotation.retain(&std::collections::HashSet::from(["kept"]));

    assert_eq!(rotation.tracked(), 1);
    // A candidate that comes back is new again, and goes first.
    assert_eq!(
        rotation.order(vec!["kept", "merged"], |key| key),
        vec!["merged", "kept"]
    );
}

// ── Phase timing ───────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn a_trace_read_mid_phase_reports_finished_phases_and_the_one_cut_off() {
    let trace = PassTrace::default();
    trace.begin();
    trace.enter(PassPhase::Probe, 3);
    tokio::time::advance(Duration::from_secs(2)).await;
    trace.enter(PassPhase::OpenWalk, 5);
    trace.start_item("chore_a");
    trace.finish_item();
    trace.start_item("chore_b");
    tokio::time::advance(Duration::from_secs(7)).await;

    let timings = trace.timings();

    assert_eq!(timings.total, Duration::from_secs(9));
    assert_eq!(
        timings.breakdown(),
        "probe=2000ms(0/3) open_walk=7000ms(1/5)[cut off at chore_b]",
    );
    assert_eq!(timings.items_done(), 1);
    assert_eq!(timings.items_unfinished(), 3 + 4);

    // Once the pass runs to its end nothing is in flight.
    trace.finish_item();
    trace.end();
    let timings = trace.timings();
    assert!(timings.in_flight.is_none());
    assert_eq!(timings.breakdown(), "probe=2000ms(0/3) open_walk=7000ms(2/5)");

    // A new pass starts from nothing.
    trace.begin();
    assert_eq!(trace.timings(), PassTimings::default());
}

#[test]
fn every_phase_has_a_distinct_name_and_gauge() {
    let metrics = Registry::new();
    crate::init(&metrics);
    let names: std::collections::HashSet<&str> = PassPhase::ALL.iter().map(|phase| phase.as_str()).collect();
    assert_eq!(names.len(), PassPhase::ALL.len());
    for phase in PassPhase::ALL {
        let gauge = format!("merge_poller.phase_ms.{}", phase.as_str());
        assert!(
            metrics.gauge_snapshot_one(&gauge).is_some(),
            "{gauge} must be registered so `bossctl metrics list --prefix merge_poller.phase_ms` shows it",
        );
    }
}

// ── The health signal ──────────────────────────────────────────────────

fn cut_off_in_open_walk() -> PassTimings {
    PassTimings {
        total: BUDGET,
        phases: vec![PhaseTiming {
            phase: PassPhase::Probe,
            elapsed: Duration::from_secs(1),
            items_total: 47,
            items_done: 47,
            item: None,
        }],
        in_flight: Some(PhaseTiming {
            phase: PassPhase::OpenWalk,
            elapsed: Duration::from_secs(179),
            items_total: 46,
            items_done: 12,
            item: Some("chore_x".to_owned()),
        }),
    }
}

/// An abandoned pass raises the signal at once, each further one updates it
/// in place, and the next pass that finishes withdraws it.
#[test]
fn abandoned_passes_raise_a_stall_that_a_completed_pass_clears() {
    let signal = detection_stall_signal();
    let mut changes = signal.subscribe();
    assert!(signal.borrow().is_none());

    record_abandoned_pass(&signal, BUDGET, cut_off_in_open_walk(), 1_000);
    assert!(changes.has_changed().unwrap(), "the first abandoned pass must notify");
    let first = changes.borrow_and_update().clone().expect("stall raised");
    assert_eq!(first.consecutive_abandoned, 1);
    let issue = detection_stall_health_issue(&first, 1_030);
    assert_eq!(issue.kind, DETECTION_STALL_ISSUE_KIND);
    assert_eq!(issue.severity, "warning", "one abandoned pass is a warning");

    record_abandoned_pass(&signal, BUDGET, cut_off_in_open_walk(), 1_240);
    let second = changes.borrow_and_update().clone().expect("stall still up");
    assert_eq!(second.consecutive_abandoned, 2);
    assert_eq!(second.first_abandoned_epoch_s, 1_000, "the run keeps its start");
    assert_eq!(second.latest_abandoned_epoch_s, 1_240);
    let issue = detection_stall_health_issue(&second, 1_300);
    assert_eq!(
        issue.severity, "error",
        "two in a row means the board is minutes behind"
    );
    assert!(issue.title.contains("last 2 detection passes"), "{}", issue.title);
    assert!(issue.body.contains("`open_walk`"), "{}", issue.body);
    assert!(issue.body.contains("12 of 46"), "{}", issue.body);
    assert!(issue.body.contains("180s budget"), "{}", issue.body);
    assert!(issue.body.contains("[cut off at chore_x]"), "{}", issue.body);

    record_completed_pass(&signal);
    assert!(changes.has_changed().unwrap(), "clearing must notify");
    assert!(changes.borrow_and_update().is_none());

    // A healthy pass on a healthy poller is not a change worth a broadcast.
    record_completed_pass(&signal);
    assert!(!changes.has_changed().unwrap());
}

/// An abandoned pass reports no observations. That must leave the adaptive
/// schedule alone: treating it as "the sweep saw no candidates" retires
/// every slot, and the per-PR layer that could still have observed a hot
/// PR's merge goes dark for as long as passes keep being abandoned.
#[test]
fn an_abandoned_pass_leaves_the_adaptive_schedule_in_place() {
    let mut schedule = PrPollSchedule::default();
    let now = Instant::now();
    schedule.apply_sweep_observations(&[("hot".to_owned(), Some(PollTier::Hot))], now, CADENCE);
    assert_eq!(schedule.tracked(), 1);

    assert!(schedule.apply_pass_observations(None, now, CADENCE).is_none());
    assert_eq!(
        schedule.tracked(),
        1,
        "an abandoned pass must not retire adaptive slots"
    );

    // A pass that completed with nothing to observe still retires them.
    assert!(schedule.apply_pass_observations(Some(&[]), now, CADENCE).is_some());
    assert_eq!(schedule.tracked(), 0);
}
