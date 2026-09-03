//! Persist a live tmux pane observation as durable run identity.
//!
//! Split out of [`super`] so the identity write can be read on its own: this
//! is the load-bearing repair for probe delivery. Adoption used to refresh
//! `shell_pid` and leave `tmux_session_name` / `tmux_spawn_state` /
//! `tmux_pane_pid` unset, so every tmux-hosted probe failed with
//! `DriverLivenessUnavailable("no durable tmux identity recorded for run")`.

use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::work::WorkDb;

/// Outcome of [`persist_observed_pane_pid`]. The durable write and the
/// in-memory adoption are deliberately allowed to disagree on one axis: a
/// pid that was *positively observed* on the live tmux pane is real evidence
/// of liveness regardless of whether the database happened to accept the
/// write at that instant, so a transient DB failure alone must not throw that
/// evidence away. Only `NoMatchingRun` — the identity itself being wrong —
/// is a hard refusal.
pub(super) enum PersistedPanePid {
    /// The durable write landed. Carries the pre-write snapshot for event
    /// diagnostics.
    Written(Option<i64>, Option<i64>),
    /// The write did not land, but the pid was positively observed on the
    /// pane, so in-memory adoption should still proceed; the next sweep pass
    /// retries the durable write. `None` means the pre-write snapshot could
    /// not be read, rather than claiming it contained no pids.
    WriteFailed(Option<(Option<i64>, Option<i64>)>),
    /// `spawn_token` matched no durable run row at all — the identity is
    /// wrong, not just unwritable. Adoption must refuse.
    NoMatchingRun,
}

/// Convert a durable-pid outcome into the diagnostic snapshot carried by an
/// in-memory adoption. `None` is the identity-mismatch refusal; the boolean
/// distinguishes an unreadable snapshot from a known pair of absent pids.
pub(super) fn adoption_pid_snapshot(outcome: PersistedPanePid) -> Option<(Option<i64>, Option<i64>, bool)> {
    match outcome {
        PersistedPanePid::Written(stored, previous) => Some((stored, previous, true)),
        PersistedPanePid::WriteFailed(Some((stored, previous))) => Some((stored, previous, true)),
        PersistedPanePid::WriteFailed(None) => Some((None, None, false)),
        PersistedPanePid::NoMatchingRun => None,
    }
}

/// One positive tmux pane observation, bundled so
/// [`persist_observed_pane_pid`] takes it as a single unit rather than six
/// positional arguments callers must keep in the right order. All three call
/// sites (`adopt_one`, and both branches of `classify_untracked_session`)
/// build the same set from a fresh [`session_pane_pid`] read plus the
/// session/token identity already in hand.
#[derive(bon::Builder)]
pub(super) struct PaneObservation<'a> {
    pub(super) execution_id: &'a str,
    pub(super) spawn_token: &'a str,
    pub(super) session_name: &'a str,
    pub(super) server_label: &'a str,
    pub(super) observed_shell_pid: i32,
    pub(super) write_reason: &'a str,
}

