use super::*;

// ── block_pending_revisions_on_parent_close / parent-PR-merged invalidation ─

/// When the parent PR merges, a human-filed `todo` revision must be converted
/// to a standalone backlog chore (autostart=false) and the revision itself
/// archived.  The operator's ask is preserved in the new chore's description.
#[test]
fn mark_chore_pr_merged_converts_todo_revision_to_chore() {
    let db = WorkDb::open(temp_db_path("rev-convert-todo")).unwrap();
    let product_id = make_revision_product(&db, "conv-todo");
    let pr_url = "https://github.com/spinyfin/mono/pull/805";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&parent_id), &checker).unwrap();
    assert_eq!(revision.status, TaskStatus::Todo);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    // The revision must now be archived (and tombstoned — see below), so
    // fetch it by raw SQL rather than `get_work_item`, which deliberately
    // excludes tombstoned rows (same contract as any other soft-deleted task).
    let conn = db.connect().unwrap();
    let rev_after = query_task(&conn, &revision.id)
        .unwrap()
        .expect("revision row must still exist");
    drop(conn);
    assert_eq!(
        rev_after.status,
        TaskStatus::Archived,
        "todo revision must be archived after parent PR merges",
    );
    assert_eq!(
        rev_after.blocked_reason, None,
        "archived revision must have no blocked_reason"
    );
    assert!(
        rev_after.deleted_at.is_some(),
        "archived revision must be tombstoned so it disappears from get_work_tree"
    );

    // A new standalone chore must carry the revision's name/description.
    let chores = db.list_chores(&product_id, None, false).unwrap();
    let new_chore = chores.iter().find(|c| c.id != parent_id && c.name == revision.name);
    assert!(
        new_chore.is_some(),
        "a new standalone chore must be created from the revision; chores: {chores:?}",
    );
    let new_chore = new_chore.unwrap();
    assert_eq!(
        new_chore.description, revision.description,
        "converted chore must carry the revision's description",
    );
    assert!(
        !new_chore.autostart,
        "backlog-converted revision must yield a non-autostart chore"
    );
    assert_eq!(new_chore.status, TaskStatus::Todo, "converted chore must start as todo");
}

/// A revision archived because its parent PR merged (the non-moot,
/// convert-to-chore path) must not leave a stale `merge_queue_state` behind —
/// otherwise the archived-but-`queued` row permanently inflates
/// `list_queued_merge_queue_members`'s membership set and every live card's
/// renumbered position (mono#58-shown-for-4).
#[test]
fn mark_chore_pr_merged_clears_merge_queue_state_on_converted_revision() {
    let db = WorkDb::open(temp_db_path("rev-convert-clears-queue")).unwrap();
    let product_id = make_revision_product(&db, "conv-todo-queue");
    let pr_url = "https://github.com/spinyfin/mono/pull/807";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&parent_id), &checker).unwrap();

    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET merge_queue_state = 'queued', merge_queue_detail = '{\"position\":1}' WHERE id = ?1",
        rusqlite::params![revision.id],
    )
    .unwrap();
    drop(conn);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let conn = db.connect().unwrap();
    let (state, detail): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT merge_queue_state, merge_queue_detail FROM tasks WHERE id = ?1",
            [&revision.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(
        state.is_none(),
        "revision converted to standalone chore must have merge_queue_state cleared"
    );
    assert!(
        detail.is_none(),
        "revision converted to standalone chore must have merge_queue_detail cleared"
    );
}

/// A revision already in `in_review` must be flipped to `done` (not
/// `blocked`) — it delivered its commit before the parent merged.
#[test]
fn mark_chore_pr_merged_keeps_in_review_revision_done_not_blocked() {
    let db = WorkDb::open(temp_db_path("rev-invalidate-in-review")).unwrap();
    let product_id = make_revision_product(&db, "inv-ir");
    let pr_url = "https://github.com/spinyfin/mono/pull/806";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&parent_id), &checker).unwrap();

    // Simulate the revision having pushed its commit and moved to in_review.
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
        rusqlite::params![revision.id],
    )
    .unwrap();
    drop(conn);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let rev_after = match db.get_work_item(&revision.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("unexpected variant: {other:?}"),
    };
    assert_eq!(
        rev_after.status,
        TaskStatus::Done,
        "in_review revision must become done (not blocked) when parent PR merges"
    );
}

/// A revision already `done` must not be touched.
#[test]
fn mark_chore_pr_merged_does_not_re_block_done_revision() {
    let db = WorkDb::open(temp_db_path("rev-invalidate-done")).unwrap();
    let product_id = make_revision_product(&db, "inv-done");
    let pr_url = "https://github.com/spinyfin/mono/pull/807";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&parent_id), &checker).unwrap();

    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'done' WHERE id = ?1",
        rusqlite::params![revision.id],
    )
    .unwrap();
    drop(conn);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let rev_after = match db.get_work_item(&revision.id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("unexpected variant: {other:?}"),
    };
    assert_eq!(
        rev_after.status,
        TaskStatus::Done,
        "already-done revision must not be touched"
    );
    assert_eq!(
        rev_after.blocked_reason, None,
        "done revision must not acquire a blocked_reason"
    );
}

/// A merge-conflict-resolution revision is moot by construction — the PR
/// cannot merge while the conflict stands.  When the parent PR merges the
/// revision must be archived silently (no follow-up chore, no attention item).
#[test]
fn mark_chore_pr_merged_retires_moot_merge_conflict_revision() {
    let db = WorkDb::open(temp_db_path("rev-moot-merge-conflict")).unwrap();
    let product_id = make_revision_product(&db, "moot-crz");
    let pr_url = "https://github.com/spinyfin/mono/pull/820";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    // Create a conflict-resolution revision the same way `conflict_watch` does.
    let crz_id = "crz_fake_for_moot_test";
    let rev_id = insert_conflict_revision_row(&db, &product_id, &parent_id, crz_id);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    assert_eq!(
        task_status(&db, &rev_id),
        "archived",
        "moot merge-conflict revision must be archived when parent PR merges",
    );

    // No standalone chore must be spawned for a moot revision.
    let chores = db.list_chores(&product_id, None, false).unwrap();
    assert!(
        !chores.iter().any(|c| c.id != parent_id),
        "no new chore must be created for a moot revision; chores: {chores:?}",
    );

    // Regression (moot-revision kanban visibility): archiving must also
    // tombstone the row — `get_work_tree` (the kanban's data source) only
    // filters on `deleted_at`, not `status`, so an archived-but-live row
    // would otherwise keep rendering (misclassified into Backlog, since
    // `WorkTask.boardColumn` has no case for `archived`).
    let conn = db.connect().unwrap();
    let deleted_at: Option<String> = conn
        .query_row("SELECT deleted_at FROM tasks WHERE id = ?1", [&rev_id], |r| r.get(0))
        .unwrap();
    assert!(deleted_at.is_some(), "archived moot revision must be tombstoned");
    drop(conn);
    let tree = db.get_work_tree(&product_id).unwrap();
    assert!(
        !tree.tasks.iter().any(|t| t.id == rev_id),
        "archived moot revision must not appear in get_work_tree output (kanban board data)",
    );
}

/// A moot revision archived silently on parent-PR-merge must not leave a
/// stale `merge_queue_state` behind — mirrors
/// `mark_chore_pr_merged_clears_merge_queue_state_on_converted_revision` but
/// for the moot (silent-archive) branch instead of the convert-to-chore one.
#[test]
fn mark_chore_pr_merged_clears_merge_queue_state_on_moot_revision() {
    let db = WorkDb::open(temp_db_path("rev-moot-clears-queue")).unwrap();
    let product_id = make_revision_product(&db, "moot-crz-queue");
    let pr_url = "https://github.com/spinyfin/mono/pull/821";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let crz_id = "crz_fake_for_moot_queue_test";
    let rev_id = insert_conflict_revision_row(&db, &product_id, &parent_id, crz_id);

    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET merge_queue_state = 'queued', merge_queue_detail = '{\"position\":1}' WHERE id = ?1",
        rusqlite::params![rev_id],
    )
    .unwrap();
    drop(conn);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    let conn = db.connect().unwrap();
    let (state, detail): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT merge_queue_state, merge_queue_detail FROM tasks WHERE id = ?1",
            [&rev_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(state.is_none(), "moot revision archive must clear merge_queue_state");
    assert!(detail.is_none(), "moot revision archive must clear merge_queue_detail");
}

