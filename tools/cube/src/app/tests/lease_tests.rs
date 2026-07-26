use super::support::{
    ExpectedCommand, FakeRunner, head_status_command, head_status_output, jj_status_clean, jj_status_conflicted,
    jj_status_dirty, lease_runner_for, mono_source_path, seed_mono_repo, unpushed_probe_command, with_database_path,
};
use clap::Parser;
use tempfile::TempDir;

use crate::cli::Cli;
use crate::command_runner::CommandRunner;
use crate::store::Store;

use crate::app::dispatch::{run_with_context, run_with_dependencies};
use crate::app::errors::CubeError;
use crate::app::repo::RepoEnsureDefaults;

#[test]
fn workspace_lease_claims_first_free_workspace_and_records_head_commit() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");
    std::fs::create_dir_all(workspace_root.join("mono-agent-005").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let first_path = workspace_root.join("mono-agent-004");
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

    let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "implement cube"]);
    let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-004");
    assert_eq!(
        result.payload["workspace"]["workspace_path"],
        first_path.display().to_string()
    );
    assert_eq!(result.payload["workspace"]["head_commit"], "abc1234");
    runner.assert_exhausted();
}

/// The on-lease fast-forward (`jj bookmark set <main> -r <main>@origin`)
/// must run between `jj git fetch` and `jj new <main>@origin` to keep
/// the local bookmark current for workers that run `jj new main` themselves
/// (spinyfin/mono#1232).
#[test]
fn workspace_lease_fast_forwards_default_branch_to_origin() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let first_path = workspace_root.join("mono-agent-004");
    // lease_runner_for already encodes the fetch → bookmark-set → new
    // ordering; assert_exhausted fails if the fast-forward step is
    // skipped or reordered.
    let runner = lease_runner_for(&first_path, "abc1234");
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "ff"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease");
    runner.assert_exhausted();
}

/// When `main@origin` cannot be resolved (the recorded default branch has
/// no matching remote-tracking bookmark after fetch), the lease must fail
/// hard rather than silently branching from a stale local bookmark.
/// The fast-forward warns and continues, but `jj new main@origin` then
/// fails with the same "revision doesn't exist" error, surfacing the
/// misconfiguration instead of producing a stale-base branch.
#[test]
fn workspace_lease_fails_when_origin_default_branch_unresolvable() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let first_path = workspace_root.join("mono-agent-004");
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
        // fast-forward warns and continues when main@origin doesn't resolve
        ExpectedCommand::revision_doesnt_exist(
            first_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
        ),
        // jj new main@origin also fails — surfaces the misconfiguration
        ExpectedCommand::revision_doesnt_exist(first_path.clone(), "jj", &["new", "main@origin"]),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "fail-unresolvable"]),
        Some(&database_path),
        &runner,
    );
    assert!(
        result.is_err(),
        "lease must fail when main@origin cannot be resolved after fetch"
    );
    runner.assert_exhausted();
}

/// New-task `@` is positioned on `main@origin` (the remote HEAD as of
/// the fetch), not the local `main` bookmark which may lag behind.
/// Regression guard for the incident where PR #1568 was cut from a
/// 3-commit-stale base because `jj new main` used the local bookmark
/// rather than the freshly-fetched remote HEAD.
#[test]
fn workspace_lease_positions_new_task_on_origin_head_not_stale_local_main() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let first_path = workspace_root.join("mono-agent-004");
    // Simulate: local `main` is stale; `main@origin` is ahead.
    // The fast-forward succeeds (advancing local `main` to `main@origin`),
    // then `jj new main@origin` positions directly on the remote head.
    // If the code were to use `jj new main` instead, the FakeRunner's
    // `assert_exhausted` would fail, pinning the regression.
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
        // fast-forward advances stale local `main` to `main@origin`
        ExpectedCommand::ok(
            first_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        // crucial: `jj new main@origin` — NOT `jj new main`
        ExpectedCommand::ok(first_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            first_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "deadbeef",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "new-task"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease must succeed");
    assert_eq!(result.payload["workspace"]["head_commit"], "deadbeef");
    // assert_exhausted proves `jj new main@origin` was called (not `jj new main`).
    runner.assert_exhausted();
}

#[test]
fn workspace_lease_auto_creates_when_pool_is_empty() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    // intentionally no workspace dirs created up front

    seed_mono_repo(&workspace_root, &database_path);

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

    let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "auto-create demo"]);
    let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-001");
    assert_eq!(result.payload["workspace"]["state"], "leased");
    assert_eq!(result.payload["workspace"]["task"], "auto-create demo");
    assert_eq!(result.payload["workspace"]["head_commit"], "abc1234");
    runner.assert_exhausted();
}

/// Auto-create for a repo whose default branch is `master`: the new
/// shared-store workspace is attached via `jj workspace add` (no per-
/// workspace clone or bookmark tracking), and the reset fast-forwards and
/// branches through `master`/`master@origin`, proving the non-`main`
/// default flows through provisioning + reset correctly.
#[test]
fn workspace_lease_auto_creates_master_default_repo() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    // Canonical shared store for the `legacy` repo (what `repo ensure`
    // would have materialised), with its `.jj/` store present.
    let source = workspace_root.parent().unwrap().join("source").join("legacy");
    std::fs::create_dir_all(source.join(".jj")).expect("seed canonical source .jj");

    {
        use crate::metadata::RepoRecord;
        use crate::store::Store;
        let store = Store::open_at(&database_path).expect("store");
        store
            .upsert_repo(&RepoRecord {
                repo: "legacy".to_string(),
                origin: "git@github.com:spinyfin/legacy.git".to_string(),
                main_branch: "master".to_string(),
                workspace_root: workspace_root.clone(),
                workspace_prefix: "legacy-agent-".to_string(),
                source: Some(source.clone()),
                clone_command: None,
            })
            .expect("seed repo");
    }

    let new_path = workspace_root.join("legacy-agent-001");
    let staging = workspace_root.join(".incoming-legacy-agent-001");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::workspace_add(workspace_root.clone(), &source, "legacy-agent-001", &staging),
        ExpectedCommand::ok(new_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/legacy.git\n",
        ),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["bookmark", "set", "master", "-r", "master@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(new_path.clone(), "jj", &["new", "master@origin"], ""),
        ExpectedCommand::ok(
            new_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "fee1dead",
        ),
    ]);

    let lease = Cli::parse_from([
        "cube",
        "workspace",
        "lease",
        "legacy",
        "--task",
        "master-default auto-create",
    ]);
    let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

    assert_eq!(result.payload["workspace"]["workspace_id"], "legacy-agent-001");
    assert_eq!(result.payload["workspace"]["state"], "leased");
    runner.assert_exhausted();
}

