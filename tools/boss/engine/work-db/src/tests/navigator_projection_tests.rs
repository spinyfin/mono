//! The Navigator projection through the real work tree: every step compares the
//! incrementally maintained tree with a complete assembly (`check`), so each
//! assertion below holds for both paths, and nothing is mocked beneath them.
use super::board_projection_tests::check;
use super::*;
use crate::board_projection::{CachedProjection, Refresh};
use boss_protocol::{
    CreateAttentionItemInput, NavigatorAction, NavigatorActivityKind, NavigatorCommandKind, NavigatorHumanAction,
    NavigatorProjection, NavigatorReason, NavigatorRoot, NavigatorSection, NavigatorWait, ReviewSection,
};

const PR: &str = "https://github.com/spinyfin/mono/pull/7";
const OTHER_PR: &str = "https://github.com/spinyfin/mono/pull/8";
const HEAD: &str = "head-1";
const NOW: i64 = 100;

type Model = Option<CachedProjection>;

fn projection(model: &Model, id: &str) -> NavigatorProjection {
    let tree = model.as_ref().expect("a retained projection").tree();
    tree.tasks
        .iter()
        .chain(&tree.chores)
        .find(|task| task.id == id)
        .unwrap_or_else(|| panic!("{id} is not on the board"))
        .navigator
        .as_deref()
        .cloned()
        .unwrap_or_else(|| panic!("{id} has no navigator projection"))
}

fn row(model: &Model, id: &str) -> NavigatorRoot {
    projection(model, id)
        .root
        .unwrap_or_else(|| panic!("{id} is folded into another row"))
}

fn card<'a>(model: &'a Model, id: &str) -> &'a boss_protocol::Task {
    let tree = model.as_ref().expect("a retained projection").tree();
    tree.tasks
        .iter()
        .chain(&tree.chores)
        .find(|task| task.id == id)
        .unwrap_or_else(|| panic!("{id} is not on the board"))
}

fn has_projection(model: &Model, id: &str) -> bool {
    let tree = model.as_ref().unwrap().tree();
    tree.tasks
        .iter()
        .chain(&tree.chores)
        .find(|task| task.id == id)
        .is_some_and(|task| task.navigator.is_some())
}

fn waiting(model: &Model, id: &str) -> NavigatorWait {
    match row(model, id).reason {
        NavigatorReason::Waiting { wait } => wait,
        other => panic!("{id} is not waiting: {other:?}"),
    }
}

fn actions(model: &Model, id: &str) -> Vec<(NavigatorHumanAction, String)> {
    row(model, id)
        .actions
        .into_iter()
        .filter_map(|action| match action {
            NavigatorAction::Attention { kind, task_id, .. } => Some((kind, task_id)),
            NavigatorAction::OpenInput { .. } | NavigatorAction::Unknown => None,
        })
        .collect()
}

fn step(db: &WorkDb, product: &str, model: &mut Model) -> Refresh {
    check(db, product, model, NOW)
}

fn exec(db: &WorkDb, id: &str, kind: ExecutionKind, status: &str, run: Option<&str>) -> String {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(id)
                .kind(kind)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    if run.is_some() {
        db.start_execution_run(&execution.id, "synthetic", "repo", "lease", "ws", "/tmp/ws")
            .unwrap();
    }
    set_exec(db, &execution.id, status);
    if let Some(run) = run {
        set_run(db, &execution.id, run);
    }
    execution.id
}

fn set_exec(db: &WorkDb, id: &str, status: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = ?2 WHERE id = ?1",
            params![id, status],
        )
        .unwrap();
}

fn set_run(db: &WorkDb, execution: &str, status: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET status = ?2 WHERE execution_id = ?1",
            params![execution, status],
        )
        .unwrap();
}

fn update(db: &WorkDb, statement: &str, id: &str) {
    db.connect().unwrap().execute(statement, [id]).unwrap();
}

/// An open PR whose current head was observed green and reviewed clean.
fn reviewed_clean_pr(db: &WorkDb, root: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = ?2, ci_required_state = 'success', pr_mergeable_state = 'mergeable'
             WHERE id = ?1",
            params![root, HEAD],
        )
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
        [&execution.id],
    )
    .unwrap();
    WorkDb::insert_review_verdict_in_tx(
        &conn,
        &execution.id,
        root,
        &crate::ReviewVerdictInput {
            head_sha: Some(HEAD.to_owned()),
            findings_count: 0,
            revision_warranted: false,
            gate_outcome: "completed_clean",
        },
    )
    .unwrap();
}

fn ask(db: &WorkDb, task: &str, execution: &str) {
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO operator_questions(id, work_item_id, execution_id, proposal_id, question_json, status, created_at)
             VALUES ('oq-navigator', ?1, ?2, 'prop', ?3, 'open', '50')",
            params![
                task,
                execution,
                r#"{"text":"Which channel?","answer_type":{"kind":"yes_no"},"explanation":"why"}"#
            ],
        )
        .unwrap();
}

