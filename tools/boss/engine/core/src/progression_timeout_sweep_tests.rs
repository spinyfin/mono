use super::test_support::*;
use super::*;
use crate::stale_worker_sweep::{STALE_WORKER_ATTENTION_KIND, resolve_stale_worker_attention};
use crate::test_support::*;
use boss_protocol::{ExecutionStatus, WorkerEvent};

#[tokio::test]
async fn a_progression_run_past_its_timeout_is_reaped_failed_and_the_item_is_untouched() {
    let (_dir, db, work_item_id, execution_id, live_states, coordinator) = progression_fixture(Some(60)).await;
    let before = description_of(&db, &work_item_id);
    let reaper = RecordingReaper::new(coordinator.clone(), db.clone());

    let outcome = sweep(&db, &live_states, &coordinator, &reaper).await;

    assert_eq!(outcome, 1);
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
    assert_eq!(
        reaper.reaped(),
        vec![(execution_id.clone(), true)],
        "reaped through the sweep's ordinary reaper, before the slot is released"
    );
    assert!(
        !coordinator
            .progression_worker_pool()
            .claimed_execution_ids()
            .await
            .contains(&execution_id)
    );
    assert_eq!(description_of(&db, &work_item_id), before, "no audit line on the item");
    let run = db.list_runs(&execution_id).unwrap().remove(0);
    assert_eq!(run.status, "failed");
    assert!(run.error_text.unwrap().contains("progression-run-timeout:"));
    assert!(db.get_execution(&execution_id).unwrap().finished_at.is_some());
    assert!(
        db.readopt_inferred_terminal_execution(&execution_id, "late hook", None)
            .is_err()
    );
    assert!(
        db.list_attention_items_for_work_item(&work_item_id)
            .unwrap()
            .iter()
            .all(|item| item.kind != STALE_WORKER_ATTENTION_KIND),
        "no stale-worker attention on the item"
    );
}

/// After an engine restart an adopted pane starts as `Spawning`. The timeout
/// still reaps it: Grok never reports `AwaitingInput`, so a restricted run
/// left on an approval prompt stays Spawning for the whole wedge.
#[tokio::test]
async fn a_spawning_progression_run_past_its_timeout_is_reaped() {
    let (_dir, db, work_item_id, execution_id, live_states, coordinator) = progression_slot(Some(60), false).await;
    let before = description_of(&db, &work_item_id);
    let reaper = RecordingReaper::new(coordinator.clone(), db.clone());

    let outcome = sweep(&db, &live_states, &coordinator, &reaper).await;

    assert_eq!(outcome, 1);
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
    assert_eq!(description_of(&db, &work_item_id), before, "no audit line on the item");
}

/// When `started_at` was never recorded, the timeout uses `created_at`.
#[tokio::test]
async fn a_progression_run_with_no_started_at_is_reaped_from_created_at() {
    let (_dir, db, work_item_id, execution_id, live_states, coordinator) = progression_fixture(Some(60)).await;
    let created = boss_engine_utils::epoch_time::now_epoch_secs()
        .saturating_sub(300)
        .to_string();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET started_at = NULL, created_at = ?1 WHERE id = ?2",
            [&created, &execution_id],
        )
        .unwrap();
    assert!(db.get_execution(&execution_id).unwrap().started_epoch().is_none());
    let before = description_of(&db, &work_item_id);
    let reaper = RecordingReaper::new(coordinator.clone(), db.clone());

    let outcome = sweep(&db, &live_states, &coordinator, &reaper).await;

    assert_eq!(outcome, 1);
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
    assert_eq!(description_of(&db, &work_item_id), before);
}

