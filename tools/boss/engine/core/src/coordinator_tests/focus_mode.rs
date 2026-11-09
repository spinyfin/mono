//! Dispatch focus mode (`bossctl dispatch focus`): while the focus set is
//! non-empty, `drain_ready_queue` admits only rows whose work is in a focused
//! project or product (or whose revision/followup lineage roots in one) and
//! holds the rest with the `focus_mode` wait reason.
//!
//! These tests pin the admission contract the feature promises:
//!
//! - in-scope rows dispatch, out-of-scope rows are held and left untouched;
//! - review revisions and post-merge followups follow their lineage;
//! - the global pause overrides focus, and focus never admits what it holds;
//! - `bossctl agents launch` (`force_dispatch`) bypasses focus;
//! - the interactive concurrency cap still applies to in-scope rows;
//! - answering a worker's question re-dispatches an in-scope row;
//! - PR-review runs follow scope, and are held rather than dropped;
//! - clearing focus (or adding the project) releases held rows.
//!
//! Shared fixtures live in [`super::helpers`].

use std::collections::HashSet;

use super::helpers::*;

use crate::dispatch_events::RecordingDispatchEventSink;
use crate::spawn_health::{SpawnHealthTracker, maybe_admit_recovery_probe};
use crate::work::{ReviewBatchCreateInput, ReviewBatchDispatch};
use boss_protocol::{
    DISPATCH_WAIT_REASON_FOCUS_MODE, DispatchFocus, DispatchFocusTargetKind, DispatchPauseScope, ExecutionKind,
    OperatorAnswer, PauseReason, Project, ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket, ReviewProfile,
};

mod explicit_launch;

struct Harness {
    _dir: tempfile::TempDir,
    db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    runner: Arc<FakeExecutionRunner>,
    product_id: String,
}

fn harness() -> Harness {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let mut coordinator = ExecutionCoordinator::new(db.clone(), WorkerPool::new(8), cube, runner.clone());
    coordinator.set_review_pool(WorkerPool::new_review(2));
    Harness {
        _dir: dir,
        db,
        coordinator: Arc::new(coordinator),
        runner,
        product_id: product.id,
    }
}

impl Harness {
    fn project(&self, name: &str) -> Project {
        self.db
            .create_project(CreateProjectInput {
                product_id: self.product_id.clone(),
                name: name.to_owned(),
                description: None,
                goal: None,
                autostart: false,
                no_design_task: true,
                design_reasoning_effort_xhigh: false,
            })
            .unwrap()
    }

