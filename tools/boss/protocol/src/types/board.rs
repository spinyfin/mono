//! Wire vocabulary for a kanban drag-and-drop gesture on the work board.
//!
//! The board's visible layout is a two-level tree: a fixed set of columns,
//! and — in the Done column — named groups within one column. Both levels
//! are *derived* from engine state (a row's status, autostart flag, blocked
//! reason and merge-queue state); neither is a stored field on the row and
//! neither is directly editable. See [`BoardGroup`] for what the Done
//! column's groups actually are.
//!
//! A drop therefore has to name *both* levels for its meaning to be
//! recoverable: a drop into the Done column is a reorder when the card was
//! already in the group it landed in, and a completion when it crossed from
//! the "Merging" group into a completion group. The engine — not the app —
//! resolves which of those a given drop was; see the
//! `boss-engine-board-gesture` crate. This module only carries the observed
//! drop target across the wire.

use serde::{Deserialize, Serialize};

/// A work-board column, in left-to-right order. The app renders exactly
/// these four and no others (`WorkBoardColumnKey` in `Models+WorkBoard.swift`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardColumn {
    Backlog,
    Doing,
    Review,
    Done,
}

impl BoardColumn {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Doing => "doing",
            Self::Review => "review",
            Self::Done => "done",
        }
    }
}

/// A named group *inside* a column. Only the Done column has groups today,
/// and they are not merely visual section headers — they separate rows with
/// genuinely different statuses:
///
/// - [`BoardGroup::Merging`] holds `in_review` rows whose PR is in a merge
///   queue or has Merge When Ready armed (`merge_queue_state` is set). These
///   rows are **in flight, not complete** — they render in the Done column
///   only because that is where a merge lands.
/// - [`BoardGroup::Completed`] is any of the completion-recency groups
///   ("Today", "Yesterday", weekday names, "Last Week", "Earlier"). Every
///   row in those is `done`; which one it lands in is derived from
///   `completed_at` and is not something a drop can choose.
///
/// Because the recency groups are indistinguishable to a drop — dropping a
/// card on "Monday" cannot backdate its completion — they collapse into one
/// `Completed` value on the wire rather than being enumerated per bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardGroup {
    Merging,
    Completed,
}

impl BoardGroup {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merging => "merging",
            Self::Completed => "completed",
        }
    }
}

/// Where a drag ended, as the app observed it.
///
/// `group` is `None` when the drop landed on the column but not on any
/// group — the column's padding or whitespace, or a column that has no
/// groups at all. That is deliberately *not* the same as "the column's
/// default group": an unqualified drop carries strictly less intent than a
/// group-qualified one, and the resolver treats it accordingly (a
/// same-column unqualified drop is always a reorder, never a transition).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardDropTarget {
    pub column: BoardColumn,
    #[serde(default)]
    pub group: Option<BoardGroup>,
}

impl BoardDropTarget {
    pub fn new(column: BoardColumn, group: Option<BoardGroup>) -> Self {
        Self { column, group }
    }

    /// Human-readable target for log lines and refusal messages.
    pub fn label(&self) -> String {
        match self.group {
            Some(group) => format!("{}/{}", self.column.as_str(), group.as_str()),
            None => self.column.as_str().to_owned(),
        }
    }
}

/// Why the engine refused (or reverted) a board gesture. Machine-readable
/// companion to the human-readable `message` on
/// [`FrontendEvent::WorkItemRefused`](crate::FrontendEvent::WorkItemRefused);
/// the app displays the message and may key presentation off the code, but
/// never decides a refusal itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardRefusalCode {
    /// The item already owns a PR, so a fresh implementation run is refused;
    /// the way to change it is a revision.
    BoundPr,
    /// Dispatch would deterministically fail because no repo resolves.
    MissingRepo,
    /// Applying the status/field patch itself was rejected (e.g. the item is
    /// gated behind an unmet dependency).
    UpdateRejected,
    /// The status change landed but dispatching a worker was refused, so the
    /// engine rolled the status back.
    DispatchRefused,
    /// A drag-to-Doing while dispatch is paused could not be forced.
    PauseOverrideRefused,
    /// The drop itself has no meaning for this card (e.g. a merging card
    /// dragged somewhere the engine cannot honour).
    DropRefused,
    /// The drop named something that is not a board card or does not exist.
    NotACard,
    /// The item's PR is already merged or closed, so a fresh run is refused
    /// and a revision cannot be added either: there is no open PR to change.
    BoundPrNotRevisable,
    /// A Move Up/Down reorder was rejected (e.g. the ordering names a task
    /// that is not in the project).
    ReorderRejected,
}

/// A follow-up the user can take after a refusal. Advisory: the app maps it
/// to a button when it has a matching affordance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardRefusalAction {
    /// Add a revision to the item's existing PR instead of re-running it.
    AddRevision,
}
