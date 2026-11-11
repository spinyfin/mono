use super::*;

use boss_protocol::{CreateRevisionInput, RequestExecutionInput};

use crate::test_support::{
    create_test_chore_manual, create_test_product, open_db, seed_daily_automation, seed_review_guide_series_for_pr,
};
use crate::work::{FakePrStateChecker, PrOpenState};

const PARENT_PR: &str = "https://github.com/spinyfin/mono/pull/3356";

fn requested_execution(db: &WorkDb, work_item_id: &str) -> WorkExecution {
    db.request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
        .unwrap()
}

/// The same execution relabelled as if it had been dispatched as `kind`
/// against `work_item_id`.
fn relabelled(execution: &WorkExecution, kind: ExecutionKind, work_item_id: &str) -> WorkExecution {
    WorkExecution {
        kind,
        work_item_id: work_item_id.to_owned(),
        ..execution.clone()
    }
}

fn set_pr(db: &WorkDb, task_id: &str, pr_url: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?1 WHERE id = ?2",
            rusqlite::params![pr_url, task_id],
        )
        .unwrap();
}

/// A run's label is the work item's id, title and PR, found through the
/// durable binding alone — nothing about the live worker is consulted.
#[test]
fn implementation_run_keeps_work_item_title_short_id_and_pr() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Fix fencer scraping");
    let execution = requested_execution(&db, &chore.id);
    set_pr(&db, &chore.id, PARENT_PR);

    let labels = resolve_execution_labels(&db, std::slice::from_ref(&execution.id));

    assert_eq!(labels.len(), 1);
    let label = &labels[0];
    assert_eq!(label.execution_id, execution.id);
    assert_eq!(label.status, ExecutionLabelStatus::Resolved);
    assert_eq!(label.work_item_id.as_deref(), Some(chore.id.as_str()));
    assert_eq!(label.work_item_title.as_deref(), Some("Fix fencer scraping"));
    assert_eq!(label.work_item_short_id, chore.short_id);
    assert!(chore.short_id.is_some());
    assert_eq!(label.product_id.as_deref(), Some(product.id.as_str()));
    assert_eq!(label.pr_url.as_deref(), Some(PARENT_PR));
    assert_eq!(label.purpose, None, "a plain implementation run needs no purpose");
}

/// A revision row has no PR of its own; the label takes the PR the execution
/// is bound to and says the run was a revision.
#[test]
fn revision_run_keeps_its_title_and_the_bound_pr() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let parent = create_test_chore_manual(&db, product.id.clone(), "Parent chore");
    set_pr(&db, &parent.id, PARENT_PR);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent.id.clone())
                .description("Address review findings")
                .build(),
            &FakePrStateChecker::always(PrOpenState::Open),
        )
        .unwrap();
    assert!(revision.pr_url.is_none());
    let execution = WorkExecution {
        kind: ExecutionKind::RevisionImplementation,
        pr_url: Some(PARENT_PR.to_owned()),
        ..requested_execution(&db, &revision.id)
    };

    let label = label_for_execution(&db, &execution);

    assert_eq!(label.status, ExecutionLabelStatus::Resolved);
    assert_eq!(label.work_item_id.as_deref(), Some(revision.id.as_str()));
    assert_eq!(label.work_item_short_id, revision.short_id);
    assert!(label.work_item_title.as_deref().is_some_and(|title| !title.is_empty()));
    assert_eq!(label.pr_url.as_deref(), Some(PARENT_PR));
    assert_eq!(label.purpose.as_deref(), Some("Revision"));
}

#[test]
fn review_run_keeps_the_reviewed_work_item_and_says_review() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Reviewed change");
    let requested = requested_execution(&db, &chore.id);
    set_pr(&db, &chore.id, PARENT_PR);
    let execution = relabelled(&requested, ExecutionKind::PrReview, &chore.id);

    let label = label_for_execution(&db, &execution);

    assert_eq!(label.status, ExecutionLabelStatus::Resolved);
    assert_eq!(label.work_item_title.as_deref(), Some("Reviewed change"));
    assert_eq!(label.work_item_short_id, chore.short_id);
    assert_eq!(label.pr_url.as_deref(), Some(PARENT_PR));
    assert_eq!(label.purpose.as_deref(), Some("Review"));
    assert_eq!(label.kind.as_deref(), Some("pr_review"));
}

