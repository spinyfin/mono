//! CI check-run helpers: provider classification, job-id parsing, and
//! the REST fetchers used by the merge-queue rebounce detector
//! (`/commits/{sha}/check-runs` and the legacy `/commits/{sha}/status`
//! combined-status endpoint that Buildkite still posts to on mono).

use crate::gh_runner::gh_output;

/// Verdict bucket a legacy commit-status / GraphQL `StatusContext`
/// `state` value maps to. Shared by the GraphQL rollup classifier
/// (`merge_poller::normalize_leaf`) and the REST `/commits/{sha}/status`
/// parser ([`parse_commit_statuses_for_failures`]) so the two paths
/// cannot drift on which states count as failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusContextVerdict {
    /// Terminal success (`SUCCESS`).
    Pass,
    /// Terminal failure. `conclusion` is the uppercased state token
    /// (`FAILURE` / `ERROR`) kept verbatim for the worker prompt /
    /// `ci_remediations.failed_checks` JSON — matching the GraphQL
    /// rollup path's spelling.
    Fail { conclusion: String },
    /// Non-terminal (`PENDING` / `EXPECTED` / unknown).
    InFlight,
}

/// Classify a legacy commit-status / `StatusContext` `state` value.
/// Accepts either case; GitHub's REST combined-status endpoint returns
/// lowercase (`failure`) while GraphQL returns uppercase (`FAILURE`).
///
/// Values per GitHub's commit-status API: SUCCESS / FAILURE / ERROR /
/// PENDING / EXPECTED. Only `FAILURE` and `ERROR` are terminal fails.
pub fn classify_status_context_state(state: &str) -> StatusContextVerdict {
    let upper = state.to_ascii_uppercase();
    match upper.as_str() {
        "SUCCESS" => StatusContextVerdict::Pass,
        "FAILURE" | "ERROR" => StatusContextVerdict::Fail { conclusion: upper },
        // PENDING (running), EXPECTED (branch protection lists the
        // context but no run has reported yet), empty, or anything
        // else GitHub may add later → wait for a terminal verdict.
        _ => StatusContextVerdict::InFlight,
    }
}

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
/// `conclusion` is GitHub's value, lowercased as returned by the REST
/// API (`failure`, `timed_out`, `action_required`, `startup_failure` —
/// see [`parse_check_runs_for_failures`] for why `cancelled`/`stale`
/// are excluded on this merge-queue path). `target_url` points
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
    if lower.contains("github.com") && (lower.contains("/actions/") || lower.contains("/check-runs/")) {
        return CiProvider::GithubActions;
    }
    CiProvider::Other
}

/// Extract the provider's job id from a `targetUrl`. Buildkite job
/// ids ride in the URL fragment (`…/builds/<n>#<job-uuid>`); GitHub
/// Actions job ids are the last path segment after `/job/`. Returns
/// `None` for URLs that don't match either pattern — the worker
/// prompt then shows the raw URL and the worker shells out manually.
///
/// This is the single canonical implementation — `ci_log_reader`'s
/// `parse_buildkite_job_id`/`parse_gha_job_id` delegate here rather than
/// duplicating the logic, so the empty-id guard and fragment stripping
/// below can't drift between the two call sites again.
pub fn parse_provider_job_id(provider: CiProvider, url: &str) -> Option<String> {
    match provider {
        CiProvider::Buildkite => {
            let (_, frag) = url.split_once('#')?;
            if frag.is_empty() { None } else { Some(frag.to_owned()) }
        }
        CiProvider::GithubActions => {
            // …/actions/runs/<run-id>/job/<job-id>[?…][#…]
            let stripped = url.split('?').next().unwrap_or(url);
            let stripped = stripped.split('#').next().unwrap_or(stripped);
            let (_, tail) = stripped.rsplit_once("/job/")?;
            let id = tail.trim_end_matches('/');
            if id.is_empty() { None } else { Some(id.to_owned()) }
        }
        CiProvider::Other => None,
    }
}

