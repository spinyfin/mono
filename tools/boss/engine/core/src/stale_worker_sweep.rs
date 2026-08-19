//! Periodic liveness backstop that detects and reaps worker slots whose
//! `claude` process is still alive but has stopped making progress.
//!
//! ## The hang this guards against
//!
//! A worker can hard-hang without its OS process dying: it backgrounds a
//! pre-push `bazel build`/`bazel test`, then idles in a self-paced loop
//! "until both gates are green". If bazel wedges (host bazel-server
//! contention, `syspolicyd` hang), the status-log files it polls for are
//! never written, the completion notification never arrives, and the
//! worker waits forever. `activity` stays `working`, the PID stays alive,
//! and the worker is indistinguishable from one doing real work. See
//! issue #976 (observed on a Crusher chore).
//!
//! [`crate::dead_pid_sweep`] cannot catch this: `kill(pid, 0)` reports
//! the parked `claude` process as perfectly healthy. The distinguishing
//! signal is *transcript progress* — a wedged worker emits no hook
//! events. [`LiveWorkerState::last_event_at`] is stamped on every hook,
//! so "no event for N minutes while `working`" is the liveness gap.
//!
//! ## Algorithm
//!
//! 1. Snapshot [`crate::live_worker_state::LiveWorkerStateRegistry`].
//! 2. For each working slot, consult tmux when the run has durable tmux
//!    identity. A live pane with recent hook or pane activity is
//!    `AliveAndWorking`; a live, quiet pane whose agent is foreground is
//!    `AliveAndGenuinelyStuck` and files an inspection attention; an absent,
//!    mismatched, or dead pane is `Dead` and is reaped through the pane-death
//!    reconciliation path.
//! 3. Runs without terminal evidence use the conservative cadence fallback:
//!    1. Skip if a tool is in flight or the driver's progress fidelity is
//!       exempt from cadence reaping.
//!    2. Require a stale hook timestamp and an execution beyond its grace
//!       period before reaping it.
//! 4. For a confirmed cadence-stale slot: mark the execution `orphaned`,
//!    append an audit line, release the pool slot, emit a dispatch event, and
//!    kick the coordinator so stranded work is redispatched.
//!
//! ## False-positive guards (cadence fallback)
//!
//! These guards apply only to the cadence fallback in step 3 above — the
//! terminal branch in step 2 reaps on an authoritative dead-pane signal with
//! no tool-in-flight or cadence check at all. For the fallback, the
//! combination of (a) `activity == Working`, (b) no tool in flight, (c) the
//! driver's [`crate::driver::ProgressFidelity`] tier not being exempt from
//! cadence reaping (`fidelity_exempt_skipped`), and (d) a generous
//! [`DEFAULT_STALE_THRESHOLD_SECS`] (30 min) keeps it conservative. A worker
//! that is merely thinking, streaming a long response, or running a
//! foreground command emits a hook (or holds `current_tool`) well inside
//! that window; only a worker that has genuinely parked itself trips the
//! reap. The [`STALE_GRACE_SECS`] guard additionally skips executions whose
//! `started_at` is too recent to have a meaningful event history.
//!
//! ## Cadence
//!
//! Runs every 60 seconds and fires once immediately on boot (same
//! pattern as [`crate::dead_pid_sweep`] / [`crate::orphan_sweep`]).
//!
//! ## Driver-independent evidence
//!
//! Tmux's pane output and foreground-command fields are properties of the
//! terminal rather than of a driver's event vocabulary. The terminal path
//! therefore applies the same three-way classification to Rich, Coarse, and
//! Minimal drivers without a fidelity exemption.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use boss_protocol::WorkerActivity;
use boss_tmux::Tmux;

use crate::coordinator::{CubeClient, ExecutionCoordinator, worker_id_for_slot};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::hold_registry::HoldRegistry;
use crate::live_worker_state::{LiveWorkerStateRegistry, iso8601_utc};
use crate::work::WorkDb;

/// Engine-owned attention kind for a worker that is alive but has stopped
/// making observable terminal progress.
pub const STALE_WORKER_ATTENTION_KIND: &str = "stale_worker";

/// Why a tmux pane is classified dead. Only [`DeadPaneEvidence::PaneExited`]
/// observed a process exit; the other two are derived from session identity
/// and must be corroborated with [`crate::durable_liveness`] before reaping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadPaneEvidence {
    /// tmux reported `#{pane_dead} == 1`.
    PaneExited { pane_dead_status: Option<String> },
    /// The session name is absent from `list-sessions`.
    SessionAbsent,
    /// The live session's spawn token does not match the run row.
    SpawnTokenMismatch,
}

impl DeadPaneEvidence {
    fn requires_pid_corroboration(&self) -> bool {
        !matches!(self, Self::PaneExited { .. })
    }

    fn reap_reason(&self, session_name: &str) -> String {
        match self {
            Self::PaneExited {
                pane_dead_status: Some(status),
            } => format!("tmux pane dead in session {session_name}: pane_dead_status={status}"),
            Self::PaneExited { pane_dead_status: None } => format!("tmux pane dead in session {session_name}"),
            Self::SessionAbsent => {
                format!("tmux session {session_name} is absent from list-sessions")
            }
            Self::SpawnTokenMismatch => {
                format!("tmux session {session_name} spawn token does not match the run row")
            }
        }
    }
}

/// Tmux evidence for one worker pane. `window_activity_epoch_secs` is tmux's
/// epoch-second `#{window_activity}` value, which advances even while the
/// session is detached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalLiveness {
    /// The session and its token are present and the pane is still live.
    Alive {
        session_name: String,
        window_activity_epoch_secs: i64,
        agent_is_foreground: bool,
        /// `#{pane_current_command}` did not equal the driver descriptor
        /// binary. Counted on [`StaleWorkerSweepOutcome`] so an inert
        /// stuck-classifier is visible without debug logging.
        foreground_command_mismatch: bool,
    },
    /// The session is absent, its token no longer matches, or tmux retained
    /// the pane after its agent process exited.
    Dead {
        session_name: String,
        evidence: DeadPaneEvidence,
    },
}

/// The three states a stale-worker pass can establish for a tmux-hosted run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerLiveness {
    AliveAndWorking,
    AliveAndGenuinelyStuck {
        session_name: String,
        window_activity_epoch_secs: i64,
    },
    Dead {
        session_name: String,
        evidence: DeadPaneEvidence,
    },
}

/// Classify hook and terminal evidence without assigning an unsafe meaning to
/// hook silence by itself.
pub fn classify_worker_liveness(
    last_event_at: Option<&str>,
    terminal: &TerminalLiveness,
    now_epoch_secs: i64,
    stale_threshold_secs: i64,
) -> WorkerLiveness {
    let (session_name, window_activity_epoch_secs, agent_is_foreground) = match terminal {
        TerminalLiveness::Alive {
            session_name,
            window_activity_epoch_secs,
            agent_is_foreground,
            ..
        } => (session_name, *window_activity_epoch_secs, *agent_is_foreground),
        TerminalLiveness::Dead { session_name, evidence } => {
            return WorkerLiveness::Dead {
                session_name: session_name.clone(),
                evidence: evidence.clone(),
            };
        }
    };

    let stale_cutoff = iso8601_utc(now_epoch_secs - stale_threshold_secs);
    let hook_is_recent = last_event_at.is_some_and(|event| event >= stale_cutoff.as_str());
    let output_is_recent = window_activity_epoch_secs >= now_epoch_secs - stale_threshold_secs;
    if hook_is_recent || output_is_recent || !agent_is_foreground {
        WorkerLiveness::AliveAndWorking
    } else {
        WorkerLiveness::AliveAndGenuinelyStuck {
            session_name: session_name.to_owned(),
            window_activity_epoch_secs,
        }
    }
}

/// Reads terminal liveness evidence for a run. An unavailable probe is an
/// error, not death: the sweep must never convert an observation failure into
/// a destructive action.
#[async_trait::async_trait]
pub trait WorkerTerminalInspector: Send + Sync {
    async fn inspect(&self, execution_id: &str) -> Result<Option<TerminalLiveness>>;
}

/// Production tmux inspector. A run without tmux identity returns `None` so
/// the legacy cadence fallback remains available while pools migrate.
pub struct TmuxWorkerTerminalInspector {
    work_db: Arc<WorkDb>,
    tmux: Tmux,
}

impl TmuxWorkerTerminalInspector {
    pub fn new(work_db: Arc<WorkDb>, tmux: Tmux) -> Self {
        Self { work_db, tmux }
    }
}

