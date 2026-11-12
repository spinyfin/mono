//! A spoke's journaled observations applied to a real engine: the hub's
//! projector reads durable frames in order and the engine's own consumers
//! (proposal pipeline, completion finalization, live state) act on them.
//!
//! The frames are committed through the same work-database transaction the
//! stream manager's sink uses, so each test starts from "the hub has received
//! it" and nothing is applied until a projector runs. A crash is the durable
//! state a crash leaves: the cursor and effect rows rewound to before the
//! record, with everything the engine already did kept.

use super::*;
use boss_engine_runtime::hub::projection::{CommandOutcome, CommandSender, Projector, ProjectorConfig};
use boss_protocol_spoke::{
    v1::{self, Command, CommandResult, ResultCode, command, record, spoke_frame},
    worker_event,
};
use boss_work_db::{HostTransport, SpokeAssignment, SpokeIngressCommit, SpokeRunKey};
use prost::Message;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::Ordering;

const HOST: &str = "spoke";
const PR_URL: &str = "https://github.com/example/repo/pull/9";

/// Records the commands the hub sends and answers them from a script.
#[derive(Default)]
struct Wire {
    sent: StdMutex<Vec<Command>>,
    unreachable: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl CommandSender for Wire {
    async fn send(&self, _: &str, command: Command, _: std::time::Duration) -> Result<CommandResult, CommandOutcome> {
        if self.unreachable.load(Ordering::SeqCst) {
            return Err(CommandOutcome::Unavailable);
        }
        let id = command.command_id.clone();
        self.sent.lock().unwrap().push(command);
        Ok(CommandResult {
            command_id: id,
            code: ResultCode::Ok as i32,
            ..Default::default()
        })
    }
}

impl Wire {
    fn decisions(&self) -> Vec<v1::ProposalDecision> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|command| match &command.body {
                Some(command::Body::ProposalDecision(decision)) => Some(decision.clone()),
                _ => None,
            })
            .collect()
    }
}

#[derive(bon::Builder)]
struct Fixture {
    state: Arc<ServerState>,
    _dir: tempfile::TempDir,
    execution_id: String,
    work_item_id: String,
    assignment: SpokeAssignment,
    received: u64,
    wire: Arc<Wire>,
}

impl Fixture {
    /// A live chore execution running on a spoke host, as dispatch leaves it.
    fn new(pr: boss_engine_completion::PrStatus) -> Self {
        let (state, dir) = test_server_state_with_fakes_and_pr_detector(
            Arc::new(FixedPrDetector(pr)),
            Some(Arc::new(EmptyProposalWorkspace)),
        );
        state.feature_flags.set("worker_proposals", true).unwrap();
        state.feature_flags.set("run_done_proposals_seam", true).unwrap();
        let (execution_id, work_item_id) = new_execution(&state, "Spoke run-done target");
        let db = &state.work_db;
        db.add_host(HOST, "worker@spoke", 1, &[]).unwrap();
        db.set_host_transport(HOST, HostTransport::SpokeReady).unwrap();
        let assignment = db.reserve_spoke_assignment(&execution_id, HOST).unwrap();
        db.bind_spoke_lease(
            &execution_id,
            assignment.assignment_generation,
            "workspace-run-done",
            "lease-run-done",
            "/remote/workspace",
        )
        .unwrap();
        db.start_execution_run_on_host(
            &execution_id,
            "worker",
            "repo",
            "lease-run-done",
            "workspace-run-done",
            dir.path().to_str().unwrap(),
            HOST,
        )
        .unwrap();
        Self::builder()
            .state(state)
            .dir(dir)
            .execution_id(execution_id)
            .work_item_id(work_item_id)
            .assignment(assignment)
            .received(0)
            .wire(Arc::new(Wire::default()))
            .build()
    }

    fn key(&self) -> SpokeRunKey {
        SpokeRunKey {
            execution_id: self.assignment.execution_id.clone(),
            assignment_generation: self.assignment.assignment_generation,
            run_id: self.assignment.run_id.clone(),
            spawn_token: self.assignment.spawn_token.clone(),
        }
    }

    fn run(&self) -> v1::Run {
        v1::Run {
            execution_id: self.assignment.execution_id.clone(),
            run_id: self.assignment.run_id.clone(),
            assignment_generation: self.assignment.assignment_generation,
            spawn_token: self.assignment.spawn_token.clone(),
        }
    }

