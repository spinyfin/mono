//! Behavioural coverage for the remaining `pr_flow.rs` PR-lifecycle
//! helpers not already exercised by `t14`:
//!   - `mark_chore_pr_closed_unmerged` (on-close terminal transition)
//!   - `mark_chore_pr_merged` return-value / idempotency semantics
//!   - `advance_pending_review_task_to_in_review` (reviewer-fallback advance)
//!   - `get_task_review_cycle_state` / `increment_task_review_cycle`
//!     (review-cycle counter + last-reviewed-sha round-trip)
//!
//! Every assertion is on an observable outcome — the returned value or the
//! task/comment state read back through a public accessor (`query_task`,
//! `get_comment`, `get_task_review_cycle_state`) — never on internal SQL or
//! private helpers. The in-memory `WorkDb` harness is the one the rest of the
//! suite uses.

use super::*;

/// Read a task row back through the public query path. Panics if absent —
/// every call site in this module created the row first.
fn task(db: &WorkDb, id: &str) -> Task {
    query_task(&db.connect().unwrap(), id).unwrap().unwrap()
}

/// Force a task into `archived` (there is no public archive helper that
/// leaves a `pr_url` in place, and the merge/close paths treat `archived`
/// exactly like `done` — already terminal).
fn set_archived(db: &WorkDb, id: &str) {
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'archived' WHERE id = ?1", params![id])
        .unwrap();
}

/// Soft-delete a task row.
fn soft_delete(db: &WorkDb, id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET deleted_at = ?2 WHERE id = ?1",
            params![id, now_string()],
        )
        .unwrap();
}

// ── mark_chore_pr_closed_unmerged ───────────────────────────────────────────

/// The happy path: an `in_review` chore whose bound PR closed unmerged moves
/// to `done`, keeps its `pr_url` (the close path never rewrites it), stamps
/// `completed_at`, and the returned `Task` reflects the new state.
#[test]
fn closed_unmerged_transitions_in_review_chore_to_done() {
    let db = WorkDb::open(temp_db_path("closed-unmerged-done")).unwrap();
    let product_id = make_revision_product(&db, "closed-done");
    let pr_url = "https://github.com/spinyfin/mono/pull/700";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let returned = db
        .mark_chore_pr_closed_unmerged(&chore_id, pr_url)
        .unwrap()
        .expect("an in_review chore with a matching pr_url must transition");
    assert_eq!(returned.status, TaskStatus::Done, "returned task must be done");
    assert_eq!(
        returned.pr_url.as_deref(),
        Some(pr_url),
        "the close path must preserve the existing pr_url"
    );
    assert!(returned.completed_at.is_some(), "reaching done must stamp completed_at");

    // Persisted state matches the returned value.
    let persisted = task(&db, &chore_id);
    assert_eq!(persisted.status, TaskStatus::Done);
    assert_eq!(persisted.pr_url.as_deref(), Some(pr_url));
    assert!(persisted.completed_at.is_some());
}

/// A chore already past `in_review` (`done` or `archived`) is a no-op:
/// `mark_chore_pr_closed_unmerged` returns `None` and leaves the row alone —
/// idempotent for late-arriving / duplicate close events.
#[test]
fn closed_unmerged_is_noop_for_already_terminal_task() {
    let db = WorkDb::open(temp_db_path("closed-unmerged-terminal")).unwrap();
    let product_id = make_revision_product(&db, "closed-terminal");
    let pr_url = "https://github.com/spinyfin/mono/pull/701";

    // Already done.
    let done_id = make_done_chore(&db, &product_id, pr_url);
    assert!(
        db.mark_chore_pr_closed_unmerged(&done_id, pr_url).unwrap().is_none(),
        "an already-done chore must not re-transition"
    );

    // Already archived.
    let archived_id = make_in_review_chore(&db, &make_revision_product(&db, "closed-arch"), pr_url);
    set_archived(&db, &archived_id);
    assert!(
        db.mark_chore_pr_closed_unmerged(&archived_id, pr_url)
            .unwrap()
            .is_none(),
        "an archived chore must not re-transition"
    );
    assert_eq!(task(&db, &archived_id).status, TaskStatus::Archived, "status untouched");
}

