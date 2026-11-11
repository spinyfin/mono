//! A revision held for review on a PR whose owning row is outside dispatch
//! focus, followed from admission to the verdict that settles its push.
//!
//! The revision's run ended by declaring delivery, so nothing stamped the
//! head it pushed. Every batch here is a real admission whose two reviewers
//! report and whose supervisor's verdict is applied.

use boss_protocol::{DispatchFocus, DispatchFocusTargetKind, RequestExecutionInput};

use super::*;
use crate::work::{PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND, ReviewBatchDispatch};

const REPO: &str = "https://github.com/example/repo";
const HEAD: &str = "head-sha";

/// An `in_review` PR-owning row that is outside the focus set.
fn out_of_focus_root(db: &WorkDb) -> String {
    let product = create_test_product(db);
    let root = create_test_chore_manual(db, product.id, "PR owner");
    bind_open_pr(db, &root.id);
    db.save_dispatch_focus(&focus_elsewhere()).unwrap();
    assert!(!db.work_item_in_dispatch_focus(&root.id, &focus_elsewhere()).unwrap());
    root.id
}

fn focus_elsewhere() -> DispatchFocus {
    let mut focus = DispatchFocus::default();
    focus.insert(DispatchFocusTargetKind::Project, "proj_elsewhere".to_owned());
    focus
}

/// A revision of `root_id`, finished and held `active` with the root's PR on
/// it. `launched` says whether it was started with `bossctl agents launch`.
fn hold_revision(db: &WorkDb, root_id: &str, launched: bool) -> String {
    let checker = FakePrStateChecker::always(PrOpenState::Open).with_head_sha(HEAD);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(root_id)
                .description("test revision ask")
                .build(),
            &checker,
        )
        .unwrap();
    let implementation = if launched {
        db.request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&revision.id)
                .force(true)
                .build(),
        )
        .unwrap()
    } else {
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap()
    };
    // Every run of the revision is over: the one that pushed, and any the
    // engine queued for it on creation.
    raw(db)
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            params![implementation.id],
        )
        .unwrap();
    raw(db)
        .execute(
            "UPDATE work_executions SET status = 'abandoned' WHERE work_item_id = ?1 AND id != ?2",
            params![revision.id, implementation.id],
        )
        .unwrap();
    raw(db)
        .execute(
            "UPDATE tasks SET status = 'active', pr_url = ?2 WHERE id = ?1",
            params![revision.id, PR_URL],
        )
        .unwrap();
    assert_eq!(
        db.work_item_in_dispatch_focus(&revision.id, &focus_elsewhere())
            .unwrap(),
        launched
    );
    revision.id
}

fn held_revision_on_out_of_focus_root(db: &WorkDb, launched: bool) -> (String, String) {
    let root_id = out_of_focus_root(db);
    let revision_id = hold_revision(db, &root_id, launched);
    (root_id, revision_id)
}

/// Admission for the revision's push, as its completion requests it.
fn admit_for_revision(db: &WorkDb, root_id: &str, revision_id: &str) -> ReviewBatchDispatch {
    let mut input = batch_input(root_id.to_owned(), HEAD);
    input.legacy_task_id = Some(revision_id.to_owned());
    db.create_pre_merge_review_batch(input, REPO).unwrap()
}

fn open_deferred_markers(db: &WorkDb) -> Vec<String> {
    db.list_open_attention_items_of_kind(PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)
        .unwrap()
        .into_iter()
        .filter_map(|item| item.work_item_id)
        .collect()
}

/// Both reviewers report, the supervisor returns a clean verdict, and the
/// verdict is applied against a PR still at `HEAD`.
fn review_to_a_clean_verdict(
    db: &WorkDb,
    root_id: &str,
    batch: &boss_protocol::ReviewBatch,
    reviewers: &[crate::work::WorkExecution],
) {
    let batch_id = &batch.id;
    for reviewer in reviewers {
        db.submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &reviewer.id,
            work_item_id: root_id,
            kind: ProposalKind::ReviewReport,
            payload_json: &format!(
                r#"{{"batch_id":"{batch_id}","target_sha":"{HEAD}","report":{{"batch_id":"{batch_id}","pr_url":"{PR_URL}","target_sha":"{HEAD}","phase":"pre_merge","summary":"Clean.","coverage":{{"files_inspected":[],"files_omitted":[],"limitations":[]}},"findings":[]}}}}"#
            ),
            idempotency_key: "report",
        })
        .unwrap()
        .unwrap();
    }
    let supervisor = db
        .review_batch_members(batch_id)
        .unwrap()
        .into_iter()
        .find(|member| member.role == ReviewBatchMemberRole::Supervisor)
        .and_then(|member| member.execution_id)
        .expect("both reports dispatch the supervisor");
    let verdict = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &supervisor,
            work_item_id: root_id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &clean_verdict_payload(batch_id, HEAD),
            idempotency_key: "verdict",
        })
        .unwrap()
        .unwrap();
    apply_verdict(
        db,
        &verdict.proposal.id,
        &FakePrStateChecker::always(PrOpenState::Open).with_head_sha(HEAD),
    )
    .unwrap();
    assert_eq!(
        db.review_batch(batch_id).unwrap().unwrap().status,
        ReviewBatchStatus::Completed
    );
}

