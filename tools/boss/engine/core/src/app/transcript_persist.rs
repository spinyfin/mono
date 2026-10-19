//! The slow half of hook ingress: persisting the transcript path a hook
//! taught us and folding the transcript into the run's cost snapshot.
//!
//! The ordering contract — this runs *after* the live-state transition and
//! its durable projection, never before them — is stated on the function and
//! in `tools/boss/docs/worker-liveness-contract.md`.

use super::*;
use crate::live_worker_state::DriverSignalKind;

/// Persist the transcript path the hook taught us and fold the transcript
/// into the run's cumulative cost snapshot.
///
/// Keyed solely on `run_id`, never on the slot mapping: it runs for a hook
/// racing ahead of `register_run_slot`, after an engine restart has wiped
/// the in-memory `WorkerRegistry`, and for a late hook after the slot was
/// released — so orphaned and late-hook executions retain the final
/// observed cost too. On a turn boundary it polls the transcript with a
/// bounded settle loop, which is why it is the *slow* half of hook ingress
/// and runs **after** the live-state transition, never before it: the
/// activity a worker reports must not wait on file I/O that a later,
/// faster event can overtake (see
/// `LiveWorkerStateRegistry::apply_sequenced_event`).
pub(super) async fn persist_transcript_and_capture_cost(
    server_state: &Arc<ServerState>,
    incoming: &crate::events_socket::IncomingHookEvent,
    run_id: &str,
    resolved_path: Option<&str>,
    from_cache: bool,
) {
    if let Some(path) = resolved_path {
        // The second driver-originated signal named in the driver-start
        // contract: a resolved transcript path means the driver created
        // its transcript. Redundant with the `HookEvent` stamp in
        // `dispatch_live_worker_state` (both are reached from the same
        // ingress), and
        // recorded anyway so the contract holds at every site that
        // learns a transcript path rather than depending on the two
        // staying adjacent.
        server_state
            .record_driver_signal(run_id, DriverSignalKind::TranscriptPath)
            .await;
        // `run_id` here is the `_boss_run_id` from the hook payload,
        // which carries the **execution_id** (`exec_*`) — not a
        // `work_runs.id` (`run_*`). The setter joins on
        // `work_runs.execution_id` so the caller doesn't have to
        // care; the local `execution_id` binding is just to make
        // the namespace explicit at the call site, since the
        // historical "run_id" naming all the way through the
        // dispatcher is what hid the wrong-namespace bug.
        let execution_id = run_id;
        match server_state
            .work_db
            .set_run_transcript_path_if_unset(execution_id, path)
        {
            Ok(SetRunTranscriptPathOutcome::Updated) => {
                server_state.dispatcher_stats.inc_persist_updated();
                if from_cache {
                    server_state.dispatcher_stats.inc_persist_from_cache();
                }
                tracing::info!(
                    execution_id,
                    transcript_path = %path,
                    from_cache,
                    "recorded transcript_path on work_run from hook payload",
                );
            }
            Ok(SetRunTranscriptPathOutcome::AlreadySet) => {
                server_state.dispatcher_stats.inc_persist_noop();
            }
            Ok(SetRunTranscriptPathOutcome::RowMissing) => {
                server_state.dispatcher_stats.inc_persist_row_missing();
                tracing::warn!(
                    execution_id,
                    transcript_path = %path,
                    "no work_runs row for execution yet; transcript_path persist deferred to a later hook",
                );
            }
            Err(err) => {
                server_state.dispatcher_stats.inc_persist_err();
                tracing::warn!(
                    execution_id,
                    ?err,
                    "failed to persist transcript_path from hook payload",
                );
            }
        }

        // Fold transcript records appended since the prior hook into a
        // cumulative, idempotent snapshot. The driver-owned containment root
        // was resolved at ingress alongside the turn boundary; failure to
        // resolve it refuses the read instead of degrading a contained driver
        // (notably Codex) to an unrestricted tail.
        let containment_root = match incoming.transcript_containment_root() {
            Ok(root) => Some(root),
            Err(error) => {
                tracing::warn!(
                    execution_id = run_id,
                    transcript_path = %path,
                    %error,
                    "refusing run cost capture because transcript containment could not be resolved",
                );
                None
            }
        };

        // A provider can fire its turn boundary just before flushing the
        // final assistant usage and turn-duration records. Poll with the same
        // bounded linear-backoff shape used by triage transcript readback.
        // This remains independent of successful finalization and runs on the
        // slot-less path too, so orphaned and late-hook executions retain the
        // final observed cost. Missing synthetic/test paths take one poll only.
        if let Some(containment_root) = containment_root {
            let settle_attempts = if incoming.is_turn_boundary() && tokio::fs::try_exists(path).await.unwrap_or(false) {
                crate::completion::TRIAGE_TRANSCRIPT_READ_ATTEMPTS
            } else {
                1
            };
            for attempt in 1..=settle_attempts {
                if attempt > 1 {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        crate::completion::TRIAGE_TRANSCRIPT_READ_RETRY_BASE_MS * u64::from(attempt - 1),
                    ))
                    .await;
                }
                if let Err(error) = server_state
                    .run_cost_capture
                    .capture_and_persist(
                        &server_state.work_db,
                        run_id,
                        std::path::Path::new(path),
                        containment_root,
                    )
                    .await
                {
                    tracing::warn!(
                        execution_id = run_id,
                        transcript_path = %path,
                        %error,
                        "failed to capture cumulative run cost from transcript",
                    );
                    break;
                }
            }
        }
    }
}
