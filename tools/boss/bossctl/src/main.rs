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

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use boss_engine::host_registry::{Host, HostCapability};
use boss_engine::work::WorkDb;

use anyhow::{Context, Result, bail};
use boss_client::{BossClient, Discovery};

mod comments;
mod dispatch_stats;
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
    Pause,
    /// Resume global dispatch. The engine immediately drains any executions
    /// that queued while paused and resumes normal dispatch. Idempotent —
    /// resuming while already running is a no-op.
    Resume,
    /// Show the current dispatch-pause state (paused/running and, if paused,
    /// when it was paused).
    State,
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
    List,
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
        } => agents_status(&cli.socket_path, cli.json, agent).await,
        Command::Agents {
            action: AgentsAction::List,
        } => agents_list_live(&cli.socket_path, cli.json).await,
        Command::Agents {
            action: AgentsAction::Stop { agent },
        } => agents_stop(&cli.socket_path, cli.json, agent).await,
        Command::Agents {
            action: AgentsAction::Focus { agent },
        } => agents_focus(&cli.socket_path, cli.json, agent).await,
        Command::Agents {
            action: AgentsAction::Send { agent, text },
        } => agents_send(&cli.socket_path, cli.json, agent, text).await,
        Command::Agents {
            action: AgentsAction::Interrupt { agent },
        } => agents_interrupt(&cli.socket_path, cli.json, agent).await,
        Command::Agents {
            action:
                AgentsAction::Transcript {
                    agent,
                    lines,
                    format,
                    no_tools,
                },
        } => agents_transcript(&cli.socket_path, cli.json, agent, lines, format, no_tools).await,
        Command::Agents {
            action: AgentsAction::Reap { run_id },
        } => agents_reap(&cli.socket_path, cli.json, run_id).await,
        Command::Agents {
            action: AgentsAction::Pools,
        } => agents_pools(&cli.socket_path, cli.json).await,
        Command::Agents {
            action:
                AgentsAction::Launch {
                    work_item_id,
                    preferred_workspace_id,
                },
        } => agents_launch(&cli.socket_path, cli.json, work_item_id, preferred_workspace_id).await,
        Command::Work {
            action:
                WorkAction::Start {
                    work_item_id,
                    priority,
                    preferred_workspace_id,
                },
        } => {
            work_start(
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
        } => work_cancel(&cli.socket_path, cli.json, execution_id).await,
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
            action: DispatchAction::State,
        } => dispatch_state(&cli.socket_path, cli.json).await,
        Command::Dispatch {
            action: DispatchAction::Stats { state_root, since, top },
        } => dispatch_stats::dispatch_stats(cli.json, state_root, since.as_deref(), top),
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
        } => hosts_add(cli.json, state_root, id, ssh_target, pool_size, tags, skip_wrapper_push).await,
        Command::Hosts {
            action: HostsAction::List { enabled, state_root },
        } => hosts_list(cli.json, state_root, enabled),
        Command::Hosts {
            action: HostsAction::Show { id, state_root },
        } => hosts_show(cli.json, state_root, id),
        Command::Hosts {
            action:
                HostsAction::Tag {
                    action: HostsTagAction::Add { id, tags, state_root },
                },
        } => hosts_tag_add(cli.json, state_root, id, tags),
        Command::Hosts {
            action:
                HostsAction::Tag {
                    action: HostsTagAction::Remove { id, tags, state_root },
                },
        } => hosts_tag_remove(cli.json, state_root, id, tags),
        Command::Hosts {
            action: HostsAction::Enable { id, state_root },
        } => hosts_set_enabled(cli.json, state_root, id, true),
        Command::Hosts {
            action: HostsAction::Disable { id, state_root },
        } => hosts_set_enabled(cli.json, state_root, id, false),
        Command::Hosts {
            action: HostsAction::Remove { id, state_root },
        } => hosts_remove(cli.json, state_root, id),
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
        Command::Reveal { id } => reveal_work_item(&cli.socket_path, cli.json, id).await,
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
                println!("dispatch paused{since_str}");
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
            bail!("engine rejected GetDispatchState: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
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

/// Resolve a positional `agent` argument to a live worker entry.
///
/// Tries, in order: (a) exact match on `run_id`, (b) exact match on
/// numeric `slot_id`, (c) case-insensitive exact match on crew
/// `name`. The first non-empty tier wins; an ambiguous tier (more
/// than one match) errors with the candidate list.
///
/// Names resolve only over currently-live slots — historical run
/// ids stay run-id-only on purpose, so a typo'd crew name doesn't
/// silently match a closed run.
fn resolve_agent_ref<'a>(reference: &str, states: &'a [LiveWorkerState]) -> Result<&'a LiveWorkerState> {
    let by_run: Vec<&LiveWorkerState> = states.iter().filter(|s| s.run_id == reference).collect();
    if !by_run.is_empty() {
        return pick_unique(reference, by_run, states);
    }
    if let Ok(slot) = reference.parse::<u8>() {
        let by_slot: Vec<&LiveWorkerState> = states.iter().filter(|s| s.slot_id == slot).collect();
        if !by_slot.is_empty() {
            return pick_unique(reference, by_slot, states);
        }
    }
    let by_name: Vec<&LiveWorkerState> = states
        .iter()
        .filter(|s| s.name.eq_ignore_ascii_case(reference))
        .collect();
    if !by_name.is_empty() {
        return pick_unique(reference, by_name, states);
    }
    bail!(
        "no live worker matches `{reference}`. {}",
        live_candidates_summary(states),
    )
}

