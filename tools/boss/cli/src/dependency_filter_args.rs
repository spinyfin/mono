//! Shared dependency-graph filter flags for list verbs.

use crate::*;

/// The four dependency-graph filter flags from design Q6. They are
/// mutually exclusive — clap enforces this so the engine never sees
/// an over-constrained request. Flattened into each
/// `*ListArgs` so every list verb gets the same surface.
#[derive(Debug, Clone, Args)]
#[group(multiple = false)]
pub(crate) struct DependencyFilterArgs {
    /// Items that the named work item depends on (its incoming edges).
    #[arg(long = "prerequisites-of", value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub(crate) prerequisites_of: Option<String>,

    /// Items that depend on the named work item (its outgoing edges).
    #[arg(long = "dependents-of", value_name = boss_protocol::WORK_ITEM_ID_VALUE_NAME)]
    pub(crate) dependents_of: Option<String>,

    /// Items in `todo` with no gating prerequisite — i.e. what the
    /// dispatcher could pick up next.
    #[arg(long = "unblocked")]
    pub(crate) unblocked: bool,

    /// Items currently gated by at least one incomplete prereq.
    #[arg(long = "blocked-by-deps")]
    pub(crate) blocked_by_deps: bool,
}

impl DependencyFilterArgs {
    pub(crate) fn into_filter(self) -> Option<DependencyFilter> {
        if let Some(id) = self.prerequisites_of {
            return Some(DependencyFilter::PrerequisitesOf { id });
        }
        if let Some(id) = self.dependents_of {
            return Some(DependencyFilter::DependentsOf { id });
        }
        if self.unblocked {
            return Some(DependencyFilter::Unblocked);
        }
        if self.blocked_by_deps {
            return Some(DependencyFilter::BlockedByDeps);
        }
        None
    }
}
