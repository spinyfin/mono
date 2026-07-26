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
use crate::app::salvage::{
    SALVAGE_COMMIT_LIMIT, SALVAGE_LOG_TEMPLATE, SALVAGE_RETENTION_SECS, gc_aged_salvage_records, list_salvage_records,
    salvage_dir, salvage_log_limit_arg, salvage_revset,
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
            duration: std::time::Duration::ZERO,
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
    // 2. gc_aged_unhealthy_workspaces → probe (fetch + head status), then
    //    remote list, bookmark set, jj new
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
        // gc_aged_unhealthy_workspaces → probe (fetch + head status), then reset
        ExpectedCommand::ok(ws_path.clone(), "jj", &["git", "fetch"], ""),
        head_status_command(&ws_path, &head_status_output("abcd1234", true, "main", "main", "main")),
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

/// The destructive half of a reset, with no leading `jj git fetch`. The
/// retention path probes first (which fetches), so it finishes the reset from
/// there rather than fetching twice per workspace inside a time-budgeted pass.
fn reset_after_fetch_commands_for(ws_path: &std::path::Path) -> Vec<ExpectedCommand> {
    vec![
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

/// The command sequence salvage issues: one `jj log` over the salvage revset,
/// then one `jj diff` per captured commit. Built from the real constants so a
/// change to either drifts the test rather than silently passing.
fn salvage_commands_for(ws_path: &std::path::Path, log_output: &str, diffs: &[(&str, &str)]) -> Vec<ExpectedCommand> {
    let revset = salvage_revset("main");
    let limit = salvage_log_limit_arg();
    let mut v = vec![ExpectedCommand::ok(
        ws_path.to_path_buf(),
        "jj",
        &[
            "log",
            "--no-graph",
            "-n",
            &limit,
            "-r",
            &revset,
            "-T",
            SALVAGE_LOG_TEMPLATE,
        ],
        log_output,
    )];
    for (commit_id, diff) in diffs {
        v.push(ExpectedCommand::ok(
            ws_path.to_path_buf(),
            "jj",
            &["diff", "--no-pager", "--git", "-r", commit_id],
            diff,
        ));
    }
    v
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

    // Retention expiry always probes first. Here `@` is empty and on main, so
    // there is nothing to salvage and the reset proceeds straight away.
    let mut script = vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        head_status_command(&ws_path, &head_status_output("abcd1234", true, "main", "main", "main")),
    ];
    script.extend(reset_after_fetch_commands_for(&ws_path));
    let runner = FakeRunner::new(script);
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

    let mut script = vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        head_status_command(&ws_path, &head_status_output("abcd1234", true, "main", "main", "main")),
    ];
    script.extend(reset_after_fetch_commands_for(&ws_path));
    let runner = FakeRunner::new(script);
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
    script.extend(reset_after_fetch_commands_for(&ws_path));
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

/// The protection that must survive the retention TTL: a workspace whose `@`
/// still holds work no remote has is left exactly as it is *while it is inside
/// its retention window*. Age is the only thing that ever lets cube touch it,
/// and the window has to elapse first.
#[test]
fn gc_leaves_quarantined_workspace_with_unpushed_work_alone_inside_the_ttl() {
    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Quarantined)
        .expect("mark quarantined");

    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    // Half a day into a one-day retention window.
    let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 43_200;
    let max_age_secs = 86_400;

    // Nothing may be issued at all: the workspace is not even a candidate yet.
    let runner = FakeRunner::default();
    let recycled = gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, max_age_secs, None);
    runner.assert_exhausted();
    assert_eq!(recycled, 0);

    let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(
        ws_after.health_status,
        Some(crate::metadata::WorkspaceHealth::Quarantined),
        "retention inside the window is untouched",
    );
}

