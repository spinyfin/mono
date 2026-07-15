//! Detached-async LLM intent classifier wired off `CommentsCreate` — Phase 1
//! task 1a of `comment-triggered-document-revisions.md` ("intent columns +
//! async classifier plumbing"). A cheap, single-shot Claude call that reads
//! one comment's body + anchor and returns one of `directive` / `question` /
//! `larger_change` plus a confidence score.
//!
//! Scoping this call to the comment itself (not the full document, not repo
//! context) matches the design's framing: "a fast, cheap, single-call step,
//! not the answer agent" (§ "The classifier (P1 — foundation)"). Which
//! artifact kinds are eligible for classification (the `resolve_doc_owner`
//! scope guard) and what routes on the result (the nudge / answer-agent
//! dispatch) are separate, later-phase tasks — this module only produces
//! the label.
//!
//! Mirrors `crate::attentions_detector`'s backstop-call shape (dedicated
//! billing env var, JSON-only prompt, `serde_json` parse of the reply) —
//! this is a classification call, not a doc-rewrite call, so it has no need
//! for length/diff sanity checks. Transport goes through the shared
//! [`crate::claude_client`] pipeline like every other Claude call in the
//! engine.

use std::time::Duration;

use serde::Deserialize;

use boss_protocol::{CommentAnchor, CommentThreadEntry, INTENT_DIRECTIVE, INTENT_LARGER_CHANGE, INTENT_QUESTION};

use crate::claude_client::{self, CallConfig, Message, MessagesRequest};

const CLASSIFIER_API_KEY_ENV: &str = "BOSS_INTENT_CLASSIFIER_API_KEY";

/// Haiku 4.5 — design § "Per-comment LLM cost/latency at scale" calls for
/// "a small/cheap model-appropriate call"; matches the cheap-classification
/// precedent in `live_status.rs` / `attentions_detector.rs`.
const CLASSIFIER_MODEL: &str = "claude-haiku-4-5-20251001";
/// The reply is one small JSON object; this is generous headroom.
const CLASSIFIER_MAX_TOKENS: u32 = 200;
/// Design § "The classifier": "a few-hundred-ms-to-low-seconds LLM round
/// trip." 30s is a generous ceiling for a call this small — the create
/// request itself never waits on this (it runs detached).
const CLASSIFIER_TIMEOUT: Duration = Duration::from_secs(30);
/// Retry attempts for a single classification (transport failure, non-2xx,
/// or an unparseable/invalid reply) before giving up. A transient hiccup
/// (rate limit, brief 5xx, one malformed reply) shouldn't abandon a comment
/// to an indefinite "classifying" state on the first miss — see
/// `crate::app::comments::spawn_comment_classifier`, whose caller records a
/// terminal failure only after this is exhausted.
const CLASSIFIER_MAX_ATTEMPTS: u32 = 3;
/// Delay between retry attempts. Small and fixed — this is a detached
/// background call, not on any request's critical path, so there's no need
/// for backoff/jitter at this volume.
const CLASSIFIER_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Resolves the API key for the classifier call: a dedicated billing-bucket
/// env var, falling back to the shared `ANTHROPIC_API_KEY` — via the shared
/// pipeline's resolver, same pattern as `crate::attentions_detector`.
pub fn resolve_api_key() -> Option<String> {
    claude_client::resolve_api_key(Some(CLASSIFIER_API_KEY_ENV))
}

/// The classifier's raw JSON reply shape.
#[derive(Deserialize)]
struct ClassifyReply {
    intent: String,
    confidence: f64,
}

/// A validated classification outcome.
#[derive(Debug)]
pub struct Classification {
    pub intent: String,
    pub confidence: f64,
}

fn build_prompt(body: &str, anchor: &CommentAnchor) -> String {
    format!(
        "You classify a reviewer's comment on a design/investigation document into \
exactly one of three intents:\n\
\n\
- \"directive\": a clear, small, actionable instruction to change the doc (e.g. \
\"typo, should be X\", \"reword this sentence\", \"add a link to Y here\").\n\
- \"larger_change\": wants a substantive change but isn't a one-line edit (e.g. \
\"this section needs a new alternative considered\", \"rethink this approach\", \
\"this whole section is missing an important case\").\n\
- \"question\": asks something rather than asking for an edit (e.g. \"why did you \
choose X over Y?\", \"what does this mean?\", \"does this handle Z?\"). Not a \
request to change the doc.\n\
\n\
Quoted section from the document (the highlighted span, with surrounding context):\n\
> {prefix}[[{exact}]]{suffix}\n\
\n\
Comment:\n\
> {body}\n\
\n\
Respond with ONLY a JSON object — no explanation, no markdown fences — of the exact \
shape: {{\"intent\": \"directive\"|\"question\"|\"larger_change\", \"confidence\": \
<number between 0.0 and 1.0>}}. Do not wrap the JSON in a code fence.",
        prefix = anchor.prefix,
        exact = anchor.exact,
        suffix = anchor.suffix,
        body = body,
    )
}