/// A create interrupted after `jj workspace add` registered the workspace
/// in the SHARED canonical store but before the publish rename leaves a
/// leftover `.incoming-<id>` dir. The next lease must forget the dangling
/// registration (best-effort) and clear the dir before re-attaching, rather
/// than colliding with jj's "workspace already exists".
#[test]
fn workspace_lease_auto_create_recovers_from_interrupted_staging() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    seed_mono_repo(&workspace_root, &database_path);
    let source = mono_source_path(&workspace_root);

    let new_path = workspace_root.join("mono-agent-001");
    let staging = workspace_root.join(".incoming-mono-agent-001");
    // Leftover from the interrupted prior create.
    std::fs::create_dir_all(staging.join(".jj")).expect("leftover staging");

    let runner = FakeRunner::new(vec![
        // The dangling store registration is forgotten first (here it
        // exists and the forget succeeds; a missing one is tolerated).
        ExpectedCommand::ok(
            workspace_root.clone(),
            "jj",
            &[
                "-R",
                &source.display().to_string(),
                "workspace",
                "forget",
                "mono-agent-001",
            ],
            "",
        ),
        ExpectedCommand::workspace_add(workspace_root.clone(), &source, "mono-agent-001", &staging),
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
            "cafef00d",
        ),
    ]);

    let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "recover staging"]);
    let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-001");
    assert_eq!(result.payload["workspace"]["state"], "leased");
    runner.assert_exhausted();
}

/// In the shared-store model the canonical repo already carries the local
/// `main`/`master` bookmarks, so auto-create must NOT re-track any bookmark
/// per workspace — it only attaches and resets. The FakeRunner's strict
/// call sequence enforces it: a stray `bookmark track …` between the
/// `workspace add` and the reset would crash with "unexpected command". If
/// cube ever regresses to per-workspace tracking, this test fails.
#[test]
fn workspace_lease_auto_create_does_not_track_bookmarks_in_shared_store() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");

    seed_mono_repo(&workspace_root, &database_path);

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
            "ba5eba11",
        ),
    ]);

    let lease = Cli::parse_from([
        "cube",
        "workspace",
        "lease",
        "mono",
        "--task",
        "no per-workspace bookmark tracking",
    ]);
    run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");
    runner.assert_exhausted();
}

#[test]
fn workspace_lease_auto_creates_next_id_after_existing() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("workspace dir");
    std::fs::create_dir_all(workspace_root.join("mono-agent-007").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // Lease both existing workspaces first so the pool is exhausted
    for (path, task) in [
        (workspace_root.join("mono-agent-001"), "first"),
        (workspace_root.join("mono-agent-007"), "second"),
    ] {
        let runner = FakeRunner::new(vec![
            ExpectedCommand::ok(
                path.clone(),
                "jj",
                &["status", "--no-pager"],
                "The working copy is clean",
            ),
            ExpectedCommand::ok(path.clone(), "jj", &["git", "fetch"], ""),
            ExpectedCommand::ok(
                path.clone(),
                "jj",
                &["git", "remote", "list"],
                "origin\tgit@github.com:spinyfin/mono.git\n",
            ),
            ExpectedCommand::ok(
                path.clone(),
                "jj",
                &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
                "",
            ),
            ExpectedCommand::ok(path.clone(), "jj", &["new", "main@origin"], ""),
            ExpectedCommand::ok(
                path.clone(),
                "jj",
                &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
                "deadbee",
            ),
        ]);
        let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", task]);
        run_with_dependencies(lease, Some(&database_path), &runner).expect("seed lease");
    }

    // Pool now exhausted; next lease should clone mono-agent-008 (max+1)
    let new_path = workspace_root.join("mono-agent-008");
    let staging = workspace_root.join(".incoming-mono-agent-008");
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
    let lease = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "third"]);
    let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-008");
    runner.assert_exhausted();
}

#[test]
fn next_workspace_id_picks_max_plus_one() {
    assert_eq!(
        crate::app::provision::next_workspace_id("mono-agent-", &[]),
        "mono-agent-001"
    );
    assert_eq!(
        crate::app::provision::next_workspace_id(
            "mono-agent-",
            &["mono-agent-001".to_string(), "mono-agent-002".to_string(),],
        ),
        "mono-agent-003"
    );
    // Non-contiguous: jumps to max+1, doesn't fill the gap.
    assert_eq!(
        crate::app::provision::next_workspace_id(
            "mono-agent-",
            &["mono-agent-001".to_string(), "mono-agent-007".to_string(),],
        ),
        "mono-agent-008"
    );
    // Mixed-prefix or non-numeric IDs are ignored.
    assert_eq!(
        crate::app::provision::next_workspace_id(
            "mono-agent-",
            &[
                "flunge-agent-099".to_string(),
                "mono-agent-abc".to_string(),
                "mono-agent-002".to_string(),
            ],
        ),
        "mono-agent-003"
    );
}

#[test]
fn workspace_lease_with_prefer_claims_named_workspace_when_free() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");
    std::fs::create_dir_all(workspace_root.join("mono-agent-005").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let preferred_path = workspace_root.join("mono-agent-005");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            preferred_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(preferred_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            preferred_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            preferred_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(preferred_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            preferred_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "def5678",
        ),
    ]);

    let lease = Cli::parse_from([
        "cube",
        "workspace",
        "lease",
        "mono",
        "--task",
        "resume cube prefer work",
        "--prefer",
        "mono-agent-005",
    ]);
    let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-005");
    assert_eq!(
        result.payload["workspace"]["workspace_path"],
        preferred_path.display().to_string()
    );
    runner.assert_exhausted();
}

#[test]
fn workspace_lease_with_prefer_falls_back_when_preferred_is_leased() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");
    std::fs::create_dir_all(workspace_root.join("mono-agent-005").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // First lease takes mono-agent-005 (the preferred one).
    let preferred_path = workspace_root.join("mono-agent-005");
    let first_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            preferred_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(preferred_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            preferred_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            preferred_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(preferred_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            preferred_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "first123",
        ),
    ]);
    let first_lease = Cli::parse_from([
        "cube",
        "workspace",
        "lease",
        "mono",
        "--task",
        "first task",
        "--prefer",
        "mono-agent-005",
    ]);
    run_with_dependencies(first_lease, Some(&database_path), &first_runner).expect("first lease");
    first_runner.assert_exhausted();

    // Second lease prefers mono-agent-005 (leased), should fall back to mono-agent-004.
    let fallback_path = workspace_root.join("mono-agent-004");
    let second_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            fallback_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(fallback_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            fallback_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            fallback_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(fallback_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            fallback_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "second456",
        ),
    ]);
    let second_lease = Cli::parse_from([
        "cube",
        "workspace",
        "lease",
        "mono",
        "--task",
        "second task",
        "--prefer",
        "mono-agent-005",
    ]);
    let result = run_with_dependencies(second_lease, Some(&database_path), &second_runner).expect("second lease");

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-004");
    second_runner.assert_exhausted();
}

#[test]
fn workspace_lease_with_unknown_prefer_falls_back_to_first_free() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");
    std::fs::create_dir_all(workspace_root.join("mono-agent-005").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let first_path = workspace_root.join("mono-agent-004");
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

    let lease = Cli::parse_from([
        "cube",
        "workspace",
        "lease",
        "mono",
        "--task",
        "fallback path",
        "--prefer",
        "mono-agent-999",
    ]);
    let result = run_with_dependencies(lease, Some(&database_path), &runner).expect("lease");

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-004");
    runner.assert_exhausted();
}

