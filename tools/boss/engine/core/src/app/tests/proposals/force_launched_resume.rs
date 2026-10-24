// Answering a question asked by a force-launched run resumes it past the
// dispatch pause; answering one asked by a normally dispatched run does not.
use super::*;
use boss_protocol::{ExecutionStatus, PauseReason};

use crate::coordinator::DispatchPauseOrigin;

#[path = "question_cube.rs"]
mod question_cube;

fn pause_dispatch(state: &Arc<ServerState>) {
    state.execution_coordinator.pause_dispatch(
        1_700_000_000,
        DispatchPauseOrigin::Operator,
        PauseReason::new("test: investigating worker failures").unwrap(),
    );
}

fn resume_execution(state: &Arc<ServerState>, task_id: &str, asking_execution: &str) -> boss_protocol::WorkExecution {
    let executions = state.work_db.list_executions(Some(task_id)).unwrap();
    assert_eq!(executions.len(), 2, "one asker and one resume: {executions:?}");
    executions
        .into_iter()
        .find(|execution| execution.id != asking_execution)
        .expect("a Yes answer mints the resume execution")
}

#[derive(Default)]
struct QuestionRunner(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl crate::runner::ExecutionRunner for QuestionRunner {
    async fn run_execution(
        &self,
        _worker_id: &str,
        _execution: &boss_protocol::WorkExecution,
        _work_item: &crate::work::WorkItem,
        _workspace_path: &std::path::Path,
        _cube_change_id: Option<&str>,
    ) -> anyhow::Result<crate::runner::RunOutcome> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::pending().await
    }
}

async fn wait_for_starts(runner: &QuestionRunner, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while runner.0.load(std::sync::atomic::Ordering::SeqCst) < count {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the runner must receive the execution");
}

async fn force_launched_question() -> (Arc<ServerState>, tempfile::TempDir, String, String, Arc<QuestionRunner>) {
    let runner = Arc::new(QuestionRunner::default());
    let (state, dir) = test_server_state_with_overrides(ServerStateOverrides {
        cube_client: Some(Arc::new(question_cube::QuestionCube::new())),
        execution_runner: Some(runner.clone()),
        ..Default::default()
    });
    state.feature_flags.set("worker_proposals", true).unwrap();
    state.feature_flags.set("run_done_proposals_seam", true).unwrap();
    pause_dispatch(&state);
    let product = crate::test_support::create_test_product(&state.work_db);
    let task = crate::test_support::create_test_chore(&state.work_db, product.id, "Force launched question");
    let task_id = task.id;
    let asking = state
        .work_db
        .request_execution(
            boss_protocol::RequestExecutionInput::builder()
                .work_item_id(&task_id)
                .build(),
        )
        .unwrap()
        .id;
    state
        .work_db
        .update_work_item(
            &task_id,
            boss_protocol::WorkItemPatch {
                description: Some("Original brief".into()),
                ..Default::default()
            },
        )
        .unwrap();
    state
        .execution_coordinator
        .force_dispatch(&asking, crate::coordinator::DispatchAdmission::OperatorForced)
        .await
        .unwrap();
    wait_for_starts(&runner, 1).await;
    assert_eq!(
        state.work_db.get_execution(&asking).unwrap().status,
        ExecutionStatus::Running
    );
    state
        .worker_registry
        .register(std::process::id() as libc::pid_t, asking.clone());
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&asking, ProposalKind::RunDone, operator_question_payload()),
        )
        .await,
    );
    assert!(state.work_db.execution_force_launched(&asking).unwrap());
    assert!(state.work_db.get_execution(&asking).unwrap().status.is_terminal());
    assert_eq!(
        state.work_db.list_executions(Some(&task_id)).unwrap().len(),
        1,
        "only the asking execution exists before answering"
    );
    assert_eq!(
        state.execution_coordinator.dispatch_pause().unwrap().origin,
        DispatchPauseOrigin::Operator
    );
    (state, dir, asking, task_id, runner)
}

