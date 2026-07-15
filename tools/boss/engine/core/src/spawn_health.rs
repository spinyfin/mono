//! Circuit breaker for the app session's worker-pane **spawn capability**.
//!
//! ## The incident this guards against
//!
//! On 2026-07-05 the laptop woke from sleep and every worker-pane spawn
//! silently produced no shell for 1.5+ hours: `ghostty_surface_new` returned
//! NULL (no active display), the app parked the pane in a surface-less
//! placeholder, and the engine saw `shell_pid=0` with zero hook events. The
//! [`crate::spawn_ack_sweep`] reaped each execution after 60s and redispatched
//! it, only to fail identically. The per-work-item churn guard in
//! [`crate::orphan_sweep`] could not stop this: it counts terminal executions
//! **per work item**, and the failures were spread across many different work
//! items, so no single item reached its own churn threshold. The fleet churned
//! for hours until the orphan guard eventually parked items one by one.
//!
//! ## What this adds
//!
//! A cross-work-item aggregator. Every never-started spawn — whether inferred
//! by the 60s [`crate::spawn_ack_sweep`] or reported proactively by the app via
//! `ReportWorkerSpawnFailed` (the fast-fail NACK) — feeds
//! [`SpawnHealthTracker::record_failure`]. When
//! [`SPAWN_HEALTH_DISTINCT_WORK_ITEM_THRESHOLD`] **distinct** work items have
//! failed to spawn a shell within [`SPAWN_HEALTH_WINDOW_SECS`], the app
//! session's spawn path — not any one work item — is treated as broken and
//! [`trip_spawn_capability_circuit`] fires: it always logs loudly and raises
//! the single `app_spawn_capability_unhealthy` attention item, instead of
//! independently churning each work item into its own churn guard.
//!
//! ## The breaker flag (`BOSS_ENABLE_SPAWN_CAPABILITY_BREAKER`, ON by default)
//!
//! Whether a trip *pauses dispatch* is a separate, config-gated decision —
//! see [`SpawnHealthTracker::breaker_enabled`] /
//! [`crate::config::WorkConfig::enable_spawn_capability_breaker`]. The
//! breaker tripped for the first time ever on 2026-07-15 on what turned out
//! to be a benign cause — display sleep + App Nap throttling the app's
//! MainActor, making spawn acks late — and latched the entire fleet's
//! dispatch — `pr_review` included — for ~40 minutes until a human noticed
//! and manually resumed it. That incident drove the flag to default off
//! between PR #2041 and the fix below. Since then, the App Nap opt-out
//! (display sleep no longer degrades spawn acks) and the half-open
//! auto-recovery probe (a transient blip self-heals instead of latching)
//! have landed, so the flag now defaults back **on** for the genuine
//! app-dead/ghost-pane incident class it was designed for.
//!
//! - **Enabled (default):** trip-side behavior pauses dispatch (review
//!   exemption stripped), PLUS automatic recovery — see below. Operators can
//!   still opt out via `BOSS_ENABLE_SPAWN_CAPABILITY_BREAKER=false`.
//! - **Disabled:** the failure-window tracking, the `tracing::error!`,
//!   the `app_spawn_capability_unhealthy` attention item, and the
//!   `spawn_capability_unhealthy` dispatch event all still fire — full
//!   observability of the condition is preserved — but
//!   [`ExecutionCoordinator::set_dispatch_paused`] is never called. The
//!   attention item and dispatch event both say plainly that the breaker
//!   would have tripped and was disabled by config.
//!
//! ## Automatic recovery (half-open probe) — enabled mode only
//!
//! A Breaker-origin pause is NOT human-in-the-loop only. Once tripped,
//! normal dispatch stays fully blocked — [`ExecutionCoordinator::drain_ready_queue`]'s
//! pause gate holds every row, `pr_review` included — which means no
//! execution could ever run to prove the app's spawn path recovered:
//! passive recovery is impossible by construction. Instead
//! [`maybe_admit_recovery_probe`], driven off the existing 60s
//! [`crate::spawn_ack_sweep`] tick, periodically force-dispatches exactly
//! ONE ready execution as a canary (bypassing the pause gate the same way
//! `bossctl agents launch` does) while the breaker is tripped. A real shell
//! pid reported for that canary is proof the spawn path works again and
//! auto-resumes dispatch ([`resume_dispatch_after_breaker_recovery`]); a
//! reap of the canary (spawn-ack timeout or app NACK) backs off
//! exponentially before the next attempt. Dispatch also auto-resumes on a
//! fresh app session registering — an app relaunch is the operator's
//! natural recovery action after e.g. waking the display, so it clears the
//! breaker exactly like a real shell pid would. This recovery machinery is
//! self-gating: it only ever activates on top of a real Breaker-origin
//! pause, and a real pause only happens when the flag is enabled, so no
//! separate flag check is needed inside it.
//!
//! An *operator* pause ([`DispatchPauseOrigin::Operator`]) is unaffected by
//! any of this and stays manual-resume-only — see the origin check in
//! [`resume_dispatch_after_breaker_recovery`].

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use boss_protocol::CreateAttentionItemInput;

use crate::app::handler_helpers::{
    METADATA_KEY_DISPATCH_PAUSE_ORIGIN, METADATA_KEY_DISPATCH_PAUSED, METADATA_KEY_DISPATCH_PAUSED_SINCE,
};
use crate::config::DEFAULT_ENABLE_SPAWN_CAPABILITY_BREAKER;
use crate::coordinator::{DispatchPauseOrigin, ExecutionCoordinator};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::work::WorkDb;

/// Number of **distinct** work items whose spawn must fail inside the window
/// before the breaker trips. Set above the noise of a single flaky spawn but
/// low enough to catch a systemic outage in the first sweep pass — the
/// 2026-07-05 incident failed spawns across three chores plus the automation
/// fleet within minutes.
pub const SPAWN_HEALTH_DISTINCT_WORK_ITEM_THRESHOLD: usize = 3;

/// Sliding window (seconds) over which distinct failing work items are counted.
pub const SPAWN_HEALTH_WINDOW_SECS: i64 = 300;

/// Kind string for the single loud attention item raised when the breaker
/// trips. Stable — external tooling and the app's attention pane pin it.
pub const SPAWN_CAPABILITY_ATTENTION_KIND: &str = "app_spawn_capability_unhealthy";

/// Backoff before the first half-open recovery probe after a trip, and the
/// base of the exponential backoff applied after each subsequent probe
/// failure. 60s mirrors [`crate::spawn_ack_sweep::SPAWN_ACK_GRACE_SECS`] — a
/// failed probe has usually already been reaped by the time the next one
/// would be eligible anyway.
pub const SPAWN_HEALTH_PROBE_BACKOFF_BASE_SECS: i64 = 60;

/// Ceiling on probe backoff so a long-lived outage still gets a recovery
/// attempt at least this often, rather than backing off forever.
pub const SPAWN_HEALTH_PROBE_BACKOFF_MAX_SECS: i64 = 900;

