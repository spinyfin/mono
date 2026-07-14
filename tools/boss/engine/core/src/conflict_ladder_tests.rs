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

    assert_eq!(
        outcome,
        LadderOutcome::FellThrough {
            residual_conflict_files: Some(1)
        }
    );
    assert!(
        rung2_eligible(Some(1)),
        "a single residual file must be rung-2 eligible"
    );
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
async fn rung1_residual_conflicts_beyond_rung2_bound_declines_rung2() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, _chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = ScriptCube::new(Script::Conflicts(vec![
        "src/a.rs 2-sided conflict".to_owned(),
        "src/b.rs 2-sided conflict".to_owned(),
    ]));

    let outcome = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate, &attempt).await;

    assert_eq!(
        outcome,
        LadderOutcome::FellThrough {
            residual_conflict_files: Some(2)
        }
    );
    assert!(
        !rung2_eligible(Some(2)),
        "two residual files must exceed the (conservative, single-file) rung-2 bound and decline to rung 3"
    );
}

#[tokio::test]
async fn rung1_rebase_error_falls_through_and_releases() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, _chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = ScriptCube::new(Script::RebaseErrors);

    let outcome = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate, &attempt).await;

    assert_eq!(
        outcome,
        LadderOutcome::FellThrough {
            residual_conflict_files: None
        }
    );
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
    assert_eq!(
        outcome,
        LadderOutcome::FellThrough {
            residual_conflict_files: None
        }
    );
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

    assert_eq!(
        outcome,
        LadderOutcome::FellThrough {
            residual_conflict_files: None
        }
    );
    // Never leased (ensure_repo failed first), so nothing to release, no goto.
    assert!(cube.gotos.lock().await.is_empty());
    assert!(cube.released.lock().await.is_empty());
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "pending");
}

#[tokio::test]
async fn rung0_stays_off_even_for_a_resolvable_residual_file_hard_gate() {
    // RUNG0_APPLY_LIVE is false (see its doc comment: gated on T2562's
    // result-gate landing) — even though "Cargo.lock" has a registered
    // resolver, try_mechanical_rungs must not attempt rung 0 today; a
    // residual Cargo.lock conflict falls straight through to the worker
    // path exactly like any other unresolvable residual conflict.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = ScriptCube::new(Script::Conflicts(vec!["Cargo.lock".to_owned()]));

    let outcome = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate, &attempt).await;

    assert_eq!(
        outcome,
        LadderOutcome::FellThrough {
            residual_conflict_files: Some(1)
        }
    );
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "pending");
    assert_eq!(row.resolved_by_rung, None);
    let (_status, reason) = chore_state(&db, &chore_id);
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
}

// ─────────────────────────── attempt_rung0 ────────────────────────────
//
// `attempt_rung0` is called directly here, bypassing the RUNG0_APPLY_LIVE
// gate proven off above, so these tests exercise the rung's actual
// mechanics: feeding residual files to the deterministic-resolver
// registry, landing an all-resolved result via `push_resolution`, and
// declining (leaving the attempt untouched) when any file can't be
// resolved or the push fails.

/// [`CubeClient`] double for direct `attempt_rung0` tests. Only
/// `push_resolution` is exercised — `attempt_rung0` never calls any other
/// method, so everything else stays `unimplemented!()` via the macro
/// default.
struct Rung0Cube {
    push_ok: bool,
    pushes: Mutex<Vec<(PathBuf, u64)>>,
}

impl Rung0Cube {
    fn new(push_ok: bool) -> Self {
        Self {
            push_ok,
            pushes: Mutex::new(Vec::new()),
        }
    }
}

