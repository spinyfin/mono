use super::*;
use boss_protocol::OperatorQuestion;

#[path = "prerequisite_explicit_tests.rs"]
mod explicit;

fn question(answer_type: OperatorAnswerType) -> OperatorQuestion {
    OperatorQuestion {
        text: "placeholder".into(),
        answer_type,
        explanation: "The other change is already in review.".into(),
    }
}

fn bind_pr(db: &WorkDb, task_id: &str, url: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = ?2 WHERE id = ?1",
            rusqlite::params![task_id, url],
        )
        .unwrap();
}

#[test]
fn create_question_rewrites_to_link_when_the_named_pr_is_bound() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
    let url = "https://github.com/acme/widget/pull/9";
    bind_pr(&db, &owner.id, url);
    let create = question(OperatorAnswerType::CreatePrerequisiteTask {
        name: "Land the wiring".into(),
        brief: format!("Finish {url} on main."),
        file_count_bypass: None,
        option_labels: None,
        existing_id: None,
        existing_pr: None,
    });
    let conn = db.connect().unwrap();
    let ResolvedQuestion::Ready(stored) = resolve_stored_question(&conn, Some(&asking), &create).unwrap() else {
        panic!("expected a stored question");
    };
    match stored.answer_type {
        OperatorAnswerType::LinkPrerequisiteTask {
            work_item_id,
            name,
            pr_url,
            ..
        } => {
            assert_eq!(work_item_id, owner.id);
            assert_eq!(name, "Land the wiring");
            assert_eq!(pr_url.as_deref(), Some(url));
        }
        other => panic!("expected link form, got {other:?}"),
    }
    assert_eq!(
        stored.text,
        "Wait for 'Land the wiring' (acme/widget#9)? This task will wait for it."
    );
}

#[test]
fn link_question_resolves_from_pr_url_and_rejects_an_unbound_pr() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
    let url = "https://github.com/acme/widget/pull/9";
    bind_pr(&db, &owner.id, url);
    let link = question(OperatorAnswerType::LinkPrerequisiteTask {
        existing_id: None,
        existing_pr: Some(url.into()),
        work_item_id: String::new(),
        name: String::new(),
        pr_url: None,
    });
    let conn = db.connect().unwrap();
    let ResolvedQuestion::Ready(stored) = resolve_stored_question(&conn, Some(&asking), &link).unwrap() else {
        panic!("expected a stored question");
    };
    let OperatorAnswerType::LinkPrerequisiteTask { work_item_id, .. } = stored.answer_type else {
        panic!("expected link form");
    };
    assert_eq!(work_item_id, owner.id);

    let missing = question(OperatorAnswerType::LinkPrerequisiteTask {
        existing_id: None,
        existing_pr: Some("https://github.com/acme/widget/pull/99".into()),
        work_item_id: String::new(),
        name: String::new(),
        pr_url: None,
    });
    let ResolvedQuestion::Rejected(reason) = resolve_stored_question(&conn, Some(&asking), &missing).unwrap() else {
        panic!("expected a rejection");
    };
    assert!(reason.contains("no open task owns the pull request"), "{reason}");
}

#[test]
fn link_question_resolves_from_canonical_id_and_never_by_name() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
    let conn = db.connect().unwrap();
    let by_id = question(OperatorAnswerType::LinkPrerequisiteTask {
        existing_id: Some(owner.id.clone()),
        existing_pr: None,
        work_item_id: String::new(),
        name: String::new(),
        pr_url: None,
    });
    let ResolvedQuestion::Ready(stored) = resolve_stored_question(&conn, Some(&asking), &by_id).unwrap() else {
        panic!("expected a stored question");
    };
    let OperatorAnswerType::LinkPrerequisiteTask { work_item_id, name, .. } = stored.answer_type else {
        panic!("expected link form");
    };
    assert_eq!(work_item_id, owner.id);
    assert_eq!(name, "Land the wiring");

    let by_name = question(OperatorAnswerType::LinkPrerequisiteTask {
        existing_id: Some("Land the wiring".into()),
        existing_pr: None,
        work_item_id: String::new(),
        name: String::new(),
        pr_url: None,
    });
    let ResolvedQuestion::Rejected(reason) = resolve_stored_question(&conn, Some(&asking), &by_name).unwrap() else {
        panic!("expected a rejection");
    };
    assert!(reason.contains("no open task matches"), "{reason}");
}

fn link_pr(existing_pr: &str) -> OperatorQuestion {
    question(OperatorAnswerType::LinkPrerequisiteTask {
        existing_id: None,
        existing_pr: Some(existing_pr.into()),
        work_item_id: String::new(),
        name: String::new(),
        pr_url: None,
    })
}

