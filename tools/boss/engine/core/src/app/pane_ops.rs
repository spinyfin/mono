//! `ServerState` methods for the small, uniformly-shaped engine→app
//! pane RPCs: focus / send-input / interrupt / reveal-work-item /
//! retire-pane / list-husk-panes. Split out of `app.rs` for file-size
//! hygiene; behavior is unchanged from when these lived inline. See
//! [`super::panes`] for the `FrontendRequest` handlers that call into
//! most of these (`reveal_work_item` is called from `app/work_items.rs`
//! instead, since it's keyed by work-item id rather than run id).

use super::*;

/// Surfaced by [`ServerState::focus_worker_pane`]. Distinguishes
/// engine-side resolution failures (run id has no allocated slot)
/// from transport/app failures so the `bossctl` handler can produce
/// a precise error message.
#[derive(Debug, thiserror::Error)]
pub enum FocusPaneError {
    #[error("no worker pane mapped for that run id")]
    UnknownRun,
    #[error("app reported error: {0:?}")]
    App(EngineToAppError),
    #[error(transparent)]
    Send(#[from] SendToAppError),
    #[error("app returned unexpected response: {0}")]
    ResponseKindMismatch(String),
}

/// Surfaced by [`ServerState::send_input_to_worker`]. Same shape as
/// [`FocusPaneError`]: separates "no slot mapping for that run id"
/// from app-side / transport failures so `bossctl agents send` can
/// produce a precise error message.
#[derive(Debug, thiserror::Error)]
pub enum SendInputError {
    #[error("no worker pane mapped for that run id")]
    UnknownRun,
    #[error("app reported error: {0:?}")]
    App(EngineToAppError),
    #[error(transparent)]
    Send(#[from] SendToAppError),
    #[error("app returned unexpected response: {0}")]
    ResponseKindMismatch(String),
}

/// Surfaced by [`ServerState::interrupt_worker_pane`]. Mirrors
/// [`FocusPaneError`] — the same error tiers apply (resolution miss,
/// app failure, transport, response shape).
#[derive(Debug, thiserror::Error)]
pub enum InterruptPaneError {
    #[error("no worker pane mapped for that run id")]
    UnknownRun,
    #[error("app reported error: {0:?}")]
    App(EngineToAppError),
    #[error(transparent)]
    Send(#[from] SendToAppError),
    #[error("app returned unexpected response: {0}")]
    ResponseKindMismatch(String),
}

/// Surfaced by [`ServerState::reveal_work_item`]. Separates
/// id-resolution failures from app-side / transport failures so
/// `bossctl reveal` can produce a precise error.
#[derive(Debug, thiserror::Error)]
pub enum RevealItemError {
    #[error("no work item found for id: {0}")]
    NotFound(String),
    #[error("work item {0} is deleted")]
    Deleted(String),
    #[error("app reported error: {0:?}")]
    App(EngineToAppError),
    #[error(transparent)]
    Send(#[from] SendToAppError),
    #[error("app returned unexpected response: {0}")]
    ResponseKindMismatch(String),
}

/// Surfaced by [`ServerState::open_document`]. Separates path-validation
/// failures (checked engine-side so the app stays a thin reader) from
/// app-side / transport failures, mirroring [`RevealItemError`].
#[derive(Debug, thiserror::Error)]
pub enum OpenDocumentError {
    #[error("no such file: {0}")]
    NotFound(String),
    #[error("not a regular file: {0}")]
    NotAFile(String),
    #[error("file is not readable: {0}")]
    NotReadable(String),
    #[error("not a markdown file (expected .md or .markdown): {0}")]
    NotMarkdown(String),
    /// Distinguished from the generic [`SendToAppError::NotRegistered`]
    /// Display text so `bossctl open` fails with an actionable remedy
    /// instead of a bare "no app session is registered" — the operator
    /// can't act on that alone.
    #[error("no Boss app session is registered — launch (or relaunch) the Boss app and try again")]
    NoAppSession,
    #[error("app reported error: {0:?}")]
    App(EngineToAppError),
    #[error(transparent)]
    Send(#[from] SendToAppError),
    #[error("app returned unexpected response: {0}")]
    ResponseKindMismatch(String),
}

/// Surfaced by [`ServerState::retire_pane`] / [`ServerState::list_husk_panes`].
#[derive(Debug, thiserror::Error)]
pub enum RetirePaneError {
    /// The engine's own `LiveWorkerStateRegistry` still shows a live,
    /// non-terminal run in this slot — it is not a husk. Refusing here
    /// is the safety check the break-glass verb exists to have: a
    /// caller must go through `agents stop` to tear down a pane the
    /// engine still considers active.
    #[error(
        "slot {slot_id} has a live engine-tracked run ({run_id}); \
         use `bossctl agents stop {run_id}` instead of retire-pane"
    )]
    LiveRunTracked { slot_id: u8, run_id: String },
    #[error("app reported error: {0:?}")]
    App(EngineToAppError),
    #[error(transparent)]
    Send(#[from] SendToAppError),
    #[error("app returned unexpected response: {0}")]
    ResponseKindMismatch(String),
}

impl ServerState {
    /// Resolve `run_id → slot_id` and ask the app to bring that
    /// worker pane to the front. Returns the resolved slot on success
    /// so callers (`bossctl agents focus`) can confirm in JSON output
    /// which slot was raised.
    pub async fn focus_worker_pane(&self, run_id: &str) -> Result<u8, FocusPaneError> {
        let Some(slot_id) = self.worker_registry.slot_for_run(run_id) else {
            return Err(FocusPaneError::UnknownRun);
        };
        let request = EngineToAppRequest::FocusWorkerPane(FocusWorkerPaneInput { slot_id });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::FocusWorkerPane { result: Ok(_) }) => Ok(slot_id),
            Ok(EngineToAppResponse::FocusWorkerPane { result: Err(err) }) => Err(FocusPaneError::App(err)),
            Ok(other) => Err(FocusPaneError::ResponseKindMismatch(format!("{other:?}"))),
            Err(err) => Err(FocusPaneError::Send(err)),
        }
    }

