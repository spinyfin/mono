use super::*;
use boss_protocol::{QueuedDisplay, QueuedTooltipTime};

const NOW: i64 = 1_800_000_000;

fn pending(db: &WorkDb, item: &str) -> WorkExecution {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(item)
                .kind(ExecutionKind::TaskImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_at = ?2 WHERE id = ?1",
            params![execution.id, NOW.to_string()],
        )
        .unwrap();
    execution
}

fn display(db: &WorkDb, item: &str, now: i64) -> QueuedDisplay {
    super::super::runtime_projection::compute_task_runtime(&db.connect().unwrap(), item, &mut 0, now)
        .unwrap()
        .runtime
        .queued_display
        .unwrap()
}

fn clear_wait_since(db: &WorkDb, execution_id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_wait_since = NULL WHERE id = ?1",
            [execution_id],
        )
        .unwrap();
}

#[test]
fn deadline_yields_to_live_cap_reason_without_a_write() {
    let (db, _, item) = setup_product_and_chore();
    let execution = pending(&db, &item);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_not_before = ?2,
         dispatch_wait_reason = 'Waiting for an available worker (11 working)' WHERE id = ?1",
            params![execution.id, (NOW + 120).to_string()],
        )
        .unwrap();
    let future = display(&db, &item, NOW);
    assert_eq!(future.text, "Queued — next attempt in 2 min");
    assert_eq!(
        future.tooltip_times,
        vec![QueuedTooltipTime {
            label: "Next dispatch attempt:".into(),
            epoch: NOW + 120,
        }]
    );
    let before = db.queued_displays(NOW, &Default::default()).unwrap();
    for now in [NOW + 120, NOW + 121] {
        let elapsed = display(&db, &item, now);
        assert_eq!(elapsed.text, "Queued — Waiting for an available worker (11 working)");
        assert!(elapsed.tooltip_times.is_empty());
        let after = db.queued_displays(now, &before).unwrap();
        assert_ne!(
            before[&item].display, after[&item].display,
            "heartbeat detects deadline expiry without a DB write"
        );
        assert_eq!(after[&item].display, elapsed);
    }
}

#[test]
fn abnormal_previous_outcome_precedes_the_current_hold() {
    for (status, verb) in [
        ("orphaned", "was orphaned"),
        ("failed", "failed"),
        ("cancelled", "was cancelled"),
        ("completed", ""),
    ] {
        let (db, _, item) = setup_product_and_chore();
        let previous = pending(&db, &item);
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET created_at = ?2, status = ?3, finished_at = ?4 WHERE id = ?1",
                params![previous.id, (NOW - 300).to_string(), status, (NOW - 60).to_string()],
            )
            .unwrap();
        let current = pending(&db, &item);
        db.set_dispatch_wait_reason(&current.id, "pool_exhausted").unwrap();
        clear_wait_since(&db, &current.id);
        let result = display(&db, &item, NOW);
        if status == "completed" {
            assert_eq!(result.text, "Queued — worker pool full");
        } else {
            assert_eq!(
                result.text,
                format!("Restarting after the previous run {verb} 1 minute ago. Queued — worker pool full")
            );
            assert_eq!(
                result.tooltip_times,
                vec![QueuedTooltipTime {
                    label: format!("Previous run {verb} at"),
                    epoch: NOW - 60,
                }]
            );
        }
    }
}

#[test]
fn retry_of_same_execution_uses_latest_run_and_then_clears_when_claimed() {
    let (db, _, item) = setup_product_and_chore();
    let execution = pending(&db, &item);
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO work_runs(id, execution_id, agent_id, status, created_at, finished_at)
         VALUES ('previous', ?1, 'agent', 'orphaned', ?2, ?3)",
            params![execution.id, (NOW - 300).to_string(), (NOW - 60).to_string()],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET pre_start_failure_count = 1, dispatch_not_before = ?2 WHERE id = ?1",
            params![execution.id, (NOW + 120).to_string()],
        )
        .unwrap();
    assert_eq!(
        display(&db, &item, NOW).text,
        "Restarting after the previous run was orphaned 1 minute ago. Retrying dispatch — next attempt in 2 min"
    );
    db.connect()
        .unwrap()
        .execute("UPDATE work_runs SET status = 'completed' WHERE id = 'previous'", [])
        .unwrap();
    assert_eq!(
        display(&db, &item, NOW).text,
        "Retrying dispatch — next attempt in 2 min"
    );
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'claimed' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
    assert!(db.get_task_runtime(&item).unwrap().queued_display.is_none());
    assert!(db.queued_displays(NOW, &Default::default()).unwrap().is_empty());
}