/// Reconcile one positive tmux pane observation into the durable identity
/// columns before any in-memory adoption or terminal readoption proceeds.
pub(super) async fn persist_observed_pane_pid(
    work_db: &WorkDb,
    dispatch_events: &dyn DispatchEventSink,
    observation: &PaneObservation<'_>,
) -> PersistedPanePid {
    let PaneObservation {
        execution_id,
        spawn_token,
        session_name,
        server_label,
        observed_shell_pid,
        write_reason,
    } = *observation;
    let before = match work_db.latest_local_pane_pid_snapshot_for_execution(execution_id) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            tracing::warn!(
                execution_id,
                session = session_name,
                observed_shell_pid,
                write_reason,
                error = %format!("{err:#}"),
                "tmux adoption: could not read the durable pid snapshot; the pid was still positively \
                 observed, so adoption proceeds and the durable write is retried on the next sweep",
            );
            return PersistedPanePid::WriteFailed(None);
        }
    };
    let (stored_shell_pid, previous_tmux_pane_pid) = before;
    tracing::info!(
        execution_id,
        session = session_name,
        stored_shell_pid = ?stored_shell_pid,
        previous_tmux_pane_pid = ?previous_tmux_pane_pid,
        observed_shell_pid,
        write_reason,
        "tmux adoption: persisting durable tmux identity from the live session",
    );

    let persisted = work_db.persist_tmux_identity_after_observation(
        execution_id,
        spawn_token,
        i64::from(observed_shell_pid),
        Some(session_name),
        Some(server_label),
    );
    match persisted {
        Ok(true) => {
            // The write targets the row identified by `spawn_token`, but
            // every liveness reader instead selects the newest LOCAL run row
            // by `created_at`. When an execution has acquired a sibling run
            // row between the two writes, that row is not the same one, and
            // the write above would silently land somewhere the liveness
            // readers never look. Read back through the same path the
            // readers use and surface the mismatch instead of leaving it to
            // resurface later as an unexplained `NeverAttached` verdict.
            match work_db.latest_local_pane_pid_snapshot_for_execution(execution_id) {
                Ok((Some(visible_shell_pid), _)) if visible_shell_pid == i64::from(observed_shell_pid) => {}
                Ok((visible_shell_pid, _)) => {
                    tracing::error!(
                        execution_id,
                        session = session_name,
                        observed_shell_pid,
                        write_reason,
                        visible_shell_pid = ?visible_shell_pid,
                        "tmux adoption: pid write landed on the spawn-token run row, but the newest local \
                         run row every liveness reader selects does not reflect it — the execution likely \
                         acquired a sibling run row between the two writes",
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        execution_id,
                        session = session_name,
                        observed_shell_pid,
                        write_reason,
                        error = %format!("{err:#}"),
                        "tmux adoption: could not verify the pid write became visible to liveness readers",
                    );
                }
            }
            PersistedPanePid::Written(before.0, before.1)
        }
        Ok(false) => {
            tracing::error!(
                execution_id,
                session = session_name,
                observed_shell_pid,
                write_reason,
                "tmux adoption: observed pane pid matched no durable tmux run; refusing to adopt the session",
            );
            dispatch_events
                .emit(
                    DispatchEvent::new(Stage::TmuxAdopt, Outcome::Error, execution_id).with_details(
                        serde_json::json!({
                            "tmux_session_name": session_name,
                            "stored_shell_pid": stored_shell_pid,
                            "previous_tmux_pane_pid": previous_tmux_pane_pid,
                            "observed_shell_pid": observed_shell_pid,
                            "shell_pid_write_reason": write_reason,
                            "error": "tmux_run_not_found",
                        }),
                    ),
                )
                .await;
            PersistedPanePid::NoMatchingRun
        }
        Err(err) => {
            tracing::warn!(
                execution_id,
                session = session_name,
                observed_shell_pid,
                write_reason,
                error = %format!("{err:#}"),
                "tmux adoption: failed to persist the observed pane pid (likely transient); the pid was \
                 still positively observed, so adoption proceeds and the durable write is retried on the \
                 next sweep",
            );
            dispatch_events
                .emit(
                    DispatchEvent::new(Stage::TmuxAdopt, Outcome::Error, execution_id).with_details(
                        serde_json::json!({
                            "tmux_session_name": session_name,
                            "stored_shell_pid": stored_shell_pid,
                            "previous_tmux_pane_pid": previous_tmux_pane_pid,
                            "observed_shell_pid": observed_shell_pid,
                            "shell_pid_write_reason": write_reason,
                            "error": format!("{err:#}"),
                            "adoption_proceeded_without_write": true,
                        }),
                    ),
                )
                .await;
            PersistedPanePid::WriteFailed(Some(before))
        }
    }
}
