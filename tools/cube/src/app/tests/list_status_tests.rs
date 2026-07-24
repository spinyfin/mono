use super::support::{
    ExpectedCommand, FakeRunner, jj_status_clean, jj_status_dirty, seed_mono_repo, with_database_path,
};
use clap::Parser;

use crate::cli::Cli;

use crate::app::dispatch::run_with_dependencies;
use crate::app::errors::CubeError;
use crate::app::util::current_epoch_s;

#[test]
fn workspace_list_shows_health_status_in_effective_state() {
    // After a lease attempt that skips dirty workspaces, workspace list
    // should show `free-dirty` for the skipped ones.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let dirty_path = workspace_root.join("mono-agent-003");
    let clean_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(dirty_path.join(".jj")).expect("dirty dir");
    std::fs::create_dir_all(clean_path.join(".jj")).expect("clean dir");

    seed_mono_repo(&workspace_root, &database_path);

    // Trigger a lease so health checks run and health_status is persisted.
    let lease_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(dirty_path.clone(), "jj", &["status", "--no-pager"], jj_status_dirty()),
        ExpectedCommand::ok(clean_path.clone(), "jj", &["status", "--no-pager"], jj_status_clean()),
        ExpectedCommand::ok(clean_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            clean_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            clean_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(clean_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            clean_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "test"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();

    // Now list workspaces and check the JSON output.
    let list_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "list", "--json"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("list");

    let workspaces = list_result.payload["workspaces"].as_array().expect("workspaces array");
    // 003 is free-dirty, 007 is leased
    let ws_003 = workspaces
        .iter()
        .find(|w| w["workspace_id"] == "mono-agent-003")
        .expect("003");
    let ws_007 = workspaces
        .iter()
        .find(|w| w["workspace_id"] == "mono-agent-007")
        .expect("007");
    assert_eq!(ws_003["health_status"], "dirty");
    assert_eq!(ws_003["state"], "free");
    assert_eq!(ws_007["state"], "leased");
}

#[test]
fn workspace_list_state_filter_accepts_free_dirty() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let dirty_path = workspace_root.join("mono-agent-003");
    let clean_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(dirty_path.join(".jj")).expect("dirty dir");
    std::fs::create_dir_all(clean_path.join(".jj")).expect("clean dir");

    seed_mono_repo(&workspace_root, &database_path);

    // Trigger a lease to run health checks and persist health_status.
    let lease_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(dirty_path.clone(), "jj", &["status", "--no-pager"], jj_status_dirty()),
        ExpectedCommand::ok(clean_path.clone(), "jj", &["status", "--no-pager"], jj_status_clean()),
        ExpectedCommand::ok(clean_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            clean_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            clean_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(clean_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            clean_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "test"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");

    // --state free-dirty should return only mono-agent-003
    let dirty_list = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "list", "--state", "free-dirty"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("list dirty");

    let workspaces = dirty_list.payload["workspaces"].as_array().expect("workspaces");
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0]["workspace_id"], "mono-agent-003");

    // --state free should return zero (003 is free-dirty, 007 is leased)
    let free_list = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "list", "--state", "free"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("list free");
    assert_eq!(
        free_list.payload["workspaces"].as_array().unwrap().len(),
        0,
        "no purely-free workspaces should remain after leasing the only clean one"
    );
}

#[test]
fn workspace_heartbeat_extends_expiry() {
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
    let before_expiry = lease_result.payload["workspace"]["lease_expires_at_epoch_s"]
        .as_i64()
        .expect("initial expiry");

    // Sleep a touch so wall-clock current_epoch_s advances; the
    // heartbeat handler uses it as the new base.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    // Heartbeat with the default TTL — since current_epoch_s
    // moved forward by >1s since the lease, the new expiry must be
    // strictly greater than the initial one.
    let beat_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "heartbeat", "--lease", &lease_id]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("heartbeat");

    let after_expiry = beat_result.payload["workspace"]["lease_expires_at_epoch_s"]
        .as_i64()
        .expect("new expiry");
    assert!(
        after_expiry > before_expiry,
        "heartbeat should advance expiry: before={before_expiry}, after={after_expiry}"
    );

    // Also confirm a custom shorter TTL is honored exactly.
    let custom = run_with_dependencies(
        Cli::parse_from([
            "cube",
            "workspace",
            "heartbeat",
            "--lease",
            &lease_id,
            "--ttl-seconds",
            "60",
        ]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("heartbeat custom");
    let custom_expiry = custom.payload["workspace"]["lease_expires_at_epoch_s"]
        .as_i64()
        .expect("custom expiry");
    let now_after = current_epoch_s().expect("now");
    // expiry should be ~60s after the call; allow some slack for slow runners.
    let delta = custom_expiry - now_after;
    assert!(
        (delta - 60).abs() <= 5,
        "custom expiry {custom_expiry} should be ~now+60={}, delta {delta}s",
        now_after + 60
    );
}

#[test]
fn workspace_status_includes_jj_status_output() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let workspace_path = workspace_root.join("mono-agent-004");
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
    let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "implement cube"]);
    run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
    lease_runner.assert_exhausted();

    let status_runner = FakeRunner::new(vec![ExpectedCommand::ok(
        workspace_path.clone(),
        "jj",
        &["status"],
        "The working copy is clean",
    )]);
    let status = Cli::parse_from([
        "cube",
        "workspace",
        "status",
        "--workspace",
        &workspace_path.display().to_string(),
    ]);
    let status_result = run_with_dependencies(status, Some(&database_path), &status_runner).expect("status");

    assert_eq!(status_result.payload["jj_status"], "The working copy is clean");
    assert!(status_result.message.contains("jj_status:"));
    status_runner.assert_exhausted();
}

