//! Worker-pool mechanics and scheduler basics: slot claiming/affinity,
//! worker-id round-trips, priority ordering, capacity, pool exhaustion and
//! recovery, force-dispatch, and heartbeat/kick behavior.
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;
use boss_protocol::{CommentAnchor, CreateCommentInput, CreateInvestigationInput};

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
fn pool_dispatch_policy_for_worker_id_pins_review_and_automation_to_claude_opus() {
    // Review and automation pools always dispatch on Claude at the strong
    // tier (Opus) per the automated-reviewer design §5 — independent of
    // whatever driver the reviewed/automated row itself carries. Main-pool
    // workers have no policy and fall through to the row's own driver /
    // the effort-driven default.
    let expected = PoolDispatchPolicy {
        driver: "claude",
        model_tier: PoolModelTier::Strong,
    };
    for ordinal in 1u8..=MAX_REVIEW_POOL_SIZE as u8 {
        let wid = format!("review-{ordinal}");
        assert_eq!(
            pool_dispatch_policy_for_worker_id(&wid),
            Some(expected),
            "review pool worker {wid:?} must dispatch on Claude/Opus"
        );
    }
    for ordinal in 1u8..=MAX_AUTOMATION_POOL_SIZE as u8 {
        let wid = format!("auto-worker-{ordinal}");
        assert_eq!(
            pool_dispatch_policy_for_worker_id(&wid),
            Some(expected),
            "automation pool worker {wid:?} must dispatch on Claude/Opus"
        );
    }
    for ordinal in 1u8..=MAX_WORKER_POOL_SIZE as u8 {
        let wid = format!("worker-{ordinal}");
        assert_eq!(
            pool_dispatch_policy_for_worker_id(&wid),
            None,
            "main pool worker {wid:?} must have no pool dispatch policy"
        );
    }
}

/// The kind-side companion of the policy above must agree with it: a kind
/// this reports as pool-pinned really does land on a pool whose worker ids
/// carry a dispatch policy, and an ordinary implementation kind does not.
/// Traffic allocation declines the former on the strength of this claim
/// (`work::driver_allocation::decide_execution_driver`), so the two must not
/// drift apart.
#[test]
fn kind_always_dispatches_on_pool_driver_matches_the_pinned_pools() {
    for (kind, worker_id) in [
        (boss_protocol::ExecutionKind::PrReview, "review-1"),
        (boss_protocol::ExecutionKind::AutomationTriage, "auto-worker-1"),
    ] {
        assert!(
            kind_always_dispatches_on_pool_driver(&kind),
            "{kind} runs on {worker_id}, whose driver is pinned"
        );
        assert!(
            pool_dispatch_policy_for_worker_id(worker_id).is_some(),
            "{worker_id} must carry a pinned dispatch policy"
        );
    }
    for kind in [
        boss_protocol::ExecutionKind::TaskImplementation,
        boss_protocol::ExecutionKind::ChoreImplementation,
        boss_protocol::ExecutionKind::RevisionImplementation,
        boss_protocol::ExecutionKind::InvestigationImplementation,
        boss_protocol::ExecutionKind::CiRemediation,
        boss_protocol::ExecutionKind::ConflictResolution,
    ] {
        assert!(
            !kind_always_dispatches_on_pool_driver(&kind),
            "{kind} dispatches on the main pool, which has no pinned driver"
        );
    }
    assert_eq!(pool_dispatch_policy_for_worker_id("worker-1"), None);
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
    seed_local_claude_driver(&db);
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
    seed_local_claude_driver(&db);
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
    seed_local_claude_driver(&db);
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
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;

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
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;

    let events = publisher.publish_calls.lock().await;
    let reasons: Vec<&str> = events.iter().map(|(_, _, _, reason)| reason.as_str()).collect();
    assert!(reasons.contains(&"execution_started"));
    assert!(reasons.contains(&"execution_run_completed"));
    let last_status = events
        .iter()
        .rev()
        .find(|(_, _, _, reason)| reason == "execution_run_completed")
        .map(|(_, _, status, _)| status.clone());
    assert_eq!(last_status.as_deref(), Some("running"));

    // The kanban activity-icon depends on a work-tree invalidation
    // on run completion, otherwise the card would stay stuck on
    // "active" after the spawn run closed. Confirm the
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
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;

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
        .force_dispatch(&queued_exec.id, DispatchAdmission::OperatorForced)
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

    // Wait for the warm-up to settle (its run finishes leaving the
    // execution live in `running`). After that the rescan has had its
    // chance to touch the parked chore — it must not have.
    wait_for_execution_status(
        db.as_ref(),
        &db.list_executions(Some(&warm.id)).unwrap()[0].id,
        ExecutionStatus::Running,
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

#[tokio::test]
async fn force_dispatch_refuses_a_failed_startup_preflight() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db,
        WorkerPool::new(1),
        Arc::new(FakeCubeClient::default()),
        Arc::new(FakeExecutionRunner::default()),
    ));
    coordinator.set_dispatch_preflight_block(Some("tmux 3.2 is required".to_owned()));

    let error = coordinator
        .force_dispatch("exec-preflight-blocked", DispatchAdmission::OperatorForced)
        .await
        .expect_err("force dispatch must not bypass a failed tmux preflight");
    assert!(error.to_string().contains("tmux 3.2 is required"));
}

