//! Shared `gh api repos/{slug}/compare/{base}...{head}` spawn envelope.
//!
//! This is the single source of truth for the compare endpoint, Accept header,
//! and subprocess envelope used by every Boss compare fetcher. Callers keep
//! their own type-specific parsing of the returned string and their own
//! fail-open vs fail-closed semantics — these helpers only build the request
//! and return raw stdout.

use anyhow::{Context, Result};

use crate::gh_runner::{gh_output_blocking, run_gh};

/// GitHub JSON Accept header shared by every compare call.
pub const GH_COMPARE_ACCEPT: &str = "Accept: application/vnd.github+json";

/// `repos/{slug}/compare/{base}...{head}` endpoint used by every engine
/// compare fetcher.
pub fn gh_compare_endpoint(repo_slug: &str, base: &str, head: &str) -> String {
    format!("repos/{repo_slug}/compare/{base}...{head}")
}

/// `gh api` argv for a compare: endpoint, Accept header, jq projection.
pub fn gh_compare_api_args<'a>(endpoint: &'a str, jq: &'a str) -> [&'a str; 6] {
    ["api", endpoint, "-H", GH_COMPARE_ACCEPT, "--jq", jq]
}

/// Shell out to `gh api repos/<repo_slug>/compare/<base>...<head>` with the
/// GitHub JSON `Accept` header and the caller-supplied `jq` projection,
/// returning the trimmed stdout.
pub async fn gh_compare_jq(repo_slug: &str, base: &str, head: &str, jq: &str) -> Result<String> {
    let endpoint = gh_compare_endpoint(repo_slug, base, head);
    let stdout = run_gh(&gh_compare_api_args(&endpoint, jq), &format!("gh api {endpoint}")).await?;
    Ok(stdout.trim().to_owned())
}

/// Blocking counterpart of [`gh_compare_jq`] for call sites that already
/// run off the tokio runtime (e.g. `spawn_blocking` review-verdict apply).
pub fn gh_compare_jq_blocking(repo_slug: &str, base: &str, head: &str, jq: &str) -> Result<String> {
    let endpoint = gh_compare_endpoint(repo_slug, base, head);
    let display = format!("gh api {endpoint}");
    let output = gh_output_blocking(&gh_compare_api_args(&endpoint, jq))
        .with_context(|| format!("failed to spawn `{display}`"))?;
    if !output.status.success() {
        anyhow::bail!("`{display}` failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Fetch GitHub's merge-base commit for two immutable commit identities. The
/// comparison response's file list is deliberately ignored because that API
/// can cap it; complete file inventory comes from
/// [`crate::pr_files::fetch_complete_pr_file_inventory`].
pub async fn fetch_merge_base(repository: &str, base_sha: &str, head_sha: &str) -> Result<String> {
    let (owner, repo) = repository
        .split_once('/')
        .filter(|(owner, repo)| !owner.is_empty() && !repo.is_empty())
        .with_context(|| format!("invalid comparison repository identity `{repository}`"))?;
    let stdout = gh_compare_jq(&format!("{owner}/{repo}"), base_sha, head_sha, ".merge_base_commit.sha").await?;
    parse_merge_base_sha(&stdout).with_context(|| {
        format!("`gh api compare {owner}/{repo} {base_sha}...{head_sha}` returned no merge_base_commit.sha")
    })
}

fn parse_merge_base_sha(stdout: &str) -> Option<String> {
    let sha = stdout.trim().trim_matches('"');
    (!sha.is_empty() && sha != "null").then(|| sha.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_helpers_share_endpoint_and_args() {
        let endpoint = gh_compare_endpoint("org/repo", "abc", "def");
        assert_eq!(endpoint, "repos/org/repo/compare/abc...def");
        assert_eq!(
            gh_compare_api_args(&endpoint, ".merge_base_commit.sha"),
            [
                "api",
                "repos/org/repo/compare/abc...def",
                "-H",
                "Accept: application/vnd.github+json",
                "--jq",
                ".merge_base_commit.sha",
            ],
        );
    }

    #[test]
    fn merge_base_sha_rejects_empty_or_json_null() {
        assert_eq!(parse_merge_base_sha("  abcdef  ").as_deref(), Some("abcdef"));
        assert_eq!(parse_merge_base_sha("\"abcdef\"").as_deref(), Some("abcdef"));
        assert!(parse_merge_base_sha("").is_none());
        assert!(parse_merge_base_sha("null").is_none());
    }
}
