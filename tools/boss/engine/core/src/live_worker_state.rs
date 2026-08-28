//! In-memory store of per-slot [`LiveWorkerState`] values.
//!
//! The events socket consumer feeds this; bossctl reads from it via
//! the frontend RPC; the topic broker re-publishes the full snapshot
//! whenever any slot changes so UI subscribers can push to the kanban
//! Doing icon and the pane titlebar pill in near-real-time.
//!
//! Keyed by slot id (the interactive pool spans 1..=16 across two pages,
//! with automation/review above it), not run id — run records finalise
//! quickly after spawn (they model the spawn act, not the worker's
//! life). Two consecutive runs in the same slot reuse the slot key.

use std::collections::HashMap;
use std::sync::Mutex;

use boss_protocol::{ExecutionKind, LiveWorkerState, SessionStartSource, WorkItemBinding, WorkerActivity, WorkerEvent};

use crate::driver::ProgressFidelity;

/// Attributed worker-pool label for a live run (`"main"`, `"automation"`,
/// or `"review"`). Matches
/// [`crate::coordinator::ExecutionCoordinator::attributed_pool_label`]:
/// review work always reports `"review"`, automation triage and any
/// automation-sourced work report `"automation"`, everything else
/// reports `"main"`. Independent of which physical slot the run
/// occupies (automation can spill into a main-pool Lower Decks slot).
///
/// Used at spawn registration to stamp [`LiveWorkerState::pool`] so
/// `bossctl agents list` can render pool without joining the execution
/// table or re-deriving attribution.
pub fn attributed_pool_label(kind: ExecutionKind, has_source_automation: bool) -> &'static str {
    match kind {
        ExecutionKind::PrReview => "review",
        ExecutionKind::AutomationTriage => "automation",
        _ if has_source_automation => "automation",
        _ => "main",
    }
}

/// Pool + execution-kind stamps carried into
/// [`LiveWorkerStateRegistry::register_spawn_with_capabilities`] so production
/// dispatch can populate [`LiveWorkerState::pool`] / [`LiveWorkerState::kind`]
/// without growing the register-spawn arity further. Both fields are `None`
/// for tests and any spawn path that does not know them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveSpawnRouting {
    /// Attributed pool (`"main"` / `"automation"` / `"review"`).
    pub pool: Option<String>,
    /// Execution kind snake_case string (see [`ExecutionKind::as_str`]).
    pub kind: Option<String>,
}

impl LiveSpawnRouting {
    /// Both fields unset — the historical test/default shape.
    pub fn none() -> Self {
        Self::default()
    }

    /// Stamp both fields for a production dispatch.
    pub fn new(pool: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            pool: Some(pool.into()),
            kind: Some(kind.into()),
        }
    }
}

/// The model identifier the engine uses when no `SessionStart` hook
/// has yet reported one — this is the model the launcher *asked* for,
/// surfaced so the UI can render the real model name immediately
/// instead of "Claude Unknown".
pub const DEFAULT_LAUNCH_MODEL: &str = "opus";

/// How long a slot must be stuck in `Spawning` with no hook events before
/// [`LiveWorkerStateRegistry::mark_stalled_spawns`] transitions it to
/// `WaitingForInput`. 30 seconds matches the dead-PID grace period and
/// gives a fresh-but-slow worker enough runway while being well below the
/// typical interactive-wait tolerance.
///
/// This threshold only ever promotes slots that already have a reported
/// `shell_pid` (see the guard in `mark_stalled_spawns`) — a slot with no
/// pid at all is a different failure class entirely, handled by
/// `crate::spawn_ack_sweep` instead. See that module's doc comment for
/// the 2026-07-03/04 false-live incident this split addresses.
pub const STALLED_SPAWN_THRESHOLD_SECS: i64 = 30;

/// How long a spawn may go without a **driver-originated** signal
/// before [`LiveWorkerStateRegistry::unverified_driver_starts`] reports
/// it as a never-started driver for
/// [`crate::spawn_ack_sweep`] to reap.
///
/// ## Why this is a separate, longer window than the two above
///
/// [`STALLED_SPAWN_THRESHOLD_SECS`] and
/// [`crate::spawn_ack_sweep::SPAWN_ACK_GRACE_SECS`] both answer "did the
/// *pane* come up?". This one answers the strictly stronger question
/// "did the *driver binary* come up?" — the question no check in Boss
/// asked before, and the one the 2026-07-30 incident turned on: a pane
/// hosting nothing but an idle login shell reported `shell_pid=92697`
/// and satisfied every pane-level check forever.
///
/// 300s is deliberately far above any real driver startup. A healthy
/// driver's first hook (`SessionStart`) fires within seconds of exec.
/// The one historically legitimate multi-minute pre-hook wait — claude's
/// first-run folder-trust dialog, which is the entire reason
/// [`LiveWorkerStateRegistry::mark_stalled_spawns`] exists — is
/// *pre-suppressed* at provision time by
/// `boss_engine_driver::claude`'s `hasTrustDialogAccepted` seeding, so
/// no driver should ever legitimately sit pre-hook for minutes. Five
/// minutes leaves an order of magnitude of headroom over that reality
/// while still bounding the hold: before this, the hold was unbounded.
pub const DRIVER_START_GRACE_SECS: i64 = 300;

/// How long after the most recent hook a slot may keep advertising
/// `Spawning` before [`LiveWorkerStateRegistry::downgrade_stale_activity`]
/// moves it to `Idle`.
///
/// Covers the shape `mark_stalled_spawns` deliberately ignores: a slot
/// that *did* receive at least one hook (so `last_event_at` is set —
/// typically `SessionStart(Resume)` on reattach, which stamps the
/// timestamp without leaving `Spawning`) and then went quiet because
/// `events.sock` degraded. Without this timer the slot sits at
/// `activity=spawning` forever, which is a lie once an event has been
/// observed. Same 30s window as the stalled-spawn threshold so the two
/// honesty timers move in lockstep.
pub const STALE_ACTIVITY_DOWNGRADE_SECS: i64 = 30;

/// Thread-safe registry of LiveWorkerState entries, keyed by slot id.
#[derive(Default)]
pub struct LiveWorkerStateRegistry {
    /// Every live slot's complete record. One map, not several parallel
    /// ones keyed by the same `u8`: a slot's whole footprint is
    /// established by a single `insert` and torn down by a single
    /// `remove`, so no registration or release site has to remember a
    /// per-field lifecycle, and there is no failure mode where one table
    /// keeps a stale entry a sibling table already dropped.
    inner: Mutex<HashMap<u8, SlotEntry>>,
}

/// One slot's full record: the wire-format state the app and `bossctl`
/// render, plus the engine-side bookkeeping only the sweeps read.
struct SlotEntry {
    state: LiveWorkerState,
    meta: SlotMeta,
}

/// Per-slot engine-side bookkeeping.
///
/// Deliberately kept out of [`LiveWorkerState`]: that struct is the wire
/// format the app/bossctl consume, and none of these values has a UI
/// consumer — only the sweeps'.
///
/// Builder-constructed per the repo's convention for structs past five
/// fields. Only `spawned_at` and `awaiting_input_capable` are decided by
/// the caller; the rest start at the one value a fresh entry can honestly
/// hold (`bon` defaults the two `Option` fields to `None` on its own), so
/// the construction site states exactly what it knows and nothing else.
#[derive(bon::Builder)]
struct SlotMeta {
    /// Set when a `Notification` hook arrives, cleared on the next `Stop`.
    /// Lets us turn a `Stop` into `WaitingForInput` rather than `Idle`
    /// when claude is paused on a permission prompt.
    #[builder(default = false)]
    notification_pending: bool,
    /// Epoch-seconds timestamp recorded when `register_spawn` creates the
    /// entry. Used by `mark_stalled_spawns` to detect workers that have
    /// been stuck in `Spawning` without any hook event (the initial
    /// directory-trust prompt fires before `SessionStart`, so the normal
    /// `Notification`→`WaitingForInput` path is never triggered for it),
    /// and by `unverified_driver_starts` to age a spawn against
    /// [`DRIVER_START_GRACE_SECS`].
    spawned_at: i64,
    /// [`ProgressFidelity`] tier declared by the driver running this slot,
    /// set by [`LiveWorkerStateRegistry::set_progress_fidelity`] after
    /// spawn. Consulted by `crate::stale_worker_sweep` to decide whether —
    /// and at what threshold — cadence-based staleness applies to this
    /// slot. `None` (never declared) reads as [`ProgressFidelity::Rich`]
    /// — today's only driver (Claude) and every existing call site that
    /// never sets this explicitly, so the default preserves current
    /// behaviour unchanged.
    ///
    /// In-memory only, and not persisted or rehydrated anywhere: if the
    /// engine restarts while a worker is alive, the registry starts empty
    /// and the slot re-defaults to `Rich` until the driver re-declares
    /// (which today only happens at spawn, not on rehydrate). For a
    /// `Coarse`- or `Minimal`-tier driver this silently re-enables
    /// cadence-based staleness judgement for a slot the exemption was
    /// meant to protect — a live worker mid-turn with no per-tool event
    /// can then be swept as stale. No-op today (Claude is `Rich`), but a
    /// real gap for the first non-`Rich` driver.
    progress_fidelity: Option<ProgressFidelity>,
    /// Does this run's driver declare `Capability::AwaitingInputSignal`?
    /// Gates whether `apply_event` trusts a `WorkerEvent::Notification` as
    /// a genuine "worker is blocked on human input" signal.
    ///
    /// Seeded `true` by `register_spawn` (Claude is the only driver in
    /// production today, and it provides the capability), so the ~30
    /// existing test call sites keep working unchanged. Production spawn
    /// sites that resolve a real driver call
    /// `register_spawn_with_capabilities` instead, passing the resolved
    /// value directly so it can never be left at the default by a
    /// forgotten follow-up call — see that method's doc for why the honest
    /// default on absence is "never fake it", not a lower-fidelity guess.
    awaiting_input_capable: bool,
    /// Epoch-seconds timestamp of the first **driver-originated** signal
    /// observed for the slot's current run — the moment Boss gained
    /// positive evidence that the driver binary itself is running.
    ///
    /// ## Why this is not `LiveWorkerState::last_event_at`
    ///
    /// `last_event_at` is a *display* timestamp and is written by paths
    /// that are not the driver: [`LiveWorkerStateRegistry::mark_stalled_spawns`]
    /// synthesizes one when it promotes a slot to `WaitingForInput`, and
    /// [`LiveWorkerStateRegistry::mark_errored`] stamps one on an
    /// engine-side verdict. Treating it as proof of driver start would
    /// let the engine's own guesses vouch for a driver that never ran.
    /// This field is written by exactly one method
    /// ([`LiveWorkerStateRegistry::record_driver_signal`]) from exactly
    /// two call sites in the hook ingress — a real worker hook, and
    /// receipt of a `transcript_path` — so "has this driver started?"
    /// has a single, unforgeable answer.
    ///
    /// Note what is deliberately absent: `shell_pid`. A reported
    /// foreground pid is the *shell hosting the pane*, not the driver
    /// (`GhosttyTerminalView.swift`'s `onSurfaceAttached` reads
    /// `ghostty_surface_foreground_pid`, which is the login shell when
    /// the driver was never exec'd). Every check that treated a positive
    /// pid as evidence of a working worker is what the 2026-07-30
    /// incident walked through untouched.
    driver_signal_at: Option<i64>,
    /// Whether this registration is one driver-start verification may
    /// judge at all. See [`DriverStartExpectation`].
    #[builder(default = DriverStartExpectation::EngineSpawned)]
    driver_start_expectation: DriverStartExpectation,
}

/// Whether the engine is entitled to expect a driver start for a slot's
/// current registration.
///
/// [`LiveWorkerStateRegistry::unverified_driver_starts`] asks "did the
/// driver binary Boss launched ever come up?". That question presupposes
/// Boss launched one, and one registration path does not launch anything:
/// re-adoption ([`crate::app::ServerState`]'s convergence for a worker
/// that outlived the execution the engine wrongly terminalized) registers
/// a slot for a process that has been running, unobserved, for however
/// long. Its `spawned_at` is the moment the engine noticed, not the moment
/// anything exec'd, so aging that stamp answers a question nobody asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverStartExpectation {
    /// The engine launched a driver for this registration and is owed
    /// proof it came up. The normal spawn path.
    EngineSpawned,
    /// The registration re-adopted an already-running worker. Driver-start
    /// verification does not apply; the convergence rules that already own
    /// a running worker — `crate::dead_pid_sweep`, `crate::husk_pane_sweep`,
    /// `crate::stale_worker_sweep` and `crate::orphan_sweep`'s redispatch
    /// guard — judge it on evidence about the process that actually exists.
    Readopted,
}

/// What Boss knows about a re-adopted worker at the moment it re-registers
/// the slot. Passed to [`LiveWorkerStateRegistry::register_readoption`].
///
/// The two triggers differ in kind, and collapsing them would either
/// discard real proof or manufacture it:
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadoptionEvidence {
    /// A worker hook arrived for a run the engine had already
    /// terminalized. That is driver-originated proof of exactly the sort
    /// [`LiveWorkerStateRegistry::record_driver_signal`] exists to record,
    /// so re-adoption records it rather than throwing it away.
    DriverHook,
    /// Only a recorded shell pid was observed alive (`crate::durable_liveness`
    /// probing the pid the app reported for the pane). That is evidence
    /// about the *shell*, never about the driver — the exact conflation
    /// `driver_signal_at` exists to prevent — so nothing is recorded as
    /// driver proof.
    LiveShellPid,
}