    /// The hub durably receives `body`: stored and acknowledged, not applied.
    fn receive(&mut self, body: record::Body) {
        let db = &self.state.work_db;
        let admission = db.open_spoke_journal(HOST, "journal", self.received, 1).unwrap();
        self.received += 1;
        let record = v1::Record {
            sequence: self.received,
            observed: Some(v1::Stamp {
                boot_id: "boot".into(),
                wall_unix_ms: 1_000,
                ..Default::default()
            }),
            run: Some(self.run()),
            body: Some(body),
        };
        let frame = v1::SpokeFrame {
            body: Some(spoke_frame::Body::Record(record.clone())),
        }
        .encode_to_vec();
        db.commit_spoke_record(
            SpokeIngressCommit::builder()
                .host_id(HOST)
                .generation(admission.generation)
                .journal_id(&admission.journal_id)
                .sequence(record.sequence)
                .frame(&frame)
                .epoch(1)
                .features("execution=1")
                .observed_at_ms(1_000)
                .runs(std::slice::from_ref(&self.key()))
                .build(),
        )
        .unwrap();
    }

    fn receive_done(&mut self, key: &str, payload: Value) {
        self.receive_proposal(key, ProposalKind::RunDone, v1::ProposalKind::RunDone, payload);
    }

    fn receive_proposal(&mut self, key: &str, kind: ProposalKind, wire: v1::ProposalKind, payload: Value) {
        self.receive(record::Body::Proposal(v1::Proposal {
            proposal_id: key.into(),
            kind: wire as i32,
            payload_contract: "proposal-json/1".into(),
            canonical_json: serde_json::to_vec(&json!({"kind": kind, "payload": payload, "idempotency_key": key}))
                .unwrap(),
            ..Default::default()
        }));
    }

    fn receive_event(&mut self, event: &boss_protocol::WorkerEvent) {
        self.receive(record::Body::Event(worker_event::to_wire(event)));
    }

    fn receive_pane(&mut self, life: v1::PaneLife, exit_code: Option<i32>) {
        self.receive(record::Body::Pane(v1::PaneState {
            life: life as i32,
            session: "worker".into(),
            pane_id: "%1".into(),
            pid: 4242,
            exit_code,
            boot_id: "boot".into(),
            ..Default::default()
        }));
    }

    /// A projector over this engine, like a fresh engine process would have.
    fn projector(&self) -> Projector {
        Projector::new(
            self.state.work_db.as_ref().clone(),
            crate::app::spoke_projection::applier(&self.state),
            self.wire.clone(),
            Arc::new(tokio::sync::Notify::new()),
            ProjectorConfig::builder()
                .retry_initial(std::time::Duration::from_millis(5))
                .retry_max(std::time::Duration::from_millis(20))
                .build(),
        )
    }

    /// The durable state a crash leaves after an application that never reached
    /// its cursor commit: nothing recorded for it, the cursor behind it.
    fn crash_before_the_cursor_moved(&self, applied: u64) {
        let conn = self.state.work_db.connect().unwrap();
        conn.execute("UPDATE spoke_projection SET applied = ?1", [applied as i64])
            .unwrap();
        conn.execute("DELETE FROM spoke_effects WHERE sequence > ?1", [applied as i64])
            .unwrap();
    }

    fn proposals(&self) -> Vec<WorkerProposal> {
        self.state
            .work_db
            .list_worker_proposals_for_work_item(&self.work_item_id, None, None)
            .unwrap()
    }

    /// What the run's slot shows, terminated or not.
    fn activity(&self) -> Option<boss_protocol::WorkerActivity> {
        self.state
            .live_worker_states
            .snapshot()
            .into_iter()
            .find(|slot| slot.run_id == self.execution_id)
            .map(|slot| slot.activity)
    }
}

fn delivered_with_a_resolvable_pr() -> Fixture {
    Fixture::new(boss_engine_completion::PrStatus::Fresh { url: PR_URL.into() })
}

fn queued_done() -> Value {
    json!({"outcome": "delivered", "summary": "Finished while the hub was away"})
}

