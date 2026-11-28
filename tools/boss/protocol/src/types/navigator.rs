//! The engine-computed Navigator Work-view projection carried on each work-tree
//! task (design: `tools/boss/docs/designs/navigator-work-view.md`).
//!
//! One row represents an original task plus every revision linked beneath it.
//! The engine decides section membership, ordering, the winning reason and the
//! available actions; a client renders these fields and never re-derives them.
//! Every field is additive: a client that predates the projection ignores it,
//! and an engine that predates it simply omits it.
//!
//! A revision folded into its owner carries only [`NavigatorProjection::root_task_id`];
//! the owner (whose id equals its own `root_task_id`) carries [`NavigatorRoot`].

use serde::{Deserialize, Serialize};

/// Bumped when the meaning of an existing field changes, never for an additive one.
/// Adding a variant is additive: each client-facing enum has an `Unknown`
/// catch-all so an older Rust client keeps decoding the rest of the card.
pub const NAVIGATOR_PROJECTION_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigatorProjection {
    pub version: u32,
    /// The row this task belongs to: its own id for a family root, the owning
    /// root for a folded revision.
    pub root_task_id: String,
    /// Present only on the family root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<NavigatorRoot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct NavigatorRoot {
    /// Open actionable references, most urgent first. A question survives here
    /// even when a running agent keeps the row in Working.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub actions: Vec<NavigatorAction>,
    /// Current live activity, actionable input waits first, then progressing
    /// agents, with creation time and ids as tie-breaks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub activities: Vec<NavigatorActivityRef>,
    /// Advisory descriptors for controls the engine would accept now; the
    /// mutation RPCs still enforce their own gates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub commands: Vec<NavigatorCommand>,
    pub default_detail: NavigatorDetail,
    pub reason: NavigatorReason,
    /// Engine-authored phrase for `reason`; clients render it as given.
    pub reason_label: String,
    pub revisions: NavigatorRevisionSummary,
    pub section: NavigatorSection,
    /// Sorts ascending, byte-wise, into the engine's display order across every
    /// section (the section is its first character).
    pub sort_key: String,
    /// Confirmed merge of the root's current PR binding; `Some` exactly when
    /// the section is Landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_evidence: Option<NavigatorMergeEvidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigatorSection {
    /// Display order is this variant order: Ready is first, above Working.
    Ready,
    Working,
    NeedsAttention,
    Backlog,
    Landed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigatorDetail {
    Activity,
    Attention,
    Review,
    History,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigatorActivityKind {
    Implementing,
    Revising,
    FixingCi,
    ResolvingConflict,
    Reviewing,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigatorHumanAction {
    Question,
    Input,
    Intervention,
    Review,
    Merge,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigatorCommandKind {
    Start,
    Retry,
    Park,
    EditBrief,
    CancelMerge,
    GenerateGuide,
    OpenPr,
    #[serde(other)]
    Unknown,
}

/// The single winning explanation for the row's section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NavigatorReason {
    Merged {
        evidence: NavigatorMergeEvidence,
    },
    Active {
        activity: NavigatorActivityKind,
    },
    Human {
        action: NavigatorHumanAction,
    },
    Waiting {
        wait: NavigatorWait,
    },
    #[serde(other)]
    Unknown,
}

/// Why a Backlog row is not moving. `Diagnostic` carries the engine's own
/// sentence for a hold it has no dedicated variant for; it is never dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NavigatorWait {
    Failed,
    Interrupted,
    Queued,
    Ready,
    Starting,
    Waiting,
    ActivityUnavailable,
    WaitingReview,
    WaitingChecks,
    Merging,
    ApplyingReview,
    ReviewQueued,
    RepairQueued,
    WaitingDependency,
    FocusHeld,
    DispatchPaused,
    Capacity,
    Quota,
    Backoff,
    ChainHold,
    /// Unix epoch seconds (decimal string).
    NotBefore {
        at: String,
    },
    Parked,
    Deferred,
    DeferredConflict {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blocker: Option<String>,
    },
    HumanWork,
    UnavailableParent,
    Diagnostic {
        label: String,
    },
    CompletedWithoutPr,
    MergeUnverified,
    #[serde(other)]
    Unknown,
}

impl NavigatorWait {
    /// Default phrase for the wait.
    pub fn label(&self) -> String {
        match self {
            Self::Failed => "Failed".to_owned(),
            Self::Interrupted => "Interrupted".to_owned(),
            Self::Queued => "Queued".to_owned(),
            Self::Ready => "Ready".to_owned(),
            Self::Starting => "Starting".to_owned(),
            Self::Waiting => "Waiting".to_owned(),
            Self::ActivityUnavailable => "Activity unavailable".to_owned(),
            Self::WaitingReview => "Waiting for review".to_owned(),
            Self::WaitingChecks => "Waiting for checks".to_owned(),
            Self::Merging => "Merging".to_owned(),
            Self::ApplyingReview => "Applying review".to_owned(),
            Self::ReviewQueued => "Review queued".to_owned(),
            Self::RepairQueued => "Repair queued".to_owned(),
            Self::WaitingDependency => "Waiting on dependency".to_owned(),
            Self::FocusHeld => "Focus held".to_owned(),
            Self::DispatchPaused => "Dispatch paused".to_owned(),
            Self::Capacity => "Waiting for capacity".to_owned(),
            Self::Quota => "Waiting for quota".to_owned(),
            Self::Backoff => "Retrying after error".to_owned(),
            Self::ChainHold => "Held behind related work".to_owned(),
            Self::NotBefore { .. } => "Scheduled for later".to_owned(),
            Self::Parked => "Parked".to_owned(),
            Self::Deferred => "Deferred".to_owned(),
            Self::DeferredConflict { .. } => "Conflict deferred".to_owned(),
            Self::HumanWork => "Human work".to_owned(),
            Self::UnavailableParent => "Parent unavailable".to_owned(),
            Self::Diagnostic { label } => label.clone(),
            Self::CompletedWithoutPr => "Completed without PR".to_owned(),
            Self::MergeUnverified => "Completed; merge unverified".to_owned(),
            Self::Unknown => "Unknown".to_owned(),
        }
    }
}

impl NavigatorHumanAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Question => "Answer",
            Self::Input => "Input needed",
            Self::Intervention => "Intervention",
            Self::Review => "Review",
            Self::Merge => "Ready to merge",
            Self::Unknown => "Unknown",
        }
    }
}

