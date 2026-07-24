//! Lease, repo-ensure, and pane-spawn failure handling: attention items,
//! dispatch-event stages, retry/backoff, and the requeue-vs-give-up branches.
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;

/// Operators previously saw lease failures show up as a vague
/// "no slot available" because the engine swallowed the cube
/// stderr. The dispatcher now logs the full anyhow chain at
/// `tracing::error!` *before* `record_start_failure` writes its
/// own warn line, so the verbatim cube stderr lands in the
/// engine log. Stale-working-copy recovery is owned by cube
/// (cube PR #254); this test only pins the loud-logging
/// contract.
#[tokio::test]
async fn lease_failure_logs_cube_stderr_at_error_before_recording_failure() {
    let buffer = log_capture::install();
    let starting_offset = buffer.lock().len();

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient {
        fail_lease: true,
        ..FakeCubeClient::default()
    });
    // No retries: go straight to permanent failure so the test does
    // not have to wait through exponential backoff delays.
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![]),
    );
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

    // Slice out only the bytes written after the test started so
    // we don't trip over events emitted by other parallel tests
    // sharing the same global subscriber.
    let captured = String::from_utf8_lossy(&buffer.lock()[starting_offset..]).to_string();
    let our_lines: Vec<&str> = captured.lines().filter(|line| line.contains(&execution_id)).collect();
    assert!(
        !our_lines.is_empty(),
        "expected captured log lines for execution {execution_id}, got nothing.\n\
             Full slice was:\n{captured}"
    );

    let error_idx = our_lines
        .iter()
        .position(|line| line.contains("ERROR") && line.contains("cube workspace lease attempt failed"))
        .unwrap_or_else(|| {
            panic!(
                "expected a tracing::error! log for the cube lease failure;\n\
                     captured lines for this execution were:\n{:#?}",
                our_lines
            )
        });
    let error_line = our_lines[error_idx];
    // The fake's lease error message *is* the simulated cube
    // stderr; the engine must surface it verbatim rather than
    // truncating or pattern-matching.
    assert!(
        error_line.contains("cube workspace lease failed"),
        "error log line must include the cube stderr verbatim, got:\n{error_line}"
    );

    let warn_idx = our_lines
        .iter()
        .position(|line| line.contains("WARN") && line.contains("recorded execution start failure"))
        .unwrap_or_else(|| {
            panic!(
                "expected a tracing::warn! log from record_start_failure;\n\
                     captured lines for this execution were:\n{:#?}",
                our_lines
            )
        });

    assert!(
        error_idx < warn_idx,
        "error log must precede record_start_failure's warn log; \
             got error at {error_idx}, warn at {warn_idx}.\n\
             Captured lines:\n{:#?}",
        our_lines
    );
}

/// Shared per-process tracing capture used by tests that need
/// to assert on log output. We can't install a per-test
/// subscriber because cargo runs library tests in parallel
/// threads of the same process and `set_global_default`
/// rejects a second installer. Tests that opt in slice the
/// shared buffer by execution_id (which is unique per test) to
/// isolate their own events.
mod log_capture {
    use std::io;
    use std::sync::{Arc, Mutex, OnceLock};

    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    pub(super) struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
            self.0.lock().expect("shared log buffer poisoned")
        }
    }

    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("shared log buffer poisoned")
                .extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct SharedMakeWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> MakeWriter<'a> for SharedMakeWriter {
        type Writer = SharedWriter;

        fn make_writer(&'a self) -> Self::Writer {
            SharedWriter(self.0.clone())
        }
    }

    pub(super) fn install() -> SharedBuffer {
        static BUFFER: OnceLock<SharedBuffer> = OnceLock::new();
        BUFFER
            .get_or_init(|| {
                let buffer = SharedBuffer(Arc::new(Mutex::new(Vec::new())));
                let subscriber = tracing_subscriber::fmt()
                    .with_writer(SharedMakeWriter(buffer.0.clone()))
                    .with_ansi(false)
                    .with_target(false)
                    .with_max_level(tracing::Level::TRACE)
                    .finish();
                // Tolerate the "already set" race: another test
                // binary or a stray init in the same process
                // shouldn't sink the suite. The capture only
                // works if our subscriber wins, but if it
                // doesn't, the assertions below will fail
                // loudly with a clear "no captured lines"
                // message.
                let _ = tracing::subscriber::set_global_default(subscriber);
                buffer
            })
            .clone()
    }
}

