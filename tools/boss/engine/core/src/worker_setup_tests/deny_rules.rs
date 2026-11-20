use super::super::*;
use super::helpers::*;

#[test]
fn settings_json_denies_boss_state_dir_edits_and_emits_no_read_or_write_rule() {
    // The acceptance criterion for the worker-sandboxing change: a worker
    // spawned by the engine cannot touch any file under the Boss state dir.
    // The deny list must name the dir and the `**` subtree so an
    // `Edit("…/Boss")` and an `Edit("…/Boss/state.db")` both deny.
    //
    // Only `Edit` rules are emitted.
    //
    // Not `Write` — Claude Code's permission engine matches both the Edit and
    // Write *tools* against `Edit(path)` rules, so a `Write(path)` deny rule
    // matches nothing and is dead weight (previously surfaced as a startup
    // warning: "Write(...) is not matched by file permission checks — only
    // Edit(path) rules are").
    //
    // And not `Read` — Claude Code 2.1.257 refuses to auto-approve a compound
    // Bash command pairing `cd` with a relative file read whenever the
    // session carries ANY `Read()` deny rule (an existence-only predicate
    // over the deny list, so narrowing the glob or adding an allow entry
    // cannot suppress it), which stalled every `--permission-mode auto`
    // worker on a dialog no human was watching. The read side of the fence is
    // carried by the path-guard PreToolUse hook instead — see
    // `path_guard.rs`, which proves the hook blocks the relative-path-after-
    // `cd` shape the glob never covered.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
    let deny = parsed["permissions"]["deny"].as_array().expect("deny array present");
    let deny_set: Vec<&str> = deny.iter().filter_map(|v| v.as_str()).collect();
    let boss_dir = "/Users/brianduff/Library/Application Support/Boss";
    // Absolute `//` spelling: the single-slash form is project-relative and
    // fences nothing.
    for rule in [format!("Edit(/{boss_dir})"), format!("Edit(/{boss_dir}/**)")] {
        assert!(
            deny_set.iter().any(|r| *r == rule),
            "expected deny rule {rule} in {deny_set:?}",
        );
    }
    assert!(
        !deny_set.iter().any(|r| r.starts_with(&format!("Write({boss_dir}"))),
        "expected no Write(...) deny rule for the Boss state dir (inert in Claude Code's permission engine): {deny_set:?}",
    );
    assert!(
        !deny_set.iter().any(|r| r.starts_with("Read(")),
        "expected NO Read(...) deny rule at all — any one of them arms Claude Code's \
         cd-with-relative-read permission prompt and stalls the worker: {deny_set:?}",
    );
}

#[test]
fn settings_json_denies_bossctl_and_engine_lifecycle_verbs() {
    // bossctl is coordinator-only; `boss engine start|stop` reach
    // into engine process state. The rest of the `boss` surface
    // talks to the engine over its IPC socket and is fine.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
    let deny: Vec<&str> = parsed["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for rule in [
        "Bash(bossctl)",
        "Bash(bossctl:*)",
        "Bash(boss engine start)",
        "Bash(boss engine start:*)",
        "Bash(boss engine stop)",
        "Bash(boss engine stop:*)",
        r#"Bash("$BOSS_BIN" engine start)"#,
        r#"Bash("$BOSS_BIN" engine start:*)"#,
        r#"Bash("$BOSS_BIN" engine stop)"#,
        r#"Bash("$BOSS_BIN" engine stop:*)"#,
        "Bash($BOSS_BIN engine start)",
        "Bash($BOSS_BIN engine start:*)",
        "Bash($BOSS_BIN engine stop)",
        "Bash($BOSS_BIN engine stop:*)",
    ] {
        assert!(deny.contains(&rule), "expected deny rule {rule} in {deny:?}",);
    }
}

