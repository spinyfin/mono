//! Shared "discard the conflicted lockfile, regenerate it from the merged
//! manifest" strategy used by every built-in lockfile resolver.

use std::path::Path;

use crate::ConflictedFile;
use crate::ResolveOutcome;
use crate::command::{CommandRunner, run_or_fail};

/// `manifest_filename` is the sibling manifest (`Cargo.toml`,
/// `MODULE.bazel`) the regeneration command reads; it must already be
/// conflict-free (jj's structural merge resolves non-overlapping manifest
/// edits before rung 0 ever runs — see the design's rung ordering).
/// `program`/`args` invoke the tool that rewrites the lockfile in place.
pub(crate) async fn regenerate_lockfile(
    runner: &dyn CommandRunner,
    workspace_path: &Path,
    file: &ConflictedFile,
    manifest_filename: &str,
    program: &str,
    args: &[&str],
    base_revision: Option<&str>,
) -> ResolveOutcome {
    let lock_path = workspace_path.join(&file.path);
    let Some(dir) = lock_path.parent() else {
        return ResolveOutcome::Declined {
            reason: format!("{} has no parent directory", file.path),
        };
    };

    let manifest_path = dir.join(manifest_filename);
    if !manifest_path.is_file() {
        return ResolveOutcome::Declined {
            reason: format!("manifest {} not found", manifest_path.display()),
        };
    }

    if let Ok(manifest) = std::fs::read_to_string(&manifest_path)
        && manifest
            .lines()
            .any(|line| line.starts_with("<<<<<<<") || line.starts_with("%%%%%%%"))
    {
        return ResolveOutcome::Declined {
            reason: format!("manifest {} still has merge conflicts", manifest_path.display()),
        };
    }

    let seed = match base_revision {
        Some(revision) => match base_lockfile(runner, workspace_path, file, revision).await {
            Ok(seed) => seed,
            Err(outcome) => return outcome,
        },
        None => None,
    };
    if let Some(seed) = seed {
        if let Err(err) = std::fs::write(&lock_path, seed) {
            return ResolveOutcome::Failed {
                reason: format!("cannot seed {} from PR base: {err}", file.path),
            };
        }
    } else {
        // No base lockfile (or no base supplied): regenerate from the merged
        // manifest. This is not a resolution until the owner command succeeds.
        if let Err(e) = std::fs::remove_file(&lock_path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return ResolveOutcome::Failed {
                reason: format!("failed to remove conflicted {}: {e}", file.path),
            };
        }
    }

    if let Err(outcome) = run_or_fail(runner, dir, program, args, "").await {
        return outcome;
    }

    if !lock_path.is_file() {
        return ResolveOutcome::Declined {
            reason: format!(
                "`{program} {}` succeeded but did not regenerate {}",
                args.join(" "),
                file.path
            ),
        };
    }

    ResolveOutcome::Resolved {
        summary: format!("regenerated {} via `{program} {}`", file.path, args.join(" ")),
    }
}

