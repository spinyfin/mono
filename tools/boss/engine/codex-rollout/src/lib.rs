//! Pure Codex rollout JSONL shape helpers shared by the driver progress
//! normaliser and the transcript-markdown converter.
//!
//! Both consumers need the same dialect facts (tool-call identity,
//! `exec`/`exec_command` → Bash reshape, content-block text extraction,
//! tool-output body / exit-code). Putting them here keeps the two
//! higher-level crates free of a dependency cycle: `driver` must not
//! depend on `transcript-markdown`, and the converter must not depend
//! on the driver.
//!
//! This crate intentionally does **not** emit live-status filler events
//! (`turn started` / `turn completed`) — those are progress-pipeline
//! concerns that belong only in the driver.

pub mod cell;

use serde_json::{Map, Value, json};

/// Canonicalised rollout tool call ready for progress or transcript use.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalRolloutToolCall {
    pub call_id: Option<String>,
    pub tool_name: String,
    pub tool_input: Value,
    /// Whether this call itself starts a shell command, as opposed to being
    /// a pure continuation of one already running (a `tools.write_stdin`
    /// cell, or any non-`exec`/`exec_command` tool).
    ///
    /// Computed from the **raw** dialect input, not from `tool_input`: for
    /// the cell-harness (JavaScript) dialect, `tool_input.command` is
    /// already the *extracted* command string once reshaped by
    /// [`exec_command`], so re-deriving this fact from it would find no
    /// `tools.exec_command(` call and misclassify every command-bearing
    /// cell as a continuation. See
    /// `boss_engine_driver::codex::rollout_calls::RolloutCallTracker::observe_output`,
    /// the sole consumer, for why the distinction matters.
    pub issues_command: bool,
}

/// Parsed tool-output body plus error flag derived from the rollout dialect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalRolloutToolOutput {
    pub body: String,
    /// `true` only when a structured exit code — top-level `exit_code`
    /// (codex-cli 0.145.0's exec dialect) or the legacy `metadata.exit_code`
    /// — is present and non-zero. Plain string / content-array outputs with
    /// no parseable exit code (including a truncated JSON payload) stay
    /// `false`: an unobserved exit code is never promoted to an error, but
    /// it isn't asserted as success either.
    pub is_error: bool,
}

/// Whether `item_type` is a rollout tool-call record
/// (`custom_tool_call` / `function_call`).
pub fn is_rollout_tool_call(item_type: &str) -> bool {
    matches!(item_type, "custom_tool_call" | "function_call")
}

/// Whether `item_type` is a rollout tool-output record
/// (`custom_tool_call_output` / `function_call_output`).
pub fn is_rollout_tool_output(item_type: &str) -> bool {
    matches!(item_type, "custom_tool_call_output" | "function_call_output")
}

/// Canonicalise a rollout tool-call payload into a Bash-shaped (or
/// pass-through) name/input pair.
///
/// Returns `None` when `item_type` is not a tool-call type or the payload
/// has no usable `name`. `call_id` is optional so transcript rendering can
/// still surface calls that lack correlation ids; the progress pipeline
/// should require it before tracking.
///
/// Shared by `boss_engine_driver::codex::progress` and
/// `boss_transcript_markdown` — keep both consumers on this helper rather
/// than re-implementing the reshape.
pub fn canonical_rollout_tool_call(item_type: &str, payload: &Map<String, Value>) -> Option<CanonicalRolloutToolCall> {
    if !is_rollout_tool_call(item_type) {
        return None;
    }
    let call_id = payload.get("call_id").and_then(Value::as_str).map(str::to_owned);
    let provider_name = payload.get("name")?.as_str()?;
    let raw_input = payload
        .get("input")
        .or_else(|| payload.get("arguments"))
        .cloned()
        .unwrap_or(Value::Null);
    let parsed_input = match raw_input {
        Value::String(text) => serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)),
        other => other,
    };

    let issues_command = match provider_name {
        "exec" | "exec_command" => exec_input_issues_command(&parsed_input),
        _ => true,
    };
    let (tool_name, tool_input) = match provider_name {
        "exec" | "exec_command" => ("Bash".to_owned(), json!({"command": exec_command(&parsed_input)})),
        other => (other.to_owned(), parsed_input),
    };
    Some(CanonicalRolloutToolCall {
        call_id,
        tool_name,
        tool_input,
        issues_command,
    })
}

