use super::support::{
    ExpectedCommand, FakeRunner, gc_noop_command, gc_pr_remote_noop_command, lease_runner_for, lease_runner_with_setup,
    release_guard_reusable_command, seed_mono_repo, with_database_path, write_setup_yaml,
};
use clap::Parser;
use serde_json::json;

use crate::cli::Cli;

use crate::app::dispatch::run_with_dependencies;
use crate::app::errors::CubeError;

#[test]
fn workspace_setup_returns_empty_when_no_setup_yaml() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).unwrap();

    seed_mono_repo(&workspace_root, &database_path);

    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();

    let setup_runner = FakeRunner::default();
    let setup = Cli::parse_from([
        "cube",
        "workspace",
        "setup",
        "--workspace",
        &workspace_path.display().to_string(),
    ]);
    let result = run_with_dependencies(setup, Some(&database_path), &setup_runner).expect("setup");
    setup_runner.assert_exhausted();
    assert_eq!(result.message, "No setup steps are configured for mono-agent-001.");
    assert_eq!(result.payload["setup"]["steps"], json!([]));
}

// ── gc tests ─────────────────────────────────────────────────────────────

#[test]
fn workspace_setup_runs_steps_then_skips_when_fingerprint_unchanged() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).unwrap();
    std::fs::write(workspace_path.join("pnpm-lock.yaml"), b"v1").unwrap();
    write_setup_yaml(
        &workspace_path,
        r#"version: 1
steps:
  - id: deps
    command: pnpm install --frozen-lockfile
    fingerprint:
      - file: pnpm-lock.yaml
"#,
    );

    seed_mono_repo(&workspace_root, &database_path);

    // First lease runs the deps step.
    let lease_runner = lease_runner_with_setup(
        &workspace_path,
        "abc1234",
        vec![ExpectedCommand::ok(
            workspace_path.clone(),
            "sh",
            &["-c", "pnpm install --frozen-lockfile"],
            "",
        )],
    );
    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("first lease");
    lease_runner.assert_exhausted();
    let setup_steps = lease_result.payload["setup"]["steps"].as_array().expect("steps array");
    assert_eq!(setup_steps.len(), 1);
    assert_eq!(setup_steps[0]["id"], "deps");
    assert_eq!(setup_steps[0]["status"], "ran");
    let lease_id = lease_result.payload["workspace"]["lease_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Release so we can re-lease cleanly.
    let release_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        release_guard_reusable_command(&workspace_path),
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
        gc_noop_command(&workspace_path),
        gc_pr_remote_noop_command(&workspace_path),
    ]);
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release");
    release_runner.assert_exhausted();

    // Second lease: lockfile unchanged, deps step is skipped (no
    // pnpm command in expectations).
    let second_lease_runner = lease_runner_for(&workspace_path, "def5678");
    let second_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo2"]),
        Some(&database_path),
        &second_lease_runner,
    )
    .expect("second lease");
    second_lease_runner.assert_exhausted();
    let second_steps = second_result.payload["setup"]["steps"].as_array().expect("steps array");
    assert_eq!(second_steps.len(), 1);
    assert_eq!(second_steps[0]["status"], "skipped");
    assert_eq!(second_steps[0]["reason"], "fingerprint_unchanged");
}

#[test]
fn workspace_setup_reruns_when_lockfile_changes() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).unwrap();
    std::fs::write(workspace_path.join("pnpm-lock.yaml"), b"v1").unwrap();
    write_setup_yaml(
        &workspace_path,
        r#"version: 1
steps:
  - id: deps
    command: pnpm install
    fingerprint:
      - file: pnpm-lock.yaml
"#,
    );

    seed_mono_repo(&workspace_root, &database_path);

    let lease_runner = lease_runner_with_setup(
        &workspace_path,
        "abc1234",
        vec![ExpectedCommand::ok(
            workspace_path.clone(),
            "sh",
            &["-c", "pnpm install"],
            "",
        )],
    );
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("first lease");
    lease_runner.assert_exhausted();

    // Lockfile bumps; re-running setup explicitly (without re-leasing)
    // should pick up the change.
    std::fs::write(workspace_path.join("pnpm-lock.yaml"), b"v2").unwrap();

    let setup_runner = FakeRunner::new(vec![ExpectedCommand::ok(
        workspace_path.clone(),
        "sh",
        &["-c", "pnpm install"],
        "",
    )]);
    let setup_result = run_with_dependencies(
        Cli::parse_from([
            "cube",
            "workspace",
            "setup",
            "--workspace",
            &workspace_path.display().to_string(),
        ]),
        Some(&database_path),
        &setup_runner,
    )
    .expect("setup");
    setup_runner.assert_exhausted();
    let steps = setup_result.payload["setup"]["steps"].as_array().unwrap();
    assert_eq!(steps[0]["status"], "ran");
}

