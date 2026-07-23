//! Worker proposals: the mediated worker→engine submission mechanism that
//! replaces the marker/text-block seams (`[effort-escalation]`, `FOLLOWUPS:`,
//! hook-observed PR-creation inference, …) with durable, typed rows.
//!
//! `WorkerProposal` mirrors the `worker_proposals` table verbatim —
//! attribution, idempotency, and disposition state, all engine-owned.
//! `ProposalKind` is the closed v1 vocabulary; each kind has its own
//! payload struct (stored, JSON-encoded, in `WorkerProposal::payload_json`)
//! matching the design's "Data model" kinds table exactly.
//!
//! Design: `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! §"Data model". This module ships types + schema only — no engine
//! behavior (submission, validation, apply pipeline) lands until the
//! follow-on implementation tasks.

use super::common::EffortLevel;
use serde::{Deserialize, Serialize};

/// Closed v1 vocabulary for `worker_proposals.kind`. Engine-owned: adding a
/// kind is an engine change, not a worker-side extension point (design
/// §"Non-goals": "A general-purpose workflow/plugin system"). Exhaustive
/// match enforces that every kind-keyed callsite handles a new variant
/// explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    Attention,
    EffortEscalation,
    Blocked,
    DeferredScope,
    FollowupTask,
    AutomationOutcome,
    PrCreated,
}

impl ProposalKind {
    pub const ALL: &'static [ProposalKind] = &[
        ProposalKind::Attention,
        ProposalKind::EffortEscalation,
        ProposalKind::Blocked,
        ProposalKind::DeferredScope,
        ProposalKind::FollowupTask,
        ProposalKind::AutomationOutcome,
        ProposalKind::PrCreated,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProposalKind::Attention => "attention",
            ProposalKind::EffortEscalation => "effort_escalation",
            ProposalKind::Blocked => "blocked",
            ProposalKind::DeferredScope => "deferred_scope",
            ProposalKind::FollowupTask => "followup_task",
            ProposalKind::AutomationOutcome => "automation_outcome",
            ProposalKind::PrCreated => "pr_created",
        }
    }
}

impl std::fmt::Display for ProposalKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ProposalKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "attention" => Ok(ProposalKind::Attention),
            "effort_escalation" => Ok(ProposalKind::EffortEscalation),
            "blocked" => Ok(ProposalKind::Blocked),
            "deferred_scope" => Ok(ProposalKind::DeferredScope),
            "followup_task" => Ok(ProposalKind::FollowupTask),
            "automation_outcome" => Ok(ProposalKind::AutomationOutcome),
            "pr_created" => Ok(ProposalKind::PrCreated),
            other => Err(format!(
                "unknown proposal kind `{other}`; expected one of: attention, effort_escalation, \
                 blocked, deferred_scope, followup_task, automation_outcome, pr_created"
            )),
        }
    }
}

/// Lifecycle of a `worker_proposals` row. `proposed` is the durable ingress
/// state every submission starts in; everything else is a disposition.
///
/// - `applied`: the apply pipeline (auto-apply or the human batch-accept
///   gesture, per kind) produced an effect; `WorkerProposal::applied_ref`
///   points at the row it produced.
/// - `rejected`: a policy/human judgment declined the proposal (e.g. a
///   dedup verdict that an equivalent task already exists); `decision_reason`
///   carries why.
/// - `superseded`: a newer proposal in the same idempotency scope replaced
///   this one before it was decided (e.g. triage revises its outcome).
/// - `expired`: still undecided when the originating execution reached a
///   terminal state, for kinds that are only meaningful in-flight (v1:
///   `effort_escalation` and `blocked` only — never `followup_task`, whose
///   pending proposals outlive their execution in the `followup` attention
///   group).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    #[default]
    Proposed,
    Applied,
    Rejected,
    Superseded,
    Expired,
}

