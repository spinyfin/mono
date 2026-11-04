//! Periodic cube-lease heartbeat: keeps every live worker's cube
//! workspace lease from TTL-expiring out from under it.
//!
//! ## Why this exists
//!
//! Cube hands the engine a workspace via a *lease* that carries a TTL
//! (cube's default is 86400 s / 24 h). Cube runs a TTL sweep that
//! reclaims any lease whose expiry has passed — it marks the workspace
//! `free` and clears the lease *even if a worker is still alive in it*.
//! Cube exposes `cube workspace heartbeat <lease>` to push the expiry
//! forward, but only the engine knows a worker is still running, so the
//! engine is the only thing that can call it.
//!
//! Before this sweep the engine never heartbeated anything. Any worker
//! that ran longer than the lease TTL (large chores, multi-bazel builds,
//! reviews) had its workspace reclaimed mid-run: cube flipped it to
//! `free` while a live worker kept editing it, cube and the engine
//! desynced, and the pool filled with "phantom-free" workspaces (cube
//! says free, a live worker is actually there). New dispatches landed on
//! a phantom-free workspace, the engine's occupancy guard refused it,
//! and the pool starved. Recovering took a manual reset of ~30
//! workspaces. This sweep is the root-cause fix.
//!
//! ## Algorithm (mirrors [`crate::dead_pid_sweep`])
//!
//! Every [`heartbeat_interval`] (default 300 s — deliberately ≪ the
//! 86400 s TTL, see "TTL ownership" below):
//!
//! ## Which cube gets the beat
//!
//! A lease is tracked only by the cube on the host its workspace lives on,
//! so every call routes through [`ExecutionCubes`], which resolves the
//! execution's dispatch host. `local` resolves to the engine's own cube
//! client (the local-only path is unchanged); a remote execution resolves
//! to the same `ssh <host> cube …` transport the dispatch loop leased with.
//! Heartbeat failure events — and, for non-local hosts, success events —
//! carry `cube_command` / `cube_cwd` the same way the lease and repo-ensure
//! stages already do, so a misrouted beat is readable on the timeline.
//!
//! 1. Snapshot [`crate::live_worker_state::LiveWorkerStateRegistry`].
//! 2. For each slot with a live `shell_pid` and non-terminal activity
//!    whose execution is non-terminal and has recorded a
//!    `cube_lease_id`:
//!    1. Probe the PID via `kill(pid, 0)` (shared with the dead-PID
//!       sweep). A **dead** PID is *skipped* — we deliberately stop
//!       heartbeating the instant the process is gone, so the lease
//!       expires on its own and cube frees the workspace within ~TTL
//!       (this is what makes "kill the worker → lease frees within
//!       ~TTL" hold). The dead-PID sweep reaps the slot in parallel.
//!    2. Otherwise call
//!       [`CubeClient::heartbeat_lease`](boss_execution_contracts::CubeClient::heartbeat_lease)
//!       with an explicit TTL, refreshing the expiry to now + TTL.
//!
//! ## TTL ownership (engine-owned, not implicit)
//!
//! The engine owns the heartbeat-interval ↔ TTL relationship explicitly
//! rather than relying on cube's default: it passes
//! [`LEASE_TTL_SECS`] on every heartbeat and ticks every
//! [`DEFAULT_HEARTBEAT_INTERVAL`]. With 300 s ≪ 86400 s the lease is
//! refreshed many times per TTL window, so many consecutive missed/failed
//! heartbeats (a transiently busy engine, a flaky cube call) are
//! tolerated before any lease is at risk.
//!
//! ## Engine restart
//!
//! The periodic sweep keys off the in-memory live-worker registry,
//! which is *empty* immediately after an engine restart (it is rebuilt
//! as workers re-send hook events). To stop a long-running worker from
//! being stranded in that gap, two complementary mechanisms work together:
//!
//! 1. [`reheartbeat_live_runs`] runs once at startup and pushes every
//!    `Live`-verdict lease forward by a full TTL immediately.
//! 2. Every subsequent pass of [`run_one_pass`] also scans the DB for
//!    non-terminal executions with a recorded lease that are *not yet
//!    present* in the in-memory registry (the "DB-fallback sweep"). This
//!    covers quiet workers (e.g. a long `bazel build`) that emit no hook
//!    events for many minutes — they receive a continuous stream of
//!    heartbeats until they re-register via a hook, are reaped by the
//!    liveness gate below, or their execution reaches a terminal state by
//!    some other path.
//!
//! ## DB-fallback liveness gate
//!
//! Unlike the registry sweep, a DB-fallback row has no in-memory `shell_pid`
//! to `kill(pid, 0)` — that is exactly why it fell through to the DB-fallback
//! path. Before 2026-07-29 this meant the DB-fallback sweep heartbeated
//! every such row unconditionally, forever, with no liveness check at all:
//! an execution whose worker had crashed *before* an engine restart (so it
//! could never re-register) had its lease refreshed every pass indefinitely,
//! pinning its cube workspace with nothing to reclaim it short of a manual
//! reset. [`db_fallback_death_evidence`] closes this gap by
//! reusing the same *durable* (DB-persisted, restart-robust) signals
//! [`crate::dead_pane_sweep`] and [`crate::lost_workspace_sweep`] already
//! established for exactly this purpose: the worker shell pid the app
//! reported (`work_runs.shell_pid`), probed via
//! [`crate::dead_pane_sweep::shell_pid_death_evidence`] — the pid-death
//! signal's single, exclusive implementation, reused rather than
//! reimplemented here so the grace window and liveness gates below can never
//! drift between call sites — or, if no pid was ever reported, whether the
//! execution's *current* run has been alive well past the pane-attach
//! deadline. Only LOCAL executions are eligible (a pid probe says nothing
//! about a worker on another host; those leases are
//! [`crate::remote_lease_reconcile`]'s job against the *remote* cube). Only
//! [`crate::execution_liveness::PaneLivenessVerdict`]-eligible states are
//! ever reaped: the gate checks `execution.status.is_live()` first, exactly
//! mirroring `dead_pane_sweep::reconcile_if_pane_dead` and
//! `lost_workspace_sweep::reconcile_if_execution_dead_at` — a
//! `waiting_review`/`waiting_merge` row is a park state whose worker finished
//! its job, opened its PR, and exited by design, and must never be reaped
//! just because its pane is (correctly) gone. On positive evidence of death
//! from a *live* row the row is reaped and its lease is **actively
//! released through the preservation gate** via [`reap_and_release_lease`] — unlike the dead-PID
//! skip above, which deliberately leaves the lease to expire on its own TTL,
//! a DB-fallback row has no other mechanism watching it, so waiting out the
//! (24 h) TTL is not an option. Everything short of positive evidence from a
//! live row (no durable pid yet and still within the attach deadline, a
//! probe that returns alive/EPERM/an unexpected errno, or an unresolvable
//! host/pid lookup) heartbeats exactly as before — this gate only ever
//! *stops* heartbeating on proof, never on absence of proof.
//!
//! ## Relationship with `HeartbeatGuard` (coordinator.rs)
//!
//! `coordinator.rs` also contains a `HeartbeatGuard` that was added for
//! the same 2026-05-12 incident. For in-process / blocking runners
//! (e.g. test fakes where `spawn_worker` blocks until the run
//! completes), the guard fires correctly throughout the run. For the
//! production *pane-spawn* path, `spawn_worker` returns immediately
//! after handing the pane off, and the guard is dropped right after —
//! which means it almost never fires a single beat for a pane worker.
//! That is the accurate root-cause framing: the guard existed but was
//! dropped before it could cover the pane worker's lifetime. This
//! module's periodic sweep is the complementary fix that covers the
//! pane-worker gap. Both mechanisms are intentionally left in place:
//! the guard covers blocking runners; this sweep covers pane workers.
//!
//! ## Auto-reap on sustained heartbeat failure
//!
//! A heartbeat failure alone used to be purely observational: it was
//! logged and evented, but the execution row was left exactly as it was —
//! forever, if the lease never recovered. That is precisely what happened
//! on 2026-07-03: three `automation_triage` panes died in their first
//! second (before ever registering with the live-worker registry), cube
//! reclaimed/never-tracked their leases, and every heartbeat pass warned
//! `lease ... is not tracked` for 30+ minutes with no consequence — the
//! rows stayed `waiting_human`/`running`, the redundant-spawn guard (which
//! only reads `work_executions.status`, a *paper* liveness signal — see
//! `crate::execution_liveness`) treated them as live, and every retry died
//! with `redundant_spawn`. The workspace directory itself was still on
//! disk, so `crate::lost_workspace_sweep`'s missing-directory check never
//! fired either.
//!
//! [`HeartbeatFailureBreaker`] closes this gap: it tracks *consecutive*
//! heartbeat failures per execution across passes (reset to zero on the
//! next success). Once a lease has failed
//! [`AUTO_REAP_AFTER_CONSECUTIVE_FAILURES`] times in a row, that is only
//! *candidate* evidence — a heartbeat failure proves the `cube workspace
//! heartbeat` call errored or timed out, which is equally consistent with a
//! transient cube CLI hiccup or a cube daemon outage hitting every in-flight
//! lease at once (in which case reaping would mass-reclaim live workers'
//! workspaces — precisely the working-copy-stolen / duplicate-dispatch harm
//! this module exists to avoid). So before reaping, [`auto_reap_dead_lease`]
//! takes an independent second opinion via
//! [`crate::run_reconcile::confirm_execution_dead`] — the same `cube
//! workspace list` classifier the startup reconciler uses — and only
//! proceeds if it comes back [`RunReconcileVerdict::Dead`]. A `Live` or
//! `Unknown` verdict (including because the confirmation probe itself
//! failed, e.g. mid-outage) resets the streak instead of reaping. Only once
//! confirmed dead does it auto-reap the execution through the exact same
//! terminal path as `bossctl agents reap` (`WorkDb::mark_execution_orphaned`).
//! At the default 300 s cadence the streak alone reaches threshold within 15
//! minutes, chosen so a confirmed-dead lease self-heals well before the next
//! automation retry (which fires every 15 min): a pane that dies at spawn
//! now self-heals on its very first retry instead of wedging behind
//! `redundant_spawn`.
//!
//! **Offline-host bypass.** The confirm-dead gate exists to avoid mass-reaping
//! live workers during a *cube* outage. It does NOT apply when the
//! execution's bound host has been deliberately taken offline — the operator
//! disabled it (`bossctl hosts disable`), the dispatch-health circuit breaker
//! auto-disabled it, or it was removed. There, the disable is itself positive
//! evidence the worker is unreachable, and no `cube workspace list`
//! confirmation is possible. So [`auto_reap_dead_lease`] checks
//! [`WorkDb::execution_bound_host_offline`] first and, when the host is
//! offline, reaps directly without the confirm probe. This is the heartbeat
//! half of the 2026-07-03 anaplian fix (in-flight executions on a disabled
//! host that heartbeat-errored forever with no re-route); the proactive
//! drainer is [`crate::host_reconcile`], and this path is the
//! belt-and-suspenders for a lease whose streak crosses threshold between
//! that sweep's ticks. A cube outage never flips a host's `enabled` flag, so
//! the bypass cannot fire spuriously during one.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

