use super::*;
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    repo: PathBuf,
    worker: PathBuf,
    replacement: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let boss_engine_test_git::jj::JjRepo {
            repo,
            worker,
            replacement,
        } = boss_engine_test_git::jj::JjRepo::new(root.path());
        Self {
            root,
            repo,
            worker,
            replacement,
        }
    }

    async fn record(&self, id: &str) -> ExecutionBookmark {
        create(&LocalJj, &self.worker, id, "local").await.unwrap()
    }

    async fn edit(&self, record: &ExecutionBookmark) {
        std::fs::write(self.worker.join("work.txt"), "first change\n").unwrap();
        LocalJj
            .run(&self.worker, &["describe", "-m", "First change"])
            .await
            .unwrap();
        LocalJj
            .run(&self.worker, &["new", "-m", "Second change"])
            .await
            .unwrap();
        LocalJj
            .run(&self.worker, &["bookmark", "set", &record.head(), "-r", "@"])
            .await
            .unwrap();
        std::fs::write(self.worker.join("second.txt"), "second change\n").unwrap();
        LocalJj.run(&self.worker, &["status"]).await.unwrap();
    }
}

#[tokio::test]
async fn terminated_run_survives_released_removed_reused_and_foreign_leased_workspace() {
    for state in ["released", "removed", "reused", "leased_elsewhere"] {
        let f = Fixture::new().await;
        let record = f.record("exec_terminated").await;
        f.edit(&record).await;
        match state {
            "removed" => {
                LocalJj.run(&f.repo, &["workspace", "forget", "worker"]).await.unwrap();
                std::fs::remove_dir_all(&f.worker).unwrap();
            }
            "reused" | "leased_elsewhere" => {
                LocalJj
                    .run(&f.worker, &["new", "root()", "-m", "Unrelated lease"])
                    .await
                    .unwrap();
                std::fs::write(f.worker.join("foreign.txt"), "someone else's work\n").unwrap();
            }
            _ => {}
        }
        let patch = diff(&LocalJj, &record).await.unwrap();
        assert!(
            patch.contains("work.txt") && patch.contains("second.txt"),
            "{state}: {patch}"
        );
        assert!(!patch.contains("foreign.txt"), "{state}: {patch}");
        assert!(restore(&LocalJj, &record, &f.replacement).await.unwrap());
        assert_eq!(
            std::fs::read_to_string(f.replacement.join("second.txt")).unwrap(),
            "second change\n"
        );
        if state == "leased_elsewhere" {
            assert_eq!(
                std::fs::read_to_string(f.worker.join("foreign.txt")).unwrap(),
                "someone else's work\n"
            );
        }
    }
}

#[tokio::test]
async fn revision_bookmark_recovers_unpushed_work_after_its_workspace_is_removed() {
    let f = Fixture::new().await;
    let record = f.record("exec_revision").await;
    f.edit(&record).await;
    std::fs::remove_dir_all(&f.worker).unwrap();
    assert!(restore(&LocalJj, &record, &f.replacement).await.unwrap());
    assert!(f.replacement.join("work.txt").exists());
}

#[tokio::test]
async fn consecutive_recoveries_keep_inherited_work_when_the_next_run_makes_no_edits() {
    let f = Fixture::new().await;
    let prior = f.record("exec_prior").await;
    f.edit(&prior).await;
    assert!(restore(&LocalJj, &prior, &f.replacement).await.unwrap());
    let next = create_from(&LocalJj, &f.replacement, "exec_next", "local", Some(&prior))
        .await
        .unwrap();
    std::fs::remove_dir_all(&f.worker).unwrap();
    std::fs::remove_dir_all(&f.replacement).unwrap();
    let patch = unpublished_diff(&LocalJj, &next).await.unwrap();
    assert!(patch.contains("work.txt") && patch.contains("second.txt"));
}

#[tokio::test]
async fn publication_cleanup_cannot_remove_the_engine_owned_recovery_pointer() {
    let f = Fixture::new().await;
    let record = f.record("exec_retained").await;
    f.edit(&record).await;
    // Cube forgets consumed boss/exec_* refs. Its existing cleanup never owns
    // the engine's recovery namespace, and no pool coordination is required.
    LocalJj
        .run(&f.repo, &["bookmark", "forget", &record.publication()])
        .await
        .unwrap();
    std::fs::remove_dir_all(&f.worker).unwrap();
    assert!(diff(&LocalJj, &record).await.unwrap().contains("second.txt"));
    assert!(restore(&LocalJj, &record, &f.replacement).await.unwrap());
}

#[tokio::test]
async fn published_work_with_an_empty_working_child_is_not_reported_as_abandoned() {
    let f = Fixture::new().await;
    let record = f.record("exec_published").await;
    f.edit(&record).await;
    LocalJj.run(&f.worker, &["git", "export"]).await.unwrap();
    // A local transport fixture supplies actual remote-bookmark metadata
    // without contacting GitHub or publishing a branch.
    let git_store = f.repo.join(".jj/repo/store/git");
    LocalJj
        .run(
            &f.repo,
            &["git", "remote", "add", "origin", git_store.to_str().unwrap()],
        )
        .await
        .unwrap();
    LocalJj.run(&f.repo, &["git", "fetch"]).await.unwrap();
    LocalJj
        .run(&f.worker, &["new", "-m", "Post publication checkpoint"])
        .await
        .unwrap();
    LocalJj
        .run(&f.worker, &["bookmark", "set", &record.head(), "-r", "@"])
        .await
        .unwrap();
    assert!(!diff(&LocalJj, &record).await.unwrap().is_empty());
    assert!(unpublished_diff(&LocalJj, &record).await.unwrap().is_empty());
    assert!(!restore(&LocalJj, &record, &f.replacement).await.unwrap());
    assert!(!f.replacement.join("work.txt").exists());
}

#[tokio::test]
async fn empty_run_is_distinct_from_a_missing_or_unrelated_pointer() {
    let f = Fixture::new().await;
    let record = f.record("exec_empty").await;
    assert!(diff(&LocalJj, &record).await.unwrap().is_empty());
    LocalJj
        .run(
            &f.repo,
            &["bookmark", "set", &record.head(), "-r", "root()", "--allow-backwards"],
        )
        .await
        .unwrap();
    assert!(
        diff(&LocalJj, &record)
            .await
            .unwrap_err()
            .to_string()
            .contains("baseline")
    );
    LocalJj
        .run(&f.repo, &["bookmark", "delete", &record.head()])
        .await
        .unwrap();
    assert!(
        diff(&LocalJj, &record)
            .await
            .unwrap_err()
            .to_string()
            .contains("exactly one")
    );
    assert!(f.root.path().exists());
}
