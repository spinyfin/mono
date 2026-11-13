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
        Self::on_spoke(state, dir, execution_id, work_item_id, true)
    }

    /// Assign an existing execution to the spoke host. `start` begins its run
    /// there; an execution whose run was already started is only bound.
    fn on_spoke(
        state: Arc<ServerState>,
        dir: tempfile::TempDir,
        execution_id: String,
        work_item_id: String,
        start: bool,
    ) -> Self {
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
        if start {
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
        }
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

    fn receive_event_from_subagent(&mut self, event: &boss_protocol::WorkerEvent) {
        let mut wire = worker_event::to_wire(event);
        wire.from_subagent = true;
        self.receive(record::Body::Event(wire));
    }

    fn receive_run_write(&mut self, contract: &str, key: &str, payload: Value) {
        self.receive(record::Body::Proposal(v1::Proposal {
            proposal_id: key.into(),
            kind: v1::ProposalKind::OtherValidated as i32,
            payload_contract: contract.into(),
            canonical_json: serde_json::to_vec(&payload).unwrap(),
            ..Default::default()
        }));
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
        self.projector_with(
            ProjectorConfig::builder()
                .retry_initial(std::time::Duration::from_millis(5))
                .retry_max(std::time::Duration::from_millis(20))
                .build(),
        )
    }

    fn projector_with(&self, config: ProjectorConfig) -> Projector {
        Projector::new(
            self.state.work_db.as_ref().clone(),
            crate::app::spoke_projection::applier(&self.state),
            self.wire.clone(),
            Arc::new(tokio::sync::Notify::new()),
            config,
        )
    }

    fn product_id(&self) -> String {
        match self.state.work_db.get_work_item(&self.work_item_id).unwrap() {
            boss_protocol::WorkItem::Task(task) | boss_protocol::WorkItem::Chore(task) => task.product_id,
            other => panic!("fixture work item is not a task or chore: {other:?}"),
        }
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
    // Finalization runs off the delivery pass, so it lands just after the drain.
    eventually("the queued done to finalize the run", || is_terminal(&fx)).await;
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

#[tokio::test]
async fn an_awaiting_input_stop_is_restored_as_parked_after_memory_is_cleared() {
    use boss_protocol::WorkerEvent;
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_event(&WorkerEvent::Stop {
        session_id: "remote-session".into(),
        stop_hook_active: false,
        stop_reason: boss_protocol::StopReason::AwaitingInput,
    });
    fx.projector().drain().await;
    assert!(crate::app::spoke_probes::spoke_parked_for_delivery(
        &fx.state,
        &fx.execution_id
    ));
    fx.state.spoke_turns.clear();
    assert!(!crate::app::spoke_probes::spoke_parked_for_delivery(
        &fx.state,
        &fx.execution_id
    ));
    fx.state.restore_spoke_turns().unwrap();
    assert!(crate::app::spoke_probes::spoke_parked_for_delivery(
        &fx.state,
        &fx.execution_id
    ));
}

#[tokio::test]
async fn subagent_events_do_not_move_the_parent_turn() {
    use boss_protocol::WorkerEvent;
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_event(&WorkerEvent::UserPromptSubmit {
        session_id: "main".into(),
        prompt: "go".into(),
    });
    fx.projector().drain().await;
    fx.receive_event_from_subagent(&WorkerEvent::Stop {
        session_id: "sub".into(),
        stop_hook_active: false,
        stop_reason: boss_protocol::StopReason::Completed,
    });
    fx.projector().drain().await;
    assert!(!crate::app::spoke_probes::spoke_parked_for_delivery(
        &fx.state,
        &fx.execution_id
    ));
    fx.receive_event(&WorkerEvent::Stop {
        session_id: "main".into(),
        stop_hook_active: false,
        stop_reason: boss_protocol::StopReason::AwaitingInput,
    });
    fx.projector().drain().await;
    assert!(crate::app::spoke_probes::spoke_parked_for_delivery(
        &fx.state,
        &fx.execution_id
    ));
    fx.receive_event_from_subagent(&WorkerEvent::PreToolUse {
        session_id: "sub".into(),
        tool_name: "Bash".into(),
        tool_input: json!({"command": "ls"}),
    });
    fx.projector().drain().await;
    assert!(crate::app::spoke_probes::spoke_parked_for_delivery(
        &fx.state,
        &fx.execution_id
    ));
}

#[tokio::test]
async fn a_run_scoped_write_is_applied_through_the_local_handler() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CONFLICT_PRODUCER,
        "producer-1",
        boss_protocol_spoke::proposal::conflict_producer_body(
            &fx.execution_id,
            "feature",
            "main",
            vec!["src/lib.rs".into()],
        ),
    );
    fx.projector().drain().await;
    let decisions = fx.wire.decisions();
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert_eq!(decisions[0].code, ResultCode::Ok as i32);
    let recorded = fx
        .state
        .work_db
        .latest_conflict_resolution_for_work_item(&fx.work_item_id)
        .unwrap()
        .expect("producer conflict row");
    assert_eq!(recorded.event_source, "producer_rebase");
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CONFLICT_PRODUCER,
        "producer-1",
        boss_protocol_spoke::proposal::conflict_producer_body(
            &fx.execution_id,
            "feature",
            "main",
            vec!["src/lib.rs".into()],
        ),
    );
    fx.projector().drain().await;
    assert_eq!(fx.wire.decisions().len(), 1, "the same write is not applied twice");
}

#[tokio::test]
async fn a_producer_write_cannot_claim_another_execution_via_run_id() {
    let mut fx = delivered_with_a_resolvable_pr();
    let mut payload = boss_protocol_spoke::proposal::conflict_producer_body(
        "exec_someone_else",
        "feature",
        "main",
        vec!["src/lib.rs".into()],
    );
    payload["run_id"] = json!(fx.execution_id);
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CONFLICT_PRODUCER,
        "producer-spoof",
        payload,
    );
    fx.projector().drain().await;
    let decisions = fx.wire.decisions();
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert_eq!(decisions[0].code, ResultCode::Rejected as i32);
    assert!(
        fx.state
            .work_db
            .latest_conflict_resolution_for_work_item(&fx.work_item_id)
            .unwrap()
            .is_none(),
        "a spoofed run_id must not record a producer conflict"
    );
}

#[tokio::test]
async fn a_run_scoped_write_for_another_runs_attempt_is_refused() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CONFLICT_MARK_FAILED,
        "mark-1",
        boss_protocol_spoke::proposal::conflict_mark_failed_body(
            &fx.execution_id,
            "crz_not_this_run",
            "obsolescence_suspected",
            json!({}),
        ),
    );
    fx.projector().drain().await;
    let decisions = fx.wire.decisions();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].code, ResultCode::Rejected as i32);
}

