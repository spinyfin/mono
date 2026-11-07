use super::reconcile_tests::one;
use super::*;
use boss_engine_test_git::jj::JjRepo;

mod history;

fn publish_main(repo: &JjRepo) -> String {
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    JjRepo::run(&repo.repo, &["git", "export"]);
    JjRepo::run(&repo.repo, &["log", "-r", "main", "--no-graph", "-T", "commit_id"])
}

#[tokio::test]
async fn chore_restores_all_commits_on_fetched_main_and_reports_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::with_origin(dir.path());
    let prior = create(&LocalJj, &repo.worker, "exec_prior", "local").await.unwrap();
    std::fs::write(repo.worker.join("one"), "first fix").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "First fix"]);
    let first = JjRepo::run(&repo.worker, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Second fix"]);
    JjRepo::run(&repo.worker, &["bookmark", "set", &prior.head(), "-r", "@"]);
    std::fs::write(repo.worker.join("two"), "second fix").unwrap();
    JjRepo::snapshot(&repo.worker);
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Toolchain fix"]);
    std::fs::write(repo.repo.join("toolchain"), "updated").unwrap();
    let main = publish_main(&repo);
    let report = restore_rebased(&LocalJj, &prior, &repo.replacement, None, "main", "origin")
        .await
        .unwrap();
    assert_eq!(report.base_sha, main);
    assert!(report.commits.contains(&first));
    assert!(report.commits.contains("Second fix"));
    assert!(report.conflicts.is_empty());
    for file in ["one", "two", "toolchain"] {
        assert!(repo.replacement.join(file).exists(), "{file}");
    }
    assert!(report.instructions().contains(&main));
    assert!(report.instructions().contains(&prior.head()));
    assert!(!report.instructions().contains("--allow-backwards"));
    assert!(report.instructions().contains("earlier validation does not satisfy"));
    let next = test_support::create_legacy_three_bookmark_fixture_from(
        &LocalJj,
        &repo.replacement,
        "exec_next",
        "local",
        Some(&prior),
        report.inherited_base.as_deref(),
    )
    .await
    .unwrap();
    let patch = diff(&LocalJj, &next).await.unwrap();
    assert!(patch.contains("second fix"));
    assert!(
        !patch.contains("toolchain"),
        "new main changes must not enter the successor diff"
    );
}

#[tokio::test]
async fn revision_preserves_unpushed_fixes_on_top_of_newer_bound_pr_head() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::with_origin(dir.path());
    std::fs::write(repo.worker.join("pr"), "published PR").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Published PR"]);
    JjRepo::run(&repo.worker, &["bookmark", "set", "pr/77", "-r", "@"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Unpushed revision"]);
    let prior = create(&LocalJj, &repo.worker, "exec_revision", "local").await.unwrap();
    std::fs::write(repo.worker.join("local-fix"), "preserved").unwrap();
    JjRepo::snapshot(&repo.worker);
    JjRepo::run(&repo.repo, &["new", "pr/77", "-m", "New PR commit"]);
    std::fs::write(repo.repo.join("remote-fix"), "new PR work").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "pr/77", "-r", "@"]);
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Main advances"]);
    std::fs::write(repo.repo.join("toolchain"), "updated").unwrap();
    let main = publish_main(&repo);
    let report = restore_rebased(&LocalJj, &prior, &repo.replacement, Some("pr/77"), "main", "origin")
        .await
        .unwrap();
    assert_eq!(report.base_sha, main);
    assert!(report.instructions().contains("--allow-backwards"));
    assert!(report.commits.contains("Unpushed revision"));
    assert!(report.commits.contains("New PR commit"));
    for file in ["pr", "local-fix", "remote-fix", "toolchain"] {
        assert!(repo.replacement.join(file).exists(), "{file}");
    }
    assert!(report.conflicts.is_empty());
    let baseline = report.inherited_base.as_ref().unwrap();
    assert!(
        !JjRepo::run(
            &repo.replacement,
            &["log", "-r", &format!("{baseline}::@"), "--no-graph", "-T", "commit_id"],
        )
        .is_empty()
    );
    let files = JjRepo::run(&repo.replacement, &["file", "list", "-r", baseline]);
    let names: Vec<_> = files
        .lines()
        .map(|f| Path::new(f).file_name().unwrap().to_str().unwrap())
        .collect();
    assert!(names.contains(&"pr"), "{files}");
    assert!(names.contains(&"remote-fix"), "{files}");
    assert!(!names.contains(&"local-fix"), "{files}");
}

