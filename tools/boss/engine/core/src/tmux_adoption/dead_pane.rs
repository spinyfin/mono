//! A retained tmux session is not a live worker.
//!
//! `remain-on-exit` keeps a session listed after its worker pane has
//! exited, with `#{pane_dead}=1` and `#{pane_dead_status}` still readable.
//! Session existence alone used to be treated as liveness by the adoption
//! pass, so a spawn that died at the first command (status 127) was
//! re-adopted every minute, then reaped again — never a clean failure.

use boss_tmux::Tmux;

use crate::dispatch_events::{DispatchEventSink, Stage};
use crate::work::WorkDb;

use super::{TmuxAdoptionOutcome, TmuxIdentityObservation, persist_observed_pane_state};

/// How [`probe_worker_pane_liveness`] classified one session's worker pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WorkerPaneLiveness {
    /// `#{pane_dead}` was `0`.
    Live,
    /// `#{pane_dead}` was `1`. Terminal evidence: must not be adopted or
    /// handed to re-adoption as a live worker.
    Dead {
        pane_dead_status: Option<String>,
        last_output: Option<String>,
    },
    /// The pane-dead probe itself failed. Not proof of life or death; the
    /// caller leaves the session for a later pass.
    Unreadable,
}

/// Probe the session and, if the worker pane is dead or unreadable, handle
/// it and return `true` so the caller does not treat the session as live.
#[allow(clippy::too_many_arguments)]
pub(super) async fn skip_if_not_live(
    work_db: &WorkDb,
    tmux: &Tmux,
    dispatch_events: &dyn DispatchEventSink,
    execution_id: &str,
    session_name: &str,
    spawn_token: &str,
    outcome: &mut TmuxAdoptionOutcome,
) -> bool {
    match probe_worker_pane_liveness(tmux, session_name).await {
        WorkerPaneLiveness::Live => false,
        WorkerPaneLiveness::Dead {
            pane_dead_status,
            last_output,
        } => {
            reconcile_dead_worker_pane(
                work_db,
                tmux,
                dispatch_events,
                execution_id,
                session_name,
                spawn_token,
                pane_dead_status,
                last_output,
                outcome,
            )
            .await;
            true
        }
        WorkerPaneLiveness::Unreadable => true,
    }
}

