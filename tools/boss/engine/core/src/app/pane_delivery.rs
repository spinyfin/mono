//! Verified pane-injection delivery.
//!
//! `SendToPane` only proves the engine handed bytes to the app, which
//! writes them to the worker's pty. It does not prove the foreground
//! process treated them as a pending user prompt. Text injected while
//! the worker is idle at its prompt (the `Stop`-boundary probe path)
//! has proven reliable. Text injected while the worker is actively
//! mid-turn — the urgent `PostToolUse` probe path, and the chore-update
//! auto-notice, which can land at any point in a turn — depends on what
//! the driver's foreground process does with mid-turn stdin: when it
//! does not consume stdin at all, injected bytes survive in the tty input
//! buffer and are later executed by the interactive shell
//! (ghostty-codex-pane-viability investigation, Q2 Layer D, measured
//! against the retired `codex exec` shape). That is a **safety** issue,
//! not hygiene, and it is why the trait default is
//! [`crate::driver::MidTurnPaneInput::Rejects`] — a driver earns
//! `Buffers` by measurement, not by argument.
//!
//! **A buffering driver may fold the write into the running turn.** Codex's
//! bare TUI does: the injected text is queued, delivered at the next
//! tool-call boundary, and answered *inside* the turn that was already
//! running, which emits one turn boundary for the two prompts. Claude
//! defers it into a fresh turn instead. Nothing in this module depends on
//! which — a write either reaches the agent or it does not — but callers
//! that go on to wait for a boundary do depend on it, so "the next turn
//! boundary" below means "the next one the driver actually emits", which for
//! a folding driver is the running turn's own.
//!
//! Before any write, [`ServerState::inject_pane_text_verified`] therefore
//! consults a [`PaneInputPosture`], which combines two orthogonal facts:
//!
//! * the slot's live [`boss_protocol::WorkerActivity`] — is the worker
//!   parked at its prompt ([`boss_protocol::WorkerActivity::accepts_typed_input`]),
//!   mid-turn, pre-session, or gone; and
//! * the run's driver — does its foreground process buffer mid-turn stdin
//!   ([`crate::driver::AgentDriver::mid_turn_pane_input`])?
//!
//! Conflating those two was the delivery bug: the guard refused *every*
//! mid-turn write, and the urgent probe path fires on `PostToolUse` where
//! the activity is `Working` by construction, so `probe --urgent` could
//! never deliver on any driver. Splitting them keeps the refusal exactly
//! where the tty-leak risk is (a driver whose process does not read
//! mid-turn stdin) and lets an interactive TUI take mid-turn input the
//! same way it takes a human's keystrokes.
//!
//! A refusal is surfaced as [`PaneInjectOutcome::NotAcceptingInput`] —
//! never silently dropped.
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
//! When the activity guard passes, this path still closes the
//! observability gap without over-correcting into duplicate delivery:
//! it waits for a `UserPromptSubmit` hook — the CLI's own confirmation
//! that it enqueued something as the next prompt — and, since that hook
//! firing for pane-injected text (as opposed to text the CLI itself
//! echoed) has never been validated end-to-end, it also falls back to
//! scanning the worker's session transcript for the injected text
//! before giving up. Callers that get back
//! [`PaneInjectOutcome::Unconfirmed`] must not treat that as "lost" and
//! auto-redeliver — they should record the unconfirmed state and let
//! whoever is watching the probe topic decide.

use super::*;
use boss_protocol::WorkerActivity;

