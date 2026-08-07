//! A reusable forced-tool-call structured-output helper over the shared
//! [`crate::claude_client`] pipeline.
//!
//! Several engine features need the same thing: force the model to emit a
//! *typed* structured value by defining a single tool whose `input_schema` is
//! the target shape, forcing that tool with `tool_choice`, and deserialising
//! the tool's `input` straight into a Rust type — never parsing free-form
//! prose. Today the [`crate::planner`] does this inline; the dedup decision
//! (`tools/boss/docs/designs/notification-dedup-scoring.md` R7) needs it too.
//! Rather than copy the request-shaping / extraction / error-mapping a second
//! time, this module factors it out so both share one implementation. See
//! design R7: "build it as a small reusable `structured_call` helper … so the
//! auto-populate Planner can share it."
//!
//! ## What it owns vs. what the caller owns
//!
//! This helper owns the *mechanics* of a forced tool call: assembling the
//! `tools` / `tool_choice` blocks, sending via [`crate::claude_client`], pulling
//! the forced tool's `input` out of the response, and mapping the shared
//! [`ClaudeError`](crate::claude_client::ClaudeError) taxonomy into a typed
//! [`StructuredCallOutcome`]. The caller owns its *feature*: the model, token
//! budget, effort, system prompt, user prompt, tool name/description, the JSON
//! schema, the [`CallConfig`] (timeout + retry), and all post-validation of the
//! deserialised value. It stays a pure transform — no DB, no config, no writes.

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::claude_client::{self, CallConfig, ClaudeError, Message, MessagesRequest, MessagesResponse};

/// Everything one forced-tool-call needs, minus transport config (that is the
/// [`CallConfig`]). Uses `#[derive(bon::Builder)]` per the repo's giant-struct
/// convention so an additive field never forces every construction site to
/// change; callers may also use a struct literal since every field is public.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct StructuredCall {
    /// Concrete model id (e.g. `claude-haiku-4-5-20251001`). The Messages API
    /// needs a concrete id, not a `--model` alias.
    pub model: String,
    /// Output token ceiling for the reply.
    pub max_tokens: u32,
    /// `output_config.effort` (`"high"`, …). `None` omits the block.
    pub effort: Option<String>,
    /// System prompt. `None` omits it.
    pub system: Option<String>,
    /// The single user-turn prompt carrying the rendered input.
    pub user_prompt: String,
    /// Name of the forced tool; must match the name used to extract the reply.
    pub tool_name: String,
    /// One-line tool description shown to the model alongside the schema.
    pub tool_description: String,
    /// JSON Schema for the tool's `input` — the structured-output contract.
    pub input_schema: Value,
}

/// Distinguishable outcomes for one structured call. Mirrors
/// [`crate::planner::PlannerOutcome`] / [`crate::live_status::SummarizerOutcome`]
/// so callers can tell "no API key" from "model 429" from "succeeded but the
/// output was off-contract", and record/telemeter the right thing — a bare
/// `anyhow::Result<T>` would erase that distinction.
#[derive(Debug, Clone)]
pub enum StructuredCallOutcome<T> {
    /// The forced tool call deserialised cleanly into `T`.
    Success(T),
    /// No API key was supplied; no network call was made.
    NoApiKey,
    /// Anthropic returned a non-2xx response. `status` is the numeric code;
    /// `snippet` is the first ~200 chars of the body.
    ApiError { status: u16, snippet: String },
    /// The HTTP client failed before/while getting a response (timeout, TLS,
    /// DNS, connection reset), or a 2xx body could not be decoded.
    Transport(String),
    /// A response arrived but the model did not call the tool, or the tool
    /// `input` did not deserialise into `T`. A validation failure, not a
    /// transport error.
    InvalidOutput(String),
}

impl<T> StructuredCallOutcome<T> {
    /// Short stable tag for logs / audit rows.
    pub fn tag(&self) -> &'static str {
        match self {
            StructuredCallOutcome::Success(_) => "success",
            StructuredCallOutcome::NoApiKey => "no_api_key",
            StructuredCallOutcome::ApiError { .. } => "api_error",
            StructuredCallOutcome::Transport(_) => "transport_error",
            StructuredCallOutcome::InvalidOutput(_) => "invalid_output",
        }
    }

    /// Human-readable detail for logs. `Success` carries no `T`-specific detail
    /// (that would need a `Display` bound); the caller logs the value itself.
    pub fn detail(&self) -> String {
        match self {
            StructuredCallOutcome::Success(_) => "structured output received".to_owned(),
            StructuredCallOutcome::NoApiKey => "ANTHROPIC_API_KEY not configured on the engine".to_owned(),
            StructuredCallOutcome::ApiError { status, snippet } => {
                format!("anthropic returned {status}: {snippet}")
            }
            StructuredCallOutcome::Transport(err) => err.clone(),
            StructuredCallOutcome::InvalidOutput(err) => err.clone(),
        }
    }
}

