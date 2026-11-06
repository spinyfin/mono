use super::helpers::*;
use crate::coordinator::{CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceStatus, FreshStart};
use boss_engine_recovery::execution_bookmark::{self as recovery, LocalJj};
use boss_engine_test_git::jj::JjRepo;

fn commit(repo: &std::path::Path, rev: &str) -> String {
    JjRepo::run(repo, &["log", "--no-graph", "-r", rev, "-T", "commit_id"])
}

fn advance_trunk(repo: &std::path::Path) {
    JjRepo::run(repo, &["new", "main", "-m", "Upstream advance"]);
    std::fs::write(repo.join("upstream.txt"), "someone else's work").unwrap();
    JjRepo::run(repo, &["bookmark", "set", "main", "-r", "@"]);
}

/// Two concurrent operations move the recovery head to different commits,
/// leaving the bookmark conflicted: a genuine integrity failure (unlike a
/// rewritten or missing baseline, which is not one). The publication head
/// keeps the unpublished work, so archives still name it.
fn conflict_recovery_head(repo: &std::path::Path, record: &recovery::ExecutionBookmark) {
    let operation = JjRepo::run(repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
    for target in [commit(repo, "main"), commit(repo, "root()")] {
        JjRepo::run(
            repo,
            &[
                "--at-op",
                operation.trim(),
                "bookmark",
                "set",
                &record.head(),
                "-r",
                &target,
                "--allow-backwards",
            ],
        );
    }
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
    if !disabled_source {
        // Two concurrent operations move the recovery head to different
        // commits, leaving the bookmark conflicted: a genuine integrity failure
        // (unlike a rewritten or missing baseline, which is not one).
        let operation = JjRepo::run(&repo.repo, &["op", "log", "--no-graph", "-n", "1", "-T", "id"]);
        for target in [commit(&repo.repo, "main"), current.clone()] {
            JjRepo::run(
                &repo.repo,
                &[
                    "--at-op",
                    operation.trim(),
                    "bookmark",
                    "set",
                    &record.head(),
                    "-r",
                    &target,
                    "--allow-backwards",
                ],
            );
        }
        let failure = recovery::diff(&LocalJj, &record).await.unwrap_err();
        assert_eq!(
            recovery::pointer_integrity_kind(&failure),
            Some(recovery::PointerIntegrityKind::Other)
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
        // The guide must use its capture even after the PR advances.
        JjRepo::run(&repo.repo, &["new", "@", "-m", "PR advances after capture"]);
        std::fs::write(repo.repo.join("shared.txt"), "newer published work\n").unwrap();
        JjRepo::run(&repo.repo, &["bookmark", "set", "pr/99", "-r", "@"]);
        assert_ne!(commit(&repo.repo, "pr/99"), current);
    }
    if pr {
        // Dispatch snapshots the bound PR's head before creating the pointer.
        db.set_execution_pr_head_before(&next.id, &current).unwrap();
    }
    let next = db.get_execution(&next.id).unwrap();
    let source_pointers = JjRepo::run(&repo.repo, &["bookmark", "list", &format!("glob:*{source}*")]);
    let trunk_then = commit(&repo.repo, "main");
    let shared = repo.repo.clone();
    let cube = Arc::new(FakeCubeClient {
        workspace_root: Some(dir.path().to_path_buf()),
        next_workspace_id: Mutex::new(Some("replacement".into())),
        real_bookmarks: true,
        position_real_workspace: true,
        on_create_change: Some(Arc::new(move |_| advance_trunk(&shared))),
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
        coordinator.set_host_adapter_provider(Arc::new(ScratchHostProvider {
            local: coordinator.host_adapter(),
            unavailable_source: false,
        }));
    }
    coordinator.set_review_pool(WorkerPool::new_review(1));
    let coordinator = Arc::new(coordinator);
    let worker = coordinator
        .pool_for_execution_for_test(&next)
        .claim_worker(&next.id, None)
        .await
        .unwrap();
    coordinator
        .schedule_execution_for_test(&next, &worker, DispatchAdmission::Queued)
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
    let expected = if pr { current } else { trunk_then };
    assert_eq!(fresh.base_commit.as_deref(), Some(expected.as_str()));
    if !pr {
        assert_ne!(commit(&repo.repo, "main"), expected);
    }
    assert!(recovery::diff(&LocalJj, &fresh).await.unwrap().is_empty());
    // Scheduler passes must not refire the running attempt or lease again.
    for _ in 0..3 {
        coordinator.drain_ready_queue_for_test().await;
        assert!(
            !db.list_ready_executions()
                .unwrap()
                .iter()
                .any(|ready| ready.id == next.id)
        );
    }
    assert_eq!(db.get_execution(&next.id).unwrap().status, ExecutionStatus::Running);
    assert_eq!(db.get_execution(&next.id).unwrap().pre_start_failure_count, 0);
    assert_eq!(cube.lease_calls.lock().await.len(), 1);
    let archive = format!("preserved-archive/{source}/{preserved}");
    if kind.is_read_only_review() {
        assert_eq!(
            JjRepo::run(&repo.repo, &["bookmark", "list", &format!("glob:*{source}*")]),
            source_pointers
        );
        assert_eq!(db.execution_bookmark(&source).unwrap(), record);
        assert!(db.execution_bookmark_retired_reason(&source).unwrap().is_none());
        assert!(
            db.list_attention_items_for_work_item(&db.owning_work_item_id(&next.work_item_id))
                .unwrap()
                .is_empty()
        );
        assert!(
            JjRepo::run(
                &repo.repo,
                &["bookmark", "list", &format!("glob:preserved-archive/{source}/*")]
            )
            .is_empty()
        );
        assert!(
            db.list_open_attention_items_of_kind(crate::execution_bookmark_recovery::CHAIN_BLOCKED)
                .unwrap()
                .is_empty()
        );
        return;
    }
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
    if disabled_source {
        assert!(
            notes[0]
                .body_markdown
                .contains("recovery store is on host local; dispatch selected destination")
        );
    }
    if !resume {
        for name in [record.base(), record.head()] {
            assert!(
                JjRepo::run(
                    &repo.repo,
                    &[
                        "log",
                        "--no-graph",
                        "-r",
                        &format!("bookmarks(exact:{name})"),
                        "-T",
                        "commit_id"
                    ]
                )
                .is_empty()
            );
        }
        assert_eq!(commit(&repo.repo, &record.publication()), preserved);
    }
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
    let records = if disabled_source {
        assert!(
            !candidates.iter().any(|candidate| candidate.execution.id == next.id),
            "local prune sweep excludes remote records"
        );
        vec![fresh]
    } else {
        assert!(candidates.iter().any(|candidate| candidate.execution.id == next.id));
        candidates
            .into_iter()
            .map(|candidate| db.execution_bookmark(&candidate.execution.id).unwrap())
            .collect()
    };
    for live in records {
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

struct ScratchHostProvider {
    local: Arc<dyn crate::host_adapter::HostAdapter>,
    unavailable_source: bool,
}

#[async_trait]
impl crate::host_adapter::HostAdapterProvider for ScratchHostProvider {
    async fn adapter_for(
        &self,
        host: &crate::host_registry::Host,
    ) -> Result<Arc<dyn crate::host_adapter::HostAdapter>> {
        if host.id == "local" {
            anyhow::ensure!(!self.unavailable_source, "source transport unavailable");
            return Ok(self.local.clone());
        }
        Ok(Arc::new(ScratchDestination {
            inner: self.local.clone(),
        }))
    }
}

#[tokio::test]
async fn unreachable_source_transport_retries_without_retiring_source() {
    source_host_dispatch(false).await;
}

#[tokio::test]
async fn full_remote_source_waits_without_retiring_recoverable_work() {
    source_host_dispatch(true).await;
}

async fn source_host_dispatch(full: bool) {
    let dir = tempdir().unwrap();
    let (db, prior, next) = super::blocked_workspace::blocked_pair(&dir.path().join("boss.db"));
    seed_local_claude_driver(&db);
    db.add_host("destination", "user@destination", 2, &[]).unwrap();
    db.add_host("source", "user@source", 1, &[]).unwrap();
    for host in ["destination", "source"] {
        crate::test_support::insert_host_capability(&db, host, "driver=claude", "auto");
    }
    db.set_host_enabled("local", false).unwrap();
    db.set_host_enabled("source", full).unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = 'task_implementation' WHERE id IN (?1, ?2)",
            rusqlite::params![prior.id, next.id],
        )
        .unwrap();
    if full {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_runs SET status = 'active', host_id = 'source' WHERE execution_id = ?1",
                [&prior.id],
            )
            .unwrap();
        assert_eq!(db.active_runs_per_host().unwrap().get("source"), Some(&1));
    }
    let next = db.get_execution(&next.id).unwrap();
    let repo = JjRepo::new(dir.path());
    super::recovery::configure_recovery_origin(&repo.repo);
    let record = recovery::create(&LocalJj, &repo.worker, &prior.id, "source")
        .await
        .unwrap();
    db.record_execution_bookmark(&record).unwrap();
    std::fs::write(repo.worker.join("work.txt"), "intact source work").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let old = commit(&repo.repo, &record.head());
    let cube = Arc::new(FakeCubeClient {
        real_bookmarks: true,
        position_real_workspace: true,
        workspace_root: Some(dir.path().to_owned()),
        next_workspace_id: Mutex::new(Some("replacement".into())),
        ..Default::default()
    });
    let mut coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..Default::default()
        }),
    );
    coordinator.set_host_adapter_provider(Arc::new(UnreachableSourceProvider(coordinator.host_adapter())));
    let coordinator = Arc::new(coordinator);
    let worker = coordinator.worker_pool().claim_worker(&next.id, None).await.unwrap();
    let result = coordinator
        .schedule_execution_for_test(&next, &worker, DispatchAdmission::Queued)
        .await;
    if full {
        let err = result.unwrap_err();
        assert!(format!("{err:#}").contains("no free slot"), "{err:#}");
        assert!(db.execution_bookmark_optional(&prior.id).unwrap().is_some());
        assert!(db.execution_bookmark_retired_reason(&prior.id).unwrap().is_none());
        assert!(cube.lease_calls.lock().await.is_empty());
    } else {
        let error = result.unwrap_err();
        assert!(
            format!("{error:#}").contains("source transport unavailable"),
            "{error:#}"
        );
        let next = db.get_execution(&next.id).unwrap();
        assert!(next.started_at.is_none());
        assert_eq!(next.pre_start_failure_count, 1);
        assert!(next.dispatch_not_before.is_some());
        assert!(db.execution_bookmark_optional(&prior.id).unwrap().is_some());
        assert!(db.execution_bookmark_retired_reason(&prior.id).unwrap().is_none());
    }
    for name in [record.head(), record.publication()] {
        assert_eq!(commit(&repo.repo, &name), old);
    }
    assert!(
        JjRepo::run(
            &repo.repo,
            &["bookmark", "list", &format!("glob:preserved-archive/{}/*", prior.id)]
        )
        .is_empty()
    );
}

