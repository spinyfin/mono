//! The notification dedup decision — the single LLM step of the
//! near-duplicate-reconciliation feature.
//!
//! See `tools/boss/docs/designs/notification-dedup-scoring.md` §3 "The dedup
//! decision (the one LLM step)", R1 (model tier), R7 (reusable structured-call
//! substrate), and R13 (sensibility-reason validation). This module is task 3
//! of that design's breakdown.
//!
//! ## Pure transform, fail-safe to Keep
//!
//! [`decide_dedup`] takes a typed [`DedupInput`] (candidate attention + the
//! bounded comparison set of open attentions and non-terminal work items) and
//! returns a typed [`DedupDecision`]. It performs no writes. Its cardinal rule
//! is the design's: *a false fold is worse than a missed one*. So every failure
//! mode — no API key, a 4xx/5xx, a timeout, an off-contract reply, or a verdict
//! that fails engine-side validation — collapses to a [`DedupVerdict::Keep`],
//! never an error that could block notification creation. The `Result` return
//! is retained for signature stability; in practice it is always `Ok`.
//!
//! ## Structured output is enforced, not requested
//!
//! The call rides the reusable [`crate::structured_call`] helper (design R7):
//! a single forced tool call whose `input_schema` is
//! [`dedup_decision_schema`](boss_protocol::dedup_decision_schema). The model is
//! obligated to emit the [`DedupDecision`] shape, deserialised straight into the
//! Rust type — a malformed reply is a clean Keep, never parse-and-hope.
//!
//! ## Bounded model / tokens / timeout (design R1)
//!
//! The creation-time decision is frequent and a bounded binary-ish judgment, so
//! it defaults to the cheap [`DEDUP_MODEL`] (Haiku) with a tight token and
//! timeout budget. These are single tunable constants — R1 says start cheap and
//! upshift to Sonnet only if the measured false-fold rate warrants it.

use std::time::Duration;

use anyhow::Result;

use boss_protocol::{
    AttentionBrief, CANDIDATE_ID, DedupDecision, DedupInput, DedupVerdict, WorkItemBrief, dedup_decision_schema,
};

use crate::claude_client::{CallConfig, RetryPolicy};
use crate::structured_call::{StructuredCall, StructuredCallOutcome, structured_call};

/// The model the dedup decision runs on. Haiku by default (design R1): the
/// creation-time call is frequent and the judgment is bounded (one candidate
/// vs. a short prefiltered set), so a cheap tier is the right starting point.
/// A concrete id, not a `--model` alias — the Messages API needs a concrete id.
/// This is the *single tunable constant* R1 calls out: measure the false-fold
/// rate and upshift to Sonnet here if precision proves insufficient.
pub const DEDUP_MODEL: &str = "claude-haiku-4-5-20251001";

/// Output token ceiling. A verdict + confidence + a short rationale + at most a
/// couple of bounded edits fits comfortably; a tight bound keeps a runaway
/// reply from blowing the synchronous creation path's latency budget.
pub const DEDUP_MAX_TOKENS: u32 = 1024;

/// Wall-clock budget for one decision round trip. The creation path is
/// synchronous, so this is bounded — a wedged call fails safe to Keep rather
/// than hanging notification creation. Haiku on a small prompt returns well
/// inside this.
pub const DEDUP_TIMEOUT: Duration = Duration::from_secs(20);

/// Total attempts per decision: one retry of a transient failure, then fail
/// safe. Only 429/5xx/overloaded/transport are retried (see
/// [`ClaudeError::is_retryable`](crate::claude_client::ClaudeError::is_retryable)).
pub const DEDUP_ATTEMPTS: u32 = 2;

/// Backoff before the single retry.
pub const DEDUP_BACKOFF: Duration = Duration::from_millis(500);

/// Upper bound on backoff. Only one retry ever happens (`DEDUP_ATTEMPTS`), so
/// this just needs to be >= `DEDUP_BACKOFF`; it never actually caps anything.
pub const DEDUP_MAX_BACKOFF: Duration = Duration::from_millis(500);

