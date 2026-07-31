use super::*;

use std::path::PathBuf;

use crate::merge_poller::{
    OpenPrCiStatus, OpenPrMergeability, OpenPrStatus, PrLifecycleState, PrReviewState, TrunkQueueCheckFailure,
};
use crate::work::TrunkMergeIntentInsertInput;

const PR_URL: &str = "https://github.com/brianduff/flunge/pull/1156";
/// The head `brianduff/flunge` #1156 was actually evicted on.
const EVICTED_HEAD: &str = "a898daa34a0151810ec44b2d69722f5df21119dd";

fn test_db() -> WorkDb {
    WorkDb::open(PathBuf::from(":memory:")).unwrap()
}

/// A `trunk_queue`-mechanism product with one `in_review` chore bound to
/// [`PR_URL`], and the merge-poller candidate that names it.
fn seed(db: &WorkDb, name: &str, mechanism: Option<&str>) -> PendingMergeCheck {
    let product = crate::test_support::create_test_product_named(db, name);
    if let Some(mechanism) = mechanism {
        db.set_product_merge_mechanism(&product.id, Some(mechanism)).unwrap();
    }
    let task = crate::test_support::create_test_chore_manual(db, product.id.clone(), name);
    PendingMergeCheck {
        work_item_id: task.id,
        product_id: product.id,
        pr_url: PR_URL.to_owned(),
    }
}

/// The probe shape the merge poller produces for #1156 at its evicted head:
/// open, mergeable, CI clean on the PR's own head (both Buildkite contexts
/// were green — the failure was on the construction branch), and carrying
/// the failed Trunk check.
fn evicted_probe(head_sha: &str) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(PR_URL)
        .state(PrLifecycleState::Open(OpenPrStatus {
            mergeability: OpenPrMergeability::Clean,
            ci: OpenPrCiStatus::Clean,
        }))
        .head_ref_oid(head_sha.to_owned())
        .base_ref_name("main".to_owned())
        .labels(Vec::new())
        .review(PrReviewState::Approved { reviewers: Vec::new() })
        .trunk_queue_check_failure(TrunkQueueCheckFailure {
            name: "Trunk Merge Queue (main)".to_owned(),
            conclusion: "FAILURE".to_owned(),
            details_url: "https://app.trunk.io/flunge/merge-queue/c1478ade-ef63-4ba9-86de-b45801e5fb5e/1156".to_owned(),
        })
        .build()
}

/// The same PR with no Trunk episode at all — what every non-evicted probe
/// looks like.
fn healthy_probe() -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(PR_URL)
        .state(PrLifecycleState::Open(OpenPrStatus {
            mergeability: OpenPrMergeability::Clean,
            ci: OpenPrCiStatus::Clean,
        }))
        .head_ref_oid(EVICTED_HEAD.to_owned())
        .base_ref_name("main".to_owned())
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .build()
}

/// The bug this module exists for: a `trunk_queue` product's PR evicted from
/// the queue by an episode Boss never submitted. Before adoption there is no
/// intent, so the queue poller enumerates nothing and no eviction is ever
/// remediated.
#[test]
fn adopts_an_eviction_no_merge_intent_tracks() {
    let db = test_db();
    let candidate = seed(&db, "flunge", Some("trunk_queue"));

    assert!(
        db.get_active_trunk_merge_intent(&candidate.work_item_id)
            .unwrap()
            .is_none(),
        "precondition: nothing tracks this episode",
    );
    assert!(adopt_unattributed_trunk_queue_episode(
        &db,
        &candidate,
        &evicted_probe(EVICTED_HEAD)
    ));

    let intent = db
        .get_active_trunk_merge_intent(&candidate.work_item_id)
        .unwrap()
        .expect("adoption inserts an active intent the queue poller will enumerate");
    assert_eq!(intent.pr_url, PR_URL);
    assert_eq!(intent.pr_number, 1156);
    assert_eq!(intent.repo, "brianduff/flunge");
    assert_eq!(intent.target_branch, "main");
    assert_eq!(
        intent.adopted_at_head_sha.as_deref(),
        Some(EVICTED_HEAD),
        "provenance: a non-NULL head sha is what distinguishes an adopted episode from a merge click",
    );

    // The adopted row is a first-class member of the poller's candidate set —
    // this is the whole point of adopting rather than remediating here.
    let listed = db.list_active_trunk_merge_intents().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].intent.id, intent.id);
    assert_eq!(listed[0].product_id, candidate.product_id);
}