#[test]
fn reviewer_kind_adds_write_and_push_deny_rules_standard_does_not() {
    // Standard workers must not carry the reviewer deny rules — that
    // would break every implementation worker.
    let std_input = sample_input(); // worker_kind: Standard
    let std_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&std_input, &ClaudeDriver)).unwrap();
    let std_deny: Vec<&str> = std_parsed["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for rule in reviewer_deny_rules(&std_input.workspace_path) {
        assert!(
            !std_deny.contains(&rule.as_str()),
            "standard worker must NOT carry reviewer deny rule: {rule}",
        );
    }

    // Reviewer workers must carry every rule from reviewer_deny_rules().
    let mut rev_input = sample_input();
    rev_input.worker_kind = WorkerKind::Reviewer;
    let rev_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&rev_input, &ClaudeDriver)).unwrap();
    let rev_deny: Vec<&str> = rev_parsed["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for rule in reviewer_deny_rules(&rev_input.workspace_path) {
        assert!(
            rev_deny.contains(&rule.as_str()),
            "reviewer worker must carry deny rule: {rule} (got {rev_deny:?})",
        );
    }
    // Spot-check the most critical publish rules.
    for critical in [
        "Bash(jj git push:*)",
        "Bash(gh pr create:*)",
        "Bash(gh pr comment:*)",
        "Bash(cube pr:*)",
    ] {
        assert!(
            rev_deny.contains(&critical),
            "reviewer must deny {critical} (got {rev_deny:?})",
        );
    }
    // The reviewer's file-write deny is scoped to the worker-workspaces root
    // (NOT a blanket `**`) so it can still write its one out-of-tree
    // structured-output artifact, while sibling workspaces stay protected.
    let fence = rev_input
        .workspace_path
        .parent()
        .unwrap_or(&rev_input.workspace_path)
        .display();
    let critical = format!("Edit(/{fence}/**)");
    assert!(
        rev_deny.contains(&critical.as_str()),
        "reviewer must deny workspaces-root-scoped {critical} (got {rev_deny:?})",
    );
    // Write(...) rules are never emitted: Claude Code matches both the Edit
    // and Write tools against Edit(path) rules, so a parallel Write(path)
    // rule would be inert.
    assert!(
        !rev_deny.iter().any(|r| r.starts_with("Write(")),
        "reviewer must NOT carry any Write(...) rule — inert in Claude Code's permission engine (got {rev_deny:?})",
    );
    // And it must NOT carry the blanket file-write deny — that would block
    // the artifact write outside the checkout.
    assert!(
        !rev_deny.contains(&"Edit(**)"),
        "reviewer must NOT carry blanket Edit(**) (got {rev_deny:?})",
    );
}

