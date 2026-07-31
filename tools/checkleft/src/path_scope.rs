//! Framework-level path scope: the single positive/negative matcher core that
//! decides both "is this file a target of this check" and "is this file
//! excluded from this check".
//!
//! [`PathScope`] generalises the exclude-only matcher that predated it into
//! one value carrying an optional positive globset alongside the existing
//! negative one. It is the single enforcement point applied at every
//! selection site — built-in, component, and declarative — plus the
//! finding-stage backstop, so there is exactly one place that answers "does
//! this file match this check's effective scope".

use std::path::Path;

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::input::ChangeSet;

/// A compiled glob-matcher for repo-root-relative path scoping: an optional
/// positive (include) set and an optional negative (exclude) set.
///
/// Built once per check instance from:
/// - the check-entry `applies_to` patterns (framework include side), and
/// - the union of the accumulated global exclude patterns and the per-check
///   exclude patterns.
///
/// `include: None` means universal (every path is in scope); `exclude: None`
/// means nothing is excluded. A [`Default`] instance is universal-include,
/// empty-exclude — the same no-op behavior the predecessor `PathScope`
/// had for excludes alone.
#[derive(Debug, Clone, Default)]
pub struct PathScope {
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
}

fn build_globset(patterns: &[String], field_label: &str) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).with_context(|| format!("invalid {field_label} glob pattern `{pattern}`"))?;
        builder.add(glob);
    }
    let globset = builder
        .build()
        .with_context(|| format!("failed to compile {field_label} patterns into a GlobSet"))?;
    Ok(Some(globset))
}

impl PathScope {
    /// Build a scope from repo-root-relative include and exclude glob patterns.
    ///
    /// An empty `include` slice means universal (every path is in scope); an
    /// empty `exclude` slice means nothing is excluded. Returns an error if any
    /// pattern is invalid globset syntax.
    pub fn new(include_patterns: &[String], exclude_patterns: &[String]) -> Result<Self> {
        Ok(Self {
            include: build_globset(include_patterns, "applies_to")?,
            exclude: build_globset(exclude_patterns, "exclude")?,
        })
    }

    /// Build a scope with only a negative (exclude) side — the shape every call
    /// site used exclusively before the include side existed.
    pub fn exclude_only(patterns: &[String]) -> Result<Self> {
        Self::new(&[], patterns)
    }

    /// Returns `true` if the repo-root-relative `path` is within this scope's
    /// positive selection (universal when no `include` patterns were given).
    pub fn is_included(&self, path: &Path) -> bool {
        match &self.include {
            Some(globset) => globset.is_match(path),
            None => true,
        }
    }

    /// Returns `true` if the repo-root-relative `path` is matched by any exclude pattern.
    pub fn is_excluded(&self, path: &Path) -> bool {
        match &self.exclude {
            Some(globset) => globset.is_match(path),
            None => false,
        }
    }

    /// Returns `true` if `path` is in scope: included (or no include patterns)
    /// and not excluded. Excludes always win over includes.
    pub fn is_in_scope(&self, path: &Path) -> bool {
        self.is_included(path) && !self.is_excluded(path)
    }

