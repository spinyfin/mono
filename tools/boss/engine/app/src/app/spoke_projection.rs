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

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use boss_engine_driver::DriverRegistry;
use boss_engine_proposal_validation::{derive_idempotency_key, validate_caller_idempotency_key, validate_payload};
use boss_engine_runtime::hub::{HubStream, projection};
use boss_protocol::{ProposalErrorCode, ProposalKind, ProposalSubmissionError, WorkerProposal};
use projection::{
    Applied, Decision, EventObservation, Life, ObservationApplier, Origin, PaneObservation, ProjectorConfig, Proposed,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::{
    ServerState,
    proposals::{self, AttributedCaller, Submission},
    worker_events::dispatch_worker_event_fanout,
};

/// Consecutive internal failures of one proposal before the hub stops holding
/// its host's cursor for it and answers the worker with a refusal. An internal
/// failure is a database fault in practice, so this is generous: it outlasts a
/// restart-length outage, and bounds a deterministic fault that would otherwise
/// hold every record behind it forever.
const MAX_INTERNAL_ATTEMPTS: u32 = 12;

struct ServerApplier {
    /// Weak, so the projector task never keeps a shut-down engine alive.
    state: Weak<ServerState>,
    drivers: DriverRegistry,
    /// Internal failures per `(run, proposal)`, in memory: a restart gives a
    /// proposal a fresh budget, which is the safe direction.
    internal_failures: Mutex<HashMap<String, u32>>,
}

/// The applier that routes projected observations into `state`.
pub(super) fn applier(state: &Arc<ServerState>) -> Arc<dyn ObservationApplier> {
    Arc::new(ServerApplier {
        state: Arc::downgrade(state),
        drivers: DriverRegistry::default(),
        internal_failures: Mutex::new(HashMap::new()),
    })
}

/// Start applying what the hub's spokes report, on the engine task scope.
pub(super) fn start(state: &Arc<ServerState>, stream: &HubStream) {
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

    /// Count one more internal failure of `key`. Returns whether the budget is
    /// spent.
    fn spend_internal_attempt(&self, key: &str) -> bool {
        let mut failures = self
            .internal_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let spent = failures.entry(key.to_owned()).or_insert(0);
        *spent += 1;
        *spent >= MAX_INTERNAL_ATTEMPTS
    }

    fn forget(&self, key: &str) {
        self.internal_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
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
        // supplies transcript reads by run identity instead.
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
        .with_ingress_seq(ingress_seq);
        dispatch_worker_event_fanout(&state, &incoming).await;
        Applied::Done
    }

    async fn proposal(&self, origin: &Origin, kind: ProposalKind, payload: &Value, key: &str) -> Proposed {
        let state = match self.state() {
            Ok(state) => state,
            Err(reason) => return Proposed::Retry(reason),
        };
        let execution_id = origin.run.execution_id.as_str();
        let internal = format!("{}:{key}", origin.run.run_id);
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
            Ok(Submission::Replayed(proposal)) => {
                self.forget(&internal);
                Proposed::Decided(accepted(&proposal, true))
            }
            Ok(Submission::Accepted(accepted_submission)) => {
                self.forget(&internal);
                let decision = accepted(&accepted_submission.proposal, accepted_submission.already_submitted);
                // The spoke's worker holds no connection open for this answer,
                // so teardown has no acknowledgement to wait for.
                let (sent, delivered) = oneshot::channel();
                let _ = sent.send(true);
                accepted_submission.finish(delivered).await;
                Proposed::Decided(decision)
            }
            Err(error) if error.code == ProposalErrorCode::Internal => {
                if self.spend_internal_attempt(&internal) {
                    self.forget(&internal);
                    Proposed::Decided(rejected(&error))
                } else {
                    Proposed::Retry(error.message)
                }
            }
            Err(error) => {
                self.forget(&internal);
                Proposed::Decided(rejected(&error))
            }
        }
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
