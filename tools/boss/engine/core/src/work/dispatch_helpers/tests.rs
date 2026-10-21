use super::*;
use crate::test_support::*;

/// Open a fresh per-test in-memory `WorkDb`. Mirrors the
/// `temp_db_path` convention in `work/tests.rs`: each
/// `WorkDb::open(":memory:")` allocates a unique shared-cache db.
fn open_db() -> WorkDb {
    WorkDb::open(PathBuf::from(":memory:")).unwrap()
}

/// Create a product with the given repo default (or none). Returns
/// the product id.
fn product_with_repo(db: &WorkDb, repo: Option<&str>) -> String {
    create_test_product_with_repo(db, "Boss", repo).id
}

/// Raw-insert a task row with full control over `kind` and
/// `deleted_at`, bypassing the create-time repo invariant. Mirrors
/// the legacy-row inserts in `work/tests/t01.rs`. Returns the id.
fn insert_raw_task(conn: &Connection, product_id: &str, kind: &str, deleted_at: Option<&str>) -> String {
    let id = next_id("task");
    let now = now_string();
    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via)
         VALUES (?1, ?2, NULL, ?3, 'Raw', '', 'todo', NULL, NULL, ?4, ?5, ?5, 1, 'medium', 'test')",
        params![id, product_id, kind, deleted_at, now],
    )
    .unwrap();
    id
}

// ── dispatch_retry_at_for_execution ─────────────────────────────────────

/// Fixed "now" the retry-window tests judge `dispatch_not_before`
/// against, so the fixtures below never meet the real clock.
const NOW_SECS: i64 = 1_700_000_000;
/// A `dispatch_not_before` one hour after [`NOW_SECS`].
const FUTURE: &str = "1700003600";
/// A `dispatch_not_before` one hour before [`NOW_SECS`].
const PAST: &str = "1699996400";

/// Minimal `ready` execution with a given `pre_start_failure_count`
/// / `dispatch_not_before`, for exercising
/// `dispatch_retry_at_for_execution`'s branches in isolation.
fn ready_execution(pre_start_failure_count: i64, dispatch_not_before: Option<&str>) -> WorkExecution {
    WorkExecution::builder()
        .id("exec_test")
        .work_item_id("task_test")
        .created_at("2026-01-01T00:00:00Z")
        .kind(ExecutionKind::TaskImplementation)
        .repo_remote_url("https://github.com/test/repo")
        .status(ExecutionStatus::Ready)
        .pre_start_failure_count(pre_start_failure_count)
        .maybe_dispatch_not_before(dispatch_not_before)
        .build()
}

#[test]
fn dispatch_retry_at_none_for_non_ready_status() {
    let mut exec = ready_execution(1, Some(FUTURE));
    exec.status = ExecutionStatus::Running;
    assert_eq!(dispatch_retry_at_for_execution(&exec, NOW_SECS), None);
}

#[test]
fn dispatch_retry_at_none_when_no_prior_failure() {
    let exec = ready_execution(0, Some(FUTURE));
    assert_eq!(dispatch_retry_at_for_execution(&exec, NOW_SECS), None);
}

#[test]
fn dispatch_retry_at_none_when_dispatch_not_before_absent() {
    let exec = ready_execution(1, None);
    assert_eq!(dispatch_retry_at_for_execution(&exec, NOW_SECS), None);
}

#[test]
fn dispatch_retry_at_none_when_dispatch_not_before_unparseable() {
    let exec = ready_execution(1, Some("not-a-number"));
    assert_eq!(dispatch_retry_at_for_execution(&exec, NOW_SECS), None);
}

#[test]
fn dispatch_retry_at_none_when_dispatch_not_before_in_past() {
    let exec = ready_execution(1, Some(PAST));
    assert_eq!(dispatch_retry_at_for_execution(&exec, NOW_SECS), None);
}

#[test]
fn dispatch_retry_at_some_when_dispatch_not_before_in_future() {
    let exec = ready_execution(1, Some(FUTURE));
    assert_eq!(
        dispatch_retry_at_for_execution(&exec, NOW_SECS),
        Some(FUTURE.to_owned())
    );
}

// ── attention_target_from_input ─────────────────────────────────────────

#[test]
fn attention_target_accepts_execution_only() {
    let db = open_db();
    let product = product_with_repo(&db, Some("git@github.com:spinyfin/mono.git"));
    let chore = db
        .create_chore(CreateChoreInput::builder().product_id(product).name("Chore").build())
        .unwrap();
    let exec = db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id).build())
        .unwrap();

    let conn = db.connect().unwrap();
    let input = CreateAttentionItemInput {
        execution_id: Some(exec.id.clone()),
        ..Default::default()
    };
    let (resolved_exec, resolved_work) = attention_target_from_input(&conn, &input).unwrap();
    assert_eq!(resolved_exec.as_deref(), Some(exec.id.as_str()));
    assert_eq!(resolved_work, None);
}

