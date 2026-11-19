//! Antigravity conformance: published samples and reconstructed regressions.
//!
//! `agy_captured` replays unchanged sanitized 1.5.5 home payload samples.
//! All helper-built timelines are synthetic regressions based on study
//! observations, not recordings. Settings tests check Boss's renderer and
//! validator only; they provide no live CLI permission evidence.
//! Qualified 1.5.7+ hook and permission recaptures remain unavailable here.
//! Nested/concurrent children, reordering and rewrite composition are untested.

#[path = "agy_captured.rs"]
mod captured;

use std::path::Path;

use boss_protocol::{NormalizeError, StopReason, WorkerEvent};
use serde_json::{Value, json};

use crate::conformance::fixtures::{
    AGY_HOOK_SESSION_JSONL, CANONICAL_SESSION_ID, decode_progress, expected_agy_session_events, normalize_session_id,
    project_command,
};
use crate::driver::agy::permissions::{
    AGENT_MODE, QUEUED_MESSAGES, RuleList, SettingsPlan, TOOL_PERMISSION, parse_rendered, render_settings,
    validate_rule,
};
use crate::driver::agy::{MIN_QUALIFIED_AGY_VERSION, REFUSED_TOOL_PREFIX};
use crate::driver::{AgentDriver, AgyDriver, ProgressSessionConfig, WorkerKind};

/// Parent / child conversation ids from the hook-surface study's ordinary
/// 1.5.5 child probe.
const PARENT_1_5_5: &str = "42227c9d-cdd1-4aa2-b4cf-2233178cd0d1";
const CHILD_1_5_5: &str = "74b3e2a3-2e36-4cfd-982a-bc499d905419";

/// 1.1.12 ordinary-child probe (missing parent idle Stop).
const PARENT_1_1_12: &str = "0a3a05ed-0000-4000-8000-000000000001";
const CHILD_1_1_12: &str = "233f406b-0000-4000-8000-000000000002";

/// Brief canary that 1.5.5+ repeats on every hook as `lastUserInput`.
const BRIEF_CANARY: &str = "the whole 50KB brief must not be logged";

fn decode_agy(jsonl: &str) -> Vec<WorkerEvent> {
    decode_progress(&AgyDriver::default(), jsonl)
}

fn turn_ends(jsonl: &str) -> Vec<crate::driver::TurnEnd> {
    let driver = AgyDriver::default();
    decode_agy(jsonl)
        .into_iter()
        .filter_map(|event| driver.turn_boundary(&event))
        .collect()
}

fn session_start(cid: &str) -> String {
    json!({
        "hookEventName": "SessionStart",
        "conversationId": cid,
        "modelName": "gemini-3.8-flash-low",
        "cwd": "/tmp/ws",
        "workspacePaths": ["/tmp/ws"],
        "transcriptPath": format!("/tmp/brain/{cid}/transcript_full.jsonl"),
        "lastUserInput": BRIEF_CANARY,
    })
    .to_string()
}

fn pre_tool(cid: &str, id: &str, command: &str) -> String {
    json!({
        "hookEventName": "PreToolUse",
        "conversationId": cid,
        "modelName": "gemini-3.8-flash-low",
        "lastUserInput": BRIEF_CANARY,
        "toolCall": {
            "id": id,
            "name": "run_command",
            "args": {
                "CommandLine": command,
                "WaitMsBeforeAsync": 5000,
            }
        }
    })
    .to_string()
}

fn post_tool(cid: &str, id: &str, command: &str, error: &str) -> String {
    json!({
        "hookEventName": "PostToolUse",
        "conversationId": cid,
        "error": error,
        "lastUserInput": BRIEF_CANARY,
        "toolCall": {
            "id": id,
            "name": "run_command",
            "args": {
                "CommandLine": command,
                "WaitMsBeforeAsync": 5000,
            }
        }
    })
    .to_string()
}

fn pre_invocation(cid: &str, invocation_num: u64) -> String {
    json!({
        "hookEventName": "PreInvocation",
        "conversationId": cid,
        "invocationNum": invocation_num,
        "initialNumSteps": 1,
        "lastUserInput": BRIEF_CANARY,
    })
    .to_string()
}

fn post_invocation(cid: &str, invocation_num: u64) -> String {
    json!({
        "hookEventName": "PostInvocation",
        "conversationId": cid,
        "invocationNum": invocation_num,
        "lastUserInput": BRIEF_CANARY,
    })
    .to_string()
}