#[test]
fn reviewer_launches_keep_absolute_write_and_publish_denies_for_every_tier() {
    use crate::driver::{AgentDriver, SpawnRequest};
    use boss_protocol::ReviewModelTier;

    let mut input = sample_input();
    input.worker_kind = WorkerKind::Reviewer;
    input.workspace_path = PathBuf::from("/worker checkouts/reviewer");
    let settings = TempDir::new().unwrap();
    let settings_path = settings.path().join("reviewer.json");
    std::fs::write(&settings_path, render_settings_json(&input, &ClaudeDriver)).unwrap();
    let policy: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    let deny = policy["permissions"]["deny"].as_array().unwrap();

    // These are the rule forms observed to reject Edit, Write, Bash redirects
    // and publishing in real auto / skip-permissions Claude sessions. Pin the
    // absolute spelling independently of the renderer: /worker checkouts/**
    // is project-relative and allowed real writes in both launch modes.
    assert!(deny.contains(&serde_json::json!("Edit(//worker checkouts/**)")));
    assert!(!deny.contains(&serde_json::json!("Edit(/worker checkouts/**)")));
    assert!(
        !deny.contains(&serde_json::json!("Edit(**)")),
        "artifact must remain writable"
    );
    for command in [
        "git push",
        "jj git push",
        "gh pr create",
        "gh pr merge",
        "gh pr close",
        "gh pr edit",
        "gh pr comment",
        "gh pr review",
        "cube pr",
    ] {
        for suffix in ["", ":*"] {
            assert!(deny.contains(&serde_json::json!(format!("Bash({command}{suffix})"))));
        }
    }

    let menu = &ClaudeDriver.descriptor().model_menu;
    const SKIP: &str = "--dangerously-skip-permissions";
    const AUTO: &str = "--permission-mode auto";
    let expected_modes = [
        ((menu.review_model_for_tier)(ReviewModelTier::Fast), SKIP),
        ((menu.review_model_for_tier)(ReviewModelTier::Balanced), SKIP),
        ((menu.review_model_for_tier)(ReviewModelTier::Strong), AUTO),
        ("fable", AUTO),
    ];
    for (model, base_mode) in expected_modes {
        for non_opus_auto_mode in [false, true] {
            let expected = if non_opus_auto_mode { AUTO } else { base_mode };
            let plan = ClaudeDriver.spawn_invocation(SpawnRequest {
                model,
                effort: None,
                settings_path: Some(&settings_path),
                non_opus_auto_mode,
                permission_mode_override: input.worker_kind.forced_permission_mode(),
                run_id: Some(&input.run_id),
            });
            assert!(
                plan.command
                    .contains(&format!("--settings '{}'", settings_path.display()))
            );
            assert!(
                plan.command.contains(expected),
                "unverified reviewer mode: {}",
                plan.command
            );
            // Fenced in every permission mode; and refused without settings
            // in both skip-permissions and auto mode.
            let gate = ClaudeDriver.launch_fence_flag();
            input
                .worker_kind
                .verify_launch_command_fenced(gate, &plan.command)
                .unwrap();
            let unfenced = ClaudeDriver.spawn_invocation(SpawnRequest {
                model,
                effort: None,
                settings_path: None,
                non_opus_auto_mode,
                permission_mode_override: input.worker_kind.forced_permission_mode(),
                run_id: Some(&input.run_id),
            });
            assert!(unfenced.command.contains(expected));
            assert!(
                input
                    .worker_kind
                    .verify_launch_command_fenced(gate, &unfenced.command)
                    .is_err(),
                "reviewer without --settings must be refused: {}",
                unfenced.command
            );
        }
    }

    // The real launch path always hands the reviewer its settings file: a
    // skip-permissions reviewer with no `--settings` would be unfenced.
    let workspace = TempDir::new().unwrap();
    let mut real = sample_input();
    real.worker_kind = WorkerKind::Reviewer;
    real.workspace_path = workspace.path().to_path_buf();
    let written = write_workspace_files(&real, &ClaudeDriver).unwrap();
    let plan = ClaudeDriver.spawn_invocation(SpawnRequest {
        model: (menu.review_model_for_tier)(ReviewModelTier::Fast),
        effort: None,
        settings_path: Some(&written.settings_path),
        non_opus_auto_mode: false,
        permission_mode_override: real.worker_kind.forced_permission_mode(),
        run_id: Some(&real.run_id),
    });
    assert!(plan.command.contains("--dangerously-skip-permissions"));
    assert!(plan.command.contains("--settings '"), "{}", plan.command);
    // The launch builders' own gate accepts the fenced command and rejects the
    // same skip-permissions command without a settings file.
    let gate = ClaudeDriver.launch_fence_flag();
    real.worker_kind
        .verify_launch_command_fenced(gate, &plan.command)
        .unwrap();
    let unfenced = ClaudeDriver.spawn_invocation(SpawnRequest {
        model: (menu.review_model_for_tier)(ReviewModelTier::Fast),
        effort: None,
        settings_path: None,
        non_opus_auto_mode: false,
        permission_mode_override: real.worker_kind.forced_permission_mode(),
        run_id: Some(&real.run_id),
    });
    assert!(
        real.worker_kind
            .verify_launch_command_fenced(gate, &unfenced.command)
            .is_err()
    );
    assert!(
        WorkerKind::Triage
            .verify_launch_command_fenced(gate, &unfenced.command)
            .is_err()
    );
    WorkerKind::Standard
        .verify_launch_command_fenced(gate, &unfenced.command)
        .unwrap();
    // The referenced settings file carries the write fence as well.
    assert!(
        written_deny_has_fence(&written.settings_path),
        "reviewer settings must carry the absolute write fence"
    );
    // Publication is fenced by a PreToolUse guard, not only by prefix rules.
    let written_settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&written.settings_path).unwrap()).unwrap();
    assert!(
        written_settings["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.to_string().contains("Boss reviewer no-write/no-publish guard")),
        "reviewer must carry the publication guard"
    );
}

