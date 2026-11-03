use super::helpers::*;
use boss_engine_recovery::execution_bookmark::{self as recovery, LocalJj};
use boss_engine_test_git::jj::JjRepo;

#[tokio::test]
async fn dispatch_prefers_preserved_work_over_retired_legacy_pointers() {
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "pr/99", "-r", "@"]);
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
    let record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("fix"), "unpushed fix").unwrap();
    let tip = recovery::preserve(&LocalJj, &record, &repo.worker)
        .await
        .unwrap()
        .unwrap();
    db.record_preserved_tip(&prior.id, &tip).unwrap();
    db.retire_execution_bookmark(&prior.id, "legacy pointers retired")
        .unwrap();
    JjRepo::run(
        &repo.repo,
        &[
            "bookmark",
            "delete",
            &record.head(),
            &record.base(),
            &record.publication(),
        ],
    );
    let cube = Arc::new(FakeCubeClient {
        position_real_workspace: true,
        ..FakeCubeClient::default()
    });
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    );
    let lease = CubeWorkspaceLease {
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
        lease_id: "next".into(),
        dirty_verified: None,
    };
    let recovered = coordinator
        .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", Some(99), Some(99))
        .await
        .unwrap()
        .unwrap();
    assert!(recovered.has_work);
    assert_eq!(recovered.predecessor, prior.id);
    assert_eq!(
        std::fs::read_to_string(repo.replacement.join("fix")).unwrap(),
        "unpushed fix"
    );
    assert_eq!(
        JjRepo::run(&repo.replacement, &["log", "-r", "@", "--no-graph", "-T", "change_id"]),
        tip.change_id
    );
    assert!(db.execution_restore_report(&next.id).unwrap().is_some());
}

#[tokio::test]
async fn undecidable_preserved_bookmark_starts_fresh_and_raises_one_attention() {
    let dir = tempdir().unwrap();
    let repo = JjRepo::new(dir.path());
    let (db, prior, mut next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    JjRepo::run(&repo.repo, &["bookmark", "set", "main", "pr/99", "-r", "@"]);
    let record = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("fix"), "unpushed fix").unwrap();
    let tip = recovery::preserve(&LocalJj, &record, &repo.worker)
        .await
        .unwrap()
        .unwrap();
    db.record_preserved_tip(&prior.id, &tip).unwrap();
    let name = recovery::preserved_name(&prior.id);
    for title in ["First unrelated work", "Second unrelated work"] {
        JjRepo::run(&repo.repo, &["new", "main", "-m", title]);
        std::fs::write(repo.repo.join(title), title).unwrap();
        JjRepo::run(&repo.repo, &["status"]);
    }
    let op = JjRepo::run(&repo.repo, &["op", "log", "-n", "1", "--no-graph", "-T", "id"]);
    for rev in [
        "description(substring:\"First unrelated\")",
        "description(substring:\"Second unrelated\")",
    ] {
        JjRepo::run(
            &repo.repo,
            &["--at-op", &op, "bookmark", "set", &name, "-r", rev, "--allow-backwards"],
        );
    }
    let rev = format!("bookmarks(exact:\"{name}\")");
    let before = JjRepo::run(
        &repo.repo,
        &["log", "--no-graph", "-r", &rev, "-T", "commit_id ++ \"\\n\""],
    );
    assert_eq!(before.lines().count(), 2);
    let cube = Arc::new(FakeCubeClient {
        position_real_workspace: true,
        ..FakeCubeClient::default()
    });
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    );
    let lease = CubeWorkspaceLease {
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
        lease_id: "next".into(),
        dirty_verified: None,
    };
    for _ in 0..2 {
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
    }
    assert!(!repo.replacement.join("fix").exists());
    assert_eq!(
        JjRepo::run(
            &repo.repo,
            &["log", "--no-graph", "-r", &rev, "-T", "commit_id ++ \"\\n\""]
        ),
        before
    );
    assert_eq!(
        db.list_attention_items(&prior.id)
            .unwrap()
            .iter()
            .filter(|item| item.kind == "preserved_work_not_restaged")
            .count(),
        1
    );
    let calls = cube.goto_calls.lock().await.len();
    next.kind = ExecutionKind::PrReview;
    assert!(
        coordinator
            .recover_execution_bookmark_for_test(&next, &lease, &coordinator.host_adapter(), "mono", Some(99), Some(99))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        cube.goto_calls.lock().await.len(),
        calls,
        "reviews never enter recovery"
    );
}
