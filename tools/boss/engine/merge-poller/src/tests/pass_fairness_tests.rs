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

use crate::work::{LatePrCandidate, ReviewBatchDispatch};
use boss_observation_contracts::ReviewBatchEnqueuer;
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
struct SlowUpkeepPublisher {
    inner: RecordingPublisher,
    upkept: std::sync::Mutex<Vec<String>>,
    /// Delay of each open-PR poll-state publish.
    runtime_delay: Duration,
    /// Delay of each work-item-changed publish (merges).
    changed_delay: Duration,
}

impl Default for SlowUpkeepPublisher {
    fn default() -> Self {
        Self::timed(UPKEEP, Duration::ZERO)
    }
}

impl SlowUpkeepPublisher {
    fn timed(runtime_delay: Duration, changed_delay: Duration) -> Self {
        Self {
            inner: RecordingPublisher::default(),
            upkept: std::sync::Mutex::new(Vec::new()),
            runtime_delay,
            changed_delay,
        }
    }

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
        tokio::time::sleep(self.changed_delay).await;
    }

    async fn publish_work_item_runtime_changed(&self, product_id: &str, work_item_id: &str, reason: &str) {
        self.upkept.lock().unwrap().push(work_item_id.to_owned());
        self.inner
            .publish_work_item_runtime_changed(product_id, work_item_id, reason)
            .await;
        tokio::time::sleep(self.runtime_delay).await;
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
    publisher: &dyn ExecutionPublisher,
    metrics: &Registry,
    state: &PassState,
) -> Option<(SweepOutcome, Vec<PollObservation>)> {
    budgeted_pass_with(db, probe, publisher, None, metrics, state).await
}

async fn budgeted_pass_with(
    db: &WorkDb,
    probe: &StubProbe,
    publisher: &dyn ExecutionPublisher,
    completion_handler: Option<&dyn CompletionRequests>,
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
                completion_handler,
                host: None,
                parked_ids: None,
            },
            state,
            BUDGET,
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

    let _completed = budgeted_pass(&db, &stub, &publisher, &metrics, &state).await;

    assert!(
        is_done(&db, &merged.work_item_id),
        "a merged PR must be recorded even when it is listed behind more open-PR upkeep than the pass has time for",
    );

    let timings = state.trace().timings();
    let terminal = timings
        .phases
        .iter()
        .find(|phase| phase.phase == PassPhase::TerminalWalk)
        .expect("the terminal walk finished");
    assert_eq!((terminal.items_done, terminal.items_total), (1, 1));
    let open = timings
        .phases
        .iter()
        .chain(timings.in_flight.as_ref())
        .find(|phase| phase.phase == PassPhase::OpenWalk)
        .expect("the open walk ran");
    assert_eq!(open.items_total, 8);
    assert!(open.items_done < 8, "{open:?}");
    assert!(
        timings.phases.iter().any(|phase| phase.phase == PassPhase::StrandedCi),
        "the open walk must leave a reserve so later upkeep still runs: {}",
        timings.breakdown(),
    );
    assert!(timings.breakdown().contains("open_walk="), "{}", timings.breakdown());
    assert!(metrics.gauge_value("merge_poller.phase_ms.open_walk").unwrap() > 100_000);
    assert!(metrics.gauge_value("merge_poller.items_unfinished").unwrap() > 0);
    assert_eq!(metrics.counter_value("merge_poller.passes"), Some(1));
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
        let _completed = budgeted_pass(&db, &stub, &publisher, &metrics, &state).await;
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
        let _ = budgeted_pass(&db, &stub, &publisher, &metrics, &fresh).await;
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
        interrupted: false,
        total: BUDGET,
        probe_work: Default::default(),
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
    assert!(issue.body.contains("walks every open candidate"), "{}", issue.body);

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

fn phase_ran(timings: &PassTimings, phase: PassPhase) -> bool {
    timings
        .phases
        .iter()
        .chain(timings.in_flight.as_ref())
        .any(|timing| timing.phase == phase)
}

