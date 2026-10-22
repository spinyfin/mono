use super::support::{ExpectedCommand, FakeRunner, audit_events, jj_status_dirty, seed_mono_repo, with_database_path};
use clap::Parser;

use crate::cli::Cli;

use crate::app::dispatch::run_with_dependencies;
use crate::app::errors::CubeError;

#[test]
fn workspace_lease_recovers_from_stale_jj_working_copy() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-004");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // First `jj git fetch` returns the stale-working-copy error.
    // The wrapper should run `jj workspace update-stale` once, then
    // retry the original command. The remainder of the lease then
    // proceeds normally.
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::stale(workspace_path.clone(), "jj", &["git", "fetch"]),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["workspace", "update-stale"],
            "Working copy now at: abc1234",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "stale demo"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease should auto-recover from stale");
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-004");
    assert_eq!(result.payload["workspace"]["head_commit"], "abc1234");

    // The recovery is observable in the audit log.
    let audit_dir = database_path.parent().unwrap().join("audit");
    let logs = std::fs::read_dir(&audit_dir)
        .expect("audit dir")
        .filter_map(|e| e.ok())
        .map(|e| std::fs::read_to_string(e.path()).expect("audit log"))
        .collect::<Vec<_>>()
        .join("");
    assert!(
        logs.contains("\"event\":\"workspace.stale_recovered\""),
        "expected stale_recovered audit event, got: {logs}"
    );
    assert!(
        logs.contains(workspace_path.display().to_string().as_str()),
        "audit event should record the workspace path"
    );
}

#[test]
fn workspace_lease_surfaces_stale_recovery_failure() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-004");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // `jj git fetch` reports stale; `jj workspace update-stale`
    // itself fails. The lease must not pretend success — surface a
    // distinct StaleRecoveryFailed error.
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::stale(workspace_path.clone(), "jj", &["git", "fetch"]),
        ExpectedCommand {
            cwd: workspace_path.clone(),
            program: "jj".to_string(),
            args: vec!["workspace".to_string(), "update-stale".to_string()],
            result: Err(CubeError::CommandFailed {
                program: "jj".to_string(),
                args: vec!["workspace".to_string(), "update-stale".to_string()],
                status: Some(1),
                stderr: "Error: workspace operation failed".to_string(),
            }),
            creates_dir: None,
            duration: std::time::Duration::ZERO,
        },
    ]);

    let error = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "stale fail"]),
        Some(&database_path),
        &runner,
    )
    .expect_err("lease should fail when stale recovery itself fails");
    runner.assert_exhausted();

    match error {
        CubeError::StaleRecoveryFailed {
            workspace_path: path,
            cause,
        } => {
            assert_eq!(path, workspace_path);
            assert!(
                cause.contains("update-stale"),
                "cause should mention update-stale: {cause}"
            );
        }
        other => panic!("expected StaleRecoveryFailed, got {other:?}"),
    }
}

