//! `resolve_repo_for_work_item` resolution across task/product/design/docs
//! overrides, plus product `design_repo` / `docs_repo` set-and-clear config.

use super::*;

#[test]
fn resolve_repo_returns_task_override_when_set() {
    let (_dir, _path, db, _product_id, task_id) = make_resolve_scaffold(
        "resolve-override-set",
        Some("git@github.com:spinyfin/product-default.git"),
    );
    set_task_repo(&db, &task_id, Some("git@github.com:spinyfin/per-task.git"));

    let conn = db.connect().unwrap();
    let resolved = resolve_repo_for_work_item(&conn, &task_id).unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("git@github.com:spinyfin/per-task.git"),
        "non-empty task override must win over the product default",
    );
}

#[test]
fn resolve_repo_treats_empty_override_as_unset() {
    let (_dir, _path, db, _product_id, task_id) = make_resolve_scaffold(
        "resolve-override-empty",
        Some("git@github.com:spinyfin/product-default.git"),
    );
    set_task_repo(&db, &task_id, Some(""));

    let conn = db.connect().unwrap();
    let resolved = resolve_repo_for_work_item(&conn, &task_id).unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("git@github.com:spinyfin/product-default.git"),
        "empty-string override must fall through to the product default",
    );
}

#[test]
fn resolve_repo_falls_back_to_product_when_override_null() {
    let (_dir, _path, db, _product_id, task_id) = make_resolve_scaffold(
        "resolve-override-null",
        Some("git@github.com:spinyfin/product-default.git"),
    );
    // Leave tasks.repo_remote_url at its insert-time NULL.

    let conn = db.connect().unwrap();
    let resolved = resolve_repo_for_work_item(&conn, &task_id).unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("git@github.com:spinyfin/product-default.git"),
        "NULL override must inherit from the product",
    );
}

#[test]
fn resolve_repo_returns_none_when_both_null() {
    let (_dir, _path, db, _product_id, task_id) = make_resolve_scaffold("resolve-both-null", None);
    // Both tasks.repo_remote_url and products.repo_remote_url are
    // NULL; the dispatcher will treat the Ok(None) as an
    // unresolved row and record an attention item.

    let conn = db.connect().unwrap();
    let resolved = resolve_repo_for_work_item(&conn, &task_id).unwrap();
    assert!(
        resolved.is_none(),
        "both-NULL must resolve to Ok(None), got {resolved:?}",
    );
}

/// Design tasks on a product with `design_repo` set must resolve
/// to `design_repo` rather than `repo_remote_url`. Acceptance
/// criterion: "`boss task show --json` for a `kind=design` task
/// on that product resolves to `design_repo` for its repo,
/// without any task-level `--repo` set."
#[test]
fn resolve_repo_uses_design_repo_for_design_kind() {
    let (_dir, path) = disk_db_path("resolve-design-repo");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .design_repo("git@github.com:linkedin-sandbox/bduff.git")
                .build(),
        )
        .unwrap();
    // Project creation seeds a `kind = 'design'` task.
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Project".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        })
        .unwrap();

    // Find the seed design task (ordinal = 0).
    let design_task_id: String = {
        let conn = db.connect().unwrap();
        conn.query_row(
            "SELECT id FROM tasks WHERE project_id = ?1 AND kind = 'design'",
            [&project.id],
            |row| row.get(0),
        )
        .unwrap()
    };

    {
        let conn = db.connect().unwrap();
        let resolved = resolve_repo_for_work_item(&conn, &design_task_id).unwrap();
        assert_eq!(
            resolved.as_deref(),
            Some("git@github.com:linkedin-sandbox/bduff.git"),
            "design task must resolve to product.design_repo",
        );
    }

    // Implementation-kind tasks on the same product are unaffected.
    let chore = create_test_chore_manual(&db, product.id.clone(), "Implementation chore");
    let conn = db.connect().unwrap();
    let resolved = resolve_repo_for_work_item(&conn, &chore.id).unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("git@github.com:spinyfin/mono.git"),
        "implementation-kind tasks must continue to resolve to product.repo_remote_url",
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

/// `design_repo` falls through to `repo_remote_url` when unset.
/// Pre-existing products (and the explicit None path) behave
/// exactly as before.
#[test]
fn resolve_repo_design_kind_without_design_repo_falls_through_to_product_repo() {
    let (_dir, path) = disk_db_path("resolve-design-no-override");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Project".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        })
        .unwrap();
    let conn = db.connect().unwrap();
    let design_task_id: String = conn
        .query_row(
            "SELECT id FROM tasks WHERE project_id = ?1 AND kind = 'design'",
            [&project.id],
            |row| row.get(0),
        )
        .unwrap();
    let resolved = resolve_repo_for_work_item(&conn, &design_task_id).unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("git@github.com:spinyfin/mono.git"),
        "without design_repo, a design task must resolve to product.repo_remote_url",
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

/// Task-level `--repo` still wins over `design_repo`. The new
/// override slots in as a new middle layer; it does not change
/// the priority of per-row overrides above it. To plant a
/// row-level override on a single-repo product (the typical case
/// when `design_repo` is set), the test bypasses the
/// task-creation invariant and writes the column directly.
#[test]
fn resolve_repo_task_override_wins_over_design_repo() {
    let (_dir, path) = disk_db_path("resolve-design-override-wins");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .design_repo("git@github.com:linkedin-sandbox/bduff.git")
                .build(),
        )
        .unwrap();
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Project".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: false,
        })
        .unwrap();
    let conn = db.connect().unwrap();
    let design_task_id: String = conn
        .query_row(
            "SELECT id FROM tasks WHERE project_id = ?1 AND kind = 'design'",
            [&project.id],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "UPDATE tasks SET repo_remote_url = ?2 WHERE id = ?1",
        params![design_task_id, "git@github.com:custom/elsewhere.git"],
    )
    .unwrap();

    let resolved = resolve_repo_for_work_item(&conn, &design_task_id).unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("git@github.com:custom/elsewhere.git"),
        "row-level override must win over product.design_repo",
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

