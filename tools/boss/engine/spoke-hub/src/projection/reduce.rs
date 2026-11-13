//! Pure reducers: one stored ingress frame in, the observations it carries out.
//!
//! Nothing here touches a database, a socket or engine state, so every rule
//! about what a frame means is testable on bytes alone. The projector owns
//! ordering, ownership checks and the cursor; the applier owns what an
//! observation does to work state.
//!
//! Decoding fails closed. A frame that does not decode, fails validation, or
//! carries an event or proposal this build cannot read is *quarantined*: a
//! failed `quarantine` effect is stored and never retried, and the cursor
//! moves on. It is never mapped to a success, and never silently dropped: an
//! unknown stop reason must not become completion evidence, and a proposal
//! this hub cannot read gets an explicit refusal rather than silence.

use boss_protocol::{ProposalKind, WorkerEvent};
use boss_protocol_spoke::{
    Validate,
    proposal::{self, ProposalJsonEnvelope},
    v1::{self, PaneState, Run, Stamp, record, spoke_frame},
    worker_event,
};
use prost::Message;
use serde_json::Value;

use boss_work_db::SpokeIngressRow;

/// The payload contract the hub reads `propose` calls under.
pub const PROPOSAL_JSON: &str = proposal::PROPOSAL_JSON;

/// Where an observation came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Origin {
    pub host_id: String,
    pub journal_id: String,
    /// The ingress sequence of the record that carried it.
    pub sequence: u64,
    /// The spoke's stamp when it recorded the observation.
    pub observed: Stamp,
    pub run: Run,
}

/// A hook event of one run, in the worker's emission order.
#[derive(Debug, Clone, PartialEq)]
pub struct EventObservation {
    pub origin: Origin,
    pub event: WorkerEvent,
    /// The event fired inside a subagent, not in the run's main session.
    pub from_subagent: bool,
}

/// What a queued proposal asks the hub to do.
#[derive(Debug, Clone, PartialEq)]
pub enum ProposalRequest {
    /// A proposal the existing validators and apply pipeline already handle.
    Standard {
        kind: ProposalKind,
        payload: Value,
        idempotency_key: String,
    },
    /// A run-scoped write the gateway queued under its own contract. The
    /// hub applies the same ownership-checked handlers a local worker's
    /// matching RPC uses.
    RunWrite {
        contract: String,
        payload: Value,
        idempotency_key: String,
    },
    /// The hub reads the record but refuses it, with this reason. Refusal is a
    /// decision the spoke needs, not a reason to hold the cursor.
    Refused(String),
}

/// A proposal the worker queued while the hub may have been unreachable.
#[derive(Debug, Clone, PartialEq)]
pub struct ProposalObservation {
    pub origin: Origin,
    pub proposal_id: String,
    pub request: ProposalRequest,
}

/// An online read, bound to the assignment authenticated at ingress.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerObservation {
    pub origin: Origin,
    pub request: v1::WorkerRequest,
}

/// A pane the host looked at, from a `PaneState` record or an inventory page.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneObservation {
    pub origin: Origin,
    pub pane: PaneState,
}

/// What the host saw its pane doing. Absence, an exit and an unreadable pane
/// stay separate observations: only the hub's policy gives any of them a work
/// consequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Life {
    Alive,
    /// The command ended, with the status tmux kept if it kept one.
    Exited {
        exit_code: Option<i32>,
    },
    /// Ended because the launch input never reached the driver.
    BriefNotDelivered,
    /// The host looked and the session is not there.
    Missing,
    /// The host could not tell. Not a death.
    Indeterminate,
}

