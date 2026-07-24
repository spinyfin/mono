use super::support::{
    ExpectedCommand, FakeRunner, audit_events, force_lease_expiry, jj_status_clean, jj_status_dirty, lease_runner_for,
    seed_mono_repo, with_database_path,
};
use clap::Parser;
use serde_json::json;

use crate::cli::Cli;

use crate::app::dispatch::run_with_dependencies;
use crate::app::util::current_epoch_s;

#[test]
fn workspace_reconcile_promotes_stale_dirty_to_clean() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let ws_path = workspace_root.join("mono-agent-008");
    std::fs::create_dir_all(ws_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    {
        use crate::metadata::{WorkspaceCandidate, WorkspaceHealth};
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-008".to_string(),
                    workspace_path: ws_path.clone(),
                }],
            )
            .unwrap();
        store
            .update_workspace_health("mono", "mono-agent-008", WorkspaceHealth::Dirty)
            .unwrap();
    }

    let runner = FakeRunner::new(vec![ExpectedCommand::ok(
        ws_path.clone(),
        "jj",
        &["status", "--no-pager"],
        jj_status_clean(),
    )]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "reconcile"]),
        Some(&database_path),
        &runner,
    )
    .expect("reconcile");
    runner.assert_exhausted();

    assert_eq!(result.payload["promoted_to_clean"].as_array().unwrap().len(), 1);
    assert_eq!(result.payload["promoted_to_clean"][0]["workspace_id"], "mono-agent-008");
    assert_eq!(result.payload["still_unhealthy"].as_array().unwrap().len(), 0);

    // DB must reflect the promoted health.
    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(ws.health_status, Some(crate::metadata::WorkspaceHealth::Clean));
    assert!(
        ws.unhealthy_since_epoch_s.is_none(),
        "unhealthy_since must be cleared after promotion"
    );
}

#[test]
fn workspace_reconcile_still_unhealthy_when_dirty_on_disk() {
    // `cube workspace reconcile` on a workspace that is STILL dirty on disk
    // must report it as `still_unhealthy` and NOT update the DB to clean.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let ws_path = workspace_root.join("mono-agent-008");
    std::fs::create_dir_all(ws_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    {
        use crate::metadata::{WorkspaceCandidate, WorkspaceHealth};
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-008".to_string(),
                    workspace_path: ws_path.clone(),
                }],
            )
            .unwrap();
        store
            .update_workspace_health("mono", "mono-agent-008", WorkspaceHealth::Dirty)
            .unwrap();
    }

    let runner = FakeRunner::new(vec![ExpectedCommand::ok(
        ws_path.clone(),
        "jj",
        &["status", "--no-pager"],
        jj_status_dirty(),
    )]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "reconcile"]),
        Some(&database_path),
        &runner,
    )
    .expect("reconcile");
    runner.assert_exhausted();

    assert_eq!(result.payload["promoted_to_clean"].as_array().unwrap().len(), 0);
    assert_eq!(result.payload["still_unhealthy"].as_array().unwrap().len(), 1);
    assert_eq!(result.payload["still_unhealthy"][0]["workspace_id"], "mono-agent-008");
    assert_eq!(result.payload["still_unhealthy"][0]["new_health"], "dirty");

    // DB must still show dirty.
    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(ws.health_status, Some(crate::metadata::WorkspaceHealth::Dirty));
}

#[test]
fn workspace_reconcile_dry_run_does_not_update_db() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let ws_path = workspace_root.join("mono-agent-008");
    std::fs::create_dir_all(ws_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    {
        use crate::metadata::{WorkspaceCandidate, WorkspaceHealth};
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-008".to_string(),
                    workspace_path: ws_path.clone(),
                }],
            )
            .unwrap();
        store
            .update_workspace_health("mono", "mono-agent-008", WorkspaceHealth::Dirty)
            .unwrap();
    }

    let runner = FakeRunner::new(vec![ExpectedCommand::ok(
        ws_path.clone(),
        "jj",
        &["status", "--no-pager"],
        jj_status_clean(),
    )]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "reconcile", "--dry-run"]),
        Some(&database_path),
        &runner,
    )
    .expect("reconcile dry-run");
    runner.assert_exhausted();

    assert_eq!(result.payload["dry_run"], true);
    assert_eq!(result.payload["promoted_to_clean"].as_array().unwrap().len(), 1);

    // DB must NOT have been updated — health stays dirty.
    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(
        ws.health_status,
        Some(crate::metadata::WorkspaceHealth::Dirty),
        "dry-run must not modify the DB"
    );
}