#[async_trait::async_trait]
impl WorkerTerminalInspector for TmuxWorkerTerminalInspector {
    async fn inspect(&self, execution_id: &str) -> Result<Option<TerminalLiveness>> {
        let Some(run) = self.work_db.tmux_run_for_execution(execution_id)? else {
            return Ok(None);
        };

        let session_exists = self
            .tmux
            .list_sessions()
            .await?
            .iter()
            .any(|session| session.name == run.tmux_session_name);
        let observation = crate::tmux_adoption::observe_tmux_identity(
            &self.tmux,
            &run.tmux_session_name,
            &run.tmux_spawn_token,
            session_exists,
        )
        .await;
        match observation.adoption_state {
            boss_protocol::TmuxAdoptionState::NotTmuxHosted => Ok(None),
            boss_protocol::TmuxAdoptionState::ProbeUnavailable => {
                anyhow::bail!("tmux identity probe unavailable for session {}", run.tmux_session_name)
            }
            boss_protocol::TmuxAdoptionState::SessionMissing => Ok(Some(TerminalLiveness::Dead {
                session_name: run.tmux_session_name,
                evidence: DeadPaneEvidence::SessionAbsent,
            })),
            boss_protocol::TmuxAdoptionState::TokenMismatch => Ok(Some(TerminalLiveness::Dead {
                session_name: run.tmux_session_name,
                evidence: DeadPaneEvidence::SpawnTokenMismatch,
            })),
            boss_protocol::TmuxAdoptionState::Adopted if observation.pane_dead == Some(true) => {
                Ok(Some(TerminalLiveness::Dead {
                    session_name: run.tmux_session_name,
                    evidence: DeadPaneEvidence::PaneExited {
                        pane_dead_status: observation.pane_dead_status,
                    },
                }))
            }
            boss_protocol::TmuxAdoptionState::Adopted => {
                let window_activity_epoch_secs = observation
                    .window_activity_epoch_secs
                    .context("tmux window_activity missing on a live pane")?;
                let current_command = observation.current_command.unwrap_or_default();
                let driver_slug = self
                    .work_db
                    .get_execution_driver_slug(execution_id)?
                    .unwrap_or_default();
                let driver_binary = crate::driver::DriverRegistry::default()
                    .require(&driver_slug)
                    .map(|driver| driver.descriptor().binary.to_owned())
                    .unwrap_or_else(|err| {
                        tracing::warn!(
                            execution_id,
                            driver_slug,
                            ?err,
                            "stale-worker sweep: unknown driver while inspecting tmux pane"
                        );
                        String::new()
                    });
                let foreground_command_mismatch = !driver_binary.is_empty() && current_command != driver_binary;
                if foreground_command_mismatch {
                    tracing::warn!(
                        execution_id,
                        driver_slug,
                        expected_binary = driver_binary,
                        observed_command = current_command,
                        "stale-worker sweep: pane foreground command differs from driver binary; \
                         AliveAndGenuinelyStuck is unreachable while this mismatch holds"
                    );
                }
                Ok(Some(TerminalLiveness::Alive {
                    session_name: run.tmux_session_name,
                    window_activity_epoch_secs,
                    agent_is_foreground: !driver_binary.is_empty() && current_command == driver_binary,
                    foreground_command_mismatch,
                }))
            }
        }
    }
}

/// Resolve any open `stale_worker` attention for the work item backing
/// `execution_id`. Used by teardown paths that have an execution id but not
/// its work item id.
pub fn resolve_stale_worker_attention(work_db: &WorkDb, execution_id: &str) {
    let Some(execution) = crate::sweep_loop::lookup_execution_or_warn(
        work_db,
        execution_id,
        "stale-worker sweep: failed to look up execution while resolving stale_worker attention",
    ) else {
        return;
    };
    resolve_stale_worker_attention_for_work_item(work_db, &execution.work_item_id);
}

/// Resolve any open `stale_worker` attention for `work_item_id`.
///
/// The stale-worker sweep already has the execution row, so using this
/// variant avoids an extra execution lookup for every checked slot.
pub fn resolve_stale_worker_attention_for_work_item(work_db: &WorkDb, work_item_id: &str) {
    if let Err(err) = work_db.resolve_external_tracker_attention(work_item_id, STALE_WORKER_ATTENTION_KIND) {
        tracing::warn!(
            work_item_id,
            ?err,
            "stale-worker sweep: failed to resolve stale-worker attention"
        );
    }
}

/// No hook event for this long while a worker is `working` with no tool
/// in flight ⇒ presumed wedged. 30 minutes is deliberately generous: a
/// healthy worker emits a `PreToolUse`/`PostToolUse`/`UserPromptSubmit`
/// hook far more often than this, so the threshold sits well clear of
/// normal think/stream gaps while still bounding the indefinite hang the
/// incident exhibited (~35 min and counting before manual recovery).
pub const DEFAULT_STALE_THRESHOLD_SECS: i64 = 1_800;

/// Grace period after `started_at` (epoch seconds) during which we skip
/// staleness probing, mirroring [`crate::dead_pid_sweep::DEAD_PID_GRACE_SECS`].
/// Guards against reaping a freshly-dispatched run whose pane is still
/// spinning up and has not yet emitted its first hook.
pub const STALE_GRACE_SECS: i64 = 60;

/// Reaps a confirmed-stale worker's OS process tree and tears down its
/// pane/slot — the exact teardown `bossctl agents stop` performs (the
/// `release_worker_pane` path: app pane release → `reap_worker_process_tree`
/// SIGTERM/SIGKILL ladder → pool-slot release → live-state drop).
///
/// The reconcile path *must* go through this before the cube workspace
/// becomes eligible for re-lease. The original sweep released the pool
/// slot without ever killing the `claude` process, so a redispatch's
/// `any_free` lease could land in the still-occupied workspace and two
/// live workers would interleave edits in one working copy. Freeing the
/// slot while the process lives converts a false-positive cancel into a
/// workspace-sharing catastrophe; reaping first closes that gap (same
/// requirement as the `bossctl agents stop` leak in #1006 and the
/// PR-merge retire path in T1561 — this is yet another retire path that
/// skipped teardown).
#[async_trait::async_trait]
pub trait StaleWorkerReaper: Send + Sync {
    /// Kill the worker process tree for `execution_id` and release its
    /// pane/slot. Idempotent: a worker already gone is a no-op.
    async fn reap_worker(&self, execution_id: &str);
}

/// Counts from one pass of the sweep; logged at `info` when a reap
/// occurs.
#[derive(Debug, Default, bon::Builder)]
#[builder(on(String, into))]
pub struct StaleWorkerSweepOutcome {
    pub reaped: usize,
    pub alive_and_working: usize,
    pub genuinely_stuck: usize,
    pub dead: usize,
    pub terminal_probe_failed: usize,
    pub fresh_skipped: usize,
    pub tool_in_flight_skipped: usize,
    pub not_working_skipped: usize,
    pub grace_skipped: usize,
    /// Slots skipped because the live-state `last_event_at` predates the
    /// execution's own `started_at` — a mis-attributed event from a
    /// recycled slot (the false-positive cancel guard, defect 1).
    pub pre_start_event_skipped: usize,
    /// Slots skipped because an operator placed an explicit hold on the
    /// execution via `bossctl agents hold` (see [`crate::hold_registry`]).
    pub held_skipped: usize,
    /// Slots skipped by the cadence fallback because the driver's
    /// [`crate::driver::ProgressFidelity`] tier (`Coarse`/`Minimal`) is
    /// exempt from hook-cadence-based reaping — those drivers do not emit
    /// hooks reliably enough for silence to imply a stall.
    pub fidelity_exempt_skipped: usize,
    /// Live panes whose `#{pane_current_command}` did not equal the driver
    /// binary. Stuck-detection is inert for these slots until the comparison
    /// matches a real foreground command.
    pub foreground_command_mismatch: usize,
    /// Inferred `Dead` (absent session / token mismatch) where
    /// [`crate::durable_liveness`] still reports an alive pid, so the sweep
    /// refused to reap.
    pub dead_uncorroborated: usize,
}

impl crate::sweep_loop::SweepOutcome for StaleWorkerSweepOutcome {
    fn has_activity(&self) -> bool {
        self.reaped > 0
    }

    fn log(&self) {
        tracing::info!(
            reaped = self.reaped,
            alive_and_working = self.alive_and_working,
            genuinely_stuck = self.genuinely_stuck,
            dead = self.dead,
            terminal_probe_failed = self.terminal_probe_failed,
            fresh_skipped = self.fresh_skipped,
            tool_in_flight_skipped = self.tool_in_flight_skipped,
            grace_skipped = self.grace_skipped,
            held_skipped = self.held_skipped,
            fidelity_exempt_skipped = self.fidelity_exempt_skipped,
            foreground_command_mismatch = self.foreground_command_mismatch,
            dead_uncorroborated = self.dead_uncorroborated,
            "stale-worker sweep: pass complete",
        );
    }
}