/// Deadline (seconds since [`SpawnHealthTracker::mark_probe_dispatched`])
/// after which an in-flight probe that never resolved through
/// [`SpawnHealthTracker::record_probe_success`] or
/// [`SpawnHealthTracker::record_probe_failure`] is treated as failed by
/// [`SpawnHealthTracker::try_admit_probe`] itself.
///
/// Both of those normal resolution paths assume the canary either reports a
/// shell pid or gets reaped by [`crate::spawn_ack_sweep::reap_never_started_spawn`].
/// But `force_dispatch` returns as soon as scheduling completes, and the
/// actual pane spawn happens later in a detached task — if that task's
/// `adapter.spawn_worker` call itself errors, the execution goes straight to
/// terminal `failed` with no live slot, which both reap paths skip (a
/// terminal execution isn't `Spawning` and isn't reap-eligible). Without this
/// deadline that leaves `in_flight` set forever, so `try_admit_probe` would
/// refuse to admit a next canary and dispatch would stay Breaker-paused
/// until a human ran `bossctl dispatch resume` — the exact latch this module
/// exists to eliminate. Twice
/// [`crate::spawn_ack_sweep::SPAWN_ACK_GRACE_SECS`] gives the normal reap
/// path a full chance to resolve the probe first; this is strictly a
/// last-resort backstop for the terminal-without-reap case.
pub const SPAWN_HEALTH_PROBE_STALL_DEADLINE_SECS: i64 = 120;

/// Sentinel for [`SpawnHealthTracker::last_disabled_signal_at`] meaning "no
/// disabled-mode signal has fired yet" — distinct from a real epoch-seconds
/// timestamp of `0`, so a fresh tracker always signals on its first trip
/// regardless of what `now_epoch_secs` happens to be (e.g. in tests).
const NO_DISABLED_SIGNAL_YET: i64 = i64::MIN;

/// Exponential backoff (capped) for the half-open probe after
/// `consecutive_failures` consecutive failed attempts.
fn probe_backoff_secs(consecutive_failures: u32) -> i64 {
    let shift = consecutive_failures.saturating_sub(1).min(4);
    SPAWN_HEALTH_PROBE_BACKOFF_BASE_SECS
        .saturating_mul(1i64 << shift)
        .min(SPAWN_HEALTH_PROBE_BACKOFF_MAX_SECS)
}

/// State of the half-open recovery probe admitted through a Breaker pause.
/// Lives alongside (but independent of) the failure-window state: a probe
/// cycle starts the moment [`SpawnHealthTracker::try_admit_probe`] first
/// returns `true` after a trip and ends in either
/// [`SpawnHealthTracker::record_probe_success`] (full reset) or repeated
/// [`SpawnHealthTracker::record_probe_failure`] calls (growing backoff).
#[derive(Debug, Default)]
struct ProbeState {
    /// Execution id of the canary currently admitted through the pause, if
    /// any. `try_admit_probe` refuses to admit a second one while this is
    /// `Some` — unless it has gone stale, see
    /// [`SPAWN_HEALTH_PROBE_STALL_DEADLINE_SECS`].
    in_flight: Option<String>,
    /// Epoch seconds at which `in_flight` was set by
    /// [`SpawnHealthTracker::mark_probe_dispatched`]. Meaningless while
    /// `in_flight` is `None`.
    dispatched_at: i64,
    /// Epoch seconds before which no new probe may be admitted.
    next_attempt_at: i64,
    /// Consecutive probe failures since the last success; drives the
    /// exponential backoff in [`probe_backoff_secs`].
    consecutive_failures: u32,
}

/// Tuning for [`SpawnHealthTracker`]'s failure-window trip: how many
/// **distinct** work items must fail within how many seconds. Grouped into
/// its own struct (rather than two more top-level `SpawnHealthTracker`
/// fields) to keep that struct's field count small.
#[derive(Debug, Clone, Copy)]
struct FailureWindowConfig {
    threshold: usize,
    window_secs: i64,
}

/// Cross-work-item failure aggregator for the app spawn path.
///
/// Holds a bounded sliding window of `(work_item_id, epoch_secs)` failures and
/// counts distinct work items in-window. Cheap to share (`Arc`): a
/// `std::sync::Mutex` guards the small vector and is never held across an
/// `.await`.
#[derive(Debug)]
pub struct SpawnHealthTracker {
    /// `(work_item_id, epoch_secs)` of recent spawn failures, pruned to the
    /// window on every `record_failure`.
    recent: Mutex<Vec<(String, i64)>>,
    window: FailureWindowConfig,
    /// Half-open recovery probe state — see [`ProbeState`].
    probe: Mutex<ProbeState>,
    /// Whether a trip is allowed to actually pause dispatch — see
    /// [`WorkConfig::enable_spawn_capability_breaker`](crate::config::WorkConfig::enable_spawn_capability_breaker)
    /// and [`Self::with_breaker_enabled`]. Defaults to
    /// [`DEFAULT_ENABLE_SPAWN_CAPABILITY_BREAKER`] (`true`) on the raw
    /// constructors below, matching the config default; production wiring
    /// in `app.rs` always calls [`Self::with_breaker_enabled`] with the
    /// config value, so an operator's `BOSS_ENABLE_SPAWN_CAPABILITY_BREAKER=false`
    /// override still takes effect there.
    breaker_enabled: bool,
    /// Epoch seconds of the last disabled-mode "would have tripped" signal
    /// for the current outage, or `0` if none has fired yet — see
    /// [`Self::mark_disabled_trip_signaled`]. Time-windowed (not a one-shot
    /// latch cleared by [`Self::record_success`]) because in disabled mode
    /// dispatch never pauses, so spawns keep flowing and `record_success`
    /// fires on every real shell pid throughout the outage; a flapping spawn
    /// path would otherwise re-signal (and raise a fresh durable attention
    /// item) on every single success/failure cycle.
    last_disabled_signal_at: AtomicI64,
}

impl Default for SpawnHealthTracker {
    fn default() -> Self {
        Self::with_config(SPAWN_HEALTH_DISTINCT_WORK_ITEM_THRESHOLD, SPAWN_HEALTH_WINDOW_SECS)
    }
}

