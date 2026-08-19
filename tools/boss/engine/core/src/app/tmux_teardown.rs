//! Token-verified tmux session teardown — the tmux-hosted counterpart of
//! the SIGTERM→SIGKILL ladder [`reap_worker_process_tree`] already runs
//! against every released worker's pane pid.
//!
//! Every terminal path that calls [`ServerState::release_worker_pane`]
//! (completion, cancel, orphan reconcile, husk retire, stale escalation,
//! `bossctl agents stop`) ends up here too, via
//! [`ServerState::reap_tmux_worker`]: read back the durably-recorded tmux
//! session identity for the run, refuse to touch anything unless the
//! session's *live* token matches exactly, signal the pane pid's process
//! group, destroy the session, and clear the identity columns. Safe (and a
//! cheap no-op past the first DB read) to call for a run that was never
//! tmux-hosted at all — which is every run today, since `workers.tmux_hosting`
//! defaults off.
//!
//! There is no lower-level "kill by session name" reachable from here, or
//! from anywhere else in this crate: [`boss_tmux::Tmux::kill_session_verified`]
//! is the only teardown entry point `boss-tmux` exposes publicly, and it
//! requires the expected token as an argument — a `kill-session` on a name
//! alone is rejected at that API boundary, not by convention here. A token
//! mismatch means the session that currently answers to this name is not
//! the one this row's teardown owns — most likely because the original
//! session was already destroyed and its name recycled onto a different
//! execution — so this function refuses to signal or kill it and leaves
//! the identity columns in place for the leaked-session sweep
//! (`crate::husk_pane_sweep`) to reconcile.

use boss_tmux::{KillSessionError, KillSessionOutcome, Tmux};

use super::*;
use crate::work::TmuxIdentity;

/// Mirrors the private `BOSS_SPAWN_TOKEN_ENV` constant [`crate::spawn_flow`]
/// and [`crate::tmux_adoption`] each redeclare for their own authoritative
/// read — see `tmux_adoption`'s module doc for why this crate does not
/// share one constant across these reads.
const TMUX_SPAWN_TOKEN_ENV: &str = "BOSS_SPAWN_TOKEN";

/// What [`ServerState::reap_tmux_worker`] did for one execution's teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TmuxTeardownOutcome {
    /// No tmux session identity is durably recorded for this execution's
    /// current run — not a tmux-hosted worker, or an earlier teardown call
    /// already cleared the columns. Nothing to do.
    NotTmuxHosted,
    /// The session's live token matched (or the session was already gone),
    /// the process group was signalled when applicable, the session was
    /// destroyed, and the identity columns were cleared.
    Reaped,
    /// The session's live token did not match the durably-recorded one, or
    /// a `tmux` invocation failed outright. Refused to touch the session;
    /// the identity columns are left in place for a later pass to
    /// reconcile or retry.
    Refused,
}