#[test]
fn attention_target_accepts_work_item_only() {
    let db = open_db();
    let product = product_with_repo(&db, None);
    let conn = db.connect().unwrap();
    let work_id = insert_raw_task(&conn, &product, "chore", None);

    let input = CreateAttentionItemInput {
        work_item_id: Some(work_id.clone()),
        ..Default::default()
    };
    let (resolved_exec, resolved_work) = attention_target_from_input(&conn, &input).unwrap();
    assert_eq!(resolved_exec, None);
    assert_eq!(resolved_work.as_deref(), Some(work_id.as_str()));
}

#[test]
fn attention_target_rejects_both_set() {
    let db = open_db();
    let conn = db.connect().unwrap();
    let input = CreateAttentionItemInput {
        execution_id: Some("exec_x".to_owned()),
        work_item_id: Some("task_x".to_owned()),
        ..Default::default()
    };
    let err = attention_target_from_input(&conn, &input).unwrap_err();
    assert!(
        err.to_string()
            .contains("must reference either execution_id or work_item_id, not both"),
        "unexpected error: {err}",
    );
}

#[test]
fn attention_target_rejects_neither_set() {
    let db = open_db();
    let conn = db.connect().unwrap();
    let input = CreateAttentionItemInput::default();
    let err = attention_target_from_input(&conn, &input).unwrap_err();
    assert!(
        err.to_string()
            .contains("must reference either execution_id or work_item_id"),
        "unexpected error: {err}",
    );
}

#[test]
fn attention_target_treats_empty_strings_as_absent() {
    let db = open_db();
    let product = product_with_repo(&db, None);
    let conn = db.connect().unwrap();
    let work_id = insert_raw_task(&conn, &product, "chore", None);

    // An empty-string execution_id is filtered out, so the work_item_id
    // alone governs — no "both set" error.
    let input = CreateAttentionItemInput {
        execution_id: Some(String::new()),
        work_item_id: Some(work_id.clone()),
        ..Default::default()
    };
    let (resolved_exec, resolved_work) = attention_target_from_input(&conn, &input).unwrap();
    assert_eq!(resolved_exec, None);
    assert_eq!(resolved_work.as_deref(), Some(work_id.as_str()));

    // Both empty → treated as neither set.
    let empty = CreateAttentionItemInput {
        execution_id: Some(String::new()),
        work_item_id: Some(String::new()),
        ..Default::default()
    };
    let err = attention_target_from_input(&conn, &empty).unwrap_err();
    assert!(
        err.to_string()
            .contains("must reference either execution_id or work_item_id"),
        "unexpected error: {err}",
    );
}

#[test]
fn attention_target_surfaces_unknown_execution() {
    let db = open_db();
    let conn = db.connect().unwrap();
    let input = CreateAttentionItemInput {
        execution_id: Some("exec_does_not_exist".to_owned()),
        ..Default::default()
    };
    let err = attention_target_from_input(&conn, &input).unwrap_err();
    assert!(
        err.to_string().contains("unknown execution: exec_does_not_exist"),
        "unexpected error: {err}",
    );
}

#[test]
fn attention_target_surfaces_unknown_work_item() {
    let db = open_db();
    let conn = db.connect().unwrap();
    let input = CreateAttentionItemInput {
        work_item_id: Some("task_does_not_exist".to_owned()),
        ..Default::default()
    };
    let err = attention_target_from_input(&conn, &input).unwrap_err();
    assert!(
        err.to_string().contains("unknown task: task_does_not_exist"),
        "unexpected error: {err}",
    );
}

// ── repo_unresolved_kind_label ──────────────────────────────────────────

#[test]
fn kind_label_chore_task_returns_chore() {
    let db = open_db();
    let product = product_with_repo(&db, None);
    let conn = db.connect().unwrap();
    let chore_id = insert_raw_task(&conn, &product, "chore", None);
    assert_eq!(repo_unresolved_kind_label(&conn, &chore_id).unwrap(), "chore");
}

#[test]
fn kind_label_non_chore_task_kinds_return_task() {
    let db = open_db();
    let product = product_with_repo(&db, None);
    let conn = db.connect().unwrap();
    for kind in ["task", "design", "investigation", "project_task", "revision"] {
        let id = insert_raw_task(&conn, &product, kind, None);
        assert_eq!(
            repo_unresolved_kind_label(&conn, &id).unwrap(),
            "task",
            "kind `{kind}` should map to the `task` label",
        );
    }
}

