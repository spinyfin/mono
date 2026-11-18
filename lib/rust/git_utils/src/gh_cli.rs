//! Helpers that invoke the `gh` CLI to fetch GitHub PR head metadata.
//!
//! These functions shell out to the `gh` binary rather than using the
//! GitHub REST API directly. They are suitable for contexts where a
//! short-lived `gh`-authenticated call is simpler than a full App-JWT
//! flow — in particular, fetching PR head SHAs / branch names from the
//! engine without requiring embedded App credentials.

use std::process::Stdio;

use anyhow::{Result, anyhow};
use tokio::process::Command;

/// Spawn a `gh` subprocess with the standard stdio / kill-on-drop
/// settings, returning the trimmed stdout on success. `display` is a
/// human-readable rendering of the command, reused in both the
/// spawn-failure context and the non-zero-exit error message (which also
/// carries the captured stderr).
async fn run_gh(args: &[&str], display: &str) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|err| {
            GhError::new(
                classify_spawn_error(&err),
                format!("failed to spawn `{display}`: {err}"),
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(error_from_stderr(display, stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Whether repeating a failed `gh` call can succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhErrorKind {
    /// Connectivity, timeout, server-side or rate-limit failure: a later
    /// attempt can succeed.
    Transient,
    /// A missing PR/repository, rejected credentials, an unusable `gh`
    /// binary or a malformed answer: every attempt fails the same way.
    Deterministic,
}

/// A failed `gh` invocation, carrying whether retrying it is worthwhile.
/// Downcast from the returned [`anyhow::Error`] with [`is_deterministic`].
#[derive(Debug)]
pub struct GhError {
    kind: GhErrorKind,
    message: String,
}

impl GhError {
    pub fn new(kind: GhErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> GhErrorKind {
        self.kind
    }
}

impl std::fmt::Display for GhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GhError {}

/// True when `error` is a [`GhError`] that retrying cannot fix.
pub fn is_deterministic(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<GhError>()
        .is_some_and(|err| err.kind == GhErrorKind::Deterministic)
}

/// Typed error for a failed `gh` invocation, classified from `stderr`.
pub fn error_from_stderr(display: &str, stderr: &str) -> anyhow::Error {
    GhError::new(classify_stderr(stderr), format!("`{display}` failed: {stderr}")).into()
}

/// Retry policy for `gh` stderr: a missing resource or an authorization
/// failure repeats identically, so it is deterministic. A rate limit, and
/// anything unfamiliar, keeps the retry behaviour.
fn classify_stderr(stderr: &str) -> GhErrorKind {
    match classify_failure(stderr) {
        GhFailure::NotAuthorized | GhFailure::NotFound => GhErrorKind::Deterministic,
        GhFailure::RateLimited | GhFailure::Other => GhErrorKind::Transient,
    }
}

/// A spawn failure is deterministic only when `gh` is missing or not
/// executable. Resource exhaustion (`EAGAIN`, `EMFILE`, `ETXTBSY`) clears up.
fn classify_spawn_error(err: &std::io::Error) -> GhErrorKind {
    match err.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => GhErrorKind::Deterministic,
        _ => GhErrorKind::Transient,
    }
}

// ── Failure classification shared with `boss-github` ─────────────────────────
//
// HTTP status, authorization, missing-resource and rate-limit recognition lives
// here once; callers map it onto their own retry policy.

/// What a failed `gh` invocation says about its cause, independent of any
/// caller's retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhFailure {
    /// Primary or secondary rate limit (including the HTTP 403 it can arrive
    /// as). Waiting is the remedy.
    RateLimited,
    /// Rejected or missing credentials, or an authorization refusal
    /// (HTTP 401 / 403 that is not a rate limit).
    NotAuthorized,
    /// The PR, repository or other resource does not exist (HTTP 404).
    NotFound,
    /// Anything else: connectivity, timeouts, server errors, unfamiliar text.
    Other,
}

/// Scan `gh`'s stderr for an HTTP status code pattern like "(HTTP 404)" or "HTTP 404".
pub fn parse_http_status(stderr: &str) -> Option<u16> {
    let lower = stderr.to_lowercase();
    let pos = lower.find("http ")?;
    // `to_lowercase` can change byte lengths for non-ASCII text, so index the
    // lowered string itself; status digits are ASCII either way.
    let digits: String = lower[pos + 5..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u16>().ok()
}

/// Classify `gh` stderr. Matching is case-insensitive. A rate limit is checked
/// first because it is delivered as an HTTP 403 and must not read as an
/// authorization failure.
pub fn classify_failure(stderr: &str) -> GhFailure {
    let lower = stderr.to_lowercase();
    if lower.contains("rate limit") || lower.contains("secondary rate") || lower.contains("http 429") {
        return GhFailure::RateLimited;
    }
    const NOT_AUTHORIZED: [&str; 6] = [
        "http 401",
        "http 403",
        "bad credentials",
        "gh auth login",
        "authentication token",
        "not logged in",
    ];
    if NOT_AUTHORIZED.iter().any(|needle| lower.contains(needle)) {
        return GhFailure::NotAuthorized;
    }
    const NOT_FOUND: [&str; 3] = [
        "could not resolve to a pullrequest",
        "could not resolve to a repository",
        "not found",
    ];
    if parse_http_status(stderr) == Some(404) || NOT_FOUND.iter().any(|needle| lower.contains(needle)) {
        return GhFailure::NotFound;
    }
    GhFailure::Other
}

/// Fetch the head commit SHA (`headRefOid`) for a PR by shelling out to
/// `gh pr view <pr_number> -R <repo_slug> --json headRefOid --jq .headRefOid`.
///
/// Returns an error if the command fails or if the returned SHA is empty.
pub async fn fetch_pr_head_oid(repo_slug: &str, pr_number: u64) -> Result<String> {
    let pr_str = pr_number.to_string();
    let sha = run_gh(
        &[
            "pr",
            "view",
            &pr_str,
            "-R",
            repo_slug,
            "--json",
            "headRefOid",
            "--jq",
            ".headRefOid",
        ],
        &format!("gh pr view {pr_number} -R {repo_slug} --json headRefOid"),
    )
    .await?;
    parse_head_sha_output(sha, pr_number, repo_slug)
}

/// Fetch a PR's head commit SHA via GitHub's REST `pulls` endpoint
/// (`repos/{repo}/pulls/{n}` → `.head.sha`), bypassing the GraphQL path
/// `gh pr view` uses.
///
/// When `no_cache` is true, sends `Cache-Control: no-cache` so an
/// intermediary revalidates rather than reusing a cached representation —
/// required at the PR-completion teardown seam so a stale merge-poller
/// snapshot cannot be recorded as the forensic post-teardown head.
/// When false, issues a plain GET (callers that only need a best-effort
/// tripwire snapshot, e.g. merge-parent deletion).
///
/// Returns an error when the command fails or the response omits the head
/// SHA, matching [`fetch_pr_head_oid`]'s fail-closed contract. Callers that
/// prefer fail-open map the error to `None`.
pub async fn fetch_pr_head_sha_rest(repo_slug: &str, pr_number: u64, no_cache: bool) -> Result<String> {
    let endpoint = format!("repos/{repo_slug}/pulls/{pr_number}");
    let sha = if no_cache {
        run_gh(
            &["api", "-H", "Cache-Control: no-cache", &endpoint, "--jq", ".head.sha"],
            &format!("gh api -H Cache-Control:no-cache {endpoint} --jq .head.sha"),
        )
        .await?
    } else {
        run_gh(
            &["api", &endpoint, "--jq", ".head.sha"],
            &format!("gh api {endpoint} --jq .head.sha"),
        )
        .await?
    };
    parse_head_sha_output(sha, pr_number, repo_slug)
}

/// Teardown-time REST head read with `Cache-Control: no-cache`. See
/// [`fetch_pr_head_sha_rest`].
pub async fn fetch_pr_head_oid_fresh(repo_slug: &str, pr_number: u64) -> Result<String> {
    fetch_pr_head_sha_rest(repo_slug, pr_number, true).await
}

/// Fetch the head branch name (`headRefName`) for a PR by shelling out to
/// `gh pr view <pr_number> -R <repo_slug> --json headRefName --jq .headRefName`.
///
/// Returns an error if the command fails or if the returned ref is empty.
pub async fn fetch_pr_head_ref(repo_slug: &str, pr_number: u64) -> Result<String> {
    let pr_str = pr_number.to_string();
    let head_ref = run_gh(
        &[
            "pr",
            "view",
            &pr_str,
            "-R",
            repo_slug,
            "--json",
            "headRefName",
            "--jq",
            ".headRefName",
        ],
        &format!("gh pr view {pr_number} -R {repo_slug}"),
    )
    .await?;
    if head_ref.is_empty() {
        return Err(anyhow!("empty headRefName for PR {pr_number} in {repo_slug}"));
    }
    Ok(head_ref)
}

/// Fetch the target-branch ref name (`baseRefName`) for a PR by shelling
/// out to `gh pr view <pr_number> -R <repo_slug> --json baseRefName --jq
/// .baseRefName`.
///
/// Returns an error if the command fails or if the returned ref is empty.
pub async fn fetch_pr_base_ref(repo_slug: &str, pr_number: u64) -> Result<String> {
    let pr_str = pr_number.to_string();
    let base_ref = run_gh(
        &[
            "pr",
            "view",
            &pr_str,
            "-R",
            repo_slug,
            "--json",
            "baseRefName",
            "--jq",
            ".baseRefName",
        ],
        &format!("gh pr view {pr_number} -R {repo_slug} --json baseRefName"),
    )
    .await?;
    if base_ref.is_empty() {
        return Err(anyhow!("empty baseRefName for PR {pr_number} in {repo_slug}"));
    }
    Ok(base_ref)
}

/// Return the base of an open PR; closed and merged PRs cannot anchor recovery.
pub async fn fetch_open_pr_base(repo_slug: &str, pr_number: u64) -> Result<Option<String>> {
    let output = run_gh(
        &[
            "pr",
            "view",
            &pr_number.to_string(),
            "-R",
            repo_slug,
            "--json",
            "state,baseRefName",
            "--jq",
            ".state, .baseRefName",
        ],
        "gh pr view recovery anchor",
    )
    .await?;
    parse_open_pr_base(&output)
}

fn parse_open_pr_base(output: &str) -> Result<Option<String>> {
    let mut lines = output.lines();
    match lines.next() {
        Some("OPEN") => {
            let base = lines
                .next()
                .filter(|s| !s.is_empty() && *s != "null")
                .ok_or_else(|| anyhow!("open PR has no baseRefName"))?;
            Ok(Some(base.to_owned()))
        }
        Some("CLOSED" | "MERGED") => Ok(None),
        _ => Err(anyhow!("PR lookup returned an unknown state")),
    }
}

/// Validate and return the SHA string from `gh pr view ... --jq .headRefOid`
/// stdout. Returns an error when the output is empty (which means GitHub
/// returned a null or the JQ filter found nothing).
pub(crate) fn parse_head_sha_output(sha: String, pr_number: u64, repo_slug: &str) -> Result<String> {
    if sha.is_empty() {
        return Err(GhError::new(
            GhErrorKind::Deterministic,
            format!("empty headRefOid for PR {pr_number} in {repo_slug}"),
        )
        .into());
    }
    Ok(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_anchor_requires_structured_open_state() {
        assert_eq!(parse_open_pr_base("OPEN\nmain").unwrap(), Some("main".into()));
        for state in ["CLOSED", "MERGED"] {
            assert_eq!(parse_open_pr_base(&format!("{state}\nmain")).unwrap(), None);
        }
        for invalid in ["OPEN", "OPEN\nnull", "unknown\nmain"] {
            assert!(parse_open_pr_base(invalid).is_err());
        }
    }

    #[test]
    fn stderr_classification_separates_missing_pr_and_auth_from_transport() {
        for stderr in [
            "GraphQL: Could not resolve to a PullRequest with the number of 9. (repository.pullRequest)",
            "HTTP 404: Not Found (https://api.github.com/repos/o/r/pulls/9)",
            "HTTP 401: Bad credentials",
            "Resource not accessible by integration (HTTP 403)",
            "To get started with GitHub CLI, please run:  gh auth login",
        ] {
            assert_eq!(classify_stderr(stderr), GhErrorKind::Deterministic, "{stderr}");
        }
        for stderr in [
            "error connecting to api.github.com",
            "HTTP 502: Bad Gateway",
            "HTTP 403: API rate limit exceeded for user",
            "i/o timeout",
        ] {
            assert_eq!(classify_stderr(stderr), GhErrorKind::Transient, "{stderr}");
        }
    }

    #[test]
    fn spawn_failures_are_deterministic_only_when_gh_is_unusable() {
        use std::io::{Error, ErrorKind};
        for kind in [ErrorKind::NotFound, ErrorKind::PermissionDenied] {
            assert_eq!(classify_spawn_error(&Error::from(kind)), GhErrorKind::Deterministic);
        }
        for kind in [ErrorKind::WouldBlock, ErrorKind::ResourceBusy, ErrorKind::Interrupted] {
            assert_eq!(classify_spawn_error(&Error::from(kind)), GhErrorKind::Transient);
        }
    }

    #[test]
    fn is_deterministic_reads_the_typed_kind_through_context() {
        let deterministic: anyhow::Error = GhError::new(GhErrorKind::Deterministic, "missing").into();
        assert!(is_deterministic(&deterministic));
        assert!(is_deterministic(&deterministic.context("while snapshotting")));
        let transient: anyhow::Error = GhError::new(GhErrorKind::Transient, "offline").into();
        assert!(!is_deterministic(&transient));
        assert!(!is_deterministic(&anyhow!("untyped")));
        let classified = error_from_stderr("gh pr view", "Resource not accessible by integration (HTTP 403)");
        assert!(is_deterministic(&classified), "{classified:#}");
        assert!(!is_deterministic(&error_from_stderr(
            "gh pr view",
            "error connecting to api.github.com"
        )));
    }

    #[test]
    fn parse_head_sha_output_returns_sha_unchanged() {
        let sha = parse_head_sha_output("abc123deadbeef".to_owned(), 42, "spinyfin/mono").unwrap();
        assert_eq!(sha, "abc123deadbeef");
    }

    #[test]
    fn parse_head_sha_output_rejects_empty_string() {
        let err = parse_head_sha_output("".to_owned(), 99, "owner/repo").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("empty headRefOid"),
            "error should mention empty headRefOid: {msg}"
        );
        assert!(msg.contains("99"), "error should include the PR number: {msg}");
        assert!(msg.contains("owner/repo"), "error should include the repo slug: {msg}");
    }

    #[test]
    fn parse_head_sha_output_preserves_40_char_sha() {
        let full_sha = "a".repeat(40);
        let result = parse_head_sha_output(full_sha.clone(), 1, "org/repo").unwrap();
        assert_eq!(result, full_sha);
    }

    #[test]
    fn rate_limit_wins_over_the_403_it_arrives_as() {
        for stderr in [
            "gh: API rate limit exceeded (HTTP 403)",
            "You have exceeded a secondary rate limit (HTTP 403)",
            "HTTP 429: Too Many Requests",
        ] {
            assert_eq!(classify_failure(stderr), GhFailure::RateLimited, "{stderr}");
        }
    }

    #[test]
    fn non_rate_limit_403_and_credentials_are_not_authorized() {
        for stderr in [
            "Resource not accessible by integration (HTTP 403)",
            "HTTP 401: Bad credentials",
            "To get started with GitHub CLI, please run:  gh auth login",
            "The authentication token is invalid",
        ] {
            assert_eq!(classify_failure(stderr), GhFailure::NotAuthorized, "{stderr}");
        }
    }

    #[test]
    fn missing_resources_are_not_found_case_insensitively() {
        for stderr in [
            "GraphQL: Could not resolve to a PullRequest with the number of 9.",
            "gh: Not Found (HTTP 404)",
            "http 404: not found",
        ] {
            assert_eq!(classify_failure(stderr), GhFailure::NotFound, "{stderr}");
        }
    }

    #[test]
    fn connectivity_and_server_errors_are_other() {
        for stderr in [
            "error connecting to api.github.com",
            "HTTP 502: Bad Gateway",
            "i/o timeout",
            "",
        ] {
            assert_eq!(classify_failure(stderr), GhFailure::Other, "{stderr}");
        }
    }

    #[test]
    fn http_status_is_parsed_from_embedded_text() {
        assert_eq!(parse_http_status("gh: Not Found (HTTP 404)"), Some(404));
        assert_eq!(parse_http_status("gh: API rate limit exceeded (HTTP 429)"), Some(429));
        assert_eq!(parse_http_status("no status here"), None);
    }
}
