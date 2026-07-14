//! Behavioural coverage for the core PR-lifecycle state transitions in
//! `pr_flow.rs`, asserting observable outcomes (return values + re-queried
//! entity state) rather than internal SQL:
//!
//!   - `mark_chore_pr_merged`: the `Some(Task)` merged-transition contract
//!     (status → `done`, `pr_url` bound) and the `None` no-op when there is
//!     no eligible PR (already-terminal / missing row).
//!   - `mark_chore_pr_closed_unmerged`: the closed-unmerged terminal, and how
//!     it is *distinguished* from the merged path — it does not resolve the
//!     PR's `in_revision` comments the way a merge does.
//!   - `record_worker_pr_completion` vs `record_worker_no_op_completion`: the
//!     PR-produced path binds `pr_url` and advances to `in_review`; the no-op
//!     path closes to `done` with no `pr_url`. Plus the terminal-execution
//!     idempotency no-op.
//!   - `update_task_pr_poll_state`: the persisted CI/review columns reflect
//!     the input across a couple of representative state changes (complements
//!     the return-value-focused assertions in `t14`).
//!
//! The `update_task_pr_poll_state` return-value semantics, the
//! `reopen_comments_for_closed_unmerged_pr` fan-out, and the `completed_at`
//! semantics of the completion helpers are already covered in `t14` / `t10`;
//! this module deliberately covers the transitions those files do not, rather
//! than duplicating them.

use super::*;

// ── mark_chore_pr_merged: core Some/None contract ───────────────────────────

/// The merge path: an `in_review` chore with a bound PR advances to `done`
/// and returns `Some(Task)` carrying the merged status and the PR url.
#[test]
fn mark_chore_pr_merged_advances_in_review_to_done_and_binds_pr() {
    let db = WorkDb::open(temp_db_path("merged-advance-done")).unwrap();
    let product_id = make_revision_product(&db, "merged-advance");
    let pr_url = "https://github.com/spinyfin/mono/pull/4001";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let returned = db.mark_chore_pr_merged(&chore_id, pr_url).unwrap();
    let returned = returned.expect("a mergeable in_review chore must return Some(Task)");
    assert_eq!(
        returned.status,
        TaskStatus::Done,
        "the returned task must have advanced to the merged/terminal status",
    );
    assert_eq!(
        returned.pr_url.as_deref(),
        Some(pr_url),
        "the returned task must carry the bound PR url",
    );

    // Re-query to confirm the transition is durable, not just reflected in the
    // returned value.
    assert_eq!(task_status(&db, &chore_id), "done");
    let WorkItem::Chore(requeried) = db.get_work_item(&chore_id).unwrap() else {
        panic!("expected a chore");
    };
    assert_eq!(requeried.pr_url.as_deref(), Some(pr_url));
}

/// No eligible PR to act on: a chore already in a terminal status (its PR was
/// merged on a prior pass) is a no-op — `mark_chore_pr_merged` returns `None`
/// and leaves the row untouched. This is what makes the merge poller safe to
/// re-run against a late-arriving merge event.
#[test]
fn mark_chore_pr_merged_noop_when_already_done() {
    let db = WorkDb::open(temp_db_path("merged-noop-done")).unwrap();
    let product_id = make_revision_product(&db, "merged-noop");
    let pr_url = "https://github.com/spinyfin/mono/pull/4002";
    let chore_id = make_done_chore(&db, &product_id, pr_url);

    let returned = db.mark_chore_pr_merged(&chore_id, pr_url).unwrap();
    assert!(
        returned.is_none(),
        "an already-done chore has no eligible PR transition and must return None",
    );
    assert_eq!(task_status(&db, &chore_id), "done", "the row must be untouched");
}

/// A missing work item is a no-op, not an error — the poller may race a delete.
#[test]
fn mark_chore_pr_merged_noop_when_task_missing() {
    let db = WorkDb::open(temp_db_path("merged-noop-missing")).unwrap();
    let returned = db
        .mark_chore_pr_merged("chr_does_not_exist", "https://github.com/spinyfin/mono/pull/4003")
        .unwrap();
    assert!(returned.is_none(), "an unknown work item must return None");
}

// ── mark_chore_pr_closed_unmerged: terminal + distinction from merged ───────