/// Retention is bounded, not removed. Past the TTL, a quarantined workspace
/// whose `@` still holds unpushed work is reclaimed — but only after the work
/// has been written to a durable salvage record. Before this, that workspace
/// was withheld forever.
#[test]
fn gc_salvages_unpushed_work_before_reclaiming_past_the_ttl() {
    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Quarantined)
        .expect("mark quarantined");

    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 60 * 86_400;
    let max_age_secs = 86_400;

    let mut script = vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        head_status_command(
            &ws_path,
            &head_status_output("abcd1234", false, "wip-bookmark", "wip-bookmark", ""),
        ),
        unpushed_probe_command(&ws_path, "abcd1234\t6e6b90bc\n"),
    ];
    script.extend(salvage_commands_for(
        &ws_path,
        "abcd1234\t6e6b90bc\thalf-finished refactor\n",
        &[("6e6b90bc", "diff --git a/x b/x\n+one\n")],
    ));
    script.extend(reset_after_fetch_commands_for(&ws_path));
    let runner = FakeRunner::new(script);

    let recycled = gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, max_age_secs, None);
    runner.assert_exhausted();
    assert_eq!(recycled, 1, "an expired retention is reclaimed once salvaged");

    let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(ws_after.health_status, None, "workspace returns to the pool");
    assert_eq!(ws_after.unhealthy_since_epoch_s, None);

    // The work is retrievable: manifest, commit index, and a real patch.
    let records = list_salvage_records(Some(&database_path), None, None).expect("salvage records");
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.manifest.workspace_id, "mono-agent-001");
    assert_eq!(record.manifest.repo, "mono");
    assert_eq!(record.manifest.prior_health, "quarantined");
    assert_eq!(record.manifest.commits.len(), 1);
    assert_eq!(record.manifest.commits[0].change_id, "abcd1234");
    assert_eq!(record.manifest.commits[0].description, "half-finished refactor");
    let patch = std::fs::read_to_string(record.path.join(&record.manifest.commits[0].patch)).expect("patch file");
    assert!(patch.contains("+one"), "the patch holds the actual work: {patch}");
    assert!(record.path.join("commits.txt").exists());

    let events = audit_events(&tempdir);
    let salvaged: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.retention_salvaged")
        .collect();
    assert_eq!(salvaged.len(), 1);
    assert_eq!(salvaged[0]["unpushed_commits"], "abcd1234:6e6b90bc");
    assert_eq!(salvaged[0]["workspace_id"], "mono-agent-001");
}

/// If the work cannot be captured, the workspace is NOT reclaimed. Bounding
/// retention is only defensible while expiry cannot lose anything, so a failed
/// salvage has to be a hard stop rather than something the reset proceeds past.
#[test]
fn gc_leaves_workspace_retained_when_salvage_fails() {
    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Dirty)
        .expect("mark dirty");

    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 60 * 86_400;
    let max_age_secs = 86_400;

    // The salvage log call fails; no reset commands may follow it.
    let revset = salvage_revset("main");
    let limit = salvage_log_limit_arg();
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        head_status_command(
            &ws_path,
            &head_status_output("abcd1234", false, "wip-bookmark", "wip-bookmark", ""),
        ),
        unpushed_probe_command(&ws_path, "abcd1234\t6e6b90bc\n"),
        ExpectedCommand::no_such_remote_bookmark(
            ws_path.to_path_buf(),
            "jj",
            &[
                "log",
                "--no-graph",
                "-n",
                &limit,
                "-r",
                &revset,
                "-T",
                SALVAGE_LOG_TEMPLATE,
            ],
        ),
    ]);

    let recycled = gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, max_age_secs, None);
    runner.assert_exhausted();
    assert_eq!(recycled, 0, "a workspace whose work could not be captured is not reset");

    let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(
        ws_after.health_status,
        Some(crate::metadata::WorkspaceHealth::Dirty),
        "the workspace stays retained and is retried next pass",
    );

    let events = audit_events(&tempdir);
    let failed: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.retention_salvage_failed")
        .collect();
    assert_eq!(failed.len(), 1);
}

