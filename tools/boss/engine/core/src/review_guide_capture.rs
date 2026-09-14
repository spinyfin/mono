//! Reconcile immutable source packets for the future PR review-guide runner.
//!
//! This module is intentionally the only lifecycle-facing entry point. It
//! allocates observation order before the asynchronous GitHub read, resolves
//! revisions to their canonical root, and records collection failures durably.
//! The packet crate owns pinned source collection and reference validation;
//! this module owns engine lifetime and reconciliation semantics.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use boss_github::pr_files::PrComparisonMetadata;
use boss_pr_review_sources::{
    PinnedComparison, SourcePacket, collect_pinned_source_packet, collect_pinned_source_packet_with_metadata,
};

use crate::feature_flags::FeatureFlagsStore;
use crate::work::{PrReviewGuideSourceCapture, PrSourceCapturePersistOutcome, PrSourceCaptureTrigger, WorkDb};

/// Operator-controlled rollout gate for automatic source capture.
pub const REVIEW_GUIDE_SOURCE_CAPTURE_FLAG: &str = "review_guide_source_capture";

/// Operator-controlled rollout gate for launching generation jobs from
/// captured comparisons. Independent of [`REVIEW_GUIDE_SOURCE_CAPTURE_FLAG`]
/// so capture and generation can be staged separately — generation simply
/// has nothing to consume while capture alone is enabled.
pub const REVIEW_GUIDE_GENERATION_FLAG: &str = "review_guide_generation";

type SourcePacketFuture = Pin<Box<dyn Future<Output = Result<SourcePacket>> + Send>>;
pub(crate) type PacketCollectFn = Arc<
    dyn Fn(String, Option<PinnedComparison>, Option<String>, Option<PrComparisonMetadata>) -> SourcePacketFuture
        + Send
        + Sync,
>;

type MetadataFuture = Pin<Box<dyn Future<Output = Result<PrComparisonMetadata>> + Send>>;
#[derive(Clone)]
pub struct SourcePacketCollector {
    collect: PacketCollectFn,
    metadata: Arc<dyn Fn(String, Option<String>) -> MetadataFuture + Send + Sync>,
}

impl SourcePacketCollector {
    #[cfg(test)]
    pub(crate) fn fixture(collect: PacketCollectFn, packet: SourcePacket) -> Self {
        Self {
            collect,
            metadata: Arc::new(move |_, _| {
                let packet = packet.clone();
                Box::pin(async move {
                    Ok(PrComparisonMetadata {
                        number: packet.pr_number,
                        title: packet.title,
                        body: packet.body,
                        base_repository: packet.base_repository,
                        head_repository: packet.head_repository,
                        head_ref_name: "fixture".to_owned(),
                        base_sha: packet.observed_base_sha,
                        head_sha: packet.head_sha,
                        changed_files: packet.files.len() as u64,
                    })
                })
            }),
        }
    }
}

type CaptureKey = (String, String, String);

type CaptureMap = HashMap<CaptureKey, Arc<Mutex<i64>>>;

fn in_flight_captures() -> &'static Mutex<CaptureMap> {
    static SET: OnceLock<Mutex<CaptureMap>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn github_source_packet_collector() -> SourcePacketCollector {
    let collect: PacketCollectFn = Arc::new(|pr_url, observed, expected_head_branch, metadata| {
        Box::pin(async move {
            match (observed, metadata) {
                (observed, Some(metadata)) => {
                    let independently_observed = observed.is_some();
                    let observed = observed.unwrap_or(PinnedComparison {
                        base_sha: metadata.base_sha.clone(),
                        head_sha: metadata.head_sha.clone(),
                    });
                    collect_pinned_source_packet_with_metadata(
                        &pr_url,
                        &observed,
                        expected_head_branch.as_deref(),
                        metadata,
                        independently_observed,
                    )
                    .await
                }
                (Some(observed), None) => {
                    collect_pinned_source_packet(&pr_url, &observed, expected_head_branch.as_deref()).await
                }
                (None, None) => {
                    let metadata = boss_github::pr_files::fetch_pr_comparison_metadata(&pr_url).await?;
                    let observed = PinnedComparison {
                        base_sha: metadata.base_sha.clone(),
                        head_sha: metadata.head_sha.clone(),
                    };
                    collect_pinned_source_packet_with_metadata(
                        &pr_url,
                        &observed,
                        expected_head_branch.as_deref(),
                        metadata,
                        false,
                    )
                    .await
                }
            }
        })
    });
    SourcePacketCollector {
        collect,
        metadata: Arc::new(|url, expected_branch| {
            Box::pin(async move {
                let metadata = boss_github::pr_files::fetch_pr_comparison_metadata(&url).await?;
                if let Some(expected) = expected_branch {
                    anyhow::ensure!(
                        metadata.head_ref_name == expected,
                        "PR head branch changed or belongs to another execution"
                    );
                }
                Ok(metadata)
            })
        }),
    }
}

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