#[tokio::test]
async fn a_producer_write_is_keyed_on_the_submission_not_the_branch_names() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CONFLICT_PRODUCER,
        "producer-a",
        boss_protocol_spoke::proposal::conflict_producer_body(
            &fx.execution_id,
            "feature",
            "main",
            vec!["src/a.rs".into()],
        ),
    );
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CONFLICT_PRODUCER,
        "producer-b",
        boss_protocol_spoke::proposal::conflict_producer_body(
            &fx.execution_id,
            "feature",
            "main",
            vec!["src/b.rs".into()],
        ),
    );
    fx.projector().drain().await;
    let decisions = fx.wire.decisions();
    assert_eq!(decisions.len(), 2, "{decisions:?}");
    assert!(decisions.iter().all(|d| d.code == ResultCode::Ok as i32));
    let latest = fx
        .state
        .work_db
        .latest_conflict_resolution_for_work_item(&fx.work_item_id)
        .unwrap()
        .expect("second producer row");
    assert!(
        latest
            .conflict_diagnosis
            .as_deref()
            .is_some_and(|diagnosis| diagnosis.contains("src/b.rs")),
        "distinct keys record distinct files: {:?}",
        latest.conflict_diagnosis
    );
    fx.crash_before_the_cursor_moved(1);
    fx.projector().drain().await;
    let rows = fx
        .state
        .work_db
        .list_conflict_resolutions(None, &[], Some(&fx.work_item_id), None)
        .unwrap();
    assert_eq!(rows.len(), 2, "replay of the second key is not a third row");
}

#[tokio::test]
async fn an_answer_reply_completes_the_thread_and_survives_a_crash_replay() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = 'answer_agent' WHERE id = ?1",
            [&fx.execution_id],
        )
        .unwrap();
    let comment = fx
        .state
        .work_db
        .create_comment(
            boss_protocol::CreateCommentInput::builder()
                .artifact_kind("work_item")
                .artifact_id(&fx.work_item_id)
                .anchor(boss_protocol::CommentAnchor {
                    exact: "why".into(),
                    ..Default::default()
                })
                .body("why does this retry?")
                .author("operator")
                .doc_version("v0")
                .build(),
        )
        .unwrap();
    fx.state.work_db.transition_comment_to_answering(&comment.id).unwrap();
    let run = fx
        .state
        .work_db
        .create_answer_agent_run(&comment.id, "work_item", &fx.work_item_id, "v0", 0)
        .unwrap();
    fx.state
        .work_db
        .bind_answer_agent_run_execution(&run.id, &fx.execution_id)
        .unwrap();
    fx.receive_run_write(
        boss_protocol_spoke::proposal::ANSWER_REPLY,
        "answer-1",
        boss_protocol_spoke::proposal::answer_reply_body(&fx.execution_id, "because the backoff is exponential"),
    );
    fx.projector().drain().await;
    assert_eq!(fx.wire.decisions()[0].code, ResultCode::Ok as i32);
    let answered = fx.state.work_db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(answered.status, boss_protocol::COMMENT_STATUS_ANSWERED);
    let thread = fx.state.work_db.list_comment_thread_entries(&comment.id).unwrap();
    assert!(thread.iter().any(|entry| entry.body.contains("exponential")));
    fx.crash_before_the_cursor_moved(0);
    fx.projector().drain().await;
    assert_eq!(
        fx.state.work_db.list_comment_thread_entries(&comment.id).unwrap().len(),
        thread.len(),
        "crash replay does not add a second thread entry"
    );
}

#[tokio::test]
async fn a_failed_answer_attempt_is_refused_instead_of_treated_as_already_replied() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = 'answer_agent' WHERE id = ?1",
            [&fx.execution_id],
        )
        .unwrap();
    let comment = fx
        .state
        .work_db
        .create_comment(
            boss_protocol::CreateCommentInput::builder()
                .artifact_kind("work_item")
                .artifact_id(&fx.work_item_id)
                .anchor(boss_protocol::CommentAnchor {
                    exact: "why".into(),
                    ..Default::default()
                })
                .body("why?")
                .author("operator")
                .doc_version("v0")
                .build(),
        )
        .unwrap();
    fx.state.work_db.transition_comment_to_answering(&comment.id).unwrap();
    let run = fx
        .state
        .work_db
        .create_answer_agent_run(&comment.id, "work_item", &fx.work_item_id, "v0", 0)
        .unwrap();
    fx.state
        .work_db
        .bind_answer_agent_run_execution(&run.id, &fx.execution_id)
        .unwrap();
    fx.state
        .work_db
        .complete_answer_agent_run(
            &run.id,
            boss_protocol::ANSWER_AGENT_RUN_STATUS_FAILED,
            None,
            Some("stopped"),
        )
        .unwrap();
    fx.receive_run_write(
        boss_protocol_spoke::proposal::ANSWER_REPLY,
        "answer-failed",
        boss_protocol_spoke::proposal::answer_reply_body(&fx.execution_id, "too late"),
    );
    fx.projector().drain().await;
    assert_eq!(fx.wire.decisions()[0].code, ResultCode::Rejected as i32);
    assert!(
        fx.state
            .work_db
            .list_comment_thread_entries(&comment.id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_guide_outcome_write_uses_canonical_json_and_replays_after_a_crash() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = 'revision_implementation' WHERE id = ?1",
            [&fx.execution_id],
        )
        .unwrap();
    let (_root, series) = boss_work_db::test_support::seed_published_guide(fx.state.work_db.as_ref(), 9);
    let comment = boss_work_db::test_support::add_guide_comment(fx.state.work_db.as_ref(), &series, "fix retry");
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_comments SET status = 'in_revision', revise_task_id = ?2 WHERE id = ?1",
            [&comment.id, &fx.work_item_id],
        )
        .unwrap();
    fx.receive_run_write(
        boss_protocol_spoke::proposal::GUIDE_OUTCOME,
        "guide-1",
        boss_protocol_spoke::proposal::guide_outcome_body(
            &fx.execution_id,
            &comment.id,
            &boss_protocol::GuideCommentDisposition::NoChange,
            "already handled",
            false,
        ),
    );
    fx.projector().drain().await;
    assert_eq!(
        fx.wire.decisions()[0].code,
        ResultCode::Ok as i32,
        "{:?}",
        fx.wire.decisions()
    );
    fx.crash_before_the_cursor_moved(0);
    fx.projector().drain().await;
    let outcomes: i64 = fx
        .state
        .work_db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM guide_comment_outcomes WHERE comment_id = ?1",
            [&comment.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(outcomes, 1, "crash replay does not record a second guide outcome");
}

#[tokio::test]
async fn a_ci_mark_failed_write_uses_canonical_json_and_replays_after_a_crash() {
    let mut fx = delivered_with_a_resolvable_pr();
    let attempt = fx
        .state
        .work_db
        .insert_ci_remediation(boss_work_db::CiRemediationInsertInput {
            product_id: fx.product_id(),
            work_item_id: fx.work_item_id.clone(),
            pr_url: PR_URL.into(),
            pr_number: 9,
            head_branch: "feature".into(),
            head_sha_at_trigger: "a".repeat(40),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("pending ci attempt");
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CI_MARK_FAILED,
        "ci-1",
        boss_protocol_spoke::proposal::ci_mark_failed_body(&fx.execution_id, &attempt.id, "wont_fix", json!({})),
    );
    fx.projector().drain().await;
    assert_eq!(
        fx.wire.decisions()[0].code,
        ResultCode::Ok as i32,
        "{:?}",
        fx.wire.decisions()
    );
    let stored = fx.state.work_db.get_ci_remediation(&attempt.id).unwrap().unwrap();
    assert_eq!(stored.status, "failed");
    fx.crash_before_the_cursor_moved(0);
    fx.projector().drain().await;
    let stored = fx.state.work_db.get_ci_remediation(&attempt.id).unwrap().unwrap();
    assert_eq!(stored.status, "failed");
}

#[tokio::test]
async fn a_conflict_mark_failed_write_uses_canonical_json() {
    let mut fx = delivered_with_a_resolvable_pr();
    let attempt = fx
        .state
        .work_db
        .insert_conflict_resolution(boss_work_db::ConflictResolutionInsertInput {
            product_id: fx.product_id(),
            work_item_id: fx.work_item_id.clone(),
            pr_url: PR_URL.into(),
            pr_number: 9,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base".into()),
            head_sha_before: Some("head".into()),
        })
        .unwrap()
        .expect("pending conflict attempt");
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CONFLICT_MARK_FAILED,
        "mark-owned",
        boss_protocol_spoke::proposal::conflict_mark_failed_body(
            &fx.execution_id,
            &attempt.id,
            "obsolescence_suspected",
            json!({}),
        ),
    );
    fx.projector().drain().await;
    assert_eq!(
        fx.wire.decisions()[0].code,
        ResultCode::Ok as i32,
        "{:?}",
        fx.wire.decisions()
    );
    let stored = fx.state.work_db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(stored.status, "failed");
}

