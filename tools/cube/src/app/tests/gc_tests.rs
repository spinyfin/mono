use super::support::{
    ExpectedCommand, FakeRunner, audit_events, gc_noop_command, gc_pr_remote_noop_command, head_status_command,
    head_status_output, lease_runner_for, release_guard_reusable_command, release_runner_for, seed_mono_repo,
    unpushed_probe_command, with_database_path,
};
use clap::Parser;
use tempfile::TempDir;

use crate::cli::Cli;

use crate::app::dispatch::run_with_dependencies;
use crate::app::errors::CubeError;
use crate::app::gc::{
    POOL_GC_LAST_AT_KEY, POOL_GC_STARTED_AT_KEY, gc_aged_unhealthy_workspaces, maybe_trigger_pool_gc,
    pool_gc_has_aged_unhealthy_backlog,
};
use crate::app::util::current_epoch_s;

#[test]
fn workspace_gc_verb_forgets_consumed_bookmarks_on_free_workspaces() {
    // Two workspaces: 001 gets leased (skipped by gc), 002 stays free (gc'd).
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let ws1_path = workspace_root.join("mono-agent-001"); // will be leased
    let ws2_path = workspace_root.join("mono-agent-002"); // stays free
    std::fs::create_dir_all(ws1_path.join(".jj")).expect("ws1 dir");
    std::fs::create_dir_all(ws2_path.join(".jj")).expect("ws2 dir");

    seed_mono_repo(&workspace_root, &database_path);

    // Lease ws1 (001) — picks it first since it's clean. This also syncs
    // ws2 into the registry as free.
    let lease_runner = lease_runner_for(&ws1_path, "abc1234");
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "keep leased"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();

    // gc: ws1 is leased → skipped; ws2 is free → fetch + forget.
    let gc_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(ws2_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            ws2_path.clone(),
            "jj",
            &[
                "log",
                "-r",
                "bookmarks(glob:\"boss/exec_*\") & ::main",
                "--no-graph",
                "-T",
                "bookmarks ++ \"\\n\"",
            ],
            "boss/exec_dead_01",
        ),
        gc_pr_remote_noop_command(&ws2_path),
        ExpectedCommand::ok(ws2_path.clone(), "jj", &["bookmark", "forget", "boss/exec_dead_01"], ""),
    ]);
    let gc_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "gc"]),
        Some(&database_path),
        &gc_runner,
    )
    .expect("gc");
    gc_runner.assert_exhausted();

    let results = gc_result.payload["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);

    let ws1_r = results.iter().find(|r| r["workspace_id"] == "mono-agent-001").unwrap();
    assert_eq!(ws1_r["skipped"], true);
    assert_eq!(ws1_r["skipped_reason"], "leased");

    let ws2_r = results.iter().find(|r| r["workspace_id"] == "mono-agent-002").unwrap();
    assert_eq!(ws2_r["skipped"], false);
    assert_eq!(ws2_r["bookmarks_forgotten"].as_array().unwrap().len(), 1);
    assert_eq!(ws2_r["bookmarks_forgotten"][0], "boss/exec_dead_01");
}

#[test]
fn workspace_gc_dry_run_lists_without_forgetting() {
    // dry-run: fetch + log are called, but bookmark forget is NOT.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    // Lease then release to get the workspace into the registry as free.
    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "seed"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();
    let lease_id = lease_result.payload["workspace"]["lease_id"]
        .as_str()
        .unwrap()
        .to_string();
    let release_runner = release_runner_for(&workspace_path);
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release");
    release_runner.assert_exhausted();

    // dry-run: fetch + log, but NO bookmark forget.
    let gc_runner = FakeRunner::new(vec![
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["git", "fetch"], ""),
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
            "boss/exec_dry_01",
        ),
        gc_pr_remote_noop_command(&workspace_path),
    ]);
    let gc_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "gc", "--dry-run"]),
        Some(&database_path),
        &gc_runner,
    )
    .expect("gc dry-run");
    gc_runner.assert_exhausted();

    assert!(gc_result.message.contains("dry-run"));
    let results = gc_result.payload["results"].as_array().unwrap();
    assert_eq!(results[0]["bookmarks_forgotten"].as_array().unwrap().len(), 1);
    assert_eq!(results[0]["bookmarks_forgotten"][0], "boss/exec_dry_01");
}