/// A CI-fix revision is moot by construction — the PR cannot merge while CI
/// is failing.  Archive it silently without creating a follow-up chore.
#[test]
fn mark_chore_pr_merged_retires_moot_ci_fix_revision() {
    let db = WorkDb::open(temp_db_path("rev-moot-ci-fix")).unwrap();
    let product_id = make_revision_product(&db, "moot-ci");
    let pr_url = "https://github.com/spinyfin/mono/pull/821";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let rem_id = "rem_fake_for_moot_test";
    let rev_id = insert_ci_fix_revision_row(&db, &product_id, &parent_id, rem_id);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    assert_eq!(
        task_status(&db, &rev_id),
        "archived",
        "moot CI-fix revision must be archived when parent PR merges",
    );

    let chores = db.list_chores(&product_id, None, false).unwrap();
    assert!(
        !chores.iter().any(|c| c.id != parent_id),
        "no new chore must be created for a moot CI-fix revision; chores: {chores:?}",
    );

    // Regression (operator report: T2111 — "Fix failing CI" on an
    // already-merged PR lingering on the kanban): the archived CI-fix
    // revision must vanish from get_work_tree, not just flip status.
    let tree = db.get_work_tree(&product_id).unwrap();
    assert!(
        !tree.tasks.iter().any(|t| t.id == rev_id),
        "archived moot CI-fix revision must not appear in get_work_tree output",
    );
}

/// An `active` (WIP) human-filed revision must be converted to a standalone
/// chore with `autostart = true` so the work is immediately redispatched on a
/// fresh PR.  The revision itself is archived.
#[test]
fn mark_chore_pr_merged_converts_active_revision_to_autostart_chore() {
    let db = WorkDb::open(temp_db_path("rev-convert-active")).unwrap();
    let product_id = make_revision_product(&db, "conv-active");
    let pr_url = "https://github.com/spinyfin/mono/pull/822";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&parent_id), &checker).unwrap();

    // Simulate the revision being dispatched (worker running).
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'active' WHERE id = ?1",
        rusqlite::params![revision.id],
    )
    .unwrap();
    drop(conn);

    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();

    assert_eq!(
        task_status(&db, &revision.id),
        "archived",
        "active (WIP) revision must be archived after parent PR merges",
    );

    let chores = db.list_chores(&product_id, None, false).unwrap();
    let new_chore = chores.iter().find(|c| c.id != parent_id && c.name == revision.name);
    assert!(
        new_chore.is_some(),
        "a new standalone chore must be created for a WIP revision; chores: {chores:?}",
    );
    let new_chore = new_chore.unwrap();
    assert_eq!(
        new_chore.description, revision.description,
        "converted chore must carry the revision's description",
    );
    assert!(
        new_chore.autostart,
        "WIP-converted revision must yield an autostart chore (restart immediately)",
    );
    assert_eq!(new_chore.status, TaskStatus::Todo, "converted chore must start as todo");

    // The archived original revision must vanish from the board; only the
    // new standalone chore should render.
    let tree = db.get_work_tree(&product_id).unwrap();
    assert!(
        !tree.tasks.iter().any(|t| t.id == revision.id),
        "archived WIP revision must not appear in get_work_tree output",
    );
}

/// `list_active_revision_executions_for_chain` returns executions with a
/// cube lease but not ones that are already terminal.
#[test]
fn list_active_revision_executions_for_chain_returns_leased_only() {
    let db = WorkDb::open(temp_db_path("rev-list-active-exec")).unwrap();
    let product_id = make_revision_product(&db, "lare");
    let pr_url = "https://github.com/spinyfin/mono/pull/808";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&parent_id), &checker).unwrap();

    // Insert a running execution WITH a cube lease.
    // Note: work_executions.priority is an i64 (0 = default); tasks.priority is TEXT.
    let exec_id = next_id("exec");
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO work_executions
                 (id, work_item_id, kind, status, repo_remote_url, cube_lease_id,
                  cube_workspace_id, workspace_path, priority, prefer_is_soft,
                  created_at, started_at)
             VALUES (?1, ?2, 'revision_implementation', 'running',
                     'git@github.com:spinyfin/mono.git', 'lease-abc',
                     'mono-agent-001', '/tmp/ws', 0, 0,
                     '2026-01-01T00:00:00Z', '2026-01-01T00:01:00Z')",
        rusqlite::params![exec_id, revision.id],
    )
    .unwrap();

    // Insert a terminal execution (cancelled) — must NOT be returned.
    let exec_terminal = next_id("exec");
    conn.execute(
        "INSERT INTO work_executions
                 (id, work_item_id, kind, status, repo_remote_url, cube_lease_id,
                  cube_workspace_id, workspace_path, priority, prefer_is_soft,
                  created_at, finished_at)
             VALUES (?1, ?2, 'revision_implementation', 'cancelled',
                     'git@github.com:spinyfin/mono.git', 'lease-old',
                     'mono-agent-001', '/tmp/ws', 0, 0,
                     '2026-01-01T00:00:00Z', '2026-01-01T00:05:00Z')",
        rusqlite::params![exec_terminal, revision.id],
    )
    .unwrap();
    drop(conn);

    let active = db.list_active_revision_executions_for_chain(&parent_id).unwrap();
    assert_eq!(active.len(), 1, "only the running leased execution must be returned");
    assert_eq!(active[0].id, exec_id);
}

/// `list_active_revision_executions_for_chain` returns empty for a chain
/// root with no revisions.
#[test]
fn list_active_revision_executions_for_chain_empty_for_no_revisions() {
    let db = WorkDb::open(temp_db_path("rev-list-exec-empty")).unwrap();
    let product_id = make_revision_product(&db, "lare-empty");
    let pr_url = "https://github.com/spinyfin/mono/pull/809";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let active = db.list_active_revision_executions_for_chain(&parent_id).unwrap();
    assert!(active.is_empty(), "chain with no revisions must yield empty vec");
}

/// Regression: `mark_chore_pr_merged` archives (and now tombstones — see
/// `block_pending_revisions_on_parent_close`) a WIP revision in the same
/// transaction that flips the chain root to `done`. The merge poller's
/// `stop_active_revision_executions` runs immediately afterward, in a
/// separate step, to force-release the WIP revision's cube lease —
/// `list_active_revision_executions_for_chain` must still find that
/// execution even though its task row is now tombstoned, or the lease
/// leaks forever.
#[test]
fn list_active_revision_executions_for_chain_finds_execution_after_revision_archived_and_tombstoned() {
    let db = WorkDb::open(temp_db_path("rev-list-exec-after-archive")).unwrap();
    let product_id = make_revision_product(&db, "lare-archived");
    let pr_url = "https://github.com/spinyfin/mono/pull/810";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&parent_id), &checker).unwrap();

    // Simulate the revision being dispatched (worker running with a leased
    // cube workspace), mirroring `mark_chore_pr_merged_converts_active_revision_to_autostart_chore`.
    let exec_id = next_id("exec");
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'active' WHERE id = ?1",
        rusqlite::params![revision.id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO work_executions
                 (id, work_item_id, kind, status, repo_remote_url, cube_lease_id,
                  cube_workspace_id, workspace_path, priority, prefer_is_soft,
                  created_at, started_at)
             VALUES (?1, ?2, 'revision_implementation', 'running',
                     'git@github.com:spinyfin/mono.git', 'lease-wip',
                     'mono-agent-001', '/tmp/ws', 0, 0,
                     '2026-01-01T00:00:00Z', '2026-01-01T00:01:00Z')",
        rusqlite::params![exec_id, revision.id],
    )
    .unwrap();
    drop(conn);

    // Parent PR merges — archives (and tombstones) the WIP revision.
    db.mark_chore_pr_merged(&parent_id, pr_url).unwrap();
    assert_eq!(task_status(&db, &revision.id), "archived");
    let conn = db.connect().unwrap();
    let deleted_at: Option<String> = conn
        .query_row("SELECT deleted_at FROM tasks WHERE id = ?1", [&revision.id], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(deleted_at.is_some(), "archived revision must be tombstoned");
    drop(conn);

    // The merge poller's follow-up step must still find the WIP execution
    // to force-release its lease, even though the task row is now
    // tombstoned.
    let active = db.list_active_revision_executions_for_chain(&parent_id).unwrap();
    assert_eq!(
        active.len(),
        1,
        "the WIP revision's live execution must still be found for lease release after archival"
    );
    assert_eq!(active[0].id, exec_id);
}

