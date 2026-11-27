//! Reconcile immutable source packets for the future PR review-guide runner.
//!
//! This module is intentionally the only lifecycle-facing entry point. It
//! allocates observation order before the asynchronous GitHub read, resolves
//! revisions to their canonical root, and records collection failures durably.
//! The packet crate owns pinned source collection and reference validation;
//! this module owns engine lifetime and reconciliation semantics.

#[cfg(test)]
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use boss_event_bus::{Event, EventBus, EventKind, TopicFilter};
use boss_github::pr_files::PrComparisonMetadata;
use boss_pr_review_sources::{
    PinnedComparison, SourcePacket, collect_pinned_source_packet, collect_pinned_source_packet_with_metadata,
};
use boss_protocol::{ExecutionKind, ExecutionStatus};

use crate::feature_flags::{ExperimentArm, FeatureFlagsStore, REVIEW_GUIDE_WHEN_READY_EXPERIMENT, assign_arm};
use crate::work::{
    PrReviewGuideSourceCapture, PrSourceCapturePersistOutcome, PrSourceCaptureTrigger, WorkDb, experiment_bucket,
};

/// Operator-controlled rollout gate for automatic source capture.
pub const REVIEW_GUIDE_SOURCE_CAPTURE_FLAG: &str = "review_guide_source_capture";

/// Operator-controlled rollout gate for launching generation jobs from
/// captured comparisons. Independent of [`REVIEW_GUIDE_SOURCE_CAPTURE_FLAG`]
/// so capture and generation can be staged separately — generation simply
/// has nothing to consume while capture alone is enabled.
pub const REVIEW_GUIDE_GENERATION_FLAG: &str = "review_guide_generation";

type SourcePacketFuture = Pin<Box<dyn Future<Output = Result<SourcePacket>> + Send>>;
pub type PacketCollectFn = Arc<
    dyn Fn(String, Option<PinnedComparison>, Option<String>, Option<PrComparisonMetadata>) -> SourcePacketFuture
        + Send
        + Sync,
>;

type MetadataFuture = Pin<Box<dyn Future<Output = Result<PrComparisonMetadata>> + Send>>;
type LiveHeadFuture = Pin<Box<dyn Future<Output = Result<String>> + Send>>;
/// Stops a cancelled `pr_review_guide` worker after admission supersedes it.
pub type CancelledGuideStopFn = Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
#[derive(Clone)]
pub struct SourcePacketCollector {
    collect: PacketCollectFn,
    pub metadata: Arc<dyn Fn(String, Option<String>) -> MetadataFuture + Send + Sync>,
    pub live_head: Arc<dyn Fn(String) -> LiveHeadFuture + Send + Sync>,
}

impl SourcePacketCollector {
    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture(collect: PacketCollectFn, packet: SourcePacket) -> Self {
        let metadata_base = packet.observed_base_sha.clone();
        Self::fixture_with_metadata_base(collect, packet, metadata_base)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture_with_metadata_base(collect: PacketCollectFn, packet: SourcePacket, metadata_base: String) -> Self {
        let live_head_sha = packet.head_sha.clone();
        Self {
            collect,
            metadata: Arc::new(move |_, _| {
                let packet = packet.clone();
                let metadata_base = metadata_base.clone();
                Box::pin(async move {
                    Ok(PrComparisonMetadata {
                        number: packet.pr_number,
                        title: packet.title,
                        body: packet.body,
                        base_repository: packet.base_repository,
                        head_repository: packet.head_repository,
                        head_ref_name: "fixture".to_owned(),
                        base_sha: metadata_base,
                        head_sha: packet.head_sha,
                        changed_files: packet.files.len() as u64,
                    })
                })
            }),
            live_head: Arc::new(move |_| {
                let live_head_sha = live_head_sha.clone();
                Box::pin(async move { Ok(live_head_sha) })
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

#[cfg(test)]
type AfterCapturePersistHook = Box<dyn FnMut(&CaptureKey)>;
#[cfg(test)]
type BeforeInFlightClaimReleaseHook = Box<dyn FnMut(&Arc<Mutex<i64>>)>;

#[cfg(test)]
thread_local! {
    static AFTER_CAPTURE_PERSIST: RefCell<Option<AfterCapturePersistHook>> = const { RefCell::new(None) };
    static BEFORE_IN_FLIGHT_CLAIM_RELEASE: RefCell<Option<BeforeInFlightClaimReleaseHook>> =
        const { RefCell::new(None) };
}

#[cfg(test)]
fn set_after_capture_persist_hook(hook: AfterCapturePersistHook) {
    AFTER_CAPTURE_PERSIST.with(|slot| *slot.borrow_mut() = Some(hook));
}

#[cfg(test)]
fn clear_after_capture_persist_hook() {
    AFTER_CAPTURE_PERSIST.with(|slot| *slot.borrow_mut() = None);
}

#[cfg(test)]
fn run_after_capture_persist(key: &CaptureKey) {
    AFTER_CAPTURE_PERSIST.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook(key);
        }
    });
}

#[cfg(test)]
fn set_before_in_flight_claim_release_hook(hook: BeforeInFlightClaimReleaseHook) {
    BEFORE_IN_FLIGHT_CLAIM_RELEASE.with(|slot| *slot.borrow_mut() = Some(hook));
}

#[cfg(test)]
fn clear_before_in_flight_claim_release_hook() {
    BEFORE_IN_FLIGHT_CLAIM_RELEASE.with(|slot| *slot.borrow_mut() = None);
}

#[cfg(test)]
fn run_before_in_flight_claim_release(sequence: &Arc<Mutex<i64>>) {
    BEFORE_IN_FLIGHT_CLAIM_RELEASE.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook(sequence);
        }
    });
}

