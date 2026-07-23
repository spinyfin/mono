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
mod hosts;
mod logs;
mod review;
use boss_engine::dispatch_events::DispatchEvent;
use boss_engine::dispatch_reader;
use boss_protocol::{
    FrontendEvent, FrontendRequest, LiveStatusDebugReport, LiveStatusSlotDebug, LiveWorkerState, MetricLiveEntry,
    ROSTER, RequestExecutionInput, WorkExecution, WorkItem, WorkRun, WorkspacePoolEntry,
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
    /// Inject a probe prompt into a worker. If the worker is currently
    /// parked (idle between turns, or sitting at its prompt after a
    /// Stop that followed a notification/permission prompt) the text
    /// lands immediately; if the worker is actively running it is
    /// queued and delivered at the next Stop boundary. With
    /// `--urgent`, the probe is delivered at the next tool-call
    /// boundary (PostToolUse) instead of the next Stop boundary, so
    /// the coordinator can redirect a mid-task worker without waiting
    /// for it to finish its current turn. The engine always waits for
    /// any in-flight tool call to return before injecting, so no work
    /// is discarded.
    Probe {
        /// Worker reference: run id, slot id, or crew name (e.g.
        /// `Riker`). Crew names resolve only over currently-live
        /// slots; case-insensitive.
        agent: String,
        /// Probe text the worker will see as its next prompt.
        text: String,
        /// Deliver the probe at the next tool-call boundary
        /// (PostToolUse) instead of the next Stop boundary. Urgent
        /// probes jump ahead of any queued non-urgent probes and are
        /// prefixed with `[coordinator-nudge]` in the transcript so
        /// the worker and human readers can identify them.
        #[arg(long)]
        urgent: bool,
    },
    /// Work-item dispatch aliases for symmetry with `boss`.
    Work {
        #[command(subcommand)]
        action: WorkAction,
    },
    /// Inspect the cube workspace pool.
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
    /// Inspect the dispatch-pipeline event stream (file-scan only —
    /// works when the engine is wedged).
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
    /// discovered from the local machine.
    Hosts {
        #[command(subcommand)]
        action: HostsAction,
    },
    /// Inspect and prune terminal `work_executions` rows (retention).
    ///
    /// A running engine already prunes this on a recurring background
    /// sweep (see `crate::execution_retention_sweep`); this verb is for
    /// on-demand cleanup between sweeps or while the engine is stopped.
    /// Reads/writes `state.db` directly, scoped to this install's state
    /// root (`--state-root`, `BOSS_DB_PATH`, or
    /// `$HOME/Library/Application Support/Boss` — same resolution as
    /// `metrics`/`hosts`) — never a cross-install sweep.
    Executions {
        #[command(subcommand)]
        action: ExecutionsAction,
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
    /// The primary log (`engine`, default) is `engine-trace.jsonl` —
    /// structured JSONL tracing events from the running engine. The `audit`
    /// log is `engine-audit.log` — compact lifecycle records (start, socket
    /// bind, shutdown) useful for timeline reconstruction after an incident.
    ///
    /// Output is plain text suitable for copy/paste into a shake report.
    Logs {
        /// Which log to read.
        /// `engine` → `engine-trace.jsonl` (structured trace, primary);
        /// `audit`  → `engine-audit.log` (lifecycle events).
        #[arg(value_enum, default_value_t = LogSource::Engine)]
        source: LogSource,
        /// Print the last N lines (default 50).
        #[arg(short = 'n', long = "tail", default_value_t = 50)]
        tail: usize,
        /// Stream appended lines live, like `tail -f`. Polls every 250 ms;
        /// press Ctrl-C to stop.
        #[arg(short = 'f', long)]
        follow: bool,
        /// Filter to lines containing this substring (case-sensitive).
        #[arg(long)]
        grep: Option<String>,
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
    /// Print the full per-execution timeline for one execution id,
    /// with stage durations and the full `error_message` on any
    /// failure event.
    Diagnose {
        execution_id: String,
        /// Override the Boss state root.
        #[arg(long)]
        state_root: Option<PathBuf>,
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
    Pause,
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
    Pause,
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

/// Which engine log file `bossctl logs` should read.
#[derive(clap::ValueEnum, Debug, Clone, PartialEq)]
pub(crate) enum LogSource {
    /// `engine-trace.jsonl` — structured tracing events (primary log).
    Engine,
    /// `engine-audit.log` — lifecycle events (start, socket bind, shutdown).
    Audit,
}

impl std::fmt::Display for LogSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogSource::Engine => write!(f, "engine"),
            LogSource::Audit => write!(f, "audit"),
        }
    }
}

#[derive(Subcommand, Debug)]
enum AgentsAction {
    /// List worker sessions and their current state.
    List {
        /// Also include "husk" panes — slots the app currently hosts
        /// a session in that the engine has no live-tracked run for
        /// (crash, terminal-fail path bug, spawn-ack timeout).
        /// Invisible on the default view; this is what `retire-pane`
        /// targets, and what causes a `SlotBusy` dispatch rejection.
        #[arg(long)]
        all: bool,
    },
    /// Show detailed status for a single worker. Falls back to the
    /// historical run record if the reference is a run id that is no
    /// longer live.
    Status {
        /// Worker reference: run id, slot id, or crew name (e.g.
        /// `Riker`). Crew names resolve only over currently-live
        /// slots; case-insensitive.
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
        /// slot id, or crew name. For completed executions, pass the
        /// execution id shown by `bossctl agents status <exec_id>`.
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
    /// The run id MUST be passed explicitly (no slot-id / crew-name
    /// fallback): the live-worker registry is the source for the
    /// fallbacks and an orphaned worker is by definition not in it.
    Reap {
        /// Execution / run id of the orphaned worker (e.g.
        /// `exec_18ad6336fedcb190_12`). Look this up with `bossctl
        /// workspace summary` or `boss chore show`.
        run_id: String,
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
    /// Break-glass: tear down a "husk" pane — a worker pane the app
    /// still hosts that the engine has NO live-tracked run for (crash,
    /// terminal-fail path bug, spawn-ack timeout). Neither `stop` nor
    /// `reap` can reach this case: both resolve a run id through the
    /// live-worker registry, and the engine has already dropped that
    /// mapping for a husk — `stop` fails client-side with "no live
    /// worker matches" before it ever talks to the engine.
    ///
    /// Refuses if the engine's own live-worker registry still shows a
    /// live run in the slot — that pane is not a husk; use `agents
    /// stop` for it instead. Use `agents list --all` to see which
    /// slots are husks before retiring one.
    RetirePane {
        /// Slot id to retire (1-indexed, matches the app's Workers
        /// grid numbering and `agents list --all` output).
        slot_id: u8,
    },
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
    },
    /// Cancel a queued or running execution.
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
}

#[derive(Subcommand, Debug)]
enum WorkspaceAction {
    /// Summarize cube workspace pool state.
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
    /// Register a new remote host. The host is enabled immediately and
    /// persisted to `state.db`. Phase 3 eagerly pushes the
    /// `boss-remote-run` wrapper to the host as part of registration;
    /// pass `--skip-wrapper-push` to suppress that (offline / dry-run /
    /// test fixtures).
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
    /// Delete terminal (`abandoned` / `failed` / `orphaned` / `cancelled`)
    /// `work_executions` rows past the retention bound. `completed`
    /// executions are never touched. Always keeps the most recent
    /// `--keep-per-work-item` eligible rows per work item regardless of
    /// age, so recent diagnostics survive.
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
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("bossctl: {err:#}");
            ExitCode::from(1)
        }
    }
}

