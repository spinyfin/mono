//! Ideas: create / list / get / update / delete / graduate, short-id
//! allocation, and schema presence.

use super::*;
use boss_protocol::{CreateIdeaInput, EffortLevel, IdeaGraduationKind, IdeaPatch, IdeaStatus, ReasoningMode, TaskKind};

fn create_idea(db: &WorkDb, product_id: &str, name: &str, body: &str) -> Idea {
    db.create_idea(
        CreateIdeaInput::builder()
            .product_id(product_id)
            .name(name)
            .body(body)
            .created_via("test")
            .build(),
    )
    .unwrap()
}

#[test]
fn create_list_get_idea_round_trip() {
    let path = temp_db_path("idea-roundtrip");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));

    let i1 = create_idea(&db, &product.id, "First draft", "# One\n");
    assert_eq!(i1.short_id, Some(1));
    assert_eq!(i1.status, IdeaStatus::Draft);
    assert_eq!(i1.created_via, "test");
    assert_eq!(i1.display_label(), "I1");
    assert_eq!(i1.body, "# One\n");
    assert!(i1.graduated_to_id.is_none());

    let i2 = create_idea(&db, &product.id, "Second draft", "# Two\n");
    assert_eq!(i2.short_id, Some(2));

    let listed = db.list_ideas(&product.id, None).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, i2.id);
    assert_eq!(listed[1].id, i1.id);

    let got = db.get_idea(&i1.id).unwrap().expect("i1 present");
    assert_eq!(got.name, "First draft");

    let by_short = db.get_idea_by_short_id(&product.id, 2).unwrap().expect("I2 present");
    assert_eq!(by_short.id, i2.id);
}

#[test]
fn list_ideas_filters_by_status() {
    let path = temp_db_path("idea-list-status");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));

    let draft = create_idea(&db, &product.id, "Keep drafting", "body");
    let graduated = create_idea(&db, &product.id, "Ship it", "ship body");
    db.graduate_idea(&graduated.id, IdeaGraduationKind::Chore, None, None, None)
        .unwrap();

    let drafts = db.list_ideas(&product.id, Some(IdeaStatus::Draft)).unwrap();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].id, draft.id);

    let grads = db.list_ideas(&product.id, Some(IdeaStatus::Graduated)).unwrap();
    assert_eq!(grads.len(), 1);
    assert_eq!(grads[0].id, graduated.id);

    let all = db.list_ideas(&product.id, None).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn create_idea_rejects_empty_name() {
    let path = temp_db_path("idea-empty-name");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));
    let err = db
        .create_idea(CreateIdeaInput::builder().product_id(&product.id).name("  ").build())
        .unwrap_err();
    assert!(err.to_string().contains("empty"), "err={err}");
}

#[test]
fn update_idea_patches_name_and_body() {
    let path = temp_db_path("idea-update");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));
    let idea = create_idea(&db, &product.id, "Old", "old body");

    let updated = db
        .update_idea(&idea.id, IdeaPatch::builder().name("New").body("new body").build())
        .unwrap();
    assert_eq!(updated.name, "New");
    assert_eq!(updated.body, "new body");
    assert_eq!(updated.status, IdeaStatus::Draft);
}

#[test]
fn graduate_idea_to_chore_is_atomic() {
    let path = temp_db_path("idea-graduate-chore");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));
    let idea = create_idea(&db, &product.id, "A chore idea", "do the thing");

    let (graduated, chore, project) = db
        .graduate_idea(
            &idea.id,
            IdeaGraduationKind::Chore,
            Some("Shipped chore".into()),
            Some(EffortLevel::Small),
            Some(ReasoningMode::Investigation),
        )
        .unwrap();
    assert!(project.is_none());
    let chore = chore.expect("chore produced");
    assert_eq!(graduated.status, IdeaStatus::Graduated);
    assert_eq!(graduated.graduated_to_id.as_deref(), Some(chore.id.as_str()));
    assert_eq!(chore.name, "Shipped chore");
    assert_eq!(chore.description, "do the thing");
    assert_eq!(chore.effort_level, Some(EffortLevel::Small));
    assert_eq!(chore.reasoning, Some(ReasoningMode::Investigation));
    assert!(chore.created_via.starts_with(CREATED_VIA_IDEA_GRADUATION_PREFIX));

    let chores = db.list_chores(&product.id, None, false).unwrap();
    assert_eq!(chores.len(), 1);
    assert_eq!(chores[0].id, chore.id);

    let err = db
        .graduate_idea(&idea.id, IdeaGraduationKind::Chore, None, None, None)
        .unwrap_err();
    assert!(
        err.to_string().contains("cannot be graduated"),
        "expected non-draft refusal, got {err}"
    );
    assert_eq!(db.list_chores(&product.id, None, false).unwrap().len(), 1);
}

