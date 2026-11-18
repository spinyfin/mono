//! Crash and replay tests for the observation projection, over a real on-disk
//! `WorkDb` and the real durable sink. A "crash" is the projector's task being
//! aborted (or its database handle dropped) at a chosen point, then a new
//! projector opening the same file, which is what an engine restart does.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use boss_protocol::{ProposalKind, WorkerEvent};
use boss_protocol_spoke::v1::{self, Command, CommandResult, ResultCode, Run, command, record, spoke_frame};
use boss_protocol_spoke::worker_event;
use boss_spoke_custody::Custody;
use boss_spoke_hub::WorkDbSink;
use boss_spoke_hub::projection::{
    Applied, CommandSender, Decision, EventObservation, ObservationApplier, Origin, PROPOSAL_DECISION, PaneObservation,
    Projector, ProjectorConfig, Proposed, QUARANTINE, TranscriptSource,
};
use boss_spoke_stream::CommandOutcome;
use boss_spoke_stream::store::SpokeObservationSink;
use boss_work_db::{SpokeEffectState, SpokeRunKey, WorkDb};
use prost::Message;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::support::*;

const HOST: &str = "host-a";

fn config() -> ProjectorConfig {
    ProjectorConfig::builder()
        .sweep_interval(Duration::from_millis(40))
        .delivery_interval(Duration::from_millis(40))
        .retry_initial(Duration::from_millis(5))
        .retry_max(Duration::from_millis(20))
        .halt_hold(Duration::from_millis(80))
        .delivery_deadline(Duration::from_millis(500))
        .watermark_wait(Duration::from_millis(600))
        .watermark_poll(Duration::from_millis(5))
        .build()
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Everything the engine's consumers were asked to do, kept outside any one
/// projector so it survives the crash of the projector that did it.
#[derive(Default)]
struct Log {
    events: Vec<String>,
    proposals: Vec<String>,
    panes: Vec<(String, i32)>,
    /// What the applier's own idempotency key made authoritative: one entry per
    /// distinct proposal, however many times the projection applied it.
    authoritative: HashSet<String>,
}

/// The hub transcript copy an event was given and what that file held at the
/// moment the event was dispatched.
type HeldCopy = Option<(std::path::PathBuf, Vec<u8>)>;

/// What the applier saw of transcript custody.
#[derive(Clone, Default)]
struct Custodied {
    sources: Arc<Mutex<Vec<(u64, HeldCopy)>>>,
    attached: Arc<Mutex<Vec<(String, u64)>>>,
    /// The first `transcript_attached` parks forever, as a crash after a chunk
    /// is held and before its record's cursor moves would.
    hang_attach: Arc<AtomicBool>,
    attach_entered: Arc<Notify>,
}

impl Custodied {
    fn sources(&self) -> Vec<(u64, HeldCopy)> {
        self.sources.lock().unwrap().clone()
    }
}

#[derive(Clone, Default, bon::Builder)]
#[builder(on(String, into))]
struct Applier {
    log: Arc<Mutex<Log>>,
    /// Applying a proposal parks forever, as a crash mid-application would.
    hang_proposals: Arc<AtomicBool>,
    entered: Arc<Notify>,
    event_retries: Arc<AtomicU32>,
    block_events: Arc<AtomicBool>,
    release_event: Arc<Notify>,
    custody: Custodied,
}

impl Applier {
    fn log<T>(&self, read: impl FnOnce(&Log) -> T) -> T {
        read(&self.log.lock().unwrap())
    }
}

#[async_trait]
impl ObservationApplier for Applier {
    async fn event(&self, observation: &EventObservation) -> Applied {
        if self
            .event_retries
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
            .is_ok()
        {
            return Applied::Retry("database busy".into());
        }
        let kind = match &observation.event {
            WorkerEvent::Stop { .. } => "stop",
            WorkerEvent::PreToolUse { .. } => "pre_tool_use",
            _ => "other",
        };
        let held = observation
            .transcript
            .as_ref()
            .map(|source| (source.path.clone(), std::fs::read(&source.path).unwrap()));
        self.log
            .lock()
            .unwrap()
            .events
            .push(format!("{}#{kind}", observation.origin.sequence));
        self.custody
            .sources
            .lock()
            .unwrap()
            .push((observation.origin.sequence, held));
        if self.block_events.load(Ordering::SeqCst) {
            self.entered.notify_one();
            self.release_event.notified().await;
        }
        Applied::Done
    }

    async fn transcript_attached(&self, origin: &Origin, source: &TranscriptSource) -> Applied {
        if self.custody.hang_attach.swap(false, Ordering::SeqCst) {
            self.custody.attach_entered.notify_one();
            std::future::pending::<()>().await;
        }
        self.custody
            .attached
            .lock()
            .unwrap()
            .push((origin.run.execution_id.clone(), source.materialized));
        Applied::Done
    }

    async fn proposal(&self, origin: &Origin, _: ProposalKind, _: &Value, key: &str) -> Proposed {
        self.log
            .lock()
            .unwrap()
            .proposals
            .push(format!("{}#{key}", origin.sequence));
        self.entered.notify_one();
        if self.hang_proposals.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        self.log.lock().unwrap().authoritative.insert(key.to_owned());
        Proposed::Decided(Decision {
            accepted: true,
            reason: String::new(),
            result: json!({"state": "applied"}),
        })
    }

    async fn pane(&self, observation: &PaneObservation) -> Applied {
        self.log
            .lock()
            .unwrap()
            .panes
            .push((observation.origin.run.execution_id.clone(), observation.pane.life));
        Applied::Done
    }
}

/// Records every command and answers from a script.
#[derive(Default)]
struct Sender {
    sent: Mutex<Vec<(String, Command)>>,
    script: Mutex<Vec<Result<CommandResult, CommandOutcome>>>,
    unavailable: Mutex<HashSet<String>>,
}

impl Sender {
    fn will(&self, answer: Result<CommandResult, CommandOutcome>) {
        self.script.lock().unwrap().push(answer);
    }

    fn sent(&self) -> Vec<(String, Command)> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl CommandSender for Sender {
    async fn send(&self, host_id: &str, command: Command, _: Duration) -> Result<CommandResult, CommandOutcome> {
        if self.unavailable.lock().unwrap().contains(host_id) {
            return Err(CommandOutcome::Unavailable);
        }
        let id = command.command_id.clone();
        self.sent.lock().unwrap().push((host_id.to_owned(), command));
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            return Ok(CommandResult {
                command_id: id,
                code: ResultCode::Ok as i32,
                ..Default::default()
            });
        }
        script.remove(0)
    }
}

fn refused(reason: &str) -> Result<CommandResult, CommandOutcome> {
    Ok(CommandResult {
        code: ResultCode::Rejected as i32,
        reason: reason.into(),
        ..Default::default()
    })
}

fn projector(db: &WorkDb, applier: &Applier, sender: &Arc<Sender>, wake: Arc<Notify>) -> Projector {
    Projector::new(db.clone(), Arc::new(applier.clone()), sender.clone(), wake, config())
}

fn frame(record: &v1::Record) -> Vec<u8> {
    v1::SpokeFrame {
        body: Some(spoke_frame::Body::Record(record.clone())),
    }
    .encode_to_vec()
}

fn record_of(sequence: u64, run: &Run, body: record::Body) -> v1::Record {
    v1::Record {
        sequence,
        observed: Some(stamp()),
        run: Some(run.clone()),
        body: Some(body),
    }
}

fn proposal_record(sequence: u64, run: &Run, key: &str) -> v1::Record {
    record_of(
        sequence,
        run,
        record::Body::Proposal(v1::Proposal {
            proposal_id: key.into(),
            kind: v1::ProposalKind::RunDone as i32,
            payload_contract: "proposal-json/1".into(),
            canonical_json: serde_json::to_vec(&json!({
                "kind": ProposalKind::RunDone,
                "payload": {"outcome": "delivered", "summary": "queued while offline"},
                "idempotency_key": key,
            }))
            .unwrap(),
            capture_epoch: 1,
            ..Default::default()
        }),
    )
}

fn stop_record(sequence: u64, run: &Run) -> v1::Record {
    let mut wire = worker_event::to_wire(&WorkerEvent::Stop {
        session_id: "session".into(),
        stop_hook_active: false,
        stop_reason: boss_protocol::StopReason::Completed,
    });
    if let Some(v1::worker_event::Event::Stop(stop)) = &mut wire.event {
        stop.capture_epoch = 1;
    }
    record_of(sequence, run, record::Body::Event(wire))
}

fn tool_record(sequence: u64, run: &Run) -> v1::Record {
    record_of(
        sequence,
        run,
        record::Body::Event(worker_event::to_wire(&WorkerEvent::PreToolUse {
            session_id: "session".into(),
            tool_name: "Bash".into(),
            tool_input: json!({"command": "ls"}),
        })),
    )
}

fn capture_record(sequence: u64, run: &Run) -> v1::Record {
    record_of(
        sequence,
        run,
        record::Body::Capture(v1::PaneCapture {
            subscription_id: "view".into(),
            ..Default::default()
        }),
    )
}

fn pane_record(sequence: u64, run: &Run, life: v1::PaneLife) -> v1::Record {
    record_of(
        sequence,
        run,
        record::Body::Pane(v1::PaneState {
            life: life as i32,
            session: "worker".into(),
            pane_id: "%1".into(),
            pid: 42,
            boot_id: "boot".into(),
            ..Default::default()
        }),
    )
}

/// A host's side of the stream: it opens its journal and commits frames
/// through the same durable sink the stream manager uses.
struct Spoke {
    sink: WorkDbSink,
    session: boss_spoke_stream::store::Session,
}

impl Spoke {
    async fn connect(db: &WorkDb, wake: Option<Arc<Notify>>) -> Self {
        Self::connect_host(db, HOST, wake).await
    }

    async fn connect_host(db: &WorkDb, host: &str, wake: Option<Arc<Notify>>) -> Self {
        let sink = match wake {
            Some(wake) => WorkDbSink::with_wake(db.clone(), wake),
            None => WorkDbSink::new(db.clone()),
        };
        let mut hello = hello(host);
        hello.last_record = db.spoke_journal(host).unwrap().map_or(0, |j| j.received);
        let admission = sink.open(&hello, &selection()).await.expect("open journal");
        Self {
            sink,
            session: session(host, admission.generation),
        }
    }

    /// Commit a record; the sink acknowledges receipt without applying it.
    async fn send(&self, record: &v1::Record) -> u64 {
        self.sink
            .commit(&self.session, &frame(record), record)
            .await
            .expect("durable receipt")
            .through
    }

    async fn send_raw(&self, sequence: u64, run: &Run, bytes: &[u8]) -> u64 {
        let record = record_of(sequence, run, record::Body::Gap(v1::Gap::default()));
        self.sink
            .commit(&self.session, bytes, &record)
            .await
            .expect("durable receipt")
            .through
    }
}

struct World {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.db");
        Self { _dir: dir, path }
    }

    fn custody(&self, db: &WorkDb) -> Arc<Custody> {
        Arc::new(Custody::new(db.clone(), self._dir.path().join("custody")))
    }

    /// Open the database file, as an engine start does.
    fn open(&self) -> WorkDb {
        open_at(&self.path)
    }
}

