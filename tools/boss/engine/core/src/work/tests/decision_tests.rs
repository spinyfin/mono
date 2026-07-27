//! Product decision records (`product_decisions`): create / list / get /
//! revoke / supersede, short-id allocation, and schema presence.

use super::*;
use boss_protocol::{CreateDecisionInput, DecisionKind, DecisionStatus};

fn create_decision(db: &WorkDb, product_id: &str, kind: DecisionKind, title: &str, body: &str) -> Decision {
    db.create_decision(
        CreateDecisionInput::builder()
            .product_id(product_id)
            .kind(kind)
            .title(title)
            .body(body)
            .created_by("human")
            .created_via("test")
            .build(),
    )
    .unwrap()
}

#[test]
fn create_list_get_decision_round_trip() {
    let path = temp_db_path("decision-roundtrip");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));

    let d1 = create_decision(
        &db,
        &product.id,
        DecisionKind::Wontfix,
        "No checkleft all-gating",
        "We considered all-gating and declined for now.",
    );
    assert_eq!(d1.short_id, Some(1));
    assert_eq!(d1.status, DecisionStatus::Active);
    assert_eq!(d1.kind, DecisionKind::Wontfix);
    assert_eq!(d1.created_via, "test");
    assert_eq!(d1.display_label(), "D1");

    let d2 = create_decision(
        &db,
        &product.id,
        DecisionKind::Decided,
        "Remote is the plan",
        "Local concurrency ceiling stands; remote workers are the scale path.",
    );
    assert_eq!(d2.short_id, Some(2));

    let active = db.list_decisions(&product.id, false).unwrap();
    assert_eq!(active.len(), 2);
    // Newest first.
    assert_eq!(active[0].id, d2.id);
    assert_eq!(active[1].id, d1.id);

    let got = db.get_decision(&d1.id).unwrap().expect("d1 present");
    assert_eq!(got.title, "No checkleft all-gating");

    let by_short = db
        .get_decision_by_short_id(&product.id, 2)
        .unwrap()
        .expect("D2 present");
    assert_eq!(by_short.id, d2.id);
}

#[test]
fn list_decisions_hides_inactive_unless_requested() {
    let path = temp_db_path("decision-list-inactive");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));

    let active = create_decision(&db, &product.id, DecisionKind::Decided, "Keep", "keep body");
    let revoked = create_decision(&db, &product.id, DecisionKind::Wontfix, "Drop", "drop body");
    db.revoke_decision(&revoked.id).unwrap();

    let listed = db.list_decisions(&product.id, false).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, active.id);

    let all = db.list_decisions(&product.id, true).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn revoke_decision_is_idempotent_and_blocks_superseded() {
    let path = temp_db_path("decision-revoke");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));

    let d = create_decision(&db, &product.id, DecisionKind::Wontfix, "Title", "Body");
    let once = db.revoke_decision(&d.id).unwrap();
    assert_eq!(once.status, DecisionStatus::Revoked);
    let twice = db.revoke_decision(&d.id).unwrap();
    assert_eq!(twice.status, DecisionStatus::Revoked);

    let pred = create_decision(&db, &product.id, DecisionKind::Decided, "Old", "old");
    let succ = create_decision(&db, &product.id, DecisionKind::Decided, "New", "new");
    db.supersede_decision(&pred.id, &succ.id).unwrap();
    let err = db.revoke_decision(&pred.id).unwrap_err();
    assert!(
        err.to_string().contains("superseded"),
        "expected superseded error, got {err}"
    );
}

#[test]
fn supersede_decision_links_successor_and_validates() {
    let path = temp_db_path("decision-supersede");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));
    let other = create_test_product_with_repo(&db, "Flunge", Some("git@example.com:flunge.git"));

    let pred = create_decision(&db, &product.id, DecisionKind::Decided, "Old plan", "old");
    let succ = create_decision(&db, &product.id, DecisionKind::Decided, "New plan", "new");
    let foreign = create_decision(&db, &other.id, DecisionKind::Decided, "Other", "other");

    let updated = db.supersede_decision(&pred.id, &succ.id).unwrap();
    assert_eq!(updated.status, DecisionStatus::Superseded);
    assert_eq!(updated.superseded_by.as_deref(), Some(succ.id.as_str()));

    let err_self = db.supersede_decision(&succ.id, &succ.id).unwrap_err();
    assert!(err_self.to_string().contains("itself"));

    let err_foreign = db.supersede_decision(&succ.id, &foreign.id).unwrap_err();
    assert!(err_foreign.to_string().contains("same product"));

    let err_inactive = db.supersede_decision(&pred.id, &succ.id).unwrap_err();
    assert!(err_inactive.to_string().contains("active"));
}

#[test]
fn create_decision_rejects_empty_fields_and_unknown_related() {
    let path = temp_db_path("decision-validate");
    let db = WorkDb::open(path).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@example.com:boss.git"));

    let empty_title = db.create_decision(
        CreateDecisionInput::builder()
            .product_id(&product.id)
            .kind(DecisionKind::Wontfix)
            .title("  ")
            .body("body")
            .created_by("human")
            .build(),
    );
    assert!(empty_title.is_err());

    let bad_related = db.create_decision(
        CreateDecisionInput::builder()
            .product_id(&product.id)
            .kind(DecisionKind::Wontfix)
            .title("t")
            .body("b")
            .created_by("human")
            .related_work_item_id("task_does_not_exist")
            .build(),
    );
    assert!(
        bad_related.unwrap_err().to_string().contains("unknown work item"),
        "related work item must be validated"
    );
}

#[test]
fn product_decisions_table_present_on_fresh_and_migrated_db() {
    let path = temp_db_path("decision-schema");
    let db = WorkDb::open(path.clone()).unwrap();
    {
        let conn = db.connect().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'product_decisions')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists, "fresh db must have product_decisions");
        let seq: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'decision_short_id_sequences')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(seq, "fresh db must have decision_short_id_sequences");
    }

    // Upgrade path: drop the tables and re-open so the incremental
    // migration re-creates them without touching tasks columns.
    {
        let conn = db.connect().unwrap();
        conn.execute_batch(
            "DROP TABLE product_decisions;
             DROP TABLE decision_short_id_sequences;",
        )
        .unwrap();
    }
    drop(db);
    let db = WorkDb::open(path).unwrap();
    let conn = db.connect().unwrap();
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'product_decisions')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(exists, "re-open must re-create product_decisions");
    // Effort provenance columns must still be present — no collision.
    assert!(table_has_column(&conn, "tasks", "effort_matched_rule").unwrap());
    assert!(table_has_column(&conn, "tasks", "effort_reasons").unwrap());
    assert!(table_has_column(&conn, "tasks", "blocked_detail").unwrap());
}
