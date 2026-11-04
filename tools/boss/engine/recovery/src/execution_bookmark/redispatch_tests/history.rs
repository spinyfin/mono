use super::*;

fn log(repo: &Path, revset: &str, template: &str) -> String {
    JjRepo::run(repo, &["log", "--no-graph", "-r", revset, "-T", template])
}

fn checkpoint(repo: &Path, record: &ExecutionBookmark) {
    JjRepo::run(
        repo,
        &["bookmark", "set", &record.head(), &record.publication(), "-r", "@"],
    );
}

#[tokio::test]
async fn retry_reuses_its_own_empty_positioning_change() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    origin(&repo);
    let record = create(&LocalJj, &repo.worker, "exec_retry", "local").await.unwrap();
    let original = log(&repo.worker, "@", "commit_id");
    assert!(
        !restore_for_retry(&LocalJj, &record, &repo.replacement, true)
            .await
            .unwrap()
    );
    assert_eq!(log(&repo.replacement, "@", "commit_id"), original);
    std::fs::write(repo.replacement.join("fix"), "retry fix\n").unwrap();
    JjRepo::run(&repo.replacement, &["describe", "-m", "Fix after retry"]);
    assert!(
        log(
            &repo.replacement,
            "(main..@) & empty() & description(exact:\"\")",
            "commit_id"
        )
        .is_empty()
    );
    assert!(diff(&LocalJj, &record).await.unwrap().contains("retry fix"));
}

#[tokio::test]
async fn redispatch_does_not_promote_positioning_changes_into_pushed_history() {
    for pr in [None, Some("pr/81")] {
        for has_fixes in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let repo = JjRepo::new(dir.path());
            origin(&repo);
            if let Some(pr) = pr {
                std::fs::write(repo.worker.join("feature"), "published feature\n").unwrap();
                JjRepo::run(&repo.worker, &["describe", "-m", "Published feature"]);
                JjRepo::run(&repo.worker, &["bookmark", "set", pr, "-r", "@"]);
                // workspace goto creates this editable child, then dispatch pins it.
                JjRepo::run(&repo.worker, &["new", pr]);
            }
            let prior = create(&LocalJj, &repo.worker, "exec_positioned", "local")
                .await
                .unwrap();
            if has_fixes {
                // Also cover an interior setup change and a post-commit empty tip.
                JjRepo::run(&repo.worker, &["new", "-m", "Preserved fix"]);
                std::fs::write(repo.worker.join("fix"), "preserved fix\n").unwrap();
                JjRepo::run(&repo.worker, &["new"]);
                checkpoint(&repo.worker, &prior);
                JjRepo::run(&repo.worker, &["new", "-m", "Second preserved fix"]);
                std::fs::write(repo.worker.join("fix"), "preserved fix\nsecond fix\n").unwrap();
                JjRepo::run(&repo.worker, &["new"]);
                checkpoint(&repo.worker, &prior);
            }
            let original = log(&repo.repo, &revision(&prior.head()), "commit_id");
            let patch = diff(&LocalJj, &prior).await.unwrap();
            let report = restore_rebased(&LocalJj, &prior, &repo.replacement, pr, "main", "origin")
                .await
                .unwrap();
            assert!(report.conflicts.is_empty());
            let next = create_from(
                &LocalJj,
                &repo.replacement,
                "exec_successor",
                "local",
                Some(&prior),
                report.inherited_base.as_deref(),
            )
            .await
            .unwrap();
            // A second redispatch with no edits must not reintroduce the setup entry.
            if has_fixes {
                restore_rebased(&LocalJj, &next, &repo.replacement, pr, "main", "origin")
                    .await
                    .unwrap();
            } else {
                assert!(log(&repo.repo, &revision(&next.head()), "commit_id").is_empty());
                // No unpublished work means normal positioning, with no pointer to restore.
                JjRepo::run(&repo.replacement, &["new", pr.unwrap_or("main")]);
            }
            std::fs::write(repo.replacement.join("review-fix"), "review correction\n").unwrap();
            JjRepo::run(&repo.replacement, &["describe", "-m", "Correct review findings"]);
            JjRepo::run(&repo.replacement, &["bookmark", "set", "feature", "-r", "@"]);
            let offenders = log(
                &repo.replacement,
                "(main..feature) & empty() & description(exact:\"\")",
                "commit_id",
            );
            assert!(offenders.is_empty(), "PR {pr:?}, fixes {has_fixes}: {offenders}");
            assert_eq!(log(&repo.repo, &revision(&prior.head()), "commit_id"), original);
            assert_eq!(diff(&LocalJj, &prior).await.unwrap(), patch);
            if has_fixes {
                assert_eq!(
                    std::fs::read_to_string(repo.replacement.join("fix")).unwrap(),
                    "preserved fix\nsecond fix\n"
                );
                assert!(log(&repo.replacement, "(main..feature) & merges()", "commit_id").is_empty());
            }
            if pr.is_some() {
                assert!(repo.replacement.join("feature").exists());
            }
            // Exercise jj's real publication validation against a separate local remote.
            let remote = dir.path().join("published");
            JjRepo::run(&repo.repo, &["git", "init", remote.to_str().unwrap()]);
            JjRepo::run(
                &repo.replacement,
                &[
                    "git",
                    "remote",
                    "add",
                    "publication",
                    remote.join(".jj/repo/store/git").to_str().unwrap(),
                ],
            );
            JjRepo::run(
                &repo.replacement,
                &["git", "push", "--remote", "publication", "-b", "feature", "--allow-new"],
            );
            JjRepo::run(&remote, &["git", "import"]);
            assert_eq!(
                log(&remote, "feature", "commit_id"),
                log(&repo.replacement, "feature", "commit_id")
            );
        }
    }
}