#[tokio::test]
async fn claude_awaiting_input_is_bounded_and_releases_the_real_progression_pool() {
    let (_dir, db, _item, execution, states, coordinator) = progression_fixture(Some(60)).await;
    states.apply_event(
        SLOT,
        &WorkerEvent::Notification {
            session_id: "s".into(),
            message: "Claude needs permission".into(),
        },
    );
    assert_eq!(
        states.get(SLOT).unwrap().activity,
        boss_protocol::WorkerActivity::WaitingForInput
    );
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'waiting_human' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
    let reaper = RecordingReaper::new(coordinator.clone(), db.clone());
    assert_eq!(sweep(&db, &states, &coordinator, &reaper).await, 1);
    assert_eq!(db.get_execution(&execution).unwrap().status, ExecutionStatus::Failed);
    assert!(states.get(SLOT).is_none());
    assert!(
        coordinator
            .progression_worker_pool()
            .claim_progression_worker("replacement", 1)
            .await
            .is_some()
    );
}

#[tokio::test]
async fn spawning_and_missing_live_state_cannot_hide_an_expired_claim() {
    for registered in [true, false] {
        let (_dir, db, _item, execution, states, coordinator) = progression_fixture(Some(60)).await;
        states.release_slot_for_run(&execution);
        if registered {
            states.register_spawn(SLOT, &execution, "claude-sonnet", 1, None);
            assert_eq!(
                states.get(SLOT).unwrap().activity,
                boss_protocol::WorkerActivity::Spawning
            );
        }
        let reaper = RecordingReaper::new(coordinator.clone(), db.clone());
        assert_eq!(sweep(&db, &states, &coordinator, &reaper).await, 1);
        assert_eq!(db.get_execution(&execution).unwrap().status, ExecutionStatus::Failed);
        assert!(coordinator.progression_worker_pool().claims().await.is_empty());
    }
}

#[tokio::test]
async fn a_live_backed_orphan_within_its_timeout_is_not_reaped() {
    let (_dir, db, _item, execution, states, coordinator) = progression_fixture(Some(3_600)).await;
    db.mark_execution_orphaned(&execution, "worker lost").unwrap();
    age_terminal(&db, &execution);
    let reaper = RecordingReaper::new(coordinator.clone(), db.clone());
    assert_eq!(sweep(&db, &states, &coordinator, &reaper).await, 0);
    assert!(reaper.reaped().is_empty());
    assert!(states.get(SLOT).is_some());
    assert_eq!(coordinator.progression_worker_pool().claims().await.len(), 1);
}

#[tokio::test]
async fn an_orphaned_claim_is_reaped_past_its_timeout_and_preserves_its_outcome() {
    let (_dir, db, item, execution, states, coordinator) = progression_fixture(Some(60)).await;
    let before = description_of(&db, &item);
    db.mark_execution_orphaned(&execution, "worker lost").unwrap();
    age_terminal(&db, &execution);
    let orphaned = db.get_execution(&execution).unwrap();
    let reaper = RecordingReaper::new(coordinator.clone(), db.clone());
    assert_eq!(sweep(&db, &states, &coordinator, &reaper).await, 1);
    assert_eq!(
        serde_json::to_value(db.get_execution(&execution).unwrap()).unwrap(),
        serde_json::to_value(orphaned).unwrap()
    );
    assert_eq!(description_of(&db, &item), before);
    assert!(states.get(SLOT).is_none());
    assert!(coordinator.progression_worker_pool().claims().await.is_empty());
    assert_eq!(sweep(&db, &states, &coordinator, &reaper).await, 0);
    assert_eq!(reaper.reaped().len(), 1);
}

#[tokio::test]
async fn an_orphan_without_live_backing_is_reaped_after_the_terminal_grace() {
    let (_dir, db, _item, execution, states, coordinator) = progression_fixture(Some(3_600)).await;
    db.mark_execution_orphaned(&execution, "worker lost").unwrap();
    age_terminal(&db, &execution);
    states.release_slot_for_run(&execution);
    let reaper = RecordingReaper::new(coordinator.clone(), db.clone());
    assert_eq!(sweep(&db, &states, &coordinator, &reaper).await, 1);
}

