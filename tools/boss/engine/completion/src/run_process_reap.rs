//! Reap worker descendants and recorded workspace occupants.
//! Detached tools can outlive their parent and process group. Capture identities
//! before TERM, and escalate only those identities and their live descendants.
//! Both scans run off the async runtime; escalation completes before lease release.

use std::path::Path;
use std::time::{Duration, Instant};

use workspace_procs::{
    OwnedProcess, ReapReport, Scope, Signal, SystemWorkspaceProcessTable, WorkspaceProcessTable, scan, signal_owned,
};

/// Grace between `SIGTERM` and `SIGKILL`. Matches the pane process-group
/// ladder's grace so the two escalate together.
pub const REAP_GRACE: Duration = Duration::from_secs(5);

/// Use only the recorded workspace, never the pane shell's mutable cwd.
pub fn pane_tree_scope(table: &dyn WorkspaceProcessTable, pane_pid: i32, workspace: Option<&Path>) -> Scope {
    let mut scope = workspace.map(Scope::for_workspace).unwrap_or_default();
    if let Ok(shell) = table.snapshot(pane_pid) {
        scope = scope.with_root(&shell);
    }
    scope
}

/// Await the full ladder before the caller can release the workspace.
pub async fn reap_pane_tree(execution_id: &str, pane_pid: i32, workspace: Option<&Path>) {
    if pane_pid <= 1 {
        return;
    }
    let execution_id = execution_id.to_owned();
    let workspace = workspace.map(Path::to_path_buf);
    let outcome = tokio::task::spawn_blocking(move || {
        let table = SystemWorkspaceProcessTable;
        let scope = pane_tree_scope(&table, pane_pid, workspace.as_deref());
        let targets = term_owned(&table, &scope, &execution_id, "pane_release");
        workspace_procs::wait_for_exit(&table, &targets, REAP_GRACE);
        kill_remaining(&table, &targets, &execution_id);
    })
    .await;
    if let Err(err) = outcome {
        tracing::warn!(?err, "run process reap: pane reap task did not run");
    }
}

/// First pass: scan, `SIGTERM` every owned process, log each. Returns what
/// was signalled so the caller knows whether an escalation pass is needed.
pub fn term_owned(
    table: &dyn WorkspaceProcessTable,
    scope: &Scope,
    execution_id: &str,
    reason: &str,
) -> Vec<OwnedProcess> {
    let first = match scan(table, scope) {
        Ok(first) => first,
        Err(err) => {
            tracing::warn!(
                execution_id,
                reason,
                ?err,
                "run process reap: process table unreadable; skipping"
            );
            return Vec::new();
        }
    };
    log_spared(execution_id, reason, &first.spared);
    let sent = signal_owned(table, &first.owned, Signal::Term);
    for s in &sent {
        log_signalled(execution_id, reason, s);
    }
    first.owned
}

/// Escalate captured identities and their live descendants, wait, and report survivors.
pub fn kill_remaining(table: &dyn WorkspaceProcessTable, targets: &[OwnedProcess], execution_id: &str) -> ReapReport {
    let reason = "pane_release_escalation";
    let mut report = ReapReport::default();
    let second = match workspace_procs::escalation_scan(table, targets) {
        Ok(second) => second,
        Err(err) => {
            tracing::warn!(
                execution_id,
                ?err,
                "run process reap: process table unreadable on SIGKILL pass"
            );
            report.error = Some(err.to_string());
            return report;
        }
    };
    if second.owned.is_empty() {
        tracing::debug!(execution_id, "run process reap: everything exited after SIGTERM");
        report.passes = 2;
        return report;
    }
    report.signalled = signal_owned(table, &second.owned, Signal::Kill);
    for s in &report.signalled {
        log_signalled(execution_id, reason, s);
    }
    workspace_procs::wait_for_exit(table, &second.owned, Duration::from_secs(2));
    report.passes = 3;
    let retained: Vec<_> = targets.iter().chain(&second.owned).cloned().collect();
    match workspace_procs::escalation_scan(table, &retained) {
        Ok(third) => {
            report.survivors = third.owned;
            report.spared = third.spared;
        }
        Err(err) => report.error = Some(err.to_string()),
    }
    log_survivors(execution_id, reason, &report.survivors);
    report
}