fn open_at(path: &Path) -> WorkDb {
    let db = work_db_at(path.to_path_buf());
    spoke_ready(&db, HOST);
    db
}

fn assign(db: &WorkDb, execution: &str) -> (SpokeRunKey, Run) {
    let owned = db.reserve_spoke_assignment(execution, HOST).unwrap();
    let key = run_key(&owned);
    let run = proto_run(&key);
    (key, run)
}

fn decisions(db: &WorkDb) -> Vec<boss_work_db::SpokeEffect> {
    // The connection is released before the effects are read back: a second
    // `connect` while one is held waits for the first.
    let keys: Vec<String> = {
        let conn = db.connect().unwrap();
        let mut statement = conn
            .prepare("SELECT effect_key FROM spoke_effects WHERE effect_kind = 'proposal_decision' ORDER BY sequence")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    keys.iter()
        .map(|key| db.spoke_effect(HOST, PROPOSAL_DECISION, key).unwrap().unwrap())
        .collect()
}

fn applied(db: &WorkDb) -> u64 {
    db.spoke_projection_position(HOST).unwrap().unwrap().applied
}

#[tokio::test]
async fn a_queued_done_received_before_a_crash_is_applied_once_and_before_the_stop_that_follows_it() {
    let world = World::new();
    let wake = Arc::new(Notify::new());
    let run;
    {
        // The hub receives the spoke's offline work, acknowledges it, and
        // crashes before applying any of it.
        let db = world.open();
        run = assign(&db, "exec_1").1;
        let spoke = Spoke::connect(&db, Some(wake.clone())).await;
        assert_eq!(spoke.send(&proposal_record(1, &run, "key-1")).await, 1);
        assert_eq!(spoke.send(&stop_record(2, &run)).await, 2);
        let position = db.spoke_projection_position(HOST).unwrap().unwrap();
        assert_eq!(
            (position.received, position.applied),
            (2, 0),
            "receipt is independent of application"
        );
    }

    let db = world.open();
    let applier = Applier::default();
    let sender = Arc::new(Sender::default());
    let running = tokio::spawn(projector(&db, &applier, &sender, wake.clone()).run());
    eventually("both records to be applied", || applied(&db) == 2).await;

    assert_eq!(applier.log(|l| l.proposals.clone()), ["1#key-1"]);
    assert_eq!(applier.log(|l| l.events.clone()), ["2#stop"]);
    assert_eq!(applier.log(|l| l.authoritative.len()), 1);
    // The decision is owed to the spoke and delivered once it is reachable.
    eventually("the decision to be delivered", || sender.sent().len() == 1).await;
    let (host, sent) = sender.sent().remove(0);
    assert_eq!(host, HOST);
    assert_eq!(sent.run, Some(run.clone()));
    assert!(
        sent.not_after_unix_ms > 0,
        "a decision is valid for a while, not instantly expired"
    );
    let Some(command::Body::ProposalDecision(decision)) = sent.body else {
        panic!("a proposal decision was expected")
    };
    assert_eq!(
        (decision.proposal_id.as_str(), decision.code),
        ("key-1", ResultCode::Ok as i32)
    );
    eventually("the decision to be settled", || {
        decisions(&db)[0].state == SpokeEffectState::Done
    })
    .await;
    running.abort();

    // A further restart finds nothing to do: still one authoritative effect.
    let again = Applier::default();
    let restarted = projector(&world.open(), &again, &sender, wake);
    restarted.drain().await;
    assert!(again.log(|l| l.proposals.is_empty() && l.events.is_empty()));
    assert_eq!(sender.sent().len(), 1, "a settled decision is never sent again");
}

#[tokio::test]
async fn a_crash_after_applying_and_before_the_cursor_moves_repeats_the_application_and_keeps_one_effect() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&proposal_record(1, &run, "key-1")).await;
    spoke.send(&stop_record(2, &run)).await;

    // The engine applies the proposal and dies before its cursor moves.
    let applier = Applier::default();
    applier.hang_proposals.store(true, Ordering::SeqCst);
    let sender = Arc::new(Sender::default());
    let running = tokio::spawn(projector(&db, &applier, &sender, Arc::new(Notify::new())).run());
    applier.entered.notified().await;
    running.abort();
    let _ = running.await;
    assert_eq!(
        applied(&db),
        0,
        "the cursor does not move before the record's effects are recorded"
    );
    assert!(decisions(&db).is_empty());
    assert!(
        applier.log(|l| l.events.is_empty()),
        "the stop behind the proposal waits for it"
    );

    // After the restart the same record is applied again. The applier's own
    // idempotency (the proposal key) keeps that one authoritative effect.
    applier.hang_proposals.store(false, Ordering::SeqCst);
    let db = world.open();
    projector(&db, &applier, &sender, Arc::new(Notify::new())).drain().await;
    assert_eq!(applier.log(|l| l.proposals.clone()), ["1#key-1", "1#key-1"]);
    assert_eq!(applier.log(|l| l.authoritative.len()), 1);
    assert_eq!(decisions(&db).len(), 1);
    assert_eq!(applier.log(|l| l.events.clone()), ["2#stop"]);
    assert_eq!(applied(&db), 2);
}