pub fn github_source_packet_collector() -> SourcePacketCollector {
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
                (None, None) => anyhow::bail!("collector requires resolved metadata when no endpoints were observed"),
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
        live_head: Arc::new(|url| {
            Box::pin(async move {
                let (owner, repo, number) = boss_github::pr_url::parse_pr_url_parts(&url)
                    .with_context(|| format!("source capture requires a canonical PR URL: {url}"))?;
                let slug = format!("{owner}/{repo}");
                let timer = boss_gh_telemetry::GhCallTimer::start_args(&[
                    "pr",
                    "view",
                    &number.to_string(),
                    "-R",
                    &slug,
                    "--json",
                    "headRefOid",
                ]);
                let result = git_utils::gh_cli::fetch_pr_head_oid(&slug, number).await;
                let outcome = match &result {
                    Ok(_) => boss_gh_telemetry::GhOutcome::Ok,
                    Err(error) if boss_gh_telemetry::classify::is_rate_limited(&error.to_string()) => {
                        boss_gh_telemetry::GhOutcome::RateLimited
                    }
                    Err(_) => boss_gh_telemetry::GhOutcome::Error,
                };
                timer.finish(outcome, None);
                result
            })
        }),
    }
}

/// Capture for an explicit `GenerateReviewGuide` request, independently of automatic
/// rollout gates. Await persistence so the caller can admit generation even
/// when another collector already stored the packet, and report failures.
pub async fn capture_review_guide_source_manually(
    work_db: &WorkDb,
    root_task_id: &str,
    pr_url: &str,
    collector: &SourcePacketCollector,
) -> Result<()> {
    let sequence = work_db.allocate_pr_review_guide_source_observation_sequence()?;
    let result = boss_gh_telemetry::scope(boss_gh_telemetry::callers::REVIEW_GUIDE_SOURCE_CAPTURE, async {
        let metadata = (collector.metadata)(pr_url.to_owned(), None).await?;
        let packet = (collector.collect)(pr_url.to_owned(), None, None, Some(metadata)).await?;
        work_db.persist_pr_review_guide_source_capture(
            root_task_id,
            sequence,
            PrSourceCaptureTrigger::Manual,
            &packet,
        )?;
        Ok(())
    })
    .await;
    if let Err(error) = &result {
        record_capture_failure(work_db, root_task_id, pr_url, sequence, error);
    }
    result
}

/// One durable source-capture request formed by a verified lifecycle seam.
/// Optional probe data makes creation/completion fetch fresh metadata while a
/// successful poller probe supplies its exact immutable endpoints.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct SourceCaptureRequest {
    pub root_task_id: String,
    pub pr_url: String,
    pub trigger: PrSourceCaptureTrigger,
    pub observed: Option<PinnedComparison>,
    pub expected_head_branch: Option<String>,
    pub observation_sequence: Option<i64>,
    pub revision: Option<RevisionCapture>,
}

