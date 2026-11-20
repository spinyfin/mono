//! `bossctl work wait` — block until an execution leaves its pre-run
//! states, so callers stop hand-rolling loops that poll the engine DB after
//! `work start` / `agents launch`.
//!
//! Also home to the retry predicate behind `agents launch --wait-for-slot`.

use super::*;

use boss_protocol::ExecutionStatus;
use std::time::{Duration, Instant};

/// Process exit status when the awaited execution left the waited-on states
/// and is running (or finished) normally.
pub(crate) const EXIT_STARTED: i32 = 0;
/// Process exit status when it left them by failing (failed, abandoned,
/// cancelled or orphaned).
pub(crate) const EXIT_FAILED: i32 = 1;
/// Process exit status when `--timeout` elapsed first. Distinct from clap's
/// usage error status (2).
pub(crate) const EXIT_TIMED_OUT: i32 = 3;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Default `--until-not` set: the pre-run states.
pub(crate) const DEFAULT_UNTIL_NOT: &[&str] = &["ready", "claimed"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitVerdict {
    Started,
    Failed,
    TimedOut,
}

impl WaitVerdict {
    pub(crate) fn exit_code(self) -> i32 {
        match self {
            Self::Started => EXIT_STARTED,
            Self::Failed => EXIT_FAILED,
            Self::TimedOut => EXIT_TIMED_OUT,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
        }
    }
}

pub(crate) struct WaitOutcome {
    pub verdict: WaitVerdict,
    pub execution: WorkExecution,
}

/// Parse `--until-not ready,claimed` (already split by clap) into statuses.
pub(crate) fn parse_until_not(values: &[String]) -> Result<Vec<ExecutionStatus>> {
    let mut out = Vec::new();
    for value in values {
        let status = value
            .trim()
            .parse::<ExecutionStatus>()
            .map_err(|e| anyhow::anyhow!(e))?;
        if !out.contains(&status) {
            out.push(status);
        }
    }
    if out.is_empty() {
        bail!("--until-not needs at least one status");
    }
    Ok(out)
}

/// Verdict for an execution that has left the waited-on states.
pub(crate) fn verdict_for_exit(status: &ExecutionStatus) -> WaitVerdict {
    match status {
        ExecutionStatus::Failed
        | ExecutionStatus::Abandoned
        | ExecutionStatus::Cancelled
        | ExecutionStatus::Orphaned => WaitVerdict::Failed,
        _ => WaitVerdict::Started,
    }
}

/// Poll `fetch` until the execution's status is outside `until_not` (or
/// terminal), or `timeout` elapses. `fetch` returns the current execution.
pub(crate) async fn wait_until<F, Fut>(
    mut fetch: F,
    until_not: &[ExecutionStatus],
    timeout: Option<Duration>,
    interval: Duration,
) -> Result<WaitOutcome>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<WorkExecution>>,
{
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        let execution = fetch().await?;
        if !until_not.contains(&execution.status) {
            return Ok(WaitOutcome {
                verdict: verdict_for_exit(&execution.status),
                execution,
            });
        }
        // A terminal status can never leave the waited-on set by itself
        // changing again; do not spin on it.
        if execution.status.is_terminal() {
            return Ok(WaitOutcome {
                verdict: verdict_for_exit(&execution.status),
                execution,
            });
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(WaitOutcome {
                verdict: WaitVerdict::TimedOut,
                execution,
            });
        }
        tokio::time::sleep(interval).await;
    }
}

/// Newest execution (by creation time) in `executions`.
pub(crate) fn latest_execution(executions: Vec<WorkExecution>) -> Option<WorkExecution> {
    executions.into_iter().max_by(|a, b| {
        let key = |e: &WorkExecution| (e.created_at.parse::<i64>().unwrap_or(0), e.created_at.clone());
        key(a).cmp(&key(b))
    })
}

