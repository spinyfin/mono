use serde::{Deserialize, Serialize};

/// Where a cold board build stands, carried by
/// [`FrontendEvent::BoardPopulationProgress`](crate::FrontendEvent::BoardPopulationProgress).
///
/// A cold build is the full read behind the first `GetWorkTree` for a
/// product (or one after the cached projection was dropped). It can run for
/// minutes on a large database, and the reply carries nothing until it
/// finishes, so these pushes are the only sign of life a client gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardPopulationPhase {
    /// The engine began a full read of the product's board.
    Started,
    /// One stage of the read finished; the event's `stage` names it.
    Stage,
    /// The snapshot was serialized and written to the requesting session.
    /// `items` and `bytes` describe it.
    Done,
}
