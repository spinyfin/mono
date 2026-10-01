use serde::{Deserialize, Serialize};

/// Engine-side health snapshot returned by
/// [`FrontendEvent::EngineHealthResult`]. The chore that introduced
/// this surface (#699) was triggered by silent summarization failure
/// when `ANTHROPIC_API_KEY` is missing — the macOS app showed nothing,
/// the user only noticed because live-status sentences never appeared.
///
/// `issues` is the structured list the UI renders. It is intentionally
/// extensible: the chore notes other required config (engine socket
/// path, etc.) "likely also applies", so the shape is "report a list
/// of named problems" rather than a one-off boolean.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct EngineHealthReport {
    /// Stamped Boss version of the running engine (`1.0.N` on a
    /// release tag, `1.0.N-dev-<sha>` otherwise, or `unknown` if
    /// unstamped). Same string as `EngineVersionResult.version`.
    #[serde(default)]
    #[builder(default)]
    pub engine_version: String,
    /// Full git commit sha of the running engine (or `unknown`). Same
    /// string as `EngineVersionResult.git_sha`.
    #[serde(default)]
    #[builder(default)]
    pub engine_git_sha: String,
    /// True iff the engine's agent config had an `ANTHROPIC_API_KEY`
    /// at startup. Surfaced as a top-level bit (rather than only via
    /// the `issues` list) so a CLI consumer doing
    /// `boss engine health --json | jq .anthropic_api_key_present`
    /// gets a single boolean without having to grep through the issues
    /// array.
    pub anthropic_api_key_present: bool,
    /// True when dispatch is globally paused. A paused engine will not
    /// dispatch new executions from any source until explicitly resumed via
    /// `SetDispatchPaused { paused: false }`. Surfaced as a top-level field
    /// (in addition to the `issues` list entry) so CLI consumers can check
    /// it with a simple `jq .dispatch_paused`.
    #[serde(default)]
    pub dispatch_paused: bool,
    /// True when automation-originated activity is globally paused. See
    /// `SetAutomationPaused` — independent of `dispatch_paused`. Surfaced
    /// as a top-level field for the same `jq .automation_paused`
    /// convenience `dispatch_paused` provides.
    #[serde(default)]
    pub automation_paused: bool,
    /// Review guides this engine process re-enqueued at startup because
    /// they had failed before start on a different build. `None` when it
    /// re-enqueued nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_guide_reenqueue: Option<ReviewGuideReenqueueSummary>,
    /// Issues the UI should render, in display order (highest priority
    /// first). Empty when the engine is healthy.
    pub issues: Vec<EngineHealthIssue>,
    /// Active pre-start spawn-failure streak alerts, one per (driver,
    /// worker kind) combination currently at or past the engine's
    /// consecutive-failure threshold. Each also appears in `issues` as a
    /// `pre_start_spawn_failure_streak` entry carrying the same facts as
    /// prose; this is the structured form for CLI / `jq` consumers. Empty
    /// when no combination is in a streak.
    #[serde(default)]
    pub spawn_failure_streaks: Vec<SpawnFailureStreak>,
}

/// One fleet-level alert: a (driver, worker kind) combination has failed
/// to spawn `consecutive_failures` times in a row before any worker pane
/// existed, with no successful spawn for that combination in between.
/// Raised by the engine at its streak threshold, updated in place on every
/// further failure, and dropped from the report on the combination's next
/// successful spawn.
#[derive(bon::Builder, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[builder(on(String, into))]
pub struct SpawnFailureStreak {
    /// Driver slug the failing spawns resolved to (e.g. `codex`).
    pub driver: String,
    /// Worker kind label (`standard`, `reviewer`, `triage`,
    /// `answer-agent`, `review-guide`).
    pub worker_kind: String,
    /// Pre-start failures since the combination's last successful spawn.
    /// The current count, never a capped or deduplicated one.
    pub consecutive_failures: u32,
    /// Unix epoch seconds of the first failure in this streak.
    pub first_failure_epoch_s: i64,
    /// Unix epoch seconds of the most recent failure in this streak.
    pub latest_failure_epoch_s: i64,
    /// Full error chain of the most recent failure, untruncated.
    pub latest_error: String,
    /// Execution whose spawn produced `latest_error`.
    pub latest_execution_id: String,
}

/// What the startup pass that retries pre-start-failed review guides did.
/// Shared by the engine health report and `bossctl live-status debug`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewGuideReenqueueSummary {
    /// Build identity of the engine that ran the pass.
    pub build: String,
    /// Number of guides re-enqueued (`pr_urls.len()`).
    pub count: usize,
    /// The PRs whose guides were re-enqueued.
    pub pr_urls: Vec<String>,
}

/// One UI-actionable engine-health issue. Carries pre-rendered title
/// and body strings so the macOS app can show the banner without
/// translating engine state into prose at the call site. The engine
/// owns the wording; the UI owns the styling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineHealthIssue {
    /// Stable lowercase snake_case kind identifier. The UI uses this
    /// as a styling / icon / dismissal-state key. Initial values:
    /// - `missing_anthropic_api_key` — engine started without an
    ///   `ANTHROPIC_API_KEY`; summarizer cannot succeed.
    pub kind: String,
    /// `"error"` (a user-visible feature is broken) or `"warning"`
    /// (a background feature is degraded). The banner styling keys
    /// off this so an error renders in red and a warning in amber.
    pub severity: String,
    /// One-line title rendered inline in the banner.
    pub title: String,
    /// Multi-line body with the remediation steps (e.g. which env var
    /// to set and where to restart). The UI wraps and renders verbatim.
    pub body: String,
}
