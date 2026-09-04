//! Periodic liveness backstop that detects worker slots whose `claude`
//! process is still alive but has stopped making progress, and (for the
//! non-tmux cadence fallback) reaps them.
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
//! signal is *driver-originated semantic progress* — the last time a real
//! hook/JSONL event advanced the run's [`crate::semantic_progress`]
//! checkpoint, and the tri-state tool condition that checkpoint carries.
//!
//! ## Two authorities, never conflated
//!
//! Tmux decides **exact identity and death only**: is this the precise
//! session/pane Boss spawned, and is it still alive? A live pane's terminal
//! signals — `#{window_activity}` and `#{pane_current_command}` — are
//! diagnostics, never health vetoes. Both follow display repaint rather than
//! semantic work: Claude's spinner advances `window_activity` continuously
//! even while genuinely stuck, and `pane_current_command` is a presentation
//! field (Claude publishes a version string as its process title), not
//! stable process identity. A validated killer TUI can look identical to a
//! wedged one on both fields, so neither can decide "is the agent making
//! useful progress?".
//!
//! Semantic staleness — the actual health verdict — is decided entirely by
//! driver-originated evidence: the run's [`crate::semantic_progress`]
//! checkpoint (last driver event time, tri-state tool condition), the slot's
//! `activity`, any operator hold, and the driver's declared
//! [`crate::driver::ProgressFidelity`] tier. See [`classify_semantic_staleness`].
//!
//! ## Algorithm
//!
//! 1. Snapshot [`crate::live_worker_state::LiveWorkerStateRegistry`].
//! 2. Skip a held slot, a non-`Working` slot, and a terminal execution
//!    (resolving any open attention for the last case).
//! 3. Consult tmux when the run has durable tmux identity:
//!    - Session absent, token mismatched, or pane confirmed dead → existing
//!      corroborated death handling, reaped through the pane-death
//!      reconciliation path.
//!    - Pane live, or the identity probe itself failed → evaluate semantic
//!      staleness (below). A probe failure is never proof of health or
//!      death; it degrades the evidence and is itself counted and, once
//!      past `stale_threshold_secs`, raises a non-destructive attention
//!      recording whatever the semantic-progress evidence says.
//! 4. Runs without any tmux identity (no terminal inspector, or the run
//!    predates tmux columns) use the conservative cadence fallback: skip if
//!    a tool is in flight or the driver's fidelity is exempt from cadence
//!    reaping, require a stale hook timestamp and an execution beyond its
//!    grace period, then mark the execution `orphaned`, append an audit
//!    line, release the pool slot, emit a dispatch event, and kick the
//!    coordinator so stranded work is redispatched. This path already
//!    self-heals correctly for populations without tmux identity.
//!
//! ## Semantic staleness ([`classify_semantic_staleness`])
//!
//! For a `Working`, unheld, non-terminal, tmux-identified (live-or-probe-
//! failed) slot:
//!
//! - Tool condition `in_flight` ⇒ healthy unconditionally, however old the
//!   checkpoint — a long foreground tool call (a multi-minute `bazel build`)
//!   can run with no intervening event, and reaping that would break real
//!   work.
//! - A checkpoint inside `stale_threshold_secs` ⇒ healthy.
//! - No checkpoint at all, or tool condition durably `unknown` ⇒ degraded
//!   evidence — there is not enough evidence to judge staleness either way.
//!   Never destructive; raises a degraded-evidence attention once past the
//!   threshold.
//! - The driver's [`crate::driver::ProgressFidelity`] tier below `Rich` ⇒
//!   degraded evidence. Local dispatch refuses those drivers, so a
//!   below-`Rich` live slot cannot support cadence-based judgement and is
//!   surfaced rather than silently skipped.
//! - Tool condition `idle` and the checkpoint predates `stale_threshold_secs`,
//!   on a `Rich` driver ⇒ genuinely stale. Raises (or updates) the
//!   `stale_worker` attention with the session's attach command. If the same
//!   checkpoint ALSO predates the longer [`DEFAULT_AUTO_REAP_THRESHOLD_SECS`]
//!   (two hours), the sweep does not stop at the attention — see the next
//!   section.
//!
//! ## The two-hour token-verified auto-reap
//!
//! A `Working`, unheld, `Rich`, tmux-confirmed-live slot that reads
//! `SemanticStaleness::Stale` against `stale_threshold_secs` is re-classified
//! against the longer [`DEFAULT_AUTO_REAP_THRESHOLD_SECS`]. If it clears that
//! bar too, [`attempt_auto_reap`] re-verifies the candidate ONE more time,
//! right before acting, rather than trusting the classification this pass
//! already made — possibly several `.await`s earlier:
//!
//! 1. A fresh [`WorkerTerminalInspector::inspect`] call. tmux's own
//!    dead/alive classification embeds the spawn-token check
//!    ([`DeadPaneEvidence::SpawnTokenMismatch`]), so requiring a fresh
//!    `Alive` result here is what makes the reap "token-verified" — it does
//!    not trust the identity this pass established earlier.
//! 2. A fresh [`crate::live_worker_state::LiveWorkerStateRegistry::semantic_progress_for_slot`]
//!    / [`crate::live_worker_state::LiveWorkerStateRegistry::progress_fidelity_for_slot`]
//!    read, reclassified with [`classify_semantic_staleness`] against
//!    `DEFAULT_AUTO_REAP_THRESHOLD_SECS`.
//!
//! A hold (checked both at the top of the per-slot loop and again just before
//! destructive action), a new driver
//! event, a tool going in flight, a durably-unknown tool condition, a
//! failed probe, or a changed/dead tmux identity all block the reap — the
//! candidate falls back to the ordinary non-destructive `genuinely_stuck`
//! attention and is reconsidered next pass. Only once the recheck itself
//! reconfirms `Stale` does [`execute_auto_reap`] run, in this order: a
//! recovery backup of uncommitted workspace work, marking the execution
//! orphaned and appending the `[engine-reconcile]` audit line, tearing down
//! driver-owned state, the token-verified tmux reap
//! ([`StaleWorkerReaper::reap_worker`] — the exact teardown `bossctl agents
//! stop` performs), guarded worker-slot release, then cube-lease release.
//!
//! ## Cadence
//!
//! Runs every 60 seconds and fires once immediately on boot (same
//! pattern as [`crate::dead_pid_sweep`] / [`crate::orphan_sweep`]).
//!
//! ## Observability independent of reaping
//!
//! [`StaleWorkerSweepOutcome::has_activity`] fires on any of a stuck
//! candidate, a probe failure, degraded evidence, or an uncorroborated dead
//! inference — not only on `reaped > 0` — so a pass that raises attention
//! without reaping anything is still logged.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use boss_protocol::WorkerActivity;
use boss_tmux::Tmux;

