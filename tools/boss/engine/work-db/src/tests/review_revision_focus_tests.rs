//! Reviews of a revision's push while the row that owns the PR is outside
//! dispatch focus: admission, the scope of the batch's members and guide,
//! and the hold on the revision.
//!
//! The shape throughout is a PR-owning followup row with one revision that
//! has pushed to that PR and is held `active` for review. The followup is
//! never launched and never in the focus set.

use boss_protocol::{DispatchFocus, DispatchFocusTargetKind, ExecutionStatus, RequestExecutionInput, ReviewBatchPhase};

use super::*;

const REPO: &str = "https://github.com/example/repo";
const PR_URL: &str = "https://github.com/example/repo/pull/42";

/// One statement on a connection released before returning: `WorkDb` hands
/// out a single connection, so a guard must never be held across a call.
fn sql(db: &WorkDb, statement: &str, params: impl rusqlite::Params) {
    db.connect().unwrap().execute(statement, params).unwrap();
}

struct Cycle {
    db: WorkDb,
    focus: DispatchFocus,
    root_id: String,
    revision_id: String,
    /// The revision's finished `revision_implementation` run.
    implementation_id: String,
}

/// `launched` says whether the revision was started with `bossctl agents
/// launch`, the only thing that can put it in scope here.
fn cycle(label: &str, launched: bool) -> Cycle {
    cycle_in(WorkDb::open(temp_db_path(label)).unwrap(), launched)
}

