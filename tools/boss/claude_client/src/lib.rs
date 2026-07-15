//! The single Claude (Anthropic Messages API) transport + protocol pipeline.
//!
//! # Every Claude call MUST go through this crate
//!
//! Any feature that talks to the Anthropic Messages API — planning,
//! pane summaries, live-status one-liners, the magic-wand doc editor, the
//! attentions backstop, and anything added later — MUST route through
//! [`send_messages`] (typed request) or [`send_messages_raw`] (a caller-built
//! JSON body, for structured-output/forced-tool-call requests). Do NOT stand
//! up a private `reqwest::Client`, re-declare the endpoint / `anthropic-version`
//! constants, hand-roll key resolution, or re-implement retry. The engine used
//! to carry five independent copies of that wiring; `boss-claude-client` is
//! the one place it lives.
//!
//! # Crate boundary
//!
//! This crate owns Claude API transport + protocol ONLY: client
//! construction, endpoint/version constants, key resolution, request/response
//! types, the error taxonomy, and retry/backoff. It must NEVER import from
//! the engine — that edge is one-way, engine → `boss-claude-client`. If
//! engine-trace logging or usage accounting is wanted later, it comes in via
//! a callback/trait parameter supplied by the caller, not an engine import.
//!
//! ## What the pipeline owns vs. what the caller owns
//!
//! The pipeline owns **transport + protocol**: the process-wide HTTP client
//! (with the `rustls` provider workaround installed in exactly one place), the
//! endpoint and `anthropic-version` header, the `x-api-key` header, the
//! request/response JSON shapes ([`MessagesRequest`] / [`MessagesResponse`]),
//! the [`ClaudeError`] taxonomy, and a [`RetryPolicy`] that retries transient
//! failures (HTTP 429, any 5xx including 529 "overloaded", and transport
//! errors) with exponential backoff.
//!
//! The caller owns **its feature**: model selection, system prompt, user
//! messages, `max_tokens`, temperature, any tool / `output_config` blocks, the
//! per-call [`CallConfig`] (timeout + retry), and all response
//! parsing/validation beyond the raw-transport layer. The pipeline hands back
//! a typed [`MessagesResponse`] plus small shared extraction helpers
//! ([`MessagesResponse::first_text`], [`MessagesResponse::tool_use_input`],
//! [`MessagesResponse::usage`]); feature-specific validation stays with the
//! feature.
//!
//! ## Key resolution
//!
//! [`resolve_api_key`] implements the shared precedence: an optional
//! per-feature override env var first, then [`DEFAULT_API_KEY_ENV`]
//! (`ANTHROPIC_API_KEY`). Features whose key arrives from `Config` rather than
//! the environment pass it straight to [`send_messages`] and skip this helper.

use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Anthropic Messages API endpoint. Hard-coded — nothing in this codebase
/// points at a non-prod Anthropic instance and a typo in an env override would
/// silently break every feature at once.
pub const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

/// Pinned `anthropic-version` header sent on every request.
pub const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// The base env var every feature falls back to for its API key.
pub const DEFAULT_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

// ── API key resolution ────────────────────────────────────────────────────────

/// Resolve an Anthropic API key from the environment.
///
/// `override_env` is an optional per-feature env var name (e.g.
/// `BOSS_MAGIC_WAND_API_KEY`, `BOSS_BACKSTOP_API_KEY`) that routes billing to a
/// separate spend bucket. If it is set (even to an empty string, matching the
/// prior `std::env::var(..).ok()` behaviour) its value wins; otherwise we fall
/// back to [`DEFAULT_API_KEY_ENV`]. Returns `None` when neither is set.
pub fn resolve_api_key(override_env: Option<&str>) -> Option<String> {
    resolve_api_key_from(override_env, DEFAULT_API_KEY_ENV, |name| std::env::var(name).ok())
}

