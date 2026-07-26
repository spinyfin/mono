//! Work-item free-form tags: write validation, set/add/remove semantics,
//! and round-trip through get_work_tree / query_task.

use super::*;
use boss_protocol::{WORK_ITEM_TAG_MAX_COUNT, WORK_ITEM_TAG_MAX_LEN};

#[test]
fn tags_default_empty_on_fresh_task() {
    let path = temp_db_path("tags-default-empty");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "No tags yet");

    let item = db.get_work_item(&chore.id).unwrap();
    let WorkItem::Chore(task) = item else {
        panic!("expected chore");
    };
    assert!(task.tags.is_empty());

    let tree = db.get_work_tree(&product.id).unwrap();
    let found = tree
        .chores
        .iter()
        .find(|t| t.id == chore.id)
        .expect("chore in work tree");
    assert!(found.tags.is_empty());

    let _ = std::fs::remove_file(path);
}

#[test]
fn set_tags_round_trips_through_show_and_work_tree() {
    let path = temp_db_path("tags-set-roundtrip");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Tagged chore");

    let updated = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                tags: Some(vec!["needs-human".into(), "ci-flake".into()]),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Chore(task) = updated else {
        panic!("expected chore");
    };
    assert_eq!(task.tags, vec!["needs-human", "ci-flake"]);

    let reloaded = db.get_work_item(&chore.id).unwrap();
    let WorkItem::Chore(task) = reloaded else {
        panic!("expected chore");
    };
    assert_eq!(task.tags, vec!["needs-human", "ci-flake"]);

    let tree = db.get_work_tree(&product.id).unwrap();
    let found = tree
        .chores
        .iter()
        .find(|t| t.id == chore.id)
        .expect("chore in work tree");
    assert_eq!(found.tags, vec!["needs-human", "ci-flake"]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn add_and_remove_tags_are_deduped_and_ordered() {
    let path = temp_db_path("tags-add-remove");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Add/remove");

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            tags: Some(vec!["alpha".into(), "beta".into()]),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    // Re-adding an existing tag is a no-op; new tags append.
    let updated = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                add_tags: Some(vec!["beta".into(), "gamma".into()]),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Chore(task) = updated else {
        panic!("expected chore");
    };
    assert_eq!(task.tags, vec!["alpha", "beta", "gamma"]);

    let updated = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                remove_tags: Some(vec!["beta".into(), "missing".into()]),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Chore(task) = updated else {
        panic!("expected chore");
    };
    assert_eq!(task.tags, vec!["alpha", "gamma"]);

    // Clear via empty set.
    let updated = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                tags: Some(vec![]),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Chore(task) = updated else {
        panic!("expected chore");
    };
    assert!(task.tags.is_empty());

    let _ = std::fs::remove_file(path);
}

#[test]
fn tag_longer_than_max_len_is_rejected() {
    let path = temp_db_path("tags-too-long");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Long tag");

    let too_long: String = "a".repeat(WORK_ITEM_TAG_MAX_LEN + 1);
    let err = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                tags: Some(vec![too_long]),
                ..WorkItemPatch::default()
            },
        )
        .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("too long"), "error should explain why: {message}");
    assert!(
        message.contains(&WORK_ITEM_TAG_MAX_LEN.to_string()),
        "error should name the limit: {message}"
    );

    let reloaded = db.get_work_item(&chore.id).unwrap();
    let WorkItem::Chore(task) = reloaded else {
        panic!("expected chore");
    };
    assert!(task.tags.is_empty(), "failed write must not mutate tags");

    let _ = std::fs::remove_file(path);
}

#[test]
fn more_than_max_count_tags_is_rejected() {
    let path = temp_db_path("tags-too-many");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Many tags");

    let too_many: Vec<String> = (0..=WORK_ITEM_TAG_MAX_COUNT).map(|i| format!("t{i}")).collect();
    assert_eq!(too_many.len(), WORK_ITEM_TAG_MAX_COUNT + 1);

    let err = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                tags: Some(too_many),
                ..WorkItemPatch::default()
            },
        )
        .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("too many"), "error should explain why: {message}");
    assert!(
        message.contains(&WORK_ITEM_TAG_MAX_COUNT.to_string()),
        "error should name the limit: {message}"
    );

    let reloaded = db.get_work_item(&chore.id).unwrap();
    let WorkItem::Chore(task) = reloaded else {
        panic!("expected chore");
    };
    assert!(task.tags.is_empty());

    let _ = std::fs::remove_file(path);
}

#[test]
fn max_count_tags_at_limit_succeeds() {
    let path = temp_db_path("tags-at-limit");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "At limit");

    let at_limit: Vec<String> = (0..WORK_ITEM_TAG_MAX_COUNT).map(|i| format!("tag{i}")).collect();
    let updated = db
        .update_work_item(
            &chore.id,
            WorkItemPatch {
                tags: Some(at_limit.clone()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
    let WorkItem::Chore(task) = updated else {
        panic!("expected chore");
    };
    assert_eq!(task.tags, at_limit);

    let _ = std::fs::remove_file(path);
}

#[test]
fn normalize_trims_and_drops_empty() {
    let got = crate::work::updates::normalize_and_validate_tags(&[
        "  needs-human  ".into(),
        "".into(),
        "   ".into(),
        "ci-flake".into(),
        "needs-human".into(),
    ])
    .unwrap();
    assert_eq!(got, vec!["needs-human", "ci-flake"]);
}

#[test]
fn apply_tag_patch_set_then_add_then_remove() {
    let current = vec!["keep".to_owned(), "drop".to_owned()];
    let got = crate::work::updates::apply_tag_patch(
        &current,
        Some(vec!["a".into(), "b".into()]),
        Some(vec!["c".into(), "a".into()]),
        Some(vec!["b".into()]),
    )
    .unwrap();
    assert_eq!(got, vec!["a", "c"]);
}
