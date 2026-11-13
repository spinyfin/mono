use super::*;
use std::time::Duration;

#[test]
fn repeated_start_failures_keep_each_message_and_occurrence_time() {
    let db = db();
    let product = create_test_product(&db);
    let (item, execution) = ready_chore(&db, &product.id, "repeated starts");
    let delays = [Duration::from_secs(3600); 4];
    for (index, error) in [
        "lease unavailable",
        "spawn failed",
        "lease unavailable",
        "lease unavailable",
    ]
    .into_iter()
    .enumerate()
    {
        db.record_pre_start_failure(&execution.id, "worker", None, error, &delays)
            .unwrap();
        let conn = db.connect().unwrap();
        let recorded: String = conn
            .query_row(
                "SELECT happened_at FROM dispatch_start_failures WHERE execution_id = ?1 ORDER BY rowid DESC LIMIT 1",
                [&execution.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(recorded.parse::<i64>().unwrap() >= execution.created_at.parse::<i64>().unwrap());
        // Distinct timestamps prove projection uses the event, not execution creation.
        conn.execute("UPDATE dispatch_start_failures SET happened_at = ?2 WHERE rowid = (SELECT MAX(rowid) FROM dispatch_start_failures)",
            params![execution.id, (2000 + index).to_string()]).unwrap();
    }
    let wait = wait_of(&db, &product.id, &item.id);
    assert_eq!(
        wait.dispatch_failures
            .iter()
            .map(|f| f.error_text.as_str())
            .collect::<Vec<_>>(),
        [
            "lease unavailable",
            "spawn failed",
            "lease unavailable",
            "lease unavailable"
        ]
    );
    assert_eq!(
        wait.dispatch_failures
            .iter()
            .map(|f| f.happened_at.as_str())
            .collect::<Vec<_>>(),
        ["2000", "2001", "2002", "2003"]
    );
}

#[test]
fn resume_after_spawn_return_records_stall_reason_and_failure_time() {
    let db = db();
    let product = create_test_product(&db);
    let (item, execution) = ready_chore(&db, &product.id, "spawn return");
    let (_, run) = db
        .start_execution_run(&execution.id, "worker", "repo", "lease", "ws", "/tmp/spawn-return")
        .unwrap();
    db.connect().unwrap().execute(
        "UPDATE work_runs SET status = 'completed', result_summary = 'Spawned tmux worker in slot 7', finished_at = '1000' WHERE id = ?1",
        [&run.id],
    ).unwrap();
    db.request_resume_execution(
        &execution.id,
        1,
        boss_engine_utils::epoch_time::now_epoch_secs() + 3600,
        "driver stalled awaiting model output",
    )
    .unwrap()
    .unwrap();
    let wait = wait_of(&db, &product.id, &item.id);
    assert_eq!(wait.dispatch_failures.len(), 1);
    assert_eq!(
        wait.dispatch_failures[0].error_text,
        "driver stalled awaiting model output"
    );
    assert_ne!(wait.dispatch_failures[0].happened_at, "1000");
}

#[test]
fn parks_after_either_retry_source_remain_host_lease_waits() {
    for transient in [false, true] {
        let db = db();
        let product = create_test_product(&db);
        let (item, execution) = ready_chore(&db, &product.id, "park after error");
        let execution = if transient {
            db.request_resume_execution(
                &execution.id,
                1,
                boss_engine_utils::epoch_time::now_epoch_secs() + 3600,
                "worker vanished before starting",
            )
            .unwrap()
            .unwrap()
        } else {
            db.record_pre_start_failure(
                &execution.id,
                "worker",
                None,
                "lease failed",
                &[Duration::from_secs(3600)],
            )
            .unwrap()
            .0
        };
        db.park_pre_start_attempt(
            &execution.id,
            Duration::from_secs(3600),
            "spoke lease unresolved; parked",
        )
        .unwrap();
        let wait = wait_of(&db, &product.id, &item.id);
        assert_eq!(
            wait.dispatch_wait_reason.as_deref(),
            Some(DISPATCH_WAIT_REASON_HOST_LEASE_PARK)
        );
        assert!(wait.dispatch_failures.is_empty());
    }
}

#[test]
fn retry_sequence_excludes_old_cycles_siblings_and_duplicate_attention_evidence() {
    let db = db();
    let product = create_test_product(&db);
    let (item, old) = ready_chore(&db, &product.id, "bounded history");
    db.mark_execution_orphaned(&old.id, "old cycle failure").unwrap();
    let success = pending(
        &db,
        &item.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Completed,
    );
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET finished_at = '1500' WHERE id = ?1",
            [&success.id],
        )
        .unwrap();
    let sibling = pending(&db, &item.id, ExecutionKind::PrReview, ExecutionStatus::Ready);
    db.mark_execution_orphaned(&sibling.id, "review failure").unwrap();
    let current = pending(
        &db,
        &item.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Ready,
    );
    let (_, run) = db
        .start_execution_run(&current.id, "worker", "repo", "lease", "ws", "/tmp/bounded-history")
        .unwrap();
    db.mark_execution_orphaned(&current.id, "current run failed").unwrap();
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO work_attention_items(id, execution_id, kind, status, title, body_markdown, created_at)
         VALUES ('history_attention', ?1, 'hung_worker', 'resolved', 'Hung worker', 'extra evidence', '2000')",
            [&current.id],
        )
        .unwrap();
    db.request_resume_execution(
        &current.id,
        1,
        boss_engine_utils::epoch_time::now_epoch_secs() + 3600,
        "resume",
    )
    .unwrap()
    .unwrap();
    let wait = wait_of(&db, &product.id, &item.id);
    assert_eq!(wait.dispatch_failures.len(), 1);
    assert_eq!(wait.dispatch_failures[0].error_text, "current run failed");
    // An error stamp on an existing run must become visible on the next read.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET error_text = 'updated run error' WHERE id = ?1",
            [&run.id],
        )
        .unwrap();
    assert_eq!(
        wait_of(&db, &product.id, &item.id).dispatch_failures[0].error_text,
        "updated run error"
    );
}