fn pick_unique<'a>(
    reference: &str,
    matches: Vec<&'a LiveWorkerState>,
    states: &'a [LiveWorkerState],
) -> Result<&'a LiveWorkerState> {
    if matches.len() == 1 {
        return Ok(matches[0]);
    }
    bail!(
        "`{reference}` matches multiple live workers: {}. {}",
        matches
            .iter()
            .map(|s| format!("slot {} ({}) run {}", s.slot_id, s.name, s.run_id))
            .collect::<Vec<_>>()
            .join(", "),
        live_candidates_summary(states),
    )
}

fn live_candidates_summary(states: &[LiveWorkerState]) -> String {
    if states.is_empty() {
        return "no live workers".into();
    }
    let mut sorted: Vec<&LiveWorkerState> = states.iter().collect();
    sorted.sort_by_key(|s| s.slot_id);
    let labels: Vec<String> = sorted
        .iter()
        .map(|s| format!("slot {} ({})", s.slot_id, s.name))
        .collect();
    format!("Live: {}", labels.join(", "))
}

/// True if `reference` looks like a name or numeric slot id (so a
/// resolver miss should be terminal rather than falling back to a
/// historical run-id lookup). A run id like `exec_18ad...` falls
/// through both checks.
fn looks_like_name_or_slot(reference: &str) -> bool {
    if reference.parse::<u8>().is_ok() {
        return true;
    }
    ROSTER.iter().any(|name| name.eq_ignore_ascii_case(reference))
}

/// If `selector` looks like a friendly work-item id (`T42`, `t42`, `P7`,
/// `p7`), resolve it to the primary id via the engine and search `states`
/// for a live worker running that work item. Returns the matching state,
/// or `None` when the selector isn't a friendly-id form or no live worker
/// is found for the resolved item.
async fn resolve_tnnn_to_live_worker<'a>(
    client: &mut BossClient,
    selector: &str,
    states: &'a [LiveWorkerState],
) -> Result<Option<&'a LiveWorkerState>> {
    if selector.len() < 2 {
        return Ok(None);
    }
    let first = selector.as_bytes()[0];
    if first != b'T' && first != b't' && first != b'P' && first != b'p' {
        return Ok(None);
    }
    let n: i64 = match selector[1..].parse() {
        Ok(n) if n > 0 => n,
        _ => return Ok(None),
    };
    let products = match client
        .send_request(&FrontendRequest::ListProducts)
        .await
        .context("listing products for friendly-id resolution")?
    {
        FrontendEvent::ProductsList { products } => products,
        _ => return Ok(None),
    };
    for product in &products {
        let item = match client
            .send_request(&FrontendRequest::GetWorkItemByShortId {
                product_id: product.id.clone(),
                short_id: n,
            })
            .await
            .context("resolving friendly id")?
        {
            FrontendEvent::WorkItemResult { item } => item,
            _ => continue,
        };
        let primary_id = match &item {
            WorkItem::Product(p) => p.id.as_str(),
            WorkItem::Project(p) => p.id.as_str(),
            WorkItem::Task(t) | WorkItem::Chore(t) => t.id.as_str(),
        };
        if let Some(state) = states.iter().find(|s| s.work_item_id.as_deref() == Some(primary_id)) {
            return Ok(Some(state));
        }
    }
    Ok(None)
}

/// Resolve `reference` to a live worker's run id, accepting run ids,
/// slot ids, crew names, and friendly work-item ids (T42, P7). Falls
/// back to the original `resolve_agent_ref` error when no match is found.
async fn resolve_agent_ref_or_work_item(
    client: &mut BossClient,
    reference: &str,
    states: &[LiveWorkerState],
) -> Result<String> {
    match resolve_agent_ref(reference, states) {
        Ok(state) => Ok(state.run_id.clone()),
        Err(agent_err) => {
            if let Some(state) = resolve_tnnn_to_live_worker(client, reference, states).await? {
                return Ok(state.run_id.clone());
            }
            Err(agent_err)
        }
    }
}