#[test]
fn kind_label_project_and_product_ids() {
    let db = open_db();
    let conn = db.connect().unwrap();
    // Project / product ids classify by prefix and never hit the DB.
    assert_eq!(repo_unresolved_kind_label(&conn, "proj_abc").unwrap(), "project");
    assert_eq!(repo_unresolved_kind_label(&conn, "prod_abc").unwrap(), "product");
}

#[test]
fn kind_label_errors_on_soft_deleted_or_unknown_task() {
    let db = open_db();
    let product = product_with_repo(&db, None);
    let conn = db.connect().unwrap();

    let deleted = insert_raw_task(&conn, &product, "chore", Some("2026-01-01T00:00:00Z"));
    let err = repo_unresolved_kind_label(&conn, &deleted).unwrap_err();
    assert!(
        err.to_string().contains("unknown task"),
        "soft-deleted task should be unknown (got `{err}`)",
    );

    let err = repo_unresolved_kind_label(&conn, "task_missing").unwrap_err();
    assert!(
        err.to_string().contains("unknown task: task_missing"),
        "unexpected error: {err}",
    );
}

// ── repo_unresolved_attention_body ──────────────────────────────────────

#[test]
fn attention_body_pins_exact_message_format() {
    // This is the single-source message the design's R1 mitigation
    // depends on; pin it so the CLI hint and attention surface can't
    // drift apart.
    assert_eq!(
        repo_unresolved_attention_body("task_42", "chore"),
        "work item task_42 has no repo resolution; set one with `boss chore update --repo <url>` or set a product default.",
    );
    assert_eq!(
        repo_unresolved_attention_body("proj_9", "project"),
        "work item proj_9 has no repo resolution; set one with `boss project update --repo <url>` or set a product default.",
    );
}

// ── record_repo_unresolved_attention ────────────────────────────────────

#[test]
fn record_attention_inserts_open_row_then_dedupes() {
    let db = open_db();
    let product = product_with_repo(&db, None);
    let work_id = {
        let conn = db.connect().unwrap();
        let work_id = insert_raw_task(&conn, &product, "chore", None);
        record_repo_unresolved_attention(&conn, &work_id, "chore").unwrap();
        work_id
    };
    let items = db.list_attention_items_for_work_item(&work_id).unwrap();
    assert_eq!(items.len(), 1, "first call inserts one row");
    let item = &items[0];
    assert_eq!(item.kind, "repo_unresolved");
    assert_eq!(item.status, "open");
    assert_eq!(item.execution_id, None);
    assert_eq!(item.work_item_id.as_deref(), Some(work_id.as_str()));
    assert_eq!(item.body_markdown, repo_unresolved_attention_body(&work_id, "chore"),);

    // Second call while one is already open does NOT duplicate.
    {
        let conn = db.connect().unwrap();
        record_repo_unresolved_attention(&conn, &work_id, "chore").unwrap();
    }
    assert_eq!(
        db.list_attention_items_for_work_item(&work_id).unwrap().len(),
        1,
        "idempotent: no duplicate while an open row exists",
    );
}

// ── ensure_dispatch_repo_resolvable ─────────────────────────────────────

#[test]
fn ensure_dispatch_ok_and_writes_no_attention_when_repo_resolves() {
    let db = open_db();
    let product = product_with_repo(&db, Some("git@github.com:spinyfin/mono.git"));
    let chore = db
        .create_chore(CreateChoreInput::builder().product_id(product).name("Chore").build())
        .unwrap();

    {
        let mut conn = db.connect().unwrap();
        ensure_dispatch_repo_resolvable(&mut conn, &chore.id).unwrap();
    }
    assert!(
        db.list_attention_items_for_work_item(&chore.id).unwrap().is_empty(),
        "a resolvable work item must not raise an attention item",
    );
}

#[test]
fn ensure_dispatch_bails_and_writes_one_sticky_attention_when_unresolvable() {
    let db = open_db();
    let product = product_with_repo(&db, None);
    let conn = db.connect().unwrap();
    let chore_id = insert_raw_task(&conn, &product, "chore", None);
    drop(conn);

    let mut conn = db.connect().unwrap();
    let err = ensure_dispatch_repo_resolvable(&mut conn, &chore_id).unwrap_err();
    // Bails with the exact single-source message.
    assert_eq!(err.to_string(), repo_unresolved_attention_body(&chore_id, "chore"),);
    drop(conn);

    // The sticky row was committed despite the bail, and exactly one
    // exists (a second precheck dedupes rather than piling up).
    let items = db.list_attention_items_for_work_item(&chore_id).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, "repo_unresolved");
    assert_eq!(items[0].status, "open");

    {
        let mut conn = db.connect().unwrap();
        let _ = ensure_dispatch_repo_resolvable(&mut conn, &chore_id).unwrap_err();
    }
    assert_eq!(
        db.list_attention_items_for_work_item(&chore_id).unwrap().len(),
        1,
        "repeated prechecks stay sticky-deduped",
    );
}

