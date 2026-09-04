//! Restart-robust "is this worker's process still running?" probe, keyed by
//! execution id and sourced entirely from durable state.
//!
//! ## Why this exists
//!
//! Every liveness decision in this crate used to read one of two in-memory
//! structures: [`crate::live_worker_state::LiveWorkerStateRegistry`] (the
//! per-slot live state) or the [`crate::coordinator::WorkerPool`]'s claim
//! table. Both are *derived* bookkeeping — the engine's belief about what is
//! running — and both are cleared unconditionally at the end of
//! [`crate::app::ServerState::release_worker_pane`], which runs on every
//! terminal path "successfully or not".
//!
//! That is fine while the belief is right. It is catastrophic when the belief
//! is wrong, because every consumer of the belief then agrees with it. The
//! 2026-07-28 incident is the shape: eight `claude` workers alive, two visible
//! to the engine, six executions terminalized seconds after start while their
//! processes kept working for another nine minutes. The re-dispatchers asked
//! the pool "is this execution claimed?", got `false` (correct — the claim had
//! been released), and dispatched a second worker onto a row whose first worker
//! was still running. Three workers ended up on one chore, two on another.
//!
//! The engine already persists the one fact that would have settled it:
//! `work_runs.shell_pid`, written by
//! [`crate::work::WorkDb::set_run_shell_pid_for_execution`] the moment the
//! app reports the pane's shell pid. It survives an engine restart, it
//! survives `release_worker_pane` clearing the registry, and it survives the
//! execution row going terminal.
//!
//! This module owns that probe so every path consults reality through one
//! implementation instead of the engine's own opinion — or its own private
//! copy of the pid semantics. Every caller goes through here:
//! [`crate::dead_pane_sweep`], the re-dispatch guard in
//! [`crate::orphan_sweep`], the re-adoption path, the husk classifier,
//! `release_worker_pane`'s reap, and [`crate::stale_worker_sweep`] when a
//! tmux session is absent or its spawn token no longer matches.
//!
//! ## Two entry points, and which one to reach for
//!
//! [`probe_execution_worker`] answers from a run row of any age;
//! [`probe_execution_worker_within`] (and the work-item-scoped
//! [`probe_work_item_worker`]) refuse to answer once the row falls outside
//! [`REDISPATCH_PID_TRUST_SECS`]. **Anything that signals a process, or that
//! restores state from the pid, must use a bounded form.** A pid is a durable
//! number, not a durable handle: once the OS recycles it the same integer names
//! an unrelated process, and the reap signals a process *group*. The unbounded
//! form is for callers whose failure direction is to decline to act, where a
//! stale pid costs a missed reconciliation rather than a killed bystander.
//!
//! ## What it is not
//!
//! This answers "does a process with the recorded pid exist", NOT "is the
//! worker healthy" or "is the worker still doing useful work". A pane whose
//! `claude` exited but whose host shell lingers reports [`WorkerProcess::Alive`]
//! here, exactly as it does for [`crate::husk_pane_sweep::live_process_evidence`].
//! That is the correct bias for *this* module's callers, which are all deciding
//! whether it is safe to start a SECOND worker or to tear a pane down — both
//! irreversible in the direction that matters. A caller that needs "alive AND
//! working" must corroborate with hook recency on top, the way
//! `live_process_evidence` does.

use crate::dead_pid_sweep::PidStatus;
use crate::live_worker_state::{LiveWorkerStateRegistry, iso8601_utc};
use crate::work::WorkDb;