#[test]
fn repeated_resumes_keep_distinct_attempts_without_borrowing_attention() {
    let db = db();
    let product = create_test_product(&db);
    let (item, mut execution) = ready_chore(&db, &product.id, "repeated resumes");
    for (index, error) in ["pane absent", "driver stalled", "pane absent"].into_iter().enumerate() {
        execution = db
            .request_resume_execution(
                &execution.id,
                index as i64 + 1,
                boss_engine_utils::epoch_time::now_epoch_secs() + 3600,
                error,
            )
            .unwrap()
            .unwrap();
    }
    let wait = wait_of(&db, &product.id, &item.id);
    assert_eq!(
        wait.dispatch_failures
            .iter()
            .map(|f| f.error_text.as_str())
            .collect::<Vec<_>>(),
        ["pane absent", "driver stalled", "pane absent"]
    );
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO work_attention_items(id, execution_id, kind, status, title, body_markdown, created_at)
        VALUES ('unrelated_history', ?1, 'hung_worker', 'open', 'Hung worker', 'belongs to another execution', '3000')",
        [&execution.id],
    )
    .unwrap();
    drop(conn);
    assert_eq!(
        wait_of(&db, &product.id, &item.id).dispatch_failures,
        wait.dispatch_failures
    );
}

#[test]
fn permanent_start_failure_adds_one_attempt_after_retry_events() {
    let db = db();
    let product = create_test_product(&db);
    let (item, execution) = ready_chore(&db, &product.id, "exhausted retries");
    for _ in 0..2 {
        db.record_pre_start_failure(
            &execution.id,
            "worker",
            None,
            "lease failed",
            &[Duration::from_secs(3600)],
        )
        .unwrap();
    }
    // A later recovery shows the terminal failed start as well as the earlier retry.
    let next = pending(
        &db,
        &item.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Ready,
    );
    db.record_pre_start_failure(&next.id, "worker", None, "spawn failed", &[Duration::from_secs(3600)])
        .unwrap();
    assert_eq!(
        wait_of(&db, &product.id, &item.id)
            .dispatch_failures
            .iter()
            .map(|f| f.error_text.as_str())
            .collect::<Vec<_>>(),
        ["lease failed", "lease failed", "spawn failed"]
    );
}

