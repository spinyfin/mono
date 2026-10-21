//! Tests for repo-declared workspace cleanup.
//!
//! cube's side only: a fake repo whose workspaces declare a cleanup command,
//! and a runner that stands in for the command — recording which workspace it
//! ran against and simulating the bytes it frees by moving the injected
//! free-space reading. Nothing here knows what a real cleanup command does.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use clap::Parser;
use tempfile::TempDir;

use super::support::{
    ExpectedCommand, FakeRunner, audit_events, gc_exec_sweep_command, gc_pr_noop_command, write_setup_yaml,
};

use crate::app::cleanup::{CleanupOptions, CleanupReport, CleanupTrigger, run_cleanup_pass};
use crate::app::disk::testing::{self, AMPLE_TEST_VOLUME, volume_with_free};
use crate::app::dispatch::run_with_dependencies;
use crate::app::errors::CubeError;
use crate::cli::Cli;
use crate::command_runner::{CommandInvocation, CommandRunner};
use crate::config::PoolConfig;
use crate::metadata::{RepoRecord, WorkspaceCandidate, WorkspaceHealth};
use crate::store::{CleanupResult, CleanupStatus, Store};

const GIB: u64 = 1024 * 1024 * 1024;

const DECLARES_CLEANUP: &str = "version: 1\ncleanup:\n  command: ./clean-this-workspace\n";

/// The free-space marks the default configuration produces on the test volume.
fn marks() -> (u64, u64) {
    let pool = PoolConfig::default();
    (
        pool.free_space_floor_bytes(AMPLE_TEST_VOLUME.total_bytes),
        pool.cleanup_high_water_bytes(AMPLE_TEST_VOLUME.total_bytes),
    )
}

struct Pool {
    tempdir: TempDir,
    database_path: PathBuf,
    store: Store,
    workspaces: Vec<PathBuf>,
}

impl Pool {
    /// A `mono` repo with `count` free workspaces, `mono-agent-001..`, each
    /// declaring a cleanup command. Workspace `n` was last active at
    /// `n * 100`, so a higher number is more recently used.
    fn seed(count: usize) -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let database_path = tempdir.path().join("state.db");
        let workspace_root = tempdir.path().join("workspaces");
        let mut store = Store::open_at(&database_path).expect("store");
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
        let mut candidates = Vec::new();
        let mut workspaces = Vec::new();
        for n in 1..=count {
            let id = format!("mono-agent-{n:03}");
            let path = workspace_root.join(&id);
            std::fs::create_dir_all(path.join(".jj")).expect("workspace dir");
            write_setup_yaml(&path, DECLARES_CLEANUP);
            candidates.push(WorkspaceCandidate {
                workspace_id: id,
                workspace_path: path.clone(),
            });
            workspaces.push(path);
        }
        store.sync_workspaces("mono", &candidates).expect("sync");
        let pool = Self {
            tempdir,
            database_path,
            store,
            workspaces,
        };
        for n in 1..=count {
            pool.store
                .set_workspace_last_activity("mono", &format!("mono-agent-{n:03}"), n as i64 * 100)
                .expect("activity");
        }
        pool
    }

    fn id(&self, index: usize) -> String {
        format!("mono-agent-{:03}", index + 1)
    }

    fn pass(&self, runner: &dyn CommandRunner, dry_run: bool) -> CleanupReport {
        run_cleanup_pass(
            runner,
            &self.store,
            Some(&self.database_path),
            &CleanupOptions {
                repo: None,
                workspace: None,
                trigger: CleanupTrigger::Manual,
                dry_run,
                deadline: None,
            },
        )
    }

    fn cleanup_status(&self, index: usize) -> Option<CleanupStatus> {
        self.store
            .get_workspace_cleanup_status("mono", &self.id(index))
            .expect("status")
    }

    fn claim(&self, index: usize) -> bool {
        // A fresh connection, as a concurrent `cube workspace lease` has.
        Store::open_at(&self.database_path)
            .expect("second connection")
            .claim_specific_workspace("mono", &self.id(index), "boss", "task", "lease", 5_000, None)
            .expect("claim")
            .is_some()
    }
}

/// What the stand-in command does in a given workspace (by directory name).
type OutcomeFn<'a> = Box<dyn Fn(&str) -> Result<String, CubeError> + 'a>;
/// Runs while the command is in flight, as a concurrent lease would.
type DuringFn<'a> = Box<dyn Fn(&str) + 'a>;

