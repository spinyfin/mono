//! The slow half of hook ingress: persisting the transcript path a hook
//! taught us and folding the transcript into the run's cost snapshot.
//!
//! The ordering contract — this runs *after* the live-state transition and
//! its durable projection, never before them — is stated on the function and
//! in `tools/boss/docs/worker-liveness-contract.md`.

use super::*;
use boss_engine_live_worker_state::DriverSignalKind;

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
    incoming: &boss_engine_runtime::events_socket::IncomingHookEvent,
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
                boss_engine_completion::TRIAGE_TRANSCRIPT_READ_ATTEMPTS
            } else {
                1
            };
            for attempt in 1..=settle_attempts {
                if attempt > 1 {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        boss_engine_completion::TRIAGE_TRANSCRIPT_READ_RETRY_BASE_MS * u64::from(attempt - 1),
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

/// Look up the transcript path the run is currently writing to (via
/// `WorkRun`), and stat its current byte size so we can use that as
/// the lower bound for the next reply-extraction read. Returns
/// `(None, 0)` when the run has no transcript path recorded yet —
/// the in-flight bookkeeping still tracks the dispatched probe, but
/// `dispatch_probe_reply_on_stop` will skip emission with a warning
/// rather than fabricate empty reply text.
///
/// The `run_id` argument is the execution id (`exec_*`) carried on
/// the hook event — the same value
/// `LiveStatusManager`/`dispatch_live_worker_state` plumb everywhere
/// in this file. PR #384 flagged this code path as broken (its
/// "Out of scope" section called out that `work_db.get_run(run_id)`
/// was joining the wrong namespace). Fixed here alongside the
/// `TranscriptPathResolver` impl.
pub(super) async fn transcript_offset_for_run(server_state: &ServerState, run_id: &str) -> (Option<String>, u64) {
    let path = match server_state.work_db.transcript_path_for_execution(run_id) {
        Ok(path) => path,
        Err(err) => {
            tracing::debug!(run_id, ?err, "transcript path lookup failed for probe dispatch",);
            None
        }
    };
    let Some(path_str) = path else {
        return (None, 0);
    };
    let limit = match boss_engine_runtime::run_cost::transcript_read_limit(
        &server_state.work_db,
        std::path::Path::new(&path_str),
    ) {
        Ok(limit) => limit,
        Err(_) => return (None, 0),
    };
    let offset = match tokio::fs::metadata(&path_str).await {
        Ok(meta) => meta.len(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
        Err(err) => {
            tracing::warn!(
                run_id,
                path = %path_str,
                ?err,
                "failed to stat transcript at probe dispatch; treating offset as 0",
            );
            0
        }
    };
    (Some(path_str), limit.map_or(offset, |limit| offset.min(limit)))
}
