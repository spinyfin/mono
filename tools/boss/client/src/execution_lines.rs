//! Human-readable detail lines for execution rows in `boss task show`,
//! `boss task executions` and `bossctl work executions`.
//!
//! The JSON views already carry every field verbatim; these lines are the
//! text-mode answer to "why is this run held?" and "why did it fail?" so a
//! reader does not have to fall back to reading the engine database.

use boss_protocol::{WorkExecution, WorkRun};

use crate::time_fmt::format_stored_epoch;

/// Longest error text printed inline. Longer text is cut and marked with an
/// ellipsis; the full text is always in the `--json` output.
const MAX_ERROR_CHARS: usize = 200;

/// Render a stored epoch-seconds value as an ISO-8601 local timestamp, or
/// `-` when the value is unset.
pub fn display_time(raw: Option<&str>) -> String {
    raw.map_or_else(|| "-".to_owned(), format_stored_epoch)
}

/// Collapse `text` onto one line and cut it to [`MAX_ERROR_CHARS`] characters.
pub fn truncate_one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_ERROR_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(MAX_ERROR_CHARS).collect();
    format!("{cut}…")
}

/// `waiting: <reason> since <time>` for an execution that has not finished and
/// that the dispatcher is currently holding. `None` for terminal executions
/// (a stale wait reason on a finished row is not a wait) and for executions
/// that are not being held.
pub fn waiting_line(exec: &WorkExecution) -> Option<String> {
    if exec.status.is_terminal() {
        return None;
    }
    let reason = exec.dispatch_wait_reason.as_deref()?;
    let blocker = exec
        .dispatch_wait_blocker_id
        .as_deref()
        .map(|id| format!(" (blocked by {id})"))
        .unwrap_or_default();
    let since = exec
        .dispatch_wait_since
        .as_deref()
        .map(|raw| format!(" since {}", format_stored_epoch(raw)))
        .unwrap_or_default();
    Some(format!("waiting: {reason}{blocker}{since}"))
}

/// `error: <text>` for a failed execution, and `last error: <text>` for a live
/// or queued one that has recorded a pre-start failure and is retrying. Falls
/// back to the newest failed run's error text when the execution row itself
/// carries none.
pub fn error_line(exec: &WorkExecution, runs: &[WorkRun]) -> Option<String> {
    let failed = matches!(exec.status, boss_protocol::ExecutionStatus::Failed);
    let text = exec
        .last_error
        .as_deref()
        .or_else(|| {
            if failed {
                runs.iter().rev().find_map(|run| run.error_text.as_deref())
            } else {
                None
            }
        })
        .filter(|text| !text.trim().is_empty())?;
    if !failed && exec.status.is_terminal() {
        return None;
    }
    let label = if failed { "error" } else { "last error" };
    Some(format!("{label}: {}", truncate_one_line(text)))
}

/// One `flags:` line naming how the execution was created and launched, or
/// `None` when it is an ordinary execution with nothing to report.
pub fn provenance_line(exec: &WorkExecution) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(created_via) = exec.created_via.as_deref() {
        parts.push(format!("created_via={created_via}"));
    }
    if let Some(disposition) = exec.progression_disposition.as_deref() {
        parts.push(format!("progression_disposition={disposition}"));
    }
    for (set, name) in [
        (exec.launch_failed, "launch_failed"),
        (exec.force_launched, "force_launched"),
        (exec.operator_forced_admission, "operator_forced_admission"),
    ] {
        if set {
            parts.push(name.to_owned());
        }
    }
    (!parts.is_empty()).then(|| format!("flags: {}", parts.join(" ")))
}

/// The wait, error and provenance lines for one execution, in print order.
pub fn execution_detail_lines(exec: &WorkExecution, runs: &[WorkRun]) -> Vec<String> {
    [waiting_line(exec), error_line(exec, runs), provenance_line(exec)]
        .into_iter()
        .flatten()
        .collect()
}

/// The text rows `bossctl work executions` prints for one execution: a
/// summary line, the PR URL when there is one, then the detail lines
/// indented beneath.
pub fn history_row_lines(exec: &WorkExecution, runs: &[WorkRun]) -> Vec<String> {
    let host = exec.host_id.as_deref().unwrap_or("local");
    let workspace = exec.cube_workspace_id.as_deref().unwrap_or("-");
    let mut lines = vec![format!(
        "{}  [{}]  kind={}  host={}  workspace={}  created={}  started={}  finished={}",
        exec.id,
        exec.status,
        exec.kind,
        host,
        workspace,
        display_time(Some(&exec.created_at)),
        display_time(exec.started_at.as_deref()),
        display_time(exec.finished_at.as_deref()),
    )];
    if let Some(pr_url) = &exec.pr_url {
        lines.push(format!("  pr_url: {pr_url}"));
    }
    lines.extend(
        execution_detail_lines(exec, runs)
            .into_iter()
            .map(|line| format!("  {line}")),
    );
    lines
}

#[cfg(test)]
mod tests {
    use boss_protocol::{ExecutionKind, ExecutionStatus};

    use super::*;

    fn execution(status: ExecutionStatus) -> WorkExecution {
        WorkExecution::builder()
            .id("exec_1")
            .work_item_id("task_1")
            .kind(ExecutionKind::ChoreImplementation)
            .status(status)
            .repo_remote_url("git@github.com:o/r.git")
            .created_at("1700000000")
            .build()
    }

