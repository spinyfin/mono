// Prerequisite question answer and dependency redispatch regressions.
use super::*;

fn prerequisites_of(state: &ServerState, task_id: &str) -> Vec<String> {
    state
        .work_db
        .list_dependencies(boss_protocol::ListDependenciesInput {
            work_item: task_id.into(),
            direction: Some(boss_protocol::DependencyDirection::Prereqs),
        })
        .unwrap()
        .prerequisites
        .into_iter()
        .map(|edge| edge.prerequisite_id)
        .collect()
}

fn chore_named(state: &ServerState, product_id: &str, name: &str) -> Vec<boss_protocol::Task> {
    state
        .work_db
        .list_tasks(product_id, None, None, false)
        .unwrap()
        .into_iter()
        .filter(|task| task.name == name)
        .collect()
}

const PREREQUISITE_NAME: &str = "Fix the retention-cleanup fixture race";

#[tokio::test]
async fn prerequisite_question_reads_like_the_yes_no_question_and_carries_the_proposed_task() {
    let (state, _dir, _, task_id) = parked_task_with_question(prerequisite_question_payload()).await;
    let task = question_task(&state, &task_id);
    assert!(task.status == boss_protocol::TaskStatus::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some("awaiting_operator_answer"));
    let question = task.operator_question.unwrap();
    assert_eq!(
        question.text,
        "Create prerequisite task 'Fix the retention-cleanup fixture race'? This task will wait for it."
    );
    assert_eq!(
        question.explanation,
        "CI cannot go green until the fixture is fixed on main."
    );
    assert_eq!(
        question.answer_type,
        boss_protocol::OperatorAnswerType::CreatePrerequisiteTask {
            name: PREREQUISITE_NAME.into(),
            brief: "The shared fixture races retention cleanup. Fix it on main.".into(),
        }
    );
}