/// Whether a pane write is permitted for a given `(activity, driver)` pair,
/// and if so at which of the two delivery postures.
///
/// Built with [`ServerState::pane_input_posture_for_run`], which resolves the
/// run's driver once and pairs it with the slot's live activity. Keeping this
/// a single value (rather than two separate predicate calls at each site)
/// means the urgent path, the Stop path and `bossctl agents send` cannot drift
/// apart on what they consider injectable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneInputPosture {
    /// The worker is parked at its prompt ([`WorkerActivity::Idle`] /
    /// [`WorkerActivity::WaitingForInput`]). The write becomes the next
    /// prompt immediately; this is safe under every driver.
    Parked,
    /// The worker is mid-turn ([`WorkerActivity::Working`]) and its driver
    /// declares [`crate::driver::MidTurnPaneInput::Buffers`]: the foreground
    /// process is reading stdin, so the write is held in the agent's composer
    /// — the same thing a human gets by typing into the pane while the agent
    /// works.
    ///
    /// *When* the agent then acts on it is the driver's business and differs
    /// between them: Claude submits it as a fresh turn once the current one
    /// ends; Codex's TUI folds it into the running turn, delivering it at the
    /// next tool-call boundary and answering it before that turn's single
    /// boundary. Both are "buffered"; only one of them yields a turn boundary
    /// of its own, which is why nothing downstream may assume one per
    /// delivered prompt.
    MidTurnBuffered,
    /// No write may be issued. Either there is no live state for the slot
    /// (fail closed — unknown is not "accepting"), the worker is pre-session
    /// or terminal, or it is mid-turn on a driver whose foreground process
    /// does not consume mid-turn stdin (the tty-leak case the guard exists
    /// for).
    Refused,
}

impl PaneInputPosture {
    /// Resolve the posture for a live `activity` (None = no live state for
    /// the slot) against a driver's mid-turn stdin behaviour.
    pub(crate) fn resolve(activity: Option<WorkerActivity>, mid_turn: crate::driver::MidTurnPaneInput) -> Self {
        match activity {
            Some(a) if a.accepts_typed_input() => Self::Parked,
            Some(WorkerActivity::Working) if mid_turn.buffers() => Self::MidTurnBuffered,
            _ => Self::Refused,
        }
    }

    /// True when a write may be issued at all.
    pub(crate) fn permits_write(self) -> bool {
        !matches!(self, Self::Refused)
    }

    /// True when the write lands in a mid-turn agent's input buffer rather
    /// than becoming its prompt straight away — the case where "no
    /// `UserPromptSubmit` yet" is expected rather than anomalous.
    pub(crate) fn is_mid_turn(self) -> bool {
        matches!(self, Self::MidTurnBuffered)
    }
}

/// One verified pane-injection request.
///
/// Bundled rather than passed as seven positional arguments so the call sites
/// name what they are supplying — in particular that `transcript_path` /
/// `offset_bytes` are a snapshot the caller took *before* the write (the same
/// one used for reply extraction), so the transcript fallback only ever scans
/// bytes written after the injection.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub(crate) struct PaneInjectRequest<'a> {
    pub(crate) run_id: &'a str,
    pub(crate) slot_id: u8,
    pub(crate) text: String,
    /// Transcript path to fall back to when no `UserPromptSubmit` hook
    /// arrives, or `None` when the run has no transcript recorded yet.
    pub(crate) transcript_path: Option<&'a str>,
    /// Byte length of `transcript_path` at snapshot time; the fallback scan
    /// starts here.
    pub(crate) offset_bytes: u64,
    /// How long to wait for a confirming signal before falling back to the
    /// transcript scan and then reporting `Buffered`/`Unconfirmed`.
    pub(crate) verify_timeout: Duration,
    /// The injectable posture the caller resolved via
    /// [`ServerState::pane_input_posture_for_run`].
    pub(crate) posture: PaneInputPosture,
}

