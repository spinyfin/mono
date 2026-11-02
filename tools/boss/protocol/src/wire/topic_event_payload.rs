use serde::{Deserialize, Serialize};

use super::InvalidationClass;
use crate::types::EditorialAction;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TopicEventPayload {
    WorkInvalidated {
        reason: String,
        product_id: Option<String>,
        item_ids: Vec<String>,
        /// How much of the board the change can have touched. Absent on the
        /// wire means [`InvalidationClass::Structural`].
        #[serde(default)]
        class: InvalidationClass,
    },
    ExecutionInvalidated {
        reason: String,
        execution_id: String,
        work_item_id: String,
        status: String,
    },
    /// An editorial hook decision was recorded for a product's execution.
    /// Emitted to the `"editorial_actions.<product_id>"` topic so the UI
    /// can badge product cards when actions accumulate.
    WorkEditorialAction { action: EditorialAction },
    /// The engine dropped one or more pending invalidations for this
    /// session's outbound queue while riding out a publish burst (a
    /// merge-poller sweep across many products/executions, for example)
    /// rather than disconnecting a client that was draining fine, just not
    /// fast enough for an instant. The app should treat this the same way
    /// it treats reconnecting — refetch its subscribed topics — without
    /// tearing down or reporting the connection as lost, since it never
    /// was.
    ResyncRequired,
}
