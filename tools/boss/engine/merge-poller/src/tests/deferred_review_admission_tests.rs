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
        self.enqueue_with_mode(
            work_db,
            work_item_id,
            repo_remote_url,
            pr_url,
            review_pool_size,
            boss_observation_contracts::DeltaRereviewDecision::default(),
        )
        .await
    }

    async fn enqueue_with_mode(
        &self,
        work_db: &WorkDb,
        work_item_id: &str,
        repo_remote_url: &str,
        pr_url: &str,
        review_pool_size: usize,
        decision: boss_observation_contracts::DeltaRereviewDecision,
    ) -> Result<ReviewBatchDispatch> {
        // As the production enqueuer builds it: keyed on the cycle root, and
        // naming the candidate when that is a revision of the root.
        let root_id = work_db.review_cycle_root_id(work_item_id);
        let mut input = batch_input(root_id.clone(), pr_url, self.head);
        input.legacy_task_id = (work_item_id != root_id).then(|| work_item_id.to_owned());
        input.single_reviewer_light = decision.single_reviewer_light;
        input.delta_rereview_arm = decision.arm;
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
        &|_, _| boss_observation_contracts::DeltaRereviewDecision::default(),
        None,
        16,
        &mut outcome,
    )
    .await;
    outcome
}

/// Test double for [`CompletionRequests`] that drives the production
/// `pass.rs` and `sweep.rs` deferred-admission wiring (`handler.delta_rereview_decision`)
/// with a scripted arm and deterministic PR metadata (`FixedHeadEnqueuer`).
struct DeltaRoutingCompletionHandler {
    enqueuer: FixedHeadEnqueuer,
    arm: Option<boss_protocol::DeltaRereviewBatchArm>,
    asked: std::sync::Mutex<Vec<String>>,
}