/// Render a follow-up reply's classification prompt (P3c "Reclassifying
/// follow-ups"): the same three-intent rubric as [`build_prompt`], but with
/// the original comment plus the thread's prior turns as context ahead of
/// the reply actually being classified — design § "The classifier":
/// "for a reply — the prior thread turns."
fn build_followup_prompt(
    original_body: &str,
    anchor: &CommentAnchor,
    thread: &[CommentThreadEntry],
    followup_body: &str,
) -> String {
    let mut thread_block = String::new();
    for entry in thread {
        thread_block.push_str(&format!("{} ({}):\n{}\n\n", entry.entry_kind, entry.author, entry.body));
    }
    format!(
        "You classify a reviewer's follow-up reply in an ongoing thread on a design/investigation \
document comment, into exactly one of three intents:\n\
\n\
- \"directive\": a clear, small, actionable instruction to change the doc (e.g. \
\"typo, should be X\", \"reword this sentence\", \"add a link to Y here\").\n\
- \"larger_change\": wants a substantive change but isn't a one-line edit (e.g. \
\"this section needs a new alternative considered\", \"rethink this approach\", \
\"this whole section is missing an important case\").\n\
- \"question\": asks something rather than asking for an edit (e.g. \"why did you \
choose X over Y?\", \"what does this mean?\", \"does this handle Z?\"). Not a \
request to change the doc.\n\
\n\
Quoted section from the document (the highlighted span, with surrounding context):\n\
> {prefix}[[{exact}]]{suffix}\n\
\n\
Original comment:\n\
> {original_body}\n\
\n\
Prior thread on this comment:\n\
{thread_block}\n\
Follow-up reply to classify:\n\
> {followup_body}\n\
\n\
Respond with ONLY a JSON object — no explanation, no markdown fences — of the exact \
shape: {{\"intent\": \"directive\"|\"question\"|\"larger_change\", \"confidence\": \
<number between 0.0 and 1.0>}}. Do not wrap the JSON in a code fence.",
        prefix = anchor.prefix,
        exact = anchor.exact,
        suffix = anchor.suffix,
    )
}

/// Shared call plumbing for [`classify`] and [`classify_followup`]: send the
/// prompt, parse and validate the reply. Returns `Err(message)` on any
/// failure (transport, non-2xx, malformed/invalid reply) — callers log and
/// leave the comment's state unchanged; there is no retry in this phase.
async fn call_classifier(api_key: &str, prompt: String) -> Result<Classification, String> {
    let request = MessagesRequest::builder()
        .model(CLASSIFIER_MODEL)
        .max_tokens(CLASSIFIER_MAX_TOKENS)
        .messages(vec![Message::user(prompt)])
        .build();
    let config = CallConfig::new(CLASSIFIER_TIMEOUT);

    let response = claude_client::send_messages(api_key, &request, &config)
        .await
        .map_err(|e| e.to_string())?;

    let text = response.first_text().unwrap_or_default();
    parse_classifier_reply(text)
}

/// Best-effort extraction of a bare JSON object from a classifier reply.
/// The prompt asks for an unfenced object, but smaller models (this call
/// uses Haiku) sometimes wrap the reply in a ```` ```json ```` fence anyway —
/// defense in depth alongside the prompt instruction, not a replacement for
/// it. Strips a leading/trailing markdown code fence if present, then hands
/// off to the shared [`boss_engine_utils::json_extract::find_first_balanced_object`]
/// (string/escape-aware, so a `}` inside a string value or trailing prose
/// doesn't mis-bound the slice — the same helper `pr_review.rs` uses).
/// Falls back to the trimmed input unchanged when no balanced object is
/// found, so the original text still reaches `serde_json` and produces its
/// own error.
fn strip_to_json_object(text: &str) -> &str {
    let fenced = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .map(str::trim_start)
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim);
    let candidate = fenced.unwrap_or(text);
    boss_engine_utils::json_extract::find_first_balanced_object(candidate).unwrap_or(candidate)
}

