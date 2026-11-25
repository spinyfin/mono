//! PR review-guide wire types — the `GetReviewGuideSummary` /
//! `GetReviewGuideContent` / `GetReviewGuideRun` / `GenerateReviewGuide` /
//! `RetryReviewGuide` RPC surface. Board/task detail replies use
//! [`ReviewGuideSummary`] alone; only an opened viewer fetches
//! [`ReviewGuideVersion`]'s full Markdown; [`ReviewGuideRun`] is the
//! diagnostic inspection payload for one generation run. See
//! `tools/boss/docs/designs/automatic-pr-review-guides.md`.

use serde::{Deserialize, Serialize};

/// Series identity, lifecycle, and live findings, without the generated guide body.
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
    /// Deterministic live supplement, separate from immutable version content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings: Option<ReviewGuideFindings>,
    /// Engine-recorded reason the series is `"failed"`. `None` when
    /// `lifecycle` is not `"failed"` or the attempt stored no error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Latest bound `pr_review_guide` execution id for this series, when
    /// one exists. Survives after the worker pane is gone, unlike
    /// `bossctl agents list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
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
    /// Bound `pr_review_guide` execution id, when the attempt has been
    /// dispatched. Absent while the attempt is still queued and unbound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    /// Provider usage objects keyed by provider and transcript/message identity.
    /// Absent categories remain absent; no usage observation is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_usage_json: Option<String>,
}

/// Live, deterministic supplement to the immutable generated explanation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewGuideFindings {
    pub status_text: String,
    pub addendum_markdown: String,
}

/// Diagnostic inspection of one review-guide generation run: phase
/// timestamps and source/prompt/model identities. Returned by
/// `GetReviewGuideRun`. Timestamps are the durable attempt
/// (`created_at` / `started_at` / `finished_at`) and version
/// (`generated_at`) columns; they are not independent phase clocks.
/// Source identity is the pinned comparison packet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewGuideRun {
    pub execution_id: String,
    pub attempt_id: String,
    pub series_id: String,
    pub request_epoch: i64,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// Attempt failure text when `status` is `"failed"` (or cancelled with
    /// a stored reason). Absent on a successful publish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// How many times this attempt has been recorded as failed. Zero is
    /// omitted on the wire.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    #[builder(default)]
    pub retries: i64,
    pub source: ReviewGuideRunSource,
    pub prompt_version: String,
    pub model: ReviewGuideRunModel,
    pub timings: ReviewGuideRunTimings,
}

fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

/// Pinned comparison identity for [`ReviewGuideRun`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewGuideRunSource {
    pub comparison_id: String,
    pub canonical_pr_url: String,
    pub observed_base_sha: String,
    pub merge_base_sha: String,
    pub head_sha: String,
    pub packet_hash: String,
}

/// Resolved spawn identity for [`ReviewGuideRun`]. Absent fields mean the
/// attempt was recorded before spawn config was stamped, or never launched.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewGuideRunModel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Durable attempt/version stamps for [`ReviewGuideRun`]. These are not
/// independent phase clocks: bind-to-finish includes launch delay, and
/// on success `finished_at` equals `published_at` because both are
/// written with the same `now`. Source duration is not available.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ReviewGuideRunTimings {
    /// `pr_review_guide_source_comparisons.captured_at`. Packet capture
    /// time; there is no separate source-start timestamp.
    pub source_captured_at: String,
    /// `pr_review_guide_attempts.created_at` — when the attempt was queued.
    pub queued_at: String,
    /// `pr_review_guide_attempts.started_at`. Stamped when the ready
    /// execution is bound, before the worker launches, so bind-to-finish
    /// includes scheduler/launch delay as well as model time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// `pr_review_guide_attempts.finished_at`. Terminal stamp of the
    /// attempt (success, failure, cancel, or supersede).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// `pr_review_guide_versions.generated_at`. On a successful publish
    /// this equals `finished_at` (same `now`); the derived publication
    /// interval is therefore always zero and is not a measured phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
}

/// Engine-selected revision that can still push commits to the PR and
/// therefore requires explicit merge consent. `in_review` / `done` /
/// `archived` never appear here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenMergeRevision {
    pub id: String,
    pub label: String,
    pub status: String,
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

    #[test]
    fn review_guide_run_omits_zero_retries_and_keeps_error() {
        let run = crate::ReviewGuideRun::builder()
            .execution_id("exec_1")
            .attempt_id("prga_1")
            .series_id("prgs_1")
            .request_epoch(1)
            .status("failed")
            .error("model refused")
            .retries(1)
            .source(
                crate::ReviewGuideRunSource::builder()
                    .comparison_id("prgc_1")
                    .canonical_pr_url("https://github.com/acme/widget/pull/4")
                    .observed_base_sha("base")
                    .merge_base_sha("base")
                    .head_sha("head")
                    .packet_hash("abc")
                    .build(),
            )
            .prompt_version("review-guide-v12")
            .model(crate::ReviewGuideRunModel::default())
            .timings(
                crate::ReviewGuideRunTimings::builder()
                    .source_captured_at("1")
                    .queued_at("2")
                    .build(),
            )
            .build();
        let encoded = serde_json::to_value(&run).unwrap();
        assert_eq!(encoded["error"], "model refused");
        assert_eq!(encoded["retries"], 1);

        let succeeded = crate::ReviewGuideRun::builder()
            .execution_id("exec_1")
            .attempt_id("prga_1")
            .series_id("prgs_1")
            .request_epoch(1)
            .status("succeeded")
            .source(
                crate::ReviewGuideRunSource::builder()
                    .comparison_id("prgc_1")
                    .canonical_pr_url("https://github.com/acme/widget/pull/4")
                    .observed_base_sha("base")
                    .merge_base_sha("base")
                    .head_sha("head")
                    .packet_hash("abc")
                    .build(),
            )
            .prompt_version("review-guide-v12")
            .model(crate::ReviewGuideRunModel::default())
            .timings(
                crate::ReviewGuideRunTimings::builder()
                    .source_captured_at("1")
                    .queued_at("2")
                    .build(),
            )
            .build();
        let encoded = serde_json::to_value(&succeeded).unwrap();
        assert!(encoded.get("error").is_none());
        assert!(encoded.get("retries").is_none());
    }
}
