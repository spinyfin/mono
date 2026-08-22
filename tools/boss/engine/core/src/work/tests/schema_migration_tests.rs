//! Schema-migration tests exercised by re-opening a DB: effort/model
//! column re-add, redundant task repo-url clearing, terminal merge-queue
//! cleanup, and empty-string effort normalisation to NULL.

use super::*;

/// An existing installation can carry attachment rows in the original schema
/// where deleting an execution cascaded into evidence loss. Reopening must
/// preserve those rows while removing that cascade.
#[test]
fn migration_detaches_existing_attachments_from_execution_retention() {
    let (_dir, path) = disk_db_path("work-attachments-retention-upgrade");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@github.com:test/repo.git"));
    let chore = create_test_chore(&db, &product.id, "Legacy attachment");
    let execution = create_ready_chore_execution(&db, &chore.id);
    {
        let conn = db.connect().unwrap();
        conn.execute_batch(
            "DROP TABLE work_attachments;
             CREATE TABLE work_attachments (
                 id             TEXT PRIMARY KEY,
                 execution_id   TEXT NOT NULL REFERENCES work_executions(id) ON DELETE CASCADE,
                 work_item_id   TEXT NOT NULL,
                 caption        TEXT NOT NULL DEFAULT '',
                 content_digest TEXT NOT NULL,
                 media_type     TEXT NOT NULL,
                 pixel_width    INTEGER NOT NULL,
                 pixel_height   INTEGER NOT NULL,
                 size_bytes     INTEGER NOT NULL,
                 source_name    TEXT NOT NULL,
                 created_at     TEXT NOT NULL,
                 reclaimed_at   TEXT,
                 UNIQUE (execution_id, content_digest)
             );
             CREATE INDEX work_attachments_work_item_idx
                 ON work_attachments(work_item_id, created_at);
             CREATE INDEX work_attachments_digest_idx
                 ON work_attachments(content_digest);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_attachments
                 (id, execution_id, work_item_id, caption, content_digest, media_type,
                  pixel_width, pixel_height, size_bytes, source_name, created_at, reclaimed_at)
             VALUES ('atc_legacy', ?1, ?2, 'preserved evidence', 'legacy-digest', 'png',
                     10, 10, 4, 'legacy.png', '1700000000', '1700000100')",
            rusqlite::params![execution.id, chore.id],
        )
        .unwrap();
    }
    drop(db);

    let db = WorkDb::open(path).expect("the populated legacy attachment schema migrates cleanly");
    let conn = db.connect().unwrap();
    let has_execution_fk = conn
        .prepare(
            "SELECT 1 FROM pragma_foreign_key_list('work_attachments')
             WHERE \"table\" = 'work_executions'",
        )
        .unwrap()
        .exists([])
        .unwrap();
    assert!(
        !has_execution_fk,
        "attachment retention must no longer depend on execution rows"
    );
    let reclaimed_at: Option<String> = conn
        .query_row(
            "SELECT reclaimed_at FROM work_attachments WHERE id = 'atc_legacy'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        reclaimed_at.as_deref(),
        Some("1700000100"),
        "the rebuild must copy an existing tombstone timestamp"
    );
    let index_names: Vec<String> = conn
        .prepare("SELECT name FROM pragma_index_list('work_attachments')")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        index_names.iter().any(|name| name == "work_attachments_work_item_idx"),
        "listing index must survive the rebuild, got {index_names:?}"
    );
    assert!(
        index_names.iter().any(|name| name == "work_attachments_digest_idx"),
        "orphan-sweep index must survive the rebuild, got {index_names:?}"
    );
    let duplicate_err = conn
        .execute(
            "INSERT INTO work_attachments
                 (id, execution_id, work_item_id, caption, content_digest, media_type,
                  pixel_width, pixel_height, size_bytes, source_name, created_at)
             VALUES ('atc_dup', ?1, ?2, 'replay', 'legacy-digest', 'png',
                     10, 10, 4, 'legacy.png', '1700000200')",
            rusqlite::params![execution.id, chore.id],
        )
        .unwrap_err();
    assert!(
        duplicate_err.to_string().to_lowercase().contains("constraint"),
        "UNIQUE (execution_id, content_digest) must survive the rebuild, got: {duplicate_err}"
    );
    conn.execute("DELETE FROM work_executions WHERE id = ?1", [&execution.id])
        .unwrap();
    drop(conn);
    let attachment = db
        .get_work_attachment("atc_legacy")
        .unwrap()
        .expect("the existing row survives an execution delete");
    assert_eq!(attachment.caption, "preserved evidence");
    assert_eq!(attachment.reclaimed_at.as_deref(), Some("1700000100"));
}