    /// A task in `project` with a `ready` implementation execution.
    fn ready_project_task(&self, project: &Project, name: &str) -> (String, String) {
        let task = self
            .db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(self.product_id.clone())
                    .project_id(project.id.clone())
                    .name(name)
                    .autostart(false)
                    .build(),
            )
            .unwrap();
        let execution = self.ready_execution(&task.id, ExecutionKind::TaskImplementation);
        (task.id, execution)
    }

    /// A product-level chore (in no project) with a `ready` execution.
    fn ready_loose_chore(&self, name: &str) -> (String, String) {
        let chore = create_test_chore_manual(&self.db, self.product_id.clone(), name);
        let execution = self.ready_execution(&chore.id, ExecutionKind::ChoreImplementation);
        (chore.id, execution)
    }

    fn ready_execution(&self, work_item_id: &str, kind: ExecutionKind) -> String {
        self.db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(work_item_id.to_owned())
                    .kind(kind)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap()
            .id
    }

    /// A revision row hanging off `parent_id`, with the project columns the
    /// engine leaves empty on revisions.
    fn revision_of(&self, parent_id: &str, id: &str) -> String {
        self.db
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 SELECT ?1, product_id, 'revision', 'Fix review findings', '', 'todo', '1', '1', ?2
                 FROM tasks WHERE id = ?2",
                rusqlite::params![id, parent_id],
            )
            .unwrap();
        id.to_owned()
    }

    fn set_pr_url(&self, task_id: &str, pr_url: &str) {
        self.db
            .update_work_item(
                task_id,
                WorkItemPatch {
                    pr_url: Some(pr_url.to_owned()),
                    ..WorkItemPatch::default()
                },
            )
            .unwrap();
    }

    fn focus_on(&self, projects: &[&Project]) {
        let mut focus = DispatchFocus::default();
        for project in projects {
            focus.insert(DispatchFocusTargetKind::Project, project.id.clone());
        }
        self.coordinator.restore_dispatch_focus(focus);
    }

    fn clear_focus(&self) {
        self.coordinator.restore_dispatch_focus(DispatchFocus::default());
    }

    fn execution(&self, id: &str) -> WorkExecution {
        self.db.get_execution(id).unwrap()
    }

    /// Run one drain pass, then wait until every id in `dispatched` has
    /// reached the runner. Dispatch hands the slow tail off to a task, so a
    /// row's absence can only be asserted once the rows that should dispatch
    /// have.
    async fn drain_and_wait_for(&self, dispatched: &[&str]) {
        self.coordinator.drain_ready_queue_for_test().await;
        self.wait_for_runner(dispatched).await;
    }

    /// Wait until the runner has been handed every execution in `ids`.
    async fn wait_for_runner(&self, ids: &[&str]) {
        for id in ids {
            wait_for_execution_status(&self.db, id, ExecutionStatus::Running).await;
        }
        for _ in 0..3000 {
            let dispatched = self.dispatched().await;
            if ids.iter().all(|id| dispatched.contains(*id)) {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("the runner was never handed {ids:?}");
    }

    async fn dispatched(&self) -> HashSet<String> {
        self.runner
            .calls
            .lock()
            .await
            .iter()
            .map(|call| call.1.clone())
            .collect()
    }

    fn assert_held_by_focus(&self, execution_id: &str) {
        let held = self.execution(execution_id);
        assert_eq!(held.status, ExecutionStatus::Ready, "a held row stays queued");
        assert_eq!(
            held.dispatch_wait_reason.as_deref(),
            Some(DISPATCH_WAIT_REASON_FOCUS_MODE),
            "a held row names why it is held",
        );
    }
}

/// The task row as the board sees it, to prove focus never rewrites a row.
fn task_snapshot(db: &WorkDb, task_id: &str) -> String {
    format!("{:?}", db.get_work_item(task_id).unwrap())
}

#[tokio::test]
async fn in_scope_rows_dispatch_and_out_of_scope_rows_are_held_untouched() {
    let h = harness();
    let focused = h.project("Focused");
    let other = h.project("Other");
    let (in_scope_task, in_scope_exec) = h.ready_project_task(&focused, "In scope");
    let (other_task, other_exec) = h.ready_project_task(&other, "Other project");
    let (loose_task, loose_exec) = h.ready_loose_chore("Loose chore");
    let before = (task_snapshot(&h.db, &other_task), task_snapshot(&h.db, &loose_task));
    h.focus_on(&[&focused]);

    h.drain_and_wait_for(&[&in_scope_exec]).await;

    assert_eq!(h.dispatched().await, HashSet::from([in_scope_exec.clone()]));
    h.assert_held_by_focus(&other_exec);
    h.assert_held_by_focus(&loose_exec);
    assert_eq!(
        (task_snapshot(&h.db, &other_task), task_snapshot(&h.db, &loose_task)),
        before,
        "focus is an admission gate: it must not rewrite a held row",
    );
    let _ = in_scope_task;
}

#[tokio::test]
async fn a_focused_product_admits_loose_work_and_every_project_in_it() {
    let h = harness();
    let project = h.project("Any project");
    let (_, project_exec) = h.ready_project_task(&project, "Project task");
    let (_, loose_exec) = h.ready_loose_chore("Loose chore");
    let elsewhere = create_test_product_named(&h.db, "Elsewhere");
    let outsider = create_test_chore_manual(&h.db, elsewhere.id.clone(), "Other product");
    let outsider_exec = h.ready_execution(&outsider.id, ExecutionKind::ChoreImplementation);
    let mut focus = DispatchFocus::default();
    focus.insert(DispatchFocusTargetKind::Product, h.product_id.clone());
    h.coordinator.restore_dispatch_focus(focus);

    h.drain_and_wait_for(&[&project_exec, &loose_exec]).await;

    assert_eq!(h.dispatched().await, HashSet::from([project_exec, loose_exec]));
    h.assert_held_by_focus(&outsider_exec);
}

#[tokio::test]
async fn revision_and_followup_rows_follow_their_lineage_root() {
    let h = harness();
    let focused = h.project("Focused");
    let other = h.project("Other");
    let (root, _) = h.ready_project_task(&focused, "Root");
    let (other_root, _) = h.ready_project_task(&other, "Other root");
    h.set_pr_url(&root, "https://github.com/spinyfin/mono/pull/501");
    h.set_pr_url(&other_root, "https://github.com/spinyfin/mono/pull/502");
    // Drop the roots' own executions: only the minted rows are under test.
    h.db.connect()
        .unwrap()
        .execute("DELETE FROM work_executions", [])
        .unwrap();

    // Engine-minted review revisions carry no project; their chain root does.
    let revision = h.revision_of(&root, "task_focus_rev_in");
    let other_revision = h.revision_of(&other_root, "task_focus_rev_out");
    let revision_exec = h.ready_execution(&revision, ExecutionKind::RevisionImplementation);
    let other_revision_exec = h.ready_execution(&other_revision, ExecutionKind::RevisionImplementation);

    // A post-merge review followup that no longer sits in the root's project
    // is still in scope through the task it followed up.
    let origin =
        h.db.get_work_item(&root)
            .map(|item| match item {
                WorkItem::Task(task) | WorkItem::Chore(task) => task,
                other => panic!("expected a task, got {other:?}"),
            })
            .unwrap();
    let followup =
        h.db.create_review_findings_followup(
            crate::work::ReviewFindingsFollowupInsert::builder()
                .product_id(h.product_id.clone())
                .name("Post-merge review findings")
                .created_via("pr_review:post_merge:prop_focus")
                .description("Address the post-merge findings")
                .chain_root_id(origin.id.clone())
                .autostart(false)
                .build(),
        )
        .unwrap();
    h.db.connect()
        .unwrap()
        .execute("UPDATE tasks SET project_id = NULL WHERE id = ?1", [&followup.id])
        .unwrap();
    let followup_exec = h.ready_execution(&followup.id, ExecutionKind::ChoreImplementation);
    h.focus_on(&[&focused]);

    h.drain_and_wait_for(&[&revision_exec, &followup_exec]).await;

    assert_eq!(h.dispatched().await, HashSet::from([revision_exec, followup_exec]));
    h.assert_held_by_focus(&other_revision_exec);
}

#[tokio::test]
async fn the_global_pause_overrides_focus() {
    let h = harness();
    let focused = h.project("Focused");
    let other = h.project("Other");
    let (_, in_scope_exec) = h.ready_project_task(&focused, "In scope");
    let (_, other_exec) = h.ready_project_task(&other, "Other project");
    h.focus_on(&[&focused]);
    h.coordinator.pause_dispatch_scoped(
        1_786_385_816,
        DispatchPauseOrigin::Operator,
        DispatchPauseScope::NewWork,
        PauseReason::new("test: operator pause").unwrap(),
    );

    h.coordinator.drain_ready_queue_for_test().await;

    assert!(
        h.dispatched().await.is_empty(),
        "focus must not resume anything while paused"
    );
    for id in [&in_scope_exec, &other_exec] {
        let held = h.execution(id);
        assert_eq!(held.status, ExecutionStatus::Ready);
        assert_eq!(
            held.dispatch_wait_reason.as_deref(),
            Some("dispatch_paused"),
            "the pause keeps precedence for the reason a row shows",
        );
    }

    // Resume: the in-scope row goes, the out-of-scope row now shows focus.
    h.coordinator.resume_dispatch();
    h.drain_and_wait_for(&[&in_scope_exec]).await;
    assert_eq!(h.dispatched().await, HashSet::from([in_scope_exec]));
    h.assert_held_by_focus(&other_exec);
}

#[tokio::test]
async fn an_all_spawns_pause_holds_in_scope_reviews_too() {
    let h = harness();
    let focused = h.project("Focused");
    let (task, _) = h.ready_project_task(&focused, "Reviewed");
    let review = h.ready_execution(&task, ExecutionKind::PrReview);
    h.focus_on(&[&focused]);
    h.coordinator.pause_dispatch_scoped(
        1_786_385_816,
        DispatchPauseOrigin::Operator,
        DispatchPauseScope::AllSpawns,
        PauseReason::new("test: spawn path suspect").unwrap(),
    );

    h.coordinator.drain_ready_queue_for_test().await;

    assert!(h.dispatched().await.is_empty());
    assert_eq!(
        h.execution(&review).dispatch_wait_reason.as_deref(),
        Some("dispatch_paused")
    );
}

#[tokio::test]
async fn agents_launch_bypasses_focus_with_or_without_a_pause() {
    let h = harness();
    let focused = h.project("Focused");
    let other = h.project("Other");
    let _ = h.ready_project_task(&focused, "In scope");
    let (_, other_exec) = h.ready_project_task(&other, "Other project");
    let (_, loose_exec) = h.ready_loose_chore("Loose chore");
    h.focus_on(&[&focused]);

    h.coordinator
        .force_dispatch(&other_exec, DispatchAdmission::OperatorForced)
        .await
        .expect("a forced launch bypasses focus");
    h.coordinator.pause_dispatch_scoped(
        1_786_385_816,
        DispatchPauseOrigin::Operator,
        DispatchPauseScope::NewWork,
        PauseReason::new("test: operator pause").unwrap(),
    );
    h.coordinator
        .force_dispatch(&loose_exec, DispatchAdmission::OperatorForced)
        .await
        .expect("a forced launch bypasses focus and the pause together");
    h.wait_for_runner(&[&other_exec, &loose_exec]).await;

    assert_eq!(h.dispatched().await, HashSet::from([other_exec, loose_exec]));
}

#[tokio::test]
async fn the_concurrency_cap_still_applies_to_in_scope_rows() {
    let h = harness();
    let focused = h.project("Focused");
    let other = h.project("Other");
    h.coordinator.set_max_concurrent_interactive_workers(1).unwrap();
    h.focus_on(&[&focused]);
    let (_, first) = h.ready_project_task(&focused, "First in scope");
    h.drain_and_wait_for(&[&first]).await;

    // The cap counts live workers, so the next in-scope row waits on it.
    let (_, second) = h.ready_project_task(&focused, "Second in scope");
    let (_, outside) = h.ready_project_task(&other, "Out of scope");
    h.coordinator.drain_ready_queue_for_test().await;

    assert_eq!(h.dispatched().await, HashSet::from([first]));
    let capped = h.execution(&second);
    assert_eq!(capped.status, ExecutionStatus::Ready);
    assert!(
        capped
            .dispatch_wait_reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("Waiting for an available worker")),
        "an in-scope row over the cap waits on the cap, not on focus: {:?}",
        capped.dispatch_wait_reason,
    );
    h.assert_held_by_focus(&outside);
}

