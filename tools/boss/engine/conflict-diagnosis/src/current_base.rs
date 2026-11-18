//! Fetch a branch tip into a private ref so concurrent probes cannot replace it.
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::{ConflictDiagnosis, collect, resolve_git_dir, run_git};

/// Bound on the pre-spawn `git fetch` of the PR base branch. A hung remote
/// or auth prompt must not stall the resolver spawn; expiry yields an
/// errored diagnosis and still deletes the private ref.
pub const BASE_BRANCH_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Version 2 identifies diagnoses made against a freshly fetched base branch.
/// `base_sha` in the persisted JSON is the exact tip used, independently of the
/// PR's recorded base at detection. No checkout or published ref is changed.
/// Failed probes stay at schema version 1 so hotspot aggregation does not
/// count them as fetched-base events.
pub async fn collect_current_base(
    workspace: &Path,
    base_branch: &str,
    head_sha: &str,
) -> std::io::Result<ConflictDiagnosis> {
    collect_current_base_with_timeout(workspace, base_branch, head_sha, BASE_BRANCH_FETCH_TIMEOUT).await
}

async fn collect_current_base_with_timeout(
    workspace: &Path,
    base_branch: &str,
    head_sha: &str,
    fetch_timeout: Duration,
) -> std::io::Result<ConflictDiagnosis> {
    static NEXT_REF: AtomicU64 = AtomicU64::new(0);
    let git_dir = resolve_git_dir(workspace)?;
    let private_ref = format!(
        "refs/boss/conflict-diagnosis/{}-{}",
        std::process::id(),
        NEXT_REF.fetch_add(1, Ordering::Relaxed)
    );
    let refspec = format!("+refs/heads/{base_branch}:{private_ref}");
    let result = async {
        let fetch = match tokio::time::timeout(
            fetch_timeout,
            run_git(
                &["fetch", "--no-tags", "--no-write-fetch-head", "origin", &refspec],
                workspace,
                &git_dir,
            ),
        )
        .await
        {
            Ok(output) => output?,
            Err(_) => {
                return Ok(ConflictDiagnosis::errored(
                    "",
                    head_sha,
                    format!("base branch fetch timed out after {fetch_timeout:?}"),
                ));
            }
        };
        if !fetch.status.success() {
            return Ok(ConflictDiagnosis::errored(
                "",
                head_sha,
                format!("base branch fetch failed: {}", String::from_utf8_lossy(&fetch.stderr)),
            ));
        }
        let tip = run_git(&["rev-parse", "--verify", &private_ref], workspace, &git_dir).await?;
        if !tip.status.success() {
            return Ok(ConflictDiagnosis::errored(
                "",
                head_sha,
                "could not resolve fetched base tip",
            ));
        }
        let base_sha = String::from_utf8_lossy(&tip.stdout).trim().to_owned();
        let mut diagnosis = collect(workspace, &base_sha, head_sha).await.unwrap_or_else(|error| {
            ConflictDiagnosis::errored(&base_sha, head_sha, format!("git probe failed: {error}"))
        });
        if diagnosis.error.is_none() {
            diagnosis.schema_version = 2;
        }
        Ok(diagnosis)
    }
    .await;
    let cleanup = run_git(&["update-ref", "-d", &private_ref], workspace, &git_dir).await?;
    if !cleanup.status.success() {
        return Err(std::io::Error::other(format!(
            "could not remove diagnosis ref: {}",
            String::from_utf8_lossy(&cleanup.stderr)
        )));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[tokio::test]
    async fn fetches_conflicting_tip_instead_of_ancestral_recorded_base() {
        let remote = tempfile::tempdir().unwrap();
        let repo = remote.path();
        boss_engine_test_git::init_repo_with_branch(repo, "main");
        git(repo, &["config", "user.email", "test@example.invalid"]);
        git(repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("a.txt"), "original\n").unwrap();
        git(repo, &["add", "a.txt"]);
        git(repo, &["commit", "-m", "Initial"]);
        let recorded_base = git(repo, &["rev-parse", "HEAD"]);
        git(repo, &["checkout", "-b", "feature"]);
        std::fs::write(repo.join("a.txt"), "feature\n").unwrap();
        git(repo, &["commit", "-am", "Feature"]);
        let head = git(repo, &["rev-parse", "HEAD"]);
        // Clone before main advances: even the local tracking ref is stale.
        let workspace = tempfile::tempdir().unwrap();
        git(workspace.path(), &["clone", repo.to_str().unwrap(), "."]);
        git(repo, &["checkout", "main"]);
        std::fs::write(repo.join("a.txt"), "main\n").unwrap();
        git(repo, &["commit", "-am", "Conflicting main change"]);
        let tip = git(repo, &["rev-parse", "HEAD"]);
        let stale = collect(workspace.path(), &recorded_base, &head).await.unwrap();
        assert!(stale.error.is_none());
        assert!(stale.files.is_empty(), "the old probe misses the conflict");
        let diagnosis = collect_current_base(workspace.path(), "main", &head).await.unwrap();
        assert!(diagnosis.error.is_none(), "{diagnosis:?}");
        assert_eq!(diagnosis.files.len(), 1);
        assert_eq!(diagnosis.files[0].path, "a.txt");
        assert_eq!(diagnosis.base_sha, tip);
        assert_eq!(diagnosis.head_sha, head);
        assert_eq!(diagnosis.schema_version, 2);
        assert_eq!(git(workspace.path(), &["rev-parse", "HEAD"]), head);
        assert!(git(workspace.path(), &["for-each-ref", "refs/boss/conflict-diagnosis"]).is_empty());
        let failed = collect_current_base(workspace.path(), "missing-branch", &head)
            .await
            .unwrap();
        assert!(failed.error.unwrap().contains("base branch fetch failed"));
        assert!(failed.base_sha.is_empty(), "never fall back to the stale base");
        assert_eq!(failed.schema_version, 1, "errored probes are not fetched-base events");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fetch_timeout_returns_errored_diagnosis_and_deletes_the_private_ref() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir().unwrap();
        boss_engine_test_git::init_repo_with_branch(workspace.path(), "main");
        git(workspace.path(), &["config", "user.email", "test@example.invalid"]);
        git(workspace.path(), &["config", "user.name", "Test"]);
        std::fs::write(workspace.path().join("a.txt"), "original\n").unwrap();
        git(workspace.path(), &["add", "a.txt"]);
        git(workspace.path(), &["commit", "-m", "Initial"]);
        let head = git(workspace.path(), &["rev-parse", "HEAD"]);
        // Repo-local ssh command that ignores extra BatchMode flags and hangs,
        // so a fetch of an ssh remote cannot complete before the probe deadline.
        let sleeper = workspace.path().join("hang-ssh");
        std::fs::write(&sleeper, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&sleeper, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(
            workspace.path(),
            &["config", "core.sshCommand", sleeper.to_str().unwrap()],
        );
        git(
            workspace.path(),
            &["remote", "add", "origin", "ssh://example.invalid/repo.git"],
        );

        let diagnosis = collect_current_base_with_timeout(workspace.path(), "main", &head, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(
            diagnosis
                .error
                .as_deref()
                .is_some_and(|error| error.contains("timed out")),
            "{diagnosis:?}"
        );
        assert_eq!(diagnosis.schema_version, 1);
        assert!(diagnosis.base_sha.is_empty());
        assert!(git(workspace.path(), &["for-each-ref", "refs/boss/conflict-diagnosis"]).is_empty());
    }
}
