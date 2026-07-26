//! Tests for the abandoned-branch-PR sweep's DB layer
//! ([`WorkDb::list_abandoned_pushed_branch_candidates`] and
//! [`WorkDb::bind_pr_to_task_from_terminal_execution`]). See
//! `crate::abandoned_branch_pr_sweep` for the incident these close and the
//! sweep logic that consumes these queries.

use super::*;

/// Fetch a chore/task's current `(status, pr_url)` via [`WorkDb::get_work_item`].
fn task_status_and_pr_url(db: &WorkDb, work_item_id: &str) -> (TaskStatus, Option<String>) {
    match db.get_work_item(work_item_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => (t.status, t.pr_url),
        other => panic!("expected a Task/Chore work item, got {other:?}"),
    }
}

/// Backdate `finished_at` on `execution_id` by `age_secs` seconds so grace-
/// period gates in the candidate query can be exercised without a real
/// sleep. Mirrors the raw-SQL backdating pattern used elsewhere in the
/// sweep test suites (e.g. `pool_claim_sweep`).
fn backdate_finished_at(db: &WorkDb, execution_id: &str, age_secs: i64) {
    let conn = db.connect().unwrap();
    let backdated = (boss_engine_utils::epoch_time::now_epoch_secs() - age_secs).to_string();
    conn.execute(
        "UPDATE work_executions SET finished_at = ?2 WHERE id = ?1",
        rusqlite::params![execution_id, backdated],
    )
    .unwrap();
}

