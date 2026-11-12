//! What a spoke's journaled observations do to the engine's work state.
//!
//! The hub's projector (`boss_spoke_hub::projection`) reads each host's stored
//! records in order and hands them here. Each kind goes to the consumer that
//! already owns it, so a remote worker's hook event, queued `done` and pane exit
//! are handled by the same code that handles a local worker's:
//!
//! * a **hook event** takes the fan-out a hook arriving on the local events
//!   socket takes (live state, completion on `Stop`, probes, and the rest),
//!   which is also what feeds the spoke turn map that mid-turn probe delivery
//!   reads;
//! * a **queued proposal** goes through the same validators, apply pipeline and
//!   finalization as `SubmitProposal`, and yields the decision the spoke's
//!   worker is waiting on;
//! * a **pane observation** is reflected in live state, and nothing more: what
//!   absence or an exit means for work status stays with the existing liveness
//!   policy.
//!
//! Nothing here is reachable until a hub listener is configured, and a spoke
//! host is enrolled and proven ready.

use async_trait::async_trait;
use boss_engine_driver::DriverRegistry;
use boss_engine_proposal_validation::{derive_idempotency_key, validate_caller_idempotency_key, validate_payload};
use boss_engine_runtime::hub::{HubStream, projection};
use boss_protocol::{
    ArchitecturalMismatchEvidence, FrontendRequest, ProposalErrorCode, ProposalKind, ProposalSubmissionError,
    QueueCiEvidence, WorkerProposal,
};
use boss_protocol_spoke::proposal::{
    ANSWER_REPLY, CI_MARK_FAILED, CONFLICT_MARK_FAILED, CONFLICT_PRODUCER, GUIDE_OUTCOME,
};
use projection::{
    Applied, Decision, EventObservation, Life, ObservationApplier, Origin, PaneObservation, ProjectorConfig, Proposed,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::oneshot;

use super::{
    ServerState,
    proposals::{self, AttributedCaller, Submission},
    worker_events::dispatch_worker_event_fanout,
};

struct ServerApplier {
    /// Weak, so the projector task never keeps a shut-down engine alive.
    state: Weak<ServerState>,
    drivers: DriverRegistry,
    /// Accepted submissions whose finish waits until the decision is delivered.
    pending_finish: Mutex<HashMap<String, proposals::Accepted>>,
}

/// The applier that routes projected observations into `state`.
pub(super) fn applier(state: &Arc<ServerState>) -> Arc<dyn ObservationApplier> {
    Arc::new(ServerApplier {
        state: Arc::downgrade(state),
        drivers: DriverRegistry::default(),
        pending_finish: Mutex::new(HashMap::new()),
    })
}

/// Start applying what the hub's spokes report, on the engine task scope.
pub(super) fn start(state: &Arc<ServerState>, stream: &HubStream) {
    if let Err(reason) = state.restore_spoke_turns() {
        tracing::warn!(%reason, "cannot restore spoke turn state at projector start; the first event retries it");
    }
    let projector = stream.projector(
        state.work_db.as_ref().clone(),
        applier(state),
        ProjectorConfig::default(),
    );
    boss_task_scope::spawn(projector.run());
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
        // never opens it, so the event carries none; hub-side transcript custody
        // supplies transcript reads by run identity instead. Stop-time consumers
        // that read a transcript therefore see unavailable evidence.
        if matches!(observation.event, boss_protocol::WorkerEvent::Stop { .. }) {
            tracing::info!(
                execution_id,
                from_subagent = observation.from_subagent,
                "projected spoke Stop carries no hub-local transcript path; Stop-time consumers that read a transcript see unavailable evidence"
            );
        }
        let ingress_seq = state
            .accepted_hook_seq
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let incoming = boss_engine_runtime::events_socket::IncomingHookEvent::resolve(
            driver.as_ref(),
            observation.event.clone(),
            Some(execution_id.to_owned()),
            None,
            None,
        )
        .with_subagent_origin(observation.from_subagent)
        .with_ingress_seq(ingress_seq)
        .with_observed_at_ms(observation.origin.observed.wall_unix_ms);
        dispatch_worker_event_fanout(&state, &incoming).await;
        if let Err(reason) = state.restore_spoke_turns() {
            return Applied::Retry(reason);
        }
        if let Err(reason) = state.record_spoke_turn_event(execution_id, &observation.event, observation.from_subagent)
        {
            return Applied::Retry(reason);
        }
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

    async fn decision_settled(&self, effect_key: &str, delivered: bool) {
        let accepted = self
            .pending_finish
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(effect_key);
        let Some(accepted) = accepted else {
            return;
        };
        let (sent, delivered_rx) = oneshot::channel();
        let _ = sent.send(delivered);
        accepted.finish(delivered_rx).await;
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
            Ok(Some(write)) => {
                return if write.accepted {
                    Proposed::Decided(accepted_write("already applied"))
                } else {
                    Proposed::Decided(Decision::refused(write.reason))
                };
            }
            Ok(None) => {}
            Err(error) => return Proposed::Retry(format!("{error:#}")),
        }
        let request = match request_for_run_write(execution_id, contract, payload) {
            Ok(request) => request,
            Err(reason) => return Proposed::Decided(Decision::refused(reason)),
        };
        if let Some(decision) = replay_or_ownership(&state, execution_id, &work_item_id, &request) {
            return decision;
        }
        apply_run_write(&state, origin, contract, key, request)
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
    let claimed = payload
        .get("run_id")
        .or_else(|| payload.get("execution_id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if claimed != execution_id {
        return Err("run-scoped write is not bound to this execution".into());
    }
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
fn replay_or_ownership(
    state: &ServerState,
    execution_id: &str,
    work_item_id: &str,
    request: &FrontendRequest,
) -> Option<Proposed> {
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
        FrontendRequest::CommentsPostAnswer { .. } => match state.work_db.get_execution(execution_id) {
            Err(error) => Some(Proposed::Retry(format!("{error:#}"))),
            Ok(execution) if execution.kind != boss_work_db::ExecutionKind::AnswerAgent => Some(Proposed::Decided(
                Decision::refused(format!("execution `{execution_id}` is not an answer-agent run")),
            )),
            Ok(_) => None,
        },
        _ => None,
    }
}

fn apply_run_write(
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
    let result = match request {
        FrontendRequest::CommentsPostAnswer { run_id, body } => state
            .work_db
            .apply_answer_agent_reply(&run_id, &body, Some(application))
            .map(|_| ()),
        FrontendRequest::RecordProducerSideConflict {
            execution_id,
            head_branch,
            base_branch,
            conflicted_files,
        } => state
            .work_db
            .record_producer_side_conflict(
                boss_work_db::ProducerConflictInsertInput::builder()
                    .execution_id(execution_id)
                    .head_branch(head_branch)
                    .base_branch(base_branch)
                    .conflicted_files(conflicted_files)
                    .application_key(key.to_owned())
                    .spoke_host_id(origin.host_id.clone())
                    .build(),
            )
            .map(|_| ())
            .map_err(boss_work_db::SpokeWriteError::from_work),
        FrontendRequest::CommentsRecordGuideOutcome {
            run_id,
            comment_id,
            disposition,
            body,
            request_regeneration,
        } => apply_guide_outcome(
            state,
            &run_id,
            comment_id,
            disposition,
            body,
            request_regeneration,
            application,
        ),
        FrontendRequest::MarkConflictResolutionFailed {
            attempt_id,
            reason,
            evidence,
        } => apply_conflict_mark_failed(state, &attempt_id, reason, evidence, application),
        FrontendRequest::MarkCiRemediationFailed {
            attempt_id,
            reason,
            queue_ci_evidence,
        } => apply_ci_mark_failed(state, &attempt_id, reason, queue_ci_evidence, application),
        _ => return Proposed::Decided(Decision::refused("payload contract is not supported by this hub")),
    };
    match result {
        Ok(()) => Proposed::Decided(accepted_write("applied")),
        Err(boss_work_db::SpokeWriteError::Retry(error)) => Proposed::Retry(format!("{error:#}")),
        Err(boss_work_db::SpokeWriteError::Refused(reason)) => Proposed::Decided(Decision::refused(reason)),
    }
}

fn apply_guide_outcome(
    state: &ServerState,
    run_id: &str,
    comment_id: String,
    disposition: boss_protocol::GuideCommentDisposition,
    body: String,
    request_regeneration: bool,
    application: boss_work_db::SpokeWriteApplication<'_>,
) -> std::result::Result<(), boss_work_db::SpokeWriteError> {
    if body.trim().is_empty() {
        return Err(boss_work_db::SpokeWriteError::refused(
            "guide outcome body may not be empty",
        ));
    }
    let execution = state
        .work_db
        .get_execution(run_id)
        .map_err(boss_work_db::SpokeWriteError::from_work)?;
    if execution.kind != boss_work_db::ExecutionKind::RevisionImplementation {
        return Err(boss_work_db::SpokeWriteError::refused(format!(
            "run '{run_id}' is not a revision execution"
        )));
    }
    state
        .work_db
        .record_guide_comment_outcome(
            &execution.work_item_id,
            boss_protocol::GuideCommentOutcome::builder()
                .comment_id(comment_id)
                .disposition(disposition)
                .response(body)
                .request_regeneration(request_regeneration)
                .build(),
        )
        .map(|_| ())
        .map_err(boss_work_db::SpokeWriteError::from_work)?;
    record_accepted_write(state, application)
}

fn apply_conflict_mark_failed(
    state: &ServerState,
    attempt_id: &str,
    reason: String,
    evidence: Option<ArchitecturalMismatchEvidence>,
    application: boss_work_db::SpokeWriteApplication<'_>,
) -> std::result::Result<(), boss_work_db::SpokeWriteError> {
    if reason == boss_protocol::CONFLICT_ARCHITECTURAL_MISMATCH_REASON {
        match state
            .work_db
            .record_conflict_architectural_mismatch(attempt_id, evidence.as_ref())
            .map_err(boss_work_db::SpokeWriteError::from_work)?
        {
            Some(_) => return record_accepted_write(state, application),
            None => {
                return Err(boss_work_db::SpokeWriteError::refused(format!(
                    "conflict resolution attempt {attempt_id:?} is unknown or already terminal"
                )));
            }
        }
    }
    match state
        .work_db
        .mark_conflict_resolution_failed(attempt_id, &reason)
        .map_err(boss_work_db::SpokeWriteError::from_work)?
    {
        Some(_) => record_accepted_write(state, application),
        None => Err(boss_work_db::SpokeWriteError::refused(format!(
            "conflict resolution attempt {attempt_id:?} is unknown or already terminal"
        ))),
    }
}

fn apply_ci_mark_failed(
    state: &ServerState,
    attempt_id: &str,
    reason: String,
    queue_ci_evidence: Option<QueueCiEvidence>,
    application: boss_work_db::SpokeWriteApplication<'_>,
) -> std::result::Result<(), boss_work_db::SpokeWriteError> {
    state
        .work_db
        .validate_queue_ci_failure(attempt_id, queue_ci_evidence.as_ref(), &reason)
        .map_err(boss_work_db::SpokeWriteError::from_work)?;
    let reason = match queue_ci_evidence {
        Some(evidence) => format!(
            "{reason}\nQueue verification: main_sha={} verified_head_sha={} runs={}",
            evidence.main_sha,
            evidence.verified_head_sha,
            evidence
                .runs
                .iter()
                .map(|run| format!("{}: {} (exit {})", run.check_name, run.command, run.exit_code))
                .collect::<Vec<_>>()
                .join("; ")
        ),
        None => reason,
    };
    match state
        .work_db
        .mark_ci_remediation_failed(attempt_id, &reason)
        .map_err(boss_work_db::SpokeWriteError::from_work)?
    {
        Some(_) => record_accepted_write(state, application),
        None => Err(boss_work_db::SpokeWriteError::refused(format!(
            "ci_remediation attempt {attempt_id:?} is unknown or already terminal"
        ))),
    }
}

fn record_accepted_write(
    state: &ServerState,
    application: boss_work_db::SpokeWriteApplication<'_>,
) -> std::result::Result<(), boss_work_db::SpokeWriteError> {
    state
        .work_db
        .record_spoke_applied_write(&boss_work_db::SpokeAppliedWrite {
            application_key: application.key.to_owned(),
            host_id: application.host_id.to_owned(),
            contract: application.contract.to_owned(),
            accepted: true,
            reason: String::new(),
            result_json: json!({"state": "applied"}).to_string(),
            created_at_ms: boss_engine_utils::epoch_time::now_epoch_ms() as i64,
        })
        .map_err(boss_work_db::SpokeWriteError::from_work)
}

fn accepted_write(state: &str) -> Decision {
    Decision {
        accepted: true,
        reason: String::new(),
        result: json!({ "state": state }),
    }
}
