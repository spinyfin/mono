use super::support::{ExpectedCommand, FakeRunner, seed_mono_repo, with_database_path};
use clap::Parser;

use crate::cli::Cli;

use crate::app::dispatch::run_with_dependencies;
use crate::app::errors::CubeError;

#[test]
fn workspace_remove_deletes_synced_free_row() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // Sync the workspace into the registry by listing.
    // (sync runs as a side effect of operations like lease; here we
    // seed the row directly.)
    {
        use crate::metadata::WorkspaceCandidate;
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-007".to_string(),
                    workspace_path: workspace_path.clone(),
                }],
            )
            .unwrap();
    }

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "remove", "mono-agent-007"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("remove");

    assert!(result.message.contains("Removed mono/mono-agent-007"));
    assert_eq!(result.payload["forced"], false);
    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-007");

    // Row must be gone, but the on-disk directory must remain.
    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let remaining = store
        .list_workspaces_filtered(&WorkspaceListFilter {
            repo: Some("mono"),
            ..Default::default()
        })
        .unwrap();
    assert!(remaining.is_empty(), "expected row to be deleted");
    assert!(workspace_path.is_dir(), "directory must be left intact");
}

#[test]
fn workspace_remove_refuses_leased_row_without_force() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let workspace_path = workspace_root.join("mono-agent-001");
    let lease_runner = FakeRunner::new(vec![
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
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();

    let error = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "remove", "mono-agent-001"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect_err("remove should refuse a leased row");

    match error {
        CubeError::InvalidArgument(msg) => {
            assert!(msg.contains("currently leased"), "unexpected message: {msg}");
        }
        other => panic!("expected InvalidArgument, got {other:?}"),
    }

    // Row must still be present.
    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let remaining = store
        .list_workspaces_filtered(&WorkspaceListFilter {
            repo: Some("mono"),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(remaining.len(), 1);
}

#[test]
fn workspace_remove_force_removes_leased_row() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let workspace_path = workspace_root.join("mono-agent-001");
    let lease_runner = FakeRunner::new(vec![
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
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "remove", "mono-agent-001", "--force"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("force remove");

    assert_eq!(result.payload["forced"], true);
    assert_eq!(result.payload["workspace"]["state"], "leased");

    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let remaining = store
        .list_workspaces_filtered(&WorkspaceListFilter {
            repo: Some("mono"),
            ..Default::default()
        })
        .unwrap();
    assert!(remaining.is_empty(), "row should be deleted under --force");
}

#[test]
fn workspace_remove_succeeds_when_directory_is_gone() {
    // Canonical scenario: the operator wiped the workspace directory
    // by hand and `cube workspace list` still surfaces the row. Remove
    // must succeed without touching the missing path.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    {
        use crate::metadata::WorkspaceCandidate;
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-007".to_string(),
                    workspace_path: workspace_path.clone(),
                }],
            )
            .unwrap();
    }

    // Wipe the directory like the user did manually.
    std::fs::remove_dir_all(&workspace_path).unwrap();

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "remove", "mono-agent-007"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("remove dangling row");

    assert!(result.message.contains("mono-agent-007"));

    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let remaining = store
        .list_workspaces_filtered(&WorkspaceListFilter {
            repo: Some("mono"),
            ..Default::default()
        })
        .unwrap();
    assert!(remaining.is_empty());
}

#[test]
fn workspace_remove_errors_when_workspace_id_unknown() {
    let (_tempdir, database_path) = with_database_path();

    let error = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "remove", "mono-agent-999"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect_err("remove should fail for unknown workspace");

    assert!(matches!(error, CubeError::WorkspaceNotFound(_)));
}

#[test]
fn workspace_remove_emits_audit_entry() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    {
        use crate::metadata::WorkspaceCandidate;
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-007".to_string(),
                    workspace_path: workspace_path.clone(),
                }],
            )
            .unwrap();
    }

    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "remove", "mono-agent-007"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("remove");

    let audit_dir = tempdir.path().join("audit");
    let audit_files: Vec<_> = std::fs::read_dir(&audit_dir)
        .expect("audit dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(audit_files.len(), 1, "expected one weekly audit file");

    let contents = std::fs::read_to_string(&audit_files[0]).expect("audit content");
    let line = contents.lines().last().expect("at least one event");
    let event: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(event["event"], "workspace.removed");
    assert_eq!(event["repo"], "mono");
    assert_eq!(event["workspace_id"], "mono-agent-007");
    assert_eq!(event["prior_state"], "free");
    assert_eq!(event["forced"], false);
    assert_eq!(event["expunged"], false);
}