struct UnreachableSourceProvider(Arc<dyn crate::host_adapter::HostAdapter>);

#[async_trait]
impl crate::host_adapter::HostAdapterProvider for UnreachableSourceProvider {
    async fn adapter_for(
        &self,
        host: &crate::host_registry::Host,
    ) -> Result<Arc<dyn crate::host_adapter::HostAdapter>> {
        anyhow::ensure!(host.id == "destination", "source transport unavailable");
        Ok(Arc::new(ScratchDestination { inner: self.0.clone() }))
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
    let stale = recovery::create(&LocalJj, &repo.replacement, &next.id, "local")
        .await
        .unwrap();
    std::fs::write(repo.replacement.join("stale.txt"), "distinct stale work").unwrap();
    JjRepo::run(&repo.replacement, &["status"]);
    let stale_head = commit(&repo.repo, &stale.head());
    JjRepo::run(&repo.replacement, &["new", &head, "-m", "Adopt predecessor"]);
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
        .create_or_start_fresh_for_test(
            &next,
            &lease,
            &coordinator.host_adapter(),
            (Some(&source), None),
            FreshStart {
                repo_id: "mono",
                immutable_target: None,
                bound_pr: None,
                task_pr: None,
                title: "Fresh",
            },
        )
        .await
        .unwrap();
    assert!(retained);
    let archive = format!("preserved-archive/{}/{stale_head}", next.id);
    assert_eq!(commit(&repo.repo, &archive), stale_head);
    let notes = db.list_attention_items_for_work_item(&next.work_item_id).unwrap();
    assert!(notes.iter().any(|note| note.body_markdown.contains(&archive)
        && note.title.contains("continuing predecessor adoption")));
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
        .schedule_execution_for_test(&next, &worker, DispatchAdmission::Queued)
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
async fn automated_review_keeps_historical_pointers_and_starts_without_refiring() {
    fresh_start(ExecutionKind::PrReview, false, true).await;
}

#[tokio::test]
async fn review_guide_keeps_historical_pointers_and_starts_at_captured_target() {
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
    conflict_recovery_head(&repo.repo, &record);
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
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..Default::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(2),
        Arc::new(FakeCubeClient {
            real_bookmarks: true,
            closed_pr: true,
            position_real_workspace: true,
            workspace_root: Some(dir.path().to_owned()),
            ..Default::default()
        }),
        runner.clone(),
    ));
    coordinator
        .resume_pane_spawn_for_running_execution(&next)
        .await
        .unwrap();
    assert_eq!(commit(&repo.repo, &format!("preserved-archive/{}/{old}", next.id)), old);
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
    let source_id = if inherited { &source.id } else { &next.id };
    let repo = JjRepo::new(dir.path());
    super::recovery::configure_recovery_origin(&repo.repo);
    let prior = recovery::create(&LocalJj, &repo.worker, source_id, "local")
        .await
        .unwrap();
    std::fs::write(repo.worker.join("lost.txt"), "keep this work").unwrap();
    JjRepo::run(&repo.worker, &["status"]);
    let old = commit(&repo.repo, &prior.head());
    if inherited {
        // The source head became ambiguous between recovery and adoption.
        db.record_execution_bookmark(&prior).unwrap();
        conflict_recovery_head(&repo.repo, &prior);
    }
    // Recovery has already staged unpublished history in the destination.
    JjRepo::run(&repo.replacement, &["new", &old, "-m", "Restaged work"]);
    assert!(repo.replacement.join("lost.txt").exists());
    // The engine crashed after writing pointers but before inserting the row.
    assert!(db.execution_bookmark_optional(&next.id).unwrap().is_none());
    let cube = Arc::new(FakeCubeClient {
        real_bookmarks: true,
        closed_pr: true,
        position_real_workspace: true,
        workspace_root: Some(dir.path().to_owned()),
        next_workspace_id: Mutex::new(Some("replacement".into())),
        ..Default::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..Default::default()
        }),
    ));
    let lease = CubeWorkspaceLease {
        lease_id: "lease".into(),
        workspace_id: "replacement".into(),
        workspace_path: repo.replacement.clone(),
        dirty_verified: Some(false),
    };
    let fresh = if !inherited {
        let worker = coordinator.worker_pool().claim_worker(&next.id, None).await.unwrap();
        coordinator
            .schedule_execution_for_test(&next, &worker, DispatchAdmission::Queued)
            .await
            .unwrap();
        assert_eq!(db.get_execution(&next.id).unwrap().status, ExecutionStatus::Running);
        assert_eq!(db.get_execution(&next.id).unwrap().pre_start_failure_count, 0);
        db.execution_bookmark(&next.id).unwrap()
    } else {
        let (fresh, retained) = coordinator
            .create_or_start_fresh_for_test(
                &next,
                &lease,
                &coordinator.host_adapter(),
                (inherited.then_some(&prior), None),
                FreshStart {
                    repo_id: "mono",
                    immutable_target: None,
                    bound_pr: None,
                    task_pr: Some(99),
                    title: "Fresh work",
                },
            )
            .await
            .unwrap();
        assert!(!retained);
        fresh
    };
    assert_eq!(fresh.base_commit.unwrap(), commit(&repo.repo, "main"));
    assert!(!repo.replacement.join("lost.txt").exists());
    assert_eq!(commit(&repo.repo, &format!("preserved-archive/{source_id}/{old}")), old);
    assert_eq!(
        db.list_attention_items_for_work_item(&next.work_item_id).unwrap()[0].status,
        "resolved"
    );
}

