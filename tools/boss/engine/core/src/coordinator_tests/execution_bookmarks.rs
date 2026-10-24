use super::helpers::*;
use boss_engine_recovery::execution_bookmark::{LocalJj, create};
use boss_engine_test_git::jj::JjRepo;

#[tokio::test]
async fn bookmark_creation_failure_is_recorded_before_a_worker_can_start() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Bookmark failure");
    db.reconcile_product_executions(&product.id).unwrap();
    let cube = Arc::new(FakeCubeClient {
        fail_bookmark_create: true,
        ..Default::default()
    });
    let runner = Arc::new(FakeExecutionRunner::default());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_pre_start_retry_delays(vec![]),
    );
    coordinator.kick();
    let id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    wait_for_execution_status(&db, &id, ExecutionStatus::Failed).await;
    assert!(
        runner.calls.lock().await.is_empty(),
        "a missing recovery ref must prevent spawn"
    );
    assert_eq!(cube.bookmark_calls.lock().await.as_slice(), [id.as_str()]);
    assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    let attention = db.list_attention_items(&id).unwrap();
    assert!(
        attention
            .iter()
            .any(|a| a.kind == crate::execution_bookmark_recovery::CREATE_FAILED
                && a.body_markdown.contains("jj bookmark create failed"))
    );
}

async fn wait_for_stage(
    sink: &crate::dispatch_events::RecordingDispatchEventSink,
    execution_id: &str,
    stage: &str,
) -> Vec<crate::dispatch_events::DispatchEvent> {
    for _ in 0..200 {
        let events = sink.events_for(execution_id).await;
        if events.iter().any(|event| event.stage == stage) {
            return events;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "no `{stage}` event for {execution_id}; got {:?}",
        sink.events_for(execution_id).await
    );
}

/// The dispatch event stream, not just the tracing log, must record a
/// pre-start failure. Without it the timeline stops at `cube_change_created
/// ok`, `bossctl dispatch diagnose` shows no failure, and `stage_stalled`
/// fires on a stage that had already failed.
#[tokio::test]
async fn bookmark_creation_failure_reaches_the_dispatch_event_stream() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Bookmark failure");
    db.reconcile_product_executions(&product.id).unwrap();
    let cube = Arc::new(FakeCubeClient {
        fail_bookmark_create: true,
        ..Default::default()
    });
    let sink = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![])
        .with_dispatch_events(sink.clone()),
    );
    coordinator.kick();
    let id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    let events = wait_for_stage(&sink, &id, "pre_start_retries_exhausted").await;

    let failure = events
        .iter()
        .find(|event| event.stage == "pre_start_failure")
        .expect("pre_start_failure event");
    assert_eq!(failure.outcome, "error");
    assert!(
        failure
            .error_message
            .as_deref()
            .is_some_and(|error| error.contains("jj bookmark create failed")),
        "{failure:?}"
    );
    assert_eq!(
        failure.details["reason"],
        crate::execution_bookmark_recovery::CREATE_FAILED
    );
    assert_eq!(failure.details["will_retry"], false);

    let exhausted = events
        .iter()
        .find(|event| event.stage == "pre_start_retries_exhausted")
        .expect("pre_start_retries_exhausted event");
    assert_eq!(exhausted.outcome, "error");
    assert_eq!(exhausted.details["pre_start_failure_count"], 1);

    // The failure is recorded as the timeline's terminal event, so the
    // stalled-stage detector has nothing to flag — no detector special case.
    let timeline = crate::dispatch_reader::TimelineState::from_events(&events).unwrap();
    assert!(!timeline.is_live(), "failed pre-start stage must not stay live");
    let thresholds = crate::dispatch_reader::StageThresholds::new(Duration::from_secs(1));
    assert!(timeline.stall_to_emit(&id, u128::MAX / 2, &thresholds).is_none());
}