// ── Revision dispatch via request_execution ───────────────────────────

/// Regression: T701-style bug where `request_execution` (used by the
/// orphan sweep and kanban drag) produced `task_implementation` for
/// `kind=revision` tasks instead of `revision_implementation`.
///
/// After the fix:
///   - `execution.kind` must be `"revision_implementation"`
///   - `execution.pr_url` must be set to the chain-root's PR URL
///
/// The orphan sweep re-dispatch path then produces the same shape as the
/// steady-state `reconcile_revision_execution` path so the worker prompt
/// gets the correct revision prelude and cannot open a new PR.
#[test]
fn request_execution_for_revision_task_produces_revision_implementation_kind() {
    let db = WorkDb::open(temp_db_path("req-exec-revision-kind")).unwrap();
    let product_id = make_revision_product(&db, "req-exec-kind");
    let pr_url = "https://github.com/spinyfin/mono/pull/818";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);

    // Insert a revision task manually (direct insert, as in chain_root tests).
    let revision_id = insert_revision_row(&db, &product_id, &parent_id);

    let exec = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(revision_id.clone())
                .build(),
            |_| false,
        )
        .unwrap();

    assert_eq!(
        exec.kind,
        ExecutionKind::RevisionImplementation,
        "request_execution must produce revision_implementation for kind=revision tasks, got {:?}",
        exec.kind,
    );
    assert_eq!(
        exec.pr_url.as_deref(),
        Some(pr_url),
        "revision execution must carry the chain-root's PR URL so the worker knows which branch to push to",
    );
    assert_eq!(exec.status, ExecutionStatus::Ready);
}

/// Regression: re-dispatch of a revision task (orphan-sweep path) must
/// still produce `revision_implementation` kind and the correct `pr_url`.
///
/// Scenario: the first execution was `revision_implementation` and is now
/// `abandoned` (simulating a worker crash).  A subsequent call to
/// `request_execution` creates a new `ready` execution.
#[test]
fn request_execution_redispatch_of_revision_preserves_revision_kind_and_pr_url() {
    let db = WorkDb::open(temp_db_path("req-exec-revision-redispatch")).unwrap();
    let product_id = make_revision_product(&db, "req-exec-redispatch");
    let pr_url = "https://github.com/spinyfin/mono/pull/818";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);
    let revision_id = insert_revision_row(&db, &product_id, &parent_id);

    // First dispatch.
    let first_exec = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(revision_id.clone())
                .build(),
            |_| false,
        )
        .unwrap();
    assert_eq!(first_exec.kind, ExecutionKind::RevisionImplementation);
    assert_eq!(first_exec.pr_url.as_deref(), Some(pr_url));

    // Simulate worker crash: mark the execution as abandoned.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'abandoned' WHERE id = ?1",
            rusqlite::params![first_exec.id],
        )
        .unwrap();

    // Re-dispatch (mimics orphan sweep calling request_execution_with_live_check
    // with is_live returning false for the abandoned execution).
    let second_exec = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(revision_id.clone())
                .build(),
            |_| false,
        )
        .unwrap();

    assert_ne!(
        second_exec.id, first_exec.id,
        "re-dispatch must create a fresh execution row",
    );
    assert_eq!(
        second_exec.kind,
        ExecutionKind::RevisionImplementation,
        "re-dispatched revision must still be revision_implementation, got {:?}",
        second_exec.kind,
    );
    assert_eq!(
        second_exec.pr_url.as_deref(),
        Some(pr_url),
        "re-dispatched revision must carry the chain-root's PR URL",
    );
    assert_eq!(second_exec.status, ExecutionStatus::Ready);
}

// ── Conflict-resolution revision: stop re-dispatch once the attempt retires ──

/// Insert a `kind=revision` task linked to a merge-conflict attempt.
/// Mirrors what `conflict_watch::maybe_spawn_conflict_revision` produces:
/// `created_via = "merge-conflict:<crz_id>"`, parent = the chore, and (as
/// in the steady-state loop) the row already flipped to `active`.
fn insert_conflict_revision_row(db: &WorkDb, product_id: &str, parent_task_id: &str, crz_id: &str) -> String {
    let conn = db.connect().unwrap();
    let id = next_id("task");
    let now = now_string();
    let created_via = format!("{CREATED_VIA_MERGE_CONFLICT_PREFIX}{crz_id}");
    conn.execute(
        "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, autostart, created_via, parent_task_id)
         VALUES (?1, ?2, 'revision', 'Resolve merge conflict against main', '', 'active', ?3, ?3, 0, ?4, ?5)",
        rusqlite::params![id, product_id, now, created_via, parent_task_id],
    )
    .unwrap();
    id
}

/// Regression (T906 / PR #970): a merge-conflict revision must stop being
/// re-dispatched once its `conflict_resolutions` attempt has retired
/// (`succeeded`), even though the chore PR is still open + `in_review`.
///
/// Before the fix, `reconcile_revision_execution` only consulted the chain
/// root's `pr_url`/`status` — neither of which reflects a *resolved*
/// conflict on an open PR — so it minted a fresh `revision_implementation`
/// execution on every reconcile tick. A queued `ready` row would then be
/// picked up and `start_execution_run` would flip the revision from
/// `in_review` straight back to `active`, defeating any operator attempt to
/// move the card to Review. The attempt could accumulate 8+ executions.
///
/// After the fix: a retired attempt drops the queued execution and settles
/// the revision to `in_review`; no new execution is created.
#[test]
fn merge_conflict_revision_stops_dispatch_after_attempt_succeeds() {
    let db = WorkDb::open(temp_db_path("crz-revision-stop-dispatch")).unwrap();
    let product_id = make_revision_product(&db, "crz-stop");
    let pr_url = "https://github.com/spinyfin/mono/pull/970";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let crz = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore_id.clone(),
            pr_url: pr_url.to_owned(),
            pr_number: 970,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-1".into()),
            head_sha_before: Some("head-1".into()),
        })
        .unwrap()
        .unwrap();
    let revision_id = insert_conflict_revision_row(&db, &product_id, &chore_id, &crz.id);
    db.set_conflict_resolution_revision_task_id(&crz.id, &revision_id)
        .unwrap();

    // First reconcile: attempt is still `pending`, so the revision dispatches
    // normally — this is the behaviour the fix must preserve.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_first = executions_for(&db, &revision_id);
    assert_eq!(
        after_first.len(),
        1,
        "a pending attempt must still dispatch the revision once: {after_first:?}",
    );
    assert_eq!(after_first[0].1, "ready");

    // Conflict resolves: the PR is now CLEAN and the attempt retires. The
    // `ready` execution from above is still queued (the exact race that
    // makes a manual move-to-Review pointless).
    db.mark_conflict_resolution_succeeded(&crz.id, None).unwrap();

    // Second reconcile: the fix must NOT mint another execution.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_second = executions_for(&db, &revision_id);
    assert_eq!(
        after_second.len(),
        1,
        "no new execution may be created once the attempt has retired: {after_second:?}",
    );
    assert_eq!(
        after_second[0].1, "abandoned",
        "the leftover queued execution must be abandoned so start_execution_run \
         can't flip the revision back to active",
    );
    assert_eq!(
        task_status(&db, &revision_id),
        "in_review",
        "the revision must be settled to in_review once its fix vehicle is spent",
    );

    // Third reconcile: idempotent — the in_review revision is no longer
    // dispatchable, so nothing changes and the loop stays broken.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_third = executions_for(&db, &revision_id);
    assert_eq!(
        after_third.len(),
        1,
        "reconcile must remain a no-op for a settled revision: {after_third:?}",
    );
    assert_eq!(task_status(&db, &revision_id), "in_review");
}