    /// Filter `paths` to those that are in scope. Returns a `Vec` of references
    /// into the original slice.
    pub fn filter_paths<'a>(&self, paths: &'a [std::path::PathBuf]) -> Vec<&'a std::path::PathBuf> {
        paths.iter().filter(|p| self.is_in_scope(p.as_path())).collect()
    }

    /// Return a copy of `changeset` with every out-of-scope changed file removed:
    /// the positive `include` filter is applied first, then the negative
    /// `exclude` filter subtracts from what remains — excludes always win.
    ///
    /// This is the host's selection-time subtraction for programmatic / component
    /// checks: lowering the *filtered* changeset means a guest (or a built-in Rust
    /// check) never sees an out-of-scope path and so cannot target it. The
    /// per-file `file_line_deltas` / `file_diffs` entries for dropped paths are
    /// pruned too so the lowered view stays internally consistent. A universal,
    /// empty-exclude scope returns an equivalent changeset unchanged.
    pub fn filter_changeset(&self, changeset: &ChangeSet) -> ChangeSet {
        if self.is_empty() {
            return changeset.clone();
        }
        let mut filtered = changeset.clone();
        filtered
            .changed_files
            .retain(|file| self.is_in_scope(file.path.as_path()));
        filtered
            .file_line_deltas
            .retain(|path, _| self.is_in_scope(path.as_path()));
        filtered.file_diffs.retain(|path, _| self.is_in_scope(path.as_path()));
        filtered
    }

    /// Returns `true` if this scope is universal-include and empty-exclude — a
    /// pure no-op that keeps everything.
    pub fn is_empty(&self) -> bool {
        self.include.is_none() && self.exclude.is_none()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn empty_patterns_excludes_nothing_and_includes_everything() {
        let m = PathScope::new(&[], &[]).unwrap();
        assert!(!m.is_excluded(Path::new("src/lib.rs")));
        assert!(!m.is_excluded(Path::new("vendor/dep/foo.rs")));
        assert!(m.is_included(Path::new("anything.txt")));
        assert!(m.is_empty());
    }

    #[test]
    fn default_excludes_nothing_and_includes_everything() {
        let m = PathScope::default();
        assert!(!m.is_excluded(Path::new("src/lib.rs")));
        assert!(m.is_included(Path::new("src/lib.rs")));
        assert!(m.is_empty());
    }

    #[test]
    fn exact_path_pattern_matches_only_that_path() {
        let m = PathScope::exclude_only(&["Cargo.lock".to_owned()]).unwrap();
        assert!(m.is_excluded(Path::new("Cargo.lock")));
        assert!(!m.is_excluded(Path::new("sub/Cargo.lock")));
        assert!(!m.is_excluded(Path::new("src/main.rs")));
    }

    #[test]
    fn double_star_glob_crosses_directories() {
        let m = PathScope::exclude_only(&["vendor/**".to_owned()]).unwrap();
        assert!(m.is_excluded(Path::new("vendor/dep/lib.rs")));
        assert!(m.is_excluded(Path::new("vendor/a/b/c/d.rs")));
        assert!(!m.is_excluded(Path::new("src/lib.rs")));
    }

    #[test]
    fn glob_extension_wildcard() {
        let m = PathScope::exclude_only(&["**/*.generated.ts".to_owned()]).unwrap();
        assert!(m.is_excluded(Path::new("frontend/api/client.generated.ts")));
        assert!(m.is_excluded(Path::new("client.generated.ts")));
        assert!(!m.is_excluded(Path::new("frontend/api/client.ts")));
    }

    #[test]
    fn multiple_patterns_are_unioned() {
        let m = PathScope::exclude_only(&["Cargo.lock".to_owned(), "MODULE.bazel.lock".to_owned()]).unwrap();
        assert!(m.is_excluded(Path::new("Cargo.lock")));
        assert!(m.is_excluded(Path::new("MODULE.bazel.lock")));
        assert!(!m.is_excluded(Path::new("src/lib.rs")));
    }

    #[test]
    fn subdirectory_scoped_pattern_does_not_match_sibling() {
        // backend/tests/** should exclude backend/tests/foo.rs but not tests/foo.rs
        let m = PathScope::exclude_only(&["backend/tests/**".to_owned()]).unwrap();
        assert!(m.is_excluded(Path::new("backend/tests/foo.rs")));
        assert!(!m.is_excluded(Path::new("tests/foo.rs")));
        assert!(!m.is_excluded(Path::new("other/backend/tests/foo.rs")));
    }

    #[test]
    fn filter_paths_removes_excluded_entries() {
        let m = PathScope::exclude_only(&["**/*.lock".to_owned()]).unwrap();
        let paths = vec![
            PathBuf::from("Cargo.lock"),
            PathBuf::from("src/lib.rs"),
            PathBuf::from("MODULE.bazel.lock"),
            PathBuf::from("src/main.rs"),
        ];
        let kept: Vec<_> = m.filter_paths(&paths).into_iter().cloned().collect();
        assert_eq!(kept, vec![PathBuf::from("src/lib.rs"), PathBuf::from("src/main.rs")]);
    }

    #[test]
    fn filter_paths_on_empty_scope_retains_all() {
        let m = PathScope::default();
        let paths = vec![PathBuf::from("Cargo.lock"), PathBuf::from("src/lib.rs")];
        let kept: Vec<_> = m.filter_paths(&paths).into_iter().cloned().collect();
        assert_eq!(kept, paths);
    }

    #[test]
    fn invalid_glob_returns_error() {
        let result = PathScope::exclude_only(&["[invalid".to_owned()]);
        assert!(result.is_err(), "expected error for invalid glob, got Ok");
    }

    #[test]
    fn filter_changeset_drops_excluded_files_and_prunes_diffs() {
        use crate::input::{ChangeKind, ChangedFile, FileDiff, FileLineDelta};

        let m = PathScope::exclude_only(&["vendor/**".to_owned()]).unwrap();
        let changeset = ChangeSet::new(vec![
            ChangedFile {
                path: PathBuf::from("src/lib.rs"),
                kind: ChangeKind::Modified,
                old_path: None,
            },
            ChangedFile {
                path: PathBuf::from("vendor/dep/lib.rs"),
                kind: ChangeKind::Modified,
                old_path: None,
            },
        ])
        .with_file_line_delta(PathBuf::from("vendor/dep/lib.rs"), FileLineDelta::default())
        .with_file_diff(PathBuf::from("vendor/dep/lib.rs"), FileDiff::default());

        let filtered = m.filter_changeset(&changeset);

        assert_eq!(
            filtered
                .changed_files
                .iter()
                .map(|f| f.path.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("src/lib.rs")]
        );
        assert!(filtered.file_line_deltas.is_empty(), "excluded delta should be pruned");
        assert!(filtered.file_diffs.is_empty(), "excluded diff should be pruned");
    }

    #[test]
    fn filter_changeset_on_empty_scope_retains_everything() {
        use crate::input::{ChangeKind, ChangedFile};

        let m = PathScope::default();
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("vendor/dep/lib.rs"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]);
        let filtered = m.filter_changeset(&changeset);
        assert_eq!(filtered.changed_files.len(), 1);
    }

    // ── include side ─────────────────────────────────────────────────────────

    #[test]
    fn include_patterns_restrict_to_matching_paths() {
        let m = PathScope::new(&["**/*.rs".to_owned()], &[]).unwrap();
        assert!(m.is_included(Path::new("src/lib.rs")));
        assert!(!m.is_included(Path::new("src/lib.ts")));
    }

    #[test]
    fn exclude_wins_over_include() {
        let m = PathScope::new(&["src/**".to_owned()], &["**/generated/**".to_owned()]).unwrap();
        assert!(m.is_in_scope(Path::new("src/keep.rs")));
        assert!(!m.is_in_scope(Path::new("src/generated/out.rs")));
    }

    #[test]
    fn filter_changeset_applies_include_then_exclude() {
        use crate::input::{ChangeKind, ChangedFile};

        let m = PathScope::new(&["src/**".to_owned()], &["**/generated/**".to_owned()]).unwrap();
        let changeset = ChangeSet::new(vec![
            ChangedFile {
                path: PathBuf::from("src/keep.rs"),
                kind: ChangeKind::Modified,
                old_path: None,
            },
            ChangedFile {
                path: PathBuf::from("src/generated/out.rs"),
                kind: ChangeKind::Modified,
                old_path: None,
            },
            ChangedFile {
                path: PathBuf::from("docs/readme.md"),
                kind: ChangeKind::Modified,
                old_path: None,
            },
        ]);

        let filtered = m.filter_changeset(&changeset);

        assert_eq!(
            filtered
                .changed_files
                .iter()
                .map(|f| f.path.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("src/keep.rs")]
        );
    }

    #[test]
    fn empty_include_slice_is_universal() {
        let m = PathScope::new(&[], &[]).unwrap();
        assert!(m.is_included(Path::new("anything/at/all.ext")));
    }
}
