//! Deterministic both-parents deletion tripwire (incident-002 remediation P2).
//!
//! Incident 002 (`tools/boss/docs/postmortems/incident-002-merge-conflict-\
//! deletion-blessed-by-review.md`): a merge-conflict / forward-port resolution
//! **deleted a feature a merged parent had just added** (flunge
//! `RecommendationBadge.tsx`, merged eight minutes earlier), rationalised it as
//! "supersedes", and the automated reviewer — anchored on the worker's *stated
//! purpose* — blessed the removal.
//!
//! The structural fix (postmortem §5 P2, the highest-leverage item) is a check
//! that never asks "did the worker mean to?" and only asks "did a merged parent
//! lose functionality?". For a PR that resolves a merge / forward-port, diff the
//! resolution against **both merge parents** — the PR's prior head AND the moved
//! base — not just `main`. Any file a **merged** parent added and the resolution
//! removes is a finding **regardless of the worker's stated rationale**.
//!
//! This module owns the deterministic, rename/move-aware computation. It is
//! anchored on the *fact* of the deletion, so a confident "supersedes" narrative
//! cannot walk through it (unlike a keyword- or prompt-only check). Diffing is
//! done through GitHub's compare API (`.files[]` with `status` and
//! `previous_filename`), so no local checkout is required.
//!
//! Symbol-level detection ("exported surface") is left to the reviewer rubric;
//! this engine-side tripwire is file-level, which is what caught the incident
//! (`RecommendationBadge.tsx` returned 404) and is deterministic without a
//! language-aware parser.

use anyhow::Result;
use std::collections::BTreeSet;

/// One entry from GitHub's compare `.files[]` array.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CompareFile {
    pub filename: String,
    /// `added` | `removed` | `modified` | `renamed` | `copied` | `changed` |
    /// `unchanged`.
    pub status: String,
    /// Present for `renamed` / `copied` entries: the path before the move.
    #[serde(default)]
    pub previous_filename: Option<String>,
}

/// The set of files **added** between the two compared refs (`status == added`).
pub fn added_filenames(files: &[CompareFile]) -> BTreeSet<String> {
    files
        .iter()
        .filter(|f| f.status == "added")
        .map(|f| f.filename.clone())
        .collect()
}

/// The set of files **removed** between the two compared refs
/// (`status == removed`), rename/move-aware: a path that appears as some
/// entry's `previous_filename` was *moved*, not deleted, so it is excluded even
/// if GitHub also surfaced it as removed. This is what keeps a genuine refactor
/// (rename/relocate a file) from tripping the wire.
pub fn removed_filenames(files: &[CompareFile]) -> BTreeSet<String> {
    let moved_from: BTreeSet<&str> = files.iter().filter_map(|f| f.previous_filename.as_deref()).collect();
    files
        .iter()
        .filter(|f| f.status == "removed")
        .map(|f| f.filename.as_str())
        .filter(|p| !moved_from.contains(p))
        .map(str::to_owned)
        .collect()
}

/// The tripwire set: files a merged parent **added** since the fork AND the
/// resolution **removed**. Sorted, deduped. An empty result means the
/// resolution preserved every surface the merged parent contributed.
pub fn merged_parent_deletions(
    added_on_base: &BTreeSet<String>,
    removed_by_resolution: &BTreeSet<String>,
) -> Vec<String> {
    added_on_base.intersection(removed_by_resolution).cloned().collect()
}

/// Render the tripwire file set as reviewer/operator-facing description lines.
pub fn describe_deletions(files: &[String]) -> Vec<String> {
    files
        .iter()
        .map(|f| format!("`{f}` — added by a merged parent, removed by this resolution"))
        .collect()
}

/// Compute the both-parents deletion tripwire for a forward-port / merge
/// resolution.
///
/// - `head_before` — the PR head **before** the resolution (parent 1).
/// - `base_sha` — the moved base (`main`) at conflict-detection time; it has
///   absorbed the merged sibling's additions (parent 2).
/// - `head_after` — the resolved PR head.
///
/// Uses two GitHub compare calls with three-dot (merge-base) semantics:
/// 1. `compare/{head_before}...{base_sha}` → files the base gained since the
///    fork = what a merged parent added.
/// 2. `compare/{base_sha}...{head_after}` → files the resolution dropped from
///    the base (rename/move-aware).
///
/// The tripwire is their intersection: merged-parent-added files the resolution
/// removed. Returns rendered description lines; empty when clean.
///
/// **Fail-open:** any `gh` error returns an empty set (logged). A transient
/// GitHub failure must not false-halt a legitimate resolution; the reviewer
/// rubric is the backstop. This is a deliberate, operator-visible trade-off:
/// availability over a hard-closed gate on infra flakiness.
pub async fn compute_merged_parent_deletions(
    repo_slug: &str,
    head_before: &str,
    base_sha: &str,
    head_after: &str,
) -> Vec<String> {
    if head_before.is_empty() || base_sha.is_empty() || head_after.is_empty() {
        return Vec::new();
    }

    let added = match fetch_compare_files(repo_slug, head_before, base_sha).await {
        Ok(files) => added_filenames(&files),
        Err(err) => {
            tracing::warn!(
                repo_slug,
                head_before,
                base_sha,
                error = %format!("{err:#}"),
                "merge_parent_deletion: compare(head_before...base) failed; \
                 tripwire fails open (no gate) for this pass",
            );
            return Vec::new();
        }
    };
    if added.is_empty() {
        return Vec::new();
    }

    let removed = match fetch_compare_files(repo_slug, base_sha, head_after).await {
        Ok(files) => removed_filenames(&files),
        Err(err) => {
            tracing::warn!(
                repo_slug,
                base_sha,
                head_after,
                error = %format!("{err:#}"),
                "merge_parent_deletion: compare(base...head_after) failed; \
                 tripwire fails open (no gate) for this pass",
            );
            return Vec::new();
        }
    };

    describe_deletions(&merged_parent_deletions(&added, &removed))
}

