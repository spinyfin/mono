//! Behavioural coverage for the previously-untested `pr_flow.rs`
//! PR-lifecycle helpers:
//!   - `list_tasks_with_stalled_reviewer` (reviewer-fallback candidate query)
//!   - `update_task_pr_poll_state` (merge-poller CI/review/merge-queue writer)
//!   - `reopen_comments_for_closed_unmerged_pr` (close-unmerged reconciliation)
//!
//! These assert observable outcomes (return values + resulting DB state), not
//! the internal SQL, and reuse the in-memory `WorkDb` harness the rest of the
//! suite uses.

use super::*;

// ── list_tasks_with_stalled_reviewer ────────────────────────────────────────

/// Put a freshly-created in-review chore into `active` (Doing) with a bound PR
/// url — the state the reviewer-fallback sweep inspects. Returns
/// `(product_id, chore_id)`.
fn active_chore_with_pr(db: &WorkDb, label: &str, pr_url: &str) -> (String, String) {
    let product_id = make_revision_product(db, label);
    let chore_id = make_in_review_chore(db, &product_id, pr_url);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", params![chore_id])
        .unwrap();
    (product_id, chore_id)
}

/// Create a `pr_review` execution on `work_item_id` with the given status, then
/// force its `created_at` to `created_at_secs` (a unix-seconds string) so the
/// stale-cutoff arm is deterministic regardless of wall-clock timing.
fn pr_review_execution(db: &WorkDb, work_item_id: &str, status: ExecutionStatus, created_at_secs: &str) -> String {
    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(work_item_id.to_owned())
                .kind(ExecutionKind::PrReview)
                .status(status)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_at = ?2 WHERE id = ?1",
            params![exec.id, created_at_secs],
        )
        .unwrap();
    exec.id
}

/// Arm (a): a `pr_review` execution in any terminal status — on an `active`
/// task with a non-empty `pr_url` — surfaces the task, regardless of how
/// recently it was created (terminality alone is enough, no timeout needed).
#[test]
fn stalled_reviewer_surfaces_task_with_terminal_pr_review() {
    for status in [
        ExecutionStatus::Completed,
        ExecutionStatus::Abandoned,
        ExecutionStatus::Failed,
        ExecutionStatus::Cancelled,
        ExecutionStatus::Orphaned,
    ] {
        let db = WorkDb::open(temp_db_path("stalled-terminal")).unwrap();
        let pr_url = "https://github.com/spinyfin/mono/pull/1";
        let (product_id, chore_id) = active_chore_with_pr(&db, "stalled-terminal", pr_url);
        // Created "now" (recent) so only the terminal arm can surface it.
        pr_review_execution(&db, &chore_id, status.clone(), &now_string());

        let stalled = db.list_tasks_with_stalled_reviewer(3600).unwrap();
        assert_eq!(
            stalled.len(),
            1,
            "a terminal ({status:?}) pr_review on an active task with a pr_url must surface it"
        );
        assert_eq!(stalled[0].0, chore_id, "task id");
        assert_eq!(stalled[0].1, product_id, "product id");
        assert_eq!(stalled[0].2, pr_url, "pr_url");
    }
}

