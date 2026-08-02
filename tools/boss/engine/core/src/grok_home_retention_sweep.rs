//! Periodic reclaim of Boss-owned per-run Grok home containers past
//! retention policy. Mirrors
//! `tools/boss/engine/core/src/codex_home_retention_sweep.rs`.
//!
//! Loads recorded roots exclusively from
//! `work_executions.driver_runtime_state` (see
//! [`boss_engine_driver::grok::GrokRuntimeState`]), classifies live vs
//! terminal from **execution status**, and hands the candidate list to
//! [`boss_engine_grok_home_retention`]. Never:
//!
//! - infers a home from the engine environment / `$GROK_HOME` / `~/.grok`
//! - deletes a path that is not recorded on an execution row
//! - judges liveness from file mtime
//!
//! Cleanup is best-effort and non-fatal: a reclaim error is logged and the
//! next pass retries. Idempotent when a home is already gone.

use std::sync::Arc;
use std::time::Duration;

use boss_engine_driver::grok::{GrokRuntimeState, assert_grok_home_safe_to_delete};
use boss_engine_grok_home_retention as retention;

use crate::work::WorkDb;

/// How often the retention pass runs. Same cadence as the Codex-home sweep:
/// reclaim is not time-critical once the policy window has elapsed.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Counts from one pass; logged when anything was reclaimed or errored.
#[derive(Debug, Default, PartialEq, Eq, bon::Builder)]
pub struct GrokHomeRetentionSweepOutcome {
    #[builder(default)]
    pub scanned: u64,
    #[builder(default)]
    pub deleted: u64,
    #[builder(default)]
    pub deleted_bytes: u64,
    #[builder(default)]
    pub skipped_live: u64,
    #[builder(default)]
    pub kept_in_policy: u64,
    #[builder(default)]
    pub errors: u64,
}

impl crate::sweep_loop::SweepOutcome for GrokHomeRetentionSweepOutcome {
    fn has_activity(&self) -> bool {
        self.deleted > 0 || self.errors > 0
    }

    fn log(&self) {
        tracing::info!(
            scanned = self.scanned,
            deleted = self.deleted,
            deleted_bytes = self.deleted_bytes,
            skipped_live = self.skipped_live,
            kept_in_policy = self.kept_in_policy,
            errors = self.errors,
            "grok-home retention sweep: reclaimed Boss-owned GROK_HOME run containers past policy",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
pub fn spawn_loop(work_db: Arc<WorkDb>, interval: Duration) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        async move { run_one_pass(work_db.as_ref(), false).await }
    })
}

/// Run a single reclaim pass with the default policy.
pub async fn run_one_pass(work_db: &WorkDb, dry_run: bool) -> GrokHomeRetentionSweepOutcome {
    run_one_pass_with_policy(work_db, &retention::GrokHomeRetentionPolicy::default(), dry_run).await
}