#[tokio::test]
async fn conflicts_remain_in_history_and_are_an_explicit_first_task() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::with_origin(dir.path());
    let prior = create(&LocalJj, &repo.worker, "exec_conflict", "local").await.unwrap();
    std::fs::write(repo.worker.join("base.txt"), "worker change\n").unwrap();
    JjRepo::snapshot(&repo.worker);
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Conflicting main"]);
    std::fs::write(repo.repo.join("base.txt"), "main change\n").unwrap();
    publish_main(&repo);
    let report = restore_rebased(&LocalJj, &prior, &repo.replacement, None, "main", "origin")
        .await
        .unwrap();
    assert!(report.conflicts.contains("base.txt"));
    assert!(report.instructions().contains("FIRST TASK"));
    let text = std::fs::read_to_string(repo.replacement.join("base.txt")).unwrap();
    assert!(text.contains("worker change") && text.contains("main change"));
    test_support::create_legacy_three_bookmark_fixture_from(
        &LocalJj,
        &repo.replacement,
        "exec_conflict_next",
        "local",
        Some(&prior),
        report.inherited_base.as_deref(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn missing_preserved_pointer_fails_before_modifying_the_destination() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::with_origin(dir.path());
    let prior = create(&LocalJj, &repo.worker, "exec_missing", "local").await.unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "delete", &prior.head(), &prior.publication()]);
    let before = JjRepo::run(&repo.replacement, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    let error = restore_rebased(&LocalJj, &prior, &repo.replacement, None, "main", "origin")
        .await
        .unwrap_err();
    assert!(error.to_string().contains(&prior.head()));
    assert!(is_missing_pointer_error(&error));
    assert!(!is_pointer_integrity_error(&error));
    assert_eq!(
        before,
        JjRepo::run(&repo.replacement, &["log", "-r", "@", "--no-graph", "-T", "commit_id"])
    );
}

#[tokio::test]
async fn publication_pointer_restores_newer_work_and_survives_a_missing_recovery_ref() {
    for missing_recovery in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let repo = JjRepo::with_origin(dir.path());
        let prior = create(&LocalJj, &repo.worker, "exec_publication", "local")
            .await
            .unwrap();
        JjRepo::run(&repo.worker, &["new", "-m", "Publication advanced"]);
        std::fs::write(repo.worker.join("fix"), "preserved").unwrap();
        JjRepo::run(&repo.worker, &["bookmark", "set", &prior.publication(), "-r", "@"]);
        if missing_recovery {
            JjRepo::run(&repo.repo, &["bookmark", "delete", &prior.head()]);
        }
        let report = restore_rebased(&LocalJj, &prior, &repo.replacement, None, "main", "origin")
            .await
            .unwrap();
        assert_eq!(report.pointer, prior.publication());
        assert!(report.commits.contains("Publication advanced"));
        assert!(repo.replacement.join("fix").exists());
    }
}

#[tokio::test]
async fn failed_fetch_is_not_a_pointer_integrity_error() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    // No `origin` remote: the fetch fails although the pointer is intact.
    let prior = create(&LocalJj, &repo.worker, "exec_fetch", "local").await.unwrap();
    let error = restore_rebased(&LocalJj, &prior, &repo.replacement, None, "main", "origin")
        .await
        .unwrap_err();
    assert!(!is_pointer_integrity_error(&error), "{error:#}");
}