impl ProposalState {
    pub const ALL: &'static [ProposalState] = &[
        ProposalState::Proposed,
        ProposalState::Applied,
        ProposalState::Rejected,
        ProposalState::Superseded,
        ProposalState::Expired,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProposalState::Proposed => "proposed",
            ProposalState::Applied => "applied",
            ProposalState::Rejected => "rejected",
            ProposalState::Superseded => "superseded",
            ProposalState::Expired => "expired",
        }
    }
}

impl std::fmt::Display for ProposalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ProposalState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "proposed" => Ok(ProposalState::Proposed),
            "applied" => Ok(ProposalState::Applied),
            "rejected" => Ok(ProposalState::Rejected),
            "superseded" => Ok(ProposalState::Superseded),
            "expired" => Ok(ProposalState::Expired),
            other => Err(format!(
                "unknown proposal state `{other}`; expected one of: \
                 proposed, applied, rejected, superseded, expired"
            )),
        }
    }
}

/// Closed vocabulary for `worker_proposals.decided_by`: who/what decided a
/// proposal's disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalDecider {
    /// An auto-apply policy applier acted without human/coordinator input.
    Policy,
    Coordinator,
    Human,
}

impl ProposalDecider {
    pub const ALL: &'static [ProposalDecider] = &[
        ProposalDecider::Policy,
        ProposalDecider::Coordinator,
        ProposalDecider::Human,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProposalDecider::Policy => "policy",
            ProposalDecider::Coordinator => "coordinator",
            ProposalDecider::Human => "human",
        }
    }
}

impl std::fmt::Display for ProposalDecider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ProposalDecider {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "policy" => Ok(ProposalDecider::Policy),
            "coordinator" => Ok(ProposalDecider::Coordinator),
            "human" => Ok(ProposalDecider::Human),
            other => Err(format!(
                "unknown proposal decider `{other}`; expected one of: policy, coordinator, human"
            )),
        }
    }
}

/// One row in the `worker_proposals` ingress ledger — the durable record of
/// a `boss propose <kind>` submission. `payload_json` is the JSON encoding
/// of the kind-matched payload struct below (e.g. `kind = FollowupTask` ⇒
/// `payload_json` deserializes as [`FollowupTaskProposalPayload`]); this
/// type does not enforce that correspondence itself; the submission RPC
/// validates that correspondence at write time.
///
/// `(execution_id, idempotency_key)` is UNIQUE at the schema level, so a
/// retried or resumed `boss propose` call is safe to replay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct WorkerProposal {
    pub id: String,
    pub execution_id: String,

    pub created_at: String,
    pub idempotency_key: String,
    pub kind: ProposalKind,
    pub payload_json: String,
    /// Durable ingress state. Fresh submissions default to `proposed`;
    /// pre-existing rows never predate this column since the table is new.
    #[serde(default)]
    #[builder(default)]
    pub state: ProposalState,

    /// Id of the row the apply pipeline produced (`atn_…`, `atg_…`,
    /// `task_…`, …). `None` until `state = applied`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_ref: Option<String>,

    /// When this proposal left `proposed`. `None` while still pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,

    /// Who/what decided this proposal's disposition. `None` while still
    /// `proposed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<ProposalDecider>,

    /// Human-readable reason for a `rejected` (or otherwise notable)
    /// disposition, e.g. `"duplicate of an existing task"`. `None` for
    /// proposals with no decision yet, and for most `applied` rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,

    /// Work item this proposal was filed against, derived from the
    /// execution at insert time. Denormalised (rather than joined through
    /// `work_executions` on every read) so `ListProposals` can cheaply
    /// scope to "every proposal for this work item, across executions" —
    /// the read a resumed/successor run needs to see prior dispositions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Per-kind payloads (`WorkerProposal::payload_json`, typed)
// ---------------------------------------------------------------------------

/// Payload for `ProposalKind::Attention`. Auto-applies to an attention
/// item/group — the same rows detectors write today.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttentionProposalPayload {
    pub body_markdown: String,
    pub title: String,
    /// Discriminator mirroring `work_attention_items.kind` (e.g.
    /// `"question"`, `"info"`). `None` → engine default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention_kind: Option<String>,
}

