//! A resume or successor never dispatches ahead of the preservation of the
//! run it recovers from.
//!
//! A finished run's unpublished work is preserved as its cube workspace lease
//! is released, so a successor that dispatches while the lease is still
//! recorded would start from trunk and strand that work. The transient-recovery
//! resume returns the stalled run's lease itself as soon as the pane is gone,
//! and dispatch holds the successor until the lease is gone either way. These
//! tests drive both orders against a real jj store.
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;
use crate::dispatch_events::RecordingDispatchEventSink;
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::transient_error::RecoveryPolicy;
use crate::transient_recovery::{
    NoopWorkerNudger, RecoveryContext, TransientRecoveryOutcome, TransientRecoveryReaper, run_one_pass,
};
use crate::workspace_lease_release::{
    DbPreservation, EmptyOccupancy, LeaseReleaseOutcome, PaneStatus, ReleaseEvidence, TerminalCause, apply_release,
};
use boss_engine_recovery::execution_bookmark::{self as recovery, LocalJj};
use boss_engine_test_git::jj::JjRepo;
use boss_execution_contracts::PaneReleaseOutcome;
use boss_protocol::WorkItemBinding;
use std::collections::HashSet;

const HOLD_REASON: &str = "awaiting_predecessor_preservation";
const SOCKET_ERROR_LINE: &str = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"role":"assistant","content":[{"type":"text","text":"API Error: The socket connection was closed unexpectedly."}]}}"#;
const NORMAL_LINE: &str =
    r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working on the task"}]}}"#;

/// Reports a fixed pane-teardown outcome, as the app-backed reaper does once
/// it has (or has not) confirmed the worker gone.
struct FixedReaper(PaneReleaseOutcome);

#[async_trait]
impl TransientRecoveryReaper for FixedReaper {
    async fn reap_worker(&self, _execution_id: &str) -> PaneReleaseOutcome {
        self.0
    }
}

/// A worker stalled on a transient API error, holding its cube lease on a
/// workspace with unpublished work in it, in a real jj store.
struct Stalled {
    _dir: tempfile::TempDir,
    repo: JjRepo,
    cube: Arc<FakeCubeClient>,
    coordinator: Arc<ExecutionCoordinator>,
    stalled_id: String,
}

impl Stalled {
    /// `record`: whether dispatch recorded the store location that teardown
    /// preservation reads. A started run without it cannot be preserved.
    async fn new(record: bool) -> Self {
        let dir = tempdir().unwrap();
        let repo = JjRepo::new(dir.path());
        super::recovery::configure_recovery_origin(&repo.repo);
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        seed_local_claude_driver(&db);
        let product = create_test_product(&db);
        let chore = create_test_chore_manual(&db, product.id.clone(), "Stalled chore");
        db.reconcile_product_executions(&product.id).unwrap();
        let execution = db
            .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
            .unwrap();
        db.start_execution_run(
            &execution.id,
            "worker-1",
            "mono",
            "lease-old",
            "workspace-old",
            repo.worker.to_str().unwrap(),
        )
        .unwrap();
        let transcript = dir.path().join("transcript.jsonl");
        std::fs::write(&transcript, format!("{NORMAL_LINE}\n{SOCKET_ERROR_LINE}\n")).unwrap();
        db.set_run_transcript_path_if_unset(&execution.id, transcript.to_str().unwrap())
            .unwrap();
        db.force_started_at_for_test(
            &execution.id,
            boss_engine_utils::epoch_time::now_epoch_secs().saturating_sub(600),
        )
        .unwrap();
        if record {
            let record = boss_engine_recovery_fixtures::create(&LocalJj, &repo.worker, &execution.id, "local")
                .await
                .unwrap();
            db.record_execution_bookmark(&record).unwrap();
        }
        std::fs::write(repo.worker.join("fix"), "unpublished fix").unwrap();

        // The registry cube reports the workspace as this run's, which is
        // what lets an inferred release read the live working copy; the same
        // cube later leases the replacement workspace to the successor.
        let cube = Arc::new(
            FakeCubeClient {
                workspace_root: Some(dir.path().to_path_buf()),
                real_bookmarks: true,
                position_real_workspace: true,
                ..FakeCubeClient::default()
            }
            .with_next_workspace_id("replacement")
            .with_recovery_repo(),
        );
        *cube.active_leases.lock().await = vec![
            CubeWorkspaceStatus::builder()
                .workspace_id("workspace-old")
                .workspace_path(repo.worker.clone())
                .state("leased")
                .lease_id("lease-old")
                .build(),
        ];
        let coordinator = Arc::new(ExecutionCoordinator::new(
            db,
            WorkerPool::new(2),
            cube.clone(),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        ));
        Self {
            _dir: dir,
            repo,
            cube,
            coordinator,
            stalled_id: execution.id,
        }
    }