#[tokio::test]
async fn a_crash_after_the_cursor_moves_never_applies_the_record_again() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&proposal_record(1, &run, "key-1")).await;
    let applier = Applier::default();
    let sender = Arc::new(Sender::default());
    // The spoke is unreachable, so the decision stays owed across the crash.
    sender.will(Err(CommandOutcome::Unavailable));
    projector(&db, &applier, &sender, Arc::new(Notify::new())).drain().await;
    assert_eq!(applied(&db), 1);
    assert_eq!(decisions(&db)[0].state, SpokeEffectState::Pending);
    drop(db);

    let db = world.open();
    let restarted = projector(&db, &applier, &sender, Arc::new(Notify::new()));
    restarted.drain().await;
    assert_eq!(
        applier.log(|l| l.proposals.len()),
        1,
        "an applied record is not applied again"
    );
    // What was still owed is delivered, once, by the restarted projector.
    assert_eq!(decisions(&db)[0].state, SpokeEffectState::Done);
    assert_eq!(decisions(&db).len(), 1);
}

#[tokio::test]
async fn repeated_observations_of_the_same_thing_have_one_effect() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    // The same proposal queued under three journal records, the same pane
    // seen three times, and a retransmission of an already-received frame.
    spoke.send(&proposal_record(1, &run, "key-1")).await;
    spoke.send(&pane_record(2, &run, v1::PaneLife::Alive)).await;
    spoke.send(&proposal_record(3, &run, "key-1")).await;
    spoke.send(&pane_record(4, &run, v1::PaneLife::Alive)).await;
    assert_eq!(
        spoke.send(&proposal_record(1, &run, "key-1")).await,
        4,
        "a retransmission changes nothing"
    );
    spoke.send(&proposal_record(5, &run, "key-1")).await;
    spoke.send(&pane_record(6, &run, v1::PaneLife::Alive)).await;

    let applier = Applier::default();
    let sender = Arc::new(Sender::default());
    projector(&db, &applier, &sender, Arc::new(Notify::new())).drain().await;
    assert_eq!(applied(&db), 6);
    assert_eq!(
        applier.log(|l| l.proposals.clone()),
        ["1#key-1"],
        "later records of a decided proposal apply nothing"
    );
    assert_eq!(decisions(&db).len(), 1);
    assert_eq!(sender.sent().len(), 1, "one decision is sent, not one per record");
    let observation = db.spoke_pane_observation("exec_1").unwrap().unwrap();
    assert_eq!(
        (observation.life, observation.source_sequence),
        (v1::PaneLife::Alive as i32, 6)
    );
}

#[tokio::test]
async fn a_pane_exit_is_kept_with_its_exit_code_and_the_newest_observation_wins() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&pane_record(1, &run, v1::PaneLife::Alive)).await;
    let mut exited = pane_record(2, &run, v1::PaneLife::Exited);
    if let Some(record::Body::Pane(pane)) = &mut exited.body {
        pane.exit_code = Some(3);
    }
    spoke.send(&exited).await;
    spoke.send(&pane_record(3, &run, v1::PaneLife::Indeterminate)).await;
    let applier = Applier::default();
    projector(&db, &applier, &Arc::new(Sender::default()), Arc::new(Notify::new()))
        .drain()
        .await;
    // Indeterminate is the newest word, and it is not a death.
    let observation = db.spoke_pane_observation("exec_1").unwrap().unwrap();
    assert_eq!(observation.life, v1::PaneLife::Indeterminate as i32);
    assert_eq!(
        applier.log(|l| l.panes.iter().map(|(_, life)| *life).collect::<Vec<_>>()),
        [
            v1::PaneLife::Alive as i32,
            v1::PaneLife::Exited as i32,
            v1::PaneLife::Indeterminate as i32
        ]
    );
}

#[tokio::test]
async fn a_busy_cursor_commit_retries_only_the_commit_and_does_not_reapply_the_event() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&tool_record(1, &run)).await;
    let applier = Applier::default();
    // A real database error on the cursor write: triggers abort every cursor
    // write, so the production error classification and retry arm run.
    db.connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_cursor BEFORE UPDATE ON spoke_projection
             BEGIN SELECT RAISE(ABORT, 'database is locked'); END;
             CREATE TRIGGER fail_cursor_insert BEFORE INSERT ON spoke_projection
             BEGIN SELECT RAISE(ABORT, 'database is locked'); END;",
        )
        .unwrap();
    let projector = projector(&db, &applier, &Arc::new(Sender::default()), Arc::new(Notify::new()));
    let drain = tokio::spawn({
        let projector = projector.clone();
        async move { projector.drain().await }
    });
    eventually("the event to be applied", || {
        !applier.log(|l| l.events.clone()).is_empty()
    })
    .await;
    // Let several failed commit attempts go by; each would re-apply the event
    // if the retry covered the whole record.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(applied(&db), 0, "the failing cursor commit has not advanced");
    db.connect()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_cursor; DROP TRIGGER fail_cursor_insert;")
        .unwrap();
    drain.await.unwrap();
    assert_eq!(
        applier.log(|l| l.events.clone()),
        ["1#pre_tool_use"],
        "a transient cursor-commit failure must not re-run the event applier"
    );
    assert_eq!(applied(&db), 1);
}

#[tokio::test]
async fn a_record_that_cannot_be_applied_yet_holds_the_cursor_and_the_order() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&tool_record(1, &run)).await;
    spoke.send(&stop_record(2, &run)).await;
    let applier = Applier::default();
    applier.event_retries.store(3, Ordering::SeqCst);
    projector(&db, &applier, &Arc::new(Sender::default()), Arc::new(Notify::new()))
        .drain()
        .await;
    // The first event was refused three times; nothing behind it ran early.
    assert_eq!(applier.log(|l| l.events.clone()), ["1#pre_tool_use", "2#stop"]);
    assert_eq!(applied(&db), 2);
}

#[tokio::test]
async fn an_unreadable_frame_is_quarantined_and_the_cursor_moves_on() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send_raw(1, &run, b"not a protobuf frame \xff\xff").await;
    spoke.send(&stop_record(2, &run)).await;
    let applier = Applier::default();
    projector(&db, &applier, &Arc::new(Sender::default()), Arc::new(Notify::new()))
        .drain()
        .await;
    let quarantined = db.spoke_effect(HOST, QUARANTINE, "1").unwrap().unwrap();
    assert_eq!(quarantined.state, SpokeEffectState::Failed);
    assert!(quarantined.last_error.unwrap().contains("decode"));
    assert_eq!(applier.log(|l| l.events.clone()), ["2#stop"]);
    assert_eq!(applied(&db), 2);
    assert!(
        db.pending_spoke_effects(10).unwrap().is_empty(),
        "a quarantine is never retried"
    );
}

#[tokio::test]
async fn a_superseded_attempt_is_not_applied_but_its_proposal_gets_a_refusal() {
    let world = World::new();
    let db = world.open();
    let (old_key, old_run) = assign(&db, "exec_1");
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&tool_record(1, &old_run)).await;
    spoke.send(&proposal_record(2, &old_run, "stale-key")).await;
    spoke.send(&pane_record(3, &old_run, v1::PaneLife::Exited)).await;
    // The hub gives up on the first attempt and starts a second one.
    db.resolve_spoke_assignment("exec_1", old_key.assignment_generation, "abandoned")
        .unwrap();
    let (new_key, _) = assign(&db, "exec_1");
    assert!(new_key.assignment_generation > old_key.assignment_generation);

    let applier = Applier::default();
    let sender = Arc::new(Sender::default());
    projector(&db, &applier, &sender, Arc::new(Notify::new())).drain().await;

    assert!(applier.log(|l| l.events.is_empty() && l.proposals.is_empty() && l.panes.is_empty()));
    assert!(db.spoke_pane_observation("exec_1").unwrap().is_none());
    let sent = sender.sent();
    let Some(command::Body::ProposalDecision(decision)) = &sent[0].1.body else {
        panic!("the old attempt's proposal is answered")
    };
    assert_eq!(decision.code, ResultCode::Rejected as i32);
    assert!(decision.reason.contains("superseded"), "{}", decision.reason);
}