/// Shared-ownership collaborators [`spawn_loop`] clones into each pass.
/// Bundled into one struct (rather than positional `Arc` parameters)
/// to keep `spawn_loop` under clippy's argument-count lint once the
/// operator-hold registry joined the sweep's dependencies.
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct StaleWorkerSweepDeps {
    pub work_db: Arc<WorkDb>,
    pub live_states: Arc<LiveWorkerStateRegistry>,
    /// Present for tmux-hosted pools. Missing terminal evidence falls back to
    /// the legacy cadence path until that pool migrates.
    pub terminal_inspector: Option<Arc<dyn WorkerTerminalInspector>>,
    pub coordinator: Arc<ExecutionCoordinator>,
    pub dispatch_events: Arc<dyn DispatchEventSink>,
    pub reaper: Arc<dyn StaleWorkerReaper>,
    pub hold_registry: Arc<HoldRegistry>,
    /// Forwarded to [`crate::dead_pid_sweep::reap_reported_pane_death`] so a
    /// tmux-confirmed dead pane can force-release its cube lease.
    pub cube_client: Arc<dyn CubeClient>,
}

/// Borrowed control collaborators for a single pass. Keeping teardown
/// controls together prevents the terminal-aware pass from growing a broad
/// positional argument list.
pub struct StaleWorkerSweepControls<'a> {
    pub reaper: &'a dyn StaleWorkerReaper,
    pub hold_registry: &'a HoldRegistry,
    pub cube_client: &'a dyn CubeClient,
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so a worker that wedged before the engine
/// restarted is recovered at boot without waiting for the first interval.
pub fn spawn_loop(
    deps: StaleWorkerSweepDeps,
    interval: Duration,
    stale_threshold_secs: i64,
) -> tokio::task::JoinHandle<()> {
    let StaleWorkerSweepDeps {
        work_db,
        live_states,
        terminal_inspector,
        coordinator,
        dispatch_events,
        reaper,
        hold_registry,
        cube_client,
    } = deps;
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let live_states = Arc::clone(&live_states);
        let terminal_inspector = terminal_inspector.clone();
        let coordinator = Arc::clone(&coordinator);
        let dispatch_events = Arc::clone(&dispatch_events);
        let reaper = Arc::clone(&reaper);
        let hold_registry = Arc::clone(&hold_registry);
        let cube_client = Arc::clone(&cube_client);
        async move {
            run_one_pass_with_terminal(
                work_db.as_ref(),
                live_states.as_ref(),
                terminal_inspector.as_deref(),
                coordinator.clone(),
                dispatch_events.as_ref(),
                StaleWorkerSweepControls {
                    reaper: reaper.as_ref(),
                    hold_registry: hold_registry.as_ref(),
                    cube_client: cube_client.as_ref(),
                },
                stale_threshold_secs,
            )
            .await
        }
    })
}

/// Test-only convenience wrapper for the cadence fallback.
#[cfg(test)]
pub async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    reaper: &dyn StaleWorkerReaper,
    hold_registry: &HoldRegistry,
    stale_threshold_secs: i64,
) -> StaleWorkerSweepOutcome {
    run_one_pass_with_terminal(
        work_db,
        live_states,
        None,
        coordinator,
        dispatch_events,
        StaleWorkerSweepControls {
            reaper,
            hold_registry,
            // Cadence-fallback tests never take the tmux-dead arm that
            // force-releases a cube lease.
            cube_client: &crate::test_support::NoopCube,
        },
        stale_threshold_secs,
    )
    .await
}

