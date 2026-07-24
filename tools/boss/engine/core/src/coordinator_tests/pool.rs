//! Worker-pool mechanics and scheduler basics: slot claiming/affinity,
//! worker-id round-trips, priority ordering, capacity, pool exhaustion and
//! recovery, force-dispatch, and heartbeat/kick behavior.
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;

#[tokio::test]
async fn worker_pool_clamps_size_to_hard_cap() {
    let pool = WorkerPool::new(MAX_WORKER_POOL_SIZE + 4);
    assert_eq!(pool.capacity().await, MAX_WORKER_POOL_SIZE);
}

#[tokio::test]
async fn worker_pool_prefers_workspace_affinity_over_lowest_index() {
    let pool = WorkerPool::new(2);

    // Deterministic selection fills the lowest free slot first, so the
    // two claims land on worker-1 then worker-2.
    let w_a = pool.claim_worker("exec-a", None).await.unwrap();
    let w_b = pool.claim_worker("exec-b", None).await.unwrap();
    assert_eq!(w_a, "worker-1");
    assert_eq!(w_b, "worker-2");
    pool.release_worker(&w_a, Some("ws-a")).await;
    pool.release_worker(&w_b, Some("ws-b")).await;

    // Preferring ws-b must pick the worker that recorded ws-b affinity
    // (worker-2), even though the lowest-index default would otherwise
    // pick worker-1.
    let claimed = pool.claim_worker("exec-c", Some("ws-b")).await.unwrap();
    assert_eq!(claimed, w_b);
    pool.release_worker(&claimed, Some("ws-b")).await;

    // Preferring an unknown workspace has no affinity match, so it falls
    // through to the deterministic lowest-index slot (worker-1).
    let fallback = pool.claim_worker("exec-d", Some("ws-unknown")).await.unwrap();
    assert_eq!(fallback, w_a);
}

/// `worker-{N}` and slot N must round-trip 1:1. The
/// engine-owns-slots refactor depends on this — the runner
/// derives the pane slot it sends to the app from the worker
/// id the coordinator handed it. A regression in either format
/// or parse would silently re-introduce two independent
/// numbering systems.
#[test]
fn worker_id_and_slot_id_round_trip() {
    // Covers the full interactive pool — Bridge Crew (1..=8) and Lower
    // Decks (9..=16) — so the second page round-trips 1:1 too.
    for slot in 1u8..=MAX_WORKER_POOL_SIZE as u8 {
        let worker_id = WorkerPool::worker_id_for_slot(slot);
        assert_eq!(worker_id, format!("worker-{slot}"));
        assert_eq!(slot_id_from_worker_id(&worker_id), Some(slot));
    }
}

#[test]
fn slot_id_from_worker_id_accepts_automation_pool_format() {
    // Automation-pool ordinals are offset by MAX_WORKER_POOL_SIZE (16) so the
    // two pools occupy disjoint slot ranges: interactive 1..=16, automation 17..=24.
    for ordinal in 1u8..=MAX_AUTOMATION_POOL_SIZE as u8 {
        let auto_worker_id = format!("auto-worker-{ordinal}");
        let expected_slot = ordinal + MAX_WORKER_POOL_SIZE as u8;
        assert_eq!(
            slot_id_from_worker_id(&auto_worker_id),
            Some(expected_slot),
            "expected Some({expected_slot}) for {auto_worker_id:?}"
        );
    }
    assert_eq!(slot_id_from_worker_id("auto-worker-0"), None);
    assert_eq!(slot_id_from_worker_id("auto-worker-"), None);
    assert_eq!(slot_id_from_worker_id("auto-worker-abc"), None);
}

#[test]
fn worker_id_for_slot_round_trips_with_slot_id_from_worker_id() {
    // Interactive pool: slots 1..=16 → "worker-N" → back to the same slot.
    for slot in 1u8..=MAX_WORKER_POOL_SIZE as u8 {
        let wid = worker_id_for_slot(slot);
        assert_eq!(wid, format!("worker-{slot}"));
        assert_eq!(slot_id_from_worker_id(&wid), Some(slot));
    }
    // Automation pool: slots 17..=24 → "auto-worker-M" → back to the same slot.
    let automation_end = MAX_WORKER_POOL_SIZE as u8 + MAX_AUTOMATION_POOL_SIZE as u8;
    for slot in (MAX_WORKER_POOL_SIZE as u8 + 1)..=automation_end {
        let wid = worker_id_for_slot(slot);
        let expected_ordinal = slot as usize - MAX_WORKER_POOL_SIZE;
        assert_eq!(wid, format!("auto-worker-{expected_ordinal}"));
        assert_eq!(slot_id_from_worker_id(&wid), Some(slot));
    }
    // Review pool: slots 25..=32 → "review-M" → back to the same slot.
    for slot in (automation_end + 1)..=(automation_end + MAX_REVIEW_POOL_SIZE as u8) {
        let wid = worker_id_for_slot(slot);
        let expected_ordinal = slot as usize - MAX_WORKER_POOL_SIZE - MAX_AUTOMATION_POOL_SIZE;
        assert_eq!(wid, format!("review-{expected_ordinal}"));
        assert_eq!(slot_id_from_worker_id(&wid), Some(slot));
    }
}

#[test]
fn slot_id_from_worker_id_accepts_review_pool_format() {
    // Review-pool ordinals are offset past both the interactive (16) and
    // automation (8) ranges, so they occupy slots 25..=32 — disjoint
    // from every other pool.
    for ordinal in 1u8..=MAX_REVIEW_POOL_SIZE as u8 {
        let review_worker_id = format!("review-{ordinal}");
        let expected_slot = ordinal + MAX_WORKER_POOL_SIZE as u8 + MAX_AUTOMATION_POOL_SIZE as u8;
        assert_eq!(
            slot_id_from_worker_id(&review_worker_id),
            Some(expected_slot),
            "expected Some({expected_slot}) for {review_worker_id:?}"
        );
    }
    assert_eq!(slot_id_from_worker_id("review-0"), None);
    assert_eq!(slot_id_from_worker_id("review-"), None);
    assert_eq!(slot_id_from_worker_id("review-abc"), None);
}

#[test]
fn review_pool_slots_are_disjoint_from_other_pools() {
    // The slot IDs produced by review-N (25..=32) must not overlap
    // with any interactive-pool (1..=16) or automation-pool (17..=24) slot.
    let automation_ceiling = MAX_WORKER_POOL_SIZE + MAX_AUTOMATION_POOL_SIZE;
    for ordinal in 1u8..=MAX_REVIEW_POOL_SIZE as u8 {
        let review_wid = format!("review-{ordinal}");
        let slot = slot_id_from_worker_id(&review_wid).unwrap();
        assert!(
            slot as usize > automation_ceiling,
            "review-{ordinal} must map to slot > {automation_ceiling}, got {slot}"
        );
        // Verify the reverse also works: the slot maps back to a review- id.
        let back = worker_id_for_slot(slot);
        assert!(
            back.starts_with(REVIEW_WORKER_ID_PREFIX),
            "slot {slot} must produce a review-pool worker_id, got {back:?}"
        );
    }
}

