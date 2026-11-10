//! Diagnosis packet for a progression run (design 2.3).
//!
//! Stored diagnosis is augmented by engine-side provider probes before spawn.

mod catalogue;
mod evidence;
mod gather;
pub use evidence::{EngineEvidence, EvidenceProvider, gather_evidence};
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
mod evidence_tests;
#[cfg(test)]
mod golden;
#[cfg(test)]
mod snapshots;