#[tokio::test]
async fn pr_reviews_follow_scope_and_are_held_not_dropped() {
    let h = harness();
    let focused = h.project("Focused");
    let other = h.project("Other");
    let (in_scope_pr, _) = h.ready_project_task(&focused, "Reviewed PR");
    let (other_pr, _) = h.ready_project_task(&other, "Other PR");
    h.db.connect()
        .unwrap()
        .execute("DELETE FROM work_executions", [])
        .unwrap();
    let in_scope_review = h.ready_execution(&in_scope_pr, ExecutionKind::PrReview);
    let other_review = h.ready_execution(&other_pr, ExecutionKind::PrReview);
    h.focus_on(&[&focused]);

    h.drain_and_wait_for(&[&in_scope_review]).await;
    assert_eq!(h.dispatched().await, HashSet::from([in_scope_review.clone()]));
    h.assert_held_by_focus(&other_review);

    // Adding the project releases the held review: it was never dropped.
    h.focus_on(&[&focused, &other]);
    h.drain_and_wait_for(&[&other_review]).await;
    assert_eq!(h.dispatched().await, HashSet::from([in_scope_review, other_review]));
}

#[tokio::test]
async fn clearing_focus_releases_held_rows_exactly_as_they_were() {
    let h = harness();
    let focused = h.project("Focused");
    let other = h.project("Other");
    let (_, in_scope_exec) = h.ready_project_task(&focused, "In scope");
    let (held_task, held_exec) = h.ready_project_task(&other, "Held");
    let held_before = task_snapshot(&h.db, &held_task);
    let held_execution_before = h.execution(&held_exec);
    h.focus_on(&[&focused]);
    h.drain_and_wait_for(&[&in_scope_exec]).await;
    h.assert_held_by_focus(&held_exec);
    assert_eq!(task_snapshot(&h.db, &held_task), held_before);
    assert_eq!(h.execution(&held_exec).created_at, held_execution_before.created_at);

    h.clear_focus();
    h.drain_and_wait_for(&[&held_exec]).await;

    assert_eq!(h.dispatched().await, HashSet::from([in_scope_exec, held_exec.clone()]));
    assert_eq!(
        h.execution(&held_exec).dispatch_wait_reason,
        None,
        "dispatching clears the focus hold reason"
    );
}