    fn db(&self) -> &WorkDb {
        self.coordinator.work_db()
    }

    /// One transient-recovery pass (the nudge is unavailable, so the stalled
    /// worker is orphaned and resumed) whose pane teardown reports `teardown`.
    async fn resume(
        &self,
        teardown: PaneReleaseOutcome,
    ) -> (TransientRecoveryOutcome, Arc<RecordingDispatchEventSink>) {
        let stalled = self.stalled();
        let worker_id = self
            .coordinator
            .worker_pool()
            .claim_worker(&stalled.id, None)
            .await
            .unwrap();
        let slot_id = boss_execution_contracts::slot_id_from_worker_id(&worker_id).unwrap();
        let live = LiveWorkerStateRegistry::new();
        live.register_spawn(
            slot_id,
            &stalled.id,
            "claude-opus-4-7",
            12345,
            Some(WorkItemBinding {
                work_item_id: stalled.work_item_id.clone(),
                work_item_name: "Stalled chore".to_owned(),
                execution_id: stalled.id.clone(),
            }),
        );
        live.apply_event(
            slot_id,
            &boss_protocol::WorkerEvent::Stop {
                session_id: "s".to_owned(),
                stop_hook_active: false,
                stop_reason: boss_protocol::StopReason::Completed,
            },
        );
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let policy = RecoveryPolicy::default();
        let reaper = FixedReaper(teardown);
        let cx = RecoveryContext {
            work_db: self.db(),
            live_states: &live,
            coordinator: self.coordinator.clone(),
            dispatch_events: sink.as_ref(),
            policy: &policy,
            nudger: &NoopWorkerNudger,
            reaper: &reaper,
            cube_client: self.cube.as_ref(),
        };
        let outcome = run_one_pass(
            &cx,
            &mut HashSet::new(),
            boss_engine_utils::epoch_time::now_epoch_secs(),
        )
        .await;
        (outcome, sink)
    }

    /// The `ready` successor the resume inserted, with its backoff elapsed so
    /// that only the hold can keep it from dispatching.
    fn successor(&self) -> WorkExecution {
        let successor = self
            .db()
            .list_executions(Some(&self.stalled().work_item_id))
            .unwrap()
            .into_iter()
            .find(|e| e.id != self.stalled_id && e.status == ExecutionStatus::Ready)
            .expect("the resume inserted a ready successor");
        self.db()
            .connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET dispatch_not_before = NULL WHERE id = ?1",
                [&successor.id],
            )
            .unwrap();
        self.db().get_execution(&successor.id).unwrap()
    }

    fn stalled(&self) -> WorkExecution {
        self.db().get_execution(&self.stalled_id).unwrap()
    }

    /// What a later sweep does when it finds the stalled run dead: release
    /// its lease through the inferred-death gates (preserving first).
    async fn release_like_a_later_sweep(&self) -> LeaseReleaseOutcome {
        let stalled = self.stalled();
        let preservation = DbPreservation::new(self.db(), None).with_cube(self.cube.as_ref());
        apply_release(
            self.db(),
            &stalled,
            "lease-old",
            ReleaseEvidence {
                cause: TerminalCause::Inferred,
                pane: PaneStatus::Gone,
                occupancy: &EmptyOccupancy,
                preservation: &preservation,
            },
            self.cube.force_release_lease("lease-old", Some("later sweep")),
        )
        .await
    }

    fn preserved_commit(&self) -> String {
        JjRepo::run(
            &self.repo.repo,
            &[
                "log",
                "--no-graph",
                "-r",
                &recovery::preserved_name(&self.stalled_id),
                "-T",
                "commit_id",
            ],
        )
    }

    fn wait_reason(&self, execution_id: &str) -> Option<String> {
        self.db().get_execution(execution_id).unwrap().dispatch_wait_reason
    }
}

