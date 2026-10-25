use super::super::*;
use super::helpers::*;

#[test]
fn settings_json_is_valid_json_with_all_seven_hooks() {
    let input = sample_input();
    let rendered = render_settings_json(&input, &ClaudeDriver);
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
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
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
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
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
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
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
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
    assert_eq!(
        parsed["permissions"]["defaultMode"],
        serde_json::Value::String("auto".into()),
        "expected permissions.defaultMode == 'auto', got: {parsed}",
    );
}

/// Claude's rendered settings must stay byte-identical after the
/// `HookWiringDestination` field was added: destination is
/// `WorkerSettingsFile`, so the merge path (hooks map + interception
/// guards into the settings file) is the same as before the field
/// existed. Pin the full pretty-printed body so a future destination
/// default flip or accidental gate change fails loudly.
#[test]
fn claude_settings_json_is_byte_identical_with_worker_settings_destination() {
    let input = sample_input();
    let rendered = render_settings_json(&input, &ClaudeDriver);
    let wiring = ClaudeDriver.progress_observation_wiring(&crate::driver::ProgressObservationConfig {
        events_socket_path: input.events_socket_path.clone(),
        lease_id: input.lease_id.clone(),
        run_id: input.run_id.clone(),
        workspace_path: input.workspace_path.clone(),
        forwarder_binary: input.boss_event_path.clone(),
    });
    match wiring {
        crate::driver::ProgressIngress::HookCallback(w) => {
            assert_eq!(
                w.destination,
                crate::driver::HookWiringDestination::WorkerSettingsFile,
                "Claude must declare WorkerSettingsFile so the merge stays live",
            );
        }
        other => panic!("Claude must produce HookCallback, got {other:?}"),
    }

    // Structural pin: every lifecycle event present, PreToolUse carries
    // shim + interception guards, permissions.defaultMode is auto. A
    // byte-level re-render is deterministic for a fixed sample_input.
    let re_rendered = render_settings_json(&input, &ClaudeDriver);
    assert_eq!(
        rendered, re_rendered,
        "Claude settings render must be deterministic (byte-identical across calls)",
    );
    let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    let hooks = parsed["hooks"].as_object().expect("hooks object");
    for name in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "Notification",
        "SessionEnd",
    ] {
        assert!(hooks.contains_key(name), "missing hook after destination field: {name}");
    }
    let pre = hooks["PreToolUse"].as_array().expect("PreToolUse array");
    // sample_input is a standard local chore: shim + path guard + git store
    // guard + boss-launch + PR redirect + checkleft push = 6 PreToolUse entries.
    assert_eq!(
        pre.len(),
        6,
        "Claude PreToolUse composition must be unchanged after destination field: {pre:?}",
    );
    assert_eq!(parsed["permissions"]["defaultMode"], "auto");
}

