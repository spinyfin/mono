//! Live status with visible phases and no queueing behind store maintenance.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::command_runner::{CommandInvocation, CommandRunner, RealCommandRunner};
use crate::store::Store;

use super::display::human_workspace_detail;
use super::errors::{CubeError, Result, RunResult};
use super::jj::run_jj_within;
use super::provision::find_workspace_record;

// Leave time for registry access and reporting before a capability preflight's
// 60-second deadline. Recovery commands share this deadline with the live probe.
const STATUS_TIMEOUT: Duration = Duration::from_secs(45);

pub(crate) struct StatusPhase {
    name: &'static str,
    workspace: PathBuf,
    started: Instant,
}

impl StatusPhase {
    pub(crate) fn start(name: &'static str, workspace: &Path) -> Self {
        eprintln!(
            "cube: workspace status phase={name} started workspace={}",
            workspace.display()
        );
        Self {
            name,
            workspace: workspace.to_path_buf(),
            started: Instant::now(),
        }
    }
}

impl Drop for StatusPhase {
    fn drop(&mut self) {
        eprintln!(
            "cube: workspace status phase={} finished elapsed_ms={} workspace={}",
            self.name,
            self.started.elapsed().as_millis(),
            self.workspace.display(),
        );
    }
}

/// Keep the existing stale-working-copy recovery, but bound even its `run()`
/// calls and refuse the maintenance gate for every command in the sequence.
struct StatusRunner<'a> {
    inner: &'a dyn CommandRunner,
    deadline: Instant,
}

impl CommandRunner for StatusRunner<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        self.run_with_timeout(invocation, STATUS_TIMEOUT)
    }

    fn run_with_timeout(&self, invocation: &CommandInvocation, timeout: Duration) -> Result<String> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CubeError::CommandTimedOut {
                program: invocation.program.clone(),
                args: invocation.args.clone(),
                timeout_secs: STATUS_TIMEOUT.as_secs(),
            });
        }
        self.inner
            .run_with_gate_wait(invocation, timeout.min(remaining), Duration::ZERO)
    }
}

pub(super) fn run(
    store: &mut Store,
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    path: &Path,
) -> Result<RunResult> {
    let phase = StatusPhase::start("registry_lookup", path);
    let record =
        find_workspace_record(store, path)?.ok_or_else(|| CubeError::WorkspaceNotFound(path.display().to_string()))?;
    drop(phase);
    let _phase = StatusPhase::start("live_status", path);
    let runner = StatusRunner {
        inner: runner,
        deadline: Instant::now() + STATUS_TIMEOUT,
    };
    let jj_status = run_jj_within(
        &runner,
        database_path,
        &RealCommandRunner::invocation(path, "jj", &["status", "--no-pager"]),
        Some(runner.deadline),
    )?;
    RunResult::new(
        human_workspace_detail(&record, &jj_status),
        json!({ "workspace": record, "jj_status": jj_status }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct RecordingRunner(RefCell<Vec<(Vec<String>, Duration)>>);

    impl CommandRunner for RecordingRunner {
        fn run(&self, _: &CommandInvocation) -> Result<String> {
            panic!("status must never use an unbounded command");
        }

        fn run_with_gate_wait(
            &self,
            invocation: &CommandInvocation,
            timeout: Duration,
            gate_wait: Duration,
        ) -> Result<String> {
            assert_eq!(gate_wait, Duration::ZERO);
            assert!(timeout <= STATUS_TIMEOUT);
            self.0.borrow_mut().push((invocation.args.clone(), timeout));
            if self.0.borrow().len() == 1 {
                Err(CubeError::CommandFailed {
                    program: "jj".into(),
                    args: invocation.args.clone(),
                    status: Some(1),
                    stderr: "The working copy is stale".into(),
                })
            } else {
                Ok("live output".into())
            }
        }
    }

    #[test]
    fn recovery_shares_status_deadline_and_refuses_gate_wait() {
        let dir = tempfile::tempdir().unwrap();
        let inner = RecordingRunner::default();
        let runner = StatusRunner {
            inner: &inner,
            deadline: Instant::now() + STATUS_TIMEOUT,
        };
        let result = run_jj_within(
            &runner,
            Some(&dir.path().join("state.db")),
            &RealCommandRunner::invocation(dir.path(), "jj", &["status", "--no-pager"]),
            Some(runner.deadline),
        )
        .unwrap();
        assert_eq!(result, "live output");
        let calls = inner.0.borrow();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1].0, ["workspace", "update-stale"]);
        assert!(calls[2].1 <= calls[1].1 && calls[1].1 <= calls[0].1);
    }

    #[test]
    fn expired_status_never_starts_recovery() {
        let inner = RecordingRunner::default();
        let runner = StatusRunner {
            inner: &inner,
            deadline: Instant::now(),
        };
        assert!(matches!(
            runner.run(&RealCommandRunner::invocation(
                Path::new("/unused"),
                "jj",
                &["workspace", "update-stale"],
            )),
            Err(CubeError::CommandTimedOut { .. })
        ));
        assert!(inner.0.borrow().is_empty());
    }
}