impl DeltaRoutingCompletionHandler {
    fn new(head: &'static str, arm: Option<boss_protocol::DeltaRereviewBatchArm>) -> Self {
        Self {
            enqueuer: FixedHeadEnqueuer { head },
            arm,
            asked: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl CompletionRequests for DeltaRoutingCompletionHandler {
    fn review_guide_source_capture_enabled(&self) -> bool {
        false
    }
    fn pending_background_nudge_execution_ids(&self) -> Vec<String> {
        Vec::new()
    }
    async fn recheck_background_nudge(&self, _execution_id: &str) -> Option<StopOutcome> {
        None
    }
    fn review_batch_fanout_enabled(&self) -> bool {
        true
    }
    fn review_pool_size(&self) -> usize {
        16
    }
    async fn force_release(&self, _execution_id: &str) {}
    async fn record_parent_pr_merge_cancellation(
        &self,
        _: &crate::work::WorkExecution,
        _: boss_protocol::ExecutionStatus,
        _: &str,
    ) {
    }
    async fn force_stop_execution(&self, _execution_id: &str) {}
    async fn recheck_for_pr(&self, _execution_id: &str) -> StopOutcome {
        StopOutcome::AwaitingInput
    }
    async fn recheck_for_pr_late(&self, _candidate: &crate::work::LatePrCandidate) -> StopOutcome {
        StopOutcome::AwaitingInput
    }
    fn reconcile_review_guide_source_from_probe(&self, _: &str, _: &str, _: &str, _: &str, _: i64) {}
    fn speculative_conflict_prediction_enabled(&self) -> bool {
        false
    }
    fn stacked_pr_auto_structuring_enabled(&self) -> bool {
        false
    }
    fn mechanical_rebase_enabled(&self) -> bool {
        false
    }
    fn review_batch_enqueuer(&self) -> &dyn ReviewBatchEnqueuer {
        &self.enqueuer
    }
    fn delta_rereview_decision(
        &self,
        work_db: &WorkDb,
        work_item_id: &str,
    ) -> boss_observation_contracts::DeltaRereviewDecision {
        self.asked.lock().unwrap().push(work_item_id.to_owned());
        let root_id = work_db.review_cycle_root_id(work_item_id);
        let (review_cycle, last_reviewed_sha) = work_db.get_task_review_cycle_state(&root_id).unwrap_or((0, None));
        let is_repeat = review_cycle >= 1 && last_reviewed_sha.as_deref().is_some_and(|s| !s.is_empty());
        boss_observation_contracts::DeltaRereviewDecision {
            single_reviewer_light: is_repeat && self.arm == Some(boss_protocol::DeltaRereviewBatchArm::Treatment),
            arm: self.arm,
        }
    }
}

fn seed_reaped_candidate(db: &WorkDb, review_cycle: i64, last_reviewed_sha: Option<&str>) -> (String, String) {
    let root_id = in_review_root(db);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET review_cycle = ?2, last_reviewed_sha = ?3 WHERE id = ?1",
            rusqlite::params![root_id, review_cycle, last_reviewed_sha],
        )
        .unwrap();
    let (reaped, _) = settled_batch(db, &root_id, "head-a");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE pr_review_batch_members SET status = 'failed' WHERE batch_id = ?1",
            rusqlite::params![reaped.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE pr_review_batches SET status = 'failed' WHERE id = ?1",
            rusqlite::params![reaped.id],
        )
        .unwrap();
    }
    (root_id, reaped.id)
}

/// Drive deferred admission through the production `run_one_pass_tracked`
/// entry (`pass.rs`) with a `CompletionRequests` double: round 1 and control
/// round 2 admit a full two-leaf batch, and treated round 2 admits a solo
/// Light-tier batch, persisting `delta_rereview_arm` on each batch.
#[tokio::test]
async fn run_one_pass_routes_deferred_admission_through_completion_handler() {
    use boss_protocol::DeltaRereviewBatchArm;
    for (label, review_cycle, last_sha, arm, expected_solo, expected_members) in [
        (
            "round1_treated",
            0,
            None,
            Some(DeltaRereviewBatchArm::Treatment),
            false,
            2,
        ),
        (
            "round2_control",
            1,
            Some("head-0"),
            Some(DeltaRereviewBatchArm::Control),
            false,
            2,
        ),
        (
            "round2_treated",
            1,
            Some("head-0"),
            Some(DeltaRereviewBatchArm::Treatment),
            true,
            1,
        ),
    ] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let (root_id, reaped_id) = seed_reaped_candidate(&db, review_cycle, last_sha);
        let probe = StubProbe::new();
        probe.set(PR_URL, PrLifecycleState::Open(OpenPrStatus::clean()));
        let handler = DeltaRoutingCompletionHandler::new("head-a", arm);
        let state = PassState::default();

        let (outcome, _) = run_one_pass_tracked(
            &db,
            probe.as_ref(),
            &RecordingPublisher::default(),
            SweepHandlers {
                cube_client: None,
                completion_handler: Some(&handler),
                host: None,
                parked_ids: None,
            },
            &state,
            Duration::from_secs(30),
        )
        .await;

        assert_eq!(outcome.review_admission_recovered, 1, "{label}");
        assert_eq!(handler.asked(), vec![root_id.clone()], "{label}");
        let batch = db
            .review_batches_for_cycle_root(&root_id)
            .unwrap()
            .into_iter()
            .find(|b| b.id != reaped_id)
            .expect("batch admitted");
        assert_eq!(batch.single_reviewer, expected_solo, "{label}");
        assert_eq!(batch.delta_rereview_arm, arm, "{label}");
        assert_eq!(
            db.review_batch_members(&batch.id).unwrap().len(),
            expected_members,
            "{label}"
        );
    }
}