/// Payload for `ProposalKind::EffortEscalation`. Auto-applies to a
/// worker-signal attention plus an auto-nudge pause, same as the legacy
/// `[effort-escalation]` marker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EffortEscalationProposalPayload {
    pub reason: String,
    pub requested_level: EffortLevel,
}

/// Payload for `ProposalKind::Blocked`. Auto-applies to a worker-signal
/// attention plus a nudge pause, same as the legacy `[blocked]` marker
/// (which stays as the bootstrap fallback of last resort — see the
/// design's §"Failure semantics").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockedProposalPayload {
    pub reason: String,
}

/// Payload for `ProposalKind::DeferredScope`. Auto-applies to a durable
/// audit line on the work item plus an attention.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeferredScopeProposalPayload {
    pub reason: String,
    pub summary: String,
}

/// Payload for `ProposalKind::FollowupTask`. Gated: upserts a member into
/// the originating task's `followup` attention group; task creation still
/// requires the human batch-accept gesture (dedup/scoring verdicts run
/// there, not at submission).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FollowupTaskProposalPayload {
    pub proposed_description: String,
    pub proposed_name: String,
    pub rationale: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_effort: Option<EffortLevel>,
    /// One of `"task"` / `"chore"` / `"project"`. `None` → engine default
    /// (`"chore"`, matching `CreateChoreInput`'s historical default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_work_kind: Option<String>,
}

/// Payload for `ProposalKind::AutomationOutcome`. Internally tagged on
/// `outcome` so the two triage results (`produced_task` / `skip`) are
/// distinct, non-overlapping shapes rather than a struct with optional
/// fields for both. Auto-applies with a provenance check: `ProducedTask`
/// validates `task_id` exists and carries a matching `source_automation_id`
/// before finalization reads it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AutomationOutcomeProposalPayload {
    ProducedTask { task_id: String },
    Skip { reason: String },
}

/// Payload for `ProposalKind::PrCreated`. Auto-applies with verification
/// (URL shape + product-repo slug, branch match against the execution)
/// before binding the PR to the work item — the worker's terminal action,
/// replacing the hook-observed `gh pr create` stdout inference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrCreatedProposalPayload {
    pub pr_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

// ---------------------------------------------------------------------------
// Submission errors and rate caps (the `SubmitProposal` / `ListProposals` RPCs)
// ---------------------------------------------------------------------------

/// Why a `SubmitProposal` / `ListProposals` call was refused.
///
/// The whole point of the proposal API is that a bad submission produces an
/// *immediate, typed* failure the worker can act on mid-run, rather than a
/// silent parse failure discovered at Stop (design §"Failure semantics":
/// "Malformed submission → immediate typed error, worker fixes in-run. This
/// is the primary win over parse-at-a-distance"). So the code is a closed
/// vocabulary keyed on what the caller must *do differently*, not a single
/// bucket with prose inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalErrorCode {
    /// The payload did not satisfy its kind's schema. Remediation: read
    /// [`ProposalSubmissionError::field_errors`], fix those fields, retry.
    ValidationFailed,
    /// A per-execution rate cap is exhausted
    /// ([`PROPOSAL_CAP_TOTAL_PER_EXECUTION`] /
    /// [`PROPOSAL_CAP_PER_KIND_PER_EXECUTION`]). Remediation: none in-run —
    /// this is runaway-loop protection, and the caps are generous enough
    /// that hitting one means something is looping.
    RateLimited,
    /// The connection has no local socket peer pid, so the engine cannot
    /// derive who is proposing. v1 scopes the proposal API to local workers
    /// (design §"Non-goals": remote SSH workers have no peer pid and are
    /// rejected until per-run token auth exists). Remediation: none — use
    /// the `[blocked]` bootstrap marker.
    NoLocalPeer,
    /// A local peer pid was present but its process ancestry contains no
    /// registered worker run, so the call cannot be attributed to an
    /// execution. Fails closed rather than trusting the caller-supplied run
    /// id — attribution is verified identity, never a worker-supplied flag.
    AttributionUnresolved,
    /// The caller-supplied `BOSS_RUN_ID` disagrees with the run the socket
    /// peer resolves to. `BOSS_RUN_ID` is a cross-check, not a credential
    /// (design §"Transport and authn"); a mismatch means a misconfigured
    /// env or a command copy-pasted across worker panes.
    AttributionMismatch,
    /// Attribution succeeded but the execution row it names is gone (or its
    /// work item is unreadable) — a stale registry entry for a pruned
    /// execution.
    UnknownExecution,
    /// The engine failed to persist or read the proposal. Distinguished
    /// from every code above so a worker does not "fix" a payload that was
    /// never the problem.
    Internal,
}