/// The salvage store is itself retained rather than growing forever — the
/// failure mode this whole change is about does not get to reappear one level
/// up.
#[test]
fn gc_removes_salvage_records_past_their_retention_window() {
    let (_tempdir, database_path) = with_database_path();
    let root = salvage_dir(Some(&database_path)).expect("salvage dir").join("mono");

    let write_record = |id: &str, salvaged_at: i64| {
        let dir = root.join(format!("{id}-{salvaged_at}"));
        std::fs::create_dir_all(dir.join("patches")).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::json!({
                "schema": 1,
                "repo": "mono",
                "workspace_id": id,
                "workspace_path": "/tmp/x",
                "main_branch": "main",
                "salvaged_at_epoch_s": salvaged_at,
                "unhealthy_since_epoch_s": salvaged_at,
                "retained_secs": 0,
                "prior_health": "dirty",
                "last_release_reason": serde_json::Value::Null,
                "holder": serde_json::Value::Null,
                "task": serde_json::Value::Null,
                "commits": [],
                "restore_hint": "",
            })
            .to_string(),
        )
        .unwrap();
        dir
    };

    let now = 2_000_000_000i64;
    let fresh = write_record("mono-agent-001", now - 3600);
    let stale = write_record("mono-agent-002", now - SALVAGE_RETENTION_SECS - 3600);

    let removed = gc_aged_salvage_records(Some(&database_path), now);
    assert_eq!(removed, 1);
    assert!(fresh.exists(), "a record inside the window is kept");
    assert!(!stale.exists(), "a record past the window is removed");
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

/// A stack longer than the export cap must NOT be quietly trimmed.
///
/// `jj log -n N` keeps the *newest* N, so truncation drops the oldest history
/// — and every patch that survives is then rooted on a parent that was never
/// exported, which makes the remaining series unapplicable rather than merely
/// incomplete. Salvage therefore asks for `SALVAGE_COMMIT_LIMIT + 1`, sees the
/// overflow, and refuses: nothing is written and the workspace stays retained.
#[test]
fn gc_refuses_to_reclaim_when_the_unpushed_stack_exceeds_the_salvage_cap() {
    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Dirty)
        .expect("mark dirty");

    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 60 * 86_400;
    let max_age_secs = 86_400;

    // 51 commits — one more than the cap, which is exactly what the
    // `-n LIMIT + 1` query exists to make visible.
    let log: String = (1..=SALVAGE_COMMIT_LIMIT + 1)
        .map(|n| format!("chg{n:04}\tcmt{n:04}\twip {n}\n"))
        .collect();

    let mut script = vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        head_status_command(
            &ws_path,
            &head_status_output("chg0051", false, "wip-bookmark", "wip-bookmark", ""),
        ),
        unpushed_probe_command(&ws_path, "chg0051\tcmt0051\n"),
    ];
    // Only the log runs: no `jj diff` may be issued, and no reset may follow.
    script.extend(salvage_commands_for(&ws_path, &log, &[]));
    let runner = FakeRunner::new(script);

    let recycled = gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, max_age_secs, None);
    runner.assert_exhausted();
    assert_eq!(recycled, 0, "a stack too long to export completely is not reclaimed");

    let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(
        ws_after.health_status,
        Some(crate::metadata::WorkspaceHealth::Dirty),
        "the workspace stays retained with its work intact",
    );

    // Nothing was written: not a record, not a partial directory.
    assert!(
        list_salvage_records(Some(&database_path), None, None)
            .expect("salvage records")
            .is_empty(),
        "a truncated salvage must not leave a record claiming the work was saved",
    );
    assert!(
        !salvage_dir(Some(&database_path))
            .expect("salvage dir")
            .join("mono")
            .exists(),
        "no directory is created for a salvage that refuses before writing",
    );

    let events = audit_events(&tempdir);
    let failed: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.retention_salvage_failed")
        .collect();
    assert_eq!(failed.len(), 1);
    let error = failed[0]["error"].as_str().unwrap_or_default();
    assert!(
        error.contains(&format!("more than {SALVAGE_COMMIT_LIMIT}")),
        "the audit trail names the cap that was hit: {error}",
    );
}