#[test]
fn gc_forgets_closed_pr_bookmark() {
    // A pr/42 bookmark whose GitHub PR is CLOSED is forgotten by gc.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "pr gc test"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();
    let lease_id = lease_result.payload["workspace"]["lease_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Release runner: gc finds a closed pr/42 bookmark and forgets it.
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
        // exec sweep: no consumed exec bookmarks.
        gc_noop_command(&workspace_path),
        // pr sweep: GitHub remote resolved, pr/42 found, state = CLOSED.
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "github\tgit@github.com:spinyfin/mono.git\norigin\t/local/mirror\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &[
                "log",
                "-r",
                "bookmarks(glob:\"pr/*\")",
                "--no-graph",
                "-T",
                "bookmarks ++ \"\\n\"",
            ],
            "pr/42\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "gh",
            &[
                "pr",
                "view",
                "42",
                "-R",
                "spinyfin/mono",
                "--json",
                "state",
                "--jq",
                ".state",
            ],
            "CLOSED",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "forget", "pr/42"], ""),
    ]);
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release");
    release_runner.assert_exhausted();
}

#[test]
fn gc_forgets_merged_pr_bookmark() {
    // A pr/99 bookmark whose GitHub PR is MERGED is forgotten by gc.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "pr gc merged"]),
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
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "github\tgit@github.com:spinyfin/mono.git\norigin\t/local/mirror\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &[
                "log",
                "-r",
                "bookmarks(glob:\"pr/*\")",
                "--no-graph",
                "-T",
                "bookmarks ++ \"\\n\"",
            ],
            "pr/99\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "gh",
            &[
                "pr",
                "view",
                "99",
                "-R",
                "spinyfin/mono",
                "--json",
                "state",
                "--jq",
                ".state",
            ],
            "MERGED",
        ),
        ExpectedCommand::ok(workspace_path.clone(), "jj", &["bookmark", "forget", "pr/99"], ""),
    ]);
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release");
    release_runner.assert_exhausted();
}

#[test]
fn gc_retains_open_pr_bookmark() {
    // A pr/7 bookmark whose GitHub PR is still OPEN is NOT forgotten.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "pr gc open"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();
    let lease_id = lease_result.payload["workspace"]["lease_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Release runner: gc finds pr/7 but state is OPEN — no forget call.
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
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &["git", "remote", "list"],
            "github\tgit@github.com:spinyfin/mono.git\norigin\t/local/mirror\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "jj",
            &[
                "log",
                "-r",
                "bookmarks(glob:\"pr/*\")",
                "--no-graph",
                "-T",
                "bookmarks ++ \"\\n\"",
            ],
            "pr/7\n",
        ),
        ExpectedCommand::ok(
            workspace_path.clone(),
            "gh",
            &[
                "pr",
                "view",
                "7",
                "-R",
                "spinyfin/mono",
                "--json",
                "state",
                "--jq",
                ".state",
            ],
            "OPEN",
        ),
        // No bookmark forget — pr/7 is still open.
    ]);
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release");
    release_runner.assert_exhausted();
}

#[test]
fn gc_skips_pr_sweep_when_offline() {
    // When jj git remote list fails, pr/* GC is skipped entirely.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    let workspace_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(workspace_path.join(".jj")).expect("workspace dir");

    seed_mono_repo(&workspace_root, &database_path);

    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    let lease_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "offline gc"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();
    let lease_id = lease_result.payload["workspace"]["lease_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Release runner: remote list fails → pr sweep skipped, no extra commands.
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
        ExpectedCommand {
            cwd: workspace_path.clone(),
            program: "jj".to_string(),
            args: ["git", "remote", "list"].iter().map(|s| s.to_string()).collect(),
            result: Err(CubeError::CommandFailed {
                program: "jj".to_string(),
                args: vec!["git".to_string(), "remote".to_string(), "list".to_string()],
                status: Some(1),
                stderr: "no jj repo".to_string(),
            }),
            creates_dir: None,
        },
    ]);
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "release", "--lease", &lease_id]),
        Some(&database_path),
        &release_runner,
    )
    .expect("release");
    release_runner.assert_exhausted();
}