#[test]
fn a_family_moves_through_review_revision_question_and_merge_without_changing_rows() {
    let db = WorkDb::open(temp_db_path("navigator-family")).unwrap();
    let product = make_revision_product(&db, "navigator-family");
    let root = make_in_review_chore(&db, &product, PR);
    let mut model = None;
    assert_eq!(step(&db, &product, &mut model), Refresh::Snapshot("cold"));

    // An open PR whose CI and review facts are unknown is nobody's turn.
    assert_eq!(row(&model, &root).section, NavigatorSection::Backlog);
    assert!(actions(&model, &root).is_empty());

    // Green, mergeable and clean on the current head: a person may review.
    reviewed_clean_pr(&db, &root);
    assert!(matches!(step(&db, &product, &mut model), Refresh::Incremental(_)));
    let ready = row(&model, &root);
    assert_eq!(ready.section, NavigatorSection::Ready);
    assert_eq!(
        ready.reason,
        NavigatorReason::Human {
            action: NavigatorHumanAction::Review
        }
    );
    assert_eq!(ready.reason_label, "Review");
    assert_eq!(ready.default_detail, boss_protocol::NavigatorDetail::Review);
    assert_eq!(actions(&model, &root), [(NavigatorHumanAction::Review, root.clone())]);
    assert!(ready.sort_key.starts_with("0|002|"), "{}", ready.sort_key);
    assert!(
        ready
            .commands
            .iter()
            .any(|command| command.kind == NavigatorCommandKind::OpenPr)
    );

    // Approval turns the same decision into a merge, which sorts after review.
    update(
        &db,
        "UPDATE tasks SET review_required_state = 'approved' WHERE id = ?1",
        &root,
    );
    step(&db, &product, &mut model);
    assert_eq!(actions(&model, &root), [(NavigatorHumanAction::Merge, root.clone())]);
    assert_eq!(row(&model, &root).section, NavigatorSection::Ready);
    assert!(row(&model, &root).sort_key.starts_with("0|003|"));

    // A queued revision withdraws the decision; the family keeps one row.
    let revision = insert_revision_row(&db, &product, &root);
    step(&db, &product, &mut model);
    assert_eq!(row(&model, &root).section, NavigatorSection::Backlog);
    assert!(actions(&model, &root).is_empty());
    let folded = projection(&model, &revision);
    assert_eq!(folded.root_task_id, root);
    assert!(folded.root.is_none());
    let summary = row(&model, &root).revisions;
    assert_eq!((summary.total, summary.open), (1, 1));
    assert_eq!(summary.current_task_id.as_deref(), Some(revision.as_str()));

    // A running CI-fix revision makes the original row Working. The runtime
    // change alone (no task status change after the first step) refreshes it.
    update(
        &db,
        "UPDATE tasks SET status = 'active', created_via = 'ci-fix:cir_1' WHERE id = ?1",
        &revision,
    );
    let repair = exec(
        &db,
        &revision,
        ExecutionKind::RevisionImplementation,
        "running",
        Some("active"),
    );
    step(&db, &product, &mut model);
    let working = row(&model, &root);
    assert_eq!(working.section, NavigatorSection::Working);
    assert_eq!(
        working.reason,
        NavigatorReason::Active {
            activity: NavigatorActivityKind::FixingCi
        }
    );
    assert_eq!(working.activities.len(), 1);
    assert_eq!(working.activities[0].execution_id, repair);
    assert_eq!(working.activities[0].task_id, revision);
    assert!(working.sort_key.starts_with("1|"));
    set_run(&db, &repair, "completed");
    step(&db, &product, &mut model);
    assert_eq!(
        row(&model, &root).section,
        NavigatorSection::Working,
        "a completed dispatch run under a live execution is still working"
    );

    // A question from the running revision stays visible without moving the row.
    ask(&db, &revision, &repair);
    step(&db, &product, &mut model);
    assert_eq!(row(&model, &root).section, NavigatorSection::Working);
    assert_eq!(
        actions(&model, &root),
        [(NavigatorHumanAction::Question, revision.clone())]
    );

    // Once the agent itself waits on a person, nothing is progressing.
    set_exec(&db, &repair, "waiting_human");
    step(&db, &product, &mut model);
    let waiting_on_person = row(&model, &root);
    assert_eq!(waiting_on_person.section, NavigatorSection::NeedsAttention);
    assert_eq!(
        waiting_on_person.default_detail,
        boss_protocol::NavigatorDetail::Attention
    );
    assert_eq!(waiting_on_person.actions.len(), 2);
    assert!(matches!(
        waiting_on_person.actions[1],
        NavigatorAction::OpenInput { .. }
    ));

    // Answering clears the question; the terminal wait remains.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE operator_questions SET status = 'answered' WHERE id = 'oq-navigator'",
            [],
        )
        .unwrap();
    step(&db, &product, &mut model);
    let input_only = row(&model, &root);
    assert_eq!(input_only.section, NavigatorSection::NeedsAttention);
    assert_eq!(
        input_only.reason,
        NavigatorReason::Human {
            action: NavigatorHumanAction::Input
        }
    );
    assert_eq!(input_only.default_detail, boss_protocol::NavigatorDetail::Activity);

    // The revision completes: the original PR is ready to merge again.
    set_exec(&db, &repair, "completed");
    update(&db, "UPDATE tasks SET status = 'done' WHERE id = ?1", &revision);
    step(&db, &product, &mut model);
    assert_eq!(row(&model, &root).section, NavigatorSection::Ready);
    assert_eq!(row(&model, &root).revisions.open, 0);
    assert_eq!(row(&model, &root).revisions.total, 1);

    // The real merge-observation path lands the row, and a stale running child
    // cannot take it back.
    assert!(db.mark_chore_pr_merged(&root, PR).unwrap().is_some());
    step(&db, &product, &mut model);
    let stale = insert_revision_row(&db, &product, &root);
    update(&db, "UPDATE tasks SET status = 'active' WHERE id = ?1", &stale);
    exec(
        &db,
        &stale,
        ExecutionKind::RevisionImplementation,
        "running",
        Some("active"),
    );
    step(&db, &product, &mut model);
    let landed = row(&model, &root);
    assert_eq!(landed.section, NavigatorSection::Landed);
    assert_eq!(landed.reason_label, "Merged");
    assert_eq!(landed.merge_evidence.as_ref().unwrap().pr_url, PR);
    assert!(landed.sort_key.starts_with("4|"));

    // Rebinding to another PR cannot inherit the old merge.
    update(&db, "UPDATE tasks SET status = 'done' WHERE id = ?1", &stale);
    update(
        &db,
        &format!("UPDATE tasks SET pr_url = '{OTHER_PR}' WHERE id = ?1"),
        &root,
    );
    step(&db, &product, &mut model);
    assert_eq!(row(&model, &root).section, NavigatorSection::Backlog);
    assert_eq!(waiting(&model, &root), NavigatorWait::MergeUnverified);
    assert!(row(&model, &root).merge_evidence.is_none());
    update(&db, &format!("UPDATE tasks SET pr_url = '{PR}' WHERE id = ?1"), &root);
    step(&db, &product, &mut model);
    assert_eq!(row(&model, &root).section, NavigatorSection::Landed);
}

