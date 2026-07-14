//! Periodic reconciler that detects and reaps active worker slots
//! whose underlying OS process has died.
//!
//! Complements the orphan-active sweep in [`crate::orphan_sweep`]. The
//! orphan sweep detects chores in `active` status with no live
//! execution in the worker pool. This sweep detects chores whose
//! execution IS still claimed in the pool, but the backing OS process
//! is dead (killed, OOM, crash). Without this, a kill-9'd worker
//! leaves the pool slot claimed forever and the orphan sweep skips the
//! chore ("already claimed"), leaving it stuck in Doing indefinitely.
//!
//! ## Algorithm
//!
//! 1. Snapshot [`crate::live_worker_state::LiveWorkerStateRegistry`] to
//!    get every active slot's `(slot_id, run_id, shell_pid, activity)`.
//! 2. For each slot with `shell_pid > 0` and non-terminal activity:
//!    1. Look up the execution in the DB (age guard: skip if
//!       `started_at` is within [`DEAD_PID_GRACE_SECS`] seconds or
//!       `None`, to avoid racing a fresh dispatch whose worker is still
//!       spinning up).
//!    2. Probe liveness via `kill(pid, 0)`:
//!       - `ESRCH` → process does not exist → proceed.
//!       - `0` (alive) or `EPERM` (alive, not ours) → skip.
//!       - Other errors → conservative skip with a warning.
//!    3. Corroborate a `Dead` verdict against recent in-execution hook
//!       activity (see below). If the worker is demonstrably alive, the
//!       tracked pid was the wrong identity — skip, do not reap.
//! 3. For dead PIDs with no corroborating activity:
//!    1. Mark the execution `orphaned` in the DB.
//!    2. Append an `[engine-reconcile]` audit line to the task description.
//!    3. Release the worker pool slot so the orphan sweep can redispatch.
//!    4. Emit a `dead_pid_reconcile` dispatch event.
//!    5. Kick the coordinator.
//!
//! ## False-positive guards
//!
//! The [`DEAD_PID_GRACE_SECS`] (30 s) guard skips executions whose
//! `started_at` is too recent. A worker with no `started_at` yet
//! (pane hasn't begun) is also skipped.
//!
//! ### Liveness corroboration (the T2450 false-reap fix)
//!
//! The registered `shell_pid` is the pty *foreground* pid captured once
//! at surface-init (`ghostty_surface_foreground_pid`). That identity is
//! not stable for the worker's lifetime: a wrapper/login shell can exit
//! or `exec` while the real `claude` process lives on under a different
//! pid, and macOS aggressively reuses pids (the T2450 pid, 98601, appears
//! in the dispatch log of several *older*, long-dead executions). So a
//! bare `kill(pid, 0) == ESRCH` is *not* proof the worker is dead — it
//! can equally mean "the transient process we happened to snapshot has
//! exited while the worker keeps running". Trusting it alone reaped a
//! live worker mid-tool-call in T2450: the run kept emitting transcript
//! events for a full minute after the engine logged its pid "not found".
//!
//! The engine holds a fresher, identity-independent liveness signal:
//! [`boss_protocol::LiveWorkerState::last_event_at`], stamped on every
//! hook event ([`crate::live_worker_state::LiveWorkerStateRegistry::apply_event`]).
//! A worker that is actively producing transcript events cannot be dead,
//! whatever an unrelated pid probe says. So on the *periodic speculative*
//! sweep ([`DeadPidSweepMode::PeriodicSpeculative`]) a `Dead` verdict is
//! corroborated before it is trusted: if the slot emitted a hook within
//! [`DEAD_PID_CORROBORATION_SECS`], *or* has a tool in flight, and that
//! activity is attributed to *this* execution (event at/after the
//! execution's own `started_at`, guarding against a recycled slot's
//! prior-run timestamp — cf. [`crate::stale_worker_sweep`]), the worker
//! is presumed alive and the reap is skipped. A genuinely dead worker
//! goes quiet, so its `last_event_at` ages out of the window and a later
//! pass reaps it — no false reap, at worst a bounded delay on a true one.
//!
//! The app-reattach reconcile ([`DeadPidSweepMode::AppReattach`]) does
//! *not* corroborate: it is a one-shot finalize against a *known-dead*
//! app, so the pid probe is authoritative and there is no second pass to
//! temporally disambiguate a just-before-death event from a survivor that
//! keeps emitting. Genuinely-live survivors there are already spared by
//! the `kill(pid, 0)`-alive check.
//!
//! ## Cadence
//!
//! Runs every 60 seconds and fires once immediately on boot (same
//! pattern as [`crate::orphan_sweep`]).
//!
//! ## Immediate reconciliation
//!
//! [`reap_reported_pane_death`] is the event-driven counterpart: the app
//! calls it (via `FrontendRequest::WorkerPaneDied`) the moment it
//! directly observes a worker pane die — surface creation failed or the
//! child process exited — instead of waiting for the next periodic
//! pass. It shares [`run_one_pass`]'s reap effects but skips the grace
//! period and PID probe, since the app's report is a direct observation
//! rather than a speculative one.

use std::sync::Arc;
use std::time::Duration;

use boss_protocol::{LiveWorkerState, WorkExecution};

use crate::coordinator::{ExecutionCoordinator, worker_id_for_slot};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::{LiveWorkerStateRegistry, iso8601_utc};
use crate::work::WorkDb;

/// Grace period after `started_at` (epoch seconds) during which we
/// skip PID probing. Guards against racing a fresh dispatch whose pane
/// is still spinning up and may not have fully exec'd its shell yet.
pub const DEAD_PID_GRACE_SECS: i64 = 30;

/// A `kill(pid, 0) == ESRCH` verdict on the periodic sweep is only
/// trusted if the worker has *also* gone quiet for at least this long.
/// A hook event newer than this window (or a tool in flight) proves the
/// worker's process tree is alive regardless of what the tracked pid —
/// a possibly-transient or reused snapshot — reports, so the reap is
/// skipped (the T2450 false-reap fix). 120 s is comfortably above the
/// ~10-30 s hook cadence a working worker shows, so a live worker is
/// never in danger, yet an order of magnitude below the
/// [`crate::stale_worker_sweep`] 30-min threshold, so a genuinely dead
/// worker still ages out of the window and gets reaped on a later pass.
/// This is a *corroboration* window, not a longer reap timer: it only
/// ever *prevents* reaping a demonstrably-live worker (a forbidden
/// band-aid would be lengthening the reap interval, which still kills
/// live workers — this does the opposite).
pub const DEAD_PID_CORROBORATION_SECS: i64 = 120;

/// Which entry point is driving a [`run_one_pass`] sweep. The two paths
/// legitimately differ in whether a `Dead` pid verdict is corroborated
/// against recent in-execution event activity before reaping, and whether
/// each reap files a durable pane-death attention item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeadPidSweepMode {
    /// The periodic backstop ([`spawn_loop`]). The registered shell pid
    /// is a *speculative* liveness signal that can vanish (wrapper shell
    /// exit/`exec`, macOS pid reuse) while the worker's process tree
    /// lives on, so a `Dead` verdict is corroborated against recent
    /// in-execution hook activity before reaping (T2450). Reaps here are
    /// routine crash/OOM/kill-9 recoveries, one at a time, already
    /// surfaced via the `dead_pid_reconcile` dispatch event — no
    /// attention item.
    PeriodicSpeculative,
    /// The app-reattach reconcile ([`reconcile_orphans_on_reattach`]).
    /// The prior app is known-dead, so the pid probe is authoritative and
    /// the `Dead` verdict is trusted as-is (a single-shot pass cannot
    /// temporally disambiguate a just-before-death event from a survivor
    /// that keeps emitting; live survivors are already spared by the
    /// `kill(pid, 0)`-alive check). A single relaunch can reap many panes
    /// at once, so each reap files a durable pane-death attention item.
    AppReattach,
}

