//! Handler-level coverage of the composed `worker_proposals &&
//! pr_created_proposals_seam` flag passed into apply.

use super::*;

const PR_CREATED_URL_ONE: &str = "https://github.com/spinyfin/mono/pull/1";
const PR_CREATED_URL_TWO: &str = "https://github.com/spinyfin/mono/pull/2";

fn chore_pr_url(fx: &WorkerFixture) -> Option<String> {
    match fx.server_state.work_db.get_work_item(&fx.work_item_id).unwrap() {
        boss_work_db::WorkItem::Chore(task) => task.pr_url,
        other => panic!("expected chore, got {other:?}"),
    }
}

async fn submit_pr_created(fx: &WorkerFixture, url: &str, key: &str) -> FrontendEvent {
    call_with_peer(
        &fx.server_state,
        Some(fx.peer_pid),
        submit_request_keyed(&fx.execution_id, ProposalKind::PrCreated, json!({"pr_url": url}), key),
    )
    .await
}

/// With both flags on, a conflicting `pr_created` submission is stored as
/// `Rejected` and the existing binding stays in place.
#[tokio::test]
async fn conflicting_pr_created_is_rejected_when_both_seam_flags_are_on() {
    let fx = WorkerFixture::new();
    fx.server_state.feature_flags.set("worker_proposals", true).unwrap();
    fx.server_state
        .feature_flags
        .set("pr_created_proposals_seam", true)
        .unwrap();

    let (first, _) = submitted(submit_pr_created(&fx, PR_CREATED_URL_ONE, "pr-1").await);
    assert_eq!(first.state, ProposalState::Applied);
    assert_eq!(chore_pr_url(&fx).as_deref(), Some(PR_CREATED_URL_ONE));

    let (second, _) = submitted(submit_pr_created(&fx, PR_CREATED_URL_TWO, "pr-2").await);
    assert_eq!(
        second.state,
        ProposalState::Rejected,
        "composed seam must reject a conflicting PR URL"
    );
    assert_eq!(
        chore_pr_url(&fx).as_deref(),
        Some(PR_CREATED_URL_ONE),
        "the original binding must survive a rejected conflict"
    );
}

/// With the seam flag off, the same conflicting submission is `Applied`
/// (legacy warn-and-keep-binding behaviour). Covers a regression that
/// drops `pr_created_proposals_seam` from the composed flag or reverts
/// the handler to `submit_worker_proposal`.
#[tokio::test]
async fn conflicting_pr_created_applies_legacy_when_seam_flag_is_off() {
    let fx = WorkerFixture::new();
    fx.server_state.feature_flags.set("worker_proposals", true).unwrap();
    fx.server_state
        .feature_flags
        .set("pr_created_proposals_seam", false)
        .unwrap();

    let (first, _) = submitted(submit_pr_created(&fx, PR_CREATED_URL_ONE, "pr-1").await);
    assert_eq!(first.state, ProposalState::Applied);
    assert_eq!(chore_pr_url(&fx).as_deref(), Some(PR_CREATED_URL_ONE));

    let (second, _) = submitted(submit_pr_created(&fx, PR_CREATED_URL_TWO, "pr-2").await);
    assert_eq!(
        second.state,
        ProposalState::Applied,
        "seam off must reproduce the pre-migration apply: warn, not reject"
    );
    assert_eq!(
        chore_pr_url(&fx).as_deref(),
        Some(PR_CREATED_URL_ONE),
        "the original binding must survive even when the conflicting declaration applies"
    );
}
