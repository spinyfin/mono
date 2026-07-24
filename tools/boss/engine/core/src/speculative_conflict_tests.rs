use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use tempfile::tempdir;
use tokio::sync::Mutex;

use super::*;
use crate::coordinator::{CubeRepoHandle, CubeWorkspaceLease, RebaseOutcome};
use crate::metrics::Registry;
use crate::test_support::*;
use crate::work::PendingMergeCheck;

/// Flip `products.auto_pr_maintenance_enabled` directly on the SQLite file
/// (mirrors `conflict_watch_tests.rs`'s helper of the same shape — no
/// production setter exists because nothing else needs one).
fn set_product_auto_pr_maintenance(db_path: &std::path::Path, product_id: &str, enabled: bool) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE products SET auto_pr_maintenance_enabled = ?2 WHERE id = ?1",
        rusqlite::params![product_id, if enabled { 1 } else { 0 }],
    )
    .unwrap();
}

/// What the scripted cube double's `rebase_workspace_no_push` returns.
#[derive(Clone)]
enum Script {
    Clean,
    Conflicts(Vec<String>),
    EnsureRepoErrors,
    LeaseErrors,
    RebaseErrors,
}

/// Configurable [`CubeClient`] double for the speculative sweep. Records
/// every lease/goto/rebase/release call so tests can assert both the
/// outcome and that a lease is never left dangling.
struct ScriptCube {
    script: Script,
    leases: Mutex<u64>,
    gotos: Mutex<Vec<u64>>,
    rebases: Mutex<Vec<u64>>,
    released: Mutex<Vec<String>>,
}

impl ScriptCube {
    fn new(script: Script) -> Self {
        Self {
            script,
            leases: Mutex::new(0),
            gotos: Mutex::new(Vec::new()),
            rebases: Mutex::new(Vec::new()),
            released: Mutex::new(Vec::new()),
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
        if matches!(self.script, Script::LeaseErrors) {
            anyhow::bail!("lease boom");
        }
        let mut count = self.leases.lock().await;
        *count += 1;
        Ok(CubeWorkspaceLease {
            lease_id: format!("lease-{count}"),
            workspace_id: format!("ws-{count}"),
            workspace_path: PathBuf::from("/tmp/speculative-ws"),
            dirty_verified: None,
        })
    }
    async fn goto_workspace(&self, _workspace_path: &std::path::Path, pr: u64) -> Result<()> {
        self.gotos.lock().await.push(pr);
        Ok(())
    }
    async fn rebase_workspace_no_push(&self, _workspace_path: &std::path::Path, pr: u64) -> Result<RebaseOutcome> {
        self.rebases.lock().await.push(pr);
        match &self.script {
            Script::Clean => Ok(RebaseOutcome { clean: true, pushed: false, conflicted_files: Vec::new() }),
            Script::Conflicts(files) => {
                Ok(RebaseOutcome { clean: false, pushed: false, conflicted_files: files.clone() })
            }
            Script::RebaseErrors => anyhow::bail!("rebase boom"),
            Script::EnsureRepoErrors | Script::LeaseErrors => unreachable!("must not reach rebase"),
        }
    }
    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        self.released.lock().await.push(lease_id.to_owned());
        Ok(())
    }
} }

const PR: &str = "https://github.com/foo/bar/pull/42";

