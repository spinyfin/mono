// Multiple-choice questions: a blocked worker offers two to six options, one
// is chosen, and the engine records exactly that string.
use super::*;

pub(super) fn multiple_choice_payload() -> Value {
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
            suppress_start: false,
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
        item: boss_work_db::WorkItem::Task(task),
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

/// The engine's own question, filed when progression ran out of attempts on a
/// stuck chore. The archive commits atomically with the answer.
#[tokio::test]
async fn archive_it_archives_the_item_progression_could_not_unstick() {
    let (state, _dir, _, task_id) = parked_task_with_question(multiple_choice_payload()).await;
    let product = question_task(&state, &task_id).product_id;
    let chore =
        boss_engine_runtime::test_support::create_test_chore_manual(&state.work_db, &product, "Stuck for good").id;
    let sql = |sql: String| state.work_db.connect().unwrap().execute_batch(&sql).unwrap();
    sql(format!(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'merge_conflict' WHERE id = '{chore}';
         UPDATE task_status_audit SET changed_at = CAST(changed_at AS INTEGER) - 10800 WHERE work_item_id = '{chore}'"
    ));
    for hours in [20, 10] {
        let run = state
            .work_db
            .create_execution(
                boss_protocol::CreateExecutionInput::builder()
                    .work_item_id(&chore)
                    .kind(boss_protocol::ExecutionKind::Progression)
                    .status(boss_protocol::ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap();
        sql(format!(
            "UPDATE work_executions SET progression_disposition = 'progressed',
                 created_at = CAST(strftime('%s','now') AS INTEGER) - {hours} * 3600,
                 started_at = CAST(strftime('%s','now') AS INTEGER) - {hours} * 3600,
                 finished_at = CAST(strftime('%s','now') AS INTEGER) - {hours} * 3600 WHERE id = '{}'",
            run.id
        ));
    }
    let mint = state.work_db.mint_progression_executions().unwrap();
    assert_eq!((mint.asked.len(), mint.minted.len()), (1, 0));
    let question = state
        .work_db
        .list_operator_questions(&chore)
        .unwrap()
        .remove(0)
        .question;

    let answer = choose(&state, &question.id, boss_work_db::ARCHIVE_IT).await;

    let FrontendEvent::WorkItemUpdated { item } = answer else {
        panic!("expected the updated item: {answer:?}");
    };
    let (boss_work_db::WorkItem::Chore(task) | boss_work_db::WorkItem::Task(task)) = item else {
        panic!("expected a chore");
    };
    assert_eq!(task.status, boss_protocol::TaskStatus::Archived);
    assert_eq!(
        task.archived_reason.as_deref(),
        Some("progression could not unstick it; the owner chose to archive it")
    );
    assert_eq!(
        task.archived_by.as_deref(),
        Some("manual_status_change"),
        "archived as the owner"
    );
}

fn disposition_payload(choice: &str) -> Value {
    let mut payload = multiple_choice_payload();
    payload["question"]["answer_type"]["choices"] = json!([choice, "Pick a PR", "Keep trying"]);
    payload
}

#[tokio::test]
async fn owner_archive_choices_archive_once_without_a_new_worker_or_question() {
    for choice in ["Archive this chore", "Archive this item", "Archive this task"] {
        let (state, _dir, _, task_id) = parked_task_with_question(disposition_payload(choice)).await;
        let question = question_task(&state, &task_id).operator_question.unwrap();
        for _ in 0..2 {
            assert!(matches!(
                choose(&state, &question.id, choice).await,
                FrontendEvent::WorkItemUpdated { .. }
            ));
            let task = question_task(&state, &task_id);
            assert_eq!(task.status, boss_protocol::TaskStatus::Archived);
            assert_eq!(task.last_status_actor, "human");
            assert_eq!(task.archived_by.as_deref(), Some("manual_status_change"));
            assert!(task.operator_question.is_none());
            assert_eq!(state.work_db.list_executions(Some(&task_id)).unwrap().len(), 1);
        }
        let history = state.work_db.list_operator_questions(&task_id).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, boss_protocol::OperatorQuestionStatus::Answered);
        assert_eq!(history[0].answered_by.as_deref(), Some("human"));
        let archives: i64 = state
            .work_db
            .connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1 AND property = 'status' AND new_value = 'archived'",
                [&task_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(archives, 1, "a repeated answer must not archive twice");
    }
}

fn open_continuations(state: &Arc<ServerState>, task_id: &str) -> usize {
    state
        .work_db
        .list_executions(Some(task_id))
        .unwrap()
        .iter()
        .filter(|e| {
            matches!(
                e.status,
                boss_protocol::ExecutionStatus::Ready
                    | boss_protocol::ExecutionStatus::Queued
                    | boss_protocol::ExecutionStatus::WaitingDependency
            )
        })
        .count()
}

#[tokio::test]
async fn owner_park_choice_parks_without_launching_and_unparks_into_one_continuation() {
    for revision in [false, true] {
        let (state, _dir, _, task_id) =
            parked_task_with_question_kind(disposition_payload("Park this item"), revision).await;
        let task = question_task(&state, &task_id);
        let root_id = task.parent_task_id.clone().unwrap_or_else(|| task_id.clone());
        let question = task.operator_question.unwrap();
        assert!(matches!(
            choose(&state, &question.id, "Park this item").await,
            FrontendEvent::WorkItemUpdated { .. }
        ));
        let task = question_task(&state, &task_id);
        assert_eq!(task.parked_by.as_deref(), Some("human"));
        assert!(state.work_db.is_parked(&task_id).unwrap());
        assert!(state.work_db.is_parked(&root_id).unwrap());
        assert!(task.operator_question.is_none());
        assert_eq!(task.status, boss_protocol::TaskStatus::Todo);
        assert!(task.autostart, "unpark must restore dispatch eligibility");
        assert!(task.description.contains("Operator chose:** Park this item"));
        assert_eq!(open_continuations(&state, &task_id), 0, "nothing launches while parked");
        // A repeated answer neither mints nor launches anything.
        assert!(matches!(
            choose(&state, &question.id, "Park this item").await,
            FrontendEvent::WorkItemUpdated { .. }
        ));
        assert_eq!(open_continuations(&state, &task_id), 0);

        state
            .work_db
            .park_work_item_as_actor(&root_id, false, None, "human")
            .unwrap();
        let task = question_task(&state, &task_id);
        assert!(task.parked_at.is_none());
        assert!(task.blocked_reason.is_none());
        assert!(task.operator_question.is_none());
        if !revision {
            // The fixture builds a project-less project task, which the
            // execution reconcile does not mint for; present it as the
            // ordinary chore the un-park sequence reconciles.
            let conn = state.work_db.connect().unwrap();
            conn.execute("UPDATE tasks SET kind = 'chore' WHERE id = ?1", [&task_id])
                .unwrap();
            conn.execute(
                "UPDATE work_executions SET kind = 'chore_implementation' WHERE work_item_id = ?1",
                [&task_id],
            )
            .unwrap();
        }
        // The un-park sequence reconciles the released item's executions.
        state.work_db.reconcile_item_execution(&task_id).unwrap();
        assert_eq!(
            open_continuations(&state, &task_id),
            1,
            "unpark resumes exactly one continuation (revision: {revision}): {:?}",
            state.work_db.list_executions(Some(&task_id)).unwrap()
        );
        state.work_db.reconcile_item_execution(&task_id).unwrap();
        assert_eq!(open_continuations(&state, &task_id), 1);
    }
}

#[tokio::test]
async fn owner_park_choice_with_an_unmet_prerequisite_records_the_answer_and_stays_gated() {
    let (state, _dir, _, task_id) = parked_task_with_question(disposition_payload("Park this item")).await;
    let task = question_task(&state, &task_id);
    let question = task.operator_question.clone().unwrap();
    let prerequisite =
        boss_engine_runtime::test_support::create_test_chore_manual(&state.work_db, &task.product_id, "Gate").id;
    state
        .work_db
        .add_dependency(boss_protocol::AddDependencyInput {
            dependent: task_id.clone(),
            prerequisite: prerequisite.clone(),
            relation: None,
        })
        .unwrap();
    assert!(matches!(
        choose(&state, &question.id, "Park this item").await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    let task = question_task(&state, &task_id);
    assert!(task.operator_question.is_none());
    assert_eq!(
        state.work_db.list_operator_questions(&task_id).unwrap()[0].status,
        boss_protocol::OperatorQuestionStatus::Answered
    );
    assert!(state.work_db.is_parked(&task_id).unwrap());
    assert_eq!(task.status, boss_protocol::TaskStatus::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some("dependency"));
    assert!(
        state
            .work_db
            .gating_prereqs_for(&task_id)
            .unwrap()
            .contains(&prerequisite),
        "the dependency edge survives"
    );
    assert_eq!(open_continuations(&state, &task_id), 0);
}

#[tokio::test]
async fn ordinary_choices_still_resume_including_pr_selection_and_keep_trying() {
    for choice in ["Pick a PR", "Keep trying", "Archive the old logs"] {
        let (state, _dir, _, task_id) = parked_task_with_question(disposition_payload("Archive the old logs")).await;
        let question = question_task(&state, &task_id).operator_question.unwrap();
        assert!(matches!(
            choose(&state, &question.id, choice).await,
            FrontendEvent::WorkItemUpdated { .. }
        ));
        let task = question_task(&state, &task_id);
        assert_eq!(task.status, boss_protocol::TaskStatus::Todo);
        assert!(task.parked_at.is_none());
        assert!(task.description.contains(&format!("Operator chose:** {choice}")));
        assert_eq!(state.work_db.list_executions(Some(&task_id)).unwrap().len(), 2);
    }
}

#[tokio::test]
async fn failed_archive_rolls_back_the_answer_so_the_owner_can_retry() {
    let (state, _dir, _, task_id) = parked_task_with_question(disposition_payload("Archive this item")).await;
    let question = question_task(&state, &task_id).operator_question.unwrap();
    state
        .work_db
        .connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_test_archive BEFORE UPDATE OF status ON tasks
         WHEN NEW.status = 'archived' BEGIN SELECT RAISE(ABORT, 'archive failed'); END;",
        )
        .unwrap();
    assert!(!matches!(
        choose(&state, &question.id, "Archive this item").await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    assert_eq!(
        question_task(&state, &task_id).operator_question.unwrap().id,
        question.id
    );
    assert_eq!(
        state.work_db.list_operator_questions(&task_id).unwrap()[0].status,
        boss_protocol::OperatorQuestionStatus::Open
    );
    state
        .work_db
        .connect()
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_test_archive")
        .unwrap();
    assert!(matches!(
        choose(&state, &question.id, "Archive this item").await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    assert_eq!(
        question_task(&state, &task_id).status,
        boss_protocol::TaskStatus::Archived
    );
}

#[tokio::test]
async fn disposition_labels_chosen_by_a_non_owner_resume_the_worker_instead_of_acting() {
    for (actor, choice) in [
        ("worker", "Archive this item"),
        ("worker", "Park this item"),
        ("boss", "Archive this item"),
        ("boss", "Park this item"),
    ] {
        let (state, _dir, _, task_id) = parked_task_with_question(disposition_payload(choice)).await;
        let question = question_task(&state, &task_id).operator_question.unwrap();
        state
            .work_db
            .answer_operator_question_from(
                &question.id,
                boss_protocol::OperatorAnswer::MultipleChoice { value: choice.into() },
                state.pr_state_checker.as_ref(),
                actor,
                None,
                false,
            )
            .unwrap();
        let task = question_task(&state, &task_id);
        assert_eq!(task.status, boss_protocol::TaskStatus::Todo, "{actor} {choice}");
        assert!(task.parked_at.is_none(), "{actor} {choice} must not park");
        assert!(task.description.contains(&format!("Operator chose:** {choice}")));
    }
}

#[tokio::test]
async fn bare_archive_and_park_labels_on_an_unrelated_question_resume_the_worker() {
    for choice in ["Archive", "Park", "Archive it", "Park it"] {
        let mut payload = multiple_choice_payload();
        payload["question"]["text"] = json!("What should happen to the old output?");
        payload["question"]["answer_type"]["choices"] = json!([choice, "Delete", "Keep"]);
        let (state, _dir, _, task_id) = parked_task_with_question(payload).await;
        let question = question_task(&state, &task_id).operator_question.unwrap();
        assert!(matches!(
            choose(&state, &question.id, choice).await,
            FrontendEvent::WorkItemUpdated { .. }
        ));
        let task = question_task(&state, &task_id);
        assert_eq!(task.status, boss_protocol::TaskStatus::Todo, "{choice}");
        assert!(task.parked_at.is_none(), "{choice}");
        assert!(task.description.contains(&format!("Operator chose:** {choice}")));
        assert_eq!(state.work_db.list_executions(Some(&task_id)).unwrap().len(), 2);
    }
}

#[tokio::test]
async fn owner_archive_choice_archives_a_revision() {
    let (state, _dir, _, task_id) =
        parked_task_with_question_kind(disposition_payload("Archive this revision"), true).await;
    let question = question_task(&state, &task_id).operator_question.unwrap();
    assert!(matches!(
        choose(&state, &question.id, "Archive this revision").await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    let task = question_task(&state, &task_id);
    assert_eq!(task.status, boss_protocol::TaskStatus::Archived);
    assert_eq!(task.last_status_actor, "human");
    assert!(task.operator_question.is_none());
    assert_eq!(
        state.work_db.list_operator_questions(&task_id).unwrap()[0].status,
        boss_protocol::OperatorQuestionStatus::Answered
    );
}

#[tokio::test]
async fn archive_this_chore_closes_an_unbound_merge_conflict_chore() {
    let (state, _dir, execution, id) = live_chore_execution();
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(
                &execution,
                ProposalKind::RunDone,
                disposition_payload("Archive this chore"),
            ),
        )
        .await,
    );
    let question = state.work_db.list_operator_questions(&id).unwrap().remove(0).question;
    let response = choose(&state, &question.id, "Archive this chore").await;
    let FrontendEvent::WorkItemUpdated {
        item: boss_work_db::WorkItem::Chore(task),
    } = response
    else {
        panic!("expected archived chore: {response:?}");
    };
    assert!(task.pr_url.is_none());
    assert_eq!(task.status, boss_protocol::TaskStatus::Archived);
    assert_eq!(task.last_status_actor, "human");
    assert!(task.operator_question.is_none());
    state.work_db.reconcile_product_executions(&task.product_id).unwrap();
    assert_eq!(state.work_db.list_executions(Some(&id)).unwrap().len(), 1);
    assert_eq!(state.work_db.list_operator_questions(&id).unwrap().len(), 1);
}
