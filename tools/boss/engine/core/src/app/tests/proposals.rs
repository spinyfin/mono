// Behaviour tests for the worker proposal RPC surface: `SubmitProposal`
// and `ListProposals` dispatched into `app::proposals`.
//
// Every assertion goes request-in / response-out: a `FrontendRequest` is
// handed to its handler and the `FrontendEvent` it sends back is what gets
// checked. Nothing asserts which `WorkDb` method a handler called — the
// storage layer has its own direct coverage in `work/proposals.rs`, and the
// payload schemas in the `proposal-validation` crate. What is unique to this
// layer, and therefore what is tested here, is the join of the three:
// attribution from the socket peer, validation refusals reaching the caller
// as typed errors, and idempotency/rate-cap outcomes rendered as events.
//
// Attribution is exercised with the test process's own pid registered as a
// worker: `is_descendant_of_any` / `lookup_with_ancestor_walk` both treat a
// pid as its own ancestor, so registering `std::process::id()` makes this
// process look exactly like a worker session to the handler — the same trick
// `t03` and `trust_authorization` use.

use super::*;
use crate::app::proposals;
use boss_protocol::{
    PROPOSAL_CAP_PER_KIND_PER_EXECUTION, ProposalErrorCode, ProposalKind, ProposalState, ProposalSubmissionError,
    ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket, ReviewProfile, WorkerProposal,
};
use serde_json::{Value, json};

fn operator_question_payload() -> Value {
    json!({"outcome":"blocked", "summary":"The scope needs authorization", "question":{
        "text":"Approve raising the file limit from 30 to 48?",
        "answer_type":{"kind":"yes_no"},
        "explanation":"All 48 files are needed for the requested migration."
    }})
}

async fn parked_question_task() -> (Arc<ServerState>, tempfile::TempDir, String, String) {
    let (state, dir, execution, task) = live_chore_execution();
    {
        let conn = state.work_db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET kind = 'project_task', description = 'Original brief' WHERE id = ?1",
            [&task],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET kind = 'task_implementation' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
    }
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&execution, ProposalKind::RunDone, operator_question_payload()),
        )
        .await,
    );
    (state, dir, execution, task)
}

fn question_task(state: &ServerState, id: &str) -> boss_protocol::Task {
    let crate::work::WorkItem::Task(task) = state.work_db.get_work_item(id).unwrap() else {
        panic!("expected task");
    };
    task
}

async fn answer_question(state: &Arc<ServerState>, id: &str, value: bool) -> FrontendEvent {
    call_with_peer(
        state,
        None,
        FrontendRequest::AnswerOperatorQuestion {
            id: id.into(),
            answer: boss_protocol::OperatorAnswer::YesNo { value },
        },
    )
    .await
}

#[tokio::test]
async fn question_finalize_preserves_workspace_and_never_auto_dispatches() {
    let (state, _dir, execution_id, task_id) = parked_question_task().await;
    let db = &state.work_db;
    let task = question_task(&state, &task_id);
    assert_eq!(task.status, boss_protocol::TaskStatus::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some("awaiting_operator_answer"));
    assert_eq!(
        task.blocked_detail.as_deref(),
        Some("Approve raising the file limit from 30 to 48?")
    );
    assert!(!task.autostart);
    let question = task.operator_question.as_ref().unwrap();
    assert_eq!(question.execution_id, execution_id);
    assert_eq!(question.run_summary.as_deref(), Some("The scope needs authorization"));
    assert_ne!(question.run_summary.as_deref(), Some(question.explanation.as_str()));
    let history = db.list_operator_questions(&task_id).unwrap();
    assert_eq!(history[0].question.run_summary, question.run_summary);
    assert_eq!(question.text, task.blocked_detail.as_deref().unwrap());
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, boss_protocol::ExecutionStatus::Failed);
    assert!(execution.cube_lease_id.is_none());
    assert!(execution.cube_workspace_id.is_none());
    assert!(execution.workspace_path.is_none());
    assert_eq!(execution.preferred_workspace_id.as_deref(), Some("workspace-run-done"));
    assert!(db.list_attention_items_for_work_item(&task_id).unwrap().is_empty());
    assert!(db.list_attention_items(&execution_id).unwrap().is_empty());
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET autostart = 1 WHERE id = ?1", [&task_id])
        .unwrap();
    db.reconcile_product_executions(&task.product_id).unwrap();
    db.reconcile_active_dispatch(|_| false).unwrap();
    assert_eq!(db.list_executions(Some(&task_id)).unwrap().len(), 1);
    let listed = db.list_tasks(&task.product_id, None, None, false).unwrap();
    assert_eq!(
        listed.iter().find(|item| item.id == task_id).unwrap().operator_question,
        task.operator_question
    );
    let tree = db.get_work_tree(&task.product_id).unwrap();
    assert_eq!(
        tree.tasks
            .iter()
            .find(|item| item.id == task_id)
            .unwrap()
            .operator_question,
        task.operator_question
    );
}

#[tokio::test]
async fn question_summary_survives_upgrade_of_existing_projection() {
    let (state, dir, _, task_id) = parked_question_task().await;
    let expected = question_task(&state, &task_id).operator_question.unwrap();
    state
        .work_db
        .connect()
        .unwrap()
        .execute_batch(
            "DROP INDEX work_runs_live_persona;
         DROP INDEX work_runs_execution_persona_lease;
         ALTER TABLE work_runs DROP COLUMN persona;
         ALTER TABLE work_runs DROP COLUMN persona_lease_active;
         DROP VIEW open_operator_questions;
         CREATE VIEW open_operator_questions AS
         SELECT work_item_id, json_patch(question_json,
             json_object('id', id, 'asked_at', created_at, 'execution_id', execution_id)) AS view_json
         FROM operator_questions WHERE status = 'open';
         UPDATE metadata SET value = '35' WHERE key = 'schema_version';",
        )
        .unwrap();
    assert!(
        question_task(&state, &task_id)
            .operator_question
            .unwrap()
            .run_summary
            .is_none()
    );
    let reopened = crate::work::WorkDb::open(dir.path().join("state.db")).unwrap();
    let crate::work::WorkItem::Task(task) = reopened.get_work_item(&task_id).unwrap() else {
        panic!("expected task");
    };
    assert_eq!(task.operator_question, Some(expected));
}

#[tokio::test]
async fn question_yes_appends_exact_authorization_and_mints_one_preserved_execution() {
    let (state, _dir, old_execution, task_id) = parked_question_task().await;
    let original = question_task(&state, &task_id);
    let question = original.operator_question.unwrap();
    let answer = answer_question(&state, &original.short_id.unwrap().to_string(), true).await;
    let FrontendEvent::WorkItemUpdated {
        item: crate::work::WorkItem::Task(task),
    } = answer
    else {
        panic!("expected updated task: {answer:?}");
    };
    assert_eq!(task.status, boss_protocol::TaskStatus::Todo);
    assert!(task.autostart);
    assert_eq!(task.last_status_actor, "human");
    assert!(task.blocked_reason.is_none());
    assert!(task.blocked_detail.is_none());
    assert!(task.operator_question.is_none());
    let history = state.work_db.list_operator_questions(&task_id).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, boss_protocol::OperatorQuestionStatus::Answered);
    assert_eq!(history[0].answered_by.as_deref(), Some("human"));
    let timestamp = chrono::DateTime::from_timestamp(history[0].answered_at.as_ref().unwrap().parse().unwrap(), 0)
        .unwrap()
        .format("%Y-%m-%d %H:%M UTC")
        .to_string();
    assert_eq!(
        task.description,
        format!(
            "Original brief\n\n---\n\n## Operator authorization ({timestamp})\n\n\
- **Question the previous worker asked:** Approve raising the file limit from 30 to 48?\n\
- **Answer:** Yes\n\
- **The worker's explanation:** All 48 files are needed for the requested migration.\n\
- **Asked by run:** `{old_execution}`\n\n\
The operator recorded this answer on the kanban. It is explicit, human-granted approval for exactly what the question asks and nothing broader. Treat it as the authorization the worker rules require before relaxing a check or exceeding a limit for this task; state in the PR body that the operator authorized it on the date above. It does not authorize bypassing any other check, and it does not change what the repository's checks enforce.\n"
        )
    );
    let executions = state.work_db.list_executions(Some(&task_id)).unwrap();
    assert_eq!(executions.len(), 2);
    let next = executions
        .iter()
        .find(|execution| execution.id != old_execution)
        .unwrap();
    assert_eq!(next.status, boss_protocol::ExecutionStatus::Ready);
    assert_eq!(next.preferred_workspace_id.as_deref(), Some("workspace-run-done"));
    assert!(next.allow_dirty && next.prefer_is_soft);
    for selector in [&question.id, &task_id] {
        assert!(matches!(
            answer_question(&state, selector, true).await,
            FrontendEvent::WorkItemUpdated { .. }
        ));
    }
    assert_eq!(state.work_db.list_executions(Some(&task_id)).unwrap().len(), 2);
    assert_eq!(question_task(&state, &task_id).description, task.description);
    assert!(matches!(answer_question(&state, &question.id, false).await,
        FrontendEvent::OperatorQuestionError { error: boss_protocol::OperatorQuestionError::Conflict {
            state, answer: Some(boss_protocol::OperatorAnswer::YesNo { value: true })
        }} if state == "answered"));
}

#[tokio::test]
async fn question_answer_reports_a_minted_execution_only_for_the_first_yes() {
    let yes = boss_protocol::OperatorAnswer::YesNo { value: true };
    let no = boss_protocol::OperatorAnswer::YesNo { value: false };

    let (state, _dir, _, task_id) = parked_question_task().await;
    let (_, minted) = state.work_db.answer_operator_question(&task_id, yes.clone()).unwrap();
    assert!(
        minted,
        "a first Yes mints a ready execution the scheduler must be kicked for"
    );
    let (_, minted) = state.work_db.answer_operator_question(&task_id, yes).unwrap();
    assert!(!minted, "an idempotent repeat mints nothing");

    let (state, _dir, _, task_id) = parked_question_task().await;
    let (_, minted) = state.work_db.answer_operator_question(&task_id, no).unwrap();
    assert!(!minted, "a No mints nothing");
}

