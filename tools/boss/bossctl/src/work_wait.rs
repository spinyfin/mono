//! `bossctl work wait` — block until an execution leaves its pre-run
//! states, so callers stop hand-rolling loops that poll the engine DB after
//! `work start` / `agents launch`.

use super::*;

use boss_protocol::ExecutionStatus;
use std::time::Duration;
use tokio::time::Instant;

/// Process exit status when the awaited execution left the waited-on states
/// and is running (or finished) normally.
pub(crate) const EXIT_STARTED: i32 = 0;
/// Process exit status when it left them by failing (failed, abandoned,
/// cancelled or orphaned).
pub(crate) const EXIT_FAILED: i32 = 1;
/// Process exit status when `--timeout` elapsed first. Distinct from clap's
/// usage error status (2).
pub(crate) const EXIT_TIMED_OUT: i32 = 3;
pub(crate) const EXIT_ERROR: i32 = 4;
pub(crate) const EXIT_NOT_STARTED: i32 = 5;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Default `--until-not` set: the pre-run states, as defined by
/// [`ExecutionStatus::is_pre_run`].
pub(crate) fn default_until_not() -> Vec<ExecutionStatus> {
    ExecutionStatus::ALL
        .iter()
        .filter(|s| s.is_pre_run())
        .cloned()
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitVerdict {
    Started,
    Failed,
    TimedOut,
    NotStarted,
}

impl WaitVerdict {
    pub(crate) fn exit_code(self) -> i32 {
        match self {
            Self::Started => EXIT_STARTED,
            Self::Failed => EXIT_FAILED,
            Self::TimedOut => EXIT_TIMED_OUT,
            Self::NotStarted => EXIT_NOT_STARTED,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::NotStarted => "not_started",
        }
    }
}

pub(crate) struct WaitOutcome {
    pub verdict: WaitVerdict,
    pub execution: Option<WorkExecution>,
}

/// Parse `--until-not ready,claimed` (already split by clap) into statuses.
/// An empty list selects [`default_until_not`].
pub(crate) fn parse_until_not(values: &[String]) -> Result<Vec<ExecutionStatus>> {
    if values.is_empty() {
        return Ok(default_until_not());
    }
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
    Ok(out)
}

/// Verdict for an execution that has left the waited-on states.
pub(crate) fn verdict_for_exit(status: &ExecutionStatus) -> WaitVerdict {
    match status {
        ExecutionStatus::Failed
        | ExecutionStatus::Abandoned
        | ExecutionStatus::Cancelled
        | ExecutionStatus::Orphaned => WaitVerdict::Failed,
        ExecutionStatus::Running
        | ExecutionStatus::WaitingHuman
        | ExecutionStatus::WaitingReview
        | ExecutionStatus::WaitingMerge
        | ExecutionStatus::Completed => WaitVerdict::Started,
        ExecutionStatus::Queued
        | ExecutionStatus::Ready
        | ExecutionStatus::WaitingDependency
        | ExecutionStatus::Claimed => WaitVerdict::NotStarted,
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
    let mut last = None;
    loop {
        let execution = match deadline {
            Some(deadline) => {
                if Instant::now() >= deadline {
                    return Ok(WaitOutcome {
                        verdict: WaitVerdict::TimedOut,
                        execution: last,
                    });
                }
                match tokio::time::timeout_at(deadline, fetch()).await {
                    Ok(result) => result?,
                    Err(_) => {
                        return Ok(WaitOutcome {
                            verdict: WaitVerdict::TimedOut,
                            execution: last,
                        });
                    }
                }
            }
            None => fetch().await?,
        };
        if !until_not.contains(&execution.status) {
            return Ok(WaitOutcome {
                verdict: verdict_for_exit(&execution.status),
                execution: Some(execution),
            });
        }
        // A terminal status can never leave the waited-on set by itself
        // changing again; do not spin on it.
        if execution.status.is_terminal() {
            return Ok(WaitOutcome {
                verdict: verdict_for_exit(&execution.status),
                execution: Some(execution),
            });
        }
        if last
            .as_ref()
            .is_none_or(|e: &WorkExecution| e.status != execution.status)
        {
            eprintln!("waiting for execution {} (status: {})", execution.id, execution.status);
        }
        last = Some(execution);
        let wake = Instant::now() + interval;
        tokio::time::sleep_until(deadline.map_or(wake, |d| d.min(wake))).await;
    }
}

/// Newest execution (by creation time) in `executions` that belongs to
/// `work_item_id`. `ListExecutions` also returns review-guide executions
/// keyed to a different work item (they only carry the task as
/// `owning_task_id`); those are not the item's own runs.
pub(crate) fn latest_execution(executions: Vec<WorkExecution>, work_item_id: &str) -> Option<WorkExecution> {
    executions
        .into_iter()
        .filter(|e| e.work_item_id == work_item_id)
        .max_by_key(|e| (e.created_epoch().unwrap_or(0), e.created_at.clone()))
}

pub(crate) fn outcome_lines(outcome: &WaitOutcome) -> Vec<String> {
    let Some(e) = &outcome.execution else {
        return vec![format!(
            "execution {} (no observation received)",
            outcome.verdict.label()
        )];
    };
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
    let Some(e) = &outcome.execution else {
        return serde_json::json!({"result": outcome.verdict.label(), "execution_id": null, "status": null});
    };
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
            FrontendEvent::ExecutionsList {
                work_item_id: resolved,
                executions,
            } => {
                let resolved = resolved.as_deref().unwrap_or(work_item_id);
                latest_execution(executions, resolved).ok_or_else(|| anyhow::anyhow!("no executions for {reference}"))
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
/// status distinguishing started (0), failed (1), timed out (3), errors (4),
/// and an excluded pre-run status (5).
pub(crate) async fn work_wait(
    socket_path: &Option<String>,
    json: bool,
    reference: String,
    until_not: Vec<String>,
    timeout_secs: Option<u64>,
) -> Result<()> {
    match work_wait_inner(socket_path, reference, until_not, timeout_secs).await {
        Ok(outcome) => {
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
        Err(error) => {
            if json {
                println!("{}", wait_error_json(&error));
            }
            eprintln!("work wait: {error:#}");
            std::process::exit(EXIT_ERROR)
        }
    }
}

fn wait_error_json(error: &anyhow::Error) -> serde_json::Value {
    serde_json::json!({"result": "error", "error": format!("{error:#}")})
}

async fn work_wait_inner(
    socket_path: &Option<String>,
    reference: String,
    until_not: Vec<String>,
    timeout_secs: Option<u64>,
) -> Result<WaitOutcome> {
    let until_not = parse_until_not(&until_not)?;
    eprintln!("waiting for {reference}");
    let client = tokio::sync::Mutex::new(None);
    let work_item_id = (!reference.starts_with("exec_")).then(|| reference.clone());
    let outcome = wait_until(
        || async {
            let mut client = client.lock().await;
            if client.is_none() {
                *client = Some(connect(socket_path).await?);
            }
            fetch_by_ref(client.as_mut().unwrap(), &reference, &work_item_id).await
        },
        &until_not,
        timeout_secs.map(Duration::from_secs),
        POLL_INTERVAL,
    )
    .await?;
    Ok(outcome)
}

#[cfg(test)]
#[path = "work_wait_tests.rs"]
mod tests;
