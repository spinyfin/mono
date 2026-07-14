use std::sync::Arc;

use async_trait::async_trait;
use tempfile::tempdir;
use tokio::sync::Mutex;

use super::*;
use crate::coordinator::{CubeRepoHandle, CubeWorkspaceLease, RebaseOutcome};
use crate::merge_poller::{OpenPrStatus, PrLifecycleProbe, PrLifecycleState};
use crate::test_support::*;
use crate::work::{WorkDb, WorkItem, WorkItemPatch};

fn make_in_review(db: &WorkDb, name: &str, pr_url: &str) -> (String, String) {
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

fn chore_status(db: &WorkDb, id: &str) -> (TaskStatus, Option<String>) {
    match db.get_work_item(id).unwrap() {
        WorkItem::Chore(t) => (t.status, t.blocked_reason),
        other => panic!("expected chore, got {other:?}"),
    }
}

fn candidate(product_id: &str, work_item_id: &str, pr_url: &str) -> PendingMergeCheck {
    PendingMergeCheck {
        work_item_id: work_item_id.to_owned(),
        product_id: product_id.to_owned(),
        pr_url: pr_url.to_owned(),
    }
}

fn probe(pr_url: &str, state: PrLifecycleState) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr_url.to_owned())
        .state(state)
        .base_ref_oid("abc123")
        .head_ref_oid("head456")
        .head_ref_name("feature")
        .base_ref_name("main")
        .labels(Vec::new())
        .review(crate::merge_poller::PrReviewState::Unknown)
        .build()
}

/// A `PrStateChecker` that reports every PR as `Open`, so the
/// `create_revision` create-time gate passes for the in-review chore
/// fixtures these tests build. A conflicting PR is, by definition,
/// still open — matching what `GhPrStateChecker` returns in production.
fn open_checker() -> crate::work::FakePrStateChecker {
    crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open)
}

fn probe_with_labels(pr_url: &str, state: PrLifecycleState, labels: &[&str]) -> PrLifecycleProbe {
    PrLifecycleProbe::builder()
        .url(pr_url.to_owned())
        .state(state)
        .base_ref_oid("abc123")
        .head_ref_oid("head456")
        .head_ref_name("feature")
        .base_ref_name("main")
        .labels(labels.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
        .review(crate::merge_poller::PrReviewState::Unknown)
        .build()
}

/// A [`CubeClient`] double whose engine-direct rebase always reports a clean,
/// pushed rebase — driving the rung-1 escalation-ladder path (T4).
struct CleanRebaseCube;

crate::stub_cube_client! { CleanRebaseCube {
    async fn ensure_repo(&self, _origin: &str) -> anyhow::Result<CubeRepoHandle> {
        Ok(CubeRepoHandle { repo_id: "repo-1".to_owned() })
    }
    async fn lease_workspace(
        &self,
        _repo_id: &str,
        _task: &str,
        _prefer: Option<&str>,
        _allow_dirty: bool,
        _exclude: &[&str],
    ) -> anyhow::Result<CubeWorkspaceLease> {
        Ok(CubeWorkspaceLease {
            lease_id: "L1".to_owned(),
            workspace_id: "W1".to_owned(),
            workspace_path: std::path::PathBuf::from("/tmp/rung1-ws"),
        })
    }
    async fn goto_workspace(&self, _workspace_path: &std::path::Path, _pr: u64) -> anyhow::Result<()> {
        Ok(())
    }
    async fn rebase_workspace(&self, _workspace_path: &std::path::Path, _pr: u64) -> anyhow::Result<RebaseOutcome> {
        Ok(RebaseOutcome { clean: true, pushed: true, conflicted_files: Vec::new() })
    }
    async fn release_workspace(&self, _lease_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
} }

/// Escalation ladder (T4): when a live `CubeClient` is provided and the
/// engine-direct rebase resolves cleanly, `on_conflict_detected` retires the
/// conflict at rung 1 with no worker — the parent returns to Review, the
/// attempt is `succeeded` with `resolved_by_rung = 1`, and no revision task is
/// created.
#[tokio::test]
async fn conflict_auto_resolved_by_rung1_rebase_without_spawning_worker() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/77";
    let (product, chore) = make_in_review(&db, "C-rung1", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = CleanRebaseCube;

    let transitioned = on_conflict_detected(
        &db,
        pub_.as_ref(),
        Some(&cube),
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    assert!(transitioned, "rung-1 resolution is a state change");
    // Parent back in Review, unblocked.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none(), "block should be cleared, got {reason:?}");
    // No active attempt (it retired); latest attempt succeeded at rung 1 with
    // no worker revision.
    assert!(
        db.active_conflict_resolution_for_work_item(&chore).unwrap().is_none(),
        "attempt should be terminal, not active"
    );
    let latest = db.latest_conflict_resolution_for_work_item(&chore).unwrap().unwrap();
    assert_eq!(latest.status, "succeeded");
    assert_eq!(latest.resolved_by_rung, Some(1));
    assert!(
        latest.revision_task_id.is_none(),
        "no worker revision should have been created for a rung-1 resolution"
    );
}

/// Without a `CubeClient` (rung 1 disabled / flag off), detection preserves the
/// pre-ladder worker path exactly: a revision is spawned and the parent stays
/// in Review with an in-flight attempt.
#[tokio::test]
async fn conflict_without_cube_client_spawns_worker_as_before() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/78";
    let (product, chore) = make_in_review(&db, "C-noladder", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let transitioned = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    assert!(transitioned);
    // Worker path: a revision fix vehicle exists and the attempt is still live.
    let active = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("an active attempt with a spawned revision");
    assert_eq!(active.status, "pending");
    assert!(
        active.revision_task_id.is_some(),
        "the pre-ladder path must spawn a worker revision"
    );
}

/// A [`CubeClient`] double whose engine-direct rebase leaves a fixed set of
/// residual conflicted files — drives rung 2's (T6) small-agent-profile
/// decision in `on_conflict_detected`.
struct ConflictsCube {
    conflicted_files: Vec<String>,
}

crate::stub_cube_client! { ConflictsCube {
    async fn ensure_repo(&self, _origin: &str) -> anyhow::Result<CubeRepoHandle> {
        Ok(CubeRepoHandle { repo_id: "repo-1".to_owned() })
    }
    async fn lease_workspace(
        &self,
        _repo_id: &str,
        _task: &str,
        _prefer: Option<&str>,
        _allow_dirty: bool,
        _exclude: &[&str],
    ) -> anyhow::Result<CubeWorkspaceLease> {
        Ok(CubeWorkspaceLease {
            lease_id: "L2".to_owned(),
            workspace_id: "W2".to_owned(),
            workspace_path: std::path::PathBuf::from("/tmp/rung2-ws"),
        })
    }
    async fn goto_workspace(&self, _workspace_path: &std::path::Path, _pr: u64) -> anyhow::Result<()> {
        Ok(())
    }
    async fn rebase_workspace(&self, _workspace_path: &std::path::Path, _pr: u64) -> anyhow::Result<RebaseOutcome> {
        Ok(RebaseOutcome { clean: false, pushed: false, conflicted_files: self.conflicted_files.clone() })
    }
    async fn release_workspace(&self, _lease_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
} }

/// Escalation ladder rung 2 (T6): a single residual conflicted file after
/// rung 1's rebase is bounded — `on_conflict_detected` spawns the revision
/// with the small-agent profile (`effort_level = small`) and stamps the
/// attempt `resolved_by_rung = 2` up front.
#[tokio::test]
async fn conflict_with_bounded_residue_spawns_rung2_small_agent_revision() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/79";
    let (product, chore) = make_in_review(&db, "C-rung2", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = ConflictsCube {
        conflicted_files: vec!["src/lib.rs".to_owned()],
    };

    let transitioned = on_conflict_detected(
        &db,
        pub_.as_ref(),
        Some(&cube),
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    assert!(transitioned);
    let active = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("an active attempt with a spawned rung-2 revision");
    assert_eq!(active.status, "pending");
    assert_eq!(
        active.resolved_by_rung,
        Some(2),
        "rung 2 must stamp resolved_by_rung up front, before the revision has run"
    );
    let revision_id = active.revision_task_id.expect("rung 2 must spawn a worker revision");
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => {
            assert_eq!(
                t.effort_level,
                Some(boss_protocol::EffortLevel::Small),
                "rung 2's revision must use the small-agent profile"
            );
        }
        other => panic!("expected a task-shaped revision, got {other:?}"),
    }
}

/// Escalation ladder rung 2 (T6) decline: a residue beyond the bound is
/// treated as a large/architectural conflict — the revision spawns with the
/// default (rung 3, full-worker) profile, not the small-agent one.
#[tokio::test]
async fn conflict_with_unbounded_residue_declines_rung2() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/80";
    let (product, chore) = make_in_review(&db, "C-rung2-decline", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = ConflictsCube {
        conflicted_files: vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()],
    };

    let transitioned = on_conflict_detected(
        &db,
        pub_.as_ref(),
        Some(&cube),
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    assert!(transitioned);
    let active = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("an active attempt with a spawned revision");
    // The real discriminator between "rung 2 spawned this" and "rung 3's
    // default path spawned this" is `resolved_by_rung`, stamped up front only
    // on the rung-2 branch — `effort_level` alone doesn't distinguish them
    // here because a plain chore parent's un-overridden default is already
    // `small` (see `default_revision_effort_level`), same as rung 2's
    // explicit override.
    assert_eq!(
        active.resolved_by_rung, None,
        "a declined rung 2 must not stamp resolved_by_rung (defaults to 3 at actual completion)"
    );
    assert!(
        active.revision_task_id.is_some(),
        "rung 3 must still spawn a worker revision"
    );
}

/// New-model acceptance: when a revision fix vehicle is successfully spawned,
/// the parent stays in `in_review` (Review column). The blocked state is only
/// reached when there is no tractable fix vehicle (churn cap, create_revision
/// failure, closed PR). See also `detection_blocks_parent_when_revision_fails`.
#[tokio::test]
async fn detection_keeps_parent_in_review_when_revision_spawns() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/10";
    let (product, chore) = make_in_review(&db, "C-detect", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let transitioned = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    // transitioned == true because parent went in_review→blocked→in_review
    assert!(transitioned, "first detection must return true (state changed)");

    // Parent stays in Review — not blocked.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    // Event emitted is "conflict_revision_in_flight", not "blocked_merge_conflict".
    let events = pub_.events.lock().await.clone();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        (product.clone(), chore.clone(), "conflict_revision_in_flight".into())
    );

    // crz row exists and revision was spawned.
    let attempt = db.active_conflict_resolution_for_work_item(&chore).unwrap();
    assert!(attempt.is_some(), "crz attempt row must be present");
    let attempt = attempt.unwrap();
    assert_eq!(attempt.status, "pending");
    assert!(attempt.revision_task_id.is_some(), "revision must have been spawned");
}

/// When `create_revision` fails (parent PR closed/unmerged) or the churn cap
/// pre-abandons the attempt, the parent DOES flip to `blocked: merge_conflict`
/// so the human sees the card in Blocked.
#[tokio::test]
async fn detection_blocks_parent_when_revision_fails() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/10b";
    let (product, chore) = make_in_review(&db, "C-detect-fail", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);

    let transitioned = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &closed,
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(transitioned, "detection must return true (parent blocked)");

    // Parent is blocked since there is no active fix vehicle.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("merge_conflict"));

    let events = pub_.events.lock().await.clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].2, "blocked_merge_conflict");

    // crz was abandoned (revision_create_failed).
    let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].status, "abandoned");
    assert_eq!(attempts[0].failure_reason.as_deref(), Some("revision_create_failed"),);
}