fn link_id(existing_id: &str) -> OperatorQuestion {
    question(OperatorAnswerType::LinkPrerequisiteTask {
        existing_id: Some(existing_id.into()),
        existing_pr: None,
        work_item_id: String::new(),
        name: String::new(),
        pr_url: None,
    })
}

fn create_with_text(name: &str, brief: &str, explanation: &str) -> OperatorQuestion {
    OperatorQuestion {
        text: "placeholder".into(),
        answer_type: OperatorAnswerType::CreatePrerequisiteTask {
            name: name.into(),
            brief: brief.into(),
            file_count_bypass: None,
            option_labels: None,
            existing_id: None,
            existing_pr: None,
        },
        explanation: explanation.into(),
    }
}

fn set_repo(db: &WorkDb, task_id: &str, url: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET repo_remote_url = ?2 WHERE id = ?1",
            rusqlite::params![task_id, url],
        )
        .unwrap();
}

fn insert_revision(db: &WorkDb, product_id: &str, parent_task_id: &str) -> Task {
    let conn = db.connect().unwrap();
    let id = next_id("task");
    let now = now_string();
    conn.execute(
        "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
         VALUES (?1, ?2, 'revision', 'Follow-on change', '', 'todo', ?3, ?3, ?4)",
        rusqlite::params![id, product_id, now, parent_task_id],
    )
    .unwrap();
    query_task(&conn, &id).unwrap().expect("revision row")
}

fn stored_owner_id(stored: OperatorQuestion) -> String {
    match stored.answer_type {
        OperatorAnswerType::LinkPrerequisiteTask { work_item_id, .. } => work_item_id,
        other => panic!("expected link form, got {other:?}"),
    }
}

fn ready(conn: &Connection, asking: &Task, q: &OperatorQuestion) -> OperatorQuestion {
    match resolve_stored_question(conn, Some(asking), q).unwrap() {
        ResolvedQuestion::Ready(stored) => stored,
        ResolvedQuestion::Rejected(reason) => panic!("expected a stored question, got {reason}"),
    }
}

fn rejected(conn: &Connection, asking: Option<&Task>, q: &OperatorQuestion) -> String {
    match resolve_stored_question(conn, asking, q).unwrap() {
        ResolvedQuestion::Rejected(reason) => reason,
        ResolvedQuestion::Ready(stored) => panic!("expected a rejection, got {stored:?}"),
    }
}

#[test]
fn qualified_pr_does_not_bind_a_same_numbered_pr_in_another_repo() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    set_repo(&db, &asking.id, "git@github.com:acme/widget.git");
    let local = crate::test_support::create_test_chore(&db, product.id.clone(), "Local wiring");
    let foreign = crate::test_support::create_test_chore(&db, product.id, "Foreign wiring");
    bind_pr(&db, &local.id, "https://github.com/acme/widget/pull/9");
    bind_pr(&db, &foreign.id, "https://github.com/other/repo/pull/9");
    let asking = query_task(&db.connect().unwrap(), &asking.id).unwrap().unwrap();
    let conn = db.connect().unwrap();

    assert_eq!(
        stored_owner_id(ready(&conn, &asking, &link_pr("https://github.com/other/repo/pull/9"))),
        foreign.id
    );
    assert_eq!(
        stored_owner_id(ready(&conn, &asking, &link_pr("other/repo#9"))),
        foreign.id
    );
    let reason = rejected(&conn, Some(&asking), &link_pr("https://github.com/unbound/repo/pull/9"));
    assert!(reason.contains("no open task owns the pull request"), "{reason}");
}

#[test]
fn decorated_stored_pr_url_still_matches_the_qualified_selector() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
    bind_pr(&db, &owner.id, "https://github.com/Acme/Widget/pull/9/files");
    assert_eq!(
        db.other_work_item_bound_to_pr("https://github.com/acme/widget/pull/9", &asking.id)
            .unwrap(),
        Some(owner.id.clone())
    );
    let conn = db.connect().unwrap();
    assert_eq!(
        stored_owner_id(ready(&conn, &asking, &link_pr("https://github.com/acme/widget/pull/9"))),
        owner.id
    );
    assert_eq!(
        stored_owner_id(ready(&conn, &asking, &link_pr("acme/widget#9"))),
        owner.id
    );
}

#[test]
fn create_brief_citing_a_foreign_repo_pr_stays_a_create_question() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
    bind_pr(&db, &owner.id, "https://github.com/spinyfin/mono/pull/3639");
    let create = create_with_text(
        "Land hook wiring",
        "Finish rust-lang/rust#3639 on main.",
        "The other change is already in review.",
    );
    let conn = db.connect().unwrap();
    match ready(&conn, &asking, &create).answer_type {
        OperatorAnswerType::CreatePrerequisiteTask { .. } => {}
        other => panic!("expected create form, got {other:?}"),
    }
}

