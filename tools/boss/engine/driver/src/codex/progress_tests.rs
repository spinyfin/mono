use super::*;
use tempfile::TempDir;

#[test]
fn rollout_prose_wrapped_denial_adds_a_suspected_denial_notification() {
    // Empirical capture, rollout dialect: the exec_command tool's own
    // textual wrapper embeds "Process exited with code 0" as prose while
    // the denial itself is buried further inside that same string.
    let mut session = CodexRolloutProgressSession::new(None, None, None);
    session
        .normalize_progress_events(&json!({
            "type":"session_meta",
            "payload":{"id":"thread-rollout-denied","cwd":"/tmp/workspace"}
        }))
        .unwrap();
    session
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"function_call",
                "name":"exec_command",
                "call_id":"call-denied",
                "arguments":r#"{"cmd":"touch denied.txt; echo \"exit:$?\""}"#
            }
        }))
        .unwrap();
    let events = session
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"function_call_output",
                "call_id":"call-denied",
                "output":"Process exited with code 0\nOutput:\ntouch: denied.txt: Operation not permitted\nexit:1\n"
            }
        }))
        .unwrap();
    assert!(matches!(&events[0], WorkerEvent::PostToolUse { .. }), "got {events:?}");
    assert!(
        matches!(&events[1], WorkerEvent::Notification { message, .. } if message.contains("suspected sandbox/permission denial")),
        "got {events:?}"
    );
}

#[test]
fn rollout_structured_exit_code_adds_a_notification_without_the_marker_text() {
    let mut session = CodexRolloutProgressSession::new(None, None, None);
    session
        .normalize_progress_events(&json!({
            "type":"session_meta",
            "payload":{"id":"thread-rollout-failed","cwd":"/tmp/workspace"}
        }))
        .unwrap();
    session
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "name":"exec",
                "call_id":"call-failed",
                "input":"some-tool that fails"
            }
        }))
        .unwrap();
    let events = session
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call_output",
                "call_id":"call-failed",
                "output":"{\"output\":\"boom\\n\",\"metadata\":{\"exit_code\":1}}"
            }
        }))
        .unwrap();
    assert!(matches!(&events[0], WorkerEvent::PostToolUse { .. }), "got {events:?}");
    assert!(
        matches!(&events[1], WorkerEvent::Notification { message, .. } if message.contains("non-zero exit status")),
        "got {events:?}"
    );
}

#[test]
fn rollout_clean_output_adds_no_extra_notification() {
    let mut session = CodexRolloutProgressSession::new(None, None, None);
    session
        .normalize_progress_events(&json!({
            "type":"session_meta",
            "payload":{"id":"thread-rollout-clean","cwd":"/tmp/workspace"}
        }))
        .unwrap();
    session
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "name":"exec",
                "call_id":"call-clean",
                "input":"echo hi"
            }
        }))
        .unwrap();
    let events = session
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call_output",
                "call_id":"call-clean",
                "output":[{"type":"input_text","text":"hi\n"}]
            }
        }))
        .unwrap();
    assert_eq!(events.len(), 1, "got {events:?}");
}

/// Drive one rollout turn that runs a single code-mode cell, and return the
/// events emitted at the turn boundary.
fn guard_trace_turn(session: &mut CodexRolloutProgressSession, call_id: &str) -> Vec<WorkerEvent> {
    session
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "name":"exec",
                "call_id":call_id,
                "input":"text('hello');\n"
            }
        }))
        .unwrap();
    session
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call_output",
                "call_id":call_id,
                "output":[{"type":"input_text","text":"hello\n"}]
            }
        }))
        .unwrap();
    let events = session
        .normalize_progress_events(&json!({"type":"event_msg","payload":{"type":"task_complete"}}))
        .unwrap();
    session
        .normalize_progress_events(&json!({"type":"event_msg","payload":{"type":"task_started"}}))
        .unwrap();
    events
}

/// A `CODEX_HOME` shaped like one `write_hooks_and_attest` leaves behind: a
/// config that declares and trusts one hook, an executable wrapper, and an
/// attestation binding its bytes. Returns the wrapper so a test can break
/// the chain the way a live session can.
fn armed_codex_home(home: &Path) -> PathBuf {
    use boss_engine_codex_hook_trust::{
        HookAttestationEntry, HookTrustAttestation, ObservationProof, sha256_hex_prefixed, write_attestation_file,
    };

    let guards = home.join("guards");
    std::fs::create_dir_all(&guards).unwrap();
    let wrapper = guards.join("00_path_guard.sh");
    let body = "#!/bin/sh\nexit 0\n";
    std::fs::write(&wrapper, body).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[[hooks.PreToolUse]]\n\
             matcher = \".*\"\n\
             [[hooks.PreToolUse.hooks]]\n\
             type = \"command\"\n\
             command = \"{}\"\n\
             \n\
             [hooks.state.\"k\"]\n\
             trusted_hash = \"sha256:whatever\"\n",
            wrapper.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let attestation = HookTrustAttestation {
        codex_home: home.display().to_string(),
        config_path: home.join("config.toml").display().to_string(),
        generated_at_unix: 0,
        hooks: vec![
            HookAttestationEntry::builder()
                .key("k")
                .event("pre_tool_use")
                .command(wrapper.display().to_string())
                .matcher(".*")
                .trusted_hash("sha256:whatever")
                .guard_content_sha256(sha256_hex_prefixed(body.as_bytes()))
                .observed_trust_status("trusted")
                .build(),
        ],
        observation: ObservationProof::HooksList { codex_version: None },
    };
    write_attestation_file(&super::super::guard_chain::attestation_path(home), &attestation).unwrap();
    wrapper
}

fn started_rollout_session(home: &Path) -> CodexRolloutProgressSession {
    let mut session = CodexRolloutProgressSession::new(None, None, None).with_codex_home(Some(home.to_path_buf()));
    session
        .normalize_progress_events(&json!({
            "type":"session_meta",
            "payload":{"id":"thread-guards","cwd":"/tmp/workspace"}
        }))
        .unwrap();
    session
}

fn is_silent_signal(events: &[WorkerEvent]) -> bool {
    events.iter().any(|event| {
        matches!(event, WorkerEvent::Notification { message, .. }
            if message.starts_with(super::super::GUARDS_SILENT_MARKER))
    })
}

#[test]
fn a_turn_with_tool_calls_and_no_guard_record_anywhere_reports_guards_silent() {
    let dir = TempDir::new().unwrap();
    armed_codex_home(dir.path());
    let mut session = started_rollout_session(dir.path());

    let events = guard_trace_turn(&mut session, "call-1");
    assert!(is_silent_signal(&events), "got {events:?}");
}

#[test]
fn a_quiet_turn_after_guards_have_been_seen_reports_nothing() {
    // The structural false positive this closes: `observed_tool_calls`
    // counts every rollout `custom_tool_call`, which on a code-mode model is
    // the JavaScript cell itself. A cell that invokes no inner tool fires no
    // PreToolUse at all, so a turn that only computes or prints yielded
    // observed >= 1 with zero guard records and logged "command guardrails
    // were not enforced" at error. With the chain re-verified intact this
    // turn, one earlier record is enough to explain the quiet.
    let dir = TempDir::new().unwrap();
    armed_codex_home(dir.path());
    std::fs::write(
        dir.path().join(guard_trace::GUARD_TRACE_FILENAME),
        "{\"guard\":\"01_boss_launch_guard\",\"decision\":\"approve\"}\n",
    )
    .unwrap();
    let mut session = started_rollout_session(dir.path());

    // First turn: the guard record is read and summarised.
    let first = guard_trace_turn(&mut session, "call-1");
    assert!(
        first
            .iter()
            .any(|event| matches!(event, WorkerEvent::Notification { message, .. }
                if message.starts_with(super::super::GUARD_TRACE_MARKER))),
        "got {first:?}"
    );

    // Second turn: one cell ran, it called no tool, so no new guard record
    // exists. That must be silence, not a guardrail alarm.
    let second = guard_trace_turn(&mut session, "call-2");
    assert!(
        !is_silent_signal(&second),
        "a turn whose cell invoked no tool must not report disarmed guards: {second:?}"
    );
    assert_eq!(
        second.len(),
        1,
        "only the Stop should remain for a quiet turn: {second:?}"
    );
}

/// The verbatim `exec` cell script from the reported failure's shape: a
/// `cube pr create` whose cold `repobin` build pushes it past the cell's
/// yield window.
const PR_CELL_SCRIPT: &str = concat!(
    r#"const r = await tools.exec_command({"cmd":"cube pr create --branch boss/exec_1 --title T","#,
    r#""workdir":"/ws","yield_time_ms":30000,"max_output_tokens":2000});"#,
    "\ntext(JSON.stringify(r));"
);

fn cell_harness_session() -> CodexRolloutProgressSession {
    let mut session = CodexRolloutProgressSession::new(None, None, None);
    session
        .normalize_progress_events(&json!({
            "type":"session_meta",
            "payload":{"id":"thread-cell","cwd":"/ws"}
        }))
        .unwrap();
    session
}

fn cell_exec_call(call_id: &str, script: &str) -> Value {
    cell_exec_call_named("exec", call_id, script)
}

fn cell_exec_call_named(name: &str, call_id: &str, script: &str) -> Value {
    json!({
        "type":"response_item",
        "payload":{
            "type":"custom_tool_call",
            "name":name,
            "call_id":call_id,
            "input":script
        }
    })
}

fn cell_yield_output(call_id: &str, cell_id: u32) -> Value {
    json!({
        "type":"response_item",
        "payload":{
            "type":"custom_tool_call_output",
            "call_id":call_id,
            "output":format!("Script running with cell ID {cell_id}\nWall time 11.1 seconds\nOutput:\n")
        }
    })
}