    /// Resolve `run_id → slot_id` and ask the app to write `text`
    /// into that worker pane as if the user had typed it. Returns the
    /// resolved slot on success so `bossctl agents send` can echo back
    /// which pane was targeted (useful when the agent reference was a
    /// crew name). Mirrors [`focus_worker_pane`] in shape, but this
    /// call can land at any point in the worker's turn — including
    /// mid-tool-call — so unlike a plain `SendToPane` it verifies the
    /// write actually became a queued prompt (see
    /// `inject_pane_text_verified`). This is the chore-update
    /// auto-notice path implicated in the probe-6 incident.
    ///
    /// The corrected understanding of that incident (2026-07-13) is
    /// that an unconfirmed write is *not* proof of loss — the text may
    /// still have reached the worker through a channel this engine
    /// can't yet observe. So on
    /// [`PaneInjectOutcome::Unconfirmed`] this does **not** fall back
    /// to `queue_probe`: doing so risks delivering the same notice to
    /// the worker a second time at its next `Stop` boundary, which is
    /// exactly the duplicate-delivery outcome the corrected spec warns
    /// against. Instead it returns success (the write did reach the
    /// pane) and leaves the unconfirmed state observable via the
    /// probe/lifecycle machinery rather than silently retrying.
    pub async fn send_input_to_worker(&self, run_id: &str, text: String) -> Result<u8, SendInputError> {
        let Some(slot_id) = self.worker_registry.slot_for_run(run_id) else {
            return Err(SendInputError::UnknownRun);
        };
        let (transcript_path, offset_bytes) = super::worker_events::transcript_offset_for_run(self, run_id).await;
        match self
            .inject_pane_text_verified(
                run_id,
                slot_id,
                text.clone(),
                transcript_path.as_deref(),
                offset_bytes,
                Duration::from_secs(6),
            )
            .await
        {
            PaneInjectOutcome::Confirmed => Ok(slot_id),
            PaneInjectOutcome::Unconfirmed => {
                tracing::warn!(
                    run_id,
                    slot_id,
                    "send_input_to_worker: pane write unconfirmed (no UserPromptSubmit or transcript match \
                     observed within the window); NOT re-queuing as a probe, since the corrected probe-6 \
                     understanding is that the text likely still reached the worker and redelivery would risk \
                     duplicating it",
                );
                Ok(slot_id)
            }
            PaneInjectOutcome::SendFailed(PaneSendFailure::App(err)) => Err(SendInputError::App(err)),
            PaneInjectOutcome::SendFailed(PaneSendFailure::Send(err)) => Err(SendInputError::Send(err)),
            PaneInjectOutcome::SendFailed(PaneSendFailure::ResponseKindMismatch(msg)) => {
                Err(SendInputError::ResponseKindMismatch(msg))
            }
        }
    }