/// Round-trip: setting `design_repo` via `create` and clearing it
/// via `update_work_item("")` mirrors the wire-level behaviour of
/// `repo_remote_url`. Confirms the patch path applies / clears
/// the column rather than silently ignoring it.
#[test]
fn product_design_repo_set_and_clear() {
    let (_dir, path) = disk_db_path("design-repo-set-clear");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .design_repo("git@github.com:linkedin-sandbox/bduff.git")
                .build(),
        )
        .unwrap();
    assert_eq!(
        product.design_repo.as_deref(),
        Some("git@github.com:linkedin-sandbox/bduff.git"),
    );

    let cleared = db
        .update_work_item(
            &product.id,
            WorkItemPatch {
                design_repo: Some(String::new()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Product(cleared) = cleared else {
        panic!("expected Product");
    };
    assert!(
        cleared.design_repo.is_none(),
        "empty-string patch must clear design_repo, got {:?}",
        cleared.design_repo,
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

/// Investigation tasks on a product with `docs_repo` set must
/// resolve to `docs_repo` rather than `repo_remote_url`, the
/// docs-repo analogue of the `design_repo` routing above.
/// Implementation-kind tasks on the same product are unaffected.
#[test]
fn resolve_repo_uses_docs_repo_for_investigation_kind() {
    let (_dir, path) = disk_db_path("resolve-docs-repo");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .docs_repo("git@github.com:linkedin-sandbox/bduff.git")
                .build(),
        )
        .unwrap();

    // Create a chore, then flip its kind to `investigation` directly
    // (bypassing the create invariant) so the resolver sees an
    // investigation row on a single-repo product.
    let investigation = create_test_chore_manual(&db, product.id.clone(), "Investigation");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET kind = 'investigation' WHERE id = ?1",
            [&investigation.id],
        )
        .unwrap();

        let resolved = resolve_repo_for_work_item(&conn, &investigation.id).unwrap();
        assert_eq!(
            resolved.as_deref(),
            Some("git@github.com:linkedin-sandbox/bduff.git"),
            "investigation task must resolve to product.docs_repo",
        );
    }

    // Implementation-kind tasks on the same product are unaffected.
    let chore = create_test_chore_manual(&db, product.id.clone(), "Implementation chore");
    let conn = db.connect().unwrap();
    let resolved = resolve_repo_for_work_item(&conn, &chore.id).unwrap();
    assert_eq!(
        resolved.as_deref(),
        Some("git@github.com:spinyfin/mono.git"),
        "implementation-kind tasks must continue to resolve to product.repo_remote_url",
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

/// Round-trip: setting `docs_repo` via `create` and clearing it via
/// `update_work_item("")` mirrors the `design_repo` behaviour.
/// Confirms the patch path applies / clears the column rather than
/// silently ignoring it.
#[test]
fn product_docs_repo_set_and_clear() {
    let (_dir, path) = disk_db_path("docs-repo-set-clear");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .docs_repo("git@github.com:linkedin-sandbox/bduff.git")
                .build(),
        )
        .unwrap();
    assert_eq!(
        product.docs_repo.as_deref(),
        Some("git@github.com:linkedin-sandbox/bduff.git"),
    );

    // Updating an unrelated field must leave docs_repo intact.
    let renamed = db
        .update_work_item(
            &product.id,
            WorkItemPatch {
                name: Some("Boss Renamed".to_owned()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Product(renamed) = renamed else {
        panic!("expected Product");
    };
    assert_eq!(
        renamed.docs_repo.as_deref(),
        Some("git@github.com:linkedin-sandbox/bduff.git"),
        "patch that omits docs_repo must leave it unchanged",
    );

    let cleared = db
        .update_work_item(
            &product.id,
            WorkItemPatch {
                docs_repo: Some(String::new()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Product(cleared) = cleared else {
        panic!("expected Product");
    };
    assert!(
        cleared.docs_repo.is_none(),
        "empty-string patch must clear docs_repo, got {:?}",
        cleared.docs_repo,
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

#[test]
fn resolve_repo_errors_when_parent_product_is_missing() {
    let (_dir, path, db, product_id, task_id) = make_resolve_scaffold(
        "resolve-orphan-product",
        Some("git@github.com:spinyfin/product-default.git"),
    );

    // Drop the parent product behind FK enforcement so the task
    // is left pointing at a non-existent product_id — the
    // referential-integrity break the helper must surface.
    let raw = Connection::open(&path).unwrap();
    // PRAGMA foreign_keys defaults to OFF on a fresh connection,
    // but state it explicitly so the test reads correctly.
    raw.pragma_update(None, "foreign_keys", false).unwrap();
    raw.execute("DELETE FROM products WHERE id = ?1", [&product_id])
        .unwrap();
    drop(raw);

    let conn = db.connect().unwrap();
    let err = resolve_repo_for_work_item(&conn, &task_id).unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains("orphan task") && message.contains(&task_id),
        "expected an orphan-task error mentioning the task id, got: {message}",
    );
}
