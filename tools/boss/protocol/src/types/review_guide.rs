//! PR review-guide wire types — the `GetReviewGuideSummary` /
//! `GetReviewGuideContent` / `RetryReviewGuide` RPC surface. Board/task
//! detail replies use [`ReviewGuideSummary`] alone; only an opened viewer
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
/// `RetryReviewGuide` so the caller can show progress without a second
/// round trip.
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