/// Guard against over-blocking: while the attempt is still active
/// (`pending`/`running`), a revision whose previous execution died must
/// still be re-dispatched. The fix only short-circuits *retired* attempts.
#[test]
fn merge_conflict_revision_still_redispatches_while_attempt_active() {
    let db = WorkDb::open(temp_db_path("crz-revision-active-redispatch")).unwrap();
    let product_id = make_revision_product(&db, "crz-active");
    let pr_url = "https://github.com/spinyfin/mono/pull/971";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let crz = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore_id.clone(),
            pr_url: pr_url.to_owned(),
            pr_number: 971,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-1".into()),
            head_sha_before: Some("head-1".into()),
        })
        .unwrap()
        .unwrap();
    let revision_id = insert_conflict_revision_row(&db, &product_id, &chore_id, &crz.id);
    db.set_conflict_resolution_revision_task_id(&crz.id, &revision_id)
        .unwrap();

    // First dispatch, then simulate a dead worker (execution orphaned).
    db.reconcile_product_executions(&product_id).unwrap();
    let first = executions_for(&db, &revision_id);
    assert_eq!(first.len(), 1);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'orphaned' WHERE id = ?1",
            rusqlite::params![first[0].0],
        )
        .unwrap();

    // Attempt is still pending → reconcile must re-dispatch a fresh execution.
    db.reconcile_product_executions(&product_id).unwrap();
    let second = executions_for(&db, &revision_id);
    assert_eq!(
        second.len(),
        2,
        "an active attempt must still re-dispatch after a worker dies: {second:?}",
    );
    assert!(
        second.iter().any(|(_, status)| status == "ready"),
        "a fresh ready execution must exist while the attempt is active: {second:?}",
    );
    assert_eq!(
        task_status(&db, &revision_id),
        "active",
        "the revision must stay dispatchable while its attempt is active",
    );
}

// ── CI-fix revision: stop re-dispatch once the attempt retires ──
//
// Symmetric sibling of the merge-conflict arm above. `reconcile_revision_execution`
// (via `retired_spawning_attempt_status`) keys on the task's `created_via` prefix:
// `merge-conflict:<crz_id>` → `conflict_resolutions`, `ci-fix:<id>` → `ci_remediations`.
// The merge-conflict arm is exercised by the two tests above; these mirror them for
// the `ci-fix` arm so a regression in *that* branch (a CI-fix revision minting a fresh
// `revision_implementation` execution on every reconcile tick after its
// `ci_remediations` attempt retired) can't slip through silently.

/// Insert a `pending` CI remediation attempt linked to `chore_id`, and a
/// `ci-fix:<id>` revision task parented to it. Returns `(rem_id, revision_id)`.
fn setup_ci_fix_revision(
    db: &WorkDb,
    product_id: &str,
    chore_id: &str,
    pr_url: &str,
    pr_number: i64,
) -> (String, String) {
    let rem = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product_id.to_owned(),
            work_item_id: chore_id.to_owned(),
            pr_url: pr_url.to_owned(),
            pr_number,
            head_branch: "feature".into(),
            head_sha_at_trigger: "head-1".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .unwrap();
    let revision_id = insert_ci_fix_revision_row(db, product_id, chore_id, &rem.id);
    db.set_ci_remediation_revision_task_id(&rem.id, &revision_id).unwrap();
    (rem.id, revision_id)
}

/// Regression sibling of `merge_conflict_revision_stops_dispatch_after_attempt_succeeds`
/// for the `ci-fix` arm: a CI-fix revision must stop being re-dispatched once its
/// `ci_remediations` attempt has retired (`succeeded`), even though the chore PR is
/// still open + `in_review`. After the fix: a retired attempt drops the queued
/// execution and settles the revision to `in_review`; no new execution is created.
#[test]
fn ci_fix_revision_stops_dispatch_after_attempt_succeeds() {
    let db = WorkDb::open(temp_db_path("ci-fix-revision-stop-succeeds")).unwrap();
    let product_id = make_revision_product(&db, "ci-fix-stop-ok");
    let pr_url = "https://github.com/spinyfin/mono/pull/980";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let (rem_id, revision_id) = setup_ci_fix_revision(&db, &product_id, &chore_id, pr_url, 980);

    // First reconcile: attempt is still `pending`, so the revision dispatches
    // normally — the behaviour the fix must preserve.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_first = executions_for(&db, &revision_id);
    assert_eq!(
        after_first.len(),
        1,
        "a pending attempt must still dispatch the revision once: {after_first:?}",
    );
    assert_eq!(after_first[0].1, "ready");

    // CI fix lands and the attempt retires. The `ready` execution from above is
    // still queued (the exact race that makes a manual move-to-Review pointless).
    db.mark_ci_remediation_succeeded(&rem_id, None).unwrap();

    // Second reconcile: must NOT mint another execution.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_second = executions_for(&db, &revision_id);
    assert_eq!(
        after_second.len(),
        1,
        "no new execution may be created once the attempt has retired: {after_second:?}",
    );
    assert_eq!(
        after_second[0].1, "abandoned",
        "the leftover queued execution must be abandoned so start_execution_run \
         can't flip the revision back to active",
    );
    assert_eq!(
        task_status(&db, &revision_id),
        "in_review",
        "the revision must be settled to in_review once its fix vehicle is spent",
    );

    // Third reconcile: idempotent — the in_review revision is no longer dispatchable.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_third = executions_for(&db, &revision_id);
    assert_eq!(
        after_third.len(),
        1,
        "reconcile must remain a no-op for a settled revision: {after_third:?}",
    );
    assert_eq!(task_status(&db, &revision_id), "in_review");
}

/// Second retired-status case for the `ci-fix` arm: a `failed` attempt is just
/// as terminal as `succeeded`, so it must also stop re-dispatch and settle the
/// revision to `in_review`. (A CI fix that exhausts/aborts must not keep minting
/// executions either.)
#[test]
fn ci_fix_revision_stops_dispatch_after_attempt_fails() {
    let db = WorkDb::open(temp_db_path("ci-fix-revision-stop-fails")).unwrap();
    let product_id = make_revision_product(&db, "ci-fix-stop-fail");
    let pr_url = "https://github.com/spinyfin/mono/pull/981";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let (rem_id, revision_id) = setup_ci_fix_revision(&db, &product_id, &chore_id, pr_url, 981);

    // First reconcile: pending attempt → dispatch once.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_first = executions_for(&db, &revision_id);
    assert_eq!(
        after_first.len(),
        1,
        "a pending attempt must still dispatch the revision once: {after_first:?}",
    );
    assert_eq!(after_first[0].1, "ready");

    // The CI fix attempt fails (retires terminally).
    db.mark_ci_remediation_failed(&rem_id, "ran out of attempts").unwrap();

    // Second reconcile: no new execution; queued row abandoned; revision settled.
    db.reconcile_product_executions(&product_id).unwrap();
    let after_second = executions_for(&db, &revision_id);
    assert_eq!(
        after_second.len(),
        1,
        "no new execution may be created once the attempt has retired: {after_second:?}",
    );
    assert_eq!(
        after_second[0].1, "abandoned",
        "the leftover queued execution must be abandoned once the failed attempt retires",
    );
    assert_eq!(
        task_status(&db, &revision_id),
        "in_review",
        "the revision must be settled to in_review once its (failed) fix vehicle is spent",
    );
}

