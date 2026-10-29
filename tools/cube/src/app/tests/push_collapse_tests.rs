//! Collapsing a conflicted ancestor of a resolved `@` before `cube workspace
//! push`, against a real jj repo.
//!
//! A lockfile-only conflict is resolved by rewriting the file at the
//! working-copy child. The rebased PR commit beneath it keeps jj's conflict
//! flag, which a scripted runner cannot show but `jj git push` would reject.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::checkleft_tests::CheckleftEnvGuard;
use super::pr_bookmarks_tests::DeclaredJj;
use super::support::ENV_MUTEX;
use crate::app::errors::{CubeError, Result};
use crate::app::workspace_ops::{
    BookmarkPolicy, JjCtx, LinearizeDecline, LinearizeOutcome, collapse_resolved_ancestors,
    commit_has_blocking_bookmark, workspace_push_at,
};
use crate::command_runner::{CommandInvocation, CommandRunner, RealCommandRunner};

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

    /// Divergent lockfile edits: `main` and `boss/exec_x` each change the
    /// same line, with `@` still on the clean PR commit.
    fn divergent_lockfile_pr(&self) {
        self.write("Cargo.lock", "version = 1\ndep = \"1.0\"\n");
        self.jj(&self.root, &["describe", "-m", "base"]);
        self.jj(&self.root, &["bookmark", "create", "main", "-r", "@"]);
        self.jj(&self.root, &["new", "main", "-m", "main bumps dep"]);
        self.write("Cargo.lock", "version = 1\ndep = \"1.1\"\n");
        self.jj(&self.root, &["bookmark", "set", "main", "-r", "@"]);
        self.jj(&self.root, &["new", "main-", "-m", "pr bumps dep"]);
        self.write("Cargo.lock", "version = 1\ndep = \"2.0\"\n");
        self.jj(&self.root, &["bookmark", "create", "boss/exec_x", "-r", "@"]);
    }

    /// Rebase the PR onto `main` (the PR commit stays conflicted) and leave
    /// a working-copy child holding the regenerated lockfile.
    fn rebase_pr_and_resolve_at_working_copy(&self, extra_bookmark: Option<&str>) {
        self.jj(&self.root, &["new", "-m", ""]);
        self.jj(&self.root, &["rebase", "-b", "boss/exec_x", "-d", "main"]);
        if let Some(name) = extra_bookmark {
            self.jj(&self.root, &["bookmark", "create", name, "-r", "boss/exec_x"]);
        }
        self.write("Cargo.lock", "version = 1\ndep = \"2.0\"\n");
    }

    /// Base lockfile; `main` and the PR each edit the same line differently,
    /// the PR is rebased onto `main` (leaving the PR commit conflicted), and a
    /// child commit holds the regenerated, conflict-free lockfile.
    fn lockfile_conflict_with_resolved_child(&self, extra_bookmark: Option<&str>) {
        self.divergent_lockfile_pr();
        self.rebase_pr_and_resolve_at_working_copy(extra_bookmark);
    }
}

fn ctx<'a>(repo: &'a Repo) -> JjCtx<'a> {
    JjCtx {
        runner: &repo.runner,
        database_path: None,
        cwd: &repo.root,
    }
}

/// Real jj plus a stub GitHub surface so `workspace_push_at` can drive a
/// local bare remote end to end without talking to github.com.
struct PushFixtureRunner {
    jj: DeclaredJj,
    origin: String,
}

impl CommandRunner for PushFixtureRunner {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        if invocation.program == "gh" {
            return self.run_gh(invocation);
        }
        if invocation.program == "jj" && invocation.args.first().map(String::as_str) == Some("git") {
            match invocation.args.get(1).map(String::as_str) {
                Some("remote") if invocation.args.get(2).map(String::as_str) == Some("list") => {
                    return Ok("github\tgit@github.com:spinyfin/mono.git\n".into());
                }
                Some("push") => {
                    let mut rewritten = invocation.clone();
                    rewritten.program = self.jj.binary.display().to_string();
                    rewritten
                        .env
                        .push(("JJ_CONFIG".to_string(), self.jj.config.display().to_string()));
                    for arg in rewritten.args.iter_mut() {
                        if arg == "github" {
                            *arg = self.origin.clone();
                        }
                    }
                    return RealCommandRunner.run(&rewritten);
                }
                _ => {}
            }
        }
        self.jj.run(invocation)
    }
}

impl PushFixtureRunner {
    fn run_gh(&self, invocation: &CommandInvocation) -> Result<String> {
        let args: Vec<&str> = invocation.args.iter().map(String::as_str).collect();
        if args.first() == Some(&"api")
            && args.get(1).is_some_and(|path| path.contains("/branches/"))
            && args.get(2) == Some(&"--jq")
        {
            let branch = args[1]
                .split("/branches/")
                .nth(1)
                .ok_or_else(|| CubeError::InvalidArgument("missing branch in gh api path".into()))?;
            let sha = self.jj.run(&RealCommandRunner::invocation(
                &invocation.cwd,
                "jj",
                &["log", "-r", branch, "--no-graph", "-T", "commit_id"],
            ))?;
            return Ok(sha);
        }
        Ok("[]\n".into())
    }
}