/// A review-guide run points at a comparison, not a task; the label is the
/// task the guide explains.
#[test]
fn review_guide_run_resolves_the_comparison_to_its_root_task() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id.clone(), "Guided change");
    let requested = requested_execution(&db, &root.id);
    set_pr(&db, &root.id, PARENT_PR);
    let (_series, comparison) = seed_review_guide_series_for_pr(&db, &root.id, PARENT_PR, "base", "head");
    assert!(comparison.starts_with("prgc_"), "got {comparison}");
    let execution = relabelled(&requested, ExecutionKind::PrReviewGuide, &comparison);

    let label = label_for_execution(&db, &execution);

    assert_eq!(label.status, ExecutionLabelStatus::Resolved);
    assert_eq!(label.work_item_id.as_deref(), Some(root.id.as_str()));
    assert_eq!(label.work_item_title.as_deref(), Some("Guided change"));
    assert_eq!(label.work_item_short_id, root.short_id);
    assert_eq!(label.pr_url.as_deref(), Some(PARENT_PR));
    assert_eq!(label.purpose.as_deref(), Some("Review guide"));
    assert_eq!(label.product_name.as_deref(), Some(product.name.as_str()));
    assert_eq!(label.project_name, None);
}

/// A deleted work item is its own status, carries no title or id, and never
/// echoes the raw work-item id back as a name.
#[test]
fn deleted_work_item_resolves_to_its_own_status_without_any_raw_id() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Soon deleted");
    let execution = requested_execution(&db, &chore.id);
    db.delete_work_item(&chore.id).unwrap();

    let labels = resolve_execution_labels(&db, std::slice::from_ref(&execution.id));

    let label = &labels[0];
    assert_eq!(label.status, ExecutionLabelStatus::WorkItemDeleted);
    assert_eq!(label.work_item_title, None);
    assert_eq!(label.work_item_id, None);
    assert_eq!(label.work_item_short_id, None);
    let json = serde_json::to_string(label).unwrap();
    assert!(!json.contains(&chore.id), "label leaked the work-item id: {json}");
}

#[test]
fn kinds_without_a_work_item_are_not_reported_as_deleted() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "x");
    let automation = seed_daily_automation(&db, &product.id);
    let base = requested_execution(&db, &chore.id);

    let triage = relabelled(&base, ExecutionKind::AutomationTriage, &automation.id);
    let answer = relabelled(&base, ExecutionKind::AnswerAgent, "cmt_not_a_work_item");

    let triage_label = label_for_execution(&db, &triage);
    assert_eq!(triage_label.status, ExecutionLabelStatus::NoWorkItem);
    assert_eq!(triage_label.purpose.as_deref(), Some("Automation triage"));
    let answer_label = label_for_execution(&db, &answer);
    assert_eq!(answer_label.status, ExecutionLabelStatus::NoWorkItem);
    assert_eq!(answer_label.purpose.as_deref(), Some("Answer"));
}

#[test]
fn unknown_execution_resolves_to_its_own_status_and_keeps_request_order() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Known");
    let known = requested_execution(&db, &chore.id);

    let labels = resolve_execution_labels(&db, &["exec_missing".to_owned(), known.id.clone()]);

    assert_eq!(labels.len(), 2);
    assert_eq!(labels[0].execution_id, "exec_missing");
    assert_eq!(labels[0].status, ExecutionLabelStatus::ExecutionUnknown);
    assert_eq!(labels[1].execution_id, known.id);
    assert_eq!(labels[1].status, ExecutionLabelStatus::Resolved);
}

#[test]
fn every_execution_kind_has_a_purpose_decision() {
    let plain = [ExecutionKind::ChoreImplementation, ExecutionKind::TaskImplementation];
    for kind in plain {
        assert_eq!(purpose(&kind), None, "{kind}");
    }
    for kind in [
        ExecutionKind::AnswerAgent,
        ExecutionKind::AutomationTriage,
        ExecutionKind::CiRemediation,
        ExecutionKind::ConflictResolution,
        ExecutionKind::InvestigationImplementation,
        ExecutionKind::PrReview,
        ExecutionKind::PrReviewGuide,
        ExecutionKind::ProductDesign,
        ExecutionKind::ProjectDesign,
        ExecutionKind::Progression,
        ExecutionKind::RevisionImplementation,
    ] {
        assert!(purpose(&kind).is_some(), "{kind}");
    }
}