async fn parked_question_task_with_dispatch_bus() -> (Arc<ServerState>, tempfile::TempDir, String) {
    // `kick()` only publishes `DispatchReady` when the bus flag is on, which
    // is what makes the wakeup observable without racing a real scheduler.
    let temp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RuntimeConfig::from_parts(
        crate::config::WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(temp.path().join("state.db"))
            .enable_dispatch_ready_bus(true)
            .build(),
        None,
    ));
    let state = ServerState::new_arc_with_app_pid_and_merge_probe(
        cfg,
        None,
        None,
        ServerStateOverrides {
            cube_client: Some(Arc::new(crate::test_support::AlwaysSucceedsCube)),
            execution_runner: Some(Arc::new(crate::test_support::AlwaysSucceedsRunner)),
            ..Default::default()
        },
    )
    .unwrap();
    state.feature_flags.set("worker_proposals", true).unwrap();
    state.feature_flags.set("run_done_proposals_seam", true).unwrap();
    let (execution, task) = new_execution(&state, "Run-done target");
    state
        .work_db
        .start_execution_run(
            &execution,
            "worker",
            "repo",
            "lease-run-done",
            "workspace-run-done",
            temp.path().to_str().unwrap(),
        )
        .unwrap();
    state
        .worker_registry
        .register(std::process::id() as libc::pid_t, execution.clone());
    {
        let conn = state.work_db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET kind = 'project_task', description = 'Original brief' WHERE id = ?1",
            [&task],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET kind = 'task_implementation' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
    }
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&execution, ProposalKind::RunDone, operator_question_payload()),
        )
        .await,
    );
    (state, temp, task)
}

async fn dispatch_ready_count(subscription: &mut boss_event_bus::Subscription) -> usize {
    let mut count = 0;
    while let Ok(Some(_)) = tokio::time::timeout(std::time::Duration::from_millis(200), subscription.recv()).await {
        count += 1;
    }
    count
}

/// A first Yes mints a ready execution, and answering through the RPC handler
/// must wake the scheduler so the restart does not wait for the heartbeat.
/// Only the Yes path is asserted: every handled answer also publishes a work
/// invalidation, which kicks the scheduler on its own, so a No or an
/// idempotent repeat is not a "no wakeup" case at this layer (the storage
/// layer's `minted` flag, tested above, is what distinguishes them).
#[tokio::test]
async fn question_yes_answer_through_the_handler_wakes_the_scheduler() {
    let (state, _dir, task_id) = parked_question_task_with_dispatch_bus().await;
    let mut wakeups = state
        .execution_coordinator
        .event_bus()
        .subscribe(boss_event_bus::TopicFilter::kind(
            boss_event_bus::EventKind::DispatchReady,
        ));
    assert_eq!(
        dispatch_ready_count(&mut wakeups).await,
        0,
        "nothing wakes before the answer"
    );
    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    assert!(
        dispatch_ready_count(&mut wakeups).await >= 1,
        "a Yes answer wakes the scheduler"
    );
    let task = question_task(&state, &task_id);
    assert!(task.autostart);
    assert_eq!(state.work_db.list_executions(Some(&task_id)).unwrap().len(), 2);
}

#[tokio::test]
async fn question_survives_external_ref_and_automation_reads() {
    let (state, _dir, _, task_id) = parked_question_task().await;
    let db = &state.work_db;
    let expected = question_task(&state, &task_id).operator_question;
    assert!(expected.is_some());
    db.set_external_ref(&task_id, "github", "spinyfin/mono#560", &json!({"issue_number": 560}))
        .unwrap();
    let automation = db
        .create_automation(
            boss_protocol::CreateAutomationInput::builder()
                .product_id(question_task(&state, &task_id).product_id)
                .name("auto-q")
                .trigger(boss_protocol::AutomationTrigger::Schedule {
                    cron: "0 14 * * *".to_owned(),
                    timezone: "UTC".to_owned(),
                })
                .standing_instruction("x")
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET source_automation_id = ?2 WHERE id = ?1",
            [&task_id, &automation.id],
        )
        .unwrap();

    let crate::work::WorkItem::Task(linked) = db.get_task_with_external_ref(&task_id).unwrap() else {
        panic!("expected task");
    };
    assert_eq!(linked.operator_question, expected);
    let found = db.find_by_external_ref("github", "spinyfin/mono#560").unwrap().unwrap();
    assert_eq!(found.operator_question, expected);
    let produced = db.list_tasks_for_automation(&automation.id).unwrap();
    assert_eq!(produced.len(), 1);
    assert_eq!(produced[0].operator_question, expected);
}