    fn run_with_error(error: Option<&str>) -> WorkRun {
        WorkRun::builder()
            .id("run_1")
            .agent_id("agent")
            .execution_id("exec_1")
            .created_at("1700000000")
            .status("failed")
            .maybe_error_text(error.map(str::to_owned))
            .build()
    }

    #[test]
    fn waiting_line_names_reason_blocker_and_iso_since_for_a_held_execution() {
        let mut exec = execution(ExecutionStatus::Ready);
        exec.dispatch_wait_reason = Some("awaiting_predecessor_preservation".into());
        exec.dispatch_wait_blocker_id = Some("exec_0".into());
        exec.dispatch_wait_since = Some("1700000000".into());
        let line = waiting_line(&exec).expect("a held execution has a waiting line");
        assert!(
            line.starts_with("waiting: awaiting_predecessor_preservation (blocked by exec_0) since 2023-11-1"),
            "{line}"
        );
        assert!(
            !line.contains("1700000000"),
            "epoch seconds must not be printed: {line}"
        );
    }

    #[test]
    fn waiting_line_is_absent_for_terminal_and_unheld_executions() {
        let mut stale = execution(ExecutionStatus::Failed);
        stale.dispatch_wait_reason = Some("pool_exhausted".into());
        assert_eq!(waiting_line(&stale), None);
        assert_eq!(waiting_line(&execution(ExecutionStatus::Ready)), None);
    }

    #[test]
    fn error_line_truncates_and_flattens_a_failed_execution_error() {
        let mut exec = execution(ExecutionStatus::Failed);
        exec.last_error = Some(format!("spawn aborted:\n{}", "x".repeat(500)));
        let line = error_line(&exec, &[]).expect("failed execution with last_error has an error line");
        assert!(line.starts_with("error: spawn aborted: xxx"), "{line}");
        assert!(!line.contains('\n'));
        assert!(line.ends_with('…'));
        assert_eq!(line.chars().count(), "error: ".len() + MAX_ERROR_CHARS + 1);
    }

    #[test]
    fn error_line_falls_back_to_the_newest_failed_run() {
        let exec = execution(ExecutionStatus::Failed);
        let runs = [run_with_error(Some("older")), run_with_error(Some("newer"))];
        assert_eq!(error_line(&exec, &runs).as_deref(), Some("error: newer"));
        assert_eq!(error_line(&exec, &[run_with_error(None)]), None);
    }

    #[test]
    fn error_line_labels_a_retrying_execution_and_skips_other_terminal_ones() {
        let mut retrying = execution(ExecutionStatus::Ready);
        retrying.last_error = Some("workspace lease failed".into());
        assert_eq!(
            error_line(&retrying, &[]).as_deref(),
            Some("last error: workspace lease failed")
        );
        let mut cancelled = execution(ExecutionStatus::Cancelled);
        cancelled.last_error = Some("host offline".into());
        assert_eq!(error_line(&cancelled, &[]), None);
    }

    #[test]
    fn provenance_line_lists_only_what_is_set() {
        assert_eq!(provenance_line(&execution(ExecutionStatus::Running)), None);
        let mut exec = execution(ExecutionStatus::Failed);
        exec.created_via = Some("progression_sweep".into());
        exec.launch_failed = true;
        exec.force_launched = true;
        exec.operator_forced_admission = true;
        exec.progression_disposition = Some("no_action".into());
        assert_eq!(
            provenance_line(&exec).as_deref(),
            Some(
                "flags: created_via=progression_sweep progression_disposition=no_action launch_failed \
                 force_launched operator_forced_admission"
            )
        );
    }

    #[test]
    fn history_row_prints_iso_timestamps_and_waiting_since() {
        let mut exec = execution(ExecutionStatus::Ready);
        exec.started_at = Some("1700000100".into());
        exec.dispatch_wait_reason = Some("pool_exhausted".into());
        exec.dispatch_wait_since = Some("1700000200".into());
        let text = history_row_lines(&exec, &[]).join("\n");
        assert!(text.contains("created=2023-11-1"), "{text}");
        assert!(text.contains("started=2023-11-1"), "{text}");
        assert!(text.contains("finished=-"), "{text}");
        assert!(text.contains("waiting: pool_exhausted since 2023-11-1"), "{text}");
        for epoch in ["1700000000", "1700000100", "1700000200"] {
            assert!(!text.contains(epoch), "raw epoch {epoch} leaked: {text}");
        }
    }

    #[test]
    fn history_row_falls_back_to_the_run_error_for_a_failed_execution() {
        let exec = execution(ExecutionStatus::Failed);
        let text = history_row_lines(&exec, &[run_with_error(Some("workspace lease failed"))]).join("\n");
        assert!(text.contains("  error: workspace lease failed"), "{text}");
    }

    #[test]
    fn history_row_hides_the_error_of_a_cancelled_execution() {
        let mut exec = execution(ExecutionStatus::Cancelled);
        exec.last_error = Some("host offline".into());
        let text = history_row_lines(&exec, &[]).join("\n");
        assert!(!text.contains("error"), "{text}");
    }
}
