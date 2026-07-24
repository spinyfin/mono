use super::super::*;
use super::helpers::*;

/// A leaked settings file holding a `boss-event` Stop hook with a
/// stale `BOSS_RUN_ID` (as written by a pre-fix engine build into a
/// reused workspace). Mirrors the real on-disk shape.
fn leaked_settings_json(run_id: &str) -> String {
    serde_json::json!({
        "permissions": { "defaultMode": "auto", "deny": ["Bash(bossctl)"] },
        "hooks": {
            "SessionStart": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": format!("BOSS_LEASE_ID='l' BOSS_RUN_ID='{run_id}' /Applications/Boss.app/Contents/Resources/bin/boss-event"),
                }],
            }],
            "Stop": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": format!("BOSS_LEASE_ID='l' BOSS_RUN_ID='{run_id}' /Applications/Boss.app/Contents/Resources/bin/boss-event"),
                }],
            }],
        },
    })
    .to_string()
}

#[test]
fn purge_leaked_worker_hooks_strips_stale_boss_hooks_but_keeps_other_content() {
    let dir = TempDir::new().unwrap();
    let claude_dir = dir.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    let settings = claude_dir.join("settings.json");
    std::fs::write(&settings, leaked_settings_json("exec_stale_99")).unwrap();

    purge_leaked_worker_hooks(dir.path());

    // The leaked Stop hook (and every other boss hook) is gone, so
    // a worker session can no longer fire a second Stop with the
    // stale run id. Non-hook content (the repo-style deny rules)
    // survives.
    let after: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert!(
        !std::fs::read_to_string(&settings).unwrap().contains("BOSS_RUN_ID"),
        "no leaked BOSS_RUN_ID hook may remain",
    );
    assert!(
        after.get("hooks").is_none(),
        "all hooks were boss hooks, so the now-empty hooks key is dropped",
    );
    assert_eq!(
        after["permissions"]["deny"][0], "Bash(bossctl)",
        "non-hook content must be preserved",
    );
}

#[test]
fn purge_leaked_worker_hooks_removes_pure_engine_file() {
    let dir = TempDir::new().unwrap();
    let claude_dir = dir.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    // A settings file that is *only* leaked boss hooks (no other
    // keys) is removed entirely, restoring the no-settings-in-tree
    // invariant.
    let local = claude_dir.join("settings.local.json");
    let only_hooks = serde_json::json!({
        "hooks": {
            "Stop": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": "BOSS_RUN_ID='exec_old' /bin/boss-event",
                }],
            }],
        },
    });
    std::fs::write(&local, only_hooks.to_string()).unwrap();

    purge_leaked_worker_hooks(dir.path());

    assert!(
        !local.exists(),
        "a settings file with only leaked boss hooks must be removed",
    );
}

#[test]
fn purge_leaked_worker_hooks_leaves_clean_repo_settings_untouched() {
    let dir = TempDir::new().unwrap();
    let claude_dir = dir.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    // A legitimately repo-tracked settings.json (no boss hooks) must
    // survive byte-for-byte: the cheap signature pre-check means we
    // never even parse it.
    let settings = claude_dir.join("settings.json");
    let clean = "{\n  \"hooks\": {\n    \"Stop\": [ { \"matcher\": \"*\", \"hooks\": [ { \"type\": \"command\", \"command\": \"echo hi\" } ] } ]\n  }\n}\n";
    std::fs::write(&settings, clean).unwrap();

    purge_leaked_worker_hooks(dir.path());

    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        clean,
        "a clean repo settings.json with no BOSS_RUN_ID hook must be untouched byte-for-byte",
    );
}

#[test]
fn purge_leaked_worker_hooks_is_noop_when_absent() {
    let dir = TempDir::new().unwrap();
    // No .claude/ dir at all — must not panic or create anything.
    purge_leaked_worker_hooks(dir.path());
    assert!(!dir.path().join(".claude").join("settings.json").exists());
}

/// Build a single hook group `{matcher, hooks: [{type, command}]}` whose inner
/// command carries (or not) the leaked engine signature.
fn hook_group(command: &str) -> serde_json::Value {
    serde_json::json!({
        "matcher": "*",
        "hooks": [{ "type": "command", "command": command }],
    })
}

