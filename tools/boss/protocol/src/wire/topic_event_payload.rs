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
    /// A read-only text snapshot of a remote worker's pane, or the reason
    /// there is none. Pushed on the run's [`crate::pane_capture_topic`].
    ///
    /// `text` has already had terminal control characters replaced by visible
    /// escapes, so it can be shown verbatim; it is never a terminal byte
    /// stream and nothing the viewer sends back reaches the pane.
    WorkerPaneCapture {
        run_id: String,
        host_id: String,
        state: PaneCaptureState,
        /// The snapshot (bounded, newest lines). Empty unless `state` is
        /// [`PaneCaptureState::Live`].
        #[serde(default)]
        text: String,
        /// Older lines were dropped to fit the viewer's byte budget.
        #[serde(default)]
        truncated: bool,
        /// When the host looked at the pane (its clock, display only), in
        /// Unix milliseconds. Absent when there is no snapshot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_unix_ms: Option<i64>,
        /// The engine's clock when it passed the snapshot on; the viewer
        /// derives staleness from this against its own clock.
        received_unix_ms: i64,
        /// Plain words for a state other than `live`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

/// Whether a [`TopicEventPayload::WorkerPaneCapture`] carries a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneCaptureState {
    /// `text` is the pane as the host last saw it.
    Live,
    /// The host is connected but did not negotiate pane capture.
    Unsupported,
    /// The host cannot be reached right now; any earlier text is stale.
    Unreachable,
    /// The host has no such worker any more.
    Ended,
    /// The host refused the capture (for example it is at its viewer limit).
    Refused,
}
