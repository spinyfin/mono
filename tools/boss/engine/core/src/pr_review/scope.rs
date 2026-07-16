//! Reviewer inputs: pre-fetched PR metadata ([`PrReviewContext`]), the review
//! rubric selector ([`ReviewScope`]), and the changed-file classifier that
//! derives the scope from a PR's file list.

/// Authoritative PR metadata fetched from GitHub at reviewer-spawn time.
///
/// Pre-fetched before the reviewer worker starts so the reviewer prompt
/// contains the correct base/head SHAs and changed-file list upfront —
/// orientation context the reviewer receives before touching any files.
/// The reviewer workspace is already checked out to the PR head, so file
/// reads go directly against the working tree.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct PrReviewContext {
    /// GitHub PR number (the integer, e.g. `42` for `.../pull/42`).
    pub pr_number: u64,
    /// Full SHA of the base commit the PR is diffed against.
    pub base_sha: String,
    /// Full SHA of the PR's current HEAD commit. The reviewer workspace is
    /// checked out to this SHA, so `Read`/`cat`/`grep` on workspace files
    /// reflect the PR head directly.
    pub head_sha: String,
    /// Paths of every file changed by the PR, relative to the repo root.
    pub changed_files: Vec<String>,
    /// Full output of `gh pr diff` when the diff fits within the configured
    /// line threshold. `None` when the diff was too large to embed or could
    /// not be fetched. When `Some`, the reviewer prompt embeds it directly
    /// so the reviewer skips the `gh pr diff` tool call.
    pub diff_content: Option<String>,
    /// HEAD SHA this PR was at the end of the most recent completed
    /// reviewer pass, or `None` on a PR's first review (2026-07-01
    /// revision-review experiment). Lets the reviewer prioritise the delta
    /// since that pass — content it already accepted doesn't need
    /// re-litigating — while still reviewing the PR as a whole; see the
    /// prompt's "Reviewing a revision" section, which explicitly overrides
    /// diff-only scoping for whole-PR-state defects (e.g. a duplicated
    /// module tree spread across a diff's unchanged and changed hunks).
    pub last_reviewed_sha: Option<String>,
    /// Deterministic supersession-language hits found in the PR body, commit
    /// messages, or comments (incident-002 P3). Each entry is a rendered
    /// `term — snippet` line. When non-empty the reviewer prompt carries an
    /// authoritative block requiring a verified design-doc citation for each
    /// flagged claim. Populated by the caller (the spawn path scans the PR
    /// narrative); empty here by default.
    #[builder(default)]
    pub supersession_flags: Vec<String>,
    /// Deterministic both-parents deletion-tripwire result (incident-002 P2):
    /// files a merged parent added and this forward-port/merge resolution
    /// removed, rename/move-aware. Each entry is a rendered description line.
    /// When non-empty the reviewer prompt carries an authoritative,
    /// rationale-independent block: each removed surface must be raised as a
    /// gating `regression` finding unless a design-doc citation authorises it.
    /// Populated by the caller for conflict-resolution reviews; empty here by
    /// default.
    #[builder(default)]
    pub merged_parent_deletions: Vec<String>,
}

/// Which review rubric to apply to a PR.
///
/// The reviewer renders a scope-specific initial prompt so the rubric is
/// always appropriate to the PR's content — the code rubric (correctness,
/// regressions, architecture, tests) does not apply to a pure docs delivery.
///
/// Callers should use [`classify_changed_files`] to derive this from the
/// list of files in the PR diff, falling back to [`ReviewScope::Code`] when
/// the file list is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewScope {
    /// Standard code PR — apply the full code rubric.
    Code,
    /// Docs-only PR (every changed file is a documentation file) — apply
    /// the light rubric (structure, completeness, required-sections) and
    /// skip the code rubric entirely.
    DocsOnly,
}

/// Classify a list of changed file paths as docs-only or code.
///
/// Returns [`ReviewScope::DocsOnly`] if every path in `files` is a
/// documentation file (`.md`, `.mdx`, `.rst`, `.txt`, or any path that
/// lives under a `docs/` directory at any depth). Returns
/// [`ReviewScope::Code`] if any path is a source, build, or config file,
/// or if `files` is empty (an empty diff defaults to the code rubric).
///
/// # Examples
///
/// ```
/// use boss_engine::pr_review::{classify_changed_files, ReviewScope};
///
/// assert_eq!(
///     classify_changed_files(&["docs/design.md", "README.md"]),
///     ReviewScope::DocsOnly,
/// );
/// assert_eq!(
///     classify_changed_files(&["src/lib.rs", "docs/design.md"]),
///     ReviewScope::Code,
/// );
/// assert_eq!(classify_changed_files(&[]), ReviewScope::Code);
/// ```
pub fn classify_changed_files(files: &[&str]) -> ReviewScope {
    if files.is_empty() {
        return ReviewScope::Code;
    }
    if files.iter().all(|f| is_docs_file(f)) {
        ReviewScope::DocsOnly
    } else {
        ReviewScope::Code
    }
}

fn is_docs_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".md")
        || lower.ends_with(".mdx")
        || lower.ends_with(".rst")
        || lower.ends_with(".txt")
        || lower.starts_with("docs/")
        || lower.contains("/docs/")
}

#[cfg(test)]
mod tests {
    use crate::pr_review::*;

    #[test]
    fn classify_empty_files_returns_code() {
        assert_eq!(classify_changed_files(&[]), ReviewScope::Code);
    }

    #[test]
    fn classify_all_md_files_returns_docs_only() {
        assert_eq!(
            classify_changed_files(&["README.md", "docs/design.md", "CHANGELOG.md"]),
            ReviewScope::DocsOnly,
        );
    }

    #[test]
    fn classify_mixed_returns_code() {
        assert_eq!(classify_changed_files(&["README.md", "src/lib.rs"]), ReviewScope::Code,);
    }

    #[test]
    fn classify_mdx_and_rst_count_as_docs() {
        assert_eq!(
            classify_changed_files(&["docs/guide.mdx", "notes.rst"]),
            ReviewScope::DocsOnly,
        );
    }

    #[test]
    fn classify_docs_dir_prefix_counts_as_docs() {
        assert_eq!(
            classify_changed_files(&["docs/architecture/overview.txt"]),
            ReviewScope::DocsOnly,
        );
    }

    #[test]
    fn classify_docs_subdir_in_path_counts_as_docs() {
        assert_eq!(
            classify_changed_files(&["tools/boss/docs/designs/foo.md"]),
            ReviewScope::DocsOnly,
        );
    }

    #[test]
    fn classify_rs_file_alone_returns_code() {
        assert_eq!(
            classify_changed_files(&["tools/boss/engine/src/lib.rs"]),
            ReviewScope::Code,
        );
    }

    #[test]
    fn classify_build_file_with_docs_returns_code() {
        assert_eq!(
            classify_changed_files(&["docs/guide.md", "BUILD.bazel"]),
            ReviewScope::Code,
        );
    }
}