// ── Health-check tests ───────────────────────────────────────────────

#[test]
fn workspace_lease_clean_pool_returns_lowest_workspace() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-003").join(".jj")).expect("workspace dir");
    std::fs::create_dir_all(workspace_root.join("mono-agent-007").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let first = workspace_root.join("mono-agent-003");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(first.clone(), "jj", &["status", "--no-pager"], jj_status_clean()),
        ExpectedCommand::ok(first.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            first.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            first.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(first.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            first.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "clean pool"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease");
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-003");
    // health_check array should be present
    let hc = result.payload["health_check"].as_array().expect("health_check");
    assert_eq!(hc.len(), 1);
    assert_eq!(hc[0]["workspace_id"], "mono-agent-003");
    assert_eq!(hc[0]["health"], "clean");
    assert_eq!(hc[0]["skipped"], false);
}

#[test]
fn workspace_lease_skips_dirty_picks_clean() {
    // Pool: dirty(003), clean(007) → should skip 003, lease 007.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let dirty_path = workspace_root.join("mono-agent-003");
    let clean_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(dirty_path.join(".jj")).expect("dirty dir");
    std::fs::create_dir_all(clean_path.join(".jj")).expect("clean dir");

    seed_mono_repo(&workspace_root, &database_path);

    let runner = FakeRunner::new(vec![
        // health-check 003 → dirty → skip
        ExpectedCommand::ok(dirty_path.clone(), "jj", &["status", "--no-pager"], jj_status_dirty()),
        // health-check 007 → clean → use
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

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "skip dirty"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease");
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-007");
    let hc = result.payload["health_check"].as_array().expect("health_check");
    assert_eq!(hc.len(), 2);
    assert_eq!(hc[0]["workspace_id"], "mono-agent-003");
    assert_eq!(hc[0]["health"], "dirty");
    assert_eq!(hc[0]["skipped"], true);
    assert_eq!(hc[1]["workspace_id"], "mono-agent-007");
    assert_eq!(hc[1]["health"], "clean");
    assert_eq!(hc[1]["skipped"], false);

    // mono-agent-003 must be marked dirty in the store
    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    let ws = store.get_workspace_by_path(&dirty_path).unwrap().unwrap();
    assert_eq!(ws.health_status, Some(crate::metadata::WorkspaceHealth::Dirty));
}

#[test]
fn workspace_lease_promotes_stale_dirty_db_entry_to_clean_when_recovered() {
    // Regression test for stale `free-dirty` DB entries hiding recovered workspaces.
    // Setup: mono-agent-008 is marked `health_status=dirty` in the DB (mimicking
    // a workspace that was left dirty by a crashed worker and then manually reset),
    // but `jj status` now reports a clean working copy. The lease path must:
    //   - re-check mono-agent-008 via jj status
    //   - find it clean
    //   - update the DB health to 'clean'
    //   - claim and use it (not auto-create a new workspace)
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let ws_path = workspace_root.join("mono-agent-008");
    std::fs::create_dir_all(ws_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // Seed the workspace row and mark it dirty in the DB, simulating a
    // workspace that was cleaned on disk but whose DB cache is stale.
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

    // The stale-dirty workspace is now clean on disk.
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(ws_path.clone(), "jj", &["status", "--no-pager"], jj_status_clean()),
        ExpectedCommand::ok(ws_path.clone(), "jj", &["git", "fetch"], ""),
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
        ExpectedCommand::ok(
            ws_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "recovered1",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "recover stale dirty"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease");
    runner.assert_exhausted();

    // The stale-dirty workspace must have been claimed — no new workspace created.
    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-008");
    assert_eq!(result.payload["workspace"]["head_commit"], "recovered1");

    // Health check entry must reflect that this was a stale-dirty workspace
    // that got promoted to clean.
    let hc = result.payload["health_check"].as_array().expect("health_check");
    assert_eq!(hc.len(), 1);
    assert_eq!(hc[0]["workspace_id"], "mono-agent-008");
    assert_eq!(hc[0]["health"], "clean");
    assert_eq!(hc[0]["was_stale_dirty"], true);

    // The DB must now record the workspace as clean (health cleared).
    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    // The workspace is now leased; claim clears health_status to NULL.
    assert_eq!(ws.state, crate::metadata::WorkspaceState::Leased);
    assert!(ws.health_status.is_none(), "health_status should be NULL after claim");
    assert!(
        ws.unhealthy_since_epoch_s.is_none(),
        "unhealthy_since should be cleared"
    );
}

#[test]
fn workspace_lease_stale_dirty_workspace_checked_last_after_effective_free() {
    // Ordering invariant: stale-dirty workspaces (DB says dirty) are checked
    // AFTER effective-free (null/clean health) ones so we don't pay the `jj
    // status` cost on a stale-dirty slot when a clean slot is already there.
    //
    // Pool:
    //   mono-agent-005: effective-free (null health), jj status → dirty (truly dirty)
    //   mono-agent-007: stale-dirty in DB, jj status → clean (recovered!)
    //
    // Expected traversal: check 005 first (effective-free), find dirty, then
    // check 007 (stale-dirty), find clean, update DB and lease it.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let eff_free_path = workspace_root.join("mono-agent-005");
    let stale_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(eff_free_path.join(".jj")).expect("eff-free dir");
    std::fs::create_dir_all(stale_path.join(".jj")).expect("stale-dirty dir");

    seed_mono_repo(&workspace_root, &database_path);

    {
        use crate::metadata::{WorkspaceCandidate, WorkspaceHealth};
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[
                    WorkspaceCandidate {
                        workspace_id: "mono-agent-005".to_string(),
                        workspace_path: eff_free_path.clone(),
                    },
                    WorkspaceCandidate {
                        workspace_id: "mono-agent-007".to_string(),
                        workspace_path: stale_path.clone(),
                    },
                ],
            )
            .unwrap();
        // Mark 007 as dirty in the DB (stale entry — it's actually clean on disk).
        store
            .update_workspace_health("mono", "mono-agent-007", WorkspaceHealth::Dirty)
            .unwrap();
    }

    let runner = FakeRunner::new(vec![
        // 1. effective-free 005 checked first → truly dirty → skip
        ExpectedCommand::ok(
            eff_free_path.clone(),
            "jj",
            &["status", "--no-pager"],
            jj_status_dirty(),
        ),
        // 2. stale-dirty 007 checked second → clean on disk → promote and use
        ExpectedCommand::ok(stale_path.clone(), "jj", &["status", "--no-pager"], jj_status_clean()),
        ExpectedCommand::ok(stale_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            stale_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            stale_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(stale_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            stale_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "stale007",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "ordering test"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease");
    runner.assert_exhausted();

    // The stale-dirty workspace was promoted and claimed.
    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-007");
    let hc = result.payload["health_check"].as_array().expect("health_check");
    assert_eq!(hc.len(), 2);
    assert_eq!(hc[0]["workspace_id"], "mono-agent-005");
    assert_eq!(hc[0]["health"], "dirty");
    assert_eq!(hc[1]["workspace_id"], "mono-agent-007");
    assert_eq!(hc[1]["health"], "clean");
    assert_eq!(hc[1]["was_stale_dirty"], true);
}

#[test]
fn workspace_lease_allow_dirty_reclaims_named_workspace_without_reset() {
    // --allow-dirty must claim the preferred workspace as-is and run
    // NO health check, NO `jj git fetch`, and NO `jj new main` — the
    // dirty tree is handed to the new lease-holder intact. The only jj
    // calls are the head-commit read and the read-only recovery probe.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");
    let dirty_path = workspace_root.join("mono-agent-005");
    std::fs::create_dir_all(dirty_path.join(".jj")).expect("dirty dir");

    seed_mono_repo(&workspace_root, &database_path);

    // Seed the registry rows and mark mono-agent-005 dirty, mimicking a
    // crashed worker whose unpushed work was left behind. The normal
    // lease path would skip this workspace; --allow-dirty must not.
    {
        use crate::metadata::{WorkspaceCandidate, WorkspaceHealth};
        use crate::store::Store;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[
                    WorkspaceCandidate {
                        workspace_id: "mono-agent-004".to_string(),
                        workspace_path: workspace_root.join("mono-agent-004"),
                    },
                    WorkspaceCandidate {
                        workspace_id: "mono-agent-005".to_string(),
                        workspace_path: dirty_path.clone(),
                    },
                ],
            )
            .unwrap();
        store
            .update_workspace_health("mono", "mono-agent-005", WorkspaceHealth::Dirty)
            .unwrap();
    }

    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            dirty_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "dead789",
        ),
        // The recovery probe: `@` is non-empty and on no bookmark, so the
        // unpushed probe runs and confirms there IS work to recover.
        head_status_command(&dirty_path, &head_status_output("strandedwip", false, "", "", "")),
        unpushed_probe_command(&dirty_path, "strandedwip\tfeed0001\n"),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "recover stranded work",
            "--prefer",
            "mono-agent-005",
            "--allow-dirty",
        ]),
        Some(&database_path),
        &runner,
    )
    .expect("lease");
    // assert_exhausted proves no fetch/new-main/status ran — reset skipped.
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-005");
    assert_eq!(
        result.payload["workspace"]["workspace_path"],
        dirty_path.display().to_string()
    );
    assert_eq!(result.payload["workspace"]["head_commit"], "dead789");
    let hc = result.payload["health_check"].as_array().expect("health_check");
    assert_eq!(hc.len(), 1);
    assert_eq!(hc[0]["allow_dirty"], true);
    assert_eq!(hc[0]["reset_skipped"], true);
    // P1: cube reports that it really did hand back unrecovered work.
    assert_eq!(hc[0]["dirty_verified"], true);
    assert_eq!(hc[0]["dirty_head_change_id"], "strandedwip");
    assert_eq!(hc[0]["dirty_unpushed_commits"], "strandedwip:feed0001");

    // The row is now leased to this holder.
    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    let ws = store.get_workspace_by_path(&dirty_path).unwrap().unwrap();
    assert_eq!(ws.state, crate::metadata::WorkspaceState::Leased);
}

/// P1: `--allow-dirty` succeeding is NOT proof that anything was
/// recovered. A workspace that was already reset back to an empty `@` on
/// main is handed over just as happily — and cube must say so, because a
/// caller that assumes recovery from lease success starts from an empty
/// tree and silently loses the work it was trying to save.
#[test]
fn workspace_lease_allow_dirty_reports_dirty_verified_false_when_nothing_to_recover() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let ws_path = workspace_root.join("mono-agent-005");
    std::fs::create_dir_all(ws_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);
    {
        use crate::metadata::WorkspaceCandidate;
        let mut store = Store::open_at(&database_path).unwrap();
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-005".to_string(),
                    workspace_path: ws_path.clone(),
                }],
            )
            .unwrap();
    }

    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            ws_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "beef0001",
        ),
        // Empty `@` on main: the fast path settles it, no unpushed probe.
        head_status_command(&ws_path, &head_status_output("cleanhead", true, "main", "main", "main")),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "recover stranded work",
            "--prefer",
            "mono-agent-005",
            "--allow-dirty",
        ]),
        Some(&database_path),
        &runner,
    )
    .expect("lease");
    runner.assert_exhausted();

    // The lease still succeeds — cube's job is to hand over the named
    // workspace, not to decide the caller's recovery strategy — but the
    // report is unambiguous that nothing was recovered in place.
    let hc = result.payload["health_check"].as_array().expect("health_check");
    assert_eq!(hc[0]["allow_dirty"], true);
    assert_eq!(hc[0]["dirty_verified"], false);
}