/// Arm (b): a non-terminal (`running`) `pr_review` surfaces the task only when
/// it was created before the stale cutoff (timeout); a freshly-created one does
/// not.
#[test]
fn stalled_reviewer_surfaces_running_reviewer_only_when_older_than_cutoff() {
    let stale_secs = 3600u64;
    let now: u64 = now_string().parse().unwrap();

    // Created two hours ago; the cutoff is one hour ago → created_at < cutoff.
    let old_db = WorkDb::open(temp_db_path("stalled-old")).unwrap();
    let (_p, old_chore) = active_chore_with_pr(&old_db, "stalled-old", "https://github.com/spinyfin/mono/pull/2");
    pr_review_execution(&old_db, &old_chore, ExecutionStatus::Running, &(now - 7200).to_string());
    let stalled = old_db.list_tasks_with_stalled_reviewer(stale_secs).unwrap();
    assert_eq!(
        stalled.len(),
        1,
        "a running reviewer older than the stale cutoff must surface (timeout)"
    );
    assert_eq!(stalled[0].0, old_chore);

    // Created "now" → newer than the cutoff → must NOT surface.
    let fresh_db = WorkDb::open(temp_db_path("stalled-fresh")).unwrap();
    let (_p2, fresh_chore) =
        active_chore_with_pr(&fresh_db, "stalled-fresh", "https://github.com/spinyfin/mono/pull/3");
    pr_review_execution(&fresh_db, &fresh_chore, ExecutionStatus::Running, &now.to_string());
    let none = fresh_db.list_tasks_with_stalled_reviewer(stale_secs).unwrap();
    assert!(
        none.is_empty(),
        "a running reviewer newer than the cutoff must not surface; got {none:?}"
    );
}

/// The query filters out tasks that are not `active`, have a NULL/empty
/// `pr_url`, or are soft-deleted — and returns each qualifying task exactly
/// once even when it has multiple stalled reviewers.
#[test]
fn stalled_reviewer_applies_filters_and_returns_distinct() {
    let db = WorkDb::open(temp_db_path("stalled-filters")).unwrap();
    let now = now_string();
    let now_secs: u64 = now.parse().unwrap();
    let set_active = |id: &str| {
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", params![id])
            .unwrap();
    };
    // Each chore gets its own product: `make_in_review_chore` uses a fixed
    // chore name, so co-locating several in one product trips the
    // duplicate-name guard.

    // (a) Not `active` (still in_review) — excluded despite a terminal reviewer.
    let in_review = make_in_review_chore(
        &db,
        &make_revision_product(&db, "filt-in-review"),
        "https://github.com/spinyfin/mono/pull/10",
    );
    pr_review_execution(&db, &in_review, ExecutionStatus::Completed, &now);

    // (b) Active but NULL pr_url — excluded.
    let null_pr = make_chore_root(&db, &make_revision_product(&db, "filt-null-pr"), "null-pr");
    set_active(&null_pr);
    pr_review_execution(&db, &null_pr, ExecutionStatus::Completed, &now);

    // (c) Active but empty pr_url — excluded.
    let empty_pr = make_in_review_chore(&db, &make_revision_product(&db, "filt-empty-pr"), "");
    set_active(&empty_pr);
    pr_review_execution(&db, &empty_pr, ExecutionStatus::Completed, &now);

    // (d) Soft-deleted active task with a terminal reviewer — excluded. The
    // execution is created while the task is still live (create_execution
    // resolves the task's product), then the task is tombstoned.
    let deleted = make_in_review_chore(
        &db,
        &make_revision_product(&db, "filt-deleted"),
        "https://github.com/spinyfin/mono/pull/11",
    );
    set_active(&deleted);
    pr_review_execution(&db, &deleted, ExecutionStatus::Completed, &now);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET deleted_at = ?2 WHERE id = ?1",
            params![deleted, now_string()],
        )
        .unwrap();

    // A qualifying task with TWO stalled reviewers (one terminal + one
    // timed-out) must appear exactly once (DISTINCT).
    let qualifying = make_in_review_chore(
        &db,
        &make_revision_product(&db, "filt-qualifying"),
        "https://github.com/spinyfin/mono/pull/12",
    );
    set_active(&qualifying);
    pr_review_execution(&db, &qualifying, ExecutionStatus::Abandoned, &now);
    pr_review_execution(
        &db,
        &qualifying,
        ExecutionStatus::Running,
        &(now_secs - 7200).to_string(),
    );

    let stalled = db.list_tasks_with_stalled_reviewer(3600).unwrap();
    assert_eq!(
        stalled.len(),
        1,
        "only the qualifying active/pr_url/non-deleted task must surface, once; got {stalled:?}"
    );
    assert_eq!(stalled[0].0, qualifying);
}

