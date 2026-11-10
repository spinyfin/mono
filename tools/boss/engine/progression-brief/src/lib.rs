//! Diagnosis packet for a progression run (design 2.3).
//!
//! The engine composes this from stored work-db state so the agent spends its
//! turns deciding rather than discovering. Compose never talks to git or CI:
//! conflict diagnosis and check logs are the rows already stored on
//! `conflict_resolutions` and `ci_remediations`. The agent may still run
//! `gh pr view|checks` itself.

mod catalogue;
mod gather;
mod packet;
mod render;

pub use gather::gather_progression_brief;
pub use packet::{ProgressionBriefPacket, compute_stuck_fingerprint};
pub use render::render_progression_brief;

/// Assemble the diagnosis packet for `work_item_id` and render it as the
/// progression worker's prompt body.
pub fn compose_progression_brief(db: &boss_work_db::WorkDb, work_item_id: &str) -> anyhow::Result<String> {
    let packet = gather_progression_brief(db, work_item_id)?;
    Ok(render_progression_brief(&packet))
}

#[cfg(test)]
mod snapshots;