/// Stands in for the repo's cleanup command.
struct CleanupRunner<'a> {
    /// Workspace directory names the command ran in, in order.
    ran_in: RefCell<Vec<String>>,
    free: Cell<u64>,
    freed_per_clean: u64,
    outcome: OutcomeFn<'a>,
    during: DuringFn<'a>,
}

impl<'a> CleanupRunner<'a> {
    fn new(start_free: u64, freed_per_clean: u64) -> Self {
        Self {
            ran_in: RefCell::new(Vec::new()),
            free: Cell::new(start_free),
            freed_per_clean,
            outcome: Box::new(|_| Ok(String::new())),
            during: Box::new(|_| {}),
        }
    }

    fn outcome(mut self, outcome: impl Fn(&str) -> Result<String, CubeError> + 'a) -> Self {
        self.outcome = Box::new(outcome);
        self
    }

    fn during(mut self, during: impl Fn(&str) + 'a) -> Self {
        self.during = Box::new(during);
        self
    }

    fn ran_in(&self) -> Vec<String> {
        self.ran_in.borrow().clone()
    }
}

impl CommandRunner for CleanupRunner<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String, CubeError> {
        assert_eq!(invocation.program, "sh");
        assert_eq!(invocation.args, ["-c", "./clean-this-workspace"]);
        let name = invocation.cwd.file_name().unwrap().to_string_lossy().to_string();
        let env: std::collections::HashMap<_, _> = invocation.env.iter().cloned().collect();
        assert_eq!(
            env.get("CUBE_WORKSPACE").map(String::as_str),
            Some(invocation.cwd.to_str().unwrap()),
            "the command is told which workspace to clean",
        );
        self.ran_in.borrow_mut().push(name.clone());
        (self.during)(&name);
        let outcome = (self.outcome)(&name);
        if outcome.is_ok() {
            self.free.set(self.free.get() + self.freed_per_clean);
            // The command "freed" disk: move the volume reading for the next
            // probe. Leaking the guard keeps the new reading in place.
            std::mem::forget(testing::with_reading(volume_with_free(self.free.get())));
        }
        outcome
    }
}

fn exit_status(status: i32, stderr: &str) -> CubeError {
    CubeError::CommandFailed {
        program: "sh".to_string(),
        args: vec!["-c".to_string(), "./clean-this-workspace".to_string()],
        status: Some(status),
        stderr: stderr.to_string(),
    }
}

#[test]
fn above_the_low_water_mark_nothing_is_cleaned() {
    let (low, _) = marks();
    let pool = Pool::seed(3);
    let _volume = testing::with_reading(volume_with_free(low + GIB));
    let runner = CleanupRunner::new(low + GIB, 6 * GIB);

    let report = pool.pass(&runner, false);

    assert!(runner.ran_in().is_empty(), "no pressure, no cleanup, however idle");
    assert!(report.cleaned.is_empty() && report.pressure.is_empty());
    assert_eq!(report.summary(), "no workspace cleanup needed");
}

#[test]
fn under_pressure_cleans_least_recently_used_first_and_stops_at_the_high_water_mark() {
    let (low, high) = marks();
    let pool = Pool::seed(5);
    // 1 GiB under the floor; each cleanup frees 6 GiB. Free space reads
    // low+5, low+11, low+17: the third cleanup clears the high-water mark
    // (low + 16 GiB), so cleanup stops there with two workspaces untouched.
    let start = low - GIB;
    assert!(start + 3 * 6 * GIB >= high && start + 2 * 6 * GIB < high);
    let _volume = testing::with_reading(volume_with_free(start));
    let runner = CleanupRunner::new(start, 6 * GIB);

    let report = pool.pass(&runner, false);

    assert_eq!(
        runner.ran_in(),
        ["mono-agent-001", "mono-agent-002", "mono-agent-003"],
        "least recently used go first, one at a time",
    );
    assert_eq!(report.cleaned.len(), 3);
    assert!(report.cleaned.iter().all(|c| c.bytes_freed == 6 * GIB));
    assert_eq!(report.pressure.len(), 1);
    assert_eq!(pool.cleanup_status(0), None, "a finished cleanup lifts its mark");
    assert!(
        pool.claim(3) && pool.claim(4),
        "the recently used workspaces stayed warm and leasable"
    );
}