#[test]
fn graduate_idea_to_project_seeds_design_task_without_autostart() {
    let path = temp_db_path("idea-graduate-project");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));
    let markdown = "# Proposal\n\nBuild the ideas layer.";
    let idea = create_idea(&db, &product.id, "Ideas layer", markdown);

    let (graduated, chore, project) = db
        .graduate_idea(&idea.id, IdeaGraduationKind::Project, None, None, None)
        .unwrap();
    assert!(chore.is_none());
    let project = project.expect("project produced");
    assert_eq!(graduated.status, IdeaStatus::Graduated);
    assert_eq!(graduated.graduated_to_id.as_deref(), Some(project.id.as_str()));
    assert_eq!(project.name, "Ideas layer");

    let tasks = db.list_tasks(&product.id, Some(&project.id), None, false).unwrap();
    let design = tasks
        .iter()
        .find(|t| t.kind == TaskKind::Design)
        .expect("auto-minted design task");
    assert!(!design.autostart, "design seed must not autostart from idea graduation");
    assert_eq!(design.description, markdown);
}

#[test]
fn graduate_idea_rejects_effort_on_project_without_creating_a_row() {
    let path = temp_db_path("idea-graduate-project-effort");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));
    let idea = create_idea(&db, &product.id, "Stay a draft", "body");

    let err = db
        .graduate_idea(
            &idea.id,
            IdeaGraduationKind::Project,
            None,
            Some(EffortLevel::Small),
            None,
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("only apply when graduating an idea to a chore"),
        "err={err}"
    );

    let still = db.get_idea(&idea.id).unwrap().expect("idea present");
    assert_eq!(still.status, IdeaStatus::Draft);
    assert!(still.graduated_to_id.is_none());
    assert!(db.list_projects(&product.id, None).unwrap().is_empty());
}

#[test]
fn graduate_idea_rejects_archived_without_creating_a_chore() {
    let path = temp_db_path("idea-graduate-archived");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));
    let idea = create_idea(&db, &product.id, "Archived draft", "body");
    {
        let conn = db.connect().unwrap();
        conn.execute("UPDATE ideas SET status = 'archived' WHERE id = ?1", [&idea.id])
            .unwrap();
    }

    let err = db
        .graduate_idea(&idea.id, IdeaGraduationKind::Chore, None, None, None)
        .unwrap_err();
    assert!(err.to_string().contains("archived"), "err={err}");
    assert!(db.list_chores(&product.id, None, false).unwrap().is_empty());
}

#[test]
fn delete_idea_does_not_touch_graduated_target() {
    let path = temp_db_path("idea-delete-graduated");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));
    let idea = create_idea(&db, &product.id, "To delete", "body");
    let (_, chore, _) = db
        .graduate_idea(&idea.id, IdeaGraduationKind::Chore, None, None, None)
        .unwrap();
    let chore_id = chore.expect("chore").id;

    db.delete_idea(&idea.id).unwrap();
    assert!(db.get_idea(&idea.id).unwrap().is_none());
    assert!(
        db.list_chores(&product.id, None, false)
            .unwrap()
            .iter()
            .any(|c| c.id == chore_id),
        "deleting a graduated idea must not delete the chore it became"
    );
}

#[test]
fn ideas_tables_present_on_fresh_and_migrated_db() {
    let path = temp_db_path("idea-schema");
    let db = WorkDb::open(path).unwrap();
    {
        let conn = db.connect().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'ideas')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists, "fresh db must have ideas");
        let seq: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'idea_short_id_sequences')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(seq, "fresh db must have idea_short_id_sequences");

        conn.execute_batch(
            "DROP TABLE ideas;
             DROP TABLE idea_short_id_sequences;",
        )
        .unwrap();
        migrate_ideas_tables(&conn).unwrap();

        let recreated: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'ideas')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(recreated, "migrate_ideas_tables must re-create ideas");
        assert!(table_has_column(&conn, "tasks", "effort_matched_rule").unwrap());
    }
}

#[test]
fn work_tree_includes_ideas() {
    let path = temp_db_path("idea-worktree");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));
    let idea = create_idea(&db, &product.id, "In the tree", "secret body");
    let tree = db.get_work_tree(&product.id).unwrap();
    assert_eq!(tree.ideas.len(), 1);
    assert_eq!(tree.ideas[0].id, idea.id);
    assert_eq!(tree.ideas[0].body, "secret body");
}