use crate::coordinator::{CubeClient, ExecutionCoordinator, worker_id_for_slot};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::driver::ProgressFidelity;
use crate::hold_registry::HoldRegistry;
use crate::live_worker_state::{LiveWorkerStateRegistry, iso8601_utc};
use crate::semantic_progress::SemanticToolCondition;
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

/// Tmux evidence for one worker pane. Decides exact identity and death
/// only — never a health verdict. `window_activity_epoch_secs` and
/// `pane_current_command` are carried for operator diagnostics (they can
/// help someone inspecting a session) and are never read by
/// [`classify_semantic_staleness`] or anything upstream of it: tmux's
/// `#{window_activity}` follows display repaint (Claude's spinner advances
/// it continuously even while semantically stuck) and
/// `#{pane_current_command}` is a presentation field (Claude publishes a
/// version string as its process title), not stable process identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalLiveness {
    /// The session and its token are present and the pane is still live.
    Alive {
        session_name: String,
        /// Diagnostic only. tmux's epoch-second `#{window_activity}` value.
        /// `None` when the field was unreadable or unparseable — missing
        /// diagnostics never fail the identity probe.
        window_activity_epoch_secs: Option<i64>,
        /// Diagnostic only. tmux's `#{pane_current_command}`.
        pane_current_command: Option<String>,
    },
    /// The session is absent, its token no longer matches, or tmux retained
    /// the pane after its agent process exited.
    Dead {
        session_name: String,
        evidence: DeadPaneEvidence,
    },
}

/// Why a `Working` slot's staleness cannot be judged from driver evidence.
/// Never destructive on its own; [`SemanticStaleness::DegradedEvidence`]
/// raises a non-destructive attention once past `stale_threshold_secs`
/// rather than being silently skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradedEvidenceReason {
    /// No driver event has ever established idle/in-flight for this run —
    /// no [`crate::semantic_progress::SemanticProgressCheckpoint`] at all,
    /// or one whose tool condition is still
    /// [`crate::semantic_progress::SemanticToolCondition::Unknown`].
    ToolConditionUnknown,
    /// The driver's declared [`crate::driver::ProgressFidelity`] tier is
    /// below `Rich`. Local (tmux-hosted) dispatch is expected to supply
    /// `Rich` progress; a lower tier cannot support cadence-based
    /// judgement, so it is surfaced rather than silently exempted.
    FidelityBelowRich,
}

/// Semantic staleness verdict for a `Working`, unheld, non-terminal,
/// tmux-identified slot (live pane or a failed identity probe — see
/// [`classify_semantic_staleness`]). Computed entirely from
/// driver-originated evidence: [`crate::semantic_progress::SemanticToolCondition`],
/// the semantic-progress checkpoint's timestamp, and
/// [`crate::driver::ProgressFidelity`]. Tmux terminal signals never appear
/// here — see [`TerminalLiveness`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticStaleness {
    /// A tool is in flight, or the last driver-originated event is inside
    /// `stale_threshold_secs`.
    Healthy,
    DegradedEvidence(DegradedEvidenceReason),
    /// `Rich` driver, tool durably idle, and the last driver-originated
    /// event predates `stale_threshold_secs`.
    Stale {
        progress_at: String,
    },
}

/// Classify a `Working` slot's semantic staleness from driver-originated
/// evidence alone. `progress_at`/`tool_condition` come from
/// [`crate::live_worker_state::LiveWorkerStateRegistry::semantic_progress_for_slot`]
/// (`None`/[`SemanticToolCondition::Unknown`] for a slot with no recorded
/// checkpoint); `fidelity` from
/// [`crate::live_worker_state::LiveWorkerStateRegistry::progress_fidelity_for_slot`].
/// `started_at` is the execution's own `started_epoch`, formatted with
/// [`iso8601_utc`] — the clock a run with no checkpoint at all is judged
/// against, so "no driver evidence yet" is gated by `stale_threshold_secs`
/// the same as every other verdict rather than firing right after
/// [`STALE_GRACE_SECS`].
///
/// `in_flight` is unconditionally healthy however old the checkpoint is: a
/// long foreground tool call (a multi-minute `bazel build`) can run with no
/// intervening event, and reaping that would break real work. The guard is
/// the durable checkpoint's tool condition, not a transient hook-derived
/// `current_tool` field.
pub fn classify_semantic_staleness(
    tool_condition: SemanticToolCondition,
    progress_at: Option<&str>,
    started_at: &str,
    fidelity: ProgressFidelity,
    now_epoch_secs: i64,
    stale_threshold_secs: i64,
) -> SemanticStaleness {
    if tool_condition == SemanticToolCondition::InFlight {
        return SemanticStaleness::Healthy;
    }
    let effective_at = progress_at.unwrap_or(started_at);
    let stale_cutoff = iso8601_utc(now_epoch_secs - stale_threshold_secs);
    if effective_at >= stale_cutoff.as_str() {
        return SemanticStaleness::Healthy;
    }
    if fidelity != ProgressFidelity::Rich {
        return SemanticStaleness::DegradedEvidence(DegradedEvidenceReason::FidelityBelowRich);
    }
    match tool_condition {
        SemanticToolCondition::InFlight => unreachable!("in_flight returned Healthy above"),
        SemanticToolCondition::Unknown => {
            SemanticStaleness::DegradedEvidence(DegradedEvidenceReason::ToolConditionUnknown)
        }
        SemanticToolCondition::Idle => SemanticStaleness::Stale {
            progress_at: effective_at.to_owned(),
        },
    }
}