fn cell_wait_call(call_id: &str, cell_id: u32) -> Value {
    json!({
        "type":"response_item",
        "payload":{
            "type":"function_call",
            "name":"wait",
            "call_id":call_id,
            "arguments":format!("{{\"cell_id\":\"{cell_id}\",\"yield_time_ms\":30000,\"max_tokens\":2000}}")
        }
    })
}

fn cell_session_poll_call(call_id: &str, session_id: u32) -> Value {
    cell_exec_call(
        call_id,
        &format!(
            concat!(
                r#"const r = await tools.write_stdin({{"session_id":{session_id},"chars":"","yield_time_ms":30000}});"#,
                "\ntext(JSON.stringify(r));"
            ),
            session_id = session_id
        ),
    )
}

fn cell_completed_output(call_id: &str, payload: &str) -> Value {
    json!({
        "type":"response_item",
        "payload":{
            "type":"function_call_output",
            "call_id":call_id,
            "output":[
                {"type":"input_text","text":"Script completed\nWall time 17.7 seconds\nOutput:\n"},
                {"type":"input_text","text":payload}
            ]
        }
    })
}

#[test]
fn cell_harness_exec_announces_the_shell_command_not_the_script() {
    let mut session = cell_harness_session();
    let events = session
        .normalize_progress_events(&cell_exec_call("call-pr", PR_CELL_SCRIPT))
        .unwrap();
    assert_eq!(
        events,
        vec![WorkerEvent::PreToolUse {
            session_id: "thread-cell".into(),
            tool_name: "Bash".into(),
            tool_input: json!({"command":"cube pr create --branch boss/exec_1 --title T"}),
        }],
        "the whole JavaScript cell must not render as the command"
    );
}

#[test]
fn a_yielded_cells_wait_continuation_carries_the_command_and_its_real_output() {
    // The reported failure, replayed: `cube pr create` outlives the
    // cell's yield window, so the call's own output is only a
    // placeholder and the URL arrives on the `wait`.
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call("call-pr", PR_CELL_SCRIPT))
        .unwrap();

    assert_eq!(
        session
            .normalize_progress_events(&cell_yield_output("call-pr", 1))
            .unwrap(),
        vec![WorkerEvent::ToolPoll {
            session_id: "thread-cell".into(),
            awaiting_model: true
        }],
        "a yield placeholder is not this command's result and must not be reported as one"
    );
    assert_eq!(
        session
            .normalize_progress_events(&cell_wait_call("call-wait", 1))
            .unwrap(),
        vec![WorkerEvent::ToolPoll {
            session_id: "thread-cell".into(),
            awaiting_model: false
        }],
        "a wait poll reports control transfer without re-announcing the command"
    );

    let chunk = r#"{"chunk_id":"ab","exit_code":0,"output":"https://github.com/spinyfin/mono/pull/9\n"}"#;
    let events = session
        .normalize_progress_events(&cell_completed_output("call-wait", chunk))
        .unwrap();
    assert_eq!(
        events,
        vec![WorkerEvent::PostToolUse {
            session_id: "thread-cell".into(),
            tool_name: "Bash".into(),
            tool_input: json!({"command":"cube pr create --branch boss/exec_1 --title T"}),
            tool_response: json!([
                {"type":"input_text","text":"Script completed\nWall time 17.7 seconds\nOutput:\n"},
                {"type":"input_text","text":chunk}
            ]),
        }],
        "the completion must be attributed to the originating command, not to the wait"
    );
}

/// Probe 6's `wait` output verbatim: the cell completed, but its chunk
/// carries `session_id` and no `exit_code` and only 8 of the command's
/// 12 ticks arrived — the command was still running.
const P6_RUNNING_CHUNK: &str = concat!(
    r#"{"chunk_id":"d0540d","wall_time_seconds":30.001035083,"session_id":8467,"#,
    r#""original_token_count":14,"output":"tick-1\ntick-2\ntick-3\ntick-4\ntick-5\ntick-6\ntick-7\ntick-8\n"}"#
);

#[test]
fn probe_6_replayed_end_to_end_flags_the_command_the_model_stopped_polling() {
    // The four records of `p6_hidden_exit`'s rollout, in order. The
    // `wait` comes back `Script completed` — a claim about the cell, not
    // the command — with a chunk carrying no exit code. Treating that as
    // terminal is what made the reported failure invisible: the call
    // closed, a clean `PostToolUse` went out, and the abandoned-command
    // guard never fired.
    let mut session = cell_harness_session();
    const P6_SCRIPT: &str = concat!(
        r#"const r = await tools.exec_command({"cmd":"sh -c 'for i in $(seq 1 12); do echo tick-$i; "#,
        r#"sleep 4; done; echo FINAL-LINE; exit 4'","yield_time_ms":30000});"#,
        "\ntext(JSON.stringify(r));"
    );
    const P6_COMMAND: &str = "sh -c 'for i in $(seq 1 12); do echo tick-$i; sleep 4; done; echo FINAL-LINE; exit 4'";

    session
        .normalize_progress_events(&cell_exec_call("call-p6", P6_SCRIPT))
        .unwrap();
    session
        .normalize_progress_events(&cell_yield_output("call-p6", 1))
        .unwrap();
    session
        .normalize_progress_events(&cell_wait_call("call-p6-wait", 1))
        .unwrap();
    assert_eq!(
        session
            .normalize_progress_events(&cell_completed_output("call-p6-wait", P6_RUNNING_CHUNK))
            .unwrap(),
        vec![WorkerEvent::ToolPoll {
            session_id: "thread-cell".into(),
            awaiting_model: true
        }],
        "a chunk with no exit code is not this command's result and must not be reported as one"
    );

    // The model answers `observed_exit=NONE` and the turn ends.
    let events = session
        .normalize_progress_events(&json!({
            "type":"event_msg",
            "payload":{"type":"task_complete","last_agent_message":"observed_exit=NONE","error":null}
        }))
        .unwrap();
    assert_eq!(
        events,
        vec![
            WorkerEvent::Notification {
                session_id: "thread-cell".into(),
                message: crate::codex::unobserved_command_notification(P6_COMMAND),
            },
            WorkerEvent::Stop {
                session_id: "thread-cell".into(),
                stop_hook_active: false,
                stop_reason: StopReason::Completed,
            },
        ],
        "the command the model stopped polling must be flagged unobserved"
    );
}

#[test]
fn a_cell_that_completes_with_an_exit_code_is_terminal_for_the_command() {
    // Probe 4's *second* poll: the same envelope header as probe 6's,
    // but the chunk carries `exit_code`. That, not `Script completed`,
    // is the command's terminal signal.
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call("call-p4", PR_CELL_SCRIPT))
        .unwrap();
    session
        .normalize_progress_events(&cell_yield_output("call-p4", 2))
        .unwrap();
    session
        .normalize_progress_events(&cell_wait_call("call-p4-wait", 2))
        .unwrap();
    let events = session
        .normalize_progress_events(&cell_completed_output(
            "call-p4-wait",
            r#"{"chunk_id":"5ec81c","exit_code":4,"output":"tick-12\nFINAL-LINE\n"}"#,
        ))
        .unwrap();
    assert!(matches!(&events[0], WorkerEvent::PostToolUse { .. }), "got {events:?}");
    let stop = session
        .normalize_progress_events(&json!({
            "type":"event_msg",
            "payload":{"type":"task_complete","last_agent_message":"done","error":null}
        }))
        .unwrap();
    assert!(
        matches!(stop.as_slice(), [WorkerEvent::Stop { .. }]),
        "an observed command must not be flagged abandoned, got {stop:?}"
    );
}

#[test]
fn multi_cell_session_is_not_flagged_after_a_later_poll_reports_its_exit_code() {
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call("call-start", PR_CELL_SCRIPT))
        .unwrap();
    session
        .normalize_progress_events(&cell_completed_output("call-start", P6_RUNNING_CHUNK))
        .unwrap();
    session
        .normalize_progress_events(&cell_session_poll_call("call-poll-1", 8467))
        .unwrap();
    session
        .normalize_progress_events(&cell_completed_output("call-poll-1", P6_RUNNING_CHUNK))
        .unwrap();
    session
        .normalize_progress_events(&cell_session_poll_call("call-poll-2", 8467))
        .unwrap();
    let completed = session
        .normalize_progress_events(&cell_completed_output(
            "call-poll-2",
            r#"{"chunk_id":"d0540d","session_id":8467,"exit_code":0,"output":"done\n"}"#,
        ))
        .unwrap();
    assert!(matches!(completed.as_slice(), [WorkerEvent::PostToolUse { .. }]));

    let boundary = session
        .normalize_progress_events(&json!({
            "type":"event_msg",
            "payload":{"type":"task_complete","last_agent_message":"done","error":null}
        }))
        .unwrap();
    assert!(
        matches!(boundary.as_slice(), [WorkerEvent::Stop { .. }]),
        "a session whose exit code was observed must not produce an unobserved-command notification: {boundary:?}"
    );
}

