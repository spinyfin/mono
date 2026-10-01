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
    /// Newest published `boss-v` release the app's updater has reported
    /// to this engine (`1.0.N`), or `None` if nothing has been reported
    /// since the engine started. The engine never polls for releases
    /// itself — see [`crate::FrontendRequest::ReportNewestPublishedRelease`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_published_release: Option<String>,
    /// How `engine_version` compares with `newest_published_release`.
    /// See [`engine_release_freshness`].
    #[serde(default)]
    #[builder(default)]
    pub engine_release_status: EngineReleaseStatus,
    /// True when `engine_version` is a `-dev-` build. A dev build is
    /// still reported as behind when it is; it is just never
    /// auto-installed over.
    #[serde(default)]
    #[builder(default)]
    pub engine_is_dev_build: bool,
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

/// Stable `kind` of the [`EngineHealthIssue`] raised when the running
/// engine is older than the newest published release.
pub const ENGINE_BEHIND_PUBLISHED_RELEASE_KIND: &str = "engine_behind_published_release";

/// Running engine vs. newest published `boss-v` release.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EngineReleaseStatus {
    /// The engine is at or ahead of the newest published release.
    Current,
    /// A newer release is published than the one the engine runs.
    Behind,
    /// Cannot tell: the engine is unstamped, or no newest release has
    /// been reported (or it did not parse).
    #[default]
    Unknown,
}

impl EngineReleaseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Behind => "behind",
            Self::Unknown => "unknown",
        }
    }
}

/// Result of [`engine_release_freshness`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineReleaseFreshness {
    pub status: EngineReleaseStatus,
    pub is_dev_build: bool,
}

/// Parse a numeric `MAJOR.MINOR.PATCH` release version. Rejects
/// anything else, including a `-dev-<sha>` suffix.
pub fn parse_release_version(raw: &str) -> Option<(u64, u64, u64)> {
    let mut parts = raw.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Compare the running engine's stamped version (`1.0.N`,
/// `1.0.N-dev-<sha>`, or `unknown`) with the newest published release
/// (`1.0.N`).
///
/// A dev build is stamped with the last release tag it descends from,
/// so it compares by that base: `1.0.5-dev-abc` is behind `1.0.6` and
/// current against `1.0.5`. The dev flag is reported separately so a
/// caller can show the gap without ever auto-installing over it.
pub fn engine_release_freshness(running: &str, newest_published: Option<&str>) -> EngineReleaseFreshness {
    let is_dev_build = running.contains("-dev-");
    let base = running.split_once('-').map_or(running, |(base, _)| base);
    let status = match (
        parse_release_version(base),
        newest_published.and_then(parse_release_version),
    ) {
        (Some(running), Some(newest)) if running < newest => EngineReleaseStatus::Behind,
        (Some(_), Some(_)) => EngineReleaseStatus::Current,
        _ => EngineReleaseStatus::Unknown,
    };
    EngineReleaseFreshness { status, is_dev_build }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn freshness(running: &str, newest: Option<&str>) -> (EngineReleaseStatus, bool) {
        let f = engine_release_freshness(running, newest);
        (f.status, f.is_dev_build)
    }

    #[test]
    fn older_engine_is_behind() {
        assert_eq!(
            freshness("1.0.685", Some("1.0.686")),
            (EngineReleaseStatus::Behind, false)
        );
        // Numeric, not lexical: 1.0.99 < 1.0.100.
        assert_eq!(
            freshness("1.0.99", Some("1.0.100")),
            (EngineReleaseStatus::Behind, false)
        );
    }

    #[test]
    fn equal_or_newer_engine_is_current() {
        assert_eq!(
            freshness("1.0.686", Some("1.0.686")),
            (EngineReleaseStatus::Current, false)
        );
        assert_eq!(
            freshness("1.0.687", Some("1.0.686")),
            (EngineReleaseStatus::Current, false)
        );
    }

    #[test]
    fn dev_build_is_compared_by_base_and_flagged() {
        // Behind is still reported for a dev build, never hidden.
        assert_eq!(
            freshness("1.0.685-dev-abc1234", Some("1.0.686")),
            (EngineReleaseStatus::Behind, true)
        );
        // A dev build past the newest tag is ahead of it.
        assert_eq!(
            freshness("1.0.686-dev-abc1234", Some("1.0.686")),
            (EngineReleaseStatus::Current, true)
        );
    }

    #[test]
    fn unknown_versions_are_unknown_not_current() {
        assert_eq!(
            freshness("unknown", Some("1.0.686")),
            (EngineReleaseStatus::Unknown, false)
        );
        assert_eq!(freshness("1.0.686", None), (EngineReleaseStatus::Unknown, false));
        assert_eq!(
            freshness("1.0.686", Some("boss-v1.0.686")),
            (EngineReleaseStatus::Unknown, false)
        );
        assert_eq!(freshness("", Some("1.0.686")), (EngineReleaseStatus::Unknown, false));
    }

    #[test]
    fn parse_release_version_rejects_non_release_shapes() {
        assert_eq!(parse_release_version("1.0.686"), Some((1, 0, 686)));
        assert_eq!(parse_release_version("1.0"), None);
        assert_eq!(parse_release_version("1.0.686.1"), None);
        assert_eq!(parse_release_version("1.0.686-dev-abc"), None);
    }

    #[test]
    fn health_report_without_release_fields_deserializes_as_unknown() {
        // An engine that predates these fields must read as "unknown",
        // never as "current".
        let report: EngineHealthReport =
            serde_json::from_str(r#"{"anthropic_api_key_present":true,"issues":[]}"#).unwrap();
        assert_eq!(report.newest_published_release, None);
        assert_eq!(report.engine_release_status, EngineReleaseStatus::Unknown);
        assert!(!report.engine_is_dev_build);
    }
}