#[tokio::test]
async fn detection_is_idempotent_on_repeated_probes() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/11";
    let (product, chore) = make_in_review(&db, "C-idem", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First probe: conflict detected, revision spawned, parent stays in_review.
    let first = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    // Second probe with same base sha: UNIQUE collision on crz insert.
    // Existing crz has revision_task_id → upfront flip cleared back to
    // in_review by the collision path, but no net state change vs what
    // we already have.
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(first, "first probe must return true (state changed)");
    // Second probe: upfront flip still briefly goes to blocked then clears
    // back — returns true again because task_unblocked_for_revision=true.
    // The important invariant: parent ends up in_review, exactly one crz.
    let (status, _) = chore_status(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::InReview,
        "parent must stay in_review after repeated probes"
    );
    let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(attempts.len(), 1, "same base sha must not stack crz rows");
    // Exactly one ConflictResolutionStarted typed event per probe.
    let started_count = pub_
        .typed_events
        .lock()
        .await
        .iter()
        .filter(|(_, ev)| matches!(ev, FrontendEvent::ConflictResolutionStarted { .. }))
        .count();
    assert!(started_count >= 1, "at least one ConflictResolutionStarted must fire");
    // At most two "conflict_revision_in_flight" events (one per probe), never
    // a "blocked_merge_conflict" since a fix vehicle is always in flight.
    let reasons: Vec<String> = pub_.events.lock().await.iter().map(|(_, _, r)| r.clone()).collect();
    assert!(
        reasons.iter().all(|r| r == "conflict_revision_in_flight"),
        "all work-item events must be conflict_revision_in_flight, got {reasons:?}",
    );
    let _ = second; // return value may be true or false; variant covered by the assertions above
}

/// New-model: parent was never blocked (revision spawned, stayed in_review).
/// When the PR becomes clean, the crz attempt is retired and the signal
/// cleared. The parent is already in_review — no status-change event fires.
#[tokio::test]
async fn resolution_retires_attempt_when_parent_was_in_review() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/12";
    let (product, chore) = make_in_review(&db, "C-resolve", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    // Parent is in_review (revision spawned). Verify, then resolve.
    let (status_before, _) = chore_status(&db, &chore);
    assert_eq!(status_before, TaskStatus::InReview);

    let resolved = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(resolved, "on_resolved must return true (attempt was retired)");

    // Parent still in_review — didn't change status.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    // No "merge_conflict_resolved" work-item event (parent didn't transition).
    let events = pub_.events.lock().await.clone();
    assert!(
        !events.iter().any(|(_, _, r)| r == "merge_conflict_resolved"),
        "merge_conflict_resolved must not fire when parent was already in_review",
    );

    // ConflictResolutionSucceeded typed event must fire.
    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed
            .iter()
            .any(|(_, ev)| matches!(ev, FrontendEvent::ConflictResolutionSucceeded { .. })),
        "ConflictResolutionSucceeded must fire, got {typed:?}",
    );
}

/// Old-model compatibility: when the parent IS blocked (revision_create_failed,
/// churn cap), on_resolved flips it back to in_review and emits
/// "merge_conflict_resolved".
#[tokio::test]
async fn resolution_flips_blocked_parent_back_to_in_review() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/12b";
    let (product, chore) = make_in_review(&db, "C-resolve-blocked", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);

    // Drive into blocked via create_revision failure.
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &closed,
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    let (status_before, reason_before) = chore_status(&db, &chore);
    assert_eq!(status_before, TaskStatus::Blocked);
    assert_eq!(reason_before.as_deref(), Some("merge_conflict"));

    // Now manually install a running attempt (simulates legacy worker) and resolve.
    let attempt_id = install_running_attempt(&db, &product, &chore, pr, "lease-x");
    let resolved = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(resolved);

    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    let events = pub_.events.lock().await.clone();
    assert!(
        events.iter().any(|(_, _, r)| r == "merge_conflict_resolved"),
        "merge_conflict_resolved must fire when parent was blocked, got {events:?}",
    );
    // Verify attempt was retired.
    let attempt_row = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
    assert_eq!(attempt_row.status, "succeeded");
}

#[tokio::test]
async fn resolution_is_idempotent_on_repeated_clean_probes() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/13";
    let (product, chore) = make_in_review(&db, "C-clean-noop", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First call: row is in_review (not blocked), so resolution is
    // a no-op — the WHERE guard misses, no event published.
    let r1 = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(!r1);
    assert!(pub_.events.lock().await.is_empty());

    // Drive a full conflict-resolve cycle, then call resolution
    // twice — the second call must also be a no-op.
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    let r2 = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    let r3 = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(r2);
    assert!(!r3);
}

