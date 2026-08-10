//! `bossctl` — the Boss-only CLI used by the coordinator session
//! running inside the Boss libghostty pane.
//!
//! Two-CLI design (see `tools/boss/docs/designs/main.md`):
//! - `boss` is the user-facing CLI for the work taxonomy
//!   (products / projects / tasks / chores).
//! - `bossctl` is the Boss-only CLI for control verbs
//!   (agents, probe, work start/cancel aliases, workspace summary).
//!
//! Verbs that map cleanly to existing engine RPCs are wired through;
//! verbs that need engine-side surfaces we have not built yet still
//! print a structured "not_implemented" response so the Boss session
//! can call them and see which ones are pending.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use boss_engine::work::WorkDb;

use anyhow::{Context, Result, bail};
use boss_client::{BossClient, Discovery};

mod agents;
mod comments;
mod dispatch_stats;
mod doctor;
mod hosts;
mod logs;
mod pause;
mod probe;
mod review;
mod selected_product;
mod stream_integrity;
use boss_engine::dispatch_events::DispatchEvent;
use boss_engine::dispatch_reader;
use boss_protocol::{
    DispatchAdmissionEntryPoint, FrontendEvent, FrontendRequest, HostedPaneState, HostedPaneStatus,
    LiveStatusDebugReport, LiveStatusSlotDebug, LiveWorkerState, MetricLiveEntry, ProposalKind, ProposalState, ROSTER,
    RequestExecutionInput, WorkExecution, WorkItem, WorkRun, WorkerProposal, WorkspacePoolEntry,
};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "bossctl",
    version,
    about = "Boss-only control CLI for the Boss V2 engine",
    long_about = "bossctl drives the Boss V2 engine on behalf of the coordinator session. \
                  Worker sessions do not have access to bossctl — its presence on PATH \
                  is part of how the engine distinguishes Boss-tier requests from worker traffic."
)]
struct Cli {
    /// Override the engine socket path (defaults to `BOSS_SOCKET_PATH`
    /// or the engine's standard path).
    #[arg(long, global = true)]
    socket_path: Option<String>,