/// How far back a recorded `work_runs.shell_pid` is trusted by the callers
/// that act on it: the re-dispatch guard ([`crate::orphan_sweep`], via
/// [`probe_work_item_worker`]) and the durable-pid reap in
/// [`crate::app::ServerState::release_worker_pane`] (via
/// [`probe_execution_worker_within`]).
///
/// A pid is only meaningful until the OS recycles it, so a probe against an
/// arbitrarily old row is not evidence — it is a coin flip that gets more
/// biased the longer you wait. One hour is chosen against the failure it
/// guards: the re-dispatch storm fires ~60–90 s after a run is terminalized
/// (`ORPHAN_MIN_AGE_SECS` plus one sweep interval), so an hour is two orders
/// of magnitude of headroom for the case that matters while keeping the
/// recycling window small. macOS pids wrap at ~99999, so the bound is not
/// theoretical: a days-old row can easily name a process that has nothing to
/// do with Boss, and the reap signals a process *group*.
///
/// A worker still running an hour after its run row was last touched is a
/// different (and much louder) problem that belongs to
/// [`crate::stale_worker_sweep`] and [`crate::husk_pane_sweep`], not to a
/// guard whose only job is "don't double-dispatch onto a live process".
pub const REDISPATCH_PID_TRUST_SECS: i64 = 3600;

/// A `Gone` (`kill(pid, 0) == ESRCH`) verdict from a durable-pid probe is
/// only trusted if the execution has ALSO gone quiet for at least this long.
/// A hook event newer than this window (or a tool in flight whose last
/// in-execution hook is still within
/// [`crate::stale_worker_sweep::DEFAULT_STALE_THRESHOLD_SECS`]) proves the
/// worker's process tree is alive regardless of what the *tracked* pid — a
/// possibly-transient or reused snapshot — reports, so the verdict is
/// downgraded to `Alive` rather than trusted (the shell-pid false-reap fix,
/// generalized: see [`corroborating_liveness`]). 120 s is comfortably above
/// the ~10-30 s hook cadence a working worker shows, so a live worker is
/// never in danger, yet an order of magnitude below
/// [`crate::stale_worker_sweep`]'s 30-min threshold, so a genuinely dead
/// worker still ages out of the window and gets reaped on a later pass. This
/// is a *corroboration* window, not a longer reap timer: it only ever
/// *prevents* acting on a demonstrably-live execution — lengthening the
/// underlying reap/redispatch interval instead would still act on live
/// workers, which is the opposite of what this guards.
pub const CORROBORATION_WINDOW_SECS: i64 = 120;

/// Corroborate a `Gone` pid verdict against `execution_id`'s recent
/// in-execution activity, as recorded by the (in-memory, NOT
/// restart-robust) [`LiveWorkerStateRegistry`]. Returns `Some(reason)` when
/// the execution is demonstrably alive — meaning a caller about to reap or
/// block a redispatch on a bare `Gone` verdict must not — naming the
/// contradicting signal for the log, and `None` when nothing contradicts the
/// dead verdict.
///
/// This is the **single** implementation of the corroboration check —
/// originally private to [`crate::dead_pid_sweep`] (the shell-pid
/// false-reap fix), lifted here so every durable-pid probe caller
/// ([`crate::dead_pid_sweep`], [`crate::dead_pane_sweep`], and the
/// re-dispatch guard in [`crate::orphan_sweep`]) applies the identical rule
/// instead of each carrying its own (and, historically, only one of them
/// actually doing so).
///
/// Only activity attributable to *this* execution counts: a recycled slot
/// can carry a *prior* run's `last_event_at`, and a timestamp predating this
/// execution's own `started_at` cannot be one of its events. Gating on `>=
/// started_at` means a genuinely dead worker with a stale prior-run
/// timestamp is still reaped, while a live worker with flowing events is
/// spared.
pub fn corroborating_liveness(
    live_states: &LiveWorkerStateRegistry,
    execution_id: &str,
    started_epoch: i64,
    now_epoch_secs: i64,
) -> Option<String> {
    let started_iso = iso8601_utc(started_epoch);
    // A hook whose timestamp predates this execution's start belongs to a
    // prior run on a recycled slot — not evidence of *this* worker's life.
    let last_event_at = live_states.last_event_at_for_run(execution_id);
    let in_execution_event = last_event_at.as_deref().filter(|e| *e >= started_iso.as_str());

    // A tool in flight (an unbalanced PreToolUse) means the worker is
    // legitimately busy — most importantly a long foreground `bazel build`
    // that emits no hook for many minutes. Bound the spare by the same
    // ceiling [`crate::stale_worker_sweep`] uses for "no progress": an
    // unbalanced PreToolUse whose last in-execution hook is older than that
    // is no longer treated as corroboration. Without the bound, a worker
    // killed mid-tool (activity still `Working`, `current_tool` still set)
    // would be spared forever by every durable-pid consumer — including
    // [`crate::dead_pane_sweep`], whose whole reason to exist is the deaths
    // the app never reports via `reap_reported_pane_death`. Past the ceiling
    // the `Gone` verdict stands and a later pass reaps the row;
    // `stale_worker_sweep` itself skips while a tool is in flight, so this
    // bound is what clears that shape.
    let tool_in_flight_cutoff = iso8601_utc(now_epoch_secs - crate::stale_worker_sweep::DEFAULT_STALE_THRESHOLD_SECS);
    if let Some(tool) = live_states.current_tool_for_run(execution_id)
        && let Some(event) = in_execution_event
        && event >= tool_in_flight_cutoff.as_str()
    {
        return Some(format!("tool `{tool}` in flight (last hook {event})"));
    }

    // A hook within the corroboration window proves the process tree is
    // alive whatever the pid probe says.
    let recent_cutoff = iso8601_utc(now_epoch_secs - CORROBORATION_WINDOW_SECS);
    if let Some(event) = in_execution_event
        && event >= recent_cutoff.as_str()
    {
        return Some(format!("hook event at {event} within {CORROBORATION_WINDOW_SECS}s"));
    }

    None
}

