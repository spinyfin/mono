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
/// - `rejected`: a policy/human judgment declined the proposal (e.g. dedup
///   verdict "already exists as T123"); `decision_reason` carries why.
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

/// One row in the `worker_proposals` ingress ledger — the durable record of
/// a `boss propose <kind>` submission. `payload_json` is the JSON encoding
/// of the kind-matched payload struct below (e.g. `kind = FollowupTask` ⇒
/// `payload_json` deserializes as [`FollowupTaskProposalPayload`]); this
/// type does not enforce that correspondence itself; the submission RPC
/// (task 2) validates it at write time.
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

    /// Who/what decided this proposal's disposition: `policy` (an
    /// auto-apply appliers), `coordinator`, or `human`. `None` while still
    /// `proposed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,

    /// Human-readable reason for a `rejected` (or otherwise notable)
    /// disposition, e.g. `"duplicate of T123"`. `None` for proposals with
    /// no decision yet, and for most `applied` rows.
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
