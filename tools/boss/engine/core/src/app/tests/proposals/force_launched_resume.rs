// Answering a question asked by a force-launched run resumes it past the
// dispatch pause; answering one asked by a normally dispatched run does not.
use super::*;
use boss_protocol::{ExecutionStatus, PauseReason};

use crate::coordinator::DispatchPauseOrigin;

fn pause_dispatch(state: &Arc<ServerState>) {
    state.execution_coordinator.pause_dispatch(
        1_700_000_000,
        DispatchPauseOrigin::Operator,
        PauseReason::new("test: investigating worker failures").unwrap(),
    );
}

fn resume_execution(state: &Arc<ServerState>, task_id: &str, asking_execution: &str) -> boss_protocol::WorkExecution {
    state
        .work_db
        .list_executions(Some(task_id))
        .unwrap()
        .into_iter()
        .find(|execution| execution.id != asking_execution)
        .expect("a Yes answer mints the resume execution")
}

#[tokio::test]
async fn answering_a_force_launched_runs_question_resumes_it_past_the_pause() {
    let (state, _dir, asking, task_id) = parked_question_task().await;
    state.work_db.mark_execution_force_launched(&asking).unwrap();
    pause_dispatch(&state);

    assert!(matches!(
        answer_question(&state, &task_id, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));

    let resume_id = resume_execution(&state, &task_id, &asking).id;
    for _ in 0..200 {
        if state.work_db.get_execution(&resume_id).unwrap().status != ExecutionStatus::Ready {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_ne!(
        state.work_db.get_execution(&resume_id).unwrap().status,
        ExecutionStatus::Ready,
        "the resume must start without a second force launch"
    );
    assert!(state.work_db.execution_force_launched(&resume_id).unwrap());
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