    /// Emit machine-readable JSON output where supported.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Inspect and steer worker sessions.
    Agents {
        #[command(subcommand)]
        action: AgentsAction,
    },
    /// Inject a probe prompt into a worker, delivered at the earliest
    /// opportunity its pane offers. A parked worker (idle between turns, or
    /// sitting at its prompt after a Stop that followed a
    /// notification/permission prompt) takes the text immediately. A worker
    /// that is mid-task also takes it immediately, buffered in its agent's
    /// composer the same way text typed into the pane by hand would be — so
    /// a probe can steer a worker in the middle of a long autonomous run
    /// rather than reaching it as it exits. Only a worker whose driver reads
    /// no mid-turn input at all waits for a boundary.
    ///
    /// The engine checks that the probe can actually be delivered before
    /// accepting it, and prints the boundary it committed to. If it cannot
    /// deliver at all — no live pane, or a terminal worker — this exits
    /// non-zero rather than reporting a queued probe that would never arrive.
    Probe {
        /// Worker reference: run id, slot id, or crew name (e.g.
        /// `Riker`). Crew names resolve only over currently-live
        /// slots; case-insensitive.
        agent: String,
        /// Probe text the worker will see as its next prompt.
        text: String,
        /// Jump the queue: deliver this probe before any probe already
        /// waiting for the same worker. This is priority only — it does not
        /// change *where* the probe is delivered, since the engine already
        /// picks the earliest boundary the worker's pane allows.
        #[arg(long)]
        urgent: bool,
    },
    /// Report the delivery state of a previously accepted probe, by the
    /// `probe_id` that `bossctl probe` printed.
    ///
    /// States: `queued` (waiting for its boundary), `injected` (written,
    /// awaiting confirmation), `consumed` (the worker's CLI took it as a
    /// prompt), `buffered` (written into a mid-turn agent's composer; it
    /// submits at the end of the turn), `unconfirmed` (written but unproven;
    /// also warns on stderr), `replied` (the worker answered). Any state the
    /// engine can report exits 0 — read `delivered` (or `state=`) for the
    /// delivery judgement; a non-zero exit means the id could not be read.
    /// Probe ids live in the running engine process and are not retained
    /// across a restart.
    ProbeStatus {
        /// Probe id from `bossctl probe`, e.g. `probe-4`.
        probe_id: String,
    },
    /// Work-item dispatch aliases for symmetry with `boss`.
    Work {
        #[command(subcommand)]
        action: WorkAction,
    },
    /// Inspect cube workspaces and their current leases.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Automated-review control verbs.
    Review {
        #[command(subcommand)]
        action: review::ReviewAction,
    },
    /// Diagnose the live-status pipeline (engine build SHA, API key
    /// presence, per-slot trigger/outcome/transcript-path detail).
    /// Read-only; no side effects on the engine.
    LiveStatus {
        #[command(subcommand)]
        action: LiveStatusAction,
    },
    /// Run local runtime diagnostics that do not require an engine connection.
    Doctor {
        #[command(subcommand)]
        action: DoctorAction,
    },
    /// Pause one or more Boss systems in a single call. Defaults to every
    /// pausable system when no SYSTEMS are given — `bossctl pause` alone
    /// pauses everything the systems registry currently knows about, so
    /// a future pausable subsystem joins the default set automatically
    /// instead of requiring a hardcoded update here. This is a thin
    /// wrapper over the same per-system pause logic `dispatch pause` /
    /// `automation pause` already use — those verbs are unchanged and
    /// remain the way to pause exactly one system.
    Pause {
        /// Systems to pause: `dispatch`, `automation`, or `state` (prints
        /// pause status instead of pausing — equivalent to `bossctl
        /// state`; cannot be combined with other systems). Omit to pause
        /// every system.
        systems: Vec<PauseArg>,
        /// Why these systems are being paused. Applied identically to
        /// every system this call pauses — one reason per invocation, not
        /// per system. Required to actually pause (fails with a
        /// non-zero exit if omitted); not needed when the only argument
        /// is `state`, since that verb only prints status.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Resume one or more Boss systems in a single call. Defaults to
    /// every pausable system when no SYSTEMS are given. Symmetric with
    /// `pause` — see its help for the systems registry and the
    /// relationship to `dispatch resume` / `automation resume`.
    Resume {
        /// Systems to resume. Omit to resume every system.
        systems: Vec<PauseSystem>,
    },
    /// Show pause status for every system (dispatch, automation, ...) in
    /// one view, including `paused_since` for whichever are paused.
    /// Equivalent to `bossctl pause state`.
    State,
    /// Inspect the dispatch-pipeline event stream (file-scan only —
    /// works when the engine is wedged), and pause/resume/inspect
    /// dispatch specifically via its `pause`/`resume`/`state` subcommands
    /// (see top-level `bossctl pause`/`resume`/`state` to act on every
    /// system at once instead of dispatch alone).
    Dispatch {
        #[command(subcommand)]
        action: DispatchAction,
    },
    /// Pause, resume, and inspect automation-originated activity —
    /// independent of `dispatch pause`/`resume`/`state`. See
    /// [`AutomationAction`] for the exact scope each verb holds.
    Automation {
        #[command(subcommand)]
        action: AutomationAction,
    },
    /// Query and manage engine counter / gauge metrics.
    ///
    /// `list` and `show` read `state.db` directly — they work even
    /// when the engine is wedged (values may be up to 30s stale due
    /// to the flush window). `show --live` bypasses the stale window
    /// by reading in-memory atomics via engine RPC. `reset` always
    /// goes through engine RPC so the in-memory atomic and the
    /// database row are cleared in lockstep.
    Metrics {
        #[command(subcommand)]
        action: MetricsAction,
    },
    /// Register and manage remote SSH hosts in the Boss host registry.
    ///
    /// All subcommands read or write `state.db` directly — they work
    /// even when the engine is not running. The `local` host is
    /// auto-registered at engine first start with capabilities
    /// discovered from the local machine; a remote host discovers its
    /// capabilities over SSH when `hosts add` provisions it.
    Hosts {
        #[command(subcommand)]
        action: HostsAction,
    },
    /// Cancel never-started executions, or prune terminal ones (retention).
    ///
    /// `cancel` talks to the running engine and marks `queued` /
    /// `ready` / `waiting_dependency` rows `cancelled` so dispatch will
    /// not spawn workers for moot work. `prune` is retention cleanup of
    /// already-terminal rows and reads/writes `state.db` directly
    /// (scoped to this install's state root — `--state-root`,
    /// `BOSS_DB_PATH`, or `$HOME/Library/Application Support/Boss`).
    Executions {
        #[command(subcommand)]
        action: ExecutionsAction,
    },
    /// Reclaim Boss-owned per-run Codex homes past retention policy.
    ///
    /// Operates only on roots recorded in
    /// `work_executions.driver_runtime_state` — never scans `~/.codex` or
    /// deletes a rollout because its cwd is under a cube workspace. Live
    /// (non-terminal) executions are never touched. A running engine already
    /// runs this on a background sweep (`codex_home_retention_sweep`); this
    /// verb is for on-demand cleanup between sweeps or while the engine is
    /// stopped.
    CodexHomes {
        #[command(subcommand)]
        action: CodexHomesAction,
    },
    /// Reclaim Boss-owned per-run Grok home containers past retention
    /// policy. Mirrors `codex-homes` — see that verb's doc for the shared
    /// shape (only recorded roots are candidates, live executions are
    /// never touched, a running engine already sweeps on its own
    /// schedule and this verb is for on-demand cleanup between sweeps or
    /// while the engine is stopped).
    ///
    /// A Grok "home" is a run **container** holding both `grok-home/`
    /// (`GROK_HOME`) and `process-home/` (the scoped worker `HOME`); a
    /// reclaim removes the whole container. The credential symlink under
    /// `grok-home/auth.json` is removed as a directory entry, never
    /// followed to its target.
    GrokHomes {
        #[command(subcommand)]
        action: GrokHomesAction,
    },
    /// Inspect and reclaim stored screenshot evidence (`boss attach`).
    ///
    /// `list` reads `state.db` directly (same resolution as
    /// `metrics`/`hosts`), so it works even when the engine is wedged and is
    /// the way to find what is stored when the evidence HTTP surface is not
    /// running. `sweep` is on-demand retention: age window plus a
    /// total-bytes backstop, mirroring `codex-homes`/`grok-homes`. A running
    /// engine already sweeps on its own schedule; this verb is for cleanup
    /// between sweeps or while the engine is stopped.
    ///
    /// Reclaiming deletes the image bytes and leaves the row as a tombstone,
    /// so an evidence link in an already-merged PR body explains itself
    /// rather than 404ing. Blobs shared by several rows (identical renders
    /// are stored once) survive until the last row referencing them is
    /// reclaimed.
    Attachments {
        #[command(subcommand)]
        action: AttachmentsAction,
    },
    /// Read-only inspection of `work_comments` and `answer_agent_runs` rows.
    ///
    /// Reads `state.db` directly (same resolution as `metrics`/`hosts`) —
    /// works even when the engine is wedged. Exists so diagnosing a stuck
    /// comment thread or a missing answer-agent reply doesn't require raw
    /// `sqlite3` against `state.db`.
    Comments {
        #[command(subcommand)]
        action: comments::CommentsAction,
    },
    /// Print the product the Boss UI's product chooser is currently set
    /// to — the product a short ID (`T<n>`) should be resolved against,
    /// and the value to pass to `boss --product`.
    ///
    /// Read-only in both directions: it does not change the selection and
    /// cannot be used to drive the UI. It also never guesses. When the
    /// answer is unavailable this exits non-zero and says which case it
    /// hit — the app is not connected, nothing is selected, or the
    /// selected product no longer exists — instead of falling back to a
    /// default or first product. That fallback is the bug this verb
    /// exists to remove: short IDs are scoped per product, so resolving
    /// one against the wrong product succeeds and returns a real row for
    /// the wrong work item.
    ///
    /// `--json` emits `{"status": ...}` — `selected` (with `product_id`,
    /// `name`, `slug`, `reported_at`), `app_not_connected`,
    /// `no_selection`, or `product_unknown` — so a caller can branch on
    /// the case rather than parse a message.
    SelectedProduct,
    /// Scroll the kanban in the macOS app to a work item's card and
    /// play a short transient highlight. Accepts a short id (`T607`)
    /// or a canonical id. Returns an error when the app is not
    /// running, the item is deleted, or the id is unknown.
    Reveal {
        /// Work item to reveal: short id (`T607`) or canonical id.
        id: String,
    },
    /// Open a markdown file in the Boss UI, the same as using the app's
    /// File ▸ Open. Relative paths are resolved against this process's
    /// current directory before being sent to the engine — the engine
    /// and the app run with different working directories, so a bare
    /// relative path would be ambiguous by the time it reached either
    /// of them. Re-opening a path that already has a window open
    /// focuses that window instead of opening a duplicate. Returns a
    /// non-zero exit with an actionable message when the Boss app is
    /// not running (no app session registered) — this verb never
    /// silently no-ops.
    Open {
        /// Path to the markdown file to open. May be relative to the
        /// current directory or absolute.
        path: String,
    },
    /// Read engine diagnostic logs. Works file-scan-only — no running engine
    /// required. Resolves log paths automatically from the Boss state root.
    ///
    /// Spans rotated and day-dated files transparently as one chronological
    /// stream. Default behaviour is still a short tail; use `--since` /
    /// field filters / a larger `--tail` (or `--tail 0` for unlimited) for
    /// incident queries. When the result is capped, a truncation notice is
    /// printed to stderr so a short answer is never mistaken for absence.
    /// Global `--json` emits original JSONL records one per line (no wrapper)
    /// so `jq` pipelines keep working; notices stay on stderr.
    ///
    /// Sources:
    /// - `engine` (default) — `engine-trace.jsonl` + rotated segments
    /// - `audit` — `engine-audit.log` (+ rotated, if any)
    /// - `dispatch` — `dispatch-events/current.jsonl`
    /// - `spawn` — `diagnostics/spawn-YYYY-MM-DD.jsonl`
    /// - `population-timing` — `diagnostics/population-timing-*.jsonl` (app)
    ///   and `diagnostics/engine-population-timing-*.jsonl` (engine)
    Logs {
        /// Which log / diagnostic stream to read.
        #[arg(value_enum, default_value_t = LogSource::Engine)]
        source: LogSource,
        /// Print the last N matching lines (default 50). `0` means unlimited.
        #[arg(short = 'n', long = "tail", default_value_t = 50)]
        tail: usize,
        /// Stream appended lines live, like `tail -f`. Polls every 250 ms;
        /// press Ctrl-C to stop. Initial output still honours `--tail` and
        /// filters; the live stream keeps field/grep filters but ignores
        /// `--since`/`--until`.
        #[arg(short = 'f', long)]
        follow: bool,
        /// Raw case-sensitive substring match on the whole line. Prefer
        /// `--target` / `--level` / `--field` / `--execution-id` for
        /// structured JSONL — substring grep false-positives on message
        /// bodies (e.g. a module name appearing in an unrelated event).
        #[arg(long)]
        grep: Option<String>,
        /// Only records at or after this time. Accepts relative offsets
        /// (`30m`, `6h`, `2d`, `90s`), RFC3339 (`2026-07-26T06:20:00Z`),
        /// a date (`2026-07-26` = start of that UTC day), or an epoch integer
        /// (seconds or ms).
        #[arg(long)]
        since: Option<String>,
        /// Only records at or before this time. Same formats as `--since`.
        /// A bare date (`2026-07-26`) is end-of-day UTC so that whole calendar
        /// day is included (exclusive next midnight).
        #[arg(long)]
        until: Option<String>,
        /// Match the JSON `target` field (tracing module path). Exact match
        /// or module-prefix (`boss_engine::app` matches
        /// `boss_engine::app::server`). Does **not** search message bodies.
        #[arg(long)]
        target: Option<String>,
        /// Match the JSON `level` field case-insensitively (`info`, `ERROR`).
        #[arg(long)]
        level: Option<String>,
        /// Match a structured field `key=value` (repeatable). Checks the
        /// top-level key, then a nested `fields` object. All `--field`
        /// constraints are ANDed.
        #[arg(long = "field", value_name = "KEY=VALUE")]
        fields: Vec<String>,
        /// Match `execution_id` or `run_id` (top-level or under `fields`).
        /// Convenience for the common "what happened to this execution?"
        /// query across trace / dispatch / spawn sources.
        #[arg(long)]
        execution_id: Option<String>,
        /// Override the Boss state root (defaults to
        /// `$HOME/Library/Application Support/Boss`).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum LiveStatusAction {
    /// One-shot snapshot of the live-status pipeline.
    Debug,
}

#[derive(Subcommand, Debug)]
enum DispatchAction {
    /// Print recent dispatch events from `current.jsonl`. Filterable
    /// by stage / outcome. Defaults to the last 50 events.
    Tail {
        /// Override the Boss state root (defaults to
        /// `$HOME/Library/Application Support/Boss`).
        #[arg(long)]
        state_root: Option<PathBuf>,
        /// Maximum number of events to print (most recent first).
        #[arg(short = 'n', long = "n", default_value_t = 50)]
        n: usize,
        /// Restrict to events matching this `stage` value (e.g.
        /// `pane_spawned`).
        #[arg(long)]
        stage: Option<String>,
        /// Restrict to events matching this `outcome` value (`ok`,
        /// `error`, `skipped`).
        #[arg(long)]
        outcome: Option<String>,
    },
    /// Print the dispatch timeline for one execution or work item and
    /// match known failure signatures (stall, zombie redundant_spawn,
    /// spawn_ack_timeout, lease timeout, untracked-lease storm, SlotBusy,
    /// queue-side CI, …) with evidence lines and known recovery.
    ///
    /// Accepts an execution id (`exec_…`) or a work-item id (`task_…`,
    /// `chore_…`, …). File-scan only over dispatch JSONL + engine-trace —
    /// works when the engine is wedged. Signature catalog:
    /// `tools/boss/docs/investigations/bossctl-doctor-failure-signatures-*.md`.
    Diagnose {
        /// Execution id (`exec_…`) or work-item id.
        id: String,
        /// Override the Boss state root.
        #[arg(long)]
        state_root: Option<PathBuf>,
        /// Skip the raw per-event timeline; print signature findings only.
        #[arg(long)]
        signatures_only: bool,
    },
    /// List executions whose dispatch timeline started but never
    /// reached a terminal stage (`pane_spawned ok` or any error).
    /// Useful when the engine logs a successful dispatch but no
    /// worker pane ever appeared in the Doing column.
    GhostActive {
        /// Override the Boss state root.
        #[arg(long)]
        state_root: Option<PathBuf>,
        /// Only include entries whose last event is older than this
        /// many seconds (matches the writer-side `stage_stalled`
        /// threshold). 0 means "list every non-terminal timeline".
        #[arg(long, default_value_t = 60)]
        stalled_after_secs: u64,
        /// When set, restrict the output to entries the reader
        /// considers `stalled` (last event older than
        /// `--stalled-after-secs`).
        #[arg(long)]
        include_stalled: bool,
    },
    /// Pause global dispatch. The engine stops dispatching new executions
    /// from all sources (auto-dispatch, reconciliation, dependency-gate-clear,
    /// manual start). Already-running executions are not interrupted. The
    /// paused state persists across engine restarts. Idempotent — pausing
    /// while already paused is a no-op.
    ///
    /// Independent of `bossctl automation pause`/`resume`: this already
    /// holds automation-pool executions from claiming a slot (they are not
    /// exempt, same as main-pool rows) but does NOT stop the automation
    /// scheduler from creating new triage passes or a running triage worker
    /// from recording a produced task — those keep queueing and drain once
    /// dispatch resumes. It never sets, clears, or implies `automation
    /// pause`/`resume`.
    ///
    /// Equivalent to `bossctl pause dispatch`. To also stop automation
    /// (the scope operators usually want), use `bossctl pause` (defaults
    /// to every system) or `bossctl pause dispatch automation`.
    Pause {
        /// Why dispatch is being paused. Required — a pause with no
        /// stated reason is worse than no reason field at all, since a
        /// reader downstream cannot tell a fabricated value from a real
        /// one.
        #[arg(long)]
        reason: String,
    },
    /// Resume global dispatch. The engine immediately drains any executions
    /// that queued while paused and resumes normal dispatch. Idempotent —
    /// resuming while already running is a no-op. Does not affect the
    /// independent automation-pause flag — see `Pause` above.
    Resume,
    /// Show the current dispatch-pause state (paused/running and, if paused,
    /// when it was paused).
    State {
        /// Instead of the live engine RPC state, print recent pause/resume
        /// episodes (operator- and breaker-originated) with their full audit
        /// evidence — file-scan only over `dispatch-events/current.jsonl`,
        /// so it works even when the engine is wedged.
        #[arg(long)]
        history: bool,
        /// Maximum number of episodes to print (most recent first). Only
        /// meaningful with `--history`.
        #[arg(short = 'n', long = "n", default_value_t = 10)]
        n: usize,
        /// Override the Boss state root. Only meaningful with `--history`.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Aggregate how long ready work items wait for a worker slot,
    /// broken down by the defer reason that finally cleared
    /// (`chain_serialized`, `pool_exhausted`, ...), plus the current
    /// top blocked items with their reason and wait so far. Read-only
    /// over `dispatch-events/current.jsonl` — no engine RPC, no
    /// change to dispatch behavior.
    Stats {
        /// Override the Boss state root.
        #[arg(long)]
        state_root: Option<PathBuf>,
        /// Only consider events at or after this relative duration ago
        /// (e.g. `30m`, `6h`, `2d`). Defaults to all recorded events.
        #[arg(long)]
        since: Option<String>,
        /// Maximum number of currently-blocked items to print.
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
    /// Get or set the interactive-pool ("Bridge Crew" + "Lower Decks")
    /// concurrency cap — the ceiling `drain_ready_queue` enforces on live
    /// main-pool workers separately from the underlying 16-slot worker
    /// pool. Raising it lets dispatch spill into Lower Decks (slots 9-16)
    /// instead of holding rows once 8 are live; lowering it takes effect
    /// on the very next drain pass. Persisted to `state.db`, so a change
    /// survives an engine restart. Does not affect the review pool, which
    /// always dispatches from its own pool. With no `--set`, prints the
    /// current cap.
    Concurrency {
        /// Set the cap to this value instead of just printing it. Must be
        /// at least 1; values above the 16-slot worker-pool ceiling are
        /// clamped down to it. Raising the cap immediately kicks the
        /// scheduler so newly-available capacity is used right away.
        #[arg(long)]
        set: Option<usize>,
    },
}

#[derive(Subcommand, Debug)]
enum AutomationAction {
    /// Pause automation-originated activity. The engine stops starting new
    /// automation triage passes (both the scheduler's own fires and `boss
    /// automation run`'s manual fire), and stops claiming worker slots for
    /// executions bound for the automation pool — both fresh triage
    /// executions and tasks a triage worker produces. Already-running
    /// automation workers are not interrupted; they finish normally,
    /// including recording whatever task their decision produces. The
    /// paused state persists across engine restarts. Idempotent — pausing
    /// while already paused is a no-op.
    ///
    /// Independent of `bossctl dispatch pause`/`resume`: a dispatch pause
    /// already holds automation-pool *spawns* but leaves the automation
    /// scheduler free to keep creating (queueing) new triage executions.
    /// This verb additionally stops those triage passes from starting in
    /// the first place — the tighter gate you want when the goal is
    /// curbing runaway automation-produced work items, not just throttling
    /// dispatch. It never sets, clears, or implies `dispatch pause`/`resume`.
    ///
    /// Equivalent to `bossctl pause automation`. To also stop dispatch,
    /// use `bossctl pause` (defaults to every system) or `bossctl pause
    /// dispatch automation`.
    Pause {
        /// Why automation is being paused. Required — see `dispatch
        /// pause`'s `--reason` doc for why omitting it is not allowed.
        #[arg(long)]
        reason: String,
    },
    /// Resume automation-originated activity. The engine immediately
    /// drains any automation-pool executions that queued while paused and
    /// resumes normal triage scheduling. Idempotent — resuming while
    /// already running is a no-op. Does not affect the independent
    /// dispatch-pause flag — see `Pause` above.
    Resume,
    /// Show the current automation-pause state (paused/running and, if
    /// paused, when it was paused).
    State,
}

/// The registry of pausable Boss systems. This is the single source of
/// truth for what `bossctl pause`/`bossctl resume` with no arguments
/// means ("every system") — it is a Rust enum rather than a hardcoded
/// pair of dispatch/automation booleans specifically so that adding a
/// variant here is the only change needed for a new subsystem (e.g. a
/// per-poller pause) to join the default "pause everything" set; nothing
/// else derives its own list.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum PauseSystem {
    /// Worker dispatch — see `bossctl dispatch pause`.
    Dispatch,
    /// Automation triage — see `bossctl automation pause`.
    Automation,
}

impl PauseSystem {
    /// Every system in the registry — the default scope of `bossctl
    /// pause`/`bossctl resume` when no SYSTEMS are given.
    fn all() -> Vec<PauseSystem> {
        <PauseSystem as clap::ValueEnum>::value_variants().to_vec()
    }
}

/// Positional value accepted by `bossctl pause`: any [`PauseSystem`],
/// plus `state` as a discoverable alias for `bossctl state`.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum PauseArg {
    Dispatch,
    Automation,
    /// Alias for `bossctl state` — show pause status instead of pausing.
    State,
}

impl PauseArg {
    fn as_system(self) -> Option<PauseSystem> {
        match self {
            PauseArg::Dispatch => Some(PauseSystem::Dispatch),
            PauseArg::Automation => Some(PauseSystem::Automation),
            PauseArg::State => None,
        }
    }
}

/// Output format for `bossctl agents transcript --format`.
#[derive(clap::ValueEnum, Debug, Clone, PartialEq)]
enum TranscriptFormat {
    /// Plain-text summary (default).
    Text,
    /// Raw JSONL lines as emitted by Claude Code.
    Jsonl,
    /// Converted markdown via the engine's transcript renderer.
    Markdown,
}

impl std::fmt::Display for TranscriptFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranscriptFormat::Text => write!(f, "text"),
            TranscriptFormat::Jsonl => write!(f, "jsonl"),
            TranscriptFormat::Markdown => write!(f, "markdown"),
        }
    }
}

