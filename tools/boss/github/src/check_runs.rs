//! CI check-run helpers: provider classification, job-id parsing, and
//! the REST `/commits/{sha}/check-runs` fetcher used by the merge-queue
//! rebounce detector.

use std::process::Stdio;

use tokio::process::Command;

/// CI provider inferred from a check's `targetUrl` host. The CI-watch
/// `CiLogReader` impls (Buildkite + GitHub Actions) dispatch on this;
/// the `Other` variant captures anything we don't know how to read
/// (status contexts from third-party services like Codecov, Sonar, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiProvider {
    Buildkite,
    GithubActions,
    Other,
}

/// One required check that failed at probe time. Captured pre-spawn so
/// the `ci_remediations.failed_checks` JSON is faithful to what the
/// engine saw and the worker prompt embeds the same data.
///
/// `conclusion` is GitHub's value (`FAILURE`, `TIMED_OUT`, `CANCELLED`,
/// `STARTUP_FAILURE`, `ACTION_REQUIRED`, `STALE`). `target_url` points
/// at the provider's job page; `provider` is inferred from its host;
/// `provider_job_id` is parsed from the URL when possible and `None`
/// when the format is unrecognised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredCheckFailure {
    pub name: String,
    pub conclusion: String,
    pub target_url: String,
    pub provider: CiProvider,
    pub provider_job_id: Option<String>,
}

/// Infer the CI provider from a check's `targetUrl` host.
pub fn provider_for_url(url: &str) -> CiProvider {
    if url.is_empty() {
        return CiProvider::Other;
    }
    let lower = url.to_ascii_lowercase();
    if lower.contains("buildkite.com") {
        return CiProvider::Buildkite;
    }
    // GitHub Actions URLs look like:
    //   https://github.com/<owner>/<repo>/actions/runs/<run-id>/job/<job-id>
    // (or the older /check-runs/ form). Either format → GHA.
    if lower.contains("github.com") && (lower.contains("/actions/") || lower.contains("/check-runs/"))
    {
        return CiProvider::GithubActions;
    }
    CiProvider::Other
}

/// Extract the provider's job id from a `targetUrl`. Buildkite job
/// ids ride in the URL fragment (`…/builds/<n>#<job-uuid>`); GitHub
/// Actions job ids are the last path segment after `/job/`. Returns
/// `None` for URLs that don't match either pattern — the worker
/// prompt then shows the raw URL and the worker shells out manually.
pub fn parse_provider_job_id(provider: CiProvider, url: &str) -> Option<String> {
    match provider {
        CiProvider::Buildkite => url.split_once('#').map(|(_, frag)| frag.to_owned()),
        CiProvider::GithubActions => {
            // …/actions/runs/<run-id>/job/<job-id>[?…]
            let stripped = url.split('?').next().unwrap_or(url);
            stripped
                .rsplit_once("/job/")
                .map(|(_, tail)| tail.trim_end_matches('/').to_owned())
        }
        CiProvider::Other => None,
    }
}

/// Fetch the failing CI check runs for a specific commit SHA via the GitHub
/// REST API. Used for merge-queue rebounce detection where the failing SHA is
/// the synthetic merge commit (`before_commit_sha`) assembled by the queue on
/// a `gh-readonly-queue/*` branch — not the PR head.
///
/// `owner_repo` must be in `"owner/repo"` form (e.g. `"spinyfin/mono"`).
///
/// Returns failing checks as `RequiredCheckFailure` records so the
/// `ci_remediations.failed_checks` JSON can carry the build URL, job id,
/// and provider — the same data the CI-fix revision directive shows the
/// worker for per-branch failures. Best-effort: any network or parse error
/// returns an empty vec; the insert still succeeds with `"[]"` so the worker
/// can attempt manual discovery.
pub async fn fetch_failing_checks_for_commit(
    owner_repo: &str,
    commit_sha: &str,
) -> Vec<RequiredCheckFailure> {
    let api_path = format!("repos/{owner_repo}/commits/{commit_sha}/check-runs");
    let output = Command::new("gh")
        .args(["api", &api_path])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await;
    let output = match output {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::debug!(
                commit_sha,
                stderr = %String::from_utf8_lossy(&o.stderr),
                "github: gh api check-runs failed for merge-queue commit",
            );
            return Vec::new();
        }
        Err(err) => {
            tracing::debug!(
                ?err,
                commit_sha,
                "github: failed to spawn gh for check-runs",
            );
            return Vec::new();
        }
    };
    parse_check_runs_for_failures(&output.stdout)
}