#[tokio::test]
async fn a_proposal_the_hub_cannot_read_is_refused_by_decision_not_dropped() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    let mut record = proposal_record(1, &run, "key-1");
    if let Some(record::Body::Proposal(proposal)) = &mut record.body {
        proposal.payload_contract = "not-a-real-contract/1".into();
        proposal.kind = v1::ProposalKind::OtherValidated as i32;
    }
    spoke.send(&record).await;
    let applier = Applier::default();
    let sender = Arc::new(Sender::default());
    projector(&db, &applier, &sender, Arc::new(Notify::new())).drain().await;
    assert!(applier.log(|l| l.proposals.is_empty()));
    let sent = sender.sent();
    let Some(command::Body::ProposalDecision(decision)) = &sent[0].1.body else {
        panic!("a refusal is a decision")
    };
    assert_eq!(decision.code, ResultCode::Rejected as i32);
    assert!(decision.reason.contains("not-a-real-contract/1"));
}

#[tokio::test]
async fn a_decision_waits_for_the_host_and_a_refusing_spoke_gets_it_under_a_fresh_id() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&proposal_record(1, &run, "key-1")).await;
    let sender = Arc::new(Sender::default());
    sender.will(Err(CommandOutcome::Unavailable));
    sender.will(Err(CommandOutcome::OutcomeUncertain));
    sender.will(refused("assignment generation mismatch"));
    let applier = Applier::default();
    let projector = projector(&db, &applier, &sender, Arc::new(Notify::new()));

    projector.drain().await;
    assert_eq!(
        decisions(&db)[0].state,
        SpokeEffectState::Pending,
        "an unreachable host is not an answer"
    );
    for _ in 0..3 {
        projector.drain().await;
    }
    let ids: Vec<String> = sender.sent().into_iter().map(|(_, c)| c.command_id).collect();
    // Unavailable and uncertain retry the same command; a definite refusal
    // moves to a fresh id, and the next attempt succeeds under it.
    assert_eq!(ids.len(), 4);
    assert_eq!(ids[0], ids[1]);
    assert_eq!(ids[1], ids[2]);
    assert_eq!(ids[3], format!("{}-r1", ids[0]));
    let decision = &decisions(&db)[0];
    assert_eq!((decision.state, decision.rejections), (SpokeEffectState::Done, 1));
    assert_eq!(applier.log(|l| l.proposals.len()), 1);
}

#[tokio::test]
async fn a_spoke_that_keeps_refusing_a_decision_stores_it_failed_and_never_retries() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    Spoke::connect(&db, None)
        .await
        .send(&proposal_record(1, &run, "key-1"))
        .await;
    let sender = Arc::new(Sender::default());
    for _ in 0..3 {
        sender.will(refused("unknown proposal"));
    }
    let projector = projector(&db, &Applier::default(), &sender, Arc::new(Notify::new()));
    for _ in 0..4 {
        projector.drain().await;
    }
    let decision = &decisions(&db)[0];
    assert_eq!(decision.state, SpokeEffectState::Failed);
    assert!(decision.last_error.as_ref().unwrap().contains("unknown proposal"));
    assert_eq!(sender.sent().len(), 3, "a failed effect is not retried");
}

#[tokio::test]
async fn missing_stored_evidence_halts_the_host_instead_of_skipping_it() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&tool_record(1, &run)).await;
    spoke.send(&stop_record(2, &run)).await;
    spoke.send(&tool_record(3, &run)).await;
    // The stored frame for 2 is lost (pruned past an unapplied cursor).
    db.connect()
        .unwrap()
        .execute("DELETE FROM spoke_ingress WHERE sequence = 2", [])
        .unwrap();
    let applier = Applier::default();
    projector(&db, &applier, &Arc::new(Sender::default()), Arc::new(Notify::new()))
        .drain()
        .await;
    assert_eq!(applier.log(|l| l.events.clone()), ["1#pre_tool_use"]);
    assert_eq!(
        applied(&db),
        1,
        "the cursor stops at the gap; nothing behind it is applied"
    );
}

#[tokio::test]
async fn an_unretained_capture_does_not_halt_later_records() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&tool_record(1, &run)).await;
    spoke.send(&capture_record(2, &run)).await;
    spoke.send(&proposal_record(3, &run, "done-after-capture")).await;
    let applier = Applier::default();
    projector(&db, &applier, &Arc::new(Sender::default()), Arc::new(Notify::new()))
        .drain()
        .await;
    assert_eq!(applier.log(|l| l.events.clone()), ["1#pre_tool_use"]);
    assert_eq!(applier.log(|l| l.proposals.clone()), ["3#done-after-capture"]);
    assert_eq!(applied(&db), 3);
    assert!(
        db.spoke_projection_status().unwrap()[0].halt_reason.is_none(),
        "a capture tombstone is skipped, not a missing retained frame"
    );
}

#[tokio::test]
async fn a_trailing_unretained_capture_advances_the_cursor() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&proposal_record(1, &run, "done-before-capture")).await;
    spoke.send(&capture_record(2, &run)).await;
    let applier = Applier::default();
    projector(&db, &applier, &Arc::new(Sender::default()), Arc::new(Notify::new()))
        .drain()
        .await;
    assert_eq!(applier.log(|l| l.proposals.clone()), ["1#done-before-capture"]);
    assert_eq!(applied(&db), 2);
    assert!(db.spoke_projection_status().unwrap()[0].halt_reason.is_none());
}

#[tokio::test]
async fn an_unretained_capture_is_skippable_after_a_restart() {
    let world = World::new();
    let run;
    {
        let db = world.open();
        run = assign(&db, "exec_1").1;
        let spoke = Spoke::connect(&db, None).await;
        spoke.send(&tool_record(1, &run)).await;
        spoke.send(&capture_record(2, &run)).await;
        spoke.send(&proposal_record(3, &run, "done-after-restart")).await;
    }
    let db = world.open();
    let applier = Applier::default();
    projector(&db, &applier, &Arc::new(Sender::default()), Arc::new(Notify::new()))
        .drain()
        .await;
    assert_eq!(applier.log(|l| l.events.clone()), ["1#pre_tool_use"]);
    assert_eq!(applier.log(|l| l.proposals.clone()), ["3#done-after-restart"]);
    assert_eq!(applied(&db), 3);
}

#[tokio::test]
async fn a_committed_record_wakes_the_projector_without_waiting_for_a_sweep() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let wake = Arc::new(Notify::new());
    let applier = Applier::default();
    // A sweep so slow that only the sink's wake can explain a prompt apply.
    let slow = ProjectorConfig::builder()
        .sweep_interval(Duration::from_secs(3600))
        .delivery_interval(Duration::from_secs(3600))
        .build();
    let running = tokio::spawn(
        Projector::new(
            db.clone(),
            Arc::new(applier.clone()),
            Arc::new(Sender::default()),
            wake.clone(),
            slow,
        )
        .run(),
    );
    // Let the first (immediate) sweep pass with nothing to do.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let spoke = Spoke::connect(&db, Some(wake)).await;
    spoke.send(&stop_record(1, &run)).await;
    eventually("the wake to apply the record", || applied(&db) == 1).await;
    assert_eq!(applier.log(|l| l.events.clone()), ["1#stop"]);
    running.abort();
}