#[cfg(test)]
use async_trait::async_trait;
use boss_protocol::{ExecutionKind, WorkExecution};

use crate::coordinator::ExecutionCoordinator;
use crate::dead_pid_sweep::{PidStatus, probe_pid};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::run_reconcile::{self, RunReconcileReport, RunReconcileVerdict};
use crate::work::WorkDb;
#[cfg(test)]
use boss_execution_contracts::CubeClient;

pub use boss_execution_contracts::{ExecutionCubes, ResolvedExecutionCube};

pub use boss_engine_completion::remote_host::HostRoutedCubes;

/// Consecutive cube-lease-heartbeat failures for one execution before the
/// engine treats the lease as definitively gone and auto-reaps the
/// execution (see the module-level "Auto-reap on sustained heartbeat
/// failure" doc above). At the default 300 s [`DEFAULT_HEARTBEAT_INTERVAL`]
/// this is a 15-minute bound.
pub const AUTO_REAP_AFTER_CONSECUTIVE_FAILURES: u32 = 3;

/// Tracks consecutive cube-lease-heartbeat failures per execution id
/// across passes. A transient blip (one failed subprocess call, one cube
/// hiccup) must not trigger a reap — only a lease that *never* recovers
/// should. Failures accumulate per execution id; a success, or the
/// execution going terminal by any path, clears its streak.
///
/// Deliberately in-memory rather than a DB column: it is a short-lived
/// breaker over a single engine process's uptime, not a durable fact, and
/// every consumer of it (`run_one_pass`) already runs inside that same
/// process. An engine restart resets every streak to zero, which is safe —
/// [`crate::run_reconcile`]'s startup probe independently re-verifies lease
/// liveness against cube before any redispatch is allowed.
#[derive(Default)]
pub struct HeartbeatFailureBreaker {
    consecutive_failures: Mutex<HashMap<String, u32>>,
}

impl HeartbeatFailureBreaker {
    /// Record a failed heartbeat for `execution_id` and return the new
    /// consecutive-failure count.
    fn record_failure(&self, execution_id: &str) -> u32 {
        let mut counts = self.consecutive_failures.lock().unwrap();
        let count = counts.entry(execution_id.to_owned()).or_insert(0);
        *count += 1;
        *count
    }

    /// Clear any tracked failure streak for `execution_id` — called on a
    /// successful heartbeat, and once an execution is confirmed terminal
    /// (reaped by this sweep or any other path) so the map does not grow
    /// unboundedly over the engine's lifetime.
    fn forget(&self, execution_id: &str) {
        self.consecutive_failures.lock().unwrap().remove(execution_id);
    }

    /// Drop any tracked streak whose execution id is not in `known_ids`.
    /// `forget` alone does not bound the map: an execution that records a
    /// failure and then completes normally (success/terminal transitions
    /// observed by a *different* sweep, or the row simply falling out of
    /// both the live registry and `list_in_flight_executions` between one
    /// pass and the next) is never individually `forget`-ten, so its entry
    /// would otherwise linger for the engine's lifetime. Called once per
    /// pass with the set of every execution id this pass actually saw
    /// (live-registry ∪ DB-fallback), which bounds the map to executions
    /// currently in flight.
    fn retain_only(&self, known_ids: &HashSet<String>) {
        self.consecutive_failures
            .lock()
            .unwrap()
            .retain(|execution_id, _| known_ids.contains(execution_id));
    }
}

/// Environment variable overriding the heartbeat cadence (seconds).
pub const HEARTBEAT_INTERVAL_SECS_ENV: &str = "BOSS_CUBE_LEASE_HEARTBEAT_INTERVAL_SECS";

/// Default cadence between heartbeat passes. Deliberately far below the
/// [`LEASE_TTL_SECS`] window so several passes refresh every lease
/// before it could expire.
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(300);