#[test]
fn relationship_changes_reproject_every_family_they_touch() {
    let db = WorkDb::open(temp_db_path("navigator-relationships")).unwrap();
    let product = make_revision_product(&db, "navigator-relationships");
    let first = make_in_review_chore(&db, &product, PR);
    let second = create_test_chore_manual(&db, product.clone(), "second family").id;
    update(
        &db,
        &format!("UPDATE tasks SET status = 'in_review', pr_url = '{OTHER_PR}' WHERE id = ?1"),
        &second,
    );
    let revision = insert_revision_row(&db, &product, &first);
    let nested = insert_revision_row(&db, &product, &revision);
    let mut model = None;
    step(&db, &product, &mut model);
    // A nested revision belongs to the same row as the revision above it.
    assert_eq!(projection(&model, &nested).root_task_id, first);
    assert_eq!(row(&model, &first).revisions.total, 2);
    assert_eq!(row(&model, &second).revisions.total, 0);

    // Reparenting moves the revision, and the nested one with it.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET parent_task_id = ?2 WHERE id = ?1",
            params![revision, second],
        )
        .unwrap();
    assert!(matches!(step(&db, &product, &mut model), Refresh::Incremental(_)));
    assert_eq!(projection(&model, &nested).root_task_id, second);
    assert_eq!(row(&model, &first).revisions.total, 0);
    assert_eq!(row(&model, &second).revisions.total, 2);

    // Hard deletion removes it from the family it left.
    db.connect()
        .unwrap()
        .execute("DELETE FROM tasks WHERE id = ?1", [&nested])
        .unwrap();
    step(&db, &product, &mut model);
    assert_eq!(row(&model, &second).revisions.total, 1);

    // Archiving the root removes it from the Work population; its live
    // revisions do not become independent rows. Restoring it folds them again.
    update(&db, "UPDATE tasks SET status = 'archived' WHERE id = ?1", &second);
    step(&db, &product, &mut model);
    assert!(
        !model
            .as_ref()
            .unwrap()
            .tree()
            .chores
            .iter()
            .any(|task| task.id == second)
    );
    assert!(
        !has_projection(&model, &revision),
        "revisions of an archived root are not independent rows"
    );
    update(&db, "UPDATE tasks SET status = 'in_review' WHERE id = ?1", &second);
    step(&db, &product, &mut model);
    assert_eq!(projection(&model, &revision).root_task_id, second);
    assert!(projection(&model, &revision).root.is_none());
}

#[test]
fn holds_and_deferred_conflicts_are_backlog_waits_with_engine_labels() {
    let db = WorkDb::open(temp_db_path("navigator-waits")).unwrap();
    let product = make_revision_product(&db, "navigator-waits");
    let held = create_test_chore_manual(&db, product.clone(), "held").id;
    let conflicted = make_in_review_chore(&db, &product, PR);
    update(
        &db,
        "UPDATE tasks SET review_required_state = 'approved', ci_required_state = 'success', pr_mergeable_state = 'conflicting' WHERE id = ?1",
        &conflicted,
    );
    let mut model = None;
    step(&db, &product, &mut model);

    // A ready execution the scheduler holds outside the dispatch focus set.
    let execution = exec(&db, &held, ExecutionKind::ChoreImplementation, "ready", None);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_wait_reason = 'focus_mode' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
    step(&db, &product, &mut model);
    assert_eq!(waiting(&model, &held), NavigatorWait::FocusHeld);

    // A conflict whose resolution is deferred, with a revision held by focus too.
    db.record_conflict_deferral(
        ConflictResolutionInsertInput::builder()
            .product_id(&product)
            .work_item_id(&conflicted)
            .pr_url(PR)
            .pr_number(7)
            .head_branch("feature")
            .base_branch("main")
            .base_sha_at_trigger("base-1")
            .head_sha_before("h1")
            .build(),
    )
    .unwrap();
    step(&db, &product, &mut model);
    assert_eq!(
        waiting(&model, &conflicted),
        NavigatorWait::DeferredConflict { blocker: None }
    );
    let sibling = insert_revision_row(&db, &product, &conflicted);
    let held_revision = exec(&db, &sibling, ExecutionKind::RevisionImplementation, "ready", None);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_wait_reason = 'focus_mode' WHERE id = ?1",
            [&held_revision],
        )
        .unwrap();
    step(&db, &product, &mut model);
    assert_eq!(row(&model, &conflicted).section, NavigatorSection::Backlog);
    assert!(
        row(&model, &conflicted)
            .actions
            .iter()
            .all(|action| !matches!(action, NavigatorAction::Attention { .. })),
        "a conflict is not a request to a person"
    );
    db.end_conflict_deferral(&conflicted, ConflictDeferralEnd::ConflictCleared)
        .unwrap();
    step(&db, &product, &mut model);
}

#[test]
fn engine_order_is_priority_then_start_time_and_automation_work_is_excluded() {
    let db = WorkDb::open(temp_db_path("navigator-order")).unwrap();
    let product = make_revision_product(&db, "navigator-order");
    let mut ids = Vec::new();
    for (name, priority, started) in [
        ("late", "medium", "30"),
        ("early", "medium", "10"),
        ("urgent", "high", "50"),
    ] {
        let id = create_test_chore_manual(&db, product.clone(), name).id;
        update(
            &db,
            &format!("UPDATE tasks SET priority = '{priority}', status = 'active' WHERE id = ?1"),
            &id,
        );
        let execution = exec(&db, &id, ExecutionKind::ChoreImplementation, "running", Some("active"));
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET started_at = ?2, created_at = ?2 WHERE id = ?1",
                params![execution, started],
            )
            .unwrap();
        ids.push(id);
    }
    let owned = create_test_chore_manual(&db, product.clone(), "automation").id;
    // The automation row itself is irrelevant to the projection.
    db.connect()
        .unwrap()
        .execute_batch(&format!(
            "PRAGMA foreign_keys = OFF; UPDATE tasks SET source_automation_id = 'automation-1' WHERE id = '{owned}';"
        ))
        .unwrap();
    let owned_revision = insert_revision_row(&db, &product, &owned);
    let mut model = None;
    step(&db, &product, &mut model);
    let mut by_key: Vec<_> = ids.iter().map(|id| (row(&model, id).sort_key, id.clone())).collect();
    by_key.sort();
    assert_eq!(
        by_key.into_iter().map(|(_, id)| id).collect::<Vec<_>>(),
        [ids[2].clone(), ids[1].clone(), ids[0].clone()],
        "priority first, then the earlier start"
    );
    assert!(!has_projection(&model, &owned));
    assert!(!has_projection(&model, &owned_revision));
}