/// Drop the `deferred` column (simulating a pre-classification DB) and
/// re-open: `migrate_tasks_deferred`'s ALTER TABLE path must re-add it and
/// leave existing rows at the `0` (not-future-scope) default, so an upgrade
/// never spuriously parks live work.
#[test]
fn migration_re_adds_deferred_column_defaulting_to_zero() {
    // disk_db_path required: drops a column and re-opens the DB to trigger migration.
    let (_dir, path) = disk_db_path("deferred-upgrade");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@github.com:test/repo.git"));
    let chore = create_test_chore(&db, product.id.clone(), "Legacy chore");

    {
        let conn = db.connect().unwrap();
        conn.execute("ALTER TABLE tasks DROP COLUMN deferred", []).unwrap();
        assert!(!table_has_column(&conn, "tasks", "deferred").unwrap());
    }
    drop(db);

    // Re-open re-runs the migrations.
    let db = WorkDb::open(path.clone()).unwrap();
    {
        let conn = db.connect().unwrap();
        assert!(table_has_column(&conn, "tasks", "deferred").unwrap());
        let deferred: i64 = conn
            .query_row("SELECT deferred FROM tasks WHERE id = ?1", [&chore.id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            deferred, 0,
            "an existing row must default to not-deferred after the migration"
        );
    }
}

/// Existing answer-agent tracking rows predate the dispatch-diagnostics
/// pivot. Reopening that database must add the nullable binding column and
/// its lookup index without fabricating execution ids for historical rows.
#[test]
fn migration_adds_answer_agent_execution_pivot() {
    let (_dir, path) = disk_db_path("migration-answer-agent-execution-pivot");
    let db = WorkDb::open(path.clone()).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute("DROP INDEX answer_agent_runs_by_execution", []).unwrap();
        conn.execute("ALTER TABLE answer_agent_runs DROP COLUMN execution_id", [])
            .unwrap();
        assert!(!table_has_column(&conn, "answer_agent_runs", "execution_id").unwrap());
    }
    drop(db);

    let db = WorkDb::open(path).expect("opening an existing database should run the additive migration");
    let conn = db.connect().unwrap();
    assert!(table_has_column(&conn, "answer_agent_runs", "execution_id").unwrap());
    let has_index = conn
        .query_row(
            "SELECT 1 FROM pragma_index_list('answer_agent_runs') WHERE name = 'answer_agent_runs_by_execution'",
            [],
            |_| Ok(()),
        )
        .optional()
        .unwrap()
        .is_some();
    assert!(has_index, "the execution pivot must remain indexed");
}

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

    for id in [&done_id, &archived_id] {
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

/// Comment intent taxonomy collapse: existing rows carrying the classifier's
/// retired `directive`/`larger_change` values must be re-homed onto the
/// single `revision` value on the next DB open, so they keep matching
/// `revisable_comment_predicate` (`intent = 'revision'`) instead of silently
/// falling out of the `[Revise]` candidate pool. A `question` row is an
/// untouched control.
#[test]
fn migration_collapses_directive_and_larger_change_intent_to_revision() {
    let (_dir, path) = disk_db_path("collapse-directive-larger-change-intent");
    let db = WorkDb::open(path.clone()).unwrap();
    let directive_comment = db
        .create_comment(CreateCommentInput {
            artifact_kind: "work_item".to_owned(),
            artifact_id: "t1".to_owned(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: "alpha".to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "a comment".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    let larger_change_comment = db
        .create_comment(CreateCommentInput {
            artifact_kind: "work_item".to_owned(),
            artifact_id: "t1".to_owned(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: "beta".to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "another comment".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    let question_comment = db
        .create_comment(CreateCommentInput {
            artifact_kind: "work_item".to_owned(),
            artifact_id: "t1".to_owned(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: "gamma".to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "a question".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    db.set_comment_intent(&question_comment.id, "question", 0.9).unwrap();

    // Bypass `set_comment_intent`'s validation (which now rejects these
    // retired values) to simulate legacy rows written before the collapse.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_comments SET intent = 'directive', intent_classified_at = '1' WHERE id = ?1",
            [&directive_comment.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_comments SET intent = 'larger_change', intent_classified_at = '1' WHERE id = ?1",
            [&larger_change_comment.id],
        )
        .unwrap();
    }
    drop(db);

    // Re-opening runs the migration which collapses both retired values.
    let db = WorkDb::open(path.clone()).unwrap();
    assert_eq!(
        db.get_comment(&directive_comment.id)
            .unwrap()
            .unwrap()
            .intent
            .as_deref(),
        Some("revision"),
        "a legacy 'directive' row must be re-homed onto 'revision'",
    );
    assert_eq!(
        db.get_comment(&larger_change_comment.id)
            .unwrap()
            .unwrap()
            .intent
            .as_deref(),
        Some("revision"),
        "a legacy 'larger_change' row must be re-homed onto 'revision'",
    );
    assert_eq!(
        db.get_comment(&question_comment.id).unwrap().unwrap().intent.as_deref(),
        Some("question"),
        "a 'question' row must be left untouched by the collapse migration",
    );

    let _ = std::fs::remove_file(path);
}

/// Regression test for the "comment reads answered when its answer-agent run
/// failed with no reply" incident: `migrate_correct_falsely_answered_comments_with_failed_runs`
/// must repair a `work_comments` row left `status = 'answered'` by the
/// (now-fixed) bug where a failed, no-reply-posted answer-agent run still
/// drove the comment all the way to `answered`. Seeds the exact shape the
/// pre-fix `recover_unanswered_comment` produced — a `failed` run with a
/// NULL `reply_body`, and the comment forced to `answered` via a raw UPDATE
/// bypassing the now-fixed guarded transition — then reopens the DB to
/// trigger the repair migration.
#[test]
fn migration_repairs_a_comment_falsely_answered_by_a_failed_no_reply_run() {
    let (_dir, path) = disk_db_path("repair-falsely-answered-comments");
    let db = WorkDb::open(path.clone()).unwrap();

    let comment = db
        .create_comment(CreateCommentInput {
            artifact_kind: "work_item".to_owned(),
            artifact_id: "t1".to_owned(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: "alpha".to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "why does this retry three times?".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    db.set_comment_intent(&comment.id, "question", 0.9).unwrap();
    db.transition_comment_to_answering(&comment.id).unwrap();
    let run = db
        .create_answer_agent_run(&comment.id, "work_item", "t1", "v0", 0)
        .unwrap();
    db.complete_answer_agent_run(&run.id, "failed", None, Some("stranded_no_stop"))
        .unwrap();

    // Bypass the now-fixed `transition_comment_to_answer_failed` to
    // reproduce exactly what the pre-fix `recover_unanswered_comment` wrote:
    // status forced straight to 'answered' with nothing behind it.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_comments SET status = 'answered', updated_at = '1700000000' WHERE id = ?1",
            [&comment.id],
        )
        .unwrap();
    }
    let corrupted = db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(corrupted.status, "answered");
    drop(db);

    // Re-opening runs the repair migration.
    let db = WorkDb::open(path.clone()).unwrap();
    let repaired = db.get_comment(&comment.id).unwrap().unwrap();
    assert_ne!(
        repaired.status, "answered",
        "a failed, no-reply run must never be left reading as answered"
    );
    assert_eq!(repaired.status, "answer_failed");
    assert_eq!(repaired.status_actor.as_deref(), Some("engine"));
    assert_ne!(
        repaired.updated_at, "1700000000",
        "the repair must restamp updated_at rather than leave the corrupt write's timestamp"
    );

    let _ = std::fs::remove_file(path);
}

/// The revision-intent counterpart: a comment reclassified to `revision`
/// mid-flight (the follow-up reclassifier writes `intent` without touching
/// `status`) must repair to `active`, not `answer_failed` — mirroring
/// `transition_comment_to_answer_failed`'s own fold, so the repaired comment
/// lands in the `[Revise]` pool instead of a failure state that no longer
/// describes it.
#[test]
fn migration_repairs_a_falsely_answered_revisable_comment_to_active() {
    let (_dir, path) = disk_db_path("repair-falsely-answered-revisable-comment");
    let db = WorkDb::open(path.clone()).unwrap();

    let comment = db
        .create_comment(CreateCommentInput {
            artifact_kind: "work_item".to_owned(),
            artifact_id: "t1".to_owned(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: "alpha".to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "please rename this".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    db.set_comment_intent(&comment.id, "question", 0.9).unwrap();
    db.transition_comment_to_answering(&comment.id).unwrap();
    let run = db
        .create_answer_agent_run(&comment.id, "work_item", "t1", "v0", 0)
        .unwrap();
    db.complete_answer_agent_run(&run.id, "failed", None, Some("no_reply_posted"))
        .unwrap();
    db.reclassify_comment_intent(&comment.id, "revision", 0.8).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_comments SET status = 'answered' WHERE id = ?1",
            [&comment.id],
        )
        .unwrap();
    }
    drop(db);

    let db = WorkDb::open(path.clone()).unwrap();
    let repaired = db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(repaired.status, "active");
    assert_eq!(repaired.intent.as_deref(), Some("revision"));

    let _ = std::fs::remove_file(path);
}

/// A comment that hit the bug and then received an operator follow-up
/// before this repair migration ever ran advances past `answered` to
/// `awaiting_followup` — it must still be repaired, not permanently
/// excluded just because its status moved on.
#[test]
fn migration_repairs_a_falsely_answered_comment_that_already_moved_to_awaiting_followup() {
    let (_dir, path) = disk_db_path("repair-falsely-answered-awaiting-followup");
    let db = WorkDb::open(path.clone()).unwrap();

    let comment = db
        .create_comment(CreateCommentInput {
            artifact_kind: "work_item".to_owned(),
            artifact_id: "t1".to_owned(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: "alpha".to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "why does this retry three times?".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    db.set_comment_intent(&comment.id, "question", 0.9).unwrap();
    db.transition_comment_to_answering(&comment.id).unwrap();
    let run = db
        .create_answer_agent_run(&comment.id, "work_item", "t1", "v0", 0)
        .unwrap();
    db.complete_answer_agent_run(&run.id, "failed", None, Some("stranded_no_stop"))
        .unwrap();

    // Reproduce the pre-fix bug (status forced to 'answered' with nothing
    // behind it), then an operator follow-up against the phantom answer
    // advances it further to 'awaiting_followup'.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_comments SET status = 'answered', updated_at = '1700000000' WHERE id = ?1",
            [&comment.id],
        )
        .unwrap();
    }
    db.transition_comment_to_awaiting_followup(&comment.id).unwrap();
    let corrupted = db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(corrupted.status, "awaiting_followup");
    drop(db);

    // Re-opening runs the repair migration.
    let db = WorkDb::open(path.clone()).unwrap();
    let repaired = db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(
        repaired.status, "answer_failed",
        "a falsely-answered comment must be repaired even after it advanced to awaiting_followup"
    );
    assert_eq!(repaired.status_actor.as_deref(), Some("engine"));

    let _ = std::fs::remove_file(path);
}

/// A comment that legitimately reached `answered` via a real reply must be
/// left untouched by the repair migration — it is not the bug's shape.
#[test]
fn migration_leaves_a_genuinely_answered_comment_alone() {
    let (_dir, path) = disk_db_path("repair-leaves-genuine-answer-alone");
    let db = WorkDb::open(path.clone()).unwrap();

    let comment = db
        .create_comment(CreateCommentInput {
            artifact_kind: "work_item".to_owned(),
            artifact_id: "t1".to_owned(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: "alpha".to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "what does this do?".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 0,
        })
        .unwrap();
    db.set_comment_intent(&comment.id, "question", 0.9).unwrap();
    db.transition_comment_to_answering(&comment.id).unwrap();
    let run = db
        .create_answer_agent_run(&comment.id, "work_item", "t1", "v0", 0)
        .unwrap();
    db.complete_answer_agent_run(&run.id, "replied", Some("It lives in config.rs."), None)
        .unwrap();
    db.transition_comment_to_answered(&comment.id).unwrap();
    drop(db);

    let db = WorkDb::open(path.clone()).unwrap();
    let untouched = db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(
        untouched.status, "answered",
        "a genuine reply must still read as answered"
    );

    let _ = std::fs::remove_file(path);
}

/// Upgrading a database that predates the `reasoning` column re-adds it and
/// leaves every existing row NULL. No backfill is correct, not an omission:
/// NULL is the "never classified" state the dispatcher resolves through its
/// legacy kind-floor / effort-table path, so an in-flight row keeps the exact
/// model it had before the upgrade. Backfilling `standard` here would silently
/// re-model every `large` row on the board from Opus to Sonnet.
#[test]
fn migration_re_adds_reasoning_column_and_leaves_existing_rows_null() {
    // disk_db_path required: drops a column and re-opens the DB to migrate.
    let (_dir, path) = disk_db_path("reasoning-upgrade");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@github.com:test/repo.git"));
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Legacy large chore")
                .effort_level(EffortLevel::Large)
                .build(),
        )
        .unwrap();

    {
        let conn = db.connect().unwrap();
        conn.execute("ALTER TABLE tasks DROP COLUMN reasoning", []).unwrap();
        assert!(!table_has_column(&conn, "tasks", "reasoning").unwrap());
    }
    drop(db);

    let db = WorkDb::open(path.clone()).unwrap();
    {
        let conn = db.connect().unwrap();
        assert!(table_has_column(&conn, "tasks", "reasoning").unwrap());
        let stored: Option<String> = conn
            .query_row("SELECT reasoning FROM tasks WHERE id = ?1", [&chore.id], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(stored.is_none(), "the migration must not backfill a value");
    }

    // And the row reads back as unclassified through the mapper, so the
    // dispatcher takes the legacy path for it.
    let reread = db.get_work_item(&chore.id).unwrap();
    let task = match reread {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore/task item, got {other:?}"),
    };
    assert!(task.reasoning.is_none());
    assert_eq!(
        task.effort_level,
        Some(EffortLevel::Large),
        "size signal survives intact"
    );
}

/// Legacy cancelled task rows must migrate to archived without losing their
/// row identity, historical terminal timestamp, or existing tombstone.
#[test]
fn migrate_cancelled_task_statuses_to_archived_with_provenance() {
    // disk_db_path required: the test re-opens the DB to trigger the migration.
    let (_dir, path) = disk_db_path("migration-cancelled-to-archived");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Backfill", Some("git@example.com:backfill.git"));

    let conn = db.connect().unwrap();
    conn.execute_batch("PRAGMA ignore_check_constraints = ON;").unwrap();
    let now = now_string();

    // Chain root: already `done` (its PR merged).
    let root_id = next_id("task");
    conn.execute(
        "INSERT INTO tasks (id, product_id, kind, name, description, status, pr_url, created_at, updated_at, autostart, priority, created_via)
         VALUES (?1, ?2, 'chore', 'root', '', 'done', 'https://github.com/spinyfin/mono/pull/2473', ?3, ?3, 0, 'medium', 'test')",
        params![root_id, product.id, now],
    ).unwrap();

    // Legacy cancelled revision under a merged chain root: the status
    // migration must move it to `archived` and leave `deleted_at` alone.
    let stuck_id = next_id("task");
    conn.execute(
        "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, autostart, priority, created_via, parent_task_id)
         VALUES (?1, ?2, 'revision', 'Resolve merge conflict against main', '', 'cancelled', ?3, ?3, 0, 'medium', ?4, ?5)",
        params![stuck_id, product.id, now, format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}crz_stuck"), root_id],
    ).unwrap();

    // Control 1: a live root does not change the migration outcome.
    let open_root_id = next_id("task");
    conn.execute(
        "INSERT INTO tasks (id, product_id, kind, name, description, status, pr_url, created_at, updated_at, autostart, priority, created_via)
         VALUES (?1, ?2, 'chore', 'open root', '', 'in_review', 'https://github.com/spinyfin/mono/pull/2500', ?3, ?3, 0, 'medium', 'test')",
        params![open_root_id, product.id, now],
    ).unwrap();
    let not_yet_id = next_id("task");
    conn.execute(
        "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, autostart, priority, created_via, parent_task_id)
         VALUES (?1, ?2, 'revision', 'Resolve merge conflict against main', '', 'cancelled', ?3, ?3, 0, 'medium', ?4, ?5)",
        params![not_yet_id, product.id, now, format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}crz_open"), open_root_id],
    ).unwrap();

    // Control 2: a non-moot revision is also migrated rather than lost.
    let non_moot_id = next_id("task");
    conn.execute(
        "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, autostart, priority, created_via, parent_task_id)
         VALUES (?1, ?2, 'revision', 'Address review feedback', '', 'cancelled', ?3, ?3, 0, 'medium', ?4, ?5)",
        params![non_moot_id, product.id, now, format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_feedback"), root_id],
    ).unwrap();

    drop(conn);
    drop(db);

    // Re-open the DB to trigger the migration.
    let db2 = WorkDb::open(path.clone()).unwrap();
    let conn2 = db2.connect().unwrap();

    let (stuck_status, stuck_deleted_at, stuck_by, stuck_at, stuck_reason, stuck_actor): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    ) = conn2
        .query_row(
            "SELECT status, deleted_at, archived_by, archived_at, archived_reason, last_status_actor
             FROM tasks WHERE id = ?1",
            [&stuck_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(stuck_status, "archived");
    assert_eq!(stuck_by.as_deref(), Some("legacy_cancelled_status_migration"));
    assert!(stuck_at.as_deref().is_some_and(|at| !at.is_empty()));
    assert_eq!(stuck_reason.as_deref(), Some("migrated from legacy cancelled status"));
    assert_eq!(stuck_actor, "engine");
    assert!(
        stuck_deleted_at.is_none(),
        "migration changes status without deleting the legacy row"
    );

    let (not_yet_status, not_yet_deleted_at): (String, Option<String>) = conn2
        .query_row(
            "SELECT status, deleted_at FROM tasks WHERE id = ?1",
            [&not_yet_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(not_yet_status, "archived");
    assert!(
        not_yet_deleted_at.is_none(),
        "migration must not turn a live legacy row into a tombstone"
    );

    let (non_moot_status, non_moot_deleted_at): (String, Option<String>) = conn2
        .query_row(
            "SELECT status, deleted_at FROM tasks WHERE id = ?1",
            [&non_moot_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(non_moot_status, "archived");
    assert!(
        non_moot_deleted_at.is_none(),
        "migration must not delete non-moot legacy rows"
    );

    drop(conn2);
    let _ = std::fs::remove_file(path);
}

/// `migrate_backfill_resolve_stale_dead_review_attentions` must resolve an
/// open `pr_review_died_without_findings` attention once its work item has a
/// LATER completed `pr_review` execution on record, and must leave alone an
/// attention with no such later pass — re-opening the DB triggers the
/// one-time sweep.
#[test]
fn migrate_backfill_resolve_stale_dead_review_attentions_resolves_only_when_later_review_completed() {
    // disk_db_path required: the test re-opens the DB to trigger the migration.
    let (_dir, path) = disk_db_path("migration-backfill-dead-review-attentions");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Backfill", Some("git@example.com:backfill.git"));
    let chore_healed = create_test_chore(&db, product.id.clone(), "healed by a later review");
    let chore_stuck = create_test_chore(&db, product.id.clone(), "still genuinely stale");

    let conn = db.connect().unwrap();

    // Attention filed at t=1000 for both work items.
    let attn_healed_id = next_id("attn");
    conn.execute(
        "INSERT INTO work_attention_items
            (id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at)
         VALUES (?1, NULL, ?2, 'pr_review_died_without_findings', 'open', 'died', 'stale', '1000', NULL)",
        params![attn_healed_id, chore_healed.id],
    )
    .unwrap();
    let attn_stuck_id = next_id("attn");
    conn.execute(
        "INSERT INTO work_attention_items
            (id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at)
         VALUES (?1, NULL, ?2, 'pr_review_died_without_findings', 'open', 'died', 'stale', '1000', NULL)",
        params![attn_stuck_id, chore_stuck.id],
    )
    .unwrap();

    // Only `chore_healed` has a LATER completed pr_review execution
    // (finished_at = 2000, after the attention's created_at = 1000).
    let healed_exec_id = next_id("exec");
    conn.execute(
        "INSERT INTO work_executions
            (id, work_item_id, kind, status, repo_remote_url, priority, created_at, finished_at)
         VALUES (?1, ?2, 'pr_review', 'completed', 'https://github.com/test/repo', 0, '1500', '2000')",
        params![healed_exec_id, chore_healed.id],
    )
    .unwrap();
    // `chore_stuck` only has an EARLIER dead execution (the one the
    // attention itself describes) — no later completed pass.
    let dead_exec_id = next_id("exec");
    conn.execute(
        "INSERT INTO work_executions
            (id, work_item_id, kind, status, repo_remote_url, priority, created_at, finished_at)
         VALUES (?1, ?2, 'pr_review', 'orphaned', 'https://github.com/test/repo', 0, '900', '950')",
        params![dead_exec_id, chore_stuck.id],
    )
    .unwrap();

    drop(conn);
    drop(db);

    // Re-open the DB to trigger the migration.
    let db2 = WorkDb::open(path.clone()).unwrap();
    let conn2 = db2.connect().unwrap();

    let (healed_status, healed_resolved_at): (String, Option<String>) = conn2
        .query_row(
            "SELECT status, resolved_at FROM work_attention_items WHERE id = ?1",
            [&attn_healed_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        healed_status, "resolved",
        "an attention with a later completed review pass must be auto-resolved"
    );
    assert!(healed_resolved_at.is_some());

    let stuck_status: String = conn2
        .query_row(
            "SELECT status FROM work_attention_items WHERE id = ?1",
            [&attn_stuck_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stuck_status, "open",
        "an attention with no later completed review pass must be left open"
    );

    drop(conn2);
    let _ = std::fs::remove_file(path);
}

/// Regression test for the incident that motivated
/// `migrate_repair_invalid_project_status`: the auto-unblock cascade's
/// pre-fix untyped shared writer let the `TaskStatus` literal `"todo"`
/// land in `projects.status`, which `ProjectStatus` doesn't accept. Seed
/// exactly that shape via raw SQL (bypassing the engine's typed write
/// paths, which can no longer produce it — see
/// `write_engine_project_status`/`write_engine_task_status`) and confirm
/// re-opening the DB repairs the row to `planned`, restamps `updated_at`
/// and `last_status_actor` (rather than leaving them pointing at the
/// corrupt write, as the incident's live hand-repair did), and leaves the
/// row decodable via `query_project`.
#[test]
fn migration_repairs_out_of_enum_project_status() {
    let (_dir, path) = disk_db_path("repair-invalid-project-status");
    // Seed the corrupt row directly against a pre-migration legacy schema —
    // a fresh `WorkDb::open` already carries the new `CHECK` constraint
    // added by `migrate_projects_tasks_status_check`, so a `status = 'todo'`
    // project row can no longer be written through it at all. Matches this
    // incident's real path: the bad write landed on an existing database
    // that predated both the fix and the constraint.
    let conn = rusqlite::Connection::open(&path).unwrap();
    LegacySchema::new(4)
        .products(NO_EXTRA_COLUMNS)
        .projects(PROJECTS_V4_COLUMNS)
        .tasks(TASKS_V4_COLUMNS)
        .seed(&legacy_product_seed("prod_legacy", "Legacy", "legacy"))
        .seed(
            // Stamped exactly like the pre-fix cascade wrote it:
            // `last_status_actor = 'engine'`, `updated_at` from the bad write.
            "INSERT INTO projects(id, product_id, name, slug, status, priority, last_status_actor, created_at, updated_at)
             VALUES ('proj_legacy', 'prod_legacy', 'Corrupted', 'corrupted', 'todo', 'medium', 'engine', '1700000000', '1700000000');",
        )
        .create(&conn);
    drop(conn);

    // Re-open re-runs the full migration chain, including the repair.
    let db = WorkDb::open(path.clone()).unwrap();
    let conn2 = db.connect().unwrap();

    let (status, last_status_actor, updated_at): (String, String, String) = conn2
        .query_row(
            "SELECT status, last_status_actor, updated_at FROM projects WHERE id = 'proj_legacy'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, "planned", "out-of-enum status must be repaired to planned");
    assert_eq!(last_status_actor, "engine");
    assert_ne!(
        updated_at, "1700000000",
        "the repair must restamp updated_at rather than leave the corrupt write's timestamp in place"
    );

    let repaired = query_project(&conn2, "proj_legacy")
        .unwrap()
        .expect("project must still exist");
    assert_eq!(repaired.status, ProjectStatus::Planned);

    drop(conn2);
    let _ = std::fs::remove_file(path);
}

/// Existing databases gain both halves of project-status provenance without
/// fabricating a basis for transitions that happened before the migration.
#[test]
fn migration_adds_project_status_provenance_columns() {
    let (_dir, path) = disk_db_path("project-status-provenance");
    let conn = rusqlite::Connection::open(&path).unwrap();
    LegacySchema::new(4)
        .products(NO_EXTRA_COLUMNS)
        .projects(PROJECTS_V4_COLUMNS)
        .tasks(TASKS_V4_COLUMNS)
        .seed(&legacy_product_seed("prod_legacy", "Legacy", "legacy"))
        .seed(
            "INSERT INTO projects(id, product_id, name, slug, status, priority, last_status_actor, created_at, updated_at)
             VALUES ('proj_legacy', 'prod_legacy', 'Legacy project', 'legacy-project', 'active', 'medium', 'human', '1700000000', '1700000000');",
        )
        .create(&conn);
    drop(conn);

    let db = WorkDb::open(path.clone()).unwrap();
    let conn = db.connect().unwrap();
    assert!(table_has_column(&conn, "projects", "status_basis").unwrap());
    assert!(table_has_column(&conn, "project_property_audit", "basis").unwrap());
    let basis: Option<String> = conn
        .query_row(
            "SELECT status_basis FROM projects WHERE id = 'proj_legacy'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(basis.is_none(), "legacy status basis must remain unknown, not guessed");

    drop(conn);
    let _ = std::fs::remove_file(path);
}

/// After the schema-init migration chain runs, both `projects.status` and
/// `tasks.status` must be constrained by the new `CHECK` — not just the
/// application-level `ProjectStatus`/`TaskStatus` parsers used by the typed
/// write paths. A raw SQL write of a value valid in the *other* table's
/// vocabulary (`"todo"` for projects, `"planned"` for tasks) must now fail
/// outright rather than silently land, which is exactly the failure mode
/// `migrate_projects_tasks_status_check` closes.
#[test]
fn status_check_constraint_rejects_out_of_enum_values_on_both_tables() {
    let path = temp_db_path("status-check-constraint");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@github.com:test/repo.git"));
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "P".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: true,
        })
        .unwrap();
    let chore = create_test_chore(&db, product.id.clone(), "C");

    let conn = db.connect().unwrap();
    let project_err = conn
        .execute("UPDATE projects SET status = 'todo' WHERE id = ?1", [&project.id])
        .unwrap_err();
    assert!(
        project_err.to_string().to_lowercase().contains("constraint"),
        "expected a CHECK constraint violation, got: {project_err}"
    );

    let task_err = conn
        .execute("UPDATE tasks SET status = 'planned' WHERE id = ?1", [&chore.id])
        .unwrap_err();
    assert!(
        task_err.to_string().to_lowercase().contains("constraint"),
        "expected a CHECK constraint violation, got: {task_err}"
    );
    let _ = std::fs::remove_file(path);
}

// ── status-CHECK table rebuild: FK children, recovery, rollback ─────────
//
// `migrate_projects_tasks_status_check` rebuilds `projects` and `tasks`,
// and both are foreign-key parents with live children. The fixtures below
// stand up a legacy schema whose `tasks.project_id` genuinely declares
// `REFERENCES projects(id)` (the builder's baseline `tasks` does not) plus
// the two chain-created child tables that can be seeded before the chain
// runs, so the rebuild is exercised against a database shaped like a real
// one rather than one whose parent tables happen to have no children.
//
// `automation_runs.produced_task_id` and
// `automation_dedup_suppressions.surviving_task_id` are the other two
// `tasks` children; they are not seeded here because their own parent
// tables (`automations`) are chain-created and pre-creating them in the
// fixture would freeze a stale copy of a table ~20 columns wide. Under
// enforced foreign keys any one referencing row is enough to abort the
// `DROP`, and `attention_groups` and `task_targets` supply that for both
// parents.

/// `tasks` as a pre-migration database really has it: the baseline
/// columns, but with the `project_id` foreign key the `LegacySchema`
/// builder's baseline omits — plus
/// `investigation_doc_repo_remote_url`, which
/// `e8936cb5 "derive investigation-doc repo from task"` stopped adding to
/// new databases without ever dropping it from existing ones ("existing
/// DBs retain the dead column; new DBs never get it"). It is invisible in
/// today's source and was missing from the shipped rebuild's column list,
/// so a rebuild would have taken it and its contents with it.
const TASKS_WITH_PROJECT_FK_DDL: &str = "CREATE TABLE tasks (
     id TEXT PRIMARY KEY,
     product_id TEXT NOT NULL REFERENCES products(id),
     project_id TEXT REFERENCES projects(id),
     kind TEXT NOT NULL,
     name TEXT NOT NULL,
     description TEXT NOT NULL DEFAULT '',
     status TEXT NOT NULL,
     ordinal INTEGER,
     pr_url TEXT,
     deleted_at TEXT,
     created_at TEXT NOT NULL,
     updated_at TEXT NOT NULL,
     autostart INTEGER NOT NULL DEFAULT 1,
     last_status_actor TEXT NOT NULL DEFAULT 'human',
     priority TEXT NOT NULL DEFAULT 'medium',
     investigation_doc_repo_remote_url TEXT
 );";

/// `projects` extras for a database old enough to still carry the
/// pre-pointer design-doc storage. Like the `tasks` column above, no
/// migration ever dropped these, they are invisible in today's source,
/// and the shipped rebuild's column list omitted them.
const PROJECTS_LEGACY_DESIGN_DOC_COLUMNS: &str = "last_status_actor TEXT NOT NULL DEFAULT 'human',
     design_doc TEXT, design_doc_updated_at TEXT, design_doc_draft TEXT";

/// `attention_groups` verbatim from `migrate_attentions` — a child of
/// both `projects` and `tasks`. The chain's own `CREATE TABLE IF NOT
/// EXISTS` sees it already present and later migrations `ALTER` it as
/// usual.
const ATTENTION_GROUPS_FK_CHILD_DDL: &str = "CREATE TABLE attention_groups (
     id                         TEXT PRIMARY KEY,
     product_id                 TEXT NOT NULL REFERENCES products(id),
     short_id                   INTEGER,
     kind                       TEXT NOT NULL,
     association_project_id     TEXT REFERENCES projects(id),
     association_task_id        TEXT REFERENCES tasks(id),
     source_kind                TEXT NOT NULL,
     source_task_id             TEXT,
     source_run_id              TEXT,
     source_doc_path            TEXT,
     source_doc_repo_remote_url TEXT,
     source_doc_branch          TEXT,
     grouping_key               TEXT NOT NULL,
     generation                 INTEGER NOT NULL DEFAULT 0,
     state                      TEXT NOT NULL DEFAULT 'open',
     produced_artifact_kind     TEXT,
     produced_artifact_ref      TEXT,
     created_at                 TEXT NOT NULL,
     actioned_at                TEXT,
     dismissed_at               TEXT,
     CHECK (
         (association_project_id IS NOT NULL AND association_task_id IS NULL)
         OR (association_project_id IS NULL  AND association_task_id IS NOT NULL)
     )
 );";

/// `task_targets` verbatim from `migrate_task_targets_table` — a second
/// child of `tasks`.
const TASK_TARGETS_FK_CHILD_DDL: &str = "CREATE TABLE task_targets (
     id         TEXT PRIMARY KEY,
     task_id    TEXT NOT NULL REFERENCES tasks(id),
     kind       TEXT NOT NULL CHECK (kind IN ('file', 'symbol')),
     value      TEXT NOT NULL,
     created_at TEXT NOT NULL
 );";

/// Stand up a legacy database with real foreign-key children of both
/// `projects` and `tasks`, plus any `extra` DDL the caller wants (the
/// half-migrated fixture uses it to plant an orphaned `projects_v2`).
fn seed_legacy_db_with_fk_children(path: &std::path::Path, extra_ddl: &[&str]) {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut schema = LegacySchema::new(4)
        .products(NO_EXTRA_COLUMNS)
        .projects(PROJECTS_LEGACY_DESIGN_DOC_COLUMNS)
        .ddl(TASKS_WITH_PROJECT_FK_DDL)
        .ddl(ATTENTION_GROUPS_FK_CHILD_DDL)
        .ddl(TASK_TARGETS_FK_CHILD_DDL);
    for ddl in extra_ddl {
        schema = schema.ddl(ddl);
    }
    schema
        .seed(&legacy_product_seed("prod_legacy", "Legacy", "legacy"))
        .seed(&legacy_project_seed("proj_one", "prod_legacy", "One", "one"))
        .seed(&legacy_project_seed("proj_two", "prod_legacy", "Two", "two"))
        .seed(
            "UPDATE projects
                SET design_doc = 'the whole design, inline',
                    design_doc_updated_at = '1700000009',
                    design_doc_draft = 'an unpublished draft'
              WHERE id = 'proj_one';",
        )
        // Two tasks under proj_one and one project-less chore, so both a
        // set and a NULL are exercised on the FK column itself. Each
        // project also gets its design task up front, so
        // `migrate_backfill_project_design_tasks` is a no-op and the row
        // counts these tests assert on stay exact.
        .seed(
            "INSERT INTO tasks(id, product_id, project_id, kind, name, description, status, ordinal,
                               pr_url, created_at, updated_at, investigation_doc_repo_remote_url)
             VALUES ('task_one', 'prod_legacy', 'proj_one', 'project_task', 'First', 'body one', 'in_review', 3,
                     'https://github.com/spinyfin/mono/pull/1', '1700000000', '1700000000',
                     'git@github.com:spinyfin/mono.git');",
        )
        .seed(
            "INSERT INTO tasks(id, product_id, project_id, kind, name, description, status, created_at, updated_at)
             VALUES ('task_two', 'prod_legacy', 'proj_one', 'project_task', 'Second', '', 'todo', '1700000000', '1700000000');",
        )
        .seed(
            "INSERT INTO tasks(id, product_id, kind, name, description, status, created_at, updated_at)
             VALUES ('chore_one', 'prod_legacy', 'chore', 'Chore', '', 'done', '1700000000', '1700000000');",
        )
        .seed(
            "INSERT INTO tasks(id, product_id, project_id, kind, name, description, status, created_at, updated_at)
             VALUES ('design_one', 'prod_legacy', 'proj_one', 'design', 'Design', '', 'done', '1700000000', '1700000000');",
        )
        .seed(
            "INSERT INTO tasks(id, product_id, project_id, kind, name, description, status, created_at, updated_at)
             VALUES ('design_two', 'prod_legacy', 'proj_two', 'design', 'Design', '', 'done', '1700000000', '1700000000');",
        )
        .seed(
            "INSERT INTO attention_groups(id, product_id, kind, association_project_id, source_kind,
                                          grouping_key, created_at)
             VALUES ('attn_proj', 'prod_legacy', 'review', 'proj_one', 'review', 'k1', '1700000000');",
        )
        .seed(
            "INSERT INTO attention_groups(id, product_id, kind, association_task_id, source_kind,
                                          grouping_key, created_at)
             VALUES ('attn_task', 'prod_legacy', 'review', 'task_one', 'review', 'k2', '1700000000');",
        )
        .seed(
            "INSERT INTO task_targets(id, task_id, kind, value, created_at)
             VALUES ('tt_one', 'task_one', 'file', 'tools/boss/engine/core/src/work.rs', '1700000000');",
        )
        .create(&conn);
    drop(conn);
}

/// Ids in a table, sorted — for asserting a rebuild moved every row and
/// invented none.
fn sorted_ids(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let mut stmt = conn.prepare(&format!("SELECT id FROM {table} ORDER BY id")).unwrap();
    stmt.query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap()
}

fn table_present(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get::<_, bool>(0),
    )
    .unwrap()
}

fn table_ddl(conn: &rusqlite::Connection, table: &str) -> String {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get::<_, String>(0),
    )
    .unwrap()
}

/// The regression test for the boot loop `boss-v1.0.485` shipped.
///
/// `migrate_projects_tasks_status_check` rebuilds `projects` and `tasks`
/// by `DROP TABLE` + rename. Both are foreign-key parents, and
/// `schema_init` turns `PRAGMA foreign_keys = ON` on every connection, so
/// under enforced foreign keys `DROP TABLE projects` runs an implicit
/// `DELETE FROM projects` that aborts on the first referencing `tasks`
/// row. The pre-fix migration issued the rebuild as one bare
/// `execute_batch` with foreign keys enabled and no enclosing
/// transaction, so on any real database it committed `projects_v2` and
/// its copy, aborted on the `DROP`, and wedged every later startup on
/// `table projects_v2 already exists` — the engine could not open its
/// database at all.
///
/// The existing coverage missed this because
/// `migration_repairs_out_of_enum_project_status` seeds a project with no
/// referencing rows, and
/// `status_check_constraint_rejects_out_of_enum_values_on_both_tables`
/// opens a *fresh* database, which takes the
/// `apply_final_schema_template` fast path and never runs the rebuild.
/// This test seeds children of both parents, so the `DROP` has something
/// to violate.
#[test]
fn status_check_migration_rebuilds_tables_that_have_foreign_key_children() {
    let (_dir, path) = disk_db_path("status-check-fk-children");
    seed_legacy_db_with_fk_children(&path, &[]);

    // The whole point: this open must succeed. Against the shipped
    // migration it fails with `FOREIGN KEY constraint failed`.
    let db = WorkDb::open(path.clone()).expect("engine must be able to open a database with FK children");
    let conn = db.connect().unwrap();

    assert!(
        table_ddl(&conn, "projects").contains("CHECK (status IN"),
        "projects must end up constrained: {}",
        table_ddl(&conn, "projects")
    );
    assert!(
        table_ddl(&conn, "tasks").contains("CHECK (status IN"),
        "tasks must end up constrained: {}",
        table_ddl(&conn, "tasks")
    );
    assert!(
        !table_present(&conn, "projects_v2") && !table_present(&conn, "tasks_v2"),
        "the rebuild must leave no scratch table behind"
    );

    // Every row moved, none invented.
    assert_eq!(sorted_ids(&conn, "projects"), vec!["proj_one", "proj_two"]);
    assert_eq!(
        sorted_ids(&conn, "tasks"),
        vec!["chore_one", "design_one", "design_two", "task_one", "task_two"]
    );

    // The two column families that exist only on old databases — no
    // migration ever dropped them, and neither appears in today's source —
    // must survive the rebuild with their contents. The shipped column
    // list omitted all four, so the rebuild would have discarded them.
    let (design_doc, design_doc_updated_at, design_doc_draft): (Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT design_doc, design_doc_updated_at, design_doc_draft FROM projects WHERE id = 'proj_one'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(design_doc.as_deref(), Some("the whole design, inline"));
    assert_eq!(design_doc_updated_at.as_deref(), Some("1700000009"));
    assert_eq!(design_doc_draft.as_deref(), Some("an unpublished draft"));

    let investigation_doc_repo: Option<String> = conn
        .query_row(
            "SELECT investigation_doc_repo_remote_url FROM tasks WHERE id = 'task_one'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        investigation_doc_repo.as_deref(),
        Some("git@github.com:spinyfin/mono.git")
    );

    // Non-trivial column values survive the copy, including a NULL and
    // the foreign key itself — a row count alone would not catch a
    // column list that had drifted out of alignment.
    let (project_id, description, ordinal, pr_url, deleted_at, status): (
        Option<String>,
        String,
        Option<i64>,
        Option<String>,
        Option<String>,
        String,
    ) = conn
        .query_row(
            "SELECT project_id, description, ordinal, pr_url, deleted_at, status FROM tasks WHERE id = 'task_one'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(project_id.as_deref(), Some("proj_one"));
    assert_eq!(description, "body one");
    assert_eq!(ordinal, Some(3));
    assert_eq!(pr_url.as_deref(), Some("https://github.com/spinyfin/mono/pull/1"));
    assert_eq!(deleted_at, None, "a NULL must stay NULL across the rebuild");
    assert_eq!(status, "in_review");

    let chore_project_id: Option<String> = conn
        .query_row("SELECT project_id FROM tasks WHERE id = 'chore_one'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(chore_project_id, None, "a NULL foreign key must stay NULL");

    // The children still exist and their references still resolve.
    assert_eq!(sorted_ids(&conn, "attention_groups"), vec!["attn_proj", "attn_task"]);
    assert_eq!(sorted_ids(&conn, "task_targets"), vec!["tt_one"]);
    let violations: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(violations, 0, "the rebuild must not orphan any child row");

    // Step 12: foreign key enforcement is back on for the rest of this
    // connection's life, not left off by the migration.
    let foreign_keys_on: bool = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0)).unwrap();
    assert!(
        foreign_keys_on,
        "foreign key enforcement must be restored after the rebuild"
    );

    // Idempotent: a second open must not try to rebuild again.
    drop(conn);
    drop(db);
    let db2 = WorkDb::open(path.clone()).expect("a second open must be a no-op, not a second rebuild");
    let conn2 = db2.connect().unwrap();
    assert_eq!(
        sorted_ids(&conn2, "tasks"),
        vec!["chore_one", "design_one", "design_two", "task_one", "task_two"]
    );
    assert!(!table_present(&conn2, "tasks_v2"));

    drop(conn2);
    let _ = std::fs::remove_file(path);
}

/// The operator's exact situation after running `boss-v1.0.485`: the
/// pre-fix migration committed `CREATE TABLE projects_v2` and its copy,
/// then aborted on `DROP TABLE projects`, leaving `projects`
/// unconstrained and a `projects_v2` orphan behind. Every subsequent
/// start then failed one statement earlier, on `table projects_v2 already
/// exists`, so the engine could never open its database again. A fixed
/// build has to recover that database, not just avoid creating it.
#[test]
fn status_check_migration_recovers_a_database_left_half_migrated() {
    let (_dir, path) = disk_db_path("status-check-half-migrated");
    // The orphan exactly as the failed rebuild left it: the constrained
    // shape, already carrying a copy of the rows.
    let orphan_ddl = "CREATE TABLE projects_v2 (
         id TEXT PRIMARY KEY,
         product_id TEXT NOT NULL REFERENCES products(id),
         name TEXT NOT NULL,
         slug TEXT NOT NULL,
         description TEXT NOT NULL DEFAULT '',
         goal TEXT NOT NULL DEFAULT '',
         status TEXT NOT NULL CHECK (status IN ('planned', 'active', 'blocked', 'done', 'archived')),
         priority TEXT NOT NULL,
         created_at TEXT NOT NULL,
         updated_at TEXT NOT NULL,
         last_status_actor TEXT NOT NULL DEFAULT 'human'
     );";
    seed_legacy_db_with_fk_children(&path, &[orphan_ddl]);
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "INSERT INTO projects_v2 (id, product_id, name, slug, description, goal, status, priority,
                                      created_at, updated_at, last_status_actor)
             SELECT id, product_id, name, slug, description, goal, status, priority,
                    created_at, updated_at, last_status_actor FROM projects;",
        )
        .unwrap();
    }

    let db = WorkDb::open(path.clone()).expect("a fixed build must recover a half-migrated database");
    let conn = db.connect().unwrap();

    assert!(
        !table_present(&conn, "projects_v2"),
        "the orphaned scratch table must be cleared, not adopted"
    );
    assert!(table_ddl(&conn, "projects").contains("CHECK (status IN"));
    assert!(table_ddl(&conn, "tasks").contains("CHECK (status IN"));
    assert_eq!(sorted_ids(&conn, "projects"), vec!["proj_one", "proj_two"]);
    assert_eq!(
        sorted_ids(&conn, "tasks"),
        vec!["chore_one", "design_one", "design_two", "task_one", "task_two"]
    );

    drop(conn);
    let _ = std::fs::remove_file(path);
}

/// The transaction is the thing standing between a botched rebuild and
/// unrecoverable data loss, so it is asserted directly rather than
/// assumed. The staged copy of `tasks` is sabotaged after the `INSERT`
/// and before the `DROP`, which trips the row-count assertion — by which
/// point `projects` has already been dropped and renamed *inside the
/// same transaction*. Afterwards both original tables must still be
/// there, whole and unconstrained, with no scratch table left behind.
#[test]
fn status_check_migration_rolls_back_intact_when_a_staged_copy_is_short() {
    let (_dir, path) = disk_db_path("status-check-rollback");
    seed_legacy_db_with_fk_children(&path, &[]);

    SABOTAGE_STAGED_COPY_FOR_TABLE.with(|cell| cell.set(Some("tasks")));
    let opened = WorkDb::open(path.clone());
    SABOTAGE_STAGED_COPY_FOR_TABLE.with(|cell| cell.set(None));

    let err = opened
        .err()
        .expect("an incomplete staged copy must fail the migration loudly");
    let message = format!("{err:#}");
    assert!(
        message.contains("staged 4 of 5 row(s)") && message.contains("refusing to drop"),
        "the failure must name the shortfall it refused to act on, got: {message}"
    );

    let conn = rusqlite::Connection::open(&path).unwrap();
    assert!(
        table_present(&conn, "projects") && table_present(&conn, "tasks"),
        "a failed rebuild must leave both original tables in place"
    );
    assert!(
        !table_present(&conn, "projects_v2") && !table_present(&conn, "tasks_v2"),
        "a failed rebuild must leave no scratch table behind"
    );
    assert_eq!(
        sorted_ids(&conn, "projects"),
        vec!["proj_one", "proj_two"],
        "every project row must survive a failed rebuild"
    );
    assert_eq!(
        sorted_ids(&conn, "tasks"),
        vec!["chore_one", "design_one", "design_two", "task_one", "task_two"],
        "every task row must survive a failed rebuild"
    );
    assert!(
        !table_ddl(&conn, "projects").contains("CHECK (status IN"),
        "the rollback must undo the projects rebuild that had already completed inside the transaction"
    );

    drop(conn);
    let _ = std::fs::remove_file(path);
}