/// Name of the forced tool whose `input_schema` is the [`DedupDecision`] shape.
pub const TOOL_NAME: &str = "emit_dedup_decision";

/// One-line tool description shown to the model alongside the schema.
const TOOL_DESCRIPTION: &str = "Emit the dedup verdict for the candidate notification: whether to keep it, \
     fold it into an existing notification, treat it as already covered by a \
     scheduled work item, or (when asked) suppress it as not-sensible — plus a \
     confidence, a short rationale, and any bounded edits to fold into the \
     canonical notification.";

/// Minimum trimmed length for a `Sensibility` reason to be considered specific
/// (design R13, proposal (b)): rejects bare judgments like "stale" or "moot".
const SENSIBILITY_REASON_MIN_CHARS: usize = 12;

/// Decide whether a candidate `Attention` is a duplicate (of another attention
/// or of a scheduled work item) or should be suppressed as not-sensible.
///
/// `api_key` is passed in (not read from config) so this stays a pure transform
/// with no config/DB dependency, mirroring [`crate::planner::Planner::plan`]. A
/// `None` key fails safe to [`DedupVerdict::Keep`] without a network call.
///
/// Never returns `Err` in practice: every transport/protocol/validation failure
/// maps to a fail-safe `Keep` (design §3, §6). The `Result` is kept so callers
/// and the signature stay stable if a genuinely fallible step is added later.
pub async fn decide_dedup(api_key: Option<&str>, input: &DedupInput) -> Result<DedupDecision> {
    Ok(decide_with_config(api_key, input, default_config()).await)
}

/// Default per-call transport config (timeout + one retry, real endpoint).
fn default_config() -> CallConfig {
    CallConfig::new(DEDUP_TIMEOUT).with_retry(RetryPolicy::new(DEDUP_ATTEMPTS, DEDUP_BACKOFF, DEDUP_MAX_BACKOFF))
}

/// Core of [`decide_dedup`] with the transport config injected so tests can
/// point at a mock endpoint. Runs the forced tool call, then applies
/// engine-side validation; any non-success outcome is a fail-safe `Keep`.
async fn decide_with_config(api_key: Option<&str>, input: &DedupInput, config: CallConfig) -> DedupDecision {
    let spec = build_spec(input);
    match structured_call::<DedupDecision>(api_key, &spec, &config).await {
        StructuredCallOutcome::Success(raw) => validate_decision(raw, input),
        other => {
            tracing::warn!(
                outcome = other.tag(),
                detail = %other.detail(),
                "dedup: decision call did not succeed; failing safe to Keep",
            );
            DedupDecision::keep()
        }
    }
}

/// Assemble the [`StructuredCall`] spec for one dedup decision.
fn build_spec(input: &DedupInput) -> StructuredCall {
    StructuredCall {
        model: DEDUP_MODEL.to_owned(),
        max_tokens: DEDUP_MAX_TOKENS,
        // A cheap, bounded judgment — no elevated effort budget.
        effort: None,
        system: Some(SYSTEM_PROMPT.to_owned()),
        user_prompt: build_user_prompt(input),
        tool_name: TOOL_NAME.to_owned(),
        tool_description: TOOL_DESCRIPTION.to_owned(),
        input_schema: dedup_decision_schema(),
    }
}

