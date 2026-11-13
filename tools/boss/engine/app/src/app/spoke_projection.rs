//! What a spoke's journaled observations do to the engine's work state.
//!
//! The hub's projector (`boss_spoke_hub::projection`) reads each host's stored
//! records in order and hands them here. Each kind goes to the consumer that
//! already owns it, so a remote worker's hook event, queued `done` and pane exit
//! are handled by the same code that handles a local worker's:
//!
//! * a **hook event** takes the fan-out a hook arriving on the local events
//!   socket takes (live state, completion on `Stop`, probes, and the rest).
//!   Turn evidence reaches the spoke turn map through `record_spoke_turn_event`,
//!   which this applier calls for each projected main-session hook, durably
//!   and before that hook's worker-event fan-out;
//! * a **queued proposal** goes through the same validators, apply pipeline and
//!   finalization as `SubmitProposal`, and yields the decision the spoke's
//!   worker is waiting on;
//! * a run's **transcript** is held on the hub (`boss_spoke_custody`), and the
//!   run is pointed at that copy, so the live summary, cost capture and
//!   completion read a file in the hub's custody directory addressed by run
//!   identity, never a path on the host;
//! * a **pane observation** is reflected in live state, and nothing more: what
//!   absence or an exit means for work status stays with the existing liveness
//!   policy.
//!
//! Nothing here is reachable until a hub listener is configured, and a spoke
//! host is enrolled and proven ready.