/// A worker blocks on a question for an in-scope row; the question is answered;
/// the minted execution must dispatch under focus without any force-launch.
#[tokio::test]
async fn answering_a_question_on_an_in_scope_row_redispatches_it_automatically() {
    let h = harness();
    let focused = h.project("Focused");
    let other = h.project("Other");
    let (in_scope_task, asking_exec) = h.ready_project_task(&focused, "Needs a decision");
    let (other_task, other_asking_exec) = h.ready_project_task(&other, "Needs a decision elsewhere");
    h.db.connect()
        .unwrap()
        .execute(
            "DELETE FROM work_executions WHERE status = 'ready' AND id NOT IN (?1, ?2)",
            [&asking_exec, &other_asking_exec],
        )
        .unwrap();

    for (task, asking) in [(&in_scope_task, &asking_exec), (&other_task, &other_asking_exec)] {
        let (execution, run) =
            h.db.start_execution_run(asking, "worker-1", "mono", "lease-1", "mono-agent-001", "/tmp/ws")
                .unwrap();
        finish_run_worker_pane_alive(&h.db, &execution.id, &run.id, Some("spawned"));
        let question = boss_protocol::OperatorQuestion {
            text: "Ship the change?".into(),
            explanation: "The worker is blocked on a product choice.".into(),
            answer_type: boss_protocol::OperatorAnswerType::YesNo,
        };
        crate::work::insert_question(&h.db.connect().unwrap(), &execution.id, "proposal", &question).unwrap();
        h.db.record_worker_awaiting_operator_answer(&execution.id, "needs a decision", &question)
            .unwrap();
        let outcome =
            h.db.answer_operator_question(task, OperatorAnswer::YesNo { value: false })
                .unwrap();
        assert!(outcome.minted_execution, "the answer mints the resume execution");
    }
    let resume_for = |task: &str| -> String {
        h.db.list_executions(Some(task))
            .unwrap()
            .into_iter()
            .find(|e| e.status == ExecutionStatus::Ready)
            .expect("a ready resume execution")
            .id
    };
    let in_scope_resume = resume_for(&in_scope_task);
    let other_resume = resume_for(&other_task);
    h.focus_on(&[&focused]);

    h.drain_and_wait_for(&[&in_scope_resume]).await;

    assert!(h.dispatched().await.contains(&in_scope_resume));
    assert!(!h.dispatched().await.contains(&other_resume));
    h.assert_held_by_focus(&other_resume);
}