/// Build the user message: the candidate, the two comparison lists, and the
/// sensibility instruction. Public for unit tests and the prefilter task that
/// will assemble the [`DedupInput`] this renders.
pub fn build_user_prompt(input: &DedupInput) -> String {
    let mut out = String::new();
    out.push_str(
        "You are evaluating ONE candidate notification against existing open \
         notifications and existing scheduled work items, all within a single \
         product.\n\n",
    );

    out.push_str("## Candidate notification (evaluate THIS one)\n");
    out.push_str(&render_attention(&input.candidate));
    out.push('\n');

    out.push_str("## Existing open notifications (the candidate may duplicate one of these)\n");
    if input.existing_attentions.is_empty() {
        out.push_str("(none)\n");
    } else {
        for attention in &input.existing_attentions {
            out.push_str(&render_attention(attention));
        }
    }
    out.push('\n');

    out.push_str("## Existing scheduled work items (the candidate may already be covered by one)\n");
    if input.existing_work_items.is_empty() {
        out.push_str("(none)\n");
    } else {
        for item in &input.existing_work_items {
            out.push_str(&render_work_item(item));
        }
    }
    out.push('\n');

    if input.sensibility_check {
        out.push_str(
            "## Sensibility check: ENABLED\n\
             Also decide whether the candidate is stale, moot, or not actionable. \
             Only return a `sensibility` verdict at HIGH confidence AND with a \
             specific, checkable reason — cite a work-item id (e.g. T42), a \
             PR/issue number (e.g. #123), or a file path. A vague reason \
             (\"seems low priority\") is not acceptable; if unsure, use `keep`.\n\n",
        );
    } else {
        out.push_str(
            "## Sensibility check: DISABLED\n\
             Do NOT return a `sensibility` verdict under any circumstance.\n\n",
        );
    }

    out.push_str(&format!("Call `{TOOL_NAME}` exactly once with your verdict.\n"));
    out
}

/// Render one attention item as a compact bullet the model reasons over.
fn render_attention(attention: &AttentionBrief) -> String {
    format!(
        "- id={} kind={} association={}\n  {}\n",
        attention.attention_id,
        attention.kind,
        attention.association,
        attention.rendered.trim(),
    )
}

/// Render one work item as a compact bullet.
fn render_work_item(item: &WorkItemBrief) -> String {
    format!(
        "- id={} {} ({}): {} — {}\n",
        item.work_item_id,
        item.kind,
        item.status,
        item.title.trim(),
        item.description_snippet.trim(),
    )
}

/// Engine-side validation of a model verdict (design §3). The model is never
/// trusted to reference a real id or to justify a suppression; anything that
/// fails a check is downgraded to a fail-safe `Keep`:
///
/// - `AttentionDup` — the `canonical_attention_id` must be one of
///   [`DedupInput::existing_attentions`] and never the candidate sentinel.
/// - `WorkItemDup` — the `work_item_id` must be one of
///   [`DedupInput::existing_work_items`].
/// - `Sensibility` — only when the caller asked for it
///   ([`DedupInput::sensibility_check`]) and the reason is specific and
///   checkable (design R13); otherwise Keep.
///
/// Note: confidence gating (fold on High/Medium, not on Low) and edit-bounds
/// enforcement are the *caller's* job at the fold site — this function only
/// rejects structurally invalid verdicts.
pub fn validate_decision(raw: DedupDecision, input: &DedupInput) -> DedupDecision {
    let valid = match &raw.verdict {
        DedupVerdict::Keep => true,
        DedupVerdict::AttentionDup { canonical_attention_id } => {
            canonical_attention_id != CANDIDATE_ID
                && input
                    .existing_attentions
                    .iter()
                    .any(|a| &a.attention_id == canonical_attention_id)
        }
        DedupVerdict::WorkItemDup { work_item_id } => input
            .existing_work_items
            .iter()
            .any(|w| &w.work_item_id == work_item_id),
        DedupVerdict::Sensibility { reason } => input.sensibility_check && reason_is_entity_specific(reason),
    };

    if valid {
        raw
    } else {
        tracing::warn!(
            verdict = raw.verdict.tag(),
            rationale = %raw.rationale,
            "dedup: verdict failed engine-side validation; downgrading to Keep",
        );
        DedupDecision::keep_with(raw.confidence, raw.rationale)
    }
}

