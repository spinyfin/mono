//! Generic `gh` CLI runner abstraction used by any crate that shells out
//! to the GitHub CLI (`gh`).
//!
//! [`GhRunner`] is a trait so callers can inject a fake implementation in
//! tests without spawning real processes. [`CommandGhRunner`] is the
//! production implementation.
//!
//! This module also exports the lower-level [`gh_output`] and [`run_gh`]
//! primitives used by call sites that need raw subprocess output or a
//! simple `Result<String>` with conventional error messages.

use std::process::Output;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

// ── Error / response types ────────────────────────────────────────────────────

/// Error from a `gh` invocation, carrying an optional HTTP status code for
/// classification by the caller.
#[derive(Debug)]
pub struct GhRunnerError {
    pub http_status: Option<u16>,
    pub message: String,
}

impl GhRunnerError {
    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            http_status: None,
            message: message.into(),
        }
    }

    pub fn with_status(status: u16, message: impl Into<String>) -> Self {
        Self {
            http_status: Some(status),
            message: message.into(),
        }
    }
}

/// Response from a successful `gh` REST call.
#[derive(Debug)]
pub struct GhResponse {
    pub body: Value,
}

// ── GhRunner trait ────────────────────────────────────────────────────────────

/// Abstraction over `gh` shellouts for testability.
#[async_trait]
pub trait GhRunner: Send + Sync {
    /// Run `gh api graphql -f query=<query> -F k=v ...` and return parsed JSON.
    /// When `token` is `Some`, sets `GH_TOKEN` on the process.
    async fn graphql(
        &self,
        query: &str,
        vars: &[(&str, &str)],
        token: Option<&str>,
    ) -> std::result::Result<Value, GhRunnerError>;

    /// Run `gh api <path>` (GET) and return parsed JSON body.
    /// When `token` is `Some`, sets `GH_TOKEN` on the process.
    async fn rest_get(&self, path: &str, token: Option<&str>) -> std::result::Result<GhResponse, GhRunnerError>;

    /// Run `gh api -X PATCH <path> -f k=v ...` and return parsed JSON body.
    /// When `token` is `Some`, sets `GH_TOKEN` on the process.
    async fn rest_patch(
        &self,
        path: &str,
        fields: &[(&str, &str)],
        token: Option<&str>,
    ) -> std::result::Result<GhResponse, GhRunnerError>;

    /// Run `gh api -X POST <path> --input -` with a JSON body and return parsed JSON body.
    /// When `token` is `Some`, sets `GH_TOKEN` on the process.
    async fn rest_post(
        &self,
        path: &str,
        body: &serde_json::Value,
        token: Option<&str>,
    ) -> std::result::Result<GhResponse, GhRunnerError>;
}

// ── Low-level spawn primitives ────────────────────────────────────────────────

/// Spawn a `gh` subprocess with the standard stdio / kill-on-drop envelope
/// (stdin null, stdout+stderr piped, `kill_on_drop(true)`) and return its
/// raw [`Output`].
///
/// This is the shared spawn primitive: it deliberately performs no
/// exit-code or stderr handling so each call site keeps its own tailored
/// logic on top. The returned `io::Result` is the spawn result — callers
/// apply their own context (`.with_context(...)`, `.ok()?`, `.map_err(...)`).
pub async fn gh_output(args: &[&str]) -> std::io::Result<Output> {
    Command::new("gh")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
}

