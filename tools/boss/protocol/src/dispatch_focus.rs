//! Dispatch focus mode: the persisted set of projects and products the
//! dispatcher is restricted to, and the wire shapes `bossctl dispatch focus`
//! uses to read and change it.
//!
//! Focus mode is purely a dispatch-admission gate. It never rewrites a work
//! item (no autostart flip, no status change, no deferral): clearing the set
//! leaves every row exactly as it was, and rows it held dispatch normally.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The `dispatch_wait_reason` recorded on a ready execution that focus mode
/// is holding, next to `dispatch_paused` and `interactive_concurrency_cap`.
pub const DISPATCH_WAIT_REASON_FOCUS_MODE: &str = "focus_mode";

/// The set of projects and products dispatch is restricted to. Focus mode is
/// on while the set is non-empty; an empty set means every row is in scope.
///
/// Persisted as JSON in the engine's metadata KV so it survives a restart.
/// Holds ids, never names: a rename must not change what is in scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchFocus {
    #[serde(default)]
    pub project_ids: BTreeSet<String>,
    #[serde(default)]
    pub product_ids: BTreeSet<String>,
}

impl DispatchFocus {
    /// Focus mode is on exactly while the set is non-empty.
    pub fn is_active(&self) -> bool {
        !self.project_ids.is_empty() || !self.product_ids.is_empty()
    }

    /// Whether a project or product id is already part of the set.
    pub fn contains(&self, kind: DispatchFocusTargetKind, id: &str) -> bool {
        match kind {
            DispatchFocusTargetKind::Project => self.project_ids.contains(id),
            DispatchFocusTargetKind::Product => self.product_ids.contains(id),
        }
    }

    pub fn insert(&mut self, kind: DispatchFocusTargetKind, id: String) {
        match kind {
            DispatchFocusTargetKind::Project => self.project_ids.insert(id),
            DispatchFocusTargetKind::Product => self.product_ids.insert(id),
        };
    }

    pub fn remove(&mut self, kind: DispatchFocusTargetKind, id: &str) {
        match kind {
            DispatchFocusTargetKind::Project => self.project_ids.remove(id),
            DispatchFocusTargetKind::Product => self.product_ids.remove(id),
        };
    }
}

/// How `UpdateDispatchFocus` changes the focus set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchFocusAction {
    /// Replace the set with exactly the given targets.
    Set,
    /// Add the given targets to the set.
    Add,
    /// Remove the given targets from the set.
    Remove,
    /// Empty the set, turning focus mode off. Takes no targets.
    Clear,
}

/// Whether a focus target is a project or a product.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchFocusTargetKind {
    Project,
    Product,
}

impl DispatchFocusTargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DispatchFocusTargetKind::Project => "project",
            DispatchFocusTargetKind::Product => "product",
        }
    }
}

/// One entry of the focus set, resolved for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchFocusTarget {
    pub kind: DispatchFocusTargetKind,
    pub id: String,
    /// The project's short id (`P<n>`); products have none (they are named
    /// by slug).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,
    /// Display name, or the product slug. Empty when the row no longer
    /// exists; the id is still in the set and still scopes dispatch.
    pub name: String,
}

/// The focus set and what it is currently holding back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchFocusState {
    /// `true` while the set is non-empty.
    pub active: bool,
    pub targets: Vec<DispatchFocusTarget>,
    /// Ready executions that focus mode would hold right now (their work is
    /// outside the set). Always zero while inactive.
    pub held_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_is_active_only_while_the_set_is_non_empty() {
        let mut focus = DispatchFocus::default();
        assert!(!focus.is_active());
        focus.insert(DispatchFocusTargetKind::Project, "proj_1".into());
        assert!(focus.is_active());
        assert!(focus.contains(DispatchFocusTargetKind::Project, "proj_1"));
        assert!(!focus.contains(DispatchFocusTargetKind::Product, "proj_1"));
        focus.remove(DispatchFocusTargetKind::Project, "proj_1");
        assert!(!focus.is_active());
    }

    #[test]
    fn focus_round_trips_through_json_and_tolerates_missing_fields() {
        let mut focus = DispatchFocus::default();
        focus.insert(DispatchFocusTargetKind::Product, "prod_1".into());
        focus.insert(DispatchFocusTargetKind::Project, "proj_2".into());
        let json = serde_json::to_string(&focus).unwrap();
        assert_eq!(serde_json::from_str::<DispatchFocus>(&json).unwrap(), focus);
        assert_eq!(
            serde_json::from_str::<DispatchFocus>("{}").unwrap(),
            DispatchFocus::default()
        );
    }

    #[test]
    fn focus_requests_and_replies_have_a_stable_wire_shape() {
        use crate::{FrontendEvent, FrontendRequest};

        let request = FrontendRequest::UpdateDispatchFocus {
            action: DispatchFocusAction::Add,
            targets: vec!["P12".into(), "boss".into()],
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({"type": "update_dispatch_focus", "action": "add", "targets": ["P12", "boss"]})
        );
        // `clear` carries no targets, and an omitted list reads as empty.
        let clear: FrontendRequest =
            serde_json::from_value(serde_json::json!({"type": "update_dispatch_focus", "action": "clear"})).unwrap();
        assert!(matches!(
            clear,
            FrontendRequest::UpdateDispatchFocus { action: DispatchFocusAction::Clear, targets } if targets.is_empty()
        ));
        assert_eq!(
            serde_json::to_value(FrontendRequest::GetDispatchFocus).unwrap(),
            serde_json::json!({"type": "get_dispatch_focus"})
        );

        let reply = FrontendEvent::DispatchFocusResult {
            focus: DispatchFocusState {
                active: true,
                targets: vec![DispatchFocusTarget {
                    kind: DispatchFocusTargetKind::Project,
                    id: "proj_1".into(),
                    short_id: Some(3),
                    name: "Focus".into(),
                }],
                held_count: 2,
            },
        };
        let json = serde_json::to_value(&reply).unwrap();
        assert_eq!(json["type"], "dispatch_focus_result");
        assert_eq!(json["focus"]["held_count"], 2);
        assert_eq!(json["focus"]["targets"][0]["kind"], "project");
        let back: FrontendEvent = serde_json::from_value(json).unwrap();
        assert!(matches!(back, FrontendEvent::DispatchFocusResult { focus } if focus.active));
    }
}