#[tokio::test]
async fn cycle_conflict_resolve_conflict() {
    // Integration: conflict detected (revision in flight) → PR resolved →
    // conflict again (same base sha → UNIQUE collision, crz was succeeded,
    // no new active crz → parent flips to blocked this time).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/14";
    let (product, chore) = make_in_review(&db, "C-cycle", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // 1st conflict: revision spawns, parent stays in_review.
    assert!(
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            None,
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await
    );
    let (s, _) = chore_status(&db, &chore);
    assert_eq!(s, TaskStatus::InReview);

    // Resolve: PR goes clean, attempt retired, signal cleared.
    assert!(on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await);
    let (s, _) = chore_status(&db, &chore);
    assert_eq!(s, TaskStatus::InReview);

    // 2nd conflict: same base sha → UNIQUE collision. The previous crz is
    // now succeeded (no active crz). The upfront flip goes to blocked and
    // no revision is spawned (no fresh active crz to dispatch). Parent ends
    // up blocked because there is no fix vehicle.
    assert!(
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            None,
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await
    );
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("merge_conflict"));

    let reasons: Vec<String> = pub_.events.lock().await.iter().map(|(_, _, r)| r.clone()).collect();
    // 1st conflict → "conflict_revision_in_flight"
    // resolve    → no work-item event (parent was in_review)
    // 2nd conflict → "blocked_merge_conflict" (UNIQUE collision, no active crz)
    assert_eq!(
        reasons,
        vec![
            "conflict_revision_in_flight".to_owned(),
            "blocked_merge_conflict".to_owned(),
        ],
    );
}

/// Regression test for T2396 / PR #1874: the stale-base re-arm path
/// must not permanently no-op when a succeeded crz's resolution has
/// gone stale (PR still CONFLICTING) but the PR's `baseRefOid` — fixed
/// at PR-open time — hasn't moved. GitHub never advances `baseRefOid`
/// as `main` moves under an in-review PR, so keying the re-arm insert
/// on `base_sha_at_trigger` alone collided with the succeeded row's
/// UNIQUE slot forever. `head_sha_before` DOES vary — a real resolution
/// attempt pushes a fix commit — so folding it into the key lets this
/// re-arm create a fresh row and spawn a second revision.
#[tokio::test]
async fn rearm_dispatches_fresh_attempt_when_succeeded_crz_has_stale_frozen_base() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/16";
    let (product, chore) = make_in_review(&db, "C-rearm-stale-base", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First conflict: head is "head-before-resolution". Revision spawns,
    // parent stays in_review.
    assert!(
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            None,
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe_with_head(
                pr,
                PrLifecycleState::Open(OpenPrStatus::conflict_only()),
                "head-before-resolution"
            ),
        )
        .await
    );
    let first_attempt = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("first attempt must exist");

    // The revision resolves the conflict — the worker pushes a fix commit,
    // so the head moves. The PR briefly reports clean, retiring the attempt
    // to `succeeded` (parent was in_review the whole time, so it stays there).
    assert!(on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await);
    let succeeded = db.get_conflict_resolution(&first_attempt.id).unwrap().unwrap();
    assert_eq!(succeeded.status, "succeeded");

    // Simulate the parent having been left `blocked: merge_conflict` by an
    // earlier sweep (the direct-flip UNIQUE-collision path in
    // `cycle_conflict_resolve_conflict` demonstrates how this happens).
    // This routes the next detection through the re-arm branch rather
    // than the primary WHERE-guard flip.
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("blocked".into()),
            blocked_reason: Some("merge_conflict".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    // `main` moves further before any new push happens: the PR is
    // CONFLICTING again, GitHub's baseRefOid is UNCHANGED (still
    // "abc123" — it never tracks `main`), but the head now reflects
    // the fix commit the resolved attempt pushed.
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_head(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            "head-after-resolution",
        ),
    )
    .await;
    assert!(second, "re-arm must report a state change (revision spawned)");

    // A second, distinct attempt row must exist with a fresh revision.
    let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(
        attempts.len(),
        2,
        "re-arm must create a second attempt row, got {attempts:?}"
    );
    let second_attempt = attempts
        .iter()
        .find(|a| a.id != first_attempt.id)
        .expect("a second, distinct attempt row must exist");
    assert_eq!(second_attempt.status, "pending");
    assert!(
        second_attempt.revision_task_id.is_some(),
        "re-arm must spawn a fresh revision, got {second_attempt:?}",
    );
    assert_eq!(second_attempt.base_sha_at_trigger.as_deref(), Some("abc123"));
    assert_eq!(second_attempt.head_sha_before.as_deref(), Some("head-after-resolution"));

    // Parent must be unblocked back to in_review — the fresh revision is
    // now the fix vehicle in flight.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
}

#[tokio::test]
async fn detection_skipped_when_human_moved_row_off_in_review() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/15";
    let (product, chore) = make_in_review(&db, "C-human", pr);
    // Human flipped the row to `active` after PR was opened.
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("active".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let pub_ = Arc::new(RecordingPublisher::default());

    let transitioned = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(!transitioned, "WHERE guard protects manual moves");
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::Active);
    assert!(reason.is_none());
    assert!(pub_.events.lock().await.is_empty());
}

