//! Direct coverage for [`crate::completion::run_done_declaration::audit_declared_delivery`],
//! the post-hoc (off the termination path) check that a `delivered` run_done
//! declaration's bound PR actually moved. The RPC-level tests in
//! `app/tests/proposals.rs` cover submit-time terminalization, but none of
//! them drive this specific background task — every test execution there
//! carries an empty `pr_head_before`. These tests drive the snapshot and
//! comparison directly with a [`StubBranchVerifier`].

use super::*;
use crate::completion::run_done_declaration::audit_declared_delivery;

const PR_URL: &str = "https://github.com/spinyfin/mono/pull/42";

#[tokio::test]
async fn audit_flags_attention_when_pr_head_is_unchanged() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    db.set_execution_pr_head_before(&execution_id, "sha_before").unwrap();
    let execution = db.get_execution(&execution_id).unwrap();
    let publisher = Arc::new(RecordingPublisher::default());
    let verifier = StubBranchVerifier::ok("boss/test");
    verifier.set_fresh_head_oid(Ok("sha_before".into())).await;

    audit_declared_delivery(
        verifier.as_ref(),
        &db,
        publisher.as_ref(),
        &execution_id,
        &chore_id,
        &execution.repo_remote_url,
        "sha_before",
        PR_URL,
    )
    .await;

    let items = db.list_attention_items(&execution_id).unwrap();
    assert!(
        items
            .iter()
            .any(|i| i.kind == crate::completion::RUN_DONE_AUDIT_FLAGGED_ATTENTION_KIND),
        "an unchanged head must flag a contradicted declaration for human review: {items:?}"
    );
    assert_eq!(
        publisher.attention_items_created().await,
        1,
        "the flagged attention must also publish a live-update event"
    );
}

#[tokio::test]
async fn audit_records_head_when_pr_head_moved() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    db.set_execution_pr_head_before(&execution_id, "sha_before").unwrap();
    let execution = db.get_execution(&execution_id).unwrap();
    let publisher = Arc::new(RecordingPublisher::default());
    let verifier = StubBranchVerifier::ok("boss/test");
    verifier.set_fresh_head_oid(Ok("sha_after_moved".into())).await;

    audit_declared_delivery(
        verifier.as_ref(),
        &db,
        publisher.as_ref(),
        &execution_id,
        &chore_id,
        &execution.repo_remote_url,
        "sha_before",
        PR_URL,
    )
    .await;

    let saved = db.get_execution(&execution_id).unwrap();
    assert_eq!(saved.pr_head_after.as_deref(), Some("sha_after_moved"));
    assert_eq!(saved.pr_head_after_capture.as_deref(), Some("recorded"));
    let items = db.list_attention_items(&execution_id).unwrap();
    assert!(
        items
            .iter()
            .all(|i| i.kind != crate::completion::RUN_DONE_AUDIT_FLAGGED_ATTENTION_KIND),
        "a moved head confirms the declaration; nothing should be flagged: {items:?}"
    );
    assert_eq!(
        publisher.attention_items_created().await,
        0,
        "a confirmed declaration must not publish a flagged-attention event"
    );
}

#[tokio::test]
async fn audit_no_ops_when_head_fetch_fails() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    db.set_execution_pr_head_before(&execution_id, "sha_before").unwrap();
    let execution = db.get_execution(&execution_id).unwrap();
    let publisher = Arc::new(RecordingPublisher::default());
    let verifier = StubBranchVerifier::ok("boss/test");
    verifier.set_fresh_head_oid(Err("transient gh failure".into())).await;

    audit_declared_delivery(
        verifier.as_ref(),
        &db,
        publisher.as_ref(),
        &execution_id,
        &chore_id,
        &execution.repo_remote_url,
        "sha_before",
        PR_URL,
    )
    .await;

    let items = db.list_attention_items(&execution_id).unwrap();
    assert!(
        items.is_empty(),
        "a failed head fetch is best-effort and must swallow, never flag on no evidence: {items:?}"
    );
    assert_eq!(publisher.attention_items_created().await, 0);
    let saved = db.get_execution(&execution_id).unwrap();
    assert_eq!(saved.pr_head_after, None);
    assert_eq!(saved.pr_head_after_capture.as_deref(), Some("unavailable"));
}

#[tokio::test]
async fn first_pr_completion_captures_head_without_a_dispatch_baseline() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    let execution = db.get_execution(&execution_id).unwrap();
    db.record_declared_worker_pr_completion(
        &execution_id,
        PR_URL,
        crate::work::WorkerPrCompletionTarget::PendingReview,
    )
    .unwrap();
    assert_eq!(
        db.get_execution(&execution_id)
            .unwrap()
            .pr_head_after_capture
            .as_deref(),
        Some("pending")
    );
    let publisher = RecordingPublisher::default();
    let verifier = StubBranchVerifier::ok("boss/test");
    verifier.set_fresh_head_oid(Ok("first_pushed_commit".into())).await;
    audit_declared_delivery(
        verifier.as_ref(),
        &db,
        &publisher,
        &execution_id,
        &chore_id,
        &execution.repo_remote_url,
        "",
        PR_URL,
    )
    .await;
    let saved = db.get_execution(&execution_id).unwrap();
    assert_eq!(saved.status, ExecutionStatus::Completed);
    assert_eq!(saved.pr_head_after.as_deref(), Some("first_pushed_commit"));
    assert_eq!(saved.pr_head_after_capture.as_deref(), Some("recorded"));
    assert!(db.list_attention_items(&execution_id).unwrap().is_empty());
}

#[test]
fn synchronous_head_failure_is_unavailable_even_with_a_done_declaration() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET run_done_outcome = 'delivered' WHERE id = ?1",
            [&execution_id],
        )
        .unwrap();
    let completion = db
        .record_worker_pr_completion(
            &execution_id,
            PR_URL,
            None,
            None,
            crate::work::WorkerPrCompletionTarget::PendingReview,
            None,
        )
        .unwrap()
        .unwrap();
    assert_eq!(completion.execution.pr_head_after, None);
    assert_eq!(
        completion.execution.pr_head_after_capture.as_deref(),
        Some("unavailable")
    );
}
