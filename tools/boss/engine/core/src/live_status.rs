//! Live worker status summarizer.
//!
//! Given a tail of the worker's transcript JSONL, ask a cheap
//! summarizer model for a one-sentence description of what the worker
//! is doing right now. The result lands on
//! `LiveWorkerState::live_status` (set by [`crate::live_worker_state`]
//! via the registry update added in a follow-up PR) and is broadcast
//! to the macOS app on the existing `worker.live_states` topic, where
//! it renders under the Doing-card title and on the Agents-tab worker
//! header subtitle.
//!
//! Cost / latency target (see `tools/boss/docs/designs/worker-live-status.md`):
//!
//! - Model: Haiku 4.5 (`claude-haiku-4-5-20251001`). Sonnet would buy
//!   slightly cleaner phrasing for ~3× the price and Haiku produces
//!   the gerund-style sentence we want with no fuss.
//! - Input cap: 800 tokens. The redactor trims oldest-first if the
//!   transcript window overflows.
//! - Output cap: 120 tokens. The reply is a forced tool call carrying
//!   a ≤ 8 word phrase (or a one-sentence "insufficient input" reason);
//!   the cap exists to catch a runaway response.
//! - Timeout: 5 s. P99 is usually closer to 2 s; we'd rather keep
//!   the prior value than block the loop.
//! - Budget: ≤ $1/hour at 8 fully-busy workers — see the design doc's
//!   Q3 for the arithmetic.
//!
//! Privacy is layered:
//!
//! 1. Pre-summarizer redaction via [`boss_engine_live_status_redact`] —
//!    drop deny-listed entries, truncate large values, then run the
//!    secret-pattern regexes over the assembled text.
//! 2. Prompt guardrails — the system message forbids quoting literal
//!    values longer than four words and enumerates the kinds of
//!    strings that must never appear verbatim.
//! 3. Post-output filter — same redactor regexes run over the
//!    model's reply; if the result is empty or > 90 % redaction
//!    markers, we drop it and keep the prior value.
//!
//! Failure modes keep the prior label on purpose. A flickering label
//! that empties on every transient API hiccup is worse than a label
//! that's two minutes old. They are not silent: every non-success
//! outcome is a typed [`SummarizerOutcome`] that the slot loop logs and
//! exposes through the live-status debug verb.
//!
//! The model never answers in free text. The call forces a single tool
//! ([`STATUS_TOOL_NAME`]) whose input is either a `status` phrase or an
//! explicit `insufficient_input` flag with a reason. That is what keeps
//! the model's own meta-reply ("I don't have enough context to tell…")
//! off the kanban card: it arrives as a structured outcome the engine
//! logs as an error, not as a sentence that happens to look like one.

use std::time::Duration;

use serde_json::Value;

use boss_engine_live_status_redact as live_status_redact;

use crate::claude_client::{self, CallConfig, ClaudeError, Message, MessagesRequest};
use crate::utility_model::{UtilityCall, UtilityModel, UtilityTask};

/// Haiku 4.5 is the right shape for a one-sentence cheap summary —
/// see Q3 in the design doc.
///
/// This is the *default*; the model actually used comes from the
/// [`UtilityModel`] provider, so an operator can point the summarizer at a
/// different small model without touching any work item's driver. Kept as a
/// named constant because the tests and the design doc both refer to it.
pub const SUMMARY_MODEL: &str = UtilityTask::LiveStatus.default_model();

/// Output ceiling. The forced tool call wraps a ≤ 8 word phrase (or a
/// one-sentence insufficient-input reason) in a small JSON object; this
/// is the safety net for a runaway reply.
pub const SUMMARY_MAX_TOKENS: u32 = 120;

/// Worst-case wall-clock budget for the API round trip. P99 on Haiku
/// for ~800 input tokens is comfortably under this.
pub const SUMMARY_TIMEOUT: Duration = Duration::from_secs(5);

/// Bytes of redacted prompt text we'll feed to the model per call.
/// Roughly ~800 tokens at the API's 4-bytes-per-token heuristic,
/// matching the design's input cap. Oldest entries get dropped first
/// when the transcript window overflows.
pub const MAX_PROMPT_BYTES: usize = 3_200;

/// Maximum number of transcript JSONL entries we'll consider per
/// tick before redaction. Bounds the work the redactor does on
/// chatty workers; 30 covers the most recent few turns.
pub const MAX_TRANSCRIPT_ENTRIES: usize = 30;

/// Maximum render length we'll write to `live_status`. Hard-caps the
/// kanban card subtitle to a single line at default card width (~60 chars).
pub const MAX_LIVE_STATUS_LEN: usize = 60;

/// Byte cap on the logged `insufficient_input` reason, ellipsis included.
/// The reason is redacted first, then clipped so a secret straddling
/// this cut cannot leak a prefix into the log.
pub const INSUFFICIENT_INPUT_REASON_MAX_BYTES: usize = 200;

/// Literal string used by the trigger fan-in when `activity` flips
/// to `WaitingForInput` and no prior summary is available. Written
/// directly to `live_status` without a model call.
pub const AWAITING_INPUT_LITERAL: &str = "awaiting input";

/// Literal written when `activity` flips to `WaitingOnWork` and no useful
/// prior summary exists: the worker is parked on a wait it declared itself,
/// so it must never read as needing a person.
pub const WAITING_ON_WORK_LITERAL: &str = "waiting on its own work";

/// Literal written when `activity` flips to `WaitOverdue`: the declared wait
/// expired and the worker was not re-woken.
pub const WAIT_OVERDUE_LITERAL: &str = "wait expired — check on it";

