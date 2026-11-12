use super::*;
use crate::test_support::*;
use boss_protocol::WorkItem;

#[test]
fn approval_text_avoids_internal_vocabulary() {
    let question = boss_protocol::OperatorQuestionView::builder()
        .id("question-1")
        .text("Include the additional files?")
        .answer_type(boss_protocol::OperatorAnswerType::YesNo)
        .explanation("The fix requires both files.")
        .asked_at("2026-10-23")
        .execution_id("exec_example")
        .build();
    let text = super::authorization_block(&question, "2026-10-23");
    crate::prose_vocabulary::assert_no_forbidden_prose("recorded approval", &text);
    assert!(text.contains("## Repo owner approval (2026-10-23)"));
}

#[test]
fn file_count_bypass_approval_text_is_scoped_and_avoids_internal_vocabulary() {
    let question = boss_protocol::OperatorQuestionView::builder()
        .id("question-1")
        .text("Split this 48-file change (limit 30) by landing 'Extract the first PR' first, or keep it in one PR?")
        .answer_type(boss_protocol::OperatorAnswerType::CreatePrerequisiteTask {
            name: "Extract the first PR".into(),
            brief: "Land the first capability as its own PR.".into(),
            file_count_bypass: Some(boss_protocol::FileCountBypassSpec { files: 48, limit: 30 }),
            option_labels: Some(boss_protocol::AnswerOptionLabels::split_or_bypass()),
        })
        .explanation("The change is 48 files against a limit of 30.")
        .asked_at("2026-10-23")
        .execution_id("exec_example")
        .build();
    let text = super::file_count_bypass_authorization_block(
        &question,
        boss_protocol::FileCountBypassSpec { files: 48, limit: 30 },
        "2026-10-23",
    );
    crate::prose_vocabulary::assert_no_forbidden_prose("file-count bypass approval", &text);
    assert!(text.contains("## Repo owner approval (2026-10-23)"));
    assert!(text.contains("change/file-count for this PR only, up to 48 files (repository limit 30)"));
    assert!(text.contains("Checkleft does not enforce the approved count"));
    assert!(text.contains("ask again if it is over"));
    assert!(text.contains("- **Answer:** Bypass limit — keep this change in one PR"));
    assert!(text.contains("BYPASS_CHANGE_FILE_COUNT=Split-vs-bypass question answered Bypass limit by the repo owner on 2026-10-23; one-PR exception for this PR only"));
    assert!(text.contains("It does not authorize bypassing any other check"));
}

#[test]
fn declined_answer_names_the_option_that_was_chosen() {
    let question = boss_protocol::OperatorQuestionView::builder()
        .id("question-1")
        .text("Split this 48-file change?")
        .answer_type(boss_protocol::OperatorAnswerType::CreatePrerequisiteTask {
            name: "Extract the first PR".into(),
            brief: "Land the first capability.".into(),
            file_count_bypass: None,
            option_labels: Some(boss_protocol::AnswerOptionLabels::split_or_bypass()),
        })
        .explanation("Context:\nToo big.\nSplit:\nLand the first PR.\nBypass limit:\nKeep one PR.\nRecommendation:\nBypass limit")
        .asked_at("2026-10-23")
        .execution_id("exec_example")
        .build();
    let text = super::declined_block(&question, "2026-10-23");
    assert!(text.contains("- **Answer:** Bypass limit\n"));
    assert!(text.contains("- **The worker's Bypass limit path:** Keep one PR.\n"));
}

#[test]
fn declined_answer_text_grants_no_authorization() {
    let question = boss_protocol::OperatorQuestionView::builder()
        .id("question-1")
        .text("Ship the smaller retry change?")
        .answer_type(boss_protocol::OperatorAnswerType::YesNo)
        .explanation(
            "Context:\nRetries fail open.\nYes:\nKeep the extra telemetry.\nNo:\nShip the smaller retry change instead.\nRecommendation:\nNo, to finish sooner.",
        )
        .asked_at("2026-10-23")
        .execution_id("exec_example")
        .build();
    let text = super::declined_block(&question, "2026-10-23");
    crate::prose_vocabulary::assert_no_forbidden_prose("declined answer", &text);
    assert!(text.contains("## Recorded answer (2026-10-23)"));
    assert!(text.contains("- **Answer:** No\n"));
    assert!(text.contains("- **The worker's No path:** Ship the smaller retry change instead.\n"));
    assert!(text.contains("It grants no authorization"));
    assert!(text.contains("does not authorize bypassing any check"));
}

/// A task whose `review_cycle` is a known non-zero value must serialise
/// that real value through every mapper path, not the hardcoded `0`
/// `map_task` used to return regardless of the stored column. Exercises
/// both `get_work_item` (the `boss task show --json` path, via
/// `map_task_with_parent_provenance_and_archived_reason`) and the plain
/// `map_task` base reached by `list_tasks_for_product`.
#[test]
fn review_cycle_and_last_reviewed_sha_survive_through_task_show() {
    let (_tmp, db) = open_db();
    let product = create_test_product(&db);
    let task = create_test_chore(&db, product.id.clone(), "review cycle task");

    db.increment_task_review_cycle(&task.id, Some("deadbeef")).unwrap();
    db.increment_task_review_cycle(&task.id, Some("cafef00d")).unwrap();

    let item = db.get_work_item(&task.id).unwrap();
    let WorkItem::Chore(shown) = item else {
        panic!("expected a chore, got {item:?}");
    };
    assert_eq!(shown.review_cycle, 2);
    assert_eq!(shown.last_reviewed_sha.as_deref(), Some("cafef00d"));

    let listed = db.list_tasks(&product.id, None, None, false).unwrap();
    let listed_task = listed.iter().find(|t| t.id == task.id).unwrap();
    assert_eq!(listed_task.review_cycle, 2);
    assert_eq!(listed_task.last_reviewed_sha.as_deref(), Some("cafef00d"));
}

#[test]
fn migration_survives_reopen_and_enforces_one_open_question() {
    let (dir, db) = crate::test_support::open_db();
    let conn = db.connect().unwrap();
    let trigger_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master
             WHERE type = 'trigger' AND name = 'operator_questions_task_override')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(trigger_exists, "fresh schema must retain the task override trigger");
    for id in ["oq_first", "oq_second"] {
        let result = conn.execute(
            "INSERT INTO operator_questions
             (id, work_item_id, execution_id, proposal_id, question_json, status, created_at)
             VALUES (?1, 'task_example', 'exec_example', 'prop_example', '{}', 'open', '1')",
            [id],
        );
        if id == "oq_first" {
            result.unwrap();
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("operator_questions.work_item_id")
            );
        }
    }
    withdraw_questions(&conn, "task_example", "status_edited").unwrap();
    conn.execute(
        "INSERT INTO operator_questions
         (id, work_item_id, execution_id, proposal_id, question_json, status, created_at)
         VALUES ('oq_second', 'task_example', 'exec_example', 'prop_example', '{}', 'open', '2')",
        [],
    )
    .unwrap();
    drop(conn);
    drop(db);
    let reopened = WorkDb::open(dir.path().join("state.db")).unwrap();
    let conn = reopened.connect().unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM operator_questions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
    let indexes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name IN
         ('operator_questions_one_open_per_item', 'operator_questions_by_item')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(indexes, 2);
}