/// TTL (seconds) the engine asks cube to set on every heartbeat.
///
/// Matches cube's own default of 86400 s / 24 h (`tools/cube` —
/// `DEFAULT_LEASE_TTL_SECS`). The engine still passes the value
/// *explicitly* so the interval-≪-TTL relationship is owned here and
/// cannot silently re-clamp every heartbeated lease if cube's default
/// moves again. With [`DEFAULT_HEARTBEAT_INTERVAL`] = 300 s this is a
/// ~288× margin — enormous headroom; the real reclaim paths (confirm-dead
/// auto-reap and force-release) do not depend on this TTL at all. TTL is
/// only the last-resort backstop for a lease nothing is watching.
pub const LEASE_TTL_SECS: u64 = 86400;

/// Per-call timeout for a single `cube workspace heartbeat` subprocess
/// invocation. Addresses the same cube-hang failure mode that prompted
/// timeouts on lease/repo-ensure calls, but deliberately no longer tracks
/// [`crate::coordinator::CUBE_LEASE_TIMEOUT`]: `cube workspace heartbeat`
/// is a single indexed UPDATE against the local SQLite registry, with no
/// filesystem probing, `jj` subprocess, or network in its path, so it has
/// none of the tail that made the lease bound worth relaxing.
/// Without a bound, one hung heartbeat call would
/// stall the entire pass and leave every other live worker un-heartbeated
/// until the subprocess eventually returned (or never did).
pub const HEARTBEAT_CUBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Read the heartbeat interval from [`HEARTBEAT_INTERVAL_SECS_ENV`],
/// falling back to [`DEFAULT_HEARTBEAT_INTERVAL`]. A zero or unparseable
/// value falls back to the default (a zero interval would busy-loop).
pub fn heartbeat_interval() -> Duration {
    // A zero interval would busy-loop, so require ≥ 1 s; anything below the
    // guard (including a `0`) falls back to the default.
    boss_engine_utils::env_parse::env_duration_secs_min(HEARTBEAT_INTERVAL_SECS_ENV, DEFAULT_HEARTBEAT_INTERVAL, 1)
}

/// Counts from one heartbeat pass; logged at `info` when activity occurs.
#[derive(Debug, Default, PartialEq, Eq, bon::Builder)]
pub struct HeartbeatSweepOutcome {
    /// Leases successfully refreshed this pass via the live-registry sweep.
    pub heartbeated: usize,
    /// Leases successfully refreshed via the DB-fallback sweep (in-flight
    /// executions not yet present in the live-worker registry, covering the
    /// post-restart gap until each worker re-registers via hook events).
    pub db_fallback_heartbeated: usize,
    /// Heartbeat calls that errored (lease gone, cube unreachable) or timed
    /// out (cube subprocess hung).
    pub failed: usize,
    /// Live slots whose PID was gone — left to expire on purpose.
    pub dead_pid_skipped: usize,
    /// Live slots whose `shell_pid` is not yet reported (≤ 0), and remote
    /// workers whose shell_pid is permanently 0 (they have no local pid).
    pub no_pid_skipped: usize,
    /// Live slots whose execution has not recorded a `cube_lease_id` yet.
    pub no_lease_skipped: usize,
    /// Slots whose execution/activity is already terminal.
    pub terminal_skipped: usize,
    /// Executions auto-reaped this pass after
    /// [`AUTO_REAP_AFTER_CONSECUTIVE_FAILURES`] consecutive heartbeat
    /// failures.
    pub auto_reaped: usize,
}

