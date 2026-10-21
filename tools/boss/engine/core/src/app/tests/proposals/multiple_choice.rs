// Multiple-choice questions: a blocked worker offers two to six options, one
// is chosen, and the engine records exactly that string.
use super::*;

fn multiple_choice_payload() -> Value {
    json!({"outcome":"blocked", "summary":"Two rollouts fit the brief", "question":{
        "text":"Which rollout should I use?",
        "answer_type":{"kind":"multiple_choice", "choices":["Gradual", "All at once", "Behind a flag"]},
        "explanation":"The brief allows each of them."
    }})
}

async fn choose(state: &Arc<ServerState>, id: &str, value: &str) -> FrontendEvent {
    call_with_peer(
        state,
        None,
        FrontendRequest::AnswerOperatorQuestion {
            id: id.into(),
            answer: boss_protocol::OperatorAnswer::MultipleChoice { value: value.into() },
        },
    )
    .await
}

#[tokio::test]
async fn multiple_choice_question_parks_with_its_options_on_the_card() {
    let (state, _dir, _, task_id) = parked_task_with_question(multiple_choice_payload()).await;
    let task = question_task(&state, &task_id);
    assert_eq!(task.status, boss_protocol::TaskStatus::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some("awaiting_operator_answer"));
    let question = task.operator_question.unwrap();
    assert_eq!(
        question.answer_type,
        boss_protocol::OperatorAnswerType::MultipleChoice {
            choices: vec!["Gradual".into(), "All at once".into(), "Behind a flag".into()]
        }
    );
}

#[tokio::test]
async fn choosing_records_the_clicked_option_closes_the_question_and_restarts_the_task() {
    let (state, _dir, old_execution, task_id) = parked_task_with_question(multiple_choice_payload()).await;
    let question = question_task(&state, &task_id).operator_question.unwrap();
    let answer = choose(&state, &question.id, "All at once").await;
    let FrontendEvent::WorkItemUpdated {
        item: crate::work::WorkItem::Task(task),
    } = answer
    else {
        panic!("expected updated task: {answer:?}");
    };
    assert!(task.operator_question.is_none(), "the question closes");
    assert_eq!(task.status, boss_protocol::TaskStatus::Todo);
    assert!(task.blocked_reason.is_none());
    assert_eq!(task.last_status_actor, "human");

    let history = state.work_db.list_operator_questions(&task_id).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, boss_protocol::OperatorQuestionStatus::Answered);
    assert_eq!(
        history[0].answer,
        Some(boss_protocol::OperatorAnswer::MultipleChoice {
            value: "All at once".into()
        }),
        "the recorded answer is the option that was clicked, not another one"
    );
    assert_eq!(history[0].answered_by.as_deref(), Some("human"));

    // The next run's brief carries the chosen option, and only grants the decision.
    assert!(
        task.description
            .starts_with("Original brief\n\n---\n\n## Recorded answer (")
    );
    assert!(task.description.contains("- **Operator chose:** All at once\n"));
    assert!(
        task.description
            .contains("- **Options offered:** Gradual / All at once / Behind a flag\n")
    );
    assert!(task.description.contains(&format!("`{old_execution}`")));
    assert!(task.description.contains("does not authorize bypassing any check"));
    assert!(!task.description.contains("Operator authorization"));
    let executions = state.work_db.list_executions(Some(&task_id)).unwrap();
    assert_eq!(executions.len(), 2);
    assert!(
        executions
            .iter()
            .any(|e| e.status == boss_protocol::ExecutionStatus::Ready)
    );

    // Repeating the same pick is idempotent; a different pick or a Yes/No is refused.
    assert!(matches!(
        choose(&state, &question.id, "All at once").await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    assert_eq!(state.work_db.list_executions(Some(&task_id)).unwrap().len(), 2);
    assert!(matches!(
        choose(&state, &question.id, "Gradual").await,
        FrontendEvent::OperatorQuestionError {
            error: boss_protocol::OperatorQuestionError::Conflict {
                state,
                answer: Some(boss_protocol::OperatorAnswer::MultipleChoice { value }),
            }
        } if state == "answered" && value == "All at once"
    ));
}

#[tokio::test]
async fn an_answer_that_does_not_fit_the_question_is_refused_and_leaves_it_open() {
    let (state, _dir, _, task_id) = parked_task_with_question(multiple_choice_payload()).await;
    let question = question_task(&state, &task_id).operator_question.unwrap();
    for refused in [
        choose(&state, &question.id, "Something else").await,
        choose(&state, &question.id, "gradual").await,
        answer_question(&state, &question.id, true).await,
        answer_question(&state, &question.id, false).await,
    ] {
        assert!(
            matches!(
                refused,
                FrontendEvent::OperatorQuestionError {
                    error: boss_protocol::OperatorQuestionError::ValidationFailed { .. }
                }
            ),
            "{refused:?}"
        );
    }
    let task = question_task(&state, &task_id);
    assert_eq!(task.status, boss_protocol::TaskStatus::Blocked);
    assert_eq!(task.operator_question.unwrap().id, question.id, "still open");
    assert_eq!(state.work_db.list_executions(Some(&task_id)).unwrap().len(), 1);

    // And a choice cannot answer a Yes/No question.
    let (state, _dir, _, task_id) = parked_question_task().await;
    let question = question_task(&state, &task_id).operator_question.unwrap();
    assert!(matches!(
        choose(&state, &question.id, "Yes").await,
        FrontendEvent::OperatorQuestionError {
            error: boss_protocol::OperatorQuestionError::ValidationFailed { .. }
        }
    ));
    assert!(question_task(&state, &task_id).operator_question.is_some());
}

#[tokio::test]
async fn a_multiple_choice_proposal_with_bad_options_is_rejected_at_submission() {
    let (state, _dir, execution_id, _) = live_chore_execution();
    for choices in [json!(["only one"]), json!(["a", "a"]), json!(["a", ""])] {
        let mut payload = multiple_choice_payload();
        payload["question"]["answer_type"]["choices"] = choices;
        let response = call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&execution_id, ProposalKind::RunDone, payload),
        )
        .await;
        assert!(
            matches!(response, FrontendEvent::ProposalRejected { .. }),
            "{response:?}"
        );
    }
}

#[tokio::test]
async fn unparkable_multiple_choice_retains_answerable_options() {
    let (state, _dir, execution_id, task_id) = live_chore_execution();
    state
        .work_db
        .connect()
        .unwrap()
        .execute("UPDATE tasks SET status = ?2 WHERE id = ?1", [&task_id, "in_review"])
        .unwrap();
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&execution_id, ProposalKind::RunDone, multiple_choice_payload()),
        )
        .await,
    );
    let (id, kind, options): (String, String, String) = state
        .work_db
        .connect()
        .unwrap()
        .query_row(
            "SELECT a.id, a.question_type, a.choice_options FROM attentions a
         JOIN attention_groups g ON g.id = a.group_id
         WHERE g.association_task_id = ?1 AND g.state = ?2",
            [&task_id, "open"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(kind, "multiple_choice");
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&options).unwrap(),
        vec!["Gradual", "All at once", "Behind a flag"]
    );
    let group = state
        .work_db
        .answer_attention(&id, Some("Gradual".into()), false, false)
        .unwrap();
    let members = state.work_db.list_attentions_for_group(&group.id).unwrap();
    assert_eq!(members[0].answer.as_deref(), Some("Gradual"));
}