/// Regression for the silent-release dispatch failure: when the
/// pane-spawn step inside `run_execution` fails — libghostty IPC
/// drop, prompt composition error, runner panic, all surface
/// here as `Err(_)` from `ExecutionRunner::run_execution` — the
/// coordinator MUST raise a `WorkAttentionItem` AND emit a
/// structured `pane_spawned` error event. Before this fix
/// landed, the run flipped to `failed` and the lease was
/// released, but nothing surfaced to `bossctl agents list` or
/// the kanban view; operators had nothing to chase. The
/// `RecordingDispatchEventSink` below asserts the stage timeline
/// reaches `pane_spawned: error`; the `list_attention_items`
/// assertion proves the WorkAttentionItem made it to disk.
#[tokio::test]
async fn pane_spawn_failure_raises_attention_item_and_dispatch_event() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        fail: true,
        ..FakeExecutionRunner::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_dispatch_events(recording.clone()),
    );
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

    // The execution went all the way through the lease + change
    // creation. `rescan_active_dispatch_after_release` will
    // re-queue the chore (pre-existing retry behavior, since
    // `start_execution_run` flipped tasks.status to `active`
    // before the spawn failed), so cube fakes may be invoked
    // multiple times — pin only "at least once each".
    assert!(!cube.lease_calls.lock().await.is_empty());
    assert!(!cube.create_calls.lock().await.is_empty());
    // The lease is released after the pane-spawn failure — before
    // the fix, this release was the *only* observable signal that
    // anything went wrong.
    assert!(cube.release_calls.lock().await.iter().any(|id| id == "lease-1"));

    // Loud signal #1: the WorkAttentionItem is what surfaces in
    // the kanban "Attention" lane and through `ListAttentionItems`.
    // The exact count varies — once the run finishes_execution_run
    // with `failed`, `rescan_active_dispatch_after_release` will
    // see the chore is still in `active` status (auto-advanced
    // when `start_execution_run` committed) and re-queue another
    // ready execution, which fails again. That retry behavior is
    // pre-existing; this test only pins the loud-failure contract:
    // every failed pane spawn raises exactly one attention item.
    let attention_items = db.list_attention_items(&execution_id).unwrap();
    assert!(
        !attention_items.is_empty(),
        "pane-spawn failure must raise at least one attention item; got nothing",
    );
    let first = &attention_items[0];
    assert_eq!(first.kind, "pane_spawn_failed");
    assert!(
        first.body_markdown.contains("worker pane never came up"),
        "attention body should describe the failure mode; got {:?}",
        first.body_markdown,
    );
    assert!(
        first.body_markdown.contains("worker prompt failed"),
        "attention body should include the original error; got {:?}",
        first.body_markdown,
    );

    // Loud signal #2: a structured `pane_spawned: error` event in
    // the dispatch stream, so external tooling can flag it
    // without scanning tracing logs.
    let events = recording.events_for(&execution_id).await;
    let pane_event = events
        .iter()
        .find(|event| event.stage == "pane_spawned" && event.outcome == "error")
        .unwrap_or_else(|| panic!("expected a pane_spawned:error event for {execution_id}; got {events:#?}"));
    assert!(
        pane_event
            .error_message
            .as_deref()
            .is_some_and(|msg| msg.contains("worker prompt failed")),
        "pane_spawned event must include the underlying error; got {:?}",
        pane_event.error_message,
    );
    // The stage timeline before the failure should also be
    // visible — request_recorded, worker_claimed, cube stages,
    // run_started — so an operator can confirm dispatch did get
    // through every earlier handoff. `cube_workspace_lease_attempted`
    // sits between `cube_repo_ensured` and `cube_workspace_leased`
    // and pins what the engine asked cube to do (preferred
    // workspace, fallback policy) for diagnose visibility.
    let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
    for expected in [
        "request_recorded",
        "worker_claimed",
        "cube_repo_ensured",
        "cube_workspace_lease_attempted",
        "cube_workspace_leased",
        "cube_change_created",
        "run_started",
        "pane_spawned",
    ] {
        assert!(
            stages.contains(&expected),
            "stage `{expected}` missing from dispatch timeline; got {stages:?}",
        );
    }
}

/// T267 regression (outcome 3): a slow `SpawnWorkerPane` ack that
/// nonetheless spawned the pane must NOT be treated as a spawn
/// failure. The real `PaneSpawnRunner` now converts the ack timeout
/// into a PROVISIONAL spawn (waiting_human + slot retained); the fake
/// returns that same outcome. The coordinator must then:
///   - keep the execution TRACKED in `waiting_human` (non-terminal),
///   - NOT release the cube workspace lease (a live pane may occupy it),
///   - NOT mark the run failed or emit a `pane_spawned: error` event,
///   - NOT leave a duplicate execution behind (the incident's second
///     worker came from the failed+demoted work item being re-dispatched).
///
/// The Timeout→provisional conversion itself is unit-tested in
/// `spawn_flow`; this pins the coordinator-side contract.
#[tokio::test]
async fn ack_timeout_provisional_spawn_is_tracked_not_failed_or_duplicated() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        ack_timed_out: true,
        ..FakeExecutionRunner::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_dispatch_events(recording.clone()),
    );
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::WaitingHuman).await;

    // Tracked, not failed — the pane may be live and doing work.
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::WaitingHuman);
    // The lease is retained on the tracked row (a provisional pane may
    // be occupying the workspace) — clearing it would let the workspace
    // be re-leased out from under a live worker.
    assert_eq!(
        execution.cube_lease_id.as_deref(),
        Some("lease-1"),
        "the tracked provisional execution must keep its cube lease",
    );

    // No release-while-occupied: the coordinator must not hand the
    // workspace back to cube for a provisional spawn.
    let releases = cube.release_calls.lock().await.clone();
    assert!(
        !releases.iter().any(|id| id == "lease-1"),
        "cube lease must NOT be released for a provisional (ack-timeout) spawn; releases: {releases:?}",
    );

    // No duplicate dispatch: exactly one execution exists for the
    // chore. In the incident, the failed+demoted work item spawned a
    // second worker; keeping the run tracked (is_live) makes the
    // orphan-active sweep skip it, so no duplicate is created.
    let executions = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(
        executions.len(),
        1,
        "a provisional spawn must not leave a duplicate execution behind; got {executions:#?}",
    );

    // The dispatch stream records a normal `pane_spawned: ok`, never a
    // `pane_spawned: error` — this was not a failure.
    let events = recording.events_for(&execution_id).await;
    assert!(
        events.iter().any(|e| e.stage == "pane_spawned" && e.outcome == "ok"),
        "expected a pane_spawned:ok event for the provisional spawn; got {events:#?}",
    );
    assert!(
        !events.iter().any(|e| e.stage == "pane_spawned" && e.outcome == "error"),
        "a provisional spawn must NOT emit a pane_spawned:error event; got {events:#?}",
    );
}