#[tokio::test]
async fn a_replayed_hook_stamps_last_event_at_from_the_observed_wall_time() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_event(&boss_protocol::WorkerEvent::UserPromptSubmit {
        session_id: "main".into(),
        prompt: "go".into(),
    });
    fx.projector().drain().await;
    let last = fx
        .state
        .live_worker_states
        .last_event_at_for_run(&fx.execution_id)
        .expect("activity time");
    assert_eq!(last, boss_engine_live_worker_state::iso8601_utc(1));
}

#[tokio::test]
async fn a_local_tmux_slot_does_not_record_spoke_turn_state() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.state
        .worker_registry
        .register_tmux_run_slot(&fx.execution_id, 1, "boss-worker-1");
    fx.receive_event(&boss_protocol::WorkerEvent::Stop {
        session_id: "main".into(),
        stop_hook_active: false,
        stop_reason: boss_protocol::StopReason::AwaitingInput,
    });
    fx.projector().drain().await;
    assert!(
        fx.state.work_db.spoke_turn_states().unwrap().is_empty(),
        "a local tmux pane is not a spoke turn"
    );
}

#[tokio::test]
async fn a_failed_result_lookup_retries_and_does_not_store_a_refusal() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("ALTER TABLE spoke_applied_writes RENAME TO spoke_applied_writes_hidden")
        .unwrap();
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CONFLICT_PRODUCER,
        "producer-retry",
        boss_protocol_spoke::proposal::conflict_producer_body(
            &fx.execution_id,
            "feature",
            "main",
            vec!["src/lib.rs".into()],
        ),
    );
    let projector = fx.projector_with(
        ProjectorConfig::builder()
            .retry_initial(std::time::Duration::from_millis(5))
            .retry_max(std::time::Duration::from_millis(20))
            .retry_stall(std::time::Duration::from_millis(30))
            .build(),
    );
    let draining = tokio::spawn({
        let projector = projector.clone();
        async move { projector.drain().await }
    });
    for _ in 0..40 {
        let stalled = fx
            .state
            .work_db
            .spoke_projection_status()
            .unwrap()
            .into_iter()
            .any(|host| host.stall_reason.is_some());
        if stalled {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        fx.state
            .work_db
            .spoke_projection_status()
            .unwrap()
            .iter()
            .any(|host| host.stall_reason.is_some()),
        "a host stuck retrying must surface a stall"
    );
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("ALTER TABLE spoke_applied_writes_hidden RENAME TO spoke_applied_writes")
        .unwrap();
    draining.await.unwrap();
    let decisions = fx.wire.decisions();
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert_eq!(decisions[0].code, ResultCode::Ok as i32);
    assert!(
        fx.state
            .work_db
            .spoke_projection_status()
            .unwrap()
            .iter()
            .all(|host| host.stall_reason.is_none())
    );
}

