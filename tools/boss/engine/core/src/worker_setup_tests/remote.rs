use super::super::*;
use super::helpers::*;

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
        execution_kind: "task_implementation".into(),
        task_kind: Some("task".into()),
        ..sample_input()
    };
    let parsed: serde_json::Value = serde_json::from_str(&render_remote_settings_json(&input, &ClaudeDriver)).unwrap();

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
    assert!(render_settings_json(&input, &ClaudeDriver).contains("boss-path-guard.py"));
}

#[test]
fn spoke_replaces_hub_forwarders_without_removing_policy() {
    use boss_engine_driver::{ProgressIngress, ProgressObservationConfig, hook_settings};
    let input = sample_input();
    let mut settings: serde_json::Value =
        serde_json::from_str(&render_remote_settings_json(&input, &ClaudeDriver)).unwrap();
    let policy = settings["permissions"].clone();
    let config = ProgressObservationConfig {
        events_socket_path: "/tmp/spoke.sock".into(),
        forwarder_binary: "/spoke/bin/boss-event".into(),
        run_id: "spoke-run".into(),
        lease_id: "lease".into(),
        workspace_path: "/workspace".into(),
    };
    for _ in 0..2 {
        let ProgressIngress::HookCallback(wiring) = ClaudeDriver.progress_observation_wiring(&config) else {
            panic!("expected hooks");
        };
        hook_settings::replace_forwarders(&mut settings, wiring.hooks, &config.forwarder_binary).unwrap();
    }
    assert_eq!(settings["permissions"], policy);
    for entries in settings["hooks"].as_object().unwrap().values() {
        let forwarders: Vec<_> = entries
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|entry| entry["hooks"].as_array().unwrap())
            .filter_map(|hook| hook["command"].as_str())
            .filter(|command| command.contains("boss-event"))
            .collect();
        assert_eq!(forwarders.len(), 1);
        assert!(forwarders[0].contains("spoke-run"));
        assert!(forwarders[0].contains("/tmp/spoke.sock"));
    }
    assert!(
        settings["hooks"]["PreToolUse"]
            .to_string()
            .contains("this would start Boss itself")
    );
    let first = settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        first.contains("/spoke/bin/boss-event") && first.contains("spoke-run"),
        "spoke forwarder must stay ahead of the policy guards: {first}"
    );
}
