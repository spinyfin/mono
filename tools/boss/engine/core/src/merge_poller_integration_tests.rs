//! Integration tests that drive the merge poller's sweeps together with services
//! only the engine composes: the completion handler, conflict watch with its
//! bounded remediator, the design detector and the Populator.
//!
//! These use the poller crate's fixture harness and run the sweep against the
//! engine's concrete [`CoreSweepHost`](crate::merge_poller_adapters::CoreSweepHost).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use boss_dispatch_events::RecordingDispatchEventSink;
use boss_engine_merge_poller::tests::*;
use boss_engine_merge_poller::*;
use boss_execution_contracts::CubeClient;
use boss_protocol::{ExecutionKind, TaskKind};
use tempfile::tempdir;

use crate::completion::{
    PaneReleaseOutcome, PrDetector, PrStatus, ProbeQueuer, WorkerCompletionHandler, WorkerPaneReleaser,
};
use crate::merge_poller_adapters::CoreSweepHost;
use crate::test_support::*;
use crate::work::{
    AddDependencyInput, ConflictResolutionInsertInput, ExecutionStatus, PendingMergeCheck, TaskStatus, WorkDb,
    WorkItem, WorkItemPatch,
};

/// The sweep host a production loop runs against, without a background
/// remediator: a conflict observed with the ladder enabled runs it inline.
fn core_host() -> CoreSweepHost {
    CoreSweepHost::with_remediation(None)
}

struct FixedPrDetector(Option<String>);

#[async_trait]
impl PrDetector for FixedPrDetector {
    async fn detect_pr(&self, _repo_remote_url: &str, _expected_branch: &str) -> Result<PrStatus> {
        Ok(match &self.0 {
            Some(url) => PrStatus::Fresh { url: url.clone() },
            None => PrStatus::None,
        })
    }
}

struct NoopPaneReleaser;

#[async_trait]
impl WorkerPaneReleaser for NoopPaneReleaser {
    async fn release_pane(&self, _run_id: &str) -> PaneReleaseOutcome {
        PaneReleaseOutcome::Reaped
    }
}

struct NoopProbeQueuer;

impl ProbeQueuer for NoopProbeQueuer {
    fn queue_probe(&self, _run_id: &str, _text: &str) {}
    fn clear_pending_probes(&self, _run_id: &str, _reason: &str) {}
}

mod converted_followup;
mod remediation;
mod source_capture;
mod stale_revision_sweep;
mod sweep;
mod unmergeable_reaction;