/// Which engine log / diagnostic stream `bossctl logs` should read.
#[derive(clap::ValueEnum, Debug, Clone, PartialEq)]
pub(crate) enum LogSource {
    /// `engine-trace.jsonl` — structured tracing events (primary log).
    Engine,
    /// `engine-audit.log` — lifecycle events (start, socket bind, shutdown).
    Audit,
    /// `dispatch-events/current.jsonl` — dispatch pipeline stage events.
    Dispatch,
    /// `diagnostics/spawn-YYYY-MM-DD.jsonl` — worker-spawn diagnostics.
    Spawn,
    /// App + engine population-timing day files under `diagnostics/`
    /// (`population-timing-*.jsonl` and `engine-population-timing-*.jsonl`).
    #[value(name = "population-timing")]
    PopulationTiming,
}

impl std::fmt::Display for LogSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogSource::Engine => write!(f, "engine"),
            LogSource::Audit => write!(f, "audit"),
            LogSource::Dispatch => write!(f, "dispatch"),
            LogSource::Spawn => write!(f, "spawn"),
            LogSource::PopulationTiming => write!(f, "population-timing"),
        }
    }
}

#[derive(Subcommand, Debug)]
enum AgentsAction {
    /// List worker sessions and their current state.
    List {
        /// Also include every pane the app hosts that isn't a plain
        /// live-tracked worker: a slot whose live registry entry is
        /// gone but durable state still corroborates a running process
        /// (the shape a worker the engine lost track of takes — crash,
        /// terminal-fail path bug, spawn-ack timeout), and true "husk"
        /// panes (no live process either). Invisible on the default
        /// view; this is what `retire-pane` targets, and what causes a
        /// `SlotBusy` dispatch rejection.
        #[arg(long)]
        all: bool,
    },
    /// Show detailed status for a single worker. Falls back to the
    /// historical run record if the reference is a run id that is no
    /// longer live.
    Status {
        /// Worker reference: run id, slot id, or crew name (e.g.
        /// `Riker`; case-insensitive). Resolved first against the
        /// live-worker registry, then against the engine's durable
        /// pane state, so a name still resolves after the engine drops
        /// its live registry entry (crash, terminal-fail path,
        /// spawn-ack timeout).
        agent: String,
    },
    /// Bring a worker pane to the front.
    Focus {
        /// Worker reference: run id, slot id, or crew name.
        agent: String,
    },
    /// Send text to a worker as if user-typed.
    Send {
        /// Worker reference: run id, slot id, or crew name.
        agent: String,
        text: String,
    },
    /// Interrupt a worker (Esc-equivalent).
    Interrupt {
        /// Worker reference: run id, slot id, or crew name.
        agent: String,
    },
    /// Launch a worker session for a given work item without going
    /// through the coordinator's auto-dispatch path.
    Launch {
        work_item_id: String,
        #[arg(long)]
        preferred_workspace_id: Option<String>,
    },
    /// Stop a worker session and release its lease.
    Stop {
        /// Worker reference: run id, slot id, or crew name.
        agent: String,
    },
    /// Place an explicit hold on a live worker, exempting it from the
    /// idle-park and auto-reap sweeps until released (`agents
    /// release-hold`) or the run ends. Does NOT protect the run from
    /// `agents stop`/`agents reap` — those break-glass verbs still work
    /// on a held worker. Use this to protect a worker you know is
    /// legitimately waiting on something the automated checks haven't
    /// been taught to recognize (e.g. debugging it by hand).
    Hold {
        /// Worker reference: run id, slot id, or crew name.
        agent: String,
        /// Free-text note explaining why the worker is held. Surfaced on
        /// `agents list`/`agents status`.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Release a hold placed by `agents hold`, restoring the normal
    /// idle-park/auto-reap sweep behavior. Idempotent — releasing a
    /// worker with no hold in place succeeds as a no-op.
    ReleaseHold {
        /// Worker reference: run id, slot id, or crew name.
        agent: String,
    },
    /// Print the transcript of a worker's conversation.
    ///
    /// Works for both live workers and terminal/completed executions.
    /// For a completed execution, pass the execution id (`exec_*`) or
    /// run id (`run_*`) — the engine resolves the transcript path from
    /// the persistent `work_runs.transcript_path` record.
    ///
    /// By default the full transcript is returned (lines=0 means all
    /// lines). Pass `--lines N` to tail only the last N lines.
    Transcript {
        /// Worker reference: run id (`run_*`), execution id (`exec_*`),
        /// slot id, or crew name. Resolved first against the
        /// live-worker registry, then against the engine's durable
        /// pane state, so a name still resolves for a worker the
        /// engine has lost live-track of. For a completed execution
        /// with no trace in either, pass the execution id shown by
        /// `bossctl agents status <exec_id>`.
        agent: String,
        /// Number of lines to return from the end of the transcript.
        /// 0 (the default) returns the entire transcript.
        #[arg(long, default_value_t = 0)]
        lines: usize,
        /// Output format for the transcript.
        /// `text` renders a plain-text summary (default), `jsonl` prints
        /// raw JSONL lines, and `markdown` converts the transcript to
        /// formatted markdown via the engine's transcript converter.
        #[arg(long, value_enum, default_value_t = TranscriptFormat::Text)]
        format: TranscriptFormat,
        /// Hide tool_use and tool_result segments, showing only user/assistant
        /// turns. Applies to `text` and `markdown` formats; has no effect on
        /// `jsonl` (which always emits raw lines).
        #[arg(long, default_value_t = false)]
        no_tools: bool,
    },
    /// Mark an execution as `orphaned` (terminal) without releasing
    /// its cube workspace lease. Used to recover from a Boss app
    /// crash where the worker pane died but the engine still treats
    /// the run as live — the engine's startup probe misses these
    /// when the cube lease is still within its TTL.
    ///
    /// Accepts a run/execution id, slot id, or crew name — resolved
    /// the same way every other `agents` verb resolves a worker
    /// reference: first against the live-worker registry, then against
    /// the engine's durable pane state, so an orphaned worker with no
    /// live registry entry still resolves by name or slot. A reference
    /// that misses both but has the shape of a run/execution id is
    /// still forwarded raw, for a pane already torn down everywhere
    /// but its own DB row.
    Reap {
        /// Worker reference: run/execution id (e.g.
        /// `exec_18ad6336fedcb190_12`), slot id, or crew name. Look up
        /// a bare id with `bossctl workspace summary` or `boss chore
        /// show`.
        agent: String,
    },
    /// Show each worker pool's (main, automation, review) capacity,
    /// idle count, and every currently-claimed slot with its holding
    /// execution id and whether a live worker still backs it.
    ///
    /// A claim with `live=false` and a terminal `execution_status` has
    /// outlived its execution — either the periodic pool-claim
    /// reconciler hasn't gotten to it yet (claims past their grace
    /// period self-heal within ~60-120s) or the path that terminated
    /// the execution has a bug. This is the tool for diagnosing
    /// "pool reports N/M busy but `agents list` shows fewer live
    /// workers" without manually diffing `agents list` against
    /// `dispatch.jsonl` rejections.
    Pools,
    /// Break-glass: tear down a pane the engine has NO live-tracked run
    /// for — a worker pane the app still hosts that the engine has
    /// already forgotten (crash, terminal-fail path bug, spawn-ack
    /// timeout).
    ///
    /// Accepts a slot id, crew name, or run id — resolved the same way
    /// every other `agents` verb resolves a worker reference: first
    /// against the live-worker registry, then against the engine's
    /// durable pane state. A bare number is always accepted directly
    /// as a slot id with no lookup, so a slot invisible to both the
    /// live registry and the app can still be targeted by number.
    ///
    /// Refuses if the engine's own live-worker registry still shows a
    /// live (or terminal-but-contradicted-by-real-activity) run in the
    /// slot — that pane is not safe to tear down blind; use `agents
    /// stop` for it instead. When there is no live registry entry at
    /// all but durable state corroborates a still-running process for
    /// an inferred-terminal execution (the shape a worker the engine
    /// lost track of takes), this no longer refuses: it reaps the
    /// process the same way `agents stop` would, then completes the
    /// retirement. Use `agents list --all` to see every hosted pane's
    /// state before retiring one.
    RetirePane {
        /// Worker reference: slot id (1-indexed, matches the app's
        /// Workers grid numbering and `agents list --all` output),
        /// crew name, or run id.
        agent: String,
    },
}

#[derive(Subcommand, Debug)]
enum DoctorAction {
    /// Verify that the tmux required for durable worker sessions is installed,
    /// executable, and supports session environment variables.
    Tmux,
}

#[derive(Subcommand, Debug)]
enum WorkAction {
    /// Request the engine schedule a work item for execution.
    Start {
        work_item_id: String,
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        preferred_workspace_id: Option<String>,
        /// Bypass an active *operator*-originated global dispatch pause for
        /// this one request. Distinct from `bossctl agents launch`'s pool
        /// growth: this never grows a worker pool, never skips the
        /// interactive concurrency cap, unmet dependencies, or any other
        /// admission constraint, and a breaker-originated pause is never
        /// overridable. Refuses outright (with no queued residue left
        /// behind) when any non-overridable constraint blocks the item —
        /// see
        /// `docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md`.
        #[arg(long)]
        force: bool,
    },
    /// Cancel a queued or running execution (any non-terminal status).
    ///
    /// For never-started rows only, prefer `bossctl executions cancel`,
    /// which refuses live workers (pointing at `agents stop`) and
    /// records an operator reason in the audit trail.
    Cancel { execution_id: String },
    /// Full execution history for a work item — every `work_executions`
    /// row regardless of status, oldest first, with the host each one
    /// ran on. Reads `state.db` directly (same resolution as
    /// `metrics`/`hosts`), so it works even when the engine is wedged.
    /// Exec ids are ready to paste into `bossctl dispatch diagnose`.
    Executions {
        work_item_id: String,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Inspect the `worker_proposals` ledger.
    Proposals {
        #[command(subcommand)]
        action: ProposalsAction,
    },
}

#[derive(Subcommand, Debug)]
enum ProposalsAction {
    /// List `worker_proposals` rows, newest first, optionally filtered.
    /// Shows the full ledger — including `rejected`/`expired` history —
    /// per the design's §"UI visibility and provenance": the ledger is
    /// CLI-inspectable even though it gets no app-side UI surface. Reads
    /// `state.db` directly (same resolution as `metrics`/`hosts`), so it
    /// works even when the engine is wedged.
    List {
        /// Restrict to proposals filed against this execution.
        #[arg(long)]
        execution_id: Option<String>,
        /// Restrict to proposals filed against this work item (across all
        /// its executions).
        #[arg(long)]
        work_item_id: Option<String>,
        /// Restrict to one proposal kind (e.g. `followup_task`, `blocked`).
        #[arg(long)]
        kind: Option<String>,
        /// Restrict to one disposition (`proposed`, `applied`, `rejected`,
        /// `superseded`, `expired`).
        #[arg(long)]
        state: Option<String>,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum WorkspaceAction {
    /// List cube workspaces and their current leases. Workspaces are
    /// provisioned on demand — there is no fixed pool, and a fully-leased
    /// listing is not a capacity limit.
    Summary,
}

#[derive(Subcommand, Debug)]
enum MetricsAction {
    /// List all registered counters and gauges with current value and
    /// last-update time. Reads `state.db` directly — works even when
    /// the engine is wedged. Values may be up to 30s stale due to the
    /// flush interval.
    List {
        /// Filter to metrics whose name starts with this prefix
        /// (e.g. `pr_url_capture`).
        #[arg(long)]
        prefix: Option<String>,
        /// Override the Boss state-root directory (defaults to
        /// `$HOME/Library/Application Support/Boss` or `$BOSS_DB_PATH`).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Show one metric with its description, current value, and
    /// metadata. Reads `state.db` directly by default; pass `--live`
    /// to read the in-memory atomic via engine RPC (bypasses the 30s
    /// flush-staleness window).
    Show {
        /// The metric name (e.g.
        /// `pr_url_capture.primary_path.hit`).
        name: String,
        /// Read the in-memory atomic directly via engine RPC,
        /// bypassing flush-staleness. Requires a running engine.
        #[arg(long)]
        live: bool,
        /// Override the Boss state-root directory (ignored when
        /// `--live` is set).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// GitHub API usage attributed by subsystem: calls, quota units
    /// spent, and the implied hourly rate against the 5000/hour budget.
    ///
    /// Reads the per-call `github_api_calls` rows in `state.db` directly
    /// (same resolution as `metrics list`), which is what the `github_api.*`
    /// counters cannot answer on their own: a counter is a monotonic
    /// total, and the GitHub budget is a rate over a rolling hour.
    Github {
        /// Look back this many hours (default 24).
        #[arg(long, default_value_t = 24)]
        hours: u32,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Reset one or all metrics to zero (both in-memory and in
    /// `state.db`) via engine RPC. Counters are truly monotonic
    /// across the framework's lifetime unless reset explicitly; this
    /// is the only way to restart accumulation.
    Reset {
        /// Name of the metric to reset. Mutually exclusive with
        /// `--all`.
        name: Option<String>,
        /// Reset every registered counter and gauge to zero.
        #[arg(long, conflicts_with = "name")]
        all: bool,
    },
}

#[derive(Subcommand, Debug)]
enum HostsAction {
    /// Register a new remote host. The host row is persisted to
    /// `state.db`, then provisioned: push the `boss-remote-run` wrapper,
    /// verify `cube` is invocable over non-interactive SSH, and discover
    /// the host's capabilities (`os=`, `arch=`, `gh-authed=`) by probing
    /// it. The host is left enabled only if all of that succeeds;
    /// otherwise it is disabled with the reason on `last_error`.
    ///
    /// `--skip-wrapper-push` suppresses the whole provisioning step
    /// (offline / dry-run / test fixtures). A host registered that way is
    /// enabled but unverified and reports no discovered capabilities until
    /// something provisions it.
    Add {
        /// Unique identifier for this host (e.g. `zakalwe`).
        id: String,
        /// SSH target used to reach this host (alias or `user@host`).
        #[arg(long)]
        ssh_target: String,
        /// Number of concurrent worker slots on this host.
        #[arg(long, default_value_t = 1)]
        pool_size: i64,
        /// User-defined capability tags (e.g. `--tag os=macos --tag arch=arm64`).
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// Skip the eager wrapper push at registration. The host row
        /// is still created. Use when the host is offline at
        /// registration time; the lazy push at dispatch will catch up.
        #[arg(long)]
        skip_wrapper_push: bool,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// List all registered hosts with their enabled state and capability count.
    List {
        /// Only show enabled hosts.
        #[arg(long)]
        enabled: bool,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Show full details for a single host including all capabilities.
    Show {
        id: String,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Re-run remote provisioning and capability discovery for a host.
    Probe {
        id: String,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Add or remove user-defined capability tags on a host.
    Tag {
        #[command(subcommand)]
        action: HostsTagAction,
    },
    /// Enable a previously disabled host.
    Enable {
        id: String,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Disable a host so no new work is dispatched to it.
    Disable {
        id: String,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Remove a host from the registry. Fails for the built-in `local` host.
    Remove {
        id: String,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum HostsTagAction {
    /// Add one or more user capability tags to a host.
    Add {
        id: String,
        /// Capability tag(s) to add (e.g. `os=macos`, `bazel=7`).
        #[arg(required = true)]
        tags: Vec<String>,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Remove one or more user capability tags from a host.
    Remove {
        id: String,
        /// Capability tag(s) to remove.
        #[arg(required = true)]
        tags: Vec<String>,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum ExecutionsAction {
    /// Cancel a never-started (`queued` / `ready` / `waiting_dependency`)
    /// execution so dispatch will not spawn a worker for it.
    ///
    /// This is the verb for "this queued work is moot" — e.g. the work
    /// was finished by other means and a ready successor would only
    /// re-do it. Marks the row `cancelled` (distinct from `orphaned`,
    /// which means the engine lost a once-live run).
    ///
    /// Refuses executions that have already started. Stopping a live
    /// worker is a different operation: use `bossctl agents stop`.
    /// For any non-terminal row including running, use
    /// `bossctl work cancel`.
    ///
    /// Select either by execution id or by `--work-item` (cancels every
    /// never-started execution currently on that item). Optional
    /// `--reason` is recorded in the engine audit trail.
    Cancel {
        /// Execution id to cancel (e.g. `exec_…`). Mutually exclusive
        /// with `--work-item`.
        execution_id: Option<String>,
        /// Cancel every never-started execution for this work item
        /// (canonical id or short id as accepted by the engine).
        #[arg(long)]
        work_item: Option<String>,
        /// Operator reason recorded in `engine-audit.log` and the
        /// terminalization log line.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Delete terminal (`abandoned` / `failed` / `orphaned` / `cancelled`)
    /// `work_executions` rows past the retention bound. `completed`
    /// executions are never touched. Always keeps the most recent
    /// `--keep-per-work-item` eligible rows per work item regardless of
    /// age, so recent diagnostics survive.
    ///
    /// Does **not** cancel live or queued work — only deletes rows that
    /// are already terminal. Use `executions cancel` for moot ready rows.
    Prune {
        /// Only prune rows whose `created_at` is more than this many days
        /// old.
        #[arg(long, default_value_t = boss_engine::work::DEFAULT_RETENTION_MAX_AGE_SECS / (24 * 60 * 60))]
        older_than_days: i64,
        /// Always keep at least this many of the most recent eligible
        /// executions per work item, regardless of age.
        #[arg(long, default_value_t = boss_engine::work::DEFAULT_RETENTION_KEEP_PER_WORK_ITEM)]
        keep_per_work_item: u32,
        /// Preview what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum CodexHomesAction {
    /// Reclaim recorded Boss-owned `CODEX_HOME` trees past the retention
    /// policy (age, with a total-bytes backstop). Only roots stored on
    /// execution rows are candidates; live executions are never touched.
    Sweep {
        /// Only reclaim homes whose execution age anchor (`finished_at`,
        /// else `created_at`) is older than this many days.
        #[arg(long, default_value_t = boss_engine::codex_rollout_retention::DEFAULT_MAX_AGE_DAYS)]
        older_than_days: u64,
        /// Total-bytes backstop across retained terminal homes: once
        /// exceeded, the oldest are reclaimed first regardless of age.
        #[arg(long, default_value_t = boss_engine::codex_rollout_retention::DEFAULT_MAX_TOTAL_BYTES)]
        max_total_bytes: u64,
        /// Preview what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum GrokHomesAction {
    /// Reclaim recorded Boss-owned Grok run-container trees past the
    /// retention policy (age, with a total-bytes backstop). Only roots
    /// stored on execution rows are candidates; live executions are never
    /// touched.
    Sweep {
        /// Only reclaim homes whose execution age anchor (`finished_at`,
        /// else `created_at`) is older than this many days.
        #[arg(long, default_value_t = boss_engine::grok_home_retention::DEFAULT_MAX_AGE_DAYS)]
        older_than_days: u64,
        /// Total-bytes backstop across retained terminal homes: once
        /// exceeded, the oldest are reclaimed first regardless of age.
        #[arg(long, default_value_t = boss_engine::grok_home_retention::DEFAULT_MAX_TOTAL_BYTES)]
        max_total_bytes: u64,
        /// Preview what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum AttachmentsAction {
    /// List stored evidence, newest first, with the run that produced each
    /// image and whether retention has reclaimed its bytes.
    List {
        /// Only show attachments for one work item.
        #[arg(long)]
        work_item: Option<String>,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Reclaim stored evidence past the retention policy, and collect blobs
    /// no row references (what a crash between the store write and the row
    /// insert leaves behind). Live executions' evidence is never touched.
    Sweep {
        /// Only reclaim attachments created more than this many days ago.
        #[arg(long, default_value_t = boss_engine::attachments::retention::DEFAULT_MAX_AGE_DAYS)]
        older_than_days: u64,
        /// Total-bytes backstop across retained evidence: once exceeded, the
        /// oldest is reclaimed first regardless of age.
        #[arg(long, default_value_t = boss_engine::attachments::retention::DEFAULT_MAX_TOTAL_BYTES)]
        max_total_bytes: u64,
        /// Preview what would be reclaimed without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Override the Boss state-root directory.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

// Per-binary build-info stamp + `version_string` accessor. The
// include!(env!("BOSS_BUILD_INFO_RS")) must be evaluated in this crate
// (this rust_binary sets its own rustc_env), so the shared logic is a
// macro rather than a plain function. See the boss_build_info crate.
boss_build_info::stamp!();

fn main() -> ExitCode {
    // Intercept --version/-V before Cli::parse() so we print the
    // canonical version string.
    if boss_build_info::print_version_if_requested(&build_info::version_string("bossctl")) {
        return ExitCode::SUCCESS;
    }

    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("bossctl: failed to start tokio runtime: {err}");
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(dispatch(cli)) {
        // Not unconditionally SUCCESS: a verb that read a dispatch stream from
        // which records were lost produced a usable report over incomplete
        // evidence, and an automated caller that only checks the exit status
        // must be able to tell. See `stream_integrity::EXIT_UNRECOVERED_RECORDS`.
        Ok(()) => stream_integrity::exit_code(),
        Err(err) => {
            eprintln!("bossctl: {err:#}");
            ExitCode::from(1)
        }
    }
}

async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Probe { agent, text, urgent } => {
            probe::probe_run(&cli.socket_path, cli.json, agent, text, urgent).await
        }
        Command::ProbeStatus { probe_id } => probe::probe_status(&cli.socket_path, cli.json, probe_id).await,
        Command::Agents {
            action: AgentsAction::Status { agent },
        } => agents::agents_status(&cli.socket_path, cli.json, agent).await,
        Command::Agents {
            action: AgentsAction::List { all },
        } => agents::agents_list_live(&cli.socket_path, cli.json, all).await,
        Command::Agents {
            action: AgentsAction::Stop { agent },
        } => agents::agents_stop(&cli.socket_path, cli.json, agent).await,
        Command::Agents {
            action: AgentsAction::Hold { agent, reason },
        } => agents::agents_hold(&cli.socket_path, cli.json, agent, reason).await,
        Command::Agents {
            action: AgentsAction::ReleaseHold { agent },
        } => agents::agents_release_hold(&cli.socket_path, cli.json, agent).await,
        Command::Agents {
            action: AgentsAction::Focus { agent },
        } => agents::agents_focus(&cli.socket_path, cli.json, agent).await,
        Command::Agents {
            action: AgentsAction::Send { agent, text },
        } => agents::agents_send(&cli.socket_path, cli.json, agent, text).await,
        Command::Agents {
            action: AgentsAction::Interrupt { agent },
        } => agents::agents_interrupt(&cli.socket_path, cli.json, agent).await,
        Command::Agents {
            action:
                AgentsAction::Transcript {
                    agent,
                    lines,
                    format,
                    no_tools,
                },
        } => agents::agents_transcript(&cli.socket_path, cli.json, agent, lines, format, no_tools).await,
        Command::Agents {
            action: AgentsAction::Reap { agent },
        } => agents::agents_reap(&cli.socket_path, cli.json, agent).await,
        Command::Agents {
            action: AgentsAction::Pools,
        } => agents::agents_pools(&cli.socket_path, cli.json).await,
        Command::Agents {
            action: AgentsAction::RetirePane { agent },
        } => agents::agents_retire_pane(&cli.socket_path, cli.json, agent).await,
        Command::Agents {
            action:
                AgentsAction::Launch {
                    work_item_id,
                    preferred_workspace_id,
                },
        } => agents::agents_launch(&cli.socket_path, cli.json, work_item_id, preferred_workspace_id).await,
        Command::Work {
            action:
                WorkAction::Start {
                    work_item_id,
                    priority,
                    preferred_workspace_id,
                    force,
                },
        } => {
            agents::work_start(
                &cli.socket_path,
                cli.json,
                work_item_id,
                priority,
                preferred_workspace_id,
                force,
            )
            .await
        }
        Command::Work {
            action: WorkAction::Cancel { execution_id },
        } => agents::work_cancel(&cli.socket_path, cli.json, execution_id).await,
        Command::Work {
            action: WorkAction::Executions {
                work_item_id,
                state_root,
            },
        } => work_executions(cli.json, state_root, &work_item_id),
        Command::Work {
            action:
                WorkAction::Proposals {
                    action:
                        ProposalsAction::List {
                            execution_id,
                            work_item_id,
                            kind,
                            state,
                            state_root,
                        },
                },
        } => work_proposals_list(cli.json, state_root, execution_id, work_item_id, kind, state),
        Command::Workspace {
            action: WorkspaceAction::Summary,
        } => workspace_summary(&cli.socket_path, cli.json).await,
        Command::Review {
            action: review::ReviewAction::Start { pr_number, repo },
        } => review::review_start(&cli.socket_path, cli.json, pr_number, repo).await,
        Command::Review {
            action: review::ReviewAction::Show { work_item, state_root },
        } => review::review_show(cli.json, state_root, work_item),
        Command::LiveStatus {
            action: LiveStatusAction::Debug,
        } => live_status_debug(&cli.socket_path, cli.json).await,
        Command::Doctor {
            action: DoctorAction::Tmux,
        } => doctor::run_tmux_preflight(cli.json).await,
        Command::Dispatch {
            action:
                DispatchAction::Tail {
                    state_root,
                    n,
                    stage,
                    outcome,
                },
        } => dispatch_tail(cli.json, state_root, n, stage, outcome),
        Command::Dispatch {
            action:
                DispatchAction::Diagnose {
                    id,
                    state_root,
                    signatures_only,
                },
        } => dispatch_diagnose(cli.json, state_root, &id, signatures_only),
        Command::Dispatch {
            action:
                DispatchAction::GhostActive {
                    state_root,
                    stalled_after_secs,
                    include_stalled,
                },
        } => dispatch_ghost_active(cli.json, state_root, stalled_after_secs, include_stalled),
        Command::Pause { systems, reason } => {
            if systems.iter().any(|s| matches!(s, PauseArg::State)) {
                if systems.len() > 1 {
                    bail!("`bossctl pause state` does not take additional systems");
                }
                pause::unified_state(&cli.socket_path, cli.json).await
            } else {
                let reason = pause::require_pause_reason(reason)?;
                let targets = pause::pause_arg_targets(&systems);
                pause::set_paused_for_systems(&cli.socket_path, cli.json, &targets, Some(reason)).await
            }
        }
        Command::Resume { systems } => {
            let targets = if systems.is_empty() {
                PauseSystem::all()
            } else {
                systems
            };
            pause::set_paused_for_systems(&cli.socket_path, cli.json, &targets, None).await
        }
        Command::State => pause::unified_state(&cli.socket_path, cli.json).await,
        Command::Dispatch {
            action: DispatchAction::Pause { reason },
        } => pause::dispatch_set_paused(&cli.socket_path, cli.json, Some(reason)).await,
        Command::Dispatch {
            action: DispatchAction::Resume,
        } => pause::dispatch_set_paused(&cli.socket_path, cli.json, None).await,
        Command::Dispatch {
            action: DispatchAction::State { history, n, state_root },
        } => {
            if history {
                pause::dispatch_pause_history(cli.json, state_root, n)
            } else {
                pause::dispatch_state(&cli.socket_path, cli.json).await
            }
        }
        Command::Dispatch {
            action: DispatchAction::Stats { state_root, since, top },
        } => dispatch_stats::dispatch_stats(cli.json, state_root, since.as_deref(), top),
        Command::Dispatch {
            action: DispatchAction::Concurrency { set },
        } => dispatch_concurrency(&cli.socket_path, cli.json, set).await,
        Command::Automation {
            action: AutomationAction::Pause { reason },
        } => pause::automation_set_paused(&cli.socket_path, cli.json, Some(reason)).await,
        Command::Automation {
            action: AutomationAction::Resume,
        } => pause::automation_set_paused(&cli.socket_path, cli.json, None).await,
        Command::Automation {
            action: AutomationAction::State,
        } => pause::automation_state(&cli.socket_path, cli.json).await,
        Command::Metrics {
            action: MetricsAction::List { prefix, state_root },
        } => metrics_list(cli.json, state_root, prefix.as_deref()),
        Command::Metrics {
            action: MetricsAction::Show { name, live, state_root },
        } => {
            if live {
                metrics_show_live(&cli.socket_path, cli.json, name).await
            } else {
                metrics_show(cli.json, state_root, &name)
            }
        }
        Command::Metrics {
            action: MetricsAction::Github { hours, state_root },
        } => metrics_github(cli.json, state_root, hours),
        Command::Metrics {
            action: MetricsAction::Reset { name, all },
        } => {
            let target = if all { None } else { name };
            metrics_reset(&cli.socket_path, cli.json, target).await
        }
        Command::Hosts {
            action:
                HostsAction::Add {
                    id,
                    ssh_target,
                    pool_size,
                    tags,
                    skip_wrapper_push,
                    state_root,
                },
        } => hosts::hosts_add(cli.json, state_root, id, ssh_target, pool_size, tags, skip_wrapper_push).await,
        Command::Hosts {
            action: HostsAction::List { enabled, state_root },
        } => hosts::hosts_list(cli.json, state_root, enabled),
        Command::Hosts {
            action: HostsAction::Show { id, state_root },
        } => hosts::hosts_show(cli.json, state_root, id),
        Command::Hosts {
            action: HostsAction::Probe { id, state_root },
        } => hosts::hosts_probe(cli.json, state_root, id).await,
        Command::Hosts {
            action:
                HostsAction::Tag {
                    action: HostsTagAction::Add { id, tags, state_root },
                },
        } => hosts::hosts_tag_add(cli.json, state_root, id, tags),
        Command::Hosts {
            action:
                HostsAction::Tag {
                    action: HostsTagAction::Remove { id, tags, state_root },
                },
        } => hosts::hosts_tag_remove(cli.json, state_root, id, tags),
        Command::Hosts {
            action: HostsAction::Enable { id, state_root },
        } => hosts::hosts_set_enabled(cli.json, state_root, id, true),
        Command::Hosts {
            action: HostsAction::Disable { id, state_root },
        } => hosts::hosts_set_enabled(cli.json, state_root, id, false),
        Command::Hosts {
            action: HostsAction::Remove { id, state_root },
        } => hosts::hosts_remove(cli.json, state_root, id),
        Command::Executions {
            action:
                ExecutionsAction::Cancel {
                    execution_id,
                    work_item,
                    reason,
                },
        } => agents::executions_cancel(&cli.socket_path, cli.json, execution_id, work_item, reason).await,
        Command::Executions {
            action:
                ExecutionsAction::Prune {
                    older_than_days,
                    keep_per_work_item,
                    dry_run,
                    state_root,
                },
        } => executions_prune(cli.json, state_root, older_than_days, keep_per_work_item, dry_run),
        Command::CodexHomes {
            action:
                CodexHomesAction::Sweep {
                    older_than_days,
                    max_total_bytes,
                    dry_run,
                    state_root,
                },
        } => codex_homes_sweep(cli.json, state_root, older_than_days, max_total_bytes, dry_run).await,
        Command::GrokHomes {
            action:
                GrokHomesAction::Sweep {
                    older_than_days,
                    max_total_bytes,
                    dry_run,
                    state_root,
                },
        } => grok_homes_sweep(cli.json, state_root, older_than_days, max_total_bytes, dry_run).await,
        Command::Attachments {
            action: AttachmentsAction::List { work_item, state_root },
        } => attachments_list(cli.json, state_root, work_item),
        Command::Attachments {
            action:
                AttachmentsAction::Sweep {
                    older_than_days,
                    max_total_bytes,
                    dry_run,
                    state_root,
                },
        } => attachments_sweep(cli.json, state_root, older_than_days, max_total_bytes, dry_run).await,
        Command::Comments {
            action:
                comments::CommentsAction::List {
                    task,
                    artifact,
                    artifact_kind,
                    include_resolved,
                    state_root,
                },
        } => comments::comments_list(cli.json, state_root, task, artifact, artifact_kind, include_resolved),
        Command::Comments {
            action: comments::CommentsAction::Show { comment_id, state_root },
        } => comments::comments_show(cli.json, state_root, &comment_id),
        Command::Comments {
            action: comments::CommentsAction::Runs { comment_id, state_root },
        } => comments::comments_runs(cli.json, state_root, &comment_id),
        Command::SelectedProduct => selected_product::selected_product(&cli.socket_path, cli.json).await,
        Command::Reveal { id } => agents::reveal_work_item(&cli.socket_path, cli.json, id).await,
        Command::Open { path } => agents::open_document(&cli.socket_path, cli.json, path).await,
        Command::Logs {
            source,
            tail,
            follow,
            grep,
            since,
            until,
            target,
            level,
            fields,
            execution_id,
            state_root,
        } => {
            let query = logs::LogsQuery {
                tail,
                grep,
                since,
                until,
                target,
                level,
                fields,
                execution_id,
            };
            if follow {
                logs::logs_follow(source, state_root, query).await
            } else {
                logs::logs_tail(cli.json, source, state_root, query)
            }
        }
    }
}

pub(crate) fn resolve_state_root(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    dispatch_reader::default_state_root().ok_or_else(|| {
        anyhow::anyhow!("cannot resolve Boss state root: HOME is unset and no --state-root was provided")
    })
}

fn now_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn dispatch_tail(
    json: bool,
    state_root: Option<PathBuf>,
    n: usize,
    stage_filter: Option<String>,
    outcome_filter: Option<String>,
) -> Result<()> {
    let root = resolve_state_root(state_root)?;
    let read = dispatch_reader::read_current(&root)?;
    let integrity = stream_integrity::IntegrityReport::new(read.damage);
    let events = read.events;
    let slice = filter_and_tail(&events, n, stage_filter.as_deref(), outcome_filter.as_deref());

    if json {
        println!(
            "{}",
            serde_json::json!({
                "events": build_tail_json(slice),
                "stream_integrity": integrity.to_json(),
            })
        );
    } else {
        integrity.print_notice();
        if slice.is_empty() {
            println!("no dispatch events");
        } else {
            for event in slice {
                print_dispatch_event_short(event);
            }
        }
    }
    Ok(())
}

fn dispatch_diagnose(json: bool, state_root: Option<PathBuf>, id: &str, signatures_only: bool) -> Result<()> {
    let root = resolve_state_root(state_root.clone())?;
    // Optional state.db: resolve work-item ↔ executions and SIG-2 facts.
    // Missing/unreadable db is non-fatal — diagnose stays file-scan-first.
    let db = open_state_db(state_root).ok();
    doctor::run_diagnose(&root, id, json, signatures_only, now_epoch_ms(), db.as_ref())
}

fn dispatch_ghost_active(
    json: bool,
    state_root: Option<PathBuf>,
    stalled_after_secs: u64,
    include_stalled: bool,
) -> Result<()> {
    let root = resolve_state_root(state_root)?;
    let now = now_epoch_ms();
    let threshold_ms = (stalled_after_secs as u128).saturating_mul(1000);
    let report = dispatch_reader::ghost_active(&root, now, threshold_ms)?;
    // Every entry here is an absence claim ("never reached a terminal stage"),
    // so the integrity of the mirrors it was derived from is part of the answer.
    let integrity = stream_integrity::IntegrityReport::new(report.damage);
    let mut entries = report.entries;
    if include_stalled {
        entries.retain(|e| e.stalled);
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "ghost_active": entries,
                "stream_integrity": integrity.to_json(),
            })
        );
        return Ok(());
    }
    integrity.print_notice();
    if entries.is_empty() {
        println!("no ghost-active executions");
    } else {
        for entry in &entries {
            let elapsed_s = entry.elapsed_since_last_ms / 1000;
            let stalled_tag = if entry.stalled { "  [stalled]" } else { "" };
            let work_item = entry.work_item_id.as_deref().unwrap_or("-");
            println!(
                "{}  last={}/{}  elapsed={}s  work_item={}{}",
                entry.execution_id, entry.last_stage, entry.last_outcome, elapsed_s, work_item, stalled_tag,
            );
        }
    }
    Ok(())
}

/// Current interactive-pool concurrency cap as returned by
/// [`FrontendRequest::SetDispatchConcurrency`] / [`FrontendRequest::GetDispatchConcurrency`].
struct DispatchConcurrencyState {
    limit: usize,
    max: usize,
    clamped_from: Option<usize>,
}

async fn get_dispatch_concurrency_raw(socket_path: &Option<String>) -> Result<DispatchConcurrencyState> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::GetDispatchConcurrency)
        .await
        .context("sending GetDispatchConcurrency")?;
    match response {
        FrontendEvent::DispatchConcurrencyResult {
            limit,
            max,
            clamped_from,
        } => Ok(DispatchConcurrencyState {
            limit,
            max,
            clamped_from,
        }),
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected GetDispatchConcurrency: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn set_dispatch_concurrency_raw(socket_path: &Option<String>, limit: usize) -> Result<DispatchConcurrencyState> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::SetDispatchConcurrency { limit })
        .await
        .context("sending SetDispatchConcurrency")?;
    match response {
        FrontendEvent::DispatchConcurrencyResult {
            limit,
            max,
            clamped_from,
        } => Ok(DispatchConcurrencyState {
            limit,
            max,
            clamped_from,
        }),
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected SetDispatchConcurrency: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

fn print_dispatch_concurrency(json: bool, state: &DispatchConcurrencyState) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "limit": state.limit,
                "max": state.max,
                "clamped_from": state.clamped_from,
            })
        );
    } else {
        println!(
            "interactive concurrency cap: {} (worker-pool ceiling: {})",
            state.limit, state.max
        );
        if let Some(requested) = state.clamped_from {
            println!(
                "  requested {requested} exceeded the ceiling — clamped to {}",
                state.limit
            );
        }
    }
}

async fn dispatch_concurrency(socket_path: &Option<String>, json: bool, set: Option<usize>) -> Result<()> {
    let state = match set {
        Some(limit) => set_dispatch_concurrency_raw(socket_path, limit).await?,
        None => get_dispatch_concurrency_raw(socket_path).await?,
    };
    print_dispatch_concurrency(json, &state);
    Ok(())
}

fn filter_and_tail<'a>(
    events: &'a [DispatchEvent],
    n: usize,
    stage: Option<&str>,
    outcome: Option<&str>,
) -> Vec<&'a DispatchEvent> {
    let mut filtered: Vec<&DispatchEvent> = events
        .iter()
        .filter(|e| stage.is_none_or(|s| e.stage == s))
        .filter(|e| outcome.is_none_or(|o| e.outcome == o))
        .collect();
    let total = filtered.len();
    let start = total.saturating_sub(n);
    filtered.drain(..start);
    filtered
}

fn build_tail_json(slice: Vec<&DispatchEvent>) -> serde_json::Value {
    let events: Vec<&DispatchEvent> = slice;
    serde_json::json!({
        "events": events,
    })
}

fn print_dispatch_event_short(event: &DispatchEvent) {
    let worker = event.worker_id.as_deref().unwrap_or("-");
    let err = event.error_message.as_deref().unwrap_or("");
    if err.is_empty() {
        println!(
            "{}  {}/{}  exec={}  worker={}",
            event.ts_epoch_ms, event.stage, event.outcome, event.execution_id, worker,
        );
    } else {
        println!(
            "{}  {}/{}  exec={}  worker={}  error={}",
            event.ts_epoch_ms, event.stage, event.outcome, event.execution_id, worker, err,
        );
    }
}

pub(crate) async fn connect(socket_path: &Option<String>) -> Result<BossClient> {
    let discovery = Discovery::from_env(socket_path.as_deref()).context("resolving engine discovery profile")?;
    BossClient::connect(&discovery).await.context("connecting to engine")
}

async fn workspace_summary(socket_path: &Option<String>, json: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::WorkspacePoolSummary)
        .await
        .context("sending WorkspacePoolSummary")?;
    match response {
        FrontendEvent::WorkspacePoolSummaryResult { workspaces } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "workspaces": workspaces,
                    })
                );
            } else if workspaces.is_empty() {
                println!("no cube workspaces exist yet (they are created on demand)");
            } else {
                for ws in &workspaces {
                    print_workspace_entry_short(ws);
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected workspace summary: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

fn print_workspace_entry_short(entry: &WorkspacePoolEntry) {
    let lease = entry.lease_id.as_deref().unwrap_or("-");
    let exec = entry.execution_id.as_deref().unwrap_or("-");
    let task = entry.task.as_deref().unwrap_or("-");
    println!(
        "{}  state={}  lease={}  execution={}  task=\"{}\"  path={}",
        entry.workspace_id, entry.state, lease, exec, task, entry.workspace_path,
    );
}

#[allow(dead_code)]
fn print_run_short(run: &WorkRun) {
    let started = run.started_at.as_deref().unwrap_or("-");
    println!(
        "{}  agent={}  {}  {}  exec={}",
        run.id, run.agent_id, run.status, started, run.execution_id
    );
}

fn print_execution(json: bool, execution: &WorkExecution) {
    if json {
        println!(
            "{}",
            serde_json::to_string(execution).expect("WorkExecution serializes")
        );
    } else {
        println!("execution {}", execution.id);
        println!("  work_item: {}", execution.work_item_id);
        println!("  kind:      {}", execution.kind);
        println!("  status:    {}", execution.status);
        if let Some(p) = &execution.workspace_path {
            println!("  workspace: {p}");
        }
    }
}

/// Resolve the path to `state.db`. Checks `BOSS_DB_PATH` env var
/// first (the same override the engine uses), then falls back to the
/// default under `state_root` (which itself defaults to
/// `$HOME/Library/Application Support/Boss`). The explicit
/// `state_root` arg takes priority over `BOSS_DB_PATH`.
pub(crate) fn resolve_db_path(state_root: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(root) = state_root {
        return Ok(root.join("state.db"));
    }
    if let Some(path) = std::env::var_os("BOSS_DB_PATH") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("cannot resolve Boss state.db: HOME is unset; pass --state-root"))?;
    Ok(PathBuf::from(home).join("Library/Application Support/Boss/state.db"))
}

/// Resolve `state.db`'s path via [`resolve_db_path`] and open it. This bundles
/// the `resolve_db_path` + [`WorkDb::open`] pair that every direct-DB command
/// (`comments`, `work executions`, `executions prune`, `hosts`, …) repeats,
/// attaching the standard `"opening state.db"` context on failure.
pub(crate) fn open_state_db(state_root: Option<PathBuf>) -> Result<WorkDb> {
    let db_path = resolve_db_path(state_root)?;
    WorkDb::open(db_path).context("opening state.db")
}

/// `bossctl executions prune` — on-demand retention cleanup of terminal
/// `work_executions` rows. Opens `state.db` directly via [`resolve_db_path`]
/// (same resolution `metrics`/`hosts` use), so it is always scoped to this
/// install's own state, never a cross-install sweep.
fn executions_prune(
    json: bool,
    state_root: Option<PathBuf>,
    older_than_days: i64,
    keep_per_work_item: u32,
    dry_run: bool,
) -> Result<()> {
    let db = open_state_db(state_root)?;
    let policy = boss_engine::work::ExecutionRetentionPolicy {
        max_age_secs: older_than_days.saturating_mul(24 * 60 * 60),
        keep_per_work_item,
    };
    let now_epoch = now_epoch_ms() as i64 / 1000;
    let outcome = db
        .prune_terminal_executions(policy, now_epoch, dry_run)
        .context("pruning terminal executions")?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "deleted": outcome.deleted,
                "codex_homes_reclaimed": outcome.codex_homes_reclaimed,
                "dry_run": dry_run,
                "older_than_days": older_than_days,
                "keep_per_work_item": keep_per_work_item,
            })
        );
    } else if dry_run {
        println!(
            "would delete {} terminal execution row(s) older than {}d (keeping {} most recent per work item)",
            outcome.deleted, older_than_days, keep_per_work_item
        );
    } else {
        println!(
            "deleted {} terminal execution row(s) older than {}d (kept {} most recent per work item); reclaimed {} Codex home(s)",
            outcome.deleted, older_than_days, keep_per_work_item, outcome.codex_homes_reclaimed
        );
    }
    Ok(())
}

/// `bossctl codex-homes sweep` — on-demand reclaim of recorded Boss-owned
/// CODEX_HOME trees past retention. Never scans `~/.codex`.
async fn codex_homes_sweep(
    json: bool,
    state_root: Option<PathBuf>,
    older_than_days: u64,
    max_total_bytes: u64,
    dry_run: bool,
) -> Result<()> {
    let db = open_state_db(state_root)?;
    let policy = boss_engine::codex_rollout_retention::CodexHomeRetentionPolicy::new(
        std::time::Duration::from_secs(older_than_days.saturating_mul(24 * 60 * 60)),
        max_total_bytes,
    );
    let outcome = boss_engine::codex_home_retention_sweep::run_one_pass_with_policy(&db, &policy, dry_run).await;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "scanned": outcome.scanned,
                "deleted": outcome.deleted,
                "deleted_bytes": outcome.deleted_bytes,
                "skipped_live": outcome.skipped_live,
                "kept_in_policy": outcome.kept_in_policy,
                "errors": outcome.errors,
                "dry_run": dry_run,
                "older_than_days": older_than_days,
                "max_total_bytes": max_total_bytes,
            })
        );
    } else if dry_run {
        println!(
            "would reclaim {} recorded Codex home(s) ({} bytes); scanned={}, skipped_live={}, kept_in_policy={}",
            outcome.deleted, outcome.deleted_bytes, outcome.scanned, outcome.skipped_live, outcome.kept_in_policy
        );
    } else {
        println!(
            "reclaimed {} recorded Codex home(s) ({} bytes); scanned={}, skipped_live={}, kept_in_policy={}, errors={}",
            outcome.deleted,
            outcome.deleted_bytes,
            outcome.scanned,
            outcome.skipped_live,
            outcome.kept_in_policy,
            outcome.errors
        );
    }
    Ok(())
}

/// `bossctl grok-homes sweep` — on-demand reclaim of recorded Boss-owned
/// Grok run-container trees past retention. Never scans `~/.grok`. Mirrors
/// [`codex_homes_sweep`].
async fn grok_homes_sweep(
    json: bool,
    state_root: Option<PathBuf>,
    older_than_days: u64,
    max_total_bytes: u64,
    dry_run: bool,
) -> Result<()> {
    let db = open_state_db(state_root)?;
    let policy = boss_engine::grok_home_retention::GrokHomeRetentionPolicy::new(
        std::time::Duration::from_secs(older_than_days.saturating_mul(24 * 60 * 60)),
        max_total_bytes,
    );
    let outcome = boss_engine::grok_home_retention_sweep::run_one_pass_with_policy(&db, &policy, dry_run).await;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "scanned": outcome.scanned,
                "deleted": outcome.deleted,
                "deleted_bytes": outcome.deleted_bytes,
                "skipped_live": outcome.skipped_live,
                "kept_in_policy": outcome.kept_in_policy,
                "errors": outcome.errors,
                "dry_run": dry_run,
                "older_than_days": older_than_days,
                "max_total_bytes": max_total_bytes,
            })
        );
    } else if dry_run {
        println!(
            "would reclaim {} recorded Grok home(s) ({} bytes); scanned={}, skipped_live={}, kept_in_policy={}",
            outcome.deleted, outcome.deleted_bytes, outcome.scanned, outcome.skipped_live, outcome.kept_in_policy
        );
    } else {
        println!(
            "reclaimed {} recorded Grok home(s) ({} bytes); scanned={}, skipped_live={}, kept_in_policy={}, errors={}",
            outcome.deleted,
            outcome.deleted_bytes,
            outcome.scanned,
            outcome.skipped_live,
            outcome.kept_in_policy,
            outcome.errors
        );
    }
    Ok(())
}

