use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::bypass::parse_bypass_directives_from_descriptions;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChangeSet {
    pub changed_files: Vec<ChangedFile>,
    #[serde(default)]
    pub file_line_deltas: HashMap<PathBuf, FileLineDelta>,
    #[serde(default)]
    pub file_diffs: HashMap<PathBuf, FileDiff>,
    /// Tip commit message only (`HEAD` / `jj @`).
    ///
    /// This is the surface scanned by leakage checks such as
    /// `text/forbidden-pattern` with `surfaces: [changeset]`. It deliberately
    /// does **not** include intermediate historical commit messages from the
    /// pushed range — those are re-emitted by GitHub squash/MQ templates and
    /// would otherwise false-fail green PR branches when they re-enter MQ.
    #[serde(default)]
    pub commit_description: Option<String>,
    /// Concatenated commit messages for the full `base..HEAD` range.
    ///
    /// Host-only: used by BYPASS directive parsing so a directive placed in any
    /// content commit remains visible (including under an empty jj working-copy
    /// tip). Not lowered to external/wasm checks and not scanned by text
    /// leakage checks — see [`Self::commit_description`] for that surface.
    #[serde(default)]
    pub bypass_commit_descriptions: Option<String>,
    #[serde(default)]
    pub pr_description: Option<String>,
    /// When set, a PR was identified (or the environment indicated one) but
    /// its description could not be retrieved. Mutually exclusive with a
    /// present [`Self::pr_description`].
    ///
    /// **Bypass parsing ignores this field** and continues to use only
    /// [`Self::pr_description`] text (plus commit messages). An unresolved
    /// description therefore neither grants nor revokes a bypass — the same
    /// as today's `pr_description: None` behaviour. Checks that *scan* the PR
    /// description as a subject (e.g. `text/forbidden-pattern` with
    /// `surfaces: [changeset]`) must treat a present reason as a hard failure.
    #[serde(default)]
    pub pr_description_unavailable_reason: Option<String>,
    #[serde(default)]
    pub change_id: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    /// True when this changeset is the full tracked-tree scan produced by
    /// `checkleft run --all` / [`crate::vcs::Vcs::all_files_changeset`].
    ///
    /// In that mode every tracked file is listed as `ChangeKind::Modified`
    /// with no diff hunks — it is not a real PR-sized change. Checks that
    /// gate on change *size* (e.g. `change/file-count`) must treat this as a
    /// no-op; integrity pipelines use `--all` and would otherwise always fail.
    #[serde(default)]
    pub whole_repo: bool,
}

