//! Merge and close detection for pull requests whose row is on none of the
//! status-keyed candidate lists: a row held `active` for a review, and
//! revisions resting `in_review` under a root that is terminal or deleted.

use super::*;
use boss_observation_contracts::{CompletionRequests, ReviewBatchEnqueuer, StopOutcome};
use boss_protocol::WorkExecution;
use boss_work_db::ReviewBatchDispatch;

const PR: &str = "https://github.com/acme/widget/pull/3907";
const REPO: &str = "https://github.com/acme/widget.git";

/// A held review must never reach admission: fetching PR metadata for a
/// review that dispatch focus holds is the cost the sweep avoids.
struct RefusingEnqueuer;

#[async_trait]
impl ReviewBatchEnqueuer for RefusingEnqueuer {
    async fn enqueue(&self, _: &WorkDb, _: &str, _: &str, _: &str, _: usize) -> anyhow::Result<ReviewBatchDispatch> {
        panic!("a review held by dispatch focus must not be enqueued")
    }
}

/// Completion requests with review-batch fan-out on, so the pass runs the
/// deferred-admission subphase, recording which executions it released.
struct FanoutHandler {
    released: std::sync::Mutex<Vec<String>>,
    enqueuer: RefusingEnqueuer,
}

impl FanoutHandler {
    fn new() -> Self {
        Self {
            released: std::sync::Mutex::new(Vec::new()),
            enqueuer: RefusingEnqueuer,
        }
    }
}

#[async_trait]
impl CompletionRequests for FanoutHandler {
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
    async fn force_release(&self, execution_id: &str) {
        self.released.lock().unwrap().push(execution_id.to_owned());
    }
    async fn record_parent_pr_merge_cancellation(&self, _: &WorkExecution, _: ExecutionStatus, _: &str) {}
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
}

/// A probe that counts how often each PR was asked about.
struct CountingProbe {
    inner: Arc<StubProbe>,
    asked: std::sync::Mutex<Vec<String>>,
}

impl CountingProbe {
    fn new() -> Self {
        Self {
            inner: StubProbe::new(),
            asked: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn asked_about(&self, pr_url: &str) -> usize {
        self.asked.lock().unwrap().iter().filter(|url| *url == pr_url).count()
    }
}

#[async_trait]
impl MergeProbe for CountingProbe {
    async fn probe(&self, pr_url: &str) -> Result<PrLifecycleProbe> {
        self.asked.lock().unwrap().push(pr_url.to_owned());
        self.inner.probe(pr_url).await
    }
}

fn task(db: &WorkDb, id: &str) -> boss_protocol::Task {
    match db.get_work_item(id).unwrap() {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("expected a task, got {other:?}"),
    }
}

fn finished_implementation(db: &WorkDb, chore_id: &str) -> String {
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url(REPO)
                .build(),
        )
        .unwrap();
    let (execution, run) = db
        .start_execution_run(&execution.id, "agent-1", "repo-1", "lease-1", "ws-1", "/ws/1")
        .unwrap();
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&execution.id)
            .run_id(&run.id)
            .execution_status(ExecutionStatus::Completed)
            .run_status("completed")
            .build(),
    )
    .unwrap();
    execution.id
}

/// A chore whose implementation finished and opened `pr`, held `active`
/// because the review of that PR could not be admitted. Returns
/// `(product_id, chore_id, implementation_execution_id)`.
fn chore_held_for_review(db: &WorkDb, name: &str, pr: &str) -> (String, String, String) {
    let product = create_test_product_with_repo(db, &format!("Product-{name}"), Some("git@github.com:acme/widget.git"));
    let chore = create_test_chore_manual(db, product.id.clone(), name);
    let implementation = finished_implementation(db, &chore.id);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'active', pr_url = ?2, repo_remote_url = ?3 WHERE id = ?1",
            rusqlite::params![chore.id, pr, REPO],
        )
        .unwrap();
    db.file_admission_deferred_attention(&chore.id, pr).unwrap();
    (product.id, chore.id, implementation)
}

fn focus_on_another_project(db: &WorkDb) {
    let mut focus = boss_protocol::DispatchFocus::default();
    focus.insert(
        boss_protocol::DispatchFocusTargetKind::Project,
        "proj_elsewhere".to_owned(),
    );
    db.save_dispatch_focus(&focus).unwrap();
}

