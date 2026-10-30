//! Sweep occupants before release and removal; refuse removal when verification fails.
//! Working-directory ownership catches detached tools after re-parenting.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use workspace_procs::{ReapReport, Scope, SystemWorkspaceProcessTable, WorkspaceProcessTable, reap};

use crate::app::errors::{CubeError, Result};

/// How long a release or removal waits for `SIGTERM` to take effect before
/// escalating to `SIGKILL`. Short: everything a run leaves behind is a tool
/// subprocess or a test binary, not a service with state to flush, and a
/// release is on the engine's completion path.
pub(super) const TEARDOWN_GRACE: Duration = Duration::from_secs(2);

/// Which cube operation asked for the sweep, for the log and audit lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SweepPhase {
    Release,
    Expire,
    Remove,
}

impl SweepPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Expire => "expire",
            Self::Remove => "remove",
        }
    }
}

/// Kill every process a run left inside `workspace_path` and report what
/// happened, retaining verification failures and partial signal reports.
pub(super) fn sweep_workspace_processes(
    table: &dyn WorkspaceProcessTable,
    workspace_id: &str,
    workspace_path: &Path,
    phase: SweepPhase,
) -> ReapReport {
    let report = match reap(table, &Scope::for_workspace(workspace_path), TEARDOWN_GRACE) {
        Ok(report) => report,
        Err(err) => {
            eprintln!(
                "warning: cube {}: {workspace_id}: could not read the process table to sweep {}: {err}",
                phase.as_str(),
                workspace_path.display(),
            );
            ReapReport {
                error: Some(err.to_string()),
                ..ReapReport::default()
            }
        }
    };
    for sent in &report.signalled {
        eprintln!(
            "cube {}: {workspace_id}: killed leftover process {}",
            phase.as_str(),
            sent.describe(),
        );
    }
    for survivor in &report.survivors {
        eprintln!(
            "warning: cube {}: {workspace_id}: pid {} ({}) survived SIGKILL; {}",
            phase.as_str(),
            survivor.process.pid,
            survivor.process.command,
            survivor.ownership.describe(),
        );
    }
    for spared in &report.spared {
        eprintln!(
            "cube {}: {workspace_id}: left pid {} ({}) alone: {}",
            phase.as_str(),
            spared.process.pid,
            spared.process.command,
            spared.reason.describe(),
        );
    }
    report
}

/// [`sweep_workspace_processes`] against the real process table.
pub(super) fn sweep_live_workspace_processes(
    workspace_id: &str,
    workspace_path: &Path,
    phase: SweepPhase,
) -> ReapReport {
    sweep_workspace_processes(&SystemWorkspaceProcessTable, workspace_id, workspace_path, phase)
}

/// Sweep before a directory removal, and refuse the removal when anything
/// owned is still alive afterwards.
pub(super) fn ensure_no_live_processes(
    table: &dyn WorkspaceProcessTable,
    workspace_id: &str,
    workspace_path: &Path,
) -> Result<ReapReport> {
    let report = sweep_workspace_processes(table, workspace_id, workspace_path, SweepPhase::Remove);
    if report.is_clean() {
        return Ok(report);
    }
    Err(CubeError::WorkspaceBusy {
        path: workspace_path.to_path_buf(),
        processes: report.error.clone().unwrap_or_else(|| report.survivor_summary()),
    })
}

/// [`ensure_no_live_processes`] against the real process table.
pub(super) fn ensure_no_live_processes_in(workspace_id: &str, workspace_path: &Path) -> Result<ReapReport> {
    ensure_no_live_processes(&SystemWorkspaceProcessTable, workspace_id, workspace_path)
}

/// Whether the sweep did anything worth an audit line.
pub(super) fn sweep_is_noteworthy(report: &ReapReport) -> bool {
    !report.signalled.is_empty() || !report.survivors.is_empty() || !report.spared.is_empty() || report.error.is_some()
}