async fn fetch_live_states(client: &mut BossClient) -> Result<Vec<LiveWorkerState>> {
    match client
        .send_request(&FrontendRequest::ListWorkerLiveStates)
        .await
        .context("sending ListWorkerLiveStates")?
    {
        FrontendEvent::WorkerLiveStatesList { states } => Ok(states),
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected list: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn probe_run(socket_path: &Option<String>, json: bool, agent: String, text: String, urgent: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref(&agent, &states)?.run_id.clone();
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

/// Show live runtime status for the worker referenced by `agent`
/// (run id, slot id, or crew name). Falls back to the finalised
/// `WorkRun` record (the historical snapshot the engine persists
/// once the run row finalises) when the reference looks like a
/// run id and no matching live entry is found — so the verb still
/// works for runs that have already terminated. Crew-name and
/// slot-id references that miss are *not* fall through to the
/// historical lookup; they error with the live candidate list to
/// avoid silently matching a typo against a closed run.
async fn agents_status(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;

    match resolve_agent_ref(&agent, &states) {
        Ok(state) => {
            print_live_state(json, state);
            return Ok(());
        }
        Err(err) if looks_like_name_or_slot(&agent) => return Err(err),
        Err(_) => {}
    }

    // No live entry and the reference doesn't look like a name or
    // slot — assume it's a historical run id.
    let response = client
        .send_request(&FrontendRequest::GetRun { id: agent.clone() })
        .await
        .context("sending GetRun")?;
    let run = match response {
        FrontendEvent::RunResult { run } => run,
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected status: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    };

    // `run` is the `work_runs` row for the pane-spawn task, not the
    // worker's lifecycle — every healthy spawn finalises that row
    // within ~5-8s regardless of how long the worker actually runs
    // (see the module docs on `LiveWorkerState`). Reporting `run`
    // alone reads as "an 8-second run" even when the worker is alive
    // and working minutes later. Resolve the owning execution and
    // report on *that*: prefer the live worker state if the worker is
    // still up, otherwise the execution's own status/timestamps.
    let live = states
        .iter()
        .find(|s| s.execution_id.as_deref() == Some(run.execution_id.as_str()) || s.run_id == run.execution_id);

    let execution = if live.is_some() {
        None
    } else {
        match client
            .send_request(&FrontendRequest::GetExecution {
                id: run.execution_id.clone(),
            })
            .await
            .context("sending GetExecution")?
        {
            FrontendEvent::ExecutionResult { execution } => Some(execution),
            _ => None,
        }
    };

    print_run_lifecycle(json, &run, live, execution.as_ref());
    Ok(())
}

/// Renders a historical `GetRun` lookup alongside the worker's actual
/// lifecycle rather than just the pane-spawn task row. See the
/// `agents_status` doc comment above for why the two can diverge
/// wildly (a `completed`, 8-second `run` next to a worker still alive
/// 13+ minutes later). When `live` is `Some`, the worker is still up
/// and its `LiveWorkerState` (with an authoritative `shell_pid`, not
/// the possibly-stale `shell_pid 0` baked into the spawn row's
/// `result_summary` text) is the source of truth. Otherwise `execution`
/// carries the execution's own terminal status/timestamps, when the
/// engine could resolve it.
fn print_run_lifecycle(json: bool, run: &WorkRun, live: Option<&LiveWorkerState>, execution: Option<&WorkExecution>) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "pane_spawn_run": run,
                "note": "pane_spawn_run is the pane-spawn task record only; it finalises within \
                         seconds of every healthy spawn and does not reflect the worker's \
                         lifecycle. Use live_worker_state (if present) or execution for that.",
                "live_worker_state": live,
                "execution": execution,
            })
        );
        return;
    }

    println!("run {} (pane-spawn step only — not the worker lifecycle)", run.id);
    println!("  execution:     {}", run.execution_id);
    println!("  spawn status:  {}", run.status);
    if let Some(s) = &run.started_at {
        println!("  spawn started: {s}");
    }
    if let Some(f) = &run.finished_at {
        println!("  spawn finished:{f}");
    }

    match live {
        Some(state) => {
            println!();
            println!("worker is live — actual state:");
            print_live_state(false, state);
        }
        None => match execution {
            Some(exec) => {
                println!();
                println!("worker lifecycle (execution {}):", exec.id);
                println!("  status:   {}", exec.status.as_str());
                if let Some(s) = &exec.started_at {
                    println!("  started:  {s}");
                }
                if let Some(f) = &exec.finished_at {
                    println!("  finished: {f}");
                }
            }
            None => {
                println!(
                    "  (could not resolve owning execution {} for worker lifecycle)",
                    run.execution_id
                );
            }
        },
    }
}