/// Assemble the typed [`MessagesRequest`] for a forced tool call. Public so
/// callers and tests can inspect the exact request the model will see.
pub fn build_request(spec: &StructuredCall) -> MessagesRequest {
    MessagesRequest::builder()
        .model(spec.model.as_str())
        .max_tokens(spec.max_tokens)
        .maybe_system(spec.system.clone())
        .messages(vec![Message::user(spec.user_prompt.clone())])
        // A single forced tool call IS the structured-output mechanism: the
        // model must call `tool_name`, whose `input` is the target type.
        .tools(json!([{
            "name": spec.tool_name,
            "description": spec.tool_description,
            "input_schema": spec.input_schema,
        }]))
        .tool_choice(json!({ "type": "tool", "name": spec.tool_name }))
        .maybe_output_config(spec.effort.as_ref().map(|e| json!({ "effort": e })))
        .build()
}

/// Run one structured call and deserialise the forced tool's output into `T`.
///
/// `api_key` is passed in (not read from config) so this stays a pure transform
/// — a `None` key short-circuits to [`StructuredCallOutcome::NoApiKey`] without
/// a network call, mirroring [`crate::planner::Planner::plan`]. The shared
/// [`crate::claude_client`] pipeline owns retry/backoff per `config`.
pub async fn structured_call<T: DeserializeOwned>(
    api_key: Option<&str>,
    spec: &StructuredCall,
    config: &CallConfig,
) -> StructuredCallOutcome<T> {
    let Some(api_key) = api_key else {
        return StructuredCallOutcome::NoApiKey;
    };
    let request = build_request(spec);
    match claude_client::send_messages(api_key, &request, config).await {
        Ok(response) => output_from_response(&response, &spec.tool_name),
        Err(err) => outcome_from_error(err),
    }
}

/// Pull the forced tool call's `input` out of the response and deserialise it
/// into `T`. A missing tool call or a schema mismatch is
/// [`StructuredCallOutcome::InvalidOutput`].
fn output_from_response<T: DeserializeOwned>(response: &MessagesResponse, tool_name: &str) -> StructuredCallOutcome<T> {
    let Some(input) = response.tool_use_input(tool_name) else {
        return StructuredCallOutcome::InvalidOutput(format!("model did not call the {tool_name} tool"));
    };
    match serde_json::from_value::<T>(input.clone()) {
        Ok(value) => StructuredCallOutcome::Success(value),
        Err(err) => {
            StructuredCallOutcome::InvalidOutput(format!("tool input did not match the expected schema: {err}"))
        }
    }
}

/// Map a shared [`ClaudeError`] into the matching outcome. Transport and decode
/// failures both mean "no usable bytes", so they bucket together.
fn outcome_from_error<T>(err: ClaudeError) -> StructuredCallOutcome<T> {
    match err {
        ClaudeError::Api { status, body } => StructuredCallOutcome::ApiError {
            status,
            snippet: clip(&body, 200),
        },
        ClaudeError::Transport(msg) | ClaudeError::Decode(msg) => StructuredCallOutcome::Transport(msg),
    }
}

