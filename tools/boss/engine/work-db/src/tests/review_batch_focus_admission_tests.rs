//! Pre-merge review admission consults dispatch focus and all-spawns pause
//! so out-of-scope never-started batches cannot starve in-scope reviews.

use boss_protocol::{
    CreateProjectInput, CreateTaskInput, DispatchFocus, DispatchFocusTargetKind, ExecutionStatus, ReviewBatchPhase,
};

use super::*;

fn project(db: &WorkDb, product_id: &str, name: &str) -> Project {
    db.create_project(CreateProjectInput {
        product_id: product_id.to_owned(),
        name: name.to_owned(),
        description: None,
        goal: None,
        autostart: false,
        no_design_task: true,
        design_reasoning_effort_xhigh: false,
    })
    .unwrap()
}

fn project_task(db: &WorkDb, product_id: &str, project_id: &str, name: &str) -> Task {
    let task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product_id)
                .project_id(project_id)
                .name(name)
                .autostart(false)
                .build(),
        )
        .unwrap();
    db.update_work_item(
        &task.id,
        WorkItemPatch {
            pr_url: Some(format!(
                "https://github.com/example/repo/pull/{}",
                &task.id[task.id.len().saturating_sub(4)..]
            )),
            status: Some("active".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    task
}

fn focus_on_project(db: &WorkDb, project_id: &str) {
    let mut focus = DispatchFocus::default();
    focus.insert(DispatchFocusTargetKind::Project, project_id.to_owned());
    db.save_dispatch_focus(&focus).unwrap();
}

fn create_batch(db: &WorkDb, cycle_root_id: &str, target_sha: &str) -> ReviewBatchDispatch {
    db.create_pre_merge_review_batch(
        batch_input(cycle_root_id.to_owned(), target_sha, ReviewBatchPhase::PreMerge),
        "https://github.com/example/repo",
    )
    .unwrap()
}

fn created_batch(dispatch: ReviewBatchDispatch) -> (ReviewBatch, Vec<WorkExecution>) {
    match dispatch {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected Created, got {other:?}"),
    }
}

/// While focus is on, an out-of-focus root is deferred and leaves no batch.
#[test]
fn focus_defers_an_out_of_scope_pre_merge_batch() {
    let db = WorkDb::open(temp_db_path("focus-defer-out-of-scope")).unwrap();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused");
    let other = project(&db, &product.id, "Other");
    let out = project_task(&db, &product.id, &other.id, "Out of focus PR");
    focus_on_project(&db, &focused.id);

    match create_batch(&db, &out.id, "head-out") {
        ReviewBatchDispatch::AdmissionDeferred => {}
        other => panic!("expected AdmissionDeferred for an out-of-focus root, got {other:?}"),
    }
    assert!(
        db.review_batch_for_target(&out.id, ReviewBatchPhase::PreMerge, "head-out")
            .unwrap()
            .is_none()
    );
    assert!(db.can_admit_review_batch(ReviewBatchPhase::PreMerge).unwrap());
}

/// Four never-started out-of-focus batches filling the pool are released
/// when an in-focus PR is admitted, so the in-focus batch is created.
#[test]
fn in_focus_admission_releases_four_never_started_out_of_focus_reservations() {
    let db = WorkDb::open(temp_db_path("focus-release-four-out-of-focus")).unwrap();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused");
    let other = project(&db, &product.id, "Other");
    let mut out_of_focus = Vec::new();
    for i in 0..4 {
        let task = project_task(&db, &product.id, &other.id, &format!("Out {i}"));
        let (batch, executions) = created_batch(create_batch(&db, &task.id, &format!("head-out-{i}")));
        out_of_focus.push((task, batch, executions));
    }
    assert!(
        !db.can_admit_review_batch(ReviewBatchPhase::PreMerge).unwrap(),
        "four out-of-focus batches must fill the 16-unit pool"
    );

    focus_on_project(&db, &focused.id);
    let in_focus = project_task(&db, &product.id, &focused.id, "In focus PR");
    let (admitted, executions) = created_batch(create_batch(&db, &in_focus.id, "head-in"));

    assert_eq!(admitted.cycle_root_id, in_focus.id);
    assert_eq!(admitted.generation, 1);
    assert_eq!(executions.len(), 2);
    for execution in &executions {
        assert_eq!(execution.status, ExecutionStatus::Ready);
        assert_eq!(execution.work_item_id, in_focus.id);
    }
    for (task, batch, members) in &out_of_focus {
        assert!(
            !db.list_dead_pr_review_candidates()
                .unwrap()
                .iter()
                .any(|c| c.work_item_id == task.id),
            "released batch leaves must return through batch admission, not legacy reviewer recovery"
        );
        assert!(
            db.review_batch(&batch.id).unwrap().is_none(),
            "never-started out-of-focus batch {} must be released",
            batch.id
        );
        for execution in members {
            assert_eq!(
                db.get_execution(&execution.id).unwrap().status,
                ExecutionStatus::Abandoned
            );
        }
        let deferred = db.list_tasks_awaiting_pre_merge_review_admission().unwrap();
        assert!(
            deferred
                .iter()
                .any(|c| c.task_id == task.id || c.cycle_root_id == task.id),
            "released root {} must return to deferred admission: {deferred:?}",
            task.id
        );
    }
}

/// Narrowing focus releases never-started reservations that just fell out
/// of the set, while a batch with a started member keeps its reservation.
#[test]
fn narrowing_focus_releases_never_started_out_of_scope_and_keeps_a_started_batch() {
    let db = WorkDb::open(temp_db_path("focus-narrow-keeps-started")).unwrap();
    let product = create_test_product(&db);
    let keep = project(&db, &product.id, "Keep");
    let drop = project(&db, &product.id, "Drop");
    let started_root = project_task(&db, &product.id, &drop.id, "Started out of focus");
    let never_root = project_task(&db, &product.id, &drop.id, "Never started out of focus");
    let (started_batch, started_execs) = created_batch(create_batch(&db, &started_root.id, "head-started"));
    let (never_batch, never_execs) = created_batch(create_batch(&db, &never_root.id, "head-never"));
    db.force_started_at_for_test(&started_execs[0].id, 1_700_000_000)
        .unwrap();

    focus_on_project(&db, &keep.id);
    let released = db.release_unstartable_pre_merge_review_reservations().unwrap();
    assert_eq!(released, vec![never_batch.id.clone()]);
    assert!(db.review_batch(&never_batch.id).unwrap().is_none());
    assert_eq!(
        db.get_execution(&never_execs[0].id).unwrap().status,
        ExecutionStatus::Abandoned
    );
    let kept = db.review_batch(&started_batch.id).unwrap().unwrap();
    assert_eq!(kept.status, ReviewBatchStatus::Collecting);
    assert_eq!(
        db.get_execution(&started_execs[0].id).unwrap().status,
        ExecutionStatus::Ready
    );
    assert_eq!(
        db.review_pool_reserved_units().unwrap().0,
        PRE_MERGE_BATCH_RESERVATION_UNITS
    );
}

/// Releasing a never-started batch does not consume an automatic remint
/// generation: once the root is in focus, admission is generation 1.
#[test]
fn released_never_started_batch_readmits_at_generation_one() {
    let db = WorkDb::open(temp_db_path("focus-release-preserves-generation")).unwrap();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused");
    let other = project(&db, &product.id, "Other");
    let root = project_task(&db, &product.id, &other.id, "Moves into focus");
    let (first, _) = created_batch(create_batch(&db, &root.id, "head-sha"));
    assert_eq!(first.generation, 1);

    focus_on_project(&db, &focused.id);
    assert!(matches!(
        create_batch(&db, &root.id, "head-sha"),
        ReviewBatchDispatch::AdmissionDeferred
    ));
    assert!(db.review_batch(&first.id).unwrap().is_none());

    focus_on_project(&db, &other.id);
    let (again, _) = created_batch(create_batch(&db, &root.id, "head-sha"));
    assert_eq!(again.generation, 1);
    assert_ne!(again.id, first.id);
}

/// An all-spawns pause holds reviews, so a new pre-merge batch is deferred.
/// The default new_work pause does not.
#[test]
fn all_spawns_pause_defers_pre_merge_admission_and_new_work_does_not() {
    let db = WorkDb::open(temp_db_path("pause-all-spawns-defers-review")).unwrap();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id.clone(), "review target");

    db.set_metadata("dispatch_paused", "1").unwrap();
    db.set_metadata("dispatch_paused_origin", "operator").unwrap();
    db.set_metadata("dispatch_paused_scope", "new_work").unwrap();
    created_batch(create_batch(&db, &root.id, "head-new-work"));

    let other = create_test_chore_manual(&db, product.id, "another target");
    db.set_metadata("dispatch_paused_scope", "all_spawns").unwrap();
    match create_batch(&db, &other.id, "head-all-spawns") {
        ReviewBatchDispatch::AdmissionDeferred => {}
        other => panic!("expected AdmissionDeferred under all-spawns pause, got {other:?}"),
    }
    assert!(
        db.review_batch_for_target(&other.id, ReviewBatchPhase::PreMerge, "head-all-spawns")
            .unwrap()
            .is_none()
    );
}

/// An all-spawns pause releases never-started pre-merge reservations so
/// they cannot occupy the pool while reviews cannot start.
#[test]
fn all_spawns_pause_releases_never_started_pre_merge_reservations() {
    let db = WorkDb::open(temp_db_path("pause-all-spawns-releases")).unwrap();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "review target");
    let (batch, executions) = created_batch(create_batch(&db, &root.id, "head-sha"));

    db.set_metadata("dispatch_paused", "1").unwrap();
    db.set_metadata("dispatch_paused_origin", "operator").unwrap();
    db.set_metadata("dispatch_paused_scope", "new_work").unwrap();
    assert!(
        db.release_unstartable_pre_merge_review_reservations()
            .unwrap()
            .is_empty(),
        "new_work pause must leave never-started review reservations in place"
    );
    assert!(db.review_batch(&batch.id).unwrap().is_some());

    db.set_metadata("dispatch_paused_scope", "all_spawns").unwrap();
    assert_eq!(
        db.release_unstartable_pre_merge_review_reservations().unwrap(),
        vec![batch.id.clone()]
    );
    assert!(db.review_batch(&batch.id).unwrap().is_none());
    assert_eq!(
        db.get_execution(&executions[0].id).unwrap().status,
        ExecutionStatus::Abandoned
    );
}

