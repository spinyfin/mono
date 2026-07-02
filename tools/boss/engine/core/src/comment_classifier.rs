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
//! for `crate::magic_wand`'s length/diff sanity checks.

use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use boss_protocol::{CommentAnchor, INTENT_DIRECTIVE, INTENT_LARGER_CHANGE, INTENT_QUESTION};

const CLASSIFIER_API_KEY_ENV: &str = "BOSS_INTENT_CLASSIFIER_API_KEY";
const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

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
/// env var, falling back to the shared `ANTHROPIC_API_KEY` — same pattern
/// as `crate::magic_wand::resolve_api_key` / `crate::attentions_detector`.
pub fn resolve_api_key() -> Option<String> {
    std::env::var(CLASSIFIER_API_KEY_ENV)
        .ok()
        .or_else(|| std::env::var(ANTHROPIC_API_KEY_ENV).ok())
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        // The workspace pins reqwest to `rustls-no-provider`; install a default
        // crypto provider before the first TLS handshake (same pattern as
        // `pane_summary.rs` / `magic_wand.rs`).
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .timeout(CLASSIFIER_TIMEOUT)
            .build()
            .expect("reqwest::Client::build should not fail with default config")
    })
}

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<ApiMessage<'a>>,
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ApiContentBlock>,
}

#[derive(Deserialize)]
struct ApiContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: String,
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
    let request = ApiRequest {
        model: CLASSIFIER_MODEL,
        max_tokens: CLASSIFIER_MAX_TOKENS,
        messages: vec![ApiMessage {
            role: "user",
            content: prompt,
        }],
    };

    let resp = http_client()
        .post(ANTHROPIC_MESSAGES_URL)
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_API_VERSION)
        .header("content-type", "application/json")
        .json(&request)
        .send()
        .await
        .map_err(|e| format!("HTTP send failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Anthropic API returned {status}: {text}"));
    }

    let parsed: ApiResponse = resp
        .json()
        .await
        .map_err(|e| format!("failed to parse Anthropic response: {e}"))?;

    let text = parsed
        .content
        .into_iter()
        .find(|b| b.block_type == "text")
        .map(|b| b.text)
        .unwrap_or_default();
    let text = text.trim();
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
