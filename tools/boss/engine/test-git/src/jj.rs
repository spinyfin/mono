//! Real shared-store jj fixtures. Missing declared tooling fails the test.
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct JjRepo {
    pub repo: PathBuf,
    pub worker: PathBuf,
    pub replacement: PathBuf,
}

impl JjRepo {
    pub fn new(root: &Path) -> Self {
        let repo = root.join("shared");
        let worker = root.join("worker");
        let replacement = root.join("replacement");
        let output = Command::new(Self::binary())
            .args(["git", "init"])
            .arg(&repo)
            .output()
            .expect("Bazel must provide jj");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        Self::run(&repo, &["config", "set", "--repo", "user.name", "Recovery test"]);
        Self::run(
            &repo,
            &["config", "set", "--repo", "user.email", "recovery@example.invalid"],
        );
        std::fs::write(repo.join("base.txt"), "baseline\n").unwrap();
        Self::run(&repo, &["describe", "-m", "Baseline"]);
        for (name, path) in [("worker", &worker), ("replacement", &replacement)] {
            Self::run(
                &repo,
                &["workspace", "add", path.to_str().unwrap(), "--name", name, "-r", "@"],
            );
        }
        Self {
            repo,
            worker,
            replacement,
        }
    }

    pub fn binary() -> std::ffi::OsString {
        std::env::var_os("BOSS_JJ_BIN").expect("Bazel must declare BOSS_JJ_BIN")
    }

    pub fn run(repo: &Path, args: &[&str]) -> String {
        let output = Command::new(Self::binary())
            .args(["--no-pager", "-R"])
            .arg(repo)
            .args(args)
            .output()
            .expect("Bazel must provide jj");
        assert!(
            output.status.success(),
            "jj {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}