/// `bossctl attachments list` — stored screenshot evidence, newest first.
///
/// Reads `state.db` directly like every other direct-DB verb, so it answers
/// "what evidence exists" even when the engine (and therefore the loopback
/// gallery) is not running.
fn attachments_list(json: bool, state_root: Option<PathBuf>, work_item: Option<String>) -> Result<()> {
    let db = open_state_db(state_root)?;
    let attachments = match work_item.as_deref() {
        Some(id) => db
            .list_work_attachments_for_work_item(id)
            .context("listing attachments for work item")?,
        None => db.list_all_work_attachments().context("listing attachments")?,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&attachments)?);
        return Ok(());
    }
    if attachments.is_empty() {
        println!("no stored attachments");
        return Ok(());
    }
    for attachment in &attachments {
        let state = if attachment.is_reclaimed() {
            "reclaimed"
        } else {
            "stored"
        };
        let caption = if attachment.caption.is_empty() {
            attachment.source_name.as_str()
        } else {
            attachment.caption.as_str()
        };
        println!(
            "{}  {}  {}  {}x{}  {}B  {state}  {caption}",
            attachment.id,
            attachment.work_item_id,
            attachment.execution_id,
            attachment.pixel_width,
            attachment.pixel_height,
            attachment.size_bytes,
        );
    }
    Ok(())
}

