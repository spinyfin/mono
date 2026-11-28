use super::reconcile_history::*;
use super::*;

const AUDIT: &str = "[engine-reconcile] epoch 1700000000: dead worker (exec old-run) detected via PID probe; chore reset to todo for redispatch. Uncommitted work backed up to /tmp/old-run.patch.";

#[test]
fn upgrade_moves_all_history_and_preserves_authored_bytes_and_timestamps() {
    let (_dir, db) = test_support::open_db();
    let product = test_support::create_test_product_named(&db, "audit-upgrade");
    let chore = test_support::create_test_chore_manual(&db, &product.id, "brief");
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(&product.id)
                .name("project")
                .description("")
                .no_design_task(true)
                .build(),
        )
        .unwrap();
    let brief = "Authored brief.\r\n\nKeep whitespace.\n";
    let parking = "[parking] epoch 1700000001: refresh_pr: PR is still open.";
    let description = format!("{brief}{}\n{parking}", format!("\n{AUDIT}").repeat(45));
    assert!(description.len() > 4096);
    let path = db.path().to_path_buf();
    let conn = db.connect().unwrap();
    for (table, id) in [
        ("products", &product.id),
        ("projects", &project.id),
        ("tasks", &chore.id),
    ] {
        conn.execute(
            &format!("UPDATE {table} SET description = ?2, updated_at = '123' WHERE id = ?1"),
            params![id, description],
        )
        .unwrap();
    }
    conn.execute("DELETE FROM schema_migrations WHERE id = ?1", [MIGRATION_ID])
        .unwrap();
    drop(conn);
    drop(db);
    for _ in 0..2 {
        let db = WorkDb::open(path.clone()).unwrap();
        for (table, id) in [
            ("products", &product.id),
            ("projects", &project.id),
            ("tasks", &chore.id),
        ] {
            let conn = db.connect().unwrap();
            let (stored, updated): (String, String) = conn
                .query_row(
                    &format!("SELECT description, updated_at FROM {table} WHERE id = ?1"),
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(stored, brief);
            assert_eq!(updated, "123");
            drop(conn);
            let history: Vec<_> = db
                .list_task_field_audit(id)
                .unwrap()
                .into_iter()
                .filter(|row| row.property == RECONCILIATION_PROPERTY)
                .collect();
            assert_eq!(history.len(), 46, "reopening must not duplicate migrated history");
            for row in &history[..45] {
                assert_eq!(row.detail.as_deref(), Some(AUDIT));
                assert_eq!(row.changed_at, "1700000000");
                assert_eq!(row.basis, "legacy_description");
            }
            assert_eq!(history[45].detail.as_deref(), Some(parking));
        }
    }
}

#[test]
fn migration_keeps_quoted_examples_and_non_audit_text() {
    let (_dir, db) = test_support::open_db();
    let product = test_support::create_test_product_named(&db, "audit-examples");
    let brief = format!(
        "Explain this example:\n````text\n```\n{AUDIT}\n```\n````\n\n    {AUDIT}\n[engine-reconcile] epoch invalid: example.\nMention [engine-reconcile] in prose."
    );
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE products SET description = ?2 WHERE id = ?1",
        params![
            product.id,
            format!("{brief}\n{AUDIT}\nFinal authored instruction.\n{AUDIT}")
        ],
    )
    .unwrap();
    migrate(&conn).unwrap();
    drop(conn);
    let WorkItem::Product(item) = db.get_work_item(&product.id).unwrap() else {
        panic!("product")
    };
    assert_eq!(item.description, format!("{brief}\nFinal authored instruction."));
    assert_eq!(db.list_task_field_audit(&product.id).unwrap().len(), 2);
}