#[tokio::test]
async fn deadline_cleanup_preserves_an_accepted_disposition() {
    let (_dir, db, _item, execution, states, coordinator) = progression_fixture(Some(60)).await;
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET progression_disposition = 'no_action' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
    let reaper = RecordingReaper::new(coordinator.clone(), db.clone());
    assert_eq!(sweep(&db, &states, &coordinator, &reaper).await, 1);
    assert_eq!(db.get_execution(&execution).unwrap().status, ExecutionStatus::Completed);
    assert_eq!(
        db.progression_disposition(&execution).unwrap(),
        Some(boss_protocol::ProgressionDisposition::NoAction)
    );
    assert!(coordinator.progression_worker_pool().claims().await.is_empty());
}

#[tokio::test]
async fn failed_teardown_retains_the_claim_and_retries_the_recorded_terminal_outcome() {
    for process_failed in [true, false] {
        let (_dir, db, _item, execution, states, coordinator) = progression_fixture(Some(60)).await;
        let reaper = RecordingReaper::new(coordinator.clone(), db.clone());
        let releaser: Arc<dyn WorkerPaneReleaser> = if process_failed {
            Arc::new(UnconfirmedProcess)
        } else {
            Arc::new(reaper.clone())
        };
        let viewers: Arc<dyn WorkerViewerDetach> = if process_failed {
            Arc::new(NoViewers)
        } else {
            Arc::new(UnconfirmedViewer)
        };
        let pending = ProgressionTimeoutSweep::builder()
            .work_db(db.clone())
            .live_states(states.clone())
            .coordinator(coordinator.clone())
            .pane_releaser(releaser)
            .hold_registry(Arc::new(crate::hold_registry::HoldRegistry::new()))
            .dispatch_events(Arc::new(crate::dispatch_events::NoopDispatchEventSink))
            .viewers(viewers)
            .build();
        assert_eq!(
            pending
                .run_one_pass(boss_engine_utils::epoch_time::now_epoch_secs())
                .await,
            0
        );
        age_terminal(&db, &execution);
        let failed = db.get_execution(&execution).unwrap();
        assert_eq!(failed.status, ExecutionStatus::Failed);
        assert_eq!(coordinator.progression_worker_pool().claims().await.len(), 1);
        assert_eq!(sweep(&db, &states, &coordinator, &reaper).await, 1);
        assert_eq!(
            serde_json::to_value(db.get_execution(&execution).unwrap()).unwrap(),
            serde_json::to_value(failed).unwrap()
        );
        assert!(coordinator.progression_worker_pool().claims().await.is_empty());
    }
}

#[tokio::test]
async fn exact_deadline_and_missing_start_time_use_the_published_limit() {
    let (_dir, db, _item, execution, states, coordinator) = progression_fixture(Some(60)).await;
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET started_at = NULL, created_at = '1000' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
    let sweep = ProgressionTimeoutSweep::builder()
        .work_db(db.clone())
        .live_states(states)
        .coordinator(coordinator.clone())
        .pane_releaser(Arc::new(RecordingReaper::new(coordinator, db.clone())))
        .hold_registry(Arc::new(crate::hold_registry::HoldRegistry::new()))
        .dispatch_events(Arc::new(crate::dispatch_events::NoopDispatchEventSink))
        .viewers(Arc::new(NoViewers))
        .build();
    assert_eq!(sweep.run_one_pass(1059).await, 0);
    assert_eq!(sweep.run_one_pass(1060).await, 1);
    assert!(
        db.list_runs(&execution).unwrap()[0]
            .error_text
            .as_ref()
            .unwrap()
            .contains("elapsed 60s (limit 60s)")
    );
}

