use super::super::*;
use super::helpers::*;

#[test]
fn heal_hook_command_replaces_shim_path() {
    let old_cmd = "BOSS_EVENTS_SOCKET='/tmp/events.sock' BOSS_LEASE_ID='lease-1' \
                   BOSS_RUN_ID='run-1' BOSS_WORKSPACE='/tmp/ws' \
                   '/old/bazel-bin/tools/boss/event-shim/boss-event'";
    let new_path = PathBuf::from("/stable/bin/boss-event");
    let healed = heal_hook_command(old_cmd, &new_path);
    assert!(
        healed.contains("'/stable/bin/boss-event'"),
        "should contain new path: {healed}",
    );
    assert!(
        !healed.contains("/old/bazel-bin"),
        "should not contain old path: {healed}",
    );
    // Env vars and other args must be preserved unchanged.
    assert!(healed.contains("BOSS_EVENTS_SOCKET="));
    assert!(healed.contains("BOSS_WORKSPACE="));
}

#[test]
fn heal_hook_command_handles_path_with_spaces() {
    let old_cmd = "BOSS_EVENTS_SOCKET='/tmp/e.sock' BOSS_LEASE_ID='l' \
                   BOSS_RUN_ID='r' BOSS_WORKSPACE='/tmp/ws' \
                   '/Users/x/Library/Application Support/Boss/bin/boss-event'";
    let new_path = PathBuf::from("/Users/y/Library/Application Support/Boss/bin/boss-event");
    let healed = heal_hook_command(old_cmd, &new_path);
    assert!(
        healed.contains("'/Users/y/Library/Application Support/Boss/bin/boss-event'"),
        "spaces in new path must be inside single quotes: {healed}",
    );
}

#[test]
fn heal_hook_command_no_op_when_no_boss_event_present() {
    let cmd = "SOME_VAR='val' /unrelated/binary";
    let new_path = PathBuf::from("/stable/boss-event");
    let healed = heal_hook_command(cmd, &new_path);
    assert_eq!(healed, cmd, "should return original when boss-event not found");
}

#[test]
fn heal_hook_command_no_op_when_no_opening_quote_before_shim() {
    // `boss-event` is present, but there is no single quote anywhere before
    // it, so there is no quoted token to rewrite — the early return leaves
    // the command untouched (rather than corrupting an unquoted invocation).
    let cmd = "BOSS_RUN_ID=run-1 /bare/path/boss-event";
    let new_path = PathBuf::from("/stable/bin/boss-event");
    let healed = heal_hook_command(cmd, &new_path);
    assert_eq!(
        healed, cmd,
        "no opening single quote before boss-event must return the original unchanged",
    );
}

#[test]
fn heal_hook_command_no_op_when_no_closing_quote_after_shim() {
    // There is an opening single quote before `boss-event`, but the token is
    // never closed (truncated/malformed command). Without a closing quote the
    // replacement span is undefined, so the helper leaves the command as-is.
    let cmd = "BOSS_RUN_ID='run-1' '/some/path/boss-event";
    let new_path = PathBuf::from("/stable/bin/boss-event");
    let healed = heal_hook_command(cmd, &new_path);
    assert_eq!(
        healed, cmd,
        "no closing single quote after boss-event must return the original unchanged",
    );
}

#[test]
fn heal_worker_settings_json_updates_all_hook_events() {
    // Stage a worker settings file (with a stale bazel-bin
    // boss-event path) in a settings dir, then heal the whole dir.
    let settings_dir = TempDir::new().unwrap();
    let input = WorkerSetupInput {
        run_id: "run-heal".into(),
        lease_id: "lease-heal".into(),
        workspace_path: PathBuf::from("/some/workspace/mono-agent-heal"),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/old/bazel-bin/tools/boss/event-shim/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };
    let settings_file = settings_dir.path().join("mono-agent-heal.json");
    std::fs::write(&settings_file, render_settings_json(&input)).unwrap();

    let new_path = PathBuf::from("/stable/bin/boss-event");
    heal_worker_settings_json(settings_dir.path(), &new_path);

    let settings = std::fs::read_to_string(&settings_file).unwrap();
    // All seven hook events must now reference the stable path.
    for hook in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "Notification",
        "SessionEnd",
    ] {
        assert!(
            settings.contains("/stable/bin/boss-event"),
            "{hook} hook still references stale path after heal: {settings}",
        );
    }
    assert!(
        !settings.contains("/old/bazel-bin"),
        "healed settings file must not contain the old bazel-bin path: {settings}",
    );
    // The settings file must still be valid JSON.
    let _: serde_json::Value = serde_json::from_str(&settings).unwrap();
}

#[test]
fn heal_worker_settings_json_skips_missing_settings_dir() {
    let dir = TempDir::new().unwrap();
    let new_path = PathBuf::from("/stable/boss-event");
    // Missing directory must be a no-op, not a panic.
    heal_worker_settings_json(&dir.path().join("does-not-exist"), &new_path);
    // An existing-but-empty dir is also a no-op.
    heal_worker_settings_json(dir.path(), &new_path);
}