/// Wrap a raw [`WorkerProcess`] verdict with corroboration against the live
/// registry: a `Gone` verdict is downgraded to `Alive` when
/// [`corroborating_liveness`] finds contradicting recent activity, so a
/// caller that only ever acts on `Gone` (a reaper) or only ever declines to
/// act on `Alive` (the redispatch guard) gets the safe answer without
/// duplicating the check. `Alive` and `Unknown` verdicts pass through
/// unchanged — corroboration only ever pulls a verdict *toward* "alive",
/// never away from it.
///
/// Returns the (possibly-adjusted) verdict plus the corroboration reason,
/// when one applied, for callers that want to log why a `Gone` probe was
/// not trusted.
pub fn corroborate_against_live_registry(
    process: WorkerProcess,
    live_states: &LiveWorkerStateRegistry,
    execution_id: &str,
    started_epoch: i64,
    now_epoch_secs: i64,
) -> (WorkerProcess, Option<String>) {
    let WorkerProcess::Gone { shell_pid } = process else {
        return (process, None);
    };
    match corroborating_liveness(live_states, execution_id, started_epoch, now_epoch_secs) {
        Some(reason) => (WorkerProcess::Alive { shell_pid }, Some(reason)),
        None => (WorkerProcess::Gone { shell_pid }, None),
    }
}

/// Verdict on the worker process behind an execution, derived from durable
/// state only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerProcess {
    /// A local pid was recorded and `kill(pid, 0)` says a process with that
    /// id exists (either signalable by us, or `EPERM` — present but owned by
    /// someone else).
    Alive { shell_pid: i32 },
    /// A local pid was recorded and `kill(pid, 0)` returned `ESRCH`: the
    /// process is definitively gone. The pid is carried so a caller that
    /// reconciles on death (`crate::dead_pane_sweep`) can name it in its
    /// reason string and dispatch-event details.
    Gone { shell_pid: i32 },
    /// No usable evidence: no pid was ever recorded, the run was remote, the
    /// row aged out of the trust window, or the probe itself failed with an
    /// unexpected errno. Callers MUST treat this as "I don't know", never as
    /// "gone" — it is the state a mid-spawn worker legitimately occupies.
    Unknown,
}

impl WorkerProcess {
    /// `true` only for positive evidence that a process exists.
    pub fn is_alive(self) -> bool {
        matches!(self, WorkerProcess::Alive { .. })
    }

