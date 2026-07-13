use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::command::{CommandRunner, RealCommandRunner};
use crate::lockfile::regenerate_lockfile;
use crate::{ConflictClass, ConflictedFile, DeterministicResolver, ResolveOutcome};

/// Regenerates `MODULE.bazel.lock` from the (already merged) sibling
/// `MODULE.bazel` via `bazel mod deps --lockfile_mode=update` — the bazel
/// analogue of [`crate::CargoLockResolver`].
///
/// See rung 0 in
/// `docs/designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`.
/// Real end-to-end regeneration isn't covered by a unit test here: it
/// requires a full bazel workspace (registries, toolchains) to resolve
/// against, which isn't hermetic in a plain temp directory the way
/// `cargo generate-lockfile` is for a zero-dependency crate. The shared
/// regeneration strategy this resolver delegates to is exhaustively
/// tested in `lockfile.rs` and via the fake-runner test below.
pub struct BazelModuleLockResolver {
    runner: Arc<dyn CommandRunner>,
}

impl BazelModuleLockResolver {
    pub fn new() -> Self {
        Self {
            runner: Arc::new(RealCommandRunner),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_runner(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

impl Default for BazelModuleLockResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DeterministicResolver for BazelModuleLockResolver {
    fn class(&self) -> ConflictClass {
        ConflictClass::BazelModuleLock
    }

    fn applies_to(&self, file: &ConflictedFile) -> bool {
        Path::new(&file.path).file_name().and_then(|name| name.to_str()) == Some("MODULE.bazel.lock")
    }

    async fn resolve(&self, workspace_path: &Path, file: &ConflictedFile) -> ResolveOutcome {
        regenerate_lockfile(
            self.runner.as_ref(),
            workspace_path,
            file,
            "MODULE.bazel",
            "bazel",
            &["mod", "deps", "--lockfile_mode=update"],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::FakeCommandRunner;

    fn file(path: &str) -> ConflictedFile {
        ConflictedFile {
            path: path.to_owned(),
            marker_count: Some(1),
            shape: "content".to_owned(),
        }
    }

    #[test]
    fn applies_to_matches_module_bazel_lock_by_basename() {
        let resolver = BazelModuleLockResolver::new();
        assert!(resolver.applies_to(&file("MODULE.bazel.lock")));
        assert!(resolver.applies_to(&file("nested/MODULE.bazel.lock")));
        assert!(!resolver.applies_to(&file("MODULE.bazel")));
        assert!(!resolver.applies_to(&file("Cargo.lock")));
    }

    #[tokio::test]
    async fn resolve_runs_bazel_mod_deps_with_lockfile_mode_update() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MODULE.bazel"), "module(name = \"x\")\n").unwrap();

        let runner = Arc::new(FakeCommandRunner::success());
        let resolver = BazelModuleLockResolver::with_runner(runner.clone());

        let outcome = resolver.resolve(dir.path(), &file("MODULE.bazel.lock")).await;

        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }));
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "bazel");
        assert_eq!(
            calls[0].1,
            vec!["mod".to_owned(), "deps".to_owned(), "--lockfile_mode=update".to_owned()]
        );
    }

    #[tokio::test]
    async fn declines_when_module_bazel_manifest_missing() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(FakeCommandRunner::success());
        let resolver = BazelModuleLockResolver::with_runner(runner);

        let outcome = resolver.resolve(dir.path(), &file("MODULE.bazel.lock")).await;

        assert!(matches!(outcome, ResolveOutcome::Declined { reason } if reason.contains("MODULE.bazel")));
    }
}
