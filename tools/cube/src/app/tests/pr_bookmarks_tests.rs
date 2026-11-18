//! PR heads and bookmark sweeps against a real colocated jj repo.
//!
//! Cube keeps no per-PR pointer: `goto --pr` positions on the commit GitHub
//! reports as the PR head and writes no `pr/<n>` bookmark. These tests run the
//! declared `jj` against a throwaway colocated repo plus one secondary
//! workspace in a temp dir — never cube's live shared store, never cube
//! itself, and never the test binary — to show that bookmarks cube does not own
//! are neither created nor touched, including conflicted ones.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::app::errors::{CubeError, Result};
use crate::command_runner::{CommandInvocation, CommandRunner, RealCommandRunner};

#[path = "store_gc_concurrency_tests.rs"]
mod store_gc_concurrency;

#[path = "store_gc_predecessor_tests.rs"]
mod store_gc_predecessor;

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
        self.bookmark_commits(&format!("pr/{n}"))
    }

    fn conflicted(&self) -> String {
        self.jj(&self.ws, &["bookmark", "list", "--conflicted"])
    }

    fn bookmark_commits(&self, name: &str) -> Vec<String> {
        let mut commits: Vec<String> = self
            .jj(
                &self.ws,
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    &format!("bookmarks(exact:\"{name}\")"),
                    "-T",
                    r#"commit_id ++ "\n""#,
                ],
            )
            .lines()
            .map(str::to_owned)
            .collect();
        commits.sort();
        commits
    }

    /// `name` conflicted between commits `a` and `b`, the way the incidents
    /// produced it: the git ref left at `a` by a forget, the local bookmark
    /// recreated at `b`, then an import at the colocated root.
    fn conflict_bookmark(&self, name: &str, a: &str, b: &str) {
        self.jj(&self.root, &["bookmark", "create", name, "-r", a]);
        self.root_write();
        assert_eq!(self.git_ref(name).as_deref(), Some(a), "{name} exported at the root");
        self.jj(&self.ws, &["bookmark", "forget", name]);
        self.jj(&self.ws, &["bookmark", "create", name, "-r", b]);
        self.root_write();
        assert_eq!(self.bookmark_commits(name).len(), 2, "{name} is conflicted");
    }

    /// A non-empty commit that no remote holds. It hangs off the root rather
    /// than `main`: jj resolves a bookmark conflict between a commit and its
    /// descendant to the descendant, so conflict sides must not be related by
    /// ancestry.
    fn unpublished_commit(&self) -> String {
        self.jj(&self.ws, &["new", "root()", "-m", "unpublished"]);
        std::fs::write(self.ws.join("unpublished.txt"), "keep me\n").unwrap();
        self.jj(&self.ws, &["new", "main"]);
        self.commit("description(exact:\"unpublished\\n\")")
    }

    /// A non-empty commit off the root that a remote branch holds.
    fn published_commit(&self) -> String {
        self.jj(&self.ws, &["new", "root()", "-m", "published"]);
        std::fs::write(self.ws.join("published.txt"), "shared\n").unwrap();
        self.jj(&self.ws, &["new", "main"]);
        let commit = self.commit("description(exact:\"published\\n\")");
        self.jj(&self.root, &["bookmark", "create", "pub", "-r", &commit]);
        if self.jj(&self.ws, &["git", "remote", "list"]).contains("backup") {
            self.jj(&self.ws, &["git", "fetch", "--remote", "backup"]);
        } else {
            self.protect_feat_remotely();
        }
        commit
    }

    fn git_ref(&self, name: &str) -> Option<String> {
        // The fixture creates only loose refs, so read the actual exported
        // ref without invoking an undeclared host git.
        match std::fs::read_to_string(self.root.join(format!(".git/refs/heads/{name}"))) {
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
}

/// Only GitHub's metadata lookup is simulated; all repository operations,
/// including fetching the authoritative remote, run through declared jj.
struct GithubRunner<'a> {
    jj: &'a DeclaredJj,
    pr_json: String,
}

impl<'a> GithubRunner<'a> {
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
        Self { jj, pr_json }
    }
}

