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

use boss_protocol::{CommentAnchor, INTENT_DIRECTIVE, INTENT_LARGER_CHANGE, INTENT_QUESTION};

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
<number between 0.0 and 1.0>}}.",
        prefix = anchor.prefix,
        exact = anchor.exact,
        suffix = anchor.suffix,
        body = body,
    )
}

/// Make a one-shot classification call. Returns `Err(message)` on any
/// failure (transport, non-2xx, malformed/invalid reply) — the caller logs
/// and leaves the comment's `intent` `NULL` (still `classifying`); there is
/// no retry in this phase.
pub async fn classify(api_key: &str, body: &str, anchor: &CommentAnchor) -> Result<Classification, String> {
    let prompt = build_prompt(body, anchor);
    let request = MessagesRequest::builder()
        .model(CLASSIFIER_MODEL)
        .max_tokens(CLASSIFIER_MAX_TOKENS)
        .messages(vec![Message::user(prompt)])
        .build();
    let config = CallConfig::new(CLASSIFIER_TIMEOUT);

    let response = claude_client::send_messages(api_key, &request, &config)
        .await
        .map_err(|e| e.to_string())?;

    let text = response.first_text().unwrap_or_default().trim();
    if text.is_empty() {
        return Err("Anthropic returned an empty response".to_owned());
    }

    let reply: ClassifyReply =
        serde_json::from_str(text).map_err(|e| format!("failed to parse classifier JSON reply: {e} (raw: {text})"))?;

    match reply.intent.as_str() {
        INTENT_DIRECTIVE | INTENT_QUESTION | INTENT_LARGER_CHANGE => {}
        other => return Err(format!("classifier returned unknown intent: {other}")),
    }

    Ok(Classification {
        intent: reply.intent,
        confidence: reply.confidence.clamp(0.0, 1.0),
    })
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
}
