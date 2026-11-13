//! Periodic retention sweep for terminal `work_executions` rows.
//!
//! Applies [`crate::work::ExecutionRetentionPolicy`] (see
//! `crate::work::execution_retention` for the full policy rationale) on a
//! recurring cadence so the stock of terminal executions — dominated in
//! practice by `redundant_spawn` pre-spawn aborts — never grows unbounded.
//!
//! Because [`crate::sweep_loop::spawn_sweep_loop`] fires immediately on
//! spawn, the first pass after this ships also performs the one-time
//! cleanup of whatever backlog had already accumulated (e.g. the
//! accumulated `redundant_spawn` pre-spawn aborts) — there is no separate migration
//! step. Every subsequent pass just keeps the stock bounded going forward.
//!
//! ## Ordering vs Codex home reclaim
//!
//! Each pass runs the codex-home retention reclaim **before** pruning
//! execution rows so policy-eligible homes are reclaimed while their DB
//! pointers still exist. Prune itself also reclaims any home that loses
//! its last remaining pointer (see `execution_retention`), so a race
//! cannot permanently orphan a reclaimable `CODEX_HOME`.

use std::sync::Arc;
use std::time::Duration;

use crate::work::{ExecutionRetentionPolicy, WorkDb};

/// How often the retention sweep runs. Deliberately much slower than the
/// worker-liveness sweeps (dead-pid, stale-worker, …): pruning terminal
/// rows is not time-sensitive, and the delete is a no-op in steady state
/// once the backlog is clean.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Counts from one sweep pass; logged whenever any row was pruned.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExecutionRetentionSweepOutcome {
    pub deleted: u64,
    pub driver_homes_reclaimed: u64,
}

impl crate::sweep_loop::SweepOutcome for ExecutionRetentionSweepOutcome {
    fn has_activity(&self) -> bool {
        self.deleted > 0 || self.driver_homes_reclaimed > 0
    }

    fn log(&self) {
        tracing::info!(
            deleted = self.deleted,
            driver_homes_reclaimed = self.driver_homes_reclaimed,
            "execution-retention sweep: pruned terminal work_executions rows past the retention bound",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
pub fn spawn_loop(work_db: Arc<WorkDb>, interval: Duration) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        async move { run_one_pass(work_db.as_ref()).await }
    })
}

/// Run a single retention pass with the default policy. Returns a summary;
/// callers may log it.
///
/// Order: codex-home reclaim first (while pointers exist), then row prune
/// (which also reclaims homes that lose their last pointer).
pub async fn run_one_pass(work_db: &WorkDb) -> ExecutionRetentionSweepOutcome {
    // Reclaim-before-prune: policy-eligible homes for terminal executions
    // that are about to (or may soon) leave the DB.
    let _ = crate::codex_home_retention_sweep::run_one_pass(work_db, false).await;

    let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
    prune_spoke_commands(work_db, now_epoch);
    reclaim_spoke_custody(work_db, now_epoch);

    // Storage does not probe processes; the sweep supplies the observation
    // that lets it reap staging files whose writer is proven gone.
    let staging_owner_is_dead = |pid| {
        matches!(
            crate::dead_pid_sweep::probe_pid(pid),
            crate::dead_pid_sweep::PidStatus::Dead
        )
    };
    if let Err(err) = work_db.gc_unreferenced_pr_review_guide_source_artifacts_with(&staging_owner_is_dead) {
        tracing::warn!(
            ?err,
            "execution-retention sweep: review-guide source artifact gc failed; continuing"
        );
    }

    match work_db.prune_terminal_executions(ExecutionRetentionPolicy::default(), now_epoch, false) {
        Ok(outcome) => ExecutionRetentionSweepOutcome {
            deleted: outcome.deleted,
            driver_homes_reclaimed: outcome.driver_homes_reclaimed,
        },
        Err(err) => {
            tracing::warn!(?err, "execution-retention sweep: prune failed; skipping this pass");
            ExecutionRetentionSweepOutcome::default()
        }
    }
}