fn stop(cid: &str, idle: bool, reason: &str) -> String {
    json!({
        "hookEventName": "Stop",
        "conversationId": cid,
        "fullyIdle": idle,
        "terminationReason": reason,
        "error": "",
        "executionNum": 0,
        "lastUserInput": BRIEF_CANARY,
    })
    .to_string()
}

fn jsonl(lines: &[String]) -> String {
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

fn golden_settings_plan() -> SettingsPlan<'static> {
    SettingsPlan {
        worker_kind: WorkerKind::Standard,
        workspace: Path::new("/boss-test-absent/workspaces/ws-1"),
        boss_data_dir: Some(Path::new("/boss-test-absent/Library/Application Support/Boss")),
        hooks_dir: Path::new("/boss-test-absent/agy-homes/run-1/process-home/.gemini/config"),
        control_dir: Path::new("/boss-test-absent/agy-homes/run-1/control"),
        rules_root: Path::new("/boss-test-absent/agy-homes/run-1/rules-root"),
    }
}

// ── Session settings posture ────────────────────────────────────────────────

#[test]
fn golden_session_settings_standard_worker() {
    let rendered = render_settings(&golden_settings_plan()).unwrap().to_json().unwrap();
    assert_eq!(
        rendered, GOLDEN_SETTINGS_STANDARD,
        "standard-worker --settings document must match the pinned posture byte-for-byte",
    );
}

#[test]
fn agent_mode_is_pinned_empty_so_auto_cannot_be_the_omitted_default() {
    // Permission study: an absent `agentMode` starts Auto on 1.5.7+, which
    // approves workspace edits on its own. Every kind therefore writes the
    // key as the empty string (the documented default mode).
    let rendered = render_settings(&golden_settings_plan()).unwrap().to_json().unwrap();
    let json: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(json["agentMode"], AGENT_MODE);
    assert_eq!(AGENT_MODE, "");
    assert_eq!(json["toolPermission"], TOOL_PERMISSION);
    let err = parse_rendered(r#"{"toolPermission":"request-review","trustedWorkspaces":["/ws"],"permissions":{"deny":["read_file(/data)"]}}"#)
        .expect_err("omitting agentMode must fail closed, not inherit Auto");
    let message = format!("{err:#}");
    assert!(
        message.contains("agentMode") || message.contains("missing field"),
        "expected a missing-agentMode parse error, got {message}"
    );
}

#[test]
fn golden_session_settings_pin_queued_messages_to_queue() {
    // Permission study §1 / queued-messages artifacts: the JSON key is
    // `queuedMessages` and the buffered-input value is `queue`. A CLI
    // that renames the key fails the live `--settings` probe; this
    // hermetic half pins what Boss renders.
    let json: Value = serde_json::from_str(GOLDEN_SETTINGS_STANDARD).unwrap();
    assert_eq!(json["queuedMessages"], QUEUED_MESSAGES);
    assert_eq!(QUEUED_MESSAGES, "queue");
    let err = parse_rendered(
        r#"{
            "agentMode": "",
            "toolPermission": "request-review",
            "queuedMessages": "not-a-real-mode",
            "trustedWorkspaces": ["/boss-test-absent/ws"],
            "permissions": { "deny": ["read_file(/boss-test-absent/data)"] }
        }"#,
    )
    .expect_err("invalid queuedMessages value must fail closed");
    let message = format!("{err:#}");
    assert!(
        message.contains("queuedMessages"),
        "expected a queuedMessages validation error, got {message}"
    );
}

#[test]
fn settings_parse_rejects_an_unknown_key() {
    // Permission study: `--settings` rejects an unknown key and exits 1.
    // `deny_unknown_fields` on the rendered document is the hermetic pin;
    // the live CLI check cannot run under the sandbox (CI cannot authenticate
    // agy).
    let err = parse_rendered(
        r#"{
            "agentMode": "",
            "toolPermission": "request-review",
            "trustedWorkspaces": ["/boss-test-absent/ws"],
            "permissions": { "deny": ["read_file(/boss-test-absent/data)"] },
            "toolPermissions": "typo"
        }"#,
    )
    .expect_err("unknown --settings key must fail closed");
    let message = format!("{err:#}").to_lowercase();
    assert!(
        message.contains("unknown") || message.contains("toolpermissions"),
        "expected an unknown-field error, got {message}"
    );
}

