use super::helpers::*;
use boss_engine_recovery::execution_bookmark::{self as recovery, LocalJj};
use boss_engine_test_git::jj::JjRepo;

#[tokio::test]
async fn task_and_chore_recovery_keeps_verified_pr_files_without_persisting_a_baseline() {
    for kind in [ExecutionKind::TaskImplementation, ExecutionKind::ChoreImplementation] {
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        let (db, prior, mut next) = super::blocked_workspace::blocked_pair(&path);
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE work_executions SET kind = ?1 WHERE id IN (?2, ?3)",
                rusqlite::params![kind.as_str(), prior.id, next.id],
            )
            .unwrap();
        next.kind = kind;
        let repo = JjRepo::new(dir.path());
        super::recovery::configure_recovery_origin(&repo.repo);
        let mut record = boss_engine_recovery_fixtures::create(&LocalJj, &repo.worker, &prior.id, "local")
            .await
            .unwrap();
        record.base_commit = None;
        db.record_execution_bookmark(&record).unwrap();
        std::fs::write(repo.worker.join("pr-only.txt"), "published implementation").unwrap();
        JjRepo::run(&repo.worker, &["describe", "-m", "Published implementation"]);
        JjRepo::run(&repo.worker, &["bookmark", "set", "pr/99", "-r", "@"]);
        super::recovery::preserve_work(&db, &prior.id, &repo.worker).await;
        let cube = Arc::new(FakeCubeClient {
            workspace_root: Some(dir.path().to_path_buf()),
            real_bookmarks: true,
            position_real_workspace: true,
            ..Default::default()
        });
        let coordinator = ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        );
        let lease = CubeWorkspaceLease {
            dirty_verified: Some(false),
            lease_id: "lease".into(),
            workspace_id: "replacement".into(),
            workspace_path: repo.replacement.clone(),
        };
        coordinator
            .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", None, Some(99))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.replacement.join("pr-only.txt")).unwrap(),
            "published implementation"
        );
        let reloaded = db.execution_bookmark(&prior.id).unwrap();
        // A record with no usable baseline commit is recovered against trunk:
        // nothing is repaired and the row is left as recorded.
        assert!(reloaded.base_commit.is_none());
        // Retry before the new run starts; the recorded PR still owns positioning.
        coordinator
            .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", None, Some(99))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.replacement.join("pr-only.txt")).unwrap(),
            "published implementation"
        );
        assert!(
            db.record_execution_bookmark(&reloaded).is_err(),
            "live duplicates must fail"
        );
        db.retire_execution_bookmark(&prior.id, "test retirement").unwrap();
        db.record_execution_bookmark(&reloaded).unwrap();
    }
}

#[tokio::test]
async fn checkout_of_predecessor_publication_cannot_hide_unpublished_work() {
    let dir = tempdir().unwrap();
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    let repo = JjRepo::new(dir.path());
    super::recovery::configure_recovery_origin(&repo.repo);
    let record = boss_engine_recovery_fixtures::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("published.txt"), "published").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Published"]);
    JjRepo::run(&repo.worker, &["bookmark", "set", "pr/99", "-r", "@"]);
    // A worker names its publication branch itself before it publishes.
    let publication = format!("boss/{}", prior.id);
    JjRepo::run(
        &repo.repo,
        &[
            "new",
            record.base_commit.as_deref().unwrap(),
            "-m",
            "Unpublished publication",
        ],
    );
    std::fs::write(repo.repo.join("unpublished.txt"), "unique work").unwrap();
    JjRepo::run(
        &repo.repo,
        &["bookmark", "set", &publication, "-r", "@", "--allow-backwards"],
    );
    super::recovery::preserve_work(&db, &prior.id, &repo.repo).await;
    let before = bookmark_targets(&record);
    let cube = Arc::new(FakeCubeClient {
        workspace_root: Some(dir.path().to_path_buf()),
        real_bookmarks: true,
        position_real_workspace: true,
        goto_reset_bookmark: Some(publication.clone()),
        ..Default::default()
    });
    let coordinator = ExecutionCoordinator::new(db, WorkerPool::new(1), cube, Arc::new(FakeExecutionRunner::default()));
    let lease = CubeWorkspaceLease {
        dirty_verified: Some(false),
        lease_id: "lease".into(),
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
    };
    // Teardown saved the unpublished tip before checkout moves the publication
    // branch. The saved changes still restage on the current PR head.
    let recovered = coordinator
        .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", Some(99), Some(99))
        .await
        .unwrap();
    assert!(recovered.is_some_and(|r| r.has_work));
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("unpublished.txt")).unwrap(),
        "unique work"
    );
    for (name, commit) in before {
        if let Some(commit) = commit {
            assert_eq!(
                JjRepo::run(
                    &repo.replacement,
                    &["log", "--no-graph", "-r", &commit, "-T", "commit_id"]
                ),
                commit,
                "{name} stays reachable"
            );
        }
    }
}

