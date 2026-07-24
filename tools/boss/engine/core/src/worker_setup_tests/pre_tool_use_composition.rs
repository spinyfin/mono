use super::super::*;
use super::helpers::*;

#[test]
fn revision_implementation_adds_gh_pr_create_guard_to_pre_tool_use() {
    let mut input = sample_input();
    input.execution_kind = "revision_implementation".into();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let pre = parsed["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse must be an array");
    // Must have 6 entries: the shim, the deterministic path guard, the
    // always-on boss-launch guard, the PR redirect guard (all standard workers),
    // the checkleft push guard (local standard worker), and the revision-only
    // gh-pr-create guard.
    assert_eq!(
        pre.len(),
        6,
        "revision_implementation PreToolUse must have shim + path guard + boss-launch guard + PR redirect guard + checkleft push guard + revision pr guard, got {pre:?}",
    );
    // The revision pr-guard is the Bash-matcher entry whose command inspects
    // `cube pr ensure`; both the PR redirect guard and the boss-launch guard are
    // also Bash-matched, so disambiguate by the `ensure` token.
    let revision_pr_guard = pre
        .iter()
        .find(|e| e["hooks"][0]["command"].as_str().unwrap_or("").contains("ensure"))
        .expect("revision PreToolUse must include the revision-specific gh-pr-create guard");
    // Revision guard command must block PR *creation* (gh pr create, cube pr create,
    // deprecated cube pr ensure) while pointing workers at cube pr update.
    let guard_cmd = revision_pr_guard["hooks"][0]["command"].as_str().unwrap_or("");
    assert!(
        guard_cmd.contains("gh") && guard_cmd.contains("pr") && guard_cmd.contains("create"),
        "revision guard must inspect gh pr create / cube pr create: {guard_cmd}",
    );
    assert!(
        guard_cmd.contains("cube") && guard_cmd.contains("ensure"),
        "revision guard must also block the deprecated cube pr ensure: {guard_cmd}",
    );
    assert!(
        guard_cmd.contains("cube pr update"),
        "revision guard block message must point workers at `cube pr update`: {guard_cmd}",
    );
    assert!(
        guard_cmd.contains("block"),
        "revision guard must produce a block decision: {guard_cmd}",
    );
    // The PR redirect guard must also be present (all standard workers).
    let pr_redirect_guard = pre
        .iter()
        .find(|e| {
            let cmd = e["hooks"][0]["command"].as_str().unwrap_or("");
            cmd.contains("jj git push") && cmd.contains("cube pr create") && !cmd.contains("ensure")
        })
        .expect("revision PreToolUse must include the PR redirect guard (all standard workers)");
    let redirect_cmd = pr_redirect_guard["hooks"][0]["command"].as_str().unwrap_or("");
    assert!(
        redirect_cmd.contains("cube pr update"),
        "PR redirect guard block message must mention cube pr update: {redirect_cmd}",
    );
}

#[test]
fn chore_implementation_has_pr_redirect_guard_but_no_revision_guard() {
    let input = sample_input(); // execution_kind: "chore_implementation"
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let pre = parsed["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse must be an array");
    // chore: [boss-event shim, deterministic path guard, boss-launch guard,
    // PR redirect guard (all standard workers), checkleft push guard].
    // The revision-only `gh pr create` guard (which blocks `cube pr ensure`) must NOT be present.
    assert_eq!(
        pre.len(),
        5,
        "chore_implementation PreToolUse must have shim + path guard + boss-launch guard + PR redirect guard + checkleft push guard, got {pre:?}",
    );
    assert_eq!(
        pre[0]["matcher"],
        serde_json::Value::String("*".into()),
        "first PreToolUse hook must be the catch-all shim",
    );
    let path_guard = pre[1]["hooks"][0]["command"].as_str().unwrap_or("");
    assert!(
        path_guard.contains("BOSS_DATA_DIR=") && path_guard.contains(PATH_GUARD_SCRIPT_NAME),
        "second PreToolUse hook must be the path guard, got {path_guard}",
    );
    // The PR redirect guard must be present for chore workers.
    let has_pr_redirect_guard = pre.iter().any(|e| {
        let cmd = e["hooks"][0]["command"].as_str().unwrap_or("");
        cmd.contains("jj git push") && cmd.contains("cube pr create")
    });
    assert!(has_pr_redirect_guard, "chore must carry the PR redirect guard: {pre:?}",);
    // No revision-specific guard: nothing inspects `cube ... ensure`.
    for entry in pre {
        let cmd = entry["hooks"][0]["command"].as_str().unwrap_or("");
        assert!(
            !cmd.contains("ensure"),
            "chore must not carry the revision-specific gh-pr-create guard: {cmd}",
        );
    }
}