/// The no-duplicate-remediation gate. An active intent means the queue
/// poller already tracks this work item and will resolve the eviction under
/// its own episode discriminator; a second row here would be a second
/// remediation for one eviction.
#[test]
fn does_not_adopt_when_an_active_merge_intent_already_tracks_the_work_item() {
    let db = test_db();
    let candidate = seed(&db, "flunge-tracked", Some("trunk_queue"));
    let existing = db
        .insert_trunk_merge_intent(
            TrunkMergeIntentInsertInput::builder()
                .work_item_id(candidate.work_item_id.clone())
                .pr_url(PR_URL)
                .pr_number(1156)
                .repo("brianduff/flunge")
                .target_branch("main")
                .build(),
        )
        .unwrap()
        .unwrap();

    assert!(!adopt_unattributed_trunk_queue_episode(
        &db,
        &candidate,
        &evicted_probe(EVICTED_HEAD)
    ));

    let intent = db
        .get_active_trunk_merge_intent(&candidate.work_item_id)
        .unwrap()
        .unwrap();
    assert_eq!(intent.id, existing.id, "the merge verb's intent is left untouched");
    assert!(
        intent.adopted_at_head_sha.is_none(),
        "a merge-verb intent must not be re-stamped as adopted",
    );
}

/// An evicted intent is deliberately left `active` until its fix lands, so
/// the gate above covers the entire remediation window — the sweep sees the
/// same failed head check on every pass in between.
#[test]
fn does_not_adopt_while_an_eviction_episode_is_still_being_remediated() {
    let db = test_db();
    let candidate = seed(&db, "flunge-remediating", Some("trunk_queue"));
    let existing = db
        .insert_trunk_merge_intent(
            TrunkMergeIntentInsertInput::builder()
                .work_item_id(candidate.work_item_id.clone())
                .pr_url(PR_URL)
                .pr_number(1156)
                .repo("brianduff/flunge")
                .target_branch("main")
                .build(),
        )
        .unwrap()
        .unwrap();
    db.record_trunk_merge_intent_state(&existing.id, "failed").unwrap();

    for _ in 0..3 {
        assert!(!adopt_unattributed_trunk_queue_episode(
            &db,
            &candidate,
            &evicted_probe(EVICTED_HEAD)
        ));
    }
    assert_eq!(count_intents(&db, &candidate.work_item_id), 1);
}

/// Exactly once per episode. Trunk posts its check per commit, so a head
/// whose check stays failed must not be re-adopted on every 60 s sweep —
/// especially once the poller has retired the resulting intent, which is
/// when the active-intent gate above stops covering it.
#[test]
fn adopts_at_most_once_per_head_even_after_the_intent_is_retired() {
    let db = test_db();
    let candidate = seed(&db, "flunge-once", Some("trunk_queue"));

    assert!(adopt_unattributed_trunk_queue_episode(
        &db,
        &candidate,
        &evicted_probe(EVICTED_HEAD)
    ));
    let adopted = db
        .get_active_trunk_merge_intent(&candidate.work_item_id)
        .unwrap()
        .unwrap();
    // The queue poller resolves the episode and retires the intent — e.g. a
    // human cancelled the entry. The head check stays failed regardless.
    db.retire_trunk_merge_intent(&adopted.id, "cancelled").unwrap();

    for _ in 0..3 {
        assert!(
            !adopt_unattributed_trunk_queue_episode(&db, &candidate, &evicted_probe(EVICTED_HEAD)),
            "a retired adoption must not re-fire the whole resolve/retire cycle every sweep",
        );
    }
    assert_eq!(count_intents(&db, &candidate.work_item_id), 1);
}

