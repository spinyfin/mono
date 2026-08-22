//! Notification dedup-decision contract types shared between the
//! `boss-engine` dedup caller, the LLM structured-output call, and tests.
//!
//! These types are the typed contract for the single LLM step described in
//! `tools/boss/docs/designs/notification-dedup-scoring.md` §3 "The dedup
//! decision (the one LLM step)". The engine renders a [`DedupInput`] into a
//! prompt, forces a tool call whose `input_schema` is [`dedup_decision_schema`],
//! and deserialises the model's tool input directly into a [`DedupDecision`].
//! Deserialising straight into the Rust type (rather than parsing free-form
//! prose) is what makes a malformed reply a clean *fail-safe to Keep* rather
//! than a parse-and-hope.
//!
//! The decision is a *pure transform*: `(candidate attention, comparison set
//! of open attentions + non-terminal work items) -> keep? fold into which
//! canonical? covered by which work item? stale/not-actionable?`. It performs
//! no writes; the engine owns every side effect.
//!
//! [`Confidence`](crate::Confidence) is reused from the Planner contract — the
//! High/Medium/Low tiers mean the same thing here (see design R1/R2 for how
//! confidence gates a fold vs. a Keep at the call sites).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Confidence;

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

/// A compact rendering of one `Attention` item for the dedup LLM.
///
/// The candidate and every member of the comparison set are rendered into this
/// shape so the model reasons over prose, not raw keys. `association` and
/// `group_id` are *context only* — the design forbids using them to partition
/// or filter the candidate set (the primary target case is different tasks, in
/// different cards, raising the same concern). See design §1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionBrief {
    /// The canonical item id, or the [`CANDIDATE_ID`] sentinel for the
    /// candidate being evaluated. `AttentionDup` verdicts must name an id
    /// drawn from the *existing* set, never the sentinel.
    pub attention_id: String,
    /// Which `AttentionGroup` (card) this item belongs to — context only.
    pub group_id: String,
    /// `"question"` | `"followup"` — the item kind, for context.
    pub kind: String,
    /// Project/task label, surfaced to the model for scoping context only —
    /// never used to filter or partition the comparison set.
    pub association: String,
    /// The item text rendered to prose (prompt / proposed name + description /
    /// rationale). This is what the model actually compares.
    pub rendered: String,
}

/// The sentinel [`AttentionBrief::attention_id`] used for the candidate item
/// in [`DedupInput::candidate`]. A verdict that names this id as a canonical
/// target is invalid (a candidate cannot be a duplicate of itself) and the
/// engine treats it as `Keep`.
pub const CANDIDATE_ID: &str = "candidate";

/// A compact rendering of one non-terminal work item (task, chore, revision,
/// project) for taxonomy-aware dedup. Populated only when the
/// `notification_dedup_taxonomy` sub-flag is on (design §4). Lets the model
/// return a `WorkItemDup` verdict — "this attention is already covered by a
/// scheduled row".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItemBrief {
    /// Short display id, e.g. `"T42"` / `"C7"` / `"R3"` / `"P12"`. A
    /// `WorkItemDup` verdict must name one of these.
    pub work_item_id: String,
    /// `"task"` | `"chore"` | `"revision"` | `"project"`.
    pub kind: String,
    pub title: String,
    /// Non-terminal state string, for context (e.g. `"in_progress"`).
    pub status: String,
    /// First ~200 chars of the description, for context.
    pub description_snippet: String,
}

/// Everything the dedup decision needs: the candidate plus the bounded
/// comparison set. The engine builds this after the cheap exact-dedup line has
/// already run and missed (design §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DedupInput {
    /// The `Attention` about to be persisted (creation path) or re-examined
    /// (sweep path), rendered with the [`CANDIDATE_ID`] sentinel.
    pub candidate: AttentionBrief,
    /// Top-K open `Attention` items in the same product (the prefiltered
    /// candidate set). An `AttentionDup` verdict must reference one of these.
    pub existing_attentions: Vec<AttentionBrief>,
    /// Non-terminal work items in the same product. Empty unless the taxonomy
    /// sub-flag is on. A `WorkItemDup` verdict must reference one of these.
    pub existing_work_items: Vec<WorkItemBrief>,
    /// Whether to also evaluate the sensibility filter (stale / moot / not
    /// actionable). Set from the `notification_dedup_sensibility` sub-flag;
    /// when `false` the model is told not to produce a `Sensibility` verdict.
    pub sensibility_check: bool,
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

