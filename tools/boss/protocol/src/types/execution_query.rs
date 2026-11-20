//! Cross-item execution diagnostics. Missing recorded values remain null.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionQueryDimension {
    Status,
    DispatchWaitReason,
    ProgressionDisposition,
    Model,
    Driver,
    Reasoning,
    Effort,
}

impl ExecutionQueryDimension {
    pub const ALL: [Self; 7] = [
        Self::Status,
        Self::DispatchWaitReason,
        Self::ProgressionDisposition,
        Self::Model,
        Self::Driver,
        Self::Reasoning,
        Self::Effort,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::DispatchWaitReason => "dispatch_wait_reason",
            Self::ProgressionDisposition => "progression_disposition",
            Self::Model => "model",
            Self::Driver => "driver",
            Self::Reasoning => "reasoning",
            Self::Effort => "effort",
        }
    }
}

impl std::str::FromStr for ExecutionQueryDimension {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|dimension| dimension.as_str() == value)
            .ok_or_else(|| {
                format!(
                    "unknown group dimension {value:?}; expected {}",
                    Self::ALL.map(Self::as_str).join(", ")
                )
            })
    }
}

/// Values within a filter are ORed; different filters are ANDed.
/// The literal "(unset)" selects SQL NULL in nullable fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
#[serde(default)]
pub struct ExecutionQuery {
    #[builder(default)]
    pub created_via: Vec<String>,
    #[builder(default)]
    pub kind: Vec<String>,
    #[builder(default)]
    pub status: Vec<String>,
    #[builder(default)]
    pub driver: Vec<String>,
    #[builder(default)]
    pub model: Vec<String>,
    /// Current owning-task setting, not a launch-time snapshot.
    #[builder(default)]
    pub reasoning: Vec<String>,
    /// Recorded execution effort, never inferred from current policy.
    #[builder(default)]
    pub effort: Vec<String>,
    pub since_epoch_s: Option<i64>,
    /// Product slug or canonical product id.
    pub product: Option<String>,
    /// Case-sensitive literal substring of execution or run error text.
    pub error_match: Option<String>,
    #[builder(default)]
    pub group_by: Vec<ExecutionQueryDimension>,
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ExecutionQueryRow {
    pub id: String,
    pub work_item_id: String,
    pub owning_task_id: Option<String>,
    /// Typed task label, unique within product_slug.
    pub work_item_short_id: Option<String>,
    pub product_slug: Option<String>,
    pub created_at_epoch_s: i64,
    pub created_via: Option<String>,
    pub kind: String,
    pub status: String,
    pub dispatch_wait_reason: Option<String>,
    pub progression_disposition: Option<String>,
    pub driver: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub effort: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionQueryGroup {
    pub keys: BTreeMap<String, Option<String>>,
    pub count: u64,
}

/// Row queries return rows; grouped queries return counts instead.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionQueryResult {
    pub group_by: Vec<ExecutionQueryDimension>,
    pub rows: Vec<ExecutionQueryRow>,
    pub groups: Vec<ExecutionQueryGroup>,
}
