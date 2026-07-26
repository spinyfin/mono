//! Status/priority/effort/reasoning `ValueEnum` CLI arguments and their
//! conversions to/from the wire vocabulary, plus the board (UI) vs. stored
//! status-name translation. Split out of `commands.rs` to keep that file
//! under the repo's file-size limit.

use crate::*;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ProductStatus {
    Active,
    Paused,
    Archived,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ProjectStatusArg {
    Planned,
    Active,
    Blocked,
    Done,
    Archived,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ProjectPriority {
    Low,
    Medium,
    High,
}

/// Priority enum for tasks and chores. Mirrors `ProjectPriority`
/// exactly so kanban surfaces and CLI flags speak one vocabulary.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum TaskPriority {
    Low,
    Medium,
    High,
}

/// CLI surface for `tasks.effort_level` (design §Q1):
/// `trivial | small | medium | large | max`. `max` is the human-only
/// escape hatch — the coordinator's heuristic never emits it, but
/// users can set it via `--effort max` to request Claude's maximum
/// reasoning depth.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum EffortLevelArg {
    Trivial,
    Small,
    Medium,
    Large,
    Max,
}

impl EffortLevelArg {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Trivial => "trivial",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
            Self::Max => "max",
        }
    }
}

impl From<EffortLevelArg> for boss_protocol::EffortLevel {
    fn from(value: EffortLevelArg) -> Self {
        match value {
            EffortLevelArg::Trivial => boss_protocol::EffortLevel::Trivial,
            EffortLevelArg::Small => boss_protocol::EffortLevel::Small,
            EffortLevelArg::Medium => boss_protocol::EffortLevel::Medium,
            EffortLevelArg::Large => boss_protocol::EffortLevel::Large,
            EffortLevelArg::Max => boss_protocol::EffortLevel::Max,
        }
    }
}

/// CLI surface for `tasks.reasoning`: `standard | investigation`. The
/// **capability** signal, deliberately separate from `--effort`'s **size**
/// signal — see [`boss_protocol::ReasoningMode`]. Set `investigation` when the
/// work needs diagnosing or designing before any edit; that is a request for a
/// stronger model, and inflating `--effort` to get one instead lies about how
/// big the job is.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum ReasoningArg {
    Standard,
    Investigation,
}

impl ReasoningArg {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Investigation => "investigation",
        }
    }
}

impl From<ReasoningArg> for boss_protocol::ReasoningMode {
    fn from(value: ReasoningArg) -> Self {
        match value {
            ReasoningArg::Standard => boss_protocol::ReasoningMode::Standard,
            ReasoningArg::Investigation => boss_protocol::ReasoningMode::Investigation,
        }
    }
}

/// Translation between the leaf work-item (task/chore) status taxonomy
/// as the engine *stores* it and the names the kanban board *shows*.
///
/// The board lanes are Backlog / Doing / Review / Done / Blocked. The
/// engine has always stored the left-hand legacy strings below. As of
/// the taxonomy-alignment change the CLI speaks the board's vocabulary
/// everywhere a human or `--json` consumer can see it, while the engine
/// and stored rows keep the legacy strings untouched. The legacy names
/// remain accepted on input as aliases (see [`TaskStatusArg`] /
/// [`MoveTarget`]) so old scripts and stored data keep working.
pub(crate) mod status_vocab {
    /// `(stored, ui)` pairs for every status whose name differs between
    /// the two vocabularies. `done` and `blocked` are identical in both
    /// and so are absent here — [`to_ui`] passes them (and any unknown
    /// value) through unchanged.
    const RENAMED: [(&str, &str); 3] = [("todo", "backlog"), ("active", "doing"), ("in_review", "review")];

    /// Map a stored status string to the board (UI) name shown to
    /// humans and emitted in `--json`. Unknown values pass through so
    /// the CLI never hides a status the engine starts emitting before
    /// this table is updated.
    pub fn to_ui(stored: &str) -> &str {
        RENAMED.iter().find(|(s, _)| *s == stored).map_or(stored, |(_, ui)| *ui)
    }