/// Guard against over-blocking on the `ci-fix` arm (sibling of
/// `merge_conflict_revision_still_redispatches_while_attempt_active`): while the
/// `ci_remediations` attempt is still active (`pending`/`running`), a revision
/// whose previous execution died must still be re-dispatched. The fix only
/// short-circuits *retired* attempts.
#[test]
fn ci_fix_revision_still_redispatches_while_attempt_active() {
    let db = WorkDb::open(temp_db_path("ci-fix-revision-active-redispatch")).unwrap();
    let product_id = make_revision_product(&db, "ci-fix-active");
    let pr_url = "https://github.com/spinyfin/mono/pull/982";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let (_rem_id, revision_id) = setup_ci_fix_revision(&db, &product_id, &chore_id, pr_url, 982);

    // First dispatch, then simulate a dead worker (execution orphaned).
    db.reconcile_product_executions(&product_id).unwrap();
    let first = executions_for(&db, &revision_id);
    assert_eq!(first.len(), 1);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'orphaned' WHERE id = ?1",
            rusqlite::params![first[0].0],
        )
        .unwrap();

    // Attempt is still pending → reconcile must re-dispatch a fresh execution.
    db.reconcile_product_executions(&product_id).unwrap();
    let second = executions_for(&db, &revision_id);
    assert_eq!(
        second.len(),
        2,
        "an active attempt must still re-dispatch after a worker dies: {second:?}",
    );
    assert!(
        second.iter().any(|(_, status)| status == "ready"),
        "a fresh ready execution must exist while the attempt is active: {second:?}",
    );
    assert_eq!(
        task_status(&db, &revision_id),
        "active",
        "the revision must stay dispatchable while its attempt is active",
    );
}

// ── Revision completion must leave the base row in Review, never Doing ──
//
// Contract: a base task/chore that has a revision underway must REMAIN in
// `in_review` (the Review column) the whole time — while the revision is in
// flight AND after it completes. A revision is an amendment to the base
// row's already-open PR; nothing in the revision lifecycle may transition
// the base out of Review into `active` (Doing). Only an explicit
// human/merge action advances a row out of Review.
//
// The stranding vector is `start_execution_run`: its kanban auto-advance
// historically flipped any row that was not `done`/`archived`/`blocked`
// to `active`, so a stray `ready` execution that landed on the base (a
// re-dispatch race around the revision's PR push) would yank the base
// from Review into Doing the moment it started. `reconcile_revision_execution`
// band-aided this for engine-spawned *revision* rows; the base row had no
// equivalent guard, and the base kind (chore vs project_task) made no
// difference — both share the same dispatch machinery. The fix closes the
// hole at the source so both kinds are covered by one rule.

/// Create a `project_task` in `in_review` with a bound PR, mirroring
/// `make_in_review_chore` but for a project-member task. The project's
/// auto-design seed task is skipped so the project_task is itself the
/// chain root the revision is filed against.
fn make_in_review_project_task(db: &WorkDb, product_id: &str, pr_url: &str) -> String {
    let project = db
        .create_project(CreateProjectInput {
            product_id: product_id.to_owned(),
            name: "Project for revision tests".to_owned(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
        })
        .unwrap();
    let task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product_id.to_owned())
                .project_id(project.id.clone())
                .name("Project task for revision tests")
                .autostart(false)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![task.id, pr_url],
        )
        .unwrap();
    task.id
}

/// Shared body: with a revision filed against `base_id` (PR open, base in
/// Review), a fresh `ready` execution that lands on the base — the
/// re-dispatch race a completing revision can leave behind — must NOT
/// demote the base into `active`/Doing when it starts.
fn assert_started_execution_keeps_base_in_review(db: &WorkDb, base_id: &str) {
    // File a revision against the base so the base genuinely "has a
    // revision underway" — the precondition for the contract.
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    db.create_revision(revision_input(base_id), &checker).unwrap();
    assert_eq!(task_status(db, base_id), "in_review", "precondition");

    // Simulate the stray re-dispatch: a `ready` execution bound to the
    // base, then a worker claiming it.
    let exec = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(base_id.to_owned())
                .build(),
            |_| false,
        )
        .unwrap();
    assert_eq!(exec.status, ExecutionStatus::Ready);
    db.start_execution_run(
        &exec.id,
        "worker-1",
        "mono",
        "lease-1",
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )
    .unwrap();

    assert_eq!(
        task_status(db, base_id),
        "in_review",
        "starting an execution against an in_review base must NOT demote it \
         out of Review into Doing — the revision rides the base's open PR",
    );
}

/// Base is a `chore`.
#[test]
fn revision_completion_keeps_base_chore_in_review() {
    let db = WorkDb::open(temp_db_path("rev-base-chore-in-review")).unwrap();
    let product_id = make_revision_product(&db, "base-chore-review");
    let pr_url = "https://github.com/spinyfin/mono/pull/533";
    let base_id = make_in_review_chore(&db, &product_id, pr_url);
    assert_started_execution_keeps_base_in_review(&db, &base_id);
}

/// Base is a `project_task`. Same machinery, same contract — the kind must
/// not change the outcome (the regression this guards against was a fix
/// applied to only one kind / only the revision row).
#[test]
fn revision_completion_keeps_base_project_task_in_review() {
    let db = WorkDb::open(temp_db_path("rev-base-pt-in-review")).unwrap();
    let product_id = make_revision_product(&db, "base-pt-review");
    let pr_url = "https://github.com/spinyfin/mono/pull/534";
    let base_id = make_in_review_project_task(&db, &product_id, pr_url);
    assert_started_execution_keeps_base_in_review(&db, &base_id);
}

// ── Transcript-path recording for conflict-resolution revision executions ──

/// Regression guard for the T1291 incident: a conflict-resolution revision
/// execution that IS dispatched (while the `conflict_resolutions` attempt is
/// still active) must record `transcript_path` in `work_runs` when the
/// worker fires a hook with the path.
///
/// The failure mode: `set_run_transcript_path_if_unset` receives
/// `RowMissing` when there is no `work_runs` row for the execution (the
/// execution was abandoned before dispatch). This test verifies the HAPPY
/// path — when the attempt is active and the scheduler calls
/// `start_execution_run`, the `work_runs` row IS created, and the transcript
/// path CAN be recorded. A separate test covers the abandoned-before-dispatch
/// path (no `work_runs` row → `has_run_row_for_execution` returns false).
#[test]
fn conflict_resolution_revision_execution_records_transcript_path() {
    let db = WorkDb::open(temp_db_path("crz-transcript-path")).unwrap();
    let product_id = make_revision_product(&db, "crz-transcript");
    let pr_url = "https://github.com/spinyfin/mono/pull/1291";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    // Insert a `conflict_resolutions` attempt (still `pending` / active).
    let crz = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore_id.clone(),
            pr_url: pr_url.to_owned(),
            pr_number: 1291,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-sha-1".into()),
            head_sha_before: Some("head-sha-1".into()),
        })
        .unwrap()
        .unwrap();

    // Create the revision task as `conflict_watch::maybe_spawn_conflict_revision`
    // would (created_via = "merge-conflict:<crz_id>").
    let revision_id = insert_conflict_revision_row(&db, &product_id, &chore_id, &crz.id);
    db.set_conflict_resolution_revision_task_id(&crz.id, &revision_id)
        .unwrap();

    // Reconcile: attempt is still active → a `revision_implementation`
    // execution is created with status = `ready`.
    db.reconcile_product_executions(&product_id).unwrap();
    let execs = executions_for(&db, &revision_id);
    assert_eq!(execs.len(), 1, "one execution should be created");
    assert_eq!(execs[0].1, "ready");
    let exec_id = &execs[0].0;

    // Precondition: no work_runs row yet.
    assert!(
        !db.has_run_row_for_execution(exec_id).unwrap(),
        "precondition: no work_runs row before start_execution_run",
    );

    // Simulate the scheduler dispatching the execution.
    let (_, run) = db
        .start_execution_run(
            exec_id,
            "worker-conflict-1",
            "mono",
            "lease-crz-1",
            "mono-agent-064",
            "/tmp/mono-agent-064",
        )
        .unwrap();

    // Now a work_runs row exists.
    assert!(
        db.has_run_row_for_execution(exec_id).unwrap(),
        "work_runs row must exist after start_execution_run",
    );
    assert!(
        db.transcript_path_for_execution(exec_id).unwrap().is_none(),
        "transcript_path must be NULL at run start",
    );

    // Simulate the worker's first hook event reporting its transcript path.
    let transcript_path = "/tmp/mono-agent-064/.boss/session.jsonl";
    let outcome = db.set_run_transcript_path_if_unset(exec_id, transcript_path).unwrap();
    assert!(
        matches!(outcome, SetRunTranscriptPathOutcome::Updated),
        "set_run_transcript_path_if_unset must return Updated for a new run; got {outcome:?}",
    );

    // Confirm the path is readable via the execution-id namespace.
    let recorded = db.transcript_path_for_execution(exec_id).unwrap();
    assert_eq!(
        recorded.as_deref(),
        Some(transcript_path),
        "transcript_path must be retrievable via transcript_path_for_execution",
    );

    // The run row's id must match the run we started.
    let _ = run; // run.id is the work_runs id; path was keyed on execution_id, both must agree.
}