/// `bossctl attachments sweep` — on-demand evidence retention. Mirrors
/// [`grok_homes_sweep`]: same age + total-bytes policy shape, same
/// `--dry-run`, same "a running engine already does this on a schedule".
async fn attachments_sweep(
    json: bool,
    state_root: Option<PathBuf>,
    older_than_days: u64,
    max_total_bytes: u64,
    dry_run: bool,
) -> Result<()> {
    let store_root = resolve_db_path(state_root.clone())?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve the Boss state root from the state.db path"))?;
    let db = open_state_db(state_root)?;
    let store = boss_engine::attachments::AttachmentStore::under_state_root(&store_root);
    let policy = boss_engine::attachments::AttachmentRetentionPolicy::new(
        std::time::Duration::from_secs(older_than_days.saturating_mul(24 * 60 * 60)),
        max_total_bytes,
    );
    let outcome =
        boss_engine::attachment_retention_sweep::run_one_pass_with_policy(&db, &store, &policy, dry_run).await;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "scanned": outcome.scanned,
                "reclaimed_rows": outcome.reclaimed_rows,
                "deleted_blobs": outcome.deleted_blobs,
                "reclaimed_bytes": outcome.reclaimed_bytes,
                "skipped_live": outcome.skipped_live,
                "kept_in_policy": outcome.kept_in_policy,
                "orphans_deleted": outcome.orphans_deleted,
                "errors": outcome.errors,
                "dry_run": dry_run,
                "older_than_days": older_than_days,
                "max_total_bytes": max_total_bytes,
            })
        );
    } else if dry_run {
        println!(
            "would reclaim {} attachment(s) ({} bytes, {} blob(s)) and {} orphan blob(s); scanned={}, skipped_live={}, kept_in_policy={}",
            outcome.reclaimed_rows,
            outcome.reclaimed_bytes,
            outcome.deleted_blobs,
            outcome.orphans_deleted,
            outcome.scanned,
            outcome.skipped_live,
            outcome.kept_in_policy
        );
    } else {
        println!(
            "reclaimed {} attachment(s) ({} bytes, {} blob(s)) and {} orphan blob(s); scanned={}, skipped_live={}, kept_in_policy={}, errors={}",
            outcome.reclaimed_rows,
            outcome.reclaimed_bytes,
            outcome.deleted_blobs,
            outcome.orphans_deleted,
            outcome.scanned,
            outcome.skipped_live,
            outcome.kept_in_policy,
            outcome.errors
        );
    }
    Ok(())
}