/// The close-unmerged path: an `in_review` chore whose PR was closed without
/// merging advances to `done` and returns `Some(Task)`.
#[test]
fn mark_chore_pr_closed_unmerged_advances_in_review_to_done() {
    let db = WorkDb::open(temp_db_path("closed-advance-done")).unwrap();
    let product_id = make_revision_product(&db, "closed-advance");
    let pr_url = "https://github.com/spinyfin/mono/pull/4010";
    let chore_id = make_in_review_chore(&db, &product_id, pr_url);

    let returned = db.mark_chore_pr_closed_unmerged(&chore_id, pr_url).unwrap();
    let returned = returned.expect("an in_review chore with a matching PR must return Some(Task)");
    assert_eq!(
        returned.status,
        TaskStatus::Done,
        "a closed-unmerged PR moves the chore to the closed-unmerged (done) outcome",
    );
    assert_eq!(task_status(&db, &chore_id), "done", "the transition must be durable");
}

/// Idempotent for a late-arriving close event: a chore already terminal is a
/// no-op returning `None`.
#[test]
fn mark_chore_pr_closed_unmerged_noop_when_already_terminal() {
    let db = WorkDb::open(temp_db_path("closed-noop-done")).unwrap();
    let product_id = make_revision_product(&db, "closed-noop");
    let pr_url = "https://github.com/spinyfin/mono/pull/4011";
    let chore_id = make_done_chore(&db, &product_id, pr_url);

    let returned = db.mark_chore_pr_closed_unmerged(&chore_id, pr_url).unwrap();
    assert!(
        returned.is_none(),
        "an already-terminal chore must return None on a close-unmerged event",
    );
    assert_eq!(task_status(&db, &chore_id), "done");
}

/// Create an `in_revision` comment on `artifact_id` claimed by `revise_task_id`
/// (the state a dispatched `[Revise]` batch leaves behind). Returns its id.
/// Mirrors the small helper `t14` uses for the reopen path.
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
            body: "please revise".to_owned(),
            author: "human".to_owned(),
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

/// The observable difference between the two terminal paths at the comment
/// layer: a merge *resolves* the PR's `in_revision` comments (the change
/// shipped), whereas a close-unmerged deliberately leaves them `in_revision`
/// (they are reopened separately by `reopen_comments_for_closed_unmerged_pr`,
/// so this path must not pre-resolve them).
#[test]
fn merged_resolves_comments_while_closed_unmerged_leaves_them_in_revision() {
    let db = WorkDb::open(temp_db_path("merged-vs-closed-comments")).unwrap();
    // Distinct products so the two chores don't trip the duplicate-name guard
    // (`make_in_review_chore` uses a fixed chore name).
    let merged_product = make_revision_product(&db, "merged-vs-closed-a");
    let closed_product = make_revision_product(&db, "merged-vs-closed-b");

    // Chore A: PR merges → its in_revision comment must be resolved.
    let merged_pr = "https://github.com/spinyfin/mono/pull/4020";
    let merged_id = make_in_review_chore(&db, &merged_product, merged_pr);
    let merged_cmt = in_revision_comment(&db, &merged_id, &merged_id, "shipped behaviour");

    // Chore B: PR closes unmerged → its in_revision comment must be untouched.
    let closed_pr = "https://github.com/spinyfin/mono/pull/4021";
    let closed_id = make_in_review_chore(&db, &closed_product, closed_pr);
    let closed_cmt = in_revision_comment(&db, &closed_id, &closed_id, "never shipped");

    db.mark_chore_pr_merged(&merged_id, merged_pr).unwrap();
    db.mark_chore_pr_closed_unmerged(&closed_id, closed_pr).unwrap();

    let merged_after = db.get_comment(&merged_cmt).unwrap().unwrap();
    assert_eq!(
        merged_after.status, "resolved",
        "a merged PR must resolve its in_revision comments (the change rode the PR)",
    );

    let closed_after = db.get_comment(&closed_cmt).unwrap().unwrap();
    assert_eq!(
        closed_after.status, "in_revision",
        "a closed-unmerged PR must NOT resolve its comments — reopen handles them separately",
    );
    assert_eq!(
        closed_after.revise_task_id.as_deref(),
        Some(closed_id.as_str()),
        "the closed-unmerged comment must retain its revise_task_id (not yet reopened)",
    );
}

