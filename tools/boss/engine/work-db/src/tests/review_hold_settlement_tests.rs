//! Settling a finished revision's review hold against a pull-request head.
//!
//! The shape throughout is a PR-owning root in review with one revision whose
//! run pushed, ended, and left it held `active`, and a later revision queued
//! behind it. The held revision is released by the decision of the review
//! batch for the head it pushed. These tests fix what happens when the
//! revision comes to be held for that head only after the decision was made,
//! which no decision can then release.

use boss_protocol::{ExecutionStatus, ReviewBatchPhase};

use super::*;

const REPO: &str = "https://github.com/example/repo";
const PR_URL: &str = "https://github.com/example/repo/pull/42";
const HEAD: &str = "pushed-head";

struct Held {
    db: WorkDb,
    root_id: String,
    revision_id: String,
    /// The held revision's finished `revision_implementation` run.
    run_id: String,
    /// A revision created after the held one, queued behind it.
    later_id: String,
}

fn sql(db: &WorkDb, statement: &str, params: impl rusqlite::Params) {
    db.connect().unwrap().execute(statement, params).unwrap();
}

/// A revision whose run finished and left it held for review, with nothing
/// yet recorded about the head it pushed: the state a declared delivery left
/// behind before it read the head.
fn held(label: &str) -> Held {
    let db = WorkDb::open(temp_db_path(label)).unwrap();
    let product_id = make_revision_product(&db, label);
    let root_id = make_in_review_chore(&db, &product_id, PR_URL);
    let checker = FakePrStateChecker::always(PrOpenState::Open);

    let revision = db.create_revision(revision_input(&root_id), &checker).unwrap();
    let run = db
        .list_executions(Some(&revision.id))
        .unwrap()
        .into_iter()
        .find(|execution| execution.kind == ExecutionKind::RevisionImplementation)
        .expect("a revision is created with its first run queued");
    sql(
        &db,
        "UPDATE work_executions SET status = 'completed', finished_at = '2' WHERE id = ?1",
        [&run.id],
    );
    sql(
        &db,
        "UPDATE tasks SET status = 'active', pr_url = ?2 WHERE id = ?1",
        params![revision.id, PR_URL],
    );

    let later = db.create_revision(revision_input(&root_id), &checker).unwrap();
    assert_eq!(
        db.gating_prereqs_for(&later.id).unwrap(),
        vec![revision.id.clone()],
        "the later revision waits for the held one"
    );
    Held {
        db,
        root_id,
        revision_id: revision.id,
        run_id: run.id,
        later_id: later.id,
    }
}

impl Held {
    /// A pre-merge batch for the root at `head`, admitted for the root alone:
    /// an explicit review start, or another revision's push to the same PR.
    fn batch_for_root(&self, head: &str) -> ReviewBatch {
        match self
            .db
            .create_pre_merge_review_batch(
                batch_input(self.root_id.clone(), head, ReviewBatchPhase::PreMerge),
                REPO,
            )
            .unwrap()
        {
            ReviewBatchDispatch::Created { batch, executions } => {
                for execution in &executions {
                    self.db.mark_execution_redundant(&execution.id).unwrap();
                }
                batch
            }
            other => panic!("expected a new batch, got {other:?}"),
        }
    }

    /// The batch reaches a terminal status without the decision path running:
    /// what a decision made before the revision awaited this head looks like
    /// to the revision afterwards.
    fn decided_earlier(&self, batch_id: &str, status: &str) {
        sql(
            &self.db,
            "UPDATE pr_review_batches SET status = ?2, completed_at = '3' WHERE id = ?1",
            params![batch_id, status],
        );
    }

    /// Both reviewers exhaust their retry and the batch fails through the
    /// quorum machine, which releases the revisions held for its head.
    fn fail_through_quorum(&self, batch_id: &str) {
        sql(
            &self.db,
            "UPDATE pr_review_batch_members SET status = 'failed', attempt = 2 WHERE batch_id = ?1",
            [batch_id],
        );
        assert!(matches!(
            advance_quorum(&self.db, batch_id),
            ReviewBatchQuorumOutcome::InsufficientQuorum
        ));
    }