    /// The pid that was probed, whatever the verdict — `None` only for
    /// [`WorkerProcess::Unknown`], where no usable pid existed to probe.
    /// For logs and event details; a caller deciding whether to *signal*
    /// wants [`Self::alive_pid`].
    pub fn shell_pid(self) -> Option<i32> {
        match self {
            WorkerProcess::Alive { shell_pid } | WorkerProcess::Gone { shell_pid } => Some(shell_pid),
            WorkerProcess::Unknown => None,
        }
    }

    /// The recorded pid only when the process is positively alive.
    ///
    /// This is the accessor for anything destructive or state-restoring: a
    /// `Gone` pid must never be signalled (it names a slot the OS is free to
    /// have handed to someone else) nor stamped onto a re-adopted live state.
    pub fn alive_pid(self) -> Option<i32> {
        match self {
            WorkerProcess::Alive { shell_pid } => Some(shell_pid),
            _ => None,
        }
    }

    /// Stable identifier folded into dispatch-event details and trace fields
    /// so a recurrence is attributable in one read.
    pub fn reason(self) -> &'static str {
        match self {
            WorkerProcess::Alive { .. } => "process_alive",
            WorkerProcess::Gone { .. } => "process_gone",
            WorkerProcess::Unknown => "process_unknown",
        }
    }
}

/// Pure classifier: turn a recorded pid plus a probe result into a verdict.
///
/// Split from [`probe_execution_worker`] so the decision is unit-testable
/// without spawning real processes, mirroring
/// [`crate::husk_pane_sweep::live_process_evidence_with`].
///
/// `PermissionDenied` (`EPERM`) counts as [`WorkerProcess::Alive`]: the
/// process demonstrably exists, we simply may not signal it. Reading it as
/// "gone" is the failure direction that double-dispatches onto a live worker.
/// Crate-visible because [`PidStatus`] is: the injected-probe form is an
/// internal testing seam, while the DB-backed [`probe_execution_worker`] /
/// [`probe_work_item_worker`] entry points stay `pub`.
pub(crate) fn classify_worker_process(shell_pid: Option<i64>, status: &PidStatus) -> WorkerProcess {
    // Guard before trusting the value: `kill(0, 0)` signals the caller's own
    // process group and a negative pid signals an arbitrary one, so neither is
    // a probe we would ever want to have run.
    let Some(pid) = shell_pid.filter(|pid| *pid > 0) else {
        return WorkerProcess::Unknown;
    };
    let Ok(pid) = i32::try_from(pid) else {
        return WorkerProcess::Unknown;
    };
    match status {
        PidStatus::Alive | PidStatus::PermissionDenied => WorkerProcess::Alive { shell_pid: pid },
        PidStatus::Dead => WorkerProcess::Gone { shell_pid: pid },
        PidStatus::Unknown(_) => WorkerProcess::Unknown,
    }
}

/// Probe the worker process behind `execution_id` using the pid persisted on
/// its latest LOCAL run.
///
/// Restart-robust and terminal-safe: it reads `work_runs`, so it works after
/// an engine restart wiped the in-memory registry AND after the execution row
/// went terminal — the two cases where every registry-driven check returns
/// "nothing here".
pub fn probe_execution_worker(work_db: &WorkDb, execution_id: &str) -> WorkerProcess {
    let shell_pid = match work_db.latest_local_shell_pid_for_execution(execution_id) {
        Ok(pid) => pid,
        Err(err) => {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "durable liveness: failed to read shell pid; treating as unknown",
            );
            return WorkerProcess::Unknown;
        }
    };
    probe_recorded_pid(shell_pid)
}