#[tokio::test]
async fn question_no_records_decline_and_remains_in_backlog() {
    let (state, _dir, _, task_id) = parked_question_task().await;
    assert!(matches!(
        answer_question(&state, &task_id, false).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    let task = question_task(&state, &task_id);
    assert_eq!(task.status, boss_protocol::TaskStatus::Blocked);
    assert!(!task.autostart);
    assert_eq!(task.blocked_reason.as_deref(), Some("worker_failed"));
    assert_eq!(task.description, "Original brief");
    assert!(task.operator_question.is_none());
    let record = state.work_db.list_operator_questions(&task_id).unwrap().remove(0);
    let date = chrono::DateTime::from_timestamp(record.answered_at.unwrap().parse().unwrap(), 0)
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    assert_eq!(
        task.blocked_detail.unwrap(),
        format!(
            "Operator declined on {date}: \"Approve raising the file limit from 30 to 48?\"\n\nWorker's explanation: All 48 files are needed for the requested migration.\n\nRun summary: The scope needs authorization"
        )
    );
    assert!(matches!(
        answer_question(&state, &task_id, false).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    assert_eq!(state.work_db.list_executions(Some(&task_id)).unwrap().len(), 1);
}

#[tokio::test]
async fn question_restart_delete_and_edits_withdraw_atomically() {
    for action in ["restart", "delete", "status", "reason"] {
        let (state, _dir, _, task_id) = parked_question_task().await;
        let question_id = question_task(&state, &task_id).operator_question.unwrap().id;
        let reason = match action {
            "restart" => {
                state
                    .work_db
                    .request_execution(
                        boss_protocol::RequestExecutionInput::builder()
                            .work_item_id(&task_id)
                            .build(),
                    )
                    .unwrap();
                "restarted_without_answer"
            }
            "delete" => {
                state.work_db.delete_work_item(&task_id).unwrap();
                "task_deleted"
            }
            "status" => {
                state
                    .work_db
                    .update_work_item(
                        &task_id,
                        boss_protocol::WorkItemPatch {
                            status: Some("todo".into()),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                "status_edited"
            }
            _ => {
                state
                    .work_db
                    .update_work_item(
                        &task_id,
                        boss_protocol::WorkItemPatch {
                            blocked_reason: Some("operator_override".into()),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                "reason_edited"
            }
        };
        let record = state.work_db.list_operator_questions(&task_id).unwrap().remove(0);
        assert_eq!(
            record.status,
            boss_protocol::OperatorQuestionStatus::Withdrawn,
            "{action}"
        );
        assert_eq!(record.withdrawn_reason.as_deref(), Some(reason));
        let response = answer_question(&state, &question_id, true).await;
        if action == "delete" {
            assert!(matches!(
                response,
                FrontendEvent::OperatorQuestionError {
                    error: boss_protocol::OperatorQuestionError::NotFound
                }
            ));
        } else {
            assert!(matches!(response, FrontendEvent::OperatorQuestionError {
                error: boss_protocol::OperatorQuestionError::Conflict { state, .. }
            } if state == "withdrawn"));
        }
    }
}

#[tokio::test]
async fn question_on_chore_folds_into_failure_and_unparkable_task_withdraws() {
    for kind in ["chore", "followup", "project_task"] {
        let (state, _dir, execution_id, task_id) = live_chore_execution();
        if kind == "followup" {
            state
                .work_db
                .connect()
                .unwrap()
                .execute(
                    "UPDATE tasks SET kind = ?2 WHERE id = ?1",
                    rusqlite::params![task_id, kind],
                )
                .unwrap();
        }
        if kind == "project_task" {
            state
                .work_db
                .connect()
                .unwrap()
                .execute(
                    "UPDATE tasks SET kind = 'project_task', status = 'in_review' WHERE id = ?1",
                    [&task_id],
                )
                .unwrap();
        }
        submitted(
            call_with_peer(
                &state,
                Some(std::process::id() as libc::pid_t),
                submit_request(&execution_id, ProposalKind::RunDone, operator_question_payload()),
            )
            .await,
        );
        let item = state.work_db.get_work_item(&task_id).unwrap();
        match item {
            crate::work::WorkItem::Chore(task) => {
                assert_eq!(task.blocked_reason.as_deref(), Some("worker_failed"));
                let detail = task.blocked_detail.unwrap();
                assert!(detail.contains("Approve raising the file limit from 30 to 48?"));
                assert!(detail.contains("All 48 files are needed"));
                assert!(state.work_db.list_operator_questions(&task_id).unwrap().is_empty());
            }
            crate::work::WorkItem::Task(task) => {
                assert_eq!(task.status, boss_protocol::TaskStatus::InReview);
                assert!(task.operator_question.is_none());
                let record = state.work_db.list_operator_questions(&task_id).unwrap().remove(0);
                assert_eq!(record.withdrawn_reason.as_deref(), Some("task_not_parkable"));
            }
            _ => panic!("expected task or chore"),
        }
        assert_eq!(
            state.work_db.get_execution(&execution_id).unwrap().status,
            boss_protocol::ExecutionStatus::Failed
        );
    }
}

#[tokio::test]
async fn question_answer_rolls_back_when_dispatch_cannot_create_an_execution() {
    let (state, _dir, _, task_id) = parked_question_task().await;
    let before = question_task(&state, &task_id);
    // Repository resolution is part of the dispatch transaction. A failure
    // must leave both the unanswered question and original brief intact.
    state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE products SET repo_remote_url = NULL WHERE id = ?1",
            [&before.product_id],
        )
        .unwrap();
    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkError { .. }
    ));
    let after = question_task(&state, &task_id);
    assert_eq!(after.description, before.description);
    assert_eq!(after.status, before.status);
    assert_eq!(after.blocked_reason, before.blocked_reason);
    assert_eq!(after.operator_question, before.operator_question);
    let records = state.work_db.list_operator_questions(&task_id).unwrap();
    assert_eq!(records[0].status, boss_protocol::OperatorQuestionStatus::Open);
    assert!(records[0].answer.is_none());
    assert_eq!(state.work_db.list_executions(Some(&task_id)).unwrap().len(), 1);
}

#[tokio::test]
async fn question_edit_between_acceptance_and_finalize_is_not_overwritten() {
    let (state, _dir, execution_id, task_id) = live_chore_execution();
    state.feature_flags.set("run_done_proposals_seam", false).unwrap();
    state
        .work_db
        .connect()
        .unwrap()
        .execute("UPDATE tasks SET kind = 'project_task' WHERE id = ?1", [&task_id])
        .unwrap();
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&execution_id, ProposalKind::RunDone, operator_question_payload()),
        )
        .await,
    );
    state
        .work_db
        .update_work_item(
            &task_id,
            boss_protocol::WorkItemPatch {
                status: Some("todo".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let question = state
        .work_db
        .execution_operator_question(&execution_id)
        .unwrap()
        .unwrap();
    state
        .work_db
        .record_worker_awaiting_operator_answer(&execution_id, "late finalization", &question)
        .unwrap();
    let task = question_task(&state, &task_id);
    assert_eq!(task.status, boss_protocol::TaskStatus::Todo);
    assert!(task.operator_question.is_none());
    let record = state.work_db.list_operator_questions(&task_id).unwrap().remove(0);
    assert_eq!(record.status, boss_protocol::OperatorQuestionStatus::Withdrawn);
    assert_eq!(record.withdrawn_reason.as_deref(), Some("status_edited"));
    assert!(matches!(
        answer_question(&state, &record.question.id, true).await,
        FrontendEvent::OperatorQuestionError {
            error: boss_protocol::OperatorQuestionError::Conflict { .. }
        }
    ));
}

// ── Fixtures ─────────────────────────────────────────────────────────────────

/// A product, chore, and `ready` execution, with this process registered as
/// the worker running that execution — i.e. the state a live worker session
/// presents to the engine.
struct WorkerFixture {
    server_state: Arc<ServerState>,
    _dir: tempfile::TempDir,
    execution_id: String,
    work_item_id: String,
    peer_pid: libc::pid_t,
}

impl WorkerFixture {
    fn new() -> Self {
        let (server_state, dir) = test_server_state();
        let (execution_id, work_item_id) = new_execution(&server_state, "Cleanup");
        let peer_pid = std::process::id() as libc::pid_t;
        server_state.worker_registry.register(peer_pid, execution_id.clone());
        Self {
            server_state,
            _dir: dir,
            execution_id,
            work_item_id,
            peer_pid,
        }
    }
}

/// Create a chore under a fresh product plus a `ready` execution for it,
/// returning `(execution_id, work_item_id)`.
fn new_execution(server_state: &Arc<ServerState>, chore_name: &str) -> (String, String) {
    let db = &server_state.work_db;
    let product = crate::test_support::create_test_product(db);
    let chore = crate::test_support::create_test_chore(db, product.id, chore_name);
    let execution = crate::test_support::create_ready_chore_execution(db, chore.id.clone());
    (execution.id, chore.id)
}

/// Build a per-request `Dispatch` the way `handle_frontend_connection` does
/// for a real socket frame, with `peer_pid` standing in for `SO_PEERCRED`.
fn dispatch_with_peer(state: &Arc<ServerState>, sink: &Arc<SessionSink>, peer_pid: Option<libc::pid_t>) -> Dispatch {
    Dispatch::builder()
        .server_state(state.clone())
        .work_db(state.work_db.clone())
        .sink(sink.clone())
        .session_id("session-test")
        .request_id("req-1")
        .maybe_peer_pid(peer_pid)
        .recv_instant(std::time::Instant::now())
        .decode_ms(0.0)
        .build()
}

/// Drive one proposal verb through its handler and return the reply.
/// Mirrors `app.rs`'s dispatch table for these two verbs.
async fn call_with_peer(
    state: &Arc<ServerState>,
    peer_pid: Option<libc::pid_t>,
    req: FrontendRequest,
) -> FrontendEvent {
    let sink = make_session_sink();
    let ctx = dispatch_with_peer(state, &sink, peer_pid);
    let handler = async {
        match req {
            r @ FrontendRequest::SubmitProposal { .. } => proposals::handle_submit_proposal(ctx, r).await,
            r @ FrontendRequest::ListProposals { .. } => proposals::handle_list_proposals(ctx, r).await,
            r @ FrontendRequest::GetWorkItem { .. } => super::super::work_items::handle_get_work_item(ctx, r).await,
            r @ FrontendRequest::AnswerOperatorQuestion { .. } | r @ FrontendRequest::ListOperatorQuestions { .. } => {
                super::super::work_items::handle_operator_question(ctx, r).await
            }
            other => panic!("not a supported test verb: {other:?}"),
        }
    };
    tokio::pin!(handler);
    let (response, handler_finished) = tokio::select! {
        response = sink.next() => (response.expect("handler must send a response"), false),
        _ = &mut handler => (sink.next().await.expect("handler must send a response"), true),
    };
    sink.complete_response_delivery(response.request_id.as_deref(), true);
    if !handler_finished {
        handler.await;
    }
    sink.close();
    assert!(sink.next().await.is_none(), "handler must send exactly one response");
    response.payload
}

fn submit_request(run_id: &str, kind: ProposalKind, payload: Value) -> FrontendRequest {
    FrontendRequest::SubmitProposal {
        run_id: run_id.to_owned(),
        kind,
        payload,
        idempotency_key: None,
    }
}

fn submit_request_keyed(run_id: &str, kind: ProposalKind, payload: Value, key: &str) -> FrontendRequest {
    FrontendRequest::SubmitProposal {
        run_id: run_id.to_owned(),
        kind,
        payload,
        idempotency_key: Some(key.to_owned()),
    }
}

async fn submit(fx: &WorkerFixture, kind: ProposalKind, payload: Value) -> FrontendEvent {
    call_with_peer(
        &fx.server_state,
        Some(fx.peer_pid),
        submit_request(&fx.execution_id, kind, payload),
    )
    .await
}

fn review_classification() -> ReviewClassification {
    ReviewClassification::builder()
        .changed_files(vec!["src/lib.rs".to_owned()])
        .complexity_flags(vec![])
        .has_production_code(true)
        .metadata_missing(vec![])
        .production_languages(vec![ReviewLanguageBucket::Rust])
        .profile(ReviewProfile::Light)
        .subsystem_buckets(vec!["src".to_owned()])
        .build()
}

fn review_report_payload(batch_id: &str, target_sha: &str) -> Value {
    json!({
        "batch_id": batch_id,
        "target_sha": target_sha,
        "report": {
            "batch_id": batch_id,
            "pr_url": "https://github.com/example/repo/pull/42",
            "target_sha": target_sha,
            "phase": "pre_merge",
            "summary": "Clean.",
            "coverage": {"files_inspected": [], "files_omitted": [], "limitations": []},
            "findings": []
        }
    })
}

/// Build one live batch leaf against a fake app runtime. Returning the same
/// data a real proposal RPC sees keeps the acceptance test below on the
/// complete attributed request → apply → teardown path.
fn live_batch_leaf() -> (Arc<ServerState>, tempfile::TempDir, String, String, String) {
    let (server_state, dir) = test_server_state_with_fakes();
    let (_ordinary_execution_id, work_item_id) = new_execution(&server_state, "Review target");
    let dispatch = server_state
        .work_db
        .create_pre_merge_review_batch(
            crate::work::ReviewBatchCreateInput::builder()
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
        crate::work::ReviewBatchDispatch::Created { batch, executions } => (batch.id, executions[0].id.clone()),
        other => panic!("expected a fresh review batch, got {other:?}"),
    };
    server_state
        .work_db
        .start_execution_run(
            &leaf_execution_id,
            "reviewer",
            "repo",
            "lease-reviewer",
            "workspace-reviewer",
            dir.path().to_str().unwrap(),
        )
        .unwrap();
    server_state
        .worker_registry
        .register(std::process::id() as libc::pid_t, leaf_execution_id.clone());
    (server_state, dir, work_item_id, batch_id, leaf_execution_id)
}

fn verdict_payload(batch_id: &str, target_sha: &str) -> Value {
    json!({
        "batch_id": batch_id,
        "verdict": {
            "batch_id": batch_id,
            "pr_url": "https://github.com/example/repo/pull/42",
            "target_sha": target_sha,
            "phase": "pre_merge",
            "summary": "Clean.",
            "revision_warranted": false,
            "findings": [],
            "contradictions": []
        }
    })
}

/// Build one live batch consolidator (the `Supervisor`-role member) against a
/// fake app runtime, mirroring [`live_batch_leaf`] but for the role a
/// `review_verdict` submission targets. The batch is forced straight to
/// `supervising` — the state `apply_review_verdict` requires — the same
/// shortcut `work/tests/review_batches_tests.rs::force_batch_supervising`
/// uses, since standing up three leaf reports first is not the point of
/// these tests.
fn live_batch_supervisor() -> (Arc<ServerState>, tempfile::TempDir, String, String, String) {
    use crate::work::{CreateExecutionInput, ReviewBatchCreateInput, ReviewBatchMemberCreateInput};
    use boss_protocol::{ExecutionKind, ReviewBatchMemberRole, ReviewBatchMemberStatus};

    let (server_state, dir) = test_server_state_with_fakes();
    let (_ordinary_execution_id, work_item_id) = new_execution(&server_state, "Review target");
    let execution = server_state
        .work_db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, _members) = server_state
        .work_db
        .create_review_batch(
            ReviewBatchCreateInput::builder()
                .cycle_root_id(work_item_id.clone())
                .base_sha("base-sha")
                .classification(review_classification())
                .phase(ReviewBatchPhase::PreMerge)
                .pr_number(42)
                .pr_url("https://github.com/example/repo/pull/42")
                .target_sha("head-sha")
                .build(),
            &[ReviewBatchMemberCreateInput::builder()
                .attempt(1)
                .provider_effort("medium")
                .requested_driver("claude")
                .resolved_model("test-model")
                .role(ReviewBatchMemberRole::Supervisor)
                .status(ReviewBatchMemberStatus::Pending)
                .maybe_execution_id(Some(execution.id.clone()))
                .build()],
        )
        .unwrap();
    server_state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'supervising' WHERE id = ?1",
            rusqlite::params![batch.id],
        )
        .unwrap();
    server_state
        .work_db
        .start_execution_run(
            &execution.id,
            "supervisor",
            "repo",
            "lease-supervisor",
            "workspace-supervisor",
            dir.path().to_str().unwrap(),
        )
        .unwrap();
    server_state
        .worker_registry
        .register(std::process::id() as libc::pid_t, execution.id.clone());
    (server_state, dir, work_item_id, batch.id, execution.id)
}

// ── Response accessors ───────────────────────────────────────────────────────

/// The `(proposal, already_submitted)` pair from a successful submission, or
/// a panic naming what came back instead — so a refusal reads as its own
/// error text rather than a bare pattern-match failure.
fn submitted(event: FrontendEvent) -> (WorkerProposal, bool) {
    match event {
        FrontendEvent::ProposalSubmitted {
            proposal,
            already_submitted,
        } => (proposal, already_submitted),
        FrontendEvent::ProposalRejected { error } => {
            panic!("expected a successful submission, got rejection: {error}")
        }
        other => panic!("expected ProposalSubmitted, got {other:?}"),
    }
}

fn rejected(event: FrontendEvent) -> ProposalSubmissionError {
    match event {
        FrontendEvent::ProposalRejected { error } => error,
        FrontendEvent::ProposalSubmitted { proposal, .. } => {
            panic!("expected a rejection, got a stored proposal {}", proposal.id)
        }
        other => panic!("expected ProposalRejected, got {other:?}"),
    }
}

/// An accepted review report is terminal on its own. This is the full app
/// seam, including PID attribution and RPC response: it must not depend on a
/// later driver Stop/turn-completed event to release the reviewer pane.
#[tokio::test]
async fn accepted_review_report_immediately_terminalizes_its_live_leaf() {
    let (server_state, _dir, _work_item_id, batch_id, execution_id) = live_batch_leaf();
    let peer_pid = std::process::id() as libc::pid_t;

    let (proposal, already_submitted) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(
                &execution_id,
                ProposalKind::ReviewReport,
                review_report_payload(&batch_id, "head-sha"),
            ),
        )
        .await,
    );
    assert!(!already_submitted);
    assert_eq!(proposal.state, ProposalState::Applied);
    assert!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .is_terminal(),
        "report acceptance must complete the leaf before the RPC returns"
    );
    let member = server_state
        .work_db
        .review_batch_member_for_execution(&execution_id)
        .unwrap()
        .expect("leaf must remain addressable through its persisted member row");
    assert_eq!(member.status, boss_protocol::ReviewBatchMemberStatus::Reported);
}