pub(crate) fn reconcile_review_guide_source_for_execution_with_collector(
    work_db: Arc<WorkDb>,
    feature_flags: Arc<FeatureFlagsStore>,
    execution_id: &str,
    pr_url: &str,
    trigger: PrSourceCaptureTrigger,
    observed: Option<PinnedComparison>,
    collector: SourcePacketCollector,
) -> Option<tokio::task::JoinHandle<()>> {
    if !source_capture_enabled(&feature_flags) {
        return None;
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
            return None;
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
            return None;
        }
    };
    reconcile_review_guide_source_with_collector(
        work_db,
        feature_flags,
        SourceCaptureRequest::builder()
            .root_task_id(root_task_id)
            .pr_url(pr_url)
            .trigger(trigger)
            .maybe_observed(observed)
            .maybe_expected_head_branch(expected_head_branch)
            .build(),
        collector,
    )
}

pub(crate) fn reconcile_review_guide_source_with_collector(
    work_db: Arc<WorkDb>,
    feature_flags: Arc<FeatureFlagsStore>,
    request: SourceCaptureRequest,
    collector: SourcePacketCollector,
) -> Option<tokio::task::JoinHandle<()>> {
    if !source_capture_enabled(&feature_flags) {
        return None;
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
                return None;
            }
        },
    };
    let guard = if let Some(endpoints) = &observed {
        match prepare_capture(&work_db, &root_task_id, &pr_url, endpoints, observation_sequence) {
            Ok(Some(guard)) => Some(guard),
            Ok(None) => return None,
            Err(error) => {
                record_capture_failure(&work_db, &root_task_id, &pr_url, observation_sequence, error);
                return None;
            }
        }
    } else {
        None
    };
    Some(tokio::spawn(async move {
        // Own the guard before the future is polled, including cancellation.
        let guard = guard;
        boss_gh_telemetry::scope(boss_gh_telemetry::callers::REVIEW_GUIDE_SOURCE_CAPTURE, async move {
            let metadata = match (collector.metadata)(pr_url.clone(), expected_head_branch.clone()).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    record_capture_failure(&work_db, &root_task_id, &pr_url, observation_sequence, error);
                    return;
                }
            };
            if let Some(observed) = &observed
                && observed.head_sha != metadata.head_sha
            {
                record_capture_failure(
                    &work_db,
                    &root_task_id,
                    &pr_url,
                    observation_sequence,
                    format!(
                        "PR head changed while collecting sources: observed {} but metadata returned {}",
                        observed.head_sha, metadata.head_sha
                    ),
                );
                return;
            }
            // Comparison identity is REST `base.sha` + head, not a GraphQL
            // `baseRefOid` that may track the live base tip.
            let rest_identity = PinnedComparison {
                base_sha: metadata.base_sha.clone(),
                head_sha: metadata.head_sha.clone(),
            };
            let mut guard = match guard {
                Some(guard) if observed.as_ref().is_some_and(|o| o.base_sha == rest_identity.base_sha) => guard,
                Some(_) | None => {
                    match prepare_capture(&work_db, &root_task_id, &pr_url, &rest_identity, observation_sequence) {
                        Ok(Some(guard)) => guard,
                        Ok(None) => return,
                        Err(error) => {
                            record_capture_failure(&work_db, &root_task_id, &pr_url, observation_sequence, error);
                            return;
                        }
                    }
                }
            };
            let metadata = Some(metadata);
            let result = (collector.collect)(pr_url.clone(), observed, expected_head_branch, metadata).await;
            // Coalesced observations update this sequence; hold it through persistence.
            let mut captures = in_flight_captures().lock().unwrap_or_else(|error| error.into_inner());
            let sequence = guard.sequence.lock().unwrap_or_else(|error| error.into_inner());
            let observation_sequence = *sequence;
            match result {
                Ok(packet) => match work_db.persist_pr_review_guide_source_capture(
                    &root_task_id,
                    observation_sequence,
                    trigger,
                    &packet,
                ) {
                    Ok(PrSourceCapturePersistOutcome::Stored(capture)) => {
                        tracing::info!(
                            root_task_id,
                            pr_url,
                            observation_sequence,
                            packet_hash = %capture.packet_hash,
                            complete = capture.complete,
                            "review-guide source capture: stored immutable comparison packet",
                        );
                        enqueue_review_guide_generation(&work_db, &feature_flags, &capture);
                    }
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
            captures.remove(&guard.key);
            guard.active = false;
        })
        .await;
    }))
}