#[test]
fn one_shot_cell_preserves_its_url_and_is_flagged_at_the_turn_boundary() {
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call("call-one-shot", PR_CELL_SCRIPT))
        .unwrap();
    assert_eq!(
        session
            .normalize_progress_events(&cell_completed_output(
                "call-one-shot",
                "https://github.com/spinyfin/mono/pull/2666\n",
            ))
            .unwrap(),
        vec![WorkerEvent::PostToolUse {
            session_id: "thread-cell".into(),
            tool_name: "Bash".into(),
            tool_input: json!({"command":"cube pr create --branch boss/exec_1 --title T"}),
            tool_response: json!([
                {"type":"input_text","text":"Script completed\nWall time 17.7 seconds\nOutput:\n"},
                {"type":"input_text","text":"https://github.com/spinyfin/mono/pull/2666\n"},
            ]),
        }],
        "an unstructured cell payload must reach result consumers without confirming completion"
    );
    let boundary = session
        .normalize_progress_events(&json!({
            "type":"event_msg",
            "payload":{"type":"task_complete","last_agent_message":"done","error":null}
        }))
        .unwrap();
    assert!(matches!(
        boundary.as_slice(),
        [WorkerEvent::Notification { message, .. }, WorkerEvent::Stop { .. }]
            if message == &crate::codex::unobserved_command_notification(
                "cube pr create --branch boss/exec_1 --title T"
            )
    ));
}

#[test]
fn a_yielded_cell_that_is_never_polled_is_flagged_abandoned_at_the_turn_boundary() {
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call("call-slow", PR_CELL_SCRIPT))
        .unwrap();
    session
        .normalize_progress_events(&cell_yield_output("call-slow", 1))
        .unwrap();

    // The model stops polling and ends the turn — probe 6's abandon shape.
    let events = session
        .normalize_progress_events(&json!({
            "type":"event_msg",
            "payload":{"type":"task_complete","last_agent_message":"done","error":null}
        }))
        .unwrap();
    assert_eq!(
        events,
        vec![
            WorkerEvent::Notification {
                session_id: "thread-cell".into(),
                message: crate::codex::unobserved_command_notification("cube pr create --branch boss/exec_1 --title T"),
            },
            WorkerEvent::Stop {
                session_id: "thread-cell".into(),
                stop_hook_active: false,
                stop_reason: StopReason::Completed,
            },
        ]
    );
}

const WRAPPER_POLL_SCRIPT: &str = concat!(
    r#"let r = await tools.exec_command({cmd: "bazel test //tools/boss/engine/codex-rollout:codex-rollout_test", yield_time_ms: 30000});"#,
    r#"while (!("exit_code" in r)) { r = await tools.write_stdin({session_id: r.session_id, chars: "", yield_time_ms: 300000}); }"#,
    "text(JSON.stringify(r));",
);

#[test]
fn a_completed_functions_exec_wrapper_is_not_flagged_unobserved() {
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call_named(
            "functions.exec",
            "call-wrap",
            WRAPPER_POLL_SCRIPT,
        ))
        .unwrap();
    let completed = session
        .normalize_progress_events(&cell_completed_output(
            "call-wrap",
            r#"{"chunk_id":"done","session_id":8467,"exit_code":0,"output":"2 tests passed\n"}"#,
        ))
        .unwrap();
    assert!(
        matches!(completed.as_slice(), [WorkerEvent::PostToolUse { tool_name, tool_input, .. }]
            if tool_name == "Bash"
                && tool_input.get("command").and_then(Value::as_str)
                    == Some("bazel test //tools/boss/engine/codex-rollout:codex-rollout_test")),
        "got {completed:?}"
    );
    let boundary = session
        .normalize_progress_events(&json!({
            "type":"event_msg",
            "payload":{"type":"task_complete","last_agent_message":"done","error":null}
        }))
        .unwrap();
    assert!(
        matches!(boundary.as_slice(), [WorkerEvent::Stop { .. }]),
        "a wrapper that forwarded exit_code must not be flagged unobserved: {boundary:?}"
    );
}

#[test]
fn asynchronous_write_stdin_polling_observes_the_originating_wrapper() {
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call_named(
            "functions.exec",
            "call-wrap",
            WRAPPER_POLL_SCRIPT,
        ))
        .unwrap();
    session
        .normalize_progress_events(&cell_completed_output("call-wrap", P6_RUNNING_CHUNK))
        .unwrap();
    let poll = session
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"function_call",
                "name":"write_stdin",
                "call_id":"call-poll",
                "arguments":"{\"session_id\":8467,\"chars\":\"\"}"
            }
        }))
        .unwrap();
    assert_eq!(
        poll,
        vec![WorkerEvent::ToolPoll {
            session_id: "thread-cell".into(),
            awaiting_model: false
        }]
    );
    let completed = session
        .normalize_progress_events(&cell_completed_output(
            "call-poll",
            r#"{"chunk_id":"done","session_id":8467,"exit_code":0,"output":"2 tests passed\n"}"#,
        ))
        .unwrap();
    assert!(matches!(completed.as_slice(), [WorkerEvent::PostToolUse { .. }]));
    let boundary = session
        .normalize_progress_events(&json!({
            "type":"event_msg",
            "payload":{"type":"task_complete","last_agent_message":"done","error":null}
        }))
        .unwrap();
    assert!(
        matches!(boundary.as_slice(), [WorkerEvent::Stop { .. }]),
        "a later write_stdin poll that reports exit_code must close the wrapper: {boundary:?}"
    );
}

#[test]
fn an_abandoned_functions_exec_wrapper_is_still_flagged() {
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call_named(
            "functions.exec",
            "call-wrap",
            WRAPPER_POLL_SCRIPT,
        ))
        .unwrap();
    session
        .normalize_progress_events(&cell_completed_output("call-wrap", P6_RUNNING_CHUNK))
        .unwrap();
    let events = session
        .normalize_progress_events(&json!({
            "type":"event_msg",
            "payload":{"type":"task_complete","last_agent_message":"done","error":null}
        }))
        .unwrap();
    assert_eq!(
        events,
        vec![
            WorkerEvent::Notification {
                session_id: "thread-cell".into(),
                message: crate::codex::unobserved_command_notification(
                    "bazel test //tools/boss/engine/codex-rollout:codex-rollout_test"
                ),
            },
            WorkerEvent::Stop {
                session_id: "thread-cell".into(),
                stop_hook_active: false,
                stop_reason: StopReason::Completed,
            },
        ]
    );
}

fn task_complete_event() -> Value {
    json!({
        "type":"event_msg",
        "payload":{"type":"task_complete","last_agent_message":"done","error":null}
    })
}

fn task_started_event() -> Value {
    json!({"type":"event_msg","payload":{"type":"task_started"}})
}

#[test]
fn a_compute_only_text_load_cell_emits_no_unobserved_command_at_task_complete() {
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call("call-compute", r#"text(load("r").output);"#))
        .unwrap();
    session
        .normalize_progress_events(&cell_completed_output("call-compute", "plain text"))
        .unwrap();
    let boundary = session.normalize_progress_events(&task_complete_event()).unwrap();
    assert!(
        matches!(boundary.as_slice(), [WorkerEvent::Stop { .. }]),
        "a compute-only cell must not be drained as an unobserved command: {boundary:?}"
    );
}

#[test]
fn a_non_shell_tools_cell_emits_no_unobserved_command_at_task_complete() {
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call(
            "call-image",
            r#"await tools.view_image({path:"/tmp/image.png"});"#,
        ))
        .unwrap();
    session
        .normalize_progress_events(&cell_completed_output("call-image", "ok"))
        .unwrap();
    let boundary = session.normalize_progress_events(&task_complete_event()).unwrap();
    assert!(
        matches!(boundary.as_slice(), [WorkerEvent::Stop { .. }]),
        "a non-shell tools.* cell must not be drained as an unobserved command: {boundary:?}"
    );
}

#[test]
fn a_cross_turn_poll_is_flagged_unobserved_at_the_second_task_complete() {
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call(
            "call-wrap",
            r#"text(await tools.exec_command({cmd:"slow build"}));"#,
        ))
        .unwrap();
    session
        .normalize_progress_events(&cell_completed_output("call-wrap", P6_RUNNING_CHUNK))
        .unwrap();
    let first = session.normalize_progress_events(&task_complete_event()).unwrap();
    assert!(
        matches!(
            first.as_slice(),
            [WorkerEvent::Notification { message, .. }, WorkerEvent::Stop { .. }]
                if message == &crate::codex::unobserved_command_notification("slow build")
        ),
        "turn 1 must flag the still-running wrapper: {first:?}"
    );

    session.normalize_progress_events(&task_started_event()).unwrap();
    session
        .normalize_progress_events(&cell_session_poll_call("call-poll", 8467))
        .unwrap();
    session
        .normalize_progress_events(&cell_completed_output("call-poll", P6_RUNNING_CHUNK))
        .unwrap();
    let second = session.normalize_progress_events(&task_complete_event()).unwrap();
    assert!(
        matches!(
            second.as_slice(),
            [WorkerEvent::Notification { message, .. }, WorkerEvent::Stop { .. }]
                if message == &crate::codex::unobserved_command_notification(
                    "write_stdin session 8467 (no terminal result observed)"
                )
        ),
        "turn 2 must flag the self-resolved poll: {second:?}"
    );
}

#[test]
fn a_cross_turn_dynamic_session_poll_is_flagged_at_the_second_task_complete() {
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call(
            "call-wrap",
            r#"text(await tools.exec_command({cmd:"slow build"}));"#,
        ))
        .unwrap();
    session
        .normalize_progress_events(&cell_completed_output("call-wrap", P6_RUNNING_CHUNK))
        .unwrap();
    session.normalize_progress_events(&task_complete_event()).unwrap();

    session.normalize_progress_events(&task_started_event()).unwrap();
    session
        .normalize_progress_events(&cell_exec_call(
            "call-poll",
            r#"text(await tools.write_stdin({session_id: load("run").session_id, chars:""}));"#,
        ))
        .unwrap();
    session
        .normalize_progress_events(&cell_completed_output("call-poll", P6_RUNNING_CHUNK))
        .unwrap();
    let second = session.normalize_progress_events(&task_complete_event()).unwrap();
    assert!(
        matches!(
            second.as_slice(),
            [WorkerEvent::Notification { message, .. }, WorkerEvent::Stop { .. }]
                if message == &crate::codex::unobserved_command_notification(
                    "write_stdin session 8467 (no terminal result observed)"
                )
        ),
        "a load()/store() session id must still flag the poll: {second:?}"
    );
}

