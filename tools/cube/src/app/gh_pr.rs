//! Shared REST-backed GitHub PR lookups.
//!
//! `gh pr view` and `gh pr list` are implemented over GitHub's GraphQL API,
//! which draws from a quota entirely separate from REST. Every lookup in
//! `cube` that resolves a PR's head branch or lifecycle state (`workspace
//! goto`, `workspace rebase`, `pr push`) does so through `gh api` against the
//! REST `pulls` endpoints instead, so a fleet of workers exhausting the
//! GraphQL budget never blocks these lookups — the separate, far-less-
//! contended REST (`core`) quota serves them.
//!
//! Every remaining lookup here is about *one* PR the caller already has in
//! hand. There is deliberately no bulk PR-state helper: the sole caller that
//! ever wanted one was GC's closed-PR bookmark sweep, and that sweep no
//! longer consults GitHub at all — it decides which `pr/<n>` bookmarks are
//! still referenced from local workspace state instead (see
//! [`crate::app::gc::collect_unreferenced_pr_bookmarks`]). Reintroducing a
//! batch helper here would mean some caller is once again sizing a GitHub
//! round trip by how much local bookkeeping has piled up, which is the shape
//! that hung `cube workspace release` for 4-6+ minutes.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::app::errors::{CubeError, Result};
use crate::command_runner::{CommandRunner, RealCommandRunner};

/// Fetch the raw REST representation of a PR
/// (`GET /repos/{owner}/{repo}/pulls/{number}`), never GraphQL.
///
/// On failure, the returned error's message is enriched with the quota reset
/// time when the failure looks like GitHub rate-limiting, so a caller sees
/// "retry after T" instead of a bare, opaque failure.
pub(super) fn fetch_pr_json(runner: &dyn CommandRunner, cwd: &Path, owner_repo: &str, pr_number: u64) -> Result<Value> {
    let api_path = format!("repos/{owner_repo}/pulls/{pr_number}");
    let json = runner
        .run(&RealCommandRunner::invocation(cwd, "gh", &["api", &api_path]))
        .map_err(|e| enrich_rate_limit_error(runner, cwd, e, RATE_LIMIT_HINT_TIMEOUT))?;
    Ok(serde_json::from_str(&json)?)
}

/// Extract `head.ref` from a PR's REST JSON.
pub(super) fn head_ref(value: &Value) -> Option<&str> {
    value.get("head").and_then(|h| h.get("ref")).and_then(|v| v.as_str())
}

/// Normalize a PR's REST JSON to the `"OPEN"` / `"CLOSED"` / `"MERGED"`
/// vocabulary `gh pr view --json state` used to return. REST's own `state`
/// field only ever holds `"open"`/`"closed"`; `merged` is a separate bool.
pub(super) fn state(value: &Value) -> String {
    let merged = value.get("merged").and_then(|v| v.as_bool()).unwrap_or(false);
    if merged {
        return "MERGED".to_string();
    }
    value
        .get("state")
        .and_then(|v| v.as_str())
        .map(str::to_uppercase)
        .unwrap_or_else(|| "UNKNOWN".to_string())
}

/// List every open PR whose head is `branch`, over REST
/// (`GET /repos/{owner}/{repo}/pulls?head=<owner>:<branch>&state=open`)
/// rather than `gh pr list`. Each element is the PR's full REST JSON (so a
/// caller can read `number`, `html_url`, etc.); an empty `Vec` means the
/// lookup succeeded but no open PR has that head — a `gh`/network failure
/// is a real `Err`.
pub(super) fn list_open_prs_for_branch(
    runner: &dyn CommandRunner,
    cwd: &Path,
    owner_repo: &str,
    branch: &str,
) -> Result<Vec<Value>> {
    let owner = owner_repo.split('/').next().unwrap_or(owner_repo);
    let api_path = format!("repos/{owner_repo}/pulls?head={owner}:{branch}&state=open");
    let json = runner
        .run(&RealCommandRunner::invocation(cwd, "gh", &["api", &api_path]))
        .map_err(|e| enrich_rate_limit_error(runner, cwd, e, RATE_LIMIT_HINT_TIMEOUT))?;
    Ok(serde_json::from_str(&json)?)
}

/// Find the number of the (single) open PR whose head is `branch`. `Ok(None)`
/// means the lookup succeeded but no open PR has that head.
pub(super) fn find_open_pr_for_branch(
    runner: &dyn CommandRunner,
    cwd: &Path,
    owner_repo: &str,
    branch: &str,
) -> Result<Option<u64>> {
    let prs = list_open_prs_for_branch(runner, cwd, owner_repo, branch)?;
    Ok(prs.first().and_then(|pr| pr["number"].as_u64()))
}

/// GitHub's REST (`core`) quota — the only one anything in this module draws
/// from, since every call here goes through `gh api` against a REST endpoint.
/// Reported by [`quota_reset_hint`] so a rate-limited caller is told when the
/// quota it actually exhausted resets.
const CORE_QUOTA_JQ_PATH: &str = ".resources.core.reset";

/// Cap on the best-effort `gh api rate_limit` enrichment call, independent of
/// the timeout the caller's own request used — the endpoint is cheap, so a
/// short bound is enough to keep it from becoming a second unbounded `gh`
/// subprocess on the same teardown path.
const RATE_LIMIT_HINT_TIMEOUT: Duration = Duration::from_secs(10);

/// When `err` looks like a GitHub rate-limit rejection, append when the REST
/// quota resets so the message reads "retry after T" instead of a bare
/// failure. Best-effort: the reset lookup itself is a separate, cheap `gh api
/// rate_limit` call, bounded by `timeout` so it can never itself hang the
/// caller; any failure (including a timeout) to enrich just falls back to
/// the original error untouched.
fn enrich_rate_limit_error(runner: &dyn CommandRunner, cwd: &Path, err: CubeError, timeout: Duration) -> CubeError {
    let message = err.to_string();
    if !looks_like_rate_limit(&message) {
        return err;
    }
    match quota_reset_hint(runner, cwd, timeout) {
        Some(hint) => CubeError::InvalidArgument(format!("{message} ({hint})")),
        None => err,
    }
}

fn looks_like_rate_limit(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("rate limit") || lower.contains("http 403") || lower.contains("exit code 1: http/2.0 403")
}

/// Best-effort lookup of when the REST quota resets, as a human-readable
/// "retry after" hint, bounded by `timeout`. Returns `None` on any failure
/// (including a timeout) to reach or parse the `rate_limit` endpoint — the
/// caller falls back to the unenriched error rather than propagating a
/// second failure.
fn quota_reset_hint(runner: &dyn CommandRunner, cwd: &Path, timeout: Duration) -> Option<String> {
    let out = runner
        .run_with_timeout(
            &RealCommandRunner::invocation(cwd, "gh", &["api", "rate_limit", "--jq", CORE_QUOTA_JQ_PATH]),
            timeout,
        )
        .ok()?;
    let reset_epoch: u64 = out.trim().parse().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let in_secs = reset_epoch.saturating_sub(now);
    Some(format!(
        "rate-limited; REST quota resets in ~{in_secs}s (unix time {reset_epoch}) — retry after then"
    ))
}
