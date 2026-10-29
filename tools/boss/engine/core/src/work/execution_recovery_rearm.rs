//! One-shot re-arm of work items wedged by a recovery failure that dispatch
//! can now repair. The coordinator treats a pointer-integrity failure as
//! permanent and blocks the item with `execution_recovery_failed` and
//! `autostart = 0`, and every later execution recovers from the same
//! predecessor and fails identically. Once the engine learns to repair a
//! class of failure, the items it wedged can run again, but nothing would
//! ever restart them: this runs once per schema step, with the recognizer
//! for exactly the blocker texts that step made repairable.
//!
//! Schema 39 re-armed the false "not descended" failure (the ancestry check
//! compared change ids, so a divergent change made a pointer that really
//! descends from its baseline look unrelated). Schema 43 re-arms the three
//! failures dispatch now reconciles against engine-recorded pointers: a
//! stranded baseline, diverged heads, and a missing baseline bookmark.
//! Schema 59 re-arms missing or conflicted disposable PR aliases, which
//! dispatch now reconstructs from the authoritative GitHub head.

use boss_engine_recovery::execution_bookmark::NOT_DESCENDED_MESSAGE;

use super::*;

/// The schema-39 recognizer: exactly the not-descended message followed by
/// the engine-created baseline `boss-base/exec_<id>`.
pub(super) fn is_false_not_descended_blocker(detail: &str) -> bool {
    detail
        .strip_prefix(NOT_DESCENDED_MESSAGE)
        .and_then(|rest| rest.strip_prefix(" boss-base/exec_"))
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'))
}

