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
use crate::semantic_progress::{SemanticProgressCheckpoint, SemanticToolCondition, next_tool_condition};

mod never_started_reap;
pub use never_started_reap::NeverStartedReapCommit;

/// Attributed worker-pool label for a live run (`"main"`, `"automation"`,
/// or `"review"`). Matches
/// [`crate::coordinator::ExecutionCoordinator::attributed_pool_label`]:
/// PR reviews and review guides always report `"review"`, automation triage and other
/// automation-sourced work report `"automation"`, everything else
/// reports `"main"`. Independent of which physical slot the run
/// occupies (automation can spill into a main-pool Lower Decks slot).
///
/// Used at spawn registration to stamp [`LiveWorkerState::pool`] so
/// `bossctl agents list` can render pool without joining the execution
/// table or re-deriving attribution.
pub fn attributed_pool_label(kind: ExecutionKind, has_source_automation: bool) -> &'static str {
    match kind {
        ExecutionKind::PrReview | ExecutionKind::PrReviewGuide => "review",
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
    /// `Some(true)` for a local worker hosted in a tmux session, stamped
    /// at spawn (or read from the durable run row on re-adoption).
    /// `Some(false)` marks a local run whose durable tmux stamp is missing —
    /// an invariant failure, never a supported hosting mode. `None` where
    /// tmux hosting isn't meaningful — e.g. remote workers, which have no
    /// local pane.
    pub tmux_hosted: Option<bool>,
}

impl LiveSpawnRouting {
    /// All fields unset — the historical test/default shape.
    pub fn none() -> Self {
        Self::default()
    }

    /// Stamp pool + kind for a production dispatch that has no local tmux
    /// worker — e.g. the remote-worker registration path, which has no
    /// local pane at all. Use [`Self::new_with_hosting`] for a local spawn.
    pub fn new(pool: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            pool: Some(pool.into()),
            kind: Some(kind.into()),
            tmux_hosted: None,
        }
    }

    /// Stamp pool, kind, and the tmux-hosting stamp for a local production
    /// dispatch (`start_worker`, or re-adoption from the durable run row).
    pub fn new_with_hosting(pool: Option<String>, kind: impl Into<String>, tmux_hosted: bool) -> Self {
        Self {
            pool,
            kind: Some(kind.into()),
            tmux_hosted: Some(tmux_hosted),
        }
    }
}

/// The model identifier the engine uses when no `SessionStart` hook
/// has yet reported one — this is the model the launcher *asked* for,
/// surfaced so the UI can render the real model name immediately
/// instead of "Claude Unknown".
pub const DEFAULT_LAUNCH_MODEL: &str = "opus";

/// How long a slot must be stuck in `Spawning` with no hook events before
/// [`LiveWorkerStateRegistry::mark_stalled_spawns`] will promote it —
/// to `WaitingForInput` when the driver declared
/// `Capability::AwaitingInputSignal`, or to `Idle` when it did not but
/// `driver_signal_at` is already set. 30 seconds matches the dead-PID
/// grace period and gives a fresh-but-slow worker enough runway while
/// being well below the typical interactive-wait tolerance.
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
/// [`STALLED_SPAWN_THRESHOLD_SECS`] answers "has this spawn been sitting
/// in `Spawning` long enough to promote?". This one answers the strictly
/// stronger question "did the *driver binary* come up?" — the question no
/// check in Boss asked before, and the one the 2026-07-30 incident turned
/// on: a pane hosting nothing but an idle login shell reported
/// `shell_pid=92697` and satisfied every pane-level check forever.
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
    // Serialize lifecycle mutations independently of dispatch registry reads.
    lifecycle: Mutex<()>,
    /// Driver-hook evidence for runs whose pane is launching but whose slot
    /// entry is not registered yet, keyed by run id. Armed by
    /// [`Self::arm_pending_hooks`] before the CLI starts and drained into
    /// the slot's [`SlotMeta`] atomically by registration, so a hook that
    /// wins the race against registration is not lost. Always locked
    /// *after* `inner` when both are held.
    pending_hooks: Mutex<HashMap<String, PendingHooks>>,
    #[cfg(test)]
    pub(crate) test_hooks: TestHooks,
    persona_store: Option<std::sync::Arc<crate::work::WorkDb>>,
}

/// Test-only synchronization points for lifecycle/persona race tests.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestHooks {
    pub(crate) persona_boundary: Mutex<Option<Box<dyn Fn() + Send>>>,
    pub(crate) lifecycle_waiter: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

