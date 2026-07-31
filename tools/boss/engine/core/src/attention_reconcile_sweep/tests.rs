//! Behavioural tests for the generic attention/banner reconciler.
//!
//! The two halves of the contract each get explicit coverage, because the
//! easy way to "fix" a stale banner is to make failures disappear, and that
//! is precisely the wrong fix:
//!
//! - a signal whose clearing evidence arrived is lowered, and
//! - a signal whose condition is still live keeps showing, indefinitely.

use super::*;
use crate::attention_lifecycle::{ClearedBy, lifecycle_for};
use crate::test_support::*;
use crate::work::WorkDb;
use boss_protocol::{CreateExecutionInput, ExecutionKind, ExecutionStatus};

/// Insert an `open` attention row directly, with an explicit `created_at`.
///
/// Tests must be able to place a signal before or after its evidence, which
/// the production filers (which always stamp "now") cannot express.
fn open_attention_for_work_item(db: &WorkDb, work_item_id: &str, kind: &str, created_at: i64) -> String {
    let id = format!("attn_test_{kind}_{created_at}_{work_item_id}");
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO work_attention_items
             (id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at)
         VALUES (?1, NULL, ?2, ?3, 'open', 'test', 'test', ?4, NULL)",
        rusqlite::params![id, work_item_id, kind, created_at.to_string()],
    )
    .unwrap();
    id
}

/// The execution-scoped counterpart of [`open_attention_for_work_item`] —
/// `work_item_id` NULL, `execution_id` set. Half the kinds in the registry
/// are filed this way (the completion handlers), and a reconciler that only
/// understood the work-item scope would leak every one of them.
fn open_attention_for_execution(db: &WorkDb, execution_id: &str, kind: &str, created_at: i64) -> String {
    let id = format!("attn_test_exec_{kind}_{created_at}");
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO work_attention_items
             (id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at)
         VALUES (?1, ?2, NULL, ?3, 'open', 'test', 'test', ?4, NULL)",
        rusqlite::params![id, execution_id, kind, created_at.to_string()],
    )
    .unwrap();
    id
}

