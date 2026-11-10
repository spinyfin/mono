//! Scope resolution for dispatch focus mode: which work items and ready
//! executions fall inside a focus set, and the persistence and target
//! resolution that surround it.

use boss_protocol::{DispatchFocus, DispatchFocusTargetKind, ExecutionKind, ExecutionStatus};

use super::*;

fn project(db: &WorkDb, product_id: &str, name: &str, with_design_task: bool) -> Project {
    db.create_project(CreateProjectInput {
        product_id: product_id.to_owned(),
        name: name.to_owned(),
        description: None,
        goal: None,
        autostart: false,
        no_design_task: !with_design_task,
        design_reasoning_effort_xhigh: false,
    })
    .unwrap()
}

fn project_task(db: &WorkDb, product_id: &str, project_id: &str, name: &str) -> Task {
    db.create_task(
        CreateTaskInput::builder()
            .product_id(product_id)
            .project_id(project_id)
            .name(name)
            .autostart(false)
            .build(),
    )
    .unwrap()
}

fn focus_on_project(project_id: &str) -> DispatchFocus {
    let mut focus = DispatchFocus::default();
    focus.insert(DispatchFocusTargetKind::Project, project_id.to_owned());
    focus
}

fn focus_on_product(product_id: &str) -> DispatchFocus {
    let mut focus = DispatchFocus::default();
    focus.insert(DispatchFocusTargetKind::Product, product_id.to_owned());
    focus
}

fn in_focus(db: &WorkDb, id: &str, focus: &DispatchFocus) -> bool {
    db.work_item_in_dispatch_focus(id, focus).unwrap()
}

/// A merged-PR chain root a followup can hang off.
fn merged_origin(db: &WorkDb, product_id: &str, project_id: Option<&str>, name: &str) -> Task {
    let origin = match project_id {
        Some(project_id) => project_task(db, product_id, project_id, name),
        None => create_test_chore_manual(db, product_id, name),
    };
    db.update_work_item(
        &origin.id,
        WorkItemPatch {
            pr_url: Some("https://github.com/org/repo/pull/321".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    origin
}

fn followup_of(db: &WorkDb, product_id: &str, origin: &Task, created_via: &str) -> Task {
    db.create_review_findings_followup(
        review_findings_followup::ReviewFindingsFollowupInsert::builder()
            .product_id(product_id)
            .name("Review findings")
            .created_via(created_via)
            .description("Fix the finding")
            .chain_root_id(origin.id.clone())
            .autostart(false)
            .build(),
    )
    .unwrap()
}

fn ready_execution(db: &WorkDb, work_item_id: &str, kind: ExecutionKind) -> WorkExecution {
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(work_item_id)
            .kind(kind)
            .status(ExecutionStatus::Ready)
            .build(),
    )
    .unwrap()
}

#[test]
fn explicit_launch_admits_only_its_lineage_after_database_reload() {
    let (_dir, path) = migrated_disk_db_path("focus-reload");
    let db = WorkDb::open_existing(path.clone()).unwrap();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused", false);
    let focus = focus_on_project(&focused.id);
    db.save_dispatch_focus(&focus).unwrap();
    let launched = create_test_chore_manual(&db, &product.id, "Explicitly launched");
    let sibling = create_test_chore_manual(&db, &product.id, "Unlaunched sibling");
    let request = |id: &str, force| RequestExecutionInput::builder().work_item_id(id).force(force).build();
    let queued = db.request_execution(request(&launched.id, false)).unwrap();
    assert!(!in_focus(&db, &launched.id, &focus));
    let reused = db.request_execution(request(&launched.id, true)).unwrap();
    assert_eq!(queued.id, reused.id, "an existing ready row also records launch intent");
    db.request_execution(request(&sibling.id, false)).unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = 'https://github.com/org/repo/pull/' || short_id WHERE id IN (?1, ?2)",
            params![launched.id, sibling.id],
        )
        .unwrap();

    let mut admitted = Vec::new();
    let mut held = Vec::new();
    for (root, executions) in [(&launched, &mut admitted), (&sibling, &mut held)] {
        executions.push(ready_execution(&db, &root.id, ExecutionKind::PrReview));
        let (_, comparison) = crate::test_support::seed_review_guide_series_for_pr(
            &db,
            &root.id,
            &format!("https://github.com/org/repo/pull/{}", root.short_id.unwrap()),
            "base",
            "head",
        );
        executions.push(db.create_pr_review_guide_execution(&comparison, "org/repo").unwrap());
        for reason in ["pr_review:pre_merge:test", "ci-fix:test", "merge-conflict:test"] {
            let revision = insert_revision_row(&db, &product.id, &root.id);
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE tasks SET created_via = ?2 WHERE id = ?1",
                    params![revision, reason],
                )
                .unwrap();
            executions.push(ready_execution(&db, &revision, ExecutionKind::RevisionImplementation));
        }
        let followup = followup_of(&db, &product.id, root, &format!("pr_review:post_merge:{}", root.id));
        executions.push(ready_execution(&db, &followup.id, ExecutionKind::ChoreImplementation));
        let revision = insert_revision_row(&db, &product.id, &followup.id);
        executions.push(ready_execution(&db, &revision, ExecutionKind::RevisionImplementation));
    }
    // Completing the implementation and closing its root must not consume
    // the exemption: post-merge work still needs it after a restart.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'done' WHERE id IN (?1, ?2)",
            params![launched.id, sibling.id],
        )
        .unwrap();
    drop(db);
    let db = WorkDb::open_existing(path).unwrap();
    let restored_focus = db.load_dispatch_focus().unwrap();
    assert_eq!(restored_focus, focus);
    let expected: HashSet<_> = held.iter().map(|execution| execution.id.clone()).collect();
    admitted.extend(held);
    assert_eq!(
        db.executions_outside_dispatch_focus(&admitted, &restored_focus)
            .unwrap(),
        expected
    );
}