/// Which driver-originated signal proved the driver is running. Recorded
/// for the log line and the reap's dispatch event; both variants are
/// equally authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverSignalKind {
    /// A worker hook event arrived over the events socket.
    HookEvent,
    /// A `transcript_path` was resolved for the run. Proof the driver
    /// created its transcript, even if the hook's slot fan-out was
    /// dropped (a hook can race `register_run_slot`).
    TranscriptPath,
}

impl DriverSignalKind {
    /// Stable, greppable label for logs and dispatch-event details.
    pub fn as_str(self) -> &'static str {
        match self {
            DriverSignalKind::HookEvent => "hook_event",
            DriverSignalKind::TranscriptPath => "transcript_path",
        }
    }
}

/// A slot whose spawn has gone [`DRIVER_START_GRACE_SECS`] without any
/// driver-originated signal — i.e. Boss has no evidence the driver
/// binary ever executed. Returned by
/// [`LiveWorkerStateRegistry::unverified_driver_starts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnverifiedDriverStart {
    pub slot_id: u8,
    pub run_id: String,
    /// The shell pid the app reported, if any. Carried purely so the
    /// reap can name it in the log and the attention item — it is
    /// explicitly NOT part of the decision.
    pub shell_pid: i32,
    /// How long the slot has gone without a driver signal, in seconds.
    pub silent_secs: i64,
    /// Activity the slot is advertising. Recorded for diagnosis: the
    /// 2026-07-30 grok occurrence sat at `Spawning`, while a
    /// capability-declaring driver's identical failure would have been
    /// promoted to `WaitingForInput` by `mark_stalled_spawns` first.
    pub activity: WorkerActivity,
}