/// Whether an engine refusal is the "worker pool is full" error that
/// `agents launch --wait-for-slot` retries instead of reporting.
pub(crate) fn is_hard_cap_refusal(message: &str) -> bool {
    message.contains("hard cap")
}

pub(crate) fn outcome_lines(outcome: &WaitOutcome) -> Vec<String> {
    let e = &outcome.execution;
    let mut lines = vec![format!(
        "execution {} {} (status: {})",
        e.id,
        outcome.verdict.label(),
        e.status
    )];
    if let Some(reason) = e.dispatch_wait_reason.as_deref().filter(|r| !r.is_empty()) {
        lines.push(format!("  wait reason: {reason}"));
    }
    if let Some(error) = e.last_error.as_deref().filter(|r| !r.is_empty()) {
        lines.push(format!("  last error:  {error}"));
    }
    lines
}

pub(crate) fn outcome_json(outcome: &WaitOutcome) -> serde_json::Value {
    let e = &outcome.execution;
    serde_json::json!({
        "result": outcome.verdict.label(),
        "execution_id": e.id,
        "work_item_id": e.work_item_id,
        "status": e.status.as_str(),
        "dispatch_wait_reason": e.dispatch_wait_reason,
        "last_error": e.last_error,
    })
}

async fn fetch_by_ref(
    client: &mut BossClient,
    reference: &str,
    work_item_id: &Option<String>,
) -> Result<WorkExecution> {
    if let Some(work_item_id) = work_item_id {
        let response = client
            .send_request(&FrontendRequest::ListExecutions {
                work_item_id: Some(work_item_id.clone()),
                include_revision_chain: false,
            })
            .await
            .context("sending ListExecutions")?;
        return match response {
            FrontendEvent::ExecutionsList { executions, .. } => {
                latest_execution(executions).ok_or_else(|| anyhow::anyhow!("no executions for {reference}"))
            }
            FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
                bail!("engine rejected list executions: {message}")
            }
            other => bail!("engine returned unexpected response: {other:?}"),
        };
    }
    let response = client
        .send_request(&FrontendRequest::GetExecution {
            id: reference.to_owned(),
        })
        .await
        .context("sending GetExecution")?;
    match response {
        FrontendEvent::ExecutionResult { execution } => Ok(execution),
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected execution lookup for {reference}: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// `bossctl work wait <execution|work item>`. Exits the process with a
/// status distinguishing started (0), failed (1) and timed out (3).
pub(crate) async fn work_wait(
    socket_path: &Option<String>,
    json: bool,
    reference: String,
    until_not: Vec<String>,
    timeout_secs: Option<u64>,
) -> Result<()> {
    let until_not = parse_until_not(&until_not)?;
    let mut client = connect(socket_path).await?;
    // Execution ids go straight to GetExecution; anything else is a work
    // item (friendly short ids resolved to the primary id once, up front).
    let work_item_id = if reference.starts_with("exec_") {
        None
    } else {
        match client
            .send_request(&FrontendRequest::GetWorkItem { id: reference.clone() })
            .await
            .context("sending GetWorkItem")?
        {
            FrontendEvent::WorkItemResult { item } => Some(item.primary_id().to_owned()),
            FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
                bail!("engine rejected work-item lookup for {reference}: {message}")
            }
            other => bail!("engine returned unexpected response: {other:?}"),
        }
    };
    let client = tokio::sync::Mutex::new(client);
    let outcome = wait_until(
        || async {
            let mut client = client.lock().await;
            fetch_by_ref(&mut client, &reference, &work_item_id).await
        },
        &until_not,
        timeout_secs.map(Duration::from_secs),
        POLL_INTERVAL,
    )
    .await?;
    if json {
        println!("{}", outcome_json(&outcome));
    } else {
        for line in outcome_lines(&outcome) {
            println!("{line}");
        }
    }
    match outcome.verdict.exit_code() {
        0 => Ok(()),
        code => std::process::exit(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn exec(status: ExecutionStatus) -> WorkExecution {
        WorkExecution::builder()
            .id("exec_1")
            .work_item_id("task_1")
            .kind(boss_protocol::ExecutionKind::ChoreImplementation)
            .status(status)
            .repo_remote_url("git@example.com:foo.git")
            .created_at("1747000000")
            .build()
    }

    fn default_until() -> Vec<ExecutionStatus> {
        parse_until_not(&DEFAULT_UNTIL_NOT.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn parse_until_not_rejects_unknown_and_empty() {
        assert!(parse_until_not(&["bogus".into()]).is_err());
        assert!(parse_until_not(&[]).is_err());
        assert_eq!(
            parse_until_not(&["ready".into(), "claimed".into(), "ready".into()]).unwrap(),
            vec![ExecutionStatus::Ready, ExecutionStatus::Claimed]
        );
    }

    #[test]
    fn verdicts_map_to_distinct_exit_codes() {
        assert_eq!(verdict_for_exit(&ExecutionStatus::Running).exit_code(), 0);
        assert_eq!(verdict_for_exit(&ExecutionStatus::Failed).exit_code(), 1);
        assert_eq!(verdict_for_exit(&ExecutionStatus::Cancelled).exit_code(), 1);
        assert_eq!(WaitVerdict::TimedOut.exit_code(), 3);
    }

    #[tokio::test]
    async fn returns_started_once_execution_leaves_waited_states() {
        let seq = RefCell::new(vec![
            ExecutionStatus::Running,
            ExecutionStatus::Claimed,
            ExecutionStatus::Ready,
        ]);
        let outcome = wait_until(
            || {
                let status = seq.borrow_mut().pop().unwrap();
                async move { Ok(exec(status)) }
            },
            &default_until(),
            None,
            Duration::from_millis(1),
        )
        .await
        .unwrap();
        assert_eq!(outcome.verdict, WaitVerdict::Started);
        assert_eq!(outcome.execution.status, ExecutionStatus::Running);
    }

    #[tokio::test]
    async fn reports_failure_with_last_error() {
        let outcome = wait_until(
            || async {
                let mut e = exec(ExecutionStatus::Failed);
                e.last_error = Some("cube setup failed".into());
                Ok(e)
            },
            &default_until(),
            None,
            Duration::from_millis(1),
        )
        .await
        .unwrap();
        assert_eq!(outcome.verdict, WaitVerdict::Failed);
        assert!(outcome_lines(&outcome).join("\n").contains("cube setup failed"));
        assert_eq!(outcome_json(&outcome)["result"], "failed");
    }

    #[tokio::test]
    async fn times_out_and_reports_wait_reason() {
        let outcome = wait_until(
            || async {
                let mut e = exec(ExecutionStatus::Ready);
                e.dispatch_wait_reason = Some("pool full".into());
                Ok(e)
            },
            &default_until(),
            Some(Duration::from_millis(5)),
            Duration::from_millis(1),
        )
        .await
        .unwrap();
        assert_eq!(outcome.verdict, WaitVerdict::TimedOut);
        assert!(outcome_lines(&outcome).join("\n").contains("pool full"));
    }

    #[test]
    fn hard_cap_refusal_is_recognised() {
        assert!(is_hard_cap_refusal(
            "main pool already at hard cap (16); cannot force-dispatch x"
        ));
        assert!(!is_hard_cap_refusal("unmet dependency"));
    }

    #[test]
    fn latest_execution_picks_newest() {
        let mut older = exec(ExecutionStatus::Completed);
        older.id = "exec_old".into();
        older.created_at = "100".into();
        let mut newer = exec(ExecutionStatus::Ready);
        newer.id = "exec_new".into();
        newer.created_at = "200".into();
        assert_eq!(latest_execution(vec![newer, older]).unwrap().id, "exec_new");
    }
}