impl ProposalErrorCode {
    pub const ALL: &'static [ProposalErrorCode] = &[
        ProposalErrorCode::ValidationFailed,
        ProposalErrorCode::RateLimited,
        ProposalErrorCode::NoLocalPeer,
        ProposalErrorCode::AttributionUnresolved,
        ProposalErrorCode::AttributionMismatch,
        ProposalErrorCode::UnknownExecution,
        ProposalErrorCode::Internal,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProposalErrorCode::ValidationFailed => "validation_failed",
            ProposalErrorCode::RateLimited => "rate_limited",
            ProposalErrorCode::NoLocalPeer => "no_local_peer",
            ProposalErrorCode::AttributionUnresolved => "attribution_unresolved",
            ProposalErrorCode::AttributionMismatch => "attribution_mismatch",
            ProposalErrorCode::UnknownExecution => "unknown_execution",
            ProposalErrorCode::Internal => "internal",
        }
    }
}

impl std::fmt::Display for ProposalErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One field-scoped validation complaint. `field` is the payload key as the
/// worker wrote it (`"reason"`, `"outcome"`, `"pr_url"`), so a CLI can point
/// at the offending flag by name instead of echoing a whole-payload error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProposalFieldError {
    pub field: String,
    pub message: String,
}

impl ProposalFieldError {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// A refused submission or listing, as returned by the `SubmitProposal` /
/// `ListProposals` RPCs.
///
/// `message` is always populated and human-readable on its own;
/// `field_errors` is non-empty only for [`ProposalErrorCode::ValidationFailed`]
/// and carries the per-field detail that makes fix-and-retry mechanical.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProposalSubmissionError {
    pub code: ProposalErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_errors: Vec<ProposalFieldError>,
}

impl ProposalSubmissionError {
    /// A non-validation refusal: a code plus a human-readable explanation,
    /// with no per-field detail.
    pub fn new(code: ProposalErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            field_errors: Vec::new(),
        }
    }

    /// A [`ProposalErrorCode::ValidationFailed`] refusal carrying one
    /// complaint per offending field. The summary `message` names the count
    /// so a caller that only logs the message still says something useful.
    pub fn validation(field_errors: Vec<ProposalFieldError>) -> Self {
        let message = match field_errors.len() {
            1 => format!(
                "proposal payload is invalid: {} — {}",
                field_errors[0].field, field_errors[0].message
            ),
            n => format!("proposal payload is invalid: {n} field errors"),
        };
        Self {
            code: ProposalErrorCode::ValidationFailed,
            message,
            field_errors,
        }
    }
}

impl std::fmt::Display for ProposalSubmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for ProposalSubmissionError {}

/// Maximum proposals one execution may submit across all kinds.
///
/// Deliberately generous: per design §"Transport and authn", "the goal is
/// runaway-loop protection and attribution, not scarcity". A run that files
/// 32 proposals is looping, not working.
pub const PROPOSAL_CAP_TOTAL_PER_EXECUTION: usize = 32;

/// Maximum proposals of any single kind one execution may submit. Bounds a
/// loop that is stuck re-proposing one thing without consuming the whole
/// total budget, which would mask the pattern.
pub const PROPOSAL_CAP_PER_KIND_PER_EXECUTION: usize = 8;