/// Hook evidence buffered for a run before its slot entry exists.
#[derive(Default)]
struct PendingHooks {
    driver_signal_at: Option<i64>,
    /// First driver hook event kind seen before the slot was registered.
    /// Buffered here while the run is armed, then transferred into
    /// `SlotMeta::first_hook_event` (and removed from this buffer) at
    /// registration. First-write-wins; diagnostic-only.
    first_hook_event: Option<String>,
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
/// hold (`bon` defaults `Option` fields to `None` on its own), so the
/// construction site states exactly what it knows and nothing else.
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
    /// synthesizes one when it promotes a slot out of `Spawning`, and
    /// [`LiveWorkerStateRegistry::mark_errored`] stamps one on an
    /// engine-side verdict. Treating it as proof of driver start would
    /// let the engine's own guesses vouch for a driver that never ran.
    /// This field is written by [`LiveWorkerStateRegistry::record_driver_signal`]
    /// from two call sites in the hook ingress — a real worker hook, and
    /// receipt of a `transcript_path` — and from the AgentJsonlFile ingress
    /// (`WorkerEventSink::record_driver_attach`: discovery-time file
    /// progress or attachment). There is also a non-hook-originated
    /// writer: [`crate::spawn_ack_sweep::reap_never_started_spawn`]'s
    /// liveness veto, which records a transcript found on disk as
    /// [`DriverSignalKind::CorrelatedTranscript`] or
    /// [`DriverSignalKind::CorrelatedTranscriptUnattachable`] before
    /// refusing a vetoable reap. That write is still driver-originated in
    /// the sense this field exists to protect — only the driver itself can
    /// have produced the transcript the veto found — it just arrives via a
    /// filesystem probe instead of the hook socket. It is additionally
    /// restored (not fabricated) by
    /// [`LiveWorkerStateRegistry::seed_semantic_progress`] from a durable
    /// checkpoint on re-adoption, carrying forward proof this same field
    /// already held before an engine restart. Every writer is either a
    /// direct driver-originated signal or a restoration of one, so "has
    /// this driver started?" still has a single, unforgeable answer.
    ///
    /// Note what is deliberately absent: `shell_pid`. A reported
    /// tmux pane pid (`#{pane_pid}`) identifies the pane process, which
    /// may still be the shell if the driver was never exec'd. It is zero
    /// for remote workers or before registration. Every check that treated a positive
    /// pid as evidence of a working worker is what the 2026-07-30
    /// incident walked through untouched.
    driver_signal_at: Option<i64>,
    /// Event kind of the first driver hook received for this registration's
    /// run. Diagnostic-only (it appears in spawn-confirmation failure logs):
    /// first-write-wins, keyed by `run_id`, and reset whenever the slot is
    /// re-registered. It is independent of `driver_signal_at` and plays no
    /// part in reap fencing — never use it as proof the driver started.
    first_hook_event: Option<String>,
    /// Whether this registration is a newly spawned pane or an adopted
    /// existing worker. [`DriverStartExpectation::Readopted`] still subjects
    /// the slot to the driver-start timeout
    /// ([`LiveWorkerStateRegistry::unverified_driver_starts`]); it changes
    /// the pid-less tmux-invariant diagnostic and starts a fresh grace
    /// window from this registration. See [`DriverStartExpectation`].
    #[builder(default = DriverStartExpectation::EngineSpawned)]
    driver_start_expectation: DriverStartExpectation,
    /// Last **driver-originated** progress time, stamped only by
    /// [`LiveWorkerStateRegistry::apply_event`] (and restored from the
    /// durable checkpoint on tmux re-adoption). Never written by
    /// [`Self::mark_stalled_spawns`] or [`Self::mark_errored`].
    semantic_progress_at: Option<String>,
    /// Tri-state tool condition established by driver events. Defaults to
    /// [`SemanticToolCondition::Unknown`]; unknown is never coerced to idle.
    #[builder(default = SemanticToolCondition::Unknown)]
    semantic_tool_condition: SemanticToolCondition,
    /// Consecutive [`crate::transcript_liveness::TranscriptLiveness::Undeterminable`]
    /// reap outcomes recorded for this slot's current run by
    /// [`crate::spawn_ack_sweep::reap_never_started_spawn`]. Reset only by a
    /// fresh registration (a new spawn or readoption creates a new
    /// [`SlotEntry`] from scratch) — there is deliberately no "clear on a
    /// confirmed Absent/Present" path, since either of those returns before
    /// this counter is ever consulted.
    #[builder(default)]
    undeterminable_liveness_passes: u32,
    /// Whether [`crate::spawn_ack_sweep::raise_liveness_undeterminable_attention`]
    /// has already fired for this slot's current run. Guards against raising
    /// the same attention item again on every subsequent pass once the
    /// threshold has been crossed.
    #[builder(default = false)]
    undeterminable_attention_raised: bool,
    /// Set atomically with [`LiveWorkerStateRegistry::confirm_never_started_reap`]
    /// when a never-started reap commits. [`LiveWorkerStateRegistry::record_driver_signal`]
    /// refuses to accept new evidence once this is set, so a hook that loses
    /// the mutex to the reap is not recorded as proof that would contradict
    /// an orphan already in flight.
    #[builder(default = false)]
    reap_committed: bool,
}

