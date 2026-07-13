//! Verified pane-injection delivery.
//!
//! `SendToPane` only proves the engine handed bytes to the app, which
//! writes them to the worker's pty. It does not prove Claude Code's
//! CLI treated them as a pending user prompt. Text injected while the
//! worker is idle at its prompt (the `Stop`-boundary probe path) has
//! proven reliable, but text injected while the worker is actively
//! mid-turn — the urgent `PostToolUse` probe path, and the
//! chore-update auto-notice, which can land at any point in a turn —
//! races the TUI's input handling.
//!
//! The probe-6 incident originally looked like a silent delivery
//! loss: the engine logged "injected" and the worker ran on for 20+
//! minutes on the stale spec. A 2026-07-13 correction to that
//! incident's spec established the opposite: the worker *had* acted
//! on the updated text — the defect was that delivery was
//! unverifiable, not lost. That reframes what a missing confirmation
//! means: it is evidence of an *observability gap*, not proof the
//! write evaporated. Treating it as proof-of-loss and automatically
//! re-delivering the text (the previous behavior here) risks handing
//! the worker the same instruction twice.
//!
//! [`ServerState::inject_pane_text_verified`] closes the observability
//! gap without over-correcting into duplicate delivery: it waits for a
//! `UserPromptSubmit` hook — the CLI's own confirmation that it
//! enqueued something as the next prompt — and, since that hook firing
//! for pane-injected text (as opposed to text the CLI itself echoed)
//! has never been validated end-to-end, it also falls back to scanning
//! the worker's session transcript for the injected text before giving
//! up. Callers that get back [`PaneInjectOutcome::Unconfirmed`] must
//! not treat that as "lost" and auto-redeliver — they should record the
//! unconfirmed state and let whoever is watching the probe topic decide.

use super::*;

/// Outcome of [`ServerState::inject_pane_text_verified`].
#[derive(Debug)]
pub(crate) enum PaneInjectOutcome {
    /// `SendToPane` succeeded and either a matching `UserPromptSubmit`
    /// hook or a transcript scan confirmed the text was consumed.
    Confirmed,
    /// `SendToPane` succeeded (bytes reached the app/pty) but neither a
    /// `UserPromptSubmit` hook nor a transcript scan confirmed delivery
    /// before the timeout. This is NOT proof the write was lost — the
    /// worker may have consumed it through a channel this engine can't
    /// yet observe (the probe-6 incident, corrected understanding).
    /// Callers must record this as an observable "unconfirmed" state
    /// rather than treating it as a failure and re-delivering the text.
    Unconfirmed,
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

/// Collapse runs of whitespace (including newlines) to single spaces
/// and trim the ends. Chore-update text can be multi-line, and the
/// TUI's input handling may reflow or re-wrap it before the prompt is
/// recorded, so exact substring matching on raw text is brittle —
/// comparing normalized forms tolerates that without requiring an
/// exact verbatim match.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Best-effort check for whether `text` appears anywhere in
/// `transcript_path` past `offset_bytes`. Used as the fallback
/// confirmation signal when no `UserPromptSubmit` hook arrives in
/// time — deliberately permissive (raw substring over the whole new
/// chunk, not scoped to a particular JSONL message shape) since the
/// point is to catch injected text recorded under a transcript shape
/// this engine doesn't otherwise parse, not to validate structure.
async fn transcript_shows_text(transcript_path: &str, offset_bytes: u64, text: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    let Ok(mut file) = tokio::fs::File::open(transcript_path).await else {
        return false;
    };
    let Ok(metadata) = file.metadata().await else {
        return false;
    };
    if metadata.len() <= offset_bytes {
        return false;
    }
    if file.seek(SeekFrom::Start(offset_bytes)).await.is_err() {
        return false;
    }
    let mut buf = Vec::with_capacity((metadata.len() - offset_bytes) as usize);
    if file.read_to_end(&mut buf).await.is_err() {
        return false;
    }
    let Ok(chunk) = String::from_utf8(buf) else {
        return false;
    };
    let needle = normalize_ws(text.trim());
    !needle.is_empty() && normalize_ws(&chunk).contains(&needle)
}

impl ServerState {
    /// Register a one-shot waiter for the next `UserPromptSubmit` hook
    /// on `run_id` that matches `match_text`. Returns a token that
    /// identifies exactly this waiter among any others concurrently
    /// registered for the same run — see the `delivery_waiters` field
    /// docs for why a run can have more than one outstanding waiter.
    pub(super) fn register_delivery_waiter(&self, run_id: &str, match_text: &str) -> (u64, oneshot::Receiver<String>) {
        let (tx, rx) = oneshot::channel();
        let token = self.next_delivery_token.fetch_add(1, Ordering::Relaxed);
        self.delivery_waiters
            .lock()
            .expect("delivery_waiters mutex poisoned")
            .entry(run_id.to_owned())
            .or_default()
            .push(DeliveryWaiter {
                token,
                match_text: normalize_ws(match_text.trim()),
                tx,
            });
        (token, rx)
    }

