//! PR review-guide wire types — the `GetReviewGuideSummary` /
//! `GetReviewGuideContent` / `GenerateReviewGuide` / `RetryReviewGuide` RPC
//! surface. Board/task detail replies use [`ReviewGuideSummary`] alone; only an opened viewer
//! fetches [`ReviewGuideVersion`]'s full Markdown. See
//! `tools/boss/docs/designs/automatic-pr-review-guides.md`.

use serde::{Deserialize, Serialize};

/// Series identity and current lifecycle, without any Markdown content.
/// Returned by `GetReviewGuideSummary`; also what a card/task-detail reply
/// embeds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewGuideSummary {
    pub series_id: String,
    pub root_task_id: String,
    pub canonical_pr_url: String,
    /// One of `idle` / `queued` / `generating` / `ready` / `failed` — see
    /// the design's "Job state and concurrency" table. Independent of CI,
    /// approval, and merge readiness (design invariant #6).
    pub lifecycle: String,
    /// Monotonic per-series counter; bumped on every fresh desired request
    /// (an initial capture, a source change, or an explicit retry).
    pub request_epoch: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_comparison_id: Option<String>,
    /// The currently readable version's id, if any. Fetch its content with
    /// `GetReviewGuideContent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readable_version_id: Option<String>,
}

/// One immutable, validated guide version's full content. Returned by
/// `GetReviewGuideContent`; never mutated after generation (design
/// invariant #3) — a regenerate creates a new version instead.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewGuideVersion {
    pub id: String,
    pub series_id: String,
    pub comparison_id: String,
    pub attempt_id: String,
    pub markdown: String,
    pub content_hash: String,
    pub prompt_version: String,
    pub generated_at: String,
}

/// One durable generation attempt's diagnostic state. Returned by
/// `GenerateReviewGuide` / `RetryReviewGuide` so the caller can show progress
/// without a second round trip.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewGuideAttempt {
    pub id: String,
    pub series_id: String,
    pub comparison_id: String,
    pub request_epoch: i64,
    /// One of `queued` / `running` / `succeeded` / `failed` / `cancelled` /
    /// `superseded`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Provider usage objects keyed by provider and transcript/message identity.
    /// Absent categories remain absent; no usage observation is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_usage_json: Option<String>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn manual_generation_request_round_trips_with_optional_token() {
        for token in [None, Some("click-token".to_owned())] {
            let request = crate::FrontendRequest::GenerateReviewGuide {
                root_task_id: "root".to_owned(),
                idempotency_token: token.clone(),
            };
            let encoded = serde_json::to_value(request).unwrap();
            assert_eq!(encoded["type"], "generate_review_guide");
            assert_eq!(encoded.get("idempotency_token").is_some(), token.is_some());
            let crate::FrontendRequest::GenerateReviewGuide {
                root_task_id,
                idempotency_token,
            } = serde_json::from_value(encoded).unwrap()
            else {
                panic!("wrong request")
            };
            assert_eq!(root_task_id, "root");
            assert_eq!(idempotency_token, token);
        }
    }
}
