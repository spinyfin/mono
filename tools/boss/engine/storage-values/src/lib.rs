//! Owned values and pure shared policy needed by engine storage and its services.
//! Database mapping, service orchestration, and I/O remain in their service owners.

use serde::{Deserialize, Serialize};

pub mod branch_identity;
pub mod ci_contract;
pub mod pool_policy;
pub mod postmortem_brief;

mod semantic_progress;
pub use semantic_progress::{SemanticProgressCheckpoint, SemanticToolCondition, SemanticTurnCondition};

/// One chore as the audit sees it: just enough to run the marker
/// matcher. The engine layer projects rows out of the `tasks` table
/// into this shape; tests construct it directly.
#[derive(Debug, Clone)]
pub struct ChoreForAudit {
    pub name: String,
    pub description: String,
}

/// A single append-only row in the `project_property_audit` table.
/// Records that `actor` changed `property` on `project_id` from
/// `old_value` to `new_value` at `changed_at` (epoch seconds, the
/// same format as `projects.updated_at`).
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ProjectPropertyAuditEntry {
    pub id: String,
    pub project_id: String,
    pub property: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub actor: String,
    pub changed_at: String,
    /// Human-readable basis for the transition. Present for `status`
    /// entries; `None` for older rows and design-doc property edits.
    pub basis: Option<String>,
}

/// One `work_runs` row, projected with just enough context to group and
/// rank it. The `work` DB layer's production mapper builds these with a
/// struct literal (every column named explicitly, per this repo's DB
/// mapper convention); tests may use the builder.
#[derive(Debug, Clone, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct CostRunRecord {
    pub run_id: String,
    pub execution_id: String,
    pub work_item_id: String,
    pub work_item_name: String,
    pub execution_kind: String,
    pub execution_status: String,
    pub run_status: String,
    pub created_at_epoch_s: i64,
    pub work_item_short_id: Option<i64>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub effort_level: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub agent_active_ms: Option<i64>,
    pub started_at_epoch_s: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct RunCostSnapshot {
    /// Assign every persisted cost field verbatim, including NULL. Set when a
    /// transcript generation is replaced so values absent from the replay do
    /// not survive from the obsolete generation.
    #[builder(default)]
    pub full_replacement: bool,
    pub model: Option<String>,
    pub output_tokens: Option<i64>,
    pub input_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_5m_tokens: Option<i64>,
    pub cache_creation_1h_tokens: Option<i64>,
    /// `None`: no usage observation in this snapshot. `Some(false)`: usage
    /// was observed but at least one non-zero cache write lacked a TTL split,
    /// so persisted split columns must be explicitly cleared to NULL.
    pub cache_creation_ttl_split_known: Option<bool>,
    pub rounds: Option<i64>,
    pub agent_active_ms: Option<i64>,
    /// Provider usage objects, keyed by provider and transcript/message identity.
    /// Preserve missing fields and provider-specific categories verbatim.
    pub provider_usage_json: Option<String>,
}

/// Per-execution outcome of the startup probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunReconcileVerdict {
    /// Worker is presumed still attached; dispatch should not
    /// redispatch the work item.
    Live,
    /// Worker is presumed gone; the execution row is stale and the
    /// work item should be redispatched after the row is marked
    /// abandoned by the existing reconcile transaction.
    Dead,
    /// Insufficient signal to decide. Conservatively treated as Live
    /// at dispatch time so we don't risk a duplicate, but surfaced via
    /// `tracing::warn!` so an operator can resolve manually.
    Unknown,
}

/// The stored handoff record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorHandoff {
    /// Distinguishes two writes even when their timestamp and body are equal.
    #[serde(default)]
    pub write_id: String,
    pub body: String,
    /// Unix epoch seconds.
    pub written_at: i64,
    /// Spawn token of the coordinator record that was live at write time.
    /// Empty when no coordinator record existed (a handoff written from a
    /// plain terminal before any coordinator ever ran).
    pub writer_spawn_token: String,
}

/// What the engine found when it looked for a handoff. `Missing` and
/// `Unreadable` are deliberately separate variants: "no session ever wrote
/// one" and "one was written but the engine cannot read it back" call for
/// different operator responses, and collapsing them would hide the
/// second behind the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffState {
    Present(CoordinatorHandoff),
    Missing,
    Unreadable(String),
}

impl HandoffState {
    /// Short label for the engine-audit log.
    pub fn audit_outcome(&self) -> &'static str {
        match self {
            Self::Present(_) => "present",
            Self::Missing => "missing",
            Self::Unreadable(_) => "unreadable",
        }
    }
}

#[cfg(test)]
mod tests;
