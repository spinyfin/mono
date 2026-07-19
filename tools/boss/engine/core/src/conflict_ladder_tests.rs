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
///
/// Constructed via the `new`/`with_*` associated functions below rather than
/// the generated builder — `#[derive(bon::Builder)]` is present solely to
/// satisfy the project's giant-struct convention (7 named fields).
#[derive(bon::Builder)]
#[builder(on(String, into))]
struct ScriptCube {
    script: Script,
    /// T9/T2562: what `verify_deletion_tripwire` should report. Empty
    /// (the default via [`Self::new`]) means "clean" — no network call is
    /// made since this is a scripted double, not `CommandCubeClient`.
    tripwire_findings: Vec<String>,
    /// Workspace path handed back by `lease_workspace`. Defaults to a
    /// non-existent fixed path (fine for tests that never reach rung 0's
    /// real resolvers); tests exercising a *live* rung 0 through
    /// `try_mechanical_rungs` point this at a real fixture directory via
    /// [`Self::with_workspace_path`].
    workspace_path: PathBuf,
    /// Number of leading `lease_workspace` calls to fail before succeeding —
    /// drives the rung-1 lease-retry tests (`lease_rung1_workspace`).
    /// Defaults to 0 (never fails).
    lease_failures: u32,
    /// When `Some((db_path, attempt_id))`, `rebase_workspace` opens a fresh
    /// `WorkDb` at `db_path` and snapshots the attempt's live
    /// `mechanical_rung_in_flight` marker into
    /// [`Self::marker_seen_during_rebase`] — used to prove the in-flight
    /// marker is durably persisted *while* a mechanical rung is executing.
    peek: Option<(PathBuf, String)>,
    lease_calls: Mutex<u32>,
    released: Mutex<Vec<String>>,
    gotos: Mutex<Vec<u64>>,
    rebases: Mutex<Vec<u64>>,
    pushes: Mutex<Vec<u64>>,
    marker_seen_during_rebase: Mutex<Option<Option<i64>>>,
}

impl ScriptCube {
    fn new(script: Script) -> Self {
        Self {
            script,
            tripwire_findings: Vec::new(),
            workspace_path: PathBuf::from("/tmp/ladder-ws"),
            lease_failures: 0,
            peek: None,
            lease_calls: Mutex::new(0),
            released: Mutex::new(Vec::new()),
            gotos: Mutex::new(Vec::new()),
            rebases: Mutex::new(Vec::new()),
            pushes: Mutex::new(Vec::new()),
            marker_seen_during_rebase: Mutex::new(None),
        }
    }

    /// A `CleanPushed` double that snapshots the attempt's
    /// `mechanical_rung_in_flight` marker mid-rebase (i.e. while rung 1 is
    /// in flight) into [`Self::marker_seen_during_rebase`].
    fn with_peek(db_path: PathBuf, attempt_id: String) -> Self {
        Self {
            peek: Some((db_path, attempt_id)),
            ..Self::new(Script::CleanPushed)
        }
    }

    fn with_tripwire_findings(script: Script, findings: Vec<String>) -> Self {
        Self {
            tripwire_findings: findings,
            ..Self::new(script)
        }
    }

    fn with_workspace_path(script: Script, workspace_path: PathBuf) -> Self {
        Self {
            workspace_path,
            ..Self::new(script)
        }
    }

