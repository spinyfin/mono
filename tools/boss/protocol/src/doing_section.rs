use serde::{Deserialize, Serialize};

/// Engine-owned Doing-column membership, carried with the live-worker
/// snapshot. Missing work-item keys mean Queued: no live worker owns them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoingSection {
    Coding,
    Reviewing,
    #[default]
    Queued,
}