/// Salvage runs only after the reuse probe refused — i.e. jj has just said `@`
/// holds work no remote has. A salvage log that then comes back empty means
/// the revset, the template or the parse disagreed with the probe, not that
/// there is nothing to save. That used to return `Ok` with zero commits and
/// license the destructive reset.
#[test]
fn gc_refuses_to_reclaim_when_the_salvage_log_comes_back_empty() {
    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Quarantined)
        .expect("mark quarantined");

    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 60 * 86_400;
    let max_age_secs = 86_400;

    let mut script = vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        head_status_command(
            &ws_path,
            &head_status_output("abcd1234", false, "wip-bookmark", "wip-bookmark", ""),
        ),
        unpushed_probe_command(&ws_path, "abcd1234\t6e6b90bc\n"),
    ];
    script.extend(salvage_commands_for(&ws_path, "", &[]));
    let runner = FakeRunner::new(script);

    let recycled = gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, max_age_secs, None);
    runner.assert_exhausted();
    assert_eq!(recycled, 0, "an empty capture is not a successful salvage");

    let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(
        ws_after.health_status,
        Some(crate::metadata::WorkspaceHealth::Quarantined),
        "health is unchanged — the workspace is retried next pass",
    );
    assert!(
        list_salvage_records(Some(&database_path), None, None)
            .expect("salvage records")
            .is_empty(),
    );

    let events = audit_events(&tempdir);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["event"] == "workspace.retention_salvage_failed")
            .count(),
        1,
    );
}

/// The salvage is taken *for* a reclaim. If the reclaim then fails the live
/// workspace is still there, still retained, and still the source of truth —
/// so the copy is discarded rather than left to be duplicated by every
/// subsequent pass until the 30-day salvage TTL.
#[test]
fn gc_discards_the_salvage_record_when_the_reset_fails() {
    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Dirty)
        .expect("mark dirty");

    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 60 * 86_400;
    let max_age_secs = 86_400;

    let mut script = vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        head_status_command(
            &ws_path,
            &head_status_output("abcd1234", false, "wip-bookmark", "wip-bookmark", ""),
        ),
        unpushed_probe_command(&ws_path, "abcd1234\t6e6b90bc\n"),
    ];
    script.extend(salvage_commands_for(
        &ws_path,
        "abcd1234\t6e6b90bc\thalf-finished refactor\n",
        &[("6e6b90bc", "diff --git a/x b/x\n+one\n")],
    ));
    // The reset's first step fails, so the reclaim never completes.
    script.push(ExpectedCommand::ok(
        ws_path.to_path_buf(),
        "jj",
        &["git", "remote", "list"],
        "origin\tgit@github.com:spinyfin/mono.git\n",
    ));
    script.push(ExpectedCommand::ok(
        ws_path.to_path_buf(),
        "jj",
        &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
        "",
    ));
    script.push(ExpectedCommand::no_such_remote_bookmark(
        ws_path.to_path_buf(),
        "jj",
        &["new", "main@origin"],
    ));
    let runner = FakeRunner::new(script);

    let recycled = gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, max_age_secs, None);
    runner.assert_exhausted();
    assert_eq!(recycled, 0);

    let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(
        ws_after.health_status,
        Some(crate::metadata::WorkspaceHealth::Dirty),
        "the workspace is still retained, so its work is still reachable in place",
    );
    assert!(
        list_salvage_records(Some(&database_path), None, None)
            .expect("salvage records")
            .is_empty(),
        "the orphaned copy is discarded rather than duplicated next pass",
    );

    let events = audit_events(&tempdir);
    let discarded: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.retention_salvage_discarded")
        .collect();
    assert_eq!(discarded.len(), 1);
    assert_eq!(discarded[0]["reason"], "reset_failed");
}

