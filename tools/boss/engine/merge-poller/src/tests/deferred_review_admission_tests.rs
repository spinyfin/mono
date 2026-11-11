//! `sweep_deferred_review_admission` against `in_review` cycle roots whose
//! newest pre-merge batch left the current head unreviewed.

use super::*;
use crate::work::{ReviewBatchCreateInput, ReviewBatchDispatch};
use boss_observation_contracts::ReviewBatchEnqueuer;

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
        // As the production enqueuer builds it: keyed on the cycle root, and
        // naming the candidate when that is a revision of the root.
        let root_id = work_db.review_cycle_root_id(work_item_id);
        let mut input = batch_input(root_id.clone(), pr_url, self.head);
        input.legacy_task_id = (work_item_id != root_id).then(|| work_item_id.to_owned());
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

/// Enqueuer that fails the test if the sweep reaches it.
struct PanicEnqueuer;

#[async_trait]
impl ReviewBatchEnqueuer for PanicEnqueuer {
    async fn enqueue(&self, _: &WorkDb, _: &str, _: &str, _: &str, _: usize) -> Result<ReviewBatchDispatch> {
        panic!("a held candidate must not be enqueued (it would fetch PR metadata for nothing)");
    }
}

/// A candidate outside the dispatch focus set is counted as still deferred
/// and never reaches the enqueuer, so a held root costs no PR fetch.
#[tokio::test]
async fn sweep_skips_enqueue_for_roots_held_by_focus() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let root_id = in_review_root(&db);
    db.file_admission_deferred_attention(&root_id, PR_URL).unwrap();
    let mut focus = boss_protocol::DispatchFocus::default();
    focus.insert(
        boss_protocol::DispatchFocusTargetKind::Project,
        "proj_elsewhere".to_owned(),
    );
    db.save_dispatch_focus(&focus).unwrap();
    let publisher = RecordingPublisher::default();
    let mut outcome = SweepOutcome::default();

    super::super::deferred_review_admission::sweep_deferred_review_admission_with(
        &db,
        &publisher,
        &PanicEnqueuer,
        None,
        16,
        &mut outcome,
    )
    .await;

    assert_eq!(outcome.review_admission_recovered, 0);
    assert_eq!(outcome.review_admission_still_deferred, 1);
}

/// Focus restricts new spawns, not the transition for a settled head.
#[tokio::test]
async fn focus_held_settled_review_advances_and_resolves_deferred_attention() {
    for keep_batch in [true, false] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let root_id = in_review_root(&db);
        let (batch, executions) = settled_batch(&db, &root_id, "head-a");
        // A completed batch owns one aggregate verdict, attached to its
        // latest reviewer execution for the pending-review advance query.
        let execution = executions.iter().max_by_key(|e| (&e.created_at, &e.id)).unwrap();
        {
            db.connect()
                .unwrap()
                .execute(
                    "INSERT INTO pr_review_verdicts
                (id, execution_id, work_item_id, head_sha, findings_count,
                 revision_warranted, gate_outcome, created_at, batch_id)
             VALUES (?1, ?1, ?2, 'head-a', 0, 0, ?3, '1', ?4)",
                    rusqlite::params![
                        execution.id,
                        root_id,
                        crate::work::REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
                        batch.id
                    ],
                )
                .unwrap();
        }
        db.connect()
            .unwrap()
            .execute(
                "UPDATE pr_review_batches SET status = 'completed' WHERE id = ?1",
                [&batch.id],
            )
            .unwrap();
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&root_id])
            .unwrap();
        db.file_admission_deferred_attention(&root_id, PR_URL).unwrap();
        let mut focus = boss_protocol::DispatchFocus::default();
        focus.insert(
            boss_protocol::DispatchFocusTargetKind::Project,
            "proj_elsewhere".to_owned(),
        );
        db.save_dispatch_focus(&focus).unwrap();

        if !keep_batch {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE pr_review_verdicts SET batch_id = NULL WHERE batch_id = ?1",
                [&batch.id],
            )
            .unwrap();
            conn.execute("DELETE FROM pr_review_batches WHERE id = ?1", [&batch.id])
                .unwrap();
        }

        let outcome = sweep(&db, &RecordingPublisher::default(), "head-a").await;

        assert_eq!(outcome.reviewer_fallback_advanced, 1);
        assert_eq!(outcome.review_admission_still_deferred, 0);
        assert!(
            matches!(db.get_work_item(&root_id).unwrap(), boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) if t.status == boss_protocol::TaskStatus::InReview)
        );
        assert!(
            db.list_open_attention_items_of_kind(crate::work::PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)
                .unwrap()
                .is_empty()
        );
    }
}

// ── a revision held on a root that is outside dispatch focus ───────────────

/// A revision of `root_id` whose implementation run finished without
/// stamping a contributed head (it ended by declaring delivery), held
/// `active` with the root's PR stamped on it and an open deferred marker.
fn held_revision(db: &WorkDb, root_id: &str, launched: bool) -> String {
    let revision_id = "task_held_revision";
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at,
                                parent_task_id, pr_url, explicitly_launched)
             SELECT ?1, product_id, 'revision', 'Fix review findings', '', 'active', '1', '1', ?2, ?3, ?4
             FROM tasks WHERE id = ?2",
            rusqlite::params![revision_id, root_id, PR_URL, launched],
        )
        .unwrap();
    let implementation = db
        .create_execution(
            boss_protocol::CreateExecutionInput::builder()
                .work_item_id(revision_id)
                .kind(ExecutionKind::RevisionImplementation)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed', finished_at = '2' WHERE id = ?1",
            [&implementation.id],
        )
        .unwrap();
    db.file_admission_deferred_attention(revision_id, PR_URL).unwrap();
    revision_id.to_owned()
}