#[test]
fn command_deny_next_to_read_file_is_refused() {
    // Permission study §5: a `command(...)` deny beside a `read_file` deny
    // stopped matching. Command denies belong to the PreToolUse adapter.
    let err = validate_rule(RuleList::Deny, "command(printf)").expect_err("command denies must not render");
    let message = format!("{err:#}");
    assert!(
        message.contains("native file") || message.contains("not permitted"),
        "got {message}"
    );
    validate_rule(RuleList::Deny, "read_file(/boss-test-absent/data)").unwrap();
}

// ── Reconstructed regression timelines ──────────────────────────────────────────────────

#[test]
fn reconstructed_one_tool_turn_matches_the_canonical_sequence() {
    let events: Vec<_> = decode_agy(AGY_HOOK_SESSION_JSONL)
        .into_iter()
        .map(|e| normalize_session_id(e, CANONICAL_SESSION_ID))
        .map(project_command)
        .collect();
    assert_eq!(events, expected_agy_session_events());
    for event in &events {
        let debug = format!("{event:?}");
        assert!(
            !debug.contains(BRIEF_CANARY) && !debug.contains("the whole brief must not be logged"),
            "lastUserInput must be stripped before WorkerEvent construction: {debug}"
        );
    }
}

#[test]
fn captured_1_5_5_payloads_decode_and_strip_the_prompt() {
    let cid = "ee51fade-a19c-42d1-bf36-e96a9c531813";
    let events = decode_agy(captured::HOME_1_5_5);
    assert!(
        matches!(events.as_slice(),
            [WorkerEvent::SessionStart { session_id, .. }, WorkerEvent::PreToolUse { .. }, WorkerEvent::PostToolUse { tool_response, .. }, WorkerEvent::Stop { stop_reason, .. }]
            if session_id == cid
                && *tool_response == json!({})
                && *stop_reason == StopReason::Completed
        ),
        "got {events:?}"
    );
    let dump = format!("{events:?}");
    assert!(
        !dump.contains("HOME_PROBE_DONE"),
        "prompt leaked into WorkerEvent: {dump}"
    );
}

#[test]
fn child_idle_stop_is_not_a_turn_boundary() {
    // Hook-surface study: parent 42227c9d, child 74b3e2a3. The child's
    // fullyIdle=true Stop must not become the parent's boundary.
    let stream = jsonl(&[
        session_start(PARENT_1_5_5),
        pre_tool(PARENT_1_5_5, "call_parent", "invoke_subagent"),
        session_start(CHILD_1_5_5),
        pre_tool(CHILD_1_5_5, "call_child", "printf CHILD_HOOK_OK"),
        post_tool(CHILD_1_5_5, "call_child", "printf CHILD_HOOK_OK", ""),
        stop(CHILD_1_5_5, true, "NO_TOOL_CALL"),
        stop(PARENT_1_5_5, true, "NO_TOOL_CALL"),
    ]);
    let ends = turn_ends(&stream);
    assert_eq!(ends.len(), 1, "only the parent idle Stop is a boundary; got {ends:?}");
    assert_eq!(ends[0].session_id, PARENT_1_5_5);
    assert!(!ends[0].continuation);
}

#[test]
fn overwrite_rewrite_pair_stays_one_call_by_id() {
    // Hook-surface study: PreToolUse carries the model's CommandLine;
    // PostToolUse carries the overwritten CommandLine; correlation is the
    // toolCall.id, not the argument text. The adapter reply uses `overwrite`
    // with argument keys (not a whole toolCall object).
    let reply: Value = serde_json::from_str(OVERWRITE_REPLY).unwrap();
    assert_eq!(reply["decision"], "auto_approve");
    assert_eq!(
        reply["overwrite"]["CommandLine"],
        "printf HOOK_REWRITTEN > rewritten.txt"
    );
    assert!(reply["overwrite"].get("toolCall").is_none());
    assert!(reply["overwrite"].get("args").is_none());

    let stream = jsonl(&[
        session_start(PARENT_1_5_5),
        pre_tool(PARENT_1_5_5, "call_rewrite", "printf MODEL_ORIGINAL > original.txt"),
        post_tool(
            PARENT_1_5_5,
            "call_rewrite",
            "printf HOOK_REWRITTEN > rewritten.txt",
            "",
        ),
        stop(PARENT_1_5_5, true, "NO_TOOL_CALL"),
    ]);
    let events = decode_agy(&stream);
    assert!(
        matches!(
            events.as_slice(),
            [
                WorkerEvent::SessionStart { .. },
                WorkerEvent::PreToolUse { .. },
                WorkerEvent::PostToolUse { .. },
                WorkerEvent::Stop { .. }
            ]
        ),
        "rewrite pair must not look like a refused tool; got {events:?}"
    );
}