#[tokio::test]
async fn a_done_queued_while_the_hub_was_away_finalizes_the_run_once_however_often_it_is_replayed() {
    let mut fx = delivered_with_a_resolvable_pr();
    // The spoke queued the declaration; the hub received it and acknowledged it
    // without applying it. The execution is still live.
    fx.receive_done("key-1", queued_done());
    assert!(
        fx.state
            .work_db
            .get_execution(&fx.execution_id)
            .unwrap()
            .status
            .is_live()
    );
    assert!(fx.proposals().is_empty(), "receipt applies nothing");

    // The spoke cannot be reached for the decision yet.
    fx.wire.unreachable.store(true, Ordering::SeqCst);
    fx.projector().drain().await;
    let first = fx.state.work_db.get_execution(&fx.execution_id).unwrap();
    assert!(
        first.status.is_terminal(),
        "the queued done finalized the run: {first:?}"
    );
    let proposals = fx.proposals();
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].kind, ProposalKind::RunDone);
    assert_eq!(proposals[0].state, ProposalState::Applied);
    assert_eq!(
        fx.state.work_db.execution_run_done_outcome(&fx.execution_id).unwrap(),
        Some(boss_protocol::RunDoneOutcome::Delivered)
    );
    assert!(fx.wire.decisions().is_empty(), "nothing is sent to an unreachable host");
    let attention = fx.state.work_db.list_attention_items(&fx.execution_id).unwrap().len();
    let status_before = task_status(&fx.state, &fx.work_item_id);

    // The engine dies after applying and before its cursor moved. After the
    // restart the same record is applied again.
    fx.crash_before_the_cursor_moved(0);
    fx.wire.unreachable.store(false, Ordering::SeqCst);
    let restarted = fx.projector();
    restarted.drain().await;

    let again = fx.state.work_db.get_execution(&fx.execution_id).unwrap();
    assert_eq!(
        (&again.status, &again.finished_at),
        (&first.status, &first.finished_at),
        "no second terminalization"
    );
    assert_eq!(
        fx.proposals().len(),
        1,
        "the idempotency key made the replay the same proposal"
    );
    assert_eq!(
        fx.state.work_db.list_attention_items(&fx.execution_id).unwrap().len(),
        attention
    );
    assert_eq!(task_status(&fx.state, &fx.work_item_id), status_before);

    // The worker is told once, and the decision says what happened.
    let decisions = fx.wire.decisions();
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        (decisions[0].proposal_id.as_str(), decisions[0].code),
        ("key-1", ResultCode::Ok as i32)
    );
    let shown: Value = serde_json::from_slice(&decisions[0].result_json).unwrap();
    assert_eq!(
        (shown["state"].as_str(), shown["already_submitted"].as_bool()),
        (Some("applied"), Some(true))
    );

    // The spoke re-queues the very same declaration under a later record, and the
    // restarted hub receives that too. Nothing more happens.
    fx.receive_done("key-1", queued_done());
    restarted.drain().await;
    assert_eq!(fx.proposals().len(), 1);
    assert_eq!(fx.wire.decisions().len(), 1);
    assert_eq!(
        fx.state.work_db.get_execution(&fx.execution_id).unwrap().finished_at,
        first.finished_at
    );
    let position = fx.state.work_db.spoke_projection_position(HOST).unwrap().unwrap();
    assert_eq!((position.received, position.applied), (2, 2));
    let decision = fx.state.work_db.pending_spoke_effects(10).unwrap();
    assert!(
        decision.is_empty(),
        "the decision was delivered and settled: {decision:?}"
    );
}

#[tokio::test]
async fn the_hub_refuses_what_its_own_validators_refuse_and_applies_nothing() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_done("key-bad", json!({"outcome": "somehow", "summary": "?"}));
    // A key an engine would derive for different content is not a worker's to claim.
    fx.receive_done("auto:run_done:0123456789abcdef0123456789abcdef", queued_done());
    fx.projector().drain().await;
    assert!(fx.proposals().is_empty());
    assert!(
        fx.state
            .work_db
            .get_execution(&fx.execution_id)
            .unwrap()
            .status
            .is_live()
    );
    let decisions = fx.wire.decisions();
    assert_eq!(decisions.len(), 2);
    assert!(decisions.iter().all(|d| d.code == ResultCode::Rejected as i32));
    assert!(decisions[0].reason.contains("outcome"), "{}", decisions[0].reason);
    assert!(decisions[1].reason.contains("idempotency"), "{}", decisions[1].reason);
}