pub struct RevisionCapture {
    task_id: String,
    head_before: Option<String>,
    head_after: Option<String>,
}

pub fn reconcile_review_guide_source_for_execution_with_collector(
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
        crate::expected_branch_name(
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
            .maybe_revision(
                (execution.kind == boss_protocol::ExecutionKind::RevisionImplementation
                    && trigger == PrSourceCaptureTrigger::Completion)
                    .then_some(RevisionCapture {
                        task_id: execution.work_item_id,
                        head_before: execution.pr_head_before,
                        head_after: execution.pr_head_after,
                    }),
            )
            .maybe_observed(observed)
            .maybe_expected_head_branch(expected_head_branch)
            .build(),
        collector,
    )
}

pub fn reconcile_review_guide_source_with_collector(
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
        revision,
    } = request;
    match work_db.review_guide_root_is_doc_only(&root_task_id) {
        Ok(true) => {
            tracing::info!(
                root_task_id,
                pr_url,
                "review guide not auto-generated: doc-only (design/investigation) task",
            );
            return None;
        }
        Ok(false) => {}
        Err(error) => tracing::warn!(
            root_task_id,
            pr_url,
            ?error,
            "review-guide source capture: could not read root task kind; proceeding",
        ),
    }
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
    let (guard, validate_existing) = if let Some(endpoints) = &observed {
        let existing_row = match work_db.select_probe_pr_review_guide_source_row(
            &root_task_id,
            &pr_url,
            &endpoints.base_sha,
            &endpoints.head_sha,
            observation_sequence,
        ) {
            Ok(existing_row) => existing_row,
            Err(error) => {
                record_capture_failure(&work_db, &root_task_id, &pr_url, observation_sequence, error);
                return None;
            }
        };
        (
            Some(acquire_in_flight_claim(
                &root_task_id,
                &pr_url,
                endpoints,
                observation_sequence,
            )?),
            existing_row,
        )
    } else {
        (None, false)
    };
    Some(boss_task_scope::spawn(async move {
        // Own the guard before the future is polled, including cancellation.
        let guard = guard;
        boss_gh_telemetry::scope(boss_gh_telemetry::callers::REVIEW_GUIDE_SOURCE_CAPTURE, async move {
            if validate_existing && let Some(endpoints) = observed.clone() {
                let probe_db = Arc::clone(&work_db);
                let probe_root = root_task_id.clone();
                let probe_url = pr_url.clone();
                let probe_sequence = observation_sequence;
                match tokio::task::spawn_blocking(move || {
                    probe_db.select_probe_pr_review_guide_source_capture(
                        &probe_root,
                        &probe_url,
                        &endpoints.base_sha,
                        &endpoints.head_sha,
                        probe_sequence,
                    )
                })
                .await
                {
                    Ok(Ok(true)) => return,
                    Ok(Ok(false)) => {}
                    Ok(Err(error)) => {
                        record_capture_failure(&work_db, &root_task_id, &pr_url, observation_sequence, error);
                        return;
                    }
                    Err(error) => {
                        record_capture_failure(&work_db, &root_task_id, &pr_url, observation_sequence, error);
                        return;
                    }
                }
            }
            // Completion may already have pinned the delivered head. Preserve
            // its attribution even when fetching fresh metadata fails.
            if let Some(revision) = &revision
                && let Some(head) = &revision.head_after
                && revision.head_before.as_ref() != Some(head)
                && let Err(error) = work_db
                    .record_review_guide_revision_head(
                        &root_task_id,
                        &pr_url,
                        head,
                        &revision.task_id,
                        observation_sequence,
                    )
                    .and_then(|()| match &revision.head_before {
                        Some(before) => work_db.record_review_guide_head_ancestry(&root_task_id, &pr_url, before, head),
                        None => Ok(()),
                    })
            {
                record_capture_failure(&work_db, &root_task_id, &pr_url, observation_sequence, error);
                return;
            }
            let confirmed_head = if trigger == PrSourceCaptureTrigger::Completion {
                match revision.as_ref().and_then(|revision| revision.head_after.as_ref()) {
                    Some(head) if !head.is_empty() => Some(head.clone()),
                    _ => match (collector.live_head)(pr_url.clone()).await {
                        Ok(head) if !head.is_empty() => Some(head),
                        _ => {
                            defer_unconfirmed_completion(
                                &root_task_id,
                                &pr_url,
                                observation_sequence,
                                "live PR head could not be read",
                            );
                            return;
                        }
                    },
                }
            } else {
                None
            };
            let metadata = match (collector.metadata)(pr_url.clone(), expected_head_branch.clone()).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    // A revision that already recorded the delivered head still
                    // surfaces a durable failure. Any other completion without a
                    // confirmed REST identity defers to the poller.
                    if trigger == PrSourceCaptureTrigger::Completion
                        && revision
                            .as_ref()
                            .and_then(|revision| revision.head_after.as_ref())
                            .is_none()
                    {
                        defer_unconfirmed_completion(
                            &root_task_id,
                            &pr_url,
                            observation_sequence,
                            format!("comparison metadata unavailable: {error}"),
                        );
                        return;
                    }
                    record_capture_failure(&work_db, &root_task_id, &pr_url, observation_sequence, error);
                    return;
                }
            };
            let mut observed = observed;
            if let Some(confirmed) = &confirmed_head {
                if metadata.head_sha != *confirmed {
                    defer_unconfirmed_completion(
                        &root_task_id,
                        &pr_url,
                        observation_sequence,
                        format!(
                            "REST head {} does not match confirmed live head {confirmed}",
                            metadata.head_sha
                        ),
                    );
                    return;
                }
                observed = Some(PinnedComparison {
                    base_sha: metadata.base_sha.clone(),
                    head_sha: metadata.head_sha.clone(),
                });
            }
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
            if let Some(revision) = &revision
                && revision.head_before.as_deref() != Some(metadata.head_sha.as_str())
                && revision
                    .head_after
                    .as_deref()
                    .is_none_or(|head| head == metadata.head_sha)
                && let Err(error) = work_db
                    .record_review_guide_revision_head(
                        &root_task_id,
                        &pr_url,
                        &metadata.head_sha,
                        &revision.task_id,
                        observation_sequence,
                    )
                    .and_then(|()| match &revision.head_before {
                        Some(before) => work_db.record_review_guide_head_ancestry(
                            &root_task_id,
                            &pr_url,
                            before,
                            &metadata.head_sha,
                        ),
                        None => Ok(()),
                    })
            {
                record_capture_failure(&work_db, &root_task_id, &pr_url, observation_sequence, error);
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
                    match prepare_capture(
                        &work_db,
                        &root_task_id,
                        &pr_url,
                        &rest_identity,
                        observation_sequence,
                        false,
                    ) {
                        Ok(Some(guard)) => guard,
                        Ok(None) => {
                            // A poll can capture this head before completion. Recover a
                            // missing enqueue without retrying a failed or completed job.
                            match work_db.get_latest_pr_review_guide_source_capture(&root_task_id) {
                                Ok(Some(capture))
                                    if capture.packet.canonical_pr_url == pr_url
                                        && capture.packet.head_sha == rest_identity.head_sha =>
                                {
                                    enqueue_review_guide_generation(&work_db, &feature_flags, &capture);
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    record_capture_failure(
                                        &work_db,
                                        &root_task_id,
                                        &pr_url,
                                        observation_sequence,
                                        error,
                                    );
                                    return;
                                }
                            }
                            if let Some(probe) = &observed
                                && let Err(error) = work_db.remember_pr_review_guide_probe(
                                    &pr_url,
                                    &rest_identity,
                                    &probe.base_sha,
                                    observation_sequence,
                                )
                            {
                                record_capture_failure(&work_db, &root_task_id, &pr_url, observation_sequence, error);
                            }
                            return;
                        }
                        Err(error) => {
                            record_capture_failure(&work_db, &root_task_id, &pr_url, observation_sequence, error);
                            return;
                        }
                    }
                }
            };
            let metadata = Some(metadata);
            let result = (collector.collect)(pr_url.clone(), observed, expected_head_branch, metadata).await;
            for pass in 0..2 {
                let observation_sequence = *guard.sequence.lock().unwrap_or_else(|error| error.into_inner());
                apply_capture_result(
                    CapturePersistContext {
                        work_db: &work_db,
                        feature_flags: &feature_flags,
                        root_task_id: &root_task_id,
                        pr_url: &pr_url,
                    },
                    trigger,
                    &rest_identity,
                    &result,
                    observation_sequence,
                );
                #[cfg(test)]
                run_after_capture_persist(&guard.key);
                if let Some(snapshot) = release_in_flight_claim(&mut guard, observation_sequence, pass == 1) {
                    if snapshot != observation_sequence {
                        apply_capture_result(
                            CapturePersistContext {
                                work_db: &work_db,
                                feature_flags: &feature_flags,
                                root_task_id: &root_task_id,
                                pr_url: &pr_url,
                            },
                            trigger,
                            &rest_identity,
                            &result,
                            snapshot,
                        );
                    }
                    break;
                }
            }
            drop(guard);
        })
        .await;
    }))
}