/// A pause persisted before the scope key existed restores as its origin's
/// default scope, matching the coordinator: an operator pause exempts
/// reviews, so it neither defers admission nor releases reservations.
#[test]
fn operator_pause_without_scope_does_not_hold_reviews() {
    let db = WorkDb::open(temp_db_path("pause-missing-scope-operator")).unwrap();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id.clone(), "review target");
    let (batch, _) = created_batch(create_batch(&db, &root.id, "head-a"));

    db.set_metadata("dispatch_paused", "1").unwrap();
    db.set_metadata("dispatch_paused_origin", "operator").unwrap();
    assert!(
        db.release_unstartable_pre_merge_review_reservations()
            .unwrap()
            .is_empty()
    );
    assert!(db.review_batch(&batch.id).unwrap().is_some());
    let other = create_test_chore_manual(&db, product.id, "another target");
    created_batch(create_batch(&db, &other.id, "head-b"));
}

/// The breaker's automatic all_spawns pause is transient: it neither defers
/// admission nor releases reservations, with or without a persisted scope.
#[test]
fn breaker_pause_does_not_hold_or_release_reviews() {
    let db = WorkDb::open(temp_db_path("pause-breaker-no-hold")).unwrap();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id.clone(), "review target");
    let (batch, _) = created_batch(create_batch(&db, &root.id, "head-a"));

    db.set_metadata("dispatch_paused", "1").unwrap();
    db.set_metadata("dispatch_paused_origin", "breaker").unwrap();
    for scope in [Some("all_spawns"), None] {
        if let Some(scope) = scope {
            db.set_metadata("dispatch_paused_scope", scope).unwrap();
        }
        assert!(
            db.release_unstartable_pre_merge_review_reservations()
                .unwrap()
                .is_empty()
        );
        assert!(db.review_batch(&batch.id).unwrap().is_some());
        assert_eq!(
            db.pre_merge_review_hold_without_history(&root.id, &root.id).unwrap(),
            None
        );
    }
}