#[tokio::test]
async fn turn_state_is_restored_on_a_fresh_server_and_a_persist_failure_holds_the_cursor() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_event(&boss_protocol::WorkerEvent::Stop {
        session_id: "main".into(),
        stop_hook_active: false,
        stop_reason: boss_protocol::StopReason::AwaitingInput,
    });
    fx.projector().drain().await;
    assert!(crate::app::spoke_probes::spoke_parked_for_delivery(
        &fx.state,
        &fx.execution_id
    ));
    let db = (*fx.state.work_db).clone();
    let (fresh, _dir) = test_server_state_with_overrides(
        ServerStateOverrides::builder()
            .work_db(db)
            .skip_hub_installation(true)
            .cube_client(Arc::new(boss_engine_runtime::test_support::AlwaysSucceedsCube))
            .execution_runner(Arc::new(boss_engine_runtime::test_support::AlwaysSucceedsRunner))
            .build(),
    );
    fresh.restore_spoke_turns().unwrap();
    assert!(crate::app::spoke_probes::spoke_parked_for_delivery(
        &fresh,
        &fx.execution_id
    ));

    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("ALTER TABLE spoke_turn_state RENAME TO spoke_turn_state_hidden")
        .unwrap();
    fx.receive_event(&boss_protocol::WorkerEvent::UserPromptSubmit {
        session_id: "main".into(),
        prompt: "go".into(),
    });
    let projector = fx.projector_with(
        ProjectorConfig::builder()
            .retry_initial(std::time::Duration::from_millis(5))
            .retry_max(std::time::Duration::from_millis(20))
            .retry_stall(std::time::Duration::from_millis(30))
            .build(),
    );
    let draining = tokio::spawn({
        let projector = projector.clone();
        async move { projector.drain().await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert_eq!(
        fx.state
            .work_db
            .spoke_projection_position(HOST)
            .unwrap()
            .unwrap()
            .applied,
        1,
        "a turn-state write failure must not advance the cursor"
    );
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("ALTER TABLE spoke_turn_state_hidden RENAME TO spoke_turn_state")
        .unwrap();
    draining.await.unwrap();
    assert_eq!(
        fx.state
            .work_db
            .spoke_projection_position(HOST)
            .unwrap()
            .unwrap()
            .applied,
        2
    );
}

fn watcher() -> Arc<SessionSink> {
    let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    Arc::new(SessionSink::new(
        shutdown_tx,
        crate::app::frontend_metrics::test_registry(),
    ))
}

async fn watch(state: &Arc<ServerState>, topic: String) -> Arc<SessionSink> {
    let sink = watcher();
    state.topic_broker.register_session("watcher", sink.clone()).await;
    state.topic_broker.subscribe("watcher", &[topic]).await;
    sink
}

/// Everything the sink received, waiting briefly for pushes still in flight.
async fn received(sink: &SessionSink) -> Vec<FrontendEvent> {
    let mut events = Vec::new();
    while let Ok(Some(envelope)) = tokio::time::timeout(std::time::Duration::from_millis(200), sink.next()).await {
        events.push(envelope.payload);
    }
    events
}

fn invalidation_reasons(events: &[FrontendEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            FrontendEvent::TopicEvent {
                event: boss_protocol::TopicEventPayload::WorkInvalidated { reason, .. },
                ..
            } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..150 {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn conflict_attempt(fx: &Fixture) -> boss_protocol::ConflictResolution {
    fx.state
        .work_db
        .insert_conflict_resolution(boss_work_db::ConflictResolutionInsertInput {
            product_id: fx.product_id(),
            work_item_id: fx.work_item_id.clone(),
            pr_url: PR_URL.into(),
            pr_number: 9,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base".into()),
            head_sha_before: Some("head".into()),
        })
        .unwrap()
        .expect("pending conflict attempt")
}

fn answer_agent_comment(fx: &Fixture) -> boss_protocol::WorkComment {
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = 'answer_agent' WHERE id = ?1",
            [&fx.execution_id],
        )
        .unwrap();
    let comment = fx
        .state
        .work_db
        .create_comment(
            boss_protocol::CreateCommentInput::builder()
                .artifact_kind("work_item")
                .artifact_id(&fx.work_item_id)
                .anchor(boss_protocol::CommentAnchor {
                    exact: "why".into(),
                    ..Default::default()
                })
                .body("why?")
                .author("operator")
                .doc_version("v0")
                .build(),
        )
        .unwrap();
    fx.state.work_db.transition_comment_to_answering(&comment.id).unwrap();
    let run = fx
        .state
        .work_db
        .create_answer_agent_run(&comment.id, "work_item", &fx.work_item_id, "v0", 0)
        .unwrap();
    fx.state
        .work_db
        .bind_answer_agent_run_execution(&run.id, &fx.execution_id)
        .unwrap();
    comment
}

#[tokio::test]
async fn a_replayed_hook_carries_its_observed_time_into_the_liveness_checkpoint() {
    let mut fx = delivered_with_a_resolvable_pr();
    // The host saw this at 1s past the epoch; the hub applies it long after.
    fx.receive_event(&boss_protocol::WorkerEvent::Stop {
        session_id: "main".into(),
        stop_hook_active: false,
        stop_reason: boss_protocol::StopReason::AwaitingInput,
    });
    fx.projector().drain().await;
    let observed = boss_engine_live_worker_state::iso8601_utc(1);
    let slot = fx
        .state
        .live_worker_states
        .snapshot()
        .into_iter()
        .find(|slot| slot.run_id == fx.execution_id)
        .expect("the replayed hook gave the run a slot")
        .slot_id;
    let in_memory = fx
        .state
        .live_worker_states
        .semantic_progress_for_slot(slot)
        .expect("checkpoint");
    assert_eq!(in_memory.progress_at, observed, "the registry checkpoint");
    let durable = fx
        .state
        .work_db
        .get_run_semantic_progress_checkpoint(&fx.execution_id)
        .unwrap()
        .expect("durable checkpoint");
    assert_eq!(durable.progress_at, observed, "work_runs.semantic_progress_at");

    // The stale-worker sweep reads that checkpoint, so a worker that died during
    // the outage is still judged stale, not freshly active.
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    let verdict = boss_engine_runtime::stale_worker_sweep::classify_semantic_staleness(
        durable.tool_condition,
        Some(&durable.progress_at),
        &boss_engine_live_worker_state::iso8601_utc(0),
        boss_engine_driver::ProgressFidelity::Rich,
        now,
        300,
    );
    assert!(
        matches!(
            verdict,
            boss_engine_runtime::stale_worker_sweep::SemanticStaleness::Stale { .. }
        ),
        "{verdict:?}"
    );

    // Draining an older backlog behind a newer record never moves it backwards.
    fx.receive_event(&boss_protocol::WorkerEvent::UserPromptSubmit {
        session_id: "main".into(),
        prompt: "go".into(),
    });
    fx.projector().drain().await;
    let after = fx
        .state
        .work_db
        .get_run_semantic_progress_checkpoint(&fx.execution_id)
        .unwrap()
        .unwrap();
    assert!(after.progress_at >= durable.progress_at);
}

fn review_member_on_a_spoke() -> (Fixture, String) {
    let (state, dir) = test_server_state_with_fakes();
    let (_ordinary, work_item_id) = new_execution(&state, "Review target");
    let dispatch = state
        .work_db
        .create_pre_merge_review_batch(
            boss_work_db::ReviewBatchCreateInput::builder()
                .cycle_root_id(work_item_id.clone())
                .base_sha("base-sha")
                .classification(review_classification())
                .phase(ReviewBatchPhase::PreMerge)
                .pr_number(42)
                .pr_url("https://github.com/example/repo/pull/42")
                .target_sha("head-sha")
                .build(),
            "https://github.com/example/repo",
        )
        .unwrap();
    let (batch_id, leaf_execution_id) = match dispatch {
        boss_work_db::ReviewBatchDispatch::Created { batch, executions } => (batch.id, executions[0].id.clone()),
        other => panic!("expected a fresh review batch, got {other:?}"),
    };
    (
        Fixture::on_spoke(state, dir, leaf_execution_id, work_item_id, true),
        batch_id,
    )
}

fn is_terminal(fx: &Fixture) -> bool {
    fx.state
        .work_db
        .get_execution(&fx.execution_id)
        .unwrap()
        .status
        .is_terminal()
}

#[tokio::test]
async fn a_reporting_member_is_finalized_after_a_delivery_that_missed_first() {
    let (mut fx, batch) = review_member_on_a_spoke();
    fx.receive_proposal(
        "report-1",
        ProposalKind::ReviewReport,
        v1::ProposalKind::ReviewReport,
        review_report_payload(&batch, "head-sha"),
    );
    let projector = fx.projector();
    fx.wire.unreachable.store(true, Ordering::SeqCst);
    projector.drain().await;
    projector.drain().await;
    assert!(fx.wire.decisions().is_empty());
    assert!(
        !is_terminal(&fx),
        "a member is not torn down before the spoke has its answer"
    );

    fx.wire.unreachable.store(false, Ordering::SeqCst);
    projector.drain().await;
    assert_eq!(fx.wire.decisions().len(), 1);
    eventually("the member to finalize once its decision was delivered", || {
        is_terminal(&fx)
    })
    .await;
}

#[tokio::test]
async fn a_reporting_member_is_finalized_when_the_hub_restarts_before_its_decision_is_delivered() {
    let (mut fx, batch) = review_member_on_a_spoke();
    fx.receive_proposal(
        "report-1",
        ProposalKind::ReviewReport,
        v1::ProposalKind::ReviewReport,
        review_report_payload(&batch, "head-sha"),
    );
    fx.wire.unreachable.store(true, Ordering::SeqCst);
    fx.projector().drain().await;
    assert!(!is_terminal(&fx));

    // A restarted hub has a fresh applier: nothing remembers the submission.
    fx.wire.unreachable.store(false, Ordering::SeqCst);
    fx.projector().drain().await;
    assert_eq!(fx.wire.decisions().len(), 1);
    eventually("the restarted hub to finalize the member", || is_terminal(&fx)).await;
}

#[tokio::test]
async fn a_refused_settlement_does_not_finalize_an_accepted_member_but_an_accepted_one_does() {
    use boss_engine_runtime::hub::projection::{DecisionSettled, Settlement};
    let (mut fx, batch) = review_member_on_a_spoke();
    fx.receive_proposal(
        "report-1",
        ProposalKind::ReviewReport,
        v1::ProposalKind::ReviewReport,
        review_report_payload(&batch, "head-sha"),
    );
    fx.wire.unreachable.store(true, Ordering::SeqCst);
    fx.projector().drain().await;
    assert!(!is_terminal(&fx));
    let effect_key = fx
        .state
        .work_db
        .pending_spoke_effects(10)
        .unwrap()
        .into_iter()
        .find(|effect| effect.kind == "proposal_decision")
        .expect("the accepted proposal owes a decision")
        .key;

    // A fresh applier holds no submission, as after a restart or when another
    // generation's refusal settles under a colliding key.
    let applier = crate::app::spoke_projection::applier(&fx.state);
    let settle = |code: v1::ResultCode| {
        let (applier, effect_key, execution_id) = (applier.clone(), effect_key.clone(), fx.execution_id.clone());
        async move {
            applier
                .decision_settled(&DecisionSettled {
                    effect_key: &effect_key,
                    execution_id: &execution_id,
                    proposal_id: "report-1",
                    settlement: Settlement::Delivered,
                    code: code as i32,
                })
                .await;
        }
    };
    settle(v1::ResultCode::Rejected).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !is_terminal(&fx),
        "a refused settlement must not finish an accepted proposal"
    );

    settle(v1::ResultCode::Ok).await;
    eventually("the accepted settlement to finalize the member", || is_terminal(&fx)).await;
}

#[tokio::test]
async fn an_answer_reply_refreshes_the_thread_like_the_local_handler() {
    let mut fx = delivered_with_a_resolvable_pr();
    let comment = answer_agent_comment(&fx);
    let sink = watch(
        &fx.state,
        boss_protocol::comment_topic(&comment.artifact_kind, &comment.artifact_id),
    )
    .await;
    fx.receive_run_write(
        boss_protocol_spoke::proposal::ANSWER_REPLY,
        "answer-1",
        boss_protocol_spoke::proposal::answer_reply_body(&fx.execution_id, "because"),
    );
    fx.projector().drain().await;
    assert_eq!(fx.wire.decisions()[0].code, ResultCode::Ok as i32);
    assert_eq!(
        invalidation_reasons(&received(&sink).await),
        ["comment_answered"],
        "subscribers refresh the thread"
    );
}

#[tokio::test]
async fn a_guide_outcome_refreshes_the_comment_and_announces_a_requested_regeneration() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = 'revision_implementation' WHERE id = ?1",
            [&fx.execution_id],
        )
        .unwrap();
    let (_root, series) = boss_work_db::test_support::seed_published_guide(fx.state.work_db.as_ref(), 9);
    let comment = boss_work_db::test_support::add_guide_comment(fx.state.work_db.as_ref(), &series, "fix retry");
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_comments SET status = 'in_revision', revise_task_id = ?2 WHERE id = ?1",
            [&comment.id, &fx.work_item_id],
        )
        .unwrap();
    let sink = watch(
        &fx.state,
        boss_protocol::comment_topic(&comment.artifact_kind, &comment.artifact_id),
    )
    .await;
    fx.receive_run_write(
        boss_protocol_spoke::proposal::GUIDE_OUTCOME,
        "guide-regen",
        boss_protocol_spoke::proposal::guide_outcome_body(
            &fx.execution_id,
            &comment.id,
            &boss_protocol::GuideCommentDisposition::SourceChanged,
            "fixed",
            true,
        ),
    );
    fx.projector().drain().await;
    assert_eq!(
        fx.wire.decisions()[0].code,
        ResultCode::Ok as i32,
        "{:?}",
        fx.wire.decisions()
    );
    let reasons = invalidation_reasons(&received(&sink).await);
    assert!(
        reasons.contains(&"guide_comment_outcome_recorded".to_owned()),
        "{reasons:?}"
    );
    assert!(
        fx.state
            .work_db
            .get_spoke_applied_write("guide-regen")
            .unwrap()
            .is_some_and(|write| write.accepted),
        "the result row commits with the outcome"
    );
}

