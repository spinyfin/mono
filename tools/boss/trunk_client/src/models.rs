//! Typed request/response models for the six Trunk queue endpoints Boss
//! calls: `submitPullRequest`, `getSubmittedPullRequest`, `listPullRequests`,
//! `getQueue`, `cancelPullRequest`, `restartTestsOnPullRequest`.
//!
//! Field names in Trunk's JSON are `camelCase`; every wire type here derives
//! `#[serde(rename_all = "camelCase")]` so the Rust side stays `snake_case`.

use serde::{Deserialize, Serialize};

// ── Repo / PR coordinates ─────────────────────────────────────────────────────

/// Repo coordinates as Trunk's API expects them: `{host, owner, name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrunkRepoRef {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl TrunkRepoRef {
    pub fn new(host: impl Into<String>, owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            owner: owner.into(),
            name: name.into(),
        }
    }
}

/// A PR reference: just its number, per Trunk's `{pr: {number}}` shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrunkPrRef {
    pub number: u64,
}

impl TrunkPrRef {
    pub fn new(number: u64) -> Self {
        Self { number }
    }
}

/// The `{repo, pr, targetBranch}` shape shared by `getSubmittedPullRequest`,
/// `cancelPullRequest`, and `restartTestsOnPullRequest`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrunkPrLookup {
    pub repo: TrunkRepoRef,
    pub pr: TrunkPrRef,
    pub target_branch: String,
}

impl TrunkPrLookup {
    pub fn new(repo: TrunkRepoRef, pr: TrunkPrRef, target_branch: impl Into<String>) -> Self {
        Self {
            repo,
            pr,
            target_branch: target_branch.into(),
        }
    }
}

/// Body for `getQueue`: `{repo, targetBranch}`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetQueueRequest {
    pub repo: TrunkRepoRef,
    pub target_branch: String,
}

impl GetQueueRequest {
    pub fn new(repo: TrunkRepoRef, target_branch: impl Into<String>) -> Self {
        Self {
            repo,
            target_branch: target_branch.into(),
        }
    }
}

/// `priority` on submit: Trunk accepts either an integer or a named string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TrunkPriority {
    Number(i64),
    Name(String),
}

/// `readiness` on a `TrunkPullRequest`: an object, not the enum-like string
/// this client originally guessed at. Real wire shape observed against
/// `github.com/brianduff/flunge`:
/// `{hasImpactedTargets:false, requiresImpactedTargets:false,
/// doesBaseBranchMatch:true, gitHubMergeability:"mergeable"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrunkReadiness {
    pub has_impacted_targets: bool,
    pub requires_impacted_targets: bool,
    pub does_base_branch_match: bool,
    pub git_hub_mergeability: String,
}

// ── State enums (unknown-variant tolerant) ────────────────────────────────────

/// One of the eight documented PR states in the Trunk queue, or
/// [`Unknown`](TrunkPrState::Unknown) preserving the raw wire string so a new
/// state Trunk introduces degrades gracefully instead of failing to
/// deserialize.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum TrunkPrState {
    NotReady,
    Pending,
    Testing,
    TestsPassed,
    Merged,
    Failed,
    Cancelled,
    PendingFailure,
    /// A state value this client doesn't recognize yet.
    Unknown(String),
}

impl From<String> for TrunkPrState {
    fn from(value: String) -> Self {
        match value.as_str() {
            "not_ready" => Self::NotReady,
            "pending" => Self::Pending,
            "testing" => Self::Testing,
            "tests_passed" => Self::TestsPassed,
            "merged" => Self::Merged,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "pending_failure" => Self::PendingFailure,
            _ => Self::Unknown(value),
        }
    }
}

impl From<TrunkPrState> for String {
    fn from(value: TrunkPrState) -> Self {
        match value {
            TrunkPrState::NotReady => "not_ready".to_owned(),
            TrunkPrState::Pending => "pending".to_owned(),
            TrunkPrState::Testing => "testing".to_owned(),
            TrunkPrState::TestsPassed => "tests_passed".to_owned(),
            TrunkPrState::Merged => "merged".to_owned(),
            TrunkPrState::Failed => "failed".to_owned(),
            TrunkPrState::Cancelled => "cancelled".to_owned(),
            TrunkPrState::PendingFailure => "pending_failure".to_owned(),
            TrunkPrState::Unknown(raw) => raw,
        }
    }
}

/// One of the four documented Trunk queue states, or
/// [`Unknown`](TrunkQueueState::Unknown) preserving the raw wire string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum TrunkQueueState {
    Running,
    Paused,
    Draining,
    SwitchingModes,
    /// A state value this client doesn't recognize yet.
    Unknown(String),
}