impl CommandRunner for GithubRunner<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        if invocation.program == "gh" {
            assert_eq!(invocation.args, ["api", "repos/example/repo/pulls/1"]);
            return Ok(self.pr_json.clone());
        }
        if invocation.args == ["git", "remote", "list"] {
            return Ok("backup https://github.com/example/repo".to_string());
        }
        self.jj.run(invocation)
    }
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

/// The repository-level fixture the lease entry point needs: two pool
/// workspaces of one real store, both reporting the same conflicted bookmarks.
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

    fn lease(&self) -> crate::app::errors::Result<crate::app::errors::RunResult> {
        let runner = GithubRunner::new(&self.f.runner);
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

fn goto_pr(f: &Fixture, pr_json: String) {
    let runner = GithubRunner::with_pr_json(&f.runner, pr_json);
    crate::app::workspace_ops::workspace_goto(None, &runner, Some(f.ws.display().to_string()), None, Some(1), None)
        .expect("goto --pr positions on the PR head");
}

#[test]
fn goto_pr_positions_on_the_head_sha_and_creates_no_pr_alias() {
    let f = Fixture::new();
    f.protect_feat_remotely();
    let head = f.commit("feat");

    goto_pr(
        &f,
        format!(r#"{{"state":"open","head":{{"ref":"feat","sha":"{head}"}}}}"#),
    );

    assert_eq!(f.commit("@-"), head, "@ sits directly on the PR head");
    assert_eq!(f.commit("feat"), head, "the head branch tracks the PR head");
    assert!(f.pr_commits(1).is_empty(), "no pr/1 bookmark is created");
    assert_eq!(f.git_ref("pr/1"), None, "no pr/1 git ref is exported");
    assert!(f.conflicted().trim().is_empty(), "{}", f.conflicted());
}

#[test]
fn goto_pr_positions_a_fork_pr_on_head_sha_not_the_same_named_base_branch() {
    let f = Fixture::new();
    let (forge, sha) = fork_with_new_head(&f);
    let base_feat = f.commit("feat");

    goto_pr(&f, GithubRunner::fork_json(&sha, "feat", &forge));

    assert_eq!(f.commit("@-"), sha);
    assert_eq!(
        f.commit("feat"),
        base_feat,
        "the base repo's `feat` is not moved onto the fork"
    );
    assert!(f.pr_commits(1).is_empty(), "no pr/1 bookmark is created");
    assert_eq!(f.git_ref("pr/1"), None);
    assert!(
        !f.jj(&f.ws, &["git", "remote", "list"]).contains("cube-pr-fetch"),
        "the temporary remote is removed"
    );
}

/// Conflicted bookmarks cube does not own: a legacy `pr/<n>` alias, an engine
/// pointer, and a user branch. Returns their names and sides.
fn conflict_unrelated_bookmarks(f: &Fixture) -> Vec<(&'static str, Vec<String>)> {
    let unpublished = f.unpublished_commit();
    let main = f.commit("main");
    let feat = f.commit("feat");
    let mut state = Vec::new();
    for (name, a, b) in [
        ("pr/7", &feat, &unpublished),
        ("boss-recovery/exec_x", &unpublished, &main),
        ("user-branch", &feat, &unpublished),
    ] {
        f.conflict_bookmark(name, a, b);
        state.push((name, f.bookmark_commits(name)));
    }
    state
}

fn assert_bookmarks_untouched(f: &Fixture, before: &[(&'static str, Vec<String>)]) {
    for (name, sides) in before {
        assert_eq!(&f.bookmark_commits(name), sides, "{name} is untouched");
    }
}

#[test]
fn goto_pr_leaves_unrelated_conflicted_bookmarks_untouched() {
    let f = Fixture::new();
    f.protect_feat_remotely();
    let before = conflict_unrelated_bookmarks(&f);
    let head = f.commit("feat");

    goto_pr(
        &f,
        format!(r#"{{"state":"open","head":{{"ref":"feat","sha":"{head}"}}}}"#),
    );

    assert_eq!(f.commit("@-"), head);
    assert_bookmarks_untouched(&f, &before);
    assert!(f.pr_commits(1).is_empty(), "no pr/1 alias is created");
}

#[test]
fn rebase_moves_the_branch_and_touches_no_other_bookmark() {
    let f = Fixture::new();
    // Conflicted bookmarks off the rebased branch: none of their sides moves.
    let unpublished = f.unpublished_commit();
    let old_main = f.commit("main");
    f.conflict_bookmark("pr/7", &old_main, &unpublished);
    f.conflict_bookmark("boss-recovery/exec_x", &unpublished, &old_main);
    let before: Vec<(&'static str, Vec<String>)> = ["pr/7", "boss-recovery/exec_x"]
        .into_iter()
        .map(|name| (name, f.bookmark_commits(name)))
        .collect();
    // main advances; `feat` is rebased onto it.
    f.jj(&f.root, &["new", "main", "-m", "main moves on"]);
    std::fs::write(f.root.join("c.txt"), "c\n").unwrap();
    f.jj(&f.root, &["bookmark", "set", "main", "-r", "@"]);
    f.jj(&f.root, &["new", "main"]);
    let main = f.commit("main");

    crate::app::jj::rebase_branch_onto(&f.runner, None, &f.ws, "feat", "main").expect("rebase");

    assert_eq!(f.commit("feat-"), main, "feat now sits directly on the new main");
    assert_bookmarks_untouched(&f, &before);
    assert!(f.pr_commits(1).is_empty(), "no pr/1 alias is created");
    assert_eq!(f.git_ref("pr/1"), None);
}

#[test]
fn bookmark_sweep_leaves_unrelated_conflicted_bookmarks_untouched() {
    let f = Fixture::new();
    let before = conflict_unrelated_bookmarks(&f);

    let swept = crate::app::gc::gc_workspace_bookmarks(&f.runner, None, &f.ws, false, false).unwrap();

    assert!(swept.is_empty(), "{swept:?}");
    assert_bookmarks_untouched(&f, &before);
}

#[test]
fn sweep_forgets_a_merged_exec_bookmark() {
    let f = Fixture::new();
    let main = f.commit("main");
    f.jj(&f.ws, &["bookmark", "create", "boss/exec_merged", "-r", &main]);

    let swept = crate::app::gc::gc_workspace_bookmarks(&f.runner, None, &f.ws, false, false).unwrap();

    assert_eq!(swept, ["boss/exec_merged"]);
    assert!(f.bookmark_commits("boss/exec_merged").is_empty());
}

#[test]
fn sweep_keeps_a_conflicted_exec_bookmark_when_a_side_has_unpublished_work() {
    let f = Fixture::new();
    let main = f.commit("main");
    let unpublished = f.unpublished_commit();
    f.conflict_bookmark("boss/exec_split", &main, &unpublished);
    let before = f.bookmark_commits("boss/exec_split");

    let swept = crate::app::gc::gc_workspace_bookmarks(&f.runner, None, &f.ws, false, false).unwrap();

    assert!(
        swept.is_empty(),
        "the merged side must not license forgetting: {swept:?}"
    );
    assert_eq!(f.bookmark_commits("boss/exec_split"), before, "both sides survive");
    assert!(before.contains(&unpublished));
}

#[test]
fn sweep_keeps_a_conflicted_exec_bookmark_over_an_empty_tip_atop_unpublished_work() {
    let f = Fixture::new();
    let main = f.commit("main");
    let unpublished = f.unpublished_commit();
    f.jj(&f.ws, &["new", &unpublished, "-m", "empty tip"]);
    let tip = f.commit("@");
    f.jj(&f.ws, &["new", "main"]);
    f.conflict_bookmark("boss/exec_tip", &main, &tip);
    let before = f.bookmark_commits("boss/exec_tip");

    let swept = crate::app::gc::gc_workspace_bookmarks(&f.runner, None, &f.ws, false, false).unwrap();

    assert!(swept.is_empty(), "{swept:?}");
    assert_eq!(f.bookmark_commits("boss/exec_tip"), before);
}

#[test]
fn sweep_forgets_a_conflicted_exec_bookmark_when_every_side_is_merged_published_or_empty() {
    let f = Fixture::new();
    let main = f.commit("main");
    let published = f.published_commit();
    f.conflict_bookmark("boss/exec_published", &main, &published);
    f.jj(&f.ws, &["new", "root()", "-m", "empty"]);
    let empty = f.commit("@");
    f.jj(&f.ws, &["new", "main"]);
    f.conflict_bookmark("boss/exec_empty", &main, &empty);

    let mut swept = crate::app::gc::gc_workspace_bookmarks(&f.runner, None, &f.ws, false, false).unwrap();

    swept.sort();
    assert_eq!(swept, ["boss/exec_empty", "boss/exec_published"]);
    assert!(f.bookmark_commits("boss/exec_published").is_empty());
    assert!(f.bookmark_commits("boss/exec_empty").is_empty());
}

#[test]
fn lease_with_unrelated_conflicted_bookmarks_touches_none_and_marks_no_workspace() {
    let l = LeaseEntry::new();
    l.f.protect_feat_remotely();
    let before = conflict_unrelated_bookmarks(&l.f);
    assert!(
        l.f.jj(&l.sibling, &["status"]).contains("pr/7"),
        "the sibling sees the conflict too"
    );

    let result = l.lease().expect("a store-wide bookmark conflict must not fail a lease");

    assert_eq!(result.payload["workspace"]["workspace_id"], "mono-agent-001");
    assert_bookmarks_untouched(&l.f, &before);
    assert!(l.f.pr_commits(1).is_empty());
    assert_ne!(
        l.cached_health("mono-agent-002").as_deref(),
        Some("conflicted"),
        "a repository-wide bookmark conflict is not damage to a workspace"
    );
    assert_ne!(l.cached_health("mono-agent-001").as_deref(), Some("conflicted"));
}

// ── PR head resolution: pull-ref fallback, fork fetches, temporary remotes ──

/// Two processes bringing the same fork head in at once each add their own
/// temporary remote, fetch under the shared-store lock, and leave none
/// behind. The remote additions themselves are serialized by the store's
/// remote-change lock (unserialized, jj's fail-fast git config lock makes one
/// of them fail), so the two sides meet after both observe the head is missing.
#[test]
fn concurrent_fork_fetches_use_distinct_remotes() {
    /// A two-party meeting point with a bound: a side whose partner never
    /// arrives fails the test instead of hanging the binary.
    struct Rendezvous {
        arrived: std::sync::Mutex<usize>,
        complete: std::sync::Condvar,
    }
    impl Rendezvous {
        fn wait(&self) {
            let mut arrived = self.arrived.lock().unwrap();
            *arrived += 1;
            if *arrived >= 2 {
                self.complete.notify_all();
                return;
            }
            let (_arrived, result) = self
                .complete
                .wait_timeout_while(arrived, std::time::Duration::from_secs(30), |arrived| *arrived < 2)
                .unwrap();
            assert!(
                !result.timed_out(),
                "the other fetch never reached its temporary remote"
            );
        }
    }
    struct ConcurrentFetch<'a> {
        jj: &'a DeclaredJj,
        added: std::sync::Mutex<Vec<String>>,
        both_fetching: Rendezvous,
    }
    impl CommandRunner for ConcurrentFetch<'_> {
        fn run(&self, invocation: &CommandInvocation) -> Result<String> {
            if invocation
                .args
                .starts_with(&["git".into(), "remote".into(), "add".into()])
            {
                self.added.lock().unwrap().push(invocation.args[3].clone());
            }
            let output = self.jj.run(invocation)?;
            if invocation.args.first().is_some_and(|arg| arg == "log")
                && invocation.args.iter().any(|arg| arg.starts_with("present("))
                && output.trim().is_empty()
            {
                // Both callers observe the missing head before either fetches.
                self.both_fetching.wait();
            }
            Ok(output)
        }
    }
    let f = Fixture::new();
    let (forge, sha) = fork_with_new_head(&f);
    let info = serde_json::from_str(&GithubRunner::fork_json(&sha, "feat", &forge)).unwrap();
    let runner = ConcurrentFetch {
        jj: &f.runner,
        added: Default::default(),
        both_fetching: Rendezvous {
            arrived: std::sync::Mutex::new(0),
            complete: std::sync::Condvar::new(),
        },
    };
    std::thread::scope(|scope| {
        let resolve = || {
            crate::app::pr_head::resolve_pr_head(&runner, None, &f.ws, "example/repo", 1, &info, None)
                .unwrap()
                .sha
        };
        let first = scope.spawn(resolve);
        let second = scope.spawn(resolve);
        assert_eq!(first.join().unwrap(), sha);
        assert_eq!(second.join().unwrap(), sha);
    });
    // Serialized fetches need exactly one distinct remote per caller.
    let names = runner.added.lock().unwrap();
    assert_eq!(names.len(), 2, "{names:?}");
    let distinct: std::collections::BTreeSet<_> = names.iter().collect();
    assert_eq!(distinct.len(), names.len(), "remote names are unique: {names:?}");
    assert!(!f.jj(&f.ws, &["git", "remote", "list"]).contains("cube-pr-fetch-"));
}

#[test]
fn deleted_fork_does_not_move_the_local_main_bookmark() {
    let f = Fixture::new();
    f.protect_feat_remotely();
    let main = f.commit("main");
    let head = f.commit("feat");
    let runner = GithubRunner::with_pr_json(
        &f.runner,
        format!(r#"{{"state":"open","head":{{"ref":"main","sha":"{head}","repo":null}}}}"#,),
    );
    crate::app::workspace_ops::workspace_goto(None, &runner, Some(f.ws.display().to_string()), None, Some(1), None)
        .unwrap();
    assert_eq!(f.commit("main"), main);
    assert_eq!(f.commit("@-"), head);
}

/// A final PR head that only the base repository's `refs/pull/<n>/head`
/// still serves: the source branch was deleted on merge and the fork is gone.
fn base_repo_serving_only_a_pull_ref(f: &Fixture) -> String {
    let base = f._dir.path().join("base");
    f.jj(
        f._dir.path(),
        &[
            "git",
            "clone",
            "--colocate",
            f.root.to_str().unwrap(),
            base.to_str().unwrap(),
        ],
    );
    f.jj(&base, &["bookmark", "track", "feat@origin"]);
    f.jj(&base, &["new", "feat", "-m", "squashed final head"]);
    std::fs::write(base.join("final.txt"), "final\n").unwrap();
    f.jj(&base, &["bookmark", "create", "tip", "-r", "@"]);
    let head = f.jj(&base, &["log", "--no-graph", "-r", "tip", "-T", "commit_id"]);
    f.jj(&base, &["new", "main@origin"]);
    // GitHub keeps this ref after the branch is deleted; the fixture creates
    // only loose refs, so write it directly rather than invoking host git.
    let pull = base.join(".git/refs/pull/1");
    std::fs::create_dir_all(&pull).unwrap();
    std::fs::write(pull.join("head"), format!("{head}\n")).unwrap();
    f.jj(&base, &["bookmark", "delete", "tip"]);
    f.jj(&f.ws, &["git", "remote", "add", "origin", f.root.to_str().unwrap()]);
    head
}

/// Answers GitHub's PR lookup and leaves every other command to real jj/git.
struct PrJsonOnly<'a> {
    jj: &'a DeclaredJj,
    pr_json: String,
    base: PathBuf,
    lookups: std::sync::atomic::AtomicUsize,
}

impl CommandRunner for PrJsonOnly<'_> {
    fn run(&self, invocation: &CommandInvocation) -> Result<String> {
        if invocation.program == "gh" {
            self.lookups.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(self.pr_json.clone());
        }
        // Route the selected GitHub URL to the disposable base repository.
        // Local origin stays untouched and cannot serve the missing pull ref.
        let mut invocation = invocation.clone();
        for arg in &mut invocation.args {
            assert_ne!(
                arg, "https://github.com/example/repo-tools",
                "selected the wrong repository"
            );
            if arg == "https://github.com/example/repo" {
                *arg = self.base.display().to_string();
            }
        }
        self.jj.run(&invocation)
    }
}

#[test]
fn head_is_fetched_from_the_pull_ref_when_only_it_serves_the_head() {
    let f = Fixture::new();
    let head = base_repo_serving_only_a_pull_ref(&f);
    assert_eq!(f.commit(&format!("present({head})")), "", "head must start absent");
    // A temporary remote orphaned by a killed run, and one a live fetch may own.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let url = f.root.to_str().unwrap();
    f.jj(&f.ws, &["git", "remote", "add", "cube-pr-fetch-9-100-killed", url]);
    f.jj(
        &f.ws,
        &["git", "remote", "add", &format!("cube-pr-fetch-9-{now}-live"), url],
    );
    // The GitHub upstream must be selected instead of the local mirror.
    f.jj(
        &f.ws,
        &["git", "remote", "add", "upstream", "https://github.com/example/repo"],
    );
    let runner = PrJsonOnly {
        jj: &f.runner,
        base: f._dir.path().join("base"),
        lookups: Default::default(),
        pr_json: String::new(),
    };
    let info = serde_json::json!({"head": {"ref": "deleted-branch", "sha": head, "repo": null}});
    let resolved = crate::app::pr_head::resolve_pr_head(&runner, None, &f.ws, "example/repo", 1, &info, None).unwrap();
    assert_eq!(resolved.sha, head);
    assert!(
        !resolved.same_repo,
        "a deleted head repository is not the base repository"
    );
    let remotes = f.jj(&f.ws, &["git", "remote", "list"]);
    assert!(!remotes.contains("killed"), "the orphaned remote is pruned: {remotes}");
    assert!(
        remotes.contains("-live"),
        "a remote a live fetch may own is kept: {remotes}"
    );
    assert!(
        f.pr_commits(1).is_empty() && f.git_ref("pr/1").is_none(),
        "resolving a head creates no alias"
    );
}

#[test]
fn normal_sweep_prunes_expired_fetch_remotes_without_fetching() {
    let f = Fixture::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let stale = "cube-pr-fetch-9-100-killed";
    let fresh = format!("cube-pr-fetch-9-{now}-live");
    for name in [stale, &fresh] {
        f.jj(&f.ws, &["git", "remote", "add", name, f.root.to_str().unwrap()]);
        f.jj(&f.ws, &["git", "fetch", "--remote", name]);
        assert!(!f.commit(&format!("remote_bookmarks(remote={name})")).is_empty());
    }
    crate::app::gc::gc_workspace_bookmarks(&f.runner, None, &f.ws, false, false).unwrap();
    let remotes = f.jj(&f.ws, &["git", "remote", "list"]);
    assert!(!remotes.contains(stale));
    assert!(remotes.contains(&fresh));
    let bookmarks = f.jj(&f.ws, &["bookmark", "list", "--all-remotes"]);
    assert!(!bookmarks.contains(stale), "{bookmarks}");
    assert!(bookmarks.contains(&fresh), "{bookmarks}");
}

#[test]
fn pull_ref_ignores_a_github_remote_with_a_similar_repo_name() {
    let f = Fixture::new();
    let head = base_repo_serving_only_a_pull_ref(&f);
    f.jj(
        &f.ws,
        &[
            "git",
            "remote",
            "set-url",
            "origin",
            "https://github.com/example/repo-tools",
        ],
    );
    f.jj(
        &f.ws,
        &["git", "remote", "add", "upstream", "https://github.com/example/repo"],
    );
    let runner = PrJsonOnly {
        jj: &f.runner,
        base: f._dir.path().join("base"),
        lookups: Default::default(),
        pr_json: String::new(),
    };
    let info = serde_json::json!({"head": {"ref": "gone", "sha": head, "repo": null}});
    let resolved = crate::app::pr_head::resolve_pr_head(&runner, None, &f.ws, "example/repo", 1, &info, None).unwrap();
    assert_eq!(resolved.sha, head);
}

#[test]
fn stale_remote_age_outlasts_every_network_attempt() {
    let age = std::time::Duration::from_secs(crate::app::pr_head::stale_temp_remote_age_secs());
    assert!(age > crate::app::jj::network_cmd_total_bound());
    assert!(age >= std::time::Duration::from_secs(15 * 60));
}
