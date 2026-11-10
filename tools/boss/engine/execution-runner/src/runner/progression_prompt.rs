//! Prompt composition for `progression` executions from the diagnosis packet.

use crate::work::{WorkDb, WorkExecution};

/// Compose the initial prompt for a `progression` execution.
///
/// The worker holds no checkout; the diagnosis packet is the brief. Falls
/// back to a short diagnostic prompt if gathering fails, so a spawn still
/// happens rather than dying in compose.
pub(super) async fn compose_progression_prompt(
    work_db: &WorkDb,
    execution: &WorkExecution,
    workspace: &std::path::Path,
) -> String {
    match boss_progression_brief::gather_progression_brief(work_db, &execution.work_item_id) {
        Ok(packet) => {
            // The fingerprint is already in the rendered packet, so a failed
            // write must not cost the agent its diagnosis. Compose runs once
            // per spawn and nothing retries here: a failed write drops this
            // episode from later repeat-attempt comparison (it reads the
            // persisted per-execution keys) unless this execution is composed
            // again.
            if let Err(error) =
                work_db.record_progression_episode(&execution.id, &packet.fingerprint, &packet.stuck_kind)
            {
                tracing::warn!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    %error,
                    "progression brief: failed to record the episode; continuing with the full packet"
                );
            }
            let evidence =
                boss_progression_brief::gather_evidence(&packet, &boss_progression_brief::EngineEvidence { workspace })
                    .await;
            format!(
                "{}\n{evidence}",
                boss_progression_brief::render_progression_brief(&packet)
            )
        }
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