// ── retired_spawning_attempt ────────────────────────────────────────────

/// Build a minimal `Task` carrying only the `created_via` value the
/// classifier inspects; every other field is a fixed placeholder.
fn task_with_created_via(created_via: &str) -> Task {
    Task::builder()
        .id("task_test")
        .product_id("prod_test")
        .kind(TaskKind::Revision)
        .name("Rev")
        .description("desc")
        .status(TaskStatus::Todo)
        .created_via(created_via)
        .created_at("2026-01-01T00:00:00Z")
        .updated_at("2026-01-01T00:00:00Z")
        .build()
}

#[test]
fn retired_status_none_for_non_engine_spawned() {
    let db = open_db();
    let conn = db.connect().unwrap();
    assert_eq!(
        retired_spawning_attempt(&conn, &task_with_created_via("human"))
            .unwrap()
            .map(|a| a.status),
        None,
    );
}

#[test]
fn retired_status_none_for_empty_attempt_id() {
    let db = open_db();
    let conn = db.connect().unwrap();
    let created_via = CREATED_VIA_MERGE_CONFLICT_PREFIX.to_string();
    assert_eq!(
        retired_spawning_attempt(&conn, &task_with_created_via(&created_via))
            .unwrap()
            .map(|a| a.status),
        None,
    );
}

#[test]
fn retired_status_none_when_attempt_row_missing() {
    let db = open_db();
    let conn = db.connect().unwrap();
    let created_via = format!("{CREATED_VIA_CI_FIX_PREFIX}cir_missing");
    assert_eq!(
        retired_spawning_attempt(&conn, &task_with_created_via(&created_via))
            .unwrap()
            .map(|a| a.status),
        None,
    );
}

#[test]
fn retired_status_none_while_attempt_active_some_when_terminal() {
    let db = open_db();
    let conn = db.connect().unwrap();
    let now = now_string();
    let attempt_id = next_id("crz");
    // A conflict_resolutions row routed via the merge-conflict prefix.
    conn.execute(
        "INSERT INTO conflict_resolutions
             (id, product_id, work_item_id, pr_url, pr_number, head_branch, base_branch, status, created_at)
         VALUES (?1, 'prod_test', 'task_owner', 'https://example/pr/1', 1, 'feat', 'main', 'pending', ?2)",
        params![attempt_id, now],
    )
    .unwrap();
    let created_via = format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}{attempt_id}");
    let task = task_with_created_via(&created_via);

    // Active statuses are filtered out → None.
    assert_eq!(
        retired_spawning_attempt(&conn, &task).unwrap().map(|a| a.status),
        None,
        "pending attempt is still active",
    );
    conn.execute(
        "UPDATE conflict_resolutions SET status = 'running' WHERE id = ?1",
        params![attempt_id],
    )
    .unwrap();
    assert_eq!(
        retired_spawning_attempt(&conn, &task).unwrap().map(|a| a.status),
        None,
        "running attempt is still active",
    );

    // A terminal status surfaces as Some(status).
    conn.execute(
        "UPDATE conflict_resolutions SET status = 'succeeded' WHERE id = ?1",
        params![attempt_id],
    )
    .unwrap();
    assert_eq!(
        retired_spawning_attempt(&conn, &task).unwrap().map(|a| a.status),
        Some("succeeded".to_owned()),
    );
}

// ── reconcile_revision_execution back-fill ──────────────────────────────