#[tokio::test]
async fn an_unreachable_host_does_not_starve_later_hosts_of_decisions() {
    let world = World::new();
    let db = world.open();
    db.add_host("host-b", "worker@b", 1, &[]).unwrap();
    crate::support::spoke_ready(&db, "host-b");
    let run_a = assign(&db, "exec_a").1;
    let owned_b = db.reserve_spoke_assignment("exec_b", "host-b").unwrap();
    let run_b = crate::support::proto_run(&crate::support::run_key(&owned_b));
    let spoke_a = Spoke::connect(&db, None).await;
    let spoke_b = Spoke::connect_host(&db, "host-b", None).await;
    for sequence in 1..=3 {
        spoke_a
            .send(&proposal_record(sequence, &run_a, &format!("a-{sequence}")))
            .await;
    }
    spoke_b.send(&proposal_record(1, &run_b, "b-1")).await;
    let sender = Arc::new(Sender::default());
    sender.unavailable.lock().unwrap().insert(HOST.to_owned());
    let projector = Projector::new(
        db.clone(),
        Arc::new(Applier::default()),
        sender.clone(),
        Arc::new(Notify::new()),
        ProjectorConfig::builder()
            .batch(2)
            .retry_initial(Duration::from_millis(5))
            .retry_max(Duration::from_millis(20))
            .delivery_deadline(Duration::from_millis(500))
            .build(),
    );
    projector.drain().await;
    assert!(
        sender.sent().iter().any(|(host, _)| host == "host-b"),
        "host-b must receive its decision while host-a is unreachable; sent={:?}",
        sender.sent()
    );
    let key = format!("{}:b-1", run_b.run_id);
    let settled = db.spoke_effect("host-b", PROPOSAL_DECISION, &key).unwrap().unwrap();
    assert_eq!(settled.state, SpokeEffectState::Done);
}

// ── Transcript and artifact custody ─────────────────────────────────────────

fn custody_projector(
    db: &WorkDb,
    applier: &Applier,
    sender: &Arc<Sender>,
    wake: Arc<Notify>,
    custody: &Arc<Custody>,
) -> Projector {
    Projector::with_custody(
        db.clone(),
        Arc::new(applier.clone()),
        sender.clone(),
        wake,
        config(),
        custody.clone(),
    )
}

fn transcript_record(sequence: u64, run: &Run, generation: &str, offset: u64, data: &[u8]) -> v1::Record {
    use sha2::{Digest, Sha256};
    record_of(
        sequence,
        run,
        record::Body::Transcript(v1::TranscriptDelta {
            generation: generation.into(),
            offset,
            data: data.to_vec(),
            sha256: Sha256::digest(data).to_vec(),
            eof: false,
        }),
    )
}

/// A turn-ending Stop recorded with the transcript size the spoke saw.
fn stop_at(sequence: u64, run: &Run, generation: &str, through: u64) -> v1::Record {
    stop_at_epoch(sequence, run, generation, through, 1)
}

fn stop_at_epoch(sequence: u64, run: &Run, generation: &str, through: u64, epoch: u64) -> v1::Record {
    let mut wire = worker_event::to_wire(&WorkerEvent::Stop {
        session_id: "session".into(),
        stop_hook_active: false,
        stop_reason: boss_protocol::StopReason::Completed,
    });
    if let Some(v1::worker_event::Event::Stop(stop)) = &mut wire.event {
        stop.transcript_generation = generation.into();
        stop.transcript_through = through;
        stop.capture_epoch = epoch;
    }
    record_of(sequence, run, record::Body::Event(wire))
}

fn gap_record(sequence: u64, run: &Run, resource: &str, from: u64, reason: &str) -> v1::Record {
    record_of(
        sequence,
        run,
        record::Body::Gap(v1::Gap {
            resource: resource.into(),
            from_offset: from,
            through_offset: from,
            reason: reason.into(),
        }),
    )
}

fn concluded(sequence: u64, run: &Run, ids: &[&str]) -> v1::Record {
    concluded_epoch(sequence, run, 1, ids)
}

fn concluded_epoch(sequence: u64, run: &Run, epoch: u64, ids: &[&str]) -> v1::Record {
    gap_record(
        sequence,
        run,
        &format!("output:concluded:{epoch}"),
        0,
        &serde_json::to_string(ids).unwrap(),
    )
}

fn artifact_record(
    sequence: u64,
    run: &Run,
    id: &str,
    kind: v1::ArtifactKind,
    data: &[u8],
    metadata: &[u8],
) -> v1::Record {
    use sha2::{Digest, Sha256};
    record_of(
        sequence,
        run,
        record::Body::Artifact(v1::ArtifactChunk {
            artifact_id: id.into(),
            kind: kind as i32,
            offset: 0,
            data: data.to_vec(),
            sha256: Sha256::digest(data).to_vec(),
            eof: true,
            total_bytes: data.len() as u64,
            metadata_json: metadata.to_vec(),
        }),
    )
}

fn failed_effects(db: &WorkDb, kind: &str) -> Vec<boss_work_db::SpokeEffect> {
    let keys: Vec<String> = {
        let conn = db.connect().unwrap();
        let mut statement = conn
            .prepare("SELECT effect_key FROM spoke_effects WHERE effect_kind = ?1 AND state = 'failed'")
            .unwrap();
        statement
            .query_map([kind], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    keys.iter()
        .map(|key| db.spoke_effect(HOST, kind, key).unwrap().unwrap())
        .collect()
}

#[tokio::test]
async fn a_stop_whose_bytes_are_behind_it_gets_them_without_deadlocking_on_its_own_watermark() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    // The spoke numbers the Stop at once, with the size the transcript had; the
    // bytes follow as send credit allows.
    spoke.send(&stop_at(1, &run, "0-1", 10)).await;
    spoke.send(&transcript_record(2, &run, "0-1", 0, b"01234")).await;
    spoke.send(&transcript_record(3, &run, "0-1", 5, b"56789")).await;
    spoke.send(&concluded(4, &run, &[])).await;

    let applier = Applier::default();
    let custody = world.custody(&db);
    custody_projector(
        &db,
        &applier,
        &Arc::new(Sender::default()),
        Arc::new(Notify::new()),
        &custody,
    )
    .drain()
    .await;

    assert_eq!(applied(&db), 4);
    let sources = applier.custody.sources();
    assert_eq!(sources.len(), 1);
    let (sequence, held) = &sources[0];
    assert_eq!(*sequence, 1);
    let (path, bytes) = held.as_ref().expect("the Stop is given the hub's copy");
    assert_eq!(
        bytes, b"0123456789",
        "every byte the Stop named was on the hub when it was dispatched"
    );
    assert!(path.starts_with(custody.root()));
    assert!(failed_effects(&db, "transcript_evidence_unavailable").is_empty());
    // The run is pointed at its hub copy once, and again for each replayed chunk.
    assert!(
        applier
            .custody
            .attached
            .lock()
            .unwrap()
            .iter()
            .all(|(execution, _)| execution == "exec_1")
    );
}

#[tokio::test]
async fn a_boundary_whose_bytes_never_arrive_goes_ahead_and_records_the_evidence_as_unavailable() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&transcript_record(1, &run, "0-1", 0, b"0123")).await;
    spoke.send(&concluded(2, &run, &[])).await;
    spoke.send(&stop_at(3, &run, "0-1", 100)).await;
    spoke.send(&tool_record(4, &run)).await;

    let applier = Applier::default();
    let custody = world.custody(&db);
    custody_projector(
        &db,
        &applier,
        &Arc::new(Sender::default()),
        Arc::new(Notify::new()),
        &custody,
    )
    .drain()
    .await;

    assert_eq!(
        applied(&db),
        4,
        "the host is not stranded behind a transcript that never comes"
    );
    assert_eq!(applier.log(|l| l.events.clone()), ["3#stop", "4#pre_tool_use"]);
    assert!(
        applier.custody.sources()[0].1.is_none(),
        "no copy is claimed to be complete"
    );
    let alerts = failed_effects(&db, "transcript_evidence_unavailable");
    assert_eq!(alerts.len(), 1);
    assert!(alerts[0].last_error.as_deref().unwrap().contains("4 of 100"));
}