#[tokio::test]
async fn resolution_skipped_when_human_moved_row_off_blocked() {
    // Use closed_checker so the parent actually ends up blocked
    // (revision_create_failed → no fix vehicle). The human then moves
    // the blocked row to `active` (manual override). on_resolved must
    // be a no-op because the active crz is abandoned (not pending/running).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/16";
    let (product, chore) = make_in_review(&db, "C-human-2", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &closed,
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    let (status_before, _) = chore_status(&db, &chore);
    assert_eq!(
        status_before,
        TaskStatus::Blocked,
        "sanity: closed_checker must cause blocked"
    );
    // Human moves the blocked row to `active`.
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("active".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let before_count = pub_.events.lock().await.len();
    // on_resolved: abandoned crz → no active_conflict_resolution → clear_chore
    // WHERE guard misses (status='active') → no-op.
    let r = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(!r);
    assert_eq!(pub_.events.lock().await.len(), before_count);
}

/// `CubeClient` that records every `release_workspace` call so the
/// retire-path tests can assert the lease release fired without
/// standing up a real cube process.
#[derive(Default)]
struct RecordingCubeClient {
    releases: Mutex<Vec<String>>,
    release_should_fail: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl crate::coordinator::CubeClient for RecordingCubeClient {
    async fn ensure_repo(&self, _origin: &str) -> anyhow::Result<crate::coordinator::CubeRepoHandle> {
        unreachable!("not used in conflict_watch tests")
    }
    async fn lease_workspace(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: bool,
        _: &[&str],
    ) -> anyhow::Result<crate::coordinator::CubeWorkspaceLease> {
        unreachable!("not used in conflict_watch tests")
    }
    async fn create_change(
        &self,
        _: &std::path::Path,
        _: &str,
    ) -> anyhow::Result<crate::coordinator::CubeChangeHandle> {
        unreachable!("not used in conflict_watch tests")
    }
    async fn goto_workspace(&self, _: &std::path::Path, _: u64) -> anyhow::Result<()> {
        unreachable!("not used in conflict_watch tests")
    }
    async fn release_workspace(&self, lease_id: &str) -> anyhow::Result<()> {
        self.releases.lock().await.push(lease_id.to_owned());
        if self.release_should_fail.load(std::sync::atomic::Ordering::SeqCst) {
            Err(anyhow::anyhow!("simulated lease release failure"))
        } else {
            Ok(())
        }
    }
    async fn workspace_status(&self, _: &std::path::Path) -> anyhow::Result<crate::coordinator::CubeWorkspaceStatus> {
        unreachable!()
    }
    async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn force_release_lease(&self, _: &str, _: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn list_workspaces(&self) -> anyhow::Result<Vec<crate::coordinator::CubeWorkspaceStatus>> {
        Ok(Vec::new())
    }
    async fn list_repos(&self) -> anyhow::Result<Vec<crate::coordinator::CubeRepoSummary>> {
        Ok(Vec::new())
    }
}

/// Insert a `conflict_resolutions` row in `running` for the given
/// work item and stamp the parent's `blocked_attempt_id`. Mirrors
/// what Phase 3's worker-spawn path will do at runtime; lets the
/// retire-path tests run without standing up the worker pipeline.
fn install_running_attempt(db: &WorkDb, product_id: &str, work_item_id: &str, pr_url: &str, lease_id: &str) -> String {
    let attempt = db
        .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: product_id.to_owned(),
            work_item_id: work_item_id.to_owned(),
            pr_url: pr_url.to_owned(),
            pr_number: 99,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-sha".into()),
            head_sha_before: Some("head-sha".into()),
        })
        .unwrap()
        .expect("attempt insert returns Some when no row exists yet");
    db.mark_conflict_resolution_running(&attempt.id, lease_id, "ws-1", "worker-1")
        .unwrap()
        .expect("mark_running must flip the freshly-inserted row");
    attempt.id
}

#[tokio::test]
async fn retire_with_running_attempt_releases_lease_and_emits_typed_event() {
    // Install a running attempt (different base sha than the probe) and drive
    // a resolve. The running crz is the most-recent active one so on_resolved
    // picks it up. Lease is released; ConflictResolutionSucceeded fires.
    // Parent was in_review the whole time (from on_conflict_detected which
    // spawned a revision), so no "merge_conflict_resolved" event fires.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/20";
    let (product, chore) = make_in_review(&db, "C-retire", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    // Install a running attempt (separate base-sha so no UNIQUE conflict).
    let attempt_id = install_running_attempt(&db, &product, &chore, pr, "lease-42");

    let cube = Arc::new(RecordingCubeClient::default());
    let resolved = on_resolved(
        &db,
        pub_.as_ref(),
        Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
        &candidate(&product, &chore, pr),
        &[],
        "",
        "",
    )
    .await;
    assert!(resolved, "retire path must return true");

    // Parent stays in_review — it was never blocked in new-model detection.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert!(
                t.blocked_attempt_id.is_none(),
                "blocked_attempt_id must be cleared on retire",
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }
    let attempt_row = db
        .get_conflict_resolution(&attempt_id)
        .unwrap()
        .expect("attempt row must still exist post-retire");
    assert_eq!(attempt_row.status, "succeeded");
    assert!(attempt_row.finished_at.is_some());

    assert_eq!(
        cube.releases.lock().await.as_slice(),
        ["lease-42"],
        "retire path must release the attempt's cube lease",
    );

    // Parent was in_review throughout → no "merge_conflict_resolved" event.
    let work_events = pub_.events.lock().await.clone();
    assert!(
        !work_events.iter().any(|(_, _, r)| r == "merge_conflict_resolved"),
        "merge_conflict_resolved must not fire when parent stayed in_review; got {work_events:?}",
    );

    let typed = pub_.typed_events.lock().await.clone();
    let succeeded_event = typed.iter().find(|(pid, ev)| {
        pid == &product
            && matches!(
                ev,
                FrontendEvent::ConflictResolutionSucceeded { attempt_id: id, .. } if id == &attempt_id
            )
    });
    assert!(
        succeeded_event.is_some(),
        "expected ConflictResolutionSucceeded event with attempt_id={attempt_id}, got {typed:?}",
    );
}

/// New: when the parent was blocked (old-model rows or create_revision failure)
/// AND a running attempt exists, on_resolved flips the parent to in_review
/// and emits merge_conflict_resolved.
#[tokio::test]
async fn retire_with_running_attempt_emits_resolved_when_parent_was_blocked() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/20b";
    let (product, chore) = make_in_review(&db, "C-retire-b", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Use closed_checker to put parent in blocked state.
    let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &closed,
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    let (s, _) = chore_status(&db, &chore);
    assert_eq!(s, TaskStatus::Blocked);

    let attempt_id = install_running_attempt(&db, &product, &chore, pr, "lease-42");
    let cube = Arc::new(RecordingCubeClient::default());
    let resolved = on_resolved(
        &db,
        pub_.as_ref(),
        Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
        &candidate(&product, &chore, pr),
        &[],
        "",
        "",
    )
    .await;
    assert!(resolved);
    let (status, _) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert_eq!(cube.releases.lock().await.as_slice(), ["lease-42"],);
    let work_events = pub_.events.lock().await.clone();
    assert!(
        work_events.iter().any(|(_, _, r)| r == "merge_conflict_resolved"),
        "merge_conflict_resolved must fire when parent was blocked, got {work_events:?}",
    );
    let attempt_row = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
    assert_eq!(attempt_row.status, "succeeded");
}

#[tokio::test]
async fn typed_events_arrive_in_started_then_succeeded_order() {
    // Full conflict-resolve cycle: on_conflict_detected emits
    // ConflictResolutionStarted; on_resolved emits Succeeded; both
    // events carry the same attempt_id.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/25";
    let (product, chore) = make_in_review(&db, "C-evt-order", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // on_conflict_detected creates the attempt and emits Started.
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    let started_attempt_id = {
        let typed = pub_.typed_events.lock().await.clone();
        match typed
            .iter()
            .find(|(_, ev)| matches!(ev, FrontendEvent::ConflictResolutionStarted { .. }))
        {
            Some((_, FrontendEvent::ConflictResolutionStarted { attempt_id, .. })) => attempt_id.clone(),
            other => panic!("expected ConflictResolutionStarted, got {other:?}"),
        }
    };

    let cube = Arc::new(RecordingCubeClient::default());
    on_resolved(
        &db,
        pub_.as_ref(),
        Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
        &candidate(&product, &chore, pr),
        &[],
        "",
        "",
    )
    .await;

    let typed = pub_.typed_events.lock().await.clone();
    let kinds: Vec<&'static str> = typed
        .iter()
        .map(|(_, ev)| match ev {
            FrontendEvent::ConflictResolutionStarted { .. } => "started",
            FrontendEvent::ConflictResolutionSucceeded { .. } => "succeeded",
            FrontendEvent::ConflictResolutionFailed { .. } => "failed",
            FrontendEvent::ConflictResolutionAbandoned { .. } => "abandoned",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["started", "succeeded"],
        "expected started → succeeded ordering, got {kinds:?}",
    );
    for (_, ev) in &typed {
        match ev {
            FrontendEvent::ConflictResolutionStarted { attempt_id: a, .. }
            | FrontendEvent::ConflictResolutionSucceeded { attempt_id: a, .. } => {
                assert_eq!(a, &started_attempt_id, "attempt_id payload must match");
            }
            _ => {}
        }
    }
}

/// Reconciliation path (T791/T898 scenario): parent is in `blocked: merge_conflict`
/// but an active revision is already in flight. The next CONFLICTING probe should
/// flip the parent BACK to `in_review` without spawning a second revision.
#[tokio::test]
async fn rearm_reconciles_blocked_parent_when_revision_is_in_flight() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/20r";
    let (product, chore) = make_in_review(&db, "C-rearm-reconcile", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Simulate the pre-model-change state: parent is blocked AND a revision
    // exists (T898-style). Manually flip to blocked, insert a crz, create a
    // revision, stamp the crz's revision_task_id.
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    let attempt = db
        .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 20,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("abc123".into()),
            head_sha_before: Some("head456".into()),
        })
        .unwrap()
        .expect("fresh insert");
    // Stamp a fake revision_task_id to simulate T898 being active.
    db.set_conflict_resolution_revision_task_id(&attempt.id, "task_fake_revision")
        .unwrap();
    let (s, _) = chore_status(&db, &chore);
    assert_eq!(s, TaskStatus::Blocked, "sanity: parent must be blocked before probe");

    // Now fire on_conflict_detected for the same PR (still CONFLICTING).
    // The re-arm path should find the active revision and reconcile.
    let reconciled = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    assert!(reconciled, "reconciliation must return true (state changed)");
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::InReview,
        "parent must be back in_review after reconcile"
    );
    assert!(reason.is_none());

    // Event emitted is "conflict_revision_in_flight".
    let events = pub_.events.lock().await.clone();
    assert!(
        events.iter().any(|(_, _, r)| r == "conflict_revision_in_flight"),
        "conflict_revision_in_flight event must fire during reconcile, got {events:?}",
    );
    // No second revision was spawned (task_fake_revision is still the only one).
    let all_crz = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(all_crz.len(), 1, "reconcile must not insert a new crz");
}

#[tokio::test]
async fn retire_is_idempotent_on_repeated_probes_with_active_attempt() {
    // Second sweep over a row already retired must NOT re-emit events nor
    // re-release the cube lease. Use closed_checker to put the parent in
    // blocked state (create_revision fails → no fix vehicle), then install
    // a running attempt as the lone active crz. First on_resolved retires
    // that attempt; second finds no active crz → clean no-op.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/21";
    let (product, chore) = make_in_review(&db, "C-retire-idem", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);

    // Use closed_checker: parent goes blocked (create_revision fails, crz abandoned).
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &closed,
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    // Install a running attempt as the lone active crz.
    install_running_attempt(&db, &product, &chore, pr, "lease-99");

    let cube = Arc::new(RecordingCubeClient::default());
    let first = on_resolved(
        &db,
        pub_.as_ref(),
        Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
        &candidate(&product, &chore, pr),
        &[],
        "",
        "",
    )
    .await;
    let second = on_resolved(
        &db,
        pub_.as_ref(),
        Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
        &candidate(&product, &chore, pr),
        &[],
        "",
        "",
    )
    .await;
    assert!(first, "first retire transitions");
    assert!(!second, "second probe must be a no-op");

    assert_eq!(
        cube.releases.lock().await.len(),
        1,
        "lease must be released exactly once across duplicate probes",
    );
    let succeeded_count = pub_
        .typed_events
        .lock()
        .await
        .iter()
        .filter(|(_, ev)| matches!(ev, FrontendEvent::ConflictResolutionSucceeded { .. }))
        .count();
    assert_eq!(
        succeeded_count, 1,
        "ConflictResolutionSucceeded must fire exactly once across duplicate probes",
    );
}

#[tokio::test]
async fn retire_tolerates_lease_release_failure() {
    // Cube release failures during retire must not block the
    // database transitions — the attempt is succeeded, the parent
    // is in_review, and we log + move on.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/22";
    let (product, chore) = make_in_review(&db, "C-retire-leasefail", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    let attempt_id = install_running_attempt(&db, &product, &chore, pr, "lease-zz");

    let cube = Arc::new(RecordingCubeClient::default());
    cube.release_should_fail
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let resolved = on_resolved(
        &db,
        pub_.as_ref(),
        Some(cube.as_ref() as &dyn crate::coordinator::CubeClient),
        &candidate(&product, &chore, pr),
        &[],
        "",
        "",
    )
    .await;
    assert!(resolved, "retire transitions must still report success");
    let attempt_row = db.get_conflict_resolution(&attempt_id).unwrap().unwrap();
    assert_eq!(attempt_row.status, "succeeded");
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
}

#[tokio::test]
async fn detection_emits_started_event_reuses_existing_row_on_same_base_sha() {
    // When on_conflict_detected is called a second time for the same
    // base sha while a revision is in flight, the pre-flight early-exit
    // fires and no new events are emitted (pure no-op). The first call
    // created the attempt and emitted ConflictResolutionStarted; that's
    // the authoritative event. Only one crz row must exist.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/23";
    let (product, chore) = make_in_review(&db, "C-detect-evt", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First call — creates the attempt, spawns revision, parent stays in_review.
    let first = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(first);
    let first_events = pub_.typed_events.lock().await.clone();
    assert_eq!(first_events.len(), 1, "exactly one started event on first call");
    let first_attempt_id = match &first_events[0].1 {
        FrontendEvent::ConflictResolutionStarted { attempt_id, .. } => attempt_id.clone(),
        other => panic!("unexpected event {other:?}"),
    };

    // Second call: same base sha, revision already in flight → pre-flight
    // early-exit. Returns false (no-op), no new typed events.
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(!second, "second probe with active revision must be a no-op");

    // Only one crz row; only one started event.
    let all_started: Vec<_> = pub_
        .typed_events
        .lock()
        .await
        .iter()
        .filter(|(_, ev)| matches!(ev, FrontendEvent::ConflictResolutionStarted { .. }))
        .cloned()
        .collect();
    assert_eq!(all_started.len(), 1, "no second started event from idempotent no-op");
    if let FrontendEvent::ConflictResolutionStarted { attempt_id: a, .. } = &all_started[0].1 {
        assert_eq!(a, &first_attempt_id);
    }
    let crz_count = db
        .list_conflict_resolutions(None, &[], Some(&chore), None)
        .unwrap()
        .len();
    assert_eq!(crz_count, 1, "same base sha must not create a second crz row");
    let _ = (product, first_attempt_id); // silence unused warnings
}

#[tokio::test]
async fn detection_inserts_attempt_and_emits_started_event() {
    // on_conflict_detected inserts the conflict_resolution attempt and emits
    // ConflictResolutionStarted in the same call. Parent stays in_review
    // when revision spawns (no pre-wiring needed for on_resolved to fire).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/24";
    let (product, chore) = make_in_review(&db, "C-detect-noevt", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let transitioned = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(transitioned);

    let attempt = db.active_conflict_resolution_for_work_item(&chore).unwrap();
    assert!(
        attempt.is_some(),
        "on_conflict_detected must insert a conflict_resolution row",
    );
    let attempt = attempt.unwrap();
    assert_eq!(attempt.status, "pending");

    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed.iter().any(|(_, ev)| matches!(
            ev,
            FrontendEvent::ConflictResolutionStarted { attempt_id: a, .. } if a == &attempt.id
        )),
        "ConflictResolutionStarted must fire with the new attempt id, got {typed:?}",
    );
}

#[tokio::test]
async fn detection_defers_when_rebase_attempt_is_active() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/17";
    let (product, chore) = make_in_review(&db, "C-rebase", pr);
    // Simulate auto-rebase having created its side table and a
    // running attempt for this PR. The table doesn't ship until
    // auto-rebase lands, so the conflict_watch must defer when it
    // does exist + has a non-terminal row.
    let conn = rusqlite::Connection::open(dir.path().join("boss.db")).unwrap();
    conn.execute(
        "CREATE TABLE rebase_attempts (
             id                TEXT PRIMARY KEY,
             dependent_pr_url  TEXT NOT NULL,
             status            TEXT NOT NULL
         )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO rebase_attempts (id, dependent_pr_url, status)
          VALUES ('reb_1', ?1, 'running')",
        [pr],
    )
    .unwrap();
    drop(conn);

    let pub_ = Arc::new(RecordingPublisher::default());
    let r = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(!r, "rebase-active path must defer");
    let (status, _) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview, "row stays where it was");
    assert!(pub_.events.lock().await.is_empty());
}

