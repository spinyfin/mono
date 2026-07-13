//! Verified pane-injection delivery.
//!
//! `SendToPane` only proves the engine handed bytes to the app, which
//! writes them to the worker's pty. It does not prove Claude Code's
//! CLI treated them as a pending user prompt. Text injected while the
//! worker is idle at its prompt (the `Stop`-boundary probe path) has
//! proven reliable, but text injected while the worker is actively
//! mid-turn — the urgent `PostToolUse` probe path, and the
//! chore-update auto-notice, which can land at any point in a turn —
//! races the TUI's input handling. In one incident an urgent probe and
//! a chore-update notice both vanished this way: the engine logged
//! "injected" and the worker ran on for 20+ minutes on the stale spec.
//!
//! [`ServerState::inject_pane_text_verified`] closes that gap by
//! waiting for a `UserPromptSubmit` hook — the CLI's own confirmation
//! that it enqueued something as the next prompt — before treating a
//! write as delivered.

use super::*;

/// Outcome of [`ServerState::inject_pane_text_verified`].
#[derive(Debug)]
pub(crate) enum PaneInjectOutcome {
    /// `SendToPane` succeeded and a matching `UserPromptSubmit` hook
    /// arrived within the verification window.
    Confirmed,
    /// `SendToPane` succeeded (bytes reached the app/pty) but no
    /// matching `UserPromptSubmit` arrived before the timeout — the
    /// probe-6 failure mode. The write is not necessarily lost (the
    /// worker may still be mid-turn and pick it up later), but it
    /// must not be trusted as delivered.
    Unverified,
    /// `SendToPane` itself failed at the transport or app layer.
    /// Carries enough detail for callers that need a typed error
    /// (e.g. [`ServerState::send_input_to_worker`]'s `SendInputError`)
    /// to reconstruct it without re-issuing the write.
    SendFailed(PaneSendFailure),
}

/// Failure detail for [`PaneInjectOutcome::SendFailed`].
#[derive(Debug)]
pub(crate) enum PaneSendFailure {
    App(EngineToAppError),
    Send(SendToAppError),
    ResponseKindMismatch(String),
}

impl ServerState {
    /// Register a one-shot waiter for the next `UserPromptSubmit`
    /// hook on `run_id`. See the `delivery_waiters` field docs.
    pub(super) fn register_delivery_waiter(&self, run_id: &str) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        self.delivery_waiters
            .lock()
            .expect("delivery_waiters mutex poisoned")
            .insert(run_id.to_owned(), tx);
        rx
    }

    /// Drop a delivery waiter without resolving it — used when the
    /// `SendToPane` write itself failed, so no `UserPromptSubmit`
    /// confirmation will ever follow for this attempt.
    pub(super) fn take_delivery_waiter(&self, run_id: &str) {
        self.delivery_waiters
            .lock()
            .expect("delivery_waiters mutex poisoned")
            .remove(run_id);
    }

    /// Resolve the delivery waiter for `run_id`, if any, with the
    /// `UserPromptSubmit` prompt text that just arrived. Called from
    /// `dispatch_live_worker_state` on every `UserPromptSubmit` hook;
    /// a no-op when nothing is waiting, which is the ordinary case —
    /// most prompts are the worker's own turns, not engine-injected
    /// text.
    pub(super) fn resolve_delivery_waiter(&self, run_id: &str, prompt: &str) {
        if let Some(tx) = self
            .delivery_waiters
            .lock()
            .expect("delivery_waiters mutex poisoned")
            .remove(run_id)
        {
            let _ = tx.send(prompt.to_owned());
        }
    }

    /// Write `text` into `run_id`'s worker pane (`slot_id`) and wait
    /// up to `verify_timeout` for a `UserPromptSubmit` hook whose
    /// prompt contains `text`, confirming the CLI actually enqueued
    /// it as the next prompt rather than merely accepting the pty
    /// write. This closes the probe-6 incident gap: the engine used
    /// to log "injected" the instant `SendToPane` returned Ok, even
    /// when the keystrokes raced the TUI mid-turn and were dropped
    /// before becoming a queued prompt.
    pub(super) async fn inject_pane_text_verified(
        &self,
        run_id: &str,
        slot_id: u8,
        text: String,
        verify_timeout: Duration,
    ) -> PaneInjectOutcome {
        let waiter = self.register_delivery_waiter(run_id);
        let request = EngineToAppRequest::SendToPane(SendToPaneInput {
            slot_id,
            text: text.clone(),
        });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::SendToPane { result: Ok(_) }) => {}
            Ok(EngineToAppResponse::SendToPane { result: Err(err) }) => {
                self.take_delivery_waiter(run_id);
                tracing::warn!(?err, run_id, slot_id, "pane injection rejected by app");
                return PaneInjectOutcome::SendFailed(PaneSendFailure::App(err));
            }
            Ok(other) => {
                self.take_delivery_waiter(run_id);
                tracing::warn!(run_id, slot_id, ?other, "pane injection: unexpected app response shape");
                return PaneInjectOutcome::SendFailed(PaneSendFailure::ResponseKindMismatch(format!("{other:?}")));
            }
            Err(err) => {
                self.take_delivery_waiter(run_id);
                tracing::warn!(?err, run_id, slot_id, "pane injection transport failed");
                return PaneInjectOutcome::SendFailed(PaneSendFailure::Send(err));
            }
        }
        match timeout(verify_timeout, waiter).await {
            Ok(Ok(prompt)) if prompt.contains(text.trim()) => PaneInjectOutcome::Confirmed,
            Ok(Ok(prompt)) => {
                tracing::warn!(
                    run_id,
                    slot_id,
                    %prompt,
                    injected = %text,
                    "pane injection: a UserPromptSubmit arrived but its text did not match the injected \
                     text; treating as unverified",
                );
                PaneInjectOutcome::Unverified
            }
            // Sender dropped: either a later registration for the
            // same run overwrote ours (a concurrent injection raced
            // us — rare, since dispatch is normally serialized per
            // run), or the waiter was already taken by a cleanup
            // path. Either way, no confirmation for *this* write.
            Ok(Err(_)) => PaneInjectOutcome::Unverified,
            Err(_) => {
                // Best-effort cleanup: if a newer waiter for this run
                // has since replaced ours, this removes that one
                // instead — a narrow, harmless race given injection
                // is normally serialized per run (see field docs).
                self.take_delivery_waiter(run_id);
                PaneInjectOutcome::Unverified
            }
        }
    }
}
