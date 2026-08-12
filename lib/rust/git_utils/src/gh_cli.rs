//! Helpers that invoke the `gh` CLI to fetch GitHub PR head metadata.
//!
//! These functions shell out to the `gh` binary rather than using the
//! GitHub REST API directly. They are suitable for contexts where a
//! short-lived `gh`-authenticated call is simpler than a full App-JWT
//! flow — in particular, fetching PR head SHAs / branch names from the
//! engine without requiring embedded App credentials.

use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
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
        .with_context(|| format!("failed to spawn `{display}`"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`{display}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
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

/// Validate and return the SHA string from `gh pr view ... --jq .headRefOid`
/// stdout. Returns an error when the output is empty (which means GitHub
/// returned a null or the JQ filter found nothing).
pub(crate) fn parse_head_sha_output(sha: String, pr_number: u64, repo_slug: &str) -> Result<String> {
    if sha.is_empty() {
        return Err(anyhow!("empty headRefOid for PR {pr_number} in {repo_slug}"));
    }
    Ok(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