#[tokio::test]
async fn rewritten_pr_head_does_not_replay_stale_published_commits() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::with_origin(dir.path());
    std::fs::write(repo.worker.join("stale-pr"), "old published").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Old published PR"]);
    JjRepo::run(&repo.worker, &["bookmark", "set", "pr/78", "-r", "@"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Unpushed revision"]);
    let old_pr = one(&repo.worker, "pr/78");
    let prior = create_legacy_three_bookmark_fixture_from(
        &LocalJj,
        &repo.worker,
        "exec_rewritten",
        "local",
        None,
        Some(&old_pr),
    )
    .await
    .unwrap();
    // The publication pointer may still be at the old PR baseline while
    // recovery has advanced to unpublished work.
    JjRepo::run(
        &repo.worker,
        &[
            "bookmark",
            "set",
            &prior.publication(),
            "-r",
            &prior.base(),
            "--allow-backwards",
        ],
    );
    std::fs::write(repo.worker.join("local-fix"), "preserved").unwrap();
    JjRepo::snapshot(&repo.worker);
    // The PR branch is force-pushed to a sibling of the old published commit.
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Rewritten PR"]);
    std::fs::write(repo.repo.join("rewritten-pr"), "force pushed").unwrap();
    JjRepo::run(
        &repo.repo,
        &["bookmark", "set", "pr/78", "--allow-backwards", "-r", "@"],
    );
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Main advances"]);
    std::fs::write(repo.repo.join("toolchain"), "updated").unwrap();
    publish_main(&repo);
    let report = restore_rebased(&LocalJj, &prior, &repo.replacement, Some("pr/78"), "main", "origin")
        .await
        .unwrap();
    assert!(report.conflicts.is_empty());
    for file in ["rewritten-pr", "local-fix", "toolchain"] {
        assert!(repo.replacement.join(file).exists(), "{file}");
    }
    assert!(!repo.replacement.join("stale-pr").exists());
    let prior_patch = diff(&LocalJj, &prior).await.unwrap();
    assert!(prior_patch.contains("preserved"));
    assert!(!prior_patch.contains("old published"));
    let next = test_support::create_legacy_three_bookmark_fixture_from(
        &LocalJj,
        &repo.replacement,
        "exec_successor",
        "local",
        Some(&prior),
        report.inherited_base.as_deref(),
    )
    .await
    .unwrap();
    std::fs::write(repo.replacement.join("successor-fix"), "successor work").unwrap();
    JjRepo::snapshot(&repo.replacement);
    assert_eq!(diff(&LocalJj, &prior).await.unwrap(), prior_patch);
    let patch = diff(&LocalJj, &next).await.unwrap();
    assert!(patch.contains("preserved") && patch.contains("successor work"));
    restore_rebased(&LocalJj, &next, &repo.replacement, Some("pr/78"), "main", "origin")
        .await
        .unwrap();
    assert!(diff(&LocalJj, &next).await.unwrap().contains("successor work"));
    assert!(!repo.replacement.join("stale-pr").exists());
}

#[tokio::test]
async fn abandoning_revision_child_keeps_predecessor_pointers_intact() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::with_origin(dir.path());
    JjRepo::run(&repo.repo, &["bookmark", "set", "pr/79", "-r", "main"]);
    let prior = create(&LocalJj, &repo.worker, "exec_abandon", "local").await.unwrap();
    std::fs::write(repo.worker.join("fix"), "preserved").unwrap();
    JjRepo::snapshot(&repo.worker);
    let patch = diff(&LocalJj, &prior).await.unwrap();
    restore_rebased(&LocalJj, &prior, &repo.replacement, Some("pr/79"), "main", "origin")
        .await
        .unwrap();
    std::fs::write(repo.replacement.join("successor"), "unvalidated").unwrap();
    JjRepo::run(&repo.replacement, &["abandon", "@"]);
    assert_eq!(diff(&LocalJj, &prior).await.unwrap(), patch);
}

