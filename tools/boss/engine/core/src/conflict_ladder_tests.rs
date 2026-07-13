use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use boss_protocol::FrontendEvent;
use tempfile::tempdir;
use tokio::sync::Mutex;

use super::*;
use crate::coordinator::{CubeRepoHandle, CubeWorkspaceLease, RebaseOutcome};
use crate::test_support::*;
use crate::work::{
    ConflictResolution, ConflictResolutionInsertInput, PendingMergeCheck, WorkDb, WorkItem, WorkItemPatch,
};

/// What the scripted cube double's `rebase_workspace` returns.
#[derive(Clone)]
enum Script {
    CleanPushed,
    CleanUnpushed,
    Conflicts(Vec<String>),
    EnsureRepoErrors,
    RebaseErrors,
}

/// Configurable [`CubeClient`] double for the rung-1 harness. Records the
/// gotos it was asked for and the leases it released so tests can assert the
/// workspace is always cleaned up.
struct ScriptCube {
    script: Script,
    released: Mutex<Vec<String>>,
    gotos: Mutex<Vec<u64>>,
    rebases: Mutex<Vec<u64>>,
}

impl ScriptCube {
    fn new(script: Script) -> Self {
        Self {
            script,
            released: Mutex::new(Vec::new()),
            gotos: Mutex::new(Vec::new()),
            rebases: Mutex::new(Vec::new()),
        }
    }
}

crate::stub_cube_client! { ScriptCube {
    async fn ensure_repo(&self, _origin: &str) -> Result<CubeRepoHandle> {
        if matches!(self.script, Script::EnsureRepoErrors) {
            anyhow::bail!("ensure_repo boom");
        }
        Ok(CubeRepoHandle { repo_id: "repo-1".to_owned() })
    }
    async fn lease_workspace(
        &self,
        _repo_id: &str,
        _task: &str,
        _prefer: Option<&str>,
        _allow_dirty: bool,
        _exclude: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        Ok(CubeWorkspaceLease {
            lease_id: "lease-1".to_owned(),
            workspace_id: "ws-1".to_owned(),
            workspace_path: PathBuf::from("/tmp/ladder-ws"),
        })
    }
    async fn goto_workspace(&self, _workspace_path: &std::path::Path, pr: u64) -> Result<()> {
        self.gotos.lock().await.push(pr);
        Ok(())
    }
    async fn rebase_workspace(&self, _workspace_path: &std::path::Path, pr: u64) -> Result<RebaseOutcome> {
        self.rebases.lock().await.push(pr);
        match &self.script {
            Script::CleanPushed => Ok(RebaseOutcome { clean: true, pushed: true, conflicted_files: Vec::new() }),
            Script::CleanUnpushed => Ok(RebaseOutcome { clean: true, pushed: false, conflicted_files: Vec::new() }),
            Script::Conflicts(files) => {
                Ok(RebaseOutcome { clean: false, pushed: false, conflicted_files: files.clone() })
            }
            Script::RebaseErrors => anyhow::bail!("rebase boom"),
            Script::EnsureRepoErrors => unreachable!("ensure_repo already errored"),
        }
    }
    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        self.released.lock().await.push(lease_id.to_owned());
        Ok(())
    }
} }

const PR: &str = "https://github.com/foo/bar/pull/42";

/// Build an in-review chore blocked on merge_conflict (mirroring the upfront
/// flip `on_conflict_detected` performs) plus a fresh pending
/// `conflict_resolutions` attempt, and return the pieces the harness needs.
fn blocked_with_attempt(db: &WorkDb) -> (PendingMergeCheck, ConflictResolution, String) {
    let product = create_test_product_with_repo(db, "LadderProd", Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore_manual(db, product.id.clone(), "ladder-chore");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(PR.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    // Sanity: the fixture must resolve a repo, or rung 1 can't lease.
    assert!(
        db.resolve_repo_for_task(&chore.id).unwrap().is_some(),
        "test fixture chore must resolve a repo_remote_url"
    );
    // Upfront flip to blocked, as `on_conflict_detected` does before the harness.
    db.mark_chore_blocked_merge_conflict(&chore.id, PR).unwrap();
    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore.id.clone(),
            pr_url: PR.to_owned(),
            pr_number: 42,
            head_branch: "feature".to_owned(),
            base_branch: "main".to_owned(),
            base_sha_at_trigger: Some("base111".to_owned()),
            head_sha_before: Some("head222".to_owned()),
        })
        .unwrap()
        .expect("fresh attempt row");
    let candidate = PendingMergeCheck {
        work_item_id: chore.id.clone(),
        product_id: product.id.clone(),
        pr_url: PR.to_owned(),
    };
    (candidate, attempt, chore.id)
}

