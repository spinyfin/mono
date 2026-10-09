//! `boss guidance show` — per-product coordinator guidance
//! (`BOSS_COORDINATOR.md`), re-read from GitHub at the default branch's
//! current HEAD.
//!
//! The engine injects the same views into a fresh coordinator session's
//! start brief; this verb is the on-demand path: see a change that landed
//! mid-session, retry a read that failed at launch, or check which commit
//! sha the rules in context came from. Coordinator-only. Design:
//! `tools/boss/docs/coordinator-product-guidance.md`.

use boss_protocol::{CoordinatorGuidanceState, CoordinatorGuidanceView, FrontendEvent, FrontendRequest};
use clap::{Args, Subcommand};

use crate::data::resolve_product;
use crate::{CliError, RunContext, connect_for_work, print_entity, unexpected_event};

#[derive(Debug, Subcommand)]
pub(crate) enum GuidanceCommand {
    /// Read each product's `BOSS_COORDINATOR.md` from GitHub at the
    /// default branch's current HEAD and print it with the commit sha it
    /// was read at. Every product gets one entry whatever happened: loaded,
    /// missing (repo reachable, no file), over the engine's size cap (text
    /// withheld), fetch failed (nothing known — not "no guidance"), no
    /// repo, or a non-GitHub remote.
    Show(GuidanceShowArgs),
}

#[derive(Debug, Clone, Args)]
pub(crate) struct GuidanceShowArgs {
    /// Product to read guidance for (id, slug, or name). Omit to read every
    /// non-archived product.
    #[arg(long)]
    pub(crate) product: Option<String>,
}

pub(crate) async fn run_guidance_command(command: GuidanceCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        GuidanceCommand::Show(args) => run_show(ctx, args).await,
    }
}

async fn run_show(ctx: &RunContext, args: GuidanceShowArgs) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    let product_id = match args.product {
        Some(selector) => Some(resolve_product(&mut client, Some(selector), ctx).await?.id),
        None => None,
    };
    let guidance: Vec<CoordinatorGuidanceView> = rpc_call!(
        client,
        FrontendRequest::ListCoordinatorGuidance { product_id },
        "guidance show",
        FrontendEvent::CoordinatorGuidanceList { guidance } => guidance,
    )?;
    print_entity(ctx, &serde_json::json!({ "guidance": guidance }), || {
        if guidance.is_empty() {
            println!("No products are registered, so there is no product coordinator guidance to read.");
        }
        for (index, view) in guidance.iter().enumerate() {
            if index > 0 {
                println!();
            }
            print_view_human(view);
        }
    })
}

/// Human rendering: a header per product, the one-line state (shared with
/// the session-start brief via `CoordinatorGuidanceView::describe_state`),
/// and the full text when there is one.
fn print_view_human(view: &CoordinatorGuidanceView) {
    println!("== {} ({}) ==", view.product_name, view.product_id);
    println!("{}", view.describe_state());
    println!("read at: {}", view.fetched_at);
    if let CoordinatorGuidanceState::Loaded { markdown, .. } = &view.state {
        println!("--- {} begins ---", view.path);
        println!("{}", markdown.trim_end());
        println!("--- {} ends ---", view.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_accepts_an_optional_product_selector() {
        use clap::Parser;

        #[derive(Parser)]
        struct Probe {
            #[command(subcommand)]
            command: GuidanceCommand,
        }

        let all = Probe::try_parse_from(["boss", "show"]).unwrap();
        let GuidanceCommand::Show(args) = all.command;
        assert_eq!(args.product, None);

        let one = Probe::try_parse_from(["boss", "show", "--product", "boss"]).unwrap();
        let GuidanceCommand::Show(args) = one.command;
        assert_eq!(args.product.as_deref(), Some("boss"));
    }
}