#[tokio::test]
async fn a_gap_the_spoke_declared_is_unavailable_evidence_and_not_an_alert() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&transcript_record(1, &run, "0-1", 0, b"01234")).await;
    spoke
        .send(&gap_record(2, &run, "transcript:0-1", 5, "source replaced"))
        .await;
    spoke.send(&concluded(3, &run, &[])).await;
    spoke.send(&stop_at(4, &run, "0-1", 9)).await;

    let applier = Applier::default();
    let custody = world.custody(&db);
    custody_projector(
        &db,
        &applier,
        &Arc::new(Sender::default()),
        Arc::new(Notify::new()),
        &custody,
    )
    .drain()
    .await;

    assert_eq!(applied(&db), 4);
    assert!(applier.custody.sources()[0].1.is_none());
    assert!(failed_effects(&db, "transcript_evidence_unavailable").is_empty());
    let row = db.spoke_custody("exec_1", 1, "transcript:0-1").unwrap().unwrap();
    assert_eq!(row.state, boss_work_db::CustodyState::Unavailable);
    assert_eq!(row.materialized, 5, "the bytes that did arrive are kept");
}

#[tokio::test]
async fn a_crash_after_a_chunk_is_held_and_before_the_cursor_moves_replays_without_duplicating_bytes() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&transcript_record(1, &run, "0-1", 0, b"abc")).await;
    spoke.send(&transcript_record(2, &run, "0-1", 3, b"def")).await;

    let applier = Applier::default();
    applier.custody.hang_attach.store(true, Ordering::SeqCst);
    let custody = world.custody(&db);
    let sender = Arc::new(Sender::default());
    let running = tokio::spawn(custody_projector(&db, &applier, &sender, Arc::new(Notify::new()), &custody).run());
    applier.custody.attach_entered.notified().await;
    running.abort();
    let _ = running.await;
    assert_eq!(applied(&db), 0, "the cursor does not move before the record is applied");
    let row = db.spoke_custody("exec_1", 1, "transcript:0-1").unwrap().unwrap();
    assert_eq!(row.materialized, 3, "the chunk was durably held before the crash");

    // The restart meets both records again.
    let db = world.open();
    let custody = world.custody(&db);
    custody_projector(&db, &applier, &sender, Arc::new(Notify::new()), &custody)
        .drain()
        .await;
    assert_eq!(applied(&db), 2);
    let row = db.spoke_custody("exec_1", 1, "transcript:0-1").unwrap().unwrap();
    assert_eq!(row.materialized, 6);
    assert_eq!(std::fs::read(custody.root().join(&row.path)).unwrap(), b"abcdef");
}

#[tokio::test]
async fn a_resource_the_hub_cannot_trust_raises_one_alert_and_the_host_keeps_projecting() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&transcript_record(1, &run, "0-1", 0, b"abcd")).await;
    // A hole, then more of the same resource, then unrelated progress.
    spoke.send(&transcript_record(2, &run, "0-1", 20, b"zz")).await;
    spoke.send(&transcript_record(3, &run, "0-1", 4, b"ef")).await;
    spoke.send(&tool_record(4, &run)).await;

    let applier = Applier::default();
    let custody = world.custody(&db);
    custody_projector(
        &db,
        &applier,
        &Arc::new(Sender::default()),
        Arc::new(Notify::new()),
        &custody,
    )
    .drain()
    .await;

    assert_eq!(applied(&db), 4);
    assert_eq!(applier.log(|l| l.events.clone()), ["4#pre_tool_use"]);
    let alerts = failed_effects(&db, "custody_halt");
    assert_eq!(
        alerts.len(),
        1,
        "later records of a halted resource repeat the alert, not add to it"
    );
    assert!(
        alerts[0]
            .last_error
            .as_deref()
            .unwrap()
            .contains("expected bytes from offset 4")
    );
    let row = db.spoke_custody("exec_1", 1, "transcript:0-1").unwrap().unwrap();
    assert_eq!((row.state, row.materialized), (boss_work_db::CustodyState::Halted, 4));
}

#[tokio::test]
async fn artifacts_and_output_gaps_are_held_by_run_identity_and_survive_replay() {
    use sha2::{Digest, Sha256};
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    let whole = br#"{"findings":[]}"#;
    let digest: String = Sha256::digest(whole).iter().map(|b| format!("{b:02x}")).collect();
    let id = format!("1:{digest}");
    let chunk = |offset: usize, len: usize, sequence: u64| {
        let data = &whole[offset..offset + len];
        record_of(
            sequence,
            &run,
            record::Body::Artifact(v1::ArtifactChunk {
                artifact_id: id.clone(),
                kind: v1::ArtifactKind::ReviewResult as i32,
                offset: offset as u64,
                data: data.to_vec(),
                sha256: Sha256::digest(data).to_vec(),
                eof: offset + len == whole.len(),
                total_bytes: whole.len() as u64,
                metadata_json: br#"{"output_kind":"review-result"}"#.to_vec(),
            }),
        )
    };
    spoke.send(&chunk(0, 6, 1)).await;
    spoke.send(&chunk(6, whole.len() - 6, 2)).await;
    spoke
        .send(&gap_record(3, &run, "output:capture", 0, "the spool had no room"))
        .await;

    let applier = Applier::default();
    let custody = world.custody(&db);
    let sender = Arc::new(Sender::default());
    custody_projector(&db, &applier, &sender, Arc::new(Notify::new()), &custody)
        .drain()
        .await;
    // A second pass over the same records, as after a restart, changes nothing.
    sqlite_rewind(&db);
    custody_projector(&db, &applier, &sender, Arc::new(Notify::new()), &custody)
        .drain()
        .await;

    let rows = db.spoke_custody_for_execution("exec_1").unwrap();
    let artifact = rows.iter().find(|r| r.kind == "artifact").unwrap();
    assert_eq!(artifact.state, boss_work_db::CustodyState::Complete);
    let destination = world._dir.path().join("out").join("review.json");
    let kind = boss_spoke_custody::StructuredOutputKind::ReviewResult;
    assert_eq!(
        custody.collect_output("exec_1", 1, kind, &destination).unwrap(),
        boss_spoke_custody::Collected::Installed
    );
    assert_eq!(std::fs::read(destination).unwrap(), whole);
    assert!(failed_effects(&db, "custody_halt").is_empty());
}

#[tokio::test]
async fn another_run_keeps_projecting_while_a_completion_waits_for_bytes() {
    let world = World::new();
    let db = world.open();
    let run_a = assign(&db, "exec_a").1;
    let run_b = assign(&db, "exec_b").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&concluded(1, &run_a, &[])).await;
    spoke.send(&stop_at(2, &run_a, "0-1", 10)).await;
    spoke.send(&tool_record(3, &run_b)).await;

    let applier = Applier::default();
    let custody = world.custody(&db);
    let wake = Arc::new(Notify::new());
    let running = tokio::spawn(
        Projector::with_custody(
            db.clone(),
            Arc::new(applier.clone()),
            Arc::new(Sender::default()),
            wake.clone(),
            ProjectorConfig::builder()
                .sweep_interval(Duration::from_millis(40))
                .delivery_interval(Duration::from_millis(40))
                .retry_initial(Duration::from_millis(5))
                .retry_max(Duration::from_millis(20))
                .halt_hold(Duration::from_millis(80))
                .watermark_wait(Duration::from_secs(5))
                .watermark_poll(Duration::from_millis(5))
                .build(),
            custody.clone(),
        )
        .run(),
    );
    eventually("the other run's observation is applied while the Stop waits", || {
        applier.log(|l| l.events == ["3#pre_tool_use"]) && applied(&db) == 3
    })
    .await;
    assert!(
        !applier.custody.sources().iter().any(|(sequence, _)| *sequence == 2),
        "the Stop is still parked"
    );

    spoke.send(&transcript_record(4, &run_a, "0-1", 0, b"0123456789")).await;
    wake.notify_waiters();
    eventually("the parked Stop applies once its bytes arrive", || {
        applier.log(|l| l.events == ["3#pre_tool_use", "2#stop"])
    })
    .await;
    let sources = applier.custody.sources();
    let held = sources
        .iter()
        .find(|(sequence, _)| *sequence == 2)
        .and_then(|(_, held)| held.as_ref())
        .expect("hub copy");
    assert_eq!(held.1, b"0123456789");
    running.abort();
    let _ = running.await;
}

