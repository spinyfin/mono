use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::command::{CommandRunner, RealCommandRunner};
use crate::lockfile::regenerate_lockfile;
use crate::{ConflictClass, ConflictedFile, DeterministicResolver, ResolveOutcome};

/// Regenerates `Cargo.lock` from the (already merged) sibling
/// `Cargo.toml` via `cargo generate-lockfile`. Both sides' manifest edits
/// are represented in the regenerated lockfile because it resolves
/// against the merged manifest, not either parent's lockfile.
///
/// See rung 0 in
/// `docs/designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`.
pub struct CargoLockResolver {
    runner: Arc<dyn CommandRunner>,
}

impl CargoLockResolver {
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

impl Default for CargoLockResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DeterministicResolver for CargoLockResolver {
    fn class(&self) -> ConflictClass {
        ConflictClass::CargoLock
    }

    fn applies_to(&self, file: &ConflictedFile) -> bool {
        Path::new(&file.path).file_name().and_then(|name| name.to_str()) == Some("Cargo.lock")
    }

    async fn resolve(&self, workspace_path: &Path, file: &ConflictedFile) -> ResolveOutcome {
        regenerate_lockfile(
            self.runner.as_ref(),
            workspace_path,
            file,
            "Cargo.toml",
            "cargo",
            &["generate-lockfile"],
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
    fn applies_to_matches_cargo_lock_by_basename() {
        let resolver = CargoLockResolver::new();
        assert!(resolver.applies_to(&file("Cargo.lock")));
        assert!(resolver.applies_to(&file("tools/boss/protocol/Cargo.lock")));
        assert!(!resolver.applies_to(&file("Cargo.toml")));
        assert!(!resolver.applies_to(&file("Cargo.lock.bak")));
        assert!(!resolver.applies_to(&file("MODULE.bazel.lock")));
    }

    #[tokio::test]
    async fn resolve_delegates_to_shared_lockfile_regeneration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();

        let runner = Arc::new(FakeCommandRunner::success());
        let resolver = CargoLockResolver::with_runner(runner.clone());

        let outcome = resolver.resolve(dir.path(), &file("Cargo.lock")).await;

        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }));
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "cargo");
        assert_eq!(calls[0].1, vec!["generate-lockfile".to_owned()]);
    }

    /// End-to-end against the real `cargo` binary: a zero-dependency
    /// fixture package needs no network access to regenerate its
    /// lockfile, so this stays hermetic. Skipped if `cargo` isn't on
    /// PATH (mirrors `boss_conflict_diagnosis`'s `which_git` guard).
    #[tokio::test]
    async fn real_cargo_regenerates_a_conflicted_lockfile() {
        if which("cargo").is_none() {
            eprintln!("skipping: cargo not on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("lib.rs"), "").unwrap();
        std::fs::write(
            dir.path().join("Cargo.lock"),
            "<<<<<<< ours\ngarbage\n=======\n>>>>>>> theirs\n",
        )
        .unwrap();

        let resolver = CargoLockResolver::new();
        let outcome = resolver.resolve(dir.path(), &file("Cargo.lock")).await;

        match outcome {
            ResolveOutcome::Resolved { .. } => {
                let regenerated = std::fs::read_to_string(dir.path().join("Cargo.lock")).unwrap();
                assert!(
                    regenerated.contains("fixture"),
                    "regenerated lockfile should reference the package"
                );
                assert!(!regenerated.contains("<<<<<<<"), "conflict markers must be gone");
            }
            ResolveOutcome::Declined { reason } => panic!("expected cargo to regenerate cleanly, declined: {reason}"),
        }
    }

    fn which(program: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(program);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }
}