#[tokio::test]
async fn hook_events_take_the_worker_event_fan_out_and_record_the_turn_boundary() {
    use boss_protocol::{WorkerActivity, WorkerEvent};
    let mut fx = delivered_with_a_resolvable_pr();
    let session_id = "remote-session".to_owned();
    fx.receive_event(&WorkerEvent::UserPromptSubmit {
        session_id: session_id.clone(),
        prompt: "go".into(),
    });
    fx.receive_event(&WorkerEvent::PreToolUse {
        session_id: session_id.clone(),
        tool_name: "Bash".into(),
        tool_input: json!({"command": "ls"}),
    });
    let projector = fx.projector();
    projector.drain().await;
    // The remote worker got a live-state slot and shows as working, exactly as a
    // hook arriving on the local events socket would have made it.
    assert_eq!(fx.activity(), Some(WorkerActivity::Working));
    assert!(
        !crate::app::spoke_probes::spoke_parked_for_delivery(&fx.state, &fx.execution_id),
        "mid-turn delivery stays closed while the worker is working"
    );

    // Events are applied in the order the worker emitted them: a Stop followed
    // by a tool call is a worker that resumed, not one that parked.
    fx.receive_event(&WorkerEvent::Stop {
        session_id: session_id.clone(),
        stop_hook_active: false,
        stop_reason: boss_protocol::StopReason::AwaitingInput,
    });
    fx.receive_event(&WorkerEvent::PreToolUse {
        session_id,
        tool_name: "Bash".into(),
        tool_input: json!({"command": "ls"}),
    });
    projector.drain().await;
    assert_eq!(fx.activity(), Some(WorkerActivity::Working));
    assert!(!crate::app::spoke_probes::spoke_parked_for_delivery(
        &fx.state,
        &fx.execution_id
    ));
    let position = fx.state.work_db.spoke_projection_position(HOST).unwrap().unwrap();
    assert_eq!(position.applied, 4);
}

#[tokio::test]
async fn a_pane_exit_marks_the_slot_and_is_kept_with_its_exit_code_but_alive_revives_it() {
    use boss_protocol::{WorkerActivity, WorkerEvent};
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_event(&WorkerEvent::PreToolUse {
        session_id: "remote-session".into(),
        tool_name: "Bash".into(),
        tool_input: json!({}),
    });
    fx.receive_pane(v1::PaneLife::Exited, Some(3));
    let projector = fx.projector();
    projector.drain().await;
    assert_eq!(fx.activity(), Some(WorkerActivity::Terminated));
    let kept = fx
        .state
        .work_db
        .spoke_pane_observation(&fx.execution_id)
        .unwrap()
        .unwrap();
    assert_eq!((kept.life, kept.exit_code), (v1::PaneLife::Exited as i32, Some(3)));
    // The exit is evidence, not a verdict: the execution is as it was.
    assert!(
        fx.state
            .work_db
            .get_execution(&fx.execution_id)
            .unwrap()
            .status
            .is_live()
    );

    // A host that could not tell says nothing; one that finds it alive again
    // takes the slot back out of the exited display.
    fx.receive_pane(v1::PaneLife::Indeterminate, None);
    projector.drain().await;
    assert_eq!(fx.activity(), Some(WorkerActivity::Terminated));
    fx.receive_pane(v1::PaneLife::Alive, None);
    projector.drain().await;
    assert_ne!(fx.activity(), Some(WorkerActivity::Terminated));
    let kept = fx
        .state
        .work_db
        .spoke_pane_observation(&fx.execution_id)
        .unwrap()
        .unwrap();
    assert_eq!(kept.life, v1::PaneLife::Alive as i32);
}

#[tokio::test]
async fn a_review_guide_is_refused_from_a_spoke_without_touching_the_hub() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_proposal(
        "guide-1",
        ProposalKind::ReviewGuide,
        v1::ProposalKind::OtherValidated,
        json!({"body_markdown": "# guide"}),
    );
    fx.projector().drain().await;
    assert!(fx.proposals().is_empty());
    let decisions = fx.wire.decisions();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].code, ResultCode::Rejected as i32);
    assert!(decisions[0].reason.contains("review guide"));
}
