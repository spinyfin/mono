// `resolve_doc_owner` — the reverse `pr_doc:*` artifact → owning-task
// resolver. Design:
// `tools/boss/docs/designs/comment-triggered-document-revisions.md`
// §"The revision-vs-general-task decision".

use super::*;

const MONO_REPO: &str = "git@github.com:spinyfin/mono.git";

/// Find a project's auto-created `kind = 'design'` task.
fn design_task_for(db: &WorkDb, product_id: &str, project_id: &str) -> Task {
    db.list_tasks(product_id, Some(project_id), None, false)
        .unwrap()
        .into_iter()
        .find(|t| t.kind == TaskKind::Design)
        .expect("project should have an auto-created design task")
}

/// Stand up a project-less investigation and an execution attached to it,
/// spawned with the engine's default `BossExecPrefix` branch naming so the
/// caller can compute the exact `expected_branch_name` it would push to.
fn seed_investigation_with_execution(db: &WorkDb) -> (Task, WorkExecution) {
    let (_product, investigation) = seed_investigation_for_doc(db);
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(investigation.id.clone())
                .kind(ExecutionKind::InvestigationImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    (investigation, execution)
}

fn execution_branch(execution: &WorkExecution) -> String {
    crate::completion::expected_branch_name(
        &execution.id,
        &execution.branch_naming,
        execution.worker_branch_prefix.as_deref(),
    )
}

fn pr_doc_artifact_id(repo: &str, branch: &str, path: &str) -> String {
    format!("pr_doc:{repo}:{branch}:{path}")
}

// ── artifact_kind / parsing guards ──────────────────────────────────────────

#[test]
fn resolve_doc_owner_returns_none_for_work_item_artifact_kind() {
    let path = temp_db_path("doc-owner-work-item");
    let db = WorkDb::open(path.clone()).unwrap();

    let resolved = db.resolve_doc_owner("work_item", "task-123").unwrap();
    assert!(resolved.is_none());

    let _ = std::fs::remove_file(path);
}

#[test]
fn resolve_doc_owner_returns_none_for_unparseable_artifact_id() {
    let path = temp_db_path("doc-owner-unparseable");
    let db = WorkDb::open(path.clone()).unwrap();

    assert!(db.resolve_doc_owner("pr_doc", "not-a-pr-doc-id").unwrap().is_none());
    assert!(db.resolve_doc_owner("pr_doc", "pr_doc:only-repo").unwrap().is_none());

    let _ = std::fs::remove_file(path);
}

#[test]
fn resolve_doc_owner_returns_none_when_nothing_matches() {
    let path = temp_db_path("doc-owner-no-match");
    let db = WorkDb::open(path.clone()).unwrap();

    let artifact_id = pr_doc_artifact_id(MONO_REPO, "main", "tools/boss/docs/designs/nowhere.md");
    assert!(db.resolve_doc_owner("pr_doc", &artifact_id).unwrap().is_none());

    let _ = std::fs::remove_file(path);
}

// ── execution-branch mapping ────────────────────────────────────────────────

#[test]
fn resolve_doc_owner_matches_execution_branch_for_investigation_with_no_pr() {
    let path = temp_db_path("doc-owner-exec-branch-no-pr");
    let db = WorkDb::open(path.clone()).unwrap();
    let (investigation, execution) = seed_investigation_with_execution(&db);

    let branch = execution_branch(&execution);
    let artifact_id = pr_doc_artifact_id(MONO_REPO, &branch, "docs/investigation.md");

    let owner = db
        .resolve_doc_owner("pr_doc", &artifact_id)
        .unwrap()
        .expect("should resolve");
    assert_eq!(owner.task_id, investigation.id);
    assert_eq!(owner.task_kind, TaskKind::Investigation);
    assert_eq!(owner.chain_root_id, investigation.id);
    assert_eq!(owner.pr_url, None);
    assert_eq!(owner.pr_lifecycle, DocOwnerPrLifecycle::NoPr);

    let _ = std::fs::remove_file(path);
}

#[test]
fn resolve_doc_owner_execution_branch_reads_open_when_pr_open() {
    let path = temp_db_path("doc-owner-exec-branch-open");
    let db = WorkDb::open(path.clone()).unwrap();
    let (investigation, execution) = seed_investigation_with_execution(&db);
    let pr_url = "https://github.com/spinyfin/mono/pull/42".to_owned();
    db.update_work_item(
        &investigation.id,
        WorkItemPatch {
            status: Some("in_review".to_owned()),
            pr_url: Some(pr_url.clone()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let branch = execution_branch(&execution);
    let artifact_id = pr_doc_artifact_id(MONO_REPO, &branch, "docs/investigation.md");

    let owner = db
        .resolve_doc_owner("pr_doc", &artifact_id)
        .unwrap()
        .expect("should resolve");
    assert_eq!(owner.pr_url.as_deref(), Some(pr_url.as_str()));
    assert_eq!(owner.pr_lifecycle, DocOwnerPrLifecycle::Open);

    let _ = std::fs::remove_file(path);
}

#[test]
fn resolve_doc_owner_execution_branch_reads_merged_when_task_done() {
    let path = temp_db_path("doc-owner-exec-branch-merged");
    let db = WorkDb::open(path.clone()).unwrap();
    let (investigation, execution) = seed_investigation_with_execution(&db);
    let pr_url = "https://github.com/spinyfin/mono/pull/42".to_owned();
    db.update_work_item(
        &investigation.id,
        WorkItemPatch {
            status: Some("done".to_owned()),
            pr_url: Some(pr_url),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let branch = execution_branch(&execution);
    let artifact_id = pr_doc_artifact_id(MONO_REPO, &branch, "docs/investigation.md");

    let owner = db
        .resolve_doc_owner("pr_doc", &artifact_id)
        .unwrap()
        .expect("should resolve");
    assert_eq!(owner.pr_lifecycle, DocOwnerPrLifecycle::Merged);

    let _ = std::fs::remove_file(path);
}

#[test]
fn resolve_doc_owner_execution_branch_scope_guards_non_design_investigation_task() {
    let path = temp_db_path("doc-owner-exec-branch-scope-guard");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = db
        .create_product(CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some(MONO_REPO.to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Fix the thing")
                .build(),
        )
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let branch = execution_branch(&execution);
    let artifact_id = pr_doc_artifact_id(MONO_REPO, &branch, "docs/whatever.md");

    // A chore's own PR branch is a real execution-branch match, but the
    // scope guard excludes any task kind outside {Design, Investigation}.
    assert!(db.resolve_doc_owner("pr_doc", &artifact_id).unwrap().is_none());

    let _ = std::fs::remove_file(path);
}

#[test]
fn resolve_doc_owner_execution_branch_returns_none_when_execution_unknown() {
    let path = temp_db_path("doc-owner-exec-branch-unknown");
    let db = WorkDb::open(path.clone()).unwrap();

    let artifact_id = pr_doc_artifact_id(MONO_REPO, "boss/exec_does_not_exist", "docs/investigation.md");
    assert!(db.resolve_doc_owner("pr_doc", &artifact_id).unwrap().is_none());

    let _ = std::fs::remove_file(path);
}

// ── path-triple mapping (post-merge / no execution row) ─────────────────────

#[test]
fn resolve_doc_owner_matches_project_design_doc_pointer() {
    let path = temp_db_path("doc-owner-project-pointer");
    let db = WorkDb::open(path.clone()).unwrap();
    let (product, project) = seed_project_for_design_doc(&db);
    db.set_project_design_doc(SetProjectDesignDocInput {
        project_id: project.id.clone(),
        design_doc_repo_remote_url: Some(MONO_REPO.to_owned()),
        design_doc_branch: Some("main".to_owned()),
        design_doc_path: Some("tools/boss/docs/designs/foo.md".to_owned()),
        unset: false,
    })
    .unwrap();
    let design_task = design_task_for(&db, &product.id, &project.id);

    let artifact_id = pr_doc_artifact_id(MONO_REPO, "main", "tools/boss/docs/designs/foo.md");
    let owner = db
        .resolve_doc_owner("pr_doc", &artifact_id)
        .unwrap()
        .expect("should resolve");
    assert_eq!(owner.task_id, design_task.id);
    assert_eq!(owner.task_kind, TaskKind::Design);
    assert_eq!(owner.chain_root_id, design_task.id);
    assert_eq!(owner.pr_url, None);
    assert_eq!(owner.pr_lifecycle, DocOwnerPrLifecycle::NoPr);

    let _ = std::fs::remove_file(path);
}

#[test]
fn resolve_doc_owner_project_pointer_match_requires_exact_branch() {
    let path = temp_db_path("doc-owner-project-pointer-branch-mismatch");
    let db = WorkDb::open(path.clone()).unwrap();
    let (_product, project) = seed_project_for_design_doc(&db);
    db.set_project_design_doc(SetProjectDesignDocInput {
        project_id: project.id.clone(),
        design_doc_repo_remote_url: Some(MONO_REPO.to_owned()),
        design_doc_branch: Some("main".to_owned()),
        design_doc_path: Some("tools/boss/docs/designs/foo.md".to_owned()),
        unset: false,
    })
    .unwrap();

    let artifact_id = pr_doc_artifact_id(MONO_REPO, "not-main", "tools/boss/docs/designs/foo.md");
    assert!(db.resolve_doc_owner("pr_doc", &artifact_id).unwrap().is_none());

    let _ = std::fs::remove_file(path);
}

#[test]
fn resolve_doc_owner_matches_task_doc_pointer_for_project_less_investigation() {
    let path = temp_db_path("doc-owner-task-pointer");
    let db = WorkDb::open(path.clone()).unwrap();
    let (_product, investigation) = seed_investigation_for_doc(&db);
    db.set_task_doc_pointer(
        &investigation.id,
        Some(MONO_REPO),
        Some("main"),
        Some("tools/boss/docs/investigations/foo.md"),
    )
    .unwrap();

    let artifact_id = pr_doc_artifact_id(MONO_REPO, "main", "tools/boss/docs/investigations/foo.md");
    let owner = db
        .resolve_doc_owner("pr_doc", &artifact_id)
        .unwrap()
        .expect("should resolve");
    assert_eq!(owner.task_id, investigation.id);
    assert_eq!(owner.task_kind, TaskKind::Investigation);
    assert_eq!(owner.chain_root_id, investigation.id);

    let _ = std::fs::remove_file(path);
}