#[test]
fn worker_page_label_partitions_interactive_pool_only() {
    // Bridge Crew is page 0 (slots 1..=8), Lower Decks is page 1
    // (slots 9..=16). Non-interactive slots (automation/review/remote)
    // have no page label.
    for slot in 1u8..=WORKER_PAGE_SIZE as u8 {
        assert_eq!(worker_page_label(slot).as_deref(), Some("Bridge Crew"), "slot {slot}");
    }
    for slot in (WORKER_PAGE_SIZE as u8 + 1)..=MAX_WORKER_POOL_SIZE as u8 {
        assert_eq!(worker_page_label(slot).as_deref(), Some("Lower Decks"), "slot {slot}");
    }
    assert_eq!(worker_page_label(0), None);
    assert_eq!(
        worker_page_label(MAX_WORKER_POOL_SIZE as u8 + 1),
        None,
        "first automation slot has no page"
    );
    assert_eq!(
        worker_page_label(crate::worker_registry::REMOTE_SLOT_BASE),
        None,
        "remote virtual slot has no page"
    );
}

#[test]
fn automation_pool_slots_are_disjoint_from_regular_pool() {
    // The slot IDs produced by auto-worker-N (17..=24) must not
    // overlap with any interactive-pool slot (1..=16).
    for ordinal in 1u8..=MAX_AUTOMATION_POOL_SIZE as u8 {
        let auto_wid = format!("auto-worker-{ordinal}");
        let slot = slot_id_from_worker_id(&auto_wid).unwrap();
        assert!(
            slot as usize > MAX_WORKER_POOL_SIZE,
            "auto-worker-{ordinal} must map to slot > {MAX_WORKER_POOL_SIZE}, got {slot}"
        );
        // Verify the reverse also works: the slot maps back to an auto-worker- id.
        let back = worker_id_for_slot(slot);
        assert!(
            back.starts_with(AUTOMATION_WORKER_ID_PREFIX),
            "slot {slot} must produce an automation-pool worker_id, got {back:?}"
        );
    }
}

#[test]
fn slot_busy_occupant_walks_the_with_context_wrapped_chain() {
    // The spawn flow always wraps `StartWorkerError` with
    // `.with_context(...)` before it reaches the coordinator (see
    // `runner.rs`'s `spawning worker pane for run {}` wrapper), so
    // a naive `err.downcast_ref::<StartWorkerError>()` on the
    // outermost error would never match. This pins the chain-walk
    // that makes extraction work anyway.
    let root = StartWorkerError::AppError(EngineToAppError::SlotBusy {
        occupying_run_id: Some("run-husk".to_owned()),
    });
    let wrapped: anyhow::Error = anyhow::Error::new(root).context("spawning worker pane for run exec-1");
    assert_eq!(slot_busy_occupant(&wrapped), Some(Some("run-husk".to_owned())));
}

#[test]
fn slot_busy_occupant_handles_missing_occupying_run_id() {
    // Older apps predating the field send `SlotBusy` with no
    // payload — must decode as `Some(None)` (the error IS
    // SlotBusy, but the occupant is unknown), not `None`
    // (not-a-SlotBusy-error at all).
    let root = StartWorkerError::AppError(EngineToAppError::SlotBusy { occupying_run_id: None });
    let wrapped: anyhow::Error = anyhow::Error::new(root).context("spawning worker pane for run exec-2");
    assert_eq!(slot_busy_occupant(&wrapped), Some(None));
}

#[test]
fn slot_busy_occupant_is_none_for_other_start_worker_errors() {
    let root = StartWorkerError::AppError(EngineToAppError::NoAvailableSlot);
    let wrapped: anyhow::Error = anyhow::Error::new(root).context("spawning worker pane for run exec-3");
    assert_eq!(slot_busy_occupant(&wrapped), None);
}

#[test]
fn slot_busy_occupant_is_none_for_unrelated_errors() {
    let wrapped = anyhow::anyhow!("workspace lease failed");
    assert_eq!(slot_busy_occupant(&wrapped), None);
}

#[test]
fn slot_id_from_worker_id_rejects_garbage() {
    assert_eq!(slot_id_from_worker_id(""), None);
    assert_eq!(slot_id_from_worker_id("worker"), None);
    assert_eq!(slot_id_from_worker_id("worker-"), None);
    assert_eq!(slot_id_from_worker_id("worker-0"), None);
    assert_eq!(slot_id_from_worker_id("worker-abc"), None);
    assert_eq!(slot_id_from_worker_id("agent-1"), None);
}

#[test]
fn pool_model_override_for_worker_id_returns_opus_for_review_and_automation() {
    // Review and automation pools always pin to Opus per the automated-reviewer
    // design §5. Main-pool workers have no override and fall through to the
    // effort-driven default.
    for ordinal in 1u8..=MAX_REVIEW_POOL_SIZE as u8 {
        let wid = format!("review-{ordinal}");
        assert_eq!(
            pool_model_override_for_worker_id(&wid),
            Some("opus"),
            "review pool worker {wid:?} must return opus override"
        );
    }
    for ordinal in 1u8..=MAX_AUTOMATION_POOL_SIZE as u8 {
        let wid = format!("auto-worker-{ordinal}");
        assert_eq!(
            pool_model_override_for_worker_id(&wid),
            Some("opus"),
            "automation pool worker {wid:?} must return opus override"
        );
    }
    for ordinal in 1u8..=MAX_WORKER_POOL_SIZE as u8 {
        let wid = format!("worker-{ordinal}");
        assert_eq!(
            pool_model_override_for_worker_id(&wid),
            None,
            "main pool worker {wid:?} must return no override"
        );
    }
}

#[tokio::test]
async fn worker_pool_claims_lowest_free_slot_deterministically() {
    // Claim-release-claim must always return to the lowest free slot —
    // the deterministic replacement for the old random spread. Every
    // claim after a release lands back on worker-1, never a higher slot.
    let pool = WorkerPool::new(4);
    for i in 0..50 {
        let claimed = pool.claim_worker(&format!("exec-{i}"), None).await.unwrap();
        assert_eq!(
            claimed, "worker-1",
            "deterministic claim must always pick the lowest free slot"
        );
        pool.release_worker(&claimed, None).await;
    }
    // Held claims fill strictly in ascending slot order.
    let mut held = Vec::new();
    for i in 0..4 {
        held.push(pool.claim_worker(&format!("hold-{i}"), None).await.unwrap());
    }
    assert_eq!(held, vec!["worker-1", "worker-2", "worker-3", "worker-4"]);
}