#[tokio::test]
async fn prerequisite_yes_creates_chore_edge_and_note_then_redispatches_when_it_completes() {
    let (state, _dir, old_execution, task_id) = parked_task_with_question(prerequisite_question_payload()).await;
    let original = question_task(&state, &task_id);
    let answer = answer_question(&state, &original.short_id.unwrap().to_string(), true).await;
    let FrontendEvent::WorkItemUpdated {
        item: crate::work::WorkItem::Task(task),
    } = answer
    else {
        panic!("expected updated task: {answer:?}");
    };

    // The task left Needs Attention and is parked behind its dependency.
    assert_eq!(task.status, boss_protocol::TaskStatus::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some("dependency"));
    assert!(task.operator_question.is_none());
    assert!(
        task.autostart,
        "the dependency cascade only redispatches an autostart task"
    );
    let history = state.work_db.list_operator_questions(&task_id).unwrap();
    assert_eq!(history[0].status, boss_protocol::OperatorQuestionStatus::Answered);

    // One new chore, in the same product, carrying the proposed brief, with normal dispatch.
    let created = chore_named(&state, &task.product_id, PREREQUISITE_NAME);
    assert_eq!(created.len(), 1);
    let prerequisite = &created[0];
    assert_eq!(prerequisite.kind, boss_protocol::TaskKind::Chore);
    assert_eq!(prerequisite.product_id, task.product_id);
    assert_eq!(
        prerequisite.description,
        "The shared fixture races retention cleanup. Fix it on main."
    );
    assert!(prerequisite.autostart);
    assert_eq!(prerequisite.status, boss_protocol::TaskStatus::Todo);

    // The `blocks` edge: the answered task depends on the new one.
    assert_eq!(prerequisites_of(&state, &task_id), vec![prerequisite.id.clone()]);

    // The brief records the approval.
    let timestamp = chrono::DateTime::from_timestamp(history[0].answered_at.as_ref().unwrap().parse().unwrap(), 0)
        .unwrap()
        .format("%Y-%m-%d %H:%M UTC")
        .to_string();
    assert_eq!(
        task.description,
        format!(
            "Original brief\n\n---\n\n## Operator-approved prerequisite ({timestamp})\n\n\
- **Prerequisite task:** {PREREQUISITE_NAME} (`{}`)\n\
- **Why this task cannot proceed without it:** CI cannot go green until the fixture is fixed on main.\n\
- **Asked by run:** `{old_execution}`\n\n\
The operator approved this on the kanban. The engine created it as a chore with normal dispatch. This task now depends on it (a `blocks` dependency): it stays parked until the prerequisite is done, and the engine then dispatches it again. When you resume, assume the prerequisite's change is on `main`; do not redo its work here.\n",
            prerequisite.id
        )
    );

    // A repeated answer, by either selector, changes nothing.
    for selector in [&history[0].question.id, &task_id] {
        assert!(matches!(
            answer_question(&state, selector, true).await,
            FrontendEvent::WorkItemUpdated { .. }
        ));
    }
    assert_eq!(chore_named(&state, &task.product_id, PREREQUISITE_NAME).len(), 1);
    assert_eq!(prerequisites_of(&state, &task_id), vec![prerequisite.id.clone()]);
    assert_eq!(question_task(&state, &task_id).description, task.description);
    assert!(matches!(answer_question(&state, &task_id, false).await,
        FrontendEvent::OperatorQuestionError { error: boss_protocol::OperatorQuestionError::Conflict {
            state, answer: Some(boss_protocol::OperatorAnswer::YesNo { value: true })
        }} if state == "answered"));

    // While the prerequisite is open the task is not dispatched ...
    let executions_before = state.work_db.list_executions(Some(&task_id)).unwrap();
    assert!(
        executions_before
            .iter()
            .all(|e| e.status != boss_protocol::ExecutionStatus::Ready)
    );

    // ... and when it completes the existing dependency machinery redispatches it.
    state
        .work_db
        .update_work_item(
            &prerequisite.id,
            boss_protocol::WorkItemPatch {
                status: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let redispatched = question_task(&state, &task_id);
    assert_eq!(redispatched.status, boss_protocol::TaskStatus::Todo);
    assert!(
        state
            .work_db
            .list_executions(Some(&task_id))
            .unwrap()
            .iter()
            .any(|e| e.status == boss_protocol::ExecutionStatus::Ready),
        "the prerequisite completing mints a ready execution for the parked task"
    );
}

#[tokio::test]
async fn prerequisite_yes_links_an_equivalent_open_task_instead_of_duplicating_it() {
    let (state, _dir, _, task_id) = parked_task_with_question(prerequisite_question_payload()).await;
    let product_id = question_task(&state, &task_id).product_id;
    // A finished task of the same name does not count; an open one, however
    // it is spelled, does.
    let finished = crate::test_support::create_test_chore(&state.work_db, product_id.clone(), PREREQUISITE_NAME);
    state
        .work_db
        .update_work_item(
            &finished.id,
            boss_protocol::WorkItemPatch {
                status: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let open = state
        .work_db
        .create_chore(
            boss_protocol::CreateChoreInput::builder()
                .product_id(product_id.clone())
                .name("  fix the Retention-Cleanup fixture race ")
                .description("Existing brief")
                .force_duplicate(true)
                .build(),
        )
        .unwrap();

    let FrontendEvent::WorkItemUpdated {
        item: crate::work::WorkItem::Task(task),
    } = answer_question(&state, &task_id, true).await
    else {
        panic!("expected updated task");
    };
    assert_eq!(prerequisites_of(&state, &task_id), vec![open.id.clone()]);
    assert_eq!(
        state
            .work_db
            .list_tasks(&product_id, None, None, false)
            .unwrap()
            .iter()
            .filter(|t| t.name.trim().eq_ignore_ascii_case(PREREQUISITE_NAME))
            .count(),
        2,
        "the finished task and the existing open one; nothing new was created"
    );
    let crate::work::WorkItem::Chore(open_after) = state.work_db.get_work_item(&open.id).unwrap() else {
        panic!("expected chore");
    };
    assert_eq!(open_after.description, "Existing brief");
    assert_eq!(task.blocked_reason.as_deref(), Some("dependency"));
    assert!(
        task.description.contains(
            "An equivalent open task already existed, so the engine linked to it instead of creating a duplicate."
        ),
        "the brief says the existing task was reused: {}",
        task.description
    );
}

#[tokio::test]
async fn prerequisite_yes_enables_dispatch_on_a_backlog_candidate_and_skips_a_human_blocked_one() {
    let (state, _dir, _, task_id) = parked_task_with_question(prerequisite_question_payload()).await;
    let product_id = question_task(&state, &task_id).product_id;
    let blocked = crate::test_support::create_test_chore_manual(&state.work_db, product_id.clone(), PREREQUISITE_NAME);
    state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'worker_failed' WHERE id = ?1",
            [&blocked.id],
        )
        .unwrap();
    let backlog = state
        .work_db
        .create_chore(
            boss_protocol::CreateChoreInput::builder()
                .product_id(product_id.clone())
                .name(PREREQUISITE_NAME)
                .autostart(false)
                .force_duplicate(true)
                .build(),
        )
        .unwrap();
    assert!(!backlog.autostart);

    let FrontendEvent::WorkItemUpdated {
        item: crate::work::WorkItem::Task(task),
    } = answer_question(&state, &task_id, true).await
    else {
        panic!("expected updated task");
    };
    assert_eq!(prerequisites_of(&state, &task_id), vec![backlog.id.clone()]);
    let crate::work::WorkItem::Chore(linked) = state.work_db.get_work_item(&backlog.id).unwrap() else {
        panic!("expected chore");
    };
    assert!(linked.autostart, "the linked backlog task is made dispatchable");
    assert!(
        state
            .work_db
            .list_executions(Some(&backlog.id))
            .unwrap()
            .iter()
            .any(|e| e.status == boss_protocol::ExecutionStatus::Ready)
    );
    assert!(
        task.description
            .contains("turned autostart on and queued it for dispatch")
    );
}

#[tokio::test]
async fn revision_prerequisite_answers_redispatch_only_after_completion() {
    for deduplicate in [false, true] {
        let (state, _dir, asking_execution, revision_id) =
            parked_task_with_question_kind(prerequisite_question_payload(), true).await;
        let revision = question_task(&state, &revision_id);
        let existing = deduplicate.then(|| {
            let chore = crate::test_support::create_test_chore_manual(
                &state.work_db,
                revision.product_id.clone(),
                PREREQUISITE_NAME,
            );
            state
                .work_db
                .update_work_item(
                    &chore.id,
                    boss_protocol::WorkItemPatch {
                        status: Some("in_review".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            chore
        });
        assert!(matches!(
            answer_question(&state, &revision_id, true).await,
            FrontendEvent::WorkItemUpdated { .. }
        ));
        let chores = chore_named(&state, &revision.product_id, PREREQUISITE_NAME);
        assert_eq!(chores.len(), 1);
        let prerequisite = &chores[0];
        if let Some(existing) = existing {
            assert_eq!(prerequisite.id, existing.id);
        }
        assert!(
            state
                .work_db
                .list_executions(Some(&revision_id))
                .unwrap()
                .iter()
                .any(|e| e.id == asking_execution && e.status == boss_protocol::ExecutionStatus::Failed)
        );
        for status in ["in_review", "done"] {
            let parked = question_task(&state, &revision_id);
            assert_eq!(parked.status, boss_protocol::TaskStatus::Blocked);
            assert_eq!(parked.blocked_reason.as_deref(), Some("dependency"));
            assert!(parked.operator_question.is_none());
            assert!(
                state
                    .work_db
                    .list_executions(Some(&revision_id))
                    .unwrap()
                    .iter()
                    .all(|e| e.status != boss_protocol::ExecutionStatus::Ready)
            );
            state
                .work_db
                .update_work_item(
                    &prerequisite.id,
                    boss_protocol::WorkItemPatch {
                        status: Some(status.into()),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        assert_eq!(
            question_task(&state, &revision_id).status,
            boss_protocol::TaskStatus::Todo
        );
        assert!(
            state
                .work_db
                .list_executions(Some(&revision_id))
                .unwrap()
                .iter()
                .any(|e| e.id != asking_execution
                    && e.status == boss_protocol::ExecutionStatus::Ready
                    && e.kind == boss_protocol::ExecutionKind::RevisionImplementation)
        );
    }
}

#[tokio::test]
async fn prerequisite_dedup_skips_manual_gates_and_restarts_terminal_executions() {
    for gated in [false, true] {
        let (state, _dir, _, task_id) = parked_task_with_question(prerequisite_question_payload()).await;
        let product_id = question_task(&state, &task_id).product_id;
        for (deferred, human_driven) in [(true, false), (false, true)] {
            state
                .work_db
                .create_chore(
                    boss_protocol::CreateChoreInput::builder()
                        .product_id(&product_id)
                        .name(PREREQUISITE_NAME)
                        .deferred(deferred)
                        .human_driven(human_driven)
                        .autostart(false)
                        .force_duplicate(true)
                        .build(),
                )
                .unwrap();
        }
        let candidate = state
            .work_db
            .create_chore(
                boss_protocol::CreateChoreInput::builder()
                    .product_id(&product_id)
                    .name(PREREQUISITE_NAME)
                    .autostart(false)
                    .force_duplicate(true)
                    .build(),
            )
            .unwrap();
        let prior = state
            .work_db
            .request_execution(
                boss_protocol::RequestExecutionInput::builder()
                    .work_item_id(&candidate.id)
                    .build(),
            )
            .unwrap();
        state
            .work_db
            .connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'failed' WHERE id = ?1",
                [&prior.id],
            )
            .unwrap();
        let gate = crate::test_support::create_test_chore_manual(&state.work_db, product_id, "Upstream dependency");
        if gated {
            state
                .work_db
                .add_dependency(boss_protocol::AddDependencyInput {
                    dependent: candidate.id.clone(),
                    prerequisite: gate.id.clone(),
                    relation: None,
                })
                .unwrap();
        }
        assert!(matches!(
            answer_question(&state, &task_id, true).await,
            FrontendEvent::WorkItemUpdated { .. }
        ));
        assert_eq!(prerequisites_of(&state, &task_id), vec![candidate.id.clone()]);
        let executions = state.work_db.list_executions(Some(&candidate.id)).unwrap();
        assert!(executions.iter().any(|e| e.id != prior.id
            && e.status
                == if gated {
                    boss_protocol::ExecutionStatus::WaitingDependency
                } else {
                    boss_protocol::ExecutionStatus::Ready
                }));
        if gated {
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
            assert!(
                state
                    .work_db
                    .list_executions(Some(&candidate.id))
                    .unwrap()
                    .iter()
                    .any(|e| e.id != prior.id && e.status == boss_protocol::ExecutionStatus::Ready)
            );
        }
    }
}

/// A prerequisite in the revision's own chain keeps the `in_review` release.
#[tokio::test]
async fn same_chain_revision_prerequisite_satisfies_at_in_review() {
    use crate::test_support::{create_test_chore_manual, create_test_product_with_repo};
    let (state, _dir) = test_server_state_with_fakes();
    let db = &state.work_db;
    let product = create_test_product_with_repo(db, "RevisionPrereq", Some("git@example.com:rev/prereq.git"));
    let root = create_test_chore_manual(db, product.id.clone(), "Chain root chore");
    let set = |id: &str, status: &str| {
        db.update_work_item(
            id,
            boss_protocol::WorkItemPatch {
                status: Some(status.into()),
                ..Default::default()
            },
        )
        .unwrap();
    };
    db.update_work_item(
        &root.id,
        boss_protocol::WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some("https://github.com/example/repo/pull/1".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let revision = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(root.id.as_str())
                .description("Fix failing CI")
                .autostart(false)
                .build(),
            &crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open),
        )
        .unwrap();
    // Same-chain stacking keeps the in_review relaxation.
    let sibling = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(root.id.as_str())
                .description("Address review findings")
                .autostart(false)
                .build(),
            &crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open),
        )
        .unwrap();
    set(&revision.id, "in_review");
    assert!(
        db.gating_prereqs_for(&sibling.id).unwrap().is_empty(),
        "an in_review revision in the same chain does not gate the next writer"
    );
}

#[tokio::test]
async fn prerequisite_no_parks_in_backlog_like_a_yes_no_decline() {
    let (state, _dir, _, task_id) = parked_task_with_question(prerequisite_question_payload()).await;
    let before = question_task(&state, &task_id);
    let tasks_before = state
        .work_db
        .list_tasks(&before.product_id, None, None, false)
        .unwrap()
        .len();
    assert!(matches!(
        answer_question(&state, &task_id, false).await,
        FrontendEvent::WorkItemUpdated { .. }
    ));
    let task = question_task(&state, &task_id);
    assert_eq!(task.status, boss_protocol::TaskStatus::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some("worker_failed"));
    assert!(!task.autostart);
    assert!(task.operator_question.is_none());
    assert_eq!(task.description, "Original brief");
    assert!(task.blocked_detail.unwrap().contains(
        "\"Create prerequisite task 'Fix the retention-cleanup fixture race'? This task will wait for it.\""
    ));
    assert!(prerequisites_of(&state, &task_id).is_empty());
    assert_eq!(
        state
            .work_db
            .list_tasks(&before.product_id, None, None, false)
            .unwrap()
            .len(),
        tasks_before,
        "a No creates nothing"
    );
}

#[tokio::test]
async fn racing_prerequisite_approvals_create_one_task_and_one_edge() {
    let (state, _dir, _, task_id) = parked_task_with_question(prerequisite_question_payload()).await;
    let yes = boss_protocol::OperatorAnswer::YesNo { value: true };
    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| state.work_db.answer_operator_question(&task_id, yes.clone())))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(outcomes.iter().all(Result::is_ok), "{outcomes:?}");
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| o.as_ref().unwrap().prerequisite_work_item_id.is_some())
            .count(),
        1,
        "exactly one racer performed the approval"
    );
    let product_id = question_task(&state, &task_id).product_id;
    assert_eq!(chore_named(&state, &product_id, PREREQUISITE_NAME).len(), 1);
    assert_eq!(prerequisites_of(&state, &task_id).len(), 1);
}