/// When a revision execution was created without `pr_url` (e.g. because
/// `reconcile_active_dispatch` ran before the parent chore pushed its PR),
/// `reconcile_revision_execution` must back-fill `pr_url` from the chain
/// root so the next lease attempt passes `--resume-pr` and the workspace
/// lands on the PR head instead of main.
#[test]
fn reconcile_revision_backfills_pr_url_on_existing_execution() {
    let db = open_db();
    let product = product_with_repo(&db, Some("git@github.com:spinyfin/mono.git"));

    // Create the parent chore and give it an open PR.
    let chore_id = insert_raw_task(&db.connect().unwrap(), &product, "chore", None);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/spinyfin/mono/pull/55' WHERE id = ?1",
            params![chore_id],
        )
        .unwrap();

    // Create the revision task with parent_task_id pointing to the chore.
    let revision_id = next_id("task");
    let now = now_string();
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO tasks (id, product_id, kind, name, description, status, \
             created_at, updated_at, parent_task_id, autostart, priority, created_via) \
             VALUES (?1, ?2, 'revision', 'Rev', '', 'active', ?3, ?3, ?4, 1, 'medium', 'test')",
            params![revision_id, product, now, chore_id],
        )
        .unwrap();

    // Create a revision execution WITHOUT pr_url — simulates the race where
    // reconcile_active_dispatch ran before the parent chore had a PR URL.
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision_id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    assert!(exec.pr_url.is_none(), "execution must start without pr_url");

    // Fetch the revision task row and run reconcile.
    let revision_task = {
        let conn = db.connect().unwrap();
        query_task(&conn, &revision_id)
            .unwrap()
            .expect("revision task must exist")
    };
    let mut result = ExecutionReconcileResult::default();
    {
        let mut conn2 = db.connect().unwrap();
        let tx = conn2
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let mut pending = PendingEvents::new();
        reconcile_revision_execution(&mut pending, &tx, &mut result, &revision_task).unwrap();
        tx.commit().unwrap();
    }

    // After reconcile, the execution must have pr_url set from the chain root.
    let conn = db.connect().unwrap();
    let updated = query_execution(&conn, &exec.id)
        .unwrap()
        .expect("execution must still exist");
    assert_eq!(
        updated.pr_url.as_deref(),
        Some("https://github.com/spinyfin/mono/pull/55"),
        "reconcile_revision_execution must back-fill pr_url on an existing execution \
         that was created without it"
    );
}

// ── dispatch-time catch-up gate: moot revision archived + tombstoned ────

/// When a revision "slips through" (e.g. engine restart, or the revision
/// was created after the parent PR had already merged) and the chain
/// root is found `done`/`archived` at dispatch time, a moot revision
/// (merge-conflict / CI-fix) must be archived silently — mirroring
/// `block_pending_revisions_on_parent_close` — and, like that path, the
/// archive must also tombstone the row so it disappears from
/// `get_work_tree` (the kanban's data source), not just flip status.
#[test]
fn reconcile_revision_execution_archives_and_tombstones_moot_revision_at_dispatch_time_catch_up() {
    let db = open_db();
    let product = product_with_repo(&db, Some("git@github.com:spinyfin/mono.git"));

    // Parent chore already merged before this reconcile tick observed it.
    let chore_id = insert_raw_task(&db.connect().unwrap(), &product, "chore", None);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'done', pr_url = 'https://github.com/spinyfin/mono/pull/56' WHERE id = ?1",
            params![chore_id],
        )
        .unwrap();

    // A CI-fix revision that slipped through, still sitting in `todo`.
    let revision_id = next_id("task");
    let now = now_string();
    let created_via = format!("{CREATED_VIA_CI_FIX_PREFIX}rem_slipped");
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO tasks (id, product_id, kind, name, description, status, \
             created_at, updated_at, parent_task_id, autostart, priority, created_via) \
             VALUES (?1, ?2, 'revision', 'Rev', '', 'todo', ?3, ?3, ?4, 1, 'medium', ?5)",
            params![revision_id, product, now, chore_id, created_via],
        )
        .unwrap();

    let revision_task = {
        let conn = db.connect().unwrap();
        query_task(&conn, &revision_id)
            .unwrap()
            .expect("revision task must exist")
    };
    let mut result = ExecutionReconcileResult::default();
    {
        let mut conn2 = db.connect().unwrap();
        let tx = conn2
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let mut pending = PendingEvents::new();
        reconcile_revision_execution(&mut pending, &tx, &mut result, &revision_task).unwrap();
        tx.commit().unwrap();
    }

    let conn = db.connect().unwrap();
    let (status, deleted_at, archived_by, archived_at, archived_reason, actor): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    ) = conn
        .query_row(
            "SELECT status, deleted_at, archived_by, archived_at, archived_reason, last_status_actor
             FROM tasks WHERE id = ?1",
            params![revision_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .unwrap();
    assert_eq!(
        status, "archived",
        "moot revision slipped past the merge poller must be archived at dispatch-time catch-up"
    );
    assert!(
        deleted_at.is_some(),
        "dispatch-time catch-up archive must also tombstone the row, matching \
         block_pending_revisions_on_parent_close"
    );
    assert_eq!(
        archived_by.as_deref(),
        Some(ARCHIVE_MECHANISM_PARENT_CLOSE_DISPATCH_RECONCILIATION)
    );
    assert!(archived_at.as_deref().is_some_and(|at| !at.is_empty()));
    assert!(archived_reason.as_deref().is_some_and(|reason| !reason.is_empty()));
    assert_eq!(actor, "engine");
}

/// A revision that slipped through with `merge_queue_state = 'queued'`
/// still set (e.g. it briefly entered GitHub's merge queue before the
/// parent chore's own merge made it moot) must not leave that state
/// behind when archived at dispatch-time catch-up — otherwise it
/// permanently inflates `list_queued_merge_queue_members`'s membership
/// set and every live card's renumbered position (mono#58-shown-for-4).
#[test]
fn reconcile_revision_execution_clears_merge_queue_state_on_moot_archive() {
    let db = open_db();
    let product = product_with_repo(&db, Some("git@github.com:spinyfin/mono.git"));

    let chore_id = insert_raw_task(&db.connect().unwrap(), &product, "chore", None);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'done', pr_url = 'https://github.com/spinyfin/mono/pull/56' WHERE id = ?1",
            params![chore_id],
        )
        .unwrap();

    let revision_id = next_id("task");
    let now = now_string();
    let created_via = format!("{CREATED_VIA_CI_FIX_PREFIX}rem_queued");
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO tasks (id, product_id, kind, name, description, status, \
             created_at, updated_at, parent_task_id, autostart, priority, created_via, \
             merge_queue_state, merge_queue_detail) \
             VALUES (?1, ?2, 'revision', 'Rev', '', 'todo', ?3, ?3, ?4, 1, 'medium', ?5, \
             'queued', '{\"position\":1,\"state\":\"QUEUED\"}')",
            params![revision_id, product, now, chore_id, created_via],
        )
        .unwrap();

    let revision_task = {
        let conn = db.connect().unwrap();
        query_task(&conn, &revision_id)
            .unwrap()
            .expect("revision task must exist")
    };
    let mut result = ExecutionReconcileResult::default();
    {
        let mut conn2 = db.connect().unwrap();
        let tx = conn2
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let mut pending = PendingEvents::new();
        reconcile_revision_execution(&mut pending, &tx, &mut result, &revision_task).unwrap();
        tx.commit().unwrap();
    }

    let conn = db.connect().unwrap();
    let (merge_queue_state, merge_queue_detail): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT merge_queue_state, merge_queue_detail FROM tasks WHERE id = ?1",
            params![revision_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(
        merge_queue_state.is_none(),
        "moot-revision archive at dispatch-time catch-up must clear merge_queue_state"
    );
    assert!(
        merge_queue_detail.is_none(),
        "moot-revision archive at dispatch-time catch-up must clear merge_queue_detail"
    );
}

