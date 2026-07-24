use super::support::{
    ExpectedCommand, FakeRunner, gc_noop_command, gc_pr_remote_noop_command, head_status_command, head_status_output,
    jj_status_clean, jj_status_conflicted, lease_runner_for, release_guard_reusable_command, release_runner_for,
    seed_mono_repo, unpushed_probe_command, with_database_path,
};
use clap::Parser;

use crate::cli::Cli;
use crate::command_runner::{CommandInvocation, CommandRunner};
use crate::lock::RepoLock;
use crate::metadata::{WorkspaceHealth, WorkspaceRecord, WorkspaceState};
use crate::store::{Store, WorkspaceListFilter};

use crate::app::dispatch::run_with_dependencies;
use crate::app::errors::{CubeError, Result};
use crate::app::reset::PRESERVED_UNPUSHED_RELEASE_REASON;
use crate::app::util::repo_lock_path;

#[test]
fn workspace_release_clears_health_status() {
    // After a workspace is released, its health_status should be NULL
    // so it gets re-checked at next lease time.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let ws_path = workspace_root.join("mono-agent-003");
    std::fs::create_dir_all(ws_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let lease_runner = lease_runner_for(&ws_path, "abc1234");
    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "test"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();
    let lease_id = lease_result.payload["workspace"]["lease_id"]
        .as_str()
        .unwrap()
        .to_string();

    let release_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(ws_path.clone(), "jj", &["git", "fetch"], ""),
        release_guard_reusable_command(&ws_path),
        ExpectedCommand::ok(
            ws_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            ws_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(ws_path.clone(), "jj", &["new", "main@origin"], ""),
        gc_noop_command(&ws_path),
        gc_pr_remote_noop_command(&ws_path),
    ]);
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release");
    release_runner.assert_exhausted();

    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(ws.health_status, None, "health_status should be cleared on release");
}

#[test]
fn workspace_lease_release_list_workspace_list_shows_effective_state() {
    // `cube workspace list` output message should show `free-conflicted`
    // for a workspace whose health_status is `conflicted`.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let conflicted_path = workspace_root.join("mono-agent-003");
    let clean_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(conflicted_path.join(".jj")).expect("conflicted dir");
    std::fs::create_dir_all(clean_path.join(".jj")).expect("clean dir");

    seed_mono_repo(&workspace_root, &database_path);

    // Run a lease that skips the conflicted workspace and picks the clean one.
    let lease_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            conflicted_path.clone(),
            "jj",
            &["status", "--no-pager"],
            &jj_status_conflicted("fix-burst"),
        ),
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

    let list = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "list"]),
        Some(&database_path),
        &FakeRunner::default(),
    )
    .expect("list");

    // The human-readable message should contain "free-conflicted" for 003.
    assert!(
        list.message.contains("free-conflicted"),
        "expected free-conflicted in list message: {}",
        list.message
    );
}

#[test]
fn workspace_release_resets_and_frees_the_workspace() {
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
    let lease_result = run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
    let lease_id = lease_result.payload["workspace"]["lease_id"]
        .as_str()
        .expect("lease id")
        .to_string();
    lease_runner.assert_exhausted();

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
    let release = Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]);
    let release_result = run_with_dependencies(release, Some(&database_path), &release_runner).expect("release");

    assert_eq!(release_result.payload["workspace"]["state"], "free");
    assert_eq!(release_result.payload["workspace"]["lease_id"], serde_json::Value::Null);
    release_runner.assert_exhausted();
}

