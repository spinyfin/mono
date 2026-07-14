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

use anyhow::{Context, Result};
use serde_json::Value;

use crate::gh_runner::run_gh;

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
}