#[test]
fn reviewer_publish_guard_blocks_global_option_and_absolute_path_forms() {
    use std::io::Write as _;
    let script = crate::driver::codex::codex_reviewer_publish_guard_script();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("guard.py");
    std::fs::write(&path, script).unwrap();
    let decide = |command: &str| -> String {
        let mut child = std::process::Command::new("python3")
            .arg(&path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let payload = serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}});
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        drop(child.stdin.take());
        let out: serde_json::Value = serde_json::from_slice(&child.wait_with_output().unwrap().stdout).unwrap();
        out["decision"].as_str().unwrap().to_owned()
    };
    for command in [
        "jj -R /abs/ws git push",
        "git -C /abs/ws push origin x",
        "gh --repo spinyfin/mono pr edit 3705 --title x",
        "gh -R spinyfin/mono pr comment 1 --body x",
        "/opt/homebrew/bin/gh pr merge 1",
        "/usr/bin/git push",
        "\"$CUBE_BIN\" pr update --branch b",
    ] {
        assert_eq!(decide(command), "block", "{command} must be blocked");
    }
    for command in [
        "jj -R /abs/ws log",
        "git -C /abs/ws status",
        "gh --repo spinyfin/mono pr view 1",
        "gh pr diff 1",
        "\"$BOSS_BIN\" propose review-guide --body x",
    ] {
        assert_eq!(decide(command), "approve", "{command} must be approved");
    }
}

#[test]
fn reviewer_root_workspace_fences_the_entire_filesystem() {
    assert!(reviewer_deny_rules(Path::new("/")).contains(&"Edit(//**)".to_owned()));
}

#[test]
fn reviewer_rejects_a_relative_workspace_with_a_typed_launch_error() {
    let mut input = sample_input();
    input.worker_kind = WorkerKind::Reviewer;
    input.workspace_path = PathBuf::from("relative/reviewer");
    let err = write_workspace_files(&input, &ClaudeDriver).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("reviewer workspace must be absolute"));
}

#[test]
fn reviewer_settings_json_has_same_top_level_shape_as_standard() {
    let mut rev_input = sample_input();
    rev_input.worker_kind = WorkerKind::Reviewer;
    let rev_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&rev_input, &ClaudeDriver)).unwrap();

    let std_input = sample_input();
    let std_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&std_input, &ClaudeDriver)).unwrap();

    let reviewer_keys: std::collections::BTreeSet<&str> = rev_parsed
        .as_object()
        .expect("reviewer settings must be a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    let standard_keys: std::collections::BTreeSet<&str> = std_parsed
        .as_object()
        .expect("standard settings must be a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    assert!(
        reviewer_keys == standard_keys,
        "reviewer settings must not add top-level settings: reviewer={reviewer_keys:?}, standard={standard_keys:?}",
    );
}

