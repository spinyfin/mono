//! Collapsing a conflicted ancestor of a resolved `@` before `cube workspace
//! push`, against a real jj repo.
//!
//! A lockfile-only conflict is resolved by rewriting the file at the
//! working-copy child. The rebased PR commit beneath it keeps jj's conflict
//! flag, which a scripted runner cannot show but `jj git push` would reject.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::pr_bookmarks_tests::DeclaredJj;
use crate::app::workspace_ops::{JjCtx, collapse_resolved_ancestors};
use crate::command_runner::{CommandRunner, RealCommandRunner};

struct Repo {
    _dir: TempDir,
    root: PathBuf,
    runner: DeclaredJj,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("jj-config.toml");
        std::fs::write(
            &config,
            "[user]\nname = \"Cube Test\"\nemail = \"cube@example.invalid\"\n",
        )
        .unwrap();
        let binary = std::fs::canonicalize(std::env::var("BOSS_JJ_BIN").expect("Bazel must declare BOSS_JJ_BIN"))
            .expect("declared jj binary exists");
        let root = dir.path().join("root");
        let repo = Self {
            _dir: dir,
            root: root.clone(),
            runner: DeclaredJj { binary, config },
        };
        let parent = repo._dir.path().to_path_buf();
        repo.jj(&parent, &["git", "init", "--colocate", root.to_str().unwrap()]);
        repo
    }

    fn jj(&self, cwd: &Path, args: &[&str]) -> String {
        self.runner
            .run(&RealCommandRunner::invocation(cwd, "jj", args))
            .unwrap_or_else(|e| panic!("jj {args:?}: {e}"))
    }

    fn write(&self, name: &str, body: &str) {
        std::fs::write(self.root.join(name), body).unwrap();
    }

    fn conflicted(&self, revset: &str) -> String {
        self.jj(
            &self.root,
            &[
                "log",
                "-r",
                revset,
                "--no-graph",
                "-T",
                r#"if(conflict, commit_id ++ "\n")"#,
            ],
        )
    }

    /// Base lockfile; `main` and the PR each edit the same line differently,
    /// the PR is rebased onto `main` (leaving the PR commit conflicted), and a
    /// child commit holds the regenerated, conflict-free lockfile.
    fn lockfile_conflict_with_resolved_child(&self, extra_bookmark: Option<&str>) {
        self.write("Cargo.lock", "version = 1\ndep = \"1.0\"\n");
        self.jj(&self.root, &["describe", "-m", "base"]);
        self.jj(&self.root, &["bookmark", "create", "main", "-r", "@"]);
        self.jj(&self.root, &["new", "main", "-m", "main bumps dep"]);
        self.write("Cargo.lock", "version = 1\ndep = \"1.1\"\n");
        self.jj(&self.root, &["bookmark", "set", "main", "-r", "@"]);
        self.jj(&self.root, &["new", "main-", "-m", "pr bumps dep"]);
        self.write("Cargo.lock", "version = 1\ndep = \"2.0\"\n");
        self.jj(&self.root, &["bookmark", "create", "boss/exec_x", "-r", "@"]);
        self.jj(&self.root, &["new", "-m", ""]);
        self.jj(&self.root, &["rebase", "-b", "boss/exec_x", "-d", "main"]);
        if let Some(name) = extra_bookmark {
            self.jj(&self.root, &["bookmark", "create", name, "-r", "boss/exec_x"]);
        }
        self.write("Cargo.lock", "version = 1\ndep = \"2.0\"\n");
    }
}

fn ctx<'a>(repo: &'a Repo) -> JjCtx<'a> {
    JjCtx {
        runner: &repo.runner,
        database_path: None,
        cwd: &repo.root,
    }
}

#[test]
fn resolved_lockfile_child_pushes_after_ancestor_collapse() {
    let repo = Repo::new();
    repo.lockfile_conflict_with_resolved_child(Some("boss-recovery/exec_x"));
    assert!(
        !repo.conflicted("main..@").trim().is_empty(),
        "fixture must start with a conflicted PR commit under the resolved child"
    );
    assert!(repo.conflicted("@").trim().is_empty(), "child is already resolved");
    let tree_before = repo.jj(&repo.root, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);

    collapse_resolved_ancestors(&ctx(&repo), "boss/exec_x");

    assert!(
        repo.conflicted("main..@").trim().is_empty(),
        "no conflicted commit may remain in the range jj would push"
    );
    let drift = repo.jj(
        &repo.root,
        &["diff", "--from", tree_before.trim(), "--to", "@", "--summary"],
    );
    assert!(
        drift.trim().is_empty(),
        "collapse must preserve the resolved tree: {drift}"
    );
    let lock = std::fs::read_to_string(repo.root.join("Cargo.lock")).unwrap();
    assert_eq!(lock, "version = 1\ndep = \"2.0\"\n");
}

#[test]
fn published_pr_bookmark_on_conflicted_ancestor_blocks_collapse() {
    let repo = Repo::new();
    repo.lockfile_conflict_with_resolved_child(Some("pr/9"));

    collapse_resolved_ancestors(&ctx(&repo), "boss/exec_x");

    assert!(
        !repo.conflicted("main..@").trim().is_empty(),
        "a stacked-PR boundary must not be rewritten"
    );
}