#[test]
fn hook_group_is_leaked_detects_signature_in_inner_command() {
    // A group whose inner command carries the BOSS_RUN_ID= signature is
    // engine-injected (leaked); a group with an ordinary command is not.
    assert!(
        hook_group_is_leaked(&hook_group("BOSS_RUN_ID='exec_x' /bin/boss-event")),
        "a group with the BOSS_RUN_ID signature is leaked",
    );
    assert!(
        !hook_group_is_leaked(&hook_group("echo hi")),
        "a group with no signature is not leaked",
    );
    // A group missing the inner `hooks` array entirely is trivially not leaked.
    assert!(
        !hook_group_is_leaked(&serde_json::json!({ "matcher": "*" })),
        "a malformed group with no inner hooks array is not leaked",
    );
}

#[test]
fn strip_leaked_hooks_removes_leaked_group_and_reports_change() {
    // A settings value whose only hook group is engine-injected: the group is
    // removed, the now-empty event key is dropped, the now-empty `hooks` key is
    // dropped, and the function reports that it changed the value.
    let mut value = serde_json::json!({
        "permissions": { "deny": ["Bash(bossctl)"] },
        "hooks": {
            "Stop": [ hook_group("BOSS_RUN_ID='exec_stale' /bin/boss-event") ],
        },
    });
    let changed = strip_leaked_hooks(&mut value);
    assert!(changed, "stripping a leaked group must report a change");
    assert!(
        value.get("hooks").is_none(),
        "the sole event's group was leaked, so the empty hooks key is dropped: {value}",
    );
    // Non-hook content is preserved untouched.
    assert_eq!(value["permissions"]["deny"][0], "Bash(bossctl)");
}

#[test]
fn strip_leaked_hooks_leaves_clean_value_unchanged() {
    // A settings value with only legitimate (non-signature) hooks is left
    // structurally intact and the function reports no change.
    let original = serde_json::json!({
        "hooks": {
            "Stop": [ hook_group("echo hi") ],
        },
    });
    let mut value = original.clone();
    let changed = strip_leaked_hooks(&mut value);
    assert!(!changed, "a clean value must report no change");
    assert_eq!(value, original, "a clean value must be left unchanged");
}

#[test]
fn strip_leaked_hooks_removes_only_the_leaked_group_from_mixed_value() {
    // A mixed event array holding one leaked group and one legitimate group:
    // only the leaked group is removed; the clean group (and its event key)
    // survives, and the change is reported.
    let mut value = serde_json::json!({
        "hooks": {
            "Stop": [
                hook_group("BOSS_RUN_ID='exec_stale' /bin/boss-event"),
                hook_group("echo keep-me"),
            ],
            // A second, entirely-clean event must be preserved intact.
            "SessionEnd": [ hook_group("echo session-end") ],
        },
    });
    let changed = strip_leaked_hooks(&mut value);
    assert!(changed, "removing the leaked group must report a change");

    let stop = value["hooks"]["Stop"].as_array().expect("Stop event survives");
    assert_eq!(stop.len(), 1, "only the leaked group is removed: {value}");
    assert_eq!(
        stop[0]["hooks"][0]["command"], "echo keep-me",
        "the legitimate Stop group must survive",
    );
    assert_eq!(
        value["hooks"]["SessionEnd"][0]["hooks"][0]["command"], "echo session-end",
        "an unrelated clean event must be preserved intact",
    );
}

#[test]
fn strip_leaked_hooks_returns_false_when_no_hooks_key() {
    // A value with no `hooks` object is a no-op that reports no change.
    let mut value = serde_json::json!({ "permissions": { "deny": [] } });
    assert!(
        !strip_leaked_hooks(&mut value),
        "a value with no hooks key must report no change",
    );
}

#[test]
fn write_workspace_files_purges_leaked_in_tree_settings() {
    let _shared = lock_shared_settings_dir();
    let _home = HomeGuard::new();
    let dir = TempDir::new().unwrap();
    let claude_dir = dir.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    // Simulate a warm-cached workspace carrying a stale settings.json
    // from a prior execution.
    std::fs::write(claude_dir.join("settings.json"), leaked_settings_json("exec_prev_run")).unwrap();

    let input = WorkerSetupInput {
        run_id: "exec_current".into(),
        lease_id: "test-lease".into(),
        workspace_path: dir.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };

    write_workspace_files(&input, &ClaudeDriver).unwrap();

    let settings = claude_dir.join("settings.json");
    // The leaked prior run's hook must be gone after setup; only
    // the engine's out-of-tree `--settings` file carries hooks now.
    if settings.exists() {
        assert!(
            !std::fs::read_to_string(&settings).unwrap().contains("BOSS_RUN_ID"),
            "write_workspace_files must purge the stale in-tree boss hook",
        );
    }
}