/// Reads terminal liveness evidence for a run. An unavailable probe is an
/// error, not death: the sweep must never convert an observation failure into
/// a destructive action.
#[async_trait::async_trait]
pub trait WorkerTerminalInspector: Send + Sync {
    async fn inspect(&self, execution_id: &str) -> Result<Option<TerminalLiveness>>;

    /// Operator-facing `tmux …` prefix for inspect/kill commands in attention items.
    fn operator_prefix(&self) -> String {
        format!("tmux -S {}", boss_tmux::TEST_SOCKET_PATH)
    }

    /// Operator-facing `tmux …` prefix for `execution_id`'s specific run —
    /// the durable socket, unless that run's recorded identity still lives
    /// on the pre-move `-L boss` server, in which case a command addressed
    /// there instead. Defaults to [`Self::operator_prefix`] for implementors
    /// (including test stubs) that never route by run.
    fn operator_prefix_for_run(&self, execution_id: &str) -> String {
        let _ = execution_id;
        self.operator_prefix()
    }

    /// Operator-facing attach command for an execution's session.
    fn attach_session_command_for_run(&self, execution_id: &str, session_name: &str) -> String {
        format!(
            "{} attach-session -t {}",
            self.operator_prefix_for_run(execution_id),
            boss_tmux::quote_for_shell(session_name)
        )
    }
}

/// Production tmux inspector. A run without tmux identity returns `None` so
/// the legacy cadence fallback remains available while pools migrate.
pub struct TmuxWorkerTerminalInspector {
    work_db: Arc<WorkDb>,
    tmux: Tmux,
    legacy_tmux: Option<Tmux>,
}

impl TmuxWorkerTerminalInspector {
    pub fn new(work_db: Arc<WorkDb>, tmux: Tmux, legacy_tmux: Option<Tmux>) -> Self {
        Self {
            work_db,
            tmux,
            legacy_tmux,
        }
    }

    fn tmux_for_run(&self, server_label: &str) -> &Tmux {
        if server_label == boss_tmux::SERVER_LABEL {
            self.legacy_tmux.as_ref().unwrap_or(&self.tmux)
        } else {
            &self.tmux
        }
    }
}

#[async_trait::async_trait]
impl WorkerTerminalInspector for TmuxWorkerTerminalInspector {
    fn operator_prefix(&self) -> String {
        self.tmux.operator_prefix()
    }

    fn operator_prefix_for_run(&self, execution_id: &str) -> String {
        match self.work_db.tmux_run_for_execution(execution_id) {
            Ok(Some(run)) => self.tmux_for_run(&run.tmux_server_label).operator_prefix(),
            Ok(None) | Err(_) => self.tmux.operator_prefix(),
        }
    }

    fn attach_session_command_for_run(&self, execution_id: &str, session_name: &str) -> String {
        match self.work_db.tmux_run_for_execution(execution_id) {
            Ok(Some(run)) => self
                .tmux_for_run(&run.tmux_server_label)
                .attach_session_command(session_name),
            Ok(None) | Err(_) => self.tmux.attach_session_command(session_name),
        }
    }