/// Companion test: a conflict-resolution revision execution that was abandoned
/// BEFORE the scheduler dispatched it has no `work_runs` row.
/// `has_run_row_for_execution` must return `false` so the `TailRunTranscript`
/// handler can surface a clear "never dispatched" message instead of the
/// generic "no transcript path recorded" error.
#[test]
fn abandoned_conflict_resolution_revision_execution_has_no_run_row() {
    let db = WorkDb::open(temp_db_path("crz-abandoned-no-run")).unwrap();
    let product_id = make_revision_product(&db, "crz-abandoned");
    let pr_url = "https://github.com/spinyfin/mono/pull/1292";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let crz = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore_id.clone(),
            pr_url: pr_url.to_owned(),
            pr_number: 1292,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-sha-2".into()),
            head_sha_before: Some("head-sha-2".into()),
        })
        .unwrap()
        .unwrap();
    let revision_id = insert_conflict_revision_row(&db, &product_id, &chore_id, &crz.id);
    db.set_conflict_resolution_revision_task_id(&crz.id, &revision_id)
        .unwrap();

    // First reconcile: attempt is active → execution created with status=ready.
    db.reconcile_product_executions(&product_id).unwrap();
    let execs = executions_for(&db, &revision_id);
    assert_eq!(execs.len(), 1);
    let exec_id = &execs[0].0.clone();
    assert_eq!(execs[0].1, "ready");

    // Conflict resolves before the scheduler picks up the execution.
    db.mark_conflict_resolution_succeeded(&crz.id, None).unwrap();

    // Second reconcile: execution is abandoned (no worker ran).
    db.reconcile_product_executions(&product_id).unwrap();
    let execs = executions_for(&db, &revision_id);
    assert_eq!(execs.len(), 1);
    assert_eq!(execs[0].1, "abandoned", "execution must be abandoned");

    // No work_runs row — the scheduler never called start_execution_run.
    assert!(
        !db.has_run_row_for_execution(exec_id).unwrap(),
        "abandoned execution must have no work_runs row; the TailRunTranscript handler \
         must surface NeverDispatched rather than KnownNoTranscript",
    );

    // transcript_path_for_execution must return None (consistent with current behaviour).
    assert!(
        db.transcript_path_for_execution(exec_id).unwrap().is_none(),
        "no transcript path must be recorded for an execution that was never dispatched",
    );
}

// ── Per-PR single-writer guard: live_execution_elsewhere_in_chain ──────────

/// A revision must see the chain root's live execution: the root (the task
/// that owns the PR) is a chain member, so a `running` execution on it blocks
/// any sibling revision's dispatch onto the shared jj backing store.
#[test]
fn live_execution_elsewhere_in_chain_finds_live_root_from_revision() {
    let db = WorkDb::open(temp_db_path("chain-live-root-from-rev")).unwrap();
    let product_id = make_revision_product(&db, "live-root");
    let pr_url = "https://github.com/spinyfin/mono/pull/1577";
    let root_id = make_in_review_chore(&db, &product_id, pr_url);
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&root_id), &checker).unwrap();

    // A live (running) resume on the chain root.
    let root_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root_id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();

    let found = db.live_execution_elsewhere_in_chain(&revision.id).unwrap();
    assert_eq!(
        found.map(|e| e.id),
        Some(root_exec.id),
        "a revision must observe the chain root's live implementation execution",
    );
}

/// Symmetrically, the chain root must see a revision's live execution — a
/// conflict-resolution worker rebasing the stack must block the root resume.
#[test]
fn live_execution_elsewhere_in_chain_finds_live_revision_from_root() {
    let db = WorkDb::open(temp_db_path("chain-live-rev-from-root")).unwrap();
    let product_id = make_revision_product(&db, "live-rev");
    let pr_url = "https://github.com/spinyfin/mono/pull/1815";
    let root_id = make_in_review_chore(&db, &product_id, pr_url);
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&root_id), &checker).unwrap();

    let rev_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::WaitingHuman)
                .build(),
        )
        .unwrap();

    let found = db.live_execution_elsewhere_in_chain(&root_id).unwrap();
    assert_eq!(
        found.map(|e| e.id),
        Some(rev_exec.id),
        "the chain root must observe a revision's live execution",
    );
}

/// The work item's OWN execution and TERMINAL executions are ignored — only
/// a live execution on a DIFFERENT chain member counts.
#[test]
fn live_execution_elsewhere_in_chain_ignores_self_and_terminal() {
    let db = WorkDb::open(temp_db_path("chain-live-ignores")).unwrap();
    let product_id = make_revision_product(&db, "ignores");
    let pr_url = "https://github.com/spinyfin/mono/pull/300";
    let root_id = make_in_review_chore(&db, &product_id, pr_url);
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&root_id), &checker).unwrap();

    // Live execution on the ROOT itself — must be excluded when scanning
    // from the root (it's the work item we're asking about).
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(root_id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .build(),
    )
    .unwrap();

    // A TERMINAL execution on the revision — must not count as live.
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(revision.id.clone())
            .kind(ExecutionKind::RevisionImplementation)
            .status(ExecutionStatus::Completed)
            .build(),
    )
    .unwrap();

    assert!(
        db.live_execution_elsewhere_in_chain(&root_id).unwrap().is_none(),
        "scanning from the root must exclude the root's own live exec and the revision's terminal exec",
    );
}

/// A chore with no revisions is a single-member chain, so a live execution on
/// it never registers as an "elsewhere" sibling — the guard is a no-op and
/// preserves the historical per-work-item behaviour for chain-less work.
#[test]
fn live_execution_elsewhere_in_chain_noop_for_choreless_chain() {
    let db = WorkDb::open(temp_db_path("chain-live-choreless")).unwrap();
    let product_id = make_revision_product(&db, "choreless");
    let chore_id = make_chore_root(&db, &product_id, "solo");

    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore_id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .build(),
    )
    .unwrap();

    assert!(
        db.live_execution_elsewhere_in_chain(&chore_id).unwrap().is_none(),
        "a chore with no revisions must report no chain sibling",
    );
}

// ── Reviewer-fallback single-live-worker guard ─────────────────────────────

/// Helper: a chore held in `active` (Doing) with a bound PR url.
fn make_active_chore_with_pr(db: &WorkDb, product_id: &str, pr_url: &str) -> String {
    let id = make_in_review_chore(db, product_id, pr_url);
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'active' WHERE id = ?1",
        rusqlite::params![id],
    )
    .unwrap();
    id
}

