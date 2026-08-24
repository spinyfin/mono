//! Design-doc pointer tests: project-level detector sync, property-audit
//! row recording, on-approve conflict surfacing, and the per-task
//! doc-pointer (any work-item kind).

use super::*;

#[test]
fn sync_project_design_doc_from_detector_populates_when_null() {
    let path = temp_db_path("detector-sync-empty");
    let db = WorkDb::open(path.clone()).unwrap();
    let (_product, project) = seed_project_for_design_doc(&db);

    let wrote = db
        .sync_project_design_doc_from_detector(
            &project.id,
            Some("git@github.com:spinyfin/mono.git"),
            Some("main"),
            "tools/boss/docs/designs/foo.md",
        )
        .unwrap();
    assert!(wrote, "expected the detector hook to write");

    let updated = db.get_project(&project.id).unwrap();
    assert_eq!(
        updated.design_doc_path.as_deref(),
        Some("tools/boss/docs/designs/foo.md"),
    );
    assert_eq!(
        updated.design_doc_repo_remote_url.as_deref(),
        Some("git@github.com:spinyfin/mono.git"),
    );
    assert_eq!(updated.design_doc_branch.as_deref(), Some("main"));

    let _ = std::fs::remove_file(path);
}

#[test]
fn sync_project_design_doc_from_detector_skips_when_pointer_set() {
    let path = temp_db_path("detector-sync-skip");
    let db = WorkDb::open(path.clone()).unwrap();
    let (_product, project) = seed_project_for_design_doc(&db);

    db.set_project_design_doc(set_design_doc_input(&project.id, "tools/boss/docs/designs/manual.md"))
        .unwrap();

    let wrote = db
        .sync_project_design_doc_from_detector(
            &project.id,
            Some("git@github.com:spinyfin/mono.git"),
            Some("main"),
            "tools/boss/docs/designs/from-detector.md",
        )
        .unwrap();
    assert!(!wrote, "expected the detector hook to no-op");

    let unchanged = db.get_project(&project.id).unwrap();
    assert_eq!(
        unchanged.design_doc_path.as_deref(),
        Some("tools/boss/docs/designs/manual.md"),
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn sync_project_design_doc_from_detector_validates_path() {
    let path = temp_db_path("detector-sync-bad-path");
    let db = WorkDb::open(path.clone()).unwrap();
    let (_product, project) = seed_project_for_design_doc(&db);

    let err = db
        .sync_project_design_doc_from_detector(&project.id, None, None, "/absolute/path.md")
        .unwrap_err()
        .to_string();
    assert!(err.contains("repo-relative"), "got: {err}");

    let _ = std::fs::remove_file(path);
}

#[test]
fn audit_records_first_set_as_old_null_new_value() {
    let path = temp_db_path("audit-first-set");
    let db = WorkDb::open(path.clone()).unwrap();
    let (_product, project) = seed_project_for_design_doc(&db);

    db.set_project_design_doc(set_design_doc_input(&project.id, "tools/boss/docs/designs/foo.md"))
        .unwrap();

    let audit = db.list_project_property_audit(&project.id).unwrap();
    assert_eq!(
        audit.len(),
        1,
        "path-only edit on a fresh project should produce exactly one row, got {audit:#?}",
    );
    assert_eq!(audit[0].property, "design_doc_path");
    assert!(audit[0].old_value.is_none());
    assert_eq!(audit[0].new_value.as_deref(), Some("tools/boss/docs/designs/foo.md"),);
    assert_eq!(audit[0].actor, AUDIT_ACTOR_HUMAN);
    assert_eq!(audit[0].project_id, project.id);

    let _ = std::fs::remove_file(path);
}

#[test]
fn audit_records_one_row_per_changed_column() {
    let path = temp_db_path("audit-three-cols");
    let db = WorkDb::open(path.clone()).unwrap();
    let (_product, project) = seed_project_for_design_doc(&db);

    db.set_project_design_doc(SetProjectDesignDocInput {
        project_id: project.id.clone(),
        design_doc_repo_remote_url: Some("https://github.com/myorg/wiki.git".to_owned()),
        design_doc_branch: Some("docs".to_owned()),
        design_doc_path: Some("designs/foo.md".to_owned()),
        unset: false,
    })
    .unwrap();

    let audit = db.list_project_property_audit(&project.id).unwrap();
    let properties: HashSet<&str> = audit.iter().map(|e| e.property.as_str()).collect();
    assert_eq!(properties.len(), 3, "got: {audit:#?}");
    assert!(properties.contains("design_doc_repo_remote_url"));
    assert!(properties.contains("design_doc_branch"));
    assert!(properties.contains("design_doc_path"));

    let _ = std::fs::remove_file(path);
}

#[test]
fn audit_no_op_writes_emit_no_extra_rows() {
    let path = temp_db_path("audit-noop");
    let db = WorkDb::open(path.clone()).unwrap();
    let (_product, project) = seed_project_for_design_doc(&db);

    let input = SetProjectDesignDocInput {
        project_id: project.id.clone(),
        design_doc_repo_remote_url: Some("https://github.com/myorg/wiki.git".to_owned()),
        design_doc_branch: Some("docs".to_owned()),
        design_doc_path: Some("designs/foo.md".to_owned()),
        unset: false,
    };
    db.set_project_design_doc(input.clone()).unwrap();
    let after_first = db.list_project_property_audit(&project.id).unwrap().len();
    db.set_project_design_doc(input).unwrap();
    let after_second = db.list_project_property_audit(&project.id).unwrap().len();
    assert_eq!(
        after_first, after_second,
        "second identical write should not emit any audit rows",
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn audit_records_unset_as_old_value_new_null() {
    let path = temp_db_path("audit-unset");
    let db = WorkDb::open(path.clone()).unwrap();
    let (_product, project) = seed_project_for_design_doc(&db);

    db.set_project_design_doc(SetProjectDesignDocInput {
        project_id: project.id.clone(),
        design_doc_repo_remote_url: Some("https://github.com/myorg/wiki.git".to_owned()),
        design_doc_branch: Some("docs".to_owned()),
        design_doc_path: Some("designs/foo.md".to_owned()),
        unset: false,
    })
    .unwrap();
    db.set_project_design_doc(SetProjectDesignDocInput {
        project_id: project.id.clone(),
        design_doc_repo_remote_url: None,
        design_doc_branch: None,
        design_doc_path: None,
        unset: true,
    })
    .unwrap();

    let audit = db.list_project_property_audit(&project.id).unwrap();
    assert_eq!(audit.len(), 6, "3 set + 3 unset = 6 rows, got: {audit:#?}",);
    for entry in &audit[3..] {
        assert!(
            entry.old_value.is_some(),
            "unset row should retain the prior value as old_value",
        );
        assert!(entry.new_value.is_none(), "unset row should record new_value as NULL",);
        assert_eq!(entry.actor, AUDIT_ACTOR_HUMAN);
    }

    let _ = std::fs::remove_file(path);
}

#[test]
fn audit_records_detector_actor_on_sync() {
    let path = temp_db_path("audit-detector-actor");
    let db = WorkDb::open(path.clone()).unwrap();
    let (_product, project) = seed_project_for_design_doc(&db);

    let wrote = db
        .sync_project_design_doc_from_detector(
            &project.id,
            Some("git@github.com:spinyfin/mono.git"),
            Some("main"),
            "tools/boss/docs/designs/foo.md",
        )
        .unwrap();
    assert!(wrote);

    let audit = db.list_project_property_audit(&project.id).unwrap();
    assert!(!audit.is_empty(), "detector sync should emit at least one audit row",);
    for entry in &audit {
        assert_eq!(
            entry.actor, AUDIT_ACTOR_DESIGN_DETECTOR,
            "detector-sync rows must carry the engine actor: {entry:#?}",
        );
    }
    let property_set: HashSet<&str> = audit.iter().map(|e| e.property.as_str()).collect();
    assert!(property_set.contains("design_doc_path"));

    let _ = std::fs::remove_file(path);
}

#[test]
fn surface_design_doc_conflict_on_approve_no_pointer_is_no_op() {
    let path = temp_db_path("approve-conflict-no-pointer");
    let db = WorkDb::open(path.clone()).unwrap();
    let (product, project) = seed_project_for_design_doc(&db);
    let execution = seed_execution_for(&db, &product.id, &project.id);

    let item = db
        .surface_design_doc_conflict_on_approve(
            &project.id,
            &execution.id,
            None,
            None,
            "tools/boss/docs/designs/foo.md",
        )
        .unwrap();
    assert!(item.is_none());
    assert!(db.list_attention_items(&execution.id).unwrap().is_empty());

    let _ = std::fs::remove_file(path);
}

#[test]
fn surface_design_doc_conflict_on_approve_silent_when_pointer_matches() {
    let path = temp_db_path("approve-conflict-match");
    let db = WorkDb::open(path.clone()).unwrap();
    let (product, project) = seed_project_for_design_doc(&db);
    let execution = seed_execution_for(&db, &product.id, &project.id);

    db.set_project_design_doc(set_design_doc_input(&project.id, "tools/boss/docs/designs/foo.md"))
        .unwrap();

    // Approved doc matches: same path, inherits same repo, default
    // branch matches the resolved default.
    let item = db
        .surface_design_doc_conflict_on_approve(
            &project.id,
            &execution.id,
            None,
            None,
            "tools/boss/docs/designs/foo.md",
        )
        .unwrap();
    assert!(item.is_none(), "expected silent no-op when pointers agree");

    let _ = std::fs::remove_file(path);
}

#[test]
fn surface_design_doc_conflict_on_approve_emits_attention_item_when_pointer_differs() {
    let path = temp_db_path("approve-conflict-emits");
    let db = WorkDb::open(path.clone()).unwrap();
    let (product, project) = seed_project_for_design_doc(&db);
    let execution = seed_execution_for(&db, &product.id, &project.id);

    db.set_project_design_doc(set_design_doc_input(&project.id, "tools/boss/docs/designs/manual.md"))
        .unwrap();

    let item = db
        .surface_design_doc_conflict_on_approve(
            &project.id,
            &execution.id,
            None,
            None,
            "tools/boss/docs/designs/from-task.md",
        )
        .unwrap()
        .expect("conflict should surface an attention item");
    assert_eq!(item.kind, "design_doc_pointer_conflict");
    assert!(
        item.body_markdown.contains("manual.md"),
        "body should name the project's path: {}",
        item.body_markdown,
    );
    assert!(
        item.body_markdown.contains("from-task.md"),
        "body should name the approved path: {}",
        item.body_markdown,
    );

    let items = db.list_attention_items(&execution.id).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, "design_doc_pointer_conflict");

    // Project pointer must not be overwritten by the helper.
    let unchanged = db.get_project(&project.id).unwrap();
    assert_eq!(
        unchanged.design_doc_path.as_deref(),
        Some("tools/boss/docs/designs/manual.md"),
    );

    let _ = std::fs::remove_file(path);
}

// ── Per-task doc-pointer (any work-item kind) ──────────────────────────
// The task-level analogue of the project design-doc pointer tests above.
// These exercise the storage + resolution layer that backs the doc-link
// card affordance. Independent of kind: any work item may carry `doc_*`.
// The gh PR scan that feeds these methods is covered by
// `design_detector`'s `parse_pr_scan_matching` unit tests; here we drive
// the columns directly.

#[test]
fn sync_task_doc_pointer_from_detector_populates_when_null() {
    let db = WorkDb::open(temp_db_path("task-doc-sync-empty")).unwrap();
    let (_product, task) = seed_investigation_for_doc(&db);

    let wrote = db
        .sync_task_doc_pointer_from_detector(
            &task.id,
            Some("git@github.com:spinyfin/mono.git"),
            Some("boss/exec_abc_1"),
            "docs/investigations/foo.md",
        )
        .unwrap();
    assert!(wrote, "expected the detector hook to write the empty pointer");
    assert_eq!(
        db.task_doc_path(&task.id).unwrap().as_deref(),
        Some("docs/investigations/foo.md"),
    );
}

#[test]
fn sync_task_doc_pointer_from_detector_skips_when_set() {
    let db = WorkDb::open(temp_db_path("task-doc-sync-skip")).unwrap();
    let (_product, task) = seed_investigation_for_doc(&db);

    db.set_task_doc_pointer(&task.id, None, Some("main"), Some("docs/investigations/manual.md"))
        .unwrap();
    let wrote = db
        .sync_task_doc_pointer_from_detector(&task.id, None, Some("x"), "docs/investigations/other.md")
        .unwrap();
    assert!(!wrote, "a task that already has a pointer wins — detector no-ops");
    assert_eq!(
        db.task_doc_path(&task.id).unwrap().as_deref(),
        Some("docs/investigations/manual.md"),
    );
}

#[test]
fn sync_task_doc_pointer_validates_path() {
    let db = WorkDb::open(temp_db_path("task-doc-bad-path")).unwrap();
    let (_product, task) = seed_investigation_for_doc(&db);
    let err = db
        .sync_task_doc_pointer_from_detector(&task.id, None, None, "/absolute/path.md")
        .unwrap_err()
        .to_string();
    assert!(err.contains("repo-relative"), "got: {err}");
}

#[test]
fn set_task_doc_pointer_branch_only_keeps_path() {
    // path = None updates only the branch (the merged-after-set path).
    let db = WorkDb::open(temp_db_path("task-doc-branch-only")).unwrap();
    let (_product, task) = seed_investigation_for_doc(&db);
    db.set_task_doc_pointer(
        &task.id,
        None,
        Some("boss/exec_abc_1"),
        Some("docs/investigations/foo.md"),
    )
    .unwrap();
    db.set_task_doc_pointer(&task.id, None, Some("main"), None).unwrap();

    let conn = db.connect().unwrap();
    let mut queries = 0u64;
    let state = resolve_task_doc_pointer(&conn, &task.id, |_| None, &mut queries)
        .unwrap()
        .expect("pointer still set");
    match state {
        ProjectDesignDocState::Resolved { resolved, .. } => {
            assert_eq!(resolved.path, "docs/investigations/foo.md", "path is preserved");
            assert_eq!(resolved.branch, "main", "branch was advanced to main");
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
}

/// Operator CLI path (`boss task set-doc`): set via `set_task_doc`, assert
/// every read path that returns a `Task` surfaces a resolved
/// `doc_link_state` (`set_task_doc` itself, `get_work_item`,
/// `get_work_item_by_short_id`, `get_work_tree`), then `--unset` clears the
/// affordance on all of them. Round-trips the same write path the
/// detector uses (`set_task_doc_pointer`) under a structured input.
#[test]
fn set_task_doc_round_trip_exposes_doc_link_in_work_tree() {
    let db = WorkDb::open(temp_db_path("task-doc-cli-roundtrip")).unwrap();
    let (product, task) = seed_investigation_for_doc(&db);

    let updated = db
        .set_task_doc(SetTaskDocPointerInput {
            task_id: task.id.clone(),
            doc_path: Some("docs/investigations/manual.md".to_owned()),
            doc_branch: Some("boss/exec_manual_1".to_owned()),
            doc_repo_remote_url: None,
            unset: false,
        })
        .unwrap();
    assert_eq!(updated.id, task.id);
    assert_eq!(
        db.task_doc_path(&task.id).unwrap().as_deref(),
        Some("docs/investigations/manual.md"),
    );
    // (a) set_task_doc's own return attaches the just-written pointer.
    assert_doc_link_resolved(
        updated.doc_link_state.as_ref(),
        "docs/investigations/manual.md",
        "boss/exec_manual_1",
        "set_task_doc return",
    );

    // (b) get_work_item (canonical-id / `boss task show <id>` path).
    let by_id = match db.get_work_item(&task.id).unwrap() {
        WorkItem::Task(t) => t,
        other => panic!("expected WorkItem::Task, got {other:?}"),
    };
    assert_doc_link_resolved(
        by_id.doc_link_state.as_ref(),
        "docs/investigations/manual.md",
        "boss/exec_manual_1",
        "get_work_item",
    );

    // (c) get_work_item_by_short_id (`boss task show <short-id>` path).
    let short_id = task.short_id.expect("seeded investigation must have a short_id");
    let by_short = match db
        .get_work_item_by_short_id(&product.id, short_id)
        .unwrap()
        .expect("short_id must resolve")
    {
        WorkItem::Task(t) => t,
        other => panic!("expected WorkItem::Task, got {other:?}"),
    };
    assert_doc_link_resolved(
        by_short.doc_link_state.as_ref(),
        "docs/investigations/manual.md",
        "boss/exec_manual_1",
        "get_work_item_by_short_id",
    );

    let tree = db.get_work_tree(&product.id).unwrap();
    let found = tree
        .tasks
        .iter()
        .find(|t| t.id == task.id)
        .expect("investigation must appear in work tree");
    assert_doc_link_resolved(
        found.doc_link_state.as_ref(),
        "docs/investigations/manual.md",
        "boss/exec_manual_1",
        "get_work_tree",
    );

    // Path validation matches project design docs (Q8).
    let err = db
        .set_task_doc(SetTaskDocPointerInput {
            task_id: task.id.clone(),
            doc_path: Some("/absolute/path.md".to_owned()),
            unset: false,
            ..SetTaskDocPointerInput::default()
        })
        .unwrap_err()
        .to_string();
    assert!(err.contains("repo-relative"), "got: {err}");

    // --unset clears all three columns and hides the affordance on every
    // read path (d).
    let cleared = db
        .set_task_doc(SetTaskDocPointerInput {
            task_id: task.id.clone(),
            unset: true,
            ..SetTaskDocPointerInput::default()
        })
        .unwrap();
    assert_eq!(cleared.id, task.id);
    assert!(
        cleared.doc_link_state.is_none(),
        "set_task_doc --unset return must clear doc_link_state",
    );
    assert!(db.task_doc_path(&task.id).unwrap().is_none());

    let cleared_by_id = match db.get_work_item(&task.id).unwrap() {
        WorkItem::Task(t) => t,
        other => panic!("expected WorkItem::Task, got {other:?}"),
    };
    assert!(
        cleared_by_id.doc_link_state.is_none(),
        "get_work_item after unset must hide doc_link_state",
    );
    let cleared_by_short = match db
        .get_work_item_by_short_id(&product.id, short_id)
        .unwrap()
        .expect("short_id must still resolve")
    {
        WorkItem::Task(t) => t,
        other => panic!("expected WorkItem::Task, got {other:?}"),
    };
    assert!(
        cleared_by_short.doc_link_state.is_none(),
        "get_work_item_by_short_id after unset must hide doc_link_state",
    );

    let tree_cleared = db.get_work_tree(&product.id).unwrap();
    let found_cleared = tree_cleared
        .tasks
        .iter()
        .find(|t| t.id == task.id)
        .expect("investigation still in tree");
    assert!(
        found_cleared.doc_link_state.is_none(),
        "unset must hide the doc-link affordance",
    );
    assert_doc_link_json_null(found_cleared, "get_work_tree after unset");

    // A chore with no pointer still serializes `doc_link_state: null`
    // (field present, not absent) — this is how `boss task show --json`
    // distinguishes "no doc" from "field missing".
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Ordinary chore")
                .build(),
        )
        .unwrap();
    let chore_item = match db.get_work_item(&chore.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected chore WorkItem, got {other:?}"),
    };
    assert!(
        chore_item.doc_link_state.is_none(),
        "chore with no pointer still has None doc_link_state",
    );
    assert_doc_link_json_null(&chore_item, "get_work_item chore with no pointer");

    // Design-with-project and no per-task pointer: project-level
    // design_doc is a separate concept, so Task.doc_link_state stays
    // None until a per-task pointer is written.
    let (_prod2, project) = seed_project_for_design_doc(&db);
    let tree_with_project = db.get_work_tree(&project.product_id).unwrap();
    let design_with_project = tree_with_project
        .tasks
        .iter()
        .find(|t| t.kind == TaskKind::Design && t.project_id.as_deref() == Some(project.id.as_str()))
        .expect("project-seeded design task");
    let design_item = match db.get_work_item(&design_with_project.id).unwrap() {
        WorkItem::Task(t) => t,
        other => panic!("expected WorkItem::Task for design, got {other:?}"),
    };
    assert!(
        design_item.doc_link_state.is_none(),
        "design task WITH a project and no per-task pointer has None doc_link_state",
    );
}

fn assert_doc_link_resolved(state: Option<&ProjectDesignDocState>, path: &str, branch: &str, where_: &str) {
    let Some(state) = state else {
        panic!("{where_} must surface resolved doc_link_state");
    };
    match state {
        ProjectDesignDocState::Resolved { resolved, .. } => {
            assert_eq!(resolved.path, path, "{where_} path");
            assert_eq!(resolved.branch, branch, "{where_} branch");
        }
        other => panic!("{where_}: expected Resolved, got {other:?}"),
    }
}

fn assert_doc_link_json_null(task: &Task, where_: &str) {
    let json = serde_json::to_value(task).expect("task must serialize");
    assert!(
        json.get("doc_link_state").is_some(),
        "{where_}: doc_link_state must be present in JSON; keys {:?}",
        json.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>())
    );
    assert!(
        json["doc_link_state"].is_null(),
        "{where_}: unset doc_link_state must be JSON null, got {}",
        json["doc_link_state"]
    );
}

fn leaf_task(item: WorkItem) -> Task {
    match item {
        WorkItem::Task(t) | WorkItem::Chore(t) => t,
        other => panic!("expected leaf work item, got {other:?}"),
    }
}

/// The detector write path (`sync_task_doc_pointer_from_detector`) plus
/// every read path that returns a Task must surface the pointer for
/// **every** kind — a newly added kind is covered by `TaskKind::ALL`
/// rather than by someone remembering to extend an allowlist.
#[test]
fn detector_populates_per_task_doc_for_every_kind() {
    let db = WorkDb::open(temp_db_path("task-doc-every-kind")).unwrap();
    let product = create_test_product(&db);
    for kind in TaskKind::ALL {
        let task = seed_work_item_of_kind(&db, &product, kind);
        let wrote = db
            .sync_task_doc_pointer_from_detector(&task.id, None, Some("boss/exec_kind"), "docs/investigations/foo.md")
            .unwrap();
        assert!(wrote, "detector must write an empty pointer for kind={kind}");

        let by_id = leaf_task(db.get_work_item(&task.id).unwrap());
        assert_doc_link_resolved(
            by_id.doc_link_state.as_ref(),
            "docs/investigations/foo.md",
            "boss/exec_kind",
            &format!("get_work_item kind={kind}"),
        );

        let tree = db.get_work_tree(&product.id).unwrap();
        if let Some(found) = tree.tasks.iter().chain(tree.chores.iter()).find(|t| t.id == task.id) {
            assert_doc_link_resolved(
                found.doc_link_state.as_ref(),
                "docs/investigations/foo.md",
                "boss/exec_kind",
                &format!("get_work_tree kind={kind}"),
            );
        }
    }
}

/// `set-doc` then `show` (get_work_item) round-trips the pointer for two
/// different kinds — a chore (the reported miss) and an investigation
/// (the historical allowlist). Combined with
/// `detector_populates_per_task_doc_for_every_kind` this is the
/// write/read contract for any kind.
#[test]
fn set_doc_then_show_round_trips_for_chore_and_investigation() {
    let db = WorkDb::open(temp_db_path("task-doc-set-show-kinds")).unwrap();
    let product = create_test_product(&db);
    for kind in [TaskKind::Chore, TaskKind::Investigation] {
        let task = seed_work_item_of_kind(&db, &product, &kind);
        let updated = db
            .set_task_doc(SetTaskDocPointerInput {
                task_id: task.id.clone(),
                doc_path: Some("docs/investigations/manual.md".to_owned()),
                doc_branch: Some("main".to_owned()),
                doc_repo_remote_url: None,
                unset: false,
            })
            .unwrap();
        assert_doc_link_resolved(
            updated.doc_link_state.as_ref(),
            "docs/investigations/manual.md",
            "main",
            &format!("set_task_doc return kind={kind}"),
        );
        let shown = leaf_task(db.get_work_item(&task.id).unwrap());
        assert_doc_link_resolved(
            shown.doc_link_state.as_ref(),
            "docs/investigations/manual.md",
            "main",
            &format!("get_work_item after set-doc kind={kind}"),
        );
        let listed = db.list_tasks(&product.id, None, None, false).unwrap();
        let listed_row = listed
            .iter()
            .find(|t| t.id == task.id)
            .unwrap_or_else(|| panic!("list_tasks must include kind={kind}"));
        assert_doc_link_resolved(
            listed_row.doc_link_state.as_ref(),
            "docs/investigations/manual.md",
            "main",
            &format!("list_tasks kind={kind}"),
        );
        if kind == TaskKind::Chore {
            let chores = db.list_chores(&product.id, None, false).unwrap();
            let chore_row = chores
                .iter()
                .find(|t| t.id == task.id)
                .expect("list_chores must include the chore");
            assert_doc_link_resolved(
                chore_row.doc_link_state.as_ref(),
                "docs/investigations/manual.md",
                "main",
                "list_chores",
            );
        }
    }
}

/// Design-with-a-project still owns the *project-level* design-doc
/// pointer, but a per-task pointer written on that row must also
/// surface — the two concepts stay separate, and neither is gated
/// off the other.
#[test]
fn design_with_project_still_surfaces_per_task_doc_pointer() {
    let db = WorkDb::open(temp_db_path("task-doc-design-with-project")).unwrap();
    let (_product, project) = seed_project_for_design_doc(&db);
    let tree = db.get_work_tree(&project.product_id).unwrap();
    let design = tree
        .tasks
        .iter()
        .find(|t| t.kind == TaskKind::Design && t.project_id.as_deref() == Some(project.id.as_str()))
        .expect("project-seeded design task")
        .clone();
    db.set_task_doc(SetTaskDocPointerInput {
        task_id: design.id.clone(),
        doc_path: Some("docs/designs/also-on-the-task.md".to_owned()),
        doc_branch: Some("main".to_owned()),
        unset: false,
        ..SetTaskDocPointerInput::default()
    })
    .unwrap();
    let shown = leaf_task(db.get_work_item(&design.id).unwrap());
    assert_doc_link_resolved(
        shown.doc_link_state.as_ref(),
        "docs/designs/also-on-the-task.md",
        "main",
        "design-with-project per-task pointer",
    );
}

#[test]
fn resolve_task_doc_pointer_none_when_unset() {
    let db = WorkDb::open(temp_db_path("task-doc-resolve-unset")).unwrap();
    let (_product, task) = seed_investigation_for_doc(&db);
    let conn = db.connect().unwrap();
    let mut queries = 0u64;
    let state = resolve_task_doc_pointer(&conn, &task.id, |_| None, &mut queries).unwrap();
    assert!(state.is_none(), "no pointer -> None so the affordance stays hidden");
}

#[test]
fn resolve_task_doc_pointer_builds_same_product_urls() {
    let db = WorkDb::open(temp_db_path("task-doc-resolve")).unwrap();
    let (_product, task) = seed_investigation_for_doc(&db);
    db.sync_task_doc_pointer_from_detector(&task.id, None, Some("boss/exec_abc_1"), "docs/investigations/foo.md")
        .unwrap();

    let conn = db.connect().unwrap();
    let mut queries = 0u64;
    let state = resolve_task_doc_pointer(&conn, &task.id, |_| None, &mut queries)
        .unwrap()
        .expect("pointer set -> resolved");
    match state {
        ProjectDesignDocState::Resolved {
            resolved,
            web_url,
            raw_content_url,
            workspace_path,
        } => {
            assert_eq!(resolved.path, "docs/investigations/foo.md");
            assert_eq!(resolved.branch, "boss/exec_abc_1");
            // doc_repo None -> inherits the product's repo.
            assert_eq!(resolved.repo_remote_url, "git@github.com:spinyfin/mono.git");
            assert!(
                matches!(resolved.kind, ResolvedDesignDocKind::SameProduct { .. }),
                "the task's own product owns the repo"
            );
            assert_eq!(
                web_url,
                "https://github.com/spinyfin/mono/blob/boss/exec_abc_1/docs/investigations/foo.md"
            );
            // The PR-head branch's `/` must be %2F-encoded in its own path segment.
            assert_eq!(
                raw_content_url.as_deref(),
                Some("https://raw.githubusercontent.com/spinyfin/mono/boss%2Fexec_abc_1/docs/investigations/foo.md")
            );
            assert!(workspace_path.is_none(), "the |_| None lookup yields no workspace");
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
}