/// `force_dispatch`'s original bug: `claim_worker_force`'s pool-growth path
/// always minted `worker-N` ids bounded by `MAX_WORKER_POOL_SIZE`, no matter
/// which `WorkerPool` instance it was called on. Pin the fix directly at the
/// `WorkerPool` level — independent of `force_dispatch`/pool classification
/// — by growing a review pool past its configured size and checking both
/// the minted id's prefix and the cap it is bounded by.
#[tokio::test]
async fn claim_worker_force_grows_review_pool_with_review_prefix_and_own_hard_cap() {
    let pool = WorkerPool::new_review(1);
    let first = pool.claim_worker("exec-0", None).await.unwrap();
    assert_eq!(first, "review-1");
    assert_eq!(pool.idle_count().await, 0);

    let grown = pool
        .claim_worker_force("exec-1", None)
        .await
        .expect("claim_worker_force should grow the review pool by one slot");
    assert_eq!(
        grown, "review-2",
        "a forced claim that grows the review pool must mint a review- prefixed id, not worker-",
    );
    assert_eq!(pool.capacity().await, 2);

    // Fill up to the review pool's OWN hard cap (MAX_REVIEW_POOL_SIZE),
    // never MAX_WORKER_POOL_SIZE (16, and much larger).
    for i in 2..MAX_REVIEW_POOL_SIZE {
        pool.claim_worker_force(&format!("exec-{i}"), None)
            .await
            .unwrap_or_else(|| panic!("claim_worker_force should still grow the review pool at slot {i}"));
    }
    assert_eq!(pool.capacity().await, MAX_REVIEW_POOL_SIZE);
    assert!(
        pool.claim_worker_force("overflow", None).await.is_none(),
        "claim_worker_force must reject once the review pool hits its OWN hard cap \
         ({MAX_REVIEW_POOL_SIZE}), not the much larger MAX_WORKER_POOL_SIZE",
    );
    assert_eq!(pool.capacity().await, MAX_REVIEW_POOL_SIZE);
}

/// The core regression this fix closes: `force_dispatch` on a `pr_review`
/// execution must classify it to the review pool exactly like
/// `drain_ready_queue` does, claim a `review-` worker id (not `worker-`),
/// resolve the pinned Opus reviewer policy through that id, and never touch
/// the interactive/main pool's busy count. Before the fix, `force_dispatch`
/// called `claim_worker_force` unconditionally on the main pool, which is
/// exactly how a `pr_review` recovery-probe landed a review on Sonnet in an
/// interactive slot (see the work item this closes).
#[tokio::test]
async fn force_dispatch_pr_review_claims_review_pool_worker_and_preserves_reviewer_policy() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/99";
    let (_, chore_id) = make_pr_review_fixture(&db, Some(pr_url));
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );
    coord.set_review_pool(WorkerPool::new_review(1));
    let coordinator = Arc::new(coord);

    let worker_id = coordinator
        .force_dispatch(&execution.id, DispatchAdmission::OperatorForced)
        .await
        .expect("force_dispatch should claim a review-pool slot");
    assert!(
        worker_id.starts_with(REVIEW_WORKER_ID_PREFIX),
        "a force-dispatched pr_review must claim a review- worker id, got {worker_id:?}",
    );
    assert_eq!(
        pool_dispatch_policy_for_worker_id(&worker_id),
        Some(PoolDispatchPolicy {
            driver: "claude",
            model_tier: PoolModelTier::Strong,
        }),
        "the review-pool worker id force_dispatch claimed must still resolve the pinned \
         Claude/Opus reviewer policy",
    );
    assert_eq!(
        coordinator.worker_pool().busy_count().await,
        0,
        "force-dispatching a pr_review must not consume an interactive/main-pool slot",
    );
    assert_eq!(coordinator.review_worker_pool().idle_count().await, 0);
}

