use super::*;

struct TestJj;

impl CommandRunner for TestJj {
    fn run(&self, invocation: &crate::command_runner::CommandInvocation) -> Result<String> {
        let mut invocation = invocation.clone();
        assert_eq!(invocation.program, "jj");
        invocation.program = std::fs::canonicalize(std::env::var_os("BOSS_JJ_BIN").unwrap())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        RealCommandRunner.run(&invocation)
    }
}

// Keep all jj operations real; GitHub reads use the local remote fixture.
struct PublishFixture<'a> {
    worker: &'a Path,
    remote: &'a Path,
}

impl CommandRunner for PublishFixture<'_> {
    fn lock_pr_bookmarks(&self, _cwd: &Path) -> Result<crate::lock::RepoLock> {
        crate::lock::RepoLock::for_workspace(self.worker)
    }

    fn run(&self, invocation: &crate::command_runner::CommandInvocation) -> Result<String> {
        if invocation.program == "gh" {
            if invocation.args == ["api", "repos/test/repo/pulls/42"] {
                return Ok(r#"{"head":{"ref":"feature"}}"#.into());
            }
            if invocation.args == ["api", "repos/test/repo/pulls?head=test:feature&state=open"] {
                return Ok(r#"[{"html_url":"https://github.com/test/repo/pull/42"}]"#.into());
            }
            assert_eq!(
                invocation.args,
                ["api", "repos/test/repo/branches/feature", "--jq", ".commit.sha"]
            );
            TestJj.run(&RealCommandRunner::invocation(self.remote, "jj", &["git", "import"]))?;
            return jj_commit_id(&TestJj, self.remote, "feature");
        }
        if invocation.args == ["git", "remote", "list"] {
            return Ok("origin git@github.com:test/repo.git\n".into());
        }
        let mut invocation = invocation.clone();
        invocation.cwd = self.worker.to_owned();
        TestJj.run(&invocation)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Scenario {
    /// The rebase conflicts; the worker resolves it with a fresh duplicate.
    Plain,
    /// Concurrent operations left `pr/42` conflicted between two rewrites of
    /// the published head.
    AmbiguousRewrite,
    /// `pr/42` is conflicted against a commit unrelated to anything published.
    AmbiguousUnrelated,
    /// The incident shape: the pre-rebase commit stays visible through another
    /// bookmark, and the pushed resolution shares a change id with the
    /// conflicted rebase result, evolved from the pre-rebase commit.
    Divergent,
}

#[test]
fn resolved_rebase_updates_shared_pr_bookmark_for_next_launch() {
    exercise_publication(Scenario::Plain);
}

#[test]
fn resolved_rebase_replaces_an_ambiguous_pr_bookmark_at_publication() {
    exercise_publication(Scenario::AmbiguousRewrite);
}

#[test]
fn divergent_resolution_of_the_published_head_moves_the_pr_bookmark() {
    exercise_publication(Scenario::Divergent);
}

#[test]
fn unrelated_ambiguous_pr_bookmark_side_is_still_refused() {
    exercise_publication(Scenario::AmbiguousUnrelated);
}

fn exercise_publication(scenario: Scenario) {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let runner = TestJj;
    let jj = |cwd: &Path, args: &[&str]| runner.run(&RealCommandRunner::invocation(cwd, "jj", args)).unwrap();
    jj(&repo, &["git", "init", "--colocate"]);
    jj(&repo, &["config", "set", "--repo", "user.name", "Test"]);
    jj(&repo, &["config", "set", "--repo", "user.email", "test@example.com"]);
    std::fs::write(repo.join("file"), "base\n").unwrap();
    jj(&repo, &["commit", "-m", "base"]);
    jj(&repo, &["bookmark", "set", "base", "-r", "@-"]);
    std::fs::write(repo.join("file"), "feature\n").unwrap();
    jj(&repo, &["describe", "-m", "feature"]);
    jj(&repo, &["bookmark", "set", "pr/42", "feature", "-r", "@"]);
    let remote = temp.path().join("remote");
    std::fs::create_dir(&remote).unwrap();
    jj(&remote, &["git", "init"]);
    let remote_git = remote.join(".jj/repo/store/git");
    jj(&repo, &["git", "remote", "add", "origin", remote_git.to_str().unwrap()]);
    jj(&repo, &["git", "push", "-b", "feature", "--allow-new"]);
    let published_before = jj_commit_id(&runner, &repo, "feature").unwrap();
    // Only a head that was genuinely published is on record, as a production
    // publication of it would have left it. The conflicted rebase result below
    // is never pushed and so is never recorded.
    crate::app::pr_bookmarks::record_head(&repo, 42, &published_before).unwrap();
    jj(&repo, &["new", "base", "-m", "upstream"]);
    std::fs::write(repo.join("file"), "upstream\n").unwrap();
    jj(&repo, &["bookmark", "set", "main", "-r", "@"]);
    jj(&repo, &["rebase", "-r", "feature", "-d", "main"]);
    assert_eq!(
        jj(&repo, &["log", "-r", "pr/42", "--no-graph", "-T", "conflict"]).trim(),
        "true"
    );
    let conflicted = jj_commit_id(&runner, &repo, "pr/42").unwrap();
    if scenario == Scenario::Divergent {
        // Recovery bookmarks keep the pre-rebase commit addressable next to
        // its conflicted rewrite.
        jj(&repo, &["bookmark", "set", "boss-base/test", "-r", &published_before]);
    }

    // A revision stages its own history, preserving the old conflicted head.
    let worker = temp.path().join("worker");
    jj(&repo, &["workspace", "add", worker.to_str().unwrap(), "-r", "main"]);
    if scenario == Scenario::Divergent {
        jj(&worker, &["edit", &published_before]);
        // Distinguish this rewrite before rebasing: identical metadata and
        // trees can recreate the first conflicted commit within one timestamp
        // tick, so resolving it would replace that side instead of diverging.
        jj(&worker, &["describe", "-m", "independent resolution"]);
        jj(&worker, &["rebase", "-r", "@", "-d", "main"]);
    } else {
        jj(&worker, &["duplicate", "feature", "--insert-before", "@"]);
        jj(&worker, &["edit", "@-"]);
    }
    std::fs::write(worker.join("file"), "upstream and feature\n").unwrap();
    jj(&worker, &["bookmark", "set", "feature", "-r", "@", "--allow-backwards"]);
    let pushed = jj_commit_id(&runner, &worker, "feature").unwrap();
    assert_ne!(pushed, conflicted);
    if scenario == Scenario::Divergent {
        let change = jj(&repo, &["log", "-r", &conflicted, "--no-graph", "-T", "change_id"]);
        let same_change = jj(
            &repo,
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("present({})", change.trim()),
                "-T",
                r#"commit_id ++ "\n""#,
            ],
        );
        let mut same_change: Vec<_> = same_change.lines().collect();
        same_change.sort();
        let mut expected = vec![conflicted.as_str(), pushed.as_str()];
        expected.sort();
        assert_eq!(same_change, expected, "expected the incident's divergent change");
    }
    assert_eq!(
        jj(&worker, &["log", "-r", &pushed, "--no-graph", "-T", "conflict"]).trim(),
        "false"
    );

    let ambiguous = matches!(scenario, Scenario::AmbiguousRewrite | Scenario::AmbiguousUnrelated);
    if ambiguous {
        // Concurrent bookmark operations can leave two added targets. Publish
        // must resolve that ambiguity explicitly; launch must never pick one.
        let operation = jj(&repo, &["op", "log", "--no-graph", "--limit", "1", "-T", "id"]);
        if scenario == Scenario::AmbiguousRewrite {
            // The other target is a rewrite of the conflicted rebase result.
            jj(&repo, &["describe", "-r", &conflicted, "-m", "concurrent rewrite"]);
        } else {
            jj(&repo, &["new", "base", "-m", "other history"]);
            jj(&repo, &["bookmark", "set", "pr/42", "-r", "@", "--allow-backwards"]);
        }
        jj(
            &repo,
            &[
                "--at-op",
                operation.trim(),
                "bookmark",
                "set",
                "pr/42",
                "-r",
                &pushed,
                "--allow-backwards",
            ],
        );
        let heads = jj(
            &repo,
            &[
                "log",
                "--no-graph",
                "-r",
                r#"bookmarks(exact:"pr/42")"#,
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        );
        assert_eq!(heads.lines().count(), 2);
        let rejected = temp.path().join("ambiguous-launch");
        assert!(
            runner
                .run(&RealCommandRunner::invocation(
                    &repo,
                    "jj",
                    &[
                        "workspace",
                        "add",
                        rejected.to_str().unwrap(),
                        "-r",
                        r#"bookmarks(exact:"pr/42")"#
                    ],
                ))
                .is_err()
        );
    }

    // Exercise the command recommended by the conflict handoff, including
    // its real push, verification and shared-bookmark synchronization.
    let published = crate::app::tests::with_checkleft_fixture(|| {
        let publisher = PublishFixture {
            worker: &worker,
            remote: &remote,
        };
        let (bookmark, pr) = if ambiguous {
            (Some("feature".into()), None)
        } else {
            (None, Some(42))
        };
        crate::app::workspace_ops::workspace_push_at(worker.clone(), None, &publisher, bookmark, pr)
    });
    // The push itself lands either way; only the bookkeeping is in question.
    assert_eq!(jj_commit_id(&runner, &remote, "feature").unwrap(), pushed);
    let heads = jj(
        &repo,
        &[
            "log",
            "--no-graph",
            "-r",
            "bookmarks(exact:\"pr/42\")",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    if scenario == Scenario::AmbiguousUnrelated {
        let error = published.unwrap_err().to_string();
        assert!(error.contains("will not pick a side"), "{error}");
        assert!(error.contains("will not help"), "{error}");
        assert_eq!(heads.lines().count(), 2, "an unrelated side must not be discarded");
        return;
    }
    published.unwrap();
    assert_eq!(heads.lines().collect::<Vec<_>>(), vec![pushed.as_str()]);
    jj(&repo, &["git", "export"]);
    jj(&repo, &["git", "import"]);
    assert_eq!(jj_commit_id(&runner, &repo, "pr/42").unwrap(), pushed);
    assert_eq!(jj_commit_id(&runner, &repo, "pr/42@git").unwrap(), pushed);
    let next = temp.path().join("next");
    jj(
        &repo,
        &[
            "workspace",
            "add",
            next.to_str().unwrap(),
            "-r",
            "bookmarks(exact:\"pr/42\")",
        ],
    );
    assert_eq!(jj_commit_id(&runner, &next, "@-").unwrap(), pushed);
}