/// Lease-release reap against the real process table: the full blocking
/// ladder on a worker thread, with the workspace path the terminalizing
/// write captured. `reason` names the completion path for the log lines.
pub async fn reap_workspace(execution_id: &str, workspace_path: &Path, reason: &'static str) -> Option<ReapReport> {
    let started = Instant::now();
    let scope = Scope::for_workspace(workspace_path);
    let execution_id_owned = execution_id.to_owned();
    let outcome = tokio::task::spawn_blocking(move || {
        workspace_procs::reap(&SystemWorkspaceProcessTable, &scope, REAP_GRACE)
            .inspect(|report| log_report(&execution_id_owned, reason, report))
    })
    .await;
    let report = match outcome {
        Ok(Ok(report)) => report,
        Ok(Err(err)) => {
            tracing::warn!(
                execution_id,
                reason,
                ?err,
                "run process reap: process table unreadable; skipping"
            );
            return None;
        }
        Err(err) => {
            tracing::warn!(execution_id, reason, ?err, "run process reap: reap task did not run");
            return None;
        }
    };
    tracing::info!(
        execution_id,
        reason,
        workspace_path = %workspace_path.display(),
        killed = report.killed_pids().len(),
        survivors = report.survivors.len(),
        spared = report.spared.len(),
        passes = report.passes,
        elapsed_ms = started.elapsed().as_millis(),
        "run process reap: workspace sweep complete",
    );
    Some(report)
}

/// Log every line of a finished [`ReapReport`].
pub fn log_report(execution_id: &str, reason: &str, report: &ReapReport) {
    if let Some(error) = &report.error {
        tracing::error!(execution_id, reason, error, "run process reap: verification incomplete");
    }
    for s in &report.signalled {
        log_signalled(execution_id, reason, s);
    }
    log_survivors(execution_id, reason, &report.survivors);
    log_spared(execution_id, reason, &report.spared);
}

fn log_signalled(execution_id: &str, reason: &str, s: &workspace_procs::SignalledProcess) {
    tracing::info!(
        execution_id,
        reason,
        pid = s.pid,
        command = %s.command,
        age_secs = s.age_secs,
        signal = s.signal.as_str(),
        owned_by = %s.ownership.describe(),
        argv = %s.argv,
        "run process reap: killed leftover process",
    );
}

fn log_survivors(execution_id: &str, reason: &str, survivors: &[OwnedProcess]) {
    for s in survivors {
        tracing::error!(
            execution_id,
            reason,
            pid = s.process.pid,
            command = %s.process.command,
            owned_by = %s.ownership.describe(),
            argv = %s.process.argv_summary(),
            "run process reap: process survived SIGKILL",
        );
    }
}