/// A `pr_review:` revision that slips past the merge-poller sweep and is
/// resolved by the dispatch-time catch-up gate instead must produce the
/// same `followup` (with origin provenance and the rewritten description)
/// that `block_pending_revisions_on_parent_close` produces — see the
/// `t09.rs` followup suite for the sweep-path equivalents.
///
/// Before both paths shared `resolve_revision_on_parent_close`, this gate
/// carried its own copy of the moot-vs-convert branch that predated the
/// followup kind, so a revision reaching merge via this path silently
/// degraded to a plain `chore` with no origin back-reference — the same
/// work item classified differently purely by which path observed the
/// merge first.
#[test]
fn reconcile_revision_execution_converts_pr_review_revision_to_followup_at_dispatch_time_catch_up() {
    let db = open_db();
    let product = product_with_repo(&db, Some("git@github.com:spinyfin/mono.git"));

    // Parent chore already merged before this reconcile tick observed it.
    // `short_id` is what the followup must carry as `origin_task_short_id`.
    let chore_id = insert_raw_task(&db.connect().unwrap(), &product, "chore", None);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'done', short_id = 42, \
             pr_url = 'https://github.com/spinyfin/mono/pull/1537' WHERE id = ?1",
            params![chore_id],
        )
        .unwrap();

    // A PR-review revision that slipped through, still sitting in `todo`.
    let revision_id = next_id("task");
    let now = now_string();
    let created_via = format!("{CREATED_VIA_PR_REVIEW_PREFIX}exec_slipped");
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO tasks (id, product_id, kind, name, description, status, \
             created_at, updated_at, parent_task_id, autostart, priority, created_via) \
             VALUES (?1, ?2, 'revision', 'Rev', \
             'Address ALL findings before finalising this revision.', 'todo', ?3, ?3, ?4, 1, 'medium', ?5)",
            params![revision_id, product, now, chore_id, created_via],
        )
        .unwrap();

    let revision_task = {
        let conn = db.connect().unwrap();
        query_task(&conn, &revision_id)
            .unwrap()
            .expect("revision task must exist")
    };
    let mut result = ExecutionReconcileResult::default();
    {
        let mut conn2 = db.connect().unwrap();
        let tx = conn2
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let mut pending = PendingEvents::new();
        reconcile_revision_execution(&mut pending, &tx, &mut result, &revision_task).unwrap();
        tx.commit().unwrap();
    }

    let chores = db.list_chores(&product, None, false).unwrap();
    let followup = chores
        .iter()
        .find(|c| c.id != chore_id && c.kind == TaskKind::Followup)
        .unwrap_or_else(|| {
            panic!("dispatch-time catch-up must convert a pr_review revision to a followup; chores: {chores:?}")
        });
    assert!(
        !chores.iter().any(|c| c.id != chore_id && c.kind == TaskKind::Chore),
        "no plain chore may be minted alongside the followup; chores: {chores:?}",
    );
    assert_eq!(
        followup.origin_task_short_id,
        Some(42),
        "followup must carry the chain root's short_id as origin_task_short_id",
    );
    assert_eq!(
        followup.origin_pr_number,
        Some(1537),
        "followup must carry the PR number parsed from the chain root's pr_url",
    );
    assert!(
        followup.description.contains("closing this follow-up"),
        "followup description must be rewritten off the revision wording; got {:?}",
        followup.description,
    );

    // Conversion preserves the one work-item identity materialised by the
    // review execution; the revision row itself is now the followup.
    let conn = db.connect().unwrap();
    let (kind, status, deleted_at, archived_reason): (String, String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT kind, status, deleted_at, archived_reason FROM tasks WHERE id = ?1",
            params![revision_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(followup.id, revision_id);
    assert_eq!(kind, "followup");
    assert_eq!(status, "todo");
    assert!(deleted_at.is_none());
    assert!(archived_reason.is_none());
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE created_via = ?1",
            params![created_via],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "both parent-close paths must preserve one findings row");
}

