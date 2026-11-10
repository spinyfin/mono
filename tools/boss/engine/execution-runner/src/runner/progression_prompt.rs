//! Prompt composition for `progression` executions from the diagnosis packet.

use crate::work::{WorkDb, WorkExecution};

/// Compose the initial prompt for a `progression` execution.
///
/// The worker holds no checkout; the diagnosis packet is the brief. Falls
/// back to a short diagnostic prompt if gathering fails, so a spawn still
/// happens rather than dying in compose.
pub(super) fn compose_progression_prompt(work_db: &WorkDb, execution: &WorkExecution) -> String {
    match boss_progression_brief::compose_progression_brief(work_db, &execution.work_item_id) {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                %error,
                "progression brief: gather failed; using fallback prompt"
            );
            fallback_prompt(&execution.id, &execution.work_item_id, &error)
        }
    }
}

fn fallback_prompt(execution_id: &str, work_item_id: &str, error: &anyhow::Error) -> String {
    format!(
        "# Progression diagnosis\n\n\
         You are a restricted unstick agent. You hold no code checkout.\n\n\
         The engine could not assemble the diagnosis packet for `{work_item_id}` \
         (execution `{execution_id}`): {error}\n\n\
         Use read-only `boss task show` / `boss pr status` / `gh pr view|checks` \
         to diagnose, then act with `boss propose`.\n"
    )
}