#[tokio::test]
async fn worker_pool_strict_spillover_fills_bridge_crew_before_lower_decks() {
    // The interactive pool is two pages of WORKER_PAGE_SIZE. Bridge Crew
    // (page 0) must be fully occupied before any Lower Decks (page 1) slot
    // is claimed, and a freed Bridge Crew slot must be preferred over an
    // idle Lower Decks slot at the next claim (preference is claim-time
    // only — running Lower Decks workers are never migrated).
    let pool = WorkerPool::new(MAX_WORKER_POOL_SIZE);

    // The first WORKER_PAGE_SIZE claims all land on Bridge Crew, in order.
    for n in 1..=WORKER_PAGE_SIZE {
        let claimed = pool.claim_worker(&format!("bc-{n}"), None).await.unwrap();
        assert_eq!(claimed, format!("worker-{n}"), "claim {n} must stay on Bridge Crew");
        assert_eq!(
            worker_page_label(slot_id_from_worker_id(&claimed).unwrap()).as_deref(),
            Some("Bridge Crew")
        );
    }

    // With all 8 Bridge Crew slots occupied, the 9th concurrent claim is
    // the first to spill into Lower Decks — worker-9, slot 9, page 1.
    let spill = pool.claim_worker("ld-1", None).await.unwrap();
    assert_eq!(spill, format!("worker-{}", WORKER_PAGE_SIZE + 1));
    let spill_slot = slot_id_from_worker_id(&spill).unwrap();
    assert_eq!(spill_slot, WORKER_PAGE_SIZE as u8 + 1);
    assert_eq!(worker_page_label(spill_slot).as_deref(), Some("Lower Decks"));

    // Free a Bridge Crew slot (worker-3). The next claim must reclaim it
    // rather than continuing to grow Lower Decks — strict spillover applies
    // at claim time, so a free page-0 slot always beats an idle page-1 one.
    pool.release_worker("worker-3", None).await;
    let reclaim = pool.claim_worker("bc-again", None).await.unwrap();
    assert_eq!(
        reclaim, "worker-3",
        "a freed Bridge Crew slot must be preferred over Lower Decks"
    );
}

#[tokio::test]
async fn higher_priority_executions_run_first() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let early = create_test_chore(&db, product.id.clone(), "Old");
    let late = create_test_chore(&db, product.id.clone(), "New");
    db.reconcile_product_executions(&product.id).unwrap();

    // Bump the later chore's priority — it should run first despite
    // the older one being in the queue first.
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(late.id.clone())
            .priority(10)
            .build(),
    )
    .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));
    coordinator.kick();

    for _ in 0..100 {
        let runs = runner.calls.lock().await;
        if !runs.is_empty() {
            break;
        }
        drop(runs);
        sleep(Duration::from_millis(10)).await;
    }

    let calls = runner.calls.lock().await;
    assert!(!calls.is_empty(), "scheduler did not start any run");
    let started_execution_id = &calls[0].1;
    let late_execution = db.list_executions(Some(&late.id)).unwrap().pop().unwrap();
    assert_eq!(
        started_execution_id, &late_execution.id,
        "expected the higher-priority chore to run first"
    );
    // Old chore should still be queued (and was NOT picked).
    let early_execution = db.list_executions(Some(&early.id)).unwrap().pop().unwrap();
    assert_eq!(early_execution.status, ExecutionStatus::Ready);
}

/// Dispatch-class acceptance test (operator directive: revisions before
/// tasks/chores, ordered by revision kind): a merge-conflict-fixing
/// revision (class 1) must claim a single free slot before an ordinary
/// chore (class 5) that has been sitting in the ready queue longer —
/// the exact opposite of what plain FIFO-by-creation-time would pick.
#[tokio::test]
async fn merge_conflict_revision_outranks_older_ready_chore_for_a_single_free_slot() {
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

    // Older, ordinary chore — created (and thus ready) first.
    let chore = create_test_chore(&db, product.id.clone(), "Ordinary chore");
    db.reconcile_product_executions(&product.id).unwrap();

    // Newer merge-conflict-fixing revision — `created_at` is stamped
    // far in the future so a plain FIFO queue would place it dead last;
    // dispatch class must still put it first.
    let revision_id = "task_merge_conflict_outranks_test";
    {
        let conn = db.connect().unwrap();
        conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, created_via)
                 VALUES (?1, ?2, 'revision', 'Fix merge conflict', '', 'todo', '2099-01-01T00:00:00Z', '2099-01-01T00:00:00Z', 'merge-conflict:crz_1')",
                rusqlite::params![revision_id, product.id],
            )
            .unwrap();
    }
    let revision_execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision_id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));
    coordinator.kick();

    for _ in 0..100 {
        let runs = runner.calls.lock().await;
        if !runs.is_empty() {
            break;
        }
        drop(runs);
        sleep(Duration::from_millis(10)).await;
    }

    let calls = runner.calls.lock().await;
    assert!(!calls.is_empty(), "scheduler did not start any run");
    assert_eq!(
        &calls[0].1, &revision_execution.id,
        "the merge-conflict revision must dispatch before the older ordinary chore",
    );
    drop(calls);

    let chore_execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    assert_eq!(
        chore_execution.status,
        ExecutionStatus::Ready,
        "the older chore must remain queued behind the higher-class revision",
    );
}

/// Selection-time ordering only — a higher dispatch class must never
/// preempt a worker that already claimed the slot. Once a slot is
/// running, a newly-arrived class-1 revision simply queues behind it
/// like anything else and dispatches only when the slot frees.
#[tokio::test]
async fn running_worker_is_never_preempted_by_a_higher_dispatch_class_arrival() {
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

    let _chore = create_test_chore(&db, product.id.clone(), "Ordinary chore");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));
    coordinator.kick();

    // Wait for the chore to actually claim the single slot and go
    // `running` before the higher-class revision even exists.
    for _ in 0..200 {
        let executions = db.list_executions(None).unwrap();
        if executions.iter().any(|e| e.status == ExecutionStatus::Running) {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    {
        let calls = runner.calls.lock().await;
        assert_eq!(calls.len(), 1, "the chore must have claimed the single slot first");
    }

    // A class-1 merge-conflict revision arrives after the slot is gone.
    let revision_id = "task_merge_conflict_arrives_after_slot_claimed";
    {
        let conn = db.connect().unwrap();
        conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, created_via)
                 VALUES (?1, ?2, 'revision', 'Fix merge conflict', '', 'todo', '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z', 'merge-conflict:crz_2')",
                rusqlite::params![revision_id, product.id],
            )
            .unwrap();
    }
    let revision_execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision_id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    coordinator.kick();
    // Give the scheduler a window to (incorrectly) act, if it were
    // going to. There is no positive event to wait on here — the
    // assertion is that nothing changes.
    sleep(Duration::from_millis(100)).await;

    let calls = runner.calls.lock().await;
    assert_eq!(
        calls.len(),
        1,
        "the running worker must not be preempted by a newly-arrived higher-class execution",
    );
    drop(calls);

    let revision_status = db.get_execution(&revision_execution.id).unwrap().status;
    assert_eq!(
        revision_status,
        ExecutionStatus::Ready,
        "the higher-class revision must queue behind the running slot, not preempt it",
    );
}