/// Whether the engine created this slot's current registration.
///
/// Driver-start verification asks whether the current registration ever
/// produced a driver signal. A re-adoption registers a worker that was
/// already running before this engine process began tracking it, so
/// `spawned_at` is the moment the engine noticed, not the moment anything
/// exec'd — a fresh grace window, not an exemption from the timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverStartExpectation {
    /// The engine launched a driver for this registration and is owed
    /// proof it came up. The normal spawn path.
    EngineSpawned,
    /// The registration re-adopted an already-running worker. The
    /// driver-start timeout still applies after a fresh grace window from
    /// this registration; a shell-only re-adoption with no driver signal
    /// must time out. What this mark changes is the pid-less tmux-invariant
    /// diagnostic (a missing pane pid is legal for a worker this engine
    /// process did not launch) and that grace window's start. Driver-start
    /// verification still requires a driver-originated signal; a live login
    /// shell alone is not proof that the driver ever ran.
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
    /// probing the tmux `#{pane_pid}` the engine recorded at session creation). That is evidence
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
    /// A transcript for the run was found on disk by the reaper's own
    /// liveness probe ([`crate::transcript_liveness`]) — a rollout newer
    /// than the pre-spawn baseline under the run's progress-ingress root
    /// that discovery's own correlation rules would also attach, or the
    /// recorded transcript path itself. Only the driver writes either, so
    /// this is driver-start proof even when no ingress ever delivered an
    /// event for the run (2026-09-13: four live Codex workers reaped
    /// because discovery never attached rollouts that existed).
    CorrelatedTranscript,
    /// Same proof as [`Self::CorrelatedTranscript`] — a rollout the
    /// liveness probe found on disk — but one discovery's own correlation
    /// rules would NOT attach to the run (a `cwd` mismatch, an oversized
    /// `session_meta`, etc.). Still driver-originated evidence the driver
    /// ran, but distinguished from a clean attachment: an operator reading
    /// this signal kind knows discovery has an unrelated bug worth fixing,
    /// where `CorrelatedTranscript` implies discovery would have worked.
    CorrelatedTranscriptUnattachable,
}

impl DriverSignalKind {
    /// Stable, greppable label for logs and dispatch-event details.
    pub fn as_str(self) -> &'static str {
        match self {
            DriverSignalKind::HookEvent => "hook_event",
            DriverSignalKind::TranscriptPath => "transcript_path",
            DriverSignalKind::CorrelatedTranscript => "correlated_transcript",
            DriverSignalKind::CorrelatedTranscriptUnattachable => "correlated_transcript_unattachable",
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
    /// The tmux `#{pane_pid}` the engine read at session creation, if any. Carried purely so the
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
    /// Use durable persona leases for every production registration/release path.
    pub fn with_work_db(work_db: std::sync::Arc<crate::work::WorkDb>) -> Self {
        Self {
            persona_store: Some(work_db),
            ..Self::default()
        }
    }

    pub(crate) fn release_persona_for_run(&self, run_id: &str) {
        let _lifecycle = self.lock_lifecycle();
        let registered = self
            .inner
            .lock()
            .expect("registry mutex poisoned")
            .values()
            .any(|entry| entry.state.run_id == run_id);
        if !registered {
            self.release_persona_locked(run_id);
        }
    }

    fn release_persona_locked(&self, run_id: &str) {
        #[cfg(test)]
        self.notify_persona_boundary();
        if let Some(db) = &self.persona_store
            && let Err(error) = db.release_persona(run_id)
        {
            tracing::error!(run_id, %error, "could not release durable persona lease");
        }
    }
    fn lock_lifecycle(&self) -> std::sync::MutexGuard<'_, ()> {
        #[cfg(test)]
        if matches!(self.lifecycle.try_lock(), Err(std::sync::TryLockError::WouldBlock))
            && let Some(waiter) = self.test_hooks.lifecycle_waiter.lock().unwrap().take()
        {
            let _ = waiter.send(());
        }
        self.lifecycle.lock().expect("lifecycle mutex poisoned")
    }