/// When a pane-spawn fails for an `automation_triage` execution, the
/// matching `automation_runs` row must be flipped from the scheduler's
/// pessimistic `failed_will_retry` to `failed_gave_up`. Without this,
/// a non-self-healing failure (e.g. invalid worker_id format) leaves
/// the Automations tab showing a pending retry that will never happen.
#[tokio::test]
async fn pane_spawn_failure_finalises_automation_run_to_failed_gave_up() {
    use crate::work::{AutomationFireRecord, CreateAutomationInput};
    use boss_protocol::{AUTOMATION_OUTCOME_FAILED_GAVE_UP, AutomationTrigger};

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let automation = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Nightly check".to_owned(),
            repo_remote_url: None,
            trigger: AutomationTrigger::Schedule {
                cron: "0 2 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            },
            standing_instruction: "audit the repo".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    // Create the triage execution that the scheduler would normally create.
    let triage_exec = db
        .create_automation_triage_execution(&automation.id, "git@github.com:spinyfin/mono.git")
        .unwrap();

    // Record the automation run at the pessimistic `failed_will_retry`
    // that the scheduler stamps when it dispatches (schedule advanced).
    let scheduled_for: i64 = 1_000_000;
    db.record_automation_run_and_advance(
        AutomationFireRecord::builder()
            .automation_id(automation.id.clone())
            .scheduled_for(scheduled_for)
            .started_at(scheduled_for)
            .outcome(boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
            .triage_execution_id(triage_exec.id.clone())
            .build(),
    )
    .unwrap();

    // Confirm the run is `failed_will_retry` before we touch the coordinator.
    let run_before = db
        .automation_run_for_triage_execution(&triage_exec.id)
        .unwrap()
        .expect("automation run must exist");
    assert_eq!(
        run_before.outcome, "failed_will_retry",
        "precondition: scheduler stamps failed_will_retry on dispatch"
    );

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        fail: true,
        ..FakeExecutionRunner::default()
    });
    let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    // Wire in a 1-slot automation pool so the triage execution gets
    // dispatched (it targets the automation pool, not the main pool).
    coord.set_automation_pool(WorkerPool::new_automation(1));
    let coordinator = Arc::new(coord);
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &triage_exec.id, ExecutionStatus::Failed).await;

    // The automation run must now show `failed_gave_up`, not `failed_will_retry`.
    let run_after = db
        .automation_run_for_triage_execution(&triage_exec.id)
        .unwrap()
        .expect("automation run must still exist");
    assert_eq!(
        run_after.outcome, AUTOMATION_OUTCOME_FAILED_GAVE_UP,
        "pane-spawn failure must finalize automation run to failed_gave_up; \
             got {:?} — the Automations tab would show a phantom pending retry",
        run_after.outcome,
    );
}

/// A `pr_review` spawn failure must NOT demote the work item back to
/// `todo`. The PrReview exception in the pane-spawn failure handler
/// skips `demote_active_work_item_to_todo` so the kanban card stays
/// in its current state (here: `active`, as it would be just after an
/// implementation run that produced a PR). The symmetrical chore path
/// (`pane_spawn_failure_raises_attention_item_and_dispatch_event`) DOES
/// demote — this test pins the carve-out in the opposite direction.
#[tokio::test]
async fn pane_spawn_failure_for_pr_review_does_not_demote_work_item() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    // Create the chore with autostart=false so `rescan_active_dispatch`
    // never re-queues it after the PrReview execution fails. Only the
    // PrReview execution we inject below reaches the dispatcher.
    let chore = create_test_chore_manual(&db, product.id.clone(), "Reviewed chore");

    // Simulate the post-implementation state: the chore is `active`
    // (auto-advanced by `start_execution_run` when the implementation
    // run began) and a PrReview execution was just enqueued by the
    // completion handler. `autostart = 0` is already set, so the
    // rescan sweep skips this chore even after the review pool frees up.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'active', updated_at = '1' WHERE id = ?1",
            rusqlite::params![chore.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_executions
                   (id, work_item_id, kind, status, repo_remote_url, priority, created_at)
                 VALUES (?1, ?2, ?3, 'ready', ?4, 0, '1')",
            rusqlite::params![
                "exec-pr-review-1",
                chore.id,
                EXECUTION_KIND_PR_REVIEW,
                "git@github.com:spinyfin/mono.git"
            ],
        )
        .unwrap();
    }

    let cube = Arc::new(FakeCubeClient::default());
    // fail=true simulates the pane-spawn failure path (libghostty IPC
    // error, prompt composition failure, etc.) for the pr_review
    // execution. The coordinator must NOT call demote_active_work_item_to_todo.
    let runner = Arc::new(FakeExecutionRunner {
        fail: true,
        ..FakeExecutionRunner::default()
    });
    // The coordinator already has a review pool (DEFAULT_REVIEW_POOL_SIZE
    // slots) by default — no extra setup needed; the PrReview execution
    // routes there automatically.
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), "exec-pr-review-1", ExecutionStatus::Failed).await;

    let item = db.get_work_item(&chore.id).unwrap();
    let status = match item {
        WorkItem::Chore(t) | WorkItem::Task(t) => t.status,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_ne!(
        status,
        TaskStatus::Todo,
        "pr_review spawn failure must not demote the work item to `todo`; \
             got `{status}` — the skip-demote guard for pr_review is absent or broken",
    );
}