#[test]
fn workspace_lease_allow_dirty_errors_when_preferred_missing() {
    // --allow-dirty must never silently fall back to a fresh
    // workspace: if the named workspace is unknown, fail loudly so the
    // recovering worker is not routed away from the dirty tree.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-004").join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let runner = FakeRunner::new(vec![]);
    let err = run_with_dependencies(
        Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "recover stranded work",
            "--prefer",
            "mono-agent-999",
            "--allow-dirty",
        ]),
        Some(&database_path),
        &runner,
    )
    .expect_err("expected lease to fail for unknown preferred workspace");
    runner.assert_exhausted();
    assert!(matches!(err, CubeError::WorkspaceNotFound(_)));
}

#[test]
fn workspace_lease_allow_dirty_errors_when_preferred_leased() {
    // A live lease on the preferred workspace must block dirty reclaim
    // rather than stomping the active holder's working copy.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let busy_path = workspace_root.join("mono-agent-004");
    std::fs::create_dir_all(busy_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // First lease takes mono-agent-004.
    let first_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(busy_path.clone(), "jj", &["status", "--no-pager"], jj_status_clean()),
        ExpectedCommand::ok(busy_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            busy_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            busy_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(busy_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            busy_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "live0001",
        ),
    ]);
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "live work"]),
        Some(&database_path),
        &first_runner,
    )
    .expect("first lease");
    first_runner.assert_exhausted();

    let runner = FakeRunner::new(vec![]);
    let err = run_with_dependencies(
        Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "recover stranded work",
            "--prefer",
            "mono-agent-004",
            "--allow-dirty",
        ]),
        Some(&database_path),
        &runner,
    )
    .expect_err("expected lease to fail for leased preferred workspace");
    runner.assert_exhausted();
    assert!(matches!(err, CubeError::InvalidArgument(_)));
}

