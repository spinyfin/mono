//! Product-scoped decision records: standing "considered and declined" /
//! operator-owned rulings that outlive any single work item.
//!
//! Design: `tools/boss/docs/designs/retire-the-coordinator-s-memory-make-the-defaults-teach-the-right-thing.md`
//! §T-B2-decision. These are deliberately **not** a new `TaskStatus` —
//! `Cancelled` already covers terminal-without-delivery work items, and
//! product knowledge that should surface when filing near work needs its
//! own durable corpus (see the CLI follow-on that adds `boss decision`).

use super::common::default_unknown_created_via;
use serde::{Deserialize, Serialize};

/// Closed vocabulary for `product_decisions.kind`.
///
/// - [`Self::Wontfix`] — considered and deliberately declined (do not
///   re-file a fix for this design choice).
/// - [`Self::Decided`] — affirmative standing operator-owned ruling
///   (the plan *is* X; treat deviations as bugs against the decision,
///   not free-form new work).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// Considered and declined.
    #[default]
    Wontfix,
    /// Affirmative standing decision / operator-owned policy.
    Decided,
}

impl DecisionKind {
    pub const ALL: &'static [DecisionKind] = &[DecisionKind::Wontfix, DecisionKind::Decided];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wontfix => "wontfix",
            Self::Decided => "decided",
        }
    }
}

impl std::fmt::Display for DecisionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DecisionKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "wontfix" => Ok(Self::Wontfix),
            "decided" => Ok(Self::Decided),
            other => Err(format!(
                "unknown decision kind `{other}`; expected one of: wontfix, decided"
            )),
        }
    }
}

/// Lifecycle of a `product_decisions` row.
///
/// - [`Self::Active`] — in force; surfaces when filing near work.
/// - [`Self::Superseded`] — replaced by a later decision (`superseded_by`
///   points at the successor).
/// - [`Self::Revoked`] — no longer in force and not replaced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    #[default]
    Active,
    Superseded,
    Revoked,
}

impl DecisionStatus {
    pub const ALL: &'static [DecisionStatus] = &[
        DecisionStatus::Active,
        DecisionStatus::Superseded,
        DecisionStatus::Revoked,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Revoked => "revoked",
        }
    }

    /// True when the decision is still in force.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }
}

impl std::fmt::Display for DecisionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DecisionStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "superseded" => Ok(Self::Superseded),
            "revoked" => Ok(Self::Revoked),
            other => Err(format!(
                "unknown decision status `{other}`; expected one of: active, superseded, revoked"
            )),
        }
    }
}

/// One product-scoped decision record — the wire shape of a
/// `product_decisions` row.
///
/// Field order follows the crate convention: identity, required
/// alphabetical, optional alphabetical. Serde is name-keyed so order
/// does not affect the wire format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct Decision {
    pub id: String,
    /// Per-product `D<n>` short id. Always set on rows written by a
    /// current engine; `None` only if a row somehow predates the column
    /// (there is no pre-column era — the column ships with the table).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,

    pub product_id: String,
    pub body: String,
    pub created_at: String,
    /// Surface that created this decision (`cli`, `mac_app`, `unknown`, …).
    #[serde(default = "default_unknown_created_via")]
    #[builder(default = default_unknown_created_via())]
    pub created_via: String,

    /// Who recorded the decision (operator handle, `human`, `coordinator`, …).
    pub created_by: String,
    pub kind: DecisionKind,
    pub status: DecisionStatus,
    pub title: String,
    pub updated_at: String,
    /// Free-form search tokens (space- or comma-separated) that help the
    /// CLI/app surface this decision when filing semantically near work.
    /// Not a structured tag set — keep it short.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords: Option<String>,

    /// Optional link to the work item that produced or motivated the
    /// decision (e.g. the investigation that concluded wontfix). Soft
    /// reference — the work item may later be deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_work_item_id: Option<String>,

    /// When `status = superseded`, the successor decision's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

impl Decision {
    /// Human-facing short label (`D12`), or the canonical id when no
    /// short id is allocated.
    pub fn display_label(&self) -> String {
        decision_short_id_label(self.short_id).unwrap_or_else(|| self.id.clone())
    }
}

/// Format a decision short id as `D<n>`. Parallel to
/// [`super::common::short_id_label`] (`T<n>`) and the automation
/// `A<n>` convention.
pub fn decision_short_id_label(short_id: Option<i64>) -> Option<String> {
    short_id.map(|n| format!("D{n}"))
}

/// Input for [`crate::FrontendRequest::CreateDecision`].
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CreateDecisionInput {
    pub product_id: String,
    pub body: String,
    /// Who is recording the decision. Required — empty is rejected.
    pub created_by: String,
    pub kind: DecisionKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_via: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_work_item_id: Option<String>,
}
