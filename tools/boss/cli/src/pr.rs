//! `boss pr status` / `boss pr body` — bounded, read-only worker verbs
//! over PR state Boss already has stored, so a worker session does not
//! need `gh pr view` / `gh pr list --head` just to answer "what's my own
//! PR's mergeable/CI state" or "what's my own PR's current description."
//!
//! Same identity model as `boss context` / `boss propose`: no work-item
//! argument, the caller's own PR is resolved from `BOSS_RUN_ID` on the
//! engine side — see `require_run_id` in `context.rs`.
//!
//! Design: `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! §"Read-only model access and the exposure boundary".

use boss_protocol::{FrontendEvent, FrontendRequest, PrBodyView, PrStatusView};

use crate::propose::render_proposal_rejection;
use crate::{CliError, PrCommand, PrStatusArgs, RunContext, connect_for_work, print_entity, unexpected_event};

/// Read this worker session's execution id. See `propose::require_run_id`
/// for the reasoning: neither `boss pr status` nor `boss pr body` accepts
/// it as a flag either, so a command copy-pasted between worker panes
/// fails loudly instead of reading another run's PR.
fn require_run_id() -> Result<String, CliError> {
    std::env::var("BOSS_RUN_ID")
        .ok()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| CliError::usage("BOSS_RUN_ID is not set — `boss pr` only works inside a Boss worker session."))
}

pub(crate) async fn run_pr_command(command: PrCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        PrCommand::Status(args) => run_pr_status_command(ctx, args).await,
        PrCommand::Body => run_pr_body_command(ctx).await,
    }
}

async fn run_pr_status_command(ctx: &RunContext, args: PrStatusArgs) -> Result<(), CliError> {
    let run_id = require_run_id()?;
    let mut client = connect_for_work(ctx).await?;
    let response = client
        .send_request(&FrontendRequest::GetPrStatus {
            run_id,
            refresh: args.refresh,
        })
        .await
        .map_err(|err| {
            CliError::engine_unavailable(format!("boss pr status lost the engine connection mid-request: {err}"))
        })?;

    match response {
        FrontendEvent::PrStatusResult { status } => print_entity(ctx, &status, || print_pr_status_human(&status)),
        FrontendEvent::ProposalRejected { error } => Err(render_proposal_rejection(None, error)),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("pr status", &other)),
    }
}

fn print_pr_status_human(status: &PrStatusView) {
    let Some(pr_url) = &status.pr_url else {
        println!("No PR bound to this work item yet.");
        return;
    };
    println!("PR: {pr_url}");
    println!(
        "Mergeable: {}",
        status.mergeable.as_deref().unwrap_or("(not yet probed)")
    );
    println!(
        "Merge state: {}",
        status.merge_state_status.as_deref().unwrap_or("(not yet probed)")
    );
    println!("Head SHA: {}", status.head_sha.as_deref().unwrap_or("(not yet probed)"));
    match &status.observed_at {
        Some(observed_at) => println!("Observed at: {observed_at} (unix epoch seconds)"),
        None => println!("Observed at: never — the merge poller hasn't probed this PR yet"),
    }
    if status.refreshed {
        println!("This response reflects a live check just performed for this call.");
    } else if status.refresh_throttled {
        println!(
            "--refresh was requested but the engine-wide refresh budget was exhausted; \
             showing the last stored observation instead."
        );
    } else {
        println!(
            "This is Boss's last stored observation, not a live check — pass --refresh for \
             a bounded live check."
        );
    }
}

async fn run_pr_body_command(ctx: &RunContext) -> Result<(), CliError> {
    let run_id = require_run_id()?;
    let mut client = connect_for_work(ctx).await?;
    let response = client
        .send_request(&FrontendRequest::GetPrBody { run_id })
        .await
        .map_err(|err| {
            CliError::engine_unavailable(format!("boss pr body lost the engine connection mid-request: {err}"))
        })?;

    match response {
        FrontendEvent::PrBodyResult { body } => print_entity(ctx, &body, || print_pr_body_human(&body)),
        FrontendEvent::ProposalRejected { error } => Err(render_proposal_rejection(None, error)),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("pr body", &other)),
    }
}

fn print_pr_body_human(body: &PrBodyView) {
    match &body.pr_url {
        Some(pr_url) => println!("PR: {pr_url}"),
        None => println!("PR: (none bound to this work item yet)"),
    }
    match &body.title {
        Some(title) => println!("Title: {title}"),
        None => println!("Title: (none stored)"),
    }
    match &body.body {
        Some(text) if text.is_empty() => println!("Body: (empty description)"),
        Some(text) => {
            println!("Body:");
            println!("{text}");
        }
        None => println!(
            "Body: (none stored — this execution never snapshotted a baseline, most likely \
             because it is about to open a brand-new PR rather than resume an existing one; \
             if you need to confirm this is not a fetch-failure case, fall back to `gh pr view \
             --json body`)"
        ),
    }
}
