use super::*;

// ── Archived work-item lifecycle guards (proj_18be59fc8d8b2440_363) ────────
//
// Covers three defects observed around a gated revision whose parent PR
// merged: (1) the auto-archival was silent, (2) `bossctl work start`
// happily created a stranded `ready` execution on the archived row, and
// (3) `boss task move --to doing` reported success and then the engine
// silently snapped the row back to `archived`.

/// Regression (proj_18be59fc8d8b2440_363): a moot-revision auto-archival
/// must not be silent. It must (a) stamp `archived_reason` so `boss task
/// show` explains the disappearance, and (b) raise an open
/// `revision_archived` attention item so the operator is notified instead
/// of discovering the state change during a later debugging session.
#[test]
fn mark_chore_pr_merged_surfaces_moot_revision_archival() {
    let db = WorkDb::open(temp_db_path("rev-moot-surfaced")).unwrap();
    let product_id = make_revision_product(&db, "moot-surfaced");
    let pr_url = "https://github.com/spinyfin/mono/pull/823";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let rem_id = "rem_fake_for_surfaced_test";
    let rev_id = insert_ci_fix_revision_row(&db, &product_id, &parent_id, rem_id);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let conn = db.connect().unwrap();
    let task = query_task(&conn, &rev_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Archived);
    let reason = task
        .archived_reason
        .as_deref()
        .expect("archived_reason must be set for an engine-auto-archived revision");
    assert!(
        reason.contains("parent PR merged"),
        "archived_reason must explain the parent-PR-merged trigger: {reason}"
    );
    drop(conn);

    let attentions = db.list_attention_items_for_work_item(&rev_id).unwrap();
    assert_eq!(
        attentions.len(),
        1,
        "exactly one attention item must be raised for the silent archival: {attentions:?}"
    );
    assert_eq!(attentions[0].kind, "revision_archived");
    assert_eq!(attentions[0].status, "open");
}

/// Regression (proj_18be59fc8d8b2440_363): `RequestExecution` (the
/// `bossctl work start` path) against a terminal work item must be refused
/// loudly instead of silently minting a `ready` execution that can never be
/// dispatched — the row is closed, so nothing will ever pick it up, and it
/// just strands a phantom pending row.
#[test]
fn request_execution_refuses_archived_work_item() {
    let db = WorkDb::open(temp_db_path("request-execution-refuses-archived")).unwrap();
    let product_id = make_revision_product(&db, "req-exec-archived");
    let chore = create_test_chore_manual(&db, product_id.clone(), "archived chore");

    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("archived".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let err = db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("terminal") && err.contains("archived"),
        "unexpected error: {err}"
    );

    let executions = executions_for(&db, &chore.id);
    assert!(
        executions.is_empty(),
        "no execution row must be created for a refused terminal work item: {executions:?}"
    );
}