/// A soft-deleted chore is invisible to the close path: `None`, no change.
#[test]
fn closed_unmerged_is_noop_for_deleted_task() {
    let db = WorkDb::open(temp_db_path("closed-unmerged-deleted")).unwrap();
    let product_id = make_revision_product(&db, "closed-deleted");
    let pr_url = "https://github.com/spinyfin/mono/pull/702";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);
    soft_delete(&db, &chore_id);

    assert!(
        db.mark_chore_pr_closed_unmerged(&chore_id, pr_url).unwrap().is_none(),
        "a soft-deleted chore must not transition"
    );
    assert_eq!(
        task(&db, &chore_id).status,
        TaskStatus::InReview,
        "a deleted chore's status must be left untouched"
    );
}

/// The `WHERE pr_url = ?2` guard: a close event carrying a *different* PR url
/// than the one the chore is bound to matches zero rows, so the status stays
/// `in_review`. The function still returns `Some(task)` (the row is neither
/// terminal nor deleted), but that task reflects the *unchanged* state — a
/// stale/misrouted close must not retire the wrong PR's task.
#[test]
fn closed_unmerged_pr_url_mismatch_does_not_transition() {
    let db = WorkDb::open(temp_db_path("closed-unmerged-mismatch")).unwrap();
    let product_id = make_revision_product(&db, "closed-mismatch");
    let bound_pr = "https://github.com/spinyfin/mono/pull/703";
    let other_pr = "https://github.com/spinyfin/mono/pull/999";
    let chore_id = make_in_review_chore(&db, &product_id, bound_pr);

    let returned = db
        .mark_chore_pr_closed_unmerged(&chore_id, other_pr)
        .unwrap()
        .expect("a live in_review chore is returned even when the pr_url guard skips the update");
    assert_eq!(
        returned.status,
        TaskStatus::InReview,
        "a mismatched pr_url must leave the task in in_review"
    );
    assert_eq!(
        task(&db, &chore_id).status,
        TaskStatus::InReview,
        "persisted status must be unchanged when the pr_url does not match"
    );
}

/// Unlike the merged path, `mark_chore_pr_closed_unmerged` must NOT resolve
/// the chore's `in_revision` comments: the close-unmerged reconciliation story
/// is to *reopen* them (via the separate `reopen_comments_for_closed_unmerged_pr`),
/// so this function leaving them as-is is load-bearing — resolving here would
/// immediately undo that reopen.
#[test]
fn closed_unmerged_leaves_in_revision_comments_untouched() {
    let db = WorkDb::open(temp_db_path("closed-unmerged-comments")).unwrap();
    let product_id = make_revision_product(&db, "closed-comments");
    let pr_url = "https://github.com/spinyfin/mono/pull/704";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    // A comment left `in_revision`, claimed by the chore, as a `[Revise]`
    // batch would have before the PR closed.
    let comment = db
        .create_comment(CreateCommentInput {
            artifact_kind: "work_item".to_owned(),
            artifact_id: chore_id.clone(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: "please revise this".to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "reviewer note".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 1,
        })
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_comments SET status = 'in_revision', revise_task_id = ?2 WHERE id = ?1",
            params![comment.id, chore_id],
        )
        .unwrap();

    db.mark_chore_pr_closed_unmerged(&chore_id, pr_url).unwrap();

    let after = db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(
        after.status, "in_revision",
        "close-unmerged must not resolve in_revision comments (they are reopened separately)"
    );
    assert_eq!(
        after.revise_task_id.as_deref(),
        Some(chore_id.as_str()),
        "the comment's revise_task_id claim must be preserved for the reopen pass"
    );
}

// ── mark_chore_pr_merged return-value / idempotency ─────────────────────────