/// Equivalent of the test above for an automation-pool kind: a regular
/// task execution whose owning task carries `source_automation_id` must
/// force-dispatch to an `auto-worker-` id (never `worker-`), resolve the
/// same pinned Claude/Opus pool policy, and leave the interactive pool
/// untouched.
#[tokio::test]
async fn force_dispatch_automation_sourced_task_claims_automation_pool_worker() {
    use crate::work::CreateAutomationInput;

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    let automation = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Test automation".to_owned(),
            repo_remote_url: None,
            trigger: boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * 1-5".to_owned(),
                timezone: "UTC".to_owned(),
            },
            standing_instruction: "do maintenance".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    let auto_chore = create_test_chore_manual(&db, product.id.clone(), "Automation chore");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
            rusqlite::params![automation.id, auto_chore.id],
        )
        .unwrap();
    }
    let execution = create_ready_chore_execution(&db, auto_chore.id.clone());

    let cube = Arc::new(FakeCubeClient::default());
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );
    coord.set_automation_pool(WorkerPool::new_automation(1));
    let coordinator = Arc::new(coord);

    let worker_id = coordinator
        .force_dispatch(&execution.id, DispatchAdmission::OperatorForced)
        .await
        .expect("force_dispatch should claim an automation-pool slot");
    assert!(
        worker_id.starts_with(AUTOMATION_WORKER_ID_PREFIX),
        "a force-dispatched automation-sourced task must claim an auto-worker- id, got {worker_id:?}",
    );
    assert_eq!(
        pool_dispatch_policy_for_worker_id(&worker_id),
        Some(PoolDispatchPolicy {
            driver: "claude",
            model_tier: PoolModelTier::Strong,
        }),
    );
    assert_eq!(
        coordinator.worker_pool().busy_count().await,
        0,
        "force-dispatching an automation-sourced task must not consume an interactive/main-pool slot",
    );
    assert_eq!(coordinator.automation_worker_pool().idle_count().await, 0);
}

/// Covers the `bossctl agents launch` entry point itself (not a direct
/// `force_dispatch` call): `RequestExecutionInput { force: true }` — the
/// same call `app/executions.rs` makes — on an automation-sourced chore
/// must still route to the automation pool, not silently land on the main
/// pool the way `force_dispatch` used to for every kind.
#[tokio::test]
async fn agents_launch_force_path_routes_automation_sourced_chore_to_automation_pool() {
    use crate::work::CreateAutomationInput;

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    let automation = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Test automation".to_owned(),
            repo_remote_url: None,
            trigger: boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * 1-5".to_owned(),
                timezone: "UTC".to_owned(),
            },
            standing_instruction: "do maintenance".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();

    // `autostart(false)` so the chore does NOT get an auto-created `ready`
    // execution — the launch verb's whole point is to skip-the-queue for a
    // chore that would otherwise sit parked.
    let auto_chore = create_test_chore_manual(&db, product.id.clone(), "Launched automation chore");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
            rusqlite::params![automation.id, auto_chore.id],
        )
        .unwrap();
    }

    let cube = Arc::new(FakeCubeClient::default());
    let mut coord = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );
    coord.set_automation_pool(WorkerPool::new_automation(1));
    let coordinator = Arc::new(coord);

    // Mirror `app/executions.rs`'s `force = true` handler exactly:
    // `request_execution` (force) then `force_dispatch` the resulting
    // `ready` row.
    let launched = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(auto_chore.id.clone())
                .force(true)
                .build(),
        )
        .unwrap();
    assert_eq!(launched.status, ExecutionStatus::Ready);

    let worker_id = coordinator
        .force_dispatch(&launched.id, DispatchAdmission::OperatorForced)
        .await
        .expect("agents-launch force_dispatch should claim an automation-pool slot");
    assert!(
        worker_id.starts_with(AUTOMATION_WORKER_ID_PREFIX),
        "bossctl agents launch on an automation-sourced chore must claim an auto-worker- id, \
         got {worker_id:?}",
    );
    assert_eq!(
        coordinator.worker_pool().busy_count().await,
        0,
        "bossctl agents launch on an automation-sourced chore must not consume an \
         interactive/main-pool slot",
    );
    assert_eq!(coordinator.automation_worker_pool().idle_count().await, 0);
}

/// Regression for PR #345 — `bossctl work start`
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
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Running).await;
}