impl HeartbeatSweepOutcome {
    fn has_activity(&self) -> bool {
        self.heartbeated > 0 || self.db_fallback_heartbeated > 0 || self.failed > 0 || self.auto_reaped > 0
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn. The returned handle is detached by the
/// caller (the loop lives for the engine's lifetime). Owns the
/// [`HeartbeatFailureBreaker`] across passes so consecutive-failure streaks
/// survive from one tick to the next.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    coordinator: Arc<ExecutionCoordinator>,
    cubes: Arc<dyn ExecutionCubes>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    boss_task_scope::spawn(async move {
        let breaker = HeartbeatFailureBreaker::default();
        loop {
            let outcome = run_one_pass(
                work_db.as_ref(),
                live_states.as_ref(),
                cubes.as_ref(),
                dispatch_events.as_ref(),
                &breaker,
            )
            .await;
            if outcome.auto_reaped > 0 {
                // An auto-reap clears the redundant-spawn guard's blocker
                // for that work item; kick the scheduler so a queued retry
                // dispatches immediately instead of waiting for the next
                // opportunistic kick.
                coordinator.kick();
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// The dependencies shared by every helper in a heartbeat pass, bundled so
/// they can be threaded through as a single argument instead of four
/// (keeps helper signatures under clippy's `too_many_arguments`).
struct HeartbeatCtx<'a> {
    work_db: &'a WorkDb,
    cubes: &'a dyn ExecutionCubes,
    dispatch_events: &'a dyn DispatchEventSink,
    breaker: &'a HeartbeatFailureBreaker,
    /// Occupancy probe for the lease release after a reap; `None` scans the
    /// host's process table.
    occupancy: Option<&'a dyn crate::workspace_lease_release::WorkspaceOccupancy>,
}

/// The identifiers naming one heartbeat target, bundled for the same
/// reason as [`HeartbeatCtx`].
#[derive(Clone, Copy)]
struct HeartbeatTarget<'a> {
    execution_id: &'a str,
    lease_id: &'a str,
    work_item_id: &'a str,
    cube_workspace_id: &'a str,
}

/// Positive, durable evidence that a DB-fallback row's worker is gone —
/// gathered without any dependency on the in-memory
/// [`LiveWorkerStateRegistry`], which by construction has nothing for these
/// rows (that is exactly why they are in the DB-fallback sweep instead of
/// the registry sweep above). Reuses the same durable signals
/// [`crate::dead_pane_sweep`] and [`crate::lost_workspace_sweep`] already
/// established for local-worker liveness, so a genuinely-dead worker is
/// never mistaken for a merely-quiet or not-yet-registered one.
#[derive(Debug, Clone, Copy)]
enum DbFallbackDeathEvidence {
    /// The durably-recorded (`work_runs.shell_pid`) pid for the execution's
    /// latest LOCAL run no longer exists (`kill(pid, 0)` → ESRCH), and the
    /// driver was not merely delivering its expected one-shot exit.
    ShellPidDead { shell_pid: i64 },
    /// No pid was ever durably recorded for the execution and it has been
    /// alive well past the pane-attach deadline — the pane never came up in
    /// the first place.
    PaneNeverAttached,
}

impl DbFallbackDeathEvidence {
    /// Human-readable clause folded into the orphan / force-release reason
    /// text.
    fn reason_clause(self) -> String {
        match self {
            Self::ShellPidDead { shell_pid } => {
                format!("its worker shell pid {shell_pid} no longer exists (kill(0)=ESRCH)")
            }
            Self::PaneNeverAttached => {
                "its worker pane never reported a shell pid within the attach deadline".to_owned()
            }
        }
    }

    /// Stable identifier folded into the dispatch event's `reason` detail.
    fn event_reason(self) -> &'static str {
        match self {
            Self::ShellPidDead { .. } => "db_fallback_shell_pid_dead",
            Self::PaneNeverAttached => "db_fallback_pane_never_attached",
        }
    }
}

/// Durable, restart-robust liveness check for a DB-fallback row: one whose
/// execution id is absent from the in-memory live-worker registry this
/// pass, so there is no in-memory `shell_pid` to `kill(pid, 0)` the way the
/// registry sweep above does. Without this gate the DB-fallback sweep would
/// heartbeat any such row forever regardless of whether its worker is still
/// running — the bug this function closes.
///
/// Scoped to LOCAL executions only: a pid probe (or the absence of one) says
/// nothing about a worker on another host. Remote leases are reconciled by
/// `crate::remote_lease_reconcile` against the *remote* cube instead, so a
/// remote row here always returns `None` (heartbeat proceeds unchanged).
///
/// Errs toward `None` (keep heartbeating) on every ambiguous or unreadable
/// signal — this function only ever acts on positive evidence of death,
/// exactly like `crate::dead_pane_sweep` and `crate::lost_workspace_sweep`,
/// whose durable signals it reuses.
async fn db_fallback_death_evidence(
    work_db: &WorkDb,
    execution: &WorkExecution,
    now_epoch_s: i64,
) -> Option<DbFallbackDeathEvidence> {
    // Only reconcile states where a live worker is expected to still be
    // holding the workspace — exactly `is_live()`, mirroring
    // `dead_pane_sweep::reconcile_if_pane_dead` and
    // `lost_workspace_sweep::reconcile_if_execution_dead_at`'s identical gate.
    // Every other non-terminal state (`waiting_review`, `waiting_merge`,
    // `queued`, `ready`, `waiting_dependency`, ...) is a normal park state
    // whose worker has already finished its job (opened a PR, in the review
    // cases) and exited BY DESIGN — a dead or never-reported pid there is
    // expected, not a zombie, and force-releasing its lease would hand back a
    // workspace that is deliberately retained for a follow-up revision.
    if !execution.status.is_live() {
        return None;
    }

    match work_db.latest_run_host_for_execution(&execution.id) {
        Ok(Some(host)) if host == "local" => {}
        Ok(_) => return None,
        Err(err) => {
            tracing::debug!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "cube-lease heartbeat: DB-fallback liveness check could not resolve run host; heartbeating conservatively",
            );
            return None;
        }
    }

    // Signal 1 — the durable pid-death probe. This is `dead_pane_sweep`'s
    // exclusive implementation of the signal (see its module doc: "exactly
    // one reaper per death signal"); reusing it here means the grace window,
    // the `is_live()` gate, and the one-shot-driver exemption can never drift
    // between the two call sites.
    // No `live_states` to corroborate against: by construction, a DB-fallback
    // row has nothing in the in-memory registry (see this function's doc
    // comment) — `dead_pane_sweep::shell_pid_death_evidence` falls back to
    // trusting the bare durable probe exactly as it did before corroboration
    // existed.
    if let Some(shell_pid) =
        crate::dead_pane_sweep::shell_pid_death_evidence(work_db, execution, now_epoch_s, None).await
    {
        return Some(DbFallbackDeathEvidence::ShellPidDead { shell_pid });
    }

    // Signal 2 — the pane never attached. Only applies when no durable pid
    // was ever recorded at all (a recorded-but-dead pid is signal 1's job,
    // handled above); if a pid IS recorded but still alive (or ambiguous),
    // this must not fall through to a never-attached verdict.
    let (shell_pid, latest_tmux_observed_pid) = match work_db
        .latest_local_pane_pid_snapshot_for_execution(&execution.id)
    {
        Ok(snapshot) => snapshot,
        Err(err) => {
            tracing::debug!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "cube-lease heartbeat: DB-fallback liveness check could not read durable pane pid snapshot; heartbeating conservatively",
            );
            return None;
        }
    };
    let pid_match = latest_tmux_observed_pid.is_none() || shell_pid == latest_tmux_observed_pid;
    if pid_match {
        tracing::debug!(
            execution_id = %execution.id,
            stored_shell_pid = ?shell_pid,
            latest_tmux_observed_pid = ?latest_tmux_observed_pid,
            "cube-lease heartbeat: compared durable shell pid with latest tmux pane observation",
        );
    } else {
        tracing::warn!(
            execution_id = %execution.id,
            stored_shell_pid = ?shell_pid,
            latest_tmux_observed_pid = ?latest_tmux_observed_pid,
            "cube-lease heartbeat: durable shell pid disagrees with latest tmux pane observation",
        );
    }
    if shell_pid.is_some() {
        // A pid was recorded but signal 1 did not consider it dead — leave it
        // alone rather than judging the never-attached deadline against it.
        return None;
    }

    // Age off the *current* run's own start, not `execution.started_epoch()`:
    // `start_execution_run` stamps `work_executions.started_at =
    // COALESCE(started_at, now)`, so on a resumed execution that field is
    // frozen at the FIRST run's start and never advances. Judging a freshly
    // re-dispatched (pid-less-so-far) run against that ancient timestamp
    // would immediately trip the never-attached deadline and force-release a
    // live worker's lease. `latest_run_started_epoch_for_execution` anchors on
    // `COALESCE(started_at, created_at)`, and `start_execution_run` stamps
    // both columns fresh on every resume, so it ages correctly across restarts.
    let run_started_epoch = match work_db.latest_run_started_epoch_for_execution(&execution.id) {
        Ok(epoch) => epoch,
        Err(err) => {
            tracing::debug!(
                execution_id = %execution.id,
                error = %format!("{err:#}"),
                "cube-lease heartbeat: DB-fallback liveness check could not read latest run start; heartbeating conservatively",
            );
            return None;
        }
    };
    let verdict = crate::execution_liveness::classify_pane_liveness(None, run_started_epoch, now_epoch_s);
    verdict.is_dead().then_some(DbFallbackDeathEvidence::PaneNeverAttached)
}

/// Run a single heartbeat pass: refresh the cube lease of every live
/// worker, auto-reaping any execution whose lease has failed
/// [`AUTO_REAP_AFTER_CONSECUTIVE_FAILURES`] consecutive times. Returns a
/// summary of what happened.
pub async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    cubes: &dyn ExecutionCubes,
    dispatch_events: &dyn DispatchEventSink,
    breaker: &HeartbeatFailureBreaker,
) -> HeartbeatSweepOutcome {
    run_one_pass_impl(
        work_db,
        live_states,
        cubes,
        dispatch_events,
        breaker,
        HEARTBEAT_CUBE_TIMEOUT,
        None,
    )
    .await
}