#[test]
fn a_yielded_polling_cell_is_not_flagged_after_the_origin_completes() {
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call(
            "call-wrap",
            r#"text(await tools.exec_command({cmd:"slow build"}));"#,
        ))
        .unwrap();
    session
        .normalize_progress_events(&cell_completed_output("call-wrap", P6_RUNNING_CHUNK))
        .unwrap();
    session
        .normalize_progress_events(&cell_session_poll_call("call-poll", 8467))
        .unwrap();
    session
        .normalize_progress_events(&cell_yield_output("call-poll", 2))
        .unwrap();
    session
        .normalize_progress_events(&cell_wait_call("call-wait", 2))
        .unwrap();
    session
        .normalize_progress_events(&cell_completed_output(
            "call-wait",
            r#"{"chunk_id":"done","session_id":8467,"exit_code":0,"output":"done\n"}"#,
        ))
        .unwrap();
    let boundary = session.normalize_progress_events(&task_complete_event()).unwrap();
    assert!(
        matches!(boundary.as_slice(), [WorkerEvent::Stop { .. }]),
        "a yielded poll whose origin later reports exit_code must not be flagged: {boundary:?}"
    );
}

#[test]
fn a_cell_harness_exit_code_is_no_longer_read_as_a_clean_command() {
    // Probe 1's shape end to end: the chunk's top-level `exit_code` sits
    // behind the harness's prose header, which used to make every Codex
    // command read as a non-error.
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call(
            "call-fail",
            r#"await tools.exec_command({"cmd":"sh -c 'exit 7'"});"#,
        ))
        .unwrap();
    let events = session
        .normalize_progress_events(&cell_completed_output(
            "call-fail",
            r#"{"chunk_id":"66d4c6","exit_code":7,"output":"LINE-ONE\n"}"#,
        ))
        .unwrap();
    assert!(matches!(&events[0], WorkerEvent::PostToolUse { .. }), "got {events:?}");
    assert!(
        matches!(&events[1], WorkerEvent::Notification { message, .. } if message.contains("non-zero exit status")),
        "got {events:?}"
    );
}

#[test]
fn a_wait_naming_an_unknown_cell_reaches_an_operator() {
    // A `NormalizeError` alone would not: the production reader treats
    // every `Err` as the expected steady state, bumps a counter and logs
    // at `debug!`. The failure has to travel on a channel the engine
    // actually surfaces, so it files an attention item instead.
    let mut session = cell_harness_session();
    let events = session
        .normalize_progress_events(&cell_wait_call("call-wait", 4))
        .unwrap();
    let [WorkerEvent::Notification { session_id, message }] = events.as_slice() else {
        panic!("an uncorrelatable continuation must not look like a clean no-op: {events:?}");
    };
    assert_eq!(session_id, "thread-cell");
    assert!(
        message.starts_with(crate::codex::UNOBSERVED_COMMAND_MARKER),
        "{message}"
    );
    assert!(message.contains("cell 4"), "{message}");
}

#[test]
fn an_output_for_an_untracked_call_reaches_an_operator() {
    let mut session = cell_harness_session();
    let events = session
        .normalize_progress_events(&cell_completed_output("ghost", r#"{"exit_code":0,"output":"x"}"#))
        .unwrap();
    let [WorkerEvent::Notification { message, .. }] = events.as_slice() else {
        panic!("an unattributable tool output must not look like a clean no-op: {events:?}");
    };
    assert!(
        message.starts_with(crate::codex::UNOBSERVED_COMMAND_MARKER),
        "{message}"
    );
    assert!(message.contains("ghost"), "{message}");
}

#[test]
fn a_new_session_cannot_resolve_a_previous_sessions_cell() {
    let mut session = cell_harness_session();
    session
        .normalize_progress_events(&cell_exec_call("call-pr", PR_CELL_SCRIPT))
        .unwrap();
    session
        .normalize_progress_events(&cell_yield_output("call-pr", 1))
        .unwrap();
    session
        .normalize_progress_events(&json!({
            "type":"session_meta",
            "payload":{"id":"thread-cell-2","cwd":"/ws"}
        }))
        .unwrap();
    let events = session
        .normalize_progress_events(&cell_wait_call("call-wait", 1))
        .unwrap();
    let [WorkerEvent::Notification { session_id, message }] = events.as_slice() else {
        panic!("a stale cell must not resolve silently: {events:?}");
    };
    assert_eq!(session_id, "thread-cell-2");
    assert!(
        message.starts_with(crate::codex::UNOBSERVED_COMMAND_MARKER),
        "{message}"
    );
}

#[test]
fn transcript_rendering_correlates_the_wait_continuation_to_its_command() {
    let mut session = CodexTranscriptSession::default();
    assert_eq!(
        session.normalize_transcript_entry(cell_exec_call("call-pr", PR_CELL_SCRIPT)),
        json!({
            "type":"assistant",
            "content":[{
                "type":"tool_use",
                "name":"Bash",
                "input":{"command":"cube pr create --branch boss/exec_1 --title T"},
            }]
        })
    );
    assert_eq!(
        session.normalize_transcript_entry(cell_yield_output("call-pr", 1)),
        lifecycle_system(
            "cell_running",
            "command still running; no result delivered yet (cell 1)"
        ),
        "a yield placeholder is not a tool result, and must say why there is none yet"
    );
    assert_eq!(
        session.normalize_transcript_entry(cell_wait_call("call-wait", 1)),
        json!({"type":"system"}),
        "a wait poll must not render as a tool named `wait`"
    );
    let rendered = session.normalize_transcript_entry(cell_completed_output(
        "call-wait",
        r#"{"chunk_id":"done","session_id":1,"exit_code":0,"output":"done\n"}"#,
    ));
    assert_eq!(rendered.get("type").and_then(Value::as_str), Some("tool_result"));
    assert!(
        rendered
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|content| content.contains("done\n")),
        "completed cell output must be preserved: {rendered}"
    );
    assert_eq!(
        rendered.get("tool_input"),
        Some(&json!({"command":"cube pr create --branch boss/exec_1 --title T"}))
    );
}

#[test]
fn a_command_whose_result_never_arrives_says_so_in_the_transcript() {
    // Probe 6 in the transcript: the assistant `tool_use` is followed by
    // a poll that still does not deliver a result, and the model stops.
    // There is no turn-boundary drain here, so this note is the only
    // thing a reader (or a marker scan) has to explain the missing
    // result.
    let mut session = CodexTranscriptSession::default();
    session.normalize_transcript_entry(cell_exec_call("call-p6", PR_CELL_SCRIPT));
    session.normalize_transcript_entry(cell_yield_output("call-p6", 1));
    session.normalize_transcript_entry(cell_wait_call("call-p6-wait", 1));
    assert_eq!(
        session.normalize_transcript_entry(cell_completed_output("call-p6-wait", P6_RUNNING_CHUNK)),
        lifecycle_system(
            "cell_running",
            "command still running; no result delivered yet (cell 1)"
        ),
        "a `Script completed` with no exit code is not the command's result"
    );
}

#[test]
fn an_unconfirmed_cell_result_keeps_its_body_in_the_transcript() {
    let mut session = CodexTranscriptSession::default();
    session.normalize_transcript_entry(cell_exec_call("call-url", PR_CELL_SCRIPT));
    let rendered = session.normalize_transcript_entry(cell_completed_output(
        "call-url",
        "https://github.com/spinyfin/mono/pull/2666\n",
    ));
    assert_eq!(rendered.get("type").and_then(Value::as_str), Some("tool_result"));
    assert_eq!(
        rendered.get("content").and_then(Value::as_str),
        Some("https://github.com/spinyfin/mono/pull/2666\n")
    );
}

/// The readoption contract for this session's per-run state.
///
/// A session rebuilt from [`ProgressSessionNormalizer::resume_state`] must
/// carry the guard-trace read cursor and the "guards have been seen"
/// fact forward. Both are read cursors over an append-only log that the
/// rollout bytes themselves say nothing about, so a fresh session cannot
/// re-derive either one — it can only re-announce what the run already
/// reported.
#[test]
fn a_resumed_session_does_not_re_announce_guard_decisions_it_already_read() {
    let dir = TempDir::new().unwrap();
    armed_codex_home(dir.path());
    std::fs::write(
        dir.path().join(guard_trace::GUARD_TRACE_FILENAME),
        "{\"guard\":\"01_boss_launch_guard\",\"decision\":\"approve\"}\n",
    )
    .unwrap();

    // The pre-restart engine: one turn, whose boundary reads and reports
    // the one guard record.
    let mut before = started_rollout_session(dir.path());
    let first = guard_trace_turn(&mut before, "call-1");
    assert!(
        first
            .iter()
            .any(|event| matches!(event, WorkerEvent::Notification { message, .. }
                if message.starts_with(super::super::GUARD_TRACE_MARKER))),
        "precondition — the first turn reports the guard record: {first:?}"
    );
    let state = before.resume_state().expect("a rollout session snapshots its state");

    // The post-restart engine: a brand new session over the same run,
    // re-seeded from that snapshot rather than from zero.
    let mut after = CodexRolloutProgressSession::new(None, None, None).with_codex_home(Some(dir.path().to_path_buf()));
    after.restore_resume_state(&state).unwrap();

    let second = guard_trace_turn(&mut after, "call-2");
    assert!(
        !second
            .iter()
            .any(|event| matches!(event, WorkerEvent::Notification { message, .. }
                if message.starts_with(super::super::GUARD_TRACE_MARKER))),
        "a restart must not re-report a guard decision the run already reported: {second:?}"
    );
    assert!(
        !second
            .iter()
            .any(|event| matches!(event, WorkerEvent::Notification { message, .. }
                if message.starts_with(super::super::GUARDS_SILENT_MARKER))),
        "guards were seen before the restart, so the first post-restart turn must not claim \
         they were inert: {second:?}"
    );
    assert_eq!(second.len(), 1, "only the Stop should remain: {second:?}");
}

/// The negative control for the test above: without the restore, the same
/// fresh session re-announces the decision and mis-reports the run. This
/// is what a readoption that rebuilt the session from nothing would do.
#[test]
fn an_unrestored_session_re_announces_guard_decisions_and_proves_the_restore_matters() {
    let dir = TempDir::new().unwrap();
    armed_codex_home(dir.path());
    std::fs::write(
        dir.path().join(guard_trace::GUARD_TRACE_FILENAME),
        "{\"guard\":\"01_boss_launch_guard\",\"decision\":\"approve\"}\n",
    )
    .unwrap();
    let mut before = started_rollout_session(dir.path());
    let _ = guard_trace_turn(&mut before, "call-1");

    // What a from-scratch re-attachment looks like: the session_meta
    // record at the head of the file is read a second time (which is its
    // own duplicate `SessionStart`), and nothing carries the guard cursor.
    let mut after = started_rollout_session(dir.path());
    let second = guard_trace_turn(&mut after, "call-2");
    assert!(
        second
            .iter()
            .any(|event| matches!(event, WorkerEvent::Notification { message, .. }
                if message.starts_with(super::super::GUARD_TRACE_MARKER))),
        "the already-read guard record is reported a second time: {second:?}"
    );
}

/// A tool call whose output record has not arrived yet is correlation the
/// rollout carries only once. Losing it across a restart turns the
/// matching `function_call_output` into an unpaired record and silently
/// drops a `PostToolUse` for a tool call that really did complete.
#[test]
fn a_resumed_session_still_pairs_a_tool_call_that_started_before_the_restart() {
    let mut before = CodexRolloutProgressSession::new(None, None, None);
    before
        .normalize_progress_events(&json!({
            "type":"session_meta",
            "payload":{"id":"thread-split","cwd":"/tmp/workspace"}
        }))
        .unwrap();
    before
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"function_call",
                "name":"exec_command",
                "call_id":"call-split",
                "arguments":r#"{"cmd":"printf hi"}"#
            }
        }))
        .unwrap();
    let state = before.resume_state().unwrap();

    let mut after = CodexRolloutProgressSession::new(None, None, None);
    after.restore_resume_state(&state).unwrap();
    let events = after
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"function_call_output",
                "call_id":"call-split",
                "output":"hi\n"
            }
        }))
        .unwrap();
    assert!(
        matches!(
            events.as_slice(),
            [WorkerEvent::PostToolUse { session_id, tool_name, .. }]
                if session_id == "thread-split" && tool_name == "Bash"
        ),
        "the restored session must still know both the thread and the open call: {events:?}"
    );
}