/// A failure that will be retried is recorded too, flagged `will_retry`
/// with its backoff, and is not followed by a terminal "exhausted" event.
#[tokio::test]
async fn retried_pre_start_failure_is_recorded_with_its_backoff() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Bookmark failure");
    db.reconcile_product_executions(&product.id).unwrap();
    let cube = Arc::new(FakeCubeClient {
        fail_bookmark_create: true,
        ..Default::default()
    });
    let sink = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube,
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![Duration::from_secs(3600)])
        .with_dispatch_events(sink.clone()),
    );
    coordinator.kick();
    let id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    let events = wait_for_stage(&sink, &id, "pre_start_failure").await;
    let failure = events.iter().find(|event| event.stage == "pre_start_failure").unwrap();
    assert_eq!(failure.outcome, "error");
    assert_eq!(failure.details["will_retry"], true);
    assert_eq!(failure.details["retry_delay_secs"], 3600);
    assert_eq!(failure.details["pre_start_failure_count"], 1);
    assert!(events.iter().all(|event| event.stage != "pre_start_retries_exhausted"));
}

#[tokio::test]
async fn bookkeeping_failure_is_terminal_and_recovery_does_not_record_it_twice() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path.clone()).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Bookkeeping failure");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution = db.list_executions(Some(&chore.id)).unwrap().remove(0);
    db.claim_execution_for_dispatch(&execution.id).unwrap();
    let execution = db.get_execution(&execution.id).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_failure BEFORE UPDATE OF pre_start_failure_count ON work_executions
         BEGIN SELECT RAISE(ABORT, 'bookkeeping unavailable'); END;",
        )
        .unwrap();
    let sink = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            Arc::new(FakeCubeClient::default()),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_dispatch_events(sink.clone()),
    );
    let original = anyhow!("underlying bookmark error").context("bookmark creation failed");
    let error = coordinator
        .record_start_failure(
            coordinator.clone(),
            &execution,
            "worker-1",
            Some("mono"),
            ("execution_bookmark_create_failed", "Bookmark failed"),
            &original,
        )
        .await
        .unwrap_err();
    // Even if the database recovers, fallback must not count this attempt again.
    connection.execute_batch("DROP TRIGGER reject_failure").unwrap();
    coordinator
        .recover_failed_dispatch(&execution, "worker-1", &error)
        .await;
    let events = sink.events_for(&execution.id).await;
    assert_eq!(events.len(), 1, "{events:?}");
    let failure = &events[0];
    assert_eq!(failure.stage, "pre_start_failure");
    assert_eq!(failure.outcome, "error");
    assert_eq!(failure.worker_id.as_deref(), Some("worker-1"));
    assert_eq!(failure.cube_repo_id.as_deref(), Some("mono"));
    assert_eq!(
        failure.error_message.as_deref(),
        Some("bookmark creation failed: underlying bookmark error")
    );
    assert_eq!(failure.details["reason"], "execution_bookmark_create_failed");
    assert_eq!(failure.details["will_retry"], false);
    assert!(
        failure.details["bookkeeping_error"]
            .as_str()
            .unwrap()
            .contains("bookkeeping unavailable")
    );
    assert!(failure.details.get("pre_start_failure_count").is_none());
    let after = db.get_execution(&execution.id).unwrap();
    assert_eq!(after.pre_start_failure_count, 0);
    assert_eq!(after.status, ExecutionStatus::Ready);
    let timeline = crate::dispatch_reader::TimelineState::from_events(&events).unwrap();
    assert!(!timeline.is_live());
    let thresholds = crate::dispatch_reader::StageThresholds::new(Duration::from_secs(1));
    assert!(
        timeline
            .stall_to_emit(&execution.id, u128::MAX / 2, &thresholds)
            .is_none()
    );
}

