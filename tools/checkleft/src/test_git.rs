//! Portable git repository constructors used by checkleft's own tests.
//!
//! Not a product API — kept as a library module so `src/` unit tests and
//! `tests/` integration tests share one constructor. Pinning the first branch
//! at `git init` time needs git 2.28+; `symbolic-ref` of `HEAD` on an empty
//! repo is the equivalent two-step that older git still accepts.

use std::path::Path;
use std::process::Command;

/// Initialize a non-bare git repo at `path` whose `HEAD` names `branch`.
pub fn init_repo_with_branch(path: &Path, branch: &str) {
    init_repo(path, branch, false);
}

/// Initialize a bare git repo at `path` whose `HEAD` names `branch`.
pub fn init_bare_repo_with_branch(path: &Path, branch: &str) {
    init_repo(path, branch, true);
}

fn init_repo(path: &Path, branch: &str, bare: bool) {
    let mut init = Command::new("git");
    init.arg("init").arg("-q");
    if bare {
        init.arg("--bare");
    }
    let output = init.arg(path).output().expect("spawn git init");
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

fn symbolic_ref_head(path: &Path) -> String {
    let output = Command::new("git")
        .args(["symbolic-ref", "HEAD"])
        .current_dir(path)
        .output()
        .expect("spawn git symbolic-ref HEAD");
    assert!(
        output.status.success(),
        "git symbolic-ref HEAD failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("symbolic-ref stdout utf-8")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{init_bare_repo_with_branch, init_repo_with_branch, symbolic_ref_head};

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
    fn init_bare_repo_with_branch_points_head_at_main() {
        let dir = tempfile::tempdir().expect("tempdir");
        let remote = dir.path().join("remote.git");
        init_bare_repo_with_branch(&remote, "main");
        assert_eq!(symbolic_ref_head(&remote), "refs/heads/main");
    }
}