use async_trait::async_trait;
use boss_engine_driver::{AgentDriver, DriverRegistry};
use boss_engine_live_worker_state::DriverSignalKind;
use boss_engine_proposal_validation::{derive_idempotency_key, validate_caller_idempotency_key, validate_payload};
use boss_engine_runtime::hub::{HubStream, projection};
use boss_engine_runtime::live_status_loop::{LiveStatusBroadcaster, LiveStatusRun, TranscriptPathResolver};
use boss_engine_runtime::worker_refresh_liveness::WorkerRefreshPermit;
use boss_protocol::{
    ArchitecturalMismatchEvidence, FrontendRequest, ProposalErrorCode, ProposalKind, ProposalSubmissionError,
    QueueCiEvidence, WorkerProposal,
};
use boss_protocol_spoke::proposal::{
    ANSWER_REPLY, CI_MARK_FAILED, CONFLICT_MARK_FAILED, CONFLICT_PRODUCER, GUIDE_OUTCOME,
};
use boss_protocol_spoke::v1::ResultCode;
use projection::{
    Applied, Decision, DecisionSettled, EventObservation, Life, ObservationApplier, Origin, PaneObservation,
    ProjectorConfig, Proposed, Settlement, TranscriptSource,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::oneshot;

use super::{
    ServerState,
    proposals::{self, AttributedCaller, Submission},
    run_writes::{self, ConflictMarked, WriteOrigin},
    worker_events::dispatch_worker_event_fanout,
};

struct ServerApplier {
    /// Weak, so the projector task never keeps a shut-down engine alive.
    state: Weak<ServerState>,
    drivers: DriverRegistry,
    /// Accepted submissions whose finish waits until the decision settles. Lost
    /// on a restart; `decision_settled` then rebuilds the obligation from the
    /// stored proposal.
    pending_finish: Mutex<HashMap<String, proposals::Accepted>>,
    /// Decisions whose finish already ran while delivery was still being
    /// retried, so the terminal settlement does not run it a second time.
    finished_early: Mutex<HashSet<String>>,
}

/// The applier that routes projected observations into `state`.
pub(super) fn applier(state: &Arc<ServerState>) -> Arc<dyn ObservationApplier> {
    Arc::new(ServerApplier {
        state: Arc::downgrade(state),
        drivers: DriverRegistry::default(),
        pending_finish: Mutex::new(HashMap::new()),
        finished_early: Mutex::new(HashSet::new()),
    })
}

/// Start applying what the hub's spokes report, on the engine task scope.
pub(super) fn start(state: &Arc<ServerState>, stream: &HubStream) {
    if let Err(reason) = state.restore_spoke_turns() {
        tracing::warn!(%reason, "cannot restore spoke turn state at projector start; every sweep retries it");
    }
    let db = state.work_db.as_ref().clone();
    // Transcript and artifact bytes live beside the engine's other per-execution
    // artifacts. Without a state root there is nowhere to hold them, and the
    // records are received and left alone rather than lost.
    let projector = match boss_engine_driver::transcript_store::transcript_store_root() {
        Ok(root) => stream.projector_with_custody(
            db.clone(),
            applier(state),
            ProjectorConfig::default(),
            Arc::new(boss_engine_runtime::hub::projection::Custody::new(db, root)),
        ),
        Err(error) => {
            tracing::warn!(%error, "spoke transcript custody is off: no Boss state root to hold the bytes");
            stream.projector(db, applier(state), ProjectorConfig::default())
        }
    };
    boss_task_scope::spawn(projector.run());
}

/// Start the live summary for a spoke run's slot. The summarizer reads the run's
/// transcript, which for a spoke run is the hub's custody copy.
pub(super) fn start_summary_slot(state: &Arc<ServerState>, slot_id: u8, run_id: &str, driver: Arc<dyn AgentDriver>) {
    let broadcaster: Arc<dyn LiveStatusBroadcaster> = state.clone();
    state.live_status_manager.start_slot(
        slot_id,
        LiveStatusRun::new(run_id, driver),
        state.utility_model.clone(),
        state.live_worker_states.clone(),
        broadcaster,
        Arc::new(SpokeSlotResolver(Arc::clone(state))),
    );
}

/// Finds a spoke run's transcript like any other run's, but never asks whether
/// its worker is alive the way a local run's summarizer does. That check looks
/// for a local tmux session and a local shell pid; a spoke's worker has neither
/// on this machine, and its liveness is what the spoke reports. Treating the
/// absence of a local process as death would mark a healthy remote worker
/// terminated.
struct SpokeSlotResolver(Arc<ServerState>);

#[async_trait]
impl TranscriptPathResolver for SpokeSlotResolver {
    async fn transcript_path(&self, run_id: &str) -> Option<std::path::PathBuf> {
        TranscriptPathResolver::transcript_path(self.0.as_ref(), run_id).await
    }

    async fn worker_can_refresh(&self, run_id: &str) -> WorkerRefreshPermit {
        match self.0.work_db.get_execution(run_id) {
            Ok(execution) if execution.status.is_terminal() => WorkerRefreshPermit::Stop,
            _ => WorkerRefreshPermit::Refresh,
        }
    }

    async fn transcript_containment_root(&self, run_id: &str) -> Option<std::path::PathBuf> {
        self.transcript_path(run_id)
            .await
            .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
    }
}

impl ServerApplier {
    fn state(&self) -> Result<Arc<ServerState>, String> {
        // Holding the cursor is right when the engine is going away: the record
        // stays unapplied for the next start instead of being skipped.
        self.state
            .upgrade()
            .ok_or_else(|| "the engine is shutting down".to_owned())
    }
}

#[async_trait]
impl ObservationApplier for ServerApplier {
    async fn event(&self, observation: &EventObservation) -> Applied {
        let state = match self.state() {
            Ok(state) => state,
            Err(reason) => return Applied::Retry(reason),
        };
        let execution_id = observation.origin.run.execution_id.as_str();
        let slug = match state.work_db.get_execution_driver_slug(execution_id) {
            Ok(Some(slug)) => slug,
            Ok(None) => {
                tracing::warn!(
                    execution_id,
                    "dropping a spoke hook event: the execution no longer exists"
                );
                return Applied::Done;
            }
            Err(error) => return Applied::Retry(format!("{error:#}")),
        };
        let driver = match self.drivers.require(&slug) {
            Ok(driver) => driver,
            Err(error) => {
                tracing::error!(
                    execution_id,
                    driver = %slug,
                    %error,
                    "dropping a spoke hook event: its driver is not registered in this build"
                );
                return Applied::Done;
            }
        };
        // The host's transcript path is a display path on that host. The hub
        // never opens it: a turn-ending event carries the hub's own copy, held
        // by run identity, once every byte the spoke named is on the hub.
        // Without one the Stop-time consumers see unavailable evidence, which
        // custody has recorded with its reason.
        if observation.transcript.is_none() && matches!(observation.event, boss_protocol::WorkerEvent::Stop { .. }) {
            tracing::info!(
                execution_id,
                from_subagent = observation.from_subagent,
                "projected spoke Stop has no hub transcript copy; Stop-time consumers that read a transcript see unavailable evidence"
            );
        }
        let ingress_seq = state
            .accepted_hook_seq
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let mut incoming = boss_engine_runtime::events_socket::IncomingHookEvent::resolve(
            driver.as_ref(),
            observation.event.clone(),
            Some(execution_id.to_owned()),
            None,
            None,
        )
        .with_subagent_origin(observation.from_subagent)
        .with_ingress_seq(ingress_seq)
        .with_observed_at_ms(observation.origin.observed.wall_unix_ms);
        if let Some(source) = &observation.transcript {
            incoming = incoming.with_transcript_source(source.path.to_string_lossy().into_owned(), source.root.clone());
        } else if matches!(observation.event, boss_protocol::WorkerEvent::Stop { .. }) {
            incoming = incoming.without_cached_transcript();
        }
        // Persist the turn state before the fan-out: a failure here retries the
        // record, and a retry must not re-run the fan-out's turn-boundary
        // handling (completion, probes) for a hook it already dispatched.
        if let Err(reason) = state.restore_spoke_turns() {
            return Applied::Retry(reason);
        }
        if let Err(reason) = state.record_spoke_turn_event(execution_id, &observation.event, observation.from_subagent)
        {
            return Applied::Retry(reason);
        }
        dispatch_worker_event_fanout(&state, &incoming).await;
        Applied::Done
    }

    async fn transcript_attached(&self, origin: &Origin, source: &TranscriptSource) -> Applied {
        let state = match self.state() {
            Ok(state) => state,
            Err(reason) => return Applied::Retry(reason),
        };
        let execution_id = origin.run.execution_id.as_str();
        let path = source.path.to_string_lossy().into_owned();
        // A newer generation replaces the pointer; replaying an older one does
        // not roll it back. Local hooks stay first-writer-wins.
        if !state
            .transcript_path_cache
            .record_if_newer(execution_id, &path, &source.generation)
        {
            // The cache already holds this or a newer generation. Still fill a
            // work_runs row that did not exist when the first chunk attached.
            if let Err(error) = state.work_db.set_run_transcript_path_if_unset(execution_id, &path) {
                return Applied::Retry(format!("{error:#}"));
            }
            return Applied::Done;
        }
        match state.work_db.set_run_transcript_path(execution_id, &path) {
            Ok(_) => {}
            Err(error) => return Applied::Retry(format!("{error:#}")),
        }
        state
            .record_driver_signal(execution_id, DriverSignalKind::TranscriptPath)
            .await;
        Applied::Done
    }

    async fn proposal(&self, origin: &Origin, kind: ProposalKind, payload: &Value, key: &str) -> Proposed {
        let state = match self.state() {
            Ok(state) => state,
            Err(reason) => return Proposed::Retry(reason),
        };
        let execution_id = origin.run.execution_id.as_str();
        let work_item_id = match state.work_db.work_item_for_execution(execution_id) {
            Ok(Some(id)) => id,
            Ok(None) => return Proposed::Decided(Decision::refused(format!("execution `{execution_id}` is unknown"))),
            Err(error) => return Proposed::Retry(format!("{error:#}")),
        };
        // Re-validate with the hub's own validators. The spoke validated too,
        // but a spoke's word is not what the hub applies.
        let validated = match validate_payload(kind, payload) {
            Ok(validated) => validated,
            Err(fields) => return Proposed::Decided(rejected(&ProposalSubmissionError::validation(fields))),
        };
        if !key_is_acceptable(execution_id, kind, &validated.canonical_json, key) {
            return Proposed::Decided(Decision::refused("the idempotency key is not one a worker may use"));
        }
        let caller = AttributedCaller {
            execution_id: execution_id.to_owned(),
            work_item_id,
        };
        let submitted =
            proposals::submit_for_caller(&state, &state.work_db, &caller, kind, &validated.canonical_json, key).await;
        match submitted {
            Ok(Submission::Replayed(proposal)) => Proposed::Decided(accepted(&proposal, true)),
            Ok(Submission::Accepted(accepted_submission)) => {
                let decision = accepted(&accepted_submission.proposal, accepted_submission.already_submitted);
                // Finish after the projector commits the decision effect. The
                // oneshot stays unresolved until `decision_settled`, so a
                // reporting member is not torn down before the spoke is answered.
                let finish_key = format!("{}:{key}", origin.run.run_id);
                self.pending_finish
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(finish_key, accepted_submission);
                Proposed::Decided(decision)
            }
            Err(error) if error.code == ProposalErrorCode::Internal => Proposed::Retry(error.message),
            Err(error) => Proposed::Decided(rejected(&error)),
        }
    }

    async fn decision_settled(&self, decision: &DecisionSettled<'_>) {
        let Ok(state) = self.state() else {
            return;
        };
        let held = {
            let mut pending = self
                .pending_finish
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match decision.settlement {
                // A reporting member is torn down only once its ack is known, so
                // it waits for the terminal settlement. Guide and run-done
                // finalization are idempotent and need not wait for a spoke that
                // may stay unreachable.
                Settlement::Pending => match pending.get(decision.effect_key) {
                    Some(accepted) if !accepted.needs_delivered_ack() => pending.remove(decision.effect_key),
                    _ => None,
                },
                Settlement::Delivered | Settlement::GaveUp => pending.remove(decision.effect_key),
            }
        };
        let mut finished_early = self
            .finished_early
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let accepted = match (held, decision.settlement) {
            (Some(accepted), Settlement::Pending) => {
                finished_early.insert(decision.effect_key.to_owned());
                accepted
            }
            (Some(accepted), _) => {
                finished_early.remove(decision.effect_key);
                accepted
            }
            (None, Settlement::Pending) => return,
            (None, _) => {
                if finished_early.remove(decision.effect_key) {
                    return;
                }
                if decision.code != ResultCode::Ok as i32 {
                    // A refused or superseded decision must not rebuild and
                    // finish another generation's accepted proposal: content-
                    // derived keys collide by execution and canonical JSON.
                    return;
                }
                drop(finished_early);
                // This process no longer holds the submission (the hub
                // restarted after the decision was recorded): rebuild what it
                // still owes from the stored proposal.
                match state
                    .work_db
                    .find_worker_proposal_by_idempotency_key(decision.execution_id, decision.proposal_id)
                {
                    Ok(Some(proposal)) => match proposals::Accepted::rederive(&state, &state.work_db, proposal) {
                        Some(accepted) => accepted,
                        None => return,
                    },
                    Ok(None) => return,
                    Err(error) => {
                        tracing::warn!(
                            execution_id = decision.execution_id,
                            %error,
                            "cannot reload a settled proposal to finalize it; the finalization is skipped"
                        );
                        return;
                    }
                }
            }
        };
        let (sent, delivered_rx) = oneshot::channel();
        let _ = sent.send(decision.settlement == Settlement::Delivered);
        // Finalization can reach GitHub; it must not hold up the delivery pass
        // that serves every host.
        boss_task_scope::spawn(async move { accepted.finish(delivered_rx).await });
    }

    async fn sweep(&self) {
        let Ok(state) = self.state() else {
            return;
        };
        // Cheap once restored. A restore that failed earlier is retried here, so
        // a parked worker with no further hooks still becomes deliverable.
        match tokio::task::spawn_blocking(move || state.restore_spoke_turns()).await {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => tracing::warn!(%reason, "cannot restore spoke turn state; retrying on the next sweep"),
            Err(error) => tracing::warn!(%error, "spoke turn restore task failed"),
        }
    }

    async fn run_write(&self, origin: &Origin, contract: &str, payload: &Value, key: &str) -> Proposed {
        let state = match self.state() {
            Ok(state) => state,
            Err(reason) => return Proposed::Retry(reason),
        };
        let execution_id = origin.run.execution_id.as_str();
        let work_item_id = match state.work_db.work_item_for_execution(execution_id) {
            Ok(Some(id)) => id,
            Ok(None) => return Proposed::Decided(Decision::refused(format!("execution `{execution_id}` is unknown"))),
            Err(error) => return Proposed::Retry(format!("{error:#}")),
        };
        match state.work_db.get_spoke_applied_write(key) {
            Ok(Some(write)) if contract != GUIDE_OUTCOME => {
                return if write.accepted {
                    Proposed::Decided(accepted_write("already applied"))
                } else {
                    Proposed::Decided(Decision::refused(write.reason))
                };
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(error) => return Proposed::Retry(format!("{error:#}")),
        }
        let request = match request_for_run_write(execution_id, contract, payload) {
            Ok(request) => request,
            Err(reason) => return Proposed::Decided(Decision::refused(reason)),
        };
        if let Some(decision) = replay_or_ownership(&state, &work_item_id, &request) {
            return decision;
        }
        apply_run_write(&state, origin, contract, key, request).await
    }

    async fn pane(&self, observation: &PaneObservation) -> Applied {
        let state = match self.state() {
            Ok(state) => state,
            Err(reason) => return Applied::Retry(reason),
        };
        let execution_id = observation.origin.run.execution_id.as_str();
        let registry = &state.live_worker_states;
        // Only the host's own look at its pane moves the display. Exit is
        // positive evidence; an absent session is inferred death, shown but not
        // fenced so a later Alive observation or a hook can still revive the
        // slot; an indeterminate look says nothing.
        let changed = match observation.life() {
            Life::Exited { .. } | Life::BriefNotDelivered => registry.mark_terminated(execution_id),
            Life::Missing => registry.mark_terminated_display(execution_id),
            Life::Alive => registry.revive_from_liveness_inference(execution_id),
            Life::Indeterminate => false,
        };
        if changed {
            state.broadcast_live_worker_states().await;
        }
        Applied::Done
    }
}

/// A key the spoke gateway may have produced: one the engine would derive, a
/// keyless wait's fresh identity, or a caller key the engine would accept. The
/// `auto:` namespace is reserved, so a worker cannot claim an engine-derived
/// key for a different submission.
fn key_is_acceptable(execution_id: &str, kind: ProposalKind, canonical_json: &str, key: &str) -> bool {
    key == derive_idempotency_key(execution_id, kind, canonical_json)
        || (kind == ProposalKind::Wait && key.starts_with(&format!("auto:wait:{execution_id}:")))
        || validate_caller_idempotency_key(key).is_ok()
}

/// The decision the worker's CLI shows for an accepted proposal. Bounded: the
/// stored row, which can carry a large payload, is not echoed back.
fn accepted(proposal: &WorkerProposal, already_submitted: bool) -> Decision {
    Decision {
        accepted: true,
        reason: String::new(),
        result: json!({
            "proposal_id": proposal.idempotency_key,
            "hub_proposal": proposal.id,
            "kind": proposal.kind,
            "state": proposal.state,
            "applied_ref": proposal.applied_ref,
            "already_submitted": already_submitted,
        }),
    }
}

fn rejected(error: &ProposalSubmissionError) -> Decision {
    Decision {
        accepted: false,
        reason: error.message.clone(),
        result: serde_json::to_value(error).unwrap_or_else(|_| json!({"message": error.message})),
    }
}

fn request_for_run_write(execution_id: &str, contract: &str, payload: &Value) -> Result<FrontendRequest, String> {
    bind_run_write(execution_id, contract, payload)?;
    match contract {
        GUIDE_OUTCOME => Ok(FrontendRequest::CommentsRecordGuideOutcome {
            run_id: string_field(payload, "run_id")?,
            comment_id: string_field(payload, "comment_id")?,
            disposition: serde_json::from_value(
                payload
                    .get("disposition")
                    .cloned()
                    .ok_or_else(|| "guide outcome is missing disposition".to_owned())?,
            )
            .map_err(|error| format!("guide outcome disposition is unreadable: {error}"))?,
            body: string_field(payload, "body")?,
            request_regeneration: payload
                .get("request_regeneration")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        ANSWER_REPLY => Ok(FrontendRequest::CommentsPostAnswer {
            run_id: string_field(payload, "run_id")?,
            body: string_field(payload, "body")?,
        }),
        CONFLICT_PRODUCER => Ok(FrontendRequest::RecordProducerSideConflict {
            execution_id: string_field(payload, "execution_id")?,
            head_branch: string_field(payload, "head_branch")?,
            base_branch: string_field(payload, "base_branch")?,
            conflicted_files: serde_json::from_value(
                payload.get("conflicted_files").cloned().unwrap_or_else(|| json!([])),
            )
            .map_err(|error| format!("conflicted files are unreadable: {error}"))?,
        }),
        CONFLICT_MARK_FAILED => Ok(FrontendRequest::MarkConflictResolutionFailed {
            attempt_id: string_field(payload, "attempt_id")?,
            reason: string_field(payload, "reason")?,
            evidence: extra_field::<ArchitecturalMismatchEvidence>(payload, "evidence")?,
        }),
        CI_MARK_FAILED => Ok(FrontendRequest::MarkCiRemediationFailed {
            attempt_id: string_field(payload, "attempt_id")?,
            reason: string_field(payload, "reason")?,
            queue_ci_evidence: extra_field::<QueueCiEvidence>(payload, "queue_ci_evidence")?,
        }),
        other => Err(format!("payload contract `{other}` is not supported by this hub")),
    }
}

/// Check the field each contract actually writes against: `execution_id` for
/// a producer-side conflict, `run_id` for the others. A payload that carries
/// both fields with different values is refused rather than trusting `run_id`
/// to stand in for another execution.
fn bind_run_write(execution_id: &str, contract: &str, payload: &Value) -> Result<(), String> {
    let run_id = payload.get("run_id").and_then(Value::as_str);
    let execution_field = payload.get("execution_id").and_then(Value::as_str);
    if let (Some(run_id), Some(execution_field)) = (run_id, execution_field)
        && run_id != execution_field
    {
        return Err("run-scoped write carries both run_id and execution_id with different values".into());
    }
    let claimed = match contract {
        CONFLICT_PRODUCER => execution_field,
        _ => run_id,
    }
    .unwrap_or_default();
    if claimed != execution_id {
        return Err("run-scoped write is not bound to this execution".into());
    }
    Ok(())
}

fn string_field(payload: &Value, name: &str) -> Result<String, String> {
    payload
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("run-scoped write is missing `{name}`"))
}

fn extra_field<T: DeserializeOwned>(payload: &Value, name: &str) -> Result<Option<T>, String> {
    let extra = payload.get("extra").cloned().unwrap_or(Value::Null);
    match extra.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| format!("{name} is unreadable: {error}")),
    }
}

/// Replay-safe ownership checks that must not go through the live handlers
/// a second time: a terminal attempt stays accepted, and an attempt that
/// belongs to another work item is refused.
fn replay_or_ownership(state: &ServerState, work_item_id: &str, request: &FrontendRequest) -> Option<Proposed> {
    match request {
        FrontendRequest::MarkConflictResolutionFailed { attempt_id, .. } => {
            match state.work_db.get_conflict_resolution(attempt_id) {
                Err(error) => Some(Proposed::Retry(format!("{error:#}"))),
                Ok(None) => None,
                Ok(Some(attempt)) if attempt.work_item_id != work_item_id => Some(Proposed::Decided(
                    Decision::refused(format!("conflict attempt `{attempt_id}` does not belong to this run")),
                )),
                Ok(Some(attempt)) if attempt.status == "failed" || attempt.status == "succeeded" => {
                    Some(Proposed::Decided(accepted_write("already terminal")))
                }
                Ok(Some(_)) => None,
            }
        }
        FrontendRequest::MarkCiRemediationFailed { attempt_id, .. } => {
            match state.work_db.get_ci_remediation(attempt_id) {
                Err(error) => Some(Proposed::Retry(format!("{error:#}"))),
                Ok(None) => None,
                Ok(Some(attempt)) if attempt.work_item_id != work_item_id => Some(Proposed::Decided(
                    Decision::refused(format!("ci attempt `{attempt_id}` does not belong to this run")),
                )),
                Ok(Some(attempt)) if attempt.status == "failed" || attempt.status == "succeeded" => {
                    Some(Proposed::Decided(accepted_write("already terminal")))
                }
                Ok(Some(_)) => None,
            }
        }
        _ => None,
    }
}

async fn apply_run_write(
    state: &ServerState,
    origin: &Origin,
    contract: &str,
    key: &str,
    request: FrontendRequest,
) -> Proposed {
    let application = boss_work_db::SpokeWriteApplication {
        key,
        host_id: origin.host_id.as_str(),
        contract,
    };
    let projected = WriteOrigin::PROJECTED;
    let result: std::result::Result<(), boss_work_db::SpokeWriteError> = match request {
        FrontendRequest::CommentsPostAnswer { run_id, body } => {
            run_writes::answer_reply(state, projected, &run_id, &body, Some(application))
                .await
                .map(|_| ())
        }
        FrontendRequest::RecordProducerSideConflict {
            execution_id,
            head_branch,
            base_branch,
            conflicted_files,
        } => run_writes::producer_conflict(
            state,
            boss_work_db::ProducerConflictInsertInput::builder()
                .execution_id(execution_id)
                .head_branch(head_branch)
                .base_branch(base_branch)
                .conflicted_files(conflicted_files)
                .application_key(key.to_owned())
                .spoke_host_id(origin.host_id.clone())
                .build(),
        )
        .map(|_| ()),
        FrontendRequest::CommentsRecordGuideOutcome {
            run_id,
            comment_id,
            disposition,
            body,
            request_regeneration,
        } => {
            let outcome = boss_protocol::GuideCommentOutcome::builder()
                .comment_id(comment_id)
                .disposition(disposition)
                .response(body)
                .request_regeneration(request_regeneration)
                .build();
            run_writes::guide_outcome(state, projected, &run_id, outcome, Some(application))
                .await
                .map(|_| ())
        }
        FrontendRequest::MarkConflictResolutionFailed {
            attempt_id,
            reason,
            evidence,
        } => run_writes::conflict_mark_failed(state, &attempt_id, &reason, evidence.as_ref(), Some(application))
            .await
            .map(|_: ConflictMarked| ()),
        FrontendRequest::MarkCiRemediationFailed {
            attempt_id,
            reason,
            queue_ci_evidence,
        } => run_writes::ci_mark_failed(
            state,
            &attempt_id,
            &reason,
            queue_ci_evidence.as_ref(),
            Some(application),
        )
        .await
        .map(|_| ()),
        _ => return Proposed::Decided(Decision::refused("payload contract is not supported by this hub")),
    };
    match result {
        Ok(()) => Proposed::Decided(accepted_write("applied")),
        Err(boss_work_db::SpokeWriteError::Retry(error)) => Proposed::Retry(format!("{error:#}")),
        Err(boss_work_db::SpokeWriteError::Refused(reason)) => Proposed::Decided(Decision::refused(reason)),
    }
}

fn accepted_write(state: &str) -> Decision {
    Decision {
        accepted: true,
        reason: String::new(),
        result: json!({ "state": state }),
    }
}