#[test]
fn typed_question_attentions_reach_the_owning_family_until_actioned_or_dismissed() {
    let db = WorkDb::open(temp_db_path("navigator-attentions")).unwrap();
    let product = make_revision_product(&db, "navigator-attentions");
    let root = make_in_review_chore(&db, &product, PR);
    let revision = insert_revision_row(&db, &product, &root);
    let mut model = None;
    step(&db, &product, &mut model);
    assert!(actions(&model, &root).is_empty());

    let (attention, group) = db
        .create_attention(
            CreateAttentionInput::builder()
                .kind("question")
                .association_task_id(&revision)
                .group_key("navigator-question")
                .source_kind("design_doc")
                .source_doc_path("docs/question.md")
                .question_type("yes_no")
                .prompt_text("Ship it?")
                .build(),
        )
        .unwrap();
    // Attention groups are in the full-snapshot invalidation class.
    assert_eq!(step(&db, &product, &mut model), Refresh::Snapshot("broad_input"));
    let asked = row(&model, &root);
    assert_eq!(asked.section, NavigatorSection::NeedsAttention);
    assert_eq!(
        asked.reason,
        NavigatorReason::Human {
            action: NavigatorHumanAction::Question
        }
    );
    assert_eq!(asked.default_detail, boss_protocol::NavigatorDetail::Attention);
    assert_eq!(
        actions(&model, &root),
        [(NavigatorHumanAction::Question, revision.clone())]
    );

    // Saving an answer is not actioning the group: it stays the person's turn.
    db.answer_attention(&attention.id, Some("yes".to_owned()), false, false)
        .unwrap();
    assert_eq!(step(&db, &product, &mut model), Refresh::Snapshot("broad_input"));
    assert_eq!(
        actions(&model, &root),
        [(NavigatorHumanAction::Question, revision.clone())]
    );
    assert_eq!(row(&model, &root).section, NavigatorSection::NeedsAttention);

    db.dismiss_attention(&group.id, None).unwrap();
    assert_eq!(step(&db, &product, &mut model), Refresh::Snapshot("broad_input"));
    assert!(actions(&model, &root).is_empty(), "a dismissed group is history");
    assert_eq!(row(&model, &root).section, NavigatorSection::Backlog);
}

