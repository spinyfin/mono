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

/// Bulk list surfaces must project `tags` the same way single-item show
/// does — previously `list_tasks` / `list_chores` / `list_revisions` used
/// mappers that hard-coded an empty vec, so `boss task list --json` never
/// emitted the field even when the row carried tags in the DB.
#[test]
fn list_tasks_chores_and_revisions_project_tags_like_show() {
    let path = temp_db_path("tags-list-projection");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product(&db);
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Tagged project".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
        })
        .unwrap();

    let tagged_chore = create_test_chore_manual(&db, product.id.clone(), "Tagged chore");
    let untagged_chore = create_test_chore_manual(&db, product.id.clone(), "Untagged chore");
    db.update_work_item(
        &tagged_chore.id,
        WorkItemPatch {
            tags: Some(vec!["needs-human".into(), "ci-flake".into()]),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let tagged_task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Tagged task")
                .autostart(false)
                .build(),
        )
        .unwrap();
    let untagged_task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Untagged task")
                .autostart(false)
                .build(),
        )
        .unwrap();
    db.update_work_item(
        &tagged_task.id,
        WorkItemPatch {
            tags: Some(vec!["grok".into()]),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let tagged_revision_id = insert_revision_row(&db, &product.id, &tagged_chore.id);
    let untagged_revision_id = insert_revision_row(&db, &product.id, &untagged_chore.id);
    db.update_work_item(
        &tagged_revision_id,
        WorkItemPatch {
            tags: Some(vec!["revision-tag".into()]),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    // Show baseline: tagged rows carry tags; untagged rows are empty vec
    // (serde skip_serializing_if means the JSON key is omitted — same as
    // list after this fix).
    let show_tagged_chore = match db.get_work_item(&tagged_chore.id).unwrap() {
        WorkItem::Chore(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(show_tagged_chore.tags, vec!["needs-human", "ci-flake"]);
    let show_untagged_chore = match db.get_work_item(&untagged_chore.id).unwrap() {
        WorkItem::Chore(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert!(show_untagged_chore.tags.is_empty());

    // list_chores
    let chores = db.list_chores(&product.id, None, false).unwrap();
    let listed_tagged = chores.iter().find(|t| t.id == tagged_chore.id).expect("tagged chore");
    let listed_untagged = chores
        .iter()
        .find(|t| t.id == untagged_chore.id)
        .expect("untagged chore");
    assert_eq!(listed_tagged.tags, show_tagged_chore.tags);
    assert_eq!(listed_untagged.tags, show_untagged_chore.tags);

    // list_tasks (product-wide + project-scoped)
    let show_tagged_task = match db.get_work_item(&tagged_task.id).unwrap() {
        WorkItem::Task(t) => t,
        other => panic!("expected task, got {other:?}"),
    };
    let show_untagged_task = match db.get_work_item(&untagged_task.id).unwrap() {
        WorkItem::Task(t) => t,
        other => panic!("expected task, got {other:?}"),
    };
    for listed in [
        db.list_tasks(&product.id, None, None, false).unwrap(),
        db.list_tasks(&product.id, Some(&project.id), None, false).unwrap(),
    ] {
        let tagged = listed.iter().find(|t| t.id == tagged_task.id).expect("tagged task");
        let untagged = listed.iter().find(|t| t.id == untagged_task.id).expect("untagged task");
        assert_eq!(tagged.tags, show_tagged_task.tags);
        assert_eq!(untagged.tags, show_untagged_task.tags);
    }
    // Chores also appear on product-wide list_tasks (flavor-complete leaf list).
    let product_tasks = db.list_tasks(&product.id, None, None, false).unwrap();
    let chore_on_task_list = product_tasks
        .iter()
        .find(|t| t.id == tagged_chore.id)
        .expect("chore on task list");
    assert_eq!(chore_on_task_list.tags, show_tagged_chore.tags);

    // list_revisions — compare against the same query_task path show uses.
    let show_tagged_rev = query_task(&db.connect().unwrap(), &tagged_revision_id)
        .unwrap()
        .expect("tagged revision");
    let show_untagged_rev = query_task(&db.connect().unwrap(), &untagged_revision_id)
        .unwrap()
        .expect("untagged revision");
    assert_eq!(show_tagged_rev.tags, vec!["revision-tag"]);
    assert!(show_untagged_rev.tags.is_empty());

    let revisions = db.list_revisions(&product.id, None, false, None).unwrap();
    let listed_tagged_rev = revisions
        .iter()
        .find(|t| t.id == tagged_revision_id)
        .expect("tagged revision");
    let listed_untagged_rev = revisions
        .iter()
        .find(|t| t.id == untagged_revision_id)
        .expect("untagged revision");
    assert_eq!(listed_tagged_rev.tags, show_tagged_rev.tags);
    assert_eq!(listed_untagged_rev.tags, show_untagged_rev.tags);

    // JSON shape: non-empty tags serialize; empty tags are omitted (same as show).
    let tagged_json = serde_json::to_value(listed_tagged).unwrap();
    assert_eq!(
        tagged_json.get("tags").and_then(|v| v.as_array()).map(|a| a.len()),
        Some(2)
    );
    let untagged_json = serde_json::to_value(listed_untagged).unwrap();
    assert!(
        untagged_json.get("tags").is_none(),
        "empty tags must skip_serializing like show, got {untagged_json}"
    );

    let _ = std::fs::remove_file(path);
}