#[test]
fn triage_kind_adds_no_publish_deny_rules_standard_does_not() {
    // Triage workers must carry the read-only / no-publish denylist (they
    // investigate and emit a marker; they must not edit, push, or open a
    // PR). Standard implementation workers must NOT carry it.
    let std_input = sample_input(); // worker_kind: Standard
    let std_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&std_input, &ClaudeDriver)).unwrap();
    let std_deny: Vec<&str> = std_parsed["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for rule in triage_deny_rules() {
        assert!(
            !std_deny.contains(&rule.as_str()),
            "standard worker must NOT carry triage deny rule: {rule}",
        );
    }

    let mut triage_input = sample_input();
    triage_input.worker_kind = WorkerKind::Triage;
    let triage_parsed: serde_json::Value =
        serde_json::from_str(&render_settings_json(&triage_input, &ClaudeDriver)).unwrap();
    let triage_deny: Vec<&str> = triage_parsed["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for critical in [
        "Edit(**)",
        "Bash(jj git push:*)",
        "Bash(git push:*)",
        "Bash(gh pr create:*)",
        "Bash(cube pr:*)",
    ] {
        assert!(
            triage_deny.contains(&critical),
            "triage worker must deny {critical} (got {triage_deny:?})",
        );
    }
    // Write(...) rules are never emitted — see the note on the reviewer test
    // above; Edit(**) alone covers both the Edit and Write tools.
    assert!(
        !triage_deny.iter().any(|r| r.starts_with("Write(")),
        "triage worker must NOT carry any Write(...) rule — inert in Claude Code's permission engine (got {triage_deny:?})",
    );
    // `boss task create` is the triage worker's sole write action and must
    // NOT be denied (none of the no-publish rules touch it).
    assert!(
        !triage_deny.iter().any(|r| r.contains("task create")),
        "triage worker must be able to run `boss task create` (got {triage_deny:?})",
    );
}

#[test]
fn settings_json_does_not_deny_workspace_paths() {
    // Defensive: a buggy deny rule that accidentally fences off
    // `~/Documents/dev/workspaces/…` would break every worker
    // (their lease lives there). Verify no deny rule names the
    // workspace root.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
    let deny: Vec<&str> = parsed["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for rule in &deny {
        assert!(
            !rule.contains("workspaces"),
            "deny rule must not target the workspaces dir: {rule}",
        );
    }
}

// ── Data-dir fence ↔ path-guard hook tie-in ───────────────────────────────
//
// The `Edit(...)` deny globs are a literal-path belt on top of the
// deterministic path-guard `PreToolUse` hook, which is what actually enforces
// the Boss-data-dir boundary (it is tool-agnostic and canonicalises relative
// paths against the call's `cwd`). These tests pin that the belt is derived
// from the hook actually being wired into the same settings file, rather than
// the two agreeing by coincidence because both read `EngineDataDirSandbox`.

#[test]
fn settings_json_carries_the_data_dir_globs_and_the_path_guard_hook_together() {
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();

    let boss_dir = "/Users/brianduff/Library/Application Support/Boss";
    let deny: Vec<&str> = parsed["permissions"]["deny"]
        .as_array()
        .expect("deny array present")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        deny.iter().any(|r| *r == format!("Edit(/{boss_dir}/**)")),
        "expected the data-dir Edit glob: {deny:?}",
    );

    let guarded = parsed["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse array present")
        .iter()
        .any(hook_entry_runs_path_guard);
    assert!(
        guarded,
        "the deny globs describe a boundary the path-guard hook enforces; the hook must be in \
         the same settings file: {}",
        parsed["hooks"]["PreToolUse"],
    );
}

#[test]
fn data_dir_fence_is_armed_only_when_the_path_guard_hook_is_wired_in() {
    assert_eq!(
        data_dir_fence(EngineDataDirSandbox::Enabled, true, true),
        DataDirFence::PathGuardHook,
    );
    // Remote worker: no engine data dir on that host to fence.
    assert_eq!(
        data_dir_fence(EngineDataDirSandbox::Disabled, true, true),
        DataDirFence::NotThroughThisFile,
    );
    // A driver whose guards do not live in this settings file (a DriverOwned
    // hook wiring, or a byte-stream ingress) never receives it — only
    // Claude's spawn plan passes `--settings` — so the deny list must not
    // claim a fence this file cannot carry.
    assert_eq!(
        data_dir_fence(EngineDataDirSandbox::Enabled, false, false),
        DataDirFence::NotThroughThisFile,
    );
}

#[test]
#[should_panic(expected = "no boss-path-guard.py PreToolUse hook was wired into it")]
fn data_dir_fence_fails_loudly_when_the_sandbox_is_on_but_the_hook_is_missing() {
    // A spawn path that keeps the sandbox flag but drops the hook leaves the
    // boundary unenforced. That must fail loudly, not quietly render a
    // settings file whose deny list implies a fence that is not there.
    let _ = data_dir_fence(EngineDataDirSandbox::Enabled, true, false);
}