#[test]
fn upgrade_classifies_existing_deadlines_and_installs_attempt_storage() {
    let (_dir, path) = disk_db_path("dispatch-failure-upgrade");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let (item, execution) = ready_chore(&db, &product.id, "legacy park");
    let (retry_item, retry) = ready_chore(&db, &product.id, "legacy retry");
    db.record_pre_start_failure(
        &retry.id,
        "worker",
        None,
        "preserved legacy failure",
        &[Duration::from_secs(3600)],
    )
    .unwrap();
    db.record_pre_start_failure(
        &execution.id,
        "worker",
        None,
        "lease failed",
        &[Duration::from_secs(3600)],
    )
    .unwrap();
    db.park_pre_start_attempt(
        &execution.id,
        Duration::from_secs(3600),
        "spoke lease unresolved; parked for 3600s",
    )
    .unwrap();
    let conn = db.connect().unwrap();
    conn.execute_batch(
        "DROP TABLE dispatch_start_failures;
        ALTER TABLE work_executions DROP COLUMN dispatch_not_before_source;
        DELETE FROM schema_migrations WHERE id = '20261113_0633_dispatch_failure_history';",
    )
    .unwrap();
    drop(conn);
    drop(db);
    let upgraded = WorkDb::open(path).unwrap();
    let legacy = wait_of(&upgraded, &product.id, &retry_item.id);
    assert_eq!(legacy.dispatch_failures.len(), 1);
    assert_eq!(legacy.dispatch_failures[0].error_text, "preserved legacy failure");
    assert!(
        legacy.dispatch_failures[0].happened_at.parse::<i64>().unwrap() >= retry.created_at.parse::<i64>().unwrap()
    );
    let wait = wait_of(&upgraded, &product.id, &item.id);
    assert_eq!(
        wait.dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_HOST_LEASE_PARK)
    );
    upgraded
        .record_pre_start_failure(
            &execution.id,
            "worker",
            None,
            "new failure",
            &[Duration::from_secs(3600); 2],
        )
        .unwrap();
    assert_eq!(
        wait_of(&upgraded, &product.id, &item.id).dispatch_failures[0].error_text,
        "new failure"
    );
}

#[test]
fn adopting_a_successor_preserves_the_source_of_a_later_host_deadline() {
    let db = db();
    let product = create_test_product(&db);
    let (item, dead) = ready_chore(&db, &product.id, "adopt parked successor");
    db.mark_execution_orphaned(&dead.id, "pane disappeared").unwrap();
    let successor = pending(
        &db,
        &item.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Ready,
    );
    db.park_pre_start_attempt(
        &successor.id,
        Duration::from_secs(7200),
        "spoke lease unresolved; parked",
    )
    .unwrap();
    db.request_resume_execution(
        &dead.id,
        2,
        boss_engine_utils::epoch_time::now_epoch_secs() + 3600,
        "resume missing pane",
    )
    .unwrap()
    .unwrap();
    let wait = wait_of(&db, &product.id, &item.id);
    assert_eq!(
        wait.dispatch_wait_reason.as_deref(),
        Some(DISPATCH_WAIT_REASON_HOST_LEASE_PARK)
    );
    assert!(wait.dispatch_failures.is_empty());
}

#[test]
fn failed_parked_pane_retains_its_error_after_spawn_return() {
    let db = db();
    let product = create_test_product(&db);
    let (item, execution) = ready_chore(&db, &product.id, "failed parked pane");
    let (_, run) = db
        .start_execution_run(&execution.id, "worker", "repo", "lease", "ws", "/tmp/failed-pane")
        .unwrap();
    db.connect().unwrap().execute("UPDATE work_runs SET status = 'completed', finished_at = '1000', result_summary = 'Spawned tmux worker' WHERE id = ?1", [&run.id]).unwrap();
    db.fail_pane_parked_execution(&execution.id, "worker driver exited with code 7")
        .unwrap()
        .unwrap();
    let retry = pending(
        &db,
        &item.id,
        ExecutionKind::ChoreImplementation,
        ExecutionStatus::Ready,
    );
    db.record_pre_start_failure(&retry.id, "worker", None, "lease failed", &[Duration::from_secs(3600)])
        .unwrap();
    assert_eq!(
        wait_of(&db, &product.id, &item.id)
            .dispatch_failures
            .iter()
            .map(|f| f.error_text.as_str())
            .collect::<Vec<_>>(),
        ["worker driver exited with code 7", "lease failed"]
    );
}