impl ServerState {
    /// tmux-hosted counterpart of the process-tree signal
    /// [`Self::release_worker_pane`] / [`Self::reap_untracked_worker_process`]
    /// already send to `shell_pid`. See the module doc for the full
    /// sequence and why a bare kill-by-name is unreachable from here.
    pub(super) async fn reap_tmux_worker(&self, execution_id: &str) -> TmuxTeardownOutcome {
        let identity = match self.work_db.tmux_identity_for_execution(execution_id) {
            Ok(Some(identity)) => identity,
            Ok(None) => return TmuxTeardownOutcome::NotTmuxHosted,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    error = %format!("{err:#}"),
                    "reap_tmux_worker: failed reading tmux identity; treating as not tmux-hosted",
                );
                return TmuxTeardownOutcome::NotTmuxHosted;
            }
        };
        #[cfg(test)]
        {
            let override_tmux = self.tmux_override.lock().unwrap().clone();
            if let Some(tmux) = override_tmux {
                return self.reap_tmux_worker_with(&tmux, execution_id, &identity).await;
            }
        }
        let tmux = match Tmux::resolve() {
            Ok(tmux) => tmux,
            Err(err) => {
                tracing::error!(
                    execution_id,
                    session = %identity.session_name,
                    error = %format!("{err:#}"),
                    "reap_tmux_worker: tmux unresolvable; cannot verify or destroy the recorded session — \
                     leaving the identity columns for a retry",
                );
                return TmuxTeardownOutcome::Refused;
            }
        };
        self.reap_tmux_worker_with(&tmux, execution_id, &identity).await
    }

    /// Test-only: install a stubbed [`Tmux`] that [`Self::reap_tmux_worker`]
    /// uses instead of resolving a real tmux binary. Lets a test exercise
    /// the caller-side wiring (`release_worker_pane`,
    /// `reap_untracked_worker_process`) end-to-end against a scripted
    /// `CommandRunner`, the same way [`Self::reap_tmux_worker_with`]'s own
    /// direct-call tests do.
    #[cfg(test)]
    pub(crate) fn set_tmux_override_for_test(&self, tmux: Tmux) {
        *self.tmux_override.lock().unwrap() = Some(tmux);
    }

    /// [`Self::reap_tmux_worker`]'s dependency-injected body — split out so
    /// tests can drive it against a stubbed [`Tmux`] without a real tmux
    /// binary on the test host, mirroring [`crate::tmux_adoption`]'s
    /// `FakeTmuxServer` pattern. `pub(super)` (not private) so
    /// `app::tests::tmux_teardown` can call it directly.
    pub(super) async fn reap_tmux_worker_with(
        &self,
        tmux: &Tmux,
        execution_id: &str,
        identity: &TmuxIdentity,
    ) -> TmuxTeardownOutcome {
        match tmux
            .show_environment(&identity.session_name, TMUX_SPAWN_TOKEN_ENV)
            .await
        {
            Ok(Some(actual)) if actual == identity.spawn_token => {}
            Ok(Some(actual)) => {
                tracing::error!(
                    execution_id,
                    session = %identity.session_name,
                    expected = %identity.spawn_token,
                    actual,
                    "reap_tmux_worker: live session token does not match the durably-recorded one; \
                     refusing to signal or kill it — a leaked-session sweep must reconcile this",
                );
                self.dispatch_events
                    .emit(
                        crate::dispatch_events::DispatchEvent::new(
                            crate::dispatch_events::Stage::TmuxTokenMismatch,
                            crate::dispatch_events::Outcome::Error,
                            execution_id,
                        )
                        .with_details(serde_json::json!({
                            "tmux_session_name": identity.session_name,
                            "operation": "teardown_preflight",
                        })),
                    )
                    .await;
                return TmuxTeardownOutcome::Refused;
            }
            Ok(None) => {
                // Already gone (or never carried a Boss token at all).
                // Nothing to signal or kill — fall straight through to
                // clearing our own bookkeeping.
                return self.finish_tmux_reap(execution_id, identity);
            }
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    session = %identity.session_name,
                    error = %format!("{err:#}"),
                    "reap_tmux_worker: show-environment failed; leaving the session and identity columns \
                     alone for a retry",
                );
                return TmuxTeardownOutcome::Refused;
            }
        }

        // Verified match: safe to signal the recorded pane pid's process
        // group. This is the same ladder every app-hosted release already
        // runs (`reap_worker_process_tree`) — it stays because the reason
        // it exists stays: node-based agents commonly ignore the SIGHUP a
        // pty teardown delivers.
        if let Some(pane_pid) = identity
            .pane_pid
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|pid| *pid > 0)
        {
            reap_worker_process_tree(pane_pid, Duration::from_secs(5));
        }

        match tmux
            .kill_session_verified(&identity.session_name, &identity.spawn_token)
            .await
        {
            Ok(KillSessionOutcome::Killed | KillSessionOutcome::Absent) => {
                self.finish_tmux_reap(execution_id, identity)
            }
            Err(KillSessionError::TokenMismatch {
                session,
                expected,
                actual,
            }) => {
                tracing::error!(
                    execution_id,
                    session,
                    expected,
                    actual,
                    "reap_tmux_worker: session token changed between verification and kill; refusing to \
                     kill it",
                );
                self.dispatch_events
                    .emit(
                        crate::dispatch_events::DispatchEvent::new(
                            crate::dispatch_events::Stage::TmuxTokenMismatch,
                            crate::dispatch_events::Outcome::Error,
                            execution_id,
                        )
                        .with_details(serde_json::json!({
                            "tmux_session_name": session,
                            "operation": "teardown_kill",
                        })),
                    )
                    .await;
                TmuxTeardownOutcome::Refused
            }
            Err(KillSessionError::Tmux(err)) => {
                tracing::warn!(
                    execution_id,
                    session = %identity.session_name,
                    error = %format!("{err:#}"),
                    "reap_tmux_worker: kill-session failed; leaving the identity columns for a retry",
                );
                TmuxTeardownOutcome::Refused
            }
        }
    }

    fn finish_tmux_reap(&self, execution_id: &str, identity: &TmuxIdentity) -> TmuxTeardownOutcome {
        match self
            .work_db
            .clear_tmux_identity_for_execution(execution_id, &identity.spawn_token)
        {
            Ok(true) => {
                tracing::info!(
                    execution_id,
                    session = %identity.session_name,
                    "reap_tmux_worker: session reaped and identity columns cleared",
                );
                TmuxTeardownOutcome::Reaped
            }
            Ok(false) => {
                tracing::debug!(
                    execution_id,
                    "reap_tmux_worker: identity columns already cleared (idempotent)",
                );
                TmuxTeardownOutcome::Reaped
            }
            Err(err) => {
                // The session itself may already be dead/destroyed, but the
                // identity columns still claim a live tmux identity — a
                // future teardown call must retry the clear, so this must
                // NOT report `Reaped` (whose contract is "and the identity
                // columns were cleared").
                tracing::warn!(
                    execution_id,
                    error = %format!("{err:#}"),
                    "reap_tmux_worker: failed clearing tmux identity columns",
                );
                TmuxTeardownOutcome::Refused
            }
        }
    }
}