/// Items whose recorded blocker `recognizes` as repairable are moved back to
/// `todo` with autostart on. Failures outside this migration's recognizer
/// stay blocked: this step makes no claim that dispatch can repair them.
/// Rearmed items still run the integrity check, so a pointer that
/// truly cannot be vouched for blocks again, loudly, with the new precise
/// text that this recognizer does not match.
pub(super) fn migrate(conn: &Connection, recognizes: fn(&str) -> bool) -> Result<usize> {
    let candidates: Vec<(String, String)> = {
        let mut statement = conn.prepare(
            "SELECT id, blocked_detail FROM tasks
             WHERE status = 'blocked'
               AND blocked_reason = 'execution_recovery_failed'
               AND blocked_detail IS NOT NULL
               AND deleted_at IS NULL",
        )?;
        statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let ids: Vec<String> = candidates
        .into_iter()
        .filter(|(_, detail)| recognizes(detail))
        .map(|(id, _)| id)
        .collect();
    for id in &ids {
        conn.execute(
            "UPDATE tasks
             SET status = 'todo', blocked_reason = NULL, blocked_detail = NULL, autostart = 1,
                 last_status_actor = 'engine', updated_at = ?2
             WHERE id = ?1",
            params![id, now_string()],
        )?;
    }
    let rearmed = ids.len();
    for id in ids {
        let Some(previous) = query_latest_execution_for_work_item(conn, &id)? else {
            continue;
        };
        if !previous.status.is_terminal() {
            continue;
        }
        // The reconciler only looks at the latest execution, so a never-started
        // pending attempt shadowed by the terminal one would never be promoted.
        // Retire it; the replacement below becomes the single live execution.
        // A started non-terminal row is a live worker: leave the item alone.
        let has_started_live: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_executions WHERE work_item_id = ?1
             AND status NOT IN ('completed', 'failed', 'cancelled', 'orphaned', 'abandoned')
             AND started_at IS NOT NULL)",
            [&id],
            |row| row.get(0),
        )?;
        if has_started_live {
            continue;
        }
        conn.execute(
            "UPDATE work_executions SET status = 'cancelled', finished_at = ?2
             WHERE work_item_id = ?1
               AND status NOT IN ('completed', 'failed', 'cancelled', 'orphaned', 'abandoned')",
            params![id, now_string()],
        )?;
        let kind = execution_kind_for_work_item(conn, &id)?;
        let pr_url = if kind == ExecutionKind::RevisionImplementation {
            get_chain_root_task(conn, &id)?.and_then(|root| root.pr_url)
        } else {
            None
        };
        // Preserve the terminal attempt and its bookmarks as recovery
        // provenance. The normal reconciler applies dependency admission
        // and project ordering before promoting this replacement to ready.
        insert_execution(
            conn,
            CreateExecutionInput::builder()
                .work_item_id(id)
                .kind(kind)
                .status(ExecutionStatus::WaitingDependency)
                .maybe_pr_url(pr_url)
                .build(),
        )?;
    }
    if rearmed > 0 {
        tracing::info!(
            rearmed,
            "re-armed work items blocked by a now-repairable recovery failure"
        );
    }
    Ok(rearmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_engine_recovery::execution_bookmark::{
        DIVERGED_MESSAGE, UNRESOLVED_MESSAGE, is_legacy_reconcilable_blocker,
    };

    /// Undo every schema step after 38 so reopening replays them.
    const RESET_TO_38: &str =
        "DROP INDEX work_executions_in_flight_idx; ALTER TABLE ci_remediations DROP COLUMN requeue_enqueued_at;
        ALTER TABLE ci_remediations DROP COLUMN queue_base_sha;
        ALTER TABLE execution_bookmarks DROP COLUMN base_commit;
        ALTER TABLE execution_bookmarks DROP COLUMN reconciliation_note;
        ALTER TABLE execution_bookmarks DROP COLUMN retired_at;
        ALTER TABLE execution_bookmarks DROP COLUMN retired_reason;
        ALTER TABLE tasks DROP COLUMN pr_updated_at;
        DROP TABLE schema_migrations;
        INSERT OR REPLACE INTO metadata VALUES ('schema_version', '38');";

    /// Undo later schema steps so reopening replays the schema 43 re-arm.
    const RESET_TO_42: &str = "DROP INDEX work_executions_in_flight_idx;
        ALTER TABLE execution_bookmarks DROP COLUMN base_commit;
        ALTER TABLE execution_bookmarks DROP COLUMN reconciliation_note;
        ALTER TABLE execution_bookmarks DROP COLUMN retired_at;
        ALTER TABLE execution_bookmarks DROP COLUMN retired_reason;
        ALTER TABLE tasks DROP COLUMN pr_updated_at;
        DROP TABLE schema_migrations;
        INSERT OR REPLACE INTO metadata VALUES ('schema_version', '42');";

    /// Create a chore under a fresh product and park it the way the
    /// coordinator does after a pointer-integrity failure.
    fn insert_blocked(db: &WorkDb, product_id: &str, name: &str, reason: &str, detail: &str) -> String {
        let id = crate::test_support::create_test_chore_manual(db, product_id, name).id;
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'blocked', blocked_reason = ?2, blocked_detail = ?3, autostart = 0 WHERE id = ?1",
                params![id, reason, detail],
            )
            .unwrap();
        id
    }

    fn product(db: &WorkDb) -> String {
        crate::test_support::create_test_product_with_repo(db, "rearm", Some("git@example.invalid:foo/bar.git")).id
    }

    fn state(db: &WorkDb, id: &str) -> (String, Option<String>, Option<String>, i64) {
        db.connect()
            .unwrap()
            .query_row(
                "SELECT status, blocked_reason, blocked_detail, autostart FROM tasks WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap()
    }

    #[test]
    fn rearms_only_items_blocked_by_the_not_descended_message() {
        let db = WorkDb::open_in_memory().unwrap();
        let product = product(&db);
        let stale = format!("{NOT_DESCENDED_MESSAGE} boss-base/exec_18dd050856e97880_290");
        let wedged = insert_blocked(&db, &product, "wedged", "execution_recovery_failed", &stale);
        let others = [
            insert_blocked(
                &db,
                &product,
                "missing pointers",
                "execution_recovery_failed",
                "expected recovery pointer boss-recovery/exec_1 or boss/exec_1 to resolve to exactly one change; both are missing",
            ),
            insert_blocked(
                &db,
                &product,
                "trailing text",
                "execution_recovery_failed",
                &format!("{stale}: something else went wrong"),
            ),
            insert_blocked(&db, &product, "other reason", "worker_failed", &stale),
        ];

        let conn = db.connect().unwrap();
        assert_eq!(migrate(&conn, is_false_not_descended_blocker).unwrap(), 1);
        assert_eq!(
            migrate(&conn, is_false_not_descended_blocker).unwrap(),
            0,
            "a second run is a no-op"
        );
        drop(conn);

        assert_eq!(state(&db, &wedged), ("todo".to_owned(), None, None, 1));
        for id in &others {
            let (status, reason, detail, autostart) = state(&db, id);
            assert_eq!(status, "blocked", "{id}");
            assert!(reason.is_some() && detail.is_some(), "{id}");
            assert_eq!(autostart, 0, "{id}");
        }
    }

    #[test]
    fn migration_and_reconcile_restart_terminal_implementations_once() {
        for kind in [
            ExecutionKind::ChoreImplementation,
            ExecutionKind::TaskImplementation,
            ExecutionKind::RevisionImplementation,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("work.db");
            let pr_url = "https://github.com/example/repo/pull/42";
            let (product_id, id, failed_id, prerequisite) = {
                let db = WorkDb::open(path.clone()).unwrap();
                let product_id = product(&db);
                let stale = format!("{NOT_DESCENDED_MESSAGE} boss-base/exec_failed");
                let id = insert_blocked(&db, &product_id, "wedged", "execution_recovery_failed", &stale);
                let prerequisite = crate::test_support::create_test_chore_manual(&db, &product_id, "prerequisite").id;
                db.connect().unwrap().execute(
                    "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at) VALUES (?1, ?2, 'blocks', '1')",
                    params![id, prerequisite],
                ).unwrap();
                if kind == ExecutionKind::TaskImplementation {
                    let project = db
                        .create_project(CreateProjectInput {
                            product_id: product_id.clone(),
                            name: "recovery project".to_owned(),
                            description: None,
                            goal: None,
                            autostart: true,
                            no_design_task: true,
                            design_reasoning_effort_xhigh: false,
                        })
                        .unwrap();
                    db.connect()
                        .unwrap()
                        .execute(
                            "UPDATE tasks SET kind = 'project_task', project_id = ?2, ordinal = 1 WHERE id = ?1",
                            params![id, project.id],
                        )
                        .unwrap();
                }
                if kind == ExecutionKind::RevisionImplementation {
                    let root = crate::test_support::create_test_chore_manual(&db, &product_id, "root");
                    let conn = db.connect().unwrap();
                    conn.execute(
                        "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
                        params![root.id, pr_url],
                    )
                    .unwrap();
                    conn.execute(
                        "UPDATE tasks SET kind = 'revision', parent_task_id = ?2 WHERE id = ?1",
                        params![id, root.id],
                    )
                    .unwrap();
                }
                let failed = db
                    .create_execution(
                        CreateExecutionInput::builder()
                            .work_item_id(id.clone())
                            .kind(kind.clone())
                            .status(ExecutionStatus::Failed)
                            .started_at("1")
                            .finished_at("2")
                            .build(),
                    )
                    .unwrap();
                db.connect().unwrap().execute_batch(RESET_TO_38).unwrap();
                (product_id, id, failed.id, prerequisite)
            };
            let db = WorkDb::open(path.clone()).unwrap();
            let queued = query_latest_execution_for_work_item(&db.connect().unwrap(), &id)
                .unwrap()
                .unwrap();
            assert_eq!(queued.status, ExecutionStatus::WaitingDependency);
            assert_ne!(queued.id, failed_id);
            assert_eq!(db.recovery_predecessor(&queued).unwrap().unwrap().id, failed_id);
            assert_eq!(queued.kind, kind);
            if kind == ExecutionKind::RevisionImplementation {
                assert_eq!(queued.pr_url.as_deref(), Some(pr_url));
            }
            for _ in 0..2 {
                db.reconcile_product_executions(&product_id).unwrap();
            }
            assert_eq!(
                query_latest_execution_for_work_item(&db.connect().unwrap(), &id)
                    .unwrap()
                    .unwrap()
                    .status,
                ExecutionStatus::WaitingDependency
            );
            db.connect()
                .unwrap()
                .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&prerequisite])
                .unwrap();
            drop(db);
            let db = WorkDb::open(path).unwrap();
            db.reconcile_product_executions(&product_id).unwrap();
            let conn = db.connect().unwrap();
            let fresh = query_latest_execution_for_work_item(&conn, &id).unwrap().unwrap();
            assert_eq!(fresh.id, queued.id);
            assert_eq!(fresh.status, ExecutionStatus::Ready);
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM work_executions WHERE work_item_id = ?1",
                    [&id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 2, "one failed attempt and exactly one replacement");
        }
    }

    #[test]
    fn rearm_replaces_an_older_pending_execution_shadowed_by_a_terminal_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.db");
        let (product_id, id, pending_id, failed_id, prerequisite) = {
            let db = WorkDb::open(path.clone()).unwrap();
            let product_id = product(&db);
            let stale = format!("{NOT_DESCENDED_MESSAGE} boss-base/exec_failed");
            let id = insert_blocked(&db, &product_id, "wedged", "execution_recovery_failed", &stale);
            let prerequisite = crate::test_support::create_test_chore_manual(&db, &product_id, "prerequisite").id;
            db.connect()
                .unwrap()
                .execute(
                    "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at) VALUES (?1, ?2, 'blocks', '1')",
                    params![id, prerequisite],
                )
                .unwrap();
            let pending = db
                .create_execution(
                    CreateExecutionInput::builder()
                        .work_item_id(id.clone())
                        .kind(ExecutionKind::ChoreImplementation)
                        .status(ExecutionStatus::WaitingDependency)
                        .build(),
                )
                .unwrap();
            let failed = db
                .create_execution(
                    CreateExecutionInput::builder()
                        .work_item_id(id.clone())
                        .kind(ExecutionKind::ChoreImplementation)
                        .status(ExecutionStatus::Failed)
                        .build(),
                )
                .unwrap();
            db.connect().unwrap().execute_batch(RESET_TO_38).unwrap();
            (product_id, id, pending.id, failed.id, prerequisite)
        };
        let db = WorkDb::open(path).unwrap();
        let live = |db: &WorkDb| -> Vec<(String, ExecutionStatus)> {
            let conn = db.connect().unwrap();
            let mut statement = conn
                .prepare(
                    "SELECT id FROM work_executions WHERE work_item_id = ?1
                     AND status NOT IN ('completed', 'failed', 'cancelled', 'orphaned', 'abandoned')",
                )
                .unwrap();
            let ids = statement
                .query_map([&id], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            ids.into_iter()
                .map(|exec_id| {
                    let status = query_execution(&conn, &exec_id).unwrap().unwrap().status;
                    (exec_id, status)
                })
                .collect()
        };
        let conn = db.connect().unwrap();
        let replacement = query_latest_execution_for_work_item(&conn, &id).unwrap().unwrap();
        assert_ne!(replacement.id, failed_id);
        assert_ne!(replacement.id, pending_id);
        assert_eq!(
            query_execution(&conn, &pending_id).unwrap().unwrap().status,
            ExecutionStatus::Cancelled
        );
        drop(conn);

        for _ in 0..2 {
            db.reconcile_product_executions(&product_id).unwrap();
        }
        assert_eq!(
            live(&db),
            vec![(replacement.id.clone(), ExecutionStatus::WaitingDependency)],
            "stays waiting while the prerequisite is unmet"
        );

        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&prerequisite])
            .unwrap();
        db.reconcile_product_executions(&product_id).unwrap();
        assert_eq!(live(&db), vec![(replacement.id, ExecutionStatus::Ready)]);
    }

    #[test]
    fn opening_a_pre_migration_database_rearms_wedged_items_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.db");
        let stale = format!("{NOT_DESCENDED_MESSAGE} boss-base/exec_18dd050856e97880_290");
        let (product_id, wedged) = {
            let db = WorkDb::open(path.clone()).unwrap();
            let product_id = product(&db);
            let wedged = insert_blocked(&db, &product_id, "wedged", "execution_recovery_failed", &stale);
            db.connect().unwrap().execute_batch(RESET_TO_38).unwrap();
            (product_id, wedged)
        };
        let db = WorkDb::open(path.clone()).unwrap();
        assert_eq!(state(&db, &wedged), ("todo".to_owned(), None, None, 1));

        // The migration is stamped done: a later block with the same text (a
        // pointer that genuinely does not descend) stays blocked across restarts.
        let real = insert_blocked(&db, &product_id, "genuine", "execution_recovery_failed", &stale);
        drop(db);
        let db = WorkDb::open(path).unwrap();
        assert_eq!(state(&db, &real).0, "blocked");
    }

    #[test]
    fn schema_43_rearms_every_now_reconcilable_shape_and_nothing_else() {
        let db = WorkDb::open_in_memory().unwrap();
        let product = product(&db);
        let id = "exec_18dd8e49b9602c50_16a";
        let wedged = [
            insert_blocked(
                &db,
                &product,
                "stranded baseline",
                "execution_recovery_failed",
                &format!("{NOT_DESCENDED_MESSAGE} boss-base/{id}"),
            ),
            insert_blocked(
                &db,
                &product,
                "diverged pointers",
                "execution_recovery_failed",
                &format!("preserved pointers boss-recovery/{id} and boss/{id} {DIVERGED_MESSAGE}"),
            ),
            insert_blocked(
                &db,
                &product,
                "missing baseline",
                "execution_recovery_failed",
                &format!("recovery bookmark boss-base/{id} {UNRESOLVED_MESSAGE}; found 0"),
            ),
        ];
        let others = [
            insert_blocked(
                &db,
                &product,
                "missing heads",
                "execution_recovery_failed",
                "expected recovery pointer boss-recovery/exec_1 or boss/exec_1 to resolve to exactly one change; both are missing",
            ),
            insert_blocked(
                &db,
                &product,
                "conflicted baseline",
                "execution_recovery_failed",
                &format!("recovery bookmark boss-base/{id} {UNRESOLVED_MESSAGE}; found 2"),
            ),
            insert_blocked(
                &db,
                &product,
                "precise not-contained report",
                "execution_recovery_failed",
                &format!("preserved work of execution {id} is not contained in PR head pr/1 (abc): …"),
            ),
            insert_blocked(
                &db,
                &product,
                "other reason",
                "worker_failed",
                &format!("{NOT_DESCENDED_MESSAGE} boss-base/{id}"),
            ),
        ];
        let conn = db.connect().unwrap();
        assert_eq!(migrate(&conn, is_legacy_reconcilable_blocker).unwrap(), 3);
        assert_eq!(
            migrate(&conn, is_legacy_reconcilable_blocker).unwrap(),
            0,
            "a second run is a no-op"
        );
        drop(conn);
        for id in &wedged {
            assert_eq!(state(&db, id), ("todo".to_owned(), None, None, 1), "{id}");
        }
        for id in &others {
            let (status, reason, detail, autostart) = state(&db, id);
            assert_eq!(status, "blocked", "{id}");
            assert!(reason.is_some() && detail.is_some(), "{id}");
            assert_eq!(autostart, 0, "{id}");
        }
    }

    #[test]
    fn opening_a_schema_40_database_rearms_baseline_and_divergence_wedges_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.db");
        let id = "exec_18dd8e49b9602c50_16a";
        let missing = format!("recovery bookmark boss-base/{id} {UNRESOLVED_MESSAGE}; found 0");
        let (product_id, wedged, diverged) = {
            let db = WorkDb::open(path.clone()).unwrap();
            let product_id = product(&db);
            let wedged = insert_blocked(
                &db,
                &product_id,
                "missing baseline",
                "execution_recovery_failed",
                &missing,
            );
            let diverged = insert_blocked(
                &db,
                &product_id,
                "diverged",
                "execution_recovery_failed",
                &format!("preserved pointers boss-recovery/{id} and boss/{id} {DIVERGED_MESSAGE}"),
            );
            db.connect().unwrap().execute_batch(RESET_TO_42).unwrap();
            (product_id, wedged, diverged)
        };
        let db = WorkDb::open(path.clone()).unwrap();
        assert_eq!(state(&db, &wedged), ("todo".to_owned(), None, None, 1));
        assert_eq!(state(&db, &diverged), ("todo".to_owned(), None, None, 1));
        // The columns the same step adds are present afterwards.
        db.connect()
            .unwrap()
            .execute_batch(
                "SELECT base_commit, reconciliation_note, retired_at, retired_reason FROM execution_bookmarks",
            )
            .unwrap();
        // Stamped done: a later block with the same text stays blocked.
        let real = insert_blocked(&db, &product_id, "genuine", "execution_recovery_failed", &missing);
        drop(db);
        let db = WorkDb::open(path).unwrap();
        assert_eq!(state(&db, &real).0, "blocked");
    }
}
