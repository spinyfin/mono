//! Dependency edges between work items, and the inputs and filters used
//! to create, remove, and list them.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Field-ordering convention (applies to all structs in this file):
//
//   1. Identity fields first: `id`, `short_id`, primary FK identifiers.
//
//   2. Required (non-`Option`) fields, alphabetical within this group.
//
//   3. Optional (`Option<T>`) fields, alphabetical within this group.
//
// Struct *definitions* are sorted alphabetically by type name.
// Both orderings reduce merge conflicts when adding new structs or fields.
// Serde JSON and Swift Codable are both name-keyed, so field order does not
// affect wire format.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AddDependencyInput {
    /// Selector or id of the work item that becomes gated.
    pub dependent: String,

    /// Selector or id of the work item that gates it.
    pub prerequisite: String,

    /// Defaults to `"blocks"` if omitted.
    #[serde(default)]
    pub relation: Option<String>,
}

/// Direction of a dependency listing — incoming (prereqs of the
/// named row), outgoing (dependents), or both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyDirection {
    Prereqs,
    Dependents,
    #[default]
    Both,
}

/// One enriched dependency edge as displayed by `boss <kind> show`.
/// Unlike [`WorkItemDependency`] (a raw storage row with both
/// endpoints), this struct collapses the edge into "the peer + the
/// fact that this is a `relation` edge." `id` / `kind` / `name` /
/// `status` describe the peer (the prerequisite when this edge sits
/// in `prerequisites`, the dependent when it sits in `dependents`),
/// so the human / JSON renderer doesn't need a second lookup.
///
/// `kind` is `task`, `chore`, or `project` — derived from the id
/// prefix and the row's `tasks.kind`. UI surfaces use it to choose
/// the right icon / link.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependencyEdge {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub relation: String,
    pub status: String,
}

/// Predicate applied to `boss <kind> list` requests to surface only
/// the rows that match a dependency-graph question. Q6 spells out
/// four flags; this enum is the one-flag-per-variant projection.
/// CLI parsing rejects combinations (the four flags are mutually
/// exclusive at the surface) so the engine never sees an
/// over-constrained request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DependencyFilter {
    /// Only items that the named row depends on (its incoming edges).
    PrerequisitesOf { id: String },
    /// Only items that depend on the named row (its outgoing edges).
    DependentsOf { id: String },
    /// Only items in `todo` with no gating prerequisite — i.e. the
    /// rows the dispatcher could pick up next.
    Unblocked,
    /// Only items currently gated by at least one incomplete prereq.
    BlockedByDeps,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListDependenciesInput {
    /// Selector or id of the work item to list edges for.
    pub work_item: String,

    #[serde(default)]
    pub direction: Option<DependencyDirection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoveDependencyInput {
    pub dependent: String,
    pub prerequisite: String,
    #[serde(default)]
    pub relation: Option<String>,
}

/// One row of the `work_item_dependencies` table — an edge from a
/// dependent to a prerequisite. `relation` is `"blocks"` for v1; the
/// column exists so future relation types (`"relates-to"`,
/// `"duplicates"`, …) can ship without a re-migration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkItemDependency {
    pub dependent_id: String,
    pub prerequisite_id: String,
    pub created_at: String,
    #[serde(default = "default_relation")]
    pub relation: String,
}

pub fn default_relation() -> String {
    "blocks".to_owned()
}

/// Resolved dependency listing for a single work item. Each side
/// carries [`DependencyEdge`] entries with the peer's status and
/// name already joined in. Used by `boss <kind> show` and (in time)
/// the macOS dep section. Distinct from [`WorkItemDependencyView`]
/// because that one returns raw edge rows for the depend-list verb.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkItemDependencyDetail {
    pub work_item_id: String,
    pub dependents: Vec<DependencyEdge>,
    pub prerequisites: Vec<DependencyEdge>,
}

/// Two parallel edge lists for one work item — incoming (rows that
/// gate me) and outgoing (rows that I gate). Returned by
/// `ListDependencies` and embedded in `boss <kind> show`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkItemDependencyView {
    pub work_item_id: String,
    pub dependents: Vec<WorkItemDependency>,
    pub prerequisites: Vec<WorkItemDependency>,
}