/// A snapshot that lost fields — a truncated write, an older shape — must
/// fail deserialization rather than default its way to an all-zero
/// session. Accepting `{}` would hand `apply` exactly the state the type's
/// own doc calls fatal (no thread id, a zero guard cursor, no guard
/// records seen) and report `Ok` while doing it, so the caller's loud
/// path — the attention item — would never fire.
#[test]
fn a_degenerate_resume_snapshot_is_rejected_rather_than_defaulted() {
    let mut session = CodexRolloutProgressSession::new(None, None, None);
    let err = session
        .restore_resume_state(&json!({}))
        .expect_err("an empty object is not a session snapshot");
    assert!(err.contains("rollout resume state"), "got {err}");

    // A real snapshot with one field dropped is the truncation case, and
    // must be refused for the same reason.
    let mut full = CodexRolloutProgressSession::new(None, None, None);
    full.normalize_progress_events(&json!({
        "type":"session_meta",
        "payload":{"id":"thread-partial","cwd":"/tmp/workspace"}
    }))
    .unwrap();
    let mut state = full.resume_state().unwrap();
    state
        .as_object_mut()
        .expect("the snapshot is an object")
        .remove("guards")
        .expect("precondition: the field was there to drop");
    assert!(
        session.restore_resume_state(&state).is_err(),
        "a snapshot missing a field must not silently resume with that field zeroed",
    );

    // `current_thread_id` is the one genuine exception: `None` is a state
    // a session really holds, before its `session_meta` record.
    let mut without_thread = full.resume_state().unwrap();
    without_thread
        .as_object_mut()
        .unwrap()
        .remove("current_thread_id")
        .unwrap();
    session
        .restore_resume_state(&without_thread)
        .expect("an absent thread id is a meaningful value, not a lost field");
}

#[test]
fn a_chain_broken_after_guards_were_seen_still_reports_guards_silent() {
    // The session-lifetime defect, measured live on codex-cli 0.145.0: a
    // TUI session whose `$CODEX_HOME/guards` was removed between two turns
    // ran turn 2's shell command with zero guard records, and Codex said
    // nothing. `guard_records_seen` was already latched by turn 1, so the
    // one signal that names this condition stayed silent for good.
    let dir = TempDir::new().unwrap();
    armed_codex_home(dir.path());
    std::fs::write(
        dir.path().join(guard_trace::GUARD_TRACE_FILENAME),
        "{\"guard\":\"01_boss_launch_guard\",\"decision\":\"approve\"}\n",
    )
    .unwrap();
    let mut session = started_rollout_session(dir.path());

    let first = guard_trace_turn(&mut session, "call-1");
    assert!(!is_silent_signal(&first), "turn 1 is guarded: {first:?}");

    std::fs::remove_dir_all(dir.path().join("guards")).unwrap();

    let second = guard_trace_turn(&mut session, "call-2");
    assert!(
        is_silent_signal(&second),
        "a chain removed mid-session must still be reported: {second:?}"
    );

    // And it keeps firing: a guardrail that is still inert on the next turn
    // is still a defect, not a one-shot notice.
    let third = guard_trace_turn(&mut session, "call-3");
    assert!(is_silent_signal(&third), "got {third:?}");
}