/// Outcome of [`ServerState::inject_pane_text_verified`].
#[derive(Debug)]
pub(crate) enum PaneInjectOutcome {
    /// `SendToPane` succeeded and either a matching `UserPromptSubmit`
    /// hook or a transcript scan confirmed the text was consumed.
    Confirmed,
    /// `SendToPane` succeeded into a mid-turn agent whose driver buffers
    /// stdin ([`PaneInputPosture::MidTurnBuffered`]), and no
    /// `UserPromptSubmit` arrived inside the verification window. Unlike
    /// [`Self::Unconfirmed`] this is the *expected* shape of a successful
    /// mid-turn delivery: the agent is still finishing its turn, so it has
    /// not submitted the buffered text yet and cannot be expected to. The
    /// end-to-end confirmation for this case is the worker's reply at the
    /// next turn boundary the driver emits, which the in-flight probe
    /// machinery already captures.
    ///
    /// For a driver that folds the buffered prompt into the running turn
    /// (Codex's TUI) that boundary is the running turn's own, and the reply
    /// is already inside it — there is no second boundary to wait for and
    /// nothing may wait for one. A `UserPromptSubmit` confirmation is
    /// additionally unreachable on such a driver, because the folded prompt
    /// never starts a turn and so never mints one; the transcript fallback
    /// above is the only signal that can confirm it, and `Buffered` is the
    /// honest answer when neither lands inside the window.
    Buffered,
    /// `SendToPane` succeeded (bytes reached the app/pty) but neither a
    /// `UserPromptSubmit` hook nor a transcript scan confirmed delivery
    /// before the timeout. This is NOT proof the write was lost — the
    /// worker may have consumed it through a channel this engine can't
    /// yet observe (the probe-6 incident, corrected understanding).
    /// Callers must record this as an observable "unconfirmed" state
    /// rather than treating it as a failure and re-delivering the text.
    Unconfirmed,
    /// Refused before any pty write: the resolved [`PaneInputPosture`] was
    /// [`PaneInputPosture::Refused`] — the slot is pre-session or terminal,
    /// no live state is registered for it, or it is mid-turn on a driver that
    /// does not consume mid-turn stdin. `activity` is `None` when the
    /// registry has no entry for the slot (fail closed — unknown is not
    /// "accepting"). Callers must surface this rather than silently drop the
    /// text or re-issue the write.
    NotAcceptingInput { activity: Option<WorkerActivity> },
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

    /// Live activity for `slot_id`, if any, used by the typed-input
    /// guard. Missing state fails closed — see
    /// [`PaneInjectOutcome::NotAcceptingInput`].
    pub(super) fn pane_typed_input_activity(&self, slot_id: u8) -> Option<WorkerActivity> {
        self.live_worker_states.get(slot_id).map(|s| s.activity)
    }

    /// What this run's driver does with mid-turn pane input.
    ///
    /// Fails closed to [`crate::driver::MidTurnPaneInput::Rejects`] when the
    /// driver cannot be resolved (unknown execution, unregistered slug, DB
    /// error). An unresolvable driver is exactly the case where we cannot
    /// rule out the tty-leak, so it must not be treated as buffering.
    pub(super) fn run_mid_turn_pane_input(&self, run_id: &str) -> crate::driver::MidTurnPaneInput {
        use crate::driver::{DriverRegistry, MidTurnPaneInput};

        let slug = match self.work_db.get_execution_driver_slug(run_id) {
            Ok(Some(slug)) => slug,
            Ok(None) => {
                tracing::debug!(
                    run_id,
                    "pane posture: no driver recorded for run; refusing mid-turn input",
                );
                return MidTurnPaneInput::Rejects;
            }
            Err(err) => {
                tracing::warn!(
                    run_id,
                    ?err,
                    "pane posture: driver lookup failed; refusing mid-turn input",
                );
                return MidTurnPaneInput::Rejects;
            }
        };
        match DriverRegistry::default().get(&slug) {
            Some(driver) => driver.mid_turn_pane_input(),
            None => {
                tracing::warn!(
                    run_id,
                    driver = %slug,
                    "pane posture: driver slug not in registry; refusing mid-turn input",
                );
                MidTurnPaneInput::Rejects
            }
        }
    }

    /// Resolve the full [`PaneInputPosture`] for `run_id` on `slot_id`:
    /// the slot's live activity paired with the run driver's mid-turn stdin
    /// behaviour. Every pane-injection site goes through this so they cannot
    /// disagree about what is injectable.
    pub(super) fn pane_input_posture_for_run(&self, run_id: &str, slot_id: u8) -> PaneInputPosture {
        let activity = self.pane_typed_input_activity(slot_id);
        // Skip the driver lookup entirely when the activity alone decides —
        // parked slots are injectable under every driver, and pre-session /
        // terminal slots under none. Only `Working` needs the DB round-trip.
        if activity.is_some_and(WorkerActivity::accepts_typed_input) {
            return PaneInputPosture::Parked;
        }
        if activity != Some(WorkerActivity::Working) {
            return PaneInputPosture::Refused;
        }
        PaneInputPosture::resolve(activity, self.run_mid_turn_pane_input(run_id))
    }