#[tokio::test]
async fn redispatch_preserves_authored_empty_and_undescribed_nonempty_changes() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    origin(&repo);
    let prior = create(&LocalJj, &repo.worker, "exec_authored", "local").await.unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Intentional checkpoint"]);
    JjRepo::run(&repo.worker, &["new"]);
    std::fs::write(repo.worker.join("unfinished"), "work without a message\n").unwrap();
    checkpoint(&repo.worker, &prior);
    restore_rebased(&LocalJj, &prior, &repo.replacement, None, "main", "origin")
        .await
        .unwrap();
    assert!(
        !log(
            &repo.replacement,
            "(main..@) & empty() & description(substring:\"Intentional checkpoint\")",
            "commit_id"
        )
        .is_empty()
    );
    assert!(
        !log(
            &repo.replacement,
            "(main..@) & ~empty() & description(exact:\"\")",
            "commit_id"
        )
        .is_empty()
    );
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("unfinished")).unwrap(),
        "work without a message\n"
    );
}

#[tokio::test]
async fn redispatch_preserves_empty_merge_topology() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    origin(&repo);
    let prior = create(&LocalJj, &repo.worker, "exec_merge", "local").await.unwrap();
    std::fs::write(repo.worker.join("left"), "left change\n").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Left change"]);
    let left = log(&repo.worker, "@", "commit_id");
    JjRepo::run(&repo.worker, &["new", "main", "-m", "Right change"]);
    std::fs::write(repo.worker.join("right"), "right change\n").unwrap();
    JjRepo::run(&repo.worker, &["new", &left, "@"]);
    checkpoint(&repo.worker, &prior);
    restore_rebased(&LocalJj, &prior, &repo.replacement, None, "main", "origin")
        .await
        .unwrap();
    assert_eq!(
        log(
            &repo.replacement,
            "(main..@) & merges() & empty() & description(exact:\"\")",
            "parents.len()"
        ),
        "2"
    );
    for file in ["left", "right"] {
        assert!(repo.replacement.join(file).exists());
    }
}