/// A salvage record names the holder and task the work came from. Both live
/// columns are always NULL by the time retention GC runs (release clears
/// them), so the manifest used to carry `null` for the one field that answers
/// "whose work is this?".
#[test]
fn salvage_record_attributes_the_holder_and_task_from_the_prior_lease() {
    let (tempdir, database_path) = with_database_path();
    let (mut store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    // Lease and release the workspace the way a real worker would, then let it
    // go unhealthy — exactly the sequence that ends with holder/task NULL.
    store
        .claim_specific_workspace(
            "mono",
            "mono-agent-001",
            "worker@host:1234",
            "port the recommendations surface",
            "lease-abc",
            1_000,
            Some(2_000),
        )
        .expect("claim")
        .expect("claimed");
    store
        .release_workspace("lease-abc", Some("unpushed_work_preserved"))
        .expect("release");
    let freed = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(freed.holder, None, "the live columns are still cleared on release");
    assert_eq!(freed.task, None);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Dirty)
        .expect("mark dirty");
    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 60 * 86_400;

    let mut script = vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        head_status_command(
            &ws_path,
            &head_status_output("abcd1234", false, "wip-bookmark", "wip-bookmark", ""),
        ),
        unpushed_probe_command(&ws_path, "abcd1234\t6e6b90bc\n"),
    ];
    script.extend(salvage_commands_for(
        &ws_path,
        "abcd1234\t6e6b90bc\thalf-finished refactor\n",
        &[("6e6b90bc", "diff --git a/x b/x\n+one\n")],
    ));
    script.extend(reset_after_fetch_commands_for(&ws_path));
    let runner = FakeRunner::new(script);

    let recycled = gc_aged_unhealthy_workspaces(&runner, &store, Some(&database_path), fake_now, 86_400, None);
    runner.assert_exhausted();
    assert_eq!(recycled, 1);

    let records = list_salvage_records(Some(&database_path), None, None).expect("salvage records");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].manifest.holder.as_deref(), Some("worker@host:1234"));
    assert_eq!(
        records[0].manifest.task.as_deref(),
        Some("port the recommendations surface"),
    );

    let events = audit_events(&tempdir);
    let salvaged: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "workspace.retention_salvaged")
        .collect();
    assert_eq!(salvaged.len(), 1);
    assert_eq!(salvaged[0]["prior_holder"], "worker@host:1234");
    assert_eq!(salvaged[0]["prior_task"], "port the recommendations surface");
}

/// Debris under the salvage root — a directory with no parseable manifest —
/// is invisible to `list_salvage_records`, so the retention sweep above can
/// never age it out. It has to be swept on its own terms or it accumulates
/// forever.
#[test]
fn gc_sweeps_manifest_less_salvage_debris_past_the_grace_period() {
    let (_tempdir, database_path) = with_database_path();
    let root = salvage_dir(Some(&database_path)).expect("salvage dir").join("mono");

    let partial = root.join("mono-agent-001-123.partial");
    std::fs::create_dir_all(partial.join("patches")).unwrap();
    std::fs::write(partial.join("patches/001-abc.diff"), b"diff").unwrap();

    // `now` far in the future relative to the directory's real mtime, so it is
    // past the grace period.
    let now = current_epoch_s().unwrap() + 86_400;
    let removed = gc_aged_salvage_records(Some(&database_path), now);
    assert_eq!(removed, 1);
    assert!(!partial.exists(), "manifest-less debris is swept");

    // A partial written just now is inside the grace period and left alone,
    // so a concurrent in-flight salvage is never swept out from under itself.
    let fresh = root.join("mono-agent-002-456.partial");
    std::fs::create_dir_all(&fresh).unwrap();
    assert_eq!(
        gc_aged_salvage_records(Some(&database_path), current_epoch_s().unwrap()),
        0
    );
    assert!(fresh.exists(), "an in-flight salvage is inside the grace period");
}

/// The aged-unhealthy backlog check gates how soon the next pool GC pass may
/// run. It used to match only `Dirty`/`Conflicted`, so a backlog made entirely
/// of quarantined rows — which retention now reclaims by exactly the same
/// route — reported "no backlog": a partial 20-second pass stamped completion
/// and the next auto GC waited the full 24h instead of the 5-minute retry,
/// leaving the free pool withheld far longer than the retention TTL implies.
#[test]
fn aged_quarantined_rows_count_as_backlog_so_gc_retries_in_minutes() {
    let (tempdir, database_path) = with_database_path();
    let (mut store, _ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Quarantined)
        .expect("mark quarantined");
    let six_days_ago = current_epoch_s().unwrap() - (6 * 86_400);
    store
        .set_workspace_unhealthy_since("mono", "mono-agent-001", six_days_ago)
        .expect("set unhealthy_since");

    let now = current_epoch_s().unwrap();
    assert!(
        pool_gc_has_aged_unhealthy_backlog(&store, now).unwrap(),
        "an aged quarantined workspace is backlog: retention reclaims it by age plus salvage",
    );

    // The interval that follows from that: a pass 10 minutes ago is past
    // POOL_GC_BACKLOG_RETRY_SECS (5m) and nowhere near AUTO_GC_INTERVAL_SECS
    // (24h), so the next lease retriggers.
    store.set_pool_metadata_i(POOL_GC_LAST_AT_KEY, now - (10 * 60)).unwrap();
    maybe_trigger_pool_gc(&mut store, Some(&database_path), now).expect("trigger gc");
    assert_eq!(
        store
            .get_pool_metadata_i(POOL_GC_STARTED_AT_KEY)
            .unwrap()
            .expect("a quarantined backlog must retrigger inside the 5-minute retry window"),
        now,
    );
}