#[test]
fn lease_and_release_emit_audit_log_entries() {
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
    let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "audit smoke"]);
    let lease_result = run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
    let lease_id = lease_result.payload["workspace"]["lease_id"]
        .as_str()
        .expect("lease id")
        .to_string();
    lease_runner.assert_exhausted();

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
    let release = Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id, "--reason", "done"]);
    run_with_dependencies(release, Some(&database_path), &release_runner).expect("release");
    release_runner.assert_exhausted();

    let audit_dir = tempdir.path().join("audit");
    let audit_files: Vec<_> = std::fs::read_dir(&audit_dir)
        .expect("audit dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(audit_files.len(), 1, "expected one weekly audit file");

    let contents = std::fs::read_to_string(&audit_files[0]).expect("audit content");
    let events: Vec<serde_json::Value> = contents
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .collect();
    let by_event: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| {
            let name = e["event"].as_str().unwrap_or_default();
            name == "lease.acquired" || name == "lease.released"
        })
        .collect();
    assert_eq!(
        by_event.len(),
        2,
        "expected one lease.acquired + one lease.released event"
    );

    let acquired = by_event[0];
    assert_eq!(acquired["event"], "lease.acquired");
    assert_eq!(acquired["repo"], "mono");
    assert_eq!(acquired["workspace_id"], "mono-agent-004");
    assert_eq!(acquired["lease_id"], lease_id);
    assert_eq!(acquired["task"], "audit smoke");
    assert_eq!(acquired["head_commit"], "abc1234");
    assert!(acquired["holder"].is_string());
    assert!(acquired["ts"].as_str().unwrap().ends_with('Z'));

    let released = by_event[1];
    assert_eq!(released["event"], "lease.released");
    assert_eq!(released["lease_id"], lease_id);
    assert_eq!(released["reason"], "done");
    assert_eq!(released["keep_dirty"], false);

    // The instrumentation chore also requires that every `jj`
    // operation cube runs against a leased workspace is auditable.
    // Each reset emits a fetch + bookmark-set + new triple, and we
    // have a lease and a release: so six `workspace.jj_op` entries on
    // the timeline.
    let jj_ops: Vec<&serde_json::Value> = events.iter().filter(|e| e["event"] == "workspace.jj_op").collect();
    assert_eq!(
        jj_ops.len(),
        6,
        "expected 6 workspace.jj_op events (fetch+bookmark-set+new each for lease+release)"
    );
    let workspace_path_str = workspace_path.display().to_string();
    for op in &jj_ops {
        assert_eq!(op["workspace_path"], workspace_path_str);
    }
}

#[test]
fn workspace_release_by_workspace_id_resolves_active_lease() {
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
    let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]);
    run_with_dependencies(lease, Some(&database_path), &lease_runner).expect("lease");
    lease_runner.assert_exhausted();

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
    let release = Cli::parse_from(["cube", "workspace", "release", "mono-agent-004"]);
    let result = run_with_dependencies(release, Some(&database_path), &release_runner).expect("release by id");

    assert_eq!(result.payload["workspace"]["state"], "free");
    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-004");
    release_runner.assert_exhausted();
}

#[test]
fn workspace_release_by_workspace_id_errors_when_not_leased() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // sync_workspaces is normally called inside lease, so trigger it
    // via list with the registry knowing about this workspace.
    let list = Cli::parse_from(["cube", "workspace", "list", "--repo", "mono"]);
    let _ = run_with_dependencies(list, Some(&database_path), &FakeRunner::default());

    let release = Cli::parse_from(["cube", "workspace", "release", "mono-agent-004"]);
    let error =
        run_with_dependencies(release, Some(&database_path), &FakeRunner::default()).expect_err("release should fail");
    // Workspace id is unknown to the registry until something has synced
    // it, so this surfaces as WorkspaceNotFound.
    assert!(matches!(error, CubeError::WorkspaceNotFound(_)));
}