/// The merged path advances an `in_review` chore to `done`, records the
/// `pr_url`, and returns the updated task. (Side-effects like the revision
/// cascade are covered in t09/t12/t20; here we pin the plain return value.)
#[test]
fn merged_transitions_in_review_chore_to_done() {
    let db = WorkDb::open(temp_db_path("merged-done")).unwrap();
    let product_id = make_revision_product(&db, "merged-done");
    let pr_url = "https://github.com/spinyfin/mono/pull/800";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let returned = db
        .mark_chore_pr_merged(&chore_id, pr_url)
        .unwrap()
        .expect("an in_review chore must transition on merge");
    assert_eq!(returned.status, TaskStatus::Done);
    assert_eq!(returned.pr_url.as_deref(), Some(pr_url), "merge stamps the pr_url");
    assert!(returned.completed_at.is_some(), "merge stamps completed_at");
    assert_eq!(task(&db, &chore_id).status, TaskStatus::Done);
}

/// `mark_chore_pr_merged` is idempotent against already-terminal and deleted
/// rows: `None`, no change — safe for a merge poller that re-observes a merge.
#[test]
fn merged_is_noop_for_terminal_or_deleted_task() {
    let pr_url = "https://github.com/spinyfin/mono/pull/801";

    // Already done.
    let db = WorkDb::open(temp_db_path("merged-noop-done")).unwrap();
    let done_id = make_done_chore(&db, &make_revision_product(&db, "merged-done2"), pr_url);
    assert!(db.mark_chore_pr_merged(&done_id, pr_url).unwrap().is_none());

    // Archived.
    let arch_id = make_in_review_chore(&db, &make_revision_product(&db, "merged-arch"), pr_url);
    set_archived(&db, &arch_id);
    assert!(db.mark_chore_pr_merged(&arch_id, pr_url).unwrap().is_none());
    assert_eq!(task(&db, &arch_id).status, TaskStatus::Archived);

    // Deleted.
    let del_id = make_in_review_chore(&db, &make_revision_product(&db, "merged-del"), pr_url);
    soft_delete(&db, &del_id);
    assert!(db.mark_chore_pr_merged(&del_id, pr_url).unwrap().is_none());
    assert_eq!(
        task(&db, &del_id).status,
        TaskStatus::InReview,
        "a deleted chore's status is left untouched by the merge path"
    );
}

// ── advance_pending_review_task_to_in_review ────────────────────────────────

/// Put a chore into `active` (Doing) with a bound PR url — the state the
/// reviewer-fallback advance inspects.
fn active_chore_with_pr(db: &WorkDb, label: &str, pr_url: &str) -> String {
    let product_id = make_revision_product(db, label);
    let chore_id = make_in_review_chore(db, &product_id, pr_url);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", params![chore_id])
        .unwrap();
    chore_id
}

/// The basic advance: an `active` task with a `pr_url` and no live worker
/// moves to `in_review` and reports `true`.
#[test]
fn advance_moves_active_task_with_pr_to_in_review() {
    let db = WorkDb::open(temp_db_path("advance-basic")).unwrap();
    let chore_id = active_chore_with_pr(&db, "advance-basic", "https://github.com/spinyfin/mono/pull/900");

    let advanced = db.advance_pending_review_task_to_in_review(&chore_id).unwrap();
    assert!(advanced, "an active task with a pr_url and no live worker must advance");
    assert_eq!(task(&db, &chore_id).status, TaskStatus::InReview);
}

/// Idempotent: a task already past `active` (e.g. already `in_review`) reports
/// `false` and is left unchanged — a second fallback sweep is harmless.
#[test]
fn advance_is_idempotent_when_already_in_review() {
    let db = WorkDb::open(temp_db_path("advance-idempotent")).unwrap();
    let product_id = make_revision_product(&db, "advance-idem");
    // make_in_review_chore leaves the task in `in_review`.
    let chore_id = make_in_review_chore(&db, &product_id, "https://github.com/spinyfin/mono/pull/901");

    let advanced = db.advance_pending_review_task_to_in_review(&chore_id).unwrap();
    assert!(!advanced, "a task already in in_review must not re-advance");
    assert_eq!(task(&db, &chore_id).status, TaskStatus::InReview);
}

