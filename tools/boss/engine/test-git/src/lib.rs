//! Portable git repository constructors shared by boss-engine test suites.
//!
//! Pinning the first branch at `git init` time needs git 2.28+; `symbolic-ref`
//! of `HEAD` on an empty repo is the equivalent two-step that older git still
//! accepts.

use std::path::Path;
use std::process::Command;

/// Initialize a git repo at `path` whose `HEAD` names `branch`.
///
/// Panics if git is unavailable or either step fails. Use
/// [`try_init_repo_with_branch`] instead when the caller needs to skip
/// rather than fail in a hermetic sandbox that lacks git.
pub fn init_repo_with_branch(path: &Path, branch: &str) {
    let output = Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .output()
        .expect("spawn git init");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let head = format!("refs/heads/{branch}");
    let output = Command::new("git")
        .args(["symbolic-ref", "HEAD", &head])
        .current_dir(path)
        .output()
        .expect("spawn git symbolic-ref");
    assert!(
        output.status.success(),
        "git symbolic-ref HEAD {head} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        symbolic_ref_head(path),
        head,
        "HEAD must point at the intended branch after init"
    );
}

/// Initialize a git repo at `path` whose `HEAD` names `branch`.
///
/// Returns `false` when git is missing or either step fails so callers can
/// skip rather than fail in a hermetic sandbox.
pub fn try_init_repo_with_branch(path: &Path, branch: &str) -> bool {
    let Ok(output) = Command::new("git").args(["init", "-q"]).current_dir(path).output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    let head = format!("refs/heads/{branch}");
    let Ok(output) = Command::new("git")
        .args(["symbolic-ref", "HEAD", &head])
        .current_dir(path)
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    assert_eq!(
        symbolic_ref_head(path),
        head,
        "HEAD must point at the intended branch after init"
    );
    true
}

fn symbolic_ref_head(path: &Path) -> String {
    Command::new("git")
        .args(["symbolic-ref", "HEAD"])
        .current_dir(path)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{init_repo_with_branch, symbolic_ref_head, try_init_repo_with_branch};

    #[test]
    fn init_repo_with_branch_points_head_at_main() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo_with_branch(dir.path(), "main");
        assert_eq!(symbolic_ref_head(dir.path()), "refs/heads/main");
    }

    #[test]
    fn init_repo_with_branch_points_head_at_a_namespaced_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo_with_branch(dir.path(), "boss/exec_test");
        assert_eq!(symbolic_ref_head(dir.path()), "refs/heads/boss/exec_test");
    }

    #[test]
    fn try_init_repo_with_branch_points_head_at_main() {
        let dir = tempfile::tempdir().expect("tempdir");
        if !try_init_repo_with_branch(dir.path(), "main") {
            eprintln!("skipping: git unavailable in sandbox");
            return;
        }
        assert_eq!(symbolic_ref_head(dir.path()), "refs/heads/main");
    }

    #[test]
    fn try_init_repo_with_branch_points_head_at_a_custom_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        if !try_init_repo_with_branch(dir.path(), "pr-branch") {
            eprintln!("skipping: git unavailable in sandbox");
            return;
        }
        assert_eq!(symbolic_ref_head(dir.path()), "refs/heads/pr-branch");
    }
}