/// Create a chore whose single execution is terminal (`abandoned`), has a
/// recorded workspace, and carries no `pr_url` anywhere — the baseline
/// abandoned-branch-PR candidate shape. `task_status` lets callers exercise
/// every kanban status the sweep must (or must not) treat as recoverable.
fn make_terminal_chore(db: &WorkDb, label: &str, task_status: &str, age_secs: i64) -> (String, String, String) {
    let (_product_id, chore_id, exec_id) = make_waiting_human_chore(db, label);
    db.mark_execution_redundant(&exec_id).unwrap();
    backdate_finished_at(db, &exec_id, age_secs);
    db.update_work_item(
        &chore_id,
        WorkItemPatch {
            status: Some(task_status.to_owned()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    (_product_id, chore_id, exec_id)
}

const GRACE_SECS: i64 = 15 * 60;
const LOOKBACK_SECS: i64 = 7 * 24 * 60 * 60;

#[test]
fn list_abandoned_finds_terminated_execution_for_todo_task() {
    let db = WorkDb::open(temp_db_path("abandoned-pr-todo")).unwrap();
    // The whole point of this query vs. the Bug-B late-PR query: it must
    // still find the row after the task fell back to `todo`, not just
    // `active`.
    let (_, chore_id, exec_id) = make_terminal_chore(&db, "abandoned-pr-todo", "todo", GRACE_SECS + 60);

    let candidates = db
        .list_abandoned_pushed_branch_candidates(GRACE_SECS, LOOKBACK_SECS)
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].execution_id, exec_id);
    assert_eq!(candidates[0].work_item_id, chore_id);
}

#[test]
fn list_abandoned_finds_terminated_execution_for_active_and_blocked_task() {
    let db = WorkDb::open(temp_db_path("abandoned-pr-active-blocked")).unwrap();
    let (_, _, exec_active) = make_terminal_chore(&db, "abandoned-pr-active", "active", GRACE_SECS + 60);
    let (_, _, exec_blocked) = make_terminal_chore(&db, "abandoned-pr-blocked", "blocked", GRACE_SECS + 60);

    let candidates = db
        .list_abandoned_pushed_branch_candidates(GRACE_SECS, LOOKBACK_SECS)
        .unwrap();
    let ids: Vec<_> = candidates.iter().map(|c| c.execution_id.clone()).collect();
    assert!(ids.contains(&exec_active));
    assert!(ids.contains(&exec_blocked));
}

#[test]
fn list_abandoned_excludes_execution_still_within_grace_period() {
    let db = WorkDb::open(temp_db_path("abandoned-pr-grace")).unwrap();
    // Finished only 60s ago — well inside the 15-minute grace window, so
    // this looks like "worker hasn't reached PR creation yet," not
    // abandoned.
    make_terminal_chore(&db, "abandoned-pr-grace", "todo", 60);

    let candidates = db
        .list_abandoned_pushed_branch_candidates(GRACE_SECS, LOOKBACK_SECS)
        .unwrap();
    assert!(
        candidates.is_empty(),
        "an execution that finished inside the grace period must not be a candidate yet"
    );
}

#[test]
fn list_abandoned_excludes_execution_beyond_lookback() {
    let db = WorkDb::open(temp_db_path("abandoned-pr-lookback")).unwrap();
    make_terminal_chore(&db, "abandoned-pr-lookback", "todo", LOOKBACK_SECS + 3600);

    let candidates = db
        .list_abandoned_pushed_branch_candidates(GRACE_SECS, LOOKBACK_SECS)
        .unwrap();
    assert!(
        candidates.is_empty(),
        "an execution older than the lookback bound must be excluded"
    );
}

#[test]
fn list_abandoned_excludes_task_with_pr_url_already_set() {
    let db = WorkDb::open(temp_db_path("abandoned-pr-has-pr")).unwrap();
    let (_, chore_id, _) = make_terminal_chore(&db, "abandoned-pr-has-pr", "todo", GRACE_SECS + 60);
    db.update_work_item(
        &chore_id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some("https://github.com/foo/bar/pull/1".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let candidates = db
        .list_abandoned_pushed_branch_candidates(GRACE_SECS, LOOKBACK_SECS)
        .unwrap();
    assert!(
        candidates.is_empty(),
        "a task that already has a pr_url must not be re-surfaced"
    );
}

#[test]
fn list_abandoned_excludes_closed_task_statuses() {
    let db = WorkDb::open(temp_db_path("abandoned-pr-closed")).unwrap();
    make_terminal_chore(&db, "abandoned-pr-done", "done", GRACE_SECS + 60);
    make_terminal_chore(&db, "abandoned-pr-archived", "archived", GRACE_SECS + 60);
    make_terminal_chore(&db, "abandoned-pr-cancelled", "cancelled", GRACE_SECS + 60);

    let candidates = db
        .list_abandoned_pushed_branch_candidates(GRACE_SECS, LOOKBACK_SECS)
        .unwrap();
    assert!(
        candidates.is_empty(),
        "an explicit close decision (done/archived/cancelled) must never be auto-recovered"
    );
}

#[test]
fn list_abandoned_excludes_work_item_with_a_live_execution() {
    let db = WorkDb::open(temp_db_path("abandoned-pr-live-worker")).unwrap();
    let (_, chore_id, _terminal_exec) = make_terminal_chore(&db, "abandoned-pr-live", "todo", GRACE_SECS + 60);

    // The reconcile sweep re-enqueued a fresh execution on the same work
    // item after the task fell back to `todo` (autostart chore) — still
    // running when this sweep pass runs.
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore_id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:foo/bar.git")
            .build(),
    )
    .unwrap();

    let candidates = db
        .list_abandoned_pushed_branch_candidates(GRACE_SECS, LOOKBACK_SECS)
        .unwrap();
    assert!(
        candidates.is_empty(),
        "a work item with a live execution must never be a candidate — opening a PR from the \
         dead run's stale branch would strand the running worker"
    );
}

#[test]
fn bind_pr_to_task_from_terminal_execution_refuses_work_item_with_live_execution() {
    let db = WorkDb::open(temp_db_path("bind-pr-live-worker")).unwrap();
    let (_, chore_id, _) = make_terminal_chore(&db, "bind-pr-live", "todo", GRACE_SECS + 60);

    // A live execution appears on the work item between the candidate
    // query and the bind call — the bind itself must also refuse.
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore_id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:foo/bar.git")
            .build(),
    )
    .unwrap();

    let updated = db
        .bind_pr_to_task_from_terminal_execution(&chore_id, "https://github.com/foo/bar/pull/7")
        .unwrap();
    assert!(!updated, "must not bind while a live execution exists on the work item");

    let (_, pr_url) = task_status_and_pr_url(&db, &chore_id);
    assert_eq!(pr_url, None);
}

#[test]
fn bind_pr_to_task_from_terminal_execution_transitions_todo_task_to_in_review() {
    let db = WorkDb::open(temp_db_path("bind-pr-todo")).unwrap();
    let (_, chore_id, _) = make_terminal_chore(&db, "bind-pr-todo", "todo", GRACE_SECS + 60);

    let updated = db
        .bind_pr_to_task_from_terminal_execution(&chore_id, "https://github.com/foo/bar/pull/42")
        .unwrap();
    assert!(updated, "a todo task with no pr_url must be bindable");

    let (status, pr_url) = task_status_and_pr_url(&db, &chore_id);
    assert_eq!(status.as_str(), "in_review");
    assert_eq!(pr_url.as_deref(), Some("https://github.com/foo/bar/pull/42"));
}

#[test]
fn bind_pr_to_task_from_terminal_execution_is_idempotent_when_pr_url_already_set() {
    let db = WorkDb::open(temp_db_path("bind-pr-idempotent")).unwrap();
    let (_, chore_id, _) = make_terminal_chore(&db, "bind-pr-idempotent", "todo", GRACE_SECS + 60);

    let first = db
        .bind_pr_to_task_from_terminal_execution(&chore_id, "https://github.com/foo/bar/pull/1")
        .unwrap();
    assert!(first);

    // A concurrent sweep pass (or a human) racing the same bind must be a
    // no-op, not a silent overwrite of a different URL.
    let second = db
        .bind_pr_to_task_from_terminal_execution(&chore_id, "https://github.com/foo/bar/pull/2")
        .unwrap();
    assert!(!second, "must not rebind a task that already carries a pr_url");

    let (_, pr_url) = task_status_and_pr_url(&db, &chore_id);
    assert_eq!(pr_url.as_deref(), Some("https://github.com/foo/bar/pull/1"));
}

#[test]
fn bind_pr_to_task_from_terminal_execution_refuses_closed_task() {
    let db = WorkDb::open(temp_db_path("bind-pr-closed")).unwrap();
    let (_, chore_id, _) = make_terminal_chore(&db, "bind-pr-closed", "done", GRACE_SECS + 60);

    let updated = db
        .bind_pr_to_task_from_terminal_execution(&chore_id, "https://github.com/foo/bar/pull/9")
        .unwrap();
    assert!(
        !updated,
        "an explicitly closed task must never be reopened by auto-heal"
    );

    let (status, pr_url) = task_status_and_pr_url(&db, &chore_id);
    assert_eq!(status.as_str(), "done");
    assert_eq!(pr_url, None);
}