#[test]
fn auto_gc_updates_timestamp_when_stale() {
    // When last_pool_gc_at is older than 24h, lease stamps last_pool_gc_started_at
    // and runs the pass synchronously before returning, so last_pool_gc_at is also
    // advanced by the time the lease command completes.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("dir");

    seed_mono_repo(&workspace_root, &database_path);

    // Set last_pool_gc_at to 25h ago so the next lease triggers gc.
    let old_ts = current_epoch_s().unwrap() - (25 * 60 * 60);
    {
        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        store.set_pool_metadata_i(POOL_GC_LAST_AT_KEY, old_ts).unwrap();
    }

    let workspace_path = workspace_root.join("mono-agent-001");
    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "stale gc test"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();

    // last_pool_gc_started_at must have been set ≈ now (GC was triggered).
    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    let started_ts = store
        .get_pool_metadata_i(POOL_GC_STARTED_AT_KEY)
        .unwrap()
        .expect("last_pool_gc_started_at should be set after triggering GC");
    let now = current_epoch_s().unwrap();
    assert!(now - started_ts < 10, "last_pool_gc_started_at should be near now");
    assert!(
        started_ts > old_ts,
        "last_pool_gc_started_at should have advanced past old completion timestamp"
    );

    // The pass runs synchronously, so last_pool_gc_at must also have
    // advanced by the time the lease command returns — this is the crux
    // of the fix: started and completed no longer diverge.
    let completed_ts = store
        .get_pool_metadata_i(POOL_GC_LAST_AT_KEY)
        .unwrap()
        .expect("last_pool_gc_at should be set after the synchronous pass completes");
    assert!(now - completed_ts < 10, "last_pool_gc_at should be near now");
    assert_eq!(
        completed_ts, started_ts,
        "a synchronous pass should stamp completion at the same instant it started"
    );
}

#[test]
fn auto_gc_skips_when_already_ran_within_24h() {
    // When last_pool_gc_at is recent, lease does NOT update it.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("dir");

    seed_mono_repo(&workspace_root, &database_path);

    // Set last_pool_gc_at to 1h ago — well within 24h.
    let recent_ts = current_epoch_s().unwrap() - 3600;
    {
        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        store.set_pool_metadata_i(POOL_GC_LAST_AT_KEY, recent_ts).unwrap();
    }

    let workspace_path = workspace_root.join("mono-agent-001");
    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "recent gc test"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();

    // last_pool_gc_at must NOT have changed.
    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    let ts = store
        .get_pool_metadata_i(POOL_GC_LAST_AT_KEY)
        .unwrap()
        .expect("last_pool_gc_at should be set");
    assert_eq!(ts, recent_ts, "last_pool_gc_at should NOT change within 24h");
    // And last_pool_gc_started_at should not be set either.
    let started = store.get_pool_metadata_i(POOL_GC_STARTED_AT_KEY).unwrap();
    assert!(
        started.is_none(),
        "last_pool_gc_started_at should not be set when gc was skipped"
    );
}

#[test]
fn auto_gc_skips_when_in_progress() {
    // When last_pool_gc_started_at is recent (< POOL_GC_IN_PROGRESS_TIMEOUT_SECS)
    // and last_pool_gc_at is old, a lease does NOT retrigger — the pass is assumed
    // in progress (guards against two overlapping lease invocations).
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("dir");

    seed_mono_repo(&workspace_root, &database_path);

    let old_completed = current_epoch_s().unwrap() - (25 * 60 * 60); // 25h ago
    let recent_started = current_epoch_s().unwrap() - 60; // 1 min ago
    {
        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        store.set_pool_metadata_i(POOL_GC_LAST_AT_KEY, old_completed).unwrap();
        store
            .set_pool_metadata_i(POOL_GC_STARTED_AT_KEY, recent_started)
            .unwrap();
    }

    let workspace_path = workspace_root.join("mono-agent-001");
    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "in-progress gc test"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();

    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    // GC must NOT have been retriggered (started_at unchanged).
    let ts = store
        .get_pool_metadata_i(POOL_GC_STARTED_AT_KEY)
        .unwrap()
        .expect("last_pool_gc_started_at should still be set");
    assert_eq!(
        ts, recent_started,
        "last_pool_gc_started_at should NOT change while pass is in progress"
    );
}

