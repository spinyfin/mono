//! Fixtures for tests of the merge poller, shared with the engine's own test
//! suite through the `test-support` feature.

use std::sync::Arc;

use tempfile::TempDir;

use crate::work::WorkDb;

pub use boss_execution_contracts::test_doubles::{NoopCube, RecordingPublisher};
pub use boss_work_db::test_support::{
    TEST_REPO_REMOTE_URL, create_active_chore, create_execution_started_now, create_execution_started_secs_ago,
    create_old_execution, create_product, create_ready_chore_execution, create_spawned_execution, create_test_chore,
    create_test_chore_manual, create_test_product, create_test_product_named, create_test_product_with_repo,
    finish_run_worker_pane_alive, insert_host_capability, log_capture, review_guide_source_packet,
    seed_daily_automation, seed_published_guide, seed_review_guide_series, seed_review_guide_series_for_pr,
    start_tmux_run,
};

/// Build identity stamped on fixture databases. The poller never compares it
/// with a running binary's, only with what the same database recorded.
const FIXTURE_BUILD_IDENTITY: &str = "merge-poller-test-build";

/// Drive the production deferred-admission pass with deterministic PR metadata.
pub async fn sweep_deferred_review_admission(
    db: &WorkDb,
    enqueuer: &dyn boss_observation_contracts::ReviewBatchEnqueuer,
) -> crate::SweepOutcome {
    let mut outcome = crate::SweepOutcome::default();
    crate::deferred_review_admission::sweep_deferred_review_admission_with(
        db,
        &RecordingPublisher::default(),
        enqueuer,
        None,
        16,
        &mut outcome,
    )
    .await;
    outcome
}

/// Open a fresh file-backed [`WorkDb`] under a throwaway [`TempDir`], stamped
/// with [`FIXTURE_BUILD_IDENTITY`]. The returned `TempDir` must outlive the
/// `WorkDb`: dropping it deletes the backing `state.db`.
pub fn open_db() -> (TempDir, WorkDb) {
    let (dir, db) = crate::work::test_support::open_db();
    (dir, db.with_engine_build_identity(FIXTURE_BUILD_IDENTITY))
}

/// Like [`open_db`], wrapped in an [`Arc`] for sharing with spawned tasks.
pub fn open_db_arc() -> (TempDir, Arc<WorkDb>) {
    let (dir, db) = open_db();
    (dir, Arc::new(db))
}