/// An explicitly started batch is not released by an all-spawns
/// pause: deleting it would re-admit it later as a non-explicit batch.
#[test]
fn all_spawns_pause_keeps_explicit_never_started_batches() {
    let db = WorkDb::open(temp_db_path("pause-keeps-explicit")).unwrap();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "review target");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![root.id, "https://github.com/example/repo/pull/1"],
        )
        .unwrap();
    let mut input = batch_input(root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge);
    input.pr_url = "https://github.com/example/repo/pull/1".to_owned();
    let (batch, _) = created_batch(db.request_pre_merge_review_batch_for_pool(input, 16).unwrap());

    db.set_metadata("dispatch_paused", "1").unwrap();
    db.set_metadata("dispatch_paused_origin", "operator").unwrap();
    db.set_metadata("dispatch_paused_scope", "all_spawns").unwrap();
    assert!(
        db.release_unstartable_pre_merge_review_reservations()
            .unwrap()
            .is_empty()
    );
    assert!(db.review_batch(&batch.id).unwrap().is_some());
}

/// Releasing for a focus hold refreshes the text of an attention item an
/// earlier pool-full deferral already opened.
#[test]
fn release_refreshes_an_existing_admission_deferred_item() {
    let db = WorkDb::open(temp_db_path("focus-release-refreshes-attention")).unwrap();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused");
    let other = project(&db, &product.id, "Other");
    let root = project_task(&db, &product.id, &other.id, "Out of focus");
    created_batch(create_batch(&db, &root.id, "head-sha"));
    db.upsert_external_tracker_attention(
        &root.id,
        PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND,
        "stale",
        "stale body",
    )
    .unwrap();

    focus_on_project(&db, &focused.id);
    db.release_unstartable_pre_merge_review_reservations().unwrap();

    let items = db
        .list_open_attention_items_of_kind(PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_ne!(items[0].title, "stale");
    assert!(items[0].body_markdown.contains("focus"));
}

/// A deleted root or producing revision must not poison the release pass
/// and prevent an unrelated in-focus PR from taking a free reservation.
#[test]
fn focus_admission_survives_deleted_reservation_owners() {
    for deleted_root in [true, false] {
        let db = WorkDb::open(temp_db_path("focus-deleted-reservation-owner")).unwrap();
        let product = create_test_product(&db);
        let keep = project(&db, &product.id, "Keep");
        let other = project(&db, &product.id, "Other");
        let root = project_task(&db, &product.id, &other.id, "Old root");
        let producer = project_task(&db, &product.id, &other.id, "Old revision");
        let (batch, _) = created_batch(create_batch(&db, &root.id, "old-head"));
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE pr_review_batches SET producing_work_item_id = ?2 WHERE id = ?1",
            rusqlite::params![batch.id, producer.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET deleted_at = '1' WHERE id = ?1",
            [if deleted_root { &root.id } else { &producer.id }],
        )
        .unwrap();
        drop(conn);
        focus_on_project(&db, &keep.id);
        let target = project_task(&db, &product.id, &keep.id, "In focus");

        created_batch(create_batch(&db, &target.id, "new-head"));

        assert!(db.review_batch(&batch.id).unwrap().is_none());
        let attention = db
            .list_open_attention_items_of_kind(PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)
            .unwrap();
        assert_eq!(attention.len(), 1);
        assert_eq!(
            attention[0].work_item_id.as_deref(),
            Some(if deleted_root {
                producer.id.as_str()
            } else {
                root.id.as_str()
            })
        );
    }
}

#[test]
fn focus_admission_checks_capacity_even_when_release_fails() {
    let db = WorkDb::open(temp_db_path("focus-release-error")).unwrap();
    let product = create_test_product(&db);
    let keep = project(&db, &product.id, "Keep");
    let other = project(&db, &product.id, "Other");
    let root = project_task(&db, &product.id, &other.id, "Old root");
    let (old, _) = created_batch(create_batch(&db, &root.id, "old-head"));
    db.connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_reservation_release BEFORE DELETE ON pr_review_batches
         BEGIN SELECT RAISE(ABORT, 'injected release failure'); END;",
        )
        .unwrap();
    focus_on_project(&db, &keep.id);
    let target = project_task(&db, &product.id, &keep.id, "In focus");

    created_batch(create_batch(&db, &target.id, "new-head"));

    assert!(db.review_batch(&old.id).unwrap().is_some());
    assert_eq!(db.review_pool_reserved_units().unwrap().0, 8);
}

