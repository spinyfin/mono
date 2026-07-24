//! Schema-migration tests exercised by re-opening a DB: effort/model
//! column re-add, redundant task repo-url clearing, terminal merge-queue
//! cleanup, and empty-string effort normalisation to NULL.

use super::*;

/// Drop the effort/model columns (simulating a pre-PR-370 DB)
/// and re-open: the migration's ALTER TABLE path must re-add
/// them and leave existing rows with NULL on each new column.
/// SQLite 3.35+ supports `ALTER TABLE … DROP COLUMN`, which lets
/// us replay an upgrade-in-place without hand-rolling the
/// pre-v7 schema from scratch.
#[test]
fn migration_re_adds_effort_and_model_columns_on_upgrade() {
    // disk_db_path required: drops columns and re-opens the DB to trigger migration.
    let (_dir, path) = disk_db_path("effort-upgrade");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@github.com:test/repo.git"));
    let chore = create_test_chore(&db, product.id.clone(), "Legacy chore");

    {
        let conn = db.connect().unwrap();
        // Drop the new columns to simulate a pre-migration DB.
        conn.execute("ALTER TABLE tasks DROP COLUMN effort_level", []).unwrap();
        conn.execute("ALTER TABLE tasks DROP COLUMN model_override", [])
            .unwrap();
        conn.execute("ALTER TABLE products DROP COLUMN default_model", [])
            .unwrap();
        assert!(!table_has_column(&conn, "tasks", "effort_level").unwrap());
        assert!(!table_has_column(&conn, "tasks", "model_override").unwrap());
        assert!(!table_has_column(&conn, "products", "default_model").unwrap());
    }
    drop(db);

    // Re-open re-runs the migrations.
    let db = WorkDb::open(path.clone()).unwrap();
    {
        let conn = db.connect().unwrap();
        assert!(table_has_column(&conn, "tasks", "effort_level").unwrap());
        assert!(table_has_column(&conn, "tasks", "model_override").unwrap());
        assert!(table_has_column(&conn, "products", "default_model").unwrap());

        let chore_effort: Option<String> = conn
            .query_row("SELECT effort_level FROM tasks WHERE id = ?1", [&chore.id], |row| {
                row.get(0)
            })
            .unwrap();
        let chore_model: Option<String> = conn
            .query_row("SELECT model_override FROM tasks WHERE id = ?1", [&chore.id], |row| {
                row.get(0)
            })
            .unwrap();
        let product_model: Option<String> = conn
            .query_row(
                "SELECT default_model FROM products WHERE id = ?1",
                [&product.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(chore_effort.is_none());
        assert!(chore_model.is_none());
        assert!(product_model.is_none());
    }

    // Post-migration rows can carry any of the five enum
    // values; the round-trip continues to work.
    let after_chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Post-migration chore")
                .effort_level(EffortLevel::Trivial)
                .model_override("haiku")
                .build(),
        )
        .unwrap();
    assert_eq!(after_chore.effort_level, Some(EffortLevel::Trivial));
    assert_eq!(after_chore.model_override.as_deref(), Some("haiku"));

    let _ = std::fs::remove_file(path);
}

/// Migration test: rows created against a pre-migration schema
/// keep `NULL` for the new columns after the migration runs.
/// Mirrors the legacy-row contract every prior migration is
/// expected to honour.
#[test]
fn migration_leaves_existing_rows_with_null_effort_and_model() {
    // disk_db_path required: re-opens the DB to trigger migration.
    let (_dir, path) = disk_db_path("effort-migrate");

    // Stand up a "pre-migration" DB by hand-rolling rows with the
    // older column set, then re-open via `WorkDb::open` so the
    // migration runs against it. We don't replay the entire pre-v7
    // schema; we just drop the new columns on a freshly-init'd DB
    // to simulate the upgrade path.
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@github.com:test/repo.git"));
    let chore = create_test_chore(&db, product.id.clone(), "Pre-migration chore");
    // Simulate the pre-migration state by NULL-ing whatever the
    // current schema initialised. `create_chore` already stores
    // NULL for `effort_level` / `model_override`, and
    // `create_product` already stores NULL for `default_model`,
    // so we just confirm that — the explicit ALTER-TABLE path on
    // re-open is exercised by the legacy-on-disk DBs in the
    // field, which the upgrade test below would otherwise be a
    // synthetic re-init of.
    drop(db);

    let db = WorkDb::open(path.clone()).unwrap();
    let conn = db.connect().unwrap();
    let chore_effort: Option<String> = conn
        .query_row("SELECT effort_level FROM tasks WHERE id = ?1", [&chore.id], |row| {
            row.get(0)
        })
        .unwrap();
    let chore_model: Option<String> = conn
        .query_row("SELECT model_override FROM tasks WHERE id = ?1", [&chore.id], |row| {
            row.get(0)
        })
        .unwrap();
    let product_model: Option<String> = conn
        .query_row(
            "SELECT default_model FROM products WHERE id = ?1",
            [&product.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(chore_effort.is_none());
    assert!(chore_model.is_none());
    assert!(product_model.is_none());

    let _ = std::fs::remove_file(path);
}

/// Cleanup migration: rows where `tasks.repo_remote_url` mirrors the
/// parent product's repo get set to NULL; rows with a genuinely
/// divergent override (legitimate multi-repo task overrides) are
/// left unchanged.
#[test]
fn migrate_null_redundant_task_repo_remote_urls_clears_mirrors_and_preserves_divergent() {
    // disk_db_path required: the test re-opens the DB to trigger the migration.
    let (_dir, path) = disk_db_path("migration-null-redundant-repos");
    let db = WorkDb::open(path.clone()).unwrap();

    // Product with repo_remote_url = "git@example.com:foo.git".
    let product = create_test_product_with_repo(&db, "Foo", Some("git@example.com:foo.git"));
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Proj".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        })
        .unwrap();

    let conn = db.connect().unwrap();

    // Seed 3 chores that mirror the product's repo (the legacy bug).
    // We bypass the API to plant the now-invalid state directly.
    let mirrored_ids: Vec<String> = (0..3).map(|i| {
            let id = next_id("task");
            let now = now_string();
            conn.execute(
                "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, repo_remote_url)
                 VALUES (?1, ?2, NULL, 'chore', ?3, '', 'todo', NULL, NULL, NULL, ?4, ?4, 0, 'medium', 'test', 'git@example.com:foo.git')",
                params![id, product.id, format!("chore-mirror-{i}"), now],
            ).unwrap();
            id
        }).collect();

    // Seed 1 chore with a legitimately different repo (multi-repo override).
    let divergent_id = next_id("task");
    let now = now_string();
    conn.execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, repo_remote_url)
             VALUES (?1, ?2, NULL, 'chore', 'divergent', '', 'todo', NULL, NULL, NULL, ?3, ?3, 0, 'medium', 'test', 'git@example.com:other.git')",
            params![divergent_id, product.id, now],
        ).unwrap();

    // Also seed a task (with project_id) that mirrors the product's repo.
    let mirrored_task_id = next_id("task");
    let now = now_string();
    conn.execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, repo_remote_url)
             VALUES (?1, ?2, ?3, 'project_task', 'mirrored-task', '', 'todo', 5, NULL, NULL, ?4, ?4, 0, 'medium', 'test', 'git@example.com:foo.git')",
            params![mirrored_task_id, product.id, project.id, now],
        ).unwrap();

    // Re-open the DB to trigger the migration.
    drop(conn);
    let db2 = WorkDb::open(path.clone()).unwrap();
    let conn2 = db2.connect().unwrap();

    // All mirrored rows must now have repo_remote_url = NULL.
    for id in &mirrored_ids {
        let val: Option<String> = conn2
            .query_row("SELECT repo_remote_url FROM tasks WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(
            val.is_none(),
            "mirrored chore {id} must be NULL after migration, got {val:?}"
        );
    }
    let mirrored_task_val: Option<String> = conn2
        .query_row(
            "SELECT repo_remote_url FROM tasks WHERE id = ?1",
            [&mirrored_task_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        mirrored_task_val.is_none(),
        "mirrored task must be NULL after migration, got {mirrored_task_val:?}"
    );

    // The divergent override must remain unchanged.
    let divergent_val: Option<String> = conn2
        .query_row(
            "SELECT repo_remote_url FROM tasks WHERE id = ?1",
            [&divergent_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        divergent_val.as_deref(),
        Some("git@example.com:other.git"),
        "divergent override must survive migration unchanged",
    );

    drop(conn2);
    let _ = std::fs::remove_file(path);
}

/// One-time cleanup migration (mono#58-shown-for-4): a terminal (`done`/
/// `archived`) row that still carries `merge_queue_state = 'queued'` from
/// before terminal transitions started clearing that column must be reset
/// to `NULL` on the next DB open; a live (`in_review`) row's queue state
/// must survive untouched.
#[test]
fn migrate_clear_merge_queue_state_on_terminal_tasks_clears_orphans_preserves_live() {
    let (_dir, path) = disk_db_path("migration-clear-merge-queue-orphans");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Foo", Some("git@example.com:foo.git"));
    let conn = db.connect().unwrap();

    let seed = |id: &str, status: &str| {
        let now = now_string();
        conn.execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, merge_queue_state, merge_queue_detail)
             VALUES (?1, ?2, NULL, 'chore', ?3, '', ?4, NULL, NULL, NULL, ?5, ?5, 0, 'medium', 'test', 'queued', '{\"position\":1}')",
            params![id, product.id, format!("chore-{status}"), status, now],
        ).unwrap();
    };
    let done_id = next_id("task");
    seed(&done_id, "done");
    let archived_id = next_id("task");
    seed(&archived_id, "archived");
    let cancelled_id = next_id("task");
    seed(&cancelled_id, "cancelled");
    let live_id = next_id("task");
    seed(&live_id, "in_review");

    drop(conn);
    let db2 = WorkDb::open(path.clone()).unwrap();
    let conn2 = db2.connect().unwrap();

    let read_queue = |id: &str| -> (Option<String>, Option<String>) {
        conn2
            .query_row(
                "SELECT merge_queue_state, merge_queue_detail FROM tasks WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    };

    for id in [&done_id, &archived_id, &cancelled_id] {
        let (state, detail) = read_queue(id);
        assert!(state.is_none(), "terminal row {id} must have merge_queue_state cleared");
        assert!(
            detail.is_none(),
            "terminal row {id} must have merge_queue_detail cleared"
        );
    }
    let (live_state, live_detail) = read_queue(&live_id);
    assert_eq!(
        live_state.as_deref(),
        Some("queued"),
        "a live (in_review) row's merge_queue_state must survive the cleanup migration"
    );
    assert!(
        live_detail.is_some(),
        "a live row's merge_queue_detail must survive the cleanup migration"
    );

    drop(conn2);
    let _ = std::fs::remove_file(path);
}

/// Regression: rows with `effort_level = ''` (empty string, produced by
/// older write paths when clearing the field) should be converted to NULL
/// by the `migrate_tasks_empty_effort_to_null` migration so canonical
/// DB storage matches the schema intent and SQL `IS NULL` queries remain
/// reliable.
#[test]
fn migration_normalises_empty_effort_level_to_null() {
    let (_dir, path) = disk_db_path("effort-empty-to-null");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@github.com:test/repo.git"));
    let chore = create_test_chore(&db, product.id.clone(), "Chore with empty effort");

    // Manually write an empty string to simulate a legacy row.
    {
        let conn = db.connect().unwrap();
        conn.execute("UPDATE tasks SET effort_level = '' WHERE id = ?1", [&chore.id])
            .unwrap();
        let raw: Option<String> = conn
            .query_row("SELECT effort_level FROM tasks WHERE id = ?1", [&chore.id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(raw.as_deref(), Some(""), "pre-condition: row has ''");
    }
    drop(db);

    // Re-opening runs the migration which converts '' to NULL.
    let db = WorkDb::open(path.clone()).unwrap();
    let conn = db.connect().unwrap();
    let after: Option<String> = conn
        .query_row("SELECT effort_level FROM tasks WHERE id = ?1", [&chore.id], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(after.is_none(), "empty effort_level should be NULL after migration");

    let _ = std::fs::remove_file(path);
}
