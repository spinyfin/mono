//! Typed work-board invalidation classes and the runtime patch that answers a
//! runtime-class invalidation (hub-and-spoke design §2.4).

use super::*;

/// How much of a product's board one `WorkInvalidated` push can have changed.
///
/// The class is chosen where the engine publishes, from what the writer
/// changed. It is a routing hint, not a promise: a consumer that applies a
/// [`FrontendEvent::WorkRuntimePatch`] still compares the patched rows with
/// what it holds and falls back to a full board fetch when a structural field
/// differs, so a misclassified site costs a full fetch rather than a stale
/// board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvalidationClass {
    /// Only runtime fields of the named cards changed (dispatch wait, PR poll
    /// state, merge-queue position, doc pointer): no card was added,
    /// removed, re-parented or re-statused, and no dependency edge moved.
    /// Consumers may re-read just the named cards.
    Runtime,
    /// Membership, lanes, projects, dependencies, attentions or deletion may
    /// have changed, or the writer is not known to be one-card. Consumers
    /// re-read the whole product board. This is the wire default, so a push
    /// from an engine that predates the field is handled as it always was.
    #[default]
    #[serde(other)]
    Structural,
}

/// The most cards one runtime-class push (or one runtime-only burst of them)
/// may name before a whole-board read is the cheaper answer. A patch re-reads
/// each named card's whole revision chain, so its cost grows with the cards
/// named while the tree's does not; past this many the engine promotes a
/// coalesced push to [`InvalidationClass::Structural`] and the app asks for the
/// tree. The app holds the same number for the bursts it assembles itself.
pub const MAX_RUNTIME_PATCH_ITEMS: usize = 64;

impl InvalidationClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Structural => "structural",
            Self::Runtime => "runtime",
        }
    }

    /// The class of one push that stands in for two. Structural wins: a
    /// consumer that skipped a structural change leaves its board wrong,
    /// whereas one that reads the whole board for a runtime change only pays
    /// for it.
    pub fn merge(self, other: Self) -> Self {
        if self == Self::Structural || other == Self::Structural {
            Self::Structural
        } else {
            Self::Runtime
        }
    }
}

/// One card of a [`FrontendEvent::WorkRuntimePatch`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkRuntimePatchItem {
    /// A `WorkItem::Task` or `WorkItem::Chore`, with the same derived
    /// projections the board tree attaches (revision sequence, review
    /// section, AI-review badge, doc link, and so on).
    pub item: WorkItem,
    /// The card's `TaskRuntime`, as `WorkTree::task_runtimes` carries it.
    pub runtime: TaskRuntime,
}
