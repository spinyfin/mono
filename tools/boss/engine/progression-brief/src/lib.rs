//! Diagnosis packet for a progression run (design 2.3).
//!
//! Stored diagnosis is augmented by engine-side provider probes before spawn.

mod catalogue;
mod evidence;
mod gather;
pub use evidence::{EngineEvidence, EvidenceProvider, gather_evidence};
mod packet;
mod render;

pub use boss_work_db::compute_stuck_fingerprint;
pub use gather::gather_progression_brief;
pub use packet::ProgressionBriefPacket;
pub use render::render_progression_brief;

#[cfg(test)]
mod evidence_tests;
#[cfg(test)]
mod golden;
#[cfg(test)]
mod snapshots;