#[test]
fn bare_number_prefers_the_asking_repo_and_owner_repo_form_resolves() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    set_repo(&db, &asking.id, "git@github.com:acme/widget.git");
    let local = crate::test_support::create_test_chore(&db, product.id.clone(), "Local wiring");
    let foreign = crate::test_support::create_test_chore(&db, product.id, "Foreign wiring");
    bind_pr(&db, &local.id, "https://github.com/acme/widget/pull/9");
    bind_pr(&db, &foreign.id, "https://github.com/other/repo/pull/9");
    let asking = query_task(&db.connect().unwrap(), &asking.id).unwrap().unwrap();
    let conn = db.connect().unwrap();
    assert_eq!(stored_owner_id(ready(&conn, &asking, &link_pr("9"))), local.id);
    assert_eq!(stored_owner_id(ready(&conn, &asking, &link_pr("#9"))), local.id);
    assert_eq!(
        stored_owner_id(ready(&conn, &asking, &link_pr("acme/widget#9"))),
        local.id
    );
}

#[test]
fn friendly_id_resolves_and_a_revision_id_maps_to_its_chain_root() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let owner = crate::test_support::create_test_chore(&db, product.id.clone(), "Land the wiring");
    let revision = insert_revision(&db, &product.id, &owner.id);
    let conn = db.connect().unwrap();
    let short_id: i64 = conn
        .query_row("SELECT short_id FROM tasks WHERE id = ?1", [&owner.id], |row| {
            row.get(0)
        })
        .unwrap();
    let friendly = format!("T{short_id}");
    assert_eq!(stored_owner_id(ready(&conn, &asking, &link_id(&friendly))), owner.id);
    assert_eq!(stored_owner_id(ready(&conn, &asking, &link_id(&revision.id))), owner.id);
}

#[test]
fn id_and_pr_that_name_different_tasks_are_rejected_on_create_and_link() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let by_id = crate::test_support::create_test_chore(&db, product.id.clone(), "By id");
    let by_pr = crate::test_support::create_test_chore(&db, product.id, "By pr");
    let url = "https://github.com/acme/widget/pull/9";
    bind_pr(&db, &by_pr.id, url);
    let conn = db.connect().unwrap();
    let link = question(OperatorAnswerType::LinkPrerequisiteTask {
        existing_id: Some(by_id.id.clone()),
        existing_pr: Some(url.into()),
        work_item_id: String::new(),
        name: String::new(),
        pr_url: None,
    });
    let reason = rejected(&conn, Some(&asking), &link);
    assert!(reason.contains("different open tasks"), "{reason}");

    let create = question(OperatorAnswerType::CreatePrerequisiteTask {
        name: "Land hook wiring".into(),
        brief: "Finish the other change on main.".into(),
        file_count_bypass: None,
        option_labels: None,
        existing_id: Some(by_id.id.clone()),
        existing_pr: Some(url.into()),
    });
    let reason = rejected(&conn, Some(&asking), &create);
    assert!(reason.contains("different open tasks"), "{reason}");
}

#[test]
fn finished_task_is_rejected_on_a_link_and_leaves_create_as_create() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
    let url = "https://github.com/acme/widget/pull/9";
    bind_pr(&db, &owner.id, url);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&owner.id])
        .unwrap();
    let conn = db.connect().unwrap();
    let reason = rejected(&conn, Some(&asking), &link_id(&owner.id));
    assert!(reason.contains("already finished"), "{reason}");
    match ready(
        &conn,
        &asking,
        &create_with_text("Land hook wiring", &format!("Finish {url} on main."), "Need it."),
    )
    .answer_type
    {
        OperatorAnswerType::CreatePrerequisiteTask { .. } => {}
        other => panic!("expected create form, got {other:?}"),
    }
}

#[test]
fn cycle_on_an_existing_owner_rejects_create_instead_of_offering_a_duplicate() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
    let url = "https://github.com/acme/widget/pull/9";
    bind_pr(&db, &owner.id, url);
    deps::insert_edge(&db.connect().unwrap(), &owner.id, &asking.id, RELATION_BLOCKS, "1").unwrap();
    let conn = db.connect().unwrap();
    let create = question(OperatorAnswerType::CreatePrerequisiteTask {
        name: "Land the wiring".into(),
        brief: format!("Finish {url} on main."),
        file_count_bypass: None,
        option_labels: None,
        existing_id: Some(owner.id.clone()),
        existing_pr: None,
    });
    let reason = rejected(&conn, Some(&asking), &create);
    assert!(reason.contains("cycle of waiting tasks"), "{reason}");
}

