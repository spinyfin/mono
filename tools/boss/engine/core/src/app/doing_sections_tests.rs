use super::*;
use crate::test_support::{create_test_chore_manual, create_test_product, open_db, seed_review_guide_series};
use boss_protocol::{ExecutionStatus, RequestExecutionInput, WorkItemBinding, WorkerActivity};

fn worker(id: &str, kind: ExecutionKind, activity: WorkerActivity) -> LiveWorkerState {
    let mut state = LiveWorkerState::new_spawning_with_routing(
        1,
        "execution",
        "model",
        0,
        Some(WorkItemBinding {
            work_item_id: id.into(),
            work_item_name: "item".into(),
            execution_id: "execution".into(),
        }),
        Some("automation".into()),
        Some(kind.as_str().into()),
    );
    state.activity = activity;
    state
}

#[test]
fn every_execution_kind_and_activity_has_a_section() {
    use ExecutionKind::*;
    use WorkerActivity::*;
    let (_dir, db) = open_db();
    for (kind, live_section) in [
        (TaskImplementation, DoingSection::Coding),
        (ChoreImplementation, DoingSection::Coding),
        (RevisionImplementation, DoingSection::Coding),
        (InvestigationImplementation, DoingSection::Coding),
        (CiRemediation, DoingSection::Coding),
        (ConflictResolution, DoingSection::Coding),
        (ProjectDesign, DoingSection::Coding),
        (ProductDesign, DoingSection::Coding),
        (AutomationTriage, DoingSection::Coding),
        (AnswerAgent, DoingSection::Coding),
        (PrReview, DoingSection::Reviewing),
        (PrReviewGuide, DoingSection::Reviewing),
    ] {
        for activity in [Spawning, Working, Idle, WaitingForInput, Errored, Terminated] {
            let state = worker("task_card", kind.clone(), activity);
            let map = classify_doing_sections(&db, &[state]);
            let actual = map.get("task_card").copied().unwrap_or_default();
            let expected = if activity.is_terminal() {
                DoingSection::Queued
            } else {
                live_section
            };
            assert_eq!(actual, expected, "{kind:?}, {activity:?}");
        }
    }
}

#[test]
fn coding_wins_in_either_snapshot_order_and_release_reveals_review() {
    let (_dir, db) = open_db();
    let coding = worker("task_card", ExecutionKind::ConflictResolution, WorkerActivity::Idle);
    let review = worker("task_card", ExecutionKind::PrReview, WorkerActivity::Working);
    for states in [
        vec![coding.clone(), review.clone()],
        vec![review.clone(), coding.clone()],
    ] {
        assert_eq!(classify_doing_sections(&db, &states)["task_card"], DoingSection::Coding);
    }
    assert_eq!(
        classify_doing_sections(&db, &[review])["task_card"],
        DoingSection::Reviewing
    );
    assert!(classify_doing_sections(&db, &[]).is_empty());
}

#[test]
fn dispatch_status_and_wait_reasons_never_invent_a_live_worker() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let task = create_test_chore_manual(&db, product.id.clone(), "queued card");
    let execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(&task.id).build())
        .unwrap();
    for status in [
        ExecutionStatus::Queued,
        ExecutionStatus::Ready,
        ExecutionStatus::WaitingDependency,
        ExecutionStatus::Claimed,
        ExecutionStatus::Running,
        ExecutionStatus::WaitingHuman,
        ExecutionStatus::WaitingReview,
        ExecutionStatus::WaitingMerge,
        ExecutionStatus::Completed,
        ExecutionStatus::Failed,
        ExecutionStatus::Abandoned,
        ExecutionStatus::Cancelled,
        ExecutionStatus::Orphaned,
    ] {
        for reason in [
            None,
            Some("dispatch_paused"),
            Some("automation_paused"),
            Some("pool_exhausted"),
            Some("waiting_dependency"),
            Some("not_before"),
            Some("unknown"),
        ] {
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE work_executions SET status = ?1, dispatch_wait_reason = ?2 WHERE id = ?3",
                    rusqlite::params![status.as_str(), reason, execution.id],
                )
                .unwrap();
            // An adopted slot (no binding) makes the classifier read this
            // execution row, so the status and wait reason are really loaded.
            let mut adopted = worker(&task.id, ExecutionKind::ChoreImplementation, WorkerActivity::Terminated);
            adopted.run_id = execution.id.clone();
            adopted.work_item_id = None;
            adopted.kind = None;
            assert!(
                classify_doing_sections(&db, std::slice::from_ref(&adopted)).is_empty(),
                "{status:?}, {reason:?}: terminal slot is Queued"
            );
            adopted.activity = WorkerActivity::Working;
            assert_eq!(
                classify_doing_sections(&db, &[adopted])[&task.id],
                DoingSection::Coding,
                "{status:?}, {reason:?}: live slot is Coding"
            );
        }
    }
    // A live slot remains authoritative even if bookkeeping is terminal.
    let state = worker(&task.id, ExecutionKind::ChoreImplementation, WorkerActivity::Working);
    assert_eq!(classify_doing_sections(&db, &[state])[&task.id], DoingSection::Coding);
}

#[test]
fn review_guide_comparison_resolves_to_owning_card_including_after_adoption() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let task = create_test_chore_manual(&db, product.id.clone(), "review guide card");
    let (_, comparison) = seed_review_guide_series(&db, &task.id);
    let state = worker(&comparison, ExecutionKind::PrReviewGuide, WorkerActivity::Working);
    assert_eq!(
        classify_doing_sections(&db, std::slice::from_ref(&state)),
        BTreeMap::from([(task.id.clone(), DoingSection::Reviewing)])
    );

    let execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(&task.id).build())
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET kind = 'pr_review_guide', work_item_id = ?1 WHERE id = ?2",
            rusqlite::params![comparison, execution.id],
        )
        .unwrap();
    let mut adopted = state;
    adopted.run_id = execution.id;
    adopted.work_item_id = None;
    adopted.kind = None;
    assert_eq!(
        classify_doing_sections(&db, &[adopted])[&task.id],
        DoingSection::Reviewing
    );
}

#[test]
fn unbound_slots_do_not_create_cards_and_untyped_bound_slots_remain_active() {
    let (_dir, db) = open_db();
    let mut state = LiveWorkerState::new_spawning(1, "missing", "model", 0, None);
    assert!(classify_doing_sections(&db, std::slice::from_ref(&state)).is_empty());
    state.work_item_id = Some("task_card".into());
    assert_eq!(
        classify_doing_sections(&db, &[state])["task_card"],
        DoingSection::Coding
    );
}
