use super::super::*;
use super::helpers::*;

// ── Answer-agent capability-restricted dispatch (P3a) ──────────────────────

fn answer_agent_input() -> WorkerSetupInput {
    WorkerSetupInput {
        execution_kind: "answer_agent".into(),
        task_kind: None,
        worker_kind: WorkerKind::AnswerAgent,
        ..sample_input()
    }
}

#[test]
fn worker_kind_for_execution_maps_every_kind() {
    use boss_protocol::ExecutionKind;
    // Restricted kinds get their reduced posture.
    assert_eq!(
        worker_kind_for_execution(&ExecutionKind::AnswerAgent),
        WorkerKind::AnswerAgent
    );
    assert_eq!(
        worker_kind_for_execution(&ExecutionKind::PrReview),
        WorkerKind::Reviewer
    );
    assert_eq!(
        worker_kind_for_execution(&ExecutionKind::AutomationTriage),
        WorkerKind::Triage
    );
    // Everything else is a Standard implementer.
    for kind in [
        ExecutionKind::TaskImplementation,
        ExecutionKind::ChoreImplementation,
        ExecutionKind::RevisionImplementation,
        ExecutionKind::ProjectDesign,
        ExecutionKind::ProductDesign,
        ExecutionKind::InvestigationImplementation,
        ExecutionKind::CiRemediation,
        ExecutionKind::ConflictResolution,
    ] {
        assert_eq!(worker_kind_for_execution(&kind), WorkerKind::Standard, "kind {kind:?}");
    }
}

#[test]
fn forced_permission_mode_is_dontask_only_for_answer_agent() {
    // The forced CLI mode is derived from WorkerKind (not a separate switch on
    // ExecutionKind), so it cannot diverge from the settings posture.
    assert_eq!(WorkerKind::AnswerAgent.forced_permission_mode(), Some("dontAsk"));
    assert_eq!(WorkerKind::Standard.forced_permission_mode(), None);
    assert_eq!(WorkerKind::Reviewer.forced_permission_mode(), None);
    assert_eq!(WorkerKind::Triage.forced_permission_mode(), None);
}

#[test]
fn answer_agent_settings_use_dontask_allowlist_mode() {
    // The whole point of P3a: an allowlist, not a blocklist. `dontAsk`
    // auto-denies every tool call except `permissions.allow` matches and
    // built-in read-only Bash — a true deny-by-default sandbox.
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&answer_agent_input())).unwrap();
    assert_eq!(
        parsed["permissions"]["defaultMode"],
        serde_json::Value::String("dontAsk".into()),
        "answer agent must run deny-by-default; got: {parsed}",
    );
}

#[test]
fn answer_agent_allow_list_is_exactly_the_reduced_table() {
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&answer_agent_input())).unwrap();
    let allow: Vec<&str> = parsed["permissions"]["allow"]
        .as_array()
        .expect("answer agent settings carry an allow list")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    // Exactly the read-only tools plus the single thread-reply command —
    // nothing else. A change here is a capability change and must be reviewed.
    assert_eq!(
        allow,
        vec![
            "Read",
            "Grep",
            "Glob",
            &format!("Bash({}:*)", crate::answer_agent::THREAD_REPLY_COMMAND),
        ],
    );
    // Crucially, bare `Bash` is NOT allowlisted — that would grant arbitrary
    // shell and defeat the sandbox (read-only Bash is covered by dontAsk).
    assert!(!allow.contains(&"Bash"));
}

#[test]
fn answer_agent_deny_belt_blocks_every_mutating_surface() {
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&answer_agent_input())).unwrap();
    let deny: Vec<&str> = parsed["permissions"]["deny"]
        .as_array()
        .expect("deny array present")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    // Every answer_agent_deny_rules() entry must survive into the settings —
    // deny always wins over allow, so this belt holds under any permission mode.
    for rule in answer_agent_deny_rules() {
        assert!(
            deny.contains(&rule.as_str()),
            "deny belt missing {rule:?}; got: {deny:?}"
        );
    }
    // Spot-check the load-bearing categories: file writes, push/PR, and cube.
    for expected in [
        "Edit(**)",
        "NotebookEdit(**)",
        "Bash(git push)",
        "Bash(cube pr)",
        "Bash(cube:*)",
    ] {
        assert!(
            deny.contains(&expected),
            "expected deny to include {expected:?}; got: {deny:?}"
        );
    }
    // Write(...) rules are never emitted — Claude Code matches both the Edit
    // and Write tools against Edit(path) rules, so a parallel Write(path)
    // rule would be inert (this was the bug: the generator used to emit
    // dead Write(...) deny rules that Claude Code warned about at startup).
    assert!(
        !deny.iter().any(|r| r.starts_with("Write(")),
        "answer agent must NOT carry any Write(...) rule — inert in Claude Code's permission engine (got {deny:?})",
    );
}

#[test]
fn answer_agent_gets_read_only_claude_md_not_the_pr_deliverable_one() {
    let md = claude_md_for(&answer_agent_input());
    assert!(md.contains("answer-agent"));
    assert!(md.contains("Read-only mandate"));
    // Must NOT be the Standard "a PR is the deliverable" file.
    assert!(!md.contains("Pull requests are the deliverable"));
    assert!(!md.contains("cube pr create --branch"));
}

#[test]
fn standard_worker_keeps_auto_mode_and_no_allow_list() {
    // Regression guard: the allowlist posture is scoped to the answer agent.
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&sample_input())).unwrap();
    assert_eq!(
        parsed["permissions"]["defaultMode"],
        serde_json::Value::String("auto".into())
    );
    assert!(
        parsed["permissions"].get("allow").is_none(),
        "standard workers must not carry an allow list"
    );
}
