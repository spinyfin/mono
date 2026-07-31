//! Shared REST-backed GitHub PR lookups.
//!
//! `gh pr view` and `gh pr list` are implemented over GitHub's GraphQL API,
//! which draws from a quota entirely separate from REST. Every single-PR hot
//! path in `cube` that resolves a PR's head branch or lifecycle state
//! (`workspace goto`, `workspace rebase`, `pr push`) does so through `gh api`
//! against the REST `pulls` endpoints instead, so a fleet of workers
//! exhausting the GraphQL budget never blocks these lookups — the separate,
//! far-less-contended REST (`core`) quota serves them.
//!
//! The one exception is [`fetch_pr_states_batch`], used by GC's closed-PR
//! bookmark sweep: that caller walks *every* `pr/<n>` bookmark a reused
//! workspace has accumulated, not a single PR, and REST's `pulls/{n}`
//! endpoint has no batch form — one bookmark meant one full serial round
//! trip, unbounded by any timeout, which was observed to hang `cube
//! workspace release` for 4-6+ minutes on a single stuck call. GraphQL's
//! aliasing lets the whole sweep go out as one round trip instead, the same
//! technique the merge-poller reconciler uses for its own batched probe
//! (`tools/boss/engine/core/src/merge_poller/probe.rs`).

use std::collections::HashMap;
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
        .map_err(|e| enrich_rate_limit_error(runner, cwd, e, RateLimitQuota::Core, RATE_LIMIT_HINT_TIMEOUT))?;
    Ok(serde_json::from_str(&json)?)
}

/// Fetch the `state` (`"OPEN"` / `"CLOSED"` / `"MERGED"`) of every PR in
/// `pr_numbers`, all assumed to belong to `owner_repo`, in a single `gh api
/// graphql` round trip via aliased `pullRequest(...)` blocks — instead of one
/// REST call per PR. See the module doc for why this one lookup goes through
/// GraphQL when everything else here deliberately doesn't.
///
/// Returns an empty map for an empty `pr_numbers` without spawning a
/// subprocess. A PR number absent from the returned map means GraphQL's node
/// for it came back null (force-deleted/transferred) — the caller decides
/// how to treat that, same as a REST 404 would. In practice `gh api graphql`
/// exits non-zero whenever the response body carries a top-level `errors`
/// array, even alongside otherwise-usable partial `data` (a per-node
/// resolution error, e.g. one node hitting a permissions issue), so that
/// case surfaces as `Err` for the whole batch rather than a partial map —
/// the caller cannot distinguish "one node had an error" from "genuinely
/// nothing resolved" here.
///
/// Bounded by `timeout` so a hung or rate-limited GitHub cannot block the
/// caller indefinitely; the timeout (or any other transport failure) is
/// returned as `Err`, never silently swallowed into an empty result, so a
/// caller can tell "genuinely nothing closed" apart from "the lookup never
/// completed".
pub(super) fn fetch_pr_states_batch(
    runner: &dyn CommandRunner,
    cwd: &Path,
    owner_repo: &str,
    pr_numbers: &[u64],
    timeout: Duration,
) -> Result<HashMap<u64, String>> {
    let mut out = HashMap::new();
    if pr_numbers.is_empty() {
        return Ok(out);
    }
    let (owner, repo) = owner_repo
        .split_once('/')
        .ok_or_else(|| CubeError::InvalidArgument(format!("`{owner_repo}` is not an `owner/repo` slug")))?;

    let mut query = format!(r#"{{ repository(owner: "{owner}", name: "{repo}") {{"#);
    for (idx, number) in pr_numbers.iter().enumerate() {
        query.push_str(&format!(" pr{idx}: pullRequest(number: {number}) {{ state }}"));
    }
    query.push_str(" } }");

    let json = runner
        .run_with_timeout(
            &RealCommandRunner::invocation(cwd, "gh", &["api", "graphql", "-f", &format!("query={query}")]),
            timeout,
        )
        .map_err(|e| enrich_rate_limit_error(runner, cwd, e, RateLimitQuota::GraphQl, timeout))?;
    let body: Value = serde_json::from_str(&json)?;

    if let Some(errors) = body.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        return Err(CubeError::InvalidArgument(format!(
            "batched PR-state graphql query for {owner_repo} returned errors: {errors:?}"
        )));
    }

    let repo_node = &body["data"]["repository"];
    for (idx, number) in pr_numbers.iter().enumerate() {
        let alias = format!("pr{idx}");
        if let Some(state) = repo_node.get(alias).and_then(|n| n["state"].as_str()) {
            out.insert(*number, state.to_string());
        }
    }
    Ok(out)
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
        .map_err(|e| enrich_rate_limit_error(runner, cwd, e, RateLimitQuota::Core, RATE_LIMIT_HINT_TIMEOUT))?;
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

/// Which of GitHub's two independent rate-limit quotas a call draws from,
/// so [`quota_reset_hint`] reports the reset time for the quota that was
/// actually exhausted rather than always assuming REST `core`.
#[derive(Clone, Copy)]
enum RateLimitQuota {
    Core,
    GraphQl,
}

impl RateLimitQuota {
    fn jq_path(self) -> &'static str {
        match self {
            RateLimitQuota::Core => ".resources.core.reset",
            RateLimitQuota::GraphQl => ".resources.graphql.reset",
        }
    }

    fn label(self) -> &'static str {
        match self {
            RateLimitQuota::Core => "REST",
            RateLimitQuota::GraphQl => "GraphQL",
        }
    }
}