    fn settle(&self, head: &str) -> ReviewHoldSettlement {
        self.db.settle_revision_review_hold(&self.revision_id, head).unwrap()
    }

    fn status(&self) -> TaskStatus {
        match self.db.get_work_item(&self.revision_id).unwrap() {
            WorkItem::Task(task) | WorkItem::Chore(task) => task.status,
            other => panic!("expected a task, got {other:?}"),
        }
    }

    fn awaited_head(&self) -> Option<String> {
        self.db.get_revision_stop_contributed_head(&self.run_id).unwrap()
    }

    /// The later revision is no longer waiting on anything and its run can
    /// be claimed.
    fn assert_later_revision_is_dispatchable(&self) {
        assert!(
            self.db.gating_prereqs_for(&self.later_id).unwrap().is_empty(),
            "nothing gates the later revision once the held one is released"
        );
        let runs = self.db.list_executions(Some(&self.later_id)).unwrap();
        let pending: Vec<_> = runs.iter().filter(|run| !run.status.is_terminal()).collect();
        assert_eq!(pending.len(), 1, "one pending run for the later revision: {runs:?}");
        assert_eq!(pending[0].status, ExecutionStatus::Ready);
    }

    fn assert_later_revision_still_waits(&self) {
        assert_eq!(
            self.db.gating_prereqs_for(&self.later_id).unwrap(),
            vec![self.revision_id.clone()]
        );
    }
}

/// The batch for the pushed head was admitted for another row and reached its
/// verdict before this revision was held for that head. Its decision found no
/// revision to release, so nothing pending will. Settling releases it, and
/// the revision queued behind it becomes dispatchable in the same step.
#[test]
fn a_revision_held_after_its_head_was_reviewed_is_released_when_settled() {
    let held = held("hold-settle-after-verdict");
    let batch = held.batch_for_root(HEAD);
    held.decided_earlier(&batch.id, "completed");
    assert_eq!(held.status(), TaskStatus::Active);

    assert_eq!(
        held.settle(HEAD),
        ReviewHoldSettlement::Released {
            batch_id: batch.id,
            batch_status: ReviewBatchStatus::Completed,
        }
    );

    assert_eq!(held.status(), TaskStatus::InReview);
    assert_eq!(held.awaited_head().as_deref(), Some(HEAD));
    held.assert_later_revision_is_dispatchable();
}

/// A failed batch is a terminal decision too, and it carries no verdict.
/// A release that asked for one would leave this revision held for good.
#[test]
fn a_revision_held_after_its_heads_batch_failed_is_released_when_settled() {
    let held = held("hold-settle-after-failure");
    let batch = held.batch_for_root(HEAD);
    held.decided_earlier(&batch.id, "failed");

    assert!(matches!(
        held.settle(HEAD),
        ReviewHoldSettlement::Released {
            batch_status: ReviewBatchStatus::Failed,
            ..
        }
    ));

    assert_eq!(held.status(), TaskStatus::InReview);
    held.assert_later_revision_is_dispatchable();
}

/// A quorum failure is a terminal decision: it releases the held revision,
/// makes the revision queued behind it dispatchable, and publishes the
/// dependent's board change so the card is no longer shown as gated.
#[tokio::test]
async fn quorum_failure_unblocks_the_queued_revision_and_publishes_its_board_change() {
    use boss_event_bus::{Event, EventKind, TopicFilter};
    let held = held("quorum-fail-board-event");
    let batch = held.batch_for_root(HEAD);
    assert!(matches!(held.settle(HEAD), ReviewHoldSettlement::AwaitingBatch { .. }));
    let later = match held.db.get_work_item(&held.later_id).unwrap() {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("expected a task, got {other:?}"),
    };
    let mut sub = held
        .db
        .event_bus()
        .subscribe(TopicFilter::kind(EventKind::WorkItemChanged));
    held.fail_through_quorum(&batch.id);
    held.assert_later_revision_is_dispatchable();
    let mut saw_later = false;
    for _ in 0..8 {
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), sub.recv())
            .await
            .expect("WorkItemChanged should publish when a quorum failure unblocks a dependent")
            .unwrap();
        if let Event::WorkItemChanged { task_id, product_id } = event
            && task_id == held.later_id
        {
            assert_eq!(product_id, later.product_id);
            saw_later = true;
            break;
        }
    }
    assert!(
        saw_later,
        "the queued revision's board change must publish on quorum failure"
    );
}