#[tokio::test]
async fn admission_preview_names_focus_as_a_blocker_that_force_start_does_not_override() {
    let h = harness();
    let focused = h.project("Focused");
    let other = h.project("Other");
    let (in_scope_task, _) = h.ready_project_task(&focused, "In scope");
    let (other_task, _) = h.ready_project_task(&other, "Other project");
    h.focus_on(&[&focused]);

    let in_scope = h.coordinator.evaluate_dispatch_admission(&in_scope_task).await.unwrap();
    assert!(
        in_scope
            .blockers
            .iter()
            .all(|b| b.code != boss_protocol::ADMISSION_BLOCKER_FOCUS_MODE),
        "{:?}",
        in_scope.blockers
    );
    let outside = h.coordinator.evaluate_dispatch_admission(&other_task).await.unwrap();
    assert!(!outside.would_dispatch);
    assert!(
        outside
            .blockers
            .iter()
            .any(|b| b.code == boss_protocol::ADMISSION_BLOCKER_FOCUS_MODE),
        "{:?}",
        outside.blockers
    );

    h.clear_focus();
    let cleared = h.coordinator.evaluate_dispatch_admission(&other_task).await.unwrap();
    assert!(
        cleared
            .blockers
            .iter()
            .all(|b| b.code != boss_protocol::ADMISSION_BLOCKER_FOCUS_MODE)
    );
}

#[tokio::test]
async fn held_count_reports_only_rows_focus_is_holding() {
    let h = harness();
    let focused = h.project("Focused");
    let other = h.project("Other");
    let _ = h.ready_project_task(&focused, "In scope");
    let _ = h.ready_project_task(&other, "Other one");
    let _ = h.ready_loose_chore("Loose chore");

    assert_eq!(h.coordinator.focus_held_count().unwrap(), 0, "no focus, nothing held");
    h.focus_on(&[&focused]);
    assert_eq!(h.coordinator.focus_held_count().unwrap(), 2);
    h.clear_focus();
    assert_eq!(h.coordinator.focus_held_count().unwrap(), 0);
}

