//! Direct coverage for [`crate::completion::run_done_declaration::audit_declared_delivery`],
//! the post-hoc (off the termination path) check that a `delivered` run_done
//! declaration's bound PR actually moved. The RPC-level tests in
//! `app/tests/proposals.rs` cover submit-time terminalization, but none of
//! them drive this specific background task — every test execution there
//! carries an empty `pr_head_before`, so the spawn in
//! `finalize_declared_delivery` returns before ever calling
//! `fetch_pr_head_oid`. These tests call the free function directly with a
//! [`StubBranchVerifier`] instead.

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
    verifier.set_head_oid(Ok("sha_before".into())).await;

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
async fn audit_no_ops_when_pr_head_moved() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());
    db.set_execution_pr_head_before(&execution_id, "sha_before").unwrap();
    let execution = db.get_execution(&execution_id).unwrap();
    let publisher = Arc::new(RecordingPublisher::default());
    let verifier = StubBranchVerifier::ok("boss/test");
    verifier.set_head_oid(Ok("sha_after_moved".into())).await;

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
    verifier.set_head_oid(Err("transient gh failure".into())).await;

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
}