/// Project state literals from the authoritative activity, preserving narrative summaries.
pub fn activity_subtitle(activity: boss_protocol::WorkerActivity, prior: Option<&str>) -> Option<String> {
    if let Some(text) = prior
        && !text.is_empty()
        && ![AWAITING_INPUT_LITERAL, WAITING_ON_WORK_LITERAL, WAIT_OVERDUE_LITERAL].contains(&text)
    {
        return Some(text.to_owned());
    }
    match activity {
        boss_protocol::WorkerActivity::WaitingForInput => Some(AWAITING_INPUT_LITERAL.to_owned()),
        boss_protocol::WorkerActivity::WaitingOnWork => Some(WAITING_ON_WORK_LITERAL.to_owned()),
        boss_protocol::WorkerActivity::WaitOverdue => Some(WAIT_OVERDUE_LITERAL.to_owned()),
        _ => None,
    }
}

/// Literal string written when `activity` flips to `Errored`.
pub const ERRORED_LITERAL: &str = "errored — check logs";

/// Name of the forced tool the summarizer must call. Its input schema
/// ([`status_tool_schema`]) is the only reply shape the engine accepts.
pub const STATUS_TOOL_NAME: &str = "report_status";

/// Input schema of [`STATUS_TOOL_NAME`]. Exactly one of the two
/// branches is meaningful per reply: a `status` phrase, or
/// `insufficient_input: true` with a `reason`. The engine reads the flag
/// first, so a model that sets it and also fills `status` with its
/// explanation never gets that explanation onto the card.
pub fn status_tool_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "status": {
                "type": "string",
                "description": "A 4–8 word present-continuous phrase, no subject, describing what the worker is doing right now (e.g. \"Reading work-board header view\"). Leave empty when insufficient_input is true.",
            },
            "insufficient_input": {
                "type": "boolean",
                "description": "true only when the transcript tail shows no worker activity to describe. Never explain this in `status`.",
            },
            "reason": {
                "type": "string",
                "description": "When insufficient_input is true: one short sentence naming what the tail lacks.",
            },
        },
        "required": ["insufficient_input"],
        "additionalProperties": false,
    })
}

/// Tool definitions block for the summarizer request: the single forced
/// tool, shaped as the Messages API expects.
pub fn status_tool_definitions() -> Value {
    serde_json::json!([{
        "name": STATUS_TOOL_NAME,
        "description": "Report the worker's current activity for its status line, or report that the transcript tail has too little activity to tell.",
        "input_schema": status_tool_schema(),
    }])
}

/// Distinguishable outcomes for one summarizer call. Returned by
/// [`summarize_transcript`] so the trigger fan-in can both log the
/// reason and surface it through the live-status debug verb. Previously
/// every failure path returned `None`, which made it impossible to
/// tell "missing API key" apart from "transcript empty after
/// redaction" or "Anthropic returned 429" — all three look identical
/// from the outside, all logged at `warn` at best, and all three are
/// silent failure modes the chore wants observable.
#[derive(Debug, Clone)]
pub enum SummarizerOutcome {
    /// Model returned a one-sentence summary that passed the post-filter.
    /// Caller writes this to `live_status`.
    Success(String),
    /// The [`UtilityModel`] provider could not produce a credential for
    /// [`UtilityTask::LiveStatus`], so no summarizer call can ever succeed.
    /// The payload is the provider's rendered reason, which names the env
    /// vars it consulted. The trigger fan-in surfaces this through the debug
    /// verb and (one-time, at startup) logs at `error` so the user notices.
    NoApiKey(String),
    /// Transcript tail had content but every entry was deny-listed or
    /// fully redacted. Benign — happens early in a worker's life or
    /// when the transcript is dominated by tool reads of secret files.
    EmptyAfterRedaction,
    /// Anthropic returned a non-2xx response. `status` is the numeric
    /// code (e.g. 401, 429, 529). `snippet` is the first ~120 chars of
    /// the response body, with secret-pattern redaction applied so a
    /// rogue error message can't leak credentials into the log.
    ApiError { status: u16, snippet: String },
    /// The HTTP client failed before getting a response (timeout, TLS
    /// handshake failure, DNS, connection reset).
    Transport(String),
    /// The model replied but the post-filter dropped the result —
    /// either fully-redacted or empty after trim. Caller keeps prior.
    PostFilterDropped,
    /// The model reported, through the forced tool's
    /// `insufficient_input` flag, that the tail it was shown carried
    /// nothing it could describe. The payload is the model's (redacted,
    /// clipped) reason plus what the engine actually sent, so the log line
    /// names the cause. This is an engine error, not a status: the slot
    /// loop keeps the prior label and never renders the reason.
    InsufficientInput {
        reason: String,
        input_lines: usize,
        input_bytes: usize,
    },
    /// The model's reply did not carry the forced tool call, or its input
    /// did not match [`status_tool_schema`]. Payload is a short
    /// description of what came back instead.
    MalformedReply(String),
}

