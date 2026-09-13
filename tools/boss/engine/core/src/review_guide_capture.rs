//! Reconcile immutable source packets for the future PR review-guide runner.
//!
//! This module is intentionally the only lifecycle-facing entry point. It
//! allocates observation order before the asynchronous GitHub read, resolves
//! revisions to their canonical root, and records collection failures durably.
//! The packet crate owns pinned source collection and reference validation;
//! this module owns engine lifetime and reconciliation semantics.

use std::sync::Arc;

use boss_pr_review_sources::{PinnedComparison, collect_pinned_source_packet};

use crate::feature_flags::FeatureFlagsStore;
use crate::work::{PrSourceCapturePersistOutcome, PrSourceCaptureTrigger, WorkDb};

/// Operator-controlled rollout gate for automatic source capture.
pub const REVIEW_GUIDE_SOURCE_CAPTURE_FLAG: &str = "review_guide_source_capture";

/// One durable source-capture request formed by a verified lifecycle seam.
/// Optional probe data makes creation/completion fetch fresh metadata while a
/// successful poller probe supplies its exact immutable endpoints.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct SourceCaptureRequest {
    pub root_task_id: String,
    pub pr_url: String,
    pub trigger: PrSourceCaptureTrigger,
    pub observed: Option<PinnedComparison>,
    pub expected_head_branch: Option<String>,
    pub observation_sequence: Option<i64>,
}

/// Request capture from an execution-owned lifecycle seam. Revisions are
/// collapsed to their root before the asynchronous read begins, preserving
/// one canonical PR series across the entire implementation chain.
pub(crate) fn reconcile_review_guide_source_for_execution(
    work_db: Arc<WorkDb>,
    feature_flags: Arc<FeatureFlagsStore>,
    execution_id: &str,
    pr_url: &str,
    trigger: PrSourceCaptureTrigger,
) {
    if !source_capture_enabled(&feature_flags) {
        return;
    }
    let execution = match work_db.get_execution(execution_id) {
        Ok(execution) => execution,
        Err(error) => {
            tracing::warn!(
                execution_id,
                pr_url,
                ?error,
                "review-guide source capture: could not load execution for association verification",
            );
            return;
        }
    };
    let expected_head_branch = (execution.kind != boss_protocol::ExecutionKind::RevisionImplementation).then(|| {
        crate::completion::expected_branch_name(
            execution_id,
            &execution.branch_naming,
            execution.worker_branch_prefix.as_deref(),
        )
    });
    let root_task_id = match work_db.review_guide_source_root_for_execution(execution_id) {
        Ok(root_task_id) => root_task_id,
        Err(error) => {
            tracing::warn!(
                execution_id,
                pr_url,
                ?error,
                "review-guide source capture: could not resolve canonical root",
            );
            return;
        }
    };
    reconcile_review_guide_source(
        work_db,
        feature_flags,
        SourceCaptureRequest::builder()
            .root_task_id(root_task_id)
            .pr_url(pr_url)
            .trigger(trigger)
            .maybe_expected_head_branch(expected_head_branch)
            .build(),
    );
}