#[tokio::test]
async fn sweep_reports_real_unpushed_revisions_independent_of_original_workspace_and_ignores_empty_runs() {
    for original_state in ["released", "removed", "reused", "leased_elsewhere"] {
        for has_work in [false, true] {
            let dir = tempdir().unwrap();
            let path = dir.path().join("boss.db");
            let db = Arc::new(WorkDb::open(path.clone()).unwrap());
            let product = create_test_product(&db);
            let chore = create_test_chore_manual(&db, product.id, "Recover revision");
            let prior = db
                .create_execution(
                    CreateExecutionInput::builder()
                        .work_item_id(chore.id)
                        .kind(ExecutionKind::RevisionImplementation)
                        .status(ExecutionStatus::Ready)
                        .build(),
                )
                .unwrap();
            db.start_execution_run(
                &prior.id,
                "worker",
                "mono",
                "released-lease",
                "old-workspace",
                "/workspace/removed",
            )
            .unwrap();
            let repo = JjRepo::new(dir.path());
            let record = create(&LocalJj, &repo.worker, &prior.id, "local").await.unwrap();
            db.record_execution_bookmark(&record).unwrap();
            if has_work {
                std::fs::write(repo.worker.join("revision.txt"), "unpushed revision").unwrap();
                JjRepo::run(&repo.worker, &["describe", "-m", "Unpushed revision"]);
            }
            db.mark_execution_orphaned(&prior.id, "worker terminated").unwrap();
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute(
                    "UPDATE work_executions SET finished_at = CAST(unixepoch('now') - 1000 AS TEXT) WHERE id = ?1",
                    [&prior.id],
                )
                .unwrap();
            match original_state {
                "removed" => std::fs::remove_dir_all(&repo.worker).unwrap(),
                "reused" | "leased_elsewhere" => {
                    JjRepo::run(&repo.worker, &["new", "root()", "-m", "Unrelated lease"]);
                    std::fs::write(repo.worker.join("foreign.txt"), "foreign edits").unwrap();
                }
                _ => {}
            }
            let cube = Arc::new(FakeCubeClient::default());
            let coordinator = ExecutionCoordinator::new(
                db.clone(),
                WorkerPool::new(1),
                cube.clone(),
                Arc::new(FakeExecutionRunner::default()),
            );
            let reported = crate::abandoned_execution_bookmarks::run_one_pass(&db, &coordinator)
                .await
                .unwrap();
            assert_eq!(reported, usize::from(has_work));
            let attentions = db.list_attention_items(&prior.id).unwrap();
            assert_eq!(attentions.len(), usize::from(has_work));
            if has_work {
                assert_eq!(
                    attentions[0].kind,
                    crate::abandoned_execution_bookmarks::RECOVERABLE_WORK
                );
                assert!(attentions[0].body_markdown.contains(&record.head()));
                crate::abandoned_execution_bookmarks::run_one_pass(&db, &coordinator)
                    .await
                    .unwrap();
                assert_eq!(
                    db.list_attention_items(&prior.id).unwrap().len(),
                    1,
                    "repeated probes must not duplicate attention"
                );
            }
            assert!(cube.status_calls.lock().await.is_empty());
            assert!(cube.lease_calls.lock().await.is_empty());
            if matches!(original_state, "reused" | "leased_elsewhere") {
                assert_eq!(
                    std::fs::read_to_string(repo.worker.join("foreign.txt")).unwrap(),
                    "foreign edits"
                );
            }
        }
    }
}

/// The abandoned-bookmark sweep is the loudness path that still fails a
/// missing pointer: dispatch and resume must not, but the sweep still
/// raises `execution_bookmark_recovery_failed` attention.
#[tokio::test]
async fn missing_pointer_is_reported_instead_of_skipped_by_the_sweep() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path.clone()).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "Missing pointer");
    let prior = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(&prior.id, "worker", "mono", "released", "removed", "/workspace/removed")
        .unwrap();
    db.mark_execution_orphaned(&prior.id, "terminated").unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE work_executions SET finished_at = CAST(unixepoch('now') - 1000 AS TEXT) WHERE id = ?1",
            [&prior.id],
        )
        .unwrap();
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        Arc::new(FakeCubeClient::default()),
        Arc::new(FakeExecutionRunner::default()),
    );
    let failure = crate::abandoned_execution_bookmarks::run_one_pass(&db, &coordinator)
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("no engine-created recovery bookmark"));
    let attentions = db.list_attention_items(&prior.id).unwrap();
    assert_eq!(attentions.len(), 1);
    assert_eq!(attentions[0].kind, crate::execution_bookmark_recovery::RECOVERY_FAILED);
    assert!(
        attentions[0]
            .body_markdown
            .contains("no engine-created recovery bookmark")
    );
}