#[test]
fn deny_rules_omit_the_data_dir_globs_when_the_fence_is_not_carried_here() {
    let input = sample_input();
    let rules = deny_rules(&input, DataDirFence::NotThroughThisFile);
    assert!(
        !rules.iter().any(|r| r.contains("Application Support/Boss")),
        "an unfenced settings file must not carry data-dir globs: {rules:?}",
    );
    // The static coordinator-surface guards are unconditional and survive.
    assert!(rules.iter().any(|r| r == "Bash(bossctl)"), "{rules:?}");
}

#[test]
fn hook_entry_runs_path_guard_matches_only_the_gate_script_entry() {
    assert!(hook_entry_runs_path_guard(&serde_json::json!({
        "matcher": "*",
        "hooks": [{"type": "command", "command": "BOSS_DATA_DIR='/d' python3 '/t/boss-path-guard.py'"}],
    })));
    assert!(!hook_entry_runs_path_guard(&serde_json::json!({
        "matcher": "Bash",
        "hooks": [{"type": "command", "command": "python3 '/t/boss-checkleft-push-guard.py'"}],
    })));
    assert!(!hook_entry_runs_path_guard(&serde_json::json!({"matcher": "Bash"})));
}

#[test]
fn review_guide_extends_the_read_only_deny_body_with_shell_option_denies() {
    let mut expected = answer_agent_deny_rules();
    expected.extend(crate::driver::codex::review_guide_deny_rules());
    assert_eq!(review_guide_deny_rules(), expected);
}

#[test]
fn claude_review_guide_settings_allow_only_submission_and_wire_the_exact_command_guard() {
    let mut input = sample_input();
    input.worker_kind = WorkerKind::ReviewGuide;
    let settings: serde_json::Value = serde_json::from_str(&render_settings_json(&input, &ClaudeDriver)).unwrap();
    assert_eq!(settings["permissions"]["defaultMode"], "dontAsk");
    assert_eq!(
        settings["permissions"]["allow"],
        serde_json::json!(review_guide_allow_rules())
    );
    let hook = settings["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| {
            entry["hooks"][0]["command"].as_str().is_some_and(|command| {
                command.contains("REVIEW_GUIDE_COMMAND") || command.contains("def allowed(payload)")
            })
        })
        .expect("guide guard must be wired into Claude hooks");
    assert_eq!(hook["matcher"], ".*");
    let command = hook["hooks"][0]["command"].as_str().unwrap();
    for (tool, shell, expected) in [
        ("Bash", "\"$BOSS_BIN\" propose review-guide --body '# Guide'", "approve"),
        (
            "Bash",
            "\"$BOSS_BIN\" propose review-guide --body '# Guide\nswift run\nboss engine start\n'",
            "approve",
        ),
        ("Bash", "cat README.md", "block"),
        ("Read", "", "block"),
        (
            "Bash",
            "\"$BOSS_BIN\" propose review-guide --body 'x'; touch /tmp/x",
            "block",
        ),
    ] {
        use std::io::Write;
        let mut child = std::process::Command::new("sh")
            .args(["-c", command])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                serde_json::json!({"tool_name": tool, "tool_input": {"command": shell}})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(result.status.success());
        let decision: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(decision["decision"], expected, "{shell}");
    }
}

