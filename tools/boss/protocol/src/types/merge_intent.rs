//! A person's standing request to merge a work item's PR.
//!
//! One merge request can outlive the merge attempt it started. The attempt
//! (a place in GitHub's merge queue, an armed auto-merge, a Trunk queue
//! entry) ends when CI fails in the queue or the PR conflicts with its base;
//! the request does not. The engine records it as a [`MergeIntent`] on the
//! work item so that, once an automatic fix has landed, it knows a merge was
//! already asked for and against which PR head.
//!
//! See `tools/boss/docs/merge-intent.md` for the lifecycle.

use serde::{Deserialize, Serialize};

/// The request stands: a merge attempt is in flight, or the engine is
/// fixing whatever ended the last one.
pub const MERGE_INTENT_ACTIVE: &str = "active";
/// The PR merged.
pub const MERGE_INTENT_MERGED: &str = "merged";
/// The work item finished without the PR merging (closed unmerged, deleted).
pub const MERGE_INTENT_CLOSED: &str = "closed";
/// The person who asked took the request back: they moved the card out of
/// review or bound it to a different PR.
pub const MERGE_INTENT_WITHDRAWN: &str = "withdrawn";
/// Something other than an automatic fix changed what was approved: a
/// reviewer requested changes, a non-remediation revision was created, the
/// item was parked, or it became blocked for a reason no automatic fix owns.
pub const MERGE_INTENT_INVALIDATED: &str = "invalidated";
/// The item is mergeable again but must be looked at first. `detail` says
/// what changed since the approved head.
pub const MERGE_INTENT_NEEDS_REVIEW: &str = "needs_review";
/// The merge attempt ended with the PR unchanged and nothing left to fix.
/// Asking again retries it.
pub const MERGE_INTENT_LAPSED: &str = "lapsed";
/// A newer merge request for the same item replaced this one.
pub const MERGE_INTENT_SUPERSEDED: &str = "superseded";

/// `reason` codes. Each names the ending category; `detail` carries the specific cause.
pub const MERGE_INTENT_REASON_MERGED: &str = "merged";
pub const MERGE_INTENT_REASON_TASK_FINISHED: &str = "task_finished";
pub const MERGE_INTENT_REASON_TASK_DELETED: &str = "task_deleted";
pub const MERGE_INTENT_REASON_MOVED_OUT_OF_REVIEW: &str = "moved_out_of_review";
pub const MERGE_INTENT_REASON_PR_REBOUND: &str = "pr_rebound";
pub const MERGE_INTENT_REASON_NEW_REQUEST: &str = "new_request";
pub const MERGE_INTENT_REASON_CHANGES_REQUESTED: &str = "changes_requested";
pub const MERGE_INTENT_REASON_NON_REMEDIATION_REVISION: &str = "non_remediation_revision";
pub const MERGE_INTENT_REASON_PARKED: &str = "parked";
pub const MERGE_INTENT_REASON_BLOCKED: &str = "blocked";
pub const MERGE_INTENT_REASON_REMOVED_FROM_MERGE_QUEUE: &str = "removed_from_merge_queue";
/// No safe automatic merge decision is available: fixes changed the PR,
/// a required CI fix was declined or never started, or a head is unknown.
/// The detail distinguishes these cases so a person can decide what to do.
pub const MERGE_INTENT_REASON_NO_DECISION: &str = "no_decision";
/// The PR head moved with no automatic fix to account for it.
pub const MERGE_INTENT_REASON_CHANGED_OUTSIDE_REMEDIATION: &str = "changed_outside_remediation";
/// The attempt ended, the PR is as approved, and nothing is being fixed.
pub const MERGE_INTENT_REASON_ATTEMPT_ENDED: &str = "attempt_ended";

/// The standing merge request on a work item, as shown on its card.
///
/// `Task::merge_intent` carries the item's most recent intent while it still
/// says something about the card: an active one, or one that ended in
/// `needs_review`, `lapsed` or `invalidated` on a card that is still in
/// review on the same PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct MergeIntent {
    pub id: String,
    pub work_item_id: String,
    pub pr_url: String,
    /// One of the `MERGE_INTENT_*` status constants.
    pub status: String,
    /// The PR head the merge was requested for. `None` when the head could
    /// not be read at request time, in which case nothing can later be shown
    /// to be unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_head_sha: Option<String>,
    /// The PR head the engine compared against `approved_head_sha` when it
    /// last decided what to do with this intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_head_sha: Option<String>,
    /// One of the `MERGE_INTENT_REASON_*` codes once the intent has ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// One plain sentence for the card: why the intent ended and what to do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Merges the engine has re-requested under this intent without a new
    /// request from a person.
    #[serde(default)]
    #[builder(default)]
    pub automatic_merges: i64,
    /// When the merge was requested (epoch seconds, as text).
    pub requested_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
}

impl MergeIntent {
    /// `true` while the request still stands.
    pub fn is_active(&self) -> bool {
        self.status == MERGE_INTENT_ACTIVE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nulls_from_the_card_view_read_as_absent_fields() {
        // The engine builds this JSON in SQL, where an unset column is an
        // explicit `null` rather than a missing key.
        let intent: MergeIntent = serde_json::from_str(
            r#"{"id":"mi_1","work_item_id":"task_1","pr_url":"https://github.com/o/r/pull/1",
                "status":"active","approved_head_sha":null,"decided_head_sha":null,"reason":null,
                "detail":null,"automatic_merges":0,"requested_at":"100","ended_at":null}"#,
        )
        .unwrap();
        assert!(intent.is_active());
        assert_eq!(intent.approved_head_sha, None);
        assert_eq!(intent.ended_at, None);
        let wire = serde_json::to_string(&intent).unwrap();
        assert!(!wire.contains("null"), "unset fields stay off the wire: {wire}");
    }

    /// The macOS client reads these keys by name (`WorkMergeIntent.parse`).
    #[test]
    fn the_wire_keys_the_app_reads_are_pinned() {
        let intent = MergeIntent::builder()
            .id("mi_1")
            .work_item_id("task_1")
            .pr_url("https://github.com/o/r/pull/1")
            .status(MERGE_INTENT_NEEDS_REVIEW)
            .approved_head_sha("aaa")
            .decided_head_sha("bbb")
            .reason(MERGE_INTENT_REASON_NO_DECISION)
            .detail("The PR changed.")
            .automatic_merges(1)
            .requested_at("100")
            .ended_at("200")
            .build();
        let wire = serde_json::to_value(&intent).unwrap();
        for (key, value) in [
            ("status", serde_json::json!("needs_review")),
            ("approved_head_sha", serde_json::json!("aaa")),
            ("decided_head_sha", serde_json::json!("bbb")),
            ("reason", serde_json::json!("no_decision")),
            ("detail", serde_json::json!("The PR changed.")),
            ("automatic_merges", serde_json::json!(1)),
        ] {
            assert_eq!(wire[key], value, "{key}");
        }
    }
}