/// Run a single reclaim pass with an explicit policy (bossctl / tests).
pub async fn run_one_pass_with_policy(
    work_db: &WorkDb,
    policy: &retention::GrokHomeRetentionPolicy,
    dry_run: bool,
) -> GrokHomeRetentionSweepOutcome {
    let now_epoch = boss_engine_utils::epoch_time::now_epoch_secs();

    let executions = match work_db.list_executions_with_driver_runtime_state() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "grok-home retention sweep: failed to list executions with runtime state; skipping"
            );
            return GrokHomeRetentionSweepOutcome::default();
        }
    };

    let mut homes = Vec::new();
    for execution in &executions {
        let Some(state) = execution.driver_runtime_state.as_ref() else {
            continue;
        };
        let runtime = match GrokRuntimeState::from_driver_runtime_state(state) {
            Ok(r) => r,
            Err(_) => {
                // Not a Grok payload (or corrupt) — other drivers store
                // their own shape here (e.g. Codex). Skip silently.
                continue;
            }
        };
        if runtime.grok_home.as_os_str().is_empty() {
            continue;
        }
        // Reclaim the whole run container (`grok-home/` + `process-home/`),
        // not GROK_HOME alone, so the scoped process HOME is never left as
        // a permanent orphan of its sibling.
        let container = runtime
            .grok_home
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(runtime.grok_home);
        let age_anchor_epoch = execution
            .finished_epoch()
            .or_else(|| execution.created_epoch())
            .unwrap_or(now_epoch);
        // Live = non-terminal. Ready/queued rows that somehow hold a
        // recorded home (e.g. re-dispatch after partial provision) stay
        // protected until they finish.
        let is_live = !execution.status.is_terminal();
        let size_bytes = retention::directory_size_bytes(&container);
        homes.push(retention::OwnedGrokHome {
            path: container,
            execution_id: execution.id.clone(),
            is_live,
            age_anchor_epoch,
            size_bytes,
        });
    }

    if homes.is_empty() {
        return GrokHomeRetentionSweepOutcome::default();
    }

    match retention::execute_reclaim(&homes, now_epoch, policy, dry_run, &assert_grok_home_safe_to_delete) {
        Ok(report) => {
            for error in &report.errors {
                tracing::warn!(%error, "grok-home retention sweep: failed to reclaim a recorded root");
            }
            GrokHomeRetentionSweepOutcome {
                scanned: report.scanned as u64,
                deleted: report.deleted.len() as u64,
                deleted_bytes: report.deleted_bytes,
                skipped_live: report.skipped_live as u64,
                kept_in_policy: report.kept_in_policy as u64,
                errors: report.errors.len() as u64,
            }
        }
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "grok-home retention sweep: execute_reclaim failed; skipping this pass"
            );
            GrokHomeRetentionSweepOutcome::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crate::work::CreateChoreInput;
    use boss_engine_driver::grok::GrokRuntimeState;
    use boss_protocol::DriverRuntimeState;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_epoch() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn seed_execution_with_home(
        db: &WorkDb,
        work_item_id: &str,
        status: &str,
        finished_at: i64,
        grok_home: &std::path::Path,
    ) -> String {
        let execution = db
            .request_execution(
                boss_protocol::RequestExecutionInput::builder()
                    .work_item_id(work_item_id)
                    .build(),
            )
            .unwrap();
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions SET status = ?2, finished_at = ?3, created_at = ?3 WHERE id = ?1",
                rusqlite::params![execution.id, status, finished_at.to_string()],
            )
            .unwrap();
        }
        let state = GrokRuntimeState {
            grok_home: grok_home.to_path_buf(),
            process_home: grok_home.with_file_name("process-home"),
            auth_source_path: PathBuf::from("/tmp/source-auth.json"),
            session_id: "11111111-1111-4111-8111-111111111111".into(),
            workspace_path: PathBuf::from("/tmp/ws"),
            grok_version: None,
        }
        .to_driver_runtime_state();
        db.set_driver_runtime_state(&execution.id, Some(&state)).unwrap();
        execution.id
    }

    /// Driven by a test-owned current-thread runtime rather than
    /// `#[tokio::test]` so the homes-root override can be held for the
    /// whole sweep instead of only around the `set_var` — mirrors the
    /// equivalent Codex sweep test's rationale.
    #[test]
    fn sweep_reclaims_only_recorded_terminal_containers_past_age() {
        let tmp = tempfile::tempdir().unwrap();
        let homes_root = tmp.path().join("boss-grok-homes");
        std::fs::create_dir_all(&homes_root).unwrap();

        let _env_guard = boss_engine_driver::test_support::grok_homes_override(&homes_root);

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(sweep_reclaims_only_recorded_terminal_containers_past_age_body(
                &homes_root,
            ));
    }

    async fn sweep_reclaims_only_recorded_terminal_containers_past_age_body(homes_root: &std::path::Path) {
        let (_dir, db) = open_db();
        let product_id = create_test_product_with_repo(&db, "p", Some("https://github.com/test/repo")).id;
        let chore = |name: &str| {
            db.create_chore(
                CreateChoreInput::builder()
                    .product_id(product_id.clone())
                    .name(name.to_owned())
                    .build(),
            )
            .unwrap()
            .id
        };
        let old_item = chore("grok-retention-old");
        let live_item = chore("grok-retention-live");
        let recent_item = chore("grok-retention-recent");

        let now = now_epoch();
        let old_container = homes_root.join("old-run");
        let live_container = homes_root.join("live-run");
        let recent_container = homes_root.join("recent-run");
        for c in [&old_container, &live_container, &recent_container] {
            std::fs::create_dir_all(c.join("grok-home/sessions")).unwrap();
            std::fs::create_dir_all(c.join("process-home")).unwrap();
            std::fs::write(c.join("grok-home/marker"), "x").unwrap();
        }

        seed_execution_with_home(
            &db,
            &old_item,
            "completed",
            now - 40 * 24 * 60 * 60,
            &old_container.join("grok-home"),
        );
        seed_execution_with_home(
            &db,
            &live_item,
            "running",
            now - 40 * 24 * 60 * 60,
            &live_container.join("grok-home"),
        );
        seed_execution_with_home(
            &db,
            &recent_item,
            "completed",
            now - 60,
            &recent_container.join("grok-home"),
        );

        // An unrecorded container under the root must never be touched.
        let orphan = homes_root.join("orphan-unrecorded");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("marker"), "y").unwrap();

        // Sanity: safety accepts the old terminal container under the env root.
        assert_grok_home_safe_to_delete(&old_container)
            .unwrap_or_else(|e| panic!("old_container must be safe to delete under homes root: {e:#}"));

        let listed = db.list_executions_with_driver_runtime_state().unwrap();
        assert_eq!(
            listed.len(),
            3,
            "three executions should carry recorded runtime state; got {listed:?}"
        );

        let policy = retention::GrokHomeRetentionPolicy::new(Duration::from_secs(14 * 24 * 60 * 60), u64::MAX);
        let outcome = run_one_pass_with_policy(&db, &policy, false).await;
        assert_eq!(outcome.errors, 0, "reclaim errors should be empty: {outcome:?}");
        assert_eq!(
            outcome.deleted, 1,
            "only the old terminal container is reclaimed: {outcome:?}"
        );
        assert_eq!(outcome.skipped_live, 1, "{outcome:?}");
        assert!(!old_container.exists(), "old terminal container must be gone");
        assert!(
            live_container.join("grok-home/marker").is_file(),
            "live container must survive"
        );
        assert!(
            recent_container.join("grok-home/marker").is_file(),
            "recent terminal container kept"
        );
        assert!(orphan.join("marker").is_file(), "unrecorded root must never be deleted");

        // Idempotent second pass.
        let outcome2 = run_one_pass_with_policy(&db, &policy, false).await;
        assert_eq!(outcome2.errors, 0, "{outcome2:?}");
        assert_eq!(
            outcome2.deleted, 1,
            "already-gone path still counts as reclaimed: {outcome2:?}"
        );
    }

    #[tokio::test]
    async fn sweep_is_noop_without_runtime_state() {
        let (_dir, db) = open_db();
        let outcome = run_one_pass(&db, false).await;
        assert_eq!(outcome, GrokHomeRetentionSweepOutcome::default());
    }

    #[tokio::test]
    async fn sweep_skips_non_grok_runtime_payloads() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "c");
        let execution = create_ready_chore_execution(&db, &chore.id);
        db.set_driver_runtime_state(
            &execution.id,
            Some(&DriverRuntimeState::new(serde_json::json!({"other": true}))),
        )
        .unwrap();
        let outcome = run_one_pass(&db, false).await;
        assert_eq!(outcome.scanned, 0);
        assert_eq!(outcome.deleted, 0);
    }
}