#[test]
fn workspace_setup_on_create_skips_after_first_run() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).unwrap();
    write_setup_yaml(
        &workspace_path,
        r#"version: 1
steps:
  - id: secrets
    command: ./decode-secrets.sh
    run_when: on-create
"#,
    );

    seed_mono_repo(&workspace_root, &database_path);

    // First lease: on-create runs once.
    let lease_runner = lease_runner_with_setup(
        &workspace_path,
        "abc1234",
        vec![ExpectedCommand::ok(
            workspace_path.clone(),
            "sh",
            &["-c", "./decode-secrets.sh"],
            "",
        )],
    );
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("first lease");
    lease_runner.assert_exhausted();

    // Release before re-leasing the same workspace.
    let workspace_record = {
        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        store.get_workspace_by_path(&workspace_path).unwrap().unwrap()
    };
    let release_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        release_guard_reusable_command(&workspace_path),
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
        gc_noop_command(&workspace_path),
        gc_pr_remote_noop_command(&workspace_path),
    ]);
    run_with_dependencies(
        Cli::parse_from([
            "cube",
            "workspace",
            "release",
            "--lease",
            workspace_record.lease_id.as_deref().unwrap(),
        ]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release");
    release_runner.assert_exhausted();

    // Second lease: on-create should skip (no decode-secrets in expectations).
    let second_runner = lease_runner_for(&workspace_path, "def5678");
    let second = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo2"]),
        Some(&database_path),
        &second_runner,
    )
    .expect("second lease");
    second_runner.assert_exhausted();
    let steps = second.payload["setup"]["steps"].as_array().unwrap();
    assert_eq!(steps[0]["status"], "skipped");
    assert_eq!(steps[0]["reason"], "already_ran");
}

#[test]
fn workspace_setup_failure_surfaces_step_id_and_retains_lease() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).unwrap();
    write_setup_yaml(
        &workspace_path,
        r#"version: 1
steps:
  - id: deps
    command: pnpm install
    run_when: always
"#,
    );

    seed_mono_repo(&workspace_root, &database_path);

    let failing = ExpectedCommand {
        cwd: workspace_path.clone(),
        program: "sh".to_string(),
        args: vec!["-c".to_string(), "pnpm install".to_string()],
        result: Err(CubeError::CommandFailed {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "pnpm install".to_string()],
            status: Some(1),
            stderr: "boom".to_string(),
        }),
        creates_dir: None,
    };
    let lease_runner = lease_runner_with_setup(&workspace_path, "abc1234", vec![failing]);

    let error = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect_err("lease should surface setup failure");
    lease_runner.assert_exhausted();
    match error {
        CubeError::SetupStepFailed { step, error } => {
            assert_eq!(step, "deps");
            assert!(error.contains("pnpm"), "error mentions program: {error}");
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // Lease is retained: the workspace row remains leased so the user
    // can rerun `cube workspace setup` to repair it.
    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    let record = store.get_workspace_by_path(&workspace_path).unwrap().unwrap();
    assert_eq!(record.state, crate::metadata::WorkspaceState::Leased);
    assert!(record.lease_id.is_some());
}

#[test]
fn workspace_setup_failure_with_release_flag_releases_lease() {
    // The engine passes `--release-on-setup-failure` so a setup failure
    // it can't repair never leaks a leased-but-unusable workspace (the
    // anaplian failure-mode A). Same failing setup as the retain test,
    // but the workspace must end up FREE with the lease cleared.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).unwrap();
    write_setup_yaml(
        &workspace_path,
        r#"version: 1
steps:
  - id: deps
    command: pnpm install
    run_when: always
"#,
    );

    seed_mono_repo(&workspace_root, &database_path);

    let failing = ExpectedCommand {
        cwd: workspace_path.clone(),
        program: "sh".to_string(),
        args: vec!["-c".to_string(), "pnpm install".to_string()],
        result: Err(CubeError::CommandFailed {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "pnpm install".to_string()],
            status: Some(1),
            stderr: "boom".to_string(),
        }),
        creates_dir: None,
    };
    let lease_runner = lease_runner_with_setup(&workspace_path, "abc1234", vec![failing]);

    let error = run_with_dependencies(
        Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "demo",
            "--release-on-setup-failure",
        ]),
        Some(&database_path),
        &lease_runner,
    )
    .expect_err("lease should surface setup failure");
    lease_runner.assert_exhausted();
    // The original setup error is still surfaced — the flag changes the
    // lease disposition, not the returned error.
    match error {
        CubeError::SetupStepFailed { step, error } => {
            assert_eq!(step, "deps");
            assert!(error.contains("pnpm"), "error mentions program: {error}");
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // The lease was released: the workspace is free again and nothing
    // is stranded (the fix for the leak).
    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    let record = store.get_workspace_by_path(&workspace_path).unwrap().unwrap();
    assert_eq!(
        record.state,
        crate::metadata::WorkspaceState::Free,
        "release-on-setup-failure must hand the workspace back",
    );
    assert!(
        record.lease_id.is_none(),
        "the lease id must be cleared after a released setup failure",
    );
}