#[test]
fn workspace_release_keep_dirty_skips_reset_and_records_reason() {
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
    lease_runner.assert_exhausted();

    // No reset commands expected — --keep-dirty short-circuits the
    // jj git fetch / jj new main@origin pair.
    let release_runner = FakeRunner::default();
    let result = run_with_dependencies(
        Cli::parse_from([
            "cube",
            "workspace",
            "release",
            "--lease",
            &lease_id,
            "--reason",
            "crash",
            "--keep-dirty",
        ]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release");

    assert_eq!(result.payload["workspace"]["state"], "free");
    assert_eq!(result.payload["workspace"]["last_release_reason"], "crash");
    assert!(result.message.contains("kept dirty"));
    release_runner.assert_exhausted();
}

// ── release-time dirty preservation (P0) ───────────────────────────────
//
// A plain `cube workspace release --lease <id>` — the exact invocation
// the Boss engine issues from `cube_commands.rs` — must NOT destroy a
// working copy that holds work existing on no remote. Before this guard,
// every engine restart reset those trees and the in-flight work was gone.

/// Lease `mono-agent-001` and return its lease id, leaving the registry
/// in the "leased, workspace on disk" state the release tests need.
fn lease_agent_001_for_release_test(workspace_path: &std::path::Path, database_path: &std::path::Path) -> String {
    let lease_runner = lease_runner_for(workspace_path, "abc1234");
    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "demo"]),
        Some(database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();
    lease_result.payload["workspace"]["lease_id"]
        .as_str()
        .expect("lease id")
        .to_string()
}

/// Read back the single registry row for `mono-agent-001`.
fn agent_001_record(database_path: &std::path::Path) -> WorkspaceRecord {
    Store::open_at(database_path)
        .expect("store")
        .list_workspaces_filtered(&WorkspaceListFilter {
            repo: Some("mono"),
            workspace_id: Some("mono-agent-001"),
            ..Default::default()
        })
        .expect("list")
        .into_iter()
        .next()
        .expect("workspace row")
}

/// A bare release whose `@` is non-empty and on no remote keeps the
/// working copy: no `jj new`, workspace freed but flagged dirty, and the
/// preservation is reported on the payload and the message. This is the
/// P0 behaviour the engine depends on.
#[test]
fn workspace_release_preserves_working_copy_holding_unpushed_work() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");
    seed_mono_repo(&workspace_root, &database_path);
    let workspace_path = workspace_root.join("mono-agent-001");
    let lease_id = lease_agent_001_for_release_test(&workspace_path, &database_path);

    // fetch, then the two guard probes — and nothing else. The absence of
    // `jj new main@origin` from these expectations is the assertion that
    // matters: FakeRunner rejects any command it was not given.
    let release_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        head_status_command(&workspace_path, &head_status_output("wipchange", false, "", "", "")),
        unpushed_probe_command(&workspace_path, "wipchange\tdeadbeef\n"),
    ]);
    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release");
    release_runner.assert_exhausted();

    assert_eq!(result.payload["preserved_unpushed_work"], true);
    assert_eq!(result.payload["preserved_head_change_id"], "wipchange");
    assert_eq!(result.payload["preserved_unpushed_commits"], "wipchange:deadbeef");
    assert_eq!(result.payload["workspace"]["state"], "free");
    assert!(
        result.message.contains("working copy preserved"),
        "message should say the tree was kept: {}",
        result.message
    );

    // Freed but marked dirty, so the lease health check skips it and no
    // fresh worker lands on top of the preserved work.
    let record = agent_001_record(&database_path);
    assert_eq!(record.state, WorkspaceState::Free);
    assert_eq!(record.health_status, Some(WorkspaceHealth::Dirty));
    // With no caller-supplied --reason, the preservation itself is the
    // reason, so `cube workspace list` explains the dirty row.
    assert_eq!(
        record.last_release_reason.as_deref(),
        Some(PRESERVED_UNPUSHED_RELEASE_REASON)
    );
}

/// The steady state is untouched: a worker that pushed its work leaves an
/// empty `@` on main, the guard says "reusable", and the release resets
/// exactly as it always did.
#[test]
fn workspace_release_still_resets_when_working_copy_is_reusable() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");
    seed_mono_repo(&workspace_root, &database_path);
    let workspace_path = workspace_root.join("mono-agent-001");
    let lease_id = lease_agent_001_for_release_test(&workspace_path, &database_path);

    let release_runner = release_runner_for(&workspace_path);
    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release");
    release_runner.assert_exhausted();

    assert_eq!(result.payload["preserved_unpushed_work"], false);
    assert_eq!(result.payload["workspace"]["state"], "free");
    assert_eq!(
        agent_001_record(&database_path).health_status,
        None,
        "a reset workspace must not be flagged dirty"
    );
}

/// `--force-reset` is the deliberate opt-out: the guard probe is skipped
/// entirely and the destructive reset runs regardless of the tree.
#[test]
fn workspace_release_force_reset_overrides_the_preservation_guard() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");
    seed_mono_repo(&workspace_root, &database_path);
    let workspace_path = workspace_root.join("mono-agent-001");
    let lease_id = lease_agent_001_for_release_test(&workspace_path, &database_path);

    // No head-status probe at all: --force-reset skips it, so the
    // expectations are the plain fetch+reset sequence.
    let release_runner = FakeRunner::new(vec![
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
        gc_noop_command(&workspace_path),
        gc_pr_remote_noop_command(&workspace_path),
    ]);
    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id, "--force-reset"]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release");
    release_runner.assert_exhausted();
    assert_eq!(result.payload["preserved_unpushed_work"], false);
    assert!(result.message.starts_with("Released mono-agent-001."));
}

