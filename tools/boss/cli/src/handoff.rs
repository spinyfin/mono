//! `boss handoff write` / `boss handoff show` — the coordinator session
//! handoff.
//!
//! The outgoing coordinator session writes a short brief (operator-stated
//! facts that changed the world, decisions, open threads, prohibitions);
//! the engine stores it as coordinator-private state and hands it to the
//! next fresh coordinator session as that session's first prompt. `show`
//! reads it back at any time (after context compaction, say). Design:
//! `tools/boss/docs/coordinator-session-handoff.md`.

use boss_protocol::{CoordinatorHandoffView, FrontendEvent, FrontendRequest};
use clap::{Args, Subcommand};

use crate::{CliError, RunContext, connect_for_work, print_entity, unexpected_event};

#[derive(Debug, Subcommand)]
pub(crate) enum HandoffCommand {
    /// Replace the stored handoff with the contents of FILE (`-` reads
    /// stdin, so a heredoc works with no scratch file). The engine stamps
    /// the write with the time and the live coordinator session, which is
    /// how the next session judges staleness. Each write replaces the
    /// whole handoff: carry forward what is still true. Blank bodies and
    /// bodies over the engine's cap (16 KiB) are rejected — a handoff is
    /// a brief of operator-stated facts, decisions, open threads, and
    /// prohibitions, not a transcript.
    Write(HandoffWriteArgs),
    /// Print the stored handoff with when it was written, how old it is,
    /// and whether the current coordinator session wrote it. Says
    /// explicitly when none has ever been written; a stored-but-unreadable
    /// handoff is an error, never an empty result.
    Show,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct HandoffWriteArgs {
    /// Path to the handoff text, or `-` to read it from stdin.
    pub(crate) file: String,
}

pub(crate) async fn run_handoff_command(command: HandoffCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        HandoffCommand::Write(args) => run_write(ctx, args).await,
        HandoffCommand::Show => run_show(ctx).await,
    }
}

fn read_body(file: &str) -> Result<String, CliError> {
    if file == "-" {
        let mut body = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut body)
            .map_err(|e| CliError::internal(anyhow::anyhow!("read stdin: {e}")))?;
        return Ok(body);
    }
    std::fs::read_to_string(file).map_err(|e| CliError::usage(format!("cannot read handoff file {file}: {e}")))
}

async fn run_write(ctx: &RunContext, args: HandoffWriteArgs) -> Result<(), CliError> {
    let body = read_body(&args.file)?;
    let mut client = connect_for_work(ctx).await?;
    let handoff: CoordinatorHandoffView = rpc_call!(
        client,
        FrontendRequest::SetCoordinatorHandoff { body },
        "handoff write",
        FrontendEvent::CoordinatorHandoffSet { handoff } => handoff,
    )?;
    print_entity(ctx, &handoff, || {
        println!(
            "Coordinator handoff written: {} bytes at {}.",
            handoff.body.len(),
            handoff.written_at_iso8601
        );
        if !handoff.written_by_current_session {
            println!(
                "note: the engine has no live coordinator session on record, so this handoff is not attributed \
                 to one; the next coordinator session will see it as written by an unknown session."
            );
        }
    })
}

async fn run_show(ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    let handoff: Option<CoordinatorHandoffView> = rpc_call!(
        client,
        FrontendRequest::GetCoordinatorHandoff,
        "handoff show",
        FrontendEvent::CoordinatorHandoffResult { handoff } => handoff,
    )?;
    print_entity(ctx, &serde_json::json!({ "handoff": handoff }), || match &handoff {
        None => println!(
            "No coordinator handoff is stored: no coordinator session has ever written one. \
             Write one with `boss handoff write -` (stdin) or `boss handoff write <file>`."
        ),
        Some(handoff) => {
            let writer = if handoff.written_by_current_session {
                "this coordinator session".to_owned()
            } else if handoff.writer_spawn_token.is_empty() {
                "an unknown session (no coordinator was on record at write time)".to_owned()
            } else {
                format!(
                    "an EARLIER coordinator session (spawn token {}); nothing said to the current session is in it",
                    handoff.writer_spawn_token
                )
            };
            let age = boss_engine_utils::iso8601::format_elapsed_ago(
                handoff.written_at,
                handoff.written_at + handoff.age_secs,
            )
            .unwrap_or_else(|| handoff.written_at_iso8601.clone());
            let written =
                crate::time_fmt::format_epoch(handoff.written_at, &crate::time_fmt::resolve_display_tz(false));
            println!("Written: {written} ({age}) by {writer}");
            println!("---");
            println!("{}", handoff.body);
        }
    })
}