#[test]
fn a_leased_workspace_is_never_cleaned() {
    let (low, _) = marks();
    let pool = Pool::seed(3);
    // The least recently used workspace is the one a worker is using.
    assert!(pool.claim(0));
    let start = low - GIB;
    let _volume = testing::with_reading(volume_with_free(start));
    let runner = CleanupRunner::new(start, 30 * GIB);

    let report = pool.pass(&runner, false);

    assert_eq!(runner.ran_in(), ["mono-agent-002"]);
    assert_eq!(report.cleaned.len(), 1);
}

#[test]
fn dirty_and_unhealthy_workspaces_are_left_claimable() {
    let (low, _) = marks();
    let pool = Pool::seed(2);
    pool.store
        .update_workspace_health("mono", &pool.id(0), WorkspaceHealth::Dirty)
        .expect("health");
    let start = low - GIB;
    let _volume = testing::with_reading(volume_with_free(start));
    let runner = CleanupRunner::new(start, 30 * GIB);

    pool.pass(&runner, false);

    assert_eq!(
        runner.ran_in(),
        ["mono-agent-002"],
        "a recovery lease needs the dirty one intact"
    );
}

#[test]
fn dry_run_lists_the_plan_and_changes_nothing() {
    let (low, _) = marks();
    let pool = Pool::seed(3);
    let start = low - GIB;
    let _volume = testing::with_reading(volume_with_free(start));
    let runner = CleanupRunner::new(start, 6 * GIB);

    let report = pool.pass(&runner, true);

    assert!(runner.ran_in().is_empty(), "a dry run runs no cleanup command");
    assert!(report.cleaned.is_empty());
    let planned: Vec<_> = report.planned.iter().map(|p| p.workspace_id.as_str()).collect();
    assert_eq!(planned, ["mono-agent-001", "mono-agent-002", "mono-agent-003"]);
    assert!(report.summary().starts_with("Dry run: would clean up to 3"));
    for index in 0..3 {
        assert_eq!(pool.cleanup_status(index), None);
    }
    assert!(
        audit_events(&pool.tempdir)
            .iter()
            .all(|e| e["event"] != "workspace.cleanup")
    );
}

#[test]
fn a_lease_during_cleanup_never_receives_the_workspace() {
    let (low, _) = marks();
    let pool = Pool::seed(2);
    let start = low - GIB;
    let _volume = testing::with_reading(volume_with_free(start));
    let database_path = pool.database_path.clone();
    let observed = RefCell::new(Vec::new());
    let runner = CleanupRunner::new(start, 30 * GIB).during(|cleaning| {
        // While the command is running, a lease (a second connection, as a
        // separate `cube workspace lease` process has) tries to claim.
        let mut store = Store::open_at(&database_path).expect("second connection");
        let named = store
            .claim_specific_workspace("mono", cleaning, "boss", "named", "lease-named", 5_000, None)
            .expect("claim");
        let any = store
            .claim_workspace("mono", "boss", "any", "lease-any", 5_000, None, Some(cleaning))
            .expect("claim");
        observed.borrow_mut().push((
            cleaning.to_string(),
            named.map(|r| r.workspace_id),
            any.map(|r| r.workspace_id),
        ));
    });

    pool.pass(&runner, false);
    drop(runner);

    let observed = observed.into_inner();
    assert_eq!(observed.len(), 1);
    let (cleaning, named, any) = &observed[0];
    assert_eq!(cleaning, "mono-agent-001");
    assert_eq!(*named, None, "the workspace being cleaned cannot be claimed by name");
    assert_eq!(
        any.as_deref(),
        Some("mono-agent-002"),
        "a lease that would have taken it gets a different workspace instead"
    );
}

#[test]
fn a_busy_workspace_is_skipped_unmarked_and_the_next_one_cleaned() {
    let (low, _) = marks();
    let pool = Pool::seed(3);
    let start = low - GIB;
    let _volume = testing::with_reading(volume_with_free(start));
    let runner = CleanupRunner::new(start, 30 * GIB).outcome(|name| {
        if name == "mono-agent-001" {
            Err(exit_status(75, "a build is running"))
        } else {
            Ok(String::new())
        }
    });

    let report = pool.pass(&runner, false);

    assert_eq!(report.busy, ["mono-agent-001"]);
    assert!(report.failed.is_empty(), "declining is not a failure");
    assert_eq!(runner.ran_in(), ["mono-agent-001", "mono-agent-002"]);
    assert_eq!(pool.cleanup_status(0), None, "a busy workspace is not withheld");
    assert!(pool.claim(0));
}

