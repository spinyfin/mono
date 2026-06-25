//! GitHub Check Runs API annotation backend.
//!
//! Posts checkleft findings as a GitHub *check run* — a CI-agnostic REST surface
//! that works identically on GitHub Actions and Buildkite (or any other CI, or
//! none) given a token with `Checks: write`. The check run shows up in the PR's
//! Checks tab, and its annotations render inline on the PR diff.
//!
//! GitHub accepts at most 50 annotations per request, so a run with N findings is
//! one `POST /repos/{owner}/{repo}/check-runs` (creating the run with the first
//! batch) followed by `ceil(N/50) - 1` `PATCH`es that append the rest. For 230
//! findings that is 1 POST + 4 PATCHes.
//!
//! Failure handling is the *caller's* policy: every fallible step returns a
//! `Result`, and this module performs no logging and makes no process-exit
//! decision. The caller decides whether a posting failure is a non-fatal warning
//! (the default) or fatal (`--annotations-strict`).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::annotate::{Annotation, AnnotationLevel, annotation_from_finding};
use crate::output::{CheckResult, Severity};

/// Maximum annotations GitHub accepts in a single check-run create/update request.
pub const MAX_ANNOTATIONS_PER_REQUEST: usize = 50;

/// Default base URL for the GitHub REST API. Overridable (e.g. for GitHub
/// Enterprise, or a mock server in tests) via the `base_url` parameter of
/// [`post_check_run`].
pub const GITHUB_API_BASE_URL: &str = "https://api.github.com";

/// The `name` of the check run as it appears in the GitHub Checks tab.
pub const CHECK_RUN_NAME: &str = "checkleft";

/// GitHub's per-annotation `title` ceiling (characters).
const MAX_ANNOTATION_TITLE_CHARS: usize = 255;

/// GitHub's per-annotation `message` ceiling (bytes). Findings are short in
/// practice; this guards a pathological message from 422-ing an entire batch.
const MAX_ANNOTATION_MESSAGE_BYTES: usize = 64 * 1024;

/// The Check Runs API `annotation_level` string for an [`AnnotationLevel`].
fn annotation_level_str(level: AnnotationLevel) -> &'static str {
    match level {
        AnnotationLevel::Failure => "failure",
        AnnotationLevel::Warning => "warning",
        AnnotationLevel::Notice => "notice",
    }
}

/// The check run `conclusion`, derived from the most severe finding.
///
/// Mirrors checkleft's own exit semantics: any `Error` finding fails the run; a
/// run with only warnings/notices is `neutral` (advisory — it does not mark the
/// PR's check red); a run with no findings is `success`.
pub fn conclusion_for(results: &[CheckResult]) -> &'static str {
    let mut any_finding = false;
    for result in results {
        for finding in &result.findings {
            if finding.severity == Severity::Error {
                return "failure";
            }
            any_finding = true;
        }
    }
    if any_finding { "neutral" } else { "success" }
}

/// Finding totals by severity, used to build the check-run output summary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FindingCounts {
    pub errors: usize,
    pub warnings: usize,
    pub notices: usize,
}

impl FindingCounts {
    pub fn total(self) -> usize {
        self.errors + self.warnings + self.notices
    }
}

/// Tally findings across every check result by severity.
pub fn count_findings(results: &[CheckResult]) -> FindingCounts {
    let mut counts = FindingCounts::default();
    for result in results {
        for finding in &result.findings {
            match finding.severity {
                Severity::Error => counts.errors += 1,
                Severity::Warning => counts.warnings += 1,
                Severity::Info => counts.notices += 1,
            }
        }
    }
    counts
}

/// Short one-line title for the check-run output (e.g. `"12 findings"`).
pub fn output_title(counts: FindingCounts) -> String {
    match counts.total() {
        0 => "No findings".to_owned(),
        1 => "1 finding".to_owned(),
        n => format!("{n} findings"),
    }
}