struct ScratchDestination {
    inner: Arc<dyn crate::host_adapter::HostAdapter>,
}

#[async_trait]
impl crate::host_adapter::HostAdapter for ScratchDestination {
    fn host_id(&self) -> &str {
        "destination"
    }
    async fn start_fresh_workspace(&self, workspace: &Path) -> Result<String> {
        self.inner.start_fresh_workspace(workspace).await
    }
    async fn release_execution_bookmark_heads(
        &self,
        record: &recovery::ExecutionBookmark,
        release_names: bool,
    ) -> Result<Vec<(String, String)>> {
        self.inner.release_execution_bookmark_heads(record, release_names).await
    }
    async fn create_execution_bookmark(
        &self,
        workspace: &Path,
        execution_id: &str,
        predecessor: Option<&recovery::ExecutionBookmark>,
        inherited_base: Option<&str>,
    ) -> Result<recovery::ExecutionBookmark> {
        let mut record = self
            .inner
            .create_execution_bookmark(workspace, execution_id, predecessor, inherited_base)
            .await?;
        record.host_id = "destination".into();
        Ok(record)
    }
    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
        self.inner.ensure_repo(origin).await
    }

    async fn lease_workspace(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
        allow_dirty: bool,
        exclude_workspace_ids: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        self.inner
            .lease_workspace(repo_id, task, prefer_workspace_id, allow_dirty, exclude_workspace_ids)
            .await
    }

    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        self.inner.release_workspace(lease_id).await
    }

    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
        self.inner.heartbeat_lease(lease_id, ttl_seconds).await
    }

    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
        self.inner.force_release_lease(lease_id, reason).await
    }

    async fn create_change(&self, workspace_path: &Path, title: &str) -> Result<CubeChangeHandle> {
        self.inner.create_change(workspace_path, title).await
    }

    async fn positioned_parent(&self, workspace: &Path) -> Result<String> {
        self.inner.positioned_parent(workspace).await
    }

    async fn goto_workspace(&self, workspace_path: &Path, pr: u64) -> Result<()> {
        self.inner.goto_workspace(workspace_path, pr).await
    }

    async fn goto_workspace_revision(&self, workspace_path: &Path, sha: &str) -> Result<()> {
        self.inner.goto_workspace_revision(workspace_path, sha).await
    }

    async fn workspace_status(&self, workspace_path: &Path) -> Result<CubeWorkspaceStatus> {
        self.inner.workspace_status(workspace_path).await
    }

    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        self.inner.list_workspaces().await
    }

    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        self.inner.list_repos().await
    }

    fn command_repr(&self, args: &[&str]) -> Option<(String, String)> {
        self.inner.command_repr(args)
    }

    async fn spawn_worker(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome> {
        self.inner
            .spawn_worker(worker_id, execution, work_item, workspace_path, cube_change_id)
            .await
    }
}