/// The automation-dispatch consistency fix (T410 field incident,
/// 2026-07-15): a `SlotBusy` app rejection means the engine and the
/// app disagree about a slot's occupancy — it is an engine-side
/// infrastructure issue, not a genuine dispatch failure of the task
/// itself. On `SlotBusy` the coordinator must NOT demote the work
/// item back to `todo` (which would require a human to notice and
/// manually re-drag the card) and must NOT hand the offending slot
/// straight back to the free pool (which would just let the very
/// next dispatch pass re-select it and repeat the rejection). Instead
/// the item stays `active`, a fresh `ready` execution is queued via
/// the ordinary `rescan_active_dispatch_after_release` path, and the
/// bad slot is left claimed (for `pool_claim_sweep` to reclaim once
/// its grace period passes) — exactly the "stays in Doing, dispatches
/// on the next free slot" behavior a plain pool-exhaustion wait gets.
#[tokio::test]
async fn slot_busy_pane_spawn_failure_requeues_without_demoting_and_holds_slot() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();
    let first_execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        slot_busy: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &first_execution_id, ExecutionStatus::Failed).await;

    // Give the requeue's own kick a moment to land — the fresh `ready`
    // execution is created synchronously inside `run_execution`'s tail,
    // but dispatch of it happens on a follow-up scheduler pass.
    for _ in 0..50 {
        if db
            .list_executions(Some(&chore.id))
            .unwrap()
            .iter()
            .any(|e| e.id != first_execution_id)
        {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }

    let item = db.get_work_item(&chore.id).unwrap();
    let status = match item {
        WorkItem::Chore(t) | WorkItem::Task(t) => t.status,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_ne!(
        status,
        TaskStatus::Todo,
        "slot-busy spawn failure must not demote the work item to `todo`; got `{status}`",
    );

    let executions = db.list_executions(Some(&chore.id)).unwrap();
    assert!(
        executions.iter().any(|e| e.id != first_execution_id),
        "slot-busy spawn failure must queue a fresh execution for redispatch; got {executions:#?}",
    );

    assert_eq!(
        coordinator.worker_pool().idle_count().await,
        0,
        "the slot-busy slot must be held (not freed) so the next dispatch pass doesn't \
             immediately re-select and repeat the rejection — pool_claim_sweep reclaims it later",
    );
}

/// Sibling of the above for the `automation_triage` execution kind,
/// whose synthetic "work item" is the automation itself (no `tasks`
/// row, so `rescan_active_dispatch` cannot requeue it). On `SlotBusy`
/// the coordinator must fire a fresh triage execution immediately —
/// mirroring `EngineTriageDispatcher::fire` — and re-point the
/// occurrence's `automation_runs` row at it with the scheduler's own
/// pessimistic `failed_will_retry` outcome, rather than giving up with
/// the terminal `failed_gave_up` a genuine (non-desync) spawn failure
/// gets (see `pane_spawn_failure_finalises_automation_run_to_failed_gave_up`).
#[tokio::test]
async fn slot_busy_pane_spawn_failure_requeues_automation_triage_instead_of_giving_up() {
    use crate::work::{AutomationFireRecord, CreateAutomationInput};
    use boss_protocol::AutomationTrigger;

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let automation = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Nightly check".to_owned(),
            repo_remote_url: None,
            trigger: AutomationTrigger::Schedule {
                cron: "0 2 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            },
            standing_instruction: "audit the repo".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let triage_exec = db
        .create_automation_triage_execution(&automation.id, "git@github.com:spinyfin/mono.git")
        .unwrap();

    let scheduled_for: i64 = 1_000_000;
    db.record_automation_run_and_advance(
        AutomationFireRecord::builder()
            .automation_id(automation.id.clone())
            .scheduled_for(scheduled_for)
            .started_at(scheduled_for)
            .outcome(boss_protocol::AUTOMATION_OUTCOME_FAILED_WILL_RETRY)
            .triage_execution_id(triage_exec.id.clone())
            .build(),
    )
    .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        slot_busy: true,
        ..FakeExecutionRunner::default()
    });
    let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    coord.set_automation_pool(WorkerPool::new_automation(1));
    let coordinator = Arc::new(coord);
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &triage_exec.id, ExecutionStatus::Failed).await;

    // The automation run must now point at a NEW triage execution,
    // still `failed_will_retry` (not `failed_gave_up`) — a fresh
    // attempt was queued rather than the occurrence being abandoned.
    // The re-point happens in `run_execution`'s tail, after the DB
    // commit `wait_for_execution_status` observes, so poll briefly
    // for it to land instead of racing a single read.
    let mut run_after = db.list_automation_runs(&automation.id).unwrap().into_iter().next();
    for _ in 0..50 {
        if run_after
            .as_ref()
            .is_some_and(|r| r.triage_execution_id.as_deref() != Some(triage_exec.id.as_str()))
        {
            break;
        }
        sleep(Duration::from_millis(10)).await;
        run_after = db.list_automation_runs(&automation.id).unwrap().into_iter().next();
    }
    let run_after = run_after.expect("automation run must still exist");
    assert_ne!(
        run_after.triage_execution_id.as_deref(),
        Some(triage_exec.id.as_str()),
        "slot-busy failure must re-point the run at a fresh triage execution, not leave it on the failed one",
    );
    // With a 1-slot automation pool held busy by the just-failed
    // execution, the fresh retry execution this requeue created has
    // nowhere to dispatch yet — the drain correctly reports pool
    // exhaustion and marks it `pool_throttled` (the SAME "queued,
    // will dispatch when a slot frees" state a genuine full pool
    // gets). Either that or the scheduler's own initial
    // `failed_will_retry` stamp is acceptable here; what must NEVER
    // happen is the terminal `failed_gave_up` a real (non-desync)
    // spawn failure gets.
    assert_ne!(
        run_after.outcome,
        boss_protocol::AUTOMATION_OUTCOME_FAILED_GAVE_UP,
        "slot-busy failure must requeue, not give up; got {:?}",
        run_after.outcome,
    );
    assert!(
        matches!(run_after.outcome.as_str(), "failed_will_retry" | "pool_throttled"),
        "slot-busy failure must leave the run in a retryable (non-terminal) state; got {:?}",
        run_after.outcome,
    );

    // The new triage execution must actually exist and be dispatchable
    // (not itself already terminal).
    let new_execution_id = run_after
        .triage_execution_id
        .clone()
        .expect("requeued run must carry a triage_execution_id");
    assert_ne!(new_execution_id, triage_exec.id);
    let new_execution = db.get_execution(&new_execution_id).unwrap();
    assert_ne!(
        new_execution.status,
        ExecutionStatus::Failed,
        "the requeued triage execution must not itself be pre-failed",
    );
}

