//! `boss context` — the one-call, one-round-trip sanitized read bundle for
//! a worker session: its own task/project/product, its project's sibling
//! tasks (each with dependency edges), the edges touching its own task,
//! open attention groups on its work item, and its own work item's
//! proposals across executions with dispositions.
//!
//! Takes no arguments: exactly like `boss propose --list`, the engine
//! resolves the caller's identity from the socket peer, never from a
//! flag — see `require_run_id` below for why an id flag is deliberately
//! not offered.
//!
//! Design: `tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md`
//! §"Read-only model access and the exposure boundary".

use boss_protocol::{FrontendEvent, FrontendRequest, WorkerContextBundle};

use crate::propose::{print_proposals_table, render_proposal_rejection};
use crate::{
    CliError, RunContext, connect_for_work, format_dependency_section, new_dynamic_table,
    print_attention_groups_section, print_entity, print_project_details, print_table, print_task_details,
    unexpected_event,
};

/// Read this worker session's execution id. See `propose::require_run_id`
/// for the reasoning: `boss context` never accepts it as a flag either, so
/// that a command copy-pasted between worker panes fails loudly instead of
/// reading another run's work item.
fn require_run_id() -> Result<String, CliError> {
    std::env::var("BOSS_RUN_ID")
        .ok()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            CliError::usage("BOSS_RUN_ID is not set — `boss context` only works inside a Boss worker session.")
        })
}

pub(crate) async fn run_context_command(ctx: &RunContext) -> Result<(), CliError> {
    let run_id = require_run_id()?;
    let mut client = connect_for_work(ctx).await?;
    let response = client
        .send_request(&FrontendRequest::GetWorkerContext { run_id })
        .await
        .map_err(|err| {
            CliError::engine_unavailable(format!("boss context lost the engine connection mid-request: {err}"))
        })?;

    match response {
        FrontendEvent::WorkerContextResult { bundle } => print_entity(ctx, &bundle, || print_bundle_human(&bundle)),
        FrontendEvent::ProposalRejected { error } => Err(render_proposal_rejection(None, error)),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("context", &other)),
    }
}

fn print_bundle_human(bundle: &WorkerContextBundle) {
    print_task_details("Your task:", &bundle.task, Some(&bundle.product), false);
    for line in format_dependency_section(&bundle.own_dependencies) {
        println!("{line}");
    }

    if let Some(project) = &bundle.project {
        println!();
        print_project_details("Project:", project, Some(&bundle.product), false);
    }

    if !bundle.sibling_tasks.is_empty() {
        println!();
        println!("Sibling tasks in project ({}):", bundle.sibling_tasks.len());
        let mut table = new_dynamic_table(["ID", "NAME", "STATUS", "PR URL", "DEPS"]);
        for sibling in &bundle.sibling_tasks {
            let dep_count = sibling.dependencies.prerequisites.len() + sibling.dependencies.dependents.len();
            table.add_row([
                sibling.task.id.as_str(),
                sibling.task.name.as_str(),
                sibling.task.status.display_label(),
                sibling.task.pr_url.as_deref().unwrap_or("-"),
                &dep_count.to_string(),
            ]);
        }
        print_table(table);
    }

    print_attention_groups_section(&bundle.attention_groups);

    if !bundle.proposals.is_empty() {
        println!();
        println!("Proposals against this work item ({}):", bundle.proposals.len());
        print_proposals_table(&bundle.proposals);
    }
}