/// Bus-routed sibling of [`kick_during_active_scheduler_latches_pending_wakeup`]
/// — same TOCTOU contract, exercised across `Event::DispatchReady` instead
/// of a direct `kick()` call. This is the gate for
/// `WorkConfig::enable_dispatch_ready_bus`: turning the flag on must not
/// weaken the kick/drain TOCTOU fix from PR #345, it must merely change
/// *how* the wakeup reaches the double latch.
///
/// With the flag on, `kick()` no longer latches `scheduling_pending`
/// itself — it publishes and returns. This test pins both halves of that
/// split: (1) a bus-routed `kick()` publishes `Event::DispatchReady`
/// without touching `scheduling_pending`, and (2) delivering that event
/// to `note_dispatch_ready` (exactly what
/// [`ExecutionCoordinator::spawn_dispatch_ready_subscriber`]'s loop does
/// per received event) while `scheduling_active` is already `true` still
/// latches `scheduling_pending`, so the alive scheduler re-enters its
/// drain loop instead of exiting on stale state.
#[tokio::test]
async fn dispatch_ready_event_during_active_scheduler_latches_pending_wakeup() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let cube = Arc::new(FakeCubeClient::default());
    let mut coordinator =
        ExecutionCoordinator::new(db, WorkerPool::new(1), cube, Arc::new(FakeExecutionRunner::default()));
    coordinator.set_enable_dispatch_ready_bus(true);
    let coordinator = Arc::new(coordinator);

    // Subscribe by hand rather than via `spawn_dispatch_ready_subscriber`
    // — that method's own reconcile-on-start pass would race the
    // `scheduling_active`/`scheduling_pending` state this test hand-sets
    // below. This is the same filter that method installs.
    let mut subscription = coordinator
        .event_bus
        .subscribe(TopicFilter::kind(EventKind::DispatchReady));

    // Simulate "another scheduler is already running".
    coordinator.scheduling_active.store(true, Ordering::Release);
    coordinator.scheduling_pending.store(false, Ordering::Release);

    coordinator.kick();
    assert!(
        !coordinator.scheduling_pending.load(Ordering::Acquire),
        "a bus-routed kick() must not latch scheduling_pending directly — only the \
         subscriber's reaction to the event may, otherwise the flag changes nothing \
         observable and there is nothing to roll back",
    );

    let event = subscription
        .recv()
        .await
        .expect("bus-routed kick() must publish DispatchReady");
    assert_eq!(event, Event::DispatchReady);

    // The subscriber's reaction to the event it just received.
    coordinator.note_dispatch_ready();

    assert!(
        coordinator.scheduling_pending.load(Ordering::Acquire),
        "a DispatchReady event delivered while scheduling_active is true must still latch \
         scheduling_pending so the alive scheduler re-enters its drain loop instead of \
         exiting on stale state — same TOCTOU contract as \
         kick_during_active_scheduler_latches_pending_wakeup, now proven on the bus-routed path",
    );
}

/// `spawn_dispatch_ready_subscriber` must spawn nothing when
/// `enable_dispatch_ready_bus` is off (the default) — an installation
/// that never opts in never pays for a subscriber loop, and `kick()`'s
/// direct path remains the sole trigger.
#[tokio::test]
async fn spawn_dispatch_ready_subscriber_is_noop_when_flag_off() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db,
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner::default()),
    ));

    assert!(
        coordinator.spawn_dispatch_ready_subscriber().is_none(),
        "the bus flag defaults off, so no subscriber task should be spawned",
    );
}

/// End-to-end gate for the flag-on wakeup path: unlike
/// [`dispatch_ready_event_during_active_scheduler_latches_pending_wakeup`]
/// (which subscribes by hand and calls `note_dispatch_ready` itself) and
/// [`spawn_dispatch_ready_subscriber_is_noop_when_flag_off`] (which never
/// turns the flag on), this test runs the real
/// `spawn_dispatch_ready_subscriber` loop with the flag on and proves
/// `kick()` reaches `note_dispatch_ready` through `kick -> Event::DispatchReady
/// on the bus -> the subscriber's loop` with no hand-call anywhere. An
/// inverted flag check, a wrong `TopicFilter`, or a loop that never calls
/// `note_dispatch_ready` would each hang this test until the bounded
/// `tokio::time::timeout` fires.
#[tokio::test]
async fn spawn_dispatch_ready_subscriber_delivers_kick_without_hand_call() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let cube = Arc::new(FakeCubeClient::default());
    let mut coordinator =
        ExecutionCoordinator::new(db, WorkerPool::new(1), cube, Arc::new(FakeExecutionRunner::default()));
    coordinator.set_enable_dispatch_ready_bus(true);
    let coordinator = Arc::new(coordinator);

    // Pre-set `scheduling_active` before the subscriber spawns, so its
    // reconcile-on-start `note_dispatch_ready()` call (see
    // `spawn_dispatch_ready_subscriber`'s doc) finds a scheduler already
    // "active" and only latches `scheduling_pending`, rather than racing
    // a real `run_scheduler` task to completion on an empty queue.
    coordinator.scheduling_active.store(true, Ordering::Release);
    coordinator.scheduling_pending.store(false, Ordering::Release);

    let handle = coordinator
        .spawn_dispatch_ready_subscriber()
        .expect("enable_dispatch_ready_bus is on, so a subscriber task must spawn");

    // Wait for the reconcile-on-start pass to latch pending, then reset
    // it to model an in-flight drain that has already observed and
    // cleared the wakeup — exactly the state the real, later kick()
    // below must re-latch.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if coordinator.scheduling_pending.load(Ordering::Acquire) {
                break;
            }
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("subscriber's reconcile-on-start pass must latch scheduling_pending");
    coordinator.scheduling_pending.store(false, Ordering::Release);

    // The real path under test — no hand-call to note_dispatch_ready.
    coordinator.kick();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if coordinator.scheduling_pending.load(Ordering::Acquire) {
                break;
            }
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect(
        "kick() with enable_dispatch_ready_bus on must reach note_dispatch_ready through \
         the real subscriber loop (kick -> bus -> subscriber -> note_dispatch_ready) \
         within a bounded timeout",
    );

    handle.abort();
}

