use super::helpers::*;
use boss_engine_recovery::execution_bookmark::{self as recovery, LocalJj};
use boss_engine_test_git::jj::JjRepo;

fn commit(repo: &std::path::Path, rev: &str) -> String {
    JjRepo::run(repo, &["log", "--no-graph", "-r", rev, "-T", "commit_id"])
}

/// Both preserved work and the new PR edit the same file differently. The
/// baseline points at the new PR while the old heads retain the unpushed edit.
async fn fresh_start(kind: ExecutionKind, resume: bool, pr: bool) {
    fresh_start_on_host(kind, resume, pr, false).await;
}

async fn fresh_start_on_host(kind: ExecutionKind, resume: bool, pr: bool, disabled_source: bool) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&path);
    seed_local_claude_driver(&db);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?1 WHERE id = ?2",
            rusqlite::params![
                pr.then_some("https://github.com/spinyfin/mono/pull/99"),
                next.work_item_id
            ],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = ?1, pr_url = ?2 WHERE id IN (?3, ?4)",
            rusqlite::params![
                kind.as_str(),
                pr.then_some("https://github.com/spinyfin/mono/pull/99"),
                prior.id,
                next.id
            ],
        )
        .unwrap();
    let next = db.get_execution(&next.id).unwrap();
    let source = if resume { next.id.clone() } else { prior.id.clone() };
    let repo = JjRepo::new(dir.path());
    super::recovery::configure_recovery_origin(&repo.repo);
    let record = recovery::create(&LocalJj, &repo.worker, &source, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("shared.txt"), "unpublished, non-compiling work\n").unwrap();
    JjRepo::run(&repo.worker, &["describe", "-m", "Preserved edit"]);
    let preserved = commit(&repo.repo, &record.head());
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Rebased PR"]);
    std::fs::write(repo.repo.join("shared.txt"), "current published work\n").unwrap();
    JjRepo::run(&repo.repo, &["bookmark", "set", "pr/99", "-r", "@"]);
    let current = commit(&repo.repo, "@");
    JjRepo::run(
        &repo.repo,
        &["bookmark", "set", &record.base(), "-r", "@", "--allow-backwards"],
    );
    let failure = recovery::diff(&LocalJj, &record).await.unwrap_err();
    assert_eq!(
        recovery::pointer_integrity_kind(&failure),
        Some(recovery::PointerIntegrityKind::NotDescended)
    );
    if pr {
        let failure = recovery::reconcile(&LocalJj, &record, Some("pr/99"), "main")
            .await
            .unwrap_err();
        assert_eq!(
            recovery::pointer_integrity_kind(&failure),
            Some(recovery::PointerIntegrityKind::NotContained)
        );
    }
    if kind == ExecutionKind::PrReviewGuide {
        let packet = crate::test_support::review_guide_source_packet(&commit(&repo.repo, "main"), &current);
        let crate::work::PrSourceCapturePersistOutcome::Stored(capture) = db
            .persist_pr_review_guide_source_capture(
                &next.work_item_id,
                99,
                crate::work::PrSourceCaptureTrigger::Creation,
                &packet,
            )
            .unwrap()
        else {
            panic!("expected capture")
        };
        let attempt = db
            .create_pr_review_guide_attempt(
                &capture.series_id,
                &capture.comparison_id,
                boss_review_guide::PROMPT_VERSION,
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET work_item_id = ?1 WHERE id IN (?2, ?3)",
                rusqlite::params![capture.comparison_id, prior.id, next.id],
            )
            .unwrap();
        db.bind_pr_review_guide_attempt_execution(&attempt.id, &next.id)
            .unwrap();
    }
    let next = db.get_execution(&next.id).unwrap();
    let cube = Arc::new(FakeCubeClient {
        workspace_root: Some(dir.path().to_path_buf()),
        next_workspace_id: Mutex::new(Some("replacement".into())),
        real_bookmarks: true,
        position_real_workspace: true,
        ..Default::default()
    });
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..Default::default()
    });
    let mut coordinator = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    if disabled_source {
        db.add_host("destination", "user@destination", 2, &[]).unwrap();
        db.set_host_enabled("local", false).unwrap();
        crate::test_support::insert_host_capability(&db, "destination", "driver=claude", "auto");
        // Use the real local scratch store for the destination transport too.
        // Selection is verified from the persisted run host below.
        coordinator.set_host_adapter_provider(Arc::new(ScratchHostProvider(coordinator.host_adapter())));
    }
    coordinator.set_review_pool(WorkerPool::new_review(1));
    let coordinator = Arc::new(coordinator);
    let worker = coordinator
        .pool_for_execution(&next)
        .claim_worker(&next.id, None)
        .await
        .unwrap();
    coordinator
        .schedule_execution(&next, &worker, DispatchAdmission::Queued)
        .await
        .unwrap();
    assert_eq!(db.get_execution(&next.id).unwrap().status, ExecutionStatus::Running);
    assert_eq!(db.get_execution(&next.id).unwrap().pre_start_failure_count, 0);
    if disabled_source {
        assert_eq!(
            db.latest_run_host_for_execution(&next.id).unwrap().as_deref(),
            Some("destination")
        );
    }
    let fresh = db.execution_bookmark(&next.id).unwrap();
    let expected = if pr { current } else { commit(&repo.repo, "main") };
    assert_eq!(fresh.base_commit.as_deref(), Some(expected.as_str()));
    assert!(recovery::diff(&LocalJj, &fresh).await.unwrap().is_empty());
    let archive = format!("preserved/{source}/{preserved}");
    assert_eq!(commit(&repo.repo, &archive), preserved);
    assert!(JjRepo::run(&repo.repo, &["file", "show", "-r", &archive, "root:shared.txt"]).contains("non-compiling"));
    let notes = db
        .list_attention_items_for_work_item(&db.owning_work_item_id(&next.work_item_id))
        .unwrap();
    let notes: Vec<_> = notes
        .iter()
        .filter(|note| note.kind == crate::execution_bookmark_recovery::RECOVERY_RETIRED)
        .collect();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].status, "resolved");
    assert!(notes[0].body_markdown.contains(&archive));
    assert!(
        db.list_open_attention_items_of_kind(crate::execution_bookmark_recovery::CHAIN_BLOCKED)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        commit(&repo.repo, &archive),
        preserved,
        "later prune must retain archives"
    );
    {
        // Scheduler passes see the running review, rather than refiring the
        // failed pre-start attempt and accumulating recovery cards.
        for _ in 0..3 {
            coordinator.drain_ready_queue().await;
            assert!(
                !db.list_ready_executions()
                    .unwrap()
                    .iter()
                    .any(|ready| ready.id == next.id)
            );
        }
        assert_eq!(db.get_execution(&next.id).unwrap().status, ExecutionStatus::Running);
        assert_eq!(db.get_execution(&next.id).unwrap().pre_start_failure_count, 0);
        assert_eq!(
            cube.lease_calls.lock().await.len(),
            1,
            "scheduler must not refire the running execution"
        );
    }
    for name in [fresh.head(), fresh.publication()] {
        JjRepo::run(
            &repo.repo,
            &["bookmark", "set", &name, "-r", &preserved, "--allow-backwards"],
        );
    }
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed', finished_at = '1' WHERE id = ?1",
            [&next.id],
        )
        .unwrap();
    let candidates = db.list_prune_candidates(0, 0, 0, 100).unwrap();
    assert!(candidates.iter().any(|candidate| candidate.execution.id == next.id));
    for candidate in candidates {
        let live = db.execution_bookmark(&candidate.execution.id).unwrap();
        let outcome = recovery::prune(
            &LocalJj,
            &live,
            &[recovery::Voucher::commit("published", &preserved).unwrap()],
            "main",
        )
        .await
        .unwrap();
        assert!(matches!(outcome, recovery::PruneOutcome::Published { .. }));
    }
    assert_eq!(commit(&repo.repo, &archive), preserved);
}