/// A review-guide worker's live PreToolUse chain (path + launch +
/// review-guide) must treat a proven `--body` literal as data, including
/// Markdown that quotes launch commands and the Boss data directory, while
/// still rejecting chaining, redirects, and substitutions.
#[test]
fn review_guide_hook_chain_masks_literal_body_and_still_rejects_wrappers() {
    use crate::driver::{AgentDriver, ToolUseInterceptionConfig};
    use std::io::Write as _;

    let data = TempDir::new().unwrap();
    let data_dir = data.path();
    let script_dir = TempDir::new().unwrap();
    let script = script_dir.path().join(PATH_GUARD_SCRIPT_NAME);
    std::fs::write(&script, PATH_GUARD_SCRIPT).unwrap();
    let wiring = ClaudeDriver.tool_use_interception_wiring(&ToolUseInterceptionConfig {
        data_dir: Some(data_dir.to_path_buf()),
        path_guard_script: Some(script),
        checkleft_guard_script: None,
        git_store_guard_script: None,
        is_revision: false,
        is_standard_worker: false,
        is_reviewer: false,
        is_review_guide: true,
        run_id: None,
        workspace_path: None,
    });
    let commands: Vec<String> = wiring
        .pre_tool_use_hooks
        .iter()
        .filter_map(|entry| entry["hooks"][0]["command"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        commands.len(),
        3,
        "local review-guide worker must get path + launch + review-guide: {commands:?}"
    );

    let body_command = format!(
        "\"$BOSS_BIN\" propose review-guide --body '# Guide\n\
         ## Problem\n\
         Workers sometimes quote `swift run` and `boss engine start`.\n\
         Isolated engines use `bazel run //tools/boss/engine/core:engine` without a socket path in the prose.\n\
         The data directory is {}.\n\
         ## Implementation\n\
         ## Example\n\
         ## Review\n'",
        data_dir.display()
    );
    let run = |hook: &str, command: &str, boss_bin: Option<&str>, path: Option<&str>| {
        let payload = serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}, "cwd": "/"});
        let mut process = std::process::Command::new("sh");
        process.args(["-c", hook]).env_remove("BOSS_BIN");
        if let Some(bin) = boss_bin {
            process.env("BOSS_BIN", bin);
        }
        if let Some(path) = path {
            process.env("PATH", path);
        }
        let mut child = process
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("sh must be available");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    for hook in &commands {
        let decision = run(hook, &body_command, Some("/usr/bin/true"), None);
        assert_eq!(
            decision["decision"], "approve",
            "literal guide body must pass {hook}: {decision}"
        );
    }
    for (label, command) in [
        (
            "chaining",
            r#""$BOSS_BIN" propose review-guide --body 'x' && boss engine start"#,
        ),
        (
            "redirect",
            r#""$BOSS_BIN" propose review-guide --body 'x' > /tmp/guide.md"#,
        ),
        (
            "substitution",
            r#""$BOSS_BIN" propose review-guide --body "$(cat secret)""#,
        ),
    ] {
        let decisions: Vec<_> = commands
            .iter()
            .map(|hook| run(hook, command, Some("/usr/bin/true"), None))
            .collect();
        assert!(
            decisions.iter().any(|d| d["decision"] == "block"),
            "{label} must be rejected by the review-guide chain: {decisions:?} for {command}"
        );
    }

    // A valid literal body does not establish which executable the shell runs.
    let repobin = script_dir.path().join("repobin");
    std::fs::write(&repobin, b"#!/bin/sh\n").unwrap();
    let boss = script_dir.path().join("boss");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&repobin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&repobin, &boss).unwrap();
    }
    let path = format!("{}:{}", script_dir.path().display(), std::env::var("PATH").unwrap());
    for (command, bin, expected_reason) in [
        (r#""$BOSS_BIN" propose review-guide --body 'guide'"#, None, "unset"),
        (
            r#""$BOSS_BIN" propose review-guide --body 'guide'"#,
            boss.to_str(),
            "repobin",
        ),
        ("boss propose review-guide --body 'guide'", None, "repobin"),
    ] {
        let decision = run(BOSS_LAUNCH_GUARD_COMMAND, command, bin, Some(&path));
        assert_eq!(decision["decision"], "block", "{decision}");
        assert!(
            decision["reason"].as_str().unwrap().contains(expected_reason),
            "{decision}"
        );
    }
}

fn written_deny_has_fence(path: &Path) -> bool {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    v["permissions"]["deny"].as_array().unwrap().iter().any(|r| {
        r.as_str()
            .is_some_and(|r| r.starts_with("Edit(//") && r.ends_with("/**)"))
    })
}