/// Regression for the automation-pool dispatch stall (2026-06-03):
/// an `automation_triage` execution must drive PAST `worker_claimed`
/// — through host selection and the cube repo-ensure handoff — to the
/// `cube_workspace_lease_attempted` stage, exactly like every other
/// pool. The original symptom was the execution sitting silently at
/// `worker_claimed` with no further dispatch event until the stall
/// watchdog reaped it ~30s later. This test pins that the
/// previously-silent gap now emits `host_selected:ok` and
/// `cube_repo_ensure_attempted`, and that the lease stage is reached.
#[tokio::test]
async fn automation_triage_execution_advances_past_worker_claimed_to_lease() {
    use crate::work::CreateAutomationInput;
    use boss_protocol::AutomationTrigger;

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let automation = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Nightly check".to_owned(),
            repo_remote_url: None,
            trigger: AutomationTrigger::Schedule {
                cron: "0 2 * * *".to_owned(),
                timezone: "UTC".to_owned(),
            },
            standing_instruction: "audit the repo".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();
    let triage_exec = db
        .create_automation_triage_execution(&automation.id, "git@github.com:spinyfin/mono.git")
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
        .with_dispatch_events(recording.clone());
    // Wire a 1-slot automation pool so the triage execution (which
    // targets the automation pool, not the main pool) is dispatched.
    coord.set_automation_pool(WorkerPool::new_automation(1));
    let coordinator = Arc::new(coord);
    coordinator.kick();

    // Poll the dispatch stream directly rather than a specific
    // execution status: the contract under test is "advances to the
    // lease stage", independent of the final run state.
    let mut reached_lease = false;
    for _ in 0..200 {
        let events = recording.events_for(&triage_exec.id).await;
        if events.iter().any(|e| e.stage == "cube_workspace_lease_attempted") {
            reached_lease = true;
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    let events = recording.events_for(&triage_exec.id).await;
    let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
    assert!(
        reached_lease,
        "automation execution never reached `cube_workspace_lease_attempted` \
             (stalled at worker_claimed?); timeline was {stages:?}",
    );

    // The previously-silent claimed -> repo-ensure handoff now emits
    // explicit milestones.
    for expected in [
        "worker_claimed",
        "host_selected",
        "cube_repo_ensure_attempted",
        "cube_workspace_lease_attempted",
    ] {
        assert!(
            stages.contains(&expected),
            "automation execution must advance through `{expected}`; got {stages:?}",
        );
    }

    // Host selection resolved successfully — it did not fail out.
    let host_selected = events
        .iter()
        .find(|e| e.stage == "host_selected")
        .expect("host_selected event present");
    assert_eq!(
        host_selected.outcome, "ok",
        "automation host selection must succeed; got {host_selected:?}",
    );

    // The watchdog signature we are fixing must be absent.
    assert!(
        !stages.contains(&"stage_stalled"),
        "automation execution must not stall; got {stages:?}",
    );
}

/// Regression for the regular-pool dispatch stall (T1849): a
/// `revision_implementation` execution (main pool) must drive PAST
/// `worker_claimed` — through host selection and the cube repo-ensure
/// handoff — to `cube_workspace_lease_attempted`, exactly like the
/// automation pool. The original symptom was the three early-exit guards
/// in `schedule_execution` (redundant-spawn, chain-serializer,
/// gating-prereqs) returning `Err` without emitting any dispatch event,
/// so the timeline sat at `worker_claimed/ok` until the stall watchdog
/// fired ~30s later and the orphan sweep abandoned the execution.
#[tokio::test]
async fn revision_implementation_execution_advances_past_worker_claimed_to_lease() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    // autostart=false so the reconcile sweep never enqueues a second
    // execution in parallel — only the one we inject reaches the dispatcher.
    let chore = create_test_chore_manual(&db, product.id.clone(), "Impl chore");
    let impl_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_dispatch_events(recording.clone()),
    );
    coordinator.kick();

    // Poll the dispatch stream directly — the contract is "advances to
    // the lease stage", independent of the final run state.
    let mut reached_lease = false;
    for _ in 0..200 {
        let events = recording.events_for(&impl_exec.id).await;
        if events.iter().any(|e| e.stage == "cube_workspace_lease_attempted") {
            reached_lease = true;
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    let events = recording.events_for(&impl_exec.id).await;
    let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
    assert!(
        reached_lease,
        "revision_implementation execution never reached `cube_workspace_lease_attempted` \
             (stalled at worker_claimed?); timeline was {stages:?}",
    );

    // The previously-silent gap must now emit explicit milestones.
    for expected in [
        "worker_claimed",
        "host_selected",
        "cube_repo_ensure_attempted",
        "cube_workspace_lease_attempted",
    ] {
        assert!(
            stages.contains(&expected),
            "revision_implementation execution must advance through `{expected}`; got {stages:?}",
        );
    }

    // Host selection resolved successfully — it must not have failed out.
    let host_selected = events
        .iter()
        .find(|e| e.stage == "host_selected")
        .expect("host_selected event present");
    assert_eq!(
        host_selected.outcome, "ok",
        "revision_implementation host selection must succeed; got {host_selected:?}",
    );

    // The stall-watchdog signature we are fixing must be absent.
    assert!(
        !stages.contains(&"stage_stalled"),
        "revision_implementation execution must not stall; got {stages:?}",
    );
}

/// The `pane_spawned: ok` event must carry the resolved spawn
/// knobs (effort level, claude effort value, model) so
/// `bossctl dispatch diagnose <exec-id>` can answer "what did
/// this worker actually launch with" — design §Q2 ("surfaces the
/// chosen model, effort value, and level on the dispatch
/// instrumentation stream"). The fake runner reports a synthetic
/// `SpawnConfig`; this test pins that the coordinator forwards
/// it into the event's `details.spawn_config` field.
#[tokio::test]
async fn pane_spawned_event_carries_spawn_config_details() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Trivial chore")
                .effort_level(crate::work::EffortLevel::Trivial)
                .build(),
        )
        .unwrap();
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        slot_id: Some(1),
        spawn_config: Some(crate::effort::SpawnConfig {
            effort_level: Some(crate::work::EffortLevel::Trivial),
            claude_effort: Some("low"),
            model: "sonnet".to_owned(),
            driver: crate::effort::ENGINE_DEFAULT_DRIVER.to_owned(),
            prompt_addendum: None,
        }),
        ..FakeExecutionRunner::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube, runner).with_dispatch_events(recording.clone()),
    );
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::WaitingHuman).await;

    let events = recording.events_for(&execution_id).await;
    let pane_event = events
        .iter()
        .find(|event| event.stage == "pane_spawned" && event.outcome == "ok")
        .unwrap_or_else(|| panic!("expected pane_spawned:ok event for {execution_id}; got {events:#?}"));
    let spawn = pane_event.details.get("spawn_config").unwrap_or_else(|| {
        panic!(
            "pane_spawned event missing spawn_config in details: {:?}",
            pane_event.details
        )
    });
    assert_eq!(spawn["effort_level"], "trivial");
    assert_eq!(spawn["claude_effort"], "low");
    assert_eq!(spawn["model"], "sonnet");
    assert_eq!(spawn["prompt_addendum_applied"], false);
}

