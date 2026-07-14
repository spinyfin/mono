//! Planner contract types shared between `boss-engine` callers and tests.
//!
//! The Planner is a reusable LLM mini-coordinator:
//! `Planner::plan(PlannerInput) -> Result<PlannerOutput>`. These types define
//! the typed contract so every caller speaks the same shape.
//!
//! See `tools/boss/docs/designs/auto-populate-project-tasks-on-design-pr-merge.md`
//! for the full design.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{EffortLevel, TaskKind};

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

/// Provenance record for the design doc fetched by the Planner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocRef {
    /// Canonical remote URL of the repository the doc lives in.
    pub repo_remote_url: String,
    /// The branch name or commit SHA at which the doc was fetched.
    pub git_ref: String,
    /// Repo-relative path to the design doc, e.g. `tools/boss/docs/designs/foo.md`.
    pub path: String,
}

/// Slim project view supplied to the Planner as context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectContext {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub goal: String,
}

/// Slim product view supplied to the Planner as context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductContext {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub repo_remote_url: String,
}

/// Minimal task record passed to the Planner so it can avoid proposing
/// tasks whose names duplicate ones already in the project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskBrief {
    pub id: String,
    pub name: String,
}

/// All inputs the Planner needs to produce a task-graph proposal.
#[derive(bon::Builder, Debug, Clone, Serialize, Deserialize)]
#[builder(on(String, into))]
pub struct PlannerInput {
    /// Full text of the merged design doc fetched live from GitHub.
    pub design_doc: String,
    /// Provenance record so the audit trail can reproduce the fetch.
    pub design_doc_ref: DocRef,
    /// Project the tasks will be created in.
    pub project: ProjectContext,
    /// Product the project belongs to.
    pub product: ProductContext,
    /// Tasks already in the project — a dedup hint for the Planner.
    pub existing_tasks: Vec<TaskBrief>,
    /// Hard cap surfaced to the model; proposals exceeding it are rejected.
    pub max_tasks: usize,
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

/// The Planner's confidence in the task-graph it extracted.
///
/// `Low` does not block materialization — tasks are always staged with
/// `autostart = false` and require operator release regardless — but it
/// escalates the attention-item prominence so the operator knows to
/// scrutinize the plan before releasing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single task proposed by the Planner.
///
/// `handle` is a proposal-local identifier (e.g. `"schema-migration"`) used
/// to reference this task in [`ProposedEdge`] dependency declarations.  The
/// Materializer resolves handles to real task ids at apply time.
#[derive(bon::Builder, Debug, Clone, Serialize, Deserialize)]
#[builder(on(String, into))]
pub struct ProposedTask {
    /// Proposal-local identifier; referenced by [`ProposedEdge`].
    pub handle: String,
    pub name: String,
    pub description: String,
    /// `project_task` (default) or `investigation`.  The Planner never
    /// emits `design`, `chore`, `revision`, or `task`.
    pub kind: TaskKind,
    /// Effort estimate; the Planner never emits `max` (human-only).
    pub effort: EffortLevel,
    /// Soft ordering hint — not a hard dependency gate (edges are).
    pub ordinal: i64,
}

/// A directed dependency edge between two proposed tasks, expressed by handle.
///
/// Semantics: `prerequisite` must land before `dependent` can start.
/// Mirrors the `blocks` relation in `add_dependency`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedEdge {
    /// Handle of the task that is gated (must wait for the prerequisite).
    pub dependent: String,
    /// Handle of the task that gates it.
    pub prerequisite: String,
}

/// A soft file-overlap hint between two otherwise-parallel proposed tasks,
/// expressed by handle.
///
/// This is deliberately NOT a [`ProposedEdge`]: it never gates dispatch — both
/// `task_a` and `task_b` remain independently startable, exactly like the
/// design's "merge_order is non-blocking by construction" (see the
/// merge-conflict-reduction design, "Composition with T2253's P5-lite"). It
/// exists only so a later merge-time consumer can order the two PRs and stamp
/// the later one with a preservation obligation. The Planner emits this only
/// when two tasks it has otherwise declared parallel are clearly and
/// substantially likely to co-edit the same file(s) — never for incidental
/// overlap.
#[derive(bon::Builder, Debug, Clone, Serialize, Deserialize)]
#[builder(on(String, into))]
pub struct ProposedMergeOrderHint {
    /// Handle of one of the two overlapping tasks.
    pub task_a: String,
    /// Handle of the other overlapping task.
    pub task_b: String,
    /// Free-text rationale: which file(s)/surface the two tasks are expected
    /// to co-edit.
    pub reason: String,
}