impl SpawnHealthTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with explicit tuning (tests use tight values). Defaults
    /// `breaker_enabled` to [`DEFAULT_ENABLE_SPAWN_CAPABILITY_BREAKER`]
    /// (`true`) — the same default as
    /// [`crate::config::WorkConfig::enable_spawn_capability_breaker`]. Tests
    /// that exercise the disabled (observability-only) path must opt out
    /// explicitly via `.with_breaker_enabled(false)`.
    pub fn with_config(threshold: usize, window_secs: i64) -> Self {
        Self {
            recent: Mutex::new(Vec::new()),
            window: FailureWindowConfig {
                threshold: threshold.max(1),
                window_secs: window_secs.max(1),
            },
            probe: Mutex::new(ProbeState::default()),
            breaker_enabled: DEFAULT_ENABLE_SPAWN_CAPABILITY_BREAKER,
            last_disabled_signal_at: AtomicI64::new(NO_DISABLED_SIGNAL_YET),
        }
    }

    /// Set whether a trip is allowed to actually pause dispatch. Consuming
    /// (`mut self -> Self`) so production wiring reads
    /// `SpawnHealthTracker::new().with_breaker_enabled(cfg.work.enable_spawn_capability_breaker)`
    /// before wrapping the tracker in an `Arc`.
    pub fn with_breaker_enabled(mut self, enabled: bool) -> Self {
        self.breaker_enabled = enabled;
        self
    }

    /// `true` if a trip is currently allowed to pause dispatch.
    pub fn breaker_enabled(&self) -> bool {
        self.breaker_enabled
    }

    /// `true` the first time this is called in [`SPAWN_HEALTH_WINDOW_SECS`]
    /// of `now_epoch_secs` — the disabled-mode equivalent of the enabled
    /// path's `coordinator.is_dispatch_paused()` idempotency check, so a
    /// sustained outage logs/raises attention at most once per window
    /// instead of on every subsequent failure.
    ///
    /// Time-windowed rather than a one-shot latch cleared by
    /// [`Self::record_success`]: disabled mode never pauses dispatch, so
    /// spawns keep flowing throughout an outage and `record_success` fires
    /// on every real shell pid reported in between failures. A flapping
    /// spawn path (some spawns succeed, some don't) would otherwise clear
    /// the one-shot latch on every intervening success and raise a fresh
    /// durable attention item on every subsequent failure burst — several
    /// per hour is plausible with a multi-worker pool. Windowing instead of
    /// clearing on success caps it at one signal per window even while the
    /// outage keeps flapping, while a genuinely separate outage — one that
    /// starts more than a window after the last signal — still gets its own.
    fn mark_disabled_trip_signaled(&self, now_epoch_secs: i64) -> bool {
        let last = self.last_disabled_signal_at.load(Ordering::Acquire);
        if last != NO_DISABLED_SIGNAL_YET && now_epoch_secs.saturating_sub(last) < self.window.window_secs {
            return false;
        }
        self.last_disabled_signal_at.store(now_epoch_secs, Ordering::Release);
        true
    }

    /// Record one never-started spawn for `work_item_id` at `now_epoch_secs`.
    ///
    /// Prunes failures older than the window, appends this one, and returns
    /// the number of **distinct** work items that have failed in-window when
    /// that count has reached the threshold (a candidate trip); otherwise
    /// `None`.
    ///
    /// Level-triggered: once at/over the threshold this returns `Some` on
    /// *every* subsequent failure. Idempotency of the resulting action lives
    /// in [`trip_spawn_capability_circuit`] (which no-ops when dispatch is
    /// already paused), so the loud signal fires exactly once per outage.
    pub fn record_failure(&self, work_item_id: &str, now_epoch_secs: i64) -> Option<usize> {
        let mut recent = self.recent.lock().unwrap();
        let cutoff = now_epoch_secs - self.window.window_secs;
        recent.retain(|(_, ts)| *ts >= cutoff);
        recent.push((work_item_id.to_owned(), now_epoch_secs));
        let distinct: HashSet<&str> = recent.iter().map(|(w, _)| w.as_str()).collect();
        let distinct = distinct.len();
        (distinct >= self.window.threshold).then_some(distinct)
    }

    /// Reset the breaker. Called when a spawn provably worked (a real shell
    /// pid was reported) or a fresh app session registered, so stale
    /// pre-recovery failures no longer count toward a trip.
    ///
    /// Deliberately does NOT clear the disabled-mode signal window
    /// ([`Self::mark_disabled_trip_signaled`]): in disabled mode dispatch
    /// never pauses, so this fires on every real shell pid throughout an
    /// outage, including in between bursts of a flapping spawn path. Letting
    /// a success reset the window would re-arm the signal on every flap and
    /// raise a fresh durable attention item per burst; a genuinely new
    /// outage still gets its own signal once the window elapses.
    pub fn record_success(&self) {
        self.recent.lock().unwrap().clear();
    }

    /// Window length in seconds (for the trip event's `details`).
    pub fn window_secs(&self) -> i64 {
        self.window.window_secs
    }

    /// `true` if the half-open recovery probe may be admitted right now: no
    /// probe is currently in flight and backoff since the last failed probe
    /// has elapsed. Peek-only — does not itself claim anything. The caller
    /// still has to find a ready execution and dispatch it, then call
    /// [`Self::mark_probe_dispatched`] once that actually succeeds.
    ///
    /// Also the backstop for a canary that never resolves through either
    /// normal path (see [`SPAWN_HEALTH_PROBE_STALL_DEADLINE_SECS`]): a probe
    /// still `in_flight` after the stall deadline is treated as failed right
    /// here — cleared and backed off exactly like
    /// [`Self::record_probe_failure`] would — so the probe cycle can never
    /// wedge open forever.
    pub fn try_admit_probe(&self, now_epoch_secs: i64) -> bool {
        let mut probe = self.probe.lock().unwrap();
        if probe.in_flight.is_some()
            && now_epoch_secs.saturating_sub(probe.dispatched_at) >= SPAWN_HEALTH_PROBE_STALL_DEADLINE_SECS
        {
            tracing::warn!(
                stalled_execution_id = probe.in_flight.as_deref().unwrap_or_default(),
                "spawn-capability breaker: in-flight recovery probe went stale without resolving; \
                 treating as failed so the probe cycle does not wedge open",
            );
            probe.in_flight = None;
            probe.consecutive_failures = probe.consecutive_failures.saturating_add(1);
            probe.next_attempt_at = now_epoch_secs + probe_backoff_secs(probe.consecutive_failures);
        }
        probe.in_flight.is_none() && now_epoch_secs >= probe.next_attempt_at
    }

    /// Record that `execution_id` is the canary admitted through the
    /// Breaker pause at `now_epoch_secs`. Until it resolves (success,
    /// [`Self::record_probe_failure`], or the
    /// [`SPAWN_HEALTH_PROBE_STALL_DEADLINE_SECS`] backstop in
    /// [`Self::try_admit_probe`]), further admission is refused — only one
    /// probe is ever in flight at a time.
    pub fn mark_probe_dispatched(&self, execution_id: &str, now_epoch_secs: i64) {
        let mut probe = self.probe.lock().unwrap();
        probe.in_flight = Some(execution_id.to_owned());
        probe.dispatched_at = now_epoch_secs;
    }

    /// `true` if `execution_id` is the currently in-flight recovery probe.
    pub fn is_probe_execution(&self, execution_id: &str) -> bool {
        self.probe.lock().unwrap().in_flight.as_deref() == Some(execution_id)
    }

    /// The in-flight probe failed (reaped by the spawn-ack-timeout sweep, an
    /// app NACK, or a synchronous force-dispatch error) — clear it and back
    /// off exponentially before the next attempt. No-op when `execution_id`
    /// isn't the current in-flight probe, so an unrelated reap during the
    /// same outage can't disturb this outage's probe schedule.
    pub fn record_probe_failure(&self, execution_id: &str, now_epoch_secs: i64) {
        let mut probe = self.probe.lock().unwrap();
        if probe.in_flight.as_deref() != Some(execution_id) {
            return;
        }
        probe.in_flight = None;
        probe.consecutive_failures = probe.consecutive_failures.saturating_add(1);
        probe.next_attempt_at = now_epoch_secs + probe_backoff_secs(probe.consecutive_failures);
    }

    /// The in-flight probe succeeded (a real shell pid was reported for
    /// it): fully reset the probe state so the next outage's probing starts
    /// fresh, with no inherited backoff. Returns `true` only when
    /// `execution_id` was in fact the in-flight probe — the caller uses
    /// this to decide whether to auto-resume dispatch.
    pub fn record_probe_success(&self, execution_id: &str) -> bool {
        let mut probe = self.probe.lock().unwrap();
        if probe.in_flight.as_deref() != Some(execution_id) {
            return false;
        }
        *probe = ProbeState::default();
        true
    }

    /// Unconditionally clear all half-open probe state (in-flight canary,
    /// backoff, failure count). Called when a fresh app session registers —
    /// any probe or backoff left over from before is moot once the operator
    /// has taken their own recovery action.
    pub fn reset_probe(&self) {
        *self.probe.lock().unwrap() = ProbeState::default();
    }
}