#[tokio::test]
async fn answering_a_force_launched_runs_question_resumes_it_past_the_pause() {
    let (state, _dir, asking, task_id, runner) = force_launched_question().await;

    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));

    let resume_id = resume_execution(&state, &task_id, &asking).id;
    wait_for_starts(&runner, 2).await;
    assert_eq!(
        state.work_db.get_execution(&resume_id).unwrap().status,
        ExecutionStatus::Running,
        "the resume must start without a second force launch: {:?}",
        (
            state.work_db.list_executions(Some(&task_id)).unwrap(),
            state.work_db.list_attention_items(&resume_id).unwrap(),
            state.execution_coordinator.dispatch_pause()
        )
    );
    assert!(!state.work_db.execution_force_launched(&resume_id).unwrap());
    assert!(
        state.execution_coordinator.is_dispatch_paused(),
        "the pause itself is untouched"
    );
}

#[tokio::test]
async fn answering_a_normally_dispatched_runs_question_still_waits_behind_the_pause() {
    let (state, _dir, asking, task_id) = parked_question_task().await;
    assert!(!state.work_db.execution_force_launched(&asking).unwrap());
    pause_dispatch(&state);

    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));

    let resume_id = resume_execution(&state, &task_id, &asking).id;
    let mut held = None;
    for _ in 0..200 {
        let execution = state.work_db.get_execution(&resume_id).unwrap();
        if execution.dispatch_wait_reason.as_deref() == Some("dispatch_paused") {
            held = Some(execution);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let held = held.expect("the scheduler must record that the pause is holding the resume");
    assert_eq!(held.status, ExecutionStatus::Ready);
    assert!(!state.work_db.execution_force_launched(&resume_id).unwrap());
}

#[tokio::test]
async fn unpaused_answer_uses_normal_dispatch() {
    let (state, _dir, asking, task_id, _runner) = force_launched_question().await;
    state
        .execution_coordinator
        .set_dispatch_preflight_block(Some("test normal dispatch hold".into()));
    state.execution_coordinator.resume_dispatch();
    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    let resume = resume_execution(&state, &task_id, &asking);
    assert_eq!(resume.status, ExecutionStatus::Ready);
    assert!(!state.work_db.execution_force_launched(&resume.id).unwrap());
    assert!(state.work_db.list_attention_items(&resume.id).unwrap().is_empty());
}

#[tokio::test]
async fn breaker_pause_holds_answered_resume() {
    let (state, _dir, asking, task_id, _runner) = force_launched_question().await;
    state.execution_coordinator.resume_dispatch();
    state.execution_coordinator.pause_dispatch(
        1_700_000_001,
        DispatchPauseOrigin::Breaker,
        PauseReason::new("spawn path failed").unwrap(),
    );
    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    let resume = resume_execution(&state, &task_id, &asking);
    assert_eq!(resume.status, ExecutionStatus::Ready);
    assert!(!state.work_db.execution_force_launched(&resume.id).unwrap());
}

#[tokio::test]
async fn failed_resume_admission_has_visible_attention() {
    let (state, _dir, asking, task_id, _runner) = force_launched_question().await;
    state
        .execution_coordinator
        .set_dispatch_preflight_block(Some("test runtime unavailable".into()));
    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    let resume = resume_execution(&state, &task_id, &asking);
    assert_eq!(resume.status, ExecutionStatus::Ready);
    let attention = state.work_db.list_attention_items(&resume.id).unwrap();
    assert!(
        attention
            .iter()
            .any(|item| item.kind == "question_resume_dispatch_failed"
                && item.body_markdown.contains("test runtime unavailable"))
    );
}

#[tokio::test]
async fn answer_transaction_identifies_force_launched_resume() {
    let (state, _dir, asking, task_id, _runner) = force_launched_question().await;
    let questions = state.work_db.list_operator_questions(&task_id).unwrap();
    assert_eq!(questions[0].question.execution_id, asking);
    let outcome = state
        .work_db
        .answer_operator_question(&task_id, boss_protocol::OperatorAnswer::YesNo { value: true })
        .unwrap();
    let resume = resume_execution(&state, &task_id, &asking);
    assert_eq!(
        outcome.force_resume_execution_id,
        Some(resume.id.clone()),
        "resume: {resume:?}"
    );
}
