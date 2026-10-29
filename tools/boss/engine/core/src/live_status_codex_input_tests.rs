//! Summarizer-input tests for Codex rollouts.
//!
//! Each test feeds a realistic rollout segment through the run's transcript
//! normalizer exactly as the slot loop does ([`normalize_like_slot_loop`]
//! mirrors `live_status_loop::normalize_lines`), then through
//! [`redact_and_assemble`], and checks what the model would actually be shown.
//!
//! The shape under test is a code-mode worker (`gpt-6-astra`) spending a long
//! stretch on tool calls with no assistant prose. Every tool call in such a
//! segment must appear as a `tool: Bash(...)` line even when the cell script
//! exceeds the redactor's per-value ceiling.

use super::test_support::status_reply;
use super::*;
use crate::driver::{AgentDriver, CodexDriver};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Rollout prelude shared by the segments: session meta, the turn start, the
/// injected developer/user messages, and the records Codex writes around
/// them that carry nothing for the summarizer.
fn rollout_prelude(prompt: &str) -> Vec<Value> {
    vec![
        json!({"timestamp":"2026-10-28T19:40:00.000Z","type":"session_meta","payload":{"id":"thread-1","cwd":"/w","originator":"codex_exec"}}),
        json!({"timestamp":"2026-10-28T19:40:00.100Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}),
        json!({"timestamp":"2026-10-28T19:40:00.200Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions> ".repeat(100)}]}}),
        json!({"timestamp":"2026-10-28T19:40:00.300Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":prompt}]}}),
        json!({"timestamp":"2026-10-28T19:40:00.400Z","type":"world_state","payload":{"full":true,"state":{}}}),
        json!({"timestamp":"2026-10-28T19:40:00.500Z","type":"turn_context","payload":{"turn_id":"t1","cwd":"/w"}}),
        json!({"timestamp":"2026-10-28T19:40:00.600Z","type":"event_msg","payload":{"type":"user_message","message":prompt}}),
    ]
}