#[test]
fn launching_an_intermediate_revision_admits_descendants_but_not_its_root_or_siblings() {
    let db = WorkDb::open(temp_db_path("focus-launched-revision")).unwrap();
    let product = create_test_product(&db);
    let focus = focus_on_project("proj_elsewhere");
    let root = merged_origin(&db, &product.id, None, "Unlaunched root");
    let launched = insert_revision_row(&db, &product.id, &root.id);
    let descendant = insert_revision_row(&db, &product.id, &launched);
    let sibling = insert_revision_row(&db, &product.id, &root.id);
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(&launched)
            .force(true)
            .build(),
    )
    .unwrap();
    assert!(in_focus(&db, &launched, &focus));
    assert!(in_focus(&db, &descendant, &focus));
    assert!(!in_focus(&db, &root.id, &focus));
    assert!(!in_focus(&db, &sibling, &focus));
}

#[test]
fn a_refused_launch_and_a_pause_only_override_do_not_grant_focus_admission() {
    let db = WorkDb::open(temp_db_path("focus-launch-refusal")).unwrap();
    let product = create_test_product(&db);
    let focus = focus_on_project("proj_elsewhere");
    let task = create_test_chore_manual(&db, &product.id, "Pause override");
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(&task.id)
            .bypass_dispatch_pause(true)
            .build(),
    )
    .unwrap();
    assert!(!in_focus(&db, &task.id, &focus));
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&task.id])
        .unwrap();
    let error = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&task.id)
                .force(true)
                .build(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("terminal"), "{error}");
    assert!(!in_focus(&db, &task.id, &focus));
}