/// Whether an `exec` / `exec_command` call's raw (pre-reshape) input starts a
/// command, as opposed to being a pure continuation.
///
/// The plain shell-tool dialect (`cmd`/`command`) always issues a command by
/// definition. The cell-harness dialect only does when its script contains a
/// command-bearing `tools.*` call ([`cell::commands_from_cell_script`]); a
/// pure `tools.write_stdin` continuation script does not. Any other shape —
/// an argv array, or an object using some other key — is unrecognised, and
/// defaults to `true` (issues a command) rather than `false`: treating an
/// unknown shape as a pure continuation would route its output through
/// `sessions` in `observe_output` and let it be silently subsumed into
/// someone else's origin, which is exactly the failure mode this field
/// exists to prevent. Only a positively-identified continuation script (a
/// string with no command-bearing cell call) returns `false`.
fn exec_input_issues_command(parsed_input: &Value) -> bool {
    if parsed_input
        .get("cmd")
        .or_else(|| parsed_input.get("command"))
        .is_some()
    {
        return true;
    }
    !parsed_input
        .as_str()
        .is_some_and(|source| cell::commands_from_cell_script(source).is_empty())
}

/// The command an `exec` / `exec_command` tool call runs.
///
/// Two input dialects reach this, and they are not variants of one shape:
///
/// - a JSON object carrying `cmd` / `command` — the plain shell-tool form.
/// - **JavaScript source** — the cell harness form (`gpt-5.6-terra`), where
///   the command is an argument to a `tools.exec_command({…})` call inside
///   the script. Parsing the input as JSON fails outright here, so without
///   [`cell::cell_script_command`] the whole script becomes "the command"
///   and renders into live status, operator notifications, and the
///   editorial audit as such.
///
/// A script with no command-bearing call (a pure `tools.write_stdin`
/// continuation) still falls back to its source: it genuinely has no
/// command of its own, and showing the script is more honest than
/// attributing someone else's command to it.
fn exec_command(parsed_input: &Value) -> String {
    parsed_input
        .get("cmd")
        .or_else(|| parsed_input.get("command"))
        .map(coerce_command_to_string)
        .or_else(|| parsed_input.as_str().and_then(cell::cell_script_command))
        .unwrap_or_else(|| coerce_command_to_string(parsed_input))
}