/// A failed read does not prove absence: verify it against the same base
/// revision before removing the conflicted file. Missing revisions, transport
/// failures and unreadable existing files remain failures, with their evidence.
async fn base_lockfile(
    runner: &dyn CommandRunner,
    workspace: &Path,
    file: &ConflictedFile,
    revision: &str,
) -> Result<Option<String>, ResolveOutcome> {
    let jj = std::env::var_os("BOSS_JJ_BIN")
        .map(std::path::PathBuf::from)
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
        .unwrap_or_else(|| "jj".into());
    let args = ["file", "show", "-r", revision, "--", file.path.as_str()].map(std::ffi::OsString::from);
    let read = runner.run(&jj, &args, Some(workspace)).await;
    if let Ok(output) = &read
        && output.success
    {
        return Ok(Some(output.stdout.clone()));
    }
    let args = ["file", "list", "-r", revision, "--", file.path.as_str()].map(std::ffi::OsString::from);
    let listing = runner.run(&jj, &args, Some(workspace)).await;
    if let Ok(output) = &listing
        && output.success
        && output.stdout.trim().is_empty()
    {
        return Ok(None);
    }
    Err(ResolveOutcome::Failed {
        reason: format!(
            "cannot read {} from PR base {revision}: {read:?}; absence check: {listing:?}",
            file.path
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::FakeCommandRunner;

    fn file(path: &str) -> ConflictedFile {
        ConflictedFile {
            path: path.to_owned(),
            marker_count: Some(3),
            shape: "content".to_owned(),
        }
    }

    #[tokio::test]
    async fn declines_when_manifest_missing() {
        let dir = tempfile::tempdir().unwrap();
        let runner = FakeCommandRunner::success();
        let outcome = regenerate_lockfile(
            &runner,
            dir.path(),
            &file("Cargo.lock"),
            "Cargo.toml",
            "cargo",
            &["generate-lockfile"],
            None,
        )
        .await;
        assert!(matches!(outcome, ResolveOutcome::Declined { reason } if reason.contains("Cargo.toml")));
        assert!(
            runner.calls.lock().unwrap().is_empty(),
            "must not shell out without a manifest"
        );
    }

    #[tokio::test]
    async fn removes_conflicted_lockfile_before_running_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "<<<<<<< ours\n").unwrap();

        let runner = FakeCommandRunner::success_writing_file("Cargo.lock", "regenerated\n");
        let outcome = regenerate_lockfile(
            &runner,
            dir.path(),
            &file("Cargo.lock"),
            "Cargo.toml",
            "cargo",
            &["generate-lockfile"],
            None,
        )
        .await;

        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("Cargo.lock")).unwrap(),
            "regenerated\n",
            "conflicted lockfile content should be discarded, replaced by the regenerated one"
        );
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "cargo");
        assert_eq!(calls[0].1, vec!["generate-lockfile".to_owned()]);
        assert_eq!(calls[0].2, dir.path());
    }

    #[tokio::test]
    async fn declines_when_command_succeeds_but_lockfile_not_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "<<<<<<< ours\n").unwrap();

        // Command exits 0 but (unrealistically) never rewrites the lockfile
        // it was supposed to regenerate.
        let runner = FakeCommandRunner::success();
        let outcome = regenerate_lockfile(
            &runner,
            dir.path(),
            &file("Cargo.lock"),
            "Cargo.toml",
            "cargo",
            &["generate-lockfile"],
            None,
        )
        .await;

        match outcome {
            ResolveOutcome::Declined { reason } => {
                assert!(reason.contains("did not regenerate"), "reason was: {reason}");
                assert!(reason.contains("Cargo.lock"), "reason was: {reason}");
            }
            other => panic!("expected Declined, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fails_with_stderr_when_command_fails() {
        // A non-zero exit from the regeneration command is an operational
        // failure (the resolver ran, its tool errored) — `Failed`, not a
        // clean `Declined`. This is the shape of the cold-workspace bazel
        // env failure: `bazel mod deps` exits non-zero because a gitignored
        // artifact is absent.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();

        let runner = FakeCommandRunner::failure("manifest is invalid");
        let outcome = regenerate_lockfile(
            &runner,
            dir.path(),
            &file("Cargo.lock"),
            "Cargo.toml",
            "cargo",
            &["generate-lockfile"],
            None,
        )
        .await;

        match outcome {
            ResolveOutcome::Failed { reason } => assert!(reason.contains("manifest is invalid")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fails_when_spawn_itself_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();

        let runner = FakeCommandRunner::spawn_error();
        let outcome = regenerate_lockfile(
            &runner,
            dir.path(),
            &file("Cargo.lock"),
            "Cargo.toml",
            "cargo",
            &["generate-lockfile"],
            None,
        )
        .await;

        assert!(matches!(outcome, ResolveOutcome::Failed { reason } if reason.contains("failed to spawn")));
    }

    #[tokio::test]
    async fn tolerates_lockfile_already_absent() {
        // Rung 1's engine-direct rebase may have already left the file
        // deleted (e.g. a delete/modify shape); regeneration should still
        // proceed rather than declining on a missing lockfile.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();

        let runner = FakeCommandRunner::success_writing_file("Cargo.lock", "regenerated\n");
        let outcome = regenerate_lockfile(
            &runner,
            dir.path(),
            &file("Cargo.lock"),
            "Cargo.toml",
            "cargo",
            &["generate-lockfile"],
            None,
        )
        .await;

        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }));
    }
}
