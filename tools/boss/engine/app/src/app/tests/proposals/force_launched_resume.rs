// Answering a question asked by a force-launched run resumes it past the
// dispatch pause; answering one asked by a normally dispatched run does not.
use super::*;
use async_trait::async_trait;
use boss_engine_dispatch::coordinator::DispatchPauseOrigin;
use boss_engine_runtime::test_support::release_finished_execution_lease;
use boss_execution_contracts::{ExecutionRunner, RunOutcome};
use boss_protocol::{DispatchPauseScope, ExecutionStatus, PauseReason, ProposalKind};

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
struct QuestionRunner(std::sync::atomic::AtomicUsize, tokio::sync::Notify);

#[async_trait]
impl ExecutionRunner for QuestionRunner {
    async fn run_execution(
        &self,
        _worker_id: &str,
        _execution: &boss_protocol::WorkExecution,
        _work_item: &boss_protocol::WorkItem,
        _workspace_path: &std::path::Path,
        _cube_change_id: Option<&str>,
    ) -> anyhow::Result<RunOutcome> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.1.notify_one();
        std::future::pending().await
    }
}

async fn release_worker_for(state: &Arc<ServerState>, execution_id: &str) {
    let Some(worker_id) = state
        .execution_coordinator
        .worker_pool()
        .claims()
        .await
        .into_iter()
        .find(|claim| claim.execution_id == execution_id)
        .map(|claim| claim.worker_id)
    else {
        return;
    };
    state
        .execution_coordinator
        .release_worker_and_kick(&worker_id, execution_id, None)
        .await;
}

async fn wait_for_starts(state: &ServerState, runner: &QuestionRunner, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while runner.0.load(std::sync::atomic::Ordering::SeqCst) < count {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|err| {
        panic!(
            "the runner must receive {count} executions: {err}; executions: {:?}",
            state.work_db.list_executions(None)
        )
    });
}

async fn force_launched_question_holding_predecessor_lease()
-> (Arc<ServerState>, tempfile::TempDir, String, String, Arc<QuestionRunner>) {
    let runner = Arc::new(QuestionRunner::default());
    let (state, dir) = test_server_state_with_overrides(ServerStateOverrides {
        cube_client: Some(Arc::new(boss_engine_runtime::test_support::AlwaysSucceedsCube)),
        execution_runner: Some(runner.clone()),
        ..Default::default()
    });
    state.feature_flags.set("worker_proposals", true).unwrap();
    state.feature_flags.set("run_done_proposals_seam", true).unwrap();
    pause_dispatch(&state);
    let product = boss_engine_runtime::test_support::create_test_product(&state.work_db);
    let task =
        boss_engine_runtime::test_support::create_test_chore(&state.work_db, product.id, "Force launched question");
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
            boss_protocol::WorkItemPatch::builder()
                .description("Original brief")
                .build(),
        )
        .unwrap();
    state
        .execution_coordinator
        .force_dispatch(&asking, boss_execution_contracts::DispatchAdmission::OperatorForced)
        .await
        .unwrap();
    wait_for_starts(&state, &runner, 1).await;
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

async fn force_launched_question() -> (Arc<ServerState>, tempfile::TempDir, String, String, Arc<QuestionRunner>) {
    let (state, dir, asking, task_id, runner) = force_launched_question_holding_predecessor_lease().await;
    // The pending runner fake never returns, so teardown never releases the
    // cube lease. Dispatch holds a successor while that lease is recorded.
    release_finished_execution_lease(&state.work_db, &asking);
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
    wait_for_starts(&state, &runner, 2).await;
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
    assert!(
        state.work_db.execution_force_launched(&resume_id).unwrap(),
        "the force-launch lineage must carry onto the minted resume"
    );
    assert!(
        state.execution_coordinator.is_dispatch_paused(),
        "the pause itself is untouched"
    );
}