#[test]
fn workspace_lease_one_clean_n_conflicted_uses_clean() {
    // Pool: conflicted(003), clean(007) → should skip conflicted, use clean.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let conflicted_path = workspace_root.join("mono-agent-003");
    let clean_path = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(conflicted_path.join(".jj")).expect("conflicted dir");
    std::fs::create_dir_all(clean_path.join(".jj")).expect("clean dir");

    seed_mono_repo(&workspace_root, &database_path);

    let runner = FakeRunner::new(vec![
        // health-check 003 → conflicted (save as fallback, keep looking)
        ExpectedCommand::ok(
            conflicted_path.clone(),
            "jj",
            &["status", "--no-pager"],
            &jj_status_conflicted("fix-burst"),
        ),
        // health-check 007 → clean → use
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

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "prefer clean"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease");
    runner.assert_exhausted();

    // 007 was used; 003 (conflicted) was not repaired because 007 was clean.
    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-007");
    let hc = result.payload["health_check"].as_array().expect("health_check");
    assert_eq!(hc.len(), 2);
    assert_eq!(hc[0]["workspace_id"], "mono-agent-003");
    assert_eq!(hc[0]["health"], "conflicted");
    assert_eq!(hc[1]["workspace_id"], "mono-agent-007");
    assert_eq!(hc[1]["health"], "clean");
}

#[test]
fn workspace_lease_all_conflicted_repairs_lowest_and_returns_it() {
    // Pool: conflicted(003), conflicted(007) → repair 003 (lowest) and use it.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let path_003 = workspace_root.join("mono-agent-003");
    let path_007 = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(path_003.join(".jj")).expect("003 dir");
    std::fs::create_dir_all(path_007.join(".jj")).expect("007 dir");

    seed_mono_repo(&workspace_root, &database_path);

    let runner = FakeRunner::new(vec![
        // health-check 003 → conflicted (save as first fallback)
        ExpectedCommand::ok(
            path_003.clone(),
            "jj",
            &["status", "--no-pager"],
            &jj_status_conflicted("fix-burst"),
        ),
        // health-check 007 → conflicted (already have a fallback, don't replace)
        ExpectedCommand::ok(
            path_007.clone(),
            "jj",
            &["status", "--no-pager"],
            &jj_status_conflicted("fix-burst"),
        ),
        // repair 003: forget the conflicted bookmark
        ExpectedCommand::ok(path_003.clone(), "jj", &["bookmark", "forget", "fix-burst"], ""),
        // reset 003
        ExpectedCommand::ok(path_003.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            path_003.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            path_003.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(path_003.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            path_003.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "abc1234",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "all conflicted"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease");
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-003");
    let hc = result.payload["health_check"].as_array().expect("health_check");
    // Both conflicted workspaces appear in health_check
    assert_eq!(hc.len(), 2);
    assert_eq!(hc[0]["workspace_id"], "mono-agent-003");
    assert_eq!(hc[0]["health"], "conflicted");
    // 003 was chosen (not skipped), 007 was skipped (already have a candidate)
    assert_eq!(hc[0]["skipped"], false);
    assert_eq!(hc[1]["workspace_id"], "mono-agent-007");
    assert_eq!(hc[1]["skipped"], true);
}

#[test]
fn workspace_lease_all_dirty_auto_creates_fresh_workspace() {
    // Pool: dirty(003), dirty(007) → no reusable slot → auto-create a new
    // workspace instead of blocking. The dirty entries must be preserved.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let path_003 = workspace_root.join("mono-agent-003");
    let path_007 = workspace_root.join("mono-agent-007");
    std::fs::create_dir_all(path_003.join(".jj")).expect("003 dir");
    std::fs::create_dir_all(path_007.join(".jj")).expect("007 dir");

    seed_mono_repo(&workspace_root, &database_path);

    // After health-checking 003 and 007 as dirty, the lease path falls
    // through to auto_create_workspace which clones a new workspace.
    let new_path = workspace_root.join("mono-agent-008");
    let staging = workspace_root.join(".incoming-mono-agent-008");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(path_003.clone(), "jj", &["status", "--no-pager"], jj_status_dirty()),
        ExpectedCommand::ok(path_007.clone(), "jj", &["status", "--no-pager"], jj_status_dirty()),
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
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "all dirty"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease should succeed via auto-create when all existing workspaces are dirty");
    runner.assert_exhausted();

    // The leased workspace is the newly created one.
    assert_eq!(
        result.payload["workspace"]["workspace_id"], "mono-agent-008",
        "expected newly created workspace"
    );
    assert_eq!(result.payload["workspace"]["state"], "leased");

    // Both dirty workspaces are still in the store with their health marked.
    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    for path in [&path_003, &path_007] {
        let ws = store.get_workspace_by_path(path).unwrap().unwrap();
        assert_eq!(
            ws.health_status,
            Some(crate::metadata::WorkspaceHealth::Dirty),
            "dirty workspace should be preserved at {}",
            path.display()
        );
    }
}

/// Part-1 integration test — the test gap that let the bad clone-based
/// provisioning ship (PR #126 passed because the FakeRunner only *simulated*
/// `jj git clone`). This drives the REAL `auto_create_workspace` against a
/// real throwaway colocated jj repo with a real `jj`/`git`, and asserts the
/// new workspace SHARES the canonical object store rather than being an
/// independent clone. A FakeRunner can prove only that cube *issued* a
/// command; only a real `jj workspace add` proves the store is shared.
#[test]
fn auto_create_workspace_attaches_real_shared_store() {
    use crate::command_runner::RealCommandRunner;

    // Requires real jj + git; skip in sandboxes that lack them rather than
    // failing (mirrors the other real-subprocess tests in this crate).
    if which::which("jj").is_err() || which::which("git").is_err() {
        eprintln!("skipping auto_create_workspace_attaches_real_shared_store: jj or git not on PATH");
        return;
    }

    let tempdir = TempDir::new().unwrap();
    let canonical = tempdir.path().join("canonical");
    std::fs::create_dir_all(&canonical).unwrap();

    // Build a real colocated canonical repo with a `main` branch and some
    // history — what `materialize_repo_source_if_missing` produces at
    // `repo ensure` time.
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&canonical)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main", "."]);
    git(&["config", "user.email", "cube-test@example.com"]);
    git(&["config", "user.name", "cube-test"]);
    std::fs::write(canonical.join("README.md"), "hello\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "initial"]);

    let runner = RealCommandRunner;
    // Colocate jj over the git repo; this imports `main` as a local bookmark,
    // exactly like cube's canonical-repo materialize.
    runner
        .run(&RealCommandRunner::invocation(
            &canonical,
            "jj",
            &["git", "init", "--colocate"],
        ))
        .expect("jj git init --colocate on canonical");

    let workspace_root = tempdir.path().join("workspaces");
    let repo_record = crate::metadata::RepoRecord {
        repo: "mono".to_string(),
        origin: "git@github.com:spinyfin/mono.git".to_string(),
        main_branch: "main".to_string(),
        workspace_root: workspace_root.clone(),
        workspace_prefix: "mono-agent-".to_string(),
        source: Some(canonical.clone()),
        clone_command: None,
    };

    let candidate = crate::app::provision::auto_create_workspace(&runner, &repo_record, &[]).expect("auto-create");
    assert_eq!(candidate.workspace_id, "mono-agent-001");
    let ws = candidate.workspace_path.clone();

    // 1. `.jj/repo` is a FILE pointer into the canonical store, not its own
    //    directory — this is what makes it a shared-store attachment rather
    //    than an independent clone (the whole point of the fix).
    let repo_marker = ws.join(".jj").join("repo");
    assert!(
        repo_marker.is_file(),
        ".jj/repo must be a file pointer for a shared-store workspace; a directory means an independent clone"
    );
    let target = std::fs::read_to_string(&repo_marker).unwrap();
    assert!(
        target.contains("canonical"),
        "the .jj/repo pointer must reference the canonical store, got: {target}"
    );

    // 2. No independent `.git` of its own (non-colocated secondary workspace).
    assert!(
        !ws.join(".git").exists(),
        "a shared-store workspace must not carry its own .git"
    );

    // 3. The canonical repo lists the new workspace by name.
    let list = runner
        .run(&RealCommandRunner::invocation(&canonical, "jj", &["workspace", "list"]))
        .expect("jj workspace list");
    assert!(
        list.contains("mono-agent-001"),
        "canonical `jj workspace list` must include the attached workspace: {list}"
    );

    // 4. Disk footprint is working-copy-sized, not a full history copy: the
    //    workspace's own `.jj` is materially smaller than the canonical store.
    fn dir_size(p: &std::path::Path) -> u64 {
        let mut total = 0;
        if let Ok(rd) = std::fs::read_dir(p) {
            for entry in rd.flatten() {
                let Ok(md) = entry.metadata() else { continue };
                if md.is_dir() {
                    total += dir_size(&entry.path());
                } else {
                    total += md.len();
                }
            }
        }
        total
    }
    let ws_jj = dir_size(&ws.join(".jj"));
    let canon_jj = dir_size(&canonical.join(".jj"));
    assert!(
        ws_jj < canon_jj,
        "workspace .jj ({ws_jj} bytes) must be smaller than the shared canonical store ({canon_jj} bytes); a full clone would be comparable"
    );

    // 5. The shared store is usable from the workspace: the canonical
    //    `main` history resolves there (proves the attach, not just files).
    let log = runner
        .run(&RealCommandRunner::invocation(
            &ws,
            "jj",
            &["log", "--no-graph", "-r", "main", "-T", "description.first_line()"],
        ))
        .expect("jj log -r main in workspace");
    assert!(
        log.contains("initial"),
        "workspace must see the canonical history via the shared store: {log}"
    );
}

/// When leasing an existing workspace whose repo has a local source mirror,
/// the reset must use `main@github` (the real upstream remote) for both
/// the fast-forward AND the `jj new` positioning, not the stale local
/// `main@origin` mirror.
#[test]
fn workspace_lease_fast_forwards_using_github_remote_when_source_exists() {
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-004");
    let source_dir = tempdir.path().join("source").join("mono");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");
    std::fs::create_dir_all(&source_dir).expect("source dir");

    let ensure = Cli::parse_from(["cube", "repo", "ensure", "--origin", "git@github.com:spinyfin/mono.git"]);
    let ensure_defaults = RepoEnsureDefaults {
        repo_root: source_dir.parent().unwrap().to_path_buf(),
        workspace_root: workspace_root.clone(),
    };
    run_with_context(
        ensure,
        Some(&database_path),
        &FakeRunner::default(),
        Some(&ensure_defaults),
        None,
    )
    .expect("repo");

    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
        // detect_upstream_tracking_remote() returns the github remote
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\t/local/mirror\ngithub\tgit@github.com:spinyfin/mono.git\n",
        ),
        // fast-forward against github, not the stale origin
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@github", "--allow-backwards"],
            "",
        ),
        // `jj new` also targets the real upstream (main@github), not main@origin
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["new", "main@github"], ""),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "cafe5678",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "github-ff"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease must use github remote for fast-forward");
    assert_eq!(result.payload["workspace"]["head_commit"], "cafe5678");
    runner.assert_exhausted();
}

// ── Bounded health scan ─────────────────────────────────────────────────────
// Regression cover for the dispatch outage: a free pool that had gone
// all-dirty made every lease serially re-probe every free workspace, ~17s of
// `jj status` calls re-deriving a verdict already committed to SQLite, before
// provisioning a fresh workspace anyway. The lease then blew past the
// caller's 30s bound. These tests pin the three bounds that replaced it.

/// Build a pool of `count` workspaces whose cached health in the registry is
/// already `dirty`, and return their ids in the order the scan considers them.
fn seed_cached_dirty_pool(
    workspace_root: &std::path::Path,
    database_path: &std::path::Path,
    count: usize,
) -> Vec<String> {
    use crate::metadata::{WorkspaceCandidate, WorkspaceHealth};

    let ids: Vec<String> = (1..=count).map(|n| format!("mono-agent-{n:03}")).collect();
    let candidates: Vec<WorkspaceCandidate> = ids
        .iter()
        .map(|id| {
            let path = workspace_root.join(id);
            std::fs::create_dir_all(path.join(".jj")).expect("workspace dir");
            WorkspaceCandidate {
                workspace_id: id.clone(),
                workspace_path: path,
            }
        })
        .collect();

    let mut store = Store::open_at(database_path).unwrap();
    store.sync_workspaces("mono", &candidates).unwrap();
    for id in &ids {
        store
            .update_workspace_health("mono", id, WorkspaceHealth::Dirty)
            .unwrap();
    }
    ids
}

/// The `jj` sequence a freshly auto-created workspace goes through: create,
/// reset, read head.
fn auto_create_commands(workspace_root: &std::path::Path, new_id: &str, head: &str) -> Vec<ExpectedCommand> {
    let new_path = workspace_root.join(new_id);
    let staging = workspace_root.join(format!(".incoming-{new_id}"));
    vec![
        ExpectedCommand::workspace_add_mono(workspace_root, &staging),
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
            head,
        ),
    ]
}

#[test]
fn workspace_lease_trusts_cached_dirty_health_instead_of_reprobing_the_pool() {
    // Ten workspaces, all recorded `dirty` in the registry. The old scan ran
    // `jj status` against all ten (and would have against a hundred); the
    // bounded one probes only this lease's revalidation window and provisions.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(&workspace_root).expect("workspace root");
    seed_mono_repo(&workspace_root, &database_path);
    let ids = seed_cached_dirty_pool(&workspace_root, &database_path, 10);

    // Cursor starts at 0, so the window is the first three by id — and each
    // is still dirty on disk, so none is promoted.
    let mut commands: Vec<ExpectedCommand> = ids
        .iter()
        .take(3)
        .map(|id| {
            ExpectedCommand::ok(
                workspace_root.join(id),
                "jj",
                &["status", "--no-pager"],
                jj_status_dirty(),
            )
        })
        .collect();
    commands.extend(auto_create_commands(&workspace_root, "mono-agent-011", "fresh011"));
    let runner = FakeRunner::new(commands);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "cached health"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease should provision rather than walk the whole dirty pool");
    // Exhaustion is the assertion that matters: exactly three `jj status`
    // probes ran, not ten.
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-011");
    let scan = &result.payload["health_scan"];
    assert_eq!(scan["cached_unhealthy"], 10);
    assert_eq!(scan["probed"], 3);
    assert_eq!(scan["revalidated"], 3);
    assert_eq!(scan["trusted_cache_skips"], 7);
    assert_eq!(scan["outcome"], "none");
}

#[test]
fn workspace_lease_revalidation_window_rotates_across_leases() {
    // The cached-unhealthy set is not ignored forever — each lease re-probes a
    // different slice, so a workspace cleaned out of band is picked back up
    // within ceil(N / STALE_HEALTH_REVALIDATE_PER_LEASE) leases.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(&workspace_root).expect("workspace root");
    seed_mono_repo(&workspace_root, &database_path);
    let ids = seed_cached_dirty_pool(&workspace_root, &database_path, 9);

    let dirty_probe = |id: &String| {
        ExpectedCommand::ok(
            workspace_root.join(id),
            "jj",
            &["status", "--no-pager"],
            jj_status_dirty(),
        )
    };

    // Lease 1 probes ids[0..3], lease 2 probes ids[3..6] — no overlap.
    let mut commands: Vec<ExpectedCommand> = ids[0..3].iter().map(dirty_probe).collect();
    commands.extend(auto_create_commands(&workspace_root, "mono-agent-010", "fresh010"));
    commands.extend(ids[3..6].iter().map(dirty_probe));
    commands.extend(auto_create_commands(&workspace_root, "mono-agent-011", "fresh011"));
    let runner = FakeRunner::new(commands);

    for task in ["rotate one", "rotate two"] {
        run_with_dependencies(
            Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", task]),
            Some(&database_path),
            &runner,
        )
        .expect("lease");
    }
    runner.assert_exhausted();
}

#[test]
fn workspace_lease_health_scan_stops_at_the_probe_cap() {
    // Twelve workspaces with no cached health at all (so every one is a
    // first-class candidate) that all turn out dirty on disk. The scan must
    // stop at the probe cap and provision rather than walking all twelve.
    use crate::metadata::WorkspaceCandidate;

    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(&workspace_root).expect("workspace root");
    seed_mono_repo(&workspace_root, &database_path);

    let ids: Vec<String> = (1..=12).map(|n| format!("mono-agent-{n:03}")).collect();
    {
        let candidates: Vec<WorkspaceCandidate> = ids
            .iter()
            .map(|id| {
                let path = workspace_root.join(id);
                std::fs::create_dir_all(path.join(".jj")).expect("workspace dir");
                WorkspaceCandidate {
                    workspace_id: id.clone(),
                    workspace_path: path,
                }
            })
            .collect();
        let mut store = Store::open_at(&database_path).unwrap();
        store.sync_workspaces("mono", &candidates).unwrap();
    }

    let mut commands: Vec<ExpectedCommand> = ids
        .iter()
        .take(8)
        .map(|id| {
            ExpectedCommand::ok(
                workspace_root.join(id),
                "jj",
                &["status", "--no-pager"],
                jj_status_dirty(),
            )
        })
        .collect();
    commands.extend(auto_create_commands(&workspace_root, "mono-agent-013", "fresh013"));
    let runner = FakeRunner::new(commands);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "probe cap"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease should provision once the probe cap is hit");
    runner.assert_exhausted();

    let scan = &result.payload["health_scan"];
    assert_eq!(scan["effective_free"], 12);
    assert_eq!(scan["probed"], 8);
    assert_eq!(scan["truncated"], "max_probes");
    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-013");
}

#[test]
fn workspace_lease_prefer_reaches_a_cached_dirty_workspace_outside_the_window() {
    // `--prefer` is an explicit request and always earns its probe, even when
    // the named workspace is cached as dirty and this lease's rotation window
    // would not have reached it. A recovered workspace is promoted and used.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(&workspace_root).expect("workspace root");
    seed_mono_repo(&workspace_root, &database_path);
    let ids = seed_cached_dirty_pool(&workspace_root, &database_path, 10);

    // ids[8] is well outside the cursor-0 window of ids[0..3].
    let preferred = &ids[8];
    let preferred_path = workspace_root.join(preferred);
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            preferred_path.clone(),
            "jj",
            &["status", "--no-pager"],
            jj_status_clean(),
        ),
        ExpectedCommand::ok(preferred_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            preferred_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            preferred_path.clone(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(preferred_path.clone(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            preferred_path.clone(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            "pref009",
        ),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "prefer outside window",
            "--prefer",
            preferred,
        ]),
        Some(&database_path),
        &runner,
    )
    .expect("lease");
    runner.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], preferred.as_str());
    let scan = &result.payload["health_scan"];
    assert_eq!(scan["probed"], 1);
    assert_eq!(scan["promoted_to_clean"], 1);
}

