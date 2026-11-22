//! A held revision stays under AI review throughout its started review cycle.

use boss_protocol::{
    ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket,
    ReviewProfile,
};

use super::*;

const PR_URL: &str = "https://github.com/example/repo/pull/42";

/// An `in_review` chain root with a bound PR and a revision that has finished
/// implementing and is held `active` with the PR URL stamped on it.
struct HeldRevision {
    db: WorkDb,
    root_id: String,
    revision_id: String,
}

fn held_revision(label: &str) -> HeldRevision {
    let db = WorkDb::open(temp_db_path(label)).unwrap();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "revision root");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![root.id, PR_URL],
        )
        .unwrap();
    let checker = FakePrStateChecker::always(PrOpenState::Open).with_head_sha("head-sha");
    let revision = db.create_revision(revision_input(&root.id), &checker).unwrap();
    let implementation = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            rusqlite::params![implementation.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'active', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![revision.id, PR_URL],
        )
        .unwrap();
    HeldRevision {
        db,
        root_id: root.id,
        revision_id: revision.id,
    }
}

/// A pre-merge review batch on the chain root, as the reviewers are launched.
fn start_review_batch(held: &HeldRevision) -> String {
    start_review_batch_for_target(held, "head-sha")
}

fn start_review_batch_for_target(held: &HeldRevision, target_sha: &str) -> String {
    let classification = ReviewClassification::builder()
        .changed_files(vec!["tools/boss/engine/pr-review/src/parsing.rs".to_owned()])
        .complexity_flags(vec![])
        .has_production_code(true)
        .metadata_missing(vec![])
        .production_languages(vec![ReviewLanguageBucket::Rust])
        .profile(ReviewProfile::Light)
        .subsystem_buckets(vec!["tools/boss/engine".to_owned()])
        .additions(12)
        .deletions(3)
        .build();
    let member = ReviewBatchMemberCreateInput::builder()
        .attempt(1)
        .provider_effort("medium")
        .requested_driver("claude")
        .resolved_model("test-model")
        .role(ReviewBatchMemberRole::ClaudeReviewer)
        .status(ReviewBatchMemberStatus::Pending)
        .build();
    let input = ReviewBatchCreateInput::builder()
        .cycle_root_id(held.root_id.clone())
        .base_sha("base-sha")
        .classification(classification)
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(42)
        .pr_url(PR_URL)
        .target_sha(target_sha)
        .build();
    let batch = held.db.create_review_batch(input, &[member]).unwrap().0;
    let execution = held
        .db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(&held.root_id)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let conn = held.db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'orphaned', started_at = '2026-10-31T00:00:00Z' WHERE id = ?1",
        [&execution.id],
    )
    .unwrap();
    conn.execute(
        "UPDATE pr_review_batch_members SET execution_id = ?2 WHERE batch_id = ?1",
        rusqlite::params![batch.id, execution.id],
    )
    .unwrap();
    batch.id
}

#[test]
fn a_held_revision_is_under_review_while_the_roots_batch_is_live() {
    let held = held_revision("revision-ai-review-live-batch");
    assert!(
        held.db.list_revisions_under_ai_review().unwrap().is_empty(),
        "a held revision with no review running is not being worked on"
    );

    let batch_id = start_review_batch(&held);
    assert_eq!(
        held.db.list_revisions_under_ai_review().unwrap(),
        vec![held.revision_id.clone()]
    );

    let projected = projected_revision(&held);
    assert!(
        projected.ai_reviewing,
        "a started orphaned reviewer retains the cycle caption"
    );
    assert_eq!(projected.ai_review_stage.as_deref(), Some("reviewers"));

    // Consolidation and verdict apply are still the review in progress.
    for status in ["supervising", "applying"] {
        held.db
            .connect()
            .unwrap()
            .execute(
                "UPDATE pr_review_batches SET status = ?2 WHERE id = ?1",
                rusqlite::params![batch_id, status],
            )
            .unwrap();
        assert_eq!(
            held.db.list_revisions_under_ai_review().unwrap(),
            vec![held.revision_id.clone()],
            "{status}"
        );
        let projected = projected_revision(&held);
        assert!(projected.ai_reviewing, "{status}");
        assert_eq!(
            projected.ai_review_stage.as_deref(),
            Some(if status == "applying" {
                "applying"
            } else {
                "consolidation"
            })
        );
    }

    // Once the review is consolidated the hold is released and nothing is
    // left to group.
    held.db
        .connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'completed' WHERE id = ?1",
            rusqlite::params![batch_id],
        )
        .unwrap();
    assert!(held.db.list_revisions_under_ai_review().unwrap().is_empty());
}

#[test]
fn a_running_review_execution_on_the_revision_itself_counts() {
    let held = held_revision("revision-ai-review-own-execution");
    let review = held
        .db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(held.revision_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    assert_eq!(
        held.db.list_revisions_under_ai_review().unwrap(),
        vec![held.revision_id.clone()]
    );
    held.db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            rusqlite::params![review.id],
        )
        .unwrap();
    assert!(held.db.list_revisions_under_ai_review().unwrap().is_empty());
}