#[tokio::test]
async fn fresh_pointer_floor_is_the_positioned_commit_when_trunk_advances_afterwards() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Fresh work");
    db.reconcile_product_executions(&product.id).unwrap();
    let id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    let repo = JjRepo::new(dir.path());
    super::recovery::configure_recovery_origin(&repo.repo);
    let trunk_then = commit(&repo.repo, "main");
    let shared = repo.repo.clone();
    let positioned = Arc::new(std::sync::Mutex::new(None::<String>));
    let positioned_at_create = Arc::clone(&positioned);
    let cube = Arc::new(
        FakeCubeClient {
            workspace_root: Some(dir.path().to_path_buf()),
            real_bookmarks: true,
            position_real_workspace: true,
            on_create_change: Some(Arc::new(move |workspace| {
                *positioned_at_create.lock().unwrap() = Some(commit(workspace, "@-"));
                advance_trunk(&shared);
            })),
            ..Default::default()
        }
        .with_next_workspace_id("worker"),
    );
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..Default::default()
        }),
    ));
    coordinator.kick();
    wait_for_execution_status(&db, &id, ExecutionStatus::Running).await;
    let trunk_now = commit(&repo.repo, "main");
    let floor = positioned.lock().unwrap().clone().expect("create_change captures @-");
    assert_ne!(trunk_now, trunk_then);
    assert_ne!(
        trunk_now, floor,
        "the floor must be the positioned commit, not trunk() re-resolved after it advanced"
    );
    assert_eq!(
        db.execution_bookmark(&id).unwrap().base_commit.as_deref(),
        Some(floor.as_str())
    );
}