#[test]
fn a_failed_cleanup_withholds_the_workspace_and_is_retried_without_pressure() {
    let (low, _) = marks();
    let pool = Pool::seed(2);
    let start = low - GIB;
    let _volume = testing::with_reading(volume_with_free(start));
    let runner = CleanupRunner::new(start, 30 * GIB).outcome(|name| {
        if name == "mono-agent-001" {
            Err(exit_status(1, "rm: Permission denied"))
        } else {
            Ok(String::new())
        }
    });

    let report = pool.pass(&runner, false);

    assert_eq!(report.failed.len(), 1);
    assert!(report.failed[0].error.contains("Permission denied"));
    assert_eq!(pool.cleanup_status(0), Some(CleanupStatus::Failed));
    assert!(!pool.claim(0), "a failed cleanup is not leased in a broken state");
    assert_eq!(
        runner.ran_in(),
        ["mono-agent-001", "mono-agent-002"],
        "the pass carried on"
    );

    // Time passes (the failure is older than the retry throttle) and the disk
    // is fine again. The retry does not wait for pressure: the workspace is
    // out of the pool until it succeeds.
    let long_ago = 1_000;
    assert!(
        pool.store
            .begin_workspace_cleanup("mono", &pool.id(0), long_ago, 0)
            .expect("begin")
    );
    pool.store
        .finish_workspace_cleanup("mono", &pool.id(0), CleanupResult::Failed, long_ago)
        .expect("finish");
    let _healthy = testing::with_reading(AMPLE_TEST_VOLUME);
    let retry_runner = CleanupRunner::new(AMPLE_TEST_VOLUME.available_bytes, 0);

    let report = pool.pass(&retry_runner, false);

    assert_eq!(retry_runner.ran_in(), ["mono-agent-001"]);
    assert_eq!(report.cleaned.len(), 1);
    assert_eq!(pool.cleanup_status(0), None);
    assert!(pool.claim(0), "once the retry succeeds the workspace is leasable again");
}

#[test]
fn a_failure_is_not_retried_inside_the_throttle_window() {
    let (low, _) = marks();
    let pool = Pool::seed(1);
    let start = low - GIB;
    let _volume = testing::with_reading(volume_with_free(start));
    let failing = CleanupRunner::new(start, 0).outcome(|_| Err(exit_status(1, "boom")));
    pool.pass(&failing, false);
    assert_eq!(failing.ran_in().len(), 1);

    let again = CleanupRunner::new(start, 0).outcome(|_| Err(exit_status(1, "boom")));
    pool.pass(&again, false);

    assert!(again.ran_in().is_empty(), "a command that just failed is not hammered");
    assert_eq!(pool.cleanup_status(0), Some(CleanupStatus::Failed));
}

#[test]
fn a_workspace_is_not_cleaned_again_until_it_has_been_leased_again() {
    let (low, _) = marks();
    let pool = Pool::seed(1);
    let start = low - GIB;
    let _volume = testing::with_reading(volume_with_free(start));
    let first = CleanupRunner::new(start, GIB);
    pool.pass(&first, false);
    assert_eq!(first.ran_in().len(), 1);

    let _still_low = testing::with_reading(volume_with_free(start));
    let second = CleanupRunner::new(start, GIB);
    pool.pass(&second, false);
    assert!(
        second.ran_in().is_empty(),
        "cleaning an already clean tree frees nothing"
    );

    // A worker leases and releases it: build state exists again.
    pool.store
        .set_workspace_last_activity("mono", &pool.id(0), crate::app::util::current_epoch_s().unwrap() + 60)
        .expect("activity");
    let third = CleanupRunner::new(start, GIB);
    pool.pass(&third, false);
    assert_eq!(third.ran_in().len(), 1);
}

#[test]
fn a_repo_that_declares_no_cleanup_is_left_alone() {
    let (low, _) = marks();
    let pool = Pool::seed(2);
    write_setup_yaml(&pool.workspaces[0], "version: 1\nsteps: []\n");
    let start = low - GIB;
    let _volume = testing::with_reading(volume_with_free(start));
    let runner = CleanupRunner::new(start, 30 * GIB);

    pool.pass(&runner, false);

    assert_eq!(runner.ran_in(), ["mono-agent-002"]);
}