#[test]
fn an_empty_focus_set_contains_everything() {
    let db = WorkDb::open(temp_db_path("focus-empty")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Anything");
    let focus = DispatchFocus::default();
    assert!(!focus.is_active());
    assert!(in_focus(&db, &chore.id, &focus));
    let execution = ready_execution(&db, &chore.id, ExecutionKind::ChoreImplementation);
    assert!(
        db.executions_outside_dispatch_focus(&[execution], &focus)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_focused_project_admits_its_tasks_and_design_seed_task_and_nothing_else() {
    let db = WorkDb::open(temp_db_path("focus-project")).unwrap();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused", true);
    let other = project(&db, &product.id, "Other", true);
    let member = project_task(&db, &product.id, &focused.id, "Member");
    let outsider = project_task(&db, &product.id, &other.id, "Outsider");
    let loose_chore = create_test_chore_manual(&db, product.id.clone(), "Loose chore");
    let focus = focus_on_project(&focused.id);

    assert!(in_focus(&db, &member.id, &focus));
    assert!(!in_focus(&db, &outsider.id, &focus));
    assert!(
        !in_focus(&db, &loose_chore.id, &focus),
        "a product-level chore is not part of a focused project"
    );

    let design_task_id: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT id FROM tasks WHERE project_id = ?1 AND kind = 'design'",
            [&focused.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        in_focus(&db, &design_task_id, &focus),
        "a project's design seed task counts as part of its project"
    );
    let other_design_task_id: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT id FROM tasks WHERE project_id = ?1 AND kind = 'design'",
            [&other.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!in_focus(&db, &other_design_task_id, &focus));
}

#[test]
fn a_focused_product_admits_every_project_and_loose_work_in_it() {
    let db = WorkDb::open(temp_db_path("focus-product")).unwrap();
    let focused = create_test_product_named(&db, "Focused");
    let other = create_test_product_named(&db, "Other");
    let in_project = {
        let p = project(&db, &focused.id, "P", false);
        project_task(&db, &focused.id, &p.id, "In a project")
    };
    let loose = create_test_chore_manual(&db, focused.id.clone(), "Loose");
    let elsewhere = create_test_chore_manual(&db, other.id.clone(), "Elsewhere");
    let focus = focus_on_product(&focused.id);

    assert!(in_focus(&db, &in_project.id, &focus));
    assert!(in_focus(&db, &loose.id, &focus));
    assert!(!in_focus(&db, &elsewhere.id, &focus));
}

#[test]
fn a_revision_is_in_scope_through_its_chain_root() {
    let db = WorkDb::open(temp_db_path("focus-revision")).unwrap();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused", false);
    let other = project(&db, &product.id, "Other", false);
    let root = project_task(&db, &product.id, &focused.id, "Root");
    let other_root = project_task(&db, &product.id, &other.id, "Other root");
    // The engine-minted revision rows carry no project of their own.
    let revision = insert_revision_row(&db, &product.id, &root.id);
    let other_revision = insert_revision_row(&db, &product.id, &other_root.id);
    let focus = focus_on_project(&focused.id);

    assert!(in_focus(&db, &revision, &focus));
    assert!(!in_focus(&db, &other_revision, &focus));

    // A review revision's execution is held or admitted by the same rule.
    let admitted = ready_execution(&db, &revision, ExecutionKind::RevisionImplementation);
    let held = ready_execution(&db, &other_revision, ExecutionKind::RevisionImplementation);
    let outside = db
        .executions_outside_dispatch_focus(&[admitted.clone(), held.clone()], &focus)
        .unwrap();
    assert!(!outside.contains(&admitted.id));
    assert!(outside.contains(&held.id));
}

#[test]
fn a_post_merge_followup_is_in_scope_through_its_origin_even_after_leaving_the_project() {
    let db = WorkDb::open(temp_db_path("focus-followup")).unwrap();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused", false);
    let other = project(&db, &product.id, "Other", false);
    let origin = merged_origin(&db, &product.id, Some(&focused.id), "Origin");
    let other_origin = merged_origin(&db, &product.id, Some(&other.id), "Other origin");
    let followup = followup_of(&db, &product.id, &origin, "pr_review:post_merge:prop_1");
    let other_followup = followup_of(&db, &product.id, &other_origin, "pr_review:post_merge:prop_2");
    let focus = focus_on_project(&focused.id);

    // Minted inside the origin's project, so in scope directly...
    assert!(in_focus(&db, &followup.id, &focus));
    assert!(!in_focus(&db, &other_followup.id, &focus));

    // ...and still in scope by lineage once it no longer carries a project.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET project_id = NULL WHERE id IN (?1, ?2)",
            params![followup.id, other_followup.id],
        )
        .unwrap();
    assert!(in_focus(&db, &followup.id, &focus));
    assert!(!in_focus(&db, &other_followup.id, &focus));

    // A revision of that followup resolves through its chain root, then the
    // followup's origin.
    let revision = insert_revision_row(&db, &product.id, &followup.id);
    assert!(in_focus(&db, &revision, &focus));
}

#[test]
fn review_executions_are_attributed_to_the_task_they_review() {
    let db = WorkDb::open(temp_db_path("focus-review")).unwrap();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused", false);
    let other = project(&db, &product.id, "Other", false);
    let reviewed = project_task(&db, &product.id, &focused.id, "Reviewed");
    let unreviewed = project_task(&db, &product.id, &other.id, "Other PR");
    let revision = insert_revision_row(&db, &product.id, &reviewed.id);
    let in_scope = ready_execution(&db, &reviewed.id, ExecutionKind::PrReview);
    let on_revision = ready_execution(&db, &revision, ExecutionKind::PrReview);
    let out_of_scope = ready_execution(&db, &unreviewed.id, ExecutionKind::PrReview);
    let focus = focus_on_project(&focused.id);

    let outside = db
        .executions_outside_dispatch_focus(&[in_scope.clone(), on_revision.clone(), out_of_scope.clone()], &focus)
        .unwrap();
    assert_eq!(outside, HashSet::from([out_of_scope.id]));
}

#[test]
fn an_unattributable_execution_is_out_of_scope() {
    let db = WorkDb::open(temp_db_path("focus-unattributable")).unwrap();
    let product = create_test_product(&db);
    let focus = focus_on_product(&product.id);
    assert!(!in_focus(&db, "task_does_not_exist", &focus));
    assert!(!in_focus(&db, "cmt_does_not_exist", &focus));
    let comment = comment_on(&db, "work_item", "cmt_self");
    assert!(!in_focus(&db, &comment.id, &focus));
    assert!(!in_focus(&db, "prgc_does_not_exist", &focus));
    assert!(!in_focus(&db, "not-an-id", &focus));
}

#[test]
fn the_focus_set_round_trips_through_the_metadata_kv() {
    let db = WorkDb::open(temp_db_path("focus-persist")).unwrap();
    assert_eq!(db.load_dispatch_focus().unwrap(), DispatchFocus::default());
    let mut focus = focus_on_project("proj_a");
    focus.insert(DispatchFocusTargetKind::Product, "prod_b".to_owned());
    db.save_dispatch_focus(&focus).unwrap();
    assert_eq!(db.load_dispatch_focus().unwrap(), focus);
    db.save_dispatch_focus(&DispatchFocus::default()).unwrap();
    assert!(!db.load_dispatch_focus().unwrap().is_active());
}

#[test]
fn targets_resolve_by_id_short_id_and_product_slug_and_refuse_tasks() {
    let db = WorkDb::open(temp_db_path("focus-targets")).unwrap();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused", false);
    let task = project_task(&db, &product.id, &focused.id, "A task");
    let project_short_id = focused.short_id.expect("projects carry short ids");

    assert_eq!(
        db.resolve_dispatch_focus_target(&focused.id).unwrap(),
        (DispatchFocusTargetKind::Project, focused.id.clone())
    );
    assert_eq!(
        db.resolve_dispatch_focus_target(&format!("P{project_short_id}"))
            .unwrap(),
        (DispatchFocusTargetKind::Project, focused.id.clone())
    );
    assert_eq!(
        db.resolve_dispatch_focus_target(&product.id).unwrap(),
        (DispatchFocusTargetKind::Product, product.id.clone())
    );
    assert_eq!(
        db.resolve_dispatch_focus_target(&product.slug).unwrap(),
        (DispatchFocusTargetKind::Product, product.id.clone())
    );
    let task_error = db.resolve_dispatch_focus_target(&task.id).unwrap_err().to_string();
    assert!(task_error.contains("not a project or product"), "{task_error}");
    let task_short_error = db
        .resolve_dispatch_focus_target(&format!("T{}", task.short_id.unwrap()))
        .unwrap_err()
        .to_string();
    assert!(
        task_short_error.contains("not a project or product"),
        "{task_short_error}"
    );
    assert!(db.resolve_dispatch_focus_target("proj_missing").is_err());
    assert!(db.resolve_dispatch_focus_target("nonsense-slug").is_err());
    assert!(db.resolve_dispatch_focus_target("  ").is_err());
}

#[test]
fn describe_resolves_names_and_survives_missing_rows() {
    let db = WorkDb::open(temp_db_path("focus-describe")).unwrap();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused", false);
    let mut focus = focus_on_project(&focused.id);
    focus.insert(DispatchFocusTargetKind::Product, product.id.clone());
    focus.insert(DispatchFocusTargetKind::Project, "proj_gone".to_owned());
    let targets = db.describe_dispatch_focus(&focus).unwrap();
    assert_eq!(targets.len(), 3);
    let named = targets.iter().find(|t| t.id == focused.id).unwrap();
    assert_eq!(named.name, "Focused");
    assert_eq!(named.short_id, focused.short_id);
    assert_eq!(targets.iter().find(|t| t.id == product.id).unwrap().name, product.slug);
    assert_eq!(targets.iter().find(|t| t.id == "proj_gone").unwrap().name, "");
}

fn comment_on(db: &WorkDb, artifact_kind: &str, artifact_id: &str) -> WorkComment {
    db.create_comment(CreateCommentInput {
        artifact_kind: artifact_kind.to_owned(),
        artifact_id: artifact_id.to_owned(),
        doc_version: "v0".to_owned(),
        anchor: CommentAnchor {
            exact: "text".to_owned(),
            prefix: String::new(),
            suffix: String::new(),
        },
        body: "why?".to_owned(),
        author: "user:test@example.com".to_owned(),
        plain_text_projection_version: 1,
    })
    .unwrap()
}

#[test]
fn answer_agent_on_a_pr_doc_comment_follows_the_design_task_that_owns_the_doc() {
    const REPO: &str = "https://github.com/acme/widget.git";
    const DOC_PATH: &str = "docs/design.md";
    let db = WorkDb::open(temp_db_path("focus-answer-pr-doc")).unwrap();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused", true);
    let other = project(&db, &product.id, "Other", true);
    for (project, path) in [(&focused, DOC_PATH), (&other, "docs/other.md")] {
        db.set_project_design_doc(SetProjectDesignDocInput {
            project_id: project.id.clone(),
            design_doc_repo_remote_url: Some(REPO.to_owned()),
            design_doc_branch: Some("main".to_owned()),
            design_doc_path: Some(path.to_owned()),
            unset: false,
        })
        .unwrap();
    }
    let focus = focus_on_project(&focused.id);

    let on_focused = comment_on(&db, "pr_doc", &format!("pr_doc:{REPO}:main:{DOC_PATH}"));
    let on_other = comment_on(&db, "pr_doc", &format!("pr_doc:{REPO}:main:docs/other.md"));
    let on_unowned = comment_on(&db, "pr_doc", &format!("pr_doc:{REPO}:main:docs/nobody.md"));
    assert!(in_focus(&db, &on_focused.id, &focus));
    assert!(!in_focus(&db, &on_other.id, &focus));
    assert!(!in_focus(&db, &on_unowned.id, &focus));
}

/// A project task with an open PR and a published review guide; returns the
/// guide series id.
fn published_guide_on(db: &WorkDb, product_id: &str, project_id: &str, pr_number: u32) -> String {
    use crate::test_support::seed_review_guide_series_for_pr;
    let root = project_task(db, product_id, project_id, &format!("PR {pr_number}"));
    let pr_url = format!("https://github.com/acme/widget/pull/{pr_number}");
    db.update_work_item(
        &root.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some(pr_url.clone()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let (series, comparison) = seed_review_guide_series_for_pr(db, &root.id, &pr_url, "base", "head");
    let attempt = db
        .create_pr_review_guide_attempt(&series, &comparison, "review-guide-v1")
        .unwrap();
    db.publish_pr_review_guide_version(&attempt.id, "# Guide\n\nOriginal quote", "raw")
        .unwrap();
    series
}

#[test]
fn answer_agent_on_a_review_guide_comment_follows_the_series_root_task() {
    use crate::test_support::{add_guide_comment, open_db};
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused", false);
    let other = project(&db, &product.id, "Other", false);
    let in_series = published_guide_on(&db, &product.id, &focused.id, 9);
    let out_series = published_guide_on(&db, &product.id, &other.id, 10);
    let focus = focus_on_project(&focused.id);

    let in_comment = add_guide_comment(&db, &in_series, "why?");
    let out_comment = add_guide_comment(&db, &out_series, "why?");
    assert!(in_focus(&db, &in_comment.id, &focus));
    assert!(!in_focus(&db, &out_comment.id, &focus));
}

#[test]
fn answer_agent_on_a_work_item_comment_follows_the_work_item() {
    let db = WorkDb::open(temp_db_path("focus-answer-work-item")).unwrap();
    let product = create_test_product(&db);
    let focused = project(&db, &product.id, "Focused", false);
    let other = project(&db, &product.id, "Other", false);
    let inside = project_task(&db, &product.id, &focused.id, "Inside");
    let outside = project_task(&db, &product.id, &other.id, "Outside");
    let focus = focus_on_project(&focused.id);
    assert!(in_focus(&db, &comment_on(&db, "work_item", &inside.id).id, &focus));
    assert!(!in_focus(&db, &comment_on(&db, "work_item", &outside.id).id, &focus));
}
