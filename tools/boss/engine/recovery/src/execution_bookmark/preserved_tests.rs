use super::*;
use boss_engine_test_git::jj::JjRepo;

fn fixture(root: &Path) -> JjRepo {
    let repo = JjRepo::new(root);
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    JjRepo::run(&repo.repo, &["git", "export"]);
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
    JjRepo::run(&repo.repo, &["git", "fetch"]);
    repo
}

fn names(repo: &JjRepo) -> String {
    JjRepo::run(&repo.repo, &["bookmark", "list", "-T", "name ++ \"\\n\""])
}

#[tokio::test]
async fn conflicted_variants_prefer_pr_ancestry_then_jj_evolution() {
    for on_pr in [0, 1, 2] {
        let dir = tempfile::tempdir().unwrap();
        let repo = fixture(dir.path());
        let record = create(&LocalJj, &repo.worker, "exec_divergent", "local").await.unwrap();
        std::fs::write(repo.worker.join("common"), "common unpublished parent").unwrap();
        JjRepo::run(&repo.worker, &["new", "-m", "Diverging child"]);
        std::fs::write(repo.worker.join("work"), "preserved work").unwrap();
        let saved = preserve(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
        let op = JjRepo::run(&repo.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
        for title in ["Earlier variant", "Middle variant", "Later variant"] {
            JjRepo::run(&repo.repo, &["--at-op", &op, "describe", &saved.commit_id, "-m", title]);
        }
        let variants = JjRepo::run(
            &repo.repo,
            &[
                "log",
                "--no-graph",
                "-r",
                &revision(&preserved_name(&record.execution_id)),
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        );
        assert_eq!(variants.lines().count(), 3);
        let mut positioning = vec!["new", "-m", "Current PR head"];
        if on_pr == 0 {
            positioning.push("main");
        } else {
            positioning.push("description(substring:\"Earlier variant\")");
            if on_pr == 2 {
                positioning.push("description(substring:\"Middle variant\")");
            }
        }
        JjRepo::run(&repo.replacement, &positioning);
        let report = restore_preserved(&LocalJj, &record, &repo.replacement)
            .await
            .unwrap()
            .unwrap();
        if on_pr > 0 {
            assert!(
                report.commits.is_empty(),
                "PR history already contains the selected variant"
            );
        } else {
            assert!(report.commits.contains("Later variant"), "{}", report.commits);
        }
        assert_eq!(
            std::fs::read_to_string(repo.replacement.join("work")).unwrap(),
            "preserved work"
        );
    }
}

#[tokio::test]
async fn unpushed_tip_creates_exactly_one_bookmark_and_release_retry_reuses_it() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_preserve", "local").await.unwrap();
    let before = names(&repo);
    std::fs::write(repo.worker.join("work"), "uncommitted edits").unwrap();
    let tip = preserve(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    assert_eq!(tip.count, 1);
    assert_eq!(names(&repo).lines().count(), before.lines().count() + 1);
    assert!(names(&repo).contains("preserved/exec_preserve"));
    assert_eq!(
        JjRepo::run(&repo.worker, &["log", "-r", "@", "--no-graph", "-T", "change_id"]),
        tip.change_id
    );
    assert_eq!(preserve(&LocalJj, &record, &repo.worker).await.unwrap(), Some(tip));
    assert_eq!(names(&repo).lines().count(), before.lines().count() + 1);
}

#[tokio::test]
async fn published_work_and_empty_trailing_commit_create_no_bookmark() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_published", "local").await.unwrap();
    std::fs::write(repo.worker.join("work"), "published edits").unwrap();
    JjRepo::run(&repo.worker, &["bookmark", "set", "published", "-r", "@"]);
    JjRepo::run(&repo.worker, &["git", "export"]);
    JjRepo::run(&repo.repo, &["git", "fetch"]);
    JjRepo::run(&repo.worker, &["new", "-m", "Empty trailing commit"]);
    let before = names(&repo);
    assert_eq!(preserve(&LocalJj, &record, &repo.worker).await.unwrap(), None);
    assert_eq!(names(&repo), before);
}

#[tokio::test]
async fn missing_and_forgotten_workspace_uses_last_journaled_working_copy() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_lost", "local").await.unwrap();
    std::fs::write(repo.worker.join("work"), "journaled edits").unwrap();
    let tip = JjRepo::run(&repo.worker, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    std::fs::remove_dir_all(&repo.worker).unwrap();
    JjRepo::run(&repo.repo, &["workspace", "forget", "worker"]);
    let saved = preserve(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    assert_eq!(saved.commit_id, tip);
    assert_eq!(saved.count, 1);
}

#[tokio::test]
async fn restoration_follows_rewrites_and_keeps_change_ids_on_new_base() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_restore", "local").await.unwrap();
    std::fs::write(repo.worker.join("work"), "preserved edits").unwrap();
    let saved = preserve(&LocalJj, &record, &repo.worker).await.unwrap().unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Rewritten description"]);
    JjRepo::run(&repo.replacement, &["new", "main", "-m", "New upstream"]);
    std::fs::write(repo.replacement.join("upstream"), "upstream edits").unwrap();
    let base = JjRepo::run(&repo.replacement, &["log", "-r", "@", "--no-graph", "-T", "commit_id"]);
    let report = restore_preserved(&LocalJj, &record, &repo.replacement)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.base_sha, base);
    assert!(report.conflicts.is_empty());
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("work")).unwrap(),
        "preserved edits"
    );
    assert!(repo.replacement.join("upstream").exists());
    assert_eq!(
        JjRepo::run(&repo.replacement, &["log", "-r", "@", "--no-graph", "-T", "change_id"]),
        saved.change_id
    );
    create_from(
        &LocalJj,
        &repo.replacement,
        "exec_next",
        "local",
        None,
        report.inherited_base.as_deref(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn restoration_keeps_content_conflicts_for_worker_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(dir.path());
    let record = create(&LocalJj, &repo.worker, "exec_conflicts", "local").await.unwrap();
    std::fs::write(repo.worker.join("base.txt"), "worker edits\n").unwrap();
    preserve(&LocalJj, &record, &repo.worker).await.unwrap();
    JjRepo::run(&repo.replacement, &["new", "main", "-m", "Upstream edits"]);
    std::fs::write(repo.replacement.join("base.txt"), "upstream edits\n").unwrap();
    let report = restore_preserved(&LocalJj, &record, &repo.replacement)
        .await
        .unwrap()
        .unwrap();
    assert!(!report.conflicts.is_empty());
    assert!(report.instructions().contains("FIRST TASK"));
    let file = std::fs::read_to_string(repo.replacement.join("base.txt")).unwrap();
    assert!(file.contains("worker edits") && file.contains("upstream edits"));
}
