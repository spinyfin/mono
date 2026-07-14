//! Shared "discard the conflicted lockfile, regenerate it from the merged
//! manifest" strategy used by every built-in lockfile resolver.

use std::path::Path;

use crate::ConflictedFile;
use crate::ResolveOutcome;
use crate::command::{CommandRunner, run_or_decline};

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

    // The conflicted lockfile still has merge markers in it; discard it so
    // the regeneration tool writes fresh content instead of tripping over
    // invalid syntax (or, worse, silently trusting stale entries). Rung 1's
    // engine-direct rebase may have already removed it, so a missing file
    // isn't itself an error.
    if let Err(e) = std::fs::remove_file(&lock_path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return ResolveOutcome::Declined {
            reason: format!("failed to remove conflicted {}: {e}", file.path),
        };
    }

    if let Err(outcome) = run_or_decline(runner, dir, program, args, "").await {
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
    async fn declines_with_stderr_when_command_fails() {
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
        )
        .await;

        match outcome {
            ResolveOutcome::Declined { reason } => assert!(reason.contains("manifest is invalid")),
            other => panic!("expected Declined, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn declines_when_spawn_itself_fails() {
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
        )
        .await;

        assert!(matches!(outcome, ResolveOutcome::Declined { reason } if reason.contains("failed to spawn")));
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
        )
        .await;

        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }));
    }
}