fn add_origin_and_publish(repo: &Repo, branches: &[&str]) -> String {
    let origin = repo._dir.path().join("origin.git");
    let git = std::process::Command::new("git")
        .args(["init", "--bare", origin.to_str().unwrap()])
        .output()
        .expect("git init --bare");
    assert!(git.status.success(), "{}", String::from_utf8_lossy(&git.stderr));
    repo.jj(
        &repo.root,
        &["git", "remote", "add", "origin", origin.to_str().unwrap()],
    );
    for branch in branches {
        repo.jj(
            &repo.root,
            &["git", "push", "--remote", "origin", "-b", branch, "--allow-new"],
        );
    }
    "origin".to_owned()
}

#[test]
fn resolved_lockfile_child_pushes_after_ancestor_collapse() {
    let repo = Repo::new();
    repo.divergent_lockfile_pr();
    repo.jj(
        &repo.root,
        &["bookmark", "create", "boss-recovery/exec_x", "-r", "boss/exec_x"],
    );
    let origin = add_origin_and_publish(&repo, &["main", "boss/exec_x"]);
    repo.rebase_pr_and_resolve_at_working_copy(None);
    assert!(
        !repo.conflicted("main..@").trim().is_empty(),
        "fixture must start with a conflicted PR commit under the resolved child"
    );
    assert!(repo.conflicted("@").trim().is_empty(), "child is already resolved");
    let tree_before = repo.jj(&repo.root, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);

    let runner = PushFixtureRunner {
        jj: DeclaredJj {
            binary: repo.runner.binary.clone(),
            config: repo.runner.config.clone(),
        },
        origin,
    };
    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_gate_disabled();
    let result = workspace_push_at(repo.root.clone(), None, &runner, Some("boss/exec_x".into()), Some(9))
        .expect("resolved history must push");
    assert_eq!(result.payload["status"], "pushed");
    assert_eq!(result.payload["pushed"], true);

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
    assert_eq!(lock.trim_end(), "version = 1\ndep = \"2.0\"");
    assert!(
        repo.conflicted("boss/exec_x@origin").trim().is_empty(),
        "the remote branch must not contain a conflicted commit"
    );
    let remote_lock = repo.jj(&repo.root, &["file", "show", "-r", "boss/exec_x@origin", "Cargo.lock"]);
    assert_eq!(remote_lock.trim_end(), "version = 1\ndep = \"2.0\"");
    let description = repo.jj(
        &repo.root,
        &["log", "-r", "boss/exec_x", "--no-graph", "-T", "description"],
    );
    assert!(
        description.contains("Resolve merge conflict on PR #9"),
        "pushed head must keep the resolve-conflict title: {description}"
    );
    assert!(
        description.contains("Conflicted ancestors collapsed into this resolution"),
        "collapse header must describe a verified resolution, not replay-only: {description}"
    );
    assert!(
        !description.contains("Collapsed replay-only"),
        "push-path collapse must not claim the ancestors were replay-only: {description}"
    );
}

#[test]
fn published_pr_bookmark_on_conflicted_ancestor_blocks_collapse() {
    let repo = Repo::new();
    repo.lockfile_conflict_with_resolved_child(Some("pr/9"));

    let outcome = collapse_resolved_ancestors(&ctx(&repo), "boss/exec_x").expect("decline is not an error");
    assert_eq!(
        outcome,
        LinearizeOutcome::Declined(LinearizeDecline::BookmarkedAncestor),
        "a published pr/ bookmark must be what blocked the collapse"
    );
    assert!(
        !repo.conflicted("main..@").trim().is_empty(),
        "a stacked-PR boundary must not be rewritten"
    );
}

#[test]
fn recovery_bookmark_is_not_a_stack_boundary_but_a_published_pr_bookmark_is() {
    let recovery_repo = Repo::new();
    recovery_repo.lockfile_conflict_with_resolved_child(Some("boss-recovery/x"));
    let recovery_ctx = ctx(&recovery_repo);
    let conflicted = recovery_repo.conflicted("main..@");
    let ancestor = conflicted.lines().next().expect("conflicted ancestor").trim();
    assert!(
        !commit_has_blocking_bookmark(&recovery_ctx, ancestor, BookmarkPolicy::PushBranch("boss/exec_x")).unwrap(),
        "boss-recovery/ bookmarks are engine pointers, not stack boundaries"
    );

    let published_repo = Repo::new();
    published_repo.lockfile_conflict_with_resolved_child(Some("pr/9"));
    let published_ctx = ctx(&published_repo);
    let conflicted = published_repo.conflicted("main..@");
    let ancestor = conflicted.lines().next().expect("conflicted ancestor").trim();
    assert!(
        commit_has_blocking_bookmark(&published_ctx, ancestor, BookmarkPolicy::PushBranch("boss/exec_x")).unwrap(),
        "a published pr/ bookmark on a conflicted ancestor is a stack boundary"
    );
}