#[test]
fn auto_gc_allows_retry_after_stuck_timeout() {
    // When last_pool_gc_started_at is old (> POOL_GC_IN_PROGRESS_TIMEOUT_SECS) and
    // last_pool_gc_at was never updated (pass never completed, e.g. the process was
    // killed by external means), a new pass is triggered.
    let (tempdir, database_path) = with_database_path();
    let workspace_root = tempdir.path().join("workspaces");
    std::fs::create_dir_all(workspace_root.join("mono-agent-001").join(".jj")).expect("dir");

    seed_mono_repo(&workspace_root, &database_path);

    let old_completed = current_epoch_s().unwrap() - (25 * 60 * 60); // 25h ago
    let stuck_started = current_epoch_s().unwrap() - (10 * 60); // 10 min ago (> 5 min timeout)
    {
        use crate::store::Store;
        let store = Store::open_at(&database_path).unwrap();
        store.set_pool_metadata_i(POOL_GC_LAST_AT_KEY, old_completed).unwrap();
        store
            .set_pool_metadata_i(POOL_GC_STARTED_AT_KEY, stuck_started)
            .unwrap();
    }

    let workspace_path = workspace_root.join("mono-agent-001");
    let lease_runner = lease_runner_for(&workspace_path, "abc1234");
    run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "stuck gc retry test"]),
        Some(&database_path),
        &lease_runner,
    )
    .expect("lease");
    lease_runner.assert_exhausted();

    use crate::store::Store;
    let store = Store::open_at(&database_path).unwrap();
    let ts = store
        .get_pool_metadata_i(POOL_GC_STARTED_AT_KEY)
        .unwrap()
        .expect("last_pool_gc_started_at should be set");
    let now = current_epoch_s().unwrap();
    assert!(
        ts > stuck_started,
        "last_pool_gc_started_at should have advanced past stuck value"
    );
    assert!(now - ts < 10, "last_pool_gc_started_at should be near now");
}

#[test]
fn pool_gc_backlog_retriggers_before_24h_elapsed() {
    // When an aged-unhealthy workspace is still sitting in the pool,
    // pool_gc_has_aged_unhealthy_backlog reports a backlog, so
    // maybe_trigger_pool_gc must retry after POOL_GC_BACKLOG_RETRY_SECS
    // (5 min) rather than waiting the full AUTO_GC_INTERVAL_SECS (24h).
    let (tempdir, database_path) = with_database_path();
    let (mut store, _ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Dirty)
        .expect("mark dirty");

    // Backdate unhealthy_since well past the 5-day default threshold so
    // this workspace counts as aged-unhealthy backlog.
    let six_days_ago = current_epoch_s().unwrap() - (6 * 86_400);
    store
        .set_workspace_unhealthy_since("mono", "mono-agent-001", six_days_ago)
        .expect("set unhealthy_since");

    // Last pass completed 10 minutes ago: past POOL_GC_BACKLOG_RETRY_SECS
    // (5 min) but nowhere near AUTO_GC_INTERVAL_SECS (24h).
    let now = current_epoch_s().unwrap();
    let ten_min_ago = now - (10 * 60);
    store.set_pool_metadata_i(POOL_GC_LAST_AT_KEY, ten_min_ago).unwrap();

    assert!(
        pool_gc_has_aged_unhealthy_backlog(&store, now).unwrap(),
        "an aged dirty workspace should be reported as backlog"
    );

    maybe_trigger_pool_gc(&mut store, Some(&database_path), now).expect("trigger gc");

    let started_ts = store
        .get_pool_metadata_i(POOL_GC_STARTED_AT_KEY)
        .unwrap()
        .expect("pass should have been (re)triggered because of the backlog");
    assert_eq!(
        started_ts, now,
        "backlog should force a retry despite being < 24h since last pass"
    );
    let completed_ts = store.get_pool_metadata_i(POOL_GC_LAST_AT_KEY).unwrap().unwrap();
    assert_eq!(
        completed_ts, now,
        "the synchronous pass should stamp completion at `now`"
    );
}

