//! Escalation-ladder rungs: what `on_conflict_detected` does with a live
//! `CubeClient` — rung 1's engine-direct rebase, rung 2's bounded-residue
//! small-agent profile, its decline to rung 3, and the lease-unavailable path.

use std::sync::Arc;

use tempfile::tempdir;
use tokio::sync::Mutex;

use super::super::*;
use super::helpers::*;
use crate::coordinator::{CubeRepoHandle, CubeWorkspaceLease, RebaseOutcome};
use crate::merge_poller::{OpenPrStatus, PrLifecycleState};
use crate::test_support::*;
use crate::work::{WorkDb, WorkItem};

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
/// with the small-agent profile (`effort_level = trivial`) and stamps the
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
                Some(boss_protocol::EffortLevel::Trivial),
                "rung 2's revision must use the small-agent profile (trivial: cheaper than \
                 rung 3's un-overridden `small` default, while still respecting the #746 \
                 no-Haiku floor)"
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
    assert_eq!(
        active.resolved_by_rung, None,
        "a declined rung 2 must not stamp resolved_by_rung (defaults to 3 at actual completion)"
    );
    let revision_id = active
        .revision_task_id
        .clone()
        .expect("rung 3 must still spawn a worker revision");
    // Rung 3's fallback leaves `effort_level` unset on `CreateRevisionInput`;
    // `default_revision_effort_level` resolves an un-overridden plain-chore
    // root to `small` — genuinely distinct from rung 2's explicit `trivial`
    // override (see `conflict_with_bounded_residue_spawns_rung2_small_agent_revision`),
    // so `effort_level` is now itself a real discriminator between the two
    // paths, not just `resolved_by_rung`.
    match db.get_work_item(&revision_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => {
            assert_eq!(
                t.effort_level,
                Some(boss_protocol::EffortLevel::Small),
                "rung 3's fallback must use the ordinary small default, not rung 2's trivial profile"
            );
        }
        other => panic!("expected a task-shaped revision, got {other:?}"),
    }
}

/// A [`CubeClient`] double whose `lease_workspace` always errors — drives the
/// rung-1 lease-unavailable path (the T501/flunge PR #917 incident: a
/// transient cube dirty-reclaim refusal on both attempts).
struct LeaseFailsCube {
    lease_calls: Mutex<u32>,
}

impl LeaseFailsCube {
    fn new() -> Self {
        Self {
            lease_calls: Mutex::new(0),
        }
    }
}

crate::stub_cube_client! { LeaseFailsCube {
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
        *self.lease_calls.lock().await += 1;
        anyhow::bail!("lease boom");
    }
} }

/// Escalation ladder (T4/incident defense-in-depth): when rung 1's
/// workspace lease fails on both the initial attempt and the retry, the
/// ladder must NOT spawn a full-worker (rung 3) revision this tick — that
/// was the actual incident: a pure infra hiccup escalated straight to the
/// most expensive rung. The attempt stays `pending` with no
/// `revision_task_id`, and the parent stays `blocked: merge_conflict`
/// (already flipped upfront) so the next `conflict_watch` tick retries the
/// ladder from scratch instead of a worker being spawned on a stale signal.
#[tokio::test]
async fn conflict_with_lease_unavailable_spawns_no_worker_this_tick() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/81";
    let (product, chore) = make_in_review(&db, "C-lease-unavailable", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = LeaseFailsCube::new();

    let transitioned = on_conflict_detected(
        &db,
        pub_.as_ref(),
        Some(&cube),
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    assert!(transitioned, "the upfront blocked-flip is itself a state change");
    // Retried exactly once (two total attempts), not spawned in a loop.
    assert_eq!(*cube.lease_calls.lock().await, 2);
    // No worker spawned: the attempt is live but has no revision vehicle.
    let active = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("attempt row must still exist, pending a retry");
    assert_eq!(active.status, "pending");
    assert!(
        active.revision_task_id.is_none(),
        "a lease-unavailable tick must not spawn a full-worker revision"
    );
    // Parent stays blocked (no tractable fix vehicle this tick) rather than
    // returning to Review with a phantom in-flight revision.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
}