impl LiveWorkerStateRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stamp the initial state for a freshly-allocated slot. Activity
    /// is `Spawning` until the first hook arrives. Any prior entry
    /// for this slot is replaced — the previous worker has been
    /// released, so its terminal state isn't useful.
    ///
    /// `binding` is the work-item linkage for the run. Production
    /// dispatch always passes `Some`; in-process tests and any
    /// future direct-launch path that bypasses the work tables may
    /// pass `None`.
    ///
    /// Seeds `awaiting_input_capable` `true` (Claude's historical
    /// behaviour) — the ~30 existing test call sites in this crate rely on
    /// that default. Production spawn paths that resolve an actual driver
    /// must call [`Self::register_spawn_with_capabilities`] instead, so the
    /// capability travels with registration rather than depending on a
    /// second call that a future call site could forget.
    #[track_caller]
    pub fn register_spawn(
        &self,
        slot_id: u8,
        run_id: impl Into<String>,
        model: impl Into<String>,
        shell_pid: i32,
        binding: Option<WorkItemBinding>,
    ) {
        self.register_spawn_with_capabilities(
            slot_id,
            run_id,
            model,
            shell_pid,
            binding,
            true,
            LiveSpawnRouting::none(),
        );
    }

    /// Same as [`Self::register_spawn`], but takes `awaiting_input_capable`
    /// directly instead of seeding `true` and relying on a follow-up
    /// [`Self::set_awaiting_input_capable`] call. Production spawn sites
    /// that resolve a real driver should call this: it closes the
    /// fail-open gap where a spawn site that registers a slot but forgets
    /// the setter would silently default to "trust `Notification`", and it
    /// removes the window between the two calls where a concurrently
    /// delivered hook event would be evaluated against that stale default.
    ///
    /// `routing` carries the attributed worker pool (`"main"` /
    /// `"automation"` / `"review"`) and the execution kind
    /// (`"task_implementation"`, …). Production dispatch always passes
    /// both so `bossctl agents list` can render them without joining the
    /// execution table; tests may leave them `None` via
    /// [`LiveSpawnRouting::none`].
    ///
    /// Arity is one over clippy's default: the six spawn-identity args
    /// (slot/run/model/pid/binding/capability) predate this method's
    /// routing stamp, and collapsing them further would obscure the
    /// call site. Routing is already a struct to absorb pool + kind.
    ///
    /// Registration is traced, mirroring [`Self::release_slot`]'s removal
    /// trace. This registry is the *only* thing `bossctl agents list`
    /// renders, so "was this run ever listed, and for how long?" is a
    /// question operators ask of the engine trace after the fact. Removal
    /// was already greppable; registration was not, so the trace could
    /// show a slot being cleared with no record that it was ever occupied
    /// — and a run that never appeared in `agents list` was
    /// indistinguishable from one that appeared and was cleared
    /// milliseconds later. `#[track_caller]` names the spawn path
    /// (production dispatch vs. the remote-worker lazy registration)
    /// without threading a reason through every call site. The line is
    /// emitted *after* the insert, as `release_slot` emits its own after
    /// the removal, so the timestamps of the two halves are comparable.
    ///
    /// Displacing an existing entry additionally logs a `warn`: that is
    /// the prior run's last moment of visibility, and it happens with no
    /// `release_slot` to pair against.
    #[allow(clippy::too_many_arguments)]
    #[track_caller]
    pub fn register_spawn_with_capabilities(
        &self,
        slot_id: u8,
        run_id: impl Into<String>,
        model: impl Into<String>,
        shell_pid: i32,
        binding: Option<WorkItemBinding>,
        awaiting_input_capable: bool,
        routing: LiveSpawnRouting,
    ) {
        let caller = std::panic::Location::caller();
        let state = LiveWorkerState::new_spawning_with_routing(
            slot_id,
            run_id,
            model,
            shell_pid,
            binding,
            routing.pool,
            routing.kind,
        );
        // Copy what the trace line needs before `state` is moved into the
        // map. The line itself is emitted *after* the mutation, mirroring
        // `release_slot` — the two halves are meant to be diffed by
        // timestamp for a run id, so each must be stamped at the point its
        // own mutation actually lands, not before.
        let run_id = state.run_id.clone();
        let model = state.model.clone();
        let pool = state.pool.clone();
        let kind = state.kind.clone();
        let work_item_id = state.work_item_id.clone();

        // The whole entry — wire state and engine bookkeeping alike — is
        // replaced in one `insert`. A recycled slot therefore cannot
        // inherit *any* of the previous occupant's metadata. That is
        // hygiene for `progress_fidelity` (a fresh occupant defaults to
        // `Rich` until the spawn flow declares a tier) and load-bearing
        // for `driver_signal_at`: a slot whose prior run had a healthy
        // driver must never vouch for a new run whose driver never
        // exec'd, which is precisely the "one process stands in for
        // another" confusion that signal exists to remove.
        let meta = SlotMeta::builder()
            .spawned_at(boss_engine_utils::epoch_time::now_epoch_secs())
            .awaiting_input_capable(awaiting_input_capable)
            .build();
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let displaced = guard
            .insert(slot_id, SlotEntry { state, meta })
            .map(|prior| prior.state);
        drop(guard);

        tracing::info!(
            slot_id,
            run_id = %run_id,
            model = %model,
            shell_pid,
            pool = pool.as_deref().unwrap_or("-"),
            kind = kind.as_deref().unwrap_or("-"),
            work_item_id = work_item_id.as_deref().unwrap_or("-"),
            awaiting_input_capable,
            replaced_run_id = displaced.as_ref().map(|prior| prior.run_id.as_str()).unwrap_or("-"),
            registered_by = %caller,
            "live-state registry: slot entry registered; run is now visible to `bossctl agents list`",
        );

        // A slot re-registered without an intervening `release_slot` is the
        // desync `EngineToAppError::SlotBusy` exists to prevent. The prior
        // run silently disappears from `agents list` here, so without this
        // line the trace would carry two `registered` events and one
        // `cleared` for the same slot, and diffing the pair for the
        // displaced run id would give the wrong answer.
        if let Some(prior) = displaced {
            tracing::warn!(
                slot_id,
                run_id = %prior.run_id,
                activity = prior.activity.as_str(),
                shell_pid = prior.shell_pid,
                replaced_by_run_id = %run_id,
                registered_by = %caller,
                "live-state registry: registration displaced a live entry without a release_slot; \
                 the prior run's visibility in `bossctl agents list` ends here",
            );
        }
    }

    /// Register a slot for a worker that was **already running** before
    /// the engine re-established tracking for it.
    ///
    /// Same registration as [`Self::register_spawn_with_capabilities`],
    /// plus the two things re-adoption must not get wrong:
    ///
    /// 1. The entry is marked [`DriverStartExpectation::Readopted`], so
    ///    [`Self::unverified_driver_starts`] leaves it alone. Registration
    ///    stamps `spawned_at` with the current time — correct for a spawn,
    ///    a fiction for a re-adoption, where the process may have been
    ///    running for hours. Aging that fiction against
    ///    [`DRIVER_START_GRACE_SECS`] would report a healthy long-running
    ///    worker as a driver that never started and reap it: pane torn
    ///    down (which signals the recorded shell pid's process *group*),
    ///    workspace torn down, cube lease force-released. That is the
    ///    incident re-adoption exists to prevent, re-created by the check
    ///    meant to prevent a different one.
    /// 2. When the re-adoption was triggered by a worker hook
    ///    ([`ReadoptionEvidence::DriverHook`]) the driver signal is
    ///    recorded, because that hook *is* driver-originated proof and
    ///    discarding real evidence is never the safe default. A pid-only
    ///    trigger ([`ReadoptionEvidence::LiveShellPid`]) records nothing:
    ///    a live shell says nothing about the driver.
    ///
    /// Exists as its own method rather than a flag on the spawn
    /// registration so the marking cannot be forgotten by a caller that
    /// registers and then returns — the failure direction of forgetting it
    /// is a live worker being killed.
    #[allow(clippy::too_many_arguments)]
    #[track_caller]
    pub fn register_readoption(
        &self,
        slot_id: u8,
        run_id: impl Into<String>,
        model: impl Into<String>,
        shell_pid: i32,
        binding: Option<WorkItemBinding>,
        awaiting_input_capable: bool,
        routing: LiveSpawnRouting,
        evidence: ReadoptionEvidence,
    ) {
        let run_id = run_id.into();
        self.register_spawn_with_capabilities(
            slot_id,
            run_id.clone(),
            model,
            shell_pid,
            binding,
            awaiting_input_capable,
            routing,
        );
        {
            let mut guard = self.inner.lock().expect("registry mutex poisoned");
            if let Some(entry) = guard.get_mut(&slot_id) {
                entry.meta.driver_start_expectation = DriverStartExpectation::Readopted;
            }
        }
        if evidence == ReadoptionEvidence::DriverHook {
            self.record_driver_signal(&run_id, DriverSignalKind::HookEvent);
        }
        tracing::info!(
            slot_id,
            run_id = %run_id,
            evidence = ?evidence,
            "live-state registry: slot re-adopted for an already-running worker; \
             driver-start verification does not apply to this registration",
        );
    }

    /// Declare the [`ProgressFidelity`] tier for `slot_id`'s driver. The
    /// spawn flow calls this right after `register_spawn` with the
    /// resolved driver's `progress_fidelity()`. Slots this is never called
    /// for (e.g. most tests) default to [`ProgressFidelity::Rich`] via
    /// [`Self::progress_fidelity_for_slot`].
    ///
    /// A no-op for a slot with no live entry, mirroring the other per-slot
    /// setters: the tier belongs to the occupant, so there is nothing to
    /// declare it against.
    pub fn set_progress_fidelity(&self, slot_id: u8, fidelity: ProgressFidelity) {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        if let Some(entry) = guard.get_mut(&slot_id) {
            entry.meta.progress_fidelity = Some(fidelity);
        }
    }

    /// The declared [`ProgressFidelity`] tier for `slot_id`, or
    /// [`ProgressFidelity::Rich`] if never declared. Read by
    /// `crate::stale_worker_sweep`.
    pub fn progress_fidelity_for_slot(&self, slot_id: u8) -> ProgressFidelity {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        guard
            .get(&slot_id)
            .and_then(|entry| entry.meta.progress_fidelity)
            .unwrap_or(ProgressFidelity::Rich)
    }

    /// Record whether the driver spawned into `slot_id` declares
    /// `Capability::AwaitingInputSignal`. `register_spawn` seeds every
    /// slot `true` (Claude's historical behaviour); a caller that resolves
    /// a driver without the capability calls this with `false` so
    /// `apply_event` stops trusting a `WorkerEvent::Notification` for this
    /// slot as an "awaiting human input" signal.
    ///
    /// Deliberately does not attempt to re-derive `WaitingForInput` from a
    /// lower-fidelity channel when set to `false` — per the agent-driver
    /// design's absence policy for this capability (Degrade, not
    /// Synthesize), a driver that can't know this state must not have
    /// Boss guess it. `apply_event` honours that by leaving activity
    /// untouched on a `Notification` it doesn't trust, so the worker
    /// reads as `Working`/`Idle` rather than a fabricated
    /// `WaitingForInput`.
    ///
    /// A no-op (silently ignored, no entry created) if the slot has no
    /// live entry — mirrors the benign-drop behaviour of the other
    /// per-slot setters when a hook or wiring call races spawn/release.
    /// Whether `slot_id`'s driver can signal "awaiting input", the flag that
    /// gates the `WaitingForInput` promotion in `derive_activity` and
    /// `mark_stalled_spawns`.
    ///
    /// Defaults to `true` for a slot with no recorded answer, matching the
    /// gates themselves — an unregistered slot must not silently lose the
    /// promotion. Read side of [`Self::set_awaiting_input_capable`] and of the
    /// `awaiting_input_capable` argument to
    /// [`Self::register_spawn_with_capabilities`], so a registration site's
    /// derivation is assertable rather than only observable through the
    /// activity it later produces.
    pub fn awaiting_input_capable(&self, slot_id: u8) -> bool {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .get(&slot_id)
            .map(|entry| entry.meta.awaiting_input_capable)
            .unwrap_or(true)
    }

    pub fn set_awaiting_input_capable(&self, slot_id: u8, capable: bool) {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        if let Some(entry) = guard.get_mut(&slot_id) {
            entry.meta.awaiting_input_capable = capable;
        }
    }

    /// Drop the entry for `slot_id`. Called when the engine releases
    /// a pane (slot is recycled).
    ///
    /// Removal is traced. Dropping an entry is not merely bookkeeping: a
    /// slot with no live-tracked run becomes a husk candidate, and
    /// `husk_pane_sweep` kills that pane's process one pass later. There
    /// was previously no trace event on removal, so when live workers were
    /// killed the clearing call site was invisible in the logs and the
    /// mechanism could not be identified from a production incident.
    /// `#[track_caller]` records which caller cleared it without requiring
    /// every call site to thread a reason through.
    #[track_caller]
    pub fn release_slot(&self, slot_id: u8) {
        let caller = std::panic::Location::caller();
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        // One `remove` drops the slot's entire footprint — wire state and
        // every piece of engine bookkeeping — so no field can outlive the
        // occupant it describes.
        let removed = guard.remove(&slot_id).map(|entry| entry.state);
        drop(guard);

        match removed {
            Some(state) => tracing::info!(
                slot_id,
                run_id = %state.run_id,
                activity = state.activity.as_str(),
                last_event_at = ?state.last_event_at,
                current_tool = ?state.current_tool,
                shell_pid = state.shell_pid,
                cleared_by = %caller,
                "live-state registry: slot entry cleared; slot is now a husk candidate",
            ),
            None => tracing::debug!(
                slot_id,
                cleared_by = %caller,
                "live-state registry: release_slot on a slot with no entry (no-op)",
            ),
        }
    }

    /// Drop the live-state entry belonging to `run_id`, whichever slot it
    /// currently occupies. Returns the slot id that was released, or
    /// `None` if no live entry matches `run_id` (already released, or
    /// never registered — a benign no-op).
    ///
    /// For callers that only know the run id, not the slot — e.g.
    /// `TransientRecoveryReaper::reap_worker` on a
    /// [`crate::completion::PaneReleaseOutcome::NoLiveWorker`] answer,
    /// where `release_worker_pane` found no run→slot mapping and so never
    /// reached its own [`Self::release_slot`] call. Left alone, that shape
    /// strands both the pool claim and this live-state entry: an entry
    /// still backing the claim is exactly what `pool_claim_sweep` skips by
    /// design, so nothing else ever reconciles it. Dropping the entry here
    /// clears that gate.
    #[track_caller]
    pub fn release_slot_for_run(&self, run_id: &str) -> Option<u8> {
        let slot_id = {
            let guard = self.inner.lock().expect("registry mutex poisoned");
            guard
                .values()
                .find(|entry| entry.state.run_id == run_id)
                .map(|entry| entry.state.slot_id)
        }?;
        self.release_slot(slot_id);
        Some(slot_id)
    }

    /// Snapshot of every entry. Used by the frontend RPC handler and
    /// by the topic publisher.
    pub fn snapshot(&self) -> Vec<LiveWorkerState> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        let mut out: Vec<LiveWorkerState> = guard.values().map(|entry| entry.state.clone()).collect();
        out.sort_by_key(|s| s.slot_id);
        out
    }

    /// Update the shell pid for the slot that owns `run_id`. Returns
    /// the slot id if the entry was found and updated, or `None` if
    /// no live slot matches. Called when the app sends
    /// `UpdateWorkerShellPid` after the libghostty surface initializes.
    pub fn update_shell_pid(&self, run_id: &str, shell_pid: i32) -> Option<u8> {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        for entry in guard.values_mut() {
            if entry.state.run_id == run_id {
                let slot_id = entry.state.slot_id;
                entry.state.shell_pid = shell_pid;
                return Some(slot_id);
            }
        }
        None
    }

    /// Record that a **driver-originated** signal arrived for `run_id` —
    /// positive proof the driver binary is running.
    ///
    /// This is the single writer of `driver_signal_at`, and the only
    /// thing in Boss that may answer "has this driver started?". It is
    /// deliberately keyed by `run_id` rather than slot: the hook ingress
    /// resolves `transcript_path` *before* it looks up the slot mapping
    /// (`worker_events.rs`), and that lookup can legitimately miss for a
    /// hook racing `register_run_slot`. Keying on the run means the
    /// proof lands whenever the live-state registry knows the run, not
    /// only when the slot fan-out survives.
    ///
    /// Idempotent and monotonic: the FIRST signal wins and later ones do
    /// not move the timestamp. The question this answers is "did the
    /// driver ever start?", not "when was it last alive" — that is
    /// `last_event_at`'s job, and conflating the two is what let a
    /// synthesized timestamp masquerade as driver evidence.
    ///
    /// Returns the slot id when a live entry matched, `None` otherwise
    /// (a hook for a released or unknown run — a benign no-op).
    pub fn record_driver_signal(&self, run_id: &str, kind: DriverSignalKind) -> Option<u8> {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let entry = guard.values_mut().find(|entry| entry.state.run_id == run_id)?;
        let slot_id = entry.state.slot_id;
        if entry.meta.driver_signal_at.is_some() {
            // Already proven; keep the first timestamp.
            return Some(slot_id);
        }
        entry.meta.driver_signal_at = Some(boss_engine_utils::epoch_time::now_epoch_secs());
        drop(guard);
        tracing::info!(
            slot_id,
            run_id,
            signal = kind.as_str(),
            "driver-start verified: first driver-originated signal received for this run",
        );
        Some(slot_id)
    }

    /// Whether a driver-originated signal has been recorded for `slot_id`.
    pub fn driver_signal_at(&self, slot_id: u8) -> Option<i64> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        guard.get(&slot_id).and_then(|entry| entry.meta.driver_signal_at)
    }

    /// Whether driver-start verification applies to `slot_id`'s current
    /// registration. `None` for a slot with no live entry.
    pub fn driver_start_expectation(&self, slot_id: u8) -> Option<DriverStartExpectation> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        guard.get(&slot_id).map(|entry| entry.meta.driver_start_expectation)
    }

    /// Every live slot that has gone `threshold_secs` past its spawn
    /// **without any driver-originated signal** — Boss has no evidence
    /// the driver binary ever executed.
    ///
    /// ## What this deliberately does NOT look at
    ///
    /// - **`shell_pid`.** A positive pid is the login shell hosting the
    ///   pane, not the driver. `crate::spawn_ack_sweep`'s old
    ///   `shell_pid > 0` skip and `mark_stalled_spawns`'s inverse
    ///   `shell_pid <= 0` skip between them left a slot with a live
    ///   shell and no driver owned by neither.
    /// - **`activity`.** Restricting to `Spawning` would re-open the
    ///   same hole from the other side: `mark_stalled_spawns` promotes a
    ///   capability-declaring driver's identical failure to
    ///   `WaitingForInput`, which would then escape this check.
    /// - **`awaiting_input_capable`.** The capability gates whether Boss
    ///   may *interpret* a `Notification` as "awaiting a human". It has
    ///   nothing to say about whether a process exists, so it must not
    ///   gate driver-start verification — that exemption is exactly why
    ///   grok's occurrence went undetected.
    /// - **`last_event_at`.** Written by `mark_stalled_spawns` and
    ///   `mark_errored` from engine-side inference. Only
    ///   `driver_signal_at` is unforgeable.
    ///
    /// The result is that this check fires for every driver, with any
    /// capability set, in any activity, with or without a reported pid.
    ///
    /// ## The one exemption, and why it is not a hole
    ///
    /// A slot registered by [`Self::register_readoption`] —
    /// [`DriverStartExpectation::Readopted`] — is skipped. That path
    /// re-registers a worker that was **already running**, so its
    /// `spawned_at` records when the engine noticed the process, not when
    /// anything exec'd; a worker re-adopted after six hours of work would
    /// otherwise be 300 s "silent" the instant it was re-adopted and be
    /// reaped for a driver start that happened long before Boss lost
    /// track of it. The exemption is narrow in the way that matters: it
    /// turns off *this* check only, and a re-adopted worker remains fully
    /// owned by the rules that judge a process on evidence about the
    /// process — `crate::dead_pid_sweep`, `crate::husk_pane_sweep`,
    /// `crate::stale_worker_sweep`, and `crate::orphan_sweep`'s redispatch
    /// guard, which is itself one of the two triggers that produce a
    /// re-adoption. A re-adoption triggered by a worker hook additionally
    /// carries a real `driver_signal_at`, so it would be skipped by the
    /// first check above regardless.
    ///
    /// A slot whose `spawned_at` is in the future is skipped as too
    /// recent, the same as any other in-window spawn.
    pub fn unverified_driver_starts(&self, now_epoch_secs: i64, threshold_secs: i64) -> Vec<UnverifiedDriverStart> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        let cutoff = now_epoch_secs.saturating_sub(threshold_secs);
        let mut out = Vec::new();
        for (slot_id, entry) in guard.iter() {
            if entry.meta.driver_signal_at.is_some() {
                continue;
            }
            if entry.meta.driver_start_expectation == DriverStartExpectation::Readopted {
                continue;
            }
            if entry.meta.spawned_at > cutoff {
                continue;
            }
            out.push(UnverifiedDriverStart {
                slot_id: *slot_id,
                run_id: entry.state.run_id.clone(),
                shell_pid: entry.state.shell_pid,
                silent_secs: now_epoch_secs.saturating_sub(entry.meta.spawned_at),
                activity: entry.state.activity,
            });
        }
        out.sort_by_key(|c| c.slot_id);
        out
    }

    /// Set the `held` flag for the slot that owns `run_id` — mirrors
    /// [`Self::update_shell_pid`]'s find-and-set shape. Returns the slot
    /// id if the entry was found and updated, or `None` if no live slot
    /// matches. Called by the `HoldRun`/`ReleaseHoldRun` RPC handlers so
    /// `bossctl agents list`/`status` reflect an operator hold
    /// immediately, without waiting for the next hook event.
    pub fn set_held(&self, run_id: &str, held: bool) -> Option<u8> {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        for entry in guard.values_mut() {
            if entry.state.run_id == run_id {
                let slot_id = entry.state.slot_id;
                entry.state.held = held;
                return Some(slot_id);
            }
        }
        None
    }

    /// Look up the state for one slot.
    pub fn get(&self, slot_id: u8) -> Option<LiveWorkerState> {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .get(&slot_id)
            .map(|entry| entry.state.clone())
    }

    /// Return the `run_id` of the non-terminal slot currently working on
    /// `work_item_id`, or `None` if no such slot exists. Used by the
    /// chore-update notification path to locate the worker that needs to
    /// hear about an in-flight spec change.
    pub fn run_id_for_work_item(&self, work_item_id: &str) -> Option<String> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        guard
            .values()
            .map(|entry| &entry.state)
            .find(|state| !state.activity.is_terminal() && state.work_item_id.as_deref() == Some(work_item_id))
            .map(|state| state.run_id.clone())
    }

    /// Return the current `shell_pid` for the non-terminal slot running
    /// `run_id`, or `None` if no such slot exists or its pid is unset
    /// (`0`, the not-yet-plumbed-back sentinel — see
    /// [`boss_protocol::LiveWorkerState::shell_pid`]). Used by
    /// [`crate::background_children::RegistryBackgroundActivityProbe`] to
    /// resolve a Stop-boundary execution id to the pid its process-tree
    /// scan should walk.
    pub fn shell_pid_for_run(&self, run_id: &str) -> Option<i32> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        guard
            .values()
            .map(|entry| &entry.state)
            .find(|state| !state.activity.is_terminal() && state.run_id == run_id)
            .map(|state| state.shell_pid)
            .filter(|pid| *pid > 0)
    }

    /// Return the most recent hook-activity stamp for `run_id`. Callers use
    /// this as an opaque watermark: equality means no hook arrived since the
    /// snapshot, while a change proves the worker resumed after its Stop.
    ///
    /// Deliberately reads `last_tool_ended_at` alone, never `last_event_at`.
    /// `last_tool_ended_at` is written from exactly one place —
    /// [`Self::apply_event`]'s `PostToolUse` arm — so it can only advance on
    /// a real hook from the worker. `last_event_at` is also stamped by
    /// engine-side inference ([`Self::mark_stalled_spawns`],
    /// [`Self::mark_errored`]) that runs with no worker activity at all;
    /// treating it as proof of resumption would let the engine's own
    /// bookkeeping (e.g. an events-socket decode failure) retire a
    /// suppressed nudge for a worker that never actually resumed — the
    /// exact fail-closed failure mode
    /// [`crate::completion::WorkerCompletionHandler::recheck_background_nudge`]
    /// must avoid. `None` here means "no hook-only evidence available yet",
    /// not "nothing changed" — callers must treat it as inconclusive rather
    /// than as license to retire tracking.
    pub fn activity_watermark_for_run(&self, run_id: &str) -> Option<String> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        guard
            .values()
            .find(|entry| !entry.state.activity.is_terminal() && entry.state.run_id == run_id)
            .and_then(|entry| entry.state.last_tool_ended_at.clone())
    }

    /// True iff a live state entry exists for `run_id` whose activity
    /// indicates the worker is still attached to the slot. Used by
    /// `RequestExecution` to detect "the latest execution is
    /// non-terminal on paper but the worker is gone" — that's the
    /// stale-`waiting_human` shape that would otherwise make a
    /// kanban-driven re-dispatch a silent no-op.
    ///
    /// `Terminated` and `Errored` count as **not** live: the slot is
    /// no longer holding the run open. Everything else
    /// (`Spawning`/`Working`/`WaitingForInput`/`Idle`) does.
    pub fn is_run_live(&self, run_id: &str) -> bool {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        guard
            .values()
            .any(|entry| entry.state.run_id == run_id && !entry.state.activity.is_terminal())
    }

    /// Return the current [`WorkerActivity`] for the non-terminal slot
    /// running `run_id`, or `None` if no such slot exists. Used by the
    /// merge-poller staged-URL recheck path to decide whether a live
    /// worker is mid-turn (`Working`) — finalizing while mid-turn reaps
    /// the worker before its remaining prompt steps run. If duplicate live
    /// slots exist for a run, prefers `Working` conservatively.
    pub fn activity_for_run(&self, run_id: &str) -> Option<WorkerActivity> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        let mut first = None;
        for state in guard.values().map(|entry| &entry.state) {
            if state.activity.is_terminal() || state.run_id != run_id {
                continue;
            }
            if state.activity == WorkerActivity::Working {
                return Some(WorkerActivity::Working);
            }
            first.get_or_insert(state.activity);
        }
        first
    }

    /// Return `last_event_at` for the non-terminal slot running `run_id`.
    /// Prefer a `Working` slot when duplicates exist so the mid-turn PR
    /// completion horizon is measured against the activity that is actually
    /// blocking terminalization.
    pub fn last_event_at_for_run(&self, run_id: &str) -> Option<String> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        let mut first = None;
        for state in guard.values().map(|entry| &entry.state) {
            if state.activity.is_terminal() || state.run_id != run_id {
                continue;
            }
            if state.activity == WorkerActivity::Working {
                return state.last_event_at.clone();
            }
            if first.is_none() {
                first = state.last_event_at.clone();
            }
        }
        first
    }

    /// Return `current_tool` (a tool in flight — an unbalanced `PreToolUse`)
    /// for the non-terminal slot running `run_id`. Mirrors
    /// [`Self::last_event_at_for_run`]'s duplicate-slot preference so the two
    /// stay consistent when read together for liveness corroboration — see
    /// [`crate::durable_liveness::corroborating_liveness`].
    pub fn current_tool_for_run(&self, run_id: &str) -> Option<String> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        let mut first = None;
        for state in guard.values().map(|entry| &entry.state) {
            if state.activity.is_terminal() || state.run_id != run_id {
                continue;
            }
            if state.activity == WorkerActivity::Working {
                return state.current_tool.clone();
            }
            if first.is_none() {
                first = state.current_tool.clone();
            }
        }
        first
    }

    /// Apply a hook event to the state for `slot_id`. Returns `true`
    /// if the entry actually changed, so callers can suppress no-op
    /// topic pushes. Returns `false` if no entry exists for the slot
    /// (event arrived before spawn registered or after release) — the
    /// caller should treat that as a benign drop.
    ///
    /// `SessionStart` carries an optional `model` from the hook payload;
    /// when present it is treated as authoritative and overwrites the
    /// launch default stamped at spawn. When absent (Codex stdout
    /// `thread.started`, older fixtures), the launch default is retained.
    pub fn apply_event(&self, slot_id: u8, event: &WorkerEvent) -> bool {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let now = current_iso8601();
        // The wire state and the per-slot bookkeeping this arm reads and
        // writes now live in one entry, so a single lookup reaches both;
        // `SlotEntry`'s fields split-borrow without a second map access.
        let Some(SlotEntry { state, meta }) = guard.get_mut(&slot_id) else {
            return false;
        };
        let before = state.clone();

        state.last_event_at = Some(now);
        // Any hook event is proof the worker's session is responsive
        // again — clear a stale recovery banner regardless of which
        // event arrived, so real progress after a nudge is never
        // shadowed by "recovering from API error …".
        state.recovery_status = None;

        match event {
            WorkerEvent::SessionStart { source, model, .. } => {
                // Authoritative model from the hook when present. Launch
                // defaults (`opus`, resolved effort slug, …) are a
                // provisional stamp so the UI never shows "Claude Unknown"
                // before the first hook; once SessionStart reports the
                // real id we prefer it. Empty/None leaves the launch value.
                if let Some(model) = model {
                    state.model = model.clone();
                }
                // SessionStart with source=resume keeps the existing
                // activity when the slot has already left Spawning
                // (worker is resuming mid-life, not spawning fresh). For
                // Startup — and for any SessionStart that arrives while
                // still Spawning — leave Spawning for Idle: the session
                // is alive. SessionStart alone does not start a turn;
                // Working arrives on UserPromptSubmit / PreToolUse.
                if state.activity == WorkerActivity::Spawning
                    && matches!(
                        source,
                        SessionStartSource::Startup
                            | SessionStartSource::Clear
                            | SessionStartSource::Compact
                            | SessionStartSource::Other
                    )
                {
                    state.activity = WorkerActivity::Idle;
                }
                // Resume deliberately leaves Spawning alone so reattach /
                // spawn-ack proof-of-life can stamp last_event_at without
                // claiming the worker is past spawn. The stale-activity
                // timer ([`Self::downgrade_stale_activity`]) then moves
                // Spawning → Idle once last_event_at ages out, instead of
                // silently advertising "spawning" after events.sock
                // degrades and no further hooks arrive.
            }
            WorkerEvent::UserPromptSubmit { .. } => {
                state.activity = WorkerActivity::Working;
                state.current_tool = None;
            }
            WorkerEvent::PreToolUse { tool_name, .. } => {
                state.activity = WorkerActivity::Working;
                state.current_tool = Some(tool_name.clone());
                meta.notification_pending = false;
            }
            WorkerEvent::PostToolUse { .. } => {
                state.current_tool = None;
                state.last_tool_ended_at = state.last_event_at.clone();
                // Don't flip to Idle here — Stop is the authoritative
                // turn boundary. Worker may chain multiple tools.
                state.activity = WorkerActivity::Working;
            }
            WorkerEvent::Notification { .. } => {
                // Only trust this as an "awaiting human input" signal when
                // the run's driver declared `Capability::AwaitingInputSignal`
                // (see `set_awaiting_input_capable`). Absent that — a
                // driver that doesn't back the signal, or emitted one it
                // shouldn't have — leave activity untouched rather than
                // guess: this is one of two places the "don't fake
                // WaitingForInput" contract is enforced, the other being
                // `mark_stalled_spawns`'s own `awaiting_input_capable` check.
                if meta.awaiting_input_capable {
                    state.activity = WorkerActivity::WaitingForInput;
                    state.current_tool = None;
                    meta.notification_pending = true;
                }
            }
            WorkerEvent::Stop { .. } => {
                let was_pending = std::mem::take(&mut meta.notification_pending);
                state.current_tool = None;
                state.activity = if was_pending {
                    WorkerActivity::WaitingForInput
                } else {
                    WorkerActivity::Idle
                };
            }
            WorkerEvent::SessionEnd { .. } => {
                state.activity = WorkerActivity::Terminated;
                // Deliberately does NOT clear `current_tool`. On the normal
                // path `Stop` has already cleared it, so preserving it here
                // is a no-op. On the ABNORMAL path — a `SessionEnd` that
                // arrives while a `PreToolUse` is still unbalanced — the
                // unbalanced tool is the single most valuable piece of
                // evidence the engine holds: the worker was mid-tool when
                // the session claimed to end, so the claim is contradicted
                // by the worker's own hook stream and the process is very
                // likely still running. `husk_pane_sweep` reads exactly
                // this field (via
                // [`crate::husk_pane_sweep::live_process_evidence`]) before
                // it kills a pane's process, and clearing it here erased
                // the contradiction at precisely the moment it mattered.
                //
                // 2026-07-26: six live workers received a synchronized
                // `SessionEnd { reason: "other" }` burst inside 250ms while
                // their `claude` processes kept running (three were inside a
                // multi-minute foreground `bazel` build, so no further hook
                // was ever going to arrive). This arm flipped all six to
                // `Terminated` and wiped their in-flight tool; 107 seconds
                // later the husk sweep retired five of them, killing live
                // work. Keeping `current_tool` is what lets the corroboration
                // guard see through a `SessionEnd` the process did not honor.
                meta.notification_pending = false;
            }
        }

        before != *state
    }

    /// Replace the live-status string for `slot_id` and stamp
    /// `live_status_at` with the current ISO-8601 timestamp. Returns
    /// `true` iff the entry actually changed — callers gate the
    /// `broadcast_live_worker_states` push on this exactly like
    /// [`Self::apply_event`] does.
    ///
    /// Pass `Some(text)` to set the field and `None` to clear it
    /// (used when a worker has been idle long enough that the prior
    /// summary would be misleading). Clearing also wipes
    /// `live_status_at` so the staleness UI never has a dangling
    /// timestamp.
    ///
    /// Returns `false` if no entry exists for the slot (event
    /// arrived before spawn registered, or after release) — the
    /// caller treats that as a benign drop, mirroring `apply_event`.
    ///
    /// The registry never decides on its own whether the update is
    /// appropriate for the current activity. The trigger fan-in
    /// owns that policy (e.g., don't refresh while `Spawning`,
    /// suppress stale writes after `Idle`); the registry just stores
    /// the value the caller passed.
    pub fn set_live_status(&self, slot_id: u8, status: Option<String>) -> bool {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let Some(state) = guard.get_mut(&slot_id).map(|entry| &mut entry.state) else {
            return false;
        };
        match (&status, &state.live_status) {
            (None, None) => {
                // Already cleared; nothing to broadcast.
                false
            }
            (None, Some(_)) => {
                // Clearing wipes both halves of the pair so the
                // staleness UI never has a dangling timestamp.
                state.live_status = None;
                state.live_status_at = None;
                true
            }
            (Some(_), _) => {
                // Always advance the timestamp on a successful set —
                // the staleness UI keys off it and the broadcast cost
                // (8 slots × < 1 KiB at < 1 Hz aggregate) is the
                // budget the design's Q6 already accepted. The
                // text-equality short-circuit was tempting but would
                // freeze `last_status_at` until the model picked a
                // different phrasing, which is exactly the
                // "no summarizer activity for >5min" stale signal
                // we'd then misfire on.
                state.live_status = status;
                state.live_status_at = Some(current_iso8601());
                true
            }
        }
    }

    /// Replace the `recovery_status` banner for `slot_id` — set by
    /// [`crate::transient_recovery`] while a slot is being auto-recovered
    /// from a transient Claude API error. Returns `true` iff the entry
    /// actually changed.
    ///
    /// Deliberately independent of [`Self::set_live_status`]: that
    /// field's owner (the live-status summarizer loop) clears it after
    /// ~30s of continuous `Idle`, which is shorter than the
    /// transient-recovery grace period — coupling the two would have
    /// the recovery banner wiped before a human ever saw it. This field
    /// is instead cleared by [`Self::apply_event`] the moment any hook
    /// event arrives (proof the worker resumed) or by [`Self::release_slot`]
    /// when the slot is torn down.
    pub fn set_recovery_status(&self, slot_id: u8, status: Option<String>) -> bool {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let Some(state) = guard.get_mut(&slot_id).map(|entry| &mut entry.state) else {
            return false;
        };
        if state.recovery_status == status {
            return false;
        }
        state.recovery_status = status;
        true
    }

    /// Mark a slot as errored. Used when the events socket fails to
    /// decode a payload or repeatedly drops connections. Returns
    /// `true` if the entry actually changed.
    pub fn mark_errored(&self, slot_id: u8) -> bool {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let Some(state) = guard.get_mut(&slot_id).map(|entry| &mut entry.state) else {
            return false;
        };
        if state.activity == WorkerActivity::Errored {
            return false;
        }
        state.activity = WorkerActivity::Errored;
        state.current_tool = None;
        state.last_event_at = Some(current_iso8601());
        true
    }

    /// Detect worker slots stuck in `Spawning` with no hook events for
    /// longer than `threshold_secs` seconds and transition them to
    /// `WaitingForInput`.
    ///
    /// The initial directory-trust prompt that Claude Code shows at
    /// session startup (for models that use `--permission-mode auto`)
    /// fires *before* `SessionStart`, so no hook event ever arrives and
    /// the normal `Notification`→`WaitingForInput` path is never
    /// triggered. An unattended headless worker can never answer the
    /// prompt, so the run stalls indefinitely with no UI signal. This
    /// method is the detection path: if `last_event_at` is `None` (no
    /// hook at all) and the slot has been in `Spawning` for more than
    /// `threshold_secs` seconds, the activity is promoted to
    /// `WaitingForInput` so the existing kanban dot and
    /// `WorkerWaitingIndicator` fire.
    ///
    /// **Requires `shell_pid > 0`.** A slot that never reported a shell
    /// pid at all has produced no evidence that any process — let alone
    /// one blocked on an interactive prompt — ever started. Promoting
    /// such a slot to `WaitingForInput` is exactly the 2026-07-03/04
    /// false-live incident: the slot sat at `activity=waiting_for_input,
    /// shell_pid=0` forever, presenting as "the worker needs a human"
    /// when there was nothing an operator could attach to and answer.
    /// A pid-less spawn stall is a different failure class, left in
    /// `Spawning` here and handled instead by
    /// `crate::spawn_ack_sweep::run_one_pass`, which terminal-fails and
    /// redispatches it after a longer grace window.
    ///
    /// **Also gated on `awaiting_input_capable`.** A slot whose driver
    /// doesn't declare `Capability::AwaitingInputSignal` is left in
    /// `Spawning` here too — promoting it would be exactly the same kind
    /// of "no events for N seconds ⇒ assume the worker awaits a human"
    /// guess `apply_event` refuses to make for an untrusted `Notification`.
    ///
    /// ## Reconciliation with driver-start verification
    ///
    /// Both skips above are *presentation* decisions — "may Boss claim
    /// this worker awaits a human?" — and both remain correct as stated.
    /// What they must never do is decide whether the slot keeps its
    /// resources, and before driver-start verification existed they did
    /// exactly that by omission: the `awaiting_input_capable` skip left
    /// grok's never-started spawn parked at `Spawning` forever, and the
    /// promotion in the capability-declaring case moved the slot out of
    /// `Spawning` where `spawn_ack_sweep`'s activity filter could no
    /// longer see it. Neither escape survives now:
    ///
    /// - [`Self::unverified_driver_starts`] reads only `driver_signal_at`
    ///   and `spawned_at`, so it is blind to activity, capability and pid
    ///   and covers both branches identically.
    /// - The `last_event_at` this method synthesizes below is explicitly
    ///   NOT a driver signal. It moves the display timestamp only;
    ///   `driver_signal_at` is untouched, so a promotion here can never
    ///   vouch for a driver that never ran.
    ///
    /// Returns the slot IDs that were changed so callers can broadcast
    /// the updated snapshot. Normal-running workers (whose `SessionStart`
    /// hook fires within seconds of spawn) always have `last_event_at`
    /// set before the threshold elapses; this method ignores them.
    pub fn mark_stalled_spawns(&self, now_epoch_secs: i64, threshold_secs: i64) -> Vec<u8> {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let cutoff = now_epoch_secs.saturating_sub(threshold_secs);
        let mut changed = Vec::new();
        for (slot_id, SlotEntry { state, meta }) in guard.iter_mut() {
            if state.activity != WorkerActivity::Spawning {
                continue;
            }
            if state.shell_pid <= 0 {
                // No process has reported in at all — `spawn_ack_sweep`
                // owns this slot, not the directory-trust-prompt path.
                continue;
            }
            if !meta.awaiting_input_capable {
                // This driver never declared `Capability::AwaitingInputSignal`,
                // so — same "don't fake it" contract `apply_event` enforces on
                // `Notification` — this sweep must not promote the slot either.
                // Leave it in `Spawning` rather than guess, mirroring the
                // zero-pid case just above; `dead_pid_sweep`'s process-liveness
                // backstop is the honest fallback for this driver class (see
                // the design doc's "ProgressObservation minimum-fidelity tier"
                // decision).
                continue;
            }
            if state.last_event_at.is_some() {
                // SessionStart (or any other hook) already fired — the
                // worker is past the startup phase; not our concern.
                continue;
            }
            if meta.spawned_at > cutoff {
                // Spawned too recently; give the worker more time.
                continue;
            }
            state.activity = WorkerActivity::WaitingForInput;
            // Display timestamp only — this is the engine narrating its
            // own inference, not the driver reporting in. `driver_signal_at`
            // is deliberately NOT written here: if it were, this promotion
            // would silently satisfy driver-start verification and re-open
            // the hole. See `unverified_driver_starts`.
            state.last_event_at = Some(iso8601_utc(now_epoch_secs));
            changed.push(*slot_id);
        }
        changed
    }

    /// Downgrade `Spawning` slots whose `last_event_at` is older than
    /// `threshold_secs` to `Idle`.
    ///
    /// Complements [`Self::mark_stalled_spawns`]: that method only
    /// considers slots that have *never* received a hook
    /// (`last_event_at == None`) and promotes them to
    /// `WaitingForInput` under the directory-trust-prompt hypothesis.
    /// This method owns the complementary lie — a slot that *did*
    /// receive a hook (so `last_event_at` is set) but is still
    /// advertising `Spawning`. That shape arises when:
    ///
    /// - `SessionStart(Resume)` stamps `last_event_at` without leaving
    ///   `Spawning` (deliberate: reattach/spawn-ack need a proof-of-life
    ///   signal that does not claim the worker is past spawn), then
    /// - `events.sock` degrades and no further hooks arrive.
    ///
    /// After the threshold the honest claim is "we saw life, then
    /// silence" — `Idle` — not "still spawning". Leaves
    /// `last_event_at == None` alone (stalled-spawn / spawn-ack own
    /// that), and never touches non-`Spawning` activities
    /// (`Working` with a long think, `WaitingForInput` while a human
    /// decides, …).
    ///
    /// Returns the slot IDs that changed so callers can broadcast.
    pub fn downgrade_stale_activity(&self, now_epoch_secs: i64, threshold_secs: i64) -> Vec<u8> {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let cutoff = iso8601_utc(now_epoch_secs.saturating_sub(threshold_secs));
        let mut changed = Vec::new();
        for (slot_id, entry) in guard.iter_mut() {
            let state = &mut entry.state;
            if state.activity != WorkerActivity::Spawning {
                continue;
            }
            let Some(last) = state.last_event_at.as_deref() else {
                // No event yet — mark_stalled_spawns / spawn_ack_sweep.
                continue;
            };
            // Fixed-width ISO-8601: lexicographic order == chronological.
            if last >= cutoff.as_str() {
                continue;
            }
            state.activity = WorkerActivity::Idle;
            state.current_tool = None;
            changed.push(*slot_id);
        }
        changed
    }

    /// Override the recorded spawn timestamp for `slot_id`. Only
    /// available in tests — production code always uses the wall-clock
    /// time stamped by `register_spawn`.
    #[cfg(test)]
    pub fn set_spawn_time_for_test(&self, slot_id: u8, epoch_secs: i64) {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        if let Some(entry) = guard.get_mut(&slot_id) {
            entry.meta.spawned_at = epoch_secs;
        }
    }

    /// Override `last_event_at` for `slot_id` to an arbitrary ISO-8601
    /// string. Test seam for reproducing a *recycled-slot* live state —
    /// a slot whose `run_id` was replaced for the current execution but
    /// whose `last_event_at` still carries a prior run's timestamp. Only
    /// available in tests; production stamps this wall-clock in
    /// `apply_event`.
    #[cfg(test)]
    pub fn set_last_event_at_for_test(&self, slot_id: u8, last_event_at: impl Into<String>) {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        if let Some(entry) = guard.get_mut(&slot_id) {
            entry.state.last_event_at = Some(last_event_at.into());
        }
    }
}

