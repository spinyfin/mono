//! `boss handoff write` / `boss handoff show` — the coordinator session
//! handoff.
//!
//! The outgoing coordinator session writes a short brief (operator-stated
//! facts that changed the world, decisions, open threads, prohibitions);
//! the engine stores it as coordinator-private state and hands it to the
//! next fresh coordinator session as that session's first prompt. `show`
//! reads it back at any time (after context compaction, say). Design:
//! `tools/boss/docs/coordinator-session-handoff.md`.

use boss_protocol::{CoordinatorHandoffAuthorizationDelivery, CoordinatorHandoffView, FrontendEvent, FrontendRequest};
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
    /// Private authorization for the live coordinator session.
    #[arg(long, default_value = "")]
    pub(crate) authorization: String,
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
        FrontendRequest::SetCoordinatorHandoff { body, authorization: args.authorization },
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
    let (handoff, authorization_delivery): (
        Option<CoordinatorHandoffView>,
        Option<CoordinatorHandoffAuthorizationDelivery>,
    ) = rpc_call!(
        client,
        FrontendRequest::GetCoordinatorHandoff,
        "handoff show",
        FrontendEvent::CoordinatorHandoffResult { handoff, authorization_delivery } => (handoff, authorization_delivery),
    )?;
    print_entity(
        ctx,
        &serde_json::json!({ "handoff": handoff, "authorization_delivery": authorization_delivery }),
        || {
            print_handoff(&handoff);
            if let Some(delivery) = &authorization_delivery {
                println!("---");
                println!("{}", describe_authorization_delivery(delivery));
            }
        },
    )
}

/// The credential line a coordinator-ancestry `show` prints: where the
/// credential now is, or which step failed. The credential itself is never
/// in the response.
fn describe_authorization_delivery(delivery: &CoordinatorHandoffAuthorizationDelivery) -> String {
    if delivery.requested {
        format!(
            "Handoff write credential: redelivery requested — {}. Wait for it there; do not retry `boss handoff show` in a loop.",
            delivery.detail
        )
    } else {
        format!(
            "Handoff write credential: redelivery FAILED — {}. Report this to the operator; a reset will also be unable to request a fresh handoff until it is fixed.",
            delivery.detail
        )
    }
}

fn print_handoff(handoff: &Option<CoordinatorHandoffView>) {
    match handoff {
        None => println!(
            "No coordinator handoff is stored: no coordinator session has ever written one. \
             Write one with `boss handoff write --authorization AUTHORIZATION -` (stdin) or `boss handoff write --authorization AUTHORIZATION <file>`."
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
    }
}

#[cfg(test)]
mod tests {
    use super::HandoffCommand;
    use clap::Parser;

    #[test]
    fn show_names_the_credential_outcome_without_the_credential() {
        let requested = boss_protocol::CoordinatorHandoffAuthorizationDelivery::builder()
            .requested(true)
            .detail("the main session's inbox accepted the handoff credential")
            .build();
        let line = super::describe_authorization_delivery(&requested);
        assert!(line.contains("redelivery requested"), "{line}");
        assert!(line.contains("inbox accepted"), "{line}");
        let failed = boss_protocol::CoordinatorHandoffAuthorizationDelivery::builder()
            .requested(false)
            .detail("no Claude inbox registration under coordinator pane process 42")
            .build();
        let line = super::describe_authorization_delivery(&failed);
        assert!(line.contains("FAILED"), "{line}");
        assert!(line.contains("pane process 42"), "{line}");
        assert!(line.contains("Report this to the operator"), "{line}");
    }

    #[test]
    fn absent_credential_reaches_engine_for_rejection_and_audit() {
        let parsed = crate::Cli::try_parse_from(["boss", "handoff", "write", "-"]).unwrap();
        let crate::Commands::Handoff {
            command: HandoffCommand::Write(args),
        } = parsed.command
        else {
            panic!("expected a handoff write");
        };
        assert_eq!(args.file, "-");
        assert!(args.authorization.is_empty());
    }

    #[test]
    fn delivered_prompt_commands_parse_with_authorization() {
        for source in [
            include_str!(env!("HANDOFF_SYSTEM_PROMPT")),
            include_str!(env!("HANDOFF_RESET_PROMPT")),
        ] {
            let start = source.find("boss handoff write --authorization ").unwrap();
            let command = source[start..]
                .replace("{authorization}", "test-authorization")
                .replace("AUTHORIZATION", "test-authorization");
            let parsed = crate::Cli::try_parse_from(
                command
                    .split_whitespace()
                    .take(6)
                    .map(|word| word.trim_end_matches(['`', ','])),
            )
            .unwrap();
            let crate::Commands::Handoff {
                command: HandoffCommand::Write(args),
            } = parsed.command
            else {
                panic!("expected a handoff write");
            };
            assert_eq!(args.authorization, "test-authorization");
            assert_eq!(args.file, "-");
        }
    }
}
