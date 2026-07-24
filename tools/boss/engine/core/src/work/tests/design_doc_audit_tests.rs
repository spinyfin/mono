//! Design-doc pointer tests: project-level detector sync, property-audit
//! row recording, on-approve conflict surfacing, and the task-level
//! doc-pointer analogue for project-less investigations.

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

// ── Per-task doc-pointer (project-less investigations) ──────────────────
// The task-level analogue of the project design-doc pointer tests above.
// These exercise the storage + resolution layer that backs the doc-link
// card affordance for project-less docs-backed items (investigations),
// which have no project pointer to populate. The gh PR scan that feeds
// these methods is covered by `design_detector`'s `parse_pr_scan_matching`
// unit tests; here we drive the columns directly.

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
            // The PR-head branch's `/` must be %2F-encoded in the ?ref= query.
            assert_eq!(
                raw_content_url.as_deref(),
                Some(
                    "https://raw.githubusercontent.com/spinyfin/mono/docs/investigations/foo.md?ref=boss%2Fexec_abc_1"
                )
            );
            assert!(workspace_path.is_none(), "the |_| None lookup yields no workspace");
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
}
