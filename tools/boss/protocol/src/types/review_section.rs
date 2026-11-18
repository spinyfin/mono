//! The Review-column section a card is grouped under.

use serde::{Deserialize, Serialize};

/// Which labelled section of the Review column a card belongs to. The engine
/// resolves this once per card (see `Task::review_section`); clients only
/// render the key they are given and never re-derive it from badge state.
///
/// Variants are declared in the order the column shows them, so the derived
/// `Ord` is the display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSection {
    /// Nothing is left for an agent to do; only review / merge remains.
    /// Exactly `Task::ready_for_review`.
    Ready,
    /// A revision is running or queued on the PR (the orange "in revision"
    /// badge).
    InRevision,
    /// An AI review is queued or running, or it reported findings that no
    /// revision has started on yet.
    AiReview,
    /// Open PR with no human action and no agent action pending, but not
    /// yet ready either: required CI is still running or has not been polled,
    /// or GitHub has not reported mergeability yet.
    Pending,
    /// Needs a human or a fix before it can progress: a blocked work item, a
    /// failing required CI check, a merge conflict, or only blocked
    /// revisions.
    Blocked,
}

impl ReviewSection {
    /// Wire value, identical to the serde representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::InRevision => "in_revision",
            Self::AiReview => "ai_review",
            Self::Pending => "pending",
            Self::Blocked => "blocked",
        }
    }
}