#[tokio::test]
async fn resume_preserves_the_stalled_runs_work_and_returns_its_lease_before_the_successor_can_dispatch() {
    let s = Stalled::new(true).await;

    let (outcome, sink) = s.resume(PaneReleaseOutcome::Reaped).await;
    assert_eq!(outcome.resumed, 1);

    // The resume itself preserved the work and returned the lease: nothing
    // is left for a later sweep to do, however late it runs.
    let stalled = s.stalled();
    assert_eq!(stalled.status, ExecutionStatus::Orphaned);
    assert!(stalled.cube_lease_id.is_none(), "the stalled run's lease was returned");
    let released = s.cube.force_release_calls.lock().await.clone();
    assert_eq!(released.len(), 1, "{released:?}");
    assert_eq!(released[0].0, "lease-old");
    let tip = s
        .db()
        .preserved_tip(&s.stalled_id)
        .unwrap()
        .expect("the unpublished work was recorded as the preserved tip");
    assert_eq!(s.preserved_commit(), tip.commit_id);
    assert_eq!(sink.events().await[0].details["predecessor_lease"], "released");
    assert!(matches!(
        s.release_like_a_later_sweep().await,
        LeaseReleaseOutcome::NoLease
    ));

    // Nothing holds the successor, and it restores the work on any workspace.
    let successor = s.successor();
    assert!(s.db().recovery_predecessor_holding_lease(&successor).unwrap().is_none());
    assert_eq!(successor.preferred_workspace_id, None);
    s.coordinator.drain_ready_queue_for_test().await;
    wait_for_execution_status(s.db(), &successor.id, ExecutionStatus::Running).await;
    assert_eq!(
        std::fs::read_to_string(s.repo.replacement.join("fix")).unwrap(),
        "unpublished fix"
    );
    assert_ne!(
        s.db()
            .get_execution(&successor.id)
            .unwrap()
            .cube_workspace_id
            .as_deref(),
        Some("workspace-old"),
        "the successor leases a free workspace, never the stalled run's"
    );
}

#[tokio::test]
async fn successor_is_held_until_a_later_sweep_releases_the_lease_then_restores_the_work() {
    let s = Stalled::new(true).await;

    // A predecessor orphaned while the engine was offline still needs
    // preservation even after cube's 24-hour lease TTL.
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    s.db()
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET finished_at = ?2 WHERE id = ?1",
            rusqlite::params![
                s.stalled_id,
                (boss_engine_utils::epoch_time::now_epoch_secs() - 90_000).to_string()
            ],
        )
        .unwrap();
    s.db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    let successor = s.successor();
    assert!(
        s.db()
            .list_terminal_executions_holding_lease()
            .unwrap()
            .iter()
            .any(|e| e.id == s.stalled_id)
    );
    assert!(s.db().recovery_predecessor_holding_lease(&successor).unwrap().is_some());

    // The drain holds it with the named reason and touches neither a slot nor
    // a workspace: it does not start from trunk.
    s.coordinator.drain_ready_queue_for_test().await;
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready
    );
    assert_eq!(s.wait_reason(&successor.id).as_deref(), Some(HOLD_REASON));
    assert!(s.cube.lease_calls.lock().await.is_empty());

    // So does an operator's force launch, which skips the drain's holds.
    let forced = s
        .coordinator
        .force_dispatch(&successor.id, DispatchAdmission::OperatorForced)
        .await
        .unwrap_err();
    assert!(format!("{forced:#}").contains(HOLD_REASON), "{forced:#}");
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready
    );
    assert!(s.cube.lease_calls.lock().await.is_empty());

    // The sweep that finally finds the stalled run dead preserves and releases.
    assert_eq!(s.release_like_a_later_sweep().await, LeaseReleaseOutcome::Released);
    assert!(s.db().preserved_tip(&s.stalled_id).unwrap().is_some());
    assert!(s.db().recovery_predecessor_holding_lease(&successor).unwrap().is_none());

    // Now it dispatches onto a free workspace and restores the work.
    s.coordinator.drain_ready_queue_for_test().await;
    wait_for_execution_status(s.db(), &successor.id, ExecutionStatus::Running).await;
    assert_eq!(
        std::fs::read_to_string(s.repo.replacement.join("fix")).unwrap(),
        "unpublished fix"
    );
    assert!(s.wait_reason(&successor.id).is_none(), "dispatching clears the hold");
}

