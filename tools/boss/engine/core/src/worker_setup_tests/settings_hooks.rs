use super::super::*;
use super::helpers::*;

#[test]
fn settings_json_is_valid_json_with_all_seven_hooks() {
    let input = sample_input();
    let rendered = render_settings_json(&input);
    let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    let hooks = parsed.get("hooks").unwrap().as_object().unwrap();
    for name in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "Notification",
        "SessionEnd",
    ] {
        assert!(hooks.contains_key(name), "missing hook: {name}");
        let entries = hooks.get(name).unwrap().as_array().unwrap();
        // The boss-event shim is always the first entry for every
        // hook event. `PreToolUse` carries extra entries (the
        // deterministic path guard, the always-on boss-launch guard,
        // plus a revision-only guard); the other six events are wired
        // exactly once.
        assert!(!entries.is_empty(), "{name} has no hook entries");
        assert_eq!(entries[0]["matcher"], "*");
        if name != "PreToolUse" {
            assert_eq!(entries.len(), 1, "{name} should have exactly one hook entry");
        }
    }
}

#[test]
fn settings_json_threads_socket_lease_and_shim_into_command() {
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(command.contains("events.sock"));
    assert!(command.contains("lease-uuid-abc"));
    assert!(command.contains("boss-event"));
    assert!(command.starts_with("BOSS_EVENTS_SOCKET="));
}

#[test]
fn settings_json_inlines_workspace_into_every_hook_command() {
    // The shim writes its on-disk event buffer relative to
    // `BOSS_WORKSPACE` when the engine socket is unreachable. The
    // hook command must inline-prefix this env var so the buffer
    // lives in the lease's workspace regardless of cwd.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let workspace_str = input.workspace_path.display().to_string();
    for hook_name in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "Notification",
        "SessionEnd",
    ] {
        let command = parsed["hooks"][hook_name][0]["hooks"][0]["command"]
            .as_str()
            .unwrap_or_else(|| panic!("missing command for {hook_name}"));
        assert!(
            command.contains(&format!("BOSS_WORKSPACE='{workspace_str}'")),
            "{hook_name} command missing BOSS_WORKSPACE=<workspace>: {command}",
        );
    }
}

#[test]
fn settings_json_inlines_run_id_into_every_hook_command() {
    // BOSS_RUN_ID must be inline-prefixed on every hook command so
    // the `boss-event` shim can splice `_boss_run_id` into the
    // payload regardless of whether claude propagates env from the
    // worker pane to its hook subprocess. Without this, the engine
    // can't correlate hook events to runs and the live worker
    // state stays pinned at `Spawning` for the worker's lifetime.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    for hook_name in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "Notification",
        "SessionEnd",
    ] {
        let command = parsed["hooks"][hook_name][0]["hooks"][0]["command"]
            .as_str()
            .unwrap_or_else(|| panic!("missing command for {hook_name}"));
        assert!(
            command.contains("BOSS_RUN_ID='run-sample'"),
            "{hook_name} command missing BOSS_RUN_ID=<run_id>: {command}",
        );
    }
}

#[test]
fn settings_json_pins_permissions_default_mode_to_auto() {
    // Workers must spawn in claude's "auto mode" so the soft
    // do-not-ask-the-human-for-permission instruction in the
    // system prompt is enforced at the harness level — without
    // this, a worker whose user has a global `default`
    // permission mode hangs on the first tool call and the
    // execution stalls until a human clicks yes. `auto` (not
    // `bypassPermissions`) is the intended shape: it runs
    // autonomously while still honoring the user's permission
    // allow/deny rules, which the environment policy requires.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    assert_eq!(
        parsed["permissions"]["defaultMode"],
        serde_json::Value::String("auto".into()),
        "expected permissions.defaultMode == 'auto', got: {parsed}",
    );
}

#[test]
fn shell_escape_quotes_paths_with_spaces() {
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
    // Application Support has a space — must round-trip through
    // single-quote escaping.
    assert!(command.contains("'/Users/brianduff/Library/Application Support/Boss/events.sock'"));
}

#[test]
fn shell_escape_single_quote_uses_outer_close_inner_open_pattern() {
    // Ensure paths containing single-quotes can't break out of the
    // quoting envelope. Standard POSIX trick: ' is closed, then
    // \' is appended literally, then ' reopens the quote.
    let escaped = shell_quote("a'b");
    assert_eq!(escaped, r#"'a'\''b'"#);
}