#[test]
fn typed_followup_attentions_reach_the_owning_family_until_actioned_or_dismissed() {
    let db = WorkDb::open(temp_db_path("navigator-followups")).unwrap();
    let product = make_revision_product(&db, "navigator-followups");
    let root = make_in_review_chore(&db, &product, PR);
    let revision = insert_revision_row(&db, &product, &root);
    let mut model = None;
    step(&db, &product, &mut model);
    assert!(actions(&model, &root).is_empty());

    let (_attention, group) = db
        .create_attention(
            CreateAttentionInput::builder()
                .kind("followup")
                .association_task_id(&revision)
                .group_key("navigator-followup")
                .source_kind("task_transcript")
                .source_task_id(&revision)
                .proposed_name("Add retry")
                .proposed_description("Retry transient failures.")
                .build(),
        )
        .unwrap();
    assert_eq!(step(&db, &product, &mut model), Refresh::Snapshot("broad_input"));
    let asked = row(&model, &root);
    assert_eq!(asked.section, NavigatorSection::NeedsAttention);
    assert_eq!(
        asked.reason,
        NavigatorReason::Human {
            action: NavigatorHumanAction::Question
        }
    );
    assert_eq!(asked.default_detail, boss_protocol::NavigatorDetail::Attention);
    assert_eq!(
        actions(&model, &root),
        [(NavigatorHumanAction::Question, revision.clone())]
    );
    let attention_ids: Vec<_> = asked
        .actions
        .iter()
        .filter_map(|action| match action {
            NavigatorAction::Attention { attention_id, .. } => Some(attention_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(attention_ids, [group.id.as_str()]);

    db.dismiss_attention(&group.id, None).unwrap();
    assert_eq!(step(&db, &product, &mut model), Refresh::Snapshot("broad_input"));
    assert!(actions(&model, &root).is_empty(), "a dismissed follow-up is history");
    assert_eq!(row(&model, &root).section, NavigatorSection::Backlog);
}

fn in_review_named(db: &WorkDb, product_id: &str, name: &str, pr_url: &str) -> String {
    let id = create_test_chore_manual(db, product_id, name).id;
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            params![id, pr_url],
        )
        .unwrap();
    id
}

fn spawn_live(db: &WorkDb, work_item_id: &str, kind: ExecutionKind) -> String {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(work_item_id)
                .kind(kind)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (execution, run) = db
        .start_execution_run(&execution.id, "synthetic", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&execution.id)
            .run_id(&run.id)
            .execution_status(ExecutionStatus::Running)
            .run_status("completed")
            .build(),
    )
    .unwrap();
    execution.id
}

#[test]
fn a_completed_dispatch_run_under_a_live_execution_is_working_then_waiting_then_gone() {
    let db = WorkDb::open(temp_db_path("navigator-live-run")).unwrap();
    let product = make_revision_product(&db, "navigator-live-run");
    let root = create_test_chore_manual(&db, product.clone(), "live worker").id;
    let mut model = None;
    step(&db, &product, &mut model);

    let execution = spawn_live(&db, &root, ExecutionKind::ChoreImplementation);
    assert!(matches!(step(&db, &product, &mut model), Refresh::Incremental(_)));
    let working = row(&model, &root);
    assert_eq!(working.section, NavigatorSection::Working);
    assert_eq!(working.activities.len(), 1);
    assert_eq!(working.activities[0].execution_id, execution);

    set_exec(&db, &execution, "waiting_human");
    step(&db, &product, &mut model);
    let waiting_on_person = row(&model, &root);
    assert_eq!(waiting_on_person.section, NavigatorSection::NeedsAttention);
    assert!(
        waiting_on_person
            .actions
            .iter()
            .any(|action| matches!(action, NavigatorAction::OpenInput { .. })),
        "a waiting_human execution with a completed dispatch run is still an input wait"
    );

    set_exec(&db, &execution, "completed");
    step(&db, &product, &mut model);
    assert_eq!(row(&model, &root).section, NavigatorSection::Backlog);
    assert!(row(&model, &root).activities.is_empty());
}

#[test]
fn archived_family_members_stay_with_their_owner_and_unresolved_chains_fall_back() {
    let db = WorkDb::open(temp_db_path("navigator-archive")).unwrap();
    let product = make_revision_product(&db, "navigator-archive");
    let root = make_in_review_chore(&db, &product, PR);
    let archived_child = insert_revision_row(&db, &product, &root);
    let live_child = insert_revision_row(&db, &product, &root);
    let intermediate = insert_revision_row(&db, &product, &root);
    let descendant = insert_revision_row(&db, &product, &intermediate);
    let mut model = None;
    step(&db, &product, &mut model);
    assert_eq!(row(&model, &root).revisions.total, 4);

    update(
        &db,
        "UPDATE tasks SET status = 'archived' WHERE id = ?1",
        &archived_child,
    );
    step(&db, &product, &mut model);
    assert_eq!(
        row(&model, &root).revisions.total,
        4,
        "an archived child stays in the family"
    );
    assert_eq!(projection(&model, &live_child).root_task_id, root);
    assert!(
        !model
            .as_ref()
            .unwrap()
            .tree()
            .tasks
            .iter()
            .any(|task| task.id == archived_child)
    );

    update(&db, "UPDATE tasks SET status = 'archived' WHERE id = ?1", &intermediate);
    update(&db, "UPDATE tasks SET status = 'active' WHERE id = ?1", &descendant);
    let nested_worker = spawn_live(&db, &descendant, ExecutionKind::RevisionImplementation);
    step(&db, &product, &mut model);
    let through_archive = row(&model, &root);
    assert_eq!(through_archive.revisions.total, 4);
    assert_eq!(projection(&model, &descendant).root_task_id, root);
    assert_eq!(through_archive.section, NavigatorSection::Working);
    assert_eq!(through_archive.activities[0].execution_id, nested_worker);
    assert!(
        model
            .as_ref()
            .unwrap()
            .tree()
            .tasks
            .iter()
            .all(|task| task.id != intermediate)
    );

    let other = in_review_named(&db, &product, "other root", OTHER_PR);
    let orphan = insert_revision_row(&db, &product, &other);
    step(&db, &product, &mut model);
    update(&db, "UPDATE tasks SET status = 'archived' WHERE id = ?1", &other);
    step(&db, &product, &mut model);
    assert!(
        !has_projection(&model, &orphan),
        "archiving a root does not mint Parent unavailable rows"
    );
    update(&db, "UPDATE tasks SET status = 'in_review' WHERE id = ?1", &other);
    step(&db, &product, &mut model);
    assert_eq!(projection(&model, &orphan).root_task_id, other);
    assert!(projection(&model, &orphan).root.is_none());

    let missing_parent = in_review_named(
        &db,
        &product,
        "missing parent",
        "https://github.com/spinyfin/mono/pull/9",
    );
    let missing_child = insert_revision_row(&db, &product, &missing_parent);
    step(&db, &product, &mut model);
    db.connect()
        .unwrap()
        .execute_batch(&format!(
            "PRAGMA foreign_keys = OFF; DELETE FROM tasks WHERE id = '{missing_parent}';"
        ))
        .unwrap();
    step(&db, &product, &mut model);
    assert_eq!(waiting(&model, &missing_child), NavigatorWait::UnavailableParent);

    let tombstoned_parent = in_review_named(
        &db,
        &product,
        "tombstoned parent",
        "https://github.com/spinyfin/mono/pull/10",
    );
    let tombstoned_child = insert_revision_row(&db, &product, &tombstoned_parent);
    step(&db, &product, &mut model);
    update(
        &db,
        "UPDATE tasks SET deleted_at = '1' WHERE id = ?1",
        &tombstoned_parent,
    );
    step(&db, &product, &mut model);
    assert_eq!(waiting(&model, &tombstoned_child), NavigatorWait::UnavailableParent);
}

#[test]
fn archived_broken_ancestor_keeps_visible_revision_fallback() {
    for deleted in [false, true] {
        let db = WorkDb::open(temp_db_path("navigator-archived-broken")).unwrap();
        let product = make_revision_product(&db, "navigator-archived-broken");
        let owner = make_in_review_chore(&db, &product, PR);
        let ancestor = insert_revision_row(&db, &product, &owner);
        let child = insert_revision_row(&db, &product, &ancestor);
        let mut model = None;
        step(&db, &product, &mut model);
        update(&db, "UPDATE tasks SET status = 'archived' WHERE id = ?1", &ancestor);
        update(&db, "UPDATE tasks SET status = 'todo' WHERE id = ?1", &child);
        step(&db, &product, &mut model);
        assert_eq!(projection(&model, &child).root_task_id, owner);
        if deleted {
            update(&db, "UPDATE tasks SET deleted_at = '1' WHERE id = ?1", &owner);
        } else {
            db.connect()
                .unwrap()
                .execute_batch(&format!(
                    "PRAGMA foreign_keys = OFF; DELETE FROM tasks WHERE id = '{owner}';"
                ))
                .unwrap();
        }
        step(&db, &product, &mut model);
        assert_eq!(projection(&model, &child).root_task_id, child);
        assert_eq!(row(&model, &child).section, NavigatorSection::Backlog);
        assert_eq!(waiting(&model, &child), NavigatorWait::UnavailableParent);
    }
}

#[test]
fn active_spawn_and_readopted_runs_are_working() {
    let db = WorkDb::open(temp_db_path("navigator-spawn-readopt")).unwrap();
    let product = make_revision_product(&db, "navigator-spawn-readopt");
    let task = create_test_chore_manual(&db, &product, "live worker").id;
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&task)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let mut model = None;
    step(&db, &product, &mut model);
    db.start_execution_run(&execution.id, "worker", "repo", "lease", "ws", "/tmp/ws")
        .unwrap();
    step(&db, &product, &mut model);
    assert_eq!(row(&model, &task).section, NavigatorSection::Working);

    set_exec(&db, &execution.id, "waiting_human");
    step(&db, &product, &mut model);
    assert_eq!(row(&model, &task).section, NavigatorSection::NeedsAttention);
    db.mark_execution_orphaned(&execution.id, "lost worker").unwrap();
    step(&db, &product, &mut model);
    assert_eq!(row(&model, &task).section, NavigatorSection::Backlog);
    db.readopt_inferred_terminal_execution(&execution.id, "hook_after_terminal", None)
        .unwrap();
    step(&db, &product, &mut model);
    assert_eq!(row(&model, &task).section, NavigatorSection::Working);
    assert_eq!(row(&model, &task).activities[0].execution_id, execution.id);
}