impl DeadPidSweepMode {
    /// Whether a `Dead` pid verdict is corroborated against recent
    /// in-execution event activity before reaping.
    fn corroborate_liveness(self) -> bool {
        matches!(self, DeadPidSweepMode::PeriodicSpeculative)
    }

    /// Whether each reap also files a [`PANE_DEATH_ATTENTION_KIND`]
    /// attention item on the work item.
    fn file_pane_death_attention(self) -> bool {
        matches!(self, DeadPidSweepMode::AppReattach)
    }
}

/// `work_attention_items.kind` filed when a pane is reaped specifically
/// via [`reconcile_orphans_on_reattach`] (an app relaunch killed it),
/// as opposed to the periodic [`spawn_loop`] pass (crash/OOM/kill-9).
/// Scoped to the work item (not the execution) and deduped on `open`
/// status via [`crate::work::WorkDb::upsert_work_item_attention`] so a
/// relaunch that kills many panes at once — or repeated relaunches
/// before a human acks — doesn't pile up duplicate rows for the same
/// chore.
pub const PANE_DEATH_ATTENTION_KIND: &str = "pane_death_reconcile";

/// Counts from one pass of the sweep; logged at `info` when activity
/// occurs.
#[derive(Debug, Default)]
pub struct DeadPidSweepOutcome {
    pub reaped: usize,
    pub alive_skipped: usize,
    pub unknown_pid_skipped: usize,
    pub grace_skipped: usize,
    /// Slots whose pid probed `Dead` but whose recent in-execution hook
    /// activity (or in-flight tool) contradicted the probe — the tracked
    /// pid was the wrong identity and the (live) worker was spared. This
    /// is the T2450 false-reap that would otherwise have fired.
    pub live_corroborated_skipped: usize,
}

impl crate::sweep_loop::SweepOutcome for DeadPidSweepOutcome {
    fn has_activity(&self) -> bool {
        self.reaped > 0
    }

    fn log(&self) {
        tracing::info!(
            reaped = self.reaped,
            alive_skipped = self.alive_skipped,
            live_corroborated_skipped = self.live_corroborated_skipped,
            grace_skipped = self.grace_skipped,
            "dead-pid sweep: pass complete",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so post-crash orphans are resolved on
/// engine boot without waiting for the first interval.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let live_states = Arc::clone(&live_states);
        let coordinator = Arc::clone(&coordinator);
        let dispatch_events = Arc::clone(&dispatch_events);
        async move {
            run_one_pass(
                work_db.as_ref(),
                live_states.as_ref(),
                coordinator.clone(),
                dispatch_events.as_ref(),
                DeadPidSweepMode::PeriodicSpeculative,
            )
            .await
        }
    })
}

/// Reconcile engine-side worker slots against live worker processes
/// immediately after the macOS app re-attaches (a relaunch against a
/// surviving engine — see `handle_register_app_session`).
///
/// A worker's shell process is a child of the app process, so when the
/// app is killed and relaunched every in-flight worker's process dies
/// with it — yet the engine's slot bindings, pool claims, and DB
/// execution rows all survive the app's death. Left alone they sit
/// orphaned until the next periodic [`spawn_loop`] pass (up to its
/// interval later), producing the three-way desync from the 2026-07-03
/// relaunch: the engine slot stays bound to a terminated run, the new
/// app has no pane for that slot, and the work item stays "active"
/// indefinitely.
///
/// This is the event-driven counterpart to the periodic sweep: on
/// re-attach we run one [`run_one_pass`] immediately so dead workers are
/// finalized (execution → `orphaned`, pool slot released, cube lease
/// freed via the coordinator, chore redispatchable) within seconds
/// instead of waiting for the timer. It reuses the sweep's PID-liveness
/// probe verbatim, so a worker whose process somehow survived the
/// relaunch is never reaped (`kill(pid, 0)` still reports it alive) —
/// this checks *process* liveness, not lease health, which the
/// cube-lease heartbeat keeps refreshing even for dead-process
/// executions.
///
/// Unlike the periodic [`spawn_loop`] pass, every reap here files a
/// [`PANE_DEATH_ATTENTION_KIND`] attention item on the affected work
/// item (deduped/rate-limited — see that constant's docs) so an
/// operator has a durable, dismissable record that the relaunch reset
/// their in-flight work, not just a `dead_pid_reconcile` line in the
/// dispatch tail.
pub async fn reconcile_orphans_on_reattach(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    prior_app_pid: libc::pid_t,
    new_app_pid: libc::pid_t,
) -> DeadPidSweepOutcome {
    tracing::info!(
        prior_app_pid,
        new_app_pid,
        "app re-attach: reconciling engine worker slots against live processes",
    );
    let outcome = run_one_pass(
        work_db.as_ref(),
        live_states.as_ref(),
        coordinator,
        dispatch_events.as_ref(),
        DeadPidSweepMode::AppReattach,
    )
    .await;
    tracing::info!(
        prior_app_pid,
        new_app_pid,
        reaped = outcome.reaped,
        alive_skipped = outcome.alive_skipped,
        grace_skipped = outcome.grace_skipped,
        "app re-attach: slot reconciliation complete",
    );
    outcome
}