/// Internal implementation that accepts a configurable per-call timeout
/// (exposed so tests can inject a short timeout without waiting 30 s).
async fn run_one_pass_impl(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    cubes: &dyn ExecutionCubes,
    dispatch_events: &dyn DispatchEventSink,
    breaker: &HeartbeatFailureBreaker,
    heartbeat_timeout: Duration,
    occupancy: Option<&dyn crate::workspace_lease_release::WorkspaceOccupancy>,
) -> HeartbeatSweepOutcome {
    let ctx = HeartbeatCtx {
        work_db,
        cubes,
        dispatch_events,
        breaker,
        occupancy,
    };
    let mut outcome = HeartbeatSweepOutcome::default();
    let mut registered_run_ids: HashSet<String> = HashSet::new();
    let mut db_fallback_sweep_succeeded = false;

    for state in live_states.snapshot() {
        // Slot hasn't reported a shell pid yet — we can't probe liveness
        // with `kill(pid, 0)`, so this branch has nothing to adjudicate.
        //
        // A pid-less slot must stay out of `registered_run_ids` because the
        // DB-fallback sweep below is the only path that resolves the owning
        // cube per execution (and applies `db_fallback_death_evidence`). A
        // REMOTE worker's `shell_pid` is 0 permanently by design (see
        // `register_remote_worker_slot`), so it lands here on every pass
        // for the whole of its life; marking it registered would hide it
        // from that sweep and leave its remote lease unheartbeated. A local
        // slot whose app has not yet reported its pid is covered by the
        // same fall-through, which errs toward heartbeating on every
        // ambiguous signal.
        if state.shell_pid <= 0 {
            outcome.no_pid_skipped += 1;
            continue;
        }

        // Terminal activity → the completion / teardown path owns lease
        // release; there is nothing to keep alive.
        if state.activity.is_terminal() {
            outcome.terminal_skipped += 1;
            registered_run_ids.insert(state.run_id.clone());
            breaker.forget(&state.run_id);
            continue;
        }

        let execution_id = &state.run_id;
        registered_run_ids.insert(execution_id.clone());

        let execution = match work_db.get_execution(execution_id) {
            Ok(execution) => execution,
            Err(err) => {
                // A live slot whose run_id has no execution row: in-process
                // test slots, or a row deleted out from under us. Nothing to
                // heartbeat.
                tracing::debug!(
                    execution_id,
                    ?err,
                    "cube-lease heartbeat: no execution row for live slot; skipping",
                );
                continue;
            }
        };

        // Execution already terminal (completion raced our snapshot). Its
        // lease is being / has been released by the completion path; do not
        // re-extend it.
        if execution.status.is_terminal() {
            outcome.terminal_skipped += 1;
            breaker.forget(execution_id);
            continue;
        }

        let Some(lease_id) = execution.cube_lease_id.as_deref() else {
            // Live slot whose execution never reached `start_execution_run`
            // (no lease recorded yet). Nothing to heartbeat this pass.
            outcome.no_lease_skipped += 1;
            continue;
        };

        // Liveness gate: only refresh leases held by a process that is
        // actually alive. A dead PID is LEFT to expire — stopping the
        // heartbeat the instant the process is gone is precisely what makes
        // "kill the worker → lease frees within ~TTL" hold. The dead-PID
        // sweep reaps the slot in parallel.
        if matches!(probe_pid(state.shell_pid), PidStatus::Dead) {
            outcome.dead_pid_skipped += 1;
            continue;
        }
        // Alive, alive-but-not-ours (EPERM), or an unexpected probe error:
        // heartbeat. Erring toward refreshing is deliberate — extending a
        // maybe-dead lease costs at most one TTL window, while failing to
        // extend a live one reclaims a working copy out from under an active
        // worker (the incident this whole module fixes).

        let target = HeartbeatTarget {
            execution_id,
            lease_id,
            work_item_id: &execution.work_item_id,
            cube_workspace_id: execution.cube_workspace_id.as_deref().unwrap_or(""),
        };
        let succeeded = heartbeat_one(
            &ctx,
            &target,
            heartbeat_timeout,
            &mut outcome.heartbeated,
            &mut outcome.failed,
        )
        .await;
        record_heartbeat_result(&ctx, &execution, lease_id, succeeded, &mut outcome.auto_reaped).await;
    }

    // DB-fallback sweep: heartbeat in-flight executions not yet present in
    // the live-worker registry. This covers the post-restart gap: the
    // registry is empty until each worker re-registers via hook events, so
    // a quiet worker (e.g. a long bazel build emitting no hooks) would get
    // only the one-shot startup beat and then go un-heartbeated. By scanning
    // the DB every pass we continuously cover such workers until they
    // re-register or their execution reaches a terminal state.
    let now_epoch_s = boss_engine_utils::epoch_time::now_epoch_secs();
    match work_db.list_in_flight_executions() {
        Ok(in_flight) => {
            db_fallback_sweep_succeeded = true;
            for execution in in_flight {
                // Track every in-flight id we saw this pass (whether or not
                // it ends up heartbeated) so the end-of-pass breaker cleanup
                // below never drops a streak for an execution that's still
                // genuinely in flight. `insert` returning `false` means the
                // registry sweep above already handled it.
                if !registered_run_ids.insert(execution.id.clone()) {
                    continue;
                }
                let Some(lease_id) = execution.cube_lease_id.as_deref() else {
                    continue;
                };
                let execution_id = &execution.id;

                // Liveness gate: unlike the registry sweep above, a
                // DB-fallback row has no in-memory pid to probe — probe the
                // durable, restart-robust signal instead. Only on positive
                // evidence of death do we stop heartbeating and reap;
                // everything else (including "we can't tell") heartbeats as
                // before, erring toward keeping a maybe-live lease alive.
                if let Some(evidence) = db_fallback_death_evidence(ctx.work_db, &execution, now_epoch_s).await {
                    let death_clause = evidence.reason_clause();
                    let reason = format!(
                        "cube-lease heartbeat: DB-fallback sweep found positive evidence its worker is gone \
                         ({death_clause}); reaping and releasing its cube lease instead of continuing to heartbeat it",
                    );
                    let details = serde_json::json!({
                        "reason": evidence.event_reason(),
                        "cube_workspace_id": execution.cube_workspace_id.as_deref().unwrap_or(""),
                    });
                    if reap_and_release_lease(
                        &ctx,
                        &execution,
                        lease_id,
                        &reason,
                        &death_clause,
                        Stage::CubeLeaseAutoReap,
                        details,
                    )
                    .await
                    {
                        outcome.auto_reaped += 1;
                        tracing::warn!(
                            execution_id,
                            lease_id,
                            evidence = evidence.event_reason(),
                            "cube-lease heartbeat: DB-fallback sweep reaped an execution whose worker is provably gone",
                        );
                    }
                    continue;
                }

                let mut succeeded_count = 0usize;
                let mut failed_count = 0usize;
                let target = HeartbeatTarget {
                    execution_id,
                    lease_id,
                    work_item_id: &execution.work_item_id,
                    cube_workspace_id: execution.cube_workspace_id.as_deref().unwrap_or(""),
                };
                let succeeded = heartbeat_one(
                    &ctx,
                    &target,
                    heartbeat_timeout,
                    &mut succeeded_count,
                    &mut failed_count,
                )
                .await;
                if succeeded {
                    outcome.db_fallback_heartbeated += 1;
                    tracing::debug!(
                        execution_id,
                        lease_id,
                        "cube-lease heartbeat: DB-fallback beat (not yet in live registry)",
                    );
                }
                outcome.failed += failed_count;
                record_heartbeat_result(&ctx, &execution, lease_id, succeeded, &mut outcome.auto_reaped).await;
            }
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                "cube-lease heartbeat: failed to query in-flight executions for DB-fallback sweep",
            );
        }
    }

    // Terminal remote executions whose worker may still be running: a cancel
    // makes the execution terminal (so it drops out of the in-flight scan)
    // but cannot stop the remote process, and `force_release` keeps the lease
    // held until a pid probe proves death. Keep that retained lease alive
    // until `remote_lease_reconcile` observes death and clears the lease
    // columns, at which point the row leaves this set. Runs with no recorded
    // pid are skipped: they can never be proven dead, so their lease is left
    // to the cube TTL rather than being kept alive forever.
    match work_db.terminal_remote_cleanup_runs() {
        Ok(handles) => {
            for handle in handles.into_iter().filter(|handle| handle.remote_pid.is_some()) {
                let Ok(execution) = work_db.get_execution(&handle.execution_id) else {
                    continue;
                };
                let Some(lease_id) = execution.cube_lease_id.as_deref() else {
                    continue;
                };
                let target = HeartbeatTarget {
                    execution_id: &execution.id,
                    lease_id,
                    work_item_id: &execution.work_item_id,
                    cube_workspace_id: execution.cube_workspace_id.as_deref().unwrap_or(""),
                };
                let mut failed_count = 0usize;
                if heartbeat_one(
                    &ctx,
                    &target,
                    heartbeat_timeout,
                    &mut outcome.db_fallback_heartbeated,
                    &mut failed_count,
                )
                .await
                {
                    tracing::debug!(
                        execution_id = %execution.id,
                        lease_id,
                        "cube-lease heartbeat: kept a retained terminal remote lease alive",
                    );
                }
                outcome.failed += failed_count;
            }
        }
        Err(err) => tracing::warn!(?err, "cube-lease heartbeat: terminal remote lease query failed"),
    }

    // Bound the breaker map to executions this pass actually observed as
    // in-flight. Skip this when the DB-fallback query itself failed: in that
    // case `registered_run_ids` only reflects the live-registry sweep, and
    // pruning against it would wrongly drop streaks for executions that are
    // still in flight but weren't in the live registry this pass.
    if db_fallback_sweep_succeeded {
        breaker.retain_only(&registered_run_ids);
    }

    if outcome.has_activity() {
        tracing::info!(
            heartbeated = outcome.heartbeated,
            db_fallback_heartbeated = outcome.db_fallback_heartbeated,
            failed = outcome.failed,
            dead_pid_skipped = outcome.dead_pid_skipped,
            no_lease_skipped = outcome.no_lease_skipped,
            auto_reaped = outcome.auto_reaped,
            "cube-lease heartbeat: pass complete",
        );
    }

    outcome
}