    /// Map a board (UI) status name back to the stored string the engine
    /// persists and filters on — the inverse of [`to_ui`]. `blocked`,
    /// `done`, and `archived` are identical in both vocabularies and pass
    /// through, as does any unknown value. This is the single source of
    /// truth for the board→stored direction; both [`TaskStatusArg::as_str`]
    /// and [`MoveTarget::as_status`] delegate here.
    pub fn to_stored(ui: &str) -> &str {
        RENAMED.iter().find(|(_, u)| *u == ui).map_or(ui, |(stored, _)| *stored)
    }
}

/// Identity function kept for call-site symmetry: all display boundaries
/// call `with_display_status` to mark the intent. The actual board (UI)
/// label is produced at each display site via
/// `task.status.display_label()` rather than by mutating the typed field.
pub(crate) fn with_display_status(task: Task) -> Task {
    task
}

/// [`with_display_status`] for the `WorkItem` envelope: passes through
/// task/chore variants unchanged (display transformation happens at each
/// display site); leaves products / projects untouched.
pub(crate) fn work_item_with_display_status(item: WorkItem) -> WorkItem {
    item
}

/// `boss task|chore update --status` and `--status` list filters.
///
/// The variants are the board (UI) names; the legacy stored names are
/// accepted as hidden aliases for backward compatibility. [`Self::as_str`]
/// always returns the stored string, so both the wire patch sent to the
/// engine and the status-filter comparison stay in the stored vocabulary.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum TaskStatusArg {
    #[value(alias = "todo")]
    Backlog,
    #[value(alias = "active")]
    Doing,
    Blocked,
    #[value(alias = "in-review", alias = "in_review")]
    Review,
    Done,
    Archived,
    Cancelled,
}

/// `boss task|chore move --to`. Same board-name-primary,
/// legacy-name-alias scheme as [`TaskStatusArg`]; [`Self::as_status`]
/// returns the stored string.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum MoveTarget {
    #[value(alias = "todo")]
    Backlog,
    #[value(alias = "active")]
    Doing,
    #[value(alias = "in-review", alias = "in_review")]
    Review,
    Done,
    Blocked,
    Archived,
    Cancelled,
}

impl ProductStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Archived => "archived",
        }
    }
}

impl ProjectStatusArg {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Archived => "archived",
        }
    }
}

impl ProjectPriority {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl TaskPriority {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl TaskStatusArg {
    /// The stored status string sent to the engine and used for
    /// status-filter comparisons. Maps the board (UI) variant name back
    /// to the legacy stored vocabulary.
    pub(crate) fn as_str(self) -> &'static str {
        status_vocab::to_stored(self.board_name())
    }

    /// The board (UI) name for this variant, i.e. the primary spelling of
    /// its `ValueEnum`. Fed to [`status_vocab::to_stored`] by [`Self::as_str`].
    pub(crate) fn board_name(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Doing => "doing",
            Self::Blocked => "blocked",
            Self::Review => "review",
            Self::Done => "done",
            Self::Archived => "archived",
            Self::Cancelled => "cancelled",
        }
    }
}

impl MoveTarget {
    /// The stored status string the engine persists. Maps the board (UI)
    /// variant name back to the legacy stored vocabulary via the shared
    /// [`status_vocab::to_stored`] table.
    pub(crate) fn as_status(self) -> &'static str {
        status_vocab::to_stored(self.board_name())
    }

    /// The board (UI) name for this variant, i.e. the primary spelling of
    /// its `ValueEnum`. Fed to [`status_vocab::to_stored`] by [`Self::as_status`].
    pub(crate) fn board_name(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Doing => "doing",
            Self::Review => "review",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Archived => "archived",
            Self::Cancelled => "cancelled",
        }
    }
}
