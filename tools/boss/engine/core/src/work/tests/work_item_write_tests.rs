//! `update_work_item` write-path tests: concurrent writes never surface
//! "database is locked", and `last_status_actor` is only flipped by a
//! genuine status change, never by a no-op status patch.

use super::*;

/// Regression: previously, four `boss chore bind-pr` calls in
/// flight against the same engine would race on a single sqlite
/// connection-per-call with no busy-timeout, and one of them
/// would surface "database is locked" to the caller. With WAL +
/// busy_timeout + IMMEDIATE transactions, concurrent writes on
/// distinct rows must all succeed. (`WorkDb` now pools one shared
/// connection behind a mutex, which serializes these writes
/// in-process rather than at the SQLite layer — the busy-timeout
/// path is no longer what's exercised here, but the observable
/// contract this test asserts, no "database is locked" and every
/// write lands, still holds.)
#[test]
fn concurrent_writes_do_not_return_database_locked() {
    const WORKERS: usize = 8;

    // Must use an on-disk database: WAL mode (which serialises
    // concurrent writers via busy_timeout) is incompatible with
    // SQLite's shared-cache in-memory mode, causing
    // SQLITE_LOCKED_SHAREDCACHE errors that busy_timeout cannot retry.
    let (_dir, path) = disk_db_path("concurrent-writes");
    let db = std::sync::Arc::new(WorkDb::open(path.clone()).unwrap());

    let product = create_test_product(&db);

    // One chore per worker, so each write hits a distinct row —
    // matching the real-world reconcile pattern where a script
    // binds N PRs to N different chores in parallel.
    let chore_ids: Vec<String> = (0..WORKERS)
        .map(|i| create_test_chore_manual(&db, product.id.clone(), format!("Concurrent chore {i}")).id)
        .collect();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(WORKERS));
    let handles: Vec<_> = chore_ids
        .iter()
        .enumerate()
        .map(|(i, chore_id)| {
            let db = db.clone();
            let chore_id = chore_id.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                // Park every worker at the gate so the writes
                // truly land on the engine at the same instant.
                barrier.wait();
                db.update_work_item(
                    &chore_id,
                    WorkItemPatch {
                        pr_url: Some(format!("https://github.com/spinyfin/mono/pull/{}", 100 + i)),
                        ..WorkItemPatch::default()
                    },
                )
            })
        })
        .collect();

    let mut failures: Vec<String> = Vec::new();
    for (i, handle) in handles.into_iter().enumerate() {
        match handle.join().unwrap() {
            Ok(_) => {}
            Err(err) => failures.push(format!("worker {i}: {err:#}")),
        }
    }
    assert!(
        failures.is_empty(),
        "expected all {WORKERS} concurrent writes to succeed, got failures: {failures:?}",
    );

    // And the writes must have actually persisted, not silently
    // been swallowed by a retry that lost its update.
    for (i, chore_id) in chore_ids.iter().enumerate() {
        let item = db.get_work_item(chore_id).unwrap();
        let WorkItem::Chore(task) = item else {
            panic!("expected chore {chore_id} to round-trip as a Chore");
        };
        assert_eq!(
            task.pr_url.as_deref(),
            Some(format!("https://github.com/spinyfin/mono/pull/{}", 100 + i).as_str()),
        );
    }

    let _ = std::fs::remove_file(&path);
    // WAL writes leave -wal / -shm sidecar files; clean them up
    // so the temp dir doesn't leak.
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