/// A rejected report does not masquerade as completion. Once the worker's
/// turn ends, the existing batch finalizer records the missing accepted report
/// as a member failure and releases the pane, making the error observable and
/// retryable instead of leaving a zombie leaf.
#[tokio::test]
async fn rejected_review_report_becomes_a_visible_member_failure_on_stop() {
    let (server_state, _dir, _work_item_id, batch_id, execution_id) = live_batch_leaf();
    let peer_pid = std::process::id() as libc::pid_t;

    let (proposal, already_submitted) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(
                &execution_id,
                ProposalKind::ReviewReport,
                review_report_payload(&batch_id, "wrong-head"),
            ),
        )
        .await,
    );
    assert!(!already_submitted);
    assert_eq!(proposal.state, ProposalState::Rejected);
    let outcome = server_state.completion_handler.on_stop(&execution_id).await;
    assert!(matches!(
        outcome,
        crate::completion::StopOutcome::ReviewPassCompleted { .. }
    ));
    assert!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .is_terminal(),
        "a rejected report must end as an explicit member failure, not a live pane"
    );
    assert_eq!(
        server_state
            .work_db
            .review_batch_member_for_execution(&execution_id)
            .unwrap()
            .unwrap()
            .status,
        boss_protocol::ReviewBatchMemberStatus::Failed,
    );
}

/// The consolidating supervisor must be torn down the same way a leaf is. A
/// `review_verdict` acceptance stages the member `reported` and moves the
/// batch to `applying` synchronously, exactly like a `review_report`
/// acceptance does for a leaf — so it must reach the same finalize/teardown
/// path, not just the leaf's own `ReviewReport` branch.
#[tokio::test]
async fn accepted_review_verdict_immediately_terminalizes_its_live_supervisor() {
    let (server_state, _dir, _work_item_id, batch_id, execution_id) = live_batch_supervisor();
    let peer_pid = std::process::id() as libc::pid_t;

    let (proposal, already_submitted) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(
                &execution_id,
                ProposalKind::ReviewVerdict,
                verdict_payload(&batch_id, "head-sha"),
            ),
        )
        .await,
    );
    assert!(!already_submitted);
    // `review_verdict` applies asynchronously — the RPC response reports the
    // synchronous staging outcome (`Proposed`), not the reconciler's later
    // materialisation.
    assert_eq!(proposal.state, ProposalState::Proposed);
    assert!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .is_terminal(),
        "verdict acceptance must complete the consolidator before the RPC returns, exactly as \
         report acceptance does for a leaf"
    );
    let member = server_state
        .work_db
        .review_batch_member_for_execution(&execution_id)
        .unwrap()
        .expect("supervisor must remain addressable through its persisted member row");
    assert_eq!(member.status, boss_protocol::ReviewBatchMemberStatus::Reported);
}

/// A replayed acceptance (the same idempotency key resubmitted, e.g. by a
/// worker retrying after a crash between apply and this handler's own
/// teardown call) must still reach the finalizer. The finalizer itself is
/// idempotent — it no-ops on an execution that is already terminal — so the
/// only way a replay can leave a pane stuck open is if the caller never
/// calls it a second time.
#[tokio::test]
async fn a_replayed_review_report_acceptance_still_reaches_the_finalizer() {
    let (server_state, _dir, _work_item_id, batch_id, execution_id) = live_batch_leaf();
    let peer_pid = std::process::id() as libc::pid_t;
    let payload = review_report_payload(&batch_id, "head-sha");

    let (first, already_submitted) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request_keyed(&execution_id, ProposalKind::ReviewReport, payload.clone(), "replay-key"),
        )
        .await,
    );
    assert!(!already_submitted);
    assert_eq!(first.state, ProposalState::Applied);
    assert!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .is_terminal(),
        "the first acceptance must already have torn the leaf down"
    );

    // Replay: same idempotency key, same execution — the shape a crashed
    // worker's retry takes. This must not error, and the (already-terminal)
    // finalizer call it triggers must be a harmless no-op rather than a
    // panic or a spurious error log path.
    let (replay, replay_already_submitted) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request_keyed(&execution_id, ProposalKind::ReviewReport, payload, "replay-key"),
        )
        .await,
    );
    assert!(replay_already_submitted);
    assert_eq!(replay.id, first.id);
    assert_eq!(replay.state, ProposalState::Applied);
    assert!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .is_terminal()
    );
}

/// A leaf with a real durable `shell_pid` must be genuinely reaped by the
/// pane releaser on report acceptance. `live_batch_leaf`'s execution has no durable
/// `work_runs.shell_pid`, so `release_worker_pane` finds nothing to signal
/// and the reap is a no-op. Give the leaf a real OS process as its
/// durable pid (the same fixture `worker_process_reaping.rs` uses for its
/// own "genuinely reaps a real process" coverage), submit an accepted
/// report through the real RPC handler, and confirm the process actually
/// dies — proving this is a genuine kill, not a simulated one.
///
/// The handler waits for the session writer to flush the acknowledgement
/// before it calls the finalizer, so the reap cannot kill a client still
/// blocked waiting for this response.
#[tokio::test]
async fn accepted_review_report_reaps_a_real_worker_process() {
    let (server_state, _dir, _work_item_id, batch_id, execution_id) = live_batch_leaf();
    let peer_pid = std::process::id() as libc::pid_t;

    let mut child = crate::test_support::spawn_group_leader_sleeper();
    let pid = child.id() as i64;
    super::tmux_stub::install_teardown(&server_state, &execution_id, pid);
    assert!(
        server_state
            .work_db
            .set_run_shell_pid_for_execution(&execution_id, pid)
            .unwrap(),
        "live_batch_leaf's start_execution_run must have left a run row to record the pid against"
    );

    let (proposal, _) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(
                &execution_id,
                ProposalKind::ReviewReport,
                review_report_payload(&batch_id, "head-sha"),
            ),
        )
        .await,
    );
    assert_eq!(proposal.state, ProposalState::Applied);

    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait task")
        .expect("wait on child");
    assert!(
        !status.success(),
        "the live pane releaser must have actually reaped the leaf's real worker process — a \
         no-op releaser (or one that only ever ran against a fake) would leave it running"
    );
}

/// Build one live (`running`) `chore_implementation` execution against a
/// fake app runtime — the shape a `run_done` declaration is actually
/// submitted against (unlike a review-batch leaf/supervisor, most kinds
/// declaring `run_done` are plain implementation workers with no batch
/// machinery at all).
fn live_chore_execution() -> (Arc<ServerState>, tempfile::TempDir, String, String) {
    let (server_state, dir) = test_server_state_with_fakes();
    // Every test using this fixture exercises the `run_done` synchronous
    // finalize path, which `handle_submit_proposal` only takes with both
    // halves of the seam's kill switch on — see
    // `finalize_run_done_declaration`'s gate in `app::proposals`.
    server_state.feature_flags.set("worker_proposals", true).unwrap();
    server_state.feature_flags.set("run_done_proposals_seam", true).unwrap();
    let (execution_id, work_item_id) = new_execution(&server_state, "Run-done target");
    server_state
        .work_db
        .start_execution_run(
            &execution_id,
            "worker",
            "repo",
            "lease-run-done",
            "workspace-run-done",
            dir.path().to_str().unwrap(),
        )
        .unwrap();
    server_state
        .worker_registry
        .register(std::process::id() as libc::pid_t, execution_id.clone());
    (server_state, dir, execution_id, work_item_id)
}

/// Bind a PR to `work_item_id` directly — the same shortcut
/// [`live_batch_supervisor`] takes for its batch's status column, since
/// standing up a real PR-open flow first is not the point of these tests.
fn set_task_pr_url(server_state: &Arc<ServerState>, work_item_id: &str, pr_url: &str) {
    server_state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
            rusqlite::params![work_item_id, pr_url],
        )
        .unwrap();
}

fn task_status(server_state: &Arc<ServerState>, work_item_id: &str) -> boss_protocol::TaskStatus {
    match server_state.work_db.get_work_item(work_item_id).unwrap() {
        crate::work::WorkItem::Task(task) | crate::work::WorkItem::Chore(task) => task.status,
        other => panic!("expected a task/chore, got {other:?}"),
    }
}