#[test]
fn workspace_list_reconciles_free_row_whose_directory_is_missing() {
    // Canonical scenario from the chore: an operator wiped the
    // workspace directory by hand and the row remained in cube's
    // registry. `cube workspace list` must notice and self-heal
    // rather than handing out the stale row to the next caller.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // Seed a free row, then yank the directory out from under cube.
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
    std::fs::remove_dir_all(&workspace_path).expect("wipe workspace dir");

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "list"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("list");

    assert_eq!(
        result.payload["reconciled"]["removed"][0]["workspace_id"],
        "mono-agent-007"
    );
    assert_eq!(result.payload["reconciled"]["removed"][0]["prior_state"], "free");
    assert_eq!(result.payload["reconciled"]["held"], json!([]));
    assert_eq!(result.payload["workspaces"], json!([]));

    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let remaining = store.list_workspaces_filtered(&WorkspaceListFilter::default()).unwrap();
    assert!(remaining.is_empty(), "row must be deleted by reconcile");

    let events = audit_events(&tempdir);
    let reconciled: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.dir_missing_reconciled")
        .collect();
    assert_eq!(reconciled.len(), 1);
    assert_eq!(reconciled[0]["repo"], "mono");
    assert_eq!(reconciled[0]["workspace_id"], "mono-agent-007");
    assert_eq!(reconciled[0]["prior_state"], "free");
}

#[test]
fn workspace_list_reconciles_leased_row_with_expired_lease() {
    // A worker leased a workspace, then was rm-rf'd along with its
    // directory and never released. The lease has aged past its TTL,
    // so reconcile is allowed to force-release and forget the row.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    let lease_id = lease_result.payload["workspace"]["lease_id"]
        .as_str()
        .expect("lease id")
        .to_string();
    lease_runner.assert_exhausted();

    // Age the lease into the past, then wipe the directory.
    force_lease_expiry(&database_path, &lease_id, 1);
    std::fs::remove_dir_all(&workspace_path).expect("wipe workspace dir");

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "list"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("list");

    assert_eq!(
        result.payload["reconciled"]["removed"][0]["workspace_id"],
        "mono-agent-001"
    );
    assert_eq!(result.payload["reconciled"]["removed"][0]["prior_state"], "leased");
    assert_eq!(result.payload["reconciled"]["held"], json!([]));

    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let remaining = store.list_workspaces_filtered(&WorkspaceListFilter::default()).unwrap();
    assert!(
        remaining.is_empty(),
        "expired+missing row must be force-released and deleted"
    );

    let events = audit_events(&tempdir);
    let reconciled: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.dir_missing_reconciled")
        .collect();
    assert_eq!(reconciled.len(), 1);
    assert_eq!(reconciled[0]["prior_state"], "leased");
    assert_eq!(reconciled[0]["lease_id"], lease_id);
}

#[test]
fn workspace_list_holds_leased_row_when_lease_still_active() {
    // The lease is still within its TTL, so we can't know whether
    // the holder is mid-setup or genuinely dead. Defer to the
    // operator: warn + audit but leave the row untouched.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    let lease_id = lease_result.payload["workspace"]["lease_id"]
        .as_str()
        .expect("lease id")
        .to_string();
    lease_runner.assert_exhausted();

    // Push the expiry far into the future so reconcile sees it as
    // active even after we wipe the directory.
    let far_future = current_epoch_s().expect("now") + 86_400;
    force_lease_expiry(&database_path, &lease_id, far_future);
    std::fs::remove_dir_all(&workspace_path).expect("wipe workspace dir");

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "list"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("list");

    assert_eq!(result.payload["reconciled"]["removed"], json!([]));
    assert_eq!(
        result.payload["reconciled"]["held"][0]["workspace_id"],
        "mono-agent-001"
    );
    assert_eq!(result.payload["reconciled"]["held"][0]["prior_state"], "leased");

    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let remaining = store.list_workspaces_filtered(&WorkspaceListFilter::default()).unwrap();
    assert_eq!(remaining.len(), 1, "active-lease+missing row must be left in place");
    assert_eq!(remaining[0].state, crate::metadata::WorkspaceState::Leased);

    let events = audit_events(&tempdir);
    let held: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.dir_missing_held")
        .collect();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0]["lease_id"], lease_id);
    assert_eq!(held[0]["lease_expires_at_epoch_s"], far_future);
}

#[test]
fn workspace_list_reconcile_is_noop_when_directories_exist() {
    // When nothing has drifted, reconcile must not emit any audit
    // events or surface any reconciled/held rows.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
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
                    workspace_id: "mono-agent-001".to_string(),
                    workspace_path: workspace_path.clone(),
                }],
            )
            .unwrap();
    }

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "list"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("list");

    assert_eq!(result.payload["reconciled"]["removed"], json!([]));
    assert_eq!(result.payload["reconciled"]["held"], json!([]));
    assert!(audit_events(&tempdir).is_empty());
}