/// Run a single dead-PID sweep pass. Returns a summary of what
/// happened; callers may log it.
///
/// Takes `coordinator` as `Arc` because kicking the scheduler
/// requires `Arc<ExecutionCoordinator>` — the kick path spawns a
/// tokio task that holds a reference.
///
/// `mode` selects the two behaviors that legitimately differ between the
/// periodic backstop and the app-reattach reconcile — whether a `Dead`
/// pid verdict is corroborated against recent in-execution event activity
/// before reaping, and whether each reap files a durable pane-death
/// attention item. See [`DeadPidSweepMode`].
pub async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    mode: DeadPidSweepMode,
) -> DeadPidSweepOutcome {
    let file_pane_death_attention = mode.file_pane_death_attention();
    let mut outcome = DeadPidSweepOutcome::default();
    let snapshot = live_states.snapshot();

    let now_epoch_secs: i64 = crate::epoch_time::now_epoch_secs();
    let grace_cutoff = now_epoch_secs - DEAD_PID_GRACE_SECS;

    for state in snapshot {
        // Skip slots with unknown PID (pane not yet reported a pid back).
        if state.shell_pid <= 0 {
            outcome.unknown_pid_skipped += 1;
            continue;
        }

        // Skip terminal slots — the completion path handles these.
        if state.activity.is_terminal() {
            continue;
        }

        let execution_id = &state.run_id;

        // Look up the execution for the age guard and work_item_id.
        let Some(execution) = crate::sweep_loop::lookup_execution_or_warn(
            work_db,
            execution_id,
            "dead-pid sweep: failed to look up execution; skipping slot",
        ) else {
            continue;
        };

        // Skip executions already in a terminal DB state (completion
        // path may have raced the sweep).
        if execution.status.is_terminal() {
            continue;
        }

        // Grace-period guard: skip executions whose `started_at` is
        // within DEAD_PID_GRACE_SECS or not yet recorded. A missing
        // `started_at` means the pane hasn't fully exec'd yet.
        let started_epoch = execution.started_epoch();
        let started_epoch = match started_epoch {
            None => {
                outcome.grace_skipped += 1;
                continue;
            }
            Some(t) if t >= grace_cutoff => {
                outcome.grace_skipped += 1;
                continue;
            }
            Some(t) => t,
        };

        // Probe PID liveness via kill(pid, 0).
        match probe_pid(state.shell_pid) {
            PidStatus::Alive | PidStatus::PermissionDenied => {
                outcome.alive_skipped += 1;
                continue;
            }
            PidStatus::Unknown(err) => {
                tracing::warn!(
                    execution_id,
                    pid = state.shell_pid,
                    error = %err,
                    "dead-pid sweep: unexpected kill(0) error; skipping conservatively",
                );
                outcome.alive_skipped += 1;
                continue;
            }
            PidStatus::Dead => {
                // Fall through to reap.
            }
        }

        // Corroborate the `Dead` verdict before trusting it (periodic
        // speculative sweep only). The tracked shell pid is a fragile
        // identity — a wrapper shell that exited/exec'd, or a reused pid —
        // so `ESRCH` does not by itself prove the worker is dead. If the
        // worker emitted a hook within DEAD_PID_CORROBORATION_SECS or has
        // a tool in flight (attributed to THIS execution), it is
        // demonstrably alive and the reap is skipped. See module docs;
        // this is the T2450 false-reap fix.
        if mode.corroborate_liveness()
            && let Some(activity) = corroborating_liveness(&state, started_epoch, now_epoch_secs)
        {
            tracing::warn!(
                execution_id,
                pid = state.shell_pid,
                slot_id = state.slot_id,
                probe = "kill(pid,0)",
                probe_result = "ESRCH",
                corroborating_activity = %activity,
                "dead-pid sweep: pid probed dead but worker is demonstrably alive for this \
                 execution; the tracked shell pid was a transient/reused identity — NOT reaping \
                 (T2450 false-reap guard)",
            );
            outcome.live_corroborated_skipped += 1;
            continue;
        }

        // No corroborating activity: the worker is presumed genuinely
        // dead. Capture what the probe and live-state actually observed so
        // the reap is explainable from the run's record and the dispatch
        // tail (the operator's "a transcript must never just stop with no
        // reason" ask).
        let observation = LivenessProbeObservation::from_dead_probe(&state, now_epoch_secs);
        let reason = format!(
            "dead-pid-reconcile: shell PID {} not found (kill(pid,0)=ESRCH); {}; \
             no live-event corroboration within {DEAD_PID_CORROBORATION_SECS}s — process presumed dead",
            state.shell_pid,
            observation.activity_summary(),
        );
        let reaped = reap_dead_execution(
            work_db,
            coordinator.clone(),
            dispatch_events,
            &state,
            &execution,
            ReapOptions {
                reason: &reason,
                now_epoch_secs,
                file_pane_death_attention,
                probe_observation: Some(&observation),
            },
        )
        .await;
        if reaped {
            outcome.reaped += 1;
        }
    }

    outcome
}

/// Corroborate a `Dead` pid verdict against the slot's recent
/// in-execution activity. Returns `Some(reason)` when the worker is
/// demonstrably alive — meaning the reap must be skipped, with `reason`
/// naming the contradicting signal for the log — and `None` when nothing
/// contradicts the dead probe.
///
/// Only activity attributable to *this* execution counts: a recycled slot
/// can carry a *prior* run's `last_event_at` (the slot/exec identity class
/// investigated for [`crate::stale_worker_sweep`]), and a timestamp
/// predating this execution's own `started_at` cannot be one of its
/// events. Gating on `>= started_at` means a genuinely dead worker with a
/// stale prior-run timestamp is still reaped, while a live worker with
/// flowing events is spared.
fn corroborating_liveness(state: &LiveWorkerState, started_epoch: i64, now_epoch_secs: i64) -> Option<String> {
    let started_iso = iso8601_utc(started_epoch);
    // A hook whose timestamp predates this execution's start belongs to a
    // prior run on a recycled slot — not evidence of *this* worker's life.
    let in_execution_event = state.last_event_at.as_deref().filter(|e| *e >= started_iso.as_str());

    // A tool in flight (an unbalanced PreToolUse) means the worker is
    // legitimately busy — most importantly a long foreground `bazel build`
    // that emits no hook for many minutes. Genuine death mid-tool closes
    // the pane and is caught by the app's authoritative pane-death report
    // (`reap_reported_pane_death`), not this speculative probe, so sparing
    // here cannot strand a truly-dead worker.
    if let Some(tool) = state.current_tool.as_deref()
        && let Some(event) = in_execution_event
    {
        return Some(format!("tool `{tool}` in flight (last hook {event})"));
    }

    // A hook within the corroboration window proves the process tree is
    // alive whatever the pid probe says.
    let recent_cutoff = iso8601_utc(now_epoch_secs - DEAD_PID_CORROBORATION_SECS);
    if let Some(event) = in_execution_event
        && event >= recent_cutoff.as_str()
    {
        return Some(format!("hook event at {event} within {DEAD_PID_CORROBORATION_SECS}s"));
    }

    None
}

/// Immediately reap the execution behind `run_id` after the app reports
/// its worker pane died — either `ghostty_surface_new` returned NULL
/// (surface never attached) or the pane's child process exited with no
/// app-side restart handler for it (only the Boss pane restarts itself;
/// see `FrontendRequest::WorkerPaneDied`).
///
/// Unlike [`run_one_pass`], this skips [`DEAD_PID_GRACE_SECS`] and the
/// `kill(pid, 0)` liveness probe: those exist to protect the periodic
/// sweep's *speculative* signal (a PID it can no longer find) from
/// racing a worker that is merely slow to start. Here the app is
/// reporting a *direct observation* of its own pane, so there is
/// nothing to protect against racing — waiting the grace period would
/// only delay reconciliation for no benefit. Returns `true` if an
/// execution was actually reaped.
///
/// Never files a [`PANE_DEATH_ATTENTION_KIND`] attention item — that is
/// reserved for [`reconcile_orphans_on_reattach`], where a single app
/// relaunch can kill many panes at once and an operator needs a durable
/// record. A single reported pane death is comparatively rare and
/// already surfaced via the `dead_pid_reconcile` dispatch event.
pub async fn reap_reported_pane_death(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    run_id: &str,
    detail: &str,
) -> bool {
    let Some(state) = live_states.snapshot().into_iter().find(|s| s.run_id == run_id) else {
        tracing::warn!(
            run_id,
            "worker_pane_died: no live slot found for run_id (already released?)"
        );
        return false;
    };

    if state.activity.is_terminal() {
        // Already finalized via the normal completion path; nothing to do.
        return false;
    }

    let Some(execution) = crate::sweep_loop::lookup_execution_or_warn(
        work_db,
        run_id,
        "worker_pane_died: failed to look up execution; skipping reap",
    ) else {
        return false;
    };

    if execution.status.is_terminal() {
        // Completion path raced the app's report; nothing to do.
        return false;
    }

    let now_epoch_secs: i64 = crate::epoch_time::now_epoch_secs();
    let reason = format!("worker-pane-died: {detail}");
    reap_dead_execution(
        work_db,
        coordinator,
        dispatch_events,
        &state,
        &execution,
        ReapOptions {
            reason: &reason,
            now_epoch_secs,
            file_pane_death_attention: false,
            probe_observation: None,
        },
    )
    .await
}