#[test]
fn project_worker_question_reaches_task_but_design_question_does_not() {
    let db = WorkDb::open(temp_db_path("navigator-project-question")).unwrap();
    let product = make_revision_product(&db, "navigator-project-question");
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.clone(),
            name: "Project".into(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(&product)
                .project_id(&project.id)
                .name("Task")
                .build(),
        )
        .unwrap();
    let execution = spawn_live(&db, &task.id, ExecutionKind::TaskImplementation);
    let mut model = None;
    step(&db, &product, &mut model);
    db.record_worker_awaiting_operator_answer(
        &execution,
        "decision needed",
        &boss_protocol::OperatorQuestion {
            text: "Use the proposed layout?".into(),
            answer_type: boss_protocol::OperatorAnswerType::YesNo,
            explanation: "The layout needs a decision.".into(),
        },
    )
    .unwrap()
    .expect("live worker failure");
    assert_eq!(step(&db, &product, &mut model), Refresh::Snapshot("broad_input"));
    assert_eq!(row(&model, &task.id).section, NavigatorSection::NeedsAttention);
    assert_eq!(
        actions(&model, &task.id),
        [(NavigatorHumanAction::Question, task.id.clone())]
    );
    let group: String = db.connect().unwrap().query_row(
        "SELECT id FROM attention_groups WHERE source_task_id = ?1 AND source_kind = 'task_transcript' AND association_task_id IS NULL",
        [&task.id], |row| row.get(0)).unwrap();
    db.dismiss_attention(&group, None).unwrap();
    db.create_attention(
        CreateAttentionInput::builder()
            .kind("question")
            .association_project_id(&project.id)
            .source_task_id(&task.id)
            .group_key("design-question")
            .source_kind("design_doc")
            .source_doc_path("docs/design.md")
            .question_type("yes_no")
            .prompt_text("Use this design?")
            .build(),
    )
    .unwrap();
    step(&db, &product, &mut model);
    assert_eq!(row(&model, &task.id).section, NavigatorSection::Backlog);
    assert!(actions(&model, &task.id).is_empty());
}

/// Navigator Ready is the Review column's Ready fact (`classify_review_section`),
/// except an open question keeps the row in Needs attention.
#[test]
fn navigator_ready_shares_the_kanban_ready_predicate() {
    let db = WorkDb::open(temp_db_path("navigator-ready-kanban")).unwrap();
    let product = make_revision_product(&db, "navigator-ready-kanban");
    let ready = in_review_named(&db, &product, "ready", PR);
    let pending = in_review_named(&db, &product, "pending", OTHER_PR);
    let conflicted = in_review_named(&db, &product, "conflicted", "https://github.com/spinyfin/mono/pull/9");
    let revising = in_review_named(&db, &product, "revising", "https://github.com/spinyfin/mono/pull/10");
    let asked = in_review_named(&db, &product, "asked", "https://github.com/spinyfin/mono/pull/11");
    reviewed_clean_pr(&db, &ready);
    reviewed_clean_pr(&db, &pending);
    reviewed_clean_pr(&db, &conflicted);
    reviewed_clean_pr(&db, &revising);
    reviewed_clean_pr(&db, &asked);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET ci_required_state = 'in_progress' WHERE id = ?1",
            [&pending],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_mergeable_state = 'conflicting' WHERE id = ?1",
            [&conflicted],
        )
        .unwrap();
    let revision = insert_revision_row(&db, &product, &revising);
    let question_exec = exec(&db, &asked, ExecutionKind::ChoreImplementation, "completed", None);
    ask(&db, &asked, &question_exec);

    let mut model = None;
    step(&db, &product, &mut model);

    assert_eq!(card(&model, &ready).review_section, Some(ReviewSection::Ready));
    assert_eq!(row(&model, &ready).section, NavigatorSection::Ready);
    assert_eq!(
        row(&model, &ready).reason,
        NavigatorReason::Human {
            action: NavigatorHumanAction::Review
        }
    );

    assert_eq!(card(&model, &pending).review_section, Some(ReviewSection::Pending));
    assert_eq!(row(&model, &pending).section, NavigatorSection::Backlog);
    assert_eq!(waiting(&model, &pending), NavigatorWait::WaitingChecks);

    assert_eq!(card(&model, &conflicted).review_section, Some(ReviewSection::Blocked));
    assert_eq!(row(&model, &conflicted).section, NavigatorSection::Backlog);

    assert_eq!(card(&model, &revising).review_section, Some(ReviewSection::InRevision));
    assert_eq!(row(&model, &revising).section, NavigatorSection::Backlog);
    assert_eq!(projection(&model, &revision).root_task_id, revising);

    // Kanban Ready does not look at open questions; Navigator Ready does.
    assert_eq!(card(&model, &asked).review_section, Some(ReviewSection::Ready));
    assert_eq!(row(&model, &asked).section, NavigatorSection::NeedsAttention);
    assert_eq!(
        actions(&model, &asked),
        [
            (NavigatorHumanAction::Question, asked.clone()),
            (NavigatorHumanAction::Review, asked.clone())
        ]
    );

    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET ci_required_state = 'in_progress' WHERE id = ?1",
            [&ready],
        )
        .unwrap();
    step(&db, &product, &mut model);
    assert_eq!(card(&model, &ready).review_section, Some(ReviewSection::Pending));
    assert_eq!(row(&model, &ready).section, NavigatorSection::Backlog);

    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET ci_required_state = 'success' WHERE id = ?1", [&ready])
        .unwrap();
    step(&db, &product, &mut model);
    assert_eq!(card(&model, &ready).review_section, Some(ReviewSection::Ready));
    assert_eq!(row(&model, &ready).section, NavigatorSection::Ready);
}