struct ScratchHostProvider(Arc<dyn crate::host_adapter::HostAdapter>);

#[async_trait]
impl crate::host_adapter::HostAdapterProvider for ScratchHostProvider {
    async fn adapter_for(&self, _: &crate::host_registry::Host) -> Result<Arc<dyn crate::host_adapter::HostAdapter>> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn disabled_recovery_host_does_not_prevent_fresh_dispatch() {
    fresh_start_on_host(ExecutionKind::TaskImplementation, false, false, true).await;
}

#[tokio::test]
async fn task_archives_and_starts_from_main() {
    fresh_start(ExecutionKind::TaskImplementation, false, false).await;
}

#[tokio::test]
async fn stale_current_names_retry_adoption_without_retiring_valid_predecessor() {
    let dir = tempdir().unwrap();
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    let repo = JjRepo::new(dir.path());
    super::recovery::configure_recovery_origin(&repo.repo);
    let source = recovery::create(&LocalJj, &repo.worker, &prior.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&source).unwrap();
    std::fs::write(repo.worker.join("work.txt"), "recovered work").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let head = commit(&repo.repo, &source.head());
    JjRepo::run(&repo.replacement, &["new", &head, "-m", "Recovered"]);
    recovery::create(&LocalJj, &repo.replacement, &next.id, "local")
        .await
        .unwrap();
    let cube = Arc::new(FakeCubeClient {
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
        lease_id: "lease".into(),
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
        dirty_verified: None,
    };
    let (record, retained) = coordinator
        .create_or_start_fresh(
            &next,
            &lease,
            &coordinator.host_adapter,
            (Some(&source), None),
            (None, "Fresh"),
        )
        .await
        .unwrap();
    assert!(retained);
    assert!(db.execution_bookmark_optional(&prior.id).unwrap().is_some());
    assert!(
        recovery::diff(&LocalJj, &record)
            .await
            .unwrap()
            .contains("recovered work")
    );
    assert!(repo.replacement.join("work.txt").exists());
}

#[tokio::test]
async fn task_with_merged_pr_and_no_recovery_starts_without_pr_goto() {
    let dir = tempdir().unwrap();
    let (db, _, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    seed_local_claude_driver(&db);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = 'https://github.com/spinyfin/mono/pull/99' WHERE id = ?1",
            [&next.work_item_id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = 'task_implementation' WHERE id = ?1",
            [&next.id],
        )
        .unwrap();
    let next = db.get_execution(&next.id).unwrap();
    let cube = Arc::new(FakeCubeClient {
        closed_pr: true,
        ..Default::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..Default::default()
        }),
    ));
    let worker = coordinator.worker_pool().claim_worker(&next.id, None).await.unwrap();
    coordinator
        .schedule_execution(&next, &worker, DispatchAdmission::Queued)
        .await
        .unwrap();
    assert!(cube.goto_calls.lock().await.is_empty());
    assert_eq!(cube.create_calls.lock().await.len(), 1);
    assert_eq!(db.get_execution(&next.id).unwrap().status, ExecutionStatus::Running);
}

#[tokio::test]
async fn chore_archives_and_starts_from_pr() {
    fresh_start(ExecutionKind::ChoreImplementation, false, true).await;
}

#[tokio::test]
async fn revision_archives_and_starts_from_pr() {
    fresh_start(ExecutionKind::RevisionImplementation, false, true).await;
}

#[tokio::test]
async fn conflict_resolution_archives_and_starts_fresh() {
    fresh_start(ExecutionKind::ConflictResolution, false, true).await;
}

#[tokio::test]
async fn ci_fix_archives_and_starts_fresh() {
    fresh_start(ExecutionKind::CiRemediation, false, true).await;
}

#[tokio::test]
async fn automated_review_with_missing_preserved_work_starts_without_refiring() {
    fresh_start(ExecutionKind::PrReview, false, true).await;
}

#[tokio::test]
async fn review_guide_archives_and_starts_fresh() {
    fresh_start(ExecutionKind::PrReviewGuide, false, true).await;
}

#[tokio::test]
async fn resume_archives_before_reusing_its_pointer_names() {
    fresh_start(ExecutionKind::RevisionImplementation, true, true).await;
}

#[tokio::test]
async fn startup_pane_resume_archives_damaged_pointers_and_spawns() {
    let dir = tempdir().unwrap();
    let (db, _, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    seed_local_claude_driver(&db);
    let repo = JjRepo::new(dir.path());
    super::recovery::configure_recovery_origin(&repo.repo);
    let record = recovery::create(&LocalJj, &repo.worker, &next.id, "local")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("work.txt"), "keep unpublished work").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let old = commit(&repo.repo, &record.head());
    JjRepo::run(&repo.repo, &["new", "main", "-m", "Unrelated baseline"]);
    std::fs::write(repo.repo.join("other.txt"), "other work").unwrap();
    JjRepo::run(
        &repo.repo,
        &["bookmark", "set", &record.base(), "-r", "@", "--allow-backwards"],
    );
    let (next, _) = db
        .start_execution_run(
            &next.id,
            "worker-1",
            "mono",
            "lease",
            "worker",
            repo.worker.to_str().unwrap(),
        )
        .unwrap();
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..Default::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(2),
        Arc::new(FakeCubeClient {
            real_bookmarks: true,
            position_real_workspace: true,
            ..Default::default()
        }),
        runner.clone(),
    ));
    coordinator
        .resume_pane_spawn_for_running_execution(&next)
        .await
        .unwrap();
    assert_eq!(commit(&repo.repo, &format!("preserved/{}/{old}", next.id)), old);
    let fresh = db.execution_bookmark(&next.id).unwrap();
    assert_eq!(fresh.base_commit, Some(commit(&repo.repo, "main")));
    assert!(recovery::diff(&LocalJj, &fresh).await.unwrap().is_empty());
    assert!(!repo.worker.join("work.txt").exists());
    for _ in 0..100 {
        if !runner.calls.lock().await.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("startup resume never spawned the worker");
}

