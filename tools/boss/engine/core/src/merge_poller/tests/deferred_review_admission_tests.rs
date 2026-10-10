//! `sweep_deferred_review_admission` against `in_review` cycle roots whose
//! newest pre-merge batch left the current head unreviewed.

use super::*;
use crate::completion::ReviewBatchEnqueuer;
use crate::work::{ReviewBatchCreateInput, ReviewBatchDispatch};

const PR_URL: &str = "https://github.com/example/repo/pull/42";

/// Stub for the `gh pr view` metadata fetch: admits a batch at a fixed head.
struct FixedHeadEnqueuer {
    head: &'static str,
}

#[async_trait]
impl ReviewBatchEnqueuer for FixedHeadEnqueuer {
    async fn enqueue(
        &self,
        work_db: &WorkDb,
        work_item_id: &str,
        repo_remote_url: &str,
        pr_url: &str,
        review_pool_size: usize,
    ) -> Result<ReviewBatchDispatch> {
        let input = batch_input(work_db.review_cycle_root_id(work_item_id), pr_url, self.head);
        work_db.create_pre_merge_review_batch_for_pool(input, repo_remote_url, review_pool_size)
    }
}

fn batch_input(root_id: String, pr_url: &str, head: &str) -> ReviewBatchCreateInput {
    ReviewBatchCreateInput::builder()
        .cycle_root_id(root_id)
        .base_sha("base-sha")
        .classification(
            boss_protocol::ReviewClassification::builder()
                .changed_files(vec!["src/lib.rs".to_owned()])
                .complexity_flags(vec![])
                .has_production_code(true)
                .metadata_missing(vec![])
                .production_languages(vec![boss_protocol::ReviewLanguageBucket::Rust])
                .profile(boss_protocol::ReviewProfile::Light)
                .subsystem_buckets(vec!["src".to_owned()])
                .build(),
        )
        .phase(boss_protocol::ReviewBatchPhase::PreMerge)
        .pr_number(42)
        .pr_url(pr_url)
        .target_sha(head)
        .build()
}

fn in_review_root(db: &WorkDb) -> String {
    let product = create_test_product(db);
    let root = create_test_chore_manual(db, product.id, "review target");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![root.id, PR_URL],
        )
        .unwrap();
    root.id
}

/// Create a batch at `head` and settle all its leaf executions as terminal.
fn settled_batch(
    db: &WorkDb,
    root_id: &str,
    head: &str,
) -> (boss_protocol::ReviewBatch, Vec<crate::work::WorkExecution>) {
    match db
        .create_pre_merge_review_batch(
            batch_input(root_id.to_owned(), PR_URL, head),
            "https://github.com/example/repo",
        )
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch, executions } => {
            for execution in &executions {
                db.mark_execution_redundant(&execution.id).unwrap();
            }
            (batch, executions)
        }
        other => panic!("expected a new batch, got {other:?}"),
    }
}

fn batches_for(db: &WorkDb, root_id: &str) -> Vec<(i64, String, String)> {
    let conn = db.connect().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT generation, target_sha, status FROM pr_review_batches
             WHERE cycle_root_id = ?1 ORDER BY created_at, generation, id",
        )
        .unwrap();
    stmt.query_map([root_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

async fn sweep(db: &WorkDb, publisher: &RecordingPublisher, head: &'static str) -> SweepOutcome {
    let mut outcome = SweepOutcome::default();
    super::super::deferred_review_admission::sweep_deferred_review_admission_with(
        db,
        publisher,
        &FixedHeadEnqueuer { head },
        None,
        16,
        &mut outcome,
    )
    .await;
    outcome
}

/// An `in_review` root whose newest batch was reaped gets generation 2 at
/// the same head, and the recovery is counted and published.
#[tokio::test]
async fn sweep_remints_a_reaped_batch_for_an_in_review_root() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let root_id = in_review_root(&db);
    let (batch, _) = settled_batch(&db, &root_id, "head-a");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE pr_review_batch_members SET status = 'failed' WHERE batch_id = ?1",
            rusqlite::params![batch.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE pr_review_batches SET status = 'failed' WHERE id = ?1",
            rusqlite::params![batch.id],
        )
        .unwrap();
    }
    let publisher = RecordingPublisher::default();

    let outcome = sweep(&db, &publisher, "head-a").await;

    assert_eq!(outcome.review_admission_recovered, 1);
    assert_eq!(
        batches_for(&db, &root_id),
        vec![
            (1, "head-a".to_owned(), "failed".to_owned()),
            (2, "head-a".to_owned(), "collecting".to_owned()),
        ]
    );
    assert!(
        publisher
            .lifecycle_reasons()
            .await
            .contains(&"review_admission_recovered".to_owned())
    );
}

/// An `in_review` root whose newest batch completed as `stale_head` gets a
/// generation-1 batch at the head the PR has now.
#[tokio::test]
async fn sweep_admits_the_new_head_after_a_stale_head_verdict() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let root_id = in_review_root(&db);
    let (batch, executions) = settled_batch(&db, &root_id, "head-a");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE pr_review_batches SET status = 'completed' WHERE id = ?1",
            rusqlite::params![batch.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pr_review_verdicts (
                id, execution_id, work_item_id, head_sha, findings_count,
                revision_warranted, gate_outcome, revision_task_id, created_at, batch_id, proposal_id
             ) VALUES ('rvv_stale', ?1, ?2, 'head-a', 2, 1, ?3, NULL, '1', ?4, 'prop_stale')",
            rusqlite::params![
                executions[0].id,
                root_id,
                crate::work::REVIEW_GATE_OUTCOME_STALE_HEAD,
                batch.id
            ],
        )
        .unwrap();
    }
    let publisher = RecordingPublisher::default();

    let outcome = sweep(&db, &publisher, "head-b").await;

    assert_eq!(outcome.review_admission_recovered, 1);
    assert_eq!(
        batches_for(&db, &root_id),
        vec![
            (1, "head-a".to_owned(), "completed".to_owned()),
            (1, "head-b".to_owned(), "collecting".to_owned()),
        ]
    );

    // The replacement batch switches the arm off: a second pass is a no-op.
    let again = sweep(&db, &publisher, "head-b").await;
    assert_eq!(again.review_admission_recovered, 0);
}