crate::stub_cube_client! { Rung0Cube {
    async fn push_resolution(&self, workspace_path: &std::path::Path, pr: u64) -> Result<()> {
        self.pushes.lock().await.push((workspace_path.to_path_buf(), pr));
        if self.push_ok {
            Ok(())
        } else {
            anyhow::bail!("push boom");
        }
    }
} }

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Write a workspace fixture with a conflicted `Cargo.lock` that
/// `CargoLockResolver` (a `ResolverRegistry::with_builtins()` built-in) can
/// regenerate cleanly against the sibling `Cargo.toml`.
fn write_resolvable_cargo_lock_fixture(ws: &std::path::Path, package_name: &str) {
    std::fs::write(
        ws.join("Cargo.toml"),
        format!("[package]\nname = \"{package_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    std::fs::create_dir(ws.join("src")).unwrap();
    std::fs::write(ws.join("src").join("lib.rs"), "").unwrap();
    std::fs::write(
        ws.join("Cargo.lock"),
        "<<<<<<< ours\ngarbage\n=======\n>>>>>>> theirs\n",
    )
    .unwrap();
}

#[tokio::test]
async fn rung0_all_resolved_pushes_and_retires_at_rung_0() {
    // Exercises the real built-in CargoLockResolver (ResolverRegistry has no
    // injection seam for a fake one here), so this needs a real `cargo` —
    // skip gracefully where it's absent, mirroring
    // `boss_deterministic_resolvers`'s own real-execution test.
    if which("cargo").is_none() {
        eprintln!("skipping: cargo not on PATH");
        return;
    }
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());

    let ws = tempdir().unwrap();
    write_resolvable_cargo_lock_fixture(ws.path(), "fixture-r0");
    let lease = CubeWorkspaceLease {
        lease_id: "lease-r0".to_owned(),
        workspace_id: "ws-r0".to_owned(),
        workspace_path: ws.path().to_path_buf(),
    };
    let cube = Rung0Cube::new(true);

    let outcome = attempt_rung0(
        &db,
        pub_.as_ref(),
        &cube,
        &candidate,
        &attempt,
        &lease,
        &["Cargo.lock".to_owned()],
    )
    .await;

    assert_eq!(outcome, LadderOutcome::Retired);
    // Attempt retired at rung 0.
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "succeeded");
    assert_eq!(row.resolved_by_rung, Some(0));
    // Parent back in Review, unblocked — same retire contract as rung 1.
    let (status, reason) = chore_state(&db, &chore_id);
    assert_eq!(status, "InReview");
    assert!(reason.is_none(), "block should be cleared, got {reason:?}");
    // Landed via push_resolution, not any other verb.
    assert_eq!(*cube.pushes.lock().await, vec![(ws.path().to_path_buf(), 42)]);
    let regenerated = std::fs::read_to_string(ws.path().join("Cargo.lock")).unwrap();
    assert!(!regenerated.contains("<<<<<<<"), "conflict markers must be gone");
    // Success event published, same as rung 1.
    let typed = pub_.typed_events.lock().await;
    assert!(
        typed
            .iter()
            .any(|(_, e)| matches!(e, FrontendEvent::ConflictResolutionSucceeded { .. })),
        "expected ConflictResolutionSucceeded"
    );
}

#[tokio::test]
async fn rung0_declined_file_falls_through_without_pushing() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, _chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    let lease = CubeWorkspaceLease {
        lease_id: "lease-r0b".to_owned(),
        workspace_id: "ws-r0b".to_owned(),
        workspace_path: PathBuf::from("/tmp/rung0-declined"),
    };
    let cube = Rung0Cube::new(true);

    // No registered resolver claims an ordinary source file.
    let outcome = attempt_rung0(
        &db,
        pub_.as_ref(),
        &cube,
        &candidate,
        &attempt,
        &lease,
        &["src/lib.rs".to_owned()],
    )
    .await;

    assert_eq!(
        outcome,
        LadderOutcome::FellThrough {
            residual_conflict_files: None
        }
    );
    assert!(
        cube.pushes.lock().await.is_empty(),
        "must not push when any residual file is declined"
    );
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "pending");
    assert_eq!(row.resolved_by_rung, None);
}

#[tokio::test]
async fn rung0_push_failure_falls_through_without_marking_succeeded() {
    if which("cargo").is_none() {
        eprintln!("skipping: cargo not on PATH");
        return;
    }
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());

    let ws = tempdir().unwrap();
    write_resolvable_cargo_lock_fixture(ws.path(), "fixture-r0-pf");
    let lease = CubeWorkspaceLease {
        lease_id: "lease-r0c".to_owned(),
        workspace_id: "ws-r0c".to_owned(),
        workspace_path: ws.path().to_path_buf(),
    };
    let cube = Rung0Cube::new(false);

    let outcome = attempt_rung0(
        &db,
        pub_.as_ref(),
        &cube,
        &candidate,
        &attempt,
        &lease,
        &["Cargo.lock".to_owned()],
    )
    .await;

    assert_eq!(
        outcome,
        LadderOutcome::FellThrough {
            residual_conflict_files: None
        }
    );
    // Every file resolved, but the push failed — must not mark succeeded.
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "pending");
    assert_eq!(row.resolved_by_rung, None);
    let (_status, reason) = chore_state(&db, &chore_id);
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
}
