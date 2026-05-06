//! `bossctl` — the Boss-only CLI used by the coordinator session
//! running inside the Boss libghostty pane.
//!
//! Two-CLI design (see `tools/boss/docs/designs/main.md`):
//! - `boss` is the user-facing CLI for the work taxonomy
//!   (products / projects / tasks / chores).
//! - `bossctl` is the Boss-only CLI for control verbs
//!   (agents, probe, work start/cancel aliases, workspace summary).
//!
//! This binary scaffolds the subcommand surface. The verb handlers
//! return "not yet implemented" until the engine RPCs they call
//! through to land in subsequent sub-phases.

use std::process::ExitCode;

use anyhow::Result;
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
    /// Override the engine socket path (defaults to BOSS_SOCKET_PATH
    /// or `/tmp/boss-engine.sock`).
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
    /// Inject a probe prompt that a worker answers on its next Stop
    /// boundary; the reply is captured back to bossctl.
    Probe {
        /// Run id to probe.
        run_id: String,
        /// Probe text the worker will see as its next prompt.
        text: String,
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
}

#[derive(Subcommand, Debug)]
enum AgentsAction {
    /// List worker sessions and their current state.
    List,
    /// Show detailed status for a single worker.
    Status { run_id: String },
    /// Bring a worker pane to the front.
    Focus { run_id: String },
    /// Send text to a worker as if user-typed.
    Send { run_id: String, text: String },
    /// Interrupt a worker (Esc-equivalent).
    Interrupt { run_id: String },
    /// Launch a worker session for a given work item without going
    /// through the coordinator's auto-dispatch path.
    Launch {
        work_item_id: String,
        #[arg(long)]
        preferred_workspace_id: Option<String>,
    },
    /// Stop a worker session and release its lease.
    Stop { run_id: String },
    /// Print the most recent transcript chunk from a worker.
    Transcript {
        run_id: String,
        #[arg(long, default_value_t = 100)]
        lines: usize,
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
}

#[derive(Subcommand, Debug)]
enum WorkspaceAction {
    /// Summarize cube workspace pool state.
    Summary,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("bossctl: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    // The actual engine RPC wiring lands in a subsequent sub-phase.
    // For now every verb prints a structured "not yet implemented"
    // message so the Boss session can call them and see the engine
    // hasn't hooked them up yet, without crashing the CLI.
    let verb = describe_verb(&cli.command);
    if cli.json {
        let payload = serde_json::json!({
            "status": "not_implemented",
            "verb": verb,
        });
        println!("{}", payload);
    } else {
        println!("bossctl {verb}: not yet implemented");
    }
    Ok(())
}

fn describe_verb(command: &Command) -> String {
    match command {
        Command::Agents { action } => match action {
            AgentsAction::List => "agents list".into(),
            AgentsAction::Status { .. } => "agents status".into(),
            AgentsAction::Focus { .. } => "agents focus".into(),
            AgentsAction::Send { .. } => "agents send".into(),
            AgentsAction::Interrupt { .. } => "agents interrupt".into(),
            AgentsAction::Launch { .. } => "agents launch".into(),
            AgentsAction::Stop { .. } => "agents stop".into(),
            AgentsAction::Transcript { .. } => "agents transcript".into(),
        },
        Command::Probe { .. } => "probe".into(),
        Command::Work { action } => match action {
            WorkAction::Start { .. } => "work start".into(),
            WorkAction::Cancel { .. } => "work cancel".into(),
        },
        Command::Workspace { action } => match action {
            WorkspaceAction::Summary => "workspace summary".into(),
        },
    }
}