// ── record_worker_pr_completion vs record_worker_no_op_completion ────────────

/// The PR-produced path: with `target = InReview`, the task advances to
/// `in_review` with `pr_url` bound, and the execution finalises to `completed`
/// with `pr_url` bound and its cube lease cleared.
#[test]
fn record_worker_pr_completion_binds_pr_and_advances_to_in_review() {
    let db = WorkDb::open(temp_db_path("rwpc-inreview-bind")).unwrap();
    let (_product_id, chore_id, exec_id) = make_waiting_human_chore(&db, "rwpc-inreview");
    let pr_url = "https://github.com/spinyfin/mono/pull/4030";

    let completion = db
        .record_worker_pr_completion(&exec_id, pr_url, None, WorkerPrCompletionTarget::InReview)
        .unwrap()
        .expect("a live execution must return Some(WorkerPrCompletion)");

    // Returned values.
    let WorkItem::Chore(task) = &completion.work_item else {
        panic!("expected a chore work item");
    };
    assert_eq!(task.status, TaskStatus::InReview, "the task must advance to in_review");
    assert_eq!(task.pr_url.as_deref(), Some(pr_url), "the task must bind the PR url");
    assert_eq!(
        completion.execution.status,
        ExecutionStatus::Completed,
        "the execution must finalise to completed",
    );
    assert_eq!(
        completion.execution.pr_url.as_deref(),
        Some(pr_url),
        "the execution row must record the PR url",
    );
    assert!(
        completion.execution.cube_lease_id.is_none(),
        "the cube lease must be released on completion",
    );

    // Re-queried state confirms durability.
    assert_eq!(task_status(&db, &chore_id), "in_review");
}

/// The no-op path: `record_worker_no_op_completion` closes the task to `done`
/// with **no** `pr_url` (the worker verified the work was already on `main`
/// and correctly refused to fabricate a PR), and finalises the execution.
#[test]
fn record_worker_no_op_completion_closes_done_without_pr() {
    let db = WorkDb::open(temp_db_path("rwnc-noop-done")).unwrap();
    let (_product_id, chore_id, exec_id) = make_waiting_human_chore(&db, "rwnc-noop");

    let completion = db
        .record_worker_no_op_completion(&exec_id, "already present on main")
        .unwrap()
        .expect("a live execution must return Some(WorkerPrCompletion)");

    let WorkItem::Chore(task) = &completion.work_item else {
        panic!("expected a chore work item");
    };
    assert_eq!(task.status, TaskStatus::Done, "the no-op path closes the task to done");
    assert!(
        task.pr_url.is_none(),
        "a no-op completion must NOT bind a pr_url — there is no PR",
    );
    assert_eq!(
        completion.execution.status,
        ExecutionStatus::Completed,
        "the execution must finalise to completed",
    );
    assert!(
        completion.execution.pr_url.is_none(),
        "the execution must record no pr_url for a no-op completion",
    );

    assert_eq!(task_status(&db, &chore_id), "done");
}

/// Head-to-head: the two completion signals differ in exactly the way the
/// engine relies on — the PR path lands in `in_review` *with* a `pr_url`, the
/// no-op path lands in `done` *without* one. Running both on sibling
/// executions makes the divergence explicit.
#[test]
fn pr_completion_and_no_op_completion_differ_in_status_and_pr_binding() {
    let db = WorkDb::open(temp_db_path("rwpc-vs-noop")).unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/4040";

    let (_pa, pr_chore, pr_exec) = make_waiting_human_chore(&db, "pr-path");
    let (_pb, noop_chore, noop_exec) = make_waiting_human_chore(&db, "noop-path");

    let pr_done = db
        .record_worker_pr_completion(&pr_exec, pr_url, None, WorkerPrCompletionTarget::InReview)
        .unwrap()
        .unwrap();
    let noop_done = db
        .record_worker_no_op_completion(&noop_exec, "already done")
        .unwrap()
        .unwrap();

    // Task status diverges: in_review (PR) vs done (no-op).
    assert_eq!(task_status(&db, &pr_chore), "in_review");
    assert_eq!(task_status(&db, &noop_chore), "done");

    // PR binding diverges: bound (PR) vs unbound (no-op).
    let pr_url_bound = matches!(&pr_done.work_item, WorkItem::Chore(t) if t.pr_url.as_deref() == Some(pr_url));
    let noop_url_unbound = matches!(&noop_done.work_item, WorkItem::Chore(t) if t.pr_url.is_none());
    assert!(pr_url_bound, "PR completion must bind the pr_url");
    assert!(noop_url_unbound, "no-op completion must leave the pr_url unbound");

    // Both finalise their execution.
    assert_eq!(pr_done.execution.status, ExecutionStatus::Completed);
    assert_eq!(noop_done.execution.status, ExecutionStatus::Completed);
}