#[test]
fn revision_cannot_wait_on_its_own_chain_root() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let root = crate::test_support::create_test_chore(&db, product.id.clone(), "Land the wiring");
    let url = "https://github.com/acme/widget/pull/9";
    bind_pr(&db, &root.id, url);
    let asking = insert_revision(&db, &product.id, &root.id);
    let conn = db.connect().unwrap();
    let reason = rejected(&conn, Some(&asking), &link_pr(url));
    assert!(reason.contains("already revising"), "{reason}");
    let reason = rejected(&conn, Some(&asking), &link_id(&root.id));
    assert!(reason.contains("already revising"), "{reason}");
    match ready(
        &conn,
        &asking,
        &create_with_text("Follow-on work", &format!("Finish {url} on main."), "Need it."),
    )
    .answer_type
    {
        OperatorAnswerType::CreatePrerequisiteTask { .. } => {}
        other => panic!("expected create form after dropping the chain-root PR, got {other:?}"),
    }
}

#[test]
fn slug_pr_at_survives_multibyte_delimiters() {
    let mut found = Vec::new();
    scan_pr_refs("Finish “acme/widget#9” next.", &mut found);
    scan_pr_refs("See—acme/widget#9 for the rest.", &mut found);
    scan_pr_refs("See\u{00a0}acme/widget#9 after that.", &mut found);
    scan_pr_refs("Finish acme/widget #9 on main.", &mut found);
    found.sort();
    found.dedup();
    assert_eq!(found, vec!["https://github.com/acme/widget/pull/9".to_owned()]);
}

#[test]
fn link_question_without_an_asking_task_is_rejected() {
    let q = link_pr("https://github.com/acme/widget/pull/9");
    let (_tmp, db) = crate::test_support::open_db();
    let conn = db.connect().unwrap();
    let reason = rejected(&conn, None, &q);
    assert!(reason.contains("can only be asked from a task"), "{reason}");
}
fn create_pr(pr: &str) -> OperatorQuestion {
    let mut q = create_with_text("Land wiring", "Finish the wiring.", "Need it.");
    if let OperatorAnswerType::CreatePrerequisiteTask { existing_pr, .. } = &mut q.answer_type {
        *existing_pr = Some(pr.into());
    }
    q
}

#[test]
fn ambiguous_owners_reject_explicit_and_scanned_create_and_link_questions() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let url = "https://github.com/acme/widget/pull/9";
    for name in ["First owner", "Second owner"] {
        let owner = crate::test_support::create_test_chore(&db, product.id.clone(), name);
        bind_pr(&db, &owner.id, url);
    }
    let conn = db.connect().unwrap();
    for q in [
        create_pr(url),
        create_with_text("Land wiring", &format!("Finish {url}."), "Need it."),
        link_pr(url),
    ] {
        let reason = rejected(&conn, Some(&asking), &q);
        assert!(reason.contains("more than one open task"), "{reason}");
    }
    let scanned = rejected(
        &conn,
        Some(&asking),
        &create_with_text("Land wiring", &format!("Finish {url}."), "Need it."),
    );
    assert!(scanned.contains("Cited PR acme/widget#9:"), "{scanned}");
    // With a known asking repository, ambiguity is still scoped to that repository.
    drop(conn);
    set_repo(&db, &asking.id, "git@github.com:acme/widget.git");
    let conn = db.connect().unwrap();
    let asking = query_task(&conn, &asking.id).unwrap().unwrap();
    for q in [link_pr("9"), create_pr("9")] {
        let reason = rejected(&conn, Some(&asking), &q);
        assert!(reason.contains("more than one open task"), "{reason}");
    }
    assert!(rejected(&conn, Some(&asking), &link_pr("9")).contains("in this repository"));
}

#[test]
fn bare_number_without_a_local_owner_never_binds_a_foreign_owner() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    set_repo(&db, &asking.id, "git@github.com:acme/widget.git");
    let owner = crate::test_support::create_test_chore(&db, product.id, "Foreign wiring");
    bind_pr(&db, &owner.id, "https://github.com/other/repo/pull/9");
    let conn = db.connect().unwrap();
    let asking = query_task(&conn, &asking.id).unwrap().unwrap();
    assert!(rejected(&conn, Some(&asking), &link_pr("9")).contains("no open task owns"));
    assert!(rejected(&conn, Some(&asking), &create_pr("9")).contains("no open task owns"));
}

