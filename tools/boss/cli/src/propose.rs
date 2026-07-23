//! `boss propose <kind>` / `boss propose --list` — the mediated worker→engine
//! proposal submission verb set.
//!
//! Pure CLI + wire-protocol glue: every verb here translates flags into a
//! [`FrontendRequest::SubmitProposal`] / [`FrontendRequest::ListProposals`]
//! call and renders the typed reply. All validation (payload schema, rate
//! caps, attribution) happens engine-side — see
//! `boss_engine_proposal_validation` and `engine/core/src/app/proposals.rs`
//! — so a malformed submission comes back as a
//! [`FrontendEvent::ProposalRejected`] the worker can act on immediately,
//! rather than a client-side guess at what the engine would have accepted.
//!
//! Design: `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! §"CLI surface".

use std::path::PathBuf;

use boss_protocol::{
    FrontendEvent, FrontendRequest, ProposalErrorCode, ProposalKind, ProposalState, ProposalSubmissionError,
    WorkerProposal,
};
use clap::{Args, Subcommand, ValueEnum};

use crate::{
    CliError, EffortLevelArg, RunContext, connect_for_work, new_dynamic_table, print_entity, print_table,
    unexpected_event,
};

/// `boss propose <kind> [flags]` or `boss propose --list [--kind K] [--state S]`.
///
/// Every submission is synchronous: the engine validates and persists
/// before the command exits. On success the proposal id (`prp_…`) and its
/// current state are printed; on a malformed submission the offending
/// fields are printed and the command exits non-zero so a worker session
/// can fix and retry in the same run — see [`render_proposal_rejection`].
///
/// Idempotency is automatic: omit `--idempotency-key` (every verb below)
/// and the engine derives one from your execution id, the kind, and a hash
/// of the payload, so a retried or resumed command replays the existing
/// row (`already_submitted: true`) instead of duplicating it.
#[derive(Debug, Clone, Args)]
pub(crate) struct ProposeArgs {
    #[command(subcommand)]
    command: Option<ProposeCommand>,

    /// List every proposal filed against your own work item — across all
    /// its executions, including prior/resumed runs — instead of
    /// submitting a new one. Mutually exclusive with a `<kind>`
    /// subcommand.
    #[arg(long)]
    list: bool,

    /// With `--list`, restrict the listing to one proposal kind.
    #[arg(long, requires = "list", value_enum)]
    kind: Option<ProposalKindArg>,

    /// With `--list`, restrict the listing to one disposition (state).
    /// Omit to see every state, including `rejected` / `expired` history
    /// — that history is the point of `--list`.
    #[arg(long, requires = "list", value_enum)]
    state: Option<ProposalStateArg>,
}