    fn with_lease_failures(script: Script, lease_failures: u32) -> Self {
        Self {
            lease_failures,
            ..Self::new(script)
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
        let mut calls = self.lease_calls.lock().await;
        *calls += 1;
        if *calls <= self.lease_failures {
            anyhow::bail!("lease boom (call {calls})");
        }
        Ok(CubeWorkspaceLease {
            lease_id: "lease-1".to_owned(),
            workspace_id: "ws-1".to_owned(),
            workspace_path: self.workspace_path.clone(),
        })
    }
    async fn goto_workspace(&self, _workspace_path: &std::path::Path, pr: u64) -> Result<()> {
        self.gotos.lock().await.push(pr);
        Ok(())
    }
    async fn rebase_workspace(&self, _workspace_path: &std::path::Path, pr: u64) -> Result<RebaseOutcome> {
        self.rebases.lock().await.push(pr);
        if let Some((db_path, attempt_id)) = &self.peek {
            // Rung 1 stamped the in-flight marker before calling us; snapshot
            // it through a fresh handle to prove it is durably on disk while
            // the rung is executing.
            let peek_db = WorkDb::open(db_path.clone()).unwrap();
            let marker = peek_db.get_conflict_resolution(attempt_id).unwrap().unwrap().mechanical_rung_in_flight;
            *self.marker_seen_during_rebase.lock().await = Some(marker);
        }
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
    async fn push_resolution(&self, _workspace_path: &std::path::Path, pr: u64) -> Result<()> {
        self.pushes.lock().await.push(pr);
        Ok(())
    }
    async fn verify_deletion_tripwire(
        &self,
        _repo_slug: &str,
        _head_before: &str,
        _base_sha: &str,
        _pr_number: u64,
    ) -> Vec<String> {
        self.tripwire_findings.clone()
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
async fn rung1_deletion_tripwire_hit_halts_for_signoff_instead_of_retiring() {
    // T9/T2562: a clean, pushed rung-1 rebase whose result the deletion
    // tripwire flags must NOT auto-retire as a success — it must halt
    // identically to how a worker-driven (rung 2/3) resolution halts on
    // the same tripwire: blocked:deletion_signoff + an attention item, no
    // ConflictResolutionSucceeded event, and no worker spawn (the caller
    // sees `HaltedForSignoff`, not `FellThrough`, so it never dispatches).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = ScriptCube::with_tripwire_findings(
        Script::CleanPushed,
        vec!["`components/RecommendationBadge.tsx` — added by a merged parent, removed by this resolution".to_owned()],
    );

    let outcome = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate, &attempt).await;

    assert_eq!(outcome, LadderOutcome::HaltedForSignoff);
    // The attempt still gets stamped rung 1 / succeeded — it did push a
    // mechanical resolution; the safety gate is orthogonal telemetry.
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "succeeded");
    assert_eq!(row.resolved_by_rung, Some(1));
    // Parent halted in blocked:deletion_signoff, NOT back in Review.
    let (status, reason) = chore_state(&db, &chore_id);
    assert_eq!(status, "Blocked");
    assert_eq!(reason.as_deref(), Some("deletion_signoff"));
    // An operator sign-off attention item was filed.
    let items = db.list_attention_items_for_work_item(&chore_id).unwrap();
    assert!(
        items
            .iter()
            .any(|i| i.kind == crate::merge_parent_deletion::SIGNOFF_ATTENTION_KIND),
        "expected a merged_parent_deletion_signoff attention item, got {items:?}"
    );
    // Must NOT be reported as a success.
    let typed = pub_.typed_events.lock().await;
    assert!(
        !typed
            .iter()
            .any(|(_, e)| matches!(e, FrontendEvent::ConflictResolutionSucceeded { .. })),
        "a tripwire-halted attempt must not publish ConflictResolutionSucceeded"
    );
    drop(typed);
    assert!(
        !pub_
            .lifecycle_reasons()
            .await
            .contains(&"merge_conflict_resolved".to_owned()),
        "a tripwire-halted attempt must not report merge_conflict_resolved"
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

/// Incident defense-in-depth (T501/flunge PR #917): a rung-1 lease failure
/// that clears on retry must NOT fall through — the ladder proceeds exactly
/// as if the first attempt had never failed.
#[tokio::test]
async fn rung1_lease_fails_once_then_retry_succeeds_and_proceeds() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, _chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = ScriptCube::with_lease_failures(Script::CleanPushed, 1);

    let outcome = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate, &attempt).await;

    assert_eq!(outcome, LadderOutcome::Retired);
    assert_eq!(*cube.lease_calls.lock().await, 2, "one failure, then one retry");
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "succeeded");
    assert_eq!(row.resolved_by_rung, Some(1));
}

