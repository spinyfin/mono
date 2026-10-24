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

/// Runs `jj` invocations with the Bazel-declared binary and a fixed identity,
/// so the fixture needs no global jj config and cannot read the host's.
struct DeclaredJj {
    binary: PathBuf,
    config: PathBuf,
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
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("jj-config.toml");
        std::fs::write(
            &config,
            "[user]\nname = \"Cube Test\"\nemail = \"cube@example.invalid\"\n",
        )
        .unwrap();
        let root = dir.path().join("root");
        let ws = dir.path().join("ws");
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
            &["workspace", "add", ws.to_str().unwrap(), "--name", "ws", "-r", "main"],
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
struct LeaseRunner<'a>(&'a DeclaredJj);

impl CommandRunner for LeaseRunner<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        if invocation.program == "gh" {
            assert_eq!(invocation.args, ["api", "repos/example/repo/pulls/1"]);
            return Ok(r#"{"state":"open","head":{"ref":"feat"}}"#.to_string());
        }
        if invocation.args == ["git", "remote", "list"] {
            return Ok("backup https://github.com/example/repo".to_string());
        }
        self.0.run(invocation)
    }
}

#[test]
fn lease_repairs_obsolete_conflict_and_preserves_unique_conflict() {
    for unique in [false, true] {
        let f = Fixture::new();
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
        let result = crate::app::health::repair_conflicted_bookmarks(
            &LeaseRunner(&f.runner),
            None,
            &f.ws,
            &["pr/1".to_string()],
        );
        if unique {
            let error = result.unwrap_err().to_string();
            assert!(error.contains(&head) && error.contains(&other), "{error}");
            assert_eq!(f.pr_commits(1).len(), 2);
            assert_eq!(f.git_ref(1), Some(other));
        } else {
            result.unwrap();
            assert_eq!(f.pr_commits(1), vec![head.clone()]);
            assert_eq!(f.git_ref(1), Some(head));
        }
    }
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
    crate::app::pr::set_pr_bookmark(&f.runner, &f.ws, Some(1), "feat").unwrap();
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
    positioned_bookmark: std::sync::mpsc::Sender<()>,
    proceed: std::sync::mpsc::Receiver<()>,
}

impl CommandRunner for PausingGotoRunner {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        if invocation.args.iter().any(|arg| arg == "feat@backup & ::@") {
            self.positioned_bookmark.send(()).unwrap();
            self.proceed.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        }
        LeaseRunner(&self.runner).run(invocation)
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