/// The verdict the LLM returns for a candidate. Serialised internally-tagged
/// on the `type` discriminator so it maps 1:1 to the flat, model-friendly
/// [`dedup_decision_schema`] and deserialises straight into this enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DedupVerdict {
    /// Not a duplicate, not stale — persist the candidate normally.
    Keep,
    /// A near-duplicate of an existing open `Attention`; fold into that
    /// canonical item. The id MUST be one of [`DedupInput::existing_attentions`].
    AttentionDup { canonical_attention_id: String },
    /// Already covered by a scheduled work item; suppress-with-pointer (High)
    /// or link (Medium). The id MUST be one of [`DedupInput::existing_work_items`].
    WorkItemDup { work_item_id: String },
    /// Stale, moot, or not actionable. Acted on only at `High` confidence and
    /// only when the reason is specific and checkable (design R13); otherwise
    /// the engine downgrades it to `Keep`.
    Sensibility { reason: String },
}

impl DedupVerdict {
    /// Short stable tag for logs / telemetry.
    pub fn tag(&self) -> &'static str {
        match self {
            DedupVerdict::Keep => "keep",
            DedupVerdict::AttentionDup { .. } => "attention_dup",
            DedupVerdict::WorkItemDup { .. } => "work_item_dup",
            DedupVerdict::Sensibility { .. } => "sensibility",
        }
    }
}

/// Which free-text field of the canonical `Attention` a bounded merge edit
/// targets. Append-only, explanatory prose only — never the question text, an
/// answer, or structural fields (design §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditableField {
    /// Append to the canonical `Attention`'s `rationale`.
    RationaleAppend,
    /// Append to the canonical `Attention`'s `proposed_description`.
    DescriptionAppend,
}

/// A single bounded, append-only edit to the canonical `Attention`, folding in
/// new information carried by the duplicate. Length-bounded and only ever
/// meaningful for an `AttentionDup` verdict (design §5). Applying the edit is a
/// later task; this crate only carries the contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalEdit {
    pub field: EditableField,
    /// The text to append (bounded length; over-budget edits are dropped by
    /// the applier, never truncated mid-fold).
    pub new_text: String,
}

/// The structured output of one dedup decision — deserialised directly from
/// the forced tool call. `proposed_edits` is only meaningful for an
/// `AttentionDup` verdict and may be empty (the common, safe default).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DedupDecision {
    pub verdict: DedupVerdict,
    pub confidence: Confidence,
    /// Short "why" note, persisted to `attention_merges.decision_rationale`.
    pub rationale: String,
    /// Bounded minor edits to fold into the canonical; may be empty.
    #[serde(default)]
    pub proposed_edits: Vec<CanonicalEdit>,
}

impl DedupDecision {
    /// The fail-safe decision: keep the candidate, no edits. Used whenever the
    /// call fails, the output is malformed, or engine-side validation rejects
    /// a verdict — the design's cardinal rule is "a false fold is worse than a
    /// missed one", so every failure mode lands here.
    pub fn keep() -> Self {
        Self::keep_with(Confidence::High, String::new())
    }

