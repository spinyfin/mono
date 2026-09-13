//! Shared `gh pr view --json ...` fetch/parse helpers for callers that only
//! need a PR's changed-file paths (or a superset that includes them).
//!
//! Before this module existed, three call sites in `boss-engine`
//! (`design_detector`, `runner`, `stacked_pr_structuring`) each hand-rolled
//! their own `gh pr view <url> --json <fields>` shellout, exit-code check,
//! and JSON parse. `fetch_pr_view_json` centralizes the shellout (on top of
//! [`crate::gh_runner::run_gh`]'s existing spawn/exit-code boilerplate) for
//! any field set; [`parse_changed_file_paths`] and [`fetch_pr_changed_files`]
//! cover the common paths-only case.

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::gh_runner::{gh_output, run_gh};
use crate::pr_url::parse_pr_url_parts;

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

/// Immutable endpoints and presentation fields returned by GitHub's pull
/// request REST resource. Source collectors must use these SHAs rather than a
/// branch name or an ambient worker checkout.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct PrComparisonMetadata {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub base_repository: String,
    pub head_repository: String,
    pub head_ref_name: String,
    pub base_sha: String,
    pub head_sha: String,
    pub changed_files: u64,
}

/// One file from GitHub's paginated `pulls/{number}/files` REST endpoint.
/// The API's `patch` is intentionally optional: omission is meaningful (for
/// example a binary or an API size cap) and callers must never equate it with
/// an empty diff.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct PrFileInventoryEntry {
    pub filename: String,
    pub previous_filename: Option<String>,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    pub patch: Option<String>,
}

/// Run `gh pr view <pr_url> --json <fields>` and parse stdout as JSON.
/// `fields` is a comma-separated list, e.g. `"files"` or
/// `"files,headRefName,baseRefName"` — callers that need more than the
/// changed-file paths (e.g. ref names, PR body, commits) use this directly
/// and pick their own fields out of the returned [`Value`].
pub async fn fetch_pr_view_json(pr_url: &str, fields: &str) -> Result<Value> {
    let display = format!("gh pr view {pr_url} --json {fields}");
    let stdout = run_gh(&["pr", "view", pr_url, "--json", fields], &display).await?;
    serde_json::from_str(&stdout).with_context(|| format!("failed to parse `{display}` JSON"))
}