fn focus_elsewhere(db: &WorkDb) -> boss_protocol::DispatchFocus {
    let mut focus = boss_protocol::DispatchFocus::default();
    focus.insert(
        boss_protocol::DispatchFocusTargetKind::Project,
        "proj_elsewhere".to_owned(),
    );
    db.save_dispatch_focus(&focus).unwrap();
    focus
}

fn task_status(db: &WorkDb, task_id: &str) -> boss_protocol::TaskStatus {
    match db.get_work_item(task_id).unwrap() {
        boss_protocol::WorkItem::Task(task) | boss_protocol::WorkItem::Chore(task) => task.status,
        other => panic!("expected a task, got {other:?}"),
    }
}

fn open_deferred_markers(db: &WorkDb) -> Vec<String> {
    db.list_open_attention_items_of_kind(crate::work::PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)
        .unwrap()
        .into_iter()
        .filter_map(|item| item.work_item_id)
        .collect()
}

/// The root that owns the PR is outside focus and was never launched. A
/// revision that was explicitly launched has its push reviewed anyway; one
/// that was not stays deferred, without a PR fetch.
#[tokio::test]
async fn sweep_admits_the_review_of_a_launched_revisions_push_through_focus() {
    for launched in [true, false] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let root_id = in_review_root(&db);
        let revision_id = held_revision(&db, &root_id, launched);
        let focus = focus_elsewhere(&db);
        let publisher = RecordingPublisher::default();
        let mut outcome = SweepOutcome::default();

        if launched {
            outcome = sweep(&db, &publisher, "head-a").await;
        } else {
            super::super::deferred_review_admission::sweep_deferred_review_admission_with(
                &db,
                &publisher,
                &PanicEnqueuer,
                None,
                16,
                &mut outcome,
            )
            .await;
        }

        let batches = db.review_batches_for_cycle_root(&root_id).unwrap();
        if !launched {
            assert_eq!(outcome.review_admission_still_deferred, 1);
            assert!(batches.is_empty());
            assert_eq!(open_deferred_markers(&db), vec![revision_id]);
            continue;
        }
        assert_eq!(outcome.review_admission_recovered, 1, "{outcome:?}");
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].producing_work_item_id.as_deref(), Some(revision_id.as_str()));
        let members = db.list_executions(Some(&root_id)).unwrap();
        assert_eq!(members.len(), 2);
        assert!(
            db.executions_outside_dispatch_focus(&members, &focus)
                .unwrap()
                .is_empty(),
            "the admitted reviewers must be able to dispatch"
        );
        assert!(open_deferred_markers(&db).is_empty());
        assert_eq!(
            task_status(&db, &revision_id),
            boss_protocol::TaskStatus::Active,
            "the revision stays held while its review runs"
        );
    }
}

/// The revision's review was deferred, then run by a batch admitted against
/// the root (`bossctl review start`). That batch is settled at the head the
/// PR still has, so the revision leaves its hold and its marker is cleared.
#[tokio::test]
async fn sweep_releases_a_revision_whose_head_was_reviewed_under_its_root() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let root_id = in_review_root(&db);
    let revision_id = held_revision(&db, &root_id, false);
    let (batch, executions) = settled_batch(&db, &root_id, "head-a");
    // The batch's one verdict is recorded against the root, on its newest
    // reviewer execution.
    let reviewer = executions.iter().max_by_key(|e| (&e.created_at, &e.id)).unwrap();
    {
        let conn = db.connect().unwrap();
        // An explicit start records its launch on the root it is admitted for.
        conn.execute("UPDATE tasks SET explicitly_launched = 1 WHERE id = ?1", [&root_id])
            .unwrap();
        conn.execute(
            "INSERT INTO pr_review_verdicts
                (id, execution_id, work_item_id, head_sha, findings_count,
                 revision_warranted, gate_outcome, created_at, batch_id)
             VALUES (?1, ?1, ?2, 'head-a', 0, 0, ?3, '1', ?4)",
            rusqlite::params![
                reviewer.id,
                root_id,
                crate::work::REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
                batch.id
            ],
        )
        .unwrap();
        conn.execute(
            "UPDATE pr_review_batches SET status = 'completed' WHERE id = ?1",
            [&batch.id],
        )
        .unwrap();
    }
    focus_elsewhere(&db);
    // The marker a deferral left behind, as rows deferred before admission
    // cleared it for the whole cycle still carry.
    db.file_admission_deferred_attention(&revision_id, PR_URL).unwrap();

    let outcome = sweep(&db, &RecordingPublisher::default(), "head-a").await;

    assert_eq!(outcome.reviewer_fallback_advanced, 1, "{outcome:?}");
    assert_eq!(task_status(&db, &revision_id), boss_protocol::TaskStatus::InReview);
    assert!(open_deferred_markers(&db).is_empty());
    assert_eq!(
        db.review_batches_for_cycle_root(&root_id).unwrap().len(),
        1,
        "a settled head is not reviewed again"
    );
}