#[tokio::test]
async fn chore_stages_copies_on_configured_branch_and_remote() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::with_origin(dir.path());
    JjRepo::run(&repo.repo, &["git", "remote", "rename", "origin", "upstream"]);
    JjRepo::run(&repo.repo, &["bookmark", "rename", "main", "trunk"]);
    JjRepo::run(
        &repo.repo,
        &["config", "set", "--repo", "revset-aliases.\"trunk()\"", "trunk"],
    );
    JjRepo::run(&repo.repo, &["git", "export"]);
    let prior = create(&LocalJj, &repo.worker, "exec_chore_copy", "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("fix"), "preserved").unwrap();
    JjRepo::snapshot(&repo.worker);
    let patch = diff(&LocalJj, &prior).await.unwrap();
    let report = restore_rebased(&LocalJj, &prior, &repo.replacement, None, "trunk", "upstream")
        .await
        .unwrap();
    let next = test_support::create_legacy_three_bookmark_fixture_from(
        &LocalJj,
        &repo.replacement,
        "exec_chore_next",
        "local",
        Some(&prior),
        report.inherited_base.as_deref(),
    )
    .await
    .unwrap();
    assert!(diff(&LocalJj, &next).await.unwrap().contains("preserved"));
    JjRepo::run(&repo.replacement, &["abandon", "@-"]);
    assert_eq!(diff(&LocalJj, &prior).await.unwrap(), patch);
}

#[tokio::test]
async fn stacked_revision_keeps_parent_commits_on_their_original_history() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::with_origin(dir.path());
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Parent PR"]);
    std::fs::write(repo.repo.join("parent"), "parent work").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "parent-pr", "-r", "@"]);
    JjRepo::run(&repo.repo, &["git", "export"]);
    let parent = JjRepo::run(&repo.repo, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    JjRepo::run(&repo.worker, &["new", "parent-pr", "-m", "Child PR"]);
    std::fs::write(repo.worker.join("child"), "child work").unwrap();
    JjRepo::run(&repo.worker, &["bookmark", "set", "pr/80", "-r", "@"]);
    JjRepo::run(&repo.worker, &["new"]);
    let prior = create(&LocalJj, &repo.worker, "exec_stacked", "local").await.unwrap();
    std::fs::write(repo.worker.join("fix"), "unpublished").unwrap();
    JjRepo::snapshot(&repo.worker);
    let report = restore_rebased(
        &LocalJj,
        &prior,
        &repo.replacement,
        Some("pr/80"),
        "parent-pr",
        "origin",
    )
    .await
    .unwrap();
    assert_eq!(report.base_sha, parent);
    assert!(!report.commits.contains("Parent PR"));
    assert_eq!(
        JjRepo::run(
            &repo.replacement,
            &["log", "-r", &format!("{parent} & ::@"), "--no-graph", "-T", "commit_id"]
        ),
        parent
    );
    for file in ["parent", "child", "fix"] {
        assert!(repo.replacement.join(file).exists());
    }
}

#[tokio::test]
async fn unpushed_local_rebase_of_the_pr_line_does_not_replay_pr_commits() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::with_origin(dir.path());
    std::fs::write(repo.worker.join("pr-one"), "first").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "PR commit one"]);
    JjRepo::run(&repo.worker, &["new", "-m", "PR commit two"]);
    std::fs::write(repo.worker.join("pr-two"), "second").unwrap();
    JjRepo::run(&repo.worker, &["bookmark", "set", "pr/79", "-r", "@"]);
    let pr_head = one(&repo.worker, "pr/79");
    JjRepo::run(&repo.worker, &["new", "-m", "Unpushed revision"]);
    let prior = create_legacy_three_bookmark_fixture_from(
        &LocalJj,
        &repo.worker,
        "exec_local_rebase",
        "local",
        None,
        Some(&pr_head),
    )
    .await
    .unwrap();
    assert_eq!(prior.base_commit.as_deref(), Some(pr_head.as_str()));
    std::fs::write(repo.worker.join("local-fix"), "preserved").unwrap();
    JjRepo::snapshot(&repo.worker);
    // The PR branch is published, which keeps the original head visible.
    let remote_dir = tempfile::tempdir().unwrap();
    let remote = JjRepo::new(remote_dir.path());
    JjRepo::run(
        &repo.repo,
        &[
            "git",
            "remote",
            "add",
            "published",
            remote.repo.join(".jj/repo/store/git").to_str().unwrap(),
        ],
    );
    JjRepo::run(
        &repo.repo,
        &[
            "git",
            "push",
            "--remote",
            "published",
            "--bookmark",
            "pr/79",
            "--allow-new",
        ],
    );
    // Main advances, and the worker rebases the whole PR line onto it without
    // pushing, so the recovery pointer follows the rewritten commits.
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Main advances"]);
    std::fs::write(repo.repo.join("toolchain"), "updated").unwrap();
    publish_main(&repo);
    JjRepo::run(&repo.worker, &["rebase", "-b", "@", "-d", "main"]);
    assert_ne!(one(&repo.worker, &prior.head()), pr_head);
    // Cube's bound-PR checkout leaves `pr/79` at the published head.
    JjRepo::run(
        &repo.repo,
        &["bookmark", "set", "pr/79", "-r", "pr/79@published", "--allow-backwards"],
    );
    assert_eq!(one(&repo.repo, "pr/79"), pr_head);
    // The patch holds the PR line and the work above it, never upstream changes.
    let patch = diff(&LocalJj, &prior).await.unwrap();
    assert!(patch.contains("local-fix") && patch.contains("pr-two"), "{patch}");
    assert!(!patch.contains("toolchain"), "{patch}");

    let report = restore_rebased(&LocalJj, &prior, &repo.replacement, Some("pr/79"), "main", "origin")
        .await
        .unwrap();
    assert!(report.conflicts.is_empty(), "{}", report.conflicts);
    for file in ["pr-one", "pr-two", "local-fix", "toolchain"] {
        assert!(repo.replacement.join(file).exists(), "{file}");
    }
    let titles = JjRepo::run(
        &repo.replacement,
        &[
            "log",
            "-r",
            "main..@",
            "--no-graph",
            "-T",
            "description.first_line() ++ \"\\n\"",
        ],
    );
    for title in ["PR commit one", "PR commit two"] {
        assert_eq!(titles.lines().filter(|line| *line == title).count(), 1, "{titles}");
    }
}