#[test]
fn a_revision_being_implemented_again_is_not_under_review() {
    let held = held_revision("revision-ai-review-reimplementation");
    start_review_batch(&held);
    held.db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(held.revision_id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    assert!(
        held.db.list_revisions_under_ai_review().unwrap().is_empty(),
        "a new implementation run supersedes the held push"
    );
}

#[test]
fn only_a_held_active_revision_with_the_stamped_pr_is_considered() {
    let held = held_revision("revision-ai-review-not-held");
    start_review_batch(&held);
    let set = |sql: &str| {
        held.db
            .connect()
            .unwrap()
            .execute(sql, rusqlite::params![held.revision_id])
            .unwrap();
    };

    set("UPDATE tasks SET pr_url = NULL WHERE id = ?1");
    assert!(held.db.list_revisions_under_ai_review().unwrap().is_empty());
    set("UPDATE tasks SET pr_url = 'https://github.com/example/repo/pull/42' WHERE id = ?1");
    assert_eq!(held.db.list_revisions_under_ai_review().unwrap().len(), 1);

    // Released by the verdict: folded into the parent, no longer in Doing.
    set("UPDATE tasks SET status = 'in_review' WHERE id = ?1");
    assert!(held.db.list_revisions_under_ai_review().unwrap().is_empty());
}

fn projected_revision(held: &HeldRevision) -> Task {
    match held.db.get_work_item(&held.revision_id).unwrap() {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("unexpected row {other:?}"),
    }
}

#[test]
fn ready_only_reviewers_remain_queued_even_with_a_collecting_batch() {
    let held = held_revision("revision-ai-review-ready-only");
    start_review_batch(&held);
    held.db
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'ready', started_at = NULL WHERE kind = 'pr_review'",
            [],
        )
        .unwrap();
    assert!(held.db.list_revisions_under_ai_review().unwrap().is_empty());
    let projected = projected_revision(&held);
    assert!(!projected.ai_reviewing);
    assert_eq!(projected.ai_review_stage, None);
}

#[test]
fn review_phase_blocked_revisions_are_folded_by_the_engine() {
    let held = held_revision("revision-folded-blocks");
    for reason in [
        "merge_conflict",
        "ci_failure",
        "ci_failure_exhausted",
        "review_feedback",
    ] {
        held.db
            .connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'blocked', blocked_reason = ?2 WHERE id = ?1",
                rusqlite::params![held.revision_id, reason],
            )
            .unwrap();
        assert!(projected_revision(&held).board_folded_revision, "{reason}");
    }
}

#[test]
fn reaped_started_review_stays_reviewing_before_and_after_readmission() {
    let held = held_revision("revision-ai-review-reaped");
    let failed = start_review_batch(&held);
    held.db
        .connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'failed' WHERE id = ?1",
            [&failed],
        )
        .unwrap();
    let assert_reviewing = || {
        assert_eq!(
            held.db.list_revisions_under_ai_review().unwrap(),
            vec![held.revision_id.clone()]
        );
        assert!(projected_revision(&held).ai_reviewing);
        assert_eq!(projected_revision(&held).ai_review_stage.as_deref(), Some("reviewers"));
    };
    assert_reviewing();
    let replacement = start_review_batch_for_target(&held, "replacement-fixture");
    let conn = held.db.connect().unwrap();
    conn.execute(
        "UPDATE pr_review_batches SET generation = 2, target_sha = 'head-sha' WHERE id = ?1",
        [&replacement],
    )
    .unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'ready', started_at = NULL WHERE id IN (
            SELECT execution_id FROM pr_review_batch_members WHERE batch_id = ?1)",
        [&replacement],
    )
    .unwrap();
    drop(conn);
    assert_reviewing();
}

#[test]
fn a_review_started_before_the_latest_implementation_does_not_count() {
    let held = held_revision("revision-ai-review-old-cycle");
    start_review_batch(&held);
    held.db
        .connect()
        .unwrap()
        .execute("UPDATE pr_review_batches SET created_at = '1'", [])
        .unwrap();
    assert!(held.db.list_revisions_under_ai_review().unwrap().is_empty());
    assert!(!projected_revision(&held).ai_reviewing);
}

