//! One-shot re-arm of work items wedged by the false "not descended" recovery
//! failure. The ancestry check compared change ids, so a divergent change made
//! it report a pointer that really descends from its baseline as unrelated;
//! the coordinator treats that as permanent and blocks the item with
//! `execution_recovery_failed` and `autostart = 0`, and every later execution
//! recovers from the same predecessor and fails identically. With the check
//! fixed those items can run again, but nothing would ever restart them.

use boss_engine_recovery::execution_bookmark::NOT_DESCENDED_MESSAGE;

use super::*;

/// Items whose recorded blocker is exactly the not-descended failure: the
/// message, then the engine-created baseline `boss-base/exec_<id>`. Any other
/// recovery failure (missing or conflicted pointers, a foreign host) is a real
/// pointer problem that re-running cannot repair, so it is left blocked.
/// Rearmed items still run the (now correct) check, so a pointer that truly
/// does not descend blocks again, loudly.
pub(super) fn migrate(conn: &Connection) -> Result<usize> {
    let prefix = format!("{NOT_DESCENDED_MESSAGE} boss-base/exec_");
    let mut statement = conn.prepare(
        "UPDATE tasks
         SET status = 'todo', blocked_reason = NULL, blocked_detail = NULL, autostart = 1,
             last_status_actor = 'engine', updated_at = ?2
         WHERE status = 'blocked'
           AND blocked_reason = 'execution_recovery_failed'
           AND deleted_at IS NULL
           AND substr(blocked_detail, 1, length(?1)) = ?1
           AND substr(blocked_detail, length(?1) + 1) NOT GLOB '*[^A-Za-z0-9_]*'
         RETURNING id",
    )?;
    let ids = statement
        .query_map(params![prefix, now_string()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let rearmed = ids.len();
    for id in ids {
        let Some(previous) = query_latest_execution_for_work_item(conn, &id)? else {
            continue;
        };
        let has_pending: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_executions WHERE work_item_id = ?1
             AND status NOT IN ('completed', 'failed', 'cancelled', 'orphaned', 'abandoned'))",
            [&id],
            |row| row.get(0),
        )?;
        if !previous.status.is_terminal() || has_pending {
            continue;
        }
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
            "re-armed work items blocked by the false recovery ancestry failure"
        );
    }
    Ok(rearmed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(migrate(&conn).unwrap(), 1);
        assert_eq!(migrate(&conn).unwrap(), 0, "a second run is a no-op");
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
    fn migration_and_reconcile_restart_terminal_chores_and_revisions_once() {
        for revision in [false, true] {
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
                if revision {
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
                            .kind(if revision {
                                ExecutionKind::RevisionImplementation
                            } else {
                                ExecutionKind::ChoreImplementation
                            })
                            .status(ExecutionStatus::Failed)
                            .started_at("1")
                            .finished_at("2")
                            .build(),
                    )
                    .unwrap();
                db.connect()
                    .unwrap()
                    .execute("UPDATE metadata SET value = '38' WHERE key = 'schema_version'", [])
                    .unwrap();
                (product_id, id, failed.id, prerequisite)
            };
            let db = WorkDb::open(path.clone()).unwrap();
            let queued = query_latest_execution_for_work_item(&db.connect().unwrap(), &id)
                .unwrap()
                .unwrap();
            assert_eq!(queued.status, ExecutionStatus::WaitingDependency);
            assert_ne!(queued.id, failed_id);
            assert_eq!(db.recovery_predecessor(&queued).unwrap().unwrap().id, failed_id);
            if revision {
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
    fn opening_a_pre_migration_database_rearms_wedged_items_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.db");
        let stale = format!("{NOT_DESCENDED_MESSAGE} boss-base/exec_18dd050856e97880_290");
        let (product_id, wedged) = {
            let db = WorkDb::open(path.clone()).unwrap();
            let product_id = product(&db);
            let wedged = insert_blocked(&db, &product_id, "wedged", "execution_recovery_failed", &stale);
            db.connect()
                .unwrap()
                .execute("UPDATE metadata SET value = '38' WHERE key = 'schema_version'", [])
                .unwrap();
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
}