/// Regression for the 2026-05-12 "`@` got re-pointed mid-flight"
/// incident (`mono-agent-001`, Worf's report). Pre-fix, the engine
/// never called `cube_client.heartbeat_lease` from anywhere — the
/// trait method had only stub implementations in test mocks. Any
/// worker that ran longer than the lease TTL (today 24h on both cube
/// and the engine heartbeat path) had
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
    wait_for_execution_status(db.as_ref(), &execution_id, ExecutionStatus::Running).await;
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
    let ready = db.list_ready_executions().unwrap();
    let fresh = coordinator.stranded_ready_executions(&ready, 60_000);
    assert!(
        fresh.is_empty(),
        "row younger than the threshold must not be flagged as stranded: {fresh:?}",
    );

    // Threshold of zero: any ready row should appear. The
    // execution we just inserted is in the queue with age >= 0.
    let any = coordinator.stranded_ready_executions(&ready, 0);
    assert!(
        any.iter().any(|(id, _)| id == &execution_id),
        "with min_age_ms=0 the helper must surface the freshly-inserted ready row; \
             got {any:?}",
    );
}

#[test]
fn overdue_ready_answer_agent_files_one_question_specific_attention() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let comment = db
        .create_comment(CreateCommentInput {
            artifact_kind: "pr_doc".to_owned(),
            artifact_id: "https://example.test/acme/repo.git:main:docs/answer.md".to_owned(),
            doc_version: "v1".to_owned(),
            anchor: CommentAnchor {
                exact: "Why is the job waiting?".to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "Why is the job waiting?".to_owned(),
            author: "operator".to_owned(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    db.transition_comment_to_answering(&comment.id).unwrap();
    let run = db
        .create_answer_agent_run(
            &comment.id,
            &comment.artifact_kind,
            &comment.artifact_id,
            &comment.doc_version,
            0,
        )
        .unwrap();
    let execution = db
        .create_answer_agent_execution(&comment.id, "https://example.test/acme/repo.git")
        .unwrap();
    db.bind_answer_agent_run_execution(&run.id, &execution.id).unwrap();

    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        Arc::new(FakeCubeClient::default()),
        Arc::new(FakeExecutionRunner::default()),
    );
    let ready = db.list_ready_executions().unwrap();
    coordinator.raise_ready_answer_agent_alarms(&ready, Duration::ZERO);
    coordinator.raise_ready_answer_agent_alarms(&ready, Duration::ZERO);

    // Read the item back through the same public API a real surface
    // (e.g. `handle_list_attention_items_for_work_item`) would use — not a
    // raw `SELECT ... WHERE work_item_id = ?`, which would pass even if
    // the comment-id row were unreachable through every other read path.
    let attentions = db.list_attention_items_for_work_item(&comment.id).unwrap();
    assert_eq!(
        attentions.len(),
        1,
        "repeated scheduler passes must deduplicate the alarm"
    );
    assert_eq!(attentions[0].kind, ANSWER_AGENT_READY_AGE_ATTENTION_KIND);
    let body_prefix = format!(
        "Question comment `{}` on document `pr_doc:{}` was first seen waiting more than ",
        comment.id, comment.artifact_id,
    );
    let body_suffix = format!(
        " without starting. See the engine log for the current age.\n\nUse `bossctl dispatch diagnose {}` to inspect its dispatch timeline.",
        execution.id,
    );
    assert!(
        attentions[0].body_markdown.starts_with(&body_prefix),
        "attention body must identify the waiting question and document"
    );
    assert!(
        attentions[0].body_markdown.ends_with(&body_suffix),
        "attention body must preserve the diagnosis command"
    );
    let attention_item_id = attentions[0].id.clone();

    db.create_run(
        CreateRunInput::builder()
            .agent_id("answer-agent")
            .execution_id(&execution.id)
            .started_at(boss_engine_utils::epoch_time::now_epoch_secs().to_string())
            .build(),
    )
    .unwrap();
    db.reconcile_stale_attention_signals().unwrap();
    let resolved = db.list_attention_items_for_work_item(&comment.id).unwrap();
    assert_eq!(
        resolved
            .iter()
            .find(|item| item.id == attention_item_id)
            .unwrap()
            .status,
        "resolved",
        "the answer-agent run start must clear its queue-age attention"
    );
}

#[tokio::test]
async fn answer_agent_start_records_queue_wait_metric_and_dispatch_event() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("Answer-agent metrics")
                .repo_remote_url("https://example.test/acme/repo.git")
                .build(),
        )
        .unwrap();
    let investigation = db
        .create_investigation(
            CreateInvestigationInput::builder()
                .product_id(product.id.clone())
                .name("Question owner")
                .build(),
        )
        .unwrap();
    db.set_task_doc_pointer(
        &investigation.id,
        Some("https://example.test/acme/repo.git"),
        Some("main"),
        Some("docs/answer.md"),
    )
    .unwrap();
    let comment = db
        .create_comment(CreateCommentInput {
            artifact_kind: "pr_doc".to_owned(),
            artifact_id: "pr_doc:https://example.test/acme/repo.git:main:docs/answer.md".to_owned(),
            doc_version: "v1".to_owned(),
            anchor: CommentAnchor {
                exact: "Why is this queued?".to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "Why is this queued?".to_owned(),
            author: "operator".to_owned(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    db.set_comment_intent(&comment.id, "question", 0.95).unwrap();
    db.transition_comment_to_answering(&comment.id).unwrap();
    let run = db
        .create_answer_agent_run(
            &comment.id,
            &comment.artifact_kind,
            &comment.artifact_id,
            &comment.doc_version,
            0,
        )
        .unwrap();
    let execution = db
        .create_answer_agent_execution(&comment.id, "https://example.test/acme/repo.git")
        .unwrap();
    db.bind_answer_agent_run_execution(&run.id, &execution.id).unwrap();

    let metrics = Arc::new(crate::metrics::Registry::new());
    crate::metrics_init::init_all(&metrics);
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let mut coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        Arc::new(FakeCubeClient::default()),
        Arc::new(FakeExecutionRunner::default()),
    )
    .with_dispatch_events(recording.clone());
    coordinator.set_metrics(metrics.clone());
    let coordinator = Arc::new(coordinator);
    let worker_id = coordinator
        .worker_pool()
        .claim_worker(&execution.id, None)
        .await
        .expect("worker should be available");

    coordinator
        .schedule_execution(&execution, &worker_id, DispatchAdmission::Queued)
        .await
        .unwrap();

    assert_eq!(
        metrics
            .counter_snapshot_one("answer_agent.started")
            .map(|snapshot| snapshot.value),
        Some(1),
    );
    assert_eq!(
        metrics
            .counter_snapshot_one("answer_agent.queue_wait_ms")
            .map(|snapshot| snapshot.value),
        Some(1),
    );
    let events = recording.events_for(&execution.id).await;
    let started = events
        .iter()
        .find(|event| event.stage == "run_started" && event.outcome == "ok")
        .unwrap_or_else(|| panic!("expected run_started event, got {events:#?}"));
    assert_eq!(
        started.details.get("execution_kind").and_then(|value| value.as_str()),
        Some("answer_agent"),
    );
    assert!(
        started
            .details
            .get("queue_wait_ms")
            .and_then(|value| value.as_u64())
            .is_some(),
        "run_started must carry queue_wait_ms: {started:#?}",
    );
}

// ── Concurrent dispatch hand-off tests ────────────────────────────────────

/// Create a `ready` chore execution with an explicit repo remote and
/// dispatch priority. `priority` is the ready queue's second sort key
/// (`priority DESC`), which is how these tests pin a specific row to the
/// head of the line rather than relying on `created_at`/`id` tiebreaks.
fn ready_chore_execution(db: &WorkDb, product_id: &str, name: &str, repo_remote_url: &str, priority: i64) -> String {
    let chore = create_test_chore(db, product_id.to_owned(), name);
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .repo_remote_url(repo_remote_url)
            .priority(priority)
            .build(),
    )
    .unwrap()
    .id
}

/// The head-of-line blocking regression observed on 2026-07-15.
///
/// A backlog of fast items behind one slow item must drain immediately
/// rather than waiting out the slow item's cube timeout budget. The slow
/// row is pinned FIRST in the ready queue (highest priority), so with the
/// old inline `schedule_execution(...).await` the two fast rows could not
/// dispatch until its `ensure_repo` returned — which is exactly the
/// ~1/minute drain the incident observed against free slots.
///
/// The slow item blocks for a delay far longer than the assertion window,
/// so the test fails on the old code by timeout rather than by a flaky
/// timing margin.
#[tokio::test]
async fn fast_items_dispatch_without_waiting_behind_a_slow_one() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    const SLOW_REPO: &str = "git@github.com:spinyfin/slow.git";
    const FAST_REPO: &str = "git@github.com:spinyfin/mono.git";

    // Priority DESC ⇒ the slow row is the head of the queue.
    let slow = ready_chore_execution(&db, &product.id, "Slow", SLOW_REPO, 100);
    let fast_a = ready_chore_execution(&db, &product.id, "Fast A", FAST_REPO, 50);
    let fast_b = ready_chore_execution(&db, &product.id, "Fast B", FAST_REPO, 50);

    let cube = Arc::new(FakeCubeClient {
        slow_ensure_origin: Some(SLOW_REPO.to_owned()),
        slow_ensure_delay: Duration::from_secs(600),
        ..FakeCubeClient::default()
    });
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    // Three free slots: nothing here is a capacity problem. The only
    // reason a fast row could fail to dispatch is the slow row ahead of
    // it.
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(3),
        cube.clone(),
        runner.clone(),
    ));
    coordinator.kick();

    wait_for_execution_status(db.as_ref(), &fast_a, ExecutionStatus::Running).await;
    wait_for_execution_status(db.as_ref(), &fast_b, ExecutionStatus::Running).await;

    // ...while the slow row is still stuck in its `ensure_repo`.
    assert_eq!(
        db.get_execution(&slow).unwrap().status,
        ExecutionStatus::Claimed,
        "the slow row should still be dispatching (claimed, not yet running) — the \
         point is that the fast rows overtook it rather than queueing behind it",
    );
}

