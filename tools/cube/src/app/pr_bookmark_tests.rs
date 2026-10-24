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

#[test]
fn resolved_rebase_updates_shared_pr_bookmark_for_next_launch() {
    exercise_publication(false);
}

#[test]
fn resolved_rebase_replaces_an_ambiguous_pr_bookmark_at_publication() {
    exercise_publication(true);
}

fn exercise_publication(ambiguous: bool) {
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
    jj(&repo, &["new", "base", "-m", "upstream"]);
    std::fs::write(repo.join("file"), "upstream\n").unwrap();
    jj(&repo, &["bookmark", "set", "main", "-r", "@"]);
    jj(&repo, &["rebase", "-r", "feature", "-d", "main"]);
    assert_eq!(
        jj(&repo, &["log", "-r", "pr/42", "--no-graph", "-T", "conflict"]).trim(),
        "true"
    );

    // A revision stages its own history, preserving the old conflicted head.
    let worker = temp.path().join("worker");
    jj(&repo, &["workspace", "add", worker.to_str().unwrap(), "-r", "main"]);
    jj(&worker, &["duplicate", "feature", "--insert-before", "@"]);
    jj(&worker, &["edit", "@-"]);
    std::fs::write(worker.join("file"), "upstream and feature\n").unwrap();
    jj(&worker, &["bookmark", "set", "feature", "-r", "@", "--allow-backwards"]);
    jj(&worker, &["git", "push", "-b", "feature"]);
    jj(&remote, &["git", "import"]);
    let pushed = jj_commit_id(&runner, &remote, "feature").unwrap();
    assert_eq!(jj_commit_id(&runner, &worker, "feature").unwrap(), pushed);
    assert_eq!(
        jj(&worker, &["log", "-r", &pushed, "--no-graph", "-T", "conflict"]).trim(),
        "false"
    );

    if ambiguous {
        // Concurrent bookmark operations can leave two added targets. Publish
        // must resolve that ambiguity explicitly; launch must never pick one.
        let operation = jj(&repo, &["op", "log", "--no-graph", "--limit", "1", "-T", "id"]);
        jj(&repo, &["new", "base", "-m", "other history"]);
        jj(&repo, &["bookmark", "set", "pr/42", "-r", "@", "--allow-backwards"]);
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

    // The post-publish operation must replace, rather than fast-forward, pr/42.
    record_published_pr_head(&runner, &worker, Some(42), &pushed).unwrap();
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