    #[cfg(test)]
    fn notify_persona_boundary(&self) {
        let hook = self.test_hooks.persona_boundary.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }

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
        let _lifecycle = self.lock_lifecycle();
        self.register_spawn_locked(
            slot_id,
            run_id,
            model,
            shell_pid,
            binding,
            awaiting_input_capable,
            routing,
        );
    }

    #[allow(clippy::too_many_arguments)]
    #[track_caller]
    fn register_spawn_locked(
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
        let mut state = LiveWorkerState::new_spawning_with_routing_and_hosting(
            slot_id,
            run_id,
            model,
            shell_pid,
            binding,
            routing.pool,
            routing.kind,
            routing.tmux_hosted,
        );
        // Persona DB work stays outside the registry lock: dispatch holds the
        // DB connection while calling `is_run_live`, so the reverse order
        // could deadlock.
        #[cfg(test)]
        self.notify_persona_boundary();
        if let Some(db) = &self.persona_store {
            match db.lease_persona_for_execution(&state.run_id) {
                Ok(name) => state.name = name,
                Err(error) => {
                    tracing::warn!(run_id = %state.run_id, %error, "could not load persona; using execution identity")
                }
            }
        }
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
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
        let mut meta = SlotMeta::builder()
            .spawned_at(boss_engine_utils::epoch_time::now_epoch_secs())
            .awaiting_input_capable(awaiting_input_capable)
            .build();
        // Hand over hooks that arrived between CLI launch and now, under the
        // same `inner` lock as the insert so a concurrent hook either lands
        // in the pending buffer before this drain or sees the new entry.
        let pending = self
            .pending_hooks
            .lock()
            .expect("pending hooks mutex poisoned")
            .remove(&run_id);
        if let Some(pending) = pending {
            if pending.driver_signal_at.is_some() {
                tracing::info!(
                    slot_id,
                    run_id = %run_id,
                    "driver-start verified: hook received before live-state registration",
                );
            }
            meta.driver_signal_at = pending.driver_signal_at;
            meta.first_hook_event = pending.first_hook_event;
        }
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

        // A slot re-registered without an intervening `release_slot` is an
        // engine bookkeeping desync (and what `EngineToAppError::SlotBusy`
        // reports when the app's viewer disagrees). The prior
        // run silently disappears from `agents list` here, so without this
        // line the trace would carry two `registered` events and one
        // `cleared` for the same slot, and diffing the pair for the
        // displaced run id would give the wrong answer.
        if let Some(prior) = displaced {
            // The displaced run's lease would otherwise stay active forever
            // (a same-run re-registration keeps its lease).
            if prior.run_id != run_id {
                self.release_persona_locked(&prior.run_id);
            }
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
    /// plus the three things re-adoption must not get wrong:
    ///
    /// 1. The entry is marked [`DriverStartExpectation::Readopted`], so
    ///    the pid-less tmux-invariant diagnostic does not treat a missing
    ///    pane pid as illegal, and `spawned_at` starts a fresh driver-start
    ///    grace window. The driver-start timeout still applies: a
    ///    shell-only re-adoption must time out unless a real driver signal
    ///    was observed.
    /// 2. When the re-adoption was triggered by a worker hook
    ///    ([`ReadoptionEvidence::DriverHook`]) the driver signal is
    ///    recorded, because that hook *is* driver-originated proof and
    ///    discarding real evidence is never the safe default. A pid-only
    ///    trigger ([`ReadoptionEvidence::LiveShellPid`]) records nothing:
    ///    a live shell says nothing about the driver.
    /// 3. Re-adopting the slot's current run is a reconciliation observation,
    ///    not registration: it retains every [`LiveWorkerState`] field and
    ///    `spawned_at`, while (re)asserting the readopted driver-start
    ///    expectation. Full registration below applies only when the slot's
    ///    occupant genuinely changes. A positive observed shell pid repairs a
    ///    provisional or stale stored pid; a zero observation never clobbers a
    ///    known pid.
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
        let _lifecycle = self.lock_lifecycle();
        let run_id = run_id.into();
        // A prior DB failure may have left a placeholder name: look the persona
        // up outside the registry lock, then revalidate before patching.
        let needs_name_repair = self.persona_store.is_some()
            && self
                .inner
                .lock()
                .expect("registry mutex poisoned")
                .get(&slot_id)
                .is_some_and(|entry| {
                    entry.state.run_id == run_id && entry.state.name == boss_protocol::placeholder_worker_name(&run_id)
                });
        let repaired_name = match (needs_name_repair, &self.persona_store) {
            (true, Some(db)) => match db.lease_persona_for_execution(&run_id) {
                Ok(name) => Some(name),
                Err(error) => {
                    tracing::warn!(slot_id, run_id = %run_id, %error, "re-adoption could not repair placeholder persona name");
                    None
                }
            },
            _ => None,
        };
        let (retained_existing_state, repaired_shell_pid) = {
            let mut guard = self.inner.lock().expect("registry mutex poisoned");
            match guard.get_mut(&slot_id) {
                Some(entry) if entry.state.run_id == run_id => {
                    if let Some(name) = repaired_name
                        && entry.state.name == boss_protocol::placeholder_worker_name(&run_id)
                    {
                        entry.state.name = name;
                    }
                    // The worker already owns this slot. Re-adoption is a
                    // reconciliation observation, not a new spawn, so retain
                    // every live-state field (including an operator hold) rather
                    // than replacing it with the freshly-spawned fiction.
                    entry.meta.driver_start_expectation = DriverStartExpectation::Readopted;
                    let repaired_shell_pid = (shell_pid > 0 && shell_pid != entry.state.shell_pid).then(|| {
                        let previous_shell_pid = entry.state.shell_pid;
                        entry.state.shell_pid = shell_pid;
                        previous_shell_pid
                    });
                    (true, repaired_shell_pid)
                }
                _ => (false, None),
            }
        };
        if retained_existing_state {
            if evidence == ReadoptionEvidence::DriverHook {
                self.record_driver_signal(&run_id, DriverSignalKind::HookEvent);
            }
            if let Some(previous_shell_pid) = repaired_shell_pid {
                tracing::info!(
                    slot_id,
                    run_id = %run_id,
                    previous_shell_pid,
                    shell_pid,
                    "live-state registry: re-adoption repaired the retained shell pid",
                );
            }
            tracing::info!(
                slot_id,
                run_id = %run_id,
                evidence = ?evidence,
                "live-state registry: re-adoption found the same run already tracked; retained its live state",
            );
            return;
        }
        self.register_spawn_locked(
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
             driver-start verification still applies and starts a fresh grace window",
        );
    }

    /// Restore a durable semantic-progress checkpoint onto a freshly
    /// re-adopted slot.
    ///
    /// No-ops when the slot already has a display `last_event_at`: that means
    /// this process has observed the worker (or retained its live state), so
    /// the in-memory fields are newer than the checkpoint. Never writes
    /// `last_event_at` — that field is also stamped by engine inference, and
    /// seeding it would let `downgrade_stale_activity` coerce unknown
    /// (`Spawning`) to idle once the restored stamp ages. The checkpoint is
    /// itself durable proof of a driver-originated event, so it also restores
    /// `driver_signal_at` — carrying the driver-start proof across an engine
    /// restart without treating shell liveness as proof.
    pub fn seed_semantic_progress(&self, slot_id: u8, checkpoint: &SemanticProgressCheckpoint) {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let Some(entry) = guard.get_mut(&slot_id) else {
            return;
        };
        if entry.meta.driver_signal_at.is_none() {
            // `progress_at` is written only at worker-event ingress. Prefer
            // its original timestamp, but retain the proof even if a legacy
            // row carries an unparsable timestamp.
            entry.meta.driver_signal_at = Some(
                boss_engine_utils::iso8601::parse_iso8601_to_epoch(&checkpoint.progress_at)
                    .unwrap_or_else(boss_engine_utils::epoch_time::now_epoch_secs),
            );
        }
        if entry.state.last_event_at.is_some() {
            return;
        }
        entry.meta.semantic_progress_at = Some(checkpoint.progress_at.clone());
        entry.meta.semantic_tool_condition = checkpoint.tool_condition;
        match checkpoint.tool_condition {
            SemanticToolCondition::InFlight => {
                entry.state.activity = WorkerActivity::Working;
            }
            SemanticToolCondition::Idle => {
                entry.state.activity = WorkerActivity::Idle;
            }
            SemanticToolCondition::Unknown => {
                // Leave Spawning. Unknown must never be coerced to idle.
            }
        }
    }

    /// The durable semantic-progress checkpoint currently held for `slot_id`,
    /// or `None` if no driver-originated event has been recorded (and none
    /// was restored on re-adoption).
    pub fn semantic_progress_for_slot(&self, slot_id: u8) -> Option<SemanticProgressCheckpoint> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        guard.get(&slot_id).and_then(|entry| {
            Some(SemanticProgressCheckpoint {
                progress_at: entry.meta.semantic_progress_at.clone()?,
                tool_condition: entry.meta.semantic_tool_condition,
            })
        })
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
        let _lifecycle = self.lock_lifecycle();
        let caller = std::panic::Location::caller();
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        // One `remove` drops the slot's entire footprint — wire state and
        // every piece of engine bookkeeping — so no field can outlive the
        // occupant it describes.
        let removed = guard.remove(&slot_id).map(|entry| entry.state);
        drop(guard);
        if let Some(state) = &removed {
            self.release_persona_locked(&state.run_id);
        }

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
    /// `TransientRecoveryReaper::reap_worker` after `release_worker_pane`
    /// found no run→slot mapping (its `NoLiveWorker` and untracked-viewer
    /// arms skip [`Self::release_slot`]). Left alone, that shape strands both the
    /// pool claim and this live-state entry: an entry still backing the
    /// claim is exactly what `pool_claim_sweep` skips by design, so
    /// nothing else ever reconciles it. Dropping the entry here clears
    /// that gate.
    #[track_caller]
    pub fn release_slot_for_run(&self, run_id: &str) -> Option<u8> {
        let _lifecycle = self.lock_lifecycle();
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let slot_id = guard.values().find(|entry| entry.state.run_id == run_id)?.state.slot_id;
        guard.remove(&slot_id);
        drop(guard);
        self.release_persona_locked(run_id);
        tracing::info!(slot_id, run_id, cleared_by = %std::panic::Location::caller(), "live-state registry: matching run entry cleared");
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

    /// Test-only pid seeder. Production stamps `shell_pid` at registration
    /// via [`Self::register_spawn_with_capabilities`]; this remains for
    /// tests that seed or mutate a pid without the full spawn flow.
    /// Returns the slot id if the entry was found and updated, or `None`
    /// if no live slot matches.
    #[cfg(test)]
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
    /// This is the writer of `driver_signal_at` for a live-observed
    /// signal — [`LiveWorkerStateRegistry::seed_semantic_progress`] is the
    /// only other writer, and it only restores a value this method (or a
    /// prior engine process's call to it) already established, so this
    /// remains the sole source of a *new* "has this driver started?"
    /// answer. It is deliberately keyed by `run_id` rather than slot: the hook ingress
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
        let Some(entry) = guard.values_mut().find(|entry| entry.state.run_id == run_id) else {
            // No entry yet: buffer the proof if registration is pending for
            // this run (see `arm_pending_hooks`), else a benign no-op.
            // `guard` stays held so registration cannot interleave.
            if let Some(pending) = self
                .pending_hooks
                .lock()
                .expect("pending hooks mutex poisoned")
                .get_mut(run_id)
            {
                pending
                    .driver_signal_at
                    .get_or_insert_with(boss_engine_utils::epoch_time::now_epoch_secs);
            }
            return None;
        };
        let slot_id = entry.state.slot_id;
        if entry.meta.reap_committed {
            // The never-started reap already committed under this same
            // mutex; accepting the signal now would record proof for a
            // worker the sweep is about to orphan.
            return None;
        }
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

    /// Arm hook buffering for `run_id` before its CLI is launched, discarding
    /// any evidence buffered for an earlier attempt of the same run so a
    /// retry cannot inherit it. Hooks that arrive before the slot is
    /// registered are held and moved into the slot by registration; see
    /// [`Self::disarm_pending_hooks`] for the failure path.
    pub fn arm_pending_hooks(&self, run_id: &str) {
        self.pending_hooks
            .lock()
            .expect("pending hooks mutex poisoned")
            .insert(run_id.to_owned(), PendingHooks::default());
    }

    /// Drop buffered pre-registration evidence for `run_id` (the launch
    /// failed before registration, so nothing will ever drain it).
    pub fn disarm_pending_hooks(&self, run_id: &str) {
        self.pending_hooks
            .lock()
            .expect("pending hooks mutex poisoned")
            .remove(run_id);
    }

    /// Record the event kind of the first driver hook seen for `run_id`.
    ///
    /// Diagnostic-only and first-write-wins; the value is reset when the slot
    /// is re-registered and is independent of `driver_signal_at` and reap
    /// fencing. Buffers into the pending slot when registration has not
    /// happened yet (see [`Self::arm_pending_hooks`]).
    pub fn record_hook_event_kind(&self, run_id: &str, kind: &str) {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        if let Some(entry) = guard.values_mut().find(|entry| entry.state.run_id == run_id) {
            if entry.meta.first_hook_event.is_none() {
                entry.meta.first_hook_event = Some(kind.to_owned());
                tracing::info!(run_id, first_hook_event = kind, "first driver hook received");
            }
        } else if let Some(pending) = self
            .pending_hooks
            .lock()
            .expect("pending hooks mutex poisoned")
            .get_mut(run_id)
        {
            pending.first_hook_event.get_or_insert_with(|| kind.to_owned());
        }
    }

    /// The first hook event kind recorded for `run_id`'s live slot, if any.
    /// Diagnostic-only: see [`Self::record_hook_event_kind`].
    pub fn first_hook_event_for_run(&self, run_id: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .values()
            .find(|entry| entry.state.run_id == run_id)
            .and_then(|entry| entry.meta.first_hook_event.clone())
    }

    /// Whether a driver-originated signal has been recorded for `slot_id`.
    pub fn driver_signal_at(&self, slot_id: u8) -> Option<i64> {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        guard.get(&slot_id).and_then(|entry| entry.meta.driver_signal_at)
    }

    /// Whether any live slot for `run_id` has recorded a driver-originated
    /// signal. Spawn-time confirmation keys on the run rather than the slot
    /// so a hook that won the race against slot fan-out still counts.
    pub fn has_driver_signal_for_run(&self, run_id: &str) -> bool {
        let guard = self.inner.lock().expect("registry mutex poisoned");
        guard
            .values()
            .any(|entry| entry.state.run_id == run_id && entry.meta.driver_signal_at.is_some())
    }

    /// Record one `LivenessUndeterminable` reap outcome for `run_id`'s live
    /// slot, so [`crate::spawn_ack_sweep::reap_never_started_spawn`] can
    /// escalate a permanently-unreadable liveness answer instead of holding
    /// it forever behind a log line.
    ///
    /// Returns `Some(true)` the first time the consecutive count reaches
    /// `threshold` — the caller should raise exactly one attention item —
    /// `Some(false)` on every other call (including after the item has
    /// already been raised, so it is never raised twice for the same
    /// registration), and `None` when no live slot matches `run_id` (already
    /// released — a benign no-op).
    pub fn record_liveness_undeterminable(&self, run_id: &str, threshold: u32) -> Option<bool> {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        let entry = guard.values_mut().find(|entry| entry.state.run_id == run_id)?;
        entry.meta.undeterminable_liveness_passes = entry.meta.undeterminable_liveness_passes.saturating_add(1);
        let should_raise =
            entry.meta.undeterminable_liveness_passes == threshold && !entry.meta.undeterminable_attention_raised;
        if should_raise {
            entry.meta.undeterminable_attention_raised = true;
        }
        Some(should_raise)
    }

    /// Whether `slot_id`'s current registration is owed driver-start proof
    /// for a pane this engine spawned (`EngineSpawned`) or was re-adopted
    /// (`Readopted`) — see [`DriverStartExpectation`]. Driver-start
    /// verification itself applies to both cases; this only distinguishes
    /// which timeout question is in play. `None` for a slot with no live
    /// entry.
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
    /// ## Re-adoption preserves proof, not an exemption
    ///
    /// A re-adopted slot remains subject to this check. Its registration
    /// timestamp starts a fresh grace window, during which durable semantic
    /// progress is restored if the driver had signalled before an engine
    /// restart. That checkpoint restores `driver_signal_at`, so a genuine
    /// long-running worker remains protected. A re-adoption supported only
    /// by a live login shell has no such proof and must time out: shell
    /// liveness says nothing about whether the driver ever executed.
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

    /// Set the `held` flag for the slot that owns `run_id`. Walks the
    /// registry for a matching `run_id` and writes the flag in place.
    /// Returns the slot id if the entry was found and updated, or `None`
    /// if no live slot matches. Called by the `HoldRun`/`ReleaseHoldRun`
    /// RPC handlers so `bossctl agents list`/`status` reflect an operator
    /// hold immediately, without waiting for the next hook event.
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

        state.last_event_at = Some(now.clone());
        // Driver-originated only: this method is the hook/JSONL ingress.
        // Engine-synthesized display stamps (`mark_stalled_spawns`,
        // `mark_errored`) never reach here, so they cannot advance the
        // semantic checkpoint.
        meta.semantic_progress_at = Some(now);
        meta.semantic_tool_condition = next_tool_condition(event, meta.semantic_tool_condition);
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

        // These timestamps are stamped on hook ingress, including events
        // that leave the observable worker state unchanged. Compare a copy
        // with their prior values restored so only another field makes the
        // broadcast-dedup gate report a change.
        let mut comparable_after = state.clone();
        comparable_after.last_event_at = before.last_event_at.clone();
        comparable_after.last_tool_ended_at = before.last_tool_ended_at.clone();
        before != comparable_after
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
    /// longer than `threshold_secs` seconds and transition them off
    /// `Spawning`, onto whichever activity the driver's own liveness
    /// evidence actually supports.
    ///
    /// The initial directory-trust prompt that Claude Code shows at
    /// session startup (for models that use `--permission-mode auto`)
    /// fires *before* `SessionStart`, so no hook event ever arrives and
    /// the normal `Notification`→`WaitingForInput` path is never
    /// triggered. An unattended headless worker can never answer the
    /// prompt, so the run stalls indefinitely with no UI signal. This
    /// method is the detection path: if `last_event_at` is `None` (no
    /// hook at all) and the slot has been in `Spawning` for more than
    /// `threshold_secs` seconds, the activity is promoted so the existing
    /// kanban dot fires instead of sitting on the `Spawning`/unknown icon.
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
    /// **Two liveness bases, branched on `awaiting_input_capable`.** A
    /// driver's capability declaration decides which claim Boss is allowed
    /// to make, not whether it may claim anything at all:
    ///
    /// - A driver that declares `Capability::AwaitingInputSignal` (Claude)
    ///   gets the directory-trust-prompt promotion above: silence alone
    ///   (`last_event_at == None`) is read as "blocked on the initial
    ///   prompt" and promoted to `WaitingForInput`.
    /// - A driver that omits it (Codex, Grok) never gets that promotion —
    ///   guessing "awaits a human" from silence is exactly the kind of
    ///   "no events for N seconds ⇒ assume blocked" leap `apply_event`
    ///   refuses to make for an untrusted `Notification` from the same
    ///   driver class. But the omission is a claim about the *stream*, not
    ///   about whether Boss has any evidence at all — `meta.driver_signal_at`
    ///   is a *different*, capability-independent fact: a driver-originated
    ///   signal (a hook event, or a resolved `transcript_path`) has been
    ///   observed for this run. For a file-ingress driver like Codex that
    ///   fires as soon as discovery sees the rollout grow — even while the
    ///   first `session_meta` line is still incomplete — and again when
    ///   `AgentJsonlProgressManager` attaches. Proof the process is alive
    ///   and has begun writing its transcript, well before the reader has
    ///   parsed (let alone dispatched) a single complete record. Wait a
    ///   full turn's `thinking` before the first parseable line and this
    ///   sweep is the only thing that would otherwise notice the worker is
    ///   stuck showing `Spawning`/unknown. When that proof exists, the
    ///   honest claim is
    ///   `Idle` — alive, no specific claim about what it is doing — never
    ///   `WaitingForInput`, which this driver class gave no basis for. When
    ///   even that proof is absent, this sweep still leaves the slot in
    ///   `Spawning` rather than guess; `dead_pid_sweep`'s process-liveness
    ///   backstop and driver-start verification (below) are the honest
    ///   fallback for that case.
    ///
    /// ## Reconciliation with driver-start verification
    ///
    /// Both promotion decisions above are *presentation* decisions — "what
    /// may Boss claim this worker is doing?" — and neither decides whether
    /// the slot keeps its resources. Before driver-start verification
    /// existed, the capability skip did exactly that by omission: it left
    /// grok's never-started spawn parked at `Spawning` forever, and the
    /// promotion in the capability-declaring case moved the slot out of
    /// `Spawning` where `spawn_ack_sweep`'s activity filter could no longer
    /// see it. Neither escape survives now:
    ///
    /// - [`Self::unverified_driver_starts`] reads only `driver_signal_at`
    ///   and `spawned_at`, so it is blind to activity, capability and pid
    ///   and covers every branch above identically.
    /// - The `last_event_at` this method synthesizes below is explicitly
    ///   NOT a driver signal. It moves the display timestamp only;
    ///   `driver_signal_at` is untouched (and, in the `Idle` branch, was
    ///   already set by the real driver-originated signal that justified
    ///   the promotion — this method never writes it), so a promotion here
    ///   can never vouch for a driver that never ran.
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
            if state.last_event_at.is_some() {
                // SessionStart (or any other hook) already fired — the
                // worker is past the startup phase; not our concern.
                continue;
            }
            if meta.spawned_at > cutoff {
                // Spawned too recently; give the worker more time.
                continue;
            }
            let promoted = if meta.awaiting_input_capable {
                WorkerActivity::WaitingForInput
            } else if meta.driver_signal_at.is_some() {
                // No capability-backed basis to claim "awaits a human", but
                // real driver-originated evidence (a hook, or — for Codex —
                // rollout file growth during discovery, or attach) says the
                // process is alive. See the branch above for why `Idle`,
                // not `WaitingForInput`, is the only honest claim here.
                WorkerActivity::Idle
            } else {
                // Neither a capability-backed guess nor driver-originated
                // evidence exists yet. Leave it in `Spawning` rather than
                // guess, mirroring the zero-pid case above; `dead_pid_sweep`'s
                // backstop and driver-start verification's reap are the
                // honest fallback for this case (see the design doc's
                // "ProgressObservation minimum-fidelity tier" decision).
                continue;
            };
            state.activity = promoted;
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

    /// Override `last_tool_ended_at` for `slot_id`. Test seam for a
    /// duplicate `PostToolUse` whose ingress timestamp must advance even
    /// though no tool is active.
    #[cfg(test)]
    pub fn set_last_tool_ended_at_for_test(&self, slot_id: u8, last_tool_ended_at: impl Into<String>) {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        if let Some(entry) = guard.get_mut(&slot_id) {
            entry.state.last_tool_ended_at = Some(last_tool_ended_at.into());
        }
    }

    /// Override `activity` for `slot_id`. Test seam for a `Working` slot
    /// whose tool condition is still [`SemanticToolCondition::Unknown`]
    /// (no Pre/PostToolUse has ever established idle/in-flight).
    #[cfg(test)]
    pub fn set_activity_for_test(&self, slot_id: u8, activity: WorkerActivity) {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        if let Some(entry) = guard.get_mut(&slot_id) {
            entry.state.activity = activity;
        }
    }

    /// Override the checkpoint's `tool_condition` for `slot_id` directly,
    /// bypassing [`Self::seed_semantic_progress`]'s no-op guard (which
    /// refuses once `last_event_at` is set). Test seam for reproducing a
    /// driver state reset — the tool condition reverting to
    /// [`SemanticToolCondition::Unknown`] — landing between two
    /// classifications of the same live slot within one sweep pass.
    #[cfg(test)]
    pub fn set_semantic_tool_condition_for_test(&self, slot_id: u8, tool_condition: SemanticToolCondition) {
        let mut guard = self.inner.lock().expect("registry mutex poisoned");
        if let Some(entry) = guard.get_mut(&slot_id) {
            entry.meta.semantic_tool_condition = tool_condition;
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
mod tests;