    /// A `Keep` decision preserving the model's confidence and rationale — used
    /// when a would-be dup is downgraded to `Keep` because it failed validation
    /// (e.g. a hallucinated id), so the log still shows what the model thought.
    pub fn keep_with(confidence: Confidence, rationale: impl Into<String>) -> Self {
        Self {
            verdict: DedupVerdict::Keep,
            confidence,
            rationale: rationale.into(),
            proposed_edits: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// JSON Schema for structured-output enforcement
// ---------------------------------------------------------------------------

/// The JSON Schema used as the `input_schema` of the Anthropic forced tool
/// call that constrains the model's response to [`DedupDecision`].
///
/// Conservative in the same style as
/// [`planner_output_schema`](crate::planner_output_schema): every field is
/// `required`, every enum enumerates its legal values, and each `verdict`
/// branch pins its discriminator and payload with `additionalProperties:
/// false`. A response outside this shape fails to deserialise and the engine
/// falls back to `Keep`.
pub fn dedup_decision_schema() -> Value {
    json!({
        "type": "object",
        "required": ["verdict", "confidence", "rationale", "proposed_edits"],
        "additionalProperties": false,
        "properties": {
            "verdict": {
                "type": "object",
                "description": "The dedup verdict for the candidate notification.",
                "oneOf": [
                    {
                        "title": "keep",
                        "type": "object",
                        "required": ["type"],
                        "additionalProperties": false,
                        "properties": {
                            "type": {
                                "type": "string",
                                "enum": ["keep"],
                                "description": "Not a duplicate and not stale; persist the candidate normally."
                            }
                        }
                    },
                    {
                        "title": "attention_dup",
                        "type": "object",
                        "required": ["type", "canonical_attention_id"],
                        "additionalProperties": false,
                        "properties": {
                            "type": { "type": "string", "enum": ["attention_dup"] },
                            "canonical_attention_id": {
                                "type": "string",
                                "description": "attention_id of the existing notification this candidate duplicates. MUST be one of the ids in the 'Existing open notifications' list — never 'candidate' or an invented id."
                            }
                        }
                    },
                    {
                        "title": "work_item_dup",
                        "type": "object",
                        "required": ["type", "work_item_id"],
                        "additionalProperties": false,
                        "properties": {
                            "type": { "type": "string", "enum": ["work_item_dup"] },
                            "work_item_id": {
                                "type": "string",
                                "description": "id of the scheduled work item already covering this candidate. MUST be one of the ids in the 'Existing scheduled work items' list."
                            }
                        }
                    },
                    {
                        "title": "sensibility",
                        "type": "object",
                        "required": ["type", "reason"],
                        "additionalProperties": false,
                        "properties": {
                            "type": { "type": "string", "enum": ["sensibility"] },
                            "reason": {
                                "type": "string",
                                "description": "Why the candidate is stale, moot, or not actionable. MUST cite a specific, checkable fact — a work-item id (e.g. T42), a PR/issue reference (e.g. #123), or a file path. Vague reasons like 'low priority' are rejected."
                            }
                        }
                    }
                ]
            },
            "confidence": {
                "type": "string",
                "enum": ["high", "medium", "low"],
                "description": "Confidence in this verdict. A dup folds on high/medium; a sensibility suppression only on high."
            },
            "rationale": {
                "type": "string",
                "description": "A short 'why' note explaining the verdict; recorded for provenance."
            },
            "proposed_edits": {
                "type": "array",
                "description": "Bounded, append-only edits folding new information into the canonical attention. Only meaningful for an attention_dup verdict; use an empty array otherwise.",
                "items": {
                    "type": "object",
                    "required": ["field", "new_text"],
                    "additionalProperties": false,
                    "properties": {
                        "field": {
                            "type": "string",
                            "enum": ["rationale_append", "description_append"],
                            "description": "Which free-text field of the canonical attention to append to."
                        },
                        "new_text": {
                            "type": "string",
                            "description": "Short text to append (a sentence or two at most)."
                        }
                    }
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
    fn keep_verdict_round_trips_as_bare_discriminator() {
        let decision = DedupDecision::keep();
        let json = serde_json::to_value(&decision).expect("serialises");
        assert_eq!(json["verdict"]["type"], "keep");
        // Internally-tagged unit variant carries only the discriminator.
        assert!(json["verdict"].get("canonical_attention_id").is_none());
        let back: DedupDecision = serde_json::from_value(json).expect("deserialises");
        assert_eq!(back, decision);
    }

    #[test]
    fn attention_dup_verdict_round_trips() {
        let decision = DedupDecision {
            verdict: DedupVerdict::AttentionDup {
                canonical_attention_id: "A5".to_owned(),
            },
            confidence: Confidence::High,
            rationale: "same missing-index concern".to_owned(),
            proposed_edits: vec![CanonicalEdit {
                field: EditableField::RationaleAppend,
                new_text: "also seen on startup".to_owned(),
            }],
        };
        let json = serde_json::to_value(&decision).expect("serialises");
        assert_eq!(json["verdict"]["type"], "attention_dup");
        assert_eq!(json["verdict"]["canonical_attention_id"], "A5");
        assert_eq!(json["proposed_edits"][0]["field"], "rationale_append");
        let back: DedupDecision = serde_json::from_value(json).expect("deserialises");
        assert_eq!(back, decision);
    }

    #[test]
    fn work_item_dup_and_sensibility_round_trip() {
        for verdict in [
            DedupVerdict::WorkItemDup {
                work_item_id: "T42".to_owned(),
            },
            DedupVerdict::Sensibility {
                reason: "references PR #123 which is now merged".to_owned(),
            },
        ] {
            let decision = DedupDecision {
                verdict: verdict.clone(),
                confidence: Confidence::Medium,
                rationale: "r".to_owned(),
                proposed_edits: vec![],
            };
            let json = serde_json::to_string(&decision).expect("serialises");
            let back: DedupDecision = serde_json::from_str(&json).expect("deserialises");
            assert_eq!(back.verdict, verdict);
        }
    }

    #[test]
    fn proposed_edits_defaults_to_empty_when_omitted() {
        // A model reply that omits proposed_edits still deserialises cleanly.
        let json = json!({
            "verdict": { "type": "keep" },
            "confidence": "low",
            "rationale": "novel"
        });
        let decision: DedupDecision = serde_json::from_value(json).expect("deserialises");
        assert!(decision.proposed_edits.is_empty());
        assert_eq!(decision.confidence, Confidence::Low);
    }

    #[test]
    fn verdict_tags_are_stable() {
        assert_eq!(DedupVerdict::Keep.tag(), "keep");
        assert_eq!(
            DedupVerdict::AttentionDup {
                canonical_attention_id: "A1".into()
            }
            .tag(),
            "attention_dup",
        );
        assert_eq!(
            DedupVerdict::WorkItemDup {
                work_item_id: "T1".into()
            }
            .tag(),
            "work_item_dup",
        );
        assert_eq!(DedupVerdict::Sensibility { reason: "x".into() }.tag(), "sensibility");
    }

    #[test]
    fn dedup_input_round_trips() {
        let input = DedupInput {
            candidate: AttentionBrief {
                attention_id: CANDIDATE_ID.to_owned(),
                group_id: "g1".to_owned(),
                kind: "question".to_owned(),
                association: "proj/foo".to_owned(),
                rendered: "missing index on created_at".to_owned(),
            },
            existing_attentions: vec![AttentionBrief {
                attention_id: "A5".to_owned(),
                group_id: "g2".to_owned(),
                kind: "question".to_owned(),
                association: "proj/bar".to_owned(),
                rendered: "add an index for the attention query".to_owned(),
            }],
            existing_work_items: vec![WorkItemBrief {
                work_item_id: "T42".to_owned(),
                kind: "task".to_owned(),
                title: "Add created_at index".to_owned(),
                status: "in_progress".to_owned(),
                description_snippet: "index the attentions table".to_owned(),
            }],
            sensibility_check: true,
        };
        let json = serde_json::to_string(&input).expect("serialises");
        let back: DedupInput = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back, input);
    }

    #[test]
    fn schema_is_valid_json_with_all_verdict_branches() {
        let schema = dedup_decision_schema();
        let s = serde_json::to_string(&schema).expect("schema serialises");
        assert!(s.contains("\"verdict\""));
        assert!(s.contains("\"confidence\""));
        assert!(s.contains("\"proposed_edits\""));
        // All four verdict discriminators are enumerated.
        for tag in ["keep", "attention_dup", "work_item_dup", "sensibility"] {
            assert!(s.contains(tag), "schema missing verdict branch {tag}");
        }
        // The verdict is a discriminated union.
        assert!(schema["properties"]["verdict"]["oneOf"].is_array());
        assert_eq!(schema["properties"]["verdict"]["oneOf"].as_array().unwrap().len(), 4,);
    }

    #[test]
    fn a_schema_shaped_reply_deserialises_into_the_contract() {
        // Mirror exactly what the model emits under `dedup_decision_schema`
        // for an attention_dup with an edit — proves schema and type agree.
        let reply = json!({
            "verdict": { "type": "attention_dup", "canonical_attention_id": "A5" },
            "confidence": "high",
            "rationale": "both flag the missing created_at index",
            "proposed_edits": [
                { "field": "description_append", "new_text": "also observed at startup" }
            ]
        });
        let decision: DedupDecision = serde_json::from_value(reply).expect("schema-shaped reply deserialises");
        assert_eq!(
            decision.verdict,
            DedupVerdict::AttentionDup {
                canonical_attention_id: "A5".to_owned()
            },
        );
        assert_eq!(decision.confidence, Confidence::High);
        assert_eq!(decision.proposed_edits.len(), 1);
        assert_eq!(decision.proposed_edits[0].field, EditableField::DescriptionAppend);
    }
}