/// This is the fix's central claim, exercised end to end through the real
/// RPC handler: an accepted `delivered` declaration with a resolvable PR
/// terminalizes its execution and takes the `PendingReview` hold (active +
/// bound PR) before the RPC even returns — no later Stop boundary required.
/// The hold itself is released asynchronously once the reviewer-admission
/// decision is made; see `completion::tests::t14` for direct coverage of
/// that release. Mirrors `accepted_review_report_immediately_terminalizes_its_live_leaf`
/// for the whole-execution (not batch-member) finalize path.
#[tokio::test]
async fn accepted_run_done_delivered_immediately_terminalizes_a_bound_pr() {
    let (server_state, _dir, execution_id, work_item_id) = live_chore_execution();
    set_task_pr_url(&server_state, &work_item_id, "https://github.com/example/repo/pull/9");
    let peer_pid = std::process::id() as libc::pid_t;

    let (proposal, already_submitted) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(
                &execution_id,
                ProposalKind::RunDone,
                json!({"outcome": "delivered", "summary": "Shipped it"}),
            ),
        )
        .await,
    );
    assert!(!already_submitted);
    assert_eq!(proposal.state, ProposalState::Applied);
    assert!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .is_terminal(),
        "a `delivered` declaration with a resolvable PR must terminalize before the RPC returns"
    );
    assert_eq!(
        task_status(&server_state, &work_item_id),
        // PendingReview is represented by active plus the bound PR while
        // the asynchronous reviewer-admission path is still deciding.
        boss_protocol::TaskStatus::Active
    );
}

/// A worker can declare `delivered` without the engine being able to resolve
/// any PR (no bound `pr_url`, nothing staged, no artifact) — the declaration
/// is still definitive: the run ends and its resources are released. The
/// task records a visible failure and a flagged attention records the mismatch.
#[tokio::test]
async fn accepted_run_done_delivered_without_a_resolvable_pr_still_terminalizes() {
    let (server_state, _dir, execution_id, work_item_id) = live_chore_execution();
    let peer_pid = std::process::id() as libc::pid_t;

    let (proposal, _) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(
                &execution_id,
                ProposalKind::RunDone,
                json!({"outcome": "delivered", "summary": "Shipped it, I think"}),
            ),
        )
        .await,
    );
    assert_eq!(proposal.state, ProposalState::Applied);
    let execution = server_state.work_db.get_execution(&execution_id).unwrap();
    assert_eq!(
        execution.status,
        boss_protocol::ExecutionStatus::Failed,
        "still terminalizes — a declaration is definitive even when the engine cannot verify it"
    );
    assert_eq!(
        task_status(&server_state, &work_item_id),
        boss_protocol::TaskStatus::Blocked,
        "an unverifiable delivery is a visible failure"
    );
    let items = server_state.work_db.list_attention_items(&execution_id).unwrap();
    assert!(
        items
            .iter()
            .any(|i| i.kind == crate::completion::RUN_DONE_AUDIT_FLAGGED_ATTENTION_KIND),
        "the unresolved PR must be flagged for a human: {items:?}"
    );
}

/// `no-changes-needed` closes the task as `done` without a PR, synchronously
/// at submit — the same terminal `record_worker_no_op_completion` already
/// reaches from a Stop boundary, now reached from the declaration itself.
#[tokio::test]
async fn accepted_run_done_no_changes_needed_immediately_closes_the_task() {
    let (server_state, _dir, execution_id, work_item_id) = live_chore_execution();
    let peer_pid = std::process::id() as libc::pid_t;

    let (proposal, _) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(
                &execution_id,
                ProposalKind::RunDone,
                json!({"outcome": "no_changes_needed", "summary": "Already fixed on main"}),
            ),
        )
        .await,
    );
    assert_eq!(proposal.state, ProposalState::Applied);
    assert_eq!(
        server_state.work_db.get_execution(&execution_id).unwrap().status,
        boss_protocol::ExecutionStatus::Completed
    );
    assert_eq!(
        task_status(&server_state, &work_item_id),
        boss_protocol::TaskStatus::Done
    );
}

/// A mandated stop fails visibly, without bypassing a check or cycling workers.
#[tokio::test]
async fn mandated_check_approval_stop_fails_visibly_without_replacement_workers() {
    let (server_state, _dir, execution_id, work_item_id) = live_chore_execution();
    let peer_pid = std::process::id() as libc::pid_t;
    let reason = "Repository check requires an exclusion to proceed; AGENTS.md forbids relaxing it without approval";
    submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(&execution_id, ProposalKind::Blocked, json!({"reason": reason})),
        )
        .await,
    );
    let (proposal, _) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(
                &execution_id,
                ProposalKind::RunDone,
                json!({"outcome": "blocked", "summary": "Check policy decision required; no bypass applied"}),
            ),
        )
        .await,
    );
    assert_eq!(proposal.state, ProposalState::Applied);
    let db = &server_state.work_db;
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, boss_protocol::ExecutionStatus::Failed);
    assert!(execution.cube_lease_id.is_none());
    assert!(execution.workspace_path.is_none());
    let boss_protocol::WorkItem::Chore(task) = db.get_work_item(&work_item_id).unwrap() else {
        panic!("expected chore");
    };
    assert_eq!(task.status, boss_protocol::TaskStatus::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some("worker_failed"));
    let detail = task.blocked_detail.as_deref().unwrap();
    assert!(detail.contains(reason));
    assert!(detail.contains("no bypass applied"));
    assert!(detail.contains(&execution_id));
    assert!(!db.dispatch_admission_facts(&work_item_id).unwrap().deliberate_parked);
    // Repeated automatic passes must not consume churn strikes or mint anything.
    for _ in 0..4 {
        assert!(
            !db.list_orphan_active_candidates(0)
                .unwrap()
                .iter()
                .any(|candidate| candidate == &work_item_id)
        );
        assert!(!db.rescan_active_dispatch().unwrap().contains(&work_item_id));
        db.reconcile_product_executions(&task.product_id).unwrap();
        assert_eq!(
            db.latest_execution_for_work_item(&work_item_id).unwrap().unwrap().id,
            execution_id
        );
    }
    // This is the task-show/board wire representation, not execution-only attention.
    let response = call_with_peer(
        &server_state,
        None,
        FrontendRequest::GetWorkItem {
            id: work_item_id.clone(),
        },
    )
    .await;
    let FrontendEvent::WorkItemResult {
        item: boss_protocol::WorkItem::Chore(shown),
    } = response
    else {
        panic!("task show must return the failed chore");
    };
    let shown = serde_json::to_value(shown).unwrap();
    assert!(shown["blocked_detail"].as_str().unwrap().contains(reason));
}

/// The end-to-end proof the incidents demand: submitting `run_done` reaps a
/// REAL worker process — not a fake, not a later Stop boundary — and does so
/// without ever touching the network. This test's own `branch_verifier` and
/// `merge_probe` are the real production collaborators (`test_server_state_with_fakes`
/// only fakes `cube`/pane-spawn); if the synchronous finalize path made a
/// live `gh` call, this test would hang or fail in a sandboxed CI run with
/// no GitHub credentials. It doesn't, because it never calls either
/// collaborator — see `completion::run_done_declaration`'s module doc.
/// Mirrors `accepted_review_report_reaps_a_real_worker_process`.
#[tokio::test]
async fn accepted_run_done_delivered_reaps_a_real_worker_process_without_network() {
    let (server_state, _dir, execution_id, work_item_id) = live_chore_execution();
    set_task_pr_url(&server_state, &work_item_id, "https://github.com/example/repo/pull/9");
    let peer_pid = std::process::id() as libc::pid_t;

    let mut child = crate::test_support::spawn_group_leader_sleeper();
    let pid = child.id() as i64;
    super::tmux_stub::install_teardown(&server_state, &execution_id, pid);
    assert!(
        server_state
            .work_db
            .set_run_shell_pid_for_execution(&execution_id, pid)
            .unwrap(),
        "live_chore_execution's start_execution_run must have left a run row to record the pid against"
    );

    let (proposal, _) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(
                &execution_id,
                ProposalKind::RunDone,
                json!({"outcome": "delivered", "summary": "Shipped it"}),
            ),
        )
        .await,
    );
    assert_eq!(proposal.state, ProposalState::Applied);

    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("join wait task")
        .expect("wait on child");
    assert!(
        !status.success(),
        "the run_done finalize must have actually reaped the worker's real process — a no-op \
         releaser (or one that only ever ran against a fake) would leave it running"
    );
}

/// With the `run_done_proposals_seam` kill switch off, an accepted
/// `run_done` proposal must still apply (the durable stamp is
/// unconditional — see `apply_run_done`'s doc comment) but must NOT tear
/// the worker down or advance the task synchronously: that decision
/// belongs to the legacy health-alone read inside the Stop-boundary
/// satisfied-deliverable gate, not to this RPC path. Without this gate the
/// flag is not actually a kill switch, contrary to its documented purpose
/// and to every other `worker_proposals`-gated read site.
#[tokio::test]
async fn accepted_run_done_delivered_does_not_finalize_with_the_seam_off() {
    let (server_state, _dir, execution_id, work_item_id) = live_chore_execution();
    server_state
        .feature_flags
        .set("run_done_proposals_seam", false)
        .unwrap();
    set_task_pr_url(&server_state, &work_item_id, "https://github.com/example/repo/pull/9");
    let peer_pid = std::process::id() as libc::pid_t;

    let (proposal, _) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(
                &execution_id,
                ProposalKind::RunDone,
                json!({"outcome": "delivered", "summary": "Shipped it"}),
            ),
        )
        .await,
    );
    assert_eq!(
        proposal.state,
        ProposalState::Applied,
        "the stamp itself is unconditional regardless of the seam flag"
    );
    assert!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .is_live(),
        "with the seam off, the RPC path must not terminalize the execution — that decision is \
         still the legacy Stop-boundary gate's to make"
    );
    assert_eq!(
        server_state.work_db.execution_run_done_outcome(&execution_id).unwrap(),
        Some(boss_protocol::RunDoneOutcome::Delivered),
        "the declaration must still be readable by the legacy gate"
    );
}

fn listed(event: FrontendEvent) -> (String, Vec<WorkerProposal>) {
    match event {
        FrontendEvent::ProposalsList {
            work_item_id,
            proposals,
        } => (work_item_id, proposals),
        FrontendEvent::ProposalRejected { error } => panic!("expected a listing, got rejection: {error}"),
        other => panic!("expected ProposalsList, got {other:?}"),
    }
}

