//! Fetch a branch tip into a private ref so concurrent probes cannot replace it.
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{ConflictDiagnosis, collect, resolve_git_dir, run_git};

/// Version 2 identifies diagnoses made against a freshly fetched base branch.
/// `base_sha` in the persisted JSON is the exact tip used, independently of the
/// PR's recorded base at detection. No checkout or published ref is changed.
pub async fn collect_current_base(
    workspace: &Path,
    base_branch: &str,
    head_sha: &str,
) -> std::io::Result<ConflictDiagnosis> {
    static NEXT_REF: AtomicU64 = AtomicU64::new(0);
    let git_dir = resolve_git_dir(workspace)?;
    let private_ref = format!(
        "refs/boss/conflict-diagnosis/{}-{}",
        std::process::id(),
        NEXT_REF.fetch_add(1, Ordering::Relaxed)
    );
    let refspec = format!("+refs/heads/{base_branch}:{private_ref}");
    let fetch = run_git(
        &["fetch", "--no-tags", "--no-write-fetch-head", "origin", &refspec],
        workspace,
        &git_dir,
    )
    .await?;
    let result = async {
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
        diagnosis.schema_version = 2;
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
    }
}