#[test]
fn stale_revalidation_window_rotates_and_wraps() {
    use crate::app::workspace::{advanced_stale_cursor, stale_revalidation_window};

    // Empty set: nothing to revalidate, cursor stays put.
    assert_eq!(stale_revalidation_window(0, 0, 3), Vec::<usize>::new());
    assert_eq!(advanced_stale_cursor(0, 0, 0), 0);

    // Walks forward three at a time — when all three are actually probed.
    assert_eq!(stale_revalidation_window(10, 0, 3), vec![0, 1, 2]);
    assert_eq!(advanced_stale_cursor(10, 0, 3), 3);
    assert_eq!(stale_revalidation_window(10, 3, 3), vec![3, 4, 5]);
    assert_eq!(advanced_stale_cursor(10, 3, 3), 6);
    // …and wraps around the end of the list.
    assert_eq!(stale_revalidation_window(10, 9, 3), vec![9, 0, 1]);
    assert_eq!(advanced_stale_cursor(10, 9, 3), 2);

    // Fewer entries than the window: every entry is covered exactly once.
    assert_eq!(stale_revalidation_window(2, 0, 3), vec![0, 1]);
    assert_eq!(advanced_stale_cursor(2, 0, 2), 0);

    // A cursor left over from a larger pool (or a corrupt negative value)
    // still lands inside the list rather than panicking.
    assert_eq!(stale_revalidation_window(4, 97, 2), vec![1, 2]);
    assert_eq!(advanced_stale_cursor(4, 97, 2), 3);
    assert_eq!(stale_revalidation_window(4, -1, 2), vec![3, 0]);
    assert_eq!(advanced_stale_cursor(4, -1, 2), 1);

    // A lease that reached only part of its window advances only that far,
    // so the entries it never probed come up again next lease instead of
    // being skipped past.
    assert_eq!(advanced_stale_cursor(10, 0, 0), 0);
    assert_eq!(advanced_stale_cursor(10, 0, 1), 1);
    assert_eq!(stale_revalidation_window(10, 1, 3), vec![1, 2, 3]);

    // Covering the whole set takes ceil(N / per_lease) leases *that reach
    // their window*: the rotation itself is complete. Whether a given lease
    // gets that far is a separate, deliberately opportunistic question —
    // see STALE_HEALTH_REVALIDATE_PER_LEASE.
    let mut cursor = 0i64;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..4 {
        let window = stale_revalidation_window(10, cursor, 3);
        cursor = advanced_stale_cursor(10, cursor, window.len());
        seen.extend(window);
    }
    assert_eq!(
        seen.len(),
        10,
        "every cached-unhealthy entry revalidated within 4 window-reaching leases"
    );
}