/// Human-readable summary body, broken down by severity.
pub fn output_summary(counts: FindingCounts) -> String {
    if counts.total() == 0 {
        return "checkleft found no findings.".to_owned();
    }
    format!(
        "checkleft found {total} {findings}: {e} {errors}, {w} {warnings}, {n} {notices}.",
        total = counts.total(),
        findings = if counts.total() == 1 { "finding" } else { "findings" },
        e = counts.errors,
        errors = if counts.errors == 1 { "error" } else { "errors" },
        w = counts.warnings,
        warnings = if counts.warnings == 1 { "warning" } else { "warnings" },
        n = counts.notices,
        notices = if counts.notices == 1 { "notice" } else { "notices" },
    )
}

/// One annotation in a check-run `output.annotations` array.
///
/// Field names follow the GitHub Check Runs API. `start_column`/`end_column` are
/// only permitted when `start_line == end_line`, which always holds here (the
/// shared core never emits multi-line ranges), so they are forwarded as-is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckRunAnnotation {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_column: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
    pub annotation_level: &'static str,
    pub title: String,
    pub message: String,
}

impl CheckRunAnnotation {
    pub fn from_annotation(a: &Annotation) -> Self {
        // GitHub rejects columns unless the annotation is single-line; the shared
        // core guarantees that, but assert it defensively before forwarding them.
        let single_line = a.start_line == a.end_line;
        CheckRunAnnotation {
            path: a.path.clone(),
            start_line: a.start_line,
            end_line: a.end_line,
            start_column: single_line.then_some(a.start_column).flatten(),
            end_column: single_line.then_some(a.end_column).flatten(),
            annotation_level: annotation_level_str(a.level),
            title: truncate_chars(&a.title, MAX_ANNOTATION_TITLE_CHARS),
            message: truncate_bytes(&a.message, MAX_ANNOTATION_MESSAGE_BYTES),
        }
    }
}

/// Project every finding across all check results into wire-ready annotations,
/// in result/finding order. Findings without a file location are dropped —
/// `annotation_from_finding` encodes that rule (GitHub requires a path on every
/// annotation).
pub fn collect_annotations(results: &[CheckResult]) -> Vec<CheckRunAnnotation> {
    results
        .iter()
        .flat_map(|result| {
            result
                .findings
                .iter()
                .filter_map(|finding| annotation_from_finding(&result.check_id, finding))
        })
        .map(|a| CheckRunAnnotation::from_annotation(&a))
        .collect()
}

#[derive(Debug, Serialize)]
struct CheckRunOutput {
    title: String,
    summary: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    annotations: Vec<CheckRunAnnotation>,
}

#[derive(Debug, Serialize)]
struct CreateCheckRunBody<'a> {
    name: &'a str,
    head_sha: &'a str,
    status: &'a str,
    conclusion: &'a str,
    output: CheckRunOutput,
}

#[derive(Debug, Serialize)]
struct UpdateCheckRunBody {
    output: CheckRunOutput,
}

#[derive(Debug, Deserialize)]
struct CheckRunResponse {
    id: u64,
}