/// Incident defense-in-depth (T501/flunge PR #917): a rung-1 lease that
/// fails on both the initial attempt and the retry must NOT escalate
/// straight to a full worker — it reports
/// [`LadderOutcome::MechanicalRungsUnavailable`] so the caller
/// (`conflict_watch::on_conflict_detected`) spawns nothing this tick and
/// retries the ladder on the next `conflict_watch` tick instead.
#[tokio::test]
async fn rung1_lease_fails_twice_reports_mechanical_rungs_unavailable() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, _chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = ScriptCube::with_lease_failures(Script::CleanPushed, 2);

    let outcome = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate, &attempt).await;

    assert_eq!(outcome, LadderOutcome::MechanicalRungsUnavailable);
    assert_eq!(
        *cube.lease_calls.lock().await,
        2,
        "exactly one retry, not an unbounded loop"
    );
    // Never actually leased, so nothing was ever released, and no goto/rebase
    // was attempted.
    assert!(cube.gotos.lock().await.is_empty());
    assert!(cube.released.lock().await.is_empty());
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "pending");
}

/// Re-block a chore already retired by a prior mechanical-rung attempt and
/// insert a fresh `conflict_resolutions` row for it, mirroring the
/// stale-base re-arm path `on_conflict_detected` drives when `main` moves
/// again and the PR re-conflicts after an earlier resolution succeeded.
/// The UNIQUE key is `(work_item_id, base_sha_at_trigger, head_sha_before)`,
/// so the second row needs a distinct `head_sha_before` to avoid colliding
/// with the first attempt's (already-succeeded) row.
fn second_conflict_attempt(db: &WorkDb, product_id: &str, chore_id: &str) -> (PendingMergeCheck, ConflictResolution) {
    db.mark_chore_blocked_merge_conflict(chore_id, PR).unwrap();
    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product_id.to_owned(),
            work_item_id: chore_id.to_owned(),
            pr_url: PR.to_owned(),
            pr_number: 42,
            head_branch: "feature".to_owned(),
            base_branch: "main".to_owned(),
            base_sha_at_trigger: Some("base111".to_owned()),
            head_sha_before: Some("head333".to_owned()),
        })
        .unwrap()
        .expect("fresh second-conflict attempt row");
    let candidate = PendingMergeCheck {
        work_item_id: chore_id.to_owned(),
        product_id: product_id.to_owned(),
        pr_url: PR.to_owned(),
    };
    (candidate, attempt)
}