/// The Planner's structured output — a validated, typed task-graph proposal.
///
/// This is the shape the engine deserialises directly from the Anthropic
/// structured-output call.  A JSON Schema for this type is exported by
/// [`planner_output_schema`] for use as the forced tool-call `input_schema`.
#[derive(bon::Builder, Debug, Clone, Serialize, Deserialize)]
#[builder(on(String, into))]
pub struct PlannerOutput {
    pub tasks: Vec<ProposedTask>,
    /// Dependency edges between tasks, referenced by handle.
    pub edges: Vec<ProposedEdge>,
    /// Soft file-overlap hints between otherwise-parallel tasks, referenced
    /// by handle. Never gates dispatch — see [`ProposedMergeOrderHint`].
    /// `#[serde(default)]` so a proposal from before this field existed still
    /// deserialises.
    #[serde(default)]
    #[builder(default)]
    pub merge_order_hints: Vec<ProposedMergeOrderHint>,
    pub confidence: Confidence,
    /// `false` when the design doc contained no task-breakdown section at
    /// all — a clean no-op signal, distinct from "found a breakdown but it
    /// was empty".
    pub breakdown_found: bool,
    /// Free-text rationale from the Planner, persisted in `planner_runs` for
    /// the operator to inspect after the fact.
    pub notes: String,
    /// One `[effort-classification] …` line per proposed task, in the same
    /// format the coordinator and engine emit today.
    pub effort_audit: Vec<String>,
}

// ---------------------------------------------------------------------------
// Apply result
// ---------------------------------------------------------------------------

/// Result returned by `Materializer::apply` after a successful (or partially
/// deduped) application of a [`PlannerOutput`] proposal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyResult {
    /// IDs of tasks created in this run (already existed rows are in `skipped`).
    pub created: Vec<String>,
    /// Names of tasks that were skipped because a non-deleted task with that
    /// name already existed in the project.
    pub skipped: Vec<String>,
    /// Number of `blocks` dependency edges inserted.
    pub edges_created: usize,
    /// Number of non-blocking `merge_order` edges inserted from the proposal's
    /// `merge_order_hints` (soft file-overlap pairings; never gate dispatch).
    /// `#[serde(default)]` so an [`ApplyResult`] serialized before this field
    /// existed still deserialises.
    #[serde(default)]
    pub merge_order_edges_created: usize,
}

// ---------------------------------------------------------------------------
// JSON Schema for structured-output enforcement
// ---------------------------------------------------------------------------