/// Post checkleft findings to GitHub as a check run.
///
/// Creates the run (`POST .../check-runs`) with the first batch of ≤50
/// annotations, then appends each remaining batch with `PATCH .../check-runs/{id}`.
/// Returns the created check run's id on success.
///
/// Every failure (non-2xx response, transport error, malformed body) is surfaced
/// as an `Err`; this function neither logs nor decides the process exit code — the
/// caller applies the non-fatal/strict policy.
///
/// `base_url` is the GitHub REST root (`https://api.github.com`, or a GitHub
/// Enterprise / mock-server URL); the `reqwest` client is built internally after
/// the rustls provider is installed, since the client cannot be constructed
/// before then.
pub async fn post_check_run(
    base_url: &str,
    owner_repo: &str,
    token: &str,
    head_sha: &str,
    results: &[CheckResult],
) -> Result<u64> {
    crate::vcs::ensure_rustls_provider();
    let client = reqwest::Client::new();

    let counts = count_findings(results);
    let title = output_title(counts);
    let summary = output_summary(counts);
    let conclusion = conclusion_for(results);
    let annotations = collect_annotations(results);

    // The create request carries the first batch (or none when there are no
    // findings); always issue exactly one create, then PATCH the remainder.
    let mut batches = annotations.chunks(MAX_ANNOTATIONS_PER_REQUEST);
    let first = batches.next().unwrap_or(&[]);

    let create_url = format!("{}/repos/{}/check-runs", base_url.trim_end_matches('/'), owner_repo);
    let create_body = CreateCheckRunBody {
        name: CHECK_RUN_NAME,
        head_sha,
        status: "completed",
        conclusion,
        output: CheckRunOutput {
            title: title.clone(),
            summary: summary.clone(),
            annotations: first.to_vec(),
        },
    };
    let bytes = send_json(client.post(&create_url), token, &create_body).await?;
    let created: CheckRunResponse =
        serde_json::from_slice(&bytes).context("parsing GitHub check-run create response")?;
    let id = created.id;
    info!(
        check_run_id = id,
        owner_repo,
        conclusion,
        annotations = annotations.len(),
        "created checkleft check run"
    );

    let update_url = format!("{create_url}/{id}");
    for (batch_index, batch) in batches.enumerate() {
        let update_body = UpdateCheckRunBody {
            output: CheckRunOutput {
                title: title.clone(),
                summary: summary.clone(),
                annotations: batch.to_vec(),
            },
        };
        send_json(client.patch(&update_url), token, &update_body).await?;
        info!(
            check_run_id = id,
            // batch 0 is the create above; PATCH batches start at 1.
            batch = batch_index + 1,
            count = batch.len(),
            "appended check-run annotation batch"
        );
    }

    Ok(id)
}

/// Serialize `body` as JSON, attach the GitHub headers + bearer auth, send, and
/// return the response bytes — turning any non-2xx status into an `Err` that
/// carries a truncated response body to aid debugging.
async fn send_json<B: Serialize>(request: reqwest::RequestBuilder, token: &str, body: &B) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(body).context("serializing check-run request body")?;
    let response = request
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "checkleft-cli")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .bearer_auth(token)
        .body(payload)
        .send()
        .await
        .context("sending check-run request to GitHub")?;

    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .context("reading GitHub check-run response body")?;
    if !status.is_success() {
        let snippet = truncate_chars(String::from_utf8_lossy(&bytes).trim(), 500);
        bail!("GitHub check-run API returned HTTP {status}: {snippet}");
    }
    Ok(bytes.to_vec())
}

/// Truncate `s` to at most `max_chars` characters, appending `…` when it had to
/// cut.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