impl SummarizerOutcome {
    /// The short tag rendered in logs and in the debug verb output —
    /// matches the names the chore enumerates.
    pub fn tag(&self) -> &'static str {
        match self {
            SummarizerOutcome::Success(_) => "success",
            SummarizerOutcome::NoApiKey(_) => "no_api_key",
            SummarizerOutcome::EmptyAfterRedaction => "empty_after_redaction",
            SummarizerOutcome::ApiError { .. } => "api_error",
            SummarizerOutcome::Transport(_) => "transport_error",
            SummarizerOutcome::PostFilterDropped => "post_filter_dropped",
            SummarizerOutcome::InsufficientInput { .. } => "insufficient_input",
            SummarizerOutcome::MalformedReply(_) => "malformed_reply",
        }
    }

    /// Human-readable detail for the debug verb. Returned alongside
    /// `tag` so a JSON consumer can pick the structured tag and still
    /// render the verbose form.
    pub fn detail(&self) -> String {
        match self {
            SummarizerOutcome::Success(text) => {
                // First 80 chars so the verb output stays single-line.
                let clip: String = text.chars().take(80).collect();
                if clip.len() < text.len() {
                    format!("{clip}…")
                } else {
                    clip
                }
            }
            SummarizerOutcome::NoApiKey(reason) => reason.clone(),
            SummarizerOutcome::EmptyAfterRedaction => {
                "transcript empty after deny-list + secret-pattern redaction".to_owned()
            }
            SummarizerOutcome::ApiError { status, snippet } => {
                format!("anthropic returned {status}: {snippet}")
            }
            SummarizerOutcome::Transport(err) => err.clone(),
            SummarizerOutcome::PostFilterDropped => {
                "model reply rejected by post-filter (empty / mostly redacted)".to_owned()
            }
            SummarizerOutcome::InsufficientInput {
                reason,
                input_lines,
                input_bytes,
            } => format!(
                "model reported insufficient input over {input_lines} transcript lines / {input_bytes} redacted bytes: {reason}"
            ),
            SummarizerOutcome::MalformedReply(what) => {
                format!("model reply did not carry a valid {STATUS_TOOL_NAME} call: {what}")
            }
        }
    }
}

/// Top-level entry: redact `transcript_lines`, build a prompt, call
/// the model, post-filter the response. Returns a typed outcome so
/// the trigger fan-in can distinguish "no api key" from "model 429"
/// from "transcript empty after redaction" — see [`SummarizerOutcome`].
///
/// The caller keeps the prior `live_status` value on every non-success
/// outcome.
pub async fn summarize_transcript(utility: &dyn UtilityModel, transcript_lines: &[Value]) -> SummarizerOutcome {
    let call = match utility.resolve(UtilityTask::LiveStatus) {
        Ok(call) => call,
        Err(err) => {
            tracing::error!(%err, "live_status: summarizer skipped — no utility-model credential");
            return SummarizerOutcome::NoApiKey(err.to_string());
        }
    };
    let redacted = redact_and_assemble(transcript_lines);
    if redacted.trim().is_empty() {
        tracing::debug!("live_status: transcript empty after redaction");
        return SummarizerOutcome::EmptyAfterRedaction;
    }
    match claude_one_sentence(&call, &redacted).await {
        Ok(ClaudeReply::Success(summary)) => SummarizerOutcome::Success(summary),
        Ok(ClaudeReply::PostFilterDropped) => {
            tracing::warn!("live_status: post-filter dropped the model reply",);
            SummarizerOutcome::PostFilterDropped
        }
        Ok(ClaudeReply::InsufficientInput { reason }) => {
            // The input is the engine's to account for: say what was sent
            // alongside the model's reason, so the cause (a tail with no
            // activity, or an input path that dropped it) is in the log.
            let input_lines = redacted.lines().count();
            let input_bytes = redacted.len();
            tracing::error!(
                reason = %reason,
                input_lines,
                input_bytes,
                transcript_entries = transcript_lines.len(),
                "live_status: summarizer reported insufficient input; keeping prior status",
            );
            SummarizerOutcome::InsufficientInput {
                reason,
                input_lines,
                input_bytes,
            }
        }
        Ok(ClaudeReply::Malformed(what)) => {
            tracing::error!(what = %what, "live_status: summarizer reply did not match the status tool schema");
            SummarizerOutcome::MalformedReply(what)
        }
        Err(ClaudeError::Api { status, body }) => {
            // Redact the body before logging so an error response
            // containing the key (some Anthropic 401 bodies echo
            // headers) can't leak into engine stderr.
            let snippet = live_status_redact::redact_text(&boss_engine_utils::string_clip::clip_to_bytes(&body, 120));
            tracing::error!(
                status,
                snippet = %snippet,
                "live_status: anthropic returned non-2xx",
            );
            SummarizerOutcome::ApiError { status, snippet }
        }
        Err(ClaudeError::Transport(msg)) => {
            tracing::error!(err = %msg, "live_status: transport error");
            SummarizerOutcome::Transport(msg)
        }
        Err(ClaudeError::Decode(msg)) => {
            tracing::error!(err = %msg, "live_status: failed to decode anthropic response");
            SummarizerOutcome::Transport(msg)
        }
    }
}

/// Apply [`live_status_redact`] to a transcript tail and assemble the
/// remaining text into the single prompt body we feed to the model.
/// Oldest entries drop first when the trimmed prompt exceeds
/// [`MAX_PROMPT_BYTES`].
///
/// The shape is deliberately simple — one line per surviving entry,
/// `kind: short summary` — so the model sees event boundaries and
/// can't confuse two adjacent tool calls.
pub fn redact_and_assemble(transcript_lines: &[Value]) -> String {
    let mut rendered: Vec<String> = Vec::new();
    let start = transcript_lines.len().saturating_sub(MAX_TRANSCRIPT_ENTRIES);
    for line in &transcript_lines[start..] {
        if live_status_redact::should_drop_entry(line) {
            continue;
        }
        let truncated = live_status_redact::truncate_large_values(line.clone());
        let summary = render_entry(&truncated);
        if summary.trim().is_empty() {
            continue;
        }
        rendered.push(live_status_redact::redact_text(&summary));
    }
    // Trim oldest-first until we fit under the input cap. We render
    // the body bottom-up to keep the freshest events.
    let mut total: usize = rendered.iter().map(|s| s.len() + 1).sum();
    let mut start = 0usize;
    while total > MAX_PROMPT_BYTES && start < rendered.len() {
        total -= rendered[start].len() + 1;
        start += 1;
    }
    rendered[start..].join("\n")
}