/// The reviewer-fallback must NOT advance active→in_review while a live
/// IMPLEMENTATION execution is working the task (T1577 incident): doing so
/// strands the implementation worker in the Review lane with no Doing card.
#[test]
fn reviewer_fallback_refuses_while_impl_exec_live() {
    let db = WorkDb::open(temp_db_path("reviewer-refuse-impl-live")).unwrap();
    let product_id = make_revision_product(&db, "refuse-impl");
    let pr_url = "https://github.com/spinyfin/mono/pull/1577";
    let chore_id = make_active_chore_with_pr(&db, &product_id, pr_url);

    // A stale/timed-out pr_review execution is what surfaces this task as a
    // fallback candidate — but a live implementation resume is also present.
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore_id.clone())
            .kind(ExecutionKind::PrReview)
            .status(ExecutionStatus::Abandoned)
            .build(),
    )
    .unwrap();
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore_id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .build(),
    )
    .unwrap();

    let advanced = db.advance_pending_review_task_to_in_review(&chore_id).unwrap();
    assert!(
        !advanced,
        "must refuse to advance while a live implementation execution is working the task",
    );
    let task = query_task(&db.connect().unwrap(), &chore_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Active, "task must stay in Doing");
}

/// With only a live `pr_review` execution (the actual reviewer), the
/// reviewer-fallback advances the lane as before.
#[test]
fn reviewer_fallback_advances_with_only_live_reviewer() {
    let db = WorkDb::open(temp_db_path("reviewer-advance-reviewer")).unwrap();
    let product_id = make_revision_product(&db, "advance-rev");
    let pr_url = "https://github.com/spinyfin/mono/pull/42";
    let chore_id = make_active_chore_with_pr(&db, &product_id, pr_url);

    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore_id.clone())
            .kind(ExecutionKind::PrReview)
            .status(ExecutionStatus::Running)
            .build(),
    )
    .unwrap();

    let advanced = db.advance_pending_review_task_to_in_review(&chore_id).unwrap();
    assert!(
        advanced,
        "a live pr_review execution must not block the reviewer-fallback"
    );
    let task = query_task(&db.connect().unwrap(), &chore_id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::InReview);
}

/// End-to-end proof that `get_work_tree_instrumented` captures the
/// per-item runtime N+1: for `N` items that each have a live execution and
/// a run, the `db.task_runtimes` segment must report `rows == N` and
/// `db_queries == 2 * N` (one latest-execution lookup + one latest-run
/// lookup per item). This is the segment expected to dominate the ~7s live
/// trace, and the reason the app-side `request` duration scales
/// super-linearly with item count.
#[test]
fn get_work_tree_instrumented_reports_task_runtime_nplus1() {
    use crate::population_timing::{PopulationTrace, segment};
    use std::time::Instant;

    let db = WorkDb::open(temp_db_path("pop-timing-nplus1")).unwrap();
    let product = db
        .create_product(CreateProductInput {
            name: "Boss".to_owned(),
            description: None,
            repo_remote_url: Some("git@github.com:spinyfin/mono.git".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap();

    const N: usize = 5;
    for i in 0..N {
        let chore = create_test_chore(&db, product.id.clone(), format!("Chore {i}"));
        // A live (running) execution keeps `query_task_runtime` on its
        // 2-query path: latest-execution is itself live, so no live-fallback
        // lookup, then one latest-run lookup.
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap();
        db.create_run(CreateRunInput {
            execution_id: execution.id.clone(),
            agent_id: "agent_1".to_owned(),
            status: Some("active".to_owned()),
            error_text: None,
            result_summary: None,
            transcript_path: None,
            artifacts_path: None,
            started_at: Some("100".to_owned()),
            finished_at: None,
        })
        .unwrap();
    }

    let mut trace = PopulationTrace::new(product.id.clone(), "req-nplus1", Some(1), Instant::now());
    let tree = db.get_work_tree_instrumented(&product.id, &mut trace).unwrap();
    assert_eq!(tree.chores.len(), N);
    assert_eq!(tree.task_runtimes.len(), N);

    let (_dur, rows, db_queries) = trace
        .segment_counts(segment::DB_TASK_RUNTIMES)
        .expect("db.task_runtimes segment must be recorded");
    assert_eq!(rows, Some(N as i64), "one runtime row per item");
    assert_eq!(
        db_queries,
        Some(2 * N as i64),
        "N+1 fan-out: 2 point queries per item (latest execution + latest run)"
    );

    // The bulk chores query, by contrast, is a single statement — no
    // per-row count is attributed to it.
    let (_dur, chore_rows, chore_queries) = trace
        .segment_counts(segment::DB_CHORES)
        .expect("db.chores segment must be recorded");
    assert_eq!(chore_rows, Some(N as i64));
    assert_eq!(chore_queries, None);

    // The uninstrumented wrapper returns the identical tree with no trace.
    let plain = db.get_work_tree(&product.id).unwrap();
    assert_eq!(plain.chores.len(), N);
}

// ── T1503/T1496 regression: revision rows must not jump to in_review ─────────
// while their execution is live.

/// Regression (T1503/T1496): `reconcile_revision_execution` must NOT create a
/// duplicate `ready` execution when the revision task already has a live
/// (`running` or `waiting_human`) execution. Before the fix the `_ =>` arm
/// fired for every reconcile tick while the execution was `waiting_human`
/// (because `waiting_human.can_reconcile()` is `false`, so neither of the two
/// update arms matched), producing an unbounded cascade of `ready` rows that
/// each spawned a fresh worker — up to 8+ concurrent workers on the same
/// revision task.
///
/// After the fix a new arm explicitly matches `is_live()` executions and
/// returns early without creating anything.
#[test]
fn reconcile_does_not_create_duplicate_execution_for_live_revision() {
    let db = WorkDb::open(temp_db_path("revision-no-dup-live")).unwrap();
    let product_id = make_revision_product(&db, "no-dup-live");
    let pr_url = "https://github.com/spinyfin/mono/pull/1425";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);
    let revision_id = insert_revision_row(&db, &product_id, &parent_id);

    // First reconcile creates the initial ready execution.
    db.reconcile_product_executions(&product_id).unwrap();
    let execs = executions_for(&db, &revision_id);
    assert_eq!(execs.len(), 1, "first reconcile must create exactly one execution");
    assert_eq!(execs[0].1, "ready");

    // Simulate pane spawn: coordinator sets task active, execution → waiting_human.
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'waiting_human' WHERE id = ?1",
        rusqlite::params![execs[0].0],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'active' WHERE id = ?1",
        rusqlite::params![revision_id],
    )
    .unwrap();
    drop(conn);

    // Second reconcile — the live execution guard must prevent a duplicate.
    db.reconcile_product_executions(&product_id).unwrap();
    let execs_after = executions_for(&db, &revision_id);
    assert_eq!(
        execs_after.len(),
        1,
        "reconcile must not create a duplicate execution while a live one exists: {execs_after:?}",
    );
    assert_eq!(
        task_status(&db, &revision_id),
        "active",
        "revision must remain active while its execution is live",
    );

    // Third reconcile: idempotent — still no duplicate.
    db.reconcile_product_executions(&product_id).unwrap();
    let execs_third = executions_for(&db, &revision_id);
    assert_eq!(
        execs_third.len(),
        1,
        "repeated reconcile must remain idempotent for live revision: {execs_third:?}",
    );
}

/// Variant: same guard applies when the execution is `running` (not just
/// `waiting_human`). A running execution is also `is_live()` and must not
/// receive a duplicate.
#[test]
fn reconcile_does_not_create_duplicate_execution_for_running_revision() {
    let db = WorkDb::open(temp_db_path("revision-no-dup-running")).unwrap();
    let product_id = make_revision_product(&db, "no-dup-running");
    let pr_url = "https://github.com/spinyfin/mono/pull/1426";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);
    let revision_id = insert_revision_row(&db, &product_id, &parent_id);

    // First reconcile creates the initial ready execution.
    db.reconcile_product_executions(&product_id).unwrap();
    let execs = executions_for(&db, &revision_id);
    assert_eq!(execs.len(), 1);
    assert_eq!(execs[0].1, "ready");

    // Simulate execution in running state.
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'running' WHERE id = ?1",
        rusqlite::params![execs[0].0],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'active' WHERE id = ?1",
        rusqlite::params![revision_id],
    )
    .unwrap();
    drop(conn);

    // Reconcile must not create a duplicate.
    db.reconcile_product_executions(&product_id).unwrap();
    let execs_after = executions_for(&db, &revision_id);
    assert_eq!(
        execs_after.len(),
        1,
        "reconcile must not create a duplicate execution while execution is running: {execs_after:?}",
    );
    assert_eq!(task_status(&db, &revision_id), "active");
}