/// Pure extraction of `files[].path` from a `gh pr view --json files...`
/// response. A missing or non-array `files` key yields an empty vec.
pub fn parse_changed_file_paths(root: &Value) -> Vec<String> {
    root.get("files")
        .and_then(|v| v.as_array())
        .map(|files| {
            files
                .iter()
                .filter_map(|f| f.get("path").and_then(|p| p.as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Fetch a PR's changed-file paths: `gh pr view <pr_url> --json files` plus
/// [`parse_changed_file_paths`]. The paths-only convenience wrapper for
/// callers that don't need any other `gh pr view` field.
pub async fn fetch_pr_changed_files(pr_url: &str) -> Result<Vec<String>> {
    let root = fetch_pr_view_json(pr_url, "files").await?;
    Ok(parse_changed_file_paths(&root))
}

/// Fetch canonical comparison metadata for a PR through the same `gh`
/// transport used elsewhere in Boss. A missing fork head repository or any
/// missing SHA is an error: source collection must not fall back to the
/// mutable base repository or branch tip.
pub async fn fetch_pr_comparison_metadata(pr_url: &str) -> Result<PrComparisonMetadata> {
    let (owner, repo, number) = parse_pr_url_parts(pr_url)
        .with_context(|| format!("PR source capture requires a canonical GitHub PR URL, got `{pr_url}`"))?;
    let endpoint = format!("repos/{owner}/{repo}/pulls/{number}");
    let output = gh_output(&["api", &endpoint]).await?;
    if !output.status.success() {
        bail!(
            "`gh api {endpoint}` failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    let value: Value =
        serde_json::from_slice(&output.stdout).with_context(|| format!("failed to parse `gh api {endpoint}` JSON"))?;
    parse_pr_comparison_metadata(&value)
}

/// Fetch every changed-file entry and reject partial coverage. GitHub's REST
/// endpoint is paginated and `gh --paginate --slurp` returns one nested array
/// per page; flattening only after checking the top-level shape keeps a
/// missing/changed response from looking like an empty comparison.
pub async fn fetch_complete_pr_file_inventory(
    repository: &str,
    number: u64,
    expected_changed_files: u64,
) -> Result<Vec<PrFileInventoryEntry>> {
    let (owner, repo) = repository
        .split_once('/')
        .filter(|(owner, repo)| !owner.is_empty() && !repo.is_empty())
        .with_context(|| format!("invalid PR repository identity `{repository}`"))?;
    let endpoint = format!("repos/{owner}/{repo}/pulls/{number}/files?per_page=100");
    let output = gh_output(&["api", "--paginate", "--slurp", &endpoint]).await?;
    if !output.status.success() {
        bail!(
            "`gh api --paginate {endpoint}` failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("failed to parse paginated `gh api {endpoint}` JSON"))?;
    let entries = parse_paginated_pr_file_inventory(&value)?;
    if entries.len() as u64 != expected_changed_files {
        bail!(
            "GitHub changed-file coverage is incomplete: metadata reported {expected_changed_files} files but paginated inventory returned {}",
            entries.len(),
        );
    }
    Ok(entries)
}

/// Fetch GitHub's merge-base commit for two immutable commit identities. The
/// comparison response's file list is deliberately ignored because that API
/// can cap it; complete file inventory comes from
/// [`fetch_complete_pr_file_inventory`].
pub async fn fetch_merge_base(repository: &str, base_sha: &str, head_sha: &str) -> Result<String> {
    let (owner, repo) = repository
        .split_once('/')
        .filter(|(owner, repo)| !owner.is_empty() && !repo.is_empty())
        .with_context(|| format!("invalid comparison repository identity `{repository}`"))?;
    let endpoint = gh_compare_endpoint(&format!("{owner}/{repo}"), base_sha, head_sha);
    let output = gh_output(&gh_compare_api_args(&endpoint, ".merge_base_commit.sha")).await?;
    if !output.status.success() {
        bail!(
            "`gh api {endpoint}` failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    parse_merge_base_sha(&String::from_utf8_lossy(&output.stdout))
        .with_context(|| format!("`gh api {endpoint}` returned no merge_base_commit.sha"))
}

fn parse_merge_base_sha(stdout: &str) -> Option<String> {
    let sha = stdout.trim().trim_matches('"');
    (!sha.is_empty() && sha != "null").then(|| sha.to_owned())
}

/// Strict pure parser for the PR metadata contract used by source capture.
pub fn parse_pr_comparison_metadata(value: &Value) -> Result<PrComparisonMetadata> {
    let string = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .with_context(|| format!("PR metadata is missing required `{pointer}`"))
    };
    Ok(PrComparisonMetadata {
        number: value
            .get("number")
            .and_then(Value::as_u64)
            .context("PR metadata is missing required `number`")?,
        title: string("/title")?,
        body: value.get("body").and_then(Value::as_str).map(str::to_owned),
        base_repository: string("/base/repo/full_name")?,
        head_repository: string("/head/repo/full_name")?,
        head_ref_name: string("/head/ref")?,
        base_sha: string("/base/sha")?,
        head_sha: string("/head/sha")?,
        changed_files: value
            .get("changed_files")
            .and_then(Value::as_u64)
            .context("PR metadata is missing required `changed_files`")?,
    })
}

/// Parse `gh api --paginate --slurp` output without silently accepting a
/// missing page, a malformed file entry, or an unexpected response shape.
pub fn parse_paginated_pr_file_inventory(value: &Value) -> Result<Vec<PrFileInventoryEntry>> {
    let pages = value
        .as_array()
        .context("paginated PR file inventory must be an array of pages")?;
    let mut entries = Vec::new();
    for (page_index, page) in pages.iter().enumerate() {
        let page = page
            .as_array()
            .with_context(|| format!("PR file inventory page {page_index} is not an array"))?;
        for (entry_index, entry) in page.iter().enumerate() {
            let required = |name: &str| {
                entry
                    .get(name)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .with_context(|| {
                        format!("PR file inventory page {page_index} entry {entry_index} is missing `{name}`")
                    })
            };
            entries.push(PrFileInventoryEntry {
                filename: required("filename")?,
                previous_filename: entry
                    .get("previous_filename")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                status: required("status")?,
                additions: entry.get("additions").and_then(Value::as_u64).with_context(|| {
                    format!("PR file inventory page {page_index} entry {entry_index} is missing `additions`")
                })?,
                deletions: entry.get("deletions").and_then(Value::as_u64).with_context(|| {
                    format!("PR file inventory page {page_index} entry {entry_index} is missing `deletions`")
                })?,
                patch: entry.get("patch").and_then(Value::as_str).map(str::to_owned),
            });
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_changed_file_paths_extracts_paths() {
        let root = serde_json::json!({
            "files": [
                {"path": "src/a.rs", "changeType": "MODIFIED"},
                {"path": "src/b.rs", "changeType": "ADDED"},
            ]
        });
        assert_eq!(parse_changed_file_paths(&root), vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn parse_changed_file_paths_missing_files_key_is_empty() {
        let root = serde_json::json!({"headRefName": "foo"});
        assert!(parse_changed_file_paths(&root).is_empty());
    }

    #[test]
    fn parse_changed_file_paths_non_array_files_is_empty() {
        let root = serde_json::json!({"files": "not-an-array"});
        assert!(parse_changed_file_paths(&root).is_empty());
    }

    #[test]
    fn parse_changed_file_paths_skips_entries_without_path() {
        let root = serde_json::json!({"files": [{"changeType": "MODIFIED"}, {"path": "src/c.rs"}]});
        assert_eq!(parse_changed_file_paths(&root), vec!["src/c.rs"]);
    }

    #[test]
    fn comparison_metadata_requires_immutable_repositories_and_endpoints() {
        let value = serde_json::json!({
            "number": 8,
            "title": "Pin sources",
            "body": null,
            "changed_files": 2,
            "base": {"repo": {"full_name": "acme/widget"}, "sha": "a"},
            "head": {"repo": {"full_name": "fork/widget"}, "ref": "review-guide-sources", "sha": "b"},
        });
        assert_eq!(
            parse_pr_comparison_metadata(&value).unwrap(),
            PrComparisonMetadata {
                number: 8,
                title: "Pin sources".to_owned(),
                body: None,
                base_repository: "acme/widget".to_owned(),
                head_repository: "fork/widget".to_owned(),
                head_ref_name: "review-guide-sources".to_owned(),
                base_sha: "a".to_owned(),
                head_sha: "b".to_owned(),
                changed_files: 2,
            }
        );
        let no_head_repo = serde_json::json!({
            "number": 8, "title": "Pin sources", "changed_files": 0,
            "base": {"repo": {"full_name": "acme/widget"}, "sha": "a"},
            "head": {"repo": null, "sha": "b"},
        });
        assert!(parse_pr_comparison_metadata(&no_head_repo).is_err());
    }

    #[test]
    fn paginated_inventory_preserves_rename_and_missing_patch() {
        let pages = serde_json::json!([[
            {"filename": "new.rs", "previous_filename": "old.rs", "status": "renamed", "additions": 1, "deletions": 1},
            {"filename": "image.png", "status": "modified", "additions": 0, "deletions": 0, "patch": null}
        ]]);
        assert_eq!(
            parse_paginated_pr_file_inventory(&pages).unwrap(),
            vec![
                PrFileInventoryEntry {
                    filename: "new.rs".to_owned(),
                    previous_filename: Some("old.rs".to_owned()),
                    status: "renamed".to_owned(),
                    additions: 1,
                    deletions: 1,
                    patch: None,
                },
                PrFileInventoryEntry {
                    filename: "image.png".to_owned(),
                    previous_filename: None,
                    status: "modified".to_owned(),
                    additions: 0,
                    deletions: 0,
                    patch: None,
                },
            ]
        );
    }

    #[test]
    fn paginated_inventory_rejects_missing_file_keys() {
        let pages = serde_json::json!([[{"filename": "a.rs", "status": "modified", "additions": 1}]]);
        assert!(parse_paginated_pr_file_inventory(&pages).is_err());
    }

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