/// Truncate `s` to at most `max_bytes` bytes, cutting on a UTF-8 char boundary
/// and appending `…` when it had to cut.
fn truncate_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    const ELLIPSIS: &str = "…";
    let mut end = max_bytes.saturating_sub(ELLIPSIS.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &s[..end])
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::Value;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::output::{Finding, Location, Severity};

    fn finding(severity: Severity, message: &str, file: &str, line: Option<u32>, column: Option<u32>) -> Finding {
        Finding {
            severity,
            message: message.to_owned(),
            location: Some(Location {
                path: PathBuf::from(file),
                line,
                column,
            }),
            remediations: vec![],
            suggested_fix: None,
        }
    }

    fn finding_no_location(severity: Severity, message: &str) -> Finding {
        Finding {
            severity,
            message: message.to_owned(),
            location: None,
            remediations: vec![],
            suggested_fix: None,
        }
    }

    fn result(check_id: &str, findings: Vec<Finding>) -> CheckResult {
        CheckResult {
            check_id: check_id.to_owned(),
            findings,
        }
    }

    // ── pure mapping ──────────────────────────────────────────────────────────

    #[test]
    fn conclusion_is_failure_when_any_error() {
        let results = vec![result(
            "lint/rust",
            vec![
                finding(Severity::Warning, "w", "a.rs", Some(1), None),
                finding(Severity::Error, "e", "b.rs", Some(2), None),
            ],
        )];
        assert_eq!(conclusion_for(&results), "failure");
    }

    #[test]
    fn conclusion_is_neutral_with_only_warnings_and_notices() {
        let results = vec![result(
            "fmt/rust",
            vec![
                finding(Severity::Warning, "w", "a.rs", Some(1), None),
                finding(Severity::Info, "i", "b.rs", Some(2), None),
            ],
        )];
        assert_eq!(conclusion_for(&results), "neutral");
    }

    #[test]
    fn conclusion_is_success_when_no_findings() {
        let results = vec![result("fmt/rust", vec![])];
        assert_eq!(conclusion_for(&results), "success");
        assert_eq!(conclusion_for(&[]), "success");
    }

    #[test]
    fn count_findings_tallies_by_severity() {
        let results = vec![
            result(
                "a",
                vec![
                    finding(Severity::Error, "e", "a.rs", Some(1), None),
                    finding(Severity::Warning, "w", "a.rs", Some(2), None),
                ],
            ),
            result("b", vec![finding(Severity::Info, "i", "b.rs", Some(1), None)]),
        ];
        let counts = count_findings(&results);
        assert_eq!(
            counts,
            FindingCounts {
                errors: 1,
                warnings: 1,
                notices: 1
            }
        );
        assert_eq!(counts.total(), 3);
    }

    #[test]
    fn title_and_summary_render_counts() {
        assert_eq!(output_title(FindingCounts::default()), "No findings");
        assert_eq!(
            output_title(FindingCounts {
                errors: 1,
                ..Default::default()
            }),
            "1 finding"
        );
        assert_eq!(
            output_title(FindingCounts {
                errors: 2,
                warnings: 3,
                notices: 0
            }),
            "5 findings"
        );
        assert_eq!(output_summary(FindingCounts::default()), "checkleft found no findings.");
        assert_eq!(
            output_summary(FindingCounts {
                errors: 1,
                warnings: 2,
                notices: 0
            }),
            "checkleft found 3 findings: 1 error, 2 warnings, 0 notices."
        );
    }

    #[test]
    fn collect_annotations_maps_and_drops_locationless() {
        let results = vec![result(
            "lint/rust",
            vec![
                finding(Severity::Error, "bad", "src/lib.rs", Some(42), Some(7)),
                finding_no_location(Severity::Error, "no location"),
            ],
        )];
        let annotations = collect_annotations(&results);
        assert_eq!(annotations.len(), 1);
        let a = &annotations[0];
        assert_eq!(a.path, "src/lib.rs");
        assert_eq!(a.start_line, 42);
        assert_eq!(a.end_line, 42);
        assert_eq!(a.start_column, Some(7));
        assert_eq!(a.end_column, None);
        assert_eq!(a.annotation_level, "failure");
        assert_eq!(a.title, "lint/rust");
        assert_eq!(a.message, "bad");
    }

    #[test]
    fn annotation_serializes_with_expected_fields() {
        let a = CheckRunAnnotation::from_annotation(&Annotation {
            path: "a.rs".to_owned(),
            start_line: 3,
            end_line: 3,
            start_column: None,
            end_column: None,
            level: AnnotationLevel::Warning,
            title: "fmt/rust".to_owned(),
            message: "msg".to_owned(),
            rule_id: "fmt/rust".to_owned(),
        });
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["path"], "a.rs");
        assert_eq!(v["start_line"], 3);
        assert_eq!(v["end_line"], 3);
        assert_eq!(v["annotation_level"], "warning");
        assert_eq!(v["title"], "fmt/rust");
        assert_eq!(v["message"], "msg");
        // Absent columns must be omitted, not serialized as null.
        assert!(v.get("start_column").is_none());
        assert!(v.get("end_column").is_none());
    }

    #[test]
    fn truncate_chars_appends_ellipsis_when_over_limit() {
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        // Counts characters, not bytes.
        assert_eq!(truncate_chars("ééééé", 5), "ééééé");
    }

    #[test]
    fn truncate_bytes_cuts_on_char_boundary() {
        assert_eq!(truncate_bytes("abc", 10), "abc");
        // "aaaébbbb" is 9 bytes ("é" is two); a 7-byte budget lands the cut at
        // byte 4 — mid-"é" — so it must step back to the boundary at byte 3.
        let out = truncate_bytes("aaaébbbb", 7);
        assert_eq!(out, "aaa…");
        assert!(out.len() <= 7);
        assert!(out.ends_with('…'));
    }

    // ── HTTP: create + batched PATCH append ──────────────────────────────────

    fn many_findings(n: usize) -> Vec<CheckResult> {
        let findings = (0..n)
            .map(|i| finding(Severity::Warning, "w", "a.rs", Some(i as u32 + 1), None))
            .collect();
        vec![result("lint/rust", findings)]
    }

    #[tokio::test]
    async fn create_only_for_small_batch() {
        let server = MockServer::start().await;
        // The matcher also asserts the bearer token + JSON content-type reach GitHub.
        Mock::given(method("POST"))
            .and(path("/repos/o/r/check-runs"))
            .and(header("authorization", "Bearer tok"))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 555 })))
            .expect(1)
            .mount(&server)
            .await;

        let results = many_findings(10);
        let id = post_check_run(&server.uri(), "o/r", "tok", "deadbeef", &results)
            .await
            .expect("post succeeds");
        assert_eq!(id, 555);

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "no PATCH expected for <=50 findings");
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["name"], "checkleft");
        assert_eq!(body["head_sha"], "deadbeef");
        assert_eq!(body["status"], "completed");
        assert_eq!(body["conclusion"], "neutral");
        assert_eq!(body["output"]["annotations"].as_array().unwrap().len(), 10);
    }

    #[tokio::test]
    async fn batches_in_chunks_of_fifty() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/check-runs"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 7 })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/repos/o/r/check-runs/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": 7 })))
            .expect(2)
            .mount(&server)
            .await;

        // 120 findings => 50 (POST) + 50 (PATCH) + 20 (PATCH).
        let results = many_findings(120);
        let id = post_check_run(&server.uri(), "o/r", "tok", "sha", &results)
            .await
            .expect("post succeeds");
        assert_eq!(id, 7);

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        let lens: Vec<usize> = requests
            .iter()
            .map(|r| {
                let body: Value = serde_json::from_slice(&r.body).unwrap();
                body["output"]["annotations"].as_array().unwrap().len()
            })
            .collect();
        assert_eq!(lens, vec![50, 50, 20]);
        // PATCH bodies re-send title + summary (GitHub requires them on output).
        let patch_body: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(patch_body["output"]["title"], "120 findings");
        assert!(patch_body["output"]["summary"].is_string());
    }

    #[tokio::test]
    async fn create_with_no_findings_posts_green_run() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/check-runs"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
            .expect(1)
            .mount(&server)
            .await;

        let results = vec![result("fmt/rust", vec![])];
        post_check_run(&server.uri(), "o/r", "tok", "sha", &results)
            .await
            .expect("post succeeds");

        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["conclusion"], "success");
        assert_eq!(body["output"]["title"], "No findings");
        assert!(body["output"].get("annotations").is_none());
    }

    #[tokio::test]
    async fn non_2xx_response_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/check-runs"))
            .respond_with(ResponseTemplate::new(422).set_body_string("{\"message\":\"Validation Failed\"}"))
            .mount(&server)
            .await;

        let results = many_findings(3);
        let err = post_check_run(&server.uri(), "o/r", "tok", "sha", &results)
            .await
            .expect_err("non-2xx must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("422"), "error should include status: {msg}");
        assert!(msg.contains("Validation Failed"), "error should include body: {msg}");
    }
}