/// Spawn `gh` via [`gh_output`] and return the trimmed stdout on success.
/// `display` is a human-readable rendering of the command and is reused in
/// both the spawn-failure context and the non-zero-exit error message
/// (which also carries the captured stderr).
///
/// This is the happy-path convenience for sites that want a
/// `Result<String>` with the conventional "spawn failed" / "command
/// failed: <stderr>" error shape. Sites that need different exit-code
/// handling (graceful degradation, JSON parsing) call [`gh_output`]
/// directly.
pub async fn run_gh(args: &[&str], display: &str) -> Result<String> {
    let output = gh_output(args)
        .await
        .with_context(|| format!("failed to spawn `{display}`"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`{display}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// A PR's live `mergeQueueEntry` sub-state (GitHub's merge-queue membership
/// record). `state` is GitHub's raw enum value (`AWAITING_CHECKS`,
/// `MERGEABLE`, `LOCKED`, `QUEUED`, `UNMERGEABLE`); `position` and
/// `enqueued_at` are `None` when GitHub didn't report them (rare — schema
/// says both are non-null while queued, but degrade gracefully rather than
/// drop the whole entry over a partial response).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeQueueEntry {
    pub state: String,
    pub position: Option<i64>,
    /// RFC 3339 timestamp of when the PR entered the queue.
    pub enqueued_at: Option<String>,
}

/// Query GitHub's GraphQL API for `pr_url`'s current merge-queue membership.
/// Returns `Some(entry)` when the PR's `mergeQueueEntry` is non-null (the PR
/// is queued), `None` when it isn't queued or on any error — a non-canonical
/// URL, a spawn/HTTP failure, a non-zero `gh` exit, or unparseable JSON.
///
/// This lives alongside [`gh_output`] because several engine call sites need
/// the same probe. It deliberately uses the GraphQL API rather than a
/// `gh pr view --json` field because `mergeQueueEntry` is not exposed as a
/// `--json` field in all installed versions of the `gh` CLI; the GraphQL API
/// is stable across versions. Degrades gracefully (returns `None`) so callers
/// can treat "not in queue" and "couldn't tell" identically.
pub async fn pr_merge_queue_entry(pr_url: &str) -> Option<MergeQueueEntry> {
    let (owner, repo, number) = crate::pr_url::parse_pr_url_parts(pr_url)?;
    let query = format!(
        r#"{{ repository(owner: "{owner}", name: "{repo}") {{ pullRequest(number: {number}) {{ mergeQueueEntry {{ state position enqueuedAt }} }} }} }}"#
    );
    let output = gh_output(&["api", "graphql", "-f", &format!("query={query}")]).await;
    let out = output.ok()?;
    if !out.status.success() {
        return None;
    }
    let body: Value = serde_json::from_slice(&out.stdout).ok()?;
    let node = &body["data"]["repository"]["pullRequest"]["mergeQueueEntry"];
    if node.is_null() {
        return None;
    }
    Some(MergeQueueEntry {
        state: node["state"].as_str().unwrap_or("").to_owned(),
        position: node["position"].as_i64(),
        enqueued_at: node["enqueuedAt"].as_str().map(str::to_owned),
    })
}

/// Whether `pr_url` is currently in its repository's merge queue. Thin
/// boolean wrapper over [`pr_merge_queue_entry`] for callers that only need
/// membership, not the sub-state detail.
pub async fn pr_in_merge_queue(pr_url: &str) -> bool {
    pr_merge_queue_entry(pr_url).await.is_some()
}

// ── CommandGhRunner (production) ──────────────────────────────────────────────

/// Production [`GhRunner`] that spawns the `gh` CLI binary.
pub struct CommandGhRunner;

/// Scan `gh`'s stderr for an HTTP status code pattern like "(HTTP 404)" or "HTTP 404".
fn parse_http_status_from_stderr(stderr: &str) -> Option<u16> {
    let lower = stderr.to_lowercase();
    if let Some(pos) = lower.find("http ") {
        let after = &stderr[pos + 5..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(code) = digits.parse::<u16>() {
            return Some(code);
        }
    }
    None
}

/// Map a non-success `gh` exit into a status-carrying [`GhRunnerError`],
/// extracting any HTTP status code embedded in stderr.
fn gh_status_error(output: &std::process::Output) -> GhRunnerError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let status = parse_http_status_from_stderr(&stderr).unwrap_or(0);
    GhRunnerError::with_status(status, stderr.trim().to_owned())
}

/// Spawn `gh <args>` (optionally with `GH_TOKEN` set), wait for completion, and
/// return its captured [`Output`](std::process::Output) once the exit status is
/// verified. Spawn failures map to transient errors; non-zero exits map via
/// [`gh_status_error`].
async fn execute_gh(args: &[String], token: Option<&str>) -> std::result::Result<std::process::Output, GhRunnerError> {
    let mut cmd = Command::new("gh");
    if let Some(t) = token {
        cmd.env("GH_TOKEN", t);
    }
    let output = cmd
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| GhRunnerError::transient(format!("failed to spawn gh: {e}")))?;

    if !output.status.success() {
        return Err(gh_status_error(&output));
    }
    Ok(output)
}

#[async_trait]
impl GhRunner for CommandGhRunner {
    async fn graphql(
        &self,
        query: &str,
        vars: &[(&str, &str)],
        token: Option<&str>,
    ) -> std::result::Result<Value, GhRunnerError> {
        let mut args = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={query}"),
        ];
        for (k, v) in vars {
            args.push("-F".to_owned());
            args.push(format!("{k}={v}"));
        }
        let output = execute_gh(&args, token).await?;

        serde_json::from_slice(&output.stdout)
            .map_err(|e| GhRunnerError::transient(format!("failed to parse graphql response: {e}")))
    }

    async fn rest_get(&self, path: &str, token: Option<&str>) -> std::result::Result<GhResponse, GhRunnerError> {
        let output = execute_gh(&["api".to_owned(), path.to_owned()], token).await?;

        let body = serde_json::from_slice(&output.stdout)
            .map_err(|e| GhRunnerError::transient(format!("failed to parse REST response: {e}")))?;
        Ok(GhResponse { body })
    }

    async fn rest_patch(
        &self,
        path: &str,
        fields: &[(&str, &str)],
        token: Option<&str>,
    ) -> std::result::Result<GhResponse, GhRunnerError> {
        let mut args = vec!["api".to_owned(), "-X".to_owned(), "PATCH".to_owned(), path.to_owned()];
        for (k, v) in fields {
            args.push("-f".to_owned());
            args.push(format!("{k}={v}"));
        }
        let output = execute_gh(&args, token).await?;

        let body = serde_json::from_slice(&output.stdout)
            .map_err(|e| GhRunnerError::transient(format!("failed to parse PATCH response: {e}")))?;
        Ok(GhResponse { body })
    }

    async fn rest_post(
        &self,
        path: &str,
        body: &serde_json::Value,
        token: Option<&str>,
    ) -> std::result::Result<GhResponse, GhRunnerError> {
        use tokio::io::AsyncWriteExt as _;
        let stdin_bytes = serde_json::to_vec(body)
            .map_err(|e| GhRunnerError::transient(format!("failed to serialize POST body: {e}")))?;
        let mut cmd = Command::new("gh");
        if let Some(t) = token {
            cmd.env("GH_TOKEN", t);
        }
        cmd.args(["api", "-X", "POST", "--input", "-", path])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| GhRunnerError::transient(format!("failed to spawn gh: {e}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&stdin_bytes)
                .await
                .map_err(|e| GhRunnerError::transient(format!("failed to write POST body: {e}")))?;
        }
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| GhRunnerError::transient(format!("failed to wait for gh: {e}")))?;

        if !output.status.success() {
            return Err(gh_status_error(&output));
        }

        let body = serde_json::from_slice(&output.stdout)
            .map_err(|e| GhRunnerError::transient(format!("failed to parse POST response: {e}")))?;
        Ok(GhResponse { body })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::Output;

    use super::*;

    /// Build a failed `gh` [`Output`] (non-zero exit) carrying `stderr`, so we
    /// can exercise [`gh_status_error`]'s observable behavior end to end.
    fn failed_output(stderr: &str) -> Output {
        Output {
            // Non-zero exit code: a real `gh` failure.
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn parses_http_404_not_found() {
        assert_eq!(parse_http_status_from_stderr("gh: Not Found (HTTP 404)"), Some(404));
    }

    #[test]
    fn parses_rate_limit_429() {
        assert_eq!(
            parse_http_status_from_stderr("gh: API rate limit exceeded (HTTP 429)"),
            Some(429)
        );
    }

    #[test]
    fn parses_server_error_503() {
        assert_eq!(
            parse_http_status_from_stderr("gh: Service Unavailable (HTTP 503)"),
            Some(503)
        );
    }

    #[test]
    fn parses_status_case_insensitively() {
        assert_eq!(parse_http_status_from_stderr("internal error: http 500"), Some(500));
    }

    #[test]
    fn api_url_is_not_mistaken_for_a_status() {
        // A bare URL has no "http " token (no space after the scheme), so the
        // https:// prefix must not produce a false-positive status code.
        assert_eq!(
            parse_http_status_from_stderr("error connecting to https://api.github.com/repos/o/r"),
            None
        );
    }

    #[test]
    fn stderr_without_a_status_yields_none() {
        assert_eq!(parse_http_status_from_stderr("could not connect to host"), None);
    }

    #[test]
    fn malformed_non_numeric_status_yields_none() {
        assert_eq!(parse_http_status_from_stderr("gh: weird (HTTP abc)"), None);
    }

    #[test]
    fn status_error_carries_parsed_status_and_trimmed_message() {
        let err = gh_status_error(&failed_output("  gh: Not Found (HTTP 404)\n"));
        assert_eq!(err.http_status, Some(404));
        assert_eq!(err.message, "gh: Not Found (HTTP 404)");
    }

    #[test]
    fn status_error_without_code_yields_status_zero_and_trimmed_message() {
        // No HTTP status in stderr => status 0 and the trimmed stderr as the
        // message, so callers treat it as a non-HTTP (transient) failure.
        let err = gh_status_error(&failed_output("\ncould not connect to host\n"));
        assert_eq!(err.http_status, Some(0));
        assert_eq!(err.message, "could not connect to host");
    }
}