#[tokio::test]
async fn failed_positioned_commit_capture_retries_without_creating_a_pointer() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Fresh work");
    db.reconcile_product_executions(&product.id).unwrap();
    let id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    let repo = JjRepo::new(dir.path());
    super::recovery::configure_recovery_origin(&repo.repo);
    let trunk_then = commit(&repo.repo, "main");
    let shared = repo.repo.clone();
    let positioned = Arc::new(std::sync::Mutex::new(None::<String>));
    let positioned_at_create = Arc::clone(&positioned);
    let cube = Arc::new(
        FakeCubeClient {
            workspace_root: Some(dir.path().to_path_buf()),
            real_bookmarks: true,
            position_real_workspace: true,
            fail_first_n_positioned_parent: AtomicUsize::new(1),
            on_create_change: Some(Arc::new(move |workspace| {
                *positioned_at_create.lock().unwrap() = Some(commit(workspace, "@-"));
                advance_trunk(&shared);
            })),
            ..Default::default()
        }
        .with_next_workspace_id("worker"),
    );
    let sink = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..Default::default()
            }),
        )
        .with_pre_start_retry_delays(vec![Duration::from_millis(5)])
        .with_dispatch_events(sink.clone()),
    );
    coordinator.kick();
    for _ in 0..200 {
        let events = sink.events_for(&id).await;
        if events.iter().any(|event| event.stage == "pre_start_failure") {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    let events = sink.events_for(&id).await;
    let failure = events
        .iter()
        .find(|event| event.stage == "pre_start_failure")
        .expect("capture failure must be a pre-start failure");
    assert_eq!(failure.details["will_retry"], true);
    assert!(
        failure
            .error_message
            .as_deref()
            .unwrap()
            .contains("could not capture the positioned commit")
    );
    assert!(db.execution_bookmark_optional(&id).unwrap().is_none());
    wait_for_execution_status(&db, &id, ExecutionStatus::Running).await;
    let trunk_now = commit(&repo.repo, "main");
    let floor = positioned
        .lock()
        .unwrap()
        .clone()
        .expect("retry create_change captures @-");
    assert_ne!(trunk_now, trunk_then);
    assert_ne!(
        trunk_now, floor,
        "the retry must record the positioned commit, not a re-resolved trunk"
    );
    assert_eq!(
        db.execution_bookmark(&id).unwrap().base_commit.as_deref(),
        Some(floor.as_str()),
        "the retry must record the positioned commit, not a re-resolved trunk"
    );
}
