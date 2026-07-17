//! Attentions: the human-actionable question/followup groups, their
//! provenance ledger, and the per-work-item attention rows.

use super::common::EffortLevel;
use serde::{Deserialize, Serialize};

/// One member of an [`AttentionGroup`]. Id prefix `atn`.
///
/// Question groups carry the `question_type` / `prompt_text` /
/// `choice_options` / `answer` fields. Followup groups carry the
/// `proposed_*` / `rationale` fields. Both share `source_anchor`,
/// `answer_state`, and `confidence_source`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct Attention {
    pub id: String,
    pub group_id: String,
    /// Display order within the group (1-based).
    pub ordinal: i64,
    /// Doc section / heading slug (questions) or transcript offset hint.
    /// Drives inline placement in the design-doc viewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_anchor: Option<String>,
    /// `"open"` | `"answered"` | `"skipped"` | `"dismissed"`.
    #[builder(default = "open".to_string())]
    pub answer_state: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<String>,
    // --- question fields (populated when group.kind = "question") ---
    /// `"yes_no"` | `"multiple_choice"` | `"prompt"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_type: Option<String>,
    /// The question shown to the human.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_text: Option<String>,
    /// JSON array of strings (`multiple_choice` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice_options: Option<String>,
    /// Captured answer: `"yes"`/`"no"`, chosen index/value, or free text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    // --- followup fields (populated when group.kind = "followup") ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_description: Option<String>,
    /// Effort hint (`"trivial"` … `"max"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_effort: Option<String>,
    /// `"task"` | `"chore"` | `"project"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_work_kind: Option<String>,
    /// Why the agent suggested this followup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    /// `"structured"` (from a manifest/sentinel) or `"extracted"`
    /// (from a model pass over a transcript or doc).
    #[builder(default = "structured".to_string())]
    pub confidence_source: String,
    /// Count of independent reports folded into this item. `1` for a freshly
    /// created item; incremented each time a near-duplicate is reconciled into
    /// this canonical. Surfaced as a priority signal in the Notifications UI.
    #[builder(default = 1)]
    pub score: i64,
    /// Set to the covering work-item id (e.g. `"T42"`) on a Medium-confidence
    /// `WorkItemDup` verdict. The attention remains open but carries a UI
    /// cross-reference chip to the work item. `None` for items without a
    /// detected work-item duplicate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_work_item_id: Option<String>,
}

/// One attention group — the human-actionable unit of the Attentions
/// feature. Id prefix `atg`. Related attentions (questions or followups)
/// collect into a group keyed by a stable `grouping_key`; the group is
/// what the human reads and acts on, producing a single downstream
/// artifact.
///
/// Design: `tools/boss/docs/designs/attentions.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct AttentionGroup {
    pub id: String,
    /// Per-product `A<n>` friendly id. `None` until the engine assigns
    /// one at creation time. Partial-unique index enforces uniqueness
    /// per product when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,

    pub product_id: String,
    pub created_at: String,
    /// Bumped each time the same source re-runs after the prior group
    /// was actioned/dismissed, keeping "one group ⇒ one revision" true.
    #[builder(default = 0)]
    pub generation: i64,

    /// Stable derived key — the upsert dedup target for reconciliation.
    /// Shape: `"question|{project_id}|doc:{path}"` or
    /// `"followup|{task_id}"`.
    pub grouping_key: String,

    /// `"question"` or `"followup"`.
    pub kind: String,

    /// `"design_doc"` | `"task_transcript"` | `"manual"`.
    pub source_kind: String,

    /// `"open"` | `"partially_answered"` | `"actioned"` | `"dismissed"`.
    #[builder(default = "open".to_string())]
    pub state: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actioned_at: Option<String>,

    /// Exactly one of `association_project_id` / `association_task_id`
    /// is set — the XOR constraint mirrors `work_attention_items`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub association_project_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub association_task_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_at: Option<String>,

    /// Set when the group has been actioned: `"revision"` |
    /// `"design_task"` | `"tasks"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub produced_artifact_kind: Option<String>,

    /// JSON: revision task id / new task ids / PR url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub produced_artifact_ref: Option<String>,

    /// Head branch for in-review viewing of the source doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_doc_branch: Option<String>,

    /// Repo-relative design-doc path (populated for `design_doc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_doc_path: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_doc_repo_remote_url: Option<String>,

    /// Transcript pointer (`runs.id`); pairs with `runs.transcript_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_run_id: Option<String>,

    /// Originating design/impl task (jump-back target for the UI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_task_id: Option<String>,
}

/// One row of the `attention_merges` provenance ledger. Records every fold
/// event: which candidate was reconciled into which canonical (or suppressed
/// as a work-item dup / sensibility), the model that decided, the rationale,
/// and any bounded edits applied to the canonical. Id prefix `merge`.
///
/// Design: `tools/boss/docs/designs/notification-dedup-scoring.md` §2, §8.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct AttentionMerge {
    pub id: String,
    /// Rendered text of the candidate that was folded.
    pub candidate_summary: String,
    pub created_at: String,
    /// Model slug used for the dedup decision.
    pub model: String,
    pub product_id: String,
    /// `"creation"` | `"sweep"` | `"sensibility"`.
    pub trigger: String,
    /// Set for `AttentionDup` folds; `None` for `WorkItemDup` / sensibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_attention_id: Option<String>,
    /// Set for `WorkItemDup` folds; `None` for `AttentionDup` / sensibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_work_item_id: Option<String>,
    /// Source run / task id of the duplicate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_rationale: Option<String>,
    /// Set for sweep folds (retired loser row id); `None` for creation-time
    /// folds (the candidate was never persisted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_attention_id: Option<String>,
    /// JSON `[{field, before, after}]` or `None` when no edit was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits_applied: Option<String>,
}