/// While the batch for the head is still running, settling only records the
/// head. The batch's own decision then releases the revision.
#[test]
fn settling_against_a_running_batch_holds_until_its_decision() {
    let held = held("hold-settle-running");
    let batch = held.batch_for_root(HEAD);
    assert_eq!(held.awaited_head(), None, "nothing named the head yet");

    assert_eq!(
        held.settle(HEAD),
        ReviewHoldSettlement::AwaitingBatch {
            batch_id: batch.id.clone()
        }
    );
    assert_eq!(held.status(), TaskStatus::Active);
    assert_eq!(held.awaited_head().as_deref(), Some(HEAD));
    held.assert_later_revision_still_waits();

    held.fail_through_quorum(&batch.id);

    assert_eq!(held.status(), TaskStatus::InReview);
    held.assert_later_revision_is_dispatchable();
}

/// With no batch for the head there is nothing to settle against. The head is
/// still recorded, so the batch admitted for it later releases the revision.
#[test]
fn settling_without_a_batch_records_the_head_and_holds() {
    let held = held("hold-settle-no-batch");

    assert_eq!(held.settle(HEAD), ReviewHoldSettlement::NoBatch);

    assert_eq!(held.status(), TaskStatus::Active);
    assert_eq!(held.awaited_head().as_deref(), Some(HEAD));
    held.assert_later_revision_still_waits();
}

/// A decision for some other head does not release the revision: what it
/// pushed is not what was reviewed.
#[test]
fn a_decision_for_another_head_does_not_release_the_revision() {
    let held = held("hold-settle-other-head");
    let batch = held.batch_for_root("earlier-head");
    held.decided_earlier(&batch.id, "completed");

    assert_eq!(held.settle(HEAD), ReviewHoldSettlement::NoBatch);

    assert_eq!(held.status(), TaskStatus::Active);
    held.assert_later_revision_still_waits();
}

/// A revision whose worker is running again is not held: its run decides
/// where it goes when it ends.
#[test]
fn a_revision_with_a_live_run_is_not_released() {
    let held = held("hold-settle-live-run");
    let batch = held.batch_for_root(HEAD);
    held.decided_earlier(&batch.id, "completed");
    held.db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(held.revision_id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();

    assert_eq!(held.settle(HEAD), ReviewHoldSettlement::NotHeld);

    assert_eq!(held.status(), TaskStatus::Active);
}

/// The cycle root is not a revision and holds nothing of this kind.
#[test]
fn the_cycle_root_has_no_revision_hold_to_settle() {
    let held = held("hold-settle-root");
    let batch = held.batch_for_root(HEAD);
    held.decided_earlier(&batch.id, "completed");

    assert_eq!(
        held.db.settle_revision_review_hold(&held.root_id, HEAD).unwrap(),
        ReviewHoldSettlement::NotHeld
    );
    assert_eq!(held.status(), TaskStatus::Active);
}

/// Reaping an inert batch fails it, which is a terminal decision for its
/// head like any other failure. The revision held for that head is released
/// with it, and the one queued behind becomes dispatchable.
#[test]
fn reaping_a_batch_releases_the_revision_held_for_its_head() {
    let held = held("hold-settle-reaped");
    let batch = held.batch_for_root(HEAD);
    assert!(matches!(held.settle(HEAD), ReviewHoldSettlement::AwaitingBatch { .. }));
    sql(
        &held.db,
        "UPDATE pr_review_batch_members SET status = 'failed', attempt = 2 WHERE batch_id = ?1",
        [&batch.id],
    );
    sql(
        &held.db,
        "UPDATE pr_review_batches SET updated_at = '1' WHERE id = ?1",
        [&batch.id],
    );

    assert_eq!(held.db.reap_inert_review_batches(0).unwrap(), vec![batch.id]);

    assert_eq!(held.status(), TaskStatus::InReview);
    held.assert_later_revision_is_dispatchable();
}