/// `bossctl work executions` — full execution history for a work item,
/// oldest first, with the host each execution ran on. Opens `state.db`
/// directly via [`resolve_db_path`] (same resolution `metrics`/`hosts`
/// use), so it works even when the engine is wedged.
fn work_executions(json: bool, state_root: Option<PathBuf>, work_item_id: &str) -> Result<()> {
    let db = open_state_db(state_root)?;
    let executions = db.list_executions(Some(work_item_id)).context("listing executions")?;
    let host_ids = db
        .execution_host_ids_for_item(work_item_id)
        .context("resolving execution hosts")?;
    let hosts: Vec<String> = executions
        .iter()
        .map(|e| host_ids.get(&e.id).cloned().unwrap_or_else(|| "local".to_owned()))
        .collect();

    if json {
        let entries: Vec<serde_json::Value> = executions
            .iter()
            .zip(hosts.iter())
            .map(|(exec, host)| {
                let mut value = serde_json::to_value(exec).unwrap_or(serde_json::Value::Null);
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("host_id".into(), serde_json::Value::String(host.clone()));
                }
                value
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "work_item_id": work_item_id,
                "executions": entries,
            })
        );
    } else if executions.is_empty() {
        println!("no executions for {work_item_id}");
    } else {
        for (exec, host) in executions.iter().zip(hosts.iter()) {
            print_execution_history_row(exec, host);
        }
    }
    Ok(())
}