#[tokio::test]
async fn scheduler_passes_preferred_workspace_to_lease_and_records_affinity() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .preferred_workspace_id("mono-agent-007")
            .build(),
    )
    .unwrap();

    let cube = Arc::new(FakeCubeClient::default().with_next_workspace_id("mono-agent-007"));
    let runner = Arc::new(FakeExecutionRunner::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));
    coordinator.kick();

    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::WaitingHuman).await;

    let calls = cube.lease_calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].2.as_deref(), Some("mono-agent-007"));
    drop(calls);

    let execution = db.get_execution(&execution.id).unwrap();
    assert_eq!(execution.cube_workspace_id.as_deref(), Some("mono-agent-007"));
    assert_eq!(
        coordinator.worker_pool().worker_affinity("worker-1").await.as_deref(),
        Some("mono-agent-007")
    );
}

#[tokio::test]
async fn coordinator_publishes_execution_topic_events() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let publisher = Arc::new(RecordingPublisher::default());
    let coordinator = Arc::new(ExecutionCoordinator::with_publisher(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
        publisher.clone(),
    ));
    coordinator.kick();

    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::WaitingHuman).await;

    let events = publisher.publish_calls.lock().await;
    let reasons: Vec<&str> = events.iter().map(|(_, _, _, reason)| reason.as_str()).collect();
    assert!(reasons.contains(&"execution_started"));
    assert!(reasons.contains(&"execution_run_completed"));
    let last_status = events
        .iter()
        .rev()
        .find(|(_, _, _, reason)| reason == "execution_run_completed")
        .map(|(_, _, status, _)| status.clone());
    assert_eq!(last_status.as_deref(), Some("waiting_human"));

    // The kanban activity-icon depends on a work-tree invalidation
    // on run completion, otherwise the card would stay stuck on
    // "active" after the agent moved to waiting_human. Confirm the
    // coordinator now fires the broadcast on the completion path
    // too — not just on execution-start auto-advance.
    let work_item_events = publisher.events.lock().await;
    assert!(
        work_item_events
            .iter()
            .any(|(_, _, reason)| { reason == "execution_run_completed" }),
        "expected execution_run_completed work-item invalidation, got: {:?}",
        *work_item_events,
    );
}

/// When `start_execution_run` auto-advances `tasks.status` to
/// `'active'`, the coordinator must also publish a work-tree
/// invalidation so kanban subscribers re-fetch the board. Without
/// this, the DB has the right value but the GUI never refreshes
/// — the bug surfaced manually that this test exists to prevent.
#[tokio::test]
async fn coordinator_publishes_work_item_changed_on_execution_start() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let publisher = Arc::new(RecordingPublisher::default());
    let coordinator = Arc::new(ExecutionCoordinator::with_publisher(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
        publisher.clone(),
    ));
    coordinator.kick();

    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::WaitingHuman).await;

    // Work-item invalidation should have fired with the chore's
    // product id and the chore's work-item id. Reason wording
    // isn't load-bearing but we assert it's there to confirm the
    // call site is the auto-advance one and not some unrelated
    // future broadcast.
    let work_item_events = publisher.events.lock().await;
    assert!(
        work_item_events.iter().any(|(product_id, work_item_id, reason)| {
            product_id == &product.id && work_item_id == &chore.id && reason == "execution_started_auto_advance"
        }),
        "expected execution_started_auto_advance event for chore {} on product {}, got: {:?}",
        chore.id,
        product.id,
        *work_item_events,
    );

    // And the DB-level auto-advance itself: the chore status must
    // have flipped from `todo` to `active` when the execution
    // started running.
    let advanced = db.get_work_item(&chore.id).unwrap();
    match advanced {
        WorkItem::Chore(t) | WorkItem::Task(t) => {
            assert_eq!(t.status, TaskStatus::Active, "chore should auto-advance to active");
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn scheduler_respects_worker_pool_capacity() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let first_project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Design A".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
        })
        .unwrap();
    let second_project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Design B".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
        })
        .unwrap();
    db.create_task(
        CreateTaskInput::builder()
            .product_id(product.id.clone())
            .project_id(first_project.id.clone())
            .name("A1")
            .build(),
    )
    .unwrap();
    db.create_task(
        CreateTaskInput::builder()
            .product_id(product.id.clone())
            .project_id(second_project.id.clone())
            .name("B1")
            .build(),
    )
    .unwrap();
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    ));
    coordinator.kick();
    for _ in 0..100 {
        let executions = db.list_executions(None).unwrap();
        if executions
            .iter()
            .filter(|execution| execution.status == ExecutionStatus::Running)
            .count()
            == 1
        {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }

    let executions = db.list_executions(None).unwrap();
    assert_eq!(
        executions
            .iter()
            .filter(|execution| execution.status == ExecutionStatus::Running)
            .count(),
        1,
        "pool cap = 1 must keep exactly one execution `running`",
    );
    // Project design now lives on a per-project `kind = 'design'`
    // task at `ordinal = 0`, with the user's project_tasks at
    // `ordinal >= 1`. Only the design tasks are eligible for
    // `ready` until they complete; the user-tasks stay
    // `waiting_dependency` behind their project's design. So the
    // shape is: 1 running design, 1 ready design (gated on the
    // pool slot), 2 waiting_dependency project_tasks.
    assert_eq!(
        executions
            .iter()
            .filter(|execution| execution.status == ExecutionStatus::Ready)
            .count(),
        1,
    );
    assert_eq!(
        executions
            .iter()
            .filter(|execution| execution.status == ExecutionStatus::WaitingDependency)
            .count(),
        2,
    );
    assert_eq!(coordinator.worker_pool().idle_count().await, 0);
}