/// A review guide queued for the PR, as one waits when focus holds it.
fn queued_review_guide(db: &WorkDb, chore_id: &str, pr: &str) -> (String, String) {
    let (series, comparison) =
        crate::work::test_support::seed_review_guide_series_for_pr(db, chore_id, pr, "base", "head");
    let attempt = db.create_pr_review_guide_attempt(&series, &comparison, "test").unwrap();
    let attempt = db.dispatch_pr_review_guide_attempt(&attempt.id, chore_id).unwrap();
    (attempt.id, attempt.execution_id.expect("dispatch binds an execution"))
}

fn open_followups(db: &WorkDb, product_id: &str, chore_id: &str) -> usize {
    db.list_attention_groups(
        product_id,
        None,
        Some(chore_id),
        Some(boss_engine_storage_values::attention_kinds::FOLLOWUP_ATTENTION_KIND),
        None,
    )
    .unwrap()
    .len()
}

fn open_deferred_markers(db: &WorkDb) -> usize {
    db.list_open_attention_items_of_kind(crate::work::PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)
        .unwrap()
        .len()
}

async fn pass(db: &WorkDb, probe: &dyn MergeProbe, handler: &FanoutHandler) -> SweepOutcome {
    run_one_pass(db, probe, &RecordingPublisher::default(), None, Some(handler), None).await
}