#[test]
fn workspace_remove_expunge_deletes_row_and_directory() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");
    std::fs::write(workspace_path.join("marker"), "x").expect("marker file");

    seed_mono_repo(&workspace_root, &database_path);

    {
        use crate::metadata::WorkspaceCandidate;
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-007".to_string(),
                    workspace_path: workspace_path.clone(),
                }],
            )
            .unwrap();
    }

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "remove", "mono-agent-007", "--expunge"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("expunge remove");

    assert_eq!(result.payload["expunged"], true);
    assert!(result.message.contains("deleted workspace directory"));
    assert!(!workspace_path.exists(), "expected on-disk directory to be removed");

    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let remaining = store
        .list_workspaces_filtered(&WorkspaceListFilter {
            repo: Some("mono"),
            ..Default::default()
        })
        .unwrap();
    assert!(remaining.is_empty(), "row should be deleted");
}

#[test]
fn workspace_remove_expunge_tolerates_missing_directory() {
    // The directory may already be gone (operator wiped it manually);
    // --expunge should still succeed and clean up the row.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    {
        use crate::metadata::WorkspaceCandidate;
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-007".to_string(),
                    workspace_path: workspace_path.clone(),
                }],
            )
            .unwrap();
    }

    std::fs::remove_dir_all(&workspace_path).unwrap();

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "remove", "mono-agent-007", "--expunge"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("expunge tolerates missing dir");

    assert_eq!(result.payload["expunged"], true);
    assert!(!workspace_path.exists());
}

#[test]
fn workspace_remove_without_expunge_leaves_directory_intact() {
    // Regression check on PR #291's default behaviour: omitting
    // --expunge must keep the on-disk workspace directory.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");
    std::fs::write(workspace_path.join("marker"), "x").expect("marker file");

    seed_mono_repo(&workspace_root, &database_path);

    {
        use crate::metadata::WorkspaceCandidate;
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-007".to_string(),
                    workspace_path: workspace_path.clone(),
                }],
            )
            .unwrap();
    }

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "remove", "mono-agent-007"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("remove");

    assert_eq!(result.payload["expunged"], false);
    assert!(
        workspace_path.is_dir(),
        "directory must remain when --expunge is not passed"
    );
    assert!(
        workspace_path.join("marker").is_file(),
        "directory contents must be preserved"
    );
}

#[test]
fn workspace_remove_expunge_makes_removal_durable_against_lease_resync() {
    // After --expunge, a follow-up lease's discover/sync round must
    // NOT resurrect the row (that was the gap that motivated the
    // flag).
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    {
        use crate::metadata::WorkspaceCandidate;
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-007".to_string(),
                    workspace_path: workspace_path.clone(),
                }],
            )
            .unwrap();
    }

    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "remove", "mono-agent-007", "--expunge"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("expunge remove");

    // A subsequent lease must not see the just-expunged workspace.
    // It will auto-create a fresh `mono-agent-001` instead via the
    // FakeRunner's `jj git clone` expectation. (The fake runner just
    // records the invocation; we manually create the resulting
    // directory.)
    let new_path = workspace_root.join("mono-agent-001");
    let staging = workspace_root.join(".incoming-mono-agent-001");
    let lease_runner = FakeRunner::new(vec![
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

    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "after-expunge"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease after expunge");

    assert_eq!(
        lease_result.payload["workspace"]["workspace_id"], "mono-agent-001",
        "lease should auto-create a fresh slot, not resurrect the expunged one"
    );

    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let rows = store
        .list_workspaces_filtered(&WorkspaceListFilter {
            repo: Some("mono"),
            ..Default::default()
        })
        .unwrap();
    let ids: Vec<_> = rows.iter().map(|r| r.workspace_id.as_str()).collect();
    assert!(
        !ids.contains(&"mono-agent-007"),
        "expunged workspace must not reappear; saw {ids:?}"
    );
}

#[test]
fn workspace_remove_without_expunge_resurrects_on_next_lease() {
    // Documents the without-expunge gap: PR #291 removed the row but
    // left the directory, so the next lease's discover/sync brings
    // the row back as state=Free. This test pins that behaviour;
    // operators who want durable removal must use --expunge.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    {
        use crate::metadata::WorkspaceCandidate;
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-007".to_string(),
                    workspace_path: workspace_path.clone(),
                }],
            )
            .unwrap();
    }

    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "remove", "mono-agent-007"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("remove");

    // Without --expunge the dir is still there, so the next lease
    // discovers it and re-syncs the row.
    let lease_runner = FakeRunner::new(vec![
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
    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "resync"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease re-syncs row");

    assert_eq!(
        lease_result.payload["workspace"]["workspace_id"], "mono-agent-007",
        "without --expunge the discovered directory resurrects the row"
    );
}