#[tokio::test]
async fn rung0_live_resolves_a_lockfile_only_residue_including_on_a_second_conflict() {
    // Regression test for spinyfin/mono#2032 (chore T2680): with
    // RUNG0_APPLY_LIVE now true, a residual conflict whose files are ones
    // the deterministic-resolver registry recognizes (here: Cargo.lock —
    // see `registry.rs`'s
    // `cargo_lock_and_bazel_module_lock_both_resolve_deterministically_together`
    // for the equivalent proof covering MODULE.bazel.lock, which needs a
    // fake command runner rather than a real bazel toolchain to stay
    // hermetic) must retire at rung 0 through the *live* `try_mechanical_rungs`
    // call chain, with no worker spawned — and must do so again on a
    // SECOND, independent conflict for the same PR (the "main churns fast,
    // PR re-conflicts after a prior resolution" case from the operator
    // report), not just the first time.
    if which("cargo").is_none() {
        eprintln!("skipping: cargo not on PATH");
        return;
    }
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());

    let ws = tempdir().unwrap();
    write_resolvable_cargo_lock_fixture(ws.path(), "fixture-r0-live");
    let cube = ScriptCube::with_workspace_path(
        Script::Conflicts(vec!["Cargo.lock".to_owned()]),
        ws.path().to_path_buf(),
    );

    // First conflict.
    let outcome = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate, &attempt).await;
    assert_eq!(
        outcome,
        LadderOutcome::Retired,
        "rung 0 must resolve live now that RUNG0_APPLY_LIVE is true"
    );
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "succeeded");
    assert_eq!(row.resolved_by_rung, Some(0));
    let (status, reason) = chore_state(&db, &chore_id);
    assert_eq!(status, "InReview");
    assert!(reason.is_none(), "block should be cleared, got {reason:?}");
    assert_eq!(
        *cube.pushes.lock().await,
        vec![42],
        "rung 0 must land via push_resolution"
    );

    // main churns again: a SECOND, independent conflict on the same PR.
    // The re-arm path re-blocks the chore and inserts a fresh attempt row
    // rather than ever reusing the first (already-succeeded) one.
    let (candidate2, attempt2) = second_conflict_attempt(&db, &candidate.product_id, &chore_id);

    let outcome2 = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate2, &attempt2).await;
    assert_eq!(
        outcome2,
        LadderOutcome::Retired,
        "rung 0 must also resolve deterministically on a re-conflict, not just the first time"
    );
    let row2 = db.get_conflict_resolution(&attempt2.id).unwrap().unwrap();
    assert_eq!(row2.status, "succeeded");
    assert_eq!(row2.resolved_by_rung, Some(0));
    let (status2, reason2) = chore_state(&db, &chore_id);
    assert_eq!(status2, "InReview");
    assert!(reason2.is_none(), "block should be cleared again, got {reason2:?}");
    assert_eq!(
        *cube.pushes.lock().await,
        vec![42, 42],
        "second conflict must also land via push_resolution"
    );
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
    /// T9/T2562: what `verify_deletion_tripwire` should report; empty (the
    /// default via [`Self::new`]) means "clean".
    tripwire_findings: Vec<String>,
    pushes: Mutex<Vec<(PathBuf, u64)>>,
}

impl Rung0Cube {
    fn new(push_ok: bool) -> Self {
        Self {
            push_ok,
            tripwire_findings: Vec::new(),
            pushes: Mutex::new(Vec::new()),
        }
    }