/// `bossctl work proposals list` — the `worker_proposals` ledger,
/// optionally filtered by execution/work-item/kind/state. Opens `state.db`
/// directly via [`resolve_db_path`] (same resolution `metrics`/`hosts`/
/// `work executions` use), so it works even when the engine is wedged.
///
/// Per the design's §"UI visibility and provenance", proposals get no
/// app-side listing surface — this CLI verb is the full ledger, including
/// `rejected`/`expired` history, which is why `state` is left unfiltered
/// by default rather than defaulting to `proposed` only.
fn work_proposals_list(
    json: bool,
    state_root: Option<PathBuf>,
    execution_id: Option<String>,
    work_item_id: Option<String>,
    kind: Option<String>,
    state: Option<String>,
) -> Result<()> {
    let kind = kind
        .map(|k| k.parse::<ProposalKind>())
        .transpose()
        .map_err(|err| anyhow::anyhow!(err))
        .context("parsing --kind")?;
    let state = state
        .map(|s| s.parse::<ProposalState>())
        .transpose()
        .map_err(|err| anyhow::anyhow!(err))
        .context("parsing --state")?;

    let db = open_state_db(state_root)?;
    let proposals = db
        .list_worker_proposals(execution_id.as_deref(), work_item_id.as_deref(), kind, state)
        .context("listing worker proposals")?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "proposals": proposals,
            })
        );
    } else if proposals.is_empty() {
        println!("no proposals match the given filters");
    } else {
        for proposal in &proposals {
            print_proposal_row(proposal);
        }
    }
    Ok(())
}

fn print_proposal_row(proposal: &WorkerProposal) {
    let work_item = proposal.work_item_id.as_deref().unwrap_or("-");
    println!(
        "{}  [{}]  kind={}  execution={}  work_item={}  created={}",
        proposal.id, proposal.state, proposal.kind, proposal.execution_id, work_item, proposal.created_at,
    );
    if let Some(applied_ref) = &proposal.applied_ref {
        println!("  applied_ref: {applied_ref}");
    }
    if let Some(decision_reason) = &proposal.decision_reason {
        let decided_by = proposal
            .decided_by
            .map(|d| d.to_string())
            .unwrap_or_else(|| "-".to_owned());
        println!("  decision ({decided_by}): {decision_reason}");
    }
}

fn print_execution_history_row(exec: &WorkExecution, host: &str) {
    let workspace = exec.cube_workspace_id.as_deref().unwrap_or("-");
    let started = exec.started_at.as_deref().unwrap_or("-");
    let finished = exec.finished_at.as_deref().unwrap_or("-");
    println!(
        "{}  [{}]  kind={}  host={}  workspace={}  created={}  started={}  finished={}",
        exec.id, exec.status, exec.kind, host, workspace, exec.created_at, started, finished,
    );
    if let Some(pr_url) = &exec.pr_url {
        println!("  pr_url: {pr_url}");
    }
}

/// Format a millisecond timestamp as a human-friendly relative age
/// string ("3m ago", "2h ago", "never"). Shown next to each metric
/// in the `list` / `show` output.
fn format_age_ms(ts_ms: i64, now_ms: u128) -> String {
    if ts_ms <= 0 {
        return "(never)".into();
    }
    let now_i64 = now_ms as i64;
    let diff_ms = now_i64.saturating_sub(ts_ms);
    if diff_ms < 0 {
        return "(just now)".into();
    }
    let diff_s = diff_ms / 1000;
    if diff_s < 60 {
        return format!("({}s ago)", diff_s);
    }
    let diff_m = diff_s / 60;
    if diff_m < 60 {
        return format!("({}m ago)", diff_m);
    }
    let diff_h = diff_m / 60;
    if diff_h < 24 {
        return format!("({}h ago)", diff_h);
    }
    let diff_d = diff_h / 24;
    format!("({}d ago)", diff_d)
}

/// A unified metric row for rendering, covering both counters and
/// gauges loaded from `state.db`.
struct MetricRow {
    name: String,
    description: String,
    kind: &'static str,
    value: i64,
    timestamp_ms: i64,
    stale: bool,
}

fn load_metric_rows(db_path: PathBuf, prefix: Option<&str>) -> Result<Vec<MetricRow>> {
    let db = WorkDb::open(db_path).context("opening state.db")?;
    let (counters, gauges) = db.metrics_load_all().context("reading metrics from state.db")?;

    let mut rows: Vec<MetricRow> = counters
        .into_iter()
        .map(|c| MetricRow {
            name: c.name,
            description: c.description,
            kind: "counter",
            value: c.value as i64,
            timestamp_ms: c.updated_at_ms,
            stale: false,
        })
        .chain(gauges.into_iter().map(|g| MetricRow {
            name: g.name,
            description: g.description,
            kind: "gauge",
            value: g.value,
            timestamp_ms: g.observed_at_ms,
            stale: false,
        }))
        .collect();

    if let Some(p) = prefix {
        rows.retain(|r| r.name.starts_with(p));
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(rows)
}

fn print_metric_row_short(row: &MetricRow, now_ms: u128, name_width: usize) {
    let age = format_age_ms(row.timestamp_ms, now_ms);
    let stale_tag = if row.stale { " [stale]" } else { "" };
    println!(
        "{:<width$}  {:>12}  {:>10}  {}{}",
        row.name,
        row.value,
        age,
        row.kind,
        stale_tag,
        width = name_width,
    );
}

fn metrics_list(json: bool, state_root: Option<PathBuf>, prefix: Option<&str>) -> Result<()> {
    let db_path = resolve_db_path(state_root)?;
    let rows = load_metric_rows(db_path, prefix)?;
    let now = now_epoch_ms();

    if json {
        let entries: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "description": r.description,
                    "kind": r.kind,
                    "value": r.value,
                    "timestamp_ms": r.timestamp_ms,
                    "stale": r.stale,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "metrics": entries }));
    } else if rows.is_empty() {
        println!("no metrics in state.db (engine may not have flushed yet)");
    } else {
        let name_width = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
        for row in &rows {
            print_metric_row_short(row, now, name_width);
        }
    }
    Ok(())
}

fn metrics_show(json: bool, state_root: Option<PathBuf>, name: &str) -> Result<()> {
    let db_path = resolve_db_path(state_root)?;
    let rows = load_metric_rows(db_path, None)?;
    let now = now_epoch_ms();

    let row = rows.iter().find(|r| r.name == name);
    match row {
        None => {
            if json {
                println!("{}", serde_json::json!({ "entry": null, "name": name }));
            } else {
                println!("metric not found: {name}");
                println!("  (engine may not have flushed yet; try --live to read in-memory value)");
            }
        }
        Some(r) => {
            let age = format_age_ms(r.timestamp_ms, now);
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "entry": {
                            "name": r.name,
                            "description": r.description,
                            "kind": r.kind,
                            "value": r.value,
                            "timestamp_ms": r.timestamp_ms,
                            "stale": r.stale,
                        }
                    })
                );
            } else {
                let stale_tag = if r.stale {
                    "  [stale: not registered by current engine]"
                } else {
                    ""
                };
                println!("{}{}", r.name, stale_tag);
                println!("  description:   {}", r.description);
                println!("  kind:          {}", r.kind);
                println!("  value:         {}", r.value);
                println!("  last_updated:  {age}");
            }
        }
    }
    Ok(())
}