/// Design and investigation workers are also `WorkerKind::Standard` and must
/// carry the PR redirect guard. The original root cause (T686) was a DESIGN
/// worker's prelude diverging from chore/task preludes — pin that cross-prelude
/// invariant here so future drift is caught immediately.
#[test]
fn design_and_investigation_workers_carry_pr_redirect_guard() {
    for execution_kind in ["project_design", "investigation_implementation"] {
        let mut input = sample_input();
        input.execution_kind = execution_kind.into();
        let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
        let pre = parsed["hooks"]["PreToolUse"]
            .as_array()
            .unwrap_or_else(|| panic!("{execution_kind}: PreToolUse must be an array"));
        // Must carry the PR redirect guard: the Bash-matcher hook whose
        // command inspects `jj git push` and `cube pr create` (but not
        // `ensure`, which is the revision-only guard).
        let has_pr_redirect_guard = pre.iter().any(|e| {
            let cmd = e["hooks"][0]["command"].as_str().unwrap_or("");
            cmd.contains("jj git push") && cmd.contains("cube pr create") && !cmd.contains("ensure")
        });
        assert!(
            has_pr_redirect_guard,
            "{execution_kind} worker must carry the PR redirect guard: {pre:?}",
        );
        // Must NOT carry the revision-specific guard (which blocks `cube pr ensure`).
        let has_revision_guard = pre
            .iter()
            .any(|e| e["hooks"][0]["command"].as_str().unwrap_or("").contains("ensure"));
        assert!(
            !has_revision_guard,
            "{execution_kind} worker must not carry the revision-specific guard: {pre:?}",
        );
    }
}

/// Every worker session — regardless of kind — must carry a PreToolUse
/// guard that blocks *launching Boss itself*: the macOS app, its bundled
/// engine, or an app-macos test that can spawn the real app. `bazel build`
/// must stay allowed. The guard is a Bash-matcher inline-Python decision
/// hook (distinct from the revision gh-pr-create Bash guard).
#[test]
fn every_worker_blocks_launching_boss_in_pre_tool_use() {
    for kind in ["chore_implementation", "revision_implementation"] {
        let mut input = sample_input();
        input.execution_kind = kind.into();
        let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
        let pre = parsed["hooks"]["PreToolUse"]
            .as_array()
            .expect("PreToolUse must be an array");
        // Disambiguate from the gh-pr-create guard by content.
        let guard = pre
            .iter()
            .find(|e| {
                let c = e["hooks"][0]["command"].as_str().unwrap_or("");
                c.contains("Boss.app") && c.contains("app-macos")
            })
            .unwrap_or_else(|| panic!("{kind} PreToolUse must include a boss-launch guard"));
        assert_eq!(
            guard["matcher"], "Bash",
            "{kind} boss-launch guard must match the Bash tool",
        );
        let cmd = guard["hooks"][0]["command"].as_str().unwrap_or("");
        // Covers app launch (open / bundle binary / bundle id), the engine
        // binary by basename, bazel run of an app-macos target, and swift
        // run — and blocks.
        assert!(
            cmd.contains("Boss.app")
                && cmd.contains("dev.spinyfin.bossmacapp")
                && cmd.contains("app-macos")
                && cmd.contains("'swift'")
                && cmd.contains("block"),
            "{kind} boss-launch guard must block app/engine/run launches: {cmd}",
        );
        // `bazel test` / `swift test` run the app-macos unit suite, which
        // has no test_host, and must stay allowed. The guard only ever
        // inspects a group whose second token is `run`.
        assert!(
            cmd.contains("rest[1]=='run'"),
            "{kind} boss-launch guard must key on `run`, never on test: {cmd}",
        );
    }
}

/// Defense-in-depth: even if `execution_kind` is wrong (e.g. a revision
/// re-dispatched as `task_implementation` due to a bug), the guard fires
/// as long as `task_kind == "revision"`.  This ensures the structural
/// invariant holds regardless of execution-kind derivation errors.
#[test]
fn revision_task_kind_adds_gh_pr_create_guard_even_with_wrong_execution_kind() {
    let mut input = sample_input();
    // Simulate the bug scenario: execution_kind was mis-derived as
    // task_implementation but the task itself is a revision.
    input.execution_kind = "task_implementation".into();
    input.task_kind = Some("revision".into());

    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let pre = parsed["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse must be an array");

    // shim + path guard + boss-launch guard + PR redirect guard (all standard)
    // + checkleft push guard + revision-specific pr guard = 6 total
    assert_eq!(
        pre.len(),
        6,
        "revision task_kind must add the revision pr guard on top of the PR redirect guard \
         (shim + path guard + boss-launch guard + PR redirect guard + checkleft push guard + \
         revision pr guard) even when execution_kind is wrong, got {pre:?}",
    );
    // The revision-specific guard blocks `cube pr ensure` — find it by that token.
    let revision_pr_guard = pre
        .iter()
        .find(|e| e["hooks"][0]["command"].as_str().unwrap_or("").contains("ensure"))
        .expect("revision task_kind must include the revision-specific gh-pr-create guard");
    let guard_cmd = revision_pr_guard["hooks"][0]["command"].as_str().unwrap_or("");
    assert!(
        guard_cmd.contains("block"),
        "revision guard must produce a block decision: {guard_cmd}",
    );
}