#[tokio::test]
async fn a_named_output_arriving_after_stop_is_collectable_at_dispatch() {
    use sha2::{Digest, Sha256};
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    let whole = br#"{"findings":[]}"#;
    let digest: String = Sha256::digest(whole).iter().map(|b| format!("{b:02x}")).collect();
    let id = format!("{}:{digest}", v1::ArtifactKind::ReviewResult as i32);

    spoke.send(&transcript_record(1, &run, "0-1", 0, b"hello")).await;
    spoke.send(&concluded(2, &run, &[&id])).await;
    spoke.send(&stop_at(3, &run, "0-1", 5)).await;

    let applier = Applier::default();
    let custody = world.custody(&db);
    let wake = Arc::new(Notify::new());
    let running =
        tokio::spawn(custody_projector(&db, &applier, &Arc::new(Sender::default()), wake.clone(), &custody).run());
    eventually("the Stop is parked until the named artifact exists", || {
        applied(&db) == 3
    })
    .await;
    assert!(
        applier.log(|l| l.events.is_empty()),
        "Stop is not dispatched with a missing named output"
    );

    spoke
        .send(&artifact_record(
            4,
            &run,
            &id,
            v1::ArtifactKind::ReviewResult,
            whole,
            br#"{"output_kind":"review-result"}"#,
        ))
        .await;
    wake.notify_waiters();
    eventually("Stop dispatches once the named artifact is complete", || {
        applier.log(|l| l.events == ["3#stop"])
    })
    .await;
    running.abort();
    let _ = running.await;

    let destination = world._dir.path().join("out").join("review.json");
    assert_eq!(
        custody
            .collect_output(
                "exec_1",
                1,
                boss_spoke_custody::StructuredOutputKind::ReviewResult,
                &destination
            )
            .unwrap(),
        boss_spoke_custody::Collected::Installed
    );
}

#[tokio::test]
async fn a_stop_with_no_transcript_watermark_still_waits_for_a_named_output() {
    use sha2::{Digest, Sha256};
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    let whole = br#"{"pr_url":"https://github.com/example/repo/pull/1"}"#;
    let digest: String = Sha256::digest(whole).iter().map(|b| format!("{b:02x}")).collect();
    let id = format!("{}:{digest}", v1::ArtifactKind::PrUrl as i32);

    spoke.send(&concluded(1, &run, &[&id])).await;
    spoke.send(&stop_record(2, &run)).await;

    let applier = Applier::default();
    let custody = world.custody(&db);
    let wake = Arc::new(Notify::new());
    let running =
        tokio::spawn(custody_projector(&db, &applier, &Arc::new(Sender::default()), wake.clone(), &custody).run());
    eventually("cursor advanced with the Stop parked", || applied(&db) == 2).await;
    assert!(applier.log(|l| l.events.is_empty()));

    spoke
        .send(&artifact_record(3, &run, &id, v1::ArtifactKind::PrUrl, whole, b"{}"))
        .await;
    wake.notify_waiters();
    eventually("Stop dispatches after the named PR-url artifact completes", || {
        applier.log(|l| l.events == ["2#stop"])
    })
    .await;
    running.abort();
    let _ = running.await;
    let destination = world._dir.path().join("pr.json");
    assert_eq!(
        custody
            .collect_output(
                "exec_1",
                1,
                boss_spoke_custody::StructuredOutputKind::PrUrl,
                &destination
            )
            .unwrap(),
        boss_spoke_custody::Collected::Installed
    );
}

#[tokio::test]
async fn a_done_proposal_waits_for_a_named_output_arriving_later() {
    use sha2::{Digest, Sha256};
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    let whole = br#"{"pr_url":"https://github.com/example/repo/pull/1"}"#;
    let digest: String = Sha256::digest(whole).iter().map(|b| format!("{b:02x}")).collect();
    let id = format!("{}:{digest}", v1::ArtifactKind::PrUrl as i32);

    spoke.send(&transcript_record(1, &run, "0-1", 0, b"hello")).await;
    spoke.send(&concluded(2, &run, &[&id])).await;
    let mut done = proposal_record(3, &run, "done-1");
    if let Some(record::Body::Proposal(proposal)) = &mut done.body {
        proposal.transcript_generation = "0-1".into();
        proposal.transcript_through = 5;
    }
    spoke.send(&done).await;

    let applier = Applier::default();
    let custody = world.custody(&db);
    let wake = Arc::new(Notify::new());
    let running =
        tokio::spawn(custody_projector(&db, &applier, &Arc::new(Sender::default()), wake.clone(), &custody).run());
    eventually("cursor advanced with the done proposal parked", || applied(&db) == 3).await;
    assert!(
        applier.log(|l| l.proposals.is_empty()),
        "done is not applied with a missing named output"
    );

    spoke
        .send(&artifact_record(4, &run, &id, v1::ArtifactKind::PrUrl, whole, b"{}"))
        .await;
    wake.notify_waiters();
    eventually("done applies once the named artifact is complete", || {
        applier.log(|l| l.proposals == ["3#done-1"])
    })
    .await;
    running.abort();
    let _ = running.await;
    let destination = world._dir.path().join("pr.json");
    assert_eq!(
        custody
            .collect_output(
                "exec_1",
                1,
                boss_spoke_custody::StructuredOutputKind::PrUrl,
                &destination
            )
            .unwrap(),
        boss_spoke_custody::Collected::Installed
    );
}

/// Move the projection cursor back to the start, as if the cursor commit of
/// every record had been lost.
fn sqlite_rewind(db: &WorkDb) {
    let conn = db.connect().unwrap();
    conn.execute("UPDATE spoke_projection SET applied = 0", []).unwrap();
}

fn stop_epoch(sequence: u64, run: &Run, epoch: u64) -> v1::Record {
    let mut record = stop_at(sequence, run, "0-1", 5);
    if let Some(record::Body::Event(event)) = &mut record.body
        && let Some(v1::worker_event::Event::Stop(stop)) = &mut event.event
    {
        stop.capture_epoch = epoch;
    }
    record
}

#[tokio::test]
async fn second_turn_waits_for_its_own_capture_and_serializes_concurrent_settlers() {
    use sha2::{Digest, Sha256};
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    let custody = world.custody(&db);
    let applier = Applier::default();
    let sender = Arc::new(Sender::default());
    let wake = Arc::new(Notify::new());
    let projector = custody_projector(&db, &applier, &sender, wake.clone(), &custody);
    spoke.send(&transcript_record(1, &run, "0-1", 0, b"hello")).await;
    spoke.send(&stop_epoch(2, &run, 1)).await;
    spoke.send(&gap_record(3, &run, "output:concluded:1", 0, "[]")).await;
    projector.drain().await;
    assert_eq!(applier.log(|l| l.events.clone()), ["2#stop"]);
    spoke.send(&stop_epoch(4, &run, 2)).await;
    let running = tokio::spawn(projector.clone().run());
    eventually("second Stop parked despite the first empty capture", || {
        applied(&db) == 4
    })
    .await;
    assert_eq!(applier.log(|l| l.events.clone()), ["2#stop"]);
    let whole = br#"{"findings":[]}"#;
    let digest: String = Sha256::digest(whole).iter().map(|b| format!("{b:02x}")).collect();
    let id = format!("{}:{digest}", v1::ArtifactKind::ReviewResult as i32);
    spoke
        .send(&gap_record(
            5,
            &run,
            "output:concluded:2",
            0,
            &serde_json::to_string(&[&id]).unwrap(),
        ))
        .await;
    wake.notify_waiters();
    eventually("marker projected before artifact", || applied(&db) == 5).await;
    assert_eq!(applier.log(|l| l.events.clone()), ["2#stop"]);
    applier.block_events.store(true, Ordering::SeqCst);
    spoke
        .send(&artifact_record(
            6,
            &run,
            &id,
            v1::ArtifactKind::ReviewResult,
            whole,
            br#"{"output_kind":"review-result"}"#,
        ))
        .await;
    wake.notify_waiters();
    applier.entered.notified().await;
    let destination = world._dir.path().join("review.json");
    assert_eq!(
        custody
            .collect_output(
                "exec_1",
                1,
                boss_spoke_custody::StructuredOutputKind::ReviewResult,
                &destination
            )
            .unwrap(),
        boss_spoke_custody::Collected::Installed
    );
    assert_eq!(std::fs::read(destination).unwrap(), whole);
    // This independent drain competes with the blocked sweep/host drain.
    spoke.send(&tool_record(7, &run)).await;
    projector.drain().await;
    assert_eq!(applier.log(|l| l.events.clone()), ["2#stop", "4#stop"]);
    applier.block_events.store(false, Ordering::SeqCst);
    applier.release_event.notify_one();
    eventually("settlement committed", || {
        db.pending_spoke_observations().unwrap().is_empty()
    })
    .await;
    assert_eq!(
        applier.log(|l| l.events.clone()),
        ["2#stop", "4#stop", "7#pre_tool_use"]
    );
    running.abort();
    let _ = running.await;
}