/// Enqueue one durable `pr_review_guide` generation attempt for a freshly
/// stored/upgraded comparison — the connection point between task 1's
/// immutable source capture and this task's execution machinery. Gated
/// independently by [`REVIEW_GUIDE_GENERATION_FLAG`]. Best-effort: any
/// failure here is logged and dropped rather than propagated, matching every
/// other outcome in this reconciler — a missed enqueue is recoverable (the
/// next observation, or an explicit retry, tries again), while losing the
/// packet that was just durably captured would not be.
fn enqueue_review_guide_generation(
    work_db: &WorkDb,
    feature_flags: &FeatureFlagsStore,
    capture: &PrReviewGuideSourceCapture,
) {
    if !feature_flags.is_enabled(REVIEW_GUIDE_GENERATION_FLAG) {
        return;
    }
    match work_db.live_or_queued_pr_review_guide_execution_for_comparison(&capture.comparison_id) {
        Ok(Some(_)) => {
            tracing::debug!(
                comparison_id = %capture.comparison_id,
                "review-guide generation: a job for this exact comparison is already in flight; not enqueuing another",
            );
            return;
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                comparison_id = %capture.comparison_id,
                ?error,
                "review-guide generation: could not check for a live execution; skipping enqueue",
            );
            return;
        }
    }
    let repo_remote_url = match work_db.get_work_item(&capture.root_task_id) {
        Ok(item) => match item {
            crate::work::WorkItem::Task(task) | crate::work::WorkItem::Chore(task) => task.repo_remote_url,
            _ => None,
        },
        Err(error) => {
            tracing::warn!(
                root_task_id = %capture.root_task_id,
                ?error,
                "review-guide generation: could not resolve the root task's repository; skipping enqueue",
            );
            return;
        }
    };
    let Some(repo_remote_url) = repo_remote_url else {
        tracing::warn!(
            root_task_id = %capture.root_task_id,
            "review-guide generation: root task has no repository; skipping enqueue",
        );
        return;
    };
    let attempt = match work_db.create_pr_review_guide_attempt(
        &capture.series_id,
        &capture.comparison_id,
        boss_review_guide::PROMPT_VERSION,
    ) {
        Ok(attempt) => attempt,
        Err(error) => {
            tracing::warn!(
                comparison_id = %capture.comparison_id,
                ?error,
                "review-guide generation: could not create a durable attempt",
            );
            return;
        }
    };
    let execution = match work_db.create_pr_review_guide_execution(&capture.comparison_id, &repo_remote_url) {
        Ok(execution) => execution,
        Err(error) => {
            tracing::warn!(
                comparison_id = %capture.comparison_id,
                attempt_id = %attempt.id,
                ?error,
                "review-guide generation: could not create the execution row",
            );
            return;
        }
    };
    if let Err(error) = work_db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id) {
        tracing::warn!(
            attempt_id = %attempt.id,
            execution_id = %execution.id,
            ?error,
            "review-guide generation: could not bind the attempt to its execution",
        );
        return;
    }
    tracing::info!(
        comparison_id = %capture.comparison_id,
        attempt_id = %attempt.id,
        execution_id = %execution.id,
        "review-guide generation: enqueued a durable generation attempt",
    );
}

fn prepare_capture(
    db: &WorkDb,
    root: &str,
    url: &str,
    endpoints: &PinnedComparison,
    sequence: i64,
) -> Result<Option<InFlightGuard>> {
    if db.select_complete_pr_review_guide_source_capture(
        root,
        url,
        &endpoints.base_sha,
        &endpoints.head_sha,
        sequence,
    )? {
        return Ok(None);
    }
    let mut captures = in_flight_captures().lock().unwrap_or_else(|error| error.into_inner());
    if db.select_complete_pr_review_guide_source_capture(
        root,
        url,
        &endpoints.base_sha,
        &endpoints.head_sha,
        sequence,
    )? {
        return Ok(None);
    }
    Ok(InFlightGuard::acquire_locked(
        &mut captures,
        (
            format!("{root}:{url}"),
            endpoints.base_sha.clone(),
            endpoints.head_sha.clone(),
        ),
        sequence,
    ))
}

struct InFlightGuard {
    key: CaptureKey,
    active: bool,
    sequence: Arc<Mutex<i64>>,
}

impl InFlightGuard {
    #[cfg(test)]
    fn acquire(key: CaptureKey, sequence: i64) -> Option<Self> {
        let mut captures = in_flight_captures().lock().unwrap_or_else(|error| error.into_inner());
        Self::acquire_locked(&mut captures, key, sequence)
    }