#[test]
fn denied_and_unknown_verdict_calls_have_no_post_tool_use() {
    // Separate synthetic streams model the two observed missing-post cases.
    for id in ["call_denied", "call_unknown_verdict"] {
        let stream = jsonl(&[
            session_start(PARENT_1_5_5),
            pre_tool(PARENT_1_5_5, id, "printf REFUSED_MARKER"),
            stop(PARENT_1_5_5, true, "NO_TOOL_CALL"),
        ]);
        let events = decode_agy(&stream);
        assert!(
            matches!(events.as_slice(),
                [WorkerEvent::SessionStart { .. }, WorkerEvent::PreToolUse { .. },
                 WorkerEvent::Notification { message, .. }, WorkerEvent::Stop { .. }]
                if message.contains(REFUSED_TOOL_PREFIX) && message.contains(id)
            ),
            "refused call {id}: {events:?}"
        );
    }
}

#[test]
fn exit_seven_post_tool_use_error_is_empty() {
    // Hook-surface study: an intentional `exit 7` still produced
    // PostToolUse.error="". Empty error is not exit 0 and is not a tool
    // failure the activity machine can observe.
    let stream = jsonl(&[
        session_start(PARENT_1_5_5),
        pre_tool(PARENT_1_5_5, "call_exit7", "sh -c 'exit 7'"),
        post_tool(PARENT_1_5_5, "call_exit7", "sh -c 'exit 7'", ""),
        stop(PARENT_1_5_5, true, "NO_TOOL_CALL"),
    ]);
    let events = decode_agy(&stream);
    let WorkerEvent::PostToolUse { tool_response, .. } = &events[2] else {
        panic!("expected PostToolUse, got {events:?}");
    };
    assert_eq!(tool_response, &json!({}));
    assert_eq!(turn_ends(&stream).len(), 1);
}

#[test]
fn background_build_non_idle_stop_continues_then_idles() {
    // Hook-surface study, real-build timeline: fullyIdle=false → delayed
    // PostToolUse → automatic PreInvocation invocationNum=0 → fullyIdle=true.
    let stream = jsonl(&[
        session_start(PARENT_1_5_5),
        pre_tool(
            PARENT_1_5_5,
            "call_build",
            "bazel build //tools/boss/engine/core:engine",
        ),
        post_invocation(PARENT_1_5_5, 0),
        pre_invocation(PARENT_1_5_5, 1),
        post_invocation(PARENT_1_5_5, 1),
        stop(PARENT_1_5_5, false, "NO_TOOL_CALL"),
        post_tool(
            PARENT_1_5_5,
            "call_build",
            "bazel build //tools/boss/engine/core:engine",
            "",
        ),
        pre_invocation(PARENT_1_5_5, 0),
        post_invocation(PARENT_1_5_5, 0),
        stop(PARENT_1_5_5, true, "NO_TOOL_CALL"),
    ]);
    let ends = turn_ends(&stream);
    assert_eq!(ends.len(), 1, "only the final idle Stop is a boundary; got {ends:?}");
    assert_eq!(ends[0].session_id, PARENT_1_5_5);
}

#[test]
fn older_missing_parent_boundary_is_not_recovered_from_the_child() {
    // Hook-surface study, 1.1.12 ordinary-child timeline: parent
    // fullyIdle=false, then child fullyIdle=true, and no later parent idle
    // Stop during observation. The child's boundary must not stand in.
    let stream = jsonl(&[
        session_start(PARENT_1_1_12),
        pre_tool(PARENT_1_1_12, "call_invoke", "invoke_subagent"),
        stop(PARENT_1_1_12, false, "NO_TOOL_CALL"),
        session_start(CHILD_1_1_12),
        pre_tool(CHILD_1_1_12, "call_child", "printf CHILD_DONE"),
        stop(CHILD_1_1_12, true, "NO_TOOL_CALL"),
    ]);
    assert!(
        turn_ends(&stream).is_empty(),
        "1.1.12 missing parent idle Stop must not be recovered from the child"
    );
}