/// Cap on the best-effort `gh api rate_limit` enrichment call, independent of
/// the timeout the caller's own request used — the endpoint is cheap, so a
/// short bound is enough to keep it from becoming a second unbounded `gh`
/// subprocess on the same teardown path.
const RATE_LIMIT_HINT_TIMEOUT: Duration = Duration::from_secs(10);

/// When `err` looks like a GitHub rate-limit rejection, append when `quota`
/// resets so the message reads "retry after T" instead of a bare failure.
/// Best-effort: the reset lookup itself is a separate, cheap `gh api
/// rate_limit` call, bounded by `timeout` so it can never itself hang the
/// caller; any failure (including a timeout) to enrich just falls back to
/// the original error untouched.
fn enrich_rate_limit_error(
    runner: &dyn CommandRunner,
    cwd: &Path,
    err: CubeError,
    quota: RateLimitQuota,
    timeout: Duration,
) -> CubeError {
    let message = err.to_string();
    if !looks_like_rate_limit(&message) {
        return err;
    }
    match quota_reset_hint(runner, cwd, quota, timeout) {
        Some(hint) => CubeError::InvalidArgument(format!("{message} ({hint})")),
        None => err,
    }
}

fn looks_like_rate_limit(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("rate limit") || lower.contains("http 403") || lower.contains("exit code 1: http/2.0 403")
}

/// Best-effort lookup of when `quota` resets, as a human-readable "retry
/// after" hint, bounded by `timeout`. Returns `None` on any failure
/// (including a timeout) to reach or parse the `rate_limit` endpoint — the
/// caller falls back to the unenriched error rather than propagating a
/// second failure.
fn quota_reset_hint(
    runner: &dyn CommandRunner,
    cwd: &Path,
    quota: RateLimitQuota,
    timeout: Duration,
) -> Option<String> {
    let out = runner
        .run_with_timeout(
            &RealCommandRunner::invocation(cwd, "gh", &["api", "rate_limit", "--jq", quota.jq_path()]),
            timeout,
        )
        .ok()?;
    let reset_epoch: u64 = out.trim().parse().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let in_secs = reset_epoch.saturating_sub(now);
    Some(format!(
        "rate-limited; {} quota resets in ~{in_secs}s (unix time {reset_epoch}) — retry after then",
        quota.label()
    ))
}