/// Ghost-active regression: when the worker pool is exhausted,
/// chores that lost the dispatcher's claim race must NOT have
/// `tasks.status` flipped to `'active'`. They stay in `todo` so
/// `boss chore list --status active` and `bossctl agents list`
/// agree on which chores actually have a worker.
///
/// Setup: pool capped at 1, three autostart chores reconciled into
/// `ready` executions back-to-back. Only one can be dispatched —
/// the other two must remain `todo` with no run record. This is
/// the test that would have caught the "6 active, 4 workers"
/// observation in the bug report.
#[tokio::test]
async fn pool_exhaustion_does_not_ghost_activate_chores() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    let mut chore_ids = Vec::new();
    for index in 0..3 {
        let chore = create_test_chore(&db, product.id.clone(), format!("Chore {index}"));
        chore_ids.push(chore.id);
    }
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    ));
    coordinator.kick();

    // Wait for the dispatcher to settle on exactly one running
    // execution. With pool=1 and 3 ready chores the loop must
    // claim the first slot, then break on pool exhaustion.
    for _ in 0..200 {
        let executions = db.list_executions(None).unwrap();
        if executions
            .iter()
            .filter(|execution| execution.status == ExecutionStatus::Running)
            .count()
            == 1
        {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }

    // One chore active with a run, two stay todo with no run.
    let mut active_with_run = 0usize;
    let mut still_todo = 0usize;
    for chore_id in &chore_ids {
        let item = db.get_work_item(chore_id).unwrap();
        let status = match item {
            WorkItem::Chore(t) | WorkItem::Task(t) => t.status,
            other => panic!("expected chore/task, got {other:?}"),
        };
        let executions = db.list_executions(Some(chore_id)).unwrap();
        assert_eq!(executions.len(), 1, "exactly one execution per chore");
        let runs = db.list_runs(&executions[0].id).unwrap();
        match status.as_str() {
            "active" => {
                assert_eq!(executions[0].status, ExecutionStatus::Running);
                assert_eq!(runs.len(), 1, "active chore must have a run record");
                assert_eq!(runs[0].status, "active");
                active_with_run += 1;
            }
            "todo" => {
                assert_eq!(executions[0].status, ExecutionStatus::Ready);
                assert!(
                    runs.is_empty(),
                    "todo chore must not have a run record yet, got {runs:?}",
                );
                still_todo += 1;
            }
            other => panic!(
                "chore {chore_id} unexpectedly in status `{other}` — \
                     `active` and `todo` are the only valid states for this \
                     pool-exhausted scenario",
            ),
        }
    }
    assert_eq!(
        active_with_run, 1,
        "exactly one chore should be active with a run; got {active_with_run}",
    );
    assert_eq!(
        still_todo, 2,
        "two chores should stay `todo` with no run; got {still_todo}",
    );
    assert_eq!(coordinator.worker_pool().idle_count().await, 0);
}