/// A read-only review inherits nothing from the reviewer before it: a damaged
/// predecessor record is not even
/// consulted, so the review dispatches on the PR's current head, the
/// pointers are not moved and nothing is retired or filed. This is the
/// mono#3201 shape: sixteen reviews died in pre-start recovery against one
/// predecessor record whose preserved changes the rewritten PR no longer
/// carried under their change ids.
#[tokio::test]
async fn read_only_review_ignores_a_damaged_predecessor_record() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let (db, prior, mut next) = super::blocked_workspace::blocked_pair(&path);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = 'pr_review' WHERE id IN (?1, ?2)",
            rusqlite::params![prior.id, next.id],
        )
        .unwrap();
    next.kind = ExecutionKind::PrReview;
    let repo = JjRepo::new(dir.path());
    let record = boss_engine_recovery_fixtures::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("published.txt"), "published").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Published"]);
    JjRepo::run(&repo.worker, &["bookmark", "set", "pr/99", "-r", "@"]);
    // A worker names its publication branch itself before it publishes.
    let publication = format!("boss/{}", prior.id);
    JjRepo::run(
        &repo.repo,
        &[
            "new",
            record.base_commit.as_deref().unwrap(),
            "-m",
            "Unpublished publication",
        ],
    );
    std::fs::write(repo.repo.join("unpublished.txt"), "unique work").unwrap();
    JjRepo::run(
        &repo.repo,
        &["bookmark", "set", &publication, "-r", "@", "--allow-backwards"],
    );
    let before = bookmark_targets(&record);
    let cube = Arc::new(FakeCubeClient {
        workspace_root: Some(dir.path().to_path_buf()),
        real_bookmarks: true,
        goto_reset_bookmark: Some(publication.clone()),
        ..Default::default()
    });
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    );
    let lease = CubeWorkspaceLease {
        dirty_verified: Some(false),
        lease_id: "lease".into(),
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
    };
    assert!(db.recovery_predecessor(&next).unwrap().is_none());
    let recovered = coordinator
        .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", Some(99), Some(99))
        .await
        .unwrap();
    assert!(recovered.is_none(), "{recovered:?}");
    assert_eq!(bookmark_targets(&record), before);
    assert!(
        db.execution_bookmark_optional(&prior.id).unwrap().is_some(),
        "not retired"
    );
    assert!(db.list_attention_items(&prior.id).unwrap().is_empty());
    assert!(db.execution_restore_report(&next.id).unwrap().is_none());

    let head = JjRepo::run(
        &repo.repo,
        &["log", "--no-graph", "-r", &publication, "-T", "commit_id"],
    );
    let operation = JjRepo::run(&repo.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    for message in ["left rewrite", "right rewrite"] {
        JjRepo::run(
            &repo.repo,
            &["--at-op", operation.trim(), "describe", head.trim(), "-m", message],
        );
    }
    let operation = JjRepo::run(&repo.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);

    // An unreadable store makes any attempted lineage inspection fail. Cover
    // both review kinds and their own retry record as well as the predecessor.
    for kind in [ExecutionKind::PrReview, ExecutionKind::PrReviewGuide] {
        next.kind = kind;
        assert!(
            coordinator
                .recover_execution_bookmark_for_test(
                    &next,
                    &lease,
                    &coordinator.host_adapter(),
                    "mono",
                    Some(99),
                    Some(99)
                )
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            JjRepo::run(&repo.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]),
            operation,
            "conflicted predecessor bookmarks must not be touched"
        );
        let mut unreadable = record.clone();
        unreadable.execution_id = next.id.clone();
        unreadable.repo_path = dir.path().join("unreadable-store");
        db.record_execution_bookmark(&unreadable).unwrap();
        assert!(
            coordinator
                .recover_execution_bookmark_for_test(
                    &next,
                    &lease,
                    &coordinator.host_adapter(),
                    "mono",
                    Some(99),
                    Some(99)
                )
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.execution_bookmark(&next.id).unwrap().repo_path,
            unreadable.repo_path,
            "the retry record is untouched"
        );
        db.delete_execution_bookmark_and_restore_report(&next.id).unwrap();
    }
}

