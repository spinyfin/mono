//! Fixtures shared by every `ci_watch` test module.
//!
//! The `pub(super) use` re-exports below let each sibling test module
//! pull the whole test vocabulary in with a single `use super::helpers::*;`.

pub(super) use std::sync::Arc;

pub(super) use tempfile::tempdir;

pub(super) use super::super::*;
pub(super) use crate::merge_poller::{CiProvider, OpenPrStatus, PrLifecycleProbe, PrLifecycleState};
pub(super) use crate::test_support::*;
pub(super) use crate::work::{TaskStatus, WorkDb, WorkItem, WorkItemPatch};

pub(super) fn make_in_review(db: &WorkDb, name: &str, pr_url: &str) -> (String, String) {
    let product = create_test_product_with_repo(db, &format!("Product-{name}"), Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore_manual(db, product.id.clone(), name);
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    (product.id, chore.id)
}

pub(super) fn candidate(product_id: &str, work_item_id: &str, pr_url: &str) -> PendingMergeCheck {
    PendingMergeCheck {
        work_item_id: work_item_id.to_owned(),
        product_id: product_id.to_owned(),
        pr_url: pr_url.to_owned(),
    }
}

pub(super) fn probe(pr_url: &str, head_sha: &str) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr_url.to_owned())
        .state(PrLifecycleState::Open(OpenPrStatus::clean()))
        .base_ref_oid("base-1")
        .head_ref_oid(head_sha.to_owned())
        .labels(Vec::new())
        .review(crate::merge_poller::PrReviewState::Unknown)
        .build()
}

pub(super) fn probe_with_labels(pr_url: &str, head_sha: &str, labels: &[&str]) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr_url.to_owned())
        .state(PrLifecycleState::Open(OpenPrStatus::clean()))
        .base_ref_oid("base-1")
        .head_ref_oid(head_sha.to_owned())
        .labels(labels.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
        .review(crate::merge_poller::PrReviewState::Unknown)
        .build()
}

pub(super) fn failure(name: &str, conclusion: &str) -> RequiredCheckFailure {
    RequiredCheckFailure {
        name: name.into(),
        conclusion: conclusion.into(),
        target_url: "https://buildkite.com/foo/bar/builds/1#x".into(),
        provider: CiProvider::Buildkite,
        provider_job_id: Some("x".into()),
    }
}

pub(super) fn one_failure() -> Vec<RequiredCheckFailure> {
    vec![RequiredCheckFailure {
        name: "ci/test".into(),
        conclusion: "FAILURE".into(),
        target_url: "https://buildkite.com/anthropic/mono/builds/42#job-uuid".into(),
        provider: CiProvider::Buildkite,
        provider_job_id: Some("job-uuid".into()),
    }]
}

pub(super) fn chore_state(db: &WorkDb, id: &str) -> (TaskStatus, Option<String>) {
    match db.get_work_item(id).unwrap() {
        WorkItem::Chore(t) => (t.status, t.blocked_reason),
        other => panic!("expected chore, got {other:?}"),
    }
}

/// The create-time revision gate's PR-state probe for tests. The
/// production CI producer feeds `StaticPrStateChecker(Open)` (the poller
/// just observed the PR open at clean mergeability); tests use the fake
/// so `create_revision`'s `assert_parent_revisable` sees an open PR
/// without a `gh` round-trip.
pub(super) fn fix_checker() -> crate::work::FakePrStateChecker {
    crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open)
}