#[tokio::test]
async fn a_suspend_gap_does_not_consume_the_run_timeout() {
    let (_dir, db, _item, execution, states, coordinator) = progression_fixture(Some(60)).await;
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET started_at = ?1 WHERE id = ?2",
            [&now.to_string(), &execution],
        )
        .unwrap();
    let sweep = ProgressionTimeoutSweep::builder()
        .work_db(db.clone())
        .live_states(states)
        .coordinator(coordinator.clone())
        .pane_releaser(Arc::new(RecordingReaper::new(coordinator, db.clone())))
        .hold_registry(Arc::new(crate::hold_registry::HoldRegistry::new()))
        .dispatch_events(Arc::new(crate::dispatch_events::NoopDispatchEventSink))
        .viewers(Arc::new(NoViewers))
        .build();
    assert_eq!(
        sweep
            .run_deadline_pass(DeadlineClock {
                wall_epoch_secs: now,
                awake_secs: 0,
            })
            .await,
        0
    );
    assert_eq!(
        sweep
            .run_deadline_pass(DeadlineClock {
                wall_epoch_secs: now + 8 * 3600,
                awake_secs: 30,
            })
            .await,
        0
    );
    assert_ne!(db.get_execution(&execution).unwrap().status, ExecutionStatus::Failed);
    assert_eq!(
        sweep
            .run_deadline_pass(DeadlineClock {
                wall_epoch_secs: now + 8 * 3600 + 30,
                awake_secs: 60,
            })
            .await,
        1
    );
    assert_eq!(db.get_execution(&execution).unwrap().status, ExecutionStatus::Failed);
}

#[tokio::test]
async fn a_sleep_before_first_sweep_does_not_exhaust_a_short_budget() {
    for sleep_secs in [5 * 60, 8 * 3600] {
        let (_dir, db, _item, execution, states, coordinator) = progression_fixture(Some(180)).await;
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET started_at = ?1 WHERE id = ?2",
                [&now.saturating_sub(sleep_secs).to_string(), &execution],
            )
            .unwrap();
        let sweep = ProgressionTimeoutSweep::builder()
            .work_db(db.clone())
            .live_states(states)
            .coordinator(coordinator.clone())
            .pane_releaser(Arc::new(RecordingReaper::new(coordinator, db.clone())))
            .hold_registry(Arc::new(crate::hold_registry::HoldRegistry::new()))
            .dispatch_events(Arc::new(crate::dispatch_events::NoopDispatchEventSink))
            .viewers(Arc::new(NoViewers))
            .build();
        // First observation charges at most one sweep interval (60s), not the sleep gap.
        assert_eq!(
            sweep
                .run_deadline_pass(DeadlineClock {
                    wall_epoch_secs: now,
                    awake_secs: 0,
                })
                .await,
            0,
            "sleep_secs={sleep_secs}"
        );
        assert_ne!(db.get_execution(&execution).unwrap().status, ExecutionStatus::Failed);
        // 119s more awake (total 179s < 180s): still running.
        assert_eq!(
            sweep
                .run_deadline_pass(DeadlineClock {
                    wall_epoch_secs: now + 119,
                    awake_secs: 119,
                })
                .await,
            0,
            "sleep_secs={sleep_secs}"
        );
        // 1s more awake (total 180s >= 180s): reaped.
        assert_eq!(
            sweep
                .run_deadline_pass(DeadlineClock {
                    wall_epoch_secs: now + 120,
                    awake_secs: 120,
                })
                .await,
            1,
            "sleep_secs={sleep_secs}"
        );
        assert_eq!(db.get_execution(&execution).unwrap().status, ExecutionStatus::Failed);
    }
}