/// What the liveness probe and live-state observed at the moment a
/// periodic-sweep reap was decided. Recorded on the reap's `tracing` log
/// and `dead_pid_reconcile` dispatch-event details so a future death is
/// explainable from the run's record — the operator's explicit ask that a
/// reaped run's transcript never "just stops" with no indication of why.
/// Only produced on the speculative periodic path; the app-reported
/// pane-death reap has no probe to describe.
struct LivenessProbeObservation {
    /// The liveness probe performed, e.g. `"kill(pid,0)"`.
    probe: &'static str,
    /// The probe's raw result, e.g. `"ESRCH"`.
    result: &'static str,
    /// The slot's last hook timestamp (ISO-8601), if any.
    last_event_at: Option<String>,
    /// Age of `last_event_at` in seconds at reap time, if parseable.
    last_event_age_secs: Option<i64>,
    /// Tool in flight at reap time (unbalanced `PreToolUse`), if any.
    current_tool: Option<String>,
}

impl LivenessProbeObservation {
    /// Build the observation for a `kill(pid,0)==ESRCH` verdict from the
    /// slot's live state.
    fn from_dead_probe(state: &LiveWorkerState, now_epoch_secs: i64) -> Self {
        let last_event_age_secs = state
            .last_event_at
            .as_deref()
            .and_then(crate::iso8601::parse_iso8601_to_epoch)
            .map(|e| now_epoch_secs - e);
        Self {
            probe: "kill(pid,0)",
            result: "ESRCH",
            last_event_at: state.last_event_at.clone(),
            last_event_age_secs,
            current_tool: state.current_tool.clone(),
        }
    }

    /// One-line human summary of the live-state activity at reap time,
    /// folded into the orphan `reason` recorded on the run record.
    fn activity_summary(&self) -> String {
        let event = match (&self.last_event_at, self.last_event_age_secs) {
            (Some(at), Some(age)) => format!("last hook {at} ({age}s ago)"),
            (Some(at), None) => format!("last hook {at}"),
            (None, _) => "no hook event ever observed".to_owned(),
        };
        match &self.current_tool {
            Some(tool) => format!("{event}, tool `{tool}` in flight"),
            None => format!("{event}, no tool in flight"),
        }
    }

    /// The observation as JSON fields for the dispatch-event details.
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "probe": self.probe,
            "probe_result": self.result,
            "last_event_at": self.last_event_at,
            "last_event_age_secs": self.last_event_age_secs,
            "current_tool": self.current_tool,
        })
    }
}

/// Per-reap parameters that don't identify *what* is being reaped (that's
/// `state`/`execution`) but *how* to record and report the reap. Bundled
/// to keep [`reap_dead_execution`]'s argument count under the
/// `clippy::too_many_arguments` threshold.
struct ReapOptions<'a> {
    reason: &'a str,
    now_epoch_secs: i64,
    file_pane_death_attention: bool,
    /// What the liveness probe observed, when the reap came from the
    /// speculative periodic sweep. `None` for the app-reported pane-death
    /// reap, which has no speculative probe to describe.
    probe_observation: Option<&'a LivenessProbeObservation>,
}

/// Shared reap effects for a single dead worker: mark the execution
/// orphaned, back up uncommitted workspace work, append the
/// `[engine-reconcile]` audit line, release the pool slot, emit a
/// `dead_pid_reconcile` dispatch event, and (when
/// `file_pane_death_attention` is set) file a durable
/// [`PANE_DEATH_ATTENTION_KIND`] attention item. Shared between
/// [`run_one_pass`] and [`reap_reported_pane_death`] so all paths — the
/// periodic sweep, an app-reattach reconcile, and an authoritative app
/// report — leave the DB, pool, and audit trail in the same shape.
/// Returns `false` (with no other effect) if the DB write to mark the
/// execution orphaned fails.
async fn reap_dead_execution(
    work_db: &WorkDb,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    state: &LiveWorkerState,
    execution: &WorkExecution,
    options: ReapOptions<'_>,
) -> bool {
    let ReapOptions {
        reason,
        now_epoch_secs,
        file_pane_death_attention,
        probe_observation,
    } = options;
    let execution_id = &state.run_id;

    tracing::info!(
        execution_id,
        work_item_id = %execution.work_item_id,
        pid = state.shell_pid,
        slot_id = state.slot_id,
        probe = probe_observation.map(|o| o.probe),
        probe_result = probe_observation.map(|o| o.result),
        last_event_at = probe_observation.and_then(|o| o.last_event_at.as_deref()),
        last_event_age_secs = probe_observation.and_then(|o| o.last_event_age_secs),
        current_tool = probe_observation.and_then(|o| o.current_tool.as_deref()),
        reason,
        "dead-pid reconcile: reaping execution and releasing slot",
    );

    // Mark the execution orphaned so the DB reflects the crash and
    // bossctl agents transcript <exec-id> still works.
    if let Err(err) = work_db.mark_execution_orphaned(execution_id, reason) {
        tracing::warn!(
            execution_id,
            ?err,
            "dead-pid reconcile: failed to mark execution orphaned; skipping reap",
        );
        return false;
    }

    // Snapshot the dead worker's uncommitted workspace work to a
    // durable patch before the slot is released and the workspace
    // becomes eligible for re-lease/reset. Best-effort: a failed or
    // empty capture returns None and never blocks the reap.
    let recovery_patch = crate::recovery_backup::backup_dead_execution(execution);

    // Append [engine-reconcile] audit line to the task description
    // so a human inspecting the chore can see why it was reset (and
    // where to find the recovery patch, if one was captured).
    if let Some(work_item_id) = &state.work_item_id
        && let Err(err) = crate::reconcile_audit::append_reconcile_audit(
            work_db,
            work_item_id,
            now_epoch_secs,
            &format!("dead worker (exec {execution_id}) detected via PID probe; chore reset to todo for redispatch"),
            recovery_patch.as_deref(),
        )
    {
        tracing::warn!(
            work_item_id,
            ?err,
            "dead-pid reconcile: failed to append audit line to description (non-fatal)",
        );
    }

    // Release the worker pool slot so the orphan sweep detects
    // the chore and creates a fresh ready execution for redispatch.
    // Use worker_id_for_slot (not WorkerPool::worker_id_for_slot) so
    // automation-pool slots (> MAX_WORKER_POOL_SIZE) produce the
    // "auto-worker-N" prefix and release_worker_and_kick routes to the
    // correct pool via pool_for_worker_id.
    let worker_id = worker_id_for_slot(state.slot_id);
    coordinator.release_worker_and_kick(&worker_id, None).await;

    // Structured event for bossctl dispatch tail. Fold in what the
    // liveness probe observed (probe type, result, and the live-state
    // activity snapshot) so an operator can see exactly why the engine
    // concluded the worker was dead — not just that it did.
    let mut details = serde_json::json!({
        "dead_pid": state.shell_pid,
        "slot_id": state.slot_id,
        "recovery_patch": recovery_patch
            .as_deref()
            .map(|p| p.display().to_string()),
    });
    if let Some(observation) = probe_observation
        && let Some(obj) = details.as_object_mut()
        && let serde_json::Value::Object(probe) = observation.to_json()
    {
        obj.extend(probe);
    }
    dispatch_events
        .emit(
            DispatchEvent::new(Stage::DeadPidReconcile, Outcome::Ok, execution_id)
                .with_work_item(&execution.work_item_id)
                .with_details(details),
        )
        .await;

    if file_pane_death_attention {
        file_pane_death_attention_item(work_db, &execution.work_item_id, execution_id);
    }

    true
}