/// [`probe_execution_worker`], but refusing to answer from a run row the pid
/// table can no longer vouch for.
///
/// Same read as the unbounded probe, plus the pid-reuse bound
/// [`probe_work_item_worker`] already applies: a row whose most recent
/// timestamp (`finished_at` when the run has ended, else `created_at`) is
/// further back than `max_age_secs` yields [`WorkerProcess::Unknown`] rather
/// than a verdict.
///
/// **Use this, not [`probe_execution_worker`], for anything destructive.**
/// A recorded pid is a durable *number*, not a durable *handle*: once the OS
/// recycles it, the same integer names an unrelated process, and the reap in
/// [`crate::app::ServerState::release_worker_pane`] signals a process
/// **group**. The read-only callers (the husk classifier's
/// `durable_live_process_evidence`, `bossctl`-facing state) keep the unbounded
/// form on purpose: their failure direction is "decline to act", so a stale
/// pid there costs a missed retirement, never a killed bystander.
///
/// `finished_at` rather than `created_at` is the anchor because the window
/// bounds *pid staleness*, not run duration: a worker that has been running
/// for six hours and was wrongly terminalized one minute ago is exactly the
/// case this path exists to reap, and anchoring on `created_at` would exempt
/// it. `finished_at` is written by the teardown that lost track of the worker,
/// so it dates the engine's last first-hand knowledge of the process.
pub fn probe_execution_worker_within(
    work_db: &WorkDb,
    execution_id: &str,
    max_age_secs: i64,
    now_epoch_secs: i64,
) -> WorkerProcess {
    let shell_pid =
        match work_db.latest_local_shell_pid_for_execution_within(execution_id, max_age_secs, now_epoch_secs) {
            Ok(pid) => pid,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    error = %format!("{err:#}"),
                    "durable liveness: failed to read shell pid within trust window; treating as unknown",
                );
                return WorkerProcess::Unknown;
            }
        };
    probe_recorded_pid(shell_pid)
}

/// The newest local worker process recorded against `work_item_id` within
/// [`REDISPATCH_PID_TRUST_SECS`], and whether it is still running.
///
/// Returns `None` when the item has no trustworthy recorded pid at all. When
/// it does, the returned execution id names the run that owns the process —
/// which for the failure this guards is a run the engine has ALREADY
/// terminalized, so callers must not assume it is non-terminal.
pub fn probe_work_item_worker(
    work_db: &WorkDb,
    work_item_id: &str,
    now_epoch_secs: i64,
) -> Option<(String, WorkerProcess)> {
    let recorded = match work_db.latest_local_worker_process_for_work_item(
        work_item_id,
        REDISPATCH_PID_TRUST_SECS,
        now_epoch_secs,
    ) {
        Ok(row) => row,
        Err(err) => {
            tracing::warn!(
                work_item_id,
                error = %format!("{err:#}"),
                "durable liveness: failed to read work item worker process; treating as absent",
            );
            None
        }
    };
    let (execution_id, shell_pid) = recorded?;
    Some((execution_id, probe_recorded_pid(Some(shell_pid))))
}

