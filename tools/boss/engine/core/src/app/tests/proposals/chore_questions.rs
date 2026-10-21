// Operator question cards for chore-like work items: a chore or followup
// that stops blocked on a human decision parks on the same card route a task
// does.
use super::*;
use rusqlite::OptionalExtension;

/// Every persisted kind and status retains an answerable operator surface.
#[tokio::test]
async fn blocked_question_always_has_an_answerable_surface() {
    for kind in ["task", "project_task", "chore", "followup"] {
        for status in ["todo", "active", "blocked", "in_review", "done", "archived"] {
            for answer in ["yes", "no"] {
                let (state, _dir, execution_id, task_id) = live_chore_execution();
                state
                    .work_db
                    .connect()
                    .unwrap()
                    .execute(
                        "UPDATE tasks SET kind = ?2, status = ?3,
                     blocked_reason = CASE WHEN ?3 = 'blocked' THEN 'ci_failed' ELSE NULL END,
                     blocked_detail = CASE WHEN ?3 = 'blocked' THEN 'Existing blocker' ELSE NULL END
                     WHERE id = ?1",
                        rusqlite::params![task_id, kind, status],
                    )
                    .unwrap();
                submitted(
                    call_with_peer(
                        &state,
                        Some(std::process::id() as libc::pid_t),
                        submit_request(&execution_id, ProposalKind::RunDone, operator_question_payload()),
                    )
                    .await,
                );
                let questions = state.work_db.list_operator_questions(&task_id).unwrap();
                assert_eq!(questions.len(), 1, "{kind}/{status}");
                let open = questions[0].status == boss_protocol::OperatorQuestionStatus::Open;
                assert_eq!(open, matches!(status, "todo" | "active"), "{kind}/{status}");
                let attention_id: Option<String> = state
                    .work_db
                    .connect()
                    .unwrap()
                    .query_row(
                        "SELECT a.id FROM attentions a JOIN attention_groups g ON g.id = a.group_id
                     WHERE g.association_task_id = ?1 AND g.kind = 'question'
                     AND g.state = 'open' AND a.answer_state = 'open'
                     AND a.question_type = 'yes_no'",
                        [&task_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .unwrap();
                if open {
                    assert!(attention_id.is_none());
                } else {
                    let id = attention_id.expect("unparkable question must be answerable in attentions");
                    let group = state
                        .work_db
                        .answer_attention(&id, Some(answer.to_owned()), false, false)
                        .unwrap();
                    let members = state.work_db.list_attentions_for_group(&group.id).unwrap();
                    assert_eq!(members.len(), 1);
                    assert_eq!(members[0].answer.as_deref(), Some(answer));
                    let prompt = members[0].prompt_text.as_deref().unwrap();
                    assert!(prompt.contains("Approve raising the file limit"));
                    assert!(prompt.contains("All 48 files are needed"));
                    let actual: String = state
                        .work_db
                        .connect()
                        .unwrap()
                        .query_row("SELECT status FROM tasks WHERE id = ?1", [&task_id], |row| row.get(0))
                        .unwrap();
                    assert_eq!(actual, status, "{kind}/{status}");
                    if status == "blocked" {
                        let detail: String = state
                            .work_db
                            .connect()
                            .unwrap()
                            .query_row("SELECT blocked_detail FROM tasks WHERE id = ?1", [&task_id], |row| {
                                row.get(0)
                            })
                            .unwrap();
                        assert_eq!(detail, "Existing blocker");
                    }
                    assert!(
                        state
                            .work_db
                            .record_worker_failure(&execution_id, "duplicate completion")
                            .unwrap()
                            .is_none()
                    );
                    assert_eq!(state.work_db.list_attentions_for_group(&group.id).unwrap().len(), 1);
                }
            }
        }
    }
}

/// A chore or followup worker that stops blocked on a human decision parks
/// on a question card exactly like a task does: an `open` question row, the
/// `awaiting_operator_answer` reason (never `worker_failed`), and the question
/// text as the blocked badge's detail. The question is wired through the same
/// `insert_question` / `record_worker_awaiting_operator_answer` route the task
/// card uses — nothing is scraped out of the summary.
#[tokio::test]
async fn question_on_chore_or_followup_parks_with_an_open_question_card() {
    for kind in ["chore", "followup"] {
        let (state, _dir, execution_id, task_id) = live_chore_execution();
        state
            .work_db
            .connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET kind = ?2 WHERE id = ?1",
                rusqlite::params![task_id, kind],
            )
            .unwrap();
        submitted(
            call_with_peer(
                &state,
                Some(std::process::id() as libc::pid_t),
                submit_request(&execution_id, ProposalKind::RunDone, operator_question_payload()),
            )
            .await,
        );
        let crate::work::WorkItem::Chore(chore) = state.work_db.get_work_item(&task_id).unwrap() else {
            panic!("expected a chore-like item for kind {kind}");
        };
        assert_eq!(chore.status, boss_protocol::TaskStatus::Blocked, "{kind}");
        assert_eq!(
            chore.blocked_reason.as_deref(),
            Some("awaiting_operator_answer"),
            "{kind} must wait on the human, not read as a worker failure"
        );
        assert_eq!(
            chore.blocked_detail.as_deref(),
            Some("Approve raising the file limit from 30 to 48?"),
            "{kind}"
        );
        assert!(!chore.autostart, "{kind}");
        let question = chore
            .operator_question
            .as_ref()
            .unwrap_or_else(|| panic!("{kind} must project its open question onto the card"));
        assert_eq!(question.execution_id, execution_id);
        assert_eq!(question.text, "Approve raising the file limit from 30 to 48?");
        assert_eq!(
            question.explanation,
            "All 48 files are needed for the requested migration."
        );
        assert_eq!(question.run_summary.as_deref(), Some("The scope needs authorization"));
        let history = state.work_db.list_operator_questions(&task_id).unwrap();
        assert_eq!(history.len(), 1, "{kind}");
        assert_eq!(history[0].status, boss_protocol::OperatorQuestionStatus::Open, "{kind}");
        let execution = state.work_db.get_execution(&execution_id).unwrap();
        assert_eq!(execution.status, boss_protocol::ExecutionStatus::Failed);
        assert_eq!(execution.preferred_workspace_id.as_deref(), Some("workspace-run-done"));
        // No duplicate attention item: the question card is the one surface.
        assert!(
            state
                .work_db
                .list_attention_items_for_work_item(&task_id)
                .unwrap()
                .is_empty()
        );
        // Parked, not auto-redispatched: only the answer may start another attempt.
        state.work_db.reconcile_product_executions(&chore.product_id).unwrap();
        assert_eq!(
            state.work_db.list_executions(Some(&task_id)).unwrap().len(),
            1,
            "{kind}"
        );
    }
}

/// Yes on a chore's question card works like Yes on a task's: the dated
/// authorization is appended to the chore's brief, the chore goes back to
/// `todo` with autostart, and one ready execution is minted that prefers the
/// asking run's preserved workspace. No converts the park into the plain
/// `worker_failed` shape with the declined question recorded.
#[tokio::test]
async fn question_on_chore_answers_like_a_task() {
    let (state, _dir, old_execution, chore_id) = live_chore_execution();
    state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET description = 'Original brief' WHERE id = ?1",
            [&chore_id],
        )
        .unwrap();
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&old_execution, ProposalKind::RunDone, operator_question_payload()),
        )
        .await,
    );
    let answer = answer_question(&state, &chore_id, true).await;
    let FrontendEvent::WorkItemUpdated {
        item: crate::work::WorkItem::Chore(chore),
    } = answer
    else {
        panic!("expected updated chore: {answer:?}");
    };
    assert_eq!(chore.status, boss_protocol::TaskStatus::Todo);
    assert!(chore.autostart);
    assert_eq!(chore.last_status_actor, "human");
    assert!(chore.blocked_reason.is_none());
    assert!(chore.blocked_detail.is_none());
    assert!(chore.operator_question.is_none());
    assert!(
        chore
            .description
            .starts_with("Original brief\n\n---\n\n## Repo owner approval (")
    );
    assert!(
        chore
            .description
            .contains("- **Question the previous worker asked:** Approve raising the file limit from 30 to 48?\n")
    );
    assert!(
        chore
            .description
            .contains(&format!("- **Asked by run:** `{old_execution}`\n"))
    );
    let history = state.work_db.list_operator_questions(&chore_id).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, boss_protocol::OperatorQuestionStatus::Answered);
    let executions = state.work_db.list_executions(Some(&chore_id)).unwrap();
    assert_eq!(executions.len(), 2);
    let next = executions
        .iter()
        .find(|execution| execution.id != old_execution)
        .unwrap();
    assert_eq!(next.status, boss_protocol::ExecutionStatus::Ready);
    assert_eq!(next.preferred_workspace_id.as_deref(), Some("workspace-run-done"));

    // No on a fresh chore: the park becomes the ordinary declined shape.
    let (state, _dir, execution_id, chore_id) = live_chore_execution();
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&execution_id, ProposalKind::RunDone, operator_question_payload()),
        )
        .await,
    );
    let answer = answer_question(&state, &chore_id, false).await;
    let FrontendEvent::WorkItemUpdated {
        item: crate::work::WorkItem::Chore(chore),
    } = answer
    else {
        panic!("expected updated chore: {answer:?}");
    };
    assert_eq!(chore.status, boss_protocol::TaskStatus::Blocked);
    assert_eq!(chore.blocked_reason.as_deref(), Some("worker_failed"));
    let detail = chore.blocked_detail.unwrap();
    assert!(detail.contains("Operator declined on"));
    assert!(detail.contains("Approve raising the file limit from 30 to 48?"));
    assert!(chore.operator_question.is_none());
    assert_eq!(state.work_db.list_executions(Some(&chore_id)).unwrap().len(), 1);
}

/// A task already `in_review` cannot park: its question is recorded as
/// withdrawn (`task_not_parkable`) and its status remains `in_review`.
#[tokio::test]
async fn question_on_unparkable_task_withdraws() {
    let (state, _dir, execution_id, task_id) = live_chore_execution();
    state
        .work_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET kind = 'project_task', status = 'in_review' WHERE id = ?1",
            [&task_id],
        )
        .unwrap();
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&execution_id, ProposalKind::RunDone, operator_question_payload()),
        )
        .await,
    );
    let crate::work::WorkItem::Task(task) = state.work_db.get_work_item(&task_id).unwrap() else {
        panic!("expected task");
    };
    assert_eq!(task.status, boss_protocol::TaskStatus::InReview);
    assert!(task.operator_question.is_none());
    let record = state.work_db.list_operator_questions(&task_id).unwrap().remove(0);
    assert_eq!(record.withdrawn_reason.as_deref(), Some("task_not_parkable"));
    assert_eq!(
        state.work_db.get_execution(&execution_id).unwrap().status,
        boss_protocol::ExecutionStatus::Failed
    );
}