/// Execute one `cube workspace heartbeat` call with a timeout. Increments
/// either `*succeeded` or `*failed`, emits a dispatch error event on
/// failure or timeout, and returns whether the heartbeat succeeded so the
/// caller can feed [`HeartbeatFailureBreaker`].
async fn heartbeat_one(
    ctx: &HeartbeatCtx<'_>,
    target: &HeartbeatTarget<'_>,
    timeout: Duration,
    succeeded: &mut usize,
    failed: &mut usize,
) -> bool {
    let HeartbeatTarget {
        execution_id,
        lease_id,
        work_item_id,
        cube_workspace_id,
    } = *target;
    // Route to the cube that owns this lease before anything else. An
    // unroutable execution is a heartbeat that did NOT happen, so it takes
    // the same loud failure path as a cube that answered with an error —
    // counted, evented, and fed to the breaker. Never a silent skip: a
    // quietly-unheartbeated lease is exactly how a live worker loses its
    // workspace to the TTL sweep.
    let resolved = match ctx.cubes.cube_for_execution(execution_id).await {
        Ok(resolved) => resolved,
        Err(err) => {
            *failed += 1;
            tracing::warn!(
                execution_id,
                lease_id,
                error = %format!("{err:#}"),
                "cube-lease heartbeat: could not resolve the cube that owns this lease; the lease cannot be \
                 refreshed from here (the worker's workspace is at risk)",
            );
            ctx.dispatch_events
                .emit(
                    DispatchEvent::new(Stage::CubeLeaseHeartbeat, Outcome::Error, execution_id)
                        .with_work_item(work_item_id)
                        .with_cube_lease(lease_id)
                        .with_error(&err)
                        .with_details(serde_json::json!({
                            "ttl_secs": LEASE_TTL_SECS,
                            "cube_workspace_id": cube_workspace_id,
                            "cube_unroutable": true,
                        })),
                )
                .await;
            return false;
        }
    };
    let cube = resolved.cube.as_ref();
    // Surface WHICH cube this beat went to, the same way the lease and
    // repo-ensure stages already do (`cube_command` / `cube_cwd`). Best-
    // effort display only — the local-vs-remote decision below uses the
    // host id from resolution, not this label.
    let cube_invocation = cube.command_repr(&["workspace", "heartbeat", lease_id]);
    let result = tokio::time::timeout(timeout, cube.heartbeat_lease(lease_id, Some(LEASE_TTL_SECS))).await;
    match result {
        Ok(Ok(())) => {
            *succeeded += 1;
            tracing::debug!(
                execution_id,
                lease_id,
                ttl_secs = LEASE_TTL_SECS,
                host_id = %resolved.host_id,
                "cube-lease heartbeat: refreshed lease",
            );
            // Emit Outcome::Ok only for non-local hosts. Local workers would
            // produce a 300 s-cadence event storm across every live slot;
            // remote beats are rare and the routing is what live verification
            // needs to see. Gate on the resolved host id from
            // `cube_for_execution`, not on sniffing `command_repr`'s display
            // cwd — adapters may return `None` or format cwd differently.
            if !resolved.is_local() {
                ctx.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::CubeLeaseHeartbeat, Outcome::Ok, execution_id)
                            .with_work_item(work_item_id)
                            .with_cube_lease(lease_id)
                            .with_cube_invocation(cube_invocation)
                            .with_details(serde_json::json!({
                                "ttl_secs": LEASE_TTL_SECS,
                                "cube_workspace_id": cube_workspace_id,
                                "host_id": resolved.host_id,
                            })),
                    )
                    .await;
            }
            true
        }
        Ok(Err(err)) => {
            *failed += 1;
            tracing::warn!(
                execution_id,
                lease_id,
                error = %format!("{err:#}"),
                "cube-lease heartbeat: failed to refresh lease (cube may have reclaimed it; the worker's workspace is at risk)",
            );
            ctx.dispatch_events
                .emit(
                    DispatchEvent::new(Stage::CubeLeaseHeartbeat, Outcome::Error, execution_id)
                        .with_work_item(work_item_id)
                        .with_cube_lease(lease_id)
                        .with_cube_invocation(cube_invocation)
                        .with_error(&err)
                        .with_details(serde_json::json!({
                            "ttl_secs": LEASE_TTL_SECS,
                            "cube_workspace_id": cube_workspace_id,
                        })),
                )
                .await;
            false
        }
        Err(_elapsed) => {
            *failed += 1;
            let err = anyhow::anyhow!(
                "cube workspace heartbeat timed out after {}s (cube subprocess may be hung)",
                timeout.as_secs()
            );
            tracing::warn!(
                execution_id,
                lease_id,
                timeout_secs = timeout.as_secs(),
                "cube-lease heartbeat: heartbeat call timed out; treating as failure so other leases continue",
            );
            ctx.dispatch_events
                .emit(
                    DispatchEvent::new(Stage::CubeLeaseHeartbeat, Outcome::Error, execution_id)
                        .with_work_item(work_item_id)
                        .with_cube_lease(lease_id)
                        .with_cube_invocation(cube_invocation)
                        .with_error(&err)
                        .with_details(serde_json::json!({
                            "ttl_secs": LEASE_TTL_SECS,
                            "cube_workspace_id": cube_workspace_id,
                            "timed_out": true,
                        })),
                )
                .await;
            false
        }
    }
}

/// Feed one heartbeat outcome into `breaker` and, once the consecutive-
/// failure streak for `execution`'s id reaches
/// [`AUTO_REAP_AFTER_CONSECUTIVE_FAILURES`], auto-reap it via
/// [`auto_reap_dead_lease`]. A success clears the streak — a lease that
/// recovers even once is no longer evidence of death.
async fn record_heartbeat_result(
    ctx: &HeartbeatCtx<'_>,
    execution: &WorkExecution,
    lease_id: &str,
    succeeded: bool,
    auto_reaped: &mut usize,
) {
    if succeeded {
        ctx.breaker.forget(&execution.id);
        return;
    }
    let consecutive_failures = ctx.breaker.record_failure(&execution.id);
    if consecutive_failures < AUTO_REAP_AFTER_CONSECUTIVE_FAILURES {
        return;
    }
    if auto_reap_dead_lease(ctx, execution, lease_id, consecutive_failures).await {
        *auto_reaped += 1;
    }
}