#[tokio::test]
async fn a_conflict_mark_failed_broadcasts_the_failure_and_checks_the_reason_itself() {
    let mut fx = delivered_with_a_resolvable_pr();
    let attempt = conflict_attempt(&fx);
    let sink = watch(&fx.state, boss_protocol::work_product_topic(&fx.product_id())).await;
    // The hub does not trust the spoke's own reason check.
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CONFLICT_MARK_FAILED,
        "mark-bad",
        boss_protocol_spoke::proposal::conflict_mark_failed_body(
            &fx.execution_id,
            &attempt.id,
            "made_up_reason",
            json!({}),
        ),
    );
    fx.projector().drain().await;
    assert_eq!(fx.wire.decisions()[0].code, ResultCode::Rejected as i32);
    assert_eq!(
        fx.state
            .work_db
            .get_conflict_resolution(&attempt.id)
            .unwrap()
            .unwrap()
            .status,
        "pending"
    );

    fx.receive_run_write(
        boss_protocol_spoke::proposal::CONFLICT_MARK_FAILED,
        "mark-ok",
        boss_protocol_spoke::proposal::conflict_mark_failed_body(
            &fx.execution_id,
            &attempt.id,
            "obsolescence_suspected",
            json!({}),
        ),
    );
    fx.projector().drain().await;
    assert_eq!(fx.wire.decisions()[1].code, ResultCode::Ok as i32);
    let events = received(&sink).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, FrontendEvent::ConflictResolutionFailed { attempt_id, .. } if attempt_id == &attempt.id)),
        "the failure appears in the activity feed: {events:?}"
    );
}

#[tokio::test]
async fn a_ci_mark_failed_broadcasts_the_failure() {
    let mut fx = delivered_with_a_resolvable_pr();
    let attempt = fx
        .state
        .work_db
        .insert_ci_remediation(boss_work_db::CiRemediationInsertInput {
            product_id: fx.product_id(),
            work_item_id: fx.work_item_id.clone(),
            pr_url: PR_URL.into(),
            pr_number: 9,
            head_branch: "feature".into(),
            head_sha_at_trigger: "a".repeat(40),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("pending ci attempt");
    let sink = watch(&fx.state, boss_protocol::work_product_topic(&fx.product_id())).await;
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CI_MARK_FAILED,
        "ci-broadcast",
        boss_protocol_spoke::proposal::ci_mark_failed_body(&fx.execution_id, &attempt.id, "wont_fix", json!({})),
    );
    fx.projector().drain().await;
    assert_eq!(fx.wire.decisions()[0].code, ResultCode::Ok as i32);
    let events = received(&sink).await;
    assert!(
        events.iter().any(
            |event| matches!(event, FrontendEvent::CiRemediationFailed { attempt_id, .. } if attempt_id == &attempt.id)
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_producer_conflict_is_counted_by_class() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CONFLICT_PRODUCER,
        "producer-metric",
        boss_protocol_spoke::proposal::conflict_producer_body(
            &fx.execution_id,
            "feature",
            "main",
            vec!["Cargo.lock".into()],
        ),
    );
    fx.projector().drain().await;
    let recorded = fx
        .state
        .work_db
        .latest_conflict_resolution_for_work_item(&fx.work_item_id)
        .unwrap()
        .expect("row");
    let class = recorded.conflict_class.expect("classified");
    let name = format!(
        "conflict.{}.{}.classified",
        boss_engine_merge_poller::sanitize_metric_name_component(&fx.product_id()),
        boss_engine_merge_poller::sanitize_metric_name_component(&class),
    );
    assert_eq!(fx.state.metrics.counter_value(&name), Some(1), "{name}");
}

#[tokio::test]
async fn a_busy_database_at_the_domain_write_retries_and_stores_no_refusal() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CONFLICT_PRODUCER,
        "producer-busy",
        boss_protocol_spoke::proposal::conflict_producer_body(
            &fx.execution_id,
            "feature",
            "main",
            vec!["src/lib.rs".into()],
        ),
    );
    // The result lookup before the write succeeds (readers are not blocked), and
    // the write itself then meets a held writer lock.
    let holder = fx.state.work_db.connect_new().unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    fx.state
        .work_db
        .connect()
        .unwrap()
        .busy_timeout(std::time::Duration::from_millis(30))
        .unwrap();
    let projector = fx.projector();
    let mut draining = Box::pin(projector.drain());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(400), &mut draining)
            .await
            .is_err(),
        "the record is held while the database is busy"
    );
    let effects: i64 = holder
        .query_row("SELECT COUNT(*) FROM spoke_effects", [], |row| row.get(0))
        .unwrap();
    assert_eq!(effects, 0, "no decision, and so no stored refusal, while busy");
    assert!(
        fx.state
            .work_db
            .get_spoke_applied_write("producer-busy")
            .unwrap()
            .is_none()
    );
    holder.execute_batch("ROLLBACK").unwrap();
    draining.await;
    let decisions = fx.wire.decisions();
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert_eq!(decisions[0].code, ResultCode::Ok as i32);
    assert!(
        fx.state
            .work_db
            .get_spoke_applied_write("producer-busy")
            .unwrap()
            .is_some_and(|write| write.accepted)
    );
}