/// A no-op status patch (patch.status == current status) must NOT flip
/// `last_status_actor` to 'human'. Regression test for the bug where
/// `status_changed = patch.status.is_some()` caused any patch that
/// carried a status field to overwrite the actor, silently disabling
/// the engine's auto-unblock cascade.
#[test]
fn noop_status_patch_preserves_last_status_actor_for_task() {
    let path = temp_db_path("noop-status-actor-task");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "P", Some("git@github.com:example/repo.git"));
    let chore = create_test_chore_manual(&db, product.id.clone(), "C");

    // Simulate the engine having set the status by writing directly.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET last_status_actor = 'engine' WHERE id = ?1",
            rusqlite::params![chore.id],
        )
        .unwrap();
    }

    // No-op status patch: same value the row already has ('todo').
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("todo".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let conn = db.connect().unwrap();
    let actor: String = conn
        .query_row(
            "SELECT last_status_actor FROM tasks WHERE id = ?1",
            rusqlite::params![chore.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        actor, "engine",
        "no-op status patch must not flip last_status_actor from 'engine' to 'human'"
    );

    let _ = std::fs::remove_file(path);
}

/// Same invariant as `noop_status_patch_preserves_last_status_actor_for_task`
/// but exercised on the project path.
#[test]
fn noop_status_patch_preserves_last_status_actor_for_project() {
    let path = temp_db_path("noop-status-actor-project");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Prod", None);
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Proj".into(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();

    // Pre-seed last_status_actor = 'engine' directly.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE projects SET last_status_actor = 'engine' WHERE id = ?1",
            rusqlite::params![project.id],
        )
        .unwrap();
    }

    // No-op status patch: project default status is 'planned'.
    let current_status = project.status.to_string();
    db.update_work_item(
        &project.id,
        WorkItemPatch {
            status: Some(current_status),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let conn = db.connect().unwrap();
    let actor: String = conn
        .query_row(
            "SELECT last_status_actor FROM projects WHERE id = ?1",
            rusqlite::params![project.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        actor, "engine",
        "no-op status patch must not flip last_status_actor from 'engine' to 'human'"
    );

    let _ = std::fs::remove_file(path);
}

/// A genuine status change must still flip `last_status_actor` to 'human'.
#[test]
fn real_status_change_sets_last_status_actor_human_for_task() {
    let path = temp_db_path("real-status-actor-task");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "P", Some("git@github.com:example/repo.git"));
    let chore = create_test_chore_manual(&db, product.id.clone(), "C");

    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET last_status_actor = 'engine' WHERE id = ?1",
            rusqlite::params![chore.id],
        )
        .unwrap();
    }

    // Genuine status change: todo → active.
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let conn = db.connect().unwrap();
    let actor: String = conn
        .query_row(
            "SELECT last_status_actor FROM tasks WHERE id = ?1",
            rusqlite::params![chore.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        actor, "human",
        "genuine status change must flip last_status_actor to 'human'"
    );

    let _ = std::fs::remove_file(path);
}

/// The automatic postmortem sweep used to call this actor-aware write path
/// with `actor = engine`, letting a partial task-kind predicate certify the
/// whole project complete. The authoritative project writer now rejects that
/// authority even when no open task happens to be visible at the instant of
/// the write.
#[test]
fn engine_cannot_mark_project_done() {
    let path = temp_db_path("engine-project-completion-refused");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "P", Some("git@github.com:example/repo.git"));
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(product.id)
                .name("Project")
                .no_design_task(true)
                .build(),
        )
        .unwrap();

    let err = db
        .update_work_item_as_actor(
            &project.id,
            WorkItemPatch {
                status: Some("done".to_owned()),
                ..WorkItemPatch::default()
            },
            boss_protocol::LAST_STATUS_ACTOR_ENGINE,
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("engine-originated project completion is not allowed"),
        "got: {err}"
    );
    assert_eq!(db.get_project(&project.id).unwrap().status, ProjectStatus::Planned);
    assert!(db.list_project_property_audit(&project.id).unwrap().is_empty());

    let conn = db.connect().unwrap();
    let err = write_engine_project_status(&conn, &project.id, ProjectStatus::Done, "1700000000")
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot mark projects done"), "got: {err}");
    drop(conn);

    let _ = std::fs::remove_file(path);
}

/// Deliberate completion remains available even with open tasks, but both the
/// current project row and the append-only audit must make that override
/// explicit. A later status change must not erase the completion evidence.
#[test]
fn explicit_project_completion_with_open_tasks_records_basis_and_history() {
    let path = temp_db_path("explicit-project-completion-basis");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "P", Some("git@github.com:example/repo.git"));
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(&product.id)
                .name("Project")
                .no_design_task(true)
                .build(),
        )
        .unwrap();
    db.create_task(
        CreateTaskInput::builder()
            .product_id(&product.id)
            .project_id(&project.id)
            .name("Still open")
            .build(),
    )
    .unwrap();

    db.update_work_item(
        &project.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let completed = db.get_project(&project.id).unwrap();
    assert_eq!(completed.status, ProjectStatus::Done);
    assert_eq!(completed.last_status_actor, boss_protocol::LAST_STATUS_ACTOR_HUMAN);
    let basis = completed.status_basis.as_deref().expect("completion basis");
    assert!(basis.contains("completion override requested by human"), "got: {basis}");
    assert!(basis.contains("1 open and 0 terminal"), "got: {basis}");

    db.update_work_item(
        &project.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let audit = db.list_project_property_audit(&project.id).unwrap();
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[0].property, "status");
    assert_eq!(audit[0].old_value.as_deref(), Some("planned"));
    assert_eq!(audit[0].new_value.as_deref(), Some("done"));
    assert_eq!(audit[0].basis.as_deref(), Some(basis));
    assert_eq!(audit[1].old_value.as_deref(), Some("done"));
    assert_eq!(audit[1].new_value.as_deref(), Some("active"));

    let _ = std::fs::remove_file(path);
}