#[test]
fn mixed_case_scanned_references_resolve_to_one_pr() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let owner = crate::test_support::create_test_chore(&db, product.id, "Land wiring");
    bind_pr(&db, &owner.id, "https://github.com/acme/widget/pull/9");
    let q = create_with_text(
        "Land wiring",
        "Finish https://github.com/Acme/Widget/pull/9 and acme/widget#9.",
        "Need it.",
    );
    assert_eq!(stored_owner_id(ready(&db.connect().unwrap(), &asking, &q)), owner.id);
}

#[test]
fn finished_historical_owners_do_not_make_an_open_owner_ambiguous() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let url = "https://github.com/acme/widget/pull/9";
    for status in ["done", "archived"] {
        let owner = crate::test_support::create_test_chore(&db, product.id.clone(), status);
        bind_pr(&db, &owner.id, url);
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET status = ?2 WHERE id = ?1", params![owner.id, status])
            .unwrap();
    }
    let owner = crate::test_support::create_test_chore(&db, product.id, "Open owner");
    bind_pr(&db, &owner.id, url);
    set_repo(&db, &asking.id, "git@github.com:acme/widget.git");
    let conn = db.connect().unwrap();
    let asking = query_task(&conn, &asking.id).unwrap().unwrap();
    for q in [
        link_pr(url),
        link_pr("9"),
        create_pr(url),
        create_with_text("Land wiring", &format!("Finish {url}."), "Need it."),
    ] {
        assert_eq!(stored_owner_id(ready(&conn, &asking, &q)), owner.id);
    }
    conn.execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&owner.id])
        .unwrap();
    assert!(rejected(&conn, Some(&asking), &link_pr(url)).contains("already finished"));
}

#[test]
fn scanned_cycle_rejection_identifies_the_pr_and_owner() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let owner = crate::test_support::create_test_chore(&db, product.id, "Land wiring");
    let url = "https://github.com/acme/widget/pull/9";
    bind_pr(&db, &owner.id, url);
    let conn = db.connect().unwrap();
    deps::insert_edge(&conn, &owner.id, &asking.id, RELATION_BLOCKS, "1").unwrap();
    let reason = rejected(
        &conn,
        Some(&asking),
        &create_with_text("Land wiring", &format!("Finish {url}."), "Need it."),
    );
    assert!(reason.contains("Cited PR acme/widget#9:"), "{reason}");
    assert!(reason.contains(&owner.name) && reason.contains(&owner.id), "{reason}");
}

#[test]
fn answering_a_stored_link_rechecks_chain_and_owner_liveness_without_adding_edges() {
    for state in ["same_chain", "done", "archived", "deleted"] {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let owner = crate::test_support::create_test_chore(&db, product.id.clone(), "Land wiring");
        let asking = insert_revision(&db, &product.id, &owner.id);
        let target = if state == "same_chain" {
            owner
        } else {
            crate::test_support::create_test_chore(&db, product.id, "Other owner")
        };
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .created_via("test")
                    .work_item_id(&asking.id)
                    .kind(ExecutionKind::RevisionImplementation)
                    .repo_remote_url("git@github.com:acme/widget.git")
                    .build(),
            )
            .unwrap();
        let mut q = link_id(&target.id);
        if let OperatorAnswerType::LinkPrerequisiteTask { work_item_id, .. } = &mut q.answer_type {
            *work_item_id = target.id.clone();
        }
        let conn = db.connect().unwrap();
        crate::pr_flow::insert_question(&conn, &execution.id, "prop_test", &q).unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'awaiting_operator_answer' WHERE id = ?1",
            [&asking.id],
        )
        .unwrap();
        if state == "deleted" {
            conn.execute("UPDATE tasks SET deleted_at = '1' WHERE id = ?1", [&target.id])
                .unwrap();
        } else if state != "same_chain" {
            conn.execute("UPDATE tasks SET status = ?2 WHERE id = ?1", params![target.id, state])
                .unwrap();
        }
        drop(conn);
        let err = db
            .answer_operator_question(&asking.id, boss_protocol::OperatorAnswer::YesNo { value: true })
            .unwrap_err()
            .to_string();
        let expected = match state {
            "same_chain" => SAME_CHAIN_REJECTION,
            "deleted" => "no longer exists",
            _ => "already finished",
        };
        assert!(err.contains(expected), "{state}: {err}");
        let conn = db.connect().unwrap();
        assert!(
            deps::prerequisites_of(&conn, &asking.id, Some(RELATION_BLOCKS))
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn bare_number_without_an_asking_repo_requires_a_unique_open_owner() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    for (name, url) in [
        ("Local wiring", "https://github.com/acme/widget/pull/9"),
        ("Foreign wiring", "https://github.com/other/repo/pull/9"),
    ] {
        let owner = crate::test_support::create_test_chore(&db, product.id.clone(), name);
        bind_pr(&db, &owner.id, url);
    }
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE products SET repo_remote_url = NULL WHERE id = ?1",
        [&product.id],
    )
    .unwrap();
    assert!(asking.repo_remote_url.is_none());
    assert!(resolve_repo_for_work_item(&conn, &asking.id).unwrap().is_none());
    for q in [link_pr("9"), create_pr("9")] {
        let reason = rejected(&conn, Some(&asking), &q);
        assert!(reason.contains("pass the full GitHub URL"), "{reason}");
    }
}