fn attention_status(db: &WorkDb, id: &str) -> (String, Option<String>) {
    let conn = db.connect().unwrap();
    conn.query_row(
        "SELECT status, resolved_at FROM work_attention_items WHERE id = ?1",
        [id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
    )
    .unwrap()
}

/// Stamp a `work_runs` row's `started_at`, standing in for a run that began
/// at a specific moment relative to a signal.
fn force_run_started_at(db: &WorkDb, execution_id: &str, epoch_secs: i64) {
    let conn = db.connect().unwrap();
    let updated = conn
        .execute(
            "UPDATE work_runs SET started_at = ?2 WHERE execution_id = ?1",
            rusqlite::params![execution_id, epoch_secs.to_string()],
        )
        .unwrap();
    assert!(updated > 0, "expected a run row for {execution_id}");
}

/// Drive a work item through the real start path so a `work_runs` row exists
/// with the production shape, then pin its `started_at`.
///
/// Deliberately goes through `create_execution` rather than
/// `request_execution`: the latter is one of the *dispatch* entry points and
/// already resolves `churn_guard_parked` and the dispatch-failure columns
/// inline, which would clear the very signals these tests are about before
/// the sweep ever ran. The state under test is a run that started by some
/// other path — which is exactly the class of start the old code never
/// noticed.
fn start_run_at(db: &WorkDb, work_item_id: &str, started_at: i64) -> String {
    let execution = create_ready_chore_execution(db, work_item_id.to_owned());
    db.start_execution_run(&execution.id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    force_run_started_at(db, &execution.id, started_at);
    execution.id
}

fn completed_execution_at(db: &WorkDb, work_item_id: &str, kind: ExecutionKind, finished_at: i64) -> String {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(work_item_id.to_owned())
                .kind(kind)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'completed', finished_at = ?2 WHERE id = ?1",
        rusqlite::params![execution.id, finished_at.to_string()],
    )
    .unwrap();
    execution.id
}

/// Stamp the three dispatch-failure columns directly, with an explicit
/// `dispatch_failed_at`.
///
/// Tests need to place the stamp before or after a run start, which
/// `bounce_dispatch_failed_to_backlog` (always "now") cannot express. Kept as
/// its own non-async fn so the connection guard is never in scope across an
/// `.await` in the test bodies.
fn force_dispatch_failure(db: &WorkDb, work_item_id: &str, reason: &str, error: &str, failed_at: i64) {
    let conn = db.connect().unwrap();
    let updated = conn
        .execute(
            "UPDATE tasks
             SET dispatch_failed_reason = ?2,
                 dispatch_failed_error  = ?3,
                 dispatch_failed_at     = ?4
             WHERE id = ?1",
            rusqlite::params![work_item_id, reason, error, failed_at.to_string()],
        )
        .unwrap();
    assert_eq!(updated, 1, "expected a task row for {work_item_id}");
}

fn dispatch_failure_columns(db: &WorkDb, work_item_id: &str) -> (Option<String>, Option<String>, Option<String>) {
    let conn = db.connect().unwrap();
    conn.query_row(
        "SELECT dispatch_failed_reason, dispatch_failed_error, dispatch_failed_at FROM tasks WHERE id = ?1",
        [work_item_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

// ── The reported symptom: the stale "Failed to start" banner ───────────────

/// The exact shape from the incident: dispatch failed and stamped the card,
/// the item then ran successfully, and the banner survived it. The reconciler
/// clears the columns the banner reads from.
///
/// The stamp is written directly rather than via
/// `bounce_dispatch_failed_to_backlog` because the *live* fix already clears
/// it inside `start_execution_run` — the state under test here is the one
/// only the backstop can reach: a row stamped by an engine process that
/// predates that inline clear, whose recovery run has already happened.
#[tokio::test]
async fn clears_a_dispatch_failure_banner_once_a_run_has_started_since() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Stale banner");

    start_run_at(&db, &chore.id, 2000);
    force_dispatch_failure(
        &db,
        &chore.id,
        "cube_workspace_lease_failed",
        "cube: disk pressure: 34.2 GiB free",
        1000,
    );

    let outcome = run_one_pass(&db).await;
    assert_eq!(outcome.dispatch_banners_cleared, 1);
    assert_eq!(dispatch_failure_columns(&db, &chore.id), (None, None, None));
}

/// The boundary. A work item whose dispatch is still failing has no later
/// run, so nothing clears — the banner must keep showing for as long as the
/// condition holds, with no timer and no lane-based expiry behind it.
#[tokio::test]
async fn keeps_a_dispatch_failure_banner_when_nothing_has_started_since() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Still broken");

    // A run that started BEFORE the failure — the item used to work, and
    // that history says nothing about the current dispatch failure.
    start_run_at(&db, &chore.id, 1000);
    assert!(
        db.bounce_dispatch_failed_to_backlog(&chore.id, "cube_workspace_lease_failed", "lease refused")
            .unwrap(),
        "the bounce must stamp the row for this test to mean anything",
    );
    force_dispatch_failure(&db, &chore.id, "cube_workspace_lease_failed", "lease refused", 5000);

    let outcome = run_one_pass(&db).await;
    assert_eq!(outcome.dispatch_banners_cleared, 0);
    let (reason, error, at) = dispatch_failure_columns(&db, &chore.id);
    assert_eq!(reason.as_deref(), Some("cube_workspace_lease_failed"));
    assert_eq!(error.as_deref(), Some("lease refused"));
    assert_eq!(at.as_deref(), Some("5000"));
}

/// The live path: starting a run clears the stamp in the same transaction,
/// without waiting for the periodic sweep. This is what makes the banner come
/// down promptly for every dispatch caller, not just the one that used to
/// clear it.
#[test]
fn starting_a_run_clears_the_dispatch_failure_banner_inline() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Recovers");

    db.bounce_dispatch_failed_to_backlog(&chore.id, "cube_workspace_lease_failed", "lease refused")
        .unwrap();
    assert!(dispatch_failure_columns(&db, &chore.id).0.is_some());

    let execution_id = create_old_execution(&db, &chore.id);
    db.start_execution_run(&execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();

    assert_eq!(
        dispatch_failure_columns(&db, &chore.id),
        (None, None, None),
        "a started run refutes `dispatch_failed_reason` and must clear it without a sweep pass",
    );
}

// ── The general rule, per ClearedBy variant ────────────────────────────────

#[tokio::test]
async fn work_resumed_kinds_resolve_once_a_later_run_starts() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Parked then resumed");

    let attention = open_attention_for_work_item(&db, &chore.id, crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND, 1000);
    start_run_at(&db, &chore.id, 1500);

    let outcome = run_one_pass(&db).await;
    assert_eq!(outcome.attentions_resolved, 1);
    let (status, resolved_at) = attention_status(&db, &attention);
    assert_eq!(status, "resolved");
    assert!(
        resolved_at.is_some(),
        "resolution must stamp `resolved_at` — the row stays inspectable, it is not deleted",
    );
}

#[tokio::test]
async fn work_resumed_kinds_stay_open_when_the_only_run_predates_the_signal() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Still parked");

    start_run_at(&db, &chore.id, 1000);
    let attention = open_attention_for_work_item(&db, &chore.id, crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND, 5000);

    let outcome = run_one_pass(&db).await;
    assert_eq!(outcome.attentions_resolved, 0);
    assert_eq!(attention_status(&db, &attention).0, "open");
}

