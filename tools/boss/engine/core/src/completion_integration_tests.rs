//! Integration tests that drive the completion handler together with services
//! only the engine composes: the merge poller's sweeps, the terminal-work
//! sweep, conflict watch, review-guide validation and review-verdict
//! application.
//!
//! These tests use the completion crate's test-support harness and inject
//! the engine's concrete collaborators.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use boss_engine_completion::tests::*;
// Both the fixture harness and the shared test support define this wrapper;
// the harness's (which takes the optional summary) is the one these scenarios use.
use boss_engine_completion::tests::finish_run_worker_pane_alive;
use boss_engine_completion::*;
use boss_execution_contracts::ExecutionPublisher;
use boss_host_execution::types::{CubeRepoSummary, CubeWorkspaceStatus};
use boss_metrics::Registry;
use boss_observation_contracts::{
    MergeProbe, NoopMergeProbe, OpenPrMergeability, PrPollStateWriter, ReviewBatchEnqueuer,
};
use boss_protocol::{BranchNaming, ExecutionKind, ExecutionStatus, ProposalKind, ReviewLanguageBucket, ReviewProfile};
use tempfile::{TempDir, tempdir};
use tokio::sync::Mutex;

use crate::test_support::*;
use crate::work::{
    ConflictResolutionInsertInput, CreateExecutionInput, FakePrStateChecker, PendingMergeCheck, PrOpenState,
    PrStateChecker, ReviewBatchCreateInput, ReviewBatchMemberCreateInput, ReviewBatchMemberRole,
    ReviewBatchMemberStatus, ReviewBatchPhase, ReviewClassification, SubmitWorkerProposalInput, TaskStatus, WorkDb,
    WorkItem, WorkItemPatch, WorkerPrCompletionTarget,
};

/// The completion fixture harness, wired to the engine's real detectors and
/// validators rather than the crate fixtures' no-op collaborators.
fn engine_harness(db: Arc<WorkDb>, detector: Arc<dyn PrDetector>) -> TestHarness {
    let mut harness = TestHarness::new(db, detector);
    harness.handler = harness
        .handler
        .with_collaborators(crate::completion_adapters::completion_collaborators());
    harness
}

/// [`integration_handler`] with the engine's real collaborators.
fn engine_integration_handler(db: Arc<WorkDb>, pr_url: &str) -> WorkerCompletionHandler {
    integration_handler(db, pr_url).with_collaborators(crate::completion_adapters::completion_collaborators())
}

mod conflict_watch;
mod followup_detection;
mod poller;
mod review_guide;
mod review_guide_links;
mod review_verdict_apply;
mod teardown_sweep_race;