#[tokio::test]
async fn failed_preservation_raises_an_attention_item_carrying_the_error_before_the_successor_starts() {
    // No recorded store location: the stalled run's work cannot be preserved.
    let s = Stalled::new(false).await;

    let (outcome, sink) = s.resume(PaneReleaseOutcome::Reaped).await;
    assert_eq!(outcome.resumed, 1);

    // The failure is raised against the stalled run, naming the error and the
    // lease, before anything can dispatch: the lease is returned anyway (a
    // recovery problem never strands a workspace), and the attention item is
    // the durable pointer to the work that was not bookmarked.
    assert_eq!(sink.events().await[0].details["predecessor_lease"], "released");
    assert!(s.stalled().cube_lease_id.is_none());
    let items = s.db().list_attention_items(&s.stalled_id).unwrap();
    let item = items
        .iter()
        .find(|item| item.kind == "preserved_work_needs_inspection" && item.status == "open")
        .unwrap_or_else(|| panic!("expected an open inspection item, got {items:?}"));
    assert!(
        item.body_markdown
            .contains("workspace recovery store location is unavailable for preservation"),
        "{}",
        item.body_markdown
    );
    assert!(item.body_markdown.contains("lease-old"), "{}", item.body_markdown);
    assert!(s.db().preserved_tip(&s.stalled_id).unwrap().is_none());

    // Having been told, the successor is free to start; it has no work to
    // restore, so it starts from trunk rather than from a guess.
    let successor = s.successor();
    assert!(s.db().recovery_predecessor_holding_lease(&successor).unwrap().is_none());
    s.coordinator.drain_ready_queue_for_test().await;
    wait_for_execution_status(s.db(), &successor.id, ExecutionStatus::Running).await;
    assert!(!s.repo.replacement.join("fix").exists());
    assert!(
        s.db()
            .list_attention_items(&s.stalled_id)
            .unwrap()
            .iter()
            .any(|item| item.kind == "preserved_work_needs_inspection" && item.status == "open"),
        "starting does not quietly clear the warning"
    );
}

#[tokio::test]
async fn inline_reconcile_keeps_successor_held_until_preservation_finishes() {
    let s = Stalled::new(true).await;
    // A missing recorded path makes occupancy unconfirmable. The durable
    // start time and absent pane pid still prove the pane never attached.
    s.db()
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET started_at = ?2 WHERE execution_id = ?1",
            rusqlite::params![
                s.stalled_id,
                (boss_engine_utils::epoch_time::now_epoch_secs() - 600).to_string()
            ],
        )
        .unwrap();
    s.db()
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET workspace_path = NULL WHERE id = ?1",
            [&s.stalled_id],
        )
        .unwrap();
    let next = s
        .db()
        .create_execution(
            boss_protocol::CreateExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .kind(s.stalled().kind)
                .status(ExecutionStatus::Ready)
                .repo_remote_url(s.stalled().repo_remote_url)
                .build(),
        )
        .unwrap();
    assert!(s.db().recovery_predecessor_holding_lease(&next).unwrap().is_none());
    let err = s
        .coordinator
        .force_dispatch(&next.id, DispatchAdmission::OperatorForced)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains(HOLD_REASON), "{err:#}");
    assert!(s.stalled().status.is_terminal());
    assert_eq!(s.stalled().cube_lease_id.as_deref(), Some("lease-old"));
    assert!(s.db().preserved_tip(&s.stalled_id).unwrap().is_none());
    assert_eq!(s.db().get_execution(&next.id).unwrap().status, ExecutionStatus::Ready);
    assert_eq!(s.wait_reason(&next.id).as_deref(), Some(HOLD_REASON));
    assert!(s.cube.lease_calls.lock().await.is_empty());

    s.db()
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![s.stalled_id, s.repo.worker.to_str().unwrap()],
        )
        .unwrap();
    assert_eq!(s.release_like_a_later_sweep().await, LeaseReleaseOutcome::Released);
    s.coordinator.drain_ready_queue_for_test().await;
    wait_for_execution_status(s.db(), &next.id, ExecutionStatus::Running).await;
    assert_eq!(
        std::fs::read_to_string(s.repo.replacement.join("fix")).unwrap(),
        "unpublished fix"
    );
}