fn chore_state(db: &WorkDb, id: &str) -> (String, Option<String>) {
    match db.get_work_item(id).unwrap() {
        WorkItem::Chore(t) => (format!("{:?}", t.status), t.blocked_reason),
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn rung1_clean_rebase_retires_attempt_at_rung_1_with_no_worker() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = ScriptCube::new(Script::CleanPushed);

    let outcome = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate, &attempt).await;

    assert_eq!(outcome, LadderOutcome::Retired);
    // Attempt retired at rung 1.
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "succeeded");
    assert_eq!(row.resolved_by_rung, Some(1));
    // Parent back in Review, unblocked.
    let (status, reason) = chore_state(&db, &chore_id);
    assert_eq!(status, "InReview");
    assert!(reason.is_none(), "block should be cleared, got {reason:?}");
    // Workspace positioned on the PR, rebased, and released.
    assert_eq!(*cube.gotos.lock().await, vec![42]);
    assert_eq!(*cube.rebases.lock().await, vec![42]);
    assert_eq!(*cube.released.lock().await, vec!["lease-1".to_owned()]);
    // Success event published.
    let typed = pub_.typed_events.lock().await;
    assert!(
        typed
            .iter()
            .any(|(_, e)| matches!(e, FrontendEvent::ConflictResolutionSucceeded { .. })),
        "expected ConflictResolutionSucceeded"
    );
    drop(typed);
    assert!(
        pub_.lifecycle_reasons()
            .await
            .contains(&"merge_conflict_resolved".to_owned())
    );
}

#[tokio::test]
async fn rung1_residual_conflicts_falls_through_and_releases() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = ScriptCube::new(Script::Conflicts(vec!["src/lib.rs 2-sided conflict".to_owned()]));

    let outcome = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate, &attempt).await;

    assert_eq!(outcome, LadderOutcome::FellThrough);
    // Attempt untouched (still pending) — the worker path will drive it.
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "pending");
    assert_eq!(row.resolved_by_rung, None);
    // Parent still blocked (harness did not retire).
    let (_status, reason) = chore_state(&db, &chore_id);
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
    // Lease still released.
    assert_eq!(*cube.released.lock().await, vec!["lease-1".to_owned()]);
}

#[tokio::test]
async fn rung1_rebase_error_falls_through_and_releases() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, _chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = ScriptCube::new(Script::RebaseErrors);

    let outcome = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate, &attempt).await;

    assert_eq!(outcome, LadderOutcome::FellThrough);
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "pending");
    // Leased then released even though the rebase blew up.
    assert_eq!(*cube.released.lock().await, vec!["lease-1".to_owned()]);
}

#[tokio::test]
async fn rung1_clean_but_unpushed_falls_through() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = ScriptCube::new(Script::CleanUnpushed);

    let outcome = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate, &attempt).await;

    // Never retire on an unpushed branch — the PR wasn't updated.
    assert_eq!(outcome, LadderOutcome::FellThrough);
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "pending");
    let (_status, reason) = chore_state(&db, &chore_id);
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
    assert_eq!(*cube.released.lock().await, vec!["lease-1".to_owned()]);
}

#[tokio::test]
async fn rung1_ensure_repo_error_falls_through_without_lease() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, _chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = ScriptCube::new(Script::EnsureRepoErrors);

    let outcome = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate, &attempt).await;

    assert_eq!(outcome, LadderOutcome::FellThrough);
    // Never leased (ensure_repo failed first), so nothing to release, no goto.
    assert!(cube.gotos.lock().await.is_empty());
    assert!(cube.released.lock().await.is_empty());
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "pending");
}