#[tokio::test]
async fn partial_pointer_initialization_archives_before_recreating_the_names() {
    initialization_fallback(false).await;
}

#[tokio::test]
async fn inherited_baseline_adoption_failure_archives_and_starts_fresh() {
    initialization_fallback(true).await;
}

async fn initialization_fallback(inherited: bool) {
    let dir = tempdir().unwrap();
    let (db, source, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    let source_id = if inherited { &source.id } else { &next.id };
    let repo = JjRepo::new(dir.path());
    super::recovery::configure_recovery_origin(&repo.repo);
    let mut prior = recovery::create(&LocalJj, &repo.worker, source_id, "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("lost.txt"), "keep this work").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let old = commit(&repo.repo, &prior.head());
    if inherited {
        // The source baseline disappeared between recovery and adoption.
        prior.base_commit = None;
        JjRepo::run(&repo.repo, &["bookmark", "forget", &prior.base()]);
        db.record_execution_bookmark(&prior).unwrap();
    }
    // Recovery has already staged unpublished history in the destination.
    JjRepo::run(&repo.replacement, &["new", &old, "-m", "Restaged work"]);
    assert!(repo.replacement.join("lost.txt").exists());
    // The engine crashed after writing pointers but before inserting the row.
    assert!(db.execution_bookmark_optional(&next.id).unwrap().is_none());
    let cube = Arc::new(FakeCubeClient {
        real_bookmarks: true,
        position_real_workspace: true,
        workspace_root: Some(dir.path().to_owned()),
        ..Default::default()
    });
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    );
    let lease = CubeWorkspaceLease {
        lease_id: "lease".into(),
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
        dirty_verified: Some(false),
    };
    let (fresh, retained) = coordinator
        .create_or_start_fresh(
            &next,
            &lease,
            &coordinator.host_adapter,
            (inherited.then_some(&prior), None),
            (None, "Fresh work"),
        )
        .await
        .unwrap();
    assert!(!retained);
    assert_eq!(fresh.base_commit.unwrap(), commit(&repo.repo, "main"));
    assert!(!repo.replacement.join("lost.txt").exists());
    assert_eq!(commit(&repo.repo, &format!("preserved/{source_id}/{old}")), old);
    assert_eq!(
        db.list_attention_items_for_work_item(&next.work_item_id).unwrap()[0].status,
        "resolved"
    );
}