/// Compact a single transcript JSONL entry into a one-line summary
/// we can hand to the model. The claude transcript format varies
/// across message kinds; we only need to surface the rough action
/// shape (assistant text, tool use, tool result).
fn render_entry(line: &Value) -> String {
    let entry_type = line.get("type").and_then(Value::as_str).unwrap_or("unknown");
    match entry_type {
        "user" => line
            .get("text")
            .and_then(Value::as_str)
            .map(|text| {
                let one_line = text.trim().replace('\n', " ");
                format!(
                    "user: {}",
                    boss_engine_utils::string_clip::clip_to_bytes(&one_line, 200)
                )
            })
            .unwrap_or_else(|| "user: prompt".to_owned()),
        "assistant" => render_assistant(line),
        "tool_result" => {
            let name = line.get("tool_name").and_then(Value::as_str).unwrap_or("tool");
            let verb = if line.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                "failed"
            } else {
                "returned"
            };
            let output = line
                .get("content")
                .and_then(compact_value_text)
                .map(|text| boss_engine_utils::string_clip::clip_to_bytes(&text, 200));
            match output {
                Some(output) if !output.is_empty() => format!("tool: {name} {verb} {output}"),
                _ => format!("tool: {name} {verb}"),
            }
        }
        // Codex lifecycle fillers (`task_started` / bare `task_complete` /
        // `turn_aborted`) are normalized as system so they don't masquerade as
        // worker prose for marker scans. Still surface a short line so the
        // live-status summary keeps turn-boundary breadcrumbs.
        "system" => line
            .get("message")
            .and_then(Value::as_str)
            .map(|msg| {
                let one_line = msg.trim().replace('\n', " ");
                format!(
                    "system: {}",
                    boss_engine_utils::string_clip::clip_to_bytes(&one_line, 200)
                )
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn compact_value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().replace('\n', " ")),
        Value::Array(values) => {
            let joined = values
                .iter()
                .filter_map(compact_value_text)
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            (!joined.is_empty()).then_some(joined)
        }
        Value::Object(object) => ["text", "stdout", "stderr", "content", "output"]
            .into_iter()
            .find_map(|key| object.get(key).and_then(compact_value_text)),
        _ => None,
    }
}

fn render_assistant(line: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    let message = line.get("message").unwrap_or(line);
    let content = message.get("content").and_then(Value::as_array);
    if let Some(blocks) = content {
        for block in blocks {
            let Some(obj) = block.as_object() else { continue };
            let block_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
            match block_type {
                "text" => {
                    if let Some(t) = obj.get("text").and_then(Value::as_str) {
                        let one_line = t.trim().replace('\n', " ");
                        parts.push(format!(
                            "assistant: {}",
                            boss_engine_utils::string_clip::clip_to_bytes(&one_line, 200)
                        ));
                    }
                }
                "tool_use" => {
                    let name = obj.get("name").and_then(Value::as_str).unwrap_or("Tool");
                    // For Bash, surface the first ~80 chars of the
                    // command. For other tools, the name alone is
                    // typically enough signal — the model just needs
                    // to know "the worker ran Edit, then Bash".
                    let arg = if name == "Bash" {
                        obj.get("input")
                            .and_then(|i| i.get("command"))
                            .and_then(Value::as_str)
                            .map(|c| boss_engine_utils::string_clip::clip_to_bytes(c, 80))
                    } else {
                        obj.get("input")
                            .and_then(|i| i.get("file_path"))
                            .and_then(Value::as_str)
                            .map(|c| boss_engine_utils::string_clip::clip_to_bytes(c, 80))
                    };
                    if let Some(arg) = arg {
                        parts.push(format!("tool: {name}({arg})"));
                    } else {
                        parts.push(format!("tool: {name}"));
                    }
                }
                "thinking" => {
                    if let Some(t) = obj.get("thinking").and_then(Value::as_str) {
                        let one_line = t.trim().replace('\n', " ");
                        parts.push(format!(
                            "thinking: {}",
                            boss_engine_utils::string_clip::clip_to_bytes(&one_line, 200)
                        ));
                    }
                }
                _ => {}
            }
        }
    } else if let Some(t) = message.get("text").and_then(Value::as_str) {
        parts.push(format!(
            "assistant: {}",
            boss_engine_utils::string_clip::clip_to_bytes(t, 200)
        ));
    }
    parts.join(" | ")
}

/// Build the prompt for the model. The system message carries the
/// guardrails; the user message carries the redacted transcript and
/// the explicit ask.
pub fn build_messages(transcript: &str) -> (String, String) {
    let system = SYSTEM_PROMPT.to_owned();
    let mut user = String::new();
    user.push_str(
        "Below is a redacted tail of the worker's transcript. Lines may be \
         truncated (marked <truncated>) and any sensitive substrings are \
         replaced with <redacted>; treat both markers as censored values, not \
         as the worker's words. Call report_status with a 4–8 word verb-led \
         phrase (no subject) describing what the worker is doing right now. \
         Lines starting with `tool:` are tool calls the worker made and their \
         results; a tail made only of tool calls is normal and describes the \
         worker's activity. Do not quote literal values longer than two words \
         from the transcript. Do not include file paths, URLs, tokens, keys, \
         or anything that looks like a password. Only if the tail shows no \
         worker activity at all, set insufficient_input to true with a short \
         reason instead of a status.\n\nTranscript tail:\n",
    );
    user.push_str(transcript);
    (system, user)
}