/// Drive deferred admission through the production `reconcile_batch` entry
/// (`sweep.rs`) with a `CompletionRequests` double, asserting the targeted
/// sweep calls `handler.delta_rereview_decision` and admits the matching shape.
#[tokio::test]
async fn reconcile_batch_routes_deferred_admission_through_completion_handler() {
    use boss_protocol::DeltaRereviewBatchArm;
    for (label, arm, expected_solo, expected_members) in [
        ("round2_control", Some(DeltaRereviewBatchArm::Control), false, 2),
        ("round2_treated", Some(DeltaRereviewBatchArm::Treatment), true, 1),
    ] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let (root_id, reaped_id) = seed_reaped_candidate(&db, 1, Some("head-0"));
        let probe = StubProbe::new();
        probe.set(PR_URL, PrLifecycleState::Open(OpenPrStatus::clean()));
        let handler = DeltaRoutingCompletionHandler::new("head-a", arm);
        let urls = vec![PR_URL.to_owned()];

        let (outcome, _) = reconcile_batch(
            &db,
            probe.as_ref(),
            &RecordingPublisher::default(),
            None,
            Some(&handler),
            None,
            &urls,
        )
        .await;

        assert_eq!(outcome.review_admission_recovered, 1, "{label}");
        assert_eq!(handler.asked(), vec![root_id.clone()], "{label}");
        let batch = db
            .review_batches_for_cycle_root(&root_id)
            .unwrap()
            .into_iter()
            .find(|b| b.id != reaped_id)
            .expect("batch admitted");
        assert_eq!(batch.single_reviewer, expected_solo, "{label}");
        assert_eq!(batch.delta_rereview_arm, arm, "{label}");
        assert_eq!(
            db.review_batch_members(&batch.id).unwrap().len(),
            expected_members,
            "{label}"
        );
    }
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
        &|_, _| boss_observation_contracts::DeltaRereviewDecision::default(),
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
                &|_, _| boss_observation_contracts::DeltaRereviewDecision::default(),
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

/// The batch for the head the revision pushed failed for good: both
/// automatic generations ended without a verdict, so no replacement will be
/// minted and no verdict row exists. A failed batch is a terminal decision,
/// so the revision leaves its hold on the sweep's next pass without a verdict.
#[tokio::test]
async fn sweep_releases_a_revision_whose_heads_batch_failed_without_a_verdict() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let root_id = in_review_root(&db);
    let revision_id = held_revision(&db, &root_id, false);
    let (batch, _) = settled_batch(&db, &root_id, "head-a");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE pr_review_batch_members SET status = 'failed' WHERE batch_id = ?1",
            rusqlite::params![batch.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE pr_review_batches SET status = 'failed', generation = ?2 WHERE id = ?1",
            rusqlite::params![batch.id, crate::work::MAX_AUTOMATIC_PRE_MERGE_BATCH_GENERATIONS],
        )
        .unwrap();
    }
    let publisher = RecordingPublisher::default();

    let outcome = sweep(&db, &publisher, "head-a").await;

    assert_eq!(outcome.reviewer_fallback_advanced, 1, "{outcome:?}");
    assert_eq!(task_status(&db, &revision_id), boss_protocol::TaskStatus::InReview);
    assert!(open_deferred_markers(&db).is_empty());
    assert_eq!(
        batches_for(&db, &root_id),
        vec![(
            crate::work::MAX_AUTOMATIC_PRE_MERGE_BATCH_GENERATIONS,
            "head-a".to_owned(),
            "failed".to_owned()
        )],
        "the generation cap is not exceeded to release the revision"
    );

    // Nothing is left for a later pass to find.
    let again = sweep(&db, &publisher, "head-a").await;
    assert_eq!(again.reviewer_fallback_advanced, 0, "{again:?}");
}

#[tokio::test(start_paused = true)]
async fn review_pool_sampling_survives_yield_before_upkeep() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let root = in_review_root(&db);
    db.create_pre_merge_review_batch(batch_input(root, PR_URL, "head"), "https://github.com/example/repo")
        .unwrap();
    let expected = db.review_pool_reserved_units().unwrap();
    assert!(expected.0 + expected.1 > 0);
    let state = PassState::default();
    let (outcome, _) = run_one_pass_tracked(
        &db,
        StubProbe::new().as_ref(),
        &RecordingPublisher::default(),
        SweepHandlers {
            cube_client: None,
            completion_handler: None,
            host: None,
            parked_ids: None,
        },
        &state,
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(outcome.reserved_review_units, expected.0 + expected.1);
    assert!(
        !state
            .trace()
            .timings()
            .phases
            .iter()
            .any(|p| p.phase == PassPhase::ReviewBatchUpkeep)
    );
}