#[tokio::test]
async fn a_second_question_on_the_resumed_run_still_bypasses_the_operator_pause() {
    let (state, _dir, asking, task_id, runner) = force_launched_question().await;
    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    let first_resume = resume_execution(&state, &task_id, &asking).id;
    wait_for_starts(&state, &runner, 2).await;
    assert!(state.work_db.execution_force_launched(&first_resume).unwrap());
    release_worker_for(&state, &asking).await;

    state
        .worker_registry
        .register(std::process::id() as libc::pid_t, first_resume.clone());
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&first_resume, ProposalKind::RunDone, operator_question_payload()),
        )
        .await,
    );
    assert!(state.work_db.get_execution(&first_resume).unwrap().status.is_terminal());
    release_finished_execution_lease(&state.work_db, &first_resume);
    release_worker_for(&state, &first_resume).await;

    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    let second_resume = state
        .work_db
        .list_executions(Some(&task_id))
        .unwrap()
        .into_iter()
        .find(|execution| execution.id != asking && execution.id != first_resume)
        .expect("the second Yes answer mints another resume")
        .id;
    wait_for_starts(&state, &runner, 3).await;
    assert_eq!(
        state.work_db.get_execution(&second_resume).unwrap().status,
        ExecutionStatus::Running,
        "the second resume must start without another force launch: {:?}",
        (
            state.work_db.list_executions(Some(&task_id)).unwrap(),
            state.work_db.list_attention_items(&second_resume).unwrap(),
            state.execution_coordinator.dispatch_pause()
        )
    );
    assert!(state.work_db.execution_force_launched(&second_resume).unwrap());
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
async fn unpaused_force_launched_answer_takes_operator_forced_admission() {
    let (state, _dir, asking, task_id, runner) = force_launched_question().await;
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
    assert!(
        state.work_db.execution_force_launched(&resume.id).unwrap(),
        "lineage is recorded even when the pause is already clear"
    );
    assert!(state.work_db.list_attention_items(&resume.id).unwrap().is_empty());
    // The pending runner fake retains the fixture's only worker slot.
    // Model the terminal asker's exit before checking normal admission.
    let worker_id = state
        .execution_coordinator
        .worker_pool()
        .claims()
        .await
        .into_iter()
        .find(|claim| claim.execution_id == asking)
        .expect("the asking runner holds a worker slot")
        .worker_id;
    state
        .execution_coordinator
        .release_worker_and_kick(&worker_id, &asking, None)
        .await;
    state.execution_coordinator.set_dispatch_preflight_block(None);
    state.execution_coordinator.kick();
    while runner.0.load(std::sync::atomic::Ordering::SeqCst) < 2 {
        runner.1.notified().await;
    }
    assert_eq!(
        state.work_db.get_execution(&resume.id).unwrap().status,
        ExecutionStatus::Running,
        "the drain must start the answered resume once preflight clears"
    );
    assert!(state.work_db.execution_force_launched(&resume.id).unwrap());
    assert!(!state.execution_coordinator.is_dispatch_paused());
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
    assert!(
        state.work_db.execution_force_launched(&resume.id).unwrap(),
        "the stamp records lineage and does not bypass a breaker pause"
    );
}

#[tokio::test]
async fn failed_resume_admission_starts_after_the_hold_clears() {
    let (state, _dir, asking, task_id, runner) = force_launched_question().await;
    state
        .execution_coordinator
        .set_dispatch_preflight_block(Some("test runtime unavailable".into()));
    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    let resume = resume_execution(&state, &task_id, &asking);
    assert_eq!(resume.status, ExecutionStatus::Ready);
    assert!(
        state
            .work_db
            .execution_has_operator_forced_admission(&resume.id)
            .unwrap(),
        "a refused immediate launch must keep operator-forced admission"
    );
    assert!(
        state.work_db.list_attention_items(&resume.id).unwrap().is_empty(),
        "a transient hold is not a failed resume: {:?}",
        state.work_db.list_attention_items(&resume.id).unwrap()
    );

    release_worker_for(&state, &asking).await;
    state.execution_coordinator.set_dispatch_preflight_block(None);
    state.execution_coordinator.kick();
    wait_for_starts(&state, &runner, 2).await;
    assert_eq!(
        state.work_db.get_execution(&resume.id).unwrap().status,
        ExecutionStatus::Running,
        "the drain must start the resume after the hold clears, with no second force launch: {:?}",
        (
            state.work_db.list_executions(Some(&task_id)).unwrap(),
            state.execution_coordinator.dispatch_pause()
        )
    );
    assert!(state.work_db.execution_force_launched(&resume.id).unwrap());
}

#[tokio::test]
async fn force_launched_resume_starts_after_predecessor_lease_clears() {
    let (state, _dir, asking, task_id, runner) = force_launched_question_holding_predecessor_lease().await;
    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    let resume = resume_execution(&state, &task_id, &asking);
    assert_eq!(resume.status, ExecutionStatus::Ready);
    assert!(
        state
            .work_db
            .execution_has_operator_forced_admission(&resume.id)
            .unwrap()
    );
    assert_eq!(
        resume.dispatch_wait_reason.as_deref(),
        Some("awaiting_predecessor_preservation"),
        "the asking run still holds its cube lease: {resume:?}"
    );

    release_finished_execution_lease(&state.work_db, &asking);
    release_worker_for(&state, &asking).await;
    state.execution_coordinator.kick();
    wait_for_starts(&state, &runner, 2).await;
    assert_eq!(
        state.work_db.get_execution(&resume.id).unwrap().status,
        ExecutionStatus::Running,
        "the drain must start the resume once the predecessor lease is gone: {:?}",
        state.work_db.list_executions(Some(&task_id)).unwrap()
    );
}

#[tokio::test]
async fn answering_a_force_launched_runs_question_resumes_it_past_an_all_spawns_pause() {
    let (state, _dir, asking, task_id, runner) = force_launched_question().await;
    state.execution_coordinator.pause_dispatch_scoped(
        1_700_000_002,
        DispatchPauseOrigin::Operator,
        DispatchPauseScope::AllSpawns,
        PauseReason::new("test: spawn path suspect").unwrap(),
    );
    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    let resume_id = resume_execution(&state, &task_id, &asking).id;
    wait_for_starts(&state, &runner, 2).await;
    assert_eq!(
        state.work_db.get_execution(&resume_id).unwrap().status,
        ExecutionStatus::Running,
        "operator-forced admission must start the resume under an all-spawns pause"
    );
    assert_eq!(
        state.execution_coordinator.dispatch_pause().unwrap().scope,
        DispatchPauseScope::AllSpawns
    );
}