/// List every live worker slot (model, activity, current tool, last
/// event time). Unlike the previous `agents list`, this is sourced
/// from the engine's in-memory LiveWorkerState rather than from the
/// finalised WorkRun records — those finalise within ~1s of spawn
/// and don't reflect the live worker.
async fn agents_list_live(socket_path: &Option<String>, json: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "live_worker_states": states,
            })
        );
    } else if states.is_empty() {
        println!("no active workers");
    } else {
        for state in &states {
            print_live_state_short(state);
        }
    }
    Ok(())
}

async fn agents_stop(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref_or_work_item(&mut client, &agent, &states).await?;
    let response = client
        .send_request(&FrontendRequest::StopRun { run_id: run_id.clone() })
        .await
        .context("sending StopRun")?;
    match response {
        FrontendEvent::RunStopped { run_id: returned } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "stopped",
                        "run_id": returned,
                    })
                );
            } else {
                println!("stopped run {returned}");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected stop: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn agents_focus(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref_or_work_item(&mut client, &agent, &states).await?;
    let response = client
        .send_request(&FrontendRequest::FocusWorkerPane { run_id: run_id.clone() })
        .await
        .context("sending FocusWorkerPane")?;
    match response {
        FrontendEvent::WorkerPaneFocused {
            run_id: returned,
            slot_id,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "focused",
                        "run_id": returned,
                        "slot_id": slot_id,
                    })
                );
            } else {
                println!("focused slot {slot_id} (run {returned})");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected focus: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn reveal_work_item(socket_path: &Option<String>, json: bool, id: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::RevealWorkItem { id: id.clone() })
        .await
        .context("sending RevealWorkItem")?;
    match response {
        FrontendEvent::WorkItemRevealed { id: canonical_id } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "revealed",
                        "id": canonical_id,
                    })
                );
            } else {
                println!("revealed {canonical_id}");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected reveal: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// Inject `text` into the worker pane referenced by `agent`, as if
/// the user had typed it and pressed Return. The submit step is the
/// app-side writer's responsibility: after pasting the body via
/// libghostty's text path it synthesises a Return keystroke, which
/// is what makes the prompt land. Earlier revisions of this CLI
/// appended a trailing `\n` here in the hope that the paste path
/// would treat it as Enter; it does not (the `\n` lands as a literal
/// newline character in the input field), so the writer owns
/// submission now and the CLI ships the text verbatim.
async fn agents_send(socket_path: &Option<String>, json: bool, agent: String, text: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref_or_work_item(&mut client, &agent, &states).await?;
    let response = client
        .send_request(&FrontendRequest::SendInputToWorker {
            run_id: run_id.clone(),
            text,
        })
        .await
        .context("sending SendInputToWorker")?;
    match response {
        FrontendEvent::WorkerInputSent {
            run_id: returned,
            slot_id,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "sent",
                        "run_id": returned,
                        "slot_id": slot_id,
                    })
                );
            } else {
                println!("sent input to slot {slot_id} (run {returned})");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected send: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// Interrupt the worker referenced by `agent` — equivalent to the
/// human pressing Esc inside that worker's pane. Cancels the
/// in-flight turn without killing the run.
async fn agents_interrupt(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref_or_work_item(&mut client, &agent, &states).await?;
    let response = client
        .send_request(&FrontendRequest::InterruptWorkerPane { run_id: run_id.clone() })
        .await
        .context("sending InterruptWorkerPane")?;
    match response {
        FrontendEvent::WorkerPaneInterrupted {
            run_id: returned,
            slot_id,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "interrupted",
                        "run_id": returned,
                        "slot_id": slot_id,
                    })
                );
            } else {
                println!("interrupted slot {slot_id} (run {returned})");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected interrupt: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// Skip-the-queue spawn for `bossctl agents launch <work-item-id>`.
/// Maps to `RequestExecution { force: true, .. }`: the engine grows
/// the worker pool by one slot up to the hard cap when every
/// configured slot is busy and dispatches the work item immediately,
/// rather than letting the auto-dispatcher defer until a slot frees
/// up.
async fn agents_launch(
    socket_path: &Option<String>,
    json: bool,
    work_item_id: String,
    preferred_workspace_id: Option<String>,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::RequestExecution {
            input: RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .maybe_preferred_workspace_id(preferred_workspace_id)
                .force(true)
                .build(),
        })
        .await
        .context("sending RequestExecution (force)")?;
    match response {
        FrontendEvent::ExecutionRequested { execution }
        | FrontendEvent::ExecutionCreated { execution }
        | FrontendEvent::ExecutionResult { execution } => {
            print_execution(json, &execution);
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected agents launch: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn work_start(
    socket_path: &Option<String>,
    json: bool,
    work_item_id: String,
    priority: Option<i64>,
    preferred_workspace_id: Option<String>,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::RequestExecution {
            input: RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .maybe_priority(priority)
                .maybe_preferred_workspace_id(preferred_workspace_id)
                .build(),
        })
        .await
        .context("sending RequestExecution")?;
    match response {
        FrontendEvent::ExecutionRequested { execution }
        | FrontendEvent::ExecutionCreated { execution }
        | FrontendEvent::ExecutionResult { execution } => {
            print_execution(json, &execution);
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected work start: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn work_cancel(socket_path: &Option<String>, json: bool, execution_id: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::CancelExecution {
            execution_id: execution_id.clone(),
        })
        .await
        .context("sending CancelExecution")?;
    match response {
        FrontendEvent::ExecutionCancelled { execution } => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&execution).expect("WorkExecution serializes")
                );
            } else {
                println!("cancelled execution {}", execution.id);
                println!("  status:    {}", execution.status);
                if let Some(f) = &execution.finished_at {
                    println!("  finished:  {f}");
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected work cancel: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn agents_transcript(
    socket_path: &Option<String>,
    json: bool,
    agent: String,
    lines: usize,
    format: TranscriptFormat,
    no_tools: bool,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;

    // For live workers resolve via the registry. For completed/terminal
    // executions the live registry has no entry — fall through and let
    // the engine query work_runs.transcript_path from the DB. The
    // engine's resolve_transcript_for_tail handles both the exec_* and
    // run_* namespaces, so passing the raw ref works for either id form.
    // Friendly ids (T42) are tried as live-worker references first.
    let run_id = match resolve_agent_ref(&agent, &states) {
        Ok(state) => state.run_id.clone(),
        Err(err) if looks_like_name_or_slot(&agent) => return Err(err),
        Err(_) => {
            if let Some(state) = resolve_tnnn_to_live_worker(&mut client, &agent, &states).await? {
                state.run_id.clone()
            } else {
                agent.clone()
            }
        }
    };

    let response = client
        .send_request(&FrontendRequest::TailRunTranscript {
            run_id: run_id.clone(),
            lines,
        })
        .await
        .context("sending TailRunTranscript")?;
    match response {
        FrontendEvent::RunTranscriptTail {
            run_id: returned,
            transcript_path,
            lines: tail,
            truncated,
        } => {
            let render_opts = boss_engine::transcript_markdown::RenderOpts {
                hide_tools: no_tools,
                ..Default::default()
            };
            if format == TranscriptFormat::Text || format == TranscriptFormat::Markdown {
                let joined = tail.join("\n");
                let events = boss_engine::transcript_markdown::parse_transcript(&joined);
                let rendered = if format == TranscriptFormat::Markdown {
                    let segments = boss_engine::transcript_markdown::events_to_segments(&events, &render_opts);
                    boss_engine::transcript_markdown::segments_to_markdown(&segments)
                } else {
                    boss_engine::transcript_markdown::render_text(&events, &render_opts)
                };
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "run_id": returned,
                            "transcript_path": transcript_path,
                            "rendered": rendered,
                            "truncated": truncated,
                        })
                    );
                } else {
                    if truncated {
                        println!(
                            "transcript {transcript_path} (showing last {} lines; older content omitted)",
                            tail.len()
                        );
                    } else {
                        println!("transcript {transcript_path} ({} lines)", tail.len());
                    }
                    print!("{rendered}");
                }
                return Ok(());
            }
            // TranscriptFormat::Jsonl — dump raw JSONL lines.
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "run_id": returned,
                        "transcript_path": transcript_path,
                        "lines": tail,
                        "truncated": truncated,
                    })
                );
            } else {
                if truncated {
                    println!(
                        "transcript {transcript_path} (showing last {} lines; older content omitted)",
                        tail.len()
                    );
                } else {
                    println!("transcript {transcript_path} ({} lines)", tail.len());
                }
                for line in tail {
                    println!("{line}");
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected transcript tail: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn agents_reap(socket_path: &Option<String>, json: bool, run_id: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::ReapRun { run_id: run_id.clone() })
        .await
        .context("sending ReapRun")?;
    match response {
        FrontendEvent::RunReaped {
            run_id: returned,
            execution,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "reaped",
                        "run_id": returned,
                        "execution": execution,
                    })
                );
            } else {
                println!("reaped run {returned}");
                println!("  execution:        {}", execution.id);
                println!("  status:           {}", execution.status);
                if let Some(ws) = &execution.cube_workspace_id {
                    println!("  workspace_id:     {ws}  (preserved for re-lease)");
                }
                if let Some(path) = &execution.workspace_path {
                    println!("  workspace_path:   {path}");
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected reap: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn agents_pools(socket_path: &Option<String>, json: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::WorkerPoolSummary)
        .await
        .context("sending WorkerPoolSummary")?;
    match response {
        FrontendEvent::WorkerPoolSummaryResult { pools } => {
            if json {
                println!("{}", serde_json::json!({ "pools": pools }));
            } else {
                for pool in &pools {
                    println!(
                        "{}: {}/{} claimed ({} idle)",
                        pool.name,
                        pool.claims.len(),
                        pool.capacity,
                        pool.idle,
                    );
                    for claim in &pool.claims {
                        let status = claim.execution_status.as_deref().unwrap_or("?");
                        let work_item = claim.work_item_id.as_deref().unwrap_or("-");
                        let flag = if claim.live { "" } else { "  <-- LEAKED?" };
                        println!(
                            "  {}  execution={}  status={}  work_item={}{}",
                            claim.worker_id, claim.execution_id, status, work_item, flag,
                        );
                    }
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected pool summary: {message}")
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

fn print_live_state(json: bool, state: &LiveWorkerState) {
    if json {
        println!("{}", serde_json::to_string(state).expect("LiveWorkerState serializes"));
        return;
    }
    println!("slot {} ({})", state.slot_id, state.name);
    println!("  run:           {}", state.run_id);
    println!("  model:         {}", state.model);
    println!("  activity:      {}", state.activity.as_str());
    println!("  shell_pid:     {}", state.shell_pid);
    if let Some(recovery) = &state.recovery_status {
        println!("  recovery:      {recovery}");
    }
    if let Some(id) = &state.work_item_id {
        println!("  work_item:     {id}");
    }
    if let Some(name) = &state.work_item_name {
        println!("  work_item_name:{name}");
    }
    if let Some(id) = &state.execution_id {
        println!("  execution:     {id}");
    }
    if let Some(tool) = &state.current_tool {
        println!("  current_tool:  {tool}");
    }
    if let Some(ts) = &state.last_event_at {
        println!("  last_event_at: {ts}");
    }
    if let Some(ts) = &state.last_tool_ended_at {
        println!("  last_tool_end: {ts}");
    }
}

fn print_live_state_short(state: &LiveWorkerState) {
    let tool = state.current_tool.as_deref().unwrap_or("-");
    let work_item = state.work_item_id.as_deref().unwrap_or("-");
    let work_item_name = state.work_item_name.as_deref().unwrap_or("-");
    print!(
        "slot {}  name={}  run={}  model={}  activity={}  tool={}  work_item={}  work_item_name=\"{}\"",
        state.slot_id,
        state.name,
        state.run_id,
        state.model,
        state.activity.as_str(),
        tool,
        work_item,
        work_item_name,
    );
    // Surfaced whenever the transient-recovery sweep is actively nudging
    // this slot — without this an auto-recovering worker prints as plain
    // `activity=idle`, indistinguishable from a normally-finished turn.
    if let Some(recovery) = &state.recovery_status {
        print!("  recovery=\"{recovery}\"");
    }
    println!();
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
    let db_path = resolve_db_path(state_root)?;
    let db = WorkDb::open(db_path).context("opening state.db")?;
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
    let db_path = resolve_db_path(state_root)?;
    let db = WorkDb::open(db_path).context("opening state.db")?;
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

// ── bossctl hosts handlers ────────────────────────────────────────────────────

fn open_hosts_db(state_root: Option<PathBuf>) -> Result<WorkDb> {
    let db_path = resolve_db_path(state_root)?;
    WorkDb::open(db_path).context("opening state.db for hosts")
}

async fn hosts_add(
    json: bool,
    state_root: Option<PathBuf>,
    id: String,
    ssh_target: String,
    pool_size: i64,
    tags: Vec<String>,
    skip_wrapper_push: bool,
) -> Result<()> {
    let db = open_hosts_db(state_root)?;
    let host = db.add_host(&id, &ssh_target, pool_size, &tags)?;

    // Phase 3: eagerly push the wrapper unless suppressed. A push
    // failure leaves the host row in place but disabled with the
    // failure cause persisted, matching the design's "host that can't
    // accept the wrapper is a host that can't run jobs" stance.
    let push_outcome = if skip_wrapper_push {
        None
    } else {
        Some(eager_push_wrapper(&db, &host.id, &ssh_target).await)
    };

    let host = db.get_host(&host.id)?.context("host disappeared after registration")?;
    let caps = db.list_host_capabilities(&host.id)?;
    if json {
        let mut obj = host_to_json(&host, &caps);
        if let Some(outcome) = push_outcome.as_ref() {
            obj["wrapper_push"] = serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null);
        }
        println!("{}", obj);
    } else {
        println!("registered host {}", host.id);
        print_host_detail(&host, &caps);
        if let Some(outcome) = push_outcome.as_ref() {
            match outcome {
                EagerPushOutcome::Ok { version } => {
                    println!("wrapper push: ok (version {version})");
                }
                EagerPushOutcome::Skipped { reason } => {
                    println!("wrapper push: skipped ({reason})");
                }
                EagerPushOutcome::Failed { kind, detail } => {
                    println!(
                        "wrapper push: failed ({kind}) — host disabled. \
                         Fix the cause, then run `bossctl hosts probe {id}`.\n\
                         detail: {detail}"
                    );
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum EagerPushOutcome {
    Ok {
        version: String,
    },
    Skipped {
        reason: String,
    },
    Failed {
        /// One of `disk_full` / `permission_denied` / `connection_lost`
        /// / `unclassified` (matches the design's Q6 subclass labels).
        kind: String,
        detail: String,
    },
}

async fn eager_push_wrapper(db: &WorkDb, host_id: &str, ssh_target: &str) -> EagerPushOutcome {
    use boss_engine::remote_wrapper::expected_version;
    use boss_engine::ssh_transport::{SshTransport, default_control_socket_dir};
    use boss_engine::wrapper_distribution::{CubeProbeOutcome, push_wrapper, subclass_label, verify_cube_invocable};

    let Some(socket_dir) = default_control_socket_dir() else {
        return EagerPushOutcome::Skipped {
            reason: "HOME unset; cannot determine control-socket dir".to_owned(),
        };
    };
    let transport = SshTransport::new(host_id, ssh_target, &socket_dir);

    if let Err(err) = transport.open_control_master().await {
        let detail = format!("opening ssh control master: {err:#}");
        let _ = db.set_host_enabled(host_id, false);
        return EagerPushOutcome::Failed {
            kind: "connection_lost".to_owned(),
            detail,
        };
    }

    let outcome = push_wrapper(&transport).await;
    let outcome = match outcome {
        Ok(o) => o,
        Err(err) => {
            let _ = db.set_host_enabled(host_id, false);
            return EagerPushOutcome::Failed {
                kind: "unclassified".to_owned(),
                detail: format!("wrapper push errored: {err:#}"),
            };
        }
    };
    match outcome {
        boss_engine::wrapper_distribution::WrapperPushOutcome::Ok => {
            // The wrapper script itself is present and runs — but that
            // says nothing about whether the separate `cube` binary it
            // (and every dispatch-time `ssh <host> cube ...` call) depends
            // on is actually on the remote's non-interactive PATH. Catch
            // that gap here, at registration time, instead of leaving a
            // registered-but-broken host to fail every future dispatch
            // silently (the anaplian incident).
            match verify_cube_invocable(&transport).await {
                Ok(CubeProbeOutcome::Ok) => EagerPushOutcome::Ok {
                    version: expected_version(),
                },
                Ok(CubeProbeOutcome::Failed(detail)) => {
                    let msg = format!("cube not invocable via non-interactive ssh: {detail}");
                    let _ = db.set_host_enabled(host_id, false);
                    let _ = db.set_host_last_error(host_id, Some(&msg));
                    EagerPushOutcome::Failed {
                        kind: "unclassified".to_owned(),
                        detail: msg,
                    }
                }
                Err(err) => {
                    let msg = format!("probing cube invocability errored: {err:#}");
                    let _ = db.set_host_enabled(host_id, false);
                    let _ = db.set_host_last_error(host_id, Some(&msg));
                    EagerPushOutcome::Failed {
                        kind: "unclassified".to_owned(),
                        detail: msg,
                    }
                }
            }
        }
        boss_engine::wrapper_distribution::WrapperPushOutcome::Failed(kind, detail) => {
            let _ = db.set_host_enabled(host_id, false);
            EagerPushOutcome::Failed {
                kind: subclass_label(&kind).to_owned(),
                detail,
            }
        }
    }
}

fn hosts_list(json: bool, state_root: Option<PathBuf>, only_enabled: bool) -> Result<()> {
    let db = open_hosts_db(state_root)?;
    let mut hosts = db.list_hosts()?;
    if only_enabled {
        hosts.retain(|h| h.enabled);
    }
    if json {
        let arr: Vec<serde_json::Value> = hosts
            .iter()
            .map(|h| {
                let caps = db.list_host_capabilities(&h.id).unwrap_or_default();
                host_to_json(h, &caps)
            })
            .collect();
        println!("{}", serde_json::json!({ "hosts": arr }));
    } else if hosts.is_empty() {
        println!("no hosts registered");
    } else {
        for host in &hosts {
            let caps = db.list_host_capabilities(&host.id).unwrap_or_default();
            print_host_short(host, &caps);
        }
    }
    Ok(())
}

fn hosts_show(json: bool, state_root: Option<PathBuf>, id: String) -> Result<()> {
    let db = open_hosts_db(state_root)?;
    match db.get_host(&id)? {
        None => {
            if json {
                println!("{}", serde_json::json!({ "host": null, "id": id }));
            } else {
                println!("host not found: {id}");
            }
        }
        Some(host) => {
            let caps = db.list_host_capabilities(&host.id)?;
            if json {
                println!("{}", host_to_json(&host, &caps));
            } else {
                print_host_detail(&host, &caps);
            }
        }
    }
    Ok(())
}

fn hosts_tag_add(json: bool, state_root: Option<PathBuf>, id: String, tags: Vec<String>) -> Result<()> {
    let db = open_hosts_db(state_root)?;
    for tag in &tags {
        db.add_user_host_capability(&id, tag)?;
    }
    let host = db.get_host(&id)?.context("host disappeared after tag add")?;
    let caps = db.list_host_capabilities(&id)?;
    if json {
        println!("{}", host_to_json(&host, &caps));
    } else {
        println!("added {} tag(s) to host {id}", tags.len());
        print_host_detail(&host, &caps);
    }
    Ok(())
}

fn hosts_tag_remove(json: bool, state_root: Option<PathBuf>, id: String, tags: Vec<String>) -> Result<()> {
    let db = open_hosts_db(state_root)?;
    for tag in &tags {
        db.remove_user_host_capability(&id, tag)?;
    }
    let host = db.get_host(&id)?.context("host disappeared after tag remove")?;
    let caps = db.list_host_capabilities(&id)?;
    if json {
        println!("{}", host_to_json(&host, &caps));
    } else {
        println!("removed {} tag(s) from host {id}", tags.len());
        print_host_detail(&host, &caps);
    }
    Ok(())
}

fn hosts_set_enabled(json: bool, state_root: Option<PathBuf>, id: String, enabled: bool) -> Result<()> {
    let db = open_hosts_db(state_root)?;
    db.set_host_enabled(&id, enabled)?;
    let host = db.get_host(&id)?.context("host disappeared after enable/disable")?;
    let caps = db.list_host_capabilities(&host.id)?;
    if json {
        println!("{}", host_to_json(&host, &caps));
    } else {
        let verb = if enabled { "enabled" } else { "disabled" };
        println!("{verb} host {id}");
    }
    Ok(())
}

fn hosts_remove(json: bool, state_root: Option<PathBuf>, id: String) -> Result<()> {
    let db = open_hosts_db(state_root)?;
    db.remove_host(&id)?;
    if json {
        println!("{}", serde_json::json!({ "status": "removed", "id": id }));
    } else {
        println!("removed host {id}");
    }
    Ok(())
}

fn host_to_json(host: &Host, caps: &[HostCapability]) -> serde_json::Value {
    serde_json::json!({
        "id": host.id,
        "ssh_target": host.ssh_target,
        "pool_size": host.pool_size,
        "enabled": host.enabled,
        "last_seen_at": host.last_seen_at,
        "last_error_text": host.last_error_text,
        "consecutive_failures": host.consecutive_failures,
        "created_at": host.created_at,
        "capabilities": caps.iter().map(|c| serde_json::json!({
            "capability": c.capability,
            "source": c.source,
        })).collect::<Vec<_>>(),
    })
}

fn print_host_short(host: &Host, caps: &[HostCapability]) {
    let enabled = if host.enabled { "enabled" } else { "disabled" };
    let target = host.ssh_target.as_deref().unwrap_or("(local)");
    println!(
        "{}  {}  pool={}  caps={}  target={}",
        host.id,
        enabled,
        host.pool_size,
        caps.len(),
        target,
    );
}

fn print_host_detail(host: &Host, caps: &[HostCapability]) {
    let enabled = if host.enabled { "enabled" } else { "disabled" };
    println!("host {}", host.id);
    println!("  status:      {enabled}");
    println!("  pool_size:   {}", host.pool_size);
    if let Some(t) = &host.ssh_target {
        println!("  ssh_target:  {t}");
    }
    println!("  created_at:  {}", host.created_at);
    if let Some(s) = &host.last_seen_at {
        println!("  last_seen:   {s}");
    }
    if let Some(e) = &host.last_error_text {
        println!("  last_error:  {e}");
    }
    if host.consecutive_failures > 0 {
        println!("  consecutive_failures: {}", host.consecutive_failures);
    }
    if caps.is_empty() {
        println!("  capabilities: (none)");
    } else {
        println!("  capabilities:");
        for cap in caps {
            println!("    {} [{}]", cap.capability, cap.source);
        }
    }
}

#[cfg(test)]
mod tests;