// ── Happy path ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn submission_persists_a_proposed_row_attributed_to_the_caller() {
    let fx = WorkerFixture::new();

    let (proposal, already) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "stuck"})).await);

    assert!(!already);
    assert!(proposal.id.starts_with("prp_"), "{}", proposal.id);
    assert_eq!(proposal.execution_id, fx.execution_id);
    assert_eq!(proposal.work_item_id.as_deref(), Some(fx.work_item_id.as_str()));
    // `blocked` auto-applies (crate::work::proposal_apply::apply_policy) —
    // the apply pipeline runs inside the same submission transaction.
    assert_eq!(proposal.state, ProposalState::Applied);
    assert!(proposal.applied_ref.as_deref().is_some_and(|r| r.starts_with("attn_")));
    assert_eq!(proposal.decided_by, Some(boss_protocol::ProposalDecider::Policy));
    assert!(proposal.decided_at.is_some());
}

/// The macOS app's deferred-scope badge and Notifications window only
/// re-fetch on `AttentionItemCreated` (`ChatViewModel+DeferredScope.swift`) —
/// same as the legacy marker-detector paths in `completion.rs`. A proposal
/// that auto-applies to a fresh (not replayed) attention item must publish
/// that event on the work item's product topic, exactly like the legacy
/// paths do, or the UI never live-updates.
#[tokio::test]
async fn a_fresh_auto_applied_proposal_publishes_attention_item_created() {
    let fx = WorkerFixture::new();
    let product_id = fx
        .server_state
        .work_db
        .get_work_item(&fx.work_item_id)
        .unwrap()
        .product_id()
        .to_owned();

    // Register and subscribe a session on the product topic so the topic
    // broker actually has somewhere to deliver the push — the same
    // subscribe-then-observe flow a real macOS session goes through via
    // `handle_subscribe`.
    let push_session = "push-listener";
    let push_sink = make_session_sink();
    fx.server_state
        .topic_broker
        .register_session(push_session, push_sink.clone())
        .await;
    fx.server_state
        .topic_broker
        .subscribe(push_session, &[boss_protocol::work_product_topic(&product_id)])
        .await;

    let (proposal, already) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "stuck"})).await);
    assert!(!already);
    assert_eq!(proposal.state, ProposalState::Applied);
    let applied_ref = proposal.applied_ref.clone().unwrap();

    push_sink.close();
    let pushed = push_sink
        .next()
        .await
        .expect("an AttentionItemCreated push must have been queued for the subscribed session")
        .payload;
    match pushed {
        FrontendEvent::AttentionItemCreated { item } => assert_eq!(item.id, applied_ref),
        other => panic!("expected AttentionItemCreated, got {other:?}"),
    }
}

/// A replayed (`already_submitted`) proposal did not create a new row this
/// call, so it must not publish a second `AttentionItemCreated` for the same
/// item — that would look like a brand-new attention item to the UI.
#[tokio::test]
async fn a_replayed_proposal_does_not_republish_attention_item_created() {
    let fx = WorkerFixture::new();
    let product_id = fx
        .server_state
        .work_db
        .get_work_item(&fx.work_item_id)
        .unwrap()
        .product_id()
        .to_owned();

    let (first, _) = submitted(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request_keyed(
                &fx.execution_id,
                ProposalKind::Blocked,
                json!({"reason": "stuck"}),
                "key-1",
            ),
        )
        .await,
    );
    assert_eq!(first.state, ProposalState::Applied);

    let push_session = "push-listener";
    let push_sink = make_session_sink();
    fx.server_state
        .topic_broker
        .register_session(push_session, push_sink.clone())
        .await;
    fx.server_state
        .topic_broker
        .subscribe(push_session, &[boss_protocol::work_product_topic(&product_id)])
        .await;

    let (replay, already) = submitted(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request_keyed(
                &fx.execution_id,
                ProposalKind::Blocked,
                json!({"reason": "stuck"}),
                "key-1",
            ),
        )
        .await,
    );
    assert!(already);
    assert_eq!(replay.id, first.id);

    push_sink.close();
    assert!(
        push_sink.next().await.is_none(),
        "a replayed submission must not publish another AttentionItemCreated",
    );
}

/// `followup_task` is Gated — it awaits the human batch-accept gesture, so a
/// fresh submission must land untouched in `proposed`.
#[tokio::test]
async fn submission_of_a_gated_kind_stays_proposed() {
    let fx = WorkerFixture::new();

    let (proposal, _) = submitted(
        submit(
            &fx,
            ProposalKind::FollowupTask,
            json!({"proposed_name": "N", "proposed_description": "D", "rationale": "R"}),
        )
        .await,
    );

    assert_eq!(proposal.state, ProposalState::Proposed);
    assert_eq!(proposal.applied_ref, None);
    assert_eq!(proposal.decided_by, None);
    assert_eq!(proposal.decided_at, None);
}

/// A fresh (not replayed) `followup_task` submission stages a member into
/// the originating task's `followup` attention group at submission time,
/// regardless of the kind's `Gated` apply policy — so the card must be live
/// in the Notifications window from that moment, not only after some
/// unrelated refresh. Assert the same `AttentionCreated` event every other
/// attention-creating path publishes (`app/attentions.rs`) goes out on the
/// work item's product topic.
#[tokio::test]
async fn a_fresh_followup_task_submission_publishes_attention_created() {
    let fx = WorkerFixture::new();
    let product_id = fx
        .server_state
        .work_db
        .get_work_item(&fx.work_item_id)
        .unwrap()
        .product_id()
        .to_owned();

    let push_session = "push-listener";
    let push_sink = make_session_sink();
    fx.server_state
        .topic_broker
        .register_session(push_session, push_sink.clone())
        .await;
    fx.server_state
        .topic_broker
        .subscribe(push_session, &[boss_protocol::work_product_topic(&product_id)])
        .await;

    let (proposal, already) = submitted(
        submit(
            &fx,
            ProposalKind::FollowupTask,
            json!({"proposed_name": "N", "proposed_description": "D", "rationale": "R"}),
        )
        .await,
    );
    assert!(!already);
    assert_eq!(proposal.state, ProposalState::Proposed);

    push_sink.close();
    let pushed = push_sink
        .next()
        .await
        .expect("an AttentionCreated push must have been queued for the subscribed session")
        .payload;
    match pushed {
        FrontendEvent::AttentionCreated { attention, group } => {
            assert_eq!(attention.source_proposal_id.as_deref(), Some(proposal.id.as_str()));
            assert_eq!(group.product_id, product_id);
        }
        other => panic!("expected AttentionCreated, got {other:?}"),
    }
}

/// The stored payload is the validation layer's canonical form, not the raw
/// bytes the caller sent — so the apply pipeline can deserialise it directly.
#[tokio::test]
async fn stored_payload_is_canonicalised() {
    let fx = WorkerFixture::new();
    let (proposal, _) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "  padded  "})).await);
    assert_eq!(proposal.payload_json, r#"{"reason":"padded"}"#);
}

/// Every v1 kind must be submittable — a kind the engine accepts on the wire
/// but cannot store would be a dead verb the CLI still advertises.
#[tokio::test]
async fn every_kind_can_be_submitted() {
    let fx = WorkerFixture::new();
    for &kind in ProposalKind::ALL {
        let payload = match kind {
            ProposalKind::Attention => json!({"title": "T", "body_markdown": "B"}),
            ProposalKind::EffortEscalation => json!({"requested_level": "large", "reason": "R"}),
            ProposalKind::Blocked => json!({"reason": "R"}),
            ProposalKind::DeferredScope => json!({"summary": "S", "reason": "R"}),
            ProposalKind::FollowupTask => {
                json!({"proposed_name": "N", "proposed_description": "D", "rationale": "R"})
            }
            ProposalKind::AutomationOutcome => json!({"outcome": "skip", "reason": "clean"}),
            ProposalKind::PrCreated => json!({"pr_url": "https://github.com/o/r/pull/1"}),
            ProposalKind::ReviewGuide => json!({"body_markdown": "# Guide"}),
            ProposalKind::ReviewReport => {
                json!({
                    "batch_id": "rvb_missing",
                    "target_sha": "head_missing",
                    "report": {
                        "batch_id": "rvb_missing",
                        "pr_url": "https://github.com/o/r/pull/1",
                        "target_sha": "head_missing",
                        "phase": "pre_merge",
                        "summary": "Clean.",
                        "coverage": {"files_inspected": [], "files_omitted": [], "limitations": []},
                        "findings": [],
                    },
                })
            }
            ProposalKind::ReviewVerdict => json!({
                "batch_id": "rvb_missing",
                "verdict": {
                    "batch_id": "rvb_missing",
                    "pr_url": "https://github.com/o/r/pull/1",
                    "target_sha": "head_missing",
                    "phase": "pre_merge",
                    "summary": "Clean.",
                    "revision_warranted": false,
                    "findings": [],
                    "contradictions": [],
                },
            }),
            ProposalKind::RunDone => json!({"outcome": "delivered", "summary": "S"}),
        };
        let (proposal, _) = submitted(submit(&fx, kind, payload).await);
        assert_eq!(proposal.kind, kind);
    }
}

// ── Validation ───────────────────────────────────────────────────────────────

/// The property the whole design turns on: a malformed submission comes back
/// as a typed, field-scoped error *during the run*, so the worker can fix it
/// and retry — rather than failing a transcript scrape at Stop.
#[tokio::test]
async fn invalid_payload_is_refused_with_field_level_detail() {
    let fx = WorkerFixture::new();

    let error = rejected(submit(&fx, ProposalKind::Blocked, json!({"resaon": "typo"})).await);

    assert_eq!(error.code, ProposalErrorCode::ValidationFailed);
    let fields: Vec<&str> = error.field_errors.iter().map(|e| e.field.as_str()).collect();
    assert!(fields.contains(&"reason"), "{fields:?}");
    assert!(fields.contains(&"resaon"), "{fields:?}");
}

/// A refused submission must leave no trace: the worker retries the fixed
/// command, and a half-written row would make that retry look like a
/// duplicate.
#[tokio::test]
async fn a_refused_submission_stores_nothing() {
    let fx = WorkerFixture::new();
    rejected(submit(&fx, ProposalKind::Blocked, json!({"reason": ""})).await);

    let (_, proposals) = listed(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            FrontendRequest::ListProposals {
                run_id: fx.execution_id.clone(),
                kind: None,
                state: None,
            },
        )
        .await,
    );
    assert!(proposals.is_empty(), "a rejected submission must not persist a row");
}