impl ChangeSet {
    pub fn new(changed_files: Vec<ChangedFile>) -> Self {
        Self {
            changed_files,
            file_line_deltas: HashMap::new(),
            file_diffs: HashMap::new(),
            commit_description: None,
            bypass_commit_descriptions: None,
            pr_description: None,
            pr_description_unavailable_reason: None,
            change_id: None,
            repository: None,
            whole_repo: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.changed_files.is_empty()
    }

    pub fn with_commit_description(mut self, commit_description: Option<String>) -> Self {
        self.commit_description = commit_description;
        self
    }

    pub fn with_bypass_commit_descriptions(mut self, bypass_commit_descriptions: Option<String>) -> Self {
        self.bypass_commit_descriptions = bypass_commit_descriptions;
        self
    }

    pub fn with_pr_description(mut self, pr_description: Option<String>) -> Self {
        self.pr_description = pr_description;
        self
    }

    pub fn with_pr_description_unavailable_reason(mut self, reason: Option<String>) -> Self {
        self.pr_description_unavailable_reason = reason;
        self
    }

    pub fn with_change_id(mut self, change_id: Option<String>) -> Self {
        self.change_id = change_id;
        self
    }

    pub fn with_repository(mut self, repository: Option<String>) -> Self {
        self.repository = repository;
        self
    }

    /// Mark this changeset as a whole-repo (`--all`) scan rather than a
    /// scoped PR/diff change. See [`Self::whole_repo`].
    pub fn with_whole_repo(mut self, whole_repo: bool) -> Self {
        self.whole_repo = whole_repo;
        self
    }

    pub fn with_file_line_delta(mut self, path: PathBuf, delta: FileLineDelta) -> Self {
        self.file_line_deltas.insert(path, delta);
        self
    }

    pub fn with_file_diff(mut self, path: PathBuf, diff: FileDiff) -> Self {
        self.file_line_deltas.insert(path.clone(), diff.line_delta());
        self.file_diffs.insert(path, diff);
        self
    }

    /// The precise added-line ranges for `path`, or `None` when no diff data
    /// exists for it in this changeset (e.g. `--all` mode, which carries no
    /// hunk data at all). Callers should treat `None` as "no line restriction
    /// available" rather than "no lines changed" — an empty-but-present range
    /// list (e.g. a rename with no content change) legitimately means zero
    /// changed lines.
    pub fn changed_lines(&self, path: &Path) -> Option<&[(u32, u32)]> {
        self.file_diffs.get(path).map(|diff| diff.added_line_ranges.as_slice())
    }

    pub fn bypass_reason(&self, bypass_name: &str) -> Option<String> {
        // Prefer the full-range bypass surface so a BYPASS in any content commit
        // is visible; fall back to the tip commit message for tests and local
        // runs that only populate `commit_description`.
        let commit_for_bypass = self
            .bypass_commit_descriptions
            .as_deref()
            .or(self.commit_description.as_deref());
        parse_bypass_directives_from_descriptions(commit_for_bypass, self.pr_description.as_deref())
            .get(bypass_name)
            .cloned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FileLineDelta {
    pub added_lines: usize,
    pub removed_lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FileDiff {
    #[serde(default)]
    pub hunks: Vec<DiffHunk>,
    /// Precise post-image (new-file) line ranges, inclusive on both ends, that
    /// this diff *added*. Unlike a `DiffHunk`'s `new_start..new_start+new_lines`
    /// span (which includes unified-diff context lines), these ranges cover
    /// exactly the `+` lines — built by tracking the post-image line counter
    /// while walking the patch. Used by `ChangeSet::changed_lines` to filter
    /// findings down to PR-changed lines.
    #[serde(default)]
    pub added_line_ranges: Vec<(u32, u32)>,
}

impl FileDiff {
    pub fn line_delta(&self) -> FileLineDelta {
        let mut delta = FileLineDelta::default();
        for hunk in &self.hunks {
            delta.added_lines = delta.added_lines.saturating_add(hunk.added_lines);
            delta.removed_lines = delta.removed_lines.saturating_add(hunk.removed_lines);
        }
        delta
    }

    /// Whether post-image line `line` (1-based) falls inside one of this diff's
    /// added-line ranges.
    pub fn contains_added_line(&self, line: u32) -> bool {
        self.added_line_ranges
            .iter()
            .any(|&(start, end)| line >= start && line <= end)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub added_lines: usize,
    pub removed_lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub old_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeVersion {
    Current,
    Base,
}

pub trait SourceTree: Send + Sync {
    fn read_file(&self, path: &Path) -> Result<Vec<u8>>;

    fn read_file_versioned(&self, path: &Path, version: TreeVersion) -> Result<Vec<u8>> {
        match version {
            TreeVersion::Current => self.read_file(path),
            TreeVersion::Base => bail!("base revision reads are not supported by this source tree"),
        }
    }

    fn exists(&self, path: &Path) -> bool;

    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;

    fn glob(&self, pattern: &str) -> Result<Vec<PathBuf>>;
}

#[cfg(test)]
mod tests {
    use super::{ChangeKind, ChangeSet, ChangedFile};

    #[test]
    fn bypass_reason_uses_commit_description() {
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: "backend/blob/src/v3/auth.rs".into(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some(
            "BYPASS_API_BREAKING_SURFACE=Legitimate exception in commit.".to_owned(),
        ));

        assert_eq!(
            changeset.bypass_reason("BYPASS_API_BREAKING_SURFACE"),
            Some("Legitimate exception in commit.".to_owned())
        );
    }

    #[test]
    fn bypass_reason_prefers_pr_description_over_commit_description() {
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: "backend/blob/src/v3/auth.rs".into(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some("BYPASS_API_BREAKING_SURFACE=From commit.".to_owned()))
        .with_pr_description(Some("BYPASS_API_BREAKING_SURFACE=From PR.".to_owned()));

        assert_eq!(
            changeset.bypass_reason("BYPASS_API_BREAKING_SURFACE"),
            Some("From PR.".to_owned())
        );
    }

    /// Unavailable PR description must not grant or revoke bypass: only the
    /// resolved body text (and commit messages) feed directive parsing.
    #[test]
    fn bypass_reason_ignores_pr_description_unavailable_reason() {
        let with_commit_only = ChangeSet::new(vec![ChangedFile {
            path: "backend/blob/src/v3/auth.rs".into(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some("BYPASS_API_BREAKING_SURFACE=Still valid from commit.".to_owned()))
        .with_pr_description_unavailable_reason(Some(
            "GitHub API did not return a description for PR 1 in o/r".to_owned(),
        ));

        assert_eq!(
            with_commit_only.bypass_reason("BYPASS_API_BREAKING_SURFACE"),
            Some("Still valid from commit.".to_owned()),
            "unavailable PR body must not revoke a commit-message bypass"
        );

        let no_directive = ChangeSet::new(vec![ChangedFile {
            path: "backend/blob/src/v3/auth.rs".into(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some("ordinary commit".to_owned()))
        .with_pr_description_unavailable_reason(Some(
            "GitHub API did not return a description for PR 1 in o/r".to_owned(),
        ));

        assert_eq!(
            no_directive.bypass_reason("BYPASS_API_BREAKING_SURFACE"),
            None,
            "unavailable PR body must not invent a bypass"
        );
    }

    /// Regression: BYPASS lives on a non-tip historical commit while the tip
    /// description (the leakage surface) is clean / empty.
    #[test]
    fn bypass_reason_reads_full_range_when_tip_commit_description_lacks_directive() {
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: "backend/blob/src/v3/auth.rs".into(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some("wip: empty working-copy tip".to_owned()))
        .with_bypass_commit_descriptions(Some(
            "feat: add large file\n\nBYPASS_FILE_SIZE=Intentionally large; one-off exception.\n\nwip: empty working-copy tip"
                .to_owned(),
        ));

        assert_eq!(
            changeset.bypass_reason("BYPASS_FILE_SIZE"),
            Some("Intentionally large; one-off exception.".to_owned())
        );
        // Tip surface stays free of the bypass text's sibling noise for leakage
        // scanners that only read `commit_description`.
        assert_eq!(
            changeset.commit_description.as_deref(),
            Some("wip: empty working-copy tip")
        );
    }

    /// Leakage surface must not inherit historical commit messages that only
    /// appear on the bypass/full-range field.
    #[test]
    fn tip_commit_description_is_independent_of_bypass_range() {
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: "docs/design.md".into(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_commit_description(Some("docs: clean tip message".to_owned()))
        .with_bypass_commit_descriptions(Some(
            "docs: clean tip message\n\nRecord operator refutation\n\nFold ZZ3718 into design".to_owned(),
        ));

        assert_eq!(changeset.commit_description.as_deref(), Some("docs: clean tip message"));
        assert!(
            changeset
                .bypass_commit_descriptions
                .as_deref()
                .is_some_and(|s| s.contains("ZZ3718") && s.contains("operator"))
        );
        // No BYPASS directive in either surface → None.
        assert_eq!(changeset.bypass_reason("BYPASS_ANYTHING"), None);
    }

    #[test]
    fn changeset_metadata_fields_round_trip_through_builders() {
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: "backend/blob/src/v3/auth.rs".into(),
            kind: ChangeKind::Modified,
            old_path: None,
        }])
        .with_change_id(Some("235".to_owned()))
        .with_repository(Some("example/flunge".to_owned()));

        assert_eq!(changeset.change_id.as_deref(), Some("235"));
        assert_eq!(changeset.repository.as_deref(), Some("example/flunge"));
    }
}