/// Clip a string to `max` bytes on a char boundary, appending an ellipsis if
/// truncated. Bounds the error snippet stored in the outcome.
fn clip(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.len() + c.len_utf8() > max {
            out.push('…');
            return out;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::time::Duration;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
    struct Emitted {
        n: i64,
        label: String,
    }

    fn sample_spec() -> StructuredCall {
        StructuredCall {
            model: "claude-haiku-4-5-20251001".to_owned(),
            max_tokens: 256,
            effort: Some("high".to_owned()),
            system: Some("be precise".to_owned()),
            user_prompt: "emit something".to_owned(),
            tool_name: "emit".to_owned(),
            tool_description: "emit a value".to_owned(),
            input_schema: json!({
                "type": "object",
                "required": ["n", "label"],
                "additionalProperties": false,
                "properties": {
                    "n": { "type": "integer" },
                    "label": { "type": "string" }
                }
            }),
        }
    }

    #[test]
    fn build_request_forces_the_named_tool() {
        let request = build_request(&sample_spec());
        let body = serde_json::to_value(&request).expect("serialises");
        assert_eq!(body["model"], "claude-haiku-4-5-20251001");
        assert_eq!(body["max_tokens"], 256);
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["system"], "be precise");
        // Structured output is enforced via a forced tool call.
        assert_eq!(body["tool_choice"]["type"], "tool");
        assert_eq!(body["tool_choice"]["name"], "emit");
        assert_eq!(body["tools"][0]["name"], "emit");
        assert_eq!(body["tools"][0]["input_schema"]["required"][0], "n");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn build_request_omits_optional_blocks_when_unset() {
        let mut spec = sample_spec();
        spec.effort = None;
        spec.system = None;
        let body = serde_json::to_value(build_request(&spec)).expect("serialises");
        assert!(body.get("output_config").is_none());
        assert!(body.get("system").is_none());
        // Tools are always present — that is the whole point.
        assert!(body["tools"].is_array());
    }

    #[test]
    fn output_from_response_extracts_and_deserialises() {
        let response: MessagesResponse = serde_json::from_value(json!({
            "content": [
                { "type": "text", "text": "" },
                { "type": "tool_use", "name": "emit", "input": { "n": 7, "label": "ok" } }
            ]
        }))
        .unwrap();
        match output_from_response::<Emitted>(&response, "emit") {
            StructuredCallOutcome::Success(v) => {
                assert_eq!(
                    v,
                    Emitted {
                        n: 7,
                        label: "ok".into()
                    }
                );
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn output_from_response_rejects_missing_tool_call() {
        let response: MessagesResponse =
            serde_json::from_value(json!({ "content": [{ "type": "text", "text": "hi" }] })).unwrap();
        assert!(matches!(
            output_from_response::<Emitted>(&response, "emit"),
            StructuredCallOutcome::InvalidOutput(_),
        ));
    }

    #[test]
    fn output_from_response_rejects_off_schema_input() {
        // Missing the required `label` field → deserialisation fails.
        let response: MessagesResponse = serde_json::from_value(json!({
            "content": [{ "type": "tool_use", "name": "emit", "input": { "n": 7 } }]
        }))
        .unwrap();
        assert!(matches!(
            output_from_response::<Emitted>(&response, "emit"),
            StructuredCallOutcome::InvalidOutput(_),
        ));
    }

    #[tokio::test]
    async fn no_api_key_short_circuits_without_network() {
        let outcome: StructuredCallOutcome<Emitted> =
            structured_call(None, &sample_spec(), &CallConfig::new(Duration::from_secs(1))).await;
        assert!(matches!(outcome, StructuredCallOutcome::NoApiKey));
        assert_eq!(outcome.tag(), "no_api_key");
    }

    #[tokio::test]
    async fn end_to_end_success_against_mock_anthropic() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", claude_client::ANTHROPIC_API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{ "type": "tool_use", "name": "emit", "input": { "n": 42, "label": "answer" } }]
            })))
            .mount(&server)
            .await;

        let config = CallConfig::new(Duration::from_secs(5)).with_endpoint(format!("{}/v1/messages", server.uri()));
        let outcome: StructuredCallOutcome<Emitted> = structured_call(Some("test-key"), &sample_spec(), &config).await;
        match outcome {
            StructuredCallOutcome::Success(v) => assert_eq!(v.n, 42),
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn maps_api_error_to_typed_outcome() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        let config = CallConfig::new(Duration::from_secs(5)).with_endpoint(format!("{}/v1/messages", server.uri()));
        let outcome: StructuredCallOutcome<Emitted> = structured_call(Some("k"), &sample_spec(), &config).await;
        match outcome {
            StructuredCallOutcome::ApiError { status, .. } => assert_eq!(status, 401),
            other => panic!("expected ApiError, got {other:?}"),
        }
        assert_eq!(outcome.tag(), "api_error");
    }

    #[test]
    fn clip_truncates_on_char_boundary() {
        assert_eq!(clip("hello", 100), "hello");
        assert_eq!(clip("hello world", 5), "hello…");
    }
}