/// The engine's prune sweep and its candidate query.
mod prune_sweep {
    use super::super::helpers::*;
    use crate::execution_bookmark_prune::{BATCH_SIZE, GH_LOOKUPS_PER_PASS, UNPROVEN, run_one_pass};
    use crate::work::{PrOpenState, PrStateChecker, prune_outcome};
    use boss_engine_recovery::execution_bookmark::{ExecutionBookmark, LocalJj, create};
    use boss_engine_test_git::jj::JjRepo;
    use std::sync::atomic::AtomicUsize;

    const PR_URL: &str = "https://github.com/spinyfin/mono/pull/7";
    const LONG_AGO: i64 = 24 * 60 * 60;

    struct Fixture {
        _dir: tempfile::TempDir,
        db: Arc<WorkDb>,
        repo: JjRepo,
        coordinator: ExecutionCoordinator,
        product_id: String,
    }

    fn fixture() -> Fixture {
        let dir = tempdir().unwrap();
        let path = dir.path().join("boss.db");
        let db = Arc::new(WorkDb::open(path.clone()).unwrap());
        let product = create_test_product(&db);
        let repo = JjRepo::new(dir.path());
        let coordinator = ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            Arc::new(FakeCubeClient::default()),
            Arc::new(FakeExecutionRunner::default()),
        );
        Fixture {
            _dir: dir,
            db,
            repo,
            coordinator,
            product_id: product.id,
        }
    }

    impl Fixture {
        fn sql(&self, sql: &str, params: impl rusqlite::Params) {
            rusqlite::Connection::open(self._dir.path().join("boss.db"))
                .unwrap()
                .execute(sql, params)
                .unwrap();
        }

        /// A finished local execution (orphaned `finished_secs_ago` seconds ago)
        /// with a live recovery record whose pointers sit on the worker's `@`.
        /// Returns the chore id, the execution id and the record.
        async fn execution(&self, name: &str, finished_secs_ago: i64) -> (String, String, ExecutionBookmark) {
            let chore = create_test_chore_manual(&self.db, self.product_id.clone(), name);
            let execution = self
                .db
                .create_execution(
                    CreateExecutionInput::builder()
                        .work_item_id(chore.id.clone())
                        .kind(boss_protocol::ExecutionKind::RevisionImplementation)
                        .status(ExecutionStatus::Ready)
                        .build(),
                )
                .unwrap();
            self.db
                .start_execution_run(&execution.id, "worker", "mono", "lease", "workspace", "/workspace")
                .unwrap();
            let record = create(&LocalJj, &self.repo.worker, &execution.id, "local")
                .await
                .unwrap();
            self.db.record_execution_bookmark(&record).unwrap();
            self.db
                .mark_execution_orphaned(&execution.id, "worker terminated")
                .unwrap();
            self.sql(
                "UPDATE work_executions SET finished_at = CAST(unixepoch('now') - ?2 AS TEXT) WHERE id = ?1",
                rusqlite::params![execution.id, finished_secs_ago],
            );
            (chore.id, execution.id, record)
        }

        /// Give every pointer a real unpushed change and return its commit.
        fn write_unpushed_work(&self, record: &ExecutionBookmark) -> String {
            std::fs::write(self.repo.worker.join("revision.txt"), "unpushed revision").unwrap();
            JjRepo::run(&self.repo.worker, &["describe", "-m", "Unpushed revision"]);
            self.commit_of(&record.head())
        }

        fn commit_of(&self, bookmark: &str) -> String {
            JjRepo::run(
                &self.repo.worker,
                &[
                    "--ignore-working-copy",
                    "log",
                    "--no-graph",
                    "-r",
                    &format!("bookmarks(exact:\"{bookmark}\")"),
                    "-T",
                    "commit_id",
                ],
            )
            .trim()
            .to_owned()
        }

        fn bookmark_exists(&self, name: &str) -> bool {
            !JjRepo::run(
                &self.repo.worker,
                &[
                    "--ignore-working-copy",
                    "bookmark",
                    "list",
                    "-T",
                    "name ++ \"\\n\"",
                    name,
                ],
            )
            .trim()
            .is_empty()
        }

        fn close_chain(&self, chore_id: &str, pr_url: Option<&str>, observed_head: Option<&str>) {
            self.sql(
                "UPDATE tasks SET status = 'done', pr_url = ?2, pr_head_sha = ?3 WHERE id = ?1",
                rusqlite::params![chore_id, pr_url, observed_head],
            );
        }

        fn outcome(&self, execution_id: &str) -> Option<String> {
            self.db.execution_bookmark_prune_outcome(execution_id).unwrap()
        }

        fn retired(&self, execution_id: &str) -> bool {
            self.db
                .execution_bookmark_retired_reason(execution_id)
                .unwrap()
                .is_some()
        }

        fn unproven_items(&self, execution_id: &str) -> usize {
            self.db
                .list_attention_items(execution_id)
                .unwrap()
                .iter()
                .filter(|item| item.kind == UNPROVEN)
                .count()
        }

        fn uncheck(&self, execution_id: &str) {
            self.sql(
                "UPDATE execution_bookmarks SET prune_checked_at = NULL WHERE execution_id = ?1",
                [execution_id],
            );
        }

        async fn pass(&self, pr_state: Arc<dyn PrStateChecker>) -> crate::execution_bookmark_prune::PruneReport {
            run_one_pass(&self.db, &self.coordinator, pr_state).await.unwrap()
        }
    }

    /// Counts `inspect` calls and answers with a preset head or an error.
    struct CountingChecker {
        head_sha: Option<String>,
        fail: bool,
        calls: AtomicUsize,
    }

    impl CountingChecker {
        fn new(head_sha: Option<&str>, fail: bool) -> Arc<Self> {
            Arc::new(Self {
                head_sha: head_sha.map(str::to_owned),
                fail,
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl PrStateChecker for CountingChecker {
        fn check(&self, _pr_url: &str) -> Result<PrOpenState> {
            Ok(PrOpenState::Merged)
        }

        fn inspect(&self, pr_url: &str) -> Result<crate::work::PrInspect> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                anyhow::bail!("gh unavailable for {pr_url}");
            }
            Ok(crate::work::PrInspect {
                open_state: PrOpenState::Merged,
                head_sha: self.head_sha.clone(),
            })
        }
    }

    fn no_github() -> Arc<CountingChecker> {
        CountingChecker::new(None, false)
    }

    // ── list_prune_candidates ───────────────────────────────────────────────

    #[tokio::test]
    async fn candidates_are_terminal_local_live_records_past_the_grace_window() {
        let f = fixture();
        let grace = 15 * 60;
        let (_, old, _) = f.execution("Old", LONG_AGO).await;
        let (_, fresh, _) = f.execution("Fresh", 1).await;
        let (_, remote, _) = f.execution("Remote", LONG_AGO).await;
        f.sql(
            "UPDATE execution_bookmarks SET host_id = 'zakalwe' WHERE execution_id = ?1",
            [&remote],
        );
        let (_, retired, _) = f.execution("Retired", LONG_AGO).await;
        f.db.retire_execution_bookmark(&retired, "test").unwrap();
        let (_, running, _) = f.execution("Running", LONG_AGO).await;
        f.sql(
            "UPDATE work_executions SET status = 'running' WHERE id = ?1",
            [&running],
        );
        let ids: Vec<String> =
            f.db.list_prune_candidates(grace, 3600, 3600, 100)
                .unwrap()
                .into_iter()
                .map(|c| c.execution.id)
                .collect();
        assert_eq!(
            ids,
            vec![old.clone()],
            "fresh/remote/retired/running rows must be skipped: {fresh} {remote}"
        );
    }

    #[tokio::test]
    async fn recheck_intervals_depend_on_the_last_outcome_and_unchecked_rows_come_first() {
        let f = fixture();
        let (_, checked, _) = f.execution("Checked", 3 * LONG_AGO).await;
        let (_, never, _) = f.execution("Never checked", LONG_AGO).await;
        let stamp = |id: &str, outcome: &str, age: i64| {
            f.sql(
            "UPDATE execution_bookmarks SET prune_outcome = ?2, prune_checked_at = unixepoch('now') - ?3 WHERE execution_id = ?1",
            rusqlite::params![id, outcome, age],
        );
        };
        let listed = |open: i64, unproven: i64| -> Vec<String> {
            f.db.list_prune_candidates(0, open, unproven, 100)
                .unwrap()
                .into_iter()
                .map(|c| c.execution.id)
                .collect()
        };
        // Open-chain rows wait the open interval; unproven and errored rows the
        // longer unproven one.
        stamp(&checked, prune_outcome::PR_OPEN, 100);
        assert_eq!(listed(1000, 10), vec![never.clone()]);
        assert_eq!(listed(50, 10_000), vec![never.clone(), checked.clone()]);
        stamp(&checked, prune_outcome::UNPROVEN, 100);
        assert_eq!(listed(10_000, 1000), vec![never.clone()]);
        assert_eq!(listed(10_000, 50), vec![never.clone(), checked.clone()]);
        stamp(&checked, prune_outcome::ERROR, 100);
        assert_eq!(listed(10_000, 1000), vec![never.clone()]);
        // Never-checked rows lead even though the checked one finished earlier,
        // and the batch limit cuts the checked row off first.
        let limited = f.db.list_prune_candidates(0, 10_000, 50, 1).unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].execution.id, never);
    }

    #[tokio::test]
    async fn candidates_carry_the_chain_state_and_deduplicated_pr_heads() {
        let f = fixture();
        let (chore, execution, _) = f.execution("Chain", LONG_AGO).await;
        let (open_chore, open_execution, _) = f.execution("Open chain", LONG_AGO).await;
        let head_a = "a".repeat(40);
        let head_b = "b".repeat(40);
        f.db.set_execution_pr_head_before(&execution, &head_a).unwrap();
        f.sql(
            "UPDATE work_executions SET pr_head_after = ?2 WHERE id = ?1",
            rusqlite::params![execution, head_a],
        );
        // A later execution on the same chain that records the next head.
        let second =
            f.db.create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.clone())
                    .kind(boss_protocol::ExecutionKind::RevisionImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap()
            .id;
        f.sql(
            "UPDATE work_executions SET pr_head_after = ?2 WHERE id = ?1",
            rusqlite::params![second, head_b],
        );
        f.close_chain(&chore, Some(PR_URL), Some(&head_b));
        let candidates = f.db.list_prune_candidates(0, 0, 0, 100).unwrap();
        let closed = candidates.iter().find(|c| c.execution.id == execution).unwrap();
        assert!(closed.chain_closed);
        assert_eq!(closed.pr_url.as_deref(), Some(PR_URL));
        assert_eq!(closed.observed_pr_head.as_deref(), Some(head_b.as_str()));
        assert_eq!(
            closed.recorded_pr_heads,
            vec![head_a, head_b],
            "the same head recorded before and after is listed once, oldest first"
        );
        let open = candidates.iter().find(|c| c.execution.id == open_execution).unwrap();
        assert!(
            !open.chain_closed,
            "a chain whose root is not done/archived stays open: {open_chore}"
        );
        assert_eq!(open.pr_url, None);
    }

    // ── the sweep pass ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_open_chain_is_stamped_and_left_alone() {
        let f = fixture();
        let (_, id, record) = f.execution("Open", LONG_AGO).await;
        f.write_unpushed_work(&record);
        let report = f.pass(no_github()).await;
        assert_eq!((report.examined, report.pr_open), (1, 1));
        assert_eq!(f.outcome(&id).as_deref(), Some(prune_outcome::PR_OPEN));
        assert!(!f.retired(&id));
        assert!(f.bookmark_exists(&record.head()) && f.bookmark_exists(&record.publication()));
        // Stamped just now: the next pass does not look again.
        assert_eq!(f.pass(no_github()).await.examined, 0);
    }

    #[tokio::test]
    async fn a_finished_execution_inside_the_grace_window_is_not_examined() {
        let f = fixture();
        let (chore, id, _) = f.execution("Just finished", 1).await;
        f.close_chain(&chore, None, None);
        assert_eq!(f.pass(no_github()).await.examined, 0);
        assert_eq!(f.outcome(&id), None);
    }

    #[tokio::test]
    async fn work_carried_by_the_observed_pr_head_is_published_and_retired() {
        let f = fixture();
        let (chore, id, record) = f.execution("Published", LONG_AGO).await;
        let work = f.write_unpushed_work(&record);
        f.close_chain(&chore, Some(PR_URL), Some(&work));
        let github = no_github();
        let report = f.pass(github.clone()).await;
        assert_eq!((report.examined, report.published, report.unproven), (1, 1, 0));
        assert_eq!(github.calls(), 0, "a recorded head needs no GitHub lookup");
        assert_eq!(f.outcome(&id).as_deref(), Some(prune_outcome::PUBLISHED));
        assert!(f.retired(&id));
        for name in [record.base(), record.head(), record.publication()] {
            assert!(!f.bookmark_exists(&name), "{name} survived");
        }
    }

    #[tokio::test]
    async fn pointers_that_are_already_gone_retire_the_record() {
        let f = fixture();
        let (chore, id, record) = f.execution("Gone", LONG_AGO).await;
        f.close_chain(&chore, Some(PR_URL), None);
        JjRepo::run(
            &f.repo.worker,
            &["bookmark", "forget", &record.head(), &record.publication()],
        );
        let report = f.pass(no_github()).await;
        assert_eq!((report.examined, report.already_gone), (1, 1));
        assert_eq!(f.outcome(&id).as_deref(), Some(prune_outcome::ALREADY_GONE));
        assert!(f.retired(&id));
        assert!(!f.bookmark_exists(&record.base()));
    }

    #[tokio::test]
    async fn unproven_work_keeps_its_pointers_and_raises_one_attention_item() {
        let f = fixture();
        let (chore, id, record) = f.execution("Unproven", LONG_AGO).await;
        f.write_unpushed_work(&record);
        f.close_chain(&chore, None, None);
        let report = f.pass(no_github()).await;
        assert_eq!((report.examined, report.unproven, report.gh_lookups), (1, 1, 0));
        assert_eq!(f.outcome(&id).as_deref(), Some(prune_outcome::UNPROVEN));
        assert!(!f.retired(&id));
        assert!(f.bookmark_exists(&record.head()) && f.bookmark_exists(&record.publication()));
        assert!(
            !f.bookmark_exists(&record.base()),
            "the baseline is rebuilt from the record"
        );
        assert_eq!(f.unproven_items(&id), 1);
        let body =
            f.db.list_attention_items(&id)
                .unwrap()
                .into_iter()
                .find(|item| item.kind == UNPROVEN)
                .unwrap()
                .body_markdown;
        assert!(
            body.contains("so the pointers were kept"),
            "two kept heads read plural: {body}"
        );
        assert!(
            body.contains("The baseline `boss-base/") && body.contains("rebuilt from the record"),
            "the baseline is named apart: {body}"
        );
        assert!(
            !body.contains("carried by a voucher and forgotten"),
            "no voucher carried the baseline: {body}"
        );
        // The daily re-check finds the same thing and does not stack a second item.
        f.sql(
            "UPDATE execution_bookmarks SET prune_checked_at = prune_checked_at - ?2 WHERE execution_id = ?1",
            rusqlite::params![id, 2 * LONG_AGO],
        );
        assert_eq!(f.pass(no_github()).await.unproven, 1);
        assert_eq!(f.unproven_items(&id), 1, "the open item is re-raised, not duplicated");
    }

    #[tokio::test]
    async fn github_head_lookup_publishes_work_no_recorded_head_carries() {
        let f = fixture();
        let (chore, id, record) = f.execution("GitHub voucher", LONG_AGO).await;
        let work = f.write_unpushed_work(&record);
        f.close_chain(&chore, Some(PR_URL), None);
        let github = CountingChecker::new(Some(&work), false);
        let report = f.pass(github.clone()).await;
        assert_eq!((report.published, report.gh_lookups), (1, 1));
        assert_eq!(github.calls(), 1);
        assert_eq!(f.outcome(&id).as_deref(), Some(prune_outcome::PUBLISHED));
        assert!(f.retired(&id));
        assert!(!f.bookmark_exists(&record.head()));
    }

    #[tokio::test]
    async fn a_publication_head_ahead_of_recovery_survives_until_github_vouches_for_it() {
        let f = fixture();
        let (chore, id, record) = f.execution("Ahead", LONG_AGO).await;
        let work = f.write_unpushed_work(&record);
        // The worker pushed one more commit: only the publication head has it.
        JjRepo::run(&f.repo.worker, &["new", "-m", "Pushed follow-up"]);
        std::fs::write(f.repo.worker.join("follow-up.txt"), "follow-up").unwrap();
        JjRepo::run(&f.repo.worker, &["status"]);
        let ahead = JjRepo::run(
            &f.repo.worker,
            &[
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                "@",
                "-T",
                "commit_id",
            ],
        )
        .trim()
        .to_owned();
        JjRepo::run(
            &f.repo.worker,
            &[
                "bookmark",
                "set",
                &record.publication(),
                "-r",
                &ahead,
                "--allow-backwards",
            ],
        );
        // The merge poller saw only the older head, and GitHub cannot be asked.
        f.close_chain(&chore, Some(PR_URL), Some(&work));
        let report = f.pass(CountingChecker::new(None, true)).await;
        assert_eq!((report.unproven, report.gh_lookups, report.errors), (1, 1, 0));
        assert!(
            f.bookmark_exists(&record.publication()),
            "the head with unpublished work must stay"
        );
        assert_eq!(f.commit_of(&record.publication()), ahead);
        assert!(!f.retired(&id));
        // The retry, with GitHub reporting the head that carries the follow-up.
        f.uncheck(&id);
        let report = f.pass(CountingChecker::new(Some(&ahead), false)).await;
        assert_eq!((report.published, report.gh_lookups), (1, 1));
        assert!(f.retired(&id));
        assert!(!f.bookmark_exists(&record.publication()));
    }

    #[tokio::test]
    async fn failed_github_lookups_leave_the_pointers_in_place() {
        let f = fixture();
        let (chore, id, record) = f.execution("Lookup fails", LONG_AGO).await;
        f.write_unpushed_work(&record);
        f.close_chain(&chore, Some(PR_URL), None);
        let report = f.pass(CountingChecker::new(None, true)).await;
        assert_eq!((report.unproven, report.gh_lookups, report.errors), (1, 1, 0));
        assert_eq!(f.outcome(&id).as_deref(), Some(prune_outcome::UNPROVEN));
        assert!(f.bookmark_exists(&record.head()) && f.bookmark_exists(&record.publication()));
    }

    #[tokio::test]
    async fn github_lookups_are_budgeted_per_pass() {
        let f = fixture();
        let total = GH_LOOKUPS_PER_PASS + 2;
        assert!(total <= BATCH_SIZE);
        let mut ids = Vec::new();
        let mut last_record = None;
        for n in 0..total {
            let (chore, id, record) = f.execution(&format!("Budget {n}"), LONG_AGO).await;
            f.close_chain(&chore, Some(PR_URL), None);
            ids.push(id);
            last_record = Some(record);
        }
        f.write_unpushed_work(&last_record.unwrap());
        let github = CountingChecker::new(None, false);
        let report = f.pass(github.clone()).await;
        assert_eq!(report.examined, total);
        assert_eq!(report.unproven, total);
        assert_eq!(report.gh_lookups, GH_LOOKUPS_PER_PASS);
        assert_eq!(github.calls(), GH_LOOKUPS_PER_PASS);
    }

    #[tokio::test]
    async fn a_record_whose_store_cannot_be_read_is_stamped_error_without_stopping_the_pass() {
        let f = fixture();
        let (broken_chore, broken, broken_record) = f.execution("Broken store", 3 * LONG_AGO).await;
        f.close_chain(&broken_chore, Some(PR_URL), None);
        f.sql(
            "UPDATE execution_bookmarks SET repo_path = '/nonexistent/boss-prune-test' WHERE execution_id = ?1",
            [&broken],
        );
        let (chore, healthy, record) = f.execution("Healthy", LONG_AGO).await;
        let work = f.write_unpushed_work(&record);
        f.close_chain(&chore, Some(PR_URL), Some(&work));
        let report = f.pass(no_github()).await;
        assert_eq!((report.examined, report.errors, report.published), (2, 1, 1));
        assert_eq!(f.outcome(&broken).as_deref(), Some(prune_outcome::ERROR));
        assert!(!f.retired(&broken));
        assert_eq!(f.outcome(&healthy).as_deref(), Some(prune_outcome::PUBLISHED));
        assert!(
            f.bookmark_exists(&broken_record.head()),
            "an unreadable record's pointers are untouched"
        );
    }
}