/// A claimed (mid-spawn) member keeps its batch: abandoning it would fail
/// the in-flight spawn.
#[test]
fn release_keeps_batch_with_claimed_member() {
    let db = WorkDb::open(temp_db_path("focus-release-keeps-claimed")).unwrap();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused");
    let other = project(&db, &product.id, "Other");
    let out = project_task(&db, &product.id, &other.id, "Out of focus PR");
    let (batch, executions) = created_batch(create_batch(&db, &out.id, "head-claimed"));
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'claimed' WHERE id = ?1",
            rusqlite::params![executions[0].id],
        )
        .unwrap();
    focus_on_project(&db, &focused.id);
    assert!(
        db.release_unstartable_pre_merge_review_reservations()
            .unwrap()
            .is_empty()
    );
    assert!(db.review_batch(&batch.id).unwrap().is_some());
}

/// A garbage persisted focus value is treated as focus off for admission.
#[test]
fn unreadable_persisted_focus_does_not_block_admission() {
    let db = WorkDb::open(temp_db_path("focus-garbage-admits")).unwrap();
    let product = create_test_product(&db);
    let proj = project(&db, &product.id, "P");
    let root = project_task(&db, &product.id, &proj.id, "PR");
    db.set_metadata("dispatch_focus", "{not json").unwrap();
    created_batch(create_batch(&db, &root.id, "head-garbage"));
}