fn yes_on_stored_link(db: &WorkDb, asking: &Task, target: &Task) {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(&asking.id)
                .kind(ExecutionKind::ChoreImplementation)
                .repo_remote_url(crate::test_support::TEST_REPO_REMOTE_URL)
                .build(),
        )
        .unwrap();
    let mut q = link_id(&target.id);
    if let OperatorAnswerType::LinkPrerequisiteTask { work_item_id, .. } = &mut q.answer_type {
        *work_item_id = target.id.clone();
    }
    let conn = db.connect().unwrap();
    crate::pr_flow::insert_question(&conn, &execution.id, "prop_link", &q).unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'awaiting_operator_answer' WHERE id = ?1",
        [&asking.id],
    )
    .unwrap();
    drop(conn);
    db.answer_operator_question(&asking.id, boss_protocol::OperatorAnswer::YesNo { value: true })
        .unwrap();
}

fn assert_parked_behind(db: &WorkDb, asking_id: &str, target_id: &str) {
    let conn = db.connect().unwrap();
    let asking = query_task(&conn, asking_id).unwrap().unwrap();
    assert_eq!(asking.status, TaskStatus::Blocked);
    assert_eq!(asking.blocked_reason.as_deref(), Some("dependency"));
    let prereqs = deps::prerequisites_of(&conn, asking_id, Some(RELATION_BLOCKS)).unwrap();
    assert_eq!(prereqs.len(), 1);
    assert_eq!(prereqs[0].prerequisite_id, target_id);
}

fn open_question_count(db: &WorkDb, work_item_id: &str) -> usize {
    db.list_operator_questions(work_item_id)
        .unwrap()
        .into_iter()
        .filter(|q| q.status == boss_protocol::OperatorQuestionStatus::Open)
        .count()
}

fn ready_execution_count(db: &WorkDb, work_item_id: &str) -> usize {
    db.list_executions(Some(work_item_id))
        .unwrap()
        .into_iter()
        .filter(|e| e.status == ExecutionStatus::Ready)
        .count()
}

#[test]
fn linking_to_a_task_waiting_on_its_own_question_does_not_restart_it() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let target = crate::test_support::create_test_chore_manual(&db, product.id, "Shared fixture");
    let target_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(&target.id)
                .kind(ExecutionKind::ChoreImplementation)
                .repo_remote_url(crate::test_support::TEST_REPO_REMOTE_URL)
                .build(),
        )
        .unwrap();
    let target_q = OperatorQuestion {
        text: "Continue with the shared fixture?".into(),
        answer_type: OperatorAnswerType::YesNo,
        explanation: "Need a decision before this work can proceed.".into(),
    };
    let conn = db.connect().unwrap();
    crate::pr_flow::insert_question(&conn, &target_exec.id, "prop_target", &target_q).unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'failed', finished_at = ?2 WHERE id = ?1",
        rusqlite::params![target_exec.id, now_string()],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'blocked', blocked_reason = 'awaiting_operator_answer' WHERE id = ?1",
        [&target.id],
    )
    .unwrap();
    drop(conn);
    assert_eq!(open_question_count(&db, &target.id), 1);
    yes_on_stored_link(&db, &asking, &target);
    assert_parked_behind(&db, &asking.id, &target.id);
    assert_eq!(open_question_count(&db, &target.id), 1);
    let target = query_task(&db.connect().unwrap(), &target.id).unwrap().unwrap();
    assert_eq!(target.status, TaskStatus::Blocked);
    assert_eq!(target.blocked_reason.as_deref(), Some("awaiting_operator_answer"));
    assert_eq!(ready_execution_count(&db, &target.id), 0);
}