#[test]
fn pool_gc_skips_before_24h_when_no_backlog() {
    // With no aged-unhealthy workspace in the pool, the throttle must fall
    // back to the full AUTO_GC_INTERVAL_SECS (24h) — a pass that completed
    // only 10 minutes ago should NOT be retriggered.
    let (tempdir, database_path) = with_database_path();
    let (mut store, _ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    let now = current_epoch_s().unwrap();
    let ten_min_ago = now - (10 * 60);
    store.set_pool_metadata_i(POOL_GC_LAST_AT_KEY, ten_min_ago).unwrap();

    assert!(
        !pool_gc_has_aged_unhealthy_backlog(&store, now).unwrap(),
        "a pool with no unhealthy workspaces should report no backlog"
    );

    maybe_trigger_pool_gc(&mut store, Some(&database_path), now).expect("trigger gc");

    assert!(
        store.get_pool_metadata_i(POOL_GC_STARTED_AT_KEY).unwrap().is_none(),
        "pass should not be retriggered within 24h when there is no backlog"
    );
    let completed_ts = store.get_pool_metadata_i(POOL_GC_LAST_AT_KEY).unwrap().unwrap();
    assert_eq!(
        completed_ts, ten_min_ago,
        "last_pool_gc_at should be untouched when the pass is skipped"
    );
}

#[test]
fn workspace_gc_verb_runs_unhealthy_recycler() {
    // `cube workspace gc` should run the aged-unhealthy recycler, not just
    // forget consumed bookmarks. A dirty workspace past the threshold is reset.
    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Dirty)
        .expect("mark dirty");

    // Backdate unhealthy_since to 6 days ago so the real clock is past the 5-day threshold.
    let six_days_ago = current_epoch_s().unwrap() - (6 * 86_400);
    store
        .set_workspace_unhealthy_since("mono", "mono-agent-001", six_days_ago)
        .expect("set unhealthy_since");
    drop(store);

    // FakeRunner sequence:
    // 1. gc_workspace_bookmarks: fetch, log (no consumed bookmarks), gc_collect pr remote
    // 2. gc_aged_unhealthy_workspaces → reset_workspace: fetch, remote list, bookmark set, jj new
    let gc_runner = FakeRunner::new(vec![
        // gc_workspace_bookmarks
        ExpectedCommand::ok(ws_path.clone(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            ws_path.clone(),
            "jj",
            &[
                "log",
                "-r",
                "bookmarks(glob:\"boss/exec_*\") & ::main",
                "--no-graph",
                "-T",
                "bookmarks ++ \"\\n\"",
            ],
            "",
        ),
        gc_pr_remote_noop_command(&ws_path),
        // gc_aged_unhealthy_workspaces → reset_workspace
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
    ]);

    let gc_result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "gc"]),
        Some(&database_path),
        &gc_runner,
    )
    .expect("gc");
    gc_runner.assert_exhausted();

    assert_eq!(
        gc_result.payload["unhealthy_recycled"].as_u64().unwrap(),
        1,
        "one dirty workspace should have been recycled"
    );
    assert!(
        gc_result.message.contains("1 unhealthy workspace(s) recycled"),
        "message should report recycled count: {}",
        gc_result.message
    );

    // The workspace should now be clean in the store.
    use crate::store::Store;
    let store2 = Store::open_at(&database_path).unwrap();
    let ws_after = store2.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(
        ws_after.health_status, None,
        "workspace health should be cleared after gc recycling"
    );
}