/// Reconcile a root-owned source capture, optionally using a sequence
/// allocated when a caller's
/// lifecycle probe started. This lets the merge poller preserve observation
/// order even if GitHub responds to a newer probe before an older one.
pub(crate) fn reconcile_review_guide_source(
    work_db: Arc<WorkDb>,
    feature_flags: Arc<FeatureFlagsStore>,
    request: SourceCaptureRequest,
) {
    if !source_capture_enabled(&feature_flags) {
        return;
    }
    let SourceCaptureRequest {
        root_task_id,
        pr_url,
        trigger,
        observed,
        expected_head_branch,
        observation_sequence,
    } = request;
    let observation_sequence = match observation_sequence {
        Some(sequence) => sequence,
        None => match work_db.allocate_pr_review_guide_source_observation_sequence() {
            Ok(sequence) => sequence,
            Err(error) => {
                tracing::warn!(
                    root_task_id,
                    pr_url,
                    ?error,
                    "review-guide source capture: could not allocate observation sequence",
                );
                return;
            }
        },
    };
    tokio::spawn(async move {
        let observed = match observed {
            Some(observed) => observed,
            None => match boss_github::pr_files::fetch_pr_comparison_metadata(&pr_url).await {
                Ok(metadata) => PinnedComparison {
                    base_sha: metadata.base_sha,
                    head_sha: metadata.head_sha,
                },
                Err(error) => {
                    record_capture_failure(&work_db, &root_task_id, &pr_url, observation_sequence, error);
                    return;
                }
            },
        };
        match collect_pinned_source_packet(&pr_url, &observed, expected_head_branch.as_deref()).await {
            Ok(packet) => match work_db.persist_pr_review_guide_source_capture(
                &root_task_id,
                observation_sequence,
                trigger,
                &packet,
            ) {
                Ok(PrSourceCapturePersistOutcome::Stored(capture)) => tracing::info!(
                    root_task_id,
                    pr_url,
                    observation_sequence,
                    packet_hash = %capture.packet_hash,
                    complete = capture.complete,
                    "review-guide source capture: stored immutable comparison packet",
                ),
                Ok(PrSourceCapturePersistOutcome::Existing(capture)) => tracing::debug!(
                    root_task_id,
                    pr_url,
                    observation_sequence,
                    existing_sequence = capture.observation_sequence,
                    "review-guide source capture: comparison packet already present",
                ),
                Ok(PrSourceCapturePersistOutcome::IgnoredStaleObservation) => tracing::debug!(
                    root_task_id,
                    pr_url,
                    observation_sequence,
                    "review-guide source capture: ignored delayed observation",
                ),
                Err(error) => tracing::warn!(
                    root_task_id,
                    pr_url,
                    observation_sequence,
                    ?error,
                    "review-guide source capture: could not persist packet",
                ),
            },
            Err(error) => record_capture_failure(&work_db, &root_task_id, &pr_url, observation_sequence, error),
        }
    });
}

fn source_capture_enabled(feature_flags: &FeatureFlagsStore) -> bool {
    feature_flags.is_enabled(REVIEW_GUIDE_SOURCE_CAPTURE_FLAG)
}

fn record_capture_failure(
    work_db: &WorkDb,
    root_task_id: &str,
    pr_url: &str,
    observation_sequence: i64,
    error: impl std::fmt::Display,
) {
    let error = error.to_string();
    match work_db.record_pr_review_guide_source_capture_failure(root_task_id, pr_url, observation_sequence, &error) {
        Ok(true) => tracing::warn!(
            root_task_id,
            pr_url,
            observation_sequence,
            error,
            "review-guide source capture: recorded collection failure",
        ),
        Ok(false) => tracing::debug!(
            root_task_id,
            pr_url,
            observation_sequence,
            "review-guide source capture: ignored delayed collection failure",
        ),
        Err(persist_error) => tracing::warn!(
            root_task_id,
            pr_url,
            observation_sequence,
            ?persist_error,
            original_error = %error,
            "review-guide source capture: could not record collection failure",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature_flags::FeatureFlagsStore;
    use crate::test_support::open_db;

    #[test]
    fn automatic_capture_defaults_off_and_respects_the_rollout_flag() {
        let directory = tempfile::tempdir().unwrap();
        let flags = FeatureFlagsStore::new(directory.path().join("feature-flags.toml"));
        flags.load().unwrap();
        assert!(!source_capture_enabled(&flags));
        flags.set(REVIEW_GUIDE_SOURCE_CAPTURE_FLAG, true).unwrap();
        assert!(source_capture_enabled(&flags));
    }

    #[test]
    fn disabled_reconciler_does_not_allocate_or_spawn_collection() {
        let (_directory, work_db) = open_db();
        let work_db = Arc::new(work_db);
        let flag_directory = tempfile::tempdir().unwrap();
        let flags = Arc::new(FeatureFlagsStore::new(flag_directory.path().join("feature-flags.toml")));
        reconcile_review_guide_source(
            work_db.clone(),
            flags,
            SourceCaptureRequest::builder()
                .root_task_id("unreachable-root")
                .pr_url("https://github.com/acme/widget/pull/25")
                .trigger(PrSourceCaptureTrigger::Creation)
                .observed(PinnedComparison {
                    base_sha: "base".to_owned(),
                    head_sha: "head".to_owned(),
                })
                .build(),
        );
        assert_eq!(
            work_db.allocate_pr_review_guide_source_observation_sequence().unwrap(),
            1
        );
    }
}