struct NoopEnqueuer;

#[async_trait]
impl ReviewBatchEnqueuer for NoopEnqueuer {
    async fn enqueue(
        &self,
        _work_db: &WorkDb,
        _work_item_id: &str,
        _repo_remote_url: &str,
        _pr_url: &str,
        _review_pool_size: usize,
    ) -> Result<ReviewBatchDispatch> {
        unreachable!("review batch fanout is off in this fixture")
    }
}

/// Completion handler whose PR recheck, late-PR, and background-nudge
/// paths each take [`UPKEEP`], so those families contend for the budget.
struct SlowRecheckHandler {
    calls: HandlerCalls,
    pending_nudges: Vec<String>,
    enqueuer: NoopEnqueuer,
    /// Whether review-batch fanout is on, which turns the deferred review
    /// admission subphase on; it reads the pool size each time it runs.
    fanout: bool,
}

/// What [`SlowRecheckHandler`] finished, recorded after its delay.
#[derive(Default)]
struct HandlerCalls {
    rechecked: std::sync::Mutex<Vec<String>>,
    late: std::sync::Mutex<Vec<String>>,
    nudges: std::sync::Mutex<Vec<String>>,
    pool_size_reads: std::sync::atomic::AtomicUsize,
}

impl SlowRecheckHandler {
    fn new(pending_nudges: Vec<String>) -> Self {
        Self {
            calls: HandlerCalls::default(),
            pending_nudges,
            enqueuer: NoopEnqueuer,
            fanout: false,
        }
    }

    fn with_review_fanout(mut self) -> Self {
        self.fanout = true;
        self
    }

    /// How many times the review admission subphase actually ran.
    fn admission_runs(&self) -> usize {
        self.calls.pool_size_reads.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait]
impl CompletionRequests for SlowRecheckHandler {
    fn review_guide_source_capture_enabled(&self) -> bool {
        false
    }
    fn pending_background_nudge_execution_ids(&self) -> Vec<String> {
        self.pending_nudges.clone()
    }
    async fn recheck_background_nudge(&self, execution_id: &str) -> Option<StopOutcome> {
        tokio::time::sleep(UPKEEP).await;
        self.calls.nudges.lock().unwrap().push(execution_id.to_owned());
        None
    }
    fn review_batch_fanout_enabled(&self) -> bool {
        self.fanout
    }
    fn review_pool_size(&self) -> usize {
        self.calls
            .pool_size_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        0
    }
    async fn force_release(&self, _execution_id: &str) {}
    async fn force_stop_execution_as(&self, _execution_id: &str, _actor: boss_protocol::StatusActor, _reason: &str) {}
    async fn recheck_for_pr(&self, execution_id: &str) -> StopOutcome {
        tokio::time::sleep(UPKEEP).await;
        self.calls.rechecked.lock().unwrap().push(execution_id.to_owned());
        StopOutcome::AwaitingInput
    }
    async fn recheck_for_pr_late(&self, candidate: &LatePrCandidate) -> StopOutcome {
        tokio::time::sleep(UPKEEP).await;
        self.calls.late.lock().unwrap().push(candidate.execution_id.clone());
        StopOutcome::AwaitingInput
    }
    fn reconcile_review_guide_source_from_probe(
        &self,
        _root_task_id: &str,
        _pr_url: &str,
        _base_sha: &str,
        _head_sha: &str,
        _observation_sequence: i64,
    ) {
    }
    fn speculative_conflict_prediction_enabled(&self) -> bool {
        false
    }
    fn stacked_pr_auto_structuring_enabled(&self) -> bool {
        false
    }
    fn mechanical_rebase_enabled(&self) -> bool {
        false
    }
    fn review_batch_enqueuer(&self) -> &dyn ReviewBatchEnqueuer {
        &self.enqueuer
    }
}

fn waiting_human_execution(db: &WorkDb, product_id: &str, label: &str) -> String {
    let chore = create_test_chore_manual(db, product_id.to_owned(), format!("Recheck {label}"));
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:foo/bar.git")
                .build(),
        )
        .unwrap();
    let (exec, run) = db
        .start_execution_run(&exec.id, "agent-1", "repo-1", "lease-1", "ws-1", "/workspaces/ws-1")
        .unwrap();
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&exec.id)
            .run_id(&run.id)
            .execution_status(ExecutionStatus::WaitingHuman)
            .run_status("completed")
            .build(),
    )
    .unwrap();
    exec.id
}