/// The records one completed code-mode cell leaves in the rollout: the
/// reasoning item ahead of it, the `exec` call, its completed output with
/// the forwarded chunk, and the token-count event after it.
fn completed_cell(call_id: &str, script: &str, output: &str) -> Vec<Value> {
    let chunk =
        json!({"chunk_id":"abc","wall_time_seconds":0.3,"exit_code":0,"original_token_count":900,"output":output})
            .to_string();
    vec![
        json!({"timestamp":"2026-10-28T19:40:01.000Z","type":"response_item","payload":{"type":"reasoning","id":"rs","summary":[],"encrypted_content":"gAAAA"}}),
        json!({"timestamp":"2026-10-28T19:40:01.100Z","type":"response_item","payload":{"type":"custom_tool_call","id":"ctc","status":"completed","call_id":call_id,"name":"exec","input":script}}),
        json!({"timestamp":"2026-10-28T19:40:01.400Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":call_id,"output":format!("Script completed\nWall time 0.3 seconds\nOutput:\n\n{chunk}")}}),
        json!({"timestamp":"2026-10-28T19:40:01.500Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100}}}}),
    ]
}

/// Six file reads and searches, each its own JSON-form cell (the shape every
/// captured transcript uses), each returning a few KiB of source.
fn rollout_fixture_tool_heavy() -> Vec<Value> {
    let mut lines = rollout_prelude(&"Generate a review guide for PR #3356. ".repeat(60));
    let commands = [
        "sed -n '1,120p' tools/boss/engine/core/src/work/execution_bookmarks.rs",
        "sed -n '120,260p' tools/boss/engine/core/src/work/execution_bookmarks.rs",
        "sed -n '1,80p' tools/boss/engine/core/src/coordinator/execution.rs",
        "rg -n 'retire|recovery' tools/boss/engine/core/src/teardown.rs",
        "rg -n 'bookmark' tools/boss/engine/core/src/sweep_loop.rs",
        "sed -n '1,200p' tools/boss/engine/core/src/execution_bookmark_recovery.rs",
    ];
    for (i, cmd) in commands.iter().enumerate() {
        let script = format!(
            "const r = await tools.exec_command({});\ntext(JSON.stringify(r));",
            json!({"cmd": cmd, "workdir": "/w", "yield_time_ms": 30000, "max_output_tokens": 2000})
        );
        let file_body = format!("// line of rust source {i}\n").repeat(120);
        lines.extend(completed_cell(&format!("call_{i}"), &script, &file_body));
    }
    lines
}

/// A code-mode cell written in JavaScript object shorthand (so the command
/// cannot be recovered from it and the whole script stands in for the
/// command) that batches ten reads and runs past the redactor's per-value
/// ceiling.
fn big_shorthand_cell(call_id: &str) -> Vec<Value> {
    let mut script = String::new();
    for i in 0..10 {
        script.push_str(&format!(
            "const r{i} = await tools.exec_command({{cmd: \"sed -n '{a},{b}p' tools/boss/engine/core/src/work/execution_bookmarks.rs\", workdir: \"/Users/operator/.local/share/cube/workspaces/mono-agent-677\", yield_time_ms: 30000, max_output_tokens: 2000}});\n",
            a = i * 100 + 1,
            b = (i + 1) * 100
        ));
    }
    script.push_str("text([r0,r1,r2,r3,r4,r5,r6,r7,r8,r9].map(r => r.output).join('\\n'));");
    assert!(
        script.len() > live_status_redact::MAX_VALUE_BYTES,
        "fixture must exceed the per-value ceiling: {}",
        script.len()
    );
    completed_cell(call_id, &script, &"// rust\n".repeat(400))
}

/// Mirrors `live_status_loop::normalize_lines` for a Codex run: every raw
/// rollout record goes through the run's session normalizer before
/// redaction sees it.
fn normalize_like_slot_loop(raw: Vec<Value>) -> Vec<Value> {
    let driver = CodexDriver::default();
    let mut session = driver.transcript_session();
    raw.into_iter()
        .map(|line| match session.as_mut() {
            Some(session) => session.normalize_transcript_entry(line),
            None => driver.normalize_transcript_entry(line),
        })
        .collect()
}

fn tool_call_lines(prompt: &str) -> Vec<&str> {
    prompt.lines().filter(|line| line.starts_with("tool: Bash(")).collect()
}

fn tool_result_lines(prompt: &str) -> Vec<&str> {
    prompt
        .lines()
        .filter(|line| line.starts_with("tool: Bash returned") || line.starts_with("tool: Bash failed"))
        .collect()
}

#[test]
fn codex_tool_heavy_segment_reaches_summarizer_with_tool_activity() {
    let normalized = normalize_like_slot_loop(rollout_fixture_tool_heavy());
    let prompt = redact_and_assemble(&normalized);
    assert!(!prompt.trim().is_empty());
    // Pure tool activity: no assistant prose anywhere in the segment.
    assert!(!prompt.lines().any(|line| line.starts_with("assistant:")), "{prompt}");
    // Every call and every result survives into the model's input.
    assert_eq!(tool_call_lines(&prompt).len(), 6, "{prompt}");
    assert_eq!(tool_result_lines(&prompt).len(), 6, "{prompt}");
    assert!(prompt.contains("sed -n '1,120p'"), "{prompt}");
    assert!(prompt.contains("rg -n 'bookmark'"), "{prompt}");
    assert!(prompt.contains("execution_bookmarks.rs"), "{prompt}");
}

#[test]
fn codex_oversize_cell_scripts_keep_their_tool_calls_in_the_prompt() {
    let mut raw = rollout_prelude("Generate a review guide for PR #3356.");
    for i in 0..4 {
        raw.extend(big_shorthand_cell(&format!("call_{i}")));
    }
    let normalized = normalize_like_slot_loop(raw);
    let prompt = redact_and_assemble(&normalized);
    // Each oversize cell still contributes its call line and its result line.
    assert_eq!(tool_call_lines(&prompt).len(), 4, "{prompt}");
    assert_eq!(tool_result_lines(&prompt).len(), 4, "{prompt}");
    for line in tool_call_lines(&prompt) {
        assert!(line.contains("exec_command"), "call line lost its command head: {line}");
        assert!(line.contains("sed -n"), "call line lost its command head: {line}");
    }
    // The ceiling still holds: the script body never reaches the prompt.
    assert!(!prompt.contains("yield_time_ms: 30000, max_output_tokens"), "{prompt}");
}

#[tokio::test]
async fn codex_tool_heavy_segment_is_what_the_model_is_sent() {
    // End of the input path: the request body the summarizer actually
    // posts carries the tool activity, and the structured status comes
    // back as the outcome.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(status_reply("Reading execution bookmark code")))
        .expect(1)
        .mount(&server)
        .await;
    let provider = crate::utility_model::AnthropicUtilityModel::from_lookup(Some("test-key".to_owned()), |_| None)
        .with_endpoint(format!("{}/v1/messages", server.uri()));

    let mut raw = rollout_fixture_tool_heavy();
    raw.extend(big_shorthand_cell("call_big"));
    let normalized = normalize_like_slot_loop(raw);
    let outcome = summarize_transcript(&provider, &normalized).await;
    assert!(
        matches!(&outcome, SummarizerOutcome::Success(text) if text == "Reading execution bookmark code"),
        "got {outcome:?}"
    );

    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let sent = body["messages"][0]["content"].as_str().unwrap();
    assert_eq!(tool_call_lines(sent).len(), 7, "{sent}");
    assert_eq!(tool_result_lines(sent).len(), 7, "{sent}");
    assert!(sent.contains("rg -n 'retire|recovery'"), "{sent}");
    assert_eq!(body["tool_choice"]["name"], STATUS_TOOL_NAME);
}