const SYSTEM_PROMPT: &str = "\
You label a coding worker's current activity for a kanban card subtitle. \
Output a 4–8 word verb-led phrase — no subject, no trailing period. \
Examples of the exact style required:\n\
- \"Reading work-board header view\"\n\
- \"Grepping for repo badge rendering\"\n\
- \"Editing card layout\"\n\
- \"Running bazel build\"\n\
- \"Waiting on CI\"\n\
- \"Investigating scroll handler bug\"\n\
Strict rules:\n\
- 4–8 words max, present-continuous verb first, no leading subject.\n\
- Describe the *action*, not the *content* — \"Reading auth config\" \
not \"reading file containing <value>\".\n\
- Never quote any literal value longer than two words from the input.\n\
- Never include file paths, URLs, API tokens, keys, passwords, or \
strings that look like secrets.\n\
- Always reply by calling the report_status tool. Put the phrase in \
`status`. If the tail shows no worker activity at all, set \
`insufficient_input` to true and give a one-sentence `reason` instead; \
never write an explanation into `status`.\
";

/// Internal reply shape from [`claude_one_sentence`]. Success carries
/// the cleaned summary; `PostFilterDropped` distinguishes "Anthropic
/// returned a response but the post-filter rejected it" from any
/// transport-level error; `InsufficientInput` and `Malformed` are the
/// two ways a well-formed HTTP response can still carry no status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeReply {
    Success(String),
    PostFilterDropped,
    /// The forced tool call set `insufficient_input`. The reason has
    /// been through the secret-pattern redactor and clipped to at most
    /// [`INSUFFICIENT_INPUT_REASON_MAX_BYTES`] (ellipsis included).
    InsufficientInput {
        reason: String,
    },
    /// No forced tool call in the reply, or an input the schema does
    /// not describe.
    Malformed(String),
}

/// Ask for a one-sentence summary via the shared [`crate::claude_client`]
/// pipeline, addressed at whatever the [`UtilityModel`] provider resolved.
/// Returns `ClaudeReply` (success vs. post-filter-dropped) on a successful
/// round trip, or the shared [`ClaudeError`] which `summarize_transcript` maps
/// into the matching [`SummarizerOutcome`] — the surface needs to distinguish
/// "model 429" from "TLS handshake failed", which a bare
/// `anyhow::Result<String>` would erase.
pub async fn claude_one_sentence(call: &UtilityCall, transcript: &str) -> Result<ClaudeReply, ClaudeError> {
    let (system, user) = build_messages(transcript);
    let request = MessagesRequest::builder()
        .model(call.model.clone())
        .max_tokens(SUMMARY_MAX_TOKENS)
        .system(system)
        .messages(vec![Message::user(user)])
        .tools(status_tool_definitions())
        .tool_choice(serde_json::json!({"type": "tool", "name": STATUS_TOOL_NAME}))
        .build();
    let config = CallConfig::new(SUMMARY_TIMEOUT).with_endpoint(call.endpoint.clone());
    let response = claude_client::send_messages(&call.api_key, &request, &config).await?;
    let Some(input) = response.tool_use_input(STATUS_TOOL_NAME) else {
        let got = response
            .content
            .iter()
            .map(|block| block.block_type.as_str())
            .collect::<Vec<_>>()
            .join(",");
        return Ok(ClaudeReply::Malformed(format!(
            "no {STATUS_TOOL_NAME} tool_use block; content block types: [{got}]"
        )));
    };
    Ok(parse_status_reply(input))
}

/// Redact-then-clip bound for [`ClaudeReply::InsufficientInput`]: the
/// returned string is at most [`INSUFFICIENT_INPUT_REASON_MAX_BYTES`]
/// including the ellipsis `clip_to_bytes` appends on truncation.
fn clip_insufficient_input_reason(redacted: &str) -> String {
    if redacted.len() <= INSUFFICIENT_INPUT_REASON_MAX_BYTES {
        return redacted.to_owned();
    }
    boss_engine_utils::string_clip::clip_to_bytes(redacted, INSUFFICIENT_INPUT_REASON_MAX_BYTES - '…'.len_utf8())
}

/// Interpret the forced tool call's input. The `insufficient_input`
/// flag wins over any `status` text, so an explanation the model also
/// typed into `status` can never be mistaken for a label.
pub fn parse_status_reply(input: &Value) -> ClaudeReply {
    let Some(object) = input.as_object() else {
        return ClaudeReply::Malformed(format!("tool input is not an object: {}", kind_of(input)));
    };
    match object.get("insufficient_input") {
        Some(Value::Bool(true)) => {
            let raw = object
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("(no reason given)");
            let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
            let reason = clip_insufficient_input_reason(&live_status_redact::redact_text(&one_line));
            return ClaudeReply::InsufficientInput { reason };
        }
        Some(Value::Bool(false)) | None => {}
        Some(other) => {
            return ClaudeReply::Malformed(format!(
                "insufficient_input is {} rather than a boolean",
                kind_of(other)
            ));
        }
    }
    match object.get("status") {
        Some(Value::String(status)) => {
            let cleaned = clean_summary(status);
            if cleaned.is_empty() {
                ClaudeReply::PostFilterDropped
            } else {
                ClaudeReply::Success(cleaned)
            }
        }
        Some(other) => ClaudeReply::Malformed(format!("status is {} rather than a string", kind_of(other))),
        None => ClaudeReply::Malformed("neither status nor insufficient_input was set".to_owned()),
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Post-process the model reply: trim whitespace and quotes, drop a
/// trailing period, apply the same secret-pattern redactor over the
/// output, and reject anything that ends up mostly redacted (the
/// salvage check from [`live_status_redact::is_mostly_redacted`]).
///
/// Returns an empty string on rejection so the caller can fall back
/// to "keep prior".
pub fn clean_summary(raw: &str) -> String {
    let stripped = boss_engine_utils::json_extract::strip_wrapping_quotes(raw);
    let redacted = live_status_redact::redact_text(stripped);
    if live_status_redact::is_mostly_redacted(&redacted) {
        return String::new();
    }
    // Single-line shape. The UI renders this on a single line; any
    // model-injected newline becomes a space.
    let one_line = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    boss_engine_utils::string_clip::clip_to_bytes(&one_line, MAX_LIVE_STATUS_LEN)
}

/// Mock-response builders shared by every test that stands in for the
/// summarizer endpoint. The engine only accepts the forced
/// [`STATUS_TOOL_NAME`] call, so a mock that answered in free text would
/// exercise the malformed-reply path rather than the one under test.
#[cfg(test)]
pub(crate) mod test_support {
    use super::STATUS_TOOL_NAME;
    use serde_json::{Value, json};

    /// A Messages API body carrying `status` through the forced tool.
    pub(crate) fn status_reply(status: &str) -> Value {
        json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_status",
                "name": STATUS_TOOL_NAME,
                "input": {"insufficient_input": false, "status": status},
            }],
            "stop_reason": "tool_use",
        })
    }

    /// A Messages API body in which the model declines to label the tail.
    pub(crate) fn insufficient_input_reply(reason: &str) -> Value {
        json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_status",
                "name": STATUS_TOOL_NAME,
                "input": {"insufficient_input": true, "reason": reason},
            }],
            "stop_reason": "tool_use",
        })
    }
}