/// How long the hub keeps its copies of a finished spoke run's transcript and
/// artifacts. The same window as a terminal execution's Codex home and its
/// diagnostics: long enough for "what happened last week?".
pub const SPOKE_CUSTODY_MAX_AGE_SECS: i64 = 14 * 24 * 60 * 60;

/// Reclaim the hub's custody files of spoke runs that finished long enough ago.
///
/// Like the home sweeps, it acts only on what the ledger recorded, and liveness
/// is the execution's status, never a file's age: a run that is still going, or
/// that this database cannot place, keeps its bytes.
fn reclaim_spoke_custody(work_db: &WorkDb, now_epoch: i64) {
    let Ok(root) = boss_engine_driver::transcript_store::transcript_store_root() else {
        return;
    };
    let custody = crate::hub::projection::Custody::new(work_db.clone(), root);
    let executions = match work_db.spoke_custody_executions() {
        Ok(executions) => executions,
        Err(error) => {
            tracing::warn!(%error, "execution-retention sweep: cannot list spoke custody owners");
            return;
        }
    };
    for execution_id in executions {
        let Ok(execution) = work_db.get_execution(&execution_id) else {
            continue;
        };
        if !execution.status.is_terminal() {
            continue;
        }
        let finished = execution
            .finished_epoch()
            .or_else(|| execution.created_epoch())
            .unwrap_or(now_epoch);
        if now_epoch.saturating_sub(finished) < SPOKE_CUSTODY_MAX_AGE_SECS {
            continue;
        }
        match custody.reclaim_execution(&execution_id) {
            Ok(bytes) => tracing::info!(execution = %execution_id, bytes, "reclaimed spoke transcript custody"),
            Err(error) => tracing::warn!(execution = %execution_id, %error, "spoke custody retention failed"),
        }
    }
}