fn skip_head(db: &WorkDb, root: &str, sha: &str, outcome: &'static str) {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
    db.record_skip_review_verdict(&execution.id, root, Some(sha), outcome)
        .unwrap();
}

/// A new head with a review due (admission deferred) and CI still green and
/// MERGEABLE unchanged is not Ready on either surface until a verdict matches
/// that head.
#[test]
fn a_new_unreviewed_head_drops_ready_on_both_surfaces() {
    let db = WorkDb::open(temp_db_path("navigator-ready-new-head")).unwrap();
    let product = make_revision_product(&db, "navigator-ready-new-head");
    let root = in_review_named(&db, &product, "ready", PR);
    reviewed_clean_pr(&db, &root);
    let mut model = None;
    step(&db, &product, &mut model);
    assert_eq!(card(&model, &root).review_section, Some(ReviewSection::Ready));
    assert_eq!(row(&model, &root).section, NavigatorSection::Ready);
    assert_eq!(
        card(&model, &root).ai_review_state.as_deref(),
        Some("reviewed_all_clear")
    );

    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET pr_head_sha = 'head-2' WHERE id = ?1", [&root])
        .unwrap();
    db.create_attention_item(
        CreateAttentionItemInput::builder()
            .kind("pr_review_admission_deferred")
            .title("review deferred")
            .body_markdown("waiting for slot")
            .work_item_id(&root)
            .build(),
    )
    .unwrap();
    step(&db, &product, &mut model);
    assert_eq!(card(&model, &root).ci_required_state.as_deref(), Some("success"));
    assert_eq!(card(&model, &root).pr_mergeable_state.as_deref(), Some("mergeable"));
    assert_eq!(card(&model, &root).ai_review_state.as_deref(), Some("not_reviewed"));
    assert_eq!(card(&model, &root).review_section, Some(ReviewSection::AiReview));
    assert_eq!(row(&model, &root).section, NavigatorSection::Backlog);
}

/// A head moved without a Boss producer has no review due, so Ready still
/// admits it on both surfaces.
#[test]
fn a_head_moved_without_a_boss_producer_stays_ready_on_both_surfaces() {
    let db = WorkDb::open(temp_db_path("navigator-ready-human-push")).unwrap();
    let product = make_revision_product(&db, "navigator-ready-human-push");
    let root = in_review_named(&db, &product, "ready", PR);
    reviewed_clean_pr(&db, &root);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET pr_head_sha = 'head-2' WHERE id = ?1", [&root])
        .unwrap();
    let mut model = None;
    step(&db, &product, &mut model);
    assert_eq!(
        card(&model, &root).ai_review_state.as_deref(),
        Some("review_not_required")
    );
    assert_eq!(card(&model, &root).review_section, Some(ReviewSection::Ready));
    assert_eq!(row(&model, &root).section, NavigatorSection::Ready);
}

/// A revision push when revision-triggered reviews are disabled schedules no
/// reviewer, so the new head stays Ready on both surfaces.
#[test]
fn a_revision_push_without_revision_triggered_reviews_stays_ready_on_both_surfaces() {
    let db = WorkDb::open(temp_db_path("navigator-ready-revision-off")).unwrap();
    let product = make_revision_product(&db, "navigator-ready-revision-off");
    let root = in_review_named(&db, &product, "ready", PR);
    reviewed_clean_pr(&db, &root);
    let revision = insert_revision_row(&db, &product, &root);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET pr_head_sha = 'head-2' WHERE id = ?1", [&root])
        .unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'in_review' WHERE id = ?1", [&revision])
        .unwrap();
    let execution = exec(&db, &revision, ExecutionKind::RevisionImplementation, "completed", None);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET pr_head_after = 'head-2' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
    let mut model = None;
    step(&db, &product, &mut model);
    assert_eq!(
        card(&model, &root).ai_review_state.as_deref(),
        Some("review_not_required")
    );
    assert_eq!(card(&model, &root).review_section, Some(ReviewSection::Ready));
    assert_eq!(row(&model, &root).section, NavigatorSection::Ready);
}

/// A no-op / pure-rebase skip records a skip verdict on the new head. Ready
/// must still admit it on both surfaces.
#[test]
fn a_noop_skipped_head_after_a_clean_verdict_stays_ready_on_both_surfaces() {
    let db = WorkDb::open(temp_db_path("navigator-ready-skipped-head")).unwrap();
    let product = make_revision_product(&db, "navigator-ready-skipped-head");
    let root = in_review_named(&db, &product, "ready", PR);
    reviewed_clean_pr(&db, &root);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET pr_head_sha = 'head-2' WHERE id = ?1", [&root])
        .unwrap();
    skip_head(&db, &root, "head-2", crate::REVIEW_GATE_OUTCOME_SKIPPED_NOOP);
    let mut model = None;
    step(&db, &product, &mut model);
    assert_eq!(
        card(&model, &root).ai_review_state.as_deref(),
        Some("review_not_required")
    );
    assert_eq!(card(&model, &root).review_section, Some(ReviewSection::Ready));
    assert_eq!(row(&model, &root).section, NavigatorSection::Ready);
}

