use super::super::*;

#[test]
fn remote_settings_drop_data_dir_sandbox_but_keep_hooks_and_static_denies() {
    // A remote worker's events socket is the forwarded /tmp socket and
    // its shim is resolved by name on the remote PATH.
    let input = WorkerSetupInput {
        run_id: "exec_remote_1".into(),
        lease_id: "lease-remote".into(),
        workspace_path: PathBuf::from("/Users/zak/Documents/dev/workspaces/mono-agent-003"),
        events_socket_path: PathBuf::from("/tmp/boss-events-exec_remote_1.sock"),
        boss_event_path: PathBuf::from("boss-event"),
        draft_pr_mode: false,
        execution_kind: "task_implementation".into(),
        task_kind: Some("task".into()),
        worker_kind: WorkerKind::Standard,
    };
    let parsed: serde_json::Value = serde_json::from_str(&render_remote_settings_json(&input)).unwrap();

    // All seven boss-event hook events are still wired.
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
    }

    // The boss-event command points at the FORWARDED socket + remote
    // shim resolved by name.
    let stop_cmd = hooks["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(stop_cmd.contains("/tmp/boss-events-exec_remote_1.sock"));
    assert!(stop_cmd.contains("BOSS_RUN_ID='exec_remote_1'"));
    assert!(stop_cmd.trim_end().ends_with("'boss-event'"));

    // No engine-data-dir sandbox: the deny list must NOT fence the
    // worker off the forwarded socket's parent (/tmp), and there is
    // no python path-guard hook (the script is never shipped remote).
    let deny = parsed["permissions"]["deny"].as_array().unwrap();
    assert!(
        !deny.iter().any(|r| {
            let s = r.as_str().unwrap();
            s.starts_with("Read(/tmp") || s.starts_with("Write(/tmp") || s.starts_with("Edit(/tmp")
        }),
        "remote settings must not fence the worker off /tmp: {deny:?}"
    );
    let pre = serde_json::to_string(&hooks["PreToolUse"]).unwrap();
    assert!(
        !pre.contains("boss-path-guard.py") && !pre.contains("BOSS_DATA_DIR="),
        "remote settings must not install the data-dir path-guard hook"
    );

    // The static guards survive: bossctl deny + the boss-launch guard.
    assert!(deny.iter().any(|r| r.as_str() == Some("Bash(bossctl)")));
    assert!(
        pre.contains("this would start Boss itself"),
        "boss-launch guard must remain on remote workers"
    );

    // Sanity: the LOCAL renderer DOES install the path guard, proving
    // the remote variant is the one dropping it.
    assert!(render_settings_json(&input).contains("boss-path-guard.py"));
}