/// Root-cause regression (2026-07-01): pool exhaustion is a
/// transient capacity wait, not a failure. A chore that repeatedly loses
/// the pool-claim race (`worker_claimed/skipped reason=pool_exhausted`,
/// cycle after cycle across drain passes) must stay untouched — no
/// execution ever marked `failed`, `autostart` never flipped — and must
/// dispatch on its own the instant a slot frees, via the ordinary
/// `release_worker_and_kick` re-scan. No `force_dispatch` / manual
/// `bossctl work start` should ever be required to recover it.
#[tokio::test]
async fn pool_exhaustion_recovers_automatically_when_slot_frees_without_manual_intervention() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    let winner = create_test_chore(&db, product.id.clone(), "Winner");
    let waiter = create_test_chore(&db, product.id.clone(), "Waiter");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    ));
    coordinator.kick();

    // Settle: one chore claims the sole slot; the other is left `ready`
    // behind the exhausted pool.
    for _ in 0..200 {
        let running = db.list_executions(Some(&winner.id)).unwrap();
        let waiting = db.list_executions(Some(&waiter.id)).unwrap();
        if running.iter().any(|e| e.status == ExecutionStatus::Running)
            && waiting.len() == 1
            && waiting[0].status == ExecutionStatus::Ready
        {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }

    // Reproduce "repeated cycles" from the incident report: several more
    // drain passes while the pool stays full. None of these may touch
    // the waiting row.
    for _ in 0..5 {
        coordinator.kick();
        sleep(Duration::from_millis(10)).await;
    }

    let waiter_task = match db.get_work_item(&waiter.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(
        waiter_task.status.as_str(),
        "todo",
        "pool-exhausted chore must stay queued in Backlog, not be demoted or archived",
    );
    assert!(
        waiter_task.autostart,
        "pool exhaustion is a transient wait, not a failure — autostart must never be flipped off",
    );
    let waiter_executions = db.list_executions(Some(&waiter.id)).unwrap();
    assert_eq!(
        waiter_executions.len(),
        1,
        "no duplicate/extra execution should be created while waiting on the pool",
    );
    assert_eq!(
        waiter_executions[0].status,
        ExecutionStatus::Ready,
        "the waiting execution must stay `ready`, never `failed`, across pool_exhausted cycles",
    );

    // Free the slot exactly like a real completion would: every
    // completion path funnels through `release_worker_and_kick`.
    let winner_execution = db.list_executions(Some(&winner.id)).unwrap().remove(0);
    let claimed_worker_id = coordinator
        .worker_pool()
        .claims()
        .await
        .into_iter()
        .find(|claim| claim.execution_id == winner_execution.id)
        .map(|claim| claim.worker_id)
        .expect("winner's execution should hold a claimed worker slot");
    coordinator.release_worker_and_kick(&claimed_worker_id, None).await;

    // No manual intervention: the waiter must pick up the freed slot on
    // its own, driven purely by the release's kick.
    let mut waiter_running = false;
    for _ in 0..200 {
        let executions = db.list_executions(Some(&waiter.id)).unwrap();
        if executions.iter().any(|e| e.status == ExecutionStatus::Running) {
            waiter_running = true;
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert!(
        waiter_running,
        "pool-exhausted chore must auto-dispatch the instant a slot frees — no manual work-start needed",
    );
}

/// Boot-time heal: a `tasks.status = 'active'` row whose
/// executions never produced a `work_runs` entry (e.g. previous
/// engine crashed between the kanban drag and the dispatch claim,
/// or a `RequestExecution` raced ahead of an exhausted pool) is
/// demoted back to `todo` on startup. Items WITH run history are
/// left alone — `reconcile_active_dispatch` is the right tool for
/// those.
#[tokio::test]
async fn heal_ghost_active_demotes_chores_without_run_history() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    // Ghost A: dragged to Doing but no execution exists at all.
    let ghost_a = create_test_chore_manual(&db, product.id.clone(), "Ghost A");
    db.update_work_item(
        &ghost_a.id,
        crate::work::WorkItemPatch {
            status: Some("active".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();

    // Ghost B: dragged to Doing, has a `ready` execution but no
    // run yet — the "RequestExecution raced an exhausted pool"
    // shape from the bug report.
    let ghost_b = create_test_chore_manual(&db, product.id.clone(), "Ghost B");
    db.update_work_item(
        &ghost_b.id,
        crate::work::WorkItemPatch {
            status: Some("active".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(ghost_b.id.clone())
            .build(),
    )
    .unwrap();

    // Real worker: started a run before the engine restarted,
    // mimicking a crashed-mid-flight chore. heal must NOT touch
    // this — `reconcile_active_dispatch` redispatches it.
    let real = create_test_chore_manual(&db, product.id.clone(), "Real worker");
    let real_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(real.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    db.start_execution_run(
        &real_exec.id,
        "worker-1",
        "mono",
        "lease-1",
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )
    .unwrap();

    let healed = db.heal_ghost_active_chores().unwrap();
    let mut healed_ids: Vec<String> = healed.iter().map(|h| h.work_item_id.clone()).collect();
    healed_ids.sort();
    let mut expected = vec![ghost_a.id.clone(), ghost_b.id.clone()];
    expected.sort();
    assert_eq!(healed_ids, expected, "healed only the ghost rows");
    // product_id rides along so the caller can publish a
    // work-item-changed event on the product's kanban topic.
    for h in &healed {
        assert_eq!(h.product_id, product.id, "healed row should carry its product_id");
    }

    // Demoted ghosts now sit in `todo` and are stamped as engine-
    // initiated so the kanban can attribute the move correctly
    // instead of blaming the human who last dragged the row.
    for id in &[&ghost_a.id, &ghost_b.id] {
        match db.get_work_item(id).unwrap() {
            WorkItem::Chore(t) | WorkItem::Task(t) => {
                assert_eq!(t.status, TaskStatus::Todo);
                assert_eq!(t.last_status_actor, "engine");
            }
            other => panic!("expected chore/task, got {other:?}"),
        }
    }

    // Ghost B's stranded `ready` execution was abandoned so the
    // dispatcher won't claim a slot for a chore that just got
    // pulled out of the Doing column.
    let ghost_b_execs = db.list_executions(Some(&ghost_b.id)).unwrap();
    assert_eq!(ghost_b_execs.len(), 1);
    assert_eq!(ghost_b_execs[0].status, ExecutionStatus::Abandoned);

    // The real chore stays `active` with its `running` execution
    // intact — heal is conservative.
    match db.get_work_item(&real.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::Active),
        other => panic!("expected chore/task, got {other:?}"),
    }
    let real_execs = db.list_executions(Some(&real.id)).unwrap();
    assert_eq!(real_execs.len(), 1);
    assert_eq!(real_execs[0].status, ExecutionStatus::Running);
}

/// Regression coverage for PR #228. Default-sized pool
/// (`MAX_WORKER_POOL_SIZE` = 8) must dispatch all five chores when
/// they autostart back-to-back — the original bug was a pool that
/// silently capped at 1 (and an earlier-still incarnation that
/// capped at 4), so `kick()` broke out of `run_scheduler` after
/// claiming the first few workers and the rest stayed `ready`.
/// This test would have caught that: it asserts every one of the
/// five executions reaches `running`, and that the pool consumed
/// five distinct worker slots (so dispatch fanned out into the
/// 5..=8 range that the original bug had unreachable).
#[tokio::test]
async fn default_pool_dispatches_five_concurrent_autostart_chores() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    // Five autostart chores — the same shape `boss chore create`
    // produces when `--no-autostart` is omitted. Reconcile then
    // promotes each to a `ready` execution row.
    for index in 0..5 {
        create_test_chore(&db, product.id.clone(), format!("Chore {index}"));
    }
    db.reconcile_product_executions(&product.id).unwrap();

    // Use the default pool size so this test pins the contract
    // `WorkConfig::load_from_env` exposes to production.
    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(MAX_WORKER_POOL_SIZE),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    ));
    coordinator.kick();

    for _ in 0..200 {
        let executions = db.list_executions(None).unwrap();
        if executions
            .iter()
            .filter(|execution| execution.status == ExecutionStatus::Running)
            .count()
            == 5
        {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }

    let executions = db.list_executions(None).unwrap();
    let running = executions
        .iter()
        .filter(|execution| execution.status == ExecutionStatus::Running)
        .count();
    assert_eq!(
        running, 5,
        "expected all 5 autostart chores to be dispatched concurrently, got {running} running",
    );
    // Five of the default pool's slots are now busy; the remainder stay
    // idle. Derive the expectation from the pool size so this keeps pinning
    // the contract as the interactive pool grows pages.
    assert_eq!(coordinator.worker_pool().idle_count().await, MAX_WORKER_POOL_SIZE - 5);
}

/// `bossctl agents launch` (Phase 7 of the v2 plan) must dispatch
/// even when every configured slot is busy — the verb's whole point
/// is to *skip the queue*. We mirror the cap test above
/// (`scheduler_respects_worker_pool_capacity`) but with a smaller
/// pool so we can sit under the hard cap, fill every slot, and
/// then prove `force_dispatch` grows the pool by one slot and runs
/// the launched item immediately rather than leaving it `ready`.
#[tokio::test]
async fn force_dispatch_bypasses_configured_pool_cap() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let busy = create_test_chore(&db, product.id.clone(), "Already running");
    // A second chore that will sit in `ready` because the
    // configured pool size is 1 and `busy` claimed it.
    let queued = create_test_chore_manual(&db, product.id.clone(), "Skip the queue");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    ));
    coordinator.kick();

    // Wait for the first chore to actually be claimed by the lone
    // worker slot — otherwise force_dispatch might race the
    // scheduler and grow the pool unnecessarily.
    for _ in 0..200 {
        let busy_exec = db.list_executions(Some(&busy.id)).unwrap().pop().unwrap();
        if busy_exec.status == ExecutionStatus::Running {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(coordinator.worker_pool().idle_count().await, 0);
    assert_eq!(coordinator.worker_pool().capacity().await, 1);

    // `bossctl agents launch <queued.id>` enters the engine via
    // `RequestExecution { force: true }`. Promote `queued` to a
    // `ready` execution (the auto-start opt-out kept it parked),
    // then call the same coordinator entry point that `app.rs`
    // hits when `force = true`.
    let queued_exec = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(queued.id.clone())
                .force(true)
                .build(),
        )
        .unwrap();
    let worker_id = coordinator
        .force_dispatch(&queued_exec.id)
        .await
        .expect("force_dispatch should bypass the cap and return a worker id");
    assert_eq!(
        worker_id, "worker-2",
        "expected force_dispatch to grow the pool with a new slot",
    );

    for _ in 0..200 {
        let queued_after = db.list_executions(Some(&queued.id)).unwrap().pop().unwrap();
        if queued_after.status == ExecutionStatus::Running {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }

    let queued_after = db.list_executions(Some(&queued.id)).unwrap().pop().unwrap();
    assert_eq!(
        queued_after.status,
        ExecutionStatus::Running,
        "force-launched execution should be dispatched immediately",
    );
    assert_eq!(
        coordinator.worker_pool().capacity().await,
        2,
        "force_dispatch must grow the pool by one slot",
    );
    assert_eq!(coordinator.worker_pool().idle_count().await, 0);
}

/// The pool-grow path is hard-capped at `MAX_WORKER_POOL_SIZE`
/// because the macOS app renders one pane per interactive slot. A
/// force-launch request that arrives with every hard-cap slot busy must
/// surface a real error instead of silently overcommitting.
/// On-free rescan regression: a chore whose `tasks.status` is
/// `active` but whose latest execution is terminal (worker died,
/// cube lease errored, kanban-drag-while-pool-was-full) must be
/// redispatched the next time a worker frees up. Without the
/// rescan, `kick()` only sees `ready` executions and the stuck
/// chore stays in Doing forever.
#[tokio::test]
async fn worker_release_redispatches_active_chore_with_terminal_execution() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    // Warm-up chore: gets a normal `ready` execution so the
    // dispatcher has something to consume the single pool slot.
    // Its run completes via FakeExecutionRunner (WaitingHuman), at
    // which point the pool worker is released and our rescan fires.
    let warm = create_test_chore(&db, product.id.clone(), "Warm-up");
    db.reconcile_product_executions(&product.id).unwrap();

    // Stuck chore: `active` with a `failed` execution row,
    // mimicking the bug — worker died, kanban card stayed in
    // Doing, and the create-time dispatch path won't ever look
    // at it again.
    let stuck = create_test_chore(&db, product.id.clone(), "Stuck");
    db.update_work_item(
        &stuck.id,
        crate::work::WorkItemPatch {
            status: Some("active".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(stuck.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Failed)
            .repo_remote_url("git@github.com:spinyfin/mono.git")
            .build(),
    )
    .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    ));
    coordinator.kick();

    // Wait for the stuck chore to reach a non-failed execution
    // — that means the rescan inserted a fresh `ready` row and
    // the post-release `kick()` claimed it.
    for _ in 0..400 {
        let executions = db.list_executions(Some(&stuck.id)).unwrap();
        if executions.iter().any(|exec| exec.status.is_live()) {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }

    let warm_execs = db.list_executions(Some(&warm.id)).unwrap();
    let stuck_execs = db.list_executions(Some(&stuck.id)).unwrap();
    panic!(
        "stuck chore was never redispatched after warm-up release;\nwarm executions: {warm_execs:?}\nstuck executions: {stuck_execs:?}",
    );
}

/// Negative case for the rescan: an `autostart=false` chore that
/// is parked in `active` with a terminal execution must remain
/// untouched even after a worker frees up. The on-free rescan is
/// recurring; without the autostart filter it would loop on a
/// chore the user explicitly opted out of auto-handling.
#[tokio::test]
async fn worker_release_skips_no_autostart_active_chore() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    let warm = create_test_chore(&db, product.id.clone(), "Warm-up");
    db.reconcile_product_executions(&product.id).unwrap();

    let parked = create_test_chore_manual(&db, product.id.clone(), "Parked");
    db.update_work_item(
        &parked.id,
        crate::work::WorkItemPatch {
            status: Some("active".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(parked.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Failed)
            .repo_remote_url("git@github.com:spinyfin/mono.git")
            .build(),
    )
    .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner::default()),
    ));
    coordinator.kick();

    // Wait for the warm-up to settle (its run will finish on
    // WaitingHuman). After that the rescan has had its chance to
    // touch the parked chore — it must not have.
    wait_for_execution_status(
        db.as_ref(),
        &db.list_executions(Some(&warm.id)).unwrap()[0].id,
        ExecutionStatus::WaitingHuman,
    )
    .await;
    // Give the post-release rescan a clear window in which to
    // (incorrectly) redispatch the parked chore. 100ms is plenty
    // — the rescan is synchronous on the release path.
    sleep(Duration::from_millis(100)).await;

    let parked_execs = db.list_executions(Some(&parked.id)).unwrap();
    assert_eq!(
        parked_execs.len(),
        1,
        "autostart=false parked chore must not be redispatched, got {parked_execs:?}",
    );
    assert_eq!(parked_execs[0].status, ExecutionStatus::Failed);
}

#[tokio::test]
async fn force_dispatch_errors_at_hard_cap() {
    let pool = WorkerPool::new(MAX_WORKER_POOL_SIZE);
    for i in 0..MAX_WORKER_POOL_SIZE {
        pool.claim_worker(&format!("exec-{i}"), None)
            .await
            .expect("hard-cap pool should hand out one slot per claim");
    }
    assert_eq!(pool.idle_count().await, 0);
    assert!(
        pool.claim_worker_force("overflow", None).await.is_none(),
        "claim_worker_force must reject when the pool is already at the hard cap",
    );
    assert_eq!(
        pool.capacity().await,
        MAX_WORKER_POOL_SIZE,
        "rejected force-claim must not grow the pool past the hard cap",
    );
}

/// Regression for `task_18ae9d21044843b8_44` — `bossctl work start`
/// returned `status: ready` but no scheduler ever ran, leaving the
/// row stranded. Root cause was a TOCTOU between the scheduler's
/// last `list_ready_executions()` call and dropping its
/// `scheduling_active` guard: a `kick()` that landed in that
/// window observed `active=true`, returned without spawning, and
/// the guard then dropped to `false` with no scheduler running.
///
/// The fix latches every `kick()` into `scheduling_pending` so the
/// alive scheduler always notices the wakeup. This test pins the
/// contract: a `kick()` that arrives while `scheduling_active` is
/// already true MUST set `scheduling_pending` so the running
/// scheduler can re-enter its drain loop.
#[tokio::test]
async fn kick_during_active_scheduler_latches_pending_wakeup() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db,
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    ));

    // Simulate "another scheduler is already running".
    coordinator.scheduling_active.store(true, Ordering::Release);
    coordinator.scheduling_pending.store(false, Ordering::Release);

    coordinator.kick();

    assert!(
        coordinator.scheduling_pending.load(Ordering::Acquire),
        "kick that lost the active-flag race must still latch pending so the alive \
             scheduler re-enters its drain loop instead of exiting on stale state",
    );
}