    fn with_tripwire_findings(push_ok: bool, findings: Vec<String>) -> Self {
        Self {
            tripwire_findings: findings,
            ..Self::new(push_ok)
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
    async fn verify_deletion_tripwire(
        &self,
        _repo_slug: &str,
        _head_before: &str,
        _base_sha: &str,
        _pr_number: u64,
    ) -> Vec<String> {
        self.tripwire_findings.clone()
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
async fn rung0_deletion_tripwire_hit_halts_for_signoff_instead_of_retiring() {
    // T9/T2562: same halt contract as rung 1 — a rung-0 push the deletion
    // tripwire flags must not auto-retire as a success.
    if which("cargo").is_none() {
        eprintln!("skipping: cargo not on PATH");
        return;
    }
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());

    let ws = tempdir().unwrap();
    write_resolvable_cargo_lock_fixture(ws.path(), "fixture-r0-sig");
    let lease = CubeWorkspaceLease {
        lease_id: "lease-r0d".to_owned(),
        workspace_id: "ws-r0d".to_owned(),
        workspace_path: ws.path().to_path_buf(),
    };
    let cube = Rung0Cube::with_tripwire_findings(
        true,
        vec!["`components/Foo.tsx` — added by a merged parent, removed by this resolution".to_owned()],
    );

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

    assert_eq!(outcome, LadderOutcome::HaltedForSignoff);
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "succeeded");
    assert_eq!(row.resolved_by_rung, Some(0));
    let (status, reason) = chore_state(&db, &chore_id);
    assert_eq!(status, "Blocked");
    assert_eq!(reason.as_deref(), Some("deletion_signoff"));
    let items = db.list_attention_items_for_work_item(&chore_id).unwrap();
    assert!(
        items
            .iter()
            .any(|i| i.kind == crate::merge_parent_deletion::SIGNOFF_ATTENTION_KIND),
        "expected a merged_parent_deletion_signoff attention item, got {items:?}"
    );
    let typed = pub_.typed_events.lock().await;
    assert!(
        !typed
            .iter()
            .any(|(_, e)| matches!(e, FrontendEvent::ConflictResolutionSucceeded { .. })),
        "a tripwire-halted attempt must not publish ConflictResolutionSucceeded"
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
async fn rung0_declines_both_no_resolver_and_resolver_ran_files_together() {
    // Regression coverage for the decline-log mislabeling defect: a residue
    // can mix a file no resolver claims at all ("docs/readme.md") with a
    // file a resolver *does* claim (`lib.rs`, per
    // `RegistryAppendUnionResolver::applies_to`) but that resolver itself
    // declines (here because the workspace path doesn't exist, so its read
    // fails) — the two are genuinely different failure classes
    // (`DeclinedFile::matched_resolver`) and the ladder must fall through
    // cleanly for a batch mixing both, not just for one class in isolation.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, _chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    let lease = CubeWorkspaceLease {
        lease_id: "lease-r0d".to_owned(),
        workspace_id: "ws-r0d".to_owned(),
        workspace_path: PathBuf::from("/tmp/rung0-declined-mixed-does-not-exist"),
    };
    let cube = Rung0Cube::new(true);

    let outcome = attempt_rung0(
        &db,
        pub_.as_ref(),
        &cube,
        &candidate,
        &attempt,
        &lease,
        &["docs/readme.md".to_owned(), "src/lib.rs".to_owned()],
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
        "must not push when any residual file is declined, even a mixed-class batch"
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

// ── Restart-recovery: persist in-flight mechanical state, reconcile at startup ──
// (2026-07-18 flunge incident: a rung-0 attempt killed mid-rung by an engine
// restart vanished with no verdict and left its parent blocked:merge_conflict
// pointing at a dead attempt forever.)

/// Read a task's `blocked_attempt_id`.
fn chore_blocked_attempt_id(db: &WorkDb, id: &str) -> Option<String> {
    match db.get_work_item(id).unwrap() {
        WorkItem::Chore(t) => t.blocked_attempt_id,
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn mechanical_rung_marker_is_persisted_during_rung1_and_cleared_after_retire() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let (candidate, attempt, _chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    // The peek double snapshots the marker mid-rebase (rung 1 in flight).
    let cube = ScriptCube::with_peek(db_path.clone(), attempt.id.clone());

    let outcome = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate, &attempt).await;
    assert_eq!(outcome, LadderOutcome::Retired);

    // Requirement 1: while the mechanical rung ran, the attempt durably
    // carried `mechanical_rung_in_flight = 1` on disk.
    assert_eq!(
        *cube.marker_seen_during_rebase.lock().await,
        Some(Some(RUNG_ENGINE_DIRECT_REBASE)),
        "rung-1 in-flight marker must be persisted on disk during the rebase",
    );
    // After the rung concludes the marker (and its mechanical lease/workspace)
    // is cleared; the succeeded row carries only the terminal rung stamp.
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "succeeded");
    assert_eq!(row.resolved_by_rung, Some(1));
    assert_eq!(
        row.mechanical_rung_in_flight, None,
        "marker must be cleared once the rung concludes"
    );
    assert_eq!(
        row.cube_lease_id, None,
        "mechanical lease must be cleared once the rung concludes"
    );
    assert_eq!(row.cube_workspace_id, None);
}

#[tokio::test]
async fn mechanical_rung_marker_is_cleared_on_fall_through() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, _chore_id) = blocked_with_attempt(&db);
    let pub_ = Arc::new(RecordingPublisher::default());
    let cube = ScriptCube::new(Script::Conflicts(vec!["src/lib.rs 2-sided conflict".to_owned()]));

    let outcome = try_mechanical_rungs(&db, pub_.as_ref(), &cube, &candidate, &attempt).await;
    assert!(matches!(outcome, LadderOutcome::FellThrough { .. }));

    // The attempt is handed to the worker path still pending, and carries no
    // stale mechanical marker/lease — otherwise the startup reconciler would
    // later mistake a live revision-backed attempt for an orphan.
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "pending");
    assert_eq!(row.mechanical_rung_in_flight, None);
    assert_eq!(row.cube_lease_id, None);
    assert_eq!(row.cube_workspace_id, None);
}

#[tokio::test]
async fn reconcile_recovers_orphaned_mechanical_attempt_and_frees_its_slot() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (_candidate, attempt, chore_id) = blocked_with_attempt(&db);

    // Model the incident: the engine stamped the in-flight rung-0 marker and
    // then died mid-rung (no retire, no clear).
    db.stamp_conflict_resolution_mechanical_rung(&attempt.id, 0, "lease-dead", "ws-dead")
        .unwrap();
    // Precondition: the wedged state — parent blocked pointing at a
    // non-terminal, no-revision attempt with a live in-flight marker.
    let (status, reason) = chore_state(&db, &chore_id);
    assert_eq!(status, "Blocked");
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
    assert_eq!(
        chore_blocked_attempt_id(&db, &chore_id).as_deref(),
        Some(attempt.id.as_str())
    );
    assert_eq!(
        db.get_conflict_resolution(&attempt.id)
            .unwrap()
            .unwrap()
            .mechanical_rung_in_flight,
        Some(0),
    );

    let recovered = db.reconcile_orphaned_conflict_ladder_attempts().unwrap();

    // Requirement 2/3: exactly this attempt is recovered, at the rung it died on.
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].work_item_id, chore_id);
    assert_eq!(recovered[0].attempt_id.as_deref(), Some(attempt.id.as_str()));
    assert_eq!(recovered[0].rung, Some(0));

    // The orphaned attempt is abandoned with the slot freed (base SHA nulled)
    // and its marker cleared.
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "abandoned");
    assert_eq!(
        row.failure_reason.as_deref(),
        Some("engine_restart_orphaned_ladder_attempt")
    );
    assert_eq!(
        row.base_sha_at_trigger, None,
        "UNIQUE slot must be freed for a fresh attempt"
    );
    assert_eq!(row.mechanical_rung_in_flight, None);

    // The parent is back in Review with no attempt pointer, so the watcher
    // re-detects the still-open conflict on the next sweep.
    let (status, reason) = chore_state(&db, &chore_id);
    assert_eq!(status, "InReview");
    assert!(reason.is_none());
    assert_eq!(chore_blocked_attempt_id(&db, &chore_id), None);
    assert!(
        db.active_conflict_resolution_for_work_item(&chore_id)
            .unwrap()
            .is_none(),
        "no active attempt should remain after recovery",
    );

    // The freed slot admits a fresh attempt at the original (base, head) key.
    let fresh = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: recovered[0].product_id.clone(),
            work_item_id: chore_id.clone(),
            pr_url: PR.to_owned(),
            pr_number: 42,
            head_branch: "feature".to_owned(),
            base_branch: "main".to_owned(),
            base_sha_at_trigger: Some("base111".to_owned()),
            head_sha_before: Some("head222".to_owned()),
        })
        .unwrap();
    assert!(
        fresh.is_some(),
        "the freed UNIQUE slot must admit a fresh attempt at the same key"
    );
}