/// Whether a `Sensibility` reason is specific and checkable enough to act on
/// (design R13, combining proposals (a) entity reference + (b) length floor).
/// Conservative on purpose: a false suppression of a real attention is worse
/// than letting a near-dup through, so a reason that does not clearly cite a
/// concrete fact is rejected (→ Keep).
fn reason_is_entity_specific(reason: &str) -> bool {
    let trimmed = reason.trim();
    trimmed.chars().count() >= SENSIBILITY_REASON_MIN_CHARS && has_concrete_reference(trimmed)
}

/// Does the text cite a concrete, checkable entity? Recognises: an issue/PR
/// reference (`#123`), a short work-item id (an uppercase letter immediately
/// followed by a digit, e.g. `T42`), or a source file path/extension.
fn has_concrete_reference(text: &str) -> bool {
    let bytes = text.as_bytes();
    for pair in bytes.windows(2) {
        // "#123" — issue / PR reference.
        if pair[0] == b'#' && pair[1].is_ascii_digit() {
            return true;
        }
        // "T42" / "C7" / "R3" / "P12" — a short work-item id.
        if pair[0].is_ascii_uppercase() && pair[1].is_ascii_digit() {
            return true;
        }
    }
    const EXTS: &[&str] = &[
        ".rs", ".swift", ".md", ".toml", ".py", ".ts", ".tsx", ".js", ".jsx", ".json", ".sql", ".sh", ".yaml", ".yml",
        ".proto", ".bazel",
    ];
    let lower = text.to_ascii_lowercase();
    EXTS.iter().any(|ext| lower.contains(ext))
}