/// One proposal submission per variant, matching `ProposalKind` exactly.
#[derive(Debug, Clone, Subcommand)]
pub(crate) enum ProposeCommand {
    /// Escalate this run's effort level. Auto-applies to a worker-signal
    /// attention and pauses the auto-nudge loop until acknowledged.
    ///
    /// Example: `boss propose effort-escalation --level large --reason
    /// "multi-subsystem race; brief didn't mention the engine/app boundary"`
    EffortEscalation(EffortEscalationArgs),
    /// Declare that you cannot proceed without a human/coordinator
    /// decision. Auto-applies to a worker-signal attention and pauses the
    /// auto-nudge loop.
    ///
    /// Example: `boss propose blocked --reason "bazel E0583 survives
    /// clean --expunge; need direction"`
    Blocked(BlockedArgs),
    /// Propose a follow-on task or chore that is out of scope for this
    /// run. Gated: this upserts into the originating task's `followup`
    /// attention group; the task itself is created only by the human
    /// batch-accept gesture (which also runs dedup checks — a duplicate
    /// proposal is not an error, it comes back `rejected` with a reason
    /// visible via `--list`).
    ///
    /// Example: `boss propose followup-task --name "Add retry to the X
    /// client" --description-file d.md --effort small --work-kind chore
    /// --rationale "observed transient 5xx during this task"`
    FollowupTask(FollowupTaskArgs),
    /// Record scope you consciously decided not to deliver from this run's
    /// brief. Auto-applies to a durable audit line on the work item plus
    /// an attention.
    ///
    /// Example: `boss propose deferred-scope --summary "wiring for the
    /// third data source" --reason "needs a new ingestion pipeline"`
    DeferredScope(DeferredScopeArgs),
    /// File an ad-hoc attention (question or info notice) for the human.
    /// Auto-applies to the same attention rows the engine's own detectors
    /// write.
    ///
    /// Example: `boss propose attention --title "Ambiguous requirement"
    /// --body-file b.md`
    Attention(AttentionProposalArgs),
    /// Declare this automation triage pass's outcome: either the task id
    /// you created, or that there was nothing to do. Auto-applies with a
    /// provenance check (a `--produced-task` id must actually exist and
    /// carry this run's `source_automation_id`).
    ///
    /// Examples: `boss propose automation-outcome --produced-task
    /// task_abc123` / `boss propose automation-outcome --skip --reason
    /// "repo is clean"`
    AutomationOutcome(AutomationOutcomeArgs),
    /// Declare that you opened a PR — the worker's terminal action after
    /// `cube pr create`. Auto-applies with verification (URL shape,
    /// product-repo slug, branch match against your execution) and binds
    /// the PR to the work item.
    ///
    /// Example: `boss propose pr-created --url
    /// https://github.com/o/r/pull/123`
    PrCreated(PrCreatedArgs),
}

