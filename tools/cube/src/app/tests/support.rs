use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite;
use tempfile::TempDir;

use crate::command_runner::{CommandInvocation, CommandRunner};
use crate::reuse_guard;

use crate::app::errors::{CubeError, Result};

/// Mutex serialising tests that touch the process environment — either
/// mutating it (PATH, CUBE_CHECKLEFT_BIN) or depending on it, such as any
/// test that spawns a binary by bare name and needs PATH intact.
/// `env::set_var` is not thread-safe and PATH is process-global, so a test
/// that reads it must hold this lock too, not just the ones that write it.
pub(super) static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(super) fn with_database_path() -> (TempDir, std::path::PathBuf) {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let database_path = tempdir.path().join("state.db");
    (tempdir, database_path)
}

pub(super) fn jj_status_clean() -> &'static str {
    "The working copy is clean\nWorking copy : abc1234 (empty) main"
}

pub(super) fn jj_status_dirty() -> &'static str {
    "Working copy changes:\nM tools/cube/src/app.rs\n\nWorking copy : abc1234 some change"
}

pub(super) fn jj_status_conflicted(bookmark: &str) -> String {
    format!(
        "The working copy is clean\nWorking copy : abc1234 (empty) main\nThese bookmarks have conflicts:\n  {bookmark}\n  Use `jj bookmark list` to see details."
    )
}

pub(super) fn write_setup_yaml(workspace_path: &std::path::Path, body: &str) {
    let path = workspace_path.join(".cube/setup.yaml");
    std::fs::create_dir_all(path.parent().unwrap()).expect("setup dir");
    std::fs::write(&path, body).expect("setup yaml");
}