/// Bridges a freshly stored or upgraded comparison to a durable generation
/// attempt plus its execution row. Gated independently by
/// [`REVIEW_GUIDE_GENERATION_FLAG`]. Best-effort: any failure here is logged
/// and dropped rather than propagated, matching every other outcome in this
/// reconciler. An explicit retry can recover a missed enqueue;
/// reconcile recovers only attempts that were durably created.
///
/// Admission is series-scoped: at most one non-terminal attempt is allowed
/// per series. A duplicate observation of the same comparison is a no-op;
/// a newer comparison cancels the obsolete attempt (and its execution)
/// before the replacement is created.
fn enqueue_review_guide_generation(
    work_db: &WorkDb,
    feature_flags: &FeatureFlagsStore,
    capture: &PrReviewGuideSourceCapture,
) {
    if !feature_flags.is_enabled(REVIEW_GUIDE_GENERATION_FLAG) {
        return;
    }
    // A treated family gets its guide when the PR is ready for a person or
    // when someone opens it, not on every new comparison.
    if review_guide_when_ready_arm(work_db, feature_flags, &capture.root_task_id) == Some(ExperimentArm::Treatment) {
        tracing::debug!(
            root_task_id = %capture.root_task_id,
            comparison_id = %capture.comparison_id,
            "review-guide generation: not enqueued on capture; the family generates when its PR is ready or opened",
        );
        return;
    }
    admit_review_guide_generation(work_db, capture);
}