/// The pure parse+validate step of a classifier call: turn the model's raw
/// reply text into a validated [`Classification`], independent of transport.
/// Kept as a standalone helper so its decision behavior (empty-reply,
/// malformed-JSON, unknown-intent, confidence clamping) is unit-testable
/// without a network call. [`call_classifier`] invokes it on the response
/// text — its observable behavior must match what lived inline before,
/// except that a fenced/prose-wrapped reply is now tolerated rather than
/// treated as malformed JSON (see [`strip_to_json_object`]).
fn parse_classifier_reply(text: &str) -> Result<Classification, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Anthropic returned an empty response".to_owned());
    }

    let candidate = strip_to_json_object(text);
    let reply: ClassifyReply = serde_json::from_str(candidate)
        .map_err(|e| format!("failed to parse classifier JSON reply: {e} (raw: {text})"))?;

    match reply.intent.as_str() {
        INTENT_DIRECTIVE | INTENT_QUESTION | INTENT_LARGER_CHANGE => {}
        other => return Err(format!("classifier returned unknown intent: {other}")),
    }

    Ok(Classification {
        intent: reply.intent,
        confidence: reply.confidence.clamp(0.0, 1.0),
    })
}

/// Retry [`call_classifier`] up to [`CLASSIFIER_MAX_ATTEMPTS`] times, pausing
/// [`CLASSIFIER_RETRY_DELAY`] between attempts. Returns the last error once
/// exhausted — the caller (`spawn_comment_classifier` /
/// `spawn_followup_classifier`) treats that as the terminal outcome rather
/// than retrying further itself.
async fn call_classifier_with_retries(api_key: &str, prompt: String) -> Result<Classification, String> {
    let mut last_err = String::new();
    for attempt in 1..=CLASSIFIER_MAX_ATTEMPTS {
        match call_classifier(api_key, prompt.clone()).await {
            Ok(classification) => return Ok(classification),
            Err(err) => {
                last_err = err;
                if attempt < CLASSIFIER_MAX_ATTEMPTS {
                    tokio::time::sleep(CLASSIFIER_RETRY_DELAY).await;
                }
            }
        }
    }
    Err(last_err)
}

/// Make a classification call for a fresh top-level comment, retrying
/// transient failures (see [`call_classifier_with_retries`]).
pub async fn classify(api_key: &str, body: &str, anchor: &CommentAnchor) -> Result<Classification, String> {
    call_classifier_with_retries(api_key, build_prompt(body, anchor)).await
}