/// Returns the JSON Schema used as the `input_schema` of the Anthropic forced
/// tool-call that constrains the Planner's response to [`PlannerOutput`].
///
/// The schema is intentionally conservative: it marks every field `required`
/// and enumerates the legal values for all enum fields, so a deserialization
/// failure means the model returned something outside the contract rather than
/// a missing optional.
pub fn planner_output_schema() -> Value {
    json!({
        "type": "object",
        "required": [
            "tasks",
            "edges",
            "merge_order_hints",
            "confidence",
            "breakdown_found",
            "notes",
            "effort_audit"
        ],
        "additionalProperties": false,
        "properties": {
            "tasks": {
                "type": "array",
                "description": "Proposed implementation tasks extracted from the design doc.",
                "items": {
                    "type": "object",
                    "required": ["handle", "name", "description", "kind", "effort", "ordinal"],
                    "additionalProperties": false,
                    "properties": {
                        "handle": {
                            "type": "string",
                            "description": "Proposal-local identifier for this task, referenced in edges."
                        },
                        "name": {
                            "type": "string",
                            "description": "Short task name as it will appear in Boss."
                        },
                        "description": {
                            "type": "string",
                            "description": "Full task description, including the [effort-classification] audit line."
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["project_task", "investigation"],
                            "description": "Task kind. Use project_task by default; investigation for research/audit/diagnose tasks."
                        },
                        "effort": {
                            "type": "string",
                            "enum": ["trivial", "small", "medium", "large"],
                            "description": "Effort estimate. Never use max (human-only)."
                        },
                        "ordinal": {
                            "type": "integer",
                            "description": "Soft ordering hint; not a hard dependency gate."
                        }
                    }
                }
            },
            "edges": {
                "type": "array",
                "description": "Dependency edges between proposed tasks, by handle.",
                "items": {
                    "type": "object",
                    "required": ["dependent", "prerequisite"],
                    "additionalProperties": false,
                    "properties": {
                        "dependent": {
                            "type": "string",
                            "description": "Handle of the task that is gated (must wait)."
                        },
                        "prerequisite": {
                            "type": "string",
                            "description": "Handle of the task that gates it (must land first)."
                        }
                    }
                }
            },
            "merge_order_hints": {
                "type": "array",
                "description": "Soft file-overlap hints between otherwise-parallel tasks, by handle. Never gates dispatch — use a `blocks` edge for true prerequisites instead.",
                "items": {
                    "type": "object",
                    "required": ["task_a", "task_b", "reason"],
                    "additionalProperties": false,
                    "properties": {
                        "task_a": {
                            "type": "string",
                            "description": "Handle of one of the two overlapping tasks."
                        },
                        "task_b": {
                            "type": "string",
                            "description": "Handle of the other overlapping task."
                        },
                        "reason": {
                            "type": "string",
                            "description": "Which file(s)/surface the two tasks are expected to co-edit."
                        }
                    }
                }
            },
            "confidence": {
                "type": "string",
                "enum": ["high", "medium", "low"],
                "description": "Planner's confidence in the quality of this proposal."
            },
            "breakdown_found": {
                "type": "boolean",
                "description": "true if the doc contained a task-breakdown section; false for a clean no-op."
            },
            "notes": {
                "type": "string",
                "description": "Free-text rationale persisted in planner_runs for operator review."
            },
            "effort_audit": {
                "type": "array",
                "description": "One [effort-classification] line per proposed task.",
                "items": {
                    "type": "string"
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_output_schema_is_valid_json() {
        let schema = planner_output_schema();
        // Must serialise without panic and contain required keys.
        let s = serde_json::to_string(&schema).expect("schema serialises");
        assert!(s.contains("\"tasks\""));
        assert!(s.contains("\"edges\""));
        assert!(s.contains("\"confidence\""));
        assert!(s.contains("\"breakdown_found\""));
    }

    #[test]
    fn planner_output_round_trips() {
        let output = PlannerOutput {
            tasks: vec![ProposedTask {
                handle: "schema".into(),
                name: "Add schema".into(),
                description: "Add the schema types.".into(),
                kind: TaskKind::ProjectTask,
                effort: EffortLevel::Small,
                ordinal: 1,
            }],
            edges: vec![],
            merge_order_hints: vec![],
            confidence: Confidence::High,
            breakdown_found: true,
            notes: "Clear breakdown found.".into(),
            effort_audit: vec![
                "[effort-classification] level=`small` matched-rule=`rule 1` reasons=\"protocol types\"".into(),
            ],
        };

        let json = serde_json::to_string(&output).expect("serialises");
        let back: PlannerOutput = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back.tasks.len(), 1);
        assert_eq!(back.tasks[0].handle, "schema");
        assert_eq!(back.confidence, Confidence::High);
        assert!(back.breakdown_found);
    }

    #[test]
    fn apply_result_round_trips() {
        let result = ApplyResult {
            created: vec!["task_abc".into(), "task_def".into()],
            skipped: vec!["Existing task".into()],
            edges_created: 1,
            merge_order_edges_created: 2,
        };

        let json = serde_json::to_string(&result).expect("serialises");
        let back: ApplyResult = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back.created.len(), 2);
        assert_eq!(back.skipped.len(), 1);
        assert_eq!(back.edges_created, 1);
        assert_eq!(back.merge_order_edges_created, 2);
    }

    /// An `ApplyResult` JSON payload written before `merge_order_edges_created`
    /// existed must still deserialise (the field defaults to 0).
    #[test]
    fn apply_result_deserialises_without_merge_order_field() {
        let legacy = r#"{"created":["task_a"],"skipped":[],"edges_created":3}"#;
        let back: ApplyResult = serde_json::from_str(legacy).expect("legacy payload deserialises");
        assert_eq!(back.edges_created, 3);
        assert_eq!(back.merge_order_edges_created, 0);
    }

    #[test]
    fn confidence_display() {
        assert_eq!(Confidence::High.as_str(), "high");
        assert_eq!(Confidence::Medium.as_str(), "medium");
        assert_eq!(Confidence::Low.as_str(), "low");
        assert_eq!(Confidence::High.to_string(), "high");
    }

    // -----------------------------------------------------------------------
    // Schema-as-contract tests
    //
    // These pin the *observable contract* of `planner_output_schema()` against
    // the types it is meant to enforce, so the hand-maintained JSON Schema
    // cannot silently drift from `PlannerOutput` / `ProposedTask` /
    // `ProposedEdge` (or from the `Confidence` / `TaskKind` / `EffortLevel`
    // enums) and cause runtime deserialization failures. They deliberately
    // derive their expectations from serde and the enum types — not from the
    // `json!` literal — so a change to either side that the other doesn't
    // mirror trips a failure.
    // -----------------------------------------------------------------------

    /// The serde field names of a value, as actually emitted by `Serialize`.
    fn serde_field_names<T: Serialize>(value: &T) -> Vec<String> {
        let mut keys: Vec<String> = serde_json::to_value(value)
            .expect("serialises")
            .as_object()
            .expect("value serialises to a JSON object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// Pull a JSON array of strings out of the schema at `pointer`.
    fn string_array(schema: &Value, pointer: &str) -> Vec<String> {
        schema
            .pointer(pointer)
            .unwrap_or_else(|| panic!("schema has no value at {pointer}"))
            .as_array()
            .unwrap_or_else(|| panic!("schema value at {pointer} is not an array"))
            .iter()
            .map(|v| {
                v.as_str()
                    .unwrap_or_else(|| panic!("entry at {pointer} is not a string"))
                    .to_string()
            })
            .collect()
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    /// A fully-populated sample used to derive the serde field names of every
    /// output type (one task, one edge) without restating them by hand.
    fn sample_output() -> PlannerOutput {
        PlannerOutput {
            tasks: vec![ProposedTask {
                handle: "schema".into(),
                name: "Add schema".into(),
                description: "Add the schema types.".into(),
                kind: TaskKind::ProjectTask,
                effort: EffortLevel::Small,
                ordinal: 1,
            }],
            edges: vec![ProposedEdge {
                dependent: "impl".into(),
                prerequisite: "schema".into(),
            }],
            merge_order_hints: vec![ProposedMergeOrderHint {
                task_a: "schema".into(),
                task_b: "impl".into(),
                reason: "both touch the shared config module".into(),
            }],
            confidence: Confidence::High,
            breakdown_found: true,
            notes: "Clear breakdown found.".into(),
            effort_audit: vec!["[effort-classification] level=`small`".into()],
        }
    }

    #[test]
    fn schema_top_level_required_matches_planner_output_fields() {
        let schema = planner_output_schema();
        let required = sorted(string_array(&schema, "/required"));
        let fields = serde_field_names(&sample_output());
        assert_eq!(
            required, fields,
            "top-level `required` must list exactly PlannerOutput's serde fields"
        );
    }

    #[test]
    fn schema_task_items_required_matches_proposed_task_fields() {
        let schema = planner_output_schema();
        let sample = sample_output();
        let required = sorted(string_array(&schema, "/properties/tasks/items/required"));
        let fields = serde_field_names(&sample.tasks[0]);
        assert_eq!(
            required, fields,
            "tasks[].items `required` must list exactly ProposedTask's serde fields"
        );
    }

    #[test]
    fn schema_edge_items_required_matches_proposed_edge_fields() {
        let schema = planner_output_schema();
        let sample = sample_output();
        let required = sorted(string_array(&schema, "/properties/edges/items/required"));
        let fields = serde_field_names(&sample.edges[0]);
        assert_eq!(
            required, fields,
            "edges[].items `required` must list exactly ProposedEdge's serde fields"
        );
    }

    #[test]
    fn schema_merge_order_hint_items_required_matches_field_names() {
        let schema = planner_output_schema();
        let sample = sample_output();
        let required = sorted(string_array(&schema, "/properties/merge_order_hints/items/required"));
        let fields = serde_field_names(&sample.merge_order_hints[0]);
        assert_eq!(
            required, fields,
            "merge_order_hints[].items `required` must list exactly ProposedMergeOrderHint's serde fields"
        );
    }

    #[test]
    fn merge_order_hints_defaults_to_empty_when_absent() {
        // Older/hand-written documents without this field must still
        // deserialise (`#[serde(default)]`), so a proposal predating this
        // field is not rejected outright.
        let doc = json!({
            "tasks": [],
            "edges": [],
            "confidence": "high",
            "breakdown_found": false,
            "notes": "",
            "effort_audit": []
        });
        let output: PlannerOutput = serde_json::from_value(doc).expect("missing merge_order_hints defaults to []");
        assert!(output.merge_order_hints.is_empty());
    }

    #[test]
    fn schema_confidence_enum_matches_confidence_variants() {
        let schema = planner_output_schema();
        let enum_vals = string_array(&schema, "/properties/confidence/enum");
        let expected: Vec<String> = [Confidence::High, Confidence::Medium, Confidence::Low]
            .iter()
            .map(|c| c.as_str().to_string())
            .collect();
        assert_eq!(
            enum_vals, expected,
            "confidence enum must equal the Confidence variants' as_str() values"
        );
        // Every listed value must round-trip through serde into a Confidence.
        for v in &enum_vals {
            let c: Confidence = serde_json::from_value(Value::String(v.clone()))
                .unwrap_or_else(|_| panic!("{v} is not a valid Confidence"));
            assert_eq!(c.as_str(), v);
        }
    }

    #[test]
    fn schema_kind_enum_is_the_planner_subset_of_task_kind() {
        let schema = planner_output_schema();
        let kinds = string_array(&schema, "/properties/tasks/items/properties/kind/enum");
        assert_eq!(
            kinds,
            vec!["project_task".to_string(), "investigation".to_string()],
            "kind enum must be the Planner's project_task/investigation subset"
        );
        // Each listed value is a genuine TaskKind, and matches the intended variant.
        let parsed: Vec<TaskKind> = kinds
            .iter()
            .map(|v| {
                serde_json::from_value(Value::String(v.clone()))
                    .unwrap_or_else(|_| panic!("{v} is not a valid TaskKind"))
            })
            .collect();
        assert_eq!(parsed, vec![TaskKind::ProjectTask, TaskKind::Investigation]);
    }

    #[test]
    fn schema_effort_enum_matches_effort_level_minus_max() {
        let schema = planner_output_schema();
        let efforts = string_array(&schema, "/properties/tasks/items/properties/effort/enum");
        assert_eq!(
            efforts,
            vec![
                "trivial".to_string(),
                "small".to_string(),
                "medium".to_string(),
                "large".to_string()
            ],
            "effort enum must be the non-max EffortLevel values"
        );
        let parsed: Vec<EffortLevel> = efforts
            .iter()
            .map(|v| {
                serde_json::from_value(Value::String(v.clone()))
                    .unwrap_or_else(|_| panic!("{v} is not a valid EffortLevel"))
            })
            .collect();
        assert_eq!(
            parsed,
            vec![
                EffortLevel::Trivial,
                EffortLevel::Small,
                EffortLevel::Medium,
                EffortLevel::Large
            ]
        );
        // `max` is a real EffortLevel but deliberately excluded (human-only).
        assert!(
            !efforts.contains(&"max".to_string()),
            "effort enum must not offer the human-only `max`"
        );
        assert_eq!(EffortLevel::Max.as_str(), "max");
    }

    #[test]
    fn schema_conformant_document_deserializes_into_planner_output() {
        // A document that satisfies every `required` the schema lists, using
        // legal enum values, must deserialize into PlannerOutput — proving the
        // schema and the deserialization target agree.
        let doc = json!({
            "tasks": [{
                "handle": "schema",
                "name": "Add schema",
                "description": "Add the schema types.",
                "kind": "project_task",
                "effort": "small",
                "ordinal": 1
            }],
            "edges": [{ "dependent": "impl", "prerequisite": "schema" }],
            "merge_order_hints": [{ "task_a": "schema", "task_b": "impl", "reason": "shared config module" }],
            "confidence": "high",
            "breakdown_found": true,
            "notes": "Clear breakdown.",
            "effort_audit": ["[effort-classification] level=`small`"]
        });
        let output: PlannerOutput = serde_json::from_value(doc).expect("schema-conformant document deserializes");
        assert_eq!(output.tasks[0].kind, TaskKind::ProjectTask);
        assert_eq!(output.tasks[0].effort, EffortLevel::Small);
        assert_eq!(output.edges[0].prerequisite, "schema");
        assert_eq!(output.merge_order_hints[0].task_a, "schema");
        assert_eq!(output.confidence, Confidence::High);
    }

    #[test]
    fn document_missing_a_required_field_fails_to_deserialize() {
        // Same document minus `confidence` (a schema-required field) must not
        // deserialize — pinning that the required list is load-bearing.
        let doc = json!({
            "tasks": [],
            "edges": [],
            "breakdown_found": false,
            "notes": "",
            "effort_audit": []
        });
        let result: Result<PlannerOutput, _> = serde_json::from_value(doc);
        assert!(
            result.is_err(),
            "a document missing the required `confidence` field must not deserialize"
        );
    }
}