/// A caller-supplied `--reason` wins over the synthetic preservation
/// reason, so an operator's crash annotation is not overwritten.
#[test]
fn workspace_release_preservation_does_not_clobber_caller_reason() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");
    seed_mono_repo(&workspace_root, &database_path);
    let workspace_path = workspace_root.join("mono-agent-001");
    let lease_id = lease_agent_001_for_release_test(&workspace_path, &database_path);

    let release_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        head_status_command(&workspace_path, &head_status_output("wip2", false, "", "", "")),
        unpushed_probe_command(&workspace_path, "wip2\tcafe1234\n"),
    ]);
    let result = run_with_dependencies(
        Cli::parse_from([
            "cube",
            "workspace",
            "release",
            "--lease",
            &lease_id,
            "--reason",
            "engine-restart",
        ]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release");
    release_runner.assert_exhausted();
    assert_eq!(result.payload["preserved_unpushed_work"], true);
    assert_eq!(result.payload["workspace"]["last_release_reason"], "engine-restart");
}

/// `--keep-dirty` and `--force-reset` are mutually exclusive: they ask for
/// opposite things, and silently letting one win would be exactly the
/// class of ambiguity this change exists to remove.
#[test]
fn workspace_release_rejects_keep_dirty_with_force_reset() {
    let parsed = Cli::try_parse_from([
        "cube",
        "workspace",
        "release",
        "mono-agent-001",
        "--keep-dirty",
        "--force-reset",
    ]);
    assert!(parsed.is_err(), "--keep-dirty and --force-reset must conflict");
}

#[test]
fn workspace_force_release_skips_reset() {
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

    // Force-release runs no shell commands.
    let release_runner = FakeRunner::default();
    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "force-release", "--lease", &lease_id]),
        Some(&database_path),
        &release_runner,
    )
    .expect("force-release");

    assert_eq!(result.payload["workspace"]["state"], "free");
    assert_eq!(result.payload["workspace"]["last_release_reason"], "force-released");
    release_runner.assert_exhausted();
}

#[test]
fn workspace_release_gc_forgets_consumed_bookmarks() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "gc test"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();
    let lease_id = lease_result.payload["workspace"]["lease_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Release runner returns a consumed bookmark from the gc log query.
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
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &[
                "log",
                "-r",
                "bookmarks(glob:\"boss/exec_*\") & ::main",
                "--no-graph",
                "-T",
                "bookmarks ++ \"\\n\"",
            ],
            "boss/exec_18abcd_01",
        ),
        gc_pr_remote_noop_command(&workspace_path),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["bookmark", "forget", "boss/exec_18abcd_01"],
            "",
        ),
    ]);
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release");
    release_runner.assert_exhausted();
}

/// Shared state for [`BlockingFetchRunner`]: the fetch signals `entered`
/// when it starts and then blocks until the test sets `released`. A single
/// mutex backs both flags (a Condvar may only ever pair with one mutex).
#[derive(Default)]
struct GateState {
    entered: bool,
    released: bool,
}

struct FetchGate {
    state: std::sync::Mutex<GateState>,
    cv: std::sync::Condvar,
}

/// A `CommandRunner` whose `jj git fetch` blocks until the test releases
/// it, so we can inspect cube's lock state *while* a network op is in
/// flight. All other commands return canned output; `jj git remote list`
/// returns a local (non-github) mirror so the reset and gc sweeps complete
/// without reaching out to `gh`.
struct BlockingFetchRunner {
    gate: std::sync::Arc<FetchGate>,
}

impl CommandRunner for BlockingFetchRunner {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        let args: Vec<&str> = invocation.args.iter().map(String::as_str).collect();
        if invocation.program == "jj" && args.first() == Some(&"git") && args.get(1) == Some(&"fetch") {
            {
                let mut state = self.gate.state.lock().unwrap();
                state.entered = true;
                self.gate.cv.notify_all();
            }
            let mut state = self.gate.state.lock().unwrap();
            while !state.released {
                state = self.gate.cv.wait(state).unwrap();
            }
            return Ok(String::new());
        }
        if invocation.program == "jj" && args == ["git", "remote", "list"] {
            // Local mirror form: keeps the reset's upstream detection and
            // the gc pr-sweep from making any github/gh network calls.
            return Ok("origin /local/path/to/mirror\n".to_string());
        }
        Ok(String::new())
    }
}