/// Execution-scoped rows (`work_item_id` NULL) must reconcile identically —
/// they are how the completion handlers file, and skipping them would leave
/// most of the automatic kinds leaking exactly as before.
#[tokio::test]
async fn execution_scoped_attentions_resolve_through_their_execution_s_work_item() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Execution scoped");

    let dead = create_ready_chore_execution(&db, chore.id.clone());
    let attention = open_attention_for_execution(&db, &dead.id, crate::completion::NUDGE_BREAKER_ATTENTION_KIND, 1000);
    start_run_at(&db, &chore.id, 1500);

    let outcome = run_one_pass(&db).await;
    assert_eq!(outcome.attentions_resolved, 1);
    assert_eq!(attention_status(&db, &attention).0, "resolved");
}

#[tokio::test]
async fn execution_kind_completed_resolves_only_for_the_declared_kind() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Dead review");

    let attention = open_attention_for_work_item(
        &db,
        &chore.id,
        crate::pr_review_recovery::PR_REVIEW_DIED_ATTENTION_KIND,
        1000,
    );

    // A completed implementation pass is NOT evidence about the review — a
    // churned retry must never mask a still-dead reviewer.
    completed_execution_at(&db, &chore.id, ExecutionKind::ChoreImplementation, 2000);
    let outcome = run_one_pass(&db).await;
    assert_eq!(outcome.attentions_resolved, 0);
    assert_eq!(attention_status(&db, &attention).0, "open");

    // A completed review of the right kind is.
    completed_execution_at(&db, &chore.id, ExecutionKind::PrReview, 3000);
    let outcome = run_one_pass(&db).await;
    assert_eq!(outcome.attentions_resolved, 1);
    assert_eq!(attention_status(&db, &attention).0, "resolved");
}

/// The other half of the boundary, on the attention side: a kind the registry
/// declares as human-owned is never touched, however much later activity the
/// item sees. `worker_recovery_exhausted` is the sharpest case — it is a
/// dispatch *gate* that `list_orphan_active_candidates` consults, so an
/// automatic clear would silently re-enable redispatch for an item a human
/// was asked to look at.
#[tokio::test]
async fn human_decision_kinds_are_never_auto_resolved() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Escalated");

    let attention = open_attention_for_work_item(&db, &chore.id, crate::work::ATTENTION_KIND_RECOVERY_EXHAUSTED, 1000);
    start_run_at(&db, &chore.id, 9000);
    completed_execution_at(&db, &chore.id, ExecutionKind::PrReview, 9500);

    let outcome = run_one_pass(&db).await;
    assert_eq!(outcome.attentions_resolved, 0);
    assert_eq!(attention_status(&db, &attention).0, "open");
}

/// Producer-owned kinds are equally untouched: the producer's direct read of
/// its own condition beats any evidence proxy this sweep could apply.
#[tokio::test]
async fn producer_owned_kinds_are_left_to_their_producer() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Abandoned branch");

    let attention = open_attention_for_work_item(
        &db,
        &chore.id,
        crate::abandoned_branch_pr_sweep::ATTENTION_KIND_ABANDONED_BRANCH_NO_PR,
        1000,
    );
    start_run_at(&db, &chore.id, 9000);

    let outcome = run_one_pass(&db).await;
    assert_eq!(outcome.attentions_resolved, 0);
    assert_eq!(attention_status(&db, &attention).0, "open");
    assert_eq!(
        lifecycle_for(crate::abandoned_branch_pr_sweep::ATTENTION_KIND_ABANDONED_BRANCH_NO_PR)
            .unwrap()
            .cleared_by,
        ClearedBy::ProducerReconciles,
    );
}

/// A signal that is re-raised after being resolved must come back. Resolution
/// is not a suppression list — the reconciler only ever acts on the `open`
/// row in front of it.
#[tokio::test]
async fn a_condition_that_re_trips_after_resolution_shows_again() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Flaps");

    let first = open_attention_for_work_item(&db, &chore.id, crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND, 1000);
    start_run_at(&db, &chore.id, 1500);
    assert_eq!(run_one_pass(&db).await.attentions_resolved, 1);
    assert_eq!(attention_status(&db, &first).0, "resolved");

    // The guard trips again, after the run that cleared the first one.
    let second = open_attention_for_work_item(&db, &chore.id, crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND, 9000);
    assert_eq!(run_one_pass(&db).await.attentions_resolved, 0);
    assert_eq!(attention_status(&db, &second).0, "open");
}

#[tokio::test]
async fn a_pass_with_nothing_to_do_reports_no_activity() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    create_test_chore(&db, product.id, "Quiet");

    let outcome = run_one_pass(&db).await;
    assert!(outcome.is_empty());
    assert_eq!(outcome, AttentionReconcileOutcome::default());
}

/// Re-running the pass over already-reconciled state must be a no-op, since
/// it runs every five minutes forever.
#[tokio::test]
async fn the_pass_is_idempotent() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "Idempotent");

    open_attention_for_work_item(&db, &chore.id, crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND, 1000);
    start_run_at(&db, &chore.id, 1500);

    assert_eq!(run_one_pass(&db).await.attentions_resolved, 1);
    assert!(run_one_pass(&db).await.is_empty());
}
