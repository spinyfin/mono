//! Tests that drive upper engine services against a real [`WorkDb`].
//!
//! They live here, not under `work/`, because each one calls into a service
//! that sits above storage (the merge poller, completion, the live-worker
//! registry, review-guide validation, the sweeps, the board bridge). Storage's
//! own unit tests stay beside the schema and SQL they pin and may not name any
//! of those services; these tests may not name storage's crate-private items.
//! They go through `WorkDb`'s public operations and seed through
//! [`raw`] where a state is unreachable by API.
//!
//! One module per subject, named for the behaviour under test. When a service
//! is extracted into its own crate, its module moves with it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use boss_protocol::{ProposalKind, ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket, ReviewProfile};
use rusqlite::{Connection, params};

use crate::test_support::create_test_chore_manual;
use crate::work::{ReviewBatchCreateInput, SubmitWorkerProposalInput, SubmitWorkerProposalOutcome, WorkDb};

mod automation_triage_context;
mod board_notification_bridge;
mod persona_live_state;
mod project_postmortem;
mod review_guide_jobs;
mod review_guide_source_capture;
mod review_stage_metadata;
mod stalled_reviewer;
mod sweep_reads;
mod work_item_lifecycle;

/// The PR every review-batch fixture below targets.
const PR_URL: &str = "https://github.com/example/repo/pull/42";

/// Open an independent connection to `db`'s file for explicit seeding.
///
/// The database must be file-backed (use `test_support::open_db`). The
/// connection carries the same durability, busy-timeout and foreign-key
/// settings as `WorkDb`'s own, and each statement runs in autocommit mode so
/// nothing it does can hold a lock across a `WorkDb` call.
fn raw(db: &WorkDb) -> Connection {
    assert!(
        !db.is_in_memory(),
        "explicit seeding needs a file-backed database; open it with test_support::open_db"
    );
    let conn = Connection::open(db.path()).expect("open the explicit seeding connection");
    conn.busy_timeout(Duration::from_secs(5))
        .expect("set the seeding busy timeout");
    conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")
        .expect("apply the seeding pragmas");
    conn
}

/// Unix seconds as the decimal string the durable rows store.
fn now_string() -> String {
    boss_engine_utils::epoch_time::now_epoch_secs().to_string()
}

/// A process-unique id with `prefix`, for rows seeded through [`raw`].
fn next_id(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = boss_engine_utils::epoch_time::now_epoch_nanos();
    format!("{prefix}_{nanos:x}_{:x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// A chore with a bound PR that is waiting in review.
fn in_review_chore(db: &WorkDb, product_id: &str, pr_url: &str) -> String {
    let chore = create_test_chore_manual(db, product_id, "Chore for revision tests");
    raw(db)
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            params![chore.id, pr_url],
        )
        .unwrap();
    chore.id
}

/// Insert a `kind = 'revision'` task row under `parent_task_id`, bypassing the
/// revision gate.
fn insert_revision_row(db: &WorkDb, product_id: &str, parent_task_id: &str) -> String {
    let id = next_id("task");
    raw(db)
        .execute(
            "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
             VALUES (?1, ?2, 'revision', 'Test revision', '', 'todo', ?3, ?3, ?4)",
            params![id, product_id, now_string(), parent_task_id],
        )
        .unwrap();
    id
}

/// `(id, status)` of every execution bound to `work_item_id`, oldest first.
fn executions_for(db: &WorkDb, work_item_id: &str) -> Vec<(String, String)> {
    let conn = raw(db);
    let mut stmt = conn
        .prepare(
            "SELECT id, status FROM work_executions
             WHERE work_item_id = ?1 ORDER BY created_at ASC, id ASC",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![work_item_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

/// The stored status of `task_id`.
fn task_status(db: &WorkDb, task_id: &str) -> String {
    raw(db)
        .query_row("SELECT status FROM tasks WHERE id = ?1", params![task_id], |r| {
            r.get::<_, String>(0)
        })
        .unwrap()
}

/// Put an open-PR review cycle root in `in_review` on [`PR_URL`].
fn bind_open_pr(db: &WorkDb, task_id: &str) {
    raw(db)
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            params![task_id, PR_URL],
        )
        .unwrap();
}

/// Force a batch into `supervising`, as if every leaf had reported.
fn force_batch_supervising(db: &WorkDb, batch_id: &str) {
    raw(db)
        .execute(
            "UPDATE pr_review_batches SET status = 'supervising' WHERE id = ?1",
            params![batch_id],
        )
        .unwrap();
}

fn classification() -> ReviewClassification {
    ReviewClassification::builder()
        .changed_files(vec!["tools/boss/engine/pr-review/src/parsing.rs".to_owned()])
        .complexity_flags(vec![])
        .has_production_code(true)
        .metadata_missing(vec![])
        .production_languages(vec![ReviewLanguageBucket::Rust])
        .profile(ReviewProfile::Light)
        .subsystem_buckets(vec!["tools/boss/engine".to_owned()])
        .additions(12)
        .deletions(3)
        .build()
}

/// A review batch targeting `target_sha` on [`PR_URL`].
fn batch_input(cycle_root_id: String, target_sha: &str, phase: ReviewBatchPhase) -> ReviewBatchCreateInput {
    ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root_id)
        .base_sha("base-sha")
        .classification(classification())
        .phase(phase)
        .pr_number(42)
        .pr_url(PR_URL)
        .target_sha(target_sha)
        .build()
}

/// Submit a clean leaf report for `batch_id` as `execution_id`.
fn submit_review_report(
    db: &WorkDb,
    execution_id: &str,
    work_item_id: &str,
    batch_id: &str,
    target_sha: &str,
    idempotency_key: &str,
) -> SubmitWorkerProposalOutcome {
    db.submit_worker_proposal(SubmitWorkerProposalInput {
        execution_id,
        work_item_id,
        kind: ProposalKind::ReviewReport,
        payload_json: &format!(
            r#"{{"batch_id":"{batch_id}","target_sha":"{target_sha}","report":{{"batch_id":"{batch_id}","pr_url":"{PR_URL}","target_sha":"{target_sha}","phase":"pre_merge","summary":"Clean.","coverage":{{"files_inspected":[],"files_omitted":[],"limitations":[]}},"findings":[]}}}}"#
        ),
        idempotency_key,
    })
    .unwrap()
    .unwrap()
}

/// A supervisor's clean verdict payload for `batch_id`.
fn clean_verdict_payload(batch_id: &str, target_sha: &str) -> String {
    format!(
        r#"{{"batch_id":"{batch_id}","verdict":{{"batch_id":"{batch_id}","pr_url":"{PR_URL}","target_sha":"{target_sha}","phase":"pre_merge","summary":"Clean.","revision_warranted":false,"findings":[],"contradictions":[]}}}}"#
    )
}