#[test]
fn a_broken_chain_does_not_suppress_the_surviving_guards_trace() {
    // `verify_armed_chain_on_disk` stops at the first bad entry, so one
    // wrapper removed of several leaves the rest live and recording. Their
    // decisions — a block, a guard error — are exactly what an operator
    // needs while the chain is degraded, so the turn must report both.
    let dir = TempDir::new().unwrap();
    armed_codex_home(dir.path());
    let trace = dir.path().join(guard_trace::GUARD_TRACE_FILENAME);
    std::fs::write(
        &trace,
        "{\"guard\":\"01_boss_launch_guard\",\"decision\":\"approve\"}\n",
    )
    .unwrap();
    let mut session = started_rollout_session(dir.path());

    let first = guard_trace_turn(&mut session, "call-1");
    assert!(!is_silent_signal(&first), "turn 1 is guarded: {first:?}");

    std::fs::remove_dir_all(dir.path().join("guards")).unwrap();
    std::fs::write(
        &trace,
        "{\"guard\":\"01_boss_launch_guard\",\"decision\":\"approve\"}\n\
         {\"guard\":\"02_push_guard\",\"decision\":\"block\",\"reason\":\"jj git push\"}\n",
    )
    .unwrap();

    let second = guard_trace_turn(&mut session, "call-2");
    assert!(
        is_silent_signal(&second),
        "the broken chain must be reported: {second:?}"
    );
    let trace_summary = second
        .iter()
        .find_map(|event| match event {
            WorkerEvent::Notification { message, .. } if message.starts_with(super::super::GUARD_TRACE_MARKER) => {
                Some(message.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("the surviving guards' decisions must still be reported: {second:?}"));
    assert!(
        trace_summary.contains("jj git push"),
        "the block a live guard recorded must reach the operator: {trace_summary}"
    );
}

#[test]
fn a_broken_chain_is_reported_even_on_a_turn_that_ran_no_tool_call() {
    // A broken chain is broken whether or not the model happened to call a
    // tool, and reporting it early is what gives an operator a chance to
    // act before the next command runs unguarded.
    let dir = TempDir::new().unwrap();
    armed_codex_home(dir.path());
    let mut session = started_rollout_session(dir.path());
    std::fs::remove_dir_all(dir.path().join("guards")).unwrap();

    let events = session
        .normalize_progress_events(&json!({"type":"event_msg","payload":{"type":"task_complete"}}))
        .unwrap();
    assert!(is_silent_signal(&events), "got {events:?}");
}

#[test]
fn a_reader_with_no_codex_home_makes_no_claim_about_arming() {
    // Fixtures and the stateless path have no run-private CODEX_HOME. They
    // must not manufacture a guardrail alarm out of that absence.
    let mut session = CodexRolloutProgressSession::new(None, None, None);
    session
        .normalize_progress_events(&json!({
            "type":"session_meta",
            "payload":{"id":"thread-guards","cwd":"/tmp/workspace"}
        }))
        .unwrap();
    let events = guard_trace_turn(&mut session, "call-1");
    assert!(!is_silent_signal(&events), "got {events:?}");
}

#[test]
fn rollout_records_become_canonical_renderable_records() {
    let agent_echo = normalize_rollout(json!({
        "type":"event_msg",
        "payload":{"type":"agent_message","message":"answer"}
    }));
    assert_eq!(agent_echo, json!({"type":"system"}));

    let injected_user = normalize_rollout(json!({
        "type":"response_item",
        "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"context"}]}
    }));
    assert_eq!(injected_user, json!({"type":"system"}));

    let empty_assistant = normalize_rollout(json!({
        "type":"response_item",
        "payload":{"type":"message","role":"assistant","content":[]}
    }));
    assert_eq!(empty_assistant, json!({"type":"system"}));

    let aborted = normalize_rollout(json!({
        "type":"event_msg",
        "payload":{"type":"turn_aborted","turn_id":"turn-1","reason":"interrupted"}
    }));
    // Lifecycle fillers are system (not assistant) so they don't count as
    // worker prose for marker scans / flush-race retries.
    assert_eq!(aborted["type"], "system");
    assert_eq!(aborted["subtype"], "turn_aborted");
    assert_eq!(aborted["message"], "turn aborted: interrupted");

    let started = normalize_rollout(json!({
        "type":"event_msg",
        "payload":{"type":"task_started","turn_id":"turn-1"}
    }));
    assert_eq!(started["type"], "system");
    assert_eq!(started["subtype"], "task_started");
    assert_eq!(started["message"], "turn started");

    let bare_complete = normalize_rollout(json!({
        "type":"event_msg",
        "payload":{"type":"task_complete","turn_id":"turn-1"}
    }));
    assert_eq!(bare_complete["type"], "system");
    assert_eq!(bare_complete["message"], "turn completed");

    let complete_with_prose = normalize_rollout(json!({
        "type":"event_msg",
        "payload":{
            "type":"task_complete",
            "turn_id":"turn-1",
            "last_agent_message":"[blocked] reason=\"needs a decision\""
        }
    }));
    assert_eq!(complete_with_prose["type"], "assistant");
    assert_eq!(
        complete_with_prose["content"][0]["text"],
        "[blocked] reason=\"needs a decision\""
    );

    let mut session = CodexTranscriptSession::default();
    let call = session.normalize(json!({
        "type":"response_item",
        "payload":{
            "type":"custom_tool_call",
            "call_id":"call-1",
            "name":"exec",
            "input":"echo rollout"
        }
    }));
    assert_eq!(call["type"], "assistant");
    assert_eq!(call["content"][0]["name"], "Bash");
    assert_eq!(call["content"][0]["input"], json!({"command":"echo rollout"}));

    let output = session.normalize(json!({
        "type":"response_item",
        "payload":{
            "type":"custom_tool_call_output",
            "call_id":"call-1",
            "output":[{"type":"input_text","text":"rollout\n"}]
        }
    }));
    assert_eq!(output["type"], "tool_result");
    assert_eq!(output["tool_name"], "Bash");
    assert_eq!(output["tool_input"], json!({"command":"echo rollout"}));
    assert_eq!(output["content"], "rollout\n");
    assert_eq!(output["is_error"], false);
}

#[test]
fn task_started_rearms_last_agent_message_fallback_for_each_turn() {
    let mut session = CodexTranscriptSession::default();

    session.normalize(json!({"type":"event_msg","payload":{"type":"task_started"}}));
    assert_eq!(
        session.normalize(json!({
            "type":"response_item",
            "payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"first"}]}
        }))["type"],
        "assistant"
    );
    let first_complete = session.normalize(json!({
        "type":"event_msg",
        "payload":{"type":"task_complete","last_agent_message":"first"}
    }));
    assert_eq!(first_complete["type"], "system");
    assert_eq!(first_complete["subtype"], "task_complete");

    session.normalize(json!({"type":"event_msg","payload":{"type":"task_started"}}));
    let second_complete = session.normalize(json!({
        "type":"event_msg",
        "payload":{"type":"task_complete","last_agent_message":"final"}
    }));
    assert_eq!(second_complete["type"], "assistant");
    assert_eq!(second_complete["content"][0]["text"], "final");
}

#[test]
fn real_rollout_dialect_maps_directly_to_ordered_worker_milestones() {
    let mut session = CodexRolloutProgressSession::new(
        Some("run-real".into()),
        None,
        Some(PathBuf::from("/tmp/rollout-real.jsonl")),
    );
    let records = [
        json!({
            "type":"session_meta",
            "payload":{"id":"thread-real","cwd":"/tmp/workspace"}
        }),
        json!({
            "type":"event_msg",
            "payload":{"type":"task_started","turn_id":"turn-real"}
        }),
        json!({
            "type":"response_item",
            "payload":{
                "type":"function_call",
                "name":"exec_command",
                "call_id":"call-real",
                "arguments":r#"{"cmd":"gh pr create --title test"}"#
            }
        }),
        json!({
            "type":"response_item",
            "payload":{
                "type":"function_call_output",
                "call_id":"call-real",
                "output":"https://github.com/example/repo/pull/7\n"
            }
        }),
        json!({
            "type":"event_msg",
            "payload":{
                "type":"task_complete",
                "turn_id":"turn-real",
                "last_agent_message":"done"
            }
        }),
    ];
    let events = records
        .iter()
        .flat_map(|record| session.normalize_progress_events(record).unwrap())
        .collect::<Vec<_>>();

    assert!(matches!(
        &events[0],
        WorkerEvent::SessionStart { session_id, .. } if session_id == "thread-real"
    ));
    assert!(matches!(
        &events[1],
        WorkerEvent::UserPromptSubmit { session_id, .. } if session_id == "thread-real"
    ));
    assert_eq!(
        events[2],
        WorkerEvent::PreToolUse {
            session_id: "thread-real".into(),
            tool_name: "Bash".into(),
            tool_input: json!({"command":"gh pr create --title test"}),
        }
    );
    assert_eq!(
        events[3],
        WorkerEvent::PostToolUse {
            session_id: "thread-real".into(),
            tool_name: "Bash".into(),
            tool_input: json!({"command":"gh pr create --title test"}),
            tool_response: json!("https://github.com/example/repo/pull/7\n"),
        }
    );
    assert_eq!(
        events[4],
        WorkerEvent::Stop {
            session_id: "thread-real".into(),
            stop_hook_active: false,
            stop_reason: StopReason::Completed,
        }
    );
    assert_eq!(
        session.transcript_path_for_session(&records[4]).as_deref(),
        Some("/tmp/rollout-real.jsonl")
    );
}

/// Records captured verbatim from a codex-cli 0.160.1 rollout written by
/// the driver's own spawn line (bare TUI, `--no-alt-screen -a never`,
/// `gpt-6-astra`) on 2026-10-06, with ids and paths shortened and the
/// multi-kilobyte `base_instructions` / `world_state` bodies dropped.
/// Two turns: a code-mode `exec` cell that ran `echo hello-boss-qual`
/// and completed, then a `sleep 45` cell whose turn was interrupted with
/// one Escape. The interleaved `item_completed`, `thread_settings_applied`,
/// `token_usage_record`, `world_state` and `turn_context` records are
/// what 0.160.1 (and 0.153.4) write around them; the normaliser declines
/// each one (`UnknownEvent`) and the stream consumer skips it, so the
/// milestone fanout below must come out identical with them present.
const ROLLOUT_0_160_1: &str = concat!(
    r#"{"timestamp":"2026-10-07T01:59:04.958Z","ordinal":0,"type":"session_meta","payload":{"creator_user_id":"user-x","creator_account_id":"acct-x","session_id":"thread-0160","id":"thread-0160","timestamp":"2026-10-07T01:59:04.872Z","cwd":"/ws/mono-agent-001","runtime_workspace_roots":["/ws/mono-agent-001"],"originator":"codex-tui","cli_version":"0.160.1","source":"cli","thread_source":"user","model_provider":"openai"}}"#,
    "\n",
    r#"{"timestamp":"2026-10-07T01:59:04.959Z","ordinal":1,"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1","root_turn_id":"turn-1","started_at":1791338344,"model_context_window":258400,"collaboration_mode_kind":"default"}}"#,
    "\n",
    r#"{"timestamp":"2026-10-07T01:59:06.803Z","ordinal":6,"type":"world_state","payload":{"full":true,"state":{"agents_md":{},"apps_instructions":false}}}"#,
    "\n",
    r#"{"timestamp":"2026-10-07T01:59:06.804Z","ordinal":7,"type":"turn_context","payload":{"turn_id":"turn-1","root_turn_id":"turn-1","disabled_plugin_ids":[],"cwd":"/ws/mono-agent-001","workspace_roots":["/ws/mono-agent-001"]}}"#,
    "\n",
    r#"{"timestamp":"2026-10-07T01:59:06.809Z","ordinal":9,"type":"event_msg","payload":{"type":"item_completed","thread_id":"thread-0160","turn_id":"turn-1","item":{"type":"UserMessage","id":"um-1","content":[{"type":"text","text":"Run the shell command `echo hello-boss-qual` and then reply with exactly: DONE-ONE"}]}}}"#,
    "\n",
    r#"{"timestamp":"2026-10-07T01:59:09.301Z","ordinal":12,"type":"response_item","payload":{"type":"custom_tool_call","id":"ctc-1","status":"completed","call_id":"call-1","name":"exec","input":"text(await tools.exec_command({cmd:\"echo hello-boss-qual\"}));\n","internal_chat_message_metadata_passthrough":{"turn_id":"turn-1","create_time":1791338347.279956}}}"#,
    "\n",
    r#"{"timestamp":"2026-10-07T01:59:09.318Z","ordinal":13,"type":"token_usage_record","payload":{"thread_id":"thread-0160","turn_id":"turn-1","session_id":"thread-0160","root_turn_id":"turn-1","response_id":"resp-1","usage":{"input_tokens":13794,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":40,"reasoning_output_tokens":0,"total_tokens":13834}}}"#,
    "\n",
    r#"{"timestamp":"2026-10-07T01:59:09.659Z","ordinal":15,"type":"response_item","payload":{"type":"custom_tool_call_output","id":"ctco-1","call_id":"call-1","output":[{"type":"input_text","text":"Script completed\nWall time 0.2 seconds\nOutput:\n"},{"type":"input_text","text":"{\"chunk_id\":\"741396\",\"wall_time_seconds\":0.00000725,\"exit_code\":0,\"original_token_count\":4,\"output\":\"hello-boss-qual\\n\"}"}],"internal_chat_message_metadata_passthrough":{"turn_id":"turn-1","create_time":1791338349.659072}},"metadata":{"client_authored":false,"fallback_token_limit_override":12000}}"#,
    "\n",
    r#"{"timestamp":"2026-10-07T01:59:09.659Z","ordinal":16,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":13794,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":40,"reasoning_output_tokens":0,"total_tokens":13834}}}}"#,
    "\n",
    r#"{"timestamp":"2026-10-07T01:59:13.087Z","ordinal":21,"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","last_agent_message":"DONE-ONE","started_at":1791338344,"completed_at":1791338353,"duration_ms":8129,"time_to_first_token_ms":3668}}"#,
    "\n",
    r#"{"timestamp":"2026-10-07T01:59:16.624Z","ordinal":22,"type":"event_msg","payload":{"type":"thread_settings_applied","thread_id":"thread-0160","thread_settings":{"model":"gpt-6-astra","model_provider_id":"openai","service_tier":"default","approval_policy":"never","approvals_reviewer":"user","permission_profile":{"type":"disabled"},"cwd":"/ws/mono-agent-001"}}}"#,
    "\n",
    r#"{"timestamp":"2026-10-07T01:59:16.625Z","ordinal":23,"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2","root_turn_id":"turn-2","started_at":1791338356,"model_context_window":258400,"collaboration_mode_kind":"default"}}"#,
    "\n",
    r#"{"timestamp":"2026-10-07T01:59:19.457Z","ordinal":31,"type":"response_item","payload":{"type":"custom_tool_call","id":"ctc-2","status":"completed","call_id":"call-2","name":"exec","input":"text(await tools.exec_command({cmd:\"sleep 45 && echo late-output\",yield_time_ms:1000}));\n","internal_chat_message_metadata_passthrough":{"turn_id":"turn-2","create_time":1791338357.071264}}}"#,
    "\n",
    r#"{"timestamp":"2026-10-07T01:59:20.674Z","ordinal":33,"type":"response_item","payload":{"type":"custom_tool_call_output","id":"ctco-2","call_id":"call-2","output":[{"type":"input_text","text":"Script completed\nWall time 1.2 seconds\nOutput:\n"},{"type":"input_text","text":"{\"chunk_id\":\"f2fe3e\",\"wall_time_seconds\":1.001832083,\"session_id\":80143,\"original_token_count\":0,\"output\":\"\"}"}],"internal_chat_message_metadata_passthrough":{"turn_id":"turn-2","create_time":1791338360.674208}},"metadata":{"client_authored":false,"fallback_token_limit_override":12000}}"#,
    "\n",
    r#"{"timestamp":"2026-10-07T01:59:26.574Z","ordinal":40,"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"turn-2","reason":"interrupted","started_at":1791338356,"completed_at":1791338366,"duration_ms":9948}}"#,
    "\n",
);

#[test]
fn real_0_160_1_rollout_maps_to_ordered_milestones_with_abort() {
    let mut session = CodexRolloutProgressSession::new(
        Some("run-0160".into()),
        None,
        Some(PathBuf::from("/tmp/rollout-0160.jsonl")),
    );
    let mut events = Vec::new();
    let mut declined = Vec::new();
    for line in ROLLOUT_0_160_1.lines() {
        let record: serde_json::Value = serde_json::from_str(line).expect("fixture line is JSON");
        match session.normalize_progress_events(&record) {
            Ok(batch) => events.extend(batch),
            Err(NormalizeError::UnknownEvent(name)) => declined.push(name),
            Err(other) => panic!("0.160.1 record must never be malformed to the normaliser: {other:?} for {line}"),
        }
    }
    // Everything 0.160.1 interleaves that the dialect has no milestone for
    // is either an explicitly ignored record (`world_state`,
    // `turn_context`, `token_count`) or declined by name — never
    // silently mapped to a milestone, never fatal.
    assert_eq!(
        declined,
        vec![
            "event_msg/item_completed",
            "token_usage_record",
            "event_msg/thread_settings_applied",
        ]
    );

    assert!(matches!(
        &events[0],
        WorkerEvent::SessionStart { session_id, .. } if session_id == "thread-0160"
    ));
    assert!(matches!(&events[1], WorkerEvent::UserPromptSubmit { .. }));
    // The code-mode `exec` cell surfaces as a Bash call. Both 0.153.4 and
    // 0.160.1 write the cell's argument object in JavaScript shorthand
    // (`{cmd:"…"}`, unquoted key); the cell parser recovers that `cmd`
    // so live status and abandoned-command detection see the shell
    // command, not the wrapping JavaScript.
    let command_1 = "echo hello-boss-qual";
    assert_eq!(
        events[2],
        WorkerEvent::PreToolUse {
            session_id: "thread-0160".into(),
            tool_name: "Bash".into(),
            tool_input: json!({"command": command_1}),
        }
    );
    assert!(matches!(
        &events[3],
        WorkerEvent::PostToolUse { tool_name, tool_response, .. }
            if tool_name == "Bash" && tool_response.to_string().contains("hello-boss-qual")
    ));
    assert_eq!(
        events[4],
        WorkerEvent::Stop {
            session_id: "thread-0160".into(),
            stop_hook_active: false,
            stop_reason: StopReason::Completed,
        }
    );
    // Second turn: the yielded cell, then the single-Escape abort lands as
    // Notification + Stop{Interrupted} on the ordinary boundary channel.
    assert!(matches!(&events[5], WorkerEvent::UserPromptSubmit { .. }));
    let command_2 = "sleep 45 && echo late-output";
    assert_eq!(
        events[6],
        WorkerEvent::PreToolUse {
            session_id: "thread-0160".into(),
            tool_name: "Bash".into(),
            tool_input: json!({"command": command_2}),
        }
    );
    // The `sleep 45` cell's output chunk carries a `session_id` and no
    // `exit_code`: the command was still running when the abort landed,
    // so the cell-aware tracker keeps the call open and emits no
    // PostToolUse for it — the Escape, not a result, ends the turn.
    let tail = &events[7..];
    assert!(
        !tail.iter().any(|e| matches!(e, WorkerEvent::PostToolUse { .. })),
        "a yielded, still-running cell must not be reported as finished: {tail:?}"
    );
    assert!(
        tail.iter().any(|e| matches!(e, WorkerEvent::Notification { .. })),
        "abort must carry a notification: {tail:?}"
    );
    assert_eq!(
        tail.last(),
        Some(&WorkerEvent::Stop {
            session_id: "thread-0160".into(),
            stop_hook_active: false,
            stop_reason: StopReason::Interrupted,
        })
    );
    assert_eq!(
        session
            .transcript_path_for_session(&json!({"type":"event_msg","payload":{"type":"task_complete"}}))
            .as_deref(),
        Some("/tmp/rollout-0160.jsonl")
    );
}

#[test]
fn model_output_is_progress_but_usage_retries_and_background_exits_are_not() {
    let mut session = CodexRolloutProgressSession::new(Some("run-progress".into()), None, None);
    session
        .normalize_progress_events(&json!({
            "type": "session_meta", "payload": {"id": "thread-progress"}
        }))
        .unwrap();
    for payload in [
        json!({"type": "agent_message", "message": "Working on the fix"}),
        json!({"type": "agent_reasoning", "text": "Consider the pending request"}),
    ] {
        let events = session
            .normalize_progress_events(&json!({"type": "event_msg", "payload": payload}))
            .unwrap();
        assert!(matches!(events.as_slice(), [WorkerEvent::ModelProgress { .. }]));
    }
    for payload in [
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Working"}]}),
        json!({"type": "reasoning", "encrypted_content": "opaque-model-output", "summary": []}),
        json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "Reasoning"}]}),
    ] {
        let events = session
            .normalize_progress_events(&json!({"type": "response_item", "payload": payload}))
            .unwrap();
        assert!(matches!(events.as_slice(), [WorkerEvent::ModelProgress { .. }]));
    }
    for payload in [
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Initial prompt"}]}),
        json!({"type": "message", "role": "assistant", "content": []}),
        json!({"type": "reasoning", "summary": [], "encrypted_content": null}),
    ] {
        assert!(matches!(
            session.normalize_progress_events(&json!({"type": "response_item", "payload": payload})),
            Err(NormalizeError::UnknownEvent(_))
        ));
    }
    for payload in [
        json!({"type": "token_count", "info": {"total_token_usage": {"output_tokens": 10}}}),
        json!({"type": "token_count", "info": {"total_token_usage": {"output_tokens": 10}}}),
        json!({"type": "agent_message", "message": ""}),
    ] {
        assert!(
            session
                .normalize_progress_events(&json!({"type": "event_msg", "payload": payload}))
                .unwrap()
                .is_empty()
        );
    }
    assert!(matches!(
        session.normalize_progress_events(&json!({
            "type": "event_msg", "payload": {"type": "background_terminal_exit", "exit_code": 0}
        })),
        Err(NormalizeError::UnknownEvent(_))
    ));
}