#[tokio::test]
async fn reconcile_recovers_bare_pending_attempt_with_no_mechanical_marker() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    // A bare pending, no-marker, no-revision attempt: no rung was ever
    // stamped (e.g. the engine died before entering rung 0, or between
    // rungs with no marker live). `rung` must be reported as `None`, not
    // mistaken for "nothing to recover".
    let (_candidate, attempt, chore_id) = blocked_with_attempt(&db);
    assert_eq!(
        db.get_conflict_resolution(&attempt.id)
            .unwrap()
            .unwrap()
            .mechanical_rung_in_flight,
        None,
    );

    let recovered = db.reconcile_orphaned_conflict_ladder_attempts().unwrap();

    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].work_item_id, chore_id);
    assert_eq!(recovered[0].attempt_id.as_deref(), Some(attempt.id.as_str()));
    assert_eq!(recovered[0].rung, None);

    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "abandoned");
    assert_eq!(
        row.failure_reason.as_deref(),
        Some("engine_restart_orphaned_ladder_attempt")
    );
    assert_eq!(row.base_sha_at_trigger, None, "UNIQUE slot must be freed");

    let (status, reason) = chore_state(&db, &chore_id);
    assert_eq!(status, "InReview");
    assert!(reason.is_none());
    assert_eq!(chore_blocked_attempt_id(&db, &chore_id), None);
}