/// Flip `products.auto_pr_maintenance_enabled` directly on the
/// SQLite file so opt-out tests can drive the gate without
/// exposing a setter that production code doesn't yet need.
fn set_product_auto_pr_maintenance(db_path: &std::path::Path, product_id: &str, enabled: bool) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE products SET auto_pr_maintenance_enabled = ?2 WHERE id = ?1",
        rusqlite::params![product_id, if enabled { 1 } else { 0 }],
    )
    .unwrap();
}

// ----- Phase 6 #18: opt-out gates conflict-watch flows -----

#[tokio::test]
async fn detection_skipped_when_product_opt_out_flag_disabled() {
    // Acceptance: an opted-out product's conflict-watch is a no-op.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/600";
    let (product, chore) = make_in_review(&db, "C-optout-prod", pr);
    set_product_auto_pr_maintenance(&db_path, &product, false);

    let pub_ = Arc::new(RecordingPublisher::default());
    let r = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(!r, "opted-out product must not flip to blocked");
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
    assert!(pub_.events.lock().await.is_empty());
}

#[tokio::test]
async fn detection_skipped_when_pr_has_opt_out_label() {
    // Per-PR label is the finer-grained opt-out — even on a
    // product with auto-maintenance enabled, a single labelled PR
    // is left alone.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/601";
    let (product, chore) = make_in_review(&db, "C-optout-label", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let r = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_labels(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            &["boss/no-auto-rebase"],
        ),
    )
    .await;
    assert!(!r, "labelled PR must not flip to blocked");
    let (status, _) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(pub_.events.lock().await.is_empty());
}

#[tokio::test]
async fn opt_out_label_match_is_case_insensitive() {
    // GitHub labels preserve case but the engine tolerates
    // BOSS/No-Auto-Rebase / etc. on the same gate so users don't
    // need to remember exact casing.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/602";
    let (product, chore) = make_in_review(&db, "C-optout-case", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let r = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_labels(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            &["Boss/No-Auto-Rebase"],
        ),
    )
    .await;
    assert!(!r);
}