/// End-to-end regression for the same race: even when a `kick()`
/// loses the active-flag race, the row it queued for must still
/// reach a worker. We can't deterministically force the OS into
/// the exact "scheduler just finished its drain" timing, but we
/// can prove the contract works by simulating the surviving
/// scheduler picking up the wakeup: the pending bit is the
/// in-process signal; if the pending bit is honored on the next
/// run_scheduler entry, the new row gets processed.
#[tokio::test]
async fn ready_row_added_during_active_window_still_dispatches() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Stranded by lost wakeup");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    ));

    // Simulate the bug-trigger sequence:
    //   1. A previous scheduler is "alive" (active=true) but
    //      has already finished its drain.
    //   2. RequestExecution lands, inserts a ready row, calls
    //      kick(). With the old code: kick observes active=true,
    //      returns, and the (now-exiting) scheduler drops the
    //      guard without re-checking. New row stranded.
    //   3. With the fix: kick latches pending=true.
    coordinator.scheduling_active.store(true, Ordering::Release);
    coordinator.scheduling_pending.store(false, Ordering::Release);
    coordinator.kick(); // noop on `active`, but latches pending

    // Now simulate the previous scheduler exiting: it must
    // honour the pending bit. Drop `active` and re-enter
    // `run_scheduler` exactly as the lossless-wakeup logic
    // would on the post-drain re-check path.
    coordinator.scheduling_active.store(false, Ordering::Release);
    assert!(
        coordinator.scheduling_pending.load(Ordering::Acquire),
        "post-drain re-check must see pending=true so the new row is not lost",
    );

    // The fix re-claims `active` and re-enters the drain. Kick
    // again to simulate that re-entry (this is what the
    // post-drain block in `run_scheduler` does internally), and
    // assert the row reaches `waiting_human`.
    coordinator.kick();
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::WaitingHuman).await;
}