    /// Drop the delivery waiter identified by `token` (if it's still
    /// present) without resolving it — used when the `SendToPane`
    /// write itself failed, or when the verification window elapsed,
    /// so no confirmation will ever follow for this specific attempt.
    /// Only removes the matching token, leaving any other waiters for
    /// the same run untouched.
    pub(super) fn take_delivery_waiter(&self, run_id: &str, token: u64) {
        let mut guard = self.delivery_waiters.lock().expect("delivery_waiters mutex poisoned");
        if let Some(waiters) = guard.get_mut(run_id) {
            waiters.retain(|w| w.token != token);
            if waiters.is_empty() {
                guard.remove(run_id);
            }
        }
    }

    /// Resolve the delivery waiter for `run_id` whose `match_text` is
    /// contained in `prompt` (both normalized), if any. Called from
    /// `dispatch_live_worker_state` on every `UserPromptSubmit` hook; a
    /// no-op when nothing matches, which is the ordinary case — most
    /// prompts are the worker's own turns, not engine-injected text.
    /// Matching on content (rather than "the first waiter for this
    /// run") means an unrelated prompt arriving while a wait is
    /// outstanding cannot steal the waiter for different injected text.
    pub(super) fn resolve_delivery_waiter(&self, run_id: &str, prompt: &str) {
        let mut guard = self.delivery_waiters.lock().expect("delivery_waiters mutex poisoned");
        let Some(waiters) = guard.get_mut(run_id) else {
            return;
        };
        let normalized_prompt = normalize_ws(prompt);
        let Some(idx) = waiters
            .iter()
            .position(|w| !w.match_text.is_empty() && normalized_prompt.contains(&w.match_text))
        else {
            return;
        };
        let waiter = waiters.remove(idx);
        if waiters.is_empty() {
            guard.remove(run_id);
        }
        drop(guard);
        let _ = waiter.tx.send(prompt.to_owned());
    }

    /// Write `text` into `run_id`'s worker pane (`slot_id`) and wait up
    /// to `verify_timeout` for confirmation that the CLI actually
    /// enqueued it as the next prompt, rather than merely accepting the
    /// pty write. Confirmation comes from either of two independent
    /// signals — a matching `UserPromptSubmit` hook, or (since that
    /// hook firing for pane-injected text has never been validated
    /// end-to-end) a scan of the worker's session transcript for the
    /// injected text — so a gap in one signal doesn't by itself produce
    /// a false "unconfirmed".
    ///
    /// `transcript_path`/`offset_bytes` should be captured by the
    /// caller *before* this call (the same snapshot used for reply
    /// extraction) so the transcript fallback only looks at bytes
    /// written after the injection, not pre-existing content.
    ///
    /// Returns [`PaneInjectOutcome::Unconfirmed`], not an error, when
    /// neither signal confirms in time — see the module docs for why
    /// callers must not treat that as proof of loss.
    pub(super) async fn inject_pane_text_verified(
        &self,
        run_id: &str,
        slot_id: u8,
        text: String,
        transcript_path: Option<&str>,
        offset_bytes: u64,
        verify_timeout: Duration,
    ) -> PaneInjectOutcome {
        let (token, waiter) = self.register_delivery_waiter(run_id, &text);
        let request = EngineToAppRequest::SendToPane(SendToPaneInput {
            slot_id,
            text: text.clone(),
        });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::SendToPane { result: Ok(_) }) => {}
            Ok(EngineToAppResponse::SendToPane { result: Err(err) }) => {
                self.take_delivery_waiter(run_id, token);
                tracing::warn!(?err, run_id, slot_id, "pane injection rejected by app");
                return PaneInjectOutcome::SendFailed(PaneSendFailure::App(err));
            }
            Ok(other) => {
                self.take_delivery_waiter(run_id, token);
                tracing::warn!(run_id, slot_id, ?other, "pane injection: unexpected app response shape");
                return PaneInjectOutcome::SendFailed(PaneSendFailure::ResponseKindMismatch(format!("{other:?}")));
            }
            Err(err) => {
                self.take_delivery_waiter(run_id, token);
                tracing::warn!(?err, run_id, slot_id, "pane injection transport failed");
                return PaneInjectOutcome::SendFailed(PaneSendFailure::Send(err));
            }
        }
        match timeout(verify_timeout, waiter).await {
            Ok(Ok(_prompt)) => PaneInjectOutcome::Confirmed,
            // Sender dropped without resolving: the timeout will handle
            // cleanup below via the transcript fallback, since a UserPromptSubmit
            // for unrelated text may still arrive later and we don't want
            // to wait on it. Fall straight through to the transcript check.
            Ok(Err(_)) | Err(_) => {
                self.take_delivery_waiter(run_id, token);
                if let Some(path) = transcript_path
                    && transcript_shows_text(path, offset_bytes, &text).await
                {
                    tracing::info!(
                        run_id,
                        slot_id,
                        "pane injection confirmed via transcript scan (no UserPromptSubmit observed)",
                    );
                    return PaneInjectOutcome::Confirmed;
                }
                PaneInjectOutcome::Unconfirmed
            }
        }
    }
}