/// Cube lease failures also need the loud-failure contract: a
/// `WorkAttentionItem` AND a structured event. This pins both —
/// the older `lease_failure_logs_cube_stderr_at_error_before_recording_failure`
/// test only asserts the tracing log shape.
#[tokio::test]
async fn cube_lease_failure_raises_attention_item_and_dispatch_event() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient {
        fail_lease: true,
        ..FakeCubeClient::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    // No retries: go straight to permanent failure so the test does
    // not have to wait through exponential backoff delays.
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![])
        .with_dispatch_events(recording.clone()),
    );
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

    let attention_items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        attention_items.len(),
        1,
        "cube lease failure must raise exactly one attention item",
    );
    assert_eq!(attention_items[0].kind, "cube_workspace_lease_failed");
    assert!(attention_items[0].body_markdown.contains("cube workspace lease failed"));

    let events = recording.events_for(&execution_id).await;
    // The lease attempt event is emitted before the call, so the
    // timeline pins what the engine *intended* to do even when
    // cube refuses.
    let attempt_event = events
        .iter()
        .find(|event| event.stage == "cube_workspace_lease_attempted")
        .expect("cube_workspace_lease_attempted event missing");
    assert_eq!(attempt_event.outcome, "ok");
    assert_eq!(
        attempt_event.details.get("attempt").and_then(|v| v.as_u64()),
        Some(1),
        "first attempt event should carry attempt=1; got {:?}",
        attempt_event.details,
    );

    let lease_failed = events
        .iter()
        .find(|event| event.stage == "cube_workspace_lease_failed")
        .expect("cube_workspace_lease_failed event missing");
    assert_eq!(lease_failed.outcome, "error");
    assert!(
        lease_failed
            .error_message
            .as_deref()
            .is_some_and(|m| m.contains("cube workspace lease failed")),
        "lease_failed event must carry the verbatim cube error; got {:?}",
        lease_failed.error_message,
    );
    assert_eq!(
        lease_failed.details.get("reason").and_then(|v| v.as_str()),
        Some("cube_error"),
        "lease_failed event must classify reason; got {:?}",
        lease_failed.details,
    );

    // The success event must NOT be emitted, and the timeline
    // must NOT include later stages — dispatch bailed at the
    // lease step.
    let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
    assert!(
        !stages.contains(&"cube_workspace_leased"),
        "cube_workspace_leased (success) must not appear when lease fails; got {stages:?}",
    );
    assert!(!stages.contains(&"cube_change_created"));
    assert!(!stages.contains(&"run_started"));
    assert!(!stages.contains(&"pane_spawned"));
}