#[test]
fn a_mark_whose_repo_no_longer_declares_cleanup_is_lifted_not_retried_forever() {
    let pool = Pool::seed(1);
    let long_ago = 1_000;
    assert!(
        pool.store
            .begin_workspace_cleanup("mono", &pool.id(0), long_ago, 0)
            .expect("begin")
    );
    pool.store
        .finish_workspace_cleanup("mono", &pool.id(0), CleanupResult::Failed, long_ago)
        .expect("finish");
    write_setup_yaml(&pool.workspaces[0], "version: 1\nsteps: []\n");
    let runner = CleanupRunner::new(AMPLE_TEST_VOLUME.available_bytes, 0);

    pool.pass(&runner, false);

    assert!(runner.ran_in().is_empty(), "there is no command to run");
    assert_eq!(
        pool.cleanup_status(0),
        None,
        "the workspace is not withheld for a step that is gone"
    );
}

#[test]
fn every_cleanup_is_audited_with_workspace_reason_and_bytes() {
    let (low, _) = marks();
    let pool = Pool::seed(2);
    let start = low - GIB;
    let _volume = testing::with_reading(volume_with_free(start));
    let runner = CleanupRunner::new(start, 30 * GIB).outcome(|name| {
        if name == "mono-agent-001" {
            Err(exit_status(1, "boom"))
        } else {
            Ok("removed 1 tree".to_string())
        }
    });

    pool.pass(&runner, false);

    let events: Vec<_> = audit_events(&pool.tempdir)
        .into_iter()
        .filter(|e| e["event"] == "workspace.cleanup")
        .collect();
    assert_eq!(events.len(), 2);
    let cleaned = events.iter().find(|e| e["workspace_id"] == "mono-agent-002").unwrap();
    assert_eq!(cleaned["outcome"], "cleaned");
    assert_eq!(cleaned["reason"], "low_disk");
    assert_eq!(cleaned["trigger"], "manual");
    assert_eq!(cleaned["bytes_freed"], 30 * GIB);
    assert_eq!(cleaned["output"], "removed 1 tree");
    let failed = events.iter().find(|e| e["workspace_id"] == "mono-agent-001").unwrap();
    assert_eq!(failed["outcome"], "failed");
    assert_eq!(failed["bytes_freed"], 0);
}

#[test]
fn workspace_gc_dry_run_reports_the_cleanup_plan_through_the_cli() {
    let (low, _) = marks();
    let pool = Pool::seed(1);
    let _volume = testing::with_reading(volume_with_free(low - GIB));
    let workspace = pool.workspaces[0].clone();
    // The bookmark half of `gc` runs first; it is not under test here.
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(workspace.clone(), "jj", &["git", "fetch"], ""),
        gc_exec_sweep_command(&workspace, "boss/exec_dry_01"),
        gc_pr_noop_command(&workspace),
    ]);

    let result = run_with_dependencies(
        Cli::parse_from(["cube", "workspace", "gc", "--dry-run"]),
        Some(&pool.database_path),
        &runner,
    )
    .expect("gc dry-run");
    runner.assert_exhausted();

    assert!(result.message.contains("would clean up to 1"), "{}", result.message);
    let planned = result.payload["workspace_cleanup"]["planned"].as_array().unwrap();
    assert_eq!(planned.len(), 1);
    assert_eq!(planned[0]["workspace_id"], "mono-agent-001");
    assert_eq!(planned[0]["reason"], "low_disk");
    assert_eq!(pool.cleanup_status(0), None);
}

/// A pool whose workspaces declare no cleanup, so the periodic pass can run
/// against real (and here, absent) commands without anything to execute.
fn throttle_pool() -> Pool {
    let pool = Pool::seed(1);
    write_setup_yaml(&pool.workspaces[0], "version: 1\nsteps: []\n");
    pool
}