#[test]
fn failed_upgrade_rolls_back_history_and_description_together() {
    let (_dir, db) = test_support::open_db();
    let product = test_support::create_test_product_named(&db, "audit-rollback");
    let conn = db.connect().unwrap();
    let original = format!("Brief\n{AUDIT}");
    conn.execute(
        "UPDATE products SET description = ?2 WHERE id = ?1",
        params![product.id, original],
    )
    .unwrap();
    conn.execute("DELETE FROM schema_migrations WHERE id = ?1", [MIGRATION_ID])
        .unwrap();
    conn.execute_batch("CREATE TRIGGER reject_brief_cleanup BEFORE UPDATE OF description ON products BEGIN SELECT RAISE(ABORT, 'cleanup rejected'); END;").unwrap();
    let path = db.path().to_path_buf();
    drop(conn);
    drop(db);
    assert!(WorkDb::open(path.clone()).is_err());
    let conn = Connection::open(path).unwrap();
    let stored: String = conn
        .query_row("SELECT description FROM products WHERE id = ?1", [&product.id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored, original);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_status_audit WHERE property = ?1",
            [RECONCILIATION_PROPERTY],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn upgrade_ignores_four_space_indented_fence_markers_inside_fenced_blocks() {
    let (_dir, db) = test_support::open_db();
    let product = test_support::create_test_product_named(&db, "audit-indented-fence");
    let authored = "Brief\r\n```text\r\n    ```\r\n[engine-reconcile] epoch 1: example.\r\n```\r\n";
    let real_event = "[engine-reconcile] epoch 2: real event.";
    let full_description = format!("{authored}\n{real_event}");

    let path = db.path().to_path_buf();
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE products SET description = ?2 WHERE id = ?1",
        params![product.id, full_description],
    )
    .unwrap();
    conn.execute("DELETE FROM schema_migrations WHERE id = ?1", [MIGRATION_ID])
        .unwrap();
    drop(conn);
    drop(db);

    for _ in 0..2 {
        let db = WorkDb::open(path.clone()).unwrap();
        let WorkItem::Product(item) = db.get_work_item(&product.id).unwrap() else {
            panic!("product")
        };
        assert_eq!(item.description, authored);
        let history: Vec<_> = db
            .list_task_field_audit(&product.id)
            .unwrap()
            .into_iter()
            .filter(|row| row.property == RECONCILIATION_PROPERTY)
            .collect();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].detail.as_deref(), Some(real_event));
        assert_eq!(history[0].changed_at, "2");
        assert_eq!(history[0].basis, "legacy_description");
    }
}

#[test]
fn upgrade_rescans_unclosed_fence_so_trailing_records_still_move() {
    let (_dir, db) = test_support::open_db();
    let product = test_support::create_test_product_named(&db, "audit-unclosed-fence");
    let authored = "Brief\r\n```text\r\nunclosed authored example\r\n";
    let real_event = "[engine-reconcile] epoch 2: real event.";
    let parking_event = "[parking] epoch 3: refresh_pr: PR is still open.";
    let full_description = format!("{authored}\n{real_event}\n{parking_event}");

    let path = db.path().to_path_buf();
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE products SET description = ?2 WHERE id = ?1",
        params![product.id, full_description],
    )
    .unwrap();
    conn.execute("DELETE FROM schema_migrations WHERE id = ?1", [MIGRATION_ID])
        .unwrap();
    drop(conn);
    drop(db);

    for _ in 0..2 {
        let db = WorkDb::open(path.clone()).unwrap();
        let WorkItem::Product(item) = db.get_work_item(&product.id).unwrap() else {
            panic!("product")
        };
        assert_eq!(item.description, authored);
        let history: Vec<_> = db
            .list_task_field_audit(&product.id)
            .unwrap()
            .into_iter()
            .filter(|row| row.property == RECONCILIATION_PROPERTY)
            .collect();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].detail.as_deref(), Some(real_event));
        assert_eq!(history[0].changed_at, "2");
        assert_eq!(history[1].detail.as_deref(), Some(parking_event));
        assert_eq!(history[1].changed_at, "3");
    }
}