// ── request_pr_review ───────────────────────────────────────────────────

fn chore_with_pr(db: &WorkDb, pr_url: &str, status: &str) -> String {
    let product = product_with_repo(db, Some("https://github.com/test/repo"));
    let chore = db
        .create_chore(CreateChoreInput::builder().product_id(product).name("chore").build())
        .unwrap();
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some(status.to_owned()),
            pr_url: Some(pr_url.to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    chore.id
}

#[test]
fn request_pr_review_creates_fresh_ready_execution() {
    let db = open_db();
    let work_item_id = chore_with_pr(&db, "https://github.com/test/repo/pull/1", "active");
    let checker = FakePrStateChecker::always(PrOpenState::Open);

    let execution = db.request_pr_review(&work_item_id, &checker).unwrap();

    assert_eq!(execution.kind, ExecutionKind::PrReview);
    assert_eq!(execution.status, ExecutionStatus::Ready);
    assert_eq!(execution.work_item_id, work_item_id);
}

/// Regression test: a cycle root in `in_review` stays `in_review` when a
/// new review pass is enqueued via `request_pr_review_in_tx`, per
/// `tools/boss/docs/designs/work-kanban.md`'s cycle-root status
/// contract.
#[test]
fn request_pr_review_keeps_in_review_task_in_review() {
    let db = open_db();
    let work_item_id = chore_with_pr(&db, "https://github.com/test/repo/pull/8", "in_review");
    let checker = FakePrStateChecker::always(PrOpenState::Open);

    let execution = db.request_pr_review(&work_item_id, &checker).unwrap();

    assert_eq!(execution.kind, ExecutionKind::PrReview);
    let task = query_task(&db.connect().unwrap(), &work_item_id).unwrap().unwrap();
    assert_eq!(
        task.status,
        TaskStatus::InReview,
        "enqueuing a review pass must NEVER move an in_review row out of Review"
    );
}

#[test]
fn request_pr_review_is_idempotent_on_existing_ready_pr_review() {
    let db = open_db();
    let work_item_id = chore_with_pr(&db, "https://github.com/test/repo/pull/2", "active");
    let checker = FakePrStateChecker::always(PrOpenState::Open);

    let first = db.request_pr_review(&work_item_id, &checker).unwrap();
    let second = db.request_pr_review(&work_item_id, &checker).unwrap();

    assert_eq!(
        first.id, second.id,
        "a second trigger while a pr_review is already queued must reuse the same execution"
    );
}

// ── create_pr_review_execution_dedup (concurrent-enqueue regression) ────
//
// Reproduces the incident these tests guard against: two independent
// PR-completion triggers (the Stop-hook path and the merge-poller's
// `pr_recheck` sweep) each observed the producing execution as
// not-yet-terminal and independently enqueued a `pr_review` execution
// for the same unchanged head sha, minting two near-identical findings
// revisions from a single push.

#[test]
fn create_pr_review_execution_dedup_creates_fresh_ready_execution() {
    let db = open_db();
    let work_item_id = chore_with_pr(&db, "https://github.com/test/repo/pull/100", "active");

    let (execution, is_new) = db
        .create_pr_review_execution_dedup(&work_item_id, "https://github.com/test/repo")
        .unwrap();

    assert!(is_new, "first call must create a fresh execution");
    assert_eq!(execution.kind, ExecutionKind::PrReview);
    assert_eq!(execution.status, ExecutionStatus::Ready);
    assert_eq!(execution.work_item_id, work_item_id);
}

#[test]
fn create_pr_review_execution_dedup_reuses_existing_nonterminal_pr_review() {
    // Two triggers for the SAME unchanged head, back to back — the exact
    // shape of the concurrent-enqueue race (Stop hook + pr_recheck sweep
    // both racing past the enqueue check before either recorded its
    // completion).
    let db = open_db();
    let work_item_id = chore_with_pr(&db, "https://github.com/test/repo/pull/101", "active");

    let (first, first_is_new) = db
        .create_pr_review_execution_dedup(&work_item_id, "https://github.com/test/repo")
        .unwrap();
    let (second, second_is_new) = db
        .create_pr_review_execution_dedup(&work_item_id, "https://github.com/test/repo")
        .unwrap();

    assert!(first_is_new, "first call must create a fresh execution");
    assert!(
        !second_is_new,
        "second call for the same unchanged head must reuse, not duplicate"
    );
    assert_eq!(
        first.id, second.id,
        "both calls must resolve to the SAME pr_review execution row"
    );

    let reviews: Vec<_> = db
        .list_ready_executions()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == ExecutionKind::PrReview && e.work_item_id == work_item_id)
        .collect();
    assert_eq!(
        reviews.len(),
        1,
        "exactly one pr_review execution must exist for the work item after two triggers; got {reviews:?}",
    );
}