#[cfg(test)]
#[path = "live_status_codex_input_tests.rs"]
mod codex_input_tests;

#[cfg(test)]
mod tests {
    use super::test_support::{insufficient_input_reply, status_reply};
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn assistant_text(text: &str) -> Value {
        json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": text}],
            },
        })
    }

    fn assistant_tool_use(name: &str, input: Value) -> Value {
        json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "tool_use", "name": name, "input": input}],
            },
        })
    }

    #[test]
    fn render_entry_handles_assistant_text() {
        let line = assistant_text("Investigating the scroll handler bug");
        let s = render_entry(&line);
        assert!(s.contains("assistant"));
        assert!(s.contains("Investigating"));
    }

    #[test]
    fn render_entry_summarises_bash_tool_use_with_command_prefix() {
        let line = assistant_tool_use("Bash", json!({"command": "cargo test -p boss-engine"}));
        let s = render_entry(&line);
        assert!(s.contains("Bash"), "got {s}");
        assert!(s.contains("cargo test"), "got {s}");
    }

    #[test]
    fn render_entry_summarises_edit_tool_use_with_file_path() {
        let line = assistant_tool_use("Edit", json!({"file_path": "tools/boss/engine/src/app.rs"}));
        let s = render_entry(&line);
        assert!(s.contains("Edit"));
        assert!(s.contains("app.rs"));
    }

    #[test]
    fn render_entry_includes_user_text_and_tool_output() {
        assert_eq!(
            render_entry(&json!({"type":"user","text":"please inspect the parser"})),
            "user: please inspect the parser"
        );
        assert_eq!(
            render_entry(&json!({
                "type":"tool_result",
                "tool_name":"Bash",
                "content":"all tests passed\n"
            })),
            "tool: Bash returned all tests passed"
        );
        assert_eq!(
            render_entry(&json!({
                "type":"tool_result",
                "tool_name":"Bash",
                "content":"command exited 1\n",
                "is_error":true
            })),
            "tool: Bash failed command exited 1"
        );
        assert_eq!(
            render_entry(&json!({"type":"tool_result","tool_name":"Bash","content":"","is_error":true})),
            "tool: Bash failed"
        );
    }

    #[test]
    fn redact_and_assemble_drops_deny_listed_entries() {
        let lines = vec![
            assistant_tool_use("Read", json!({"file_path": "/Users/x/.ssh/id_rsa"})),
            assistant_text("Looking at the test failure"),
        ];
        let out = redact_and_assemble(&lines);
        // SSH read got dropped; assistant text survived.
        assert!(!out.contains("id_rsa"), "out = {out}");
        assert!(out.contains("test failure"), "out = {out}");
    }

    #[test]
    fn redact_and_assemble_applies_secret_pattern_to_assistant_text() {
        let lines = vec![assistant_text("Token is ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345 from env")];
        let out = redact_and_assemble(&lines);
        assert!(out.contains("<redacted>"));
        assert!(!out.contains("ghp_"));
    }

    #[test]
    fn redact_and_assemble_trims_oldest_when_over_cap() {
        // Build > MAX_PROMPT_BYTES of bland assistant text. Older
        // entries should drop first; the freshest should survive.
        let mut lines: Vec<Value> = (0..50)
            .map(|i| assistant_text(&format!("step {i}: running checks {}", "x".repeat(100))))
            .collect();
        lines.push(assistant_text("FRESHEST"));
        let out = redact_and_assemble(&lines);
        assert!(out.contains("FRESHEST"), "freshest entry must survive");
        assert!(out.len() <= MAX_PROMPT_BYTES + 256);
    }

    #[test]
    fn clean_summary_strips_quotes_and_period() {
        assert_eq!(
            clean_summary("\"running tests after the redactor lands.\""),
            "running tests after the redactor lands",
        );
    }

    #[test]
    fn clean_summary_rejects_mostly_redacted_reply() {
        // Model echo of a string that's almost entirely redaction
        // markers — must fall back to empty.
        let s = clean_summary("<redacted> <redacted> <redacted> <redacted>");
        assert!(s.is_empty(), "expected empty, got {s:?}");
    }

    #[test]
    fn clean_summary_post_filters_a_leaked_token() {
        // If the model echoed a token back at us, the post-filter
        // must catch it.
        let s = clean_summary("running tests with sk-ant-api03-abcdefghijklmnopqrstuvwxyz");
        assert!(s.contains("<redacted>"));
        assert!(!s.contains("sk-ant"));
    }

    #[test]
    fn clean_summary_collapses_internal_newlines_to_spaces() {
        assert_eq!(
            clean_summary("running tests\nafter the\nlayout fix"),
            "running tests after the layout fix",
        );
    }

    /// A provider with no credential resolvable for `LiveStatus`. Built
    /// through the injectable lookup so the test never touches process env.
    fn keyless_provider() -> crate::utility_model::AnthropicUtilityModel {
        crate::utility_model::AnthropicUtilityModel::from_lookup(None, |_| None)
    }

    fn keyed_provider() -> crate::utility_model::AnthropicUtilityModel {
        crate::utility_model::AnthropicUtilityModel::from_lookup(Some("key".to_owned()), |_| None)
    }

    #[test]
    fn summarize_transcript_reports_no_api_key() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let provider = keyless_provider();
        let result = rt.block_on(summarize_transcript(&provider, &[]));
        assert!(matches!(result, SummarizerOutcome::NoApiKey(_)));
        assert_eq!(result.tag(), "no_api_key");
        // The detail must name what to set, not just that something is missing.
        assert!(result.detail().contains("ANTHROPIC_API_KEY"), "{}", result.detail());
    }

    #[tokio::test]
    async fn summarize_transcript_reports_empty_after_redaction() {
        // Use the *exact* shape the redactor's `should_drop_entry`
        // matches — a top-level `content` array. The
        // `assistant_tool_use` helper wraps content under `message`,
        // which `should_drop_entry` does not currently walk into.
        // That coverage gap is its own bug; the chore explicitly
        // asks us not to fix it here, only to surface it via
        // observability (the per-slot debug verb will show
        // `last_outcome=success` even when the redactor missed). For
        // this test we use a deliberately-redactor-recognised fixture
        // so the EmptyAfterRedaction branch is exercised end-to-end
        // without depending on an outbound HTTP call.
        let lines = vec![serde_json::json!({
            "type": "assistant",
            "content": [{
                "type": "tool_use",
                "name": "Read",
                "input": {"file_path": "/Users/x/.ssh/id_rsa"},
            }],
        })];
        let provider = keyed_provider();
        let result = summarize_transcript(&provider, &lines).await;
        assert!(
            matches!(result, SummarizerOutcome::EmptyAfterRedaction),
            "outcome was {:?}",
            result
        );
        assert_eq!(result.tag(), "empty_after_redaction");
    }

    #[test]
    fn parse_status_reply_reads_the_status_branch() {
        assert_eq!(
            parse_status_reply(&json!({"insufficient_input": false, "status": "\"Reading auth config.\""})),
            ClaudeReply::Success("Reading auth config".to_owned())
        );
        // Flag omitted entirely is also the status branch.
        assert_eq!(
            parse_status_reply(&json!({"status": "Running bazel build"})),
            ClaudeReply::Success("Running bazel build".to_owned())
        );
    }

    #[test]
    fn parse_status_reply_insufficient_input_wins_over_status_text() {
        // A model that sets the flag *and* types its explanation into
        // `status` must not get that explanation onto the card.
        let reply = parse_status_reply(&json!({
            "insufficient_input": true,
            "status": "I don't have enough context from the transcript to determine what the worker is doing",
            "reason": "only the prompt and a turn-start marker are present",
        }));
        assert_eq!(
            reply,
            ClaudeReply::InsufficientInput {
                reason: "only the prompt and a turn-start marker are present".to_owned()
            }
        );
    }

    #[test]
    fn parse_status_reply_redacts_and_clips_the_reason() {
        // Spaced ordinary words survive the generic 32+ base64 redaction
        // pattern, so the clip path actually runs. A GitHub token starts
        // before byte 200 and ends after it: clip-then-redact would keep
        // a `ghp_` prefix too short to match the token pattern; redact-
        // then-clip removes the whole token first.
        let head = "word ".repeat(37);
        let token = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345";
        let tail = "more ".repeat(80);
        let raw = format!("{head}{token} {tail}");
        let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let token_at = one_line.find(token).expect("fixture must contain the token");
        assert!(
            token_at < INSUFFICIENT_INPUT_REASON_MAX_BYTES
                && token_at + token.len() > INSUFFICIENT_INPUT_REASON_MAX_BYTES,
            "token must straddle the {INSUFFICIENT_INPUT_REASON_MAX_BYTES}-byte cut; starts at {token_at}"
        );

        let reply = parse_status_reply(&json!({
            "insufficient_input": true,
            "reason": raw,
        }));
        let ClaudeReply::InsufficientInput { reason } = reply else {
            panic!("expected insufficient input, got {reply:?}");
        };
        assert!(!reason.contains("ghp_"), "{reason}");
        assert!(reason.contains("<redacted>"), "{reason}");
        assert!(reason.ends_with('…'), "{reason}");
        assert!(reason.len() <= INSUFFICIENT_INPUT_REASON_MAX_BYTES, "{}", reason.len());
    }

    #[test]
    fn parse_status_reply_rejects_shapes_outside_the_schema() {
        assert!(matches!(
            parse_status_reply(&json!("working")),
            ClaudeReply::Malformed(_)
        ));
        assert!(matches!(
            parse_status_reply(&json!({"insufficient_input": "yes", "status": "x"})),
            ClaudeReply::Malformed(_)
        ));
        assert!(matches!(
            parse_status_reply(&json!({"status": 7})),
            ClaudeReply::Malformed(_)
        ));
        assert!(matches!(parse_status_reply(&json!({})), ClaudeReply::Malformed(_)));
        // An empty / fully-redacted status is the post-filter's call, not
        // a schema violation.
        assert_eq!(
            parse_status_reply(&json!({"status": "<redacted> <redacted>"})),
            ClaudeReply::PostFilterDropped
        );
    }

    #[test]
    fn summarizer_outcome_tag_strings_match_chore_spec() {
        // The chore's debug-verb contract names these four outcomes
        // explicitly: success / no_api_key / api_error / empty_after_redaction.
        // The strings are part of the public-facing JSON, so pin them.
        assert_eq!(SummarizerOutcome::Success("running tests".into()).tag(), "success");
        assert_eq!(SummarizerOutcome::NoApiKey("no credential".into()).tag(), "no_api_key");
        assert_eq!(
            SummarizerOutcome::ApiError {
                status: 429,
                snippet: "rate limited".into()
            }
            .tag(),
            "api_error"
        );
        assert_eq!(SummarizerOutcome::EmptyAfterRedaction.tag(), "empty_after_redaction");
        assert_eq!(
            SummarizerOutcome::InsufficientInput {
                reason: "r".into(),
                input_lines: 2,
                input_bytes: 40
            }
            .tag(),
            "insufficient_input"
        );
        assert_eq!(SummarizerOutcome::MalformedReply("x".into()).tag(), "malformed_reply");
    }

    #[tokio::test]
    async fn end_to_end_summarize_with_wiremock() {
        // live_status now shares the engine-wide `claude_client` pipeline. Drive
        // a realistic Anthropic response through it with the exact request
        // `claude_one_sentence` builds (system guardrails + redacted transcript),
        // and confirm the shared client sets the auth/version headers and that
        // `clean_summary` post-processes the first text block.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", claude_client::ANTHROPIC_API_VERSION))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(status_reply("running tests after the redactor lands")),
            )
            .mount(&server)
            .await;

        // Drive the real entry point through a provider pointed at the mock,
        // so the endpoint/model/credential all come from the seam rather than
        // from constants this test re-declares.
        let provider = crate::utility_model::AnthropicUtilityModel::from_lookup(Some("test-key".to_owned()), |_| None)
            .with_endpoint(format!("{}/v1/messages", server.uri()));
        let call = provider.resolve(UtilityTask::LiveStatus).expect("key was supplied");
        assert_eq!(call.model, SUMMARY_MODEL);

        let reply = claude_one_sentence(&call, "assistant: investigating the bug")
            .await
            .expect("mock success");
        let ClaudeReply::Success(summary) = reply else {
            panic!("expected a summary, got {reply:?}");
        };
        assert_eq!(summary, "running tests after the redactor lands");

        // The request forces the status tool so the reply can never be
        // free text.
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["tool_choice"], json!({"type": "tool", "name": STATUS_TOOL_NAME}));
        assert_eq!(body["tools"][0]["name"], STATUS_TOOL_NAME);
        assert_eq!(body["tools"][0]["input_schema"], status_tool_schema());
        assert_eq!(body["max_tokens"], SUMMARY_MAX_TOKENS);
    }

    #[tokio::test]
    async fn insufficient_input_reply_is_a_logged_outcome_not_a_status() {
        // The model's "I can't tell" arrives as a structured flag and is
        // surfaced as its own outcome — never as a Success the loop would
        // write to the card.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(insufficient_input_reply(
                "the tail holds only a turn-start marker and the prompt",
            )))
            .mount(&server)
            .await;
        let provider = crate::utility_model::AnthropicUtilityModel::from_lookup(Some("test-key".to_owned()), |_| None)
            .with_endpoint(format!("{}/v1/messages", server.uri()));
        let lines = vec![json!({"type": "system", "message": "turn started"})];
        let outcome = summarize_transcript(&provider, &lines).await;
        let SummarizerOutcome::InsufficientInput {
            reason,
            input_lines,
            input_bytes,
        } = &outcome
        else {
            panic!("expected InsufficientInput, got {outcome:?}");
        };
        assert_eq!(reason, "the tail holds only a turn-start marker and the prompt");
        assert_eq!(*input_lines, 1);
        assert_eq!(*input_bytes, "system: turn started".len());
        assert_eq!(outcome.tag(), "insufficient_input");
        assert!(outcome.detail().contains("1 transcript lines"), "{}", outcome.detail());
    }

    #[tokio::test]
    async fn free_text_reply_is_malformed_not_a_status() {
        // A free-text reply that ignores the forced tool is a malformed
        // reply, never a status.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "text", "text": "I don't have enough context from the transcript to determine…"}],
            })))
            .mount(&server)
            .await;
        let provider = crate::utility_model::AnthropicUtilityModel::from_lookup(Some("test-key".to_owned()), |_| None)
            .with_endpoint(format!("{}/v1/messages", server.uri()));
        let lines = vec![assistant_text("Investigating the scroll handler bug")];
        let outcome = summarize_transcript(&provider, &lines).await;
        assert!(
            matches!(&outcome, SummarizerOutcome::MalformedReply(what) if what.contains("text")),
            "got {outcome:?}"
        );
        assert_eq!(outcome.tag(), "malformed_reply");
    }

    #[tokio::test]
    async fn end_to_end_redacts_transcript_before_calling_model() {
        // Verifies the privacy promise: even when a secret slips into
        // the assistant text, the redacted prompt body fed to the
        // model never contains it verbatim.
        let lines = vec![assistant_text(
            "trying token sk-ant-api03-ZZZZZZZZZZZZZZZZZZZZZZZZZZZZ now",
        )];
        let body = redact_and_assemble(&lines);
        let (_system, user) = build_messages(&body);
        assert!(user.contains("<redacted>"), "user prompt was {user}");
        assert!(!user.contains("sk-ant-api03-Z"), "leaked: {user}");
    }
}