/// The arm `root_task_id`'s PR family draws in `review_guide_when_ready` right
/// now, or `None` when the experiment is not running or the family cannot be
/// resolved (which leaves today's behaviour in force).
///
/// The same family id and key the launch record uses
/// (`experiment_family_root`), so this decision and the arm recorded on the
/// guide's execution agree for as long as the flags stay put.
pub fn review_guide_when_ready_arm(
    work_db: &WorkDb,
    feature_flags: &FeatureFlagsStore,
    root_task_id: &str,
) -> Option<ExperimentArm> {
    let state = feature_flags.experiment_state(REVIEW_GUIDE_WHEN_READY_EXPERIMENT);
    if !state.is_running() {
        return None;
    }
    let family = match work_db
        .connect()
        .and_then(|conn| crate::work::chain_root(&conn, root_task_id))
    {
        Ok(family) => family,
        Err(error) => {
            tracing::warn!(
                root_task_id,
                ?error,
                "review_guide_when_ready: could not resolve the PR family; treating it as untouched",
            );
            return None;
        }
    };
    assign_arm(state, experiment_bucket(&family, REVIEW_GUIDE_WHEN_READY_EXPERIMENT))
}

/// Generate the guide for a treated family whose PR the merge poller has just
/// observed ready for a person: required checks green on `head_sha`, and no
/// live review batch, review or revision in the family. Returns whether an
/// attempt was admitted.
///
/// Cheap while the experiment is off (flags only) and in steady state (one
/// indexed read once the head has been tried), because the poller asks on every
/// pass. A comparison that already has any attempt is left alone, so a failed
/// guide is not relaunched each pass.
pub fn enqueue_review_guide_when_ready(
    work_db: &WorkDb,
    feature_flags: &FeatureFlagsStore,
    root_task_id: &str,
    pr_url: &str,
    head_sha: &str,
) -> bool {
    if !feature_flags.is_enabled(REVIEW_GUIDE_GENERATION_FLAG)
        || !feature_flags
            .experiment_state(REVIEW_GUIDE_WHEN_READY_EXPERIMENT)
            .is_running()
    {
        return false;
    }
    let (series_id, comparison_id) =
        match work_db.review_guide_untried_comparison_at_head(root_task_id, pr_url, head_sha) {
            Ok(Some(untried)) => untried,
            Ok(None) => return false,
            Err(error) => {
                tracing::warn!(
                    root_task_id,
                    pr_url,
                    ?error,
                    "review-guide ready trigger: could not read the series"
                );
                return false;
            }
        };
    if review_guide_when_ready_arm(work_db, feature_flags, root_task_id) != Some(ExperimentArm::Treatment) {
        return false;
    }
    match work_db.review_guide_family_is_quiet(root_task_id) {
        Ok(true) => {}
        Ok(false) => return false,
        Err(error) => {
            tracing::warn!(
                root_task_id,
                pr_url,
                ?error,
                "review-guide ready trigger: could not read the family"
            );
            return false;
        }
    }
    tracing::info!(
        root_task_id,
        pr_url,
        comparison_id,
        "review-guide ready trigger: the PR is ready for a person; generating its guide",
    );
    admit_review_guide_generation_for(work_db, root_task_id, &series_id, &comparison_id)
}