/// Refuses to advance a task with no `pr_url`, and a soft-deleted task —
/// both report `false` and leave the row untouched.
#[test]
fn advance_refuses_without_pr_or_when_deleted() {
    // No pr_url: an active task with an empty pr_url must not advance.
    let db = WorkDb::open(temp_db_path("advance-no-pr")).unwrap();
    let no_pr = active_chore_with_pr(&db, "advance-no-pr", "");
    assert!(
        !db.advance_pending_review_task_to_in_review(&no_pr).unwrap(),
        "an active task without a pr_url must not advance"
    );
    assert_eq!(task(&db, &no_pr).status, TaskStatus::Active);

    // Soft-deleted: excluded even with a valid pr_url.
    let deleted = active_chore_with_pr(&db, "advance-deleted", "https://github.com/spinyfin/mono/pull/902");
    soft_delete(&db, &deleted);
    assert!(
        !db.advance_pending_review_task_to_in_review(&deleted).unwrap(),
        "a soft-deleted task must not advance"
    );
}

// ── get_task_review_cycle_state / increment_task_review_cycle ────────────────

/// A brand-new task starts at cycle 0 with no last-reviewed sha.
#[test]
fn review_cycle_state_starts_at_zero_with_no_sha() {
    let (db, _p, chore_id) = setup_product_and_chore();
    let (cycle, last_sha) = db.get_task_review_cycle_state(&chore_id).unwrap();
    assert_eq!(cycle, 0, "a fresh task has completed zero review cycles");
    assert!(last_sha.is_none(), "a fresh task has no last-reviewed sha");
}

/// Each increment bumps the counter by one and round-trips the recorded sha;
/// successive passes accumulate on the same row and overwrite the sha.
#[test]
fn review_cycle_increments_and_round_trips_sha() {
    let (db, _p, chore_id) = setup_product_and_chore();

    db.increment_task_review_cycle(&chore_id, Some("sha_pass_1")).unwrap();
    let (cycle, last_sha) = db.get_task_review_cycle_state(&chore_id).unwrap();
    assert_eq!(cycle, 1);
    assert_eq!(last_sha.as_deref(), Some("sha_pass_1"));

    db.increment_task_review_cycle(&chore_id, Some("sha_pass_2")).unwrap();
    let (cycle, last_sha) = db.get_task_review_cycle_state(&chore_id).unwrap();
    assert_eq!(cycle, 2, "a second pass accumulates on the same counter");
    assert_eq!(
        last_sha.as_deref(),
        Some("sha_pass_2"),
        "the most recent pass's sha overwrites the prior one"
    );
}

/// A `None` or empty-string sha records `NULL` — the reviewer could not
/// determine the PR HEAD — while still advancing the cycle counter.
#[test]
fn review_cycle_none_or_empty_sha_records_null() {
    let (db, _p, chore_id) = setup_product_and_chore();

    // Seed a real sha first so we can prove it is cleared, not merely absent.
    db.increment_task_review_cycle(&chore_id, Some("sha_seed")).unwrap();

    db.increment_task_review_cycle(&chore_id, None).unwrap();
    let (cycle, last_sha) = db.get_task_review_cycle_state(&chore_id).unwrap();
    assert_eq!(cycle, 2, "a None sha still advances the counter");
    assert!(last_sha.is_none(), "a None sha records NULL");

    db.increment_task_review_cycle(&chore_id, Some("")).unwrap();
    let (cycle, last_sha) = db.get_task_review_cycle_state(&chore_id).unwrap();
    assert_eq!(cycle, 3, "an empty sha still advances the counter");
    assert!(last_sha.is_none(), "an empty-string sha is stored as NULL, not \"\"");
}

/// `increment_task_review_cycle` errors on an unknown or soft-deleted task —
/// there is no row to advance, and silently succeeding would hide a bug.
#[test]
fn review_cycle_increment_errors_on_unknown_or_deleted_task() {
    let (db, _p, chore_id) = setup_product_and_chore();

    assert!(
        db.increment_task_review_cycle("chr_does_not_exist", Some("sha"))
            .is_err(),
        "incrementing an unknown task must error"
    );

    soft_delete(&db, &chore_id);
    assert!(
        db.increment_task_review_cycle(&chore_id, Some("sha")).is_err(),
        "incrementing a soft-deleted task must error (WHERE deleted_at IS NULL matches nothing)"
    );
}
