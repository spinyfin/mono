//! Planner runs — the `planner_runs` audit ledger and its outcome
//! vocabulary — plus the effort-audit reporting types.

use super::common::EffortLevel;
use serde::{Deserialize, Serialize};

/// Suggested action a human reviewer might take, encoded so JSON
/// consumers can branch on it without parsing free text. Mirrors
/// the annotation strings in [`EffortAuditMarkerRow::annotation`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffortAuditAnnotation {
    /// Rate exceeds the configured under-classification threshold:
    /// the marker maps the row to a level workers commonly judge
    /// too low. Surface as "consider promoting to <higher level>."
    ConsiderPromoting,
    /// Rate is below the well-classified ceiling AND match volume
    /// is above the well-classified floor: the marker is doing its
    /// job. Surface as "marker holds; level correct."
    MarkerHolds,
    /// Either threshold-eligible but on the over-class side, or
    /// volume too low to call. No callout.
    None,
}

/// Per-marker analysis row in the effort-audit report. One entry
/// per marker in the §Q4 corpus that matched at least one chore in
/// the product (markers with zero matches are filtered out so the
/// table stays scannable).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct EffortAuditMarkerRow {
    /// Of those, the count that subsequently raised an
    /// `[effort-escalation]` marker promoting the row to a higher
    /// level (per [`EffortLevel`]'s natural ordering trivial < small
    /// < medium < large < max).
    pub escalations: u32,

    /// Marker string from the §Q4 corpus, e.g. `"rename"`,
    /// `"investigate"`, `"engine"`, lowercased.
    pub marker: String,

    /// Total chores (kind = `chore`) on the product whose title or
    /// description matched this marker, regardless of whether they
    /// escalated.
    pub matches: u32,

    /// Heuristic level the marker maps to per §Q4 (`trivial` for
    /// mechanical-edit markers, `medium` for multi-subsystem hints,
    /// `large` for investigate-family markers).
    pub original_level: EffortLevel,

    /// Human-readable callout produced when the rate / volume cross
    /// the thresholds named in `engine/src/effort.rs`. Empty when
    /// the marker is neither "consider promoting" nor "marker
    /// holds."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<String>,

    /// `escalations / matches` as a 0.0-1.0 fraction. `0.0` when
    /// `matches > 0 && escalations == 0`; absent (per
    /// [`Option`]'s `None`) when `matches == 0` so callers don't
    /// have to special-case divide-by-zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub under_class_rate: Option<f64>,
}

/// Output shape for `boss product audit-effort <product>`. One
/// snapshot of the marker corpus's under-classification rates
/// against the recorded escalation events for a single product.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct EffortAuditReport {
    pub product_id: String,
    /// Epoch seconds when the audit was generated, for the
    /// human-readable header.
    pub generated_at: String,

    pub product_slug: String,
    /// Per-marker analysis, sorted by `under_class_rate`
    /// descending so the noisy markers are visible first. Markers
    /// with zero matches are filtered out.
    pub rows: Vec<EffortAuditMarkerRow>,

    /// Total chores (kind = `chore`, `deleted_at IS NULL`) on the
    /// product that the audit scanned for marker matches.
    pub total_chores: u32,

    /// Total escalation events the audit considered (after window
    /// filter). Equal to the sum of per-marker `escalations` only
    /// when every event carried exactly one marker — events can
    /// match multiple markers and double-count by design.
    pub total_escalations: u32,

    /// Under-classification threshold (0.0-1.0) at which the audit
    /// produces a "consider promoting" callout. Echoed back so
    /// JSON consumers don't have to re-import the constant.
    pub under_class_threshold: f64,

    /// Window cap in days applied to escalation events
    /// (`created_at` after now - window). `None` means "no window;
    /// include all recorded escalations."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_days: Option<u32>,
}

// ---- planner_runs outcome constants ----
// Used by the Populator, WorkDb accessors, and any caller that needs to
// interpret a `PlannerRun.outcome` value without hard-coding string literals.

/// Planner run has been claimed; the Populator is executing.
pub const PLANNER_OUTCOME_RUNNING: &str = "running";
/// Planner run succeeded and tasks were created with `autostart = false`
/// (staged, awaiting operator release).
pub const PLANNER_OUTCOME_STAGED: &str = "staged";
/// Tasks were created and released (dispatching). Reserved for future use
/// when the auto-dispatch path is enabled.
pub const PLANNER_OUTCOME_APPLIED: &str = "applied";
/// Design doc had no task-breakdown section; clean no-op.
pub const PLANNER_OUTCOME_NO_BREAKDOWN: &str = "no_breakdown";
/// Proposal exceeded the `max_tasks` cap; rejected whole.
pub const PLANNER_OUTCOME_REJECTED_TOO_MANY: &str = "rejected_too_many";
/// Proposal contained a dependency cycle; rejected whole.
pub const PLANNER_OUTCOME_REJECTED_CYCLE: &str = "rejected_cycle";
/// Design doc fetch failed after bounded retries.
pub const PLANNER_OUTCOME_FETCH_FAILED: &str = "fetch_failed";
/// Design doc returned 404 (moved or deleted since merge).
pub const PLANNER_OUTCOME_DOC_MISSING: &str = "doc_missing";
/// LLM call failed or returned invalid output after retries.
pub const PLANNER_OUTCOME_PLANNER_FAILED: &str = "planner_failed";
/// Project already has implementation tasks; populate skipped to avoid
/// merging with pre-seeded work.
pub const PLANNER_OUTCOME_SKIPPED_PRE_SEEDED: &str = "skipped_pre_seeded";
/// A prior `running`/`staged`/`applied` row already exists for this
/// project; this invocation is a duplicate and was skipped.
pub const PLANNER_OUTCOME_SKIPPED_ALREADY_POPULATED: &str = "skipped_already_populated";

/// One row in the `planner_runs` audit ledger.
///
/// Every Planner invocation writes exactly one row (inserted as
/// `outcome = 'running'` on claim, then updated to a terminal outcome on
/// completion). The UNIQUE partial index `planner_runs_one_per_project`
/// enforces at most one live row (`outcome IN ('running','staged','applied')`)
/// per `project_id`, making this table the per-project idempotency gate.
///
/// Design: `tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md`
/// §"Durable audit trail".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct PlannerRun {
    pub id: String,
    pub project_id: String,
    pub product_id: String,
    /// The `kind = 'design'` task whose PR merge triggered this run.
    /// `None` for operator-initiated runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_task_id: Option<String>,
    /// What initiated this run: `"merge_trigger"` | `"operator"` | `"replan"`.
    pub caller: String,
    /// `"<repo_remote_url>|<ref>|<path>"` of the doc fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_ref: Option<String>,
    /// Model slug used for the Planner call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Short summary of the Planner's input (doc length, project name,
    /// existing-task count). For the operator audit view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_summary: Option<String>,
    /// Verbatim structured JSON returned by the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<String>,
    /// The `[effort-classification]` lines emitted per proposed task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_audit: Option<String>,
    /// Free-text rationale from the Planner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// One of the `PLANNER_OUTCOME_*` constants.
    pub outcome: String,
    /// Human-readable summary of what was created/skipped/rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