// Design-family Fable-tier dispatch floor (policy addendum, 2026-07-13):
// `WorkDb::is_design_family` lineage-walking, and `create_revision`'s
// default `effort_level` derivation for design-family chain roots. The
// dispatch-floor precedence itself (model resolution, hand-set-vs-heuristic
// opt-out) is covered by the `effort.rs` unit tests
// (`resolve_spawn_config_with_family_floor`); these tests cover the two
// pieces that need a real `WorkDb`: lineage discovery and revision-create
// defaulting.

/// Create a project (auto-creating its `kind=design` seed task), mark that
/// design task "in review" with `pr_url`, and return the design task's id —
/// a design-family revision-chain-root fixture.
fn make_in_review_design_task(db: &WorkDb, product_id: &str, pr_url: &str) -> String {
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(product_id)
                .name("Design-family fixture project")
                .autostart(false)
                .build(),
        )
        .unwrap();
    let design = db
        .list_tasks(product_id, Some(&project.id), None, false)
        .unwrap()
        .into_iter()
        .find(|t| t.kind == TaskKind::Design)
        .expect("project should have an auto-created design task");
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
        params![design.id, pr_url],
    )
    .unwrap();
    design.id
}

/// Create an `investigation` task, mark it "in review" with `pr_url`, and
/// return its id — the other design-family kind, product-scoped (no
/// project needed).
fn make_in_review_investigation(db: &WorkDb, product_id: &str, pr_url: &str) -> String {
    let task = db
        .create_investigation(
            boss_protocol::CreateInvestigationInput::builder()
                .product_id(product_id)
                .name("Investigation fixture")
                .autostart(false)
                .build(),
        )
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
        params![task.id, pr_url],
    )
    .unwrap();
    task.id
}

// ── WorkDb::is_design_family ────────────────────────────────────────────────

#[test]
fn is_design_family_true_for_direct_design_and_investigation_tasks() {
    let db = WorkDb::open(temp_db_path("family-direct")).unwrap();
    let product_id = make_revision_product(&db, "family-direct");

    let design_id = make_in_review_design_task(&db, &product_id, "https://github.com/spinyfin/mono/pull/900");
    let design = query_task(&db.connect().unwrap(), &design_id).unwrap().unwrap();
    assert!(db.is_design_family(&design).unwrap(), "a design task is design-family");

    let investigation_id = make_in_review_investigation(&db, &product_id, "https://github.com/spinyfin/mono/pull/901");
    let investigation = query_task(&db.connect().unwrap(), &investigation_id).unwrap().unwrap();
    assert!(
        db.is_design_family(&investigation).unwrap(),
        "an investigation task is design-family"
    );
}

#[test]
fn is_design_family_false_for_plain_chore() {
    let db = WorkDb::open(temp_db_path("family-chore")).unwrap();
    let product_id = make_revision_product(&db, "family-chore");
    let chore = create_test_chore_manual(&db, &product_id, "Ordinary chore");
    assert!(!db.is_design_family(&chore).unwrap());
}

#[test]
fn is_design_family_true_transitively_through_revision_chain() {
    let db = WorkDb::open(temp_db_path("family-lineage")).unwrap();
    let product_id = make_revision_product(&db, "family-lineage");
    let design_id = make_in_review_design_task(&db, &product_id, "https://github.com/spinyfin/mono/pull/910");

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    // R1: revision of the design task.
    let r1 = db.create_revision(revision_input(&design_id), &checker).unwrap();
    assert!(
        db.is_design_family(&r1).unwrap(),
        "a revision of a design task is design-family"
    );

    // R2: revision of R1 — lineage walk must go two hops deep.
    let r2 = db.create_revision(revision_input(&r1.id), &checker).unwrap();
    assert!(
        db.is_design_family(&r2).unwrap(),
        "a revision of a revision of a design task is design-family"
    );
}

#[test]
fn is_design_family_false_for_revision_of_ordinary_chore() {
    let db = WorkDb::open(temp_db_path("family-lineage-chore")).unwrap();
    let product_id = make_revision_product(&db, "family-lineage-chore");
    let pr_url = "https://github.com/spinyfin/mono/pull/911";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&chore_id), &checker).unwrap();
    assert!(
        !db.is_design_family(&revision).unwrap(),
        "a revision of an ordinary chore is not design-family"
    );
}

// ── create_revision: design-family default effort ──────────────────────────

#[test]
fn create_revision_defaults_to_large_effort_for_design_task_parent() {
    let db = WorkDb::open(temp_db_path("default-large-design")).unwrap();
    let product_id = make_revision_product(&db, "default-large-design");
    let design_id = make_in_review_design_task(&db, &product_id, "https://github.com/spinyfin/mono/pull/920");

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&design_id), &checker).unwrap();
    assert_eq!(
        revision.effort_level,
        Some(boss_protocol::EffortLevel::Large),
        "a revision of a design task with no explicit --effort defaults to large"
    );
}

#[test]
fn create_revision_defaults_to_large_effort_for_investigation_parent() {
    let db = WorkDb::open(temp_db_path("default-large-investigation")).unwrap();
    let product_id = make_revision_product(&db, "default-large-investigation");
    let investigation_id = make_in_review_investigation(&db, &product_id, "https://github.com/spinyfin/mono/pull/921");

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&investigation_id), &checker).unwrap();
    assert_eq!(
        revision.effort_level,
        Some(boss_protocol::EffortLevel::Large),
        "a revision of an investigation task with no explicit --effort defaults to large"
    );
}

#[test]
fn create_revision_of_revision_of_design_task_still_defaults_to_large() {
    let db = WorkDb::open(temp_db_path("default-large-chain")).unwrap();
    let product_id = make_revision_product(&db, "default-large-chain");
    let design_id = make_in_review_design_task(&db, &product_id, "https://github.com/spinyfin/mono/pull/922");

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let r1 = db.create_revision(revision_input(&design_id), &checker).unwrap();
    assert_eq!(r1.effort_level, Some(boss_protocol::EffortLevel::Large));

    let r2 = db.create_revision(revision_input(&r1.id), &checker).unwrap();
    assert_eq!(
        r2.effort_level,
        Some(boss_protocol::EffortLevel::Large),
        "a revision-of-a-revision of a design task still defaults to large"
    );
}

#[test]
fn create_revision_explicit_effort_overrides_design_family_default() {
    // T289's regression: an explicit --effort must always win over the
    // design-family default, on either side (up or down).
    let db = WorkDb::open(temp_db_path("explicit-overrides-default")).unwrap();
    let product_id = make_revision_product(&db, "explicit-overrides-default");
    let design_id = make_in_review_design_task(&db, &product_id, "https://github.com/spinyfin/mono/pull/923");

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let input = CreateRevisionInput::builder()
        .parent_task_id(design_id)
        .description("mechanical doc-typo fix")
        .effort_level(boss_protocol::EffortLevel::Small)
        .build();
    let revision = db.create_revision(input, &checker).unwrap();
    assert_eq!(
        revision.effort_level,
        Some(boss_protocol::EffortLevel::Small),
        "an explicit --effort on a design-family revision must be stored verbatim"
    );
}

#[test]
fn create_revision_still_defaults_to_small_for_non_design_family_parent() {
    // Regression guard: the design-family carve-out must not change the
    // existing default for ordinary chore/task parents (revision-tasks.md
    // §Q7). Covered indirectly by `create_revision_inherits_product_and_project_from_root`;
    // pinned here explicitly next to the design-family cases for contrast.
    let db = WorkDb::open(temp_db_path("default-small-unchanged")).unwrap();
    let product_id = make_revision_product(&db, "default-small-unchanged");
    let pr_url = "https://github.com/spinyfin/mono/pull/924";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db.create_revision(revision_input(&chore_id), &checker).unwrap();
    assert_eq!(revision.effort_level, Some(boss_protocol::EffortLevel::Small));
}