/// An explicit review start is admitted and retained when focus or an
/// all-spawns pause holds automatic admission.
#[test]
fn explicit_start_is_admitted_and_retained_under_holds() {
    for hold in ["focus", "all_spawns"] {
        let db = WorkDb::open(temp_db_path("focus-explicit-admitted")).unwrap();
        let product = create_test_product(&db);
        let focused = project(&db, &product.id, "Focused");
        let other = project(&db, &product.id, "Other");
        let out = project_task(&db, &product.id, &other.id, "Out of focus PR");
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
                rusqlite::params![out.id],
            )
            .unwrap();
        if hold == "focus" {
            focus_on_project(&db, &focused.id);
        } else {
            db.set_metadata("dispatch_paused", "1").unwrap();
            db.set_metadata("dispatch_paused_origin", "operator").unwrap();
            db.set_metadata("dispatch_paused_scope", "all_spawns").unwrap();
        }
        let mut input = batch_input(out.id.clone(), "head-explicit", ReviewBatchPhase::PreMerge);
        input.pr_url = format!(
            "https://github.com/example/repo/pull/{}",
            &out.id[out.id.len().saturating_sub(4)..]
        );
        let (batch, _) = created_batch(db.request_pre_merge_review_batch_for_pool(input, 16).unwrap());
        assert!(batch.explicit);
        assert!(
            db.release_unstartable_pre_merge_review_reservations()
                .unwrap()
                .is_empty()
        );
        assert!(db.review_batch(&batch.id).unwrap().is_some());
    }
}