/// The revalidation cursor may only move across entries a lease actually
/// probed.
///
/// The scan orders effective-free candidates ahead of the rotation window and
/// stops at the first clean one, so the ordinary healthy-pool lease never
/// reaches the window at all. The cursor used to be computed and persisted up
/// front, off the *planned* window — so three cached-dirty entries were
/// skipped per lease while nothing had looked at them, and a workspace an
/// operator cleaned out of band could stay cached dirty indefinitely between
/// GC passes despite the documented rotation.
#[test]
fn lease_leaves_the_revalidation_cursor_alone_when_a_clean_candidate_ends_the_scan() {
    use crate::app::workspace::stale_health_cursor_key;
    use crate::metadata::WorkspaceCandidate;

    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(&workspace_root).expect("workspace root");
    seed_mono_repo(&workspace_root, &database_path);
    let dirty_ids = seed_cached_dirty_pool(&workspace_root, &database_path, 6);

    // One healthy, cached-clean workspace alongside the six cached-dirty ones.
    // It sorts last by id, but effective-free candidates are ordered ahead of
    // the revalidation window regardless.
    let clean_id = "mono-agent-090";
    let clean_path = workspace_root.join(clean_id);
    std::fs::create_dir_all(clean_path.join(".jj")).expect("clean workspace dir");
    {
        let mut candidates: Vec<WorkspaceCandidate> = dirty_ids
            .iter()
            .map(|id| WorkspaceCandidate {
                workspace_id: id.clone(),
                workspace_path: workspace_root.join(id),
            })
            .collect();
        candidates.push(WorkspaceCandidate {
            workspace_id: clean_id.to_string(),
            workspace_path: clean_path.clone(),
        });
        let mut store = Store::open_at(&database_path).unwrap();
        store.sync_workspaces("mono", &candidates).unwrap();
    }

    // Lease one: the clean candidate answers immediately. Not one entry of the
    // rotation window is touched.
    let first = FakeRunner::new({
        let mut c = vec![ExpectedCommand::ok(
            clean_path.clone(),
            "jj",
            &["status", "--no-pager"],
            jj_status_clean(),
        )];
        c.extend(vec![
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
                "clean090",
            ),
        ]);
        c
    });
    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "healthy pool"]),
        Some(&database_path),
        &first,
    )
    .expect("lease");
    first.assert_exhausted();

    assert_eq!(result.payload["workspace"]["workspace_id"], clean_id);
    let scan = &result.payload["health_scan"];
    assert_eq!(scan["probed"], 1, "the scan stops at the first clean candidate");
    assert_eq!(scan["revalidation_window"], 3);
    assert_eq!(scan["window_revalidated"], 0, "the window was never reached");

    assert_eq!(
        Store::open_at(&database_path)
            .unwrap()
            .get_pool_metadata_i(&stale_health_cursor_key("mono"))
            .unwrap(),
        None,
        "a lease that probed no window entry must not move the cursor",
    );

    // The consequence that matters: the next lease — which does reach the
    // window, the clean workspace now being leased — re-offers the SAME first
    // three ids rather than the ones the up-front advance would have skipped
    // to.
    let dirty_probe = |id: &String| {
        ExpectedCommand::ok(
            workspace_root.join(id),
            "jj",
            &["status", "--no-pager"],
            jj_status_dirty(),
        )
    };
    let mut second_commands: Vec<ExpectedCommand> = dirty_ids[0..3].iter().map(dirty_probe).collect();
    second_commands.extend(auto_create_commands(&workspace_root, "mono-agent-091", "fresh091"));
    let second = FakeRunner::new(second_commands);
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "now under pressure"]),
        Some(&database_path),
        &second,
    )
    .expect("lease");
    // Exhaustion IS the assertion: ids[0..3] were probed, not ids[3..6].
    second.assert_exhausted();

    assert_eq!(
        Store::open_at(&database_path)
            .unwrap()
            .get_pool_metadata_i(&stale_health_cursor_key("mono"))
            .unwrap(),
        Some(3),
        "the cursor advances once entries are genuinely probed",
    );
}