/// File (or no-op onto an already-`open` one) a [`PANE_DEATH_ATTENTION_KIND`]
/// attention item for `work_item_id`, naming the reaped `execution_id`.
/// Best-effort: a filing failure is logged and swallowed — an attention
/// item is a courtesy on top of the reap, not a precondition for it.
fn file_pane_death_attention_item(work_db: &WorkDb, work_item_id: &str, execution_id: &str) {
    let title = "App relaunch killed a worker pane".to_owned();
    let body = format!(
        "An app relaunch reset this chore: its worker pane's process died along with the \
         previous app instance, and the engine reconciled execution `{execution_id}` — marking \
         it orphaned and freeing its pool slot so the orphan sweep can redispatch. No work was \
         lost beyond the in-progress turn (any uncommitted workspace changes were backed up \
         where possible).\n\n\
         This item is informational; dismiss it once you've confirmed the chore resumed. It \
         won't be re-filed for this chore while it stays open, even if further relaunches kill \
         subsequent panes."
    );
    if let Err(err) = work_db.upsert_work_item_attention(work_item_id, PANE_DEATH_ATTENTION_KIND, &title, &body) {
        tracing::warn!(
            work_item_id,
            execution_id,
            ?err,
            "dead-pid sweep: failed to file pane-death attention item (non-fatal)",
        );
    }
}

pub(crate) enum PidStatus {
    Alive,
    Dead,
    PermissionDenied,
    Unknown(std::io::Error),
}