/// Shared `--idempotency-key` override, flattened into every kind's args.
///
/// Almost never needed: omit it and the engine derives the same key your
/// retried/resumed command would derive, so replays are automatically
/// safe. Set it explicitly only if you need a caller-chosen replay scope
/// narrower or wider than "this exact payload".
#[derive(Debug, Clone, Args)]
struct IdempotencyArgs {
    #[arg(long = "idempotency-key", value_name = "KEY")]
    idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct EffortEscalationArgs {
    /// Requested effort level. `max` is the human-only escape hatch — use
    /// it when you need Claude's maximum reasoning depth regardless of
    /// what the original brief's scope markers suggest.
    #[arg(long, value_enum)]
    level: EffortLevelArg,

    /// Why the assigned effort level is insufficient.
    #[arg(long)]
    reason: String,

    #[command(flatten)]
    common: IdempotencyArgs,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct BlockedArgs {
    /// Why you cannot proceed without a human/coordinator decision.
    #[arg(long)]
    reason: String,

    #[command(flatten)]
    common: IdempotencyArgs,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct DeferredScopeArgs {
    /// One-line description of the scope you did not deliver.
    #[arg(long)]
    summary: String,

    /// Why you deferred it (needs plumbing this run doesn't have, it's a
    /// separate concern, etc.).
    #[arg(long)]
    reason: String,

    #[command(flatten)]
    common: IdempotencyArgs,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct AttentionProposalArgs {
    /// Attention title shown to the human.
    #[arg(long)]
    title: String,

    /// Attention body markdown, inline. Exactly one of `--body` /
    /// `--body-file` is required; prefer `--body-file` for anything with
    /// backticks, quotes, or multiple lines so the shell never has to
    /// quote markdown.
    #[arg(long, conflicts_with = "body_file")]
    body: Option<String>,

    /// Path to a file containing the attention body markdown.
    #[arg(long = "body-file", value_name = "PATH", conflicts_with = "body")]
    body_file: Option<PathBuf>,

    /// Discriminator mirroring `work_attention_items.kind` (e.g.
    /// `"question"`, `"info"`). Omit for the engine default.
    #[arg(long = "attention-kind")]
    attention_kind: Option<String>,

    #[command(flatten)]
    common: IdempotencyArgs,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct FollowupTaskArgs {
    /// Proposed task/chore name.
    #[arg(long)]
    name: String,

    /// Proposed task/chore description, inline. Exactly one of
    /// `--description` / `--description-file` is required; prefer
    /// `--description-file` since this is usually the longest field in
    /// the payload.
    #[arg(long, conflicts_with = "description_file")]
    description: Option<String>,

    /// Path to a file containing the proposed description.
    #[arg(long = "description-file", value_name = "PATH", conflicts_with = "description")]
    description_file: Option<PathBuf>,

    /// Effort hint for the proposed work. Omit to let the human size it.
    #[arg(long, value_enum)]
    effort: Option<EffortLevelArg>,

    /// Work-item kind the proposal should become: `task`, `chore`, or
    /// `project`. Omit for the engine default (`chore`).
    #[arg(long = "work-kind", value_enum)]
    work_kind: Option<ProposedWorkKindArg>,

    /// Why you're suggesting this follow-up.
    #[arg(long)]
    rationale: String,

    #[command(flatten)]
    common: IdempotencyArgs,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct AutomationOutcomeArgs {
    /// The task id you created for this triage pass. Mutually exclusive
    /// with `--skip`; one of the two is required.
    #[arg(long = "produced-task", value_name = "TASK_ID", conflicts_with = "skip")]
    produced_task: Option<String>,

    /// Declare that this triage pass found nothing to do. Requires
    /// `--reason`. Mutually exclusive with `--produced-task`.
    #[arg(long, conflicts_with = "produced_task", requires = "reason")]
    skip: bool,

    /// Reason for `--skip` (e.g. `"repo is clean"`). Required with
    /// `--skip`; rejected with `--produced-task`.
    #[arg(long, requires = "skip")]
    reason: Option<String>,

    #[command(flatten)]
    common: IdempotencyArgs,
}

#[derive(Debug, Clone, Args)]
pub(crate) struct PrCreatedArgs {
    /// The PR's canonical GitHub URL, e.g.
    /// `https://github.com/o/r/pull/123`.
    #[arg(long)]
    url: String,

    /// Branch the PR was opened from, if you want it recorded. Optional —
    /// the engine also verifies branch match against your execution when
    /// present.
    #[arg(long)]
    branch: Option<String>,

    #[command(flatten)]
    common: IdempotencyArgs,
}

/// CLI-local mirror of [`ProposalKind`] so `--kind` gets enumerated
/// `--help` output and shell completion (clap's `ValueEnum` can't be
/// implemented directly on the protocol type — neither it nor `ValueEnum`
/// is local to this crate).
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum ProposalKindArg {
    Attention,
    EffortEscalation,
    Blocked,
    DeferredScope,
    FollowupTask,
    AutomationOutcome,
    PrCreated,
}

impl From<ProposalKindArg> for ProposalKind {
    fn from(value: ProposalKindArg) -> Self {
        match value {
            ProposalKindArg::Attention => ProposalKind::Attention,
            ProposalKindArg::EffortEscalation => ProposalKind::EffortEscalation,
            ProposalKindArg::Blocked => ProposalKind::Blocked,
            ProposalKindArg::DeferredScope => ProposalKind::DeferredScope,
            ProposalKindArg::FollowupTask => ProposalKind::FollowupTask,
            ProposalKindArg::AutomationOutcome => ProposalKind::AutomationOutcome,
            ProposalKindArg::PrCreated => ProposalKind::PrCreated,
        }
    }
}

/// CLI-local mirror of [`ProposalState`], for the same reason as
/// [`ProposalKindArg`].
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum ProposalStateArg {
    Proposed,
    Applied,
    Rejected,
    Superseded,
    Expired,
}

impl From<ProposalStateArg> for ProposalState {
    fn from(value: ProposalStateArg) -> Self {
        match value {
            ProposalStateArg::Proposed => ProposalState::Proposed,
            ProposalStateArg::Applied => ProposalState::Applied,
            ProposalStateArg::Rejected => ProposalState::Rejected,
            ProposalStateArg::Superseded => ProposalState::Superseded,
            ProposalStateArg::Expired => ProposalState::Expired,
        }
    }
}

/// CLI-local mirror of `followup_task.proposed_work_kind`'s closed
/// vocabulary (`"task"` / `"chore"` / `"project"`), for the same reason as
/// [`ProposalKindArg`].
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum ProposedWorkKindArg {
    Task,
    Chore,
    Project,
}

impl ProposedWorkKindArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Chore => "chore",
            Self::Project => "project",
        }
    }
}

pub(crate) async fn run_propose_command(args: ProposeArgs, ctx: &RunContext) -> Result<(), CliError> {
    match (args.list, args.command) {
        (true, Some(_)) => Err(CliError::usage(
            "--list cannot be combined with a `boss propose <kind>` subcommand",
        )),
        (true, None) => {
            run_propose_list(
                ctx,
                args.kind.map(ProposalKind::from),
                args.state.map(ProposalState::from),
            )
            .await
        }
        (false, Some(command)) => run_propose_submit(ctx, command).await,
        (false, None) => Err(CliError::usage(
            "specify a proposal kind (e.g. `boss propose blocked --reason ...`) or pass --list",
        )),
    }
}

/// Read this worker session's execution id. `boss propose` never accepts
/// it as a flag — see `run_comment_reply`'s doc comment for the same
/// argument: an execution-id flag can be copy-pasted between worker panes
/// and silently misattribute, where the env var is fixed by the session
/// the engine itself spawned.
fn require_run_id() -> Result<String, CliError> {
    std::env::var("BOSS_RUN_ID").map_err(|_| {
        CliError::usage("BOSS_RUN_ID is not set — `boss propose` only works inside a Boss worker session.")
    })
}

/// Resolve a `--field` / `--field-file` pair. clap's `conflicts_with`
/// already rules out both being set; this only has to handle "exactly one"
/// vs "neither".
fn resolve_text_or_file(flag: &str, text: Option<String>, file: Option<PathBuf>) -> Result<String, CliError> {
    match (text, file) {
        (Some(text), None) => Ok(text),
        (None, Some(path)) => std::fs::read_to_string(&path)
            .map_err(|err| CliError::usage(format!("failed to read --{flag}-file {}: {err}", path.display()))),
        (None, None) => Err(CliError::usage(format!("--{flag} or --{flag}-file is required"))),
        (Some(_), Some(_)) => unreachable!("clap conflicts_with rules out --{flag} and --{flag}-file together"),
    }
}

async fn run_propose_submit(ctx: &RunContext, command: ProposeCommand) -> Result<(), CliError> {
    let run_id = require_run_id()?;

    let (kind, payload, idempotency_key) = match command {
        ProposeCommand::EffortEscalation(args) => (
            ProposalKind::EffortEscalation,
            serde_json::json!({
                "requested_level": boss_protocol::EffortLevel::from(args.level).as_str(),
                "reason": args.reason,
            }),
            args.common.idempotency_key,
        ),
        ProposeCommand::Blocked(args) => (
            ProposalKind::Blocked,
            serde_json::json!({ "reason": args.reason }),
            args.common.idempotency_key,
        ),
        ProposeCommand::DeferredScope(args) => (
            ProposalKind::DeferredScope,
            serde_json::json!({ "summary": args.summary, "reason": args.reason }),
            args.common.idempotency_key,
        ),
        ProposeCommand::Attention(args) => {
            let body_markdown = resolve_text_or_file("body", args.body, args.body_file)?;
            (
                ProposalKind::Attention,
                serde_json::json!({
                    "title": args.title,
                    "body_markdown": body_markdown,
                    "attention_kind": args.attention_kind,
                }),
                args.common.idempotency_key,
            )
        }
        ProposeCommand::FollowupTask(args) => {
            let proposed_description = resolve_text_or_file("description", args.description, args.description_file)?;
            (
                ProposalKind::FollowupTask,
                serde_json::json!({
                    "proposed_name": args.name,
                    "proposed_description": proposed_description,
                    "rationale": args.rationale,
                    "proposed_effort": args.effort.map(|e| boss_protocol::EffortLevel::from(e).as_str()),
                    "proposed_work_kind": args.work_kind.map(ProposedWorkKindArg::as_str),
                }),
                args.common.idempotency_key,
            )
        }
        ProposeCommand::AutomationOutcome(args) => {
            let payload = match (args.produced_task, args.skip, args.reason) {
                (Some(task_id), false, None) => serde_json::json!({ "outcome": "produced_task", "task_id": task_id }),
                (None, true, Some(reason)) => serde_json::json!({ "outcome": "skip", "reason": reason }),
                (None, true, None) => return Err(CliError::usage("--skip requires --reason")),
                (None, false, _) => {
                    return Err(CliError::usage(
                        "either --produced-task <task-id> or --skip --reason <reason> is required",
                    ));
                }
                (Some(_), true, _) => return Err(CliError::usage("--produced-task and --skip are mutually exclusive")),
                (Some(_), false, Some(_)) => return Err(CliError::usage("--reason is only used with --skip")),
            };
            (ProposalKind::AutomationOutcome, payload, args.common.idempotency_key)
        }
        ProposeCommand::PrCreated(args) => (
            ProposalKind::PrCreated,
            serde_json::json!({ "pr_url": args.url, "branch": args.branch }),
            args.common.idempotency_key,
        ),
    };

    let mut client = connect_for_work(ctx).await?;
    let response = client
        .send_request(&FrontendRequest::SubmitProposal {
            run_id,
            kind,
            payload,
            idempotency_key,
        })
        .await
        .map_err(CliError::internal)?;

    match response {
        FrontendEvent::ProposalSubmitted {
            proposal,
            already_submitted,
        } => print_entity(
            ctx,
            &serde_json::json!({ "proposal": proposal, "already_submitted": already_submitted }),
            || {
                if !ctx.quiet {
                    if already_submitted {
                        println!(
                            "{} already submitted ({}) — replayed, not duplicated.",
                            proposal.id, proposal.state
                        );
                    } else {
                        println!("{} submitted ({}).", proposal.id, proposal.state);
                    }
                }
            },
        ),
        FrontendEvent::ProposalRejected { error } => Err(render_proposal_rejection(Some(kind), error)),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("propose", &other)),
    }
}

async fn run_propose_list(
    ctx: &RunContext,
    kind: Option<ProposalKind>,
    state: Option<ProposalState>,
) -> Result<(), CliError> {
    let run_id = require_run_id()?;
    let mut client = connect_for_work(ctx).await?;
    let response = client
        .send_request(&FrontendRequest::ListProposals { run_id, kind, state })
        .await
        .map_err(CliError::internal)?;

    match response {
        FrontendEvent::ProposalsList {
            work_item_id,
            proposals,
        } => print_entity(
            ctx,
            &serde_json::json!({ "work_item_id": work_item_id, "proposals": proposals }),
            || {
                if !ctx.quiet {
                    if proposals.is_empty() {
                        println!("No proposals filed against {work_item_id}.");
                    } else {
                        print_proposals_table(&proposals);
                    }
                }
            },
        ),
        FrontendEvent::ProposalRejected { error } => Err(render_proposal_rejection(None, error)),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("propose --list", &other)),
    }
}

fn print_proposals_table(proposals: &[WorkerProposal]) {
    let mut table = new_dynamic_table(["ID", "KIND", "STATE", "DECIDED BY", "REASON", "CREATED"]);
    for proposal in proposals {
        table.add_row([
            proposal.id.as_str(),
            proposal.kind.as_str(),
            proposal.state.as_str(),
            proposal.decided_by.map(|d| d.as_str()).unwrap_or(""),
            proposal.decision_reason.as_deref().unwrap_or(""),
            proposal.created_at.as_str(),
        ]);
    }
    print_table(table);
}

/// Map a payload field name (as the engine's validator names it) back to
/// the CLI flag that fills it, so a field-level error points the worker at
/// something they can actually type. Falls back to the raw field name for
/// kinds/fields with no direct flag (e.g. `payload` itself, for a
/// non-object submission — unreachable from this CLI, but the engine's
/// vocabulary is shared with any other caller of `SubmitProposal`).
fn flag_hint_for_field(kind: ProposalKind, field: &str) -> Option<&'static str> {
    match (kind, field) {
        (ProposalKind::Attention, "title") => Some("--title"),
        (ProposalKind::Attention, "body_markdown") => Some("--body / --body-file"),
        (ProposalKind::Attention, "attention_kind") => Some("--attention-kind"),
        (ProposalKind::EffortEscalation, "requested_level") => Some("--level"),
        (ProposalKind::EffortEscalation, "reason") => Some("--reason"),
        (ProposalKind::Blocked, "reason") => Some("--reason"),
        (ProposalKind::DeferredScope, "summary") => Some("--summary"),
        (ProposalKind::DeferredScope, "reason") => Some("--reason"),
        (ProposalKind::FollowupTask, "proposed_name") => Some("--name"),
        (ProposalKind::FollowupTask, "proposed_description") => Some("--description / --description-file"),
        (ProposalKind::FollowupTask, "rationale") => Some("--rationale"),
        (ProposalKind::FollowupTask, "proposed_effort") => Some("--effort"),
        (ProposalKind::FollowupTask, "proposed_work_kind") => Some("--work-kind"),
        (ProposalKind::AutomationOutcome, "outcome") => Some("--produced-task / --skip"),
        (ProposalKind::AutomationOutcome, "task_id") => Some("--produced-task"),
        (ProposalKind::AutomationOutcome, "reason") => Some("--reason"),
        (ProposalKind::PrCreated, "pr_url") => Some("--url"),
        (ProposalKind::PrCreated, "branch") => Some("--branch"),
        _ => None,
    }
}