fn late_pr_execution(db: &WorkDb, product_id: &str, label: &str) -> String {
    let id = waiting_human_execution(db, product_id, label);
    db.mark_execution_redundant(&id).unwrap();
    id
}

/// Slow open-PR upkeep exceeds the reserve (each candidate costs more than
/// the remaining quarter). Upkeep and recovery still complete within a
/// bounded number of rotated passes; completion is recorded after the
/// handler delay, so a dropped in-flight item does not count.
#[tokio::test(start_paused = true)]
async fn slow_open_walk_still_reaches_recheck_late_pr_and_upkeep() {
    let (_dir, db) = open_db();
    let metrics = Registry::new();
    crate::init(&metrics);
    let listed = tracked_prs(&db, 9);
    let product_id = listed[0].product_id.clone();
    for i in 1..=3 {
        waiting_human_execution(&db, &product_id, &format!("live-{i}"));
        late_pr_execution(&db, &product_id, &format!("late-{i}"));
    }
    let stub = StubProbe::new();
    let publisher = SlowUpkeepPublisher::default();
    let handler = SlowRecheckHandler::new(Vec::new()).with_review_fanout();
    let state = PassState::default();
    let mut upkeep_ran = false;

    for ci in [
        OpenPrCiStatus::Clean,
        OpenPrCiStatus::InFlight,
        OpenPrCiStatus::Clean,
        OpenPrCiStatus::InFlight,
        OpenPrCiStatus::Clean,
        OpenPrCiStatus::InFlight,
    ] {
        set_all_open(&stub, &listed, ci);
        let _ = budgeted_pass_with(&db, &stub, &publisher, Some(&handler), &metrics, &state).await;
        upkeep_ran |= phase_ran(&state.trace().timings(), PassPhase::StrandedCi);
    }

    assert!(
        upkeep_ran,
        "local upkeep must run within three rotated passes: {}",
        state.trace().timings().breakdown(),
    );
    assert!(
        handler.admission_runs() > 0,
        "deferred review admission must actually run, not just be entered, within six rotated passes",
    );
    assert!(
        !handler.calls.rechecked.lock().unwrap().is_empty(),
        "pending rechecks must complete within six rotated passes",
    );
    assert!(
        !handler.calls.late.lock().unwrap().is_empty(),
        "late-PR recovery must complete within six rotated passes",
    );
}

/// Slow PR rechecks plus late candidates: late_pr is still serviced because
/// the three per-execution families rotate which one starts after upkeep.
#[tokio::test(start_paused = true)]
async fn slow_rechecks_still_service_late_pr() {
    let (_dir, db) = open_db();
    let metrics = Registry::new();
    crate::init(&metrics);
    let listed = tracked_prs(&db, 1);
    let product_id = listed[0].product_id.clone();
    for i in 1..=4 {
        waiting_human_execution(&db, &product_id, &format!("live-{i}"));
    }
    for i in 1..=3 {
        late_pr_execution(&db, &product_id, &format!("late-{i}"));
    }
    let stub = StubProbe::new();
    set_all_open(&stub, &listed, OpenPrCiStatus::Clean);
    let publisher = RecordingPublisher::default();
    let handler = SlowRecheckHandler::new(Vec::new());
    let state = PassState::default();

    for _ in 0..6 {
        let _ = budgeted_pass_with(&db, &stub, &publisher, Some(&handler), &metrics, &state).await;
    }

    assert!(
        !handler.calls.late.lock().unwrap().is_empty(),
        "late-PR recovery must complete even when rechecks are slow: rechecked={:?} late={:?}",
        handler.calls.rechecked.lock().unwrap(),
        handler.calls.late.lock().unwrap(),
    );
}