/// Coerce a Bash `command` value to a single string for rendering / capture.
///
/// - strings pass through
/// - arrays of argv tokens are space-joined
/// - other JSON shapes are pretty-printed so the fence is never empty
pub fn coerce_command_to_string(command: &Value) -> String {
    match command {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| match part {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// Flatten a rollout tool-output `payload.output` value to the text the
/// model actually saw, with no structural interpretation.
///
/// This is the **single** flattener for a rollout `payload.output`: cell
/// classification, the transcript session and the driver's PR-URL capture
/// feed all go through it, so one value can never be read two ways. Two
/// implementations previously split that job, and their block-type policies
/// already disagreed — a block type one carried and the other dropped would
/// have been invisible to classification (closing a still-running call)
/// while staying visible to capture.
///
/// The policy is therefore deliberately structural rather than an
/// allowlist: any object's `text` is taken, whatever its `type`. Blocks with
/// no `text` (an image block, say) flatten to the empty string rather than
/// being dropped, because the harness's own header/payload split relies on
/// the blank line an empty block leaves behind.
///
/// Distinct from [`canonical_rollout_tool_output`], which *unwraps* the
/// structured chunk form and hands back its inner `output` field. Dialect
/// classification — is this a [`cell`] harness envelope? — has to run
/// against the flattened text, before any such interpretation.
pub fn flatten_tool_output_text(output: &Value) -> String {
    match output {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => extract_text_blocks(blocks),
        Value::Object(object) => object.get("text").map(flatten_tool_output_text).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Join the text of a Codex content-block array, newline-separated.
/// Element-wise [`flatten_tool_output_text`].
pub fn extract_text_blocks(content: &[Value]) -> String {
    content
        .iter()
        .map(flatten_tool_output_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse a rollout tool-output `payload.output` value.
///
/// Observed forms:
/// - content-block array (`custom_tool_call_output`) whose joined text is
///   itself the JSON string form below (codex-cli 0.145.0's exec dialect):
///   body from `output`, `is_error` from the embedded top-level `exit_code`
/// - content-block array whose joined text is plain (not JSON): joined
///   text as body, non-error
/// - plain string (`function_call_output`): used as body, non-error, unless
///   it parses as the JSON string form below
/// - JSON string `{"output":"…","exit_code":N}` or the legacy
///   `{"output":"…","metadata":{"exit_code":N}}` shape: body from `output`,
///   `is_error` when the exit code is present and non-zero
/// - object with the same shape as the JSON string form
///
/// A JSON string form that fails to parse (e.g. a truncated payload) is not
/// valid JSON, so it falls back to the plain-text case rather than being
/// treated as a successful (`is_error: false`) structured result — the
/// truncated exit code is unobserved, not confirmed absent.
pub fn canonical_rollout_tool_output(output: &Value) -> CanonicalRolloutToolOutput {
    match output {
        Value::Array(blocks) => {
            let joined = extract_text_blocks(blocks);
            parse_possibly_structured_text(joined)
        }
        Value::String(text) => parse_possibly_structured_text(text.clone()),
        Value::Object(_) => structured_tool_output(output).unwrap_or(CanonicalRolloutToolOutput {
            body: serde_json::to_string_pretty(output).unwrap_or_default(),
            is_error: false,
        }),
        _ => CanonicalRolloutToolOutput {
            body: String::new(),
            is_error: false,
        },
    }
}

/// Try to parse `text` as the JSON tool-output shape; fall back to using it
/// verbatim as a non-error body when it isn't JSON, isn't the expected
/// shape, or fails to parse (e.g. truncation).
fn parse_possibly_structured_text(text: String) -> CanonicalRolloutToolOutput {
    match serde_json::from_str::<Value>(&text) {
        Ok(parsed) => structured_tool_output(&parsed).unwrap_or(CanonicalRolloutToolOutput {
            body: text,
            is_error: false,
        }),
        Err(_) => CanonicalRolloutToolOutput {
            body: text,
            is_error: false,
        },
    }
}

fn structured_tool_output(value: &Value) -> Option<CanonicalRolloutToolOutput> {
    let obj = value.as_object()?;
    let body_value = obj.get("output")?;
    let body = match body_value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => extract_text_blocks(blocks),
        other => other.to_string(),
    };
    let is_error = obj
        .get("exit_code")
        .and_then(exit_code_nonzero)
        .or_else(|| {
            obj.get("metadata")
                .and_then(|meta| meta.get("exit_code"))
                .and_then(exit_code_nonzero)
        })
        .unwrap_or(false);
    Some(CanonicalRolloutToolOutput { body, is_error })
}

fn exit_code_nonzero(value: &Value) -> Option<bool> {
    match value {
        Value::Number(n) => n.as_i64().map(|code| code != 0).or_else(|| {
            n.as_u64()
                .map(|code| code != 0)
                .or_else(|| n.as_f64().map(|code| code != 0.0))
        }),
        Value::String(s) => s.parse::<i64>().ok().map(|code| code != 0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exec_command_string_args_become_bash() {
        let payload = json!({
            "name": "exec_command",
            "call_id": "c1",
            "arguments": r#"{"cmd":"echo hi"}"#
        })
        .as_object()
        .unwrap()
        .clone();
        let call = canonical_rollout_tool_call("function_call", &payload).expect("call");
        assert_eq!(call.call_id.as_deref(), Some("c1"));
        assert_eq!(call.tool_name, "Bash");
        assert_eq!(call.tool_input.get("command").and_then(Value::as_str), Some("echo hi"));
    }

    #[test]
    fn exec_cmd_array_is_space_joined() {
        let payload = json!({
            "name": "exec",
            "call_id": "c2",
            "input": {"cmd": ["echo", "hi"]}
        })
        .as_object()
        .unwrap()
        .clone();
        let call = canonical_rollout_tool_call("custom_tool_call", &payload).expect("call");
        assert_eq!(call.tool_name, "Bash");
        assert_eq!(call.tool_input.get("command").and_then(Value::as_str), Some("echo hi"));
    }

    #[test]
    fn cell_harness_exec_reshapes_to_the_shell_command_not_the_script() {
        // Verbatim `custom_tool_call` from probe 6 of the exit-code
        // investigation: the input is JavaScript, so it never parses as
        // JSON and the command has to come out of the script.
        let payload = json!({
            "type": "custom_tool_call",
            "name": "exec",
            "call_id": "call_aW1HJVpKwjjhVOK85B8payHB",
            "input": "const r = await tools.exec_command({\"cmd\":\"cube pr create --branch b\",\"workdir\":\"/w\",\"yield_time_ms\":30000,\"max_output_tokens\":2000});\ntext(JSON.stringify(r));"
        })
        .as_object()
        .unwrap()
        .clone();
        let call = canonical_rollout_tool_call("custom_tool_call", &payload).expect("call");
        assert_eq!(call.tool_name, "Bash");
        assert_eq!(
            call.tool_input.get("command").and_then(Value::as_str),
            Some("cube pr create --branch b")
        );
    }

    #[test]
    fn continuation_only_cell_script_still_falls_back_to_its_source() {
        let source = "const r = load(\"run\");\nconst p = await tools.write_stdin({\"session_id\":r.session_id});";
        let payload = json!({"name": "exec", "call_id": "c9", "input": source})
            .as_object()
            .unwrap()
            .clone();
        let call = canonical_rollout_tool_call("custom_tool_call", &payload).expect("call");
        assert_eq!(call.tool_input.get("command").and_then(Value::as_str), Some(source));
    }

    #[test]
    fn exec_input_issues_command_covers_the_known_and_unknown_shapes() {
        assert!(exec_input_issues_command(&json!({"cmd": "echo hi"})));
        assert!(exec_input_issues_command(&json!(
            "const r = await tools.exec_command({\"cmd\":\"echo hi\"});"
        )));
        assert!(!exec_input_issues_command(&json!(
            "const r = load(\"run\");\nconst p = await tools.write_stdin({\"session_id\":r.session_id});"
        )));
        // An unrecognised shape (here, an argv array with no `cmd`/`command`
        // key) must default to `true`: treating it as a pure continuation
        // would let `observe_output` silently subsume its output into
        // someone else's session.
        assert!(exec_input_issues_command(&json!({"argv": ["echo", "hi"]})));
    }

    #[test]
    fn flatten_tool_output_text_covers_the_observed_output_shapes() {
        assert_eq!(flatten_tool_output_text(&json!("plain")), "plain");
        assert_eq!(
            flatten_tool_output_text(&json!([
                {"type":"input_text","text":"Script completed"},
                {"type":"input_text","text":"{}"}
            ])),
            "Script completed\n{}"
        );
        assert_eq!(flatten_tool_output_text(&Value::Null), "");
    }

    #[test]
    fn non_exec_tool_passes_name_and_input_through() {
        let payload = json!({
            "name": "Read",
            "call_id": "c3",
            "input": {"path": "/tmp/x"}
        })
        .as_object()
        .unwrap()
        .clone();
        let call = canonical_rollout_tool_call("custom_tool_call", &payload).expect("call");
        assert_eq!(call.tool_name, "Read");
        assert_eq!(call.tool_input, json!({"path": "/tmp/x"}));
    }

    #[test]
    fn extract_text_blocks_takes_text_whatever_the_block_type_is() {
        // No block-type allowlist: an additive text-bearing type must not be
        // invisible to cell classification while staying visible to capture.
        let blocks = vec![
            json!({"type":"input_text","text":"a"}),
            json!({"type":"output_text","text":"b"}),
            json!({"type":"text","text":"c"}),
            json!({"type":"some_future_text","text":"d"}),
            json!({"type":"image","url":"x"}),
        ];
        assert_eq!(extract_text_blocks(&blocks), "a\nb\nc\nd\n");
    }

    #[test]
    fn flatten_tool_output_text_reads_a_bare_text_object() {
        assert_eq!(
            flatten_tool_output_text(&json!({"type":"output_text","text":"hi"})),
            "hi"
        );
        assert_eq!(flatten_tool_output_text(&json!({"type":"image","url":"x"})), "");
    }

    #[test]
    fn tool_output_array_is_non_error_body() {
        let out = canonical_rollout_tool_output(&json!([
            {"type":"input_text","text":"Script completed"},
            {"type":"input_text","text":""}
        ]));
        assert_eq!(out.body, "Script completed\n");
        assert!(!out.is_error);
    }

    #[test]
    fn tool_output_plain_string_is_non_error() {
        let out = canonical_rollout_tool_output(&json!("https://example.com/pr/1\n"));
        assert_eq!(out.body, "https://example.com/pr/1\n");
        assert!(!out.is_error);
    }

    #[test]
    fn tool_output_json_string_with_exit_code_sets_is_error() {
        let raw = r#"{"output":"boom\n","metadata":{"exit_code":1}}"#;
        let out = canonical_rollout_tool_output(&Value::String(raw.to_owned()));
        assert_eq!(out.body, "boom\n");
        assert!(out.is_error);
    }

    #[test]
    fn tool_output_json_string_exit_zero_is_ok() {
        let raw = r#"{"output":"ok","metadata":{"exit_code":0}}"#;
        let out = canonical_rollout_tool_output(&Value::String(raw.to_owned()));
        assert_eq!(out.body, "ok");
        assert!(!out.is_error);
    }

    #[test]
    fn tool_output_object_form_supported() {
        let out = canonical_rollout_tool_output(&json!({
            "output": "fail",
            "metadata": {"exit_code": 127}
        }));
        assert_eq!(out.body, "fail");
        assert!(out.is_error);
    }

    #[test]
    fn tool_output_json_string_with_top_level_exit_code_sets_is_error() {
        let raw = r#"{"output":"boom\n","exit_code":9}"#;
        let out = canonical_rollout_tool_output(&Value::String(raw.to_owned()));
        assert_eq!(out.body, "boom\n");
        assert!(out.is_error);
    }

    #[test]
    fn tool_output_content_block_array_with_embedded_top_level_exit_code() {
        let raw = r#"{"output":"exited badly\n","exit_code":137}"#;
        let out = canonical_rollout_tool_output(&json!([{"type": "output_text", "text": raw}]));
        assert_eq!(out.body, "exited badly\n");
        assert!(out.is_error);
    }

    #[test]
    fn tool_output_content_block_array_with_embedded_zero_exit_code_is_ok() {
        let raw = r#"{"output":"all good\n","exit_code":0}"#;
        let out = canonical_rollout_tool_output(&json!([{"type": "output_text", "text": raw}]));
        assert_eq!(out.body, "all good\n");
        assert!(!out.is_error);
    }

    #[test]
    fn tool_output_truncated_json_degrades_to_non_error_raw_body() {
        // A truncated payload is not valid JSON: the parser must not
        // classify it as a confirmed-successful structured result, but it
        // also must not fabricate an error from an unobserved exit code —
        // it falls back to the raw text as the body, non-error.
        let raw = r#"{"output":"partial\n","exit_code":1"#;
        let out = canonical_rollout_tool_output(&json!([{"type": "output_text", "text": raw}]));
        assert_eq!(out.body, raw);
        assert!(!out.is_error);
    }
}