impl PaneObservation {
    /// The observation's life, reading a value this build does not know as
    /// [`Life::Indeterminate`] rather than as anything conclusive.
    pub fn life(&self) -> Life {
        match v1::PaneLife::try_from(self.pane.life) {
            Ok(v1::PaneLife::Alive) => Life::Alive,
            Ok(v1::PaneLife::Exited) => Life::Exited {
                exit_code: self.pane.exit_code,
            },
            Ok(v1::PaneLife::BriefNotDelivered) => Life::BriefNotDelivered,
            Ok(v1::PaneLife::Missing) => Life::Missing,
            Ok(v1::PaneLife::Indeterminate | v1::PaneLife::PaneUnspecified) | Err(_) => Life::Indeterminate,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Observation {
    Event(EventObservation),
    Proposal(ProposalObservation),
    Pane(PaneObservation),
    Worker(WorkerObservation),
}

impl Observation {
    pub fn origin(&self) -> &Origin {
        match self {
            Self::Event(observation) => &observation.origin,
            Self::Proposal(observation) => &observation.origin,
            Self::Pane(observation) => &observation.origin,
            Self::Worker(observation) => &observation.origin,
        }
    }
}

/// Why a frame carries nothing for this projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// A command result: stored with its receipt, so applying it again would
    /// be a second settlement.
    SettledAtIngress,
    /// Display data for a viewer. The stream manager already fed it to
    /// whoever was watching; nothing replays it.
    LiveOnly,
    /// Real, but another consumer owns its meaning.
    OtherConsumer(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reduction {
    Skip(Skip),
    /// Cannot be read. Stored as a failed effect and never retried; the cursor
    /// moves on.
    Quarantine(String),
    Observations(Vec<Observation>),
}

/// Reduce one stored frame.
pub fn reduce(host_id: &str, journal_id: &str, row: &SpokeIngressRow) -> Reduction {
    if row.frame.is_empty() {
        return Reduction::Skip(Skip::LiveOnly);
    }
    let frame = match v1::SpokeFrame::decode(row.frame.as_slice()) {
        Ok(frame) => frame,
        Err(error) => return Reduction::Quarantine(format!("frame does not decode: {error}")),
    };
    let Some(spoke_frame::Body::Record(record)) = frame.body else {
        return Reduction::Quarantine("frame is not a record".into());
    };
    if let Err(error) = record.validate() {
        return Reduction::Quarantine(format!("record is invalid: {error}"));
    }
    let v1::Record {
        sequence,
        observed,
        run,
        body,
    } = record;
    if sequence != row.sequence {
        return Reduction::Quarantine(format!(
            "record sequence {sequence} does not match its ingress sequence {}",
            row.sequence
        ));
    }
    let origin = |run: Run| Origin {
        host_id: host_id.to_owned(),
        journal_id: journal_id.to_owned(),
        sequence,
        observed: observed.clone().unwrap_or_default(),
        run,
    };
    // `validate` already required a run on every record kind below that is
    // run-bound, so a missing one here is a validator bug, not input.
    let Some(body) = body else {
        return Reduction::Quarantine("record has no body".into());
    };
    match body {
        record::Body::Event(wire) => {
            let Some(run) = run else {
                return Reduction::Quarantine("event record has no run".into());
            };
            match worker_event::from_wire(wire) {
                Ok(decoded) => Reduction::Observations(vec![Observation::Event(EventObservation {
                    origin: origin(run),
                    event: decoded.event,
                    from_subagent: decoded.from_subagent,
                })]),
                Err(error) => Reduction::Quarantine(format!("worker event is unreadable: {error}")),
            }
        }
        record::Body::Proposal(proposal) => {
            let Some(run) = run else {
                return Reduction::Quarantine("proposal record has no run".into());
            };
            Reduction::Observations(vec![Observation::Proposal(ProposalObservation {
                origin: origin(run),
                request: proposal_request(&proposal),
                proposal_id: proposal.proposal_id,
            })])
        }
        record::Body::Pane(pane) => {
            let Some(run) = run else {
                return Reduction::Quarantine("pane record has no run".into());
            };
            Reduction::Observations(vec![Observation::Pane(PaneObservation {
                origin: origin(run),
                pane,
            })])
        }
        record::Body::Inventory(snapshot) => Reduction::Observations(
            snapshot
                .runs
                .into_iter()
                .filter_map(|member| {
                    Some(Observation::Pane(PaneObservation {
                        origin: origin(member.run?),
                        pane: member.pane?,
                    }))
                })
                .collect(),
        ),
        record::Body::Result(_) => Reduction::Skip(Skip::SettledAtIngress),
        record::Body::Capture(_) => Reduction::Skip(Skip::LiveOnly),
        record::Body::Capacity(_) => Reduction::Skip(Skip::OtherConsumer("capacity")),
        record::Body::Transcript(_) => Reduction::Skip(Skip::OtherConsumer("transcript")),
        record::Body::Artifact(_) => Reduction::Skip(Skip::OtherConsumer("artifact")),
        record::Body::Gap(_) => Reduction::Skip(Skip::OtherConsumer("gap")),
        record::Body::Request(request) => {
            let Some(run) = run else {
                return Reduction::Quarantine("worker request has no run".into());
            };
            Reduction::Observations(vec![Observation::Worker(WorkerObservation {
                origin: origin(run),
                request,
            })])
        }
    }
}

fn proposal_request(proposal: &v1::Proposal) -> ProposalRequest {
    if proposal::is_run_write_contract(&proposal.payload_contract) {
        if proposal.kind != v1::ProposalKind::OtherValidated as i32 {
            return ProposalRequest::Refused("run-scoped writes must be stamped OtherValidated on the wire".into());
        }
        let payload: Value = match serde_json::from_slice(&proposal.canonical_json) {
            Ok(Value::Object(map)) => Value::Object(map),
            Ok(_) => return ProposalRequest::Refused("run-scoped write payload must be a JSON object".into()),
            Err(error) => return ProposalRequest::Refused(format!("run-scoped write is unreadable: {error}")),
        };
        return ProposalRequest::RunWrite {
            contract: proposal.payload_contract.clone(),
            payload,
            idempotency_key: proposal.proposal_id.clone(),
        };
    }
    if proposal.payload_contract != PROPOSAL_JSON {
        return ProposalRequest::Refused(format!(
            "payload contract `{}` is not supported by this hub",
            proposal.payload_contract
        ));
    }
    let envelope: ProposalJsonEnvelope = match serde_json::from_slice(&proposal.canonical_json) {
        Ok(envelope) => envelope,
        Err(error) => return ProposalRequest::Refused(format!("proposal envelope is unreadable: {error}")),
    };
    if envelope.idempotency_key != proposal.proposal_id {
        return ProposalRequest::Refused("proposal id does not match its idempotency key".into());
    }
    if proposal::wire_kind(envelope.kind) as i32 != proposal.kind {
        return ProposalRequest::Refused(format!(
            "wire kind does not match the `{}` payload it carries",
            envelope.kind
        ));
    }
    if envelope.kind == ProposalKind::ReviewGuide {
        return ProposalRequest::Refused(
            "review guides need a host-local git store and are not accepted from a spoke".into(),
        );
    }
    ProposalRequest::Standard {
        kind: envelope.kind,
        payload: envelope.payload,
        idempotency_key: envelope.idempotency_key,
    }
}
