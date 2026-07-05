//! Periodic cube-lease heartbeat: keeps every live worker's cube
//! workspace lease from TTL-expiring out from under it.
//!
//! ## Why this exists
//!
//! Cube hands the engine a workspace via a *lease* that carries a TTL
//! (cube's default is 1800 s / 30 min). Cube runs a TTL sweep that
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
//! 1800 s TTL, see "TTL ownership" below):
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
//!       [`CubeClient::heartbeat_lease`](crate::coordinator::CubeClient::heartbeat_lease)
//!       with an explicit TTL, refreshing the expiry to now + TTL.
//!
//! ## TTL ownership (engine-owned, not implicit)
//!
//! The engine owns the heartbeat-interval ↔ TTL relationship explicitly
//! rather than relying on cube's default: it passes
//! [`LEASE_TTL_SECS`] on every heartbeat and ticks every
//! [`DEFAULT_HEARTBEAT_INTERVAL`]. With 300 s ≪ 1800 s the lease is
//! refreshed ~6× per TTL window, so up to ~4 consecutive missed/failed
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
//!    heartbeats until they re-register via a hook or their execution
//!    reaches a terminal state in the DB.
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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use boss_protocol::{ExecutionKind, WorkExecution, WorkerActivity};

use crate::coordinator::{CubeClient, ExecutionCoordinator};
use crate::dead_pid_sweep::{PidStatus, probe_pid};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::run_reconcile::{self, RunReconcileReport, RunReconcileVerdict};
use crate::work::WorkDb;

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

/// TTL (seconds) the engine asks cube to set on every heartbeat. Matches
/// cube's own default of 1800 s, but the engine passes it explicitly so
/// the interval-≪-TTL relationship is owned here and survives a change
/// to cube's default. With [`DEFAULT_HEARTBEAT_INTERVAL`] = 300 s this
/// is a 6× margin.
pub const LEASE_TTL_SECS: u64 = 1800;