/// Token-verified `#{pane_dead}` plus a best-effort pane capture for the
/// failure reason. The caller has already matched `BOSS_SPAWN_TOKEN`.
pub(super) async fn probe_worker_pane_liveness(tmux: &Tmux, session_name: &str) -> WorkerPaneLiveness {
    let observation = match super::observe_pane_dead_state(tmux, session_name).await {
        Ok(observation) => observation,
        Err(err) => {
            tracing::warn!(
                session = session_name,
                error = %format!("{err:#}"),
                "tmux session sweep: pane_dead unreadable; leaving this session for a later pass \
                 rather than treating session existence as a live worker",
            );
            return WorkerPaneLiveness::Unreadable;
        }
    };
    if observation.pane_dead != Some(true) {
        return WorkerPaneLiveness::Live;
    }
    let last_output = match tmux.capture_pane(session_name).await {
        Ok(text) => {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        Err(err) => {
            tracing::debug!(
                session = session_name,
                error = %format!("{err:#}"),
                "tmux session sweep: dead pane last output unreadable",
            );
            None
        }
    };
    WorkerPaneLiveness::Dead {
        pane_dead_status: observation.pane_dead_status,
        last_output,
    }
}

/// Persist the dead-pane observation, terminalize a still-live execution
/// once, and token-verified-kill the retained session. Never rebuilds
/// derived bookkeeping and never hands the session to re-adoption.
#[allow(clippy::too_many_arguments)]
pub(super) async fn reconcile_dead_worker_pane(
    work_db: &WorkDb,
    tmux: &Tmux,
    dispatch_events: &dyn DispatchEventSink,
    execution_id: &str,
    session_name: &str,
    spawn_token: &str,
    pane_dead_status: Option<String>,
    last_output: Option<String>,
    outcome: &mut TmuxAdoptionOutcome,
) {
    let observation = TmuxIdentityObservation {
        adoption_state: boss_protocol::TmuxAdoptionState::Adopted,
        pane_dead: Some(true),
        pane_dead_status: pane_dead_status.clone(),
        window_activity_epoch_secs: None,
        current_command: None,
    };
    persist_observed_pane_state(work_db, execution_id, spawn_token, session_name, &observation);

    let reason = dead_pane_reason(session_name, pane_dead_status.as_deref(), last_output.as_deref());
    let execution = match work_db.get_execution(execution_id) {
        Ok(execution) => execution,
        Err(err) => {
            tracing::warn!(
                execution_id,
                session = session_name,
                error = %format!("{err:#}"),
                "tmux session sweep: dead pane observed but the execution row could not be loaded",
            );
            outcome.dead_panes += 1;
            kill_retained_session(tmux, session_name, spawn_token, execution_id).await;
            return;
        }
    };

    if !execution.status.is_terminal() {
        let prior_status = execution.status.as_str().to_owned();
        let reconciled = crate::execution_liveness::finalize_gone_execution(
            work_db,
            dispatch_events,
            &execution,
            &reason,
            &format!("its tmux worker pane was dead ({reason})"),
            Stage::PaneDeathReconcile,
            serde_json::json!({
                "reason": "tmux_pane_dead",
                "prior_status": prior_status,
                "tmux_session_name": session_name,
                "pane_dead_status": pane_dead_status,
                "last_output": last_output.as_deref().map(truncate_pane_output),
                "kind": execution.kind.as_str(),
            }),
        )
        .await;
        tracing::warn!(
            execution_id,
            session = session_name,
            pane_dead_status = pane_dead_status.as_deref().unwrap_or(""),
            reconciled,
            "tmux session sweep: worker pane is dead; execution reconciled as terminal rather than adopted",
        );
    } else {
        tracing::info!(
            execution_id,
            session = session_name,
            status = %execution.status,
            pane_dead_status = pane_dead_status.as_deref().unwrap_or(""),
            "tmux session sweep: worker pane is dead; not treating the retained session as a live worker",
        );
    }

    // Persist pane-capture diagnostics against the matched run even when
    // this pass does not perform the terminal transition (dead-pid
    // reconciliation commonly orphans the execution first). Killing the
    // retained session below discards the pane contents, so the run row
    // is the only place the snippet survives. The SQL requires Orphaned
    // status at write time, preserving other terminal outcomes even if
    // another reconciler wins the transition concurrently.
    if let Err(err) = work_db.record_tmux_run_failure_reason(execution_id, spawn_token, &reason) {
        tracing::warn!(
            execution_id,
            session = session_name,
            error = %format!("{err:#}"),
            "tmux session sweep: dead pane observed but the run-row diagnostics could not be persisted",
        );
    }

    kill_retained_session(tmux, session_name, spawn_token, execution_id).await;
    outcome.dead_panes += 1;
}

pub(super) fn dead_pane_reason(
    session_name: &str,
    pane_dead_status: Option<&str>,
    last_output: Option<&str>,
) -> String {
    let mut reason = match pane_dead_status {
        Some(status) if !status.is_empty() => {
            format!("tmux pane dead in session {session_name}: pane_dead_status={status}")
        }
        _ => format!("tmux pane dead in session {session_name}"),
    };
    if let Some(output) = last_output.map(truncate_pane_output).filter(|text| !text.is_empty()) {
        reason.push_str("; last output: ");
        reason.push_str(&output);
    }
    reason
}

fn truncate_pane_output(text: &str) -> String {
    const MAX_CHARS: usize = 400;
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_CHARS {
        return trimmed.to_owned();
    }
    let mut snippet = trimmed
        .chars()
        .rev()
        .take(MAX_CHARS)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    if let Some(cut) = snippet.find(char::is_whitespace) {
        snippet = snippet[cut..].trim_start().to_owned();
    }
    format!("…{snippet}")
}

async fn kill_retained_session(tmux: &Tmux, session_name: &str, spawn_token: &str, execution_id: &str) {
    match tmux.kill_session_verified(session_name, spawn_token).await {
        Ok(boss_tmux::KillSessionOutcome::Killed | boss_tmux::KillSessionOutcome::Absent) => {}
        Err(err) => {
            tracing::warn!(
                execution_id,
                session = session_name,
                error = %format!("{err:#}"),
                "tmux session sweep: failed to reap the retained dead pane; a later pass will retry",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::dead_pane_reason;

    #[test]
    fn reason_includes_exit_status_and_last_output() {
        let reason = dead_pane_reason("boss-worker-1", Some("127"), Some("zsh: command not found: codex\n"));
        assert!(
            reason.contains("pane_dead_status=127"),
            "reason must name the pane exit status, got {reason}"
        );
        assert!(
            reason.contains("command not found: codex"),
            "reason must carry the dead pane last output, got {reason}"
        );
    }
}