pub(super) fn lease_runner_for(workspace_path: &std::path::Path, head: &str) -> FakeRunner {
    FakeRunner::new(vec![
        ExpectedCommand::ok(
            workspace_path.to_path_buf(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            workspace_path.to_path_buf(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            workspace_path.to_path_buf(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            workspace_path.to_path_buf(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            head,
        ),
    ])
}

/// The dirty-reclaim guard's first probe: what `@` is and what bookmarks
/// its parents carry. `output` is the tab-separated line
/// [`reuse_guard::HEAD_STATUS_TEMPLATE`] produces — build it with
/// [`head_status_output`].
pub(super) fn head_status_command(workspace_path: &std::path::Path, output: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        workspace_path.to_path_buf(),
        "jj",
        &["log", "--no-graph", "-r", "@", "-T", reuse_guard::HEAD_STATUS_TEMPLATE],
        output,
    )
}

/// Render a head-status line the way `jj log` would.
pub(super) fn head_status_output(change_id: &str, is_empty: bool, rendered: &str, local: &str, remote: &str) -> String {
    format!("{change_id}\t{is_empty}\t{rendered}\t{local}\t{remote}")
}

/// The guard's second probe: the commits a reset would orphan. Only issued
/// when the first probe didn't already settle it (`@` empty on main).
/// An empty `output` means nothing would be lost.
pub(super) fn unpushed_probe_command(workspace_path: &std::path::Path, output: &str) -> ExpectedCommand {
    let revset = reuse_guard::unpushed_work_revset("main");
    ExpectedCommand::ok(
        workspace_path.to_path_buf(),
        "jj",
        &[
            "log",
            "--no-graph",
            "-n",
            reuse_guard::UNPUSHED_PROBE_LIMIT,
            "-r",
            &revset,
            "-T",
            reuse_guard::UNPUSHED_PROBE_TEMPLATE,
        ],
        output,
    )
}

/// Returns an expected gc-log command that reports no consumed bookmarks.
/// Add to any release runner after `jj new main@origin` to satisfy the gc check.
pub(super) fn gc_noop_command(workspace_path: &std::path::Path) -> ExpectedCommand {
    ExpectedCommand::ok(
        workspace_path.to_path_buf(),
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
    )
}

/// Command that satisfies the `gc_collect_closed_pr_bookmarks` remote-list
/// probe with a non-GitHub remote, causing the pr/* sweep to skip.
pub(super) fn gc_pr_remote_noop_command(workspace_path: &std::path::Path) -> ExpectedCommand {
    ExpectedCommand::ok(
        workspace_path.to_path_buf(),
        "jj",
        &["git", "remote", "list"],
        "origin /local/path/to/mirror\n",
    )
}

/// The release-time reuse-guard probe that says "this working copy is
/// safe to reset": an empty `@` sitting on `main`, which
/// `needs_unpushed_probe` settles without a second jj call. Every
/// release expectation needs this between `jj git fetch` and the reset.
pub(super) fn release_guard_reusable_command(workspace_path: &std::path::Path) -> ExpectedCommand {
    head_status_command(
        workspace_path,
        &head_status_output("releaseok", true, "main", "main", "main"),
    )
}

/// Standard release runner: fetch, reuse-guard probe, reset, then gc-noop
/// (exec + pr sweeps).
pub(super) fn release_runner_for(workspace_path: &std::path::Path) -> FakeRunner {
    FakeRunner::new(vec![
        ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        release_guard_reusable_command(workspace_path),
        ExpectedCommand::ok(
            workspace_path.to_path_buf(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            workspace_path.to_path_buf(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["new", "main@origin"], ""),
        gc_noop_command(workspace_path),
        gc_pr_remote_noop_command(workspace_path),
    ])
}

pub(super) fn lease_runner_with_setup(
    workspace_path: &std::path::Path,
    head: &str,
    setup_steps: Vec<ExpectedCommand>,
) -> FakeRunner {
    let mut commands = vec![
        ExpectedCommand::ok(
            workspace_path.to_path_buf(),
            "jj",
            &["status", "--no-pager"],
            "The working copy is clean",
        ),
        ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["git", "fetch"], ""),
        ExpectedCommand::ok(
            workspace_path.to_path_buf(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            workspace_path.to_path_buf(),
            "jj",
            &["bookmark", "set", "main", "-r", "main@origin", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(workspace_path.to_path_buf(), "jj", &["new", "main@origin"], ""),
        ExpectedCommand::ok(
            workspace_path.to_path_buf(),
            "jj",
            &["log", "--no-graph", "-r", "@", "-T", "commit_id.short()"],
            head,
        ),
    ];
    commands.extend(setup_steps);
    FakeRunner::new(commands)
}

/// Deterministic canonical-source path for a seeded repo, derived from the
/// test's `workspace_root` so auto-create tests can reference the
/// `jj workspace add -R <source>` argument without threading extra state.
pub(super) fn mono_source_path(workspace_root: &std::path::Path) -> std::path::PathBuf {
    workspace_root.parent().unwrap().join("source").join("mono")
}

pub(super) fn seed_mono_repo(workspace_root: &std::path::Path, database_path: &std::path::Path) {
    // The shared-store model requires a materialised canonical repo for the
    // pool to attach to; `repo ensure` always creates one. Mirror that here so
    // auto-create exercises `jj workspace add` against a present `.jj/` store.
    let source = mono_source_path(workspace_root);
    std::fs::create_dir_all(source.join(".jj")).expect("seed canonical source .jj");
    let store = crate::store::Store::open_at(database_path).expect("store");
    store
        .upsert_repo(&crate::metadata::RepoRecord {
            repo: "mono".to_string(),
            origin: "git@github.com:spinyfin/mono.git".to_string(),
            main_branch: "main".to_string(),
            workspace_root: workspace_root.to_path_buf(),
            workspace_prefix: "mono-agent-".to_string(),
            source: Some(source),
            clone_command: None,
        })
        .expect("seed repo");
}

/// Sets `lease_expires_at_epoch_s` directly in the SQLite store.
/// Used by reconcile tests to age a lease past its TTL without having
/// to wait wall-clock seconds.
pub(super) fn force_lease_expiry(database_path: &std::path::Path, lease_id: &str, epoch_s: i64) {
    let conn = rusqlite::Connection::open(database_path).expect("sqlite open");
    let updated = conn
        .execute(
            "UPDATE workspaces SET lease_expires_at_epoch_s = ?1 WHERE lease_id = ?2",
            rusqlite::params![epoch_s, lease_id],
        )
        .expect("force expiry");
    assert_eq!(updated, 1, "expected exactly one row updated by force_lease_expiry");
}

pub(super) fn audit_events(tempdir: &TempDir) -> Vec<serde_json::Value> {
    let audit_dir = tempdir.path().join("audit");
    let Ok(read) = std::fs::read_dir(&audit_dir) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for entry in read.flatten() {
        let contents = std::fs::read_to_string(entry.path()).expect("audit content");
        for line in contents.lines() {
            events.push(serde_json::from_str(line).expect("audit line"));
        }
    }
    events
}

#[derive(Default)]
pub(super) struct FakeRunner {
    pub(super) expectations: RefCell<VecDeque<ExpectedCommand>>,
    /// Every timeout the code under test handed to
    /// [`CommandRunner::run_with_timeout`], in call order. Lets a test assert
    /// that a caller's wall-clock budget actually reached the subprocess
    /// bound, rather than the default 120s network timeout.
    observed_timeouts: RefCell<Vec<Duration>>,
}

impl FakeRunner {
    pub(super) fn new(expectations: Vec<ExpectedCommand>) -> Self {
        Self {
            expectations: RefCell::new(expectations.into()),
            observed_timeouts: RefCell::new(Vec::new()),
        }
    }

    pub(super) fn assert_exhausted(&self) {
        assert!(
            self.expectations.borrow().is_empty(),
            "unexpected commands remaining: {:?}",
            self.expectations.borrow()
        );
    }

    /// The timeouts passed to `run_with_timeout`, in call order.
    pub(super) fn observed_timeouts(&self) -> Vec<Duration> {
        self.observed_timeouts.borrow().clone()
    }

    fn next_expectation(&self, invocation: &CommandInvocation) -> ExpectedCommand {
        let expected = self
            .expectations
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected command invocation: {invocation:?}"));
        assert_eq!(expected.cwd, invocation.cwd);
        assert_eq!(expected.program, invocation.program);
        assert_eq!(expected.args, invocation.args);
        if let Some(path) = &expected.creates_dir {
            std::fs::create_dir_all(path).expect("create simulated workspace dir");
        }
        expected
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        self.next_expectation(invocation).result
    }

    /// Honours `timeout` against the expectation's simulated `duration`, so a
    /// test can exercise the "this subprocess outlives the caller's budget"
    /// path the way [`crate::command_runner::RealCommandRunner`] does: sleep
    /// until the bound, kill, and report [`CubeError::CommandTimedOut`]. The
    /// sleep is real (bounded by `timeout`) so wall-clock-dependent logic —
    /// notably "is there budget left for a retry?" — behaves as it would in
    /// production.
    fn run_with_timeout(&self, invocation: &CommandInvocation, timeout: Duration) -> Result<String> {
        self.observed_timeouts.borrow_mut().push(timeout);
        let expected = self.next_expectation(invocation);
        if expected.duration > timeout {
            std::thread::sleep(timeout);
            return Err(CubeError::CommandTimedOut {
                program: invocation.program.clone(),
                args: invocation.args.clone(),
                timeout_secs: timeout.as_secs(),
            });
        }
        if !expected.duration.is_zero() {
            std::thread::sleep(expected.duration);
        }
        expected.result
    }
}

#[derive(Debug, bon::Builder)]
#[builder(on(String, into))]
pub(super) struct ExpectedCommand {
    pub(super) cwd: PathBuf,
    pub(super) program: String,
    pub(super) args: Vec<String>,
    pub(super) result: Result<String>,
    pub(super) creates_dir: Option<PathBuf>,
    /// How long this command pretends to take. Zero (the default) is the
    /// instant-completion behaviour every existing expectation relies on.
    #[builder(default = Duration::ZERO)]
    pub(super) duration: Duration,
}

impl ExpectedCommand {
    pub(super) fn ok(cwd: PathBuf, program: &str, args: &[&str], stdout: &str) -> Self {
        Self {
            cwd,
            program: program.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            result: Ok(stdout.to_string()),
            creates_dir: None,
            duration: Duration::ZERO,
        }
    }

    pub(super) fn creating_dir(mut self, path: PathBuf) -> Self {
        self.creates_dir = Some(path);
        self
    }

    /// Make this command take `duration` of simulated wall clock. A command
    /// whose duration exceeds the timeout it is run under is killed and
    /// reported as [`CubeError::CommandTimedOut`], exactly as the real runner
    /// would — which is how a test can stand in for a slow remote.
    pub(super) fn taking(mut self, duration: Duration) -> Self {
        self.duration = duration;
        self
    }

    /// Build an expectation for the shared-store auto-create's
    /// `jj -R <source> workspace add --name <id> <staging>` invocation, run
    /// in `workspace_root` and materialising the dotted staging dir. Replaces
    /// the old independent-clone provisioning sequence.
    pub(super) fn workspace_add(
        workspace_root: PathBuf,
        source: &std::path::Path,
        workspace_id: &str,
        staging: &std::path::Path,
    ) -> Self {
        Self::ok(
            workspace_root,
            "jj",
            &[
                "-R",
                &source.display().to_string(),
                "workspace",
                "add",
                "--name",
                workspace_id,
                &staging.display().to_string(),
            ],
            "",
        )
        .creating_dir(staging.to_path_buf())
    }

    /// Convenience for the many `mono` auto-create tests: derives the
    /// canonical source (via [`mono_source_path`]) and the workspace id (from
    /// the `.incoming-<id>` staging basename) so callers pass only the two
    /// vars every such test already has in scope.
    pub(super) fn workspace_add_mono(workspace_root: &std::path::Path, staging: &std::path::Path) -> Self {
        let source = mono_source_path(workspace_root);
        let workspace_id = staging
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix(".incoming-"))
            .expect("staging dir must be named .incoming-<workspace-id>");
        Self::workspace_add(workspace_root.to_path_buf(), &source, workspace_id, staging)
    }

    /// Build an expectation for the `git ls-remote --symref <origin> HEAD`
    /// default-branch probe, returning the symref output that points `HEAD`
    /// at `branch` (the shape `git` actually prints).
    pub(super) fn ls_remote_symref(cwd: PathBuf, origin: &str, branch: &str) -> Self {
        Self::ok(
            cwd,
            "git",
            &["ls-remote", "--symref", origin, "HEAD"],
            &format!(
                "ref: refs/heads/{branch}\tHEAD\n\
                     0000000000000000000000000000000000000000\tHEAD"
            ),
        )
    }

    /// Build an expectation that simulates jj's stale-working-copy
    /// failure. The wording matches what `cube`'s `run_jj` wrapper
    /// looks for via `JJ_STALE_SIGNATURE`.
    pub(super) fn stale(cwd: PathBuf, program: &str, args: &[&str]) -> Self {
        let args_owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        Self {
            cwd,
            program: program.to_string(),
            args: args_owned.clone(),
            result: Err(CubeError::CommandFailed {
                program: program.to_string(),
                args: args_owned,
                status: Some(1),
                stderr: "Error: The working copy is stale (not updated since operation \
                             0123456789ab). Run `jj workspace update-stale` to update it."
                    .to_string(),
            }),
            creates_dir: None,
            duration: Duration::ZERO,
        }
    }

    /// Build an expectation that simulates jj's op-log divergence
    /// failure. The wording matches `JJ_OP_DIVERGED_SIGNATURE` and
    /// is recovered by `jj workspace update-stale`, same as `stale`.
    pub(super) fn op_diverged(cwd: PathBuf, program: &str, args: &[&str]) -> Self {
        let args_owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        Self {
            cwd,
            program: program.to_string(),
            args: args_owned.clone(),
            result: Err(CubeError::CommandFailed {
                program: program.to_string(),
                args: args_owned,
                status: Some(255),
                stderr: "Internal error: The repo was loaded at operation a44a2f689f46, \
                             which seems to be a sibling of the working copy's operation \
                             17fb914fb03f"
                    .to_string(),
            }),
            creates_dir: None,
            duration: Duration::ZERO,
        }
    }

    /// Build an expectation that simulates `jj bookmark track
    /// <name>@origin` failing because the named remote bookmark
    /// does not exist in the repo. Matches `JJ_NO_REMOTE_BOOKMARK_SIGNATURE`
    /// and is the expected outcome for whichever of `main@origin` /
    /// `master@origin` this repo does not use.
    pub(super) fn no_such_remote_bookmark(cwd: PathBuf, program: &str, args: &[&str]) -> Self {
        let args_owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        let bookmark = args_owned.last().cloned().unwrap_or_default();
        Self {
            cwd,
            program: program.to_string(),
            args: args_owned.clone(),
            result: Err(CubeError::CommandFailed {
                program: program.to_string(),
                args: args_owned,
                status: Some(1),
                stderr: format!("Error: No such remote bookmark: {bookmark}"),
            }),
            creates_dir: None,
            duration: Duration::ZERO,
        }
    }

    /// Build an expectation that simulates `jj bookmark set <name> -r
    /// <name>@origin` failing because the remote-tracking target does
    /// not resolve. Matches `JJ_REVISION_DOESNT_EXIST_SIGNATURE` and is
    /// the wording jj prints when a repo's recorded default branch has
    /// no matching `@origin` bookmark — tolerated by the on-lease
    /// fast-forward.
    pub(super) fn revision_doesnt_exist(cwd: PathBuf, program: &str, args: &[&str]) -> Self {
        let args_owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        let target = args_owned
            .iter()
            .rev()
            .find(|a| a.contains("@origin"))
            .cloned()
            .unwrap_or_default();
        Self {
            cwd,
            program: program.to_string(),
            args: args_owned.clone(),
            result: Err(CubeError::CommandFailed {
                program: program.to_string(),
                args: args_owned,
                status: Some(1),
                stderr: format!("Error: Revision `{target}` doesn't exist"),
            }),
            creates_dir: None,
            duration: Duration::ZERO,
        }
    }

    /// Build an expectation that simulates jj's "no jj repo" failure.
    /// The wording matches `JJ_NO_JJ_REPO_SIGNATURE` and is recovered
    /// by `jj git init --colocate` when `.git/` is present.
    pub(super) fn no_jj_repo(cwd: PathBuf, program: &str, args: &[&str]) -> Self {
        let args_owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        Self {
            cwd,
            program: program.to_string(),
            args: args_owned.clone(),
            result: Err(CubeError::CommandFailed {
                program: program.to_string(),
                args: args_owned,
                status: Some(1),
                stderr: "Error: There is no jj repo in \".\"\n\
                             Hint: It looks like this is a git repo. You can create a jj repo \
                             backed by it by running this:\njj git init --colocate"
                    .to_string(),
            }),
            creates_dir: None,
            duration: Duration::ZERO,
        }
    }
}