/// Make a classification call for an operator's follow-up reply (P3c), with
/// the original comment and the thread's prior turns as context, retrying
/// transient failures (see [`call_classifier_with_retries`]).
pub async fn classify_followup(
    api_key: &str,
    original_body: &str,
    anchor: &CommentAnchor,
    thread: &[CommentThreadEntry],
    followup_body: &str,
) -> Result<Classification, String> {
    call_classifier_with_retries(
        api_key,
        build_followup_prompt(original_body, anchor, thread, followup_body),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_prompt_contains_body_and_anchor() {
        let anchor = CommentAnchor {
            exact: "the retry logic".to_owned(),
            prefix: "before ".to_owned(),
            suffix: " after".to_owned(),
        };
        let prompt = build_prompt("why does this retry three times?", &anchor);
        assert!(prompt.contains("the retry logic"));
        assert!(prompt.contains("why does this retry three times?"));
        assert!(prompt.contains("directive"));
        assert!(prompt.contains("question"));
        assert!(prompt.contains("larger_change"));
    }

    #[test]
    fn classify_reply_parses_valid_json() {
        let raw = r#"{"intent": "question", "confidence": 0.92}"#;
        let reply: ClassifyReply = serde_json::from_str(raw).unwrap();
        assert_eq!(reply.intent, "question");
        assert_eq!(reply.confidence, 0.92);
    }

    #[test]
    fn build_followup_prompt_contains_original_thread_and_followup() {
        let anchor = CommentAnchor {
            exact: "the retry logic".to_owned(),
            prefix: "before ".to_owned(),
            suffix: " after".to_owned(),
        };
        let thread = vec![
            CommentThreadEntry::builder()
                .id("cte_1")
                .comment_id("cmt_1")
                .entry_kind("answer")
                .author("engine")
                .body("The retry backoff is exponential because…")
                .created_at("2026-01-01T00:00:00Z")
                .build(),
        ];
        let prompt = build_followup_prompt(
            "why does this retry three times?",
            &anchor,
            &thread,
            "ok, please document that in the doc then",
        );
        assert!(prompt.contains("the retry logic"));
        assert!(prompt.contains("why does this retry three times?"));
        assert!(prompt.contains("The retry backoff is exponential because…"));
        assert!(prompt.contains("ok, please document that in the doc then"));
        assert!(prompt.contains("directive"));
        assert!(prompt.contains("question"));
        assert!(prompt.contains("larger_change"));
    }

    #[test]
    fn build_followup_prompt_handles_an_empty_thread() {
        let anchor = CommentAnchor {
            exact: "x".to_owned(),
            prefix: String::new(),
            suffix: String::new(),
        };
        let prompt = build_followup_prompt("original", &anchor, &[], "follow-up");
        assert!(prompt.contains("original"));
        assert!(prompt.contains("follow-up"));
    }

    #[test]
    fn parse_reply_rejects_an_empty_reply() {
        let err = parse_classifier_reply("").unwrap_err();
        assert_eq!(err, "Anthropic returned an empty response");
    }

    #[test]
    fn parse_reply_rejects_a_whitespace_only_reply() {
        let err = parse_classifier_reply("   \n\t ").unwrap_err();
        assert_eq!(err, "Anthropic returned an empty response");
    }

    #[test]
    fn parse_reply_rejects_malformed_json() {
        let err = parse_classifier_reply("not json at all").unwrap_err();
        assert!(err.contains("failed to parse classifier JSON reply"));
    }

    #[test]
    fn parse_reply_tolerates_a_json_fenced_reply() {
        let raw = "```json\n{\"intent\": \"question\", \"confidence\": 0.95}\n```";
        let classification = parse_classifier_reply(raw).unwrap();
        assert_eq!(classification.intent, "question");
        assert_eq!(classification.confidence, 0.95);
    }

    #[test]
    fn parse_reply_tolerates_a_bare_fenced_reply() {
        let raw = "```\n{\"intent\": \"directive\", \"confidence\": 0.8}\n```";
        let classification = parse_classifier_reply(raw).unwrap();
        assert_eq!(classification.intent, "directive");
    }

    #[test]
    fn parse_reply_tolerates_prose_wrapped_json() {
        let raw = "Here's the classification: {\"intent\": \"larger_change\", \"confidence\": 0.6} Hope that helps!";
        let classification = parse_classifier_reply(raw).unwrap();
        assert_eq!(classification.intent, "larger_change");
    }

    #[test]
    fn parse_reply_rejects_an_unrecognised_intent() {
        let err = parse_classifier_reply(r#"{"intent": "praise", "confidence": 0.9}"#).unwrap_err();
        assert_eq!(err, "classifier returned unknown intent: praise");
    }

    #[test]
    fn parse_reply_accepts_each_recognised_intent() {
        for intent in [INTENT_DIRECTIVE, INTENT_QUESTION, INTENT_LARGER_CHANGE] {
            let raw = format!(r#"{{"intent": "{intent}", "confidence": 0.5}}"#);
            let classification = parse_classifier_reply(&raw).unwrap();
            assert_eq!(classification.intent, intent);
        }
    }

    #[test]
    fn parse_reply_passes_an_in_range_confidence_through_unchanged() {
        let classification = parse_classifier_reply(r#"{"intent": "question", "confidence": 0.73}"#).unwrap();
        assert_eq!(classification.confidence, 0.73);
    }

    #[test]
    fn parse_reply_clamps_an_over_range_confidence_to_one() {
        let classification = parse_classifier_reply(r#"{"intent": "directive", "confidence": 1.5}"#).unwrap();
        assert_eq!(classification.confidence, 1.0);
    }

    #[test]
    fn parse_reply_clamps_a_negative_confidence_to_zero() {
        let classification = parse_classifier_reply(r#"{"intent": "larger_change", "confidence": -0.3}"#).unwrap();
        assert_eq!(classification.confidence, 0.0);
    }
}