/// Run one stale-worker pass with optional terminal evidence. Returns a
/// summary of what happened; callers may log it.
///
/// Takes `coordinator` as `Arc` because kicking the scheduler requires
/// `Arc<ExecutionCoordinator>` — the kick path spawns a tokio task that
/// holds a reference.
pub async fn run_one_pass_with_terminal(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    terminal_inspector: Option<&dyn WorkerTerminalInspector>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    controls: StaleWorkerSweepControls<'_>,
    stale_threshold_secs: i64,
) -> StaleWorkerSweepOutcome {
    let StaleWorkerSweepControls {
        reaper,
        hold_registry,
        cube_client,
    } = controls;
    let mut outcome = StaleWorkerSweepOutcome::default();
    let snapshot = live_states.snapshot();

    let now_epoch_secs: i64 = boss_engine_utils::epoch_time::now_epoch_secs();
    let grace_cutoff = now_epoch_secs - STALE_GRACE_SECS;

    for state in snapshot {
        // Operator hold (`bossctl agents hold`): checked before every
        // other guard so a held run is never reaped regardless of how
        // stale-looking it is — see `crate::hold_registry`'s "sweeps must
        // respect it" contract. Manual `bossctl agents stop`/`reap` still
        // work on a held run; only this automated sweep is exempted.
        if hold_registry.is_held(&state.run_id) {
            outcome.held_skipped += 1;
            continue;
        }

        // Only `working` slots are candidates. `Spawning` is still
        // coming up (no event history expected); `Idle` and
        // `WaitingForInput` are handled by the completion and
        // transient-recovery paths; terminal states are done.
        if state.activity != WorkerActivity::Working {
            outcome.not_working_skipped += 1;
            continue;
        }

        // Look the execution up before the inspector so a terminal row
        // resolves its stale_worker attention even when the production
        // inspector returns `None` (`tmux_run_for_execution` excludes
        // terminal executions). Manual stop / finalize also resolve via
        // [`resolve_stale_worker_attention`] in `release_worker_pane`.
        let Some(execution) = crate::sweep_loop::lookup_execution_or_warn(
            work_db,
            &state.run_id,
            "stale-worker sweep: failed to look up execution; skipping slot",
        ) else {
            continue;
        };
        if execution.status.is_terminal() {
            resolve_stale_worker_attention_for_work_item(work_db, &execution.work_item_id);
            continue;
        }

        // Tmux is the authoritative liveness source for migrated pools. Its
        // terminal-level signals apply equally to Rich, Coarse, and Minimal
        // drivers, so none of those fidelity tiers is exempt here.
        let terminal = match terminal_inspector {
            Some(inspector) => match inspector.inspect(&state.run_id).await {
                Ok(Some(terminal)) => Some(terminal),
                Ok(None) => None,
                Err(err) => {
                    tracing::warn!(
                        execution_id = %state.run_id,
                        ?err,
                        "stale-worker sweep: tmux liveness probe failed; skipping slot",
                    );
                    outcome.terminal_probe_failed += 1;
                    continue;
                }
            },
            None => None,
        };
        if let Some(terminal) = terminal {
            if let TerminalLiveness::Alive {
                foreground_command_mismatch: true,
                ..
            } = &terminal
            {
                outcome.foreground_command_mismatch += 1;
            }
            let Some(started_epoch) = execution.started_epoch() else {
                outcome.grace_skipped += 1;
                continue;
            };
            if started_epoch >= grace_cutoff {
                outcome.grace_skipped += 1;
                continue;
            }

            match classify_worker_liveness(
                state.last_event_at.as_deref(),
                &terminal,
                now_epoch_secs,
                stale_threshold_secs,
            ) {
                WorkerLiveness::AliveAndWorking => {
                    outcome.alive_and_working += 1;
                    resolve_stale_worker_attention_for_work_item(work_db, &execution.work_item_id);
                }
                WorkerLiveness::AliveAndGenuinelyStuck {
                    session_name,
                    window_activity_epoch_secs,
                } => {
                    outcome.genuinely_stuck += 1;
                    let body = format!(
                        "Worker execution `{}` has had no hook or pane output for more than {} seconds while its agent remains the foreground command. The session is still live, so Boss did not reap it.\n\nSession: `{session_name}`\n\nLast pane output: {}\n\nInspect it with:\n\n```sh\ntmux -L boss attach -t {session_name}\n```",
                        state.run_id,
                        stale_threshold_secs,
                        iso8601_utc(window_activity_epoch_secs),
                    );
                    if let Err(err) = work_db.upsert_external_tracker_attention(
                        &execution.work_item_id,
                        STALE_WORKER_ATTENTION_KIND,
                        "Worker appears stuck; inspection required",
                        &body,
                    ) {
                        tracing::warn!(
                            execution_id = %state.run_id,
                            ?err,
                            "stale-worker sweep: failed to raise stuck-worker attention",
                        );
                    }
                }
                WorkerLiveness::Dead { session_name, evidence } => {
                    if evidence.requires_pid_corroboration() {
                        let process = crate::durable_liveness::probe_execution_worker_within(
                            work_db,
                            &state.run_id,
                            crate::durable_liveness::REDISPATCH_PID_TRUST_SECS,
                            now_epoch_secs,
                        );
                        if process.is_alive() {
                            tracing::warn!(
                                execution_id = %state.run_id,
                                session_name,
                                ?evidence,
                                "stale-worker sweep: tmux inferred death without an observed exit, \
                                 but durable_liveness still reports the worker pid alive; not reaping"
                            );
                            outcome.dead_uncorroborated += 1;
                            continue;
                        }
                    }
                    outcome.dead += 1;
                    let reason = evidence.reap_reason(&session_name);
                    tracing::info!(
                        execution_id = %state.run_id,
                        session_name,
                        ?evidence,
                        reason,
                        "stale-worker sweep: tmux reports a dead worker; reconciling it",
                    );
                    if crate::dead_pid_sweep::reap_observed_worker_death(
                        work_db,
                        live_states,
                        coordinator.clone(),
                        dispatch_events,
                        cube_client,
                        &state.run_id,
                        &reason,
                    )
                    .await
                    {
                        outcome.reaped += 1;
                    }
                }
            }
            continue;
        }

        let Some(effective_threshold_secs) = live_states
            .progress_fidelity_for_slot(state.slot_id)
            .stale_threshold_secs(stale_threshold_secs)
        else {
            outcome.fidelity_exempt_skipped += 1;
            continue;
        };

        // A tool in flight means the worker is legitimately busy — most
        // importantly a long foreground `bazel build`/`bazel test`,
        // which can run for many minutes with no intervening hook.
        // Reaping that would break real work; skip it and let the
        // pre-push gate's `timeout` guidance bound the wedged-tool case.
        if state.current_tool.is_some() {
            outcome.tool_in_flight_skipped += 1;
            continue;
        }

        // Build the staleness cutoff as a fixed-width ISO-8601 string so we
        // can compare `last_event_at < stale_cutoff` lexicographically —
        // the format is the same one the registry stamps, so byte order
        // matches chronological order and no date parsing is needed.
        let stale_cutoff = iso8601_utc(now_epoch_secs - effective_threshold_secs);

        // No hook yet at all ⇒ nothing to judge staleness against; the
        // dead-PID / grace paths cover a truly stuck spawn.
        let Some(last_event_at) = state.last_event_at.as_deref() else {
            outcome.fresh_skipped += 1;
            continue;
        };

        // Newer than the threshold ⇒ healthy.
        if last_event_at >= stale_cutoff.as_str() {
            outcome.fresh_skipped += 1;
            continue;
        }

        let execution_id = &state.run_id;

        // Grace-period guard: skip executions whose `started_at` is
        // within STALE_GRACE_SECS or not yet recorded.
        let Some(started_epoch) = execution.started_epoch() else {
            outcome.grace_skipped += 1;
            continue;
        };
        if started_epoch >= grace_cutoff {
            outcome.grace_skipped += 1;
            continue;
        }

        // Event-attribution guard (defect 1 — the false-positive cancel).
        //
        // The `last_event_at` we are about to judge MUST belong to THIS
        // execution. The live-state registry is keyed by *slot*, and a
        // slot is reused across consecutive runs; on a slot recycle the
        // events-socket / live-state association can leave a *prior run's*
        // last-event timestamp attached to the slot (the slot/exec/pane
        // identity class investigated in PR #1213). A hook timestamp that
        // predates the execution's own `started_at` cannot possibly be one
        // of its events — it is that recycled-slot artifact. Reaping on it
        // false-cancels a healthy, actively-working worker, releases its
        // lease, and lets a redispatch's `any_free` lease collide in the
        // same workspace (the incident this fix exists for). Key the
        // staleness decision to the current execution's own timeline:
        // never treat a pre-start timestamp as in-execution activity. A
        // worker whose events are genuinely flowing always stamps
        // `last_event_at` at or after `started_at`, so this can only skip
        // the mis-attributed case — a worker with flowing events is
        // un-cancellable by this path. Log loudly so the misattribution is
        // visible without ever cancelling a live worker.
        let started_iso = iso8601_utc(started_epoch);
        if last_event_at < started_iso.as_str() {
            tracing::warn!(
                execution_id,
                slot_id = state.slot_id,
                last_event_at,
                started_at = %started_iso,
                stale_threshold_secs,
                "stale-worker sweep: last_event_at predates this execution's started_at — \
                 mis-attributed event from a recycled slot (cf. PR #1213); NOT reaping \
                 (worker presumed healthy, staleness un-evaluable for this run)",
            );
            outcome.pre_start_event_skipped += 1;
            continue;
        }

        tracing::info!(
            execution_id,
            work_item_id = %execution.work_item_id,
            slot_id = state.slot_id,
            last_event_at,
            stale_threshold_secs,
            "stale-worker sweep: worker alive but no progress past threshold; reaping execution and releasing slot",
        );

        // Mark the execution orphaned so the DB reflects the wedge and
        // `bossctl agents transcript <exec-id>` still works.
        let reason = format!(
            "stale-worker-reconcile: no hook event since {last_event_at} (> {stale_threshold_secs}s) while working with no tool in flight; worker presumed wedged on a backgrounded/idle wait"
        );
        if let Err(err) = work_db.mark_execution_orphaned(execution_id, &reason) {
            tracing::warn!(
                execution_id,
                ?err,
                "stale-worker sweep: failed to mark execution orphaned; skipping reap",
            );
            continue;
        }

        // This reap releases the pool slot and drops this slot's live-state
        // entry, so no later pass can reconcile a still-open stale_worker
        // attention for this work item — resolve it now while we still hold
        // `execution.work_item_id`.
        resolve_stale_worker_attention_for_work_item(work_db, &execution.work_item_id);

        // Reap termination path (stale-worker sweep): tear down any
        // driver-owned state outside the workspace. `mark_execution_orphaned`
        // preserves `workspace_path`, so the pre-call `execution` snapshot
        // is still current.
        crate::driver_teardown::teardown_driver_workspace(
            work_db,
            execution_id,
            execution.workspace_path.as_deref().map(std::path::Path::new),
            crate::driver_teardown::TeardownReason::StaleWorkerReconcile,
        )
        .await;

        // Snapshot the wedged worker's uncommitted workspace work to a
        // durable patch before the slot is released and the workspace
        // becomes eligible for re-lease/reset. Best-effort: a failed or
        // empty capture returns None and never blocks the reap.
        let recovery_patch = boss_engine_recovery::recovery_backup::backup_dead_execution(&execution);

        // Append [engine-reconcile] audit line to the task description so
        // a human inspecting the chore can see why it was reset (and
        // where to find the recovery patch, if one was captured).
        if let Some(work_item_id) = &state.work_item_id
            && let Err(err) = crate::reconcile_audit::append_reconcile_audit(
                work_db,
                work_item_id,
                now_epoch_secs,
                &format!(
                    "stale worker (exec {execution_id}) detected — no transcript progress for > {stale_threshold_secs}s while working; chore reset to todo for redispatch"
                ),
                recovery_patch.as_deref(),
            )
        {
            tracing::warn!(
                work_item_id,
                ?err,
                "stale-worker sweep: failed to append audit line to description (non-fatal)",
            );
        }

        // Reap the worker's OS process tree BEFORE the slot/lease is
        // freed (defect 2). The original sweep released the pool slot
        // without ever killing the `claude` process — so a redispatch's
        // `any_free` lease could land in the still-occupied workspace and
        // two live workers would interleave edits in one working copy.
        // Route through the same teardown `bossctl agents stop` uses
        // (`release_worker_pane`: app pane release → process-tree
        // SIGTERM/SIGKILL → pool-slot release → live-state drop), so the
        // process is dead (at minimum SIGTERM-signalled) before the kick
        // below can trigger a redispatch that re-leases the workspace.
        // This must precede any lease release; a lease freed while the
        // process lives is what turned the false cancel into a
        // workspace-sharing catastrophe.
        reaper.reap_worker(execution_id).await;

        // Release the worker pool slot so the orphan sweep detects the
        // chore and creates a fresh ready execution for redispatch.
        // Use worker_id_for_slot (not WorkerPool::worker_id_for_slot) so
        // automation-pool slots (> MAX_WORKER_POOL_SIZE) produce the
        // "auto-worker-N" prefix and release_worker_and_kick routes to the
        // correct pool via pool_for_worker_id. Idempotent with the
        // pool-slot release the reaper's `release_worker_pane` already
        // performed in production (find-or-skip no-op); in tests where the
        // reaper is a recording stub, this is what frees the slot.
        let worker_id = worker_id_for_slot(state.slot_id);
        coordinator.release_worker_and_kick(&worker_id, None).await;

        // Structured event for bossctl dispatch tail.
        dispatch_events
            .emit(
                DispatchEvent::new(Stage::StaleWorkerReconcile, Outcome::Ok, execution_id)
                    .with_work_item(&execution.work_item_id)
                    .with_details(serde_json::json!({
                        "slot_id": state.slot_id,
                        "last_event_at": last_event_at,
                        "stale_threshold_secs": stale_threshold_secs,
                        "recovery_patch": recovery_patch
                            .as_deref()
                            .map(|p| p.display().to_string()),
                    })),
            )
            .await;

        outcome.reaped += 1;
    }

    outcome
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use boss_protocol::{WorkItemBinding, WorkerEvent};

    use super::*;
    use crate::coordinator::ExecutionCoordinator;
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::driver::ProgressFidelity;
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::test_support::*;
    use crate::work::ExecutionStatus;

    // A staleness threshold whose cutoff lands in the *future*, so any
    // `last_event_at` stamped "now" by `apply_event` compares as stale.
    // This lets us exercise the staleness branch deterministically
    // without a way to backdate the in-memory `last_event_at`.
    const ALWAYS_STALE: i64 = -120;
    // A threshold whose cutoff is an hour in the past, so a just-stamped
    // event is comfortably fresh.
    const NEVER_STALE: i64 = 3_600;

    fn live_terminal(activity_at: i64, agent_is_foreground: bool) -> TerminalLiveness {
        TerminalLiveness::Alive {
            session_name: "boss-worker-test".to_owned(),
            window_activity_epoch_secs: activity_at,
            agent_is_foreground,
            foreground_command_mismatch: false,
        }
    }

    #[test]
    fn recent_hook_or_terminal_activity_means_alive_and_working() {
        let now = 10_000;
        assert_eq!(
            classify_worker_liveness(Some(&iso8601_utc(9_500)), &live_terminal(8_000, true), now, 1_000,),
            WorkerLiveness::AliveAndWorking,
        );
        assert_eq!(
            classify_worker_liveness(None, &live_terminal(9_500, true), now, 1_000),
            WorkerLiveness::AliveAndWorking,
        );
        assert_eq!(
            classify_worker_liveness(None, &live_terminal(8_000, false), now, 1_000),
            WorkerLiveness::AliveAndWorking,
        );
    }

    #[test]
    fn sustained_quiet_agent_pane_is_genuinely_stuck() {
        assert_eq!(
            classify_worker_liveness(None, &live_terminal(8_000, true), 10_000, 1_000),
            WorkerLiveness::AliveAndGenuinelyStuck {
                session_name: "boss-worker-test".to_owned(),
                window_activity_epoch_secs: 8_000,
            },
        );
    }

    #[test]
    fn dead_pane_is_dead_regardless_of_recent_hooks() {
        assert_eq!(
            classify_worker_liveness(
                Some(&iso8601_utc(9_999)),
                &TerminalLiveness::Dead {
                    session_name: "boss-worker-test".to_owned(),
                    evidence: DeadPaneEvidence::PaneExited {
                        pane_dead_status: Some("1".to_owned()),
                    },
                },
                10_000,
                1_000,
            ),
            WorkerLiveness::Dead {
                session_name: "boss-worker-test".to_owned(),
                evidence: DeadPaneEvidence::PaneExited {
                    pane_dead_status: Some("1".to_owned()),
                },
            },
        );
    }

    /// Records every `reap_worker` call and, at reap time, snapshots
    /// whether the execution's pool slot is still claimed. The production
    /// reaper (`ServerState::release_worker_pane`) kills the OS process
    /// tree and frees the slot; this stub only records, leaving the
    /// sweep's own `release_worker_and_kick` to free the slot — so the
    /// "still claimed at reap time" snapshot proves the reap ran BEFORE
    /// the slot/lease was released (defect 2's ordering requirement).
    struct RecordingReaper {
        coordinator: Arc<ExecutionCoordinator>,
        reaped: StdMutex<Vec<(String, bool)>>,
    }

    impl RecordingReaper {
        fn new(coordinator: Arc<ExecutionCoordinator>) -> Self {
            Self {
                coordinator,
                reaped: StdMutex::new(Vec::new()),
            }
        }

        /// `(execution_id, slot_still_claimed_at_reap)` for each reap.
        fn reaped(&self) -> Vec<(String, bool)> {
            self.reaped.lock().unwrap().clone()
        }
    }

    struct StaticTerminalInspector(TerminalLiveness);

    #[async_trait]
    impl WorkerTerminalInspector for StaticTerminalInspector {
        async fn inspect(&self, _execution_id: &str) -> Result<Option<TerminalLiveness>> {
            Ok(Some(self.0.clone()))
        }
    }

    /// A terminal inspector that always fails the probe — models a tmux
    /// command erroring (e.g. the server is unreachable). The sweep must
    /// treat this as "unknown", never as "dead".
    struct FailingTerminalInspector;

    #[async_trait]
    impl WorkerTerminalInspector for FailingTerminalInspector {
        async fn inspect(&self, _execution_id: &str) -> Result<Option<TerminalLiveness>> {
            Err(anyhow::anyhow!("tmux probe failed"))
        }
    }

    #[async_trait]
    impl StaleWorkerReaper for RecordingReaper {
        async fn reap_worker(&self, execution_id: &str) {
            let still_claimed = self
                .coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(execution_id);
            self.reaped
                .lock()
                .unwrap()
                .push((execution_id.to_owned(), still_claimed));
        }
    }

    // ─── helpers ─────────────────────────────────────────────────────────────

    fn register_slot(live_states: &LiveWorkerStateRegistry, slot_id: u8, execution_id: &str, work_item_id: &str) {
        live_states.register_spawn(
            slot_id,
            execution_id,
            "claude-opus-4-7",
            std::process::id() as i32,
            Some(WorkItemBinding {
                work_item_id: work_item_id.to_owned(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
    }

    /// Drive a slot to `Working` with NO tool in flight (a balanced
    /// PreToolUse/PostToolUse pair). `last_event_at` is stamped "now".
    fn drive_to_working_idle(live_states: &LiveWorkerStateRegistry, slot_id: u8) {
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

    /// Drive a slot to `Working` WITH a tool in flight (PreToolUse only,
    /// no balancing PostToolUse) — models a long foreground bazel build.
    fn drive_to_working_tool_in_flight(live_states: &LiveWorkerStateRegistry, slot_id: u8) {
        live_states.apply_event(
            slot_id,
            &WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );
    }

    // ─── tests ───────────────────────────────────────────────────────────────

    /// The core invariant: a `working`, tool-idle slot whose last hook is
    /// older than the threshold has its execution orphaned, its pool slot
    /// released, and a `stale_worker_reconcile` event emitted.
    #[tokio::test]
    async fn stale_idle_worker_is_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;
        let claimed_before = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(claimed_before.contains(&execution_id));

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &HoldRegistry::new(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 1, "stale idle worker must be reaped");

        let exec = db.get_execution(&execution_id).unwrap();
        assert_eq!(exec.status, ExecutionStatus::Orphaned);

        let claimed_after = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(
            !claimed_after.contains(&execution_id),
            "pool slot must be released after reap",
        );

        // Defect 2: the worker's process tree must be reaped, and the reap
        // must run BEFORE the pool slot / cube lease is released. The
        // recording reaper snapshots the slot as still-claimed at reap
        // time, which pins the reap-before-release ordering.
        assert_eq!(
            reaper.reaped(),
            vec![(execution_id.clone(), true)],
            "reconcile must reap the process tree before releasing the slot/lease",
        );

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "stale_worker_reconcile");
        assert_eq!(events[0].outcome, "ok");
        assert_eq!(events[0].work_item_id.as_deref(), Some(work_item_id.as_str()));

        let item = db.get_work_item(&work_item_id).unwrap();
        let desc = match &item {
            boss_protocol::WorkItem::Chore(t) | boss_protocol::WorkItem::Task(t) => t.description.clone(),
            _ => panic!("expected chore"),
        };
        assert!(desc.contains("[engine-reconcile]"), "got: {desc:?}");
    }

    #[tokio::test]
    async fn stuck_tmux_worker_raises_attention_without_reaping() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);
        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let inspector = StaticTerminalInspector(live_terminal(0, true));
        let hold_registry = HoldRegistry::new();
        let outcome = run_one_pass_with_terminal(
            db.as_ref(),
            &live_states,
            Some(&inspector),
            coordinator.clone(),
            sink.as_ref(),
            StaleWorkerSweepControls {
                reaper: reaper.as_ref(),
                hold_registry: &hold_registry,
                cube_client: &NoopCube,
            },
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.genuinely_stuck, 1);
        assert_eq!(outcome.reaped, 0);
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
        assert!(reaper.reaped().is_empty());
        let attention = db
            .list_attention_items_for_work_item(&work_item_id)
            .unwrap()
            .into_iter()
            .find(|item| item.kind == STALE_WORKER_ATTENTION_KIND)
            .expect("stuck worker must raise an attention item");
        assert!(
            attention
                .body_markdown
                .contains("tmux -L boss attach -t boss-worker-test")
        );
    }

    /// The `WorkerLiveness::Dead` arm reaps through
    /// `reap_observed_worker_death` with the `pane_dead_status` folded into
    /// the reason string, and resolves any previously-open `stale_worker`
    /// attention for the work item — the ProducerReconciles contract
    /// `attention_lifecycle.rs` declares for this kind.
    #[tokio::test]
    async fn dead_tmux_pane_is_reaped_and_resolves_open_attention() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);
        let execution_id = create_old_execution(&db, &work_item_id);
        db.start_execution_run(&execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
            .unwrap();
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        // A prior pass raised a stale_worker attention (e.g. the worker was
        // genuinely stuck before its pane died). It must be resolved once
        // the pane is confirmed dead, or it is stranded open forever.
        db.upsert_external_tracker_attention(
            &work_item_id,
            STALE_WORKER_ATTENTION_KIND,
            "Worker appears stuck; inspection required",
            "prior body",
        )
        .unwrap();

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let inspector = StaticTerminalInspector(TerminalLiveness::Dead {
            session_name: "boss-worker-test".to_owned(),
            evidence: DeadPaneEvidence::PaneExited {
                pane_dead_status: Some("143".to_owned()),
            },
        });
        let hold_registry = HoldRegistry::new();
        let outcome = run_one_pass_with_terminal(
            db.as_ref(),
            &live_states,
            Some(&inspector),
            coordinator.clone(),
            sink.as_ref(),
            StaleWorkerSweepControls {
                reaper: reaper.as_ref(),
                hold_registry: &hold_registry,
                cube_client: &AlwaysSucceedsCube,
            },
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.dead, 1);
        assert_eq!(outcome.reaped, 1);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Orphaned
        );
        let run = db.list_runs(&execution_id).unwrap().pop().expect("orphaned run");
        let run_reason = run.result_summary.or(run.error_text).unwrap_or_default();
        assert!(
            run_reason.contains("pane_dead_status=143"),
            "reap reason must record tmux pane_dead_status, got {run_reason:?}"
        );
        // `reap_observed_worker_death` reconciles through the dead-pid-sweep
        // teardown path, not the injected `StaleWorkerReaper` — that
        // reaper backs only the cadence-fallback OS-process kill.
        assert!(reaper.reaped().is_empty());

        let open_items: Vec<_> = db
            .list_attention_items_for_work_item(&work_item_id)
            .unwrap()
            .into_iter()
            .filter(|item| item.kind == STALE_WORKER_ATTENTION_KIND && item.status == "open")
            .collect();
        assert!(
            open_items.is_empty(),
            "a dead pane must resolve any open stale_worker attention for the work item: {open_items:?}",
        );
    }

    /// A tmux probe failure must never be treated as death: the slot is
    /// skipped entirely, `terminal_probe_failed` is incremented, and nothing
    /// is reaped.
    #[tokio::test]
    async fn terminal_probe_failure_skips_the_slot_without_reaping() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);
        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let inspector = FailingTerminalInspector;
        let hold_registry = HoldRegistry::new();
        let outcome = run_one_pass_with_terminal(
            db.as_ref(),
            &live_states,
            Some(&inspector),
            coordinator.clone(),
            sink.as_ref(),
            StaleWorkerSweepControls {
                reaper: reaper.as_ref(),
                hold_registry: &hold_registry,
                cube_client: &NoopCube,
            },
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.terminal_probe_failed, 1);
        assert_eq!(outcome.reaped, 0);
        assert_eq!(outcome.dead, 0);
        assert_eq!(outcome.genuinely_stuck, 0);
        assert_eq!(outcome.alive_and_working, 0);
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
        assert!(reaper.reaped().is_empty());
        assert!(sink.events().await.is_empty());
    }

    /// A slot found `AliveAndWorking` after terminal evidence resolves any
    /// previously-open `stale_worker` attention — the recovery half of the
    /// ProducerReconciles contract.
    #[tokio::test]
    async fn alive_and_working_resolves_previously_open_attention() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);
        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        db.upsert_external_tracker_attention(
            &work_item_id,
            STALE_WORKER_ATTENTION_KIND,
            "Worker appears stuck; inspection required",
            "prior body",
        )
        .unwrap();

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        // Recent window activity and an in-flight hook both read as
        // AliveAndWorking regardless of foreground state.
        let inspector = StaticTerminalInspector(live_terminal(i64::MAX / 2, true));
        let hold_registry = HoldRegistry::new();
        let outcome = run_one_pass_with_terminal(
            db.as_ref(),
            &live_states,
            Some(&inspector),
            coordinator.clone(),
            sink.as_ref(),
            StaleWorkerSweepControls {
                reaper: reaper.as_ref(),
                hold_registry: &hold_registry,
                cube_client: &NoopCube,
            },
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.alive_and_working, 1);
        assert_eq!(outcome.reaped, 0);
        assert!(reaper.reaped().is_empty());

        let open_items: Vec<_> = db
            .list_attention_items_for_work_item(&work_item_id)
            .unwrap()
            .into_iter()
            .filter(|item| item.kind == STALE_WORKER_ATTENTION_KIND && item.status == "open")
            .collect();
        assert!(
            open_items.is_empty(),
            "a recovered worker must resolve its open stale_worker attention: {open_items:?}",
        );
    }

    /// A slot whose execution has already reached a terminal DB status is
    /// skipped before the inspector (the production inspector cannot return
    /// `Some` for a terminal execution — `tmux_run_for_execution` excludes
    /// them) and any open `stale_worker` attention is resolved.
    #[tokio::test]
    async fn terminal_execution_is_skipped_and_resolves_open_attention() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);
        let execution_id = create_old_execution(&db, &work_item_id);
        db.mark_execution_orphaned(&execution_id, "test: already settled")
            .unwrap();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        db.upsert_external_tracker_attention(
            &work_item_id,
            STALE_WORKER_ATTENTION_KIND,
            "Worker appears stuck; inspection required",
            "prior body",
        )
        .unwrap();

        let coordinator = make_coordinator(db.clone(), 1);
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let hold_registry = HoldRegistry::new();
        // No inspector: the production `tmux_run_for_execution` predicate
        // returns `None` for a terminal execution, so the sweep used to fall
        // through to cadence and `continue` without resolving attention.
        let outcome = run_one_pass_with_terminal(
            db.as_ref(),
            &live_states,
            None,
            coordinator.clone(),
            sink.as_ref(),
            StaleWorkerSweepControls {
                reaper: reaper.as_ref(),
                hold_registry: &hold_registry,
                cube_client: &NoopCube,
            },
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 0);
        assert_eq!(outcome.dead, 0);
        assert_eq!(outcome.genuinely_stuck, 0);
        assert_eq!(outcome.alive_and_working, 0);
        assert!(reaper.reaped().is_empty());
        assert!(sink.events().await.is_empty());

        let open_items: Vec<_> = db
            .list_attention_items_for_work_item(&work_item_id)
            .unwrap()
            .into_iter()
            .filter(|item| item.kind == STALE_WORKER_ATTENTION_KIND && item.status == "open")
            .collect();
        assert!(
            open_items.is_empty(),
            "a terminal execution must resolve its open stale_worker attention: {open_items:?}",
        );
    }

    /// A `working` slot whose last hook is *recent* (within the
    /// threshold) is left alone — the common healthy case.
    #[tokio::test]
    async fn fresh_worker_is_not_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &HoldRegistry::new(),
            NEVER_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "fresh worker must not be reaped");
        assert_eq!(outcome.fresh_skipped, 1);
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
    }

    /// A `working` slot WITH a tool in flight (e.g. a long foreground
    /// bazel build) is never reaped even past the threshold — this is the
    /// critical false-positive guard.
    #[tokio::test]
    async fn worker_with_tool_in_flight_is_not_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_tool_in_flight(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &HoldRegistry::new(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(
            outcome.reaped, 0,
            "a tool in flight (long foreground build) must never be reaped",
        );
        assert_eq!(outcome.tool_in_flight_skipped, 1);
        assert!(sink.events().await.is_empty());
    }

    /// A slot that is still `Spawning` (no working transition yet) is not
    /// a candidate.
    #[tokio::test]
    async fn non_working_activity_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        // Left at Spawning — no events applied.

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &HoldRegistry::new(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 0);
        assert_eq!(outcome.not_working_skipped, 1);
    }

    /// A stale-looking `working` slot whose execution started within the
    /// grace window is skipped, guarding against racing a fresh dispatch.
    #[tokio::test]
    async fn recent_started_at_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_execution_started_now(&db, &work_item_id);

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &HoldRegistry::new(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "grace period must prevent reaping fresh dispatches");
        assert_eq!(outcome.grace_skipped, 1);
    }

    /// Regression test (a) for the false-positive cancel: slot reuse.
    ///
    /// Slot 1 was recycled — its live-state `run_id` now points at the
    /// CURRENT execution, but its `last_event_at` still carries a PRIOR
    /// run's (much older) timestamp, the recycled-slot attribution
    /// artifact from the incident. The current execution started AFTER
    /// that stale timestamp. Even at an always-stale threshold, the
    /// reconciler must NOT reap: a hook timestamp predating the
    /// execution's own `started_at` cannot be one of its events, so
    /// staleness is un-evaluable and the (healthy) worker is left alone.
    /// Without the event-attribution guard this slot would be reaped,
    /// false-cancelling a live worker.
    #[tokio::test]
    async fn slot_reuse_stale_prior_event_is_not_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        // The CURRENT execution started 5 minutes ago (clears the grace
        // window) — `create_old_execution` stamps started_at to now-300.
        let execution_id = create_old_execution(&db, &work_item_id);
        let started_epoch = db
            .get_execution(&execution_id)
            .unwrap()
            .started_at
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);
        // The recycled slot carries a PRIOR run's last-event timestamp,
        // an hour before THIS execution even started — the exact
        // mis-attribution the incident hit (last_event_at "03:43:55Z"
        // predating the 06:24Z dispatch).
        live_states.set_last_event_at_for_test(1, iso8601_utc(started_epoch - 3_600));

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &HoldRegistry::new(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(
            outcome.reaped, 0,
            "a worker whose only 'staleness' is a recycled-slot prior-run timestamp must not be reaped",
        );
        assert_eq!(outcome.pre_start_event_skipped, 1);
        // Execution untouched, slot still claimed, no reap, no event.
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
        assert!(
            coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id),
            "the healthy worker's slot must remain claimed",
        );
        assert!(
            reaper.reaped().is_empty(),
            "no process reap may fire for a healthy worker"
        );
        assert!(sink.events().await.is_empty());
    }

    /// Regression test (b): a legitimate reconcile-cancel must reap the
    /// worker's process tree BEFORE the slot/lease is freed. The
    /// recording reaper captures the pool slot as still-claimed at reap
    /// time; combined with the slot being released by the end of the
    /// pass, that pins the reap-before-release ordering the incident
    /// required (lease freed while the process lived is what produced the
    /// shared-workspace catastrophe).
    #[tokio::test]
    async fn reconcile_reaps_process_before_releasing_slot() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &HoldRegistry::new(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 1);
        // Exactly one reap, for this execution, observed while the slot
        // was STILL claimed → reap ran before the slot/lease release.
        assert_eq!(reaper.reaped(), vec![(execution_id.clone(), true)]);
        // …and by the end of the pass the slot is released.
        assert!(
            !coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id),
            "slot must be released after the reap",
        );
    }

    /// Coarse-fidelity slots are exempt from the cadence fallback only.
    /// Terminal evidence still classifies them; this test has no inspector.
    #[tokio::test]
    async fn coarse_fidelity_slot_is_exempt_from_cadence_staleness_without_terminal_evidence() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        live_states.set_progress_fidelity(1, ProgressFidelity::Coarse);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &HoldRegistry::new(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "a Coarse-fidelity slot must never be cadence-reaped");
        assert_eq!(outcome.fidelity_exempt_skipped, 1);
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
        assert!(reaper.reaped().is_empty());
        assert!(sink.events().await.is_empty());
    }

    /// Minimal fidelity receives the same cadence-only exemption.
    #[tokio::test]
    async fn minimal_fidelity_slot_is_exempt_from_cadence_staleness_without_terminal_evidence() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        live_states.set_progress_fidelity(1, ProgressFidelity::Minimal);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &HoldRegistry::new(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(
            outcome.reaped, 0,
            "a Minimal-fidelity slot must never be cadence-reaped"
        );
        assert_eq!(outcome.fidelity_exempt_skipped, 1);
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
        assert!(reaper.reaped().is_empty());
        assert!(sink.events().await.is_empty());
    }

    /// A slot with no declared fidelity (the default, and every existing
    /// call site's behaviour) is treated as `Rich` and reaped exactly like
    /// today — this is the "Claude's sweep behaviour must be unchanged"
    /// requirement, pinned as a test.
    #[tokio::test]
    async fn unset_fidelity_defaults_to_rich_and_reaps_like_today() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        // No set_progress_fidelity call — this is the untouched default.
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &HoldRegistry::new(),
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(
            outcome.reaped, 1,
            "default (Rich) fidelity must reap exactly like before this change"
        );
        assert_eq!(
            outcome.fidelity_exempt_skipped, 0,
            "a Rich/default slot is judged, not exempted"
        );
    }

    /// A stale-looking `working` slot whose execution an operator has
    /// explicitly held (`bossctl agents hold`) is never reaped, even past
    /// the threshold — the auto-reap sweep must respect a hold.
    #[tokio::test]
    async fn held_worker_is_not_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let hold_registry = HoldRegistry::new();
        hold_registry.hold(&execution_id, Some("debugging by hand".to_owned()), 0);

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &hold_registry,
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "a held worker must never be reaped");
        assert_eq!(outcome.held_skipped, 1);
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
        assert!(
            coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id),
            "a held worker's slot must remain claimed",
        );
        assert!(reaper.reaped().is_empty(), "no process reap may fire for a held worker");
    }

    /// An inferred Dead verdict (session absent / token mismatch) must not
    /// reap when durable_liveness still sees the recorded worker pid alive.
    #[tokio::test]
    async fn inferred_dead_skips_reap_when_durable_pid_is_alive() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);
        let execution_id = create_old_execution(&db, &work_item_id);
        db.start_execution_run(&execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
            .unwrap();
        assert!(
            db.set_run_shell_pid_for_execution(&execution_id, i64::from(std::process::id()))
                .unwrap()
        );

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot(&live_states, 1, &execution_id, &work_item_id);
        drive_to_working_idle(&live_states, 1);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let inspector = StaticTerminalInspector(TerminalLiveness::Dead {
            session_name: "boss-worker-test".to_owned(),
            evidence: DeadPaneEvidence::SessionAbsent,
        });
        let hold_registry = HoldRegistry::new();
        let outcome = run_one_pass_with_terminal(
            db.as_ref(),
            &live_states,
            Some(&inspector),
            coordinator.clone(),
            sink.as_ref(),
            StaleWorkerSweepControls {
                reaper: reaper.as_ref(),
                hold_registry: &hold_registry,
                cube_client: &NoopCube,
            },
            ALWAYS_STALE,
        )
        .await;

        assert_eq!(outcome.dead_uncorroborated, 1);
        assert_eq!(outcome.dead, 0);
        assert_eq!(outcome.reaped, 0);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Running
        );
        assert!(reaper.reaped().is_empty());
    }

    /// `#{pane_current_command}` the stuck-classifier compares to the driver
    /// binary. A live Boss coordinator pane reported `#{pane_current_command}`
    /// as `2.1.235` (observed 2026-08-19) — a version string, not a binary
    /// name. Worker panes are
    /// spawned as `<login-shell> -l -i -c '. .boss/<script>'`, so this field
    /// is the sourced script's current child (`claude` when the descriptor
    /// is on PATH as itself; otherwise `node`, the shell, or a version
    /// string like the coordinator observation).
    const CLASSIFIER_DRIVER_BINARY: &str = "claude";

    struct ScriptedTmux {
        sessions: Vec<String>,
        tokens: std::collections::HashMap<String, String>,
        fields: std::collections::HashMap<(String, String), String>,
        calls: StdMutex<Vec<Vec<String>>>,
    }

    impl ScriptedTmux {
        fn new() -> Self {
            Self {
                sessions: Vec::new(),
                tokens: std::collections::HashMap::new(),
                fields: std::collections::HashMap::new(),
                calls: StdMutex::new(Vec::new()),
            }
        }

        fn with_session(mut self, name: &str, token: &str) -> Self {
            self.sessions.push(name.to_owned());
            self.tokens.insert(name.to_owned(), token.to_owned());
            self
        }

        fn with_field(mut self, session: &str, format: &str, value: &str) -> Self {
            self.fields
                .insert((session.to_owned(), format.to_owned()), value.to_owned());
            self
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    fn ok_tmux(stdout: impl Into<String>) -> boss_tmux::CommandOutput {
        boss_tmux::CommandOutput {
            success: true,
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    #[async_trait]
    impl boss_tmux::CommandRunner for ScriptedTmux {
        async fn run(
            &self,
            _program: &std::path::Path,
            args: &[std::ffi::OsString],
            _cwd: Option<&std::path::Path>,
        ) -> std::io::Result<boss_tmux::CommandOutput> {
            let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
            self.calls.lock().unwrap().push(args.clone());
            match args.get(2).map(String::as_str) {
                Some("list-sessions") => {
                    let stdout = self
                        .sessions
                        .iter()
                        .map(|name| format!("{name}\t\n"))
                        .collect::<String>();
                    Ok(ok_tmux(stdout))
                }
                Some("show-environment") => {
                    let session = &args[4];
                    let var = args[5].as_str();
                    match self.tokens.get(session) {
                        Some(value) if var == boss_tmux::TMUX_SPAWN_TOKEN_ENV => {
                            Ok(ok_tmux(format!("{var}={value}\n")))
                        }
                        _ => Ok(boss_tmux::CommandOutput {
                            success: false,
                            code: Some(1),
                            stdout: String::new(),
                            stderr: format!("unknown variable: {var}"),
                        }),
                    }
                }
                Some("display-message") => {
                    let session = &args[5];
                    let format = &args[6];
                    match self.fields.get(&(session.clone(), format.clone())) {
                        Some(value) => Ok(ok_tmux(format!("{value}\n"))),
                        None => Ok(ok_tmux("\n")),
                    }
                }
                other => panic!("unexpected tmux command in inspector test: {other:?} (full args={args:?})"),
            }
        }
    }

    fn stamp_tmux_run(db: &WorkDb, execution_id: &str, session: &str, token: &str) {
        db.start_execution_run(execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
            .unwrap();
        assert!(
            db.record_tmux_spawn_intent_for_execution(execution_id, "boss", session, token)
                .unwrap()
        );
        assert!(
            db.record_tmux_session_created_for_execution(execution_id, token, 4242)
                .unwrap()
        );
    }

    async fn inspect_with(
        db: WorkDb,
        execution_id: &str,
        server: ScriptedTmux,
    ) -> (Result<Option<TerminalLiveness>>, Vec<Vec<String>>) {
        let server = std::sync::Arc::new(server);
        let tmux = Tmux::with_runner(
            "/opt/homebrew/bin/tmux",
            std::sync::Arc::clone(&server) as std::sync::Arc<dyn boss_tmux::CommandRunner>,
        )
        .unwrap();
        let inspector = TmuxWorkerTerminalInspector::new(std::sync::Arc::new(db), tmux);
        let verdict = inspector.inspect(execution_id).await;
        (verdict, server.calls())
    }

    #[tokio::test]
    async fn inspector_live_pane_with_matching_driver_binary() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_old_execution(&db, &work_item_id);
        stamp_tmux_run(&db, &execution_id, "boss-worker-1", "tok-1");

        let (verdict, calls) = inspect_with(
            db,
            &execution_id,
            ScriptedTmux::new()
                .with_session("boss-worker-1", "tok-1")
                .with_field("boss-worker-1", "#{pane_dead}", "0")
                .with_field("boss-worker-1", "#{window_activity}", "9000")
                .with_field("boss-worker-1", "#{pane_current_command}", CLASSIFIER_DRIVER_BINARY),
        )
        .await;

        assert_eq!(
            verdict.unwrap(),
            Some(TerminalLiveness::Alive {
                session_name: "boss-worker-1".to_owned(),
                window_activity_epoch_secs: 9000,
                agent_is_foreground: true,
                foreground_command_mismatch: false,
            })
        );
        assert_eq!(
            calls,
            vec![
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "list-sessions".to_owned(),
                    "-F".to_owned(),
                    "#{session_name}\t#{@boss_spawn_token}".to_owned(),
                ],
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "show-environment".to_owned(),
                    "-t".to_owned(),
                    "boss-worker-1".to_owned(),
                    "BOSS_SPAWN_TOKEN".to_owned(),
                ],
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "display-message".to_owned(),
                    "-p".to_owned(),
                    "-t".to_owned(),
                    "boss-worker-1".to_owned(),
                    "#{pane_dead}".to_owned(),
                ],
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "display-message".to_owned(),
                    "-p".to_owned(),
                    "-t".to_owned(),
                    "boss-worker-1".to_owned(),
                    "#{window_activity}".to_owned(),
                ],
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "display-message".to_owned(),
                    "-p".to_owned(),
                    "-t".to_owned(),
                    "boss-worker-1".to_owned(),
                    "#{pane_current_command}".to_owned(),
                ],
            ]
        );
    }

    #[tokio::test]
    async fn inspector_missing_session_is_dead() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_old_execution(&db, &work_item_id);
        stamp_tmux_run(&db, &execution_id, "boss-worker-1", "tok-1");

        let (verdict, calls) = inspect_with(db, &execution_id, ScriptedTmux::new()).await;
        assert_eq!(
            verdict.unwrap(),
            Some(TerminalLiveness::Dead {
                session_name: "boss-worker-1".to_owned(),
                evidence: DeadPaneEvidence::SessionAbsent,
            })
        );
        assert_eq!(
            calls,
            vec![vec![
                "-L".to_owned(),
                "boss".to_owned(),
                "list-sessions".to_owned(),
                "-F".to_owned(),
                "#{session_name}\t#{@boss_spawn_token}".to_owned(),
            ]]
        );
    }

    #[tokio::test]
    async fn inspector_token_mismatch_is_dead() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_old_execution(&db, &work_item_id);
        stamp_tmux_run(&db, &execution_id, "boss-worker-1", "tok-1");

        let (verdict, calls) = inspect_with(
            db,
            &execution_id,
            ScriptedTmux::new().with_session("boss-worker-1", "other-token"),
        )
        .await;
        assert_eq!(
            verdict.unwrap(),
            Some(TerminalLiveness::Dead {
                session_name: "boss-worker-1".to_owned(),
                evidence: DeadPaneEvidence::SpawnTokenMismatch,
            })
        );
        assert_eq!(
            calls,
            vec![
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "list-sessions".to_owned(),
                    "-F".to_owned(),
                    "#{session_name}\t#{@boss_spawn_token}".to_owned(),
                ],
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "show-environment".to_owned(),
                    "-t".to_owned(),
                    "boss-worker-1".to_owned(),
                    "BOSS_SPAWN_TOKEN".to_owned(),
                ],
            ]
        );
    }

    #[tokio::test]
    async fn inspector_pane_dead_carries_status() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_old_execution(&db, &work_item_id);
        stamp_tmux_run(&db, &execution_id, "boss-worker-1", "tok-1");

        let (verdict, calls) = inspect_with(
            db,
            &execution_id,
            ScriptedTmux::new()
                .with_session("boss-worker-1", "tok-1")
                .with_field("boss-worker-1", "#{pane_dead}", "1")
                .with_field("boss-worker-1", "#{pane_dead_status}", "143"),
        )
        .await;
        assert_eq!(
            verdict.unwrap(),
            Some(TerminalLiveness::Dead {
                session_name: "boss-worker-1".to_owned(),
                evidence: DeadPaneEvidence::PaneExited {
                    pane_dead_status: Some("143".to_owned()),
                },
            })
        );
        assert_eq!(
            calls,
            vec![
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "list-sessions".to_owned(),
                    "-F".to_owned(),
                    "#{session_name}\t#{@boss_spawn_token}".to_owned(),
                ],
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "show-environment".to_owned(),
                    "-t".to_owned(),
                    "boss-worker-1".to_owned(),
                    "BOSS_SPAWN_TOKEN".to_owned(),
                ],
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "display-message".to_owned(),
                    "-p".to_owned(),
                    "-t".to_owned(),
                    "boss-worker-1".to_owned(),
                    "#{pane_dead}".to_owned(),
                ],
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "display-message".to_owned(),
                    "-p".to_owned(),
                    "-t".to_owned(),
                    "boss-worker-1".to_owned(),
                    "#{pane_dead_status}".to_owned(),
                ],
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "display-message".to_owned(),
                    "-p".to_owned(),
                    "-t".to_owned(),
                    "boss-worker-1".to_owned(),
                    "#{window_activity}".to_owned(),
                ],
            ]
        );
    }

    #[tokio::test]
    async fn inspector_unparseable_window_activity_is_an_error() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_old_execution(&db, &work_item_id);
        stamp_tmux_run(&db, &execution_id, "boss-worker-1", "tok-1");

        let (verdict, calls) = inspect_with(
            db,
            &execution_id,
            ScriptedTmux::new()
                .with_session("boss-worker-1", "tok-1")
                .with_field("boss-worker-1", "#{pane_dead}", "0")
                .with_field("boss-worker-1", "#{window_activity}", "not-a-number"),
        )
        .await;
        assert!(
            verdict.is_err(),
            "unparseable window_activity must be Err, got {verdict:?}"
        );
        assert_eq!(
            calls,
            vec![
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "list-sessions".to_owned(),
                    "-F".to_owned(),
                    "#{session_name}\t#{@boss_spawn_token}".to_owned(),
                ],
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "show-environment".to_owned(),
                    "-t".to_owned(),
                    "boss-worker-1".to_owned(),
                    "BOSS_SPAWN_TOKEN".to_owned(),
                ],
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "display-message".to_owned(),
                    "-p".to_owned(),
                    "-t".to_owned(),
                    "boss-worker-1".to_owned(),
                    "#{pane_dead}".to_owned(),
                ],
                vec![
                    "-L".to_owned(),
                    "boss".to_owned(),
                    "display-message".to_owned(),
                    "-p".to_owned(),
                    "-t".to_owned(),
                    "boss-worker-1".to_owned(),
                    "#{window_activity}".to_owned(),
                ],
            ]
        );
    }

    #[tokio::test]
    async fn inspector_foreground_mismatch_is_flagged() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let execution_id = create_old_execution(&db, &work_item_id);
        stamp_tmux_run(&db, &execution_id, "boss-worker-1", "tok-1");

        let (verdict, _) = inspect_with(
            db,
            &execution_id,
            ScriptedTmux::new()
                .with_session("boss-worker-1", "tok-1")
                .with_field("boss-worker-1", "#{pane_dead}", "0")
                .with_field("boss-worker-1", "#{window_activity}", "9000")
                .with_field("boss-worker-1", "#{pane_current_command}", "node"),
        )
        .await;
        assert_eq!(
            verdict.unwrap(),
            Some(TerminalLiveness::Alive {
                session_name: "boss-worker-1".to_owned(),
                window_activity_epoch_secs: 9000,
                agent_is_foreground: false,
                foreground_command_mismatch: true,
            })
        );
    }
}
