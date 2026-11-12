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
use boss_spoke_hub::WorkDbSink;
use boss_spoke_hub::projection::{
    Applied, CommandSender, Decision, EventObservation, ObservationApplier, Origin, PROPOSAL_DECISION, PaneObservation,
    Projector, ProjectorConfig, Proposed, QUARANTINE,
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

#[derive(Clone, Default)]
struct Applier {
    log: Arc<Mutex<Log>>,
    /// Applying a proposal parks forever, as a crash mid-application would.
    hang_proposals: Arc<AtomicBool>,
    entered: Arc<Notify>,
    event_retries: Arc<AtomicU32>,
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
        self.log
            .lock()
            .unwrap()
            .events
            .push(format!("{}#{kind}", observation.origin.sequence));
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
            ..Default::default()
        }),
    )
}

fn stop_record(sequence: u64, run: &Run) -> v1::Record {
    record_of(
        sequence,
        run,
        record::Body::Event(worker_event::to_wire(&WorkerEvent::Stop {
            session_id: "session".into(),
            stop_hook_active: false,
            stop_reason: boss_protocol::StopReason::Completed,
        })),
    )
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
        let sink = match wake {
            Some(wake) => WorkDbSink::with_wake(db.clone(), wake),
            None => WorkDbSink::new(db.clone()),
        };
        let mut hello = hello(HOST);
        hello.last_record = db.spoke_journal(HOST).unwrap().map_or(0, |j| j.received);
        let admission = sink.open(&hello, &selection()).await.expect("open journal");
        Self {
            sink,
            session: session(HOST, admission.generation),
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
async fn an_unreadable_frame_is_quarantined_for_an_operator_and_the_cursor_moves_on() {
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
        proposal.payload_contract = "guide-outcome/1".into();
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
    assert!(decision.reason.contains("guide-outcome/1"));
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
async fn a_spoke_that_keeps_refusing_a_decision_ends_it_for_an_operator() {
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