fn cycle_in(db: WorkDb, launched: bool) -> Cycle {
    let product = create_test_product(&db);
    let mut focus = DispatchFocus::default();
    focus.insert(DispatchFocusTargetKind::Project, "proj_elsewhere".to_owned());
    db.save_dispatch_focus(&focus).unwrap();

    let origin = create_test_chore_manual(&db, &product.id, "Merged origin");
    db.update_work_item(
        &origin.id,
        WorkItemPatch {
            pr_url: Some("https://github.com/example/repo/pull/7".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let root = db
        .create_review_findings_followup(
            crate::ReviewFindingsFollowupInsert::builder()
                .product_id(&product.id)
                .name("Review findings")
                .created_via("pr_review:post_merge:test")
                .description("Fix the finding")
                .chain_root_id(origin.id.clone())
                .autostart(false)
                .build(),
        )
        .unwrap();
    sql(
        &db,
        "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
        params![root.id, PR_URL],
    );

    let (revision_id, implementation_id) = hold_revision(&db, &product.id, &root.id, launched);
    assert_eq!(
        db.work_item_in_dispatch_focus(&revision_id, &focus).unwrap(),
        launched,
        "only the launch puts the revision in scope"
    );
    assert!(!db.work_item_in_dispatch_focus(&root.id, &focus).unwrap());
    Cycle {
        db,
        focus,
        root_id: root.id,
        revision_id,
        implementation_id,
    }
}

/// A revision of `root_id` whose run pushed, ended, and left it held `active`
/// for review. Returns the revision and its finished run.
fn hold_revision(db: &WorkDb, product_id: &str, root_id: &str, launched: bool) -> (String, String) {
    let revision_id = insert_revision_row(db, product_id, root_id);
    let implementation = db
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&revision_id)
                .force(launched)
                .build(),
        )
        .unwrap();
    assert_eq!(implementation.kind, ExecutionKind::RevisionImplementation);
    sql(
        db,
        "UPDATE work_executions SET status = 'completed', finished_at = '2' WHERE id = ?1",
        [&implementation.id],
    );
    sql(
        db,
        "UPDATE tasks SET status = 'active', pr_url = ?2 WHERE id = ?1",
        params![revision_id, PR_URL],
    );
    (revision_id, implementation.id)
}

impl Cycle {
    /// Automatic admission for the revision's push at `head`, as its
    /// completion and the deferred-admission sweep both request it.
    fn admit_for_revision(&self, head: &str) -> ReviewBatchDispatch {
        self.admit_for(&self.revision_id, head)
    }

    fn admit_for(&self, revision_id: &str, head: &str) -> ReviewBatchDispatch {
        let mut input = batch_input(self.root_id.clone(), head, ReviewBatchPhase::PreMerge);
        input.legacy_task_id = Some(revision_id.to_owned());
        self.db.create_pre_merge_review_batch(input, REPO).unwrap()
    }

    /// Both reviewers of `batch_id` exhaust their retry and the batch fails.
    fn fail_batch(&self, batch_id: &str) {
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

    /// Admission naming only the root: the sweep's readmission arm.
    fn admit_for_root(&self, head: &str) -> ReviewBatchDispatch {
        self.db
            .create_pre_merge_review_batch(
                batch_input(self.root_id.clone(), head, ReviewBatchPhase::PreMerge),
                REPO,
            )
            .unwrap()
    }

    /// `bossctl review start`, which is always admitted against the root.
    fn start_review_for_root(&self, head: &str) -> ReviewBatchDispatch {
        self.db
            .request_pre_merge_review_batch_for_pool(
                batch_input(self.root_id.clone(), head, ReviewBatchPhase::PreMerge),
                16,
            )
            .unwrap()
    }

    fn outside_focus(&self, executions: &[WorkExecution]) -> HashSet<String> {
        self.db
            .executions_outside_dispatch_focus(executions, &self.focus)
            .unwrap()
    }

    /// Work items carrying an open review-admission-deferred marker.
    fn deferred_markers(&self) -> Vec<String> {
        self.db
            .list_open_attention_items_of_kind(PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)
            .unwrap()
            .into_iter()
            .filter_map(|item| item.work_item_id)
            .collect()
    }

    fn awaited_head(&self) -> Option<String> {
        self.db
            .get_revision_stop_contributed_head(&self.implementation_id)
            .unwrap()
    }

    fn revision_status(&self) -> TaskStatus {
        self.status_of(&self.revision_id)
    }

    fn status_of(&self, task_id: &str) -> TaskStatus {
        self.task(task_id).status
    }

    fn task(&self, task_id: &str) -> Task {
        match self.db.get_work_item(task_id).unwrap() {
            WorkItem::Task(task) | WorkItem::Chore(task) => task,
            other => panic!("expected a task, got {other:?}"),
        }
    }

    /// One more revision of the root, held like the first and not launched.
    fn hold_another_revision(&self) -> String {
        let product_id = self.task(&self.root_id).product_id;
        hold_revision(&self.db, &product_id, &self.root_id, false).0
    }
}

fn created(dispatch: ReviewBatchDispatch) -> (ReviewBatch, Vec<WorkExecution>) {
    match dispatch {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected a new batch, got {other:?}"),
    }
}

/// The launch on the revision admits the review of what it pushed: the batch
/// is created, keeps its reservation, and every member may dispatch. The root
/// is not admitted with it.
#[test]
fn a_launched_revisions_push_is_reviewed_while_its_root_stays_out_of_focus() {
    let cycle = cycle("revision-focus-launched", true);
    let db = &cycle.db;
    assert_eq!(
        db.pre_merge_review_hold_without_history(&cycle.root_id, &cycle.revision_id)
            .unwrap(),
        None
    );

    let (batch, executions) = created(cycle.admit_for_revision("head-a"));

    assert_eq!(
        batch.producing_work_item_id.as_deref(),
        Some(cycle.revision_id.as_str())
    );
    assert_eq!(executions.len(), 2);
    for execution in &executions {
        assert_eq!(execution.work_item_id, cycle.root_id, "members are keyed on the root");
    }
    assert!(cycle.outside_focus(&executions).is_empty());
    assert!(
        db.release_unstartable_pre_merge_review_reservations()
            .unwrap()
            .is_empty(),
        "a batch whose members can dispatch keeps its reservation"
    );
    assert!(db.review_batch(&batch.id).unwrap().is_some());
    assert!(cycle.deferred_markers().is_empty());

    // The root's own review, at a head no launched revision pushed, is
    // still held, and so is a reviewer on the root outside this batch.
    assert!(matches!(
        cycle.admit_for_root("head-b"),
        ReviewBatchDispatch::AdmissionDeferred
    ));
    let stray = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&cycle.root_id)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    assert_eq!(
        cycle.outside_focus(std::slice::from_ref(&stray)),
        HashSet::from([stray.id])
    );
}

/// Control for the test above: without the launch the same push is deferred.
#[test]
fn an_unlaunched_revisions_push_stays_deferred_by_focus() {
    let cycle = cycle("revision-focus-unlaunched", false);
    let db = &cycle.db;
    assert_eq!(
        db.pre_merge_review_hold_without_history(&cycle.root_id, &cycle.revision_id)
            .unwrap(),
        Some("focus_mode")
    );

    assert!(matches!(
        cycle.admit_for_revision("head-a"),
        ReviewBatchDispatch::AdmissionDeferred
    ));

    assert!(db.review_batches_for_cycle_root(&cycle.root_id).unwrap().is_empty());
    db.file_admission_deferred_attention(&cycle.revision_id, PR_URL)
        .unwrap();
    let marker = db
        .list_open_attention_items_of_kind(PR_REVIEW_ADMISSION_DEFERRED_ATTENTION_KIND)
        .unwrap()
        .pop()
        .expect("the deferral is marked on the revision");
    assert_eq!(marker.title, "Automated reviewer: held by focus mode");
}

/// The supervisor is added to the batch after its leaves report, as one more
/// reviewer keyed on the root. It follows the batch through focus too.
#[test]
fn the_supervisor_of_a_launched_revisions_review_dispatches_through_focus() {
    let cycle = cycle("revision-focus-supervisor", true);
    let db = &cycle.db;
    let (batch, executions) = created(cycle.admit_for_revision("head-a"));
    for execution in &executions {
        submit_review_report(db, &execution.id, &cycle.root_id, &batch.id, "head-a", "report");
    }
    let supervisor = db
        .review_batch_members(&batch.id)
        .unwrap()
        .into_iter()
        .find(|member| member.role == ReviewBatchMemberRole::Supervisor)
        .and_then(|member| member.execution_id)
        .expect("both reports dispatch the supervisor");
    let supervisor = db.get_execution(&supervisor).unwrap();

    assert_eq!(supervisor.work_item_id, cycle.root_id);
    assert!(cycle.outside_focus(&[supervisor]).is_empty());
}

/// A generation minted to replace a failed one reviews the same push, so it
/// is admitted for the same revision even though the request names the root.
#[test]
fn a_reminted_generation_keeps_the_launched_revision_it_reviews_for() {
    let cycle = cycle("revision-focus-remint", true);
    let db = &cycle.db;
    let (failed, executions) = created(cycle.admit_for_revision("head-a"));
    for execution in &executions {
        db.mark_execution_redundant(&execution.id).unwrap();
    }
    sql(
        db,
        "UPDATE pr_review_batch_members SET status = 'failed' WHERE batch_id = ?1",
        [&failed.id],
    );
    sql(
        db,
        "UPDATE pr_review_batches SET status = 'failed' WHERE id = ?1",
        [&failed.id],
    );

    let (reminted, executions) = created(cycle.admit_for_root("head-a"));

    assert_eq!(reminted.generation, 2);
    assert_eq!(
        reminted.producing_work_item_id.as_deref(),
        Some(cycle.revision_id.as_str())
    );
    assert!(cycle.outside_focus(&executions).is_empty());
}

/// The review guide describes one head of the root's PR. It is in scope once
/// a batch ties that head to the launched revision, and only for that head.
#[test]
fn the_review_guide_follows_the_review_of_the_head_it_describes() {
    for (reviewed_head, in_scope) in [("head-a", true), ("head-b", false)] {
        // Guide source packets are stored beside the database file.
        let dir = tempfile::tempdir().unwrap();
        let cycle = cycle_in(WorkDb::open(dir.path().join("boss.db")).unwrap(), true);
        let db = &cycle.db;
        let (_, comparison) =
            crate::test_support::seed_review_guide_series_for_pr(db, &cycle.root_id, PR_URL, "base", "head-a");
        let guide = db
            .create_pr_review_guide_execution(&comparison, "example/repo")
            .unwrap();
        assert_eq!(
            cycle.outside_focus(std::slice::from_ref(&guide)).len(),
            1,
            "nothing ties the head to the revision before its review is admitted"
        );

        created(cycle.admit_for_revision(reviewed_head));

        assert_eq!(
            cycle.outside_focus(&[guide]).is_empty(),
            in_scope,
            "guide for head-a, review of {reviewed_head}"
        );
    }
}

/// A deferral creates no batch, so it has to leave behind the head the
/// revision is held for. A retry after the PR has moved re-points it: the
/// batch that settles the revision's push will be admitted at the new head.
#[test]
fn a_deferred_admission_records_the_head_the_revision_is_held_for() {
    let cycle = cycle("revision-focus-awaited-head", false);
    assert_eq!(
        cycle.awaited_head(),
        None,
        "a run that ended by declaring delivery stamps none"
    );

    assert!(matches!(
        cycle.admit_for_revision("head-a"),
        ReviewBatchDispatch::AdmissionDeferred
    ));
    assert_eq!(cycle.awaited_head().as_deref(), Some("head-a"));

    assert!(matches!(
        cycle.admit_for_revision("head-b"),
        ReviewBatchDispatch::AdmissionDeferred
    ));
    assert_eq!(cycle.awaited_head().as_deref(), Some("head-b"));
}

/// The PR moves while the revision's review is deferred. The batch finally
/// admitted is at the head the PR has by then, and its decision (here a
/// failure, which no later verdict would make up for) releases the revision.
#[test]
fn a_batch_at_the_head_the_pr_moved_to_releases_the_revision_deferred_before_it() {
    let cycle = cycle("revision-focus-moved-head", false);
    let db = &cycle.db;
    assert!(matches!(
        cycle.admit_for_revision("head-a"),
        ReviewBatchDispatch::AdmissionDeferred
    ));
    db.save_dispatch_focus(&DispatchFocus::default()).unwrap();

    let (batch, _) = created(cycle.admit_for_revision("head-b"));
    cycle.fail_batch(&batch.id);

    assert_eq!(cycle.revision_status(), TaskStatus::InReview);
}

/// Two revisions of one PR are both deferred. One review of the head they
/// share releases both, not whichever the query happens to return first.
#[test]
fn every_revision_held_for_a_head_is_released_when_it_settles() {
    let cycle = cycle("revision-focus-two-held", false);
    let second = cycle.hold_another_revision();
    for revision in [&cycle.revision_id, &second] {
        assert!(matches!(
            cycle.admit_for(revision, "head-a"),
            ReviewBatchDispatch::AdmissionDeferred
        ));
    }

    let (batch, _) = created(cycle.start_review_for_root("head-a"));
    cycle.fail_batch(&batch.id);

    assert_eq!(cycle.revision_status(), TaskStatus::InReview);
    assert_eq!(cycle.status_of(&second), TaskStatus::InReview);
}

/// Admission can run before the revision's run is terminal (the Stop path
/// admits first). That run records its own head at its Stop, and an older
/// finished run must not be stamped in its place.
#[test]
fn admission_does_not_record_a_head_while_the_revisions_newest_run_is_live() {
    let cycle = cycle("revision-focus-live-run", false);
    let db = &cycle.db;
    let live = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&cycle.revision_id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    sql(
        db,
        "UPDATE work_executions SET status = 'waiting_human' WHERE id = ?1",
        [&live.id],
    );

    assert!(matches!(
        cycle.admit_for_revision("head-a"),
        ReviewBatchDispatch::AdmissionDeferred
    ));

    assert_eq!(db.get_revision_stop_contributed_head(&live.id).unwrap(), None);
    assert_eq!(cycle.awaited_head(), None);
}

/// The marker sits on the revision that was deferred, the explicit start is
/// admitted against the root. Admission clears it wherever it sits.
#[test]
fn a_review_started_for_the_root_clears_the_marker_on_the_deferred_revision() {
    let cycle = cycle("revision-focus-marker", false);
    let db = &cycle.db;
    assert!(matches!(
        cycle.admit_for_revision("head-a"),
        ReviewBatchDispatch::AdmissionDeferred
    ));
    db.file_admission_deferred_attention(&cycle.revision_id, PR_URL)
        .unwrap();
    assert_eq!(cycle.deferred_markers(), vec![cycle.revision_id.clone()]);

    let (batch, _) = created(cycle.start_review_for_root("head-a"));

    assert!(batch.explicit);
    assert_eq!(batch.producing_work_item_id, None);
    assert!(cycle.deferred_markers().is_empty());
}

/// A batch that fails is a terminal review decision too. Started for the
/// root, it still releases the revision that was deferred for that head.
#[test]
fn a_failed_review_started_for_the_root_releases_the_deferred_revision() {
    let cycle = cycle("revision-focus-failed-batch", false);
    let db = &cycle.db;
    assert!(matches!(
        cycle.admit_for_revision("head-a"),
        ReviewBatchDispatch::AdmissionDeferred
    ));
    let (batch, _) = created(cycle.start_review_for_root("head-a"));
    // Re-raised while the batch runs, to show the release clears it.
    db.file_admission_deferred_attention(&cycle.revision_id, PR_URL)
        .unwrap();

    cycle.fail_batch(&batch.id);

    assert_eq!(cycle.revision_status(), TaskStatus::InReview);
    assert!(cycle.deferred_markers().is_empty());
}
