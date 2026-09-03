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
    for rule in [format!("Edit({boss_dir})"), format!("Edit({boss_dir}/**)")] {
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
    let critical = format!("Edit({fence}/**)");
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
        deny.iter().any(|r| *r == format!("Edit({boss_dir}/**)")),
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