/// Regression for the 2026-05-12 "`@` got re-pointed mid-flight"
/// incident (`mono-agent-001`, Worf's report). Pre-fix, the engine
/// never called `cube_client.heartbeat_lease` from anywhere — the
/// trait method had only stub implementations in test mocks. Any
/// worker that ran longer than `DEFAULT_LEASE_TTL_SECS = 1800` had
/// its lease silently age out, after which the next
/// `cube workspace lease` call from another execution reclaimed
/// the workspace and ran `jj new <main>` on the still-active
/// worker's working copy.
///
/// This test pins down the fix: while the guard is alive, the
/// heartbeat fires at the configured interval; dropping the guard
/// stops the heartbeat. The default 5-minute production interval
/// is shortened to 50 ms here so the test stays fast.
#[tokio::test]
async fn heartbeat_guard_renews_lease_until_dropped() {
    use super::helpers::{HeartbeatGuard, LocalHostAdapter};
    use crate::host_adapter::HostAdapter;

    let cube = Arc::new(FakeCubeClient::default());
    // Thin shim: wrap the FakeCubeClient in a LocalHostAdapter so the
    // HostAdapter-typed HeartbeatGuard interface is satisfied. The test
    // still inspects heartbeat_calls on the inner FakeCubeClient.
    let adapter: Arc<dyn HostAdapter> = Arc::new(LocalHostAdapter::new(
        cube.clone() as Arc<dyn CubeClient>,
        Arc::new(FakeExecutionRunner::default()),
    ));
    let guard = HeartbeatGuard::spawn_with_interval(
        adapter,
        "lease-1".to_owned(),
        "exec-1".to_owned(),
        "run-1".to_owned(),
        "worker-1".to_owned(),
        Duration::from_millis(50),
    );

    // Three intervals: expect at least two heartbeats (the first
    // tick is consumed at startup so the timer measures gaps).
    sleep(Duration::from_millis(180)).await;
    let beats_during = cube.heartbeat_calls.lock().await.len();
    assert!(
        beats_during >= 2,
        "expected >= 2 heartbeats in ~180ms with a 50ms interval, got {beats_during}",
    );
    for (lease, ttl) in cube.heartbeat_calls.lock().await.iter() {
        assert_eq!(lease, "lease-1");
        assert!(ttl.is_none(), "engine heartbeats use cube's default TTL");
    }

    // Drop stops the task. Sleep through more intervals and
    // assert the count is frozen — proving the heartbeat is
    // scoped to the guard's lifetime and cannot extend a lease
    // the run has already finished with.
    drop(guard);
    sleep(Duration::from_millis(50)).await;
    let beats_after_drop_snapshot = cube.heartbeat_calls.lock().await.len();
    sleep(Duration::from_millis(200)).await;
    let beats_final = cube.heartbeat_calls.lock().await.len();
    assert_eq!(
        beats_final, beats_after_drop_snapshot,
        "heartbeat must stop firing after the guard is dropped",
    );
}

/// Regression for `exec_18af3ba5259d32a8_12` (2026-05-13): a `ready`
/// execution row that misses its scheduler wakeup sits at
/// `status_transition` until the 90s-age orphan-active reconciler
/// rescues it. With the heartbeat installed, the same stranded row
/// reaches a worker within one heartbeat interval — no abandon /
/// redispatch needed.
///
/// The test simulates the failure mode by inserting a `ready` row
/// without calling `kick()`, then spawning the heartbeat with a
/// short interval. The heartbeat must observe the stranded row
/// (the "fail loudly" surface for operators) and re-kick so the
/// scheduler drains it.
#[tokio::test]
async fn heartbeat_rekicks_when_ready_row_was_orphaned_by_a_dropped_kick() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Stranded by lost wakeup");
    // Inserts a `ready` execution row but does NOT call `kick()`.
    // This mirrors the post-mortem evidence: the row exists, the
    // status_transition event was written, but no scheduler ever
    // picked the row up.
    db.reconcile_product_executions(&product.id).unwrap();
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    ));

    // Confirm the precondition: the row is `ready` and no scheduler
    // is running. (No `kick()` has been called.)
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Ready,
        "precondition: row must be `ready` before the heartbeat fires",
    );

    // Install the heartbeat with a short interval so the test
    // doesn't have to sleep for 15s of production cadence. The
    // heartbeat's startup-stagger sleep also uses this interval.
    let _handle = coordinator.spawn_scheduler_heartbeat(Duration::from_millis(80));

    // Within a few intervals the heartbeat should kick the
    // scheduler, drain the row, and move it through to
    // `waiting_human` via the fake runner.
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::WaitingHuman).await;
}

/// `stranded_ready_executions` is the read-side helper the heartbeat
/// uses to surface dropped-wakeup symptoms. This test pins its
/// contract directly so the heartbeat's `warn!` line is asserted on
/// without depending on timer behaviour: a row younger than the
/// configured threshold is invisible to the helper; once the row
/// crosses the threshold it appears with its actual age.
#[tokio::test]
async fn stranded_ready_executions_only_returns_rows_past_the_threshold() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Age boundary");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    ));

    // Threshold far in the future: the freshly-inserted row is too
    // young to count as stranded.
    let fresh = coordinator.stranded_ready_executions(60_000);
    assert!(
        fresh.is_empty(),
        "row younger than the threshold must not be flagged as stranded: {fresh:?}",
    );

    // Threshold of zero: any ready row should appear. The
    // execution we just inserted is in the queue with age >= 0.
    let any = coordinator.stranded_ready_executions(0);
    assert!(
        any.iter().any(|(id, _)| id == &execution_id),
        "with min_age_ms=0 the helper must surface the freshly-inserted ready row; \
             got {any:?}",
    );
}
