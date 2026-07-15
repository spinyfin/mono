use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::exclusion::{DeclaredExclusion, ExclusionStatus};
use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
use crate::output::{CheckResult, Finding};

#[async_trait]
pub trait ConfiguredCheck: Send + Sync {
    /// Run the check, emitting incremental per-file progress ticks via
    /// `on_file_processed`. The argument is the cumulative count of eligible
    /// files processed so far. Call once per eligible file (i.e. files that
    /// would be counted by [`Self::applicable_file_count`]).
    ///
    /// A check that does not iterate files may ignore `on_file_processed`
    /// entirely; the progress UI then reports it as a single unit of work.
    async fn run_with_progress(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        on_file_processed: Arc<dyn Fn(usize) + Send + Sync>,
    ) -> Result<CheckResult>;

    /// Run the check without progress reporting.
    ///
    /// The default discards the per-file ticks [`Self::run_with_progress`]
    /// emits, which is what every non-interactive caller wants.
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        self.run_with_progress(changeset, tree, Arc::new(|_| {})).await
    }

    /// Count the files in `changeset` that this check will actually process.
    ///
    /// The default returns the full changeset size, which is correct for checks that
    /// iterate over every file. Override in checks that filter by extension or change
    /// kind so the progress UI reports the accurate eligible count.
    fn applicable_file_count(&self, changeset: &ChangeSet) -> usize {
        changeset.changed_files.len()
    }

    /// Exclusions this configured check honors that are eligible for stale-exclusion
    /// auditing (see [`crate::exclusion`]). Each carries the inputs it depends on;
    /// checkleft re-evaluates an exclusion only when one of those inputs changes in the
    /// diff. The default returns none, so a check opts into auditing simply by
    /// overriding this.
    fn declared_exclusions(&self) -> Vec<DeclaredExclusion> {
        Vec::new()
    }

    /// Re-evaluate a single declared exclusion as if it were not configured, to decide
    /// whether it is still load-bearing. The runner only calls this for exclusions whose
    /// declared dependencies intersect the changeset.
    ///
    /// Implementations must fail safe: when staleness cannot be proven (file unreadable,
    /// ambiguous target, entry not recognized), return [`ExclusionStatus::Unknown`]
    /// rather than guessing [`ExclusionStatus::Stale`]. The default returns `Unknown`.
    async fn evaluate_exclusion(
        &self,
        _exclusion: &DeclaredExclusion,
        _tree: &dyn SourceTree,
    ) -> Result<ExclusionStatus> {
        Ok(ExclusionStatus::Unknown)
    }
}

/// Count the non-deleted files in `changeset` that `predicate` accepts.
///
/// This is the [`ConfiguredCheck::applicable_file_count`] shape for checks that scan
/// changed files by path. Pass the same `predicate` here and to [`run_per_text_file`] so
/// the reported denominator always matches the number of progress ticks the run emits.
pub fn count_applicable(changeset: &ChangeSet, predicate: impl Fn(&Path) -> bool) -> usize {
    changeset
        .changed_files
        .iter()
        .filter(|f| !matches!(f.kind, ChangeKind::Deleted) && predicate(&f.path))
        .count()
}

/// Read every non-deleted changed file matching `predicate` and hand its decoded text to
/// `per_file`, which pushes any [`Finding`]s it produces.
///
/// A file whose contents cannot be read, or which is not valid UTF-8, is skipped silently:
/// `per_file` never sees it, but it still counts toward progress. Every file the predicate
/// accepts ticks `on_file_processed` exactly once, with the cumulative count — so the final
/// tick equals [`count_applicable`] over the same predicate.
pub fn run_per_text_file(
    changeset: &ChangeSet,
    tree: &dyn SourceTree,
    predicate: impl Fn(&Path) -> bool,
    on_file_processed: &dyn Fn(usize),
    mut per_file: impl FnMut(&ChangedFile, &str, &mut Vec<Finding>),
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut processed = 0usize;

    for changed_file in &changeset.changed_files {
        if matches!(changed_file.kind, ChangeKind::Deleted) || !predicate(&changed_file.path) {
            continue;
        }

        if let Ok(contents) = tree.read_file(&changed_file.path)
            && let Ok(contents) = std::str::from_utf8(&contents)
        {
            per_file(changed_file, contents, &mut findings);
        }

        processed += 1;
        on_file_processed(processed);
    }

    findings
}

#[async_trait]
pub trait Check: Send + Sync {
    fn id(&self) -> &str;

    fn description(&self) -> &str;

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>>;

    /// Like `configure`, but also passes the CHECKS file directory (repo-root-relative).
    /// Checks that need to scope exclusions to the config subtree should override this.
    /// The default delegates to `configure`, ignoring the scope.
    fn configure_scoped(&self, config: &toml::Value, _config_dir: Option<&Path>) -> Result<Arc<dyn ConfiguredCheck>> {
        self.configure(config)
    }

    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree, config: &toml::Value) -> Result<CheckResult> {
        self.configure(config)?.run(changeset, tree).await
    }
}