/// One active or historical blocked-reason for a work item — the
/// wire shape of a `task_blocked_signals` row. The set of rows for
/// one `work_item_id` is the parent's multi-signal block state; the
/// scalar `Task::blocked_reason` is a denormalised "primary reason"
/// cache derived from this set per the design's §Q2 priority order.
///
/// `reason` is one of the documented signals (`'dependency'`,
/// `'merge_conflict'`, `'review_feedback'`, `'ci_failure'`,
/// `'ci_failure_exhausted'`); the engine treats the set as open so
/// new reasons can ship without bumping this type. `attempt_id` is a
/// soft FK into the attempt table for the matching reason
/// (`conflict_resolutions` for `'merge_conflict'`, `ci_remediations`
/// for the CI signals, etc.) and is `None` for `'dependency'` (the
/// prereqs are queried via `work_item_dependencies` instead).
///
/// `cleared_at` is `None` while the signal is active and is stamped
/// when the signal clears; rows are retained as history alongside
/// `conflict_resolutions` and `ci_remediations`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockedSignal {
    pub work_item_id: String,
    pub created_at: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared_at: Option<String>,
}

/// Input for creating a new attention (question or followup member).
/// The engine resolves or creates the appropriate group based on the
/// association and source fields; callers may pass an explicit
/// `group_id` to join an already-open group.
#[derive(bon::Builder, Debug, Clone, Default, Serialize, Deserialize)]
#[builder(on(String, into))]
pub struct CreateAttentionInput {
    /// `"question"` or `"followup"`.
    pub kind: String,
    /// Explicit group to join. When `None` the engine derives or creates
    /// the group from `(kind, association, source_*)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Caller-supplied grouping key override. Ignored when `group_id` is
    /// set; the engine computes the key from association + source when
    /// both are `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub association_project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub association_task_id: Option<String>,
    /// `"design_doc"` | `"task_transcript"` | `"manual"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_doc_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_doc_repo_remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_doc_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_anchor: Option<String>,
    // question content
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice_options: Option<String>,
    // followup content
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_work_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    /// `"structured"` or `"extracted"`. Defaults to `"structured"` when
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence_source: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CreateAttentionItemInput {
    pub body_markdown: String,
    pub kind: String,
    pub title: String,
    /// The execution this item attaches to. `Some` for the common
    /// execution-scoped case; `None` together with `work_item_id =
    /// Some(...)` for sticky pre-dispatch items like `repo_unresolved`.
    #[serde(default)]
    pub execution_id: Option<String>,

    pub resolved_at: Option<String>,
    pub status: Option<String>,
    /// The work item this item attaches to when no execution row
    /// exists. Mutually exclusive with `execution_id` — the engine
    /// rejects inputs where both are set or both are missing.
    #[serde(default)]
    pub work_item_id: Option<String>,
}

/// One recorded effort-level escalation event — the wire shape of
/// an `effort_escalations` row. Written by the coordinator's
/// escalation handler (design §Q5) when a worker raises an
/// `[effort-escalation]` Stop-boundary marker; read by the
/// heuristic feedback-loop audit report (`boss product
/// audit-effort`).
///
/// Carries the row's `original_level` (what the heuristic chose at
/// creation time), the `new_level` the worker requested, and the
/// list of `markers` the heuristic recorded as having matched the
/// row when it picked the original level. The audit report
/// aggregates these by marker to surface "marker X under-classified
/// Y% of the time" without changing the heuristic itself.
///
/// `markers` is the §Q4 marker corpus the heuristic uses; entries
/// are the literal marker strings ("rename", "investigate", etc.)
/// stored as a JSON array in SQLite. `rule_id` is optional and
/// names the §Q4 rule that fired (`"rule-2"`, `"rule-5"`, etc.) for
/// the heuristic's own bookkeeping; the audit report does not
/// depend on it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct EffortEscalation {
    pub id: String,
    pub product_id: String,
    pub work_item_id: String,
    pub created_at: String,
    pub markers: Vec<String>,
    pub new_level: EffortLevel,
    pub original_level: EffortLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct WorkAttentionItem {
    pub id: String,
    pub body_markdown: String,
    pub created_at: String,
    pub kind: String,
    pub status: String,
    pub title: String,
    /// The execution this item attaches to, when the failure has a
    /// concrete execution row (e.g. a worker run failed mid-flight).
    /// `None` when the item attaches to a work item directly because
    /// no execution row exists yet — the `repo_unresolved` flow per
    /// `multi-repo-work-modeling.md` Q5 is the load-bearing case.
    #[serde(default)]
    pub execution_id: Option<String>,

    pub resolved_at: Option<String>,
    /// The work item this item attaches to when there is no execution
    /// row (sticky, pre-dispatch failures). Mutually exclusive with
    /// `execution_id` — exactly one of the two is `Some`.
    #[serde(default)]
    pub work_item_id: Option<String>,
    /// Set when this item was closed via "create task" (currently only
    /// `deferred_scope` items support that closure path) — the id of the
    /// followup task the conversion produced. `None` for every item still
    /// open or closed by another path (e.g. accepted/resolved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub converted_task_id: Option<String>,
}

/// One open `deferred_scope` attention item, paired with the id of the
/// work item whose execution recorded it. `WorkAttentionItem` itself
/// carries only `execution_id` for this kind (never `work_item_id` — see
/// [`WorkAttentionItem::work_item_id`]), so callers that need to place the
/// item on a specific kanban card (rather than just an execution) need the
/// join already done. Produced by the engine's product-wide deferred-scope
/// listing (`WorkDb::list_open_deferred_scope_attentions_for_product`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeferredScopeAttention {
    pub item: WorkAttentionItem,
    pub source_work_item_id: String,
}
