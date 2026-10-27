use super::*;
use boss_protocol::{TaskKind, TaskStatus};
use rusqlite::{OptionalExtension, params};

fn fixture(
    kind: &TaskKind,
    status: &TaskStatus,
    deleted: bool,
) -> (Arc<ServerState>, tempfile::TempDir, String, String) {
    let (state, dir, execution, mut task) = live_chore_execution();
    if *kind == TaskKind::Revision {
        state.work_db.connect().unwrap().execute(
            "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/example/repo/pull/1' WHERE id = ?1",
            [&task],
        )
        .unwrap();
        let revision = state
            .work_db
            .create_revision(
                boss_protocol::CreateRevisionInput::builder()
                    .parent_task_id(&task)
                    .description("Original brief")
                    .autostart(false)
                    .build(),
                &crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open),
            )
            .unwrap();
        if matches!(status, TaskStatus::Blocked | TaskStatus::Done | TaskStatus::Archived) {
            state
                .work_db
                .connect()
                .unwrap()
                .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&task])
                .unwrap();
        }
        task = revision.id;
        state
            .work_db
            .connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET work_item_id = ?2, kind = 'revision_implementation' WHERE id = ?1",
                params![execution, task],
            )
            .unwrap();
    } else if matches!(
        kind,
        TaskKind::Design | TaskKind::DesignPostmortem | TaskKind::ProjectTask | TaskKind::Task
    ) {
        let product: String = state
            .work_db
            .connect()
            .unwrap()
            .query_row("SELECT product_id FROM tasks WHERE id = ?1", [&task], |row| row.get(0))
            .unwrap();
        let project = state
            .work_db
            .create_project(
                boss_protocol::CreateProjectInput::builder()
                    .product_id(product)
                    .name("Question project")
                    .no_design_task(true)
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        state
            .work_db
            .connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET project_id = ?2 WHERE id = ?1",
                params![task, project.id],
            )
            .unwrap();
    }
    state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET kind = ?2, status = ?3,
         pr_url = CASE WHEN ?3 = 'in_review' THEN 'https://github.com/example/repo/pull/1' ELSE NULL END,
         blocked_reason = CASE WHEN ?3 = 'blocked' THEN 'ci_failed' ELSE NULL END,
         blocked_detail = CASE WHEN ?3 = 'blocked' THEN 'Existing blocker' ELSE NULL END,
         deleted_at = CASE WHEN ?4 THEN '123' ELSE NULL END WHERE id = ?1",
            params![task, kind.as_str(), status.as_str(), deleted],
        )
        .unwrap();
    (state, dir, execution, task)
}

/// All protocol kinds/statuses, all answer types and both deletion states:
/// live parents expose a card that can be answered and submitted; deleted
/// parents retain history without creating an actionable attention.
#[tokio::test]
async fn blocked_question_always_has_an_answerable_surface() {
    for kind in TaskKind::ALL {
        for status in TaskStatus::ALL {
            for (payload, answer) in [
                (operator_question_payload(), "yes"),
                (operator_question_payload(), "no"),
                (multiple_choice::multiple_choice_payload(), "Gradual"),
                (prerequisite_question_payload(), "yes"),
                (prerequisite_question_payload(), "no"),
            ] {
                for deleted in [false, true] {
                    exercise(kind, status, payload.clone(), answer, deleted).await;
                }
            }
        }
    }
}