/// Root-cause regression test: a `cube workspace release` whose `jj git
/// fetch` is wedged must NOT be holding the per-repo lock. If it were, this
/// repo would be unable to lease/release any other workspace — the exact
/// pool-wide wedge this fix removes.
#[test]
fn release_does_not_hold_repo_lock_across_stalled_fetch() {
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");
    seed_mono_repo(&workspace_root, &database_path);

    // Lease the workspace normally so there is a live lease to release.
    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "lock probe"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();
    let lease_id = lease_result.payload["workspace"]["lease_id"]
        .as_str()
        .unwrap()
        .to_string();

    let gate = Arc::new(FetchGate {
        state: std::sync::Mutex::new(GateState::default()),
        cv: std::sync::Condvar::new(),
    });
    let runner = BlockingFetchRunner {
        gate: Arc::clone(&gate),
    };
    let lock_path = repo_lock_path("mono", Some(&database_path)).expect("lock path");

    std::thread::scope(|scope| {
        // Run the release on a worker thread; its fetch will block.
        let release_handle = scope.spawn(|| {
            run_with_dependencies(
                Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
                Some(&database_path),
                &runner,
            )
        });

        // Wait until the release is parked inside `jj git fetch`.
        {
            let mut state = gate.state.lock().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !state.entered {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(remaining > Duration::ZERO, "release never reached the fetch");
                let (guard, _timeout) = gate.cv.wait_timeout(state, remaining).unwrap();
                state = guard;
            }
        }

        // While the fetch is wedged, the per-repo lock must be free. Acquire
        // it on a helper thread and wait for it with a timeout so a
        // regression (lock held across the fetch) is detected without
        // hanging the test.
        let (tx, rx) = mpsc::channel();
        let probe_path = lock_path.clone();
        scope.spawn(move || {
            // Blocks here under a regression until the release drops the
            // lock; sends once acquired so the test can measure latency.
            let acquired = RepoLock::acquire(&probe_path);
            let _ = tx.send(());
            drop(acquired);
        });
        let lock_was_free = rx.recv_timeout(Duration::from_secs(5)).is_ok();

        // Always unblock the fetch and join, so the test exits cleanly even
        // on the regression path, then assert.
        {
            let mut state = gate.state.lock().unwrap();
            state.released = true;
            gate.cv.notify_all();
        }
        let result = release_handle.join().expect("release thread").expect("release ok");
        assert_eq!(result.payload["workspace"]["state"], "free");
        assert!(
            lock_was_free,
            "per-repo lock was held during release's fetch — the network op is still \
                 inside the critical section (regression)"
        );
    });
}

/// Graceful degradation: if the release reset's fetch fails outright, the
/// lease is still released (the worker is never stranded) and the freed
/// workspace is marked dirty so the next lease re-resets it.
#[test]
fn release_degrades_to_dirty_when_reset_fetch_fails() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");
    seed_mono_repo(&workspace_root, &database_path);

    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "degrade test"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();
    let lease_id = lease_result.payload["workspace"]["lease_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Release runner: the very first command (the reset fetch) fails with a
    // non-transient error, so the reset aborts before any further command.
    let release_runner = FakeRunner::new(vec![ExpectedCommand {
        cwd: workspace_path.clone(),
        program: "jj".to_string(),
        args: vec!["git".to_string(), "fetch".to_string()],
        result: Err(CubeError::CommandFailed {
            program: "jj".to_string(),
            args: vec!["git".to_string(), "fetch".to_string()],
            status: Some(1),
            stderr: "fatal: permission denied (publickey)".to_string(),
        }),
        creates_dir: None,
    }]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release should succeed even when reset fails");
    release_runner.assert_exhausted();

    // Lease succeeded as a release; workspace is free again.
    assert_eq!(result.payload["workspace"]["state"], "free");
    assert_eq!(result.payload["reset_failed"], serde_json::Value::Bool(true));

    // And it is flagged dirty so the next lease won't hand out an un-reset tree.
    let store = crate::store::Store::open_at(&database_path).expect("store");
    let records = store
        .list_workspaces_filtered(&crate::store::WorkspaceListFilter {
            repo: Some("mono"),
            workspace_id: Some("mono-agent-001"),
            ..Default::default()
        })
        .expect("list");
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].health_status,
        Some(crate::metadata::WorkspaceHealth::Dirty),
        "reset failure should mark the freed workspace dirty"
    );
}