/// The launch on the revision carries to the review of what it pushed: the
/// batch is admitted for the revision, and its verdict moves the revision out
/// of `active`.
#[test]
fn a_launched_revisions_review_is_admitted_through_focus_and_releases_it() {
    let (_dir, db) = open_db();
    let (root_id, revision_id) = held_revision_on_out_of_focus_root(&db, true);

    let ReviewBatchDispatch::Created { batch, executions } = admit_for_revision(&db, &root_id, &revision_id) else {
        panic!("the launched revision's review must be admitted through focus");
    };
    assert_eq!(batch.producing_work_item_id.as_deref(), Some(revision_id.as_str()));
    assert_eq!(read_task(&db, &revision_id).status, TaskStatus::Active);

    review_to_a_clean_verdict(&db, &root_id, &batch, &executions);

    assert_eq!(read_task(&db, &revision_id).status, TaskStatus::InReview);
    assert!(open_deferred_markers(&db).is_empty());
}

/// Focus defers the review of a revision that was not launched, and the
/// deferral is marked on the revision. `bossctl review start` then runs the
/// review, admitted against the root. The marker clears when that batch is
/// admitted, and its verdict releases the revision it never named.
#[test]
fn a_review_started_for_the_root_releases_the_revision_that_was_deferred() {
    let (_dir, db) = open_db();
    let (root_id, revision_id) = held_revision_on_out_of_focus_root(&db, false);
    assert!(matches!(
        admit_for_revision(&db, &root_id, &revision_id),
        ReviewBatchDispatch::AdmissionDeferred
    ));
    db.file_admission_deferred_attention(&revision_id, PR_URL).unwrap();
    assert_eq!(open_deferred_markers(&db), vec![revision_id.clone()]);

    let ReviewBatchDispatch::Created { batch, executions } = db
        .request_pre_merge_review_batch_for_pool(batch_input(root_id.clone(), HEAD), 16)
        .unwrap()
    else {
        panic!("an explicit review start must be admitted");
    };
    assert_eq!(
        batch.producing_work_item_id, None,
        "an explicit start is admitted against the row that owns the PR"
    );
    assert!(
        open_deferred_markers(&db).is_empty(),
        "the review is running, so nothing is deferred any more"
    );

    review_to_a_clean_verdict(&db, &root_id, &batch, &executions);

    assert_eq!(read_task(&db, &revision_id).status, TaskStatus::InReview);
    assert!(open_deferred_markers(&db).is_empty());
}

/// Two revisions of one PR are both deferred. The verdict of the one review
/// that covers both releases both.
#[test]
fn a_verdict_releases_every_revision_held_for_its_head() {
    let (_dir, db) = open_db();
    let root_id = out_of_focus_root(&db);
    let revisions = [hold_revision(&db, &root_id, false), hold_revision(&db, &root_id, false)];
    for revision_id in &revisions {
        assert!(matches!(
            admit_for_revision(&db, &root_id, revision_id),
            ReviewBatchDispatch::AdmissionDeferred
        ));
    }

    let ReviewBatchDispatch::Created { batch, executions } = db
        .request_pre_merge_review_batch_for_pool(batch_input(root_id.clone(), HEAD), 16)
        .unwrap()
    else {
        panic!("an explicit review start must be admitted");
    };
    review_to_a_clean_verdict(&db, &root_id, &batch, &executions);

    for revision_id in &revisions {
        assert_eq!(read_task(&db, revision_id).status, TaskStatus::InReview);
    }
}

/// A batch admitted for one head resolves the deferred marker of the revisions
/// awaiting that head only. A revision whose review is deferred at a different
/// head keeps its marker, because its own push is still waiting to be admitted.
#[test]
fn a_batch_leaves_the_marker_of_a_revision_awaiting_another_head() {
    let (_dir, db) = open_db();
    let root_id = out_of_focus_root(&db);
    let matching = hold_revision(&db, &root_id, false);
    let other = hold_revision(&db, &root_id, false);

    assert!(matches!(
        admit_for_revision(&db, &root_id, &matching),
        ReviewBatchDispatch::AdmissionDeferred
    ));
    let mut later = batch_input(root_id.clone(), "later-head-sha");
    later.legacy_task_id = Some(other.clone());
    assert!(matches!(
        db.create_pre_merge_review_batch(later, REPO).unwrap(),
        ReviewBatchDispatch::AdmissionDeferred
    ));
    db.file_admission_deferred_attention(&matching, PR_URL).unwrap();
    db.file_admission_deferred_attention(&other, PR_URL).unwrap();

    let ReviewBatchDispatch::Created { .. } = db
        .request_pre_merge_review_batch_for_pool(batch_input(root_id.clone(), HEAD), 16)
        .unwrap()
    else {
        panic!("an explicit review start must be admitted");
    };

    assert_eq!(
        open_deferred_markers(&db),
        vec![other],
        "only the revision awaiting a different head stays marked"
    );
}