fn current_iso8601() -> String {
    let secs = boss_engine_utils::epoch_time::now_epoch_secs();
    boss_engine_utils::iso8601::format_epoch_iso8601(secs)
}

/// Format `epoch_secs` as the same fixed-width ISO-8601 UTC string
/// (`YYYY-MM-DDTHH:MM:SSZ`) the registry stamps into `last_event_at`.
/// Because the format is fixed-width, lexicographic string ordering
/// matches chronological ordering — the stale-worker sweep builds a
/// cutoff timestamp with this and compares `last_event_at < cutoff`
/// directly, with no date parsing.
pub fn iso8601_utc(epoch_secs: i64) -> String {
    boss_engine_utils::iso8601::format_epoch_iso8601(epoch_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::StopReason;

    fn pre_tool(tool: &str) -> WorkerEvent {
        WorkerEvent::PreToolUse {
            session_id: "s".into(),
            tool_name: tool.into(),
            tool_input: serde_json::Value::Null,
        }
    }

    fn post_tool(tool: &str) -> WorkerEvent {
        WorkerEvent::PostToolUse {
            session_id: "s".into(),
            tool_name: tool.into(),
            tool_input: serde_json::Value::Null,
            tool_response: serde_json::Value::Null,
        }
    }

    fn stop_event() -> WorkerEvent {
        WorkerEvent::Stop {
            session_id: "s".into(),
            stop_hook_active: false,
            stop_reason: StopReason::Completed,
        }
    }

    #[test]
    fn update_shell_pid_finds_slot_by_run_id() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(3, "run-abc", "claude-opus-4-7", 0, None);
        let slot = reg.update_shell_pid("run-abc", 55555);
        assert_eq!(slot, Some(3));
        let state = reg.get(3).unwrap();
        assert_eq!(state.shell_pid, 55555);
    }

    #[test]
    fn update_shell_pid_returns_none_for_unknown_run_id() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(3, "run-abc", "claude-opus-4-7", 0, None);
        let slot = reg.update_shell_pid("run-xyz", 99999);
        assert_eq!(slot, None);
        let state = reg.get(3).unwrap();
        assert_eq!(state.shell_pid, 0, "unmatched run must not be modified");
    }

    #[test]
    fn register_spawn_creates_entry_with_spawning_activity() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(2, "run-1", "claude-opus-4-7", 12345, None);
        let state = reg.get(2).unwrap();
        assert_eq!(state.slot_id, 2);
        assert_eq!(state.run_id, "run-1");
        assert_eq!(state.model, "claude-opus-4-7");
        assert_eq!(state.shell_pid, 12345);
        assert_eq!(state.activity, WorkerActivity::Spawning);
        assert!(state.work_item_id.is_none());
        assert!(state.work_item_name.is_none());
        assert!(state.execution_id.is_none());
    }

    #[test]
    fn activity_for_run_prefers_working_across_duplicate_live_slots() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.register_spawn(2, "run-1", "claude-opus-4-7", 2, None);
        reg.apply_event(
            1,
            &WorkerEvent::Stop {
                session_id: "s".into(),
                stop_hook_active: false,
                stop_reason: StopReason::Completed,
            },
        );
        reg.apply_event(
            2,
            &WorkerEvent::PreToolUse {
                session_id: "s".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::Value::Null,
            },
        );

        assert_eq!(reg.activity_for_run("run-1"), Some(WorkerActivity::Working));
    }

    #[test]
    fn register_spawn_with_binding_records_work_item_fields() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(
            2,
            "exec-1",
            "claude-opus-4-7",
            12345,
            Some(WorkItemBinding {
                work_item_id: "task_18ad1b81532ac910_4".into(),
                work_item_name: "Fix fencer scraping".into(),
                execution_id: "exec-1".into(),
            }),
        );
        let state = reg.get(2).unwrap();
        assert_eq!(state.work_item_id.as_deref(), Some("task_18ad1b81532ac910_4"));
        assert_eq!(state.work_item_name.as_deref(), Some("Fix fencer scraping"));
        assert_eq!(state.execution_id.as_deref(), Some("exec-1"));
        assert!(state.pool.is_none());
        assert!(state.kind.is_none());
    }

    #[test]
    fn register_spawn_with_capabilities_stamps_pool_and_kind() {
        // Production spawn paths pass attributed pool + execution kind so
        // `bossctl agents list` can render them without joining the
        // execution table. Tests that use `register_spawn` leave both None.
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn_with_capabilities(
            2,
            "exec-1",
            "claude-opus-4-7",
            12345,
            Some(WorkItemBinding {
                work_item_id: "task_abc".into(),
                work_item_name: "Fix fencer scraping".into(),
                execution_id: "exec-1".into(),
            }),
            true,
            LiveSpawnRouting::new("automation", "chore_implementation"),
        );
        let state = reg.get(2).unwrap();
        assert_eq!(state.pool.as_deref(), Some("automation"));
        assert_eq!(state.kind.as_deref(), Some("chore_implementation"));
    }

    #[test]
    fn attributed_pool_label_matches_coordinator_routing() {
        assert_eq!(attributed_pool_label(ExecutionKind::PrReview, false), "review");
        assert_eq!(attributed_pool_label(ExecutionKind::PrReview, true), "review");
        assert_eq!(
            attributed_pool_label(ExecutionKind::AutomationTriage, false),
            "automation"
        );
        assert_eq!(
            attributed_pool_label(ExecutionKind::TaskImplementation, true),
            "automation"
        );
        assert_eq!(attributed_pool_label(ExecutionKind::ChoreImplementation, false), "main");
        assert_eq!(
            attributed_pool_label(ExecutionKind::RevisionImplementation, false),
            "main"
        );
    }

    #[test]
    fn release_slot_clears_entry() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        assert!(reg.get(1).is_some());
        reg.release_slot(1);
        assert!(reg.get(1).is_none());
    }

    #[test]
    fn pre_tool_use_marks_working_with_tool_name() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        let changed = reg.apply_event(1, &pre_tool("Bash"));
        assert!(changed);
        let state = reg.get(1).unwrap();
        assert_eq!(state.activity, WorkerActivity::Working);
        assert_eq!(state.current_tool.as_deref(), Some("Bash"));
        assert!(state.last_event_at.is_some());
    }

    #[test]
    fn post_tool_use_clears_current_tool_and_records_end_time() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(1, &pre_tool("Bash"));
        reg.apply_event(1, &post_tool("Bash"));
        let state = reg.get(1).unwrap();
        assert!(state.current_tool.is_none());
        assert!(state.last_tool_ended_at.is_some());
        assert_eq!(state.activity, WorkerActivity::Working);
    }

    #[test]
    fn stop_after_tools_transitions_to_idle() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(1, &pre_tool("Bash"));
        reg.apply_event(1, &post_tool("Bash"));
        reg.apply_event(1, &stop_event());
        let state = reg.get(1).unwrap();
        assert_eq!(state.activity, WorkerActivity::Idle);
        assert!(state.current_tool.is_none());
    }

    #[test]
    fn notification_then_stop_marks_waiting_for_input() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(
            1,
            &WorkerEvent::Notification {
                session_id: "s".into(),
                message: "claude needs permission".into(),
            },
        );
        reg.apply_event(1, &stop_event());
        let state = reg.get(1).unwrap();
        assert_eq!(state.activity, WorkerActivity::WaitingForInput);
    }

    #[test]
    fn pretooluse_after_notification_clears_pending_flag_and_marks_working() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(
            1,
            &WorkerEvent::Notification {
                session_id: "s".into(),
                message: "permission".into(),
            },
        );
        reg.apply_event(1, &pre_tool("Edit"));
        reg.apply_event(1, &stop_event());
        let state = reg.get(1).unwrap();
        // Stop without a fresh notification should now be Idle.
        assert_eq!(state.activity, WorkerActivity::Idle);
    }

    #[test]
    fn awaiting_input_incapable_driver_never_shows_waiting_for_input() {
        // A driver that doesn't declare `Capability::AwaitingInputSignal`
        // must never produce `WaitingForInput`, even if a `Notification`
        // event somehow arrives — the honest degrade is to leave activity
        // untouched (Working here) so `Stop` falls through to `Idle`,
        // never a fabricated `WaitingForInput`.
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.set_awaiting_input_capable(1, false);
        reg.apply_event(1, &pre_tool("Bash"));
        let before = reg.get(1).unwrap();
        assert_eq!(before.activity, WorkerActivity::Working);

        reg.apply_event(
            1,
            &WorkerEvent::Notification {
                session_id: "s".into(),
                message: "claude needs permission".into(),
            },
        );
        let after_notification = reg.get(1).unwrap();
        assert_eq!(
            after_notification.activity,
            WorkerActivity::Working,
            "an untrusted Notification must not change activity"
        );

        reg.apply_event(1, &stop_event());
        let after_stop = reg.get(1).unwrap();
        assert_eq!(
            after_stop.activity,
            WorkerActivity::Idle,
            "Stop must resolve to Idle, not a guessed WaitingForInput"
        );
    }

    #[test]
    fn awaiting_input_capable_defaults_true_matching_claude_behaviour() {
        // Every existing caller of `register_spawn` (Claude is the only
        // production driver today) must see byte-identical behaviour
        // without calling `set_awaiting_input_capable` at all.
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(
            1,
            &WorkerEvent::Notification {
                session_id: "s".into(),
                message: "claude needs permission".into(),
            },
        );
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);
    }

    #[test]
    fn release_slot_resets_awaiting_input_capable_to_default_on_respawn() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.set_awaiting_input_capable(1, false);
        reg.release_slot(1);
        reg.register_spawn(1, "run-2", "claude-opus-4-7", 1, None);
        reg.apply_event(
            1,
            &WorkerEvent::Notification {
                session_id: "s".into(),
                message: "claude needs permission".into(),
            },
        );
        assert_eq!(
            reg.get(1).unwrap().activity,
            WorkerActivity::WaitingForInput,
            "a fresh spawn into a recycled slot must not inherit the prior run's flag"
        );
    }

    #[test]
    fn register_spawn_with_capabilities_seeds_flag_at_registration() {
        // The capability travels with registration in one call, so there is
        // no window where a hook event could race a follow-up setter call.
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn_with_capabilities(1, "run-1", "claude-opus-4-7", 1, None, false, LiveSpawnRouting::none());
        reg.apply_event(
            1,
            &WorkerEvent::Notification {
                session_id: "s".into(),
                message: "claude needs permission".into(),
            },
        );
        assert_eq!(
            reg.get(1).unwrap().activity,
            WorkerActivity::Spawning,
            "an untrusted Notification must not change activity"
        );
    }

    #[test]
    fn set_awaiting_input_capable_is_a_noop_for_unregistered_slot() {
        let reg = LiveWorkerStateRegistry::new();
        // Must not panic when no entry exists for the slot (event/wiring
        // race ahead of spawn registration, or after release).
        reg.set_awaiting_input_capable(7, false);
        assert!(reg.get(7).is_none());
    }

    #[test]
    fn session_end_marks_terminated() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(
            1,
            &WorkerEvent::SessionEnd {
                session_id: "s".into(),
                reason: "exit".into(),
            },
        );
        let state = reg.get(1).unwrap();
        assert_eq!(state.activity, WorkerActivity::Terminated);
    }

    /// Regression test for the 2026-07-26 mass husk-retirement: a
    /// `SessionEnd` that arrives while a `PreToolUse` is still unbalanced
    /// must NOT erase the in-flight tool.
    ///
    /// That unbalanced tool is the evidence
    /// `husk_pane_sweep::live_process_evidence` uses to prove the worker is
    /// still running before an irreversible kill. Clearing it here made a
    /// worker inside a multi-minute foreground `bazel` build — which emits
    /// no further hook by definition — indistinguishable from a genuinely
    /// dead one, and five such workers were SIGTERMed mid-work.
    #[test]
    fn session_end_preserves_a_tool_still_in_flight() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 4242, None);
        reg.apply_event(1, &pre_tool("Bash"));
        reg.apply_event(
            1,
            &WorkerEvent::SessionEnd {
                session_id: "s".into(),
                reason: "other".into(),
            },
        );

        let state = reg.get(1).unwrap();
        assert_eq!(state.activity, WorkerActivity::Terminated);
        assert_eq!(
            state.current_tool.as_deref(),
            Some("Bash"),
            "an unbalanced PreToolUse must survive SessionEnd — it is the proof the process is still working",
        );
    }

    /// The normal path is unaffected: `Stop` already cleared the tool, so
    /// a `SessionEnd` after a clean turn boundary still leaves it unset.
    #[test]
    fn session_end_after_stop_leaves_no_tool_in_flight() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 4242, None);
        reg.apply_event(1, &pre_tool("Bash"));
        reg.apply_event(1, &stop_event());
        reg.apply_event(
            1,
            &WorkerEvent::SessionEnd {
                session_id: "s".into(),
                reason: "other".into(),
            },
        );

        let state = reg.get(1).unwrap();
        assert_eq!(state.activity, WorkerActivity::Terminated);
        assert!(state.current_tool.is_none());
    }

    #[test]
    fn session_start_startup_promotes_spawning_to_idle() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(
            1,
            &WorkerEvent::SessionStart {
                session_id: "s".into(),
                source: SessionStartSource::Startup,
                model: None,
            },
        );
        let state = reg.get(1).unwrap();
        assert_eq!(state.activity, WorkerActivity::Idle);
    }

    #[test]
    fn apply_event_returns_false_when_slot_not_registered() {
        let reg = LiveWorkerStateRegistry::new();
        let changed = reg.apply_event(7, &stop_event());
        assert!(!changed);
    }

    #[test]
    fn snapshot_returns_entries_sorted_by_slot() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(3, "run-3", "claude-opus-4-7", 0, None);
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 0, None);
        reg.register_spawn(2, "run-2", "claude-opus-4-7", 0, None);
        let states = reg.snapshot();
        assert_eq!(states.len(), 3);
        assert_eq!(states[0].slot_id, 1);
        assert_eq!(states[1].slot_id, 2);
        assert_eq!(states[2].slot_id, 3);
    }

    #[test]
    fn set_live_status_writes_text_and_stamps_timestamp() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        let changed = reg.set_live_status(1, Some("running tests after the layout fix".into()));
        assert!(changed);
        let state = reg.get(1).unwrap();
        assert_eq!(state.live_status.as_deref(), Some("running tests after the layout fix"),);
        assert!(state.live_status_at.is_some());
    }

    #[test]
    fn set_live_status_clears_both_fields() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.set_live_status(1, Some("doing a thing".into()));
        let changed = reg.set_live_status(1, None);
        assert!(changed);
        let state = reg.get(1).unwrap();
        assert!(state.live_status.is_none());
        assert!(state.live_status_at.is_none());
    }

    #[test]
    fn set_live_status_returns_false_when_clearing_already_empty_slot() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        let changed = reg.set_live_status(1, None);
        assert!(!changed);
    }

    #[test]
    fn set_live_status_returns_true_on_repeated_set_to_advance_timestamp() {
        // Two consecutive sets with the same text must still return
        // true so the broadcast fires — the staleness UI keys off
        // `live_status_at`, and freezing it on text equality would
        // misfire the "no summarizer activity" warning.
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        let first = reg.set_live_status(1, Some("running tests".into()));
        let second = reg.set_live_status(1, Some("running tests".into()));
        assert!(first);
        assert!(second);
    }

    #[test]
    fn set_live_status_returns_false_when_slot_unknown() {
        let reg = LiveWorkerStateRegistry::new();
        let changed = reg.set_live_status(7, Some("orphan".into()));
        assert!(!changed);
    }

    #[test]
    fn set_live_status_round_trips_through_snapshot() {
        // The snapshot is what the topic publisher serialises, so
        // confirm that a successful `set_live_status` shows up in
        // both the named getter and the snapshot list.
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(2, "run-2", "claude-opus-4-7", 0, None);
        reg.set_live_status(2, Some("editing the redactor".into()));
        let states = reg.snapshot();
        let s = states.iter().find(|s| s.slot_id == 2).unwrap();
        assert_eq!(s.live_status.as_deref(), Some("editing the redactor"));
        assert!(s.live_status_at.is_some());
    }

    #[test]
    fn release_slot_clears_live_status_pair() {
        // Releasing a slot drops the entry whole, so a subsequent
        // re-spawn into the same slot starts with `None`/`None`.
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.set_live_status(1, Some("doing a thing".into()));
        reg.release_slot(1);
        assert!(reg.get(1).is_none());
        reg.register_spawn(1, "run-2", "claude-opus-4-7", 1, None);
        let state = reg.get(1).unwrap();
        assert!(state.live_status.is_none());
        assert!(state.live_status_at.is_none());
    }

    #[test]
    fn set_recovery_status_writes_and_clears() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        let changed = reg.set_recovery_status(1, Some("recovering from API error (attempt 1/3)".into()));
        assert!(changed);
        assert_eq!(
            reg.get(1).unwrap().recovery_status.as_deref(),
            Some("recovering from API error (attempt 1/3)")
        );

        let changed = reg.set_recovery_status(1, None);
        assert!(changed);
        assert!(reg.get(1).unwrap().recovery_status.is_none());
    }

    #[test]
    fn set_recovery_status_returns_false_when_slot_unknown_or_unchanged() {
        let reg = LiveWorkerStateRegistry::new();
        assert!(!reg.set_recovery_status(7, Some("recovering".into())));

        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        assert!(reg.set_recovery_status(1, Some("recovering".into())));
        // Setting the identical value again is a no-op.
        assert!(!reg.set_recovery_status(1, Some("recovering".into())));
    }

    #[test]
    fn apply_event_clears_recovery_status_on_any_hook() {
        // Proof the worker's session is responsive again — any hook
        // event, not just one that flips activity to Working, must
        // clear a stale recovery banner so it never shadows real
        // progress or a normal idle-between-turns state.
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.set_recovery_status(1, Some("recovering from API error (attempt 1/3)".into()));
        assert!(reg.get(1).unwrap().recovery_status.is_some());

        reg.apply_event(1, &stop_event());
        assert!(
            reg.get(1).unwrap().recovery_status.is_none(),
            "recovery_status must clear on the next hook event"
        );
    }

    #[test]
    fn release_slot_clears_recovery_status() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.set_recovery_status(1, Some("recovering from API error (attempt 1/3)".into()));
        reg.release_slot(1);
        assert!(reg.get(1).is_none());
        reg.register_spawn(1, "run-2", "claude-opus-4-7", 1, None);
        assert!(reg.get(1).unwrap().recovery_status.is_none());
    }

    #[test]
    fn mark_errored_transitions_and_returns_changed() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        assert!(reg.mark_errored(1));
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Errored);
        // Idempotent.
        assert!(!reg.mark_errored(1));
    }

    #[test]
    fn run_id_for_work_item_finds_live_binding() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(
            3,
            "exec-42",
            "claude-opus-4-7",
            99,
            Some(WorkItemBinding {
                work_item_id: "chore_abc".into(),
                work_item_name: "My chore".into(),
                execution_id: "exec-42".into(),
            }),
        );
        assert_eq!(reg.run_id_for_work_item("chore_abc").as_deref(), Some("exec-42"));
        assert!(reg.run_id_for_work_item("chore_other").is_none());
    }

    #[test]
    fn run_id_for_work_item_ignores_terminal_slots() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(
            1,
            "exec-dead",
            "claude-opus-4-7",
            10,
            Some(WorkItemBinding {
                work_item_id: "chore_xyz".into(),
                work_item_name: "Terminated chore".into(),
                execution_id: "exec-dead".into(),
            }),
        );
        reg.apply_event(
            1,
            &WorkerEvent::SessionEnd {
                session_id: "s".into(),
                reason: "exit".into(),
            },
        );
        assert!(reg.run_id_for_work_item("chore_xyz").is_none());
    }

    // ── mark_stalled_spawns (initial directory-trust prompt detection) ────────

    /// Regression test for the initial-directory-trust-prompt detection path.
    ///
    /// The directory-trust prompt that Claude Code shows at session startup
    /// (for Opus / `--permission-mode auto` workers) fires *before*
    /// `SessionStart`, so no hook ever arrives and the slot stays in `Spawning`
    /// with `last_event_at = None`. `mark_stalled_spawns` must detect this and
    /// flip the slot to `WaitingForInput` so the kanban dot + indicator fire.
    #[test]
    fn stalled_spawn_with_no_events_transitions_to_waiting_for_input() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);

        // Backdate the spawn time so the threshold has elapsed.
        let old_spawn = 1_700_000_000_i64;
        reg.set_spawn_time_for_test(1, old_spawn);

        // No hooks have arrived — last_event_at is None, activity is Spawning.
        let before = reg.get(1).unwrap();
        assert_eq!(before.activity, WorkerActivity::Spawning);
        assert!(before.last_event_at.is_none());

        let now = old_spawn + STALLED_SPAWN_THRESHOLD_SECS + 1;
        let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

        assert_eq!(changed, vec![1], "slot 1 should be reported as changed");
        let after = reg.get(1).unwrap();
        assert_eq!(after.activity, WorkerActivity::WaitingForInput);
        assert!(
            after.last_event_at.is_some(),
            "last_event_at must be stamped on the stall transition"
        );
    }

    /// A worker that received at least one hook event (even just `SessionStart`)
    /// is NOT considered stalled, even if it is still in `Spawning` state
    /// (which can't happen in practice, but is a meaningful boundary).
    #[test]
    fn spawn_with_events_is_not_marked_stalled() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(2, "run-2", "claude-opus-4-7", 1, None);

        // Fire SessionStart so last_event_at gets set.
        reg.apply_event(
            2,
            &WorkerEvent::SessionStart {
                session_id: "s".into(),
                source: SessionStartSource::Startup,
                model: None,
            },
        );

        // Backdate the spawn time.
        reg.set_spawn_time_for_test(2, 1_700_000_000);

        let now = 1_700_000_000 + STALLED_SPAWN_THRESHOLD_SECS + 100;
        let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

        assert!(changed.is_empty(), "slot with events must not be flagged");
        let state = reg.get(2).unwrap();
        assert_eq!(state.activity, WorkerActivity::Idle);
    }

    /// A worker that spawned very recently is not yet considered stalled —
    /// it just needs more time to start.
    #[test]
    fn freshly_spawned_worker_not_marked_stalled() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(3, "run-3", "claude-opus-4-7", 1, None);

        // The spawn time is "now", so the threshold has not elapsed.
        let now = 1_700_000_100_i64;
        reg.set_spawn_time_for_test(3, now - STALLED_SPAWN_THRESHOLD_SECS + 5);

        let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

        assert!(changed.is_empty(), "freshly-spawned worker must not be flagged");
        assert_eq!(
            reg.get(3).unwrap().activity,
            WorkerActivity::Spawning,
            "activity must remain Spawning"
        );
    }

    /// Regression test for the 2026-07-03/04 false-live incident: a
    /// slot that never reported a shell pid must NOT be promoted to
    /// `WaitingForInput` by `mark_stalled_spawns`, no matter how long it
    /// has been stuck in `Spawning`. Promoting it there previously left
    /// the slot parked forever at `activity=waiting_for_input,
    /// shell_pid=0` — a state with nothing for a human to attach to and
    /// answer. This slot is instead left in `Spawning` for
    /// `spawn_ack_sweep` to terminal-fail and redispatch.
    #[test]
    fn zero_pid_spawn_is_not_promoted_to_waiting_for_input() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 0, None);

        let old_spawn = 1_700_000_000_i64;
        reg.set_spawn_time_for_test(1, old_spawn);

        // Far past the threshold — if this were a shell_pid > 0 slot it
        // would have been promoted long ago.
        let now = old_spawn + STALLED_SPAWN_THRESHOLD_SECS * 10;
        let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

        assert!(
            changed.is_empty(),
            "a pid-less slot must never be promoted by this path"
        );
        let state = reg.get(1).unwrap();
        assert_eq!(
            state.activity,
            WorkerActivity::Spawning,
            "must remain Spawning — spawn_ack_sweep owns the pid-less timeout path"
        );
        assert_eq!(state.shell_pid, 0);
    }

    /// Mirrors `awaiting_input_incapable_driver_never_shows_waiting_for_input`
    /// for the stalled-spawn path: a slot whose driver doesn't declare
    /// `Capability::AwaitingInputSignal` must never be promoted to
    /// `WaitingForInput` by `mark_stalled_spawns`, even after the threshold
    /// elapses with `shell_pid > 0` and no hook event — exactly the shape
    /// that promotes a capable slot. It must be left in `Spawning` instead
    /// of the fabricated state.
    #[test]
    fn awaiting_input_incapable_driver_never_marked_stalled() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.set_awaiting_input_capable(1, false);

        let old_spawn = 1_700_000_000_i64;
        reg.set_spawn_time_for_test(1, old_spawn);

        let now = old_spawn + STALLED_SPAWN_THRESHOLD_SECS + 1;
        let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

        assert!(
            changed.is_empty(),
            "an awaiting-input-incapable driver's slot must never be marked stalled"
        );
        let state = reg.get(1).unwrap();
        assert_eq!(
            state.activity,
            WorkerActivity::Spawning,
            "must remain Spawning — no lower-fidelity fallback exists yet for this driver"
        );
    }

    /// Workers in non-Spawning states are never touched by `mark_stalled_spawns`.
    #[test]
    fn non_spawning_states_not_affected_by_stall_detection() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);

        // Advance to Working via PreToolUse.
        reg.apply_event(1, &pre_tool("Bash"));

        reg.set_spawn_time_for_test(1, 1_700_000_000);

        let now = 1_700_000_000 + STALLED_SPAWN_THRESHOLD_SECS + 100;
        let changed = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);

        assert!(changed.is_empty(), "Working slot must not be flagged");
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Working);
    }

    /// `mark_stalled_spawns` is idempotent: once a slot transitions to
    /// `WaitingForInput`, it is no longer in `Spawning` and will not be
    /// transitioned again.
    #[test]
    fn mark_stalled_spawns_is_idempotent() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.set_spawn_time_for_test(1, 1_700_000_000);

        let now = 1_700_000_000 + STALLED_SPAWN_THRESHOLD_SECS + 1;
        let first = reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);
        assert_eq!(first, vec![1]);

        let second = reg.mark_stalled_spawns(now + 10, STALLED_SPAWN_THRESHOLD_SECS);
        assert!(second.is_empty(), "should not fire again after first transition");
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::WaitingForInput);
    }

    #[test]
    fn progress_fidelity_defaults_to_rich_when_never_set() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        assert_eq!(reg.progress_fidelity_for_slot(1), ProgressFidelity::Rich);
        // Even for a slot that was never registered at all.
        assert_eq!(reg.progress_fidelity_for_slot(9), ProgressFidelity::Rich);
    }

    #[test]
    fn set_progress_fidelity_round_trips() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.set_progress_fidelity(1, ProgressFidelity::Coarse);
        assert_eq!(reg.progress_fidelity_for_slot(1), ProgressFidelity::Coarse);
    }

    /// The trace is only useful if the `registered`/`cleared` pair balances
    /// per run id. A slot re-registered without an intervening
    /// `release_slot` breaks that: the prior run leaves `agents list` with
    /// no `cleared` line of its own. Pin that the displacement is greppable
    /// — both as a dedicated `warn` naming the displaced run, and as a
    /// `replaced_run_id` field on the registration line itself.
    #[test]
    fn register_spawn_traces_a_displaced_entry() {
        let buffer = crate::test_support::log_capture::install();
        let start = buffer.lock().len();

        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(7, "run-displaced-first", "claude-opus-4-7", 11, None);
        reg.register_spawn(7, "run-displaced-second", "claude-opus-4-7", 22, None);

        let captured = String::from_utf8(buffer.lock()[start..].to_vec()).expect("utf8 log capture");
        let ours: Vec<&str> = captured
            .lines()
            .filter(|line| line.contains("run-displaced-"))
            .collect();

        let warn = ours
            .iter()
            .find(|line| line.contains("displaced a live entry without a release_slot"))
            .unwrap_or_else(|| panic!("no displacement warning captured; lines: {ours:#?}"));
        assert!(warn.contains("WARN"), "displacement must be a warning: {warn}");
        assert!(
            warn.contains("run_id=run-displaced-first"),
            "the warning must name the run that lost its listing: {warn}"
        );
        assert!(
            warn.contains("replaced_by_run_id=run-displaced-second"),
            "the warning must name the run that took the slot: {warn}"
        );

        let second_registration = ours
            .iter()
            .find(|line| line.contains("run is now visible") && line.contains("run_id=run-displaced-second"))
            .unwrap_or_else(|| panic!("no registration line for the second run; lines: {ours:#?}"));
        assert!(
            second_registration.contains("replaced_run_id=\"run-displaced-first\""),
            "the registration line must carry the displaced run id: {second_registration}"
        );

        // A registration onto an empty slot must not claim a displacement.
        let first_registration = ours
            .iter()
            .find(|line| line.contains("run is now visible") && line.contains("run_id=run-displaced-first"))
            .unwrap_or_else(|| panic!("no registration line for the first run; lines: {ours:#?}"));
        assert!(
            first_registration.contains("replaced_run_id=\"-\""),
            "a fresh slot must report no displacement: {first_registration}"
        );
    }

    #[test]
    fn register_spawn_resets_fidelity_on_slot_recycle() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.set_progress_fidelity(1, ProgressFidelity::Minimal);
        assert_eq!(reg.progress_fidelity_for_slot(1), ProgressFidelity::Minimal);

        // Slot 1 is recycled for a new run — must not inherit the prior
        // occupant's declared tier.
        reg.register_spawn(1, "run-2", "claude-opus-4-7", 2, None);
        assert_eq!(reg.progress_fidelity_for_slot(1), ProgressFidelity::Rich);
    }

    #[test]
    fn release_slot_clears_progress_fidelity() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.set_progress_fidelity(1, ProgressFidelity::Coarse);
        reg.release_slot(1);
        assert_eq!(reg.progress_fidelity_for_slot(1), ProgressFidelity::Rich);
    }

    // ── SessionStart model authority + stale-activity downgrade ──────────────

    #[test]
    fn session_start_model_overwrites_launch_default() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "opus", 1, None);
        assert_eq!(reg.get(1).unwrap().model, "opus");

        reg.apply_event(
            1,
            &WorkerEvent::SessionStart {
                session_id: "s".into(),
                source: SessionStartSource::Startup,
                model: Some("claude-opus-4-7".into()),
            },
        );
        assert_eq!(
            reg.get(1).unwrap().model,
            "claude-opus-4-7",
            "SessionStart model is authoritative over the launch default",
        );
    }

    #[test]
    fn session_start_without_model_keeps_launch_default() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "opus", 1, None);
        reg.apply_event(
            1,
            &WorkerEvent::SessionStart {
                session_id: "s".into(),
                source: SessionStartSource::Startup,
                model: None,
            },
        );
        assert_eq!(
            reg.get(1).unwrap().model,
            "opus",
            "absent model must not wipe the launch default",
        );
    }

    #[test]
    fn session_start_resume_stamps_model_but_leaves_spawning() {
        // Resume is the reattach proof-of-life path: stamp model + last_event_at
        // without claiming the worker is past spawn. The stale-activity timer
        // then downgrades Spawning → Idle once last_event_at ages out.
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "opus", 1, None);
        reg.apply_event(
            1,
            &WorkerEvent::SessionStart {
                session_id: "s".into(),
                source: SessionStartSource::Resume,
                model: Some("claude-sonnet-4-6".into()),
            },
        );
        let state = reg.get(1).unwrap();
        assert_eq!(state.model, "claude-sonnet-4-6");
        assert_eq!(state.activity, WorkerActivity::Spawning);
        assert!(state.last_event_at.is_some());
    }

    #[test]
    fn stale_last_event_at_downgrades_spawning_to_idle() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        // Resume leaves Spawning while stamping last_event_at.
        reg.apply_event(
            1,
            &WorkerEvent::SessionStart {
                session_id: "s".into(),
                source: SessionStartSource::Resume,
                model: None,
            },
        );
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Spawning);

        // Age last_event_at past the downgrade threshold.
        let now = 1_700_000_100_i64;
        let stale_at = iso8601_utc(now - STALE_ACTIVITY_DOWNGRADE_SECS - 1);
        reg.set_last_event_at_for_test(1, stale_at);

        let changed = reg.downgrade_stale_activity(now, STALE_ACTIVITY_DOWNGRADE_SECS);
        assert_eq!(changed, vec![1]);
        assert_eq!(
            reg.get(1).unwrap().activity,
            WorkerActivity::Idle,
            "stale last_event_at while Spawning must not keep advertising spawning",
        );
    }

    #[test]
    fn recent_last_event_at_does_not_downgrade_spawning() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(
            1,
            &WorkerEvent::SessionStart {
                session_id: "s".into(),
                source: SessionStartSource::Resume,
                model: None,
            },
        );
        let now = 1_700_000_100_i64;
        reg.set_last_event_at_for_test(1, iso8601_utc(now - 5));

        let changed = reg.downgrade_stale_activity(now, STALE_ACTIVITY_DOWNGRADE_SECS);
        assert!(changed.is_empty());
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Spawning);
    }

    #[test]
    fn downgrade_stale_activity_ignores_slots_with_no_events() {
        // No last_event_at → mark_stalled_spawns / spawn_ack_sweep, not us.
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        let now = 1_700_000_100_i64;
        let changed = reg.downgrade_stale_activity(now, STALE_ACTIVITY_DOWNGRADE_SECS);
        assert!(changed.is_empty());
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Spawning);
    }

    #[test]
    fn downgrade_stale_activity_ignores_non_spawning() {
        let reg = LiveWorkerStateRegistry::new();
        reg.register_spawn(1, "run-1", "claude-opus-4-7", 1, None);
        reg.apply_event(1, &pre_tool("Bash"));
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Working);

        let now = 1_700_000_100_i64;
        reg.set_last_event_at_for_test(1, iso8601_utc(now - STALE_ACTIVITY_DOWNGRADE_SECS - 60));
        let changed = reg.downgrade_stale_activity(now, STALE_ACTIVITY_DOWNGRADE_SECS);
        assert!(changed.is_empty());
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Working);
    }

    // ─── driver-start verification ──────────────────────────────────────────

    /// Register a slot aged past `DRIVER_START_GRACE_SECS`, with a live
    /// foreground shell pid — the 2026-07-30 shape.
    fn aged_slot_with_live_shell(reg: &LiveWorkerStateRegistry, slot: u8, run: &str, awaiting_input_capable: bool) {
        reg.register_spawn_with_capabilities(
            slot,
            run,
            "grok-4.6",
            92697,
            None,
            awaiting_input_capable,
            LiveSpawnRouting::none(),
        );
        reg.set_spawn_time_for_test(
            slot,
            boss_engine_utils::epoch_time::now_epoch_secs() - (DRIVER_START_GRACE_SECS + 60),
        );
    }

    #[test]
    fn unverified_driver_starts_reports_a_pane_with_a_live_shell_and_no_driver() {
        let reg = LiveWorkerStateRegistry::new();
        aged_slot_with_live_shell(&reg, 1, "run-a", false);

        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let found = reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS);

        assert_eq!(found.len(), 1, "a live shell pid must not exempt the slot");
        assert_eq!(found[0].slot_id, 1);
        assert_eq!(found[0].run_id, "run-a");
        assert_eq!(found[0].shell_pid, 92697);
        assert_eq!(found[0].activity, WorkerActivity::Spawning);
        assert!(found[0].silent_secs >= DRIVER_START_GRACE_SECS);
    }

    #[test]
    fn a_driver_signal_removes_the_slot_from_the_unverified_set() {
        let reg = LiveWorkerStateRegistry::new();
        aged_slot_with_live_shell(&reg, 1, "run-a", false);
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        assert_eq!(reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS).len(), 1);

        assert_eq!(reg.record_driver_signal("run-a", DriverSignalKind::HookEvent), Some(1));

        assert!(
            reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS).is_empty(),
            "a driver-originated signal is proof the driver started",
        );
    }

    #[test]
    fn either_driver_signal_kind_counts_as_proof() {
        for kind in [DriverSignalKind::HookEvent, DriverSignalKind::TranscriptPath] {
            let reg = LiveWorkerStateRegistry::new();
            aged_slot_with_live_shell(&reg, 1, "run-a", false);
            assert_eq!(reg.record_driver_signal("run-a", kind), Some(1));
            let now = boss_engine_utils::epoch_time::now_epoch_secs();
            assert!(
                reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS).is_empty(),
                "{kind:?} must count as driver-start proof",
            );
        }
    }

    /// The signal is first-write-wins: it answers "did the driver ever
    /// start?", not "when was it last alive". Later signals must not move it.
    #[test]
    fn record_driver_signal_keeps_the_first_timestamp() {
        let reg = LiveWorkerStateRegistry::new();
        aged_slot_with_live_shell(&reg, 1, "run-a", false);
        reg.record_driver_signal("run-a", DriverSignalKind::HookEvent);
        let first = reg.driver_signal_at(1).unwrap();
        reg.record_driver_signal("run-a", DriverSignalKind::TranscriptPath);
        assert_eq!(reg.driver_signal_at(1), Some(first));
    }

    /// Register a re-adopted slot aged past `DRIVER_START_GRACE_SECS` —
    /// the shape re-adoption always produces, because registration stamps
    /// `spawned_at` with the current time for a process that has in fact
    /// been running for however long.
    fn aged_readopted_slot(reg: &LiveWorkerStateRegistry, slot: u8, run: &str, evidence: ReadoptionEvidence) {
        reg.register_readoption(
            slot,
            run,
            "grok-4.6",
            92697,
            None,
            false,
            LiveSpawnRouting::none(),
            evidence,
        );
        reg.set_spawn_time_for_test(
            slot,
            boss_engine_utils::epoch_time::now_epoch_secs() - (DRIVER_START_GRACE_SECS + 60),
        );
    }

    /// A worker re-adopted on a live shell pid alone has no driver-start
    /// proof and never will — a worker parked at `waiting_human` emits no
    /// further hook by definition. Aging its re-registration would reap a
    /// live worker mid-work, which is the incident re-adoption exists to
    /// prevent.
    #[test]
    fn a_readopted_slot_is_never_reported_as_a_never_started_driver() {
        let reg = LiveWorkerStateRegistry::new();
        aged_readopted_slot(&reg, 1, "run-a", ReadoptionEvidence::LiveShellPid);

        assert_eq!(reg.driver_start_expectation(1), Some(DriverStartExpectation::Readopted));
        assert!(
            reg.driver_signal_at(1).is_none(),
            "a live shell pid is not driver-start proof and must not be recorded as any",
        );

        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        assert!(
            reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS).is_empty(),
            "driver-start verification does not apply to a registration that spawned nothing",
        );
    }

    /// A hook arriving after the engine terminalized the run came from the
    /// driver itself, so re-adoption records it rather than discarding it.
    #[test]
    fn a_hook_triggered_readoption_records_the_driver_signal() {
        let reg = LiveWorkerStateRegistry::new();
        aged_readopted_slot(&reg, 1, "run-a", ReadoptionEvidence::DriverHook);

        assert!(
            reg.driver_signal_at(1).is_some(),
            "the hook that triggered the re-adoption IS driver-originated proof",
        );
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        assert!(reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS).is_empty());
    }

    /// The exemption belongs to the registration, not the slot: recycling
    /// the slot for a genuine spawn must restore verification.
    #[test]
    fn recycling_a_readopted_slot_for_a_real_spawn_restores_verification() {
        let reg = LiveWorkerStateRegistry::new();
        aged_readopted_slot(&reg, 1, "run-a", ReadoptionEvidence::LiveShellPid);

        aged_slot_with_live_shell(&reg, 1, "run-b", false);

        assert_eq!(
            reg.driver_start_expectation(1),
            Some(DriverStartExpectation::EngineSpawned)
        );
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let found = reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].run_id, "run-b");
    }

    #[test]
    fn record_driver_signal_is_a_no_op_for_an_unknown_run() {
        let reg = LiveWorkerStateRegistry::new();
        aged_slot_with_live_shell(&reg, 1, "run-a", false);
        assert_eq!(reg.record_driver_signal("run-other", DriverSignalKind::HookEvent), None);
        assert!(reg.driver_signal_at(1).is_none());
    }

    /// The reconciliation that closes the grok hole: `mark_stalled_spawns`
    /// declines to promote a driver without `Capability::AwaitingInputSignal`,
    /// and that exemption must not carry over to driver-start verification.
    #[test]
    fn mark_stalled_spawns_capability_exemption_does_not_extend_to_driver_start() {
        let reg = LiveWorkerStateRegistry::new();
        aged_slot_with_live_shell(&reg, 1, "run-a", false);

        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        assert!(
            reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS).is_empty(),
            "precondition: the capability-less driver is exempt from promotion",
        );
        assert_eq!(reg.get(1).unwrap().activity, WorkerActivity::Spawning);

        assert_eq!(
            reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS).len(),
            1,
            "driver-start verification must cover the driver mark_stalled_spawns skips",
        );
    }

    /// The promotion path's synthesized `last_event_at` must not be mistaken
    /// for driver evidence, and leaving `Spawning` must not hide the slot.
    #[test]
    fn mark_stalled_spawns_promotion_is_not_driver_evidence() {
        let reg = LiveWorkerStateRegistry::new();
        aged_slot_with_live_shell(&reg, 1, "run-a", true);

        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        assert_eq!(reg.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS), vec![1]);
        let state = reg.get(1).unwrap();
        assert_eq!(state.activity, WorkerActivity::WaitingForInput);
        assert!(state.last_event_at.is_some());

        assert!(
            reg.driver_signal_at(1).is_none(),
            "an engine-synthesized timestamp is not a driver signal",
        );
        let found = reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS);
        assert_eq!(found.len(), 1, "a promoted slot stays subject to verification");
        assert_eq!(found[0].activity, WorkerActivity::WaitingForInput);
    }

    #[test]
    fn unverified_driver_starts_respects_the_grace_window() {
        let reg = LiveWorkerStateRegistry::new();
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        reg.register_spawn(1, "run-a", "grok-4.6", 92697, None);
        reg.set_spawn_time_for_test(1, now - 5);

        assert!(
            reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS).is_empty(),
            "a fresh spawn must be given its window before being judged",
        );
    }

    /// A recycled slot must not inherit the previous occupant's driver-start
    /// proof — otherwise a healthy prior run would vouch for a new run whose
    /// driver never exec'd.
    #[test]
    fn re_registering_a_slot_clears_the_prior_driver_signal() {
        let reg = LiveWorkerStateRegistry::new();
        aged_slot_with_live_shell(&reg, 1, "run-a", false);
        reg.record_driver_signal("run-a", DriverSignalKind::HookEvent);
        assert!(reg.driver_signal_at(1).is_some());

        aged_slot_with_live_shell(&reg, 1, "run-b", false);

        assert!(reg.driver_signal_at(1).is_none());
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let found = reg.unverified_driver_starts(now, DRIVER_START_GRACE_SECS);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].run_id, "run-b");
    }

    #[test]
    fn releasing_a_slot_clears_its_driver_signal() {
        let reg = LiveWorkerStateRegistry::new();
        aged_slot_with_live_shell(&reg, 1, "run-a", false);
        reg.record_driver_signal("run-a", DriverSignalKind::HookEvent);
        reg.release_slot(1);
        assert!(reg.driver_signal_at(1).is_none());
    }

    /// A real hook through the normal `apply_event` path does NOT by itself
    /// stamp the driver signal — the hook ingress records it explicitly. This
    /// pins that the two are separate concerns so a future refactor of
    /// `apply_event` cannot silently start (or stop) vouching for a driver.
    #[test]
    fn apply_event_alone_does_not_stamp_the_driver_signal() {
        let reg = LiveWorkerStateRegistry::new();
        aged_slot_with_live_shell(&reg, 1, "run-a", false);
        reg.apply_event(1, &pre_tool("Bash"));
        assert!(reg.get(1).unwrap().last_event_at.is_some());
        assert!(reg.driver_signal_at(1).is_none());
    }
}