impl NavigatorActivityKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Implementing => "Implementing",
            Self::Revising => "Revising",
            Self::FixingCi => "Fixing CI",
            Self::ResolvingConflict => "Resolving conflict",
            Self::Reviewing => "Reviewing",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigatorActivityRef {
    pub execution_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// An actionable reference the row's detail view offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NavigatorAction {
    /// A question, intervention, review or merge decision owned by `task_id`.
    /// `attention_id` is stable for as long as the same decision is open.
    Attention {
        attention_id: String,
        kind: NavigatorHumanAction,
        task_id: String,
    },
    /// An agent blocked on a person with no structured question; the client
    /// opens the agent's terminal.
    OpenInput { activity: NavigatorActivityRef },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigatorCommand {
    pub kind: NavigatorCommandKind,
    pub task_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigatorMergeEvidence {
    /// Canonical PR url of the binding the merge was observed on.
    pub pr_url: String,
    /// Unix epoch seconds (decimal string) when the engine confirmed the merge.
    pub observed_at: String,
    /// GitHub's merge time when known; otherwise clients label `observed_at`
    /// as an observation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigatorRevisionSummary {
    /// Revisions that can still change the PR (todo, active or blocked).
    pub open: u32,
    /// Every revision in the family, regardless of origin or status.
    pub total: u32,
    /// The newest open revision: the family's current ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_task_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folded_revision_serializes_without_root_fields() {
        let folded = NavigatorProjection {
            version: NAVIGATOR_PROJECTION_VERSION,
            root_task_id: "task_root".into(),
            root: None,
        };
        let value = serde_json::to_value(&folded).unwrap();
        assert_eq!(value, serde_json::json!({"version": 1, "root_task_id": "task_root"}));
        assert_eq!(serde_json::from_value::<NavigatorProjection>(value).unwrap(), folded);
    }

    #[test]
    fn root_projection_round_trips_every_tagged_shape() {
        let activity = NavigatorActivityRef {
            execution_id: "exec_1".into(),
            task_id: "task_root".into(),
            run_id: Some("run_1".into()),
        };
        let root = NavigatorRoot::builder()
            .actions(vec![
                NavigatorAction::Attention {
                    attention_id: "oq_1".into(),
                    kind: NavigatorHumanAction::Question,
                    task_id: "task_root".into(),
                },
                NavigatorAction::OpenInput {
                    activity: activity.clone(),
                },
            ])
            .activities(vec![activity])
            .commands(vec![NavigatorCommand {
                kind: NavigatorCommandKind::OpenPr,
                task_id: "task_root".into(),
            }])
            .default_detail(NavigatorDetail::Attention)
            .reason(NavigatorReason::Waiting {
                wait: NavigatorWait::DeferredConflict {
                    blocker: Some("task_other".into()),
                },
            })
            .reason_label("Conflict deferred")
            .revisions(NavigatorRevisionSummary {
                open: 1,
                total: 2,
                current_task_id: Some("task_rev".into()),
            })
            .section(NavigatorSection::NeedsAttention)
            .sort_key("1")
            .build();
        let projection = NavigatorProjection {
            version: NAVIGATOR_PROJECTION_VERSION,
            root_task_id: "task_root".into(),
            root: Some(root),
        };
        let value = serde_json::to_value(&projection).unwrap();
        assert_eq!(value["root"]["section"], "needs_attention");
        assert_eq!(value["root"]["reason"]["wait"]["kind"], "deferred_conflict");
        assert_eq!(value["root"]["actions"][1]["type"], "open_input");
        assert_eq!(
            serde_json::from_value::<NavigatorProjection>(value).unwrap(),
            projection
        );
    }

    #[test]
    fn deferred_conflict_without_a_sibling_omits_the_blocker() {
        let value = serde_json::to_value(NavigatorWait::DeferredConflict { blocker: None }).unwrap();
        assert_eq!(value, serde_json::json!({"kind": "deferred_conflict"}));
    }

    #[test]
    fn unknown_wire_variants_keep_the_projection_decodable() {
        assert_eq!(
            serde_json::from_value::<NavigatorSection>(serde_json::json!("future_section")).unwrap(),
            NavigatorSection::Unknown
        );
        assert_eq!(
            serde_json::from_value::<NavigatorWait>(serde_json::json!({"kind": "future_wait"})).unwrap(),
            NavigatorWait::Unknown
        );
        assert_eq!(
            serde_json::from_value::<NavigatorReason>(serde_json::json!({"type": "future_reason"})).unwrap(),
            NavigatorReason::Unknown
        );
        assert_eq!(
            serde_json::from_value::<NavigatorCommandKind>(serde_json::json!("future_command")).unwrap(),
            NavigatorCommandKind::Unknown
        );
        let value = serde_json::json!({
            "version": 1,
            "root_task_id": "task_root",
            "root": {
                "default_detail": "future_detail",
                "reason": {"type": "future_reason"},
                "reason_label": "Tomorrow",
                "revisions": {"open": 0, "total": 0},
                "section": "future_section",
                "sort_key": "0"
            }
        });
        let projection = serde_json::from_value::<NavigatorProjection>(value).unwrap();
        let root = projection.root.unwrap();
        assert_eq!(root.section, NavigatorSection::Unknown);
        assert_eq!(root.default_detail, NavigatorDetail::Unknown);
        assert_eq!(root.reason, NavigatorReason::Unknown);
    }

    #[test]
    fn ready_section_is_an_additive_wire_variant() {
        assert_eq!(
            serde_json::to_value(NavigatorSection::Ready).unwrap(),
            serde_json::json!("ready")
        );
        assert_eq!(
            serde_json::from_value::<NavigatorSection>(serde_json::json!("ready")).unwrap(),
            NavigatorSection::Ready
        );
    }
}
