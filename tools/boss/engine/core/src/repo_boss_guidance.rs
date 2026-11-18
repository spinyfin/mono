//! The engine's read of a product repo's `BOSS.md` for worker prompts.
//!
//! This is the engine half of [`boss_execution_runner::repo_guidance`]: it
//! implements the [`RepoGuidanceSource`] seam the prompt composer asks, using
//! the same GitHub read, size cap and last-known-good cache as the
//! coordinator's `BOSS_COORDINATOR.md` ([`crate::coordinator_guidance`]) with
//! only the file and its cache namespace changed.
//!
//! The read happens at dispatch time, on the spawn path, so it is bounded by
//! [`DISPATCH_FETCH_BUDGET`]: a GitHub that is slow or down turns into the
//! cached copy or a visible note in the prompt, never into a stalled or
//! refused dispatch.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use boss_engine_design_docs::DesignDocsService;
use boss_execution_runner::repo_guidance::RepoGuidanceSource;
use boss_protocol::{CoordinatorGuidanceView, Product};

use crate::coordinator_guidance::{REPO_BOSS_FILE, load_product_guidance_file};
use crate::work::WorkDb;

/// Bound on one dispatch-time `BOSS.md` read. A cold read is two sequential
/// `gh api` calls (HEAD sha, then the blob) and a warm one is a single call;
/// on overrun the read is reported as failed and the last-known-good copy, or
/// a visible note, is used instead.
pub const DISPATCH_FETCH_BUDGET: Duration = Duration::from_secs(10);

/// Reads `BOSS.md` for a product through the engine's design-docs GitHub
/// service, persisting and falling back to the last-known-good copy in the
/// work database.
pub struct RepoBossGuidanceLoader {
    design_docs: Arc<DesignDocsService>,
    work_db: Arc<WorkDb>,
}

impl RepoBossGuidanceLoader {
    pub fn new(design_docs: Arc<DesignDocsService>, work_db: Arc<WorkDb>) -> Self {
        Self { design_docs, work_db }
    }
}

impl std::fmt::Debug for RepoBossGuidanceLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoBossGuidanceLoader").finish_non_exhaustive()
    }
}

#[async_trait]
impl RepoGuidanceSource for RepoBossGuidanceLoader {
    async fn read(&self, product: &Product) -> CoordinatorGuidanceView {
        let mut views = load_product_guidance_file(
            REPO_BOSS_FILE,
            &self.design_docs,
            &self.work_db,
            std::slice::from_ref(product),
            DISPATCH_FETCH_BUDGET,
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .await;
        // One view per input product, always.
        views.remove(0)
    }
}