/// Fetch and parse the `.files[]` array of a GitHub compare between two refs.
async fn fetch_compare_files(repo_slug: &str, base: &str, head: &str) -> Result<Vec<CompareFile>> {
    let endpoint = format!("repos/{repo_slug}/compare/{base}...{head}");
    let stdout = crate::gh_invocation::run_gh(
        &[
            "api",
            &endpoint,
            "-H",
            "Accept: application/vnd.github+json",
            "--jq",
            ".files // []",
        ],
        &format!("gh api {endpoint}"),
    )
    .await?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let files: Vec<CompareFile> = serde_json::from_str(trimmed)?;
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cf(filename: &str, status: &str, prev: Option<&str>) -> CompareFile {
        CompareFile {
            filename: filename.to_owned(),
            status: status.to_owned(),
            previous_filename: prev.map(str::to_owned),
        }
    }

    #[test]
    fn incident_002_deletion_is_flagged() {
        // The base gained RecommendationBadge.tsx (the merged sibling); the
        // resolution removed it.
        let added = added_filenames(&[
            cf("components/RecommendationBadge.tsx", "added", None),
            cf("backend/recommendations.ts", "modified", None),
        ]);
        let removed = removed_filenames(&[
            cf("components/RecommendationBadge.tsx", "removed", None),
            cf("components/PlanPageV2.tsx", "modified", None),
        ]);
        let deletions = merged_parent_deletions(&added, &removed);
        assert_eq!(deletions, vec!["components/RecommendationBadge.tsx".to_owned()]);
    }

    #[test]
    fn genuine_rename_is_not_flagged() {
        // The base added foo.rs; the resolution *renamed* it to bar.rs — a
        // move, not a deletion. GitHub reports the new path as `renamed` with
        // previous_filename=foo.rs and does not emit a `removed foo.rs`; but
        // even if it did (aggressive threshold), we exclude moved-from paths.
        let added = added_filenames(&[cf("src/foo.rs", "added", None)]);
        let removed = removed_filenames(&[
            cf("src/bar.rs", "renamed", Some("src/foo.rs")),
            cf("src/foo.rs", "removed", None), // belt-and-suspenders: still excluded
        ]);
        assert!(
            merged_parent_deletions(&added, &removed).is_empty(),
            "a renamed/moved file must not trip the tripwire",
        );
    }

    #[test]
    fn preserved_surface_is_not_flagged() {
        let added = added_filenames(&[cf("a.tsx", "added", None)]);
        // Resolution modified a.tsx but did not remove it.
        let removed = removed_filenames(&[cf("a.tsx", "modified", None)]);
        assert!(merged_parent_deletions(&added, &removed).is_empty());
    }

    #[test]
    fn removal_of_non_merged_parent_file_is_not_flagged() {
        // The resolution removed z.tsx, but z.tsx was NOT something a merged
        // parent added (it's not in `added`), so it is out of scope for this
        // tripwire (the PR's own churn, not merged-parent loss).
        let added = added_filenames(&[cf("a.tsx", "added", None)]);
        let removed = removed_filenames(&[cf("z.tsx", "removed", None)]);
        assert!(merged_parent_deletions(&added, &removed).is_empty());
    }

    #[test]
    fn multiple_deletions_sorted() {
        let added = added_filenames(&[
            cf("b.tsx", "added", None),
            cf("a.tsx", "added", None),
            cf("c.tsx", "added", None),
        ]);
        let removed = removed_filenames(&[cf("a.tsx", "removed", None), cf("b.tsx", "removed", None)]);
        assert_eq!(
            merged_parent_deletions(&added, &removed),
            vec!["a.tsx".to_owned(), "b.tsx".to_owned()],
        );
    }

    #[test]
    fn describe_deletions_is_readable() {
        let lines = describe_deletions(&["components/RecommendationBadge.tsx".to_owned()]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("RecommendationBadge.tsx"));
        assert!(lines[0].contains("merged parent"));
    }

    #[test]
    fn compare_file_parses_github_shape() {
        let json = r#"[
            {"filename":"a.tsx","status":"added"},
            {"filename":"b.tsx","status":"renamed","previous_filename":"old_b.tsx"}
        ]"#;
        let files: Vec<CompareFile> = serde_json::from_str(json).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(added_filenames(&files), BTreeSet::from(["a.tsx".to_owned()]));
        assert_eq!(files[1].previous_filename.as_deref(), Some("old_b.tsx"));
    }
}
