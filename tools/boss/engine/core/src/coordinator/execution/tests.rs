use super::pr_number_for_workspace_goto;
use crate::test_support::{create_active_chore, create_product, open_db, seed_review_guide_series};
use crate::work::{CreateCommentInput, WorkItemPatch};
use boss_protocol::{CommentAnchor, ExecutionKind, ExecutionStatus, WorkExecution, WorkItem};

#[test]
fn answer_agent_on_open_guide_pr_positions_on_that_pr() {
    let (_dir, db) = open_db();
    let root = create_active_chore(&db, &create_product(&db), "impl");
    db.update_work_item(
        &root,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some("https://github.com/acme/widget/pull/9".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let (series, comparison) = seed_review_guide_series(&db, &root);
    let attempt = db
        .create_pr_review_guide_attempt(&series, &comparison, "review-guide-v1")
        .unwrap();
    let crate::work::PublishReviewGuideOutcome::Published(version) = db
        .publish_pr_review_guide_version(&attempt.id, "# Guide\n\nquote", "raw")
        .unwrap()
    else {
        panic!("expected published guide")
    };
    let comment = db
        .create_comment_with_guide_version(
            CreateCommentInput::builder()
                .artifact_kind("pr_review_guide")
                .artifact_id(series)
                .anchor(CommentAnchor {
                    exact: "quote".into(),
                    ..Default::default()
                })
                .body("why retry?")
                .author("user:test")
                .doc_version("hash")
                .plain_text_projection_version(1)
                .build(),
            Some(&version.id),
        )
        .unwrap();
    let execution = WorkExecution::builder()
        .id("exec_answer")
        .work_item_id(comment.id)
        .kind(ExecutionKind::AnswerAgent)
        .status(ExecutionStatus::Ready)
        .repo_remote_url("https://github.com/acme/widget")
        .created_at("2026-01-01T00:00:00Z")
        .build();
    let work_item = db.get_work_item(&root).unwrap();
    assert_eq!(pr_number_for_workspace_goto(&db, &execution, &work_item), Some(9));
}

#[test]
fn answer_agent_on_merged_guide_pr_does_not_position() {
    let (_dir, db) = open_db();
    let root = create_active_chore(&db, &create_product(&db), "impl");
    db.update_work_item(
        &root,
        WorkItemPatch {
            status: Some("done".to_owned()),
            pr_url: Some("https://github.com/acme/widget/pull/9".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let (series, comparison) = seed_review_guide_series(&db, &root);
    let attempt = db
        .create_pr_review_guide_attempt(&series, &comparison, "review-guide-v1")
        .unwrap();
    let crate::work::PublishReviewGuideOutcome::Published(version) = db
        .publish_pr_review_guide_version(&attempt.id, "# Guide\n\nquote", "raw")
        .unwrap()
    else {
        panic!("expected published guide")
    };
    let comment = db
        .create_comment_with_guide_version(
            CreateCommentInput::builder()
                .artifact_kind("pr_review_guide")
                .artifact_id(series)
                .anchor(CommentAnchor {
                    exact: "quote".into(),
                    ..Default::default()
                })
                .body("why retry?")
                .author("user:test")
                .doc_version("hash")
                .plain_text_projection_version(1)
                .build(),
            Some(&version.id),
        )
        .unwrap();
    let execution = WorkExecution::builder()
        .id("exec_answer_merged")
        .work_item_id(comment.id)
        .kind(ExecutionKind::AnswerAgent)
        .status(ExecutionStatus::Ready)
        .repo_remote_url("https://github.com/acme/widget")
        .created_at("2026-01-01T00:00:00Z")
        .build();
    let work_item = db.get_work_item(&root).unwrap();
    assert_eq!(
        pr_number_for_workspace_goto(&db, &execution, &work_item),
        None,
        "a merged PR's target is never Open, so goto must not be attempted"
    );
}

#[test]
fn answer_agent_on_document_does_not_position_on_a_pr() {
    let (_dir, db) = open_db();
    let execution = WorkExecution::builder()
        .id("exec_doc")
        .work_item_id("cmt_missing")
        .kind(ExecutionKind::AnswerAgent)
        .status(ExecutionStatus::Ready)
        .repo_remote_url("https://github.com/acme/widget")
        .created_at("2026-01-01T00:00:00Z")
        .build();
    let work_item = WorkItem::Chore(
        boss_protocol::Task::builder()
            .id("task_x")
            .product_id("prod")
            .kind(boss_protocol::TaskKind::Chore)
            .name("x")
            .description("x")
            .status(boss_protocol::TaskStatus::Active)
            .created_at("2026-01-01T00:00:00Z")
            .updated_at("2026-01-01T00:00:00Z")
            .build(),
    );
    assert_eq!(pr_number_for_workspace_goto(&db, &execution, &work_item), None);
}