/// Fetch the failing CI checks for a specific commit SHA via the GitHub
/// REST API. Used for merge-queue rebounce detection where the failing SHA is
/// the synthetic merge commit (`before_commit_sha`) assembled by the queue on
/// a `gh-readonly-queue/*` branch — not the PR head.
///
/// Reads **both** surfaces GitHub exposes for a commit's CI:
///   - `/commits/{sha}/check-runs` — modern check runs (GitHub Actions,
///     most CI integrations).
///   - `/commits/{sha}/status` — legacy commit statuses (Buildkite on
///     mono still posts only these; check-runs returns `total_count: 0`
///     for every mono merge-queue synthetic commit).
///
/// Status-context failures are classified with
/// [`classify_status_context_state`] — the same mapping the GraphQL
/// rollup path uses in `merge_poller::normalize_leaf` — so the two
/// "did CI fail" answers cannot drift apart.
///
/// `owner_repo` must be in `"owner/repo"` form (e.g. `"spinyfin/mono"`).
///
/// Returns failing checks as `RequiredCheckFailure` records so the
/// `ci_remediations.failed_checks` JSON can carry the build URL, job id,
/// and provider — the same data the CI-fix revision directive shows the
/// worker for per-branch failures. Best-effort: any network or parse error
/// on either endpoint contributes nothing from that surface; an empty
/// combined result is still a valid return so the caller can fall back to
/// a generic directive.
pub async fn fetch_failing_checks_for_commit(owner_repo: &str, commit_sha: &str) -> Vec<RequiredCheckFailure> {
    let mut failures = fetch_failing_check_runs(owner_repo, commit_sha).await;
    let status_failures = fetch_failing_commit_statuses(owner_repo, commit_sha).await;
    // Prefer check-run rows when both surfaces name the same check
    // (dedup by name, first wins). Check runs typically carry richer
    // job-id fragments; status contexts are the Buildkite-only fallback.
    for failure in status_failures {
        if !failures.iter().any(|f| f.name == failure.name) {
            failures.push(failure);
        }
    }
    failures
}

async fn fetch_failing_check_runs(owner_repo: &str, commit_sha: &str) -> Vec<RequiredCheckFailure> {
    let api_path = format!("repos/{owner_repo}/commits/{commit_sha}/check-runs");
    let output = gh_output(&["api", &api_path]).await;
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
            tracing::debug!(?err, commit_sha, "github: failed to spawn gh for check-runs",);
            return Vec::new();
        }
    };
    parse_check_runs_for_failures(&output.stdout)
}

async fn fetch_failing_commit_statuses(owner_repo: &str, commit_sha: &str) -> Vec<RequiredCheckFailure> {
    let api_path = format!("repos/{owner_repo}/commits/{commit_sha}/status");
    let output = gh_output(&["api", &api_path]).await;
    let output = match output {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::debug!(
                commit_sha,
                stderr = %String::from_utf8_lossy(&o.stderr),
                "github: gh api commit status failed for merge-queue commit",
            );
            return Vec::new();
        }
        Err(err) => {
            tracing::debug!(?err, commit_sha, "github: failed to spawn gh for commit status",);
            return Vec::new();
        }
    };
    parse_commit_statuses_for_failures(&output.stdout)
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
        // `cancelled` is deliberately excluded here (unlike
        // `merge_poller::is_failure_conclusion`, which does treat it as a
        // failure): the merge queue cancels sibling checks on dequeue, so
        // counting a cancellation as a "required check failure" on this
        // rebounce-detection path would misreport queue churn as a real CI
        // failure. `stale` is excluded for the same reason — GitHub marks a
        // check `stale` when a newer commit supersedes it mid-run.
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