#[test]
fn request_pr_review_refuses_when_no_pr_url() {
    let db = open_db();
    let product = product_with_repo(&db, Some("https://github.com/test/repo"));
    let chore = db
        .create_chore(CreateChoreInput::builder().product_id(product).name("chore").build())
        .unwrap();
    let checker = FakePrStateChecker::always(PrOpenState::Open);

    let err = db.request_pr_review(&chore.id, &checker).unwrap_err();
    assert!(err.to_string().contains("no open PR"), "unexpected error: {err}");
}

#[test]
fn request_pr_review_refuses_when_task_terminal() {
    let db = open_db();
    let work_item_id = chore_with_pr(&db, "https://github.com/test/repo/pull/3", "done");
    let checker = FakePrStateChecker::always(PrOpenState::Open);

    let err = db.request_pr_review(&work_item_id, &checker).unwrap_err();
    assert!(err.to_string().contains("done"), "unexpected error: {err}");
}

#[test]
fn request_pr_review_refuses_when_pr_merged() {
    let db = open_db();
    let work_item_id = chore_with_pr(&db, "https://github.com/test/repo/pull/4", "active");
    let checker = FakePrStateChecker::always(PrOpenState::Merged);

    let err = db.request_pr_review(&work_item_id, &checker).unwrap_err();
    assert!(err.to_string().contains("already merged"), "unexpected error: {err}");
}

#[test]
fn request_pr_review_refuses_when_pr_closed_unmerged() {
    let db = open_db();
    let work_item_id = chore_with_pr(&db, "https://github.com/test/repo/pull/5", "active");
    let checker = FakePrStateChecker::always(PrOpenState::ClosedUnmerged);

    let err = db.request_pr_review(&work_item_id, &checker).unwrap_err();
    assert!(err.to_string().contains("closed"), "unexpected error: {err}");
}

#[test]
fn request_pr_review_refuses_when_execution_already_live() {
    let db = open_db();
    let work_item_id = chore_with_pr(&db, "https://github.com/test/repo/pull/6", "active");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "INSERT INTO work_executions (id, work_item_id, kind, status, repo_remote_url, created_at)
             VALUES ('exec_live_test', ?1, 'chore_implementation', 'running', 'https://github.com/test/repo', ?2)",
            params![work_item_id, now_string()],
        )
        .unwrap();
    }
    let checker = FakePrStateChecker::always(PrOpenState::Open);

    let err = db.request_pr_review(&work_item_id, &checker).unwrap_err();
    assert!(err.to_string().contains("already live"), "unexpected error: {err}");
}

#[test]
fn request_pr_review_accepts_friendly_id() {
    let db = open_db();
    let product = product_with_repo(&db, Some("https://github.com/test/repo"));
    let chore = db
        .create_chore(CreateChoreInput::builder().product_id(product).name("chore").build())
        .unwrap();
    let friendly_id = boss_protocol::short_id_label(chore.short_id).expect("chore has a short id");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            pr_url: Some("https://github.com/test/repo/pull/7".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let checker = FakePrStateChecker::always(PrOpenState::Open);

    let execution = db.request_pr_review(&friendly_id, &checker).unwrap();
    assert_eq!(execution.work_item_id, chore.id);
}