/// Pure parser for the GitHub REST `/commits/{sha}/check-runs` response body.
/// Returns `RequiredCheckFailure` records for every completed check with a
/// failure-class conclusion. Extracted as a pure function for unit-testing
/// without a live `gh` call.
///
/// GitHub REST check-run conclusions: `success`, `failure`, `neutral`,
/// `cancelled`, `timed_out`, `action_required`, `skipped`, `stale`.
/// Buildkite also emits `startup_failure` (a Buildkite-specific value that
/// appears in the field even though it isn't in the GitHub schema).
pub fn parse_check_runs_for_failures(body: &[u8]) -> Vec<RequiredCheckFailure> {
    let body: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let runs = match body["check_runs"].as_array() {
        Some(arr) => arr,
        None => return Vec::new(),
    };
    let mut failures = Vec::new();
    for run in runs {
        if run["status"].as_str() != Some("completed") {
            continue;
        }
        let conclusion = match run["conclusion"].as_str() {
            Some(c) => c,
            None => continue,
        };
        if !matches!(
            conclusion,
            "failure" | "timed_out" | "action_required" | "startup_failure"
        ) {
            continue;
        }
        let name = run["name"].as_str().unwrap_or_default().to_owned();
        // `details_url` points to the CI provider's build page (Buildkite
        // URL, GHA run URL, etc.) — the equivalent of GraphQL `targetUrl`.
        // Fall back to `html_url` (the GitHub check page) when absent.
        let target_url = run["details_url"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| run["html_url"].as_str())
            .unwrap_or_default()
            .to_owned();
        let provider = provider_for_url(&target_url);
        let provider_job_id = parse_provider_job_id(provider, &target_url);
        failures.push(RequiredCheckFailure {
            name,
            conclusion: conclusion.to_owned(),
            target_url,
            provider,
            provider_job_id,
        });
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_check_runs_for_failures_returns_failing_entries() {
        let body = br#"{
            "check_runs": [
                {
                    "name": "ci/build",
                    "status": "completed",
                    "conclusion": "failure",
                    "details_url": "https://buildkite.com/org/mono/builds/1666#job-abc",
                    "html_url": "https://github.com/org/repo/runs/123"
                },
                {
                    "name": "ci/lint",
                    "status": "completed",
                    "conclusion": "success",
                    "details_url": "https://buildkite.com/org/mono/builds/1666#job-xyz",
                    "html_url": "https://github.com/org/repo/runs/124"
                },
                {
                    "name": "ci/deploy",
                    "status": "in_progress",
                    "conclusion": null,
                    "details_url": "https://buildkite.com/org/mono/builds/1667",
                    "html_url": "https://github.com/org/repo/runs/125"
                }
            ]
        }"#;
        let failures = parse_check_runs_for_failures(body);
        assert_eq!(failures.len(), 1, "only the failed completed check");
        assert_eq!(failures[0].name, "ci/build");
        assert_eq!(failures[0].conclusion, "failure");
        assert_eq!(
            failures[0].target_url,
            "https://buildkite.com/org/mono/builds/1666#job-abc"
        );
        assert_eq!(failures[0].provider, CiProvider::Buildkite);
        assert_eq!(failures[0].provider_job_id.as_deref(), Some("job-abc"));
    }

    #[test]
    fn parse_check_runs_for_failures_timed_out_and_action_required() {
        let body = br#"{
            "check_runs": [
                {
                    "name": "slow-check",
                    "status": "completed",
                    "conclusion": "timed_out",
                    "details_url": "https://buildkite.com/org/p/builds/42",
                    "html_url": ""
                },
                {
                    "name": "manual-check",
                    "status": "completed",
                    "conclusion": "action_required",
                    "details_url": "https://github.com/org/repo/actions/runs/99/job/7",
                    "html_url": ""
                }
            ]
        }"#;
        let failures = parse_check_runs_for_failures(body);
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].name, "slow-check");
        assert_eq!(failures[1].name, "manual-check");
        assert_eq!(failures[1].provider, CiProvider::GithubActions);
    }

    #[test]
    fn parse_check_runs_for_failures_falls_back_to_html_url_when_details_url_empty() {
        let body = br#"{
            "check_runs": [
                {
                    "name": "check",
                    "status": "completed",
                    "conclusion": "failure",
                    "details_url": "",
                    "html_url": "https://github.com/org/repo/runs/42"
                }
            ]
        }"#;
        let failures = parse_check_runs_for_failures(body);
        assert_eq!(failures.len(), 1);
        assert_eq!(
            failures[0].target_url,
            "https://github.com/org/repo/runs/42"
        );
    }

    #[test]
    fn parse_check_runs_for_failures_empty_on_malformed_json() {
        assert!(parse_check_runs_for_failures(b"not json").is_empty());
        assert!(parse_check_runs_for_failures(b"{}").is_empty());
    }

    /// `provider_for_url` infers the CI provider purely from the host /
    /// path of the check's `targetUrl`. Buildkite is host-only; GitHub
    /// Actions additionally requires an `/actions/` or `/check-runs/`
    /// segment; everything else (including a bare github.com URL and the
    /// empty string) is `Other`. Matching is case-insensitive.
    #[test]
    fn provider_for_url_classifies_hosts() {
        use super::CiProvider::*;
        let cases: &[(&str, super::CiProvider)] = &[
            // Buildkite — host match is sufficient.
            ("https://buildkite.com/acme/mono/builds/42", Buildkite),
            (
                "https://buildkite.com/acme/mono/builds/42#01h-job-uuid",
                Buildkite,
            ),
            // GitHub Actions — github.com host PLUS an /actions/ or
            // /check-runs/ segment.
            (
                "https://github.com/anthropic/mono/actions/runs/123/job/456",
                GithubActions,
            ),
            (
                "https://github.com/anthropic/mono/check-runs/789",
                GithubActions,
            ),
            // Bare github.com without either segment → Other (e.g. a PR
            // or status URL we can't read logs from).
            ("https://github.com/anthropic/mono/pull/7", Other),
            // Empty string and unrelated third-party hosts → Other.
            ("", Other),
            ("https://app.codecov.io/gh/anthropic/mono", Other),
            ("https://sonarcloud.io/dashboard?id=mono", Other),
            // Case-insensitivity: an upper/mixed-case host still matches.
            (
                "HTTPS://BuildKite.COM/Acme/Mono/Builds/42",
                Buildkite,
            ),
            (
                "https://GITHUB.com/anthropic/mono/ACTIONS/runs/1/job/2",
                GithubActions,
            ),
        ];
        for (url, expected) in cases {
            assert_eq!(
                super::provider_for_url(url),
                *expected,
                "provider_for_url({url:?})",
            );
        }
    }

    /// `parse_provider_job_id` extracts the provider-native job id from the
    /// `targetUrl`. Buildkite ids ride in the URL fragment (after `#`);
    /// GitHub Actions ids are the last path segment after `/job/` (with any
    /// `?query` stripped and a trailing `/` trimmed). Anything that doesn't
    /// match — or `CiProvider::Other` — yields `None`.
    #[test]
    fn parse_provider_job_id_extracts_or_none() {
        use super::CiProvider::*;
        // Buildkite: fragment after '#'.
        assert_eq!(
            super::parse_provider_job_id(
                Buildkite,
                "https://buildkite.com/acme/mono/builds/123#job-uuid",
            ),
            Some("job-uuid".to_owned()),
        );
        // Buildkite with no fragment → None.
        assert_eq!(
            super::parse_provider_job_id(Buildkite, "https://buildkite.com/acme/mono/builds/123"),
            None,
        );
        // GitHub Actions: last segment after '/job/'.
        assert_eq!(
            super::parse_provider_job_id(
                GithubActions,
                "https://github.com/anthropic/mono/actions/runs/12345/job/67890",
            ),
            Some("67890".to_owned()),
        );
        // GitHub Actions: '?query' is stripped before extracting.
        assert_eq!(
            super::parse_provider_job_id(
                GithubActions,
                "https://github.com/anthropic/mono/actions/runs/12345/job/67890?check_suite_focus=true",
            ),
            Some("67890".to_owned()),
        );
        // GitHub Actions: trailing '/' is trimmed.
        assert_eq!(
            super::parse_provider_job_id(
                GithubActions,
                "https://github.com/anthropic/mono/actions/runs/12345/job/67890/",
            ),
            Some("67890".to_owned()),
        );
        // GitHub Actions URL with no '/job/' segment → None.
        assert_eq!(
            super::parse_provider_job_id(
                GithubActions,
                "https://github.com/anthropic/mono/actions/runs/12345",
            ),
            None,
        );
        // CiProvider::Other never parses a job id, regardless of the URL.
        assert_eq!(
            super::parse_provider_job_id(Other, "https://buildkite.com/acme/mono/builds/1#x"),
            None,
        );
        assert_eq!(super::parse_provider_job_id(Other, ""), None);
    }
}