#[tokio::test]
async fn resolution_skipped_when_product_opt_out_flag_disabled() {
    // Symmetric retire-path gate: an opted-out product's retire
    // is also a no-op so the engine doesn't undo a manual
    // intervention on a row it has stopped auto-managing.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/603";
    let (product, chore) = make_in_review(&db, "C-optout-retire", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Detect conflict with maintenance enabled: new model keeps parent
    // in_review (revision spawned). Then disable maintenance and assert
    // the retire path is a no-op.
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    // New-model: parent stays in_review after detection (revision in flight).
    let (status_before, _) = chore_status(&db, &chore);
    assert_eq!(status_before, TaskStatus::InReview);
    let before = pub_.events.lock().await.len();
    set_product_auto_pr_maintenance(&db_path, &product, false);

    let r = on_resolved(&db, pub_.as_ref(), None, &candidate(&product, &chore, pr), &[], "", "").await;
    assert!(!r, "opted-out product must not retire automatically");
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
    assert_eq!(pub_.events.lock().await.len(), before);
}

// ----- Phase 6 #16: churn guard -----

/// Re-open the SQLite file and back-date a `conflict_resolutions`
/// row's `created_at` so churn-guard tests can simulate "this
/// attempt is 30 minutes old without sleeping the test for 30
/// minutes." Pure plumbing — production code never touches
/// `created_at` after insert.
fn rewind_attempt_created_at(db_path: &std::path::Path, attempt_id: &str, secs_ago: i64) {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let new_ts = (now_secs - secs_ago).to_string();
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE conflict_resolutions SET created_at = ?2 WHERE id = ?1",
        rusqlite::params![attempt_id, new_ts],
    )
    .unwrap();
}

#[tokio::test]
async fn churn_guard_pre_abandons_fourth_attempt_in_window() {
    // Phase 6 #16 acceptance: 4 conflict-resolve cycles in <1h →
    // 4th attempt is abandoned with `churn_threshold_exceeded`.
    // We exercise the WorkDb insert path directly so the test
    // doesn't need to thread through a full worker-spawn cycle.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/700";
    let (product, chore) = make_in_review(&db, "C-churn", pr);
    // Move parent into blocked so the insert path's task-side
    // stamp matches its WHERE guard for the live attempts.
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();

    // First three attempts inside the window go live.
    let make_input = |sha: &str| crate::work::ConflictResolutionInsertInput {
        product_id: product.clone(),
        work_item_id: chore.clone(),
        pr_url: pr.into(),
        pr_number: 700,
        head_branch: "feature".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: Some(sha.into()),
        head_sha_before: Some("head".into()),
    };
    let a1 = db.insert_conflict_resolution(make_input("sha-1")).unwrap().unwrap();
    let a2 = db.insert_conflict_resolution(make_input("sha-2")).unwrap().unwrap();
    let a3 = db.insert_conflict_resolution(make_input("sha-3")).unwrap().unwrap();
    for id in [&a1.id, &a2.id, &a3.id] {
        let row = db.get_conflict_resolution(id).unwrap().unwrap();
        assert_eq!(row.status, "pending", "first three attempts must be live");
        assert!(row.failure_reason.is_none());
    }

    // Fourth attempt — same hour — trips the guard.
    let a4 = db.insert_conflict_resolution(make_input("sha-4")).unwrap().unwrap();
    assert_eq!(
        a4.status, "abandoned",
        "fourth attempt inside the window must be pre-abandoned",
    );
    assert_eq!(
        a4.failure_reason.as_deref(),
        Some("churn_threshold_exceeded"),
        "failure_reason must record the guard",
    );
    assert!(
        a4.finished_at.is_some(),
        "pre-abandoned attempt must carry finished_at so it's terminal",
    );

    // Parent's `blocked_attempt_id` must still point at the
    // most-recent live attempt (a3), not the dead a4.
    match db.get_work_item(&chore).unwrap() {
        crate::work::WorkItem::Chore(t) => {
            assert_eq!(
                t.blocked_attempt_id.as_deref(),
                Some(a3.id.as_str()),
                "blocked_attempt_id must not retarget at the pre-abandoned row",
            );
            assert_eq!(t.status, TaskStatus::Blocked);
            assert_eq!(t.blocked_reason.as_deref(), Some("merge_conflict"));
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn churn_guard_does_not_count_attempts_older_than_window() {
    // The guard's window is rolling-1h. Back-date three attempts
    // to > 1h ago and a brand-new fourth must go live.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/701";
    let (product, chore) = make_in_review(&db, "C-churn-rollover", pr);
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    let make_input = |sha: &str| crate::work::ConflictResolutionInsertInput {
        product_id: product.clone(),
        work_item_id: chore.clone(),
        pr_url: pr.into(),
        pr_number: 701,
        head_branch: "feature".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: Some(sha.into()),
        head_sha_before: Some("head".into()),
    };
    let a1 = db.insert_conflict_resolution(make_input("sha-1")).unwrap().unwrap();
    let a2 = db.insert_conflict_resolution(make_input("sha-2")).unwrap().unwrap();
    let a3 = db.insert_conflict_resolution(make_input("sha-3")).unwrap().unwrap();
    // Push all three outside the 1h window (3700s > 3600s).
    for id in [&a1.id, &a2.id, &a3.id] {
        rewind_attempt_created_at(&db_path, id, 3_700);
    }

    let a4 = db.insert_conflict_resolution(make_input("sha-4")).unwrap().unwrap();
    assert_eq!(
        a4.status, "pending",
        "older-than-window attempts must not contribute to the guard",
    );
}

// ----- Phase 3 cutover: engine-triggered revision as the fix vehicle -----

#[tokio::test]
async fn detection_spawns_revision_and_stamps_attempt() {
    // A genuinely-new conflict creates a `kind=revision` task (parent =
    // chore, merge-conflict provenance), stamps the ledger row's
    // `revision_task_id`, creates NO bespoke conflict_resolution execution,
    // and leaves the parent in `in_review` (new-model parent-state).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/30";
    let (product, chore) = make_in_review(&db, "C-rev-spawn", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    assert!(
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            None,
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await
    );

    // Parent stays in_review — the revision card is the Doing card.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::InReview,
        "parent must stay in Review while revision is in flight"
    );
    assert!(reason.is_none());

    let attempt = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("a pending attempt row must exist");
    assert_eq!(attempt.status, "pending");
    let rev_id = attempt
        .revision_task_id
        .clone()
        .expect("the producer must stamp revision_task_id on the attempt");

    let revision = match db.get_work_item(&rev_id).unwrap() {
        WorkItem::Task(t) => t,
        other => panic!("expected revision task, got {other:?}"),
    };
    assert_eq!(revision.kind, TaskKind::Revision);
    assert_eq!(revision.parent_task_id.as_deref(), Some(chore.as_str()));
    assert_eq!(revision.created_via, format!("merge-conflict:{}", attempt.id));
    assert_eq!(revision.description, "Resolve merge conflict against main");

    // No bespoke conflict_resolution execution: the revision rides the
    // reconcile loop's revision_implementation dispatch instead.
    let ready = db.list_ready_executions().unwrap();
    assert!(
        !ready.iter().any(|e| e.kind == ExecutionKind::ConflictResolution),
        "cutover must not create a conflict_resolution execution; got {ready:?}",
    );
}

#[tokio::test]
async fn detection_idempotent_does_not_double_spawn_revision() {
    // Re-firing on the same base sha reuses the existing attempt (whose
    // revision_task_id is already set) and spawns no second revision.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/31";
    let (product, chore) = make_in_review(&db, "C-rev-idem", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    // Reset to in_review so the second probe re-enters the primary flip
    // path with the same base sha (UNIQUE collision on the ledger).
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("in_review".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(attempts.len(), 1, "same base sha must not stack attempts");
    let revision_backed = attempts.iter().filter(|r| r.revision_task_id.is_some()).count();
    assert_eq!(revision_backed, 1, "exactly one revision-backed attempt");
}

#[tokio::test]
async fn churn_abandoned_attempt_spawns_no_revision() {
    // The 4th conflict in the rolling window is pre-abandoned by the
    // churn guard; the producer's `status == 'pending'` guard means it
    // gets no revision (the cap is enforced before create).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/32";
    let (product, chore) = make_in_review(&db, "C-rev-churn", pr);

    // Three prior attempts in the window arm the guard. Plant them while
    // the chore is still `in_review` so the producer's primary flip path
    // (not the re-arm short-circuit) reaches the insert for the fourth.
    for sha in ["s1", "s2", "s3"] {
        db.insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 32,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some(sha.into()),
            head_sha_before: Some("head".into()),
        })
        .unwrap();
    }

    let pub_ = Arc::new(RecordingPublisher::default());
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        // probe base is "abc123" — a fourth distinct sha in the window.
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    let fourth = db
        .list_conflict_resolutions(None, &[], Some(&chore), None)
        .unwrap()
        .into_iter()
        .find(|r| r.base_sha_at_trigger.as_deref() == Some("abc123"))
        .expect("fourth attempt row must exist");
    assert_eq!(fourth.status, "abandoned");
    assert_eq!(fourth.failure_reason.as_deref(), Some("churn_threshold_exceeded"),);
    assert!(
        fourth.revision_task_id.is_none(),
        "churn-abandoned attempt must spawn no revision",
    );
    // Churn cap = no fix vehicle → parent must be blocked (human-attention terminal).
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::Blocked,
        "churn cap exhausted: parent must be blocked"
    );
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
}

// Helper: build a probe with an explicit head SHA (all other fields match
// the default `probe()` helper).
fn probe_with_head(pr_url: &str, state: PrLifecycleState, head_sha: &str) -> PrLifecycleProbe {
    PrLifecycleProbe {
        head_ref_oid: Some(head_sha.to_owned()),
        ..probe(pr_url, state)
    }
}

// Helper: build a probe with an explicit head SHA and base SHA.
fn probe_with_head_and_base(pr_url: &str, state: PrLifecycleState, head_sha: &str, base_sha: &str) -> PrLifecycleProbe {
    PrLifecycleProbe {
        head_ref_oid: Some(head_sha.to_owned()),
        base_ref_oid: Some(base_sha.to_owned()),
        ..probe(pr_url, state)
    }
}

#[tokio::test]
async fn stale_head_sha_supersedes_pending_crz() {
    // Regression test for T1795 / T1764.
    //
    // Scenario: a crz is spawned for head SHA A.  The revision pushes a
    // commit (head moves to B) but doesn't resolve the conflict; then
    // the exec is abandoned by the orphan sweep (NudgeBreakerParked was
    // never the stop outcome), leaving the crz `pending` with
    // `revision_task_id` set and `head_sha_before = A`.
    //
    // On the next sweep the probe reports head SHA B.  conflict_watch
    // must detect the mismatch, abandon the stale crz, and spawn a
    // fresh resolution against B rather than returning false (no-op).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/40";
    let (product, chore) = make_in_review(&db, "C-stale-sha", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First detection: probe reports head SHA "head-A".  crz spawned, revision
    // created, parent stays in_review.
    let first = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_head(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()), "head-A"),
    )
    .await;
    assert!(first, "first detection must return true");

    let original_crz = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("crz must exist after first detection");
    assert_eq!(original_crz.head_sha_before.as_deref(), Some("head-A"));
    let original_id = original_crz.id.clone();

    // Simulate: revision pushed (head moves to "head-B"), exec abandoned.
    // We leave the crz as `pending` with `revision_task_id` set (the orphan
    // sweep does not call finalize_conflict_resolution_attempt).

    // Second sweep: probe reports head SHA "head-B" (head moved).
    // This must abandon the stale crz and spawn a fresh one.
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_head(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()), "head-B"),
    )
    .await;
    assert!(second, "second probe with new head SHA must re-detect (return true)");

    // Same base SHA (both probes use the default "abc123"): the stale crz
    // is abandoned (base_sha_at_trigger nullified) and a fresh row is
    // inserted with the current head SHA.  Two rows in total:
    // one abandoned (the original) and one pending (the fresh one).
    let all = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(all.len(), 2, "stale crz abandoned, fresh crz created");
    let abandoned = all
        .iter()
        .find(|r| r.id == original_id)
        .expect("original crz must still exist");
    assert_eq!(abandoned.status, "abandoned", "original crz must be abandoned");
    assert_eq!(abandoned.failure_reason.as_deref(), Some("superseded_stale_head"));
    let fresh = all.iter().find(|r| r.id != original_id).expect("fresh crz must exist");
    assert_eq!(fresh.status, "pending", "fresh crz must be pending");
    assert_eq!(
        fresh.head_sha_before.as_deref(),
        Some("head-B"),
        "fresh crz carries the current head SHA"
    );
    assert!(
        fresh.revision_task_id.is_some(),
        "fresh revision must be stamped on the new crz"
    );

    // Parent stays in_review (fresh revision spawned).
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
}

