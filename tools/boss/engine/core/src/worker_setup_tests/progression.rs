use super::super::*;
use super::helpers::*;

// ── Progression-agent capability-restricted dispatch ───────────────────────

fn progression_input() -> WorkerSetupInput {
    WorkerSetupInput {
        execution_kind: "progression".into(),
        task_kind: None,
        worker_kind: WorkerKind::Progression,
        ..sample_input()
    }
}

fn rendered_permissions() -> serde_json::Value {
    let parsed: serde_json::Value =
        serde_json::from_str(&render_settings_json(&progression_input(), &ClaudeDriver)).unwrap();
    parsed["permissions"].clone()
}

fn strings(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn progression_execution_kind_maps_to_the_progression_worker_kind() {
    use boss_protocol::ExecutionKind;
    assert_eq!(
        worker_kind_for_execution(&ExecutionKind::Progression),
        WorkerKind::Progression,
        "a progression execution gets its own worker kind"
    );
    assert_eq!(WorkerKind::Progression.label(), "progression");
    assert_eq!(WorkerKind::Progression.forced_permission_mode(), Some("dontAsk"));
}

#[test]
fn progression_settings_use_the_dontask_allowlist_mode() {
    assert_eq!(rendered_permissions()["defaultMode"], serde_json::json!("dontAsk"));
}

#[test]
fn progression_allow_list_is_exactly_the_reduced_table() {
    // A change here is a capability change and must be a visible diff.
    let expected: Vec<String> = [
        "Read",
        "Grep",
        "Glob",
        "Bash(boss propose:*)",
        "Bash(boss task show:*)",
        "Bash(boss task list:*)",
        "Bash(boss pr status:*)",
        r#"Bash("$BOSS_BIN" propose:*)"#,
        r#"Bash("$BOSS_BIN" task show:*)"#,
        r#"Bash("$BOSS_BIN" task list:*)"#,
        r#"Bash("$BOSS_BIN" pr status:*)"#,
        "Bash(gh pr view:*)",
        "Bash(gh pr checks:*)",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(progression_allow_rules(), expected);
    assert_eq!(strings(&rendered_permissions()["allow"]), expected);
    assert!(!expected.contains(&"Bash".to_owned()), "bare Bash is never allowlisted");
}

#[test]
fn progression_deny_belt_is_exactly_this_set() {
    let mut expected: Vec<String> = ["Edit(**)", "NotebookEdit(**)"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    // Publish verbs (push, gh pr/issue writes, cube pr) shared with the other
    // read-only kinds.
    for rule in [
        "Bash(jj git push)",
        "Bash(jj git push:*)",
        "Bash(git push)",
        "Bash(git push:*)",
        "Bash(gh pr create)",
        "Bash(gh pr create:*)",
        "Bash(gh pr merge)",
        "Bash(gh pr merge:*)",
        "Bash(gh pr close)",
        "Bash(gh pr close:*)",
        "Bash(gh pr edit)",
        "Bash(gh pr edit:*)",
        "Bash(gh pr comment)",
        "Bash(gh pr comment:*)",
        "Bash(gh pr review)",
        "Bash(gh pr review:*)",
        "Bash(gh issue create)",
        "Bash(gh issue create:*)",
        "Bash(gh issue comment)",
        "Bash(gh issue comment:*)",
        "Bash(gh issue close)",
        "Bash(gh issue close:*)",
        "Bash(gh issue edit)",
        "Bash(gh issue edit:*)",
        "Bash(cube pr)",
        "Bash(cube pr:*)",
        r#"Bash("$CUBE_BIN" pr)"#,
        r#"Bash("$CUBE_BIN" pr:*)"#,
        "Bash($CUBE_BIN pr)",
        "Bash($CUBE_BIN pr:*)",
        // All of cube.
        "Bash(cube)",
        "Bash(cube:*)",
        r#"Bash("$CUBE_BIN")"#,
        r#"Bash("$CUBE_BIN":*)"#,
        "Bash($CUBE_BIN)",
        "Bash($CUBE_BIN:*)",
        // All of jj and git: the agent holds no checkout.
        "Bash(jj)",
        "Bash(jj:*)",
        "Bash(git)",
        "Bash(git:*)",
        // The remaining mutating gh verbs.
        "Bash(gh pr ready:*)",
        "Bash(gh pr reopen:*)",
        "Bash(gh pr lock:*)",
        "Bash(gh pr unlock:*)",
        "Bash(gh issue reopen:*)",
        "Bash(gh issue delete:*)",
        "Bash(gh issue transfer:*)",
        "Bash(gh issue pin:*)",
        "Bash(gh issue unpin:*)",
        "Bash(gh issue lock:*)",
        "Bash(gh issue unlock:*)",
        // All API calls, including flag-first mutations.
        "Bash(gh api)",
        "Bash(gh api:*)",
    ] {
        expected.push(rule.to_owned());
    }
    let mut got = progression_deny_rules();
    got.sort();
    expected.sort();
    assert_eq!(got, expected);
}

#[test]
fn progression_deny_belt_survives_into_the_rendered_settings_with_the_worker_denylist() {
    let deny = strings(&rendered_permissions()["deny"]);
    for rule in progression_deny_rules() {
        assert!(deny.contains(&rule), "deny belt missing {rule:?}");
    }
    // The ordinary worker denylist still applies on top.
    assert!(deny.contains(&"Bash(boss engine stop)".to_owned()));
    assert!(
        !deny.iter().any(|r| r.starts_with("Write(")),
        "Write is matched through Edit rules; a Write(...) rule would be inert"
    );
}

#[test]
fn progression_agent_gets_its_own_rules_not_the_pr_deliverable_ones() {
    let md = claude_md_for(&progression_input());
    assert!(md.contains("progression-agent"));
    assert!(md.contains("Read-only mandate"));
    assert!(md.contains(r#""$BOSS_BIN" propose done"#));
    assert!(!md.contains("Pull requests are the deliverable"));
    assert!(!md.contains("pr create --branch"));
    assert!(!md.contains("Cube lease id"));
}