    /// Write `req.text` into `req.run_id`'s worker pane (`req.slot_id`) and
    /// wait up to `req.verify_timeout` for confirmation that the CLI actually
    /// enqueued it as the next prompt, rather than merely accepting the
    /// pty write.
    ///
    /// **Posture guard (first):** `req.posture` must have been resolved by the
    /// caller via [`Self::pane_input_posture_for_run`]. A
    /// [`PaneInputPosture::Refused`] posture returns
    /// [`PaneInjectOutcome::NotAcceptingInput`] with no bytes written to the
    /// pty — this is the safety guard for foreground processes that do not
    /// consume mid-turn stdin.
    ///
    /// **Confirmation (when the guard passes):** either a matching
    /// `UserPromptSubmit` hook, or (since that hook firing for
    /// pane-injected text has never been validated end-to-end) a scan
    /// of the worker's session transcript for the injected text — so a
    /// gap in one signal doesn't by itself produce a false
    /// "unconfirmed".
    ///
    /// When neither signal confirms in time the result depends on the
    /// posture, because "no prompt submitted yet" means different things in
    /// each: a [`PaneInputPosture::Parked`] worker should have submitted
    /// immediately, so that is [`PaneInjectOutcome::Unconfirmed`]; a
    /// [`PaneInputPosture::MidTurnBuffered`] worker is still finishing its
    /// turn and is *expected* not to have submitted yet, so that is
    /// [`PaneInjectOutcome::Buffered`]. Neither is an error, and neither is
    /// proof of loss — see the module docs for why callers must not
    /// auto-redeliver.
    pub(super) async fn inject_pane_text_verified(&self, req: PaneInjectRequest<'_>) -> PaneInjectOutcome {
        let PaneInjectRequest {
            run_id,
            slot_id,
            text,
            transcript_path,
            offset_bytes,
            verify_timeout,
            posture,
        } = req;
        let activity = self.pane_typed_input_activity(slot_id);
        if !posture.permits_write() {
            tracing::warn!(
                run_id,
                slot_id,
                activity = activity.map(WorkerActivity::as_str),
                "pane injection refused: no injectable posture for this worker/driver pair"
            );
            return PaneInjectOutcome::NotAcceptingInput { activity };
        }

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
                if posture.is_mid_turn() {
                    // Expected: the agent is still mid-turn, so it has not
                    // submitted the buffered text yet. Not an anomaly, and
                    // not a delivery failure.
                    tracing::info!(
                        run_id,
                        slot_id,
                        "pane injection buffered by a mid-turn agent; the agent submits it when its \
                         composer next drains — for a folding driver that is inside the running turn, \
                         for a deferring one it is the turn after",
                    );
                    return PaneInjectOutcome::Buffered;
                }
                PaneInjectOutcome::Unconfirmed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Behavior tests for the transcript-confirmation fallback helpers.
    //!
    //! These assert observable outcomes of `normalize_ws` and
    //! `transcript_shows_text` — the correctness-sensitive fallback path
    //! `inject_pane_text_verified` relies on when no `UserPromptSubmit`
    //! hook arrives to confirm injected text. They are deliberately
    //! hermetic: every transcript is a tempfile written in-test, so no
    //! real session transcripts are touched and results are deterministic.

    use super::{normalize_ws, transcript_shows_text};
    use std::io::Write;

    /// Write `bytes` to a fresh tempfile and return `(handle, path)`. The
    /// handle must be kept alive for the file to survive; the `String`
    /// path is what `transcript_shows_text` takes.
    fn transcript_with(bytes: &[u8]) -> (tempfile::NamedTempFile, String) {
        let mut file = tempfile::NamedTempFile::new().expect("create tempfile");
        file.write_all(bytes).expect("write transcript bytes");
        file.flush().expect("flush transcript");
        let path = file.path().to_str().expect("utf-8 tempfile path").to_owned();
        (file, path)
    }

    #[test]
    fn normalize_ws_collapses_whitespace_runs_to_single_spaces() {
        assert_eq!(normalize_ws("chore   update\t\tnotice"), "chore update notice");
    }

    #[test]
    fn normalize_ws_collapses_newlines_and_trims_ends() {
        assert_eq!(
            normalize_ws("  \n  first line\n\n  second line  \n"),
            "first line second line"
        );
    }

    #[test]
    fn normalize_ws_empty_input_is_empty() {
        assert_eq!(normalize_ws(""), "");
    }

    #[test]
    fn normalize_ws_whitespace_only_input_is_empty() {
        assert_eq!(normalize_ws("  \n\t  \r\n "), "");
    }

    #[tokio::test]
    async fn transcript_shows_text_false_when_file_missing() {
        // A path that was never created — File::open fails, so the
        // scan reports "not confirmed" rather than panicking.
        let missing = tempfile::NamedTempFile::new().unwrap();
        let path = missing.path().to_str().unwrap().to_owned();
        drop(missing); // remove the file so the path no longer exists
        assert!(!transcript_shows_text(&path, 0, "anything").await);
    }

    #[tokio::test]
    async fn transcript_shows_text_false_when_length_at_or_below_offset() {
        let body = b"prompt text recorded here";
        let (_f, path) = transcript_with(body);
        // offset exactly at EOF: nothing new to scan.
        assert!(!transcript_shows_text(&path, body.len() as u64, "prompt").await);
        // offset past EOF: same, guarded by the `<= offset_bytes` check.
        assert!(!transcript_shows_text(&path, body.len() as u64 + 100, "prompt").await);
    }

    #[tokio::test]
    async fn transcript_shows_text_only_scans_bytes_past_offset() {
        // "SECRET" lives entirely before the offset; the injected text
        // "injected message" lives after it. The scan must ignore the
        // pre-offset region and only match content written since the
        // injection snapshot was taken.
        let prefix = b"SECRET before the offset ";
        let suffix = b"injected message after the offset";
        let mut body = Vec::new();
        body.extend_from_slice(prefix);
        body.extend_from_slice(suffix);
        let (_f, path) = transcript_with(&body);

        let offset = prefix.len() as u64;
        // Pre-offset text is invisible to the scan.
        assert!(!transcript_shows_text(&path, offset, "SECRET").await);
        // Post-offset text is found.
        assert!(transcript_shows_text(&path, offset, "injected message").await);
    }

    #[tokio::test]
    async fn transcript_shows_text_matches_across_whitespace_reflow() {
        // The transcript records the prompt on a single line with
        // collapsed spacing; the needle we search with is the original
        // multi-line chore-update text. Normalized comparison bridges
        // the difference so reflow doesn't produce a false "unconfirmed".
        let (_f, path) = transcript_with(b"...before... please update the spec now ...after...");
        let needle = "please   update\nthe   spec\n\nnow";
        assert!(transcript_shows_text(&path, 0, needle).await);
    }

    #[tokio::test]
    async fn transcript_shows_text_false_for_empty_or_whitespace_needle() {
        // A non-empty chunk must not be "confirmed" by an empty needle —
        // normalized, both the empty string and a whitespace-only string
        // reduce to "" and are rejected before the substring check.
        let (_f, path) = transcript_with(b"a non-empty transcript chunk");
        assert!(!transcript_shows_text(&path, 0, "").await);
        assert!(!transcript_shows_text(&path, 0, "   \n\t ").await);
    }

    #[tokio::test]
    async fn transcript_shows_text_false_on_invalid_utf8_chunk() {
        // A partial/corrupt multibyte sequence in the new chunk makes
        // String::from_utf8 fail; the scan reports "not confirmed"
        // rather than erroring or matching arbitrarily.
        let (_f, path) = transcript_with(&[0xff, 0xfe, 0x00, 0x9f]);
        assert!(!transcript_shows_text(&path, 0, "anything").await);
    }
}