#[test]
fn workspace_status_forgets_missing_workspace_rows() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let workspace_path = workspace_root.join("mono-agent-004");
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
    let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "implement cube"]);
    run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
    lease_runner.assert_exhausted();

    std::fs::remove_dir_all(&workspace_path).expect("remove workspace dir");

    let status = Cli::parse_from([
        "cube",
        "workspace",
        "status",
        "--workspace",
        &workspace_path.display().to_string(),
    ]);
    let error = run_with_dependencies(status, Some(&database_path), &FakeRunner::default())
        .expect_err("status should forget missing workspace");

    assert!(matches!(error, CubeError::WorkspaceNotFound(_)));
}

#[test]
fn workspace_list_returns_filtered_rows() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");
    std::fs::create_dir_all(workspace_root.join("mono-agent-002").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let first_path = workspace_root.join("mono-agent-001");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            first_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(first_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            first_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            first_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(first_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            first_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);
    let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]);
    run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

    // global list returns both rows
    let list_all = Cli::parse_from(["cube", "workspace", "list"]);
    let result_all = run_with_dependencies(list_all, Some(&database_path), &FakeRunner::default()).expect("list");
    let rows = result_all.payload["workspaces"].as_array().expect("array");
    assert_eq!(rows.len(), 2);

    // state filter narrows to leased only
    let list_leased = Cli::parse_from(["cube", "workspace", "list", "--state", "leased"]);
    let result_leased =
        run_with_dependencies(list_leased, Some(&database_path), &FakeRunner::default()).expect("list leased");
    let leased = result_leased.payload["workspaces"].as_array().expect("array");
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0]["workspace_id"], "mono-agent-001");
    assert_eq!(leased[0]["state"], "leased");
    assert_eq!(leased[0]["task"], "demo");

    // invalid state returns argument error
    let list_bad = Cli::parse_from(["cube", "workspace", "list", "--state", "bogus"]);
    let error =
        run_with_dependencies(list_bad, Some(&database_path), &FakeRunner::default()).expect_err("invalid state");
    assert!(matches!(error, CubeError::InvalidArgument(_)));
}