/// The breaker's half-open recovery probe is an automatic `force_dispatch`
/// path, so it must respect focus: it picks its canary only from rows inside
/// the focus set. The out-of-scope row is queued last, which is the row the
/// probe prefers when focus is off.
#[tokio::test]
async fn the_breaker_recovery_probe_only_picks_an_in_scope_canary() {
    let h = harness();
    seed_local_claude_driver(&h.db);
    let focused = h.project("Focused");
    let other = h.project("Other");
    let (_, in_scope_exec) = h.ready_project_task(&focused, "In scope");
    let (_, other_exec) = h.ready_project_task(&other, "Other project");
    h.focus_on(&[&focused]);
    h.coordinator.pause_dispatch(
        1_786_385_816,
        DispatchPauseOrigin::Breaker,
        PauseReason::new("test: spawn-capability breaker tripped").unwrap(),
    );

    let spawn_health = SpawnHealthTracker::new();
    let events = Arc::new(RecordingDispatchEventSink::default());
    maybe_admit_recovery_probe(&h.db, &h.coordinator, &spawn_health, events.as_ref(), 1000).await;

    assert!(
        spawn_health.is_probe_execution(&in_scope_exec),
        "the canary must be the in-scope row"
    );
    assert!(!spawn_health.is_probe_execution(&other_exec));
    assert_eq!(h.execution(&other_exec).status, ExecutionStatus::Ready);
}

#[tokio::test]
async fn the_breaker_recovery_probe_admits_nothing_when_every_ready_row_is_out_of_scope() {
    let h = harness();
    seed_local_claude_driver(&h.db);
    let focused = h.project("Focused");
    let other = h.project("Other");
    let (_, other_exec) = h.ready_project_task(&other, "Other project");
    let (_, loose_exec) = h.ready_loose_chore("Loose chore");
    h.focus_on(&[&focused]);
    h.coordinator.pause_dispatch(
        1_786_385_816,
        DispatchPauseOrigin::Breaker,
        PauseReason::new("test: spawn-capability breaker tripped").unwrap(),
    );

    let spawn_health = SpawnHealthTracker::new();
    let events = Arc::new(RecordingDispatchEventSink::default());
    maybe_admit_recovery_probe(&h.db, &h.coordinator, &spawn_health, events.as_ref(), 1000).await;

    for id in [&other_exec, &loose_exec] {
        assert!(!spawn_health.is_probe_execution(id), "{id} is outside focus");
        assert_eq!(h.execution(id).status, ExecutionStatus::Ready);
    }
    assert!(h.dispatched().await.is_empty());
    assert!(events.events().await.is_empty());
}

fn pre_merge_batch(db: &WorkDb, root_id: &str, target_sha: &str) -> (String, Vec<String>) {
    let input = ReviewBatchCreateInput::builder()
        .cycle_root_id(root_id)
        .base_sha("base-sha")
        .classification(
            ReviewClassification::builder()
                .changed_files(vec!["src/lib.rs".to_owned()])
                .complexity_flags(vec![])
                .has_production_code(true)
                .metadata_missing(vec![])
                .production_languages(vec![ReviewLanguageBucket::Rust])
                .profile(ReviewProfile::Light)
                .subsystem_buckets(vec!["src".to_owned()])
                .build(),
        )
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(42)
        .pr_url("https://github.com/example/repo/pull/42")
        .target_sha(target_sha)
        .build();
    match db
        .create_pre_merge_review_batch(input, "https://github.com/example/repo")
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch, executions } => {
            (batch.id, executions.into_iter().map(|e| e.id).collect())
        }
        other => panic!("expected a new batch, got {other:?}"),
    }
}