/// Build an in-review chore (no `blocked` flip — the speculative sweep
/// never touches parent-task state) and return the poller-shaped
/// candidate the sweep consumes.
fn in_review_candidate(db: &WorkDb, label: &str) -> (PendingMergeCheck, String, String) {
    let product = create_test_product_with_repo(db, label, Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore_manual(db, product.id.clone(), format!("{label}-chore"));
    db.update_work_item(
        &chore.id,
        crate::work::WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(PR.into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();
    let candidate = PendingMergeCheck {
        work_item_id: chore.id.clone(),
        product_id: product.id.clone(),
        pr_url: PR.to_owned(),
    };
    (candidate, product.id, chore.id)
}

fn registry_with_counters() -> Registry {
    let registry = Registry::new();
    init(&registry);
    registry
}

#[tokio::test]
async fn clean_prediction_increments_counters_and_writes_no_row() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, product_id, work_item_id) = in_review_candidate(&db, "SpecClean");
    let cube = ScriptCube::new(Script::Clean);
    let registry = registry_with_counters();
    let mut schedule = SpeculativeCheckSchedule::default();

    run_speculative_pass(&db, &cube, &registry, &mut schedule, &[candidate]).await;

    assert_eq!(registry.counter_value("speculative_conflict.clean"), Some(1));
    assert_eq!(
        registry.counter_value("speculative_conflict.predicted"),
        Some(0),
        "registered by init() but never incremented"
    );
    let expected_dynamic = format!(
        "speculative_conflict.{}.clean",
        sanitize_metric_name_component(&product_id)
    );
    assert_eq!(registry.counter_value(&expected_dynamic), Some(1));

    // No telemetry row for a clean prediction — only the counters record it.
    assert!(
        db.latest_conflict_resolution_for_work_item(&work_item_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(*cube.gotos.lock().await, vec![42]);
    assert_eq!(*cube.rebases.lock().await, vec![42]);
    assert_eq!(cube.released.lock().await.len(), 1);
}

#[tokio::test]
async fn conflict_prediction_records_telemetry_row() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, _product_id, work_item_id) = in_review_candidate(&db, "SpecConflict");
    let cube = ScriptCube::new(Script::Conflicts(vec![
        "Cargo.lock".to_owned(),
        "src/lib.rs".to_owned(),
    ]));
    let registry = registry_with_counters();
    let mut schedule = SpeculativeCheckSchedule::default();

    run_speculative_pass(&db, &cube, &registry, &mut schedule, &[candidate]).await;

    assert_eq!(registry.counter_value("speculative_conflict.predicted"), Some(1));
    assert_eq!(
        registry.counter_value("speculative_conflict.clean"),
        Some(0),
        "registered by init() but never incremented"
    );

    let row = db
        .latest_conflict_resolution_for_work_item(&work_item_id)
        .unwrap()
        .expect("predicted-conflict row must be recorded");
    assert_eq!(row.status, "predicted");
    assert_eq!(row.pr_number, 42);
    assert!(row.conflict_diagnosis.is_some());

    // Purely a telemetry row: never touches the parent's status.
    match db.get_work_item(&work_item_id).unwrap() {
        crate::work::WorkItem::Chore(t) => {
            assert_eq!(format!("{:?}", t.status), "InReview");
            assert!(t.blocked_reason.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn rebase_error_skips_without_recording() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, _product_id, work_item_id) = in_review_candidate(&db, "SpecRebaseErr");
    let cube = ScriptCube::new(Script::RebaseErrors);
    let registry = registry_with_counters();
    let mut schedule = SpeculativeCheckSchedule::default();

    run_speculative_pass(&db, &cube, &registry, &mut schedule, &[candidate]).await;

    assert_eq!(registry.counter_value("speculative_conflict.predicted"), Some(0));
    assert_eq!(registry.counter_value("speculative_conflict.clean"), Some(0));
    assert!(
        db.latest_conflict_resolution_for_work_item(&work_item_id)
            .unwrap()
            .is_none()
    );
    // Lease still released even though the rebase blew up.
    assert_eq!(cube.released.lock().await.len(), 1);
}

#[tokio::test]
async fn ensure_repo_error_never_leases() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, _product_id, _work_item_id) = in_review_candidate(&db, "SpecEnsureErr");
    let cube = ScriptCube::new(Script::EnsureRepoErrors);
    let registry = registry_with_counters();
    let mut schedule = SpeculativeCheckSchedule::default();

    run_speculative_pass(&db, &cube, &registry, &mut schedule, &[candidate]).await;

    assert_eq!(*cube.leases.lock().await, 0);
    assert!(cube.gotos.lock().await.is_empty());
    assert!(cube.released.lock().await.is_empty());
}

#[tokio::test]
async fn lease_error_is_non_fatal() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, _product_id, _work_item_id) = in_review_candidate(&db, "SpecLeaseErr");
    let cube = ScriptCube::new(Script::LeaseErrors);
    let registry = registry_with_counters();
    let mut schedule = SpeculativeCheckSchedule::default();

    run_speculative_pass(&db, &cube, &registry, &mut schedule, &[candidate]).await;

    assert!(cube.gotos.lock().await.is_empty());
    assert!(cube.released.lock().await.is_empty());
}

#[tokio::test]
async fn opted_out_product_is_skipped_without_leasing() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let (candidate, product_id, _work_item_id) = in_review_candidate(&db, "SpecOptOut");
    set_product_auto_pr_maintenance(&db_path, &product_id, false);
    let cube = ScriptCube::new(Script::Clean);
    let registry = registry_with_counters();
    let mut schedule = SpeculativeCheckSchedule::default();

    run_speculative_pass(&db, &cube, &registry, &mut schedule, &[candidate]).await;

    assert_eq!(*cube.leases.lock().await, 0);
    assert_eq!(
        registry.counter_value("speculative_conflict.clean"),
        Some(0),
        "opted-out product must never increment the counter"
    );
}

#[tokio::test]
async fn pass_caps_checks_per_sweep() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let mut candidates = Vec::new();
    for i in 0..(MAX_CHECKS_PER_PASS + 2) {
        let (candidate, _product_id, _work_item_id) = in_review_candidate(&db, &format!("SpecCap{i}"));
        candidates.push(candidate);
    }
    let cube = ScriptCube::new(Script::Clean);
    let registry = registry_with_counters();
    let mut schedule = SpeculativeCheckSchedule::default();

    run_speculative_pass(&db, &cube, &registry, &mut schedule, &candidates).await;

    assert_eq!(
        *cube.leases.lock().await,
        MAX_CHECKS_PER_PASS as u64,
        "one sweep must not lease more than the per-pass cap"
    );
}

#[test]
fn schedule_due_respects_recheck_interval_then_becomes_due_again() {
    let mut schedule = SpeculativeCheckSchedule::default();
    let t0 = Instant::now();
    assert!(schedule.due("wi-1", t0), "never-checked work item is immediately due");

    schedule.mark_checked("wi-1", t0);
    assert!(
        !schedule.due("wi-1", t0 + Duration::from_secs(60)),
        "just-checked work item must not be due again within the recheck interval"
    );
    assert!(
        schedule.due("wi-1", t0 + RECHECK_INTERVAL),
        "work item becomes due again once the recheck interval elapses"
    );
}

#[test]
fn schedule_retain_known_drops_stale_entries() {
    let mut schedule = SpeculativeCheckSchedule::default();
    let t0 = Instant::now();
    schedule.mark_checked("wi-stale", t0);
    schedule.mark_checked("wi-kept", t0);

    let known: std::collections::HashSet<String> = ["wi-kept".to_owned()].into_iter().collect();
    schedule.retain_known(&known);

    // The dropped entry is immediately due again; the kept one still isn't.
    assert!(schedule.due("wi-stale", t0));
    assert!(!schedule.due("wi-kept", t0));
}