/// Pure parser for the GitHub REST `/commits/{sha}/status` (combined
/// status) response body. Returns `RequiredCheckFailure` records for
/// every status context whose `state` is a terminal failure per
/// [`classify_status_context_state`].
///
/// This is the surface Buildkite posts on mono — check-runs are empty
/// for merge-queue synthetic commits, so without this parser every
/// mono queue ejection looks like "no failing checks". Field names
/// match the REST combined-status schema (`context`, `state`,
/// `target_url`); the GraphQL rollup path uses camelCase
/// (`targetUrl`) but the same state vocabulary.
pub fn parse_commit_statuses_for_failures(body: &[u8]) -> Vec<RequiredCheckFailure> {
    let body: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let statuses = match body["statuses"].as_array() {
        Some(arr) => arr,
        None => return Vec::new(),
    };
    let mut failures = Vec::new();
    for status in statuses {
        let state = status["state"].as_str().unwrap_or("");
        let conclusion = match classify_status_context_state(state) {
            StatusContextVerdict::Fail { conclusion } => conclusion,
            StatusContextVerdict::Pass | StatusContextVerdict::InFlight => continue,
        };
        let name = status["context"].as_str().unwrap_or_default().to_owned();
        if name.is_empty() {
            continue;
        }
        let target_url = status["target_url"].as_str().unwrap_or_default().to_owned();
        let provider = provider_for_url(&target_url);
        let provider_job_id = parse_provider_job_id(provider, &target_url);
        failures.push(RequiredCheckFailure {
            name,
            conclusion,
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
        assert_eq!(failures[0].target_url, "https://github.com/org/repo/runs/42");
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
            ("https://buildkite.com/acme/mono/builds/42#01h-job-uuid", Buildkite),
            // GitHub Actions — github.com host PLUS an /actions/ or
            // /check-runs/ segment.
            (
                "https://github.com/anthropic/mono/actions/runs/123/job/456",
                GithubActions,
            ),
            ("https://github.com/anthropic/mono/check-runs/789", GithubActions),
            // Bare github.com without either segment → Other (e.g. a PR
            // or status URL we can't read logs from).
            ("https://github.com/anthropic/mono/pull/7", Other),
            // Empty string and unrelated third-party hosts → Other.
            ("", Other),
            ("https://app.codecov.io/gh/anthropic/mono", Other),
            ("https://sonarcloud.io/dashboard?id=mono", Other),
            // Case-insensitivity: an upper/mixed-case host still matches.
            ("HTTPS://BuildKite.COM/Acme/Mono/Builds/42", Buildkite),
            ("https://GITHUB.com/anthropic/mono/ACTIONS/runs/1/job/2", GithubActions),
        ];
        for (url, expected) in cases {
            assert_eq!(super::provider_for_url(url), *expected, "provider_for_url({url:?})",);
        }
    }

    /// Status-context state classification is the single spelling of
    /// "did this legacy commit status fail" shared with
    /// `merge_poller::normalize_leaf`. FAILURE and ERROR terminal-fail;
    /// SUCCESS passes; everything else waits.
    #[test]
    fn classify_status_context_state_matches_rollup_rules() {
        assert_eq!(classify_status_context_state("SUCCESS"), StatusContextVerdict::Pass);
        assert_eq!(classify_status_context_state("success"), StatusContextVerdict::Pass);
        assert_eq!(
            classify_status_context_state("FAILURE"),
            StatusContextVerdict::Fail {
                conclusion: "FAILURE".into()
            }
        );
        // REST combined-status returns lowercase; conclusion is uppercased
        // to match the GraphQL rollup path's spelling.
        assert_eq!(
            classify_status_context_state("failure"),
            StatusContextVerdict::Fail {
                conclusion: "FAILURE".into()
            }
        );
        assert_eq!(
            classify_status_context_state("error"),
            StatusContextVerdict::Fail {
                conclusion: "ERROR".into()
            }
        );
        assert_eq!(classify_status_context_state("PENDING"), StatusContextVerdict::InFlight);
        assert_eq!(
            classify_status_context_state("EXPECTED"),
            StatusContextVerdict::InFlight
        );
        assert_eq!(classify_status_context_state(""), StatusContextVerdict::InFlight);
    }

    /// Regression (mono merge-queue ejections): Buildkite posts legacy
    /// commit statuses, not check runs. A queue-ejected commit whose CI
    /// is reported only via `/commits/{sha}/status` must still surface
    /// as failing checks — otherwise the rebounce detector refuses to
    /// flip the row (empty `failures` guard).
    #[test]
    fn parse_commit_statuses_for_failures_returns_failing_contexts() {
        let body = br#"{
            "state": "failure",
            "total_count": 3,
            "statuses": [
                {
                    "context": "buildkite/mono/checks",
                    "state": "failure",
                    "target_url": "https://buildkite.com/spinyfin/mono/builds/42#job-abc"
                },
                {
                    "context": "buildkite/mono/bazel-build-test",
                    "state": "failure",
                    "target_url": "https://buildkite.com/spinyfin/mono/builds/42#job-def"
                },
                {
                    "context": "buildkite/mono/lint",
                    "state": "success",
                    "target_url": "https://buildkite.com/spinyfin/mono/builds/42#job-ghi"
                },
                {
                    "context": "codecov/project",
                    "state": "pending",
                    "target_url": "https://codecov.io/gh/spinyfin/mono"
                }
            ]
        }"#;
        let failures = parse_commit_statuses_for_failures(body);
        assert_eq!(failures.len(), 2, "only terminal-fail statuses");
        assert_eq!(failures[0].name, "buildkite/mono/checks");
        assert_eq!(failures[0].conclusion, "FAILURE");
        assert_eq!(
            failures[0].target_url,
            "https://buildkite.com/spinyfin/mono/builds/42#job-abc"
        );
        assert_eq!(failures[0].provider, CiProvider::Buildkite);
        assert_eq!(failures[0].provider_job_id.as_deref(), Some("job-abc"));
        assert_eq!(failures[1].name, "buildkite/mono/bazel-build-test");
        assert_eq!(failures[1].conclusion, "FAILURE");
    }

    #[test]
    fn parse_commit_statuses_for_failures_treats_error_as_failure() {
        let body = br#"{
            "state": "failure",
            "statuses": [
                {
                    "context": "ci/crash",
                    "state": "error",
                    "target_url": "https://buildkite.com/org/p/builds/1"
                }
            ]
        }"#;
        let failures = parse_commit_statuses_for_failures(body);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].conclusion, "ERROR");
    }

    #[test]
    fn parse_commit_statuses_for_failures_empty_on_malformed_or_green() {
        assert!(parse_commit_statuses_for_failures(b"not json").is_empty());
        assert!(parse_commit_statuses_for_failures(b"{}").is_empty());
        let green = br#"{
            "state": "success",
            "statuses": [
                {"context": "ci", "state": "success", "target_url": ""}
            ]
        }"#;
        assert!(parse_commit_statuses_for_failures(green).is_empty());
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
            super::parse_provider_job_id(Buildkite, "https://buildkite.com/acme/mono/builds/123#job-uuid",),
            Some("job-uuid".to_owned()),
        );
        // Buildkite with no fragment → None.
        assert_eq!(
            super::parse_provider_job_id(Buildkite, "https://buildkite.com/acme/mono/builds/123"),
            None,
        );
        // Buildkite with an empty fragment (trailing '#' and nothing after)
        // → None, not `Some("")`. A `Some("")` job id would satisfy
        // `provider_job_id.is_some()` in `ci_watch::fetch_and_store_log_excerpt`
        // and get picked over a sibling check with a real job id.
        assert_eq!(
            super::parse_provider_job_id(Buildkite, "https://buildkite.com/acme/mono/builds/42#"),
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
            super::parse_provider_job_id(GithubActions, "https://github.com/anthropic/mono/actions/runs/12345",),
            None,
        );
        // GitHub Actions: a '#step:' fragment (routinely present on job
        // URLs) is stripped, not appended to the id.
        assert_eq!(
            super::parse_provider_job_id(
                GithubActions,
                "https://github.com/anthropic/mono/actions/runs/12345/job/67890#step:5:1",
            ),
            Some("67890".to_owned()),
        );
        // GitHub Actions: an empty id after '/job/' → None.
        assert_eq!(
            super::parse_provider_job_id(
                GithubActions,
                "https://github.com/anthropic/mono/actions/runs/12345/job/",
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