#[test]
fn linking_to_question_dismissed_human_driven_or_deferred_is_wait_only() {
    for state in ["question_dismissed", "human_driven", "deferred"] {
        let (_tmp, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
        let target = crate::test_support::create_test_chore_manual(&db, product.id, "Shared fixture");
        let conn = db.connect().unwrap();
        match state {
            "question_dismissed" => {
                conn.execute(
                    "UPDATE tasks SET status = 'blocked', blocked_reason = 'question_dismissed' WHERE id = ?1",
                    [&target.id],
                )
                .unwrap();
            }
            "human_driven" => {
                conn.execute("UPDATE tasks SET human_driven = 1 WHERE id = ?1", [&target.id])
                    .unwrap();
            }
            "deferred" => {
                conn.execute("UPDATE tasks SET deferred = 1 WHERE id = ?1", [&target.id])
                    .unwrap();
            }
            _ => unreachable!(),
        }
        drop(conn);
        yes_on_stored_link(&db, &asking, &target);
        assert_parked_behind(&db, &asking.id, &target.id);
        let target = query_task(&db.connect().unwrap(), &target.id).unwrap().unwrap();
        match state {
            "question_dismissed" => {
                assert_eq!(target.status, TaskStatus::Blocked);
                assert_eq!(target.blocked_reason.as_deref(), Some("question_dismissed"));
            }
            "human_driven" => {
                assert!(target.human_driven);
                assert_eq!(target.status, TaskStatus::Todo);
            }
            "deferred" => {
                assert!(target.deferred);
                assert_eq!(target.status, TaskStatus::Todo);
            }
            _ => unreachable!(),
        }
        assert!(!target.autostart);
        assert_eq!(ready_execution_count(&db, &target.id), 0);
        let asking = query_task(&db.connect().unwrap(), &asking.id).unwrap().unwrap();
        assert!(
            asking
                .description
                .contains("This task now waits for the named existing task.")
        );
        assert!(!asking.description.contains("has been scheduled for dispatch"));
    }
}

#[test]
fn linking_to_a_runnable_backlog_task_still_schedules_it() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let target = crate::test_support::create_test_chore_manual(&db, product.id, "Shared fixture");
    yes_on_stored_link(&db, &asking, &target);
    assert_parked_behind(&db, &asking.id, &target.id);
    let target = query_task(&db.connect().unwrap(), &target.id).unwrap().unwrap();
    assert!(target.autostart);
    assert_eq!(target.status, TaskStatus::Todo);
    assert_eq!(ready_execution_count(&db, &target.id), 1);
    let asking = query_task(&db.connect().unwrap(), &asking.id).unwrap().unwrap();
    assert!(asking.description.contains("has been scheduled for dispatch"));
}

#[test]
fn approval_correlates_autostart_enable_with_the_final_blocked_demotion() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let target = crate::test_support::create_test_chore_manual(&db, product.id, "Shared fixture");
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET autostart = 0 WHERE id = ?1", [&asking.id])
        .unwrap();
    yes_on_stored_link(&db, &asking, &target);
    assert_parked_behind(&db, &asking.id, &target.id);
    let rows = db.list_task_field_audit(&asking.id).unwrap();
    let autostart = rows
        .iter()
        .rev()
        .find(|row| row.property == "autostart" && row.new_value.as_deref() == Some("1"))
        .expect("autostart enable is recorded");
    let operation_id = autostart
        .operation_id
        .clone()
        .expect("autostart row carries an operation id");
    let blocked = rows
        .iter()
        .rev()
        .find(|row| row.property == "status" && row.new_value.as_deref() == Some("blocked"))
        .expect("the final blocked demotion is part of the history");
    assert_eq!(blocked.operation_id.as_deref(), Some(operation_id.as_str()));
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET autostart = 0 WHERE id = ?1", [&asking.id])
        .unwrap();
    let rows = db.list_task_field_audit(&asking.id).unwrap();
    let later = rows
        .iter()
        .rev()
        .find(|row| row.property == "autostart" && row.new_value.as_deref() == Some("0"))
        .expect("later write recorded");
    assert_ne!(later.operation_id.as_deref(), Some(operation_id.as_str()));
}

#[test]
fn inherited_product_repo_does_not_bind_a_foreign_owner_of_the_same_number() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let owner = crate::test_support::create_test_chore(&db, product.id, "Foreign wiring");
    bind_pr(&db, &owner.id, "https://github.com/other/repo/pull/9");
    let conn = db.connect().unwrap();
    assert!(asking.repo_remote_url.is_none());
    assert_eq!(
        resolve_repo_for_work_item(&conn, &asking.id).unwrap().as_deref(),
        Some(crate::test_support::TEST_REPO_REMOTE_URL)
    );
    assert!(rejected(&conn, Some(&asking), &link_pr("9")).contains("no open task owns"));
    assert!(rejected(&conn, Some(&asking), &create_pr("9")).contains("no open task owns"));
}