/// Cycle-capped heads record a cycle-cap skip verdict on the chain root while
/// the attention item lives on the producing revision child. Ready must admit
/// them on both surfaces.
#[test]
fn a_cycle_capped_head_stays_ready_on_both_surfaces() {
    let db = WorkDb::open(temp_db_path("navigator-ready-cycle-cap")).unwrap();
    let product = make_revision_product(&db, "navigator-ready-cycle-cap");
    let root = in_review_named(&db, &product, "ready", PR);
    reviewed_clean_pr(&db, &root);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET pr_head_sha = 'head-2' WHERE id = ?1", [&root])
        .unwrap();
    let revision = insert_revision_row(&db, &product, &root);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'in_review' WHERE id = ?1", [&revision])
        .unwrap();
    db.create_attention_item(
        CreateAttentionItemInput::builder()
            .kind("pr_review_cycle_bound")
            .title("cycle limit")
            .body_markdown("capped")
            .work_item_id(&revision)
            .build(),
    )
    .unwrap();
    skip_head(&db, &root, "head-2", crate::REVIEW_GATE_OUTCOME_SKIPPED_CYCLE_CAP);
    let mut model = None;
    step(&db, &product, &mut model);
    assert_eq!(
        card(&model, &root).ai_review_state.as_deref(),
        Some("review_not_required")
    );
    assert_eq!(card(&model, &root).review_section, Some(ReviewSection::Ready));
    assert_eq!(row(&model, &root).section, NavigatorSection::Ready);
}

/// A fresh `gave_up` head is still recoverable by `pr_review_recovery`, so it
/// stays out of Ready on both surfaces.
#[test]
fn a_fresh_gave_up_head_drops_ready_on_both_surfaces() {
    let db = WorkDb::open(temp_db_path("navigator-ready-gave-up-fresh")).unwrap();
    let product = make_revision_product(&db, "navigator-ready-gave-up-fresh");
    let root = in_review_named(&db, &product, "ready", PR);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = ?2, ci_required_state = 'success', pr_mergeable_state = 'mergeable'
             WHERE id = ?1",
            params![root, HEAD],
        )
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&root)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
    WorkDb::insert_review_verdict_in_tx(
        &db.connect().unwrap(),
        &execution.id,
        &root,
        &crate::ReviewVerdictInput {
            head_sha: Some(HEAD.to_owned()),
            findings_count: 0,
            revision_warranted: false,
            gate_outcome: crate::REVIEW_GATE_OUTCOME_GAVE_UP,
        },
    )
    .unwrap();
    let mut model = None;
    step(&db, &product, &mut model);
    assert_eq!(card(&model, &root).ai_review_state.as_deref(), Some("not_reviewed"));
    assert_eq!(card(&model, &root).review_section, Some(ReviewSection::AiReview));
    assert_eq!(row(&model, &root).section, NavigatorSection::Backlog);
}

/// An exhausted `gave_up` head will not be re-fired by recovery, so Ready
/// admits it on both surfaces.
#[test]
fn an_exhausted_gave_up_head_stays_ready_on_both_surfaces() {
    let db = WorkDb::open(temp_db_path("navigator-ready-gave-up-exhausted")).unwrap();
    let product = make_revision_product(&db, "navigator-ready-gave-up-exhausted");
    let root = in_review_named(&db, &product, "ready", PR);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = ?2, ci_required_state = 'success', pr_mergeable_state = 'mergeable'
             WHERE id = ?1",
            params![root, HEAD],
        )
        .unwrap();
    let now = boss_engine_utils::epoch_time::now_epoch_secs().to_string();
    for _ in 0..crate::ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD {
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(&root)
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'completed', created_at = ?2 WHERE id = ?1",
                params![execution.id, now],
            )
            .unwrap();
        WorkDb::insert_review_verdict_in_tx(
            &db.connect().unwrap(),
            &execution.id,
            &root,
            &crate::ReviewVerdictInput {
                head_sha: Some(HEAD.to_owned()),
                findings_count: 0,
                revision_warranted: false,
                gate_outcome: crate::REVIEW_GATE_OUTCOME_GAVE_UP,
            },
        )
        .unwrap();
    }
    let mut model = None;
    step(&db, &product, &mut model);
    assert_eq!(
        card(&model, &root).ai_review_state.as_deref(),
        Some("review_not_required")
    );
    assert_eq!(card(&model, &root).review_section, Some(ReviewSection::Ready));
    assert_eq!(row(&model, &root).section, NavigatorSection::Ready);
}

/// A findings revision that delivered without moving the head is not due
/// another review, so Ready admits it on both surfaces.
#[test]
fn a_delivered_unmoved_findings_revision_reaches_ready_on_both_surfaces() {
    let db = WorkDb::open(temp_db_path("navigator-ready-unmoved-findings")).unwrap();
    let product = make_revision_product(&db, "navigator-ready-unmoved-findings");
    let root = in_review_named(&db, &product, "ready", PR);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = ?2, ci_required_state = 'success', pr_mergeable_state = 'mergeable'
             WHERE id = ?1",
            params![root, HEAD],
        )
        .unwrap();
    let revision = insert_revision_row(&db, &product, &root);
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&root)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
        WorkDb::insert_review_verdict_in_tx(
            &conn,
            &execution.id,
            &root,
            &crate::ReviewVerdictInput {
                head_sha: Some(HEAD.to_owned()),
                findings_count: 2,
                revision_warranted: true,
                gate_outcome: "completed_with_findings",
            },
        )
        .unwrap();
    }
    db.set_review_verdict_revision_task_id(&execution.id, &revision)
        .unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'in_review' WHERE id = ?1", [&revision])
        .unwrap();
    let fix = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&revision)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed', finished_at = '100',
                    pr_head_after = NULL WHERE id = ?1",
            [&fix.id],
        )
        .unwrap();
    let mut model = None;
    step(&db, &product, &mut model);
    assert_eq!(
        card(&model, &root).ai_review_state.as_deref(),
        Some("review_not_required")
    );
    assert_eq!(card(&model, &root).review_section, Some(ReviewSection::Ready));
    assert_eq!(row(&model, &root).section, NavigatorSection::Ready);
}