/// A slow row must not be dispatched twice when a re-drain overlaps its
/// hand-off.
///
/// A handed-off row is CAS'd to `claimed` at pickup so the reconciler
/// cannot rewrite it; overlapping drains must not spawn it twice. Now that the drain loop
/// returns without awaiting dispatch, a kick landing mid-flight re-enters
/// the loop and sees that row again — and neither the chain guard nor the
/// double-spawn guard would catch the duplicate, since both exclude the
/// execution's own id. Only the in-flight filter stops it claiming a
/// second slot.
#[tokio::test]
async fn a_dispatch_in_flight_is_not_dispatched_again_by_a_later_kick() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);

    const SLOW_REPO: &str = "git@github.com:spinyfin/slow.git";
    let slow = ready_chore_execution(&db, &product.id, "Slow", SLOW_REPO, 100);

    let cube = Arc::new(FakeCubeClient {
        slow_ensure_origin: Some(SLOW_REPO.to_owned()),
        slow_ensure_delay: Duration::from_millis(300),
        ..FakeCubeClient::default()
    });
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(4),
        cube.clone(),
        runner.clone(),
    ));

    // Kick repeatedly while the first hand-off is still resolving its
    // `ensure_repo`, modelling the resume kick / slot-release kick / 15s
    // heartbeat all landing during a slow dispatch.
    for _ in 0..6 {
        coordinator.kick();
        sleep(Duration::from_millis(20)).await;
    }

    wait_for_execution_status(db.as_ref(), &slow, ExecutionStatus::Running).await;
    sleep(Duration::from_millis(100)).await;

    assert_eq!(
        runner.calls.lock().await.len(),
        1,
        "the row must spawn exactly one worker despite the overlapping kicks",
    );
    assert_eq!(
        cube.lease_calls.lock().await.len(),
        1,
        "a re-drain must not claim a second workspace for a dispatch already in flight",
    );
}