#[tokio::test]
async fn a_failed_answer_transaction_leaves_nothing_behind() {
    let mut fx = delivered_with_a_resolvable_pr();
    let comment = answer_agent_comment(&fx);
    // The comment's final transition fails after the run and thread were written.
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_answer BEFORE UPDATE OF status ON work_comments
             WHEN NEW.status = 'answered' BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
        )
        .unwrap();
    fx.receive_run_write(
        boss_protocol_spoke::proposal::ANSWER_REPLY,
        "answer-rollback",
        boss_protocol_spoke::proposal::answer_reply_body(&fx.execution_id, "because"),
    );
    fx.projector().drain().await;
    assert_eq!(fx.wire.decisions()[0].code, ResultCode::Rejected as i32);
    assert!(
        fx.state
            .work_db
            .list_comment_thread_entries(&comment.id)
            .unwrap()
            .is_empty(),
        "the thread entry rolled back with the failed transition"
    );
    let run_status: String = fx
        .state
        .work_db
        .connect()
        .unwrap()
        .query_row(
            "SELECT status FROM answer_agent_runs WHERE comment_id = ?1",
            [&comment.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "running", "the run was not left half-completed");
    assert!(
        fx.state
            .work_db
            .get_spoke_applied_write("answer-rollback")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_conflict_mark_failed_replays_after_a_crash_and_commits_with_its_result() {
    let mut fx = delivered_with_a_resolvable_pr();
    let attempt = conflict_attempt(&fx);
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CONFLICT_MARK_FAILED,
        "mark-crash",
        boss_protocol_spoke::proposal::conflict_mark_failed_body(
            &fx.execution_id,
            &attempt.id,
            "obsolescence_suspected",
            json!({}),
        ),
    );
    fx.projector().drain().await;
    assert!(
        fx.state
            .work_db
            .get_spoke_applied_write("mark-crash")
            .unwrap()
            .is_some_and(|write| write.accepted),
        "the result row commits with the failure"
    );
    fx.crash_before_the_cursor_moved(0);
    fx.projector().drain().await;
    let stored = fx.state.work_db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(stored.status, "failed");
    let decisions = fx.wire.decisions();
    assert!(
        decisions.iter().all(|decision| decision.code == ResultCode::Ok as i32),
        "a replayed write is accepted again, not refused: {decisions:?}"
    );
}

#[tokio::test]
async fn a_failed_result_insert_rolls_the_domain_write_back() {
    let mut fx = delivered_with_a_resolvable_pr();
    let attempt = conflict_attempt(&fx);
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_result BEFORE INSERT ON spoke_applied_writes
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
        )
        .unwrap();
    fx.receive_run_write(
        boss_protocol_spoke::proposal::CONFLICT_MARK_FAILED,
        "mark-atomic",
        boss_protocol_spoke::proposal::conflict_mark_failed_body(
            &fx.execution_id,
            &attempt.id,
            "obsolescence_suspected",
            json!({}),
        ),
    );
    fx.projector().drain().await;
    assert_eq!(
        fx.state
            .work_db
            .get_conflict_resolution(&attempt.id)
            .unwrap()
            .unwrap()
            .status,
        "pending",
        "no applied failure without its result row"
    );
}

#[tokio::test]
async fn a_hook_is_not_fanned_out_again_while_its_turn_state_cannot_be_saved() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_event(&boss_protocol::WorkerEvent::Stop {
        session_id: "main".into(),
        stop_hook_active: false,
        stop_reason: boss_protocol::StopReason::AwaitingInput,
    });
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("ALTER TABLE spoke_turn_state RENAME TO spoke_turn_state_hidden")
        .unwrap();
    let projector = fx.projector();
    let mut draining = Box::pin(projector.drain());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), &mut draining)
            .await
            .is_err()
    );
    assert_eq!(
        fx.state.dispatcher_stats.snapshot().hook_events_total,
        0,
        "retries of a record whose turn state cannot be saved do not dispatch it"
    );
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("ALTER TABLE spoke_turn_state_hidden RENAME TO spoke_turn_state")
        .unwrap();
    draining.await;
    assert_eq!(fx.state.dispatcher_stats.snapshot().hook_events_total, 1);
}

#[tokio::test]
async fn turn_restoration_is_retried_without_another_hook() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_event(&boss_protocol::WorkerEvent::Stop {
        session_id: "main".into(),
        stop_hook_active: false,
        stop_reason: boss_protocol::StopReason::AwaitingInput,
    });
    fx.projector().drain().await;
    // A restart: the journal is fully applied, and the first restore fails.
    let db = (*fx.state.work_db).clone();
    let (fresh, _dir) = test_server_state_with_overrides(
        ServerStateOverrides::builder()
            .work_db(db.clone())
            .skip_hub_installation(true)
            .cube_client(Arc::new(boss_engine_runtime::test_support::AlwaysSucceedsCube))
            .execution_runner(Arc::new(boss_engine_runtime::test_support::AlwaysSucceedsRunner))
            .build(),
    );
    db.connect()
        .unwrap()
        .execute_batch("ALTER TABLE spoke_turn_state RENAME TO spoke_turn_state_hidden")
        .unwrap();
    assert!(fresh.restore_spoke_turns().is_err());
    assert!(!crate::app::spoke_probes::spoke_parked_for_delivery(
        &fresh,
        &fx.execution_id
    ));
    db.connect()
        .unwrap()
        .execute_batch("ALTER TABLE spoke_turn_state_hidden RENAME TO spoke_turn_state")
        .unwrap();
    // No hook arrives; the projector's sweep does the retry.
    crate::app::spoke_projection::applier(&fresh).sweep().await;
    assert!(crate::app::spoke_probes::spoke_parked_for_delivery(
        &fresh,
        &fx.execution_id
    ));
}

