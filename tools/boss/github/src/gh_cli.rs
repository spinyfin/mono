//! Helpers that invoke the `gh` CLI to fetch GitHub PR metadata.
//!
//! These functions shell out to the `gh` binary rather than using the
//! GitHub REST API directly. They are suitable for contexts where a
//! short-lived `gh`-authenticated call is simpler than a full App-JWT
//! flow — in particular, fetching PR head SHAs from the engine without
//! requiring embedded App credentials.

use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use tokio::process::Command;

/// Fetch the head commit SHA for a PR by shelling out to
/// `gh pr view <pr_number> -R <repo_slug> --json headRefOid --jq .headRefOid`.
///
/// Returns an error if the command fails or if the returned SHA is empty.
pub async fn fetch_pr_head_sha(repo_slug: &str, pr_number: u64) -> Result<String> {
    let pr_str = pr_number.to_string();
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_str,
            "-R",
            repo_slug,
            "--json",
            "headRefOid",
            "--jq",
            ".headRefOid",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to spawn `gh pr view {pr_number}` to fetch head SHA"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`gh pr view {pr_number} -R {repo_slug} --json headRefOid` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    parse_head_sha_output(sha, pr_number, repo_slug)
}

/// Validate and return the SHA string from `gh pr view ... --jq .headRefOid`
/// stdout. Returns an error when the output is empty (which means GitHub
/// returned a null or the JQ filter found nothing).
pub(crate) fn parse_head_sha_output(
    sha: String,
    pr_number: u64,
    repo_slug: &str,
) -> Result<String> {
    if sha.is_empty() {
        return Err(anyhow!(
            "empty headRefOid for PR {pr_number} in {repo_slug}"
        ));
    }
    Ok(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_head_sha_output_returns_sha_unchanged() {
        let sha = parse_head_sha_output(
            "abc123deadbeef".to_owned(),
            42,
            "spinyfin/mono",
        )
        .unwrap();
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
        assert!(
            msg.contains("99"),
            "error should include the PR number: {msg}"
        );
        assert!(
            msg.contains("owner/repo"),
            "error should include the repo slug: {msg}"
        );
    }

    #[test]
    fn parse_head_sha_output_preserves_40_char_sha() {
        let full_sha = "a".repeat(40);
        let result = parse_head_sha_output(full_sha.clone(), 1, "org/repo").unwrap();
        assert_eq!(result, full_sha);
    }
}