/// A completed pass with a non-empty probe batch records the probe as
/// finished, so items_unfinished is zero.
#[tokio::test(start_paused = true)]
async fn a_completed_pass_with_probed_prs_reports_zero_unfinished_items() {
    let (_dir, db) = open_db();
    let metrics = Registry::new();
    crate::init(&metrics);
    let listed = tracked_prs(&db, 3);
    let stub = StubProbe::new();
    set_all_open(&stub, &listed, OpenPrCiStatus::Clean);
    let publisher = RecordingPublisher::default();
    let state = PassState::default();

    let completed = budgeted_pass(&db, &stub, &publisher, &metrics, &state).await;
    assert!(completed.is_some(), "three cheap candidates fit the budget");

    let timings = state.trace().timings();
    assert!(timings.in_flight.is_none(), "{}", timings.breakdown());
    assert_eq!(timings.items_unfinished(), 0, "{}", timings.breakdown());
    let probe = timings
        .phases
        .iter()
        .find(|phase| phase.phase == PassPhase::Probe)
        .expect("probe ran");
    assert_eq!((probe.items_done, probe.items_total), (1, 1), "{probe:?}");
}

/// Representative headless load: fifty tracked PRs, stub probe, local DB
/// only. The open walk observes every candidate inside the unchanged 180 s
/// budget.
#[tokio::test]
async fn a_headless_pass_over_current_pr_counts_finishes_inside_the_budget() {
    let (_dir, db) = open_db();
    let listed = tracked_prs(&db, 50);
    let stub = StubProbe::new();
    set_all_open(&stub, &listed, OpenPrCiStatus::Clean);
    let publisher = RecordingPublisher::default();
    let state = PassState::default();
    let started = std::time::Instant::now();

    let _outcome = run_one_pass_tracked(
        &db,
        stub.as_ref(),
        &publisher,
        SweepHandlers {
            cube_client: None,
            completion_handler: None,
            host: None,
            parked_ids: None,
        },
        &state,
        BUDGET,
    )
    .await;

    let elapsed = started.elapsed();
    assert!(
        elapsed < BUDGET,
        "headless pass over 50 PRs took {elapsed:?}, over the {BUDGET:?} budget"
    );
    let timings = state.trace().timings();
    assert!(timings.in_flight.is_none(), "{}", timings.breakdown());
    assert_eq!(timings.items_unfinished(), 0, "{}", timings.breakdown());
    let open = timings
        .phases
        .iter()
        .find(|phase| phase.phase == PassPhase::OpenWalk)
        .expect("open walk ran");
    assert_eq!(open.items_done, 50, "{open:?}");
}