/// Fix-and-retry is the documented remediation, so it has to actually work
/// within the same run.
#[tokio::test]
async fn a_worker_can_fix_and_retry_after_a_validation_error() {
    let fx = WorkerFixture::new();
    rejected(
        submit(
            &fx,
            ProposalKind::EffortEscalation,
            json!({"requested_level": "enormous", "reason": "R"}),
        )
        .await,
    );

    let (proposal, _) = submitted(
        submit(
            &fx,
            ProposalKind::EffortEscalation,
            json!({"requested_level": "large", "reason": "R"}),
        )
        .await,
    );
    assert_eq!(proposal.kind, ProposalKind::EffortEscalation);
}

// ── Idempotency ──────────────────────────────────────────────────────────────

/// Resubmitting identical content returns the existing row with
/// `already_submitted`, rather than erroring or duplicating.
#[tokio::test]
async fn identical_resubmission_is_idempotent() {
    let fx = WorkerFixture::new();
    let (first, first_already) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "stuck"})).await);
    let (replay, replay_already) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "stuck"})).await);

    assert!(!first_already);
    assert!(replay_already, "a replay must report already_submitted");
    assert_eq!(replay.id, first.id);
}

/// The derived key is content-addressed, so a *different* proposal of the
/// same kind is a new row — idempotency must not collapse distinct
/// submissions into one.
#[tokio::test]
async fn different_content_is_a_new_proposal() {
    let fx = WorkerFixture::new();
    let (first, _) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "stuck"})).await);
    let (second, already) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "differently stuck"})).await);

    assert!(!already);
    assert_ne!(first.id, second.id);
}

/// An explicit `--idempotency-key` overrides the derived one: two different
/// payloads under the same key collapse onto the first row. This is what
/// lets a caller declare "these are the same proposal" when the content
/// changes between retries.
#[tokio::test]
async fn an_explicit_key_overrides_content_addressing() {
    let fx = WorkerFixture::new();
    let (first, _) = submitted(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request_keyed(
                &fx.execution_id,
                ProposalKind::Blocked,
                json!({"reason": "one"}),
                "my-key",
            ),
        )
        .await,
    );
    let (replay, already) = submitted(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request_keyed(
                &fx.execution_id,
                ProposalKind::Blocked,
                json!({"reason": "two"}),
                "my-key",
            ),
        )
        .await,
    );

    assert!(already);
    assert_eq!(replay.id, first.id);
    assert_eq!(replay.payload_json, first.payload_json, "the stored row is unchanged");
    assert_eq!(first.idempotency_key, "my-key");
}

/// Reusing an explicit key across a *different* `kind` is not a replay — the
/// caller submitted an unrelated proposal under the same key by mistake, so
/// it must be refused rather than silently handed back the other kind's row
/// with `already_submitted: true`.
#[tokio::test]
async fn an_explicit_key_reused_with_a_different_kind_is_refused() {
    let fx = WorkerFixture::new();
    submitted(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request_keyed(
                &fx.execution_id,
                ProposalKind::Blocked,
                json!({"reason": "stuck"}),
                "my-key",
            ),
        )
        .await,
    );

    let error = rejected(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request_keyed(
                &fx.execution_id,
                ProposalKind::DeferredScope,
                json!({"summary": "S", "reason": "R"}),
                "my-key",
            ),
        )
        .await,
    );

    assert_eq!(error.code, ProposalErrorCode::ValidationFailed);
    assert_eq!(error.field_errors[0].field, "idempotency_key");
}

/// An unset shell variable expands to an empty string. Treating that as a
/// real key would make every keyless submission from a run collide on `""`
/// and silently return the first one forever.
#[tokio::test]
async fn a_blank_explicit_key_falls_back_to_the_derived_one() {
    let fx = WorkerFixture::new();
    let (first, _) = submitted(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request_keyed(&fx.execution_id, ProposalKind::Blocked, json!({"reason": "one"}), "   "),
        )
        .await,
    );
    let (second, already) = submitted(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request_keyed(&fx.execution_id, ProposalKind::Blocked, json!({"reason": "two"}), ""),
        )
        .await,
    );

    assert!(!already, "distinct content must not collide on a blank key");
    assert_ne!(first.id, second.id);
    assert!(first.idempotency_key.starts_with("auto:"), "{}", first.idempotency_key);
}

/// The column has no other bound, unlike every payload field, so an
/// over-length explicit key must be refused rather than stored.
#[tokio::test]
async fn an_overlong_explicit_key_is_refused() {
    let fx = WorkerFixture::new();
    let key = "k".repeat(boss_engine_proposal_validation::MAX_SHORT_FIELD_CHARS + 1);
    let error = rejected(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request_keyed(
                &fx.execution_id,
                ProposalKind::Blocked,
                json!({"reason": "stuck"}),
                &key,
            ),
        )
        .await,
    );

    assert_eq!(error.code, ProposalErrorCode::ValidationFailed);
    assert_eq!(error.field_errors[0].field, "idempotency_key");
}

/// The `auto:` prefix is reserved for keys the engine derives itself — a
/// caller cannot pre-claim one.
#[tokio::test]
async fn an_explicit_key_with_the_derived_prefix_is_refused() {
    let fx = WorkerFixture::new();
    let error = rejected(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request_keyed(
                &fx.execution_id,
                ProposalKind::Blocked,
                json!({"reason": "stuck"}),
                "auto:blocked:deadbeef",
            ),
        )
        .await,
    );

    assert_eq!(error.code, ProposalErrorCode::ValidationFailed);
    assert_eq!(error.field_errors[0].field, "idempotency_key");
}

// ── Attribution ──────────────────────────────────────────────────────────────

/// The cross-check: `BOSS_RUN_ID` can only make a call fail, never grant it
/// anything. A worker claiming another run's id is refused rather than
/// filing against that run's work item.
#[tokio::test]
async fn a_run_id_that_disagrees_with_the_peer_is_refused() {
    let fx = WorkerFixture::new();
    let (other_execution, _) = new_execution(&fx.server_state, "Someone else's chore");

    let error = rejected(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            submit_request(&other_execution, ProposalKind::Blocked, json!({"reason": "stuck"})),
        )
        .await,
    );

    assert_eq!(error.code, ProposalErrorCode::AttributionMismatch);
    assert!(error.message.contains(&other_execution), "{}", error.message);
    assert!(error.message.contains(&fx.execution_id), "{}", error.message);

    // And nothing was written against the run it tried to claim.
    let stored = fx
        .server_state
        .work_db
        .count_worker_proposals_for_execution(&other_execution, ProposalKind::Blocked)
        .unwrap();
    assert_eq!(stored.total, 0);
}

/// The mismatch check must also guard the read verb — otherwise a worker
/// could enumerate another work item's proposals by passing its run id.
#[tokio::test]
async fn listing_with_a_mismatched_run_id_is_refused() {
    let fx = WorkerFixture::new();
    let (other_execution, _) = new_execution(&fx.server_state, "Someone else's chore");

    let error = rejected(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            FrontendRequest::ListProposals {
                run_id: other_execution,
                kind: None,
                state: None,
            },
        )
        .await,
    );
    assert_eq!(error.code, ProposalErrorCode::AttributionMismatch);
}

/// v1 rejects remote workers outright: with no local peer pid there is no
/// verified identity to attribute a proposal to.
#[tokio::test]
async fn a_connection_with_no_peer_pid_is_refused() {
    let fx = WorkerFixture::new();

    let error = rejected(
        call_with_peer(
            &fx.server_state,
            None,
            submit_request(&fx.execution_id, ProposalKind::Blocked, json!({"reason": "stuck"})),
        )
        .await,
    );
    assert_eq!(error.code, ProposalErrorCode::NoLocalPeer);
}

/// Attribution fails closed: a local caller that is not a worker (the human's
/// own shell, a stray script) cannot submit, even with a real run id.
#[tokio::test]
async fn a_peer_with_no_registered_worker_ancestry_is_refused() {
    let (server_state, _dir) = test_server_state();
    let (execution_id, _) = new_execution(&server_state, "Cleanup");
    // Register some *other* pid as the only worker, so the ancestor walk
    // from our peer finds nothing.
    server_state.worker_registry.register(i32::MAX, execution_id.clone());

    let error = rejected(
        call_with_peer(
            &server_state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&execution_id, ProposalKind::Blocked, json!({"reason": "stuck"})),
        )
        .await,
    );
    assert_eq!(error.code, ProposalErrorCode::AttributionUnresolved);
}

/// A registry entry pointing at an execution the DB no longer has gets its
/// own code — the caller cannot fix it, so conflating it with a payload or
/// attribution problem would send the worker chasing the wrong thing.
#[tokio::test]
async fn a_peer_resolving_to_a_pruned_execution_is_refused() {
    let (server_state, _dir) = test_server_state();
    let peer_pid = std::process::id() as libc::pid_t;
    server_state.worker_registry.register(peer_pid, "exec_gone".to_owned());

    let error = rejected(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request("exec_gone", ProposalKind::Blocked, json!({"reason": "stuck"})),
        )
        .await,
    );
    assert_eq!(error.code, ProposalErrorCode::UnknownExecution);
}

// ── Rate caps ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_per_kind_cap_is_enforced_with_a_typed_error() {
    let fx = WorkerFixture::new();
    for i in 0..PROPOSAL_CAP_PER_KIND_PER_EXECUTION {
        submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": format!("n{i}")})).await);
    }

    let error = rejected(submit(&fx, ProposalKind::Blocked, json!({"reason": "one too many"})).await);
    assert_eq!(error.code, ProposalErrorCode::RateLimited);

    // The cap is per kind, so another kind still has its own budget.
    submitted(submit(&fx, ProposalKind::DeferredScope, json!({"summary": "S", "reason": "R"})).await);
}

/// A replay at an exhausted cap must still succeed — otherwise a worker that
/// spends its budget and then retries an earlier command (a dropped reply, a
/// resumed run re-running its script) is rate-limited for work already done.
#[tokio::test]
async fn a_replay_at_the_cap_still_succeeds() {
    let fx = WorkerFixture::new();
    let mut first_id = None;
    for i in 0..PROPOSAL_CAP_PER_KIND_PER_EXECUTION {
        let (proposal, _) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": format!("n{i}")})).await);
        first_id.get_or_insert(proposal.id);
    }
    rejected(submit(&fx, ProposalKind::Blocked, json!({"reason": "new content"})).await);

    let (replay, already) = submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "n0"})).await);
    assert!(already);
    assert_eq!(Some(replay.id), first_id);
}

// ── Listing ──────────────────────────────────────────────────────────────────

