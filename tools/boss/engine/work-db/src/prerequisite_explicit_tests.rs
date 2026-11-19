use super::*;

const URL: &str = "https://github.com/acme/widget/pull/9";

fn store_question(db: &WorkDb, asking: &Task, q: &OperatorQuestion) {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&asking.id)
                .kind(ExecutionKind::ChoreImplementation)
                .repo_remote_url(crate::test_support::TEST_REPO_REMOTE_URL)
                .build(),
        )
        .unwrap();
    let conn = db.connect().unwrap();
    crate::pr_flow::insert_question(&conn, &execution.id, "prop_explicit", q).unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'awaiting_operator_answer' WHERE id = ?1",
        [&asking.id],
    )
    .unwrap();
}

fn task_count(db: &WorkDb) -> i64 {
    db.connect()
        .unwrap()
        .query_row("SELECT count(*) FROM tasks", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn explicit_create_rejects_invalid_owners_without_name_fallback() {
    for state in [
        "missing",
        "deleted",
        "done",
        "archived",
        "foreign",
        "self",
        "cycle",
        "malformed",
        "ambiguous",
    ] {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
        let product_id = if state == "foreign" {
            crate::test_support::create_test_product(&db).id
        } else {
            product.id
        };
        let owner = crate::test_support::create_test_chore(&db, product_id, "Land wiring");
        if state != "missing" {
            bind_pr(&db, if state == "self" { &asking.id } else { &owner.id }, URL);
        }
        if state == "ambiguous" {
            let duplicate = crate::test_support::create_test_chore(&db, owner.product_id.clone(), "Another owner");
            bind_pr(&db, &duplicate.id, URL);
        }
        let conn = db.connect().unwrap();
        match state {
            "deleted" => {
                conn.execute("UPDATE tasks SET deleted_at = '1' WHERE id = ?1", [&owner.id])
                    .unwrap();
            }
            "done" | "archived" => {
                conn.execute("UPDATE tasks SET status = ?2 WHERE id = ?1", params![owner.id, state])
                    .unwrap();
            }
            "cycle" => {
                deps::insert_edge(&conn, &owner.id, &asking.id, RELATION_BLOCKS, "1").unwrap();
            }
            _ => {}
        }
        let asking = query_task(&conn, &asking.id).unwrap().unwrap();
        let q = create_pr(if state == "malformed" { "not a PR" } else { URL });
        assert!(!rejected(&conn, Some(&asking), &q).is_empty(), "{state}");
        drop(conn);
        // Older stored create questions must apply the same validation at Yes.
        store_question(&db, &asking, &q);
        let before = task_count(&db);
        assert!(
            db.answer_operator_question(&asking.id, boss_protocol::OperatorAnswer::YesNo { value: true })
                .is_err(),
            "{state}"
        );
        assert_eq!(task_count(&db), before, "{state}");
        assert_eq!(open_question_count(&db, &asking.id), 1, "{state}");
        assert!(
            deps::prerequisites_of(&db.connect().unwrap(), &asking.id, Some(RELATION_BLOCKS))
                .unwrap()
                .is_empty(),
            "{state}"
        );
    }
}

#[test]
fn stored_create_links_by_pr_across_projects_despite_a_different_name() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let owner = crate::test_support::create_test_chore(&db, product.id.clone(), "Actual PR owner");
    for task in [&asking, &owner] {
        let project = db
            .create_project(
                boss_protocol::CreateProjectInput::builder()
                    .product_id(&product.id)
                    .name(&task.name)
                    .no_design_task(true)
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET project_id = ?2 WHERE id = ?1",
                params![task.id, project.id],
            )
            .unwrap();
    }
    bind_pr(&db, &owner.id, URL);
    let q = create_pr(URL);
    assert_eq!(stored_owner_id(ready(&db.connect().unwrap(), &asking, &q)), owner.id);
    store_question(&db, &asking, &q);
    let before = task_count(&db);
    db.answer_operator_question(&asking.id, boss_protocol::OperatorAnswer::YesNo { value: true })
        .unwrap();
    assert_eq!(task_count(&db), before);
    assert_parked_behind(&db, &asking.id, &owner.id);
    assert!(db.gating_prereqs_for(&asking.id).unwrap().contains(&owner.id));
    // The proposal uses the ordinary blocks path; only the merge observer's
    // confirmed current binding satisfies this PR-backed prerequisite.
    db.observe_chore_pr_merged(&owner.id, "https://github.com/acme/widget/pull/10")
        .unwrap();
    assert_parked_behind(&db, &asking.id, &owner.id);
    db.observe_chore_pr_merged(&owner.id, URL).unwrap();
    assert!(db.gating_prereqs_for(&asking.id).unwrap().is_empty());
    let resumed = query_task(&db.connect().unwrap(), &asking.id).unwrap().unwrap();
    assert_ne!(resumed.blocked_reason.as_deref(), Some("dependency"));
}

#[test]
fn answering_link_rejects_a_changed_pr_binding_and_rolls_back() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let owner = crate::test_support::create_test_chore(&db, product.id.clone(), "Original owner");
    bind_pr(&db, &owner.id, URL);
    let q = ready(&db.connect().unwrap(), &asking, &create_pr(URL));
    store_question(&db, &asking, &q);
    bind_pr(&db, &owner.id, "https://github.com/acme/widget/pull/10");
    let replacement = crate::test_support::create_test_chore(&db, product.id, "Replacement owner");
    bind_pr(&db, &replacement.id, URL);
    let err = db
        .answer_operator_question(&asking.id, boss_protocol::OperatorAnswer::YesNo { value: true })
        .unwrap_err();
    assert!(err.to_string().contains("owner changed"), "{err}");
    assert_eq!(open_question_count(&db, &asking.id), 1);
    assert!(
        deps::prerequisites_of(&db.connect().unwrap(), &asking.id, Some(RELATION_BLOCKS))
            .unwrap()
            .is_empty()
    );
}