/// A pass that yields at the open-walk reserve still raises the stall, and a
/// later pass that walks every open candidate clears it.
#[tokio::test(start_paused = true)]
async fn a_reserve_truncated_open_walk_keeps_the_stall_until_a_full_walk() {
    let (_dir, db) = open_db();
    let metrics = Registry::new();
    crate::init(&metrics);
    let listed = tracked_prs(&db, 9);
    let stub = StubProbe::new();
    set_all_open(&stub, &listed, OpenPrCiStatus::Clean);
    let slow = SlowUpkeepPublisher::default();
    let state = PassState::default();
    let signal = detection_stall_signal();

    let completed = budgeted_pass(&db, &stub, &slow, &metrics, &state).await;
    assert!(
        completed.is_some(),
        "the pass yields at the reserve rather than timing out"
    );
    let timings = state.trace().timings();
    assert!(
        timings.open_walk_truncated(),
        "nine slow open PRs cannot all fit before the reserve: {}",
        timings.breakdown(),
    );
    record_pass_result(&signal, true, &timings, BUDGET, 1_000);
    assert!(
        signal.borrow().is_some(),
        "a truncated open walk must keep merge_detection_stalled raised",
    );
    let issue = detection_stall_health_issue(signal.borrow().as_ref().unwrap(), 1_030);
    assert!(
        issue.body.contains("left the rest for the next pass") || issue.body.contains("open PR"),
        "{}",
        issue.body,
    );

    set_all_open(&stub, &listed, OpenPrCiStatus::InFlight);
    let fast = RecordingPublisher::default();
    let completed = budgeted_pass(&db, &stub, &fast, &metrics, &state).await;
    assert!(completed.is_some());
    let timings = state.trace().timings();
    assert!(
        !timings.open_walk_truncated(),
        "a cheap pass must walk every open candidate: {}",
        timings.breakdown(),
    );
    record_pass_result(&signal, true, &timings, BUDGET, 1_100);
    assert!(signal.borrow().is_none(), "a full open walk must clear the stall",);
}

/// Background-nudge IDs rotate and are stamped, so a list whose total cost
/// exceeds one pass still services every ID across passes.
#[tokio::test(start_paused = true)]
async fn slow_background_nudges_service_every_id_across_passes() {
    let (_dir, db) = open_db();
    let metrics = Registry::new();
    crate::init(&metrics);
    let listed = tracked_prs(&db, 1);
    let stub = StubProbe::new();
    set_all_open(&stub, &listed, OpenPrCiStatus::Clean);
    let publisher = RecordingPublisher::default();
    let nudge_ids: Vec<String> = (1..=4).map(|i| format!("nudge-{i}")).collect();
    let handler = SlowRecheckHandler::new(nudge_ids.clone());
    let state = PassState::default();

    for _ in 0..9 {
        let _ = budgeted_pass_with(&db, &stub, &publisher, Some(&handler), &metrics, &state).await;
    }

    let mut seen = handler.calls.nudges.lock().unwrap().clone();
    seen.sort();
    seen.dedup();
    assert_eq!(
        seen, nudge_ids,
        "every background-nudge id must complete across rotated passes, got {seen:?}"
    );
}

/// Every upkeep subphase leads on some pass, so a persistently slow early
/// subphase cannot hold the later ones off indefinitely.
#[test]
fn every_upkeep_subphase_leads_within_one_rotation() {
    use crate::pass::{UpkeepPhase, upkeep_order};
    let leaders: std::collections::HashSet<_> = (0..6).map(|turn| upkeep_order(turn)[0]).collect();
    assert_eq!(leaders.len(), 6, "{leaders:?}");
    for turn in 0..12 {
        let order = upkeep_order(turn);
        let all: std::collections::HashSet<_> = order.iter().copied().collect();
        assert_eq!(all.len(), 6, "turn {turn} must run every subphase exactly once");
    }
    assert!(upkeep_order(5)[0] == UpkeepPhase::ReviewBatchUpkeep);
}