#[tokio::test]
async fn parked_stop_and_done_are_fenced_after_reassignment_and_restart() {
    let world = World::new();
    let db = world.open();
    let (key, run) = assign(&db, "exec_1");
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&stop_epoch(1, &run, 1)).await;
    let mut done = proposal_record(2, &run, "old-done");
    if let Some(record::Body::Proposal(proposal)) = &mut done.body {
        proposal.capture_epoch = 1;
    }
    spoke.send(&done).await;
    let applier = Applier::default();
    let sender = Arc::new(Sender::default());
    let custody = world.custody(&db);
    let running = tokio::spawn(custody_projector(&db, &applier, &sender, Arc::new(Notify::new()), &custody).run());
    eventually("both boundaries parked", || applied(&db) == 2).await;
    running.abort();
    let _ = running.await;
    db.resolve_spoke_assignment("exec_1", key.assignment_generation, "abandoned")
        .unwrap();
    assign(&db, "exec_1");
    custody_projector(&world.open(), &applier, &sender, Arc::new(Notify::new()), &custody)
        .drain()
        .await;
    assert!(applier.log(|l| l.events.is_empty() && l.proposals.is_empty()));
    assert!(db.pending_spoke_observations().unwrap().is_empty());
    let sent = sender.sent();
    let Some(command::Body::ProposalDecision(decision)) = &sent[0].1.body else {
        panic!("refusal expected")
    };
    assert_eq!(decision.code, ResultCode::Rejected as i32);
}

#[tokio::test]
async fn subagent_stop_does_not_wait_for_main_capture() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    let mut stop = stop_at(1, &run, "", 0);
    if let Some(record::Body::Event(event)) = &mut stop.body {
        event.from_subagent = true;
    }
    spoke.send(&stop).await;
    spoke.send(&tool_record(2, &run)).await;
    let applier = Applier::default();
    custody_projector(
        &db,
        &applier,
        &Arc::new(Sender::default()),
        Arc::new(Notify::new()),
        &world.custody(&db),
    )
    .drain()
    .await;
    assert_eq!(applier.log(|l| l.events.clone()), ["1#stop", "2#pre_tool_use"]);
    assert!(failed_effects(&db, "transcript_evidence_unavailable").is_empty());
    assert!(
        db.spoke_effect(HOST, "pending_observation", &format!("{}:1", run.run_id))
            .unwrap()
            .is_none(),
        "a subagent Stop never enters the capture-wait outbox"
    );
}

#[tokio::test]
async fn a_capture_timeout_still_dispatches_the_hub_transcript_copy() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&transcript_record(1, &run, "0-1", 0, b"0123456789")).await;
    spoke.send(&stop_at(2, &run, "0-1", 10)).await;

    let applier = Applier::default();
    let custody = world.custody(&db);
    custody_projector(
        &db,
        &applier,
        &Arc::new(Sender::default()),
        Arc::new(Notify::new()),
        &custody,
    )
    .drain()
    .await;

    assert_eq!(applier.log(|l| l.events.clone()), ["2#stop"]);
    let (_, held) = &applier.custody.sources()[0];
    let (_, bytes) = held.as_ref().expect("the Stop is given the hub's copy");
    assert_eq!(bytes, b"0123456789");
    assert!(failed_effects(&db, "transcript_evidence_unavailable").is_empty());
    let alerts = failed_effects(&db, "output_capture_unavailable");
    assert_eq!(alerts.len(), 1);
    assert!(alerts[0].last_error.as_deref().unwrap().contains("did not conclude"));
}

#[tokio::test]
async fn a_parked_stop_does_not_expire_while_its_host_still_has_projection_lag() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&stop_at(1, &run, "0-1", 10)).await;

    let applier = Applier::default();
    let sender = Arc::new(Sender::default());
    let custody = world.custody(&db);
    let running = tokio::spawn(custody_projector(&db, &applier, &sender, Arc::new(Notify::new()), &custody).run());
    eventually("the Stop is parked", || {
        applied(&db) == 1 && applier.log(|l| l.events.is_empty())
    })
    .await;
    running.abort();
    let _ = running.await;

    spoke.send(&transcript_record(2, &run, "0-1", 0, b"0123456789")).await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    let running = tokio::spawn(custody_projector(&db, &applier, &sender, Arc::new(Notify::new()), &custody).run());
    eventually("the Stop is dispatched with bytes that were only received", || {
        applier
            .custody
            .sources()
            .iter()
            .any(|(sequence, held)| *sequence == 1 && held.as_ref().is_some_and(|(_, bytes)| bytes == b"0123456789"))
    })
    .await;
    running.abort();
    let _ = running.await;
}

#[tokio::test]
async fn a_pane_exit_stays_parked_behind_a_waiting_stop() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&stop_at(1, &run, "0-1", 5)).await;
    spoke.send(&pane_record(2, &run, v1::PaneLife::Exited)).await;

    let applier = Applier::default();
    let sender = Arc::new(Sender::default());
    let custody = world.custody(&db);
    let wake = Arc::new(Notify::new());
    let running = tokio::spawn(custody_projector(&db, &applier, &sender, wake.clone(), &custody).run());
    eventually("Stop and pane are parked", || {
        applied(&db) == 2 && applier.log(|l| l.events.is_empty() && l.panes.is_empty())
    })
    .await;

    spoke.send(&transcript_record(3, &run, "0-1", 0, b"hello")).await;
    spoke.send(&concluded(4, &run, &[])).await;
    wake.notify_waiters();
    eventually("Stop then pane apply in that order", || {
        applier.log(|l| l.events == ["1#stop"] && l.panes == [("exec_1".to_string(), v1::PaneLife::Exited as i32)])
    })
    .await;
    let (_, held) = &applier.custody.sources()[0];
    assert!(held.is_some(), "the Stop still carries the hub copy");
    running.abort();
    let _ = running.await;
}

#[tokio::test]
async fn epoch_zero_does_not_wait_for_a_concluded_marker() {
    let world = World::new();
    let db = world.open();
    let run = assign(&db, "exec_1").1;
    let spoke = Spoke::connect(&db, None).await;
    spoke.send(&transcript_record(1, &run, "0-1", 0, b"hello")).await;
    spoke.send(&stop_at_epoch(2, &run, "0-1", 5, 0)).await;

    let applier = Applier::default();
    let custody = world.custody(&db);
    custody_projector(
        &db,
        &applier,
        &Arc::new(Sender::default()),
        Arc::new(Notify::new()),
        &custody,
    )
    .drain()
    .await;

    assert_eq!(applier.log(|l| l.events.clone()), ["2#stop"]);
    let (_, held) = &applier.custody.sources()[0];
    assert_eq!(held.as_ref().unwrap().1, b"hello");
    assert!(failed_effects(&db, "transcript_evidence_unavailable").is_empty());
    assert!(failed_effects(&db, "output_capture_unavailable").is_empty());
    assert!(db.pending_spoke_observations().unwrap().is_empty());
}
