use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StuckBucket {
    /// The owner parked the item (itself, through its project, or through its
    /// revision chain root). An explicit decision, shown for completeness.
    Parked,
    WaitingOnHuman,
    WaitingOnCapacity,
    WaitingOnDependency,
    /// `blocked: dependency` on a prerequisite the owner parked. The owner's
    /// decision about the prerequisite is also the decision about this item.
    ParkedUpstream,
    /// A progression agent is live or pending for the item, or the item is
    /// inside the cooldown that follows a progression run. Being handled.
    ProgressionInFlight,
    /// Progression could not unstick the item, and the engine has said so: its
    /// last two progression runs ended in infrastructure failure, or the
    /// escalation ladder ran out and the engine asked the owner whether to park,
    /// archive or keep trying. The sweep no longer nominates it. This counts as
    /// waiting on a human; it is a separate bucket so the report can tell an
    /// item progression gave up on from one that was always the owner's call.
    ProgressionFailing,
    EngineStuck,
}

impl StuckBucket {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Parked => "parked",
            Self::WaitingOnHuman => "waiting_on_human",
            Self::WaitingOnCapacity => "waiting_on_capacity",
            Self::WaitingOnDependency => "waiting_on_dependency",
            Self::ParkedUpstream => "parked_upstream",
            Self::ProgressionInFlight => "progression_in_flight",
            Self::ProgressionFailing => "progression_failing",
            Self::EngineStuck => "engine_stuck",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct StuckRow {
    pub work_item_id: String,
    pub age_in_state_secs: i64,
    pub bucket: StuckBucket,
    pub classification: String,
    pub last_ledger_basis: String,
    pub name: String,
    /// Timing is a lower bound: historical rows start at migration time, or a
    /// missing ledger row falls back to tasks.updated_at, which any mutation can reset.
    pub age_is_lower_bound: bool,
    pub failure_basis: Option<String>,
    /// Why the item is waiting rather than running, from the engine's wait
    /// classifier: the same answer the board card and `dispatch diagnose`
    /// show. Independent of `classification` and `bucket`, which are this
    /// report's own taxonomy. `None` when the item is not waiting on dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<String>,
    /// The phrase for `wait_reason`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_label: Option<String>,
    /// What the item's last progression run said when it judged the item not
    /// stuck (`no_action`): the stated reason the item is being left alone while
    /// it waits out the longer cooldown. `None` for any other item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progression_note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct StuckReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progression_calibration: Option<ProgressionCalibration>,
    pub generated_at_epoch_s: i64,
    pub minimum_age_secs: i64,
    pub rows: Vec<StuckRow>,
    /// Operator questions filed within the report window.
    pub questions_filed: u64,
    #[serde(default)]
    #[builder(default)]
    pub questions_filed_by_stuck_kind: BTreeMap<String, u64>,
    pub unconverted_declared_blockers: u64,
}

/// One published value; absent values (the initial question alarm) remain unset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressionLimit {
    pub value: Option<u64>,
    pub source: String,
    pub basis: String,
}

/// Persisted daily calibration, also available while progression is disabled.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ProgressionCalibration {
    pub computed_at: i64,
    pub window_start: i64,
    pub snapshot_days: usize,
    pub minimum_snapshot_days: usize,
    pub inputs: BTreeMap<String, u64>,
    pub limits: BTreeMap<String, ProgressionLimit>,
    /// Ordered fingerprint changes between sampled runs, per item (including zero).
    pub fingerprint_changes_per_item: BTreeMap<String, u64>,
    pub fingerprint_risk: String,
}