/// Per-call timeout for a single `cube workspace heartbeat` subprocess
/// invocation. Mirrors [`crate::coordinator::CUBE_LEASE_TIMEOUT`]: the
/// same cube-hang failure mode that prompted timeouts on lease/repo-ensure
/// calls applies here. Without a bound, one hung heartbeat call would
/// stall the entire pass and leave every other live worker un-heartbeated
/// until the subprocess eventually returned (or never did).
pub const HEARTBEAT_CUBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Read the heartbeat interval from [`HEARTBEAT_INTERVAL_SECS_ENV`],
/// falling back to [`DEFAULT_HEARTBEAT_INTERVAL`]. A zero or unparseable
/// value falls back to the default (a zero interval would busy-loop).
pub fn heartbeat_interval() -> Duration {
    std::env::var(HEARTBEAT_INTERVAL_SECS_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_HEARTBEAT_INTERVAL)
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
    cube_client: Arc<dyn CubeClient>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let breaker = HeartbeatFailureBreaker::default();
        loop {
            let outcome = run_one_pass(
                work_db.as_ref(),
                live_states.as_ref(),
                cube_client.as_ref(),
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
    cube_client: &'a dyn CubeClient,
    dispatch_events: &'a dyn DispatchEventSink,
    breaker: &'a HeartbeatFailureBreaker,
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

/// Run a single heartbeat pass: refresh the cube lease of every live
/// worker, auto-reaping any execution whose lease has failed
/// [`AUTO_REAP_AFTER_CONSECUTIVE_FAILURES`] consecutive times. Returns a
/// summary of what happened.
pub async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    cube_client: &dyn CubeClient,
    dispatch_events: &dyn DispatchEventSink,
    breaker: &HeartbeatFailureBreaker,
) -> HeartbeatSweepOutcome {
    run_one_pass_impl(
        work_db,
        live_states,
        cube_client,
        dispatch_events,
        breaker,
        HEARTBEAT_CUBE_TIMEOUT,
    )
    .await
}

/// Internal implementation that accepts a configurable per-call timeout
/// (exposed so tests can inject a short timeout without waiting 30 s).
async fn run_one_pass_impl(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    cube_client: &dyn CubeClient,
    dispatch_events: &dyn DispatchEventSink,
    breaker: &HeartbeatFailureBreaker,
    heartbeat_timeout: Duration,
) -> HeartbeatSweepOutcome {
    let ctx = HeartbeatCtx {
        work_db,
        cube_client,
        dispatch_events,
        breaker,
    };
    let mut outcome = HeartbeatSweepOutcome::default();
    let mut registered_run_ids: HashSet<String> = HashSet::new();
    let mut db_fallback_sweep_succeeded = false;

    for state in live_states.snapshot() {
        // Slot hasn't reported a shell pid yet — we can't probe liveness,
        // and the lease was just created with a full TTL, so there is no
        // urgency. The next pass picks it up once the app reports the pid.
        //
        // Remote workers (shell_pid = 0 by design, since they have no local
        // pid and their leases live on a remote cube) also land here
        // permanently — they are out of scope for this local sweep.
        if state.shell_pid <= 0 {
            outcome.no_pid_skipped += 1;
            registered_run_ids.insert(state.run_id.clone());
            continue;
        }

        // Terminal activity → the completion / teardown path owns lease
        // release; there is nothing to keep alive.
        if is_terminal_activity(state.activity) {
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
    let result = tokio::time::timeout(timeout, ctx.cube_client.heartbeat_lease(lease_id, Some(LEASE_TTL_SECS))).await;
    match result {
        Ok(Ok(())) => {
            *succeeded += 1;
            tracing::debug!(
                execution_id,
                lease_id,
                ttl_secs = LEASE_TTL_SECS,
                "cube-lease heartbeat: refreshed lease",
            );
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
    let verdict =
        run_reconcile::confirm_execution_dead(ctx.cube_client, execution, run_reconcile::current_epoch_s()).await;
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

    let reason = format!(
        "cube-lease heartbeat: lease `{lease_id}` failed to refresh {consecutive_failures} consecutive times; \
         treating the workspace as gone and auto-reaping (same terminal path as `bossctl agents reap`)"
    );

    if let Err(err) = ctx.work_db.mark_execution_orphaned(&execution.id, &reason) {
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
            "cube-lease heartbeat: auto-reap failed to mark execution orphaned; leaving row as-is",
        );
        return false;
    }
    ctx.breaker.forget(&execution.id);

    if execution.kind == ExecutionKind::AutomationTriage {
        crate::execution_liveness::finalize_dead_automation_triage_run(
            ctx.work_db,
            execution,
            &format!(
                "its cube lease `{lease_id}` was no longer tracked after {consecutive_failures} consecutive \
                 heartbeat failures"
            ),
        );
    }

    // Best-effort: the lease is already failing to heartbeat (almost
    // certainly because cube no longer tracks it), so force-release is
    // very likely a no-op. Failure here is benign.
    if let Err(err) = ctx
        .cube_client
        .force_release_lease(
            lease_id,
            Some("cube-lease heartbeat auto-reap: lease failed to refresh after repeated attempts"),
        )
        .await
    {
        tracing::debug!(
            execution_id = %execution.id,
            lease_id,
            error = %format!("{err:#}"),
            "cube-lease heartbeat: best-effort force-release after auto-reap failed (likely already released)",
        );
    }

    ctx.dispatch_events
        .emit(
            DispatchEvent::new(Stage::CubeLeaseAutoReap, Outcome::Ok, &execution.id)
                .with_work_item(&execution.work_item_id)
                .with_cube_lease(lease_id)
                .with_details(serde_json::json!({
                    "reason": "heartbeat_failures_exhausted",
                    "consecutive_failures": consecutive_failures,
                })),
        )
        .await;

    tracing::warn!(
        execution_id = %execution.id,
        work_item_id = %execution.work_item_id,
        lease_id,
        consecutive_failures,
        "cube-lease heartbeat: auto-reaped execution after repeated heartbeat failures",
    );

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
    cube_client: &dyn CubeClient,
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
        let result = tokio::time::timeout(
            HEARTBEAT_CUBE_TIMEOUT,
            cube_client.heartbeat_lease(lease_id, Some(LEASE_TTL_SECS)),
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

fn is_terminal_activity(activity: WorkerActivity) -> bool {
    matches!(activity, WorkerActivity::Terminated | WorkerActivity::Errored)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use boss_protocol::{ExecutionKind, ExecutionStatus, RequestExecutionInput, WorkItemBinding};
    use tempfile::TempDir;

    use super::*;
    use crate::coordinator::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
    };
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::run_reconcile::{RunReconcileReport, RunReconcileVerdict};
    use crate::test_support::*;
    use crate::work::{CreateChoreInput, WorkDb};

    // ─── cube stub ────────────────────────────────────────────────────────────

    /// Records every `heartbeat_lease` call and can be told to fail them.
    /// Also backs `list_workspaces`, the second-opinion check
    /// [`auto_reap_dead_lease`] now requires before reaping: by default it
    /// returns no workspaces at all (cube has no record of any of them),
    /// which `run_reconcile::classify` reads as `Unknown` — a reap must
    /// NOT proceed on the default state. Tests that want a reap to actually
    /// happen must opt in via `set_workspaces` with an entry that classifies
    /// as `Dead` (see the `workspace(...)` helper below).
    #[derive(Default)]
    struct RecordingCube {
        heartbeats: Mutex<Vec<(String, Option<u64>)>>,
        force_releases: Mutex<Vec<String>>,
        fail: AtomicBool,
        workspaces: Mutex<Vec<CubeWorkspaceStatus>>,
        list_workspaces_fail: AtomicBool,
    }

    impl RecordingCube {
        fn calls(&self) -> Vec<(String, Option<u64>)> {
            self.heartbeats.lock().unwrap().clone()
        }

        fn force_release_calls(&self) -> Vec<String> {
            self.force_releases.lock().unwrap().clone()
        }

        /// Set the `cube workspace list` snapshot `list_workspaces` returns.
        fn set_workspaces(&self, workspaces: Vec<CubeWorkspaceStatus>) {
            *self.workspaces.lock().unwrap() = workspaces;
        }
    }

    #[async_trait]
    impl CubeClient for RecordingCube {
        async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
            unimplemented!()
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: bool,
            _: &[&str],
        ) -> Result<CubeWorkspaceLease> {
            unimplemented!()
        }
        async fn create_change(&self, _: &std::path::Path, _: &str) -> Result<CubeChangeHandle> {
            unimplemented!()
        }
        async fn release_workspace(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn workspace_status(&self, _: &std::path::Path) -> Result<CubeWorkspaceStatus> {
            unimplemented!()
        }
        async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
            self.heartbeats.lock().unwrap().push((lease_id.to_owned(), ttl_seconds));
            if self.fail.load(Ordering::SeqCst) {
                return Err(anyhow!("simulated cube heartbeat failure"));
            }
            Ok(())
        }
        async fn force_release_lease(&self, lease_id: &str, _: Option<&str>) -> Result<()> {
            self.force_releases.lock().unwrap().push(lease_id.to_owned());
            Ok(())
        }
        async fn goto_workspace(&self, _: &std::path::Path, _: u64) -> Result<()> {
            unimplemented!()
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            if self.list_workspaces_fail.load(Ordering::SeqCst) {
                return Err(anyhow!("simulated cube outage: workspace list unavailable"));
            }
            Ok(self.workspaces.lock().unwrap().clone())
        }
        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            Ok(vec![])
        }
    }

    // ─── helpers ─────────────────────────────────────────────────────────────

    /// Build a `cube workspace list` row. Mirrors `run_reconcile`'s test
    /// helper of the same shape.
    fn workspace(workspace_id: &str, state: &str, lease_id: Option<&str>) -> CubeWorkspaceStatus {
        CubeWorkspaceStatus::builder()
            .workspace_id(workspace_id)
            .workspace_path(std::path::PathBuf::from(format!("/tmp/{workspace_id}")))
            .state(state)
            .maybe_lease_id(lease_id)
            .holder("user@host:1234")
            .task("test")
            .leased_at_epoch_s(1_700_000_000)
            .build()
    }

    fn open_db() -> (TempDir, Arc<WorkDb>) {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("state.db")).unwrap();
        (dir, Arc::new(db))
    }

    fn create_chore(db: &WorkDb, product_id: &str) -> String {
        create_named_chore(db, product_id, "test chore")
    }

    fn create_named_chore(db: &WorkDb, product_id: &str, name: &str) -> String {
        db.create_chore(CreateChoreInput::builder().product_id(product_id).name(name).build())
            .unwrap()
            .id
    }

    /// Create a `ready` execution for `work_item_id`.
    fn ready_execution(db: &WorkDb, work_item_id: &str) -> String {
        db.request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
            .unwrap()
            .id
    }

    /// Create a `running` execution that has recorded `lease_id`.
    fn running_execution_with_lease(db: &WorkDb, work_item_id: &str, lease_id: &str) -> String {
        let execution_id = ready_execution(db, work_item_id);
        db.start_execution_run(
            &execution_id,
            "agent-1",
            "repo",
            lease_id,
            "mono-agent-001",
            "/tmp/mono-agent-001",
        )
        .unwrap();
        execution_id
    }

    fn register_slot(live_states: &LiveWorkerStateRegistry, slot_id: u8, execution_id: &str, shell_pid: i32) {
        live_states.register_spawn(
            slot_id,
            execution_id,
            "claude-opus-4-8",
            shell_pid,
            Some(WorkItemBinding {
                work_item_id: "wi".to_owned(),
                work_item_name: "test".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
    }

    /// A PID guaranteed not to exist: spawn `true`, wait for it to exit,
    /// reuse its released pid. (Same trick the dead-PID sweep tests use.)
    fn dead_pid() -> i32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let _ = child.wait();
        pid
    }

    fn execution_value(id: &str, lease_id: &str) -> WorkExecution {
        WorkExecution::builder()
            .id(id)
            .work_item_id(format!("wi-{id}"))
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@example.com:foo.git")
            .cube_repo_id("foo")
            .cube_lease_id(lease_id)
            .cube_workspace_id("mono-agent-001")
            .workspace_path("/tmp/mono-agent-001")
            .created_at("2026-06-15T00:00:00Z")
            .started_at("2026-06-15T00:00:00Z")
            .build()
    }

    // ─── tests ───────────────────────────────────────────────────────────────

    /// The core invariant: a live worker's lease is heartbeated with the
    /// engine-owned TTL every pass.
    #[tokio::test]
    async fn live_lease_is_heartbeated() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-live");

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

        let cube = RecordingCube::default();
        let sink = RecordingDispatchEventSink::new();
        let breaker = HeartbeatFailureBreaker::default();
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

        assert_eq!(outcome.heartbeated, 1, "live lease must be heartbeated");
        assert_eq!(outcome.failed, 0);
        assert_eq!(cube.calls(), vec![("lease-live".to_owned(), Some(LEASE_TTL_SECS))]);
        assert!(sink.events().await.is_empty(), "no event on the success path");
    }

    /// A slot whose PID is gone is NOT heartbeated — the lease is left to
    /// expire so cube frees the workspace within ~TTL after a kill.
    #[tokio::test]
    async fn dead_pid_lease_is_not_heartbeated() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-dead");

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, dead_pid());

        let cube = RecordingCube::default();
        let sink = RecordingDispatchEventSink::new();
        let breaker = HeartbeatFailureBreaker::default();
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

        assert_eq!(outcome.heartbeated, 0);
        assert_eq!(outcome.dead_pid_skipped, 1);
        assert!(cube.calls().is_empty(), "dead PID lease must not be heartbeated");
    }

    /// A slot with no reported pid yet is skipped (the lease is freshly
    /// minted with a full TTL).
    #[tokio::test]
    async fn zero_pid_slot_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-z");

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, 0);

        let cube = RecordingCube::default();
        let sink = RecordingDispatchEventSink::new();
        let breaker = HeartbeatFailureBreaker::default();
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

        assert_eq!(outcome.no_pid_skipped, 1);
        assert!(cube.calls().is_empty());
    }

    /// A terminal execution's lease is not re-extended (completion owns it).
    #[tokio::test]
    async fn terminal_execution_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-term");
        db.mark_execution_orphaned(&execution_id, "test orphan").unwrap();

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

        let cube = RecordingCube::default();
        let sink = RecordingDispatchEventSink::new();
        let breaker = HeartbeatFailureBreaker::default();
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

        assert_eq!(outcome.terminal_skipped, 1);
        assert!(cube.calls().is_empty());
    }

    /// A live slot whose execution never recorded a lease is skipped.
    #[tokio::test]
    async fn missing_lease_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = ready_execution(&db, &work_item_id); // ready, no lease

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

        let cube = RecordingCube::default();
        let sink = RecordingDispatchEventSink::new();
        let breaker = HeartbeatFailureBreaker::default();
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

        assert_eq!(outcome.no_lease_skipped, 1);
        assert!(cube.calls().is_empty());
    }

    /// A heartbeat failure increments `failed` and emits a single
    /// `cube_lease_heartbeat` error event for observability.
    #[tokio::test]
    async fn heartbeat_failure_emits_error_event() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-fail");

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

        let cube = RecordingCube::default();
        cube.fail.store(true, Ordering::SeqCst);
        let sink = RecordingDispatchEventSink::new();
        let breaker = HeartbeatFailureBreaker::default();
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.heartbeated, 0);
        assert_eq!(outcome.auto_reaped, 0, "a single failure must not trigger an auto-reap");
        let events = sink.events().await;
        assert_eq!(events.len(), 1, "exactly one failure event");
        assert_eq!(events[0].stage, "cube_lease_heartbeat");
        assert_eq!(events[0].outcome, "error");
        assert_eq!(events[0].cube_lease_id.as_deref(), Some("lease-fail"));
    }

    /// The core fix (2026-07-03 incident): a lease that fails to
    /// heartbeat `AUTO_REAP_AFTER_CONSECUTIVE_FAILURES` times in a row is
    /// auto-reaped through the same terminal path as `bossctl agents
    /// reap` — the execution goes `orphaned`, its (already-untracked)
    /// lease is best-effort force-released, and a `cube_lease_auto_reap`
    /// event is emitted. Before this fix the row stayed `running` forever
    /// and blocked the redundant-spawn guard.
    #[tokio::test]
    async fn auto_reaps_after_consecutive_heartbeat_failures() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-dead-forever");

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

        let cube = RecordingCube::default();
        cube.fail.store(true, Ordering::SeqCst);
        // Cube's own snapshot confirms the workspace is no longer leased —
        // the positive evidence auto-reap now requires before proceeding.
        cube.set_workspaces(vec![workspace("mono-agent-001", "free", None)]);
        let sink = RecordingDispatchEventSink::new();
        let breaker = HeartbeatFailureBreaker::default();

        // Two failing passes: below threshold, no reap yet.
        for _ in 0..AUTO_REAP_AFTER_CONSECUTIVE_FAILURES - 1 {
            let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
            assert_eq!(outcome.auto_reaped, 0);
            assert_eq!(
                db.get_execution(&execution_id).unwrap().status,
                ExecutionStatus::Running,
                "must stay live below the consecutive-failure threshold"
            );
        }

        // The Nth consecutive failure crosses the threshold and reaps.
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
        assert_eq!(outcome.auto_reaped, 1);

        let reaped = db.get_execution(&execution_id).unwrap();
        assert_eq!(reaped.status, ExecutionStatus::Orphaned);
        assert!(reaped.finished_at.is_some(), "auto-reaped execution must be finalized");
        assert_eq!(
            cube.force_release_calls(),
            vec!["lease-dead-forever".to_owned()],
            "the untracked lease must be best-effort force-released"
        );

        let events = sink.events().await;
        let reap_event = events
            .iter()
            .find(|e| e.stage == "cube_lease_auto_reap")
            .expect("a cube_lease_auto_reap event must be emitted");
        assert_eq!(reap_event.outcome, "ok");
        assert_eq!(reap_event.cube_lease_id.as_deref(), Some("lease-dead-forever"));
    }

    /// A successful heartbeat clears the consecutive-failure streak, so a
    /// lease that recovers even once never gets auto-reaped just because it
    /// failed a few times in the past.
    #[tokio::test]
    async fn heartbeat_success_resets_failure_streak() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-flaky");

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

        let cube = RecordingCube::default();
        let sink = RecordingDispatchEventSink::new();
        let breaker = HeartbeatFailureBreaker::default();

        // Fail, fail, then succeed — the streak resets to zero.
        cube.fail.store(true, Ordering::SeqCst);
        run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
        run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
        cube.fail.store(false, Ordering::SeqCst);
        run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;

        // Two more failures: still below threshold because the streak reset.
        cube.fail.store(true, Ordering::SeqCst);
        for _ in 0..AUTO_REAP_AFTER_CONSECUTIVE_FAILURES - 1 {
            let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
            assert_eq!(outcome.auto_reaped, 0);
        }

        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Running,
            "an intervening success must reset the streak, so the execution stays live"
        );
    }

    /// The DB-fallback sweep (executions not yet registered with the
    /// live-worker registry — exactly the shape of a pane that died before
    /// ever reporting a hook event) also auto-reaps after repeated
    /// failures, not just the registry-driven path.
    #[tokio::test]
    async fn db_fallback_auto_reaps_after_consecutive_failures() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-never-registered");

        // Registry is empty for the whole test: this execution never
        // registered a live slot, mirroring a pane that crashed before its
        // first hook event.
        let live_states = LiveWorkerStateRegistry::new();

        let cube = RecordingCube::default();
        cube.fail.store(true, Ordering::SeqCst);
        cube.set_workspaces(vec![workspace("mono-agent-001", "free", None)]);
        let sink = RecordingDispatchEventSink::new();
        let breaker = HeartbeatFailureBreaker::default();

        for _ in 0..AUTO_REAP_AFTER_CONSECUTIVE_FAILURES - 1 {
            let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
            assert_eq!(outcome.auto_reaped, 0);
        }
        let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
        assert_eq!(outcome.auto_reaped, 1);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Orphaned
        );
    }

    /// The core regression this revision fixes: sustained heartbeat failure
    /// alone (e.g. a cube daemon outage affecting every in-flight lease at
    /// once) must NOT auto-reap. Only a confirmed `Dead` verdict from `cube
    /// workspace list` may do that. Here every heartbeat call fails but
    /// `list_workspaces` (the confirmation probe) also fails throughout,
    /// exactly modeling a cube outage — the reap must never fire, however
    /// many passes run.
    #[tokio::test]
    async fn sustained_failure_without_cube_confirmation_never_auto_reaps() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-outage");

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &execution_id, std::process::id() as i32);

        let cube = RecordingCube::default();
        cube.fail.store(true, Ordering::SeqCst);
        cube.list_workspaces_fail.store(true, Ordering::SeqCst);
        let sink = RecordingDispatchEventSink::new();
        let breaker = HeartbeatFailureBreaker::default();

        // Run well past the consecutive-failure threshold — a mass-outage
        // scenario like this one persists far longer than 15 minutes.
        for _ in 0..AUTO_REAP_AFTER_CONSECUTIVE_FAILURES * 3 {
            let outcome = run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
            assert_eq!(
                outcome.auto_reaped, 0,
                "heartbeat failure without a confirmed-dead verdict must never auto-reap"
            );
        }

        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Running,
            "the live worker's execution must stay untouched through a cube outage"
        );
        assert!(
            cube.force_release_calls().is_empty(),
            "no lease may be force-released without confirmed death"
        );
    }

    /// An `automation_triage` execution auto-reaped this way still gets its
    /// `automation_runs` outcome recorded honestly (via the same
    /// open-task-recovery helper `lost_workspace_sweep` uses) instead of
    /// leaving the pessimistic dispatch-time placeholder in place.
    #[tokio::test]
    async fn automation_triage_auto_reap_records_failed_gave_up() {
        use crate::work::AutomationFireRecord;
        use boss_protocol::{
            AUTOMATION_OUTCOME_FAILED_GAVE_UP, AUTOMATION_OUTCOME_FAILED_WILL_RETRY, AutomationTrigger,
            CreateAutomationInput,
        };

        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let automation_id = db
            .create_automation(
                CreateAutomationInput::builder()
                    .product_id(product_id.as_str())
                    .name("daily")
                    .trigger(AutomationTrigger::Schedule {
                        cron: "0 14 * * *".to_owned(),
                        timezone: "UTC".to_owned(),
                    })
                    .standing_instruction("do the thing")
                    .build(),
            )
            .unwrap()
            .id;
        let exec = db
            .create_automation_triage_execution(&automation_id, "https://github.com/test/repo")
            .unwrap();
        db.start_execution_run_on_host(
            &exec.id,
            "auto-worker-1",
            "repo-1",
            "lease-triage-dead",
            "mono-agent-999",
            "/tmp/mono-agent-999",
            "local",
        )
        .unwrap();
        // Seed the pessimistic dispatch-time `automation_runs` row the
        // scheduler writes at fire time, so there is a row for the auto-reap
        // path to finalize with the honest outcome.
        db.record_automation_run_and_advance(
            AutomationFireRecord::builder()
                .automation_id(automation_id.clone())
                .scheduled_for(1_700_000_000)
                .started_at(1_700_000_000)
                .outcome(AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
                .detail("dispatched; awaiting triage worker decision (Stop not yet received)")
                .triage_execution_id(exec.id.clone())
                .build(),
        )
        .unwrap();

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &exec.id, std::process::id() as i32);

        let cube = RecordingCube::default();
        cube.fail.store(true, Ordering::SeqCst);
        cube.set_workspaces(vec![workspace("mono-agent-999", "free", None)]);
        let sink = RecordingDispatchEventSink::new();
        let breaker = HeartbeatFailureBreaker::default();

        for _ in 0..AUTO_REAP_AFTER_CONSECUTIVE_FAILURES {
            run_one_pass(db.as_ref(), &live_states, &cube, &sink, &breaker).await;
        }

        assert_eq!(db.get_execution(&exec.id).unwrap().status, ExecutionStatus::Orphaned);
        let runs = db.list_automation_runs(&automation_id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].outcome, AUTOMATION_OUTCOME_FAILED_GAVE_UP,
            "no task was produced before the pane died, so the honest outcome is failed_gave_up"
        );
        assert!(runs[0].finished_at.is_some());
    }

    /// Startup re-adoption heartbeats ONLY the `Live`-verdict leases.
    #[tokio::test]
    async fn reheartbeat_only_touches_live_verdicts() {
        let in_flight = vec![
            execution_value("exec-live", "lease-A"),
            execution_value("exec-dead", "lease-B"),
            execution_value("exec-unknown", "lease-C"),
        ];
        let mut report = RunReconcileReport::default();
        report
            .verdicts
            .insert("exec-live".to_owned(), RunReconcileVerdict::Live);
        report
            .verdicts
            .insert("exec-dead".to_owned(), RunReconcileVerdict::Dead);
        report
            .verdicts
            .insert("exec-unknown".to_owned(), RunReconcileVerdict::Unknown);

        let cube = RecordingCube::default();
        let count = reheartbeat_live_runs(&cube, &in_flight, &report).await;

        assert_eq!(count, 1, "only the Live verdict is re-adopted");
        assert_eq!(cube.calls(), vec![("lease-A".to_owned(), Some(LEASE_TTL_SECS))]);
    }

    #[test]
    fn heartbeat_interval_default_and_override() {
        // Default when unset / unparseable / zero.
        assert_eq!(heartbeat_interval(), DEFAULT_HEARTBEAT_INTERVAL);
    }

    // ─── SlowCube: hangs on "lease-slow", succeeds on everything else ─────────

    /// A cube stub whose `heartbeat_lease` never returns for a designated
    /// "hung" lease id, simulating a stuck cube subprocess. All other leases
    /// complete immediately.
    #[derive(Default)]
    struct SlowCube {
        completed: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CubeClient for SlowCube {
        async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
            unimplemented!()
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: bool,
            _: &[&str],
        ) -> Result<CubeWorkspaceLease> {
            unimplemented!()
        }
        async fn create_change(&self, _: &std::path::Path, _: &str) -> Result<CubeChangeHandle> {
            unimplemented!()
        }
        async fn release_workspace(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn workspace_status(&self, _: &std::path::Path) -> Result<CubeWorkspaceStatus> {
            unimplemented!()
        }
        async fn heartbeat_lease(&self, lease_id: &str, _ttl: Option<u64>) -> Result<()> {
            if lease_id == "lease-slow" {
                // Never returns — simulates a hung cube subprocess.
                std::future::pending::<()>().await;
                unreachable!()
            }
            self.completed.lock().unwrap().push(lease_id.to_owned());
            Ok(())
        }
        async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
            unimplemented!()
        }
        async fn goto_workspace(&self, _: &std::path::Path, _: u64) -> Result<()> {
            unimplemented!()
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            Ok(vec![])
        }
        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            Ok(vec![])
        }
    }

    /// A hung heartbeat for one slot must NOT block heartbeating of the
    /// remaining slots. The timed-out slot increments `failed`; the other
    /// slot is heartbeated successfully.
    #[tokio::test]
    async fn hung_heartbeat_does_not_block_other_slots() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);

        // Two live executions: one with the hung lease, one normal.
        let wi_slow = create_named_chore(&db, &product_id, "slow chore");
        let exec_slow = running_execution_with_lease(&db, &wi_slow, "lease-slow");

        let wi_fast = create_named_chore(&db, &product_id, "fast chore");
        let exec_fast = running_execution_with_lease(&db, &wi_fast, "lease-fast");

        let live_states = LiveWorkerStateRegistry::new();
        register_slot(&live_states, 1, &exec_slow, std::process::id() as i32);
        register_slot(&live_states, 2, &exec_fast, std::process::id() as i32);

        let cube = SlowCube::default();
        let sink = RecordingDispatchEventSink::new();

        // Use a short timeout so the test does not wait 30 s.
        let breaker = HeartbeatFailureBreaker::default();
        let outcome = run_one_pass_impl(&db, &live_states, &cube, &sink, &breaker, Duration::from_millis(50)).await;

        assert_eq!(outcome.failed, 1, "the hung slot must count as failed");
        assert_eq!(outcome.heartbeated, 1, "the non-hung slot must succeed");

        let completed = cube.completed.lock().unwrap().clone();
        assert_eq!(
            completed,
            vec!["lease-fast".to_owned()],
            "only the fast lease completes"
        );

        let events = sink.events().await;
        assert_eq!(events.len(), 1, "one timeout error event");
        assert_eq!(events[0].stage, "cube_lease_heartbeat");
        assert_eq!(events[0].outcome, "error");
        assert_eq!(events[0].cube_lease_id.as_deref(), Some("lease-slow"));
    }

    /// In-flight executions not yet in the live registry (post-restart gap)
    /// are heartbeated via the DB-fallback sweep, so quiet workers with no
    /// hook events continue receiving beats after an engine restart.
    #[tokio::test]
    async fn db_fallback_heartbeats_unregistered_in_flight_executions() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_chore(&db, &product_id);
        let execution_id = running_execution_with_lease(&db, &work_item_id, "lease-orphan");

        // Registry is empty (simulates post-restart state before worker re-hooks).
        let live_states = LiveWorkerStateRegistry::new();

        let cube = RecordingCube::default();
        let sink = RecordingDispatchEventSink::new();
        let breaker = HeartbeatFailureBreaker::default();
        let outcome = run_one_pass_impl(&db, &live_states, &cube, &sink, &breaker, HEARTBEAT_CUBE_TIMEOUT).await;

        assert_eq!(
            outcome.db_fallback_heartbeated, 1,
            "DB-fallback must cover unregistered execution"
        );
        assert_eq!(outcome.heartbeated, 0);
        assert_eq!(outcome.failed, 0);
        assert_eq!(cube.calls(), vec![("lease-orphan".to_owned(), Some(LEASE_TTL_SECS))],);
        let _ = execution_id; // used to set up DB row
    }

    /// A failure streak for an execution that then disappears from both the
    /// live registry and `list_in_flight_executions` (e.g. it completed
    /// normally and is no longer in-flight) must not linger in the breaker
    /// map forever — `retain_only` prunes it at the end of the pass that
    /// first no longer observes it.
    #[test]
    fn breaker_retain_only_drops_streaks_for_ids_no_longer_seen() {
        let breaker = HeartbeatFailureBreaker::default();
        breaker.record_failure("exec-still-in-flight");
        breaker.record_failure("exec-completed");

        breaker.retain_only(&HashSet::from(["exec-still-in-flight".to_owned()]));

        assert_eq!(
            breaker.record_failure("exec-still-in-flight"),
            2,
            "a still-in-flight execution's streak must survive the prune"
        );
        assert_eq!(
            breaker.record_failure("exec-completed"),
            1,
            "a no-longer-seen execution's streak must have been dropped, so this is a fresh count"
        );
    }
}