fn prune_spoke_commands(work_db: &WorkDb, now_epoch: i64) {
    let cutoff = now_epoch
        .saturating_mul(1000)
        .saturating_sub(boss_work_db::SETTLED_HOST_COMMAND_RETENTION_MS);
    let hosts = match work_db.list_hosts() {
        Ok(hosts) => hosts,
        Err(error) => {
            tracing::warn!(%error, "execution-retention sweep: cannot list spoke command owners");
            return;
        }
    };
    for host in hosts {
        match work_db.prune_settled_host_commands(&host.id, cutoff, boss_work_db::MIN_RETAINED_SETTLED_HOST_COMMANDS) {
            Ok(deleted) if deleted > 0 => tracing::info!(host = %host.id, deleted, "pruned settled spoke commands"),
            Ok(_) => {}
            Err(error) => tracing::warn!(host = %host.id, %error, "spoke command retention failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crate::work::CreateChoreInput;

    #[tokio::test]
    async fn sweep_prunes_old_abandoned_backlog_on_first_pass() {
        let (_dir, db) = open_db();
        let product_id = create_test_product_with_repo(&db, "test-product", Some("https://github.com/test/repo")).id;
        let work_item_id = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product_id)
                    .name("redundant-spawn-storm")
                    .build(),
            )
            .unwrap()
            .id;

        let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();
        let very_old = now_epoch - 400 * 24 * 60 * 60;
        for _ in 0..10 {
            let execution = db
                .request_execution(
                    boss_protocol::RequestExecutionInput::builder()
                        .work_item_id(work_item_id.clone())
                        .build(),
                )
                .unwrap();
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET status = 'abandoned', created_at = ?2 WHERE id = ?1",
                rusqlite::params![execution.id, very_old.to_string()],
            )
            .unwrap();
        }

        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert!(outcome.deleted > 0, "first pass must clean up the pre-existing backlog");

        let remaining = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            remaining.len() < 10,
            "backlog beyond the keep floor must be pruned, got {} remaining",
            remaining.len()
        );
    }

    #[test]
    fn sweep_reclaims_spoke_custody_of_old_finished_runs_and_keeps_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("executions");
        let _store = boss_engine_driver::test_support::transcript_store_override(&store);
        let (_dir, db) = open_db();
        let product_id = create_test_product_with_repo(&db, "test-product", Some("https://github.com/test/repo")).id;
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let make = |name: &str, status: &str, age_days: i64| {
            let work_item_id = db
                .create_chore(
                    CreateChoreInput::builder()
                        .product_id(product_id.clone())
                        .name(name)
                        .build(),
                )
                .unwrap()
                .id;
            let execution = db
                .request_execution(
                    boss_protocol::RequestExecutionInput::builder()
                        .work_item_id(work_item_id)
                        .build(),
                )
                .unwrap();
            let when = (now - age_days * 24 * 60 * 60).to_string();
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE work_executions SET status = ?2, created_at = ?3, finished_at = ?3, started_at = ?3 WHERE id = ?1",
                    rusqlite::params![execution.id, status, when],
                )
                .unwrap();
            let held = store.join(&execution.id).join("spoke").join("run_1");
            std::fs::create_dir_all(&held).unwrap();
            std::fs::write(held.join("transcript.jsonl"), b"bytes").unwrap();
            db.put_spoke_custody(
                &boss_work_db::SpokeCustody::builder()
                    .execution_id(execution.id.clone())
                    .assignment_generation(1_u64)
                    .resource("transcript:0-1")
                    .host_id("host-a")
                    .run_id("run_1")
                    .kind("transcript")
                    .state(boss_work_db::CustodyState::Open)
                    .path(format!("{}/spoke/run_1/transcript.jsonl", execution.id))
                    .materialized(5_u64)
                    .build(),
                None,
            )
            .unwrap();
            (execution.id, held)
        };
        let (old_done, old_dir) = make("old-finished", "completed", 30);
        let (recent_done, recent_dir) = make("recent-finished", "completed", 1);
        let (old_live, live_dir) = make("old-but-running", "running", 30);

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_one_pass(&db));

        assert!(!old_dir.exists(), "a long-finished run's hub copy is reclaimed");
        assert!(db.spoke_custody_for_execution(&old_done).unwrap().is_empty());
        assert!(recent_dir.exists(), "a recently finished run keeps its bytes");
        assert!(!db.spoke_custody_for_execution(&recent_done).unwrap().is_empty());
        assert!(
            live_dir.exists(),
            "a run that is still going keeps its bytes however old"
        );
        assert!(!db.spoke_custody_for_execution(&old_live).unwrap().is_empty());
    }

    #[tokio::test]
    async fn sweep_is_a_noop_once_clean() {
        let (_dir, db) = open_db();
        let db = Arc::new(db);
        let outcome = run_one_pass(db.as_ref()).await;
        assert_eq!(outcome.deleted, 0);
    }

    #[tokio::test]
    async fn sweep_reaps_source_staging_files_only_for_pids_the_host_probe_reports_dead() {
        let (dir, db) = open_db();
        let shard = dir.path().join("review-guide-sources").join("ab");
        std::fs::create_dir_all(&shard).unwrap();
        let dead = shard.join(format!("blob.{}.1.tmp", i32::MAX));
        // pid 1 always exists, so the probe never reports it dead.
        let alive = shard.join("blob.1.1.tmp");
        let own = shard.join(format!("blob.{}.1.tmp", std::process::id()));
        for path in [&dead, &alive, &own] {
            std::fs::write(path, "staged").unwrap();
        }

        run_one_pass(&db).await;

        assert!(!dead.exists(), "a staging file whose writer is gone is reaped");
        assert!(own.exists(), "this process's own staging file is never reaped");
        assert!(
            alive.exists(),
            "a staging file is kept while its writer's pid is not proven dead"
        );
    }
}
