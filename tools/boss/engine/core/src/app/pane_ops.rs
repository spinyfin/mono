//! `ServerState` methods for the small, uniformly-shaped engine→app
//! pane RPCs: focus / send-input / interrupt / reveal-work-item /
//! retire-pane / list-hosted-pane-statuses / open-document. Split out of
//! `app.rs` for file-size hygiene; behavior is unchanged from when these
//! lived inline. See
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
    /// Live worker activity does not accept typed input (or is
    /// unknown). No bytes were written to the pane — see
    /// [`boss_protocol::WorkerActivity::accepts_typed_input`].
    #[error(
        "worker is not accepting typed input (activity={})",
        activity.map(boss_protocol::WorkerActivity::as_str).unwrap_or("unknown")
    )]
    NotAcceptingInput {
        activity: Option<boss_protocol::WorkerActivity>,
    },
    #[error("app reported error: {0:?}")]
    App(EngineToAppError),
    #[error(transparent)]
    Send(#[from] SendToAppError),
    #[error("tmux pane delivery failed: {0:#}")]
    Tmux(#[source] anyhow::Error),
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
    #[error("tmux pane delivery failed: {0:#}")]
    Tmux(#[source] anyhow::Error),
    #[error("app returned unexpected response: {0}")]
    ResponseKindMismatch(String),
}

/// Outcome of [`ServerState::interrupt_plan_lookup`].
///
/// Splits what [`ServerState::interrupt_plan_for_run`]'s bare `Option`
/// collapses: a driver that was resolved and declares no interrupt mechanism
/// is a capability gap, but no driver resolving at all is a lookup failure —
/// distinct causes an operator would take different action on.
#[derive(Debug)]
pub(crate) enum InterruptPlanLookup {
    /// The driver was resolved and declares this plan.
    Plan(crate::driver::InterruptPlan),
    /// The driver was resolved and explicitly declares no interrupt
    /// mechanism (`AgentDriver::interrupt_plan() == None`).
    NotInterruptible,
    /// No driver could be resolved for this run at all — missing execution
    /// row, unregistered slug, or a DB error. Not a declared property of any
    /// driver; a lookup failure the operator can potentially fix.
    DriverUnresolved,
}

/// Surfaced by [`ServerState::reveal_work_item`]. Separates
/// id-resolution failures from app-side / transport failures so
/// `bossctl reveal` can produce a precise error.
#[derive(Debug, thiserror::Error)]
pub enum RevealItemError {
    #[error("no work item found for id: {0}")]
    NotFound(String),
    #[error("{0}")]
    Resolution(String),
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
    /// Distinguished from the generic [`SendToAppError::NotRegistered`] /
    /// [`SendToAppError::AppDisconnected`] / [`SendToAppError::SessionWedged`]
    /// Display text so `bossctl open` fails with an actionable remedy;
    /// the bare "no app session is registered" text doesn't say what to
    /// do about it.
    #[error("no Boss app session is registered — launch (or relaunch) the Boss app and try again")]
    NoAppSession,
    #[error("app reported error: {0:?}")]
    App(EngineToAppError),
    #[error(transparent)]
    Send(#[from] SendToAppError),
    #[error("app returned unexpected response: {0}")]
    ResponseKindMismatch(String),
}

/// Surfaced by [`ServerState::retire_pane`] / [`ServerState::list_hosted_pane_statuses`].
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
    /// The engine's bookkeeping says this slot is finished, but the OS and
    /// the worker's own hook stream say otherwise: the shell process is
    /// alive AND the worker either has a tool in flight or emitted a hook
    /// recently. Retiring would kill a working worker.
    ///
    /// Distinct from [`Self::LiveRunTracked`], which is the *bookkeeping*
    /// check. This one exists precisely because the bookkeeping can be
    /// wrong — see the 2026-07-26 `SessionEnd` burst documented on
    /// [`crate::husk_pane_sweep::live_process_evidence`].
    #[error(
        "slot {slot_id} holds a terminal entry for run {run_id}, but its worker process is still alive \
         ({evidence}); refusing to retire — use `bossctl agents stop {run_id}` to tear it down deliberately"
    )]
    LiveProcessCorroborated {
        slot_id: u8,
        run_id: String,
        evidence: String,
    },
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

    /// Resolve `run_id → slot_id` and write `text` into that worker pane:
    /// `tmux send-keys` for a tmux-backed session or the app's `SendToPane`
    /// RPC for a legacy app-owned pane. Returns the
    /// resolved slot on success so `bossctl agents send` can echo back
    /// which pane was targeted (useful when the agent reference was a
    /// crew name). Mirrors [`focus_worker_pane`] in shape, but refuses
    /// when the run's `(activity, driver)` pair yields no injectable
    /// posture (see [`SendInputError::NotAcceptingInput`] /
    /// `pane_input_posture_for_run`). A mid-turn worker on a driver whose
    /// foreground process buffers stdin *is* injectable — the write lands in
    /// the agent's composer, exactly as a human's keystrokes would. When the
    /// guard passes it also verifies the write actually became a queued
    /// prompt. This is the chore-update auto-notice path implicated
    /// in the probe-6 incident.
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
        let posture = self.pane_input_posture_for_run(run_id, slot_id);
        match self
            .inject_pane_text_verified(
                PaneInjectRequest::builder()
                    .run_id(run_id)
                    .slot_id(slot_id)
                    .text(text.clone())
                    .maybe_transcript_path(transcript_path.as_deref())
                    .offset_bytes(offset_bytes)
                    .verify_timeout(Duration::from_secs(6))
                    .posture(posture)
                    .build(),
            )
            .await
        {
            PaneInjectOutcome::Confirmed => Ok(slot_id),
            PaneInjectOutcome::Buffered => {
                tracing::info!(
                    run_id,
                    slot_id,
                    "send_input_to_worker: text buffered by a mid-turn agent; it surfaces as a prompt \
                     when the agent's composer next drains — inside the running turn on a driver that \
                     folds it there, at the next turn otherwise. Nothing here waits for either.",
                );
                Ok(slot_id)
            }
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
            PaneInjectOutcome::NotAcceptingInput { activity } => Err(SendInputError::NotAcceptingInput { activity }),
            PaneInjectOutcome::SendFailed(PaneSendFailure::App(err)) => Err(SendInputError::App(err)),
            PaneInjectOutcome::SendFailed(PaneSendFailure::Send(err)) => Err(SendInputError::Send(err)),
            PaneInjectOutcome::SendFailed(PaneSendFailure::Tmux(err)) => Err(SendInputError::Tmux(err)),
            PaneInjectOutcome::SendFailed(PaneSendFailure::ResponseKindMismatch(msg)) => {
                Err(SendInputError::ResponseKindMismatch(msg))
            }
        }
    }

    /// Resolve `run_id → slot_id` and deliver an Esc keystroke through tmux
    /// for a tmux-backed session or the app RPC for a legacy app-owned pane
    /// — equivalent to the human
    /// pressing Esc with the pane focused. The worker run stays
    /// alive; only the in-flight turn is cancelled. Returns the
    /// resolved slot on success so callers (`bossctl agents
    /// interrupt`) can confirm in JSON output which slot received
    /// the interrupt.
    ///
    /// For a driver whose interrupt path skips its normal turn-boundary
    /// channel (Grok's Esc-cancelled turn skips the `Stop` hook entirely —
    /// design T-12), this also snapshots and spawns the bounded
    /// interrupt-recovery observer (`crate::interrupt_recovery`) so the run's
    /// slot does not pin at `Working` forever. The snapshot is taken
    /// *before* the Esc is sent (see [`crate::driver::InterruptRecoverySnapshot`]
    /// for why), and the observer itself runs detached in the background —
    /// it does not delay this call's response. Claude and Codex are
    /// unaffected: their driver's `prepare_interrupt_recovery` default
    /// returns `None`, so nothing is spawned for them.
    pub async fn interrupt_worker_pane(&self, run_id: &str) -> Result<u8, InterruptPaneError> {
        let plan = self.interrupt_plan_for_run(run_id);
        self.deliver_interrupt_gesture(run_id, plan.as_ref()).await
    }

    /// The driver-declared [`crate::driver::InterruptPlan`] for `run_id`, or
    /// `None` when the run's driver cannot be resolved or declares itself
    /// uninterruptible.
    ///
    /// An unresolvable driver answers `None` rather than a default plan on
    /// purpose: guessing a gesture for an unknown agent is how one driver's
    /// keystroke gets typed into another's TUI, and the caller that needs a
    /// plan (the interrupting probe path) has a well-defined, visible failure
    /// for `None` — it says the driver cannot be interrupted instead of
    /// pretending otherwise.
    ///
    /// Collapses [`InterruptPlanLookup::NotInterruptible`] and
    /// [`InterruptPlanLookup::DriverUnresolved`] onto the same `None` — fine
    /// for [`Self::interrupt_worker_pane`], which falls back to a bare
    /// `Escape` either way, but wrong for a caller that must tell the two
    /// apart. Use [`Self::interrupt_plan_lookup`] there instead.
    pub(crate) fn interrupt_plan_for_run(&self, run_id: &str) -> Option<crate::driver::InterruptPlan> {
        match self.interrupt_plan_lookup(run_id) {
            InterruptPlanLookup::Plan(plan) => Some(plan),
            InterruptPlanLookup::NotInterruptible | InterruptPlanLookup::DriverUnresolved => None,
        }
    }

    /// Like [`Self::interrupt_plan_for_run`], but keeps apart the two reasons
    /// a plan can be missing: a driver that was resolved and explicitly
    /// declares no interrupt mechanism, versus no driver resolving at all
    /// (missing execution row, unregistered slug, DB error). The interrupting
    /// probe path needs this distinction — every registered driver declares a
    /// runnable interrupt plan (`every_registered_driver_declares_a_runnable_interrupt_plan`),
    /// so in production a genuinely uninterruptible driver cannot occur; a
    /// `None` in practice almost always means the lookup itself failed, and
    /// telling the operator "this driver declares no interrupt mechanism" for
    /// what is actually a resolution failure points them at a permanent
    /// capability gap when the real, fixable problem is the lookup.
    pub(crate) fn interrupt_plan_lookup(&self, run_id: &str) -> InterruptPlanLookup {
        let Some(driver) = crate::driver_transcript::driver_for_execution(&self.work_db, run_id) else {
            return InterruptPlanLookup::DriverUnresolved;
        };
        match driver.interrupt_plan() {
            Some(plan) => InterruptPlanLookup::Plan(plan),
            None => InterruptPlanLookup::NotInterruptible,
        }
    }

    /// Deliver one interrupt **attempt** — the driver's key, repeated
    /// `plan.presses` times — into `run_id`'s pane, and arm the bounded
    /// turn-end recovery observer for a driver that needs one.
    ///
    /// This is the primitive both interrupt callers share: `bossctl agents
    /// interrupt` (one attempt, fire and forget) and the interrupting probe
    /// path (one attempt per retry, each followed by its own confirmation
    /// wait). Sharing it is what keeps "what Boss sends to interrupt this
    /// driver" in exactly one place — the driver's plan — rather than a
    /// hard-coded `Escape` at each site.
    ///
    /// `plan` is `None` for a run whose driver could not be resolved or
    /// declares no interrupt mechanism. Rather than refuse, this falls back
    /// to a single `Escape`, preserving the pre-existing behaviour of
    /// `bossctl agents interrupt` for such a run: that verb is an operator
    /// reaching for the same key they would press by hand, and it worked
    /// before any plan existed. Callers for which a missing plan is
    /// *meaningful* — the probe path, which must not claim to have
    /// interrupted a driver that declares it cannot be — check
    /// [`Self::interrupt_plan_for_run`] first and never reach here with
    /// `None`.
    ///
    /// The recovery snapshot is taken **before** the keys go out (see
    /// [`crate::driver::InterruptRecoverySnapshot`] for why), and the
    /// observer runs detached — it does not delay this call. Claude and Codex
    /// arm nothing: their `prepare_interrupt_recovery` returns `None` because
    /// their cancelled turns reach the ordinary turn-boundary channel.
    pub(crate) async fn deliver_interrupt_gesture(
        &self,
        run_id: &str,
        plan: Option<&crate::driver::InterruptPlan>,
    ) -> Result<u8, InterruptPaneError> {
        let Some(slot_id) = self.worker_registry.slot_for_run(run_id) else {
            return Err(InterruptPaneError::UnknownRun);
        };
        let key = plan.map(|p| p.gesture.key).unwrap_or("Escape");
        let presses = plan.map(|p| p.gesture.presses.max(1)).unwrap_or(1);
        let press_interval = plan.map(|p| p.gesture.press_interval).unwrap_or(Duration::ZERO);
        let recovery_prep = crate::driver_transcript::driver_for_execution(&self.work_db, run_id).and_then(|driver| {
            driver
                .prepare_interrupt_recovery(run_id)
                .map(|snapshot| (driver, snapshot))
        });
        let mut result = Ok(slot_id);
        for press in 0..presses {
            if press > 0 && !press_interval.is_zero() {
                tokio::time::sleep(press_interval).await;
            }
            result = self.send_interrupt_key(run_id, slot_id, key).await;
            if result.is_err() {
                // A failed press aborts the rest of the attempt: repeating a
                // gesture the transport just refused cannot succeed, and the
                // caller needs the transport error, not the last press's.
                break;
            }
        }
        if result.is_ok()
            && let Some((driver, snapshot)) = recovery_prep
        {
            match self._self_weak.upgrade() {
                Some(arc_self) => {
                    let run_id = run_id.to_owned();
                    tokio::spawn(async move {
                        crate::interrupt_recovery::run_interrupt_recovery(driver, run_id, snapshot, arc_self).await;
                    });
                }
                None => tracing::debug!(run_id, "interrupt recovery: ServerState already dropped; skipping"),
            }
        }
        result
    }

    /// One keypress into the run's pane, over whichever transport hosts it:
    /// tmux `send-keys <key>` for a tmux-backed session, the app's
    /// `InterruptWorkerPane` RPC for a legacy app-owned pane.
    ///
    /// The app transport sends `kVK_Escape` unconditionally, so a driver plan
    /// naming any other key is only honoured on tmux-hosted panes. That is
    /// recorded here rather than silently ignored: every registered driver's
    /// plan names `Escape` today, and a future plan that does not must extend
    /// the app RPC rather than assume this path carries it.
    async fn send_interrupt_key(&self, run_id: &str, slot_id: u8, key: &str) -> Result<u8, InterruptPaneError> {
        match self.worker_registry.pane_for_run(run_id) {
            Some(pane) if pane.tmux_session_name.is_some() || pane.tmux_hosted => match pane.tmux_session_name {
                Some(session_name) => match self.tmux_for_pane_delivery() {
                    Ok(tmux) => tmux
                        .send_key(&session_name, key)
                        .await
                        .map(|_| slot_id)
                        .map_err(InterruptPaneError::Tmux),
                    Err(err) => Err(InterruptPaneError::Tmux(err)),
                },
                None => Err(InterruptPaneError::Tmux(anyhow::anyhow!(
                    "tmux-hosted pane has no session name"
                ))),
            },
            _ => {
                if key != "Escape" {
                    tracing::warn!(
                        run_id,
                        slot_id,
                        key,
                        "app-hosted pane transport only sends Escape; the driver's interrupt key is \
                         being delivered as Escape",
                    );
                }
                let request = EngineToAppRequest::InterruptWorkerPane(InterruptWorkerPaneInput { slot_id });
                match self.send_to_app(request, Duration::from_secs(5)).await {
                    Ok(EngineToAppResponse::InterruptWorkerPane { result: Ok(_) }) => Ok(slot_id),
                    Ok(EngineToAppResponse::InterruptWorkerPane { result: Err(err) }) => {
                        Err(InterruptPaneError::App(err))
                    }
                    Ok(other) => Err(InterruptPaneError::ResponseKindMismatch(format!("{other:?}"))),
                    Err(err) => Err(InterruptPaneError::Send(err)),
                }
            }
        }
    }

    /// Resolve `id` (short-form `T607` or canonical) to a work item
    /// and ask the app to scroll the kanban to that card and play a
    /// short transient highlight. Returns the canonical id on success
    /// so `bossctl reveal` can confirm what was highlighted.
    pub async fn reveal_work_item(&self, id: &str) -> Result<String, RevealItemError> {
        let canonical_id = self
            .resolve_work_item_id(id)
            .await
            .map_err(|err| RevealItemError::Resolution(err.to_string()))?;
        let item = self
            .work_db
            .get_work_item(&canonical_id)
            .map_err(|_| RevealItemError::NotFound(id.to_owned()))?;
        // `get_work_item` already filters `deleted_at IS NULL` for tasks
        // (and the resolvers above are live-only too), so a tombstoned row
        // never reaches here — it fails resolution/fetch as not-found
        // instead of a deleted-specific message.
        let canonical_id = match &item {
            crate::work::WorkItem::Task(t) | crate::work::WorkItem::Chore(t) => t.id.clone(),
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
        let metadata = std::fs::metadata(path).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => OpenDocumentError::NotFound(path.to_owned()),
            _ => OpenDocumentError::NotReadable(path.to_owned()),
        })?;
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
            Err(SendToAppError::NotRegistered | SendToAppError::AppDisconnected | SendToAppError::SessionWedged) => {
                Err(OpenDocumentError::NoAppSession)
            }
            Err(err) => Err(OpenDocumentError::Send(err)),
        }
    }

    /// Break-glass release of a worker slot the engine has NO live run
    /// tracked for — a "husk" pane: the app still hosts a session in
    /// `slot_id`, but the engine has already terminal-failed or
    /// forgotten the run that used to occupy it (crash, terminal-fail
    /// path bug, spawn-ack timeout). `bossctl agents retire-pane`
    /// resolves a crew name or run id to `slot_id` client-side before
    /// calling this — the wire request stays slot-keyed since that is
    /// what identifies a pane to the app.
    ///
    /// Refuses with [`RetirePaneError::LiveRunTracked`] when
    /// `LiveWorkerStateRegistry` still shows a live (non-terminal) run
    /// in `slot_id` — that pane is not a husk, and tearing it down
    /// would kill a pane the engine still considers active; the caller
    /// must use `agents stop` instead. Also refuses with
    /// [`RetirePaneError::LiveProcessCorroborated`] when the registry
    /// has a *terminal* entry contradicted by the worker's own hook
    /// stream showing real recent activity — same reasoning, weaker
    /// bookkeeping. Neither guard is reachable from durable state alone
    /// (see below), because both require reading a `LiveWorkerState`
    /// that a true husk, by construction, does not have.
    ///
    /// When there is no live-state entry at all but durable state
    /// (`work_runs.shell_pid` plus the execution's own row) corroborates
    /// a still-running process for an execution terminalized by
    /// inference (`orphaned`/`abandoned`) — the shape a worker the
    /// engine lost track of takes, and the one `agents stop` reaches via
    /// its own durable fallback — this does not refuse: it performs that
    /// same durable-state teardown ([`Self::release_worker_pane`]) and
    /// completes the retirement, so the verb the operator reached for
    /// handles the case instead of redirecting to a second one.
    ///
    /// Otherwise (no live-state entry and no durable evidence at all —
    /// a genuine husk) sends the same slot-keyed `ReleaseWorkerPane`
    /// request [`Self::release_worker_pane`] uses — the app's teardown
    /// is already keyed purely by `slot_id` with zero dependency on
    /// engine run-tracking state, so no app-side change is needed to
    /// honor this for a husk. Then defensively clears whatever
    /// engine-side bookkeeping might still reference the slot; for a
    /// genuine husk this is a no-op (the engine already dropped it),
    /// but it fully reconciles a slot that straddled both states (a
    /// stale `LiveWorkerState` entry a buggy terminal-fail path left
    /// behind).
    pub async fn retire_pane(&self, slot_id: u8) -> Result<(), RetirePaneError> {
        if let Some(state) = self.live_worker_states.get(slot_id) {
            // Guard 1 (bookkeeping): the engine still considers this run live.
            if !state.activity.is_terminal() {
                return Err(RetirePaneError::LiveRunTracked {
                    slot_id,
                    run_id: state.run_id,
                });
            }
            // Guard 2 (reality): the engine considers the run finished, but
            // the OS and the worker's hook stream disagree. This is the last
            // check before an irreversible SIGTERM of the worker's process
            // group, and it is deliberately independent of the bookkeeping
            // guard above — the 2026-07-26 incident was precisely a case of
            // the bookkeeping being uniformly wrong.
            let now = boss_engine_utils::epoch_time::now_epoch_secs();
            if let Some(evidence) = crate::husk_pane_sweep::live_process_evidence(&state, now) {
                tracing::warn!(
                    slot_id,
                    run_id = %state.run_id,
                    activity = state.activity.as_str(),
                    %evidence,
                    "retire_pane: refusing to retire — the live-state entry is terminal but the worker \
                     process is demonstrably alive; killing it would destroy in-flight work",
                );
                return Err(RetirePaneError::LiveProcessCorroborated {
                    slot_id,
                    run_id: state.run_id,
                    evidence,
                });
            }
        } else if let Some(run_id) = self.hosted_pane_run_for_slot(slot_id).await
            && let Some(evidence) = self.durable_live_process_evidence(&run_id)
        {
            // Guard 3 (reality, with NO bookkeeping at all): the engine has no
            // live-state entry for this slot, so guards 1 and 2 both had
            // nothing to read — which is the state a wrongly-terminalized
            // worker is always in, since the terminal path clears the entry.
            //
            // Reconciled with `agents stop` (2026-08-01): this exact shape —
            // no live registry entry, but durable state corroborating a
            // still-alive process for an execution terminalized by
            // inference (`orphaned`/`abandoned`) — is precisely what
            // `release_worker_pane`'s durable fallback
            // (`reap_untracked_worker_process`) already exists to reap. It
            // used to dead-end here in a refusal that pointed the operator
            // at a second command (`agents stop <run_id>`); now it performs
            // that identical durable-state teardown directly and completes
            // the retirement, so the verb the operator reached for handles
            // this case instead of a two-verb dance. Guards 1 and 2 above are
            // unrelated and still refuse outright: guard 1 is a run the
            // engine actively considers live, and guard 2 is a bookkeeping-
            // terminal entry contradicted by the worker's own hook stream
            // showing real recent activity — both are cases where the
            // evidence is ambiguous or points at genuine in-flight work, and
            // only a human explicitly invoking `agents stop` should decide
            // to kill that.
            tracing::warn!(
                slot_id,
                run_id = %run_id,
                %evidence,
                "retire_pane: no live registry entry for this slot, but durable state shows an \
                 inferred-terminal execution with a still-alive worker process — reaping it via the \
                 same durable-state teardown `agents stop` uses, then completing the retirement",
            );
            let outcome = self.release_worker_pane(&run_id).await;
            tracing::info!(
                slot_id,
                run_id = %run_id,
                ?outcome,
                "retire_pane: durable-state teardown completed for a terminal-entry-with-live-process pane",
            );
            return Ok(());
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
    /// classify each against [`Self::live_worker_states_snapshot`] and
    /// durable state: live, engine-lost-track-of-it-but-durably-alive
    /// (`LiveProcessNoRegistry`), or a true husk. Powers `bossctl agents
    /// list --all` and worker-reference resolution (crew name / slot id
    /// / run id) for every `agents` verb — both need to see a pane the
    /// live registry has dropped, including `LiveProcessNoRegistry` panes
    /// a husk-only view would hide.
    ///
    /// Returns an empty list (not an error) when no app session is
    /// registered — there is nothing to diff, and an operator running
    /// `agents list --all` against a headless/test engine shouldn't see
    /// a hard failure for a query that is inherently best-effort.
    pub async fn list_hosted_pane_statuses(&self) -> Result<Vec<HostedPaneStatus>, RetirePaneError> {
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
        let live_by_slot: std::collections::HashMap<u8, boss_protocol::LiveWorkerState> = self
            .live_worker_states_snapshot()
            .into_iter()
            .map(|state| (state.slot_id, state))
            .collect();
        let now = boss_engine_utils::epoch_time::now_epoch_secs();

        let mut statuses = Vec::with_capacity(hosted.len());
        for pane in hosted {
            let state = match live_by_slot.get(&pane.slot_id) {
                // The engine tracks a live run here.
                Some(state) if !state.activity.is_terminal() => HostedPaneState::Live,
                // A terminal entry for THIS run. The engine believes the run
                // ended; before that belief is allowed to justify killing the
                // pane's process, take a second opinion from the OS and the
                // worker's own hook stream.
                //
                // The `run_id` match matters: if the entry names a different
                // run, the slot was recycled and the app is hosting a pane
                // for a run that really is gone — a genuine husk, and its
                // liveness signals belong to the newer run, not this pane.
                Some(state) if state.run_id == pane.run_id => {
                    match crate::husk_pane_sweep::live_process_evidence(state, now) {
                        Some(evidence) => {
                            tracing::warn!(
                                slot_id = pane.slot_id,
                                run_id = %pane.run_id,
                                activity = state.activity.as_str(),
                                %evidence,
                                "pane classification: slot has a TERMINAL live-state entry but the worker \
                                 process is demonstrably alive; NOT a husk. The engine's own bookkeeping is \
                                 wrong for this slot — something terminalized a run whose process kept \
                                 working.",
                            );
                            HostedPaneState::LiveProcessNoRegistry { evidence }
                        }
                        None => HostedPaneState::Husk,
                    }
                }
                // No entry at all, or an entry for a different run: the
                // classic husk shape this sweep exists for.
                //
                // But "the engine has no live-state entry" is the WEAKEST
                // possible evidence of death, because that entry is dropped
                // unconditionally by `release_worker_pane` on every terminal
                // path — including the ones that fire on a wrong inference.
                // The corroboration above cannot help here: it reads a
                // `LiveWorkerState` that by definition does not exist in this
                // branch. So take the second opinion from durable state
                // instead, which survives exactly the teardown that emptied
                // the registry.
                //
                // Without this, the two halves of convergence fight: a worker
                // that is alive but quiet (parked in a long build, emitting no
                // hook to converge on) can be confirmed a husk across two
                // passes and SIGTERMed before the re-adoption path — running
                // on the same 60 s cadence — gets to it. Re-adoption and
                // retirement must not race for the same pane.
                _ => match self.durable_live_process_evidence(&pane.run_id) {
                    Some(evidence) => {
                        tracing::warn!(
                            slot_id = pane.slot_id,
                            run_id = %pane.run_id,
                            %evidence,
                            "pane classification: the engine has no live-state entry for this slot, but the \
                             run's durably-recorded worker process is still alive; NOT a husk. This is a \
                             re-adoption candidate for the sweep, and a `LiveProcessNoRegistry` pane for \
                             `agents list --all` / worker-reference resolution — see \
                             `boss_engine::worker_readoption`.",
                        );
                        HostedPaneState::LiveProcessNoRegistry { evidence }
                    }
                    None => HostedPaneState::Husk,
                },
            };
            statuses.push(HostedPaneStatus {
                slot_id: pane.slot_id,
                run_id: pane.run_id,
                crew_name: boss_protocol::name_for_slot(pane.slot_id),
                summary: pane.summary,
                task_title: pane.task_title,
                state,
            });
        }
        Ok(statuses)
    }

    /// The run id the app hosts a pane for in `slot_id`, or `None` when it
    /// hosts none (or cannot be asked). The slot-keyed inverse of
    /// [`ServerState::hosted_pane_slot_for_run`], needed by `retire_pane`,
    /// whose input is a slot rather than a run.
    ///
    /// Best-effort: a `None` here means the durable-liveness guard simply does
    /// not fire, leaving the pre-existing behaviour intact.
    async fn hosted_pane_run_for_slot(&self, slot_id: u8) -> Option<String> {
        let request = EngineToAppRequest::ListHostedPanes(ListHostedPanesInput {});
        match self.send_to_app(request, Duration::from_secs(5)).await {
            Ok(EngineToAppResponse::ListHostedPanes { result: Ok(result) }) => result
                .panes
                .into_iter()
                .find(|pane| pane.slot_id == slot_id)
                .map(|pane| pane.run_id),
            other => {
                tracing::debug!(
                    slot_id,
                    ?other,
                    "retire_pane: app could not be asked what it hosts in this slot",
                );
                None
            }
        }
    }

    /// Restart-robust counterpart to
    /// [`crate::husk_pane_sweep::live_process_evidence`] for a run the engine
    /// has no `LiveWorkerState` for at all.
    ///
    /// `live_process_evidence` corroborates a pid against the worker's hook
    /// stream, and deliberately requires both halves — for a slot whose
    /// live-state entry still exists, "pid alive" alone would match a genuine
    /// husk (the pane's shell lingers after `claude` exits) and disable the
    /// sweep. Here there is no entry to read a hook stream from, so the test is
    /// different and narrower: the pid is alive AND the execution row is
    /// terminal *by inference* (`orphaned` / `abandoned`).
    ///
    /// That pairing is what distinguishes the two cases. A genuine husk's
    /// execution went terminal for a real reason — it completed, it was
    /// cancelled — and those statuses are excluded, so a lingering shell under
    /// a finished run is still retired exactly as before. An `orphaned` row
    /// with a live process is the engine's own guess contradicted by the OS,
    /// and killing it is how in-flight work is destroyed.
    ///
    /// `Some(evidence)` means "do not retire", with `evidence` naming the
    /// contradicting signal for the log.
    fn durable_live_process_evidence(&self, run_id: &str) -> Option<String> {
        let process = crate::durable_liveness::probe_execution_worker(&self.work_db, run_id);
        let shell_pid = process.alive_pid()?;
        let execution = self.work_db.get_execution(run_id).ok()?;
        if !matches!(
            execution.status,
            boss_protocol::ExecutionStatus::Orphaned | boss_protocol::ExecutionStatus::Abandoned
        ) {
            return None;
        }
        Some(format!(
            "durably-recorded shell pid {shell_pid} is alive and execution status `{}` was \
             inferred, not decided",
            execution.status,
        ))
    }
}