#[test]
fn missing_hook_event_name_is_a_real_error_on_the_1_1_12_shape() {
    // 1.1.12 payloads omit hookEventName. The pin is 1.5.7+, which stamps
    // it; guessing the event from adjacent fields is not this normaliser's job.
    let raw = json!({
        "conversationId": PARENT_1_1_12,
        "fullyIdle": true,
        "terminationReason": "NO_TOOL_CALL",
    });
    let mut session = AgyDriver::default()
        .progress_session(&ProgressSessionConfig::default())
        .expect("agy declares a progress session");
    let err = session
        .normalize_progress_event(&raw)
        .expect_err("1.1.12 envelopes without hookEventName must fail closed");
    assert!(
        matches!(err, NormalizeError::MissingField("hook_event_name")),
        "got {err:?}"
    );
}

#[test]
fn declined_dialog_retains_pending_call_until_next_boundary() {
    // Permission study: declining the approval dialog ended the turn like
    // Escape — no Stop in 57 of 57 declined turns.
    let mut session = AgyDriver::default()
        .progress_session(&ProgressSessionConfig::default())
        .unwrap();
    for line in [
        session_start(PARENT_1_5_5),
        pre_tool(PARENT_1_5_5, "call_dialog", "touch /tmp/declined"),
    ] {
        let events = session
            .normalize_progress_events(&serde_json::from_str(&line).unwrap())
            .unwrap();
        assert!(!events.iter().any(|e| matches!(e, WorkerEvent::Stop { .. })));
        if line.contains("PreToolUse") {
            assert!(
                matches!(events.as_slice(), [WorkerEvent::PreToolUse { tool_input, .. }] if tool_input["command"] == "touch /tmp/declined")
            );
        }
    }
    let events = session
        .normalize_progress_events(&serde_json::from_str(&stop(PARENT_1_5_5, true, "NO_TOOL_CALL")).unwrap())
        .unwrap();
    assert!(
        matches!(events.as_slice(),
            [WorkerEvent::Notification { message, .. }, WorkerEvent::Stop { .. }]
            if message.contains("call_dialog") && message.contains(REFUSED_TOOL_PREFIX)
        ),
        "pending declined call must survive to the next boundary: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter_map(|e| AgyDriver::default().turn_boundary(e))
            .count(),
        1
    );
}

#[test]
fn qualified_pin_is_at_least_1_5_7() {
    let parts: Vec<_> = MIN_QUALIFIED_AGY_VERSION.split('.').collect();
    assert!(
        parts.len() >= 3 && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())),
        "MIN_QUALIFIED_AGY_VERSION must be X.Y.Z, got {MIN_QUALIFIED_AGY_VERSION}"
    );
    let nums: Vec<u64> = parts.iter().take(3).map(|p| p.parse().unwrap()).collect();
    assert!(
        nums >= vec![1, 5, 7],
        "conformance pin must be 1.5.7 or later (permission study); got {MIN_QUALIFIED_AGY_VERSION}"
    );
}

/// Measured rewrite-pair reply, translated to the adapter's `auto_approve`
/// decision (the study printed `allow`, which still raises a dialog in
/// review mode). Keys inside `overwrite` are argument names.
const OVERWRITE_REPLY: &str =
    r#"{"decision":"auto_approve","overwrite":{"CommandLine":"printf HOOK_REWRITTEN > rewritten.txt"}}"#;

const GOLDEN_SETTINGS_STANDARD: &str = r#"{
  "agentMode": "",
  "toolPermission": "request-review",
  "queuedMessages": "queue",
  "trustedWorkspaces": [
    "/boss-test-absent/workspaces/ws-1"
  ],
  "permissions": {
    "deny": [
      "read_file(/boss-test-absent/Library/Application Support/Boss)",
      "write_file(/boss-test-absent/Library/Application Support/Boss)",
      "write_file(/boss-test-absent/agy-homes/run-1/process-home/.gemini/config)",
      "write_file(/boss-test-absent/agy-homes/run-1/control)",
      "write_file(/boss-test-absent/agy-homes/run-1/rules-root)",
      "write_file(/boss-test-absent/agy-homes/run-1/process-home/.gemini/antigravity-cli/settings.json)"
    ]
  }
}
"#;
