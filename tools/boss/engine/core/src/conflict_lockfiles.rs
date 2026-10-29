//! Engine-owned regeneration of conflicted generated lockfiles in a worker's
//! own workspace.
//!
//! Rung 0 regenerates lockfiles deterministically when *every* residual
//! conflict is a lockfile. When a conflict also touches source files the
//! worker owns the rebase, but the lockfiles must still never be hand-merged
//! or chosen by a model. `boss engine conflicts regenerate-lockfiles` runs
//! this module: the same [`ResolverRegistry`] rung 0 uses, restricted to the
//! conflicted generated lockfiles, seeded from the PR base's version.

use std::path::Path;

use boss_deterministic_resolvers::{ConflictedFile, RegistryResolution, ResolverRegistry, is_generated_lockfile};

use crate::conflict_ladder::builtin_resolvers;
use crate::coordinator::strip_jj_conflict_descriptor;

/// Result of one regeneration pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockfileRegeneration {
    /// Lockfiles the engine regenerated, as `path: summary`.
    pub regenerated: Vec<String>,
    /// Conflicted paths that are not generated lockfiles and were left alone.
    pub left_for_worker: Vec<String>,
}

/// Bare conflicted paths from `jj resolve --list` output.
pub fn parse_jj_resolve_list(output: &str) -> Vec<String> {
    output
        .lines()
        .map(strip_jj_conflict_descriptor)
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Validate the command result before interpreting stdout as conflicted paths.
pub fn checked_jj_resolve_list(code: Option<i32>, stdout: &str, stderr: &str) -> Result<Vec<String>, String> {
    if code == Some(0) {
        return Ok(parse_jj_resolve_list(stdout));
    }
    // jj uses exit 2 for the explicit no-conflicts diagnostic.
    if code == Some(2) && stdout.trim().is_empty() && stderr.trim() == "Error: No conflicts found at this revision" {
        return Ok(Vec::new());
    }
    Err(format!(
        "\x60jj resolve --list\x60 failed (exit {code:?}): {}",
        stderr.trim()
    ))
}

/// Regenerates every generated lockfile in `conflicted` under `workspace`
/// from `base_ref` (e.g. `main@origin`).
pub async fn regenerate_conflicted_lockfiles(
    workspace: &Path,
    base_ref: &str,
    conflicted: &[String],
) -> Result<LockfileRegeneration, String> {
    regenerate_with(&builtin_resolvers(base_ref), workspace, conflicted).await
}

async fn regenerate_with(
    registry: &ResolverRegistry,
    workspace: &Path,
    conflicted: &[String],
) -> Result<LockfileRegeneration, String> {
    let (lockfiles, left_for_worker): (Vec<&String>, Vec<&String>) =
        conflicted.iter().partition(|path| is_generated_lockfile(path));
    let files: Vec<ConflictedFile> = lockfiles
        .iter()
        .map(|path| ConflictedFile {
            path: (*path).clone(),
            marker_count: None,
            shape: "content".to_owned(),
        })
        .collect();
    let regenerated = match registry.resolve_all(workspace, &files).await {
        RegistryResolution::AllResolved(resolved) => resolved
            .into_iter()
            .map(|file| format!("{}: {}", file.path, file.summary))
            .collect(),
        RegistryResolution::Declined { declined, failed, .. } => {
            let mut reasons: Vec<String> = failed.iter().map(|f| format!("{}: {}", f.path, f.reason)).collect();
            reasons.extend(declined.iter().map(|d| format!("{}: {}", d.path, d.reason)));
            return Err(reasons.join("; "));
        }
    };
    Ok(LockfileRegeneration {
        regenerated,
        left_for_worker: left_for_worker.into_iter().cloned().collect(),
    })
}

#[cfg(test)]
mod tests {
    use boss_deterministic_resolvers::{ConflictClass, DeterministicResolver, ResolveOutcome};

    use super::*;

    #[test]
    fn resolve_list_checks_failures_and_explicit_no_conflicts() {
        assert_eq!(
            checked_jj_resolve_list(Some(0), "Cargo.lock    2-sided conflict\n", "").unwrap(),
            ["Cargo.lock"]
        );
        assert!(checked_jj_resolve_list(Some(0), "", "").unwrap().is_empty());
        assert!(
            checked_jj_resolve_list(Some(2), "", "Error: No conflicts found at this revision\n")
                .unwrap()
                .is_empty()
        );
        for code in [Some(1), Some(2), None] {
            let error = checked_jj_resolve_list(code, "", "working copy is stale").unwrap_err();
            assert!(error.contains("working copy is stale"));
        }
        assert!(checked_jj_resolve_list(Some(2), "Cargo.lock", "Error: No conflicts found at this revision").is_err());
    }

    /// Stands in for the real lockfile tools: rewrites the file, proving the
    /// regenerated content comes from engine-invoked code.
    struct Regenerates;

    #[async_trait::async_trait]
    impl DeterministicResolver for Regenerates {
        fn class(&self) -> ConflictClass {
            ConflictClass::CargoLock
        }

        fn applies_to(&self, file: &ConflictedFile) -> bool {
            is_generated_lockfile(&file.path)
        }

        async fn resolve(&self, workspace: &Path, file: &ConflictedFile) -> ResolveOutcome {
            std::fs::write(workspace.join(&file.path), "# regenerated by engine\n").unwrap();
            ResolveOutcome::Resolved {
                summary: "regenerated".to_owned(),
            }
        }
    }

    #[tokio::test]
    async fn mixed_conflict_regenerates_the_lockfile_and_leaves_source_to_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "<<<<<<< a\nx\n=======\ny\n>>>>>>> b\n").unwrap();
        std::fs::write(dir.path().join("lib.rs"), "<<<<<<< a\nx\n=======\ny\n>>>>>>> b\n").unwrap();
        let mut registry = ResolverRegistry::empty();
        registry.register(Box::new(Regenerates));
        let conflicted = parse_jj_resolve_list("Cargo.lock    2-sided conflict\nlib.rs    2-sided conflict\n");
        assert_eq!(conflicted, ["Cargo.lock", "lib.rs"]);

        let out = regenerate_with(&registry, dir.path(), &conflicted).await.unwrap();

        assert_eq!(out.regenerated, ["Cargo.lock: regenerated"]);
        assert_eq!(out.left_for_worker, ["lib.rs"]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Cargo.lock")).unwrap(),
            "# regenerated by engine\n"
        );
        assert!(
            std::fs::read_to_string(dir.path().join("lib.rs"))
                .unwrap()
                .starts_with("<<<<<<<"),
            "source conflicts stay with the worker"
        );
    }

    #[tokio::test]
    async fn regeneration_failure_is_reported_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ResolverRegistry::empty();
        let err = regenerate_with(&registry, dir.path(), &["Cargo.lock".to_owned()])
            .await
            .unwrap_err();
        assert!(err.contains("Cargo.lock"), "{err}");
    }
}