#[test]
fn workspace_lease_recovers_from_op_log_divergence() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-004");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // `jj status` returns the op-log divergence error (exit 255,
    // "seems to be a sibling"). The wrapper should run
    // `jj workspace update-stale` once, then retry `jj status`. The
    // remainder of the lease then proceeds normally.
    let runner = FakeRunner::new(vec![
        ExpectedCommand::op_diverged(workspace_path.clone(), "jj", &["status", "--no-pager"]),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["workspace", "update-stale"],
            "Working copy now at: abc1234",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "op-diverged demo"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease should auto-recover from op-log divergence");
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-004");
    assert_eq!(result.payload["workspace"]["head_commit"], "abc1234");

    let audit_dir = database_path.parent().unwrap().join("audit");
    let logs = std::fs::read_dir(&audit_dir)
        .expect("audit dir")
        .filter_map(|e| e.ok())
        .map(|e| std::fs::read_to_string(e.path()).expect("audit log"))
        .collect::<Vec<_>>()
        .join("");
    assert!(
        logs.contains("\"event\":\"workspace.op_diverged_recovered\""),
        "expected op_diverged_recovered audit event, got: {logs}"
    );
    assert!(
        logs.contains(workspace_path.display().to_string().as_str()),
        "audit event should record the workspace path"
    );
}

#[test]
fn workspace_lease_skips_op_diverged_unrecoverable_and_provisions_new() {
    // When jj status reports op-log divergence AND jj workspace update-stale
    // fails, the poisoned workspace must be SKIPPED — not hard-failed —
    // and the lease must succeed by provisioning a fresh workspace.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-004");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // After the poisoned workspace is skipped, the pool has only that one
    // entry, so the next ID is mono-agent-005.
    let new_path = workspace_root.join("mono-agent-005");
    let staging = workspace_root.join(".incoming-mono-agent-005");

    let runner = FakeRunner::new(vec![
        // Health check: jj status → op-diverged
        ExpectedCommand::op_diverged(workspace_path.clone(), "jj", &["status", "--no-pager"]),
        // Recovery attempt: update-stale fails
        ExpectedCommand {
            cwd: workspace_path.clone(),
            program: "jj".to_string(),
            args: vec!["workspace".to_string(), "update-stale".to_string()],
            result: Err(CubeError::CommandFailed {
                program: "jj".to_string(),
                args: vec!["workspace".to_string(), "update-stale".to_string()],
                status: Some(1),
                stderr: "Error: workspace operation failed".to_string(),
            }),
            creates_dir: None,
            duration: std::time::Duration::ZERO,
        },
        // Fallback: auto-provision a fresh workspace and reset it.
        ExpectedCommand::workspace_add_mono(&workspace_root, &staging),
        ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "fresh5678",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "op-diverged fail"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease must succeed even when one workspace's stale recovery fails");
    runner.assert_exhausted();

    // The lease landed on the freshly provisioned workspace, not the poisoned one.
    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-005");
    assert_eq!(result.payload["workspace"]["head_commit"], "fresh5678");

    // The skipped workspace is recorded in health_check output.
    let hc = result.payload["health_check"].as_array().expect("health_check array");
    assert!(
        hc.iter()
            .any(|e| e["workspace_id"] == "mono-agent-004" && e["skipped"] == true),
        "mono-agent-004 must be marked skipped: {hc:?}",
    );
}

#[test]
fn workspace_lease_health_check_stale_op_signature_recovered_and_leased() {
    // Regression test for T1812: jj prints "Could not read working copy's
    // operation." (JJ_STALE_OP_SIGNATURE) instead of the older "working
    // copy is stale" text. The pre-lease health check must detect this
    // as StaleWorkingCopy, run jj workspace update-stale, and lease the
    // workspace after the retry succeeds — not hard-fail the dispatch.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-004");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let runner = FakeRunner::new(vec![
        // Health check: jj status exits non-zero with the alternate stale signature.
        ExpectedCommand {
            cwd: workspace_path.clone(),
            program: "jj".to_string(),
            args: vec!["status".to_string(), "--no-pager".to_string()],
            result: Err(CubeError::CommandFailed {
                program: "jj".to_string(),
                args: vec!["status".to_string(), "--no-pager".to_string()],
                status: Some(1),
                stderr: "Error: Could not read working copy's operation. \
                             Hint: Run jj workspace update-stale to recover."
                    .to_string(),
            }),
            creates_dir: None,
            duration: std::time::Duration::ZERO,
        },
        // Recovery: update-stale succeeds.
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["workspace", "update-stale"],
            "Working copy now at: abc1234",
        ),
        // Retry health check: clean.
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        // Normal lease reset proceeds.
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "stale-op demo"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease must succeed after stale-op recovery");
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-004");
    assert_eq!(result.payload["workspace"]["head_commit"], "abc1234");

    // Recovery is observable in the audit log.
    let audit_dir = database_path.parent().unwrap().join("audit");
    let logs = std::fs::read_dir(&audit_dir)
        .expect("audit dir")
        .filter_map(|e| e.ok())
        .map(|e| std::fs::read_to_string(e.path()).expect("audit log"))
        .collect::<Vec<_>>()
        .join("");
    assert!(
        logs.contains("\"event\":\"workspace.stale_recovered\""),
        "expected stale_recovered audit event, got: {logs}"
    );
}

#[test]
fn workspace_lease_health_check_stale_status_unrecoverable_falls_through_to_new_workspace() {
    // Regression test for T1812: when jj status returns the stale signature
    // and jj workspace update-stale itself fails, the poisoned workspace must
    // be skipped (not hard-fail the lease) and a fresh workspace provisioned.
    // This must hold regardless of fallback_policy.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-004");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // After the poisoned workspace is skipped the pool has one entry, so
    // next_workspace_id picks mono-agent-005.
    let new_path = workspace_root.join("mono-agent-005");
    let staging = workspace_root.join(".incoming-mono-agent-005");

    let runner = FakeRunner::new(vec![
        // Health check: jj status returns the stale-op alternate signature.
        ExpectedCommand {
            cwd: workspace_path.clone(),
            program: "jj".to_string(),
            args: vec!["status".to_string(), "--no-pager".to_string()],
            result: Err(CubeError::CommandFailed {
                program: "jj".to_string(),
                args: vec!["status".to_string(), "--no-pager".to_string()],
                status: Some(1),
                stderr: "Error: Could not read working copy's operation. \
                             Hint: Run jj workspace update-stale to recover."
                    .to_string(),
            }),
            creates_dir: None,
            duration: std::time::Duration::ZERO,
        },
        // Recovery attempt: update-stale fails.
        ExpectedCommand {
            cwd: workspace_path.clone(),
            program: "jj".to_string(),
            args: vec!["workspace".to_string(), "update-stale".to_string()],
            result: Err(CubeError::CommandFailed {
                program: "jj".to_string(),
                args: vec!["workspace".to_string(), "update-stale".to_string()],
                status: Some(1),
                stderr: "Error: workspace operation failed".to_string(),
            }),
            creates_dir: None,
            duration: std::time::Duration::ZERO,
        },
        // Fallback: auto-provision a fresh workspace and reset it.
        ExpectedCommand::workspace_add_mono(&workspace_root, &staging),
        ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "fresh5678",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "stale-op-unrecoverable"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease must succeed by provisioning a new workspace when stale recovery fails");
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-005");
    assert_eq!(result.payload["workspace"]["head_commit"], "fresh5678");

    // The poisoned workspace is recorded as skipped.
    let hc = result.payload["health_check"].as_array().expect("health_check array");
    assert!(
        hc.iter()
            .any(|e| e["workspace_id"] == "mono-agent-004" && e["skipped"] == true),
        "mono-agent-004 must be marked skipped: {hc:?}",
    );
}

#[test]
fn workspace_lease_colocate_inits_when_git_repo_has_no_jj() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-004");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");
    // Simulate a workspace that has .git but no .jj.
    std::fs::create_dir_all(workspace_path.join(".git")).expect(".git dir");

    seed_mono_repo(&workspace_root, &database_path);

    // `jj status` returns the "no jj repo" error. The wrapper should
    // run `jj git init --colocate` once, then retry `jj status`. The
    // remainder of the lease proceeds normally.
    let runner = FakeRunner::new(vec![
        ExpectedCommand::no_jj_repo(workspace_path.clone(), "jj", &["status", "--no-pager"]),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "init", "--colocate"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "colocate init demo"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease should auto-recover by running jj git init --colocate");
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-004");
    assert_eq!(result.payload["workspace"]["head_commit"], "abc1234");

    let audit_dir = database_path.parent().unwrap().join("audit");
    let logs = std::fs::read_dir(&audit_dir)
        .expect("audit dir")
        .filter_map(|e| e.ok())
        .map(|e| std::fs::read_to_string(e.path()).expect("audit log"))
        .collect::<Vec<_>>()
        .join("");
    assert!(
        logs.contains("\"event\":\"workspace.jj_colocate_initialised\""),
        "expected jj_colocate_initialised audit event, got: {logs}"
    );
    assert!(
        logs.contains(workspace_path.display().to_string().as_str()),
        "audit event should record the workspace path"
    );
}

#[test]
fn workspace_lease_self_heals_broken_empty_and_auto_creates() {
    // A workspace directory with neither .jj/ nor .git/ is a husk holding
    // no recoverable work. Rather than blocking the lease, cube detects it
    // via a directory check (no jj `status` call on the husk), GCs it
    // (removes the directory and forgets its row), and provisions a fresh
    // workspace by cloning. The lease then succeeds (issue #845).
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let husk_path = workspace_root.join("mono-agent-004");
    // Intentionally no .jj/ or .git/ — this is the broken-empty state.
    std::fs::create_dir_all(&husk_path).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // After the husk is GC'd the pool is empty, so `next_workspace_id`
    // reuses the lowest slot. The runner expects only the clone + track +
    // reset sequence for the fresh workspace — never a `status` call
    // against the broken-empty husk.
    let new_path = workspace_root.join("mono-agent-001");
    let staging = workspace_root.join(".incoming-mono-agent-001");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::workspace_add_mono(&workspace_root, &staging),
        ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "no git dir"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease should self-heal the husk and auto-create a fresh workspace");
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-001");
    assert_eq!(result.payload["workspace"]["state"], "leased");

    // The husk directory was removed and its registry row forgotten.
    assert!(!husk_path.exists(), "broken-empty husk should be removed");
    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let rows = store.list_workspaces_filtered(&WorkspaceListFilter::default()).unwrap();
    let ids: Vec<_> = rows.iter().map(|r| r.workspace_id.as_str()).collect();
    assert!(
        !ids.contains(&"mono-agent-004"),
        "husk row should be forgotten; saw {ids:?}"
    );
    assert!(
        ids.contains(&"mono-agent-001"),
        "fresh workspace should exist; saw {ids:?}"
    );

    // Audit log records both the detection and the GC of the husk.
    let events = audit_events(&tempdir);
    assert!(
        events.iter().any(|e| e["event"] == "workspace.broken_empty"),
        "expected workspace.broken_empty audit event; got: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "workspace.broken_empty_gc" && e["workspace_id"] == "mono-agent-004"),
        "expected workspace.broken_empty_gc audit event for the husk; got: {events:?}"
    );
}

#[test]
fn workspace_lease_self_heals_two_broken_empty_husks() {
    // Exact repro from issue #845: every free workspace is broken-empty
    // (the `ci-infra-027` / `ci-infra-028` case). The lease must GC both
    // husks and provision a fresh workspace rather than failing with
    // "no free workspace".
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let husk_a = workspace_root.join("mono-agent-027");
    let husk_b = workspace_root.join("mono-agent-028");
    // Neither has .jj/ nor .git/ — both are broken-empty husks.
    std::fs::create_dir_all(&husk_a).expect("husk a");
    std::fs::create_dir_all(&husk_b).expect("husk b");

    seed_mono_repo(&workspace_root, &database_path);

    // Both husks GC'd → pool empty → fresh workspace takes the lowest slot.
    let new_path = workspace_root.join("mono-agent-001");
    let staging = workspace_root.join(".incoming-mono-agent-001");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::workspace_add_mono(&workspace_root, &staging),
        ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "two husks"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease should succeed by GC'ing both husks and auto-creating");
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-001");
    assert!(!husk_a.exists(), "husk 027 should be removed");
    assert!(!husk_b.exists(), "husk 028 should be removed");
}

#[test]
fn workspace_lease_gcs_broken_empty_and_keeps_dirty_then_auto_creates() {
    // Mixed pool: one dirty workspace (holds possibly-unpushed work) and
    // one broken-empty husk. The husk is GC'd and a fresh workspace is
    // auto-created; the dirty workspace is left untouched for the operator
    // to reclaim. A broken-empty entry must never turn into a hard stop,
    // even when a dirty entry is also present.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let dirty_path = workspace_root.join("mono-agent-003");
    let husk_path = workspace_root.join("mono-agent-027");
    std::fs::create_dir_all(dirty_path.join(".jj")).expect("dirty dir");
    std::fs::create_dir_all(&husk_path).expect("husk dir");

    seed_mono_repo(&workspace_root, &database_path);

    // Health check visits 003 (dirty `status`) then 027 (broken-empty, no
    // jj call). The husk is GC'd; `next_workspace_id` over the surviving
    // dirty 003 yields mono-agent-004 for the fresh clone.
    let new_path = workspace_root.join("mono-agent-004");
    let staging = workspace_root.join(".incoming-mono-agent-004");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(dirty_path.clone(), "jj", &["status", "--no-pager"], jj_status_dirty()),
        ExpectedCommand::workspace_add_mono(&workspace_root, &staging),
        ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(new_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "dirty plus husk"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease should succeed: GC the husk, keep the dirty one, auto-create");
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-004");
    // The husk is gone; the dirty workspace is preserved for inspection.
    assert!(!husk_path.exists(), "broken-empty husk should be removed");
    assert!(dirty_path.exists(), "dirty workspace must be left untouched");
    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    let dirty_row = store.get_workspace_by_path(&dirty_path).unwrap().unwrap();
    assert_eq!(
        dirty_row.health_status,
        Some(crate::metadata::WorkspaceHealth::Dirty),
        "dirty workspace should still be marked dirty"
    );
}