/// The read a successor run depends on: proposals from *every* execution of
/// the work item, with prior dispositions attached, so a resumed run sees
/// sees the predecessor's rejection reason and adjusts instead of re-proposing.
#[tokio::test]
async fn listing_spans_prior_executions_and_carries_dispositions() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.clone();
    let product = crate::test_support::create_test_product(&db);
    let chore = crate::test_support::create_test_chore(&db, product.id, "Cleanup");
    let first = crate::test_support::create_ready_chore_execution(&db, chore.id.clone());
    let second = crate::test_support::create_ready_chore_execution(&db, chore.id.clone());
    let peer_pid = std::process::id() as libc::pid_t;

    // The predecessor run files a followup, which is later rejected.
    server_state.worker_registry.register(peer_pid, first.id.clone());
    let (old, _) = submitted(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            submit_request(
                &first.id,
                ProposalKind::FollowupTask,
                json!({"proposed_name": "N", "proposed_description": "D", "rationale": "R"}),
            ),
        )
        .await,
    );
    db.connect()
        .unwrap()
        .execute(
            "UPDATE worker_proposals SET state = 'rejected', decided_by = 'human',
             decision_reason = 'duplicate of an existing task', decided_at = '1747000000' WHERE id = ?1",
            rusqlite::params![old.id],
        )
        .unwrap();

    // The successor run takes over the same work item and lists.
    server_state.worker_registry.register(peer_pid, second.id.clone());
    let (work_item_id, proposals) = listed(
        call_with_peer(
            &server_state,
            Some(peer_pid),
            FrontendRequest::ListProposals {
                run_id: second.id.clone(),
                kind: None,
                state: None,
            },
        )
        .await,
    );

    assert_eq!(work_item_id, chore.id, "scope is the work item, not the execution");
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].execution_id, first.id, "the predecessor's row is visible");
    assert_eq!(proposals[0].state, ProposalState::Rejected);
    assert_eq!(
        proposals[0].decision_reason.as_deref(),
        Some("duplicate of an existing task")
    );
}

/// Another work item's proposals must never appear: the scope is derived
/// from the caller's attributed execution, so there is no field to widen it.
#[tokio::test]
async fn listing_never_leaks_another_work_items_proposals() {
    let fx = WorkerFixture::new();
    let (other_execution, other_item) = new_execution(&fx.server_state, "Someone else's chore");
    fx.server_state
        .work_db
        .submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
            execution_id: &other_execution,
            work_item_id: &other_item,
            kind: ProposalKind::Blocked,
            payload_json: r#"{"reason":"theirs"}"#,
            idempotency_key: "theirs",
        })
        .unwrap()
        .unwrap();
    submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "mine"})).await);

    let (work_item_id, proposals) = listed(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            FrontendRequest::ListProposals {
                run_id: fx.execution_id.clone(),
                kind: None,
                state: None,
            },
        )
        .await,
    );

    assert_eq!(work_item_id, fx.work_item_id);
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].payload_json, r#"{"reason":"mine"}"#);
}

#[tokio::test]
async fn listing_honours_the_kind_and_state_filters() {
    let fx = WorkerFixture::new();
    // AutoApply — lands `applied`.
    submitted(submit(&fx, ProposalKind::Blocked, json!({"reason": "stuck"})).await);
    submitted(submit(&fx, ProposalKind::DeferredScope, json!({"summary": "S", "reason": "R"})).await);
    // Gated — stays `proposed`.
    submitted(
        submit(
            &fx,
            ProposalKind::FollowupTask,
            json!({"proposed_name": "N", "proposed_description": "D", "rationale": "R"}),
        )
        .await,
    );

    let list = |kind, state| {
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            FrontendRequest::ListProposals {
                run_id: fx.execution_id.clone(),
                kind,
                state,
            },
        )
    };

    let (_, blocked) = listed(list(Some(ProposalKind::Blocked), None).await);
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0].kind, ProposalKind::Blocked);

    let (_, applied) = listed(list(None, Some(ProposalState::Applied)).await);
    assert_eq!(applied.len(), 2, "blocked + deferred_scope auto-applied");

    let (_, proposed) = listed(list(None, Some(ProposalState::Proposed)).await);
    assert_eq!(proposed.len(), 1, "followup_task is gated");
}

#[tokio::test]
async fn listing_an_execution_with_no_proposals_is_empty_not_an_error() {
    let fx = WorkerFixture::new();
    let (_, proposals) = listed(
        call_with_peer(
            &fx.server_state,
            Some(fx.peer_pid),
            FrontendRequest::ListProposals {
                run_id: fx.execution_id.clone(),
                kind: None,
                state: None,
            },
        )
        .await,
    );
    assert!(proposals.is_empty());
}

#[tokio::test]
async fn review_guide_submission_publishes_for_its_attributed_codex_execution() {
    let (state, _dir) = test_server_state();
    let db = &state.work_db;
    let product = crate::test_support::create_product(db);
    let root = crate::test_support::create_active_chore(db, &product, "guide producer");
    let (series, comparison) = crate::test_support::seed_review_guide_series(db, &root);
    let attempt = db
        .create_pr_review_guide_attempt(&series, &comparison, boss_review_guide::PROMPT_VERSION)
        .unwrap();
    let execution = db.create_pr_review_guide_execution(&comparison, "acme/widget").unwrap();
    db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
        .unwrap();
    db.start_execution_run(&execution.id, "review-1", "mono", "lease-1", "ws-1", "/tmp/ws-1")
        .unwrap();
    db.record_execution_launch_config(&execution.id, "codex", "gpt-6-astra", None)
        .unwrap();
    let pid = std::process::id() as libc::pid_t;
    state.worker_registry.register(pid, execution.id.clone());
    let guide = "# Guide\n## Problem\n## Implementation\n## Example\n## Review";
    let request = || {
        submit_request(
            &execution.id,
            ProposalKind::ReviewGuide,
            json!({"body_markdown": guide}),
        )
    };
    let response = call_with_peer(&state, Some(pid), request()).await;
    let FrontendEvent::ProposalSubmitted {
        proposal,
        already_submitted,
    } = response
    else {
        panic!("{response:?}")
    };
    assert!(!already_submitted);
    assert_eq!(proposal.state, ProposalState::Applied);
    assert_eq!(proposal.applied_ref.as_deref(), Some(attempt.id.as_str()));
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "ready");
    let version = db
        .get_pr_review_guide_version(&summary.readable_version_id.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(version.markdown, guide);
    assert!(db.get_execution(&execution.id).unwrap().status.is_terminal());
    // The same command replays its durable proposal even after finalization.
    state.worker_registry.register(pid, execution.id.clone());
    let replay = call_with_peer(&state, Some(pid), request()).await;
    assert!(matches!(
        replay,
        FrontendEvent::ProposalSubmitted {
            already_submitted: true,
            ..
        }
    ));
}

#[tokio::test]
async fn review_guide_invented_link_is_rejected_and_a_corrected_resubmission_publishes() {
    let (state, _dir) = test_server_state();
    let db = &state.work_db;
    let product = crate::test_support::create_product(db);
    let root = crate::test_support::create_active_chore(db, &product, "guide producer");
    let (series, comparison) = crate::test_support::seed_review_guide_series(db, &root);
    let attempt = db
        .create_pr_review_guide_attempt(&series, &comparison, boss_review_guide::PROMPT_VERSION)
        .unwrap();
    let execution = db.create_pr_review_guide_execution(&comparison, "acme/widget").unwrap();
    db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
        .unwrap();
    db.start_execution_run(&execution.id, "review-1", "mono", "lease-1", "ws-1", "/tmp/ws-1")
        .unwrap();
    db.record_execution_launch_config(&execution.id, "codex", "gpt-6-astra", None)
        .unwrap();
    let pid = std::process::id() as libc::pid_t;
    state.worker_registry.register(pid, execution.id.clone());
    let invented = "# Guide\n## Problem\nSee [x](https://github.com/acme/widget/blob/deadbeefdeadbeefdeadbeefdeadbeefdeadbeef/src/retry.rs#L1)\n## Implementation\n## Example\n## Review";
    let rejected = call_with_peer(
        &state,
        Some(pid),
        submit_request(
            &execution.id,
            ProposalKind::ReviewGuide,
            json!({"body_markdown": invented}),
        ),
    )
    .await;
    let FrontendEvent::ProposalSubmitted {
        proposal,
        already_submitted,
    } = rejected
    else {
        panic!("{rejected:?}")
    };
    assert!(!already_submitted);
    assert_eq!(proposal.state, ProposalState::Rejected);
    assert!(
        proposal
            .decision_reason
            .as_deref()
            .unwrap_or_default()
            .contains("does not resolve to source at the recorded head or merge-base revision"),
        "{:?}",
        proposal.decision_reason
    );
    let still_running = db
        .pr_review_guide_attempt_for_execution(&execution.id)
        .unwrap()
        .unwrap();
    assert_eq!(still_running.status, "running");
    assert!(!db.get_execution(&execution.id).unwrap().status.is_terminal());
    let corrected = "# Guide\n## Problem\n## Implementation\n## Example\n## Review";
    let published = call_with_peer(
        &state,
        Some(pid),
        submit_request(
            &execution.id,
            ProposalKind::ReviewGuide,
            json!({"body_markdown": corrected}),
        ),
    )
    .await;
    let FrontendEvent::ProposalSubmitted {
        proposal,
        already_submitted,
    } = published
    else {
        panic!("{published:?}")
    };
    assert!(!already_submitted);
    assert_eq!(proposal.state, ProposalState::Applied);
    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "ready");
}

#[tokio::test]
async fn review_guide_submission_cannot_bind_to_another_execution_or_a_chore() {
    let fx = WorkerFixture::new();
    let mismatched = call_with_peer(
        &fx.server_state,
        Some(fx.peer_pid),
        submit_request(
            "exec_other",
            ProposalKind::ReviewGuide,
            json!({"body_markdown": "# Guide"}),
        ),
    )
    .await;
    assert!(matches!(mismatched, FrontendEvent::ProposalRejected { .. }));
    let response = call_with_peer(
        &fx.server_state,
        Some(fx.peer_pid),
        submit_request(
            &fx.execution_id,
            ProposalKind::ReviewGuide,
            json!({"body_markdown": "# Guide"}),
        ),
    )
    .await;
    let FrontendEvent::ProposalSubmitted { proposal, .. } = response else {
        panic!("{response:?}")
    };
    assert_eq!(proposal.state, ProposalState::Rejected);
    assert!(
        proposal
            .decision_reason
            .unwrap()
            .contains("no running review-guide attempt")
    );
}