/// Idempotency: a second `record_worker_pr_completion` on an
/// already-finalised execution is a no-op returning `None` — safe against a
/// Stop hook that fires more than once.
#[test]
fn record_worker_pr_completion_noop_when_execution_terminal() {
    let db = WorkDb::open(temp_db_path("rwpc-terminal-noop")).unwrap();
    let (_product_id, _chore_id, exec_id) = make_waiting_human_chore(&db, "rwpc-terminal");
    let pr_url = "https://github.com/spinyfin/mono/pull/4050";

    // First call finalises the execution.
    db.record_worker_pr_completion(&exec_id, pr_url, None, WorkerPrCompletionTarget::InReview)
        .unwrap()
        .expect("first completion must return Some");

    // Second call sees a terminal execution and no-ops.
    let repeat = db
        .record_worker_pr_completion(&exec_id, pr_url, None, WorkerPrCompletionTarget::InReview)
        .unwrap();
    assert!(
        repeat.is_none(),
        "a second completion on an already-finalised execution must return None",
    );
}

// ── update_task_pr_poll_state: persisted-column reflection ──────────────────

/// Complements `t14` (which asserts the returned `PrPollStateOutcome`): here we
/// assert the *persisted* `ci_required_state` / `review_required_state` columns
/// track the input across two representative probes, and that `changed`
/// reflects whether the state actually moved.
#[test]
fn poll_state_persists_ci_and_review_columns_across_changes() {
    let (db, _p, chore_id) = setup_product_and_chore();

    let read_states = |db: &WorkDb| -> (Option<String>, Option<String>) {
        db.connect()
            .unwrap()
            .query_row(
                "SELECT ci_required_state, review_required_state FROM tasks WHERE id = ?1",
                params![chore_id],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .unwrap()
    };

    // Probe 1: pending CI, pending review.
    let out = db
        .update_task_pr_poll_state(
            &chore_id,
            PrPollStateInput {
                ci_required_state: "pending",
                review_required_state: "pending",
                ..Default::default()
            },
        )
        .unwrap();
    assert!(out.changed, "first probe (NULL → set) must count as changed");
    assert_eq!(
        read_states(&db),
        (Some("pending".to_owned()), Some("pending".to_owned())),
        "the persisted columns must reflect the first probe's input",
    );

    // Probe 2: CI recovered, review approved — both dimensions move.
    let out = db
        .update_task_pr_poll_state(
            &chore_id,
            PrPollStateInput {
                ci_required_state: "success",
                review_required_state: "approved",
                ..Default::default()
            },
        )
        .unwrap();
    assert!(out.changed, "a CI+review state change must count as changed");
    assert_eq!(
        out.prior_ci_state.as_deref(),
        Some("pending"),
        "prior_ci_state must expose the value stored before this probe",
    );
    assert_eq!(
        read_states(&db),
        (Some("success".to_owned()), Some("approved".to_owned())),
        "the persisted columns must reflect the second probe's input",
    );

    // Probe 3: identical to probe 2 — nothing moves, columns unchanged.
    let out = db
        .update_task_pr_poll_state(
            &chore_id,
            PrPollStateInput {
                ci_required_state: "success",
                review_required_state: "approved",
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!out.changed, "an identical probe must not count as changed");
    assert_eq!(
        read_states(&db),
        (Some("success".to_owned()), Some("approved".to_owned())),
        "an unchanged probe must leave the persisted columns intact",
    );
}
