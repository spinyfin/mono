//! Lifecycle of staged remote worker input (`<workspace>/<config>/runs/<run_id>`).
//!
//! The brief is written to the remote before any worker process exists, so
//! its cleanup cannot wait for a recorded remote pid. A durable marker is
//! recorded before the first transfer; the run directory is removed as soon
//! as it is known no worker will read it (failure before process creation,
//! or confirmed immediate worker death), and otherwise the marker stays for
//! [`retry_pending`] to settle once the execution is no longer live.

use std::future::Future;

use anyhow::Result;

use crate::host_adapter::{HostAdapterProvider, resolve_host_adapter};
use crate::ssh_spawn::{RemoteLaunchOutcome, SshExec};
use crate::work::WorkDb;

pub(crate) fn validate_run_id(run_id: &str) -> Result<()> {
    anyhow::ensure!(
        !run_id.is_empty()
            && run_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "invalid initial-input run id"
    );
    Ok(())
}

/// Remove the exact per-run input directory under every driver config dir.
pub(crate) async fn remove_run_input(exec: &dyn SshExec, workspace: &str, run_id: &str) -> Result<()> {
    validate_run_id(run_id)?;
    for config in [".claude", ".codex", ".grok"] {
        let path = format!("{workspace}/{config}/runs/{run_id}");
        let output = exec.run(&["rm", "-rf", "--", &path]).await?;
        anyhow::ensure!(output.success(), "remote input cleanup failed: {}", output.stderr);
    }
    Ok(())
}

/// Stage, then launch, keeping the staged input's cleanup obligation durable.
///
/// A pending marker is written before `stage` runs. A `stage` error, a
/// wrapper that reports no launch, and a worker confirmed dead right after
/// launch all prove no worker will read the input, so it is removed now (the
/// marker survives only if that removal itself fails). A `launch` error is
/// an interrupted launch: the worker may be live, so nothing is deleted and
/// the marker is left for [`retry_pending`]. On a successful launch the
/// marker also stays; the caller clears it once the remote pid is recorded.
pub(crate) async fn with_input_lifecycle<T, S, L, F>(
    exec: &dyn SshExec,
    work_db: &WorkDb,
    workspace: &str,
    run_id: &str,
    stage: S,
    launch: L,
) -> Result<RemoteLaunchOutcome>
where
    S: Future<Output = Result<T>>,
    L: FnOnce(T) -> F,
    F: Future<Output = Result<RemoteLaunchOutcome>>,
{
    let host_id = exec.host_id().to_owned();
    work_db.mark_remote_input_cleanup_pending(&host_id, run_id, workspace)?;
    let staged = match stage.await {
        Ok(staged) => staged,
        Err(error) => {
            discard(exec, work_db, workspace, run_id).await;
            return Err(error);
        }
    };
    let outcome = launch(staged).await?;
    if !outcome.launched {
        discard(exec, work_db, workspace, run_id).await;
    }
    Ok(outcome)
}

async fn discard(exec: &dyn SshExec, work_db: &WorkDb, workspace: &str, run_id: &str) {
    match remove_run_input(exec, workspace, run_id).await {
        Ok(()) => {
            if let Err(error) = work_db.clear_remote_input_cleanup(exec.host_id(), run_id) {
                tracing::warn!(?error, run_id, "remote input cleanup done but marker not cleared");
            }
        }
        Err(error) => {
            tracing::warn!(?error, run_id, "remote input cleanup failed; marker retained for retry");
        }
    }
}

/// Retry removal for every pending marker whose execution is no longer live.
/// A live execution may still be starting its worker, so its input is kept.
pub(crate) async fn retry_pending(work_db: &WorkDb, provider: &dyn HostAdapterProvider) {
    let pending = match work_db.pending_remote_input_cleanups() {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(?error, "remote input cleanup: listing pending markers failed");
            return;
        }
    };
    for item in pending {
        if matches!(work_db.get_execution(&item.run_id), Ok(execution) if execution.status.is_live()) {
            continue;
        }
        let Ok(adapter) = resolve_host_adapter(work_db, provider, &item.host_id).await else {
            continue;
        };
        match adapter.cleanup_initial_input(&item.workspace, &item.run_id).await {
            Ok(()) => {
                if let Err(error) = work_db.clear_remote_input_cleanup(&item.host_id, &item.run_id) {
                    tracing::warn!(?error, run_id = %item.run_id, "remote input cleanup: clearing marker failed");
                }
            }
            Err(error) => {
                tracing::warn!(?error, run_id = %item.run_id, "remote input cleanup retry failed; will retry");
            }
        }
    }
}