/// The anaplian failure-mode A produced an opaque `reason: "cube_error"`
/// even though the engine held the real cause (cube granted the lease,
/// then a setup step exited non-zero). Pin that a typed `CubeCliError`
/// now propagates its exit code + stderr into the `cube_workspace_lease_failed`
/// event `details` so the failure is attributable in one read.
#[tokio::test]
async fn cube_lease_failure_surfaces_exit_code_and_stderr_in_details() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient {
        fail_lease_with_cube_cli_error: true,
        ..FakeCubeClient::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![])
        .with_dispatch_events(recording.clone()),
    );
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

    let events = recording.events_for(&execution_id).await;
    let lease_failed = events
        .iter()
        .find(|event| event.stage == "cube_workspace_lease_failed")
        .expect("cube_workspace_lease_failed event missing");
    // The structured exit code is now attributable without parsing.
    assert_eq!(
        lease_failed.details.get("cube_exit_code").and_then(|v| v.as_i64()),
        Some(1),
        "lease_failed must carry the cube exit code; got {:?}",
        lease_failed.details,
    );
    // The real cause (the setup-step stderr) rides the event, not just
    // the flattened error_message.
    assert!(
        lease_failed
            .details
            .get("cube_stderr")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains("copy-config-secrets")),
        "lease_failed must carry the cube stderr; got {:?}",
        lease_failed.details,
    );
    assert_eq!(
        lease_failed.details.get("cube_host").and_then(|v| v.as_str()),
        Some("anaplian"),
    );
    // The verbatim message is still preserved for humans.
    assert!(
        lease_failed
            .error_message
            .as_deref()
            .is_some_and(|m| m.contains("copy-config-secrets")),
    );
}

/// `cube repo ensure` failures used to be recorded as
/// `cube_repo_ensured` with `outcome=error` — a success-shaped stage
/// name with an error attached, which is exactly how the anaplian
/// incident's `command not found: cube` failure hid in plain sight in
/// `dispatch.jsonl` for 12 consecutive attempts. Pin that the failure
/// now emits its own terminal `cube_repo_ensure_failed:error` stage,
/// and that the success-shaped `cube_repo_ensured` stage never
/// appears at all when the ensure call fails.
#[tokio::test]
async fn cube_repo_ensure_failure_emits_dedicated_failed_stage() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    let chore = create_test_chore(&db, product.id.clone(), "Ensure Failure");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient {
        fail_ensure: true,
        ..FakeCubeClient::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    // No retries: go straight to permanent failure so the test does
    // not have to wait through backoff delays.
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![])
        .with_dispatch_events(recording.clone()),
    );
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

    let events = recording.events_for(&execution_id).await;
    let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();

    let failed_event = events
        .iter()
        .find(|event| event.stage == "cube_repo_ensure_failed")
        .unwrap_or_else(|| panic!("cube_repo_ensure_failed event missing; got {stages:?}"));
    assert_eq!(failed_event.outcome, "error");
    assert!(
        failed_event
            .error_message
            .as_deref()
            .is_some_and(|m| m.contains("cube repo ensure failed")),
        "failed event must carry the verbatim cube error; got {:?}",
        failed_event.error_message,
    );

    // The success-shaped stage must never appear for a failed attempt
    // — that ambiguity is exactly the bug this split fixes.
    assert!(
        !stages.contains(&"cube_repo_ensured"),
        "cube_repo_ensured (success-shaped) must not appear when ensure fails; got {stages:?}",
    );
    assert!(!stages.contains(&"cube_workspace_lease_attempted"));
    assert!(!stages.contains(&"run_started"));
    assert!(!stages.contains(&"pane_spawned"));
}

/// The "failing to start" vs. "waiting for a slot" ambiguity this
/// bounce closes: a chore whose lease keeps failing (e.g. the
/// `jj bookmark set pr/<n> … refusing to move backwards` incident)
/// must not be left silently looping — the loop is over, and the
/// operator must be able to see it's broken and why straight from
/// the kanban card (`dispatch_failed_reason` / `dispatch_failed_error`),
/// not just from a `WorkAttentionItem` a separate list call surfaces.
#[tokio::test]
async fn cube_lease_failure_bounces_work_item_to_backlog_with_error() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();
    assert!(
        match db.get_work_item(&chore.id).unwrap() {
            WorkItem::Chore(t) | WorkItem::Task(t) => t.autostart,
            other => panic!("expected chore, got {other:?}"),
        },
        "autostart must start true — otherwise the bounce assertion below is vacuous",
    );

    let cube = Arc::new(FakeCubeClient {
        fail_lease: true,
        ..FakeCubeClient::default()
    });
    // No retries: go straight to permanent failure.
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![]),
    );
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

    let task = match db.get_work_item(&chore.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(
        task.status.as_str(),
        "todo",
        "a chore that fails to start must bounce back to Backlog, not strand in Doing",
    );
    assert!(
        !task.autostart,
        "autostart must be cleared so the card renders as parked in Backlog, \
             not as a phantom \"waiting for a slot\" card",
    );
    assert_eq!(
        task.dispatch_failed_reason.as_deref(),
        Some("cube_workspace_lease_failed"),
        "the failure reason must be stamped on the task for the kanban card to render",
    );
    assert_eq!(
        task.dispatch_failed_error.as_deref(),
        Some("cube workspace lease failed"),
        "the underlying cube error must be stamped on the task, not just buried in an attention item",
    );
    assert!(task.dispatch_failed_at.is_some());

    // A deliberate retry (mirroring a kanban drag or `bossctl work
    // start`) must clear the stale error — the card shouldn't keep
    // showing last time's failure once a fresh attempt is under way.
    db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();
    let retried_task = match db.get_work_item(&chore.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(retried_task.dispatch_failed_reason, None);
    assert_eq!(retried_task.dispatch_failed_error, None);
    assert_eq!(retried_task.dispatch_failed_at, None);
}

/// Pre-start failures (cube lease error, cube ensure error, etc.) should
/// be retried automatically before surfacing to the operator.
///
/// This test uses zero-length backoff delays and a single retry slot so
/// it runs quickly. It verifies:
/// 1. A single pre-start failure resets the execution to `ready` (not
///    `failed`) and `pre_start_failure_count` is incremented.
/// 2. A second failure (after retry) permanently marks the execution
///    `failed` and surfaces an attention item.
/// 3. Only one execution row exists (no sibling rows).
#[tokio::test]
async fn pre_start_failure_retries_then_permanently_fails() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Retry Chore");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient {
        fail_lease: true,
        ..FakeCubeClient::default()
    });
    // One retry (two attempts total), immediate backoff.
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![Duration::ZERO]),
    );
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

    coordinator.kick();
    // Wait for permanent failure — after 1 retry (2 total attempts)
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert_eq!(
        execution.pre_start_failure_count, 2,
        "expected 2 pre-start failures (initial + 1 retry); got {}",
        execution.pre_start_failure_count
    );

    let runs = db.list_runs(&execution_id).unwrap();
    assert_eq!(
        runs.len(),
        2,
        "expected 2 run rows (one per attempt); got {}",
        runs.len()
    );
    assert!(runs.iter().all(|r| r.status == "failed"));

    // Exactly one execution row — retries reuse the same row.
    let all_executions = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(
        all_executions.len(),
        1,
        "retries must not create sibling execution rows; got {}",
        all_executions.len()
    );

    // Permanent failure surfaces exactly one attention item.
    let attention_items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        attention_items.len(),
        1,
        "permanent pre-start failure must raise exactly one attention item"
    );
    assert_eq!(attention_items[0].kind, "cube_workspace_lease_failed");
}