    async fn inspect(&self, execution_id: &str) -> Result<Option<TerminalLiveness>> {
        let Some(run) = self.work_db.tmux_run_for_execution(execution_id)? else {
            return Ok(None);
        };
        let tmux = self.tmux_for_run(&run.tmux_server_label);

        let session_exists = tmux
            .list_sessions()
            .await?
            .iter()
            .any(|session| session.name == run.tmux_session_name);
        let observation = crate::tmux_adoption::observe_tmux_identity(
            tmux,
            &run.tmux_session_name,
            &run.tmux_spawn_token,
            session_exists,
        )
        .await;
        match observation.adoption_state {
            boss_protocol::TmuxAdoptionState::NotTmuxHosted => {
                unreachable!("observe_tmux_identity never classifies NotTmuxHosted")
            }
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
                // Carried for operator diagnostics only: `#{pane_current_command}`
                // is a presentation field (Claude publishes a version string as
                // its process title), so it must never be compared against the
                // driver binary to infer health. `#{window_activity}` follows
                // display repaint, not semantic work, and a missing reading
                // is `None` rather than a probe failure.
                Ok(Some(TerminalLiveness::Alive {
                    session_name: run.tmux_session_name,
                    window_activity_epoch_secs: observation.window_activity_epoch_secs,
                    pane_current_command: observation.current_command,
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

/// Second, longer threshold. A confirmed-live (tmux-verified), `Rich`,
/// `Working` slot whose tool condition is durably idle and whose
/// semantic-progress checkpoint ALSO predates this — not just
/// `stale_threshold_secs` — is not only flagged, it is destructively
/// reaped, after one more just-in-time recheck (see the module doc's
/// "two-hour token-verified auto-reap" section and [`attempt_auto_reap`]).
/// Two hours is deliberately far past [`DEFAULT_STALE_THRESHOLD_SECS`]: the
/// shorter threshold exists so a human notices; this one exists for the
/// case nobody does, so a wedge does not hold a workspace, cube lease, and
/// worker slot hostage indefinitely.
pub const DEFAULT_AUTO_REAP_THRESHOLD_SECS: i64 = 7_200;

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
/// requirement as the `bossctl agents stop` leak in #1006; this is
/// another retire path that skipped teardown).
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
    /// `SemanticStaleness::Stale` — a `Rich`, `Working` slot whose tool
    /// condition is durably idle and whose semantic-progress checkpoint
    /// predates the threshold, with a live tmux identity. Raises (or
    /// updates) the `stale_worker` attention; never reaped by this module.
    pub genuinely_stuck: usize,
    pub dead: usize,
    /// The tmux identity probe returned `Err` (e.g. the tmux server is
    /// unreachable). Never treated as death or health: the slot still goes
    /// through semantic-staleness evaluation and, once past
    /// `stale_threshold_secs`, raises a probe-unavailable attention
    /// recording that cadence result. Reaping stays blocked regardless of
    /// what the cadence result says.
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
    /// Cadence-fallback slots (no tmux identity for the run) exempted
    /// because the driver's [`crate::driver::ProgressFidelity`] tier
    /// (`Coarse`/`Minimal`) is exempt from hook-cadence-based reaping —
    /// those drivers do not emit hooks reliably enough for silence to imply
    /// a stall. Distinct from [`Self::fidelity_below_rich`], which covers
    /// the same tiers on the tmux-identified path.
    pub fidelity_exempt_skipped: usize,
    /// `SemanticStaleness::DegradedEvidence(ToolConditionUnknown)` — a
    /// tmux-identified `Working` slot with no semantic-progress checkpoint
    /// at all, or one whose tool condition is still durably `unknown`.
    /// Non-destructive; raises a degraded-evidence attention once past
    /// `stale_threshold_secs`.
    pub tool_condition_unknown: usize,
    /// `SemanticStaleness::DegradedEvidence(FidelityBelowRich)` — a
    /// tmux-identified `Working` slot whose driver declares below-`Rich`
    /// progress fidelity. Local (tmux-hosted) dispatch is expected to
    /// supply `Rich` progress, so this case is surfaced with a
    /// degraded-evidence attention rather than silently exempted.
    pub fidelity_below_rich: usize,
    /// Inferred `Dead` (absent session / token mismatch) where
    /// [`crate::durable_liveness`] still reports an alive pid, so the sweep
    /// refused to reap.
    pub dead_uncorroborated: usize,
    /// A `Rich`, `Working`, tmux-confirmed-live slot cleared the two-hour
    /// [`DEFAULT_AUTO_REAP_THRESHOLD_SECS`] bar on this pass's initial
    /// classification, but [`attempt_auto_reap`]'s just-in-time recheck —
    /// a fresh tmux identity probe and a fresh semantic-progress read,
    /// immediately before the destructive sequence — no longer confirmed
    /// it. The reap was aborted; the slot falls back to (and is also
    /// counted in) the ordinary [`Self::genuinely_stuck`] attention.
    pub auto_reap_aborted: usize,
}

impl crate::sweep_loop::SweepOutcome for StaleWorkerSweepOutcome {
    /// Independent of `reaped`: any degraded counter alone makes the pass
    /// worth logging, so the sweep's own degradation is visible without a
    /// reap.
    fn has_activity(&self) -> bool {
        self.reaped > 0
            || self.genuinely_stuck > 0
            || self.dead > 0
            || self.terminal_probe_failed > 0
            || self.tool_condition_unknown > 0
            || self.fidelity_below_rich > 0
            || self.dead_uncorroborated > 0
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
            tool_condition_unknown = self.tool_condition_unknown,
            fidelity_below_rich = self.fidelity_below_rich,
            dead_uncorroborated = self.dead_uncorroborated,
            auto_reap_aborted = self.auto_reap_aborted,
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

/// The two staleness thresholds one pass classifies against. Bundled to
/// keep [`run_one_pass_with_terminal`] under clippy's argument-count lint
/// once [`DEFAULT_AUTO_REAP_THRESHOLD_SECS`] joined `stale_threshold_secs`
/// as a second, independently-configurable bar.
#[derive(Debug, Clone, Copy)]
pub struct StaleWorkerThresholds {
    /// Non-destructive attention bar — see [`DEFAULT_STALE_THRESHOLD_SECS`].
    pub stale_threshold_secs: i64,
    /// Destructive auto-reap bar — see [`DEFAULT_AUTO_REAP_THRESHOLD_SECS`]
    /// and the module doc's "two-hour token-verified auto-reap" section.
    pub auto_reap_threshold_secs: i64,
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so a worker that wedged before the engine
/// restarted is recovered at boot without waiting for the first interval.
pub fn spawn_loop(
    deps: StaleWorkerSweepDeps,
    interval: Duration,
    stale_threshold_secs: i64,
    auto_reap_threshold_secs: i64,
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
                StaleWorkerThresholds {
                    stale_threshold_secs,
                    auto_reap_threshold_secs,
                },
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
        StaleWorkerThresholds {
            stale_threshold_secs,
            // No terminal inspector on this path, so the tmux-identified
            // auto-reap branch is unreachable — this value is never
            // consulted.
            auto_reap_threshold_secs: DEFAULT_AUTO_REAP_THRESHOLD_SECS,
        },
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
    thresholds: StaleWorkerThresholds,
) -> StaleWorkerSweepOutcome {
    let StaleWorkerSweepControls {
        reaper,
        hold_registry,
        cube_client,
    } = controls;
    let StaleWorkerThresholds {
        stale_threshold_secs,
        auto_reap_threshold_secs,
    } = thresholds;
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

        // Tmux is consulted for exact identity and death only — never for a
        // health verdict (see the module doc's "Two authorities" section).
        let identity = match terminal_inspector {
            Some(inspector) => match inspector.inspect(&state.run_id).await {
                Ok(Some(TerminalLiveness::Dead { session_name, evidence })) => {
                    // Tmux's death signal is authoritative on its own terms;
                    // it needs no cadence/semantic evidence.
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
                    continue;
                }
                Ok(Some(TerminalLiveness::Alive { session_name, .. })) => Some(TmuxIdentity::Live { session_name }),
                Ok(None) => None,
                Err(err) => {
                    // A failed probe is never proof of health or death — it
                    // degrades the evidence available for this pass. Fall
                    // through to the same semantic-staleness evaluation a
                    // live pane gets, rather than skipping the slot, so
                    // `terminal_probe_failed` is visible even when nothing
                    // is reaped.
                    tracing::warn!(
                        execution_id = %state.run_id,
                        ?err,
                        "stale-worker sweep: tmux identity probe failed; evaluating semantic evidence only",
                    );
                    outcome.terminal_probe_failed += 1;
                    Some(TmuxIdentity::ProbeUnavailable)
                }
            },
            None => None,
        };

        let Some(identity) = identity else {
            // No tmux identity for this run at all (no terminal inspector
            // configured, or the run predates tmux columns) — the
            // conservative cadence-only fallback below remains this
            // population's liveness backstop.
            run_cadence_fallback(
                CadenceFallbackContext {
                    work_db,
                    live_states,
                    coordinator: coordinator.clone(),
                    dispatch_events,
                    reaper,
                },
                SweepClocks {
                    grace_cutoff,
                    now_epoch_secs,
                    stale_threshold_secs,
                },
                &state,
                &execution,
                &mut outcome,
            )
            .await;
            continue;
        };

        let Some(started_epoch) = execution.started_epoch() else {
            outcome.grace_skipped += 1;
            continue;
        };
        if started_epoch >= grace_cutoff {
            outcome.grace_skipped += 1;
            continue;
        }

        let checkpoint = live_states.semantic_progress_for_slot(state.slot_id);
        let (tool_condition, progress_at) = match &checkpoint {
            Some(checkpoint) => (checkpoint.tool_condition, Some(checkpoint.progress_at.as_str())),
            None => (SemanticToolCondition::Unknown, None),
        };
        let fidelity = live_states.progress_fidelity_for_slot(state.slot_id);
        let started_at = iso8601_utc(started_epoch);

        let verdict = classify_semantic_staleness(
            tool_condition,
            progress_at,
            &started_at,
            fidelity,
            now_epoch_secs,
            stale_threshold_secs,
        );

        // Health is decided purely from `verdict` (semantic evidence) — tmux
        // identity never enters the health decision. A confirmed-live
        // identity is a *precondition* for the `Stale`/degraded attentions
        // below, though: without it, Boss cannot yet name the exact session
        // to attach to or claim "this pane's identity is verified", so a
        // probe-unavailable slot always gets the probe-unavailable
        // attention (recording whatever `verdict` says) rather than
        // `genuinely_stuck` or a fidelity/tool-condition attention, however
        // stale the semantic evidence looks. "Healthy" is the one verdict
        // exempt from that downgrade: it needs no tmux corroboration to
        // resolve a stale attention, because it was never based on tmux
        // evidence in the first place.
        match (verdict, &identity) {
            (SemanticStaleness::Healthy, _) => {
                outcome.alive_and_working += 1;
                resolve_stale_worker_attention_for_work_item(work_db, &execution.work_item_id);
            }
            (verdict, TmuxIdentity::ProbeUnavailable) => {
                raise_probe_unavailable_attention(
                    AttentionContext {
                        work_db,
                        dispatch_events,
                        state: &state,
                        execution: &execution,
                        stale_threshold_secs,
                    },
                    &verdict,
                )
                .await;
            }
            (SemanticStaleness::DegradedEvidence(reason), TmuxIdentity::Live { session_name }) => {
                match reason {
                    DegradedEvidenceReason::ToolConditionUnknown => outcome.tool_condition_unknown += 1,
                    DegradedEvidenceReason::FidelityBelowRich => outcome.fidelity_below_rich += 1,
                }
                raise_degraded_evidence_attention(
                    AttentionContext {
                        work_db,
                        dispatch_events,
                        state: &state,
                        execution: &execution,
                        stale_threshold_secs,
                    },
                    terminal_inspector,
                    session_name,
                    reason,
                    fidelity,
                )
                .await;
            }
            (SemanticStaleness::Stale { progress_at }, TmuxIdentity::Live { session_name }) => {
                // The same checkpoint also cleared the longer auto-reap
                // bar on this pass's initial read — attempt the
                // token-verified recheck-and-reap before falling back to
                // the ordinary (non-destructive) stuck attention.
                let cleared_auto_reap_bar = matches!(
                    classify_semantic_staleness(
                        tool_condition,
                        Some(progress_at.as_str()),
                        &started_at,
                        fidelity,
                        now_epoch_secs,
                        auto_reap_threshold_secs,
                    ),
                    SemanticStaleness::Stale { .. }
                );
                if cleared_auto_reap_bar {
                    let decision = attempt_auto_reap(
                        AutoReapContext::builder()
                            .work_db(work_db)
                            .live_states(live_states)
                            .hold_registry(hold_registry)
                            .coordinator(coordinator.clone())
                            .dispatch_events(dispatch_events)
                            .reaper(reaper)
                            .cube_client(cube_client)
                            .terminal_inspector(
                                terminal_inspector.expect("Live identity implies an inspector produced it"),
                            )
                            .build(),
                        &state,
                        &execution,
                        &started_at,
                        auto_reap_threshold_secs,
                        now_epoch_secs,
                    )
                    .await;
                    if matches!(decision, AutoReapDecision::Reaped) {
                        outcome.reaped += 1;
                        continue;
                    }
                    outcome.auto_reap_aborted += 1;
                }
                outcome.genuinely_stuck += 1;
                raise_stuck_attention(
                    AttentionContext {
                        work_db,
                        dispatch_events,
                        state: &state,
                        execution: &execution,
                        stale_threshold_secs,
                    },
                    terminal_inspector,
                    session_name,
                    &progress_at,
                )
                .await;
            }
        }
        continue;
    }

    outcome
}

/// Tmux identity evidence available for a slot whose terminal inspector did
/// not report `Dead` — either a confirmed-live pane, or an identity probe
/// that itself failed (never proof of health or death; see the module doc).
enum TmuxIdentity {
    Live { session_name: String },
    ProbeUnavailable,
}

/// Shared collaborators for the non-destructive tmux-path attentions.
struct AttentionContext<'a> {
    work_db: &'a WorkDb,
    dispatch_events: &'a dyn DispatchEventSink,
    state: &'a boss_protocol::LiveWorkerState,
    execution: &'a boss_protocol::WorkExecution,
    stale_threshold_secs: i64,
}

/// Pass-level clocks for the cadence fallback.
struct SweepClocks {
    grace_cutoff: i64,
    now_epoch_secs: i64,
    stale_threshold_secs: i64,
}

/// Collaborators for [`run_cadence_fallback`], bundled so the function
/// stays under clippy's argument-count lint (same convention as
/// [`StaleWorkerSweepDeps`]). Pass-level clocks stay positional via
/// [`SweepClocks`].
struct CadenceFallbackContext<'a> {
    work_db: &'a WorkDb,
    live_states: &'a LiveWorkerStateRegistry,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &'a dyn DispatchEventSink,
    reaper: &'a dyn StaleWorkerReaper,
}

fn semantic_verdict_label(verdict: &SemanticStaleness) -> &'static str {
    match verdict {
        SemanticStaleness::Healthy => "healthy",
        SemanticStaleness::DegradedEvidence(DegradedEvidenceReason::ToolConditionUnknown) => "tool_condition_unknown",
        SemanticStaleness::DegradedEvidence(DegradedEvidenceReason::FidelityBelowRich) => "fidelity_below_rich",
        SemanticStaleness::Stale { .. } => "genuinely_stuck",
    }
}

/// Shared tail for the two non-destructive tmux-path attentions: upsert the
/// `stale_worker` attention and emit a matching `Outcome::Skipped` dispatch
/// event, so a pass that only raised attention (no reap) is still
/// observable via structured telemetry (`details.verdict` names the case).
/// `semantic_verdict` is the underlying cadence result when `verdict` is
/// `probe_unavailable`; omitted for the other cases.
async fn upsert_attention_and_emit(
    ctx: AttentionContext<'_>,
    title: &str,
    body: &str,
    verdict: &str,
    semantic_verdict: Option<&str>,
) {
    if let Err(err) = ctx.work_db.upsert_external_tracker_attention(
        &ctx.execution.work_item_id,
        STALE_WORKER_ATTENTION_KIND,
        title,
        body,
    ) {
        tracing::warn!(
            execution_id = %ctx.state.run_id,
            ?err,
            "stale-worker sweep: failed to raise attention",
        );
    }
    let mut details = serde_json::json!({
        "slot_id": ctx.state.slot_id,
        "verdict": verdict,
    });
    if let Some(semantic_verdict) = semantic_verdict {
        details["semantic_verdict"] = serde_json::Value::String(semantic_verdict.to_owned());
    }
    ctx.dispatch_events
        .emit(
            DispatchEvent::new(Stage::StaleWorkerReconcile, Outcome::Skipped, &ctx.state.run_id)
                .with_work_item(&ctx.execution.work_item_id)
                .with_details(details),
        )
        .await;
}

/// Raise (or update) the `stale_worker` attention for a `Rich`-driver,
/// `Working` slot whose tool condition is durably idle and whose
/// semantic-progress checkpoint predates `stale_threshold_secs`, with a
/// confirmed-live tmux identity. Never reaps — the two-hour token-verified
/// auto-reap is a later, dependent change.
async fn raise_stuck_attention(
    ctx: AttentionContext<'_>,
    terminal_inspector: Option<&dyn WorkerTerminalInspector>,
    session_name: &str,
    progress_at: &str,
) {
    let attach_cmd = terminal_inspector
        .expect("Live identity implies an inspector produced it")
        .attach_session_command_for_run(&ctx.state.run_id, session_name);
    let body = format!(
        "Worker execution `{}` has had no driver-originated progress for more than {} seconds \
         while `Working` with its tool condition durably idle. The tmux session is still live, so Boss did not \
         reap it.\n\nLast driver event: {progress_at}\n\nSession: `{session_name}`\n\nInspect it with:\n\n```sh\n{attach_cmd}\n```",
        ctx.state.run_id, ctx.stale_threshold_secs,
    );
    upsert_attention_and_emit(
        ctx,
        "Worker appears stuck; inspection required",
        &body,
        "genuinely_stuck",
        None,
    )
    .await;
}

/// Raise (or update) the `stale_worker` attention for a tmux-identified
/// (confirmed-live) `Working` slot whose staleness cannot be judged from
/// driver evidence — durably unknown tool condition, or below-`Rich`
/// fidelity. Never destructive.
async fn raise_degraded_evidence_attention(
    ctx: AttentionContext<'_>,
    terminal_inspector: Option<&dyn WorkerTerminalInspector>,
    session_name: &str,
    reason: DegradedEvidenceReason,
    fidelity: ProgressFidelity,
) {
    let (verdict, evidence_line) = match reason {
        DegradedEvidenceReason::ToolConditionUnknown => (
            "tool_condition_unknown",
            "no driver event has ever established whether a tool is in flight for this run (tool condition \
             durably `unknown`)."
                .to_owned(),
        ),
        DegradedEvidenceReason::FidelityBelowRich => (
            "fidelity_below_rich",
            format!(
                "its driver reports {fidelity:?} progress fidelity, below the `Rich` tier local (tmux-hosted) \
                 dispatch requires for cadence-based staleness judgement."
            ),
        ),
    };
    let attach_cmd = terminal_inspector
        .expect("Live identity implies an inspector produced it")
        .attach_session_command_for_run(&ctx.state.run_id, session_name);
    let body = format!(
        "Worker execution `{}` is `Working`, but Boss cannot judge staleness for it: {evidence_line} Boss is \
         neither reaping nor assuming health while this evidence gap persists (more than {} \
         seconds).\n\nSession: `{session_name}`\n\nInspect it with:\n\n```sh\n{attach_cmd}\n```",
        ctx.state.run_id, ctx.stale_threshold_secs,
    );
    upsert_attention_and_emit(
        ctx,
        "Worker evidence degraded; staleness cannot be judged",
        &body,
        verdict,
        None,
    )
    .await;
}

/// Raise (or update) the `stale_worker` attention for a `Working` slot
/// whose tmux identity probe itself failed this pass. Never destructive,
/// and never claims `genuinely_stuck` or a fidelity/tool-condition
/// verdict: without a confirmed-live session, Boss cannot name an exact
/// identity to act on, so this attention always fires once a
/// non-`Healthy` semantic verdict persists past `stale_threshold_secs`,
/// regardless of what that verdict specifically was — it records the
/// cadence result rather than acting on it. Structured telemetry keeps
/// `"verdict": "probe_unavailable"` and adds `"semantic_verdict"` for the
/// underlying cadence result.
async fn raise_probe_unavailable_attention(ctx: AttentionContext<'_>, verdict: &SemanticStaleness) {
    let cadence_note = match verdict {
        SemanticStaleness::Healthy => unreachable!("Healthy is handled before this attention is raised"),
        SemanticStaleness::DegradedEvidence(DegradedEvidenceReason::ToolConditionUnknown) => {
            "no driver-originated event has established a tool condition for this run either".to_owned()
        }
        SemanticStaleness::DegradedEvidence(DegradedEvidenceReason::FidelityBelowRich) => {
            "the driver's progress fidelity is also below `Rich`, so cadence judgement would not have been \
             possible even with a confirmed identity"
                .to_owned()
        }
        SemanticStaleness::Stale { progress_at } => {
            format!("driver-originated evidence alone reads as stale (last driver event: {progress_at})")
        }
    };
    let body = format!(
        "Worker execution `{}` is `Working`, but the tmux identity probe failed this pass, so Boss could not \
         confirm the session's exact process identity — {cadence_note}. This is not treated as death; automatic \
         action stays blocked until a later pass re-establishes exact identity (more than {} \
         seconds).",
        ctx.state.run_id, ctx.stale_threshold_secs,
    );
    let semantic_verdict = semantic_verdict_label(verdict);
    upsert_attention_and_emit(
        ctx,
        "Worker identity probe unavailable",
        &body,
        "probe_unavailable",
        Some(semantic_verdict),
    )
    .await;
}

/// Outcome of [`attempt_auto_reap`]'s just-in-time recheck.
enum AutoReapDecision {
    /// The recheck reconfirmed live identity and `Stale` evidence;
    /// [`execute_auto_reap`] ran to completion.
    Reaped,
    /// The recheck did not reconfirm eligibility — see the `tracing::warn!`
    /// immediately before the specific return site for why. The caller
    /// falls back to the ordinary non-destructive attention.
    Aborted,
}

/// Collaborators for [`attempt_auto_reap`] / [`execute_auto_reap`], bundled
/// (with a builder, matching [`StaleWorkerSweepDeps`]/`ReapOptions`'s
/// convention for a collaborator struct past 5 fields) so the call site
/// stays under clippy's argument-count lint.
#[derive(bon::Builder)]
struct AutoReapContext<'a> {
    work_db: &'a WorkDb,
    live_states: &'a LiveWorkerStateRegistry,
    hold_registry: &'a HoldRegistry,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &'a dyn DispatchEventSink,
    reaper: &'a dyn StaleWorkerReaper,
    cube_client: &'a dyn CubeClient,
    terminal_inspector: &'a dyn WorkerTerminalInspector,
}

/// Just-in-time recheck for the two-hour auto-reap, run immediately before
/// any destructive action: a fresh tmux identity probe (which re-validates
/// the spawn token as part of its own dead/alive classification — see
/// [`TmuxWorkerTerminalInspector::inspect`]) and a fresh semantic-progress
/// read, reclassified against `auto_reap_threshold_secs`. The
/// classification that got the caller here can be several `.await`s old by
/// the time it would otherwise act; this function is what makes the reap
/// "token-verified" rather than trusting that earlier snapshot. Returns
/// [`AutoReapDecision::Reaped`] only if the recheck itself still reads a
/// confirmed-live, `Stale` candidate — in which case [`execute_auto_reap`]
/// has completed successfully.
async fn attempt_auto_reap(
    ctx: AutoReapContext<'_>,
    state: &boss_protocol::LiveWorkerState,
    execution: &boss_protocol::WorkExecution,
    started_at: &str,
    auto_reap_threshold_secs: i64,
    now_epoch_secs: i64,
) -> AutoReapDecision {
    let execution_id = &state.run_id;

    let session_name = match ctx.terminal_inspector.inspect(execution_id).await {
        Ok(Some(TerminalLiveness::Alive { session_name, .. })) => session_name,
        Ok(Some(TerminalLiveness::Dead { session_name, evidence })) => {
            tracing::warn!(
                execution_id,
                session_name,
                ?evidence,
                "stale-worker sweep: auto-reap recheck found the tmux identity no longer live; \
                 aborting the reap (falling back to the non-destructive attention)",
            );
            return AutoReapDecision::Aborted;
        }
        Ok(None) => {
            tracing::warn!(
                execution_id,
                "stale-worker sweep: auto-reap recheck lost tmux identity for this run; aborting the reap",
            );
            return AutoReapDecision::Aborted;
        }
        Err(err) => {
            tracing::warn!(
                execution_id,
                ?err,
                "stale-worker sweep: auto-reap recheck's tmux identity probe failed; aborting the reap",
            );
            return AutoReapDecision::Aborted;
        }
    };

    let checkpoint = ctx.live_states.semantic_progress_for_slot(state.slot_id);
    let (tool_condition, progress_at) = match &checkpoint {
        Some(checkpoint) => (checkpoint.tool_condition, Some(checkpoint.progress_at.as_str())),
        None => (SemanticToolCondition::Unknown, None),
    };
    let fidelity = ctx.live_states.progress_fidelity_for_slot(state.slot_id);
    let verdict = classify_semantic_staleness(
        tool_condition,
        progress_at,
        started_at,
        fidelity,
        now_epoch_secs,
        auto_reap_threshold_secs,
    );
    let SemanticStaleness::Stale { progress_at } = verdict else {
        tracing::warn!(
            execution_id,
            session_name,
            verdict = semantic_verdict_label(&verdict),
            "stale-worker sweep: auto-reap recheck's semantic evidence no longer reads stale; \
             aborting the reap (falling back to the non-destructive attention)",
        );
        return AutoReapDecision::Aborted;
    };

    if ctx.hold_registry.is_held(execution_id) {
        tracing::warn!(
            execution_id,
            "stale-worker sweep: auto-reap recheck found an operator hold; aborting the reap",
        );
        return AutoReapDecision::Aborted;
    }

    if execute_auto_reap(
        ctx,
        state,
        execution,
        &session_name,
        &progress_at,
        auto_reap_threshold_secs,
        now_epoch_secs,
    )
    .await
    {
        AutoReapDecision::Reaped
    } else {
        AutoReapDecision::Aborted
    }
}

/// Run once [`attempt_auto_reap`]'s recheck has reconfirmed a live,
/// `Stale` candidate: recovery backup, orphan the execution and append the
/// `[engine-reconcile]` audit line, tear down driver-owned state, the
/// token-verified tmux reap ([`StaleWorkerReaper::reap_worker`] — the
/// exact teardown `bossctl agents stop` performs), release the cube lease
/// and worker slot, and kick the coordinator so the chore is redispatched
/// — in that order.
async fn execute_auto_reap(
    ctx: AutoReapContext<'_>,
    state: &boss_protocol::LiveWorkerState,
    execution: &boss_protocol::WorkExecution,
    session_name: &str,
    progress_at: &str,
    auto_reap_threshold_secs: i64,
    now_epoch_secs: i64,
) -> bool {
    let AutoReapContext {
        work_db,
        live_states,
        hold_registry: _,
        coordinator,
        dispatch_events,
        reaper,
        cube_client,
        terminal_inspector: _,
    } = ctx;
    let execution_id = &state.run_id;

    tracing::warn!(
        execution_id,
        work_item_id = %execution.work_item_id,
        slot_id = state.slot_id,
        session_name,
        progress_at,
        auto_reap_threshold_secs,
        "stale-worker sweep: two-hour token-verified auto-reap firing",
    );

    // 1. Recovery backup — capture uncommitted workspace work before any
    //    teardown makes the workspace eligible for re-lease/reset.
    let recovery_patch = boss_engine_recovery::recovery_backup::backup_dead_execution(execution);

    // 2. Orphan + audit.
    let reason = format!(
        "stale-worker-auto-reap: tmux session {session_name} confirmed live and idle for more than \
         {auto_reap_threshold_secs}s (last driver event: {progress_at}); worker force-reaped after a \
         token-verified recheck"
    );
    if let Err(err) = work_db.mark_execution_orphaned(execution_id, &reason) {
        tracing::warn!(
            execution_id,
            ?err,
            "stale-worker sweep: auto-reap failed to mark execution orphaned; aborting the reap",
        );
        return false;
    }
    resolve_stale_worker_attention_for_work_item(work_db, &execution.work_item_id);
    if let Some(work_item_id) = &state.work_item_id
        && let Err(err) = crate::reconcile_audit::append_reconcile_audit(
            work_db,
            work_item_id,
            now_epoch_secs,
            &format!(
                "stale worker (exec {execution_id}) auto-reaped — tmux session {session_name} was confirmed \
                 live but idle for more than {auto_reap_threshold_secs}s (last driver event: {progress_at}); \
                 chore reset to todo for redispatch"
            ),
            recovery_patch.as_deref(),
        )
    {
        tracing::warn!(
            work_item_id,
            ?err,
            "stale-worker sweep: auto-reap failed to append audit line to description (non-fatal)",
        );
    }

    // 3. Driver teardown.
    crate::driver_teardown::teardown_driver_workspace(
        work_db,
        execution_id,
        execution.workspace_path.as_deref().map(std::path::Path::new),
        crate::driver_teardown::TeardownReason::StaleWorkerReconcile,
    )
    .await;

    // 4. Token-verified tmux reap. `attempt_auto_reap` already re-verified
    //    live identity — including the spawn token, checked as part of the
    //    tmux probe's own dead/alive classification — immediately before
    //    this call.
    reaper.reap_worker(execution_id).await;

    // 5. Release engine bookkeeping immediately after pane teardown. The
    // reaper normally does this itself; this guarded shared cleanup covers
    // test and fallback reapers without releasing a newly re-claimed slot.
    crate::dead_pid_sweep::release_reaped_execution(live_states, &coordinator, state).await;

    // 6. Lease release comes last: it is a remote RPC and must not leave an
    // await between the pane reaper's release and this guarded cleanup.
    if let Some(lease_id) = execution.cube_lease_id.as_deref() {
        crate::execution_liveness::force_release_lease_best_effort(
            execution_id,
            lease_id,
            cube_client.force_release_lease(lease_id, Some("stale-worker auto-reap: two-hour idle tmux worker")),
        )
        .await;
    }

    dispatch_events
        .emit(
            DispatchEvent::new(Stage::StaleWorkerReconcile, Outcome::Ok, execution_id)
                .with_work_item(&execution.work_item_id)
                .with_details(serde_json::json!({
                    "slot_id": state.slot_id,
                    "auto_reap": true,
                    "session_name": session_name,
                    "progress_at": progress_at,
                    "auto_reap_threshold_secs": auto_reap_threshold_secs,
                    "recovery_patch": recovery_patch
                        .as_deref()
                        .map(|p| p.display().to_string()),
                })),
        )
        .await;
    true
}

/// The conservative cadence-only fallback for a run with no tmux identity.
/// This population (no terminal inspector configured, or a run predating
/// tmux columns) already self-heals correctly; the tmux-evidence path
/// above is this module's classifier.
async fn run_cadence_fallback(
    ctx: CadenceFallbackContext<'_>,
    clocks: SweepClocks,
    state: &boss_protocol::LiveWorkerState,
    execution: &boss_protocol::WorkExecution,
    outcome: &mut StaleWorkerSweepOutcome,
) {
    let CadenceFallbackContext {
        work_db,
        live_states,
        coordinator,
        dispatch_events,
        reaper,
    } = ctx;
    let SweepClocks {
        grace_cutoff,
        now_epoch_secs,
        stale_threshold_secs,
    } = clocks;
    let Some(effective_threshold_secs) = live_states
        .progress_fidelity_for_slot(state.slot_id)
        .stale_threshold_secs(stale_threshold_secs)
    else {
        outcome.fidelity_exempt_skipped += 1;
        return;
    };

    // A tool in flight means the worker is legitimately busy — most
    // importantly a long foreground `bazel build`/`bazel test`,
    // which can run for many minutes with no intervening hook.
    // Reaping that would break real work; skip it and let the
    // pre-push gate's `timeout` guidance bound the wedged-tool case.
    if state.current_tool.is_some() {
        outcome.tool_in_flight_skipped += 1;
        return;
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
        return;
    };

    // Newer than the threshold ⇒ healthy.
    if last_event_at >= stale_cutoff.as_str() {
        outcome.fresh_skipped += 1;
        return;
    }

    let execution_id = &state.run_id;

    // Grace-period guard: skip executions whose `started_at` is
    // within STALE_GRACE_SECS or not yet recorded.
    let Some(started_epoch) = execution.started_epoch() else {
        outcome.grace_skipped += 1;
        return;
    };
    if started_epoch >= grace_cutoff {
        outcome.grace_skipped += 1;
        return;
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
        return;
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
        return;
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
    let recovery_patch = boss_engine_recovery::recovery_backup::backup_dead_execution(execution);

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

#[cfg(test)]
#[path = "stale_worker_sweep_tests.rs"]
mod tests;