/// Four never-started out-of-focus pre-merge batches fill the reservation
/// pool; once focus is on, the in-focus batch is admitted and its members
/// dispatch, while the out-of-focus members are abandoned.
#[tokio::test]
async fn in_focus_pre_merge_batch_is_admitted_and_dispatches_after_out_of_focus_reservations_fill_the_pool() {
    let h = harness();
    for driver in ["claude", "codex"] {
        crate::test_support::insert_host_capability(&h.db, "local", &format!("driver={driver}"), "auto");
    }
    let focused = h.project("Focused");
    let other = h.project("Other");
    let mut out_of_focus_execs = Vec::new();
    for i in 0..4 {
        let (task_id, _) = h.ready_project_task(&other, &format!("Out {i}"));
        h.db.connect()
            .unwrap()
            .execute("DELETE FROM work_executions WHERE work_item_id = ?1", [&task_id])
            .unwrap();
        let (_batch_id, execs) = pre_merge_batch(&h.db, &task_id, &format!("head-out-{i}"));
        out_of_focus_execs.extend(execs);
    }
    assert!(
        !h.db
            .can_admit_review_batch(boss_protocol::ReviewBatchPhase::PreMerge)
            .unwrap()
    );

    h.db.save_dispatch_focus(&{
        let mut focus = DispatchFocus::default();
        focus.insert(DispatchFocusTargetKind::Project, focused.id.clone());
        focus
    })
    .unwrap();
    h.focus_on(&[&focused]);

    let (in_scope_task, _) = h.ready_project_task(&focused, "In focus PR");
    h.db.connect()
        .unwrap()
        .execute("DELETE FROM work_executions WHERE work_item_id = ?1", [&in_scope_task])
        .unwrap();
    let (_batch_id, in_focus_execs) = pre_merge_batch(&h.db, &in_scope_task, "head-in");
    assert_eq!(in_focus_execs.len(), 2);

    h.drain_and_wait_for(&[&in_focus_execs[0], &in_focus_execs[1]]).await;

    let dispatched = h.dispatched().await;
    assert_eq!(
        dispatched,
        HashSet::from([in_focus_execs[0].clone(), in_focus_execs[1].clone()])
    );
    for id in &out_of_focus_execs {
        assert_eq!(h.execution(id).status, ExecutionStatus::Abandoned);
        assert!(!dispatched.contains(id), "out-of-focus member {id} must not dispatch");
    }
}

/// An answer that creates work launches it like `bossctl agents launch`: the
/// global pause and the focus set hold an ordinary resume, but not the work an
/// explicit human decision just created.
#[tokio::test]
async fn work_created_by_an_answer_launches_through_the_pause_and_outside_the_focus_set() {
    let h = harness();
    let focused = h.project("Focused");
    let other = h.project("Other");
    let _ = h.ready_project_task(&focused, "In scope");
    let (answered_task, answered_exec) = h.ready_project_task(&other, "Answered, outside focus");
    h.focus_on(&[&focused]);
    h.coordinator.pause_dispatch_scoped(
        1_786_385_816,
        DispatchPauseOrigin::Operator,
        DispatchPauseScope::NewWork,
        PauseReason::new("test: operator pause").unwrap(),
    );

    // Left to the scheduler, the row would just sit behind the pause.
    h.coordinator.drain_ready_queue_for_test().await;
    assert!(h.dispatched().await.is_empty());
    h.db.mark_operator_forced_admission(&answered_task).unwrap();

    let worker = h
        .coordinator
        .launch_answered_work(&answered_task)
        .await
        .expect("launching the answered work succeeds");
    assert!(worker.is_some());
    h.wait_for_runner(&[&answered_exec]).await;
    assert_eq!(h.dispatched().await, HashSet::from([answered_exec]));
}

/// Writer sequencing still applies: an execution gated behind another writer
/// on the PR is not `ready`, so there is nothing to launch until the gate
/// clears. The human's operator-forced admission is kept, and once the row is
/// ready again the drain dispatches it through the pause and outside the
/// focus set.
#[tokio::test]
async fn work_created_by_an_answer_still_waits_behind_a_live_writer() {
    let h = harness();
    let focused = h.project("Focused");
    let other = h.project("Other");
    let _ = h.ready_project_task(&focused, "In scope");
    let (task, exec) = h.ready_project_task(&other, "Gated answer");
    h.db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'waiting_dependency' WHERE work_item_id = ?1",
            [&task],
        )
        .unwrap();
    h.db.mark_operator_forced_admission(&task).unwrap();

    assert_eq!(h.coordinator.launch_answered_work(&task).await.unwrap(), None);
    assert!(h.dispatched().await.is_empty());
    assert!(
        h.db.execution_has_operator_forced_admission(&exec).unwrap(),
        "the waiting execution keeps operator-forced admission"
    );

    h.db.connect()
        .unwrap()
        .execute("UPDATE work_executions SET status = 'ready' WHERE id = ?1", [&exec])
        .unwrap();
    h.focus_on(&[&focused]);
    h.coordinator.pause_dispatch_scoped(
        1_786_385_816,
        DispatchPauseOrigin::Operator,
        DispatchPauseScope::NewWork,
        PauseReason::new("test: operator pause after the writer left").unwrap(),
    );
    h.drain_and_wait_for(&[&exec]).await;
    assert_eq!(h.dispatched().await, HashSet::from([exec]));
}