/// Probe whether `pid` is alive via `kill(pid, 0)`:
/// - Returns `Alive` when the process exists and we can signal it.
/// - Returns `Dead` when `ESRCH` (no such process).
/// - Returns `PermissionDenied` when `EPERM` (process exists, not ours).
/// - Returns `Unknown` on any other error; caller skips conservatively.
pub(crate) fn probe_pid(pid: i32) -> PidStatus {
    // SAFETY: kill(pid, 0) sends no signal; it only checks whether
    // the process exists and we have permission to signal it. The
    // `pid` value comes from the OS-reported shell_pid at spawn time.
    let ret = unsafe { libc::kill(pid, 0) };
    if ret == 0 {
        return PidStatus::Alive;
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ESRCH) => PidStatus::Dead,
        Some(libc::EPERM) => PidStatus::PermissionDenied,
        _ => PidStatus::Unknown(err),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use boss_protocol::WorkItemBinding;

    use super::*;
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::test_support::*;
    use crate::work::{ExecutionStatus, WorkDb};

    /// Register a slot in the live-state registry with the given PID and
    /// an optional work-item binding. Activity is left as `Spawning`
    /// (non-terminal, so the sweep considers it).
    fn register_slot_with_binding(
        live_states: &LiveWorkerStateRegistry,
        slot_id: u8,
        execution_id: &str,
        shell_pid: i32,
        work_item_id: &str,
    ) {
        live_states.register_spawn(
            slot_id,
            execution_id,
            "claude-opus-4-7",
            shell_pid,
            Some(WorkItemBinding {
                work_item_id: work_item_id.to_owned(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
    }

    /// Returns a PID that is guaranteed to not exist. Spawns the trivially
    /// short-lived `true` command, waits for it to exit, and returns its
    /// released PID. There is a narrow race where the OS could recycle the
    /// PID between `wait()` and `kill(0)`, but in practice this does not
    /// occur in test environments.
    fn dead_pid() -> i32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let _ = child.wait(); // blocks until the process exits
        pid
    }

    /// Drive a slot to `Working` with NO tool in flight (a balanced
    /// PreToolUse/PostToolUse pair). Stamps `last_event_at` to "now".
    fn drive_working_idle(live_states: &LiveWorkerStateRegistry, slot_id: u8) {
        use boss_protocol::WorkerEvent;
        live_states.apply_event(
            slot_id,
            &WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );
        live_states.apply_event(
            slot_id,
            &WorkerEvent::PostToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
                tool_response: serde_json::json!({}),
            },
        );
    }

    /// Drive a slot to `Working` WITH a tool in flight (a PreToolUse with
    /// no balancing PostToolUse) — models a long foreground `bazel build`.
    /// Stamps `last_event_at` to "now".
    fn drive_tool_in_flight(live_states: &LiveWorkerStateRegistry, slot_id: u8) {
        use boss_protocol::WorkerEvent;
        live_states.apply_event(
            slot_id,
            &WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );
    }

    /// The parsed `started_at` epoch for an execution (panics if unset).
    fn started_epoch_of(db: &WorkDb, execution_id: &str) -> i64 {
        db.get_execution(execution_id)
            .unwrap()
            .started_at
            .and_then(|s| s.parse::<i64>().ok())
            .expect("started_at set")
    }

    // ─── tests ───────────────────────────────────────────────────────────────

    /// A slot backed by the live test process PID is never reaped, even
    /// when the grace period has passed.
    #[tokio::test]
    async fn live_pid_is_not_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(
            &live_states,
            1,
            &execution_id,
            std::process::id() as i32, // self is always alive
            &work_item_id,
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            DeadPidSweepMode::PeriodicSpeculative,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "live PID must not be reaped");
        assert_eq!(outcome.alive_skipped, 1);
        assert!(sink.events().await.is_empty());

        let exec = db.get_execution(&execution_id).unwrap();
        assert_eq!(
            exec.status,
            ExecutionStatus::Ready,
            "execution must be untouched when PID alive"
        );
    }

    /// A slot with shell_pid == 0 (PID not yet reported by the app) is
    /// skipped — the pane may still be spinning up.
    #[tokio::test]
    async fn zero_pid_slot_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(
            &live_states,
            1,
            &execution_id,
            0, // PID unknown
            &work_item_id,
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            DeadPidSweepMode::PeriodicSpeculative,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "zero PID must be skipped");
        assert_eq!(outcome.unknown_pid_skipped, 1);
    }

    /// A slot with a very recent `started_at` is skipped by the grace
    /// guard even if the PID is dead — guards against racing a fresh
    /// dispatch whose worker process hasn't fully started yet.
    #[tokio::test]
    async fn recent_started_at_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        // Stamp started_at = NOW so the grace guard fires.
        let now_secs = crate::epoch_time::now_epoch_secs();
        db.force_started_at_for_test(&execution.id, now_secs).unwrap();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        // Use a definitely-dead PID; the grace guard should fire before
        // we even get to the kill(0) probe.
        let the_dead_pid = dead_pid();
        register_slot_with_binding(&live_states, 1, &execution.id, the_dead_pid, &work_item_id);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution.id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            DeadPidSweepMode::PeriodicSpeculative,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "grace period must prevent reaping fresh executions");
        assert_eq!(outcome.grace_skipped, 1);
    }

    /// A slot with no `started_at` set (pane not yet exec'd) is skipped.
    #[tokio::test]
    async fn missing_started_at_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        // Do NOT force started_at — leave it NULL.

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(&live_states, 1, &execution.id, dead_pid(), &work_item_id);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution.id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            DeadPidSweepMode::PeriodicSpeculative,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "missing started_at must be treated as too fresh");
        assert_eq!(outcome.grace_skipped, 1);
    }

    /// A slot backed by a Terminated-activity live state is not touched
    /// by the sweep — the completion path handles those.
    #[tokio::test]
    async fn terminal_activity_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        live_states.register_spawn(1, &execution_id, "claude-opus-4-7", std::process::id() as i32, None);
        // Advance to Terminated via a SessionEnd event.
        live_states.apply_event(
            1,
            &boss_protocol::WorkerEvent::SessionEnd {
                session_id: "test-session".to_owned(),
                reason: "end_turn".to_owned(),
            },
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            DeadPidSweepMode::PeriodicSpeculative,
        )
        .await;

        assert_eq!(
            outcome.reaped, 0,
            "Terminated activity must not be reaped by this sweep"
        );
    }

    /// The core invariant: a slot with a dead PID and an old enough
    /// execution has its execution marked `orphaned`, its pool slot
    /// released, and a `dead_pid_reconcile` dispatch event emitted.
    /// After the sweep, the orphan-active sweep can redispatch.
    #[tokio::test]
    async fn dead_pid_causes_orphan_and_slot_release() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let the_dead_pid = dead_pid();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(&live_states, 1, &execution_id, the_dead_pid, &work_item_id);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        // Verify the slot starts claimed.
        let claimed_before = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(
            claimed_before.contains(&execution_id),
            "slot must be claimed before the sweep",
        );

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            DeadPidSweepMode::PeriodicSpeculative,
        )
        .await;

        assert_eq!(outcome.reaped, 1, "dead-PID execution must be reaped");
        assert_eq!(outcome.alive_skipped, 0);

        // Execution must be terminal (`orphaned`) in the DB.
        let exec = db.get_execution(&execution_id).unwrap();
        assert_eq!(
            exec.status,
            ExecutionStatus::Orphaned,
            "execution must be marked orphaned after dead-PID reap",
        );

        // Pool slot must be free so the orphan sweep can redispatch.
        let claimed_after = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(
            !claimed_after.contains(&execution_id),
            "pool slot must be released after dead-PID reap",
        );

        // A dead_pid_reconcile dispatch event must have been emitted.
        let events = sink.events().await;
        assert_eq!(events.len(), 1, "expected exactly one dispatch event");
        assert_eq!(events[0].stage, "dead_pid_reconcile");
        assert_eq!(events[0].outcome, "ok");
        assert_eq!(events[0].work_item_id.as_deref(), Some(work_item_id.as_str()),);

        // The task description must contain the [engine-reconcile] audit line.
        let item = db.get_work_item(&work_item_id).unwrap();
        let desc = match &item {
            boss_protocol::WorkItem::Chore(t) | boss_protocol::WorkItem::Task(t) => t.description.clone(),
            _ => panic!("expected chore"),
        };
        assert!(
            desc.contains("[engine-reconcile]"),
            "task description must contain the engine-reconcile audit line; got: {desc:?}",
        );

        // The periodic sweep (file_pane_death_attention = false) must NOT
        // file a pane-death attention item — that's reserved for the
        // reattach path so routine crash reaps stay quiet.
        let attention_items = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert!(
            attention_items.is_empty(),
            "periodic dead-pid sweep must not file a pane-death attention item; got: {attention_items:?}",
        );
    }

    /// The app-re-attach entry point reaps a dead-PID slot exactly like a
    /// periodic pass: the relaunch orphan is finalized (`orphaned`), its
    /// pool slot released, and a `dead_pid_reconcile` event emitted — so a
    /// worker whose host app died does not survive as engine state.
    #[tokio::test]
    async fn reattach_reconcile_reaps_dead_pid() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let the_dead_pid = dead_pid();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(&live_states, 1, &execution_id, the_dead_pid, &work_item_id);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = reconcile_orphans_on_reattach(
            db.clone(),
            live_states.clone(),
            coordinator.clone(),
            sink.clone() as Arc<dyn DispatchEventSink>,
            // Prior (dead) app pid and the relaunched app pid; values are
            // only used for logging so any distinct pair is fine.
            1111,
            2222,
        )
        .await;

        assert_eq!(outcome.reaped, 1, "re-attach must reap the dead relaunch orphan");

        let exec = db.get_execution(&execution_id).unwrap();
        assert_eq!(
            exec.status,
            ExecutionStatus::Orphaned,
            "execution must be orphaned after re-attach reconcile",
        );

        let claimed_after = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(
            !claimed_after.contains(&execution_id),
            "pool slot must be released after re-attach reconcile",
        );

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "dead_pid_reconcile");

        // Unlike the periodic sweep, the reattach path files a durable
        // pane-death attention item on the work item.
        let attention_items = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert_eq!(
            attention_items.len(),
            1,
            "reattach reconcile must file exactly one pane-death attention item; got: {attention_items:?}",
        );
        assert_eq!(attention_items[0].kind, PANE_DEATH_ATTENTION_KIND);
        assert_eq!(attention_items[0].status, "open");
    }

    /// A single app relaunch that kills panes across several redispatch
    /// generations of the SAME chore must not pile up duplicate attention
    /// items — the second reattach reconcile against a fresh execution for
    /// the same (still-unacked) work item reuses the still-open item from
    /// the first.
    #[tokio::test]
    async fn reattach_reconcile_dedupes_pane_death_attention_across_redispatches() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        // First generation: reaped by one reattach reconcile.
        let first_execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(&live_states, 1, &first_execution_id, dead_pid(), &work_item_id);
        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&first_execution_id, None).await;
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = reconcile_orphans_on_reattach(
            db.clone(),
            live_states.clone(),
            coordinator.clone(),
            sink.clone() as Arc<dyn DispatchEventSink>,
            1111,
            2222,
        )
        .await;
        assert_eq!(outcome.reaped, 1);

        // Second generation: a fresh execution for the same chore, killed
        // by a second relaunch before anyone acked the first attention item.
        let second_execution_id = create_old_execution(&db, &work_item_id);
        register_slot_with_binding(&live_states, 2, &second_execution_id, dead_pid(), &work_item_id);
        coordinator.worker_pool().claim_worker(&second_execution_id, None).await;
        let outcome = reconcile_orphans_on_reattach(
            db.clone(),
            live_states.clone(),
            coordinator.clone(),
            sink.clone() as Arc<dyn DispatchEventSink>,
            2222,
            3333,
        )
        .await;
        assert_eq!(outcome.reaped, 1, "second relaunch must still reap the fresh execution");

        let attention_items = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert_eq!(
            attention_items.len(),
            1,
            "repeated relaunches must not pile up duplicate pane-death attention items; got: {attention_items:?}",
        );
        assert_eq!(attention_items[0].kind, PANE_DEATH_ATTENTION_KIND);
    }

    /// A worker whose process outlived the relaunch (live PID) is never
    /// reaped by the re-attach reconcile — it checks process liveness, not
    /// the app's death, so a surviving worker keeps its slot.
    #[tokio::test]
    async fn reattach_reconcile_spares_live_pid() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(
            &live_states,
            1,
            &execution_id,
            std::process::id() as i32, // self is always alive
            &work_item_id,
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = reconcile_orphans_on_reattach(
            db.clone(),
            live_states.clone(),
            coordinator.clone(),
            sink.clone() as Arc<dyn DispatchEventSink>,
            1111,
            2222,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "live PID must survive the re-attach reconcile");
        assert_eq!(outcome.alive_skipped, 1);
        assert!(sink.events().await.is_empty());

        let exec = db.get_execution(&execution_id).unwrap();
        assert_eq!(
            exec.status,
            ExecutionStatus::Ready,
            "a live worker's execution must be untouched by re-attach reconcile",
        );
    }

    // ─── reap_reported_pane_death ───────────────────────────────────────────

    /// The core invariant: an app-reported pane death reaps the execution
    /// immediately, even though `started_at` is fresh (well within
    /// `DEAD_PID_GRACE_SECS`) and the "PID" is still alive — neither guard
    /// applies here because the app's report is a direct observation, not
    /// a speculative signal to protect against.
    #[tokio::test]
    async fn reap_reported_pane_death_bypasses_grace_and_pid_checks() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        // Stamp started_at = NOW — within the grace window the periodic
        // sweep would respect, but reap_reported_pane_death must not.
        let now_secs = crate::epoch_time::now_epoch_secs();
        db.force_started_at_for_test(&execution.id, now_secs).unwrap();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        // Still-alive PID (self) — the periodic sweep would never reap this.
        register_slot_with_binding(&live_states, 1, &execution.id, std::process::id() as i32, &work_item_id);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution.id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let reaped = reap_reported_pane_death(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            &execution.id,
            "surface failed to attach",
        )
        .await;

        assert!(reaped, "app-reported pane death must reap immediately");

        let exec = db.get_execution(&execution.id).unwrap();
        assert_eq!(exec.status, ExecutionStatus::Orphaned);

        let claimed_after = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(!claimed_after.contains(&execution.id), "pool slot must be released");

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "dead_pid_reconcile");
    }

    /// No live slot for the reported `run_id` (already released, or the
    /// app raced a normal completion) is a no-op, not an error.
    #[tokio::test]
    async fn reap_reported_pane_death_returns_false_for_unknown_run_id() {
        let (_dir, db) = open_db();
        let db = Arc::new(db);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());

        let reaped = reap_reported_pane_death(
            db.as_ref(),
            &live_states,
            coordinator,
            sink.as_ref(),
            "run-does-not-exist",
            "surface failed to attach",
        )
        .await;

        assert!(!reaped);
        assert!(sink.events().await.is_empty());
    }

    /// A slot already `Terminated` was finalized via the normal
    /// completion path; the app's death report must not double-reap it.
    #[tokio::test]
    async fn reap_reported_pane_death_skips_terminal_slot() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        live_states.register_spawn(1, &execution_id, "claude-opus-4-7", std::process::id() as i32, None);
        live_states.apply_event(
            1,
            &boss_protocol::WorkerEvent::SessionEnd {
                session_id: "test-session".to_owned(),
                reason: "end_turn".to_owned(),
            },
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let reaped = reap_reported_pane_death(
            db.as_ref(),
            &live_states,
            coordinator,
            sink.as_ref(),
            &execution_id,
            "surface failed to attach",
        )
        .await;

        assert!(!reaped, "a Terminated slot must not be reaped again");
        assert!(sink.events().await.is_empty());
    }

    /// An execution already terminal in the DB (completion raced the
    /// app's report) is left untouched.
    #[tokio::test]
    async fn reap_reported_pane_death_skips_terminal_execution() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        db.mark_execution_orphaned(&execution_id, "already finalized").unwrap();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(&live_states, 1, &execution_id, std::process::id() as i32, &work_item_id);

        let coordinator = make_coordinator(db.clone(), 1);
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let reaped = reap_reported_pane_death(
            db.as_ref(),
            &live_states,
            coordinator,
            sink.as_ref(),
            &execution_id,
            "surface failed to attach",
        )
        .await;

        assert!(!reaped);
        assert!(sink.events().await.is_empty());
    }

    // ─── liveness corroboration (the T2450 false-reap fix) ────────────────────

    /// The T2450 reproduction: the registered shell pid is *dead* (a
    /// wrapper/login shell that exited or exec'd), but the worker's real
    /// `claude` process is alive and actively emitting hook events. The
    /// periodic speculative sweep must NOT reap it — the recent
    /// in-execution activity corroborates that the worker lives despite the
    /// dead pid. This is exactly the run that kept producing transcript
    /// events for a full minute after the engine logged its pid "not found".
    #[tokio::test]
    async fn dead_pid_with_recent_event_is_not_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        // Dead pid = the transient wrapper shell that exited.
        register_slot_with_binding(&live_states, 1, &execution_id, dead_pid(), &work_item_id);
        // Recent hook events = the real worker process, alive and working.
        drive_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            DeadPidSweepMode::PeriodicSpeculative,
        )
        .await;

        assert_eq!(
            outcome.reaped, 0,
            "a live worker with a dead tracked pid must NOT be reaped"
        );
        assert_eq!(outcome.live_corroborated_skipped, 1);

        // Execution untouched, slot still claimed, no reap event.
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Ready,
            "execution must be untouched when live-event activity contradicts the dead probe",
        );
        assert!(
            coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id),
            "the live worker's slot must remain claimed",
        );
        assert!(
            sink.events().await.is_empty(),
            "no reap event may be emitted for a live worker"
        );
    }

    /// A dead tracked pid with a *tool in flight* (a long foreground
    /// `bazel build` that emits no hook for minutes) is spared even when
    /// the last hook predates the corroboration window — a tool in flight
    /// is itself proof the worker is busy, and a genuine death mid-tool is
    /// caught by the app's authoritative pane-death report, not this probe.
    #[tokio::test]
    async fn dead_pid_with_tool_in_flight_is_not_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let started = started_epoch_of(&db, &execution_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(&live_states, 1, &execution_id, dead_pid(), &work_item_id);
        drive_tool_in_flight(&live_states, 1);
        // Backdate the PreToolUse well past the corroboration window, but
        // still after this execution's start (so it is correctly
        // attributed). Only the in-flight tool spares it here.
        live_states.set_last_event_at_for_test(1, iso8601_utc(started + 10));

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            DeadPidSweepMode::PeriodicSpeculative,
        )
        .await;

        assert_eq!(
            outcome.reaped, 0,
            "a tool in flight must never be reaped by the dead-pid sweep"
        );
        assert_eq!(outcome.live_corroborated_skipped, 1);
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
        assert!(sink.events().await.is_empty());
    }

    /// A genuinely dead worker — dead pid, an in-execution hook that has
    /// aged out of the corroboration window, and no tool in flight — is
    /// still reaped. Proves the corroboration guard narrows the reap to
    /// live workers rather than disabling it.
    #[tokio::test]
    async fn dead_pid_with_stale_event_and_no_tool_is_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let started = started_epoch_of(&db, &execution_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(&live_states, 1, &execution_id, dead_pid(), &work_item_id);
        drive_working_idle(&live_states, 1); // Working, no tool in flight
        // Last hook is attributed to this execution (after started) but far
        // older than DEAD_PID_CORROBORATION_SECS — no live corroboration.
        live_states.set_last_event_at_for_test(1, iso8601_utc(started + 5));

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            DeadPidSweepMode::PeriodicSpeculative,
        )
        .await;

        assert_eq!(outcome.reaped, 1, "a genuinely quiet dead worker must still be reaped");
        assert_eq!(outcome.live_corroborated_skipped, 0);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Orphaned
        );

        // The reap event carries what the liveness probe observed (probe
        // type, result, and the live-state activity snapshot) so a future
        // death is explainable from the dispatch tail — the operator's ask
        // that a reaped run never "just stops" with no indication of why.
        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "dead_pid_reconcile");
        let details = &events[0].details;
        assert_eq!(details.get("probe").and_then(|v| v.as_str()), Some("kill(pid,0)"));
        assert_eq!(details.get("probe_result").and_then(|v| v.as_str()), Some("ESRCH"));
        assert!(
            details.get("last_event_at").is_some(),
            "the probe observation's live-state snapshot must be recorded; got: {details}",
        );
    }

    /// A recycled slot carrying a *prior* run's last-event timestamp (an
    /// event predating this execution's `started_at`) must not corroborate
    /// liveness — the current worker is genuinely dead and must be reaped.
    /// Guards against the attribution artifact from [`crate::stale_worker_sweep`]
    /// leaking into the opposite (never-reap) failure here.
    #[tokio::test]
    async fn dead_pid_with_prestart_event_is_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let started = started_epoch_of(&db, &execution_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(&live_states, 1, &execution_id, dead_pid(), &work_item_id);
        // Tool "in flight" AND a recent-looking timestamp — but the
        // timestamp belongs to a PRIOR run (before this execution started),
        // so neither corroboration branch may fire.
        drive_tool_in_flight(&live_states, 1);
        live_states.set_last_event_at_for_test(1, iso8601_utc(started - 3_600));

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            DeadPidSweepMode::PeriodicSpeculative,
        )
        .await;

        assert_eq!(
            outcome.reaped, 1,
            "a prior-run timestamp on a recycled slot must not spare a genuinely dead worker",
        );
        assert_eq!(outcome.live_corroborated_skipped, 0);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Orphaned
        );
    }

    /// Gating: the app-reattach reconcile does NOT corroborate. A dead pid
    /// with recent in-execution events is still reaped there, because the
    /// prior app is known-dead and the reattach pass is one-shot with no
    /// way to temporally disambiguate a just-before-death event from a
    /// survivor. (The periodic sweep, which re-runs, is where corroboration
    /// belongs — see `dead_pid_with_recent_event_is_not_reaped`.)
    #[tokio::test]
    async fn reattach_reconcile_reaps_dead_pid_despite_recent_event() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_binding(&live_states, 1, &execution_id, dead_pid(), &work_item_id);
        drive_working_idle(&live_states, 1); // recent in-execution events

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = reconcile_orphans_on_reattach(
            db.clone(),
            live_states.clone(),
            coordinator.clone(),
            sink.clone() as Arc<dyn DispatchEventSink>,
            1111,
            2222,
        )
        .await;

        assert_eq!(
            outcome.reaped, 1,
            "reattach must reap a dead pid even with recent events (no corroboration on reattach)",
        );
        assert_eq!(outcome.live_corroborated_skipped, 0);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Orphaned
        );
        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "dead_pid_reconcile");
    }

    // ─── corroborating_liveness (pure decision) ────────────────────────────────
    //
    // These exercise the safety-critical spare/reap decision directly, without
    // driving a full sweep, so the assertions are on the observable return
    // value: `None` reaps, `Some(reason)` spares with `reason` naming the
    // contradicting signal. This is the T2450 regression guard — a bug here
    // false-reaps a live worker mid-task.

    /// Build a bare `LiveWorkerState` overriding only the two fields the
    /// decision reads (`last_event_at`, `current_tool`); everything else is
    /// the freshly-spawned default.
    fn live_state(last_event_at: Option<String>, current_tool: Option<String>) -> LiveWorkerState {
        let mut state = LiveWorkerState::new_spawning(1, "run", "claude-opus-4-7", 1234, None);
        state.last_event_at = last_event_at;
        state.current_tool = current_tool;
        state
    }

    /// An idle worker with no in-execution event and no tool in flight is
    /// not corroborated as alive — nothing contradicts the dead probe.
    #[test]
    fn corroboration_none_when_no_event_and_no_tool() {
        let started = 1_000_000;
        let now = started + 300;
        assert_eq!(corroborating_liveness(&live_state(None, None), started, now), None);
    }

    /// A tool in flight with an in-execution event spares the worker even
    /// when that event is old — models a long foreground `bazel build` that
    /// emits no hook for many minutes. The reason names the tool.
    #[test]
    fn corroboration_spares_busy_worker_with_tool_in_flight() {
        let started = 1_000_000;
        // An hour into the run: the last hook is ancient, but a tool is still
        // in flight, so the worker is legitimately busy, not dead.
        let now = started + 3_600;
        let event = iso8601_utc(started + 5);
        let reason = corroborating_liveness(&live_state(Some(event), Some("Bash".to_owned())), started, now)
            .expect("busy worker with a tool in flight must be spared");
        assert!(
            reason.contains("Bash"),
            "reason should name the in-flight tool: {reason}"
        );
    }

    /// A `last_event_at` predating this execution's `started_at` belongs to a
    /// prior run on a recycled slot and must NOT count as liveness — even
    /// though the event is within the wall-clock corroboration window.
    #[test]
    fn corroboration_ignores_event_predating_execution_start() {
        let started = 1_000_000;
        let now = started + 5;
        // 10s before this execution began, but only 15s before `now`.
        let event = iso8601_utc(started - 10);
        assert_eq!(
            corroborating_liveness(&live_state(Some(event), None), started, now),
            None
        );
    }

    /// A prior-run event cannot be rescued by a tool in flight either: the
    /// tool branch also requires the event to belong to this execution.
    #[test]
    fn corroboration_ignores_prior_run_event_even_with_tool() {
        let started = 1_000_000;
        let now = started + 30;
        let event = iso8601_utc(started - 1);
        assert_eq!(
            corroborating_liveness(&live_state(Some(event), Some("Bash".to_owned())), started, now),
            None,
        );
    }

    /// An in-execution hook within `DEAD_PID_CORROBORATION_SECS` of now
    /// proves the process tree is alive whatever the pid probe says.
    #[test]
    fn corroboration_spares_worker_with_recent_in_execution_event() {
        let started = 1_000_000;
        let now = started + 300;
        let event = iso8601_utc(now - 30); // 30s ago, well inside the window
        let reason = corroborating_liveness(&live_state(Some(event), None), started, now)
            .expect("a recent in-execution hook must spare the worker");
        assert!(
            reason.contains("hook event"),
            "reason should describe the hook event: {reason}"
        );
    }

    /// An in-execution event older than the corroboration window, with no
    /// tool in flight, does not spare the worker — it is reaped.
    #[test]
    fn corroboration_reaps_when_event_older_than_window_and_no_tool() {
        let started = 1_000_000;
        let now = started + 600;
        // After start, but 80s past the far edge of the window.
        let event = iso8601_utc(now - (DEAD_PID_CORROBORATION_SECS + 80));
        assert_eq!(
            corroborating_liveness(&live_state(Some(event), None), started, now),
            None
        );
    }

    /// Boundary: an event exactly at the window edge (`now - CORROBORATION`)
    /// is still inside the window and spares the worker.
    #[test]
    fn corroboration_spares_event_exactly_at_window_edge() {
        let started = 1_000_000;
        let now = started + 500;
        let event = iso8601_utc(now - DEAD_PID_CORROBORATION_SECS);
        assert!(
            corroborating_liveness(&live_state(Some(event), None), started, now).is_some(),
            "an event exactly at the window edge must spare the worker",
        );
    }

    /// Boundary: an event one second beyond the window edge is outside the
    /// window and, with no tool in flight, does not spare the worker.
    #[test]
    fn corroboration_reaps_event_one_second_past_window_edge() {
        let started = 1_000_000;
        let now = started + 500;
        let event = iso8601_utc(now - DEAD_PID_CORROBORATION_SECS - 1);
        assert_eq!(
            corroborating_liveness(&live_state(Some(event), None), started, now),
            None
        );
    }

    // ─── DeadPidSweepMode predicates ───────────────────────────────────────────

    /// Only the periodic speculative sweep corroborates a `Dead` verdict
    /// before reaping; the app-reattach reconcile trusts the pid probe.
    #[test]
    fn corroborate_liveness_only_for_periodic_speculative() {
        assert!(DeadPidSweepMode::PeriodicSpeculative.corroborate_liveness());
        assert!(!DeadPidSweepMode::AppReattach.corroborate_liveness());
    }

    /// Only the app-reattach reconcile files a pane-death attention item on
    /// each reap; the periodic sweep's reaps are routine and file none.
    #[test]
    fn file_pane_death_attention_only_for_app_reattach() {
        assert!(DeadPidSweepMode::AppReattach.file_pane_death_attention());
        assert!(!DeadPidSweepMode::PeriodicSpeculative.file_pane_death_attention());
    }
}