fn setup_unhealthy_gc_scenario(
    tempdir: &TempDir,
    database_path: &std::path::Path,
) -> (crate::store::Store, std::path::PathBuf) {
    use crate::metadata::{RepoRecord, WorkspaceCandidate};
    use crate::store::Store;

    let workspace_root = tempdir.path().join("workspaces");
    let ws_path = workspace_root.join("mono-agent-001");
    std::fs::create_dir_all(ws_path.join(".jj")).expect("workspace dir");

    let mut store = Store::open_at(database_path).expect("store");
    store
        .upsert_repo(&RepoRecord {
            repo: "mono".to_string(),
            origin: "git@github.com:spinyfin/mono.git".to_string(),
            main_branch: "main".to_string(),
            workspace_root: workspace_root.clone(),
            workspace_prefix: "mono-agent-".to_string(),
            source: None,
            clone_command: None,
        })
        .expect("repo");
    store
        .sync_workspaces(
            "mono",
            &[WorkspaceCandidate {
                workspace_id: "mono-agent-001".to_string(),
                workspace_path: ws_path.clone(),
            }],
        )
        .expect("sync");

    (store, ws_path)
}

/// The command sequence `reset_workspace` issues against a workspace.
fn reset_commands_for(ws_path: &std::path::Path) -> Vec<ExpectedCommand> {
    vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            ws_path.to_path_buf(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            ws_path.to_path_buf(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["new", "main@origin"], ""),
    ]
}

fn reset_runner_for(ws_path: &std::path::Path) -> FakeRunner {
    FakeRunner::new(reset_commands_for(ws_path))
}

#[test]
fn gc_resets_aged_dirty_workspace_to_clean() {
    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Dirty)
        .expect("mark dirty");

    // Verify unhealthy_since was set.
    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(ws.health_status, Some(crate::metadata::WorkspaceHealth::Dirty));
    assert!(ws.unhealthy_since_epoch_s.is_some(), "unhealthy_since should be set");

    // Simulate GC running 6 days later (threshold = 5 days).
    let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 6 * 86_400;
    let max_age_secs = 5 * 86_400;

    let runner = reset_runner_for(&ws_path);
    gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, max_age_secs, None);
    runner.assert_exhausted();

    let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(
        ws_after.health_status, None,
        "health_status should be cleared after GC reset"
    );
    assert_eq!(
        ws_after.unhealthy_since_epoch_s, None,
        "unhealthy_since_epoch_s should be cleared after GC reset"
    );
    assert_eq!(ws_after.state, crate::metadata::WorkspaceState::Free);
}

#[test]
fn gc_aged_unhealthy_workspaces_stops_at_deadline() {
    // An already-elapsed deadline must stop the pass before it starts any
    // reset work — this is what lets a bounded pool GC pass defer a large
    // backlog to the next pass instead of blocking `cube workspace lease`.
    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Dirty)
        .expect("mark dirty");

    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 6 * 86_400;
    let max_age_secs = 5 * 86_400;

    // No commands should be issued: the deadline is already in the past.
    use std::time::{Duration, Instant};
    let runner = FakeRunner::default();
    let deadline = Instant::now() - Duration::from_secs(1);
    let recycled = gc_aged_unhealthy_workspaces(
        &runner,
        &store,
        Some(&database_path),
        fake_now,
        max_age_secs,
        Some(deadline),
    );
    runner.assert_exhausted();
    assert_eq!(
        recycled, 0,
        "no candidate should be processed once the deadline has passed"
    );

    // The workspace is untouched — still dirty, still eligible next pass.
    let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(ws_after.health_status, Some(crate::metadata::WorkspaceHealth::Dirty));
}

#[test]
fn gc_resets_aged_conflicted_workspace_to_clean() {
    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Conflicted)
        .expect("mark conflicted");

    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(ws.health_status, Some(crate::metadata::WorkspaceHealth::Conflicted));
    assert!(ws.unhealthy_since_epoch_s.is_some());

    let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 6 * 86_400;
    let max_age_secs = 5 * 86_400;

    let runner = reset_runner_for(&ws_path);
    gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, max_age_secs, None);
    runner.assert_exhausted();

    let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(ws_after.health_status, None);
    assert_eq!(ws_after.unhealthy_since_epoch_s, None);
}

