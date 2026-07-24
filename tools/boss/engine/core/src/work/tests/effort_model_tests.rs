//! Chore `effort_level` / `model_override` config and the product
//! `default_model` set-and-clear round-trips.

use super::*;

/// Default-shape sanity: a freshly-created chore/task has NULL
/// for the new effort/model columns; a freshly-created product
/// has NULL for `default_model`. Confirms the migration's
/// "behaviour unchanged for unset rows" contract holds on a
/// brand-new DB (the easy case — the migration test below
/// covers an upgrade-in-place).
#[test]
fn effort_and_model_default_to_null_on_fresh_rows() {
    let path = temp_db_path("effort-fresh");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@github.com:test/repo.git"));
    assert!(product.default_model.is_none());
    let chore = create_test_chore(&db, product.id.clone(), "Trivial fix");
    assert!(chore.effort_level.is_none());
    assert!(chore.model_override.is_none());
    let _ = std::fs::remove_file(path);
}

/// `create_chore` with `effort_level` / `model_override` set
/// writes both columns; `query_task` reads them back through
/// `map_task` faithfully.
#[test]
fn effort_and_model_roundtrip_through_create_and_query() {
    let path = temp_db_path("effort-roundtrip");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@github.com:test/repo.git"));
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Big investigation")
                .effort_level(EffortLevel::Large)
                .model_override("claude-opus-4-7")
                .build(),
        )
        .unwrap();
    assert_eq!(chore.effort_level, Some(EffortLevel::Large));
    assert_eq!(chore.model_override.as_deref(), Some("claude-opus-4-7"));
    let _ = std::fs::remove_file(path);
}

/// Update verb honours `--effort` set/clear and `--model`
/// set/clear semantics (empty string clears, anything else
/// stores verbatim).
#[test]
fn update_chore_sets_and_clears_effort_and_model() {
    let path = temp_db_path("effort-update");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore(&db, product.id.clone(), "Some work");

    // Set via update.
    let updated = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                effort_level: Some("medium".into()),
                model_override: Some("sonnet".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let task = match updated {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        _ => panic!("expected chore/task item"),
    };
    assert_eq!(task.effort_level, Some(EffortLevel::Medium));
    assert_eq!(task.model_override.as_deref(), Some("sonnet"));

    // Clear via empty string.
    let cleared = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                effort_level: Some(String::new()),
                model_override: Some(String::new()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let task = match cleared {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        _ => panic!("expected chore/task item"),
    };
    assert!(task.effort_level.is_none());
    assert!(task.model_override.is_none());

    let _ = std::fs::remove_file(path);
}

/// Update verb rejects an invalid `effort_level` string with a
/// clear error that names the allowed values.
#[test]
fn update_chore_rejects_invalid_effort_level() {
    let path = temp_db_path("effort-invalid");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore(&db, product.id.clone(), "Some work");

    let err = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                effort_level: Some("galaxybrain".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("galaxybrain"));
    assert!(message.contains("trivial"));
    assert!(message.contains("max"));

    // Row was not partially updated — effort_level remains NULL.
    let after = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                name: Some("force a no-op write so we can re-read".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let task = match after {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        _ => panic!("expected chore/task item"),
    };
    assert!(task.effort_level.is_none());

    let _ = std::fs::remove_file(path);
}

/// `set_product_default_model` round-trip: set then clear.
/// Slugs are stored verbatim (no validation).
#[test]
fn product_default_model_set_and_clear() {
    let path = temp_db_path("default-model");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", None);
    assert!(product.default_model.is_none());

    let with_model = db.set_product_default_model(&product.id, Some("sonnet")).unwrap();
    assert_eq!(with_model.default_model.as_deref(), Some("sonnet"));

    // Verbatim — engine does not normalise the slug.
    let verbatim = db
        .set_product_default_model(&product.id, Some("an-unreleased-model-2099"))
        .unwrap();
    assert_eq!(verbatim.default_model.as_deref(), Some("an-unreleased-model-2099"),);

    let cleared = db.set_product_default_model(&product.id, Some("")).unwrap();
    assert!(cleared.default_model.is_none());

    let cleared_again = db.set_product_default_model(&product.id, None).unwrap();
    assert!(cleared_again.default_model.is_none());

    let _ = std::fs::remove_file(path);
}