#[test]
fn two_products_sharing_a_pr_stay_in_their_own_product() {
    let (_tmp, db) = crate::test_support::open_db();
    let product_a = crate::test_support::create_test_product_named(&db, "Alpha");
    let product_b = crate::test_support::create_test_product_named(&db, "Beta");
    let asking = crate::test_support::create_test_chore(&db, product_a.id.clone(), "Startup check");
    let local = crate::test_support::create_test_chore(&db, product_a.id.clone(), "Local wiring");
    let foreign = crate::test_support::create_test_chore(&db, product_b.id, "Foreign wiring");
    let url = "https://github.com/acme/widget/pull/9";
    bind_pr(&db, &local.id, url);
    bind_pr(&db, &foreign.id, url);
    let conn = db.connect().unwrap();
    assert_eq!(stored_owner_id(ready(&conn, &asking, &link_pr(url))), local.id);
    assert_eq!(
        stored_owner_id(ready(
            &conn,
            &asking,
            &create_with_text("Land wiring", &format!("Finish {url}."), "Need it."),
        )),
        local.id
    );
    let reason = rejected(&conn, Some(&asking), &link_id(&foreign.id));
    assert!(reason.contains("belongs to another product"), "{reason}");
    assert!(reason.contains("Foreign wiring"), "{reason}");
}

#[test]
fn a_pr_owned_only_in_another_product_stays_a_create_and_rejects_an_explicit_link() {
    let (_tmp, db) = crate::test_support::open_db();
    let product_a = crate::test_support::create_test_product_named(&db, "Alpha");
    let product_b = crate::test_support::create_test_product_named(&db, "Beta");
    let asking = crate::test_support::create_test_chore(&db, product_a.id, "Startup check");
    let foreign = crate::test_support::create_test_chore(&db, product_b.id, "Foreign wiring");
    let url = "https://github.com/acme/widget/pull/9";
    bind_pr(&db, &foreign.id, url);
    let conn = db.connect().unwrap();
    match ready(
        &conn,
        &asking,
        &create_with_text("Land wiring", &format!("Finish {url}."), "Need it."),
    )
    .answer_type
    {
        OperatorAnswerType::CreatePrerequisiteTask { .. } => {}
        other => panic!("expected create form, got {other:?}"),
    }
    let reason = rejected(&conn, Some(&asking), &link_pr(url));
    assert!(reason.contains("belongs to another product"), "{reason}");
}

#[test]
fn create_brief_with_one_bound_and_one_unbound_pr_converts_to_the_bound_owner() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let owner = crate::test_support::create_test_chore(&db, product.id, "Land the wiring");
    bind_pr(&db, &owner.id, "https://github.com/spinyfin/mono/pull/3639");
    let q = create_with_text(
        "Finish the wiring",
        "Finish spinyfin/mono #3639; it builds on spinyfin/mono#3600, which merged.",
        "Details: see also rust-lang/rust#1.",
    );
    assert_eq!(stored_owner_id(ready(&db.connect().unwrap(), &asking, &q)), owner.id);
}

#[test]
fn create_brief_with_two_bound_prs_asks_for_an_explicit_prerequisite_pr() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let first = crate::test_support::create_test_chore(&db, product.id.clone(), "First wiring");
    let second = crate::test_support::create_test_chore(&db, product.id, "Second wiring");
    bind_pr(&db, &first.id, "https://github.com/acme/widget/pull/9");
    bind_pr(&db, &second.id, "https://github.com/acme/widget/pull/10");
    let reason = rejected(
        &db.connect().unwrap(),
        Some(&asking),
        &create_with_text(
            "Land wiring",
            "Finish acme/widget#9; it builds on acme/widget#10.",
            "Need it.",
        ),
    );
    assert!(reason.contains("more than one pull request"), "{reason}");
    assert!(reason.contains("--prerequisite-pr"), "{reason}");
}

#[test]
fn foreign_only_ambiguous_pr_holders_reject_only_explicit_create_questions() {
    let (_tmp, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product_named(&db, "Asking");
    let asking = crate::test_support::create_test_chore(&db, product.id.clone(), "Startup check");
    let url = "https://github.com/acme/widget/pull/9";
    for name in ["Alpha", "Beta"] {
        let foreign = crate::test_support::create_test_product_named(&db, name);
        let owner = crate::test_support::create_test_chore(&db, foreign.id, "Foreign wiring");
        bind_pr(&db, &owner.id, url);
    }
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE products SET repo_remote_url = NULL WHERE id = ?1",
        [&product.id],
    )
    .unwrap();
    assert!(resolve_repo_for_work_item(&conn, &asking.id).unwrap().is_none());
    let implicit = create_with_text("Land wiring", &format!("Finish {url}."), "Need it.");
    assert_eq!(ready(&conn, &asking, &implicit), implicit);
    for q in [create_pr(url), create_pr("acme/widget#9"), create_pr("9")] {
        assert!(rejected(&conn, Some(&asking), &q).contains("more than one open task"));
    }
}