impl From<String> for TrunkQueueState {
    fn from(value: String) -> Self {
        match value.as_str() {
            "running" => Self::Running,
            "paused" => Self::Paused,
            "draining" => Self::Draining,
            "switching_modes" => Self::SwitchingModes,
            _ => Self::Unknown(value),
        }
    }
}

impl From<TrunkQueueState> for String {
    fn from(value: TrunkQueueState) -> Self {
        match value {
            TrunkQueueState::Running => "running".to_owned(),
            TrunkQueueState::Paused => "paused".to_owned(),
            TrunkQueueState::Draining => "draining".to_owned(),
            TrunkQueueState::SwitchingModes => "switching_modes".to_owned(),
            TrunkQueueState::Unknown(raw) => raw,
        }
    }
}

// ── submitPullRequest ─────────────────────────────────────────────────────────

/// Body for `POST /v1/submitPullRequest`. `priority`/`noBatch` are optional;
/// a bare 200 `{}` is the success response, so there is no matching response
/// type — see [`crate::TrunkClient::submit_pull_request`].
#[derive(Debug, Clone, Serialize, bon::Builder)]
#[builder(on(String, into))]
#[serde(rename_all = "camelCase")]
pub struct SubmitPullRequestRequest {
    pub repo: TrunkRepoRef,
    pub pr: TrunkPrRef,
    pub target_branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<TrunkPriority>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_batch: Option<bool>,
}

// ── getSubmittedPullRequest / listPullRequests / getQueue responses ──────────

/// Response of `getSubmittedPullRequest`, and the shape of each entry in
/// `listPullRequests.pullRequests` / `getQueue.enqueuedPullRequests`.
///
/// `bon::Builder` is kept here even though nothing constructs one today
/// (this is a `Deserialize`-only wire response): checkleft's
/// `rust/giant-structs` check mandates the builder derive on any struct
/// with more than 5 named fields, and this one has more than 5.
#[derive(Debug, Clone, Deserialize, bon::Builder)]
#[builder(on(String, into))]
#[serde(rename_all = "camelCase")]
pub struct TrunkPullRequest {
    pub id: String,
    pub state: TrunkPrState,
    #[serde(default)]
    pub readiness: Option<TrunkReadiness>,
    #[serde(default)]
    pub state_changed_at: Option<String>,
    #[serde(default)]
    pub priority_value: Option<i64>,
    #[serde(default)]
    pub priority_name: Option<String>,
    pub pr_number: u64,
    #[serde(default)]
    pub pr_title: Option<String>,
    #[serde(default)]
    pub pr_sha: Option<String>,
    #[serde(default)]
    pub pr_base_branch: Option<String>,
    #[serde(default)]
    pub pr_author: Option<String>,
}

/// Body for `POST /v1/listPullRequests`.
#[derive(Debug, Clone, Serialize, bon::Builder)]
#[builder(on(String, into))]
#[serde(rename_all = "camelCase")]
pub struct ListPullRequestsRequest {
    pub repo: TrunkRepoRef,
    pub target_branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<TrunkPrState>,
    /// Filters concluded PRs by timestamp — the reconciliation backstop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// 1-100, default 50.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take: Option<u32>,
}

/// Response of `listPullRequests`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPullRequestsResponse {
    #[serde(default)]
    pub pull_requests: Vec<TrunkPullRequest>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// Response of `getQueue`: one call returns every enqueued PR.
///
/// `bon::Builder` is kept here for the same reason as [`TrunkPullRequest`]:
/// checkleft's `rust/giant-structs` check mandates it above 5 fields
/// regardless of whether a construction site exists yet.
#[derive(Debug, Clone, Deserialize, bon::Builder)]
#[builder(on(String, into))]
#[serde(rename_all = "camelCase")]
pub struct TrunkQueue {
    pub state: TrunkQueueState,
    pub branch: String,
    #[serde(default)]
    pub concurrency: Option<u32>,
    #[serde(default)]
    pub testing_timeout_minutes: Option<u32>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub batch: Option<bool>,
    #[serde(default)]
    pub enqueued_pull_requests: Vec<TrunkPullRequest>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── state round-tripping ──────────────────────────────────────────────

    #[test]
    fn pr_state_round_trips_known_variants() {
        let cases = [
            (TrunkPrState::NotReady, "not_ready"),
            (TrunkPrState::Pending, "pending"),
            (TrunkPrState::Testing, "testing"),
            (TrunkPrState::TestsPassed, "tests_passed"),
            (TrunkPrState::Merged, "merged"),
            (TrunkPrState::Failed, "failed"),
            (TrunkPrState::Cancelled, "cancelled"),
            (TrunkPrState::PendingFailure, "pending_failure"),
        ];
        for (state, wire) in cases {
            let value = serde_json::to_value(&state).unwrap();
            assert_eq!(value, json!(wire));
            let parsed: TrunkPrState = serde_json::from_value(value).unwrap();
            assert_eq!(parsed, state);
        }
    }