/// The last group in a pass has no later group to leave time for, so only
/// the yield margin bounds it. Eight 20s open PRs cost 160s, between 0.75x
/// and 1x of the 180s budget: the pass where the open walk runs last must
/// walk them all and report no truncation.
#[tokio::test(start_paused = true)]
async fn an_open_walk_that_fits_the_budget_is_not_truncated_when_it_runs_last() {
    let (_dir, db) = open_db();
    let metrics = Registry::new();
    crate::init(&metrics);
    let listed = tracked_prs(&db, 8);
    let stub = StubProbe::new();
    let publisher = SlowUpkeepPublisher::timed(Duration::from_secs(20), Duration::ZERO);
    let state = PassState::default();

    // Pass 0 runs the open walk first, with later groups still to run.
    set_all_open(&stub, &listed, OpenPrCiStatus::Clean);
    let _ = budgeted_pass(&db, &stub, &publisher, &metrics, &state).await;
    assert!(state.trace().timings().open_walk_truncated());

    // Pass 1 runs it last.
    set_all_open(&stub, &listed, OpenPrCiStatus::InFlight);
    let completed = budgeted_pass(&db, &stub, &publisher, &metrics, &state).await;
    assert!(completed.is_some());
    let timings = state.trace().timings();
    assert!(
        !timings.open_walk_truncated(),
        "nothing follows the walk, so it must use the whole budget: {}",
        timings.breakdown(),
    );
    let signal = detection_stall_signal();
    record_pass_result(&signal, true, &timings, BUDGET, 1_000);
    assert!(signal.borrow().is_none(), "a full walk must not raise the stall");
}

/// Upkeep runs before the open walk and its stranded-blocked refresh happens
/// after the snapshot has gone stale: the later open walk must still find
/// its own probe results, observe every PR and keep its adaptive tiers.
#[tokio::test(start_paused = true)]
async fn a_stranded_blocked_reprobe_does_not_discard_the_open_walks_results() {
    let (_dir, db) = open_db();
    let metrics = Registry::new();
    crate::init(&metrics);
    let listed = tracked_prs(&db, 3);
    let product_id = listed[0].product_id.clone();
    let stub = StubProbe::new();
    // The merge takes longer than the snapshot's max age to record.
    let publisher = SlowUpkeepPublisher::timed(Duration::ZERO, Duration::from_secs(61));
    let state = PassState::default();

    // Pass 0 (open walk first) only advances the rotation.
    set_all_open(&stub, &listed, OpenPrCiStatus::Clean);
    let _ = budgeted_pass(&db, &stub, &publisher, &metrics, &state).await;

    let merged_pr = "https://github.com/foo/bar/pull/100";
    chore_in_review_for_product(&db, &product_id, "Merged", merged_pr);
    stub.set(merged_pr, PrLifecycleState::Merged);
    let stranded_pr = "https://github.com/foo/bar/pull/101";
    let stranded = chore_in_review_for_product(&db, &product_id, "Stranded", stranded_pr);
    db.update_work_item(
        &stranded,
        WorkItemPatch {
            status: Some("blocked".into()),
            blocked_reason: Some(String::new()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    {
        let conn = rusqlite::Connection::open(_dir.path().join("state.db")).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        conn.execute(
            "INSERT INTO conflict_resolutions
                 (id, product_id, work_item_id, pr_url, pr_number, head_branch, base_branch, status,
                  revision_task_id, created_at)
             VALUES ('cr-1', ?1, ?2, ?3, 101, 'feature', 'main', 'pending', ?2, '0')",
            rusqlite::params![&product_id, &stranded, stranded_pr],
        )
        .unwrap();
    }
    assert_eq!(db.list_chores_stranded_blocked_remediation().unwrap().len(), 1);
    set_all_open(&stub, &listed, OpenPrCiStatus::InFlight);
    stub.set(stranded_pr, PrLifecycleState::Open(OpenPrStatus::clean()));

    // Pass 1 runs upkeep first, after the slow terminal merge.
    let (_outcome, observations) = budgeted_pass(&db, &stub, &publisher, &metrics, &state)
        .await
        .expect("pass completes");

    let timings = state.trace().timings();
    assert!(
        !timings.open_walk_truncated(),
        "the open walk must observe every open PR: {}",
        timings.breakdown(),
    );
    for candidate in &listed {
        let tier = observations
            .iter()
            .find(|(url, _)| url == &candidate.pr_url)
            .map(|(_, tier)| *tier);
        assert!(
            matches!(tier, Some(Some(_))),
            "{} must keep its adaptive tier: {observations:?}",
            candidate.pr_url,
        );
    }
}
