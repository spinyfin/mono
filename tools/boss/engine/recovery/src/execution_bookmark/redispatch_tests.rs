use super::*;
use boss_engine_test_git::jj::JjRepo;

fn publish_main(repo: &JjRepo) -> String {
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    JjRepo::run(&repo.repo, &["git", "export"]);
    JjRepo::run(&repo.repo, &["log", "-r", "main", "--no-graph", "-T", "commit_id"])
}

fn origin(repo: &JjRepo) {
    JjRepo::run(
        &repo.repo,
        &[
            "git",
            "remote",
            "add",
            "origin",
            repo.repo.join(".jj/repo/store/git").to_str().unwrap(),
        ],
    );
    publish_main(repo);
}

#[tokio::test]
async fn chore_restores_all_commits_on_fetched_main_and_reports_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    origin(&repo);
    let prior = create(&LocalJj, &repo.worker, "exec_prior", "local").await.unwrap();
    std::fs::write(repo.worker.join("one"), "first fix").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "First fix"]);
    let first = JjRepo::run(&repo.worker, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Second fix"]);
    JjRepo::run(&repo.worker, &["bookmark", "set", &prior.head(), "-r", "@"]);
    std::fs::write(repo.worker.join("two"), "second fix").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Toolchain fix"]);
    std::fs::write(repo.repo.join("toolchain"), "updated").unwrap();
    let main = publish_main(&repo);
    let report = restore_rebased(&LocalJj, &prior, &repo.replacement, None)
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
    let next = create_from(&LocalJj, &repo.replacement, "exec_next", "local", Some(&prior))
        .await
        .unwrap();
    assert!(diff(&LocalJj, &next).await.unwrap().contains("second fix"));
}

#[tokio::test]
async fn revision_preserves_unpushed_fixes_on_top_of_newer_bound_pr_head() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    origin(&repo);
    std::fs::write(repo.worker.join("pr"), "published PR").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Published PR"]);
    JjRepo::run(&repo.worker, &["bookmark", "set", "pr/77", "-r", "@"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Unpushed revision"]);
    let prior = create(&LocalJj, &repo.worker, "exec_revision", "local").await.unwrap();
    std::fs::write(repo.worker.join("local-fix"), "preserved").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    JjRepo::run(&repo.repo, &["new", "pr/77", "-m", "New PR commit"]);
    std::fs::write(repo.repo.join("remote-fix"), "new PR work").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "pr/77", "-r", "@"]);
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Main advances"]);
    std::fs::write(repo.repo.join("toolchain"), "updated").unwrap();
    let main = publish_main(&repo);
    let report = restore_rebased(&LocalJj, &prior, &repo.replacement, Some("pr/77"))
        .await
        .unwrap();
    assert_eq!(report.base_sha, main);
    assert!(report.commits.contains("Unpushed revision"));
    assert!(report.commits.contains("New PR commit"));
    for file in ["pr", "local-fix", "remote-fix", "toolchain"] {
        assert!(repo.replacement.join(file).exists(), "{file}");
    }
    assert!(report.conflicts.is_empty());
    assert!(
        !JjRepo::run(
            &repo.replacement,
            &["log", "-r", "pr/77::@", "--no-graph", "-T", "commit_id"]
        )
        .is_empty()
    );
}

#[tokio::test]
async fn conflicts_remain_in_history_and_are_an_explicit_first_task() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    origin(&repo);
    let prior = create(&LocalJj, &repo.worker, "exec_conflict", "local").await.unwrap();
    std::fs::write(repo.worker.join("base.txt"), "worker change\n").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Conflicting main"]);
    std::fs::write(repo.repo.join("base.txt"), "main change\n").unwrap();
    publish_main(&repo);
    let report = restore_rebased(&LocalJj, &prior, &repo.replacement, None)
        .await
        .unwrap();
    assert!(report.conflicts.contains("base.txt"));
    assert!(report.instructions().contains("FIRST TASK"));
    let text = std::fs::read_to_string(repo.replacement.join("base.txt")).unwrap();
    assert!(text.contains("worker change") && text.contains("main change"));
    create_from(&LocalJj, &repo.replacement, "exec_conflict_next", "local", Some(&prior))
        .await
        .unwrap();
}

#[tokio::test]
async fn missing_preserved_pointer_fails_before_modifying_the_destination() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    origin(&repo);
    let prior = create(&LocalJj, &repo.worker, "exec_missing", "local").await.unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "delete", &prior.head(), &prior.publication()]);
    let before = JjRepo::run(&repo.replacement, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    let error = restore_rebased(&LocalJj, &prior, &repo.replacement, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains(&prior.head()));
    assert!(is_pointer_integrity_error(&error));
    assert_eq!(
        before,
        JjRepo::run(&repo.replacement, &["log", "-r", "@", "--no-graph", "-T", "commit_id"])
    );
}

#[tokio::test]
async fn publication_pointer_restores_newer_work_and_survives_a_missing_recovery_ref() {
    for missing_recovery in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let repo = JjRepo::new(dir.path());
        origin(&repo);
        let prior = create(&LocalJj, &repo.worker, "exec_publication", "local")
            .await
            .unwrap();
        JjRepo::run(&repo.worker, &["new", "-m", "Publication advanced"]);
        std::fs::write(repo.worker.join("fix"), "preserved").unwrap();
        JjRepo::run(&repo.worker, &["bookmark", "set", &prior.publication(), "-r", "@"]);
        if missing_recovery {
            JjRepo::run(&repo.repo, &["bookmark", "delete", &prior.head()]);
        }
        let report = restore_rebased(&LocalJj, &prior, &repo.replacement, None)
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
    let error = restore_rebased(&LocalJj, &prior, &repo.replacement, None)
        .await
        .unwrap_err();
    assert!(!is_pointer_integrity_error(&error), "{error:#}");
}

#[tokio::test]
async fn rewritten_pr_head_does_not_replay_stale_published_commits() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    origin(&repo);
    std::fs::write(repo.worker.join("stale-pr"), "old published").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Old published PR"]);
    JjRepo::run(&repo.worker, &["bookmark", "set", "pr/78", "-r", "@"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Unpushed revision"]);
    let prior = create(&LocalJj, &repo.worker, "exec_rewritten", "local").await.unwrap();
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
    JjRepo::run(&repo.worker, &["status"]);
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
    let report = restore_rebased(&LocalJj, &prior, &repo.replacement, Some("pr/78"))
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
    let next = create_from(&LocalJj, &repo.replacement, "exec_successor", "local", Some(&prior))
        .await
        .unwrap();
    std::fs::write(repo.replacement.join("successor-fix"), "successor work").unwrap();
    JjRepo::run(&repo.replacement, &["status"]);
    let patch = diff(&LocalJj, &next).await.unwrap();
    assert!(patch.contains("preserved") && patch.contains("successor work"));
    restore_rebased(&LocalJj, &next, &repo.replacement, Some("pr/78"))
        .await
        .unwrap();
    assert!(diff(&LocalJj, &next).await.unwrap().contains("successor work"));
    assert!(!repo.replacement.join("stale-pr").exists());
}