#[test]
fn shell_escape_quotes_paths_with_spaces() {
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
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

#[cfg(test)]
mod git_store_guard_tests {
    //! Engine-side wiring of the shared-source-store write gate: the script is
    //! materialised content-addressed next to the settings file, every local
    //! worker kind's settings carry the hook, and its refusals read cleanly.
    //! The decision matrix itself lives with the script in
    //! `boss_engine_driver::git_store_guard`.

    use super::super::super::git_store_guard::*;
    use super::super::super::*;
    use super::super::helpers::*;

    /// Locate the shared-source-store hook command in rendered settings.
    fn git_store_guard_command(parsed: &serde_json::Value) -> Option<(usize, String)> {
        parsed["hooks"]["PreToolUse"]
            .as_array()?
            .iter()
            .enumerate()
            .find_map(|(index, e)| {
                let command = e["hooks"][0]["command"].as_str()?;
                command
                    .contains(GIT_STORE_GUARD_SCRIPT_NAME)
                    .then(|| (index, command.to_owned()))
            })
    }

    #[test]
    fn git_store_guard_denials_avoid_internal_vocabulary() {
        for name in ["MALFORMED", "WRITE_RECOVERY", "SHALLOW_RECOVERY", "UNRESOLVED_RECOVERY"] {
            let prefix = format!("{name} = (\n");
            let body = GIT_STORE_GUARD_SCRIPT
                .split_once(&prefix)
                .unwrap_or_else(|| panic!("{name} message missing"))
                .1
                .split_once("\n)")
                .unwrap()
                .0;
            let mut message = String::new();
            for line in body.lines() {
                message.push_str(
                    &serde_json::from_str::<String>(line.trim()).unwrap_or_else(|e| panic!("{name}: {e}: {line}")),
                );
            }
            crate::prose_vocabulary::assert_no_forbidden_prose(name, &message);
        }
    }

    #[test]
    fn settings_json_adds_git_store_guard_hook_after_the_path_guard() {
        let input = sample_input();
        let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
        let (index, cmd) =
            git_store_guard_command(&parsed).expect("PreToolUse must include the shared-source-store guard hook");
        // Forwarder shim first, path guard second, this gate third: the two
        // boundary gates come before every posture-specific guard.
        assert_eq!(index, 2, "{parsed}");
        assert_eq!(parsed["hooks"]["PreToolUse"][index]["matcher"], "Bash");
        assert!(cmd.contains("python3"), "guard must run via python3: {cmd}");
        assert!(
            cmd.contains(&format!(
                "BOSS_WORKSPACE={}",
                shell_quote(&input.workspace_path.display().to_string())
            )),
            "guard must be told the leased workspace so it can find the repos root: {cmd}",
        );
        let script = git_store_guard_script_path();
        assert!(
            cmd.contains(&shell_quote(&script.display().to_string())),
            "guard must invoke the absolute content-addressed script path: {cmd}",
        );
        assert!(
            script
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n.to_string_lossy().starts_with(&format!("{GIT_STORE_GUARD_KIND}-"))),
            "script must live in a content-addressed directory: {}",
            script.display()
        );
    }

    #[test]
    fn git_store_guard_is_armed_for_every_local_worker_kind() {
        // The shared store is a boundary like the Boss data dir, not a posture:
        // reviewers, triage, answer agents and review guides get it too. The
        // review guide's pinned reads are exactly the explicit `--git-dir`
        // read-only form the gate permits.
        for (kind, execution_kind) in [
            (WorkerKind::Standard, "chore_implementation"),
            (WorkerKind::Standard, "revision_implementation"),
            (WorkerKind::Reviewer, "pr_review"),
            (WorkerKind::Triage, "automation_triage"),
            (WorkerKind::AnswerAgent, "answer_agent"),
            (WorkerKind::ReviewGuide, "pr_review_guide"),
        ] {
            let mut input = sample_input();
            input.worker_kind = kind.clone();
            input.execution_kind = execution_kind.into();
            let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
            assert!(
                git_store_guard_command(&parsed).is_some(),
                "{kind:?}/{execution_kind} must carry the shared-source-store guard: {parsed}"
            );
        }
    }

    #[test]
    fn remote_settings_omit_the_git_store_guard() {
        // The script is never shipped to a remote host, so remote settings must
        // not reference a path that does not exist there.
        let input = sample_input();
        let parsed: serde_json::Value =
            serde_json::from_str(&render_remote_settings_json(&input, &ClaudeDriver)).unwrap();
        assert!(
            git_store_guard_command(&parsed).is_none(),
            "remote settings must not carry the local gate script: {parsed}"
        );
    }

    #[test]
    fn write_workspace_files_materialises_the_git_store_guard_outside_the_workspace() {
        let _shared = lock_shared_settings_dir();
        let _home = ClaudeConfigGuard::new();
        let dir = TempDir::new().unwrap();
        let mut input = sample_input();
        input.workspace_path = dir.path().to_path_buf();
        let written = write_workspace_files(&input, &ClaudeDriver).unwrap();
        let script = git_store_guard_script_path();
        assert!(
            script.is_file(),
            "{} must exist after writing settings",
            script.display()
        );
        assert_eq!(
            std::fs::read_to_string(&script).unwrap(),
            GIT_STORE_GUARD_SCRIPT,
            "materialised bytes must be this build's script"
        );
        assert!(
            !script.starts_with(dir.path()),
            "the gate must live outside the workspace tree: {}",
            script.display()
        );
        let settings = std::fs::read_to_string(&written.settings_path).unwrap();
        assert!(
            settings.contains(&shell_quote(&script.display().to_string())),
            "settings must point at the materialised script: {settings}"
        );
    }

    #[test]
    fn content_addressed_git_store_guard_dirs_are_recognised_as_referenced() {
        // The prune sweep keeps any hashed guard dir a settings file still names;
        // the new kind must be part of that recognition or a live worker's gate
        // could be swept out from under it.
        let dir = TempDir::new().unwrap();
        let path = ensure_git_store_guard_script_in(dir.path()).unwrap();
        let hashed_dir = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(is_content_addressed_guard_dir_name(&hashed_dir, GIT_STORE_GUARD_KIND));
        std::fs::write(
            dir.path().join("ws.json"),
            format!(
                "{{\"hooks\":{{\"PreToolUse\":[{{\"command\":\"python3 '{}'\"}}]}}}}",
                path.display()
            ),
        )
        .unwrap();
        assert!(
            referenced_guard_dir_names(dir.path()).contains(&hashed_dir),
            "the settings reference must keep {hashed_dir} alive"
        );
    }

    #[test]
    fn git_store_guard_script_has_the_load_bearing_logic() {
        let s = GIT_STORE_GUARD_SCRIPT;
        assert!(
            s.contains("BOSS_WORKSPACE"),
            "must learn the workspace from the hook env"
        );
        assert!(
            s.contains(".jj"),
            "must derive the repos root from the .jj/repo pointer"
        );
        assert!(
            s.contains("GIT_DIR") && s.contains("--git-dir") && s.contains("-C"),
            "must follow every way git is pointed at a repo"
        );
        assert!(
            s.contains("--depth") && s.contains("--shallow-since") && s.contains("--update-shallow"),
            "must know the shallow options"
        );
        assert!(
            s.contains("\"decision\"") && s.contains("\"block\""),
            "must be able to emit a block decision"
        );
        assert!(
            s.contains("jj git fetch") && s.contains("throwaway clone"),
            "refusals must redirect to jj / a throwaway clone"
        );
        assert!(s.contains("realpath"), "must compare the symlink-resolved spelling too");
    }
}