fn admit_review_guide_generation(work_db: &WorkDb, capture: &PrReviewGuideSourceCapture) {
    admit_review_guide_generation_for(
        work_db,
        &capture.root_task_id,
        &capture.series_id,
        &capture.comparison_id,
    );
}

/// Admit one durable attempt for the comparison and dispatch its execution.
/// Returns whether an attempt was created.
fn admit_review_guide_generation_for(
    work_db: &WorkDb,
    root_task_id: &str,
    series_id: &str,
    comparison_id: &str,
) -> bool {
    match work_db.review_guide_comparison_has_attempt(comparison_id) {
        Ok(true) => return false,
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(?error, "review-guide generation: could not check existing attempt");
            return false;
        }
    }
    match work_db.resolve_repo_for_task(root_task_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            tracing::warn!(
                root_task_id,
                "review-guide generation: root task has no repository; skipping enqueue"
            );
            return false;
        }
        Err(error) => {
            tracing::warn!(
                root_task_id,
                ?error,
                "review-guide generation: could not resolve repository; skipping enqueue"
            );
            return false;
        }
    }
    let attempt = match work_db.create_automatic_pr_review_guide_attempt(series_id, comparison_id) {
        Ok(attempt) => attempt,
        Err(error) => {
            tracing::warn!(
                comparison_id,
                ?error,
                "review-guide generation: could not create a durable attempt",
            );
            return false;
        }
    };
    match work_db.dispatch_pr_review_guide_attempt(&attempt.id, root_task_id) {
        Ok(bound) => tracing::info!(
            comparison_id,
            attempt_id = %bound.id,
            execution_id = ?bound.execution_id,
            "review-guide generation: enqueued a durable generation attempt",
        ),
        Err(error) => tracing::warn!(
            comparison_id,
            attempt_id = %attempt.id,
            ?error,
            "review-guide generation: created a durable attempt but could not dispatch its execution; reconcile will retry",
        ),
    }
    true
}

struct CapturePersistContext<'a> {
    work_db: &'a WorkDb,
    feature_flags: &'a FeatureFlagsStore,
    root_task_id: &'a str,
    pr_url: &'a str,
}