/// Quarantine used to be a one-way door: `gc_aged_unhealthy_workspaces`
/// matched only `Dirty`/`Conflicted`, so a quarantined workspace was
/// stranded forever and the pool permanently lost a slot. GC now reclaims
/// it — but only after re-running the guard's probe and finding that the
/// reset would orphan nothing.
#[test]
fn gc_reclaims_aged_quarantined_workspace_once_nothing_is_orphaned() {
    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Quarantined)
        .expect("mark quarantined");

    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 6 * 86_400;
    let max_age_secs = 5 * 86_400;

    // Probe first (fetch + head status + orphan check), then the normal
    // reset sequence.
    let mut script = vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        head_status_command(&ws_path, &head_status_output("abcd1234", true, "", "", "")),
        unpushed_probe_command(&ws_path, ""),
    ];
    script.extend(reset_commands_for(&ws_path));
    let runner = FakeRunner::new(script);

    let recycled = gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, max_age_secs, None);
    runner.assert_exhausted();
    assert_eq!(recycled, 1);

    let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(ws_after.health_status, None, "quarantine must be lifted");
    assert_eq!(ws_after.unhealthy_since_epoch_s, None);
    assert_eq!(ws_after.state, crate::metadata::WorkspaceState::Free);

    let events = audit_events(&tempdir);
    let cleared: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.quarantine_cleared")
        .collect();
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0]["source"], "unhealthy_gc");
    assert_eq!(cleared[0]["reuse_reason"], "nothing_orphaned");
}

/// The protection that must survive the reclaim path: an aged quarantine
/// whose `@` still holds work no remote has is left exactly as it is. GC
/// must never reset it on age alone.
#[test]
fn gc_leaves_aged_quarantined_workspace_with_unpushed_work_alone() {
    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Quarantined)
        .expect("mark quarantined");

    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 60 * 86_400;
    let max_age_secs = 5 * 86_400;

    // The probe runs and refuses; no reset commands may follow.
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        head_status_command(
            &ws_path,
            &head_status_output("abcd1234", false, "wip-bookmark", "wip-bookmark", ""),
        ),
        unpushed_probe_command(&ws_path, "abcd1234\t6e6b90bc\n"),
    ]);

    let recycled = gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, max_age_secs, None);
    runner.assert_exhausted();
    assert_eq!(recycled, 0, "no matter how old, unpushed work is never discarded");

    let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(
        ws_after.health_status,
        Some(crate::metadata::WorkspaceHealth::Quarantined),
    );

    let events = audit_events(&tempdir);
    let refused: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.quarantine_reclaim_refused")
        .collect();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0]["source"], "unhealthy_gc");
    assert_eq!(refused[0]["unpushed_commits"], "abcd1234:6e6b90bc");
}

#[test]
fn gc_skips_recently_unhealthy_workspace() {
    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Dirty)
        .expect("mark dirty");

    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    let unhealthy_since = ws.unhealthy_since_epoch_s.unwrap();

    // GC runs only 1 day after unhealthy_since; threshold is 5 days.
    let fake_now = unhealthy_since + 86_400;
    let max_age_secs = 5 * 86_400;

    // No reset commands should be issued.
    let runner = FakeRunner::default();
    gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, max_age_secs, None);
    runner.assert_exhausted();

    let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(
        ws_after.health_status,
        Some(crate::metadata::WorkspaceHealth::Dirty),
        "recent unhealthy workspace should be left untouched"
    );
    assert_eq!(ws_after.unhealthy_since_epoch_s, Some(unhealthy_since));
}

#[test]
fn unhealthy_since_preserved_through_dirty_to_conflicted_transition() {
    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Dirty)
        .expect("mark dirty");

    let ws_after_dirty = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    let original_since = ws_after_dirty.unhealthy_since_epoch_s.unwrap();

    // Transition to conflicted without becoming clean first.
    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Conflicted)
        .expect("mark conflicted");

    let ws_after_conflicted = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(
        ws_after_conflicted.health_status,
        Some(crate::metadata::WorkspaceHealth::Conflicted)
    );
    assert_eq!(
        ws_after_conflicted.unhealthy_since_epoch_s,
        Some(original_since),
        "unhealthy_since should not be reset when transitioning between unhealthy states"
    );
}
