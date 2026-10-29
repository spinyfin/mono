//! Kill every process a worker run leaves behind when the run ends.
//!
//! Tearing down the run's tmux session and signalling its pane shell's
//! process group ([`crate::app::process_signals::reap_worker_process_tree`])
//! is not enough on macOS. An agent's tool runner spawns each tool shell
//! detached into its own process group, a `( … ) &` subshell inside that
//! shell inherits the tool shell's group, and when the tool is cut short the
//! subshell re-parents to launchd. Nothing that walks down from the pane or
//! signals its group ever sees it, so it runs until someone notices — four
//! fork loops ran for four days in a workspace that had been released and
//! removed underneath them. A test binary stranded in a Bazel sandbox after
//! its `bazel test` client was killed is the same shape with a different
//! working directory.
//!
//! This module closes that with the ownership rules in [`workspace_procs`]:
//! a process is the run's if its parent chain still reaches the pane shell
//! (whatever its process group), or if its working directory is inside the
//! run's workspace or that workspace's Bazel output base — the signal that
//! survives re-parenting. The workspace's Bazel server is spared on purpose
//! (it is meant to stay warm and exits on its own idle timeout) and is told
//! apart by the `bazel(<workspace>)` name the client gives it, never by
//! being a `java` process. See `tools/boss/docs/run-process-teardown.md`.
//!
//! Two entry points, for the two moments the engine has what it needs:
//!
//! - [`reap_pane_tree`] at pane release, when the pane pid is known and the
//!   pane shell — started in the workspace by `tmux new-session -c` — can
//!   still tell us the workspace path through its own working directory.
//!   `SIGTERM` is synchronous so the release path stays prompt; the
//!   `SIGKILL` escalation runs detached after a grace period, with a rescan
//!   so a fork loop's newest children are caught too.
//! - [`reap_workspace`] at lease release, with the workspace path the
//!   terminalizing write captured, as the last engine-side pass before the
//!   lease goes back to cube (which runs its own sweep as the safety net).
//!
//! Every signal delivered is logged with pid, command, age and why the
//! process was owned, so a leak is visible in the engine log rather than in
//! `top` four days later.

use std::path::Path;
use std::time::{Duration, Instant};

use workspace_procs::{OwnedProcess, ProcessTable, ReapReport, Scope, Signal, SystemProcessTable, scan, signal_owned};

/// Grace between `SIGTERM` and `SIGKILL`. Matches the pane process-group
/// ladder's grace so the two escalate together.
pub const REAP_GRACE: Duration = Duration::from_secs(5);

/// Build the scope for a pane-rooted reap: the pane pid as the ancestry root
/// plus, while the pane shell is alive, its working directory (the workspace)
/// as a path root. Public for tests.
pub fn pane_tree_scope(table: &dyn ProcessTable, pane_pid: i32) -> Scope {
    let mut scope = Scope {
        roots: Vec::new(),
        paths: Vec::new(),
        include_bazel_output_bases: true,
    }
    .with_root(pane_pid);
    if let Ok(shell) = table.snapshot(pane_pid)
        && let Some(cwd) = shell.cwd
    {
        scope = scope.with_path(cwd);
    }
    scope
}

/// Pane-release reap against the real process table. See the module docs.
pub fn reap_pane_tree(execution_id: &str, pane_pid: i32) {
    if pane_pid <= 1 {
        return;
    }
    let table = SystemProcessTable;
    let scope = pane_tree_scope(&table, pane_pid);
    let termed = term_owned(&table, &scope, execution_id, "pane_release");
    if termed.is_empty() {
        return;
    }
    let execution_id = execution_id.to_owned();
    boss_task_scope::spawn(async move {
        tokio::time::sleep(REAP_GRACE).await;
        let outcome =
            tokio::task::spawn_blocking(move || kill_remaining(&SystemProcessTable, &scope, &execution_id)).await;
        if let Err(err) = outcome {
            tracing::warn!(?err, "run process reap: SIGKILL pass did not run");
        }
    });
}

/// First pass: scan, `SIGTERM` every owned process, log each. Returns what
/// was signalled so the caller knows whether an escalation pass is needed.
pub fn term_owned(table: &dyn ProcessTable, scope: &Scope, execution_id: &str, reason: &str) -> Vec<OwnedProcess> {
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

/// Escalation pass: rescan (the first pass's targets may have spawned more),
/// `SIGKILL` whatever is still owned, wait briefly, and report survivors.
pub fn kill_remaining(table: &dyn ProcessTable, scope: &Scope, execution_id: &str) -> ReapReport {
    let reason = "pane_release_escalation";
    let mut report = ReapReport::default();
    let second = match scan(table, scope) {
        Ok(second) => second,
        Err(err) => {
            tracing::warn!(
                execution_id,
                ?err,
                "run process reap: process table unreadable on SIGKILL pass"
            );
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
    if let Ok(third) = scan(table, scope) {
        report.survivors = third.owned;
        report.spared = third.spared;
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
        workspace_procs::reap(&SystemProcessTable, &scope, REAP_GRACE)
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

    use workspace_procs::{Ownership, StaticProcessTable, fake_process};

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
    fn pane_scope_takes_the_workspace_from_the_pane_shells_working_directory() {
        let table = incident_table();
        let scope = pane_tree_scope(&table, 100);
        assert_eq!(scope.roots, vec![100]);
        assert_eq!(scope.paths, vec![PathBuf::from(WS)]);
        assert!(scope.include_bazel_output_bases);
        // A dead pane shell leaves only the root.
        let scope = pane_tree_scope(&table, 101);
        assert_eq!(scope.roots, vec![101]);
        assert!(scope.paths.is_empty());
    }

    #[test]
    fn term_pass_reaches_the_orphan_and_every_descendant_but_not_the_bazel_server() {
        let table = incident_table();
        let scope = pane_tree_scope(&table, 100);
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
        let scope = pane_tree_scope(&table, 100);
        term_owned(&table, &scope, "exec_1", "pane_release");
        // The pane shell is gone now; the scope still carries its path.
        let report = kill_remaining(&table, &scope, "exec_1");
        assert_eq!(report.passes, 3);
        assert_eq!(report.killed_pids(), vec![400]);
        assert!(report.is_clean());
        assert!(!table.is_alive(400));
        assert!(table.is_alive(600));

        let table = incident_table();
        table.immortal(400);
        let scope = pane_tree_scope(&table, 100);
        term_owned(&table, &scope, "exec_2", "pane_release");
        let report = kill_remaining(&table, &scope, "exec_2");
        assert_eq!(report.survivors.len(), 1);
        assert_eq!(report.survivors[0].process.pid, 400);
    }

    #[test]
    fn kill_pass_is_a_no_op_when_term_sufficed() {
        let table = incident_table();
        let scope = pane_tree_scope(&table, 100);
        term_owned(&table, &scope, "exec_1", "pane_release");
        let report = kill_remaining(&table, &scope, "exec_1");
        assert_eq!(report.passes, 2);
        assert!(report.signalled.is_empty());
        assert!(report.is_clean());
    }
}
