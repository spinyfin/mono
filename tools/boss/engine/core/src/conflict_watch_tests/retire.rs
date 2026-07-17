//! The retire path's side effects: releasing the attempt's cube lease,
//! emitting the typed `ConflictResolution*` events in order, staying
//! idempotent across duplicate probes, and tolerating a lease-release
//! failure.

use std::sync::Arc;

use async_trait::async_trait;
use tempfile::tempdir;
use tokio::sync::Mutex;

use super::super::*;
use super::helpers::*;
use crate::merge_poller::{OpenPrStatus, PrLifecycleState};
use crate::test_support::*;
use crate::work::{WorkDb, WorkItem};

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
