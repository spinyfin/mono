use super::*;
use boss_protocol::QueuedDisplay;

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
    assert_eq!(future.text, "Queued — not before in 2 min");
    assert!(
        future
            .tooltip
            .contains(&boss_engine_utils::iso8601::format_epoch_iso8601(NOW + 120))
    );
    let before = db.queued_displays(NOW).unwrap();
    for now in [NOW + 120, NOW + 121] {
        let elapsed = display(&db, &item, now);
        assert_eq!(elapsed.text, "Queued — Waiting for an available worker (11 working)");
        assert!(!elapsed.tooltip.contains("not before"));
        let after = db.queued_displays(now).unwrap();
        assert_ne!(before, after, "heartbeat detects deadline expiry without a DB write");
        assert_eq!(after[0].2, elapsed);
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
        // Fix the wait stamp so this test has no wall-clock dependency.
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET dispatch_wait_since = NULL WHERE id = ?1",
                [&current.id],
            )
            .unwrap();
        let result = display(&db, &item, NOW);
        if status == "completed" {
            assert_eq!(result.text, "Queued — worker pool full");
        } else {
            assert_eq!(
                result.text,
                format!("Restarting after the previous run {verb} 1 minute ago. Queued — worker pool full")
            );
            assert!(
                result
                    .tooltip
                    .contains(&boss_engine_utils::iso8601::format_epoch_iso8601(NOW - 60))
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
    assert!(db.queued_displays(NOW).unwrap().is_empty());
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
        assert_eq!(
            display(&db, &item, NOW).text,
            format!("Queued — {expected} (1 minute ago)")
        );
    }
}