/// The other half of head-sha keying: it is episode-scoped, not permanent.
/// A ci-fix that lands moves the head, and a fresh eviction on that new head
/// is a genuinely new episode that must be adopted again.
#[test]
fn adopts_again_for_a_new_head_after_the_previous_episode_resolved() {
    let db = test_db();
    let candidate = seed(&db, "flunge-again", Some("trunk_queue"));

    assert!(adopt_unattributed_trunk_queue_episode(
        &db,
        &candidate,
        &evicted_probe(EVICTED_HEAD)
    ));
    let first = db
        .get_active_trunk_merge_intent(&candidate.work_item_id)
        .unwrap()
        .unwrap();
    db.retire_trunk_merge_intent(&first.id, "cancelled").unwrap();

    const FIXED_HEAD: &str = "c6ce7e3a0812cf5ef843e73f8ed0b420cea6c06e";
    assert!(adopt_unattributed_trunk_queue_episode(
        &db,
        &candidate,
        &evicted_probe(FIXED_HEAD)
    ));

    let second = db
        .get_active_trunk_merge_intent(&candidate.work_item_id)
        .unwrap()
        .unwrap();
    assert_ne!(second.id, first.id);
    assert_eq!(second.adopted_at_head_sha.as_deref(), Some(FIXED_HEAD));
}

/// No Trunk episode on the head — the overwhelmingly common case, and the
/// one that must cost nothing.
#[test]
fn does_not_adopt_without_a_failed_trunk_check() {
    let db = test_db();
    let candidate = seed(&db, "flunge-healthy", Some("trunk_queue"));

    assert!(!adopt_unattributed_trunk_queue_episode(
        &db,
        &candidate,
        &healthy_probe()
    ));
    assert_eq!(count_intents(&db, &candidate.work_item_id), 0);
}

/// A product that merges directly has no Trunk queue to adopt into, whatever
/// a check leaf happens to be called.
#[test]
fn does_not_adopt_for_a_direct_merge_product() {
    let db = test_db();
    for mechanism in [None, Some("direct")] {
        let candidate = seed(&db, &format!("direct-{mechanism:?}"), mechanism);
        assert!(!adopt_unattributed_trunk_queue_episode(
            &db,
            &candidate,
            &evicted_probe(EVICTED_HEAD)
        ));
        assert_eq!(count_intents(&db, &candidate.work_item_id), 0);
    }
}

/// Without a head sha there is no key to be idempotent on, and adopting
/// anyway would re-adopt every sweep. Declining is the lesser evil — and it
/// is logged at `warn!`, not swallowed.
#[test]
fn does_not_adopt_without_a_head_sha() {
    let db = test_db();
    let candidate = seed(&db, "flunge-no-head", Some("trunk_queue"));
    let mut probe = evicted_probe(EVICTED_HEAD);
    probe.head_ref_oid = None;

    assert!(!adopt_unattributed_trunk_queue_episode(&db, &candidate, &probe));
    assert_eq!(count_intents(&db, &candidate.work_item_id), 0);
}

/// The queue an episode belongs to is the branch its PR targets, not the
/// product default — a PR queued into a release branch must not be looked up
/// against `main`'s queue.
#[test]
fn target_branch_comes_from_the_prs_own_base() {
    let db = test_db();
    let candidate = seed(&db, "flunge-release", Some("trunk_queue"));
    let mut probe = evicted_probe(EVICTED_HEAD);
    probe.base_ref_name = Some("release-2026-07".to_owned());

    assert!(adopt_unattributed_trunk_queue_episode(&db, &candidate, &probe));
    let intent = db
        .get_active_trunk_merge_intent(&candidate.work_item_id)
        .unwrap()
        .unwrap();
    assert_eq!(intent.target_branch, "release-2026-07");
}

/// …falling back to the product's configured target branch when GitHub
/// reported no base ref.
#[test]
fn target_branch_falls_back_to_the_product_default() {
    let db = test_db();
    let candidate = seed(&db, "flunge-nobase", Some("trunk_queue"));
    let mut probe = evicted_probe(EVICTED_HEAD);
    probe.base_ref_name = None;

    assert!(adopt_unattributed_trunk_queue_episode(&db, &candidate, &probe));
    let intent = db
        .get_active_trunk_merge_intent(&candidate.work_item_id)
        .unwrap()
        .unwrap();
    assert_eq!(intent.target_branch, "main");
}

fn count_intents(db: &WorkDb, work_item_id: &str) -> i64 {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM trunk_merge_intents WHERE work_item_id = ?1",
            rusqlite::params![work_item_id],
            |row| row.get(0),
        )
        .unwrap()
}