/// A quarantined row with no `unhealthy_since` stamp at all (written before
/// that column was set on this path) is treated as old by the reclaim pass, so
/// the backlog check has to agree — otherwise the two disagree in the
/// direction that pins the interval at 24h.
#[test]
fn aged_backlog_matches_the_reclaim_pass_for_quarantined_rows_without_a_clock() {
    let (tempdir, database_path) = with_database_path();
    let (store, _ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Quarantined)
        .expect("mark quarantined");
    // Null the clock, the way a row written before that column was stamped on
    // the quarantine path carries it.
    rusqlite::Connection::open(&database_path)
        .expect("sqlite open")
        .execute(
            "UPDATE workspaces SET unhealthy_since_epoch_s = NULL WHERE workspace_id = ?1",
            rusqlite::params!["mono-agent-001"],
        )
        .expect("null unhealthy_since");

    assert!(
        pool_gc_has_aged_unhealthy_backlog(&store, current_epoch_s().unwrap()).unwrap(),
        "a clockless quarantined row is a candidate for reclaim, so it is backlog too",
    );
}

/// The GC budget has to reach the subprocesses, not just the loop around them.
///
/// `gc_aged_unhealthy_workspaces` checked its deadline before *selecting* a
/// candidate and then handed the probe an unbounded `jj git fetch`: 120s per
/// attempt with two retries behind it. One slow remote could therefore hold a
/// lease for minutes, blowing both the 20-second pool-GC budget and the
/// engine's 90-second lease timeout.
#[test]
fn gc_bounds_the_reuse_probe_by_the_remaining_pass_deadline() {
    use std::time::{Duration, Instant};

    let (tempdir, database_path) = with_database_path();
    let (store, ws_path) = setup_unhealthy_gc_scenario(&tempdir, &database_path);

    store
        .update_workspace_health("mono", "mono-agent-001", crate::metadata::WorkspaceHealth::Dirty)
        .expect("mark dirty");

    let ws = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    let fake_now = ws.unhealthy_since_epoch_s.unwrap() + 6 * 86_400;

    // A fetch that would take far longer than the whole pass is allowed.
    let budget = Duration::from_millis(300);
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(ws_path.to_path_buf(), "jj", &["git", "fetch"], "").taking(Duration::from_secs(30)),
    ]);

    let started = Instant::now();
    let recycled = gc_aged_unhealthy_workspaces(
        &runner,
        &store,
        Some(&database_path),
        fake_now,
        5 * 86_400,
        Some(started + budget),
    );
    let elapsed = started.elapsed();
    runner.assert_exhausted();

    assert_eq!(
        recycled, 0,
        "a candidate whose probe cannot finish in budget is deferred"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the pass must end near its own budget, not at the network timeout: took {elapsed:?}",
    );

    // The regression itself: the timeout handed to the subprocess is the
    // remaining budget, not the 120-second default.
    let timeouts = runner.observed_timeouts();
    assert_eq!(timeouts.len(), 1, "no retry may start once the budget is gone");
    assert!(
        timeouts[0] <= budget,
        "the pass deadline must reach the subprocess bound, got {:?}",
        timeouts[0],
    );

    // Untouched, and eligible again next pass.
    let ws_after = store.get_workspace_by_path(&ws_path).unwrap().unwrap();
    assert_eq!(ws_after.health_status, Some(crate::metadata::WorkspaceHealth::Dirty));
}