/// Two duplicate `ready` rows for one work item must not annihilate each
/// other.
///
/// The orphan sweep can leave two `ready` executions on one work item;
/// `schedule_execution`'s double-spawn guard resolves that by abandoning
/// whichever one finds the other already live. That guard is inherently
/// asymmetric against the DB (one row is `running`, the other `ready`) —
/// but two rows handed off in the same drain pass are in the IDENTICAL
/// state, so a naive in-flight check would have each abandon itself and
/// leave the work item with nothing live at all. The chain guard cannot
/// prevent it either: it deliberately excludes the caller's own work item.
/// Per-work-item reservations are what keep the second row out of flight.
#[tokio::test]
async fn duplicate_ready_rows_for_one_work_item_do_not_both_abandon() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Duplicated");

    const SLOW_REPO: &str = "git@github.com:spinyfin/slow.git";
    let mut ids = Vec::new();
    for _ in 0..2 {
        ids.push(
            db.create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Ready)
                    .repo_remote_url(SLOW_REPO)
                    .build(),
            )
            .unwrap()
            .id,
        );
    }

    // Premise guard: this test is only meaningful if the drain really does
    // see two distinct `ready` rows for one work item in a single pass.
    assert_ne!(ids[0], ids[1]);
    let ready = db.list_ready_executions().unwrap();
    assert_eq!(
        ready.len(),
        2,
        "premise: both duplicate rows must be ready and dispatchable in one pass",
    );

    // Slow enough that both rows are examined while the first hand-off is
    // still resolving — the window where both are `ready` and neither is
    // DB-visible as live.
    let cube = Arc::new(FakeCubeClient {
        slow_ensure_origin: Some(SLOW_REPO.to_owned()),
        slow_ensure_delay: Duration::from_millis(300),
        ..FakeCubeClient::default()
    });
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(4),
        cube.clone(),
        runner.clone(),
    ));
    coordinator.kick();

    wait_for_execution_status(db.as_ref(), &ids[0], ExecutionStatus::Running).await;
    sleep(Duration::from_millis(100)).await;

    let statuses: Vec<ExecutionStatus> = ids.iter().map(|id| db.get_execution(id).unwrap().status).collect();
    assert_eq!(
        statuses.iter().filter(|s| **s == ExecutionStatus::Running).count(),
        1,
        "exactly one duplicate must win the dispatch, got {statuses:?}",
    );
    assert!(
        !statuses.contains(&ExecutionStatus::Abandoned),
        "the loser must stay `ready` for the DB double-spawn guard to resolve once the \
         winner is running — abandoning it here risks both rows abandoning and the chore \
         never running at all; got {statuses:?}",
    );
    assert_eq!(
        runner.calls.lock().await.len(),
        1,
        "one work item must spawn one worker",
    );
}