/// The dedup decision's system prompt. Encodes the policy the design specifies:
/// the four-way verdict space (attention dup + work-item dup + sensibility +
/// keep), the conservative "when in doubt, Keep" bar, the id-fidelity rule, and
/// the bounded-edit contract — all in a single call (design §3, §4, §5).
const SYSTEM_PROMPT: &str = "\
You are the Boss notification dedup judge. Given ONE candidate notification and \
a bounded set of existing open notifications plus existing scheduled work items \
(tasks, chores, revisions, projects) — all within a single product — you decide \
whether the candidate is a near-duplicate, is already covered by scheduled work, \
or (when asked) is not worth surfacing. You write no code and change no state: \
your entire job is to make exactly one `emit_dedup_decision` tool call.\n\
\n\
## The verdict space\n\
\n\
Return exactly one of:\n\
- `keep` — the candidate is a genuinely NEW concern: not a near-duplicate of any \
existing notification, not already covered by a scheduled work item, and (if a \
sensibility check was requested) still actionable. This is the default.\n\
- `attention_dup` — the candidate reports essentially the SAME concern as one of \
the existing open notifications, even if phrased differently, anchored \
differently, or raised by a different task. Set `canonical_attention_id` to that \
existing notification's id. The id MUST be one of the ids listed under \"Existing \
open notifications\" — never the candidate, never an invented id.\n\
- `work_item_dup` — the candidate is already covered by one of the listed \
scheduled work items (the work the candidate asks for is already tracked as that \
row). Set `work_item_id` to that item's id. The id MUST be one of the ids listed \
under \"Existing scheduled work items\".\n\
- `sensibility` — ONLY if the sensibility check is enabled: the candidate is \
stale, moot, or not actionable on its own merits. See the bar below.\n\
\n\
## The bar — when in doubt, keep\n\
\n\
Folding two DIFFERENT concerns together silently hides a real notification, \
which is worse than leaving a near-duplicate un-folded. So:\n\
- Only return `attention_dup` / `work_item_dup` when you are genuinely \
confident the two describe the same underlying concern — not merely the same \
area, file, or topic. Two notifications about the same file but different \
problems are NOT duplicates.\n\
- Prefer `keep` whenever you are unsure.\n\
- Reserve `high` confidence for clear matches; use `medium` when plausible but \
not certain; use `low` when you lean toward a match but would not stake much on \
it. A `keep` may carry any confidence.\n\
\n\
## Sensibility (only when enabled)\n\
\n\
A `sensibility` verdict suppresses a real notification, so the bar is the \
highest: return it ONLY at `high` confidence and ONLY with a specific, checkable \
reason that cites a concrete fact — a work-item id (e.g. T42), a PR/issue number \
(e.g. #123), or a file path. Reasons that are subjective or generic (\"seems low \
priority\", \"probably stale\") are NOT acceptable and will be rejected. If you \
cannot name a concrete reason, use `keep`.\n\
\n\
## rationale\n\
\n\
Always give a short `rationale`: one or two sentences on WHY you reached the \
verdict (what makes the two the same concern, or which work item covers it, or \
what concrete fact makes it not sensible). It is recorded for provenance.\n\
\n\
## proposed_edits (optional, attention_dup only)\n\
\n\
When you return `attention_dup` and the candidate carries genuinely NEW \
information the canonical notification lacks, you MAY propose a bounded, \
append-only edit to fold it in: append to the canonical's rationale \
(`rationale_append`) or its description (`description_append`). Keep each edit to \
a sentence or two. Do not rewrite or contradict the canonical; only add. Most \
folds need no edit — return an empty `proposed_edits` array. For any verdict \
other than `attention_dup`, always return an empty `proposed_edits` array.\
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_client;
    use boss_protocol::{CanonicalEdit, Confidence, EditableField};
    use serde_json::{Value, json};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn candidate() -> AttentionBrief {
        AttentionBrief {
            attention_id: CANDIDATE_ID.to_owned(),
            group_id: "g_cand".to_owned(),
            kind: "question".to_owned(),
            association: "proj/alpha".to_owned(),
            rendered: "The migration is missing an index on created_at".to_owned(),
        }
    }

    fn existing_attention(id: &str) -> AttentionBrief {
        AttentionBrief {
            attention_id: id.to_owned(),
            group_id: "g_other".to_owned(),
            kind: "question".to_owned(),
            association: "proj/beta".to_owned(),
            rendered: "Add an index for the attention-items query".to_owned(),
        }
    }

    fn work_item(id: &str) -> WorkItemBrief {
        WorkItemBrief {
            work_item_id: id.to_owned(),
            kind: "task".to_owned(),
            title: "Add created_at index".to_owned(),
            status: "in_progress".to_owned(),
            description_snippet: "index the attentions table".to_owned(),
        }
    }

    fn input_with(
        existing: Vec<AttentionBrief>,
        work_items: Vec<WorkItemBrief>,
        sensibility_check: bool,
    ) -> DedupInput {
        DedupInput {
            candidate: candidate(),
            existing_attentions: existing,
            existing_work_items: work_items,
            sensibility_check,
        }
    }

    fn decision(verdict: DedupVerdict) -> DedupDecision {
        DedupDecision {
            verdict,
            confidence: Confidence::High,
            rationale: "same missing-index concern (see created_at)".to_owned(),
            proposed_edits: vec![],
        }
    }

    // ── build_spec / prompt ────────────────────────────────────────────────

    #[test]
    fn build_spec_uses_haiku_and_forces_the_dedup_tool() {
        let spec = build_spec(&input_with(vec![], vec![], false));
        assert_eq!(spec.model, DEDUP_MODEL);
        assert_eq!(spec.max_tokens, DEDUP_MAX_TOKENS);
        assert_eq!(spec.tool_name, TOOL_NAME);
        // The schema is the shared contract schema.
        assert_eq!(spec.input_schema, dedup_decision_schema());
        // The forced request the model actually sees.
        let body = serde_json::to_value(crate::structured_call::build_request(&spec)).unwrap();
        assert_eq!(body["model"], DEDUP_MODEL);
        assert_eq!(body["tool_choice"]["name"], TOOL_NAME);
    }

    #[test]
    fn user_prompt_carries_candidate_and_both_lists() {
        let input = input_with(vec![existing_attention("A5")], vec![work_item("T42")], false);
        let prompt = build_user_prompt(&input);
        assert!(prompt.contains("Candidate notification"));
        assert!(prompt.contains("missing an index on created_at"));
        assert!(prompt.contains("id=A5"));
        assert!(prompt.contains("id=T42"));
        assert!(prompt.contains("Sensibility check: DISABLED"));
    }

    #[test]
    fn user_prompt_marks_empty_lists_and_enabled_sensibility() {
        let prompt = build_user_prompt(&input_with(vec![], vec![], true));
        assert!(prompt.contains("Existing open notifications"));
        assert!(prompt.contains("(none)"));
        assert!(prompt.contains("Sensibility check: ENABLED"));
    }

    #[test]
    fn system_prompt_encodes_the_required_policy() {
        assert!(SYSTEM_PROMPT.contains("emit_dedup_decision"));
        assert!(SYSTEM_PROMPT.contains("attention_dup"));
        assert!(SYSTEM_PROMPT.contains("work_item_dup"));
        assert!(SYSTEM_PROMPT.contains("sensibility"));
        assert!(SYSTEM_PROMPT.contains("when in doubt, keep"));
        assert!(SYSTEM_PROMPT.contains("append-only"));
    }

    // ── validation ─────────────────────────────────────────────────────────

    #[test]
    fn keep_verdict_passes_validation_unchanged() {
        let input = input_with(vec![existing_attention("A5")], vec![], false);
        let out = validate_decision(decision(DedupVerdict::Keep), &input);
        assert_eq!(out.verdict, DedupVerdict::Keep);
    }

    #[test]
    fn attention_dup_with_known_id_is_accepted() {
        let input = input_with(vec![existing_attention("A5")], vec![], false);
        let out = validate_decision(
            decision(DedupVerdict::AttentionDup {
                canonical_attention_id: "A5".to_owned(),
            }),
            &input,
        );
        assert!(matches!(out.verdict, DedupVerdict::AttentionDup { .. }));
    }

    #[test]
    fn attention_dup_with_hallucinated_id_downgrades_to_keep() {
        let input = input_with(vec![existing_attention("A5")], vec![], false);
        let out = validate_decision(
            decision(DedupVerdict::AttentionDup {
                canonical_attention_id: "A999".to_owned(),
            }),
            &input,
        );
        assert_eq!(out.verdict, DedupVerdict::Keep);
        // Confidence + rationale are preserved so the log shows the model's view.
        assert_eq!(out.confidence, Confidence::High);
        assert!(out.rationale.contains("missing-index"));
    }

    #[test]
    fn attention_dup_naming_the_candidate_sentinel_is_rejected() {
        let input = input_with(vec![existing_attention("A5")], vec![], false);
        let out = validate_decision(
            decision(DedupVerdict::AttentionDup {
                canonical_attention_id: CANDIDATE_ID.to_owned(),
            }),
            &input,
        );
        assert_eq!(out.verdict, DedupVerdict::Keep);
    }

    #[test]
    fn work_item_dup_id_membership_is_enforced() {
        let input = input_with(vec![], vec![work_item("T42")], false);
        let accepted = validate_decision(
            decision(DedupVerdict::WorkItemDup {
                work_item_id: "T42".to_owned(),
            }),
            &input,
        );
        assert!(matches!(accepted.verdict, DedupVerdict::WorkItemDup { .. }));

        let rejected = validate_decision(
            decision(DedupVerdict::WorkItemDup {
                work_item_id: "T999".to_owned(),
            }),
            &input,
        );
        assert_eq!(rejected.verdict, DedupVerdict::Keep);
    }

    #[test]
    fn sensibility_requires_the_flag_and_a_specific_reason() {
        let good_reason = DedupVerdict::Sensibility {
            reason: "task T42 already explicitly covers this".to_owned(),
        };

        // Flag on + specific reason → accepted.
        let on = input_with(vec![], vec![], true);
        assert!(matches!(
            validate_decision(decision(good_reason.clone()), &on).verdict,
            DedupVerdict::Sensibility { .. },
        ));

        // Flag off → rejected even with a good reason.
        let off = input_with(vec![], vec![], false);
        assert_eq!(
            validate_decision(decision(good_reason), &off).verdict,
            DedupVerdict::Keep
        );

        // Flag on + vague reason → rejected.
        let vague = DedupVerdict::Sensibility {
            reason: "seems low priority".to_owned(),
        };
        assert_eq!(validate_decision(decision(vague), &on).verdict, DedupVerdict::Keep);
    }

    #[test]
    fn reason_specificity_rules() {
        // Concrete references pass.
        assert!(reason_is_entity_specific("references PR #123 which is now merged"));
        assert!(reason_is_entity_specific("task T42 already covers this"));
        assert!(reason_is_entity_specific("the fix landed in schema_init.rs already"));
        // Bare judgments fail (too short and/or no concrete reference).
        assert!(!reason_is_entity_specific("stale"));
        assert!(!reason_is_entity_specific("not actionable"));
        assert!(!reason_is_entity_specific("this seems moot to me now"));
    }

    // ── end-to-end ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn no_api_key_fails_safe_to_keep() {
        let out = decide_dedup(None, &input_with(vec![existing_attention("A5")], vec![], false))
            .await
            .expect("fail-safe never errors");
        assert_eq!(out.verdict, DedupVerdict::Keep);
    }

    fn tool_use_body(input: Value) -> Value {
        json!({
            "content": [
                { "type": "text", "text": "" },
                { "type": "tool_use", "id": "toolu_x", "name": TOOL_NAME, "input": input }
            ]
        })
    }

    async fn mock_server_returning(body: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", claude_client::ANTHROPIC_API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn end_to_end_valid_attention_dup_is_returned() {
        let server = mock_server_returning(tool_use_body(json!({
            "verdict": { "type": "attention_dup", "canonical_attention_id": "A5" },
            "confidence": "high",
            "rationale": "both flag the missing created_at index",
            "proposed_edits": [{ "field": "rationale_append", "new_text": "also seen on startup" }]
        })))
        .await;

        let input = input_with(vec![existing_attention("A5")], vec![], false);
        let config = default_config().with_endpoint(format!("{}/v1/messages", server.uri()));
        let out = decide_with_config(Some("test-key"), &input, config).await;
        assert_eq!(
            out.verdict,
            DedupVerdict::AttentionDup {
                canonical_attention_id: "A5".to_owned()
            },
        );
        assert_eq!(
            out.proposed_edits,
            vec![CanonicalEdit {
                field: EditableField::RationaleAppend,
                new_text: "also seen on startup".to_owned(),
            }]
        );
    }

    #[tokio::test]
    async fn end_to_end_hallucinated_id_is_downgraded_to_keep() {
        let server = mock_server_returning(tool_use_body(json!({
            "verdict": { "type": "attention_dup", "canonical_attention_id": "A_ghost" },
            "confidence": "high",
            "rationale": "looks similar",
            "proposed_edits": []
        })))
        .await;

        let input = input_with(vec![existing_attention("A5")], vec![], false);
        let config = default_config().with_endpoint(format!("{}/v1/messages", server.uri()));
        let out = decide_with_config(Some("test-key"), &input, config).await;
        assert_eq!(out.verdict, DedupVerdict::Keep);
    }

    #[tokio::test]
    async fn end_to_end_api_error_fails_safe_to_keep() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        let input = input_with(vec![existing_attention("A5")], vec![], false);
        let config = default_config().with_endpoint(format!("{}/v1/messages", server.uri()));
        let out = decide_with_config(Some("test-key"), &input, config).await;
        assert_eq!(out.verdict, DedupVerdict::Keep);
    }
}
