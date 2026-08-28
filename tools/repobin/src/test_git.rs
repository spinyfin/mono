//! Portable git repository constructors used by repobin's tests.
//!
//! Pinning the first branch at `git init` time needs git 2.28+; `symbolic-ref`
//! of `HEAD` on an empty repo is the equivalent two-step that older git still
//! accepts.

use std::path::Path;
use std::process::Command;

/// Initialize a bare git repo at `path` whose `HEAD` names `branch`.
pub(crate) fn init_bare_repo_with_branch(path: &Path, branch: &str) {
    let output = Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(path)
        .output()
        .expect("spawn git init");
    assert!(
        output.status.success(),
        "git init --bare failed: {}",
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
    use super::{init_bare_repo_with_branch, symbolic_ref_head};

    #[test]
    fn init_bare_repo_with_branch_points_head_at_main() {
        let dir = tempfile::tempdir().expect("tempdir");
        let remote = dir.path().join("remote.git");
        init_bare_repo_with_branch(&remote, "main");
        assert_eq!(symbolic_ref_head(&remote), "refs/heads/main");
    }
}