/// The per-PR single-writer invariant must survive concurrent hand-off.
///
/// This is the hazard the in-flight registry exists for. The chain guard
/// decides liveness from `work_executions.status`, but a handed-off
/// dispatch stays `ready` until its run starts — so with a DB-only check,
/// two rows on the same chain both see "no live sibling" and both
/// dispatch, putting two writers on the one shared jj backing store cube
/// gives every same-PR workspace. The serial loop made that impossible for
/// free; the registry is what replaces it.
///
/// The revision sorts ahead of the chore (`DispatchClass` 4 before 5) and
/// is the slow one, so the chore is examined while the revision's dispatch
/// is still resolving its `ensure_repo` — precisely the window the DB
/// cannot see.
#[tokio::test]
async fn a_chain_sibling_defers_behind_a_dispatch_still_in_flight() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    const SLOW_REPO: &str = "git@github.com:spinyfin/slow.git";
    let pr_url = "https://github.com/spinyfin/mono/pull/1467";
    let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
            rusqlite::params![root_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
             SELECT 'task_rev_inflight', product_id, 'revision', 'Resolve conflicts', '', 'todo', '1', '1', ?1
             FROM tasks WHERE id = ?1",
            rusqlite::params![root_id],
        )
        .unwrap();
    }

    // Both rows are `ready`: neither is live in the DB when the other is
    // examined. Only the reservation distinguishes them.
    let revision_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id("task_rev_inflight")
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url(SLOW_REPO)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();
    let root_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root_id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient {
        slow_ensure_origin: Some(SLOW_REPO.to_owned()),
        slow_ensure_delay: Duration::from_secs(600),
        ..FakeCubeClient::default()
    });
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    // Free slots for both rows: if the chore defers, it is because of the
    // single-writer guard, not capacity.
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(4),
        cube.clone(),
        runner.clone(),
    ));
    coordinator.kick();

    // Let the drain hand the revision off and examine the chore behind it.
    sleep(Duration::from_millis(200)).await;

    assert_eq!(
        db.get_execution(&root_exec.id).unwrap().status,
        ExecutionStatus::Ready,
        "the chain root must stay `ready` — its sibling's dispatch is in flight, and \
         co-dispatching a second writer onto the shared jj backing store is the \
         two-writer corruption the chain guard exists to prevent",
    );
    assert_eq!(
        cube.ensure_calls.lock().await.as_slice(),
        [SLOW_REPO.to_owned()],
        "only the revision may dispatch; the chore must not have started one",
    );
    assert_eq!(
        db.get_execution(&revision_exec.id).unwrap().status,
        ExecutionStatus::Claimed,
        "sanity: the revision is still mid-dispatch (claimed, not yet running), so \
         the deferral above was decided against an in-flight sibling and not a \
         DB-visible live one",
    );
}
