//! Answering a question on a row that a dependency still gates, and answering
//! with the start suppressed, go through the real `AnswerOperatorQuestion`
//! handler: the answer is recorded and the request succeeds without the engine
//! asking the coordinator to start the work.

use super::*;
use boss_protocol::{ExecutionStatus, OperatorAnswer, OperatorQuestionStatus, TaskStatus};

async fn answer_with(state: &Arc<ServerState>, id: &str, value: bool, suppress_start: bool) -> FrontendEvent {
    call_with_peer(
        state,
        None,
        FrontendRequest::AnswerOperatorQuestion {
            id: id.into(),
            answer: OperatorAnswer::YesNo { value },
            suppress_start,
        },
    )
    .await
}

fn add_gate(state: &ServerState, task_id: &str) -> String {
    let product_id = question_task(state, task_id).product_id;
    let gate = boss_engine_runtime::test_support::create_test_chore_manual(&state.work_db, product_id, "Upstream gate");
    state
        .work_db
        .add_dependency(boss_protocol::AddDependencyInput {
            dependent: task_id.to_owned(),
            prerequisite: gate.id.clone(),
            relation: None,
        })
        .unwrap();
    gate.id
}

fn statuses(state: &ServerState, task_id: &str) -> Vec<ExecutionStatus> {
    state
        .work_db
        .list_executions(Some(task_id))
        .unwrap()
        .into_iter()
        .map(|execution| execution.status)
        .collect()
}

fn stored_answer(state: &ServerState, task_id: &str) -> Option<OperatorAnswer> {
    state.work_db.list_operator_questions(task_id).unwrap()[0]
        .answer
        .clone()
}

#[tokio::test]
async fn answering_a_gated_row_succeeds_and_dispatches_after_the_gate_clears() {
    for value in [true, false] {
        let (state, _dir, _, task_id) = parked_question_task().await;
        let gate_id = add_gate(&state, &task_id);
        let before = statuses(&state, &task_id);

        let FrontendEvent::WorkItemUpdated {
            item: boss_work_db::WorkItem::Task(task),
        } = answer_with(&state, &task_id, value, false).await
        else {
            panic!("answering a gated row must succeed (answer {value})");
        };
        assert_eq!(task.status, TaskStatus::Blocked);
        assert_eq!(task.blocked_reason.as_deref(), Some("dependency"));
        assert_eq!(
            task.blocked_detail.as_deref(),
            Some("Answered · waits for Upstream gate"),
            "the card can say the row is answered and what it waits for"
        );
        let history = state.work_db.list_operator_questions(&task_id).unwrap();
        assert_eq!(history[0].status, OperatorQuestionStatus::Answered);
        assert_eq!(stored_answer(&state, &task_id), Some(OperatorAnswer::YesNo { value }));
        assert_eq!(
            task.description.contains("Repo owner approval"),
            value,
            "Yes records the approval note; No records the declined path: {}",
            task.description
        );
        assert!(task.description.contains("Recorded answer") || value);
        // No start was attempted: the only new execution is waiting on its gate.
        let after = statuses(&state, &task_id);
        assert_eq!(after.len(), before.len() + 1, "{after:?}");
        assert!(after.contains(&ExecutionStatus::WaitingDependency), "{after:?}");
        assert!(!after.contains(&ExecutionStatus::Ready), "{after:?}");

        // The gate completing through the normal path releases the row.
        state
            .work_db
            .update_work_item(
                &gate_id,
                boss_protocol::WorkItemPatch {
                    status: Some("done".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            statuses(&state, &task_id).contains(&ExecutionStatus::Ready),
            "the waiting execution becomes ready once the gate is done"
        );
    }
}

#[tokio::test]
async fn suppressed_start_records_the_answer_without_requesting_a_start() {
    for gated in [false, true] {
        let (state, _dir, _, task_id) = parked_question_task().await;
        if gated {
            add_gate(&state, &task_id);
        }
        let before = statuses(&state, &task_id);
        let FrontendEvent::WorkItemUpdated {
            item: boss_work_db::WorkItem::Task(task),
        } = answer_with(&state, &task_id, true, true).await
        else {
            panic!("a suppressed-start answer must succeed (gated {gated})");
        };
        assert_eq!(
            stored_answer(&state, &task_id),
            Some(OperatorAnswer::YesNo { value: true })
        );
        assert!(task.description.contains("Repo owner approval"));
        let after = statuses(&state, &task_id);
        if gated {
            assert!(!after.contains(&ExecutionStatus::Ready), "{after:?}");
            assert_eq!(task.status, TaskStatus::Blocked);
        } else {
            assert_eq!(task.status, TaskStatus::Todo);
            // The answer leaves a schedulable continuation instead of stranding
            // the row: a never-dispatched ready execution, with no start requested.
            assert_eq!(after.len(), before.len() + 1, "{after:?}");
            assert_eq!(after.last(), Some(&ExecutionStatus::Ready), "{after:?}");
        }
    }
}

/// The continuation an ungated suppressed answer leaves is a normal ready
/// execution: the scheduler holds it while dispatch is paused and admits it
/// once the pause lifts.
#[tokio::test]
async fn suppressed_answer_continuation_follows_the_pause_and_dispatches_when_it_lifts() {
    use boss_engine_dispatch::coordinator::DispatchPauseOrigin;

    let (state, _dir, _, task_id) = parked_question_task().await;
    state.execution_coordinator.pause_dispatch(
        1_786_385_816,
        DispatchPauseOrigin::Operator,
        boss_protocol::PauseReason::new("test: operator pause").unwrap(),
    );
    let before = statuses(&state, &task_id);
    assert!(matches!(
        answer_with(&state, &task_id, true, true).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    let continuation = state
        .work_db
        .list_executions(Some(&task_id))
        .unwrap()
        .into_iter()
        .find(|execution| execution.status == ExecutionStatus::Ready)
        .expect("a ready continuation");
    assert_eq!(statuses(&state, &task_id).len(), before.len() + 1);

    state.execution_coordinator.drain_ready_queue_for_test().await;
    let held = state.work_db.get_execution(&continuation.id).unwrap();
    assert_eq!(held.status, ExecutionStatus::Ready, "the pause holds the continuation");

    state.execution_coordinator.resume_dispatch();
    state.execution_coordinator.drain_ready_queue_for_test().await;
    let admitted = state.work_db.get_execution(&continuation.id).unwrap();
    assert_ne!(
        admitted.status,
        ExecutionStatus::Ready,
        "the continuation is admitted once the pause lifts"
    );
}