#[test]
fn a_batch_created_before_the_implementation_is_finalized_still_counts() {
    let held = held_revision("revision-ai-review-finalized-after-batch");
    let batch = start_review_batch(&held);
    let exec = |sql: &str, params: &[&dyn rusqlite::ToSql]| {
        held.db.connect().unwrap().execute(sql, params).unwrap();
    };
    // The implementation started before the push's batch was created and its
    // finish time is written after the batch exists (the production ordering).
    let anchor_implementation = |started_offset: i64, finished_offset: i64| {
        exec(
            "UPDATE work_executions SET
                started_at = CAST((SELECT CAST(created_at AS INTEGER) FROM pr_review_batches WHERE id = ?1) + ?2 AS TEXT),
                finished_at = CAST((SELECT CAST(created_at AS INTEGER) FROM pr_review_batches WHERE id = ?1) + ?3 AS TEXT)
             WHERE kind = 'revision_implementation'",
            &[&batch, &started_offset, &finished_offset],
        );
    };
    anchor_implementation(-10, 5);
    // No reviewer process running: only the batch itself keeps the card active.
    exec(
        "UPDATE work_executions SET status = 'orphaned' WHERE kind = 'pr_review'",
        &[],
    );
    let assert_reviewing = |label: &str, stage: &str| {
        assert_eq!(
            held.db.list_revisions_under_ai_review().unwrap(),
            vec![held.revision_id.clone()],
            "{label}"
        );
        let projected = projected_revision(&held);
        assert!(projected.ai_reviewing, "{label}");
        assert_eq!(projected.ai_review_stage.as_deref(), Some(stage), "{label}");
    };
    for (status, stage) in [
        ("collecting", "reviewers"),
        ("supervising", "consolidation"),
        ("applying", "applying"),
    ] {
        exec(
            "UPDATE pr_review_batches SET status = ?2 WHERE id = ?1",
            &[&batch, &status],
        );
        assert_reviewing(status, stage);
    }
    // A failed generation with a started member stays reviewing until re-admitted.
    exec(
        "UPDATE pr_review_batches SET status = 'failed' WHERE id = ?1",
        &[&batch],
    );
    assert_reviewing("failed", "reviewers");

    // Re-admission: the replacement generation is ready with unstarted reviewers.
    let replacement = start_review_batch_for_target(&held, "replacement-fixture");
    exec(
        "UPDATE pr_review_batches SET generation = 2, target_sha = 'head-sha' WHERE id = ?1",
        &[&replacement],
    );
    exec(
        "UPDATE work_executions SET status = 'ready', started_at = NULL WHERE id IN (
            SELECT execution_id FROM pr_review_batch_members WHERE batch_id = ?1)",
        &[&replacement],
    );
    assert_reviewing("re-admitted", "reviewers");

    // A batch created before the implementation started belongs to an older push.
    anchor_implementation(10, 20);
    assert!(held.db.list_revisions_under_ai_review().unwrap().is_empty());
    assert!(!projected_revision(&held).ai_reviewing);
}

fn task_row(item: WorkItem) -> Task {
    match item {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("unexpected row {other:?}"),
    }
}

/// Every revision, task and chore row, in every status and blocked reason,
/// must agree between the point read and the board's work-tree projection.
#[test]
fn only_review_phase_revision_states_are_folded_by_the_engine() {
    let held = held_revision("revision-fold-negatives");
    let root = task_row(held.db.get_work_item(&held.root_id).unwrap());
    let product_id = root.product_id.clone();
    let project = held
        .db
        .create_project(CreateProjectInput {
            product_id: product_id.clone(),
            name: "fold matrix".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let task = held
        .db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product_id.clone())
                .project_id(project.id)
                .name("fold matrix task")
                .build(),
        )
        .unwrap();
    let chore = create_test_chore_manual(&held.db, product_id.clone(), "fold matrix chore");
    let rows = [
        ("revision", held.revision_id.clone(), true),
        ("task", task.id, false),
        ("chore", chore.id, false),
    ];
    let reasons = [
        None,
        Some("merge_conflict"),
        Some("ci_failure"),
        Some("ci_failure_exhausted"),
        Some("review_feedback"),
        Some("dependency"),
        Some("awaiting_operator_answer"),
        Some("worker_failed"),
    ];
    for status in ["todo", "active", "in_review", "done", "blocked"] {
        for reason in reasons {
            // A blocked reason only exists on blocked rows.
            if status != "blocked" && reason.is_some() {
                continue;
            }
            for (kind, id, is_revision) in &rows {
                held.db
                    .connect()
                    .unwrap()
                    .execute(
                        "UPDATE tasks SET status = ?2, blocked_reason = ?3 WHERE id = ?1",
                        rusqlite::params![id, status, reason],
                    )
                    .unwrap();
                let review_phase_block = matches!(
                    reason,
                    Some("merge_conflict" | "ci_failure" | "ci_failure_exhausted" | "review_feedback")
                );
                let expected = *is_revision
                    && (matches!(status, "in_review" | "done") || (status == "blocked" && review_phase_block));
                let context = format!("{kind} {status} {reason:?}");
                assert_eq!(
                    task_row(held.db.get_work_item(id).unwrap()).board_folded_revision,
                    expected,
                    "get_work_item {context}"
                );
                let tree = held.db.get_work_tree(&product_id).unwrap();
                let projected = tree
                    .chores
                    .iter()
                    .chain(tree.tasks.iter())
                    .find(|row| &row.id == id)
                    .unwrap_or_else(|| panic!("work tree is missing {context}"));
                assert_eq!(projected.board_folded_revision, expected, "work tree {context}");
            }
        }
    }
}