#[test]
fn workspace_list_reconciler_respects_repo_filter() {
    // With --repo set, only that repo's drifted rows should be
    // reconciled. Other repos' dangling rows must be left alone so a
    // narrow query doesn't quietly mutate state across the registry.
    let (tempdir, database_path) = with_database_path();
    let workspace_root_a = tempdir.path().join("repos-a/workspaces");
    let workspace_root_b = tempdir.path().join("repos-b/workspaces");
    std::fs::create_dir_all(workspace_root_a.join("mono-agent-001").join(".jj")).expect("workspace dir a");
    std::fs::create_dir_all(workspace_root_b.join("other-agent-001").join(".jj")).expect("workspace dir b");

    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "mono".to_string(),
                origin: "git@github.com:spinyfin/mono.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: workspace_root_a.clone(),
                workspace_prefix: "mono-agent-".to_string(),
                source: None,
                clone_command: None,
            })
            .expect("seed repo a");
        store
            .upsert_repo(&RepoRecord {
                repo: "other".to_string(),
                origin: "git@github.com:spinyfin/other.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: workspace_root_b.clone(),
                workspace_prefix: "other-agent-".to_string(),
                source: None,
                clone_command: None,
            })
            .expect("seed repo b");
    }

    // Seed both repos with one free row each, then wipe both dirs.
    {
        use crate::metadata::WorkspaceCandidate;
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-001".to_string(),
                    workspace_path: workspace_root_a.join("mono-agent-001"),
                }],
            )
            .unwrap();
        store
            .sync_workspaces(
                "other",
                &[WorkspaceCandidate {
                    workspace_id: "other-agent-001".to_string(),
                    workspace_path: workspace_root_b.join("other-agent-001"),
                }],
            )
            .unwrap();
    }
    std::fs::remove_dir_all(workspace_root_a.join("mono-agent-001")).expect("wipe a");
    std::fs::remove_dir_all(workspace_root_b.join("other-agent-001")).expect("wipe b");

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "list", "--repo", "mono"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("list");

    // Only the `mono` row should appear in the reconcile report.
    let removed = result.payload["reconciled"]["removed"].as_array().unwrap();
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0]["repo"], "mono");

    // The `other` repo's dangling row must still be there.
    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let other = store
        .list_workspaces_filtered(&WorkspaceListFilter {
            repo: Some("other"),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(other.len(), 1);
    assert_eq!(other[0].workspace_id, "other-agent-001");
}

#[test]
fn workspace_lease_reconciles_expired_missing_row_before_claiming() {
    // A previously leased workspace's directory was wiped while the
    // lease aged out. Lease must reconcile the dangling row before
    // claiming so it doesn't hand out the stale slot.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    let first = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("first lease");
    let lease_id = first.payload["workspace"]["lease_id"]
        .as_str()
        .expect("lease id")
        .to_string();
    lease_runner.assert_exhausted();

    force_lease_expiry(&database_path, &lease_id, 1);
    std::fs::remove_dir_all(&workspace_path).expect("wipe workspace dir");

    // The next lease should reconcile the phantom row, then auto-create
    // a fresh workspace via `jj git clone --colocate`. The runner needs
    // the clone command plus the standard reset/log triple for the
    // newly-created workspace. After reconcile deletes mono-agent-001,
    // `next_workspace_id` reuses the freed slot rather than skipping
    // ahead to mono-agent-002.
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
            "def5678",
        ),
    ]);

    let second = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "fresh"]),
        Some(&database_path),
        &runner,
    )
    .expect("second lease");
    runner.assert_exhausted();

    assert_eq!(second.payload["workspace"]["workspace_id"], "mono-agent-001");

    // Only the freshly-claimed (re-provisioned) row remains; the
    // phantom row was forgotten before the new clone created the
    // replacement.
    use crate::store::{Store, WorkspaceListFilter};
    let store = Store::open_at(&database_path).unwrap();
    let rows = store.list_workspaces_filtered(&WorkspaceListFilter::default()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].workspace_id, "mono-agent-001");
    assert_eq!(rows[0].state, crate::metadata::WorkspaceState::Leased);

    let events = audit_events(&tempdir);
    let reconciled: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.dir_missing_reconciled" && e["workspace_id"] == "mono-agent-001")
        .collect();
    assert_eq!(reconciled.len(), 1);
}