#[tokio::test]
async fn a_stall_write_that_fails_is_retried_until_engine_health_shows_it() {
    let mut fx = delivered_with_a_resolvable_pr();
    fx.receive_event(&boss_protocol::WorkerEvent::Stop {
        session_id: "main".into(),
        stop_hook_active: false,
        stop_reason: boss_protocol::StopReason::AwaitingInput,
    });
    // Both the record's apply and the first stall write fail.
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("ALTER TABLE spoke_turn_state RENAME TO spoke_turn_state_hidden")
        .unwrap();
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_stall_update BEFORE UPDATE OF stall_reason ON spoke_projection
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;
             CREATE TRIGGER fail_stall_insert BEFORE INSERT ON spoke_projection
             WHEN NEW.stall_reason IS NOT NULL
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
        )
        .unwrap();
    let projector = fx.projector_with(
        ProjectorConfig::builder()
            .retry_initial(std::time::Duration::from_millis(5))
            .retry_max(std::time::Duration::from_millis(20))
            .retry_stall(std::time::Duration::from_millis(10))
            .build(),
    );
    let draining = tokio::spawn({
        let projector = projector.clone();
        async move { projector.drain().await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        fx.state
            .work_db
            .spoke_projection_status()
            .unwrap()
            .iter()
            .all(|host| host.stall_reason.is_none()),
        "the stall could not be written yet"
    );
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_stall_update; DROP TRIGGER fail_stall_insert")
        .unwrap();
    let health = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if fx
                .state
                .work_db
                .spoke_projection_status()
                .unwrap()
                .iter()
                .any(|host| host.stall_reason.is_some())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(health.is_ok(), "the stall is written by a later retry");
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("ALTER TABLE spoke_turn_state_hidden RENAME TO spoke_turn_state")
        .unwrap();
    draining.await.unwrap();
}

#[tokio::test]
async fn a_stop_that_names_transcript_bytes_runs_against_the_hub_copy_of_them() {
    use boss_engine_runtime::hub::projection::Custody;
    use sha2::{Digest, Sha256};

    let mut fx = delivered_with_a_resolvable_pr();
    let line = b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n";
    let mut stop = worker_event::to_wire(&boss_protocol::WorkerEvent::Stop {
        session_id: "remote-session".into(),
        stop_hook_active: false,
        stop_reason: boss_protocol::StopReason::AwaitingInput,
    });
    if let Some(v1::worker_event::Event::Stop(turn)) = &mut stop.event {
        turn.transcript_generation = "0-1".into();
        turn.transcript_through = line.len() as u64;
    }
    // The spoke numbers the Stop at once; its bytes follow.
    fx.receive(record::Body::Event(stop));
    fx.receive(record::Body::Transcript(v1::TranscriptDelta {
        generation: "0-1".into(),
        offset: 0,
        data: line.to_vec(),
        sha256: Sha256::digest(line).to_vec(),
        eof: true,
    }));
    fx.receive(record::Body::Gap(v1::Gap {
        resource: "output:concluded".into(),
        reason: "[]".into(),
        ..Default::default()
    }));
    let custody = Arc::new(Custody::new(
        fx.state.work_db.as_ref().clone(),
        fx._dir.path().join("custody"),
    ));
    Projector::with_custody(
        fx.state.work_db.as_ref().clone(),
        crate::app::spoke_projection::applier(&fx.state),
        fx.wire.clone(),
        Arc::new(tokio::sync::Notify::new()),
        ProjectorConfig::builder()
            .retry_initial(std::time::Duration::from_millis(5))
            .retry_max(std::time::Duration::from_millis(20))
            .watermark_poll(std::time::Duration::from_millis(5))
            .build(),
        custody.clone(),
    )
    .drain()
    .await;

    // The run is pointed at the hub's own copy, which is what the live summary,
    // cost capture and completion open. The host's path never is.
    let recorded = fx
        .state
        .work_db
        .transcript_path_for_execution(&fx.execution_id)
        .unwrap()
        .expect("the run records its transcript");
    assert!(
        std::path::Path::new(&recorded).starts_with(custody.root()),
        "{recorded} is not under the custody root"
    );
    assert_eq!(std::fs::read(&recorded).unwrap(), line);
    assert_eq!(
        fx.state
            .work_db
            .spoke_projection_position(HOST)
            .unwrap()
            .unwrap()
            .applied,
        3
    );
    // A spoke run gets a summarizer of its own, because it now has a file to read.
    let slot = fx
        .state
        .live_worker_states
        .snapshot()
        .into_iter()
        .find(|slot| slot.run_id == fx.execution_id)
        .expect("the run has a slot");
    assert!(fx.state.live_status_manager.active_slot_ids().contains(&slot.slot_id));
}

#[tokio::test]
async fn a_newer_transcript_generation_advances_the_run_pointer() {
    use boss_engine_runtime::hub::projection::Custody;
    use sha2::{Digest, Sha256};

    let mut fx = delivered_with_a_resolvable_pr();
    let first = b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"one\"}}\n";
    let second = b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"two\"}}\n";
    fx.receive(record::Body::Transcript(v1::TranscriptDelta {
        generation: "0-1".into(),
        offset: 0,
        data: first.to_vec(),
        sha256: Sha256::digest(first).to_vec(),
        eof: true,
    }));
    let custody = Arc::new(Custody::new(
        fx.state.work_db.as_ref().clone(),
        fx._dir.path().join("custody"),
    ));
    let config = ProjectorConfig::builder()
        .retry_initial(std::time::Duration::from_millis(5))
        .retry_max(std::time::Duration::from_millis(20))
        .watermark_poll(std::time::Duration::from_millis(5))
        .build();
    Projector::with_custody(
        fx.state.work_db.as_ref().clone(),
        crate::app::spoke_projection::applier(&fx.state),
        fx.wire.clone(),
        Arc::new(tokio::sync::Notify::new()),
        config.clone(),
        custody.clone(),
    )
    .drain()
    .await;
    let first_path = fx
        .state
        .work_db
        .transcript_path_for_execution(&fx.execution_id)
        .unwrap()
        .expect("first generation is recorded");
    assert_eq!(std::fs::read(&first_path).unwrap(), first);

    fx.receive(record::Body::Transcript(v1::TranscriptDelta {
        generation: "0-2".into(),
        offset: 0,
        data: second.to_vec(),
        sha256: Sha256::digest(second).to_vec(),
        eof: true,
    }));
    Projector::with_custody(
        fx.state.work_db.as_ref().clone(),
        crate::app::spoke_projection::applier(&fx.state),
        fx.wire.clone(),
        Arc::new(tokio::sync::Notify::new()),
        config,
        custody.clone(),
    )
    .drain()
    .await;
    let second_path = fx
        .state
        .work_db
        .transcript_path_for_execution(&fx.execution_id)
        .unwrap()
        .expect("second generation is recorded");
    assert_ne!(first_path, second_path, "each generation is a different file");
    assert_eq!(std::fs::read(&second_path).unwrap(), second);
    assert_eq!(
        fx.state.transcript_path_cache.get(&fx.execution_id).as_deref(),
        Some(second_path.as_str())
    );
}

#[tokio::test]
async fn transcript_pointer_retry_persists_before_advancing_the_cache() {
    use boss_engine_runtime::hub::projection::{Origin, TranscriptSource};
    let fx = delivered_with_a_resolvable_pr();
    let applier = crate::app::spoke_projection::applier(&fx.state);
    let origin = Origin {
        host_id: HOST.into(),
        journal_id: "journal".into(),
        sequence: 1,
        observed: Default::default(),
        run: fx.run(),
    };
    let first = TranscriptSource {
        path: fx._dir.path().join("first"),
        root: fx._dir.path().to_owned(),
        generation: "0-1".into(),
        materialized: 1,
    };
    assert!(matches!(
        applier.transcript_attached(&origin, &first).await,
        boss_engine_runtime::hub::projection::Applied::Done
    ));
    fx.state.work_db.connect().unwrap().execute_batch(
        "CREATE TRIGGER fail_transcript_pointer BEFORE UPDATE OF transcript_path ON work_runs BEGIN SELECT RAISE(FAIL, 'pointer unavailable'); END;"
    ).unwrap();
    let second = TranscriptSource {
        path: fx._dir.path().join("second"),
        generation: "0-2".into(),
        ..first.clone()
    };
    assert!(matches!(
        applier.transcript_attached(&origin, &second).await,
        boss_engine_runtime::hub::projection::Applied::Retry(_)
    ));
    assert_eq!(
        fx.state.transcript_path_cache.get(&fx.execution_id).unwrap(),
        first.path.to_str().unwrap()
    );
    fx.state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_transcript_pointer;")
        .unwrap();
    assert!(matches!(
        applier.transcript_attached(&origin, &second).await,
        boss_engine_runtime::hub::projection::Applied::Done
    ));
    assert_eq!(
        fx.state
            .work_db
            .transcript_path_for_execution(&fx.execution_id)
            .unwrap()
            .unwrap(),
        second.path.to_str().unwrap()
    );
    assert_eq!(
        fx.state.transcript_path_cache.get(&fx.execution_id).unwrap(),
        second.path.to_str().unwrap()
    );
}

#[tokio::test]
async fn custody_consumers_bound_reads_and_refuse_a_halted_copy() {
    use boss_engine_runtime::hub::projection::Custody;
    use boss_engine_runtime::live_status_loop::TranscriptPathResolver;
    use sha2::{Digest, Sha256};
    let mut fx = delivered_with_a_resolvable_pr();
    let root = boss_engine_driver::transcript_store::transcript_store_root().unwrap();
    let custody = Arc::new(Custody::new(fx.state.work_db.as_ref().clone(), &root));
    let bytes = b"{\"type\":\"assistant\",\"message\":{\"id\":\"a\",\"model\":\"claude-opus\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n";
    fx.receive(record::Body::Transcript(v1::TranscriptDelta {
        generation: "0-1".into(),
        offset: 0,
        data: bytes.to_vec(),
        sha256: Sha256::digest(bytes).to_vec(),
        eof: false,
    }));
    Projector::with_custody(
        fx.state.work_db.as_ref().clone(),
        crate::app::spoke_projection::applier(&fx.state),
        fx.wire.clone(),
        Arc::new(tokio::sync::Notify::new()),
        ProjectorConfig::default(),
        custody.clone(),
    )
    .drain()
    .await;
    let path = std::path::PathBuf::from(
        fx.state
            .work_db
            .transcript_path_for_execution(&fx.execution_id)
            .unwrap()
            .unwrap(),
    );
    let resolver = crate::app::spoke_projection::SpokeSlotResolver(fx.state.clone());
    let trusted = resolver.transcript_containment_root(&fx.execution_id).await.unwrap();
    assert_eq!(trusted, custody.run_dir(&fx.execution_id, &fx.run().run_id).unwrap());
    let mut physical = bytes.to_vec();
    physical.extend_from_slice(b"{\"uncommitted\":true}\n");
    std::fs::write(&path, physical).unwrap();
    // Exercise the production spoke resolver through a Codex summary slot.
    fx.state
        .live_worker_states
        .register_spawn(1, &fx.execution_id, "codex", 0, None);
    let driver = boss_engine_driver::DriverRegistry::default().require("codex").unwrap();
    crate::app::spoke_projection::start_summary_slot(&fx.state, 1, &fx.execution_id, driver);
    fx.state.live_status_manager.notify(
        1,
        boss_engine_runtime::live_status_loop::Trigger::ActivityChanged(boss_protocol::WorkerActivity::Working),
    );
    for _ in 0..100 {
        if fx
            .state
            .live_status_manager
            .debug_store()
            .snapshot_for(1)
            .transcript_path
            .is_some()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        fx.state
            .live_status_manager
            .debug_store()
            .snapshot_for(1)
            .transcript_path
            .as_deref(),
        path.to_str()
    );
    fx.state.live_status_manager.stop_slot_for_run(1, &fx.execution_id);
    let limit = resolver.transcript_read_limit(&path).await.unwrap();
    assert_eq!(limit, bytes.len() as u64);
    let mut tail = boss_engine_runtime::transcript_tail::TranscriptTail::new_contained(&path, &trusted).unwrap();
    assert_eq!(tail.poll_through(limit).await.unwrap().len(), 1);
    assert_eq!(
        crate::app::executions::read_transcript_content(&fx.state.work_db, path.to_str().unwrap())
            .await
            .unwrap()
            .as_bytes(),
        bytes
    );
    assert!(
        fx.state
            .run_cost_capture
            .capture_and_persist(&fx.state.work_db, &fx.execution_id, &path, Some(&trusted))
            .await
            .is_ok()
    );
    let outside = fx._dir.path().join("outside.jsonl");
    std::fs::write(&outside, bytes).unwrap();
    fx.state
        .work_db
        .set_run_transcript_path(&fx.execution_id, outside.to_str().unwrap())
        .unwrap();
    assert_eq!(
        resolver.transcript_containment_root(&fx.execution_id).await.unwrap(),
        trusted
    );
    assert!(boss_engine_runtime::transcript_tail::TranscriptTail::new_contained(&outside, &trusted).is_err());
    fx.state
        .work_db
        .set_run_transcript_path(&fx.execution_id, path.to_str().unwrap())
        .unwrap();
    // A hole halts this already-attached resource.
    fx.receive(record::Body::Transcript(v1::TranscriptDelta {
        generation: "0-1".into(),
        offset: 9999,
        data: b"x".to_vec(),
        sha256: Sha256::digest(b"x").to_vec(),
        eof: false,
    }));
    Projector::with_custody(
        fx.state.work_db.as_ref().clone(),
        crate::app::spoke_projection::applier(&fx.state),
        fx.wire.clone(),
        Arc::new(tokio::sync::Notify::new()),
        ProjectorConfig::default(),
        custody,
    )
    .drain()
    .await;
    assert!(resolver.transcript_read_limit(&path).await.is_err());
    assert!(matches!(
        crate::app::handler_helpers::resolve_transcript_for_tail(&fx.state, &fx.execution_id),
        crate::app::handler_helpers::TranscriptResolution::KnownNoTranscript
    ));
    assert!(
        crate::app::executions::read_transcript_content(&fx.state.work_db, path.to_str().unwrap())
            .await
            .is_err()
    );
    assert!(
        fx.state
            .run_cost_capture
            .capture_and_persist(&fx.state.work_db, &fx.execution_id, &path, Some(&trusted))
            .await
            .is_err()
    );
    assert_eq!(
        crate::app::worker_events::transcript_offset_for_run(&fx.state, &fx.execution_id).await,
        (None, 0)
    );
}