#[tokio::test]
async fn reconcile_recovers_parent_when_blocked_attempt_row_is_missing() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    // `blocked_attempt_id` points at a row that no longer exists (e.g.
    // hard-deleted out of band). The LEFT JOIN reports `cr.id IS NULL`, so
    // there is nothing to abandon, but the parent must still be flipped
    // back to `in_review` and the dangling pointer cleared.
    let (_candidate, attempt, chore_id) = blocked_with_attempt(&db);
    db.connect()
        .unwrap()
        .execute("DELETE FROM conflict_resolutions WHERE id = ?1", [&attempt.id])
        .unwrap();
    assert_eq!(db.get_conflict_resolution(&attempt.id).unwrap(), None);
    assert_eq!(
        chore_blocked_attempt_id(&db, &chore_id).as_deref(),
        Some(attempt.id.as_str()),
        "the dangling pointer must still be in place before reconcile runs"
    );

    let recovered = db.reconcile_orphaned_conflict_ladder_attempts().unwrap();

    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].work_item_id, chore_id);
    assert_eq!(recovered[0].attempt_id.as_deref(), Some(attempt.id.as_str()));
    assert_eq!(recovered[0].rung, None, "no row to read a rung from");

    let (status, reason) = chore_state(&db, &chore_id);
    assert_eq!(status, "InReview");
    assert!(reason.is_none());
    assert_eq!(chore_blocked_attempt_id(&db, &chore_id), None);
}

#[tokio::test]
async fn reconcile_leaves_revision_backed_and_terminal_attempts_alone() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();

    // (a) A revision-backed in-flight attempt is owned by a dispatched worker
    // (supersede_if_stale handles a dead revision on re-detect), not this sweep.
    let (_candidate, attempt, chore_id) = blocked_with_attempt(&db);
    db.set_conflict_resolution_revision_task_id(&attempt.id, "task_rev_1")
        .unwrap();

    let recovered = db.reconcile_orphaned_conflict_ladder_attempts().unwrap();
    assert!(
        recovered.is_empty(),
        "a revision-backed attempt must not be recovered by the ladder sweep"
    );
    let (status, reason) = chore_state(&db, &chore_id);
    assert_eq!(status, "Blocked");
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
    assert_eq!(
        db.get_conflict_resolution(&attempt.id).unwrap().unwrap().status,
        "pending"
    );

    // (b) A terminal (failed) attempt the parent still points at is a
    // legitimate resting state (churn/human-owned), not a wedge.
    let dir2 = tempdir().unwrap();
    let db2 = WorkDb::open(dir2.path().join("boss.db")).unwrap();
    let (_c2, attempt2, chore2) = blocked_with_attempt(&db2);
    db2.mark_conflict_resolution_failed(&attempt2.id, "gave_up").unwrap();

    let recovered2 = db2.reconcile_orphaned_conflict_ladder_attempts().unwrap();
    assert!(recovered2.is_empty(), "a terminal attempt must not be recovered");
    let (status2, reason2) = chore_state(&db2, &chore2);
    assert_eq!(status2, "Blocked");
    assert_eq!(reason2.as_deref(), Some("merge_conflict"));
}
