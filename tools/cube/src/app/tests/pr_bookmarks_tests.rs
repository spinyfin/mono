//! `pr/<n>` bookmark lifecycle against a real colocated jj repo.
//!
//! The stale-git-ref cycle only exists in the interplay between jj's
//! import/export bookkeeping and the colocated `.git`, so a scripted
//! `FakeRunner` cannot show it. These tests run the declared `jj`
//! against a throwaway colocated repo plus one secondary
//! workspace in a temp dir — never cube's live shared store, never cube
//! itself, and never the test binary.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::app::errors::{CubeError, Result};
use crate::app::pr_bookmarks::{delete_pr_bookmarks, point_pr_bookmark};
use crate::command_runner::{CommandInvocation, CommandRunner, RealCommandRunner};

#[path = "pr_gc_regression_tests.rs"]
mod regression;

#[path = "store_gc_concurrency_tests.rs"]
mod store_gc_concurrency;

/// Runs `jj` invocations with the Bazel-declared binary and a fixed identity,
/// so the fixture needs no global jj config and cannot read the host's.
pub(super) struct DeclaredJj {
    pub(super) binary: PathBuf,
    pub(super) config: PathBuf,
}

impl CommandRunner for DeclaredJj {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        let mut invocation = invocation.clone();
        if invocation.program == "jj" {
            invocation.program = self.binary.display().to_string();
            invocation
                .env
                .push(("JJ_CONFIG".to_string(), self.config.display().to_string()));
        }
        RealCommandRunner.run(&invocation)
    }
}

/// A colocated `root` and one secondary workspace `ws` sharing its store, with
/// a `feat` bookmark on a commit above `main`.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    ws: PathBuf,
    runner: DeclaredJj,
}

impl Fixture {
    fn new() -> Self {
        Self::with_ws_path("ws")
    }

    /// As [`Fixture::new`], with the secondary workspace at `<tmp>/<ws_path>`.
    fn with_ws_path(ws_path: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("jj-config.toml");
        std::fs::write(
            &config,
            "[user]\nname = \"Cube Test\"\nemail = \"cube@example.invalid\"\n",
        )
        .unwrap();
        let root = dir.path().join("root");
        let ws = dir.path().join(ws_path);
        std::fs::create_dir_all(ws.parent().unwrap()).unwrap();
        // Bazel hands over a runfiles-relative path; the fixture runs jj from
        // other directories, so pin it to an absolute one up front.
        let binary = std::fs::canonicalize(std::env::var("BOSS_JJ_BIN").expect("Bazel must declare BOSS_JJ_BIN"))
            .expect("declared jj binary exists");
        let runner = DeclaredJj { binary, config };
        let fixture = Self {
            _dir: dir,
            root: root.clone(),
            ws: ws.clone(),
            runner,
        };
        let parent = fixture._dir.path().to_path_buf();
        fixture.jj(&parent, &["git", "init", "--colocate", root.to_str().unwrap()]);
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        fixture.jj(&root, &["describe", "-m", "base"]);
        fixture.jj(&root, &["bookmark", "create", "main", "-r", "@"]);
        fixture.jj(&root, &["new", "-m", "feature work"]);
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        fixture.jj(&root, &["bookmark", "create", "feat", "-r", "@"]);
        fixture.jj(&root, &["new", "main"]);
        fixture.jj(
            &root,
            &[
                "workspace",
                "add",
                ws.to_str().unwrap(),
                "--name",
                ws.file_name().unwrap().to_str().unwrap(),
                "-r",
                "main",
            ],
        );
        fixture
    }

    fn jj(&self, cwd: &Path, args: &[&str]) -> String {
        self.runner
            .run(&RealCommandRunner::invocation(cwd, "jj", args))
            .unwrap_or_else(|e| panic!("jj {args:?} in {}: {e}", cwd.display()))
    }

    fn commit(&self, revset: &str) -> String {
        self.jj(&self.ws, &["log", "--no-graph", "-r", revset, "-T", "commit_id"])
    }

    /// Any write at the colocated root, which is where jj auto-imports and
    /// auto-exports git refs.
    fn root_write(&self) {
        self.jj(&self.root, &["new", "main"]);
    }