fn apply_capture_result(
    ctx: CapturePersistContext<'_>,
    trigger: PrSourceCaptureTrigger,
    rest_identity: &PinnedComparison,
    result: &Result<SourcePacket>,
    observation_sequence: i64,
) {
    let CapturePersistContext {
        work_db,
        feature_flags,
        root_task_id,
        pr_url,
    } = ctx;
    match result {
        Ok(packet) => {
            match work_db.persist_pr_review_guide_source_capture(root_task_id, observation_sequence, trigger, packet) {
                Ok(PrSourceCapturePersistOutcome::Stored(capture)) => {
                    tracing::info!(
                        root_task_id,
                        pr_url,
                        observation_sequence,
                        packet_hash = %capture.packet_hash,
                        complete = capture.complete,
                        "review-guide source capture: stored immutable comparison packet",
                    );
                    enqueue_review_guide_generation(work_db, feature_flags, &capture);
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
                Err(error) => record_capture_failure(work_db, root_task_id, pr_url, observation_sequence, error),
            }
        }
        Err(error) => {
            if let Err(count_error) = work_db.record_pr_review_guide_source_retry_error(pr_url, rest_identity) {
                tracing::warn!(?count_error, "could not count failed source retry");
            }
            record_capture_failure(work_db, root_task_id, pr_url, observation_sequence, error);
        }
    }
}

/// Drop the in-flight claim under the map mutex and return the coalesced
/// sequence. Map lock then sequence lock, matching [`InFlightGuard::acquire_locked`].
///
/// When `force` is false, a newer coalesced sequence leaves the claim in
/// place so another persist pass can run. An arrival injected after the
/// equal-sequence read is still snapshotted and returned so it can be
/// persisted outside the lock.
fn release_in_flight_claim(guard: &mut InFlightGuard, persisted: i64, force: bool) -> Option<i64> {
    if !guard.active {
        return None;
    }
    let mut captures = in_flight_captures().lock().unwrap_or_else(|error| error.into_inner());
    let peeked = *guard.sequence.lock().unwrap_or_else(|error| error.into_inner());
    if peeked != persisted && !force {
        return None;
    }
    #[cfg(test)]
    run_before_in_flight_claim_release(&guard.sequence);
    let snapshot = *guard.sequence.lock().unwrap_or_else(|error| error.into_inner());
    captures.remove(&guard.key);
    guard.active = false;
    Some(snapshot)
}

fn acquire_in_flight_claim(
    root: &str,
    url: &str,
    endpoints: &PinnedComparison,
    sequence: i64,
) -> Option<InFlightGuard> {
    let mut captures = in_flight_captures().lock().unwrap_or_else(|error| error.into_inner());
    InFlightGuard::acquire_locked(
        &mut captures,
        (
            format!("{root}:{url}"),
            endpoints.base_sha.clone(),
            endpoints.head_sha.clone(),
        ),
        sequence,
    )
}

pub fn prepare_capture(
    db: &WorkDb,
    root: &str,
    url: &str,
    endpoints: &PinnedComparison,
    sequence: i64,
    probe: bool,
) -> Result<Option<InFlightGuard>> {
    let select = if probe {
        WorkDb::select_probe_pr_review_guide_source_capture
    } else {
        WorkDb::select_complete_pr_review_guide_source_capture
    };
    if select(db, root, url, &endpoints.base_sha, &endpoints.head_sha, sequence)? {
        return Ok(None);
    }
    Ok(acquire_in_flight_claim(root, url, endpoints, sequence))
}

pub struct InFlightGuard {
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

fn defer_unconfirmed_completion(
    root_task_id: &str,
    pr_url: &str,
    observation_sequence: i64,
    reason: impl std::fmt::Display,
) {
    tracing::info!(
        root_task_id,
        pr_url,
        observation_sequence,
        reason = %reason,
        "review-guide source capture: completion head is unconfirmed; deferring to the poller",
    );
}

/// Stops a cancelled `pr_review_guide` worker as soon as admission
/// terminalizes its execution. Admission already records the attempt as
/// `superseded` and the execution as `cancelled`; this subscriber is the
/// pane/lease teardown that `cancel_execution_in_tx` does not perform.
/// `terminal_work_sweep` remains the periodic backstop.
pub fn spawn_cancelled_review_guide_worker_stopper(
    work_db: Arc<WorkDb>,
    event_bus: Arc<EventBus>,
    stop: CancelledGuideStopFn,
) -> tokio::task::JoinHandle<()> {
    boss_event_bus::spawn_supervised("cancelled_review_guide_stop", move || {
        let work_db = Arc::clone(&work_db);
        let event_bus = Arc::clone(&event_bus);
        let stop = Arc::clone(&stop);
        async move {
            let mut subscription = event_bus.subscribe(TopicFilter::kind(EventKind::ExecutionTerminal));
            while let Some(event) = subscription.recv().await {
                let Event::ExecutionTerminal { execution_id, .. } = event else {
                    continue;
                };
                let Ok(execution) = work_db.get_execution(&execution_id) else {
                    continue;
                };
                if execution.kind != ExecutionKind::PrReviewGuide || execution.status != ExecutionStatus::Cancelled {
                    continue;
                }
                stop(execution_id).await;
            }
        }
    })
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
#[path = "review_guide_capture_tests.rs"]
mod tests;