/// Testable core of [`resolve_api_key`] with the environment lookup injected.
/// Kept private; unit tests drive it with an in-memory map so key-precedence
/// coverage never mutates the process environment.
fn resolve_api_key_from(
    override_env: Option<&str>,
    default_env: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    if let Some(name) = override_env
        && let Some(value) = lookup(name)
    {
        return Some(value);
    }
    lookup(default_env)
}

// ── Request types ─────────────────────────────────────────────────────────────

/// One Messages API message. All engine callers send a plain-string
/// `content`; the API's block-array form isn't needed here.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    /// A `user`-role message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_owned(),
            content: content.into(),
        }
    }
}

/// A typed Anthropic Messages API request. The transport-relevant fields
/// (`model`, `max_tokens`, `system`, `messages`, `temperature`) are typed;
/// the structured-output blocks (`tools`, `tool_choice`, `output_config`) are
/// caller-owned JSON so a feature can shape them however its schema needs
/// without the pipeline dictating their form. Callers building a fully custom
/// body (e.g. the planner's forced tool call) can bypass this type entirely
/// and use [`send_messages_raw`].
///
/// Uses `#[derive(bon::Builder)]` per the repo's more-than-5-field convention so an
/// additive field never forces every construction site to change.
#[derive(Debug, Clone, Serialize, bon::Builder)]
#[builder(on(String, into))]
pub struct MessagesRequest {
    /// Concrete model id (e.g. `claude-opus-4-8`). A direct API call needs a
    /// concrete id — the `--model` family aliases are resolved by the `claude`
    /// CLI, not the Messages API.
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Tool definitions, e.g. `[{ "name", "description", "input_schema" }]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Value>,
    /// `tool_choice`, e.g. `{ "type": "tool", "name": "…" }`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    /// `output_config`, e.g. `{ "effort": "high" }`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<Value>,
}

// ── Response types ────────────────────────────────────────────────────────────

/// A parsed Anthropic Messages API response. Fields default so a minimal
/// text-only response and a tool-use response both deserialize cleanly.
#[derive(Debug, Clone, Deserialize)]
pub struct MessagesResponse {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub stop_reason: Option<String>,
}

/// One content block. `block_type` discriminates `text` vs `tool_use`; the
/// tool-use fields (`name`, `input`, `id`) are absent on text blocks.
#[derive(Debug, Clone, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type", default)]
    pub block_type: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub input: Option<Value>,
    #[serde(default)]
    pub id: Option<String>,
}

/// Token accounting returned by the API.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
}

impl MessagesResponse {
    /// The text of the first `text` content block, if any. This is the
    /// first-text-block extraction shared by every free-text caller.
    pub fn first_text(&self) -> Option<&str> {
        self.content
            .iter()
            .find(|b| b.block_type == "text")
            .map(|b| b.text.as_str())
    }

    /// The `input` of the first `tool_use` block whose `name` matches
    /// `tool_name` — the extraction path for forced-tool-call structured
    /// output. The caller deserializes the returned JSON into its own schema.
    pub fn tool_use_input(&self, tool_name: &str) -> Option<&Value> {
        self.content
            .iter()
            .find(|b| b.block_type == "tool_use" && b.name.as_deref() == Some(tool_name))
            .and_then(|b| b.input.as_ref())
    }

    /// Token usage, or a zeroed [`Usage`] when the response omitted it.
    pub fn usage(&self) -> Usage {
        self.usage.unwrap_or_default()
    }
}

// ── Error taxonomy ────────────────────────────────────────────────────────────

/// A single transport/protocol-level failure. Feature code maps this into its
/// own outcome type (`PlannerOutcome`, `SummarizerOutcome`, an `error_kind`
/// tuple, …) — the pipeline deliberately does not know about those.
#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    /// A non-2xx response. `status` is the numeric code (401, 429, 529, …);
    /// `body` is the raw response body.
    #[error("anthropic returned {status}: {body}")]
    Api { status: u16, body: String },
    /// The HTTP client failed before/while getting a response (timeout, TLS,
    /// DNS, connection reset).
    #[error("transport error: {0}")]
    Transport(String),
    /// A 2xx response whose body could not be decoded into [`MessagesResponse`].
    #[error("failed to decode anthropic response: {0}")]
    Decode(String),
}

impl ClaudeError {
    /// Whether retrying could plausibly succeed: HTTP 429, any 5xx (including
    /// 529 "overloaded"), and transport errors are transient. A 2xx decode
    /// failure and 4xx (bad key/request) are not — retrying won't fix them.
    pub fn is_retryable(&self) -> bool {
        match self {
            ClaudeError::Api { status, .. } => *status == 429 || (500..=599).contains(status),
            ClaudeError::Transport(_) => true,
            ClaudeError::Decode(_) => false,
        }
    }

    /// The HTTP status code, if this was a non-2xx API error.
    pub fn status(&self) -> Option<u16> {
        match self {
            ClaudeError::Api { status, .. } => Some(*status),
            _ => None,
        }
    }
}

// ── Retry policy + per-call config ────────────────────────────────────────────

/// How many attempts to make and how long to back off between them. Backoff is
/// exponential: `base_backoff * 2^(attempt-1)` before the `attempt+1`th try,
/// applied only when the error [`is_retryable`](ClaudeError::is_retryable).
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts, `>= 1`. `1` means no retry.
    pub max_attempts: u32,
    /// Backoff before the first retry; doubles for each subsequent retry.
    pub base_backoff: Duration,
}

impl RetryPolicy {
    /// One attempt, no retry.
    pub const NONE: RetryPolicy = RetryPolicy {
        max_attempts: 1,
        base_backoff: Duration::ZERO,
    };

    pub const fn new(max_attempts: u32, base_backoff: Duration) -> Self {
        Self {
            max_attempts,
            base_backoff,
        }
    }
}

impl Default for RetryPolicy {
    /// A sane default for best-effort callers: two attempts with a short
    /// backoff, so a transient 429/5xx/overloaded is quietly retried once.
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 2,
            base_backoff: Duration::from_millis(500),
        }
    }
}

/// Per-call transport configuration. Owns the wall-clock timeout (applied
/// per attempt), the [`RetryPolicy`], and an optional endpoint override that
/// tests use to point at a mock server.
#[derive(Debug, Clone)]
pub struct CallConfig {
    pub timeout: Duration,
    pub retry: RetryPolicy,
    /// Endpoint override; `None` uses [`ANTHROPIC_MESSAGES_URL`].
    pub endpoint: Option<String>,
}

impl CallConfig {
    /// A config with the given per-attempt timeout and the default retry policy.
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            retry: RetryPolicy::default(),
            endpoint: None,
        }
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    fn url(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(ANTHROPIC_MESSAGES_URL)
    }
}

// ── The single HTTP client ────────────────────────────────────────────────────

/// The process-wide reqwest client shared by every Claude call. This is the
/// ONLY place in the engine that builds one, and the ONLY place that installs
/// the `rustls` crypto provider.
///
/// The client carries no default timeout — each request applies its own via
/// [`CallConfig::timeout`], so one client serves callers with wildly different
/// budgets (a 5 s live-status one-liner and a 180 s planning call).
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        // The workspace pins reqwest to `rustls-no-provider`, so a default
        // crypto provider must be installed before the first TLS handshake or
        // `Client::build` panics. `install_default` errors if one is already
        // set — that's fine, we ignore it.
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .build()
            .expect("reqwest::Client::build should not fail with default config")
    })
}

// ── Sending ───────────────────────────────────────────────────────────────────

/// Send a typed [`MessagesRequest`] and return the parsed response, retrying
/// transient failures per `config.retry`.
pub async fn send_messages(
    api_key: &str,
    request: &MessagesRequest,
    config: &CallConfig,
) -> Result<MessagesResponse, ClaudeError> {
    let body = serde_json::to_value(request).map_err(|err| ClaudeError::Decode(err.to_string()))?;
    send_messages_raw(api_key, &body, config).await
}

/// Send a caller-constructed JSON request body. Use this only when a feature
/// needs a body shape [`MessagesRequest`] doesn't model (e.g. a forced
/// tool-call structured-output request); otherwise prefer [`send_messages`].
pub async fn send_messages_raw(
    api_key: &str,
    body: &Value,
    config: &CallConfig,
) -> Result<MessagesResponse, ClaudeError> {
    let url = config.url();
    let attempts = config.retry.max_attempts.max(1);
    for attempt in 1..=attempts {
        match call_once(api_key, url, body, config.timeout).await {
            Ok(response) => return Ok(response),
            Err(err) if err.is_retryable() && attempt < attempts => {
                let delay = backoff_delay(&config.retry, attempt);
                tracing::warn!(
                    attempt,
                    max_attempts = attempts,
                    backoff_ms = delay.as_millis() as u64,
                    err = %err,
                    "claude_client: transient failure; retrying",
                );
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("retry loop always returns on the final attempt")
}

/// Backoff before the `attempt + 1`th try (1-based `attempt`).
fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    // attempt is always >= 1 here; cap the exponent so the shift can't overflow
    // even if a caller sets an unreasonable attempt count.
    let factor = 1u32.checked_shl(attempt - 1).unwrap_or(u32::MAX).max(1);
    policy.base_backoff.saturating_mul(factor)
}

/// One round trip: POST the body and parse the response.
async fn call_once(api_key: &str, url: &str, body: &Value, timeout: Duration) -> Result<MessagesResponse, ClaudeError> {
    let response = http_client()
        .post(url)
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_API_VERSION)
        .header("content-type", "application/json")
        .timeout(timeout)
        .json(body)
        .send()
        .await
        .map_err(|err| ClaudeError::Transport(err.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(ClaudeError::Api {
            status: status.as_u16(),
            body,
        });
    }
    response
        .json::<MessagesResponse>()
        .await
        .map_err(|err| ClaudeError::Decode(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── key resolution ────────────────────────────────────────────────────

    fn map_lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn key_precedence_prefers_override_when_set() {
        let lookup = map_lookup(&[("OVERRIDE", "override-key"), ("ANTHROPIC_API_KEY", "default-key")]);
        assert_eq!(
            resolve_api_key_from(Some("OVERRIDE"), "ANTHROPIC_API_KEY", &lookup).as_deref(),
            Some("override-key"),
        );
    }

    #[test]
    fn key_precedence_falls_back_to_default_when_override_unset() {
        let lookup = map_lookup(&[("ANTHROPIC_API_KEY", "default-key")]);
        assert_eq!(
            resolve_api_key_from(Some("OVERRIDE"), "ANTHROPIC_API_KEY", &lookup).as_deref(),
            Some("default-key"),
        );
    }

    #[test]
    fn key_precedence_uses_default_when_no_override_name_given() {
        let lookup = map_lookup(&[("OVERRIDE", "override-key"), ("ANTHROPIC_API_KEY", "default-key")]);
        assert_eq!(
            resolve_api_key_from(None, "ANTHROPIC_API_KEY", &lookup).as_deref(),
            Some("default-key"),
        );
    }

    #[test]
    fn key_precedence_returns_none_when_neither_set() {
        let lookup = map_lookup(&[]);
        assert_eq!(
            resolve_api_key_from(Some("OVERRIDE"), "ANTHROPIC_API_KEY", &lookup),
            None
        );
    }

    #[test]
    fn key_precedence_uses_empty_override_value() {
        // Matches the prior `std::env::var(..).ok()` behaviour: a set-but-empty
        // override var wins over the fallback rather than being skipped.
        let lookup = map_lookup(&[("OVERRIDE", ""), ("ANTHROPIC_API_KEY", "default-key")]);
        assert_eq!(
            resolve_api_key_from(Some("OVERRIDE"), "ANTHROPIC_API_KEY", &lookup).as_deref(),
            Some(""),
        );
    }

    // ── retry classification ──────────────────────────────────────────────

    #[test]
    fn retryable_classification() {
        assert!(
            ClaudeError::Api {
                status: 429,
                body: String::new()
            }
            .is_retryable()
        );
        assert!(
            ClaudeError::Api {
                status: 500,
                body: String::new()
            }
            .is_retryable()
        );
        assert!(
            ClaudeError::Api {
                status: 503,
                body: String::new()
            }
            .is_retryable()
        );
        // 529 "overloaded" — the case features used to drop.
        assert!(
            ClaudeError::Api {
                status: 529,
                body: String::new()
            }
            .is_retryable()
        );
        assert!(ClaudeError::Transport("connection reset".into()).is_retryable());
        // Not retryable:
        assert!(
            !ClaudeError::Api {
                status: 401,
                body: String::new()
            }
            .is_retryable()
        );
        assert!(
            !ClaudeError::Api {
                status: 400,
                body: String::new()
            }
            .is_retryable()
        );
        assert!(!ClaudeError::Decode("bad json".into()).is_retryable());
    }

    #[test]
    fn status_helper_reports_only_api_errors() {
        assert_eq!(
            ClaudeError::Api {
                status: 429,
                body: String::new()
            }
            .status(),
            Some(429)
        );
        assert_eq!(ClaudeError::Transport("x".into()).status(), None);
        assert_eq!(ClaudeError::Decode("x".into()).status(), None);
    }

    #[test]
    fn backoff_is_exponential() {
        let policy = RetryPolicy::new(4, Duration::from_millis(100));
        assert_eq!(backoff_delay(&policy, 1), Duration::from_millis(100));
        assert_eq!(backoff_delay(&policy, 2), Duration::from_millis(200));
        assert_eq!(backoff_delay(&policy, 3), Duration::from_millis(400));
    }

    // ── response extraction ───────────────────────────────────────────────

    fn parse(value: Value) -> MessagesResponse {
        serde_json::from_value(value).expect("valid MessagesResponse")
    }

    #[test]
    fn first_text_extracts_first_text_block() {
        let resp = parse(json!({
            "content": [
                { "type": "text", "text": "hello world" },
                { "type": "text", "text": "second" },
            ],
        }));
        assert_eq!(resp.first_text(), Some("hello world"));
    }

    #[test]
    fn first_text_is_none_without_a_text_block() {
        let resp = parse(json!({ "content": [{ "type": "tool_use", "name": "x", "input": {} }] }));
        assert_eq!(resp.first_text(), None);
    }

    #[test]
    fn tool_use_input_extracts_matching_tool() {
        let resp = parse(json!({
            "content": [
                { "type": "text", "text": "" },
                { "type": "tool_use", "id": "toolu_1", "name": "emit", "input": { "n": 7 } },
            ],
        }));
        assert_eq!(resp.tool_use_input("emit"), Some(&json!({ "n": 7 })));
        assert_eq!(resp.tool_use_input("other"), None);
    }

    #[test]
    fn usage_defaults_to_zero_when_absent() {
        let resp = parse(json!({ "content": [] }));
        assert_eq!(resp.usage().input_tokens, 0);
        assert_eq!(resp.usage().output_tokens, 0);
        let resp = parse(json!({ "content": [], "usage": { "input_tokens": 12, "output_tokens": 34 } }));
        assert_eq!(resp.usage().input_tokens, 12);
        assert_eq!(resp.usage().output_tokens, 34);
    }

    // ── request serialization ─────────────────────────────────────────────

    #[test]
    fn typed_request_serializes_and_omits_unset_fields() {
        let request = MessagesRequest::builder()
            .model("claude-sonnet-4-6")
            .max_tokens(60)
            .messages(vec![Message::user("hi")])
            .build();
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["max_tokens"], 60);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hi");
        // Optional fields left unset are omitted, not serialized as null.
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn typed_request_includes_system_and_tools_when_set() {
        let request = MessagesRequest::builder()
            .model("m")
            .max_tokens(10)
            .system("be terse")
            .messages(vec![Message::user("hi")])
            .tools(json!([{ "name": "t" }]))
            .output_config(json!({ "effort": "high" }))
            .build();
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["tools"][0]["name"], "t");
        assert_eq!(body["output_config"]["effort"], "high");
    }

    // ── end-to-end transport ──────────────────────────────────────────────

    fn text_request() -> MessagesRequest {
        MessagesRequest::builder()
            .model("m")
            .max_tokens(10)
            .messages(vec![Message::user("hi")])
            .build()
    }

    #[tokio::test]
    async fn send_messages_success_sets_headers_and_parses_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", ANTHROPIC_API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{ "type": "text", "text": "pong" }],
                "usage": { "input_tokens": 3, "output_tokens": 1 },
            })))
            .mount(&server)
            .await;

        let config = CallConfig::new(Duration::from_secs(5)).with_endpoint(format!("{}/v1/messages", server.uri()));
        let resp = send_messages("test-key", &text_request(), &config)
            .await
            .expect("success");
        assert_eq!(resp.first_text(), Some("pong"));
        assert_eq!(resp.usage().output_tokens, 1);
    }

    #[tokio::test]
    async fn retries_transient_5xx_then_succeeds() {
        let server = MockServer::start().await;
        // First call: 503 (retryable, consumed once). Second call: success.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{ "type": "text", "text": "ok" }],
            })))
            .mount(&server)
            .await;

        let config = CallConfig::new(Duration::from_secs(5))
            .with_retry(RetryPolicy::new(2, Duration::from_millis(1)))
            .with_endpoint(format!("{}/v1/messages", server.uri()));
        let resp = send_messages("k", &text_request(), &config)
            .await
            .expect("retry then success");
        assert_eq!(resp.first_text(), Some("ok"));
    }

    #[tokio::test]
    async fn does_not_retry_non_retryable_4xx() {
        let server = MockServer::start().await;
        // A 401 first (consumed once) then a 200. A retry would reach the 200;
        // proving we surface the 401 proves 4xx is not retried.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "content": [] })))
            .mount(&server)
            .await;

        let config = CallConfig::new(Duration::from_secs(5))
            .with_retry(RetryPolicy::new(3, Duration::from_millis(1)))
            .with_endpoint(format!("{}/v1/messages", server.uri()));
        let err = send_messages("k", &text_request(), &config).await.unwrap_err();
        match err {
            ClaudeError::Api { status, .. } => assert_eq!(status, 401),
            other => panic!("expected Api 401, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_error_on_unreachable_endpoint() {
        // Port 1 refuses connections → a transport error, no retry (fast).
        let config = CallConfig::new(Duration::from_secs(2))
            .with_retry(RetryPolicy::NONE)
            .with_endpoint("http://127.0.0.1:1/v1/messages");
        let err = send_messages("k", &text_request(), &config).await.unwrap_err();
        assert!(matches!(err, ClaudeError::Transport(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn send_messages_raw_extracts_tool_use() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{ "type": "tool_use", "name": "emit", "input": { "ok": true } }],
            })))
            .mount(&server)
            .await;
        let body = json!({ "model": "m", "max_tokens": 10, "messages": [{ "role": "user", "content": "hi" }] });
        let config = CallConfig::new(Duration::from_secs(5)).with_endpoint(format!("{}/v1/messages", server.uri()));
        let resp = send_messages_raw("k", &body, &config).await.expect("success");
        assert_eq!(resp.tool_use_input("emit"), Some(&json!({ "ok": true })));
    }
}
