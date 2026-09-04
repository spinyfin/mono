//! Ideas: markdown drafts authored over time and later graduated into a
//! chore or project. Deliberately **not** a work item — not dispatchable,
//! no execution, no PR, no attentions, no dependency edges, not on the
//! kanban. Own table, own `I<n>` short-id namespace.

use super::common::default_unknown_created_via;
use serde::{Deserialize, Serialize};

/// Lifecycle of an `ideas` row.
///
/// - [`Self::Draft`] — being authored; the only state `graduate` may act on.
/// - [`Self::Graduated`] — turned into a chore or project. `graduated_to_id`
///   points at what it became. Kept, never deleted, on graduation.
/// - [`Self::Archived`] — no longer relevant; kept for history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdeaStatus {
    #[default]
    Draft,
    Graduated,
    Archived,
}

impl IdeaStatus {
    pub const ALL: &'static [IdeaStatus] = &[IdeaStatus::Draft, IdeaStatus::Graduated, IdeaStatus::Archived];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Graduated => "graduated",
            Self::Archived => "archived",
        }
    }
}

impl std::fmt::Display for IdeaStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for IdeaStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "draft" => Ok(Self::Draft),
            "graduated" => Ok(Self::Graduated),
            "archived" => Ok(Self::Archived),
            other => Err(format!(
                "unknown idea status `{other}`; expected one of: draft, graduated, archived"
            )),
        }
    }
}

/// Target kind for [`crate::FrontendRequest::GraduateIdea`]. Graduation
/// is Ideas-only, deterministic, and not a general promote/convert mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdeaGraduationKind {
    Chore,
    Project,
}

impl IdeaGraduationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chore => "chore",
            Self::Project => "project",
        }
    }
}

impl std::fmt::Display for IdeaGraduationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for IdeaGraduationKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "chore" => Ok(Self::Chore),
            "project" => Ok(Self::Project),
            other => Err(format!(
                "unknown idea graduation target `{other}`; expected one of: chore, project"
            )),
        }
    }
}

/// One markdown draft — the wire shape of an `ideas` row.
///
/// Field order follows the crate convention: identity, required
/// alphabetical, optional alphabetical. Serde is name-keyed so order
/// does not affect the wire format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct Idea {
    pub id: String,
    /// Per-product `I<n>` short id. Always set on rows written by a
    /// current engine; `None` only if a row somehow predates the column
    /// (there is no pre-column era — the column ships with the table).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,

    pub product_id: String,
    pub body: String,
    pub created_at: String,
    /// Surface that created this idea (`cli`, `mac_app`, `unknown`, …).
    #[serde(default = "default_unknown_created_via")]
    #[builder(default = default_unknown_created_via())]
    pub created_via: String,

    pub name: String,
    pub status: IdeaStatus,
    pub updated_at: String,

    /// When `status = graduated`, the id of the chore or project this idea
    /// became. Never cleared — a graduated idea is kept, not deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graduated_to_id: Option<String>,
}

impl Idea {
    /// Human-facing short label (`I12`), or the canonical id when no
    /// short id is allocated.
    pub fn display_label(&self) -> String {
        idea_short_id_label(self.short_id).unwrap_or_else(|| self.id.clone())
    }
}

/// Format an idea short id as `I<n>`. Parallel to
/// [`super::common::short_id_label`] (`T<n>`), the automation `A<n>`
/// convention, and [`super::decision::decision_short_id_label`] (`D<n>`).
pub fn idea_short_id_label(short_id: Option<i64>) -> Option<String> {
    short_id.map(|n| format!("I{n}"))
}

/// Input for [`crate::FrontendRequest::CreateIdea`].
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CreateIdeaInput {
    pub product_id: String,
    pub name: String,
    /// Markdown draft body. `None`/absent creates an empty draft — ideas
    /// are typically authored incrementally after creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_via: Option<String>,
}

/// Input to [`crate::FrontendRequest::UpdateIdea`]. Both fields are
/// `Option`; `None` means "leave unchanged".
#[derive(Debug, Clone, Default, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct IdeaPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}