/// Half-open recovery attempt: if dispatch is currently paused by
/// [`DispatchPauseOrigin::Breaker`] (never an operator pause — see
/// [`resume_dispatch_after_breaker_recovery`]) and the tracker's backoff
/// says a probe is due, force-dispatch exactly one ready execution as a
/// canary — bypassing the pause gate the same way `bossctl agents launch`
/// does via [`ExecutionCoordinator::force_dispatch`] — and mark it in
/// flight. This is the breaker's only way out of the latch: normal
/// dispatch stays fully blocked while paused, so without this no execution
/// could ever run to prove the app's spawn path recovered.
///
/// Driven off the existing 60s [`crate::spawn_ack_sweep`] tick, which
/// already owns the `coordinator` / `spawn_health` handles this needs.
/// Cheap no-op when dispatch isn't Breaker-paused or no probe is due yet.
pub async fn maybe_admit_recovery_probe(
    work_db: &WorkDb,
    coordinator: &Arc<ExecutionCoordinator>,
    spawn_health: &SpawnHealthTracker,
    now_epoch_secs: i64,
) {
    if !coordinator.is_dispatch_paused() || coordinator.dispatch_pause_exempts_reviews() {
        // Not paused, or an operator pause (which stays manual-resume-only)
        // — never auto-probe those.
        return;
    }
    if !spawn_health.try_admit_probe(now_epoch_secs) {
        return;
    }
    let ready = match work_db.list_ready_executions() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(
                ?err,
                "spawn-capability breaker: failed to list ready executions for recovery probe",
            );
            return;
        }
    };
    // `list_ready_executions` orders by dispatch class then FIFO, so
    // `.first()` would be the fleet's single most urgent row (typically a
    // merge-conflict revision) — sacrificing it repeatedly to a sustained
    // outage's probes would burn the highest-priority work first. `.last()`
    // proves the same thing about the spawn path while spending the least
    // urgent ready row instead.
    let Some(candidate) = ready.last() else {
        // Nothing to probe with yet; try again on the next sweep tick.
        return;
    };
    tracing::warn!(
        execution_id = %candidate.id,
        work_item_id = %candidate.work_item_id,
        "spawn-capability breaker: admitting half-open recovery probe through paused dispatch",
    );
    match coordinator.force_dispatch(&candidate.id).await {
        Ok(worker_id) => {
            spawn_health.mark_probe_dispatched(&candidate.id, now_epoch_secs);
            tracing::info!(
                execution_id = %candidate.id,
                worker_id,
                "spawn-capability breaker: recovery probe dispatched; awaiting a shell-pid \
                 report (success) or a reap (failure, backs off)",
            );
        }
        Err(err) => {
            // Nothing actually reached the app — the row raced out of
            // `ready`, or the pool is at its hard cap — so there's no
            // evidence either way. Don't touch backoff; just retry on the
            // next tick.
            tracing::debug!(
                ?err,
                execution_id = %candidate.id,
                "spawn-capability breaker: recovery probe dispatch attempt did not reach the \
                 app; will retry",
            );
        }
    }
}

/// Auto-resume dispatch after Breaker-origin evidence that the app's spawn
/// path is healthy again — either the half-open recovery probe's canary
/// reported a real shell pid, or a fresh app session registered (the
/// operator's natural recovery action, e.g. relaunching the app after
/// waking the display).
///
/// No-ops when dispatch isn't currently paused, and — critically — when the
/// current pause is [`DispatchPauseOrigin::Operator`]: a human pause stays
/// manual-resume-only regardless of spawn evidence, exactly like
/// `handle_set_dispatch_paused` documents. Returns `true` if dispatch was
/// actually resumed, so the caller knows to re-kick the scheduler and
/// broadcast the updated health state.
pub async fn resume_dispatch_after_breaker_recovery(
    work_db: &WorkDb,
    coordinator: &ExecutionCoordinator,
    dispatch_events: &dyn DispatchEventSink,
    execution_id: Option<&str>,
    reason: &str,
) -> bool {
    if !coordinator.is_dispatch_paused() || coordinator.dispatch_pause_exempts_reviews() {
        return false;
    }
    coordinator.set_dispatch_paused(false, 0, DispatchPauseOrigin::Breaker);
    if let Err(err) = work_db
        .set_metadata(METADATA_KEY_DISPATCH_PAUSED, "0")
        .and_then(|()| work_db.set_metadata(METADATA_KEY_DISPATCH_PAUSED_SINCE, "0"))
    {
        tracing::warn!(
            ?err,
            reason,
            "spawn-capability breaker: failed to persist dispatch auto-resume to state.db — \
             resumed in-memory but will revert on engine restart",
        );
    }
    tracing::warn!(reason, "spawn-capability breaker: auto-resuming dispatch");
    dispatch_events
        .emit(
            DispatchEvent::new(
                Stage::SpawnCapabilityRecovered,
                Outcome::Ok,
                execution_id.unwrap_or("engine"),
            )
            .with_details(serde_json::json!({ "reason": reason })),
        )
        .await;
    true
}

/// The failure that tripped the breaker, bundled so
/// [`trip_spawn_capability_circuit`] stays under the clippy argument-count
/// limit.
pub struct TripSignal<'a> {
    pub tripping_execution_id: &'a str,
    pub tripping_work_item_id: &'a str,
    pub distinct_work_items: usize,
    pub now_epoch_secs: i64,
}