/// Auto-reap `execution` after its cube lease has failed to heartbeat
/// [`AUTO_REAP_AFTER_CONSECUTIVE_FAILURES`] consecutive times AND an
/// independent probe of `cube workspace list` confirms
/// [`RunReconcileVerdict::Dead`] — proof the workspace is gone even though
/// its directory may still be on disk (so [`crate::lost_workspace_sweep`]
/// never fires) and it may never have registered with the live-worker
/// registry (so [`crate::dead_pid_sweep`] never sees it). Routes through the
/// exact same terminal path as `bossctl agents reap`
/// (`WorkDb::mark_execution_orphaned`), so the redundant-spawn guard's
/// `is_live()` check stops treating the row as live and the next scheduler
/// tick can dispatch a fresh execution.
///
/// The consecutive-failure streak alone is NOT sufficient evidence: a
/// heartbeat failure only proves the `cube workspace heartbeat` call
/// errored or timed out, which is equally consistent with a transient cube
/// CLI hiccup or a cube daemon outage affecting every in-flight lease at
/// once. Only [`run_reconcile::confirm_execution_dead`] returning `Dead` —
/// cube's own snapshot showing the workspace is no longer `leased` under
/// our id, or has TTL-expired — is positive proof the workspace is gone.
/// If confirmation comes back `Live` or `Unknown` (including because the
/// confirmation probe itself failed, e.g. during a cube outage), this
/// leaves the row alone and clears the breaker streak so the streak must
/// rebuild before another reap attempt is considered — erring toward never
/// reclaiming a working copy out from under a live worker.
///
/// Returns `true` iff this call actually transitioned the row to terminal
/// (idempotent against a race with any other reconciler: if the row is
/// already terminal by the time we get here, this is a no-op that still
/// clears the breaker streak).
async fn auto_reap_dead_lease(
    ctx: &HeartbeatCtx<'_>,
    execution: &WorkExecution,
    lease_id: &str,
    consecutive_failures: u32,
) -> bool {
    // When the execution's bound host has been declared offline — the
    // operator ran `bossctl hosts disable`, the dispatch-health circuit
    // breaker auto-disabled it, or it was removed — that is itself positive
    // evidence the worker is unreachable: no `cube workspace list`
    // confirmation is possible (the host is gone) or needed (the operator
    // said so). Skip the confirm-dead gate and reap directly, so a heartbeat
    // that keeps erroring against a disabled host is terminalized and
    // re-routed rather than emitting `outcome=error` forever — the second
    // half of the 2026-07-03 anaplian incident. [`crate::host_reconcile`] is
    // the primary drainer for offline hosts; this is the belt-and-suspenders
    // path for a lease whose failure streak crosses the reap threshold
    // between that sweep's ticks. The mass-reap-during-cube-outage hazard the
    // confirm gate guards against does not apply here: a cube outage never
    // flips a host's `enabled` flag, so this bypass only fires when a host
    // was deliberately taken offline.
    let host_offline = ctx.work_db.execution_bound_host_offline(&execution.id).unwrap_or(false);

    if !host_offline {
        // The confirmation MUST list the workspaces of the cube that owns
        // this lease. Asking the engine's local cube about a remote
        // workspace id returns "no such workspace", which `classify` reads
        // as `Dead` — turning the guard that exists to stop mass-reaping
        // live workers into an unconditional yes for every remote run. A
        // routing failure here is treated exactly like a failed probe:
        // `Unknown`, so nothing is reaped on it.
        let cube = match ctx.cubes.cube_for_execution(&execution.id).await {
            Ok(resolved) => resolved.cube,
            Err(err) => {
                ctx.breaker.forget(&execution.id);
                tracing::warn!(
                    execution_id = %execution.id,
                    lease_id,
                    consecutive_failures,
                    error = %format!("{err:#}"),
                    "cube-lease heartbeat: consecutive failures reached auto-reap threshold but the owning cube \
                     could not be resolved to confirm death; not reaping (an unroutable cube is not proof the \
                     workspace is gone)",
                );
                return false;
            }
        };
        let verdict = run_reconcile::confirm_execution_dead(
            cube.as_ref(),
            execution,
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .await;
        if !matches!(verdict, RunReconcileVerdict::Dead) {
            // Sustained heartbeat failure alone is not proof of death — could be
            // a transient cube CLI error or a cube outage hitting every lease at
            // once. Only a confirmed `Dead` verdict from cube's own workspace
            // snapshot is. Reset the streak so a genuine cube blip doesn't leave
            // us one failure away from reaping a live worker on the next pass;
            // if the lease really is gone, it fails to heartbeat again and the
            // streak rebuilds to another confirmation attempt.
            ctx.breaker.forget(&execution.id);
            tracing::warn!(
                execution_id = %execution.id,
                lease_id,
                consecutive_failures,
                ?verdict,
                "cube-lease heartbeat: consecutive failures reached auto-reap threshold but cube workspace list did not \
                 confirm the lease is dead; not reaping (avoids mass-reaping live workers during a cube outage)",
            );
            return false;
        }
    }

    let death_clause = if host_offline {
        format!(
            "its cube lease `{lease_id}` was no longer tracked after {consecutive_failures} consecutive heartbeat \
             failures and its host is offline (disabled/removed)"
        )
    } else {
        format!(
            "its cube lease `{lease_id}` was no longer tracked after {consecutive_failures} consecutive heartbeat \
             failures"
        )
    };
    let reason = if host_offline {
        format!(
            "cube-lease heartbeat: lease `{lease_id}` failed to refresh {consecutive_failures} consecutive times and \
             its host is offline (disabled/removed); treating the workspace as gone and auto-reaping (same terminal \
             path as `bossctl agents reap`)"
        )
    } else {
        format!(
            "cube-lease heartbeat: lease `{lease_id}` failed to refresh {consecutive_failures} consecutive times; \
             treating the workspace as gone and auto-reaping (same terminal path as `bossctl agents reap`)"
        )
    };

    let details = serde_json::json!({
        "reason": if host_offline { "host_offline" } else { "heartbeat_failures_exhausted" },
        "host_offline": host_offline,
        "consecutive_failures": consecutive_failures,
    });

    let reaped = reap_and_release_lease(
        ctx,
        execution,
        lease_id,
        &reason,
        &death_clause,
        Stage::CubeLeaseAutoReap,
        details,
    )
    .await;

    if reaped {
        tracing::warn!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            lease_id,
            consecutive_failures,
            "cube-lease heartbeat: auto-reaped execution after repeated heartbeat failures",
        );
    }

    reaped
}