#[tokio::test]
async fn closed_pr_with_a_missing_baseline_recovers_against_trunk_without_retiring() {
    let dir = tempdir().unwrap();
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    let repo = JjRepo::new(dir.path());
    let mut record = boss_engine_recovery_fixtures::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    record.base_commit = None;
    db.record_execution_bookmark(&record).unwrap();
    let cube = Arc::new(FakeCubeClient {
        workspace_root: Some(dir.path().to_path_buf()),
        real_bookmarks: true,
        closed_pr: true,
        ..Default::default()
    });
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    );
    let lease = CubeWorkspaceLease {
        dirty_verified: Some(false),
        lease_id: "lease".into(),
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
    };
    assert!(
        coordinator
            .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", None, Some(99))
            .await
            .unwrap()
            .is_none()
    );
    // A missing floor falls back to trunk, so nothing is retired.
    assert!(db.execution_bookmark_optional(&prior.id).unwrap().is_some());
}

#[tokio::test]
async fn abandoned_recorded_baseline_does_not_retire_the_record() {
    let dir = tempdir().unwrap();
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    let repo = JjRepo::new(dir.path());
    super::recovery::configure_recovery_origin(&repo.repo);
    let mut record = boss_engine_recovery_fixtures::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    // The record names cube's empty positioning commit as its baseline, which
    // is then abandoned: no successor, no PR to vouch.
    JjRepo::run(&repo.repo, &["new", record.base_commit.as_deref().unwrap()]);
    let empty = JjRepo::run(&repo.repo, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    JjRepo::run(&repo.repo, &["abandon", "@"]);
    record.base_commit = Some(empty);
    db.record_execution_bookmark(&record).unwrap();
    let cube = Arc::new(FakeCubeClient {
        workspace_root: Some(dir.path().to_path_buf()),
        real_bookmarks: true,
        ..Default::default()
    });
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    );
    let lease = CubeWorkspaceLease {
        dirty_verified: Some(false),
        lease_id: "lease".into(),
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
    };
    coordinator
        .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", None, None)
        .await
        .unwrap();
    assert!(db.execution_bookmark_optional(&prior.id).unwrap().is_some());
}

#[tokio::test]
async fn rewritten_history_needs_no_checkpoint() {
    let dir = tempdir().unwrap();
    let (db, prior, _) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    let repo = JjRepo::new(dir.path());
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "-r", "@"]);
    JjRepo::run(&repo.worker, &["new", "main", "-m", "PR implementation"]);
    std::fs::write(repo.worker.join("pr.txt"), "implementation").unwrap();
    JjRepo::run(&repo.worker, &["new", "-m", "Revision"]);
    let record = boss_engine_recovery_fixtures::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("revision.txt"), "revision").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    JjRepo::run(&repo.repo, &["new", "main", "-m", "New upstream"]);
    std::fs::write(repo.repo.join("upstream.txt"), "upstream").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "new-upstream", "-r", "@"]);
    JjRepo::run(&repo.worker, &["rebase", "-b", "@", "-d", "new-upstream"]);
    let reloaded = db.execution_bookmark(&prior.id).unwrap();
    assert_eq!(
        reloaded.base_commit, record.base_commit,
        "no checkpoint rewrites the row"
    );
}

/// Where a record's worker-created publication branch and its preserved tip
/// point now, as sorted commit ids joined by commas (`None` when absent). Tests
/// compare it before and after an operation to prove the bookmarks did not move.
pub(super) fn bookmark_targets(
    record: &boss_engine_recovery::execution_bookmark::ExecutionBookmark,
) -> Vec<(String, Option<String>)> {
    [
        format!("boss/{}", record.execution_id),
        recovery::preserved_name(&record.execution_id),
    ]
    .into_iter()
    .map(|name| {
        let out = JjRepo::run(
            &record.repo_path,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                &format!("bookmarks(exact:\"{name}\")"),
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        );
        let mut ids: Vec<&str> = out.lines().filter(|line| !line.is_empty()).collect();
        ids.sort_unstable();
        let target = (!ids.is_empty()).then(|| ids.join(","));
        (name, target)
    })
    .collect()
}