#[tokio::test]
async fn restarting_after_near_budget_awake_time_does_not_grant_a_new_full_budget() {
    let (_dir, db, _item, execution, states, coordinator) = progression_fixture(Some(1800)).await;
    let t0 = 100_000_i64;
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET started_at = ?1 WHERE id = ?2",
            [&t0.to_string(), &execution],
        )
        .unwrap();
    let make_sweep = || {
        ProgressionTimeoutSweep::builder()
            .work_db(db.clone())
            .live_states(states.clone())
            .coordinator(coordinator.clone())
            .pane_releaser(Arc::new(RecordingReaper::new(coordinator.clone(), db.clone())))
            .hold_registry(Arc::new(crate::hold_registry::HoldRegistry::new()))
            .dispatch_events(Arc::new(crate::dispatch_events::NoopDispatchEventSink))
            .viewers(Arc::new(NoViewers))
            .build()
    };

    // Process 1: accrue 28 minutes (1680s) of awake time.
    let before_restart = make_sweep();
    assert_eq!(
        before_restart
            .run_deadline_pass(DeadlineClock {
                wall_epoch_secs: t0,
                awake_secs: 0,
            })
            .await,
        0
    );
    assert_eq!(
        before_restart
            .run_deadline_pass(DeadlineClock {
                wall_epoch_secs: t0 + 1680,
                awake_secs: 1680,
            })
            .await,
        0
    );
    drop(before_restart);

    // Process 2 (engine restart after an 8-hour sleep): resumes at 1680s + capped 60s = 1740s.
    // Does not fail immediately from the sleep, and does not grant a fresh 30-minute budget.
    let after_restart = make_sweep();
    let after_sleep_wall = t0 + 1680 + 8 * 3600;
    assert_eq!(
        after_restart
            .run_deadline_pass(DeadlineClock {
                wall_epoch_secs: after_sleep_wall,
                awake_secs: 0,
            })
            .await,
        0
    );
    assert_ne!(db.get_execution(&execution).unwrap().status, ExecutionStatus::Failed);

    // 59s more awake (1799s < 1800s): still running.
    assert_eq!(
        after_restart
            .run_deadline_pass(DeadlineClock {
                wall_epoch_secs: after_sleep_wall + 59,
                awake_secs: 59,
            })
            .await,
        0
    );
    // 1s more awake (1800s >= 1800s): fails right on schedule.
    assert_eq!(
        after_restart
            .run_deadline_pass(DeadlineClock {
                wall_epoch_secs: after_sleep_wall + 60,
                awake_secs: 60,
            })
            .await,
        1
    );
    assert_eq!(db.get_execution(&execution).unwrap().status, ExecutionStatus::Failed);
    assert!(
        db.get_metadata(&awake_elapsed_key(&execution)).unwrap().is_none(),
        "persisted awake-time key is cleaned up once the claim is released"
    );
}

#[tokio::test]
async fn a_backwards_wall_clock_correction_does_not_subtract_from_awake_elapsed() {
    let (_dir, db, _item, execution, states, coordinator) = progression_fixture(Some(120)).await;
    let t0 = 100_000_i64;
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET started_at = ?1 WHERE id = ?2",
            [&t0.to_string(), &execution],
        )
        .unwrap();
    let sweep = ProgressionTimeoutSweep::builder()
        .work_db(db.clone())
        .live_states(states)
        .coordinator(coordinator.clone())
        .pane_releaser(Arc::new(RecordingReaper::new(coordinator, db.clone())))
        .hold_registry(Arc::new(crate::hold_registry::HoldRegistry::new()))
        .dispatch_events(Arc::new(crate::dispatch_events::NoopDispatchEventSink))
        .viewers(Arc::new(NoViewers))
        .build();
    assert_eq!(
        sweep
            .run_deadline_pass(DeadlineClock {
                wall_epoch_secs: t0,
                awake_secs: 0,
            })
            .await,
        0
    );
    assert_eq!(
        sweep
            .run_deadline_pass(DeadlineClock {
                wall_epoch_secs: t0 + 60,
                awake_secs: 60,
            })
            .await,
        0
    );
    // Wall clock steps back one hour while the awake clock advances 60s to the deadline.
    assert_eq!(
        sweep
            .run_deadline_pass(DeadlineClock {
                wall_epoch_secs: t0 + 60 - 3600,
                awake_secs: 120,
            })
            .await,
        1
    );
    assert_eq!(db.get_execution(&execution).unwrap().status, ExecutionStatus::Failed);
    assert!(
        db.list_runs(&execution).unwrap()[0]
            .error_text
            .as_ref()
            .unwrap()
            .contains("elapsed 120s (limit 120s)")
    );
}