#[tokio::test]
async fn terminal_revision_supersedes_pending_crz_even_without_head_move() {
    // Regression test for the terminal-revision case.
    //
    // Scenario: a crz is spawned for head SHA A.  The revision completes
    // (task moves to in_review) but the execution was abandoned before
    // NudgeBreakerParked fired, so finalize_conflict_resolution_attempt
    // was never called and the crz stays `pending` with `revision_task_id`
    // set.  The head SHA did NOT change.
    //
    // On the next sweep, conflict_watch must detect that the linked
    // revision task is terminal, abandon the stale crz ("revision_terminal"),
    // and spawn a fresh resolution.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/41";
    let (product, chore) = make_in_review(&db, "C-stale-terminal", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First detection: crz spawned, revision created.
    let first = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(first, "first detection must return true");

    let original_crz = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("crz must exist after first detection");
    let original_id = original_crz.id.clone();
    let revision_id = original_crz.revision_task_id.clone().expect("revision must be spawned");

    // Simulate: revision task completed (e.g. moved to in_review) but the
    // crz was never finalised (exec abandoned outside NudgeBreakerParked).
    db.update_work_item(
        &revision_id,
        WorkItemPatch {
            status: Some("in_review".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    // Second sweep: same head SHA — but revision is terminal.
    // Must abandon stale crz and spawn fresh resolution.
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(
        second,
        "second probe with terminal revision must re-detect (return true)"
    );

    // Same base SHA, terminal revision (head didn't move): the stale crz is
    // abandoned (base_sha_at_trigger nullified) and a fresh row inserted.
    // Two rows total: one abandoned (original) and one pending (fresh).
    let all = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(all.len(), 2, "stale crz abandoned, fresh crz created");
    let abandoned = all
        .iter()
        .find(|r| r.id == original_id)
        .expect("original crz must still exist");
    assert_eq!(abandoned.status, "abandoned", "original crz must be abandoned");
    assert_eq!(abandoned.failure_reason.as_deref(), Some("superseded_stale_head"));
    let fresh = all.iter().find(|r| r.id != original_id).expect("fresh crz must exist");
    assert_eq!(fresh.status, "pending", "fresh crz must be pending");
    assert!(
        fresh.revision_task_id.is_some(),
        "fresh revision must be stamped on the new crz"
    );
    // revision_task_id must be different from the original stale revision.
    assert_ne!(
        fresh.revision_task_id.as_deref(),
        Some(revision_id.as_str()),
        "fresh revision must be a new task, not the old stale one",
    );

    // Parent stays in_review (fresh revision spawned).
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
}

#[tokio::test]
async fn stale_head_sha_and_base_advance_supersedes_pending_crz() {
    // Regression test for the real incident path (T1764: crz pending ~11h).
    //
    // Scenario: a crz is spawned for head SHA "head-A" / base SHA "base-1".
    // Over the next ~11 h main advances to "base-2" AND the PR author pushes
    // a new commit ("head-B") without resolving the conflict.  The exec is
    // abandoned by the orphan sweep, leaving the crz `pending` with
    // `revision_task_id` set.
    //
    // On the next sweep the probe reports head="head-B", base="base-2".
    // conflict_watch must:
    //   1. Detect that both head AND base moved.
    //   2. Abandon the stale crz (NOT leave it dangling as `pending`).
    //   3. Spawn a fresh crz+revision against the current (head-B, base-2).
    //   4. Exactly one active crz remaining; the stale row is terminal.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/43";
    let (product, chore) = make_in_review(&db, "C-base-advance", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First detection: probe reports head="head-A", base="base-1".
    let first = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_head_and_base(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            "head-A",
            "base-1",
        ),
    )
    .await;
    assert!(first, "first detection must return true");

    let original_crz = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("crz must exist after first detection");
    assert_eq!(original_crz.head_sha_before.as_deref(), Some("head-A"));
    assert_eq!(original_crz.base_sha_at_trigger.as_deref(), Some("base-1"));
    let original_id = original_crz.id.clone();

    // Simulate: revision pushed (head moves to "head-B"), exec abandoned;
    // meanwhile main advanced to "base-2".  The crz stays `pending` with
    // `revision_task_id` set — finalize_conflict_resolution_attempt was
    // never called by the orphan sweep.

    // Second sweep: both head AND base moved.
    // Must abandon the stale crz and spawn a fresh one.
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_head_and_base(
            pr,
            PrLifecycleState::Open(OpenPrStatus::conflict_only()),
            "head-B",
            "base-2",
        ),
    )
    .await;
    assert!(
        second,
        "second probe with new head+base SHA must re-detect (return true)"
    );

    // Stale row abandoned; fresh row inserted with the new (work_item_id, base-2) key.
    // Exactly two rows: one terminal (the old one) and one pending (the new one).
    let all = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(
        all.len(),
        2,
        "stale crz abandoned, fresh crz inserted with new base SHA"
    );

    let abandoned = all
        .iter()
        .find(|r| r.id == original_id)
        .expect("original crz must still exist");
    assert_eq!(
        abandoned.status, "abandoned",
        "original crz must be abandoned, not left pending"
    );
    assert_eq!(abandoned.failure_reason.as_deref(), Some("superseded_stale_head"));
    // base_sha_at_trigger is untouched on a base-changed abandon (the row is
    // purely terminal; its key slot is not reused).
    assert_eq!(abandoned.base_sha_at_trigger.as_deref(), Some("base-1"));

    let fresh = all.iter().find(|r| r.id != original_id).expect("fresh crz must exist");
    assert_eq!(fresh.status, "pending", "fresh crz must be pending");
    assert_eq!(
        fresh.base_sha_at_trigger.as_deref(),
        Some("base-2"),
        "fresh crz uses the new base SHA"
    );
    assert_eq!(
        fresh.head_sha_before.as_deref(),
        Some("head-B"),
        "fresh crz uses the new head SHA"
    );
    assert!(fresh.revision_task_id.is_some(), "fresh revision must be spawned");

    // Parent stays in_review (fresh revision in flight).
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
}

#[tokio::test]
async fn live_revision_same_head_sha_remains_no_op() {
    // Idempotency guard: if the crz's head SHA matches the current probe
    // and the revision task is still live, the pre-flight must NOT supersede.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/42";
    let (product, chore) = make_in_review(&db, "C-noop-live", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // First detection: crz + revision spawned.
    let first = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(first);

    // Second probe: same head SHA ("head456"), revision still live (todo/active).
    let second = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(!second, "same head + live revision must remain a no-op (false)");

    // Only the original crz exists — no supersede.
    let all = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(
        all.len(),
        1,
        "no second crz must be created when revision is still live"
    );
    assert_eq!(all[0].status, "pending");
}

#[tokio::test]
async fn create_revision_failure_abandons_attempt() {
    // When the create-time gate refuses (parent PR no longer open, R4),
    // the producer marks the ledger row `abandoned` so it never strands
    // as a pending attempt with no fix vehicle, and spawns no revision.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/33";
    let (product, chore) = make_in_review(&db, "C-rev-fail", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &closed,
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    // The parent flip precedes the gate, so the chore is still blocked;
    // the poller's merged/closed handling reconciles it on a later sweep.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("merge_conflict"));

    let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].status, "abandoned");
    assert_eq!(attempts[0].failure_reason.as_deref(), Some("revision_create_failed"),);
    assert!(attempts[0].revision_task_id.is_none());
}

/// T2381/PR#1861 regression: a row that another watcher flipped to
/// `blocked: ci_failure` and never returned to `in_review` (the exact
/// orphan the ci_watch merge-queue-rebounce gap used to produce before
/// its `unblock_for_revision` fix) must not be permanently invisible to
/// `conflict_watch`. When the live probe reports CONFLICTING, the
/// foreign-bucket takeover must re-bucket the row into
/// `blocked: merge_conflict`, supersede the stale `ci_remediations`
/// attempt (so it doesn't strand a "ci failing" badge forever), and
/// spawn a conflict-resolution revision like any other fresh detection.
#[tokio::test]
async fn foreign_bucket_takeover_rebuckets_stuck_ci_failure_row_to_conflict() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1861";
    let (product, chore) = make_in_review(&db, "C-t2381", pr);

    // Simulate the pre-fix orphan: the row is stuck `blocked: ci_failure`
    // with a still-active `ci_remediations` attempt, and NO merge_conflict
    // signal was ever recorded (conflict_watch has never touched this row).
    db.mark_chore_blocked_ci_failure(&chore, pr, None).unwrap();
    let stale_attempt = db
        .insert_ci_remediation(crate::work::CiRemediationInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.to_owned(),
            pr_number: 1861,
            head_branch: "feature".to_owned(),
            head_sha_at_trigger: "synthetic-merge-sha".to_owned(),
            attempt_kind: "fix".to_owned(),
            consumes_budget: 1,
            failed_checks: "[]".to_owned(),
            failure_kind: "merge_queue_rebounce".to_owned(),
            before_commit_sha: Some("synthetic-merge-sha".to_owned()),
        })
        .unwrap()
        .expect("fresh insert");
    assert_eq!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .map(|s| s.reason.clone())
            .collect::<Vec<_>>(),
        vec!["ci_failure".to_owned()],
        "precondition: only the ci_failure signal is active, no merge_conflict — \
         the orphan this fix targets (conflict_watch never touched this row)",
    );

    let pub_ = Arc::new(RecordingPublisher::default());
    let took_over = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(took_over, "conflict_watch must take the orphaned row over");

    // The stale ci_remediations attempt must be superseded, not left
    // dangling (it would otherwise strand a phantom "ci failing" badge
    // forever — no retire path ever marks a merge_queue_rebounce attempt
    // succeeded from a clean head-branch probe).
    let ci_attempt = db.active_ci_remediation_for_work_item(&chore).unwrap();
    assert!(
        ci_attempt.is_none(),
        "stale ci_remediations attempt must be superseded (abandoned), not left active"
    );
    let refreshed_stale = db
        .get_ci_remediation(&stale_attempt.id)
        .unwrap()
        .expect("row still exists");
    assert_eq!(refreshed_stale.status, "abandoned");

    // A conflict_resolutions attempt must now exist and the parent must
    // be either `blocked: merge_conflict` (no fix vehicle) or back in
    // `in_review` with the fix revision running — never still `ci_failure`.
    let crz = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(crz.len(), 1, "a fresh conflict_resolutions attempt must be created");
    let (status, reason) = chore_status(&db, &chore);
    assert!(
        reason.as_deref() != Some("ci_failure"),
        "row must no longer be stuck on the foreign ci_failure reason"
    );
    match status {
        TaskStatus::InReview => assert!(reason.is_none(), "in_review parent must have no blocked_reason"),
        TaskStatus::Blocked => assert_eq!(reason.as_deref(), Some("merge_conflict")),
        other => panic!("unexpected status after takeover: {other:?}"),
    }
}

/// A row blocked on a genuinely higher-priority foreign reason (design
/// §Q2: dependency > review_feedback > merge_conflict > ci_failure) must
/// NOT be taken over by conflict_watch even when the live probe reports
/// CONFLICTING — that reason's own watcher still owns the row.
#[tokio::test]
async fn foreign_bucket_takeover_declines_higher_priority_reason() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1862";
    let (product, chore) = make_in_review(&db, "C-higher-prio", pr);
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("blocked".into()),
            blocked_reason: Some("dependency".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let pub_ = Arc::new(RecordingPublisher::default());
    let took_over = on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    assert!(
        !took_over,
        "conflict_watch must not steal a higher-priority foreign block"
    );

    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(
        reason.as_deref(),
        Some("dependency"),
        "dependency block must be untouched"
    );
    let crz = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert!(crz.is_empty(), "no conflict_resolutions attempt must be created");
}