    fn pr_commits(&self, n: u64) -> Vec<String> {
        let revset = format!("bookmarks(exact:\"pr/{n}\")");
        self.jj(
            &self.ws,
            &["log", "--no-graph", "-r", &revset, "-T", r#"commit_id ++ "\n""#],
        )
        .lines()
        .map(str::to_owned)
        .collect()
    }

    fn conflicted(&self) -> String {
        self.jj(&self.ws, &["bookmark", "list", "--conflicted"])
    }

    fn git_ref(&self, n: u64) -> Option<String> {
        // The fixture creates only loose refs, so read the actual exported
        // ref without invoking an undeclared host git.
        match std::fs::read_to_string(self.root.join(format!(".git/refs/heads/pr/{n}"))) {
            Ok(value) => Some(value.trim().to_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("read git ref: {e}"),
        }
    }

    fn protect_feat_remotely(&self) {
        self.jj(
            &self.ws,
            &["git", "remote", "add", "backup", self.root.to_str().unwrap()],
        );
        self.jj(&self.ws, &["git", "fetch", "--remote", "backup"]);
    }

    /// `pr/1` exported to git at `feat`'s current commit, then `feat` rewritten
    /// from the secondary workspace (so `pr/1`, which follows the change, and
    /// the git ref diverge). Returns `(old, new)`.
    fn rewritten_feat_with_exported_pr_ref(&self) -> (String, String) {
        let old = self.commit("feat");
        self.jj(&self.root, &["bookmark", "create", "pr/1", "-r", "feat"]);
        assert_eq!(self.git_ref(1).as_deref(), Some(old.as_str()), "pr/1 exported at root");
        self.jj(&self.ws, &["describe", "-r", "feat", "-m", "feature work, reworded"]);
        let new = self.commit("feat");
        assert_ne!(old, new);
        (old, new)
    }
}

/// Control: why `forget` is not used. Forgetting from a secondary workspace
/// drops jj's record of the exported ref, so the next write at the colocated
/// root re-imports the stale ref and brings the bookmark back.
#[test]
fn forget_from_a_secondary_workspace_is_resurrected_by_the_next_auto_import() {
    let f = Fixture::new();
    let (old, _new) = f.rewritten_feat_with_exported_pr_ref();

    f.jj(&f.ws, &["bookmark", "forget", "pr/1"]);
    f.root_write();

    assert_eq!(
        f.pr_commits(1),
        vec![old],
        "the forgotten bookmark came back at the stale target"
    );
}

#[test]
fn delete_then_auto_import_does_not_resurrect() {
    let f = Fixture::new();
    let (_old, _new) = f.rewritten_feat_with_exported_pr_ref();
    f.jj(&f.ws, &["git", "export"]);
    f.protect_feat_remotely();

    delete_pr_bookmarks(&f.runner, None, &f.ws, &["pr/1".to_string()]).expect("delete");

    assert_eq!(
        f.git_ref(1),
        None,
        "the deletion reached the git repo in the same operation"
    );
    f.root_write();
    assert!(f.pr_commits(1).is_empty(), "auto-import must not bring it back");
    assert_eq!(f.git_ref(1), None);
}

#[test]
fn set_from_a_secondary_workspace_over_a_stale_git_ref_does_not_conflict() {
    let f = Fixture::new();
    let (old, new) = f.rewritten_feat_with_exported_pr_ref();
    // A stale ref left behind by a cube that used `forget`.
    f.jj(&f.ws, &["bookmark", "forget", "pr/1"]);
    assert_eq!(f.git_ref(1).as_deref(), Some(old.as_str()));

    point_pr_bookmark(&f.runner, None, &f.ws, 1, "feat").expect("point");
    f.root_write();

    assert_eq!(f.pr_commits(1), vec![new.clone()]);
    assert!(f.conflicted().is_empty(), "no conflict: {}", f.conflicted());
    assert_eq!(f.git_ref(1).as_deref(), Some(new.as_str()), "git ref moved to the head");
}

#[test]
fn conflicted_bookmark_with_an_obsolete_side_is_repaired_to_the_head() {
    let f = Fixture::new();
    let (old, new) = f.rewritten_feat_with_exported_pr_ref();
    // Reproduce the incident: forget, set from the secondary workspace, then
    // the colocated root imports the stale ref and conflicts the bookmark.
    f.jj(&f.ws, &["bookmark", "forget", "pr/1"]);
    f.jj(&f.ws, &["bookmark", "set", "pr/1", "-r", "feat"]);
    f.root_write();
    let mut sides = f.pr_commits(1);
    sides.sort();
    let mut expected = vec![old, new.clone()];
    expected.sort();
    assert_eq!(sides, expected, "precondition: `pr/1` resolves to two commits");

    point_pr_bookmark(&f.runner, None, &f.ws, 1, "feat").expect("repair");
    f.root_write();

    assert_eq!(f.pr_commits(1), vec![new.clone()]);
    assert!(f.conflicted().is_empty(), "{}", f.conflicted());
    assert_eq!(f.git_ref(1).as_deref(), Some(new.as_str()));
}

#[test]
fn conflicted_bookmark_with_unique_content_fails_loudly_and_writes_nothing() {
    let f = Fixture::new();
    // `pr/1` exported at a commit that has nothing to do with `feat`.
    f.jj(&f.root, &["new", "main", "-m", "unrelated work"]);
    std::fs::write(f.root.join("c.txt"), "c\n").unwrap();
    f.jj(&f.root, &["bookmark", "create", "pr/1", "-r", "@"]);
    let other = f.commit("pr/1");
    f.root_write();
    f.jj(&f.ws, &["bookmark", "forget", "pr/1"]);
    f.jj(&f.ws, &["bookmark", "set", "pr/1", "-r", "feat"]);
    f.root_write();
    let head = f.commit("feat");
    assert_eq!(f.pr_commits(1).len(), 2, "precondition: conflicted");
    let before = f.jj(&f.ws, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);

    let err = point_pr_bookmark(&f.runner, None, &f.ws, 1, "feat").expect_err("unique content must be refused");

    let msg = match err {
        CubeError::InvalidArgument(msg) => msg,
        other => panic!("unexpected error kind: {other}"),
    };
    assert!(msg.contains(&head) && msg.contains(&other), "names both commits: {msg}");
    assert_eq!(f.pr_commits(1).len(), 2, "still conflicted, no side picked");
    assert!(f.conflicted().contains("pr/1"));
    let after = f.jj(&f.ws, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    assert_eq!(after, before, "unsafe repair must not write an operation");
}

#[test]
fn newer_local_rewrite_is_not_an_obsolete_predecessor() {
    let f = Fixture::new();
    let old = f.commit("feat");
    f.jj(&f.ws, &["new", "feat"]);
    // Rewrite the actual feature, adding unpublished content.
    f.jj(&f.ws, &["edit", "feat"]);
    std::fs::write(f.ws.join("local.txt"), "unpublished\n").unwrap();
    f.jj(&f.ws, &["bookmark", "set", "pr/1", "-r", "@"]);
    let newer = f.commit("pr/1");
    let err = point_pr_bookmark(&f.runner, None, &f.ws, 1, &old)
        .unwrap_err()
        .to_string();
    assert!(err.contains(&old) && err.contains(&newer), "{err}");
    assert_eq!(f.pr_commits(1), vec![newer]);
}

#[test]
fn divergent_successors_do_not_prove_obsolescence() {
    let f = Fixture::new();
    let op = f.jj(&f.ws, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    f.jj(&f.ws, &["edit", "feat"]);
    std::fs::write(f.ws.join("left.txt"), "left\n").unwrap();
    f.jj(&f.ws, &["bookmark", "set", "pr/1", "-r", "@"]);
    let left = f.commit("pr/1");
    // A second successor from the same operation, not from the first rewrite.
    f.jj(
        &f.root,
        &["--at-op", &op, "describe", "-r", "feat", "-m", "divergent successor"],
    );
    let variants = f.jj(
        &f.ws,
        &["log", "--no-graph", "-r", "feat", "-T", "commit_id ++ \"\\n\""],
    );
    let right = variants.lines().find(|id| *id != left).unwrap();
    let err = point_pr_bookmark(&f.runner, None, &f.ws, 1, right)
        .unwrap_err()
        .to_string();
    assert!(err.contains(&left) && err.contains(right), "{err}");
    assert!(f.pr_commits(1).contains(&left));
}

#[test]
fn unique_singleton_stale_git_ref_survives_migration_and_gc() {
    let f = Fixture::new();
    f.jj(&f.root, &["new", "main", "-m", "unpublished"]);
    std::fs::write(f.root.join("local.txt"), "unique\n").unwrap();
    f.jj(&f.root, &["bookmark", "create", "pr/1", "-r", "@"]);
    let unique = f.commit("pr/1");
    f.root_write();
    f.jj(&f.ws, &["bookmark", "forget", "pr/1"]);
    assert!(point_pr_bookmark(&f.runner, None, &f.ws, 1, "feat").is_err());
    assert_eq!(f.pr_commits(1), vec![unique.clone()]);
    let collected = crate::app::gc::gc_workspace_bookmarks(&f.runner, None, &f.ws, false, false).unwrap();
    assert!(!collected.contains(&"pr/1".to_string()));
    assert_eq!(f.git_ref(1), Some(unique));
}

#[test]
fn export_failure_names_the_ref_even_when_jj_exits_successfully() {
    let f = Fixture::new();
    let (old, new) = f.rewritten_feat_with_exported_pr_ref();
    // Git's ref lock forces jj's export warning/exit-zero failure path.
    let lock = f.root.join(".git/refs/heads/pr/1.lock");
    std::fs::write(lock, "held by another writer\n").unwrap();
    let err = point_pr_bookmark(&f.runner, None, &f.ws, 1, "feat")
        .unwrap_err()
        .to_string();
    assert!(err.contains("refs/heads/pr/1"), "{err}");
    assert_eq!(f.pr_commits(1), vec![new]);
    assert_eq!(f.git_ref(1), Some(old));
}

/// Only GitHub's metadata lookup is simulated; all repository operations,
/// including fetching the authoritative remote, run through declared jj.
/// Network-shaped commands and lock acquisition are recorded in order.
struct LeaseRunner<'a> {
    jj: &'a DeclaredJj,
    pr_json: String,
    events: std::cell::RefCell<Vec<&'static str>>,
}

impl<'a> LeaseRunner<'a> {
    fn new(jj: &'a DeclaredJj) -> Self {
        Self::with_pr_json(
            jj,
            r#"{"state":"open","head":{"ref":"feat","sha":"unused"}}"#.to_string(),
        )
    }

    fn fork_json(sha: &str, branch: &str, clone_url: &Path) -> String {
        format!(
            r#"{{"state":"open","head":{{"ref":"{branch}","sha":"{sha}","repo":{{"full_name":"fork/repo","clone_url":"{}"}}}}}}"#,
            clone_url.display()
        )
    }

    fn with_pr_json(jj: &'a DeclaredJj, pr_json: String) -> Self {
        let pr_json = match serde_json::from_str::<serde_json::Value>(&pr_json) {
            Ok(mut info) => {
                if info["head"].get("repo").is_none() {
                    info["head"]["repo"] = serde_json::json!({"full_name": "example/repo"});
                }
                info.to_string()
            }
            Err(_) => pr_json,
        };
        Self {
            jj,
            pr_json,
            events: Default::default(),
        }
    }

    fn assert_no_network_while_locked(&self) {
        let events = self.events.borrow();
        let first_lock = events.iter().position(|e| *e == "lock");
        if let Some(lock) = first_lock {
            assert!(
                !events[lock..].contains(&"network"),
                "network command ran while the PR-bookmark lock was held: {events:?}"
            );
        }
    }
}

impl CommandRunner for LeaseRunner<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        if invocation.program == "gh" {
            assert_eq!(invocation.args, ["api", "repos/example/repo/pulls/1"]);
            self.events.borrow_mut().push("network");
            return Ok(self.pr_json.clone());
        }
        if invocation.args == ["git", "remote", "list"] {
            return Ok("backup https://github.com/example/repo".to_string());
        }
        if invocation.args.starts_with(&["git".to_string(), "fetch".to_string()]) {
            self.events.borrow_mut().push("network");
        }
        self.jj.run(invocation)
    }

    fn lock_pr_bookmarks(&self, cwd: &Path) -> Result<crate::lock::RepoLock> {
        self.events.borrow_mut().push("lock");
        crate::lock::RepoLock::for_workspace(cwd)
    }
}

/// A conflicted `pr/1` as the incident produced it: `head` (feat) and `other`
/// are its two sides. `unique` makes `other` unpublished content the head lacks;
/// otherwise it is a predecessor recorded in the head's evolution.
fn conflicted_pr_bookmark(f: &Fixture, unique: bool) -> (String, String) {
    let (_, head) = f.rewritten_feat_with_exported_pr_ref();
    f.jj(&f.ws, &["git", "export"]);
    f.protect_feat_remotely();
    let other = if unique {
        f.jj(&f.root, &["new", "main", "-m", "unpublished"]);
        std::fs::write(f.root.join("unique.txt"), "unique\n").unwrap();
        f.jj(&f.root, &["bookmark", "set", "pr/1", "-r", "@", "--allow-backwards"]);
        f.commit("pr/1")
    } else {
        // Another predecessor remains recorded in the head's evolution.
        let history = f.jj(
            &f.ws,
            &["evolog", "--no-graph", "-r", &head, "-T", "commit_id ++ \"\\n\""],
        );
        let old = history.lines().nth(1).unwrap().to_string();
        f.jj(&f.root, &["bookmark", "set", "pr/1", "-r", &old, "--allow-backwards"]);
        old
    };
    f.root_write();
    f.jj(&f.ws, &["bookmark", "forget", "pr/1"]);
    f.jj(&f.ws, &["bookmark", "set", "pr/1", "-r", &head]);
    f.root_write();
    assert_eq!(f.pr_commits(1).len(), 2);
    (head, other)
}

fn repair(f: &Fixture, runner: &LeaseRunner) -> crate::app::health::BookmarkRepair {
    let repair = crate::app::health::repair_conflicted_bookmarks(runner, None, &f.ws, &["pr/1".to_string()])
        .expect("pr/<n> repair is best-effort and never fails the lease");
    runner.assert_no_network_while_locked();
    repair
}

#[test]
fn lease_repairs_obsolete_conflict() {
    let f = Fixture::new();
    let (head, _) = conflicted_pr_bookmark(&f, false);
    let runner = LeaseRunner::with_pr_json(
        &f.runner,
        format!(r#"{{"state":"open","head":{{"ref":"feat","sha":"{head}"}}}}"#),
    );
    assert!(repair(&f, &runner).failures.is_empty());
    assert_eq!(f.pr_commits(1), vec![head.clone()]);
    assert_eq!(f.git_ref(1), Some(head));
}

#[test]
fn lease_succeeds_and_leaves_a_conflict_with_unique_content_untouched() {
    let f = Fixture::new();
    let (head, other) = conflicted_pr_bookmark(&f, true);
    let runner = LeaseRunner::with_pr_json(
        &f.runner,
        format!(r#"{{"state":"open","head":{{"ref":"feat","sha":"{head}"}}}}"#),
    );
    let repair = repair(&f, &runner);
    assert_eq!(repair.unrepaired(), ["pr/1"]);
    assert_eq!(f.pr_commits(1).len(), 2, "still conflicted, no side picked");
    assert_eq!(f.git_ref(1), Some(other));
}

fn repair_command(f: &Fixture, runner: &LeaseRunner) -> Result<crate::app::errors::RunResult> {
    let result =
        crate::app::workspace_ops::workspace_repair_pr_bookmark(None, runner, Some(f.ws.display().to_string()), 1);
    runner.assert_no_network_while_locked();
    result
}

fn head_json(head: &str) -> String {
    format!(r#"{{"state":"open","head":{{"ref":"feat","sha":"{head}"}}}}"#)
}

/// The engine's recovery repair goes through this command: a missing alias is
/// recreated under the PR-bookmark lock and exported to git, and no working
/// copy moves.
#[test]
fn repair_command_recreates_a_deleted_alias_under_the_lock() {
    let f = Fixture::new();
    let (head, _) = conflicted_pr_bookmark(&f, false);
    f.jj(&f.ws, &["bookmark", "delete", "pr/1"]);
    f.root_write();
    assert!(f.pr_commits(1).is_empty(), "precondition: the alias is gone");
    let working_copy = f.commit("@");
    let runner = LeaseRunner::with_pr_json(&f.runner, head_json(&head));

    repair_command(&f, &runner).expect("missing alias is repaired");

    assert_eq!(f.pr_commits(1), vec![head.clone()]);
    assert_eq!(f.git_ref(1), Some(head));
    assert_eq!(f.commit("@"), working_copy, "the working copy does not move");
    assert!(
        runner.events.borrow().contains(&"lock"),
        "the guarded writer takes the lock"
    );
}

#[test]
fn repair_command_repairs_an_obsolete_conflict() {
    let f = Fixture::new();
    let (head, _) = conflicted_pr_bookmark(&f, false);
    let runner = LeaseRunner::with_pr_json(&f.runner, head_json(&head));
    repair_command(&f, &runner).expect("obsolete conflict is repaired");
    assert_eq!(f.pr_commits(1), vec![head.clone()]);
    assert_eq!(f.git_ref(1), Some(head));
}

/// Unlike lease repair, which never fails a lease, the command fails loudly
/// and names the refusal, and leaves the unique side untouched.
#[test]
fn repair_command_refuses_a_conflict_with_unique_content() {
    let f = Fixture::new();
    let (head, other) = conflicted_pr_bookmark(&f, true);
    let runner = LeaseRunner::with_pr_json(&f.runner, head_json(&head));
    let err = repair_command(&f, &runner).expect_err("unique side must not be overridden");
    assert!(
        err.to_string().contains(git_utils::pr_bookmark::UNSAFE_REPAIR_PHRASE),
        "{err}"
    );
    assert_eq!(f.pr_commits(1).len(), 2, "still conflicted, no side picked");
    assert_eq!(f.git_ref(1), Some(other));
}

#[test]
fn repair_command_reports_a_response_without_a_head_sha() {
    let f = Fixture::new();
    let _ = conflicted_pr_bookmark(&f, false);
    let runner = LeaseRunner::with_pr_json(&f.runner, r#"{"state":"open","head":{"ref":"feat"}}"#.to_string());
    let err = repair_command(&f, &runner).expect_err("no head.sha");
    assert!(
        err.to_string().contains(git_utils::pr_bookmark::NO_PR_HEAD_PHRASE),
        "{err}"
    );
}

#[test]
fn repair_command_rejects_an_empty_or_malformed_head_sha() {
    let f = Fixture::new();
    let _ = conflicted_pr_bookmark(&f, false);
    for sha in [
        "",
        "abc123",
        "main",
        "z".repeat(40).as_str(),
        &format!("{}0", "a".repeat(40)),
    ] {
        let runner = LeaseRunner::with_pr_json(&f.runner, head_json(sha));
        let err = repair_command(&f, &runner).expect_err("malformed head.sha");
        assert!(
            err.to_string().contains(git_utils::pr_bookmark::NO_PR_HEAD_PHRASE),
            "{sha:?}: {err}"
        );
    }
}

#[test]
fn lease_repairs_from_head_sha_when_the_source_branch_is_gone() {
    let f = Fixture::new();
    let (head, _) = conflicted_pr_bookmark(&f, false);
    let runner = LeaseRunner::with_pr_json(
        &f.runner,
        format!(r#"{{"state":"closed","merged":true,"head":{{"ref":"deleted-branch","sha":"{head}"}}}}"#),
    );
    assert!(repair(&f, &runner).failures.is_empty());
    assert_eq!(f.pr_commits(1), vec![head]);
}

#[test]
fn lease_repairs_a_fork_headed_pr_from_head_sha() {
    let f = Fixture::new();
    let (head, _) = conflicted_pr_bookmark(&f, false);
    // `head.ref` names a branch on the fork; a same-named branch here is a
    // different commit and must not be consulted.
    let runner = LeaseRunner::with_pr_json(
        &f.runner,
        format!(r#"{{"state":"open","head":{{"ref":"main","sha":"{head}","repo":{{"full_name":"fork/repo"}}}}}}"#),
    );
    assert!(repair(&f, &runner).failures.is_empty());
    assert_eq!(f.pr_commits(1), vec![head]);
}

#[test]
fn lease_leaves_a_conflict_untouched_when_the_head_is_not_in_the_store() {
    let f = Fixture::new();
    let (_, other) = conflicted_pr_bookmark(&f, false);
    // The head repository has nothing to fetch: the fetch fails, the head stays
    // missing, and the bookmark is left alone.
    let missing = f._dir.path().join("no-such-fork");
    let runner = LeaseRunner::with_pr_json(
        &f.runner,
        LeaseRunner::fork_json("0123456789abcdef0123456789abcdef01234567", "feat", &missing),
    );
    assert_eq!(repair(&f, &runner).unrepaired(), ["pr/1"]);
    assert_eq!(f.pr_commits(1).len(), 2);
    assert!(f.pr_commits(1).contains(&other));
    assert!(
        !f.jj(&f.ws, &["git", "remote", "list"]).contains("cube-pr-fetch"),
        "the temporary remote is removed even when the fetch fails"
    );
    let sides = crate::app::pr_bookmarks::bookmark_commits_best_effort(&f.runner, None, &f.ws, "pr/1");
    assert!(
        sides.contains(&other) && sides.contains(", "),
        "the skip names both sides: {sides}"
    );
}

/// A fork's `feat` holding a commit `fork_sha` that this store has never seen,
/// as a child of the base repository's `feat`. The base repository's own
/// `feat@backup` stays at the older commit.
fn fork_with_new_head(f: &Fixture) -> (PathBuf, String) {
    let forge = f._dir.path().join("fork");
    f.jj(
        f._dir.path(),
        &[
            "git",
            "clone",
            "--colocate",
            f.root.to_str().unwrap(),
            forge.to_str().unwrap(),
        ],
    );
    f.jj(&forge, &["bookmark", "track", "feat@origin"]);
    f.jj(&forge, &["new", "feat", "-m", "fork work"]);
    std::fs::write(forge.join("fork.txt"), "fork\n").unwrap();
    f.jj(&forge, &["bookmark", "set", "feat", "-r", "@", "--allow-backwards"]);
    let sha = f.jj(&forge, &["log", "--no-graph", "-r", "feat", "-T", "commit_id"]);
    f.jj(&forge, &["new", "main@origin"]);
    if !f.jj(&f.ws, &["git", "remote", "list"]).contains("backup") {
        f.protect_feat_remotely();
    }
    assert_eq!(
        f.jj(
            &f.ws,
            &["log", "--no-graph", "-r", &format!("present({sha})"), "-T", "commit_id"]
        ),
        "",
        "precondition: the fork head is not in this store"
    );
    (forge, sha)
}

#[test]
fn lease_fetches_a_fork_head_missing_from_the_store_and_repairs_to_it() {
    let f = Fixture::new();
    conflicted_pr_bookmark(&f, false);
    let (forge, sha) = fork_with_new_head(&f);
    let runner = LeaseRunner::with_pr_json(&f.runner, LeaseRunner::fork_json(&sha, "feat", &forge));

    assert!(repair(&f, &runner).failures.is_empty());

    assert_eq!(f.pr_commits(1), vec![sha.clone()]);
    assert_eq!(f.git_ref(1), Some(sha));
    assert!(!f.jj(&f.ws, &["git", "remote", "list"]).contains("cube-pr-fetch"));
}

#[test]
fn lease_leaves_a_conflict_untouched_when_github_cannot_be_reached() {
    let f = Fixture::new();
    conflicted_pr_bookmark(&f, false);
    let runner = LeaseRunner::with_pr_json(&f.runner, "not json".to_string());
    assert_eq!(repair(&f, &runner).unrepaired(), ["pr/1"]);
    assert_eq!(f.pr_commits(1).len(), 2);
}

/// PR 1 was positioned at a head cube recorded, then rewritten in a separate
/// "forge" store and fetched, then (with `churn`) buried under hundreds of
/// operations so that no bounded op-log scan could still reach the old head. Returns `(published, rewritten)`.
fn pr_rewritten_outside_this_store(f: &Fixture, churn: bool) -> (String, String) {
    // The "forge" is a separate jj store with its own clone of the history.
    let forge = f._dir.path().join("forge");
    let parent = f._dir.path().to_path_buf();
    f.jj(
        &parent,
        &[
            "git",
            "clone",
            "--colocate",
            f.root.to_str().unwrap(),
            forge.to_str().unwrap(),
        ],
    );
    f.jj(&forge, &["bookmark", "track", "feat@origin"]);
    f.jj(&f.ws, &["git", "remote", "add", "forge", forge.to_str().unwrap()]);
    f.jj(&f.ws, &["git", "fetch", "--remote", "forge"]);
    point_pr_bookmark(&f.runner, None, &f.ws, 1, "feat@forge").expect("initial positioning");
    let published = f.commit("pr/1");
    crate::app::pr_bookmarks::record_head(&f.ws, 1, &published).unwrap();

    // Someone else (GitHub's "Update branch", a human's checkout, a worker on
    // another store) rewrites the PR branch to an unrelated commit.
    f.jj(&forge, &["new", "main@origin", "-m", "rewritten elsewhere"]);
    std::fs::write(forge.join("rewrite.txt"), "rewrite\n").unwrap();
    f.jj(&forge, &["bookmark", "set", "feat", "-r", "@", "--allow-backwards"]);
    f.jj(&forge, &["new", "main@origin"]);
    f.jj(&f.ws, &["git", "fetch", "--remote", "forge"]);
    let rewritten = f.commit("feat@forge");
    assert_ne!(rewritten, published);
    assert_eq!(
        f.jj(
            &f.ws,
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("{published} & ::{rewritten}"),
                "-T",
                "commit_id"
            ]
        ),
        "",
        "precondition: no local ancestry links the two heads"
    );

    // Ordinary activity in the shared store: remote operations from other
    // workspaces, then plain snapshots and rewrites.
    if churn {
        for _ in 0..65 {
            f.jj(&f.ws, &["git", "fetch", "--remote", "forge"]);
        }
        for i in 0..420 {
            f.jj(&f.ws, &["describe", "-m", &format!("churn {i}")]);
        }
    }
    (published, rewritten)
}

#[test]
fn point_replaces_a_head_that_was_rewritten_outside_this_store() {
    let f = Fixture::new();
    let (_, rewritten) = pr_rewritten_outside_this_store(&f, false);

    point_pr_bookmark(&f.runner, None, &f.ws, 1, "feat@forge").expect("superseded recorded head is replaced");

    assert_eq!(f.pr_commits(1), vec![rewritten.clone()]);
    assert_eq!(f.git_ref(1), Some(rewritten));
}

#[test]
fn goto_replaces_a_head_that_was_rewritten_outside_this_store() {
    let f = Fixture::new();
    let (_, rewritten) = pr_rewritten_outside_this_store(&f, true);
    f.protect_feat_remotely();
    let runner = LeaseRunner::with_pr_json(
        &f.runner,
        format!(r#"{{"state":"open","head":{{"ref":"feat","sha":"{rewritten}"}}}}"#),
    );

    crate::app::workspace_ops::workspace_goto(None, &runner, Some(f.ws.display().to_string()), None, Some(1), None)
        .expect("goto --pr replaces the superseded head");

    assert_eq!(f.pr_commits(1), vec![rewritten.clone()]);
    assert_eq!(f.git_ref(1), Some(rewritten.clone()));
    assert_eq!(f.commit("@-"), rewritten, "@ sits directly on the PR head");
}

/// Content that reached some unrelated remote branch is not evidence that a
/// conflicting side was this PR's head, whether or not that branch still exists.
#[test]
fn a_side_published_only_on_an_unrelated_branch_is_not_obsolete() {
    let f = Fixture::new();
    let (head, other) = conflicted_pr_bookmark(&f, true);
    f.jj(&f.root, &["bookmark", "create", "wip", "-r", &other]);
    f.root_write();
    f.jj(&f.ws, &["git", "fetch", "--remote", "backup"]);
    let wip_remote = r#"remote_bookmarks(exact:"wip", exact:"backup")"#;
    assert_eq!(
        f.commit(wip_remote),
        other,
        "precondition: published on the unrelated branch"
    );
    let before = f.jj(&f.ws, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);

    let err = point_pr_bookmark(&f.runner, None, &f.ws, 1, &head).expect_err("published on wip only");
    assert!(err.to_string().contains("will not pick a side"), "{err}");
    assert_eq!(f.jj(&f.ws, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]), before);

    // The backup branch is deleted and the deletion fetched: the commit was
    // never tied to this PR in the first place.
    f.jj(&f.root, &["bookmark", "delete", "wip"]);
    f.root_write();
    f.jj(&f.ws, &["git", "fetch", "--remote", "backup"]);
    assert_eq!(f.commit(wip_remote), "", "precondition: the branch is gone");
    let before = f.jj(&f.ws, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    let err = point_pr_bookmark(&f.runner, None, &f.ws, 1, &head).expect_err("branch deleted");
    assert!(err.to_string().contains("will not pick a side"), "{err}");
    assert_eq!(f.jj(&f.ws, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]), before);

    assert_eq!(f.pr_commits(1).len(), 2, "still conflicted, no side picked");
    assert!(f.pr_commits(1).contains(&other));
    let after = f.jj(&f.ws, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    assert_eq!(after, before, "refused writes leave no bookmark change");
}

#[test]
fn goto_positions_a_fork_pr_on_head_sha_not_the_same_named_base_branch() {
    let f = Fixture::new();
    let (forge, sha) = fork_with_new_head(&f);
    let base_feat = f.commit("feat");
    let runner = LeaseRunner::with_pr_json(&f.runner, LeaseRunner::fork_json(&sha, "feat", &forge));

    crate::app::workspace_ops::workspace_goto(None, &runner, Some(f.ws.display().to_string()), None, Some(1), None)
        .expect("goto --pr positions on the fork head");

    assert_eq!(f.pr_commits(1), vec![sha.clone()]);
    assert_eq!(f.git_ref(1), Some(sha.clone()));
    assert_eq!(f.commit("@-"), sha);
    assert_eq!(
        f.commit("feat"),
        base_feat,
        "the base repo's `feat` is not moved onto the fork"
    );
}

/// The repository-level fixture the lease entry point needs: two pool
/// workspaces of one real store, both reporting the same conflicted `pr/1`.
struct LeaseEntry {
    f: Fixture,
    _db_dir: TempDir,
    db: PathBuf,
    sibling: PathBuf,
}

impl LeaseEntry {
    fn new() -> Self {
        let f = Fixture::with_ws_path("workspaces/mono-agent-001");
        let sibling = f._dir.path().join("workspaces/mono-agent-002");
        f.jj(
            &f.root,
            &[
                "workspace",
                "add",
                sibling.to_str().unwrap(),
                "--name",
                "mono-agent-002",
                "-r",
                "main",
            ],
        );
        let (db_dir, db) = super::support::with_database_path();
        super::support::seed_mono_repo(&f._dir.path().join("workspaces"), &db);
        Self {
            f,
            _db_dir: db_dir,
            db,
            sibling,
        }
    }

    fn lease(&self, head: &str) -> crate::app::errors::Result<crate::app::errors::RunResult> {
        let runner = LeaseRunner::with_pr_json(
            &self.f.runner,
            format!(r#"{{"state":"open","head":{{"ref":"feat","sha":"{head}"}}}}"#),
        );
        let lease =
            <crate::cli::Cli as clap::Parser>::parse_from(["cube", "workspace", "lease", "mono", "--task", "t"]);
        crate::app::dispatch::run_with_dependencies(lease, Some(&self.db), &runner)
    }

    fn cached_health(&self, workspace_id: &str) -> Option<String> {
        rusqlite::Connection::open(&self.db)
            .expect("sqlite")
            .query_row(
                "SELECT health_status FROM workspaces WHERE workspace_id = ?1",
                [workspace_id],
                |row| row.get(0),
            )
            .expect("cached health row")
    }
}

#[test]
fn lease_entry_point_leaves_a_unique_conflict_alone_and_keeps_the_health_cache() {
    let l = LeaseEntry::new();
    let f = &l.f;
    let (head, other) = conflicted_pr_bookmark(f, true);
    assert!(
        f.jj(&l.sibling, &["status"]).contains("pr/1"),
        "the sibling sees the conflict too"
    );

    let result = l.lease(&head).expect("a refused repair must not fail the lease");

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-001");
    assert_eq!(f.pr_commits(1).len(), 2, "no side picked");
    assert!(f.pr_commits(1).contains(&other));
    assert_eq!(f.git_ref(1), Some(other), "the git ref is untouched");
    assert_eq!(
        l.cached_health("mono-agent-002").as_deref(),
        Some("conflicted"),
        "the conflict is still there, so the sibling's cached verdict stays"
    );
}

#[test]
fn lease_entry_point_repairs_an_obsolete_conflict_and_clears_the_health_cache() {
    let l = LeaseEntry::new();
    let f = &l.f;
    let (head, _) = conflicted_pr_bookmark(f, false);

    let result = l.lease(&head).expect("lease");

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-001");
    assert_eq!(f.pr_commits(1), vec![head.clone()]);
    assert_eq!(f.git_ref(1), Some(head));
    assert_eq!(
        l.cached_health("mono-agent-002"),
        None,
        "the repair cleared the sibling's cached verdict"
    );
}

#[test]
fn gc_fetches_before_taking_the_pr_bookmark_lock() {
    let f = Fixture::new();
    f.protect_feat_remotely();
    let runner = LeaseRunner::new(&f.runner);
    crate::app::gc::gc_workspace_bookmarks(&runner, None, &f.ws, true, false).unwrap();
    let events = runner.events.borrow().clone();
    assert_eq!(
        events,
        ["network", "lock"],
        "fetch must finish before the lock is taken"
    );
}

#[test]
fn gc_retains_whole_conflict_when_one_side_is_in_use() {
    let f = Fixture::new();
    let (old, head) = f.rewritten_feat_with_exported_pr_ref();
    f.jj(&f.ws, &["bookmark", "forget", "pr/1"]);
    f.jj(&f.ws, &["bookmark", "set", "pr/1", "-r", &head]);
    f.root_write();
    f.jj(&f.root, &["bookmark", "create", "archive", "-r", &old]);
    f.protect_feat_remotely();
    f.jj(&f.ws, &["new", &head]);
    assert_eq!(f.pr_commits(1).len(), 2);
    let collected = crate::app::gc::gc_workspace_bookmarks(&f.runner, None, &f.ws, false, false).unwrap();
    assert!(!collected.contains(&"pr/1".to_string()));
    assert_eq!(f.pr_commits(1).len(), 2);
    f.jj(&f.ws, &["new", "main"]);
    let collected = crate::app::gc::gc_workspace_bookmarks(&f.runner, None, &f.ws, false, false).unwrap();
    assert!(
        collected.contains(&"pr/1".to_string()),
        "both sides protected and now unused"
    );
    assert!(f.pr_commits(1).is_empty());
}

#[test]
fn publication_writers_export_from_secondary_workspace() {
    let f = Fixture::new();
    crate::app::pr::record_published_pr_head(&f.runner, &f.ws, "feat", Some(1), &f.commit("feat")).unwrap();
    assert_eq!(f.git_ref(1), Some(f.commit("feat")));
    f.jj(&f.ws, &["new", "feat", "-m", "next revision"]);
    std::fs::write(f.ws.join("next.txt"), "next\n").unwrap();
    crate::app::pr::advance_pr_bookmarks(&f.runner, &f.ws, "feat", "pr/1").unwrap();
    let head = f.commit("@");
    assert_eq!(f.pr_commits(1), vec![head.clone()]);
    assert_eq!(f.git_ref(1), Some(head));
}

struct PostReadRunner<'a> {
    runner: &'a DeclaredJj,
    reads: std::cell::Cell<usize>,
    old: String,
    head: String,
}

impl CommandRunner for PostReadRunner<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        if invocation.args.iter().any(|arg| arg == "bookmarks(exact:\"pr/1\")") {
            self.reads.set(self.reads.get() + 1);
            if self.reads.get() == 2 {
                return Ok(format!("{}\n{}", self.old, self.head));
            }
        }
        self.runner.run(invocation)
    }
}

#[test]
fn obsolete_side_in_post_read_gets_one_bounded_repair() {
    let f = Fixture::new();
    let (old, head) = f.rewritten_feat_with_exported_pr_ref();
    let runner = PostReadRunner {
        runner: &f.runner,
        reads: std::cell::Cell::new(0),
        old,
        head: head.clone(),
    };
    point_pr_bookmark(&runner, None, &f.ws, 1, "feat").unwrap();
    assert_eq!(runner.reads.get(), 3, "one final read after the repair");
    assert_eq!(f.pr_commits(1), vec![head.clone()]);
    assert_eq!(f.git_ref(1), Some(head));
}

struct WaitingGcRunner {
    runner: DeclaredJj,
    waiting: std::sync::mpsc::Sender<()>,
    lock_path: PathBuf,
}

impl CommandRunner for WaitingGcRunner {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        self.runner.run(invocation)
    }
    fn lock_pr_bookmarks(&self, cwd: &Path) -> Result<crate::lock::RepoLock> {
        assert!(
            crate::lock::RepoLock::try_acquire(&self.lock_path)?.is_none(),
            "goto must still hold the lock after setting its bookmark"
        );
        self.waiting.send(()).unwrap();
        crate::lock::RepoLock::for_workspace(cwd)
    }
}

struct PausingGotoRunner {
    runner: DeclaredJj,
    head: String,
    positioned_bookmark: std::sync::mpsc::Sender<()>,
    proceed: std::sync::mpsc::Receiver<()>,
}

impl CommandRunner for PausingGotoRunner {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        if invocation.args.iter().any(|arg| *arg == format!("{} & ::@", self.head)) {
            self.positioned_bookmark.send(()).unwrap();
            self.proceed.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        }
        LeaseRunner::with_pr_json(
            &self.runner,
            format!(r#"{{"state":"open","head":{{"ref":"feat","sha":"{}"}}}}"#, self.head),
        )
        .run(invocation)
    }
}

#[test]
fn gc_waits_until_positioning_has_made_the_bookmark_reachable() {
    let f = Fixture::new();
    f.protect_feat_remotely();
    let (positioned_tx, positioned_rx) = std::sync::mpsc::channel();
    let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
    let goto_runner = PausingGotoRunner {
        runner: DeclaredJj {
            binary: f.runner.binary.clone(),
            config: f.runner.config.clone(),
        },
        head: f.commit("feat"),
        positioned_bookmark: positioned_tx,
        proceed: proceed_rx,
    };
    let ws = f.ws.display().to_string();
    let goto = std::thread::spawn(move || {
        crate::app::workspace_ops::workspace_goto(None, &goto_runner, Some(ws), None, Some(1), None)
    });
    positioned_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let runner = WaitingGcRunner {
        runner: DeclaredJj {
            binary: f.runner.binary.clone(),
            config: f.runner.config.clone(),
        },
        waiting: tx,
        lock_path: f.root.join(".jj/repo/cube-pr-bookmarks.lock"),
    };
    let root = f.root.clone();
    let gc = std::thread::spawn(move || crate::app::gc::gc_workspace_bookmarks(&runner, None, &root, false, false));
    rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    // GC is attempting the lock from a different workspace in the same store.
    // Finish goto's final step before releasing that shared critical section.
    proceed_tx.send(()).unwrap();
    goto.join().unwrap().unwrap();
    assert!(!gc.join().unwrap().unwrap().contains(&"pr/1".to_string()));
    assert_eq!(f.pr_commits(1), vec![f.commit("feat")]);
}

/// Delegates to a [`LeaseRunner`] but fails every bounded PR-bookmark lock
/// wait, as a stalled holder would.
struct ContendedLock<'a>(LeaseRunner<'a>);

impl CommandRunner for ContendedLock<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        self.0.run(invocation)
    }

    fn lock_pr_bookmarks_within(&self, _cwd: &Path, _timeout: std::time::Duration) -> Result<crate::lock::RepoLock> {
        Err(CubeError::InvalidArgument(
            "timed out waiting for the PR-bookmark lock".into(),
        ))
    }
}

#[test]
fn lease_survives_a_contended_pr_bookmark_lock() {
    let f = Fixture::new();
    let (head, _) = conflicted_pr_bookmark(&f, false);
    let runner = ContendedLock(LeaseRunner::with_pr_json(&f.runner, head_json(&head)));
    let repair = crate::app::health::repair_conflicted_bookmarks(&runner, None, &f.ws, &["pr/1".to_string()])
        .expect("lock contention must not fail the lease");
    assert_eq!(repair.unrepaired(), ["pr/1"]);
    assert_eq!(f.pr_commits(1).len(), 2, "left conflicted and untouched");
}

/// A recording runner for the ambient deadline: it never runs anything, only
/// reports the timeout each call was given.
struct TimeoutProbe(std::cell::RefCell<Vec<std::time::Duration>>);

impl CommandRunner for TimeoutProbe {
    fn run(&self, _invocation: &CommandInvocation) -> Result<String> {
        unreachable!("run_jj goes through run_with_timeout")
    }

    fn run_with_timeout(&self, _invocation: &CommandInvocation, timeout: std::time::Duration) -> Result<String> {
        self.0.borrow_mut().push(timeout);
        Ok(String::new())
    }
}

#[test]
fn scoped_deadline_bounds_every_jj_call_even_without_an_explicit_one() {
    let probe = TimeoutProbe(Default::default());
    let call = || {
        crate::app::jj::run_jj(
            &probe,
            None,
            &RealCommandRunner::invocation(Path::new("/"), "jj", &["git", "import"]),
        )
    };
    {
        let _scope = crate::app::jj::scope_deadline(std::time::Instant::now() + std::time::Duration::from_secs(5));
        call().unwrap();
        assert!(probe.0.borrow()[0] <= std::time::Duration::from_secs(5));
    }
    call().unwrap();
    assert!(
        probe.0.borrow()[1] > std::time::Duration::from_secs(5),
        "scope restored"
    );
    let _expired = crate::app::jj::scope_deadline(std::time::Instant::now());
    assert!(matches!(call(), Err(CubeError::DeadlineExceeded { .. })));
    assert_eq!(probe.0.borrow().len(), 2, "nothing spawned past the deadline");
}