/// Render a [`ProposalSubmissionError`] as CLI output: every field-level
/// complaint gets its own line (pointing at the flag that fills it, when
/// known) before the summary message becomes the exit error. `kind` is
/// `None` for `--list` rejections, which never carry field errors.
fn render_proposal_rejection(kind: Option<ProposalKind>, error: ProposalSubmissionError) -> CliError {
    for field_error in &error.field_errors {
        match kind.and_then(|kind| flag_hint_for_field(kind, &field_error.field)) {
            Some(flag) => eprintln!("  {flag} ({}): {}", field_error.field, field_error.message),
            None => eprintln!("  {}: {}", field_error.field, field_error.message),
        }
    }
    match error.code {
        ProposalErrorCode::ValidationFailed => CliError::usage(error.message),
        ProposalErrorCode::RateLimited => CliError::application(error.message),
        ProposalErrorCode::NoLocalPeer
        | ProposalErrorCode::AttributionUnresolved
        | ProposalErrorCode::AttributionMismatch
        | ProposalErrorCode::UnknownExecution => CliError::engine_unavailable(error.message),
        ProposalErrorCode::Internal => CliError::internal(anyhow::anyhow!(error.message)),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::{Cli, Commands};

    fn parse_propose(args: &[&str]) -> ProposeArgs {
        let mut full = vec!["boss", "propose"];
        full.extend_from_slice(args);
        match Cli::parse_from(full).command {
            Commands::Propose(args) => args,
            other => panic!("expected Commands::Propose, got {other:?}"),
        }
    }

    #[test]
    fn effort_escalation_parses() {
        let args = parse_propose(&[
            "effort-escalation",
            "--level",
            "large",
            "--reason",
            "multi-subsystem race",
        ]);
        match args.command {
            Some(ProposeCommand::EffortEscalation(a)) => {
                assert_eq!(a.level, EffortLevelArg::Large);
                assert_eq!(a.reason, "multi-subsystem race");
            }
            other => panic!("expected EffortEscalation, got {other:?}"),
        }
    }

    #[test]
    fn blocked_parses() {
        let args = parse_propose(&["blocked", "--reason", "need direction"]);
        match args.command {
            Some(ProposeCommand::Blocked(a)) => assert_eq!(a.reason, "need direction"),
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn attention_body_and_body_file_are_mutually_exclusive() {
        let result = Cli::try_parse_from([
            "boss",
            "propose",
            "attention",
            "--title",
            "t",
            "--body",
            "b",
            "--body-file",
            "b.md",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn attention_body_file_parses() {
        let args = parse_propose(&["attention", "--title", "t", "--body-file", "b.md"]);
        match args.command {
            Some(ProposeCommand::Attention(a)) => {
                assert_eq!(a.title, "t");
                assert_eq!(a.body, None);
                assert_eq!(a.body_file, Some(PathBuf::from("b.md")));
            }
            other => panic!("expected Attention, got {other:?}"),
        }
    }

    #[test]
    fn followup_task_parses_all_fields() {
        let args = parse_propose(&[
            "followup-task",
            "--name",
            "n",
            "--description-file",
            "d.md",
            "--effort",
            "small",
            "--work-kind",
            "chore",
            "--rationale",
            "r",
        ]);
        match args.command {
            Some(ProposeCommand::FollowupTask(a)) => {
                assert_eq!(a.name, "n");
                assert_eq!(a.description_file, Some(PathBuf::from("d.md")));
                assert_eq!(a.effort, Some(EffortLevelArg::Small));
                assert_eq!(a.work_kind, Some(ProposedWorkKindArg::Chore));
                assert_eq!(a.rationale, "r");
            }
            other => panic!("expected FollowupTask, got {other:?}"),
        }
    }

    #[test]
    fn automation_outcome_produced_task_parses() {
        let args = parse_propose(&["automation-outcome", "--produced-task", "task_abc"]);
        match args.command {
            Some(ProposeCommand::AutomationOutcome(a)) => {
                assert_eq!(a.produced_task, Some("task_abc".to_owned()));
                assert!(!a.skip);
            }
            other => panic!("expected AutomationOutcome, got {other:?}"),
        }
    }

    #[test]
    fn automation_outcome_skip_requires_reason() {
        let result = Cli::try_parse_from(["boss", "propose", "automation-outcome", "--skip"]);
        assert!(result.is_err());
    }

    #[test]
    fn automation_outcome_produced_task_and_skip_conflict() {
        let result = Cli::try_parse_from([
            "boss",
            "propose",
            "automation-outcome",
            "--produced-task",
            "task_abc",
            "--skip",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn pr_created_parses() {
        let args = parse_propose(&["pr-created", "--url", "https://github.com/o/r/pull/123"]);
        match args.command {
            Some(ProposeCommand::PrCreated(a)) => {
                assert_eq!(a.url, "https://github.com/o/r/pull/123");
                assert_eq!(a.branch, None);
            }
            other => panic!("expected PrCreated, got {other:?}"),
        }
    }

    #[test]
    fn list_parses_with_kind_and_state_filters() {
        let args = parse_propose(&["--list", "--kind", "blocked", "--state", "rejected"]);
        assert!(args.list);
        assert_eq!(args.kind, Some(ProposalKindArg::Blocked));
        assert_eq!(args.state, Some(ProposalStateArg::Rejected));
        assert!(args.command.is_none());
    }

    #[test]
    fn list_kind_filter_without_list_flag_is_rejected() {
        let result = Cli::try_parse_from(["boss", "propose", "--kind", "blocked"]);
        assert!(result.is_err());
    }

    #[test]
    fn idempotency_key_override_parses() {
        let args = parse_propose(&["blocked", "--reason", "r", "--idempotency-key", "my-key"]);
        match args.command {
            Some(ProposeCommand::Blocked(a)) => assert_eq!(a.common.idempotency_key, Some("my-key".to_owned())),
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn resolve_text_or_file_reads_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body.md");
        std::fs::write(&path, "hello\nworld\n").unwrap();
        let resolved = resolve_text_or_file("body", None, Some(path)).unwrap();
        assert_eq!(resolved, "hello\nworld\n");
    }

    #[test]
    fn resolve_text_or_file_requires_one() {
        let err = resolve_text_or_file("body", None, None).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn flag_hint_maps_known_fields() {
        assert_eq!(
            flag_hint_for_field(ProposalKind::EffortEscalation, "requested_level"),
            Some("--level")
        );
        assert_eq!(flag_hint_for_field(ProposalKind::PrCreated, "unknown_field"), None);
    }
}