    fn acquire_locked(captures: &mut CaptureMap, key: CaptureKey, sequence: i64) -> Option<Self> {
        if let Some(existing) = captures.get(&key) {
            let mut latest = existing.lock().unwrap_or_else(|error| error.into_inner());
            *latest = (*latest).max(sequence);
            return None;
        }
        let sequence = Arc::new(Mutex::new(sequence));
        captures.insert(key.clone(), sequence.clone());
        Some(Self {
            key,
            active: true,
            sequence,
        })
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if self.active {
            in_flight_captures()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&self.key);
        }
    }
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
    use crate::test_support::{create_active_chore, create_product, open_db};
    use crate::work::{FakePrStateChecker, PrOpenState};
    use boss_protocol::{CreateExecutionInput, CreateRevisionInput, ExecutionKind, ExecutionStatus, WorkItemPatch};

    #[test]
    fn unpolled_future_releases_its_claim_and_coalesces_sequences() {
        let key = ("cancel-test".to_owned(), "base".to_owned(), "head".to_owned());
        let guard = InFlightGuard::acquire(key.clone(), 1).unwrap();
        assert!(InFlightGuard::acquire(key.clone(), 4).is_none());
        assert_eq!(*guard.sequence.lock().unwrap(), 4);
        let future = async move {
            let _guard = guard;
        };
        drop(future);
        assert!(InFlightGuard::acquire(key, 5).is_some());
    }

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
        reconcile_review_guide_source_with_collector(
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
            github_source_packet_collector(),
        );
        assert_eq!(
            work_db.allocate_pr_review_guide_source_observation_sequence().unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn execution_reconciler_persists_a_revision_capture_on_its_canonical_root() {
        let (_directory, work_db) = open_db();
        let work_db = Arc::new(work_db);
        let product = create_product(&work_db);
        let root = create_active_chore(&work_db, &product, "root source capture");
        let pr_url = "https://github.com/acme/widget/pull/25";
        work_db
            .update_work_item(
                &root,
                WorkItemPatch {
                    status: Some("in_review".to_owned()),
                    pr_url: Some(pr_url.to_owned()),
                    ..Default::default()
                },
            )
            .unwrap();
        let revision = work_db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(root.clone())
                    .description("refresh the implementation")
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
            )
            .unwrap();
        let execution = work_db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(revision.id)
                    .kind(ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let flag_directory = tempfile::tempdir().unwrap();
        let flags = Arc::new(FeatureFlagsStore::new(flag_directory.path().join("feature-flags.toml")));
        flags.set(REVIEW_GUIDE_SOURCE_CAPTURE_FLAG, true).unwrap();
        let packet = SourcePacket {
            schema_version: 3,
            canonical_pr_url: pr_url.to_owned(),
            pr_number: 25,
            title: "Captured revision".to_owned(),
            body: None,
            base_repository: "acme/widget".to_owned(),
            head_repository: "acme/widget".to_owned(),
            observed_base_sha: "base".to_owned(),
            probe_base_sha: None,
            merge_base_sha: "merge-base".to_owned(),
            head_sha: "head".to_owned(),
            files: Vec::new(),
            omissions: Vec::new(),
        };
        let fixture_packet = packet.clone();
        let collect: PacketCollectFn = Arc::new(move |url, observed, expected_head_branch, _metadata| {
            let packet = packet.clone();
            Box::pin(async move {
                let Some(observed) = observed else {
                    anyhow::bail!("execution reconciler supplied unexpected comparison identity");
                };
                if url != packet.canonical_pr_url || observed.base_sha != "base" || observed.head_sha != "head" {
                    anyhow::bail!("execution reconciler supplied unexpected comparison identity");
                }
                if expected_head_branch.is_some() {
                    anyhow::bail!("revision capture must not require an implementation branch name");
                }
                Ok(packet)
            })
        });
        let collector = SourcePacketCollector::fixture(collect, fixture_packet);
        let handle = reconcile_review_guide_source_for_execution_with_collector(
            work_db.clone(),
            flags,
            &execution.id,
            pr_url,
            PrSourceCaptureTrigger::Completion,
            Some(PinnedComparison {
                base_sha: "base".to_owned(),
                head_sha: "head".to_owned(),
            }),
            collector,
        )
        .expect("enabled execution reconciliation must start collection");
        handle.await.unwrap();

        let capture = work_db
            .get_latest_pr_review_guide_source_capture(&root)
            .unwrap()
            .expect("capture persisted on canonical root");
        assert_eq!(capture.trigger, "completion");
        assert_eq!(capture.packet.head_sha, "head");
    }
}