    #[test]
    fn pr_state_tolerates_unknown_variants() {
        let parsed: TrunkPrState = serde_json::from_value(json!("some_future_state")).unwrap();
        assert_eq!(parsed, TrunkPrState::Unknown("some_future_state".to_owned()));
        // Round-trips the raw string back out rather than losing it.
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json!("some_future_state"));
    }

    #[test]
    fn queue_state_round_trips_known_variants() {
        let cases = [
            (TrunkQueueState::Running, "running"),
            (TrunkQueueState::Paused, "paused"),
            (TrunkQueueState::Draining, "draining"),
            (TrunkQueueState::SwitchingModes, "switching_modes"),
        ];
        for (state, wire) in cases {
            let value = serde_json::to_value(&state).unwrap();
            assert_eq!(value, json!(wire));
            let parsed: TrunkQueueState = serde_json::from_value(value).unwrap();
            assert_eq!(parsed, state);
        }
    }

    #[test]
    fn queue_state_tolerates_unknown_variants() {
        let parsed: TrunkQueueState = serde_json::from_value(json!("quarantined")).unwrap();
        assert_eq!(parsed, TrunkQueueState::Unknown("quarantined".to_owned()));
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json!("quarantined"));
    }

    // ── request serialization ─────────────────────────────────────────────

    #[test]
    fn submit_request_serializes_camel_case_and_omits_unset_optionals() {
        let request = SubmitPullRequestRequest::builder()
            .repo(TrunkRepoRef::new("github.com", "brianduff", "flunge"))
            .pr(TrunkPrRef::new(978))
            .target_branch("main")
            .build();
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["repo"]["owner"], "brianduff");
        assert_eq!(body["pr"]["number"], 978);
        assert_eq!(body["targetBranch"], "main");
        assert!(body.get("priority").is_none());
        assert!(body.get("noBatch").is_none());
    }

    #[test]
    fn submit_request_includes_priority_and_no_batch_when_set() {
        let request = SubmitPullRequestRequest::builder()
            .repo(TrunkRepoRef::new("github.com", "brianduff", "flunge"))
            .pr(TrunkPrRef::new(978))
            .target_branch("main")
            .priority(TrunkPriority::Name("high".to_owned()))
            .no_batch(true)
            .build();
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["priority"], "high");
        assert_eq!(body["noBatch"], true);
    }

    #[test]
    fn pr_lookup_serializes_target_branch_as_camel_case() {
        let lookup = TrunkPrLookup::new(
            TrunkRepoRef::new("github.com", "brianduff", "flunge"),
            TrunkPrRef::new(978),
            "main",
        );
        let body = serde_json::to_value(&lookup).unwrap();
        assert_eq!(body["targetBranch"], "main");
        assert_eq!(body["pr"]["number"], 978);
    }

    // ── fixture-backed response deserialization ────────────────────────────

    #[test]
    fn deserializes_submitted_pull_request_fixture() {
        let pr: TrunkPullRequest =
            serde_json::from_str(include_str!("testdata/submitted_pull_request.json")).expect("valid fixture");
        assert_eq!(pr.id, "entry_123");
        assert_eq!(pr.state, TrunkPrState::Testing);
        assert_eq!(pr.pr_number, 978);
        assert_eq!(pr.pr_author.as_deref(), Some("brianduff"));
    }

    #[test]
    fn deserializes_list_pull_requests_fixture() {
        let response: ListPullRequestsResponse =
            serde_json::from_str(include_str!("testdata/list_pull_requests.json")).expect("valid fixture");
        assert_eq!(response.pull_requests.len(), 2);
        assert_eq!(response.pull_requests[0].state, TrunkPrState::Merged);
        assert_eq!(
            response.pull_requests[1].state,
            TrunkPrState::Unknown("quarantined".to_owned())
        );
        assert_eq!(response.next_cursor.as_deref(), Some("cursor-abc"));
    }

    #[test]
    fn deserializes_queue_fixture() {
        let queue: TrunkQueue = serde_json::from_str(include_str!("testdata/queue.json")).expect("valid fixture");
        assert_eq!(queue.state, TrunkQueueState::Running);
        assert_eq!(queue.branch, "main");
        assert_eq!(queue.enqueued_pull_requests.len(), 1);
        assert_eq!(queue.enqueued_pull_requests[0].pr_number, 978);
    }

    #[test]
    fn deserializes_paused_queue_fixture_with_unknown_state() {
        let queue: TrunkQueue =
            serde_json::from_str(include_str!("testdata/queue_paused_unknown_state.json")).expect("valid fixture");
        assert_eq!(queue.state, TrunkQueueState::Paused);
        assert_eq!(
            queue.enqueued_pull_requests[0].state,
            TrunkPrState::Unknown("brand_new_state".to_owned())
        );
    }
}