async fn metrics_show_live(socket_path: &Option<String>, json: bool, name: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::MetricsShowLive { name: name.clone() })
        .await
        .context("sending MetricsShowLive")?;
    match response {
        FrontendEvent::MetricsShowLiveResult { entry } => {
            print_metric_live_entry(json, &name, entry.as_ref());
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected metrics show --live: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

fn print_metric_live_entry(json: bool, name: &str, entry: Option<&MetricLiveEntry>) {
    let now = now_epoch_ms();
    match entry {
        None => {
            if json {
                println!("{}", serde_json::json!({ "entry": null, "name": name }));
            } else {
                println!("metric not found: {name}");
                println!("  (not registered in the current engine binary)");
            }
        }
        Some(e) => {
            let age = format_age_ms(e.timestamp_ms, now);
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "entry": {
                            "name": e.name,
                            "description": e.description,
                            "kind": e.kind,
                            "value": e.value,
                            "timestamp_ms": e.timestamp_ms,
                            "stale": e.stale,
                            "source": "live",
                        }
                    })
                );
            } else {
                let stale_tag = if e.stale {
                    "  [stale: not registered by current engine]"
                } else {
                    ""
                };
                println!("{}{}", e.name, stale_tag);
                println!("  description:   {}", e.description);
                println!("  kind:          {}  (live — read from in-memory atomic)", e.kind);
                println!("  value:         {}", e.value);
                println!("  last_updated:  {age}");
            }
        }
    }
}

/// GitHub's hourly quota for a personal token, on each of the two
/// independently-metered buckets. Rates are printed against this so the
/// table answers "over or under budget" without the reader doing the
/// division.
const GITHUB_HOURLY_BUDGET: f64 = 5000.0;

/// Convert a spend and an observed span into a points-per-hour rate.
///
/// Divides by the span the data *actually* covers, not by the requested
/// look-back: an engine that has been running the instrumented build for
/// 20 minutes, queried with `--hours 24`, would otherwise report a rate
/// ~70x under the truth. A span shorter than a minute is not a usable
/// denominator (one burst would extrapolate to an absurd rate), so it
/// reports `None` rather than a confident wrong number.
fn points_per_hour(points: i64, span_ms: i64) -> Option<f64> {
    if span_ms < 60_000 {
        return None;
    }
    Some(points as f64 * 3_600_000.0 / span_ms as f64)
}

/// Per-subsystem GitHub API attribution over a time window.
fn metrics_github(json: bool, state_root: Option<PathBuf>, hours: u32) -> Result<()> {
    let db_path = resolve_db_path(state_root)?;
    let db = WorkDb::open(db_path).context("opening state.db")?;
    let since_ms = now_epoch_ms() as i64 - (hours as i64) * 3_600_000;

    let buckets = db
        .github_api_usage_by_caller(since_ms)
        .context("reading github_api_calls from state.db")?;
    let window = db
        .github_api_usage_window(since_ms)
        .context("reading github_api_calls window from state.db")?;
    // A single sample spans zero time; treat the observed span as at
    // least the gap between first and last row.
    let span_ms = window.map(|(min, max)| max - min).unwrap_or(0);

    if json {
        let entries: Vec<serde_json::Value> = buckets
            .iter()
            .map(|b| {
                serde_json::json!({
                    "caller": b.caller,
                    "api": b.api,
                    "calls": b.calls,
                    "points": b.points,
                    "calls_without_reading": b.calls_without_reading,
                    "errors": b.errors,
                    "rate_limited": b.rate_limited,
                    "points_per_hour": points_per_hour(b.points, span_ms),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "since_epoch_ms": since_ms,
                "observed_span_ms": span_ms,
                "hourly_budget": GITHUB_HOURLY_BUDGET,
                "usage": entries,
            })
        );
        return Ok(());
    }

    if buckets.is_empty() {
        println!("no github_api_calls rows in the last {hours}h (engine may not have made a call yet)");
        return Ok(());
    }

    let span_hours = span_ms as f64 / 3_600_000.0;
    println!("GitHub API usage — {span_hours:.2}h observed (requested {hours}h look-back)");
    println!();
    let name_width = buckets.iter().map(|b| b.caller.len()).max().unwrap_or(10).max(10);
    println!(
        "{:<width$}  {:>8}  {:>8}  {:>9}  {:>11}  {:>7}  {:>7}",
        "SUBSYSTEM",
        "API",
        "CALLS",
        "UNITS",
        "UNITS/HR",
        "ERRORS",
        "429s",
        width = name_width,
    );
    for b in &buckets {
        let rate = match points_per_hour(b.points, span_ms) {
            Some(rate) => format!("{rate:.0}"),
            None => "—".to_owned(),
        };
        println!(
            "{:<width$}  {:>8}  {:>8}  {:>9}  {:>11}  {:>7}  {:>7}",
            b.caller,
            b.api,
            b.calls,
            b.points,
            rate,
            b.errors,
            b.rate_limited,
            width = name_width,
        );
    }

    // Per-bucket totals: GraphQL and REST have separate 5000/hour
    // budgets, so a combined total would compare against a limit that
    // does not exist.
    println!();
    for api in ["graphql", "rest", "cli"] {
        let points: i64 = buckets.iter().filter(|b| b.api == api).map(|b| b.points).sum();
        let calls: i64 = buckets.iter().filter(|b| b.api == api).map(|b| b.calls).sum();
        if calls == 0 {
            continue;
        }
        match points_per_hour(points, span_ms) {
            Some(rate) => {
                let pct = rate / GITHUB_HOURLY_BUDGET * 100.0;
                let verdict = if rate > GITHUB_HOURLY_BUDGET {
                    "OVER BUDGET"
                } else {
                    "within budget"
                };
                println!("{api:>8}: {calls} calls, {points} units → {rate:.0}/hr ({pct:.0}% of 5000/hr) — {verdict}");
            }
            None => println!("{api:>8}: {calls} calls, {points} units (window too short for a rate)"),
        }
    }

    let unmeasured: i64 = buckets.iter().map(|b| b.calls_without_reading).sum();
    if unmeasured > 0 {
        println!();
        println!(
            "note: {unmeasured} call(s) carried no rateLimit reading — their spend is counted \
             as calls but contributes 0 units, so the rates above are a lower bound.",
        );
    }
    Ok(())
}

async fn metrics_reset(socket_path: &Option<String>, json: bool, name: Option<String>) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::MetricsReset { name: name.clone() })
        .await
        .context("sending MetricsReset")?;
    match response {
        FrontendEvent::MetricsResetDone {
            name: returned_name,
            counters_reset,
            gauges_reset,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "reset",
                        "name": returned_name,
                        "counters_reset": counters_reset,
                        "gauges_reset": gauges_reset,
                    })
                );
            } else {
                match &name {
                    Some(n) => {
                        if counters_reset == 0 && gauges_reset == 0 {
                            println!("metric not found: {n}");
                        } else {
                            println!("reset {n} ({} counter(s), {} gauge(s))", counters_reset, gauges_reset);
                        }
                    }
                    None => {
                        println!(
                            "reset all metrics ({} counter(s), {} gauge(s))",
                            counters_reset, gauges_reset
                        );
                    }
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected metrics reset: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// One-shot diagnostic snapshot of the live-status pipeline. Mirrors
/// the chore acceptance criteria: per-slot picture (task running,
/// disabled flag, last trigger, last summarizer outcome, last
/// successful summary, current transcript path) plus engine build
/// SHA + ANTHROPIC_API_KEY presence.
async fn live_status_debug(socket_path: &Option<String>, json: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::DebugLiveStatusPipeline)
        .await
        .context("sending DebugLiveStatusPipeline")?;
    let report = match response {
        FrontendEvent::LiveStatusDebugReportEvent { report } => report,
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected live-status debug: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    };

    if json {
        println!(
            "{}",
            serde_json::to_string(&report).expect("LiveStatusDebugReport serializes"),
        );
    } else {
        print_live_status_debug_human(&report);
    }
    Ok(())
}

fn print_live_status_debug_human(report: &LiveStatusDebugReport) {
    println!("live-status pipeline debug");
    println!(
        "  engine_build_sha:           {}{}",
        report.engine_build_sha,
        if report.engine_build_dirty { " (dirty)" } else { "" },
    );
    println!("  engine_build_time:          {}", report.engine_build_time);
    println!("  engine_binary_fingerprint:  {}", report.engine_binary_fingerprint,);
    println!("  engine_process_started_at:  {}", report.engine_process_started_at,);
    println!(
        "  anthropic_api_key_present:  {}",
        if report.anthropic_api_key_present {
            "yes"
        } else {
            "NO (summarizer cannot succeed)"
        },
    );
    println!("  tracked_slots:              {}", report.tracked_slot_count);
    println!("  disabled_slots:             {}", report.disabled_slot_count);
    println!();
    print_dispatcher_stats(&report.dispatcher_stats);
    if report.slots.is_empty() {
        println!("  (no slots tracked)");
        return;
    }
    println!();
    for slot in &report.slots {
        print_live_status_slot_debug(slot);
    }
}

fn print_dispatcher_stats(stats: &boss_protocol::DispatcherStatsReport) {
    println!("dispatcher stats");
    println!(
        "  hook_events_total:                          {}",
        stats.hook_events_total
    );
    println!(
        "  hook_events_dropped_missing_run_id:         {}",
        stats.hook_events_dropped_missing_run_id,
    );
    println!(
        "  hook_events_with_transcript_path_in_payload:    {}",
        stats.hook_events_with_transcript_path_in_payload,
    );
    println!(
        "  hook_events_without_transcript_path_in_payload: {}",
        stats.hook_events_without_transcript_path_in_payload,
    );
    println!(
        "  transcript_path_persist_updated:             {}",
        stats.transcript_path_persist_updated,
    );
    println!(
        "  transcript_path_persist_noop:                {}",
        stats.transcript_path_persist_noop,
    );
    println!(
        "  transcript_path_persist_row_missing:         {}",
        stats.transcript_path_persist_row_missing,
    );
    println!(
        "  transcript_path_persist_err:                 {}",
        stats.transcript_path_persist_err,
    );
    println!(
        "  transcript_path_persist_from_cache:          {}",
        stats.transcript_path_persist_from_cache,
    );
    match (
        stats.last_hook_kind.as_deref(),
        stats.last_hook_run_id.as_deref(),
        stats.last_hook_at.as_deref(),
    ) {
        (Some(kind), Some(run_id), Some(at)) => {
            println!("  last_hook: {kind} for {run_id} @ {at}");
        }
        _ => println!("  last_hook: (no hook events dispatched yet)"),
    }
}

fn print_live_status_slot_debug(slot: &LiveStatusSlotDebug) {
    println!("slot {}", slot.slot_id);
    println!(
        "  task_running:        {}",
        if slot.task_running {
            "yes"
        } else {
            "no (notifies will drop)"
        },
    );
    println!("  disabled:            {}", if slot.disabled { "yes" } else { "no" },);
    println!(
        "  transcript_path:     {}",
        slot.transcript_path
            .as_deref()
            .unwrap_or("(unset — work_runs.transcript_path is NULL)"),
    );
    match (&slot.last_trigger_kind, &slot.last_trigger_at) {
        (Some(kind), Some(at)) => {
            println!("  last_trigger:        {kind} @ {at} (any source)");
        }
        _ => println!("  last_trigger:        (none yet)"),
    }
    match (&slot.last_real_trigger_kind, &slot.last_real_trigger_at) {
        (Some(kind), Some(at)) => {
            println!("  last_real_trigger:   {kind} @ {at} (from real hook fan-out)");
        }
        _ => println!("  last_real_trigger:   (none yet — no hook ever reached the slot loop)"),
    }
    match &slot.last_synthetic_trigger_at {
        Some(at) => println!("  last_synthetic:      timer-floor fired @ {at}"),
        None => println!("  last_synthetic:      (timer floor has not fired)"),
    }
    match (&slot.last_outcome_tag, &slot.last_outcome_at) {
        (Some(tag), Some(at)) => {
            println!("  last_outcome:        {tag} @ {at}");
            if let Some(detail) = &slot.last_outcome_detail {
                println!("    detail:            {detail}");
            }
        }
        _ => println!("  last_outcome:        (no summarizer attempt yet)"),
    }
    match (&slot.last_success_at, &slot.last_success_text) {
        (Some(at), Some(text)) => {
            println!("  last_success:        {at}");
            println!("    text:              {text}");
        }
        _ => println!("  last_success:        (no successful summary yet)"),
    }
    if let Some(bytes) = slot.last_redacted_bytes {
        println!("  last_redacted_bytes: {bytes}");
    }
    println!();
}

#[cfg(test)]
mod tests;