#[test]
fn rollout_custom_tool_variant_and_abort_preserve_ordered_fanout() {
    let mut session = CodexRolloutProgressSession::new(None, None, None);
    session
        .normalize_progress_events(&json!({
            "type":"session_meta",
            "payload":{"id":"thread-custom","cwd":"/tmp/workspace"}
        }))
        .unwrap();
    session
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "name":"exec",
                "call_id":"call-custom",
                "input":"printf custom"
            }
        }))
        .unwrap();
    let post = session
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call_output",
                "call_id":"call-custom",
                "output":[{"type":"input_text","text":"custom\n"}]
            }
        }))
        .unwrap();
    assert!(matches!(
        &post[0],
        WorkerEvent::PostToolUse {
            tool_name,
            tool_response,
            ..
        } if tool_name == "Bash" && tool_response.is_array()
    ));

    let terminal = session
        .normalize_progress_events(&json!({
            "type":"event_msg",
            "payload":{"type":"turn_aborted","reason":"interrupted"}
        }))
        .unwrap();
    assert!(matches!(&terminal[0], WorkerEvent::Notification { .. }));
    assert!(matches!(
        &terminal[1],
        WorkerEvent::Stop {
            stop_reason: StopReason::Interrupted,
            ..
        }
    ));
    assert!(matches!(
        session.normalize_progress_events(&json!({
            "type":"event_msg",
            "payload":{"type":"task_complete"}
        })),
        Err(NormalizeError::UnknownEvent(_))
    ));
}