    /// Resolve `run_id → slot_id` and ask the app to deliver an Esc
    /// keystroke to that worker pane's pty — equivalent to the human
    /// pressing Esc with the pane focused. The worker run stays
    /// alive; only the in-flight turn is cancelled. Returns the
    /// resolved slot on success so callers (`bossctl agents
    /// interrupt`) can confirm in JSON output which slot received
    /// the interrupt.
    pub async fn interrupt_worker_pane(&self, run_id: &str) -> Result<u8, InterruptPaneError> {
        let Some(slot_id) = self.worker_registry.slot_for_run(run_id) else {
            return Err(InterruptPaneError::UnknownRun);
        };
        let request = EngineToAppRequest::InterruptWorkerPane(InterruptWorkerPaneInput { slot_id });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::InterruptWorkerPane { result: Ok(_) }) => Ok(slot_id),
            Ok(EngineToAppResponse::InterruptWorkerPane { result: Err(err) }) => Err(InterruptPaneError::App(err)),
            Ok(other) => Err(InterruptPaneError::ResponseKindMismatch(format!("{other:?}"))),
            Err(err) => Err(InterruptPaneError::Send(err)),
        }
    }

    /// Resolve `id` (short-form `T607` or canonical) to a work item
    /// and ask the app to scroll the kanban to that card and play a
    /// short transient highlight. Returns the canonical id on success
    /// so `bossctl reveal` can confirm what was highlighted.
    pub async fn reveal_work_item(&self, id: &str) -> Result<String, RevealItemError> {
        let item = self
            .work_db
            .get_work_item_resolving_short_id(id)
            .map_err(|_| RevealItemError::NotFound(id.to_owned()))?
            .ok_or_else(|| RevealItemError::NotFound(id.to_owned()))?;
        let canonical_id = match &item {
            crate::work::WorkItem::Task(t) | crate::work::WorkItem::Chore(t) => {
                if t.deleted_at.is_some() {
                    return Err(RevealItemError::Deleted(id.to_owned()));
                }
                t.id.clone()
            }
            crate::work::WorkItem::Project(p) => p.id.clone(),
            crate::work::WorkItem::Product(p) => p.id.clone(),
        };
        let product_id = item.product_id().to_string();
        let request = EngineToAppRequest::RevealWorkItem(RevealWorkItemInput {
            work_item_id: canonical_id.clone(),
            product_id,
        });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::RevealWorkItem { result: Ok(_) }) => Ok(canonical_id),
            Ok(EngineToAppResponse::RevealWorkItem { result: Err(err) }) => Err(RevealItemError::App(err)),
            Ok(other) => Err(RevealItemError::ResponseKindMismatch(format!("{other:?}"))),
            Err(err) => Err(RevealItemError::Send(err)),
        }
    }

    /// Validate `path` (must exist, be a regular readable file, and
    /// have a `.md`/`.markdown` extension) and ask the app to open it
    /// in the design-renderer window — the same in-app markdown
    /// surface File ▸ Open uses. Validation lives here, not in the
    /// app, so the SwiftUI layer stays a thin reader per the design
    /// note in [`crate::protocol::FrontendRequest::OpenDocument`].
    /// Powers `bossctl open`.
    pub async fn open_document(&self, path: &str) -> Result<(), OpenDocumentError> {
        let metadata = std::fs::metadata(path).map_err(|_| OpenDocumentError::NotFound(path.to_owned()))?;
        if !metadata.is_file() {
            return Err(OpenDocumentError::NotAFile(path.to_owned()));
        }
        if std::fs::File::open(path).is_err() {
            return Err(OpenDocumentError::NotReadable(path.to_owned()));
        }
        let is_markdown = Path::new(path)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"));
        if !is_markdown {
            return Err(OpenDocumentError::NotMarkdown(path.to_owned()));
        }
        let request = EngineToAppRequest::OpenDocument(OpenDocumentInput { path: path.to_owned() });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::OpenDocument { result: Ok(_) }) => Ok(()),
            Ok(EngineToAppResponse::OpenDocument { result: Err(err) }) => Err(OpenDocumentError::App(err)),
            Ok(other) => Err(OpenDocumentError::ResponseKindMismatch(format!("{other:?}"))),
            Err(SendToAppError::NotRegistered) => Err(OpenDocumentError::NoAppSession),
            Err(err) => Err(OpenDocumentError::Send(err)),
        }
    }

    /// Break-glass release of a worker slot the engine has NO live run
    /// tracked for — a "husk" pane: the app still hosts a session in
    /// `slot_id`, but the engine has already terminal-failed or
    /// forgotten the run that used to occupy it (crash, terminal-fail
    /// path bug, spawn-ack timeout). `bossctl agents stop` / `agents
    /// reap` cannot reach this case: both key off a run id, and the
    /// engine's `WorkerRegistry` no longer has one for a husk.
    ///
    /// Refuses with [`RetirePaneError::LiveRunTracked`] when
    /// `LiveWorkerStateRegistry` still shows a live (non-terminal) run
    /// in `slot_id` — that pane is not a husk, and tearing it down
    /// would kill a pane the engine still considers active; the caller
    /// must use `agents stop` instead.
    ///
    /// Sends the same slot-keyed `ReleaseWorkerPane` request
    /// [`Self::release_worker_pane`] uses — the app's teardown is
    /// already keyed purely by `slot_id` with zero dependency on
    /// engine run-tracking state, so no app-side change is needed to
    /// honor this for a husk. Then defensively clears whatever
    /// engine-side bookkeeping might still reference the slot; for a
    /// genuine husk this is a no-op (the engine already dropped it),
    /// but it fully reconciles a slot that straddled both states (a
    /// stale `LiveWorkerState` entry a buggy terminal-fail path left
    /// behind).
    pub async fn retire_pane(&self, slot_id: u8) -> Result<(), RetirePaneError> {
        if let Some(state) = self.live_worker_states.get(slot_id)
            && !state.activity.is_terminal()
        {
            return Err(RetirePaneError::LiveRunTracked {
                slot_id,
                run_id: state.run_id,
            });
        }
        let request = EngineToAppRequest::ReleaseWorkerPane(ReleaseWorkerPaneInput {
            slot_id,
            kill_grace_seconds: 5,
        });
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::ReleaseWorkerPane { result: Ok(_) }) => {
                tracing::info!(slot_id, "retire_pane: released husk pane");
            }
            Ok(EngineToAppResponse::ReleaseWorkerPane {
                result: Err(EngineToAppError::UnknownSlot),
            }) => {
                tracing::debug!(slot_id, "retire_pane: app reports unknown slot — nothing hosted there");
            }
            Ok(EngineToAppResponse::ReleaseWorkerPane { result: Err(err) }) => {
                return Err(RetirePaneError::App(err));
            }
            Ok(other) => {
                return Err(RetirePaneError::ResponseKindMismatch(format!("{other:?}")));
            }
            Err(SendToAppError::NotRegistered) => {
                tracing::debug!(
                    slot_id,
                    "retire_pane: no app session registered; skipping app round-trip"
                );
            }
            Err(err) => return Err(RetirePaneError::Send(err)),
        }
        let worker_id = crate::coordinator::worker_id_for_slot(slot_id);
        self.execution_coordinator
            .release_worker_and_kick(&worker_id, None)
            .await;
        self.live_worker_states.release_slot(slot_id);
        self.live_status_manager.stop_slot(slot_id);
        self.broadcast_live_worker_states().await;
        Ok(())
    }

    /// Ask the app which slots it currently hosts a session in, then
    /// diff against [`Self::live_worker_states_snapshot`] to return the
    /// slots the app reports that the engine has no live-tracked run
    /// for — "husk" panes. Powers `bossctl agents list --all`.
    ///
    /// Returns an empty list (not an error) when no app session is
    /// registered — there is nothing to diff, and an operator running
    /// `agents list --all` against a headless/test engine shouldn't see
    /// a hard failure for a query that is inherently best-effort.
    pub async fn list_husk_panes(&self) -> Result<Vec<HostedPaneEntry>, RetirePaneError> {
        let request = EngineToAppRequest::ListHostedPanes(ListHostedPanesInput {});
        let hosted = match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::ListHostedPanes { result: Ok(result) }) => result.panes,
            Ok(EngineToAppResponse::ListHostedPanes { result: Err(err) }) => {
                return Err(RetirePaneError::App(err));
            }
            Ok(other) => return Err(RetirePaneError::ResponseKindMismatch(format!("{other:?}"))),
            Err(SendToAppError::NotRegistered) => return Ok(Vec::new()),
            Err(err) => return Err(RetirePaneError::Send(err)),
        };
        let live_slots: std::collections::HashSet<u8> = self
            .live_worker_states_snapshot()
            .into_iter()
            .filter(|state| !state.activity.is_terminal())
            .map(|state| state.slot_id)
            .collect();
        Ok(hosted
            .into_iter()
            .filter(|pane| !live_slots.contains(&pane.slot_id))
            .collect())
    }
}