/// Act on a tripped spawn-capability breaker: always log loudly and raise
/// the one attention item; pause dispatch too, but only when
/// [`SpawnHealthTracker::breaker_enabled`] is `true` — see the module docs'
/// "breaker flag" section.
///
/// ## Idempotency
///
/// **Enabled:** idempotent once dispatch is already paused with
/// review-exemption OFF (i.e. a prior breaker trip, or a human pause that
/// has already been escalated) — that is a no-op, so repeated failures
/// while the app is wedged never spam attention items. But an *operator*
/// pause exempts `pr_review` executions
/// ([`ExecutionCoordinator::dispatch_pause_exempts_reviews`]), so if the app
/// spawn path is also broken during an operator pause, reviews would
/// otherwise keep dispatching into the dead path and keep tripping this
/// function forever. In that case this still escalates: it re-pauses with
/// [`DispatchPauseOrigin::Breaker`] (clearing the review exemption) and
/// raises the attention item, rather than skipping as a duplicate.
///
/// **Disabled:** idempotency instead comes from
/// [`SpawnHealthTracker::mark_disabled_trip_signaled`] (dispatch is never
/// paused in this mode, so `coordinator.is_dispatch_paused()` can't serve as
/// the dedup signal) — at most one log line and one attention item per
/// [`SPAWN_HEALTH_WINDOW_SECS`] window, deliberately NOT reset by
/// [`SpawnHealthTracker::record_success`] (see that method's docs) so a
/// flapping spawn path can't spam a fresh attention item on every
/// success/failure cycle.
///
/// ## Persistence (enabled mode only)
///
/// The pause is persisted through the same metadata keys the human toggle
/// (`handle_set_dispatch_paused`) uses, so an engine restart mid-outage does
/// not resume churning. Pauses with [`DispatchPauseOrigin::Breaker`], which —
/// unlike an operator pause — does NOT exempt `pr_review` executions: the
/// app's spawn path itself is broken here, so dispatching a review would
/// just burn another attempt against the same dead path.
pub async fn trip_spawn_capability_circuit(
    work_db: &WorkDb,
    coordinator: &ExecutionCoordinator,
    dispatch_events: &dyn DispatchEventSink,
    spawn_health: &SpawnHealthTracker,
    trip: TripSignal<'_>,
) {
    let TripSignal {
        tripping_execution_id,
        tripping_work_item_id,
        distinct_work_items,
        now_epoch_secs,
    } = trip;
    let breaker_enabled = spawn_health.breaker_enabled();

    if breaker_enabled {
        if coordinator.is_dispatch_paused() && !coordinator.dispatch_pause_exempts_reviews() {
            tracing::debug!(
                tripping_execution_id,
                "spawn-capability breaker: dispatch already paused (non-exempt); skipping duplicate trip",
            );
            return;
        }
        let now_u64 = now_epoch_secs.max(0) as u64;
        coordinator.set_dispatch_paused(true, now_u64, DispatchPauseOrigin::Breaker);
        if let Err(err) = work_db
            .set_metadata(METADATA_KEY_DISPATCH_PAUSED, "1")
            .and_then(|()| work_db.set_metadata(METADATA_KEY_DISPATCH_PAUSED_SINCE, &now_u64.to_string()))
            .and_then(|()| {
                work_db.set_metadata(
                    METADATA_KEY_DISPATCH_PAUSE_ORIGIN,
                    DispatchPauseOrigin::Breaker.as_metadata_str(),
                )
            })
        {
            tracing::warn!(
                ?err,
                "spawn-capability breaker: failed to persist dispatch pause to state.db — \
                 applied in-memory but will revert on engine restart",
            );
        }
    } else if !spawn_health.mark_disabled_trip_signaled(now_epoch_secs) {
        tracing::debug!(
            tripping_execution_id,
            "spawn-capability breaker: disabled by config; already signaled this outage, skipping duplicate",
        );
        return;
    }

    let window_secs = SPAWN_HEALTH_WINDOW_SECS;
    if breaker_enabled {
        tracing::error!(
            distinct_work_items,
            window_secs,
            tripping_execution_id,
            tripping_work_item_id,
            "app spawn capability unhealthy: {distinct_work_items} distinct work items failed to \
             start a worker shell within {window_secs}s; pausing dispatch and raising attention",
        );
    } else {
        tracing::error!(
            distinct_work_items,
            window_secs,
            tripping_execution_id,
            tripping_work_item_id,
            "app spawn capability unhealthy: {distinct_work_items} distinct work items failed to \
             start a worker shell within {window_secs}s; breaker is DISABLED by config \
             (BOSS_ENABLE_SPAWN_CAPABILITY_BREAKER=false) — NOT pausing dispatch, raising attention only",
        );
    }

    let body = if breaker_enabled {
        format!(
            "The Boss app accepted worker-pane spawn requests but no shell ever came up for \
             **{distinct_work_items} different work items** within {window_secs}s — the app session's \
             pane-spawn path is unhealthy (most often `ghostty_surface_new` returning NULL after the \
             machine slept, i.e. no active display).\n\n\
             Dispatch has been **paused** to stop the engine from burning spawn attempts against a \
             dead app path. Each affected execution was reaped (see the `spawn_nack` / \
             `spawn_ack_timeout` events in `dispatch-events/current.jsonl`).\n\n\
             **Recovery is automatic:** the engine periodically force-dispatches a single queued \
             execution as a recovery probe (backing off between attempts) and auto-resumes dispatch \
             the moment one reports a real shell pid — see `spawn_capability_recovered` in \
             `dispatch-events/current.jsonl`. Relaunching the Boss app (e.g. after waking the display) \
             also clears the breaker immediately on reconnect. No manual action is required, but you \
             can still make sure the app is foreground with an active display and confirm new panes \
             spawn, or force it with `bossctl dispatch resume` / the app's dispatch toggle if recovery \
             is taking longer than expected."
        )
    } else {
        format!(
            "The Boss app accepted worker-pane spawn requests but no shell ever came up for \
             **{distinct_work_items} different work items** within {window_secs}s — the app session's \
             pane-spawn path looks unhealthy (most often `ghostty_surface_new` returning NULL after \
             the machine slept, i.e. no active display).\n\n\
             The spawn-capability breaker is **disabled by config** \
             (`BOSS_ENABLE_SPAWN_CAPABILITY_BREAKER=false`, the default) — dispatch was **NOT** \
             paused; this item is observability only. Each affected execution was reaped and will be \
             redispatched normally (see the `spawn_nack` / `spawn_ack_timeout` events in \
             `dispatch-events/current.jsonl`).\n\n\
             If this keeps happening, consider setting `BOSS_ENABLE_SPAWN_CAPABILITY_BREAKER=true` so \
             a systemic outage pauses dispatch (with automatic recovery) instead of relying on the \
             per-work-item churn guard alone."
        )
    };
    if let Err(err) = work_db.create_attention_item(CreateAttentionItemInput {
        body_markdown: body,
        kind: SPAWN_CAPABILITY_ATTENTION_KIND.to_owned(),
        title: "App worker-pane spawn capability is unhealthy".to_owned(),
        execution_id: Some(tripping_execution_id.to_owned()),
        resolved_at: None,
        status: None,
        work_item_id: None,
    }) {
        tracing::warn!(
            ?err,
            tripping_execution_id,
            "spawn-capability breaker: failed to raise attention item",
        );
    }

    dispatch_events
        .emit(
            DispatchEvent::new(Stage::SpawnCapabilityUnhealthy, Outcome::Error, tripping_execution_id)
                .with_work_item(tripping_work_item_id)
                .with_details(serde_json::json!({
                    "distinct_work_items": distinct_work_items,
                    "window_secs": window_secs,
                    "breaker_enabled": breaker_enabled,
                    "dispatch_paused": breaker_enabled,
                })),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trips_when_threshold_distinct_work_items_fail_in_window() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        assert_eq!(
            tracker.record_failure("wi-1", 1000),
            None,
            "1 distinct: below threshold"
        );
        assert_eq!(
            tracker.record_failure("wi-2", 1001),
            None,
            "2 distinct: below threshold"
        );
        assert_eq!(
            tracker.record_failure("wi-3", 1002),
            Some(3),
            "3rd distinct work item trips the breaker"
        );
    }

    #[test]
    fn same_work_item_repeated_does_not_trip() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        // One work item failing many times is a per-item problem for the
        // churn guard, not a systemic spawn-capability outage.
        for t in 0..10 {
            assert_eq!(
                tracker.record_failure("wi-hot", 1000 + t),
                None,
                "a single repeatedly-failing work item must never trip the cross-item breaker",
            );
        }
    }

    #[test]
    fn failures_outside_window_do_not_count() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        // Two failures long ago fall out of the window before the later ones.
        assert_eq!(tracker.record_failure("wi-1", 0), None);
        assert_eq!(tracker.record_failure("wi-2", 1), None);
        // These three land ~1000s later; the first two are pruned, so only
        // wi-3/wi-4 are in-window — 2 distinct, still below threshold.
        assert_eq!(tracker.record_failure("wi-3", 1000), None);
        assert_eq!(
            tracker.record_failure("wi-4", 1001),
            None,
            "stale failures outside the window must not push the distinct count over threshold",
        );
        // A third fresh distinct item now trips.
        assert_eq!(tracker.record_failure("wi-5", 1002), Some(3));
    }

    #[test]
    fn success_resets_the_breaker() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        tracker.record_failure("wi-1", 1000);
        tracker.record_failure("wi-2", 1000);
        tracker.record_success();
        // After a successful spawn the window is empty again, so it takes a
        // fresh threshold's worth of distinct failures to re-trip.
        assert_eq!(tracker.record_failure("wi-3", 1001), None);
        assert_eq!(tracker.record_failure("wi-4", 1001), None);
        assert_eq!(tracker.record_failure("wi-5", 1001), Some(3));
    }

    #[test]
    fn keeps_firing_on_every_failure_once_tripped() {
        // Documented level-triggered behavior: once distinct in-window failures
        // are at/over the threshold, record_failure returns Some on *every*
        // subsequent failure — for a brand-new distinct work item AND for a
        // repeat of an already-counted one. The signal is deduped downstream by
        // trip_spawn_capability_circuit's idempotency, not by record_failure
        // firing only once.
        let tracker = SpawnHealthTracker::with_config(3, 300);
        assert_eq!(tracker.record_failure("wi-1", 1000), None);
        assert_eq!(tracker.record_failure("wi-2", 1001), None);
        assert_eq!(tracker.record_failure("wi-3", 1002), Some(3), "3rd distinct trips");
        // A repeat of an already-counted item re-fires; distinct count is
        // unchanged (still 3 distinct items in-window).
        assert_eq!(
            tracker.record_failure("wi-1", 1003),
            Some(3),
            "repeat of a counted item still re-fires, distinct count unchanged",
        );
        // A brand-new distinct item re-fires with the higher distinct count.
        assert_eq!(
            tracker.record_failure("wi-4", 1004),
            Some(4),
            "a new distinct item re-fires with the incremented distinct count",
        );
        // And a further repeat keeps firing at the current distinct count.
        assert_eq!(
            tracker.record_failure("wi-2", 1005),
            Some(4),
            "still level-triggered on subsequent repeats",
        );
    }

    #[test]
    fn with_config_clamps_zero_to_one() {
        // threshold and window_secs are clamped with .max(1), so with_config(0, 0)
        // behaves as threshold=1 / window=1: a single failure trips immediately.
        let tracker = SpawnHealthTracker::with_config(0, 0);
        assert_eq!(tracker.window_secs(), 1, "window clamped to 1");
        assert_eq!(
            tracker.record_failure("wi-1", 1000),
            Some(1),
            "clamped threshold=1 means a single failure trips",
        );
        // 2s later the 1s window has pruned the earlier failure, so a new item
        // is again the only in-window entry — 1 distinct, which still trips.
        assert_eq!(
            tracker.record_failure("wi-2", 1002),
            Some(1),
            "1s window pruned the earlier failure; new item is the only one in-window",
        );
        // Same reasoning with a repeat of the original item after the window.
        assert_eq!(
            tracker.record_failure("wi-1", 1004),
            Some(1),
            "1s window means even a repeated item is the sole in-window entry",
        );
    }

    #[test]
    fn window_boundary_is_inclusive() {
        // The prune predicate keeps entries where ts >= now - window_secs.
        let tracker = SpawnHealthTracker::with_config(3, 300);
        // Two failures exactly window_secs (300s) before the trip attempt at
        // t=300 are retained (300 >= 300 - 300 == 0), so they count.
        assert_eq!(tracker.record_failure("wi-1", 0), None);
        assert_eq!(tracker.record_failure("wi-2", 0), None);
        assert_eq!(
            tracker.record_failure("wi-3", 300),
            Some(3),
            "entries exactly window_secs old are retained (ts >= now - window_secs)",
        );

        // A fresh tracker: entries window_secs+1 old are pruned. Two failures at
        // t=0, then a trip attempt at t=301 — the two are 301s old (> 300 window)
        // and pruned, leaving only the new one in-window (1 distinct, no trip).
        let tracker = SpawnHealthTracker::with_config(3, 300);
        assert_eq!(tracker.record_failure("wi-1", 0), None);
        assert_eq!(tracker.record_failure("wi-2", 0), None);
        assert_eq!(
            tracker.record_failure("wi-3", 301),
            None,
            "entries window_secs+1 old are pruned (ts < now - window_secs)",
        );
    }

    #[test]
    fn window_secs_accessor_returns_configured_value() {
        assert_eq!(SpawnHealthTracker::with_config(3, 300).window_secs(), 300);
        // Reflects the clamped value when the configured window is below 1.
        assert_eq!(SpawnHealthTracker::with_config(3, 0).window_secs(), 1);
    }

    // ─── half-open recovery probe ──────────────────────────────────────────

    #[test]
    fn probe_admits_immediately_after_a_fresh_trip() {
        // A brand-new tracker has no probe history, so the very first probe
        // is eligible right away — no artificial delay before the breaker's
        // first recovery attempt.
        let tracker = SpawnHealthTracker::with_config(3, 300);
        assert!(tracker.try_admit_probe(0));
    }

    #[test]
    fn mark_probe_dispatched_blocks_further_admission() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        assert!(tracker.try_admit_probe(1000));
        tracker.mark_probe_dispatched("exec-1", 1000);
        // Only one probe in flight at a time, within the stall deadline.
        assert!(!tracker.try_admit_probe(1000));
        assert!(!tracker.try_admit_probe(1000 + SPAWN_HEALTH_PROBE_STALL_DEADLINE_SECS - 1));
        assert!(tracker.is_probe_execution("exec-1"));
        assert!(!tracker.is_probe_execution("exec-2"));
    }

    #[test]
    fn stalled_probe_is_treated_as_failed_after_the_deadline() {
        // Regression: a canary that terminates by some route other than
        // record_probe_success/record_probe_failure (e.g. the detached spawn
        // task's adapter.spawn_worker erroring, which flips the execution
        // straight to terminal `failed` with no live slot for either reap
        // path to catch) must not leak `in_flight` forever and permanently
        // re-latch dispatch. try_admit_probe itself is the backstop.
        let tracker = SpawnHealthTracker::with_config(3, 300);
        tracker.mark_probe_dispatched("exec-1", 1000);
        assert!(!tracker.try_admit_probe(1000 + SPAWN_HEALTH_PROBE_STALL_DEADLINE_SECS - 1));

        // At the deadline the stalled probe is cleared and treated exactly
        // like a normal `record_probe_failure` — including the base backoff —
        // so admission isn't immediate; it lands one base-backoff later.
        let stall_hit_at = 1000 + SPAWN_HEALTH_PROBE_STALL_DEADLINE_SECS;
        assert!(
            !tracker.try_admit_probe(stall_hit_at),
            "the normal post-failure backoff still applies once the stalled probe is cleared",
        );
        assert!(!tracker.is_probe_execution("exec-1"), "the stalled probe was cleared");
        assert!(
            tracker.try_admit_probe(stall_hit_at + SPAWN_HEALTH_PROBE_BACKOFF_BASE_SECS),
            "a probe stuck in-flight past the stall deadline must eventually admit a new one",
        );
    }

    #[test]
    fn probe_failure_clears_in_flight_and_backs_off() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        tracker.mark_probe_dispatched("exec-1", 1000);
        assert!(!tracker.try_admit_probe(1000));

        tracker.record_probe_failure("exec-1", 1000);
        assert!(
            !tracker.is_probe_execution("exec-1"),
            "a failed probe is no longer in flight"
        );
        // Base backoff (60s) applies after the first failure.
        assert!(!tracker.try_admit_probe(1059));
        assert!(tracker.try_admit_probe(1060));
    }

    #[test]
    fn probe_failure_backoff_grows_exponentially_and_caps() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        let mut now = 0i64;
        let expected_backoffs = [60, 120, 240, 480, 900, 900];
        for &backoff in &expected_backoffs {
            tracker.mark_probe_dispatched("exec-1", now);
            tracker.record_probe_failure("exec-1", now);
            assert!(
                !tracker.try_admit_probe(now + backoff - 1),
                "backoff={backoff} at now={now}"
            );
            assert!(tracker.try_admit_probe(now + backoff), "backoff={backoff} at now={now}");
            now += backoff;
        }
    }

    #[test]
    fn probe_failure_for_a_different_execution_is_a_no_op() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        tracker.mark_probe_dispatched("exec-1", 1000);
        // An unrelated reap (e.g. a normal orphaned execution during the
        // same outage) must not disturb this probe's own schedule.
        tracker.record_probe_failure("exec-unrelated", 1000);
        assert!(
            tracker.is_probe_execution("exec-1"),
            "unrelated failure left the real probe untouched"
        );
        assert!(
            !tracker.try_admit_probe(1000),
            "no backoff was applied by the unrelated call"
        );
    }

    #[test]
    fn probe_success_resets_state_and_only_for_the_matching_execution() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        tracker.mark_probe_dispatched("exec-1", 1000);
        assert!(
            !tracker.record_probe_success("exec-other"),
            "success report for a non-probe execution does not complete the probe"
        );
        assert!(tracker.is_probe_execution("exec-1"), "still in flight");

        assert!(tracker.record_probe_success("exec-1"));
        assert!(!tracker.is_probe_execution("exec-1"));
        assert!(
            tracker.try_admit_probe(0),
            "a completed probe cycle starts the next one fresh"
        );
    }

    #[test]
    fn probe_success_clears_accumulated_backoff() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        tracker.mark_probe_dispatched("exec-1", 1000);
        tracker.record_probe_failure("exec-1", 1000);
        assert!(!tracker.try_admit_probe(1000), "backed off after a failure");

        tracker.mark_probe_dispatched("exec-2", 1000);
        assert!(tracker.record_probe_success("exec-2"));
        // No leftover backoff from the prior failed attempt.
        assert!(tracker.try_admit_probe(1000));
    }

    #[test]
    fn reset_probe_clears_in_flight_and_backoff_unconditionally() {
        let tracker = SpawnHealthTracker::with_config(3, 300);
        tracker.mark_probe_dispatched("exec-1", 1000);
        tracker.record_probe_failure("exec-1", 1000);
        tracker.mark_probe_dispatched("exec-2", 1000);
        assert!(!tracker.try_admit_probe(1000));

        tracker.reset_probe();
        assert!(tracker.try_admit_probe(1000));
        assert!(!tracker.is_probe_execution("exec-2"));
    }

    // ─── maybe_admit_recovery_probe / resume_dispatch_after_breaker_recovery ──

    use crate::dispatch_events::{NoopDispatchEventSink, RecordingDispatchEventSink};
    use crate::test_support::*;
    use crate::work::ExecutionStatus;

    #[tokio::test]
    async fn probe_dispatches_a_ready_execution_when_breaker_paused() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);
        let execution = create_ready_chore_execution(&db, &work_item_id);

        // Needs a coordinator that can actually carry a dispatch through —
        // the probe force-dispatches a real ready execution, not just
        // selects one.
        let coordinator = make_dispatchable_coordinator(db.clone(), 4);
        coordinator.set_dispatch_paused(true, 0, DispatchPauseOrigin::Breaker);

        let spawn_health = SpawnHealthTracker::new();
        maybe_admit_recovery_probe(&db, &coordinator, &spawn_health, 1000).await;

        assert!(
            spawn_health.is_probe_execution(&execution.id),
            "the only ready execution should have been admitted as the canary"
        );
        let reloaded = db.get_execution(&execution.id).unwrap();
        assert_ne!(
            reloaded.status,
            ExecutionStatus::Ready,
            "the canary was actually force-dispatched, not just selected"
        );
    }

    #[tokio::test]
    async fn probe_is_not_admitted_when_dispatch_is_not_paused() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);
        let execution = create_ready_chore_execution(&db, &work_item_id);

        let coordinator = make_coordinator(db.clone(), 4);
        // Dispatch is running normally — no breaker trip in play.

        let spawn_health = SpawnHealthTracker::new();
        maybe_admit_recovery_probe(&db, &coordinator, &spawn_health, 1000).await;

        assert!(!spawn_health.is_probe_execution(&execution.id));
        let reloaded = db.get_execution(&execution.id).unwrap();
        assert_eq!(
            reloaded.status,
            ExecutionStatus::Ready,
            "untouched — normal dispatch owns this row"
        );
    }

    #[tokio::test]
    async fn probe_is_never_admitted_under_an_operator_pause() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);
        let execution = create_ready_chore_execution(&db, &work_item_id);

        let coordinator = make_coordinator(db.clone(), 4);
        // An operator-originated pause must stay manual-resume-only — the
        // half-open probe is scoped strictly to Breaker-origin pauses.
        coordinator.set_dispatch_paused(true, 0, DispatchPauseOrigin::Operator);

        let spawn_health = SpawnHealthTracker::new();
        maybe_admit_recovery_probe(&db, &coordinator, &spawn_health, 1000).await;

        assert!(!spawn_health.is_probe_execution(&execution.id));
        let reloaded = db.get_execution(&execution.id).unwrap();
        assert_eq!(reloaded.status, ExecutionStatus::Ready);
    }

    #[tokio::test]
    async fn probe_is_not_re_admitted_while_one_is_already_in_flight() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);
        let first = create_ready_chore_execution(&db, &work_item_id);
        let second_item = create_active_chore(&db, &product_id, "second chore");
        let second = create_ready_chore_execution(&db, &second_item);

        let coordinator = make_dispatchable_coordinator(db.clone(), 4);
        coordinator.set_dispatch_paused(true, 0, DispatchPauseOrigin::Breaker);

        // The canary is picked from the *least* urgent ready row (`.last()`)
        // so a sustained outage's probes don't repeatedly burn the fleet's
        // highest-priority work — `second` (created later, so ordered last
        // by the FIFO tiebreak) is the one admitted, not `first`.
        let spawn_health = SpawnHealthTracker::new();
        maybe_admit_recovery_probe(&db, &coordinator, &spawn_health, 1000).await;
        assert!(spawn_health.is_probe_execution(&second.id));

        // A second tick while the first probe is unresolved must not admit
        // the other ready execution too.
        maybe_admit_recovery_probe(&db, &coordinator, &spawn_health, 1001).await;
        assert!(spawn_health.is_probe_execution(&second.id), "still the original probe");
        let first_reloaded = db.get_execution(&first.id).unwrap();
        assert_eq!(
            first_reloaded.status,
            ExecutionStatus::Ready,
            "held — only one probe at a time"
        );
    }

    #[tokio::test]
    async fn resume_after_breaker_recovery_resumes_a_breaker_pause() {
        let (_dir, db) = open_db_arc();
        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.set_dispatch_paused(true, 1000, DispatchPauseOrigin::Breaker);

        let sink = RecordingDispatchEventSink::new();
        let resumed =
            resume_dispatch_after_breaker_recovery(&db, &coordinator, &sink, Some("exec-1"), "test recovery").await;

        assert!(resumed);
        assert!(!coordinator.is_dispatch_paused());
        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "spawn_capability_recovered");
        assert_eq!(events[0].execution_id, "exec-1");
    }

    #[tokio::test]
    async fn resume_after_breaker_recovery_never_touches_an_operator_pause() {
        let (_dir, db) = open_db_arc();
        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.set_dispatch_paused(true, 1000, DispatchPauseOrigin::Operator);

        let sink = NoopDispatchEventSink;
        let resumed = resume_dispatch_after_breaker_recovery(&db, &coordinator, &sink, None, "test recovery").await;

        assert!(!resumed, "an operator pause is manual-resume-only");
        assert!(coordinator.is_dispatch_paused());
    }

    #[tokio::test]
    async fn resume_after_breaker_recovery_is_a_noop_when_not_paused() {
        let (_dir, db) = open_db_arc();
        let coordinator = make_coordinator(db.clone(), 1);

        let sink = NoopDispatchEventSink;
        let resumed = resume_dispatch_after_breaker_recovery(&db, &coordinator, &sink, None, "test recovery").await;

        assert!(!resumed);
    }

    // ─── breaker flag: disabled mode observes but never pauses ────────────

    #[tokio::test]
    async fn disabled_breaker_raises_attention_but_never_pauses_dispatch() {
        let (_dir, db) = open_db_arc();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution = create_ready_chore_execution(&db, &work_item_id);
        let coordinator = make_coordinator(db.clone(), 1);
        assert!(!coordinator.is_dispatch_paused(), "precondition");

        // `with_config` now defaults to enabled (matching production), so
        // set it explicitly here since this test is specifically about the
        // flag-off path.
        let spawn_health = SpawnHealthTracker::with_config(3, 300).with_breaker_enabled(false);
        let sink = RecordingDispatchEventSink::new();

        trip_spawn_capability_circuit(
            &db,
            &coordinator,
            &sink,
            &spawn_health,
            TripSignal {
                tripping_execution_id: &execution.id,
                tripping_work_item_id: &work_item_id,
                distinct_work_items: 3,
                now_epoch_secs: 1000,
            },
        )
        .await;

        assert!(
            !coordinator.is_dispatch_paused(),
            "a disabled breaker must never pause dispatch"
        );
        let events = sink.events().await;
        let unhealthy: Vec<_> = events
            .iter()
            .filter(|e| e.stage == "spawn_capability_unhealthy")
            .collect();
        assert_eq!(unhealthy.len(), 1, "observability event still fires");
        assert_eq!(unhealthy[0].details["breaker_enabled"], serde_json::json!(false));
        assert_eq!(unhealthy[0].details["dispatch_paused"], serde_json::json!(false));

        let attn = db.list_attention_items(&execution.id).unwrap();
        assert!(
            attn.iter().any(|a| a.kind == SPAWN_CAPABILITY_ATTENTION_KIND),
            "the attention item still fires even though dispatch was not paused",
        );
    }

    #[tokio::test]
    async fn disabled_breaker_signals_once_per_window_and_ignores_intervening_success() {
        let (_dir, db) = open_db_arc();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution = create_ready_chore_execution(&db, &work_item_id);
        let coordinator = make_coordinator(db.clone(), 1);

        let spawn_health = SpawnHealthTracker::with_config(3, 300).with_breaker_enabled(false);
        let sink = RecordingDispatchEventSink::new();

        trip_spawn_capability_circuit(
            &db,
            &coordinator,
            &sink,
            &spawn_health,
            TripSignal {
                tripping_execution_id: &execution.id,
                tripping_work_item_id: &work_item_id,
                distinct_work_items: 3,
                now_epoch_secs: 1000,
            },
        )
        .await;
        trip_spawn_capability_circuit(
            &db,
            &coordinator,
            &sink,
            &spawn_health,
            TripSignal {
                tripping_execution_id: &execution.id,
                tripping_work_item_id: &work_item_id,
                distinct_work_items: 4,
                now_epoch_secs: 1001,
            },
        )
        .await;
        let events = sink.events().await;
        assert_eq!(
            events
                .iter()
                .filter(|e| e.stage == "spawn_capability_unhealthy")
                .count(),
            1,
            "still-tripped repeats within the same outage must not spam the signal",
        );

        // Regression: in disabled mode dispatch never pauses, so spawns keep
        // flowing and `record_success` can fire in between failure bursts of
        // a flapping outage. That success must NOT re-arm the signal window —
        // a re-trip 1s later (still inside the 300s window) must stay
        // suppressed, or a flapping spawn path would raise a fresh durable
        // attention item on every burst.
        spawn_health.record_success();
        trip_spawn_capability_circuit(
            &db,
            &coordinator,
            &sink,
            &spawn_health,
            TripSignal {
                tripping_execution_id: &execution.id,
                tripping_work_item_id: &work_item_id,
                distinct_work_items: 3,
                now_epoch_secs: 1002,
            },
        )
        .await;
        let events = sink.events().await;
        assert_eq!(
            events
                .iter()
                .filter(|e| e.stage == "spawn_capability_unhealthy")
                .count(),
            1,
            "an intervening success must not re-arm the signal window while still inside it",
        );

        // Once the window has genuinely elapsed, a fresh outage gets its own
        // attention item regardless of whether `record_success` fired.
        trip_spawn_capability_circuit(
            &db,
            &coordinator,
            &sink,
            &spawn_health,
            TripSignal {
                tripping_execution_id: &execution.id,
                tripping_work_item_id: &work_item_id,
                distinct_work_items: 3,
                now_epoch_secs: 2000,
            },
        )
        .await;
        let events = sink.events().await;
        assert_eq!(
            events
                .iter()
                .filter(|e| e.stage == "spawn_capability_unhealthy")
                .count(),
            2,
            "a new outage after the window elapses gets its own signal",
        );
    }

    #[tokio::test]
    async fn enabled_breaker_still_pauses_dispatch_on_trip() {
        let (_dir, db) = open_db_arc();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution = create_ready_chore_execution(&db, &work_item_id);
        let coordinator = make_coordinator(db.clone(), 1);

        let spawn_health = SpawnHealthTracker::with_config(3, 300).with_breaker_enabled(true);
        let sink = RecordingDispatchEventSink::new();

        trip_spawn_capability_circuit(
            &db,
            &coordinator,
            &sink,
            &spawn_health,
            TripSignal {
                tripping_execution_id: &execution.id,
                tripping_work_item_id: &work_item_id,
                distinct_work_items: 3,
                now_epoch_secs: 1000,
            },
        )
        .await;

        assert!(
            coordinator.is_dispatch_paused(),
            "enabled mode still pauses dispatch on trip"
        );
        let events = sink.events().await;
        let unhealthy = &events[0];
        assert_eq!(unhealthy.details["breaker_enabled"], serde_json::json!(true));
        assert_eq!(unhealthy.details["dispatch_paused"], serde_json::json!(true));
    }
}