#[derive(Default)]
pub struct CheckRegistry {
    checks: BTreeMap<String, Arc<dyn Check>>,
}

impl CheckRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<C>(&mut self, check: C) -> Result<()>
    where
        C: Check + 'static,
    {
        self.register_arc(Arc::new(check))
    }

    pub fn register_arc(&mut self, check: Arc<dyn Check>) -> Result<()> {
        let id = check.id().to_owned();
        if self.checks.contains_key(&id) {
            bail!("check already registered: {id}");
        }
        self.checks.insert(id, check);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Check>> {
        self.checks.get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<dyn Check>> {
        self.checks.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    use anyhow::{Result, anyhow, bail};

    use super::{count_applicable, run_per_text_file};
    use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree};
    use crate::output::{Finding, Severity};

    /// Serves `contents` verbatim per path. A path mapped to `Err` reads as unreadable;
    /// an unmapped path is never requested by these tests.
    struct StubTree {
        contents: Vec<(&'static str, Result<Vec<u8>>)>,
    }

    impl SourceTree for StubTree {
        fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
            match self.contents.iter().find(|(p, _)| Path::new(p) == path) {
                Some((_, Ok(bytes))) => Ok(bytes.clone()),
                Some((_, Err(error))) => bail!("{error}"),
                None => bail!("unexpected read of {}", path.display()),
            }
        }

        fn exists(&self, _path: &Path) -> bool {
            unimplemented!("not exercised by these tests")
        }

        fn list_dir(&self, _path: &Path) -> Result<Vec<PathBuf>> {
            unimplemented!("not exercised by these tests")
        }

        fn glob(&self, _pattern: &str) -> Result<Vec<PathBuf>> {
            unimplemented!("not exercised by these tests")
        }
    }

    fn changed(path: &str, kind: ChangeKind) -> ChangedFile {
        ChangedFile {
            path: PathBuf::from(path),
            kind,
            old_path: None,
        }
    }

    fn finding(message: &str) -> Finding {
        Finding {
            fixable: false,
            severity: Severity::Error,
            message: message.to_owned(),
            location: None,
            remediations: Vec::new(),
            suggested_fix: None,
        }
    }

    #[test]
    fn skips_deleted_and_unmatched_files_without_counting_them() {
        let changeset = ChangeSet::new(vec![
            changed("keep.txt", ChangeKind::Modified),
            changed("keep.skip", ChangeKind::Modified),
            changed("gone.txt", ChangeKind::Deleted),
        ]);
        let tree = StubTree {
            contents: vec![("keep.txt", Ok(b"body".to_vec()))],
        };

        let predicate = |path: &Path| path.extension().is_some_and(|ext| ext == "txt");
        let ticks = RefCell::new(Vec::new());
        let seen = RefCell::new(Vec::new());

        let findings = run_per_text_file(
            &changeset,
            &tree,
            predicate,
            &|n| ticks.borrow_mut().push(n),
            |changed_file, contents, findings| {
                seen.borrow_mut().push((changed_file.path.clone(), contents.to_owned()));
                findings.push(finding("hit"));
            },
        );

        assert_eq!(seen.into_inner(), vec![(PathBuf::from("keep.txt"), "body".to_owned())]);
        assert_eq!(findings.len(), 1);
        assert_eq!(ticks.into_inner(), vec![1]);
        assert_eq!(count_applicable(&changeset, predicate), 1);
    }

    #[test]
    fn counts_unreadable_and_non_utf8_files_without_invoking_the_callback() {
        let changeset = ChangeSet::new(vec![
            changed("unreadable.txt", ChangeKind::Modified),
            changed("binary.txt", ChangeKind::Modified),
            changed("text.txt", ChangeKind::Added),
        ]);
        let tree = StubTree {
            contents: vec![
                ("unreadable.txt", Err(anyhow!("permission denied"))),
                // A lone 0xff byte is never valid UTF-8.
                ("binary.txt", Ok(vec![0xff, 0xfe])),
                ("text.txt", Ok(b"body".to_vec())),
            ],
        };

        let ticks = RefCell::new(Vec::new());
        let seen = RefCell::new(Vec::new());

        let findings = run_per_text_file(
            &changeset,
            &tree,
            |_| true,
            &|n| ticks.borrow_mut().push(n),
            |changed_file, _contents, findings| {
                seen.borrow_mut().push(changed_file.path.clone());
                findings.push(finding("hit"));
            },
        );

        assert_eq!(
            seen.into_inner(),
            vec![PathBuf::from("text.txt")],
            "unreadable and non-UTF-8 files must be skipped silently"
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(
            ticks.into_inner(),
            vec![1, 2, 3],
            "every applicable file ticks once, even when it cannot be decoded"
        );
        assert_eq!(count_applicable(&changeset, |_| true), 3);
    }
}
