//! `pr/<n>` bookmark lifecycle against a real colocated jj repo.
//!
//! The stale-git-ref cycle only exists in the interplay between jj's
//! import/export bookkeeping and the colocated `.git`, so a scripted
//! `FakeRunner` cannot show it. These tests run the declared `jj` (and `git`,
//! for ref inspection) against a throwaway colocated repo plus one secondary
//! workspace in a temp dir — never cube's live shared store, never cube
//! itself, and never the test binary.

use std::path::{Path, PathBuf};
use std::process::Command;

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

    fn git(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .expect("git");
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    fn git_ref(&self, n: u64) -> Option<String> {
        self.git(&["rev-parse", "--verify", "-q", &format!("refs/heads/pr/{n}")])
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
    // The only operation allowed between the two reads is the import that
    // runs first; the bookmark itself was not rewritten.
    let after = f.jj(&f.ws, &["op", "log", "--no-graph", "-n", "1", "-T", "description"]);
    assert!(
        !after.contains("set bookmark") && !after.contains("point bookmark"),
        "{after} ({before})"
    );
}