#[test]
fn rollout_call_with_no_output_is_flagged_abandoned_at_task_complete() {
    let mut session = CodexRolloutProgressSession::new(None, None, None);
    session
        .normalize_progress_events(&json!({
            "type":"session_meta",
            "payload":{"id":"thread-abandon","cwd":"/tmp/workspace"}
        }))
        .unwrap();
    session
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"function_call",
                "name":"exec_command",
                "call_id":"call-abandon",
                "arguments":r#"{"cmd":"sleep 999"}"#
            }
        }))
        .unwrap();

    // No function_call_output for call-abandon ever arrives (probe 6) —
    // task_complete still fires.
    let events = session
        .normalize_progress_events(&json!({
            "type":"event_msg",
            "payload":{"type":"task_complete","turn_id":"turn-abandon"}
        }))
        .unwrap();

    assert_eq!(
        events,
        vec![
            WorkerEvent::Notification {
                session_id: "thread-abandon".into(),
                message: crate::codex::unobserved_command_notification("sleep 999"),
            },
            WorkerEvent::Stop {
                session_id: "thread-abandon".into(),
                stop_hook_active: false,
                stop_reason: StopReason::Completed,
            },
        ]
    );
}

#[test]
fn rollout_call_with_no_output_is_flagged_abandoned_at_turn_aborted() {
    let mut session = CodexRolloutProgressSession::new(None, None, None);
    session
        .normalize_progress_events(&json!({
            "type":"session_meta",
            "payload":{"id":"thread-abort","cwd":"/tmp/workspace"}
        }))
        .unwrap();
    session
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "name":"exec",
                "call_id":"call-abort",
                "input":"sleep 999"
            }
        }))
        .unwrap();

    let events = session
        .normalize_progress_events(&json!({
            "type":"event_msg",
            "payload":{"type":"turn_aborted","reason":"interrupted"}
        }))
        .unwrap();

    assert_eq!(
        events,
        vec![
            WorkerEvent::Notification {
                session_id: "thread-abort".into(),
                message: crate::codex::unobserved_command_notification("sleep 999"),
            },
            WorkerEvent::Notification {
                session_id: "thread-abort".into(),
                message: "turn aborted: interrupted".into(),
            },
            WorkerEvent::Stop {
                session_id: "thread-abort".into(),
                stop_hook_active: false,
                stop_reason: StopReason::Interrupted,
            },
        ]
    );
}

#[test]
fn rollout_pending_non_shell_call_is_not_flagged_abandoned() {
    let mut session = CodexRolloutProgressSession::new(None, None, None);
    session
        .normalize_progress_events(&json!({
            "type":"session_meta",
            "payload":{"id":"thread-patch","cwd":"/tmp/workspace"}
        }))
        .unwrap();
    session
        .normalize_progress_events(&json!({
            "type":"response_item",
            "payload":{
                "type":"custom_tool_call",
                "name":"apply_patch",
                "call_id":"call-patch",
                "input":{"patch":"*** Begin Patch\n*** End Patch"}
            }
        }))
        .unwrap();

    // No matching call output ever arrives for call-patch, but it is not
    // exec/shell-shaped (no `command` field on its tool_input), so the
    // turn boundary must not flag it as an unobserved command.
    let events = session
        .normalize_progress_events(&json!({
            "type":"event_msg",
            "payload":{"type":"turn_aborted","reason":"interrupted"}
        }))
        .unwrap();

    assert_eq!(
        events,
        vec![
            WorkerEvent::Notification {
                session_id: "thread-patch".into(),
                message: "turn aborted: interrupted".into(),
            },
            WorkerEvent::Stop {
                session_id: "thread-patch".into(),
                stop_hook_active: false,
                stop_reason: StopReason::Interrupted,
            },
        ]
    );
}

#[test]
fn argv_shaped_exec_without_output_is_flagged_abandoned() {
    for input in [json!(["sleep", "999"]), json!({"argv": ["sleep", "999"]})] {
        let mut session = CodexRolloutProgressSession::new(None, None, None);
        session
            .normalize_progress_events(&json!({"type":"session_meta","payload":{"id":"t","cwd":"/w"}}))
            .unwrap();
        session
            .normalize_progress_events(&json!({
                "type":"response_item",
                "payload":{"type":"function_call","name":"exec_command","call_id":"c","arguments":input}
            }))
            .unwrap();
        let events = session
            .normalize_progress_events(&json!({"type":"event_msg","payload":{"type":"task_complete"}}))
            .unwrap();
        assert!(
            events.iter().any(|event| matches!(
                event,
                WorkerEvent::Notification { message, .. } if message.contains("sleep")
            )),
            "{events:?}"
        );
    }
}

/// At the driver boundary: a Codex worker ran
/// `timeout 1800 checkleft run` as a background terminal and sat in
/// `Waiting for background terminal (12m 49s • esc to interrupt)`: a chain
/// of `wait` polls, each bounded by the cell harness's 30 s yield, inside
/// one turn that had not ended. The driver must report that turn as still
/// open for the whole wait — the command is announced once, the polls
/// are continuations and are deliberately silent, and no turn boundary
/// (`Stop`) exists until Codex writes `task_complete`. Idle on the pane
/// is therefore never something this stream justifies mid-wait.
#[test]
fn a_background_wait_inside_a_turn_reports_no_turn_boundary() {
    let mut session = cell_harness_session();
    const SCRIPT: &str = concat!(
        r#"const r = await tools.exec_command({"cmd":"timeout 1800 checkleft run","yield_time_ms":30000});"#,
        "\ntext(JSON.stringify(r));"
    );
    // A completed cell whose forwarded chunk carries no exit code: the
    // command is still running at the end of this 30 s poll.
    const RUNNING_CHUNK: &str = concat!(
        r#"{"chunk_id":"c1","wall_time_seconds":30.0,"session_id":17883,"#,
        r#""original_token_count":3,"output":"checking…\n"}"#
    );

    let opened = session
        .normalize_progress_events(&json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-bg"}}))
        .unwrap();
    assert!(
        matches!(opened.as_slice(), [WorkerEvent::UserPromptSubmit { .. }]),
        "task_started opens the turn: {opened:?}"
    );

    let announced = session
        .normalize_progress_events(&cell_exec_call("call-bg", SCRIPT))
        .unwrap();
    assert_eq!(
        announced,
        vec![WorkerEvent::PreToolUse {
            session_id: "thread-cell".into(),
            tool_name: "Bash".into(),
            tool_input: json!({"command": "timeout 1800 checkleft run"}),
        }],
        "the background command is announced exactly once, when it starts"
    );

    // The wait itself: the cell yields, then the model polls it again
    // (a real wait can last many minutes; three rounds here).
    // Poll records transfer control without PostToolUse (the command
    // has not finished) or Stop (the turn has not ended).
    let mut quiet = Vec::new();
    quiet.extend(
        session
            .normalize_progress_events(&cell_yield_output("call-bg", 1))
            .unwrap(),
    );
    for round in 0..3u32 {
        let wait_id = format!("call-bg-wait-{round}");
        quiet.extend(session.normalize_progress_events(&cell_wait_call(&wait_id, 1)).unwrap());
        quiet.extend(
            session
                .normalize_progress_events(&cell_completed_output(&wait_id, RUNNING_CHUNK))
                .unwrap(),
        );
    }
    assert_eq!(
        quiet,
        std::iter::once(true)
            .chain((0..3).flat_map(|_| [false, true]))
            .map(|awaiting_model| WorkerEvent::ToolPoll {
                session_id: "thread-cell".into(),
                awaiting_model
            })
            .collect::<Vec<_>>(),
        "poll boundaries track model control without balancing the command"
    );
    assert!(
        session.calls.has_open_calls(),
        "the announced command stays open for as long as the model keeps polling it"
    );

    // The turn boundary is the driver's own signal: nothing in the
    // wait is one, and the turn's `task_complete` is the first.
    let driver = crate::codex::CodexDriver::default();
    let closed = session
        .normalize_progress_events(&json!({"type":"event_msg","payload":{"type":"task_complete"}}))
        .unwrap();
    let boundaries: Vec<_> = opened
        .iter()
        .chain(announced.iter())
        .chain(closed.iter())
        .filter(|event| crate::AgentDriver::turn_boundary(&driver, event).is_some())
        .collect();
    assert!(
        matches!(boundaries.as_slice(), [WorkerEvent::Stop { .. }]),
        "the turn's Stop is the only turn boundary in the stream: {boundaries:?}"
    );
}
