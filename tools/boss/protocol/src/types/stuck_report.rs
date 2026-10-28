use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StuckBucket {
    WaitingOnHuman,
    WaitingOnCapacity,
    WaitingOnDependency,
    EngineStuck,
}

impl StuckBucket {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WaitingOnHuman => "waiting_on_human",
            Self::WaitingOnCapacity => "waiting_on_capacity",
            Self::WaitingOnDependency => "waiting_on_dependency",
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
    /// Historical rows without recoverable timing start at migration time.
    pub age_is_lower_bound: bool,
    pub failure_basis: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct StuckReport {
    pub generated_at_epoch_s: i64,
    pub minimum_age_secs: i64,
    pub rows: Vec<StuckRow>,
    pub questions_filed_by_source: BTreeMap<String, u64>,
    #[serde(default)]
    #[builder(default)]
    pub questions_filed_by_stuck_kind: BTreeMap<String, u64>,
    pub unconverted_declared_blockers: u64,
}