/// The other way a lease can fail to reach its window: a heavy walk of dirty
/// effective-free candidates burns the entire probe budget first. That must
/// not advance the cursor past the cached-unhealthy entries it never looked
/// at either.
#[test]
fn lease_does_not_skip_unprobed_window_entries_when_the_probe_cap_is_exhausted() {
    use crate::app::workspace::stale_health_cursor_key;
    use crate::metadata::{WorkspaceCandidate, WorkspaceHealth};

    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(&workspace_root).expect("workspace root");
    seed_mono_repo(&workspace_root, &database_path);

    // Ten candidates with no cached health (all dirty on disk) plus five
    // cached-dirty ones. The probe cap is 8, so the walk never gets past the
    // effective-free set.
    let free_ids: Vec<String> = (1..=10).map(|n| format!("mono-agent-{n:03}")).collect();
    let cached_ids: Vec<String> = (20..=24).map(|n| format!("mono-agent-{n:03}")).collect();
    {
        let candidates: Vec<WorkspaceCandidate> = free_ids
            .iter()
            .chain(cached_ids.iter())
            .map(|id| {
                let path = workspace_root.join(id);
                std::fs::create_dir_all(path.join(".jj")).expect("workspace dir");
                WorkspaceCandidate {
                    workspace_id: id.clone(),
                    workspace_path: path,
                }
            })
            .collect();
        let mut store = Store::open_at(&database_path).unwrap();
        store.sync_workspaces("mono", &candidates).unwrap();
        for id in &cached_ids {
            store
                .update_workspace_health("mono", id, WorkspaceHealth::Dirty)
                .unwrap();
        }
    }

    let mut commands: Vec<ExpectedCommand> = free_ids
        .iter()
        .take(8)
        .map(|id| {
            ExpectedCommand::ok(
                workspace_root.join(id),
                "jj",
                &["status", "--no-pager"],
                jj_status_dirty(),
            )
        })
        .collect();
    commands.extend(auto_create_commands(&workspace_root, "mono-agent-025", "fresh025"));
    let runner = FakeRunner::new(commands);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "cap exhausted"]),
        Some(&database_path),
        &runner,
    )
    .expect("lease");
    runner.assert_exhausted();

    let scan = &result.payload["health_scan"];
    assert_eq!(scan["truncated"], "max_probes");
    assert_eq!(scan["probed"], 8);
    assert_eq!(scan["revalidation_window"], 3);
    assert_eq!(
        scan["window_revalidated"], 0,
        "the budget was gone before the window came up",
    );

    assert_eq!(
        Store::open_at(&database_path)
            .unwrap()
            .get_pool_metadata_i(&stale_health_cursor_key("mono"))
            .unwrap(),
        None,
        "cached-unhealthy ids that were never probed are not rotated past",
    );
}