async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Probe { agent, text, urgent } => probe_run(&cli.socket_path, cli.json, agent, text, urgent).await,
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
            action: AgentsAction::Reap { run_id },
        } => agents::agents_reap(&cli.socket_path, cli.json, run_id).await,
        Command::Agents {
            action: AgentsAction::Pools,
        } => agents::agents_pools(&cli.socket_path, cli.json).await,
        Command::Agents {
            action: AgentsAction::RetirePane { slot_id },
        } => agents::agents_retire_pane(&cli.socket_path, cli.json, slot_id).await,
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
                },
        } => {
            agents::work_start(
                &cli.socket_path,
                cli.json,
                work_item_id,
                priority,
                preferred_workspace_id,
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
        Command::Workspace {
            action: WorkspaceAction::Summary,
        } => workspace_summary(&cli.socket_path, cli.json).await,
        Command::Review {
            action: review::ReviewAction::Start { pr_number, repo },
        } => review::review_start(&cli.socket_path, cli.json, pr_number, repo).await,
        Command::LiveStatus {
            action: LiveStatusAction::Debug,
        } => live_status_debug(&cli.socket_path, cli.json).await,
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
            action: DispatchAction::Diagnose {
                execution_id,
                state_root,
            },
        } => dispatch_diagnose(cli.json, state_root, &execution_id),
        Command::Dispatch {
            action:
                DispatchAction::GhostActive {
                    state_root,
                    stalled_after_secs,
                    include_stalled,
                },
        } => dispatch_ghost_active(cli.json, state_root, stalled_after_secs, include_stalled),
        Command::Dispatch {
            action: DispatchAction::Pause,
        } => dispatch_set_paused(&cli.socket_path, cli.json, true).await,
        Command::Dispatch {
            action: DispatchAction::Resume,
        } => dispatch_set_paused(&cli.socket_path, cli.json, false).await,
        Command::Dispatch {
            action: DispatchAction::State { history, n, state_root },
        } => {
            if history {
                dispatch_pause_history(cli.json, state_root, n)
            } else {
                dispatch_state(&cli.socket_path, cli.json).await
            }
        }
        Command::Dispatch {
            action: DispatchAction::Stats { state_root, since, top },
        } => dispatch_stats::dispatch_stats(cli.json, state_root, since.as_deref(), top),
        Command::Automation {
            action: AutomationAction::Pause,
        } => automation_set_paused(&cli.socket_path, cli.json, true).await,
        Command::Automation {
            action: AutomationAction::Resume,
        } => automation_set_paused(&cli.socket_path, cli.json, false).await,
        Command::Automation {
            action: AutomationAction::State,
        } => automation_state(&cli.socket_path, cli.json).await,
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
                ExecutionsAction::Prune {
                    older_than_days,
                    keep_per_work_item,
                    dry_run,
                    state_root,
                },
        } => executions_prune(cli.json, state_root, older_than_days, keep_per_work_item, dry_run),
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
        Command::Reveal { id } => agents::reveal_work_item(&cli.socket_path, cli.json, id).await,
        Command::Open { path } => agents::open_document(&cli.socket_path, cli.json, path).await,
        Command::Logs {
            source,
            tail,
            follow,
            grep,
            state_root,
        } => {
            if follow {
                logs::logs_follow(source, state_root, tail, grep).await
            } else {
                logs::logs_tail(cli.json, source, state_root, tail, grep.as_deref())
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
    let events = dispatch_reader::read_current(&root)?;
    let slice = filter_and_tail(&events, n, stage_filter.as_deref(), outcome_filter.as_deref());

    if json {
        println!("{}", build_tail_json(slice));
    } else if slice.is_empty() {
        println!("no dispatch events");
    } else {
        for event in slice {
            print_dispatch_event_short(event);
        }
    }
    Ok(())
}

fn dispatch_diagnose(json: bool, state_root: Option<PathBuf>, execution_id: &str) -> Result<()> {
    let root = resolve_state_root(state_root)?;
    let events = dispatch_reader::read_execution(&root, execution_id)?;
    if events.is_empty() {
        if json {
            println!("{}", build_diagnose_json(execution_id, &[], &[]));
        } else {
            println!("no dispatch events recorded for execution {execution_id}");
        }
        return Ok(());
    }
    let now = now_epoch_ms();
    let durations = dispatch_reader::stage_durations_ms(&events, now);

    if json {
        println!("{}", build_diagnose_json(execution_id, &events, &durations));
    } else {
        println!("dispatch timeline for execution {execution_id}");
        for (event, duration_ms) in events.iter().zip(durations.iter()) {
            print_dispatch_event_detailed(event, *duration_ms);
        }
    }
    Ok(())
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
    let mut entries = dispatch_reader::ghost_active(&root, now, threshold_ms)?;
    if include_stalled {
        entries.retain(|e| e.stalled);
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "ghost_active": entries,
            })
        );
    } else if entries.is_empty() {
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

async fn dispatch_set_paused(socket_path: &Option<String>, json: bool, paused: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::SetDispatchPaused { paused })
        .await
        .context("sending SetDispatchPaused")?;
    match response {
        FrontendEvent::DispatchStateResult {
            paused: new_paused,
            paused_since_epoch_s,
            reviews_exempt,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "paused": new_paused,
                        "paused_since_epoch_s": paused_since_epoch_s,
                        "reviews_exempt": reviews_exempt,
                    })
                );
            } else if new_paused {
                let since_str = paused_since_epoch_s
                    .map(|s| format!(" (since epoch {s})"))
                    .unwrap_or_default();
                let exempt_str = if reviews_exempt {
                    " — PR reviews are exempt and keep dispatching"
                } else {
                    " — PR reviews are held too (spawn-capability breaker)"
                };
                println!("dispatch paused{since_str}{exempt_str}");
            } else {
                println!("dispatch resumed");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected SetDispatchPaused: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn dispatch_state(socket_path: &Option<String>, json: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::GetDispatchState)
        .await
        .context("sending GetDispatchState")?;
    match response {
        FrontendEvent::DispatchStateResult {
            paused,
            paused_since_epoch_s,
            reviews_exempt,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "paused": paused,
                        "paused_since_epoch_s": paused_since_epoch_s,
                        "reviews_exempt": reviews_exempt,
                    })
                );
            } else if paused {
                let since_str = paused_since_epoch_s
                    .map(|s| format!("  paused_since: epoch {s}"))
                    .unwrap_or_default();
                println!("state: paused");
                if !since_str.is_empty() {
                    println!("{since_str}");
                }
                if reviews_exempt {
                    println!("  reviews: exempt — PR-review executions keep dispatching");
                } else {
                    println!("  reviews: held — spawn-capability breaker pause");
                }
            } else {
                println!("state: running");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected GetDispatchState: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn automation_set_paused(socket_path: &Option<String>, json: bool, paused: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::SetAutomationPaused { paused })
        .await
        .context("sending SetAutomationPaused")?;
    match response {
        FrontendEvent::AutomationStateResult {
            paused: new_paused,
            paused_since_epoch_s,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "paused": new_paused,
                        "paused_since_epoch_s": paused_since_epoch_s,
                    })
                );
            } else if new_paused {
                let since_str = paused_since_epoch_s
                    .map(|s| format!(" (since epoch {s})"))
                    .unwrap_or_default();
                println!(
                    "automation paused{since_str} — new triage passes and automation-pool spawns are held; \
                     already-running automation workers finish normally"
                );
            } else {
                println!("automation resumed");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected SetAutomationPaused: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn automation_state(socket_path: &Option<String>, json: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::GetAutomationState)
        .await
        .context("sending GetAutomationState")?;
    match response {
        FrontendEvent::AutomationStateResult {
            paused,
            paused_since_epoch_s,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "paused": paused,
                        "paused_since_epoch_s": paused_since_epoch_s,
                    })
                );
            } else if paused {
                let since_str = paused_since_epoch_s
                    .map(|s| format!("  paused_since: epoch {s}"))
                    .unwrap_or_default();
                println!("state: paused");
                if !since_str.is_empty() {
                    println!("{since_str}");
                }
            } else {
                println!("state: running");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected GetAutomationState: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// Print recent dispatch pause/resume episodes with their full audit
/// evidence — file-scan only over `dispatch-events/current.jsonl` (same
/// pattern as `dispatch tail`/`diagnose`), so it works even when the engine
/// is wedged and doesn't depend on the live RPC state `dispatch state`
/// reads. Surfaces both operator (`dispatch_paused`/`dispatch_resumed`,
/// `origin: "operator"`) and breaker-originated (`origin: "breaker"`,
/// carrying the full `trigger` evidence — which executions/work
/// items/slots failed to spawn, over what window, against which
/// threshold) episodes in one place.
fn dispatch_pause_history(json: bool, state_root: Option<PathBuf>, n: usize) -> Result<()> {
    let root = resolve_state_root(state_root)?;
    let events = dispatch_reader::read_current(&root)?;
    let mut episodes: Vec<&DispatchEvent> = events
        .iter()
        .filter(|e| e.stage == "dispatch_paused" || e.stage == "dispatch_resumed")
        .collect();
    episodes.reverse();
    episodes.truncate(n);

    if json {
        let value: Vec<serde_json::Value> = episodes
            .iter()
            .map(|e| {
                serde_json::json!({
                    "ts_epoch_ms": e.ts_epoch_ms,
                    "stage": e.stage,
                    "execution_id": e.execution_id,
                    "work_item_id": e.work_item_id,
                    "details": e.details,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&value)?);
        return Ok(());
    }

    if episodes.is_empty() {
        println!("no pause/resume episodes recorded");
        return Ok(());
    }

    for event in episodes {
        let kind = if event.stage == "dispatch_paused" {
            "PAUSED"
        } else {
            "RESUMED"
        };
        let origin = event.details["origin"].as_str().unwrap_or("unknown");
        let actor = event.details["actor"].as_str().unwrap_or("unknown");
        println!(
            "{kind}  ts_epoch_ms={} origin={origin} actor={actor}",
            event.ts_epoch_ms
        );
        if let Some(reason) = event.details["reason"].as_str() {
            println!("  reason: {reason}");
        }
        if let Some(scope) = event.details["scope"].as_array() {
            let scope: Vec<&str> = scope.iter().filter_map(|v| v.as_str()).collect();
            println!("  scope: {}", scope.join(","));
        }
        if let Some(duration) = event.details["pause_duration_secs"].as_u64() {
            println!("  pause_duration_secs: {duration}");
        }
        let trigger = &event.details["trigger"];
        if !trigger.is_null() {
            println!("  trigger rule: {}", trigger["rule"].as_str().unwrap_or(""));
            if let Some(triggering) = trigger["triggering_events"].as_array() {
                println!("  triggering events ({}):", triggering.len());
                for ev in triggering {
                    println!(
                        "    execution_id={} work_item_id={} slot_id={} shell_pid={} epoch_secs={}",
                        ev["execution_id"].as_str().unwrap_or(""),
                        ev["work_item_id"].as_str().unwrap_or(""),
                        ev["slot_id"].as_str().unwrap_or(""),
                        ev["shell_pid"].as_i64().unwrap_or(0),
                        ev["epoch_secs"].as_i64().unwrap_or(0),
                    );
                }
            }
        }
    }
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

fn build_diagnose_json(execution_id: &str, events: &[DispatchEvent], durations: &[u128]) -> serde_json::Value {
    let detailed: Vec<serde_json::Value> = events
        .iter()
        .zip(durations.iter())
        .map(|(event, dur)| {
            let mut value = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
            if let Some(obj) = value.as_object_mut() {
                obj.insert("stage_duration_ms".into(), serde_json::Value::from(*dur as u64));
            }
            value
        })
        .collect();
    serde_json::json!({
        "execution_id": execution_id,
        "events": detailed,
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

fn print_dispatch_event_detailed(event: &DispatchEvent, stage_duration_ms: u128) {
    let worker = event.worker_id.as_deref().unwrap_or("-");
    println!(
        "  {}  {}/{}  +{}ms  worker={}",
        event.ts_epoch_ms, event.stage, event.outcome, stage_duration_ms, worker,
    );
    // Decode the claimed slot + interactive page from the worker id so a
    // Lower Decks spawn/claim failure is distinguishable from a Bridge Crew
    // one at a glance. Automation/review workers resolve a slot but no page.
    if let Some(worker_id) = &event.worker_id
        && let Some(slot) = boss_engine::coordinator::slot_id_from_worker_id(worker_id)
    {
        match boss_engine::coordinator::worker_page_label(slot) {
            Some(page) => println!("    page:      {page} (slot {slot})"),
            None => println!("    slot:      {slot}"),
        }
    }
    if let Some(lease) = &event.cube_lease_id {
        println!("    lease:     {lease}");
    }
    if let Some(workspace) = &event.cube_workspace_id {
        println!("    workspace: {workspace}");
    }
    if let Some(err) = &event.error_message {
        println!("    error:     {err}");
    }
    if !event.details.is_null() {
        match serde_json::to_string(&event.details) {
            Ok(text) => println!("    details:   {text}"),
            Err(_) => println!("    details:   <unserializable>"),
        }
    }
}

async fn connect(socket_path: &Option<String>) -> Result<BossClient> {
    let discovery = Discovery::from_env(socket_path.as_deref()).context("resolving engine discovery profile")?;
    BossClient::connect(&discovery).await.context("connecting to engine")
}

async fn probe_run(socket_path: &Option<String>, json: bool, agent: String, text: String, urgent: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = agents::fetch_live_states(&mut client).await?;
    let run_id = agents::resolve_agent_ref(&agent, &states)?.run_id.clone();
    let response = client
        .send_request(&FrontendRequest::ProbeRun {
            run_id: run_id.clone(),
            text,
            urgent,
        })
        .await
        .context("sending ProbeRun")?;
    match response {
        FrontendEvent::ProbeQueued {
            run_id: returned,
            probe_id,
            urgent: is_urgent,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": if is_urgent { "urgent" } else { "queued" },
                        "run_id": returned,
                        "probe_id": probe_id,
                        "urgent": is_urgent,
                    })
                );
            } else if is_urgent {
                println!(
                    "urgent probe queued for run {returned} (probe_id={probe_id}); will inject at next tool boundary"
                );
            } else {
                println!("probe queued for run {returned} (probe_id={probe_id})");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected probe: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
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
                println!("no workspaces in cube pool");
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
            "deleted {} terminal execution row(s) older than {}d (kept {} most recent per work item)",
            outcome.deleted, older_than_days, keep_per_work_item
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
    println!("  engine_build_sha:           {}", report.engine_build_sha);
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