#[tokio::test]
async fn force_launched_gated_resume_starts_after_the_gate_clears() {
    let (state, _dir, asking, task_id, runner) = force_launched_question().await;
    let product_id = state.work_db.get_work_item(&task_id).unwrap().product_id().to_string();
    let gate = boss_engine_runtime::test_support::create_test_chore_manual(&state.work_db, product_id, "Upstream gate");
    state
        .work_db
        .add_dependency(boss_protocol::AddDependencyInput {
            dependent: task_id.clone(),
            prerequisite: gate.id.clone(),
            relation: None,
        })
        .unwrap();

    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    let resume = resume_execution(&state, &task_id, &asking);
    assert_eq!(resume.status, ExecutionStatus::WaitingDependency);
    assert!(
        state
            .work_db
            .execution_has_operator_forced_admission(&resume.id)
            .unwrap()
    );
    assert!(state.work_db.execution_force_launched(&resume.id).unwrap());

    state
        .work_db
        .update_work_item(
            &gate.id,
            boss_protocol::WorkItemPatch {
                status: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        state.work_db.get_execution(&resume.id).unwrap().status,
        ExecutionStatus::Ready,
        "the waiting execution becomes ready once the gate is done"
    );
    release_worker_for(&state, &asking).await;
    state.execution_coordinator.kick();
    wait_for_starts(&state, &runner, 2).await;
    assert_eq!(
        state.work_db.get_execution(&resume.id).unwrap().status,
        ExecutionStatus::Running,
        "the drain must start the gated resume through the pause: {:?}",
        state.work_db.list_executions(Some(&task_id)).unwrap()
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
        outcome.dispatch_work_item_id.as_deref(),
        Some(task_id.as_str()),
        "resume: {resume:?}"
    );
    assert!(
        state
            .work_db
            .execution_has_operator_forced_admission(&resume.id)
            .unwrap()
    );
}

#[tokio::test]
async fn declining_a_force_launched_runs_question_resumes_it_past_the_pause() {
    let (state, _dir, asking, task_id, runner) = force_launched_question().await;

    assert!(matches!(
        answer_question(&state, &task_id, false).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));

    let resume_id = resume_execution(&state, &task_id, &asking).id;
    wait_for_starts(&state, &runner, 2).await;
    assert_eq!(
        state.work_db.get_execution(&resume_id).unwrap().status,
        ExecutionStatus::Running,
        "a No answer must start the resume without a second force launch"
    );
    assert!(state.work_db.execution_force_launched(&resume_id).unwrap());
    assert!(state.execution_coordinator.is_dispatch_paused());
}

#[tokio::test]
async fn declining_answer_transaction_identifies_force_launched_resume() {
    let (state, _dir, asking, task_id, _runner) = force_launched_question().await;
    let outcome = state
        .work_db
        .answer_operator_question(&task_id, boss_protocol::OperatorAnswer::YesNo { value: false })
        .unwrap();
    let resume = resume_execution(&state, &task_id, &asking);
    assert_eq!(outcome.dispatch_work_item_id.as_deref(), Some(task_id.as_str()));
    assert!(
        state
            .work_db
            .execution_has_operator_forced_admission(&resume.id)
            .unwrap()
    );
}

#[tokio::test]
async fn declining_a_normally_dispatched_runs_question_still_waits_behind_the_pause() {
    let (state, _dir, asking, task_id) = parked_question_task().await;
    pause_dispatch(&state);

    assert!(matches!(
        answer_question(&state, &task_id, false).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));

    let resume_id = resume_execution(&state, &task_id, &asking).id;
    let mut held = false;
    for _ in 0..200 {
        let execution = state.work_db.get_execution(&resume_id).unwrap();
        if execution.dispatch_wait_reason.as_deref() == Some("dispatch_paused") {
            held = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(held, "the pause must hold the resume after a No answer");
    assert!(!state.work_db.execution_force_launched(&resume_id).unwrap());
}

#[tokio::test]
async fn suppressed_start_keeps_lineage_without_operator_forced_admission() {
    let (state, _dir, asking, task_id, _runner) = force_launched_question().await;
    let outcome = state
        .work_db
        .answer_operator_question_from(
            &task_id,
            boss_protocol::OperatorAnswer::YesNo { value: true },
            &boss_work_db::GhPrStateChecker,
            boss_protocol::LAST_STATUS_ACTOR_HUMAN,
            None,
            true,
        )
        .unwrap();
    let resume = resume_execution(&state, &task_id, &asking);
    assert!(
        state.work_db.execution_force_launched(&resume.id).unwrap(),
        "the queued continuation inherits force-launch lineage"
    );
    assert!(
        !state
            .work_db
            .execution_has_operator_forced_admission(&resume.id)
            .unwrap(),
        "a suppressed start must not bypass the pause"
    );
    assert!(outcome.dispatch_work_item_id.is_none());
}