pub(crate) fn probe_recorded_pid(shell_pid: Option<i64>) -> WorkerProcess {
    let Some(pid) = shell_pid.filter(|pid| *pid > 0) else {
        return WorkerProcess::Unknown;
    };
    let Ok(pid) = i32::try_from(pid) else {
        return WorkerProcess::Unknown;
    };
    classify_worker_process(Some(i64::from(pid)), &crate::dead_pid_sweep::probe_pid(pid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use boss_protocol::WorkerEvent;

    /// A pid that is guaranteed not to exist: `kill(pid, 0)` returns `ESRCH`.
    /// Mirrors the same helper in `dead_pid_sweep`'s tests.
    fn dead_pid() -> i64 {
        // Well above any plausible live pid on macOS/Linux test hosts.
        4_194_303
    }

    // ─── corroborating_liveness / corroborate_against_live_registry ────────

    /// The 20-same-tick incident replay, condensed to a fixture: a worker
    /// whose tracked shell pid probes dead (`Gone`) while it has emitted a
    /// hook well within the corroboration window must not be treated as
    /// dead — this is the exact race from the "live workers false-reaped as
    /// orphaned" incident, where `dead_pane_sweep`'s uncorroborated probe and
    /// `dead_pid_sweep`'s corroborated one disagreed 45ms apart on the same
    /// execution.
    #[test]
    fn recent_hook_corroborates_a_gone_verdict_as_alive() {
        let live_states = LiveWorkerStateRegistry::new();
        live_states.register_spawn(1, "exec-1", "claude-opus-4-7", 12345, None);
        live_states.apply_event(
            1,
            &WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );
        live_states.apply_event(
            1,
            &WorkerEvent::PostToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
                tool_response: serde_json::json!({}),
            },
        );

        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let started_epoch = now - 300;
        assert!(
            corroborating_liveness(&live_states, "exec-1", started_epoch, now).is_some(),
            "a hook emitted moments ago must corroborate liveness",
        );

        let (process, reason) = corroborate_against_live_registry(
            WorkerProcess::Gone { shell_pid: 999 },
            &live_states,
            "exec-1",
            started_epoch,
            now,
        );
        assert_eq!(
            process,
            WorkerProcess::Alive { shell_pid: 999 },
            "a corroborated Gone verdict must be treated as Alive so callers never reap/redispatch over it",
        );
        assert!(reason.is_some());
    }

    /// A long foreground tool call (e.g. a `bazel build`) with no hook for
    /// minutes must still corroborate — this is the "tool in flight" branch,
    /// covering the same-tick incidents where the false-reap guard's log
    /// explicitly noted "tool `Bash` in flight". Past
    /// [`crate::stale_worker_sweep::DEFAULT_STALE_THRESHOLD_SECS`] the spare
    /// ages out (see [`stale_tool_in_flight_does_not_corroborate`]).
    #[test]
    fn tool_in_flight_corroborates_a_gone_verdict() {
        let live_states = LiveWorkerStateRegistry::new();
        live_states.register_spawn(1, "exec-2", "claude-opus-4-7", 12345, None);
        live_states.apply_event(
            1,
            &WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );

        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let started_epoch = now - 300;
        let (process, _) = corroborate_against_live_registry(
            WorkerProcess::Gone { shell_pid: 1 },
            &live_states,
            "exec-2",
            started_epoch,
            now,
        );
        assert_eq!(process, WorkerProcess::Alive { shell_pid: 1 });
    }

    /// An unbalanced PreToolUse whose last in-execution hook is older than
    /// the stale-worker ceiling must NOT corroborate forever — otherwise a
    /// mid-tool death the app never reported wedges `dead_pane_sweep` for
    /// the life of the engine.
    #[test]
    fn stale_tool_in_flight_does_not_corroborate() {
        let live_states = LiveWorkerStateRegistry::new();
        live_states.register_spawn(1, "exec-stale-tool", "claude-opus-4-7", 12345, None);
        live_states.apply_event(
            1,
            &WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );

        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let started_epoch = now - crate::stale_worker_sweep::DEFAULT_STALE_THRESHOLD_SECS - 600;
        // Last hook is attributed to this execution but well past the
        // tool-in-flight ceiling.
        live_states.set_last_event_at_for_test(
            1,
            iso8601_utc(now - crate::stale_worker_sweep::DEFAULT_STALE_THRESHOLD_SECS - 60),
        );

        assert!(
            corroborating_liveness(&live_states, "exec-stale-tool", started_epoch, now).is_none(),
            "a stuck tool older than the stale-worker ceiling must not corroborate forever",
        );
    }

    /// A genuinely dead worker — no live-state entry at all — must NOT be
    /// corroborated: the `Gone` verdict passes through unchanged so the
    /// reaper still reaps it. Without this, corroboration would turn into a
    /// blanket "never reap" guard instead of a targeted false-positive fix.
    #[test]
    fn no_live_state_does_not_corroborate() {
        let live_states = LiveWorkerStateRegistry::new();
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        assert!(corroborating_liveness(&live_states, "exec-missing", now - 300, now).is_none());

        let (process, reason) = corroborate_against_live_registry(
            WorkerProcess::Gone { shell_pid: 7 },
            &live_states,
            "exec-missing",
            now - 300,
            now,
        );
        assert_eq!(process, WorkerProcess::Gone { shell_pid: 7 });
        assert!(reason.is_none());
    }

    /// A hook event older than the corroboration window is stale, not
    /// corroborating — a genuinely dead worker's last-known activity must
    /// still age out and be reaped on a later pass.
    #[test]
    fn stale_hook_does_not_corroborate() {
        let live_states = LiveWorkerStateRegistry::new();
        live_states.register_spawn(1, "exec-3", "claude-opus-4-7", 12345, None);
        live_states.apply_event(
            1,
            &WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );
        live_states.apply_event(
            1,
            &WorkerEvent::PostToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
                tool_response: serde_json::json!({}),
            },
        );

        // Ask as if "now" were far past the corroboration window.
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let far_future = now + CORROBORATION_WINDOW_SECS + 300;
        assert!(corroborating_liveness(&live_states, "exec-3", now - 600, far_future).is_none());
    }

    /// A hook that predates this execution's own `started_at` belongs to a
    /// prior run on a recycled slot — it must not corroborate a DIFFERENT
    /// (later) execution's liveness.
    #[test]
    fn hook_predating_started_at_does_not_corroborate() {
        let live_states = LiveWorkerStateRegistry::new();
        live_states.register_spawn(1, "exec-4", "claude-opus-4-7", 12345, None);
        live_states.apply_event(
            1,
            &WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );
        live_states.apply_event(
            1,
            &WorkerEvent::PostToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
                tool_response: serde_json::json!({}),
            },
        );

        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        // started_at in the future relative to the hook we just stamped.
        assert!(corroborating_liveness(&live_states, "exec-4", now + 60, now).is_none());
    }

    /// Corroboration only ever pulls a verdict toward "alive" — it must
    /// never touch an already-`Alive` or `Unknown` verdict.
    #[test]
    fn corroboration_never_touches_non_gone_verdicts() {
        let live_states = LiveWorkerStateRegistry::new();
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let (alive, reason) = corroborate_against_live_registry(
            WorkerProcess::Alive { shell_pid: 5 },
            &live_states,
            "exec-5",
            now - 300,
            now,
        );
        assert_eq!(alive, WorkerProcess::Alive { shell_pid: 5 });
        assert!(reason.is_none());

        let (unknown, reason) =
            corroborate_against_live_registry(WorkerProcess::Unknown, &live_states, "exec-5", now - 300, now);
        assert_eq!(unknown, WorkerProcess::Unknown);
        assert!(reason.is_none());
    }

    #[test]
    fn zero_and_negative_pids_are_never_probed() {
        assert_eq!(
            classify_worker_process(Some(0), &PidStatus::Alive),
            WorkerProcess::Unknown,
        );
        assert_eq!(
            classify_worker_process(Some(-1), &PidStatus::Alive),
            WorkerProcess::Unknown,
        );
        assert_eq!(classify_worker_process(None, &PidStatus::Alive), WorkerProcess::Unknown);
    }

    /// `EPERM` means the process EXISTS but is not ours to signal. Reading it
    /// as gone is the direction that double-dispatches onto a live worker, so
    /// it must classify as alive.
    #[test]
    fn permission_denied_counts_as_alive() {
        assert_eq!(
            classify_worker_process(Some(42), &PidStatus::PermissionDenied),
            WorkerProcess::Alive { shell_pid: 42 },
        );
    }

    #[test]
    fn esrch_is_gone_and_other_errno_is_unknown() {
        assert_eq!(
            classify_worker_process(Some(42), &PidStatus::Dead),
            WorkerProcess::Gone { shell_pid: 42 },
        );
        assert_eq!(
            classify_worker_process(
                Some(42),
                &PidStatus::Unknown(std::io::Error::from(std::io::ErrorKind::Other)),
            ),
            WorkerProcess::Unknown,
        );
    }

    /// The whole point of the module: the probe still answers after the
    /// execution row has gone TERMINAL. Every registry-driven check returns
    /// "nothing here" in this state; the durable row does not.
    #[test]
    fn probe_answers_for_a_terminal_execution() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        // Our own pid is by definition alive.
        let execution_id = create_spawned_execution(&db, &work_item_id, i64::from(std::process::id()));
        db.mark_execution_orphaned(&execution_id, "engine believed this run was dead")
            .unwrap();

        assert!(
            probe_execution_worker(&db, &execution_id).is_alive(),
            "a terminal execution whose process is alive must still probe as alive",
        );
    }

    #[test]
    fn probe_reports_gone_for_a_dead_recorded_pid() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, dead_pid());

        let dead = i32::try_from(dead_pid()).unwrap();
        assert_eq!(
            probe_execution_worker(&db, &execution_id),
            WorkerProcess::Gone { shell_pid: dead },
        );
        // `Gone` carries its pid so `dead_pane_sweep` can name it in the reason
        // string and dispatch-event details without re-reading the row; only
        // `alive_pid` gates the destructive/state-restoring callers.
        assert_eq!(probe_execution_worker(&db, &execution_id).shell_pid(), Some(dead),);
        assert_eq!(probe_execution_worker(&db, &execution_id).alive_pid(), None);
    }

    /// The destructive caller's bound. `probe_execution_worker` answers from a
    /// row of any age — right for the read-only callers, whose failure
    /// direction is to decline — but the reap in `release_worker_pane` signals
    /// a process *group*, and a recycled pid is an unrelated bystander. The
    /// bounded form must refuse rather than hand back a verdict.
    #[test]
    fn bounded_probe_refuses_a_row_outside_the_trust_window() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, i64::from(std::process::id()));

        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        assert!(
            probe_execution_worker_within(&db, &execution_id, REDISPATCH_PID_TRUST_SECS, now).is_alive(),
            "a fresh row must still answer",
        );
        assert_eq!(
            probe_execution_worker_within(
                &db,
                &execution_id,
                REDISPATCH_PID_TRUST_SECS,
                now + REDISPATCH_PID_TRUST_SECS + 60,
            ),
            WorkerProcess::Unknown,
            "an aged-out row must yield no verdict at all, not a pid to signal",
        );
        assert!(
            probe_execution_worker(&db, &execution_id).is_alive(),
            "the unbounded probe the read-only callers use is unaffected",
        );
    }

    /// No pid recorded is `Unknown`, never `Gone` — that is the state a
    /// mid-spawn worker legitimately occupies, and treating it as death is
    /// how a live-but-slow spawn gets reaped.
    #[test]
    fn probe_reports_unknown_when_no_pid_was_ever_recorded() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_old_execution(&db, &work_item_id);

        assert_eq!(probe_execution_worker(&db, &execution_id), WorkerProcess::Unknown);
    }

    /// The work-item-scoped lookup finds the pid of a TERMINAL execution —
    /// the exact row a re-dispatcher is about to duplicate.
    #[test]
    fn work_item_probe_finds_the_terminal_executions_live_process() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_spawned_execution(&db, &work_item_id, i64::from(std::process::id()));
        db.mark_execution_orphaned(&execution_id, "wrongly terminalized")
            .unwrap();

        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let (found_exec, process) =
            probe_work_item_worker(&db, &work_item_id, now).expect("recorded pid must be found");
        assert_eq!(found_exec, execution_id);
        assert!(process.is_alive());
    }

    /// Pid-reuse guard: a row older than the trust window is not returned at
    /// all, so no caller can act on a pid the table can no longer vouch for.
    #[test]
    fn work_item_probe_ignores_rows_outside_the_trust_window() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        create_spawned_execution(&db, &work_item_id, i64::from(std::process::id()));

        // Ask as if "now" were far in the future: the run's created_at falls
        // outside REDISPATCH_PID_TRUST_SECS.
        let far_future = boss_engine_utils::epoch_time::now_epoch_secs() + REDISPATCH_PID_TRUST_SECS + 60;
        assert!(
            probe_work_item_worker(&db, &work_item_id, far_future).is_none(),
            "a pid outside the trust window must not be offered to callers",
        );
    }
}