#[tokio::test]
async fn unpushed_amendment_to_a_published_pr_commit_is_retained_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::with_origin(dir.path());
    std::fs::write(repo.worker.join("pr-one"), "first").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "PR commit one"]);
    JjRepo::run(&repo.worker, &["bookmark", "set", "pr/80", "-r", "@"]);
    let pr_head = one(&repo.worker, "pr/80");
    JjRepo::run(&repo.worker, &["new", "-m", "Unpushed revision"]);
    let prior = create_legacy_three_bookmark_fixture_from(
        &LocalJj,
        &repo.worker,
        "exec_amended_pr_commit",
        "amend",
        None,
        Some(&pr_head),
    )
    .await
    .unwrap();
    // Publish the PR commit so its original head stays visible.
    let remote_dir = tempfile::tempdir().unwrap();
    let remote = JjRepo::new(remote_dir.path());
    JjRepo::run(
        &repo.repo,
        &[
            "git",
            "remote",
            "add",
            "published",
            remote.repo.join(".jj/repo/store/git").to_str().unwrap(),
        ],
    );
    JjRepo::run(
        &repo.repo,
        &[
            "git",
            "push",
            "--remote",
            "published",
            "--bookmark",
            "pr/80",
            "--allow-new",
        ],
    );
    // The worker squashes a unique fix into the published commit without
    // pushing: same change id, different content.
    std::fs::write(repo.worker.join("amended-fix"), "only in the amendment").unwrap();
    JjRepo::run(&repo.worker, &["squash", "--use-destination-message"]);
    assert_ne!(one(&repo.worker, "pr/80"), pr_head);
    JjRepo::run(
        &repo.repo,
        &["bookmark", "set", "pr/80", "-r", "pr/80@published", "--allow-backwards"],
    );
    assert_eq!(one(&repo.repo, "pr/80"), pr_head);

    let report = restore_rebased(&LocalJj, &prior, &repo.replacement, Some("pr/80"), "main", "origin")
        .await
        .unwrap();
    assert!(report.conflicts.is_empty(), "{}", report.conflicts);
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("amended-fix")).unwrap(),
        "only in the amendment"
    );
    assert!(repo.replacement.join("pr-one").exists());
    let patch = JjRepo::run(&repo.replacement, &["diff", "--from", "main", "--to", "@", "--git"]);
    // The published content is carried once by the copy of the PR line; the
    // amended variant adds only what the published commit did not have.
    assert_eq!(patch.matches("+only in the amendment").count(), 1, "{patch}");
    assert_eq!(patch.matches("+first").count(), 1, "{patch}");
}