/// Pre-start retry: when the FIRST attempt fails but a second succeeds,
/// the execution reaches `running` and only one execution row is created.
#[tokio::test]
async fn pre_start_failure_retries_and_succeeds_on_second_attempt() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Retry Then Succeed");
    db.reconcile_product_executions(&product.id).unwrap();

    // `lease_workspace_with_fallback` makes two `lease_workspace`
    // calls per dispatch attempt (primary + `any_free` fallback).
    // Fail both calls in the first attempt so the retry path
    // actually triggers; calls 3+ succeed.
    let cube = Arc::new(FakeCubeClient {
        fail_first_n_leases: 2,
        ..FakeCubeClient::default()
    });
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            // pending=true keeps the execution in `running` so we can
            // assert on it without racing against the WaitingHuman
            // transition.
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        )
        .with_pre_start_retry_delays(vec![Duration::ZERO]),
    );
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

    coordinator.kick();
    // On the retry the lease succeeds → execution reaches `running`.
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Running).await;

    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Running);
    assert_eq!(
        execution.pre_start_failure_count, 1,
        "expected exactly 1 pre-start failure before the successful attempt; got {}",
        execution.pre_start_failure_count
    );

    // Only the one failed run row (from the initial attempt) + the active run.
    let runs = db.list_runs(&execution_id).unwrap();
    assert_eq!(
        runs.len(),
        2,
        "expected 1 failed run + 1 active run; got {}",
        runs.len()
    );

    // No attention items — the retry succeeded.
    let attention_items = db.list_attention_items(&execution_id).unwrap();
    assert!(
        attention_items.is_empty(),
        "successful retry must not surface an attention item"
    );

    // Exactly one execution row.
    let all_executions = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(all_executions.len(), 1);
}

/// When `preferred_workspace_id` is set and cube refuses that workspace,
/// the engine must NOT fall back to any other workspace — doing so would
/// silently lose state continuity (the resuming worker needs that specific
/// workspace). The dispatch must fail so the scheduler can retry with
/// the correct workspace later.
#[tokio::test]
async fn lease_with_prefer_set_does_not_fall_back_when_refused() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .preferred_workspace_id("mono-agent-003")
            .build(),
    )
    .unwrap();

    let cube = Arc::new(FakeCubeClient {
        fail_lease_when_prefer_set: true,
        ..FakeCubeClient::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    // No retries: go straight to permanent failure to avoid backoff
    // delays and to keep the lease-call assertion at exactly 1.
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            cube.clone(),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![])
        .with_dispatch_events(recording.clone()),
    );
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Failed).await;

    // Exactly one cube lease invocation: the engine must not retry
    // with a different workspace when a preferred workspace is set.
    let calls = cube.lease_calls.lock().await;
    assert_eq!(
        calls.len(),
        1,
        "engine must not retry when prefer is set; got {:?}",
        calls
    );
    assert_eq!(calls[0].2.as_deref(), Some("mono-agent-003"));
    drop(calls);

    let events = recording.events_for(&execution_id).await;
    let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();

    let attempt_events: Vec<&crate::dispatch_events::DispatchEvent> = events
        .iter()
        .filter(|e| e.stage == "cube_workspace_lease_attempted")
        .collect();
    assert_eq!(
        attempt_events.len(),
        1,
        "expected exactly one lease_attempted event; got stages {stages:?}"
    );
    assert_eq!(
        attempt_events[0]
            .details
            .get("prefer_workspace_id")
            .and_then(|v| v.as_str()),
        Some("mono-agent-003"),
    );
    assert_eq!(
        attempt_events[0]
            .details
            .get("fallback_policy")
            .and_then(|v| v.as_str()),
        Some("none"),
        "policy must be none when prefer is set — no silent workspace swap",
    );

    // Execution must fail, not succeed on a different workspace.
    assert!(
        !stages.contains(&"cube_workspace_leased"),
        "cube_workspace_leased must not appear; engine must not land on a different workspace; got {stages:?}",
    );

    let attention_items = db.list_attention_items(&execution_id).unwrap();
    assert_eq!(
        attention_items.len(),
        1,
        "terminal lease failure must raise exactly one attention item",
    );
    assert_eq!(attention_items[0].kind, "cube_workspace_lease_failed");
}

// ── reconcile_workspace_recovery: cube first, patch second ────────────
