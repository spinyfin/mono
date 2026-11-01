//! Work-item lifecycle transitions driven through the services above storage:
//! a review-findings revision converted at parent close carries its origin
//! provenance into the worker's backlink prefix, a no-op completion on a task
//! with a bound PR keeps it in review and files the admission-deferred
//! attention atomically, and a revision whose bookmarks cannot be recovered
//! raises one attention item on the PR owner.

use boss_protocol::{
    CREATED_VIA_PR_REVIEW_PREFIX, CreateExecutionInput, CreateRevisionInput, ExecutionKind, ExecutionStatus,
    FinishExecutionRunInput, TaskKind, TaskStatus, WorkItem,
};
use rusqlite::params;

use super::{executions_for, in_review_chore, insert_revision_row, raw, task_status};
use crate::test_support::{
    create_test_chore_manual, create_test_product_named, create_test_product_with_repo, open_db,
};
use crate::work::{FakePrStateChecker, PrOpenState, WorkDb};

const FOLLOWUP_PR_URL: &str = "https://github.com/spinyfin/mono/pull/1537";

/// A revision whose `created_via` starts with `"pr_review:"` must be
/// converted to a `followup` (not a plain `chore`) when the parent PR merges.
/// The followup must carry origin provenance and the rewritten description.
#[test]
fn pr_review_revision_creates_followup_with_correct_kind_and_provenance() {
    let (_dir, db) = open_db();
    let product_id = create_test_product_named(&db, "Boss-fu-create").id;
    let pr_url = FOLLOWUP_PR_URL;
    let parent_id = in_review_chore(&db, &product_id, pr_url);

    // Verify the parent has a short_id so provenance can be recorded.
    let parent_task = db.get_work_item(&parent_id).unwrap();
    let parent_short_id = match &parent_task {
        WorkItem::Chore(t) | WorkItem::Task(t) => t.short_id,
        other => panic!("unexpected variant: {other:?}"),
    };
    assert!(parent_short_id.is_some(), "parent must have a short_id for provenance");

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent_id.clone())
                .description("Address ALL findings before finalising this revision.")
                .created_via(format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_test_123"))
                .build(),
            &checker,
        )
        .unwrap();

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    // The revision is the one materialisation owned by this review
    // execution. Parent-close conversion changes that row in place rather
    // than archiving it and minting a second work item.
    let rev_after = match db.get_work_item(&revision.id).expect("revision row must still exist") {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("unexpected variant: {other:?}"),
    };
    assert_eq!(rev_after.kind, TaskKind::Followup);
    assert_eq!(rev_after.status, TaskStatus::Todo);
    assert!(rev_after.deleted_at.is_none());

    // A followup must be created (not a plain chore).
    let chores = db.list_chores(&product_id, None, false).unwrap();
    let followup = chores
        .iter()
        .find(|c| c.id == revision.id && c.kind == TaskKind::Followup);
    assert!(
        followup.is_some(),
        "a followup task must be created for a pr_review revision; chores: {chores:?}",
    );
    let followup = followup.unwrap();
    assert_eq!(
        followup.created_via,
        format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_test_123")
    );

    let materialisation_count = db
        .list_tasks(&product_id, None, None, true)
        .unwrap()
        .iter()
        .filter(|task| task.created_via == followup.created_via)
        .count();
    assert_eq!(
        materialisation_count, 1,
        "revision creation and parent-close conversion must share one execution-keyed row"
    );

    // No plain chore must be created (only the followup).
    let plain_chore = chores.iter().find(|c| c.id != parent_id && c.kind == TaskKind::Chore);
    assert!(
        plain_chore.is_none(),
        "no plain chore should exist alongside the followup; chores: {chores:?}",
    );

    // Provenance: origin_task_short_id must match the chain-root (parent chore).
    assert_eq!(
        followup.origin_task_short_id, parent_short_id,
        "followup must carry the parent's short_id as origin_task_short_id",
    );

    // Provenance: origin_pr_number must be extracted from the parent pr_url (1537).
    assert_eq!(
        followup.origin_pr_number,
        Some(1537),
        "followup must carry the PR number from the parent's pr_url",
    );

    // Description: old wording replaced.
    assert!(
        !followup.description.contains("finalising this revision"),
        "followup description must not contain 'finalising this revision'",
    );
    assert!(
        followup.description.contains("closing this follow-up"),
        "followup description must contain 'closing this follow-up'",
    );

    // Derivation must survive conversion: the revision's `pr_review:` prefix
    // is stamped onto the followup (not overwritten with `engine_auto`), so
    // dispatch emits the human "review findings" kind label.
    assert!(
        followup.created_via.starts_with(CREATED_VIA_PR_REVIEW_PREFIX),
        "followup must carry the revision's pr_review: created_via; got {:?}",
        followup.created_via,
    );
    let prefix = crate::runner::work_item::followup_pr_backlink(
        &followup.kind,
        &followup.created_via,
        followup.origin_pr_number,
        "git@github.com:spinyfin/mono.git",
    )
    .expect("followup with origin PR and github remote must yield a body prefix")
    .expect("followup must have a provenance body prefix");
    assert_eq!(
        prefix,
        "## Boss follow-up\n\nThis `review findings` follow-up derives from [the origin PR](https://github.com/spinyfin/mono/pull/1537).",
        "chain-helpers conversion must yield the complete review-findings provenance prefix"
    );
}

