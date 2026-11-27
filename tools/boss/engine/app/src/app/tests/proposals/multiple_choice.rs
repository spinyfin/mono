// Multiple-choice questions: a blocked worker offers two to six options, one
// is chosen, and the engine records exactly that string.
use super::*;

pub(super) fn multiple_choice_payload() -> Value {
    json!({"outcome":"blocked", "summary":"Two rollouts fit the brief", "question":{
        "text":"Which rollout should I use?",
        "answer_type":{
            "kind":"multiple_choice",
            "choices":["Gradual", "All at once", "Behind a flag"],
            "options":[
                {"label":"Gradual", "disposition":"resume"},
                {"label":"All at once", "disposition":"resume"},
                {"label":"Behind a flag", "disposition":"resume"}
            ]
        },
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
        boss_protocol::OperatorAnswerType::multiple_choice(vec![
            boss_protocol::OperatorChoiceOption::resume("Gradual"),
            boss_protocol::OperatorChoiceOption::resume("All at once"),
            boss_protocol::OperatorChoiceOption::resume("Behind a flag"),
        ])
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
async fn a_multiple_choice_proposal_with_bad_or_non_executable_options_is_rejected_at_submission() {
    let (state, _dir, execution_id, task_id) = live_chore_execution();
    for bad_answer_type in [
        // Missing structured `options` (choices only).
        json!({"kind":"multiple_choice", "choices":["a", "b"]}),
        // Missing `disposition` on an option.
        json!({"kind":"multiple_choice", "options":[{"label":"a"}, {"label":"b", "disposition":"resume"}]}),
        // Non-executable dispositions outside the closed engine set.
        json!({"kind":"multiple_choice", "options":[{"label":"a", "disposition":"merge"}, {"label":"b", "disposition":"resume"}]}),
        json!({"kind":"multiple_choice", "options":[{"label":"a", "disposition":"reap_worker"}, {"label":"b", "disposition":"resume"}]}),
        json!({"kind":"multiple_choice", "options":[{"label":"a", "disposition":"change_priority"}, {"label":"b", "disposition":"resume"}]}),
        // Non-executable combination: archive cannot carry a resume addendum.
        json!({"kind":"multiple_choice", "options":[
            {"label":"Chore is obsolete, archive it", "disposition":"archive", "addendum":"continue running"},
            {"label":"Retry", "disposition":"resume"}
        ]}),
        // Bad option counts or labels.
        json!({"kind":"multiple_choice", "options":[{"label":"only one", "disposition":"resume"}]}),
        json!({"kind":"multiple_choice", "options":[{"label":"a", "disposition":"resume"}, {"label":"a", "disposition":"archive"}]}),
        json!({"kind":"multiple_choice", "options":[{"label":"a", "disposition":"resume"}, {"label":"", "disposition":"resume"}]}),
    ] {
        let mut payload = multiple_choice_payload();
        payload["question"]["answer_type"] = bad_answer_type.clone();
        let response = call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&execution_id, ProposalKind::RunDone, payload),
        )
        .await;
        assert!(
            matches!(response, FrontendEvent::ProposalRejected { .. }),
            "expected rejection for {bad_answer_type}: {response:?}"
        );
    }

    // Even past wire validation, card creation in WorkDb rejects archive/park options on an already-terminal item.
    state
        .work_db
        .connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'archived' WHERE id = ?1", [&task_id])
        .unwrap();
    let (proposal, _) = submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(
                &execution_id,
                ProposalKind::RunDone,
                disposition_payload("Chore is obsolete, archive it", "archive"),
            ),
        )
        .await,
    );
    assert_eq!(proposal.state, boss_protocol::ProposalState::Rejected);
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

fn disposition_payload(choice: &str, disposition: &str) -> Value {
    let mut payload = multiple_choice_payload();
    payload["question"]["answer_type"] = json!({
        "kind": "multiple_choice",
        "choices": [choice, "Pick a PR", "Keep trying"],
        "options": [
            {"label": choice, "disposition": disposition},
            {"label": "Pick a PR", "disposition": "resume"},
            {"label": "Keep trying", "disposition": "resume"}
        ]
    });
    payload
}

#[tokio::test]
async fn owner_archive_choices_archive_once_for_any_label_without_a_new_worker_or_question() {
    for choice in [
        "Chore is obsolete, archive it",
        "No longer needed — drop it",
        "Archive",
        "Archive this chore",
    ] {
        let (state, _dir, _, task_id) = parked_task_with_question(disposition_payload(choice, "archive")).await;
        let question = question_task(&state, &task_id).operator_question.unwrap();
        for _ in 0..2 {
            assert!(matches!(
                choose(&state, &question.id, choice).await,
                FrontendEvent::WorkItemUpdated { .. }
            ));
            let task = question_task(&state, &task_id);
            assert_eq!(task.status, boss_protocol::TaskStatus::Archived, "{choice}");
            assert_eq!(task.last_status_actor, "human");
            assert_eq!(task.archived_by.as_deref(), Some("manual_status_change"));
            assert!(task.operator_question.is_none());
            assert_eq!(state.work_db.list_executions(Some(&task_id)).unwrap().len(), 1);
            assert!(
                state
                    .work_db
                    .list_attention_items_for_work_item(&task_id)
                    .unwrap()
                    .is_empty(),
                "archiving must raise no attention item"
            );
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
    for (revision, label) in [(false, "Hold off until next week"), (true, "Park this item")] {
        let (state, _dir, _, task_id) =
            parked_task_with_question_kind(disposition_payload(label, "park"), revision).await;
        let task = question_task(&state, &task_id);
        let root_id = task.parent_task_id.clone().unwrap_or_else(|| task_id.clone());
        let question = task.operator_question.unwrap();
        assert!(matches!(
            choose(&state, &question.id, label).await,
            FrontendEvent::WorkItemUpdated { .. }
        ));
        let task = question_task(&state, &task_id);
        assert_eq!(task.parked_by.as_deref(), Some("human"));
        assert!(state.work_db.is_parked(&task_id).unwrap());
        assert!(state.work_db.is_parked(&root_id).unwrap());
        assert!(task.operator_question.is_none());
        assert_eq!(task.status, boss_protocol::TaskStatus::Todo);
        assert!(task.autostart, "unpark must restore dispatch eligibility");
        assert!(task.description.contains(&format!("Operator chose:** {label}")));
        assert_eq!(open_continuations(&state, &task_id), 0, "nothing launches while parked");
        // A repeated answer neither mints nor launches anything.
        assert!(matches!(
            choose(&state, &question.id, label).await,
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
    let (state, _dir, _, task_id) = parked_task_with_question(disposition_payload("Put on ice", "park")).await;
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
        choose(&state, &question.id, "Put on ice").await,
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
async fn resume_disposition_ignores_archive_or_park_label_wording_and_carries_addendum() {
    // Even if a label literally says "Archive this item" or "Park this item",
    // a `resume` disposition must resume rather than archiving or parking:
    // label text never drives behaviour.
    for choice in ["Archive this item", "Park this item", "Archive the old logs"] {
        let mut payload = disposition_payload(choice, "resume");
        payload["question"]["answer_type"]["options"][0]["addendum"] =
            json!("Move old log files to cold storage only.");
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
        assert!(
            task.description
                .contains("- **Option addendum:** Move old log files to cold storage only.\n")
        );
        assert_eq!(state.work_db.list_executions(Some(&task_id)).unwrap().len(), 2);
    }
}

#[tokio::test]
async fn failed_archive_rolls_back_the_answer_so_the_owner_can_retry() {
    let (state, _dir, _, task_id) =
        parked_task_with_question(disposition_payload("Chore is obsolete, archive it", "archive")).await;
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
        choose(&state, &question.id, "Chore is obsolete, archive it").await,
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
        choose(&state, &question.id, "Chore is obsolete, archive it").await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    assert_eq!(
        question_task(&state, &task_id).status,
        boss_protocol::TaskStatus::Archived
    );
}

#[tokio::test]
async fn structured_dispositions_chosen_by_a_non_owner_resume_the_worker_instead_of_acting() {
    for (actor, choice, disposition) in [
        ("worker", "Chore is obsolete, archive it", "archive"),
        ("worker", "Hold off for now", "park"),
        ("boss", "Chore is obsolete, archive it", "archive"),
        ("boss", "Hold off for now", "park"),
    ] {
        let (state, _dir, _, task_id) = parked_task_with_question(disposition_payload(choice, disposition)).await;
        let question = question_task(&state, &task_id).operator_question.unwrap();
        let pid = std::process::id() as libc::pid_t;
        if actor == "worker" {
            state.worker_registry.register(pid, "other_live_worker");
        } else {
            state.set_boss_pid(pid);
        }
        assert!(matches!(
            call_with_peer(
                &state,
                Some(pid),
                FrontendRequest::AnswerOperatorQuestion {
                    suppress_start: false,
                    id: question.id.clone(),
                    answer: boss_protocol::OperatorAnswer::MultipleChoice { value: choice.into() },
                },
            )
            .await,
            FrontendEvent::WorkItemUpdated { .. }
        ));
        let task = question_task(&state, &task_id);
        assert_eq!(task.status, boss_protocol::TaskStatus::Todo, "{actor} {choice}");
        assert!(task.parked_at.is_none(), "{actor} {choice} must not park");
        assert!(task.description.contains(&format!("Operator chose:** {choice}")));
    }
}

#[tokio::test]
async fn owner_archive_choice_archives_a_revision() {
    let (state, _dir, _, task_id) =
        parked_task_with_question_kind(disposition_payload("Revision is moot, archive it", "archive"), true).await;
    let question = question_task(&state, &task_id).operator_question.unwrap();
    assert!(matches!(
        choose(&state, &question.id, "Revision is moot, archive it").await,
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

/// Regression test for an unbound "Rebase onto main to clear Trunk merge
/// conflict" chore whose worker failed, followed by a progression run offering
/// "Chore is obsolete, archive it" (`disposition: archive`). Choosing it as the
/// repo owner archives the chore immediately, releases the worker's pane and
/// workspace lease, and raises no follow-up worker, progression run, or
/// attention.
#[tokio::test]
async fn progression_archive_option_with_freeform_label_archives_unbound_chore_and_releases_resources() {
    let (state, _dir, worker_execution, id) = live_chore_execution();
    prepare_workspace_teardown(&state, &worker_execution);
    state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET name = 'Rebase onto main to clear Trunk merge conflict' WHERE id = ?1",
            [&id],
        )
        .unwrap();
    // Simulate the worker failing while its pane/lease still need teardown on archive,
    // then a progression run raising the multiple-choice card.
    state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'failed', finished_at = strftime('%s','now') WHERE id = ?1",
            [&worker_execution],
        )
        .unwrap();
    state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'worker_failed', autostart = 0 WHERE id = ?1",
            [&id],
        )
        .unwrap();
    let prog = state
        .work_db
        .create_execution(
            boss_protocol::CreateExecutionInput::builder()
                .work_item_id(&id)
                .kind(boss_protocol::ExecutionKind::Progression)
                .status(boss_protocol::ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    state
        .worker_registry
        .register(std::process::id() as libc::pid_t, prog.id.clone());
    let mut payload = disposition_payload("Chore is obsolete, archive it", "archive");
    payload["criterion"] = json!("1");
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&prog.id, ProposalKind::RunDone, payload),
        )
        .await,
    );
    state.work_db.finish_progression_declared(&prog.id).unwrap();

    let question = state.work_db.list_operator_questions(&id).unwrap().remove(0).question;
    let response = choose(&state, &question.id, "Chore is obsolete, archive it").await;
    let FrontendEvent::WorkItemUpdated {
        item: boss_work_db::WorkItem::Chore(task),
    } = response
    else {
        panic!("expected archived chore: {response:?}");
    };
    assert!(task.pr_url.is_none());
    assert_eq!(task.status, boss_protocol::TaskStatus::Archived);
    assert_eq!(task.last_status_actor, "human");
    assert_eq!(task.archived_by.as_deref(), Some("manual_status_change"));
    assert!(task.operator_question.is_none());

    // Let the spawned force_release task complete (which runs subprocess-backed
    // occupancy/preservation checks under full shard concurrency) and verify
    // both the worker pane slot and workspace lease are released.
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            if state.worker_registry.slot_for_run(&worker_execution).is_none()
                && state
                    .work_db
                    .get_execution(&worker_execution)
                    .unwrap()
                    .cube_lease_id
                    .is_none()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("spawned force_release must release worker pane slot and cube workspace lease");

    // Neither execution reconciliation nor progression nominates the archived chore or raises new cards.
    state.work_db.reconcile_product_executions(&task.product_id).unwrap();
    let mint = state.work_db.mint_progression_executions().unwrap();
    assert!(mint.minted.is_empty() && mint.asked.is_empty());
    assert_eq!(state.work_db.list_executions(Some(&id)).unwrap().len(), 2);
    assert_eq!(state.work_db.list_operator_questions(&id).unwrap().len(), 1);
    assert!(
        state
            .work_db
            .list_attention_items_for_work_item(&id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn unparkable_worker_question_attention_archives_on_owner_action_and_not_for_non_owner() {
    for (actor, expect_archived) in [("human", true), ("worker", false), ("boss", false)] {
        let (state, _dir, execution_id, task_id) = live_chore_execution();
        state
            .work_db
            .connect()
            .unwrap()
            .execute("UPDATE tasks SET status = 'in_review' WHERE id = ?1", [&task_id])
            .unwrap();
        submitted(
            call_with_peer(
                &state,
                Some(std::process::id() as libc::pid_t),
                submit_request(
                    &execution_id,
                    ProposalKind::RunDone,
                    disposition_payload("Chore is obsolete, archive it", "archive"),
                ),
            )
            .await,
        );
        state
            .work_db
            .record_worker_awaiting_operator_answer(
                &execution_id,
                "blocked on decision",
                &state
                    .work_db
                    .execution_operator_question(&execution_id)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        let (attn_id, group_id): (String, String) = state
            .work_db
            .connect()
            .unwrap()
            .query_row(
                "SELECT a.id, g.id FROM attentions a
                 JOIN attention_groups g ON g.id = a.group_id
                 WHERE g.association_task_id = ?1 AND g.state = 'open'",
                [&task_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        state
            .work_db
            .answer_attention(&attn_id, Some("Chore is obsolete, archive it".into()), false, false)
            .unwrap();

        let sink = make_session_sink();
        let pid = std::process::id() as libc::pid_t;
        let peer_pid = match actor {
            "human" => None,
            "worker" => {
                state.worker_registry.register(pid, "other_live_worker");
                Some(pid)
            }
            "boss" => {
                state.worker_registry.unregister(pid);
                state.set_boss_pid(pid);
                Some(pid)
            }
            _ => unreachable!(),
        };
        crate::app::attentions::handle_action_attention_group(
            dispatch_with_peer(&state, &sink, peer_pid),
            FrontendRequest::ActionAttentionGroup {
                id: group_id.clone(),
                skip_unanswered: false,
                member_overrides: Default::default(),
            },
        )
        .await;
        sink.close();
        let reply = sink.next().await.expect("handler must reply").payload;
        assert!(
            matches!(reply, FrontendEvent::AttentionGroupActioned { .. }),
            "{reply:?}"
        );

        let (boss_work_db::WorkItem::Chore(task) | boss_work_db::WorkItem::Task(task)) =
            state.work_db.get_work_item(&task_id).unwrap()
        else {
            panic!("expected task or chore");
        };
        let all_tasks = state.work_db.list_tasks(&task.product_id, None, None, true).unwrap();
        if expect_archived {
            assert_eq!(task.status, boss_protocol::TaskStatus::Archived);
            assert_eq!(task.last_status_actor, "human");
            assert_eq!(task.archived_by.as_deref(), Some("manual_status_change"));
            assert_eq!(all_tasks.len(), 1, "owner archive must not spawn a follow-up chore");
        } else {
            assert_eq!(task.status, boss_protocol::TaskStatus::InReview);
            assert_eq!(all_tasks.len(), 2, "non-owner action resumes via a follow-up item");
        }
    }
}