/// Shared terminal path for an execution this sweep has independently
/// proven dead: mark it orphaned, tear down driver-owned state, finalize
/// kind-specific bookkeeping, and — unlike the other DB-driven local
/// reapers ([`crate::dead_pane_sweep`], [`crate::lost_workspace_sweep`]),
/// which deliberately preserve the lease so a same-workspace resume can
/// reclaim it — release the cube lease through
/// [`crate::workspace_lease_release::release_lease_after_inferred_death`]
/// so unpublished work is preserved first. Used by both
/// [`auto_reap_dead_lease`] (consecutive-heartbeat-failure path, confirmed
/// via `cube workspace list`) and the DB-fallback sweep's own durable
/// liveness gate (confirmed via a dead or never-reported worker pid) in
/// [`run_one_pass_impl`] — in both cases the caller has already
/// independently established the worker is gone, so unlike a bare
/// heartbeat failure this reap does not need to wait out the lease TTL.
///
/// `reason` is the full sentence recorded on `mark_execution_orphaned` /
/// `force_release_lease`; `death_clause` is the shorter clause
/// [`crate::execution_liveness::finalize_dead_automation_triage_run`]
/// interpolates into its own "no task was produced (pane died before Stop:
/// {death_clause})" / "produced task X (recovered after pane death before
/// Stop: {death_clause})" sentences — passing the full `reason` there would
/// nest one sentence inside another and read incoherently.
///
/// Returns `true` iff this call actually transitioned the row to terminal
/// (idempotent against a race with any other reconciler: if the row is
/// already terminal by the time we get here, this is a no-op that still
/// clears the breaker streak).
async fn reap_and_release_lease(
    ctx: &HeartbeatCtx<'_>,
    execution: &WorkExecution,
    lease_id: &str,
    reason: &str,
    death_clause: &str,
    stage: Stage,
    details: serde_json::Value,
) -> bool {
    if let Err(err) = ctx.work_db.mark_execution_orphaned(&execution.id, reason) {
        // A concurrent sweep/guard/hook may have finalized this execution
        // between our snapshot and now — that's success from our
        // perspective (the row is no longer a live blocker), just not
        // something we get to take credit for.
        let already_terminal = ctx
            .work_db
            .get_execution(&execution.id)
            .map(|cur| cur.status.is_terminal())
            .unwrap_or(false);
        ctx.breaker.forget(&execution.id);
        if already_terminal {
            return false;
        }
        tracing::warn!(
            execution_id = %execution.id,
            error = %format!("{err:#}"),
            "cube-lease heartbeat: reap failed to mark execution orphaned; leaving row as-is",
        );
        return false;
    }
    ctx.breaker.forget(&execution.id);

    // `mark_execution_orphaned` preserves `workspace_path`, so the pre-call
    // `execution` snapshot is still current.
    crate::driver_teardown::teardown_driver_workspace(
        ctx.work_db,
        &execution.id,
        execution.workspace_path.as_deref().map(std::path::Path::new),
        crate::driver_teardown::TeardownReason::CubeLeaseAutoReap,
    )
    .await;

    if execution.kind == ExecutionKind::AutomationTriage {
        crate::execution_liveness::finalize_dead_automation_triage_run(ctx.work_db, execution, death_clause);
    }

    // Mirror host_reconcile::drain_execution's pr_review bookkeeping. The
    // shared helper advances only when the reaped pass already recorded a
    // durable ReviewResult; a missing-result pass stays in Doing for the
    // dead-review recovery sweep to re-fire.
    if execution.kind == ExecutionKind::PrReview {
        match ctx
            .work_db
            .advance_pending_review_task_to_in_review(&execution.work_item_id)
        {
            Ok(true) => tracing::info!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                "cube-lease heartbeat: reaped pr_review's completed result advanced task to in_review",
            ),
            Ok(false) => {} // No result, already past `active`, or another worker holds it.
            Err(err) => tracing::warn!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                error = %format!("{err:#}"),
                "cube-lease heartbeat: failed to advance reaped pr_review's task to in_review",
            ),
        }
    }

    // Actively release the lease rather than leaving it to expire on its
    // own TTL — the fix this module exists for. Released on the cube that
    // ISSUED it: releasing a remote lease id against the engine's local
    // cube is a guaranteed no-op that leaves the remote workspace stranded
    // for the full 24 h TTL. Best-effort throughout: the lease is already
    // known to be stale (either it has been failing to heartbeat, or we
    // independently proved the worker is gone), so a failure here — cube
    // already reclaimed it, or the host is unreachable — is benign and
    // logged rather than propagated.
    match ctx.cubes.cube_for_execution(&execution.id).await {
        Ok(resolved) => {
            crate::workspace_lease_release::release_lease_after_inferred_death_with(
                ctx.work_db,
                execution,
                crate::workspace_lease_release::PaneStatus::Gone,
                resolved.host_adapter.as_deref(),
                ctx.occupancy,
                Some(resolved.cube.as_ref()),
                resolved.cube.force_release_lease(lease_id, Some(reason)),
            )
            .await;
        }
        Err(err) => tracing::warn!(
            execution_id = %execution.id,
            lease_id,
            error = %format!("{err:#}"),
            "cube-lease heartbeat: reaped the execution but could not resolve its owning cube to force-release \
             the lease; it will be reclaimed by cube's own TTL sweep instead",
        ),
    }

    ctx.dispatch_events
        .emit(
            DispatchEvent::new(stage, Outcome::Ok, &execution.id)
                .with_work_item(&execution.work_item_id)
                .with_cube_lease(lease_id)
                .with_details(details),
        )
        .await;

    true
}

/// Re-heartbeat, once at engine startup, the cube lease of every
/// persisted in-flight execution the startup probe classified `Live`.
///
/// The periodic [`run_one_pass`] sweep keys off the in-memory live-worker
/// registry, which is empty immediately after a restart (rebuilt as
/// workers re-send hook events). Without this, a worker that legitimately
/// outlived the engine restart could have its lease lapse in the gap
/// before its next hook re-registers it. We only touch `Live` verdicts —
/// cube confirmed the lease is still bound to our recorded id and not yet
/// expired — so we never extend a lease that already belongs to someone
/// else. Best-effort: failures are logged, never fatal. Returns the
/// number of leases successfully refreshed.
pub async fn reheartbeat_live_runs(
    cubes: &dyn ExecutionCubes,
    in_flight: &[WorkExecution],
    report: &RunReconcileReport,
) -> usize {
    let mut heartbeated = 0usize;
    for execution in in_flight {
        if !matches!(
            report.verdicts.get(&execution.id).copied(),
            Some(RunReconcileVerdict::Live)
        ) {
            continue;
        }
        let Some(lease_id) = execution.cube_lease_id.as_deref() else {
            continue;
        };
        // Same host routing as the periodic sweep: a remote lease is
        // re-adopted against the remote cube that issued it. Best-effort at
        // startup like every other beat here, so an unreachable host logs
        // and the periodic sweep retries rather than blocking boot.
        let cube = match cubes.cube_for_execution(&execution.id).await {
            Ok(resolved) => resolved.cube,
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    lease_id,
                    error = %format!("{err:#}"),
                    "cube-lease heartbeat: could not resolve the owning cube to re-adopt a live lease at startup \
                     (best-effort; the periodic sweep retries)",
                );
                continue;
            }
        };
        let result = tokio::time::timeout(
            HEARTBEAT_CUBE_TIMEOUT,
            cube.heartbeat_lease(lease_id, Some(LEASE_TTL_SECS)),
        )
        .await;
        match result {
            Ok(Ok(())) => {
                heartbeated += 1;
                tracing::info!(
                    execution_id = %execution.id,
                    lease_id,
                    ttl_secs = LEASE_TTL_SECS,
                    "cube-lease heartbeat: re-adopted live lease at startup",
                );
            }
            Ok(Err(err)) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    lease_id,
                    error = %format!("{err:#}"),
                    "cube-lease heartbeat: failed to re-adopt live lease at startup (best-effort)",
                );
            }
            Err(_elapsed) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    lease_id,
                    timeout_secs = HEARTBEAT_CUBE_TIMEOUT.as_secs(),
                    "cube-lease heartbeat: startup re-adoption timed out (cube subprocess may be hung); skipping this lease",
                );
            }
        }
    }
    heartbeated
}

#[cfg(test)]
#[path = "cube_lease_heartbeat_tests.rs"]
mod tests;