// ── update_task_pr_poll_state ───────────────────────────────────────────────

/// Read a task's `pr_state_polled_at` timestamp directly.
fn poll_ts(db: &WorkDb, task_id: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT pr_state_polled_at FROM tasks WHERE id = ?1",
            params![task_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
}

/// Build a [`PrPollStateInput`] with just `ci_required_state` /
/// `review_required_state` set and every other field defaulted (`None`).
/// Cuts the boilerplate for the majority of test call sites below that don't
/// exercise the merge-queue dimension.
fn ci_review_input<'a>(ci_required_state: &'a str, review_required_state: &'a str) -> PrPollStateInput<'a> {
    PrPollStateInput {
        ci_required_state,
        review_required_state,
        ..Default::default()
    }
}

/// First probe after migration: the state moves from NULL → set, so `changed`
/// is true, `prior_ci_state` is None, and the poll timestamp is stamped.
#[test]
fn poll_state_first_probe_reports_change_with_null_prior() {
    let (db, _p, chore_id) = setup_product_and_chore();
    let out = db
        .update_task_pr_poll_state(&chore_id, ci_review_input("success", "approved"))
        .unwrap();
    assert!(out.changed, "first probe (NULL → set) must count as changed");
    assert!(
        out.prior_ci_state.is_none(),
        "no prior ci state exists before the first probe"
    );
    assert!(
        poll_ts(&db, &chore_id).is_some(),
        "pr_state_polled_at must be stamped on the first probe"
    );
}

/// An identical (equal-state) probe reports `changed = false` but still stamps
/// the poll timestamp — the documented "always stamp so operators can see the
/// poller is alive" invariant. `prior_ci_state` reflects the stored value.
#[test]
fn poll_state_unchanged_probe_still_stamps_timestamp() {
    let (db, _p, chore_id) = setup_product_and_chore();
    let input = PrPollStateInput {
        ci_required_state: "success",
        review_required_state: "approved",
        merge_queue_state: Some("mergeable"),
        ..Default::default()
    };
    db.update_task_pr_poll_state(&chore_id, input).unwrap();
    // Clear the stamp so we can prove the *unchanged* probe re-stamps it.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_state_polled_at = NULL WHERE id = ?1",
            params![chore_id],
        )
        .unwrap();

    let out = db.update_task_pr_poll_state(&chore_id, input).unwrap();
    assert!(!out.changed, "an identical probe must report changed = false");
    assert_eq!(
        out.prior_ci_state.as_deref(),
        Some("success"),
        "prior_ci_state reflects the value stored before this update"
    );
    assert!(
        poll_ts(&db, &chore_id).is_some(),
        "pr_state_polled_at must be stamped even when nothing changed"
    );
}

/// `changed` is true only when CI, review, or merge-queue state actually
/// differs; `prior_ci_state` always reports the pre-update CI value, enabling
/// fail → success detection.
#[test]
fn poll_state_change_detection_across_dimensions() {
    let (db, _p, chore_id) = setup_product_and_chore();
    // Baseline.
    db.update_task_pr_poll_state(&chore_id, ci_review_input("failure", "pending"))
        .unwrap();

    // Only the review dimension moves.
    let out = db
        .update_task_pr_poll_state(&chore_id, ci_review_input("failure", "approved"))
        .unwrap();
    assert!(out.changed, "a review-state change must count as changed");
    assert_eq!(out.prior_ci_state.as_deref(), Some("failure"));

    // Only the merge-queue dimension moves.
    let queued_failure = PrPollStateInput {
        ci_required_state: "failure",
        review_required_state: "approved",
        merge_queue_state: Some("queued"),
        ..Default::default()
    };
    let out = db.update_task_pr_poll_state(&chore_id, queued_failure).unwrap();
    assert!(out.changed, "a merge-queue-state change must count as changed");

    // Nothing moves → not changed.
    let out = db.update_task_pr_poll_state(&chore_id, queued_failure).unwrap();
    assert!(!out.changed, "an identical probe must not count as changed");

    // fail → success: prior_ci_state must still report the pre-update 'failure'
    // so the caller can clear a stale "ci failing" badge.
    let queued_success = PrPollStateInput {
        ci_required_state: "success",
        review_required_state: "approved",
        merge_queue_state: Some("queued"),
        ..Default::default()
    };
    let out = db.update_task_pr_poll_state(&chore_id, queued_success).unwrap();
    assert!(out.changed, "a ci fail → success transition must count as changed");
    assert_eq!(
        out.prior_ci_state.as_deref(),
        Some("failure"),
        "prior_ci_state must expose the previous 'failure' for fail → success detection"
    );
}

