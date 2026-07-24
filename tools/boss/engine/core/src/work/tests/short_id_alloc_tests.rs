//! Per-product `short_id` allocator: concurrent allocation, independent
//! per-product sequences, deterministic backfill, the partial unique index,
//! and protocol-struct wire round-trips.

use super::*;

/// Two threads creating chores against the same `WorkDb` (and the
/// same product) must each get a distinct `short_id`. The
/// allocator is wrapped in SQLite's per-write serialisation
/// (`BEGIN IMMEDIATE` + `busy_timeout`), so the test asserts the
/// emergent property: N parallel inserts produce N distinct,
/// gap-free ids starting at 1.
#[test]
fn allocator_concurrent_inserts_produce_distinct_short_ids() {
    // Must use an on-disk database: WAL mode (which serialises
    // concurrent writers via busy_timeout) is incompatible with
    // SQLite's shared-cache in-memory mode.
    let (_dir, path) = disk_db_path("short-id-concurrent");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:concurrent.git"));

    const N: usize = 16;
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let db = db.clone();
        let product_id = product.id.clone();
        handles.push(std::thread::spawn(move || {
            create_test_chore_manual(&db, product_id, format!("c{i}"))
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let conn = db.connect().unwrap();
    // Collect every `short_id` for this product across both tables
    // (the per-product sequence is shared by `tasks` and
    // `projects`; the single `projects` row from `create_product`
    // doesn't create one, but the product itself does not — only
    // the design task for a project would, and we created no
    // project here). The N chores should occupy a contiguous run
    // starting at 1.
    let mut ids: Vec<i64> = conn
        .prepare("SELECT short_id FROM tasks WHERE product_id = ?1 AND short_id IS NOT NULL")
        .unwrap()
        .query_map([&product.id], |row| row.get::<_, i64>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    ids.sort();
    assert_eq!(ids, (1..=N as i64).collect::<Vec<_>>(), "ids: {ids:?}");

    // The counter has advanced past every id we just observed.
    let next: i64 = conn
        .query_row(
            "SELECT next_value FROM short_id_sequences WHERE product_id = ?1",
            [&product.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(next, N as i64 + 1);

    drop(conn);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

/// Two products run independent sequences: each starts at 1, and
/// the per-product counter increments only on inserts against
/// that product.
#[test]
fn allocator_per_product_sequences_are_independent() {
    let path = temp_db_path("short-id-per-product");
    let db = WorkDb::open(path.clone()).unwrap();
    let boss = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));
    let flunge = create_test_product_with_repo(&db, "Flunge", Some("git@example.com:flunge.git"));

    let mk_chore = |product_id: &str, name: &str| create_test_chore_manual(&db, product_id, name);

    let b1 = mk_chore(&boss.id, "b1");
    let f1 = mk_chore(&flunge.id, "f1");
    let b2 = mk_chore(&boss.id, "b2");
    let f2 = mk_chore(&flunge.id, "f2");

    let conn = db.connect().unwrap();
    let short = |id: &str| -> i64 {
        conn.query_row("SELECT short_id FROM tasks WHERE id = ?1", [id], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(short(&b1.id), 1);
    assert_eq!(short(&b2.id), 2);
    assert_eq!(short(&f1.id), 1);
    assert_eq!(short(&f2.id), 2);

    drop(conn);
    let _ = std::fs::remove_file(path);
}

/// Backfill is deterministic: given the same set of (product,
/// created_at, id) tuples, two independent migration runs assign
/// the same `short_id` to every row.
///
/// Setup plants rows via raw SQL with NULL `short_id` and
/// hand-controlled `created_at` values, then invokes the
/// migration directly (the column / table already exist from
/// `WorkDb::open`, but the rows are unnumbered). The merged
/// `(created_at ASC, id ASC)` stream is the contract.
#[test]
fn migrate_short_id_backfill_is_deterministic_and_merges_tasks_and_projects() {
    fn seed_and_backfill(path: &Path) -> Vec<(String, i64)> {
        let db = WorkDb::open(path.to_path_buf()).unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
                "INSERT INTO products (id, name, slug, description, repo_remote_url, status, created_at, updated_at, default_model)
                 VALUES ('prod_a', 'A', 'a', '', NULL, 'active', '0', '0', NULL)",
                [],
            )
            .unwrap();
        // Plant 3 tasks + 2 projects with hand-chosen created_at
        // values so the merged ordering is unambiguous. The
        // expected sequence by (created_at, id):
        //   100  task_a   -> 1
        //   100  task_b   -> 2  (created_at tie, id tiebreaker)
        //   200  proj_a   -> 3
        //   300  task_c   -> 4
        //   400  proj_b   -> 5
        let rows: &[(&str, &str, &str, i64)] = &[
            ("tasks", "task_a", "chore", 100),
            ("tasks", "task_b", "chore", 100),
            ("projects", "proj_a", "", 200),
            ("tasks", "task_c", "chore", 300),
            ("projects", "proj_b", "", 400),
        ];
        for (table, id, kind, ts) in rows {
            if *table == "tasks" {
                conn.execute(
                        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, short_id)
                         VALUES (?1, 'prod_a', NULL, ?2, ?1, '', 'todo', NULL, NULL, NULL, ?3, ?3, 0, 'medium', 'test', NULL)",
                        params![id, kind, ts.to_string()],
                    )
                    .unwrap();
            } else {
                conn.execute(
                        "INSERT INTO projects (id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, short_id)
                         VALUES (?1, 'prod_a', ?1, ?1, '', '', 'planned', 'medium', ?2, ?2, NULL)",
                        params![id, ts.to_string()],
                    )
                    .unwrap();
            }
        }

        // Wipe the prior counter so the backfill replays from 1.
        conn.execute("DELETE FROM short_id_sequences WHERE product_id = 'prod_a'", [])
            .unwrap();
        migrate_short_id_columns(&conn).unwrap();

        let mut pairs: Vec<(String, i64)> = Vec::new();
        for table in &["tasks", "projects"] {
            let sql = format!("SELECT id, short_id FROM {table} WHERE product_id = 'prod_a'");
            let mut stmt = conn.prepare(&sql).unwrap();
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
                .unwrap();
            for r in rows {
                pairs.push(r.unwrap());
            }
        }
        pairs.sort();
        pairs
    }

    let path_a = temp_db_path("short-id-backfill-a");
    let path_b = temp_db_path("short-id-backfill-b");
    let run_a = seed_and_backfill(&path_a);
    let run_b = seed_and_backfill(&path_b);
    assert_eq!(run_a, run_b, "two independent runs must produce identical short_ids");

    let expected: Vec<(String, i64)> = vec![
        ("proj_a".into(), 3),
        ("proj_b".into(), 5),
        ("task_a".into(), 1),
        ("task_b".into(), 2),
        ("task_c".into(), 4),
    ];
    assert_eq!(run_a, expected);

    let _ = std::fs::remove_file(path_a);
    let _ = std::fs::remove_file(path_b);
}

/// The partial unique index `(product_id, short_id) WHERE
/// short_id IS NOT NULL` is the belt-and-braces guard from design
/// Q3 / Q8: a manual SQL insert that collides with an existing
/// per-product `short_id` must be rejected.
#[test]
fn unique_short_id_index_rejects_manual_duplicate() {
    let path = temp_db_path("short-id-index-conflict");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));
    let chore = create_test_chore_manual(&db, product.id.clone(), "c1");
    let existing_short: i64 = {
        let conn = db.connect().unwrap();
        conn.query_row("SELECT short_id FROM tasks WHERE id = ?1", [&chore.id], |row| {
            row.get(0)
        })
        .unwrap()
    };

    // Same `short_id` on a DIFFERENT product is allowed — the
    // uniqueness invariant is `(product_id, short_id)`, not
    // global. Created before the raw connection below so this
    // doesn't nest a `db.` call inside an open `db.connect()` guard.
    let other = create_test_product_with_repo(&db, "Flunge", None);

    // Try to hand-roll a second `tasks` row with the same
    // (product_id, short_id) — the partial unique index must
    // refuse it.
    let conn = db.connect().unwrap();
    let now = now_string();
    let manual_id = next_id("task");
    let err = conn
            .execute(
                "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, short_id)
                 VALUES (?1, ?2, NULL, 'chore', 'dupe', '', 'todo', NULL, NULL, NULL, ?3, ?3, 0, 'medium', 'test', ?4)",
                params![manual_id, product.id, now, existing_short],
            )
            .unwrap_err()
            .to_string();
    assert!(
        err.contains("UNIQUE constraint failed"),
        "expected UNIQUE constraint failure, got: {err}",
    );

    let other_manual_id = next_id("task");
    conn.execute(
            "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, short_id)
             VALUES (?1, ?2, NULL, 'chore', 'cross-product', '', 'todo', NULL, NULL, NULL, ?3, ?3, 0, 'medium', 'test', ?4)",
            params![other_manual_id, other.id, now, existing_short],
        )
        .expect("same short_id on a different product must be permitted");

    drop(conn);
    let _ = std::fs::remove_file(path);
}

/// `create_project` allocates a `short_id` for the project row
/// AND for the auto-spawned design task, both drawn from the
/// per-product sequence (Q1: tasks and projects share a counter).
#[test]
fn create_project_assigns_short_ids_to_project_and_design_task() {
    let path = temp_db_path("short-id-project-design");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", None);
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "P".into(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        })
        .unwrap();

    let conn = db.connect().unwrap();
    let project_short: i64 = conn
        .query_row("SELECT short_id FROM projects WHERE id = ?1", [&project.id], |row| {
            row.get(0)
        })
        .unwrap();
    let design_short: i64 = conn
        .query_row(
            "SELECT short_id FROM tasks WHERE project_id = ?1 AND kind = 'design'",
            [&project.id],
            |row| row.get(0),
        )
        .unwrap();
    // The project row is inserted before its design task, so it
    // gets the lower number.
    assert_eq!(project_short, 1);
    assert_eq!(design_short, 2);

    drop(conn);
    let _ = std::fs::remove_file(path);
}

/// `create_chore` returns a `Task` struct with `short_id` populated.
/// This is the end-to-end wire test: the protocol struct carries the
/// field through the full engine → protocol round-trip.
#[test]
fn create_chore_protocol_struct_carries_short_id() {
    let path = temp_db_path("short-id-wire-task");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);

    let chore = create_test_chore_manual(&db, product.id.clone(), "wire-test");
    assert_eq!(chore.short_id, Some(1), "first chore in product gets short_id 1");

    // A second chore in the same product gets the next number.
    let chore2 = create_test_chore_manual(&db, product.id.clone(), "wire-test-2");
    assert_eq!(chore2.short_id, Some(2));

    // `list_chores` also surfaces the field (exercises the SELECT path).
    let fetched = db.list_chores(&product.id, None, false).unwrap();
    assert_eq!(fetched[0].short_id, Some(1));

    let _ = std::fs::remove_file(path);
}

/// `create_project` returns a `Project` struct with `short_id` populated.
#[test]
fn create_project_protocol_struct_carries_short_id() {
    let path = temp_db_path("short-id-wire-project");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", None);

    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Wire Project".into(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        })
        .unwrap();
    // Project gets short_id = 1; its auto-spawned design task gets 2.
    assert_eq!(project.short_id, Some(1));

    // `get_project` also surfaces the field.
    let fetched = db.get_project(&project.id).unwrap();
    assert_eq!(fetched.short_id, Some(1));

    let _ = std::fs::remove_file(path);
}

/// `list_tasks_for_product` (used by WorkTree / Subscribe) carries
/// `short_id` in every returned `Task`.
#[test]
fn work_tree_tasks_carry_short_id() {
    let path = temp_db_path("short-id-wire-worktree");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);

    create_test_chore_manual(&db, product.id.clone(), "c1");

    let tree = db.get_work_tree(&product.id).unwrap();
    let chore = &tree.chores[0];
    assert_eq!(chore.short_id, Some(1), "WorkTree chore carries short_id");

    let _ = std::fs::remove_file(path);
}
