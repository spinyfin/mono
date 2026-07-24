use super::super::*;
use super::helpers::*;

#[test]
fn settings_json_denies_boss_state_dir_reads_writes_and_edits() {
    // The acceptance criterion for the worker-sandboxing change:
    // a worker spawned by the engine cannot, via Read / Edit /
    // Write, touch any file under the Boss state dir. The deny
    // list must name the dir and the `**` subtree for each tool
    // so a `Read("…/Boss")` ls and a `Read("…/Boss/state.db")`
    // both deny.
    //
    // Only `Read` and `Edit` rules are emitted, not `Write` — Claude Code's
    // permission engine matches both the Edit and Write *tools* against
    // `Edit(path)` rules, so a `Write(path)` deny rule matches nothing and is
    // dead weight (previously surfaced as a startup warning:
    // "Write(...) is not matched by file permission checks — only Edit(path)
    // rules are").
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let deny = parsed["permissions"]["deny"].as_array().expect("deny array present");
    let deny_set: Vec<&str> = deny.iter().filter_map(|v| v.as_str()).collect();
    let boss_dir = "/Users/brianduff/Library/Application Support/Boss";
    for tool in ["Read", "Edit"] {
        let bare = format!("{tool}({boss_dir})");
        let glob = format!("{tool}({boss_dir}/**)");
        assert!(
            deny_set.iter().any(|r| *r == bare),
            "expected deny rule {bare} in {deny_set:?}",
        );
        assert!(
            deny_set.iter().any(|r| *r == glob),
            "expected deny rule {glob} in {deny_set:?}",
        );
    }
    assert!(
        !deny_set.iter().any(|r| r.starts_with(&format!("Write({boss_dir}"))),
        "expected no Write(...) deny rule for the Boss state dir (inert in Claude Code's permission engine): {deny_set:?}",
    );
}

#[test]
fn settings_json_denies_bossctl_and_engine_lifecycle_verbs() {
    // bossctl is coordinator-only; `boss engine start|stop` reach
    // into engine process state. The rest of the `boss` surface
    // talks to the engine over its IPC socket and is fine.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
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
    ] {
        assert!(deny.contains(&rule), "expected deny rule {rule} in {deny:?}",);
    }
}

#[test]
fn reviewer_kind_adds_write_and_push_deny_rules_standard_does_not() {
    // Standard workers must not carry the reviewer deny rules — that
    // would break every implementation worker.
    let std_input = sample_input(); // worker_kind: Standard
    let std_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&std_input)).unwrap();
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
    let rev_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&rev_input)).unwrap();
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
fn reviewer_settings_json_has_fast_mode_standard_does_not() {
    // Reviewer workers are latency-sensitive: fastMode must be true.
    let mut rev_input = sample_input();
    rev_input.worker_kind = WorkerKind::Reviewer;
    let rev_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&rev_input)).unwrap();
    assert_eq!(
        rev_parsed["fastMode"],
        serde_json::json!(true),
        "reviewer settings.json must have fastMode:true",
    );

    // Standard workers must NOT have fastMode set at all.
    let std_input = sample_input();
    let std_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&std_input)).unwrap();
    assert!(
        std_parsed.get("fastMode").is_none() || std_parsed["fastMode"] == serde_json::json!(null),
        "standard worker settings.json must not carry fastMode (got {:?})",
        std_parsed.get("fastMode"),
    );
}

#[test]
fn triage_kind_adds_no_publish_deny_rules_standard_does_not() {
    // Triage workers must carry the read-only / no-publish denylist (they
    // investigate and emit a marker; they must not edit, push, or open a
    // PR). Standard implementation workers must NOT carry it.
    let std_input = sample_input(); // worker_kind: Standard
    let std_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&std_input)).unwrap();
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
    let triage_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&triage_input)).unwrap();
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
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
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