/// A capacity refusal (startup preflight) at answer time keeps operator-forced
/// admission. Clearing the block lets the drain dispatch through the pause
/// and outside the focus set.
#[tokio::test]
async fn answered_work_dispatches_through_pause_and_focus_after_a_preflight_refusal() {
    let h = harness();
    let focused = h.project("Focused");
    let other = h.project("Other");
    let _ = h.ready_project_task(&focused, "In scope");
    let (answered_task, answered_exec) = h.ready_project_task(&other, "Answered, outside focus");
    h.focus_on(&[&focused]);
    h.coordinator.pause_dispatch_scoped(
        1_786_385_816,
        DispatchPauseOrigin::Operator,
        DispatchPauseScope::NewWork,
        PauseReason::new("test: operator pause").unwrap(),
    );
    h.coordinator
        .set_dispatch_preflight_block(Some("test: local dispatch unavailable".into()));
    h.db.mark_operator_forced_admission(&answered_task).unwrap();

    let err = h
        .coordinator
        .launch_answered_work(&answered_task)
        .await
        .expect_err("preflight refuses the immediate launch");
    assert!(format!("{err:#}").contains("unavailable"), "{err:#}");
    assert_eq!(h.execution(&answered_exec).status, ExecutionStatus::Ready);
    assert!(h.db.execution_has_operator_forced_admission(&answered_exec).unwrap());
    assert!(h.dispatched().await.is_empty());

    h.coordinator.set_dispatch_preflight_block(None);
    h.drain_and_wait_for(&[&answered_exec]).await;
    assert_eq!(h.dispatched().await, HashSet::from([answered_exec]));
}

/// Operator-forced admission is the exemption that lets answered work drain
/// under an all-spawns pause; an ordinary ready row stays held.
#[tokio::test]
async fn operator_forced_work_drains_through_an_all_spawns_pause() {
    let h = harness();
    let project = h.project("Any");
    let (ordinary_task, ordinary_exec) = h.ready_project_task(&project, "Ordinary");
    let (answered_task, answered_exec) = h.ready_project_task(&project, "Answered");
    let _ = ordinary_task;
    h.db.mark_operator_forced_admission(&answered_task).unwrap();
    h.coordinator.pause_dispatch_scoped(
        1_786_385_816,
        DispatchPauseOrigin::Operator,
        DispatchPauseScope::AllSpawns,
        PauseReason::new("test: spawn path suspect").unwrap(),
    );

    h.drain_and_wait_for(&[&answered_exec]).await;
    assert_eq!(h.dispatched().await, HashSet::from([answered_exec.clone()]));
    assert_eq!(h.execution(&ordinary_exec).status, ExecutionStatus::Ready);
    assert_eq!(
        h.execution(&ordinary_exec).dispatch_wait_reason.as_deref(),
        Some("dispatch_paused")
    );
}

/// A breaker-origin pause is never overridable: a flagged ready row stays
/// held with its admission intact, then dispatches after resume.
#[tokio::test]
async fn operator_forced_work_is_held_under_a_breaker_pause_and_dispatches_after_resume() {
    let h = harness();
    let project = h.project("Any");
    let (answered_task, answered_exec) = h.ready_project_task(&project, "Answered");
    h.db.mark_operator_forced_admission(&answered_task).unwrap();
    h.coordinator.pause_dispatch_scoped(
        1_786_385_816,
        DispatchPauseOrigin::Breaker,
        DispatchPauseScope::AllSpawns,
        PauseReason::new("test: spawn path broken").unwrap(),
    );

    h.coordinator.drain_ready_queue_for_test().await;
    assert!(h.dispatched().await.is_empty());
    assert_eq!(h.execution(&answered_exec).status, ExecutionStatus::Ready);
    assert_eq!(
        h.execution(&answered_exec).dispatch_wait_reason.as_deref(),
        Some("dispatch_paused")
    );
    assert!(h.db.execution_has_operator_forced_admission(&answered_exec).unwrap());

    h.coordinator.resume_dispatch();
    h.drain_and_wait_for(&[&answered_exec]).await;
    assert_eq!(h.dispatched().await, HashSet::from([answered_exec.clone()]));
    assert!(!h.db.execution_has_operator_forced_admission(&answered_exec).unwrap());
}