async fn exercise(kind: &TaskKind, status: &TaskStatus, payload: Value, answer: &str, deleted: bool) {
    let (state, _dir, execution, task) = fixture(kind, status, deleted);
    let prerequisite = payload["question"]["answer_type"]["kind"] == "create_prerequisite_task";
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&execution, ProposalKind::RunDone, payload.clone()),
        )
        .await,
    );
    let questions = state.work_db.list_operator_questions(&task).unwrap();
    assert_eq!(questions.len(), 1, "{kind:?}/{status:?}/{deleted}");
    let open = questions[0].status == boss_protocol::OperatorQuestionStatus::Open;
    assert_eq!(
        open,
        !deleted && matches!(status, TaskStatus::Todo | TaskStatus::Active)
    );
    let attention: Option<String> = state
        .work_db
        .connect()
        .unwrap()
        .query_row(
            "SELECT a.id FROM attentions a JOIN attention_groups g ON g.id = a.group_id
         WHERE g.source_task_id = ?1 AND g.kind = 'question' AND g.state = 'open'",
            [&task],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    if deleted {
        assert!(attention.is_none());
        return;
    }
    if open {
        assert!(attention.is_none());
        let typed_answer = if answer == "Gradual" {
            boss_protocol::OperatorAnswer::MultipleChoice { value: answer.into() }
        } else {
            boss_protocol::OperatorAnswer::YesNo { value: answer == "yes" }
        };
        state.work_db.answer_operator_question(&task, typed_answer).unwrap();
        assert_eq!(
            state.work_db.list_operator_questions(&task).unwrap()[0].status,
            boss_protocol::OperatorQuestionStatus::Answered
        );
        return;
    }
    let id = attention.expect("unparkable question must be actionable");
    let group = state
        .work_db
        .answer_attention(&id, Some(answer.into()), false, false)
        .unwrap();
    let members = state.work_db.list_attentions_for_group(&group.id).unwrap();
    assert_eq!(members.len(), 1);
    let prompt = members[0].prompt_text.as_deref().unwrap();
    assert!(prompt.contains(payload["question"]["explanation"].as_str().unwrap()));
    if prerequisite {
        assert!(prompt.contains("Fix the retention-cleanup fixture race"));
        assert!(prompt.contains("The shared fixture races retention cleanup. Fix it on main."));
    }
    let result = state
        .work_db
        .action_attention_group(
            &group.id,
            false,
            &Default::default(),
            &crate::work::StaticPrStateChecker(crate::work::PrOpenState::Open),
        )
        .unwrap();
    assert_eq!(result.group.state, "actioned");
    assert_eq!(result.produced_work_item_ids.len(), 1);
    let produced = &result.produced_work_item_ids[0];
    let (description, produced_kind): (String, String) = state
        .work_db
        .connect()
        .unwrap()
        .query_row("SELECT description, kind FROM tasks WHERE id = ?1", [produced], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert!(description.contains(&format!("**Answer:** {answer}")));
    assert_eq!(
        produced_kind,
        if *status == TaskStatus::InReview {
            "revision"
        } else if group.association_project_id.is_some() {
            "design"
        } else {
            "chore"
        }
    );
    let deps = state
        .work_db
        .list_dependencies(boss_protocol::ListDependenciesInput {
            work_item: produced.clone(),
            direction: None,
        })
        .unwrap();
    if prerequisite && answer == "yes" {
        assert_eq!(deps.prerequisites.len(), 1);
        let (name, brief): (String, String) = state
            .work_db
            .connect()
            .unwrap()
            .query_row(
                "SELECT name, description FROM tasks WHERE id = ?1",
                [&deps.prerequisites[0].prerequisite_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(name, "Fix the retention-cleanup fixture race");
        assert_eq!(brief, "The shared fixture races retention cleanup. Fix it on main.");
    } else {
        assert!(deps.prerequisites.is_empty());
    }
    let actual: String = state
        .work_db
        .connect()
        .unwrap()
        .query_row("SELECT status FROM tasks WHERE id = ?1", [&task], |row| row.get(0))
        .unwrap();
    assert_eq!(actual, status.as_str());
    if *status == TaskStatus::Blocked {
        let detail: String = state
            .work_db
            .connect()
            .unwrap()
            .query_row("SELECT blocked_detail FROM tasks WHERE id = ?1", [&task], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(detail, "Existing blocker");
    }
    assert!(
        state
            .work_db
            .record_worker_failure(&execution, "duplicate completion")
            .unwrap()
            .is_none()
    );
    assert_eq!(state.work_db.list_attentions_for_group(&group.id).unwrap().len(), 1);
}