/// `merge_queue_detail` (the JSON sub-state blob: queue position, GitHub's
/// raw entry state, enqueued-at) persists alongside `merge_queue_state` and
/// independently drives `changed` — a position tick with no other dimension
/// moving must still be observable so the Review card can refresh.
#[test]
fn poll_state_merge_queue_detail_persists_and_drives_change() {
    let (db, _p, chore_id) = setup_product_and_chore();
    let detail_v1 = r#"{"position":3,"state":"QUEUED","enqueued_at":"2026-07-10T11:54:54Z"}"#;
    let out = db
        .update_task_pr_poll_state(
            &chore_id,
            PrPollStateInput {
                ci_required_state: "success",
                review_required_state: "approved",
                merge_queue_state: Some("queued"),
                merge_queue_detail: Some(detail_v1),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(out.changed, "first write of merge_queue_detail must count as changed");

    let stored: Option<String> = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT merge_queue_detail FROM tasks WHERE id = ?1",
            params![chore_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored.as_deref(),
        Some(detail_v1),
        "merge_queue_detail must be persisted verbatim"
    );

    // Same merge_queue_state ("queued"), but the position ticked down — this
    // alone must count as changed even though every other dimension is stable.
    let detail_v2 = r#"{"position":1,"state":"AWAITING_CHECKS","enqueued_at":"2026-07-10T11:54:54Z"}"#;
    let out = db
        .update_task_pr_poll_state(
            &chore_id,
            PrPollStateInput {
                ci_required_state: "success",
                review_required_state: "approved",
                merge_queue_state: Some("queued"),
                merge_queue_detail: Some(detail_v2),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        out.changed,
        "a merge_queue_detail change alone (position/state) must count as changed"
    );

    // Dequeued: merge_queue_state clears and detail clears with it.
    let out = db
        .update_task_pr_poll_state(&chore_id, ci_review_input("success", "approved"))
        .unwrap();
    assert!(
        out.changed,
        "clearing merge_queue_state/detail on dequeue must count as changed"
    );
    let stored: Option<String> = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT merge_queue_detail FROM tasks WHERE id = ?1",
            params![chore_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored, None,
        "merge_queue_detail must clear when the PR leaves the queue"
    );
}

/// A soft-deleted or entirely-absent row is a no-op: `changed = false`,
/// `prior_ci_state = None`, and (for the deleted row) the poll timestamp is not
/// stamped.
#[test]
fn poll_state_deleted_or_absent_row_is_noop() {
    let (db, _p, chore_id) = setup_product_and_chore();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET deleted_at = ?2 WHERE id = ?1",
            params![chore_id, now_string()],
        )
        .unwrap();
    let out = db
        .update_task_pr_poll_state(&chore_id, ci_review_input("success", "approved"))
        .unwrap();
    assert!(!out.changed, "a soft-deleted row must not count as changed");
    assert!(
        out.prior_ci_state.is_none(),
        "a soft-deleted row yields no prior ci state"
    );
    assert!(
        poll_ts(&db, &chore_id).is_none(),
        "a soft-deleted row must not have its poll timestamp stamped"
    );

    let out = db
        .update_task_pr_poll_state("chr_does_not_exist", ci_review_input("success", "approved"))
        .unwrap();
    assert!(!out.changed, "an absent row must not count as changed");
    assert!(out.prior_ci_state.is_none(), "an absent row yields no prior ci state");
}

// ── reopen_comments_for_closed_unmerged_pr ──────────────────────────────────

/// Create an `in_revision` comment claimed by `revise_task_id` (as a `[Revise]`
/// batch would leave it before the PR closed). Returns the comment id.
fn in_revision_comment(db: &WorkDb, artifact_id: &str, revise_task_id: &str, exact: &str) -> String {
    let c = db
        .create_comment(CreateCommentInput {
            artifact_kind: "work_item".to_owned(),
            artifact_id: artifact_id.to_owned(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: exact.to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "please change this".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 1,
        })
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_comments SET status = 'in_revision', revise_task_id = ?2 WHERE id = ?1",
            params![c.id, revise_task_id],
        )
        .unwrap();
    c.id
}

/// When a chore's PR closes unmerged, `in_revision` comments claimed by both
/// the chore itself and by revisions in its chain are reopened
/// (`status = 'active'`, `revise_task_id` cleared); the returned count covers
/// both, and comments that were never `in_revision` are untouched.
#[test]
fn reopen_comments_reopens_task_and_chain_revision_comments() {
    let db = WorkDb::open(temp_db_path("reopen-closed-unmerged")).unwrap();
    let product_id = make_revision_product(&db, "reopen");
    let root_id = make_chore_root(&db, &product_id, "root");
    let revision_id = insert_revision_row(&db, &product_id, &root_id);

    // A comment claimed by the chore directly (plain-chore vehicle) and one
    // claimed by the chain revision (PR-open vehicle).
    let root_cmt = in_revision_comment(&db, &root_id, &root_id, "fix the root behaviour");
    let rev_cmt = in_revision_comment(&db, &root_id, &revision_id, "fix the revision behaviour");

    // A control: an active comment claimed by the chore that was never put into
    // revision — reconciliation must leave it alone.
    let control_cmt = {
        let c = db
            .create_comment(CreateCommentInput {
                artifact_kind: "work_item".to_owned(),
                artifact_id: root_id.clone(),
                doc_version: "v0".to_owned(),
                anchor: CommentAnchor {
                    exact: "an unrelated active note".to_owned(),
                    prefix: String::new(),
                    suffix: String::new(),
                },
                body: "just a note".to_owned(),
                author: "user:test@example.com".to_owned(),
                plain_text_projection_version: 1,
            })
            .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_comments SET revise_task_id = ?2 WHERE id = ?1",
                params![c.id, root_id],
            )
            .unwrap();
        c.id
    };

    let affected = db.reopen_comments_for_closed_unmerged_pr(&root_id).unwrap();
    assert_eq!(
        affected, 2,
        "both the chore-owned and the revision-owned in_revision comments must be reopened"
    );

    let root_comment = db.get_comment(&root_cmt).unwrap().unwrap();
    assert_eq!(root_comment.status, "active", "chore-owned comment must be reopened");
    assert!(
        root_comment.revise_task_id.is_none(),
        "reopened comment must have revise_task_id cleared"
    );

    let rev_comment = db.get_comment(&rev_cmt).unwrap().unwrap();
    assert_eq!(rev_comment.status, "active", "revision-owned comment must be reopened");
    assert!(
        rev_comment.revise_task_id.is_none(),
        "reopened revision comment must have revise_task_id cleared"
    );

    // The already-active control comment is untouched by reconciliation.
    let control = db.get_comment(&control_cmt).unwrap().unwrap();
    assert_eq!(control.status, "active");
    assert_eq!(
        control.revise_task_id.as_deref(),
        Some(root_id.as_str()),
        "a comment that was never in_revision must keep its status and revise_task_id"
    );
}