/// The sweep as audit-log / `--json` fields.
pub(super) fn sweep_json(report: &ReapReport) -> Value {
    json!({
        "error": report.error,
        "killed": report
            .signalled
            .iter()
            .map(|s| json!({
                "pid": s.pid,
                "command": s.command,
                "argv": s.argv,
                "age_secs": s.age_secs,
                "signal": s.signal.as_str(),
                "owned_by": s.ownership.describe(),
            }))
            .collect::<Vec<_>>(),
        "survivors": report
            .survivors
            .iter()
            .map(|s| json!({
                "pid": s.process.pid,
                "command": s.process.command,
                "argv": s.process.argv_summary(),
                "owned_by": s.ownership.describe(),
            }))
            .collect::<Vec<_>>(),
        "spared": report
            .spared
            .iter()
            .map(|s| json!({
                "pid": s.process.pid,
                "command": s.process.command,
                "reason": s.reason.describe(),
            }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use workspace_procs::Signal;
    use workspace_procs_testing::{StaticProcessTable, fake_process};

    use super::*;

    const WS: &str = "/pool/mono-agent-007";

    fn table() -> StaticProcessTable {
        let mut server = fake_process(60, 1, "java", Some(WS));
        server.argv = vec!["bazel(mono-agent-007)".into()];
        StaticProcessTable::new(
            vec![
                fake_process(40, 1, "zsh", Some(WS)),
                fake_process(41, 40, "sh", Some(WS)),
                server,
                fake_process(80, 1, "other", Some("/elsewhere")),
            ],
            900,
        )
    }

    #[test]
    fn release_sweep_kills_occupants_spares_the_bazel_server_and_reports_them() {
        let table = table();
        let report = sweep_workspace_processes(&table, "mono-agent-007", Path::new(WS), SweepPhase::Release);
        assert_eq!(report.killed_pids(), vec![40, 41]);
        assert!(report.is_clean());
        assert!(table.is_alive(60));
        assert!(table.is_alive(80));
        assert!(sweep_is_noteworthy(&report));
        let json = sweep_json(&report);
        assert_eq!(json["killed"].as_array().unwrap().len(), 2);
        assert_eq!(json["killed"][0]["pid"], 40);
        assert_eq!(json["killed"][0]["signal"], "SIGTERM");
        assert_eq!(json["spared"][0]["pid"], 60);
        assert!(json["survivors"].as_array().unwrap().is_empty());
    }

    #[test]
    fn removal_proceeds_once_a_term_resistant_occupant_is_killed() {
        let table = table();
        table.ignore_term(40);
        let report = ensure_no_live_processes(&table, "mono-agent-007", Path::new(WS)).unwrap();
        assert_eq!(
            table.signals(),
            vec![(40, Signal::Term), (41, Signal::Term), (40, Signal::Kill)]
        );
        assert!(report.is_clean());
        assert_eq!(report.killed_pids(), vec![40, 41]);
    }

    #[test]
    fn removal_is_refused_while_an_occupant_survives_sigkill() {
        let table = table();
        table.immortal(40);
        let err = ensure_no_live_processes(&table, "mono-agent-007", Path::new(WS)).unwrap_err();
        match &err {
            CubeError::WorkspaceBusy { path, processes } => {
                assert_eq!(path, Path::new(WS));
                assert_eq!(processes, "40 (zsh)");
            }
            other => panic!("unexpected error {other:?}"),
        }
        assert!(table.is_alive(40), "cube does not pretend a survivor is gone");
        assert!(!table.is_alive(41));
        assert!(table.signals().contains(&(40, Signal::Kill)));
    }

    #[test]
    fn removal_error_names_the_survivors() {
        let report = ReapReport {
            survivors: vec![workspace_procs::OwnedProcess {
                process: fake_process(40, 1, "zsh", Some(WS)),
                ownership: workspace_procs::Ownership::WorkingDirectory {
                    inside: PathBuf::from(WS),
                },
            }],
            ..ReapReport::default()
        };
        let err = CubeError::WorkspaceBusy {
            path: PathBuf::from(WS),
            processes: report.error.clone().unwrap_or_else(|| report.survivor_summary()),
        };
        let text = err.to_string();
        assert!(text.contains("40 (zsh)"), "{text}");
        assert!(text.contains(WS), "{text}");
    }

    #[test]
    fn a_quiet_workspace_is_not_noteworthy() {
        let table = table();
        let report = sweep_workspace_processes(
            &table,
            "mono-agent-008",
            Path::new("/pool/mono-agent-008"),
            SweepPhase::Remove,
        );
        assert!(!sweep_is_noteworthy(&report));
        assert!(table.signals().is_empty());
        ensure_no_live_processes(&table, "mono-agent-008", Path::new("/pool/mono-agent-008")).unwrap();
    }
}

#[cfg(test)]
mod verification_tests {
    use super::*;
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use workspace_procs::{LiveProcess, Signal};
    use workspace_procs_testing::{StaticProcessTable, fake_process};

    struct FailingTable {
        inner: StaticProcessTable,
        calls: AtomicUsize,
        fail_at: usize,
    }
    impl WorkspaceProcessTable for FailingTable {
        fn list(&self) -> io::Result<Vec<LiveProcess>> {
            if self.calls.fetch_add(1, Ordering::SeqCst) >= self.fail_at {
                Err(io::Error::other("process list unavailable"))
            } else {
                self.inner.list()
            }
        }
        fn snapshot(&self, pid: i32) -> io::Result<LiveProcess> {
            self.inner.snapshot(pid)
        }
        fn signal(&self, pid: i32, signal: Signal) -> io::Result<()> {
            self.inner.signal(pid, signal)
        }
        fn own_pid(&self) -> i32 {
            900
        }
    }
    fn failing(fail_at: usize) -> FailingTable {
        FailingTable {
            inner: StaticProcessTable::new(vec![fake_process(40, 1, "tool", Some("/pool/test"))], 900),
            calls: AtomicUsize::new(0),
            fail_at,
        }
    }
    #[test]
    fn removal_refuses_initial_and_post_term_scan_failures() {
        for fail_at in [0, 1, 2] {
            let error = ensure_no_live_processes(&failing(fail_at), "test", Path::new("/pool/test")).unwrap_err();
            assert!(error.to_string().contains("process list unavailable"), "{error}");
        }
    }
    #[test]
    fn post_term_failure_retains_signal_audit() {
        let report = sweep_workspace_processes(&failing(1), "test", Path::new("/pool/test"), SweepPhase::Remove);
        assert_eq!(report.killed_pids(), vec![40]);
        assert!(report.error.is_some());
        assert!(sweep_is_noteworthy(&report));
        assert_eq!(sweep_json(&report)["error"], "process list unavailable");
    }
    #[test]
    fn spared_server_is_noteworthy_on_removal() {
        let mut server = fake_process(60, 1, "java", Some("/pool/test"));
        server.argv = vec!["bazel(test)".into()];
        let table = StaticProcessTable::new(vec![server], 900);
        let report = ensure_no_live_processes(&table, "test", Path::new("/pool/test")).unwrap();
        assert!(sweep_is_noteworthy(&report));
        assert_eq!(sweep_json(&report)["spared"][0]["pid"], 60);
    }
}