fn log_spared(execution_id: &str, reason: &str, spared: &[workspace_procs::SparedProcess]) {
    for s in spared {
        tracing::info!(
            execution_id,
            reason,
            pid = s.process.pid,
            command = %s.process.command,
            owned_by = %s.ownership.describe(),
            why = %s.reason.describe(),
            "run process reap: left process alone",
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use workspace_procs::Ownership;
    use workspace_procs_testing::{StaticProcessTable, fake_process};

    use super::*;

    const WS: &str = "/pool/mono-agent-003";

    /// Pane shell 100 (cwd = workspace) → agent 200 → tool shell 300 in its
    /// own group, plus an orphaned `( while … ) &` loop 400 re-parented to
    /// launchd and the workspace's bazel server 600.
    fn incident_table() -> StaticProcessTable {
        let mut pane = fake_process(100, 50, "zsh", Some(WS));
        pane.has_controlling_terminal = true;
        let mut agent = fake_process(200, 100, "claude", Some(WS));
        agent.has_controlling_terminal = true;
        let mut tool = fake_process(300, 200, "zsh", Some(WS));
        tool.pgid = 300;
        let mut orphan = fake_process(400, 1, "zsh", Some(WS));
        orphan.pgid = 300;
        let mut server = fake_process(600, 1, "java", Some(WS));
        server.argv = vec!["bazel(mono-agent-003)".into()];
        StaticProcessTable::new(vec![pane, agent, tool, orphan, server], 7)
    }

    #[test]
    fn pane_scope_uses_only_the_recorded_workspace() {
        let table = incident_table();
        let scope = pane_tree_scope(&table, 100, Some(Path::new(WS)));
        assert_eq!(scope.roots, vec![(100, table.snapshot(100).unwrap().start_micros)]);
        assert_eq!(scope.paths, vec![PathBuf::from(WS)]);
        assert!(scope.include_bazel_output_bases);
        // A dead pane shell leaves only the root.
        let mut pane = table.snapshot(100).unwrap();
        pane.cwd = Some(PathBuf::from("/"));
        table.replace(pane);
        let guarded = pane_tree_scope(&table, 100, Some(Path::new(WS)));
        assert_eq!(guarded.paths, vec![PathBuf::from(WS)]);
        let scope = pane_tree_scope(&table, 101, None);
        assert!(scope.roots.is_empty());
        assert!(scope.paths.is_empty());
    }

    #[test]
    fn term_pass_reaches_the_orphan_and_every_descendant_but_not_the_bazel_server() {
        let table = incident_table();
        let scope = pane_tree_scope(&table, 100, Some(Path::new(WS)));
        let termed = term_owned(&table, &scope, "exec_1", "pane_release");
        let pids: Vec<i32> = termed.iter().map(|o| o.process.pid).collect();
        assert_eq!(pids, vec![100, 200, 300, 400]);
        assert_eq!(
            termed.iter().find(|o| o.process.pid == 400).unwrap().ownership,
            Ownership::WorkingDirectory { inside: WS.into() }
        );
        assert_eq!(
            termed.iter().find(|o| o.process.pid == 300).unwrap().ownership,
            Ownership::Descendant { root: 100 }
        );
        assert!(table.is_alive(600));
        assert_eq!(table.signals().len(), 4);
        assert!(table.signals().iter().all(|(_, s)| *s == Signal::Term));
    }

    #[test]
    fn kill_pass_escalates_only_for_what_ignored_term_and_reports_survivors() {
        let table = incident_table();
        table.ignore_term(400);
        let scope = pane_tree_scope(&table, 100, Some(Path::new(WS)));
        let targets = term_owned(&table, &scope, "exec_1", "pane_release");
        // The pane shell is gone now; the scope still carries its path.
        let report = kill_remaining(&table, &targets, "exec_1");
        assert_eq!(report.passes, 3);
        assert_eq!(report.killed_pids(), vec![400]);
        assert!(report.is_clean());
        assert!(!table.is_alive(400));
        assert!(table.is_alive(600));

        let table = incident_table();
        table.immortal(400);
        let scope = pane_tree_scope(&table, 100, Some(Path::new(WS)));
        let targets = term_owned(&table, &scope, "exec_2", "pane_release");
        let report = kill_remaining(&table, &targets, "exec_2");
        assert_eq!(report.survivors.len(), 1);
        assert_eq!(report.survivors[0].process.pid, 400);
    }

    #[test]
    fn kill_pass_is_a_no_op_when_term_sufficed() {
        let table = incident_table();
        let scope = pane_tree_scope(&table, 100, Some(Path::new(WS)));
        let targets = term_owned(&table, &scope, "exec_1", "pane_release");
        let report = kill_remaining(&table, &targets, "exec_1");
        assert_eq!(report.passes, 2);
        assert!(report.signalled.is_empty());
        assert!(report.is_clean());
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    #[tokio::test]
    async fn workspace_entry_point_reaps_detached_loop_and_preserves_bazel_server() {
        let workspace = tempfile::tempdir().unwrap();
        let output = Command::new("sh")
            .args([
                "-c",
                "( while :; do sleep 1; done ) >/dev/null 2>&1 </dev/null & echo $!",
            ])
            .current_dir(workspace.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        let pid: i32 = String::from_utf8(output.stdout).unwrap().trim().parse().unwrap();
        let mut server = Command::new("sleep")
            .arg0("bazel(test-workspace)")
            .arg("300")
            .current_dir(workspace.path())
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let report = reap_workspace("cleanup-test", workspace.path(), "test").await.unwrap();
        // Clean the child even if the subsequent assertions fail.
        let server_survived = server.try_wait().unwrap().is_none();
        let _ = server.kill();
        server.wait().unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert!(report.killed_pids().contains(&pid));
        assert!(SystemWorkspaceProcessTable.snapshot(pid).is_err());
        assert!(server_survived);
        assert!(report.spared.iter().any(|p| p.process.pid == server.id() as i32));
    }
}