/// Regression (proj_18be59fc8d8b2440_363): moving a revision off `archived`
/// must be refused up front when the engine will immediately revert it —
/// i.e. when the chain root's PR already merged and the revision was
/// auto-archived as moot. Accepting the move and then silently re-archiving
/// it on the next reconcile tick is the exact "Moved task" success followed
/// by a silent revert the incident reported.
#[test]
fn move_off_archived_moot_revision_is_refused() {
    let db = WorkDb::open(temp_db_path("move-off-archived-moot-refused")).unwrap();
    let product_id = make_revision_product(&db, "move-refused");
    let pr_url = "https://github.com/spinyfin/mono/pull/824";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let rem_id = "rem_fake_for_move_refused_test";
    let rev_id = insert_ci_fix_revision_row(&db, &product_id, &parent_id, rem_id);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();
    assert_eq!(
        task_status(&db, &rev_id),
        "archived",
        "setup: revision must be archived"
    );

    let err = db
        .update_work_item(
            &rev_id,
            WorkItemPatch {
                status: Some("todo".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("moot") || err.contains("archive it again"),
        "unexpected error: {err}"
    );
    assert_eq!(
        task_status(&db, &rev_id),
        "archived",
        "the refused move must leave the row untouched"
    );
}

/// A manually archived revision whose chain root PR is still open must
/// remain reopenable — the guard added for the moot-revision case must not
/// blanket-block every archived-revision move, only the ones the engine
/// will provably re-archive.
#[test]
fn move_off_archived_revision_with_open_chain_root_succeeds() {
    let db = WorkDb::open(temp_db_path("move-off-archived-open-root-ok")).unwrap();
    let product_id = make_revision_product(&db, "move-ok");
    let pr_url = "https://github.com/spinyfin/mono/pull/825";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);
    let rev_id = insert_revision_row(&db, &product_id, &parent_id);

    // Simulate a human archiving the revision by mistake while the parent
    // PR is still open (not done/archived).
    db.update_work_item(
        &rev_id,
        WorkItemPatch {
            status: Some("archived".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    assert_eq!(
        task_status(&db, &rev_id),
        "archived",
        "setup: revision must be archived"
    );

    db.update_work_item(
        &rev_id,
        WorkItemPatch {
            status: Some("todo".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    assert_eq!(
        task_status(&db, &rev_id),
        "todo",
        "reopening a manually archived revision with a live chain root must succeed"
    );
}

/// Regression (proj_18be59fc8d8b2440_363): a revision gated on a plain
/// dependency edge whose chain-root PR merges *while the gate is still
/// blocking it* must not surface as a stranded `ready` execution once the
/// dependency clears. The auto-unblock cascade must route revision-kind
/// dependents through the chain-root-aware reconciler instead of blindly
/// minting a `ready` row.
#[test]
fn dependency_unblock_of_moot_revision_does_not_strand_ready_execution() {
    let db = WorkDb::open(temp_db_path("dep-unblock-moot-revision")).unwrap();
    let product_id = make_revision_product(&db, "dep-unblock-moot");
    let pr_url = "https://github.com/spinyfin/mono/pull/826";
    let parent_id = make_done_chore(&db, &product_id, pr_url);
    let rev_id = insert_revision_row(&db, &product_id, &parent_id);

    let prereq_id = create_test_chore_manual(&db, product_id.clone(), "prereq").id;

    db.add_dependency(AddDependencyInput {
        dependent: rev_id.clone(),
        prerequisite: prereq_id.clone(),
        relation: None,
    })
    .unwrap();
    assert_eq!(task_status(&db, &rev_id), "blocked", "setup: revision must be gated");

    // Satisfy the prereq — this fires the auto-unblock cascade.
    db.update_work_item(
        &prereq_id,
        WorkItemPatch {
            status: Some("done".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    assert_eq!(
        task_status(&db, &rev_id),
        "archived",
        "a revision whose chain root already merged must be archived, not left todo/ready, \
         once its dependency gate clears"
    );
    let executions = executions_for(&db, &rev_id);
    assert!(
        executions
            .iter()
            .all(|(_, status)| !matches!(status.as_str(), "queued" | "ready" | "waiting_dependency")),
        "no dispatchable execution may remain against the now-archived revision: {executions:?}"
    );
}

/// Regression (proj_18be59fc8d8b2440_363): the startup reconciliation sweep
/// must clean up any `ready`/`queued`/`waiting_dependency` execution that
/// slipped through against a work item already terminal or soft-deleted —
/// the backstop for the create-time guards.
#[test]
fn abandon_stranded_executions_on_closed_work_items_sweep() {
    let db = WorkDb::open(temp_db_path("abandon-stranded-sweep")).unwrap();
    let product_id = make_revision_product(&db, "stranded-sweep");
    let chore = create_test_chore_manual(&db, product_id.clone(), "stranded");

    // Force-create a `ready` execution the way a pre-fix race would have,
    // then archive the work item out from under it directly at the SQL
    // layer (bypassing the now-guarded update path) to simulate a row that
    // predates this fix shipping.
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .repo_remote_url("https://github.com/spinyfin/mono")
            .build(),
    )
    .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'archived' WHERE id = ?1",
            rusqlite::params![chore.id],
        )
        .unwrap();

    let abandoned = db.abandon_stranded_executions_on_closed_work_items().unwrap();
    assert_eq!(
        abandoned.len(),
        1,
        "the stranded ready execution must be abandoned: {abandoned:?}"
    );
    assert_eq!(abandoned[0].work_item_id, chore.id);

    let executions = executions_for(&db, &chore.id);
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].1, "abandoned");

    // Idempotent: a second pass finds nothing left to do.
    let second_pass = db.abandon_stranded_executions_on_closed_work_items().unwrap();
    assert!(second_pass.is_empty(), "sweep must be idempotent: {second_pass:?}");
}