#[test]
fn low_free_space_retriggers_the_pool_gc_well_before_24h() {
    use crate::app::gc::{POOL_GC_LAST_AT_KEY, POOL_GC_STARTED_AT_KEY, maybe_trigger_pool_gc};
    let (low, _) = marks();
    let mut pool = throttle_pool();
    let now = crate::app::util::current_epoch_s().unwrap();
    let ten_min_ago = now - 10 * 60;
    pool.store
        .set_pool_metadata_i(POOL_GC_LAST_AT_KEY, ten_min_ago)
        .unwrap();

    // A healthy volume: ten minutes since the last pass is nowhere near 24h.
    {
        let _healthy = testing::with_reading(AMPLE_TEST_VOLUME);
        maybe_trigger_pool_gc(&mut pool.store, Some(&pool.database_path), now).expect("trigger");
        assert!(
            pool.store
                .get_pool_metadata_i(POOL_GC_STARTED_AT_KEY)
                .unwrap()
                .is_none()
        );
    }

    // Below the low-water mark the volume cannot wait a day.
    let _low = testing::with_reading(volume_with_free(low - GIB));
    maybe_trigger_pool_gc(&mut pool.store, Some(&pool.database_path), now).expect("trigger");
    assert_eq!(
        pool.store.get_pool_metadata_i(POOL_GC_STARTED_AT_KEY).unwrap(),
        Some(now),
        "disk pressure should force a pass despite being < 24h since the last one"
    );
}

#[test]
fn a_pass_that_could_not_clean_anything_does_not_pin_the_short_interval() {
    use crate::app::gc::{
        POOL_GC_CLEANUP_PROGRESS_KEY, POOL_GC_LAST_AT_KEY, POOL_GC_STARTED_AT_KEY, maybe_trigger_pool_gc,
    };
    let (low, _) = marks();
    let mut pool = throttle_pool();
    let now = crate::app::util::current_epoch_s().unwrap();
    pool.store
        .set_pool_metadata_i(POOL_GC_LAST_AT_KEY, now - 10 * 60)
        .unwrap();
    // The previous pass saw pressure and walked every candidate without being
    // able to clean one (all busy, all failing): retrying every five minutes
    // would buy nothing.
    pool.store.set_pool_metadata_i(POOL_GC_CLEANUP_PROGRESS_KEY, 0).unwrap();

    let _low = testing::with_reading(volume_with_free(low - GIB));
    maybe_trigger_pool_gc(&mut pool.store, Some(&pool.database_path), now).expect("trigger");

    assert!(
        pool.store
            .get_pool_metadata_i(POOL_GC_STARTED_AT_KEY)
            .unwrap()
            .is_none(),
        "pressure with nothing cleanable falls back to the daily interval"
    );
}

/// The whole path with real commands: `sh -c` in the workspace, the
/// `CUBE_WORKSPACE` environment, and real exit statuses mapped to cleaned /
/// busy / failed.
#[test]
fn real_commands_run_in_the_workspace_and_exit_statuses_map_to_outcomes() {
    use crate::command_runner::RealCommandRunner;

    let (low, _) = marks();
    let pool = Pool::seed(3);
    write_setup_yaml(
        &pool.workspaces[0],
        "version: 1\ncleanup:\n  command: 'echo \"cleaning $CUBE_WORKSPACE\" && touch cleaned.marker'\n",
    );
    write_setup_yaml(
        &pool.workspaces[1],
        "version: 1\ncleanup:\n  command: 'echo in use >&2; exit 75'\n",
    );
    write_setup_yaml(
        &pool.workspaces[2],
        "version: 1\ncleanup:\n  command: 'echo disk on fire >&2; exit 3'\n",
    );
    // Low enough that every command's (zero) effect leaves it low, so all
    // three are tried.
    let _volume = testing::with_reading(volume_with_free(low - GIB));

    let report = pool.pass(&RealCommandRunner, false);

    assert_eq!(report.cleaned.len(), 1);
    assert_eq!(report.cleaned[0].workspace_id, "mono-agent-001");
    assert!(
        pool.workspaces[0].join("cleaned.marker").exists(),
        "the command ran with the workspace as its cwd"
    );
    assert_eq!(report.busy, ["mono-agent-002"]);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].workspace_id, "mono-agent-003");
    assert!(report.failed[0].error.contains("exit 3"), "{}", report.failed[0].error);
    assert!(
        report.failed[0].error.contains("disk on fire"),
        "{}",
        report.failed[0].error
    );
    assert_eq!(pool.cleanup_status(0), None);
    assert_eq!(pool.cleanup_status(1), None);
    assert_eq!(pool.cleanup_status(2), Some(CleanupStatus::Failed));
    let audited = audit_events(&pool.tempdir)
        .into_iter()
        .find(|e| e["event"] == "workspace.cleanup" && e["workspace_id"] == "mono-agent-001")
        .expect("audit event");
    assert!(audited["output"].as_str().unwrap().starts_with("cleaning "));
}