#[test]
fn spawn_completed_run_does_not_hide_orphaned_predecessor() {
    let (db, _, item) = setup_product_and_chore();
    let previous = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&item)
                .kind(ExecutionKind::TaskImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (_running, run) = db
        .start_execution_run(
            &previous.id,
            "agent",
            "mono",
            "lease-spawn",
            "mono-agent-qd",
            "/tmp/mono-agent-qd",
        )
        .unwrap();
    finish_run_worker_pane_alive(&db, &previous.id, &run.id, Some("spawned pane"));
    db.connect()
        .unwrap()
        .execute("UPDATE work_runs SET finished_at = '1000' WHERE id = ?1", [&run.id])
        .unwrap();
    let orphaned = db.mark_execution_orphaned(&previous.id, "worker died").unwrap();
    let finished = orphaned.finished_at.clone().expect("orphan stamps finished_at");
    let finished_epoch: i64 = finished.parse().unwrap();
    assert_ne!(
        finished, "1000",
        "spawn-complete time must not be the execution terminal time"
    );
    let replacement = db
        .request_resume_execution(&previous.id, 1, finished_epoch.saturating_sub(1), "worker died")
        .unwrap();
    db.set_dispatch_wait_reason(&replacement.id, "pool_exhausted").unwrap();
    clear_wait_since(&db, &replacement.id);
    let result = display(&db, &item, finished_epoch + 60);
    assert_eq!(
        result.text,
        "Restarting after the previous run was orphaned 1 minute ago. Queued — worker pool full"
    );
    assert_eq!(
        result.tooltip_times,
        vec![QueuedTooltipTime {
            label: "Previous run was orphaned at".into(),
            epoch: finished_epoch,
        }]
    );
}

#[test]
fn restart_prefix_skips_unrelated_or_stale_predecessors() {
    let (db, _, item) = setup_product_and_chore();
    let previous = pending(&db, &item);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_at = ?2, status = 'orphaned', finished_at = ?3,
             kind = 'pr_review' WHERE id = ?1",
            params![previous.id, (NOW - 300).to_string(), (NOW - 60).to_string()],
        )
        .unwrap();
    let current = pending(&db, &item);
    db.set_dispatch_wait_reason(&current.id, "pool_exhausted").unwrap();
    clear_wait_since(&db, &current.id);
    assert_eq!(
        display(&db, &item, NOW).text,
        "Queued — worker pool full",
        "a later execution of a different kind is not a restart of the predecessor"
    );

    let (db, _, item) = setup_product_and_chore();
    let previous = pending(&db, &item);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_at = ?2, status = 'cancelled', finished_at = ?3 WHERE id = ?1",
            params![previous.id, (NOW - 200_000).to_string(), (NOW - 172_800).to_string()],
        )
        .unwrap();
    let current = pending(&db, &item);
    db.set_dispatch_wait_reason(&current.id, "pool_exhausted").unwrap();
    clear_wait_since(&db, &current.id);
    assert_eq!(
        display(&db, &item, NOW).text,
        "Queued — worker pool full",
        "a same-kind predecessor that finished days earlier is not a restart"
    );
}

#[test]
fn queued_variants_and_wait_age_are_engine_selected() {
    let (db, _, item) = setup_product_and_chore();
    let execution = pending(&db, &item);
    for (reason, expected) in [
        ("unknown", "reason unknown"),
        ("not_before", "reason unknown"),
        ("waiting_dependency", "waiting for a dependency"),
        ("dispatch_paused", "dispatch paused"),
        ("automation_paused", "automation paused"),
        ("pool_exhausted", "worker pool full"),
        ("blocked by an earlier review", "blocked by an earlier review"),
    ] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET dispatch_wait_reason = ?2, dispatch_wait_since = ?3 WHERE id = ?1",
                params![execution.id, reason, (NOW - 60).to_string()],
            )
            .unwrap();
        let result = display(&db, &item, NOW);
        assert_eq!(result.text, format!("Queued — {expected} (1 minute ago)"));
        assert_eq!(
            result.tooltip_times,
            vec![QueuedTooltipTime {
                label: "Waiting since:".into(),
                epoch: NOW - 60,
            }]
        );
    }
}