fn make_waiting_human_chore(db: &WorkDb, label: &str) -> (String, String, String) {
    let product = create_test_product_with_repo(db, &format!("Prod-{label}"), Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore_manual(db, product.id.clone(), format!("Chore-{label}"));
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:foo/bar.git")
                .build(),
        )
        .unwrap();
    let (exec, run) = db
        .start_execution_run(&exec.id, "agent-1", "repo-1", "lease-1", "ws-1", "/workspaces/ws-1")
        .unwrap();
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&exec.id)
            .run_id(&run.id)
            .execution_status(ExecutionStatus::WaitingHuman)
            .run_status("completed")
            .build(),
    )
    .unwrap();
    (product.id, chore.id, exec.id)
}

#[test]
fn no_op_with_bound_pr_retains_review_and_files_attention_atomically() {
    let (_dir, db) = open_db();
    let (_, chore_id, exec_id) = make_waiting_human_chore(&db, "bound-pr-noop");
    let pr_url = "https://github.com/spinyfin/mono/pull/4042";
    raw(&db)
        .execute("UPDATE tasks SET pr_url = ?2 WHERE id = ?1", params![chore_id, pr_url])
        .unwrap();
    crate::completion::file_admission_deferred_attention(&db, &chore_id, pr_url);
    let completion = db
        .record_worker_no_op_completion(&exec_id, "nothing new", None)
        .unwrap()
        .unwrap();
    let WorkItem::Chore(task) = completion.work_item else {
        panic!("expected chore")
    };
    assert_eq!(task.status, TaskStatus::InReview);
    assert_eq!(task.pr_url.as_deref(), Some(pr_url));
    assert!(task.completed_at.is_none());
    assert_eq!(completion.execution.status, ExecutionStatus::Completed);
    let attention = completion.filed_attention_item.expect("contradiction must be visible");
    assert_eq!(attention.kind, "completion_with_bound_pr");
    assert!(attention.body_markdown.contains(pr_url));
    assert_eq!(db.list_attention_items(&exec_id).unwrap().len(), 1);
    assert!(
        db.list_tasks_awaiting_pre_merge_review_admission()
            .unwrap()
            .iter()
            .any(|candidate| candidate.task_id == chore_id)
    );
    assert!(
        db.record_worker_no_op_completion(&exec_id, "replayed", None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn unrecoverable_revision_raises_attention_on_the_pr_owner() {
    let (_dir, db) = open_db();
    let product = create_test_product_named(&db, "Boss-attention").id;
    let root = in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/42");
    let revision = insert_revision_row(&db, &product, &root);
    db.reconcile_product_executions(&product).unwrap();
    let execution = db.get_execution(&executions_for(&db, &revision)[0].0).unwrap();
    crate::execution_bookmark_recovery::report_chain_block(&db, &execution, "GitHub unavailable").unwrap();
    crate::execution_bookmark_recovery::report_chain_block(&db, &execution, "GitHub unavailable").unwrap();
    let attention = db
        .list_open_attention_items_of_kind(crate::execution_bookmark_recovery::CHAIN_BLOCKED)
        .unwrap();
    assert_eq!(attention.len(), 1);
    assert_eq!(attention[0].work_item_id.as_deref(), Some(root.as_str()));
    assert!(attention[0].body_markdown.contains("Later revisions"));
    assert!(attention[0].body_markdown.contains("GitHub unavailable"));
    let original_id = attention[0].id.clone();
    let next = insert_revision_row(&db, &product, &root);
    let next_execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(next.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::WaitingDependency)
                .build(),
        )
        .unwrap();
    crate::execution_bookmark_recovery::report_chain_block(&db, &next_execution, "PR head deleted").unwrap();
    let attention = db
        .list_open_attention_items_of_kind(crate::execution_bookmark_recovery::CHAIN_BLOCKED)
        .unwrap();
    assert_eq!(attention.len(), 1);
    assert_eq!(attention[0].id, original_id);
    assert!(attention[0].body_markdown.contains(&next));
    assert!(attention[0].body_markdown.contains(&next_execution.id));
    assert!(attention[0].body_markdown.contains("PR head deleted"));
    assert!(!attention[0].body_markdown.contains(&execution.id));
    assert!(!attention[0].body_markdown.contains("GitHub unavailable"));
    assert_eq!(task_status(&db, &revision), "todo");
}