/// The reported sequence. A chore finished and opened its PR; dispatch focus
/// is on another project, so its review was never admitted and the row stays
/// `active` with a review guide queued and a follow-up proposal open. The PR
/// is then merged on GitHub. The row must reach `done` on the merge alone.
#[tokio::test]
async fn merged_pr_finalizes_a_chore_held_for_a_review_that_focus_deferred() {
    let (_dir, db) = open_db();
    let (product_id, chore_id, implementation) = chore_held_for_review(&db, "held", PR);
    focus_on_another_project(&db);
    let (guide_attempt, guide_execution) = queued_review_guide(&db, &chore_id, PR);
    db.create_attention(boss_protocol::CreateAttentionInput {
        kind: boss_engine_storage_values::attention_kinds::FOLLOWUP_ATTENTION_KIND.to_owned(),
        association_task_id: Some(chore_id.clone()),
        source_kind: Some("task_transcript".to_owned()),
        source_task_id: Some(chore_id.clone()),
        proposed_name: Some("Tighten the adapter's command allowlist".to_owned()),
        proposed_description: Some("Noticed while granting commands.".to_owned()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(open_followups(&db, &product_id, &chore_id), 1);
    let handler = FanoutHandler::new();
    let probe = CountingProbe::new();

    // While the PR is open the hold stands, and the row is watched.
    let held = pass(&db, &probe, &handler).await;
    assert_eq!(held.merged, 0);
    assert_eq!(held.review_admission_still_deferred, 1, "{held:?}");
    assert_eq!(task(&db, &chore_id).status, TaskStatus::Active);
    assert_eq!(
        probe.asked_about(PR),
        1,
        "a row held for review must have its PR watched for a merge"
    );

    probe.inner.set(PR, PrLifecycleState::Merged);
    let merged = pass(&db, &probe, &handler).await;

    assert_eq!(merged.merged, 1, "{merged:?}");
    let chore = task(&db, &chore_id);
    assert_eq!(chore.status, TaskStatus::Done);
    assert!(chore.completed_at.is_some());
    assert_eq!(
        db.latest_status_audit_basis(&chore_id, "status").unwrap().as_deref(),
        Some("mark_chore_pr_merged"),
    );
    assert_eq!(db.get_merge_evidence(&chore_id).unwrap().unwrap().canonical_pr_url, PR);

    // The review that was still to come is cancelled, and says why.
    assert_eq!(
        db.get_execution(&guide_execution).unwrap().status,
        ExecutionStatus::Cancelled
    );
    let (guide_status, guide_reason): (String, Option<String>) = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT status, error FROM pr_review_guide_attempts WHERE id = ?1",
            [&guide_attempt],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(guide_status, "cancelled");
    assert_eq!(guide_reason.as_deref(), Some("parent PR merged"));
    assert_eq!(handler.released.lock().unwrap().clone(), vec![guide_execution]);
    assert_eq!(
        open_deferred_markers(&db),
        0,
        "no review of a merged PR is still to be admitted"
    );

    // What the run produced is untouched, and its follow-up stays answerable.
    assert_eq!(
        db.get_execution(&implementation).unwrap().status,
        ExecutionStatus::Completed
    );
    assert_eq!(open_followups(&db, &product_id, &chore_id), 1);

    // Nothing is left for a later pass to do.
    let after = pass(&db, &probe, &handler).await;
    assert_eq!(after.total_transitions(), 0, "{after:?}");
}

/// The hold with its review admitted but not finished: the reviewers are
/// queued when the PR merges. They are read-only, so they do not keep the row
/// off the watch, and the pass that records the merge retires them with the
/// reason recorded.
#[tokio::test]
async fn merged_pr_retires_the_review_still_queued_for_a_held_chore() {
    let (_dir, db) = open_db();
    let (_product_id, chore_id, _) = chore_held_for_review(&db, "held-reviewing", PR);
    let input = crate::work::ReviewBatchCreateInput::builder()
        .cycle_root_id(chore_id.clone())
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
        .pr_number(3907)
        .pr_url(PR)
        .target_sha("head-a")
        .build();
    let ReviewBatchDispatch::Created { batch, executions } = db.create_pre_merge_review_batch(input, REPO).unwrap()
    else {
        panic!("expected a new review batch");
    };
    assert!(!executions.is_empty());
    let probe = StubProbe::new();
    probe.set(PR, PrLifecycleState::Merged);

    let outcome = run_one_pass(&db, probe.as_ref(), &RecordingPublisher::default(), None, None, None).await;

    assert_eq!(outcome.merged, 1, "{outcome:?}");
    assert_eq!(outcome.review_batches_reaped, 1, "{outcome:?}");
    assert_eq!(task(&db, &chore_id).status, TaskStatus::Done);
    let conn = db.connect().unwrap();
    let batch_status: String = conn
        .query_row(
            "SELECT status FROM pr_review_batches WHERE id = ?1",
            [&batch.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(batch_status, "failed");
    for execution in &executions {
        let (status, reason): (String, Option<String>) = conn
            .query_row(
                "SELECT e.status,
                        (SELECT a.reason_code FROM task_status_audit a
                          WHERE a.execution_id = e.id AND a.property = 'execution_abandoned'
                          ORDER BY a.id DESC LIMIT 1)
                 FROM work_executions e WHERE e.id = ?1",
                [&execution.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "abandoned", "{}", execution.id);
        assert_eq!(reason.as_deref(), Some("review_batch_reaped"), "{}", execution.id);
    }
}

/// The same hold, with the PR closed instead: the row is retired as closed
/// without merging, exactly as an `in_review` row is.
#[tokio::test]
async fn closed_pr_retires_a_chore_held_for_review() {
    let (_dir, db) = open_db();
    let (_product_id, chore_id, _) = chore_held_for_review(&db, "held-closed", PR);
    focus_on_another_project(&db);
    let handler = FanoutHandler::new();
    let probe = CountingProbe::new();
    probe.inner.set(PR, PrLifecycleState::ClosedUnmerged);

    let outcome = pass(&db, &probe, &handler).await;

    assert_eq!(outcome.closed_unmerged, 1, "{outcome:?}");
    assert_eq!(task(&db, &chore_id).status, TaskStatus::Done);
    assert_eq!(
        db.latest_status_audit_basis(&chore_id, "status").unwrap().as_deref(),
        Some("mark_chore_pr_closed_unmerged"),
    );
    assert_eq!(open_deferred_markers(&db), 0);
}

/// A held row is watched for merge or close only. Its PR going red or
/// conflicting while it waits for review changes nothing: that upkeep starts
/// when the review releases the row to `in_review`.
#[tokio::test]
async fn open_pr_of_a_held_chore_gets_no_upkeep() {
    let (_dir, db) = open_db();
    let (_product_id, chore_id, _) = chore_held_for_review(&db, "held-red", PR);
    let probe = StubProbe::new();
    probe
        .states
        .lock()
        .unwrap()
        .insert(PR.to_owned(), Ok(probe_ci_failing(PR, "head-red")));

    let outcome = run_one_pass(&db, probe.as_ref(), &RecordingPublisher::default(), None, None, None).await;

    assert_eq!(outcome.total_transitions(), 0, "{outcome:?}");
    let chore = task(&db, &chore_id);
    assert_eq!(chore.status, TaskStatus::Active);
    assert_eq!(chore.blocked_reason, None);
}

/// A row with a run in flight that could still push to the PR belongs to
/// that run. A row moved back to `active` to redo work whose PR was closed
/// must not be retired again under the run just started for it.
#[tokio::test]
async fn a_held_chore_with_a_writer_in_flight_is_left_to_that_run() {
    let (_dir, db) = open_db();
    let (_product_id, chore_id, _) = chore_held_for_review(&db, "restarted", PR);
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore_id.clone())
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Ready)
            .repo_remote_url(REPO)
            .build(),
    )
    .unwrap();
    let probe = CountingProbe::new();
    probe.inner.set(PR, PrLifecycleState::ClosedUnmerged);

    let outcome = run_one_pass(&db, &probe, &RecordingPublisher::default(), None, None, None).await;

    assert_eq!(outcome.closed_unmerged, 0, "{outcome:?}");
    assert_eq!(task(&db, &chore_id).status, TaskStatus::Active);
    assert_eq!(probe.asked_about(PR), 0);
}

/// The per-PR reconcile reaches the same row the full pass does.
#[tokio::test]
async fn targeted_reconcile_finalizes_a_held_chore_whose_pr_merged() {
    let (_dir, db) = open_db();
    let (_product_id, chore_id, _) = chore_held_for_review(&db, "held-targeted", PR);
    let probe = StubProbe::new();

    // Open: probed, and no open-PR upkeep is run from the result.
    probe
        .states
        .lock()
        .unwrap()
        .insert(PR.to_owned(), Ok(probe_ci_failing(PR, "head-red")));
    let (open, _) = reconcile_one(
        &db,
        probe.as_ref(),
        &RecordingPublisher::default(),
        None,
        None,
        None,
        PR,
    )
    .await;
    assert_eq!(open.total_transitions(), 0, "{open:?}");
    assert_eq!(task(&db, &chore_id).status, TaskStatus::Active);

    probe.set(PR, PrLifecycleState::Merged);
    let (merged, _) = reconcile_one(
        &db,
        probe.as_ref(),
        &RecordingPublisher::default(),
        None,
        None,
        None,
        PR,
    )
    .await;
    assert_eq!(merged.merged, 1, "{merged:?}");
    assert_eq!(task(&db, &chore_id).status, TaskStatus::Done);
}

// ── revisions left in review under a root that cannot carry them ──────────

const CLOSED_PR: &str = "https://github.com/acme/widget/pull/3750";

/// How the row that owns the PR left the poller's lists.
#[derive(Clone, Copy, Debug)]
enum RootFate {
    Archived,
    DoneByHand,
    Deleted,
}

fn revision_in_review(db: &WorkDb, id: &str, root_id: &str, name: &str, created_via: &str) -> String {
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at,
                                parent_task_id, created_via, autostart, last_status_actor)
             SELECT ?1, product_id, 'revision', ?3, '', 'in_review', '1', '1', ?2, ?4, 0, 'engine'
             FROM tasks WHERE id = ?2",
            rusqlite::params![id, root_id, name, created_via],
        )
        .unwrap();
    id.to_owned()
}

/// The owner of `CLOSED_PR` with the four engine-minted revisions the report
/// names resting in review, and the owner then removed by `fate`.
fn orphaned_chain(db: &WorkDb, fate: RootFate) -> (String, String, Vec<String>) {
    let (product_id, root_id) = make_chore_in_review(db, &format!("owner-{fate:?}"), CLOSED_PR);
    let revisions = vec![
        revision_in_review(
            db,
            "task_rev_findings_1",
            &root_id,
            "PR #3750 review: 3 finding(s)",
            "pr_review:exec_a",
        ),
        revision_in_review(
            db,
            "task_rev_findings_2",
            &root_id,
            "PR #3750 review: 1 finding(s)",
            "pr_review:exec_b",
        ),
        revision_in_review(
            db,
            "task_rev_findings_3",
            &root_id,
            "PR #3750 review: 2 finding(s)",
            "pr_review:exec_c",
        ),
        revision_in_review(
            db,
            "task_rev_conflict",
            &root_id,
            "Resolve merge conflict against main",
            "merge-conflict:attempt_1",
        ),
    ];
    match fate {
        RootFate::Archived | RootFate::DoneByHand => {
            let status = if matches!(fate, RootFate::Archived) {
                "archived"
            } else {
                "done"
            };
            db.update_work_item(
                &root_id,
                WorkItemPatch {
                    status: Some(status.into()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap();
        }
        // A tombstone that did not take the revisions with it.
        RootFate::Deleted => {
            db.connect()
                .unwrap()
                .execute("UPDATE tasks SET deleted_at = '5' WHERE id = ?1", [&root_id])
                .unwrap();
        }
    }
    (product_id, root_id, revisions)
}

fn revision_statuses(db: &WorkDb, revisions: &[String]) -> Vec<String> {
    let conn = db.connect().unwrap();
    revisions
        .iter()
        .map(|id| {
            conn.query_row("SELECT status FROM tasks WHERE id = ?1", [id], |row| row.get(0))
                .unwrap()
        })
        .collect()
}

/// The second reported sequence. Revisions are in review on a PR whose
/// owning row is already archived, done or deleted, so no list names that PR.
/// The PR is then closed without merging. Every revision must end, with the
/// reason recorded, on the pass that sees the close.
#[tokio::test]
async fn closed_pr_settles_in_review_revisions_whose_root_is_gone() {
    for fate in [RootFate::Archived, RootFate::DoneByHand, RootFate::Deleted] {
        let (_dir, db) = open_db();
        let (product_id, _root_id, revisions) = orphaned_chain(&db, fate);
        let probe = CountingProbe::new();
        let publisher = RecordingPublisher::default();

        // Still open: watched, and left in review.
        let open = run_one_pass(&db, &probe, &publisher, None, None, None).await;
        assert_eq!(open.total_transitions(), 0, "{fate:?}: {open:?}");
        assert_eq!(probe.asked_about(CLOSED_PR), 1, "{fate:?}: the PR must be watched");
        assert_eq!(revision_statuses(&db, &revisions), vec!["in_review"; 4], "{fate:?}");

        probe.inner.set(CLOSED_PR, PrLifecycleState::ClosedUnmerged);
        let closed = run_one_pass(&db, &probe, &publisher, None, None, None).await;

        assert_eq!(closed.orphaned_revisions_settled, 4, "{fate:?}: {closed:?}");
        assert_eq!(revision_statuses(&db, &revisions), vec!["done"; 4], "{fate:?}");
        for revision in &revisions {
            assert_eq!(
                db.latest_status_audit_basis(revision, "status").unwrap().as_deref(),
                Some(crate::work::ORPHANED_REVISION_PR_CLOSED_UNMERGED_BASIS),
                "{fate:?}: {revision} must record why it ended"
            );
            let completed_at: Option<String> = db
                .connect()
                .unwrap()
                .query_row("SELECT completed_at FROM tasks WHERE id = ?1", [revision], |row| {
                    row.get(0)
                })
                .unwrap();
            assert!(completed_at.is_some(), "{fate:?}: {revision}");
        }
        let events = publisher.events.lock().await.clone();
        for revision in &revisions {
            assert!(
                events.iter().any(|(product, item, reason)| product == &product_id
                    && item == revision
                    && reason == "pr_closed_unmerged"),
                "{fate:?}: the board must hear that {revision} ended, got {events:?}"
            );
        }

        // The chain is settled: the PR is no longer asked about.
        let asked = probe.asked_about(CLOSED_PR);
        let after = run_one_pass(&db, &probe, &publisher, None, None, None).await;
        assert_eq!(after.total_transitions(), 0, "{fate:?}: {after:?}");
        assert_eq!(probe.asked_about(CLOSED_PR), asked, "{fate:?}");
    }
}

/// A merge of the same PR ends the revisions too, recorded as a merge.
#[tokio::test]
async fn merged_pr_settles_in_review_revisions_whose_root_is_gone() {
    let (_dir, db) = open_db();
    let (_product_id, _root_id, revisions) = orphaned_chain(&db, RootFate::Archived);
    let probe = StubProbe::new();
    probe.set(CLOSED_PR, PrLifecycleState::Merged);

    let (outcome, _) = reconcile_one(
        &db,
        probe.as_ref(),
        &RecordingPublisher::default(),
        None,
        None,
        None,
        CLOSED_PR,
    )
    .await;

    assert_eq!(outcome.orphaned_revisions_settled, 4, "{outcome:?}");
    assert_eq!(revision_statuses(&db, &revisions), vec!["done"; 4]);
    assert_eq!(
        db.latest_status_audit_basis(&revisions[0], "status")
            .unwrap()
            .as_deref(),
        Some(crate::work::ORPHANED_REVISION_PR_MERGED_BASIS),
    );
}