#[tokio::test]
async fn a_progression_run_inside_its_timeout_is_left_alone_even_when_it_looks_idle() {
    let (_dir, db, work_item_id, execution_id, live_states, coordinator) = progression_fixture(Some(3_600)).await;
    let before = description_of(&db, &work_item_id);
    let reaper = RecordingReaper::new(coordinator.clone(), db.clone());

    let outcome = sweep(&db, &live_states, &coordinator, &reaper).await;

    assert_eq!(outcome, 0);
    assert_ne!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Failed);
    assert!(reaper.reaped().is_empty());
    assert_eq!(description_of(&db, &work_item_id), before);
    assert!(
        db.list_attention_items_for_work_item(&work_item_id)
            .unwrap()
            .iter()
            .all(|item| item.kind != STALE_WORKER_ATTENTION_KIND),
    );
}

#[test]
fn the_timeout_falls_back_to_the_placeholder_when_nothing_is_published() {
    let (_dir, db) = open_db();
    assert_eq!(progression_run_timeout_secs(&db), 30 * 60);
}

/// The published limit round-trips from the engine's settings.
#[test]
fn the_engine_publishes_the_timeout_the_sweep_reads() {
    let (_dir, db) = open_db();
    db.configure_progression_calibration(
        &boss_engine_storage_values::progression::ProgressionSettings::builder()
            .run_timeout_secs(77)
            .build(),
    )
    .unwrap();
    assert_eq!(progression_run_timeout_secs(&db), 77);
}

fn open_stale_worker_attention_count(db: &WorkDb, work_item_id: &str) -> usize {
    db.list_attention_items_for_work_item(work_item_id)
        .unwrap()
        .iter()
        .filter(|item| item.kind == STALE_WORKER_ATTENTION_KIND && item.status == "open")
        .count()
}

fn seed_stale_worker_attention(db: &WorkDb, work_item_id: &str) {
    db.upsert_external_tracker_attention(work_item_id, STALE_WORKER_ATTENTION_KIND, "stuck item", "pre-existing")
        .unwrap();
    assert_eq!(open_stale_worker_attention_count(db, work_item_id), 1);
}

#[tokio::test]
async fn a_progression_timeout_reap_leaves_the_items_existing_stale_worker_attention_open() {
    let (_dir, db, work_item_id, _execution_id, live_states, coordinator) = progression_fixture(Some(60)).await;
    seed_stale_worker_attention(&db, &work_item_id);
    let reaper = RecordingReaper::new(coordinator.clone(), db.clone());

    let outcome = sweep(&db, &live_states, &coordinator, &reaper).await;

    assert_eq!(outcome, 1);
    assert_eq!(open_stale_worker_attention_count(&db, &work_item_id), 1);
}

#[tokio::test]
async fn a_progression_run_seen_terminal_by_the_sweep_leaves_the_items_stale_worker_attention_open() {
    let (_dir, db, work_item_id, execution_id, live_states, coordinator) = progression_fixture(Some(3_600)).await;
    seed_stale_worker_attention(&db, &work_item_id);
    db.mark_execution_orphaned(&execution_id, "test: ended").unwrap();
    let reaper = RecordingReaper::new(coordinator.clone(), db.clone());

    sweep(&db, &live_states, &coordinator, &reaper).await;

    assert_eq!(open_stale_worker_attention_count(&db, &work_item_id), 1);
}

#[tokio::test]
async fn the_pane_release_cleanup_helper_skips_a_progression_run_but_still_resolves_for_an_ordinary_one() {
    let (_dir, db, work_item_id, execution_id, _live_states, _coordinator) = progression_fixture(Some(3_600)).await;
    seed_stale_worker_attention(&db, &work_item_id);

    resolve_stale_worker_attention(&db, &execution_id);
    assert_eq!(
        open_stale_worker_attention_count(&db, &work_item_id),
        1,
        "progression teardown keeps the item's evidence"
    );

    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = 'chore_implementation' WHERE id = ?1",
            [&execution_id],
        )
        .unwrap();
    resolve_stale_worker_attention(&db, &execution_id);
    assert_eq!(open_stale_worker_attention_count(&db, &work_item_id), 0);
}
